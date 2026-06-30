//! NVENC buffer and I/O `#[repr(C)]` structs + the function-pointer list.

use std::ffi::c_void;

#[repr(C)]
pub(super) struct NvEncCreateInputBuffer {
    pub(super) version: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) memory_heap: u32,
    pub(super) buffer_fmt: u32,
    pub(super) reserved: u32,
    pub(super) input_buffer: *mut c_void,
    pub(super) sys_mem_buffer: *mut c_void,
    pub(super) reserved1: [u32; 57],
    pub(super) reserved2: [*mut c_void; 63],
}

#[repr(C)]
pub(super) struct NvEncCreateBitstreamBuffer {
    pub(super) version: u32,
    pub(super) size: u32,
    pub(super) memory_heap: u32,
    pub(super) reserved: u32,
    pub(super) bitstream_buffer: *mut c_void,
    pub(super) bitstream_buffer_ptr: *mut c_void,
    pub(super) reserved1: [u32; 58],
    pub(super) reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub(super) struct NvEncLockInputBuffer {
    pub(super) version: u32,
    pub(super) reserved1: u32,
    pub(super) input_buffer: *mut c_void,
    pub(super) buffer_data_ptr: *mut c_void,
    pub(super) pitch: u32,
    pub(super) reserved2: [u32; 251],
    pub(super) reserved3: [*mut c_void; 64],
}

/// `NV_ENC_LOCK_BITSTREAM` — SDK 13.0 layout
/// (vendor/nvidia/nvEncodeAPI.h:2675-2714).
///
/// Previous mirror collapsed the seven scalar fields between
/// `ltr_frame_bitmap` and the trailing `reserved1[219]` into a flat
/// `reserved: [u32; 13]` blob and used a 64-element `reserved2` ptr
/// array instead of the spec's 63 + a trailing `reserved_internal[8]`
/// u32 array. Total size happened to land at 1544 either way, so the
/// `size_of` const_assert passed — but every named [out] field after
/// offset 88 lived at the wrong byte offset, and the [in] `reserved`
/// scalar at offset 116 + the [in,out] `output_stats_ptr` at offset
/// 120 weren't where the driver expected them either.
///
/// On Blackwell + driver 580+ this manifested as
/// `NvEncLockBitstream` returning `NV_ENC_ERR_INVALID_PARAM (8)`
/// during EOS drain, and would have silently corrupted any
/// callsite that read `intra_mb_count` / `inter_mb_count` /
/// `average_mvx` / `average_mvy` / `frame_idx_display` (we don't
/// today, but every future getRCStats consumer would have).
///
/// Fields recovered against SDK 13.0 spec:
///   `temporal_id`            (NEW SDK 13)
///   `alpha_layer_size_in_bytes` (NEW SDK 13)
///   `output_stats_ptr_size`  (NEW SDK 13)
///   `reserved` scalar @ 116  (NEW SDK 13 — gap was inside the prior `[u32;13]`)
///   `output_stats_ptr`       (NEW SDK 13)
///   `frame_idx_display`      (NEW SDK 13)
///   `reserved_internal[8]`   (NEW SDK 13)
///
/// Offset assertions below catch future SDK drift at compile time
/// — `size_of` alone is insufficient (the prior layout proved it).
#[repr(C)]
pub(super) struct NvEncLockBitstream {
    pub(super) version: u32,                      // offset 0
    pub(super) bitfields: u32, // offset 4 — doNotWait:1, ltrFrame:1, getRCStats:1, reservedBitFields:29
    pub(super) output_bitstream: *mut c_void, // offset 8
    pub(super) slice_offsets: *mut u32, // offset 16
    pub(super) frame_idx: u32, // offset 24
    pub(super) hw_encode_status: u32, // offset 28
    pub(super) num_slices: u32, // offset 32
    pub(super) bitstream_size_in_bytes: u32, // offset 36
    pub(super) output_time_stamp: u64, // offset 40
    pub(super) output_duration: u64, // offset 48
    pub(super) bitstream_buffer_ptr: *mut c_void, // offset 56
    pub(super) picture_type: u32, // offset 64
    pub(super) picture_struct: u32, // offset 68
    pub(super) frame_avg_qp: u32, // offset 72
    pub(super) frame_satd: u32, // offset 76
    pub(super) ltr_frame_idx: u32, // offset 80
    pub(super) ltr_frame_bitmap: u32, // offset 84
    pub(super) temporal_id: u32, // offset 88
    pub(super) intra_mb_count: u32, // offset 92
    pub(super) inter_mb_count: u32, // offset 96
    pub(super) average_mvx: i32, // offset 100
    pub(super) average_mvy: i32, // offset 104
    pub(super) alpha_layer_size_in_bytes: u32, // offset 108
    pub(super) output_stats_ptr_size: u32, // offset 112
    pub(super) reserved: u32,  // offset 116 — must be 0
    pub(super) output_stats_ptr: *mut c_void, // offset 120
    pub(super) frame_idx_display: u32, // offset 128
    pub(super) reserved1: [u32; 219], // offset 132
    pub(super) reserved2: [*mut c_void; 63], // offset 1008
    pub(super) reserved_internal: [u32; 8], // offset 1512
                    // total size: 1544 bytes
}

/// `NV_ENC_PIC_PARAMS` — SDK 13.0 layout
/// (vendor/nvidia/nvEncodeAPI.h:2564-2625).
///
/// MAJOR LAYOUT CHANGE FROM PREVIOUS MIRROR — caused the 2026-05-01
/// post-init SIGSEGV right after frame 0 emitted from NVDEC.
///
/// 1. `codec_pic_params` was `[u8; 256]` then `[u8; 1024]`. The SDK 13
///    `NV_ENC_CODEC_PIC_PARAMS` union sizes to MAX(variant) which is
///    NV_ENC_PIC_PARAMS_AV1 = 1544 bytes (H264/HEVC = 1536; the
///    `uint32_t reserved[256]` placeholder in the union body is 1024
///    but the variants are larger and drive the union sizeof).
///    Verified against `sizeof(NV_ENC_CODEC_PIC_PARAMS)` from the
///    vendored SDK 13 header: 1544. ALSO the union's alignment is 8
///    (variants contain pointers); our `[u8; N]` mirror only had
///    1-byte alignment, so the driver expected a 4-byte alignment pad
///    BEFORE the union slot (offset 80) but we put it at offset 76
///    with no pad. Widening to `[u64; 193]` fixes both: 193*8=1544
///    bytes AND 8-byte alignment so Rust auto-pads to offset 80.
///
/// 2. `me_hint_counts_per_block: [u32; 2]` was 8 bytes; SDK 13 spec is
///    `NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE[2]` = 32 bytes (each
///    element is 1 bitfield u32 + 3 u32 reserved). Same mis-mirror as
///    NV_ENC_INITIALIZE_PARAMS.maxMEHintCountsPerBlock. Mirrored as
///    a flat `[u32; 8]` array since we never set any external ME hints.
///
/// 3. SDK 13 added 5 NEW fields between `meHintRefPicDist` and the
///    trailing reserved3 block:
///      - `reserved4: u32`
///      - (existing) alphaBuffer
///      - `meExternalSbHints: *void` (AV1 SB-level external hints)
///      - `meSbHintsCount: u32`
///      - `stateBufferIdx: u32`
///      - `outputReconBuffer: NV_ENC_OUTPUT_PTR` (= *void)
///
/// 4. Final reserved blocks rebalanced: reserved3 went `[u32; 286]` →
///    `[u32; 284]` and reserved4 went `[void*; 60]` → `[void*; 57]`
///    to keep total size matching SDK 13's spec'd layout (2840 bytes).
///
/// New const_assert at the bottom verifies size = 2840 bytes.
#[repr(C)]
pub(super) struct NvEncPicParams {
    pub(super) version: u32,
    pub(super) input_width: u32,
    pub(super) input_height: u32,
    pub(super) input_pitch: u32,
    pub(super) encode_pic_flags: u32,
    pub(super) frame_idx: u32,
    pub(super) input_timestamp: u64,
    pub(super) input_duration: u64,
    pub(super) input_buffer: *mut c_void,
    pub(super) output_bitstream: *mut c_void,
    pub(super) completion_event: *mut c_void,
    pub(super) buffer_fmt: u32,
    pub(super) picture_struct: u32,
    pub(super) picture_type: u32,
    /// Union NV_ENC_CODEC_PIC_PARAMS — sizeof = max(variants) = 1544
    /// bytes (driven by NV_ENC_PIC_PARAMS_AV1; H264/HEVC variants are
    /// 1536). The `uint32_t reserved[256]` placeholder in the union
    /// body is just the trailing fallback; the named variants are
    /// LARGER and drive the union sizeof. Was the root cause of the
    /// post-init SIGSEGV under SDK 13 plus a long tail of misaligned
    /// post-codecPicParams fields. `[u64; 193]` carries both the right
    /// size (193*8 = 1544) and the right alignment (the union has
    /// 8-byte alignment because pointer-bearing variants drive it; the
    /// natural u64 alignment forces a 4-byte pad before this field, so
    /// the field lands at offset 80 just like the C compiler emits).
    /// We never populate the H.264 / HEVC / AV1 per-frame sub-variant;
    /// the encoder runs entirely on the preset+config defaults plus
    /// what NV_ENC_PIC_PARAMS top-level fields drive (frame_idx,
    /// encode_pic_flags, etc.).
    pub(super) codec_pic_params: [u64; 193],
    /// `NVENC_EXTERNAL_ME_HINT_COUNTS_PER_BLOCKTYPE[2]` = 32 bytes per spec.
    /// Mirrored as flat `[u32; 8]` since we never set external ME hints.
    pub(super) me_hint_counts_per_block: [u32; 8],
    pub(super) me_external_hints: *mut c_void,
    /// SDK 13 spec: `uint32_t reserved2[7]` (was 6 in our mirror).
    pub(super) reserved2: [u32; 7],
    /// SDK 13 spec: `void* reserved5[2]`. Renamed for clarity.
    pub(super) reserved5: [*mut c_void; 2],
    pub(super) qp_delta_map: *mut i8,
    pub(super) qp_delta_map_size: u32,
    pub(super) reserved_bitfields: u32,
    pub(super) me_hint_ref_pic_dist: [u16; 2],
    /// NEW in SDK 13: `uint32_t reserved4` between meHintRefPicDist and
    /// alphaBuffer (was implicit padding before).
    pub(super) reserved4: u32,
    pub(super) alpha_buffer: *mut c_void,
    /// NEW in SDK 13: AV1 SB-level external ME hints pointer. Always null.
    pub(super) me_external_sb_hints: *mut c_void,
    /// NEW in SDK 13: count of meExternalSbHints entries. Always 0.
    pub(super) me_sb_hints_count: u32,
    /// NEW in SDK 13: encoder state-buffer index for stateless flow.
    /// Must be in range [0, NV_ENC_INITIALIZE_PARAMS::numStateBuffers - 1].
    /// We set numStateBuffers=0 → stateBufferIdx=0 is the only valid value.
    pub(super) state_buffer_idx: u32,
    /// NEW in SDK 13: reconstructed-frame output buffer pointer.
    /// Only used when enableReconFrameOutput=1; we leave at 0.
    pub(super) output_recon_buffer: *mut c_void,
    /// SDK 13 spec: `uint32_t reserved3[284]`.
    pub(super) reserved3: [u32; 284],
    /// SDK 13 spec: `void* reserved6[57]`. Renamed for clarity.
    pub(super) reserved6: [*mut c_void; 57],
}

// ─── NVENC function list struct ───────────────────────────────────
//
// This mirrors NV_ENCODE_API_FUNCTION_LIST from nvEncodeAPI.h.
// NvEncodeAPICreateInstance fills this in with function pointers.
// The struct layout is stable across NVENC 11+; SDK 12.2 adds a few
// AV1-specific entries at the end that we don't need here.

#[repr(C)]
pub(super) struct NvEncFunctionList {
    pub(super) version: u32,
    pub(super) reserved: u32,

    // All entries are `unsafe extern "C" fn(...) -> NVENCSTATUS`.
    // We keep them as raw pointers; they may be null for fields we
    // don't exercise.
    pub(super) nv_enc_open_encode_session: *mut c_void,
    pub(super) nv_enc_get_encode_guid_count: *mut c_void,
    pub(super) nv_enc_get_encode_profile_guid_count: *mut c_void,
    pub(super) nv_enc_get_encode_profile_guids: *mut c_void,
    pub(super) nv_enc_get_encode_guids: *mut c_void,
    pub(super) nv_enc_get_input_format_count: *mut c_void,
    pub(super) nv_enc_get_input_formats: *mut c_void,
    pub(super) nv_enc_get_encode_caps: *mut c_void,
    pub(super) nv_enc_get_encode_preset_count: *mut c_void,
    pub(super) nv_enc_get_encode_preset_guids: *mut c_void,
    pub(super) nv_enc_get_encode_preset_config: *mut c_void,

    pub(super) nv_enc_initialize_encoder: *mut c_void,
    pub(super) nv_enc_create_input_buffer: *mut c_void,
    pub(super) nv_enc_destroy_input_buffer: *mut c_void,
    pub(super) nv_enc_create_bitstream_buffer: *mut c_void,
    pub(super) nv_enc_destroy_bitstream_buffer: *mut c_void,
    pub(super) nv_enc_encode_picture: *mut c_void,
    pub(super) nv_enc_lock_bitstream: *mut c_void,
    pub(super) nv_enc_unlock_bitstream: *mut c_void,
    pub(super) nv_enc_lock_input_buffer: *mut c_void,
    pub(super) nv_enc_unlock_input_buffer: *mut c_void,
    pub(super) nv_enc_get_encode_stats: *mut c_void,
    pub(super) nv_enc_get_sequence_params: *mut c_void,
    pub(super) nv_enc_register_async_event: *mut c_void,
    pub(super) nv_enc_unregister_async_event: *mut c_void,
    pub(super) nv_enc_map_input_resource: *mut c_void,
    pub(super) nv_enc_unmap_input_resource: *mut c_void,
    pub(super) nv_enc_destroy_encoder: *mut c_void,
    pub(super) nv_enc_invalidate_ref_frames: *mut c_void,
    pub(super) nv_enc_open_encode_session_ex: *mut c_void,
    pub(super) nv_enc_register_resource: *mut c_void,
    pub(super) nv_enc_unregister_resource: *mut c_void,
    pub(super) nv_enc_reconfigure_encoder: *mut c_void,
    pub(super) reserved1: *mut c_void,
    pub(super) nv_enc_create_mv_buffer: *mut c_void,
    pub(super) nv_enc_destroy_mv_buffer: *mut c_void,
    pub(super) nv_enc_run_motion_estimation_only: *mut c_void,
    pub(super) nv_enc_get_last_error_string: *mut c_void,
    pub(super) nv_enc_set_io_cuda_streams: *mut c_void,
    // ROOT CAUSE of the 2026-05-01 SIGSEGV chain (caught by the SDK 13
    // header refresh): SDK 13 SWAPPED these two entries. In SDK 12.2
    // the order was:
    //   nv_enc_get_sequence_param_ex
    //   nv_enc_get_encode_preset_config_ex
    // In SDK 13 the order is:
    //   nv_enc_get_encode_preset_config_ex   ← SWAPPED FIRST
    //   nv_enc_get_sequence_param_ex
    // With the 12.2 order against a 13.x driver-populated list, our
    // request for fn_list.nv_enc_get_encode_preset_config_ex actually
    // picked up the pointer to nvEncGetSequenceParamEx — we then called
    // it with NvEncGetEncodePresetConfigEx's argument shape (encoder,
    // GUID, GUID, tuningInfo, *NV_ENC_PRESET_CONFIG) which made
    // nvEncGetSequenceParamEx dereference one of the GUID 32-bit
    // values as an NV_ENC_SEQUENCE_PARAM_PAYLOAD pointer. Bogus
    // dereference → SIGSEGV inside the driver.
    pub(super) nv_enc_get_encode_preset_config_ex: *mut c_void,
    pub(super) nv_enc_get_sequence_param_ex: *mut c_void,
    // SDK 13 added two new entries here (introduced for stateful encode
    // mid-stream configuration):
    pub(super) nv_enc_restore_encoder_state: *mut c_void,
    pub(super) nv_enc_lookahead_picture: *mut c_void,
    // SDK 13 sized reserved2 at 275 entries. We mirror that exactly so
    // the const_assert! on the struct size catches any future drift.
    pub(super) reserved2: [*mut c_void; 275],
}

// ─── Compile-time assertions for buffer / I/O structs ────────────

// NV_ENC_CREATE_INPUT_BUFFER (vendor/nvidia/nvEncodeAPI.h:203-214).
// 6×u32 + 2×ptr + 57×u32 + 63×ptr with alignment pads = 776 bytes.
const _: () = assert!(std::mem::size_of::<NvEncCreateInputBuffer>() == 776);

// NV_ENC_CREATE_BITSTREAM_BUFFER (vendor/nvidia/nvEncodeAPI.h:217-226).
// 4×u32 + 2×ptr + 58×u32 + 64×ptr = 776 bytes.
const _: () = assert!(std::mem::size_of::<NvEncCreateBitstreamBuffer>() == 776);

// NV_ENC_LOCK_BITSTREAM — SDK 13.0 layout
// (vendor/nvidia/nvEncodeAPI.h:2675-2714). 1544 bytes total. The size
// alone is insufficient: the prior `[u32; 13]` blob in place of the
// seven scalar fields (temporal_id ... output_stats_ptr_size +
// reserved scalar) PLUS `[*void; 64]` instead of `[*void; 63] +
// reserved_internal[8]` totalled the same 1544 bytes but every
// field after offset 88 lived at the wrong place. Driver 580+
// surfaced it as INVALID_PARAM during EOS drain. Below: per-field
// offset assertions catch the same class of drift at compile time.
const _: () = assert!(std::mem::size_of::<NvEncLockBitstream>() == 1544);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, version) == 0);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, output_bitstream) == 8);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, slice_offsets) == 16);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, frame_idx) == 24);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, bitstream_size_in_bytes) == 36);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, output_time_stamp) == 40);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, bitstream_buffer_ptr) == 56);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, picture_type) == 64);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, ltr_frame_bitmap) == 84);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, temporal_id) == 88);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, intra_mb_count) == 92);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, alpha_layer_size_in_bytes) == 108);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, output_stats_ptr_size) == 112);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, reserved) == 116);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, output_stats_ptr) == 120);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, frame_idx_display) == 128);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, reserved1) == 132);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, reserved2) == 1008);
const _: () = assert!(std::mem::offset_of!(NvEncLockBitstream, reserved_internal) == 1512);

// NV_ENC_PIC_PARAMS (vendor/nvidia/nvEncodeAPI.h:2564-2625). SDK 13.0
// total size 3360 bytes. Size grew 2048 → 2840 → 3360 across two
// rounds of mirror correction:
//
//  Round 1 (commit 53365e7, 2840): widened codec_pic_params from
//   [u8; 256] to [u8; 1024] thinking the union body was the
//   `uint32_t reserved[256]` placeholder.
//  Round 2 (this commit, 3360): the union actually sizes to
//   max(variants), and NV_ENC_PIC_PARAMS_AV1 is 1544 bytes (verified
//   via sizeof() against the vendored SDK 13 header). Switched the
//   mirror field to `[u64; 193]` so the slot has both the right
//   size (1544) AND the right alignment (8) — and the natural u64
//   alignment forces the 4-byte pre-pad the C compiler emits to
//   put codecPicParams at offset 80 instead of 76.
//
// Every field after codec_pic_params shifts forward by 524 bytes vs
// the old mirror — including the SDK 13 "new in 13" fields
// (reserved4, alphaBuffer, meExternalSbHints, meSbHintsCount,
// stateBufferIdx, outputReconBuffer) which are now at the offsets
// the driver expects.
const _: () = assert!(std::mem::size_of::<NvEncPicParams>() == 3360);

// NV_ENCODE_API_FUNCTION_LIST — 41 typed fn-pointer slots + 256-ptr
// tail. NVIDIA's real SDK 12.2 struct is smaller than this; we carry
// a deliberately-large tail so `NvEncodeAPICreateInstance` cannot
// write past our buffer if the SDK adds entries. Only checked `>=`
// for that reason. Minimum baseline: version(4)+reserved(4) +
// 41×ptr(328) = 336.
const _: () = assert!(std::mem::size_of::<NvEncFunctionList>() >= 336);
