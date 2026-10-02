//! Explicit worker-side channel mapping; mono expansion happens only at render.
use crate::{AudioError, AudioFormat, AudioResult};

/// Maps complete f32 frames. Preserves overdrive for downstream protection.
/// Equal layouts copy; mono expands; downmix/fallback uses a gain-neutral average.
/// Discrete layout averaging is explicit, not an inferred spatial speaker matrix.
pub fn map_channels(input: AudioFormat, output: AudioFormat, pcm: &[f32]) -> AudioResult<Vec<f32>> {
    if input.sample_rate_hz() != output.sample_rate_hz() {
        return Err(AudioError::InvalidConfig(
            "channel mapper cannot change sample rate".into(),
        ));
    }
    let frames = input.frames_in(pcm.len())?;
    if !pcm.iter().all(|v| v.is_finite()) {
        return Err(AudioError::InvalidFrame("non-finite channel input".into()));
    }
    if input.layout() == output.layout() {
        return Ok(pcm.to_vec());
    }
    let mut result = Vec::with_capacity(frames.interleaved_samples(output)?);
    for frame in pcm.chunks_exact(usize::from(input.channels())) {
        let sample = frame.iter().map(|s| f64::from(*s)).sum::<f64>() / f64::from(input.channels());
        result.extend(std::iter::repeat_n(
            sample as f32,
            usize::from(output.channels()),
        ));
    }
    Ok(result)
}
