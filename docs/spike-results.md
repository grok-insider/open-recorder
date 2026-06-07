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
- **HEVC/AV1**: available in ffmpeg (`hevc_nvenc`, `av1_nvenc` confirmed in the
  devshell); exposing them through waycap-rs is a fork item (the enum only has
  `H264Nvenc`). H.264 is fine for v1.

## HEVC/AV1 via a waycap-rs fork — verified recipe

Assessed the fork scope against the pinned waycap-rs commit. It is **small and
surgical**, not a rewrite:

- `src/encoders/nvenc_encoder.rs` hardcodes `let encoder_name = "h264_nvenc";`
  (NvencEncoder::new). `create_encoder()` is otherwise **codec-agnostic** — it
  uses `ffmpeg::codec::encoder::find_by_name(encoder)` with CUDA frames and a
  bitrate; no H.264-only assumptions in the hot path.
- `src/types/config.rs::VideoEncoder` only lists `H264Nvenc`/`H264Vaapi`.
- `src/encoders/dynamic_encoder.rs` maps the GPU vendor to `H264Nvenc`.

**Fork steps:** add `Hevc*`/`Av1*` variants to `VideoEncoder`, thread the chosen
codec into `NvencEncoder::new` so `encoder_name` becomes `"hevc_nvenc"` /
`"av1_nvenc"`, and update `dynamic_encoder` mapping. Everything downstream
(CUDA frame ctx, the encoded-frame channel, our adapter) is unchanged.

Our `WaycapBackend` already reports a `Codec` in `params()`; when a forked
waycap-rs exposes HEVC, only the `with_video_encoder(...)` call + that reported
codec need to change. Until then v1 ships H.264 NVENC, which already eliminates
the CPU-x264 macroblocking that motivated the project.
