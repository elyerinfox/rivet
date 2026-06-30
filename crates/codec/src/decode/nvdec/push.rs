//! **Legacy push-mode wrapper** — buffers all samples until `finish()`,
//! then calls `NvdecDecoder::new_with_pts` for the full eager parse.
//! Retained as library code; the production dispatch path uses
//! `NvdecStreamingDecoder` directly.
//!
//! Memory cost: O(total bitstream bytes) until finish() — unchanged from
//! the pre-Squad-36 steady state, since the eager constructor also took
//! Vec<Vec<u8>> up front. Task #55's goal was to reduce *frame* memory
//! (decoded frame bytes × variant count) not *bitstream* memory, so this
//! tradeoff is aligned with the intent.

use anyhow::Result;

use crate::decode::Decoder;
use crate::frame::{StreamInfo, VideoFrame};
use super::eager::NvdecDecoder;

// ─── Push-mode wrapper ─────────────────────────────────────────────
//
// cuvid's parser fundamentally wants all samples fed via
// cuvidParseVideoData in one pass so its stateful picture queue can
// build up reference-list context. Our NvdecDecoder::new() does this
// eagerly: takes Vec<Vec<u8>>, runs the whole parse loop, then hands
// back an instance pre-populated with decoded frames.
//
// The push-mode trait (#55) wants per-sample streaming. The cheapest
// safe bridge: buffer the incoming samples in a Vec, then on finish()
// call NvdecDecoder::new with the accumulated buffer. This preserves
// the full fix stack (CUVIDPARSERPARAMS size, CUVIDPICPARAMS size,
// CUVID_CREATE_PREFER_CUVID, bAnnexb=1, struct-size assertions) that
// took tasks #39/#52/#53/#65 to nail down, without re-architecting
// the parser loop into an incremental feeder.
pub struct NvdecPushDecoder {
    info: StreamInfo,
    gpu_index: u32,
    /// (bitstream bytes, PTS). The PTS is either a real demuxer PTS
    /// from `push_sample_with_pts`, or a fabricated monotonic index
    /// from `push_sample`. codec-review-2 HIGH-3: keep real PTS
    /// end-to-end so B-frame display order survives.
    pending_samples: Vec<(Vec<u8>, u64)>,
    decoded: Option<Box<dyn Decoder>>,
    finished: bool,
}

impl NvdecPushDecoder {
    pub fn new(info: StreamInfo, gpu_index: u32) -> Self {
        Self {
            info,
            gpu_index,
            pending_samples: Vec::new(),
            decoded: None,
            finished: false,
        }
    }

    /// Push with an explicit PTS. Preferred over the trait-level
    /// `push_sample` when the caller has the real demuxer timestamp,
    /// because the trait signature (no PTS) forces us to fabricate a
    /// counter that is wrong for B-frame-heavy streams.
    pub fn push_sample_with_pts(&mut self, data: &[u8], pts: u64) -> Result<()> {
        if self.finished {
            anyhow::bail!("NvdecPushDecoder: push_sample after finish");
        }
        self.pending_samples.push((data.to_vec(), pts));
        Ok(())
    }
}

impl Decoder for NvdecPushDecoder {
    fn stream_info(&self) -> &StreamInfo {
        &self.info
    }

    fn push_sample(&mut self, data: &[u8]) -> Result<()> {
        if self.finished {
            anyhow::bail!("NvdecPushDecoder: push_sample after finish");
        }
        // No real PTS supplied — fabricate a monotonic counter from
        // the current buffer length so each sample at least gets a
        // distinct timestamp. Callers that need correct display-order
        // PTS (B-frame streams) should use `push_sample_with_pts`.
        let pts = self.pending_samples.len() as u64;
        self.pending_samples.push((data.to_vec(), pts));
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let samples = std::mem::take(&mut self.pending_samples);
        // Eager decode: the existing new_with_pts does the full cuvid
        // parse loop. On success we store the resulting Decoder so
        // decode_next can delegate. On error we propagate — the outer
        // factory is responsible for CPU fallback logging.
        self.decoded = Some(NvdecDecoder::new_with_pts(
            samples,
            self.info.clone(),
            self.gpu_index,
        )?);
        Ok(())
    }

    fn decode_next(&mut self) -> Result<Option<VideoFrame>> {
        match self.decoded.as_mut() {
            Some(inner) => inner.decode_next(),
            None => {
                // Caller pulled without finishing — treat as no-op
                // rather than panic so streaming code that polls
                // decode_next opportunistically doesn't crash.
                Ok(None)
            }
        }
    }
}
