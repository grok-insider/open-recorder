# Roadmap status — research-driven feature plan

Status of the plan derived from competitor research (Medal.tv, ShadowPlay/
NVIDIA App, Outplayed/Overwolf, Steam Game Recording, OBS Replay Buffer,
AMD ReLive, gpu-screen-recorder) plus the settings-panel and UI-revamp work,
and the settings/editor feedback round (steppers + captions, live library
refresh, hideable HUD dot, useful markers, wheel zoom, frame-accurate
scrubbing, multi-segment cuts). Verified by `cargo fmt` /
`clippy -D warnings` / tests on both lanes (0 failures).

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

## Left to do

- [ ] **Visual QA of the revamped UI** — deferred because the desktop session
  was in active gameplay (and the nested sandbox has no GL for eframe). Open
  `ord-ui` after a rebuild and review library, editor, and settings pages;
  tune spacing/contrast if needed. Now also covers the new settings form
  (steppers/captions), segment cuts, and marker flags.
- [ ] **Live hardware verification of the new daemon paths** — watchdog
  recovery across a real suspend/resume, `SetConfig` capture-restart on the
  live NVENC session, `ord mark` → chapters visible in a real clip, the HUD
  dot toggling live via the new overlay setting. (All are mock/integration-
  tested; the running user daemon was intentionally left untouched. Note:
  protocol v3 — rebuild/restart `ordd`, `ord`, `ord-hud`, and `ord-ui`
  together.)
- [ ] **Commit + push + CI/cachix dispatch** — the whole batch is uncommitted
  in the working tree by design (commits only on explicit ask).
- [x] **Hyprland keybind for `ord mark`** — added to
  `~/.config/nixos/configs/hyprland.conf` (`bind = ALT, M, exec, ord mark`)
  next to the existing save bind; takes effect on the next Hyprland
  reload/rebuild.
- [x] **Home Manager module polish** — the module's `settings` example/docs
  now cover `storage`, `markers`, `hooks`, and the new `overlay` section, and
  explain the base-vs-overrides layering.

### Tier X — bigger bets (explicitly deferred)

- [ ] **Separate mic/game audio tracks** (waycap-rs fork: second Opus track;
  editor gains per-track mute) — top OBS power-user wish.
- [ ] **Per-game profiles** (game detection exists; per-game config overlays).
- [ ] **Disk-backed replay buffer** — `FrameStore` seam is in place
  (`Engine<B, S: FrameStore = RingBuffer>`); a `DiskStore` impl remains
  (see `future-features.md`).
- [ ] **Voice "clip that"** — example hook script wiring the local
  speech-stack to `ord save` (docs, not core).
- [ ] **Diagnostics page** — dropped-frame counters, ring RAM use, encoder in
  use, daemon log tail (needs a `Stats` IPC event).
- [x] **Multi-segment export** — implemented in the feedback round above
  (editor cuts + `export_segments_with`).
- [ ] **Share-link upload / clipboard of rendered exports** — parked in
  `future-features.md`, do not implement without an explicit ask.
