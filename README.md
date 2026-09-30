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
4x FIR true-peak detector is shared by the limiter and diagnostics; a second,
independent test oracle is still required before claiming a true-peak guarantee.
