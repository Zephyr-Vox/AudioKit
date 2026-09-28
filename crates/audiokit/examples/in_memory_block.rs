//! A transport-free PCM boundary: callback size need not equal media ptime.

use audiokit::{
    AudioBlock, AudioFormat, BlockContext, ChannelLayout, ClockDomain, ClockTimestamp,
    PacketDuration, StreamEpoch, TimestampQuality,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let format = AudioFormat::new(48_000, ChannelLayout::Mono)?;
    let context = BlockContext {
        source: None,
        epoch: StreamEpoch(1),
        timestamp: ClockTimestamp {
            domain: ClockDomain::new(1)?,
            sample_position: 0,
            monotonic_ns: None,
            quality: TimestampQuality::Unavailable,
            uncertainty_ns: None,
        },
    };
    let block = AudioBlock::new(format, context, vec![0.25; 256])?;
    assert_eq!(block.frames().get(), 256);
    assert_eq!(PacketDuration::Ms20.frames(format)?.get(), 960);
    Ok(())
}
