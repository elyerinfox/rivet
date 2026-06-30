use std::path::Path;

use anyhow::{Context, Result};

use codec::audio::{
    AudioCodec, AudioEncoderConfig, create_decoder as audio_decoder,
    create_encoder as audio_encoder,
};
use container::cmaf::CmafAudioMuxer;
use container::demux::AudioTrack;
use container::hls::AudioVariantSpec;
use container::AudioInfo;

use crate::cmaf_util::add_audio_sample_with_segment_flush;
use crate::spec::AudioCodecPolicy;

// ---------------------------------------------------------------------------
// PreparedAudio
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(super) struct PreparedAudio {
    pub(super) info: AudioInfo,
    pub(super) samples: Vec<(Vec<u8>, u32)>,
    pub(super) handling: String,
}

impl PreparedAudio {
    pub(super) fn has_samples(&self) -> bool {
        !self.samples.is_empty()
    }

    /// Append another track's samples after this one (for splice concat). The
    /// muxer re-times from the running duration, so the joined audio is gap-free.
    pub(super) fn extend(&mut self, other: &PreparedAudio) {
        self.samples.extend(other.samples.iter().cloned());
    }
}

// ---------------------------------------------------------------------------
// Audio preparation
// ---------------------------------------------------------------------------

pub(super) fn prepare_audio(
    track: Option<&AudioTrack>,
    policy: AudioCodecPolicy,
) -> Result<Option<PreparedAudio>> {
    let Some(track) = track else {
        return Ok(None);
    };
    if policy == AudioCodecPolicy::Drop {
        return Ok(None);
    }
    let codec = track.codec.to_ascii_lowercase();
    let passthrough_ok = matches!(codec.as_str(), "aac" | "opus" | "ac3" | "eac3");
    let force_opus = policy == AudioCodecPolicy::ForceOpus;

    if passthrough_ok && !(force_opus && codec != "opus") {
        let info = passthrough_info(&codec, track);
        let samples = track
            .samples
            .iter()
            .cloned()
            .zip(track.durations.iter().copied())
            .collect();
        return Ok(Some(PreparedAudio {
            info,
            samples,
            handling: format!("{codec} passthrough"),
        }));
    }

    if matches!(codec.as_str(), "mp3" | "vorbis") || force_opus {
        if track.channels > 2 {
            tracing::warn!(codec, channels = track.channels, "multichannel audio dropped");
            return Ok(Some(dropped(format!("{codec} ({}ch)", track.channels))));
        }
        if !matches!(codec.as_str(), "mp3" | "vorbis") {
            tracing::warn!(codec, "cannot transcode to opus; dropping audio");
            return Ok(Some(dropped(codec)));
        }
        let extra: Option<&[u8]> =
            if track.codec_private.is_empty() { None } else { Some(track.codec_private.as_slice()) };
        let mut dec = audio_decoder(&codec, extra, track.sample_rate, track.channels as u8)
            .context("audio decoder")?;
        let bitrate = if track.channels == 1 { 64_000 } else { 96_000 };
        let mut enc = audio_encoder(AudioEncoderConfig {
            codec: AudioCodec::Opus,
            sample_rate: track.sample_rate,
            channels: track.channels as u8,
            bitrate,
        })
        .context("opus encoder")?;

        let mut samples: Vec<(Vec<u8>, u32)> = Vec::new();
        let mut pts: i64 = 0;
        for packet in &track.samples {
            for frame in dec.decode(packet, pts).context("audio decode")? {
                pts = pts.saturating_add((frame.samples.len() as i64) / frame.channels.max(1) as i64);
                for pkt in enc.encode(&frame).context("opus encode")? {
                    samples.push((pkt.data, pkt.duration as u32));
                }
            }
        }
        for frame in dec.flush().context("audio flush")? {
            for pkt in enc.encode(&frame).context("opus encode flush")? {
                samples.push((pkt.data, pkt.duration as u32));
            }
        }
        for pkt in enc.flush().context("opus encoder flush")? {
            samples.push((pkt.data, pkt.duration as u32));
        }
        let info = AudioInfo::opus(48_000, track.channels, enc.extra_data());
        return Ok(Some(PreparedAudio {
            info,
            samples,
            handling: format!("{codec} → opus"),
        }));
    }

    Ok(Some(dropped(codec)))
}

fn dropped(codec: String) -> PreparedAudio {
    PreparedAudio {
        info: AudioInfo::aac_lc(48_000, 2, Vec::new()),
        samples: Vec::new(),
        handling: format!("{codec} dropped"),
    }
}

fn passthrough_info(codec: &str, track: &AudioTrack) -> AudioInfo {
    match codec {
        "aac" => AudioInfo::aac_lc(track.sample_rate, track.channels, track.asc.clone()),
        "opus" => AudioInfo::opus(track.sample_rate, track.channels, track.codec_private.clone()),
        "ac3" => AudioInfo::ac3(track.sample_rate, track.channels, track.codec_private.clone()),
        "eac3" => AudioInfo::eac3(track.sample_rate, track.channels, track.codec_private.clone()),
        _ => AudioInfo::aac_lc(track.sample_rate, track.channels, track.asc.clone()),
    }
}

// ---------------------------------------------------------------------------
// HLS audio rendition builder
// ---------------------------------------------------------------------------

pub(super) fn build_audio_rendition(
    asset_root: &Path,
    audio: &PreparedAudio,
    segment_seconds: f32,
) -> Result<Option<AudioVariantSpec>> {
    if !audio.has_samples() {
        return Ok(None);
    }
    let audio_dir = asset_root.join("audio");
    let seg_target_ticks = (segment_seconds as f64 * audio.info.timescale as f64).round() as u64;
    let mut muxer = CmafAudioMuxer::new(&audio_dir, audio.info.clone()).context("CmafAudioMuxer::new")?;
    for (payload, dur) in &audio.samples {
        add_audio_sample_with_segment_flush(&mut muxer, payload.clone(), *dur, seg_target_ticks)?;
    }
    muxer.flush_segment().context("final audio flush_segment")?;
    let manifest = muxer.finalize().context("CmafAudioMuxer finalize")?;
    let codec_string = match audio.info.codec.as_str() {
        "opus" => "opus".to_string(),
        _ => codec::codec_strings::AAC_LC_CODEC_STRING.to_string(),
    };
    Ok(Some(AudioVariantSpec {
        codec_string,
        channels: audio.info.channels,
        sample_rate: audio.info.sample_rate,
        relative_dir: "audio".to_string(),
        language: "und".to_string(),
        name: "Audio".to_string(),
        manifest,
    }))
}
