//! Virtual-clock and continuous-stream sample-accounting regression checks.
#![cfg(feature = "resampling")]
use audiokit::clock::*;
use audiokit::resample::{ResamplerConfig, ResamplerQuality};
use audiokit::stream_resample::ContinuousResampler;
use audiokit::{AudioFormat, ChannelLayout};

#[test]
fn ten_minute_clock_convergence_is_rate_and_callback_size_independent() {
    for rate in [44_100_u32, 48_000, 96_000] {
        for block in [128_usize, 480, 1024] {
            for drift in [-150.0, 150.0] {
                let dt_ms = block as f64 * 1000.0 / f64::from(rate);
                let cfg = QueueClockConfig::default();
                let mut controller = QueueClockController::new(cfg).unwrap();
                let mut queue_ms = cfg.target_ms;
                let mut ppm = 0;
                for _ in 0..(600_000.0 / dt_ms).ceil() as usize {
                    let update = controller
                        .update(queue_ms, dt_ms, QueueClockState::Running)
                        .unwrap();
                    ppm = update.correction_ppm;
                    assert!(ppm.abs() <= 500);
                    queue_ms += dt_ms * (f64::from(ppm) - drift) / 1_000_000.0;
                    assert!((50.0..=130.0).contains(&queue_ms));
                }
                assert!(
                    (f64::from(ppm) - drift).abs() <= 2.0,
                    "rate={rate}, block={block}, applied={ppm}"
                );
            }
        }
    }
}

#[test]
fn recovery_freezes_clock_and_rejects_invalid_measurements() {
    let mut c = QueueClockController::new(QueueClockConfig::default()).unwrap();
    for _ in 0..100 {
        c.update(10.0, 20.0, QueueClockState::Running).unwrap();
    }
    assert!(
        c.update(10.0, 20.0, QueueClockState::Running)
            .unwrap()
            .saturated
    );
    assert!(c.update(f64::NAN, 20.0, QueueClockState::Running).is_err());
    assert_eq!(
        c.update(500.0, 20.0, QueueClockState::Recovering)
            .unwrap()
            .correction_ppm,
        0
    );
    assert_eq!(
        c.update(90.0, 20.0, QueueClockState::Running)
            .unwrap()
            .correction_ppm,
        0
    );
}

#[test]
fn continuous_resampling_is_independent_of_ingress_chunking_and_drains_once() {
    for (src, dst) in [(44_100_u32, 48_000_u32), (48_000, 44_100), (48_000, 48_000)] {
        for quality in [ResamplerQuality::Balanced, ResamplerQuality::HighQuality] {
            for layout in [ChannelLayout::Mono, ChannelLayout::Stereo] {
                let input = AudioFormat::new(src, layout).unwrap();
                let output = AudioFormat::new(dst, layout).unwrap();
                let config = ResamplerConfig {
                    quality,
                    ..Default::default()
                };
                let count = src as usize / 5 + 17;
                let samples = (0..count)
                    .flat_map(|i| {
                        let v = (std::f32::consts::TAU * 997.0 * i as f32 / src as f32).sin() * 1.5;
                        std::iter::repeat_n(v, usize::from(input.channels()))
                    })
                    .collect::<Vec<_>>();
                let run = |chunk_frames: usize| {
                    let mut converter =
                        ContinuousResampler::new(input, output, 20, config).unwrap();
                    let mut pcm = Vec::new();
                    for chunk in samples.chunks(chunk_frames * usize::from(input.channels())) {
                        pcm.extend(converter.push(chunk).unwrap());
                    }
                    pcm.extend(converter.finish().unwrap());
                    assert!(converter.finish().unwrap().is_empty());
                    assert!(converter.push(&[]).is_err());
                    let accounting = converter.accounting();
                    assert_eq!(accounting.input_frames, count as u64);
                    let expected = (count as u64 * u64::from(dst)).div_ceil(u64::from(src))
                        + accounting.delay_frames as u64;
                    assert_eq!(accounting.output_frames, expected);
                    assert_eq!(pcm.len() as u64, expected * u64::from(output.channels()));
                    assert!(pcm.iter().all(|v| v.is_finite()));
                    assert!(pcm.iter().any(|v| v.abs() > 1.0));
                    pcm
                };
                assert_eq!(run(127), run(997));
            }
        }
    }
}
