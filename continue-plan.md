# continue-plan.md

Outstanding work after **v0.3.0** — the single forward roadmap; the shipped
record is `docs/roadmap-status.md`. Everything here is either **gated** on the
`0xfell/waycap-rs` fork, needs **real hardware** to finish, or is in-repo work
queued behind it. The shipped surface (config, validation, IPC, pure logic,
cross-platform Phase 0) is in place and tested; the items below make it *take
effect* or extend coverage.

Status legend: ⛔ blocked on external fork · 🔬 needs real-hardware spike ·
🟡 in-repo work · 💤 deferred/optional.

---

## 1. waycap-rs fork rev bump — the linchpin ⛔🟡

Phase-1 capture knobs and Phase-7 HDR are wired end-to-end through config + the
`WaycapBackend` builder, but the pinned fork rev doesn't expose the matching
`CaptureBuilder` setters yet, so they're recorded + logged, not applied. See the
`// fork:` `tracing::debug!` block in `crates/ord-core/src/waycap_backend.rs`.

Fork work (in `github.com/0xfell/waycap-rs` — the fork intentionally stays
there post-migration), then bump the rev in **both**
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

release-plz is live (it cut v0.3.0 via PRs #2/#3) and `ci.yml`'s build→cachix
lane runs on every master push. Outstanding:

- [ ] Validate the **AppImages** (`ordd`/`ord-hud`/`ord-ui`) on **non-NixOS
      NVIDIA** hardware — ordd first; `ord-ui`'s GL path is the known hazard
      (foreign-distro driver resolution via the flake `postFixup`).

## 8. Cross-platform Phase 1 🟡

Phase 0 (v0.3.0) made the workspace compile everywhere with the mock backend.
Phase 1 turns the seams into real platform work:

- [ ] **Windows transport decision:** named pipes vs the current loopback
      TCP + port-rendezvous file (`ord-common/src/transport.rs`). The TCP path
      works today; named pipes would restore filesystem-permission gating.
      Decide before any Windows daemon ships.
- [ ] File follow-up issues for the real capture backends behind
      `CaptureBackend`: **macOS** (ScreenCaptureKit + VideoToolbox) and
      **Windows** (WGC/DXGI + NVENC).
- [ ] Add a CI `cargo check --target x86_64-pc-windows-gnu` lane so the
      off-Linux build can't silently rot between releases.

## 9. Share / upload 🟡💤

Absorbed from the retired `future-features.md`: a **share-link (upload)** flow —
upload the clip (or a Discord-sized export) to a host and copy a URL to the
clipboard. Needs a destination decision (self-hosted vs. a service) and
auth/secret handling. Clipboard **copy-as-file** (`wl-copy text/uri-list`)
already shipped in v0.2. Sketched shape when revisited: `share.rs` (upload/link)
wired as an extra action on each clip card. Do not implement without an
explicit ask.

## 10. Misc / polish 💤

- [ ] Player stall-recovery threshold (`STALL_PAUSE = 2s` in `player.rs`) is a
      judgment call — revisit once a real audio-device stall is observed in the
      wild; consider a gentle "nudge the stream" retry before pausing.
- [ ] Add timestamps to the `debug_tick` telemetry lines (currently unstamped) so
      they correlate with `diag::log_line` entries.
- [ ] Vulkan video-encode path (non-CUDA, avoids the P2 issue on <580 drivers) —
      only if older-driver support becomes a goal. Deferred.

---

## Code-audit backlog (2026-07-02)

From a full-workspace audit. Grouped by concern; file:line refs are as of the
audit.

### Reliability

- [ ] Non-blocking subscriber broadcast (`server.rs:69`) — the pump thread must
      never block on a slow subscriber.
- [ ] Bound `Recorder::pending_audio` post-start (`record.rs:159`).
- [ ] Async/timeboxed disk compaction + injectable threshold
      (`disk_store.rs:139`; contract in `store.rs:34`).
- [ ] Drop-until-next-keyframe on forwarder overflow (`waycap_backend.rs:314`).
- [ ] `apply_config` engine start outside the handler/ctx locks (`server.rs:436`).
- [ ] `hyprctl` timeout + injectable game probe (`server.rs:153`).
- [ ] VFR watchdog gate (`framerate_mode = content` must not trip the no-frames
      watchdog on a static screen).
- [ ] Atomic overrides write (ordd `main.rs:406`).
- [ ] Collision-proof `output_path` (same-second saves must not overwrite).
- [ ] Prune the recordings dir (recordings are exempt today by policy — decide
      and enforce deliberately).
- [ ] Transport stale-socket/port-file hygiene (`transport.rs:46`).
- [ ] Surface disk write-failure counters into `Status`.

### Performance

- [ ] Coalesced `DiskFrameStore::window` reads (one positioned read per span,
      not per frame).
- [ ] Back-scan out-of-order inserts (ring/disk/audio) instead of front scans.
- [ ] `Packet::borrow` instead of `Packet::copy` on the save path.
- [ ] µs stream time base instead of ms (precision on long buffers).
- [ ] Incremental library refresh (`app.rs:378`).
- [ ] `filter_sort` cache (`app.rs:806`).
- [ ] Frame-buffer pool in the decode path (`player.rs:1255`).
- [ ] Hoist the settings `overridden()` computation out of the per-frame path.

### UX

- [ ] Sub-second editor time display.
- [ ] Exports surfaced in the library.
- [ ] Tab-nav restoration (`editor.rs:257`) + AccessKit on stepper/timeline
      (pairs with item 4).
- [ ] Library keyboard navigation.
- [ ] Persist editor volume/loop.
- [ ] `ord config set`.
- [ ] `--json` status output.
- [ ] `RecordState` stop-path reporting (protocol v5).
- [ ] Subscribe reconnect (client-side backoff instead of a dead stream).
- [ ] HiDPI HUD (`layershell.rs:603`).

### Testing

- [ ] Offline `execute()` fallback tests (fake ffmpeg via `ORD_FFMPEG`).
- [ ] Disk compaction tests.
- [ ] Editor math extraction into `timeline.rs` + tests.
- [ ] CLI `parse()` tests.
- [ ] `ord-hud` `apply()` tests.
- [ ] Auto-arm integration test via an injected probe.
- [ ] HEVC sub-layer PTL fixture.

### Hygiene

- [ ] Shared `Rebaser` for `mux.rs`/`record.rs`.
- [ ] Dedupe the `access_unit()` fixture (duplicated x4).
- [ ] `MonitorId` adoption or deletion.
- [ ] `av1C` 10-bit loud failure (no silent 8-bit assumption).
- [ ] Tick-vs-µs comment drift (`ring.rs:30`, `clip.rs:31`).
- [ ] Theme-token cleanup (`app.rs:1035`, raw `Color32`s in `editor.rs`).

**In flight as of 2026-07-02:** a working session (Stages 2–7) is fixing a
first slice of this backlog — starting with the Testing group's offline
`execute()` fallback tests (fake ffmpeg via `ORD_FFMPEG`, in `ord-export`) and
continuing through the Reliability/Performance items above. Check `git log`
before picking an item up; some may already be done.

---

## Suggested order

1. **waycap-rs fork bump (item 1)** — unlocks all Phase-1 knobs at once, smallest
   surgical fork change, immediately user-visible.
2. **Audio subsystem (item 2)** — biggest feature gap (separate/per-app tracks).
3. **AccessKit (item 4)** — unblocks proper UI QA and accessibility together.
4. **HDR spikes (item 3)** — largest/riskiest; gate on Spike B feasibility.
5. Perf measurement + test coverage (items 5–6) and the code-audit backlog
   alongside the above.
