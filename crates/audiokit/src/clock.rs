//! Queue-clock feedback expressed in physical units, with explicit recovery gates.
use crate::{AudioError, AudioResult};
use serde::{Deserialize, Serialize};

/// Bounded proportional queue controller. Defaults match the old 48 kHz gain in ms.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueClockConfig {
    /// Desired projected post-write queue residence in milliseconds.
    pub target_ms: f64,
    /// Maximum absolute ratio correction in ppm, 0..=1000; zero disables it.
    pub max_ppm: u16,
    /// Queue error to ratio gain, in ppm per millisecond, positive.
    pub gain_ppm_per_ms: f64,
    /// Low-pass time constant in milliseconds, positive.
    pub smoothing_ms: f64,
    /// Maximum ratio movement in ppm/second, positive.
    pub slew_ppm_per_second: f64,
    /// Symmetric filtered queue-error dead band, in milliseconds.
    pub dead_band_ms: f64,
}
impl Default for QueueClockConfig {
    fn default() -> Self {
        Self {
            target_ms: 90.0,
            max_ppm: 500,
            gain_ppm_per_ms: 48.0,
            smoothing_ms: 1000.0,
            slew_ppm_per_second: 500.0,
            dead_band_ms: 0.01,
        }
    }
}
impl QueueClockConfig {
    /// Validates finite units/ranges before state changes. Targets are limited to 2 seconds.
    pub fn validate(self) -> AudioResult<Self> {
        if !self.target_ms.is_finite()
            || !(0.0..=2000.0).contains(&self.target_ms)
            || self.max_ppm > 1000
            || !self.gain_ppm_per_ms.is_finite()
            || !(0.01..=1000.0).contains(&self.gain_ppm_per_ms)
            || !self.smoothing_ms.is_finite()
            || !(1.0..=10_000.0).contains(&self.smoothing_ms)
            || !self.slew_ppm_per_second.is_finite()
            || !(0.01..=10_000.0).contains(&self.slew_ppm_per_second)
            || !self.dead_band_ms.is_finite()
            || !(0.0..=100.0).contains(&self.dead_band_ms)
        {
            return Err(AudioError::InvalidConfig(
                "invalid queue-clock physical-unit configuration".into(),
            ));
        }
        Ok(self)
    }
}

/// Queue state provided by the scheduler; arrivals alone are not hardware-clock evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueClockState {
    /// Stable device-consumption cadence and queue watermarks are available.
    Running,
    /// Startup, rebuffering, pause, burst or a discontinuity: reset instead of adapting.
    Recovering,
}

/// One update result, including filtered error and whether a configured cap was insufficient.
#[derive(Debug, Clone, Copy, Default)]
pub struct QueueClockUpdate {
    /// Applied linked-channel correction in integer ppm.
    pub correction_ppm: i32,
    /// True when the unconstrained requested correction exceeds max_ppm.
    pub saturated: bool,
    /// Filtered target-minus-queue error in milliseconds.
    pub error_ms: f64,
}

/// One queue/device controller; not shared between independent source clocks.
pub struct QueueClockController {
    config: QueueClockConfig,
    filtered_ms: f64,
    applied_ppm: f64,
}

/// Rate inferred from host arrivals/presentation demand; this is not a synchronized remote timestamp.
#[derive(Debug, Clone, Copy)]
pub struct RateEstimate {
    /// Actual per-host-time sample rate relative to nominal, in parts per million.
    pub drift_ppm: f64,
    /// Observation duration in host monotonic nanoseconds.
    pub window_ns: u64,
}

/// Low-frequency sample-clock estimate from monotonic frame positions in one host clock.
/// Network arrival jitter remains an uncertainty source; large disturbances reset the window.
pub struct SampleRateEstimator {
    nominal_rate: u32,
    window_ns: u64,
    anchor: Option<(u64, u64)>,
    last: Option<(u64, u64)>,
    estimate: Option<RateEstimate>,
}
impl SampleRateEstimator {
    /// Uses a 1..=60 second inference window. Caller labels source arrivals as estimated time.
    pub fn new(nominal_rate: u32, window_ms: u16) -> AudioResult<Self> {
        if nominal_rate == 0 || !(1000..=60_000).contains(&window_ms) {
            return Err(AudioError::InvalidConfig(
                "invalid sample-clock inference window/rate".into(),
            ));
        }
        Ok(Self {
            nominal_rate,
            window_ns: u64::from(window_ms) * 1_000_000,
            anchor: None,
            last: None,
            estimate: None,
        })
    }
    /// Clears inferred rate on epoch changes, host pauses, rebuffering and burst discontinuities.
    pub fn reset(&mut self) {
        self.anchor = None;
        self.last = None;
        self.estimate = None;
    }
    /// Observes a forward sample position in the same host monotonic clock.
    /// Reordered/duplicate media positions are ignored. Regressing host time or >100 ms
    /// unexpected arrival/presentation error resets instead of becoming a hardware correction.
    pub fn observe(&mut self, position: u64, host_ns: u64) -> Option<RateEstimate> {
        if let Some((last_position, last_ns)) = self.last {
            if host_ns < last_ns {
                self.reset();
            } else if position <= last_position {
                return self.estimate;
            } else {
                let elapsed = host_ns - last_ns;
                let media_ns = (position - last_position) as f64 * 1_000_000_000.0
                    / f64::from(self.nominal_rate);
                if (elapsed as f64 - media_ns).abs() > 100_000_000.0 {
                    self.reset();
                }
            }
        }
        self.last = Some((position, host_ns));
        let (start_position, start_ns) = *self.anchor.get_or_insert((position, host_ns));
        let elapsed = host_ns.saturating_sub(start_ns);
        if elapsed >= self.window_ns && position >= start_position {
            let actual = (position - start_position) as f64 * 1_000_000_000.0 / elapsed as f64;
            let ppm = (actual / f64::from(self.nominal_rate) - 1.0) * 1_000_000.0;
            self.estimate = if ppm.is_finite() && ppm.abs() <= 5000.0 {
                Some(RateEstimate {
                    drift_ppm: ppm,
                    window_ns: elapsed,
                })
            } else {
                None
            };
            self.anchor = Some((position, host_ns));
        }
        self.estimate
    }
    /// Returns the latest rate inference, or None before a complete undisturbed window.
    pub fn estimate(&self) -> Option<RateEstimate> {
        self.estimate
    }
}
impl QueueClockController {
    /// Creates a controller at its target with zero correction; worker-only configuration.
    pub fn new(config: QueueClockConfig) -> AudioResult<Self> {
        let config = config.validate()?;
        Ok(Self {
            config,
            filtered_ms: config.target_ms,
            applied_ppm: 0.0,
        })
    }
    /// Returns the physical-unit configuration used by this controller.
    pub fn config(&self) -> QueueClockConfig {
        self.config
    }
    /// Resets the filtered watermark and correction after a non-clock disturbance.
    pub fn reset(&mut self) {
        self.filtered_ms = self.config.target_ms;
        self.applied_ppm = 0.0;
    }
    /// Updates with a measured/projected queue duration and actual interval in milliseconds.
    /// Invalid measurements are rejected without state changes. Recovering returns zero.
    pub fn update(
        &mut self,
        queue_ms: f64,
        elapsed_ms: f64,
        state: QueueClockState,
    ) -> AudioResult<QueueClockUpdate> {
        if !queue_ms.is_finite()
            || !(0.0..=10_000.0).contains(&queue_ms)
            || !elapsed_ms.is_finite()
            || !(0.0..=1000.0).contains(&elapsed_ms)
        {
            return Err(AudioError::InvalidFrame(
                "invalid queue clock measurement".into(),
            ));
        }
        if state == QueueClockState::Recovering || self.config.max_ppm == 0 {
            self.reset();
            return Ok(QueueClockUpdate::default());
        }
        let smoothing = 1.0 - (-elapsed_ms / self.config.smoothing_ms).exp();
        self.filtered_ms += (queue_ms - self.filtered_ms) * smoothing;
        let error_ms = self.config.target_ms - self.filtered_ms;
        let requested = if error_ms.abs() <= self.config.dead_band_ms {
            0.0
        } else {
            error_ms * self.config.gain_ppm_per_ms
        };
        let cap = f64::from(self.config.max_ppm);
        let bounded = requested.clamp(-cap, cap);
        let step = self.config.slew_ppm_per_second * elapsed_ms / 1000.0;
        self.applied_ppm += (bounded - self.applied_ppm).clamp(-step, step);
        Ok(QueueClockUpdate {
            correction_ppm: self.applied_ppm.round() as i32,
            saturated: requested.abs() > cap,
            error_ms,
        })
    }
}
