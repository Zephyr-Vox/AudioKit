# External Receiver Packet Replay

`receive-simulation` consumes original Opus arrivals and recorded render demands,
not WAV and not the metadata snapshots in `trace.json`. It calls the same
`ReceiveGraph`/`RenderGraph` used by the production audio core, including jitter,
FEC/PLC, bounded source-clock correction, source/master limiters and render channel
mapping. There is no capture APM, encoder, simulated forwarding, soundcard or server.
Coverage is `receiver-replay`, never hardware or server E2E.

## Input Contract

The authoritative strict serde models are `PacketTrace`, `PacketSource` and
`RecordedPacket`. Version 1 supports one anonymous source, one fixed epoch and
one negotiated ptime. Voice is 48 kHz mono; desktop is 48 kHz stereo. Source kind,
format and ptime must agree with the effective run config; mismatches fail before
artifact creation, never silently convert or reconfigure the source. Mixed-source
sessions, epoch switching and mid-session ptime changes are not supported yet.

```json
{
  "schema_version": 1,
  "source": {
    "source_id": 7,
    "stream_id": 3,
    "epoch": 11,
    "kind": "voice",
    "format": { "sample_rate_hz": 48000, "layout": "mono" },
    "ptime": 20
  },
  "starts_at_stream_start": true,
  "omitted_packets": 0,
  "omitted_render_ticks": 0,
  "packets": [
    {
      "sequence": 65535,
      "arrival_ns": 20000000,
      "duration": 20,
      "media_frame": null,
      "payload": null
    }
  ],
  "render_ticks_ns": [10000000, 20000000, 30000000]
}
```

This example intentionally has unavailable payload and cannot produce healthy
decoded media. Real `payload` values are numeric byte arrays containing the
original complete Opus payload, not hashes, headers, WAV samples or empty arrays.
Nonempty corrupt payloads go to the actual decoder and fail its scoped checks.

Exporter responsibilities:

- Replace account/source identifiers with per-recording anonymous nonzero IDs.
  Do not include credentials, names, network addresses or local paths.
- Use one relative monotonic host nanosecond origin for arrivals and render calls.
  It is not the remote capture clock, CPU execution time or synchronized server time.
- Preserve actual arrival order, including equal-time ordering, duplicates and
  wrapping/reordered sequences. Never sort by sequence or invent missing arrivals.
- Record fixed-size 10 ms render demands at strictly increasing host times.
  Irregular spacing is retained. Other demand sizes need a future schema extension.
- Set `media_frame` only when its source-media position is known. Null produces
  the `media_position_unknown` domain and a zero-length event range, not invented
  sample alignment. It does not affect receiver sequence processing.
- Declare missing records and initial state honestly. Omission counts may be
  null when unknown. A packet that never arrived due to network loss is absent;
  an observed arrival whose recording payload was lost has `payload: null`.

All arrivals must be no later than the final supplied render time. A recorder
whose last packets arrived after the last demand must extend its actual recording
or omit those packets and disclose the truncation; it must not invent a real
render call. Execution admits due arrivals before each demand, without exposing
future packets to an earlier tick. Packet EOF uses a bounded **synthetic drain**,
not captured connection closure or output presentation. Its PCM is retained in
the untrimmed WAV; its render metrics are outside ordinary recorded-demand totals.

## Resources and Reproduction

`receive_simulation` defaults to 8192 packets, 6000 demand ticks and 60000 ms.
Hard maxima are 65536 packets, 60000 ticks and 600000 ms. These are count/time-work
bounds, not a wall-clock execution timeout. Original JSON bytes are limited by
both `max_input_bytes` and 32 MiB. Each payload fits `max_payload_bytes` (hard
4000) and the actual receive graph's payload cap. Ordinary output demand is
preflighted against `max_pcm_samples`; synthetic
tails remain subject to the runtime output cap. Invalid inputs fail before
creating an output directory; cancellation/runtime failures finalize partial
bundles when disk I/O permits. Files are never overwritten.

Packet bytes reconstruct audio and have the same explicit retention consent as
WAV. `--retain-input`/`retain_input` authorizes `packets.json` in the bundle.
Otherwise only metadata, input hash and requested processed WAV are exported.
Source IDs must already be anonymous; the importer cannot authenticate that claim.

- `packet-replay`: authorized original packets/schedule are retained, the producer
  declares a cold start, omission counts are zero and every supplied payload exists.
- `partial-packet-replay`: retained material has missing payloads/records/demands,
  unknown omission counts or a mid-stream start. Supplied material can rerun, but
  unavailable preceding decoder/jitter state and missing content cannot be recovered.
- `metadata-only`: original recording absent; replay needs externally supplied
  byte-identical, hash-matching packet JSON. No audio can be reconstructed from
  diagnostic `trace.json` alone.

Completeness is a producer declaration, not proof of an exhaustive physical
recording. Material completeness and decoded health are separate checks: a full
arrival trace may include genuine network loss or corrupt packets. Missing
material fails `packet_material_complete`; all-missing payloads additionally fail
`received_media`. CLI exits 1 for failed scoped checks even when the run completed.
Replaying incomplete material keeps its partial classification and failures.
Diagnostic trace truncation is separately counted and does not change input-material
completeness. Hash integrity is not producer authentication.

The shared report's legacy `input_frames` is zero because no original PCM length
is asserted. `packet_replay.input_pcm_frames` is null; record/tick counts are in
`packet_recording`. Progress has an explicit `packet_records` unit. Capture/encode
delays are not covered, receive startup remains configured, and execution timing
measures receive/render only, excluding packet admission, tracing and synthetic
drain from ordinary budgets. Each recorded demand's budget is its preceding host
interval, not a hardware callback guarantee. Aligned E2E and server/device latency
remain unknown. Common `bitrate_bps` is not an encoder control in this scenario.

## Commands

```powershell
cd F:\SourceCodes\Dev\ZephyrVox\AudioKit
cargo build -p audiokit-test --release --offline --locked
.\target\release\audiokit-test.exe describe-scenario receive-simulation --json
.\target\release\audiokit-test.exe validate --config configs\receive-simulation.json --input "C:\Audio\recording.json" --json
.\target\release\audiokit-test.exe run --config configs\receive-simulation.json --input "C:\Audio\recording.json" --out-dir target\receive-001 --retain-input --quiet
.\target\release\audiokit-test.exe analyze --bundle target\receive-001 --json
.\target\release\audiokit-test.exe replay --bundle target\receive-001 --out-dir target\receive-002 --quiet
.\target\release\audiokit-test.exe compare --baseline target\receive-001 --candidate target\receive-002 --json
```

Desktop recordings need `stream: "desktop"`, `processing.enabled: false` and
a valid desktop common profile configuration (for example `bitrate_bps: 196000`).
Transport fault and injected clock controls must be zero: the supplied recording
already defines arrivals/demand. External packet sweep is explicitly unavailable.
Replay uses one fixed effective configuration; runtime parameter changes and
source re-registration/retirement events are not represented by this first input
schema. A host recording requiring those events cannot certify exact original
host behavior through this model. Long pauses still exercise actual receiver
expiry and may fail/retire the source; no hidden re-registration is inserted.
Without the `codec-opus` feature, inspection/analysis of existing packages works,
but running/validating a decoder graph returns capability-unavailable.

The SDK/client live packet exporter and hardware schedule capture are **not yet
connected**. Existing CLI diagnostic JSON does not contain these original Opus
payloads and cannot be relabeled as packet-replay. Host integration must capture
this format explicitly; the testkit does not depend on SDK/auth/network code.
