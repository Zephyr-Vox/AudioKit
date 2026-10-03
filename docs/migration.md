# Migration Status

The working baseline is CLIClient commit `6540b73`, preserving the previously
validated native playback and duplex capture changes. The source-file SHA256
manifest and 185 unit / 23 integration test result are retained locally at
`../CLIClient/target/audiokit-baseline/manifest.json`; no user audio assets are committed.

Mic report schema 7 separates Sonora execution from WAV/Ogg writes. Artifact I/O
has its own timing summary and is excluded from the packet-ready latency estimate.

The first core boundary provides validated format/packet-duration types, f32
worker blocks, opaque source/stream keys, epochs, clock-domain-tagged timestamps
and synchronous backend interfaces. The host continues to own network negotiation.

Shared limiter, gain smoothing and signal measurement implementations have moved
to the core crate. CLI compatibility modules re-export those exact implementations;
the SDK signal analyzer is only a zero-copy wrapper. The limiter's original tests
now run independently of the SDK. Host device telemetry remains in CLIClient.

Sonora configuration, DSP and scratch buffers now live in the separate backend.
The CLI delegates signed-16 quanta to it and keeps SDK packet shape, worker mutexes
and atomic delay hints. The core f32 contract has separate render/capture formats;
production conversion is deliberately unchanged pending graph resampling work.
Adaptive-default tests were moved, not removed. Sonora delay-setter errors now
propagate instead of being discarded.

## Pre-Testkit Extraction

Opus construction, packet validation and legacy i16 helpers now live in their own
backend. Independent pre-extraction libopus encoding/decoding checks remain
bit-exact. The CLI maps SDK formats/errors/source keys and delegates codec work;
voice-only payload negotiation no longer requires a disabled desktop profile to fit.
The mic probe preserves exact bit/s and complexity rather than rounding user settings.

All 13 old resampler tests moved with the production filter. The CLI packet-shaped
adapter remains thin; continuous graph PCM is f32 and has no per-block zero padding
or integer conversion. EOF filter, processor and packet padding are separately counted.
Queue feedback uses milliseconds, elapsed time and ppm/second instead of a fixed
interleaved-sample gain. Legacy CLI startup/overflow now reset this feedback state.

CPAL and Windows process capture port building blocks are independent of SDK/Tokio.
Virtual tests call production sample conversion and reference publication, guard
alloc/free/realloc, and check startup silence, overflow, underrun and actual integer
quantization. Windows activation arguments survive an abandoned timeout receiver;
native PID ownership and process exit monitoring are retained. Borrowed VT_BLOB
arguments are disarmed before PROPVARIANT teardown so a foreign deallocator cannot
free the Rust-owned parameters. The CLI reuses this activation owner; other native
port plumbing remains pending A6. Tests do not open devices.

Shared capture/receive/render owners are implemented. Decoder work occurs only on
bounded device demand, not arrivals. Source admission defaults to 32 and may be
configured up to 128; actual hardware budget, not this upper limit, governs deployment.
Speech activity excludes silent sources and is independent of the desktop bus.
Epoch changes clear source history; explicit clock recovery also clears the master's
old delayed PCM. Sequence jumps, wrap/reorder, FEC/PLC, malformed shapes, abort/drain,
actual codec roundtrips and correction direction have deterministic coverage.

Ten-minute tests cover clock estimators and queue-controller models, not ten minutes
of every full render graph. A short full graph test covers independent source/device
clocks. Inferred arrival clocks are not synchronized remote capture timestamps.

The first A5 slice now provides the shared testkit, file-processing/real-codec
virtual-roundtrip runner, diagnostic bundle v1 and headless CLI analysis/replay/compare.
CaptureGraph delegates its PCM frontend to CapturePcmGraph, so partial-chain tests
do not need a dummy encoder or duplicate DSP. The next slice adds deterministic
virtual loss/jitter/reorder/duplicate/forwarding-stall and bounded serial parameter
sweeps. Correlated render-only mix stress and independent capture/render virtual
clocks now run on the same production graphs. Slint, device-probe migration and
external packet trace replay remain pending. See `testkit.md`.
A6 still needs agent/TUI production device/scheduler
switching, bounded legacy codec-map removal and hardware/E2E verification. Native
device-to-host timestamp mapping and AEC delay calibration are not yet certified.
Non-integral 10 ms native rates are explicitly rejected by the current filter-block
adapter; supporting them is an outstanding resampler boundary improvement.

SPEC files remain uncommitted. Windows Rust 1.98 is the tested environment; macOS,
Linux and the declared Rust 1.91 floor have not been revalidated. CLI legacy Clippy
lints remain visible; no broad warning suppression is introduced.

The true-peak correction is a separate behavior change: a longer independently
validated production FIR plus configurable reconstruction headroom, not a silent
relocation. See `algorithms.md`. The 2026-10-02 baseline validation: 72 AudioKit tests, 160 CLI unit
tests and 25 CLI integration tests pass. AudioKit all-target/all-feature strict
Clippy and warning-as-error Rustdoc pass. CLI Release checks pass; its pre-existing
strict Clippy failures are not reported as a successful gate.

Local evidence is retained under `target/validation/`: complete workspace test log,
independent oracle log and release worker budget JSON. CLI results remain under
`../CLIClient/target/audiokit-baseline/final-cli.log`. Generated artifacts and user
recordings are not committed.

The 2026-10-03 A5 slice passes 93 AudioKit unit/integration tests and one doctest,
plus the optional-backend-free CLI/testkit suite. Strict all-feature/headless
Clippy and warning-as-error Rustdoc pass. The consuming CLI regression still
passes 160 unit and 25 integration tests. Release offline desktop music replay
produces byte-identical WAV with zero steady render gaps or decode failures.
Logs and local bundles are under `target/validation/a5-20261003/`; the consuming
CLI log is `../CLIClient/target/audiokit-a5-cli.log`. No native devices or server
were opened for this A5 validation; it does not complete A6.

The fault/sweep slice passes 103 unit/integration tests plus one doctest, strict
all-feature/headless Clippy and Rustdoc. Logs are
`target/validation/a5-20261003/fault-sweep-tests.log` and
`fault-sweep-headless.log`. Release clean WAV stays byte-identical to the previous
slice; deterministic fault replay also matches. Local fault/sweep bundles in that
directory record actual receiver late/duplicate/FEC/PLC outcomes, not just injected
selections. This changes no production DSP/defaults and opens no native devices.
