# open-recorder

A native, open-source, Medal.tv / ShadowPlay-style game clipper for Linux —
always-on instant replay with near-zero overhead, save the last N seconds on a
keypress, browse and trim clips. NVIDIA-first, designed cross-platform.

> **Status: in development.** The capture/clip engine, daemon, and CLI are
> implemented and tested; the egui clip library and HUD overlay models are in.
> Real NVENC capture (the `waycap` feature) builds in the nix devshell and is
> pending final end-to-end validation in a live Hyprland session. See
> [`plan.md`](./plan.md) and [`docs/spike-results.md`](./docs/spike-results.md).

## Crates

| Crate | Binary | Role |
|-------|--------|------|
| `ord-common` | – | Shared newtypes + the bincode IPC protocol + socket framing. |
| `ord-core` | – | Ring buffer, keyframe-aware clip selection, engine, ffmpeg muxer (`mux`), NVENC capture (`waycap`). |
| `ord-daemon` | `ordd` | Capture supervision + Unix-socket control + game-named clips. |
| `ord-cli` | `ord` | Thin control client for compositor keybinds. |
| `ord-overlay` | – | `Overlay` trait + HUD toast model (layer-shell surface WIP). |
| `ord-ui` | `ord-ui` | egui clip library (`gui`); CLI clip listing otherwise. |

## Build & run

```sh
# Pure logic (no GPU): builds + tests anywhere.
cargo test --workspace
cargo build --release -p ord-cli

# Real recorder (NVENC): in the project devshell (CUDA + ffmpeg + PipeWire).
nix develop
cargo build --release -p ord-daemon --features waycap   # ordd
cargo build --release -p ord-ui --features gui           # clip library window
```

Run the daemon, then drive it:

```sh
ordd &                      # starts the replay buffer
ord save --last 30          # save the last 30s
ord status                  # buffer/recording/buffered seconds
ord buffer off              # pause the buffer
```

## Hyprland integration

```ini
# ~/.config/hypr/hyprland.conf
exec-once = ordd
bind = ALT, R, exec, ord save --last 30
bind = ALT SHIFT, R, exec, ord record
# Clip library in a special workspace (like Discord/Spotify):
windowrule = workspace special:clips, match:class ^(open-recorder)$
bind = SUPER, C, togglespecialworkspace, clips
```

## Why

Steam's built-in Game Recording cannot hardware-encode on Linux + NVIDIA — it
fails to init NVENC inside its container, falls back to CPU `libx264 veryfast`,
and produces macroblocked clips. open-recorder uses the path that actually
works on this hardware: **PipeWire DMA-BUF → NVENC, in-process, copy-free**,
for the highest achievable recording performance.

Full diagnosis and evidence: [`docs/performance.md`](./docs/performance.md).

## How

- **Native Rust**, zero-copy capture/encode via the `waycap-rs` crate.
- An in-RAM ring buffer of **already-encoded** frames; "save last N seconds"
  seeks the newest keyframe and stream-copies to `.mkv` (no re-encode).
- A daemon (`ordd`) + thin CLI (`ord`) over a Unix socket; evdev global hotkeys
  that work under fullscreen keyboard grab.
- `egui` clip-library window (lives in a Hyprland special workspace) and a
  click-through `wlr-layer-shell` HUD.

Architecture: [`docs/architecture.md`](./docs/architecture.md).

## Documentation

| Doc | Contents |
|-----|----------|
| [`plan.md`](./plan.md) | The full plan: whys, hows, decisions, phases, risks. |
| [`AGENTS.md`](./AGENTS.md) | How agents/contributors work here: clean-code + testing standards. |
| [`docs/architecture.md`](./docs/architecture.md) | Crate graph, capture→encode→ring-buffer→save dataflow. |
| [`docs/performance.md`](./docs/performance.md) | Why native zero-copy; the Steam-on-NVIDIA diagnosis + evidence. |
| [`docs/overlay.md`](./docs/overlay.md) | Special-workspace vs layer-shell HUD; cross-platform overlay strategy. |
| [`docs/backends.md`](./docs/backends.md) | The `CaptureBackend` and `Overlay` traits and their implementations. |
| [`docs/testing.md`](./docs/testing.md) | Test strategy: unit / integration / golden / bench / GPU lanes. |
| [`docs/spike-results.md`](./docs/spike-results.md) | Phase-1 spike outcome: native NVENC stack builds + initializes on the 610 driver. |

## License

MIT © 2026 0xfell. See [`LICENSE`](./LICENSE).
