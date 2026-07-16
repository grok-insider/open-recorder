# Changelog

All notable, user-facing changes to open-recorder are documented here. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## Unreleased

### Fixed
- NVENC constant bitrate actually applies (waycap-rs): CBR was ignored and AV1
  clips could land at ~1.5 Mbps mush regardless of `capture.bitrate_kbps`.

### Added
- Capture bitrate policy: recommended/minimum rates for res × fps × codec;
  daemon auto-raises CBR below the mush floor and logs the effective rate.
- Encode-health monitor: toasts when measured encode rate is far under the CBR
  target; `ord status` reports `encode` / target kbps.
- Settings UI seeds CBR from the recommended rate (not a flat 12 Mbps) and
  warns when the draft is below the floor.
- `ord doctor` flags recent clips with catastrophically low bitrates.

## 0.0.1

Initial public line of the open-recorder ShadowPlay-style stack for Linux:

- `ordd` daemon with PipeWire/NVENC capture via waycap-rs, ring buffer, and clips
- `ord` CLI, `ord-hud` overlay, and `ord-ui` clip library/editor
- Capture profiles, recording reliability, and export presets
- AppImage + static musl `ord` GitHub Release assets; Nix/Cachix packaging
- Not published to crates.io
