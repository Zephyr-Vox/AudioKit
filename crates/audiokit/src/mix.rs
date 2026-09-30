//! Shared configuration and gain smoothing for multi-source audio mixing.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Minimum supported per-source gain.
pub const MIN_SOURCE_VOLUME: f32 = 0.0;
/// Maximum supported per-source gain.
pub const MAX_SOURCE_VOLUME: f32 = 4.0;

/// User-configurable source gain transition settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixConfig {
    /// Duration used when a source's target gain changes.
    pub fade_ms: u16,
}

impl Default for MixConfig {
    fn default() -> Self {
        Self { fade_ms: 50 }
    }
}

impl MixConfig {
    /// Validates the configured source-gain transition duration.
    pub fn validate(self) -> Result<Self, MixConfigError> {
        if !(2..=50).contains(&self.fade_ms) {
            return Err(MixConfigError::FadeOutOfRange);
        }
        Ok(self)
    }

    /// Converts the transition duration to a nonzero number of sample frames.
    pub fn fade_frames(self, sample_rate_hz: u32) -> Result<usize, MixConfigError> {
        self.validate()?;
        if sample_rate_hz == 0 {
            return Err(MixConfigError::InvalidSampleRate);
        }
        let frames = (u64::from(sample_rate_hz) * u64::from(self.fade_ms)).div_ceil(1_000);
        Ok(usize::try_from(frames.max(1)).expect("fade duration fits supported address spaces"))
    }
}

/// Invalid source gain smoothing configuration.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum MixConfigError {
    /// Fade duration is outside the supported range.
    #[error("mix fade_ms must be between 2 and 50")]
    FadeOutOfRange,
    /// A sample rate is required to convert milliseconds to sample frames.
    #[error("mix sample rate must be greater than zero")]
    InvalidSampleRate,
}

/// Smooths one source's gain with the same gain applied to every channel.
///
/// New instances start at silence. Call [`apply_interleaved`](Self::apply_interleaved)
/// for each block, including a zero-filled block when the source has a packet gap,
/// so transitions remain continuous across missing packets.
#[derive(Debug, Clone, Copy, Default)]
pub struct SourceGainSmoother {
    current_gain: f32,
    target_gain: f32,
    gain_step: f32,
    remaining_frames: usize,
}

impl SourceGainSmoother {
    /// Returns the current gain at the most recently processed sample frame.
    pub fn current_gain(&self) -> f32 {
        self.current_gain
    }

    /// Applies the requested gain transition in-place to interleaved float PCM.
    ///
    /// Gain advances once per sample frame and is shared across channels, avoiding
    /// stereo image shifts. A changed target starts a linear ramp from the current
    /// gain; changing the target again retargets from the instantaneous gain.
    pub fn apply_interleaved(
        &mut self,
        samples: &mut [f32],
        channels: u8,
        sample_rate_hz: u32,
        target_gain: f32,
        config: MixConfig,
    ) -> Result<(), SourceGainSmootherError> {
        if !target_gain.is_finite()
            || !(MIN_SOURCE_VOLUME..=MAX_SOURCE_VOLUME).contains(&target_gain)
        {
            return Err(SourceGainSmootherError::InvalidGain);
        }
        if channels == 0 || !samples.len().is_multiple_of(usize::from(channels)) {
            return Err(SourceGainSmootherError::InvalidFrameLayout);
        }
        let fade_frames = config.fade_frames(sample_rate_hz)?;
        if target_gain != self.target_gain {
            self.target_gain = target_gain;
            self.remaining_frames = fade_frames;
            self.gain_step = (target_gain - self.current_gain) / fade_frames as f32;
        }

        for frame in samples.chunks_exact_mut(usize::from(channels)) {
            if self.remaining_frames > 0 {
                self.current_gain += self.gain_step;
                self.remaining_frames -= 1;
                if self.remaining_frames == 0 {
                    self.current_gain = self.target_gain;
                }
            }
            for sample in frame {
                *sample *= self.current_gain;
            }
        }
        Ok(())
    }
}

/// Invalid input to a source gain smoother.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum SourceGainSmootherError {
    /// Gain is non-finite or outside the supported source-volume range.
    #[error("source gain must be finite and between 0.0 and 4.0")]
    InvalidGain,
    /// Channel count must be nonzero and divide the interleaved sample count.
    #[error("interleaved PCM sample count must be divisible by a nonzero channel count")]
    InvalidFrameLayout,
    /// The transition configuration or sample rate is invalid.
    #[error(transparent)]
    InvalidConfig(#[from] MixConfigError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_config_validates_fade_duration_and_converts_to_frames() {
        assert_eq!(MixConfig::default().fade_ms, 50);
        assert_eq!(MixConfig::default().fade_frames(48_000), Ok(2_400));
        assert_eq!(MixConfig { fade_ms: 2 }.fade_frames(44_100), Ok(89));
        assert_eq!(
            MixConfig { fade_ms: 1 }.validate(),
            Err(MixConfigError::FadeOutOfRange)
        );
        assert_eq!(
            MixConfig::default().fade_frames(0),
            Err(MixConfigError::InvalidSampleRate)
        );
    }

    #[test]
    fn gain_smoothing_is_channel_linked_and_reaches_the_target_exactly() {
        let mut smoother = SourceGainSmoother::default();
        let mut samples = vec![1.0, 0.5, 1.0, 0.5, 1.0, 0.5, 1.0, 0.5];
        smoother
            .apply_interleaved(&mut samples, 2, 1_000, 1.0, MixConfig { fade_ms: 2 })
            .unwrap();

        assert_eq!(samples, vec![0.5, 0.25, 1.0, 0.5, 1.0, 0.5, 1.0, 0.5]);
        assert_eq!(smoother.current_gain(), 1.0);
    }

    #[test]
    fn gain_retargeting_is_continuous_and_zero_blocks_advance_the_ramp() {
        let mut smoother = SourceGainSmoother::default();
        let mut first = [1.0; 1];
        smoother
            .apply_interleaved(&mut first, 1, 1_000, 1.0, MixConfig { fade_ms: 10 })
            .unwrap();
        assert_eq!(first, [0.1]);

        let mut silence = [0.0; 1];
        smoother
            .apply_interleaved(&mut silence, 1, 1_000, 0.0, MixConfig { fade_ms: 10 })
            .unwrap();
        assert_eq!(smoother.current_gain(), 0.09);

        let mut resumed = [1.0; 1];
        smoother
            .apply_interleaved(&mut resumed, 1, 1_000, 1.0, MixConfig { fade_ms: 10 })
            .unwrap();
        assert!(resumed[0] > 0.09);
        assert!(resumed[0] < 1.0);
    }

    #[test]
    fn gain_smoother_rejects_invalid_gain_and_frame_layout() {
        let mut smoother = SourceGainSmoother::default();
        assert_eq!(
            smoother.apply_interleaved(&mut [1.0], 1, 48_000, f32::NAN, MixConfig::default()),
            Err(SourceGainSmootherError::InvalidGain)
        );
        assert_eq!(
            smoother.apply_interleaved(&mut [1.0], 2, 48_000, 1.0, MixConfig::default()),
            Err(SourceGainSmootherError::InvalidFrameLayout)
        );
    }
}
