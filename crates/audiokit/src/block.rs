//! Opaque source identity and clock-tagged PCM, without server-specific metadata.

use crate::{AudioError, AudioFormat, AudioResult, SampleFrames};
use serde::{Deserialize, Serialize};
use std::{
    num::{NonZeroU16, NonZeroU64},
    ops::Range,
};

/// A nonzero opaque source identity chosen by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(NonZeroU64);
impl SourceId {
    /// Constructs an opaque identity; zero is reserved for absent source metadata.
    pub fn new(value: u64) -> AudioResult<Self> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or_else(|| AudioError::InvalidConfig("source id must be nonzero".into()))
    }
    /// Returns the host-assigned identity, without interpreting it as an account ID.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A nonzero logical stream identity within a source, mapped by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamId(NonZeroU16);
impl StreamId {
    /// Constructs a stream identity; transport heartbeats are not audio streams.
    pub fn new(value: u16) -> AudioResult<Self> {
        NonZeroU16::new(value)
            .map(Self)
            .ok_or_else(|| AudioError::InvalidConfig("stream id must be nonzero".into()))
    }
    /// Returns the host-assigned stream identity.
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

/// A source and logical stream. Decoder/DSP state must be isolated by this key and epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceKey {
    /// Opaque source identity.
    pub source: SourceId,
    /// Logical stream identity within that source.
    pub stream: StreamId,
}

/// A host-assigned generation that changes after reconnect or source restart.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct StreamEpoch(pub u64);

/// The processing policy associated with a stream, independent of protocol numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// Mono speech, with optional voice enhancement and a speech codec profile.
    Voice,
    /// Stereo desktop/media audio, bypassing voice enhancement.
    Desktop,
    /// A registered host-defined stream requiring explicit policy.
    Other,
}

/// A sample clock domain. Values from different domains cannot be directly subtracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClockDomain(NonZeroU64);
impl ClockDomain {
    /// Constructs a domain identity assigned by the host for a device or source epoch.
    pub fn new(value: u64) -> AudioResult<Self> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or_else(|| AudioError::InvalidConfig("clock domain must be nonzero".into()))
    }
    /// Returns the domain identity; this is not a wall-clock time or frequency.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Whether a clock observation is measured, inferred or unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampQuality {
    /// Reported by the device/host with a valid monotonic timestamp.
    Measured,
    /// Derived from sample counts or a host scheduling observation.
    Estimated,
    /// No valid mapping to the host monotonic clock is available.
    Unavailable,
}

/// Time of a block's first sample, in its own sample clock and the host monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockTimestamp {
    /// Domain of the sample cursor and its mapping to host monotonic time.
    pub domain: ClockDomain,
    /// First sample's per-channel frame position since this domain/epoch began.
    pub sample_position: u64,
    /// Nanoseconds in the host monotonic clock; never a Unix timestamp.
    pub monotonic_ns: Option<u64>,
    /// How this timestamp was obtained.
    pub quality: TimestampQuality,
    /// Estimated mapping error in nanoseconds; None means unknown, not zero.
    pub uncertainty_ns: Option<u64>,
}

impl ClockTimestamp {
    /// Rejects a claimed measurement without a timestamp, or an unavailable mapping with one.
    pub fn validate(self) -> AudioResult<Self> {
        if (self.quality == TimestampQuality::Measured && self.monotonic_ns.is_none())
            || (self.quality == TimestampQuality::Unavailable && self.monotonic_ns.is_some())
        {
            return Err(AudioError::InvalidConfig(
                "timestamp quality disagrees with its monotonic mapping".into(),
            ));
        }
        Ok(self)
    }
}

/// Identity and timing preserved with each block, including aggregate render blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockContext {
    /// None for an aggregate mix or an anonymous local source.
    pub source: Option<SourceKey>,
    /// Stream/device generation; sample cursors restart only with a new epoch.
    pub epoch: StreamEpoch,
    /// First sample timing, including an explicit unavailable state.
    pub timestamp: ClockTimestamp,
}

/// An owned interleaved f32 PCM block for workers, not device callbacks.
///
/// Magnitudes above unity are retained for limiter diagnostics. Construction
/// rejects non-finite PCM, incomplete layouts and overflowing sample ranges.
#[derive(Debug, Clone)]
pub struct AudioBlock {
    format: AudioFormat,
    context: BlockContext,
    samples: Vec<f32>,
    frames: SampleFrames,
}

impl AudioBlock {
    /// Takes ownership of already allocated PCM and validates its format, range and timing.
    pub fn new(format: AudioFormat, context: BlockContext, samples: Vec<f32>) -> AudioResult<Self> {
        context.timestamp.validate()?;
        let frames = format.frames_in(samples.len())?;
        if samples.iter().any(|sample| !sample.is_finite()) {
            return Err(AudioError::InvalidFrame(
                "PCM contains NaN or infinity".into(),
            ));
        }
        context
            .timestamp
            .sample_position
            .checked_add(frames.get())
            .ok_or_else(|| AudioError::InvalidFrame("sample range overflow".into()))?;
        Ok(Self {
            format,
            context,
            samples,
            frames,
        })
    }
    /// Returns the validated PCM format.
    pub const fn format(&self) -> AudioFormat {
        self.format
    }
    /// Returns source/epoch/time metadata without inspecting a transport protocol.
    pub const fn context(&self) -> BlockContext {
        self.context
    }
    /// Returns the per-channel length, which is independent of encoded-packet duration.
    pub const fn frames(&self) -> SampleFrames {
        self.frames
    }
    /// Returns the first..end per-channel sample range in this block's clock domain.
    pub fn sample_range(&self) -> Range<u64> {
        self.context.timestamp.sample_position
            ..self.context.timestamp.sample_position + self.frames.get()
    }
    /// Borrows interleaved PCM, including magnitudes above unity.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }
    /// Consumes this worker-owned block and returns its sample allocation.
    pub fn into_samples(self) -> Vec<f32> {
        self.samples
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChannelLayout;
    fn context(position: u64) -> BlockContext {
        BlockContext {
            source: None,
            epoch: StreamEpoch(4),
            timestamp: ClockTimestamp {
                domain: ClockDomain::new(1).unwrap(),
                sample_position: position,
                monotonic_ns: None,
                quality: TimestampQuality::Unavailable,
                uncertainty_ns: None,
            },
        }
    }
    #[test]
    fn block_preserves_overdrive_and_rejects_invalid_pcm_or_ranges() {
        let format = AudioFormat::new(48_000, ChannelLayout::Stereo).unwrap();
        let block = AudioBlock::new(format, context(12), vec![2.0, -2.0, 0.0, 0.0]).unwrap();
        assert_eq!(block.frames().get(), 2);
        assert_eq!(block.sample_range(), 12..14);
        assert_eq!(block.samples()[0], 2.0);
        assert!(AudioBlock::new(format, context(0), vec![0.0]).is_err());
        assert!(AudioBlock::new(format, context(0), vec![f32::NAN, 0.0]).is_err());
        assert!(AudioBlock::new(format, context(u64::MAX), vec![0.0, 0.0]).is_err());
    }
    #[test]
    fn timestamps_and_identifiers_do_not_accept_fabricated_values() {
        assert!(SourceId::new(0).is_err());
        assert!(serde_json::from_str::<StreamId>("0").is_err());
        let mut timestamp = context(0).timestamp;
        timestamp.quality = TimestampQuality::Measured;
        assert!(timestamp.validate().is_err());
        timestamp.monotonic_ns = Some(50);
        assert!(timestamp.validate().is_ok());
    }
}
