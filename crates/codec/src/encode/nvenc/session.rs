//! Live encode session + CUDA context RAII guard.

use anyhow::{bail, Result};
use std::ffi::c_void;
use std::os::raw::c_uint;
use std::ptr;

use super::constants::{
    CUcontext, FnCuCtxDestroy, FnCuCtxPopCurrent, FnCuCtxPushCurrent, FnNvEncDestroyBitstreamBuffer,
    FnNvEncDestroyEncoder, FnNvEncDestroyInputBuffer, FnNvEncEncodePicture, FnNvEncLockBitstream,
    FnNvEncLockInputBuffer, FnNvEncUnlockBitstream, FnNvEncUnlockInputBuffer, RING_SIZE,
};

/// Holds the live encode session + per-frame resources.
/// Dropped together so teardown order is enforced.
///
/// SAFETY: NVENC encoder handles and CUDA contexts are opaque pointers
/// accessed only from the thread that holds `Self`. The encoder's CUDA
/// context must be pushed current before any `fn_encode_picture` etc.
/// call — see `ctx_scope()`.
pub(super) struct EncodeSession {
    pub(super) encoder: *mut c_void,
    /// Ring of N input surfaces. Rotated per `EncodePicture` call.
    pub(super) input_buffers: [*mut c_void; RING_SIZE],
    /// Matching ring of N output (bitstream) buffers. Each input
    /// surface is paired 1:1 with an output surface so lock/unlock of
    /// bitstream i can proceed while input i+1 is being copied.
    pub(super) bitstream_buffers: [*mut c_void; RING_SIZE],
    pub(super) cuda_ctx: CUcontext,
    pub(super) width: u32,
    pub(super) height: u32,
    /// `NV_ENC_BUFFER_FORMAT_*` value chosen at session create time.
    /// Drives both the upload routine (8-bit byte copy vs 16-bit P010
    /// `<<6` shift) and the per-frame `NV_ENC_PIC_PARAMS.buffer_fmt`
    /// field — has to match `NV_ENC_INITIALIZE_PARAMS.buffer_format`
    /// or NVENC returns INVALID_PARAM on the first encode.
    pub(super) buffer_format: c_uint,

    // Function pointers captured up front. NVENC's fn-list table holds
    // opaque void* so we cast back at call time.
    pub(super) fn_destroy_input_buffer: FnNvEncDestroyInputBuffer,
    pub(super) fn_destroy_bitstream_buffer: FnNvEncDestroyBitstreamBuffer,
    pub(super) fn_lock_input_buffer: FnNvEncLockInputBuffer,
    pub(super) fn_unlock_input_buffer: FnNvEncUnlockInputBuffer,
    pub(super) fn_encode_picture: FnNvEncEncodePicture,
    pub(super) fn_lock_bitstream: FnNvEncLockBitstream,
    pub(super) fn_unlock_bitstream: FnNvEncUnlockBitstream,
    pub(super) fn_destroy_encoder: FnNvEncDestroyEncoder,

    pub(super) fn_cu_ctx_destroy: FnCuCtxDestroy,
    pub(super) fn_cu_ctx_push: FnCuCtxPushCurrent,
    pub(super) fn_cu_ctx_pop: FnCuCtxPopCurrent,
}

unsafe impl Send for EncodeSession {}

impl EncodeSession {
    /// Push this session's CUDA context on the calling thread for the
    /// duration of the returned guard. Required because tokio workers
    /// may migrate between OS threads — without an explicit push the
    /// encoder calls hit CUDA_ERROR_INVALID_CONTEXT.
    pub(super) unsafe fn ctx_scope(&self) -> Result<CtxScope> {
        unsafe { CtxScope::push(self.cuda_ctx, self.fn_cu_ctx_push, self.fn_cu_ctx_pop) }
    }
}

impl Drop for EncodeSession {
    fn drop(&mut self) {
        unsafe {
            // Push context so NvEncDestroy* calls run in the right
            // CUDA context (teardown on a different thread would
            // otherwise fail). Scope guard pops on exit.
            let _scope =
                CtxScope::push(self.cuda_ctx, self.fn_cu_ctx_push, self.fn_cu_ctx_pop).ok();

            // Teardown ring in REVERSE allocation order so the last
            // slot to be created is the first to go — matches the
            // standard RAII teardown convention and keeps the SDK's
            // internal handle tables consistent.
            for i in (0..RING_SIZE).rev() {
                if !self.input_buffers[i].is_null() {
                    (self.fn_destroy_input_buffer)(self.encoder, self.input_buffers[i]);
                }
                if !self.bitstream_buffers[i].is_null() {
                    (self.fn_destroy_bitstream_buffer)(self.encoder, self.bitstream_buffers[i]);
                }
            }
            if !self.encoder.is_null() {
                (self.fn_destroy_encoder)(self.encoder);
            }
            // Drop the scope guard BEFORE destroying the context it
            // references — explicit drop makes the ordering obvious.
            drop(_scope);
            if !self.cuda_ctx.is_null() {
                (self.fn_cu_ctx_destroy)(self.cuda_ctx);
            }
        }
    }
}

// ─── RAII: CUDA context scope guard ───────────────────────────────
pub(super) struct CtxScope {
    pop: FnCuCtxPopCurrent,
}

impl CtxScope {
    pub(super) unsafe fn push(
        ctx: CUcontext,
        push: FnCuCtxPushCurrent,
        pop: FnCuCtxPopCurrent,
    ) -> Result<Self> {
        unsafe {
            if push(ctx) != 0 {
                bail!("cuCtxPushCurrent failed");
            }
            Ok(Self { pop })
        }
    }
}

impl Drop for CtxScope {
    fn drop(&mut self) {
        let mut popped: CUcontext = ptr::null_mut();
        unsafe {
            (self.pop)(&mut popped);
        }
    }
}
