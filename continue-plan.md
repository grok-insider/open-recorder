# continue-plan.md

Outstanding work after **v0.3.0** — the single forward roadmap; the shipped
record is `docs/roadmap-status.md`. Everything here is either **gated** on the
`grok-insider/waycap-rs` fork, needs **real hardware** to finish, or is in-repo work
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

Fork work (in `github.com/grok-insider/waycap-rs` — migrated there 2026-07-03
after the original `0xfell/waycap-rs` repo vanished and broke every fetch;
the local source of truth is `~/dev/personal/waycap-rs`), then bump the rev in
**both**
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
- [ ] bound waycap's internal buffering under consumer stall: during a
      system-wide memory squeeze (observed 2026-07-03: a local nix build
      swapping the box), waycap's own channels ballooned to ~235 MB in ~5 s
      of "Could not send video frame ... Channel full" and drew the kernel
      OOM killer. Our forwarder/ring stay bounded; the growth is inside the
      fork — cap or drop there too.
- [ ] a timeout/cancel handle on the portal ScreenCast request — the D-Bus
      call can hang indefinitely (observed after a portal wedge); ordd's
      capture supervisor contains the hang, but only the fork can abort it.

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
- [x] Release PRs re-automated (2026-07-02) with a self-owned `release-pr` job
      (patch-line bumps + AI changelog) replacing the upstream-broken
      `release-plz release-pr`; deliberate minor/major milestones go through
      `manual-version-bump.yml` (open-media's model). `release-plz release`
      still cuts the tag/Release. If upstream ever fixes git-only change
      detection (release-plz/release-plz#2651), switching back is optional,
      not necessary.

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
audit. **Same-day working session (2026-07-02) closed most of this backlog** —
checked items landed on `docs/readme-refresh` (see `git log` for the
`fix(daemon)`/`fix(core)`/`fix(export)`/`feat(cli)!`/`feat(ui)`/`refactor(core)`
commits); only the unchecked items remain open.

### Reliability

- [x] Non-blocking subscriber broadcast (`server.rs:69`) — bounded per-subscriber
      queue + writer thread; a stuck subscriber is dropped, never blocks the pump.
- [x] Bound `Recorder::pending_audio` post-start (`record.rs:159`) + stall test.
- [x] Incremental disk compaction (bounded per-push budget) + injectable
      threshold + tests (`disk_store.rs`).
- [x] Drop-until-next-keyframe on forwarder overflow (`waycap_backend.rs:314`);
      audio drops now counted/logged too.
- [x] `apply_config` engine start outside the handler/ctx locks; screenshots
      decode under their own lock.
- [x] `hyprctl` hard timeout (kill after 2 s) + injectable `game_probe` in
      `ServerCtx`; auto-arm gained auto-disarm (~1 min after the game exits).
- [x] VFR watchdog gate (`framerate_mode = content` stands the watchdog down).
- [x] Atomic overrides write (temp + rename).
- [x] Collision-proof `output_path` (`-1`/`-2` suffixes).
- [x] Prune covers the recordings dir (shared budget, 5 min fresh-file grace).
- [x] Transport hygiene: atomic port-file write; stale-socket ownership
      documented on the seam.
- [ ] Surface disk write-failure counters into `Status` — the counter exists
      (`DiskFrameStore::write_errors()`), the `Status` event field does not yet
      (fold into the next protocol bump; v5 just shipped).

### Performance

- [x] Coalesced `DiskFrameStore::window` reads (adjacent payloads read once,
      zero-copy `Bytes` slices out).
- [x] Back-scan out-of-order inserts — one shared `order::insert_ts_ordered`
      for ring/disk/audio.
- [ ] `Packet::borrow` instead of `Packet::copy` on the save path (needs an
      ffmpeg-next lifetime investigation).
- [ ] µs stream time base instead of ms (precision on long buffers).
- [x] Incremental library refresh (diff by path+mtime, textures kept).
- [x] `filter_sort` cache keyed on (query, sort, generation).
- [x] Frame-buffer pool in the decode path (cap 8, `lock_tolerant`).
- [x] Settings `overridden()` hoisted to once per frame.

### UX

- [x] Sub-second editor time display (`m:ss.mmm` transport + hover).
- [x] Exports surfaced in the library (own section; open/copy/reveal/delete).
- [ ] Tab-nav restoration (`editor.rs` surrenders focus every frame) + AccessKit
      on stepper/timeline (pairs with item 4).
- [x] Library keyboard navigation (arrows/Enter/Delete/Ctrl+F + focus ring).
- [x] Persist editor volume/loop (ui-prefs state file).
- [x] `ord config set section.key value`.
- [x] `ord status --json`.
- [x] `RecordState` carries the recording path (protocol v5).
- [x] Subscribe reconnect (`ord subscribe --reconnect`; closed connections
      report and exit nonzero without it).
- [x] HiDPI HUD (buffer-scale-aware raster).

### Testing

- [x] Offline `execute()` fallback tests (fake ffmpeg via `ORD_FFMPEG`).
- [x] Disk compaction tests (injectable threshold).
- [x] Editor math extraction into `timeline.rs` + boundary tests.
- [x] CLI `parse()` tests (pure over an args iterator).
- [x] `ord-hud` `apply()` tests (moved into `ord_overlay::apply`).
- [x] Auto-arm integration test via the injected probe.
- [ ] HEVC sub-layer PTL fixture (`bitstream.rs` sub-layer skip path).

### Hygiene

- [x] Shared `Rebaser` for `mux.rs`/`record.rs` (in `mux/stream.rs`).
- [x] Dedupe the `access_unit()` fixture (`tests/common/mod.rs`; the bench keeps
      its own copy — benches can't reach `tests/`).
- [x] `MonitorId` deleted (adoption into `capture.target` considered and
      rejected: the string is the compositor-facing representation everywhere).
- [ ] `av1C` 10-bit loud failure (no silent 8-bit assumption) — do together with
      the HDR/Main10 work (item 3), where the real color_config parsing lands.
- [x] Tick-vs-µs comment drift fixed; audio ring now also evicts immediately on
      shrink (matching the engine doc).
- [x] Theme-token cleanup (all view colors come from `theme.rs`).

Also closed in the same session (not in the original backlog): export
cancellation is wedge-proof (progress-reader thread + 100 ms cancel polls), the
NVENC→software fallback only triggers on hardware-encoder stderr signatures,
deterministic `-map` stream selection + Opus-in-MP4 `-strict -2`, one-sided
`ord export --start/--end` trims, `--help` to stdout/exit 0 everywhere,
`ClipSaved` reports the actually-buffered duration, and CI gained the
cross-target `cargo check` lane (windows-gnu + apple-darwin) plus a
release-race concurrency group and a Keep-a-Changelog heading normalizer.

---

## Suggested order

1. **waycap-rs fork bump (item 1)** — unlocks all Phase-1 knobs at once, smallest
   surgical fork change, immediately user-visible.
2. **Audio subsystem (item 2)** — biggest feature gap (separate/per-app tracks).
3. **AccessKit (item 4)** — unblocks proper UI QA and accessibility together.
4. **HDR spikes (item 3)** — largest/riskiest; gate on Spike B feasibility.
5. Perf measurement + test coverage (items 5–6) and the code-audit backlog
   alongside the above.
