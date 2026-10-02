//! Fixed-ratio, anti-aliased PCM conversion for device and codec boundaries.

use crate::AudioError as MediaError;
use audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, Fft, FixedAsync, FixedSync, Resampler, SincInterpolationParameters, Slip,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::resample_format::ResampleBlockFormat as PcmFormat;

/// Selects the quality/latency tradeoff for fixed-duration PCM conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResamplerQuality {
    /// Rubato's low-delay FFT partitioning, retained as the compatibility default.
    Balanced,
    /// A 256-tap windowed-sinc filter for higher-quality anti-alias conversion.
    HighQuality,
}

/// User-adjustable resampler profile and maximum permitted filter group delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResamplerConfig {
    /// Selects the anti-alias filter block size and quality/latency tradeoff.
    pub quality: ResamplerQuality,
    /// Maximum resampler filter group delay, in milliseconds, for a device stream.
    pub max_delay_ms: u16,
    /// Maximum playback queue drift correction, in parts per million; zero disables correction.
    #[serde(default = "default_max_clock_drift_correction_ppm")]
    pub max_clock_drift_correction_ppm: u16,
}

const fn default_max_clock_drift_correction_ppm() -> u16 {
    500
}

impl Default for ResamplerConfig {
    fn default() -> Self {
        Self {
            quality: ResamplerQuality::Balanced,
            max_delay_ms: 12,
            max_clock_drift_correction_ppm: 500,
        }
    }
}

impl ResamplerConfig {
    /// Validates the rate-independent portion of the resampler configuration.
    pub fn validate(self) -> Result<Self, ResamplerConfigError> {
        if !(1..=20).contains(&self.max_delay_ms) {
            return Err(ResamplerConfigError::DelayOutOfRange);
        }
        if self.max_clock_drift_correction_ppm > 1_000 {
            return Err(ResamplerConfigError::DriftCorrectionOutOfRange);
        }
        Ok(self)
    }
}

/// Invalid user-supplied resampler settings.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum ResamplerConfigError {
    /// The permitted filter delay is outside the supported range.
    #[error("resampler max_delay_ms must be between 1 and 20")]
    DelayOutOfRange,
    /// The maximum clock drift correction exceeds the supported range.
    #[error("resampler max_clock_drift_correction_ppm must be 0..=1000")]
    DriftCorrectionOutOfRange,
}

/// Stateful, fixed-duration PCM sample-rate converter.
///
/// The converter preserves channel layout and packet duration. It is intended
/// for audio worker tasks, not CPAL callbacks. Rubato's synchronous FFT
/// resampler is used because the device/codec rates are fixed for a stream;
/// its anti-alias filter and delay are retained across consecutive frames.
pub struct PcmResampler {
    input_format: PcmFormat,
    output_format: PcmFormat,
    resampler: Option<ResamplerEngine>,
    config: ResamplerConfig,
    input_buffer: Vec<f32>,
    output_buffer: Vec<f32>,
    output_delay_frames: usize,
}

enum ResamplerEngine {
    Fft(Box<Fft<f32>>),
    Sinc(Box<Async<f32>>),
}

impl ResamplerEngine {
    fn input_frames_next(&self) -> usize {
        match self {
            Self::Fft(resampler) => resampler.input_frames_next(),
            Self::Sinc(resampler) => resampler.input_frames_next(),
        }
    }

    fn output_frames_next(&self) -> usize {
        match self {
            Self::Fft(resampler) => resampler.output_frames_next(),
            Self::Sinc(resampler) => resampler.output_frames_next(),
        }
    }

    fn output_frames_max(&self) -> usize {
        match self {
            Self::Fft(resampler) => resampler.output_frames_max(),
            Self::Sinc(resampler) => resampler.output_frames_max(),
        }
    }

    fn output_delay(&self) -> usize {
        match self {
            Self::Fft(resampler) => resampler.output_delay(),
            Self::Sinc(resampler) => resampler.output_delay(),
        }
    }

    fn process_into_buffer(
        &mut self,
        input: &InterleavedSlice<&[f32]>,
        output: &mut InterleavedSlice<&mut [f32]>,
    ) -> Result<(usize, usize), rubato::ResampleError> {
        match self {
            Self::Fft(resampler) => resampler.process_into_buffer(input, output, None),
            Self::Sinc(resampler) => resampler.process_into_buffer(input, output, None),
        }
    }
}

impl PcmResampler {
    /// Creates a converter between formats with matching channel count and packet duration.
    pub fn new(input_format: PcmFormat, output_format: PcmFormat) -> Result<Self, MediaError> {
        Self::new_with_config(input_format, output_format, ResamplerConfig::default())
    }

    /// Creates a converter using the selected quality profile and delay budget.
    pub fn new_with_config(
        input_format: PcmFormat,
        output_format: PcmFormat,
        config: ResamplerConfig,
    ) -> Result<Self, MediaError> {
        let input_format = input_format.validate()?;
        let output_format = output_format.validate()?;
        let config = config.validate().map_err(|error| {
            MediaError::InvalidFrame(format!("invalid resampler configuration: {error}"))
        })?;
        if input_format.channels != output_format.channels
            || input_format.ptime_ms != output_format.ptime_ms
        {
            return Err(MediaError::InvalidFrame(
                "resampling requires matching channels and packet duration".to_owned(),
            ));
        }

        if input_format.sample_rate == output_format.sample_rate {
            return Ok(Self {
                input_format,
                output_format,
                resampler: None,
                config,
                input_buffer: Vec::new(),
                output_buffer: Vec::new(),
                output_delay_frames: 0,
            });
        }

        let input_rate = input_format.sample_rate as usize;
        let output_rate = output_format.sample_rate as usize;
        let channels = usize::from(input_format.channels);
        let resampler = match config.quality {
            ResamplerQuality::Balanced => Fft::<f32>::new(
                input_rate,
                output_rate,
                input_format.frame_samples,
                channels,
                FixedSync::Both,
            )
            .map(|resampler| ResamplerEngine::Fft(Box::new(resampler))),
            ResamplerQuality::HighQuality => Async::<f32>::new_sinc(
                output_rate as f64 / input_rate as f64,
                1.0,
                &SincInterpolationParameters {
                    f_cutoff: Some(0.99),
                    ..SincInterpolationParameters::default()
                },
                input_format.frame_samples,
                channels,
                FixedAsync::Input,
            )
            .map(|resampler| ResamplerEngine::Sinc(Box::new(resampler))),
        }
        .map_err(|error| {
            MediaError::Processing(format!("sample-rate converter setup failed: {error}"))
        })?;
        if resampler.input_frames_next() != input_format.frame_samples
            || resampler.output_frames_next() > output_format.frame_samples
        {
            return Err(MediaError::InvalidFrame(format!(
                "sample-rate ratio does not preserve the configured packet duration (input {} vs {}, initial output {} exceeds {})",
                resampler.input_frames_next(),
                input_format.frame_samples,
                resampler.output_frames_next(),
                output_format.frame_samples
            )));
        }

        let output_delay_frames = resampler.output_delay();
        let delay_budget_frames =
            u128::from(config.max_delay_ms) * u128::from(output_format.sample_rate) / 1_000;
        if output_delay_frames as u128 > delay_budget_frames {
            return Err(MediaError::InvalidFrame(format!(
                "resampler {:?} group delay of {:.2} ms exceeds max_delay_ms={} for {} Hz output",
                config.quality,
                output_delay_frames as f64 * 1_000.0 / f64::from(output_format.sample_rate),
                config.max_delay_ms,
                output_format.sample_rate
            )));
        }
        let input_buffer = vec![0.0; input_format.interleaved_samples()];
        let output_buffer = vec![0.0; resampler.output_frames_max() * channels];
        Ok(Self {
            input_format,
            output_format,
            resampler: Some(resampler),
            config,
            input_buffer,
            output_buffer,
            output_delay_frames,
        })
    }

    /// Converts one complete interleaved PCM packet and preserves converter history.
    pub fn process_i16(&mut self, input: &[i16]) -> Result<Vec<i16>, MediaError> {
        if self.resampler.is_none() && input.len() == self.input_format.interleaved_samples() {
            return Ok(input.to_vec());
        }
        let normalized = input
            .iter()
            .map(|s| f32::from(*s) / 32_768.0)
            .collect::<Vec<_>>();
        let floating = self.process_f32(&normalized)?;
        let mut output = floating
            .into_iter()
            .map(|sample| {
                (sample * 32_768.0)
                    .round()
                    .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
            })
            .collect::<Vec<_>>();
        // Explicit legacy compatibility only. ContinuousResampler never pads individual blocks.
        output.resize(self.output_format.interleaved_samples(), 0);
        Ok(output)
    }

    /// Converts one worker block to its natural variable-length f32 output.
    /// Preserves over-full-scale PCM; no quantization, packet padding or clipping.
    /// Allocation is worker-only. Hosts join output via a frame FIFO, not resize().
    pub fn process_f32(&mut self, input: &[f32]) -> Result<Vec<f32>, MediaError> {
        if input.len() != self.input_format.interleaved_samples() {
            return Err(MediaError::InvalidFrame(format!(
                "resampler input has {} samples, expected {}",
                input.len(),
                self.input_format.interleaved_samples()
            )));
        }
        if !input.iter().all(|s| s.is_finite()) {
            return Err(MediaError::InvalidFrame(
                "non-finite resampler input".into(),
            ));
        }
        let Some(resampler) = self.resampler.as_mut() else {
            return Ok(input.to_vec());
        };
        self.input_buffer.copy_from_slice(input);
        let channels = usize::from(self.input_format.channels);
        let output_frames = resampler.output_frames_next();
        if output_frames > self.output_format.frame_samples {
            return Err(MediaError::Processing(format!(
                "resampler produced a {output_frames}-frame block, expected at most {}",
                self.output_format.frame_samples
            )));
        }
        let (_, written_frames) = {
            let input = InterleavedSlice::new(
                &self.input_buffer,
                channels,
                self.input_format.frame_samples,
            )
            .map_err(|error| {
                MediaError::Processing(format!("invalid resampler input adapter: {error}"))
            })?;
            let mut output = InterleavedSlice::new_mut(
                &mut self.output_buffer[..output_frames * channels],
                channels,
                output_frames,
            )
            .map_err(|error| {
                MediaError::Processing(format!("invalid resampler output adapter: {error}"))
            })?;
            resampler
                .process_into_buffer(&input, &mut output)
                .map_err(|error| {
                    MediaError::Processing(format!("sample-rate conversion failed: {error}"))
                })?
        };
        if written_frames != output_frames {
            return Err(MediaError::Processing(format!(
                "resampler produced {written_frames} frames, expected {output_frames}"
            )));
        }

        Ok(self.output_buffer[..written_frames * channels].to_vec())
    }

    /// Returns the filter's startup/group delay in output sample frames.
    pub fn output_delay_frames(&self) -> usize {
        self.output_delay_frames
    }

    /// Returns the source PCM format accepted by this converter.
    pub fn input_format(&self) -> PcmFormat {
        self.input_format
    }

    /// Returns the destination PCM format produced by this converter.
    pub fn output_format(&self) -> PcmFormat {
        self.output_format
    }

    /// Returns the quality and delay-budget settings used by this converter.
    pub fn config(&self) -> ResamplerConfig {
        self.config
    }
}

/// Smooths tiny sample-clock mismatches using queue-watermark feedback.
///
/// The ratio is updated once per media block. Rubato's slip resampler spreads an occasional
/// one-sample insertion or removal over a crossfade, avoiding a hard discontinuity while keeping
/// the correction bounded to the clock-drift range rather than changing the nominal stream rate.
/// Queue-watermark controller shared by native and future platform playback backends.
pub struct QueueDriftController {
    controller: crate::clock::QueueClockController,
    sample_rate_hz: u32,
    frame_samples: usize,
    channels: f64,
}

impl QueueDriftController {
    /// Creates a controller with a target queue watermark in interleaved samples.
    pub fn new(
        format: PcmFormat,
        target_samples: usize,
        max_correction_ppm: u16,
    ) -> Result<Self, MediaError> {
        if format.sample_rate == 0
            || format.channels == 0
            || format.frame_samples == 0
            || target_samples == 0
            || max_correction_ppm > 1_000
        {
            return Err(MediaError::InvalidFrame(
                "invalid queue drift controller format, target, or correction limit".to_owned(),
            ));
        }
        let channels = f64::from(format.channels);
        let target_ms = target_samples as f64 / channels * 1000.0 / f64::from(format.sample_rate);
        Ok(Self {
            controller: crate::clock::QueueClockController::new(crate::clock::QueueClockConfig {
                target_ms,
                max_ppm: max_correction_ppm,
                ..Default::default()
            })?,
            sample_rate_hz: format.sample_rate,
            frame_samples: format.frame_samples,
            channels,
        })
    }

    /// Returns positive ppm when the projected post-enqueue queue is below its target.
    pub fn correction_ppm(&mut self, projected_samples: usize) -> (i32, bool) {
        let block_ms = self.frame_samples as f64 * 1000.0 / f64::from(self.sample_rate_hz);
        let queue_ms =
            projected_samples as f64 / self.channels * 1000.0 / f64::from(self.sample_rate_hz);
        // Legacy caller has no independent device-demand clock yet. Large bursts are
        // a recovery disturbance, never evidence for a hardware ratio correction.
        let state = if (queue_ms - self.controller.config().target_ms).abs() > 100.0 {
            crate::clock::QueueClockState::Recovering
        } else {
            crate::clock::QueueClockState::Running
        };
        let update = self
            .controller
            .update(queue_ms, block_ms, state)
            .unwrap_or_default();
        (update.correction_ppm, update.saturated)
    }

    /// Clears the filtered watermark on startup, rebuffering, overflow or epoch changes.
    pub fn reset(&mut self) {
        self.controller.reset();
    }
}

/// Variable-length device-rate PCM correction driven by a [`QueueDriftController`].
/// Applies queue-controller clock corrections to fixed-size blocks without hard sample cuts.
pub struct PcmClockDriftCorrector {
    format: PcmFormat,
    resampler: Slip<f32>,
    input_buffer: Vec<f32>,
    output_buffer: Vec<f32>,
    max_correction_ppm: u16,
}

impl PcmClockDriftCorrector {
    /// Creates a corrector for one device-rate PCM block format and a 0..=1000 ppm cap.
    pub fn new(format: PcmFormat, max_correction_ppm: u16) -> Result<Self, MediaError> {
        let format = format.validate()?;
        if max_correction_ppm > 1_000 {
            return Err(MediaError::InvalidFrame(
                "maximum clock drift correction must be 0..=1000 ppm".to_owned(),
            ));
        }
        let resampler = Slip::<f32>::new(
            format.frame_samples,
            usize::from(format.channels),
            FixedAsync::Input,
        )
        .map_err(|error| {
            MediaError::Processing(format!("clock drift corrector setup failed: {error}"))
        })?;
        let input_buffer = vec![0.0; format.interleaved_samples()];
        let output_buffer = vec![0.0; resampler.output_frames_max() * usize::from(format.channels)];
        Ok(Self {
            format,
            resampler,
            input_buffer,
            output_buffer,
            max_correction_ppm,
        })
    }

    /// Applies the bounded ratio and returns one input block's variable-length output.
    /// Corrects one interleaved block and returns a variable-length output block.
    pub fn process_i16(
        &mut self,
        input: &[i16],
        requested_correction_ppm: i32,
    ) -> Result<Vec<i16>, MediaError> {
        let input = input
            .iter()
            .map(|s| f32::from(*s) / 32768.0)
            .collect::<Vec<_>>();
        Ok(self
            .process_f32(&input, requested_correction_ppm)?
            .into_iter()
            .map(|s| {
                (s * 32768.0)
                    .round()
                    .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
            })
            .collect())
    }
    /// Applies bounded linked-channel Slip to one f32 quantum without clipping/quantization.
    /// Variable output length is retained by the downstream source FIFO; worker-only allocation.
    pub fn process_f32(
        &mut self,
        input: &[f32],
        requested_correction_ppm: i32,
    ) -> Result<Vec<f32>, MediaError> {
        if input.len() != self.format.interleaved_samples() {
            return Err(MediaError::InvalidFrame(format!(
                "clock drift input has {} samples, expected {}",
                input.len(),
                self.format.interleaved_samples()
            )));
        }
        let limit = i32::from(self.max_correction_ppm);
        if !input.iter().all(|s| s.is_finite()) {
            return Err(MediaError::InvalidFrame(
                "non-finite clock correction PCM".into(),
            ));
        }
        let correction_ppm = requested_correction_ppm.clamp(-limit, limit);
        self.resampler
            .set_resample_ratio(1.0 + f64::from(correction_ppm) / 1_000_000.0, false)
            .map_err(|error| {
                MediaError::Processing(format!("clock drift ratio update failed: {error}"))
            })?;

        self.input_buffer.copy_from_slice(input);
        let channels = usize::from(self.format.channels);
        let input = InterleavedSlice::new(&self.input_buffer, channels, self.format.frame_samples)
            .map_err(|error| {
                MediaError::Processing(format!("invalid clock drift input: {error}"))
            })?;
        let output_frames = self.resampler.output_frames_next();
        let mut output = InterleavedSlice::new_mut(
            &mut self.output_buffer[..output_frames * channels],
            channels,
            output_frames,
        )
        .map_err(|error| MediaError::Processing(format!("invalid clock drift output: {error}")))?;
        let (_, written_frames) = self
            .resampler
            .process_into_buffer(&input, &mut output, None)
            .map_err(|error| {
                MediaError::Processing(format!("clock drift correction failed: {error}"))
            })?;
        if written_frames != output_frames {
            return Err(MediaError::Processing(format!(
                "clock drift corrector produced {written_frames} frames, expected {output_frames}"
            )));
        }
        Ok(self.output_buffer[..written_frames * channels].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{AudioDiagnosticsConfig, AudioSignalAnalyzer};
    use crate::limiter::{LimiterConfig, MasterLimiter};

    fn format(sample_rate: u32, channels: u8) -> PcmFormat {
        PcmFormat::new(sample_rate, channels, 20).unwrap()
    }

    #[test]
    fn queue_drift_controller_converges_for_fast_and_slow_device_clocks() {
        let format = format(48_000, 1);
        let target_samples = format.interleaved_samples() * 4 + format.interleaved_samples() / 2;
        for device_drift_ppm in [-150_i32, 150_i32] {
            let mut controller = QueueDriftController::new(format, target_samples, 500).unwrap();
            let block_samples = format.interleaved_samples() as f64;
            let mut queue_samples = target_samples as f64 - block_samples;
            let mut correction_ppm = 0;
            for _ in 0..6_000 {
                let projected_samples = queue_samples + block_samples;
                (correction_ppm, _) = controller.correction_ppm(projected_samples.round() as usize);
                let produced = f64::from(format.frame_samples as u32)
                    * (1.0 + f64::from(correction_ppm) / 1_000_000.0);
                let consumed = f64::from(format.frame_samples as u32)
                    * (1.0 + f64::from(device_drift_ppm) / 1_000_000.0);
                queue_samples += produced - consumed;
            }
            assert!(
                (correction_ppm - device_drift_ppm).abs() <= 8,
                "device drift={device_drift_ppm} ppm, applied={correction_ppm} ppm"
            );
            assert!(
                (queue_samples + block_samples - target_samples as f64).abs() < 500.0,
                "projected queue drifted to {} samples from target {target_samples}",
                queue_samples + block_samples
            );
        }
    }

    #[test]
    fn queue_feedback_and_sample_slip_keep_a_bounded_queue_for_fast_and_slow_clocks() {
        let format = format(48_000, 1);
        let block_samples = format.interleaved_samples();
        let target_samples = block_samples * 4 + block_samples / 2;
        let capacity_samples = block_samples * 16;
        let silence = vec![0; block_samples];

        for device_drift_ppm in [-150_i32, 150_i32] {
            let mut controller = QueueDriftController::new(format, target_samples, 500).unwrap();
            let mut corrector = PcmClockDriftCorrector::new(format, 500).unwrap();
            let mut queued_samples = (target_samples - block_samples) as f64;
            let mut correction_ppm = 0;
            for _ in 0..6_000 {
                let projected_samples = queued_samples + block_samples as f64;
                (correction_ppm, _) = controller.correction_ppm(projected_samples.round() as usize);
                let output = corrector.process_i16(&silence, correction_ppm).unwrap();
                let consumed_samples =
                    block_samples as f64 * (1.0 + f64::from(device_drift_ppm) / 1_000_000.0);
                queued_samples += output.len() as f64 - consumed_samples;
                assert!(queued_samples >= 0.0);
                assert!(queued_samples < capacity_samples as f64);
            }

            assert!(
                (correction_ppm - device_drift_ppm).abs() <= 8,
                "device drift={device_drift_ppm} ppm, applied={correction_ppm} ppm"
            );
            assert!((queued_samples + block_samples as f64 - target_samples as f64).abs() < 500.0);
        }
    }

    #[test]
    fn queue_drift_controller_reports_when_the_configured_limit_is_insufficient() {
        let format = format(48_000, 1);
        let mut controller = QueueDriftController::new(format, 1_000, 100).unwrap();
        let mut correction = 0;
        let mut saturated = false;
        for _ in 0..500 {
            (correction, saturated) = controller.correction_ppm(0);
        }

        assert_eq!(correction, 100);
        assert!(saturated);
    }

    #[test]
    fn queue_drift_controller_gain_is_independent_of_channel_count() {
        let mono = format(48_000, 1);
        let stereo = format(48_000, 2);
        let mono_target = mono.interleaved_samples() * 4;
        let stereo_target = stereo.interleaved_samples() * 4;
        let mut mono_controller = QueueDriftController::new(mono, mono_target, 500).unwrap();
        let mut stereo_controller = QueueDriftController::new(stereo, stereo_target, 500).unwrap();

        assert_eq!(
            mono_controller.correction_ppm(mono_target - 200),
            stereo_controller.correction_ppm(stereo_target - 400)
        );
    }

    #[test]
    fn drift_correction_config_defaults_for_older_json_and_bounds_the_ppm_limit() {
        let older_config: ResamplerConfig = serde_json::from_value(serde_json::json!({
            "quality": "balanced",
            "max_delay_ms": 12
        }))
        .unwrap();
        assert_eq!(older_config, ResamplerConfig::default());
        assert!(
            ResamplerConfig {
                max_clock_drift_correction_ppm: 1_001,
                ..ResamplerConfig::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ResamplerConfig {
                max_clock_drift_correction_ppm: 0,
                ..ResamplerConfig::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn drift_corrector_is_identity_at_zero_and_keeps_stereo_linked() {
        let pcm_format = format(48_000, 2);
        let mut corrector = PcmClockDriftCorrector::new(pcm_format, 500).unwrap();
        let mut input = Vec::with_capacity(pcm_format.interleaved_samples());
        for frame in 0..pcm_format.frame_samples {
            let sample = (12_000.0_f64
                * (std::f64::consts::TAU * 997.0 * frame as f64 / 48_000.0).sin())
                as i16;
            input.extend([sample, sample]);
        }
        assert_eq!(corrector.process_i16(&input, 0).unwrap(), input);

        let mut output_frames = 0;
        for _ in 0..250 {
            let output = corrector.process_i16(&input, 500).unwrap();
            assert_eq!(output.len() % 2, 0);
            assert!(
                output
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .all(|frame| frame[0] == frame[1])
            );
            assert!(output.iter().all(|sample| sample.unsigned_abs() <= 12_000));
            output_frames += output.len() / 2;
        }
        assert!(
            (pcm_format.frame_samples * 250 + 110..=pcm_format.frame_samples * 250 + 130)
                .contains(&output_frames)
        );

        let mut slow_clock = PcmClockDriftCorrector::new(pcm_format, 500).unwrap();
        let mut slow_output_frames = 0;
        for _ in 0..250 {
            slow_output_frames += slow_clock.process_i16(&input, -500).unwrap().len() / 2;
        }
        assert!(
            (pcm_format.frame_samples * 250 - 130..=pcm_format.frame_samples * 250 - 110)
                .contains(&slow_output_frames)
        );
    }

    #[test]
    fn converts_common_rates_with_exact_packet_duration_and_channel_linking() {
        for (input_rate, output_rate) in [(44_100, 48_000), (48_000, 44_100), (96_000, 48_000)] {
            for channels in [1, 2] {
                let input_format = format(input_rate, channels);
                let output_format = format(output_rate, channels);
                let mut converter = PcmResampler::new(input_format, output_format).unwrap();
                let mut output = Vec::new();
                for packet in 0..12 {
                    let samples = (0..input_format.frame_samples)
                        .flat_map(|frame| {
                            let phase = std::f64::consts::TAU
                                * 440.0
                                * (packet * input_format.frame_samples + frame) as f64
                                / f64::from(input_rate);
                            let sample = (phase.sin() * 12_000.0).round() as i16;
                            std::iter::repeat_n(sample, usize::from(channels))
                        })
                        .collect::<Vec<_>>();
                    let converted = converter.process_i16(&samples).unwrap();
                    assert_eq!(converted.len(), output_format.interleaved_samples());
                    if channels == 2 {
                        assert!(
                            converted
                                .as_chunks::<2>()
                                .0
                                .iter()
                                .all(|frame| frame[0] == frame[1])
                        );
                    }
                    output.extend(converted);
                }

                let steady = &output[output_format.interleaved_samples() * 2..];
                let rms = (steady
                    .iter()
                    .map(|sample| f64::from(*sample).powi(2))
                    .sum::<f64>()
                    / steady.len() as f64)
                    .sqrt();
                assert!((7_500.0..=9_500.0).contains(&rms));
                assert!(converter.output_delay_frames() > 0);
            }
        }
    }

    #[test]
    fn downsampling_rejects_tones_above_the_destination_nyquist_limit() {
        let input_format = format(48_000, 1);
        let output_format = format(44_100, 1);
        let mut converter = PcmResampler::new(input_format, output_format).unwrap();
        let mut output = Vec::new();

        for packet in 0..24 {
            let samples = (0..input_format.frame_samples)
                .map(|frame| {
                    let position = packet * input_format.frame_samples + frame;
                    (0.5 * (std::f64::consts::TAU * 23_900.0 * position as f64 / 48_000.0).sin()
                        * 32_768.0) as i16
                })
                .collect::<Vec<_>>();
            output.extend(converter.process_i16(&samples).unwrap());
        }

        let steady =
            &output[output_format.frame_samples * 4..output.len() - output_format.frame_samples];
        let rms = (steady
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / steady.len() as f64)
            .sqrt();
        assert!(rms < 80.0, "aliased RMS was {rms}");
    }

    #[test]
    fn resampled_overdrive_is_bounded_by_the_device_rate_true_peak_limiter() {
        let input_format = format(44_100, 2);
        let output_format = format(48_000, 2);
        let mut converter = PcmResampler::new(input_format, output_format).unwrap();
        let mut limiter = MasterLimiter::new(
            output_format.sample_rate,
            output_format.channels,
            LimiterConfig::default(),
        )
        .unwrap();
        let mut rendered = Vec::new();
        let mut attenuated_samples = 0;
        let mut safety_clamped_samples = 0;

        for packet in 0..32 {
            let input = (0..input_format.frame_samples)
                .flat_map(|frame| {
                    let position = packet * input_format.frame_samples + frame;
                    let sample = (12_000.0
                        * (std::f64::consts::TAU * 997.0 * position as f64
                            / f64::from(input_format.sample_rate))
                        .sin()) as i16;
                    [sample, sample]
                })
                .collect::<Vec<_>>();
            let converted = converter.process_i16(&input).unwrap();
            let overdriven = converted
                .into_iter()
                .map(|sample| f32::from(sample) / 32_768.0 * 3.5)
                .collect::<Vec<_>>();
            let (limited, metrics) = limiter.process_interleaved(&overdriven).unwrap();
            rendered.extend(limited);
            attenuated_samples += metrics.attenuated_samples;
            safety_clamped_samples += metrics.safety_clamped_samples;
        }

        let tail_frames = limiter.config().lookahead_ms.ceil() as usize
            * output_format.sample_rate as usize
            / 1_000
            + 16;
        let (tail, tail_metrics) = limiter
            .process_interleaved(&vec![
                0.0;
                tail_frames * usize::from(output_format.channels)
            ])
            .unwrap();
        rendered.extend(tail);
        safety_clamped_samples += tail_metrics.safety_clamped_samples;

        let mut analyzer = AudioSignalAnalyzer::new(AudioDiagnosticsConfig {
            discontinuity_threshold_q15: 0,
        });
        let measured = analyzer
            .observe_f32_frame(&rendered, output_format.sample_rate, output_format.channels)
            .unwrap();
        let allowed_true_peak =
            (10.0_f32.powf((limiter.config().ceiling_dbfs + 0.2) / 20.0) * 32_768.0).ceil() as u32;

        assert!(attenuated_samples > 0);
        assert_eq!(safety_clamped_samples, 0);
        assert!(measured.true_peak_q15 <= allowed_true_peak);
    }

    #[test]
    fn same_rate_conversion_is_bit_exact_and_has_no_delay() {
        let pcm_format = format(48_000, 2);
        let mut converter = PcmResampler::new(pcm_format, pcm_format).unwrap();
        let samples = (0..pcm_format.interleaved_samples())
            .map(|index| (index as i16).wrapping_mul(37))
            .collect::<Vec<_>>();

        assert_eq!(converter.process_i16(&samples).unwrap(), samples);
        assert_eq!(converter.output_delay_frames(), 0);
    }

    #[test]
    fn high_quality_profile_respects_delay_budget_and_preserves_packet_sizes() {
        for (input_rate, output_rate) in [(44_100, 48_000), (48_000, 44_100), (96_000, 48_000)] {
            let input_format = format(input_rate, 2);
            let output_format = format(output_rate, 2);
            let config = ResamplerConfig {
                quality: ResamplerQuality::HighQuality,
                ..ResamplerConfig::default()
            };
            let mut converter =
                PcmResampler::new_with_config(input_format, output_format, config).unwrap();
            let silence = vec![0; input_format.interleaved_samples()];
            for _ in 0..8 {
                assert_eq!(
                    converter.process_i16(&silence).unwrap().len(),
                    output_format.interleaved_samples()
                );
            }
            let delay_ms =
                converter.output_delay_frames() as f64 * 1_000.0 / f64::from(output_rate);
            assert!(delay_ms <= f64::from(config.max_delay_ms));
        }
    }

    #[test]
    fn high_quality_profile_rejects_a_delay_budget_smaller_than_its_filter_delay() {
        let config = ResamplerConfig {
            quality: ResamplerQuality::HighQuality,
            max_delay_ms: 1,
            ..ResamplerConfig::default()
        };
        let error =
            match PcmResampler::new_with_config(format(44_100, 1), format(48_000, 1), config) {
                Ok(_) => panic!("the configured delay budget must reject this profile"),
                Err(error) => error,
            };

        assert!(error.to_string().contains("exceeds max_delay_ms=1"));
    }

    #[test]
    fn high_quality_profile_preserves_upper_passband_and_rejects_stopband_tones() {
        let measure_rms = |quality: ResamplerQuality, tone_hz: f64| {
            let input_format = format(48_000, 1);
            let output_format = format(44_100, 1);
            let mut converter = PcmResampler::new_with_config(
                input_format,
                output_format,
                ResamplerConfig {
                    quality,
                    ..ResamplerConfig::default()
                },
            )
            .unwrap();
            let mut input = Vec::new();
            let mut output = Vec::new();
            for frame_index in 0..32 * input_format.frame_samples {
                let sample = (12_000.0_f64
                    * (std::f64::consts::TAU * tone_hz * frame_index as f64 / 48_000.0_f64).sin())
                    as i16;
                input.push(sample);
                if input.len() == input_format.frame_samples {
                    output.extend(converter.process_i16(&input).unwrap());
                    input.clear();
                }
            }
            let steady = &output_format.frame_samples * 4;
            let samples = &output[steady..output.len() - output_format.frame_samples];
            (samples
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>()
                / samples.len() as f64)
                .sqrt()
        };

        let balanced_passband = measure_rms(ResamplerQuality::Balanced, 20_000.0);
        let high_quality_passband = measure_rms(ResamplerQuality::HighQuality, 20_000.0);
        let balanced_transition = measure_rms(ResamplerQuality::Balanced, 21_800.0);
        let high_quality_transition = measure_rms(ResamplerQuality::HighQuality, 21_800.0);
        let high_quality_stopband = measure_rms(ResamplerQuality::HighQuality, 23_900.0);

        assert!((high_quality_passband / balanced_passband - 1.0).abs() < 0.01);
        assert!(
            high_quality_transition > balanced_transition * 3.0,
            "high-quality transition RMS {high_quality_transition} did not exceed balanced RMS {balanced_transition}"
        );
        assert!(
            high_quality_stopband < 1.0,
            "stopband RMS was {high_quality_stopband}"
        );
    }
}
