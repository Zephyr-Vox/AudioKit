# Algorithms and Current Limits

## Sample and Packet Clocks

`SampleFrames` counts one instant across all channels. Interleaved sample count
is `frames * channels`, checked for overflow. `AudioBlock` accepts arbitrary
complete worker blocks. Its source/epoch and `[start, end)` range belong to an
explicit clock domain. Missing timestamps remain unavailable, not zero-delay
measurements. Opus packet duration is selected once for a session; it does not
define a device callback or the Sonora 10 ms processing quantum.

## Linked Gain

The accepted gain fade is 50 ms. Each frame advances a linear ramp by
`(target - current) / ceil(rate * fade_ms / 1000)` and applies that gain to all
channels. Retargeting starts at the instantaneous gain. Zero-filled gap blocks
must still advance the ramp. Limits and defaults are defined only in `mix`.

## Lookahead Limiter

The source ceiling is -3 dBFS; the master defaults are -1 dBFS ceiling, 3 ms
lookahead, 1 ms attack and 100 ms release. Linear ceiling is `10^(dBFS / 20)`.
Channels share one attenuation envelope to preserve their relative amplitude.
The detector is the retained four-phase, 12-tap FIR with six-frame group delay.
Lookahead must cover attack. Numeric sample clamping is a last safeguard and
its use/overshoot is counted independently of ordinary attenuation.

This extraction preserves the accepted limiter behavior, not a new guarantee.
Existing true-peak tests use the same FIR as production and therefore cannot
independently establish reconstruction accuracy. A separate offline oracle and
release-mode deadline benchmarks remain required. Output-buffer allocation and
attack-window scans also mean this worker API must not run inside a native
device callback.

## Signal Measurements

Q15 magnitudes retain values above unity; integer square root keeps RMS reporting
stable. Boundary deltas compare the same channel across blocks and reset after
format changes, threshold changes or explicit gaps. A candidate can be normal
high-frequency music. Full-scale samples, candidate deltas and safety clamps
are distinct measurements, none by itself proves audible distortion. True-peak
filter history crosses blocks; a caller measuring EOF must include the DSP tail.

## Remaining Engine Corrections

The host still contains multiple receive/prebuffer waits, a sample-count-based
drift feedback gain, packet-presence source normalization, pre-enqueue AEC
reference and unbounded source-state admission. These are documented migration
risks, not fixed by moving the limiter. They will be corrected at their owning
engine boundaries with latency accounting and fault-injection tests. Native
timestamp/flag handling and callback allocation checks are separate gates.
