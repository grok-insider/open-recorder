# Changelog

All notable changes to open-recorder are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Features

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
