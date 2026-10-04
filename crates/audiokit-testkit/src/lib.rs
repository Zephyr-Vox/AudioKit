//! Production-graph debugging scenarios and bounded, portable diagnostic bundles.
//!
//! Allocating worker APIs, not callback APIs. Initial file scenarios never open
//! devices, a display service, accounts or network connections.
//!
//! ```
//! use audiokit::{AudioFormat, ChannelLayout};
//! use audiokit_testkit::{ProcessingConfig, RunConfig};
//! let config = RunConfig {
//!     processing: ProcessingConfig { enabled: false, ..Default::default() },
//!     ..Default::default()
//! };
//! let plan = config.plan(AudioFormat::new(48_000, ChannelLayout::Stereo)?)?;
//! assert_eq!(plan.capture_format.channels(), 1);
//! assert_eq!(plan.coverage, "capture-subchain");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
mod bundle;
mod config;
mod io;
mod material_capture;
mod packet_trace;
mod report;
mod runner;
mod scheduler;
mod simulation;
mod sweep;
mod timing;
mod transport;
pub use bundle::{
    Analysis, Comparison, Evidence, analyze, compare, export_bundle, export_wav, inspect,
    read_processed_wav, replay,
};
pub use config::{
    ExecutionPlan, NodeStatus, NoiseLevel, ProcessingConfig, RunConfig, Scenario, Stage,
};
#[cfg(feature = "native-cpal")]
pub use material_capture::record_microphone;
pub use material_capture::{MaterialCaptureOptions, MaterialCaptureReport};
pub use packet_trace::{PacketSource, PacketTrace, ReceiveSimulationConfig, RecordedPacket};
pub use report::{Artifact, Check, Diagnostics, Manifest, ReplayOrigin, TraceEvent};
pub use runner::{Cancellation, ProgressEvent, ProgressUnit, run};
pub use scheduler::{PauseConfig, SchedulerConfig};
pub use simulation::{ClockConfig, MixStressConfig};
pub use sweep::{SweepCase, SweepMatrix, SweepReport, sweep};
pub use transport::TransportConfig;

/// Typed worker failure; frontends map variants to stable exit codes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid configuration, resource budget, schema or input.
    #[error("invalid input: {0}")]
    Invalid(String),
    /// WAV header exceeds the configured interleaved sample cap, before PCM allocation.
    #[error(
        "invalid input: decoded sample budget exceeded: WAV requires {required} interleaved samples, limit {limit} ({sample_rate_hz} Hz, {channels} channels); increase max_pcm_samples or use shorter input"
    )]
    DecodedSampleBudget {
        /// Total scalar samples across all channels, not per-channel frames.
        required: u64,
        /// Configured maximum decoded input or output samples.
        limit: usize,
        /// Actual WAV sample rate in Hz.
        sample_rate_hz: u32,
        /// Actual WAV channel count.
        channels: u16,
    },
    /// An optional compiled backend is missing.
    #[error("capability unavailable: {0}")]
    Capability(String),
    /// Runtime budget or bounded drain failure, not a malformed CLI argument.
    #[error("execution failed: {0}")]
    Execution(String),
    /// A production graph failed.
    #[error(transparent)]
    Audio(#[from] audiokit::AudioError),
    /// Filesystem error; portable diagnostics do not include local paths.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Malformed WAV or sample data.
    #[error(transparent)]
    Wav(#[from] hound::Error),
    /// Invalid JSON document.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// User-requested stop; any partial result is finalized before return.
    #[error("cancelled")]
    Cancelled,
}
/// Result of a synchronous debugging operation.
pub type Result<T> = std::result::Result<T, Error>;

/// Compiled repository revision, or unavailable outside an AudioKit Git checkout.
pub const BUILD_REVISION: &str = env!("AUDIOKIT_REVISION");
/// Content fingerprint of Rust, TOML, Slint, SVG, PO catalogs and available lockfile.
pub const BUILD_SOURCE_DIGEST: &str = env!("AUDIOKIT_SOURCE_DIGEST");

impl Error {
    /// Stable process code: invalid 2, unavailable 3, runtime 4, cancelled 130.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Invalid(_) | Self::DecodedSampleBudget { .. } | Self::Json(_) | Self::Wav(_) => 2,
            Self::Capability(_) | Self::Audio(audiokit::AudioError::Unsupported(_)) => 3,
            Self::Audio(
                audiokit::AudioError::InvalidConfig(_) | audiokit::AudioError::InvalidFrame(_),
            ) => 2,
            Self::Cancelled => 130,
            _ => 4,
        }
    }
}

/// Validates WAV or packet material and constructs the production graph without artifacts.
pub fn plan_file(config: &RunConfig, input: &std::path::Path) -> Result<ExecutionPlan> {
    config.validate()?;
    let raw = io::bytes(input, config.input_byte_limit())?;
    if config.scenario == Scenario::ReceiveSimulation {
        let trace = packet_trace::parse(&raw, config)?;
        let plan = config.plan(trace.source.format)?;
        runner::validate_plan(config, &plan)?;
        return Ok(plan);
    }
    let (format, pcm) = io::wav(&raw, config.max_pcm_samples)?;
    runner::validate_source_budget(config, format, pcm.len())?;
    let plan = config.plan(format)?;
    runner::validate_plan(config, &plan)?;
    Ok(plan)
}

/// Reads a bounded preset (1 MiB). Execution still requires capability/config validation.
/// This does not grant new input-retention consent or perform audio/device operations.
pub fn read_config(path: &std::path::Path) -> Result<RunConfig> {
    Ok(serde_json::from_slice(&io::bytes(path, 1_048_576)?)?)
}

/// Writes a CLI-compatible preset to a new file; never overwrites an existing file.
pub fn write_config(path: &std::path::Path, config: &RunConfig) -> Result<()> {
    config.validate()?;
    io::write_json(path, config)
}

/// Loads bounded WAV PCM for an explicitly requested preview, without processing it.
/// Sample limits match RunConfig; bytes allow an extra 128 bytes for output WAV headers.
/// PCM16/24/32 and float32 are accepted.
pub fn read_wav(
    path: &std::path::Path,
    max_bytes: u64,
    max_samples: usize,
) -> Result<(audiokit::AudioFormat, Vec<f32>)> {
    if !(1..=268_435_584).contains(&max_bytes) || !(1..=67_108_864).contains(&max_samples) {
        return Err(Error::Invalid(
            "preview resource budget out of range".into(),
        ));
    }
    io::wav(&io::bytes(path, max_bytes)?, max_samples)
}
