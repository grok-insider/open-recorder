# Changelog

All notable changes to open-recorder are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
