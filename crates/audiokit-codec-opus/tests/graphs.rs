//! Same shared capture/receive/render owners with real Opus, no SDK or duplicated DSP.
use audiokit::graph::{capture::*, receive::*, render::SourceRegistration};
use audiokit::{
    AudioFormat, ChannelLayout, PacketDuration, SourceId, SourceKey, StreamEpoch, StreamId,
    StreamKind,
};
use audiokit_codec_opus::{OpusConfig, OpusDecoder, OpusEncoder, OpusProfile};

#[test]
fn real_codec_roundtrip_covers_voice_desktop_and_all_session_durations() {
    for (kind, layout, profile) in [
        (StreamKind::Voice, ChannelLayout::Mono, OpusProfile::Voice),
        (
            StreamKind::Desktop,
            ChannelLayout::Stereo,
            OpusProfile::Desktop,
        ),
    ] {
        for duration in [
            PacketDuration::Ms10,
            PacketDuration::Ms20,
            PacketDuration::Ms40,
            PacketDuration::Ms60,
        ] {
            let native = AudioFormat::new(44_100, ChannelLayout::Stereo).unwrap();
            let format = AudioFormat::new(48_000, layout).unwrap();
            let mut capture = CaptureGraph::new(
                CaptureGraphConfig {
                    input_format: native,
                    kind,
                    resampler: Default::default(),
                    max_ingress_ms: 200,
                    max_payload_bytes: 1142,
                },
                Box::new(
                    OpusEncoder::new(format, duration, profile, OpusConfig::default(), 1142)
                        .unwrap(),
                ),
                None,
            )
            .unwrap();
            let mut receive = ReceiveGraph::new(Default::default()).unwrap();
            let source = SourceRegistration {
                key: SourceKey {
                    source: SourceId::new(1).unwrap(),
                    stream: StreamId::new(1).unwrap(),
                },
                epoch: StreamEpoch(1),
                format,
                kind,
            };
            receive
                .register(
                    source,
                    Box::new(OpusDecoder::new(format, duration).unwrap()),
                )
                .unwrap();
            let mut packets = Vec::new();
            for block in 0..30 {
                let pcm = (0..882)
                    .flat_map(|frame| {
                        let value = ((block * 882 + frame) as f32 * std::f32::consts::TAU * 997.0
                            / 44100.0)
                            .sin()
                            * 0.3;
                        [value, value * 0.7]
                    })
                    .collect::<Vec<_>>();
                packets.extend(capture.push_native(&pcm).unwrap());
            }
            packets.extend(capture.finish().unwrap());
            // Offline paced ingress is intentionally separate from device-demand rendering.
            let mut output = [0.0; 1920];
            let mut now = 0_u64;
            let mut energy = 0.0_f64;
            for (sequence, packet) in packets.into_iter().enumerate() {
                assert!(packet.payload.len() <= 1142);
                receive
                    .push_packet(EncodedPacket {
                        source: source.key,
                        epoch: source.epoch,
                        sequence: sequence as u16,
                        duration,
                        arrival_ns: now,
                        payload: packet.payload,
                    })
                    .unwrap();
                for _ in 0..duration.milliseconds() / 10 {
                    receive.render_into(&mut output[..960], now).unwrap();
                    energy += output[..960]
                        .iter()
                        .map(|v| f64::from(*v).powi(2))
                        .sum::<f64>();
                    now += 10_000_000;
                }
            }
            let losses = receive.statistics().concealed_packets;
            let mut calls = 0;
            while receive.drain_into(&mut output, now).unwrap() > 0 {
                now += 20_000_000;
                calls += 1;
                assert!(calls < 30);
                assert!(output.iter().all(|v| v.is_finite() && v.abs() <= 0.892));
                energy += output.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>();
            }
            assert!(energy > 10.0);
            assert_eq!(receive.statistics().decode_errors, 0);
            assert_eq!(
                receive.statistics().concealed_packets,
                losses,
                "EOF must not inject PLC"
            );
        }
    }
}

#[test]
fn exact_bit_target_and_invalid_update_preserve_the_last_valid_setting() {
    let mono = AudioFormat::new(48_000, ChannelLayout::Mono).unwrap();
    let mut encoder = OpusEncoder::new(
        mono,
        PacketDuration::Ms20,
        OpusProfile::Voice,
        OpusConfig::default(),
        1142,
    )
    .unwrap();
    encoder.set_target_bitrate_bps(96_789).unwrap();
    assert_eq!(encoder.bitrate().unwrap(), opus::Bitrate::Bits(96_789));
    assert!(encoder.set_target_bitrate_bps(128_001).is_err());
    assert_eq!(encoder.bitrate().unwrap(), opus::Bitrate::Bits(96_789));
}
