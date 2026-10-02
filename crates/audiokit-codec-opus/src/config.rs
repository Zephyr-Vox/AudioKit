//! Shared encoder controls and per-enabled-stream payload budgeting.
use audiokit::{AudioError, AudioResult, PacketDuration};
use thiserror::Error;

/// Maximum accepted Opus payload bytes, below libopus's signed length limit.
pub const MAX_OPUS_PACKET: usize = 4000;

/// Encoding policy chosen by the host when explicitly registering a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusProfile {
    /// Mono VoIP profile, 64..=128 kbps.
    Voice,
    /// Stereo music profile, 128..=320 kbps.
    Desktop,
    /// Host-registered media with libopus automatic bitrate.
    Other,
}

/// Client-wide bitrate bound or default, in kilobits per second.
pub const MIN_VOICE_BITRATE_KBPS: u16 = 64;
/// Client-wide bitrate bound or default, in kilobits per second.
pub const MAX_VOICE_BITRATE_KBPS: u16 = 128;
/// Client-wide bitrate bound or default, in kilobits per second.
pub const DEFAULT_VOICE_BITRATE_KBPS: u16 = 96;
/// Client-wide bitrate bound or default, in kilobits per second.
pub const MIN_DESKTOP_BITRATE_KBPS: u16 = 128;
/// Client-wide bitrate bound or default, in kilobits per second.
pub const MAX_DESKTOP_BITRATE_KBPS: u16 = 320;
/// Client-wide bitrate bound or default, in kilobits per second.
pub const DEFAULT_DESKTOP_BITRATE_KBPS: u16 = 196;

/// Encoder settings shared with compatible Opus clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpusConfig {
    /// Enables redundant low-rate data for recovering the previous packet.
    pub in_band_fec: bool,
    /// Loss percentage the encoder should expect when deciding FEC effort.
    pub expected_packet_loss_percent: u8,
    /// Target voice bitrate in kilobits per second.
    pub voice_bitrate_kbps: u16,
    /// Target desktop-audio bitrate in kilobits per second.
    pub desktop_bitrate_kbps: u16,
}

impl Default for OpusConfig {
    fn default() -> Self {
        Self {
            in_band_fec: true,
            expected_packet_loss_percent: 10,
            voice_bitrate_kbps: DEFAULT_VOICE_BITRATE_KBPS,
            desktop_bitrate_kbps: DEFAULT_DESKTOP_BITRATE_KBPS,
        }
    }
}

impl OpusConfig {
    /// Validates encoder controls before a codec is created.
    pub fn validate(self) -> Result<Self, OpusConfigError> {
        if self.expected_packet_loss_percent > 100 {
            return Err(OpusConfigError::PacketLossOutOfRange);
        }
        if !(MIN_VOICE_BITRATE_KBPS..=MAX_VOICE_BITRATE_KBPS).contains(&self.voice_bitrate_kbps) {
            return Err(OpusConfigError::VoiceBitrateOutOfRange);
        }
        if !(MIN_DESKTOP_BITRATE_KBPS..=MAX_DESKTOP_BITRATE_KBPS)
            .contains(&self.desktop_bitrate_kbps)
        {
            return Err(OpusConfigError::DesktopBitrateOutOfRange);
        }
        Ok(self)
    }
}

/// Invalid Opus encoder settings.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum OpusConfigError {
    /// Expected packet loss must be a percentage from zero to one hundred.
    #[error("expected_packet_loss_percent must be 0..=100")]
    PacketLossOutOfRange,
    /// Voice bitrate must follow the client-wide voice quality bounds.
    #[error("voice_bitrate_kbps must be 64..=128")]
    VoiceBitrateOutOfRange,
    /// Desktop bitrate must follow the client-wide desktop-audio quality bounds.
    #[error("desktop_bitrate_kbps must be 128..=320")]
    DesktopBitrateOutOfRange,
}

/// Clamps only enabled profiles. An unavailable desktop stream never invalidates voice.
/// Configuration ranges are still validated for future stream registration.
pub fn effective_config_for_payload(
    config: OpusConfig,
    max_payload: usize,
    duration: PacketDuration,
    desktop_enabled: bool,
) -> AudioResult<OpusConfig> {
    let config = config
        .validate()
        .map_err(|e| AudioError::InvalidConfig(e.to_string()))?;
    let voice_bitrate_kbps = effective_bitrate(
        config.voice_bitrate_kbps,
        MIN_VOICE_BITRATE_KBPS,
        max_payload,
        duration,
    )?;
    let desktop_bitrate_kbps = if desktop_enabled {
        effective_bitrate(
            config.desktop_bitrate_kbps,
            MIN_DESKTOP_BITRATE_KBPS,
            max_payload,
            duration,
        )?
    } else {
        config.desktop_bitrate_kbps
    };
    Ok(OpusConfig {
        voice_bitrate_kbps,
        desktop_bitrate_kbps,
        ..config
    })
}

pub(crate) fn effective_bitrate(
    requested: u16,
    minimum: u16,
    max_payload: usize,
    duration: PacketDuration,
) -> AudioResult<u16> {
    if max_payload == 0 || max_payload > MAX_OPUS_PACKET {
        return Err(AudioError::InvalidConfig(
            "Opus payload budget must be 1..=4000 bytes".into(),
        ));
    }
    // bytes * 8 / milliseconds is kbit/s. 10% reserves framing and VBR headroom;
    // the encoder also receives the hard packet capacity, since averages are not bounds.
    let budget = (max_payload as u128 * 8 / u128::from(duration.milliseconds())) * 90 / 100;
    let rate = u128::from(requested).min(budget);
    if rate < u128::from(minimum) {
        return Err(AudioError::Unsupported(format!(
            "Opus packet budget at {} ms and {max_payload} payload bytes cannot meet the {minimum} kbps stream minimum",
            duration.milliseconds()
        )));
    }
    Ok(rate as u16)
}
