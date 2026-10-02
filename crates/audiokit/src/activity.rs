//! Energy activity with hysteresis/hangover. It is a mixing policy, not a speech classifier.
use crate::{AudioError, AudioResult};
use serde::{Deserialize, Serialize};

/// Configurable activity envelope. Thresholds are RMS dBFS, not sample peaks.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActivityConfig {
    /// RMS threshold for an inactive stream to enter activity.
    pub enter_dbfs: f32,
    /// Lower RMS threshold for active streams to start their hangover countdown.
    pub exit_dbfs: f32,
    /// Energy envelope attack time constant in milliseconds.
    pub attack_ms: f32,
    /// Energy envelope release time constant in milliseconds.
    pub release_ms: f32,
    /// Hold time below the exit threshold before activity ends.
    pub hangover_ms: f32,
}
impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            enter_dbfs: -45.0,
            exit_dbfs: -50.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            hangover_ms: 150.0,
        }
    }
}
impl ActivityConfig {
    /// Validates finite thresholds and nonnegative, bounded physical durations.
    pub fn validate(self) -> AudioResult<Self> {
        if !self.enter_dbfs.is_finite()
            || !self.exit_dbfs.is_finite()
            || !(-100.0..=0.0).contains(&self.enter_dbfs)
            || !(-100.0..=self.enter_dbfs).contains(&self.exit_dbfs)
            || !self.attack_ms.is_finite()
            || !(0.1..=1000.0).contains(&self.attack_ms)
            || !self.release_ms.is_finite()
            || !(0.1..=2000.0).contains(&self.release_ms)
            || !self.hangover_ms.is_finite()
            || !(0.0..=2000.0).contains(&self.hangover_ms)
        {
            return Err(AudioError::InvalidConfig(
                "invalid activity thresholds/time constants".into(),
            ));
        }
        Ok(self)
    }
}

/// Persistent per-source activity state. A silent newly registered source is inactive.
pub struct ActivityDetector {
    config: ActivityConfig,
    power: f64,
    active: bool,
    below_ms: f64,
}
impl ActivityDetector {
    /// Creates a validated detector with zero energy and no hangover.
    pub fn new(config: ActivityConfig) -> AudioResult<Self> {
        Ok(Self {
            config: config.validate()?,
            power: 0.0,
            active: false,
            below_ms: 0.0,
        })
    }
    /// Updates the envelope from finite interleaved PCM and its per-channel duration.
    /// Time constants are applied per frame, so callback/block length does not change the detector.
    pub fn observe(&mut self, pcm: &[f32], channels: u8, sample_rate: u32) -> AudioResult<bool> {
        if channels == 0
            || sample_rate == 0
            || !pcm.len().is_multiple_of(usize::from(channels))
            || !pcm.iter().all(|s| s.is_finite())
        {
            return Err(AudioError::InvalidFrame("invalid activity PCM".into()));
        }
        let dt_ms = 1000.0 / f64::from(sample_rate);
        let enter = 10_f64.powf(f64::from(self.config.enter_dbfs) / 10.0);
        let exit = 10_f64.powf(f64::from(self.config.exit_dbfs) / 10.0);
        for frame in pcm.chunks_exact(usize::from(channels)) {
            let power =
                frame.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / f64::from(channels);
            let tau = if power > self.power {
                self.config.attack_ms
            } else {
                self.config.release_ms
            };
            self.power += (power - self.power) * (1.0 - (-dt_ms / f64::from(tau)).exp());
            if !self.active && self.power >= enter {
                self.active = true;
                self.below_ms = 0.0;
            }
            if self.active {
                if self.power < exit {
                    self.below_ms += dt_ms;
                    if self.below_ms >= f64::from(self.config.hangover_ms) {
                        self.active = false;
                    }
                } else {
                    self.below_ms = 0.0;
                }
            }
        }
        Ok(self.active)
    }
    /// Returns the filtered RMS power as dBFS; silence is negative infinity.
    pub fn envelope_dbfs(&self) -> f64 {
        10.0 * self.power.log10()
    }
}
