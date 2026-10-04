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

#[test]
fn automatic_resources_grow_through_resampling_and_preserve_manual_limits() {
    use audiokit_testkit::{MAX_INPUT_BYTES, MAX_PCM_SAMPLES, inspect, read_processed_wav};
    let fixture = Fixture::new();
    let input = fixture.input(8_000, 2, 1001);
    let mut config = bypass();
    assert_eq!(config.max_input_bytes, 0);
    assert_eq!(config.max_pcm_samples, 0);
    assert_eq!(config.input_byte_limit(), MAX_INPUT_BYTES);
    assert_eq!(config.pcm_sample_limit(), MAX_PCM_SAMPLES);
    config.retain_input = true;
    let output = fixture.path("automatic");
    let d = run(&config, &input, &output, &Cancellation::default(), |_| {}).unwrap();
    let resources = &d.graph_statistics["resources"];
    assert_eq!(resources["automatic_pcm"], true);
    assert_eq!(resources["input_pcm_samples"], 2002);
    assert!(resources["output_pcm_samples"].as_u64().unwrap() > 2002);
    assert!(resources["output_capacity_samples"].as_u64().unwrap() < 16 * 1024 * 1024);
    assert_eq!(d.requested_config.max_pcm_samples, 0);
    assert_eq!(d.effective_config.max_pcm_samples, MAX_PCM_SAMPLES);
    assert!(inspect(&output).is_ok());
    let replayed = fixture.path("automatic-replay");
    replay(&output, None, &replayed, &Cancellation::default(), |_| {}).unwrap();
    assert_eq!(
        read_processed_wav(&output).unwrap(),
        read_processed_wav(&replayed).unwrap()
    );

    config.max_pcm_samples = 2002; // Fits the source, but not 8 -> 48 kHz output.
    let error = run(
        &config,
        &input,
        &fixture.path("manual"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("output sample budget exhausted"));
    let d = json(&fixture.path("manual/diagnostics.json"));
    assert_eq!(d["graph_statistics"]["resources"]["automatic_pcm"], false);
    assert!(
        d["graph_statistics"]["resources"]["output_capacity_samples"]
            .as_u64()
            .unwrap()
            <= 2002
    );
    assert!(analyze(&fixture.path("manual")).is_ok());
}

#[test]
fn automatic_sweep_caps_cannot_bypass_aggregate_reservations() {
    use audiokit_testkit::SweepMatrix;
    let mut config = bypass();
    config.scenario = Scenario::MixStress;
    let matrix = SweepMatrix {
        mix_sources: vec![1, 2, 3],
        max_total_output_samples: 3001,
        ..Default::default()
    };
    let cases = matrix.expand(&config).unwrap();
    assert_eq!(cases.len(), 3);
    assert!(cases.iter().all(|c| c.pcm_sample_limit() == 1000));
    assert_eq!(config.max_pcm_samples, 0); // Source config remains immutable.
    config.max_pcm_samples = 2000;
    assert!(matrix.expand(&config).is_err());
    config.max_pcm_samples = 0;
    assert!(
        SweepMatrix {
            max_total_output_samples: 2,
            ..matrix
        }
        .expand(&config)
        .is_err()
    );
    config.max_pcm_samples = audiokit_testkit::MAX_PCM_SAMPLES + 1;
    assert!(config.validate().is_err());
    config.max_pcm_samples = 0;
    config.max_input_bytes = audiokit_testkit::MAX_INPUT_BYTES + 1;
    assert!(config.validate().is_err());
}

#[test]
fn presets_and_exports_are_bounded_validated_and_never_overwrite() {
    use audiokit_testkit::{
        export_bundle, export_wav, inspect, read_config, read_processed_wav, read_wav, write_config,
    };
    let fixture = Fixture::new();
    let input = fixture.input(48_000, 1, 1001);
    for retained in [false, true] {
        let mut config = bypass();
        config.retain_input = retained;
        let source = fixture.path(&format!("source-{retained}"));
        run(&config, &input, &source, &Cancellation::default(), |_| {}).unwrap();
        // Unlisted files (including secrets or extra input) never enter an export.
        fs::write(source.join("unlisted.txt"), "must stay local").unwrap();
        let destination = fixture.path(&format!("export-{retained}"));
        let manifest = export_bundle(&source, &destination, &Cancellation::default()).unwrap();
        assert_eq!(manifest.input_audio_authorized, retained);
        assert_eq!(destination.join("input.wav").exists(), retained);
        assert!(!destination.join("unlisted.txt").exists());
        assert!(inspect(&destination).is_ok());
        assert!(export_bundle(&source, &destination, &Cancellation::default()).is_err());
        let wav = fixture.path(&format!("export-{retained}.wav"));
        export_wav(&source, &wav, &Cancellation::default()).unwrap();
        let bytes = fs::read(&wav).unwrap();
        assert_eq!(bytes, fs::read(source.join("processed.wav")).unwrap());
        assert!(export_wav(&source, &wav, &Cancellation::default()).is_err());
        assert_eq!(fs::read(&wav).unwrap(), bytes);
        let (format, pcm) = read_processed_wav(&source).unwrap();
        let (export_format, export_pcm) = read_wav(&wav, 268_435_584, 67_108_864).unwrap();
        assert_eq!(format, export_format);
        assert_eq!(pcm, export_pcm);
        assert_eq!(pcm, read_wav(&wav, 0, 0).unwrap().1);
        assert!(read_wav(&wav, 1, 1001).is_err());
        assert!(read_wav(&wav, 268_435_585, 1001).is_err());
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let stopped = fixture.path(&format!("stopped-{retained}"));
        assert!(matches!(
            export_bundle(&source, &stopped, &cancelled),
            Err(Error::Cancelled)
        ));
        assert!(!stopped.exists());
        assert!(matches!(
            export_wav(&source, &stopped, &cancelled),
            Err(Error::Cancelled)
        ));
        assert!(!stopped.exists());
        fs::write(source.join("processed.wav"), "tampered").unwrap();
        let rejected = fixture.path(&format!("rejected-{retained}"));
        assert!(export_bundle(&source, &rejected, &Cancellation::default()).is_err());
        assert!(export_wav(&source, &rejected, &Cancellation::default()).is_err());
        assert!(read_processed_wav(&source).is_err());
        assert!(!rejected.exists());
    }
    let config = bypass();
    let preset = fixture.path("preset.json");
    write_config(&preset, &config).unwrap();
    assert_eq!(
        serde_json::to_value(read_config(&preset).unwrap()).unwrap(),
        serde_json::to_value(&config).unwrap()
    );
    assert!(write_config(&preset, &config).is_err());
    let large = fixture.path("large.json");
    fs::write(&large, vec![b' '; 1_048_577]).unwrap();
    assert!(read_config(&large).is_err());
    fs::write(&large, br#"{"typo":true}"#).unwrap();
    assert!(read_config(&large).is_err());
}

#[test]
fn profiling_is_pcm_neutral_and_histograms_survive_trace_truncation() {
    let fixture = Fixture::new();
    let input = fixture.input(44_100, 2, 4410);
    for stream in [StreamKind::Voice, StreamKind::Desktop] {
        let mut config = RunConfig::for_scenario(Scenario::MixStress);
        config.stream = stream;
        if stream == StreamKind::Desktop {
            config.bitrate_bps = 196_000;
        }
        config.mix_stress.sources = 4;
        let plain = fixture.path(&format!("plain-{stream:?}"));
        run(&config, &input, &plain, &Cancellation::default(), |_| {}).unwrap();
        config.execution_profiling = true;
        config.max_trace_events = 1;
        let profiled = fixture.path(&format!("profiled-{stream:?}"));
        let d = run(&config, &input, &profiled, &Cancellation::default(), |_| {}).unwrap();
        assert_eq!(
            fs::read(plain.join("processed.wav")).unwrap(),
            fs::read(profiled.join("processed.wav")).unwrap()
        );
        assert!(d.trace_events_dropped > 0);
        let latency = &d.latency;
        assert_eq!(
            latency["execution_profile"]["source_processing"]["calls"],
            10
        );
        assert_eq!(latency["receive_execution"]["ordinary"]["calls"], 10);
        assert!(
            latency["receive_execution"]["ordinary"]["p99_ns"]
                .as_u64()
                .is_some()
        );
        assert!(
            latency["receive_execution"]["unbudgeted"]["calls"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(latency["execution_profile"]["decode"]["p99_ns"].is_null());
        for stage in ["queue_read", "gain", "activity", "limiter", "channel_map"] {
            let timing = &latency["execution_profile"]["source_stages"][stage];
            assert_eq!(timing["calls"], 10);
            assert!(timing["p99_ns"].as_u64().is_some());
            assert_eq!(
                timing["bins"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap())
                    .sum::<u64>(),
                10
            );
        }
        assert!(analyze(&profiled).is_ok());
    }
}

#[cfg(feature = "codec-opus")]
#[test]
fn scheduler_clean_is_bit_exact_and_pauses_have_independent_evidence() {
    use audiokit_testkit::PauseConfig;
    let fixture = Fixture::new();
    let input = fixture.input(48_000, 1, 96_000);
    let mut c = bypass();
    c.scenario = Scenario::FileRoundtrip;
    c.retain_input = true;
    c.receive.clock_window_ms = 1000;
    let plain = fixture.path("plain");
    run(&c, &input, &plain, &Cancellation::default(), |_| {}).unwrap();
    c.scheduler.enabled = true;
    c.execution_profiling = true;
    let clean = fixture.path("clean");
    let d = run(&c, &input, &clean, &Cancellation::default(), |_| {}).unwrap();
    assert_eq!(
        fs::read(plain.join("processed.wav")).unwrap(),
        fs::read(clean.join("processed.wav")).unwrap()
    );
    assert!(d.checks.iter().all(|v| v.passed), "{:?}", d.checks);
    assert_eq!(d.latency["execution_profile"]["decode"]["calls"], 200);
    assert!(
        d.latency["execution_profile"]["pcm_admission"]["calls"]
            .as_u64()
            .unwrap()
            > 0
    );
    let pause = PauseConfig {
        start_ms: 400,
        duration_ms: 80,
    };
    c.scheduler.output_pause = pause;
    let d = run(
        &c,
        &input,
        &fixture.path("output"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    let s = &d.graph_statistics["scheduler"]["statistics"];
    assert_eq!(s["worker_paused_ticks"], 0);
    assert_eq!(s["output_paused_ticks"], 8);
    assert_eq!(s["output_underrun_frames"], 0);
    assert_eq!(s["output_overflow_frames"], 1920);
    assert_eq!(s["recovery_discarded_frames"], 1920);
    assert_eq!(d.graph_statistics["receive"]["clock_recoveries"], 0);
    assert!(
        !d.checks
            .iter()
            .find(|v| v.id == "output_queue_no_drop")
            .unwrap()
            .passed
    );
    assert!(
        d.checks
            .iter()
            .filter(|v| v.id != "output_queue_no_drop")
            .all(|v| v.passed),
        "{:?}",
        d.checks
    );
    c.scheduler.output_pause = Default::default();
    c.scheduler.worker_pause = pause;
    let worker = fixture.path("worker");
    let d = run(&c, &input, &worker, &Cancellation::default(), |_| {}).unwrap();
    let s = &d.graph_statistics["scheduler"]["statistics"];
    assert_eq!(s["worker_paused_ticks"], 8);
    assert_eq!(s["output_paused_ticks"], 0);
    assert_eq!(s["output_underrun_frames"], 3840);
    assert_eq!(s["output_overflow_frames"], 0);
    assert_eq!(
        s["first_healthy_consumption_after_worker_resume_ns"],
        480_000_000_u64
    );
    assert_eq!(d.graph_statistics["receive"]["clock_recoveries"], 1);
    assert_eq!(d.graph_statistics["transport"]["pending_copies"], 0);
    let clocks = d.graph_statistics["last_steady_source_clocks"]
        .as_array()
        .unwrap();
    assert_eq!(clocks[0]["applied_correction_ppm"], 0);
    assert_eq!(clocks[0]["inferred_device_drift_ppm"], 0.0);
    assert_eq!(clocks[0]["inferred_source_drift_ppm"], 0.0);
    assert!(
        d.checks
            .iter()
            .find(|v| v.id == "output_queue_conservation")
            .unwrap()
            .passed
    );
    assert!(
        !d.checks
            .iter()
            .find(|v| v.id == "output_consumer_no_underrun")
            .unwrap()
            .passed
    );
    let replayed = fixture.path("replayed");
    replay(&worker, None, &replayed, &Cancellation::default(), |_| {}).unwrap();
    assert_eq!(
        fs::read(worker.join("processed.wav")).unwrap(),
        fs::read(replayed.join("processed.wav")).unwrap()
    );
    let analysis = analyze(&worker).unwrap();
    assert!(
        analysis
            .evidence
            .iter()
            .any(|e| e.flags.iter().any(|f| f == "output_queue_underrun") && e.frames == 480)
    );
    assert!(
        analysis
            .evidence
            .iter()
            .any(|e| e.flags.iter().any(|f| f == "worker_clock_recovery"))
    );
    c.scheduler.worker_pause = Default::default();
    c.transport.stall_start_ms = 400;
    c.transport.stall_duration_ms = 200;
    let d = run(
        &c,
        &input,
        &fixture.path("network"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    let s = &d.graph_statistics["scheduler"]["statistics"];
    assert_eq!(s["worker_paused_ticks"], 0);
    assert_eq!(s["output_paused_ticks"], 0);
    assert_eq!(s["output_underrun_frames"], 0);
    assert_eq!(d.graph_statistics["receive"]["clock_recoveries"], 0);
    assert!(
        d.graph_statistics["receive"]["concealed_packets"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        d.graph_statistics["transport"]["stalled_packets"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[cfg(feature = "codec-opus")]
#[test]
fn scheduler_does_not_invent_plc_at_delayed_network_eof() {
    for ptime in [PacketDuration::Ms10, PacketDuration::Ms20] {
        let fixture = Fixture::new();
        let input = fixture.input(44_100, 2, 4801);
        let mut c = bypass();
        c.scenario = Scenario::FileRoundtrip;
        c.ptime = ptime;
        c.transport.delay_ms = 17;
        c.clocks.capture_rate_ppm = 400;
        c.clocks.render_rate_ppm = -100;
        let plain = fixture.path("plain");
        let d = run(&c, &input, &plain, &Cancellation::default(), |_| {}).unwrap();
        c.scheduler.enabled = true;
        let queued = fixture.path("queued");
        let q = run(&c, &input, &queued, &Cancellation::default(), |_| {}).unwrap();
        assert_eq!(
            fs::read(plain.join("processed.wav")).unwrap(),
            fs::read(queued.join("processed.wav")).unwrap()
        );
        assert_eq!(
            d.graph_statistics["receive"]["concealed_packets"],
            q.graph_statistics["receive"]["concealed_packets"]
        );
        assert_eq!(
            q.graph_statistics["scheduler"]["statistics"]["output_underrun_frames"],
            0
        );
    }
}

#[cfg(feature = "codec-opus")]
#[test]
fn scheduler_eof_pause_finishes_bounded_and_cancellation_keeps_partial_bundle() {
    use audiokit_testkit::PauseConfig;
    let fixture = Fixture::new();
    let input = fixture.input(48_000, 1, 4800);
    let mut c = bypass();
    c.scenario = Scenario::FileRoundtrip;
    c.scheduler.enabled = true;
    c.scheduler.worker_pause = PauseConfig {
        start_ms: 80,
        duration_ms: 100,
    };
    c.scheduler.output_pause = PauseConfig {
        start_ms: 80,
        duration_ms: 100,
    };
    let d = run(
        &c,
        &input,
        &fixture.path("eof"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        d.graph_statistics["scheduler"]["statistics"]["worker_resume_events"],
        1
    );
    assert_eq!(d.graph_statistics["scheduler"]["pending_frames"], 0);
    assert!(
        d.checks
            .iter()
            .find(|v| v.id == "output_queue_conservation")
            .unwrap()
            .passed
    );
    let stop = Cancellation::default();
    assert!(matches!(
        run(&c, &input, &fixture.path("cancelled"), &stop, |_| stop
            .cancel()),
        Err(Error::Cancelled)
    ));
    let a = analyze(&fixture.path("cancelled")).unwrap();
    assert!(a.integrity_verified);
    assert!(!a.recorded_checks_passed);
}

#[test]
fn mix_stress_covers_source_counts_protection_and_codec_free_plan() {
    for sources in [1, 2, 4, 8, 16, 32, 64] {
        let fixture = Fixture::new();
        let input = fixture.input(48_000, 1, 4800);
        let mut config = RunConfig::for_scenario(Scenario::MixStress);
        config.mix_stress.sources = sources;
        config.mix_stress.source_gain = 4.0;
        config.receive.render.max_sources = 64;
        config.max_trace_events = 1;
        let report = run(
            &config,
            &input,
            &fixture.path("mix"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(
            report.checks.iter().all(|check| check.passed),
            "{sources}: {:?}",
            report.checks
        );
        assert_eq!(report.plan.coverage, "render-stress");
        assert_eq!(
            report.graph_statistics["mix_stress"]["admitted_source_frames"],
            (4800 * sources) as u64
        );
        assert_eq!(
            report.graph_statistics["steady_render"]["max_active_voice"],
            sources
        );
        assert!(report.graph_statistics["receive"].is_null());
        assert!(
            report.latency["receive_execution"]["budgeted_calls"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(
            report
                .plan
                .stages
                .iter()
                .any(|node| node.id == "opus_encode"
                    && node.status == audiokit_testkit::NodeStatus::NotCovered)
        );
        assert_eq!(
            report.latency["render_algorithms"]["classification"],
            "estimated"
        );
        if sources >= 8 {
            assert!(
                report.graph_statistics["steady_render"]["max_pre_master_peak_q15"]
                    .as_u64()
                    .unwrap()
                    > 32768
            );
            assert!(
                report.graph_statistics["steady_render"]["max_gain_reduction_millidb"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
        }
    }
}

#[test]
fn silent_registered_sources_do_not_change_waveform_or_energy_normalization() {
    for stream in [StreamKind::Voice, StreamKind::Desktop] {
        let fixture = Fixture::new();
        let input = fixture.input(44_100, 2, 4411);
        let mut config = RunConfig::for_scenario(Scenario::MixStress);
        config.stream = stream;
        if stream == StreamKind::Desktop {
            config.bitrate_bps = 196_000;
        }
        config.mix_stress.sources = 1;
        config.retain_input = true;
        let baseline = run(
            &config,
            &input,
            &fixture.path("baseline"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        config.mix_stress.sources = 8;
        config.mix_stress.silent_sources = 7;
        let silent = run(
            &config,
            &input,
            &fixture.path("silent"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(silent.checks.iter().all(|c| c.passed));
        let trace: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.path("silent").join("trace.json")).unwrap())
                .unwrap();
        let metrics = &trace
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["stage"] == "render_output")
            .unwrap()["metrics"];
        let elapsed = metrics["worker_call_ns"].as_u64().unwrap();
        let budget = metrics["worker_budget_ns"].as_u64().unwrap();
        assert_eq!(budget, 10_000_000);
        assert_eq!(metrics["worker_over_budget"], elapsed > budget);
        assert_eq!(baseline.output_frames, silent.output_frames);
        assert!(
            compare(&fixture.path("baseline"), &fixture.path("silent"))
                .unwrap()
                .same_output_bytes
        );
        replay(
            &fixture.path("silent"),
            None,
            &fixture.path("replay"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(
            compare(&fixture.path("silent"), &fixture.path("replay"))
                .unwrap()
                .same_output_bytes
        );
    }
}

#[test]
fn mix_admission_work_limits_cancel_and_matrix_validation_are_explicit() {
    use audiokit_testkit::{ClockConfig, SweepMatrix, sweep};
    let fixture = Fixture::new();
    let input = fixture.input(48_000, 1, 1001);
    let mut config = RunConfig::for_scenario(Scenario::MixStress);
    config.mix_stress.sources = 2;
    config.receive.render.max_sources = 1;
    assert!(audiokit_testkit::plan_file(&config, &input).is_err());
    config.receive.render.max_sources = 64;
    config.mix_stress.max_total_source_frames = 1000;
    assert!(
        run(
            &config,
            &input,
            &fixture.path("budget"),
            &Cancellation::default(),
            |_| {}
        )
        .is_err()
    );
    assert!(!fixture.path("budget").exists());
    config.mix_stress.max_total_source_frames = 100_000;
    config.clocks = ClockConfig {
        render_rate_ppm: 1,
        ..Default::default()
    };
    assert!(config.validate().is_err());
    config.clocks = Default::default();
    let matrix = SweepMatrix {
        mix_sources: vec![1, 2],
        mix_silent_sources: vec![0, 1],
        ..Default::default()
    };
    assert!(matrix.expand(&config).is_err()); // One silent source with one total source is invalid.
    let matrix = SweepMatrix {
        mix_sources: vec![1, 4, 8],
        ..Default::default()
    };
    let summary = sweep(
        &config,
        &matrix,
        &input,
        &fixture.path("sweep"),
        &Cancellation::default(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(summary.cases.len(), 3);
    assert_eq!(summary.exit_code, 0);
    config.mix_stress.max_total_source_frames = 2000;
    assert!(
        sweep(
            &config,
            &matrix,
            &input,
            &fixture.path("invalid-sweep"),
            &Cancellation::default(),
            |_, _| {}
        )
        .is_err()
    );
    assert!(!fixture.path("invalid-sweep").exists());
    config.mix_stress.max_total_source_frames = 100_000;
    let stop = Cancellation::default();
    let cancel = stop.clone();
    assert!(matches!(
        run(&config, &input, &fixture.path("cancel"), &stop, |_| cancel
            .cancel()),
        Err(Error::Cancelled)
    ));
    assert!(analyze(&fixture.path("cancel")).is_ok());
}

#[cfg(feature = "codec-opus")]
#[test]
fn independent_clocks_are_inferred_with_correct_sign_and_clamped_or_disabled() {
    use audiokit_testkit::ClockConfig;
    for (capture, render, cap) in [
        (400, -100, 500),
        (-300, 200, 500),
        (2000, -2000, 100),
        (400, -100, 0),
    ] {
        let fixture = Fixture::new();
        let input = fixture.input(48_000, 1, 96_000);
        let mut config = bypass();
        config.scenario = Scenario::FileRoundtrip;
        config.clocks = ClockConfig {
            capture_rate_ppm: capture,
            render_rate_ppm: render,
        };
        config.receive.clock_window_ms = 1000;
        config.receive.clock_slew_ppm_per_second = 1000.0;
        config.receive.render.max_source_clock_correction_ppm = cap;
        config.max_trace_events = 1;
        let report = run(
            &config,
            &input,
            &fixture.path("clock"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        let clock = &report.graph_statistics["last_steady_source_clocks"][0];
        assert!(
            (clock["inferred_source_drift_ppm"].as_f64().unwrap() - f64::from(capture)).abs() < 1.0,
            "{clock}"
        );
        assert!(
            (clock["inferred_device_drift_ppm"].as_f64().unwrap() - f64::from(render)).abs() < 1.0,
            "{clock}"
        );
        let expected = (((1_000_000.0 + f64::from(render)) / (1_000_000.0 + f64::from(capture))
            - 1.0)
            * 1_000_000.0)
            .clamp(-f64::from(cap), f64::from(cap));
        assert!(
            (clock["applied_correction_ppm"].as_f64().unwrap() - expected).abs() <= 1.0,
            "{clock}"
        );
        assert!(
            report.graph_statistics["steady_render"]["max_abs_clock_correction_ppm"]
                .as_u64()
                .unwrap()
                <= u64::from(cap)
        );
        assert_eq!(report.graph_statistics["receive"]["decode_errors"], 0);
        assert!(report.trace_events_dropped > 0);
    }
}

#[test]
fn fault_configuration_and_sweep_axes_reject_uncovered_or_unbounded_work() {
    use audiokit_testkit::{SweepMatrix, TransportConfig};
    let mut config = bypass();
    config.transport.delay_ms = 1;
    assert!(config.validate().is_err());
    for transport in [
        TransportConfig {
            loss_per_mille: 1001,
            ..Default::default()
        },
        TransportConfig {
            max_pending_packets: 4097,
            ..Default::default()
        },
        TransportConfig {
            reorder_every: 2,
            ..Default::default()
        },
        TransportConfig {
            stall_start_ms: 1,
            ..Default::default()
        },
    ] {
        assert!(transport.validate().is_err());
    }
    config = bypass();
    assert!(
        SweepMatrix {
            bitrates_bps: vec![64_000],
            ..Default::default()
        }
        .expand(&config)
        .is_err()
    );
    assert!(
        SweepMatrix {
            noise_levels: vec![audiokit_testkit::NoiseLevel::Low],
            ..Default::default()
        }
        .expand(&config)
        .is_err()
    );
    assert!(
        SweepMatrix {
            max_total_output_samples: 1,
            ..Default::default()
        }
        .expand(&RunConfig {
            max_pcm_samples: 2,
            ..config.clone()
        })
        .is_err()
    );
    config.max_pcm_samples = usize::MAX;
    assert!(SweepMatrix::default().expand(&config).is_err());
}

#[test]
fn sweep_budget_preflight_and_cancellation_never_overwrite_results() {
    use audiokit_testkit::{SweepMatrix, sweep};
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 1001);
    let mut config = bypass();
    config.max_pcm_samples = 2000;
    let matrix = SweepMatrix::default();
    let output = fixture.path("sweep");
    let result = sweep(
        &config,
        &matrix,
        &source,
        &output,
        &Cancellation::default(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.cases.len(), 1);
    assert!(
        analyze(&output.join("case-000"))
            .unwrap()
            .recorded_checks_passed
    );
    let original = fs::read(output.join("sweep.json")).unwrap();
    assert!(
        sweep(
            &config,
            &matrix,
            &source,
            &output,
            &Cancellation::default(),
            |_, _| {}
        )
        .is_err()
    );
    assert_eq!(original, fs::read(output.join("sweep.json")).unwrap());
    let too_small = SweepMatrix {
        max_total_artifact_bytes: 1024,
        ..Default::default()
    };
    assert!(
        sweep(
            &config,
            &too_small,
            &source,
            &fixture.path("budget"),
            &Cancellation::default(),
            |_, _| {}
        )
        .is_err()
    );
    assert!(!fixture.path("budget").exists());
    let too_short = SweepMatrix {
        max_input_duration_ms: 1,
        ..Default::default()
    };
    assert!(
        sweep(
            &config,
            &too_short,
            &source,
            &fixture.path("duration"),
            &Cancellation::default(),
            |_, _| {}
        )
        .is_err()
    );
    assert!(!fixture.path("duration").exists());
    let stop = Cancellation::default();
    stop.cancel();
    let result = sweep(
        &config,
        &matrix,
        &source,
        &fixture.path("cancel"),
        &stop,
        |_, _| {},
    )
    .unwrap();
    assert_eq!(result.exit_code, 130);
    assert!(result.cases.is_empty());
    assert!(fixture.path("cancel/base-config.json").is_file());
    assert!(!result.source_digest.is_empty());
    assert_eq!(
        json(&fixture.path("cancel/sweep.json"))["status"],
        "cancelled"
    );
}

#[cfg(feature = "codec-opus")]
#[test]
fn injected_loss_jitter_reorder_stall_is_reproducible_even_with_truncated_trace() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 96_000);
    let mut config = bypass();
    config.scenario = Scenario::FileRoundtrip;
    config.retain_input = true;
    config.max_trace_events = 1;
    config.transport = audiokit_testkit::TransportConfig {
        seed: 233,
        delay_ms: 20,
        jitter_ms: 15,
        loss_per_mille: 200,
        duplicate_per_mille: 100,
        reorder_every: 7,
        reorder_delay_ms: 100,
        stall_start_ms: 600,
        stall_duration_ms: 200,
        ..Default::default()
    };
    let first = run(
        &config,
        &source,
        &fixture.path("first"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    let stats = &first.graph_statistics;
    assert!(
        stats["transport"]["intentionally_dropped"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(stats["transport"]["stalled_packets"].as_u64().unwrap() > 0);
    assert!(
        stats["transport"]["reorder_selected_packets"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(stats["transport"]["pending_copies"], 0);
    assert!(
        stats["receive"]["concealed_packets"].as_u64().unwrap()
            + stats["receive"]["fec_attempts"].as_u64().unwrap()
            > 0
    );
    assert!(
        first
            .checks
            .iter()
            .find(|c| c.id == "transport_accounting")
            .unwrap()
            .passed
    );
    assert!(
        !first
            .checks
            .iter()
            .any(|c| c.id == "lossless_virtual_transport")
    );
    assert!(first.trace_events_dropped > 0);
    let second = replay(
        &fixture.path("first"),
        None,
        &fixture.path("second"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        first.graph_statistics["transport"],
        second.graph_statistics["transport"]
    );
    assert!(
        compare(&fixture.path("first"), &fixture.path("second"))
            .unwrap()
            .same_output_bytes
    );
    assert!(
        analyze(&fixture.path("first"))
            .unwrap()
            .observations
            .iter()
            .any(|s| s.contains("virtual faults enabled"))
    );
}

#[cfg(feature = "codec-opus")]
#[test]
fn duplicates_do_not_advance_decoder_and_all_loss_is_not_healthy_media() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 96_000);
    let mut config = bypass();
    config.scenario = Scenario::FileRoundtrip;
    config.transport.duplicate_per_mille = 1000;
    let duplicate = run(
        &config,
        &source,
        &fixture.path("duplicate"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        duplicate.graph_statistics["receive"]["decoded_packets"],
        duplicate.graph_statistics["capture"]["packets"]
    );
    assert_eq!(
        duplicate.graph_statistics["receive"]["duplicates"],
        duplicate.graph_statistics["capture"]["packets"]
    );
    assert!(duplicate.checks.iter().all(|c| c.passed));
    let analysis = analyze(&fixture.path("duplicate")).unwrap();
    assert_eq!(analysis.evidence.len(), 64);
    assert!(analysis.evidence_omitted > 0);
    assert!(
        analysis
            .evidence
            .iter()
            .any(|event| event.stage == "transport_arrival"
                && event.flags.contains(&"receiver_duplicate".into()))
    );
    assert!(
        analysis
            .evidence
            .iter()
            .filter(|event| event.flags.contains(&"receiver_duplicate".into()))
            .all(|event| event.clock_domain == "capture_output")
    );
    config.transport.loss_per_mille = 1000;
    let lost = run(
        &config,
        &source,
        &fixture.path("lost"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap();
    assert_eq!(lost.graph_statistics["receive"]["decoded_packets"], 0);
    let analysis = analyze(&fixture.path("lost")).unwrap();
    assert!(
        analysis
            .evidence
            .iter()
            .any(|event| event.flags.contains(&"injection_drop".into()))
    );
    assert!(
        !lost
            .checks
            .iter()
            .find(|c| c.id == "received_media")
            .unwrap()
            .passed
    );
    assert!(
        !analyze(&fixture.path("lost"))
            .unwrap()
            .recorded_checks_passed
    );
}

#[cfg(feature = "codec-opus")]
#[test]
fn transport_queue_failure_and_cancel_preserve_pending_evidence() {
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 9600);
    let mut config = bypass();
    config.scenario = Scenario::FileRoundtrip;
    config.transport.delay_ms = 200;
    config.transport.max_pending_packets = 1;
    let error = run(
        &config,
        &source,
        &fixture.path("budget"),
        &Cancellation::default(),
        |_| {},
    )
    .unwrap_err();
    assert_eq!(error.exit_code(), 4);
    assert_eq!(
        json(&fixture.path("budget/diagnostics.json"))["graph_statistics"]["transport"]["pending_copies"],
        1
    );
    assert!(analyze(&fixture.path("budget")).is_ok());
    config.transport.max_pending_packets = 1024;
    let stop = Cancellation::default();
    let cancel = stop.clone();
    let error = run(&config, &source, &fixture.path("cancel"), &stop, |event| {
        if event.input_frames >= 4800 {
            cancel.cancel();
        }
    })
    .unwrap_err();
    assert_eq!(error.exit_code(), 130);
    let stats = json(&fixture.path("cancel/diagnostics.json"));
    assert!(
        stats["graph_statistics"]["transport"]["pending_copies"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(stats["status"], "cancelled");
}

#[cfg(feature = "codec-opus")]
#[test]
fn cartesian_sweep_is_serial_and_checks_all_cases_before_creating_output() {
    use audiokit_testkit::{SweepMatrix, sweep};
    let fixture = Fixture::new();
    let source = fixture.input(48_000, 1, 9600);
    let mut config = bypass();
    config.scenario = Scenario::FileRoundtrip;
    config.max_pcm_samples = 48_000;
    let matrix = SweepMatrix {
        bitrates_bps: vec![64_000, 96_000],
        ptimes: vec![PacketDuration::Ms10, PacketDuration::Ms20],
        ..Default::default()
    };
    let configs = matrix.expand(&config).unwrap();
    assert_eq!(configs.len(), 4);
    assert_eq!(configs[0].ptime, PacketDuration::Ms10);
    assert_eq!(configs[1].ptime, PacketDuration::Ms20);
    assert_eq!(configs[2].bitrate_bps, 96_000);
    let result = sweep(
        &config,
        &matrix,
        &source,
        &fixture.path("sweep"),
        &Cancellation::default(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.cases.len(), 4);
    for case in &result.cases {
        assert!(
            analyze(&fixture.path("sweep").join(&case.bundle))
                .unwrap()
                .recorded_checks_passed
        );
    }
    let invalid = SweepMatrix {
        ptimes: vec![PacketDuration::Ms20],
        bitrates_bps: vec![96_000, 320_000],
        ..Default::default()
    };
    assert!(
        sweep(
            &config,
            &invalid,
            &source,
            &fixture.path("invalid"),
            &Cancellation::default(),
            |_, _| {}
        )
        .is_err()
    );
    assert!(!fixture.path("invalid").exists());
    let too_many = SweepMatrix {
        max_cases: 3,
        ..matrix.clone()
    };
    assert!(too_many.expand(&config).is_err());
    let stop = Cancellation::default();
    let cancel = stop.clone();
    let result = sweep(
        &config,
        &matrix,
        &source,
        &fixture.path("partial"),
        &stop,
        |case, _| {
            if case == 0 {
                cancel.cancel();
            }
        },
    )
    .unwrap();
    assert_eq!(result.exit_code, 130);
    assert_eq!(result.cases.len(), 1);
    assert!(analyze(&fixture.path("partial/case-000")).is_ok());
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
