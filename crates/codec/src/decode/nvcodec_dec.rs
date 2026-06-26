//! NVIDIA NVDEC decoder via `shiguredo_nvcodec` (Apache-2.0).
//!
//! The decode counterpart to `encode/nvenc.rs`. Compiled only under the
//! `nvidia` feature; when the feature is off the dispatcher uses the built-in
//! hand-rolled NVDEC (`decode/nvdec.rs`) instead. Like the encoder, the crate
//! bindgens the NVIDIA Video Codec SDK headers (needs libclang) and dlopens
//! CUDA at runtime — and it builds on Linux but not on a Windows MSVC host
//! (signed-vs-unsigned C enum ABI; see `Cargo.toml`).
//!
//! Output is NV12 8-bit; we deinterleave to `Yuv420p` for the pipeline. The
//! decoder is callback-based: `decode(bytes)` feeds compressed samples and the
//! handler delivers decoded frames, which we queue for `decode_next()`. This
//! plugs straight into the shared decode pump (`create_decoder_on` →
//! `push_sample` → `decode_next`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use bytes::Bytes;

use shiguredo_nvcodec::{
    DecodedFrame, Decoder as NvDecoder, DecoderCodec, DecoderConfig, Error as NvError,
    FnDecodeHandler, SurfaceFormat,
};

use super::Decoder;
use super::nv12_planes_to_yuv420p;
use crate::frame::{PixelFormat, StreamInfo, VideoFrame};

type Collector = Arc<Mutex<VecDeque<VideoFrame>>>;

/// Codecs this NVDEC wrapper handles. MPEG-2 / MPEG-4 are decodable by NVDEC
/// but not exposed by `shiguredo_nvcodec`'s `DecoderCodec`; the dispatcher
/// falls back to the built-in NVDEC for those.
pub fn supports(codec_lower: &str) -> bool {
    nvcodec_codec_for(codec_lower).is_some()
}

fn nvcodec_codec_for(codec_lower: &str) -> Option<DecoderCodec> {
    Some(match codec_lower {
        "h264" | "avc1" | "avc" => DecoderCodec::H264,
        "h265" | "hevc" | "hvc1" | "hev1" | "hvc2" | "hev2" => DecoderCodec::Hevc,
        "av1" | "av01" => DecoderCodec::Av1,
        "vp8" => DecoderCodec::Vp8,
        "vp9" | "vp09" => DecoderCodec::Vp9,
        _ => return None,
    })
}

pub struct NvcodecDecoder {
    info: StreamInfo,
    inner: NvDecoder<FnDecodeHandler<u64>>,
    collected: Collector,
    finished: bool,
    frame_counter: u64,
}

unsafe impl Send for NvcodecDecoder {}

impl NvcodecDecoder {
    pub fn new(info: StreamInfo, gpu_index: u32) -> Result<Self> {
        let codec_lower = info.codec.to_ascii_lowercase();
        let codec = nvcodec_codec_for(&codec_lower)
            .ok_or_else(|| anyhow!("nvcodec decoder: codec '{codec_lower}' unsupported"))?;

        let collected: Collector = Arc::new(Mutex::new(VecDeque::new()));
        let sink = Arc::clone(&collected);
        let color_space = info.color_space;
        let handler =
            FnDecodeHandler::new(move |frame: std::result::Result<DecodedFrame<u64>, NvError>| {
                match frame {
                    Ok(f) => {
                        let w = f.width();
                        let h = f.height();
                        let data = nv12_planes_to_yuv420p(
                            f.y_plane(),
                            f.y_stride(),
                            f.uv_plane(),
                            f.uv_stride(),
                            w,
                            h,
                        );
                        let pts = *f.user_data();
                        sink.lock().unwrap().push_back(VideoFrame::new(
                            Bytes::from(data),
                            w as u32,
                            h as u32,
                            PixelFormat::Yuv420p,
                            color_space,
                            pts,
                        ));
                    }
                    Err(e) => tracing::warn!("nvcodec decode callback error: {e:?}"),
                }
            });

        let config = DecoderConfig {
            codec,
            device_id: gpu_index as i32,
            max_num_decode_surfaces: 20,
            max_display_delay: 0,
            surface_format: SurfaceFormat::Nv12,
        };
        let inner = NvDecoder::new(config, handler)
            .map_err(|e| anyhow!("shiguredo_nvcodec::Decoder::new (gpu_index={gpu_index}): {e:?}"))?;

        Ok(Self {
            info,
            inner,
            collected,
            finished: false,
            frame_counter: 0,
        })
    }
}

impl Decoder for NvcodecDecoder {
    fn stream_info(&self) -> &StreamInfo {
        &self.info
    }

    fn push_sample(&mut self, data: &[u8]) -> Result<()> {
        self.inner
            .decode(data, self.frame_counter)
            .map_err(|e| anyhow!("shiguredo_nvcodec::Decoder::decode: {e:?}"))?;
        self.frame_counter += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.inner
            .flush()
            .map_err(|e| anyhow!("shiguredo_nvcodec::Decoder::flush: {e:?}"))?;
        Ok(())
    }

    fn decode_next(&mut self) -> Result<Option<VideoFrame>> {
        Ok(self.collected.lock().unwrap().pop_front())
    }
}
