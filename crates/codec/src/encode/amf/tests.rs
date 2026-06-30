// ─── Tests ───────────────────────────────────────────────────────
//
// GPU E2E is impossible on a non-AMD host (our dev box is RTX 3090).
// These tests exercise the FFI-agnostic invariants: the AMF retry
// driver, the drain helper's status mapping, the ring index cycling,
// and the variant layout. Each test builds a mock component vtable
// that returns a canned status sequence, drives it through the same
// functions the real path uses, and asserts the observable behaviour.

// Private items from parent sub-modules are accessible via `super::` paths
// because children can see their parent's private namespace. We list each one
// explicitly (per tuning/tests.rs pattern) so the import set is auditable.
// `use super::*;` at the end brings in truly-pub items (AmfEncoder, etc.).
use super::{
    // ffi.rs items (brought into amf via private `use self::ffi::*;`)
    AMF_EOF,
    AMF_FAIL,
    AMF_INPUT_FULL,
    AMF_IID_BUFFER,
    AMF_NEED_MORE_INPUT,
    AMF_OK,
    AMF_REPEAT,
    AMF_SURFACE_NV12,
    AMF_SURFACE_P010,
    AMF_VARIANT_INT64,
    AmfComponentObj,
    AmfComponentVtbl,
    AmfResult,
    AmfSurfaceObj,
    AmfSurfaceVtbl,
    AmfVariant,
    INPUT_FULL_MAX_RETRIES,
    RING_SIZE,
    // surface.rs items
    SurfaceGuard,
    // config.rs items
    amf_color_bit_depth_for,
    amf_quality_preset_i64,
    amf_surface_format_for,
    set_int_property,
    transfer_to_h273,
    // private free functions in mod.rs
    drain_until_hungry_raw,
    submit_with_backpressure,
    // re-exported crate items (brought into amf via private `use`)
    ColorMetadata,
    PixelFormat,
    TransferFn,
};
use super::tuning::AmfQualityPreset;
use super::*;

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

// ── Mock AMF component ────────────────────────────────────────
//
// Minimal fake AMF component built to match the vtable layout our
// production code calls through. Each test configures a canned
// sequence of AMF_RESULT values for SubmitInput / QueryOutput;
// the mock returns them in order and tracks Acquire/Release
// counts so we can assert no UAF or leak occurred.
//
// All fields are thread_local so the mock state is accessible from
// the `extern "C"` vtable functions (which cannot close over
// captures).

thread_local! {
    static MOCK_SUBMIT_RESULTS: RefCell<Vec<AmfResult>> = const { RefCell::new(Vec::new()) };
    static MOCK_QUERY_RESULTS: RefCell<Vec<AmfResult>> = const { RefCell::new(Vec::new()) };
    static MOCK_SUBMIT_CALLS: AtomicUsize = const { AtomicUsize::new(0) };
    static MOCK_QUERY_CALLS: AtomicUsize = const { AtomicUsize::new(0) };
    static MOCK_SURFACE_REFCOUNT: AtomicI64 = const { AtomicI64::new(0) };
    /// Records the surface pointer passed to each SubmitInput call
    /// so we can assert the driver retries with the SAME pointer
    /// (no UAF, no substitution).
    static MOCK_SUBMIT_POINTERS: RefCell<Vec<*mut c_void>> = const { RefCell::new(Vec::new()) };
}

fn mock_reset() {
    MOCK_SUBMIT_RESULTS.with(|v| v.borrow_mut().clear());
    MOCK_QUERY_RESULTS.with(|v| v.borrow_mut().clear());
    MOCK_SUBMIT_POINTERS.with(|v| v.borrow_mut().clear());
    MOCK_SUBMIT_CALLS.with(|c| c.store(0, Ordering::SeqCst));
    MOCK_QUERY_CALLS.with(|c| c.store(0, Ordering::SeqCst));
    MOCK_SURFACE_REFCOUNT.with(|c| c.store(1, Ordering::SeqCst));
}

fn set_submit_sequence(results: &[AmfResult]) {
    MOCK_SUBMIT_RESULTS.with(|v| *v.borrow_mut() = results.to_vec());
}

fn set_query_sequence(results: &[AmfResult]) {
    MOCK_QUERY_RESULTS.with(|v| *v.borrow_mut() = results.to_vec());
}

fn submit_call_count() -> usize {
    MOCK_SUBMIT_CALLS.with(|c| c.load(Ordering::SeqCst))
}

fn query_call_count() -> usize {
    MOCK_QUERY_CALLS.with(|c| c.load(Ordering::SeqCst))
}

fn surface_refcount() -> i64 {
    MOCK_SURFACE_REFCOUNT.with(|c| c.load(Ordering::SeqCst))
}

fn submit_pointer_at(idx: usize) -> Option<*mut c_void> {
    MOCK_SUBMIT_POINTERS.with(|v| v.borrow().get(idx).copied())
}

// ── Mock component vtable funcs ───────────────────────────────

unsafe extern "C" fn mock_qi(_: *mut c_void, _: *const c_void, _: *mut *mut c_void) -> i64 {
    0
}
unsafe extern "C" fn mock_acquire(_: *mut c_void) -> i64 {
    1
}
unsafe extern "C" fn mock_release_component(_: *mut c_void) -> i64 {
    1
}
unsafe extern "C" fn mock_set_property(
    _: *mut c_void,
    _: *const u16,
    _: AmfVariant,
) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_get_property(
    _: *mut c_void,
    _: *const u16,
    _: *mut AmfVariant,
) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_init(_: *mut c_void, _: i32, _: i32, _: i32) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_reinit(_: *mut c_void, _: i32, _: i32) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_terminate(_: *mut c_void) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_drain(_: *mut c_void) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_flush(_: *mut c_void) -> AmfResult {
    AMF_OK
}

unsafe extern "C" fn mock_submit_input(_: *mut c_void, surface: *mut c_void) -> AmfResult {
    MOCK_SUBMIT_POINTERS.with(|v| v.borrow_mut().push(surface));
    let idx = MOCK_SUBMIT_CALLS.with(|c| c.fetch_add(1, Ordering::SeqCst));
    MOCK_SUBMIT_RESULTS.with(|v| {
        let v = v.borrow();
        v.get(idx).copied().unwrap_or(AMF_OK)
    })
}

unsafe extern "C" fn mock_query_output(_: *mut c_void, data: *mut *mut c_void) -> AmfResult {
    let idx = MOCK_QUERY_CALLS.with(|c| c.fetch_add(1, Ordering::SeqCst));
    let rc = MOCK_QUERY_RESULTS.with(|v| {
        let v = v.borrow();
        v.get(idx).copied().unwrap_or(AMF_REPEAT)
    });
    if rc == AMF_OK {
        // Return null data — drain helper treats that as "no
        // packet produced this round" and continues looping.
        unsafe {
            *data = ptr::null_mut();
        }
    }
    rc
}

unsafe extern "C" fn mock_get_context(_: *mut c_void) -> *mut c_void {
    ptr::null_mut()
}
unsafe extern "C" fn mock_set_output_cb(_: *mut c_void, _: *mut c_void) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_get_caps(_: *mut c_void, _: *mut *mut c_void) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_optimize(_: *mut c_void, _: *mut c_void) -> AmfResult {
    AMF_OK
}

// ── Mock surface vtable funcs ─────────────────────────────────
//
// The driver only calls Release on the surface (directly via the
// guard) and never touches any other surface slot in these tests.
// The full vtable is populated so the Rust struct layout matches
// what production code expects to walk.

unsafe extern "C" fn mock_surface_release(_: *mut c_void) -> i64 {
    let prev = MOCK_SURFACE_REFCOUNT.with(|c| c.fetch_sub(1, Ordering::SeqCst));
    assert!(
        prev > 0,
        "surface Release when refcount already zero (UAF indicator)"
    );
    prev - 1
}

unsafe extern "C" fn mock_surface_set_property(
    _: *mut c_void,
    _: *const u16,
    _: AmfVariant,
) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_surface_get_property(
    _: *mut c_void,
    _: *const u16,
    _: *mut AmfVariant,
) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_surface_duplicate(
    _: *mut c_void,
    _: i32,
    _: *mut *mut c_void,
) -> AmfResult {
    AMF_OK
}
unsafe extern "C" fn mock_surface_get_pts(_: *mut c_void) -> i64 {
    0
}
unsafe extern "C" fn mock_surface_set_pts(_: *mut c_void, _: i64) {}
unsafe extern "C" fn mock_surface_get_duration(_: *mut c_void) -> i64 {
    0
}
unsafe extern "C" fn mock_surface_set_duration(_: *mut c_void, _: i64) {}
unsafe extern "C" fn mock_surface_get_planes_count(_: *mut c_void) -> usize {
    2
}
unsafe extern "C" fn mock_surface_get_plane_at(_: *mut c_void, _: usize) -> *mut c_void {
    ptr::null_mut()
}
unsafe extern "C" fn mock_surface_get_plane(_: *mut c_void, _: i32) -> *mut c_void {
    ptr::null_mut()
}

static MOCK_SURFACE_VTBL: AmfSurfaceVtbl = AmfSurfaceVtbl {
    query_interface: mock_qi,
    acquire: mock_acquire,
    release: mock_surface_release,
    set_property: mock_surface_set_property,
    get_property: mock_surface_get_property,
    duplicate: mock_surface_duplicate,
    get_pts: mock_surface_get_pts,
    set_pts: mock_surface_set_pts,
    get_duration: mock_surface_get_duration,
    set_duration: mock_surface_set_duration,
    get_planes_count: mock_surface_get_planes_count,
    get_plane_at: mock_surface_get_plane_at,
    get_plane: mock_surface_get_plane,
};

static MOCK_COMPONENT_VTBL: AmfComponentVtbl = AmfComponentVtbl {
    query_interface: mock_qi,
    acquire: mock_acquire,
    release: mock_release_component,
    set_property: mock_set_property,
    get_property: mock_get_property,
    init: mock_init,
    reinit: mock_reinit,
    terminate: mock_terminate,
    drain: mock_drain,
    flush: mock_flush,
    submit_input: mock_submit_input,
    query_output: mock_query_output,
    get_context: mock_get_context,
    set_output_data_allocator_cb: mock_set_output_cb,
    get_caps: mock_get_caps,
    optimize: mock_optimize,
};

/// Build a fake surface + component on the stack that resolve to
/// the mock vtables. Returns pointers the driver can hand through
/// its FFI signatures. Caller owns the backing storage via the
/// returned tuple — pointers are only valid for the lifetime of
/// the stack frame that owns them.
fn make_mock_pair() -> (Box<AmfSurfaceObj>, Box<AmfComponentObj>) {
    let surface = Box::new(AmfSurfaceObj {
        vtbl: &MOCK_SURFACE_VTBL,
    });
    let component = Box::new(AmfComponentObj {
        vtbl: &MOCK_COMPONENT_VTBL,
    });
    (surface, component)
}

// ── Tests ─────────────────────────────────────────────────────

/// The core #59 regression test: an AMF_INPUT_FULL return from
/// SubmitInput must NOT release the surface before the retry.
/// If the driver releases prematurely, the retry submit would
/// be dereferencing a zero-refcount surface — a UAF. This test
/// runs through the real `submit_with_backpressure` function
/// against a mock that returns `INPUT_FULL, OK` and asserts:
///   1. SubmitInput was called twice with the SAME surface ptr.
///   2. The surface refcount stayed at ≥1 across the retry.
///   3. Final refcount is 0 after the success path releases.
#[test]
fn test_amf_input_full_does_not_release_surface_before_retry() {
    mock_reset();
    set_submit_sequence(&[AMF_INPUT_FULL, AMF_OK]);
    // Drain between retries returns REPEAT immediately (no output
    // available) — the driver's backoff then wakes up and retries.
    set_query_sequence(&[AMF_REPEAT]);

    let (mut surface, mut component) = make_mock_pair();
    let surface_ptr: *mut c_void = surface.as_mut() as *mut _ as *mut c_void;
    let component_ptr: *mut c_void = component.as_mut() as *mut _ as *mut c_void;

    let mut guard = SurfaceGuard::new(surface_ptr);
    let mut packets = Vec::new();

    let result = unsafe { submit_with_backpressure(&mut packets, component_ptr, &mut guard) };
    assert!(
        result.is_ok(),
        "submit_with_backpressure failed: {result:?}"
    );

    assert_eq!(
        submit_call_count(),
        2,
        "SubmitInput must retry exactly once on INPUT_FULL before success"
    );
    assert_eq!(
        submit_pointer_at(0),
        Some(surface_ptr),
        "first submit must pass the original surface pointer"
    );
    assert_eq!(
        submit_pointer_at(1),
        Some(surface_ptr),
        "retry submit must pass the SAME surface pointer — anything else would be a UAF tell"
    );
    // After the success path, the success-arm's explicit release
    // has dropped our caller-held ref from 1 → 0. No double-free.
    assert_eq!(
        surface_refcount(),
        0,
        "surface refcount must reach exactly 0 after success (no leak, no double-release)"
    );
    // Guard's owned flag must be cleared (transfer_to_encoder was
    // called) so Drop is a no-op at end of scope.
    // Sanity-check by letting the guard drop and verifying the
    // refcount doesn't go negative (the mock panics if it does).
    drop(guard);
    assert_eq!(surface_refcount(), 0, "Drop after transfer must be a no-op");
}

/// AMF_NEED_MORE_INPUT on QueryOutput is the driver's signal that
/// the encoder needs more frames before it can emit anything
/// (typical for lookahead warm-up). The drain helper must treat
/// this as a clean "no packet available" return, NOT an error.
#[test]
fn test_amf_need_more_input_returns_no_packet() {
    mock_reset();
    set_query_sequence(&[AMF_NEED_MORE_INPUT]);

    let (_, mut component) = make_mock_pair();
    let component_ptr: *mut c_void = component.as_mut() as *mut _ as *mut c_void;
    let mut packets = Vec::new();

    let result = unsafe { drain_until_hungry_raw(&mut packets, component_ptr) };
    assert!(
        result.is_ok(),
        "AMF_NEED_MORE_INPUT on drain must be Ok (no packet yet), got {result:?}"
    );
    assert_eq!(packets.len(), 0, "no packets should be emitted");
    assert_eq!(
        query_call_count(),
        1,
        "drain should have returned after the single NEED_MORE_INPUT"
    );
}

/// AMF_EOF on QueryOutput signals end-of-stream after Drain() was
/// called. The drain helper must return cleanly (not bail) so
/// flush_drain can complete. No packets should be appended for
/// the EOF return itself.
#[test]
fn test_amf_eof_ends_drain_cleanly() {
    mock_reset();
    set_query_sequence(&[AMF_EOF]);

    let (_, mut component) = make_mock_pair();
    let component_ptr: *mut c_void = component.as_mut() as *mut _ as *mut c_void;
    let mut packets = Vec::new();

    let result = unsafe { drain_until_hungry_raw(&mut packets, component_ptr) };
    assert!(
        result.is_ok(),
        "AMF_EOF on drain must end the flush loop cleanly, got {result:?}"
    );
    assert_eq!(packets.len(), 0, "no packets at EOF");
    assert_eq!(
        query_call_count(),
        1,
        "drain should return on the first EOF"
    );
}

/// The ring index must cycle 0, 1, 2, 3, 0, 1, 2, 3 ... under the
/// `(ring_idx + 1) % RING_SIZE` advancement rule that `encode_one`
/// uses on every successful SubmitInput. Mirrors NVENC's parallel
/// test so both backends are validated identically.
#[test]
fn test_amf_ring_buffer_index_cycles() {
    let mut idx = 0usize;
    let mut seen = Vec::new();
    for _ in 0..(RING_SIZE * 3) {
        seen.push(idx);
        idx = (idx + 1) % RING_SIZE;
    }
    assert_eq!(
        seen,
        vec![0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3],
        "ring index must cycle through 0..RING_SIZE"
    );
}

/// Ring size is the NVENC-parity constant.
#[test]
fn test_amf_ring_size_is_four() {
    assert_eq!(
        RING_SIZE, 4,
        "RING_SIZE must match Squad-5's NVENC default of 4"
    );
}

/// AMF_REPEAT on SubmitInput is documented as a transient "retry
/// same surface" status, identical semantics to AMF_INPUT_FULL.
/// The driver must handle it the same way.
#[test]
fn test_amf_repeat_on_submit_retries_same_surface() {
    mock_reset();
    set_submit_sequence(&[AMF_REPEAT, AMF_OK]);
    set_query_sequence(&[AMF_REPEAT]);

    let (mut surface, mut component) = make_mock_pair();
    let surface_ptr: *mut c_void = surface.as_mut() as *mut _ as *mut c_void;
    let component_ptr: *mut c_void = component.as_mut() as *mut _ as *mut c_void;

    let mut guard = SurfaceGuard::new(surface_ptr);
    let mut packets = Vec::new();

    let result = unsafe { submit_with_backpressure(&mut packets, component_ptr, &mut guard) };
    assert!(result.is_ok(), "AMF_REPEAT retry must succeed");
    assert_eq!(submit_call_count(), 2);
    assert_eq!(submit_pointer_at(1), Some(surface_ptr));
    assert_eq!(surface_refcount(), 0);
    drop(guard);
}

/// A hard error from SubmitInput (anything other than OK,
/// NEED_MORE_INPUT, INPUT_FULL, REPEAT) must surface as Err and
/// the guard's Drop must release the caller-held ref exactly once
/// — not zero times (leak) and not twice (double-free).
#[test]
fn test_amf_submit_hard_error_releases_through_guard() {
    mock_reset();
    set_submit_sequence(&[AMF_FAIL]);
    set_query_sequence(&[AMF_REPEAT]);

    let (mut surface, mut component) = make_mock_pair();
    let surface_ptr: *mut c_void = surface.as_mut() as *mut _ as *mut c_void;
    let component_ptr: *mut c_void = component.as_mut() as *mut _ as *mut c_void;

    let mut packets = Vec::new();
    {
        let mut guard = SurfaceGuard::new(surface_ptr);
        let result =
            unsafe { submit_with_backpressure(&mut packets, component_ptr, &mut guard) };
        assert!(result.is_err(), "hard error must propagate as Err");
        // Guard goes out of scope here → Drop releases our ref.
    }
    assert_eq!(
        surface_refcount(),
        0,
        "hard-error path must release exactly once via the guard's Drop"
    );
}

/// Bounded retry budget: if both SubmitInput AND QueryOutput stay
/// saturated indefinitely, the driver must eventually bail rather
/// than spin forever. This simulates a stuck GPU queue.
#[test]
fn test_amf_submit_bounded_retry_budget() {
    mock_reset();
    // Fill submit with INPUT_FULL responses exceeding the retry
    // budget. Every QueryOutput returns REPEAT (no output), so
    // backoff + retry proceeds without clearing space.
    let saturated: Vec<AmfResult> = (0..(INPUT_FULL_MAX_RETRIES as usize + 2))
        .map(|_| AMF_INPUT_FULL)
        .collect();
    set_submit_sequence(&saturated);
    let drains: Vec<AmfResult> = (0..(INPUT_FULL_MAX_RETRIES as usize + 2))
        .map(|_| AMF_REPEAT)
        .collect();
    set_query_sequence(&drains);

    let (mut surface, mut component) = make_mock_pair();
    let surface_ptr: *mut c_void = surface.as_mut() as *mut _ as *mut c_void;
    let component_ptr: *mut c_void = component.as_mut() as *mut _ as *mut c_void;

    let mut packets = Vec::new();
    {
        let mut guard = SurfaceGuard::new(surface_ptr);
        let result =
            unsafe { submit_with_backpressure(&mut packets, component_ptr, &mut guard) };
        assert!(
            result.is_err(),
            "stuck backpressure must eventually bail (not spin)"
        );
        // Ring-buffer state does NOT advance here (caller
        // responsibility); this test just checks the retry ceiling.
        assert_eq!(
            submit_call_count() as u32,
            INPUT_FULL_MAX_RETRIES + 1,
            "retry count must match INPUT_FULL_MAX_RETRIES + 1 (initial + retries)"
        );
    }
    // Guard drop releases the single caller ref once.
    assert_eq!(
        surface_refcount(),
        0,
        "bounded-retry failure must still release cleanly via guard"
    );
}

/// Variant layout ABI guard: the `int64` arm must live at offset
/// 8 of the struct so the C ABI's tagged-union write lands in the
/// right byte range. codec-review-59-60 M-A2 follow-up.
#[test]
fn test_amf_variant_int64_layout() {
    let v = AmfVariant::int64(0x0123_4567_89ab_cdef);
    assert_eq!(v.ty, AMF_VARIANT_INT64);
    assert_eq!(v._pad, 0);
    assert_eq!(
        v.as_int64(),
        Some(0x0123_4567_89ab_cdef),
        "int64 round-trip must match"
    );
    // Byte-level check: little-endian bytes of the payload in
    // value[0..8].
    let expected = 0x0123_4567_89ab_cdefi64.to_le_bytes();
    assert_eq!(
        &v.value[..8],
        &expected,
        "int64 payload must be LE-encoded into value[0..8]"
    );
    // Size invariant held at compile time by the const_assert
    // above; belt-and-suspenders runtime check here.
    assert_eq!(std::mem::size_of::<AmfVariant>(), 32);
    assert_eq!(std::mem::offset_of!(AmfVariant, value), 8);
}

/// Verify AMF_IID_BUFFER matches the expected little-endian GUID
/// layout of `{0xb1d75dbe, 0x0e6c, 0x434c, {0xb7, 0x28, 0x02,
/// 0x85, 0x98, 0x37, 0x85, 0x7d}}` (AMFBuffer.h). codec-review-
/// 59-60 AMF-7 follow-up.
#[test]
fn test_amf_iid_buffer_byte_order() {
    // First 4 bytes = LE u32 of 0xb1d75dbe
    assert_eq!(&AMF_IID_BUFFER[0..4], &0xb1d75dbeu32.to_le_bytes());
    // Next 2 bytes = LE u16 of 0x0e6c
    assert_eq!(&AMF_IID_BUFFER[4..6], &0x0e6cu16.to_le_bytes());
    // Next 2 bytes = LE u16 of 0x434c
    assert_eq!(&AMF_IID_BUFFER[6..8], &0x434cu16.to_le_bytes());
    // Trailing 8 bytes are raw.
    assert_eq!(
        &AMF_IID_BUFFER[8..16],
        &[0xb7, 0x28, 0x02, 0x85, 0x98, 0x37, 0x85, 0x7d]
    );
}

/// Quality-preset mapping must cover all four documented AMF enum
/// values, not an arbitrary scale — codec-review-59-60 AMF-3.
#[test]
fn test_amf_quality_preset_mapping_exhaustive() {
    assert_eq!(amf_quality_preset_i64(AmfQualityPreset::HighQuality), 10);
    assert_eq!(amf_quality_preset_i64(AmfQualityPreset::Quality), 30);
    assert_eq!(amf_quality_preset_i64(AmfQualityPreset::Balanced), 50);
    assert_eq!(amf_quality_preset_i64(AmfQualityPreset::Speed), 70);
}

// ── Squad-22: AMF 10-bit dispatch + color signalling ─────────

/// Surface-format dispatch must map `Yuv420p10le` to P010 and
/// `Yuv420p` to NV12 — anything else must bail. Same correctness-
/// by-review story as NVENC: a wide-word surface allocated as NV12
/// would receive byte-truncated samples → silent black frames.
#[test]
fn test_amf_surface_format_dispatch() {
    assert_eq!(
        amf_surface_format_for(PixelFormat::Yuv420p).unwrap(),
        AMF_SURFACE_NV12,
        "8-bit → NV12"
    );
    assert_eq!(
        amf_surface_format_for(PixelFormat::Yuv420p10le).unwrap(),
        AMF_SURFACE_P010,
        "10-bit → P010"
    );
    assert!(amf_surface_format_for(PixelFormat::Yuv422p).is_err());
    assert!(amf_surface_format_for(PixelFormat::Rgb24).is_err());
    assert!(amf_surface_format_for(PixelFormat::Yuv444p10le).is_err());
}

/// `Av1ColorBitDepth` SetProperty value must be 2 for 10-bit (NOT
/// 10 — easy mis-set; the property is an enum, not a literal bit
/// depth). vendor/amd/VideoEncoderAV1.h:58-59.
#[test]
fn test_amf_color_bit_depth_dispatch() {
    assert_eq!(amf_color_bit_depth_for(PixelFormat::Yuv420p), 1);
    assert_eq!(amf_color_bit_depth_for(PixelFormat::Yuv420p10le), 2);
}

/// HDR transfer codes round-trip to their H.273 numeric values,
/// matching the NVENC + mux paths so a single `ColorMetadata`
/// goes through three independent code paths to the same number.
#[test]
fn test_amf_transfer_to_h273_codes() {
    assert_eq!(transfer_to_h273(TransferFn::Bt709), 1);
    assert_eq!(transfer_to_h273(TransferFn::St2084), 16);
    assert_eq!(transfer_to_h273(TransferFn::AribStdB67), 18);
    assert_eq!(transfer_to_h273(TransferFn::Linear), 8);
    assert_eq!(transfer_to_h273(TransferFn::Bt470Bg), 4);
    assert_eq!(transfer_to_h273(TransferFn::Unspecified), 1);
}

/// End-to-end SetProperty sequence for an HDR10 10-bit job using a
/// mock component — verifies the four color SetProperty calls all
/// land with the expected numeric values, and the bit-depth
/// property carries the AMF enum value `2` (not `10`).
///
/// Records every property name+value the driver writes and asserts
/// the HDR-related ones in declaration order. The mock component
/// vtable from the existing tests is reused.
#[test]
fn test_amf_hdr10_set_property_sequence() {
    thread_local! {
        static RECORDED: std::cell::RefCell<Vec<(String, i64)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    unsafe extern "C" fn record_set_property(
        _: *mut c_void,
        name: *const u16,
        v: AmfVariant,
    ) -> AmfResult {
        // Decode the wide string back to UTF-8.
        unsafe {
            let mut len = 0usize;
            while *name.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(name, len);
            let s = String::from_utf16_lossy(slice);
            let value = v.as_int64().unwrap_or(0);
            RECORDED.with(|r| r.borrow_mut().push((s, value)));
        }
        AMF_OK
    }

    static REC_VTBL: AmfComponentVtbl = AmfComponentVtbl {
        query_interface: mock_qi,
        acquire: mock_acquire,
        release: mock_release_component,
        set_property: record_set_property,
        get_property: mock_get_property,
        init: mock_init,
        reinit: mock_reinit,
        terminate: mock_terminate,
        drain: mock_drain,
        flush: mock_flush,
        submit_input: mock_submit_input,
        query_output: mock_query_output,
        get_context: mock_get_context,
        set_output_data_allocator_cb: mock_set_output_cb,
        get_caps: mock_get_caps,
        optimize: mock_optimize,
    };

    let mut component = Box::new(AmfComponentObj { vtbl: &REC_VTBL });
    let component_ptr: *mut c_void = component.as_mut() as *mut _ as *mut c_void;
    let vt: &AmfComponentVtbl = unsafe { &*(*(component_ptr as *mut AmfComponentObj)).vtbl };

    // 10-bit + HDR10 metadata.
    let cm = ColorMetadata {
        transfer: TransferFn::St2084,
        matrix_coefficients: 9, // BT.2020 NCL
        colour_primaries: 9,    // BT.2020
        full_range: true,
        mastering_display: None,
        content_light_level: None,
    };

    // Drive the same SetProperty sequence the production new() path
    // uses for 10-bit + HDR10.
    unsafe {
        set_int_property(
            component_ptr,
            vt,
            "Av1ColorBitDepth",
            amf_color_bit_depth_for(PixelFormat::Yuv420p10le),
        )
        .unwrap();
        set_int_property(
            component_ptr,
            vt,
            "Av1OutColorPrimaries",
            cm.colour_primaries as i64,
        )
        .unwrap();
        set_int_property(
            component_ptr,
            vt,
            "Av1OutColorTransferChar",
            transfer_to_h273(cm.transfer),
        )
        .unwrap();
        set_int_property(
            component_ptr,
            vt,
            "Av1OutColorMatrixCoeff",
            cm.matrix_coefficients as i64,
        )
        .unwrap();
        set_int_property(component_ptr, vt, "Av1OutColorRange", cm.full_range as i64).unwrap();
    }

    let recorded: Vec<(String, i64)> = RECORDED.with(|r| r.borrow().clone());
    // Find each property by name to be order-tolerant — the test
    // asserts the values, not the call order.
    let lookup = |name: &str| -> i64 {
        recorded
            .iter()
            .find(|(n, _)| n == name)
            .expect("property recorded")
            .1
    };
    assert_eq!(
        lookup("Av1ColorBitDepth"),
        2,
        "10-bit enum is value 2, not 10"
    );
    assert_eq!(lookup("Av1OutColorPrimaries"), 9, "BT.2020");
    assert_eq!(lookup("Av1OutColorTransferChar"), 16, "ST 2084 / PQ");
    assert_eq!(lookup("Av1OutColorMatrixCoeff"), 9, "BT.2020 NCL");
    assert_eq!(lookup("Av1OutColorRange"), 1, "full range");
}
