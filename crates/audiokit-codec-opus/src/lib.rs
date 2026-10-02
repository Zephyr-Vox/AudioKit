//! Transport-independent Opus backend. Instances belong to one stream/epoch worker.
//! Allocation happens during construction or in explicitly named legacy helpers,
//! never in the caller-buffer f32 interfaces. The native libopus C toolchain is
//! required only by this optional backend, not by the AudioKit core.

mod config;
pub use config::*;

use audiokit::backend::{AudioDecoder, AudioEncoder, DecodeRequest};
use audiokit::{AudioError, AudioFormat, AudioResult, PacketDuration, SampleFrames};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};

fn backend(error: opus::Error) -> AudioError {
    AudioError::Processing(format!("Opus: {error}"))
}

fn channels(format: AudioFormat) -> AudioResult<Channels> {
    if !matches!(
        format.sample_rate_hz(),
        8_000 | 12_000 | 16_000 | 24_000 | 48_000
    ) {
        return Err(AudioError::Unsupported(
            "Opus rate must be 8/12/16/24/48 kHz".into(),
        ));
    }
    match format.channels() {
        1 => Ok(Channels::Mono),
        2 => Ok(Channels::Stereo),
        _ => Err(AudioError::Unsupported(
            "Opus channel count must be mono or stereo".into(),
        )),
    }
}

/// One exclusive encoder; independent outbound streams never share prediction state.
pub struct OpusEncoder {
    encoder: Encoder,
    format: AudioFormat,
    duration: PacketDuration,
    samples: usize,
    max_payload: usize,
    lookahead: SampleFrames,
    profile: OpusProfile,
}

impl OpusEncoder {
    /// Builds a validated encoder. Voice requires mono/VoIP; desktop requires stereo/Audio.
    /// Other explicitly registered streams use Audio with the format's layout and auto bitrate.
    /// Packet buffers must hold the entire negotiated payload budget before state is advanced.
    pub fn new(
        format: AudioFormat,
        duration: PacketDuration,
        profile: OpusProfile,
        config: OpusConfig,
        max_payload: usize,
    ) -> AudioResult<Self> {
        let config = config
            .validate()
            .map_err(|e| AudioError::InvalidConfig(e.to_string()))?;
        let channel_mode = channels(format)?;
        let (application, rate) = match profile {
            OpusProfile::Voice if format.channels() == 1 => (
                Application::Voip,
                Some(effective_bitrate(
                    config.voice_bitrate_kbps,
                    MIN_VOICE_BITRATE_KBPS,
                    max_payload,
                    duration,
                )?),
            ),
            OpusProfile::Desktop if format.channels() == 2 => (
                Application::Audio,
                Some(effective_bitrate(
                    config.desktop_bitrate_kbps,
                    MIN_DESKTOP_BITRATE_KBPS,
                    max_payload,
                    duration,
                )?),
            ),
            OpusProfile::Other => (Application::Audio, None),
            _ => {
                return Err(AudioError::Unsupported(
                    "voice requires mono; desktop requires stereo".into(),
                ));
            }
        };
        if max_payload == 0 || max_payload > MAX_OPUS_PACKET {
            return Err(AudioError::InvalidConfig(
                "Opus payload budget must be 1..=4000 bytes".into(),
            ));
        }
        let mut encoder =
            Encoder::new(format.sample_rate_hz(), channel_mode, application).map_err(backend)?;
        encoder
            .set_inband_fec(config.in_band_fec)
            .map_err(backend)?;
        encoder
            .set_packet_loss_perc(i32::from(config.expected_packet_loss_percent))
            .map_err(backend)?;
        if let Some(rate) = rate {
            encoder
                .set_bitrate(Bitrate::Bits(i32::from(rate) * 1_000))
                .map_err(backend)?;
        }
        let lookahead = encoder.get_lookahead().map_err(backend)?;
        let lookahead = u64::try_from(lookahead)
            .map_err(|_| AudioError::Processing("negative Opus lookahead".into()))?;
        Ok(Self {
            encoder,
            format,
            duration,
            samples: duration.frames(format)?.interleaved_samples(format)?,
            max_payload,
            lookahead: SampleFrames::new(lookahead),
            profile,
        })
    }

    fn validate_buffers(&self, samples: usize, output: usize) -> AudioResult<()> {
        if samples != self.samples {
            return Err(AudioError::InvalidFrame(
                "Opus input must contain exactly one ptime".into(),
            ));
        }
        if output < self.max_payload {
            return Err(AudioError::ResourceExhausted(
                "Opus packet buffer is smaller than payload budget".into(),
            ));
        }
        Ok(())
    }

    /// Compatibility boundary for hosts still supplying protected i16 PCM; allocates one packet.
    pub fn encode_i16(&mut self, pcm: &[i16]) -> AudioResult<Vec<u8>> {
        self.validate_buffers(pcm.len(), self.max_payload)?;
        let mut output = vec![0; self.max_payload];
        let written = self.encoder.encode(pcm, &mut output).map_err(backend)?;
        output.truncate(written);
        Ok(output)
    }

    /// Reads applied libopus bitrate for diagnostics; requires the owning worker.
    pub fn bitrate(&mut self) -> AudioResult<Bitrate> {
        self.encoder.get_bitrate().map_err(backend)
    }
    /// Applies an exact bit/s target without silently rounding to the configuration's kbps units.
    /// Profile limits and a 10% payload headroom are enforced before touching codec state.
    pub fn set_target_bitrate_bps(&mut self, bits_per_second: u32) -> AudioResult<()> {
        let (minimum, maximum) = match self.profile {
            OpusProfile::Voice => (MIN_VOICE_BITRATE_KBPS, MAX_VOICE_BITRATE_KBPS),
            OpusProfile::Desktop => (MIN_DESKTOP_BITRATE_KBPS, MAX_DESKTOP_BITRATE_KBPS),
            OpusProfile::Other => {
                return Err(AudioError::Unsupported(
                    "Other profile uses automatic bitrate".into(),
                ));
            }
        };
        let budget =
            self.max_payload as u64 * 8 * 1000 / u64::from(self.duration.milliseconds()) * 90 / 100;
        if !(u32::from(minimum) * 1000..=u32::from(maximum) * 1000).contains(&bits_per_second)
            || u64::from(bits_per_second) > budget
        {
            return Err(AudioError::InvalidConfig(
                "bitrate exceeds profile or payload budget".into(),
            ));
        }
        self.encoder
            .set_bitrate(Bitrate::Bits(bits_per_second as i32))
            .map_err(backend)
    }
    /// Reads the applied FEC control, not whether an individual packet contains redundancy.
    pub fn in_band_fec(&mut self) -> AudioResult<bool> {
        self.encoder.get_inband_fec().map_err(backend)
    }
    /// Reads the applied loss-estimation percentage.
    pub fn expected_packet_loss_percent(&mut self) -> AudioResult<i32> {
        self.encoder.get_packet_loss_perc().map_err(backend)
    }
    /// Sets optional libopus CPU/quality complexity, 0..=10, on the owning worker.
    pub fn set_complexity(&mut self, complexity: u8) -> AudioResult<()> {
        if complexity > 10 {
            return Err(AudioError::InvalidConfig(
                "Opus complexity must be 0..=10".into(),
            ));
        }
        self.encoder
            .set_complexity(i32::from(complexity))
            .map_err(backend)
    }
    /// Reports the applied complexity rather than assuming a libopus default.
    pub fn complexity(&mut self) -> AudioResult<i32> {
        self.encoder.get_complexity().map_err(backend)
    }
}

impl AudioEncoder for OpusEncoder {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn packet_duration(&self) -> PacketDuration {
        self.duration
    }
    fn lookahead(&self) -> Option<SampleFrames> {
        Some(self.lookahead)
    }
    fn encode_into(&mut self, pcm: &[f32], output: &mut [u8]) -> AudioResult<usize> {
        self.validate_buffers(pcm.len(), output.len())?;
        if !pcm.iter().all(|s| s.is_finite()) {
            return Err(AudioError::InvalidFrame("non-finite Opus PCM".into()));
        }
        self.encoder
            .encode_float(pcm, &mut output[..self.max_payload])
            .map_err(backend)
    }
}

/// Decoder exclusively owned by a source/stream/epoch. Voice output remains mono.
pub struct OpusDecoder {
    decoder: Decoder,
    format: AudioFormat,
    duration: PacketDuration,
    frames: SampleFrames,
    samples: usize,
}

impl OpusDecoder {
    /// Creates a decoder with one fixed ptime; legacy stereo voice is downmixed by libopus.
    pub fn new(format: AudioFormat, duration: PacketDuration) -> AudioResult<Self> {
        let mode = channels(format)?;
        let frames = duration.frames(format)?;
        Ok(Self {
            decoder: Decoder::new(format.sample_rate_hz(), mode).map_err(backend)?,
            format,
            duration,
            frames,
            samples: frames.interleaved_samples(format)?,
        })
    }

    fn validate_request<'a>(
        &self,
        request: DecodeRequest<'a>,
        output: usize,
    ) -> AudioResult<(&'a [u8], bool)> {
        if output < self.samples {
            return Err(AudioError::ResourceExhausted(
                "Opus PCM output buffer is undersized".into(),
            ));
        }
        let (payload, fec) = match request {
            DecodeRequest::Loss => return Ok((&[], false)),
            DecodeRequest::Packet(payload) => (payload, false),
            DecodeRequest::Fec(payload) => (payload, true),
        };
        if payload.is_empty() || payload.len() > MAX_OPUS_PACKET {
            return Err(AudioError::MalformedPacket(
                "empty or oversized Opus packet".into(),
            ));
        }
        let frames = opus::packet::get_nb_samples(payload, self.format.sample_rate_hz())
            .map_err(|e| AudioError::MalformedPacket(e.to_string()))?;
        if frames as u64 != self.frames.get() {
            return Err(AudioError::MalformedPacket(
                "Opus packet duration differs from negotiated ptime".into(),
            ));
        }
        Ok((payload, fec))
    }

    /// Legacy i16 boundary; allocates a single fixed-duration frame, with identical libopus calls.
    pub fn decode_i16(&mut self, request: DecodeRequest<'_>) -> AudioResult<Vec<i16>> {
        let (payload, fec) = self.validate_request(request, self.samples)?;
        let mut output = vec![0; self.samples];
        let frames = self
            .decoder
            .decode(payload, &mut output, fec)
            .map_err(backend)?;
        if frames as u64 != self.frames.get() {
            return Err(AudioError::Processing(
                "Opus produced an unexpected duration".into(),
            ));
        }
        Ok(output)
    }
}

impl AudioDecoder for OpusDecoder {
    fn format(&self) -> AudioFormat {
        self.format
    }
    fn packet_duration(&self) -> PacketDuration {
        self.duration
    }
    fn reset(&mut self) -> AudioResult<()> {
        self.decoder.reset_state().map_err(backend)
    }
    fn decode_into(
        &mut self,
        request: DecodeRequest<'_>,
        output: &mut [f32],
    ) -> AudioResult<SampleFrames> {
        let (payload, fec) = self.validate_request(request, output.len())?;
        let frames = self
            .decoder
            .decode_float(payload, &mut output[..self.samples], fec)
            .map_err(backend)?;
        if frames as u64 != self.frames.get() {
            return Err(AudioError::Processing(
                "Opus produced an unexpected duration".into(),
            ));
        }
        Ok(self.frames)
    }
}
