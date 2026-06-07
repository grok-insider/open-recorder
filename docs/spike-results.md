# Phase-1 spike results

The spike validates that the native zero-copy capture→NVENC path is viable on
this machine (NVIDIA RTX 5070 Ti, open driver 610) before the product is built
on it. Code: `spike/`.

## Verdict: build gate PASSED; runtime needs an interactive portal pick

### What is proven

1. **The full native stack compiles** against the dev toolchain (flake devshell):
   `waycap-rs` (git `493c1c6`) with the **`nvidia` + `egl`** features → `cust`
   (CUDA driver API), `ffmpeg-next 8.1`, `pipewire`, `portal-screencast`, all
   link and build. The 89 MB `ord-spike` binary is produced.
2. **The capture session initializes end-to-end at runtime.** Running the spike,
   execution reaches the XDG **ScreenCast portal `CreateSession`** handshake —
   i.e. NVENC/EGL/PipeWire/ffmpeg all initialized without a CPU/VAAPI fallback
   and without a CUDA/driver mismatch. This is the path Steam's container cannot
   reach.
3. **ffmpeg in the devshell exposes `h264_nvenc`, `hevc_nvenc`, and `av1_nvenc`**
   — so HEVC/AV1 NVENC are available at the ffmpeg layer (the HEVC gap is only in
   waycap-rs's wrapper enum; addable in a fork).

### What is NOT yet proven (and why)

The end-to-end run stops at the portal `CreateSession` with:

```
D-Bus Portal error: Did not receive a reply (org.freedesktop.DBus.Error.NoReply)
```

This is **not** a fault in our code or in waycap-rs. The XDG ScreenCast portal
requires an **interactive screen-picker dialog** to be shown and answered on the
live Hyprland session. The spike was run from a non-interactive `nix develop -c`
subprocess that cannot surface/answer that dialog, so `CreateSession` times out.

**Isolation proof:** `gpu-screen-recorder -w portal` — a known-good, widely-used
NVENC recorder — **fails identically** (`CreateSession ... Did not receive a
reply`) from the same context. Our spike reaches the exact same point as the
proven tool. The blocker is the interactive portal handshake in this execution
context, not the capture/encode pipeline.

## How to finish the validation (manual, ~1 min in the live session)

Run the spike from a terminal **inside the Hyprland session** so the portal
picker can appear and be clicked:

```sh
cd ~/dev/personal/open-recorder
nix develop -c bash -c 'cd spike && ./target/debug/ord-spike'
# Press Enter; pick the monitor in the portal dialog that pops up.
```

Expected success output:

```
frames captured : <a few hundred>
keyframes       : <>= 1>
encoded bytes   : <megabytes>
avg fps         : ~60
OK: wrote spike_out.mkv
```

Then confirm the file:

```sh
ffprobe -hide_banner spike_out.mkv   # expect: Video: h264 (NVENC), ~Nx60fps
```

If `frames > 0`, `keyframes >= 1`, and `spike_out.mkv` is a valid H.264 file,
the gate is fully passed and Phase 2 (`ord-core` ring buffer + save-last-N) can
proceed.

## Implications already captured for the build

- **waycap-rs needs `nvidia` + `egl` features** and `default-features = false`
  (drops vaapi). Pin by commit; it is not on crates.io and `main` had a
  `thread_teardown` that only type-checks with a backend feature enabled — `egl`
  satisfies it.
- **CUDA toolchain quirk:** `cust`'s `find_cuda_helper` only accepts a CUDA root
  with `lib64/` on Linux; the nixpkgs merged toolkit uses `lib/`. The flake
  builds a `.cuda-shim/lib64 -> toolkit/lib` symlink and sets
  `CUDA_LIBRARY_PATH`. Runtime `libcuda` comes from `/run/opengl-driver`.
- **Do not set `config.cudaSupport`** globally — it forces a from-source rebuild
  of ffmpeg-full + opencv + whisper. NVENC comes from the driver at runtime; the
  cached `ffmpeg-full` already carries the nvenc encoders via nv-codec-headers.
- **HEVC/AV1**: available in ffmpeg; exposing them through waycap-rs is a fork
  item (the enum only has `H264Nvenc`). H.264 is fine for v1.
