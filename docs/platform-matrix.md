# Extraction Validation Matrix

| Component | Windows | macOS | Linux |
|---|---|---|---|
| Core formats/clock/blocks | compiled, offline-tested | not repeated | not repeated |
| Shared limiter/gain/measure | compiled, offline-tested | not repeated | not repeated |
| Sonora backend | compiled, offline-tested | not repeated | not repeated |
| Core/backend device access | absent | absent | absent |
| Native production adapter | still in CLIClient | still in CLIClient | still in CLIClient |
| CLI/Slint shared test app | not yet extracted | not yet extracted | not yet extracted |

The prior accepted Windows/macOS listening results describe the frozen client
baseline, not hardware verification of a new AudioKit device engine. This milestone
does not open a microphone or speaker. Core and Sonora need no Opus C toolchain;
the CLI still uses libopus through its old codec adapter.
