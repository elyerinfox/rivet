//! AMD AMF AV1 hardware encoder via the Advanced Media Framework runtime.
//!
//! Loads `amfrt64.dll` / `libamfrt64.so.1` at runtime via dlopen. The AV1
//! encoder component is only available on RDNA3+ silicon (Radeon RX
//! 7000 series and later). On older GPUs `CreateComponent` returns
//! `AMF_NOT_SUPPORTED` and we surface that to `select_encoder`'s
//! fallback chain.
//!
//! Session flow (mirroring the AMF sample `VCEEncoderD3D11` adapted
//! for AV1 host-memory submission):
//! 1. dlopen `amfrt64.dll` / `libamfrt64.so.1`
//! 2. AMFInit(AMF_VERSION, &factory)
//! 3. factory->CreateContext(&ctx); ctx->InitDX11(null)  /* Windows */
//!    (or ctx->InitVulkan(null) on Linux — AMF picks the first AMD GPU)
//! 4. factory->CreateComponent(ctx, AMFVideoEncoderVCN_AV1, &encoder)
//! 5. encoder->SetProperty(USAGE = TRANSCODING)        /* baseline */
//! 6. encoder->SetProperty(RATE_CONTROL_METHOD = ...)  /* from adapter */
//! 7. encoder->SetProperty(Q_INDEX_INTRA/INTER, QUALITY_PRESET,
//!    GOP_SIZE, tile count, AQ, OUTPUT_MODE, ...)
//! 8. encoder->Init(NV12, width, height)
//! 9. Per frame:
//!    - ctx->AllocSurface(HOST, NV12, w, h, &surf)
//!    - copy YUV420p → NV12 into surf's Y and UV planes
//!    - surf->SetPts(frame.pts_ticks); surf->SetProperty(FORCE_KEY)
//!    - encoder->SubmitInput(surf); (release surf)
//!    - loop: encoder->QueryOutput(&data); on AMF_OK read AMFBuffer
//!      native pointer → copy into EncodedPacket; on AMF_REPEAT break
//! 10. Flush: encoder->Drain(); drain QueryOutput until AMF_EOF
//! 11. Drop order: encoder->Terminate → encoder.Release → ctx.Terminate
//!     → ctx.Release → library handle drops last (it provides the code
//!     behind every vtable pointer we just called).
//!
//! # AMF_INPUT_FULL retry policy (#59 follow-up)
//!
//! AMF signals `AMF_INPUT_FULL` when the encoder's internal input queue
//! is saturated. The SDK's `AMFComponent` header documents this as a
//! **transient** status — NOT a failure. The correct sequence is:
//!
//!   1. Do NOT release the surface. The surface's caller-held ref is
//!      still valid, and releasing it makes the retry a use-after-free.
//!   2. Drain at least one output packet via `QueryOutput` to free a
//!      slot in the input queue.
//!   3. Retry `SubmitInput` with the SAME surface pointer.
//!   4. Only after the eventual `AMF_OK` (or `AMF_NEED_MORE_INPUT`)
//!      does the encoder take its own ref — we then release our caller-
//!      held ref.
//!
//! The ring-buffer of `RING_SIZE` pre-tracked slots follows Squad-5's
//! NVENC pattern for visibility and test coverage. Each AMF surface is
//! allocated fresh per frame (AMF's ref-counted memory model means the
//! encoder retains its own ref on submitted surfaces until the frame is
//! done, so there is nothing to reuse slot-to-slot as in NVENC); the
//! ring index is for in-flight bookkeeping and a public diagnostic
//! signal that mirrors the NVENC drain path.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr;

use super::tuning::{self, AmfRateControl};
use super::{AUTO_FROM_TARGET, EncodedPacket, Encoder, EncoderConfig};
// `ColorMetadata` is read via `config.color_metadata` on the non-test
// side (no bare-type mention) and through `use super::*` inside the
// test module; pull it in only under cfg(test) to keep release builds
// warning-clean.
#[cfg(test)]
use crate::frame::ColorMetadata;
use crate::frame::VideoFrame;

mod config;
mod ffi;
mod surface;
#[cfg(test)]
mod tests;

// Bring all sub-module items into the amf module namespace so sibling
// sub-modules can access them via `super::ItemName` and so the encoder
// code in this file can use them unqualified.
use self::config::*;
use self::ffi::*;
use self::surface::*;

// ─── Session container ────────────────────────────────────────────

/// Holds the live AMF objects. Dropped in reverse-acquisition order:
/// encoder first (it holds a strong ref on the context), context
/// second. The library handle that provides every vtable we just
/// called drops LAST via `AmfEncoder`'s field order.
struct AmfSession {
    encoder: *mut c_void,
    context: *mut c_void,
    /// Factory is a singleton owned by the AMF runtime; we get it back
    /// from AMFInit and stash it so we can create more contexts if a
    /// future Reconfigure path needs it. Not reference-counted.
    #[allow(dead_code)]
    factory: *mut c_void,

    width: u32,
    height: u32,
    pts_timescale: u64,
    /// `AMF_SURFACE_NV12` (8-bit) or `AMF_SURFACE_P010` (10-bit).
    /// Captured at session create so `upload_frame_static` knows
    /// which plane width + per-sample byte count to use.
    surface_format: i32,
}

// AMF's COM-style vtables are thread-safe per the SDK's "Thread Safety"
// appendix: every context/component object internally synchronises
// SetProperty / SubmitInput / QueryOutput. We only touch one encoder
// per `AmfEncoder`, so Send is sufficient for tokio migration.
//
// Caveat (systems-review-59-60 #4): AMF's DX11/Vulkan device init creates
// per-thread state on some driver versions. A task migrated mid-encode
// could see device-removed errors. The pipeline's `spawn_blocking`
// ensures the encoder stays on one OS thread for its lifetime, so this
// is theoretical for our usage.
unsafe impl Send for AmfSession {}

impl Drop for AmfSession {
    fn drop(&mut self) {
        unsafe {
            // Encoder first — Terminate releases internal hardware
            // resources before we drop the last COM ref.
            if !self.encoder.is_null() {
                let obj = self.encoder as *mut AmfComponentObj;
                let vt = &*(*obj).vtbl;
                let _ = (vt.terminate)(self.encoder);
                let _ = (vt.release)(self.encoder);
            }
            // Context next — same pattern. The factory is not
            // reference-counted and is owned by the runtime; do not
            // Release it.
            if !self.context.is_null() {
                let obj = self.context as *mut AmfContextObj;
                let vt = &*(*obj).vtbl;
                let _ = (vt.terminate)(self.context);
                let _ = (vt.release)(self.context);
            }
        }
    }
}

// ─── Encoder implementation ───────────────────────────────────────

// Field order matters for drop: session drops BEFORE _runtime_lib, so
// all the vtable calls inside `AmfSession::drop` still resolve to
// valid code. Library handle is declared LAST (Reference §10.8 —
// struct fields drop in source order).
pub struct AmfEncoder {
    config: EncoderConfig,
    session: Option<AmfSession>,
    encoded_packets: Vec<EncodedPacket>,
    packet_cursor: usize,
    flushed: bool,
    frame_counter: u32,
    /// Current ring slot. Advances modulo `RING_SIZE` per successful
    /// `SubmitInput`. Mirrors NVENC's `ring_idx` for observational
    /// parity and in-flight bookkeeping.
    ring_idx: usize,
    _runtime_lib: libloading::Library,
}

impl AmfEncoder {
    pub fn new(config: EncoderConfig, gpu_index: u32) -> Result<Self> {
        // AMF currently encodes AV1 only (AMFVideoEncoderVCN_AV1). H.264/H.265
        // output is validated on Intel QSV; native AMF H.264 (VCE_AVC) / H.265
        // (HEVC) is a hardware-verification follow-up (no AMD card here). Reject
        // rather than silently emit AV1 for a non-AV1 request.
        if config.codec != crate::frame::VideoCodec::Av1 {
            anyhow::bail!(
                "AMF encodes AV1 only today; for {:?} output use Intel QSV (Arc+) \
                 — native AMF H.264/H.265 is a follow-up",
                config.codec
            );
        }
        // 1. dlopen the AMF runtime. On Linux the library name is
        //    `libamfrt64.so.1`; on Windows it's `amfrt64.dll`. Both
        //    ship with the Adrenalin driver and Pro driver bundles.
        let runtime_lib = unsafe { libloading::Library::new("libamfrt64.so.1") }
            .or_else(|_| unsafe { libloading::Library::new("libamfrt64.so") })
            .or_else(|_| unsafe { libloading::Library::new("amfrt64.dll") })
            .context("loading AMF runtime library (AMD driver not present?)")?;

        unsafe {
            // 2. Factory.
            let amf_init: libloading::Symbol<FnAmfInit> =
                runtime_lib.get(b"AMFInit").context("AMFInit symbol")?;
            let mut factory: *mut c_void = ptr::null_mut();
            let rc = amf_init(AMF_VERSION, &mut factory);
            if rc != AMF_OK || factory.is_null() {
                bail!("AMFInit failed: {rc}");
            }

            // 3. Context.
            let mut context: *mut c_void = ptr::null_mut();
            let factory_obj = factory as *mut AmfFactoryObj;
            let factory_vt = &*(*factory_obj).vtbl;
            let rc = (factory_vt.create_context)(factory, &mut context);
            if rc != AMF_OK || context.is_null() {
                bail!("AMFFactory::CreateContext failed: {rc}");
            }

            // Initialize the context on a real GPU. We try DX11 first
            // (Windows / WSL2), then Vulkan (Linux). A null device ptr
            // tells AMF to pick the first AMD adapter; the caller's
            // `gpu_index` threads through the pipeline but AMF itself
            // does not expose an ordinal-based init — the driver
            // deterministically picks adapter 0 unless a VkPhysicalDevice
            // or D3D11Device is passed, so multi-AMD hosts require the
            // caller to also set `AGS_DESIRED_ADAPTER_ID` env var.
            // We emit a debug log when gpu_index != 0 so the ops team
            // can notice.
            if gpu_index != 0 {
                tracing::warn!(
                    gpu_index,
                    "AMF init picks adapter 0 unconditionally; \
                     multi-AMD hosts may need external adapter routing"
                );
            }
            let context_obj = context as *mut AmfContextObj;
            let context_vt = &*(*context_obj).vtbl;

            // Try DX11 (both Windows and WSL2 ship a DX11 runtime that
            // AMF can target). If not available — e.g., bare-metal
            // Linux — fall through to Vulkan.
            let rc_dx11 = (context_vt.init_dx11)(context, ptr::null_mut(), 0);
            if rc_dx11 != AMF_OK {
                let rc_vk = (context_vt.init_vulkan)(context, ptr::null_mut());
                if rc_vk != AMF_OK {
                    // Fail → drop context, bail.
                    (context_vt.release)(context);
                    bail!("AMFContext::InitDX11 ({rc_dx11}) and InitVulkan ({rc_vk}) both failed");
                }
            }

            // 4. Encoder component.
            let component_id = wide("AMFVideoEncoderVCN_AV1");
            let mut encoder: *mut c_void = ptr::null_mut();
            let rc = (factory_vt.create_component)(
                factory,
                context,
                component_id.as_ptr(),
                &mut encoder,
            );
            if rc != AMF_OK || encoder.is_null() {
                (context_vt.terminate)(context);
                (context_vt.release)(context);
                bail!(
                    "AMFFactory::CreateComponent(AMFVideoEncoderVCN_AV1) failed: {rc} — RDNA3+ GPU required"
                );
            }

            let encoder_obj = encoder as *mut AmfComponentObj;
            let encoder_vt = &*(*encoder_obj).vtbl;

            // 5. Apply tuning adapter params.
            let tp =
                tuning::amf_av1_params(config.target, config.tier, config.width, config.height);

            // Legacy quality override: if caller passed a concrete
            // `config.quality`, use it as the CQP q-index (0..255).
            // Otherwise use the adapter's derived value.
            let q_intra = if config.quality == AUTO_FROM_TARGET {
                tp.q_index_intra
            } else {
                // Caller-provided quality is a 0..63 CQ scale (NVENC-
                // compatible); scale up 4× to match AMF's 0..255 range.
                ((config.quality as u32 * 4).min(255)) as u8
            };
            let q_inter = q_intra.saturating_add(8);

            // Baseline: USAGE_TRANSCODING picks driver-tuned defaults,
            // then override every knob we care about so the behaviour
            // does not drift when AMD ships a new driver that tweaks
            // the USAGE preset internals.
            set_int_property(encoder, encoder_vt, "Av1Usage", AMF_USAGE_TRANSCODING)?;
            set_int_property(
                encoder,
                encoder_vt,
                "Av1RateControlMethod",
                // ChunkSeamMode::ParallelConstQp forces constant-QP so stitched
                // chunk seams are quality-flat; the QIndex below comes from the
                // tuning CQ, so quality still tracks the target.
                if config.constant_qp {
                    AMF_RC_CQP
                } else {
                    match tp.rc_mode {
                        AmfRateControl::Cqp => AMF_RC_CQP,
                        AmfRateControl::QualityVbr => AMF_RC_QUALITY_VBR,
                    }
                },
            )?;
            set_int_property(
                encoder,
                encoder_vt,
                "Av1QualityPreset",
                amf_quality_preset_i64(tp.quality_preset),
            )?;
            set_int_property(encoder, encoder_vt, "Av1QIndexIntra", q_intra as i64)?;
            set_int_property(encoder, encoder_vt, "Av1QIndexInter", q_inter as i64)?;
            if tp.rc_mode == AmfRateControl::QualityVbr {
                set_int_property(
                    encoder,
                    encoder_vt,
                    "Av1QvbrQualityLevel",
                    tp.qvbr_quality as i64,
                )?;
            }
            set_int_property(
                encoder,
                encoder_vt,
                "Av1GOPSize",
                config.keyframe_interval as i64,
            )?;
            set_int_property(encoder, encoder_vt, "Av1AQMode", tp.aq_mode as i64)?;
            set_int_property(
                encoder,
                encoder_vt,
                "Av1TilesPerFrame",
                tp.tiles_per_frame as i64,
            )?;
            // Frame-level LOB output — mandatory for MP4 muxing so
            // every OBU carries `obu_has_size_field = 1`.
            set_int_property(encoder, encoder_vt, "Av1OutputMode", AMF_OUTPUT_MODE_FRAME)?;

            // Squad-22: bit-depth + color signalling dispatch. The bit
            // depth property tells AMF to write `BitDepth=10` into the
            // AV1 sequence header; the color-* properties write the
            // four H.273 codes into the same header. AMF infers
            // `color_description_present_flag = 1` when any of the
            // three primaries/transfer/matrix codes is non-zero, so
            // setting them is sufficient — we don't have a separate
            // present-flag knob to toggle (unlike NVENC).
            let surface_fmt = amf_surface_format_for(config.pixel_format)?;
            let color_bit_depth = amf_color_bit_depth_for(config.pixel_format);
            set_int_property(encoder, encoder_vt, "Av1ColorBitDepth", color_bit_depth)?;
            // Color signalling — wire ColorMetadata. Even SDR jobs go
            // through this block so the BT.709 codes land in the OBU
            // header explicitly (rather than via "unspecified" which
            // some ABR client libraries treat as "must guess from
            // resolution + transfer", producing inconsistent gamma).
            let cm = &config.color_metadata;
            set_int_property(
                encoder,
                encoder_vt,
                "Av1OutColorPrimaries",
                cm.colour_primaries as i64,
            )?;
            set_int_property(
                encoder,
                encoder_vt,
                "Av1OutColorTransferChar",
                transfer_to_h273(cm.transfer),
            )?;
            set_int_property(
                encoder,
                encoder_vt,
                "Av1OutColorMatrixCoeff",
                cm.matrix_coefficients as i64,
            )?;
            set_int_property(
                encoder,
                encoder_vt,
                "Av1OutColorRange",
                if cm.full_range { 1 } else { 0 },
            )?;

            tracing::info!(
                width = config.width,
                height = config.height,
                target = ?config.target,
                tier = ?config.tier,
                q_index_intra = q_intra,
                q_index_inter = q_inter,
                qvbr_quality = tp.qvbr_quality,
                rc_mode = ?tp.rc_mode,
                quality_preset = ?tp.quality_preset,
                tiles_per_frame = tp.tiles_per_frame,
                ring_size = RING_SIZE,
                "AMF AV1 tuning applied"
            );

            // 6. Init the encoder on the dispatched input format. AV1
            // VCN consumes NV12 (8-bit) or P010 (10-bit) — same
            // interleaved-chroma plane layout, different sample width.
            let rc = (encoder_vt.init)(
                encoder,
                surface_fmt,
                config.width as i32,
                config.height as i32,
            );
            if rc != AMF_OK {
                (encoder_vt.release)(encoder);
                (context_vt.terminate)(context);
                (context_vt.release)(context);
                bail!(
                    "AMFComponent::Init(AV1, {fmt}, {w}x{h}) failed: {rc} \
                     (surface format dispatched for {pf:?})",
                    fmt = surface_fmt,
                    w = config.width,
                    h = config.height,
                    pf = config.pixel_format,
                );
            }

            let session = AmfSession {
                encoder,
                context,
                factory,
                width: config.width,
                height: config.height,
                // AMF uses 100-ns ticks for PTS. We receive PTS in u64
                // "sample counts" from the decoder, and convert by
                // multiplying by (10_000_000 / frame_rate).
                pts_timescale: (10_000_000.0f64 / config.frame_rate).round() as u64,
                surface_format: surface_fmt,
            };

            tracing::info!(
                width = config.width,
                height = config.height,
                gpu = gpu_index,
                "AMF AV1 encoder ready"
            );

            Ok(Self {
                config,
                session: Some(session),
                encoded_packets: Vec::new(),
                packet_cursor: 0,
                flushed: false,
                frame_counter: 0,
                ring_idx: 0,
                _runtime_lib: runtime_lib,
            })
        }
    }

    // Surface upload is a free function (`upload_frame_static`) so it
    // doesn't need `&AmfSession` and can be called without interfering
    // with `&mut self` borrows on `AmfEncoder`.

    fn encode_one(&mut self, frame: &VideoFrame) -> Result<()> {
        // Borrow the session through encode_one. The encoder/context
        // raw pointers are read from `&self.session` once and *not*
        // snapshotted into a plain-data copy. This way, a future
        // refactor that calls `self.session.take()` inside the
        // unsafe block is a compile error rather than a silent UAF.
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("encode_one called after session drop"))?;
        let encoder_ptr = session.encoder;
        let snap = SessionSnapshot {
            encoder: session.encoder,
            context: session.context,
            width: session.width,
            height: session.height,
            pts_timescale: session.pts_timescale,
            surface_format: session.surface_format,
        };
        let force_key = self
            .frame_counter
            .is_multiple_of(self.effective_keyframe_interval());
        let packets = &mut self.encoded_packets;
        let ring_slot = self.ring_idx;

        let outcome = unsafe {
            // Wrap the whole unsafe block in catch_unwind so a panic
            // in our FFI path never unwinds across the AMF C ABI (UB).
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let raw_surface = upload_frame_static(&snap, frame)?;
                // RAII guard: surface is released on every exit path
                // unless `transfer_to_encoder` is called after a
                // successful SubmitInput. This is the safety net for
                // panics partway through property sets / retries —
                // catch_unwind itself stops FFI unwinds, but inside
                // the closure any `?` or `bail!` after alloc would
                // otherwise leak the caller-held ref (codec-review-
                // 59-60 A-A4).
                let mut guard = SurfaceGuard::new(raw_surface);

                if force_key {
                    let surface_obj = guard.as_ptr() as *mut AmfSurfaceObj;
                    let surface_vt = &*(*surface_obj).vtbl;
                    let key = AmfVariant::int64(1);
                    let name = prop("Av1ForceKeyFrame");
                    (surface_vt.set_property)(guard.as_ptr(), name.as_ptr(), key);
                }

                // Submit with bounded retry on AMF_INPUT_FULL / AMF_REPEAT.
                // Both statuses are transient per AMF SDK: the caller
                // must drain output (freeing a slot in the encoder's
                // input queue) and retry with the SAME surface pointer.
                // Releasing the surface BEFORE the successful retry
                // would UAF the second SubmitInput — that's the bug
                // this task is fixing (codec-review-59-60 AMF-5).
                submit_with_backpressure(packets, encoder_ptr, &mut guard)?;

                // Drain whatever's ready now. AMF sometimes produces a
                // packet per SubmitInput, sometimes not.
                drain_until_hungry_raw(packets, encoder_ptr)?;
                Ok::<(), anyhow::Error>(())
            }));

            match result {
                Ok(inner) => inner,
                Err(_panic) => {
                    bail!("panic in AMF encode path — aborting rather than unwinding across FFI")
                }
            }
        };

        outcome?;
        self.frame_counter += 1;
        self.ring_idx = (ring_slot + 1) % RING_SIZE;
        Ok(())
    }

    fn effective_keyframe_interval(&self) -> u32 {
        if self.config.keyframe_interval == 0 {
            240
        } else {
            self.config.keyframe_interval
        }
    }

    // drain_until_hungry is a free function (see end of file) so it
    // operates on `&mut packets` rather than `&mut self`. This keeps
    // `&self.session` alive across the call and prevents a future
    // `self.session.take()` introduction from silently turning the
    // raw encoder pointer into a UAF.

    fn flush_drain(&mut self) -> Result<()> {
        let encoder_ptr = match &self.session {
            Some(s) => s.encoder,
            None => return Ok(()),
        };
        let packets = &mut self.encoded_packets;
        // Wrap the whole FFI path in catch_unwind for the same reason
        // as encode_one — Drain + QueryOutput + buffer_to_packet all
        // allocate (Bytes::copy_from_slice) and a panic unwinding
        // across the AMF C ABI is UB in debug/test builds.
        // systems-review-59-60.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            let encoder_obj = encoder_ptr as *mut AmfComponentObj;
            let encoder_vt = &*(*encoder_obj).vtbl;
            // AMF Drain() marks the pipeline as "no more input will
            // ever arrive" — after this, QueryOutput drains the
            // internal reorder buffer until AMF_EOF.
            let rc = (encoder_vt.drain)(encoder_ptr);
            if rc != AMF_OK && rc != AMF_REPEAT {
                bail!("AMF Drain failed: {rc}");
            }
            drain_until_hungry_raw(packets, encoder_ptr)?;
            Ok::<(), anyhow::Error>(())
        }));
        match result {
            Ok(inner) => inner,
            Err(_panic) => {
                bail!("panic in AMF flush path — aborting rather than unwinding across FFI")
            }
        }
    }

    /// Suppress unused warning — `c_int` type is here for future
    /// NV_ENC-style rc tables where we need to pass a C `int` through.
    #[allow(dead_code)]
    fn _suppress_unused_c_int() -> c_int {
        0
    }
}

impl Encoder for AmfEncoder {
    fn send_frame(&mut self, frame: &VideoFrame) -> Result<()> {
        if frame.format != self.config.pixel_format {
            bail!(
                "AMF session was initialized with {:?} input but frame is {:?}",
                self.config.pixel_format,
                frame.format
            );
        }
        self.encode_one(frame)
    }

    fn flush(&mut self) -> Result<()> {
        if !self.flushed {
            self.flush_drain()?;
            self.flushed = true;
        }
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Option<EncodedPacket>> {
        if self.packet_cursor < self.encoded_packets.len() {
            let pkt = self.encoded_packets[self.packet_cursor].clone();
            self.packet_cursor += 1;
            Ok(Some(pkt))
        } else {
            Ok(None)
        }
    }
}

/// Submit `guard.as_ptr()` to the encoder, retrying on transient
/// back-pressure statuses. On success the guard is marked as
/// transferred and its `Drop` becomes a no-op (the encoder's internal
/// ref now owns the surface lifetime). On hard failure the guard's
/// `Drop` releases our caller-held ref exactly once.
///
/// The #59 follow-up bug: previously the caller released the surface
/// BEFORE the retry on `AMF_INPUT_FULL`. That made the retry a
/// use-after-free because AMF rejected the frame (no ownership taken)
/// and we had just dropped our only ref. The fix is to keep the
/// caller-held ref alive across the retry loop — exactly what the
/// `SurfaceGuard` + `transfer_to_encoder` pattern encodes.
///
/// Retry policy: bounded at `INPUT_FULL_MAX_RETRIES` attempts with
/// exponential backoff starting at `INPUT_FULL_BACKOFF_MS_INITIAL` ms
/// and capped at `INPUT_FULL_BACKOFF_MS_MAX` ms. A drain pass between
/// each retry attempts to free an input slot. This is not unbounded
/// so a stuck driver can't spin us forever.
unsafe fn submit_with_backpressure(
    packets: &mut Vec<EncodedPacket>,
    encoder: *mut c_void,
    guard: &mut SurfaceGuard,
) -> Result<()> {
    unsafe {
        let encoder_obj = encoder as *mut AmfComponentObj;
        let encoder_vt = &*(*encoder_obj).vtbl;

        let mut backoff_ms = INPUT_FULL_BACKOFF_MS_INITIAL;
        for attempt in 0..=INPUT_FULL_MAX_RETRIES {
            let rc = (encoder_vt.submit_input)(encoder, guard.as_ptr());
            match rc {
                AMF_OK | AMF_NEED_MORE_INPUT => {
                    // Per AMF SDK "Reference Counting" appendix:
                    // SubmitInput takes a fresh internal ref on
                    // AMF_OK / AMF_NEED_MORE_INPUT. Our caller-held
                    // ref is now redundant — release it exactly once
                    // and mark the guard so Drop is a no-op at
                    // scope exit.
                    let surface_obj = guard.as_ptr() as *mut AmfSurfaceObj;
                    let surface_vt = &*(*surface_obj).vtbl;
                    (surface_vt.release)(guard.as_ptr());
                    guard.transfer_to_encoder();
                    return Ok(());
                }
                AMF_INPUT_FULL | AMF_REPEAT => {
                    // Transient — drain output to free an input slot,
                    // then retry. Critically: the surface is NOT
                    // released here; the guard still owns the caller-
                    // held ref and the same pointer is handed back
                    // to the retry.
                    if attempt == INPUT_FULL_MAX_RETRIES {
                        tracing::warn!(
                            status = rc,
                            attempts = attempt + 1,
                            "AMF SubmitInput backpressure exceeded retry budget — \
                             surface still caller-owned, releasing via guard"
                        );
                        bail!(
                            "AMF SubmitInput stuck at {rc} after {} attempts",
                            attempt + 1
                        );
                    }
                    // Drain first; in steady state one drain frees
                    // exactly one input slot.
                    drain_until_hungry_raw(packets, encoder)?;
                    // If drain returned without any output (encoder
                    // still warming up or mid-reorder), spin the
                    // current OS thread for `backoff_ms` so we don't
                    // busy-loop the driver. Yields on Windows and
                    // Linux — not a blocking syscall.
                    if attempt > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                        backoff_ms = (backoff_ms * 2).min(INPUT_FULL_BACKOFF_MS_MAX);
                    }
                    continue;
                }
                other => {
                    // Hard error: surface still caller-owned. Guard's
                    // Drop will release our ref on return from bail.
                    tracing::warn!(
                        status = other,
                        "AMF SubmitInput hard failure — surface still caller-owned, \
                         releasing via guard"
                    );
                    bail!("AMF SubmitInput failed: {other}");
                }
            }
        }
        // Unreachable — loop exit always via return/bail above.
        unreachable!("submit_with_backpressure loop invariant violated")
    }
}

/// Drain `QueryOutput` into `packets` until the encoder returns
/// `AMF_REPEAT` (no more data available yet), `AMF_EOF`, or
/// `AMF_NEED_MORE_INPUT`. Free function (not a method on AmfEncoder)
/// so it takes `&mut Vec<EncodedPacket>` rather than `&mut self`.
/// This keeps `&self.session` alive through the call and makes a
/// future `self.session.take()` inside the unsafe block a compile
/// error rather than a silent UAF. systems-review-59-60.
unsafe fn drain_until_hungry_raw(
    packets: &mut Vec<EncodedPacket>,
    encoder: *mut c_void,
) -> Result<()> {
    unsafe {
        loop {
            let encoder_obj = encoder as *mut AmfComponentObj;
            let encoder_vt = &*(*encoder_obj).vtbl;
            let mut data: *mut c_void = ptr::null_mut();
            let rc = (encoder_vt.query_output)(encoder, &mut data);
            match rc {
                AMF_OK => {
                    if data.is_null() {
                        continue;
                    }
                    if let Some(pkt) = buffer_to_packet(data)? {
                        packets.push(pkt);
                    }
                    // buffer_to_packet released any QueryInterface ref
                    // it took; drop the AMFData ref here.
                    let obj = data as *mut AmfBufferObj;
                    ((*(*obj).vtbl).release)(data);
                }
                // AMF_REPEAT on QueryOutput means "no more data this
                // round but more may appear later" — normal hungry
                // return for the drain loop.
                AMF_REPEAT => return Ok(()),
                // AMF_EOF is the expected terminator after `Drain()`
                // has been called — signals the encoder has flushed
                // its reorder buffer and no further output will come.
                // Treated as a clean empty return.
                AMF_EOF => return Ok(()),
                // AMF_NEED_MORE_INPUT on QueryOutput means the encoder
                // requires more frames before it can emit anything
                // (typical for initial lookahead warmup / reorder).
                // Equivalent to "no packet yet"; clean empty return.
                AMF_NEED_MORE_INPUT => return Ok(()),
                other => bail!("AMF QueryOutput failed: {other}"),
            }
        }
    }
}

/// Cross-cast an AMFData* to AMFBuffer* via QueryInterface and copy
/// its native bytes into an EncodedPacket. Free function for the same
/// reason as `drain_until_hungry_raw` — no `&self` aliasing concerns.
///
/// SAFETY precondition (codec-review-59-60 M-A1): we rely on AMFData
/// and AMFBuffer sharing the first three vtable slots (QueryInterface,
/// Acquire, Release — COM IUnknown). This is guaranteed by the AMF
/// SDK's AMFInterface inheritance chain. If QueryInterface fails we
/// bail rather than fall through to `treat AMFData as AMFBuffer` — a
/// future SDK rev that reorders AMFData vtable entries past slot 3
/// would otherwise call `GetSize` at the wrong offset and read garbage.
unsafe fn buffer_to_packet(data: *mut c_void) -> Result<Option<EncodedPacket>> {
    unsafe {
        let data_obj = data as *mut AmfBufferObj;
        let data_vt = &*(*data_obj).vtbl;

        let mut buffer: *mut c_void = ptr::null_mut();
        let qi_rc =
            (data_vt.query_interface)(data, AMF_IID_BUFFER.as_ptr() as *const c_void, &mut buffer);
        if qi_rc != 0 || buffer.is_null() {
            // Fail loudly rather than splatting bytes through a
            // possibly-shifted vtable layout.
            bail!("AMFData::QueryInterface(AMFBuffer) failed: {qi_rc}");
        }
        let buffer_obj = buffer as *mut AmfBufferObj;
        let buffer_vt = &*(*buffer_obj).vtbl;

        let size = (buffer_vt.get_size)(buffer_obj as *mut c_void);
        let native = (buffer_vt.get_native)(buffer_obj as *mut c_void) as *const u8;
        if size == 0 || native.is_null() {
            (buffer_vt.release)(buffer_obj as *mut c_void);
            return Ok(None);
        }

        let slice = std::slice::from_raw_parts(native, size);
        let data_bytes = Bytes::copy_from_slice(slice);

        let pts_ticks = (buffer_vt.get_pts)(buffer_obj as *mut c_void) as u64;

        // Read the frame-type property so we can tag keyframes in
        // the EncodedPacket. Bailing on the Get is fine — we just
        // fall back to "not a keyframe".
        let prop_name = prop("Av1OutputFrameType");
        let mut var: AmfVariant = AmfVariant {
            ty: 0,
            _pad: 0,
            value: [0; 24],
        };
        let is_keyframe =
            if (buffer_vt.get_property)(buffer_obj as *mut c_void, prop_name.as_ptr(), &mut var)
                == AMF_OK
                && var.ty == AMF_VARIANT_INT64
            {
                let mut v_bytes = [0u8; 8];
                v_bytes.copy_from_slice(&var.value[..8]);
                let v = i64::from_le_bytes(v_bytes);
                v == AMF_OUTPUT_FRAME_TYPE_KEY || v == AMF_OUTPUT_FRAME_TYPE_INTRA_ONLY
            } else {
                false
            };

        (buffer_vt.release)(buffer_obj as *mut c_void);

        Ok(Some(EncodedPacket {
            data: data_bytes,
            pts: pts_ticks,
            is_keyframe,
        }))
    }
}
