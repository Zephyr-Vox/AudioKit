//! Bounded serial Cartesian sweeps using exactly the ordinary file runner.
use crate::{Cancellation, Error, ProgressEvent, Result, RunConfig, io};
use audiokit::PacketDuration;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Typed sweep axes; empty axes retain the corresponding base configuration value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SweepMatrix {
    /// Schema version 1.
    pub schema_version: u32,
    /// Exact bit/s values, validated using the selected production profile.
    pub bitrates_bps: Vec<u32>,
    /// Fixed packet durations; never live renegotiation.
    pub ptimes: Vec<PacketDuration>,
    /// Voice suppression levels; does not implicitly enable a bypassed processor.
    pub noise_levels: Vec<crate::NoiseLevel>,
    /// Encoded jitter startup target in milliseconds.
    pub jitter_targets_ms: Vec<u16>,
    /// Render-stress source-count axis, including silent sources.
    pub mix_sources: Vec<usize>,
    /// Render-stress silent-source axis; each combination must leave audible sources.
    pub mix_silent_sources: Vec<usize>,
    /// Sum of per-case configured source-frame caps for mix stress, at most 200 million.
    pub max_total_source_frames: u64,
    /// Maximum Cartesian combinations, 1..=32.
    pub max_cases: usize,
    /// Maximum source duration per case, 1..=600000 ms; not a wall-clock timeout.
    pub max_input_duration_ms: u32,
    /// Sum of each case's configured output sample cap, 1..=268435456 samples.
    pub max_total_output_samples: u64,
    /// Conservative disk reservation for all cases, at most 2 GiB.
    pub max_total_artifact_bytes: u64,
}
impl Default for SweepMatrix {
    fn default() -> Self {
        Self {
            schema_version: 1,
            bitrates_bps: Vec::new(),
            ptimes: Vec::new(),
            noise_levels: Vec::new(),
            jitter_targets_ms: Vec::new(),
            mix_sources: Vec::new(),
            mix_silent_sources: Vec::new(),
            max_total_source_frames: 64_000_000,
            max_cases: 16,
            max_input_duration_ms: 30_000,
            max_total_output_samples: 64 * 1024 * 1024,
            max_total_artifact_bytes: 1024 * 1024 * 1024,
        }
    }
}
impl SweepMatrix {
    /// Expands a stable Cartesian order and validates all configurations without I/O.
    pub fn expand(&self, base: &RunConfig) -> Result<Vec<RunConfig>> {
        if base.scenario == crate::Scenario::ReceiveSimulation {
            return Err(Error::Capability(
                "external packet sweep is not implemented; run/replay a recording instead".into(),
            ));
        }
        base.validate()?;
        if self.schema_version != 1
            || !(1..=32).contains(&self.max_cases)
            || !(1..=600_000).contains(&self.max_input_duration_ms)
            || !(1..=268_435_456).contains(&self.max_total_output_samples)
            || !(1..=2_147_483_648).contains(&self.max_total_artifact_bytes)
            || !(1..=200_000_000).contains(&self.max_total_source_frames)
        {
            return Err(Error::Invalid("invalid sweep schema or budgets".into()));
        }
        let axes = [
            self.bitrates_bps.len(),
            self.ptimes.len(),
            self.noise_levels.len(),
            self.jitter_targets_ms.len(),
            self.mix_sources.len(),
            self.mix_silent_sources.len(),
        ];
        let count = axes
            .into_iter()
            .try_fold(1_usize, |n, len| n.checked_mul(len.max(1)))
            .ok_or_else(|| Error::Invalid("sweep combination count overflow".into()))?;
        if count > self.max_cases {
            return Err(Error::Invalid("sweep combination budget exceeded".into()));
        }
        if (base.scenario != crate::Scenario::FileRoundtrip)
            && (!self.bitrates_bps.is_empty()
                || !self.ptimes.is_empty()
                || !self.jitter_targets_ms.is_empty())
        {
            return Err(Error::Invalid(
                "codec/jitter sweep axes require roundtrip coverage".into(),
            ));
        }
        if !self.noise_levels.is_empty() && !base.processing.enabled {
            return Err(Error::Invalid(
                "noise sweep requires an enabled voice processor".into(),
            ));
        }
        if base.scenario != crate::Scenario::MixStress
            && (!self.mix_sources.is_empty() || !self.mix_silent_sources.is_empty())
        {
            return Err(Error::Invalid(
                "mix axes require render-stress coverage".into(),
            ));
        }
        if base.scenario == crate::Scenario::MixStress
            && count as u64 * base.mix_stress.max_total_source_frames > self.max_total_source_frames
        {
            return Err(Error::Invalid(
                "sum of source-frame caps exceeds sweep work budget".into(),
            ));
        }
        if count as u64 * base.max_pcm_samples as u64 > self.max_total_output_samples {
            return Err(Error::Invalid(
                "sum of per-case output caps exceeds sweep sample budget".into(),
            ));
        }
        let bitrates = if self.bitrates_bps.is_empty() {
            vec![base.bitrate_bps]
        } else {
            self.bitrates_bps.clone()
        };
        let ptimes = if self.ptimes.is_empty() {
            vec![base.ptime]
        } else {
            self.ptimes.clone()
        };
        let levels = if self.noise_levels.is_empty() {
            vec![base.processing.noise_suppression]
        } else {
            self.noise_levels.clone()
        };
        let targets = if self.jitter_targets_ms.is_empty() {
            vec![base.receive.jitter.target_ms]
        } else {
            self.jitter_targets_ms.clone()
        };
        let mut configs = vec![base.clone()];
        let counts = if self.mix_sources.is_empty() {
            vec![base.mix_stress.sources]
        } else {
            self.mix_sources.clone()
        };
        let silent = if self.mix_silent_sources.is_empty() {
            vec![base.mix_stress.silent_sources]
        } else {
            self.mix_silent_sources.clone()
        };
        configs = expand_axis(configs, &bitrates, |config, value| {
            config.bitrate_bps = value
        });
        configs = expand_axis(configs, &ptimes, |config, value| config.ptime = value);
        configs = expand_axis(configs, &levels, |config, value| {
            config.processing.noise_suppression = value
        });
        configs = expand_axis(configs, &targets, |config, value| {
            config.receive.jitter.target_ms = value
        });
        configs = expand_axis(configs, &counts, |config, value| {
            config.mix_stress.sources = value
        });
        configs = expand_axis(configs, &silent, |config, value| {
            config.mix_stress.silent_sources = value
        });
        for config in &configs {
            config.validate()?;
        }
        Ok(configs)
    }
}
fn expand_axis<T: Copy>(
    configs: Vec<RunConfig>,
    values: &[T],
    apply: impl Fn(&mut RunConfig, T),
) -> Vec<RunConfig> {
    let mut expanded = Vec::with_capacity(configs.len() * values.len());
    for config in configs {
        for value in values {
            let mut candidate = config.clone();
            apply(&mut candidate, *value);
            expanded.push(candidate);
        }
    }
    expanded
}
/// One case's scoped checks or failure; bundle directories are relative to the sweep root.
#[derive(Debug, Serialize)]
pub struct SweepCase {
    /// Zero-based Cartesian index.
    pub index: usize,
    /// Fixed case directory, never derived from arbitrary user strings.
    pub bundle: String,
    /// Completed, failed or cancelled.
    pub status: String,
    /// Ordinary CLI check/error exit code.
    pub exit_code: i32,
    /// Individual run ID if a diagnostic bundle was finalized.
    pub run_id: Option<String>,
    /// Scoped checks that failed, not an automatic audible-quality classification.
    pub failed_checks: Vec<String>,
    /// Explicit runtime error, if any.
    pub error: Option<String>,
}
/// Aggregate sweep result, also written as sweep.json even after cooperative cancellation.
#[derive(Debug, Serialize)]
pub struct SweepReport {
    /// Version 1.
    pub schema_version: u32,
    /// Revision of the binary that ran the sweep.
    pub revision: String,
    /// Source digest, including uncommitted Rust/TOML changes.
    pub source_digest: String,
    /// Completed, failed or cancelled; recorded check failures use exit code 1.
    pub status: String,
    /// Exit code for the CLI; zero only if every planned case passed.
    pub exit_code: i32,
    /// Planned combinations, including cases not attempted after cancellation.
    pub planned_cases: usize,
    /// Source fingerprint shared by all cases; source paths are not recorded.
    pub input_sha256: String,
    /// Conservative disk reservation before the sweep starts.
    pub reserved_artifact_bytes: u64,
    /// Results in stable serial execution order.
    pub cases: Vec<SweepCase>,
}
/// Executes a preflighted serial sweep in a new directory, without devices or network.
///
/// Each case receives the same immutable WAV snapshot and unchanged base fault
/// seed. Source retention follows the base config's explicit authorization.
/// Artifact budgets reserve hard JSON limits and each WAV/input cap, not just
/// observed sizes. Bounds limit media/work/space, not OS execution wall time.
pub fn sweep(
    base: &RunConfig,
    matrix: &SweepMatrix,
    input: &Path,
    output_dir: &Path,
    stop: &Cancellation,
    mut progress: impl FnMut(usize, ProgressEvent),
) -> Result<SweepReport> {
    base.validate()?;
    let configs = matrix.expand(base)?;
    let raw = io::bytes(input, base.max_input_bytes)?;
    let (format, pcm) = io::wav(&raw, base.max_pcm_samples)?;
    let frames = format.frames_in(pcm.len())?.get();
    crate::runner::validate_source_budget(base, format, pcm.len())?;
    if frames * 1000 > u64::from(format.sample_rate_hz()) * u64::from(matrix.max_input_duration_ms)
    {
        return Err(Error::Invalid(
            "source exceeds sweep duration budget".into(),
        ));
    }
    let input_samples = pcm.len();
    drop(pcm);
    let mut reserved = 3 * io::JSON_LIMIT; // Summary, matrix and base configuration.
    for config in &configs {
        crate::runner::validate_source_budget(config, format, input_samples)?;
        let plan = config.plan(format)?;
        crate::runner::validate_plan(config, &plan)?;
        reserved += 4 * io::JSON_LIMIT
            + config.max_pcm_samples as u64 * 4
            + 128
            + if config.retain_input {
                raw.len() as u64
            } else {
                0
            };
    }
    if reserved > matrix.max_total_artifact_bytes {
        return Err(Error::Invalid(
            "conservative sweep artifact reservation exceeds budget".into(),
        ));
    }
    std::fs::create_dir(output_dir)?;
    io::write_json(&output_dir.join("matrix.json"), matrix)?;
    io::write_json(&output_dir.join("base-config.json"), base)?;
    let mut report = SweepReport {
        schema_version: 1,
        revision: crate::BUILD_REVISION.into(),
        source_digest: crate::BUILD_SOURCE_DIGEST.into(),
        status: "completed".into(),
        exit_code: 0,
        planned_cases: configs.len(),
        input_sha256: io::hash(&raw),
        reserved_artifact_bytes: reserved,
        cases: Vec::new(),
    };
    for (index, config) in configs.iter().enumerate() {
        if stop.is_cancelled() {
            report.status = "cancelled".into();
            report.exit_code = 130;
            break;
        }
        let bundle = format!("case-{index:03}");
        let result = crate::runner::run_bytes(
            config,
            raw.clone(),
            &output_dir.join(&bundle),
            stop,
            |event| progress(index, event),
            None,
        );
        let case = match result {
            Ok(diagnostics) => {
                let failed_checks = diagnostics
                    .checks
                    .into_iter()
                    .filter(|c| !c.passed)
                    .map(|c| c.id)
                    .collect::<Vec<_>>();
                SweepCase {
                    index,
                    bundle,
                    status: diagnostics.status,
                    exit_code: i32::from(!failed_checks.is_empty()),
                    run_id: Some(diagnostics.run_id),
                    failed_checks,
                    error: None,
                }
            }
            Err(error) => {
                let diagnostics: Option<crate::Diagnostics> =
                    io::read_json(&output_dir.join(&bundle).join("diagnostics.json")).ok();
                SweepCase {
                    index,
                    bundle,
                    status: if matches!(error, Error::Cancelled) {
                        "cancelled"
                    } else {
                        "failed"
                    }
                    .into(),
                    exit_code: error.exit_code(),
                    run_id: diagnostics.as_ref().map(|d| d.run_id.clone()),
                    failed_checks: diagnostics
                        .map(|d| {
                            d.checks
                                .into_iter()
                                .filter(|c| !c.passed)
                                .map(|c| c.id)
                                .collect()
                        })
                        .unwrap_or_default(),
                    error: Some(error.to_string()),
                }
            }
        };
        if case.exit_code != 0 {
            report.exit_code = report.exit_code.max(case.exit_code);
            report.status = if case.exit_code == 130 {
                "cancelled"
            } else {
                "failed"
            }
            .into();
        }
        report.cases.push(case);
        if report.exit_code == 130 {
            break;
        }
    }
    io::write_json(&output_dir.join("sweep.json"), &report)?;
    Ok(report)
}
