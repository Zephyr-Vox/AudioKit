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

This workspace still has no transport/device backend or Slint GUI. Moving those
components is a later milestone, not implied by the backend example.
