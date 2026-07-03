# Architecture

How open-recorder is structured and how a frame travels from the GPU to a saved
clip.

## Crate graph

```
            +-------------+
            | ord-common  |  shared types + bincode IPC + transport seam
            +------+------+
                   ^
      +------------+-----------+--------------+--------------+
      |            |           |              |              |
+-----+----+  +----+-----+  +--+-------+  +---+--------+  +--+---------+
| ord-core |  | ord-cli  |  | ord-ui   |  | ord-overlay|  | ord-export |
| engine   |  | client   |  | egui     |  | Overlay +  |  | ffmpeg-arg |
+-----+----+  +----------+  +----------+  | ord-hud    |  | planner    |
      ^                                   +------------+  +------------+
      |
+-----+--------------------+
|       ord-daemon         |  ordd: runs core, transport listener, notifications
+--------------------------+
```

- `ord-common` has no dependencies on the others; everyone depends on it.
- `ord-core` depends only on `ord-common` (+ `waycap-rs`, `ffmpeg-next`).
- `ord-daemon` wires `ord-core` to the outside world.
- `ord-cli` / `ord-ui` / `ord-hud` are clients of the daemon transport. The HUD
  is the separate `ord-hud` binary shipped by `ord-overlay` (which also owns
  the `Overlay` trait); `ord-ui` does not depend on `ord-overlay`.
- `ord-export` (pure ffmpeg-arg planner + ffprobe/ffmpeg runner) is used by
  `ord-cli`'s `ord export` and by `ord-ui`'s in-app editor export.

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
- **Control plane (not latency-critical):** the daemon's control transport, CLI
  commands, notifications, and the GUI. A slow GUI or a CLI call must never
  stall capture.

## Processes

| Process | Crate | Role |
|---------|-------|------|
| `ordd`  | `ord-daemon` | Long-lived. Owns the capture session + ring buffer, listens on the control transport. Started as a systemd user service. Hotkeys are compositor keybinds invoking `ord`, not daemon code. **Startup is socket-first**: the control transport binds before capture starts, and every capture start/restart runs on the *capture supervisor* thread (`supervisor.rs`) — a screen-share portal request can block indefinitely, so it must never run under the handler lock, on the pump thread, or ahead of the socket. A failed initial arm retries on a bounded schedule (login race) and otherwise leaves the daemon reachable-but-degraded; supervisor restarts adopt the old engine's replay state, so recovery never discards buffered footage. |
| `ord`   | `ord-cli`    | Short-lived. Sends one command to `ordd` and exits. Bound to compositor keys. |
| GUI     | `ord-ui`     | On-demand. Clip library window (special workspace) with the inline player/editor. Talks to `ordd` over the transport. |
| `ord-hud` | `ord-overlay` | Long-lived. The click-through wlr-layer-shell HUD; subscribes to daemon events over the transport. |

## IPC protocol (`ord-common`)

Bincode-encoded request/response + event stream over the transport seam
(`ord-common/src/transport.rs`): on unix, a Unix domain socket at
`open-recorder.sock` in the `dirs`-resolved runtime directory; off-unix, a
loopback (`127.0.0.1`) TCP connection whose ephemeral port the daemon publishes
in a rendezvous file at the same path. Commands: `SaveLast { duration }`,
`ToggleRecord`, `SetBuffer { enabled }`, `Status`, `Subscribe`, `Mark`,
`Screenshot`, `GetConfig`, `SetConfig`. Events: `ClipSaved { path, duration }`,
`BufferState`, `RecordState`, `Status { .. }`, `Marked`, `CaptureRestarted`,
`ScreenshotSaved { path }`, `Config { .. }`, `Error(msg)`. Exact types live in
`ord-common` and are round-trip unit-tested.

## Configuration

Config is **layered**: the base `~/.config/open-recorder/config.toml` (often a
read-only Home Manager symlink) is never modified at runtime; settings changes
persist as a sparse diff in `$XDG_STATE_HOME/open-recorder/overrides.toml`,
written only by `ordd` via `SetConfig`. The daemon writes a default base on
first run. Sections (`ord-common/src/config.rs`):

- `[capture]` (`CaptureConfig`) — fps, `buffer_seconds`, quality, codec
  (default **h264**; hevc/av1 supported), bitrate, resolution, keyframe
  interval, framerate mode, color range, tune, `replay_storage` (ram/disk),
  target (monitor name or `portal`), `auto_arm`, hdr, `clear_on_save`.
- `[audio]` (`AudioConfig`) — desktop/mic booleans or per-application `tracks`.
- `[storage]` (`StorageConfig`) — clips/recordings dirs (default
  `~/Videos/open-recorder`), filename template, auto-prune limits.
- `[markers]` (`MarkersConfig`) — `auto_save_seconds`.
- `[overlay]` (`OverlayConfig`) — `show_status_dot`.
- `[hooks]` (`HooksConfig`) — `on_clip_saved`.
- `[export]` (`ExportConfig`) — `ord export` defaults.

There is no hotkey config: hotkeys are compositor keybinds invoking `ord`.
