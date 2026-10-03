//! Shared serializable controls and production subchain validator.
use crate::{Error, Result};
use audiokit::graph::receive::ReceiveGraphConfig;
use audiokit::resample::ResamplerConfig;
use audiokit::{AudioFormat, ChannelLayout, PacketDuration, StreamKind};
use serde::{Deserialize, Serialize};

/// Implemented offline scenarios, distinct from hardware/server E2E.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    /// Capture PCM frontend only, without a codec or packet padding.
    FileProcessing,
    /// Capture/Opus/receive/render with a paced virtual transport.
    FileRoundtrip,
}
/// Backend-neutral suppression levels.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseLevel {
    /// Explicit suppression bypass.
    Off,
    /// Least aggressive suppression.
    Low,
    /// Accepted voice baseline.
    #[default]
    Moderate,
    /// Stronger suppression.
    High,
    /// Maximum suppression; speech coloration must be evaluated.
    VeryHigh,
}
/// Sonora settings shared by frontends; file scenarios cannot supply real AEC reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProcessingConfig {
    /// Whether the production voice processor runs; false bypasses all its nodes.
    pub enabled: bool,
    /// Noise suppression aggressiveness.
    pub noise_suppression: NoiseLevel,
    /// Enables high-pass filtering.
    pub high_pass_filter: bool,
    /// Enables AGC2, including its own limiter.
    pub gain_controller2: bool,
    /// Enables adaptive digital gain; default false.
    pub adaptive_gain: bool,
    /// Requires an actual aligned playback reference; currently rejected for files.
    pub aec: bool,
}
impl Default for ProcessingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            noise_suppression: Default::default(),
            high_pass_filter: true,
            gain_controller2: true,
            adaptive_gain: false,
            aec: false,
        }
    }
}
/// Run controls. Local input paths are supplied separately, not serialized into shared bundles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunConfig {
    /// Version 1; unknown versions are rejected before output creation.
    pub schema_version: u32,
    /// Legal production graph selection.
    pub scenario: Scenario,
    /// Voice mono or desktop stereo; other stream kinds are not supported.
    pub stream: StreamKind,
    /// Optional voice processor controls.
    pub processing: ProcessingConfig,
    /// Production capture resampler controls.
    pub resampler: ResamplerConfig,
    /// Fixed session packet duration; not used by the PCM-only subchain.
    pub ptime: PacketDuration,
    /// Exact target bit/s: voice 64000..=128000, desktop 128000..=320000.
    pub bitrate_bps: u32,
    /// Complete Opus payload capacity, 1..=4000 bytes.
    pub max_payload_bytes: usize,
    /// Production receive/render controls, applied only to roundtrip.
    pub receive: ReceiveGraphConfig,
    /// Deterministic virtual forwarding; rejected for the PCM-only scenario if enabled.
    pub transport: crate::TransportConfig,
    /// Maximum input WAV bytes, 1..=268435456.
    pub max_input_bytes: u64,
    /// Maximum interleaved decoded input or output samples, each 1..=67108864.
    pub max_pcm_samples: usize,
    /// Maximum retained events, 1..=1000000; additional events are counted as lost.
    pub max_trace_events: usize,
    /// Maximum serialized trace payload bytes, 1024..=8388608; pretty JSON adds framing.
    pub max_trace_bytes: usize,
    /// Explicit authorization to include the original input audio in the bundle.
    pub retain_input: bool,
}
impl Default for RunConfig {
    fn default() -> Self {
        Self {
            schema_version: 1,
            scenario: Scenario::FileProcessing,
            stream: StreamKind::Voice,
            processing: Default::default(),
            resampler: Default::default(),
            ptime: PacketDuration::Ms20,
            bitrate_bps: 96_000,
            max_payload_bytes: 4000,
            receive: Default::default(),
            transport: Default::default(),
            max_input_bytes: 64 * 1024 * 1024,
            max_pcm_samples: 16 * 1024 * 1024,
            max_trace_events: 4096,
            max_trace_bytes: 8 * 1024 * 1024,
            retain_input: false,
        }
    }
}
/// Node coverage, not an assertion of successful execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Production implementation is called.
    Applied,
    /// A configurable node is explicitly disabled.
    Bypassed,
    /// Scenario never enters this node.
    NotCovered,
}
/// Stable stage identifier and declared coverage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    /// Stable node name used by traces and frontends.
    pub id: String,
    /// Planned execution status.
    pub status: NodeStatus,
}
/// Validated production graph selection with explicit boundary formats.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlan {
    /// Selected file scenario.
    pub scenario: Scenario,
    /// capture-subchain or virtual-roundtrip; never server E2E.
    pub coverage: String,
    /// Actual WAV input format.
    pub input_format: AudioFormat,
    /// Capture/codec format, 48 kHz mono voice or stereo desktop.
    pub capture_format: AudioFormat,
    /// Final WAV format.
    pub output_format: AudioFormat,
    /// Ordered nodes and explicit bypass/not-covered status.
    pub stages: Vec<Stage>,
}
impl RunConfig {
    /// Validates values and compiled capabilities without performing I/O.
    pub fn validate(&self) -> Result<()> {
        self.validate_parameters()?;
        if self.processing.enabled && !cfg!(feature = "processing-sonora") {
            return Err(Error::Capability("processing-sonora".into()));
        }
        if self.scenario == Scenario::FileRoundtrip && !cfg!(feature = "codec-opus") {
            return Err(Error::Capability("codec-opus".into()));
        }
        Ok(())
    }
    pub(crate) fn validate_parameters(&self) -> Result<()> {
        self.transport.validate()?;
        if self.scenario == Scenario::FileProcessing && self.transport.is_impaired() {
            return Err(Error::Invalid(
                "PCM-only scenario has no encoded transport".into(),
            ));
        }
        if self.schema_version != 1
            || !(1..=268_435_456).contains(&self.max_input_bytes)
            || !(1..=67_108_864).contains(&self.max_pcm_samples)
            || !(1..=1_000_000).contains(&self.max_trace_events)
            || !(1024..=8_388_608).contains(&self.max_trace_bytes)
            || !(1..=4000).contains(&self.max_payload_bytes)
        {
            return Err(Error::Invalid(
                "schema or resource budget out of range".into(),
            ));
        }
        self.resampler
            .validate()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        self.receive
            .validate()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if self.processing.aec {
            return Err(Error::Invalid(
                "file scenarios have no actual render reference; AEC is unavailable".into(),
            ));
        }
        if self.processing.adaptive_gain && !self.processing.gain_controller2 {
            return Err(Error::Invalid("adaptive gain requires AGC2".into()));
        }
        let bounds = match self.stream {
            StreamKind::Voice => 64_000..=128_000,
            StreamKind::Desktop => {
                if self.processing.enabled {
                    return Err(Error::Invalid(
                        "desktop must bypass voice processing".into(),
                    ));
                }
                128_000..=320_000
            }
            _ => {
                return Err(Error::Invalid(
                    "profiles support voice or desktop only".into(),
                ));
            }
        };
        if !bounds.contains(&self.bitrate_bps) {
            return Err(Error::Invalid("profile bitrate out of range".into()));
        }
        if self.scenario == Scenario::FileRoundtrip {
            self.receive
                .validate_stream(self.ptime)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            if u64::from(self.bitrate_bps) * u64::from(self.ptime.milliseconds())
                > self.max_payload_bytes as u64 * 8000
            {
                return Err(Error::Invalid(
                    "payload cannot hold requested bitrate; no silent quality reduction".into(),
                ));
            }
            if !self
                .receive
                .render
                .format
                .sample_rate_hz()
                .is_multiple_of(100)
                || self.receive.render.max_render_ms < 10
            {
                return Err(Error::Invalid(
                    "virtual callback needs an integral 10 ms quantum".into(),
                ));
            }
        }
        Ok(())
    }
    /// Generates one legal capture subchain or roundtrip; arbitrary reordering is not allowed.
    pub fn plan(&self, input: AudioFormat) -> Result<ExecutionPlan> {
        self.validate()?;
        self.plan_parameters(input)
    }
    pub(crate) fn plan_parameters(&self, input: AudioFormat) -> Result<ExecutionPlan> {
        self.validate_parameters()?;
        if !matches!(input.layout(), ChannelLayout::Mono | ChannelLayout::Stereo)
            || !(8000..=192_000).contains(&input.sample_rate_hz())
            || !input.sample_rate_hz().is_multiple_of(100)
        {
            return Err(Error::Invalid(
                "input must be mono/stereo with an integral 10 ms quantum".into(),
            ));
        }
        let capture = AudioFormat::new(
            48_000,
            if self.stream == StreamKind::Voice {
                ChannelLayout::Mono
            } else {
                ChannelLayout::Stereo
            },
        )?;
        let roundtrip = self.scenario == Scenario::FileRoundtrip;
        let mut stages = vec![
            Stage {
                id: "channel_map".into(),
                status: NodeStatus::Applied,
            },
            Stage {
                id: "capture_resample".into(),
                status: NodeStatus::Applied,
            },
            Stage {
                id: "voice_apm".into(),
                status: if self.processing.enabled {
                    NodeStatus::Applied
                } else {
                    NodeStatus::Bypassed
                },
            },
        ];
        for id in [
            "packetizer",
            "opus_encode",
            "virtual_transport",
            "encoded_jitter",
            "opus_decode",
            "source_resample",
            "source_gain",
            "source_limiter",
            "activity_mix",
            "master_limiter",
            "source_clock_correction",
        ] {
            let status = if !roundtrip {
                NodeStatus::NotCovered
            } else if id == "source_clock_correction"
                && self.receive.render.max_source_clock_correction_ppm == 0
            {
                NodeStatus::Bypassed
            } else {
                NodeStatus::Applied
            };
            stages.push(Stage {
                id: id.into(),
                status,
            });
        }
        for id in [
            "capture_device",
            "output_device",
            "server_transport",
            "aec_reference",
        ] {
            stages.push(Stage {
                id: id.into(),
                status: NodeStatus::NotCovered,
            });
        }
        Ok(ExecutionPlan {
            scenario: self.scenario,
            coverage: if roundtrip {
                "virtual-roundtrip"
            } else {
                "capture-subchain"
            }
            .into(),
            input_format: input,
            capture_format: capture,
            output_format: if roundtrip {
                self.receive.render.format
            } else {
                capture
            },
            stages,
        })
    }
}
