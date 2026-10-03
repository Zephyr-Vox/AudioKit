//! Offline graph, diagnostic integrity, cancellation and optional-feature regressions.
#[cfg(feature = "codec-opus")]
use audiokit::PacketDuration;
use audiokit::StreamKind;
use audiokit_testkit::{Cancellation, Error, RunConfig, Scenario, analyze, compare, replay, run};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "audiokit-testkit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn input(&self, rate: u32, channels: u16, frames: usize) -> PathBuf {
        let path = self.path("source.wav");
        let mut writer = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                sample_rate: rate,
                channels,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for i in 0..frames {
            for channel in 0..channels {
                writer
                    .write_sample(
                        (i as f32 * std::f32::consts::TAU * 997.0 / rate as f32).sin()
                            * (0.2 + f32::from(channel) * 0.05),
                    )
                    .unwrap();
            }
        }
        writer.finalize().unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn bypass() -> RunConfig {
    let mut config = RunConfig::default();
    config.processing.enabled = false;
    config
}
fn json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn backend_delay_budget_is_preflighted_before_creating_artifacts() {
    let fixture = Fixture::new();
    let source = fixture.input(44_100, 1, 4410);
    let mut config = bypass();
    config.resampler.quality = audiokit::resample::ResamplerQuality::HighQuality;
    config.resampler.max_delay_ms = 1;
    let error = audiokit_testkit::plan_file(&config, &source).unwrap_err();
    assert_eq!(error.exit_code(), 2);
    assert!(error.to_string().contains("max_delay_ms"));
    let output = fixture.path("invalid");
    assert!(run(&config, &source, &output, &Cancellation::default(), |_| {}).is_err());
    assert!(!output.exists());
}

#[test]
fn pcm_subchain_is_sample_exact_without_codec_padding_and_replays_exactly() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 1001);
    let mut config = bypass();
    config.retain_input = true;
    let result = run(
        &config,
        &source,
        &fixture.path("first"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(result.input_frames, 1001);
    assert_eq!(result.output_frames, 1001);
    assert!(result.checks.iter().all(|c| c.passed));
    assert_eq!(result.plan.coverage, "capture-subchain");
    assert_eq!(
        result.graph_statistics["capture"]["processing_eof_padding_frames"],
        0
    );
    assert_eq!(
        json(&fixture.path("first/manifest.json"))["reproduction"],
        "signal-replay"
    );
    let output = hound::WavReader::open(fixture.path("first/processed.wav"))
        .unwrap()
        .into_samples::<f32>()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    let input = hound::WavReader::open(&source)
        .unwrap()
        .into_samples::<f32>()
        .map(Result::unwrap)
        .collect::<Vec<_>>();
    assert_eq!(input, output);
    let replayed = replay(
        &fixture.path("first"),
        None,
        &fixture.path("second"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        replayed.replay_origin.as_ref().unwrap().run_id,
        result.run_id
    );
    let compared = compare(&fixture.path("first"), &fixture.path("second")).unwrap();
    assert!(
        compared.same_input
            && compared.same_config
            && compared.same_plan
            && compared.same_output_bytes
            && compared.same_checks
    );
    assert!(
        analyze(&fixture.path("second"))
            .unwrap()
            .recorded_checks_passed
    );
}
#[test]
fn metadata_bundle_has_no_input_path_or_audio_and_needs_hash_matching_source() {
    let fixture = Fixture::new();
    let source = fixture.input(44_100, 2, 4411);
    let result = run(
        &bypass(),
        &source,
        &fixture.path("first"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert!(!fixture.path("first/input.wav").exists());
    let manifest = fs::read_to_string(fixture.path("first/manifest.json")).unwrap();
    let config = fs::read_to_string(fixture.path("first/config.json")).unwrap();
    assert!(!manifest.contains(&fixture.0.to_string_lossy().to_string()));
    assert!(!config.contains("source.wav"));
    assert_eq!(
        analyze(&fixture.path("first")).unwrap().reproduction,
        "metadata-only"
    );
    assert!(
        replay(
            &fixture.path("first"),
            None,
            &fixture.path("missing"),
            &Cancellation::default(),
            |_| {}
        )
        .is_err()
    );
    assert!(!fixture.path("missing").exists());
    replay(
        &fixture.path("first"),
        Some(&source),
        &fixture.path("second"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert!(result.output_frames > result.input_frames);
    fs::write(&source, b"changed source").unwrap();
    assert!(
        replay(
            &fixture.path("first"),
            Some(&source),
            &fixture.path("wrong"),
            &Cancellation::default(),
            |_| {}
        )
        .is_err()
    );
}
#[test]
fn tampered_artifact_and_path_escape_are_rejected_without_output() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 4800);
    run(
        &bypass(),
        &source,
        &fixture.path("run"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    let diagnostic = fixture.path("run/diagnostics.json");
    let original = fs::read(&diagnostic).unwrap();
    fs::write(&diagnostic, b"{}").unwrap();
    assert!(analyze(&fixture.path("run")).is_err());
    fs::write(&diagnostic, original).unwrap();
    let path = fixture.path("run/manifest.json");
    let mut manifest = json(&path);
    manifest["artifacts"][0]["path"] = serde_json::json!("../source.wav");
    fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(analyze(&fixture.path("run")).is_err());
}
#[test]
fn trace_caps_do_not_change_audio_and_report_diagnostic_loss() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 4800);
    let mut config = bypass();
    config.max_trace_events = 1;
    let small = run(
        &config,
        &source,
        &fixture.path("small"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert!(small.trace_events_dropped > 0);
    run(
        &bypass(),
        &source,
        &fixture.path("normal"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert!(
        compare(&fixture.path("small"), &fixture.path("normal"))
            .unwrap()
            .same_output_bytes
    );
    assert!(
        analyze(&fixture.path("small"))
            .unwrap()
            .observations
            .iter()
            .any(|s| s.contains("budget exhausted"))
    );
}
#[test]
fn cancellation_finalizes_partial_wav_and_manifest_without_detaching() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 4800);
    let stop = Cancellation::default();
    let trigger = stop.clone();
    let error = run(
        &bypass(),
        &source,
        &fixture.path("partial"),
        &stop,
        move |_| trigger.cancel(),
    )
    .unwrap_err();
    assert!(matches!(error, Error::Cancelled));
    assert_eq!(
        json(&fixture.path("partial/manifest.json"))["complete"],
        false
    );
    assert_eq!(
        json(&fixture.path("partial/diagnostics.json"))["status"],
        "cancelled"
    );
    assert!(hound::WavReader::open(fixture.path("partial/processed.wav")).is_ok());
    assert!(
        !analyze(&fixture.path("partial"))
            .unwrap()
            .recorded_checks_passed
    );
}
#[test]
fn resource_failure_is_partial_and_existing_directory_is_never_overwritten() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 4800);
    let mut config = bypass();
    config.max_pcm_samples = 4800;
    config.scenario = Scenario::FileProcessing;
    run(
        &config,
        &source,
        &fixture.path("run"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    let original = fs::read(fixture.path("run/processed.wav")).unwrap();
    assert!(
        run(
            &config,
            &source,
            &fixture.path("run"),
            &Cancellation::default(),
            |_| {}
        )
        .is_err()
    );
    assert_eq!(
        original,
        fs::read(fixture.path("run/processed.wav")).unwrap()
    );
    config.max_input_bytes = 10;
    assert!(
        run(
            &config,
            &source,
            &fixture.path("oversize"),
            &Cancellation::default(),
            |_| {}
        )
        .is_err()
    );
    assert!(!fixture.path("oversize").exists());
}
#[test]
fn invalid_config_and_missing_reference_are_rejected_without_work() {
    let mut config = bypass();
    config.processing.aec = true;
    assert!(config.validate().is_err());
    config.processing.aec = false;
    config.stream = StreamKind::Desktop;
    config.bitrate_bps = 196_000;
    assert!(config.validate().is_ok());
    config.processing.enabled = true;
    assert!(matches!(config.validate(), Err(Error::Invalid(_))));
    assert!(serde_json::from_str::<RunConfig>(r#"{"unknown":1}"#).is_err());
    config = bypass();
    config.schema_version = 99;
    assert!(config.validate().is_err());
}
#[cfg(not(feature = "codec-opus"))]
#[test]
fn missing_codec_is_structured_capability_error() {
    let mut config = bypass();
    config.scenario = Scenario::FileRoundtrip;
    assert!(matches!(config.validate(), Err(Error::Capability(_))));
}
#[cfg(not(feature = "processing-sonora"))]
#[test]
fn missing_processor_is_structured_capability_error() {
    assert!(matches!(
        RunConfig::default().validate(),
        Err(Error::Capability(_))
    ));
}

#[cfg(feature = "processing-sonora")]
#[test]
fn sonora_subchain_retains_known_filter_and_apm_padding_without_codec() {
    let fixture = Fixture::new();
    let source = fixture.input(44_100, 2, 4411);
    let result = run(
        &RunConfig::default(),
        &source,
        &fixture.path("run"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(result.plan.output_format.channels(), 1);
    assert!(result.output_frames.is_multiple_of(480));
    assert!(
        result.graph_statistics["capture"]["processing_eof_padding_frames"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(result.latency["processor"]["frames"].is_null());
}
#[cfg(feature = "codec-opus")]
#[test]
fn opus_roundtrip_covers_profiles_ptimes_and_resampling_without_spurious_plc() {
    for (stream, bitrate) in [(StreamKind::Voice, 96_789), (StreamKind::Desktop, 196_000)] {
        for ptime in [
            PacketDuration::Ms10,
            PacketDuration::Ms20,
            PacketDuration::Ms40,
            PacketDuration::Ms60,
        ] {
            let fixture = Fixture::new();
            let source = fixture.input(44_100, 2, 13_231);
            let mut config = bypass();
            config.scenario = Scenario::FileRoundtrip;
            config.stream = stream;
            config.bitrate_bps = bitrate;
            config.ptime = ptime;
            let result = run(
                &config,
                &source,
                &fixture.path("run"),
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();
            assert!(
                result.checks.iter().all(|c| c.passed),
                "{}",
                serde_json::to_string(&result.checks).unwrap()
            );
            assert_eq!(result.effective_config.bitrate_bps, bitrate);
            assert_eq!(result.plan.output_format.channels(), 2);
            assert_eq!(result.graph_statistics["receive"]["decode_errors"], 0);
            assert_eq!(result.graph_statistics["receive"]["concealed_packets"], 0);
        }
    }
}
#[cfg(feature = "codec-opus")]
#[test]
fn roundtrip_output_cap_failure_produces_valid_partial_bundle() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 4800);
    let mut config = bypass();
    config.scenario = Scenario::FileRoundtrip;
    config.max_pcm_samples = 4800;
    assert!(
        run(
            &config,
            &source,
            &fixture.path("partial"),
            &Cancellation::default(),
            |_| {}
        )
        .is_err()
    );
    assert_eq!(
        json(&fixture.path("partial/diagnostics.json"))["status"],
        "failed"
    );
    assert!(
        !analyze(&fixture.path("partial"))
            .unwrap()
            .recorded_checks_passed
    );
}

#[cfg(all(feature = "codec-opus", feature = "processing-sonora"))]
#[test]
fn real_sonora_and_opus_use_the_complete_production_roundtrip() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 2, 10001);
    let config = RunConfig {
        scenario: Scenario::FileRoundtrip,
        ..Default::default()
    };
    let result = run(
        &config,
        &source,
        &fixture.path("run"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert!(result.checks.iter().all(|c| c.passed));
    assert!(
        result.graph_statistics["capture"]["processing_execution_ns"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        result.latency["render_algorithms"]["source_limiter_frames"]
            .as_u64()
            .unwrap()
            > 0
    );
}
#[test]
fn byte_limited_trace_is_a_prefix_and_carries_separate_clock_domains() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 4800);
    let mut config = bypass();
    config.max_trace_bytes = 1024;
    let result = run(
        &config,
        &source,
        &fixture.path("run"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert!(result.trace_events_dropped > 0);
    let trace = json(&fixture.path("run/trace.json"));
    let entries = trace.as_array().unwrap();
    assert_eq!(
        result.trace_events_attempted,
        entries.len() as u64 + result.trace_events_dropped
    );
    assert_eq!(
        result.trace_first_dropped_ordinal,
        Some(entries.len() as u64)
    );
    assert_eq!(entries[0]["time_clock_domain"], "virtual_host");
    assert_eq!(entries[0]["clock_domain"], "capture_output");
    assert!(analyze(&fixture.path("run")).is_ok());
}
#[test]
fn malicious_loss_counter_does_not_overflow_the_importer() {
    use sha2::{Digest, Sha256};
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 4800);
    run(
        &bypass(),
        &source,
        &fixture.path("run"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    let path = fixture.path("run/diagnostics.json");
    let mut report = json(&path);
    report["trace_events_dropped"] = serde_json::json!(u64::MAX);
    let data = serde_json::to_vec(&report).unwrap();
    fs::write(&path, &data).unwrap();
    let path = fixture.path("run/manifest.json");
    let mut manifest = json(&path);
    for artifact in manifest["artifacts"].as_array_mut().unwrap() {
        if artifact["path"] == "diagnostics.json" {
            artifact["bytes"] = serde_json::json!(data.len());
            artifact["sha256"] = serde_json::json!(format!("{:x}", Sha256::digest(&data)));
        }
    }
    fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(analyze(&fixture.path("run")).is_err());
}
