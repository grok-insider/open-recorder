# Changelog

All notable, user-facing changes to open-recorder are documented here. The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.0.1

Initial public line of the open-recorder ShadowPlay-style stack for Linux:

- `ordd` daemon with PipeWire/NVENC capture via waycap-rs, ring buffer, and clips
- `ord` CLI, `ord-hud` overlay, and `ord-ui` clip library/editor
- Capture profiles, recording reliability, and export presets
- AppImage + static musl `ord` GitHub Release assets; Nix/Cachix packaging
- Not published to crates.io
