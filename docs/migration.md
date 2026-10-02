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

The existing production device, receive scheduler, resampler and Opus implementations
remain in CLIClient until their corresponding extraction and regression gates pass.
The full platform engine, unified scheduler and CLI/Slint test UI are not implemented
by the initial core-types milestone. SPEC files remain uncommitted by request.

Current checks: 25 AudioKit/backend tests and 172 CLI unit / 25 CLI integration
tests pass on Windows with Rust 1.98. Core/backend strict Clippy passes. The CLI's
strict Clippy gate still reports pre-existing legacy lints (including the native
open helper's argument count and newer style lints); those are not hidden with
workspace-wide allows. macOS/Linux compilation and new hardware checks have not
been repeated for this extraction. The declared Rust 1.91 floor has not been
revalidated locally because only the 1.98 toolchain is installed.
