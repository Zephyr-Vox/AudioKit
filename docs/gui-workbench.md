# GUI Workbench

The `audiokit-test` binary has two frontends for the same synchronous testkit
runner. The default CLI remains headless. The optional `gui` feature pins Slint
runtime/build 1.18.1 with a software renderer and native file dialogs. Rust 1.92
is the workspace minimum. Opus still requires its C toolchain when enabled.

## Launch

From the AudioKit repository in PowerShell:

```powershell
cargo run -p audiokit-test --release --features gui -- --gui
```

Simplified Chinese is the default. Select English in the language menu or pass
`--language en`. Language changes affect presentation only, even while a job is
running: config values, node IDs, diagnostic keys, bundle hashes and DSP state do
not change. Slint bundles the gettext catalog from
`crates/audiokit-test/translations/zh_CN/LC_MESSAGES/audiokit-test.po`; no external
catalog installation is needed. Additional languages need their own catalog and
an explicit locale/menu mapping. Engineering JSON/check details, native device
names, paths and original backend error text retain their stable original form.

Optional starting paths/preset do not start processing or open audio devices:

```powershell
cargo run -p audiokit-test --release --features gui -- --gui --config .\preset.json --input .\input.wav --out-dir .\target\workbench
```

Choose Capture PCM, Opus roundtrip, Multi-source render or Packet replay, then
Voice/mono or Desktop/stereo. These select legal production subchains, not
arbitrary edges or a second DSP implementation. WAV accepts PCM16/24/32 and
float32 mono/stereo; packet replay uses the existing version-1 packet recording.
Record explicitly opens the chosen microphone for 1..=60 integer seconds (default
10), closes it, then runs the selected production subchain. Finish recording
processes earlier material; Cancel cancels capture AND offline processing. The
refresh icon discovers input/output devices without selecting a new non-default
device. No device is opened at launch. Packet replay cannot take microphone PCM.
See `microphone-material.md` for native counters, consent and replay boundaries.
Aligned AEC reference, live monitoring/server E2E and sweep UI remain unimplemented.

## Parameters and Results

Capture controls expose NS level, high-pass, AGC2, adaptive gain and capture
resampler quality. Codec controls expose bitrate, session ptime and startup jitter.
Render controls expose output rate/quality, admission, gain fade and stress source
count/gain. Protection controls expose source ceiling and master ceiling,
lookahead, attack, release and reconstruction headroom. Inactive nodes are disabled.
All defaults/validation come from the Rust model; invisible advanced fields survive
form edits. Changing scenario/profile resets to its shared defaults.

Advanced JSON uses the CLI's complete `RunConfig`. Apply JSON replaces the config;
Sync from controls projects current widgets back into JSON (discarding unapplied
JSON edits). Save preset saves the current controls, not unapplied JSON text.
Parameter changes never mutate a running graph: Run owns an immutable snapshot.

Resources exposes `max_input_bytes` and `max_pcm_samples` directly. The default
sample cap is 16777216 TOTAL scalar samples across all channels, about 174.8 s of
48 kHz stereo, not 16777216 frames. The default input cap is 67108864 bytes.
For a longer music WAV, select the desired profile first, then explicitly increase
these budgets (e.g. 268435456 bytes and 67108864 samples); the hard limits remain
256 MiB of input and 67108864 input OR output samples, and graph tails count toward
output. Budget errors identify required samples, duration/rate/channels and the
configured limit. No input is silently truncated or downsampled to pass validation.
The runner currently buffers raw input, decoded PCM and output, so peak memory can
substantially exceed one PCM buffer. Larger limits are opt-in, not memory estimates.
The same values can be edited in Advanced JSON; press Apply JSON before running.

Validate resolves actual input formats and constructs the production graph without
artifacts. Run creates a unique child of the selected output folder and never
overwrites a previous run. Completed does not mean audibly good: scoped checks,
partial status, unknown observations and trace drops remain visible. Metrics and
coverage are always from the last recorded result, not newly edited controls;
a failed validation does not destroy it. A new run clears stale result controls.

Checks, latency, bounded evidence, material capture and stage coverage are separate.
Unsupported counters say Not covered, not zero. Latency preserves unknown physical
E2E and domain/method information; nested execution regions are not additive.
The view omits histogram bins and bounds JSON display text to 64 KiB, while exports
retain full recorded data. Diagnostics are not an audio quality certificate.

## Export, Privacy and Reproduction

Load result validates fixed filenames, resource budgets and artifact SHA256 before
display. Export bundle copies only manifest-listed validated artifacts to a new
directory and writes its manifest last. Extra local files never enter the export.
Export WAV copies the exact float32 output bytes without another Opus encode.
Existing destinations are refused, even if a native save dialog offers overwrite.

Original input/payload retention defaults off. Imported presets/JSON force it off;
the visible Include original audio/payloads checkbox is fresh authorization for
that run. **Processed WAV is itself potentially sensitive audio and is always in
the run bundle.** Export is a local explicit action, not an automatic upload.
An exported metadata-only bundle still contains processed WAV; "metadata-only"
describes lack of original material for signal reproduction, not absence of audio.
Without retained material, replay needs the original input with the recorded hash.

Use the same binary's CLI to inspect or reproduce an exported bundle:

```powershell
.\target\release\audiokit-test.exe analyze --bundle .\exported-bundle --json
.\target\release\audiokit-test.exe replay --bundle .\exported-bundle --input .\input.wav --out-dir .\target\new-replay --quiet
.\target\release\audiokit-test.exe compare --baseline .\exported-bundle --candidate .\target\new-replay --json
```

## Audition and Ownership

Play is the only action that opens playback. It revalidates and decodes the exact
hash-checked output bytes, maps channels/resamples with `CapturePcmGraph`, and
feeds the selected CPAL port at its actual negotiated format. System default is
available without discovery; Refresh explicitly lists input/output devices. Preview
volume defaults to 0.2. Preview-only scaling/clamps and startup silence never
change the saved WAV or recorded pipeline latency. Physical presentation tail is
unknown. Native audition is compiled, but not hardware-accepted by offline tests.

UI thread owns Slint objects and lightweight parameter snapshots. One joined worker
owns graph/file/preview work. Progress is a single latest snapshot, not an unbounded
event queue. Cancel requests cooperative stop; a partial runner result is finalized
before reporting completion. Window close keeps the UI alive until its worker
finishes, while emergency event-loop exit still cancels and joins. No worker is
detached. File validation and graph preparation are bounded but not interruptible
inside every library call. UI polls completion every 50 ms.

Slint is a separately licensed optional dependency; its license is not replaced by
the workspace's MIT license. Review its distribution terms for the intended host.
Vendored play/refresh icons come from Lucide; their ISC notice is in `ui/icons/LICENSE`.

## Validation

The compiled software-window test exercises real controls, worker jobs, invalid
fields, imported consent, explicit retention, cancellation and close finalization.
For capture/roundtrip/render in both profiles, GUI output is byte-identical to
direct shared-runner output with matching effective config, plan and checks.
Headless CLI integration independently exercises the same runner and bundle format.
An actual CLI rerun of the GUI-exported capture fixture also matches input, config,
plan, checks and output WAV bytes; CLIClient compile-checks with Rust 1.92.
Screenshots at 1120x900, 720x900 and 720x700 cover parameters and scrolled results;
Chinese and English switching is tested against the same immutable run snapshot.
Synthetic native cursor gaps preserve missing frames, and material bundles retain
validated provenance through export; replay produces identical DSP output but does
not assert a new successful hardware capture.
no microphone, speaker or network is opened by these tests. Run:

```powershell
cargo test --workspace --all-features --locked
cargo test -p audiokit-test --no-default-features --locked
```

Run build modes serially: they share the CLI executable path. Local screenshots
and synthetic audio live under ignored `target/validation/gui`, never Git.
