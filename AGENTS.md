# Scrawl: Agent Instructions

Scrawl is the human-to-agent half of a feedback loop: the agent builds, the human looks, points, talks and draws, and the agent understands what was meant and acts. A capture (`⌥⇧S` on macOS, a cabled iPhone (mirror plus native video), or ScrawlKit's three-finger long press inside an iOS app) records the screen, voice, cursor, clicks and `⇧` ink, and Scrawl compiles it into a compact `.scrawl` bundle (`contact-sheet.png`, `frames/frame-XX.png`, `summary.md`, `scrawl.json`, `recording.mp4` or `recording.mov`, and `audio.wav`) in `~/Library/Application Support/scrawl/captures/`. The Mac app is Rust + GPUI with a Swift helper (`scrawl-helper`); ScrawlKit recordings are imported with `scrawl import` (`Sources/ScrawlKit`). See README for the iPhone, ScrawlKit and testing.

This is Isaac's fork of `kitlangton/scrawl`, adapting the hand-off for remote and web-based agent sessions. Implement and test Scrawl locally with native Codex agents. Murmur nonprod is reserved for changes to the `murmuration` and `back` repositories, in their corresponding workspaces.

## Principles

1. **The human's time is the scarce resource.** Starting (`⌥⇧S`), marking (`⇧`) and delivering (auto-routing or `⌥⇧V`) should feel instant and take as few steps as possible.
2. **Don't make the agent guess what Scrawl can capture.** When the OS, window server, Accessibility tree, browser page or host app knows which window, element, URL or session is on screen, record that fact instead of leaving the agent to infer it from pixels and words like "this part" or "here".
3. **Images stay visual and text carries meaning.** `contact-sheet.png` and `frames/*.png` show the screen, red ink, click ripples and each keyframe's number in a small white corner chip (no `#`, no time), with no rasterized captions or card chrome. `summary.md` and the clipboard carry the time-aligned transcript, interaction events and absolute file paths as plain text that pastes cleanly into a terminal or a GUI composer. Details are in `docs/keyframes-and-text.md`.
4. **Spend the agent's context where the signal is.** Cut keyframes only on real actions, marks or visual changes (never on speech pauses over a static screen), crop to the active region while keeping full frames on disk, and point the agent at `scrawl zoom` for a closer look around a moment.
5. **Inspect real bundles after every change.** A capture is the output; read it, don't infer it. Inspect the latest bundle in `~/Library/Application Support/scrawl/captures/`: both `summary.md` / `scrawl.json` and the visual `contact-sheet.png` / `frames/*.png`. Say when no bundle was available. Surface concrete output and UX improvements, such as noisy text, redundant frames, misaligned annotations, slow processing or clipboard friction.

## Build, test and verify

Implementation, builds and tests run locally. Keep this fork independent of the upstream author's private Macs, signing keys and release infrastructure.

- Keep `Cargo.toml` and `Cargo.lock` stable. Build with `cargo build --locked` and test with `cargo test --locked`; `cargo test` does not rebuild the app binary. The Mac build requires Xcode's Metal toolchain and macOS 26 SDK. The crate currently does not build on Linux.
- `.github/workflows/check.yml` also builds and tests macOS and Windows after pushes to `main`. Report the checks actually run and any missing native coverage.
- Clipboard changes need native read-back verification of both image and text. Preserve the user's clipboard while testing, never log its contents, and follow `/Users/i/Documents/GitHub/k/rules.md` for label-only clipboard notifications.
- **Defaults, not flags.** This fork ships the remote-session hand-off on by default rather than waiting behind an experiment for an upstream owner to enable it, and a flag that gates nothing is deleted, not pinned on. The mechanism stays for genuinely speculative work: `handoff::experiment("<name>")` reads `"experiments": {"<name>": true}` from `~/.config/scrawl/config.json` (a value other than true/false, `"yes"`/`"off"` or a number is skipped and logged, and a bad value for any config key drops only that key; tests see only `handoff::enable_experiment`, never the config file), and `scrawl reprocess` / `scrawl inspect` take `--experiment <name>` to try one on a bundle. Current experiments are listed in `docs/keyframes-and-text.md`.
- **Git:** work on this personal fork goes straight to `main`; no PR workflow. Commit subjects are at most four words with no body — the reasoning belongs in the code, the docs and the task report. `kitlangton/scrawl` is upstream to pull from, and where a fix that isn't part of the remote-session remodel belongs.
- **Feature map:** `site/src/features.json` lists every user-facing feature with its status (`stable@<version>`, `main`, `experiment:<flag>`, `branch:<name>`, `review:#<pr>`), platforms, shortcuts, config keys and docs. A change that adds, removes, renames or changes the status of a user-facing feature updates it in the same commit. A release moves its `main` entries to `stable@<version>` (`docs/release.md`). `cargo test feature_map` fails when a config key or experiment the code reads has no entry, or an entry names one that's gone. The site renders it at the unlinked `/map-tyyxsf5b/` (`MAP_DIR` in `site/scripts/feature-audit.ts`), with a status board and what looks inconsistent.
- For installation, adapt the upstream scripts to this Mac's signing setup rather than using the author's identity or remote hosts. Launch an installed app copy, not a long-lived daemon in `target/`: deleting that binary can invalidate macOS permissions. Report any signing or permission steps needed for a fork build.

## Safety and recovery

- Use recorded WAVs to test transcription and virtual displays for display-sensitive tests. Respect the user's active desktop and existing work; do not use the upstream author's personal machines.
- Never `tccutil reset` a real grant to test permissions — preview the permission panel instead (`docs/onboarding-and-settings.md`).
- Tests never post to a real OpenCode session: swap the outside steps through `IngestHooks` (ScrawlKit ingest) and `routing::Hooks` (dev-site routing).
- **A consumer's behavior is verified in the consumer.** Showing that both clipboard formats are available (the image and the text) does not show that a given composer takes both in one paste: Chromium's default rich-contenteditable paste chooses one representation (HTML, else image markup, else plain text) and never concatenates formats, and a web composer's own paste handler decides the rest. Verify or fix that in the target app, and never paper a consumer bug over with synthetic keystrokes or automatic pastes.
- **Delivery recovery:** Read the original `timeline.json` before re-sending an imported capture. Its top-level `session` is the configured destination; `context.session` only names the session visible in the recorded app. Never use the latter to choose `scrawl send --session`. `scrawl send` without `--session` uses the top-level destination and refuses a different `--session` unless `--reroute` is passed (`handoff::recorded_destination`). Preserve an explicit destination during recovery, check whether it already received the capture, and ask before overriding it. If no destination is recorded and app ownership is ambiguous, ask rather than guessing from the viewed session or recent activity. Do not change a private project's endpoint or recipient as a recovery shortcut.

## Subsystem map

Read the matching `docs/` file before changing a subsystem. Each one holds that area's invariants, CLI tools and test commands.

| Subsystem | Code | Read first |
|---|---|---|
| Shortcuts (a press records the screen; the menu opens the picker), Mac recording, more than one display | `src/shortcut.rs`, `src/capture.rs`, `src/screens.rs`, `src/ui.rs` | `docs/capture.md` |
| Keyframe selection, transcription (SpeechAnalyzer, Hex, command), `summary.md`, frames and contact sheet | `src/keyframes.rs`, `src/transcribe.rs`, `src/hex.rs`, `src/command_transcriber.rs`, `src/speech_model.rs`, `src/sketch.rs`, `src/render.rs` | `docs/keyframes-and-text.md` |
| Onboarding, permissions, Settings, command-line tool and agent skill | `src/onboarding.rs`, `src/settings.rs`, `src/command_line.rs`, `helper/Onboarding.swift`, `helper/PermissionPanel.swift`, `helper/Settings.swift` | `docs/onboarding-and-settings.md` |
| iPhone capture, ScrawlKit ingest and delivery, dev-site routing and `sdk.js` | `src/phone.rs`, `src/delivery.rs`, `src/routing.rs`, `src/web.rs`, `src/sdk.js`, `Sources/ScrawlKit/`, `helper/` | `docs/iphone-and-routing.md` |
| Capsule motion, the pill lab, tuning | `src/ui/lab.rs`, `src/ui/motion.rs`, `perf/pill/` | `docs/motion.md` |
| Video export, retention and shrinking | `src/export/`, `src/retention.rs`, `helper/Export.swift`, `helper/Media.swift` | `docs/export-and-retention.md` |
| Windows: the OS shell behind `platform::`, overlay, hotkeys, Media Foundation, Parakeet, GPUI Settings and tour | `src/capture/windows.rs`, `src/ui/windows.rs`, `src/win/`, `src/settings_window.rs`, `src/onboarding_window.rs`, `src/ax_other.rs` | `docs/windows.md` |
