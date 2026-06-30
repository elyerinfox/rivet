//! Rung and quality types — one rendition of the output ladder plus the encoder
//! quality knobs that control it.

use codec::encode::tuning::{QualityTarget, SpeedTier};
use codec::encode::{AUTO_FROM_TARGET, EncoderConfig};

/// Encoder quality knobs for a rung.
#[derive(Debug, Clone)]
pub struct Quality {
    /// Constant rate factor in the encoder-native scale (rav1e/NVENC 0..=255).
    /// `None` derives the quantizer from [`Quality::target`].
    pub crf: Option<u8>,
    /// Encoder-native speed preset. `None` derives it from [`Quality::tier`].
    pub speed_preset: Option<u8>,
    /// Perceptual quality target (used when `crf` is `None`).
    pub target: QualityTarget,
    /// Speed/efficiency tier (used when `speed_preset` is `None`).
    pub tier: SpeedTier,
    /// GOP length in frames. `None` → `2 × frame_rate` (a 2-second GOP).
    pub keyframe_interval: Option<u32>,
}

impl Default for Quality {
    fn default() -> Self {
        Self {
            crf: None,
            speed_preset: None,
            target: QualityTarget::Standard,
            tier: SpeedTier::Standard,
            keyframe_interval: None,
        }
    }
}

impl Quality {
    /// A constant-rate-factor quality.
    pub fn crf(crf: u8) -> Self {
        Self {
            crf: Some(crf),
            ..Default::default()
        }
    }

    /// A perceptual-target quality.
    pub fn target(target: QualityTarget) -> Self {
        Self {
            target,
            ..Default::default()
        }
    }

    /// Apply these knobs onto an [`EncoderConfig`] for a given frame rate.
    pub(crate) fn apply(&self, cfg: &mut EncoderConfig, frame_rate: f64) {
        cfg.target = self.target;
        cfg.tier = self.tier;
        cfg.quality = self.crf.unwrap_or(AUTO_FROM_TARGET);
        cfg.speed_preset = self.speed_preset.unwrap_or(AUTO_FROM_TARGET);
        cfg.keyframe_interval = self
            .keyframe_interval
            .unwrap_or_else(|| (frame_rate * 2.0).round().max(1.0) as u32);
    }
}

/// One rendition of the output ladder.
#[derive(Debug, Clone)]
pub struct Rung {
    /// Target width in pixels (even).
    pub width: u32,
    /// Target height in pixels (even).
    pub height: u32,
    /// Human label, e.g. `"720p"` (short side). Auto-derived by [`Rung::new`].
    pub label: String,
    /// Per-rung encoder quality.
    pub quality: Quality,
}

impl Rung {
    /// A rung at `width × height` with default quality and an auto label
    /// (`"<short-side>p"`).
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            label: format!("{}p", width.min(height)),
            quality: Quality::default(),
        }
    }

    /// Override the per-rung quality.
    pub fn with_quality(mut self, quality: Quality) -> Self {
        self.quality = quality;
        self
    }

    /// Override the label.
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Short side (the "p" number).
    pub fn short_side(&self) -> u32 {
        self.width.min(self.height)
    }
}
