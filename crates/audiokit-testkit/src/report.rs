//! Portable versioned records; metrics never imply an audibility judgment.
use crate::{ExecutionPlan, RunConfig};
use serde::{Deserialize, Serialize};

/// One explicit, scoped numeric/accounting check.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Stable check name.
    pub id: String,
    /// Whether its stated condition held, not whether audio sounds perfect.
    pub passed: bool,
    /// Evidence or applicability explanation.
    pub detail: String,
}
/// One bounded stage observation in an explicitly identified virtual clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceEvent {
    /// Bundle run identity, preserved even if the trace is opened separately.
    pub run_id: String,
    /// Stable ordinal before any diagnostic drops.
    pub ordinal: u64,
    /// Observation kind; stage_snapshot is not a sample-accurate internal trace.
    pub kind: String,
    /// Measured production boundary name.
    pub stage: String,
    /// Clock domain, e.g. native_input or virtual_output.
    pub clock_domain: String,
    /// Domain of time_ns; file scenarios use virtual_host scheduling, not CPU time.
    pub time_clock_domain: String,
    /// Virtual scheduling time, not wall-clock CPU time, in nanoseconds.
    pub time_ns: u64,
    /// First per-channel frame in this stage-local range.
    pub first_frame: u64,
    /// Per-channel frames in this observation.
    pub frames: u64,
    /// Per-run anonymous source identity; None for a mixed output boundary.
    pub source_id: Option<u64>,
    /// Logical stream identity, None for mixed output.
    pub stream_id: Option<u16>,
    /// Source epoch, None for a mixed output boundary.
    pub epoch: Option<u64>,
    /// Effective config generation; initial file scenarios have one generation.
    pub config_generation: u64,
    /// Production metrics; their own field units apply.
    pub metrics: serde_json::Value,
}
/// Complete or partial diagnostics; latency contains explicit unavailable fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostics {
    /// Original run identity when this is a re-execution, otherwise None.
    pub replay_origin: Option<ReplayOrigin>,
    /// Report schema version, currently 1.
    pub schema_version: u32,
    /// Unique run identity shared with the manifest.
    pub run_id: String,
    /// completed, cancelled or failed.
    pub status: String,
    /// Requested config; no absolute input paths.
    pub requested_config: RunConfig,
    /// Actual settings after validation; no silent bitrate downgrade is allowed.
    pub effective_config: RunConfig,
    /// Declared graph coverage and boundary formats.
    pub plan: ExecutionPlan,
    /// SHA256 of the original WAV bytes, without retaining the audio by default.
    pub input_sha256: String,
    /// Original per-channel frames.
    pub input_frames: u64,
    /// Produced per-channel frames including explicit latency/tail.
    pub output_frames: u64,
    /// Untrimmed output measurements.
    pub output_signal: serde_json::Value,
    /// Capture accounting and optional receive counters.
    pub graph_statistics: serde_json::Value,
    /// Measured execution and known/configured delays with methods and domains.
    pub latency: serde_json::Value,
    /// Explicitly scoped checks; not an audio quality certificate.
    pub checks: Vec<Check>,
    /// Number of trace observations dropped by the event/byte cap.
    pub trace_events_dropped: u64,
    /// Number of attempted observations; retained events form a bounded prefix.
    pub trace_events_attempted: u64,
    /// First discarded ordinal, None if no diagnostic loss occurred.
    pub trace_first_dropped_ordinal: Option<u64>,
    /// Internal observations not yet exposed by the graph API.
    pub unavailable_observations: Vec<String>,
    /// Structured run failure, if any; no local filenames or credentials.
    pub error: Option<String>,
}

/// Provenance for signal re-execution; it does not claim identical OS scheduling.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayOrigin {
    /// Original diagnostic bundle run identity.
    pub run_id: String,
    /// Original run status; replay does not reproduce cancellation by itself.
    pub original_status: String,
    /// Original built revision.
    pub revision: String,
    /// Original source tree fingerprint.
    pub source_digest: String,
    /// Whether target/profile/source/compiled-backend identity changed.
    pub build_changed: bool,
}
/// Artifact integrity record. Only fixed safe filenames are accepted on import.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Relative single-component filename.
    pub path: String,
    /// Exact file size.
    pub bytes: u64,
    /// SHA256 of file contents.
    pub sha256: String,
}
/// Portable bundle manifest with build identity and reproduction limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Manifest schema version, currently 1.
    pub schema_version: u32,
    /// Unique run identity.
    pub run_id: String,
    /// Built Git revision, or unavailable outside a checkout.
    pub revision: String,
    /// Hash of workspace Rust/TOML source and lockfile, including uncommitted code.
    pub source_digest: String,
    /// Compiled target operating system.
    pub os: String,
    /// Compiled target architecture.
    pub arch: String,
    /// debug or release; timing cannot be compared without this field.
    pub build_profile: String,
    /// Compiled optional backends.
    pub backends: Vec<String>,
    /// Whether the audio run completed; separate from checks passing.
    pub complete: bool,
    /// signal-replay only when input was explicitly retained; otherwise metadata-only.
    pub reproduction: String,
    /// Explicit authorization to retain the original input audio.
    pub input_audio_authorized: bool,
    /// Bounded list of required integrity-checked files.
    pub artifacts: Vec<Artifact>,
}
