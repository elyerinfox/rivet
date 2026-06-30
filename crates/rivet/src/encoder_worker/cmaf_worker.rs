//! CMAF segment worker: encodes one chunk and writes one CMAF segment file.

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::mpsc;
use codec::encode::{self, EncoderConfig};
use codec::frame::ColorMetadata;
use container::cmaf::{CmafVideoMuxer, CmafVideoMuxerOptions, SegmentInfo};
use crate::cmaf_util::add_packet_with_segment_flush;
use crate::frame_queue::{SegmentChunk, SegmentChunkQueue};
use super::{EncoderWorkerConfig, WorkerOutput, InvariantCheck, validate_or_set_rung_invariant};

/// Run the encoder loop until the chunk queue is closed and drained.
/// Designed to be wrapped in `tokio::task::spawn_blocking`.
///
/// `progress_tx` receives the shared cumulative `frames_encoded_total`
/// after every encoded frame; the caller's drain task fires wire
/// events from this stream. Multiple workers bump the same counter,
/// so the progress reading stays monotonic across worker handoffs.
#[allow(clippy::too_many_arguments)]
pub fn run_encoder_worker_blocking(
    cfg: EncoderWorkerConfig,
    queue: Arc<SegmentChunkQueue>,
    rt: tokio::runtime::Handle,
    shared_frames_encoded: Arc<std::sync::atomic::AtomicU64>,
    progress_tx: mpsc::Sender<u64>,
) -> Result<WorkerOutput> {
    let enc_config = super::build_enc_config(&cfg);
    let encoder_color_metadata = cfg.output_color_metadata;

    let mut segments_written: Vec<SegmentInfo> = Vec::new();
    let mut init_segment_written = false;

    tracing::debug!(rung_idx = cfg.rung_idx, gpu_index = ?cfg.gpu_index, "encoder worker started; awaiting first chunk");
    loop {
        let chunk = match rt.block_on(queue.pop()) {
            Some(c) => c,
            None => break,
        };
        tracing::debug!(rung_idx = cfg.rung_idx, segment = chunk.segment_idx, frames = chunk.frames.len(), "encoder worker popped chunk");
        match encode_one_segment(
            &cfg,
            &enc_config,
            encoder_color_metadata,
            chunk,
            &mut init_segment_written,
            &shared_frames_encoded,
            &progress_tx,
        )? {
            SegmentOutcome::Wrote {
                info,
                segment_idx,
                frames,
            } => {
                let role = if segment_idx == 0 {
                    "primary"
                } else {
                    "worker"
                };
                tracing::info!(
                    rung_idx = cfg.rung_idx,
                    gpu_index = ?cfg.gpu_index,
                    role,
                    segment = segment_idx,
                    frames_encoded = frames,
                    "rung segment flushed",
                );
                segments_written.push(info);
            }
            SegmentOutcome::RequeuedOnMismatch {
                chunk: rejected,
                diff,
            } => {
                // Helper from a vendor whose AV1 sequence header diverges
                // from the rung's invariant on mandatory fields. Put the
                // chunk back at the head of the queue so a matching-vendor
                // worker (always at least the initial worker) picks it up.
                // Exit clean — the run completes without this helper.
                tracing::warn!(
                    rung_idx = cfg.rung_idx,
                    gpu_index = ?cfg.gpu_index,
                    gpu_vendor = ?cfg.gpu_vendor,
                    rejected_segment = rejected.segment_idx,
                    diff = %diff,
                    "encoder worker: codec invariant mismatch on first packet — \
                     requeuing chunk for a matching-vendor worker and exiting",
                );
                let _ = queue.push_front(rejected);
                break;
            }
        }
    }

    Ok(WorkerOutput {
        gpu_index: cfg.gpu_index,
        segments: segments_written,
    })
}

/// Outcome of an `encode_one_segment` call. `Wrote` is the happy
/// path; `RequeuedOnMismatch` returns the chunk verbatim so the outer
/// loop can put it back at the head of the queue for another worker.
enum SegmentOutcome {
    Wrote {
        info: SegmentInfo,
        segment_idx: usize,
        frames: usize,
    },
    RequeuedOnMismatch {
        chunk: SegmentChunk,
        diff: String,
    },
}

fn encode_one_segment(
    cfg: &EncoderWorkerConfig,
    enc_config: &EncoderConfig,
    encoder_color_metadata: ColorMetadata,
    chunk: SegmentChunk,
    init_segment_written: &mut bool,
    shared_frames_encoded: &std::sync::atomic::AtomicU64,
    progress_tx: &mpsc::Sender<u64>,
) -> Result<SegmentOutcome> {
    let write_init = chunk.segment_idx == 0 && !*init_segment_written;
    let muxer_options = CmafVideoMuxerOptions {
        first_segment_index: (chunk.segment_idx as u32) + 1,
        first_segment_base_decode_time: chunk.segment_idx as u64 * cfg.segment_target_ticks,
        write_init_segment: write_init,
    };
    let mut muxer = CmafVideoMuxer::new_with_codec_options(
        &cfg.output_dir,
        cfg.width,
        cfg.height,
        cfg.timescale,
        encoder_color_metadata,
        cfg.codec,
        muxer_options,
    )
    .with_context(|| {
        format!(
            "creating CmafVideoMuxer for segment {} in {}",
            chunk.segment_idx,
            cfg.output_dir.display()
        )
    })?;

    let mut encoder =
        encode::select_encoder(enc_config.clone(), None).context("creating encoder for segment")?;

    // Buffered packets emitted from the encoder, awaiting either
    // commit-to-muxer (after invariant validation passes) or discard
    // (on mismatch). The first packet's bytes are the AV1 sequence
    // header OBU that we feed to the invariant validator.
    let mut pending_packets: Vec<codec::encode::EncodedPacket> = Vec::new();
    let mut first_packet_decision: Option<bool> = None; // None=undecided, Some(true)=commit, Some(false)=reject

    let segment_idx = chunk.segment_idx;
    let frame_count = chunk.frames.len();

    for frame in &chunk.frames {
        encoder
            .send_frame(frame)
            .context("encoder.send_frame in worker")?;
        while let Some(packet) = encoder
            .receive_packet()
            .context("encoder.receive_packet in worker")?
        {
            if first_packet_decision.is_none() {
                match validate_or_set_rung_invariant(
                    cfg.rung_idx,
                    cfg.gpu_vendor,
                    &cfg.rung_invariant,
                    &packet.data,
                    cfg.codec,
                )? {
                    InvariantCheck::Matched | InvariantCheck::SetByThisWorker => {
                        first_packet_decision = Some(true);
                    }
                    InvariantCheck::Mismatched { diff } => {
                        // Discard everything in flight. The muxer hasn't
                        // flushed any segment yet (first packet of a
                        // chunk is far below the segment-duration target),
                        // and init.mp4 is only written by finalize() —
                        // which we don't call. Drop muxer + encoder
                        // implicitly when we return.
                        return Ok(SegmentOutcome::RequeuedOnMismatch { chunk, diff });
                    }
                }
                pending_packets.push(packet);
                continue;
            }
            // first_packet_decision == Some(true): commit
            // First drain any buffered packets we held back during
            // validation.
            if !pending_packets.is_empty() {
                for held in pending_packets.drain(..) {
                    add_packet_with_segment_flush(
                        &mut muxer,
                        &held,
                        cfg.per_frame_ticks,
                        cfg.segment_target_ticks,
                    )
                    .context("CMAF segment-flush add (held)")?;
                }
            }
            add_packet_with_segment_flush(
                &mut muxer,
                &packet,
                cfg.per_frame_ticks,
                cfg.segment_target_ticks,
            )
            .context("CMAF segment-flush add (worker)")?;
        }
        let n = shared_frames_encoded.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
        let _ = progress_tx.try_send(n);
    }

    // Drain remaining held packets (e.g. if the only packets emitted
    // were buffered during the single validation step).
    if first_packet_decision == Some(true) && !pending_packets.is_empty() {
        for held in pending_packets.drain(..) {
            add_packet_with_segment_flush(
                &mut muxer,
                &held,
                cfg.per_frame_ticks,
                cfg.segment_target_ticks,
            )
            .context("CMAF segment-flush add (final-held)")?;
        }
    }

    encoder.flush().context("encoder.flush in worker")?;
    while let Some(packet) = encoder
        .receive_packet()
        .context("encoder.receive_packet after flush")?
    {
        add_packet_with_segment_flush(
            &mut muxer,
            &packet,
            cfg.per_frame_ticks,
            cfg.segment_target_ticks,
        )
        .context("CMAF segment-flush add post-flush (worker)")?;
    }

    let manifest = muxer
        .finalize()
        .context("finalize CmafVideoMuxer (per-segment worker)")?;

    if write_init {
        *init_segment_written = true;
    }

    let info = manifest
        .segments
        .last()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "encoder worker produced no segment for chunk idx {} (rung {}, gpu {:?}); \
                 frames in chunk = {}",
                segment_idx,
                cfg.rung_idx,
                cfg.gpu_index,
                frame_count,
            )
        })?
        .clone();
    Ok(SegmentOutcome::Wrote {
        info,
        segment_idx,
        frames: frame_count,
    })
}
