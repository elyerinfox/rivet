//! Video filters — per-frame geometric/color transforms applied to decoded
//! frames **before** per-rung scaling and encoding.
//!
//! A filter chain is a small ordered list of [`VideoFilter`]s, parsed from an
//! ffmpeg-`-vf`-style string (`parse_chain`): comma-separated filters, each
//! `name` or `name=a:b:…`. They operate on planar 4:2:0 frames — `Yuv420p`
//! (8-bit) and `Yuv420p10le` (10-bit) — which is what the pipeline normalizes to
//! before this stage runs.
//!
//! ```text
//!   crop=1280:720          crop a centered 1280×720 region
//!   crop=1280:720:0:0      crop 1280×720 at (0,0)
//!   pad=1920:1080          letterbox/pillarbox into 1920×1080 (centered, black)
//!   hflip / vflip          mirror horizontally / vertically
//!   rotate=90|180|270      rotate clockwise (90/270 swap width↔height)
//!   transpose              alias for rotate=90
//!   grayscale (or gray)    drop chroma (neutral)
//! ```
//!
//! Geometric ops are pure sample rearrangement, so they run on the raw bytes
//! with a 1- or 2-byte sample stride and work for both bit depths. 4:2:0 needs
//! even dimensions, so crop/pad sizes + offsets are rounded to even.

use anyhow::{Result, bail};
use bytes::{Bytes, BytesMut};

use crate::frame::{PixelFormat, VideoFrame};

/// One video-filter step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoFilter {
    /// Crop a `w×h` region at `(x, y)`. Even-aligned for 4:2:0.
    Crop { w: u32, h: u32, x: u32, y: u32 },
    /// Centre-crop a `w×h` region (offset derived to centre it).
    CropCenter { w: u32, h: u32 },
    /// Place the frame into a `w×h` canvas at `(x, y)`, filling the rest with
    /// neutral black. Used for letterbox / pillarbox.
    Pad { w: u32, h: u32, x: u32, y: u32 },
    /// Centre the frame in a `w×h` canvas (centred offset).
    PadCenter { w: u32, h: u32 },
    /// Mirror horizontally (left↔right).
    HFlip,
    /// Mirror vertically (top↔bottom).
    VFlip,
    /// Rotate 90° clockwise (swaps width↔height).
    Rotate90,
    /// Rotate 180°.
    Rotate180,
    /// Rotate 270° clockwise / 90° counter-clockwise (swaps width↔height).
    Rotate270,
    /// Drop chroma — set U/V to neutral so the image is grayscale.
    Grayscale,
}

/// Parse an ffmpeg-`-vf`-style chain, e.g. `"crop=1280:720,hflip"`.
pub fn parse_chain(s: &str) -> Result<Vec<VideoFilter>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        out.push(parse_one(part)?);
    }
    if out.is_empty() {
        bail!("empty filter chain");
    }
    Ok(out)
}

fn parse_one(spec: &str) -> Result<VideoFilter> {
    let (name, args) = match spec.split_once('=') {
        Some((n, a)) => (n.trim(), a.trim()),
        None => (spec.trim(), ""),
    };
    let nums = |a: &str| -> Result<Vec<u32>> {
        a.split(':')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<u32>().map_err(|_| anyhow::anyhow!("bad number '{s}' in '{spec}'")))
            .collect()
    };
    let f = match name {
        "crop" => match nums(args)?.as_slice() {
            [w, h] => VideoFilter::CropCenter { w: *w, h: *h },
            [w, h, x, y] => VideoFilter::Crop { w: *w, h: *h, x: *x, y: *y },
            _ => bail!("crop wants W:H or W:H:X:Y, got '{args}'"),
        },
        "pad" => match nums(args)?.as_slice() {
            [w, h] => VideoFilter::PadCenter { w: *w, h: *h },
            [w, h, x, y] => VideoFilter::Pad { w: *w, h: *h, x: *x, y: *y },
            _ => bail!("pad wants W:H or W:H:X:Y, got '{args}'"),
        },
        "hflip" => VideoFilter::HFlip,
        "vflip" => VideoFilter::VFlip,
        "rotate" | "transpose" => {
            let deg = if name == "transpose" {
                90
            } else {
                *nums(args)?.first().unwrap_or(&90)
            };
            match deg {
                90 => VideoFilter::Rotate90,
                180 => VideoFilter::Rotate180,
                270 => VideoFilter::Rotate270,
                o => bail!("rotate wants 90|180|270, got {o}"),
            }
        }
        "grayscale" | "gray" => VideoFilter::Grayscale,
        o => bail!("unknown filter '{o}'"),
    };
    Ok(f)
}

/// Apply a whole chain to a frame, in order.
pub fn apply_chain(frame: VideoFrame, chain: &[VideoFilter]) -> Result<VideoFrame> {
    let mut f = frame;
    for filter in chain {
        f = apply(&f, filter)?;
    }
    Ok(f)
}

/// Bytes-per-sample for the supported 4:2:0 formats.
fn bps(format: PixelFormat) -> Result<usize> {
    match format {
        PixelFormat::Yuv420p => Ok(1),
        PixelFormat::Yuv420p10le => Ok(2),
        other => bail!("video filters need Yuv420p / Yuv420p10le, got {other:?}"),
    }
}

/// Split a frame into its (Y, U, V) plane byte slices for a `w×h` 4:2:0 frame.
fn planes(frame: &VideoFrame, bps: usize) -> Result<(&[u8], &[u8], &[u8])> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let y_len = w * h * bps;
    let c_len = (w / 2) * (h / 2) * bps;
    if frame.data.len() < y_len + 2 * c_len {
        bail!(
            "frame data too small: {} < {} for {}x{}",
            frame.data.len(),
            y_len + 2 * c_len,
            w,
            h
        );
    }
    let (y, rest) = frame.data.split_at(y_len);
    let (u, v) = rest.split_at(c_len);
    Ok((y, &u[..c_len], &v[..c_len]))
}

/// Reassemble a frame from new Y/U/V planes + new dims.
fn assemble(src: &VideoFrame, w: u32, h: u32, y: Vec<u8>, u: Vec<u8>, v: Vec<u8>) -> VideoFrame {
    let mut data = BytesMut::with_capacity(y.len() + u.len() + v.len());
    data.extend_from_slice(&y);
    data.extend_from_slice(&u);
    data.extend_from_slice(&v);
    VideoFrame::new(
        data.freeze(),
        w,
        h,
        src.format,
        src.color_space,
        src.pts,
    )
}

/// Apply one filter.
pub fn apply(frame: &VideoFrame, filter: &VideoFilter) -> Result<VideoFrame> {
    let bps = bps(frame.format)?;
    let w = frame.width as usize;
    let h = frame.height as usize;
    let (y, u, v) = planes(frame, bps)?;

    match *filter {
        VideoFilter::CropCenter { w: cw, h: ch } => {
            let cw = even(cw.min(frame.width));
            let ch = even(ch.min(frame.height));
            let x = even((frame.width - cw) / 2);
            let yy = even((frame.height - ch) / 2);
            crop(frame, x, yy, cw, ch)
        }
        VideoFilter::Crop { w: cw, h: ch, x, y: yy } => crop(frame, x, yy, cw, ch),
        VideoFilter::PadCenter { w: pw, h: ph } => {
            let pw = even(pw.max(frame.width));
            let ph = even(ph.max(frame.height));
            let x = even((pw - frame.width) / 2);
            let yy = even((ph - frame.height) / 2);
            pad(frame, pw, ph, x, yy)
        }
        VideoFilter::Pad { w: pw, h: ph, x, y: yy } => pad(frame, pw, ph, x, yy),
        VideoFilter::HFlip => Ok(assemble(
            frame,
            frame.width,
            frame.height,
            hflip(y, w, h, bps),
            hflip(u, w / 2, h / 2, bps),
            hflip(v, w / 2, h / 2, bps),
        )),
        VideoFilter::VFlip => Ok(assemble(
            frame,
            frame.width,
            frame.height,
            vflip(y, w, h, bps),
            vflip(u, w / 2, h / 2, bps),
            vflip(v, w / 2, h / 2, bps),
        )),
        VideoFilter::Rotate180 => Ok(assemble(
            frame,
            frame.width,
            frame.height,
            vflip(&hflip(y, w, h, bps), w, h, bps),
            vflip(&hflip(u, w / 2, h / 2, bps), w / 2, h / 2, bps),
            vflip(&hflip(v, w / 2, h / 2, bps), w / 2, h / 2, bps),
        )),
        VideoFilter::Rotate90 => Ok(assemble(
            frame,
            frame.height,
            frame.width,
            rot90(y, w, h, bps),
            rot90(u, w / 2, h / 2, bps),
            rot90(v, w / 2, h / 2, bps),
        )),
        VideoFilter::Rotate270 => Ok(assemble(
            frame,
            frame.height,
            frame.width,
            rot270(y, w, h, bps),
            rot270(u, w / 2, h / 2, bps),
            rot270(v, w / 2, h / 2, bps),
        )),
        VideoFilter::Grayscale => {
            let neutral = neutral_chroma(frame.format);
            let mut uu = u.to_vec();
            let mut vv = v.to_vec();
            fill(&mut uu, &neutral);
            fill(&mut vv, &neutral);
            Ok(assemble(frame, frame.width, frame.height, y.to_vec(), uu, vv))
        }
    }
}

fn even(n: u32) -> u32 {
    n & !1
}

fn crop(frame: &VideoFrame, x: u32, y: u32, w: u32, h: u32) -> Result<VideoFrame> {
    let (x, y, w, h) = (even(x), even(y), even(w), even(h));
    if w == 0 || h == 0 || x + w > frame.width || y + h > frame.height {
        bail!(
            "crop {w}x{h}+{x}+{y} out of bounds for {}x{}",
            frame.width,
            frame.height
        );
    }
    let bps = bps(frame.format)?;
    let (yp, up, vp) = planes(frame, bps)?;
    let fw = frame.width as usize;
    let y_new = crop_plane(yp, fw, x as usize, y as usize, w as usize, h as usize, bps);
    let u_new = crop_plane(
        up,
        fw / 2,
        (x / 2) as usize,
        (y / 2) as usize,
        (w / 2) as usize,
        (h / 2) as usize,
        bps,
    );
    let v_new = crop_plane(
        vp,
        fw / 2,
        (x / 2) as usize,
        (y / 2) as usize,
        (w / 2) as usize,
        (h / 2) as usize,
        bps,
    );
    Ok(assemble(frame, w, h, y_new, u_new, v_new))
}

fn pad(frame: &VideoFrame, pw: u32, ph: u32, x: u32, y: u32) -> Result<VideoFrame> {
    let (pw, ph, x, y) = (even(pw), even(ph), even(x), even(y));
    if x + frame.width > pw || y + frame.height > ph {
        bail!(
            "pad {pw}x{ph} with frame {}x{} at +{x}+{y} overflows",
            frame.width,
            frame.height
        );
    }
    let bps = bps(frame.format)?;
    let (yp, up, vp) = planes(frame, bps)?;
    let (luma_fill, chroma_fill) = black_fill(frame.format);
    let fw = frame.width as usize;
    let fh = frame.height as usize;
    let y_new = pad_plane(yp, fw, fh, pw as usize, ph as usize, x as usize, y as usize, bps, &luma_fill);
    let u_new = pad_plane(
        up,
        fw / 2,
        fh / 2,
        (pw / 2) as usize,
        (ph / 2) as usize,
        (x / 2) as usize,
        (y / 2) as usize,
        bps,
        &chroma_fill,
    );
    let v_new = pad_plane(
        vp,
        fw / 2,
        fh / 2,
        (pw / 2) as usize,
        (ph / 2) as usize,
        (x / 2) as usize,
        (y / 2) as usize,
        bps,
        &chroma_fill,
    );
    Ok(assemble(frame, pw, ph, y_new, u_new, v_new))
}

// ── plane primitives (sample = `bps` bytes; pure rearrangement) ──

fn crop_plane(src: &[u8], pw: usize, x: usize, y: usize, cw: usize, ch: usize, bps: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(cw * ch * bps);
    for row in 0..ch {
        let start = ((y + row) * pw + x) * bps;
        out.extend_from_slice(&src[start..start + cw * bps]);
    }
    out
}

fn pad_plane(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    ox: usize,
    oy: usize,
    bps: usize,
    fill_sample: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(dw * dh * bps);
    for _ in 0..dw * dh {
        out.extend_from_slice(fill_sample);
    }
    for row in 0..sh {
        let s = row * sw * bps;
        let d = ((oy + row) * dw + ox) * bps;
        out[d..d + sw * bps].copy_from_slice(&src[s..s + sw * bps]);
    }
    out
}

fn hflip(src: &[u8], w: usize, h: usize, bps: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * bps];
    for row in 0..h {
        let base = row * w * bps;
        for col in 0..w {
            let s = base + col * bps;
            let d = base + (w - 1 - col) * bps;
            out[d..d + bps].copy_from_slice(&src[s..s + bps]);
        }
    }
    out
}

fn vflip(src: &[u8], w: usize, h: usize, bps: usize) -> Vec<u8> {
    let rb = w * bps;
    let mut out = vec![0u8; w * h * bps];
    for row in 0..h {
        let s = row * rb;
        let d = (h - 1 - row) * rb;
        out[d..d + rb].copy_from_slice(&src[s..s + rb]);
    }
    out
}

/// Rotate 90° clockwise: src `w×h` → dst `h×w`. dst(r,c) = src(h-1-c, r).
fn rot90(src: &[u8], w: usize, h: usize, bps: usize) -> Vec<u8> {
    let (dw, dh) = (h, w);
    let mut out = vec![0u8; dw * dh * bps];
    for r in 0..dh {
        for c in 0..dw {
            let sr = h - 1 - c;
            let sc = r;
            let s = (sr * w + sc) * bps;
            let d = (r * dw + c) * bps;
            out[d..d + bps].copy_from_slice(&src[s..s + bps]);
        }
    }
    out
}

/// Rotate 270° clockwise: src `w×h` → dst `h×w`. dst(r,c) = src(c, w-1-r).
fn rot270(src: &[u8], w: usize, h: usize, bps: usize) -> Vec<u8> {
    let (dw, dh) = (h, w);
    let mut out = vec![0u8; dw * dh * bps];
    for r in 0..dh {
        for c in 0..dw {
            let sr = c;
            let sc = w - 1 - r;
            let s = (sr * w + sc) * bps;
            let d = (r * dw + c) * bps;
            out[d..d + bps].copy_from_slice(&src[s..s + bps]);
        }
    }
    out
}

fn fill(buf: &mut [u8], sample: &[u8]) {
    for chunk in buf.chunks_exact_mut(sample.len()) {
        chunk.copy_from_slice(sample);
    }
}

/// Neutral chroma sample bytes (mid-range): 128 for 8-bit, 512 for 10-bit LE.
fn neutral_chroma(format: PixelFormat) -> Vec<u8> {
    match format {
        PixelFormat::Yuv420p => vec![128],
        _ => (512u16).to_le_bytes().to_vec(), // 10-bit mid
    }
}

/// Limited-range black: luma 16, chroma 128 (8-bit); luma 64, chroma 512 (10-bit).
fn black_fill(format: PixelFormat) -> (Vec<u8>, Vec<u8>) {
    match format {
        PixelFormat::Yuv420p => (vec![16], vec![128]),
        _ => ((64u16).to_le_bytes().to_vec(), (512u16).to_le_bytes().to_vec()),
    }
}

/// Convenience: keep `Bytes` import used regardless of cfg.
#[allow(dead_code)]
fn _bytes_marker(_b: Bytes) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::ColorSpace;

    // Build a tiny Yuv420p frame with a per-pixel luma gradient and constant chroma.
    fn frame(w: u32, h: u32) -> VideoFrame {
        let (wu, hu) = (w as usize, h as usize);
        let mut data = Vec::new();
        for r in 0..hu {
            for c in 0..wu {
                data.push((r * wu + c) as u8); // unique-ish luma
            }
        }
        data.extend(std::iter::repeat(100).take((wu / 2) * (hu / 2))); // U
        data.extend(std::iter::repeat(200).take((wu / 2) * (hu / 2))); // V
        VideoFrame::new(Bytes::from(data), w, h, PixelFormat::Yuv420p, ColorSpace::Bt709, 0)
    }

    fn luma(f: &VideoFrame) -> &[u8] {
        &f.data[..(f.width * f.height) as usize]
    }

    #[test]
    fn parse_chain_basic() {
        let c = parse_chain("crop=1280:720,hflip,pad=1920:1080").unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], VideoFilter::CropCenter { w: 1280, h: 720 });
        assert_eq!(c[1], VideoFilter::HFlip);
        assert_eq!(c[2], VideoFilter::PadCenter { w: 1920, h: 1080 });
        assert_eq!(parse_chain("rotate=270").unwrap()[0], VideoFilter::Rotate270);
        assert_eq!(parse_chain("transpose").unwrap()[0], VideoFilter::Rotate90);
        assert_eq!(parse_chain("gray").unwrap()[0], VideoFilter::Grayscale);
        assert!(parse_chain("bogus").is_err());
        assert!(parse_chain("rotate=45").is_err());
        assert!(parse_chain("crop=10").is_err());
    }

    #[test]
    fn hflip_reverses_rows() {
        let f = frame(4, 2);
        let out = apply(&f, &VideoFilter::HFlip).unwrap();
        // row 0 was [0,1,2,3] -> [3,2,1,0]
        assert_eq!(&luma(&out)[..4], &[3, 2, 1, 0]);
        assert_eq!((out.width, out.height), (4, 2));
    }

    #[test]
    fn vflip_reverses_rows_order() {
        let f = frame(2, 2);
        // luma: row0 [0,1], row1 [2,3]
        let out = apply(&f, &VideoFilter::VFlip).unwrap();
        assert_eq!(luma(&out), &[2, 3, 0, 1]);
    }

    #[test]
    fn rotate_dims_and_roundtrip() {
        let f = frame(4, 2);
        let r90 = apply(&f, &VideoFilter::Rotate90).unwrap();
        assert_eq!((r90.width, r90.height), (2, 4)); // swapped
        // rotate 90 then 270 returns to original luma
        let back = apply(&r90, &VideoFilter::Rotate270).unwrap();
        assert_eq!((back.width, back.height), (4, 2));
        assert_eq!(luma(&back), luma(&f));
        // 180 == hflip+vflip
        let r180 = apply(&f, &VideoFilter::Rotate180).unwrap();
        assert_eq!((r180.width, r180.height), (4, 2));
    }

    #[test]
    fn crop_center_extracts_region() {
        let f = frame(8, 8);
        let out = apply(&f, &VideoFilter::CropCenter { w: 4, h: 4 }).unwrap();
        assert_eq!((out.width, out.height), (4, 4));
        // centre 4x4 starts at (2,2): first luma sample = src(2,2) = 2*8+2 = 18
        assert_eq!(luma(&out)[0], 18);
        // data length consistent: 4*4 + 2*2*2 = 24
        assert_eq!(out.data.len(), 4 * 4 + 2 * (2 * 2));
    }

    #[test]
    fn pad_centers_and_fills_black() {
        let f = frame(2, 2);
        let out = apply(&f, &VideoFilter::PadCenter { w: 6, h: 6 }).unwrap();
        assert_eq!((out.width, out.height), (6, 6));
        // top-left corner is black fill (luma 16)
        assert_eq!(luma(&out)[0], 16);
        // the 2x2 sits centred at (2,2): luma(2,2) == src(0,0) == 0
        assert_eq!(luma(&out)[2 * 6 + 2], 0);
    }

    #[test]
    fn grayscale_neutralizes_chroma() {
        let f = frame(4, 4);
        let out = apply(&f, &VideoFilter::Grayscale).unwrap();
        let cstart = (4 * 4) as usize;
        assert!(out.data[cstart..].iter().all(|&b| b == 128));
        // luma untouched
        assert_eq!(luma(&out), luma(&f));
    }

    #[test]
    fn ten_bit_hflip_works() {
        // 2x2 10-bit frame: luma samples 0,1,2,3 as u16 LE
        let mut data: Vec<u8> = Vec::new();
        for s in [0u16, 1, 2, 3] {
            data.extend_from_slice(&s.to_le_bytes());
        }
        data.extend_from_slice(&(512u16).to_le_bytes()); // U (1 sample)
        data.extend_from_slice(&(512u16).to_le_bytes()); // V
        let f = VideoFrame::new(Bytes::from(data), 2, 2, PixelFormat::Yuv420p10le, ColorSpace::Bt709, 0);
        let out = apply(&f, &VideoFilter::HFlip).unwrap();
        // row0 [0,1] -> [1,0]
        assert_eq!(&out.data[0..2], &1u16.to_le_bytes());
        assert_eq!(&out.data[2..4], &0u16.to_le_bytes());
    }

    #[test]
    fn apply_chain_runs_in_order() {
        let f = frame(8, 8);
        let out = apply_chain(f, &[
            VideoFilter::CropCenter { w: 4, h: 4 },
            VideoFilter::Grayscale,
        ])
        .unwrap();
        assert_eq!((out.width, out.height), (4, 4));
        let cstart = (4 * 4) as usize;
        assert!(out.data[cstart..].iter().all(|&b| b == 128));
    }
}
