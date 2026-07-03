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
  not a v1 promise. Phase 0 shipped in v0.3.0: the whole workspace compiles on
  non-Linux with the mock backend (transport seam, `dirs`-crate paths,
  unix/linux cfg-gating); the capture/encode engine remains Linux-only.
- **License: MIT.**

## Module layout

One crate per concern. To add a top-level concern, add a `crates/<name>` and a
workspace member entry in the root `Cargo.toml`.

| Crate | Owns |
|-------|------|
| `crates/ord-common` | Shared types + the bincode IPC wire protocol (commands/events) + the cross-platform IPC transport seam (`transport.rs`: Unix socket on unix, loopback TCP + port rendezvous file elsewhere). No other I/O. |
| `crates/ord-core`   | The engine: wraps `waycap-rs`, owns the encoded-frame replay store (RAM `RingBuffer` or `DiskFrameStore` via the `FrameStore` seam), the keyframe-aware "save last N seconds" muxer (ffmpeg-next, stream-copy, no re-encode), and the pure per-app audio routing (`audio_route::plan_track`). |
| `crates/ord-daemon` | `ordd`: runs `ord-core`, supervises the buffer, exposes the control plane over the `ord-common` transport seam (Unix socket on unix; loopback TCP + rendezvous file elsewhere), game detection (`hyprctl`), storage policy (templates + prune), the capture watchdog, post-save verification + hook, and the layered-config apply (`SetConfig`). Hotkeys are compositor keybinds invoking `ord` (no evdev). |
| `crates/ord-cli`    | `ord`: thin client. Talks to the daemon socket (`save --last N`, `mark`, `shot`, `record toggle`, `status [--json]`, `buffer on/off`, `config show`/`config set`, `subscribe [--reconnect]`) + local `doctor` (NVIDIA P2 fix), `export`, and `--version`. What compositor keybinds call. |
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
- **Config is layered; only the daemon writes overrides.** The base
  `config.toml` (often a read-only Home Manager symlink) is never modified at
  runtime. Settings changes persist as a sparse diff in
  `$XDG_STATE_HOME/open-recorder/overrides.toml` (`Config::from_layers` /
  `diff_overrides` in `ord-common`), written only by `ordd` via `SetConfig`.
- **UI follows the design system.** All colors, spacing, radii, and type sizes
  in `ord-ui` come from `ord-ui/src/theme.rs` (sumi grey scale, one vermilion
  accent, 8-pt rhythm). No hardcoded `Color32`s in views.

## Testing standards (the quality guarantee)

Every crate ships tests. A change is not done until the relevant tiers below are
green. CI has no GPU, so GPU-dependent tests are gated.

| Tier | Scope | Where |
|------|-------|-------|
| **Unit** | Ring-buffer eviction & capacity math; keyframe-seek boundary math for "save last N" (the clip must start on the newest keyframe ≤ N seconds back); IPC encode/decode round-trips; newtype invariants. | `#[cfg(test)]` in each crate |
| **Integration** | Daemon socket command/event flow driven against a **mock `CaptureBackend`** (deterministic synthetic frames, no GPU). Covers `save`, `record toggle`, `status`, buffer on/off, and error surfacing. | inline in `crates/ord-daemon/src/server.rs` under `#[cfg(all(test, unix))]` |
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

Three workflows, all on `x86_64-linux` runners:

- **`.github/workflows/ci.yml`** — on every push/PR: `cargo fmt --check`,
  `cargo clippy -D warnings`, `cargo test --all` (GPU tests excluded), an MSRV
  `cargo check` (1.87), a cross-target `cargo check` lane (windows-gnu +
  apple-darwin, protects Phase 0), the `--all-features` devshell lane, and
  `nix flake check`. On master/tags it also builds every package with Nix and
  pushes the closures to `grok-insider.cachix.org`, so flake consumers pull
  prebuilt instead of compiling.
- **`.github/workflows/release.yml`** — the **patch-line release pipeline**.
  A self-owned `release-pr` job (NOT `release-plz release-pr`, whose git-only
  change detection cannot package the git-pinned waycap-rs fork) maintains a
  `chore: release vX.Y.Z+1` PR whenever `feat`/`fix` commits landed since the
  last tag: version bump (every crate inherits the single
  `[workspace.package].version`), `Cargo.lock`, and an AI-written
  `CHANGELOG.md` section. Merging it makes `release-plz release`
  (`release_always`) tag `vX.Y.Z` + create the GitHub Release; two artifact
  jobs attach the static `ord` client (`x86_64` musl) and the
  `ordd`/`ord-hud`/`ord-ui` **AppImages**. No crates.io publish (`git_only`).
  Cachix stays in ci.yml by design.
- **`.github/workflows/manual-version-bump.yml`** — repo-admin
  `workflow_dispatch` for deliberate **minor/major** milestones (feature drops,
  IPC protocol bumps); the automatic stream never leaves the patch line.

**Commit messages are Conventional Commits** — they drive the release trigger
and the changelog, so prefix every subject (`feat:`/`fix:`/`docs:`/`refactor:`/
`perf:`/`test:`/`chore:`/`ci:`; use `feat!:` or a `BREAKING CHANGE:` footer for a
break, and bump `PROTOCOL_VERSION` on any incompatible IPC change — then ship it
via a manual minor). Never hand-edit `CHANGELOG.md` — the release pipeline
generates it. See `CONTRIBUTING.md` and `docs/releasing.md`.

## Status

**v0.1** — working, hardware-verified. The full pipeline runs end-to-end on the
dev box (RTX 5070 Ti, Hyprland): PipeWire DMA-BUF → NVENC → encoded ring buffer
→ keyframe-aware save → valid 1440p `.mkv`, ffprobe-validated (see
`docs/spike-results.md`). All seven crates are implemented and tested:
`ord-common` (newtypes, IPC + `Subscribe`, versioned framing, layered config),
`ord-core` (ring buffer, keyframe clip selection, engine, audio ring, mock
backend, codec-keyed bitstream module, clip muxer + streaming recorder),
`ord-daemon`, `ord-cli`, `ord-overlay` (trait + wlr-layer-shell + `ord-hud`,
verified live over fullscreen games), `ord-ui` (library + editor), and
`ord-export`. HEVC/AV1 capture and CBR bitrate control are wired through the
pinned `grok-insider/waycap-rs` rev (when bumping it, update both
`crates/ord-core/Cargo.toml` and the `outputHashes` entry in `flake.nix`).

**v0.2.0** — the ShadowPlay-parity feature drop: `ord doctor [--fix]` (the
NVIDIA `CudaNoStablePerfLimit` profile that lifts the CUDA/NVENC P2 downclock),
capture knobs (`resolution` / `keyframe_interval_ms` / `framerate_mode` /
`color_range` / `tune`; applied once the waycap-rs fork exposes the setters —
see the `// fork:` block in `waycap_backend.rs`), disk-backed replay
(`capture.replay_storage = disk` via `DiskFrameStore` behind the `FrameStore`
seam), `ord shot`, `capture.target` + `capture.auto_arm`, multi-track +
per-application audio config (`audio.tracks`, pure routing in
`audio_route::plan_track`; live PipeWire+Opus capture is the gated follow-on),
and HDR config (`capture.hdr`, validated to HEVC/AV1; spikes in `docs/hdr.md`).
Versioning landed with it: `ord`/`ordd --version` print `X.Y.Z [protocol N]`
and `PROTOCOL_VERSION` bumped 3→4 (`Screenshot`/`ScreenshotSaved`).

**v0.2.1 / v0.2.2** — the clip-library grid fix (a real wrapping grid instead
of one off-screen row) and the editor/player QA round: an idle-aware UI
watchdog (no false `UI STALL` ANRs from a paused editor), audio-clock stall
recovery (pause instead of a 60fps spin when the clock-driving audio output
freezes), `ORD_DEBUG_LOG` telemetry, and the `ORD_AUTOPLAY` QA aid. Full
play-through and trim+export (`av1_nvenc`) verified clean on the RTX 5070 Ti.

**v0.3.0** — cross-platform Phase 0: the whole workspace compiles on non-Linux
with the mock backend. The IPC transport seam (`ord-common/src/transport.rs`:
Unix socket on unix; loopback TCP + port rendezvous file elsewhere), path
resolution via the `dirs` crate, the disk store gated `#[cfg(unix)]`, and the
waycap NVENC backend gated on `feature = "waycap"` + `target_os = "linux"`
(target-gated dependency). Project identity migrated to **grok-insider**; the
waycap-rs fork originally stayed at `github.com/0xfell/waycap-rs` — that repo has since vanished, so the fork now lives at `github.com/grok-insider/waycap-rs` (repointed in v0.4.3).
release-plz is live and cut this release (PRs #2/#3).

**v0.4.0** — the full-codebase audit round (2026-07-02):
non-blocking subscriber broadcast + off-lock engine starts in the daemon,
bounded recorder audio, drop-until-keyframe forwarding, incremental disk
compaction, scoped/cancellable export fallback, CLI `config set` /
`status --json` / `subscribe --reconnect`, editor sub-second time + library
keyboard nav + exports section, HiDPI HUD, and **`PROTOCOL_VERSION` 4→5**
(`RecordState` gained the recording's path). CI gained a cross-target
`cargo check` lane (windows-gnu + apple-darwin).

**v0.4.1 / v0.4.2** — release-pipeline proof (the first automatic patch-line
releases) and the subscriber-registration race fix (a snapshot reader can no
longer miss a racing broadcast). **v0.4.3 (capture supervisor)** — daemon
startup is socket-first and every capture start/restart runs on the dedicated
supervisor thread (`ord-daemon/src/supervisor.rs`): a hanging/slow/denied
screen-share portal leaves the daemon reachable-but-degraded (bounded retries
at login, no dialog re-spam after a user cancel), and watchdog restarts adopt
the old engine's replay state instead of discarding footage.

Forward work is tracked in `continue-plan.md` (the single roadmap); the shipped
record is `docs/roadmap-status.md`.
