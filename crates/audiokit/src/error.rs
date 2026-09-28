//! Errors at audio boundaries, without protocol or device-backend types.

use thiserror::Error;

/// A result returned by an audio operation.
pub type AudioResult<T> = Result<T, AudioError>;

/// A recoverable audio failure. The host decides session recovery policy.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AudioError {
    /// A format, parameter or timestamp violates a documented invariant.
    #[error("invalid audio configuration: {0}")]
    InvalidConfig(String),
    /// PCM has an incomplete layout, wrong length or non-finite samples.
    #[error("invalid PCM: {0}")]
    InvalidFrame(String),
    /// A requested backend, layout or operation is unavailable.
    #[error("unsupported audio capability: {0}")]
    Unsupported(String),
    /// A bounded queue, output buffer or source budget is exhausted.
    #[error("audio resource limit: {0}")]
    ResourceExhausted(String),
    /// An encoded packet cannot be accepted by the selected decoder.
    #[error("malformed audio packet: {0}")]
    MalformedPacket(String),
    /// A device became unavailable after opening.
    #[error("audio device lost: {0}")]
    DeviceLost(String),
    /// A processing or codec backend failed.
    #[error("audio processing failed: {0}")]
    Processing(String),
    /// A host cancelled the operation or closed the port.
    #[error("audio operation cancelled")]
    Cancelled,
}
