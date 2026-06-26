# Roadmap status — research-driven feature plan

Status of the plan derived from competitor research (Medal.tv, ShadowPlay/
NVIDIA App, Outplayed/Overwolf, Steam Game Recording, OBS Replay Buffer,
AMD ReLive, gpu-screen-recorder) plus the settings-panel and UI-revamp work,
the settings/editor feedback round, and the **v0.2 ShadowPlay-parity feature
drop + versioning + editor/player QA** (released v0.2.0–v0.2.2; see the section
below). Verified by `cargo fmt` / `clippy -D warnings` / tests on both lanes
(0 failures).

**Forward-looking work is tracked in [`continue-plan.md`](../continue-plan.md)**
(waycap-rs fork bump to activate the capture knobs, live per-app audio capture,
HDR spikes, AccessKit, perf measurement, packaging validation). This file is the
record of what has shipped.

## Implemented

### Tier R — reliability (the #1 complaint across every incumbent)

- [x] **Capture watchdog** — buffer armed + no frames for 5 s (suspend/resume
  kills NVENC, output changes end the portal session) → `ordd` restarts the
  capture session and broadcasts `CaptureRestarted` (HUD toast "Capture
  recovered"). Deterministic test against the mock backend.
  (`ord-daemon/src/server.rs` pump thread)
- [x] **Post-save verification** — every clip is opened in-process after the
  mux (readable container, video stream, positive duration) before the daemon
  reports success; failures surface as an actionable error instead of a
  corrupt file discovered later. (`ord_core::verify_clip`, used by the writer)
- [x] **HUD daemon-offline indicator** — grey dot replaces the armed dot when
  `ordd` is unreachable; the buffer indicator is cleared so the HUD never
  claims "armed" without knowing. (`Hud::set_daemon_offline` + layershell)

### Tier S — settings panel & daemon control

- [x] **Layered configuration** — base `config.toml` (read-only Home Manager
  symlink keeps working) + sparse runtime overrides at
  `$XDG_STATE_HOME/open-recorder/overrides.toml`, written only by the daemon.
  Pure merge/diff/`overridden_fields` in `ord-common`, fully tested.
- [x] **IPC protocol v2** — `GetConfig` / `SetConfig` / `Mark` commands;
  `Config` / `Marked` / `CaptureRestarted` events; `PROTOCOL_VERSION` bumped
  (stale binaries fail loudly).
- [x] **Tiered apply in the daemon** — storage/hooks/markers/export apply
  live; `buffer_seconds` resizes the ring in place; encoder/audio fields
  rebuild + restart capture via an injected engine factory. Invalid configs
  rejected with actionable errors. (`server::apply_config`, integration-tested)
- [x] **Settings page in `ord-ui`** — capture / audio / storage / markers /
  hooks / export sections; gold dot marks fields overriding the base config;
  validation; Revert / Reset-to-base; "Apply" vs red "Apply (restarts
  capture)"; read-only Hyprland keybind snippet with Copy. Pure, tested
  `SettingsModel` (dirty/tier/problems) separate from the egui view.
- [x] **Header daemon controls** — buffer on/off toggle, Save ▾ (last
  15/30/60/120 s — the "multiple buffer lengths" OBS users ask for), record
  toggle; all one IPC call each, off the UI thread.
- [x] `ord config show` (effective TOML + override list), `ord mark` CLI.

### Tier F — feature gaps users beg incumbents for

- [x] **Markers → MKV chapters** — `ord mark` bookmarks the moment; markers
  inside a later save are written as chapters in the file itself (ffprobe-
  verified golden test), so they cannot desync or vanish (the Steam-markers
  complaint). HUD "Marked" toast.
- [x] **Auto-save on mark** — `markers.auto_save_seconds` turns a mark into
  bookmark + clip in one keypress ("clip that" without the cloud).
- [x] **Storage policy** — configurable `storage.clips_dir`; filename
  templates (`{game}{rec}-{epoch}` default; `{date}`/`{time}` tokens; `/`
  creates subfolders → date folders; escape-proof); **auto-prune** by total
  size (`max_gib`) and/or age (`max_age_days`), exports never touched.
- [x] **Clear buffer after save** (`capture.clear_on_save`) — consecutive
  saves never overlap (gpu-screen-recorder TODO item).
- [x] **Copy as file** — clip card → Wayland clipboard as `text/uri-list`
  (`wl-copy`), paste straight into Discord.
- [x] **Vertical 9:16 export preset** — center-crop + 1080×1920 H.264 for
  Shorts/TikTok/Reels (planner-tested).

### UI revamp

- [x] **Design system** (`ord-ui/src/theme.rs`) — Japanese-corporate
  minimalism: sumi ink-grey scale (5 steps), hairline borders, small radii,
  no shadows; one **shu vermilion** accent reserved for record/danger/brand;
  muted matcha (ok/armed) + kin gold (markers/warnings); 8-pt spacing rhythm;
  fixed type scale. AGENTS.md rule: views must not hardcode colors.
- [x] **Library restyle** — brand mark + live daemon badge + search/sort +
  controls header; flat hairline cards (title + age on one line, quiet
  metadata, actions row with an overflow `⋯` menu); themed status bar, empty
  states, delete confirmation inline (no modal).
- [x] **Editor + settings on the same tokens** (timeline track/accent/marker
  colors from the theme).

### Settings & editor feedback round (user-reported)

- [x] **Spinner number inputs** — every numeric setting is a classic number
  field (type a value, arrow keys, ▴/▾ buttons) instead of a drag value;
  commits as-you-type, clamps on blur. (`settings_view::stepper_u32`)
- [x] **Captions on every setting** — each row carries a one-line description
  of the real impact (RAM/GPU cost, compatibility, privacy), and the form uses
  a fixed label column so labels and controls align.
- [x] **Hideable HUD status dot** — new `[overlay] show_status_dot` config
  (live tier). `SetConfig` now broadcasts the new effective `Config` so the
  HUD applies it instantly; `ord-hud` also fetches it on (re)connect.
  `PROTOCOL_VERSION` bumped to 3 (the `Config` shape changed).
- [x] **Browse… buttons** — clips folder (and hook script) get a file-dialog
  picker next to the path input (zenity → kdialog, off-thread, actionable
  message when neither exists).
- [x] **Live library refresh** — `ord-ui` keeps a persistent `Subscribe`
  connection: a clip saved from any client (hotkey, CLI, auto-save-on-mark)
  appears in the grid immediately; record/buffer state updates instantly.
  Record start/stop also gets clear status copy, and a finished recording
  refreshes the list.
- [x] **Useful editor markers** — the clip's `ord mark` chapters load as
  markers automatically; M adds / Shift+M removes; `[`/`]` jump between
  markers; in/out/playhead drags snap to nearby markers; gold flag heads on
  the track.
- [x] **Wheel zoom** — plain mouse wheel zooms the timeline anchored at the
  pointer; Shift+wheel (or horizontal wheel) pans when zoomed.
- [x] **Frame-accurate scrubbing** — dragging the timeline updates the paused
  preview to the exact frame under the pointer (audio stays silent): the
  decoder discards the keyframe→target run-up after every seek and the UI
  shows the first post-seek frame instead of freezing on the GOP start.
- [x] **Multi-segment cuts** — S splits at the playhead, X / right-click
  toggles a piece cut/kept; playback skips cut pieces live; export
  concatenates the kept pieces (`ord_export::export_segments_with`,
  filter_complex trim+concat, NVENC→software fallback, planner-tested).
  Stream-copy/GIF/audio presets are greyed out with an explanation while
  cuts are active.

### Stability & editor polish round (user-reported, bench-verified)

- [x] **Daemon deadlock on buffer toggle** — root-caused with a live stack
  dump: waycap-rs `Capture::close()` early-returned when `finish()` failed
  (double-drain EOF), skipping the PipeWire `Terminate` sends, so `Drop`
  joined a never-exiting capture loop while holding the daemon handler lock —
  every later command hung ("Buffer button does nothing"). Fixed in the fork
  (close tears down unconditionally; drains are idempotent), rev bumped in
  `ord-core` + flake `outputHashes`.
- [x] **Recording head/tail** — the engine pumps audio before video and NVENC
  emits frames with latency, so the recorder dropped ~1 s of audio after the
  first keyframe and let audio outrun the final video frame. The recorder now
  buffers pending audio (bounded preroll) and flushes it only up to the
  newest written video pts; `finish` drops the trailing remainder. Golden
  test asserts both tracks start and end together.
- [x] **"Play does nothing until you scrub"** — the player's sample-counting
  audio clock ignored pts: a leading audio gap (old recordings) left the
  buffer empty, freezing the clock while the full video queue deadlocked the
  demux pacing. The decode session now silence-fills the known leading gap on
  every seek, drops keyframe-run-up audio, keeps demuxing through starvation,
  and ends playback at EOF when the audio track is shorter than the file.
  Verified frame-by-frame on the affected recording in the Xvfb bench.
- [x] **Settings form layout** — steppers are one tight `[value][▴▾] suffix`
  group (painted triangle buttons, no glyph fallback), labels left-align in a
  fixed column, the scroll area explicitly fills the panel
  (`auto_shrink(false)` + `vertical_centered`), bench-verified scrolling to
  the last section.
- [x] **Editor discoverability** — visible tools row (✂ Split, ✕ Cut piece /
  ↩ Keep piece, ⚑ Marker, zoom −/+/Fit with a zoom readout), hover ghost line
  + time bubble on the timeline, resize cursor + fatter grab radius on the
  trim handles, playhead triangle head, and the editor now owns the keyboard
  (no focus-stealing after clicking a button). Save ▾ menu shows the buffered
  seconds and what each entry will do; buffer toggle and saves give instant
  status feedback.
- [x] **Split/cut v2** — cut points are first-class: drag a cut line to slide
  it (grip handle mid-track, preview follows frame-accurately, marker
  snapping), Backspace or right-click a cut line joins the pieces back
  (kept-if-either-kept semantics), "✕ Cut In→Out" removes the selected range
  in one action and clears the selection, Ctrl+Z / ↶ Undo reverts cut edits
  (bounded snapshot stack), cut pieces draw a diagonal hatch + "✕ cut" badge,
  and the piece under the pointer lifts to hint the X/right-click target.
  Model ops (`move_cut`/`join_at`/`cut_range`) are unit-tested; the whole
  flow was driven end-to-end in the Xvfb bench.

### Tier P — packaging & releases (install without compiling)

- [x] **Automated releases (release-plz).** `release-plz.toml` +
  `.github/workflows/release.yml`: every master push maintains a
  `chore: release vX.Y.Z` PR that bumps the single `[workspace.package].version`
  (all crates inherit it), refreshes `Cargo.lock`, and regenerates `CHANGELOG.md`
  from Conventional Commits. Merging it tags `vX.Y.Z` + creates the GitHub
  Release. `ord-cli` is the release unit; the other six crates fold in via
  `changelog_include`. No crates.io publish (`git_only`).
- [x] **Nix install — cachix (already live).** `ci.yml`'s `build` job pushes
  every closure to `grok-insider.cachix.org` on each master push/tag, so NixOS
  consumers substitute `open-recorder-X.Y.Z` instead of compiling. Cachix stays
  in `ci.yml` by design; `release.yml` only cuts the tag/Release + artifacts.
- [x] **Non-Nix `ord` client — static musl binary.** `upload-ord` builds `ord`
  (pure Rust: ord-cli→ord-export→ord-common, no ffmpeg/C) for
  `x86_64-unknown-linux-musl`, verifies it's static, and attaches the tarball —
  the fast on-PATH binary compositor keybinds invoke.
- [x] **Non-Nix recorder — AppImages.** `upload-appimages` bundles `ordd`,
  `ord-hud`, `ord-ui` from the flake via `nix bundle`
  (`github:ralismark/nix-appimage`), reusing the cached closure. The flake's
  runtime wrapper resolves the NVIDIA driver libs on foreign distros
  (`postFixup --suffix` over the FHS driver dirs, alongside the NixOS
  `/run/opengl-driver/lib` prefix), and `apps.{ord,ordd,ord-hud,ord-ui}` give
  clean bundle entrypoints.
- [x] **Contributor docs.** `CONTRIBUTING.md` (Conventional Commits → bump rules
  + the release flow) and a rewritten `docs/releasing.md` (release-plz +
  AppImages); `README.md` gained a non-Nix install section.

### v0.2 — ShadowPlay-parity feature drop, versioning & QA (released v0.2.0–v0.2.2)

Config + validation + IPC + pure logic shipped and tested; the parts that need
the `0xfell/waycap-rs` fork to *take effect*, real-hardware spikes, or a live
PipeWire capture engine are tracked in `continue-plan.md` (the canonical
forward plan). Released and live on the dev box at **v0.2.2 (protocol 4)**.

- [x] **`ord doctor [--fix]`** — diagnoses + installs the NVIDIA
  `CudaNoStablePerfLimit` application profile that frees `ordd` from the
  CUDA/NVENC **P2 downclock** (the real perf delta vs ShadowPlay). Verified
  against a live driver 610.
- [x] **Capture knobs** — `resolution`, `keyframe_interval_ms`, `framerate_mode`
  (cfr/vfr/content), `color_range`, `tune`: config + validation + restart
  predicate + builder plumbing. ⛔ *Applied* once the fork exposes the
  `CaptureBuilder` setters (`// fork:` block in `waycap_backend.rs`).
- [x] **Disk-backed replay** (`capture.replay_storage = disk`) — `DiskFrameStore`
  (spill + RAM index + compaction) threaded through a runtime-selected
  `Box<dyn FrameStore>` in engine/handler/server. e2e-verified.
  *(Closes the Tier-X "disk-backed replay buffer".)*
- [x] **`ord shot`** — decodes the newest GOP to a PNG (real-hardware verified).
- [x] **`capture.target`** (portal/monitor) + **`capture.auto_arm`** (arm the
  buffer when a Steam app / fullscreen window takes focus; `is_game_window`).
- [x] **Multi-track + per-application audio config** (`audio.tracks`,
  `AudioSource` selectors) + pure routing (`audio_route::plan_track`,
  `config::effective_tracks`). ⛔ Live PipeWire+Opus multi-track capture engine
  is the gated follow-on. *(Supersedes the Tier-X "separate mic/game tracks".)*
- [x] **HDR config** (`capture.hdr`, validated to HEVC/AV1). 🔬 Main10-encode +
  KMS-capture spikes in `docs/hdr.md`.
- [x] **Versioning** — `ord`/`ordd --version` → `X.Y.Z [protocol N]` (git rev on
  cargo builds, rev-less on Nix) via `ord-common::version` + `build.rs`;
  `PROTOCOL_VERSION` 3→4 (`Screenshot`/`ScreenshotSaved`); `docs/releasing.md`.
- [x] **Clip-library grid fix (v0.2.1)** — `horizontal_wrapped` inside a vertical
  `ScrollArea` never wrapped (every clip in one off-screen row); replaced with a
  deterministic grid (column count from the panel width). wisp-verified.
- [x] **Editor/player QA (v0.2.2)** — idle-aware UI watchdog (a paused editor no
  longer logs false `UI STALL` ANRs; focus-gated + 1s heartbeat); audio-clock
  **stall recovery** (pause instead of a 60fps spin if the clock-driving audio
  output freezes — clock-only, no wall-clock fallback); telemetry honors
  `ORD_DEBUG_LOG`; `ORD_AUTOPLAY` QA aid. Full 30s play-through + trim/export
  (`av1_nvenc`) verified clean on the real RTX 5070 Ti.

## Left to do

> Forward-looking work now lives in **`continue-plan.md`** (waycap-rs fork bump,
> live per-app audio capture, HDR spikes, AccessKit, perf measurement, packaging
> validation). The residue from earlier rounds:

- [x] **Visual QA of the revamped UI** — done: clip library, editor (preview +
  transport + timeline), and settings render correctly (verified via the wisp
  nested sandbox + grim on the real session; the grid bug found here was fixed
  in v0.2.1).
- [~] **Live hardware verification of the new daemon paths** — playback,
  trim/export, `ord doctor`, and the full capture→NVENC→save path are verified
  on the real RTX 5070 Ti; watchdog-across-suspend and live `SetConfig`
  capture-restart remain mock/integration-tested only. The dev box now runs
  v0.2.2 (protocol 4 — `ordd`/`ord`/`ord-hud`/`ord-ui` all on the same release).
- [x] **Commit + push + CI/cachix dispatch** — done: master green across all
  three CI jobs, closures cached, v0.2.0–v0.2.2 tagged.
- [ ] **Enable "Allow GitHub Actions to create and approve pull requests"**
  (Settings → Actions → General) so release-plz can open the release PR. One-time;
  `CACHIX_AUTH_TOKEN` and the baseline `v*` tags already exist.
- [ ] **On-hardware AppImage validation** — bundle + run the ordd/ord-hud/ord-ui
  AppImages on a non-NixOS NVIDIA box (ordd first: NVENC + driver-path proof;
  ord-ui last: GL vendor-match is the known hazard). Full checklist in
  `continue-plan.md`.
- [x] **Hyprland keybind for `ord mark`** — added to
  `~/.config/nixos/configs/hyprland.conf` (`bind = ALT, M, exec, ord mark`)
  next to the existing save bind; takes effect on the next Hyprland
  reload/rebuild.
- [x] **Home Manager module polish** — the module's `settings` example/docs
  now cover `storage`, `markers`, `hooks`, and the new `overlay` section, and
  explain the base-vs-overrides layering.

### Tier X — bigger bets (explicitly deferred)

- [~] **Separate mic/game + per-application audio tracks** — config model
  (`audio.tracks`, `AudioSource`) + pure routing (`audio_route::plan_track`)
  shipped in v0.2; the live PipeWire+Opus multi-track capture engine + N-track
  muxing + editor per-track mute remain (`continue-plan.md` item 2).
- [ ] **Per-game profiles** (game detection exists; per-game config overlays).
- [x] **Disk-backed replay buffer** — shipped in v0.2: `DiskFrameStore`
  (`capture.replay_storage = disk`) over the `FrameStore` seam, runtime-selected
  via `Box<dyn FrameStore>` and e2e-verified.
- [ ] **Voice "clip that"** — example hook script wiring the local
  speech-stack to `ord save` (docs, not core).
- [ ] **Diagnostics page** — dropped-frame counters, ring RAM use, encoder in
  use, daemon log tail (needs a `Stats` IPC event).
- [x] **Multi-segment export** — implemented in the feedback round above
  (editor cuts + `export_segments_with`).
- [ ] **Share-link upload / clipboard of rendered exports** — parked in
  `future-features.md`, do not implement without an explicit ask.
- [ ] **Flathub Flatpak of `ord-ui`** — the OBS-style GUI path (driver-matched
  GL via the `GL.nvidia` extension + PipeWire portal); daemon/CLI/HUD stay out
  of the sandbox by design. Pursue if the `ord-ui` AppImage GL story is fragile
  or for Flathub reach/auto-updates. See `continue-plan.md`.
- [ ] **Unified single-file AppImage / native `.deb`·`.rpm`·AUR** — optional
  later distribution formats; the per-binary AppImages + static `ord` ship first
  (`continue-plan.md`).
