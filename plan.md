# open-recorder — Plan

> **Historical document.** This is the genesis plan (June 2026). The codebase
> has shipped through v0.3.0; see `AGENTS.md` for current architecture and
> `continue-plan.md` for the roadmap. Kept for the durable why/how; specifics
> below may be superseded.

A native, open-source, Medal.tv / ShadowPlay-style game clipper for Linux
(NVIDIA-first), designed cross-platform, built in Rust for the highest
achievable recording performance.

This document is the durable record of **why** this project exists and **how**
it is built. It is the source of truth; code follows it.

---

## 1. Why this exists (the problem chain)

The project was born from a concrete failure: **game recordings looked like
macroblocked garbage** — heavy blocky compression artifacts during Path of
Exile gameplay.

The culprit was traced step by step:

1. The bad clips came from **Steam's built-in Game Recording**.
2. On Linux + NVIDIA, Steam's recorder cannot initialize hardware encoding.
   Its own log shows the path:

   ```
   NVENC - No CUDA support
   Created encoder VAAPI for codec 4
   libav: libva: dlopen of .../nvidia_drv_video.so failed: wrong ELF class: ELFCLASS64
   libav: Failed to initialise VAAPI connection: -1 (unknown libva error).
   Created encoder X264 for codec 4
   Configuring encoder: [preset=veryfast][tune=film]
   >>> Capture method set to Game Vulkan NV12 + libx264 high (4 threads)
   ```

   Steam tries NVENC → fails inside its pressure-vessel container
   (`No CUDA support`) → tries VAAPI → fails (Steam is 32-bit and there is no
   working 32-bit NVIDIA VA-API driver: `wrong ELF class: ELFCLASS64`) → falls
   back to **CPU `libx264 veryfast`**. Software x264 at `veryfast` on a busy
   1440p scene is exactly the macroblocking observed.

3. Even if the VAAPI driver loaded, it would not help: **NVIDIA's VA-API driver
   is decode-only.** `vainfo` on this machine shows every profile with only
   `VAEntrypointVLD` (decode) and **zero `VAEntrypointEncSlice` (encode)**. So
   Steam's VAAPI encode fallback is structurally impossible on NVIDIA.

This is a widely reported, upstream Steam/NVIDIA limitation (NixOS/nixpkgs
#378346; ValveSoftware/steam-for-linux #11919, #11028, #11166; steam-runtime
#799), not a local misconfiguration. The full evidence is in
`docs/performance.md`.

**Conclusion:** Steam Game Recording cannot be made to hardware-encode on this
stack. The community answer is to stop using it and use a recorder that drives
NVENC directly. `gpu-screen-recorder` does this and works. We go one step
further and build our own, owning the pipeline end-to-end in Rust.

---

## 2. Why native Rust (the performance ceiling)

There are two tiers of "Rust solution":

- **Tier A — wrap `gpu-screen-recorder`** (supervise its process, drive it via
  signals). ~99% of optimal performance for ~10% of the effort. We don't own
  the engine.
- **Tier B — native in-process pipeline:** PipeWire DMA-BUF capture → zero-copy
  → NVENC, all inside our Rust process. The theoretical performance ceiling:
  one process, no second binary, no signal IPC, frames never leave the GPU.

We chose **Tier B (native from day one)** for maximum performance and full
ownership. The capture→encode path is dominated by zero-copy DMA-BUF→NVENC;
both tiers bottom out there, but Tier B removes every layer of indirection above
it and lets us control buffering, keyframe cadence, and latency precisely.

The foundation crate is **`waycap-rs`** (MIT): a high-level Wayland capture +
hardware-encode library doing copy-free DMA-BUF import and NVENC/VAAPI encoding,
extracted from the WayCap recorder. It hands us **already-encoded** frames over
a channel with keyframe flags. We build the product — ring buffer, save-last-N,
daemon, hotkeys, GUI, overlay — on top.

Verified present on the dev box (`station`): `libpipewire-0.3`, `libcuda.so.1`,
`libnvidia-encode.so.1`, ffmpeg libs; nixpkgs provides `pipewire`,
`ffmpeg-full`, the CUDA toolkit, `clang`, `libdrm`, `wayland-protocols`. RTX
5070 Ti NVENC confirmed healthy (the same NVENC path Steam cannot reach from its
container).

---

## 3. How it works (architecture)

A Cargo workspace; one crate per concern. Full diagrams in
`docs/architecture.md`.

```
ord-common   shared types + bincode IPC protocol (no I/O)
ord-core     waycap-rs wrapper + encoded-frame RING BUFFER + keyframe-aware
             "save last N seconds" muxer (ffmpeg-next, stream-copy)
ord-daemon   ordd: runs core, Unix-socket control plane,
             game detection (/proc + hyprctl), notifications
ord-cli      ord: thin socket client (save --last N, record toggle, status)
ord-overlay  Overlay trait + wlr-layer-shell / X11 / Win32 impls
ord-ui       egui: clip library window + click-through HUD
```

*(Superseded details: hotkeys shipped as compositor keybinds invoking `ord` —
no evdev in the daemon; the HUD shipped as the `ord-hud` binary in
`ord-overlay`, not in `ord-ui`; and an eighth crate, `ord-export` (pure
ffmpeg-arg export planning + runner), was added.)*

### The core idea: an encoded-frame ring buffer

`waycap-rs` emits **encoded** packets (not raw frames). We keep the last N
seconds of those packets in a bounded in-RAM ring buffer — tiny footprint, the
same mechanism as ShadowPlay's "instant replay." Saving a clip is:

1. Seek to the **newest keyframe ≤ N seconds back**.
2. Stream-copy from there to the buffer head into an `.mkv` (no re-encode).
3. Mux the matching audio. Result is instant and lossless.

This keyframe-seek boundary math is the highest-risk logic and is exhaustively
unit-tested (see `docs/testing.md`).

### Control plane

`ordd` exposes a Unix domain socket at `$XDG_RUNTIME_DIR/open-recorder.sock`.
`ord` (CLI) and `ord-ui` (GUI) send commands and receive events over the bincode
protocol in `ord-common`. Compositor keybinds call `ord save --last 30` etc.

### Global hotkeys

*(Superseded: shipped as compositor keybinds invoking `ord` — e.g.
`bind = ALT, R, exec, ord save --last 30` — no evdev.)* The original design had
`ordd` read `/dev/input` via evdev so the clip key fires even when a fullscreen
game grabs the keyboard; compositor binds proved sufficient and simpler.

### Encoding defaults

**H.264 by default** — the shipped capture path (`waycap_backend.rs`) records
H.264 NVENC, which the ring buffer + stream-copy muxer handle. HEVC (best
NVENC quality/size on the 5070 Ti, ideal for local editing) and AV1 (5070 Ti +
ffmpeg-full 8.1) **shipped in v0.2.0** via the `0xfell/waycap-rs` fork
(`capture.codec`), along with CBR bitrate control — hardware-verified. `.mkv`
container (crash-safe, editor-friendly). Audio: desktop output + optional mic.

---

## 4. The overlay strategy (and cross-platform stance)

"Overlay" is two different surfaces; see `docs/overlay.md` for the full design.

1. **Clip library / manager** — a normal window, shown on demand. On
   Hyprland/i3 the cleanest "overlay" is a **special workspace**: the same
   pattern already used for Discord/Spotify on this machine
   (`togglespecialworkspace`). No overlay code — a window rule + keybind
   (`special:clips`, `SUPER+C`). The compositor does the overlaying.

2. **HUD / on-screen feedback** — "buffer active", "Clip saved!" toasts that
   float over fullscreen games. This needs a true always-on-top, click-through,
   transparent surface:
   - **Wayland (Hyprland/Sway/KDE):** `wlr-layer-shell` `OVERLAY` layer — the
     same protocol waybar/swaync already use here.
   - **X11 (i3):** transparent always-on-top window + `set_cursor_hittest(false)`
     (needs a compositor like picom).
   - **Windows:** `WS_EX_LAYERED | WS_EX_TRANSPARENT` always-on-top.

**GUI toolkit: `egui`** — one toolkit for both surfaces. Chosen over `iced`
because egui has the mature, proven cross-platform overlay + click-through story
(`egui_overlay`, `egui_window_glfw_passthrough`) and is what the closest analog
(Lapse) uses. The library window can still live in a special workspace and look
fine.

**Cross-platform reality:** the UI/overlay layer is cross-platform from day one,
but the **capture/encode engine is Linux-only for now** (`waycap-rs` =
PipeWire/Wayland + NVENC/VAAPI). Windows needs a separate DXGI→NVENC backend —
designed for via the `CaptureBackend` trait, but a future milestone, not a v1
promise. We ship Linux-first.

---

## 5. Phased roadmap

> All phases below shipped by v0.2.2.

1. **Spike (hard gate).** Flake devshell + a throwaway binary running
   `waycap-rs` `CaptureBuilder` → confirm zero-copy DMA-BUF import + NVENC HEVC
   frames arrive on the **NVIDIA 610 open driver**. The whole project depends on
   this. If it struggles, fall back to a `portal` capture path or fork
   `waycap-rs`.
2. **`ord-core`.** Encoded-frame ring buffer + keyframe-aware save-last-N muxer
   + audio mux. CLI harness writes a clip end-to-end.
3. **`ord-daemon` + `ord-cli`.** Daemon, Unix-socket IPC, evdev global hotkeys,
   game-name filenames (`/proc` + `hyprctl activewindow`), swaync notifications.
4. **`ord-ui` library.** egui gallery: thumbnails (ffmpeg), play (mpv),
   **lossless trim** (ffmpeg stream-copy), rename/delete/reveal. Wire to the
   `special:clips` workspace.
5. **HUD overlay.** `ord-overlay` wlr-layer-shell click-through "buffer active /
   clip saved" HUD.
6. **NixOS wiring (separate, in the nixos config repo).** Flake input + Home
   Manager systemd user service for `ordd` + Hyprland binds/window-rules +
   docs.
7. **Release automation & distribution.** Automated versioning + changelog +
   GitHub Releases via **release-plz** (Conventional Commits → one shared
   workspace version → `vX.Y.Z` tag; nothing on crates.io). Three install
   paths so nobody has to compile: the **grok-insider cachix cache** for Nix/NixOS
   (closures pushed by `ci.yml`), a **static `ord` musl binary** for PATH
   installs, and **`ordd`/`ord-hud`/`ord-ui` AppImages** (`nix bundle`, with the
   flake wrapper resolving the host NVIDIA driver on foreign distros). A Flathub
   **Flatpak of `ord-ui`** is the planned next GUI path. See `docs/releasing.md`,
   `CONTRIBUTING.md`, and `continue-plan.md`.

---

## 6. Risks (named honestly)

> All resolved: the spike passed on the 610 open driver, `cust`/CUDA validated
> in the devshell, waycap-rs gaps were fixed in the `0xfell` fork, and NVENC is
> hardware-verified end-to-end.

- **NVIDIA 610 open driver + DMA-BUF / explicit sync.** `waycap-rs` is tested on
  older drivers; 610 is bleeding-edge. This is the top risk and is exactly why
  the Phase-1 spike gates everything. Mitigation: `portal` capture fallback;
  fork waycap-rs if needed (MIT).
- **`cust` 0.3 / CUDA 12.9 FFI skew.** Validate in the flake devshell before
  building on it.
- **`waycap-rs` maturity** (single maintainer, v2.1.x). MIT license makes
  forking/patching clean if we hit gaps.
- **Open kernel modules and NVENC.** NVENC works for direct-CUDA recorders here
  (unlike Steam's container); the spike confirms it for our path specifically.

---

## 7. Decisions log

| Decision | Choice | Why |
|----------|--------|-----|
| Language | Rust | Performance, safety, single-binary daemon/CLI, matches house stack. |
| Engine | native `waycap-rs` (Tier B) | Highest performance ceiling; full ownership; zero-copy DMA-BUF→NVENC. |
| Codec | H.264 default; HEVC/AV1 shipped (v0.2.0) | Shipped path is H.264 NVENC by default; HEVC/AV1 + CBR landed via the `0xfell/waycap-rs` fork (`capture.codec`), hardware-verified. |
| GUI | `egui` | Mature cross-platform overlay + click-through; one toolkit for HUD + library. |
| Library overlay | Hyprland special workspace | Reuses existing pattern (Discord/Spotify); no custom code. |
| HUD overlay | `wlr-layer-shell` (Wayland), X11/Win32 later | Native floating-over-fullscreen; same protocol as waybar/swaync. |
| Hotkeys | compositor keybinds invoking `ord` | (Superseded the evdev plan: no `/dev/input` reading shipped; compositor binds proved sufficient, even under fullscreen.) |
| License | MIT | Most permissive; matches `open-usage` and `waycap-rs`. |
| Platforms | Linux-first, cross-platform by design | Engine is Linux-only today; Windows = future `CaptureBackend`. |
| Versioning / release | release-plz (Conventional Commits) | One workspace version → `vX.Y.Z` tag + GitHub Release + regenerated changelog, no manual bumps; nothing on crates.io (`git_only`). |
| Nix distribution | grok-insider cachix cache | NixOS consumers substitute prebuilt closures; pushed by `ci.yml` on every master push/tag. |
| Non-Nix distribution | static `ord` (musl) + AppImages (`nix bundle`) | `ord` is pure Rust → portable static; the native ordd/ord-hud/ord-ui reuse the working flake build as AppImages. Flatpak (`ord-ui`) planned. |
| AppImage before Flatpak | AppImage first | Reuses the cached Nix build for ~free and suits our daemon/CLI/layer-shell model; Flatpak (OBS's official path) is deferred to the `ord-ui` GUI only. |
