//! Independent codec regression and malformed-packet/state-preservation checks.
use audiokit::backend::{AudioDecoder, AudioEncoder, DecodeRequest};
use audiokit::{AudioFormat, ChannelLayout, PacketDuration};
use audiokit_codec_opus::*;

fn mono() -> AudioFormat {
    AudioFormat::new(48_000, ChannelLayout::Mono).unwrap()
}

#[test]
fn budgets_apply_only_to_enabled_streams() {
    let cfg = OpusConfig::default();
    let applied = effective_config_for_payload(cfg, 240, PacketDuration::Ms20, false).unwrap();
    assert_eq!(applied.voice_bitrate_kbps, 86);
    assert_eq!(applied.desktop_bitrate_kbps, 196);
    assert!(effective_config_for_payload(cfg, 240, PacketDuration::Ms20, true).is_err());
    assert!(effective_config_for_payload(cfg, 170, PacketDuration::Ms20, false).is_err());
}

#[test]
fn i16_backend_is_bit_exact_with_prior_encoder() {
    let mut old =
        opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
    old.set_bitrate(opus::Bitrate::Bits(96_000)).unwrap();
    old.set_inband_fec(true).unwrap();
    old.set_packet_loss_perc(10).unwrap();
    let mut new = OpusEncoder::new(
        mono(),
        PacketDuration::Ms20,
        OpusProfile::Voice,
        OpusConfig::default(),
        MAX_OPUS_PACKET,
    )
    .unwrap();
    let mut old_decoder = opus::Decoder::new(48_000, opus::Channels::Mono).unwrap();
    let mut new_decoder = OpusDecoder::new(mono(), PacketDuration::Ms20).unwrap();
    for packet in 0..100 {
        let pcm = (0..960)
            .map(|i| {
                ((std::f64::consts::TAU * 997.0 * (packet * 960 + i) as f64 / 48_000.0).sin()
                    * 12_000.0) as i16
            })
            .collect::<Vec<_>>();
        let reference = old.encode_vec(&pcm, MAX_OPUS_PACKET).unwrap();
        let actual = new.encode_i16(&pcm).unwrap();
        assert_eq!(actual, reference);
        let mut decoded = [0; 960];
        old_decoder.decode(&reference, &mut decoded, false).unwrap();
        assert_eq!(
            new_decoder
                .decode_i16(DecodeRequest::Packet(&actual))
                .unwrap(),
            decoded
        );
    }
}

#[test]
fn invalid_duration_and_buffers_do_not_advance_decoder_history() {
    let mut encoder = OpusEncoder::new(
        mono(),
        PacketDuration::Ms20,
        OpusProfile::Voice,
        OpusConfig::default(),
        240,
    )
    .unwrap();
    let pcm = vec![0.1; 960];
    let mut packet = [0; 240];
    let len = encoder.encode_into(&pcm, &mut packet).unwrap();
    assert!(len <= 240);
    let mut tested = OpusDecoder::new(mono(), PacketDuration::Ms20).unwrap();
    let mut reference = OpusDecoder::new(mono(), PacketDuration::Ms20).unwrap();
    let mut short = [0.0; 959];
    assert!(
        tested
            .decode_into(DecodeRequest::Packet(&packet[..len]), &mut short)
            .is_err()
    );
    let mut wrong_encoder = OpusEncoder::new(
        mono(),
        PacketDuration::Ms40,
        OpusProfile::Voice,
        OpusConfig::default(),
        4000,
    )
    .unwrap();
    let wrong = wrong_encoder.encode_i16(&vec![0; 1920]).unwrap();
    let mut output = [0.0; 960];
    assert!(
        tested
            .decode_into(DecodeRequest::Packet(&wrong), &mut output)
            .is_err()
    );
    tested
        .decode_into(DecodeRequest::Packet(&packet[..len]), &mut output)
        .unwrap();
    let mut expected = [0.0; 960];
    reference
        .decode_into(DecodeRequest::Packet(&packet[..len]), &mut expected)
        .unwrap();
    assert_eq!(output, expected);
    tested
        .decode_into(DecodeRequest::Loss, &mut output)
        .unwrap();
    assert!(output.iter().all(|v| v.is_finite()));
    tested.reset().unwrap();
}

#[test]
fn encoder_rejects_invalid_pcm_before_state_changes() {
    let mut tested = OpusEncoder::new(
        mono(),
        PacketDuration::Ms20,
        OpusProfile::Voice,
        OpusConfig::default(),
        240,
    )
    .unwrap();
    let mut reference = OpusEncoder::new(
        mono(),
        PacketDuration::Ms20,
        OpusProfile::Voice,
        OpusConfig::default(),
        240,
    )
    .unwrap();
    assert!(tested.encode_into(&[0.1; 960], &mut [0; 239]).is_err());
    assert!(tested.encode_into(&[f32::NAN; 960], &mut [0; 240]).is_err());
    let mut output = [0; 240];
    let mut expected = [0; 240];
    let a = tested.encode_into(&[0.1; 960], &mut output).unwrap();
    let b = reference.encode_into(&[0.1; 960], &mut expected).unwrap();
    assert_eq!(a, b);
    assert_eq!(&output[..a], &expected[..b]);
}
