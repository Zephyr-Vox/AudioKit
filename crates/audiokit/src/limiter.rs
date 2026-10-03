//! Platform-independent lookahead true-peak limiting for interleaved audio.

use thiserror::Error;

use super::diagnostics::{TRUE_PEAK_GROUP_DELAY_FRAMES, TruePeakEstimator};

/// User-adjustable parameters for the linked-channel master limiter.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimiterConfig {
    /// Linear output ceiling relative to full scale in dB (for example, -1.0).
    pub ceiling_dbfs: f32,
    /// Audio delay used to inspect upcoming samples before they reach the output.
    pub lookahead_ms: f32,
    /// Time before an upcoming peak by which gain reduction should reach its target.
    pub attack_ms: f32,
    /// Exponential return time after the signal falls below the ceiling.
    pub release_ms: f32,
    /// Extra detector target headroom for finite interpolation and gain-modulated transients.
    /// Default 0.2 dB was selected from the independent reconstruction corpus; 0..=1 dB.
    /// This is not a claim of a mathematical bound for arbitrary near-Nyquist signals.
    #[serde(default = "default_reconstruction_headroom_db")]
    pub reconstruction_headroom_db: f32,
}
const fn default_reconstruction_headroom_db() -> f32 {
    0.2
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            ceiling_dbfs: -1.0,
            lookahead_ms: 3.0,
            attack_ms: 1.0,
            release_ms: 100.0,
            reconstruction_headroom_db: default_reconstruction_headroom_db(),
        }
    }
}

impl LimiterConfig {
    /// Checks parameter ranges before a config is stored or used by the DSP.
    pub fn validate(self) -> Result<Self, LimiterConfigError> {
        if !self.ceiling_dbfs.is_finite()
            || !self.lookahead_ms.is_finite()
            || !self.attack_ms.is_finite()
            || !self.release_ms.is_finite()
            || !self.reconstruction_headroom_db.is_finite()
        {
            return Err(LimiterConfigError::NotFinite);
        }
        if !(-18.0..=-0.5).contains(&self.ceiling_dbfs)
            || !(1.0..=10.0).contains(&self.lookahead_ms)
            || !(0.1..=5.0).contains(&self.attack_ms)
            || !(30.0..=300.0).contains(&self.release_ms)
            || !(0.0..=1.0).contains(&self.reconstruction_headroom_db)
        {
            return Err(LimiterConfigError::OutOfRange);
        }
        if self.attack_ms > self.lookahead_ms {
            return Err(LimiterConfigError::AttackExceedsLookahead);
        }
        Ok(self)
    }
}

/// User-adjustable input ceiling for each speaker/stream before the mix bus.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLimiterConfig {
    /// Linear source ceiling relative to full scale in dBFS.
    pub ceiling_dbfs: f32,
}

impl Default for SourceLimiterConfig {
    fn default() -> Self {
        Self { ceiling_dbfs: -3.0 }
    }
}

impl SourceLimiterConfig {
    /// Checks the per-source ceiling before it is applied to a playback graph.
    pub fn validate(self) -> Result<Self, SourceLimiterConfigError> {
        if !self.ceiling_dbfs.is_finite() {
            return Err(SourceLimiterConfigError::NotFinite);
        }
        if !(-18.0..=-1.0).contains(&self.ceiling_dbfs) {
            return Err(SourceLimiterConfigError::OutOfRange);
        }
        Ok(self)
    }
}

/// Invalid per-source limiter parameters.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum SourceLimiterConfigError {
    /// The ceiling is NaN or infinite.
    #[error("source limiter ceiling must be finite")]
    NotFinite,
    /// The ceiling is outside its supported range.
    #[error("source limiter ceiling_dbfs must be -18..=-1")]
    OutOfRange,
}

/// Invalid master limiter parameters.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum LimiterConfigError {
    /// One or more parameters are NaN or infinite.
    #[error("limiter parameters must be finite")]
    NotFinite,
    /// One or more parameters are outside their supported range.
    #[error(
        "ceiling_dbfs must be -18..=-0.5, lookahead_ms 1..=10, attack_ms 0.1..=5, release_ms 30..=300, and reconstruction_headroom_db 0..=1"
    )]
    OutOfRange,
    /// The attack window cannot be longer than the available lookahead.
    #[error("limiter attack_ms must not exceed lookahead_ms")]
    AttackExceedsLookahead,
}

/// Errors returned when processing interleaved floating-point samples.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum LimiterError {
    /// The sample rate is unsupported or the channel count is zero.
    #[error("limiter sample rate must be 1..=384000 Hz and channel count must be positive")]
    InvalidFormat,
    /// The interleaved input does not contain complete channel groups.
    #[error("limiter input has an incomplete interleaved frame")]
    IncompleteFrame,
    /// The input contains NaN or an infinite sample.
    #[error("limiter input samples must be finite")]
    NonFiniteSample,
    /// The requested limiter settings are invalid.
    #[error(transparent)]
    InvalidConfig(#[from] LimiterConfigError),
}

/// Per-block counters and maximum attenuation measured by the limiter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct LimiterFrameMetrics {
    /// Effective delay between mix input and corresponding limiter output, in sample frames.
    pub lookahead_frames: u32,
    /// Interleaved samples processed with gain below unity.
    pub attenuated_samples: u64,
    /// Samples that required the final numeric ceiling safeguard.
    pub safety_clamped_samples: u64,
    /// Largest pre-clamp excess over the ceiling, relative amplitude in parts per billion.
    pub max_safety_overshoot_ppb: u64,
    /// Greatest gain reduction in this block, in milli-decibels.
    pub max_gain_reduction_millidb: u32,
}

/// Stateful linked-channel lookahead limiter over normalized f32 samples.
///
/// It delays all channels equally, uses a four-times-oversampled true-peak
/// detector over the delayed window, and schedules gain reduction before peaks
/// reach the output. A final sample ceiling safeguard protects against
/// attack-time and floating-point edge cases.
///
/// Linked sample peaks and target gains are cached per delayed frame. They add
/// eight bytes of array storage per lookahead-ring frame (1,160 bytes at 48 kHz/
/// 3 ms), allocated when constructed, cloned or reconfigured. Processing still
/// allocates its returned block and must run on a worker, not a native callback.
#[derive(Debug, Clone)]
pub struct MasterLimiter {
    sample_rate_hz: u32,
    channels: usize,
    config: LimiterConfig,
    ceiling: f32,
    detector_ceiling: f32,
    lookahead_frames: usize,
    attack_frames: usize,
    release_coefficient: f32,
    delay: Vec<f32>,
    true_peak_delay: Vec<f32>,
    sample_peaks: Vec<f32>,
    targets: Vec<f32>,
    true_peak_estimator: TruePeakEstimator,
    cursor_frame: usize,
    gain: f32,
}

impl MasterLimiter {
    /// Creates a limiter for a fixed interleaved sample format up to 384 kHz.
    pub fn new(
        sample_rate_hz: u32,
        channels: u8,
        config: LimiterConfig,
    ) -> Result<Self, LimiterError> {
        if sample_rate_hz == 0 || sample_rate_hz > 384_000 || channels == 0 {
            return Err(LimiterError::InvalidFormat);
        }
        let config = config.validate()?;
        let lookahead_frames = ((config.lookahead_ms * sample_rate_hz as f32 / 1000.0).ceil()
            as usize)
            .max(TRUE_PEAK_GROUP_DELAY_FRAMES);
        let attack_frames = ((config.attack_ms * sample_rate_hz as f32 / 1000.0).ceil() as usize)
            .max(1)
            .min(lookahead_frames);
        let delay_frames = lookahead_frames.saturating_add(1);
        let release_seconds = config.release_ms / 1000.0;
        let release_coefficient = (-1.0 / (release_seconds * sample_rate_hz as f32)).exp();
        Ok(Self {
            sample_rate_hz,
            channels: usize::from(channels),
            config,
            ceiling: 10.0_f32.powf(config.ceiling_dbfs / 20.0),
            detector_ceiling: 10.0_f32
                .powf((config.ceiling_dbfs - config.reconstruction_headroom_db) / 20.0),
            lookahead_frames,
            attack_frames,
            release_coefficient,
            delay: vec![0.0; delay_frames * usize::from(channels)],
            true_peak_delay: vec![0.0; delay_frames],
            sample_peaks: vec![0.0; delay_frames],
            targets: vec![1.0; delay_frames],
            true_peak_estimator: TruePeakEstimator::new(usize::from(channels)),
            cursor_frame: 0,
            gain: 1.0,
        })
    }

    /// Returns the active limiter parameters.
    pub fn config(&self) -> LimiterConfig {
        self.config
    }
    /// Returns actual delay, including the reconstruction window's minimum lookahead.
    pub fn lookahead_frames(&self) -> usize {
        self.lookahead_frames
    }

    /// Reconfigures the limiter, clearing its delay and gain history.
    ///
    /// Reconfiguration is intentionally explicit because changing lookahead
    /// changes playback latency. Callers should apply it at a stream boundary.
    pub fn set_config(&mut self, config: LimiterConfig) -> Result<(), LimiterError> {
        let replacement = Self::new(self.sample_rate_hz, self.channels as u8, config)?;
        *self = replacement;
        Ok(())
    }

    /// Clears buffered audio and gain history while retaining the active config.
    pub fn reset(&mut self) {
        self.delay.fill(0.0);
        self.true_peak_delay.fill(0.0);
        self.sample_peaks.fill(0.0);
        self.targets.fill(1.0);
        self.true_peak_estimator.reset(self.channels);
        self.cursor_frame = 0;
        self.gain = 1.0;
    }

    /// Refreshes a slot after either its raw samples or reconstructed peak changes.
    /// All other slot targets remain valid until their own next write.
    fn refresh_target(&mut self, frame: usize) {
        let peak = self.sample_peaks[frame].max(self.true_peak_delay[frame]);
        self.targets[frame] = if peak > self.detector_ceiling {
            // Preserve f32 order and a few ULPs of margin: division/multiplication
            // rounding must not turn an exact target into a safety clamp.
            (self.detector_ceiling * (1.0 - 4.0 * f32::EPSILON)) / peak
        } else {
            1.0
        };
    }

    /// Processes one interleaved block and returns an equally sized delayed block.
    ///
    /// Channels share one detector and gain envelope, preserving stereo balance.
    /// The first effective lookahead window after construction/reset is silence.
    pub fn process_interleaved(
        &mut self,
        input: &[f32],
    ) -> Result<(Vec<f32>, LimiterFrameMetrics), LimiterError> {
        if !input.len().is_multiple_of(self.channels) {
            return Err(LimiterError::IncompleteFrame);
        }
        if input.iter().any(|sample| !sample.is_finite()) {
            return Err(LimiterError::NonFiniteSample);
        }

        let delay_frames = self.delay.len() / self.channels;
        let mut output = Vec::with_capacity(input.len());
        let mut metrics = LimiterFrameMetrics {
            lookahead_frames: self.lookahead_frames.min(u32::MAX as usize) as u32,
            ..LimiterFrameMetrics::default()
        };

        for input_frame in input.chunks_exact(self.channels) {
            // Ring length is lookahead + 1, so ingress always writes behind the cursor.
            let write_frame = if self.cursor_frame == 0 {
                delay_frames - 1
            } else {
                self.cursor_frame - 1
            };
            let write_start = write_frame * self.channels;
            self.delay[write_start..write_start + self.channels].copy_from_slice(input_frame);
            self.sample_peaks[write_frame] = input_frame
                .iter()
                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
            self.refresh_target(write_frame);
            let true_peak =
                input_frame
                    .iter()
                    .copied()
                    .enumerate()
                    .fold(0.0_f64, |peak, (channel, sample)| {
                        peak.max(self.true_peak_estimator.observe_sample(
                            f64::from(sample),
                            channel,
                            self.channels,
                        ))
                    }) as f32;
            // Fractional phases reconstruct positions 63..63.75 frames behind ingress.
            // Attribute their maximum to the nearest delayed frame before it is emitted.
            let true_peak_frame = if write_frame >= TRUE_PEAK_GROUP_DELAY_FRAMES {
                write_frame - TRUE_PEAK_GROUP_DELAY_FRAMES
            } else {
                write_frame + delay_frames - TRUE_PEAK_GROUP_DELAY_FRAMES
            };
            self.true_peak_delay[true_peak_frame] = true_peak;
            // The ring is at least 65 frames: these two writes touch distinct slots.
            // Refresh both, including the old reconstructed peak at the raw write slot.
            self.refresh_target(true_peak_frame);

            let mut upcoming_target = 1.0_f32;
            let mut scheduled_gain = self.gain;
            // Two contiguous slices retain distance order without per-candidate modulo.
            let tail_len = (delay_frames - self.cursor_frame).min(self.attack_frames + 1);
            let tail = &self.targets[self.cursor_frame..self.cursor_frame + tail_len];
            let head = &self.targets[..self.attack_frames + 1 - tail_len];
            for (distance, &target) in tail.iter().chain(head.iter()).enumerate() {
                upcoming_target = upcoming_target.min(target);
                if target < self.gain {
                    // Each peak imposes its own deadline. Combining only the
                    // lowest target with its distance can miss a nearer peak.
                    let candidate_gain = if distance == 0 {
                        target
                    } else {
                        self.gain + (target - self.gain) / distance as f32
                    };
                    scheduled_gain = scheduled_gain.min(candidate_gain);
                }
            }

            if scheduled_gain < self.gain {
                self.gain = scheduled_gain;
            } else if upcoming_target > self.gain {
                self.gain =
                    upcoming_target + (self.gain - upcoming_target) * self.release_coefficient;
            }

            let reduction = (-20.0 * self.gain.max(f32::MIN_POSITIVE).log10() * 1000.0)
                .ceil()
                .max(0.0) as u32;
            metrics.max_gain_reduction_millidb = metrics.max_gain_reduction_millidb.max(reduction);

            let output_start = self.cursor_frame * self.channels;
            for channel in 0..self.channels {
                let mut sample = self.delay[output_start + channel] * self.gain;
                if self.gain < 0.999_999 {
                    metrics.attenuated_samples = metrics.attenuated_samples.saturating_add(1);
                }
                if sample.abs() > self.ceiling {
                    let overshoot_ppb = ((f64::from(sample.abs() - self.ceiling)
                        / f64::from(self.ceiling))
                        * 1_000_000_000.0)
                        .ceil() as u64;
                    metrics.max_safety_overshoot_ppb =
                        metrics.max_safety_overshoot_ppb.max(overshoot_ppb);
                    sample = sample.clamp(-self.ceiling, self.ceiling);
                    metrics.safety_clamped_samples =
                        metrics.safety_clamped_samples.saturating_add(1);
                }
                output.push(sample);
            }
            self.cursor_frame += 1;
            if self.cursor_frame == delay_frames {
                self.cursor_frame = 0;
            }
        }

        Ok((output, metrics))
    }
}

#[cfg(test)]
#[path = "limiter/reference_tests.rs"]
mod reference_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(sample_rate_hz: u32, channels: u8) -> MasterLimiter {
        MasterLimiter::new(sample_rate_hz, channels, LimiterConfig::default()).unwrap()
    }

    #[test]
    fn linked_stereo_lookahead_keeps_output_under_the_sample_ceiling() {
        let mut limiter = limiter(48_000, 2);
        let mut input = vec![0.0; 600 * 2];
        input[400 * 2] = 2.0;
        input[400 * 2 + 1] = -2.0;

        let (output, metrics) = limiter.process_interleaved(&input).unwrap();
        let ceiling = 10.0_f32.powf(-1.0 / 20.0);

        assert_eq!(output.len(), input.len());
        assert!(output.iter().all(|sample| sample.abs() <= ceiling));
        assert!(metrics.attenuated_samples > 0);
        assert!(metrics.max_gain_reduction_millidb > 0);
        assert_eq!(
            metrics.safety_clamped_samples, 0,
            "max overshoot was {} ppb",
            metrics.max_safety_overshoot_ppb
        );
        assert_eq!(output[400 * 2].abs(), output[400 * 2 + 1].abs());
    }

    #[test]
    fn true_peak_lookahead_limits_a_phase_shifted_high_frequency_tone() {
        let mut limiter = limiter(48_000, 1);
        let input = (0..4_800)
            .map(|frame| {
                (1.5 * (std::f64::consts::TAU * 0.25 * frame as f64 + std::f64::consts::FRAC_PI_4)
                    .sin()) as f32
            })
            .collect::<Vec<_>>();
        let (mut output, first_metrics) = limiter.process_interleaved(&input).unwrap();
        let (tail, tail_metrics) = limiter.process_interleaved(&vec![0.0; 300]).unwrap();
        output.extend(tail);

        let mut analyzer = super::super::diagnostics::AudioSignalAnalyzer::new(
            super::super::diagnostics::AudioDiagnosticsConfig {
                discontinuity_threshold_q15: 0,
            },
        );
        let measured = analyzer.observe_f32_frame(&output, 48_000, 1).unwrap();
        let ceiling_with_tolerance =
            (10.0_f32.powf((limiter.config().ceiling_dbfs + 0.2) / 20.0) * 32_768.0).ceil() as u32;

        assert!(
            measured.true_peak_q15 <= ceiling_with_tolerance,
            "true peak={} Q15; allowed <= {} Q15",
            measured.true_peak_q15,
            ceiling_with_tolerance
        );
        assert!(first_metrics.attenuated_samples + tail_metrics.attenuated_samples > 0);
        assert_eq!(
            first_metrics.safety_clamped_samples + tail_metrics.safety_clamped_samples,
            0
        );
    }

    #[test]
    fn true_peak_limiter_respects_ceiling_across_common_device_formats() {
        for sample_rate_hz in [44_100, 48_000, 96_000] {
            for channels in [1, 2] {
                let mut limiter = limiter(sample_rate_hz, channels);
                let input = (0..4_096)
                    .flat_map(|frame| {
                        let wave = (std::f64::consts::TAU * 0.25 * frame as f64
                            + std::f64::consts::FRAC_PI_4)
                            .sin() as f32;
                        [Some(1.5 * wave), (channels == 2).then_some(0.6 * wave)]
                            .into_iter()
                            .flatten()
                    })
                    .collect::<Vec<_>>();
                let (mut output, first_metrics) = limiter.process_interleaved(&input).unwrap();
                let tail_frames = first_metrics.lookahead_frames as usize + 16;
                let (tail, tail_metrics) = limiter
                    .process_interleaved(&vec![0.0; tail_frames * usize::from(channels)])
                    .unwrap();
                output.extend(tail);

                let expected_lookahead_frames =
                    ((limiter.config().lookahead_ms * sample_rate_hz as f32 / 1_000.0).ceil()
                        as u32)
                        .max(super::super::diagnostics::TRUE_PEAK_GROUP_DELAY_FRAMES as u32);
                assert_eq!(first_metrics.lookahead_frames, expected_lookahead_frames);
                assert!(first_metrics.attenuated_samples > 0);
                assert_eq!(
                    first_metrics.safety_clamped_samples + tail_metrics.safety_clamped_samples,
                    0,
                    "sample_rate={sample_rate_hz} channels={channels}"
                );

                let mut analyzer = super::super::diagnostics::AudioSignalAnalyzer::new(
                    super::super::diagnostics::AudioDiagnosticsConfig {
                        discontinuity_threshold_q15: 0,
                    },
                );
                let measured = analyzer
                    .observe_f32_frame(&output, sample_rate_hz, channels)
                    .unwrap();
                let ceiling_with_tolerance =
                    (10.0_f32.powf((limiter.config().ceiling_dbfs + 0.2) / 20.0) * 32_768.0).ceil()
                        as u32;
                assert!(
                    measured.true_peak_q15 <= ceiling_with_tolerance,
                    "sample_rate={sample_rate_hz} channels={channels} true_peak={} ceiling={ceiling_with_tolerance}",
                    measured.true_peak_q15
                );

                if channels == 2 {
                    for frame in output.as_chunks::<2>().0 {
                        assert!(
                            (frame[1] - frame[0] * 0.4).abs() < 1.0e-6,
                            "stereo gain mismatch at {sample_rate_hz} Hz: {frame:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn steady_over_ceiling_signal_is_limited_and_release_is_smooth() {
        let mut limiter = limiter(8_000, 1);
        let input = [2.0; 320].into_iter().chain([0.0; 320]).collect::<Vec<_>>();

        let (output, metrics) = limiter.process_interleaved(&input).unwrap();
        let ceiling = 10.0_f32.powf(-1.0 / 20.0);
        assert!(output.iter().all(|sample| sample.abs() <= ceiling));
        assert!(metrics.attenuated_samples > 100);
        assert_eq!(metrics.safety_clamped_samples, 0);
        assert_eq!(output.last(), Some(&0.0));
    }

    #[test]
    fn config_validation_rejects_non_finite_and_attack_longer_than_lookahead() {
        assert_eq!(
            LimiterConfig {
                ceiling_dbfs: f32::NAN,
                ..LimiterConfig::default()
            }
            .validate(),
            Err(LimiterConfigError::NotFinite)
        );
        assert_eq!(
            LimiterConfig {
                attack_ms: 4.0,
                lookahead_ms: 3.0,
                ..LimiterConfig::default()
            }
            .validate(),
            Err(LimiterConfigError::AttackExceedsLookahead)
        );
    }

    #[test]
    fn limiter_rejects_non_finite_input() {
        let mut limiter = limiter(48_000, 1);
        assert_eq!(
            limiter.process_interleaved(&[f32::INFINITY]),
            Err(LimiterError::NonFiniteSample)
        );
    }

    #[test]
    fn limiter_output_is_independent_of_input_block_boundaries() {
        let mut input = vec![0.0; 2_048 * 2];
        for frame in 300..520 {
            input[frame * 2] = 0.45;
            input[frame * 2 + 1] = -0.45;
        }
        input[400 * 2] = 2.0;
        input[400 * 2 + 1] = -2.0;

        let mut whole = limiter(48_000, 2);
        let (whole_output, whole_metrics) = whole.process_interleaved(&input).unwrap();

        let mut chunked = limiter(48_000, 2);
        let mut chunked_output = Vec::new();
        let mut chunked_metrics = LimiterFrameMetrics::default();
        for block in input.chunks(128) {
            let (output, metrics) = chunked.process_interleaved(block).unwrap();
            chunked_output.extend(output);
            chunked_metrics.lookahead_frames = metrics.lookahead_frames;
            chunked_metrics.attenuated_samples += metrics.attenuated_samples;
            chunked_metrics.safety_clamped_samples += metrics.safety_clamped_samples;
            chunked_metrics.max_safety_overshoot_ppb = chunked_metrics
                .max_safety_overshoot_ppb
                .max(metrics.max_safety_overshoot_ppb);
            chunked_metrics.max_gain_reduction_millidb = chunked_metrics
                .max_gain_reduction_millidb
                .max(metrics.max_gain_reduction_millidb);
        }

        assert_eq!(chunked_output, whole_output);
        assert_eq!(chunked_metrics, whole_metrics);
    }

    #[test]
    fn varying_over_ceiling_signal_does_not_need_safety_clamps() {
        let mut state = 0x7F4A_7C15_u32;
        let mut input = Vec::with_capacity(32_768 * 2);
        for _ in 0..32_768 {
            let mut next_sample = || {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let unit = (state >> 8) as f32 / (u32::MAX >> 8) as f32;
                (unit * 2.0 - 1.0) * 2.5
            };
            input.push(next_sample());
            input.push(next_sample());
        }

        let mut limiter = limiter(48_000, 2);
        let (_, metrics) = limiter.process_interleaved(&input).unwrap();

        assert_eq!(
            metrics.safety_clamped_samples, 0,
            "max overshoot was {} ppb",
            metrics.max_safety_overshoot_ppb
        );
    }
}
