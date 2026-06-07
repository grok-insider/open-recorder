# open-recorder

A native, open-source, Medal.tv / ShadowPlay-style game clipper for Linux —
always-on instant replay with near-zero overhead, save the last N seconds on a
keypress, browse and trim clips. NVIDIA-first, designed cross-platform.

> **Status: pre-implementation.** This repository currently contains the plan
> and design docs. Code lands starting with the Phase-1 spike. See
> [`plan.md`](./plan.md).

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

## License

MIT © 2026 0xfell. See [`LICENSE`](./LICENSE).
