//! Offline worker runner; artifact I/O and tracing are outside measured DSP calls.
use crate::report::{Artifact, ReplayOrigin};
use crate::{
    Check, Diagnostics, Error, ExecutionPlan, Manifest, Result, RunConfig, Scenario, TraceEvent, io,
};
use audiokit::backend::VoiceProcessor;
use audiokit::diagnostics::{AudioSignalAnalyzer, AudioSignalStats};
use audiokit::graph::capture_pcm::{CapturePcmConfig, CapturePcmGraph};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Cooperative cancellation shared by CLI/GUI and the synchronous worker.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    /// Requests stop; the worker finalizes a partial report rather than detaching.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    /// Returns whether a stop has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}
/// Bounded-rate progress delivered on the runner thread, never a device callback.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProgressEvent {
    /// Input frames consumed, per channel.
    pub input_frames: u64,
    /// Total source frames.
    pub total_frames: u64,
}
#[derive(Default)]
struct Execution {
    calls: u64,
    total_ns: u64,
    max_ns: u64,
}
impl Execution {
    fn record(&mut self, start: Instant) {
        let elapsed = start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.calls += 1;
        self.total_ns = self.total_ns.saturating_add(elapsed);
        self.max_ns = self.max_ns.max(elapsed);
    }
    fn value(&self) -> Value {
        json!({"classification":if self.calls == 0 {"unavailable"} else {"measured"}, "method":"Instant around worker call; excludes tracing and artifact I/O", "calls":self.calls, "total_ns":if self.calls == 0 { None } else { Some(self.total_ns) }, "max_ns": if self.calls == 0 { None } else { Some(self.max_ns) }, "p50_ns":null, "p95_ns":null, "p99_ns":null, "percentiles_reason":"histogram instrumentation not yet implemented"})
    }
}
#[derive(Default, serde::Serialize)]
struct RenderTotals {
    missing_frames: u64,
    max_queue_frames: u64,
    safety_clamped_samples: u64,
    max_gain_reduction_millidb: u32,
    max_abs_clock_correction_ppm: u32,
    source_lookahead_frames: Option<u32>,
    master_lookahead_frames: Option<u32>,
    source_resampler_delay_frames: Option<u64>,
}
#[cfg(feature = "codec-opus")]
impl RenderTotals {
    fn observe(&mut self, metrics: &audiokit::graph::render::RenderMetrics) {
        self.master_lookahead_frames = Some(metrics.master_limiter.lookahead_frames);
        self.safety_clamped_samples += metrics.master_limiter.safety_clamped_samples;
        self.max_gain_reduction_millidb = self
            .max_gain_reduction_millidb
            .max(metrics.master_limiter.max_gain_reduction_millidb);
        for source in &metrics.sources {
            self.missing_frames += source.missing_frames;
            self.max_queue_frames = self.max_queue_frames.max(source.queue_frames);
            self.safety_clamped_samples += source.limiter.safety_clamped_samples;
            self.source_lookahead_frames = Some(source.limiter.lookahead_frames);
            self.source_resampler_delay_frames = Some(source.resampler_delay_frames);
            self.max_abs_clock_correction_ppm = self
                .max_abs_clock_correction_ppm
                .max(source.clock_correction_ppm.unsigned_abs());
            self.max_gain_reduction_millidb = self
                .max_gain_reduction_millidb
                .max(source.limiter.max_gain_reduction_millidb);
        }
    }
}
struct Work {
    run_id: String,
    output: Vec<f32>,
    trace: Vec<TraceEvent>,
    dropped: u64,
    ordinal: u64,
    trace_bytes: usize,
    stats: Value,
    capture_execution: Execution,
    receive_execution: Execution,
    render_totals: RenderTotals,
    #[cfg(feature = "codec-opus")]
    encoded_bytes: u64,
    #[cfg(feature = "codec-opus")]
    encoded_packet_min_bytes: Option<usize>,
    #[cfg(feature = "codec-opus")]
    encoded_packet_max_bytes: Option<usize>,
    #[cfg(feature = "codec-opus")]
    last_source_clocks: Value,
}
impl Work {
    fn new(run_id: String) -> Self {
        Self {
            run_id,
            output: Vec::new(),
            trace: Vec::new(),
            dropped: 0,
            ordinal: 0,
            trace_bytes: 0,
            stats: json!({}),
            capture_execution: Default::default(),
            receive_execution: Default::default(),
            render_totals: Default::default(),
            #[cfg(feature = "codec-opus")]
            encoded_bytes: 0,
            #[cfg(feature = "codec-opus")]
            encoded_packet_min_bytes: None,
            #[cfg(feature = "codec-opus")]
            encoded_packet_max_bytes: None,
            #[cfg(feature = "codec-opus")]
            last_source_clocks: Value::Null,
        }
    }
    fn append(&mut self, pcm: &[f32], config: &RunConfig) -> Result<()> {
        if self
            .output
            .len()
            .checked_add(pcm.len())
            .is_none_or(|n| n > config.max_pcm_samples)
        {
            return Err(Error::Execution("output sample budget exhausted".into()));
        }
        self.output.extend_from_slice(pcm);
        Ok(())
    }
    #[cfg(feature = "codec-opus")]
    fn packet(&mut self, bytes: usize) {
        self.encoded_bytes += bytes as u64;
        self.encoded_packet_min_bytes = Some(
            self.encoded_packet_min_bytes
                .map_or(bytes, |n| n.min(bytes)),
        );
        self.encoded_packet_max_bytes = Some(
            self.encoded_packet_max_bytes
                .map_or(bytes, |n| n.max(bytes)),
        );
    }
    fn event(
        &mut self,
        stage: &str,
        clock: &str,
        time: u64,
        range: (u64, u64),
        metrics: Value,
        config: &RunConfig,
    ) -> Result<()> {
        let event = TraceEvent {
            run_id: self.run_id.clone(),
            ordinal: self.ordinal,
            kind: "stage_snapshot".into(),
            stage: stage.into(),
            clock_domain: clock.into(),
            time_clock_domain: "virtual_host".into(),
            time_ns: time,
            first_frame: range.0,
            frames: range.1,
            source_id: if stage == "render_output" {
                None
            } else {
                Some(1)
            },
            stream_id: if stage == "render_output" {
                None
            } else {
                Some(1)
            },
            epoch: if stage == "render_output" {
                None
            } else {
                Some(1)
            },
            config_generation: 1,
            metrics,
        };
        self.ordinal += 1;
        let bytes = serde_json::to_vec(&event)?.len();
        if self.dropped > 0
            || self.trace.len() >= config.max_trace_events
            || self.trace_bytes.saturating_add(bytes) > config.max_trace_bytes
        {
            self.dropped += 1;
        } else {
            self.trace_bytes += bytes;
            self.trace.push(event);
        }
        Ok(())
    }
}
fn processor(config: &RunConfig, plan: &ExecutionPlan) -> Result<Option<Box<dyn VoiceProcessor>>> {
    if !config.processing.enabled {
        return Ok(None);
    }
    #[cfg(feature = "processing-sonora")]
    {
        use crate::NoiseLevel;
        use audiokit_processing_sonora::{
            AudioProcessingOptions, NoiseSuppressionMode, SonoraProcessor,
        };
        let p = &config.processing;
        let ns = match p.noise_suppression {
            NoiseLevel::Off => NoiseSuppressionMode::Off,
            NoiseLevel::Low => NoiseSuppressionMode::Low,
            NoiseLevel::Moderate => NoiseSuppressionMode::Moderate,
            NoiseLevel::High => NoiseSuppressionMode::High,
            NoiseLevel::VeryHigh => NoiseSuppressionMode::VeryHigh,
        };
        Ok(Some(Box::new(SonoraProcessor::new(
            plan.capture_format,
            plan.capture_format,
            false,
            AudioProcessingOptions {
                high_pass_filter: p.high_pass_filter,
                noise_suppression: ns,
                gain_controller2: p.gain_controller2,
                adaptive_gain: p.adaptive_gain,
            },
        )?)))
    }
    #[cfg(not(feature = "processing-sonora"))]
    {
        let _ = plan;
        Err(Error::Capability("processing-sonora".into()))
    }
}
fn frontend_config(config: &RunConfig, plan: &ExecutionPlan) -> CapturePcmConfig {
    CapturePcmConfig {
        input_format: plan.input_format,
        output_format: plan.capture_format,
        kind: config.stream,
        resampler: config.resampler,
        max_ingress_ms: 200,
    }
}
enum Prepared {
    Pcm(Box<CapturePcmGraph>),
    #[cfg(feature = "codec-opus")]
    Roundtrip(Box<RoundtripGraphs>),
}

#[cfg(feature = "codec-opus")]
struct RoundtripGraphs {
    capture: audiokit::graph::capture::CaptureGraph,
    receive: audiokit::graph::receive::ReceiveGraph,
    source: audiokit::graph::render::SourceRegistration,
}

fn prepare(config: &RunConfig, plan: &ExecutionPlan) -> Result<Prepared> {
    match config.scenario {
        Scenario::FileProcessing => Ok(Prepared::Pcm(Box::new(CapturePcmGraph::new(
            frontend_config(config, plan),
            processor(config, plan)?,
        )?))),
        Scenario::FileRoundtrip => {
            #[cfg(feature = "codec-opus")]
            {
                prepare_roundtrip(config, plan).map(|graphs| Prepared::Roundtrip(Box::new(graphs)))
            }
            #[cfg(not(feature = "codec-opus"))]
            {
                Err(Error::Capability("codec-opus".into()))
            }
        }
    }
}

pub(crate) fn validate_plan(config: &RunConfig, plan: &ExecutionPlan) -> Result<()> {
    // Construct the same owned graph as run, but never process or persist audio.
    drop(prepare(config, plan)?);
    Ok(())
}

fn process_file(
    config: &RunConfig,
    plan: &ExecutionPlan,
    pcm: &[f32],
    stop: &Cancellation,
    progress: &mut impl FnMut(ProgressEvent),
    work: &mut Work,
    mut graph: Box<CapturePcmGraph>,
) -> Result<()> {
    let block_samples = plan.input_format.sample_rate_hz() as usize / 100
        * usize::from(plan.input_format.channels());
    let total_frames = plan.input_format.frames_in(pcm.len())?.get();
    let result = (|| {
        for (i, block) in pcm.chunks(block_samples).enumerate() {
            if stop.is_cancelled() {
                graph.abort();
                return Err(Error::Cancelled);
            }
            let started = Instant::now();
            let out = graph.push_native(block);
            work.capture_execution.record(started);
            let out = out?;
            let first = graph.statistics().output_frames
                - out.len() as u64 / u64::from(plan.capture_format.channels());
            work.append(&out, config)?;
            work.event(
                "capture_pcm",
                "capture_output",
                (i as u64 + 1) * 10_000_000,
                (
                    first,
                    out.len() as u64 / u64::from(plan.capture_format.channels()),
                ),
                json!(graph.statistics()),
                config,
            )?;
            if i % 10 == 0 {
                progress(ProgressEvent {
                    input_frames: graph.statistics().input_frames,
                    total_frames,
                });
            }
        }
        let started = Instant::now();
        let tail = graph.finish();
        work.capture_execution.record(started);
        work.append(&tail?, config)
    })();
    work.stats = json!({"capture": graph.statistics(), "receive": null});
    result
}

#[cfg(feature = "codec-opus")]
fn prepare_roundtrip(config: &RunConfig, plan: &ExecutionPlan) -> Result<RoundtripGraphs> {
    use audiokit::graph::{
        capture::{CaptureGraph, CaptureGraphConfig},
        receive::ReceiveGraph,
        render::SourceRegistration,
    };
    use audiokit::{SourceId, SourceKey, StreamEpoch, StreamId, StreamKind};
    use audiokit_codec_opus::{OpusConfig, OpusDecoder, OpusEncoder, OpusProfile};
    let profile = if config.stream == StreamKind::Voice {
        OpusProfile::Voice
    } else {
        OpusProfile::Desktop
    };
    let mut encoder = OpusEncoder::new(
        plan.capture_format,
        config.ptime,
        profile,
        OpusConfig::default(),
        config.max_payload_bytes,
    )?;
    encoder.set_target_bitrate_bps(config.bitrate_bps)?;
    let capture = CaptureGraph::new(
        CaptureGraphConfig {
            input_format: plan.input_format,
            kind: config.stream,
            resampler: config.resampler,
            max_ingress_ms: 200,
            max_payload_bytes: config.max_payload_bytes,
        },
        Box::new(encoder),
        processor(config, plan)?,
    )?;
    let mut receive = ReceiveGraph::new(config.receive)?;
    let source = SourceRegistration {
        key: SourceKey {
            source: SourceId::new(1)?,
            stream: StreamId::new(1)?,
        },
        epoch: StreamEpoch(1),
        format: plan.capture_format,
        kind: config.stream,
    };
    receive.register(
        source,
        Box::new(OpusDecoder::new(plan.capture_format, config.ptime)?),
    )?;
    Ok(RoundtripGraphs {
        capture,
        receive,
        source,
    })
}

#[cfg(feature = "codec-opus")]
fn schedule_packet(
    packet: audiokit::graph::capture::CapturePacket,
    sequence: u16,
    now: u64,
    transport: &mut crate::transport::scheduler::Transport,
    config: &RunConfig,
    plan: &ExecutionPlan,
    work: &mut Work,
) -> Result<()> {
    work.packet(packet.payload.len());
    let frames = u64::from(plan.capture_format.sample_rate_hz())
        * u64::from(config.ptime.milliseconds())
        / 1000;
    work.event(
        "encoded_out",
        "capture_output",
        now,
        (packet.sample_position, frames),
        json!({"sequence":sequence,"bytes":packet.payload.len()}),
        config,
    )?;
    let decision = transport.schedule(packet.payload, sequence, packet.sample_position, now)?;
    work.event(
        "transport_schedule",
        "capture_output",
        now,
        (packet.sample_position, frames),
        json!(decision),
        config,
    )
}

#[cfg(feature = "codec-opus")]
fn deliver_packets(
    transport: &mut crate::transport::scheduler::Transport,
    receive: &mut audiokit::graph::receive::ReceiveGraph,
    source: audiokit::graph::render::SourceRegistration,
    now: u64,
    work: &mut Work,
    config: &RunConfig,
) -> Result<()> {
    use audiokit::graph::receive::EncodedPacket;
    while let Some(delivery) = transport.pop_due(now) {
        let outcome = receive.push_packet(EncodedPacket {
            source: source.key,
            epoch: source.epoch,
            sequence: delivery.sequence,
            duration: config.ptime,
            arrival_ns: delivery.due_ns,
            payload: delivery.payload,
        })?;
        let frames = u64::from(source.format.sample_rate_hz())
            * u64::from(config.ptime.milliseconds())
            / 1000;
        work.event(
            "transport_arrival",
            "capture_output",
            delivery.due_ns,
            (delivery.first_frame, frames),
            json!({"packet_ordinal":delivery.ordinal,"sequence":delivery.sequence,
                "duplicate_copy":delivery.duplicate,"emitted_ns":delivery.emitted_ns,
                "delivery_delay_ns":delivery.due_ns-delivery.emitted_ns,"outcome":outcome,
                "observed_by_callback_ns":now}),
            config,
        )?;
    }
    Ok(())
}

#[cfg(feature = "codec-opus")]
fn roundtrip(
    config: &RunConfig,
    plan: &ExecutionPlan,
    pcm: &[f32],
    stop: &Cancellation,
    progress: &mut impl FnMut(ProgressEvent),
    work: &mut Work,
    graphs: Box<RoundtripGraphs>,
) -> Result<()> {
    let RoundtripGraphs {
        mut capture,
        mut receive,
        source,
    } = *graphs;
    let block_samples = plan.input_format.sample_rate_hz() as usize / 100
        * usize::from(plan.input_format.channels());
    let mut output = vec![
        0.0;
        plan.output_format.sample_rate_hz() as usize / 100
            * usize::from(plan.output_format.channels())
    ];
    let total_frames = plan.input_format.frames_in(pcm.len())?.get();
    let mut now = 0_u64;
    let mut sequence = 0_u16;
    let mut transport = crate::transport::scheduler::Transport::new(config.transport);
    let result = (|| {
        for (i, block) in pcm.chunks(block_samples).enumerate() {
            if stop.is_cancelled() {
                capture.abort();
                receive.abort();
                return Err(Error::Cancelled);
            }
            now += 10_000_000;
            let started = Instant::now();
            let packets = capture.push_native(block);
            work.capture_execution.record(started);
            for packet in packets? {
                schedule_packet(packet, sequence, now, &mut transport, config, plan, work)?;
                sequence = sequence.wrapping_add(1);
            }
            deliver_packets(&mut transport, &mut receive, source, now, work, config)?;
            let started = Instant::now();
            let metrics = receive.render_into(&mut output, now);
            work.receive_execution.record(started);
            let metrics = metrics?;
            work.render_totals.observe(&metrics);
            work.last_source_clocks = json!(receive.clock_metrics());
            work.append(&output, config)?;
            work.event(
                "render_output",
                "virtual_output",
                now,
                (metrics.sample_position, metrics.frames),
                json!(metrics),
                config,
            )?;
            if i % 10 == 0 {
                progress(ProgressEvent {
                    input_frames: capture.statistics().input_frames,
                    total_frames,
                });
            }
        }
        let started = Instant::now();
        let packets = capture.finish();
        work.capture_execution.record(started);
        for packet in packets? {
            schedule_packet(packet, sequence, now, &mut transport, config, plan, work)?;
            sequence = sequence.wrapping_add(1);
        }
        deliver_packets(&mut transport, &mut receive, source, now, work, config)?;
        // Capture EOF is not network EOF: keep real demand while delayed packets
        // are in flight. Only after the final scheduled copy is delivered may the
        // receiver drain, avoiding artificial PLC beyond the transport's lifetime.
        let mut waiting = 0;
        while !transport.is_empty() {
            if stop.is_cancelled() {
                receive.abort();
                return Err(Error::Cancelled);
            }
            waiting += 1;
            if waiting > 1000 {
                return Err(Error::Execution(
                    "virtual transport drain deadline exceeded".into(),
                ));
            }
            now += 10_000_000;
            deliver_packets(&mut transport, &mut receive, source, now, work, config)?;
            if transport.is_empty() {
                break;
            }
            let started = Instant::now();
            let metrics = receive.render_into(&mut output, now);
            work.receive_execution.record(started);
            let metrics = metrics?;
            work.render_totals.observe(&metrics);
            work.last_source_clocks = json!(receive.clock_metrics());
            work.append(&output, config)?;
            work.event(
                "render_output",
                "virtual_output",
                now,
                (metrics.sample_position, metrics.frames),
                json!(metrics),
                config,
            )?;
        }
        for _ in 0..1000 {
            if stop.is_cancelled() {
                receive.abort();
                return Err(Error::Cancelled);
            }
            now += 10_000_000;
            let started = Instant::now();
            let frames = receive.drain_into(&mut output, now);
            work.receive_execution.record(started);
            let frames = frames?;
            if frames == 0 {
                return Ok(());
            }
            work.append(
                &output[..frames as usize * usize::from(plan.output_format.channels())],
                config,
            )?;
        }
        Err(Error::Execution(
            "receive drain exceeded bounded deadline".into(),
        ))
    })();
    let encoded_frames = capture.statistics().encoded_frames;
    work.stats = json!({"capture":capture.statistics(), "receive":receive.statistics(), "transport":transport.statistics(), "last_steady_source_clocks":work.last_source_clocks, "steady_render":work.render_totals, "render_totals_scope":"ordinary demand only; drain metric API unavailable",
        "codec_payload":{"bytes":work.encoded_bytes,"packet_min_bytes":work.encoded_packet_min_bytes,"packet_max_bytes":work.encoded_packet_max_bytes,
        "observed_bitrate_bps":if encoded_frames == 0 {None} else {Some(work.encoded_bytes as f64 * 8.0 * f64::from(plan.capture_format.sample_rate_hz()) / encoded_frames as f64)},
        "method":"payload only over encoded media duration including EOF padding; VBR may differ from target; no transport headers"}});
    result
}

/// Runs a validated file scenario in a new output directory and finalizes its bundle.
///
/// `retain_input` is explicit input-audio sharing consent. Output WAV is the
/// requested test recording. Existing directories are never overwritten. On
/// cancellation/processing failure a partial manifest is written before error return;
/// disk failures can prevent finalization and are returned without hiding the error.
pub fn run(
    config: &RunConfig,
    input: &Path,
    output_dir: &Path,
    stop: &Cancellation,
    mut progress: impl FnMut(ProgressEvent),
) -> Result<Diagnostics> {
    config.validate()?;
    let raw = io::bytes(input, config.max_input_bytes)?;
    run_bytes(config, raw, output_dir, stop, &mut progress, None)
}
pub(crate) fn run_bytes(
    config: &RunConfig,
    raw: Vec<u8>,
    output_dir: &Path,
    stop: &Cancellation,
    mut progress: impl FnMut(ProgressEvent),
    replay_origin: Option<ReplayOrigin>,
) -> Result<Diagnostics> {
    config.validate()?;
    let (format, pcm) = io::wav(&raw, config.max_pcm_samples)?;
    let plan = config.plan(format)?;
    let prepared = prepare(config, &plan)?;
    std::fs::create_dir(output_dir)?;
    io::write_json(&output_dir.join("config.json"), config)?;
    if config.retain_input {
        io::write_new(&output_dir.join("input.wav"), &raw)?;
    }
    static ID: AtomicU64 = AtomicU64::new(0);
    let run_id = format!(
        "{}-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        ID.fetch_add(1, Ordering::Relaxed)
    );
    let mut work = Work::new(run_id.clone());
    let result = match prepared {
        Prepared::Pcm(graph) => {
            process_file(config, &plan, &pcm, stop, &mut progress, &mut work, graph)
        }
        #[cfg(feature = "codec-opus")]
        Prepared::Roundtrip(graphs) => {
            roundtrip(config, &plan, &pcm, stop, &mut progress, &mut work, graphs)
        }
    };
    let mut analyzer = AudioSignalAnalyzer::new(Default::default());
    let mut signal = AudioSignalStats::default();
    for block in work.output.chunks(
        plan.output_format.sample_rate_hz() as usize / 100
            * usize::from(plan.output_format.channels()),
    ) {
        signal.record_frame(
            analyzer
                .observe_f32_frame(
                    block,
                    plan.output_format.sample_rate_hz(),
                    plan.output_format.channels(),
                )
                .map_err(|e| Error::Invalid(e.to_string()))?,
        );
    }
    let mut checks = vec![
        Check {
            id: "completed".into(),
            passed: result.is_ok(),
            detail: "file scenario completed, not hardware/server E2E".into(),
        },
        Check {
            id: "finite_nonempty_output".into(),
            passed: !work.output.is_empty() && work.output.iter().all(|v| v.is_finite()),
            detail: "output PCM is finite and nonempty".into(),
        },
    ];
    if config.scenario == Scenario::FileRoundtrip {
        checks.push(Check { id:"steady_render_no_missing_frames".into(), passed:work.render_totals.missing_frames == 0, detail:"ordinary render demand has no already-started source gaps; excludes startup and drain".into() });
        let receive = &work.stats["receive"];
        checks.push(Check {
            id: "received_media".into(),
            passed: receive["decoded_packets"].as_u64().unwrap_or(0) > 0,
            detail: "at least one actual payload decoded; all-loss silence is not healthy media"
                .into(),
        });
        checks.push(Check {
            id: "decoder_no_errors".into(),
            passed: receive["decode_errors"] == 0,
            detail: "production decoder did not report corrupt/backend failures".into(),
        });
        let transport = &work.stats["transport"];
        checks.push(Check {
            id: "transport_accounting".into(),
            passed: transport["pending_copies"] == 0
                && transport["original_packets"] == work.stats["capture"]["packets"]
                && transport["original_packets"].as_u64().zip(transport["duplicated_packets"].as_u64())
                    .map(|(original, duplicate)| original + duplicate)
                    == transport["intentionally_dropped"].as_u64().zip(transport["delivered_copies"].as_u64())
                        .map(|(dropped, delivered)| dropped + delivered),
            detail: "all produced originals/copies either intentionally dropped or delivered; no pending tail".into(),
        });
        if !config.transport.is_impaired() {
            checks.push(Check {
                id: "lossless_virtual_transport".into(),
                passed: receive["decode_errors"] == 0
                    && receive["concealed_packets"] == 0
                    && receive["fec_attempts"] == 0
                    && receive["late"] == 0
                    && receive["resynchronizations"] == 0
                    && receive["decoded_packets"] == work.stats["capture"]["packets"],
                detail: "all encoded packets decoded once; no injected loss, late/reset/FEC/PLC"
                    .into(),
            });
        }
        let ceiling = 10_f64
            .powf(f64::from(config.receive.render.master_limiter.ceiling_dbfs) / 20.0)
            * 32768.0;
        checks.push(Check {
            id: "master_sample_ceiling".into(),
            passed: f64::from(signal.max_peak_q15) <= ceiling + 1.0,
            detail: "sample peak only; independent true-peak oracle is not part of this runner yet"
                .into(),
        });
    }
    let capture = &work.stats["capture"];
    let diagnostics = Diagnostics {
        replay_origin,
        schema_version: 1,
        run_id: run_id.clone(),
        status: if result.is_ok() {
            "completed"
        } else if matches!(result, Err(Error::Cancelled)) {
            "cancelled"
        } else {
            "failed"
        }
        .into(),
        requested_config: config.clone(),
        effective_config: config.clone(),
        plan: plan.clone(),
        input_sha256: io::hash(&raw),
        input_frames: format.frames_in(pcm.len())?.get(),
        output_frames: plan.output_format.frames_in(work.output.len())?.get(),
        output_signal: json!(signal),
        graph_statistics: work.stats.clone(),
        latency: json!({"capture_execution":work.capture_execution.value(), "receive_execution":work.receive_execution.value(),
            "capture_resampler":{"classification":"estimated", "method":"production backend group delay", "clock":"capture_output", "frames":capture["resampler_delay_frames"]},
            "processor":{"classification":if config.processing.enabled {"unknown"} else {"bypassed"}, "frames":capture["processing_delay_frames"], "reason":if config.processing.enabled {"Sonora does not expose algorithmic delay"} else {"bypassed"}},
            "encoder":{"classification":if config.scenario == Scenario::FileRoundtrip {"estimated"} else {"not_covered"}, "clock":"capture_output", "frames":capture["encoder_lookahead_frames"]},
            "packetization":{"classification":if config.scenario == Scenario::FileRoundtrip {"configured"} else {"not_covered"}, "ptime_ms":if config.scenario == Scenario::FileRoundtrip {Some(config.ptime.milliseconds())} else {None}, "reason":"not a constant per-sample end-to-end delay"},
            "encoded_startup":{"classification":if config.scenario == Scenario::FileRoundtrip {"configured"} else {"not_covered"}, "target_ms":if config.scenario == Scenario::FileRoundtrip {Some(config.receive.jitter.target_ms)} else {None}},
            "render_algorithms":{"classification":if config.scenario == Scenario::FileRoundtrip {"estimated"} else {"not_covered"}, "method":"production per-stage frame delays; no double-counted total", "clock":"virtual_output", "source_limiter_frames":work.render_totals.source_lookahead_frames, "master_limiter_frames":work.render_totals.master_lookahead_frames, "source_resampler_frames":work.render_totals.source_resampler_delay_frames},
            "virtual_forwarding":{"classification":if config.scenario == Scenario::FileRoundtrip {"simulated"} else {"not_covered"}, "clock":"virtual_host", "max_delivery_delay_ns":work.stats["transport"]["max_delivery_delay_ns"], "method":"scheduled delivery time minus emission; not measured server/network latency; callback observation may be up to 10 ms later"},
            "device":{"classification":"unknown", "reason":"no devices opened"}, "server_forwarding":{"classification":"unknown", "reason":"no server in this scenario"},
            "end_to_end":{"classification":"unknown", "reason":"signal alignment and full render delay instrumentation pending; components are not summed"}}),
        checks,
        trace_events_dropped: work.dropped,
        trace_events_attempted: work.ordinal,
        trace_first_dropped_ordinal: if work.dropped > 0 {
            Some(work.trace.len() as u64)
        } else {
            None
        },
        unavailable_observations: vec![
            "internal capture pre/post-resample and pre/post-APM taps".into(),
            "sample-accurate internal packet timeline".into(),
            "drain stage snapshots".into(),
            "independent true-peak oracle".into(),
        ],
        error: result.as_ref().err().map(ToString::to_string),
    };
    io::write_wav(
        &output_dir.join("processed.wav"),
        plan.output_format,
        &work.output,
    )?;
    io::write_json(&output_dir.join("diagnostics.json"), &diagnostics)?;
    io::write_json(&output_dir.join("trace.json"), &work.trace)?;
    let mut artifacts = Vec::new();
    for name in [
        "config.json",
        "diagnostics.json",
        "trace.json",
        "processed.wav",
        "input.wav",
    ] {
        if name == "input.wav" && !config.retain_input {
            continue;
        }
        let data = io::bytes(&output_dir.join(name), 512 * 1024 * 1024)?;
        artifacts.push(Artifact {
            path: name.into(),
            bytes: data.len() as u64,
            sha256: io::hash(&data),
        });
    }
    let backends = compiled_backends();
    io::write_json(
        &output_dir.join("manifest.json"),
        &Manifest {
            schema_version: 1,
            run_id,
            revision: env!("AUDIOKIT_REVISION").into(),
            source_digest: env!("AUDIOKIT_SOURCE_DIGEST").into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .into(),
            backends,
            complete: result.is_ok(),
            reproduction: if config.retain_input {
                "signal-replay"
            } else {
                "metadata-only"
            }
            .into(),
            input_audio_authorized: config.retain_input,
            artifacts,
        },
    )?;
    result?;
    progress(ProgressEvent {
        input_frames: diagnostics.input_frames,
        total_frames: diagnostics.input_frames,
    });
    Ok(diagnostics)
}

pub(crate) fn compiled_backends() -> Vec<String> {
    let mut backends = vec!["audiokit-production-graphs".into()];
    if cfg!(feature = "codec-opus") {
        backends.push("codec-opus".into());
    }
    if cfg!(feature = "processing-sonora") {
        backends.push("processing-sonora".into());
    }
    backends
}
