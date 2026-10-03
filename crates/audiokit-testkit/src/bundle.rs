//! Integrity-checked import, offline analysis and reproducible re-execution.
use crate::{
    Cancellation, Diagnostics, Error, Manifest, ProgressEvent, Result, RunConfig, TraceEvent, io,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

struct Bundle {
    manifest: Manifest,
    config: RunConfig,
    diagnostics: Diagnostics,
    trace: Vec<TraceEvent>,
    input: Option<Vec<u8>>,
}
fn load(root: &Path) -> Result<Bundle> {
    let root = root.canonicalize()?;
    let manifest_path = root.join("manifest.json").canonicalize()?;
    if !manifest_path.starts_with(&root) {
        return Err(Error::Invalid("manifest escapes bundle directory".into()));
    }
    let manifest: Manifest = io::read_json(&manifest_path)?;
    if manifest.schema_version != 1 || manifest.artifacts.len() > 5 {
        return Err(Error::Invalid(
            "unsupported manifest schema or artifact count".into(),
        ));
    }
    let input_artifact = if manifest.artifacts.iter().any(|a| a.path == "packets.json") {
        "packets.json"
    } else {
        "input.wav"
    };
    let expected: BTreeSet<_> = if manifest.input_audio_authorized {
        [
            "config.json",
            "diagnostics.json",
            "trace.json",
            "processed.wav",
            input_artifact,
        ]
        .into_iter()
        .collect()
    } else {
        [
            "config.json",
            "diagnostics.json",
            "trace.json",
            "processed.wav",
        ]
        .into_iter()
        .collect()
    };
    let mut seen = BTreeSet::new();
    let mut snapshots = BTreeMap::new();
    let mut input = None;
    for artifact in &manifest.artifacts {
        if !expected.contains(artifact.path.as_str()) || !seen.insert(artifact.path.as_str()) {
            return Err(Error::Invalid(
                "unexpected, duplicate or unsafe artifact path".into(),
            ));
        }
        let path = root.join(&artifact.path).canonicalize()?;
        if !path.starts_with(&root) {
            return Err(Error::Invalid("artifact escapes bundle directory".into()));
        }
        let max = if artifact.path.ends_with(".json") {
            io::JSON_LIMIT
        } else if artifact.path == "input.wav" {
            268_435_456
        } else {
            268_435_584
        };
        if artifact.bytes > max {
            return Err(Error::Invalid("artifact exceeds byte budget".into()));
        }
        let data = io::bytes(&path, artifact.bytes)?;
        if data.len() as u64 != artifact.bytes || io::hash(&data) != artifact.sha256 {
            return Err(Error::Invalid("artifact size or SHA256 mismatch".into()));
        }
        if artifact.path == input_artifact {
            input = Some(data);
        } else if artifact.path.ends_with(".json") {
            snapshots.insert(artifact.path.clone(), data);
        }
    }
    if seen != expected {
        return Err(Error::Invalid("missing required artifact".into()));
    }
    // Parse the exact bytes whose hash was validated, not a second mutable path read.
    let config: RunConfig = serde_json::from_slice(&snapshots["config.json"])?;
    config.validate_parameters()?;
    let diagnostics: Diagnostics = serde_json::from_slice(&snapshots["diagnostics.json"])?;
    let trace: Vec<TraceEvent> = serde_json::from_slice(&snapshots["trace.json"])?;
    let expected_plan = config.plan_parameters(diagnostics.plan.input_format)?;
    if config.schema_version != 1
        || diagnostics.schema_version != 1
        || diagnostics.run_id != manifest.run_id
        || diagnostics.plan.scenario != config.scenario
        || !["completed", "cancelled", "failed"].contains(&diagnostics.status.as_str())
        || serde_json::to_value(&expected_plan)? != serde_json::to_value(&diagnostics.plan)?
        || config.retain_input != manifest.input_audio_authorized
        || config.retain_input && config.input_artifact() != input_artifact
        || manifest.complete != (diagnostics.status == "completed")
        || manifest.reproduction != config.reproduction(&diagnostics.graph_statistics)
        || serde_json::to_value(&config)? != serde_json::to_value(&diagnostics.effective_config)?
    {
        return Err(Error::Invalid(
            "inconsistent bundle identity, configuration or completion".into(),
        ));
    }
    if trace.len() > config.max_trace_events
        || diagnostics.trace_first_dropped_ordinal
            != if diagnostics.trace_events_dropped == 0 {
                None
            } else {
                Some(trace.len() as u64)
            }
        || (trace.len() as u64).checked_add(diagnostics.trace_events_dropped)
            != Some(diagnostics.trace_events_attempted)
        || trace.iter().enumerate().any(|(i, e)| {
            e.ordinal != i as u64 || e.run_id != manifest.run_id || e.config_generation != 1
        })
        || !trace.windows(2).all(|w| w[0].ordinal < w[1].ordinal)
        || trace
            .iter()
            .any(|e| e.first_frame.checked_add(e.frames).is_none())
    {
        return Err(Error::Invalid(
            "invalid trace order, range or event budget".into(),
        ));
    }
    if input.as_ref().is_some_and(|data| {
        data.len() as u64 > config.input_byte_limit() || io::hash(data) != diagnostics.input_sha256
    }) {
        return Err(Error::Invalid(
            "retained source does not match input hash".into(),
        ));
    }
    if config.scenario == crate::Scenario::ReceiveSimulation {
        if diagnostics.input_frames != 0 {
            return Err(Error::Invalid(
                "packet input cannot assert original PCM frame count".into(),
            ));
        }
        if let Some(data) = &input {
            let packet_trace = crate::packet_trace::parse(data, &config)?;
            if packet_trace.source.format != diagnostics.plan.input_format
                || packet_trace.summary() != diagnostics.graph_statistics["packet_recording"]
            {
                return Err(Error::Invalid("packet material/report mismatch".into()));
            }
        }
    }
    Ok(Bundle {
        manifest,
        config,
        diagnostics,
        trace,
        input,
    })
}
/// Offline observations from recorded evidence, not an unproven root-cause classifier.
#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    /// Retained trace ordinal; observation order, not causal order.
    pub ordinal: u64,
    /// Actual observation stage, not a predicted root cause.
    pub stage: String,
    /// Scheduling timestamp in the explicitly named time domain.
    pub time_ns: u64,
    /// Timestamp domain, distinct from the sample domain.
    pub time_clock_domain: String,
    /// Sample range clock domain.
    pub clock_domain: String,
    /// First observed stage-local sample frame.
    pub first_frame: u64,
    /// Length of that stage-local observation interval.
    pub frames: u64,
    /// Observed injection/receiver/render flags, not audible-fault classifications.
    pub flags: Vec<String>,
}

fn evidence_flags(event: &TraceEvent) -> Vec<String> {
    let mut flags = Vec::new();
    let metrics = &event.metrics;
    if event.stage == "transport_schedule" {
        for (key, flag) in [
            ("dropped", "injection_drop"),
            ("reorder_selected", "injection_reorder_selection"),
            ("stalled", "injection_stall"),
        ] {
            if metrics[key] == true {
                flags.push(flag.into());
            }
        }
    }
    if matches!(event.stage.as_str(), "transport_arrival" | "packet_input")
        && let Some(outcome) = metrics["outcome"]
            .as_str()
            .filter(|value| *value != "accepted")
    {
        flags.push(if outcome == "payload_unavailable" {
            "recording_payload_missing".into()
        } else {
            format!("receiver_{outcome}")
        });
    }
    if event.stage == "render_output" {
        if metrics["worker_over_budget"] == true {
            flags.push("worker_budget_overrun".into());
        }
        if metrics["sources"].as_array().is_some_and(|sources| {
            sources
                .iter()
                .any(|source| source["missing_frames"].as_u64().unwrap_or(0) > 0)
        }) {
            flags.push("source_missing_frames".into());
        }
        if metrics["master_limiter"]["safety_clamped_samples"]
            .as_u64()
            .unwrap_or(0)
            > 0
        {
            flags.push("master_safety_clamp".into());
        }
    }
    flags
}

/// Integrity-verified summary and a bounded timeline of observed flags.
#[derive(Debug, Serialize)]
pub struct Analysis {
    /// Response schema version.
    pub schema_version: u32,
    /// Integrity-checked original run.
    pub run_id: String,
    /// All listed artifact sizes/hashes validated successfully.
    pub integrity_verified: bool,
    /// Whether the source signal was included with explicit consent.
    pub reproduction: String,
    /// True only when the recorded run completed and all its scoped checks passed.
    pub recorded_checks_passed: bool,
    /// Stable identifiers of failed recorded checks.
    pub failed_checks: Vec<String>,
    /// Available trace snapshots, not the number of audio blocks.
    pub trace_events: usize,
    /// Recorded dropped event count.
    pub trace_events_dropped: u64,
    /// Observed/unknown limitations and signal candidates.
    pub observations: Vec<String>,
    /// Whether the importing binary differs from the recorded source tree.
    pub build_changed: bool,
    /// At most 64 flagged retained intervals; omitted trace remains unknown.
    pub evidence: Vec<Evidence>,
    /// Flagged intervals beyond the analysis cap, separate from recorder loss.
    pub evidence_omitted: usize,
}
/// Checks integrity and summarizes existing evidence without devices, network or package mutation.
pub fn analyze(root: &Path) -> Result<Analysis> {
    let bundle = load(root)?;
    let diagnostics = &bundle.diagnostics;
    let failed = diagnostics
        .checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| c.id.clone())
        .collect::<Vec<_>>();
    let mut observations = vec![format!("observed: coverage is {}; hardware/server nodes are not covered", diagnostics.plan.coverage), "unknown: aggregate signal metrics and stage snapshots alone cannot prove an audible click or its root cause".into()];
    if diagnostics.trace_events_dropped > 0 {
        observations.push(
            "observed: event budget exhausted; missing intervals cannot be inferred as healthy"
                .into(),
        );
    }
    if bundle.config.transport.is_impaired() {
        observations.push(format!("observed: virtual faults enabled; intentionally dropped={}, duplicate copies={}, late arrivals={}, PLC slots={}, FEC attempts={}; injection selection is not proof of audible failure",
            diagnostics.graph_statistics["transport"]["intentionally_dropped"],
            diagnostics.graph_statistics["transport"]["duplicated_packets"],
            diagnostics.graph_statistics["receive"]["late"],
            diagnostics.graph_statistics["receive"]["concealed_packets"],
            diagnostics.graph_statistics["receive"]["fec_attempts"]));
    }
    if bundle.config.clocks.is_shifted() {
        observations.push(format!("observed: simulated capture/render rates are {}/{} ppm; final inferred source clocks={}; real device clocks remain unknown",
            bundle.config.clocks.capture_rate_ppm, bundle.config.clocks.render_rate_ppm,
            diagnostics.graph_statistics["last_steady_source_clocks"]));
    }
    if bundle.config.scenario == crate::Scenario::MixStress {
        observations.push(format!("observed: correlated render-stress sources={}, silent sources={}, budget overruns={}; timing excludes drain, tracing, host scheduling and real devices",
            bundle.config.mix_stress.sources, bundle.config.mix_stress.silent_sources,
            diagnostics.latency["receive_execution"]["over_budget_calls"]));
    }
    if bundle.config.scenario == crate::Scenario::ReceiveSimulation {
        observations.push(format!("observed: external single-source recording material complete={}; missing payloads={}, omitted arrivals={}, omitted demands={}; cold-start receiver only, synthetic drain and no actual device/server scheduling",
            diagnostics.graph_statistics["packet_recording"]["complete_material"],
            diagnostics.graph_statistics["packet_recording"]["missing_payloads"],
            diagnostics.graph_statistics["packet_recording"]["omitted_packets"],
            diagnostics.graph_statistics["packet_recording"]["omitted_render_ticks"]));
    }
    if diagnostics.output_signal["discontinuity_candidates"]
        .as_u64()
        .unwrap_or(0)
        > 0
    {
        observations.push("observed: sample-delta candidates exist; no confirmed audible discontinuity classification".into());
    }
    if bundle.manifest.reproduction == "metadata-only" {
        observations.push("unknown: source signal absent; exact signal replay needs the original hash-matching input".into());
    }
    let mut evidence = Vec::new();
    let mut evidence_omitted = 0;
    for event in &bundle.trace {
        let flags = evidence_flags(event);
        if flags.is_empty() {
            continue;
        }
        if evidence.len() == 64 {
            evidence_omitted += 1;
            continue;
        }
        evidence.push(Evidence {
            ordinal: event.ordinal,
            stage: event.stage.clone(),
            time_ns: event.time_ns,
            time_clock_domain: event.time_clock_domain.clone(),
            clock_domain: event.clock_domain.clone(),
            first_frame: event.first_frame,
            frames: event.frames,
            flags,
        });
    }
    Ok(Analysis {
        schema_version: 1,
        run_id: diagnostics.run_id.clone(),
        integrity_verified: true,
        reproduction: bundle.manifest.reproduction.clone(),
        recorded_checks_passed: bundle.manifest.complete && failed.is_empty(),
        failed_checks: failed,
        trace_events: bundle.trace.len(),
        trace_events_dropped: diagnostics.trace_events_dropped,
        observations,
        build_changed: build_changed(&bundle.manifest),
        evidence,
        evidence_omitted,
    })
}
/// Re-executes a validated signal scenario, preserving original config in a new directory.
/// An external input is accepted only if its hash matches the recorded source.
pub fn replay(
    bundle_dir: &Path,
    input_override: Option<&Path>,
    output_dir: &Path,
    stop: &Cancellation,
    progress: impl FnMut(ProgressEvent),
) -> Result<Diagnostics> {
    let bundle = load(bundle_dir)?;
    let input =
        match input_override {
            Some(path) => io::bytes(path, bundle.config.input_byte_limit())?,
            None if bundle.manifest.input_audio_authorized => bundle
                .input
                .ok_or_else(|| Error::Invalid("authorized input missing".into()))?,
            None => return Err(Error::Invalid(
                "metadata-only bundle: supply the original input or an authorized signal bundle"
                    .into(),
            )),
        };
    if io::hash(&input) != bundle.diagnostics.input_sha256 {
        return Err(Error::Invalid("replay input hash mismatch".into()));
    }
    let origin = crate::ReplayOrigin {
        run_id: bundle.manifest.run_id.clone(),
        original_status: bundle.diagnostics.status,
        revision: bundle.manifest.revision.clone(),
        source_digest: bundle.manifest.source_digest.clone(),
        build_changed: build_changed(&bundle.manifest),
    };
    crate::runner::run_bytes(
        &bundle.config,
        input,
        output_dir,
        stop,
        progress,
        Some(origin),
    )
}

fn build_changed(manifest: &Manifest) -> bool {
    manifest.source_digest != env!("AUDIOKIT_SOURCE_DIGEST")
        || manifest.revision != env!("AUDIOKIT_REVISION")
        || manifest.os != std::env::consts::OS
        || manifest.arch != std::env::consts::ARCH
        || manifest.build_profile
            != if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        || manifest.backends != crate::runner::compiled_backends()
}
/// Scoped comparison; build/timing differences never imply an audio regression by themselves.
#[derive(Debug, Serialize)]
pub struct Comparison {
    /// Response schema version.
    pub schema_version: u32,
    /// Baseline run identity.
    pub baseline_run: String,
    /// Candidate run identity.
    pub candidate_run: String,
    /// Whether original WAV hashes match.
    pub same_input: bool,
    /// Whether effective parameters match.
    pub same_config: bool,
    /// Whether coverage, stages and formats match.
    pub same_plan: bool,
    /// Whether output bytes match; cross-platform floating differences may be acceptable.
    pub same_output_bytes: bool,
    /// Whether the scoped recorded checks match.
    pub same_checks: bool,
    /// Whether source digests/profile/target/backend identities differ.
    pub build_changed: bool,
    /// Baseline output measurements.
    pub baseline_signal: serde_json::Value,
    /// Candidate output measurements.
    pub candidate_signal: serde_json::Value,
}
/// Compares integrity-checked bundles without changing either package or invoking processing.
pub fn compare(baseline: &Path, candidate: &Path) -> Result<Comparison> {
    let a = load(baseline)?;
    let b = load(candidate)?;
    let output_hash = |m: &Manifest| {
        m.artifacts
            .iter()
            .find(|x| x.path == "processed.wav")
            .map(|x| x.sha256.clone())
    };
    Ok(Comparison {
        schema_version: 1,
        baseline_run: a.manifest.run_id.clone(),
        candidate_run: b.manifest.run_id.clone(),
        same_input: a.diagnostics.input_sha256 == b.diagnostics.input_sha256,
        same_config: serde_json::to_value(&a.config)? == serde_json::to_value(&b.config)?,
        same_plan: serde_json::to_value(&a.diagnostics.plan)?
            == serde_json::to_value(&b.diagnostics.plan)?,
        same_output_bytes: output_hash(&a.manifest) == output_hash(&b.manifest),
        same_checks: serde_json::to_value(&a.diagnostics.checks)?
            == serde_json::to_value(&b.diagnostics.checks)?,
        build_changed: a.manifest.source_digest != b.manifest.source_digest
            || a.manifest.build_profile != b.manifest.build_profile
            || a.manifest.os != b.manifest.os
            || a.manifest.arch != b.manifest.arch
            || a.manifest.backends != b.manifest.backends,
        baseline_signal: a.diagnostics.output_signal,
        candidate_signal: b.diagnostics.output_signal,
    })
}
