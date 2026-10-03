# Source DSP Diagnosis (2026-10-03)

This report records the pre-optimization diagnosis. The subsequent equivalent
target-cache change and new measurements are in `limiter-cache-optimization.md`;
historical timings below are not measurements of the optimized implementation.

## Scope and Reproduction

This is a **render worker CPU diagnosis**, not a diagnosis of microphone noise,
network loss, native device clocks or the earlier client interruption symptom.
The production limiter, gain, activity and resampling algorithms/parameters have
not been changed. Production source admission remains 32.

Measured on Windows x86_64, AMD Ryzen 9 7945HX, Release build. CPU frequency,
power policy and competing OS work were not controlled; results are observations,
not a realtime hardware certification. No audio device, network or server was used.
Instrumentation build was based on `2879cc99de8e8ae7bb4ffb81166b259553717e5d`
with diagnostic-only changes; manifest source digest:
`f26ae5d10c881cfe0a1176121cd10d994e450dab18ea59d0117cafcf66c20bb9`.

Input was the existing local three-second float32 stereo 44.1 kHz instrumental
excerpt. Music and generated WAV/JSON bundles are not distributed in Git.
CapturePcmGraph performs channel/rate adaptation without APM or a codec; replicated
PCM feeds 32/64 independent production source states, then activity normalization
and master protection. Voice stays mono through its source limiter, desktop is
stereo; output is 48 kHz stereo. Default source gain 1, source ceiling -3 dBFS,
master ceiling -1 dBFS, limiter lookahead 3 ms, attack 1 ms, release 100 ms and
reconstruction headroom 0.2 dB are retained.

Each case has 300 ordinary 10 ms demands plus a separately excluded finite drain.
Three instrumented repeats per kind and three unprofiled repeats produced 24
diagnostic bundles. Sweeps were serial, with no simultaneous builds during CPU
measurement. All 24 retained scoped signal/queue checks passed; these checks do
**not** certify worker deadlines. No trace events were dropped. All 12 paired
profiled/unprofiled output WAVs were byte-identical; voice WAVs also match the
previous accepted 32/64-source mix-stress baseline.

Artifacts are under `target/validation/a5-20261003/`:
`source-profile-{voice,desktop}-0{1,2,3}` and
`source-baseline-{voice,desktop}-0{1,2,3}`. `case-000` is 32 sources, `case-001`
is 64. Each case includes config, diagnostics, trace, WAV and manifest with build
identity; original input is not retained. The diagnostic JSON retains all calls,
whereas `analyze`'s evidence timeline remains bounded to 64 flagged intervals.

```powershell
cargo build -p audiokit-test --release --offline --locked
.\target\release\audiokit-test.exe sweep --config configs\mix-profile.json --matrix configs\source-profile-matrix.json --input "INPUT-3s.wav" --out-dir target\source-voice-001 --quiet
.\target\release\audiokit-test.exe sweep --config configs\mix-profile-desktop.json --matrix configs\source-profile-matrix.json --input "INPUT-3s.wav" --out-dir target\source-desktop-001 --quiet
.\target\release\audiokit-test.exe analyze --bundle target\source-desktop-001\case-001 --json
```

For a disabled-timing desktop control, copy its configuration with
`execution_profiling: false`; voice can use `configs/mix-stress.json` with the same
matrix. Use new output directories and keep the same original WAV hash. Ptime and
bitrate fields are validated but not exercised in this codec-free scenario.

## Measured Attribution

Ranges below are **mean ms per ordinary call, summed across all registered
sources**, across three instrumented repeats. Columns under source stages are
disjoint children of the source-processing parent, not additional CPU time.
Allocation/validation inside a region is included; allocator cost is not isolated.
Parent-minus-children includes timing/bookkeeping gaps, not a measured allocation
or scheduling component. Decoder and source PCM admission are outside this parent.

| Kind / Sources | Worker | Source Parent | Queue Read | Gain | Activity | Source Limiter | Channel Map |
|---|---:|---:|---:|---:|---:|---:|---:|
| Voice / 32 | 3.885-4.090 | 3.466-3.645 | 0.020-0.020 | 0.012-0.013 | 0.248-0.257 | 3.149-3.316 | 0.032-0.034 |
| Voice / 64 | 7.490-7.593 | 6.985-7.072 | 0.039-0.040 | 0.025-0.026 | 0.498-0.501 | 6.343-6.433 | 0.065-0.066 |
| Desktop / 32 | 5.757-5.848 | 5.256-5.333 | 0.037-0.038 | 0.018-0.019 | 0.250-0.257 | 4.935-5.003 | 0.011-0.012 |
| Desktop / 64 | 11.150-11.348 | 10.478-10.663 | 0.076-0.077 | 0.036-0.036 | 0.504-0.506 | 9.828-10.015 | 0.022-0.022 |

Rounded ranges are for orientation; raw JSON is authoritative. Source limiter is
approximately 91% of the voice parent and 94% of the desktop parent, or roughly
81-85% / 86-89% of the whole ordinary worker call. It is the primary measured
optimization target. Activity is a smaller secondary region. Queue/gain/channel
mapping optimization alone cannot explain or remove the measured limiter cost.

## Disabled-Timing Controls

| Kind / Sources | Worker Mean ms (3 Repeats) | Worker Max ms (3 Repeats) | Calls >10 ms per 300 (3 Repeats) |
|---|---|---|---|
| Voice / 32 | 3.929 / 3.925 / 3.940 | 4.480 / 4.481 / 4.442 | 0 / 0 / 0 |
| Voice / 64 | 7.759 / 8.093 / 7.876 | 12.092 / 11.461 / 12.771 | 10 / 12 / 8 |
| Desktop / 32 | 5.749 / 5.989 / 5.846 | 6.336 / 6.558 / 7.237 | 0 / 0 / 0 |
| Desktop / 64 | 12.423 / 11.941 / 12.658 | 18.121 / 18.531 / 18.285 | 300 / 300 / 300 |

Instrumented 64-voice repeats recorded 0/1/0 overruns; 64-desktop repeats recorded
300/300/300. Disabled-timing controls establish that the desktop overrun is not
created solely by instrumentation, and voice 64 still lacks robust deadline
margin. Some profiled repeats were faster than controls. These serial short runs
cannot isolate the cost of clock reads from CPU/OS variability; do not describe
the timing overhead as zero or negative, or claim that adding instrumentation
improved DSP performance. Percentiles are the documented histogram bucket upper
bounds, not exact timings.

## Code-Level Interpretation and Next Target

`MasterLimiter::process_interleaved` performs its detector and envelope work even
when the signal is below ceiling or the source is silent. It allocates one output
buffer per call, performs linked-channel true-peak detection, then scans every
candidate in the attack window for every input frame. At default 48 kHz/1 ms,
that scan is 49 candidates per frame. The detector evaluates three fractional
phases with 128 taps each, plus the integer phase.

For 64 mono sources and a 480-frame demand, this corresponds to 11,796,480 FIR
tap products and 1,505,280 attack-window candidate visits per ordinary call.
Stereo doubles detector tap work and increases channel peak reduction work.
These are workload counts derived from code, **not** measured internal CPU
attribution. The new timer measures the complete limiter call; it cannot say
what percentage belongs to FIR, window scanning, allocation or metric updates.
Additional per-sample timers were not added because they would perturb this hot
loop; targeted profiling is still needed before choosing a major kernel change.

The next behavior-preserving investigation should target repeated peak/target
calculation and circular-window indexing in the source limiter, then its detector
kernel if necessary. Cacheable values must remain bit-equivalent. Peak deadlines
cannot be replaced by one window minimum plus its distance: a nearer, less extreme
peak can require earlier attenuation. Tail alignment, linked-channel gain,
per-source state, true-peak reconstruction and independent-oracle regressions
remain acceptance constraints. Buffers may be reused, but this report provides
no evidence that allocator overhead is the dominant cost.

No DSP optimization, accepted parameter change or production capacity increase
has been made in this diagnostic slice. No listening calibration is needed for
the instrumentation because PCM and signal/limiter decisions are unchanged.
