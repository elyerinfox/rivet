//! Video filters — per-frame geometric/color transforms applied to decoded
//! frames **before** per-rung scaling and encoding.
//!
//! The canonical representation is a list of [`VideoFilter`] **values**
//! (interpreted by [`apply`]). They have two interchangeable serializations:
//!
//! - **Structured** (objects) — with the `serde` feature, each filter
//!   (de)serializes to/from a tagged object, so a YAML/JSON DSL can write a
//!   chain as a list of objects:
//!   ```yaml
//!   filters:
//!     - crop: { w: 1280, h: 720 }   # centred (x/y optional)
//!     - hflip
//!     - rotate: 90
//!     - pad: { w: 1920, h: 1080 }
//!   ```
//! - **Textual** (ffmpeg-`-vf`-style) — [`parse_chain`] (text → values) and
//!   [`Display`]/[`chain_to_string`] (values → text):
//!   `crop=1280:720,hflip,rotate=90,pad=1920:1080`.
//!
//! [`FilterSpec`] (serde) accepts **either** form in one field, so a DSL can use
//! objects or a string interchangeably. The two round-trip:
//! `parse_chain(&chain_to_string(c)) == c`.
//!
//! Geometric ops are pure sample rearrangement, so they run on the raw bytes
//! with a 1- or 2-byte sample stride and work for both `Yuv420p` (8-bit) and
//! `Yuv420p10le` (10-bit). 4:2:0 needs even dimensions, so crop/pad sizes +
//! offsets round to even.

use std::fmt;

use anyhow::{Result, bail};
use bytes::BytesMut;

use crate::frame::{PixelFormat, VideoFrame};

/// One video-filter step. The canonical, code-interpreted representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum VideoFilter {
    /// Crop a `w×h` region. Centred when `x`/`y` are omitted, else at `(x, y)`.
    Crop {
        w: u32,
        h: u32,
        #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
        x: Option<u32>,
        #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
        y: Option<u32>,
    },
    /// Pad into a `w×h` canvas (neutral black). Centred when `x`/`y` are omitted.
    Pad {
        w: u32,
        h: u32,
        #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
        x: Option<u32>,
        #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "Option::is_none"))]
        y: Option<u32>,
    },
    /// Mirror horizontally (left↔right).
    #[cfg_attr(feature = "serde", serde(rename = "hflip"))]
    HFlip,
    /// Mirror vertically (top↔bottom).
    #[cfg_attr(feature = "serde", serde(rename = "vflip"))]
    VFlip,
    /// Rotate clockwise by 90, 180, or 270 degrees (90/270 swap width↔height).
    Rotate(u32),
    /// Drop chroma — set U/V to neutral so the image is grayscale.
    Grayscale,
}

impl fmt::Display for VideoFilter {
    /// The textual (ffmpeg-`-vf`) token for this filter.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoFilter::Crop { w, h, x: Some(x), y: Some(y) } => write!(f, "crop={w}:{h}:{x}:{y}"),
            VideoFilter::Crop { w, h, .. } => write!(f, "crop={w}:{h}"),
            VideoFilter::Pad { w, h, x: Some(x), y: Some(y) } => write!(f, "pad={w}:{h}:{x}:{y}"),
            VideoFilter::Pad { w, h, .. } => write!(f, "pad={w}:{h}"),
            VideoFilter::HFlip => write!(f, "hflip"),
            VideoFilter::VFlip => write!(f, "vflip"),
            VideoFilter::Rotate(d) => write!(f, "rotate={d}"),
            VideoFilter::Grayscale => write!(f, "grayscale"),
        }
    }
}

/// A whole chain as a comma-separated textual string (the inverse of
/// [`parse_chain`]).
pub fn chain_to_string(chain: &[VideoFilter]) -> String {
    chain.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",")
}

/// A filter chain in either form, for a DSL field that should accept both a
/// structured list or a string. Resolve with [`FilterSpec::resolve`].
#[cfg(feature = "serde")]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[serde(untagged)]
pub enum FilterSpec {
    /// An ffmpeg-`-vf`-style chain string, e.g. `"crop=1280:720,hflip"`.
    Chain(String),
    /// A structured list of filters.
    List(Vec<VideoFilter>),
}

#[cfg(feature = "serde")]
impl FilterSpec {
    /// Resolve to the concrete, **validated** filter list. The string form is
    /// validated by [`parse_chain`]; the structured form is validated by
    /// round-tripping through its textual rendering, so e.g. `rotate: 45` is
    /// rejected at config time rather than at apply time.
    pub fn resolve(&self) -> Result<Vec<VideoFilter>> {
        match self {
            FilterSpec::Chain(s) => parse_chain(s),
            FilterSpec::List(v) => parse_chain(&chain_to_string(v)),
        }
    }

    /// Collapse to the chain-string form (for string-only surfaces).
    pub fn to_chain(&self) -> String {
        match self {
            FilterSpec::Chain(s) => s.clone(),
            FilterSpec::List(v) => chain_to_string(v),
        }
    }
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
            [w, h] => VideoFilter::Crop { w: *w, h: *h, x: None, y: None },
            [w, h, x, y] => VideoFilter::Crop { w: *w, h: *h, x: Some(*x), y: Some(*y) },
            _ => bail!("crop wants W:H or W:H:X:Y, got '{args}'"),
        },
        "pad" => match nums(args)?.as_slice() {
            [w, h] => VideoFilter::Pad { w: *w, h: *h, x: None, y: None },
            [w, h, x, y] => VideoFilter::Pad { w: *w, h: *h, x: Some(*x), y: Some(*y) },
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
            if !matches!(deg, 90 | 180 | 270) {
                bail!("rotate wants 90|180|270, got {deg}");
            }
            VideoFilter::Rotate(deg)
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
    VideoFrame::new(data.freeze(), w, h, src.format, src.color_space, src.pts)
}

/// Apply one filter.
pub fn apply(frame: &VideoFrame, filter: &VideoFilter) -> Result<VideoFrame> {
    let bps = bps(frame.format)?;
    let w = frame.width as usize;
    let h = frame.height as usize;
    let (y, u, v) = planes(frame, bps)?;

    match *filter {
        VideoFilter::Crop { w: cw, h: ch, x, y: cy } => match (x, cy) {
            (Some(x), Some(cy)) => crop(frame, x, cy, cw, ch),
            _ => {
                let cw = even(cw.min(frame.width));
                let ch = even(ch.min(frame.height));
                let cx = even(frame.width.saturating_sub(cw) / 2);
                let cyc = even(frame.height.saturating_sub(ch) / 2);
                crop(frame, cx, cyc, cw, ch)
            }
        },
        VideoFilter::Pad { w: pw, h: ph, x, y: py } => {
            let pw = even(pw.max(frame.width));
            let ph = even(ph.max(frame.height));
            let px = x.map(even).unwrap_or_else(|| even(pw.saturating_sub(frame.width) / 2));
            let pyc = py.map(even).unwrap_or_else(|| even(ph.saturating_sub(frame.height) / 2));
            pad(frame, pw, ph, px, pyc)
        }
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
        VideoFilter::Rotate(180) => Ok(assemble(
            frame,
            frame.width,
            frame.height,
            vflip(&hflip(y, w, h, bps), w, h, bps),
            vflip(&hflip(u, w / 2, h / 2, bps), w / 2, h / 2, bps),
            vflip(&hflip(v, w / 2, h / 2, bps), w / 2, h / 2, bps),
        )),
        VideoFilter::Rotate(90) => Ok(assemble(
            frame,
            frame.height,
            frame.width,
            rot90(y, w, h, bps),
            rot90(u, w / 2, h / 2, bps),
            rot90(v, w / 2, h / 2, bps),
        )),
        VideoFilter::Rotate(270) => Ok(assemble(
            frame,
            frame.height,
            frame.width,
            rot270(y, w, h, bps),
            rot270(u, w / 2, h / 2, bps),
            rot270(v, w / 2, h / 2, bps),
        )),
        VideoFilter::Rotate(d) => bail!("rotate must be 90|180|270, got {d}"),
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
        bail!("crop {w}x{h}+{x}+{y} out of bounds for {}x{}", frame.width, frame.height);
    }
    let bps = bps(frame.format)?;
    let (yp, up, vp) = planes(frame, bps)?;
    let fw = frame.width as usize;
    let y_new = crop_plane(yp, fw, x as usize, y as usize, w as usize, h as usize, bps);
    let u_new = crop_plane(up, fw / 2, (x / 2) as usize, (y / 2) as usize, (w / 2) as usize, (h / 2) as usize, bps);
    let v_new = crop_plane(vp, fw / 2, (x / 2) as usize, (y / 2) as usize, (w / 2) as usize, (h / 2) as usize, bps);
    Ok(assemble(frame, w, h, y_new, u_new, v_new))
}

fn pad(frame: &VideoFrame, pw: u32, ph: u32, x: u32, y: u32) -> Result<VideoFrame> {
    let (pw, ph, x, y) = (even(pw), even(ph), even(x), even(y));
    if x + frame.width > pw || y + frame.height > ph {
        bail!("pad {pw}x{ph} with frame {}x{} at +{x}+{y} overflows", frame.width, frame.height);
    }
    let bps = bps(frame.format)?;
    let (yp, up, vp) = planes(frame, bps)?;
    let (luma_fill, chroma_fill) = black_fill(frame.format);
    let fw = frame.width as usize;
    let fh = frame.height as usize;
    let y_new = pad_plane(yp, fw, fh, pw as usize, ph as usize, x as usize, y as usize, bps, &luma_fill);
    let u_new = pad_plane(up, fw / 2, fh / 2, (pw / 2) as usize, (ph / 2) as usize, (x / 2) as usize, (y / 2) as usize, bps, &chroma_fill);
    let v_new = pad_plane(vp, fw / 2, fh / 2, (pw / 2) as usize, (ph / 2) as usize, (x / 2) as usize, (y / 2) as usize, bps, &chroma_fill);
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

fn pad_plane(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize, ox: usize, oy: usize, bps: usize, fill_sample: &[u8]) -> Vec<u8> {
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
            let s = ((h - 1 - c) * w + r) * bps;
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
            let s = (c * w + (w - 1 - r)) * bps;
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
        _ => (512u16).to_le_bytes().to_vec(),
    }
}

/// Limited-range black: luma 16, chroma 128 (8-bit); luma 64, chroma 512 (10-bit).
fn black_fill(format: PixelFormat) -> (Vec<u8>, Vec<u8>) {
    match format {
        PixelFormat::Yuv420p => (vec![16], vec![128]),
        _ => ((64u16).to_le_bytes().to_vec(), (512u16).to_le_bytes().to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::ColorSpace;
    use bytes::Bytes;

    fn frame(w: u32, h: u32) -> VideoFrame {
        let (wu, hu) = (w as usize, h as usize);
        let mut data = Vec::new();
        for r in 0..hu {
            for c in 0..wu {
                data.push((r * wu + c) as u8);
            }
        }
        data.extend(std::iter::repeat(100).take((wu / 2) * (hu / 2)));
        data.extend(std::iter::repeat(200).take((wu / 2) * (hu / 2)));
        VideoFrame::new(Bytes::from(data), w, h, PixelFormat::Yuv420p, ColorSpace::Bt709, 0)
    }
    fn luma(f: &VideoFrame) -> &[u8] {
        &f.data[..(f.width * f.height) as usize]
    }

    #[test]
    fn parse_and_display_round_trip() {
        let c = parse_chain("crop=1280:720,hflip,pad=1920:1080,rotate=90,grayscale").unwrap();
        assert_eq!(c[0], VideoFilter::Crop { w: 1280, h: 720, x: None, y: None });
        assert_eq!(c[1], VideoFilter::HFlip);
        assert_eq!(c[3], VideoFilter::Rotate(90));
        // Display ∘ parse is identity (canonical form).
        assert_eq!(chain_to_string(&c), "crop=1280:720,hflip,pad=1920:1080,rotate=90,grayscale");
        // explicit crop offset
        assert_eq!(
            parse_chain("crop=10:20:1:2").unwrap()[0],
            VideoFilter::Crop { w: 10, h: 20, x: Some(1), y: Some(2) }
        );
        assert_eq!(parse_chain("transpose").unwrap()[0], VideoFilter::Rotate(90));
        assert!(parse_chain("rotate=45").is_err());
        assert!(parse_chain("bogus").is_err());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn structured_json_round_trips() {
        // Structured object form deserializes to the same values as the string.
        let json = r#"[{"crop":{"w":1280,"h":720}},"hflip",{"rotate":90}]"#;
        let from_list: FilterSpec = serde_json::from_str(json).unwrap();
        let from_str: FilterSpec = serde_json::from_str(r#""crop=1280:720,hflip,rotate=90""#).unwrap();
        let expect = vec![
            VideoFilter::Crop { w: 1280, h: 720, x: None, y: None },
            VideoFilter::HFlip,
            VideoFilter::Rotate(90),
        ];
        assert_eq!(from_list.resolve().unwrap(), expect);
        assert_eq!(from_str.resolve().unwrap(), expect);
        // serialize the structs back out and parse the string again → identity
        assert_eq!(parse_chain(&chain_to_string(&expect)).unwrap(), expect);
    }

    #[test]
    fn hflip_reverses_rows() {
        let out = apply(&frame(4, 2), &VideoFilter::HFlip).unwrap();
        assert_eq!(&luma(&out)[..4], &[3, 2, 1, 0]);
    }

    #[test]
    fn rotate_dims_and_roundtrip() {
        let f = frame(4, 2);
        let r90 = apply(&f, &VideoFilter::Rotate(90)).unwrap();
        assert_eq!((r90.width, r90.height), (2, 4));
        let back = apply(&r90, &VideoFilter::Rotate(270)).unwrap();
        assert_eq!(luma(&back), luma(&f));
        assert!(apply(&f, &VideoFilter::Rotate(45)).is_err());
    }

    #[test]
    fn crop_center_vs_explicit() {
        let f = frame(8, 8);
        let center = apply(&f, &VideoFilter::Crop { w: 4, h: 4, x: None, y: None }).unwrap();
        assert_eq!((center.width, center.height), (4, 4));
        assert_eq!(luma(&center)[0], 18); // src(2,2)
        let explicit = apply(&f, &VideoFilter::Crop { w: 4, h: 4, x: Some(0), y: Some(0) }).unwrap();
        assert_eq!(luma(&explicit)[0], 0); // src(0,0)
    }

    #[test]
    fn pad_centers_and_grayscale() {
        let p = apply(&frame(2, 2), &VideoFilter::Pad { w: 6, h: 6, x: None, y: None }).unwrap();
        assert_eq!((p.width, p.height), (6, 6));
        assert_eq!(luma(&p)[0], 16); // black fill
        let g = apply(&frame(4, 4), &VideoFilter::Grayscale).unwrap();
        assert!(g.data[16..].iter().all(|&b| b == 128));
    }

    #[test]
    fn ten_bit_hflip() {
        let mut data: Vec<u8> = Vec::new();
        for s in [0u16, 1, 2, 3] {
            data.extend_from_slice(&s.to_le_bytes());
        }
        data.extend_from_slice(&(512u16).to_le_bytes());
        data.extend_from_slice(&(512u16).to_le_bytes());
        let f = VideoFrame::new(Bytes::from(data), 2, 2, PixelFormat::Yuv420p10le, ColorSpace::Bt709, 0);
        let out = apply(&f, &VideoFilter::HFlip).unwrap();
        assert_eq!(&out.data[0..2], &1u16.to_le_bytes());
    }
}
