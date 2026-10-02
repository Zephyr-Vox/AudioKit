//! Independent 16x, 128-tap Blackman-windowed sinc reconstruction; no production FIR is reused.
use audiokit::limiter::{LimiterConfig, MasterLimiter};
use std::f64::consts::PI;

fn reconstructed_peak(pcm: &[f32], channels: usize) -> f64 {
    let mut peak = pcm.iter().map(|v| f64::from(v.abs())).fold(0.0, f64::max);
    for phase in 1..16 {
        let fraction = phase as f64 / 16.0;
        let mut coefficients = [0.0; 128];
        for (index, coefficient) in coefficients.iter_mut().enumerate() {
            let distance = index as f64 - 63.0 - fraction;
            let sinc = if distance.abs() < 1e-12 {
                1.0
            } else {
                (PI * distance).sin() / (PI * distance)
            };
            let window = 0.42 - 0.5 * (2.0 * PI * index as f64 / 127.0).cos()
                + 0.08 * (4.0 * PI * index as f64 / 127.0).cos();
            *coefficient = sinc * window;
        }
        let sum = coefficients.iter().sum::<f64>();
        for coefficient in &mut coefficients {
            *coefficient /= sum;
        }
        // Explicit zero extension includes startup and the fully drained end of stream.
        let frames = pcm.len() / channels;
        for center in -64..frames as isize + 64 {
            for channel in 0..channels {
                let value = coefficients
                    .iter()
                    .enumerate()
                    .map(|(index, weight)| {
                        let frame = center + index as isize - 63;
                        if (0..frames as isize).contains(&frame) {
                            f64::from(pcm[frame as usize * channels + channel]) * weight
                        } else {
                            0.0
                        }
                    })
                    .sum::<f64>();
                peak = peak.max(value.abs());
            }
        }
    }
    peak
}

#[test]
fn reconstruction_headroom_is_optional_validated_and_compatible_with_older_configs() {
    let config: LimiterConfig = serde_json::from_str(
        r#"{"ceiling_dbfs":-1.0,"lookahead_ms":3.0,"attack_ms":1.0,"release_ms":100.0}"#,
    )
    .unwrap();
    assert_eq!(config.reconstruction_headroom_db, 0.2);
    assert!(
        LimiterConfig {
            reconstruction_headroom_db: f32::NAN,
            ..config
        }
        .validate()
        .is_err()
    );
    assert!(
        LimiterConfig {
            reconstruction_headroom_db: 1.1,
            ..config
        }
        .validate()
        .is_err()
    );
    assert!(
        LimiterConfig {
            reconstruction_headroom_db: 0.0,
            ..config
        }
        .validate()
        .is_ok()
    );
}

#[test]
fn independent_reconstruction_checks_tones_transients_and_complete_limiter_tail() {
    let config = LimiterConfig::default();
    let ceiling = 10_f64.powf(f64::from(config.ceiling_dbfs) / 20.0);
    for rate in [44_100_u32, 48_000, 96_000] {
        for channels in [1_usize, 2] {
            for frequency in [0.05, 0.25, 0.45] {
                for phase in [0.0, 0.73, PI / 4.0, PI / 2.0] {
                    let mut limiter = MasterLimiter::new(rate, channels as u8, config).unwrap();
                    let mut input = Vec::new();
                    for frame in 0..2048 {
                        let envelope = if (200..1800).contains(&frame) {
                            2.0
                        } else {
                            0.0
                        };
                        let value = (2.0 * PI * frequency * frame as f64 + phase).sin() * envelope;
                        for channel in 0..channels {
                            input.push((value * if channel == 0 { 1.0 } else { -0.7 }) as f32);
                        }
                    }
                    input.extend(std::iter::repeat_n(
                        0.0,
                        ((rate as f64 * 0.01).ceil() as usize + 128) * channels,
                    ));
                    let mut out = Vec::new();
                    let mut clamps = 0;
                    for chunk in input.chunks(127 * channels) {
                        let (block, metrics) = limiter.process_interleaved(chunk).unwrap();
                        clamps += metrics.safety_clamped_samples;
                        out.extend(block);
                    }
                    let peak = reconstructed_peak(&out, channels);
                    let overshoot_db = 20.0 * (peak / ceiling).log10();
                    println!(
                        "rate={rate} channels={channels} normalized_frequency={frequency} phase={phase:.5} overshoot_db={overshoot_db:.5} clamps={clamps}"
                    );
                    assert!(out.iter().all(|v| f64::from(v.abs()) <= ceiling + 1e-6));
                    assert!(
                        overshoot_db <= 0.1,
                        "independent reconstruction exceeds ceiling tolerance: {overshoot_db:.5} dB"
                    );
                }
            }
        }
    }
}
