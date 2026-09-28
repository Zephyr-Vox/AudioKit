//! Synchronous worker contracts for codecs, voice processing and native ports.
//!
//! Hosts own transport negotiation and worker scheduling. Implementations must
//! describe allocation, buffering and thread-affinity constraints separately;
//! these worker interfaces are not themselves a callback-safety guarantee.

use crate::{AudioFormat, AudioResult, ClockTimestamp, PacketDuration, SampleFrames};

/// A single-stream encoder, exclusively owned by one worker for its lifetime.
pub trait AudioEncoder: Send {
    /// Returns the PCM format accepted by the encoder.
    fn format(&self) -> AudioFormat;
    /// Returns the fixed duration selected at session setup.
    fn packet_duration(&self) -> PacketDuration;
    /// Encodes exactly one packet of PCM into a caller-owned output buffer.
    /// Returns bytes written; implementations reject undersized buffers and never truncate packets.
    fn encode_into(&mut self, pcm: &[f32], output: &mut [u8]) -> AudioResult<usize>;
    /// Returns algorithmic lookahead in the encoder's PCM sample clock, or None if unknown.
    fn lookahead(&self) -> Option<SampleFrames>;
}

/// One decoder-state advancement. FEC does not consume the following normal packet slot.
#[derive(Debug, Clone, Copy)]
pub enum DecodeRequest<'a> {
    /// Decode the next packet in source sequence order.
    Packet(&'a [u8]),
    /// Conceal exactly one known missing packet slot.
    Loss,
    /// Attempt the preceding lost slot using the following packet's optional redundancy.
    /// Successful decoding alone does not prove that redundancy was present.
    Fec(&'a [u8]),
}

/// A decoder owned by one source/stream/epoch. Independent streams never share history.
pub trait AudioDecoder: Send {
    /// Returns the decoded PCM format before render channel expansion.
    fn format(&self) -> AudioFormat;
    /// Returns this stream's fixed encoded-packet duration.
    fn packet_duration(&self) -> PacketDuration;
    /// Advances decoder state and writes complete per-channel frames to a caller-owned buffer.
    /// Packet duration is checked before advancing state; invalid packets must not corrupt history.
    fn decode_into(
        &mut self,
        request: DecodeRequest<'_>,
        output: &mut [f32],
    ) -> AudioResult<SampleFrames>;
    /// Clears prediction history on an explicitly signalled epoch change.
    fn reset(&mut self) -> AudioResult<()>;
}

/// Backend-neutral statistics; None denotes unavailable or not applicable data.
#[derive(Debug, Clone, Default)]
pub struct VoiceProcessingStats {
    /// Echo return loss in dB, if the backend and current signal permit measurement.
    pub echo_return_loss_db: Option<f64>,
    /// Echo return loss enhancement in dB, if available.
    pub echo_return_loss_enhancement_db: Option<f64>,
    /// Estimated render-to-capture delay in milliseconds.
    pub delay_ms: Option<i32>,
    /// Estimated residual echo likelihood, if the backend exposes it.
    pub residual_echo_likelihood: Option<f64>,
}

/// A persistent voice processor with separately configured capture/render formats.
///
/// One worker serializes capture and render operations. Processing uses 10 ms
/// sub-blocks; a host packetizer joins them into its independent media ptime.
pub trait VoiceProcessor: Send {
    /// Returns the input/output capture PCM format, normally mono.
    fn capture_format(&self) -> AudioFormat;
    /// Returns the render-reference format, which may be stereo at a different rate.
    fn render_format(&self) -> AudioFormat;
    /// Processes one complete 10 ms capture sub-block in place.
    fn process_capture(&mut self, pcm: &mut [f32]) -> AudioResult<()>;
    /// Analyzes one complete 10 ms reference sub-block without modifying playback audio.
    fn analyze_render(&mut self, pcm: &[f32]) -> AudioResult<()>;
    /// Applies an aligned delay estimate in milliseconds on the processing owner thread.
    fn set_delay_ms(&mut self, delay_ms: u32) -> AudioResult<()>;
    /// Returns available processing statistics without inventing zero for missing values.
    fn statistics(&self) -> VoiceProcessingStats;
    /// Reports capture-path algorithmic delay, or None if the backend does not expose it.
    fn algorithmic_delay(&self) -> Option<SampleFrames>;
}

/// The result of a nonblocking read from a preallocated capture/reference port.
#[derive(Debug, Clone, Copy)]
pub struct CaptureRead {
    /// Complete per-channel frames copied into the caller's PCM buffer.
    pub frames: SampleFrames,
    /// First copied frame's timing and measurement quality.
    pub timestamp: ClockTimestamp,
    /// Frames known to have been dropped before this read; distinct from silent PCM.
    pub gap_before: SampleFrames,
}

/// Native capture read by an audio worker. None means no block currently available, not EOF.
pub trait CapturePort: Send {
    /// Returns the port's native PCM format.
    fn format(&self) -> AudioFormat;
    /// Copies available PCM into an existing buffer without waiting for the device callback.
    fn read_into(&mut self, output: &mut [f32]) -> AudioResult<Option<CaptureRead>>;
    /// Stops capture and releases its producer using backend-specific thread ownership rules.
    fn stop(&mut self) -> AudioResult<()>;
}

/// Native playback written by an audio worker; device callbacks drain the bounded port.
pub trait PlaybackPort: Send {
    /// Returns the port's PCM format after output-clock correction.
    fn format(&self) -> AudioFormat;
    /// Writes complete frames without waiting; returns accepted frames, possibly a partial prefix.
    /// A rejected suffix must be counted and handled by the render graph's recovery policy.
    fn write(&mut self, pcm: &[f32]) -> AudioResult<SampleFrames>;
    /// Returns the device-consumed frame cursor; it includes underrun replacement samples.
    fn presented_frames(&self) -> SampleFrames;
    /// Stops playback. The host selects drain versus abort before calling this operation.
    fn stop(&mut self) -> AudioResult<()>;
}
