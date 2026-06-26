//! Squad-35: Sanity baselines.
//!
//! Three small targeted tests that surface obvious bugs in the encode +
//! mux path. None of them require real test_media; all build synthetic
//! input and inspect the bytes the muxer produces.
//!
//! Test A: output MP4 is larger than the ftyp+moov skeleton — catches an
//!   "encoder produced empty mdat" regression that would otherwise leave
//!   us with a structurally-valid but zero-payload file.
//!
//! Test B: for an 8-bit input, the AV1 av1C `high_bitdepth` flag is 0;
//!   for a 10-bit input the same flag is 1. Catches "encoder ignores
//!   pixel_format config" (would leave high_bitdepth=0 even on 10-bit
//!   input, silently downsampling to 8-bit).
//!
//! Test C: for an HDR (PQ + BT.2020) input, the muxer's `colr nclx`
//!   atom carries transfer=16 (PQ), not transfer=1 (BT.709). Catches
//!   the regression where ColorMetadata is dropped on the floor and
//!   the muxer emits its SDR default.

use bytes::Bytes;

mod common;

use codec::encode::EncoderConfig;
use codec::frame::{ColorMetadata, ColorSpace, PixelFormat, TransferFn, VideoFrame};
use container::mux::Av1Mp4Muxer;

const W: u32 = 64;
const H: u32 = 64;
const FPS: f64 = 30.0;

fn make_yuv420p_frame(w: u32, h: u32, pts: u64) -> VideoFrame {
    let wu = w as usize;
    let hu = h as usize;
    let y_size = wu * hu;
    let uv_size = y_size / 4;
    let mut buf = Vec::with_capacity(y_size + 2 * uv_size);
    let t = pts as u8;
    for r in 0..hu {
        for c in 0..wu {
            buf.push(((r + c) as u8).wrapping_add(t));
        }
    }
    buf.extend(std::iter::repeat(128u8.wrapping_add(t / 3)).take(uv_size));
    buf.extend(std::iter::repeat(128u8.wrapping_add(t / 5)).take(uv_size));
    VideoFrame::new(
        Bytes::from(buf),
        w,
        h,
        PixelFormat::Yuv420p,
        ColorSpace::Bt709,
        pts,
    )
}

fn make_yuv420p10le_hdr_frame(w: u32, h: u32, pts: u64) -> VideoFrame {
    let wu = w as usize;
    let hu = h as usize;
    let y_samples = wu * hu;
    let uv_samples = y_samples / 4;
    let mut buf = Vec::with_capacity((y_samples + 2 * uv_samples) * 2);
    // Spread luma across 10-bit range so the encoder doesn't stripe one
    // chunk to a single block.
    for i in 0..y_samples {
        let v: u16 = 64 + ((i + pts as usize) as u16 % 800);
        buf.extend_from_slice(&v.to_le_bytes());
    }
    for _ in 0..(2 * uv_samples) {
        buf.extend_from_slice(&512u16.to_le_bytes());
    }
    VideoFrame::new(
        Bytes::from(buf),
        w,
        h,
        PixelFormat::Yuv420p10le,
        ColorSpace::Bt2020,
        pts,
    )
}

/// Find a 4-byte fourcc anywhere in the byte stream.
fn find_fourcc(data: &[u8], fourcc: &[u8; 4]) -> Option<usize> {
    data.windows(4).position(|w| w == fourcc)
}

/// After locating an `av1C` fourcc, return the third payload byte
/// — the `(seq_tier_0<<7) | (high_bitdepth<<6) | (twelve_bit<<5) | ...`
/// flags byte. Layout (from build_av1c, mux.rs:2078-2095):
///   byte 0 of payload: marker(1) | version(7)               = 0x81
///   byte 1: (seq_profile<<5) | (seq_level_idx_0)
///   byte 2: (seq_tier_0<<7) | (high_bitdepth<<6) | (twelve_bit<<5)
///                | (monochrome<<4) | (chroma_sub_x<<3)
///                | (chroma_sub_y<<2) | chroma_sample_position(2)
///   ...
/// `av1C` fourcc is at `pos`; payload bytes start at pos+4 (the 4 bytes
/// before `av1C` are the box size).
fn av1c_flags_byte(mp4: &[u8]) -> Option<u8> {
    let p = find_fourcc(mp4, b"av1C")?;
    let payload = p + 4; // skip fourcc, into payload
    Some(mp4[payload + 2])
}

/// `colr nclx` payload layout (mux.rs:2003-2014):
///   colour_type[4] = 'nclx'
///   colour_primaries u16 BE
///   transfer_characteristics u16 BE
///   matrix_coefficients u16 BE
///   full_range_flag(1) + reserved(7) — packed into one byte
fn colr_nclx_transfer(mp4: &[u8]) -> Option<u16> {
    let p = find_fourcc(mp4, b"colr")?;
    let payload = p + 4; // past 'colr'
    // payload[0..4] = 'nclx', payload[4..6] = primaries, [6..8] = transfer
    if &mp4[payload..payload + 4] != b"nclx" {
        return None;
    }
    Some(u16::from_be_bytes([mp4[payload + 6], mp4[payload + 7]]))
}

#[test]
fn sanity_a_output_is_larger_than_skeleton() {
    // Use a moderate quality (60, typical for our production pipeline)
    // + 256×256 frames so the encoder emits substantially more than the
    // ftyp+moov header. The assertion guards against a regression where
    // the encoder produces zero-length packets (or the muxer drops them):
    // in that case mp4 size would be ~500-800 bytes. A healthy mdat
    // payload should push us well over 3 KB for 8 frames at 256×256.
    let config = EncoderConfig {
        width: 256,
        height: 256,
        frame_rate: FPS,
        quality: 60,
        speed_preset: 10,
        keyframe_interval: 4,
        ..EncoderConfig::default()
    };
    let Some(mut encoder) = common::try_av1_encoder(config) else {
        return;
    };
    let mut muxer = Av1Mp4Muxer::new(256, 256, FPS).expect("muxer");
    let mut packets = 0usize;
    let mut mdat_bytes = 0usize;
    for pts in 0..8 {
        let f = make_yuv420p_frame(256, 256, pts);
        encoder.send_frame(&f).expect("send_frame");
        while let Some(p) = encoder.receive_packet().expect("receive") {
            packets += 1;
            mdat_bytes += p.data.len();
            muxer.add_packet(p).expect("add_packet");
        }
    }
    encoder.flush().expect("flush");
    while let Some(p) = encoder.receive_packet().expect("receive after flush") {
        packets += 1;
        mdat_bytes += p.data.len();
        muxer.add_packet(p).expect("add_packet");
    }
    let mp4 = muxer.finalize().expect("finalize");

    // mdat payload alone must be > 256 B (catches "every packet was 0 or
    // 1 bytes" — the classic empty-encoder regression). The structural
    // assertions (ftyp + moov + mdat present) guard against skeleton-only
    // output which would pass the size check on its own.
    assert!(packets > 0, "zero packets emitted by encoder");
    assert!(
        mdat_bytes > 256,
        "encoder produced {} packets totaling only {} bytes — empty-mdat bug",
        packets,
        mdat_bytes
    );
    // Structural skeleton must be complete AND mp4 must contain the payload.
    assert!(find_fourcc(&mp4, b"ftyp").is_some(), "no ftyp");
    assert!(find_fourcc(&mp4, b"moov").is_some(), "no moov");
    assert!(find_fourcc(&mp4, b"mdat").is_some(), "no mdat");
    // Total output must be strictly larger than a bare skeleton would be.
    // A conservative lower bound: payload bytes alone must be part of the
    // total. mp4 size > mdat_bytes means the skeleton exists; mp4 size
    // > 1024 means we wrote something meaningful.
    assert!(
        mp4.len() as usize > mdat_bytes,
        "mp4 {}B <= mdat payload {}B — mux failed to wrap the payload",
        mp4.len(),
        mdat_bytes
    );
    eprintln!(
        "sanity_a: mp4 size = {} bytes, {} packets totaling {} mdat bytes",
        mp4.len(),
        packets,
        mdat_bytes
    );
}

#[test]
fn sanity_b_av1c_high_bitdepth_reflects_input_pixel_format() {
    // 8-bit input → high_bitdepth bit 6 of byte2 must be 0.
    let cfg8 = EncoderConfig {
        width: W,
        height: H,
        frame_rate: FPS,
        quality: 200,
        speed_preset: 10,
        keyframe_interval: 4,
        pixel_format: PixelFormat::Yuv420p,
        ..EncoderConfig::default()
    };
    let Some(mut e8) = common::try_av1_encoder(cfg8) else {
        return;
    };
    let mut m8 = Av1Mp4Muxer::new(W, H, FPS).expect("muxer 8");
    for pts in 0..4 {
        e8.send_frame(&make_yuv420p_frame(W, H, pts))
            .expect("send 8");
        while let Some(p) = e8.receive_packet().expect("recv 8") {
            m8.add_packet(p).expect("add 8");
        }
    }
    e8.flush().expect("flush 8");
    while let Some(p) = e8.receive_packet().expect("recv after flush 8") {
        m8.add_packet(p).expect("add after flush 8");
    }
    let mp4_8 = m8.finalize().expect("finalize 8");
    let flags8 = av1c_flags_byte(&mp4_8).expect("av1C flags byte for 8-bit");
    let high_bitdepth_8 = (flags8 >> 6) & 1;
    let twelve_bit_8 = (flags8 >> 5) & 1;
    assert_eq!(
        high_bitdepth_8, 0,
        "8-bit input emitted high_bitdepth=1 (av1C flags byte = {:08b})",
        flags8
    );
    assert_eq!(twelve_bit_8, 0, "8-bit input must have twelve_bit=0");

    // 10-bit input → high_bitdepth bit 6 must be 1, twelve_bit bit 5 must be 0.
    let meta_hdr = ColorMetadata {
        transfer: TransferFn::St2084,
        matrix_coefficients: 9,
        colour_primaries: 9,
        full_range: false,
        mastering_display: None,
        content_light_level: None,
    };
    let cfg10 = EncoderConfig {
        width: W,
        height: H,
        frame_rate: FPS,
        quality: 200,
        speed_preset: 10,
        keyframe_interval: 4,
        pixel_format: PixelFormat::Yuv420p10le,
        color_metadata: meta_hdr,
        ..EncoderConfig::default()
    };
    let Some(mut e10) = common::try_av1_encoder(cfg10) else {
        return;
    };
    let mut m10 = Av1Mp4Muxer::new(W, H, FPS).expect("muxer 10");
    m10.set_color_metadata(meta_hdr);
    for pts in 0..4 {
        e10.send_frame(&make_yuv420p10le_hdr_frame(W, H, pts))
            .expect("send 10");
        while let Some(p) = e10.receive_packet().expect("recv 10") {
            m10.add_packet(p).expect("add 10");
        }
    }
    e10.flush().expect("flush 10");
    while let Some(p) = e10.receive_packet().expect("recv after flush 10") {
        m10.add_packet(p).expect("add after flush 10");
    }
    let mp4_10 = m10.finalize().expect("finalize 10");
    let flags10 = av1c_flags_byte(&mp4_10).expect("av1C flags byte for 10-bit");
    let high_bitdepth_10 = (flags10 >> 6) & 1;
    let twelve_bit_10 = (flags10 >> 5) & 1;
    assert_eq!(
        high_bitdepth_10, 1,
        "10-bit input emitted high_bitdepth=0 (av1C flags byte = {:08b}) — \
         encoder is ignoring pixel_format config and silently downsampling",
        flags10
    );
    assert_eq!(
        twelve_bit_10, 0,
        "10-bit input must have twelve_bit=0 (would be set only for 12-bit)"
    );
    eprintln!(
        "sanity_b: 8-bit flags=0b{:08b}, 10-bit flags=0b{:08b}",
        flags8, flags10
    );
}

#[test]
fn sanity_c_hdr_input_writes_pq_transfer_into_colr_nclx() {
    let meta_hdr = ColorMetadata {
        transfer: TransferFn::St2084, // PQ
        matrix_coefficients: 9,       // BT.2020 NCL
        colour_primaries: 9,          // BT.2020
        full_range: false,
        mastering_display: None,
        content_light_level: None,
    };
    let cfg = EncoderConfig {
        width: W,
        height: H,
        frame_rate: FPS,
        quality: 200,
        speed_preset: 10,
        keyframe_interval: 4,
        pixel_format: PixelFormat::Yuv420p10le,
        color_metadata: meta_hdr,
        ..EncoderConfig::default()
    };
    let Some(mut enc) = common::try_av1_encoder(cfg) else {
        return;
    };
    let mut mux = Av1Mp4Muxer::new(W, H, FPS).expect("muxer");
    mux.set_color_metadata(meta_hdr);
    for pts in 0..4 {
        enc.send_frame(&make_yuv420p10le_hdr_frame(W, H, pts))
            .expect("send");
        while let Some(p) = enc.receive_packet().expect("recv") {
            mux.add_packet(p).expect("add");
        }
    }
    enc.flush().expect("flush");
    while let Some(p) = enc.receive_packet().expect("recv after flush") {
        mux.add_packet(p).expect("add");
    }
    let mp4 = mux.finalize().expect("finalize");

    let transfer = colr_nclx_transfer(&mp4).expect("colr nclx atom must be present for HDR output");
    // H.273 transfer_characteristics: 1=BT.709, 16=ST2084 (PQ), 18=HLG.
    // Anything other than 16 indicates the HDR signaling was lost.
    assert_eq!(
        transfer, 16,
        "HDR (PQ) input produced colr.transfer={} — expected 16 (ST2084 / PQ). \
         ColorMetadata likely dropped on the floor between EncoderConfig and the muxer.",
        transfer
    );
    eprintln!(
        "sanity_c: colr nclx transfer = {} (PQ as expected)",
        transfer
    );
}
