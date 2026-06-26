# continue-plan.md

Outstanding work after `v0.2.2` — the canonical forward plan. Everything here is
either **gated** on the `0xfell/waycap-rs` fork, needs **real hardware** to
finish, or was **noted but not changed** during QA. The shipped surface (config,
validation, IPC, pure logic) is in place and tested; the items below make it
*take effect* or extend coverage. For what has already shipped, see
`docs/roadmap-status.md`.

Status legend: ⛔ blocked on external fork · 🔬 needs real-hardware spike ·
🟡 in-repo work · 💤 deferred/optional.

---

## 1. waycap-rs fork rev bump — the linchpin ⛔🟡

Phase-1 capture knobs and Phase-7 HDR are wired end-to-end through config + the
`WaycapBackend` builder, but the pinned fork rev doesn't expose the matching
`CaptureBuilder` setters yet, so they're recorded + logged, not applied. See the
`// fork:` `tracing::debug!` block in `crates/ord-core/src/waycap_backend.rs`.

Fork work (in `github.com/0xfell/waycap-rs`), then bump the rev in **both**
`crates/ord-core/Cargo.toml` (the `waycap-rs` git dep) and `flake.nix`
`outputHashes`, and re-vendor the NAR hashes (per AGENTS.md):

- [ ] `with_gop`/keyframe-interval → honor `capture.keyframe_interval_ms`.
- [ ] framerate mode (CFR/VFR/content-sync) → honor `capture.framerate_mode`.
- [ ] color range (limited/full) → honor `capture.color_range`.
- [ ] encoder tune (performance/quality) → honor `capture.tune`.
- [ ] capture scaling / `with_dimensions` on the builder → make
      `capture.resolution` a real downscale (today it's only a container hint).
- [ ] named-monitor direct capture → make `capture.target = <monitor>` work
      (today any non-`portal` value falls back to the portal).

After the bump: replace the `// fork:` log block with the real builder calls and
add a real-hardware `#[ignore]` test per knob (devshell, dev box).

## 2. Audio subsystem — live PipeWire + Opus capture 🟡

The **config model** (`audio.tracks`, per-app `AudioSource` selectors) and the
**pure routing** (`crates/ord-core/src/audio_route.rs::plan_track`, +
`config.rs::effective_tracks`) are done and unit-tested. The **live capture
engine** that consumes them is not — the daemon still uses waycap's single mixed
Opus track.

- [ ] New `ord-core` PipeWire audio module (behind a `pwaudio` feature): connect
      a PipeWire client (align on the workspace's forked `pipewire-rs`, branch
      `metas` — don't pull a second libspa version), enumerate nodes
      (→ `ord audio list-apps`), create one virtual null sink per track, link the
      `plan_track` node ids in, relink as apps open/close streams (OBS's
      `autoconnect_targets` pattern).
- [ ] Encode each track's monitor to Opus (reuse `ffmpeg-next` libopus + the
      existing `build_opus_head`) → one `EncodedAudioFrame` receiver per track.
- [ ] Multi-track downstream: `PreparedClip.audio`/`AudioParams` → `Vec`;
      `mux/stream.rs::add_audio_stream` called per track; `Recorder`
      (`record.rs`) + clip muxer (`mux.rs`) handle N audio indices; per-track
      `AudioRingBuffer`. Golden test: ffprobe asserts N audio tracks.
- [ ] Make waycap video-only (stop requesting its audio) once this lands.
- [ ] `ord audio list-apps` CLI + Status reporting the track layout.

Risk: high (stateful async PipeWire graph). Land single-track-via-ord-core first
(parity), then add routing/per-app.

## 3. HDR — two spikes 🔬⛔

Config + validation shipped (`capture.hdr`, requires HEVC/AV1). Plan in
`docs/hdr.md`. The portal can't carry HDR, so both spikes are needed:

- [ ] **Spike A (fork):** HEVC/AV1 **Main10** NVENC + P010 frames + BT.2020/PQ
      color metadata into `hvcC`/`av1C` and the container (mastering-display +
      CLL). Golden: ffprobe `color_transfer=smpte2084`, `color_primaries=bt2020`,
      10-bit pixfmt.
- [ ] **Spike B (hardware):** a new `CaptureBackend` impl capturing the HDR plane
      as a 10-bit DMA-BUF **outside the portal** (KMS/DRM master, small
      privileged helper) on Hyprland. Decision gate: prove a 10-bit HDR DMA-BUF
      is obtainable before committing.

## 4. Accessibility — enable AccessKit 🟡

egui exposes **no AT-SPI tree** today: screen readers can't use the UI, and
automation (wisp `gui_click`/keyboard) can't target widgets — this is why QA had
to lean on `ORD_AUTOPLAY` + headless paths.

- [ ] Enable eframe/egui's `accesskit` integration in `crates/ord-ui`
      (`NativeOptions`/feature). Verify with a screen reader and re-attempt
      wisp-driven editing.

## 5. QA / test coverage gaps 🟡

- [ ] **Interactive editor coverage** (trim `I/O`, split `S`, cut `X`, join,
      markers `M`, loop, scrub): couldn't be driven via wisp (no a11y tree +
      keyboard not routed in the nested sandbox). After item 4, re-drive via
      wisp; until then add an editor-model integration test harness that
      exercises `timeline`/`segments`/`export_segments` without egui.
- [ ] **Per-app audio routing** integration test (against a mock/real PipeWire)
      once item 2 lands — only `plan_track` is unit-tested today.
- [ ] **Real-NVENC `#[ignore]` lane**: extend for HEVC/AV1/HDR/the new knobs when
      the fork lands (runs on the dev box, `--features waycap -- --ignored`).
- [ ] **Loop playback** end-to-end check (set loop on + `ORD_AUTOPLAY`, confirm
      clean wrap over several cycles) — only verified single-shot EOF so far.

## 6. Performance — actually measure it 🔬

The "ShadowPlay parity" claim is still architectural, not measured.

- [ ] Quantify capture GPU/CPU overhead on the dev box: `nvidia-smi dmon` +
      in-game fps delta with the buffer armed vs off; compare P0 vs P2 with/without
      the `ord doctor --fix` profile (Phase 0's real win).
- [ ] Run the existing `criterion` benches (`ord-core/benches`) in CI-comparable
      conditions to guard ring-push + save-path latency regressions.

## 7. Packaging & release 🟡

`release.yml` (release-plz) + `ci.yml` (3 jobs incl. build→cachix) are wired, and
v0.2.0–v0.2.2 were cut as manual SemVer tags. Outstanding:

- [ ] Enable the repo's **"Allow GitHub Actions to create and approve pull
      requests"** setting so release-plz can open its `chore: release` PRs (until
      then, bump + tag manually per `docs/releasing.md`).
- [ ] Validate the **AppImages** (`ordd`/`ord-hud`/`ord-ui`) on **non-NixOS
      NVIDIA** hardware — ordd first; `ord-ui`'s GL path is the known hazard
      (foreign-distro driver resolution via the flake `postFixup`).
- [ ] Reconcile manual tags with release-plz's version tracking (it may propose
      the next bump from commits since the last release).

## 8. Misc / polish 💤

- [ ] Player stall-recovery threshold (`STALL_PAUSE = 2s` in `player.rs`) is a
      judgment call — revisit once a real audio-device stall is observed in the
      wild; consider a gentle "nudge the stream" retry before pausing.
- [ ] Add timestamps to the `debug_tick` telemetry lines (currently unstamped) so
      they correlate with `diag::log_line` entries.
- [ ] Vulkan video-encode path (non-CUDA, avoids the P2 issue on <580 drivers) —
      only if older-driver support becomes a goal. Deferred.

---

## Suggested order

1. **waycap-rs fork bump (item 1)** — unlocks all Phase-1 knobs at once, smallest
   surgical fork change, immediately user-visible.
2. **Audio subsystem (item 2)** — biggest feature gap (separate/per-app tracks).
3. **AccessKit (item 4)** — unblocks proper UI QA and accessibility together.
4. **HDR spikes (item 3)** — largest/riskiest; gate on Spike B feasibility.
5. Perf measurement + test coverage (items 5–6) alongside the above.
