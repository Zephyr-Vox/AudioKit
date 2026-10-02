//! SDK-free production-graph regression, driven by deterministic virtual device demand.
#![cfg(feature = "resampling")]
use audiokit::backend::{AudioDecoder, AudioEncoder, DecodeRequest};
use audiokit::graph::{GraphState, capture::*, receive::*, render::*};
use audiokit::{
    AudioError, AudioFormat, AudioResult, ChannelLayout, PacketDuration, SampleFrames, SourceId,
    SourceKey, StreamEpoch, StreamId, StreamKind,
};
use std::sync::{Arc, Mutex};

fn format(channels: u8) -> AudioFormat {
    AudioFormat::new(
        48_000,
        if channels == 1 {
            ChannelLayout::Mono
        } else {
            ChannelLayout::Stereo
        },
    )
    .unwrap()
}
fn registration(id: u64, kind: StreamKind) -> SourceRegistration {
    SourceRegistration {
        key: SourceKey {
            source: SourceId::new(id).unwrap(),
            stream: StreamId::new(1).unwrap(),
        },
        epoch: StreamEpoch(1),
        format: format(if kind == StreamKind::Desktop { 2 } else { 1 }),
        kind,
    }
}
struct Encoder {
    format: AudioFormat,
    fail: bool,
}
impl AudioEncoder for Encoder {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn packet_duration(&self) -> PacketDuration {
        PacketDuration::Ms20
    }
    fn lookahead(&self) -> Option<SampleFrames> {
        Some(SampleFrames::new(0))
    }
    fn encode_into(&mut self, pcm: &[f32], out: &mut [u8]) -> AudioResult<usize> {
        if self.fail {
            return Err(AudioError::Processing("injected codec failure".into()));
        }
        out[..4].copy_from_slice(&pcm[0].to_le_bytes());
        Ok(4)
    }
}
#[derive(Clone)]
struct Decoder {
    format: AudioFormat,
    log: Arc<Mutex<Vec<String>>>,
    wrong_frames: bool,
}
impl AudioDecoder for Decoder {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn packet_duration(&self) -> PacketDuration {
        PacketDuration::Ms20
    }
    fn reset(&mut self) -> AudioResult<()> {
        self.log.lock().unwrap().push("reset".into());
        Ok(())
    }
    fn decode_into(
        &mut self,
        request: DecodeRequest<'_>,
        out: &mut [f32],
    ) -> AudioResult<SampleFrames> {
        let (label, value) = match request {
            DecodeRequest::Packet(p) => ("normal", f32::from_le_bytes(p.try_into().unwrap())),
            DecodeRequest::Fec(p) => ("fec", f32::from_le_bytes(p.try_into().unwrap())),
            DecodeRequest::Loss => ("plc", 0.0),
        };
        self.log.lock().unwrap().push(label.into());
        out.fill(value);
        Ok(SampleFrames::new(if self.wrong_frames {
            1
        } else {
            out.len() as u64 / u64::from(self.format.channels())
        }))
    }
}
fn packet(source: SourceRegistration, sequence: u16, arrival_ns: u64) -> EncodedPacket {
    EncodedPacket {
        source: source.key,
        epoch: source.epoch,
        sequence,
        duration: PacketDuration::Ms20,
        arrival_ns,
        payload: 0.2_f32.to_le_bytes().to_vec(),
    }
}
fn receiver(source: SourceRegistration) -> (ReceiveGraph, Arc<Mutex<Vec<String>>>) {
    let mut graph = ReceiveGraph::new(ReceiveGraphConfig {
        jitter: JitterBufferConfig {
            target_ms: 20,
            in_band_fec: true,
        },
        ..Default::default()
    })
    .unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    graph
        .register(
            source,
            Box::new(Decoder {
                format: source.format,
                log: Arc::clone(&log),
                wrong_frames: false,
            }),
        )
        .unwrap();
    (graph, log)
}

#[test]
fn recovery_and_epoch_replacement_do_not_unmute_a_source() {
    let mut source = registration(1, StreamKind::Voice);
    let (mut graph, log) = receiver(source);
    graph.set_gain(source.key, 0.0).unwrap();
    let mut output = [0.0; 1920];
    let mut now = 0;
    for boundary in 0..3 {
        match boundary {
            0 => {
                source.epoch = StreamEpoch(2);
                graph
                    .register(
                        source,
                        Box::new(Decoder {
                            format: source.format,
                            log: Arc::clone(&log),
                            wrong_frames: false,
                        }),
                    )
                    .unwrap();
            }
            1 => {
                assert_eq!(
                    graph.push_packet(packet(source, 1000, now)).unwrap(),
                    PacketOutcome::Resynchronized
                );
            }
            _ => graph.recover_clock().unwrap(),
        }
        let sequence = if boundary == 1 { 1000 } else { 0 };
        for offset in 0..4 {
            graph
                .push_packet(packet(source, sequence + offset, now))
                .unwrap();
        }
        for _ in 0..3 {
            now += 20_000_000;
            let metrics = graph.render_into(&mut output, now).unwrap();
            assert_eq!(metrics.sources[0].gain, 0.0);
            assert!(!metrics.sources[0].active);
            assert!(output.iter().all(|sample| *sample == 0.0));
        }
    }
    assert!(log.lock().unwrap().iter().any(|kind| kind == "normal"));
}

#[test]
fn receive_fifo_admission_covers_the_maximum_demand_before_decoding() {
    let source = registration(1, StreamKind::Voice);
    let log = Arc::new(Mutex::new(Vec::new()));
    for (capacity, admitted) in [(80, false), (110, true)] {
        let mut graph = ReceiveGraph::new(ReceiveGraphConfig {
            render: RenderGraphConfig {
                max_render_ms: 60,
                max_source_queue_ms: capacity,
                ..Default::default()
            },
            jitter: JitterBufferConfig {
                target_ms: 20,
                in_band_fec: true,
            },
            ..Default::default()
        })
        .unwrap();
        let result = graph.register(
            source,
            Box::new(Decoder {
                format: source.format,
                log: Arc::clone(&log),
                wrong_frames: false,
            }),
        );
        assert_eq!(result.is_ok(), admitted);
        assert_eq!(graph.source_count(), usize::from(admitted));
        assert!(log.lock().unwrap().is_empty());
        if admitted {
            for sequence in 0..3 {
                graph.push_packet(packet(source, sequence, 0)).unwrap();
            }
            graph.render_into(&mut [0.0; 5760], 60_000_000).unwrap();
            assert_eq!(*log.lock().unwrap(), ["normal", "normal", "normal"]);
        }
    }
}

#[test]
fn render_eof_rate_padding_is_reported_once_and_is_not_an_underrun() {
    let source = registration(1, StreamKind::Voice);
    let mut graph = RenderGraph::new(Default::default()).unwrap();
    graph.register(source).unwrap();
    graph
        .push_pcm(source.key, source.epoch, &[0.2; 135])
        .unwrap();
    graph.begin_drain().unwrap();
    graph.begin_drain().unwrap();
    let metrics = graph.render_into(&mut [0.0; 1920]).unwrap();
    assert_eq!(metrics.sources[0].rate_eof_padding_frames, 345);
    assert_eq!(metrics.sources[0].missing_frames, 0);
}

#[test]
fn capture_is_chunk_invariant_and_accounts_eof_once() {
    let input = vec![0.25; 11_777 * 2];
    let run = |chunk: usize| {
        let mut graph = CaptureGraph::new(
            CaptureGraphConfig {
                input_format: format(2),
                kind: StreamKind::Voice,
                resampler: Default::default(),
                max_ingress_ms: 200,
                max_payload_bytes: 32,
            },
            Box::new(Encoder {
                format: format(1),
                fail: false,
            }),
            None,
        )
        .unwrap();
        let mut packets = Vec::new();
        for pcm in input.chunks(chunk * 2) {
            packets.extend(graph.push_native(pcm).unwrap());
        }
        packets.extend(graph.finish().unwrap());
        assert!(graph.finish().unwrap().is_empty());
        assert_eq!(graph.state(), GraphState::Stopped);
        assert!(graph.push_native(&[0.0, 0.0]).is_err());
        assert_eq!(graph.statistics().input_frames, 11_777);
        assert_eq!(graph.statistics().encoded_frames, 12_480);
        assert_eq!(graph.statistics().packet_eof_padding_frames, 703);
        assert_eq!(graph.statistics().processing_execution_ns, None);
        packets.into_iter().flat_map(|p| p.pcm).collect::<Vec<_>>()
    };
    assert_eq!(run(127), run(9600));
}

#[test]
fn capture_backend_failure_is_terminal_but_invalid_input_is_not() {
    let mut graph = CaptureGraph::new(
        CaptureGraphConfig {
            input_format: format(1),
            kind: StreamKind::Voice,
            resampler: Default::default(),
            max_ingress_ms: 200,
            max_payload_bytes: 32,
        },
        Box::new(Encoder {
            format: format(1),
            fail: true,
        }),
        None,
    )
    .unwrap();
    assert!(graph.push_native(&[f32::NAN]).is_err());
    assert_eq!(graph.state(), GraphState::Running);
    assert!(graph.push_native(&[0.2; 960]).is_err());
    assert_eq!(graph.state(), GraphState::Stopped);
}

#[test]
fn wrapping_reorder_fec_and_plc_advance_only_on_demand() {
    let source = registration(1, StreamKind::Voice);
    let (mut graph, log) = receiver(source);
    for (sequence, at) in [(0, 0), (65534, 1), (65535, 2)] {
        assert_eq!(
            graph.push_packet(packet(source, sequence, at)).unwrap(),
            PacketOutcome::Accepted
        );
    }
    assert!(log.lock().unwrap().is_empty(), "arrival must not decode");
    assert_eq!(
        graph.push_packet(packet(source, 0, 3)).unwrap(),
        PacketOutcome::Duplicate
    );
    let mut output = [0.0; 1920];
    for now in [20_000_000, 40_000_000, 60_000_000] {
        graph.render_into(&mut output, now).unwrap();
    }
    assert_eq!(*log.lock().unwrap(), ["normal", "normal", "normal"]);
    assert!(
        output.as_chunks::<2>().0.iter().all(|v| v[0] == v[1]),
        "mono expands after decoding"
    );
    graph.push_packet(packet(source, 2, 70_000_000)).unwrap();
    graph.render_into(&mut output, 80_000_000).unwrap();
    graph.render_into(&mut output, 100_000_000).unwrap();
    graph.render_into(&mut output, 120_000_000).unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        ["normal", "normal", "normal", "fec", "normal", "plc"]
    );
    assert_eq!(graph.statistics().fec_attempts, 1);
    assert_eq!(graph.statistics().concealed_packets, 1);
    assert_eq!(
        graph.push_packet(packet(source, 0, 130_000_000)).unwrap(),
        PacketOutcome::Late
    );
}

#[test]
fn sequence_jump_epoch_and_clock_recovery_clear_histories() {
    let source = registration(1, StreamKind::Voice);
    let (mut graph, log) = receiver(source);
    graph.push_packet(packet(source, 0, 0)).unwrap();
    assert_eq!(
        graph.push_packet(packet(source, 1000, 1)).unwrap(),
        PacketOutcome::Resynchronized
    );
    assert_eq!(*log.lock().unwrap(), ["reset"]);
    let mut output = [0.0; 1920];
    let mut stale = packet(source, 1001, 2);
    stale.epoch = StreamEpoch(0);
    assert_eq!(graph.push_packet(stale).unwrap(), PacketOutcome::StaleEpoch);
    assert!(graph.render_into(&mut [0.0; 2000], 30_000_000).is_err());
    assert_eq!(
        log.lock().unwrap().len(),
        1,
        "invalid demand must not advance state"
    );
    graph.render_into(&mut output, 30_000_000).unwrap();
    assert!(graph.render_into(&mut output, 0).is_err());
    graph.recover_clock().unwrap();
    let metrics = graph.render_into(&mut output, 0).unwrap();
    assert!(output.iter().all(|v| *v == 0.0));
    assert_eq!(metrics.sources[0].missing_frames, 0);
    assert_eq!(graph.statistics().clock_recoveries, 1);
    let next = SourceRegistration {
        epoch: StreamEpoch(2),
        ..source
    };
    graph
        .register(
            next,
            Box::new(Decoder {
                format: next.format,
                log,
                wrong_frames: false,
            }),
        )
        .unwrap();
    assert_eq!(
        graph.push_packet(packet(source, 1002, 1)).unwrap(),
        PacketOutcome::StaleEpoch
    );
    graph.abort();
    graph.abort();
    assert_eq!(graph.source_count(), 0);
    assert!(graph.render_into(&mut output, 1).is_err());
}

#[test]
fn decoder_shape_failure_is_counted_and_tail_drain_does_not_invent_loss() {
    let source = registration(1, StreamKind::Voice);
    let (mut graph, log) = receiver(source);
    graph
        .register(
            SourceRegistration {
                epoch: StreamEpoch(2),
                ..source
            },
            Box::new(Decoder {
                format: source.format,
                log: Arc::clone(&log),
                wrong_frames: true,
            }),
        )
        .unwrap();
    let source = SourceRegistration {
        epoch: StreamEpoch(2),
        ..source
    };
    graph.push_packet(packet(source, 0, 0)).unwrap();
    let mut output = [0.0; 1920];
    let mut now = 20_000_000;
    let mut calls = 0;
    while graph.drain_into(&mut output, now).unwrap() > 0 {
        now += 20_000_000;
        calls += 1;
        assert!(calls < 10);
    }
    assert_eq!(graph.statistics().decode_errors, 1);
    assert_eq!(graph.statistics().concealed_packets, 0);
    assert_eq!(graph.statistics().fec_attempts, 0);
    assert_eq!(graph.state(), GraphState::Stopped);
    assert_eq!(graph.drain_into(&mut output, now).unwrap(), 0);
}

#[test]
fn admission_and_pcm_rejection_preserve_render_history() {
    let config = RenderGraphConfig {
        max_sources: 1,
        ..Default::default()
    };
    let source = registration(1, StreamKind::Voice);
    let mut graph = RenderGraph::new(config).unwrap();
    graph.register(source).unwrap();
    assert!(graph.replace_source(source).is_err());
    assert!(graph.register(registration(2, StreamKind::Voice)).is_err());
    assert!(graph.set_clock_correction(source.key, i32::MIN).is_err());
    assert!(
        graph
            .push_pcm(source.key, StreamEpoch(0), &[0.2; 960])
            .is_err()
    );
    assert!(
        graph
            .push_pcm(source.key, source.epoch, &[f32::INFINITY; 960])
            .is_err()
    );
    graph
        .push_pcm(source.key, source.epoch, &[0.2; 960])
        .unwrap();
    assert_eq!(graph.sample_position(), 0);
    assert_eq!(graph.queued_frames(source.key), Some(960));
    let mut out = [0.0; 1920];
    graph.render_into(&mut out).unwrap();
    assert_eq!(graph.sample_position(), 960);
    graph.begin_drain().unwrap();
    assert!(
        graph
            .push_pcm(source.key, source.epoch, &[0.2; 960])
            .is_err()
    );
    assert!(graph.drain_into(&mut [0.0; 10_000]).unwrap() <= 960);
}

#[test]
fn receive_clock_correction_tracks_device_minus_source_and_freezes_on_recovery() {
    let low = AudioFormat::new(8_000, ChannelLayout::Mono).unwrap();
    for (source_ppm, device_ppm) in [(150.0, -100.0), (-150.0, 100.0)] {
        let source = SourceRegistration {
            format: low,
            ..registration(1, StreamKind::Voice)
        };
        let mut graph = ReceiveGraph::new(ReceiveGraphConfig {
            render: RenderGraphConfig {
                format: AudioFormat::new(8_000, ChannelLayout::Stereo).unwrap(),
                ..Default::default()
            },
            jitter: JitterBufferConfig {
                target_ms: 20,
                in_band_fec: true,
            },
            clock_window_ms: 1000,
            clock_slew_ppm_per_second: 1000.0,
            ..Default::default()
        })
        .unwrap();
        graph
            .register(
                source,
                Box::new(Decoder {
                    format: low,
                    log: Arc::new(Mutex::new(Vec::new())),
                    wrong_frames: false,
                }),
            )
            .unwrap();
        let mut sequence = 0_u16;
        let source_step = 20_000_000.0 / (1.0 + source_ppm / 1e6);
        let device_step = 16_000_000.0 / (1.0 + device_ppm / 1e6);
        let mut next_arrival = 0.0;
        let mut output = [0.0; 256];
        for callback in 0..320 {
            let now = (callback as f64 * device_step).round() as u64;
            while next_arrival <= now as f64 {
                graph
                    .push_packet(packet(source, sequence, next_arrival.round() as u64))
                    .unwrap();
                next_arrival += source_step;
                sequence = sequence.wrapping_add(1);
            }
            graph.render_into(&mut output, now).unwrap();
            assert!(output.iter().all(|v| v.is_finite() && v.abs() <= 0.892));
        }
        let clock = graph.clock_metrics()[0];
        let expected = ((1.0 + device_ppm / 1e6) / (1.0 + source_ppm / 1e6) - 1.0) * 1e6;
        assert!(
            (f64::from(clock.applied_correction_ppm) - expected).abs() < 2.0,
            "{clock:?}"
        );
        assert!(!clock.saturated);
        assert_eq!(graph.statistics().concealed_packets, 0);
        graph.recover_clock().unwrap();
        let clock = graph.clock_metrics()[0];
        assert_eq!(clock.applied_correction_ppm, 0);
        assert!(clock.inferred_source_drift_ppm.is_none());
        assert!(clock.inferred_device_drift_ppm.is_none());
    }
}

#[test]
fn silent_speech_does_not_attenuate_voice_or_desktop_bus() {
    let run = |silent: bool| {
        let mut graph = RenderGraph::new(Default::default()).unwrap();
        let voice = registration(1, StreamKind::Voice);
        let desktop = registration(2, StreamKind::Desktop);
        graph.register(voice).unwrap();
        graph.register(desktop).unwrap();
        let quiet = registration(3, StreamKind::Voice);
        if silent {
            graph.register(quiet).unwrap();
        }
        let mut rendered = Vec::new();
        for _ in 0..12 {
            graph.push_pcm(voice.key, voice.epoch, &[0.1; 960]).unwrap();
            graph
                .push_pcm(desktop.key, desktop.epoch, &[0.1; 1920])
                .unwrap();
            if silent {
                graph.push_pcm(quiet.key, quiet.epoch, &[0.0; 960]).unwrap();
            }
            let mut out = [0.0; 1920];
            let metrics = graph.render_into(&mut out).unwrap();
            assert_eq!(metrics.active_voice, 1);
            assert_eq!(metrics.active_desktop, 1);
            rendered.extend(out);
        }
        rendered
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn inferred_sample_clocks_survive_ten_virtual_minutes_and_reset_on_disturbance() {
    use audiokit::clock::SampleRateEstimator;
    for rate in [44_100_u32, 48_000, 96_000] {
        for ppm in [-150.0, 150.0] {
            let mut estimate = SampleRateEstimator::new(rate, 10_000).unwrap();
            let step = u64::from(rate) / 50;
            for block in 0..30_000_u64 {
                let position = block * step;
                let ns =
                    (position as f64 / (f64::from(rate) * (1.0 + ppm / 1e6)) * 1e9).round() as u64;
                estimate.observe(position, ns);
            }
            assert!((estimate.estimate().unwrap().drift_ppm - ppm).abs() < 0.01);
            estimate.observe(30_000 * step, 610_000_000_000);
            assert!(
                estimate.estimate().is_none(),
                "host pause must not become inferred clock drift"
            );
        }
    }
}
