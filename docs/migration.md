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

The existing production device, receive scheduler, Sonora and Opus implementations
remain in CLIClient until their corresponding extraction and regression gates pass.
The full platform engine, unified scheduler and CLI/Slint test UI are not implemented
by the initial core-types milestone. SPEC files remain uncommitted by request.
