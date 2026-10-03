# AudioKit

Shared Rust audio components for ZephyrVox clients. Transports, account state,
UI and application permissions belong to the host.

The initial `audiokit` crate defines validated PCM formats, opaque source keys,
clock/epoch metadata and synchronous codec, processing and device-port contracts.
Its core build has no SDK, native-device, async-runtime, UI or C-toolchain dependency.

```sh
cargo test --workspace --offline
cargo run -p audiokit --example in_memory_block --offline
```

DSP/backend extraction and the shared CLI/Slint test application are incremental.
See `docs/migration.md` for implemented boundaries and remaining work. No published
package or GUI is implied by the existence of these initial contracts.

## Shared DSP

The CLI now uses this workspace's linked-channel lookahead limiter, source gain
smoothing and peak/RMS/discontinuity measurements. Their validated settings and
sample history are preserved during relocation. These are worker APIs, not
device-callback-safe APIs: construction, format changes and returned buffers can
allocate. A discontinuity candidate is not proof of an audible click. The current
4x, 128-tap Hann-sinc detector is shared by the limiter and diagnostics. A separate
16x, 128-tap Blackman-sinc oracle verifies common-rate, mono/stereo burst signals
across phases, including startup and complete tails. This is a finite regression
corpus, not a universal reconstruction guarantee for arbitrary near-Nyquist PCM.
`reconstruction_headroom_db` defaults to 0.2 dB (configurable 0..=1) to protect
finite interpolation and gain-modulation residuals. Effective limiter lookahead
is at least 64 frames; the actual delay is reported, not inferred from 3 ms alone.

## Sonora Backend

`audiokit-processing-sonora` supplies AEC3, noise suppression and AGC2 with
independent capture/render formats. `VoiceProcessor` works on interleaved f32
10 ms blocks without passing through i16. Mono/stereo 8/16/32/48 kHz are supported;
other device rates need graph resampling. Full float bypass returns unchanged
samples, even where Sonora itself would otherwise insert internal conversion.
Enabled processing delay is unavailable from the library and reported as `None`.

The accepted defaults remain Moderate suppression, high-pass on, AGC2 on and
adaptive gain off. The transitional i16 entry points keep the production CLI's
conversion path unchanged; a two-second, seeded test compares its output exactly
against an independently constructed pre-extraction Sonora configuration.

```sh
cargo run -p audiokit-processing-sonora --example voice_quantum --offline
```

## Codec, Ports and Graphs

`audiokit-codec-opus` owns exclusive worker codecs. Voice is mono VoIP at
64..=128 kbps (default 96); desktop is stereo Audio at 128..=320 (default 196).
Ptime is fixed per session, normally 20 ms, with negotiated 10/40/60 ms support.
Payload budgeting applies only to enabled streams. libopus needs a C toolchain;
it is not part of the core or Sonora-only dependency graph.

The optional `audiokit-platform` crate has CPAL capture/playback and actual-consumed
reference ports (`native-cpal`), plus Windows process-tree loopback
(`windows-process-loopback`). Fixed-capacity SPSC endpoints carry native cursors,
raw timestamps and flags. Sample callbacks do not allocate, lock, perform DSP,
write files or call async APIs. OS error callbacks are a separate backend boundary.

The core's default `resampling` feature adds continuous f32 conversion and shared
`CaptureGraph`, `ReceiveGraph` and `RenderGraph`. The receiver is driven by device
demand, with one encoded startup window, source/epoch-isolated codecs/DSP, bounded
queues, FEC/PLC, independent speech/media activity normalization and finite drain.
Arrival-time drift is explicitly inferred, not presented as a remote measured clock.

```sh
cargo test -p audiokit --no-default-features --offline
cargo test --workspace --all-features --offline
cargo run -p audiokit --release --example render_budget --offline
```

See `docs/host-integration.md` for worker ownership and timing contracts.
The first A5 testkit/CLI slice is available; see `docs/testkit.md` for file scenarios,
diagnostic bundle export, integrity checks, analysis, signal replay, deterministic
virtual forwarding faults, bounded serial sweeps, correlated mix stress and
independent virtual clocks and single-source external Opus packet replay.
See `docs/packet-replay.md` for recording consent, completeness and schedule limits.
See `docs/scheduler-profiling.md` for independent worker/consumer pauses, bounded
output-queue evidence, recovery policy and opt-in production substage profiling.
See `docs/source-dsp-diagnosis.md` for the measured 32/64-source limiter bottleneck,
mono/stereo controls and the limits of the CPU attribution.
See `docs/limiter-cache-optimization.md` for the behavior-preserving target cache,
bitwise reference coverage and before/after worker CPU results.
Slint, native device scenarios and the server host bridge are still pending.
The CLI delegates shared DSP/Opus/resampling, but still uses its legacy device and
receive scheduler until A6. No new microphone/speaker or macOS/Linux verification
is implied by the deterministic tests in this workspace.

## Debugging CLI

```sh
cargo run -p audiokit-test --release --offline --locked -- list-scenarios --json
cargo run -p audiokit-test --release --offline --locked -- run --config configs/desktop-roundtrip.json --input /path/to/input.wav --out-dir target/desktop-001 --quiet
cargo run -p audiokit-test --release --offline --locked -- analyze --bundle target/desktop-001 --json
cargo run -p audiokit-test --release --offline --locked -- sweep --config configs/voice-faults.json --matrix configs/jitter-sweep.json --input /path/to/voice-3s.wav --out-dir target/sweep-001 --quiet
```

Output is float32 WAV plus a versioned diagnostic bundle. Source audio is not
included unless `--retain-input` is explicitly supplied. The PCM-only scenario
shares the production capture frontend and does not roundtrip through Opus.
Current scenes are offline/virtual, not hardware or server E2E. Use new output
directories; existing files are never overwritten.
