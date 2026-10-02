//! Platform-independent signal measurements, not an audibility classifier.

use thiserror::Error;

const TRUE_PEAK_TAPS_PER_PHASE: usize = 128;
pub(crate) const TRUE_PEAK_GROUP_DELAY_FRAMES: usize = 64;
const TRUE_PEAK_OVERSAMPLING: usize = 4;
fn true_peak_phases() -> &'static [[f64; TRUE_PEAK_TAPS_PER_PHASE]; TRUE_PEAK_OVERSAMPLING] {
    use std::{f64::consts::PI, sync::OnceLock};
    static PHASES: OnceLock<[[f64; TRUE_PEAK_TAPS_PER_PHASE]; TRUE_PEAK_OVERSAMPLING]> =
        OnceLock::new();
    PHASES.get_or_init(|| {
        let mut phases = [[0.0; TRUE_PEAK_TAPS_PER_PHASE]; TRUE_PEAK_OVERSAMPLING];
        for (phase, taps) in phases.iter_mut().enumerate() {
            for (tap, coefficient) in taps.iter_mut().enumerate() {
                let distance = tap as f64
                    - (TRUE_PEAK_TAPS_PER_PHASE / 2 - 1) as f64
                    - phase as f64 / TRUE_PEAK_OVERSAMPLING as f64;
                let sinc = if distance.abs() < 1e-12 {
                    1.0
                } else {
                    (PI * distance).sin() / (PI * distance)
                };
                let window = 0.5
                    - 0.5 * (2.0 * PI * tap as f64 / (TRUE_PEAK_TAPS_PER_PHASE - 1) as f64).cos();
                *coefficient = sinc * window;
            }
            let sum = taps.iter().sum::<f64>();
            for coefficient in taps.iter_mut() {
                *coefficient /= sum;
            }
            taps.reverse();
        }
        phases
    })
}

/// Runtime-adjustable thresholds used by signal diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct AudioDiagnosticsConfig {
    /// Minimum same-channel sample delta considered a discontinuity candidate, in Q15 units.
    ///
    /// The default is 16,384 (half scale). Set to `0` to disable discontinuity
    /// candidate counting. This heuristic is intended to locate suspicious
    /// transitions; it does not prove that a listener would hear a click.
    pub discontinuity_threshold_q15: u16,
}

impl Default for AudioDiagnosticsConfig {
    fn default() -> Self {
        Self {
            discontinuity_threshold_q15: 16_384,
        }
    }
}

/// A measurement of one interleaved audio frame, including floating-point mix stages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct AudioFrameMetrics {
    /// Sampling rate of the measured frame, in hertz.
    pub sample_rate_hz: u32,
    /// Number of interleaved channels in the measured frame.
    pub channels: u8,
    /// Number of interleaved samples in the frame, across all channels.
    pub samples: u64,
    /// Largest absolute sample magnitude in Q15 units; values above 32,768 are retained.
    pub peak_q15: u32,
    /// Estimated 4x oversampled true peak in Q15 units.
    pub true_peak_q15: u32,
    /// Frame RMS amplitude in Q15 units, rounded down to an integer.
    pub rms_q15: u32,
    /// Samples at or above unity; this is not proof of audible clipping.
    pub full_scale_samples: u64,
    /// Same-channel adjacent sample jumps above the configured threshold.
    pub discontinuity_candidates: u64,
    /// Threshold used to count discontinuity candidates, in Q15 units.
    pub discontinuity_threshold_q15: u16,
}

/// Cumulative measurements for one named pipeline stage.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct AudioSignalStats {
    /// Number of frames observed by this stage.
    pub frames: u64,
    /// Total number of interleaved samples observed by this stage.
    pub samples: u64,
    /// Sample rate of the most recently observed frame, in hertz.
    pub last_sample_rate_hz: u32,
    /// Channel count of the most recently observed frame.
    pub last_channels: u8,
    /// Highest frame peak observed in Q15 units, including values above digital full scale.
    pub max_peak_q15: u32,
    /// Highest estimated 4x oversampled true peak observed in Q15 units.
    pub max_true_peak_q15: u32,
    /// Highest per-frame RMS observed, in Q15 units.
    pub max_frame_rms_q15: u32,
    /// Total samples at or above unity.
    pub full_scale_samples: u64,
    /// Total same-channel adjacent sample jumps above the configured threshold.
    pub discontinuity_candidates: u64,
    /// Threshold used for the most recently observed frame, in Q15 units.
    pub last_discontinuity_threshold_q15: u16,
}

impl AudioSignalStats {
    /// Adds one frame measurement using saturating counters and returns its stage-local index.
    pub fn record_frame(&mut self, metrics: AudioFrameMetrics) -> u64 {
        self.frames = self.frames.saturating_add(1);
        self.samples = self.samples.saturating_add(metrics.samples);
        self.last_sample_rate_hz = metrics.sample_rate_hz;
        self.last_channels = metrics.channels;
        self.last_discontinuity_threshold_q15 = metrics.discontinuity_threshold_q15;
        self.max_peak_q15 = self.max_peak_q15.max(metrics.peak_q15);
        self.max_true_peak_q15 = self.max_true_peak_q15.max(metrics.true_peak_q15);
        self.max_frame_rms_q15 = self.max_frame_rms_q15.max(metrics.rms_q15);
        self.full_scale_samples = self
            .full_scale_samples
            .saturating_add(metrics.full_scale_samples);
        self.discontinuity_candidates = self
            .discontinuity_candidates
            .saturating_add(metrics.discontinuity_candidates);
        self.frames.saturating_sub(1)
    }
}

/// Invalid PCM shape errors returned by [`AudioSignalAnalyzer`].
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum AudioAnalysisError {
    /// The frame has no valid sample rate or channel layout.
    #[error("PCM sample rate and channel count must be positive")]
    InvalidFormat,
    /// The sample count does not contain complete interleaved channel groups.
    #[error("PCM sample count is not divisible by its channel count")]
    IncompleteInterleavedFrame,
    /// A floating-point sample was NaN or infinite.
    #[error("audio sample is not finite")]
    NonFiniteSample,
}

/// Stateful, allocation-bounded analyzer for signed-16 PCM and normalized f32 mix audio.
///
/// The analyzer retains only the previous sample for each channel so it can
/// count frame-boundary jumps without comparing unrelated channels. Call
/// [`reset_history`](Self::reset_history) after a known discontinuity.
#[derive(Debug, Clone)]
pub struct AudioSignalAnalyzer {
    config: AudioDiagnosticsConfig,
    previous_samples: Vec<Option<i64>>,
    true_peak_estimator: TruePeakEstimator,
    previous_sample_rate_hz: Option<u32>,
}

impl AudioSignalAnalyzer {
    /// Creates an analyzer using the supplied discontinuity threshold.
    pub fn new(config: AudioDiagnosticsConfig) -> Self {
        Self {
            config,
            previous_samples: Vec::new(),
            true_peak_estimator: TruePeakEstimator::new(0),
            previous_sample_rate_hz: None,
        }
    }

    /// Replaces the threshold and clears boundary history when its value changes.
    pub fn set_config(&mut self, config: AudioDiagnosticsConfig) {
        if self.config != config {
            self.reset_history();
        }
        self.config = config;
    }

    /// Clears frame-boundary history, for example after a sequence gap or device restart.
    pub fn reset_history(&mut self) {
        self.previous_samples.fill(None);
        self.true_peak_estimator.reset(self.previous_samples.len());
        self.previous_sample_rate_hz = None;
    }

    /// Measures peak, RMS, full-scale samples, and abrupt same-channel transitions.
    ///
    /// RMS is computed with an integer square root so the result is stable across
    /// platforms. This pass performs no allocation unless the channel layout changes.
    ///
    /// # Errors
    ///
    /// Returns an error if the frame has a zero sample rate/channel count or an
    /// incomplete interleaved sample group.
    pub fn observe_i16_frame(
        &mut self,
        samples: &[i16],
        sample_rate_hz: u32,
        channels: u8,
    ) -> Result<AudioFrameMetrics, AudioAnalysisError> {
        let channel_count = usize::from(channels);
        if sample_rate_hz == 0 || channel_count == 0 {
            return Err(AudioAnalysisError::InvalidFormat);
        }
        if !samples.len().is_multiple_of(channel_count) {
            return Err(AudioAnalysisError::IncompleteInterleavedFrame);
        }
        self.prepare_layout(sample_rate_hz, channel_count);

        let mut peak_q15 = 0_u32;
        let mut true_peak_q15 = 0_u32;
        let mut full_scale_samples = 0_u64;
        let mut discontinuity_candidates = 0_u64;
        let mut sum_squares = 0_u128;
        let threshold = u32::from(self.config.discontinuity_threshold_q15);

        for (index, sample) in samples.iter().copied().enumerate() {
            let channel = index % channel_count;
            true_peak_q15 =
                true_peak_q15.max(amplitude_to_q15(self.true_peak_estimator.observe_sample(
                    f64::from(sample) / 32_768.0,
                    channel,
                    channel_count,
                )));
            let magnitude = i64::from(sample).unsigned_abs();
            peak_q15 = peak_q15.max(magnitude.min(u64::from(u32::MAX)) as u32);
            sum_squares = sum_squares.saturating_add(u128::from(magnitude) * u128::from(magnitude));
            if sample == i16::MIN || sample == i16::MAX {
                full_scale_samples = full_scale_samples.saturating_add(1);
            }

            if threshold > 0
                && let Some(previous) = self.previous_samples[channel]
            {
                let delta = (i64::from(sample) - previous).unsigned_abs();
                if delta >= u64::from(threshold) {
                    discontinuity_candidates = discontinuity_candidates.saturating_add(1);
                }
            }
            self.previous_samples[channel] = Some(i64::from(sample));
        }

        let rms_q15 = if samples.is_empty() {
            0
        } else {
            integer_sqrt(sum_squares / samples.len() as u128).min(u128::from(u32::MAX)) as u32
        };

        Ok(AudioFrameMetrics {
            sample_rate_hz,
            channels,
            samples: samples.len() as u64,
            peak_q15,
            true_peak_q15,
            rms_q15,
            full_scale_samples,
            discontinuity_candidates,
            discontinuity_threshold_q15: self.config.discontinuity_threshold_q15,
        })
    }

    /// Measures normalized floating-point interleaved audio, including values above unity.
    pub fn observe_f32_frame(
        &mut self,
        samples: &[f32],
        sample_rate_hz: u32,
        channels: u8,
    ) -> Result<AudioFrameMetrics, AudioAnalysisError> {
        let channel_count = usize::from(channels);
        if sample_rate_hz == 0 || channel_count == 0 {
            return Err(AudioAnalysisError::InvalidFormat);
        }
        if !samples.len().is_multiple_of(channel_count) {
            return Err(AudioAnalysisError::IncompleteInterleavedFrame);
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(AudioAnalysisError::NonFiniteSample);
        }
        self.prepare_layout(sample_rate_hz, channel_count);

        let mut peak_q15 = 0_u32;
        let mut true_peak_q15 = 0_u32;
        let mut full_scale_samples = 0_u64;
        let mut discontinuity_candidates = 0_u64;
        let mut sum_squares = 0_u128;
        let threshold = u64::from(self.config.discontinuity_threshold_q15);

        for (index, sample) in samples.iter().copied().enumerate() {
            let q15 = (f64::from(sample) * 32_768.0)
                .round()
                .clamp(-f64::from(u32::MAX), f64::from(u32::MAX)) as i64;
            let magnitude = q15.unsigned_abs().min(u64::from(u32::MAX));
            let channel = index % channel_count;
            true_peak_q15 = true_peak_q15.max(amplitude_to_q15(
                self.true_peak_estimator
                    .observe_sample(f64::from(sample), channel, channel_count),
            ));
            peak_q15 = peak_q15.max(magnitude as u32);
            sum_squares = sum_squares.saturating_add(u128::from(magnitude) * u128::from(magnitude));
            if magnitude >= 32_768 {
                full_scale_samples = full_scale_samples.saturating_add(1);
            }
            if threshold > 0
                && let Some(previous) = self.previous_samples[channel]
                && (q15 - previous).unsigned_abs() >= threshold
            {
                discontinuity_candidates = discontinuity_candidates.saturating_add(1);
            }
            self.previous_samples[channel] = Some(q15);
        }

        let rms_q15 = if samples.is_empty() {
            0
        } else {
            integer_sqrt(sum_squares / samples.len() as u128).min(u128::from(u32::MAX)) as u32
        };
        Ok(AudioFrameMetrics {
            sample_rate_hz,
            channels,
            samples: samples.len() as u64,
            peak_q15,
            true_peak_q15,
            rms_q15,
            full_scale_samples,
            discontinuity_candidates,
            discontinuity_threshold_q15: self.config.discontinuity_threshold_q15,
        })
    }

    fn prepare_layout(&mut self, sample_rate_hz: u32, channels: usize) {
        if self.previous_samples.len() != channels {
            self.previous_samples = vec![None; channels];
            self.true_peak_estimator.reset(channels);
        } else if self.previous_sample_rate_hz != Some(sample_rate_hz) {
            self.previous_samples.fill(None);
            self.true_peak_estimator.reset(channels);
        }
        self.previous_sample_rate_hz = Some(sample_rate_hz);
    }
}

/// Stateful 4x, 128-tap Hann-sinc reconstruction shared by diagnostics and the limiter.
#[derive(Debug, Clone)]
pub(crate) struct TruePeakEstimator {
    history: Vec<[f64; TRUE_PEAK_TAPS_PER_PHASE * 2]>,
    cursor: usize,
}

impl TruePeakEstimator {
    pub(crate) fn new(channels: usize) -> Self {
        // Initialize outside the sample path; observe only reads immutable coefficients.
        let _ = true_peak_phases();
        Self {
            history: vec![[0.0; TRUE_PEAK_TAPS_PER_PHASE * 2]; channels],
            cursor: 0,
        }
    }

    pub(crate) fn reset(&mut self, channels: usize) {
        if self.history.len() != channels {
            self.history = vec![[0.0; TRUE_PEAK_TAPS_PER_PHASE * 2]; channels];
        } else {
            self.history.fill([0.0; TRUE_PEAK_TAPS_PER_PHASE * 2]);
        }
        self.cursor = 0;
    }

    pub(crate) fn observe_sample(&mut self, sample: f64, channel: usize, channels: usize) -> f64 {
        let cursor = self.cursor;
        let history = &mut self.history[channel];
        history[cursor] = sample;
        history[cursor + TRUE_PEAK_TAPS_PER_PHASE] = sample;
        let mut peak = sample.abs();
        // Mirrored history makes every FIR window contiguous: no per-tap modulo.
        // Phase zero is an exact sample, not a fractional interpolation.
        let window = &history[cursor + 1..cursor + 1 + TRUE_PEAK_TAPS_PER_PHASE];
        peak = peak.max(window[TRUE_PEAK_TAPS_PER_PHASE / 2].abs());
        for phase in &true_peak_phases()[1..] {
            // Independent accumulators expose instruction-level parallelism without
            // fast-math, architecture-specific unsafe code or changing the kernel.
            let mut sums = [0.0; 4];
            for (samples, weights) in window
                .as_chunks::<4>()
                .0
                .iter()
                .zip(phase.as_chunks::<4>().0)
            {
                for lane in 0..4 {
                    sums[lane] += samples[lane] * weights[lane];
                }
            }
            let interpolated = sums.iter().sum::<f64>();
            peak = peak.max(interpolated.abs());
        }
        if channel + 1 == channels {
            self.cursor = (cursor + 1) % TRUE_PEAK_TAPS_PER_PHASE;
        }
        peak
    }
}

/// Returns the floor of the square root without floating-point platform variance.
fn integer_sqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut estimate = value;
    let mut next = estimate.div_ceil(2);
    while next < estimate {
        estimate = next;
        next = (estimate + value / estimate) / 2;
    }
    estimate
}

fn amplitude_to_q15(amplitude: f64) -> u32 {
    (amplitude * 32_768.0)
        .round()
        .clamp(0.0, f64::from(u32::MAX)) as u32
}
