# AudioKit Debugging Suite

The first A5 slice is implemented: a reusable `audiokit-testkit` library and the
headless `audiokit-test` application. CLI is for automation, reproduction and
evidence analysis. The optional Slint offline workbench now uses the same runner
for parameter editing, listening and export; see `gui-workbench.md`.
Deterministic virtual packet faults and serial
parameter sweeps, correlated mix stress and independent virtual sample clocks are
implemented. Single-source external Opus packet/schedule replay is available;
see `packet-replay.md` for its separate input/material contract. Explicit microphone
material acquisition is available through optional `native-cpal`, then closes the
device before invoking offline DSP; see `microphone-material.md`. Live microphone
DSP/duplex, live host packet export and host/server E2E are **not implemented yet**.
Native WAV audition is explicit and compiled, not hardware-accepted.
There is no arbitrary node-connection editor or selectable endpoint range yet.

## Production Coverage

`file-processing` calls `CapturePcmGraph`: native WAV -> production channel map ->
continuous production resampler -> optional Sonora -> float32 WAV. This is the
same frontend used by `CaptureGraph`, not a test-specific copy or a dummy codec.
Voice is mono; desktop stereo must bypass Sonora. The PCM-only path has no Opus
packet padding. Filter latency, filter EOF zeros and final APM-quantum padding
remain explicit. AEC is rejected because these file scenarios have no actual
speaker reference.

`file-roundtrip` calls `CaptureGraph` with real Opus, then feeds its packets to
`ReceiveGraph`/`RenderGraph` on a 10 ms virtual callback clock. Arrivals are paced
by input consumption, not dumped into the receiver in a burst. EOF uses explicit
drain; extra ordinary callbacks after EOF would manufacture PLC. Receive parameters
include the production source/master limiter, activity mix, gain, resampling,
jitter and clock controls. Mono voice expands to the configured render channels.
This is a **virtual roundtrip**, not a microphone, soundcard or server test.

`receive-simulation` starts directly at recorded Opus arrivals and recorded
fixed-10-ms render demands; it does not re-encode source WAV. It supports one
anonymous source/epoch with fixed negotiated ptime. Payloads, cold-start state
and omission declarations determine packet versus partial reproduction. Actual
arrival/demand times are preserved; synthetic EOF drain is explicit. WAV input,
external packet sweep and direct old CLI diagnostics import are unavailable for
this scenario. SDK/live export integration remains a host responsibility.

## Independent Virtual Sample Clocks

Roundtrip `clocks.capture_rate_ppm` and `clocks.render_rate_ppm` independently
control sample quantum timing in the same virtual host clock, each -2000..=2000.
Positive ppm means more samples per host second, not a different declared codec
format. Event times use integer rational calculations from the clock origin;
rounding does not accumulate once per tick. Default zero/zero preserves the old
10 ms schedule and its exact waveform.

Actual encoded payloads retain fixed ptime/sample counts. Render demand follows
its own clock, with due packets admitted before each demand; packets emitted in
the future are never made visible to an earlier render tick. Trace observation
ordinals are not asserted to be sorted host timestamps across these clocks.
The observing tick interval is about 10 ms (up to 10.021 ms at the slowest rate).
Inference and correction still belong entirely to the production receiver.
`simulated_clocks` records injected rates; `last_steady_source_clocks` records
independently inferred rates, applied correction and saturation. These are not
measured physical capture/presentation clocks, and a short fixture cannot certify
long-run stability. Test offsets may intentionally exceed the production cap.

```powershell
.\target\release\audiokit-test.exe run --config configs\clock-roundtrip.json --input "C:\Audio\voice-3s.wav" --out-dir target\clock-001 --quiet
```

## Correlated Mix Stress

`mix-stress` adapts WAV channels/rate using `CapturePcmGraph` with APM bypassed,
then copies each processed quantum into independent registered production render
sources. The execution plan lists this adaptation explicitly. It covers source
gain/activity/limiter, normalization, source resampling and master protection,
not capture hardware, codecs, jitter or source-clock inference. Clock correction
is zero/bypassed. It is labeled `render-stress`, never full E2E.

`mix_stress.sources` includes optional exact-silent sources; all other sources
are identical in-phase replicas. This deliberately stresses correlated peaks,
not independent natural conversations or speech intelligibility. Counts 1..=64
must fit the production render admission limit. Gain uses the production range
and fade. Per-run admitted source-frame budgets include frontend filter tails;
source-count matrices additionally reserve a sum of those per-case work caps.
Partial EOF quanta go to explicit drain, not fake full render demand.

```powershell
.\target\release\audiokit-test.exe run --config configs\mix-stress.json --input "C:\Audio\voice-3s.wav" --out-dir target\mix-001 --quiet
.\target\release\audiokit-test.exe sweep --config configs\mix-stress.json --matrix configs\mix-sweep.json --input "C:\Audio\voice-3s.wav" --out-dir target\mix-sweep-001 --quiet
```

The example matrix covers 1/2/4/8/16/32/64 sources with a source clip no longer
than three seconds. Aggregate metrics retain peak before the master limiter,
maximum energy-active source counts, reduction/clamps/queues and actual sample
ceiling checks after trace loss. A paired one-source versus one-audible/seven-silent
regression compares full output WAV bytes in both mono voice and stereo desktop
profiles, not only activity counts.

`receive_execution` measures render work in this codec-free scenario; it includes
source admission plus render for ordinary mix blocks. For roundtrip it measures
receive/render calls. Budget counters compare those measured calls against the
nominal or simulated render period; drain is not budgeted. Overruns are observed
CPU/OS timing, not a hard realtime certification or an automatic audible-failure
check. Capture preprocessing, tracing, artifact I/O and host scheduling are not
included in that deadline scope. Percentile histograms are still unavailable.
Ordinary render snapshots include worker elapsed/budget nanoseconds and an
over-budget flag. Analysis exposes flagged stage/sample ranges without calling
them audible glitches; aggregate counts survive truncated tracing.

## Virtual Forwarding Faults

Roundtrip `transport` config accepts fixed delay, independent nonnegative delay
jitter, loss/duplicate probability in per-mille, delay of every Nth original and
one scheduled forwarding pause. These operate on actual Opus payloads and original
wrapping sequences. SHA256 draws keyed by seed/packet ordinal/domain provide
portable deterministic decisions; source DSP is unchanged. The default transport
is immediate and lossless. PCM-only scenarios reject enabled transport faults.

Reorder selection delays a packet; it does not guarantee observed reordering if
the delay is too small or surrounding packets are lost. Duplicate copies arrive
after the original at the same simulated timestamp. A pause holds deliveries due
inside its interval until its end, without pausing capture or render callbacks.
This is network/forwarder stall, not a DSP-worker or output-callback stall.
Independent sample-clock offsets are configured separately from transport faults;
they do not simulate a worker/output-callback pause or an abrupt device-clock jump.

The pending-copy cap (default 1024, hard 4096) fails explicitly before queue
mutation. Normal demand continues after capture EOF only while packets remain
in flight, then the receiver explicitly drains. Trailing loss cannot be inferred
without a following sequence or explicit remote media-end protocol. No extra
demand is invented to count trailing loss as PLC.

`transport_schedule` records emission/selection/drop/due time and the encoded
sample range. `transport_arrival` records original ordinal, sequence, copy kind,
scheduled delay, receiver admission outcome and observing callback time. Arrival
timestamps are simulated host nanoseconds; callbacks observe arrivals at 10 ms
resolution by default (the configured render rate may shift it slightly). Payloads
are not included in trace. Original/drop/copy/delivery/pending
totals survive trace truncation. Virtual delay is not measured server latency.

```powershell
.\target\release\audiokit-test.exe run --config configs\voice-faults.json --input "C:\Audio\voice-3s.wav" --out-dir target\fault-001 --quiet
.\target\release\audiokit-test.exe analyze --bundle target\fault-001 --json
```

The example bypasses APM to isolate receiver behavior. Fault runs still check
finite output, media received, decoder errors, transport accounting, render gaps
and sample ceiling. A lossless expectation applies only to unimpaired transport.
Check failures return 1 with a completed bundle, not a runner crash; 100% loss is
not healthy media even though its silent PCM is finite. FEC attempts are not proof
of successful recovery or encoded redundancy.

Analysis includes at most 64 flagged retained intervals with stage/sample/time
domains and flags, plus `evidence_omitted`. Injection selection is observed test
input, not proof of an audible fault or its cause. Recorder loss and analysis
truncation are separate; unrecorded intervals remain unknown.

## Serial Parameter Matrix

`sweep` uses the same runner, configuration validator and production factory.
Typed Cartesian axes cover bitrates, ptimes, noise levels, encoded jitter startup
targets and render-stress total/silent source counts. Empty axes retain the base
setting. Uncovered codec/jitter/mix axes
and noise axes on bypassed APM are rejected. All combinations and actual graph
construction are preflighted before creating the root directory.

```powershell
.\target\release\audiokit-test.exe sweep --config configs\voice-faults.json --matrix configs\jitter-sweep.json --input "C:\Audio\voice-3s.wav" --out-dir target\jitter-sweep-001 --quiet
```

The checked-in example compares 40/80/120 ms startup targets against identical
fault decisions and the same immutable input snapshot. Maximum cases, input
duration, sum of per-case output caps and conservative artifact reservation are
bounded. Reservations include hard JSON caps, WAV caps and consented retained
input per case; they can exceed actual output size substantially. They limit
media/work/space, not OS execution wall time. Cases run serially, never in parallel.

Each `case-000` directory is an ordinary independently analyzable/replayable
bundle. `base-config.json` and `matrix.json` record the base controls and axes,
even when cancelled before the first case. `sweep.json` records build identity, case IDs,
check failures/errors, planned versus attempted cases, source hash and reservation.
Partial runs/cancellation finalize available cases and the summary. No later
case starts after cancellation; no output directory is overwritten. Disk failure
can still prevent finalization. CLI returns the summary's exit code.

## Build and Run

From the AudioKit workspace (PowerShell):

```powershell
cargo build -p audiokit-test --release --offline --locked
.\target\release\audiokit-test.exe list-scenarios --json
.\target\release\audiokit-test.exe describe-scenario file-roundtrip --json
.\target\release\audiokit-test.exe validate --config configs\desktop-roundtrip.json --input "C:\Audio\input.wav" --json
.\target\release\audiokit-test.exe run --config configs\desktop-roundtrip.json --input "C:\Audio\input.wav" --out-dir target\desktop-001 --quiet
.\target\release\audiokit-test.exe analyze --bundle target\desktop-001 --json
```

Paths above are examples; replace the WAV with your own file. Output directories
must not exist. Their parent must exist. Input supports mono/stereo float32 and
signed PCM16/24/32 WAV, 8..=192 kHz with integral 10 ms sample counts (e.g. 44.1/48
kHz, but not 22.05 kHz yet). Other file codecs are not silently converted.

`configs/file-processing.json` tests the accepted Moderate NS/HPF/AGC2 baseline,
adaptive gain off. `voice-roundtrip.json` uses mono 96 kbps; desktop uses stereo
196 kbps. Both default to 20 ms. Config supports 10/40/60 ms for isolated tests;
this is not server negotiation. The virtual payload budget is configurable and
must be replaced with the SDK-negotiated budget in a future host scenario.
Exact bit/s are retained; insufficient payload capacity fails rather than silently
lowering quality. CLI `--scenario` overrides only the scenario, not other settings.
Suppression accepts `off`, `low`, `moderate`, `high`, `very_high`; to bypass all APM
set `processing.enabled` to false. Desktop additionally needs its desktop bitrate.

Defaults, numeric units and bounds are defined by Rust `RunConfig`; unknown fields
and schema versions are rejected. Use `describe-scenario` to export the fully
expanded default configuration before tuning it. Requested/effective configs and
actual execution plans are retained in every report.
With `--input`, validation also constructs the actual selected production graph,
including resampler delay budgets and codec/backend settings. Run uses this same
factory before creating its output directory; configuration failures leave no
partial bundle. Without input, validation checks configuration and compiled
capabilities only, not unknown input-format/backend combinations.

Default application features are `processing-sonora` and `codec-opus`; libopus
needs the existing C toolchain. Neither Slint nor a display service is needed.
To build without Opus or Sonora:

```powershell
cargo build -p audiokit-test --no-default-features --offline --locked
```

Use a config with `processing.enabled=false` for that build's PCM scenario.
Requesting a missing backend returns a structured capability error, not fallback
audio. After switching features, rebuild with the desired flags before using the
shared executable path. Core-only builds still do not depend on the testkit.

## Diagnostic Bundle and Replay

Every successful run writes `processed.wav`, `config.json`, `diagnostics.json`,
`trace.json` and `manifest.json`. WAV is float32 without secondary Opus encoding,
automatic level normalization, leading-silence trimming or tail removal.
Source/master processing within the selected production graph still applies.
JSON does not contain original input paths, accounts or credentials.

Input retention is **off by default**. `--retain-input` explicitly authorizes
including the original WAV bytes (including any original metadata) as `input.wav`.
Only use it for audio you consent to include and share. A test run itself authorizes
its requested processed output recording; it does not authorize microphone or
remote/system capture, neither of which these scenarios open.

```powershell
.\target\release\audiokit-test.exe run --config configs\file-processing.json --input "C:\Audio\input.wav" --out-dir target\voice-001 --retain-input --quiet
.\target\release\audiokit-test.exe replay --bundle target\voice-001 --out-dir target\voice-002 --quiet
.\target\release\audiokit-test.exe compare --baseline target\voice-001 --candidate target\voice-002 --json
```

A metadata-only bundle can still replay when `--input` supplies the original
hash-matching WAV or packet JSON. It cannot reconstruct audio from trace text. Replay re-executes
the original file config, not real OS callback scheduling. Build/target/backend
differences are visible; a matching waveform hash is expected for deterministic
same-build runs, not demanded across all platforms.

Imports verify schema, fixed safe filenames, canonical containment, sizes, SHA256,
run/config identity, trace ranges and event order. Parsing/replay uses the same
validated byte snapshot, not a second potentially changed input read. Integrity
means content hashes match the supplied manifest, not that the manifest is signed
or its producer trusted. Analysis calls recorded checks recorded evidence; it does
not announce an audible defect or root cause from a sample jump alone.

Ctrl+C is cooperative. A cancellation or processing-budget failure finalizes a
partial WAV/report/manifest before returning. Disk failure can prevent finalization
and is returned explicitly. Existing directories/files are never overwritten.
Analysis/comparison do not modify input bundles or open devices/network.

## Diagnostic Boundaries and Latency

Trace records carry run, ordinal, anonymous source/stream/epoch, config generation,
stage-local sample range, sample-clock domain and virtual-host scheduling time.
They currently contain frontend/encoded/render snapshots, not every internal DSP
tap. Input/APM internal taps, precise packet sample-dependency timelines, drain
snapshots and the independent offline oracle are listed as unavailable.

Events form a bounded prefix (default 4096 events, 8 MiB serialized payload;
pretty-JSON framing adds space). Reports retain attempted/dropped counts and the
first lost ordinal; missing intervals cannot be interpreted as healthy. Aggregate
receive outcomes and ordinary-demand render gaps/queues/clamps/drift survive event
truncation. Drain metrics are explicitly outside the current render aggregate.
Decoded/input/output PCM and WAV bytes also have hard configured limits. These
initial offline scenarios hold bounded PCM in memory; they are not the future
real-time artifact-worker implementation.

Capture/receive execution uses `Instant` around worker calls, excluding diagnostic
serialization, post-run analysis and file writes. It includes those calls' own
buffer allocation and production in-call measurements. Sum/max/count are available;
p50/p95/p99 are null until histogram instrumentation lands. Core-backend group delay,
encoder lookahead and render-stage frame delays are separately identified from
configured ptime/startup buffers. Device/server delays and total signal-aligned
E2E remain unknown. **Do not add these fields into an asserted E2E latency.**

## Machine Protocol and Verification

All command results are versioned JSON on stdout; progress uses stderr. `--quiet`
suppresses progress. Exit codes: 0 recorded checks pass, 1 recorded checks fail,
2 invalid arguments/config/input, 3 capability unavailable, 4 runtime/I/O failure,
130 cancelled. `compare` is descriptive and returns 0 after successful comparison;
callers decide whether its differences are acceptable. `devices` and
`--gui` currently return capability-unavailable instead of pretending to run.

```powershell
cargo test --workspace --all-features --offline --locked
cargo test -p audiokit-testkit -p audiokit-test --no-default-features --offline --locked
cargo clippy --workspace --all-targets --all-features --offline --locked -- -D warnings
```

Current regression fixtures are generated signals, not redistributed user music.
The tests cover exact PCM bypass, resampler/APM tail accounting, real Opus profiles
and all durations, replay equivalence, artifact tampering/path escape, diagnostic
caps, output limits, cancellation and no-overwrite behavior. Hardware timing,
perceptual NS/AEC calibration and cross-platform builds still require later work.

Windows validation on 2026-10-03: 93 workspace unit/integration tests plus one
doctest pass; the no-default-feature CLI/testkit suite also passes. Strict Clippy
and warning-as-error Rustdoc pass. A local 44.1 kHz stereo instrumental excerpt
passed the Release desktop roundtrip with zero decoded failures or steady render
gaps, and replay produced byte-identical output WAV. This is deterministic offline
evidence, not a hardware timing or perceptual-quality certification.

The next A5 slice adds deterministic faults, bounded evidence timelines and serial
sweeps, with regressions for duplicate rejection, all-loss unhealthy media,
fault replay equality, queue exhaustion, pending-copy cancellation, preflight
budgets and whole-matrix validation. These are virtual receiver tests, not the
external packet-trace scenario or the device migration.
Validation now passes 103 workspace unit/integration tests plus one doctest.
Release clean transport retains the first slice's exact output WAV. The local
three-second instrumental fault run produced 151 originals, four injected drops,
two duplicate copies, one late arrival, three PLC slots and two FEC attempts,
with zero decoder errors and byte-identical replay. The 40/80/120 ms sweep recorded
17/1/0 late arrivals and 5/2/0 PLC slots for the same fault decisions. These counts
demonstrate the debugging workflow, not a recommendation to raise production
buffers or a claim that FEC successfully recovered each missing packet.

The third A5 slice adds independent rational capture/render clocks, correlated
mix stress, silent-source pairing, source-work admission and per-block worker
budget evidence. Windows validation on 2026-10-03 passes 109 workspace
unit/integration tests plus one doctest; the no-default-feature suite passes
24 unit/integration tests plus one doctest. Format checking, both strict Clippy
configurations, warning-as-error Rustdoc and Release build pass.

Release evidence under `target/validation/a5-20261003/`: `desktop-clock-zero`
retains the previous clean transport waveform; `independent-clocks` infers
approximately +400/-100 ppm with -500 ppm compensation and has byte-identical
replay. The seven-case `mix-sweep` passes its scoped sample-ceiling/queue/activity
checks. The observed 32-source maximum worker call was 6.90 ms, while 64 sources
had 42 of 300 ordinary calls over 10 ms, with a 13.80 ms maximum. A later
`mix64-timeline` run with per-block timing retained the exact waveform and recorded
11 of 300 calls over budget, maximum 13.77 ms. Its analysis locates overrun
intervals by stage/frame/time. CPU/OS scheduling makes these timings variable;
neither run certifies a hardware callback deadline. In particular, waveform
checks passing does **not** accept 64-source realtime performance. The production
default admission limit remains unchanged at 32; profiling is needed before
claiming a larger realtime capacity. Generated evidence/music are not committed.

The fourth A5 slice adds `receive-simulation` with bounded external original Opus
arrivals and recorded fixed-quantum demand. Windows validation on 2026-10-03
passes 117 workspace unit/integration tests plus one doctest and 26 headless
unit/integration tests plus one doctest. Both build modes pass strict Clippy;
format checking, warning-as-error Rustdoc and Release build pass. Build-mode tests
run serially because they share the CLI executable path; overlapping builds can
otherwise replace the executable while integration tests are running.

Generated voice/desktop real-codec packet fixtures reproduce the previous
roundtrip WAV byte-for-byte. Replays preserve wrapping/duplicate/reordered
sequences, irregular demand timestamps and scoped loss recovery observations.
Missing payloads, mid-stream state and omission declarations keep partial
reproduction; corrupt packets fail decoder checks. Tampering, byte/payload/work
limits, cancellation and synthetic-tail output exhaustion are covered. Release
`packet-missing-release` under `target/validation/a5-20261003/` confirms a missing
payload emits `partial-packet-replay` and check-failure exit 1, never healthy media.
A no-Opus CLI successfully analyzes that bundle. This is offline model evidence;
live client packet export, multistream/lifecycle replay and hardware E2E remain
unimplemented. See `packet-replay.md` for exact supported boundaries and commands.

The fifth slice adds independently gated worker/consumer pauses around the
roundtrip production graph and bounded output SPSC, optional substage timing and
constant-memory timing percentiles. Clean scheduling/profiling preserves output
PCM; faults retain queue conservation, consumer sample/time ranges, recovery and
quality-check failures rather than treating every completed run as healthy audio.
See `scheduler-profiling.md` for configuration, interpretation and limitations.

Local Release `mix-stage-profile` evidence under `target/validation/a5-20261003/`
retains byte-identical WAVs for all seven previous mix-sweep cases. For 64 voice
sources, mean source processing was 8.05 ms of the 8.62 ms ordinary worker mean;
bus mixing was 0.01 ms, master 0.19 ms, signal analysis 0.21 ms and PCM admission
0.15 ms. These are attributed regions, not a complete additive CPU decomposition.
The worker maximum was 12.40 ms with 32/300 calls over 10 ms; 32 sources had a
6.42 ms maximum and no observed overruns. Target-machine short-run observations
include profiling overhead and OS noise, not a hardware or 64-source acceptance.
Optimization should first investigate source DSP, not reduce audio-quality
settings or increase queue capacity to mask the overruns.

Release example faults completed with expected quality-check exit 1: the 80 ms
worker pause produced 3840 underrun frames and one explicit clock recovery, while
the independent 80 ms consumer pause produced 1920 rejected frames plus 1920
discarded backlog frames and no receiver recovery/queue underrun. Both retained
valid complete diagnostic bundles, trace and output WAV without native devices.

Validation passes 125 workspace unit/integration tests plus one doctest, and
30 headless unit/integration tests plus one doctest. Both strict Clippy modes,
format checks, warning-as-error Rustdoc and Release build pass. Deterministic
regressions include EOF with delayed packets at 10/20 ms ptime, independent
sample clocks, trace truncation, queue conservation, cancellation, explicit
recovery without residual drift and waveform-identical replay.

Source-stage diagnosis further divides ordinary source processing into bounded
queue/gain/activity/limiter/channel-map timing sets. These are nested regions;
do not double-count them with the parent. Mono/stereo tests retain exact output,
and a core graph regression verifies that toggling timing does not reset gain,
activity, limiter or tail histories. `source-dsp-diagnosis.md` records three
Release repeats with disabled-timing controls: source limiting dominates, and
64-source realtime performance remains unaccepted. This diagnostic slice does
not optimize or alter the accepted DSP algorithms.

The subsequent equivalent limiter target-cache slice passes 129 workspace
unit/integration tests plus one doctest, and 30 headless unit/integration tests
plus one doctest. A frozen scalar scan compares PCM/gain bits, counters and cursor
across boundary formats, block sizes, wraps, reset/reconfiguration and complete
tails. The independent reconstruction corpus remains passing. All 16 optimized
Release stress WAVs match the pre-change output exactly; 64-desktop calls still
sometimes exceed 10 ms, so production admission remains 32. See
`limiter-cache-optimization.md` for measurements, memory cost and reproduction.

The first optional Slint workbench now delegates file scenarios to the same runner.
It supports parameter snapshots, background execution/cancel/close finalization,
hash-validated result loading, portable bundle/WAV export and explicit native WAV
audition. GUI/API waveform parity, consent reset and 1120x900/720x900/720x700 software
screenshots are covered. See `gui-workbench.md` for commands, privacy and boundaries.
Windows validation passes 133 workspace unit/integration tests plus one doctest,
and 33 headless unit/integration tests plus one doctest. Rust 1.92 compile-checks all
workspace features/targets. Native audition, microphone/duplex and live server E2E
are not hardware-accepted by these offline tests; A6 remains pending.

The 2026-10-04 material/i18n slice adds explicitly authorized bounded microphone
acquisition through optional CPAL, then stops the device before reusing offline
DSP. Native material counters survive validated export; replay is waveform-identical
without claiming new hardware health. The Slint UI bundles Simplified Chinese and
English; switching language does not mutate a running config. Windows validation
passes 136 all-feature workspace unit/integration tests, 33 backend-free headless
tests, 36 headless CPAL-only tests and a doctest in each mode. Strict all-feature
and backend-free Clippy, warning-as-error Rustdoc, fmt/diff checks and Rust 1.92
all-feature/all-target compile checks pass. Chinese/English desktop/narrow/compact
software screenshots are checked. No native devices are opened by this validation.
Release GUI builds successfully; its actual CLI replay of the GUI-exported
capture fixture matches input, config, plan, checks and output WAV bytes.
See `microphone-material.md`; live microphone DSP, aligned AEC, platform hardware
acceptance and the A6 host bridge remain pending.
