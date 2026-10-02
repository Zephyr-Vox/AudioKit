//! An SDK-free worker example: APM quantum is independent of transport ptime.

use audiokit::backend::VoiceProcessor;
use audiokit::{AudioFormat, ChannelLayout};
use audiokit_processing_sonora::{AudioProcessingOptions, SonoraProcessor};

fn main() -> audiokit::AudioResult<()> {
    let capture = AudioFormat::new(48_000, ChannelLayout::Mono)?;
    let reference = AudioFormat::new(48_000, ChannelLayout::Stereo)?;
    let mut processor =
        SonoraProcessor::new(capture, reference, true, AudioProcessingOptions::default())?;
    processor.set_delay_ms(40)?;
    let mut captured = [0.0_f32; 480];
    processor.analyze_render(&[0.0_f32; 960])?;
    processor.process_capture(&mut captured)?;
    println!(
        "10 ms capture: {} mono samples, delay={:?}",
        captured.len(),
        processor.algorithmic_delay()
    );
    Ok(())
}
