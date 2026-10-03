//! Frozen pre-cache scan reference. It intentionally does not use production target caches.
//! The detector is shared because this verifies scheduling equivalence, not FIR accuracy;
//! the separate integration reconstruction oracle independently covers output protection.
use super::*;

struct ScalarReference(MasterLimiter);
impl ScalarReference {
    fn process(&mut self, input: &[f32]) -> Result<(Vec<f32>, LimiterFrameMetrics), LimiterError> {
        let s = &mut self.0;
        if !input.len().is_multiple_of(s.channels) {
            return Err(LimiterError::IncompleteFrame);
        }
        if input.iter().any(|sample| !sample.is_finite()) {
            return Err(LimiterError::NonFiniteSample);
        }
        let delay_frames = s.delay.len() / s.channels;
        let mut output = Vec::with_capacity(input.len());
        let mut metrics = LimiterFrameMetrics {
            lookahead_frames: s.lookahead_frames as u32,
            ..Default::default()
        };
        for input_frame in input.chunks_exact(s.channels) {
            let write_frame = (s.cursor_frame + s.lookahead_frames) % delay_frames;
            let start = write_frame * s.channels;
            s.delay[start..start + s.channels].copy_from_slice(input_frame);
            let true_peak =
                input_frame
                    .iter()
                    .copied()
                    .enumerate()
                    .fold(0.0_f64, |peak, (channel, sample)| {
                        peak.max(s.true_peak_estimator.observe_sample(
                            f64::from(sample),
                            channel,
                            s.channels,
                        ))
                    }) as f32;
            let true_peak_frame =
                (write_frame + delay_frames - TRUE_PEAK_GROUP_DELAY_FRAMES) % delay_frames;
            s.true_peak_delay[true_peak_frame] = true_peak;
            let mut upcoming_target = 1.0_f32;
            let mut scheduled_gain = s.gain;
            for distance in 0..=s.attack_frames {
                let frame_index = (s.cursor_frame + distance) % delay_frames;
                let start = frame_index * s.channels;
                let sample_peak = s.delay[start..start + s.channels]
                    .iter()
                    .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
                let peak = sample_peak.max(s.true_peak_delay[frame_index]);
                let target = if peak > s.detector_ceiling {
                    (s.detector_ceiling * (1.0 - 4.0 * f32::EPSILON)) / peak
                } else {
                    1.0
                };
                upcoming_target = upcoming_target.min(target);
                if target < s.gain {
                    let candidate_gain = if distance == 0 {
                        target
                    } else {
                        s.gain + (target - s.gain) / distance as f32
                    };
                    scheduled_gain = scheduled_gain.min(candidate_gain);
                }
            }
            if scheduled_gain < s.gain {
                s.gain = scheduled_gain;
            } else if upcoming_target > s.gain {
                s.gain = upcoming_target + (s.gain - upcoming_target) * s.release_coefficient;
            }
            let reduction = (-20.0 * s.gain.max(f32::MIN_POSITIVE).log10() * 1000.0)
                .ceil()
                .max(0.0) as u32;
            metrics.max_gain_reduction_millidb = metrics.max_gain_reduction_millidb.max(reduction);
            let output_start = s.cursor_frame * s.channels;
            for channel in 0..s.channels {
                let mut sample = s.delay[output_start + channel] * s.gain;
                if s.gain < 0.999_999 {
                    metrics.attenuated_samples = metrics.attenuated_samples.saturating_add(1);
                }
                if sample.abs() > s.ceiling {
                    let excess = ((f64::from(sample.abs() - s.ceiling) / f64::from(s.ceiling))
                        * 1_000_000_000.0)
                        .ceil() as u64;
                    metrics.max_safety_overshoot_ppb = metrics.max_safety_overshoot_ppb.max(excess);
                    sample = sample.clamp(-s.ceiling, s.ceiling);
                    metrics.safety_clamped_samples =
                        metrics.safety_clamped_samples.saturating_add(1);
                }
                output.push(sample);
            }
            s.cursor_frame = (s.cursor_frame + 1) % delay_frames;
        }
        Ok((output, metrics))
    }
}

fn equal_block(actual: &mut MasterLimiter, reference: &mut ScalarReference, pcm: &[f32]) {
    let (a, am) = actual.process_interleaved(pcm).unwrap();
    let (b, bm) = reference.process(pcm).unwrap();
    assert_eq!(
        a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        b.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    assert_eq!(am, bm);
    assert_eq!(actual.gain.to_bits(), reference.0.gain.to_bits());
    assert_eq!(actual.cursor_frame, reference.0.cursor_frame);
}

fn signal(frames: usize, channels: usize) -> Vec<f32> {
    let mut seed = 0x7124_ade1_u32;
    (0..frames * channels)
        .map(|index| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            match (index / channels) % 211 {
                0 => {
                    if index.is_multiple_of(2) {
                        8.0
                    } else {
                        -4.0
                    }
                }
                1..=64 => {
                    if index.is_multiple_of(2) {
                        0.0
                    } else {
                        -0.0
                    }
                }
                65 => f32::from_bits(1),
                66 => -f32::MIN_POSITIVE,
                _ => ((seed >> 8) as f32 / 16_777_215.0 * 2.0 - 1.0) * 2.5,
            }
        })
        .collect()
}

#[test]
fn scan_matches_frozen_reference_across_rates_layouts_windows_and_tail() {
    for rate in [8_000, 44_100, 48_000, 96_000, 384_000] {
        for channels in [1_u8, 2, 4] {
            for config in [
                LimiterConfig::default(),
                LimiterConfig {
                    ceiling_dbfs: -18.0,
                    lookahead_ms: 1.0,
                    attack_ms: 1.0,
                    release_ms: 30.0,
                    reconstruction_headroom_db: 0.0,
                },
                LimiterConfig {
                    ceiling_dbfs: -0.5,
                    lookahead_ms: 5.0,
                    attack_ms: 5.0,
                    release_ms: 300.0,
                    reconstruction_headroom_db: 1.0,
                },
                LimiterConfig {
                    lookahead_ms: 10.0,
                    attack_ms: 0.1,
                    ..Default::default()
                },
            ] {
                let mut actual = MasterLimiter::new(rate, channels, config).unwrap();
                let mut reference = ScalarReference(actual.clone());
                let mut pcm = signal(actual.lookahead_frames * 2 + 257, usize::from(channels));
                pcm.extend(std::iter::repeat_n(
                    0.0,
                    (actual.lookahead_frames + 128) * usize::from(channels),
                ));
                let mut offset = 0;
                let mut block = 0;
                while offset < pcm.len() {
                    let width = [1, 7, 127, 480][block % 4] * usize::from(channels);
                    let end = (offset + width).min(pcm.len());
                    equal_block(&mut actual, &mut reference, &pcm[offset..end]);
                    equal_block(&mut actual, &mut reference, &[]);
                    offset = end;
                    block += 1;
                }
            }
        }
    }
}

#[test]
fn extreme_finite_samples_and_supported_format_edges_match_reference() {
    for (rate, channels) in [(1, 1_u8), (48_000, 255), (384_000, 2)] {
        let mut actual = MasterLimiter::new(rate, channels, Default::default()).unwrap();
        let mut reference = ScalarReference(actual.clone());
        for amplitude in [f32::MAX, -f32::MAX, 0.0, -0.0, 0.25, 4.0, 0.0] {
            let pcm = vec![amplitude; (actual.lookahead_frames + 129) * usize::from(channels)];
            for block in pcm.chunks(127 * usize::from(channels)) {
                equal_block(&mut actual, &mut reference, block);
            }
        }
    }
}

#[test]
fn reset_reconfiguration_clone_and_rejected_input_preserve_scan_state() {
    let mut actual = MasterLimiter::new(48_000, 2, Default::default()).unwrap();
    let mut reference = ScalarReference(actual.clone());
    for cycle in 0..4 {
        equal_block(&mut actual, &mut reference, &signal(801, 2));
        for bad in [vec![0.0], vec![f32::NAN, 0.0], vec![f32::INFINITY, 0.0]] {
            assert_eq!(actual.process_interleaved(&bad), reference.process(&bad));
        }
        let mut cloned = actual.clone();
        let mut reference_clone = ScalarReference(reference.0.clone());
        equal_block(&mut cloned, &mut reference_clone, &signal(313, 2));
        equal_block(&mut actual, &mut reference, &signal(501, 2));
        let invalid = LimiterConfig {
            attack_ms: 5.0,
            lookahead_ms: 1.0,
            ..Default::default()
        };
        assert_eq!(actual.set_config(invalid), reference.0.set_config(invalid));
        equal_block(&mut actual, &mut reference, &signal(97, 2));
        if cycle % 2 == 0 {
            actual.reset();
            reference.0.reset();
        } else {
            let config = LimiterConfig {
                ceiling_dbfs: -3.0,
                lookahead_ms: 2.0,
                attack_ms: 0.1,
                reconstruction_headroom_db: 0.5,
                ..Default::default()
            };
            actual.set_config(config).unwrap();
            reference.0.set_config(config).unwrap();
        }
        equal_block(&mut actual, &mut reference, &vec![0.0; 960]);
    }
}
