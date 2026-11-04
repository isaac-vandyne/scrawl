# Video export, retention and shrinking

Read this before changing `src/export/`, `src/retention.rs`, `helper/Export.swift` or `helper/Media.swift`.

## Compressed recording for web feedback (`src/web_recording.rs`)

The optional `includeCompressedRecording` setting creates a separate `recording-web.mp4` for clipboard attachments. It never replaces the original movie or WAV. ffmpeg/ffprobe are discovered on PATH and in the usual Homebrew locations, including when the app was launched from Finder.

The first encode uses H.264, up to a 1600-pixel edge, 24 fps, yuv420p and AAC narration with MP4 faststart. Separate WAV narration is trimmed by `videoOffsetMs` to align with the video, matching export's existing clock semantics. Embedded movie audio is retained when there is no WAV. A bounded lower-bitrate retry is available, but the final file is accepted only if its actual size is at most one fifth of the original and its complete duration, audio, format and decoding checks pass. Otherwise image/text copying remains available and no video is attached.

Child processes have finite deadlines and diagnostics are discarded. Cache metadata includes original video/audio stamps, timing offset and recipe version, so copying the same capture reuses verified output. Temporary files are cleaned on ordinary failures. `recording-web.mp4` follows the existing heavy-media retention policy; the original stays available until that policy removes it normally.

## Video export (`src/export/`, `helper/Export.swift`)

`scrawl export <bundle>` writes one H.264/HEVC + AAC `.mp4`; the `videoExport` flag (default off) only gates the menu's **Export Last Capture as Video…** (→ `~/Movies/Scrawl`, progress beside the menu bar icon). Rust decides everything and the helper renders it from a JSON plan (AVAssetReader → Core Image crop/camera → Core Graphics ink → AVAssetWriter): `crop.rs` (`--crop auto|full|x,y,w,h` — auto is `autocrop` `Scope::Whole` — even-sided output size, audio window), `ink.rs` (strokes via `sketch::freehand_outline`, laser, pins, ripples; sizes mirror `render::annotate_keyframe_in_place`), `camera.rs` (`--zoom`: pure events → camera rects, critically damped springs, never zooms during fast cursor travel; derived from ScreenDemon's click zoom). Everything in the plan is on the video's clock (bundle time − `videoOffsetMs`); Mac `audio.wav` starts on the bundle clock and the video starts `videoOffsetMs` later, so export trims that lead-in from the WAV; an iPhone movie uses its own audio. Output is constant frame rate (screen movies only have frames on change). Test with `cargo test export::`.

## Retention and shrinking (`src/retention.rs`, `Media.shrink` in `helper/Media.swift`)

The app's `retention` worker (utility QoS) runs a pass 90 s after launch and hourly (`PASS_EVERY`): heavy media (`handoff::HEAVY_MEDIA`, plus `zoom/` from `DERIVED_MEDIA`) to the Trash after `mediaDays` (default 7, writes `.media-cleaned`), whole bundles after `bundleDays` (30); `"retention"` in `~/.config/scrawl/config.json`, Settings › Storage. It skips bundles under a day old or whose folder changed (ctime) in the last day, live `.media-pending`/`.media-sending` markers, `.pinned`, `.deliver.json`, and bundles `retention::claim`ed. Cabled-iPhone `recording.mov` (> 6 Mbps) is re-encoded to HEVC at 1600 px a minute after `finalize_live_capture`, written beside the bundle and renamed over the original; sample times are kept, so `scrawl.json` is unchanged. Anything reading `video` must handle it missing (`retention::missing_media_reason`). Try passes on a temp folder with `scrawl clean --captures DIR --days-ahead N [--dry-run]`.
