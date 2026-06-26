# open-recorder

A native, open-source, Medal.tv / ShadowPlay-style game clipper for Linux —
always-on instant replay with near-zero overhead, save the last N seconds on a
keypress, browse and trim clips. NVIDIA-first, designed cross-platform.

> **Status: working.** Full pipeline verified end-to-end on hardware (NVIDIA RTX
> 5070 Ti, Hyprland): `ordd --features waycap` + `ord save` records real NVENC
> H.264 clips to `~/Videos/open-recorder/` (ffprobe-validated), the click-through
> wlr-layer-shell HUD renders over fullscreen content, and `ord-ui` lists the
> library. This replaces Steam's CPU-x264 macroblocking with hardware NVENC.
> See [`plan.md`](./plan.md) and [`docs/spike-results.md`](./docs/spike-results.md).

## Crates

| Crate | Binary | Role |
|-------|--------|------|
| `ord-common` | – | Shared newtypes + the bincode IPC protocol + socket framing. |
| `ord-core` | – | Ring buffer, keyframe-aware clip selection, engine, ffmpeg muxer (`mux`), NVENC capture (`waycap`). |
| `ord-daemon` | `ordd` | Capture supervision + Unix-socket control + game-named clips. |
| `ord-cli` | `ord` | Thin control client for compositor keybinds. |
| `ord-overlay` | `ord-hud` | `Overlay` trait + HUD toast model + real wlr-layer-shell surface (`layershell`); `ord-hud` subscribes to the daemon and shows events. |
| `ord-ui` | `ord-ui` | egui clip library (`gui`); CLI clip listing otherwise. |

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
}
```

Runtime needs the NVIDIA driver (`/run/opengl-driver`, present on any NixOS
NVIDIA host) and a Wayland session with a working screencast portal.

**Skip the picker after the first run:** the first time `ordd` captures, the
screencast portal shows "Select what to share" — pick your monitor and **tick
"Allow a restore token"**, then Share. `ordd` saves the granted token to
`$XDG_STATE_HOME/open-recorder/portal-restore-token` and reuses it on every
later start, so the picker never appears again. (Without the restore-token tick
the portal re-prompts each start.)

## Install (other Linux — prebuilt, no compiling)

Not on Nix? Each [GitHub Release](https://github.com/grok-insider/open-recorder/releases)
ships prebuilt `x86_64` binaries so you never compile CUDA/ffmpeg/waycap-rs:

- **`ord` client** — `ord-<ver>-x86_64-linux-musl.tar.gz`. A static binary; put it
  on `PATH` (this is what compositor keybinds call):

  ```sh
  tar -xzf ord-*-x86_64-linux-musl.tar.gz
  install -Dm755 ord ~/.local/bin/ord
  ```

- **`ordd`, `ord-hud`, `ord-ui`** — `*-<ver>-x86_64.AppImage`. Self-contained
  (ffmpeg/Wayland/GL bundled); just mark executable and run:

  ```sh
  chmod +x ordd-*-x86_64.AppImage
  ./ordd-*-x86_64.AppImage          # the NVENC daemon
  ./ord-ui-*-x86_64.AppImage        # the clip library window
  ```

Requirements: the host **NVIDIA driver** (the AppImage resolves `libcuda.so.1` /
`libnvidia-encode.so.1` from it), a Wayland session with a working screencast
portal, and **FUSE2** (or run with `--appimage-extract-and-run` on hosts without
it). Each asset has a `.sha256` to verify.

> A Flathub **Flatpak of `ord-ui`** (driver-matched GL + PipeWire portal) is the
> planned next step for the GUI.

## Build from source

```sh
# Pure logic (no GPU): builds + tests anywhere.
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

Run the daemon, then drive it:

```sh
ordd &                      # starts the replay buffer
ord save --last 30          # save the last 30s
ord status                  # buffer/recording/buffered seconds
ord buffer off              # pause the buffer
```

## Hyprland integration

With the Home Manager module, `ordd` and `ord-hud` already run as user services,
so you only need the keybinds:

```ini
# ~/.config/hypr/hyprland.conf
# (if NOT using the HM services, also add: exec-once = ordd / exec-once = ord-hud)
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

MIT © 2026 Grok Insider. See [`LICENSE`](./LICENSE).
