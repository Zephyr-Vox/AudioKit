//! Real-codec external replay, material integrity and bounded metadata-only validation.
use audiokit::{AudioFormat, ChannelLayout, PacketDuration, StreamKind};
use audiokit_testkit::{PacketSource, PacketTrace, RecordedPacket, RunConfig, Scenario};

fn minimal() -> (RunConfig, PacketTrace) {
    let config = RunConfig::for_scenario(Scenario::ReceiveSimulation);
    let trace = PacketTrace {
        schema_version: 1,
        source: PacketSource {
            source_id: 7,
            stream_id: 3,
            epoch: 11,
            kind: StreamKind::Voice,
            format: AudioFormat::new(48_000, ChannelLayout::Mono).unwrap(),
            ptime: PacketDuration::Ms20,
        },
        starts_at_stream_start: true,
        omitted_packets: Some(0),
        omitted_render_ticks: Some(0),
        packets: vec![RecordedPacket {
            sequence: 65535,
            arrival_ns: 20_000_000,
            duration: PacketDuration::Ms20,
            media_frame: None,
            payload: Some(vec![1, 2]),
        }],
        render_ticks_ns: vec![10_000_000, 20_000_000],
    };
    (config, trace)
}

#[test]
fn packet_validation_rejects_inconsistent_formats_times_ranges_and_work() {
    let (config, trace) = minimal();
    trace.validate(&config).unwrap();
    assert!(trace.is_complete()); // Material completeness is not decoder validity.
    let mut bad = trace.clone();
    bad.source.source_id = 0;
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.source.format = AudioFormat::new(48_000, ChannelLayout::Stereo).unwrap();
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.source.ptime = PacketDuration::Ms10;
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.packets[0].duration = PacketDuration::Ms10;
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.packets[0].media_frame = Some(u64::MAX);
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.packets[0].payload = Some(vec![]);
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.packets[0].payload = Some(vec![0; 4001]);
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.render_ticks_ns = vec![20_000_000, 10_000_000];
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.render_ticks_ns = vec![10_000_000, 10_000_000];
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.packets[0].arrival_ns = 21_000_000;
    assert!(bad.validate(&config).is_err());
    let mut bad = trace.clone();
    bad.packets.insert(0, bad.packets[0].clone());
    bad.packets[0].arrival_ns = 21_000_000;
    assert!(bad.validate(&config).is_err());
    let mut config = config.clone();
    config.receive.max_payload_bytes = 1;
    assert!(trace.validate(&config).is_err());
    config.receive.max_payload_bytes = 4000;
    config.receive_simulation.max_packets = 1;
    let mut two = trace.clone();
    two.packets.push(two.packets[0].clone());
    assert!(two.validate(&config).is_err());
    config.receive_simulation.max_packets = 8192;
    config.receive_simulation.max_render_ticks = 1;
    assert!(trace.validate(&config).is_err());
    config.receive_simulation.max_render_ticks = 2;
    config.receive_simulation.max_duration_ms = 19;
    assert!(trace.validate(&config).is_err());
    config.receive_simulation.max_duration_ms = 20;
    config.max_pcm_samples = 1;
    assert!(trace.validate(&config).is_err());
    let (config, mut trace) = minimal();
    trace.packets[0].payload = None;
    trace.validate(&config).unwrap();
    assert!(!trace.is_complete());
    trace.packets.clear();
    trace.starts_at_stream_start = false;
    assert!(!trace.is_complete());
    trace.starts_at_stream_start = true;
    trace.omitted_packets = None;
    assert!(!trace.is_complete());
    trace.omitted_packets = Some(0);
    trace.omitted_render_ticks = Some(1);
    assert!(!trace.is_complete());
    let mut value = serde_json::to_value(&trace).unwrap();
    value["account_name"] = serde_json::json!("not accepted");
    assert!(serde_json::from_value::<PacketTrace>(value).is_err());
    assert!(
        audiokit_testkit::SweepMatrix::default()
            .expand(&config)
            .is_err()
    );
    #[cfg(not(feature = "codec-opus"))]
    assert!(matches!(
        config.validate(),
        Err(audiokit_testkit::Error::Capability(_))
    ));
}

#[cfg(feature = "codec-opus")]
mod codec {
    use super::*;
    use audiokit::graph::capture::{CaptureGraph, CaptureGraphConfig};
    use audiokit_codec_opus::{OpusConfig, OpusEncoder, OpusProfile};
    use audiokit_testkit::{Cancellation, Error, analyze, compare, replay, run};
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "audiokit-packets-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
        fn save(&self, trace: &PacketTrace) -> PathBuf {
            let path = self.path("recording.json");
            fs::write(&path, serde_json::to_vec(trace).unwrap()).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn recording(fixture: &Fixture, kind: StreamKind) -> (RunConfig, PacketTrace) {
        let (mut config, mut trace) = minimal();
        config.stream = kind;
        config.retain_input = true;
        trace.source.kind = kind;
        trace.source.format = AudioFormat::new(
            48_000,
            if kind == StreamKind::Voice {
                ChannelLayout::Mono
            } else {
                ChannelLayout::Stereo
            },
        )
        .unwrap();
        if kind == StreamKind::Desktop {
            config.bitrate_bps = 196_000;
        }
        let format = trace.source.format;
        let mut encoder = OpusEncoder::new(
            format,
            config.ptime,
            if kind == StreamKind::Voice {
                OpusProfile::Voice
            } else {
                OpusProfile::Desktop
            },
            OpusConfig::default(),
            config.max_payload_bytes,
        )
        .unwrap();
        encoder.set_target_bitrate_bps(config.bitrate_bps).unwrap();
        let mut capture = CaptureGraph::new(
            CaptureGraphConfig {
                input_format: format,
                kind,
                resampler: config.resampler,
                max_ingress_ms: 200,
                max_payload_bytes: config.max_payload_bytes,
            },
            Box::new(encoder),
            None,
        )
        .unwrap();
        let mut wav = hound::WavWriter::create(
            fixture.path("source.wav"),
            hound::WavSpec {
                sample_rate: 48_000,
                channels: u16::from(format.channels()),
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        trace.packets.clear();
        trace.render_ticks_ns.clear();
        let mut sequence = 0_u16;
        for tick in 1..=100_u64 {
            let now = tick * 10_000_000;
            trace.render_ticks_ns.push(now);
            let mut block = Vec::new();
            for i in (tick - 1) * 480..tick * 480 {
                for channel in 0..format.channels() {
                    let sample = (i as f32 * std::f32::consts::TAU * 997.0 / 48_000.0).sin()
                        * (0.2 + f32::from(channel) * 0.05);
                    block.push(sample);
                    wav.write_sample(sample).unwrap();
                }
            }
            for packet in capture.push_native(&block).unwrap() {
                trace.packets.push(RecordedPacket {
                    sequence,
                    arrival_ns: now,
                    duration: config.ptime,
                    media_frame: Some(packet.sample_position),
                    payload: Some(packet.payload),
                });
                sequence = sequence.wrapping_add(1);
            }
        }
        for packet in capture.finish().unwrap() {
            trace.packets.push(RecordedPacket {
                sequence,
                arrival_ns: 1_000_000_000,
                duration: config.ptime,
                media_frame: Some(packet.sample_position),
                payload: Some(packet.payload),
            });
            sequence = sequence.wrapping_add(1);
        }
        wav.finalize().unwrap();
        trace.validate(&config).unwrap();
        (config, trace)
    }
    #[test]
    fn voice_desktop_packet_replay_matches_roundtrip_and_preserves_anonymous_identity() {
        for kind in [StreamKind::Voice, StreamKind::Desktop] {
            let fixture = Fixture::new();
            let (config, trace) = recording(&fixture, kind);
            let path = fixture.save(&trace);
            let report = run(
                &config,
                &path,
                &fixture.path("receive"),
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();
            assert!(
                report.checks.iter().all(|c| c.passed),
                "{:?}",
                report.checks
            );
            assert_eq!(report.plan.coverage, "receiver-replay");
            assert_eq!(report.input_frames, 0);
            assert!(report.graph_statistics["capture"].is_null());
            assert_eq!(
                report.latency["capture_resampler"]["classification"],
                "not_covered"
            );
            assert_eq!(
                report.latency["encoded_startup"]["classification"],
                "configured"
            );
            let events: Vec<audiokit_testkit::TraceEvent> =
                serde_json::from_slice(&fs::read(fixture.path("receive/trace.json")).unwrap())
                    .unwrap();
            let packet = events.iter().find(|e| e.stage == "packet_input").unwrap();
            assert_eq!(
                (packet.source_id, packet.stream_id, packet.epoch),
                (Some(7), Some(3), Some(11))
            );
            assert!(!fixture.path("receive/input.wav").exists());
            assert_eq!(
                analyze(&fixture.path("receive")).unwrap().reproduction,
                "packet-replay"
            );
            replay(
                &fixture.path("receive"),
                None,
                &fixture.path("replayed"),
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();
            assert!(
                compare(&fixture.path("receive"), &fixture.path("replayed"))
                    .unwrap()
                    .same_output_bytes
            );
            let mut original = config.clone();
            original.scenario = Scenario::FileRoundtrip;
            run(
                &original,
                &fixture.path("source.wav"),
                &fixture.path("roundtrip"),
                &Cancellation::default(),
                |_| {},
            )
            .unwrap();
            assert!(
                compare(&fixture.path("receive"), &fixture.path("roundtrip"))
                    .unwrap()
                    .same_output_bytes
            );
        }
    }
    #[test]
    fn wrapping_duplicates_and_equal_time_order_do_not_advance_decoder_twice() {
        let fixture = Fixture::new();
        let (config, mut trace) = recording(&fixture, StreamKind::Voice);
        for packet in &mut trace.packets {
            packet.sequence = packet.sequence.wrapping_add(65_530);
        }
        let path = fixture.save(&trace);
        let clean = run(
            &config,
            &path,
            &fixture.path("clean"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        trace.packets.insert(11, trace.packets[10].clone());
        let path = fixture.save(&trace);
        let dup = run(
            &config,
            &path,
            &fixture.path("duplicate"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(dup.graph_statistics["receive"]["duplicates"], 1);
        assert_eq!(
            clean.graph_statistics["receive"]["decoded_packets"],
            dup.graph_statistics["receive"]["decoded_packets"]
        );
        assert!(
            compare(&fixture.path("clean"), &fixture.path("duplicate"))
                .unwrap()
                .same_output_bytes
        );
        let analysis = analyze(&fixture.path("duplicate")).unwrap();
        assert!(
            analysis
                .evidence
                .iter()
                .any(|e| e.flags.iter().any(|f| f == "receiver_duplicate"))
        );
    }
    #[test]
    fn arrival_reorder_irregular_demand_and_network_loss_keep_recorded_timeline() {
        let fixture = Fixture::new();
        let (config, mut trace) = recording(&fixture, StreamKind::Voice);
        let arrival = trace.packets[10].arrival_ns;
        trace.packets.swap(9, 10);
        trace.packets[9].arrival_ns = arrival;
        trace.packets[10].arrival_ns = arrival;
        trace.render_ticks_ns[29] += 5_000_000;
        let path = fixture.save(&trace);
        let report = run(
            &config,
            &path,
            &fixture.path("timing"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert_eq!(report.graph_statistics["receive"]["decode_errors"], 0);
        assert_eq!(
            report.graph_statistics["packet_replay"]["pending_records"],
            0
        );
        let events: Vec<audiokit_testkit::TraceEvent> =
            serde_json::from_slice(&fs::read(fixture.path("timing/trace.json")).unwrap()).unwrap();
        let times: Vec<u64> = events
            .iter()
            .filter(|e| e.stage == "render_output")
            .map(|e| e.time_ns)
            .collect();
        assert_eq!(times, trace.render_ticks_ns);
        let arrivals: Vec<u64> = events
            .iter()
            .filter(|e| e.stage == "packet_input")
            .map(|e| e.time_ns)
            .collect();
        assert_eq!(
            arrivals,
            trace
                .packets
                .iter()
                .map(|p| p.arrival_ns)
                .collect::<Vec<_>>()
        );
        let seq: Vec<u64> = events
            .iter()
            .filter(|e| e.stage == "packet_input")
            .map(|e| e.metrics["sequence"].as_u64().unwrap())
            .collect();
        assert!(seq[9] > seq[10]);
        replay(
            &fixture.path("timing"),
            None,
            &fixture.path("timing-replay"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(
            compare(&fixture.path("timing"), &fixture.path("timing-replay"))
                .unwrap()
                .same_output_bytes
        );
        trace.packets.remove(5); // A real never-arrived packet is not a missing recording payload.
        let path = fixture.save(&trace);
        let lost = run(
            &config,
            &path,
            &fixture.path("loss"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        // A single missing sequence may invoke FEC instead of PLC. Neither an
        // attempted recovery nor a counter alone certifies perceptual quality.
        assert!(
            lost.graph_statistics["receive"]["concealed_packets"]
                .as_u64()
                .unwrap()
                > 0
                || lost.graph_statistics["receive"]["fec_attempts"]
                    .as_u64()
                    .unwrap()
                    > 0
        );
        assert_eq!(
            lost.graph_statistics["packet_recording"]["missing_payloads"],
            0
        );
        assert_eq!(
            analyze(&fixture.path("loss")).unwrap().reproduction,
            "packet-replay"
        );
    }
    #[test]
    fn absent_material_and_midstream_state_are_not_complete_reproduction() {
        let fixture = Fixture::new();
        let (config, mut trace) = recording(&fixture, StreamKind::Voice);
        trace.packets[10].payload = None;
        trace.packets[10].media_frame = None;
        trace.starts_at_stream_start = false;
        trace.omitted_render_ticks = None;
        trace.omitted_packets = Some(2);
        let path = fixture.save(&trace);
        let report = run(
            &config,
            &path,
            &fixture.path("partial"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.id == "packet_material_complete")
                .unwrap()
                .passed
        );
        assert_eq!(
            report.graph_statistics["packet_replay"]["missing_payloads"],
            1
        );
        let analysis = analyze(&fixture.path("partial")).unwrap();
        assert_eq!(analysis.reproduction, "partial-packet-replay");
        assert!(!analysis.recorded_checks_passed);
        assert!(
            analysis
                .evidence
                .iter()
                .any(|e| e.clock_domain == "media_position_unknown"
                    && e.frames == 0
                    && e.flags.iter().any(|f| f == "recording_payload_missing"))
        );
        replay(
            &fixture.path("partial"),
            None,
            &fixture.path("partial-replay"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(
            compare(&fixture.path("partial"), &fixture.path("partial-replay"))
                .unwrap()
                .same_output_bytes
        );
    }
    #[test]
    fn metadata_requires_original_packet_bytes_and_tampering_is_rejected() {
        let fixture = Fixture::new();
        let (mut config, trace) = recording(&fixture, StreamKind::Voice);
        config.retain_input = false;
        let path = fixture.save(&trace);
        run(
            &config,
            &path,
            &fixture.path("metadata"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(!fixture.path("metadata/packets.json").exists());
        assert_eq!(
            analyze(&fixture.path("metadata")).unwrap().reproduction,
            "metadata-only"
        );
        assert!(
            replay(
                &fixture.path("metadata"),
                None,
                &fixture.path("missing"),
                &Cancellation::default(),
                |_| {}
            )
            .is_err()
        );
        replay(
            &fixture.path("metadata"),
            Some(&path),
            &fixture.path("external"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(
            compare(&fixture.path("metadata"), &fixture.path("external"))
                .unwrap()
                .same_output_bytes
        );
        config.retain_input = true;
        run(
            &config,
            &path,
            &fixture.path("retained"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        let mut changed = trace;
        changed.packets[0].sequence = 42;
        let path = fixture.save(&changed);
        assert!(
            replay(
                &fixture.path("metadata"),
                Some(&path),
                &fixture.path("mismatch"),
                &Cancellation::default(),
                |_| {}
            )
            .is_err()
        );
        fs::write(
            fixture.path("retained/packets.json"),
            serde_json::to_vec(&changed).unwrap(),
        )
        .unwrap();
        assert!(analyze(&fixture.path("retained")).is_err());
        let huge = fixture.path("huge.json");
        fs::File::create(&huge)
            .unwrap()
            .set_len(32 * 1024 * 1024 + 1)
            .unwrap();
        assert!(matches!(
            audiokit_testkit::plan_file(&config, &huge),
            Err(Error::Invalid(_))
        ));
    }
    #[test]
    fn cancel_output_tail_budget_and_corrupt_payload_preserve_diagnostics() {
        let fixture = Fixture::new();
        let (mut config, mut trace) = recording(&fixture, StreamKind::Voice);
        let path = fixture.save(&trace);
        let stop = Cancellation::default();
        let cancel = stop.clone();
        assert!(matches!(
            run(&config, &path, &fixture.path("cancelled"), &stop, |_| {
                cancel.cancel()
            }),
            Err(Error::Cancelled)
        ));
        assert!(analyze(&fixture.path("cancelled")).is_ok());
        config.max_pcm_samples = trace.render_ticks_ns.len() * 960;
        assert!(matches!(
            run(
                &config,
                &path,
                &fixture.path("budget"),
                &Cancellation::default(),
                |_| {}
            ),
            Err(Error::Execution(_))
        ));
        assert!(analyze(&fixture.path("budget")).is_ok());
        config.max_pcm_samples = 16 * 1024 * 1024;
        trace.packets[10].payload = Some(vec![255, 255, 255]);
        let path = fixture.save(&trace);
        let report = run(
            &config,
            &path,
            &fixture.path("corrupt"),
            &Cancellation::default(),
            |_| {},
        )
        .unwrap();
        assert!(
            report.graph_statistics["receive"]["decode_errors"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(
            !report
                .checks
                .iter()
                .find(|c| c.id == "decoder_no_errors")
                .unwrap()
                .passed
        );
        assert!(
            !analyze(&fixture.path("corrupt"))
                .unwrap()
                .recorded_checks_passed
        );
    }
}
