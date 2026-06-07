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
| `crates/ord-daemon` | `ordd`: runs `ord-core`, supervises the buffer, exposes the Unix-socket control plane, evdev global hotkeys, game detection (`/proc` + `hyprctl`), and notifications. |
| `crates/ord-cli`    | `ord`: thin client. Talks to the daemon socket (`save --last N`, `record toggle`, `status`, `buffer on/off`). What compositor keybinds call. |
| `crates/ord-overlay`| Platform overlay abstraction: the `Overlay` trait + `wlr-layer-shell` (Wayland), X11, and Win32 implementations. |
| `crates/ord-ui`     | `egui` UIs: the clip library/manager window and the click-through HUD. |

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

## Testing standards (the quality guarantee)

Every crate ships tests. A change is not done until the relevant tiers below are
green. CI has no GPU, so GPU-dependent tests are gated.

| Tier | Scope | Where |
|------|-------|-------|
| **Unit** | Ring-buffer eviction & capacity math; keyframe-seek boundary math for "save last N" (the clip must start on the newest keyframe ≤ N seconds back); IPC encode/decode round-trips; newtype invariants. | `#[cfg(test)]` in each crate |
| **Integration** | Daemon socket command/event flow driven against a **mock `CaptureBackend`** (deterministic synthetic frames, no GPU). Covers `save`, `record toggle`, `status`, buffer on/off, and error surfacing. | `crates/ord-daemon/tests/` |
| **Golden** | Saved `.mkv` structure assertions via `ffprobe` (codec, duration ≈ requested, keyframe at start, audio track present). | `crates/ord-core/tests/` |
| **Bench** | `criterion` benchmarks for ring-buffer push and the save-path mux latency, to catch perf regressions on the hot path. | `crates/ord-core/benches/` |
| **GPU (real hardware)** | End-to-end capture→encode→save on actual NVENC. `#[ignore]` by default; run on the dev box behind `--features gpu`. | `crates/ord-core/tests/`, marked `#[ignore]` |

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
cargo test -- --ignored --features gpu   # real-hardware GPU lane (dev box only)
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

Pre-implementation. This repository currently contains the plan and docs only.
Code lands starting with the Phase-1 spike (validate `waycap-rs` zero-copy
DMA-BUF + NVENC HEVC on the NVIDIA 610 open driver) — see `plan.md`.
