//! Production capture frontend, independently usable without a codec or packet padding.

use super::GraphState;
use crate::backend::VoiceProcessor;
use crate::resample::ResamplerConfig;
use crate::stream_resample::ContinuousResampler;
use crate::{AudioError, AudioFormat, AudioResult, PacketDuration, StreamKind};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, time::Instant};

/// Capture frontend controls, independent of the encoder's packet duration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturePcmConfig {
    /// Native PCM input format; only complete channel frames are admitted.
    pub input_format: AudioFormat,
    /// Processed PCM format: voice mono, desktop stereo.
    pub output_format: AudioFormat,
    /// Desktop processing cannot include a voice processor.
    pub kind: StreamKind,
    /// Production resampler configuration.
    pub resampler: ResamplerConfig,
    /// Maximum frames admitted per call in milliseconds, 20..=200.
    pub max_ingress_ms: u16,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::VoiceProcessingStats;
    use crate::{ChannelLayout, SampleFrames};
    struct Processor {
        invalid: bool,
    }
    fn format() -> AudioFormat {
        AudioFormat::new(48000, ChannelLayout::Mono).unwrap()
    }
    fn config() -> CapturePcmConfig {
        CapturePcmConfig {
            input_format: format(),
            output_format: format(),
            kind: StreamKind::Voice,
            resampler: Default::default(),
            max_ingress_ms: 200,
        }
    }
    impl VoiceProcessor for Processor {
        fn capture_format(&self) -> AudioFormat {
            format()
        }
        fn render_format(&self) -> AudioFormat {
            format()
        }
        fn process_capture(&mut self, pcm: &mut [f32]) -> AudioResult<()> {
            for sample in pcm {
                *sample = if self.invalid {
                    f32::NAN
                } else {
                    *sample * 0.5
                };
            }
            Ok(())
        }
        fn analyze_render(&mut self, _: &[f32]) -> AudioResult<()> {
            Ok(())
        }
        fn set_delay_ms(&mut self, _: u32) -> AudioResult<()> {
            Ok(())
        }
        fn statistics(&self) -> VoiceProcessingStats {
            Default::default()
        }
        fn algorithmic_delay(&self) -> Option<SampleFrames> {
            None
        }
    }
    #[test]
    fn bypass_has_no_packet_padding_and_finish_is_idempotent() {
        let mut graph = CapturePcmGraph::new(config(), None).unwrap();
        let input = vec![0.2; 1001];
        let mut output = graph.push_native(&input).unwrap();
        output.extend(graph.finish().unwrap());
        assert_eq!(output, input);
        assert!(graph.finish().unwrap().is_empty());
        assert_eq!(graph.statistics().output_frames, 1001);
        assert_eq!(graph.statistics().processing_eof_padding_frames, 0);
        assert!(graph.push_native(&input).is_err());
    }
    #[test]
    fn processor_eof_padding_is_separate_and_chunk_independent() {
        let process = |chunks: usize| {
            let mut graph =
                CapturePcmGraph::new(config(), Some(Box::new(Processor { invalid: false })))
                    .unwrap();
            let mut output = Vec::new();
            for block in vec![0.2; 1001].chunks(chunks) {
                output.extend(graph.push_native(block).unwrap());
            }
            output.extend(graph.finish().unwrap());
            (output, graph.statistics())
        };
        let (a, stats) = process(7);
        let (b, _) = process(1001);
        assert_eq!(a, b);
        assert_eq!(a.len(), 1440);
        assert_eq!(stats.processing_eof_padding_frames, 439);
        assert!(a[..1001].iter().all(|s| *s == 0.1));
        assert!(a[1001..].iter().all(|s| *s == 0.0));
    }
    #[test]
    fn invalid_input_is_recoverable_but_corrupt_processor_output_is_terminal() {
        let mut graph =
            CapturePcmGraph::new(config(), Some(Box::new(Processor { invalid: true }))).unwrap();
        assert!(graph.push_native(&[f32::NAN]).is_err());
        assert_eq!(graph.state(), GraphState::Running);
        assert!(graph.push_native(&[0.2; 480]).is_err());
        assert_eq!(graph.state(), GraphState::Stopped);
    }
}

/// Frontend accounting. Unknown processor latency is not reported as zero.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CapturePcmStats {
    /// Per-channel native input frames.
    pub input_frames: u64,
    /// Per-channel processed output including filter tail and APM EOF padding.
    pub output_frames: u64,
    /// Output-clock resampler group delay in frames.
    pub resampler_delay_frames: u64,
    /// Native-clock zeros used to drain the rate filter.
    pub resampler_eof_padding_frames: u64,
    /// Output-clock zeros completing the final 10 ms processor quantum.
    pub processing_eof_padding_frames: u64,
    /// Processor CPU time in nanoseconds, None when bypassed.
    pub processing_execution_ns: Option<u64>,
    /// Processor-reported output-clock delay, None if unavailable or bypassed.
    pub processing_delay_frames: Option<u64>,
}

/// Exclusive worker frontend shared by production capture and partial-chain tests.
///
/// Calls allocate worker buffers and are not device-callback-safe. Finish retains
/// filter latency and pads only an enabled processor quantum, never a codec packet.
pub struct CapturePcmGraph {
    config: CapturePcmConfig,
    processor: Option<Box<dyn VoiceProcessor>>,
    resampler: ContinuousResampler,
    fifo: VecDeque<f32>,
    quantum_samples: usize,
    stats: CapturePcmStats,
    state: GraphState,
}

impl CapturePcmGraph {
    /// Validates formats, stream semantics and processor capabilities before construction.
    pub fn new(
        config: CapturePcmConfig,
        processor: Option<Box<dyn VoiceProcessor>>,
    ) -> AudioResult<Self> {
        if !(20..=200).contains(&config.max_ingress_ms) {
            return Err(AudioError::InvalidConfig(
                "invalid capture ingress budget".into(),
            ));
        }
        match config.kind {
            StreamKind::Voice if config.output_format.channels() != 1 => {
                return Err(AudioError::InvalidConfig(
                    "voice frontend must be mono".into(),
                ));
            }
            StreamKind::Desktop if config.output_format.channels() != 2 || processor.is_some() => {
                return Err(AudioError::InvalidConfig(
                    "desktop must be stereo and bypass voice processing".into(),
                ));
            }
            _ => {}
        }
        if processor
            .as_ref()
            .is_some_and(|p| p.capture_format() != config.output_format)
        {
            return Err(AudioError::InvalidConfig(
                "processor capture format must match output".into(),
            ));
        }
        let mapped = AudioFormat::new(
            config.input_format.sample_rate_hz(),
            config.output_format.layout(),
        )?;
        let resampler =
            ContinuousResampler::new(mapped, config.output_format, 10, config.resampler)?;
        let stats = CapturePcmStats {
            resampler_delay_frames: resampler.accounting().delay_frames as u64,
            processing_execution_ns: processor.as_ref().map(|_| 0),
            processing_delay_frames: processor
                .as_ref()
                .and_then(|p| p.algorithmic_delay())
                .map(|f| f.get()),
            ..Default::default()
        };
        let quantum_samples = PacketDuration::Ms10
            .frames(config.output_format)?
            .interleaved_samples(config.output_format)?;
        Ok(Self {
            config,
            processor,
            resampler,
            fifo: VecDeque::new(),
            quantum_samples,
            stats,
            state: GraphState::Running,
        })
    }
    /// Returns lifecycle state; a stopped graph cannot accept new PCM.
    pub fn state(&self) -> GraphState {
        self.state
    }
    /// Returns accumulated sample, padding and processing accounting.
    pub fn statistics(&self) -> CapturePcmStats {
        self.stats
    }
    /// Maps channels, resamples continuously and optionally processes complete 10 ms quanta.
    pub fn push_native(&mut self, pcm: &[f32]) -> AudioResult<Vec<f32>> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        let frames = self.config.input_format.frames_in(pcm.len())?.get();
        if frames * 1000
            > u64::from(self.config.input_format.sample_rate_hz())
                * u64::from(self.config.max_ingress_ms)
        {
            return Err(AudioError::ResourceExhausted(
                "capture ingress work budget exceeded".into(),
            ));
        }
        let mapped_format = AudioFormat::new(
            self.config.input_format.sample_rate_hz(),
            self.config.output_format.layout(),
        )?;
        let mapped = crate::channel::map_channels(self.config.input_format, mapped_format, pcm)?;
        self.stats.input_frames += frames;
        let chunk =
            mapped_format.sample_rate_hz() as usize / 10 * usize::from(mapped_format.channels());
        let result = (|| {
            let mut output = Vec::new();
            for input in mapped.chunks(chunk) {
                let converted = self.resampler.push(input)?;
                output.extend(self.accept(&converted)?);
            }
            Ok(output)
        })();
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn accept(&mut self, pcm: &[f32]) -> AudioResult<Vec<f32>> {
        let mut output = Vec::new();
        if let Some(processor) = &mut self.processor {
            self.fifo.extend(pcm.iter().copied());
            while self.fifo.len() >= self.quantum_samples {
                let mut quantum = self.fifo.drain(..self.quantum_samples).collect::<Vec<_>>();
                let started = Instant::now();
                processor.process_capture(&mut quantum)?;
                if !quantum.iter().all(|v| v.is_finite()) {
                    return Err(AudioError::Processing(
                        "processor returned non-finite PCM".into(),
                    ));
                }
                let elapsed = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                self.stats.processing_execution_ns = Some(
                    self.stats
                        .processing_execution_ns
                        .unwrap_or(0)
                        .saturating_add(elapsed),
                );
                output.extend(quantum);
            }
        } else {
            output.extend_from_slice(pcm);
        }
        self.stats.output_frames +=
            output.len() as u64 / u64::from(self.config.output_format.channels());
        Ok(output)
    }
    /// Supplies one aligned production playback reference; no synthetic reference is invented.
    pub fn analyze_render(&mut self, pcm: &[f32], delay_ms: u32) -> AudioResult<()> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        let processor = self
            .processor
            .as_mut()
            .ok_or_else(|| AudioError::Unsupported("capture processing is bypassed".into()))?;
        processor.set_delay_ms(delay_ms)?;
        processor.analyze_render(pcm)
    }
    /// Drains the filter and final processor quantum once, retaining explicit padding accounting.
    pub fn finish(&mut self) -> AudioResult<Vec<f32>> {
        if self.state == GraphState::Stopped {
            return Ok(Vec::new());
        }
        self.state = GraphState::Draining;
        let result = (|| {
            let tail = self.resampler.finish()?;
            self.stats.resampler_eof_padding_frames =
                self.resampler.accounting().eof_padding_frames;
            let mut output = self.accept(&tail)?;
            if !self.fifo.is_empty() {
                let missing = self.quantum_samples - self.fifo.len();
                self.stats.processing_eof_padding_frames +=
                    missing as u64 / u64::from(self.config.output_format.channels());
                output.extend(self.accept(&vec![0.0; missing])?);
            }
            Ok(output)
        })();
        self.state = GraphState::Stopped;
        if result.is_err() {
            self.abort();
        }
        result
    }
    /// Discards pending frontend PCM. Idempotent; no detached workers exist.
    pub fn abort(&mut self) {
        self.fifo.clear();
        self.state = GraphState::Stopped;
    }
}
