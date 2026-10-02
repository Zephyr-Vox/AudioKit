//! Continuous f32 conversion: callback sizes, filter blocks and encoded ptime are independent.
use crate::resample::{PcmResampler, ResamplerConfig};
use crate::resample_format::ResampleBlockFormat;
use crate::{AudioError, AudioFormat, AudioResult};
use std::collections::VecDeque;

/// Sample accounting, separating genuine PCM from EOF filter padding.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResampleAccounting {
    /// Original input sample frames, per channel.
    pub input_frames: u64,
    /// Natural/drained output sample frames, including explicit filter latency.
    pub output_frames: u64,
    /// Zero input frames used solely to complete/filter the end of stream.
    pub eof_padding_frames: u64,
    /// Output filter group delay retained at the start, in output frames.
    pub delay_frames: usize,
}

/// Worker-owned bounded input FIFO around the same production Rubato profiles.
/// No intermediate integer quantization or per-block zero insertion occurs.
pub struct ContinuousResampler {
    input: AudioFormat,
    output: AudioFormat,
    filter: PcmResampler,
    fifo: VecDeque<f32>,
    scratch: Vec<f32>,
    accounting: ResampleAccounting,
    finished: bool,
}
impl ContinuousResampler {
    /// Creates a converter with matching layouts and a 10/20/40/60 ms worker block.
    /// The worker block is independent of both callback length and encoder ptime.
    pub fn new(
        input: AudioFormat,
        output: AudioFormat,
        worker_ms: u16,
        config: ResamplerConfig,
    ) -> AudioResult<Self> {
        if input.layout() != output.layout() || !matches!(worker_ms, 10 | 20 | 40 | 60) {
            return Err(AudioError::InvalidConfig(
                "continuous resampling requires matching layouts and a supported worker block"
                    .into(),
            ));
        }
        let input_shape =
            ResampleBlockFormat::new(input.sample_rate_hz(), input.channels(), worker_ms)?;
        let output_shape =
            ResampleBlockFormat::new(output.sample_rate_hz(), output.channels(), worker_ms)?;
        let filter = PcmResampler::new_with_config(input_shape, output_shape, config)?;
        let accounting = ResampleAccounting {
            delay_frames: filter.output_delay_frames(),
            ..Default::default()
        };
        Ok(Self {
            input,
            output,
            filter,
            fifo: VecDeque::with_capacity(input_shape.interleaved_samples() * 2),
            scratch: vec![0.0; input_shape.interleaved_samples()],
            accounting,
            finished: false,
        })
    }
    /// Pushes at most 16 worker blocks of finite PCM and returns natural filter output.
    /// Incomplete worker blocks remain in the FIFO; channel-frame remainders are rejected.
    pub fn push(&mut self, pcm: &[f32]) -> AudioResult<Vec<f32>> {
        if self.finished {
            return Err(AudioError::Cancelled);
        }
        let frames = self.input.frames_in(pcm.len())?.get();
        if pcm.len() > self.scratch.len() * 16 {
            return Err(AudioError::ResourceExhausted(
                "resample ingress exceeds bounded work budget".into(),
            ));
        }
        if !pcm.iter().all(|v| v.is_finite()) {
            return Err(AudioError::InvalidFrame("non-finite resample input".into()));
        }
        let count = self
            .accounting
            .input_frames
            .checked_add(frames)
            .ok_or_else(|| {
                AudioError::ResourceExhausted("resample input cursor overflow".into())
            })?;
        self.fifo.extend(pcm.iter().copied());
        self.accounting.input_frames = count;
        let mut output = Vec::new();
        while self.fifo.len() >= self.scratch.len() {
            for sample in &mut self.scratch {
                *sample = self.fifo.pop_front().expect("complete block");
            }
            output.extend(self.filter.process_f32(&self.scratch)?);
        }
        self.accounting.output_frames += output.len() as u64 / u64::from(self.output.channels());
        Ok(output)
    }
    /// Drains filter history exactly once. Keeps ceil(input*ratio)+group-delay output frames.
    /// EOF padding is reported separately; subsequent calls return an empty tail.
    pub fn finish(&mut self) -> AudioResult<Vec<f32>> {
        if self.finished {
            return Ok(Vec::new());
        }
        let ratio_numerator =
            u128::from(self.accounting.input_frames) * u128::from(self.output.sample_rate_hz());
        let target = if self.accounting.input_frames == 0 {
            0
        } else {
            ratio_numerator.div_ceil(u128::from(self.input.sample_rate_hz()))
                + self.accounting.delay_frames as u128
        };
        let target = u64::try_from(target)
            .map_err(|_| AudioError::ResourceExhausted("resample output cursor overflow".into()))?;
        let missing = target.saturating_sub(self.accounting.output_frames);
        let samples = usize::try_from(missing)
            .ok()
            .and_then(|n| n.checked_mul(usize::from(self.output.channels())))
            .ok_or_else(|| AudioError::ResourceExhausted("resample tail overflow".into()))?;
        let mut output = Vec::with_capacity(samples);
        while output.len() < samples {
            let available = self.fifo.len().min(self.scratch.len());
            for (index, sample) in self.scratch.iter_mut().enumerate() {
                *sample = if index < available {
                    self.fifo.pop_front().expect("available sample")
                } else {
                    0.0
                };
            }
            self.accounting.eof_padding_frames +=
                (self.scratch.len() - available) as u64 / u64::from(self.input.channels());
            output.extend(self.filter.process_f32(&self.scratch)?);
        }
        output.truncate(samples);
        self.accounting.output_frames += missing;
        self.finished = true;
        self.fifo.clear();
        Ok(output)
    }
    /// Returns PCM/tail accounting without treating EOF padding as a device underrun.
    pub fn accounting(&self) -> ResampleAccounting {
        self.accounting
    }
    /// Input frames awaiting a complete worker block; excludes internal filter history.
    pub fn pending_input_frames(&self) -> u64 {
        self.fifo.len() as u64 / u64::from(self.input.channels())
    }
}
