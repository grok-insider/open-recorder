# Architecture

How open-recorder is structured and how a frame travels from the GPU to a saved
clip.

## Crate graph

```
            +-------------+
            | ord-common  |  shared types + bincode IPC protocol (no I/O)
            +------+------+
                   ^
      +------------+------------+-------------------+
      |            |            |                   |
+-----+----+  +----+-----+  +---+------+      +-----+-----+
| ord-core |  | ord-cli  |  | ord-ui   |      | ord-overlay|
| engine   |  | client   |  | egui     |      | Overlay    |
+-----+----+  +----+-----+  +---+------+      +-----+------+
      ^             |            |                  |
      |             |            +------------------+
      |             v
+-----+-------------+------+
|       ord-daemon         |  ordd: runs core, socket, hotkeys, notifications
+--------------------------+
```

- `ord-common` has no dependencies on the others; everyone depends on it.
- `ord-core` depends only on `ord-common` (+ `waycap-rs`, `ffmpeg-next`).
- `ord-daemon` wires `ord-core` to the outside world.
- `ord-cli` / `ord-ui` are clients of the daemon socket; `ord-ui` also uses
  `ord-overlay` for the HUD.

## The capture → save dataflow

```
 Wayland compositor (Hyprland)
        │  DMA-BUF (GPU memory handle)
        ▼
 PipeWire screencast  ──(zero-copy import)──►  waycap-rs
        │                                          │  NVENC (in GPU)
        │                                          ▼
        │                                   encoded packets
        │                                   (+ keyframe flag)
        │                                          │  crossbeam channel
        ▼                                          ▼
 PipeWire audio  ──► Opus ──►        ord-core: ENCODED-FRAME RING BUFFER
                                     (bounded to N seconds, in RAM)
                                                   │
                              "save --last 30"     │
                                                   ▼
                          seek newest keyframe ≤ 30s back
                                                   │
                                                   ▼
                          ffmpeg-next stream-copy mux → clip.mkv
                          (video + audio, NO re-encode)
```

Key properties:

- **Frames never leave the GPU until encoded.** DMA-BUF import is zero-copy;
  NVENC encodes in place. Only compact encoded packets hit system RAM.
- **The ring buffer holds encoded packets**, not raw frames, so N seconds of
  1440p60 is megabytes, not gigabytes.
- **Saving is stream-copy**, not re-encode: instant and lossless. The only
  CPU work is muxing container boxes.

## Hot path vs. control plane

- **Hot path (latency-critical):** capture callback → channel → ring-buffer
  push. Lives entirely in `ord-core`. No panics, no per-frame allocation beyond
  the packet, no blocking.
- **Control plane (not latency-critical):** the daemon's Unix socket, CLI
  commands, hotkey events, notifications, and the GUI. A slow GUI or a CLI call
  must never stall capture.

## Processes

| Process | Crate | Role |
|---------|-------|------|
| `ordd`  | `ord-daemon` | Long-lived. Owns the capture session + ring buffer, listens on the socket, watches hotkeys. Started as a systemd user service. |
| `ord`   | `ord-cli`    | Short-lived. Sends one command to `ordd` and exits. Bound to compositor keys. |
| GUI     | `ord-ui`     | On-demand. Clip library window (special workspace) + HUD overlay. Talks to `ordd` over the socket. |

## IPC protocol (`ord-common`)

Bincode-encoded request/response + event stream over the Unix socket at
`$XDG_RUNTIME_DIR/open-recorder.sock`. Commands: `SaveLast(seconds)`,
`ToggleRecord`, `Status`, `BufferOn`/`BufferOff`, `SetQuality(...)`. Events:
`ClipSaved { path, duration }`, `BufferState(...)`, `Error(msg)`. Exact types
live in `ord-common` and are round-trip unit-tested.

## Configuration

User config at `~/.config/open-recorder/config.toml`: codec (hevc/av1), buffer
length, capture target (monitor name or `portal`), audio sources, clip output
directory (default `~/Videos/open-recorder`), hotkey bindings. The daemon writes
a default on first run.
