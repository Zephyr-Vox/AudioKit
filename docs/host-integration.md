# Host Integration

## Ownership

Instantiate one `CaptureGraph` per outbound stream and one `ReceiveGraph` per
playback device/clock epoch. Each graph belongs to one synchronous worker. Its
backend `Box<dyn AudioEncoder/AudioDecoder/VoiceProcessor>` is never shared with
another worker. Port construction and configuration changes occur off callbacks.
Transport negotiation, account identity, permission prompts, task cancellation,
recording consent and artifact paths belong to the host.

Map protocol speaker/stream IDs to opaque `SourceKey`; assign a fresh `StreamEpoch`
on reconnect, source restart or format change. Register mono voice and stereo
desktop before admission. The codec format is separate from the physical device
format. Select one advertised `PacketDuration` at connection establishment, never
change ptime in the middle of prediction history, and pass the real payload budget.

`CaptureGraph::push_native` accepts complete native f32 channel frames. It maps
channels, continuously resamples, optionally processes 10 ms voice quanta, then
packetizes for the encoder. Desktop rejects voice processing. `CapturePacket::pcm`
is pre-codec audio; the host may retain/export it only under explicit recording
consent. Backend failure stops capture rather than allowing half-consumed history
to continue. Unknown processor algorithmic latency remains `None`.

## Device Demand

`ReceiveGraph::push_packet` submits payload/sequence/epoch and host-monotonic arrival
time without producing audio. Call `render_into` only for actual device demand;
do not run a separate arrival-driven mixer or add another packet startup wait.
Honor the configured per-call demand limit and handle source admission failures
explicitly. A frame cursor counts per-channel frames, never interleaved samples.

The receiver owns the encoded startup/FEC/PLC policy and isolated source DSP.
The renderer owns gain/activity, source clock correction, source protection,
channel expansion, independent voice/desktop buses and master protection.
All source clock correction occurs before source/master limiting, not afterward.
`RenderGraph` is also usable for a host that already has properly scheduled PCM;
that host must not add a competing encoded jitter scheduler inside it.

After a long host pause, queue overflow/rebuffer or clock reset, call
`recover_clock` before admitting resumed packets. It clears decoder/source/output
history but preserves registered source epochs. Host-time regression is rejected,
not interpreted as a clock-rate update. Explicit `begin_drain`/`drain_into` retains
finite buffered real packets, known holes and DSP tails; EOF does not invent PLC.
`abort` discards state immediately and is idempotent.

## Ports and Reference

CPAL callbacks convert/copy complete frames through fixed-capacity SPSC ports.
Only a single producer and consumer own each endpoint; they cannot be cloned.
Overflow rejects the newest suffix and increments counters. Playback reference
is published after conversion, for the samples actually handed to the device,
including startup/underrun silence. A slow reference reader cannot block output.
The device buffer handoff is not a hardware-presentation or acoustic timestamp.

Use the raw frame API when you need flags/device timestamps. The `CapturePort`
PCM interface deliberately leaves unmapped host time unavailable. A CPAL capture
port's handoff observations share its private origin, not another port's origin.
WASAPI QPC metadata is raw device evidence, not an automatically aligned AEC delay.
Do not subtract these clocks blindly. Align/resample actual reference on a worker,
bound its FIFO, reset after gaps, and disable AEC if reference alignment is invalid.
The current extraction does not yet provide a certified multi-device time mapper.

## Diagnostic Contract

Capture statistics separate input/encoded frames, EOF padding, filter/codec
lookahead and processor execution. Render metrics include the common cursor,
per-source FIFO/activity/gain/clock correction, fixed filter delay, rate-staging
frames, both limiter lookaheads and pre/post signal measurements. Receiver counters
cover ingress outcomes, decoder/FEC/PLC errors, retirement and decoder CPU duration.
Port counters distinguish startup silence, underrun, dropped input/reference and
native failures. FEC success is not proof that the packet contained redundancy.

Keep algorithmic delays, queue residence and execution durations separate. Total
acoustic capture-to-playback latency is unavailable without capture/presentation
mapping; do not sum CPU times and call it measured E2E latency. Testkit report
aggregation, privacy/anonymization, event retention and persistent artifact I/O
are the next milestone. The library's per-block metrics are not that exporter.

See the real-codec graph integration test for construction and paced simulation.
The callbacks test uses the production adapters without opening devices. Neither
is evidence that the existing CLI already uses the new device/receive graph.
