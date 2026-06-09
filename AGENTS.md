# AGENTS.md

Instructions for AI agents and contributors working on open-recorder.

## Project overview

open-recorder is a Linux-native, zero-copy game clipper in the style of
Medal.tv / NVIDIA ShadowPlay: it keeps an always-on in-RAM ring buffer of
**already-encoded** frames and saves the last N seconds to disk on a keypress,
with near-zero overhead.

It exists because Steam's built-in Game Recording cannot hardware-encode on
Linux + NVIDIA (it fails with `NVENC - No CUDA support`, falls back to CPU
`libx264 veryfast`, and produces macroblocked output). See `docs/performance.md`
for the full diagnosis. open-recorder uses the capture/encode path that works:
**PipeWire DMA-BUF → NVENC, in-process, copy-free.**

- **Native Rust.** No wrapping of external recorder binaries. The capture and
  encode pipeline is owned in-process via the `waycap-rs` crate (PipeWire
  DMA-BUF import + ffmpeg-next/NVENC).
- **Cargo workspace**, one crate per concern (see Module layout).
- **Cross-platform by design, Linux-first.** Platform specifics sit behind two
  traits (`CaptureBackend`, `Overlay`). Linux/Wayland+NVENC ships first; a
  Windows (DXGI→NVENC) backend is a future implementation of the same trait,
  not a v1 promise.
- **License: MIT.**

## Module layout

One crate per concern. To add a top-level concern, add a `crates/<name>` and a
workspace member entry in the root `Cargo.toml`.

| Crate | Owns |
|-------|------|
| `crates/ord-common` | Shared types + the bincode IPC wire protocol (commands/events). No I/O. |
| `crates/ord-core`   | The engine: wraps `waycap-rs`, owns the encoded-frame ring buffer, and the keyframe-aware "save last N seconds" muxer (ffmpeg-next, stream-copy, no re-encode). |
| `crates/ord-daemon` | `ordd`: runs `ord-core`, supervises the buffer, exposes the Unix-socket control plane, game detection (`hyprctl`), and the post-save hook. Hotkeys are compositor keybinds invoking `ord` (no evdev). |
| `crates/ord-cli`    | `ord`: thin client. Talks to the daemon socket (`save --last N`, `record toggle`, `status`, `buffer on/off`, `subscribe`). What compositor keybinds call. |
| `crates/ord-overlay`| Platform overlay abstraction: the `Overlay` trait + `wlr-layer-shell` (Wayland) implementation + the `ord-hud` binary. X11/Win32 are future implementations of the same trait. |
| `crates/ord-ui`     | `egui` clip library/manager window: browse, play, trim, export. |
| `crates/ord-export` | Pure ffmpeg-arg export planning (`plan.rs`, no I/O) + ffprobe wrapper + ffmpeg runner with NVENC→software fallback. Presets (social/GIF/quality) are data, not code. |

The capture/encode hot path is `ord-core` only. Everything else is control
plane or presentation and must never block or copy frames on that path.

## Engineering principles

- **Traits at the platform/engine seams.** `CaptureBackend` and `Overlay` make
  the OS and the capture engine swappable. Code against the trait, never a
  concrete backend, outside the backend's own module.
- **The hot path never panics and never copies.** Encoded frames move from the
  capture callback into the ring buffer over a bounded channel. No allocation
  per frame beyond the encoded packet itself; no `unwrap`/`expect`.
- **Errors are values.** Use `Result` with `thiserror` error enums. The only
  `unwrap`/`expect` allowed are in tests and in `main()` startup wiring where a
  failure must abort the process anyway (document why).
- **`unsafe` only in FFI shims.** Any `unsafe` block must be small, isolated in
  a backend module, and carry a `// SAFETY:` comment explaining the invariant.
- **Newtypes over primitives.** Wrap domain quantities (`BufferSeconds`,
  `ClipDuration`, `MonitorId`, `Keyframe`) so units and meanings can't be mixed
  up. No bare `u64` seconds floating through APIs.
- **No comments unless they explain non-obvious intent.** The code says what;
  comments say why, only when it isn't obvious.
- **One bitstream module, two muxers.** All per-codec bitstream logic
  (extradata building, Annex-B→length-prefix packet transforms) lives in
  `ord-core/src/mux/bitstream.rs` keyed by `Codec`, and is consumed by **both**
  the clip muxer (`mux.rs`) and the streaming recorder (`record.rs`) through the
  shared stream-setup helpers. Never duplicate codec or stream-setup logic
  between the two, and never branch on `is_h264`-style booleans — match on
  `Codec` (or use its strategy) so new codecs fail to compile, not silently
  mis-mux.
- **All `Mutex` access is lock-tolerant.** Use the shared poisoned-lock-recovery
  helper (`lock_tolerant` pattern) everywhere — including UI crates. A panicked
  worker thread must degrade, not cascade panics through `lock().unwrap()`.

## Testing standards (the quality guarantee)

Every crate ships tests. A change is not done until the relevant tiers below are
green. CI has no GPU, so GPU-dependent tests are gated.

| Tier | Scope | Where |
|------|-------|-------|
| **Unit** | Ring-buffer eviction & capacity math; keyframe-seek boundary math for "save last N" (the clip must start on the newest keyframe ≤ N seconds back); IPC encode/decode round-trips; newtype invariants. | `#[cfg(test)]` in each crate |
| **Integration** | Daemon socket command/event flow driven against a **mock `CaptureBackend`** (deterministic synthetic frames, no GPU). Covers `save`, `record toggle`, `status`, buffer on/off, and error surfacing. | `crates/ord-daemon/tests/` |
| **Golden** | Saved `.mkv` structure assertions via `ffprobe` (codec, duration ≈ requested, keyframe at start, audio track present). | `crates/ord-core/tests/` |
| **Bench** | `criterion` benchmarks for ring-buffer push and the save-path mux latency, to catch perf regressions on the hot path. | `crates/ord-core/benches/` |
| **GPU (real hardware)** | End-to-end capture→encode→save on actual NVENC. `#[ignore]` by default; run on the dev box behind `--features waycap` (in the devshell). | `crates/ord-core/tests/`, marked `#[ignore]` |

Rules:

- A `CaptureBackend` mock is mandatory so the daemon and core logic are testable
  without a GPU or a live Wayland session. Never let real capture leak into
  unit/integration tests.
- "Save last N" boundary math is the highest-risk logic in the project — it gets
  exhaustive unit tests (N at/below/above buffer length, no-keyframe-in-window,
  empty buffer, single-keyframe buffer).
- Tests must be deterministic. No sleeps to "wait for frames"; drive time and
  frame arrival explicitly through the mock.

## Clean-code checklist (before every commit)

```sh
cargo fmt --all                 # format
cargo clippy --all-targets --all-features -- -D warnings   # lint, warnings = errors
cargo test --all                # unit + integration + golden (GPU tests are #[ignore])
```

All three must pass. `-D warnings` means clippy lints are hard failures, not
suggestions. Public items in library crates carry doc comments.

## Key commands

```sh
cargo build                     # debug workspace build
cargo build --release           # release binaries (ordd, ord)
cargo test --all                # the CI test set
cargo test --features waycap -- --ignored   # real-hardware GPU lane (devshell, dev box only)
nix develop                     # devshell with pipewire/ffmpeg/cuda/clang toolchain
```

## Conventions

- No comments unless they explain non-obvious intent.
- Hot path: no panics, no per-frame copies, no allocation churn.
- All FFI/`unsafe` isolated in backend modules with `// SAFETY:` notes.
- Keep user-facing messages (CLI errors, notifications) actionable.
- Match `waycap-rs` / ffmpeg semantics faithfully; these are real-time media
  APIs where field meanings and timing matter — be precise.

## CI / release

`.github/workflows/ci.yml` (added in the build phase) runs `cargo fmt --check`,
`cargo clippy -D warnings`, and `cargo test --all` (GPU tests excluded) on
`x86_64-linux`, then `nix flake check`, and pushes store paths to
`0xfell.cachix.org` so flake consumers pull prebuilt closures.

## Status

Working. The full pipeline is **verified end-to-end on hardware** (RTX 5070 Ti,
Hyprland): PipeWire DMA-BUF → NVENC H.264 → encoded ring buffer →
keyframe-aware save → valid 1440p `.mkv`, ffprobe-validated (see
`docs/spike-results.md`). Implemented and tested: `ord-common` (newtypes, IPC +
`Subscribe`, versioned framing, config), `ord-core` (ring buffer, keyframe clip
selection, engine, audio ring, mock backend, codec-keyed bitstream module, clip
muxer + streaming recorder; `mux`/`waycap` features build in the devshell),
`ord-daemon` (`ordd` socket + handler + game detection + event broadcast +
post-save hook), `ord-cli` (`ord` incl. `subscribe` and `export`), `ord-overlay`
(trait + HUD model + real wlr-layer-shell surface behind `layershell` +
`ord-hud`), `ord-ui` (clip library model + egui app behind `gui`), `ord-export`
(pure plan + runner with NVENC→software fallback). The HUD is verified live on
Hyprland over fullscreen games. Run `cargo test --all` for the CI set.

HEVC/AV1 capture and CBR bitrate control are wired end-to-end: the pinned
`0xfell/waycap-rs` rev exposes `hevc_nvenc`/`av1_nvenc` and
`RateControl::ConstantBitrate`, selected via `capture.codec` /
`capture.bitrate_kbps` in `config.toml`, with matching `hvcC`/`av1C` extradata
from the bitstream module. When bumping the waycap-rs rev, update both
`crates/ord-core/Cargo.toml` and the `outputHashes` entry in `flake.nix`.
