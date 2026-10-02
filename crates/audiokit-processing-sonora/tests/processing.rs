//! SDK-free backend contracts, bypass and baseline compatibility evidence.

use audiokit::backend::VoiceProcessor;
use audiokit::{AudioError, AudioFormat, ChannelLayout};
use audiokit_processing_sonora::{AudioProcessingOptions, NoiseSuppressionMode, SonoraProcessor};

fn format(rate: u32, stereo: bool) -> AudioFormat {
    AudioFormat::new(
        rate,
        if stereo {
            ChannelLayout::Stereo
        } else {
            ChannelLayout::Mono
        },
    )
    .unwrap()
}
fn bypass() -> AudioProcessingOptions {
    AudioProcessingOptions {
        high_pass_filter: false,
        noise_suppression: NoiseSuppressionMode::Off,
        gain_controller2: false,
        adaptive_gain: false,
    }
}

#[test]
fn float_bypass_preserves_channel_layout_without_integer_quantization() {
    for rate in [8_000, 16_000, 32_000, 48_000] {
        for stereo in [false, true] {
            let f = format(rate, stereo);
            let mut processor = SonoraProcessor::new(f, f, false, bypass()).unwrap();
            let samples = (0..rate as usize / 100)
                .flat_map(|i| {
                    let left = (i as f32 * 0.07).sin() * 0.312_345;
                    [Some(left), stereo.then_some(-left * 0.25)]
                        .into_iter()
                        .flatten()
                })
                .collect::<Vec<_>>();
            let mut processed = samples.clone();
            processor.process_capture(&mut processed).unwrap();
            assert_eq!(processed.len(), samples.len());
            assert!(
                processed
                    .iter()
                    .zip(&samples)
                    .all(|(a, b)| (a - b).abs() < 1e-6),
                "bypass mismatch at {f:?}"
            );
        }
    }
}

#[test]
fn render_and_capture_have_independent_formats_and_unknown_delay_is_not_zero() {
    let capture = format(48_000, false);
    let render = format(32_000, true);
    let mut processor =
        SonoraProcessor::new(capture, render, true, AudioProcessingOptions::default()).unwrap();
    assert_eq!(processor.capture_format(), capture);
    assert_eq!(processor.render_format(), render);
    assert_eq!(processor.algorithmic_delay(), None);
    processor.set_delay_ms(500).unwrap();
    assert!(matches!(
        processor.set_delay_ms(501),
        Err(AudioError::InvalidConfig(_))
    ));
    for _ in 0..8 {
        processor.analyze_render(&[0.0; 640]).unwrap();
        let mut pcm = [0.0; 480];
        processor.process_capture(&mut pcm).unwrap();
        assert!(pcm.iter().all(|s| s.is_finite()));
    }
}

#[test]
fn malformed_quantum_is_rejected_before_backend_history_changes() {
    let f = format(48_000, false);
    let mut processor = SonoraProcessor::new(f, f, false, bypass()).unwrap();
    assert!(processor.process_capture(&mut [0.0; 960]).is_err());
    let mut bad = [0.0; 480];
    bad[123] = f32::NAN;
    assert!(processor.process_capture(&mut bad).is_err());
    bad[123] = 1.1;
    assert!(processor.process_capture(&mut bad).is_err());
    let mut valid = [0.123_456; 480];
    processor.process_capture(&mut valid).unwrap();
    assert!(valid.iter().all(|s| (*s - 0.123_456).abs() < 1e-6));
    assert!(SonoraProcessor::new(format(44_100, false), f, false, bypass()).is_err());
}

#[test]
fn accepted_i16_path_is_bit_exact_against_frozen_client_sonora_configuration() {
    use sonora::config::{
        GainController2, HighPassFilter, MaxProcessingRate, NoiseSuppression, Pipeline,
    };
    use sonora::{AudioProcessing, Config, StreamConfig};
    // Independent construction of the pre-extraction default configuration.
    let reference_config = Config {
        pipeline: Pipeline {
            maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz,
            ..Pipeline::default()
        },
        high_pass_filter: Some(HighPassFilter::default()),
        noise_suppression: Some(NoiseSuppression::default()),
        gain_controller2: Some(GainController2 {
            adaptive_digital: None,
            ..GainController2::default()
        }),
        echo_canceller: None,
        ..Config::default()
    };
    let stream = StreamConfig::new(48_000, 1);
    let mut reference = AudioProcessing::builder()
        .config(reference_config)
        .capture_config(stream)
        .render_config(stream)
        .echo_detector(false)
        .build();
    let f = format(48_000, false);
    let mut backend = SonoraProcessor::new(f, f, false, AudioProcessingOptions::default()).unwrap();
    let mut seed = 42_u32;
    for block in 0..200 {
        let input = (0..480)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 16) as i32 - 32_768) / 16;
                let signal = (std::f32::consts::TAU * 440.0 * (block * 480 + i) as f32 / 48_000.0)
                    .sin()
                    * 8_000.0;
                signal as i16 + noise as i16
            })
            .collect::<Vec<_>>();
        let mut expected = vec![0; 480];
        reference
            .process_capture_i16(&input, &mut expected)
            .unwrap();
        let mut actual = input.clone();
        backend.process_capture_i16_10ms(&mut actual).unwrap();
        assert_eq!(actual, expected, "baseline changed in block {block}");
    }
}
