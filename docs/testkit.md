# AudioKit Debugging Suite

The first A5 slice is implemented: a reusable `audiokit-testkit` library and the
headless `audiokit-test` application. CLI is for automation, reproduction and
evidence analysis. Slint will provide the human E2E/recording/listening/export
workbench on the same runner. GUI, real devices, packet-trace replay, fault
injection, parameter sweeps and host/server E2E are **not implemented here yet**.
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
hash-matching WAV. It cannot reconstruct audio from trace text. Replay re-executes
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
callers decide whether its differences are acceptable. `devices`, `sweep` and
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
