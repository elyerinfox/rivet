//! AMF property/config building helpers.
//!
//! Wide-string encoding, per-pixel-format dispatch, H.273 transfer-code
//! mapping, quality-preset mapping, and the `set_int_property` helper
//! that drives every `AMFComponent::SetProperty` call.

use anyhow::{bail, Result};
use std::ffi::c_void;

use crate::frame::{PixelFormat, TransferFn};

// Items from ffi.rs accessed via the parent (amf) module's private-use
// re-export (`use self::ffi::*;` in mod.rs).
use super::{
    AMF_AV1_COLOR_BIT_DEPTH_8, AMF_AV1_COLOR_BIT_DEPTH_10, AMF_OK, AMF_SURFACE_NV12,
    AMF_SURFACE_P010, AmfComponentVtbl, AmfVariant,
};

// ─── Wide-string helpers ──────────────────────────────────────────

/// Encode a UTF-16 null-terminated wide string the way AMF expects
/// (the SDK property names are `wchar_t*` — on Windows that's u16,
/// on Linux wchar_t is u32 but AMF's ABI declares `amf_wchar_t = u16`
/// explicitly via its own typedef to stay portable).
pub(super) fn wide(s: &str) -> Vec<u16> {
    let mut out: Vec<u16> = s.encode_utf16().collect();
    out.push(0);
    out
}

// Property-name wide strings, one per SetProperty call we make.
// Stored as constants so we don't re-encode for every frame.
pub(super) fn prop(s: &str) -> Vec<u16> {
    wide(s)
}

// ─── Squad-22: per-pixel-format dispatch ──────────────────────────
//
// AMF VCN AV1 supports NV12 (8-bit) and P010 (10-bit) host-memory
// surfaces; both are interleaved-chroma YUV 4:2:0. Selecting the wrong
// surface format for the input depth produces silent garbage (the
// 8-bit shader path on a wide-word surface reads two adjacent samples
// per byte → noise + halved width).

pub(super) fn amf_surface_format_for(fmt: PixelFormat) -> Result<i32> {
    match fmt {
        PixelFormat::Yuv420p => Ok(AMF_SURFACE_NV12),
        PixelFormat::Yuv420p10le => Ok(AMF_SURFACE_P010),
        other => bail!("AMF AV1 expects Yuv420p or Yuv420p10le, got {other:?}"),
    }
}

pub(super) const fn amf_color_bit_depth_for(fmt: PixelFormat) -> i64 {
    match fmt {
        PixelFormat::Yuv420p10le => AMF_AV1_COLOR_BIT_DEPTH_10,
        _ => AMF_AV1_COLOR_BIT_DEPTH_8,
    }
}

// `amf_color_bit_depth_for` dispatch must agree with the pinned enum
// values in ffi.rs. Cross-check here so a future rename catches both.
const _: () = assert!(amf_color_bit_depth_for(PixelFormat::Yuv420p10le) == 2);
const _: () = assert!(amf_color_bit_depth_for(PixelFormat::Yuv420p) == 1);

/// Translate `TransferFn` → ITU-T H.273 numeric code. Same table as
/// `nvenc.rs::transfer_to_h273` and the mux's `transfer_to_h273` —
/// keeping the three in lockstep means HDR signalling matches across
/// container `colr nclx`, AMF AV1 OBU, and NVENC AV1 OBU.
pub(super) fn transfer_to_h273(tf: TransferFn) -> i64 {
    match tf {
        TransferFn::Bt709 => 1,
        TransferFn::Bt470Bg => 4,
        TransferFn::Linear => 8,
        TransferFn::St2084 => 16,
        TransferFn::AribStdB67 => 18,
        TransferFn::Unspecified => 1,
    }
}

/// Map `AmfQualityPreset` variants to the i64 values the AMF SetProperty
/// ABI expects. The enum's `#[repr(i64)]` makes this effectively a
/// discriminant read, but going through a match keeps the translation
/// explicit and audit-able against the AMD AMF header constants.
pub(super) fn amf_quality_preset_i64(preset: super::tuning::AmfQualityPreset) -> i64 {
    use super::tuning::AmfQualityPreset;
    match preset {
        AmfQualityPreset::HighQuality => 10,
        AmfQualityPreset::Quality => 30,
        AmfQualityPreset::Balanced => 50,
        AmfQualityPreset::Speed => 70,
    }
}

/// Set a single i64-valued property on an AMF component, wide-string
/// encoded. Returns the AMF_RESULT as a Rust `Result` so the call
/// site can bail cleanly when the driver rejects a knob value.
pub(super) unsafe fn set_int_property(
    obj: *mut c_void,
    vt: &AmfComponentVtbl,
    name: &str,
    value: i64,
) -> Result<()> {
    unsafe {
        let wname = wide(name);
        let rc = (vt.set_property)(obj, wname.as_ptr(), AmfVariant::int64(value));
        if rc != AMF_OK {
            bail!("AMF SetProperty({}, {}) failed: {rc}", name, value);
        }
        Ok(())
    }
}
