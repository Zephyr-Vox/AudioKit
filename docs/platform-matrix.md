# Extraction Validation Matrix

| Component | Windows | macOS | Linux |
|---|---|---|---|
| Core formats/clock/blocks | compiled, offline-tested | not repeated | not repeated |
| Shared limiter/gain/measure | compiled, offline-tested | not repeated | not repeated |
| Sonora backend | compiled, offline-tested | not repeated | not repeated |
| Shared capture/receive/render graphs | compiled, deterministic virtual tests | not repeated | not repeated |
| CPAL ports and actual-consumed reference | compiled, virtual conversion/allocation tests; hardware not tested | not repeated | not repeated |
| WASAPI process loopback | compiled, activation lifetime/flags tests; hardware not tested | unavailable | unavailable |
| Native production host adapter | still in CLIClient, A6 pending | still in CLIClient | still in CLIClient |
| Shared testkit/headless CLI | compiled, offline-tested | not repeated | not repeated |
| Optional Slint offline workbench | compiled, software-window controls/layout/runner tests | not repeated | not repeated |
| GUI native WAV audition | compiled; hardware not tested | not repeated | not repeated |
| Microphone material capture + offline DSP | compiled, synthetic cursor/bundle tests; hardware not tested | not repeated | not repeated |

The prior accepted Windows/macOS listening results describe the frozen client
baseline, not hardware verification of a new AudioKit device engine. This milestone
does not open a microphone or speaker during automated validation. GUI playback
opens a device only after the user explicitly presses Play; recording requires
Record or the explicit CLI `record` command. Recording stops before offline DSP;
it is not a live duplex/reference test. Core and Sonora need no Opus C toolchain;
the optional codec backend uses libopus. CPAL raw clock and Windows QPC metadata
are retained but are not interchangeable with host presentation timestamps.

Windows endpoint loopback is exposed through an output-device CPAL capture port;
it is not a cross-platform desktop capture promise. Linux ALSA/PipeWire and macOS
system/process capture capability, permissions, hotplug and cancellation still need
platform-specific verification. AEC processing exists, but duplex reference timing
must be calibrated on the actual devices before a host enables it.
