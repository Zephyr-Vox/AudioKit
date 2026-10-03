//! Independent rational sample-clock scheduling and bounded render stress controls.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

/// Independent simulated rates; no native timestamp or actual device is implied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClockConfig {
    /// Capture samples per host second relative to nominal, -2000..=2000 ppm.
    pub capture_rate_ppm: i32,
    /// Render demand per host second relative to nominal, -2000..=2000 ppm.
    pub render_rate_ppm: i32,
}
impl ClockConfig {
    /// Rejects rates outside the supported deterministic stress envelope.
    pub fn validate(&self) -> Result<()> {
        if self.capture_rate_ppm.unsigned_abs() > 2000 || self.render_rate_ppm.unsigned_abs() > 2000
        {
            return Err(Error::Invalid(
                "simulated sample clock exceeds 2000 ppm".into(),
            ));
        }
        Ok(())
    }
    /// True if either virtual clock differs from the nominal clock.
    pub fn is_shifted(&self) -> bool {
        *self != Self::default()
    }
}

/// Correlated source replicas stress production limiters; silence is separately registered.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MixStressConfig {
    /// Total independent registered sources, 1..=64 including silent sources.
    pub sources: usize,
    /// Sources carrying exact digital silence; must be smaller than sources.
    pub silent_sources: usize,
    /// Production source gain target for all sources, 0..=4.
    pub source_gain: f32,
    /// Sum of admitted per-source PCM frames, 1..=200000000.
    pub max_total_source_frames: u64,
}
impl Default for MixStressConfig {
    fn default() -> Self {
        Self {
            sources: 8,
            silent_sources: 0,
            source_gain: 1.0,
            max_total_source_frames: 10_000_000,
        }
    }
}
impl MixStressConfig {
    /// Validates source/work bounds; graph admission is validated separately.
    pub fn validate(&self) -> Result<()> {
        if !(1..=64).contains(&self.sources)
            || self.silent_sources >= self.sources
            || !self.source_gain.is_finite()
            || !(audiokit::mix::MIN_SOURCE_VOLUME..=audiokit::mix::MAX_SOURCE_VOLUME)
                .contains(&self.source_gain)
            || !(1..=200_000_000).contains(&self.max_total_source_frames)
        {
            return Err(Error::Invalid(
                "invalid mix stress source/gain/work limits".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) struct SampleClock {
    rate: i32,
    tick: u64,
}
impl SampleClock {
    pub fn new(rate: i32) -> Self {
        Self { rate, tick: 1 }
    }
    pub fn next_ns(&self) -> u64 {
        // Compute from the origin, not repeated rounded increments. Positive ppm
        // consumes the same 10 ms sample quantum in less host time.
        (u128::from(self.tick) * 10_000_000 * 1_000_000
            / (1_000_000 + i64::from(self.rate)) as u128) as u64
    }
    pub fn advance(&mut self) {
        self.tick += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rational_clocks_have_no_accumulated_tick_rounding_and_validate_extremes() {
        for ppm in [-2000, 0, 2000] {
            let mut clock = SampleClock::new(ppm);
            for _ in 0..60_000 {
                clock.advance();
            }
            let expected = (60_001_u128 * 10_000_000 * 1_000_000
                / (1_000_000 + i64::from(ppm)) as u128) as u64;
            assert_eq!(clock.next_ns(), expected);
        }
        assert!(
            ClockConfig {
                capture_rate_ppm: i32::MIN,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            MixStressConfig {
                sources: 1,
                silent_sources: 1,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
