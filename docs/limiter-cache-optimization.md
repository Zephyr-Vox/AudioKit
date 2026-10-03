# Limiter Target Cache (2026-10-03)

## Scope and Invariants

This optimizes the shared source/master limiter, not its audio behavior. The
128-tap four-phase detector, sample/true-peak protection, linked-channel gain,
lookahead, individual peak deadlines, attack/release equations and configurable
headroom are unchanged. No native callback, codec, transport or queue policy was
changed. Production source admission remains 32.

Only two ring slots change per input frame: the raw write and the reconstructed
peak write, offset by 64 frames. Cache their linked sample peaks and targets;
refresh both using the original f32 max/multiply/divide order. Unwritten slots
retain their target, including the reconstructed peak already present when raw
samples overwrite a slot. Replace circular indexing in the attack scan with two
contiguous slices, preserving every candidate's distance and evaluation order.
A window minimum alone cannot replace these independent attenuation deadlines.

The cache adds two f32 arrays of `lookahead_frames + 1` entries: 1,160 bytes per
limiter at default 48 kHz, at most 30,728 bytes at 384 kHz/10 ms. Construction and
reconfiguration allocate them; reset clears them. No per-frame allocations were
added, but the existing returned block still allocates on the DSP worker.

## Equivalence and Protection

`crates/audiokit/src/limiter/reference_tests.rs` retains the old scalar scan,
including per-visit raw peak/target calculation and modulo indexing. It does not
use the optimized target helper or caches. PCM and gain are compared by f32 bit
pattern (including signed zero), block metrics and cursor exactly. Tests cover
8/44.1/48/96/384 kHz, mono/stereo/four-channel, multiple wraps, 1/7/127/480-frame
blocks, empty calls, impulses, random overdrive, silence, subnormals and EOF tails.
Configs exercise the minimum FIR delay, a full-ring attack and maximum lookahead,
both ceiling/headroom extremes, and release/attack bounds. Additional cases cover
1 Hz, 255 channels, maximum finite samples, clone, reset, valid reconfiguration
and invalid input/config without state mutation.

The frozen scan shares only the unchanged production detector and setup; it is
not an independent physical reconstruction oracle. The separate integration
test uses 16x/128-tap Blackman-sinc reconstruction, covering 72 tone/transient/
layout/rate/phase cases with complete tail extension and a 0.1 dB tolerance.
That corpus remains passing; it is not a universal near-Nyquist protection bound.

## Release Observations

Windows x86_64, AMD Ryzen 9 7945HX. Same authorized local three-second float32
stereo 44.1 kHz instrumental excerpt as `source-dsp-diagnosis.md`; PCM capture
adaptation without APM/codec, independent 32/64 mono voice or stereo desktop
sources, 48 kHz stereo output, unchanged default limiter/gain/activity parameters.
Each case measures 300 ordinary 10 ms worker calls including source admission.
Drain, artifact I/O, tracing and native scheduling are excluded. CPU frequency,
power settings and OS load are uncontrolled; sweeps ran serially without builds.

One fresh pre-change sweep per stream was followed by three unprofiled optimized
sweeps per stream. The previous three-repeat baseline in the diagnosis report is
also retained. Means/maxima below are observed timings, not deadline certification.
Percentiles are histogram bucket upper bounds, not exact order statistics.

| Kind / Sources | Before Mean ms | After Means ms (3) | Before Max ms | After Maxima ms (3) | Before >10 ms /300 | After >10 ms /300 (3) |
|---|---:|---|---:|---|---:|---|
| Voice / 32 | 3.999 | 3.080 / 3.095 / 3.108 | 4.338 | 3.598 / 4.203 / 3.392 | 0 | 0 / 0 / 0 |
| Voice / 64 | 7.816 | 5.952 / 5.899 / 5.919 | 9.889 | 6.804 / 6.615 / 6.581 | 0 | 0 / 0 / 0 |
| Desktop / 32 | 5.953 | 4.575 / 4.660 / 4.605 | 7.216 | 5.482 / 6.445 / 5.345 | 0 | 0 / 0 / 0 |
| Desktop / 64 | 12.197 | 9.257 / 9.192 / 9.302 | 17.563 | 11.778 / 10.955 / 11.681 | 300 | 13 / 6 / 12 |

Observed mean reduction is about 22-25% relative to the fresh baseline, consistent
with the earlier baseline ranges. It is a whole-worker observation, not isolated
cache-cycle attribution. The three after p99 upper bounds (ms) are voice32
3.146/3.670/3.408, voice64 6.816/6.291/6.291, desktop32 5.243/5.243/5.243,
desktop64 12.583/10.486/11.534. Desktop64 still exceeds its ordinary 10 ms budget;
even voice64's short clean runs do not accept a larger production capacity.
These timings are not end-to-end audio latency or the earlier interruption root
cause. Device/codec/host work needs separate CPU margin and hardware verification.

One additional profiled sweep per stream retained byte-identical WAVs. Mean source
limiter time was 2.487/4.976 ms for voice32/64 and 3.864/8.051 ms for desktop32/64;
it remains the dominant source region. Parent source means were 2.811/5.628 and
4.185/8.717 ms respectively. Child timings belong to that parent and must not be
added again. No claim is made about FIR versus scan versus allocator cost inside
the limiter, or isolated instrumentation overhead from serial runs.

All 16 optimized outputs (12 disabled, four profiled) are byte-identical to their
corresponding fresh pre-change WAVs. All retained signal/queue checks pass and
trace drop counts are zero. These checks deliberately do not certify CPU budgets.
No new human listening calibration is required for this equivalent slice.

## Evidence and Reproduction

Local ignored artifacts under `target/validation/a5-20261003/`:
`limiter-cache-before-{voice,desktop}`, `limiter-cache-after-{voice,desktop}-0{1,2,3}`
and `limiter-cache-profile-after-{voice,desktop}`. Case 000 is 32 sources, case 001
is 64. Manifests retain revision and source digest; music/WAV/trace are not in Git.
Before build revision: `cedefca23fd159e027e455856f1ca7a7326e99c0`, digest
`869d03b322a1f5e3e23bf30eda71e29d80f5984ba323315f934b696c0c637b50`.
After measurements used that parent plus this working limiter change, digest
`35438bfa1494fbd13c3ab5812fd16c9fd0386028e57375767045aea1b6f3c5f2`.
Subsequent documentation/test changes do not alter the measured production DSP.
Original input is not bundled; reproduction requires the original matching WAV.

Validation: 129 workspace unit/integration tests plus one doctest, 30 headless
unit/integration tests plus one doctest, and 27 minimal-core tests pass. Full,
headless and minimal-core strict Clippy, warning-as-error workspace Rustdoc and
format checks pass. All 20 retained before/after bundles pass integrity analysis.
Build-mode tests were serial to avoid replacing the shared debug CLI executable.

```powershell
cargo test -p audiokit --release --offline --locked reference_tests
cargo test -p audiokit --release --offline --locked --test true_peak_oracle
cargo build -p audiokit-test --release --offline --locked
.\target\release\audiokit-test.exe sweep --config configs\mix-stress.json --matrix configs\source-profile-matrix.json --input "INPUT-3s.wav" --out-dir target\cache-voice-001 --quiet
.\target\release\audiokit-test.exe sweep --config configs\mix-profile-desktop.json --matrix configs\source-profile-matrix.json --input "INPUT-3s.wav" --out-dir target\cache-desktop-profile-001 --quiet
.\target\release\audiokit-test.exe analyze --bundle target\cache-voice-001\case-001 --json
```

Use fresh output paths. An unprofiled desktop comparison uses the desktop config
with `execution_profiling: false`; do not compare profiled times as isolated
overhead measurements. Native tests, Slint GUI and the SDK/production host bridge
remain separate work; this optimization does not implement them.
