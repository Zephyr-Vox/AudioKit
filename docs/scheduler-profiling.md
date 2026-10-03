# Scheduler Faults and Worker Profiling

These are offline, deterministic **host-policy simulations**, not native device,
thread contention, server tests or a certification of realtime performance.
Only `file-roundtrip` accepts `scheduler.enabled`. It reuses production Opus,
ReceiveGraph/RenderGraph, explicit clock recovery and the production SPSC ring.
The ring holds float samples plus virtual write timestamps; native output-port
conversion, reference delivery and actual OS callback scheduling are not covered.

## Three Independent Fault Boundaries

- `transport.stall_start_ms/stall_duration_ms`: withholds packet arrivals. Worker
  and consumer continue; the production jitter/FEC/PLC policy handles shortages.
- `scheduler.worker_pause`: arrivals continue, DSP calls stop, consumer continues.
  Empty output is zero-filled and counted. On resume, there is no catch-up burst.
  `recover_on_worker_resume` defaults true and invokes `ReceiveGraph::recover_clock`
  **before** that tick's arrivals, clearing queued packets, PCM, DSP and estimators.
  Registered epoch remains unchanged. False retains histories for a comparative
  failure model; neither setting is a claim about the existing client host policy.
- `scheduler.output_pause`: consumer stops; DSP and arrivals continue. A full ring
  rejects entire new quanta, not partial stereo frames. `discard_on_output_resume`
  defaults true, clearing stale output on resume without resetting decoder/DSP.
  False retains backlog, which increases simulated output residence time.

Pause intervals are half-open virtual host milliseconds. Zero duration disables
an interval; start <=600000 ms, duration <=1000 ms. Independent gates share the
configured rational 10 ms render-rate tick; this does not simulate separately
varying OS wakeup jitter. Capture retains its independent configured clock.
The receiver's rate estimator observes actual DSP demands, **not** output-ring
consumption; output-pause evidence is separate from inferred hardware drift.
Intervals outside the ordinary input/transport lifetime are not necessarily
exercised. An interval spanning EOF is allowed to resume before synthetic drain.

Output capacity is power-of-two 1024..65536 **interleaved samples**, default 4096,
and must hold a complete 10 ms quantum. No prebuffer is added: within a tick the
order is recovery, packet arrivals, worker, consumer. This makes fault-free runs
waveform-equivalent to direct output. Recovery may intentionally lose packets;
do not interpret missing/PLC counts as network loss when worker reset was injected.

`processed.wav` records ordinary consumer demand, including underrun zero fill.
No PCM is inserted for a paused consumer: absent demand is represented in trace
host timestamps. Synthetic EOF flush/drain is separately accounted, not a captured
device tail or ordinary recovery observation.

## Evidence and Checks

`graph_statistics.scheduler.statistics` retains pause/resume counts, produced,
enqueued, consumed, rejected/discarded/flushed frames, high-water and maximum
consumed queued age, independent of trace truncation. `pending_frames` accounts
for unfinished queues on cancellation. Two conservation identities are checked:

```text
produced = enqueued + overflow
enqueued = consumed + recovery_discarded + synthetic_eof_flushed + pending
```

`output_queue` trace records include per-tick fault/drop/underrun deltas. Its sample
range belongs to `virtual_consumer`, distinct from DSP `virtual_output`; its time
still belongs to `virtual_host`. A paused consumer has zero range length.
`analyze` flags injected gates, recovery, queue overflow/discard and underrun.
The bounded evidence timeline is not a first-audible-defect detector.
`first_healthy_consumption_after_worker_resume_ns` means a fully supplied **queue**
read; those samples can be startup silence. It is not measured media/audible recovery.
`max_queued_age_ns` observes consumed samples only; discarded/rejected frames have
no implied successful presentation time.

Quality checks `output_consumer_no_underrun` and `output_queue_no_drop` intentionally
fail when faults produce deficits. CLI exit 1 still writes a valid completed
bundle; `completed` means the simulation ran, not that its audio checks passed.

## Performance Instrumentation

`execution_profiling` defaults false. It enables non-overlapping production
timers for source processing, bus mixing, master limiting and pre/post signal
analysis. Receive demand also measures decoder and PCM admission/resampling.
Packet admission is separately measured per packet; mix PCM admission sums all
sources per ordinary call. Admission and decoder are **outside** render's
`execution_ns`, but inside the receive worker call where appropriate. No sum is
reported as end-to-end latency; setup, controller/service logic, allocations and
metric bookkeeping are not completely attributed by substage timers.
Clock reads and instrumentation overhead are included. Profiling cannot change
PCM parameters, histories or default source admission (32).

`latency.execution_profile` summarizes ordinary stages (final packet admission is
included; drain DSP is not). `capture_execution`/`receive_execution` each separate
`ordinary` budgeted calls from `unbudgeted` calls, including finish/drain. Capture
currently has no ordinary budget and is entirely in the latter set. CPU duration
never includes injected virtual time, real sleeps, tracing or file I/O.
All histograms use 512 fixed bins, eight subdivisions per power of two. p50/p95/p99
are nearest-rank **bucket upper bounds**, at most 12.5% positive-sample slack;
zero shares [0,1] ns. Totals/max are actual observations, unavailable sets are null.
No timing sample vectors are retained. Full aggregates survive trace truncation,
while exact per-block overrun locations exist only in retained trace.

Run these commands from the AudioKit checkout using a >=3-second WAV:

```powershell
cargo build -p audiokit-test --release --offline --locked
.\target\release\audiokit-test.exe run --config configs\worker-stall.json --input "INPUT.wav" --out-dir target\worker-stall --quiet
.\target\release\audiokit-test.exe analyze --bundle target\worker-stall --json
.\target\release\audiokit-test.exe run --config configs\output-stall.json --input "INPUT.wav" --out-dir target\output-stall --quiet
.\target\release\audiokit-test.exe sweep --config configs\mix-profile.json --matrix configs\mix-sweep.json --input "INPUT-3s.wav" --out-dir target\mix-profile-sweep --quiet
```

Use fresh output directories. Input retention remains explicit (`--retain-input`)
and replay requires the same input hash if not retained. Do not increase buffers
or adjust accepted DSP algorithms just to hide CPU overrun evidence. Measure
Release on the target hardware before deciding which substage needs optimization.
