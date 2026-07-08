# open-recorder

[![CI](https://github.com/grok-insider/open-recorder/actions/workflows/ci.yml/badge.svg)](https://github.com/grok-insider/open-recorder/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/grok-insider/open-recorder?sort=semver)](https://github.com/grok-insider/open-recorder/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A native, open-source, Medal.tv / ShadowPlay-style game clipper for Linux —
always-on instant replay with near-zero overhead, save the last N seconds on a
keypress, browse, trim, and export clips. **NVIDIA-first (NVENC), Wayland-native.**

> **Status: working (v0.3.0).** The full pipeline is verified end-to-end on
> hardware (RTX 5070 Ti, Hyprland): `ordd` + `ord save` records real NVENC clips,
> the click-through wlr-layer-shell HUD renders over fullscreen content, and
> `ord-ui` browses/edits the library. It replaces Steam's CPU-x264 macroblocking
> with hardware NVENC. The codebase also compiles and runs on macOS/Windows with a
> **mock** capture backend (no real recording there yet — a Windows DXGI→NVENC
> backend is future work).

## Crates

| Crate | Binary | Role |
|-------|--------|------|
| `ord-common` | – | Shared newtypes, layered config, and the bincode IPC protocol + socket framing. |
| `ord-core` | – | Ring buffer, keyframe-aware clip selection, engine, codec-keyed bitstream + clip muxer + streaming recorder (`mux`), NVENC capture (`waycap`). |
| `ord-export` | – | Pure ffmpeg-arg export planner + ffprobe wrapper + ffmpeg runner with NVENC→software fallback; presets. |
| `ord-daemon` | `ordd` | Capture supervision, Unix-socket control plane (loopback TCP off-unix), game detection (`hyprctl`), storage prune, watchdog, post-save verify + hook. |
| `ord-cli` | `ord` | Thin control client for compositor keybinds + local `doctor`/`export`. |
| `ord-overlay` | `ord-hud` | `Overlay` trait + click-through `wlr-layer-shell` HUD that subscribes to the daemon. |
| `ord-ui` | `ord-ui` | egui clip library + inline player/editor/trim/export + settings. |

## Requirements

- **NVIDIA proprietary driver** (provides `libcuda` / `libnvidia-encode`). Driver
  **≥ 580** is needed for `ord doctor`'s P2-downclock fix.
- An **NVENC-capable GPU**; **AV1/HEVC** encode needs an RTX 40/50-series card.
- A **Wayland** session with a working **screencast portal** (`xdg-desktop-portal`)
  and **wlr-layer-shell** (for the HUD), plus **PipeWire**.
- **ffmpeg / ffprobe** on `PATH` for `ord export` and `ord shot` (override with
  `ORD_FFMPEG` / `ORD_FFPROBE`).
- **FUSE2** for the AppImages (or run with `--appimage-extract-and-run`).
- **Hyprland** for automatic game-name detection / `auto_arm` (uses `hyprctl`);
  recording itself works on any wlroots compositor.

## Install (NixOS, prebuilt — no compiling)

CI builds every binary and pushes it to the **`grok-insider` cachix cache**, so you
get prebuilt closures instead of compiling CUDA/ffmpeg/waycap-rs locally. Add the
substituter once:

```nix
# configuration.nix (NixOS) or nix.conf
nix.settings = {
  substituters = [ "https://grok-insider.cachix.org" ];
  trusted-public-keys = [
    "grok-insider.cachix.org-1:ZxLVOxJ1CjdY3vQl1I99qCtwNZwIU4+/QwqSvntB/5w="
  ];
};
```

Then run or install any binary straight from the flake:

```sh
nix run github:grok-insider/open-recorder#ordd      # the NVENC daemon
nix profile install github:grok-insider/open-recorder   # all of: ord, ordd, ord-hud, ord-ui
```

### Home Manager

```nix
{
  inputs.open-recorder.url = "github:grok-insider/open-recorder";

  # in your HM config:
  imports = [ inputs.open-recorder.homeManagerModules.default ];
  programs.open-recorder.enable = true;   # installs all binaries
  # ordd + ord-hud run as user services by default; disable with:
  # programs.open-recorder.daemon.enable = false;
  # programs.open-recorder.hud.enable = false;
  # Optional: manage ~/.config/open-recorder/config.toml declaratively:
  # programs.open-recorder.settings = { capture = { fps = 60; buffer_seconds = 30; }; };
}
```

**Skip the picker after the first run:** the first time `ordd` captures, the
screencast portal shows "Select what to share" — pick your monitor and **tick
"Allow a restore token"**, then Share. `ordd` saves the granted token to
`$XDG_STATE_HOME/open-recorder/portal-restore-token` and reuses it on every later
start, so the picker never appears again.

## Install (other Linux — prebuilt, no compiling)

Not on Nix? Each [GitHub Release](https://github.com/grok-insider/open-recorder/releases)
ships prebuilt `x86_64` binaries (each with a `.sha256`):

- **`ord` client** — `ord-<ver>-x86_64-linux-musl.tar.gz`. A static binary; put it
  on `PATH` (this is what compositor keybinds call):

  ```sh
  tar -xzf ord-*-x86_64-linux-musl.tar.gz
  install -Dm755 ord ~/.local/bin/ord
  ```

- **`ordd`, `ord-hud`, `ord-ui`** — `*-<ver>-x86_64.AppImage`. Self-contained
  (ffmpeg/Wayland/GL bundled); the wrapper resolves the host NVIDIA driver:

  ```sh
  chmod +x ordd-*-x86_64.AppImage
  ./ordd-*-x86_64.AppImage          # the NVENC daemon
  ./ord-ui-*-x86_64.AppImage        # the clip library window
  ```

> A Flathub **Flatpak of `ord-ui`** (driver-matched GL + PipeWire portal) is the
> planned next step for the GUI.

## Build from source

```sh
# Pure logic (no GPU): builds + tests anywhere. Rust >= 1.87.
cargo test --workspace
cargo build --release -p ord-cli

# Real recorder (NVENC) + HUD: in the project devshell (CUDA + ffmpeg + PipeWire).
nix develop
cargo build --release -p ord-daemon --features waycap        # ordd
cargo build --release -p ord-ui --features gui               # clip library window
cargo build --release -p ord-overlay --features layershell   # ord-hud overlay

# Or build the nix packages directly (same as the cache provides):
nix build .#ordd .#ord-hud .#ord-ui .#ord-cli
```

A bare `cargo build` (without `--features waycap`) builds against a **mock**
capture backend — useful for development and CI, but it won't record real frames.

## Usage

Start the daemon, then drive it with `ord` (what your compositor keybinds call):

```sh
ordd &                  # start the replay buffer (needs --features waycap / a prebuilt to record)
ord save --last 30      # save the last N seconds (default 30)
ord mark                # bookmark now → a chapter in the next saved clip
ord shot                # screenshot the latest frame to a PNG
ord record              # toggle manual (continuous) recording
ord status [--json]     # buffer/recording state + buffered seconds (JSON for waybar)
ord buffer on|off       # arm / pause the replay buffer
ord config show         # print the effective daemon configuration
ord config set <k> <v>  # change one setting, e.g. `ord config set capture.fps 30`
ord config set overlay.pressed_keys.enabled true   # show pressed keys in demos
ord subscribe           # stream daemon events (--reconnect to survive restarts)
ord doctor --fix        # install the NVIDIA P2-downclock fix (see below)
ord export <in> [out]   # transcode/trim a clip — see `ord export --help`
ord --version           # prints "ord X.Y.Z [protocol N]"
```

- **`ord doctor [--fix]`** installs the NVIDIA `CudaNoStablePerfLimit` application
  profile that frees `ordd` from the CUDA/NVENC **P2 downclock** — the real
  performance delta vs ShadowPlay. Requires driver ≥ 580; it's the only thing
  `ord` writes under `~/.nv`, and `--fix` prints exactly what it changes.
- **`ord export`** is a local HandBrake-style transcoder (does not touch the
  daemon): `--preset high|discord|source`, `--codec av1|hevc|h264`,
  `--container mp4|mkv`, `--cq`/`--bitrate`/`--target-size`, `--max-height`/`--scale`,
  `--start`/`--end` (trim), `--no-hardware`. Needs `ffmpeg`/`ffprobe` on `PATH`.

**Environment variables:** `RUST_LOG` (daemon log level), `ORD_FFMPEG` /
`ORD_FFPROBE` (override the ffmpeg/ffprobe binaries), `ORD_DEBUG_LOG` and
`ORD_AUTOPLAY` (UI dev/QA aids).

## Hyprland integration

With the Home Manager module, `ordd` and `ord-hud` already run as user services,
so you only need the keybinds:

```ini
# ~/.config/hypr/hyprland.conf
# (if NOT using the HM services, also add: exec-once = ordd / exec-once = ord-hud)
bind = ALT, R, exec, ord save --last 30
bind = ALT SHIFT, R, exec, ord record
# Clip library in a special workspace (like Discord/Spotify):
windowrulev2 = workspace special:clips, class:^(open-recorder)$
bind = SUPER, C, togglespecialworkspace, clips
```

## Configuration

`~/.config/open-recorder/config.toml` (respects `XDG_CONFIG_HOME`); `ordd` writes
defaults on first run. The config is **layered**: the base file is never modified
at runtime — in-app/daemon changes persist as a sparse diff in
`$XDG_STATE_HOME/open-recorder/overrides.toml`.

| Section | Keys (defaults) |
|---------|-----------------|
| `[capture]` | `fps` (60), `buffer_seconds` (60), `quality` (high), `codec` (h264; hevc/av1 need RTX 40/50), `bitrate_kbps`, `resolution`, `keyframe_interval_ms` (2000), `framerate_mode` (cfr), `color_range` (limited), `tune` (performance), `replay_storage` (ram\|disk), `target` (portal), `auto_arm` (false), `hdr` (false), `clear_on_save` (false) |
| `[audio]` | `desktop`, `mic`, `tracks` (per-application audio sources) |
| `[storage]` | `clips_dir`, `recordings_dir`, `template`, `max_gib`, `max_age_days` |
| `[markers]` | `auto_save_seconds` |
| `[hooks]` | `on_clip_saved` (run a command after each save) |
| `[overlay]` | `show_status_dot`; `pressed_keys.enabled` (false), `pressed_keys.position` (`bottom_center`), `pressed_keys.x_ppm`/`y_ppm` (500/900), `pressed_keys.scale_percent` (100), `pressed_keys.opacity_percent` (92), `pressed_keys.rotation_degrees` (0), `pressed_keys.timeout_ms` (900), `pressed_keys.max_keys` (6) |
| `[export]` | `codec`, `container` (defaults for `ord export`) |

`overlay.pressed_keys.enabled = true` makes `ord-hud` read raw keyboard events
from `/dev/input` and render individual IBM Plex Mono keycaps into the
recording. The layout is editable in `ord-ui` Settings: drag the preview for a
custom position and adjust size, opacity, and 2D rotation. This is intentionally
off by default; grant input-device read permission only if you want this demo
aid.

## Why

Steam's built-in Game Recording cannot hardware-encode on Linux + NVIDIA — it
fails to init NVENC inside its container, falls back to CPU `libx264 veryfast`,
and produces macroblocked clips. open-recorder uses the path that actually works
on this hardware: **PipeWire DMA-BUF → NVENC, in-process, copy-free.** Full
diagnosis and evidence: [`docs/performance.md`](./docs/performance.md).

## How

- **Native Rust**, zero-copy capture/encode via the `waycap-rs` crate.
- An in-RAM ring buffer of **already-encoded** frames; "save last N seconds" seeks
  the newest keyframe and stream-copies to `.mkv` (no re-encode).
- A daemon (`ordd`) + thin CLI (`ord`) over a Unix socket; a capture watchdog and
  in-process post-save verification catch the "silently stopped recording" /
  "empty file" failure modes that plague ShadowPlay/ReLive.
- An `egui` clip-library window (with an inline trim/multi-cut editor + export) and
  a click-through `wlr-layer-shell` HUD.

Architecture: [`docs/architecture.md`](./docs/architecture.md).

## Documentation

| Doc | Contents |
|-----|----------|
| [`AGENTS.md`](./AGENTS.md) | How agents/contributors work here: clean-code + testing standards. |
| [`CONTRIBUTING.md`](./CONTRIBUTING.md) | Local setup, the commit/PR workflow, and gates. |
| [`CHANGELOG.md`](./CHANGELOG.md) | Release history (generated by release-plz). |
| [`docs/architecture.md`](./docs/architecture.md) | Crate graph, capture→encode→ring-buffer→save dataflow. |
| [`docs/performance.md`](./docs/performance.md) | Why native zero-copy; the Steam-on-NVIDIA diagnosis + evidence. |
| [`docs/overlay.md`](./docs/overlay.md) | Special-workspace vs layer-shell HUD; overlay strategy. |
| [`docs/backends.md`](./docs/backends.md) | The `CaptureBackend` and `Overlay` traits and their implementations. |
| [`docs/hdr.md`](./docs/hdr.md) | HDR capture (Main10 encode + KMS capture) notes. |
| [`docs/testing.md`](./docs/testing.md) | Test strategy: unit / integration / golden / bench / GPU lanes. |
| [`docs/releasing.md`](./docs/releasing.md) | SemVer/tag scheme + the release-plz flow. |
| [`docs/roadmap-status.md`](./docs/roadmap-status.md) | The shipped record: what each release delivered. |
| [`docs/spike-results.md`](./docs/spike-results.md) | The Phase-1 spike: zero-copy capture→NVENC validated on hardware. |

## License

MIT © 2026 Grok Insider. See [`LICENSE`](./LICENSE).
