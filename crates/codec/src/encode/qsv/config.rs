//! Rate-control slot mapping, codec/format dispatch helpers, and
//! `zeroed_video_param` — pure functions with no FFI side-effects.

use anyhow::{Result, bail};

use crate::encode::tuning::QsvRateControl;
use crate::frame::{PixelFormat, TransferFn};
use crate::qsv_ffi::{MfxFrameInfo, MfxInfoMfx, MfxVideoParam};

use super::ffi::{
    MFX_CODEC_AV1, MFX_CODEC_AVC, MFX_CODEC_HEVC, MFX_FOURCC_NV12, MFX_FOURCC_P010,
    MFX_PROFILE_AV1_MAIN, MFX_PROFILE_AVC_HIGH, MFX_PROFILE_HEVC_MAIN,
    MFX_PROFILE_HEVC_MAIN10, MFX_TARGET_CHROMAFORMAT_YUV420_PLUS1,
};

// ─── Codec-id mapping ─────────────────────────────────────────────────────────

/// Map our `VideoCodec` + pixel format to the QSV `(codec_id, codec_profile)`
/// pair. 10-bit H.265 selects Main 10; 10-bit H.264 stays High (QSV rejects
/// it at Query/Init).
pub(super) fn qsv_codec_ids(
    codec: crate::frame::VideoCodec,
    fmt: PixelFormat,
) -> (u32, u16) {
    let ten_bit = fmt == PixelFormat::Yuv420p10le;
    match codec {
        crate::frame::VideoCodec::Av1  => (MFX_CODEC_AV1, MFX_PROFILE_AV1_MAIN),
        crate::frame::VideoCodec::H264 => (MFX_CODEC_AVC, MFX_PROFILE_AVC_HIGH),
        crate::frame::VideoCodec::H265 if ten_bit => {
            (MFX_CODEC_HEVC, MFX_PROFILE_HEVC_MAIN10)
        }
        crate::frame::VideoCodec::H265 => (MFX_CODEC_HEVC, MFX_PROFILE_HEVC_MAIN),
    }
}

// ─── Squad-22: per-pixel-format dispatch ──────────────────────────────────────
//
// QSV AV1 takes either NV12 (8-bit, FrameInfo BitDepth* = 8, Shift=0) or
// P010 (10-bit, BitDepth* = 10, Shift=1).  The Shift bit is mandatory when
// the surface is P010 — without it, oneVPL reads samples from the lower 10
// bits → silently encodes 1/64th-amplitude noise.

/// Map a `PixelFormat` to its oneVPL FOURCC. Bails on unsupported chroma
/// (this encoder is AV1 4:2:0 only).
pub(super) fn qsv_fourcc_for(fmt: PixelFormat) -> Result<u32> {
    match fmt {
        PixelFormat::Yuv420p    => Ok(MFX_FOURCC_NV12),
        PixelFormat::Yuv420p10le => Ok(MFX_FOURCC_P010),
        other => bail!(
            "QSV AV1 expects Yuv420p or Yuv420p10le, got {other:?}"
        ),
    }
}

/// Returns `(BitDepthLuma, BitDepthChroma, Shift)` for the FrameInfo struct
/// given the input pixel format.  Shift=1 is the P010 "valid bits in upper 10"
/// signal — required for 10-bit, must be 0 for 8-bit or oneVPL rejects the
/// param set with INVALID_VIDEO_PARAM.
pub(super) const fn qsv_bit_depth_triple(fmt: PixelFormat) -> (u16, u16, u16) {
    match fmt {
        PixelFormat::Yuv420p10le => (10, 10, 1),
        _ => (8, 8, 0),
    }
}

// Squad-22: 10-bit dispatch helpers — the `(BitDepthLuma, BitDepthChroma, Shift)`
// triple must produce exactly (10, 10, 1) for Yuv420p10le.  The Shift=1 bit
// is critical: without it oneVPL reads samples from the lower 10 bits of each
// P010 word → silently encodes 1/64 amplitude noise. (8, 8, 0) for 8-bit is
// equally non-negotiable — Shift=1 on NV12 surfaces causes the runtime to bail
// with INVALID_VIDEO_PARAM.
const _: () = assert!({
    let (l, c, s) = qsv_bit_depth_triple(PixelFormat::Yuv420p10le);
    l == 10 && c == 10 && s == 1
});
const _: () = assert!({
    let (l, c, s) = qsv_bit_depth_triple(PixelFormat::Yuv420p);
    l == 8 && c == 8 && s == 0
});
// AV1 4:2:0 chroma format = MFX_CHROMAFORMAT_YUV420 (1) + 1 = 2 in the
// oneVPL "plus one" convention used by mfxExtCodingOption3.
const _: () = assert!(MFX_TARGET_CHROMAFORMAT_YUV420_PLUS1 == 2);

// ─── H.273 colour transfer helper ─────────────────────────────────────────────

/// Translate `TransferFn` → ITU-T H.273 numeric code.  Mirrors
/// `nvenc.rs::transfer_to_h273` and `amf.rs::transfer_to_h273` plus the mux
/// helper — single source of HDR signalling truth across the three encoder
/// backends and the container.
pub(super) fn transfer_to_h273(tf: TransferFn) -> u16 {
    match tf {
        TransferFn::Bt709       => 1,
        TransferFn::Bt470Bg     => 4,
        TransferFn::Linear      => 8,
        TransferFn::St2084      => 16,
        TransferFn::AribStdB67  => 18,
        TransferFn::Unspecified => 1,
    }
}

// ─── Target-usage clamping ────────────────────────────────────────────────────

/// Map a `SpeedTier` (via the tuning adapter's `target_usage` output) to the
/// oneVPL 1..7 scale.  Defends against out-of-range values from a future
/// tuning-adapter bug.
///
/// Per vendor/intel/mfxdefs.h:91-93:
/// - 1 = MFX_TARGETUSAGE_BEST_QUALITY
/// - 4 = MFX_TARGETUSAGE_BALANCED
/// - 7 = MFX_TARGETUSAGE_BEST_SPEED
///
/// The tuning adapter at `encode/tuning.rs::qsv_av1_params` clamps to 1..=6
/// (leaves headroom past "best speed" for future driver tuning selections);
/// this helper simply defends against out-of-range values.
pub(super) fn clamp_target_usage(tp_target_usage: u16) -> u16 {
    tp_target_usage.clamp(1, 7)
}

// ─── Rate-control slot mapping ────────────────────────────────────────────────

/// The three `mfxInfoMFX` rc-union slot values a given job produces, before
/// being splatted into `qpi_or_delay / qpp_or_kbps_or_icq / qpb_or_maxkbps`.
/// Pulled out as a standalone function so the slot-assignment logic can be
/// unit-tested without touching any FFI.
///
/// Per vendor/intel/mfxstructs.h:74-89:
/// - CQP: slot0=QPI, slot1=QPP, slot2=QPB
/// - ICQ: slot0=0 (unused), slot1=**ICQQuality**, slot2=0
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RateSlots {
    pub(super) slot0_qpi_or_delay: u16,
    pub(super) slot1_qpp_or_kbps_or_icq: u16,
    pub(super) slot2_qpb_or_maxkbps: u16,
}

pub(super) fn rate_slots_for_rc(
    mode: QsvRateControl,
    qp_i: u16,
    qp_p: u16,
    icq_quality: u16,
) -> RateSlots {
    match mode {
        QsvRateControl::Cqp => RateSlots {
            slot0_qpi_or_delay: qp_i,
            slot1_qpp_or_kbps_or_icq: qp_p,
            slot2_qpb_or_maxkbps: qp_p, // No B-frames; mirror QPP to QPB.
        },
        QsvRateControl::Icq => RateSlots {
            // ICQ mode: slot 0 is the InitialDelayInKB alias and is
            // unread. ICQQuality is **slot 1** per the vendored header
            // (`vendor/intel/mfxstructs.h:83`). Slot 2 is unused.
            // An earlier rev of this file (based on an incorrect reading
            // of upstream in `reviews/codec-review-59-60.md` QSV-1) put
            // ICQQuality in slot 0, which silently resolved to
            // `InitialDelayInKB` and caused oneVPL to fall back to
            // its default ICQQuality=23 for every quality tier.
            slot0_qpi_or_delay: 0,
            slot1_qpp_or_kbps_or_icq: icq_quality,
            slot2_qpb_or_maxkbps: 0,
        },
    }
}

// ─── Zeroed MfxVideoParam helper ──────────────────────────────────────────────

/// Zero-initialise an `MfxVideoParam` for use as Query's `out` param.
/// Carved out as a function so both `new()` and the unit tests share one
/// definition.
pub(super) fn zeroed_video_param() -> MfxVideoParam {
    MfxVideoParam {
        alloc_id: 0,
        reserved: [0; 2],
        reserved3: 0,
        async_depth: 0,
        mfx: MfxInfoMfx {
            reserved: [0; 7],
            low_power: 0,
            brc_param_multiplier: 0,
            frame_info: MfxFrameInfo {
                reserved: [0; 4],
                channel_id: 0,
                bit_depth_luma: 0,
                bit_depth_chroma: 0,
                shift: 0,
                frame_id: [0; 4],
                fourcc: 0,
                width: 0,
                height: 0,
                crop_x: 0,
                crop_y: 0,
                crop_w: 0,
                crop_h: 0,
                frame_rate_ext_n: 0,
                frame_rate_ext_d: 0,
                reserved3: 0,
                aspect_ratio_w: 0,
                aspect_ratio_h: 0,
                pic_struct: 0,
                chroma_format: 0,
                reserved2: 0,
            },
            codec_id: 0,
            codec_profile: 0,
            codec_level: 0,
            num_thread: 0,
            target_usage: 0,
            gop_pic_size: 0,
            gop_ref_dist: 0,
            gop_opt_flag: 0,
            idr_interval: 0,
            rate_control_method: 0,
            qpi_or_delay: 0,
            buffer_size_kb: 0,
            qpp_or_kbps_or_icq: 0,
            qpb_or_maxkbps: 0,
            num_slice: 0,
            num_ref_frame: 0,
            encoded_order: 0,
        },
        _mfx_union_pad: [0; 32],
        protected: 0,
        io_pattern: 0,
        ext_param: std::ptr::null_mut(),
        num_ext_param: 0,
        reserved2: 0,
    }
}
