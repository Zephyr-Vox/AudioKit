# Microphone Material Capture

This tool isolates native acquisition from deterministic DSP. It explicitly opens
one input port, records bounded mono/stereo float PCM at its actual negotiated
format, closes the port, then invokes the SAME runner used by file processing,
Opus virtual roundtrip and mix stress. It does not duplicate production algorithms.
No output device, network or AEC reference is opened. NS/AGC operate after capture;
this is NOT live monitoring, production scheduling or duplex/hardware E2E acceptance.

## GUI

```powershell
Set-Location F:\SourceCodes\Dev\ZephyrVox\AudioKit
cargo run -p audiokit-test --release --locked --features gui -- --gui
```

1. Choose capture PCM and voice/mono for microphone NS/AGC testing.
2. Refresh audio devices, choose the input device and recording seconds (1..=60).
3. Adjust NS/high-pass/AGC2/adaptive gain; imported JSON never authorizes retention.
4. Enable original audio retention explicitly when you need offline reproduction.
5. Press Record. Finish recording processes earlier material. Cancel requests
   cancellation of both phases; close waits for the owned worker to finalize.
6. Inspect Material capture and Checks, then audition/export processed WAV or the
   diagnostic bundle. Play is explicit and defaults to volume 0.2.

To compare algorithms using exactly the same input, retain the first recording
and import its `input.wav` for subsequent file runs. Recording again is NOT a
controlled A/B: voice, background and native timing may change.

## CLI

The default CLI stays headless, without Slint or CPAL. Enable native acquisition
explicitly; codec/processing remain the application's existing defaults:

```powershell
cargo build -p audiokit-test --release --locked --features native-cpal
.\target\release\audiokit-test.exe devices --json
.\target\release\audiokit-test.exe record --config .\configs\file-processing.json --duration-ms 10000 --capture-queue-ms 200 --stall-timeout-ms 2000 --out-dir .\target\mic-001 --retain-input
.\target\release\audiokit-test.exe analyze --bundle .\target\mic-001 --json
.\target\release\audiokit-test.exe replay --bundle .\target\mic-001 --out-dir .\target\mic-replay-001 --quiet
.\target\release\audiokit-test.exe compare --baseline .\target\mic-001 --candidate .\target\mic-replay-001 --json
```

Use a new output directory every time; its parent must exist. Omit `--input-device`
for system default, or use an exact local input ID from `devices`. Device IDs/names
are not exported. CLI defaults: 10000 ms material, 200 ms queue, 2000 ms valid-frame
stall timeout. Bounds: material 100..=60000 ms, queue 20..=1000 ms before platform
power-of-two capacity rounding, stall timeout 250..=10000 ms. Memory/file budgets
also apply; a high-rate/stereo request may exceed them. GUI uses the shared native
defaults and permits integer seconds. Options/format are recorded separately from
DSP `RunConfig`, because replay does not acquire a microphone.

Stdout uses stable JSON keys; progress is on stderr. Native features missing is
capability exit 3, invalid arguments 2, acquisition failure before material 4,
cancel 130. A valid completed DSP result with failed acquisition checks exits 1.
Ctrl+C cancels both phases, not "finish and process". If no material exists, no
bundle is claimed. With material, stopped/stalled/error runs preserve evidence;
offline processing may complete but acquisition health remains failed.

## Evidence and Reproduction

`diagnostics.json.material_capture` is optional, with its own schema version 1:

- Actual native format and acquisition options, terminal reason, material frame
  count, accepted frames and explicit inserted gap frames.
- Cursor rejects, discontinuity/timestamp-error boundaries, raw first/last native
  timestamps, callback/drop/xrun/error counters and queue high-water mark.
- Maximum callback-handoff-to-worker-read age in the port's monotonic host origin.
  It is queue/handoff age, NOT acoustic capture delay or an AEC alignment offset.
- Queued frames intentionally excluded at the recording endpoint, not missing
  media. Whole-port drop counters may include callback activity around that cut.

Missing native cursor positions become bounded zeros, never disappear from time.
Backward/repeated cursors are rejected and counted. Invalid timestamps are not
treated as zero latency. Even diagnostic metadata must obey format/frame accounting
and its scoped `material_capture_health` check when a bundle is loaded. A successful
health check only covers received cursor continuity and reported native faults,
not subjective quality, sample-clock accuracy or physical E2E delay. Timestamp
error boundaries stay visible but are not alone an audio continuity failure.

Native evidence is hashed as part of diagnostics and preserved by export.
`analyze` includes the original acquisition counters and scoped limitations;
it never opens devices to validate the recorded run again. Old
schema-1 bundles without this optional field remain readable; older binaries that
reject unknown fields need updating to read native-material bundles. No claim of
forward compatibility is made. Replayed bundles deliberately omit material capture
and its original hardware health check: compare can match config/plan/WAV while
`same_checks` is false. Build/backend changes remain visible in replay provenance.

Original audio retention defaults off. Without authorized `input.wav`, exact
offline signal reproduction requires the original hash-matching material, which
is not otherwise saved by this tool. Processed WAV always contains potentially
sensitive audio. Native callbacks only copy into the bounded production SPSC;
allocation, WAV encoding, DSP, JSON and artifact I/O occur on the owned worker.
No worker is detached. Raw material remains in memory unless retention is enabled.

Windows native integration is compiled; cursor/gap/budget/export/tamper/replay
tests are offline. Actual microphone, native cancellation, permissions, hotplug
and macOS/Linux capture still require hardware verification. Live capture DSP,
aligned playback reference/AEC, wall-clock timeline tracing and the A6 client
bridge remain separate work, not silently covered by this material tool.
