# Diagnostic Bundle Schema 1

This schema belongs to AudioKit testkit. It does not reinterpret older CLI mic
reports or `audio.diagnostics` snapshots. Every import checks version 1 and rejects
unknown versions. Rust serde models and shared validators are authoritative;
unknown config/report fields are rejected rather than silently ignored.

## Files and Identity

`manifest.json` references exactly `config.json`, `diagnostics.json`, `trace.json`
and `processed.wav`, plus optional explicitly authorized `input.wav`. Artifact
paths are fixed relative filenames; sizes and SHA256 are validated before parsing.
Missing, duplicate, unexpected, escaping or tampered artifacts fail import. Reads
have absolute byte/sample limits even when the manifest is untrusted.

Manifest contains run ID, revision, Rust/TOML/lockfile source digest (including
uncommitted files), OS/arch/build profile and compiled backends. It records run
completion separately from checks and recording authorization. Hash integrity is
not producer authentication. `signal-replay` means source WAV was included;
`metadata-only` requires an externally supplied original hash-matching WAV to replay.
Hardware and packet replay are not implemented by schema 1's file scenarios.

`config.json` contains validated effective run controls and resource budgets, no
local input filename. `diagnostics.json` retains requested/effective controls,
plan, input hash, per-channel input/output frames, signal measurements, graph
accounting, latency, checks, unavailable observations and optional error. Replay
records `replay_origin` with original identity/status/source/build change; it does
not reproduce a previous cancellation or physical callback schedule.

Status is `completed`, `cancelled` or `failed`. Completion does not imply every
check passed. A partial package can still have valid file hashes and a finalized
partial WAV. Disk failures may prevent a complete package; the operation fails
explicitly. PCM is float32 and retains pipeline leading latency/tail, not an
automatically level-normalized or trimmed listening version.

## Coverage and Trace

ExecutionPlan declares actual input/capture/output formats and a stable node
inventory; list order is not chronological order across independent clocks.
Status `applied`, `bypassed`, `not_covered` describes node coverage, not
quality. The importer recomputes the expected file plan from config and input
format without requiring optional backends just to inspect a package. Current
coverage levels are `capture-subchain`, `virtual-roundtrip` and `render-stress`,
never server E2E. Mix stress has receive/codec accounting null and source correction
bypassed; its shared capture frontend only performs declared channel/rate adaptation.

Trace is a JSON array of bounded snapshots. Every event contains run ID, ordinal,
kind/stage, first_frame/frames, anonymous source/stream/epoch when applicable,
config generation, metrics, sample clock domain, time_ns and its distinct
time_clock_domain. A mixed render boundary has no single source identity; its
production metrics contain per-source records. All initial scenarios use virtual
host nanoseconds for scheduling, not DSP CPU time or synchronized remote time.
Encoded-out samples use the capture output clock; final PCM uses virtual output.

Events are a retained prefix. Attempted = retained + dropped; ordinals are
consecutive from zero; first_dropped_ordinal is null with no loss, otherwise the
retained length. Event cap and serialized-payload byte cap apply independently.
Pretty formatting has bounded additional framing. Loss never implies a healthy
unobserved interval. Counters use checked arithmetic on import. Internal PCM taps,
exact packet dependency tracing and drain snapshots remain unavailable.

## Metrics and Latency

Per-channel frames are never interleaved sample counts. Production signal values
currently use Q15 units: peak/true-peak amplitude divided by 32768 is normalized
amplitude. Full-scale and delta-candidate counts are not proof of audible clipping
or clicks. The estimated true-peak analyzer is not the independent oracle.
EOF filter, processor-quantum and packet padding have different fields/domains.

Receive counters cover accepted/decoded packets, late/duplicate/reset/FEC/PLC and
errors. Codec payload totals include EOF packets and observed bit/s over encoded
media duration; this excludes transport headers and may differ from a VBR target.
Ordinary-demand render totals retain missing frames, queue maximum,
limiter clamps/attenuation and bounded correction even if trace truncates. Drain
metrics are not in that aggregate. Last ordinary source-clock snapshot is retained
before drain retires the source; it is inferred from virtual arrivals, not a
measured remote device clock.

Optional/defaulted `transport` controls add deterministic faults to file-roundtrip
without changing its coverage level. `graph_statistics.transport` counts original
packets, injected drops/duplicate copies, delivered/pending copies, queue high-water,
reorder selections, stalled packets and maximum simulated delivery delay. A
successful scheduler run conserves originals + duplicates = drops + deliveries,
with no pending copies; failed/cancelled runs may retain pending evidence.
Trace schedule/arrival records correlate packet ordinal, wrapping sequence and
capture-output sample range. Injection selection is not a receiver outcome.
Delivery timestamps use virtual host time, not measured server/network time.
JSON artifacts have a hard 32 MiB serialized bound on both write and import.

Latency records identify measured execution, estimated backend algorithmic frames,
configured ptime/startup or unknown/not-covered/bypassed stages. Empty timing sets
have null total/max, not a fabricated zero. p50/p95/p99 remain null until bounded
histograms land. CPU durations exclude trace serialization, artifact I/O and
post-run analysis. Device/server and aligned E2E delay are unknown. No sum is
reported that double-counts parallel work, buffering or codec/group delays.
`virtual_forwarding` uses classification `simulated`; its maximum is due time
minus emission time, while callback observation is quantized to 10 ms. It does
not replace unknown actual server/network measurements.
Independent clock controls are optional/default-zero. Injected rates are stored
separately from receiver clock inference; host arrival time remains a simulated
source estimate, not measured hardware time. Render/stress accounting adds active
source maxima and peak before master protection. Execution budget fields count
ordinary measured calls and overruns/max overrun; unavailable budgets stay null.
Mix timing includes source PCM admission/render, roundtrip timing includes
receive/render; drain, frontend work, tracing and scheduling are excluded from
those budgeted calls. These fields do not claim a real callback deadline was met.
Ordinary `render_output` snapshots also retain `worker_call_ns`,
`worker_budget_ns` and `worker_over_budget`, so an observed overrun can be located
in the sample/time timeline even when waveform checks pass.

`analyze` reports integrity-verified recorded checks and evidence limitations, not
a root-cause certainty score. `compare` compares input/config/coverage/waveform/
checks/build and preserves both original packages. Timing fields and run IDs are
not compared as deterministic audio content.

Analysis also returns a bounded `evidence` timeline (64 flagged retained intervals)
and `evidence_omitted`. Stage, ordinal, sample range/time and their separate domains
refer to actual recorded observations. Flags include injected drops/stall/reorder
selections, non-accepted arrivals, source missing frames and master safety clamps.
The `worker_budget_overrun` flag identifies measured worker calls exceeding their
simulated demand budget, not proof of an audible interruption.
Observation order is not asserted causal order or first audible-defect location.
Recorder loss remains a separate count. Sweep summaries are not run manifests:
each case retains its own normal schema-1 diagnostic bundle and replay contract.
