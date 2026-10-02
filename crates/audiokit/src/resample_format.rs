//! Legacy packet-shaped filter boundary; new graph PCM uses AudioFormat instead.
use crate::{AudioError, AudioFormat, AudioResult, ChannelLayout};

/// Validated fixed worker block for the accepted legacy Rubato profiles.
/// Public fields are retained for migration only; constructors revalidate them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResampleBlockFormat {
    /// Per-channel samples per second.
    pub sample_rate: u32,
    /// Interleaved channel count, 1..=32.
    pub channels: u8,
    /// Legacy worker block duration, not part of AudioFormat.
    pub ptime_ms: u16,
    /// Per-channel frames per worker block.
    pub frame_samples: usize,
}
impl ResampleBlockFormat {
    /// Validates integral block sizes and checked address-space arithmetic.
    pub fn new(sample_rate: u32, channels: u8, ptime_ms: u16) -> AudioResult<Self> {
        AudioFormat::new(sample_rate, ChannelLayout::Discrete(channels))?;
        if ptime_ms == 0 || ptime_ms > 120 {
            return Err(AudioError::InvalidConfig(
                "resample block must be 1..=120 ms".into(),
            ));
        }
        let numerator = u64::from(sample_rate) * u64::from(ptime_ms);
        if !numerator.is_multiple_of(1000) {
            return Err(AudioError::InvalidConfig(
                "resample block has fractional sample frames".into(),
            ));
        }
        let frame_samples = usize::try_from(numerator / 1000)
            .map_err(|_| AudioError::ResourceExhausted("resample block overflow".into()))?;
        frame_samples
            .checked_mul(usize::from(channels))
            .ok_or_else(|| AudioError::ResourceExhausted("resample buffer overflow".into()))?;
        Ok(Self {
            sample_rate,
            channels,
            ptime_ms,
            frame_samples,
        })
    }
    /// Returns interleaved samples after construction validation.
    pub fn interleaved_samples(self) -> usize {
        self.frame_samples * usize::from(self.channels)
    }
    /// Rejects inconsistent publicly constructed legacy shapes.
    pub fn validate(self) -> AudioResult<Self> {
        let expected = Self::new(self.sample_rate, self.channels, self.ptime_ms)?;
        if expected != self {
            return Err(AudioError::InvalidConfig(
                "inconsistent resample block shape".into(),
            ));
        }
        Ok(self)
    }
}
