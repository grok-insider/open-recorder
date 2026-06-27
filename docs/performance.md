# Performance & the Steam-on-NVIDIA diagnosis

Why open-recorder exists, why it is native zero-copy, and the evidence behind
the decision.

## The trigger

Game clips looked macroblocked — heavy blocky H.264 compression artifacts during
gameplay. The clips came from **Steam's built-in Game Recording**.

## Diagnosis (reproduced on the dev box, NVIDIA RTX 5070 Ti, open driver 610)

### 1. Steam falls back to CPU x264

Steam's `~/.local/share/Steam/logs/streaming_log.txt`:

```
Trying to create an encoder for recording: [hardware_enabled=true]
NVENC - No CUDA support
Created encoder VAAPI for codec 4
libav: libva: dlopen of /run/opengl-driver/lib/dri/nvidia_drv_video.so failed:
       /run/opengl-driver/lib/dri/nvidia_drv_video.so: wrong ELF class: ELFCLASS64
libav: Failed to initialise VAAPI connection: -1 (unknown libva error).
Created encoder X264 for codec 4
Configuring encoder: [threads=4][width=2560][height=1440][preset=veryfast][tune=film]
>>> Capture method set to Game Vulkan NV12 + libx264 high (4 threads)
```

The chain: NVENC fails to init inside Steam's pressure-vessel container
(`No CUDA support`) → VAAPI fallback fails (Steam is 32-bit; the NVIDIA VA-API
driver present is 64-bit → `wrong ELF class: ELFCLASS64`) → **CPU `libx264
veryfast`**. Software x264 at `veryfast` on a busy 1440p scene is exactly the
macroblocking observed.

### 2. NVIDIA VA-API cannot encode anyway

`vainfo` against the NVIDIA backend on this machine:

```
vainfo: Driver version: VA-API NVDEC driver [direct backend]
  VAProfileH264Main               : VAEntrypointVLD
  VAProfileH264High               : VAEntrypointVLD
  VAProfileHEVCMain               : VAEntrypointVLD
  ... (every profile) ...          : VAEntrypointVLD
```

Every profile exposes only `VAEntrypointVLD` (**decode**). There is **no**
`VAEntrypointEncSlice` (**encode**) anywhere. NVIDIA's VA-API driver is the
"NVDEC driver" — decode-only by design. So Steam's VAAPI **encode** fallback is
structurally impossible on NVIDIA, with or without a 32-bit driver.

### 3. The hardware is fine

`nvidia-smi` confirms the NVENC encoder is present and healthy; `libcuda.so.1`
and `libnvidia-encode.so.1` exist on the system. Recorders that drive **NVENC
directly via CUDA** (e.g. gpu-screen-recorder) work perfectly. The problem is
specific to Steam's containerized VAAPI/NVENC path, not the GPU.

## This is upstream and widely reported

Not a local misconfiguration:

- NixOS/nixpkgs **#378346** — same `ELFCLASS64` → x264 fallback; closed "not a
  NixOS bug."
- ValveSoftware/steam-for-linux **#11919** (RTX 5070, same gen), **#11028**,
  **#11166** — `NVENC - No CUDA support` / VAAPI fail / x264.
- ValveSoftware/steam-runtime **#799** — Valve dev confirms pressure-vessel does
  not reliably capture `libnvidia-encode.so.1`, and that VA-API fallback "won't
  normally work on Nvidia systems."

## Why native zero-copy is the answer

Steam's recorder is unfixable on this stack. The performant path is to drive
NVENC directly, outside any container:

```
PipeWire DMA-BUF capture  →  (zero-copy import)  →  NVENC  →  encoded packets
```

Frames stay in GPU memory until encoded; only compact packets reach RAM. This is
the same class of pipeline gpu-screen-recorder uses (and far smoother than OBS
near 100% GPU). open-recorder owns this pipeline in-process via `waycap-rs`,
removing every layer of indirection.

## Performance design rules (enforced in code; see AGENTS.md)

- **Encoded-frame ring buffer**, not raw frames — N seconds of 1440p60 is
  megabytes.
- **Save = stream-copy from newest keyframe** — instant, lossless, no re-encode.
- **Hot path: no panics, no per-frame copies, no allocation churn.**
- **H.264 default**; HEVC (best NVENC quality/size on the 5070 Ti) and AV1 are
  wired end-to-end (select via `capture.codec`), encoded through the pinned
  `0xfell/waycap-rs` fork.
- Bench the ring-buffer push and save-path latency (`criterion`) to catch
  regressions.

## Verified toolchain on the dev box

`libpipewire-0.3`, `libcuda.so.1`, `libnvidia-encode.so.1`, ffmpeg libs present;
nixpkgs provides `pipewire 1.6.5`, `ffmpeg-full 8.1`, CUDA 12.9 toolkit,
`clang 21`, `libdrm`, `wayland-protocols`. RTX 5070 Ti NVENC confirmed.
