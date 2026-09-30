//! Public, SDK-free regression tests for shared signal measurement.

use audiokit::diagnostics::{AudioAnalysisError, AudioDiagnosticsConfig, AudioSignalAnalyzer};

#[test]
fn integer_analysis_preserves_channel_boundaries_and_signed_full_scale() {
    let mut analyzer = AudioSignalAnalyzer::new(AudioDiagnosticsConfig {
        discontinuity_threshold_q15: 1,
    });
    let frame = [i16::MIN, i16::MAX];
    for _ in 0..2 {
        let metrics = analyzer.observe_i16_frame(&frame, 48_000, 2).unwrap();
        assert_eq!(metrics.peak_q15, 32_768);
        assert_eq!(metrics.full_scale_samples, 2);
        assert_eq!(metrics.discontinuity_candidates, 0);
    }
    analyzer.reset_history();
    assert_eq!(
        analyzer
            .observe_i16_frame(&[0, 0], 48_000, 2)
            .unwrap()
            .discontinuity_candidates,
        0
    );
}

#[test]
fn analysis_rejects_bad_input_without_poisoning_history() {
    let mut analyzer = AudioSignalAnalyzer::new(AudioDiagnosticsConfig::default());
    analyzer
        .observe_f32_frame(&[0.25, -0.25], 48_000, 2)
        .unwrap();
    assert_eq!(
        analyzer.observe_f32_frame(&[f32::NAN, 0.0], 48_000, 2),
        Err(AudioAnalysisError::NonFiniteSample)
    );
    assert_eq!(
        analyzer.observe_i16_frame(&[1], 48_000, 2),
        Err(AudioAnalysisError::IncompleteInterleavedFrame)
    );
    assert_eq!(
        analyzer.observe_i16_frame(&[], 0, 1),
        Err(AudioAnalysisError::InvalidFormat)
    );
    assert_eq!(
        analyzer
            .observe_f32_frame(&[0.25, -0.25], 48_000, 2)
            .unwrap()
            .discontinuity_candidates,
        0
    );
}

#[test]
fn float_analysis_retains_overdrive_and_interblock_history() {
    let mut analyzer = AudioSignalAnalyzer::new(AudioDiagnosticsConfig::default());
    let metrics = analyzer
        .observe_f32_frame(&[1.25, -1.5], 48_000, 1)
        .unwrap();
    assert_eq!(metrics.peak_q15, 49_152);
    assert_eq!(metrics.full_scale_samples, 2);
    assert_eq!(metrics.discontinuity_candidates, 1);
    assert_eq!(
        analyzer
            .observe_f32_frame(&[1.25], 48_000, 1)
            .unwrap()
            .discontinuity_candidates,
        1
    );
}

#[test]
fn true_peak_history_is_independent_of_worker_block_size() {
    let samples = (0..511)
        .map(|i| (std::f32::consts::TAU * 0.25 * i as f32 + 0.7).sin())
        .collect::<Vec<_>>();
    let mut contiguous = AudioSignalAnalyzer::new(AudioDiagnosticsConfig::default());
    let expected = contiguous
        .observe_f32_frame(&samples, 48_000, 1)
        .unwrap()
        .true_peak_q15;
    let mut split = AudioSignalAnalyzer::new(AudioDiagnosticsConfig::default());
    let actual = samples
        .chunks(13)
        .map(|s| split.observe_f32_frame(s, 48_000, 1).unwrap().true_peak_q15)
        .max()
        .unwrap();
    assert_eq!(actual, expected);
}
