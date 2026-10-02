//! Shared audio contracts and DSP, independent of transports and device APIs.
//!
//! Stateful processors belong to one worker. Device callbacks exchange samples
//! through bounded, preallocated ports; no SDK, async runtime or UI is required.

pub mod activity;
pub mod backend;
pub mod block;
pub mod channel;
pub mod clock;
pub mod diagnostics;
pub mod error;
pub mod format;
#[cfg(feature = "resampling")]
pub mod graph;
pub mod limiter;
pub mod mix;
#[cfg(feature = "resampling")]
pub mod resample;
pub mod resample_format;
pub mod spsc;
#[cfg(feature = "resampling")]
pub mod stream_resample;

pub use block::{
    AudioBlock, BlockContext, ClockDomain, ClockTimestamp, SourceId, SourceKey, StreamEpoch,
    StreamId, StreamKind, TimestampQuality,
};
pub use error::{AudioError, AudioResult};
pub use format::{AudioFormat, ChannelLayout, PacketDuration, SampleFrames};
