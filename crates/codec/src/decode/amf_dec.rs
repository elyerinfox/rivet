//! AMD AMF decoder via `shiguredo_amf` (Apache-2.0).
//!
//! New capability — the project previously had no AMD decode path (NVDEC for
//! NVIDIA, QSV for Intel). Compiled only under the `amd` feature. Like the AMF
//! encoder, the crate bindgens the AMF SDK headers (needs libclang) and dlopens
//! the AMF runtime; it builds on Linux but not on a Windows MSVC host (C enum
//! ABI; see `Cargo.toml`). Decodes H.264 / HEVC / AV1.
//!
//! AMF decode output is an NV12 host `Surface`; we deinterleave it to
//! `Yuv420p`. Callback-based (`decode(buffer)` feeds samples, the handler
//! delivers frames), so it plugs into the shared decode pump exactly like the
//! NVDEC path. `shiguredo_amf` selects the default AMF adapter — multi-AMD
//! pinning is not exposed (a `gpu_index` other than 0 is accepted but logged).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;

use shiguredo_amf::{
    DecodedFrame, Decoder as AmfDec, DecoderCodec, DecoderConfig, Error as AmfError, FnDecodeHandler,
};

use super::Decoder;
use super::nv12_planes_to_yuv420p;
use crate::frame::{PixelFormat, StreamInfo, VideoFrame};

type Collector = Arc<Mutex<VecDeque<VideoFrame>>>;

/// Codecs this AMF decoder handles.
pub fn supports(codec_lower: &str) -> bool {
    amf_codec_for(codec_lower).is_some()
}

fn amf_codec_for(codec_lower: &str) -> Option<DecoderCodec> {
    Some(match codec_lower {
        "h264" | "avc1" | "avc" => DecoderCodec::H264,
        "h265" | "hevc" | "hvc1" | "hev1" | "hvc2" | "hev2" => DecoderCodec::Hevc,
        "av1" | "av01" => DecoderCodec::Av1,
        _ => return None,
    })
}

pub struct AmfDecoder {
    info: StreamInfo,
    inner: AmfDec<FnDecodeHandler<u64>>,
    collected: Collector,
    finished: bool,
    frame_counter: u64,
}

unsafe impl Send for AmfDecoder {}

impl AmfDecoder {
    pub fn new(info: StreamInfo, gpu_index: u32) -> Result<Self> {
        let codec_lower = info.codec.to_ascii_lowercase();
        let codec = amf_codec_for(&codec_lower)
            .ok_or_else(|| anyhow!("AMF decoder: codec '{codec_lower}' unsupported"))?;
        if gpu_index != 0 {
            tracing::warn!(
                gpu_index,
                "shiguredo_amf selects the default AMF adapter; multi-GPU decode pinning unsupported"
            );
        }

        let collected: Collector = Arc::new(Mutex::new(VecDeque::new()));
        let sink = Arc::clone(&collected);
        let color_space = info.color_space;
        let out_w = info.width as usize;
        let out_h = info.height as usize;

        let handler =
            FnDecodeHandler::new(move |frame: std::result::Result<DecodedFrame<u64>, AmfError>| {
                let f = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("AMF decode callback error: {e:?}");
                        return;
                    }
                };
                let surface = f.surface();
                let (yp, uvp) = match (surface.get_plane_at(0), surface.get_plane_at(1)) {
                    (Ok(y), Ok(uv)) => (y, uv),
                    _ => {
                        tracing::warn!("AMF decode: could not read NV12 planes");
                        return;
                    }
                };
                let y_native = yp.get_native() as *const u8;
                let uv_native = uvp.get_native() as *const u8;
                if y_native.is_null() || uv_native.is_null() {
                    tracing::warn!("AMF decode: null plane pointer");
                    return;
                }
                let y_pitch = yp.get_hpitch() as usize;
                let uv_pitch = uvp.get_hpitch() as usize;
                let y_rows = yp.get_height() as usize;
                let uv_rows = uvp.get_height() as usize;
                // Clamp output dims to what the surface actually holds.
                let w = out_w.min(y_pitch);
                let h = out_h.min(y_rows).min(uv_rows * 2);
                // SAFETY: the planes hold `pitch * rows` valid bytes; we read at
                // most `h` (≤ rows) rows of `w` (≤ pitch) bytes.
                let y_slice = unsafe { std::slice::from_raw_parts(y_native, y_pitch * y_rows) };
                let uv_slice = unsafe { std::slice::from_raw_parts(uv_native, uv_pitch * uv_rows) };
                let data = nv12_planes_to_yuv420p(y_slice, y_pitch, uv_slice, uv_pitch, w, h);
                let pts = *f.user_data();
                sink.lock().unwrap().push_back(VideoFrame::new(
                    Bytes::from(data),
                    w as u32,
                    h as u32,
                    PixelFormat::Yuv420p,
                    color_space,
                    pts,
                ));
            });

        let inner = AmfDec::new(DecoderConfig { codec }, handler)
            .map_err(|e| anyhow!("shiguredo_amf::Decoder::new (gpu_index={gpu_index}): {e:?}"))?;

        Ok(Self {
            info,
            inner,
            collected,
            finished: false,
            frame_counter: 0,
        })
    }
}

impl Decoder for AmfDecoder {
    fn stream_info(&self) -> &StreamInfo {
        &self.info
    }

    fn push_sample(&mut self, data: &[u8]) -> Result<()> {
        let buf = self
            .inner
            .alloc_buffer(data.len())
            .map_err(|e| anyhow!("shiguredo_amf::Decoder::alloc_buffer: {e:?}"))?;
        let native = buf.get_native() as *mut u8;
        if native.is_null() {
            bail!("AMF decode: input buffer native pointer is null");
        }
        // SAFETY: `buf` was allocated with `data.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), native, data.len());
        }
        self.inner
            .decode(buf, self.frame_counter)
            .map_err(|e| anyhow!("shiguredo_amf::Decoder::decode: {e:?}"))?;
        self.frame_counter += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.inner
            .finish()
            .map_err(|e| anyhow!("shiguredo_amf::Decoder::finish: {e:?}"))?;
        Ok(())
    }

    fn decode_next(&mut self) -> Result<Option<VideoFrame>> {
        Ok(self.collected.lock().unwrap().pop_front())
    }
}
