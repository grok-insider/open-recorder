# Changelog

All notable changes to open-recorder are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-07-08

- Added customizable pressed-key keycaps to the overlay HUD, allowing you to see which keys are pressed with distinct visual indicators.

## [0.4.3] - 2026-07-04

- Fixed a critical bug where a hung portal dialog during capture start could make ordd unreachable; capture startup now retries automatically and never blocks the control socket.
- Fixed a race condition where subscribers could receive events out of order after a capture restart.
- Fixed build failures caused by a missing dependency repository (0xfell/waycap-rs).

## [0.4.2] - 2026-07-02

- Fixed subscriber registration no longer loses a racing broadcast, ensuring HUD and `ord subscribe` never miss state change events.

## [0.4.1] - 2026-07-02

- Internal improvements and maintenance

## [0.4.0] - 2026-07-02

The full-codebase audit round. **Breaking:** the IPC protocol bumped 4 → 5
(`RecordState` events now carry the recording's file path) — update `ord` and
`ordd` together.

### Added

- `ord config set <key> <value>` — change one setting from the CLI (typed
  against the effective config, persisted as a sparse override).
- `ord status --json` for waybar/scripts, and `ord subscribe --reconnect`
  (a closed daemon connection now reports — and optionally retries — instead
  of exiting silently).
- Starting/stopping a recording reports the file path (protocol v5).
- One-sided export trims: `--start` alone runs to the end of the clip,
  `--end` alone starts at 0.
- Auto-disarm: a buffer that `capture.auto_arm` armed turns itself off about a
  minute after the game leaves the foreground.
- Clip library: exports now appear in their own section, and the grid has
  keyboard navigation (arrows/Enter/Delete/Ctrl+F) with a focus ring.
- Editor: sub-second timecodes (`m:ss.mmm`) in the transport and hover bubble,
  so frame-stepping is visible; volume/loop persist across opens.
- HiDPI HUD: the overlay renders at the output's buffer scale (no more blur on
  scale-2 monitors).
- Disk replay store: spill write failures are counted and observable.
- CI: a cross-target `cargo check` lane (windows-gnu + apple-darwin) protects
  the "compiles everywhere with the mock backend" guarantee.

### Changed

- `ClipSaved` reports the actually-buffered duration instead of the configured
  capacity (saving right after arming no longer claims a full-length clip).
- The NVENC→software export fallback only triggers on hardware-encoder error
  signatures — a disk-full error no longer costs a full software re-encode.
- Export plans map streams explicitly (`-map 0:v:0 -map 0:a:0?`) and relax
  MP4 strictness when stream-copying Opus.
- `--help` prints to stdout and exits 0 across `ord` and `ord export`.
- Library refresh is incremental (only new/changed clips are re-probed), and
  the filter/sort result is cached between repaints.
- Releases stay git-only: crates.io publishing is disabled in
  `release-plz.toml` (manifest-level `publish = false` was tried and reverted —
  release-plz skips such packages' git releases entirely).

### Fixed

- The daemon's capture-drain pump can no longer be stalled by a frozen
  subscriber (broadcasts go through bounded per-subscriber queues), a portal
  picker during a settings apply (engines start off every lock), or a hung
  `hyprctl` (hard 2 s kill timeout).
- The streaming recorder bounds held-back audio after the header too — a
  stalled video stream during a recording no longer grows memory unboundedly.
- The capture forwarder drops until the next keyframe on channel overflow, so
  the replay buffer never holds an undecodable GOP head.
- Disk replay compaction is incremental (a bounded slice per push) instead of
  a synchronous full-file rewrite that could stall capture for seconds; save
  reads coalesce adjacent payloads into single reads.
- Export cancellation kills a wedged ffmpeg even when no progress is flowing.
- Same-second saves get `-1`/`-2` suffixes instead of overwriting; settings
  overrides persist atomically; pruning covers the recordings directory (with
  a fresh-file grace) and a `framerate_mode = content` static screen no longer
  trips the capture watchdog into restart loops.
- The UI stall watchdog survives poisoned locks (`lock_tolerant`), matching
  the project-wide rule.

## [0.3.0] - 2026-06-27

- Added cross-platform support: ordd now builds and runs on macOS and Windows (with a mock capture engine on non-Linux).
- Changed project identity and repository URLs from 0xfell to grok-insider.
- Improved path resolution to use the dirs crate, ensuring correct config, cache, state, and video directories on Linux, macOS, and Windows.
- Fixed build failures on non-Unix platforms by introducing a cross-platform IPC transport layer (TCP loopback on Windows, Unix sockets on Unix).

## [0.2.2] - 2026-06-25

### Features

- **Reliability watchdog.** If the buffer is armed but no frames arrive for
  5 s (suspend/resume kills NVENC; output changes end the portal session),
  `ordd` restarts the capture session and announces it (`CaptureRestarted`
  toast). Every saved clip is **verified in-process** (readable container,
  video stream, positive duration) before the daemon reports success — the
  "silently stopped recording" / "Saving… produced an empty file" failure
  modes that plague ShadowPlay/ReLive are now detected the moment they happen.
  The HUD also shows a distinct grey dot while the daemon is unreachable.
- **In-app settings.** A full Settings page in `ord-ui` edits the daemon
  configuration over IPC (`GetConfig`/`SetConfig`): capture (fps, buffer,
  quality, codec, CBR, clear-on-save), audio, storage, markers, hooks, and
  export defaults. Changes are layered as **runtime overrides**
  (`$XDG_STATE_HOME/open-recorder/overrides.toml`) merged over the base
  `config.toml` — a read-only Home Manager base keeps working, overridden
  fields are marked, and "Reset to base" drops them. Live-tier fields apply
  instantly; encoder fields apply via an explicit "Apply (restarts capture)".
  `ord config show` prints the effective config.
- **Markers ("clip that").** `ord mark` (bind it next to save) bookmarks the
  current moment; markers inside a later save become **MKV chapters**
  (ffprobe/players/editors show them), and `markers.auto_save_seconds` turns a
  mark into bookmark+clip in one keypress. Markers survive in the file — not
  in a sidecar that desyncs (the Steam-markers complaint).
- **Daemon controls in the library header**: arm/disarm the buffer, Save last
  15/30/60/120 s, and toggle recording — one click each, no keybind needed.
- **Storage policy.** `storage.clips_dir`, filename templates
  (`{game}{rec}-{epoch}`, `{date}/{time}` tokens, `/` creates subfolders →
  date folders), and **auto-prune** by total size (`max_gib`) and/or age
  (`max_age_days`) — exports are never touched. `capture.clear_on_save`
  drops the buffer after each save (no overlapping clips).
- **Copy as file** on every clip card (`wl-copy text/uri-list`) — paste a clip
  straight into Discord. **Vertical 9:16 export preset** (center-crop +
  1080×1920 H.264) for Shorts/TikTok/Reels.

### UI

- **Full visual revamp** on a new design system (`ord-ui/src/theme.rs`):
  Japanese-corporate minimalism — sumi ink-grey scale with hairline borders
  and small radii (no shadows, no bubbles), one vermilion accent reserved for
  record/danger/brand, muted matcha/kin functional colors, an 8-pt spacing
  rhythm and a strict type scale. Library cards, header (brand mark + live
  daemon badge + controls), status bar, editor timeline, and the new settings
  page all draw from the same tokens.

### Protocol

- IPC protocol v2: `GetConfig`, `SetConfig`, `Mark` commands; `Config`,
  `Marked`, `CaptureRestarted` events. Old `ord`/`ordd` pairs fail loudly with
  a version mismatch instead of mis-decoding.

- **HEVC/AV1 capture + CBR.** `capture.codec = "h264"|"hevc"|"av1"` and
  `capture.bitrate_kbps` (CBR) in `config.toml`, working end-to-end: the
  `0xfell/waycap-rs` fork now exposes `hevc_nvenc`/`av1_nvenc` and a
  `RateControl::ConstantBitrate` knob (1-second VBV), and the mux side writes
  the matching `hvcC`/`av1C` extradata. CBR keeps the replay buffer's RAM use
  predictable in high-motion scenes.
- **Post-save hook.** `hooks.on_clip_saved = "<program>"` runs after every saved
  clip with the clip path as `$1` (gpu-screen-recorder's `-sc`, but config-
  driven): notifications, renames, uploads. Asynchronous and off the capture
  path; a broken hook is logged, never fails the save.
- **Library search + sort + daemon status.** The clip library has
  search-as-you-type over clip names (Esc clears), a Newest/Oldest/Name sort
  selector, and a live daemon badge in the header (buffer armed + buffered
  seconds / recording / buffer off / daemon offline) polled over the control
  socket.
- **Honest export size estimates.** Every size-predictable preset in the editor
  export menu shows an estimate derived from the planner's actual bitrate math
  (including the budget overhead factor and the 100 kbps floor on very long
  selections), replacing the previous decorative constant.

### Architecture / internal

- **One bitstream module, two muxers.** All per-codec logic — `avcC`/`hvcC`/
  `av1C` extradata building and Annex-B→length-prefix packet transforms — now
  lives in `ord-core/src/mux/bitstream.rs` keyed by `Codec`, with full HEVC SPS
  and AV1 sequence-header parsing, unit-tested without ffmpeg. The clip muxer
  and the streaming recorder share one stream-setup helper (`mux/stream.rs`),
  eliminating the duplicated unsafe codecpar/extradata blocks and `is_h264`
  branches (`write_clip` shrank from ~244 lines to a readable sequence).
- **`FrameStore` seam.** The engine is generic over the replay store
  (`Engine<B, S: FrameStore = RingBuffer>`); clip selection works on a metadata
  scan instead of borrowing the RAM deque, so a disk-backed buffer (longer
  windows, low-RAM boxes) is now an implementation, not a refactor.
- **`lock_tolerant` everywhere.** The poisoned-lock-recovery helper moved to
  `ord-common` and is used by the daemon *and* all UI crates; the editor player
  had ~20 `lock().unwrap()` sites that could cascade a decode-thread panic into
  the UI thread. The player's 165-line `decode_loop` was split into a
  `DecodeSession` with one method per phase (seek/pacing/EOF/video/audio).
- **Shared IPC client.** `ord_common::client` owns connect/request/subscribe;
  `ord` and `ord-hud` no longer hand-roll the framing dance.
- Dedupe & dead code: one thumbnail extractor (library cache + editor filmstrip),
  `Preset::ALL` + `Preset::slug()` so menus and filenames can't drift, removed
  the unused `Overlay::set_visible`, `Timeline::fraction/time_at`, and
  `Player::volume`.

- **Real full-length recording.** `ord record` now starts/stops an actual NVENC
  recording (streaming muxer, separate from the replay buffer) written as a
  game-named `<game>-rec-<epoch>.mkv`, instead of the old no-op that reported
  success and wrote nothing.
- **More export presets**: GIF, audio-only, X/Twitter, and 1080p60-HQ, plus
  EBU R128 loudness normalization (`--normalize`) and NVENC multipass for
  tighter size-targeted exports. `ord export --preset gif|audio|x|1080p60`.
- **Export progress + cancel**: the library shows a live percentage and a Cancel
  button per export; the full preset menu is available on each clip and in the
  editor.
- **HUD**: a subtle buffer-active indicator (top-right dot), identical toasts
  coalesce instead of stacking, and toasts keep animating across a daemon
  restart.
- Steam games get readable clip/recording names (the window title, e.g.
  `path-of-exile-2-…`, instead of `steam-app-2694490-…`).

### Performance

- **Clip save is now off the daemon lock and copy-free.** Encoded frame payloads
  are reference-counted (`bytes::Bytes`), so `take_clip` is a refcount bump
  rather than a deep copy of the whole window (criterion: **5.43 ms → 24 µs**).
  The daemon prepares the clip under the handler lock and runs the ffmpeg mux +
  game probe off it, so the capture-drain pump is never starved — no more
  dropped frames right after a save.
- **HUD no longer redraws at 60 fps while idle.** `ord-hud` repaints only while a
  toast is animating and blocks on the event channel otherwise; glyph bitmaps are
  cached. Idle CPU dropped from a pinned core to ~0 during gameplay.
- **Editor preview parks decoding when paused** (holds ~4 frames instead of 30,
  freeing ~140–380 MB at 1440p) and reuses the preview texture on the CPU path.
- Release binaries build with thin LTO + a single codegen unit + symbol
  stripping.

### Fixed

- `RingBuffer::clear()` now resets the eviction anchor, so a buffer toggle that
  restarts capture at a lower timestamp epoch is no longer wiped on arrival.
- Export no longer panics on a multibyte character at the stderr-tail cut, and
  removes the truncated output file when ffmpeg fails.
- `ordd` survives transient accept errors, recovers from poisoned locks, and
  handles SIGTERM/SIGINT (removes the socket, exits immediately via `_exit` to
  avoid a waycap EGL atexit deadlock).
- Removed `unwrap`/`expect` from the ring-buffer eviction hot paths.

### Added

- Versioned IPC frame header (magic + `PROTOCOL_VERSION`): a stale `ord`/`ordd`/
  `ord-hud` binary now fails loudly instead of silently mis-decoding.
- `tracing` + `RUST_LOG` logging in `ordd`/core.
- `ClipDuration`/`BufferSeconds` newtypes threaded into the engine API; saves are
  clamped to the configured buffer and report the accurate duration.
- Criterion benches for ring-push, clip selection, and save-path mux latency.
- CI lane that builds, lints, and tests the feature-gated code (`waycap`/`mux`/
  `layershell`/`gui`) and evaluates the flake on every push/PR.

### Changed

- Centralized `socket_path`, ffmpeg/ffprobe binary resolution, the broadcast
  filter (`Event::is_state_change`), and the UI `ORD_*` env knobs.
- Renamed the misleading `Micros` type alias to `Ticks` (capture timestamps are
  the stream's time base — nanoseconds for waycap, not microseconds).
- Workspace-wide clippy lints, hoisted shared dependencies, MSRV (1.87), and the
  flake package version derived from `Cargo.toml`.
- Removed the Phase-1 `spike/` throwaway (its findings live in
  `docs/spike-results.md`).
