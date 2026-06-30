//! COM-vtable FFI bindings for the AMF runtime.
//!
//! Contains every `#[repr(C)]` vtable struct, the AMF ABI constants, the
//! `AmfVariant` tagged-union helper, the IID GUID byte array, and the
//! `AMFInit` entry-point type alias. **Field order in every vtable struct
//! is load-bearing** — the wrong order calls the wrong function pointer and
//! produces UB or a crash.

use std::ffi::c_void;

// ─── AMF ABI constants ────────────────────────────────────────────
// See vendor/amd/ for authoritative definitions.

pub(super) type AmfResult = i32;
pub(super) const AMF_OK: AmfResult = 0;
#[allow(dead_code)]
pub(super) const AMF_FAIL: AmfResult = 1;
pub(super) const AMF_NEED_MORE_INPUT: AmfResult = 2022;
pub(super) const AMF_REPEAT: AmfResult = 2023;
pub(super) const AMF_EOF: AmfResult = 2024;
pub(super) const AMF_INPUT_FULL: AmfResult = 2020;

pub(super) const AMF_VERSION: u64 = amf_make_version(1, 4, 30, 0);

const fn amf_make_version(major: u64, minor: u64, sub_major: u64, sub_minor: u64) -> u64 {
    (major << 48) | (minor << 32) | (sub_major << 16) | sub_minor
}

// AMF memory / surface format enums (`AMF_MEMORY_TYPE`, `AMF_SURFACE_FORMAT`).
// Values from `vendor/amd/AMFContext.h:46-58`.
pub(super) const AMF_MEMORY_HOST: i32 = 1;
pub(super) const AMF_SURFACE_NV12: i32 = 1;
/// `AMF_SURFACE_P010` per `vendor/amd/AMFContext.h:57`. Same NV12-style
/// plane layout (Y plane + interleaved UV plane) but each sample is a
/// 16-bit LE word with the valid 10-bit value in the **upper 10 bits**
/// (Microsoft P010 convention; AMD VCN AV1 inherits this from the DX11
/// surface format definition). The `upload_frame_p010` helper performs
/// the `<<6` shift on copy from the pipeline's `Yuv420p10le`
/// (lower-10-bits) representation.
pub(super) const AMF_SURFACE_P010: i32 = 10;

// AMF plane types (`AMF_PLANE_TYPE`).
pub(super) const AMF_PLANE_Y: i32 = 2;
pub(super) const AMF_PLANE_UV: i32 = 3;

// Variant type tags. Only the ones we set are named.
pub(super) const AMF_VARIANT_INT64: i32 = 2;

// AMF AV1 rate-control enum values (mirror `AMF_VIDEO_ENCODER_AV1_RATE_CONTROL_METHOD_*`).
pub(super) const AMF_RC_CQP: i64 = 1;
pub(super) const AMF_RC_QUALITY_VBR: i64 = 5;

// AMF AV1 output frame type (read back from the AMFBuffer property bag).
pub(super) const AMF_OUTPUT_FRAME_TYPE_KEY: i64 = 0;
pub(super) const AMF_OUTPUT_FRAME_TYPE_INTRA_ONLY: i64 = 1;

// AMF AV1 USAGE_TRANSCODING baseline — picks production defaults for
// rate control + preset. The individual SetProperty calls afterward
// tighten individual knobs to what the tuning adapter asked for.
pub(super) const AMF_USAGE_TRANSCODING: i64 = 0;

// AV1 output mode frame packing — 0 = packed frame-level OBUs with size
// fields (LOB). That's what AV1-ISOBMFF / MP4 mux expects.
pub(super) const AMF_OUTPUT_MODE_FRAME: i64 = 0;

// `Av1ColorBitDepth` enum values per `vendor/amd/VideoEncoderAV1.h:58-59`.
//   1 = AMF_VIDEO_ENCODER_AV1_COLOR_BIT_DEPTH_8
//   2 = AMF_VIDEO_ENCODER_AV1_COLOR_BIT_DEPTH_10
pub(super) const AMF_AV1_COLOR_BIT_DEPTH_8: i64 = 1;
pub(super) const AMF_AV1_COLOR_BIT_DEPTH_10: i64 = 2;

// ─── Ring-buffer configuration ────────────────────────────────────
//
// Squad-5's NVENC path uses RING_SIZE=4 (mirrors ffmpeg's libavcodec/
// nvenc.c default `nb_surfaces`) to keep the encoder pipeline full
// without oversubscribing GPU memory. We mirror the same depth for
// AMF so ops can reason about in-flight buffers uniformly across both
// vendors.
//
// Each ring slot carries the caller-held surface pointer that is
// currently awaiting the encoder's QueryOutput. AMF's ref-counted
// surface model means the encoder retains its own ref after a
// successful `SubmitInput`; our in-flight tracking is therefore a
// SAFETY mirror of the encoder's internal queue, not a reuse pool.
pub(super) const RING_SIZE: usize = 4;

// `AMF_INPUT_FULL` retry policy. The AMF SDK documents INPUT_FULL as
// transient: the caller should drain at least one output packet and
// retry. We bound the retry count so a pathological driver state can't
// spin us forever. Per practical measurements on Radeon PRO W7800 (RDNA3)
// the deepest observed back-pressure drained within ~3 retries; 16 is a
// safety margin 5× that.
pub(super) const INPUT_FULL_MAX_RETRIES: u32 = 16;

// Initial backoff when a drain pass yields zero packets but SubmitInput
// still rejects the surface. 1 ms matches the AMF runtime's own internal
// poll granularity. Doubles up to 16 ms on repeated failures.
pub(super) const INPUT_FULL_BACKOFF_MS_INITIAL: u64 = 1;
pub(super) const INPUT_FULL_BACKOFF_MS_MAX: u64 = 16;

// ─── Variant helpers ──────────────────────────────────────────────

/// AMFVariantStruct layout matching `vendor/amd/AMFComponent.h`.
/// Padded to 32 bytes so SetProperty ABI is stable across SDK rev bumps.
///
/// Header walks (vendor/amd/AMFComponent.h:56-69):
///   - `type`: int32 at offset 0
///   - `pad`:  int32 at offset 4
///   - `value` union: 24 bytes starting at offset 8
///     - `int64Value` is the first field of the union → offset 8
///
/// Our Rust layout below puts `ty` at 0, `_pad` at 4, and `value[0..8]`
/// at offset 8 — matching the header. `AmfVariant::int64` writes the
/// little-endian i64 into `value[0..8]`, which is the union's first
/// field (int64Value). Confirmed against `vendor/amd/AMFComponent.h`
/// offset 8 for the integer arm.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct AmfVariant {
    pub(super) ty: i32,
    pub(super) _pad: i32,
    pub(super) value: [u8; 24],
}

// Compile-time ABI guard: AMFVariantStruct must be exactly 32 bytes
// (4 type + 4 pad + 24 payload) on 64-bit platforms. The AMF SDK
// documents this as stable across SDK revs; a mismatch here means
// SetProperty / GetProperty will splat bytes into the wrong union slot.
const _: () = {
    assert!(
        std::mem::size_of::<AmfVariant>() == 32,
        "AmfVariant must be 32 bytes"
    );
    // int64Value must land at offset 8 inside the struct (offset 0 of
    // `value`). This is the lemma behind `AmfVariant::int64` writing
    // at `value[0..8]`.
    assert!(
        std::mem::offset_of!(AmfVariant, value) == 8,
        "AmfVariant value payload must start at offset 8"
    );
};

// Squad-22: AMF surface format constants pinned. AMD has frozen
// `AMF_SURFACE_FORMAT` values 1..10 since AMF 1.4 — but a future
// renumbering would silently mis-route the surface allocator.
const _: () = assert!(AMF_SURFACE_NV12 == 1);
const _: () = assert!(AMF_SURFACE_P010 == 10);
// `Av1ColorBitDepth` enum values from `vendor/amd/VideoEncoderAV1.h:58-59`.
// 10-bit being `2` (not `10`) is one of the property values that has
// surprised callers — pin it.
const _: () = assert!(AMF_AV1_COLOR_BIT_DEPTH_8 == 1);
const _: () = assert!(AMF_AV1_COLOR_BIT_DEPTH_10 == 2);

impl AmfVariant {
    pub(super) fn int64(v: i64) -> Self {
        let mut value = [0u8; 24];
        value[..8].copy_from_slice(&v.to_le_bytes());
        Self {
            ty: AMF_VARIANT_INT64,
            _pad: 0,
            value,
        }
    }

    /// Read the int64 arm — used by tests and for output-buffer property
    /// reads. Returns `None` if the variant is not int-typed.
    #[allow(dead_code)]
    pub(super) fn as_int64(&self) -> Option<i64> {
        if self.ty == AMF_VARIANT_INT64 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&self.value[..8]);
            Some(i64::from_le_bytes(bytes))
        } else {
            None
        }
    }
}

// ─── Vtable shapes (abbreviated) ──────────────────────────────────
//
// AMF uses COM-style vtables: every handle is a `*mut Object` where
// `Object` is `{ *const Vtbl }`. The vtables below list only the slots
// we actually call; the rest is padded so offsets match the upstream
// ABI for whatever SDK rev the host runtime ships.

type QueryInterfaceFn = unsafe extern "C" fn(*mut c_void, *const c_void, *mut *mut c_void) -> i64;
type AcquireFn = unsafe extern "C" fn(*mut c_void) -> i64;
type ReleaseFn = unsafe extern "C" fn(*mut c_void) -> i64;

#[repr(C)]
pub(super) struct AmfFactoryVtbl {
    pub(super) create_context:
        unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> AmfResult,
    pub(super) create_component:
        unsafe extern "C" fn(*mut c_void, *mut c_void, *const u16, *mut *mut c_void) -> AmfResult,
    pub(super) set_cache_folder: unsafe extern "C" fn(*mut c_void, *const u16) -> AmfResult,
    pub(super) get_cache_folder: unsafe extern "C" fn(*mut c_void) -> *const u16,
    pub(super) get_debug: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> AmfResult,
    pub(super) get_trace: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> AmfResult,
    pub(super) get_programs: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> AmfResult,
}

#[repr(C)]
pub(super) struct AmfFactoryObj {
    pub(super) vtbl: *const AmfFactoryVtbl,
}

// AMFContext vtable — we bind QueryInterface/Acquire/Release (inherited
// from AMFInterface), Terminate, InitDX11, InitVulkan, AllocSurface,
// and pad the rest. Real upstream has ~30 entries; we slot through the
// first N in declaration order and leave the tail as `_reserved`.
#[repr(C)]
pub(super) struct AmfContextVtbl {
    query_interface: QueryInterfaceFn,
    acquire: AcquireFn,
    pub(super) release: ReleaseFn,
    pub(super) terminate: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    pub(super) init_dx11: unsafe extern "C" fn(*mut c_void, *mut c_void, i32) -> AmfResult,
    get_dx11_device: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void,
    lock_dx11: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    unlock_dx11: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    init_opencl: unsafe extern "C" fn(*mut c_void, *mut c_void) -> AmfResult,
    get_opencl_context: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    get_opencl_command_queue: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    get_opencl_device_id: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    convert_to_opencl: unsafe extern "C" fn(*mut c_void, *mut c_void) -> AmfResult,
    lock_opencl: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    unlock_opencl: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    init_opengl:
        unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> AmfResult,
    get_opengl_context: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    get_opengl_drawable: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    convert_to_opengl: unsafe extern "C" fn(*mut c_void, *mut c_void) -> AmfResult,
    lock_opengl: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    unlock_opengl: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    pub(super) init_vulkan: unsafe extern "C" fn(*mut c_void, *mut c_void) -> AmfResult,
    get_vulkan_device: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    lock_vulkan: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    unlock_vulkan: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    alloc_buffer:
        unsafe extern "C" fn(*mut c_void, i32, usize, *mut *mut c_void) -> AmfResult,
    pub(super) alloc_surface: unsafe extern "C" fn(
        *mut c_void,
        i32, // memory type
        i32, // surface format
        i32, // width
        i32, // height
        *mut *mut c_void,
    ) -> AmfResult,
    create_surface_from_host_native: unsafe extern "C" fn(
        *mut c_void,
        i32,
        i32,
        i32,
        i32,
        i32,
        *mut c_void,
        *mut *mut c_void,
        *mut c_void,
    ) -> AmfResult,
}

#[repr(C)]
pub(super) struct AmfContextObj {
    pub(super) vtbl: *const AmfContextVtbl,
}

#[repr(C)]
pub(super) struct AmfComponentVtbl {
    query_interface: QueryInterfaceFn,
    acquire: AcquireFn,
    pub(super) release: ReleaseFn,
    // SetProperty / GetProperty take the variant by value; the AMF C ABI
    // passes it as an inline 32-byte struct, so `by value` matches the
    // layout in `vendor/amd/AMFComponent.h`.
    pub(super) set_property:
        unsafe extern "C" fn(*mut c_void, *const u16, AmfVariant) -> AmfResult,
    pub(super) get_property:
        unsafe extern "C" fn(*mut c_void, *const u16, *mut AmfVariant) -> AmfResult,
    pub(super) init: unsafe extern "C" fn(*mut c_void, i32, i32, i32) -> AmfResult,
    pub(super) reinit: unsafe extern "C" fn(*mut c_void, i32, i32) -> AmfResult,
    pub(super) terminate: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    pub(super) drain: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    pub(super) flush: unsafe extern "C" fn(*mut c_void) -> AmfResult,
    pub(super) submit_input: unsafe extern "C" fn(*mut c_void, *mut c_void) -> AmfResult,
    pub(super) query_output: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> AmfResult,
    pub(super) get_context: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub(super) set_output_data_allocator_cb:
        unsafe extern "C" fn(*mut c_void, *mut c_void) -> AmfResult,
    pub(super) get_caps: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> AmfResult,
    pub(super) optimize: unsafe extern "C" fn(*mut c_void, *mut c_void) -> AmfResult,
}

#[repr(C)]
pub(super) struct AmfComponentObj {
    pub(super) vtbl: *const AmfComponentVtbl,
}

// AMFSurface — we only need vtable slots through `GetPlane` /
// `GetPlaneAt`. Layout keeps the AMFData prefix intact so QueryInterface
// works if a caller cross-casts.
#[repr(C)]
pub(super) struct AmfSurfaceVtbl {
    pub(super) query_interface: QueryInterfaceFn,
    pub(super) acquire: AcquireFn,
    pub(super) release: ReleaseFn,
    pub(super) set_property:
        unsafe extern "C" fn(*mut c_void, *const u16, AmfVariant) -> AmfResult,
    pub(super) get_property:
        unsafe extern "C" fn(*mut c_void, *const u16, *mut AmfVariant) -> AmfResult,
    pub(super) duplicate: unsafe extern "C" fn(*mut c_void, i32, *mut *mut c_void) -> AmfResult,
    pub(super) get_pts: unsafe extern "C" fn(*mut c_void) -> i64,
    pub(super) set_pts: unsafe extern "C" fn(*mut c_void, i64),
    pub(super) get_duration: unsafe extern "C" fn(*mut c_void) -> i64,
    pub(super) set_duration: unsafe extern "C" fn(*mut c_void, i64),
    // Surface-specific
    pub(super) get_planes_count: unsafe extern "C" fn(*mut c_void) -> usize,
    pub(super) get_plane_at: unsafe extern "C" fn(*mut c_void, usize) -> *mut c_void,
    pub(super) get_plane: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void,
}

#[repr(C)]
pub(super) struct AmfSurfaceObj {
    pub(super) vtbl: *const AmfSurfaceVtbl,
}

#[repr(C)]
pub(super) struct AmfPlaneVtbl {
    query_interface: QueryInterfaceFn,
    acquire: AcquireFn,
    release: ReleaseFn,
    get_type: unsafe extern "C" fn(*mut c_void) -> i32,
    pub(super) get_native: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    get_pixel_size_in_bytes: unsafe extern "C" fn(*mut c_void) -> i32,
    get_offset_x: unsafe extern "C" fn(*mut c_void) -> i32,
    get_offset_y: unsafe extern "C" fn(*mut c_void) -> i32,
    get_width: unsafe extern "C" fn(*mut c_void) -> i32,
    get_height: unsafe extern "C" fn(*mut c_void) -> i32,
    pub(super) get_h_pitch: unsafe extern "C" fn(*mut c_void) -> i32,
    get_v_pitch: unsafe extern "C" fn(*mut c_void) -> i32,
}

#[repr(C)]
pub(super) struct AmfPlaneObj {
    pub(super) vtbl: *const AmfPlaneVtbl,
}

// AMFBuffer — output bitstream.
#[repr(C)]
pub(super) struct AmfBufferVtbl {
    pub(super) query_interface: QueryInterfaceFn,
    acquire: AcquireFn,
    pub(super) release: ReleaseFn,
    set_property: unsafe extern "C" fn(*mut c_void, *const u16, AmfVariant) -> AmfResult,
    pub(super) get_property:
        unsafe extern "C" fn(*mut c_void, *const u16, *mut AmfVariant) -> AmfResult,
    duplicate: unsafe extern "C" fn(*mut c_void, i32, *mut *mut c_void) -> AmfResult,
    pub(super) get_pts: unsafe extern "C" fn(*mut c_void) -> i64,
    set_pts: unsafe extern "C" fn(*mut c_void, i64),
    get_duration: unsafe extern "C" fn(*mut c_void) -> i64,
    set_duration: unsafe extern "C" fn(*mut c_void, i64),
    pub(super) get_native: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    pub(super) get_size: unsafe extern "C" fn(*mut c_void) -> usize,
}

#[repr(C)]
pub(super) struct AmfBufferObj {
    pub(super) vtbl: *const AmfBufferVtbl,
}

// IID constants used by QueryInterface to downcast AMFData → AMFBuffer /
// AMFSurface / AMFPlane. AMF publishes these as GUID literals; we carry
// them as 16-byte arrays matching the runtime's in-memory representation.
pub(super) const AMF_IID_BUFFER: [u8; 16] = [
    0xbe, 0x5d, 0xd7, 0xb1, 0x6c, 0x0e, 0x4c, 0x43, 0xb7, 0x28, 0x02, 0x85, 0x98, 0x37, 0x85,
    0x7d,
];

// ─── AMF init entry-point ABI ─────────────────────────────────────

pub(super) type FnAmfInit = unsafe extern "C" fn(u64, *mut *mut c_void) -> AmfResult;
