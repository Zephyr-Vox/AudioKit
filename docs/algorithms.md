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
Lookahead must cover attack. Numeric sample clamping is a last safeguard and
its use/overshoot is counted independently of ordinary attenuation.

Independent reconstruction found about 0.491 dB overshoot in the old short FIR
on a phase-shifted, 0.45 cycles/sample burst. The production detector now uses
four fractional phases of a 128-tap Hann-windowed sinc, normalized for unit DC
gain. It reconstructs 63..63.75 frames behind ingress and attributes the maximum
to frame 64. Effective lookahead is `max(ceil(rate * requested_ms / 1000), 64)`.
At 48 kHz/3 ms this remains 144 frames; at 8 kHz it is 64 frames (8 ms).
Source and master delays add; drain uses actual delays and retains detector tails.

Gain modulation and finite fractional-phase sampling do not commute with ideal
reconstruction. The configurable `reconstruction_headroom_db` (default 0.2 dB,
0..=1) sets `detector_ceiling = 10^((ceiling_dbfs - headroom) / 20)` while the
sample safety ceiling remains unchanged. Old JSON without the new field receives
that explicit default. A test-only 16x/128-tap Blackman-sinc reconstruction uses
different coefficients, window and phase grid. All 72 common-rate/layout/frequency/
phase cases, with burst edges and complete EOF extension, pass a 0.1 dB check,
stricter than the SPEC's 0.2 dB gate. Setting headroom to zero is supported but
does not inherit the default-profile acceptance result. No arbitrary-signal or
near-Nyquist universal bound is asserted by this finite corpus.

Mirrored history eliminates per-tap modulo; independent accumulation lanes remove
one long serial dependency chain. Source protection remains mono for voice until
bus expansion. Release worker probes report p50/p95/p99/max and deadline fraction,
including signal analysis, but excluding codec/device/transport/artifact I/O.
The default 32-source, 48 kHz policy leaves substantially more headroom than 64
sources on this machine; 96 kHz needs a smaller host-configured budget. Source
admission is not proof of a machine's realtime CPU capability. Allocated worker
buffers and attack-window scans must never run in native sample callbacks.

Per-slot linked sample peaks and target gains are cached. With ring length
`N = lookahead_frames + 1`, each ingress frame changes raw slot `cursor - 1`
(wrapped) and reconstructed slot `raw_slot - 64` (wrapped). Both targets are
refreshed after their respective writes; the other slots retain exactly the
original target. The raw write must include that slot's existing reconstructed
peak, even when it belongs to earlier input. These slots are distinct because
`N >= 65`. A reset clears peaks to zero and targets to unity; reconfiguration
rebuilds all state at the explicit stream boundary.

The attack window is traversed as tail/head contiguous slices, in the same
distance order as the original circular scan. Every peak still imposes its own
deadline `gain + (target - gain) / distance` for positive distance, or immediate
target gain at distance zero; a single minimum and its distance would miss nearer
peaks. No reciprocals, gain equations, FIR coefficients or
quality parameters were changed. Cache memory is `8 * N` bytes per limiter,
independent of channel count (1,160 bytes by default; at most 30,728 bytes).
See `limiter-cache-optimization.md` for bitwise reference tests and Release CPU
evidence. Production admission remains 32, not a claim of 64-source realtime.

The 2026-10-02 release probe used a Ryzen 9 7945HX (16 cores/32 logical CPUs),
Windows x86_64 and Rust 1.98.1. Each row warmed 16 blocks, then measured 100 blocks
with mono 48 kHz inputs and stereo output. This is a short worker throughput probe,
not an OS scheduler/device deadline certification or end-to-end latency measurement.

| Output Rate | Sources | CPU p99 per 20 ms Demand | Max |
|---|---|---|---|
| 48 kHz | 8 | 3.10 ms | 3.25 ms |
| 48 kHz | 32 | 9.30 ms | 9.41 ms |
| 48 kHz | 64 | 18.69 ms | 18.99 ms |
| 96 kHz | 8 | 8.78 ms | 9.35 ms |
| 96 kHz | 32 | 29.63 ms | 29.80 ms |
| 96 kHz | 64 | 53.80 ms | 53.87 ms |

The JSON under `target/validation/render-budget.json` retains all measured rates.
The 96 kHz/32-source profile fails the 20 ms worker budget on this machine; a host
must lower admission or choose a suitable render rate. Codec and device workloads
need their own additional headroom before claiming production realtime operation.

## Signal Measurements

Q15 magnitudes retain values above unity; integer square root keeps RMS reporting
stable. Boundary deltas compare the same channel across blocks and reset after
format changes, threshold changes or explicit gaps. A candidate can be normal
high-frequency music. Full-scale samples, candidate deltas and safety clamps
are distinct measurements, none by itself proves audible distortion. True-peak
filter history crosses blocks; a caller measuring EOF must include the DSP tail.

## Shared Graph Clocks and Activity

Capture keeps device callback, 10 ms processor/filter quantum and negotiated codec
ptime independent. Continuous resampling retains natural variable output, input
remainders and filter delay. EOF is the only zero padding point; filter/APM/codec
padding is accounted separately. The legacy CLI i16 packet adapter intentionally
retains its old packet normalization until the production graph switch.

One encoded receive startup window precedes demand-driven decoding. A common
render frame cursor advances only for actual device demand. Wrapping sequence
distances below half the 16-bit space are forward; bounded startup reorder may
move the left edge backwards, while a large jump clears source queues and DSP.
FEC uses only the immediately following slot and leaves its normal packet intact;
otherwise PLC advances exactly one known missing slot. Decoder shape/finiteness
failure is counted, prediction/source DSP reset, and one explicit zero slot used.
Capacity is preflighted before new PCM mutates source filters. New keys are refused
at admission limits; the decoded FIFO must cover maximum demand plus ptime and
30 ms filter/rate headroom. Epoch replacement, explicit retirement, drain and abort
clear the appropriate state. Replacements/recovery retain host target gain/mute
policy but restart the fade from silence. Clock recovery also resets old master
delayed PCM. Final rate-quantum padding is reported separately from underruns.

Activity uses per-frame EWMA power, separate attack/release time constants,
enter/exit dBFS hysteresis and hangover. It is an energy detector, not speech VAD.
Only active unmuted sources affect `1/sqrt(N)` normalization; voice and desktop use
independent buses and no automatic ducking. The square-root assumption is about
uncorrelated expected power, not a bound on correlated peaks; both limiter stages
remain necessary. Bus coefficient smoothing defaults to 5 ms, gain fade to 50 ms.

Queue feedback is `ppm = gain_ppm_per_ms * filtered(target_ms - queue_ms)`, with
elapsed-time smoothing, dead band, slew and cap. Recovery freezes/reset feedback.
Source/device rate estimators compare monotonic sample positions in one host-time
domain over configured windows. Unexpected arrival error over 100 ms resets a
window; network arrival time remains estimated, never a measured remote clock.
The linked-channel source ratio is `(1 + device_ppm/1e6)/(1 + source_ppm/1e6)`;
Rubato Slip spreads occasional insert/remove operations rather than hard cuts.
Correction precedes both limiters. Missing-slot disturbances freeze adaptation;
host pauses/rebuffering require explicit recovery. Saturation and unavailable
estimates are observable, and unknown time is not serialized as zero latency.

## Shared Capture Frontend and Offline Runner

`CapturePcmGraph` owns the same channel map, continuous filter and optional Sonora
state previously owned directly by `CaptureGraph`. The packet graph delegates
to it, then exclusively packetizes/encodes returned PCM. Its PCM-only consumer
never invokes a dummy codec: EOF pads an enabled 10 ms processor quantum but not
an absent Opus packet. Original preflight input errors leave history untouched;
advanced processor/filter failures are terminal. Non-finite processor output is
rejected before entering downstream buffers. Tests independently verify chunk
invariance, processor padding and exact same-rate bypass; existing graph/codec
regressions preserve capture packet behavior.

The file-roundtrip runner paces capture ingress and device demand on a virtual
10 ms host clock. It does not admit an entire song into the encoded jitter queue
at once or issue ordinary demand forever after EOF. Explicit receive drain keeps
real queued packets and finite tails without inventing PLC. Its timelines are
models, not OS presentation timestamps. Sample-range clocks and host-time clocks
are distinct fields. CPU is measured only around worker calls, never by adding
WAV I/O time to packet-ready latency. Frame-based backend delay and configured
startup/ptime stay separate; full signal-aligned delay remains unknown.

Observation storage is a capped prefix with independently retained aggregate
receive/render statistics. Missing observations carry lost ordinals/counts and
are not proof of healthy audio. Hash-validated source bytes, config and graph
plans support re-execution; metadata-only traces cannot reconstruct a waveform.
Bundle integrity is not producer authentication or an audibility classifier.

## Remaining Host Work

The production CLI still contains its legacy packet scheduler/device engine:
packet-presence normalization, multiple waits, pre-enqueue reference and old map
admission are not solved merely by building shared graphs. A6 must replace them.
The CLI already delegates shared codecs/resampling/DSP and safe Windows activation.
Reference-to-capture time mapping, native hotplug, duplex AEC calibration and full
hardware drift runs remain separate verification gates. Basic offline report
persistence and bounded snapshots are implemented. Real-time artifact workers,
event correlation, independent output-oracle scenarios and GUI microphone/duplex
scenarios remain A5 work. The initial offline Slint workbench delegates shared
runner execution/export; its optional audition is not physical E2E certification.
