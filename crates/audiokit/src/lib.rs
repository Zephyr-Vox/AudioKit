//! Shared audio contracts and DSP, independent of transports and device APIs.
//!
//! Stateful processors belong to one worker. Device callbacks exchange samples
//! through bounded, preallocated ports; no SDK, async runtime or UI is required.

pub mod backend;
pub mod block;
pub mod error;
pub mod format;

pub use block::{
    AudioBlock, BlockContext, ClockDomain, ClockTimestamp, SourceId, SourceKey, StreamEpoch,
    StreamId, StreamKind, TimestampQuality,
};
pub use error::{AudioError, AudioResult};
pub use format::{AudioFormat, ChannelLayout, PacketDuration, SampleFrames};
