//! Explicit worker-owned WAV audition; never modifies the accepted artifact.
use audiokit::backend::PlaybackPort;
use audiokit::graph::capture_pcm::{CapturePcmConfig, CapturePcmGraph};
use audiokit::{AudioFormat, ChannelLayout, StreamKind};
use audiokit_platform::cpal::CpalPlayback;
use audiokit_testkit::{Cancellation, Error, ProgressEvent, ProgressUnit, Result};
use std::{
    path::Path,
    thread,
    time::{Duration, Instant},
};

pub(super) fn play(
    path: &Path,
    selected: Option<&str>,
    volume: f32,
    stop: &Cancellation,
    mut progress: impl FnMut(ProgressEvent),
) -> Result<String> {
    if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
        return Err(Error::Invalid("preview volume must be 0..=1".into()));
    }
    let (input, pcm) = audiokit_testkit::read_processed_wav(path)?;
    if stop.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let preferred = AudioFormat::new(input.sample_rate_hz(), ChannelLayout::Stereo)?;
    let mut port = CpalPlayback::open(selected, preferred, 120, 30)
        .map_err(|e| Error::Execution(e.to_string()))?;
    let output = port.format();
    if !matches!(output.layout(), ChannelLayout::Mono | ChannelLayout::Stereo) {
        return Err(Error::Capability(
            "preview requires mono/stereo output".into(),
        ));
    }
    let mut reference = port.take_reference();
    let mut graph = CapturePcmGraph::new(
        CapturePcmConfig {
            input_format: input,
            output_format: output,
            kind: if output.channels() == 1 {
                StreamKind::Voice
            } else {
                StreamKind::Desktop
            },
            resampler: Default::default(),
            max_ingress_ms: 20,
        },
        None,
    )?;
    let mut clamped = 0_u64;
    let chunks = (input.sample_rate_hz() as usize / 100).max(1) * usize::from(input.channels());
    let channels = usize::from(output.channels());
    let mut last_motion = Instant::now();
    let mut last_frames = 0;
    // Keep backlog below 60 ms in a 120 ms port. Never throw away an unaccepted suffix.
    {
        let mut feed = |block: Vec<f32>| -> Result<()> {
            let samples = block
                .into_iter()
                .map(|s| {
                    let scaled = s * volume;
                    if scaled.abs() > 0.999 {
                        clamped += 1;
                    }
                    scaled.clamp(-0.999, 0.999)
                })
                .collect::<Vec<_>>();
            let quantum = (output.sample_rate_hz() as usize / 100).max(1) * channels;
            for block in samples.chunks(quantum) {
                let mut offset = 0;
                while offset < block.len() {
                    if stop.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    if let Some(reader) = &mut reference {
                        while reader.pop().is_some() {}
                    }
                    let frames = port.presented_frames().get();
                    if frames != last_frames {
                        last_frames = frames;
                        last_motion = Instant::now();
                    }
                    if last_motion.elapsed() > Duration::from_secs(2) {
                        return Err(Error::Execution("preview device stopped consuming".into()));
                    }
                    if port.queued_frames() > output.sample_rate_hz() as usize * 60 / 1000 {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    let accepted = port.write(&block[offset..])?.get() as usize * channels;
                    offset += accepted;
                    if accepted == 0 {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
            Ok(())
        };
        for (index, block) in pcm.chunks(chunks).enumerate() {
            feed(graph.push_native(block)?)?;
            progress(ProgressEvent {
                input_frames: ((index + 1) * chunks).min(pcm.len()) as u64
                    / u64::from(input.channels()),
                total_frames: pcm.len() as u64 / u64::from(input.channels()),
                unit: ProgressUnit::InputFrames,
            });
        }
        feed(graph.finish()?)?;
        // Small files still need to cross the port's startup threshold. This silence
        // belongs only to audition, never the WAV or pipeline latency measurements.
        feed(vec![
            0.0;
            output.sample_rate_hz() as usize * 40 / 1000 * channels
        ])?;
    }
    let end = Instant::now() + Duration::from_secs(2);
    while port.queued_frames() != 0 {
        if stop.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Some(reader) = &mut reference {
            while reader.pop().is_some() {}
        }
        if Instant::now() > end {
            return Err(Error::Execution("preview drain timed out".into()));
        }
        thread::sleep(Duration::from_millis(2));
    }
    let stats = port.telemetry().snapshot();
    port.stop()?;
    Ok(format!(
        "Preview queue drained: {} Hz / {} ch; device underrun frames {}; preview clamps {}; physical presentation tail unknown",
        output.sample_rate_hz(),
        output.channels(),
        stats.underrun_frames,
        clamped
    ))
}
