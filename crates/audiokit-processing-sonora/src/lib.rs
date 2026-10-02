//! Persistent, synchronous Sonora AEC3/NS/AGC2 backend, without host or SDK state.
//!
//! One worker owns each instance and serializes capture, render and delay updates.
//! Calls accept one 10 ms quantum, independent of Opus ptime. Construction allocates
//! planar and legacy i16 scratch buffers. Sonora's own internal allocation behavior
//! is not guaranteed callback-safe; do not call this backend from device callbacks.

mod options;
pub use options::{AudioProcessingOptions, NoiseSuppressionMode};
pub use sonora::AudioProcessingStats;

use audiokit::backend::{VoiceProcessingStats, VoiceProcessor};
use audiokit::{AudioError, AudioFormat, AudioResult, SampleFrames};
use sonora::config::{
    AdaptiveDigital, EchoCanceller, GainController2, HighPassFilter, MaxProcessingRate,
    NoiseSuppression, NoiseSuppressionLevel, Pipeline,
};
use sonora::{AudioProcessing, Config, StreamConfig};

/// Largest supported reference-to-capture delay in milliseconds.
pub const MAX_STREAM_DELAY_MS: u32 = 500;

/// Backend owned by one voice session; unrelated streams must never share history.
#[derive(Debug)]
pub struct SonoraProcessor {
    capture_format: AudioFormat,
    render_format: AudioFormat,
    apm: AudioProcessing,
    echo_cancellation: bool,
    float_bypass: bool,
    capture_i16: Vec<i16>,
    render_i16: Vec<i16>,
    capture_input: Vec<f32>,
    capture_output: Vec<f32>,
    render_input: Vec<f32>,
    render_output: Vec<f32>,
}

impl SonoraProcessor {
    /// Builds separately configured capture and reference paths with no packetizer.
    ///
    /// Native rates are 8/16/32/48 kHz, mono/stereo. Other device rates must be
    /// resampled by the graph. AEC only works when aligned references are supplied.
    pub fn new(
        capture_format: AudioFormat,
        render_format: AudioFormat,
        enable_echo_cancellation: bool,
        options: AudioProcessingOptions,
    ) -> AudioResult<Self> {
        let capture_samples = quantum_samples(capture_format)?;
        let render_samples = quantum_samples(render_format)?;
        let config = Config {
            pipeline: Pipeline {
                maximum_internal_processing_rate: if capture_format.sample_rate_hz() > 32_000 {
                    MaxProcessingRate::Rate48kHz
                } else {
                    MaxProcessingRate::Rate32kHz
                },
                ..Pipeline::default()
            },
            high_pass_filter: options.high_pass_filter.then(HighPassFilter::default),
            noise_suppression: match options.noise_suppression {
                NoiseSuppressionMode::Off => None,
                NoiseSuppressionMode::Low => Some(NoiseSuppression {
                    level: NoiseSuppressionLevel::Low,
                    ..NoiseSuppression::default()
                }),
                NoiseSuppressionMode::Moderate => Some(NoiseSuppression::default()),
                NoiseSuppressionMode::High => Some(NoiseSuppression {
                    level: NoiseSuppressionLevel::High,
                    ..NoiseSuppression::default()
                }),
                NoiseSuppressionMode::VeryHigh => Some(NoiseSuppression {
                    level: NoiseSuppressionLevel::VeryHigh,
                    ..NoiseSuppression::default()
                }),
            },
            gain_controller2: gain_controller2_config(options),
            echo_canceller: enable_echo_cancellation.then(EchoCanceller::default),
            ..Config::default()
        };
        let apm = AudioProcessing::builder()
            .config(config)
            .capture_config(stream_config(capture_format))
            .render_config(stream_config(render_format))
            .echo_detector(enable_echo_cancellation)
            .build();
        Ok(Self {
            capture_format,
            render_format,
            apm,
            echo_cancellation: enable_echo_cancellation,
            float_bypass: !enable_echo_cancellation
                && !options.high_pass_filter
                && options.noise_suppression == NoiseSuppressionMode::Off
                && !options.gain_controller2,
            capture_i16: vec![0; capture_samples],
            render_i16: vec![0; render_samples],
            capture_input: vec![0.0; capture_samples],
            capture_output: vec![0.0; capture_samples],
            render_input: vec![0.0; render_samples],
            render_output: vec![0.0; render_samples],
        })
    }

    /// Returns whether this backend expects AEC references.
    pub const fn echo_cancellation_enabled(&self) -> bool {
        self.echo_cancellation
    }

    /// Legacy signed-16 capture entry point, preserving the accepted client's conversion path.
    /// Accepts exactly 10 ms, and leaves the caller's samples unchanged on validation failure.
    pub fn process_capture_i16_10ms(&mut self, pcm: &mut [i16]) -> AudioResult<()> {
        validate_length(pcm.len(), self.capture_i16.len())?;
        self.capture_i16.copy_from_slice(pcm);
        self.apm
            .process_capture_i16(&self.capture_i16, pcm)
            .map_err(sonora_error)
    }

    /// Legacy signed-16 reference entry point; analyzes but never modifies playback PCM.
    pub fn analyze_render_i16_10ms(&mut self, pcm: &[i16]) -> AudioResult<()> {
        validate_length(pcm.len(), self.render_i16.len())?;
        if !self.echo_cancellation {
            return Ok(());
        }
        self.apm
            .process_render_i16(pcm, &mut self.render_i16)
            .map_err(sonora_error)
    }

    /// Returns complete native statistics for legacy diagnostic report compatibility.
    pub fn native_statistics(&self) -> AudioProcessingStats {
        self.apm.statistics().clone()
    }
}

impl VoiceProcessor for SonoraProcessor {
    fn capture_format(&self) -> AudioFormat {
        self.capture_format
    }
    fn render_format(&self) -> AudioFormat {
        self.render_format
    }
    fn process_capture(&mut self, pcm: &mut [f32]) -> AudioResult<()> {
        validate_float(pcm, self.capture_input.len())?;
        // Sonora can internally resample even with every module disabled. A
        // requested raw baseline must not acquire that filter delay or coloration.
        if self.float_bypass {
            return Ok(());
        }
        let channels = usize::from(self.capture_format.channels());
        deinterleave(pcm, &mut self.capture_input, channels);
        process_planar(
            &mut self.apm,
            &self.capture_input,
            &mut self.capture_output,
            channels,
            true,
        )?;
        interleave(&self.capture_output, pcm, channels);
        Ok(())
    }
    fn analyze_render(&mut self, pcm: &[f32]) -> AudioResult<()> {
        validate_float(pcm, self.render_input.len())?;
        if !self.echo_cancellation {
            return Ok(());
        }
        let channels = usize::from(self.render_format.channels());
        deinterleave(pcm, &mut self.render_input, channels);
        process_planar(
            &mut self.apm,
            &self.render_input,
            &mut self.render_output,
            channels,
            false,
        )
    }
    fn set_delay_ms(&mut self, delay_ms: u32) -> AudioResult<()> {
        if delay_ms > MAX_STREAM_DELAY_MS {
            return Err(AudioError::InvalidConfig(
                "Sonora delay must be 0..=500 ms".into(),
            ));
        }
        self.apm
            .set_stream_delay_ms(delay_ms as i32)
            .map_err(sonora_error)
    }
    fn statistics(&self) -> VoiceProcessingStats {
        let stats = self.apm.statistics();
        VoiceProcessingStats {
            echo_return_loss_db: stats.echo_return_loss,
            echo_return_loss_enhancement_db: stats.echo_return_loss_enhancement,
            delay_ms: stats.delay_ms,
            residual_echo_likelihood: stats.residual_echo_likelihood,
        }
    }
    fn algorithmic_delay(&self) -> Option<SampleFrames> {
        self.float_bypass.then_some(SampleFrames::new(0))
    }
}

fn quantum_samples(format: AudioFormat) -> AudioResult<usize> {
    if ![8_000, 16_000, 32_000, 48_000].contains(&format.sample_rate_hz()) || format.channels() > 2
    {
        return Err(AudioError::Unsupported(
            "Sonora requires native 8/16/32/48 kHz mono/stereo PCM".into(),
        ));
    }
    Ok(format.sample_rate_hz() as usize / 100 * usize::from(format.channels()))
}
fn validate_length(actual: usize, expected: usize) -> AudioResult<()> {
    if actual != expected {
        return Err(AudioError::InvalidFrame(format!(
            "Sonora 10 ms quantum expected {expected} samples, got {actual}"
        )));
    }
    Ok(())
}
fn validate_float(pcm: &[f32], expected: usize) -> AudioResult<()> {
    validate_length(pcm.len(), expected)?;
    if pcm.iter().any(|s| !s.is_finite() || s.abs() > 1.0) {
        return Err(AudioError::InvalidFrame(
            "Sonora expects finite PCM in -1..=1".into(),
        ));
    }
    Ok(())
}
fn stream_config(format: AudioFormat) -> StreamConfig {
    StreamConfig::new(format.sample_rate_hz(), u16::from(format.channels()))
}
fn gain_controller2_config(options: AudioProcessingOptions) -> Option<GainController2> {
    options.gain_controller2.then(|| GainController2 {
        adaptive_digital: options.adaptive_gain.then(AdaptiveDigital::default),
        ..GainController2::default()
    })
}

fn sonora_error(error: sonora::Error) -> AudioError {
    AudioError::Processing(format!("Sonora processing failed: {error}"))
}

fn deinterleave(input: &[f32], output: &mut [f32], channels: usize) {
    let frames = input.len() / channels;
    for (i, sample) in input.iter().enumerate() {
        output[(i % channels) * frames + i / channels] = *sample;
    }
}
fn interleave(input: &[f32], output: &mut [f32], channels: usize) {
    let frames = output.len() / channels;
    for (i, sample) in output.iter_mut().enumerate() {
        *sample = input[(i % channels) * frames + i / channels];
    }
}
fn process_planar(
    apm: &mut AudioProcessing,
    input: &[f32],
    output: &mut [f32],
    channels: usize,
    capture: bool,
) -> AudioResult<()> {
    // Fixed-size references avoid a per-quantum Vec allocation for supported layouts.
    if channels == 1 {
        if capture {
            apm.process_capture_f32(&[input], &mut [output])
        } else {
            apm.process_render_f32(&[input], &mut [output])
        }
    } else {
        let (left, right) = input.split_at(input.len() / 2);
        let midpoint = output.len() / 2;
        let (out_left, out_right) = output.split_at_mut(midpoint);
        if capture {
            apm.process_capture_f32(&[left, right], &mut [out_left, out_right])
        } else {
            apm.process_render_f32(&[left, right], &mut [out_left, out_right])
        }
    }
    .map_err(sonora_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_gain_is_opt_in_and_uses_sonora_defaults() {
        let defaults = gain_controller2_config(AudioProcessingOptions::default()).unwrap();
        assert!(defaults.adaptive_digital.is_none());
        let enabled = gain_controller2_config(AudioProcessingOptions {
            adaptive_gain: true,
            ..AudioProcessingOptions::default()
        })
        .unwrap();
        assert_eq!(enabled.adaptive_digital, Some(AdaptiveDigital::default()));
        let legacy = serde_json::json!({ "high_pass_filter": true, "noise_suppression": "moderate", "gain_controller2": true });
        let decoded: AudioProcessingOptions = serde_json::from_value(legacy).unwrap();
        assert!(!decoded.adaptive_gain);
    }
}
