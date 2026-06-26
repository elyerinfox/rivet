//! Stub Intel QSV decoder, compiled when the `qsv` feature is **off**.
//!
//! Keeps `qsv::QsvDecoder` a real type so the dispatcher in
//! `decode/mod.rs` compiles unchanged, but construction always errors —
//! the dispatcher then bails ("no GPU decoder available"), exactly as if
//! the host had no Intel hardware. Enable `--features qsv` to compile the
//! real oneVPL-backed decoder (`qsv.rs`).

use anyhow::{Result, bail};

use super::Decoder;
use crate::frame::{StreamInfo, VideoFrame};

#[allow(dead_code)]
pub struct QsvDecoder {
    info: StreamInfo,
}

impl QsvDecoder {
    pub fn new(_info: StreamInfo, _gpu_index: u32) -> Result<Self> {
        bail!(
            "QSV (Intel oneVPL) decode support was not compiled in; \
             rebuild with the `qsv` feature enabled to use Intel hardware decode"
        )
    }
}

impl Decoder for QsvDecoder {
    fn stream_info(&self) -> &StreamInfo {
        &self.info
    }
    fn push_sample(&mut self, _data: &[u8]) -> Result<()> {
        unreachable!("stub QSV decoder is never constructed")
    }
    fn finish(&mut self) -> Result<()> {
        unreachable!("stub QSV decoder is never constructed")
    }
    fn decode_next(&mut self) -> Result<Option<VideoFrame>> {
        unreachable!("stub QSV decoder is never constructed")
    }
}
