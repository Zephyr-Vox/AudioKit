//! Validated PCM layouts and packet durations, with explicit sample-frame units.

use crate::{AudioError, AudioResult};
use serde::{Deserialize, Serialize};

/// Channel identity, distinct from the number of samples in a media packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayout {
    /// One signal. Duplicating it for stereo playback does not create spatial data.
    Mono,
    /// Interleaved left and right channels.
    Stereo,
    /// An explicitly unspecified device layout; a host must choose its channel matrix.
    Discrete(u8),
}

impl ChannelLayout {
    /// Returns the channel count. An AudioFormat validates discrete counts before use.
    pub const fn channels(self) -> u8 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Discrete(n) => n,
        }
    }
}

/// PCM rate and layout. Deserialization uses the same validation as construction.
///
/// Rates are 1..=384000 Hz; layouts have 1..=32 channels. Codec backends may
/// support a narrower set. Packet duration is intentionally not part of a format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "FormatFields", into = "FormatFields")]
pub struct AudioFormat {
    sample_rate_hz: u32,
    layout: ChannelLayout,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatFields {
    sample_rate_hz: u32,
    layout: ChannelLayout,
}

impl AudioFormat {
    /// Constructs a validated format, rejecting zero/excessive rates and channel counts.
    pub fn new(sample_rate_hz: u32, layout: ChannelLayout) -> AudioResult<Self> {
        if !(1..=384_000).contains(&sample_rate_hz) || !(1..=32).contains(&layout.channels()) {
            return Err(AudioError::InvalidConfig(
                "rate must be 1..=384000 Hz and channels 1..=32".into(),
            ));
        }
        Ok(Self {
            sample_rate_hz,
            layout,
        })
    }

    /// Returns samples per second in each channel.
    pub const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }
    /// Returns channel identity, including explicitly unspecified layouts.
    pub const fn layout(self) -> ChannelLayout {
        self.layout
    }
    /// Returns the number of interleaved channels.
    pub const fn channels(self) -> u8 {
        self.layout.channels()
    }

    /// Validates a sample slice length and returns per-channel sample frames.
    pub fn frames_in(self, interleaved_samples: usize) -> AudioResult<SampleFrames> {
        let channels = usize::from(self.channels());
        if !interleaved_samples.is_multiple_of(channels) {
            return Err(AudioError::InvalidFrame(
                "incomplete interleaved channel group".into(),
            ));
        }
        let frames = u64::try_from(interleaved_samples / channels)
            .map_err(|_| AudioError::InvalidFrame("frame count cannot be represented".into()))?;
        Ok(SampleFrames::new(frames))
    }
}

impl TryFrom<FormatFields> for AudioFormat {
    type Error = AudioError;
    fn try_from(fields: FormatFields) -> AudioResult<Self> {
        Self::new(fields.sample_rate_hz, fields.layout)
    }
}
impl From<AudioFormat> for FormatFields {
    fn from(format: AudioFormat) -> Self {
        Self {
            sample_rate_hz: format.sample_rate_hz,
            layout: format.layout,
        }
    }
}

/// A number of samples in each channel, never an interleaved sample count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SampleFrames(u64);

impl SampleFrames {
    /// Wraps a per-channel count, including zero for an empty read or stream tail.
    pub const fn new(frames: u64) -> Self {
        Self(frames)
    }
    /// Returns the per-channel count.
    pub const fn get(self) -> u64 {
        self.0
    }
    /// Converts to a host slice length, checking both arithmetic and address-space limits.
    pub fn interleaved_samples(self, format: AudioFormat) -> AudioResult<usize> {
        self.0
            .checked_mul(u64::from(format.channels()))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| AudioError::ResourceExhausted("PCM sample count overflow".into()))
    }
    /// Expresses this count as milliseconds in the specified format's clock.
    pub fn milliseconds(self, format: AudioFormat) -> f64 {
        self.0 as f64 * 1_000.0 / f64::from(format.sample_rate_hz())
    }
}

/// A session's fixed encoded-packet duration, independent of device callback size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub enum PacketDuration {
    /// Ten milliseconds; also the voice processor's sub-block duration.
    Ms10,
    /// Twenty milliseconds; the current default.
    #[default]
    Ms20,
    /// Forty milliseconds.
    Ms40,
    /// Sixty milliseconds.
    Ms60,
}

impl PacketDuration {
    /// Returns milliseconds per encoded packet.
    pub const fn milliseconds(self) -> u16 {
        match self {
            Self::Ms10 => 10,
            Self::Ms20 => 20,
            Self::Ms40 => 40,
            Self::Ms60 => 60,
        }
    }
    /// Returns an exact sample count or rejects a rate with fractional sample frames.
    pub fn frames(self, format: AudioFormat) -> AudioResult<SampleFrames> {
        let numerator = u64::from(format.sample_rate_hz()) * u64::from(self.milliseconds());
        if !numerator.is_multiple_of(1_000) {
            return Err(AudioError::Unsupported(
                "packet duration has fractional sample frames at this rate".into(),
            ));
        }
        Ok(SampleFrames::new(numerator / 1_000))
    }
    /// Chooses an advertised duration in the client's fixed 20/10/40/60 preference order.
    /// The host still owns protocol negotiation and passes the final duration to its graphs.
    pub fn select(advertised_ms: &[u16]) -> Option<Self> {
        [Self::Ms20, Self::Ms10, Self::Ms40, Self::Ms60]
            .into_iter()
            .find(|duration| advertised_ms.contains(&duration.milliseconds()))
    }
}
impl TryFrom<u16> for PacketDuration {
    type Error = AudioError;
    fn try_from(value: u16) -> AudioResult<Self> {
        match value {
            10 => Ok(Self::Ms10),
            20 => Ok(Self::Ms20),
            40 => Ok(Self::Ms40),
            60 => Ok(Self::Ms60),
            _ => Err(AudioError::Unsupported(
                "ptime must be 10, 20, 40 or 60 ms".into(),
            )),
        }
    }
}
impl From<PacketDuration> for u16 {
    fn from(value: PacketDuration) -> Self {
        value.milliseconds()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deserialization_cannot_bypass_format_validation() {
        for input in [
            r#"{"sample_rate_hz":0,"layout":"mono"}"#,
            r#"{"sample_rate_hz":48000,"layout":{"discrete":0}}"#,
            r#"{"sample_rate_hz":48000,"layout":{"discrete":33}}"#,
        ] {
            assert!(serde_json::from_str::<AudioFormat>(input).is_err());
        }
    }
    #[test]
    fn sample_units_and_packet_clock_are_independent() {
        let stereo = AudioFormat::new(48_000, ChannelLayout::Stereo).unwrap();
        assert_eq!(PacketDuration::Ms20.frames(stereo).unwrap().get(), 960);
        assert_eq!(stereo.frames_in(512).unwrap().get(), 256);
        assert!(stereo.frames_in(513).is_err());
        assert!(
            SampleFrames::new(u64::MAX)
                .interleaved_samples(stereo)
                .is_err()
        );
        assert_eq!(
            PacketDuration::select(&[60, 10, 20]),
            Some(PacketDuration::Ms20)
        );
        assert_eq!(PacketDuration::select(&[10]), Some(PacketDuration::Ms10));
        assert!(serde_json::from_str::<PacketDuration>("15").is_err());
    }
}
