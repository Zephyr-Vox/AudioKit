//! Native PCM -> channel map -> continuous resample -> optional 10 ms voice DSP -> packetizer/codec.
use super::GraphState;
use super::capture_pcm::{CapturePcmConfig, CapturePcmGraph};
use crate::backend::{AudioEncoder, VoiceProcessor};
use crate::resample::ResamplerConfig;
use crate::{AudioError, AudioFormat, AudioResult, StreamKind};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, time::Instant};

/// Capture graph's host-neutral controls. Encoder/processor effective configs stay with backends.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureGraphConfig {
    /// Native source rate/layout; callback lengths need not equal ptime.
    pub input_format: AudioFormat,
    /// Voice requires mono encoding, desktop stereo; desktop bypasses voice enhancement.
    pub kind: StreamKind,
    /// Continuous worker filter profile and delay cap.
    pub resampler: ResamplerConfig,
    /// Per-call ingress budget in milliseconds, 20..=200.
    pub max_ingress_ms: u16,
    /// Full encoded packet output capacity, 1..=4000 bytes.
    pub max_payload_bytes: usize,
}

/// PCM/packet accounting and measured worker execution time; no artifact I/O is included.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CaptureGraphStats {
    /// Original per-channel native input frames.
    pub input_frames: u64,
    /// Frames encoded after rate/channel conversion, including explicit EOF padding.
    pub encoded_frames: u64,
    /// Encoded packet count.
    pub packets: u64,
    /// Zero frames to complete an APM quantum at EOF.
    pub processing_eof_padding_frames: u64,
    /// Zero frames to complete a codec packet at EOF.
    pub packet_eof_padding_frames: u64,
    /// Total processor execution nanoseconds; missing when bypassed.
    pub processing_execution_ns: Option<u64>,
    /// Total encoder execution nanoseconds, not codec algorithmic delay.
    pub encoding_execution_ns: u64,
    /// Output-clock resampler filter delay.
    pub resampler_delay_frames: u64,
    /// Backend-reported processing delay; None is unknown/bypassed, not zero.
    pub processing_delay_frames: Option<u64>,
    /// Encoder lookahead in encoder sample frames, if reported.
    pub encoder_lookahead_frames: Option<u64>,
}

/// One complete fixed-ptime outbound packet and its pre-codec PCM.
/// The host decides whether to retain/export PCM under explicit recording consent.
pub struct CapturePacket {
    /// Complete backend payload; no protocol headers or private timestamps are injected.
    pub payload: Vec<u8>,
    /// Normalized pre-codec PCM, mono for voice and stereo for desktop.
    pub pcm: Vec<f32>,
    /// First encoded frame's per-channel output cursor.
    pub sample_position: u64,
}

/// Exclusive synchronous capture pipeline. Hosts own device polling, permissions and network tasks.
pub struct CaptureGraph {
    config: CaptureGraphConfig,
    encoder: Box<dyn AudioEncoder>,
    frontend: CapturePcmGraph,
    packet_fifo: VecDeque<f32>,
    packet_samples: usize,
    packet_buffer: Vec<u8>,
    stats: CaptureGraphStats,
    state: GraphState,
}
impl CaptureGraph {
    /// Builds one production graph from independently owned backend instances.
    /// Every shape/config is checked before any PCM is admitted; desktop rejects a processor.
    pub fn new(
        config: CaptureGraphConfig,
        encoder: Box<dyn AudioEncoder>,
        processor: Option<Box<dyn VoiceProcessor>>,
    ) -> AudioResult<Self> {
        config
            .resampler
            .validate()
            .map_err(|e| AudioError::InvalidConfig(e.to_string()))?;
        if !(20..=200).contains(&config.max_ingress_ms)
            || !(1..=4000).contains(&config.max_payload_bytes)
        {
            return Err(AudioError::InvalidConfig(
                "invalid capture ingress/payload budget".into(),
            ));
        }
        let format = encoder.format();
        match config.kind {
            StreamKind::Voice if format.channels() != 1 => {
                return Err(AudioError::InvalidConfig(
                    "voice encoder must be mono".into(),
                ));
            }
            StreamKind::Desktop if format.channels() != 2 || processor.is_some() => {
                return Err(AudioError::InvalidConfig(
                    "desktop must be stereo and bypass voice processing".into(),
                ));
            }
            _ => {}
        }
        if processor
            .as_ref()
            .is_some_and(|p| p.capture_format() != format)
        {
            return Err(AudioError::InvalidConfig(
                "processor capture format must match encoder".into(),
            ));
        }
        let frontend = CapturePcmGraph::new(
            CapturePcmConfig {
                input_format: config.input_format,
                output_format: format,
                kind: config.kind,
                resampler: config.resampler,
                max_ingress_ms: config.max_ingress_ms,
            },
            processor,
        )?;
        let packet_samples = encoder
            .packet_duration()
            .frames(format)?
            .interleaved_samples(format)?;
        let frontend_stats = frontend.statistics();
        let stats = CaptureGraphStats {
            processing_execution_ns: frontend_stats.processing_execution_ns,
            processing_delay_frames: frontend_stats.processing_delay_frames,
            encoder_lookahead_frames: encoder.lookahead().map(|f| f.get()),
            resampler_delay_frames: frontend_stats.resampler_delay_frames,
            ..Default::default()
        };
        Ok(Self {
            config,
            encoder,
            frontend,
            packet_fifo: VecDeque::new(),
            packet_samples,
            packet_buffer: vec![0; config.max_payload_bytes],
            stats,
            state: GraphState::Running,
        })
    }
    /// Returns the actual requested graph controls; backend configuration is never duplicated here.
    pub fn config(&self) -> CaptureGraphConfig {
        self.config
    }
    /// Returns lifecycle state. Reconstruction is required for a new device/stream epoch.
    pub fn state(&self) -> GraphState {
        self.state
    }
    /// Returns cumulative signal/execution/delay accounting without PCM or credentials.
    pub fn statistics(&self) -> CaptureGraphStats {
        let mut stats = self.stats;
        let frontend = self.frontend.statistics();
        stats.input_frames = frontend.input_frames;
        stats.processing_eof_padding_frames = frontend.processing_eof_padding_frames;
        stats.processing_execution_ns = frontend.processing_execution_ns;
        stats
    }
    /// Admits a bounded arbitrary native block, preserving callback remainders in FIFOs.
    pub fn push_native(&mut self, pcm: &[f32]) -> AudioResult<Vec<CapturePacket>> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        let converted = match self.frontend.push_native(pcm) {
            Ok(pcm) => pcm,
            Err(error) => {
                // Validation rejects untouched input; only an advanced/failed
                // frontend is terminal. Preserve the production recovery contract.
                if self.frontend.state() == GraphState::Stopped {
                    self.abort();
                }
                return Err(error);
            }
        };
        let result = self.accept_converted(&converted);
        // A failing backend may have advanced history. Never resume a half-consumed graph.
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn accept_converted(&mut self, pcm: &[f32]) -> AudioResult<Vec<CapturePacket>> {
        self.packet_fifo.extend(pcm.iter().copied());
        let mut result = Vec::new();
        while self.packet_fifo.len() >= self.packet_samples {
            let pcm = self
                .packet_fifo
                .drain(..self.packet_samples)
                .collect::<Vec<_>>();
            let started = Instant::now();
            let len = self.encoder.encode_into(&pcm, &mut self.packet_buffer)?;
            self.stats.encoding_execution_ns = self
                .stats
                .encoding_execution_ns
                .saturating_add(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
            if len == 0 || len > self.packet_buffer.len() {
                return Err(AudioError::Processing(
                    "invalid encoded packet length".into(),
                ));
            }
            result.push(CapturePacket {
                payload: self.packet_buffer[..len].to_vec(),
                pcm,
                sample_position: self.stats.encoded_frames,
            });
            self.stats.encoded_frames +=
                (self.packet_samples / usize::from(self.encoder.format().channels())) as u64;
            self.stats.packets += 1;
        }
        Ok(result)
    }
    /// Analyzes an aligned 10 ms actual-playback reference on the same graph owner.
    /// A host without a valid reference must keep AEC disabled; no synthetic reference is invented.
    pub fn analyze_render(&mut self, pcm: &[f32], delay_ms: u32) -> AudioResult<()> {
        if self.state != GraphState::Running {
            return Err(AudioError::Cancelled);
        }
        self.frontend.analyze_render(pcm, delay_ms)
    }
    /// Drains known resampler/FIFO tails once, recording APM/codec EOF padding separately.
    /// Unknown processor latency remains unknown; this does not certify a backend's internal tail.
    pub fn finish(&mut self) -> AudioResult<Vec<CapturePacket>> {
        if self.state == GraphState::Stopped {
            return Ok(Vec::new());
        }
        let result = self.finish_inner();
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn finish_inner(&mut self) -> AudioResult<Vec<CapturePacket>> {
        self.state = GraphState::Draining;
        let tail = self.frontend.finish()?;
        let mut result = self.accept_converted(&tail)?;
        if !self.packet_fifo.is_empty() {
            let missing = self.packet_samples - self.packet_fifo.len();
            self.stats.packet_eof_padding_frames +=
                (missing / usize::from(self.encoder.format().channels())) as u64;
            self.packet_fifo.extend(std::iter::repeat_n(0.0, missing));
            result.extend(self.accept_converted(&[])?);
        }
        self.state = GraphState::Stopped;
        Ok(result)
    }
    /// Immediately discards pending FIFO tails. Idempotent; no background tasks are detached.
    pub fn abort(&mut self) {
        self.frontend.abort();
        self.packet_fifo.clear();
        self.state = GraphState::Stopped;
    }
}
