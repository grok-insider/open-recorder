# HDR capture — two-spike plan

HDR (10-bit, BT.2020 + PQ) is wired as a config surface (`capture.hdr`) and a
backend flag today, but true end-to-end HDR depends on two spikes. This file
records the plan and the load-bearing constraint behind it.

## The load-bearing constraint: the portal can't carry HDR

open-recorder captures exclusively through the **XDG ScreenCast portal** →
PipeWire (`ord-core/src/waycap_backend.rs`). gpu-screen-recorder's manual is
explicit that **HDR is not available on portal capture** — the portal path
tonemaps HDR to SDR. Real HDR needs a **direct KMS monitor capture** path
(gsr's `-w screen`, behind a small root/cap helper). So HDR is *not* "add
Main10 to the encoder"; it needs a new capture path. This is why
`capture.hdr` is validated (requires HEVC/AV1) but documented as dependent on
the KMS spike, and why the daemon rejects `hdr=true` with an H.264 codec.

## Spike A — 10-bit / HDR encode (waycap-rs fork)

Lower risk, reusable regardless of capture path; testable on 10-bit content even
when tonemapped.

- waycap-rs fork: HEVC/AV1 **Main10** NVENC, P010 frames.
- Color metadata: BT.2020 primaries, SMPTE 2084 (PQ) transfer into the
  bitstream and the container (mastering-display + content-light-level boxes).
- `ord-core/src/mux/bitstream.rs` already builds `hvcC`/`av1C` from the stream's
  parameter sets, so Main10 extradata follows the same path; the new work is the
  container color/mastering metadata on the muxer side.
- Golden test: `ffprobe` asserts 10-bit pixel format + `color_transfer=smpte2084`
  + `color_primaries=bt2020`.

## Spike B — KMS HDR capture backend (new `CaptureBackend`)

The hard, hardware-sensitive part; the only path to real end-to-end HDR.

- A new `CaptureBackend` impl that captures the HDR plane as a 10-bit DMA-BUF
  outside the portal (DRM master / a small privileged helper like
  `gsr-kms-server`), on Hyprland.
- Feeds the same encoded-frame channel as the waycap backend, so the ring
  buffer, clip selection, and muxer are unchanged.
- Decision gate: prove a 10-bit HDR DMA-BUF is obtainable on the dev box before
  committing; otherwise HDR stays "10-bit SDR-tonemapped via the portal".

## Status

- Config + validation + backend flag: **done** (`capture.hdr`).
- Spike A (Main10 encode + metadata): **pending** waycap-rs fork rev bump.
- Spike B (KMS capture): **pending** hardware spike (devshell + dev box).
