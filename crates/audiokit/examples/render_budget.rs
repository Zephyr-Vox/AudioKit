//! Release-mode virtual render worker timing; not a device/hardware deadline certification.
use audiokit::graph::render::{RenderGraph, RenderGraphConfig, SourceRegistration};
use audiokit::{
    AudioFormat, ChannelLayout, SourceId, SourceKey, StreamEpoch, StreamId, StreamKind,
};
use std::{hint::black_box, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut reports = Vec::new();
    for rate in [44_100_u32, 48_000, 96_000] {
        for count in [1_usize, 8, 32, 64] {
            let mut graph = RenderGraph::new(RenderGraphConfig {
                max_sources: count,
                format: AudioFormat::new(rate, ChannelLayout::Stereo)?,
                ..Default::default()
            })?;
            let mono = AudioFormat::new(48_000, ChannelLayout::Mono)?;
            let sources = (1..=count)
                .map(|id| {
                    Ok(SourceRegistration {
                        key: SourceKey {
                            source: SourceId::new(id as u64)?,
                            stream: StreamId::new(1)?,
                        },
                        epoch: StreamEpoch(1),
                        format: mono,
                        kind: StreamKind::Voice,
                    })
                })
                .collect::<audiokit::AudioResult<Vec<_>>>()?;
            for source in &sources {
                graph.register(*source)?;
            }
            let pcm = (0..960)
                .map(|frame| (frame as f32 * 0.1).sin() * 0.3)
                .collect::<Vec<_>>();
            let mut output = vec![0.0; (rate / 50) as usize * 2];
            let mut timings = Vec::with_capacity(100);
            for iteration in 0..116 {
                let start = Instant::now();
                for source in &sources {
                    graph.push_pcm(source.key, source.epoch, &pcm)?;
                }
                black_box(graph.render_into(&mut output)?);
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                if iteration >= 16 {
                    timings.push(elapsed);
                }
            }
            timings.sort_by(f64::total_cmp);
            reports.push(serde_json::json!({"sources": count, "sample_rate_hz": rate,
            "demand_ms": 20, "measured_blocks": timings.len(), "p50_ms": timings[49],
            "p95_ms": timings[94], "p99_ms": timings[98], "max_ms": timings[99],
            "p99_deadline_fraction": timings[98] / 20.0,
            "includes": "source submit/resample, source/master limiter, activity, pre/post diagnostics",
            "excludes": "codec, device callback, transport, artifact IO"}));
        }
    }
    println!("{}", serde_json::to_string_pretty(&reports)?);
    Ok(())
}
