//! Clip trim editor (behind the `gui` feature).
//!
//! An inline A/V editor built on [`Player`]: a video preview with real
//! play/pause/loop + audio, a draggable in/out timeline with a time ruler,
//! zoom-adaptive filmstrip, audio waveform, markers (the clip's `ord mark`
//! chapters load automatically) and multi-segment cuts (split at the playhead,
//! toggle pieces off — playback and export skip them), keyboard shortcuts,
//! click-to-type numeric times, and an "Export selection" that hands the
//! result to `ord-export`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use ord_export::{Preset, Trim};

use crate::format::{human_duration, human_duration_ms, parse_duration};
use crate::player::{DecKind, Player, PreviewFrame};
use crate::prefs::{self, EditorPrefs};
use crate::project::EditorProject;
use crate::timeline::{self, DragTarget, Segments, View};

/// Tiles across the *visible* view (regenerated on zoom/pan).
const FILMSTRIP_TILES: usize = 16;
/// Peak buckets for the whole-clip waveform (drawn under the filmstrip).
const WAVEFORM_PEAKS: usize = 512;
/// Wait this long after the last zoom/pan change before re-extracting tiles.
const FILMSTRIP_DEBOUNCE: Duration = Duration::from_millis(180);
/// Snap radius (px) for markers under the in/out/playhead drags and clicks.
const SNAP_PX: f32 = 6.0;

/// Which numeric time field is open for typing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeField {
    In,
    Out,
    Playhead,
}

/// What the editor wants the app to do after a frame.
pub enum EditorAction {
    None,
    Back,
    Export {
        preset: Preset,
        trim: Option<Trim>,
        /// Multi-segment cut list (ascending, non-overlapping). When `Some`,
        /// it supersedes `trim` and the pieces are concatenated on export.
        segments: Option<Vec<Trim>>,
        mute: bool,
        /// Preview/export rate (`0.25`…`2.0`).
        speed: f32,
    },
    /// Write the selection into the clips library (stream-copy when possible).
    SaveAsClip {
        trim: Option<Trim>,
        segments: Option<Vec<Trim>>,
        mute: bool,
        speed: f32,
    },
}

/// Editor state for a single clip.
pub struct EditorState {
    clip: PathBuf,
    label: String,
    player: Player,
    /// Pure domain (timeline, cuts, markers, history, mute/volume/zoom).
    project: EditorProject,
    /// Chapters probed off-thread land here (the clip's `ord mark` bookmarks).
    chapters_rx: Receiver<Vec<f64>>,
    drag: Option<DragTarget>,
    /// Pre-drag snapshot for a cut-line drag (one undo entry per drag).
    drag_undo: Option<Segments>,
    /// `(generation, tile_index, image)` — stale generations are dropped.
    strip_rx: Receiver<(u64, usize, egui::ColorImage)>,
    strip: Vec<Option<egui::TextureHandle>>,
    /// Generation of the in-flight / current filmstrip job.
    strip_gen: u64,
    /// `(start, span)` the current tiles cover (source seconds).
    strip_cover: (f64, f64),
    /// Desired cover after zoom/pan; drained after [`FILMSTRIP_DEBOUNCE`].
    strip_pending: Option<(f64, f64)>,
    strip_pending_at: Instant,
    wave_rx: Receiver<Vec<f32>>,
    /// Normalized peak envelope for the whole clip (`None` until decoded).
    wave: Option<Vec<f32>>,
    /// Click-to-type In / Out / playhead (`m:ss.mmm`).
    time_edit: Option<(TimeField, String)>,
    debug: bool,
    dbg_log_at: Instant,
    /// In-flight export progress from the library shell (`None` = idle).
    export_progress: Option<f32>,
    /// Last decode seek issued during a timeline scrub (throttle intermediate
    /// seeks so keyframe run-up can finish and the preview updates live).
    last_scrub_seek: Option<(f64, Instant)>,
}

impl EditorState {
    /// Open the editor for `clip`. Returns an error if the media can't be opened.
    pub fn new(clip: PathBuf, label: String, ctx: &egui::Context) -> Result<Self, String> {
        let mut player = Player::open(&clip)?;
        let saved = prefs::load();
        player.set_volume(saved.volume);
        player.set_loop(saved.looping);
        if crate::tuning::autoplay() {
            player.play();
        }
        let project = EditorProject::new(player.duration(), saved.volume);
        let dur = player.duration().max(1e-6);
        let strip_rx = spawn_filmstrip(&clip, 1, 0.0, dur, FILMSTRIP_TILES, ctx.clone());
        let wave_rx = spawn_waveform(&clip, WAVEFORM_PEAKS, ctx.clone());
        let chapters_rx = spawn_chapters(&clip, ctx.clone());
        Ok(Self {
            clip,
            label,
            player,
            project,
            chapters_rx,
            drag: None,
            drag_undo: None,
            strip_rx,
            strip: vec![None; FILMSTRIP_TILES],
            strip_gen: 1,
            strip_cover: (0.0, dur),
            strip_pending: None,
            strip_pending_at: Instant::now(),
            wave_rx,
            wave: None,
            time_edit: None,
            debug: crate::tuning::debug_overlay(),
            dbg_log_at: Instant::now(),
            export_progress: None,
            last_scrub_seek: None,
        })
    }

    pub fn clip(&self) -> &PathBuf {
        &self.clip
    }

    /// Feed export progress from the app (library shell owns the job).
    pub fn set_export_progress(&mut self, progress: Option<f32>) {
        self.export_progress = progress;
    }

    /// Pause playback (used when the window loses focus / is hidden).
    pub fn pause_player(&mut self) {
        if self.player.is_playing() {
            self.player.pause();
        }
    }

    fn seek_to(&mut self, t: f64) {
        self.project.timeline.set_playhead(t);
        self.player.seek(t);
        self.last_scrub_seek = Some((t, Instant::now()));
    }

    /// Scrub the playhead (and optionally the decoder) during a drag.
    ///
    /// The playhead UI always follows the pointer. Decode seeks are throttled
    /// so the demuxer can finish keyframe run-up and paint intermediate frames
    /// instead of thrashing on every pointer motion; `force` is used on
    /// drag-end / click so the final frame is exact.
    fn scrub_to(&mut self, t: f64, force: bool, ctx: &egui::Context) {
        self.project.timeline.set_playhead(t);
        let now = Instant::now();
        if crate::layout::should_scrub_seek(
            self.last_scrub_seek,
            now,
            t,
            force,
            Duration::from_millis(50),
            0.04,
        ) {
            self.player.seek(t);
            self.last_scrub_seek = Some((t, now));
        }
        // Keep repainting while paused so the scrubbed frame paints promptly.
        ctx.request_repaint();
    }

    /// Run a cut mutation with undo: the previous state is kept only when the
    /// mutation reports an actual change.
    fn edit_cuts(&mut self, f: impl FnOnce(&mut Segments) -> bool) {
        self.project.edit_cuts(f);
    }

    /// Revert the most recent cut edit (Ctrl+Z / the Undo button).
    fn undo_cut_edit(&mut self) {
        self.project.undo_cut();
    }

    /// Place a marker at the playhead (deduplicated, kept sorted).
    fn add_marker(&mut self) {
        let ph = self.project.timeline.playhead();
        self.project.markers.add(ph);
    }

    /// Render the editor; returns the action the user took (if any).
    pub fn ui(&mut self, ctx: &egui::Context, wd: &crate::diag::Watchdog) -> EditorAction {
        // Pull in any decoded filmstrip tiles (drop stale generations).
        while let Ok((gen, i, img)) = self.strip_rx.try_recv() {
            if gen != self.strip_gen {
                continue;
            }
            if let Some(slot) = self.strip.get_mut(i) {
                *slot = Some(ctx.load_texture(
                    format!("strip-{gen}-{i}"),
                    img,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
        // Waveform peaks (whole-clip, once).
        if self.wave.is_none() {
            if let Ok(peaks) = self.wave_rx.try_recv() {
                if !peaks.is_empty() {
                    self.wave = Some(peaks);
                }
            }
        }
        // The clip's chapters (`ord mark` bookmarks) arrive as markers.
        while let Ok(chapters) = self.chapters_rx.try_recv() {
            self.project.markers.extend(chapters);
        }

        // Player drives the playhead during playback; keep its range in sync.
        self.player.set_range(
            self.project.timeline.in_point(),
            self.project.timeline.out_point(),
        );
        if self.player.is_playing() {
            self.project.timeline.set_playhead(self.player.position());
            // Cut segments are skipped live, so the preview plays exactly what
            // an export would contain.
            if !self.project.segments.is_trivial() {
                let pos = self.project.timeline.playhead();
                if let Some(target) = self
                    .project
                    .segments
                    .skip_target(pos, self.project.timeline.out_point())
                {
                    self.seek_to(target);
                }
            }
        }

        let mut action = self.keyboard(ctx);

        egui::TopBottomPanel::top("editor-top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let back = ui.button("← Back");
                crate::a11y::button(&back, "Back");
                if back.clicked() {
                    action = EditorAction::Back;
                }
                ui.separator();
                ui.label(egui::RichText::new(&self.label).strong().size(15.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Space play · I/O in-out · S split · X cut · ⌫ join · Ctrl+Z undo · M marker · wheel zoom",
                        )
                        .color(ui.visuals().weak_text_color())
                        .size(12.0),
                    )
                    .on_hover_text(
                        "←/→ seek 1s (Shift: 5s) · ,/. frame-step · Home/End to in/out\n\
                         M places a marker · Shift+M removes the nearest · [/] jump between markers\n\
                         S splits at the playhead; drag a cut line to slide it; X (or right-click \
                         a piece) cuts/keeps it; Backspace (or right-click a cut line) joins the \
                         pieces; Ctrl+Z undoes cut edits\n\
                         Cut pieces are skipped when playing and joined out on export\n\
                         Mouse wheel zooms at the pointer; Shift+wheel pans when zoomed",
                    );
                });
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("editor-bottom").show(ctx, |ui| {
            ui.add_space(8.0);
            self.transport_ui(ui);
            ui.add_space(8.0);
            self.timeline_ui(ui);
            ui.add_space(6.0);
            self.tools_ui(ui);
            ui.add_space(6.0);
            self.export_ui(ui, &mut action);
            ui.add_space(8.0);
        });

        wd.beat("editor:preview");
        let panel_frame =
            egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(8.0));
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                self.preview_ui(ui, ctx);
            });
        wd.beat("editor:done");

        if self.debug {
            self.debug_tick(ctx);
        }

        action
    }

    fn keyboard(&mut self, ctx: &egui::Context) -> EditorAction {
        // While a numeric time field is open, leave focus alone so typing works;
        // Escape cancels the edit without applying.
        if self.time_edit.is_some() {
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.time_edit = None;
            }
            return EditorAction::None;
        }
        // The editor owns the keyboard outside text entry. Drop any lingering
        // widget focus (a clicked button/slider keeps egui focus) so Space can
        // never double-trigger the last-clicked control.
        ctx.memory_mut(|m| {
            if let Some(focused) = m.focused() {
                m.surrender_focus(focused);
            }
        });
        let (
            space,
            key_i,
            key_o,
            left,
            right,
            shift,
            comma,
            period,
            home,
            end,
            plus,
            minus,
            mkey,
            esc,
        ) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::I),
                i.key_pressed(egui::Key::O),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.modifiers.shift,
                i.key_pressed(egui::Key::Comma),
                i.key_pressed(egui::Key::Period),
                i.key_pressed(egui::Key::Home),
                i.key_pressed(egui::Key::End),
                i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                i.key_pressed(egui::Key::Minus),
                i.key_pressed(egui::Key::M),
                i.key_pressed(egui::Key::Escape),
            )
        });
        let (split, toggle_seg, prev_marker, next_marker, join, undo_key, redo_key) =
            ctx.input(|i| {
                (
                    i.key_pressed(egui::Key::S),
                    i.key_pressed(egui::Key::X),
                    i.key_pressed(egui::Key::OpenBracket),
                    i.key_pressed(egui::Key::CloseBracket),
                    i.key_pressed(egui::Key::Backspace) || i.key_pressed(egui::Key::Delete),
                    i.modifiers.command && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                    i.modifiers.command
                        && (i.modifiers.shift && i.key_pressed(egui::Key::Z)
                            || i.key_pressed(egui::Key::Y)),
                )
            });

        if esc {
            return EditorAction::Back;
        }
        if space {
            self.player.toggle();
        }
        let ph = self.project.timeline.playhead();
        if key_i {
            self.project.timeline.set_in(ph);
            self.seek_to(self.project.timeline.in_point());
        }
        if key_o {
            self.project.timeline.set_out(ph);
            self.seek_to(self.project.timeline.out_point());
        }
        let frame = 1.0 / self.player.fps().max(1.0);
        if left {
            self.seek_to(ph - if shift { 5.0 } else { 1.0 });
        }
        if right {
            self.seek_to(ph + if shift { 5.0 } else { 1.0 });
        }
        if comma {
            self.seek_to(ph - frame);
        }
        if period {
            self.seek_to(ph + frame);
        }
        if home {
            self.seek_to(self.project.timeline.in_point());
        }
        if end {
            self.seek_to(self.project.timeline.out_point());
        }
        if plus {
            self.project.zoom = (self.project.zoom * 1.5).min(60.0);
        }
        if minus {
            self.project.zoom = (self.project.zoom / 1.5).max(1.0);
        }
        if mkey && !shift {
            self.add_marker();
        }
        if mkey && shift {
            self.remove_nearest_marker(ph);
        }
        if prev_marker {
            if let Some(t) = self.prev_marker(ph) {
                self.seek_to(t);
            }
        }
        if next_marker {
            if let Some(t) = self.next_marker(ph) {
                self.seek_to(t);
            }
        }
        if split {
            self.edit_cuts(|s| s.split_at(ph));
        }
        if toggle_seg {
            self.edit_cuts(|s| s.toggle_at(ph));
        }
        if join {
            self.edit_cuts(|s| s.join_at(ph).is_some());
        }
        if undo_key {
            self.undo_cut_edit();
        }
        if redo_key {
            self.project.redo_cut();
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F3)) {
            self.debug = !self.debug;
        }
        EditorAction::None
    }

    /// The nearest marker strictly before `t`.
    fn prev_marker(&self, t: f64) -> Option<f64> {
        self.project.markers.prev(t)
    }

    /// The nearest marker strictly after `t`.
    fn next_marker(&self, t: f64) -> Option<f64> {
        self.project.markers.next(t)
    }

    /// Remove the marker closest to `t` (within a second), e.g. a misplaced M.
    fn remove_nearest_marker(&mut self, t: f64) {
        self.project.markers.remove_nearest(t, 1.0);
    }

    /// Debug overlay + throttled log to `/tmp/ord-ui-debug.log` (toggle: F3 or
    /// the `ORD_DEBUG` env var). Surfaces clock vs audio-buffer vs queue state so
    /// A/V-sync and decode/throughput issues are visible.
    fn debug_tick(&mut self, ctx: &egui::Context) {
        let s = self.player.stats();
        let dur = self.player.duration();
        let fps = self.player.fps();
        egui::Area::new(egui::Id::new("ord-debug"))
            .fixed_pos(egui::pos2(14.0, 60.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.monospace(format!(
                        "pos {:.2}/{:.2}  audio:{}  play:{}",
                        s.position, dur, s.has_audio, s.playing
                    ));
                    ui.monospace(format!(
                        "abuf {:.0}ms  vq {}  dec {}  drop {}",
                        s.audio_buf_ms, s.frames_queued, s.decoded, s.dropped
                    ));
                    ui.monospace(format!(
                        "decoder {}  src_fps {fps:.2}  zoom {:.1}",
                        s.decoder.label(),
                        self.project.zoom
                    ));
                    ui.monospace("F3: toggle debug");
                });
            });

        if self.dbg_log_at.elapsed() >= Duration::from_millis(1000) {
            self.dbg_log_at = Instant::now();
            let line = format!(
                "[ord-ui] decoder={} pos={:.2} dur={:.2} audio={} play={} abuf_ms={:.0} vq={} dec={} drop={}\n",
                s.decoder.label(), s.position, dur, s.has_audio, s.playing, s.audio_buf_ms, s.frames_queued, s.decoded, s.dropped
            );
            // Use the shared diagnostics path so `ORD_DEBUG_LOG` redirects the
            // telemetry too (it was hardcoded, diverging from `diag::log_line`).
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(crate::diag::log_path())
            {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let avail = ui.available_size();
        // Aspect-fit (w, h) inside the available area for source dims (tw, th).
        let fit = |tw: usize, th: usize| -> egui::Vec2 {
            let ar = (tw.max(1) as f32) / (th.max(1) as f32);
            let mut w = avail.x;
            let mut h = w / ar;
            if h > avail.y {
                h = avail.y;
                w = h * ar;
            }
            egui::vec2(w, h)
        };
        match self.player.frame(ctx) {
            PreviewFrame::Texture(tex) => {
                let [tw, th] = tex.size();
                let size = fit(tw, th);
                ui.vertical_centered(|ui| {
                    let img = egui::Image::new(&tex)
                        .fit_to_exact_size(size)
                        .rounding(8.0)
                        .sense(egui::Sense::click());
                    let resp = ui.add(img);
                    crate::a11y::button(&resp, "Play/Pause preview");
                    if resp.clicked() {
                        self.player.toggle();
                    }
                });
            }
            PreviewFrame::Gl => {
                let [tw, th] = self.player.video_size().unwrap_or([16, 9]);
                let size = fit(tw, th);
                ui.vertical_centered(|ui| {
                    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                    crate::a11y::button(&resp, "Play/Pause preview");
                    // Draw the NV12 frame on the GPU into this rect.
                    ui.painter().add(self.player.gl_callback(rect));
                    if resp.clicked() {
                        self.player.toggle();
                    }
                });
            }
            PreviewFrame::None => {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("decoding preview…")
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            }
        }
    }

    fn transport_ui(&mut self, ui: &mut egui::Ui) {
        let frame = 1.0 / self.player.fps().max(1.0);
        ui.horizontal(|ui| {
            let to_in = ui.button("⏮ In").on_hover_text("Jump to in (Home)");
            crate::a11y::button(&to_in, "Jump to In");
            if to_in.clicked() {
                self.seek_to(self.project.timeline.in_point());
            }
            let prev_f = ui.button("−1f").on_hover_text("Previous frame (,)");
            crate::a11y::button(&prev_f, "Previous frame");
            if prev_f.clicked() {
                self.seek_to(self.project.timeline.playhead() - frame);
            }
            let play_label = if self.player.is_playing() {
                "⏸ Pause"
            } else {
                "▶ Play"
            };
            let play = ui.button(egui::RichText::new(play_label).strong());
            crate::a11y::button(
                &play,
                if self.player.is_playing() {
                    "Pause"
                } else {
                    "Play"
                },
            );
            if play.clicked() {
                self.player.toggle();
            }
            let next_f = ui.button("+1f").on_hover_text("Next frame (.)");
            crate::a11y::button(&next_f, "Next frame");
            if next_f.clicked() {
                self.seek_to(self.project.timeline.playhead() + frame);
            }
            let to_out = ui.button("Out ⏭").on_hover_text("Jump to out (End)");
            crate::a11y::button(&to_out, "Jump to Out");
            if to_out.clicked() {
                self.seek_to(self.project.timeline.out_point());
            }

            ui.separator();
            let mut looping = self.player.looping();
            let loop_btn = ui.selectable_label(looping, "↻ Loop");
            crate::a11y::button(&loop_btn, "Loop");
            if loop_btn.clicked() {
                looping = !looping;
                self.player.set_loop(looping);
                let _ = prefs::save(EditorPrefs {
                    volume: self.project.volume,
                    looping,
                });
            }

            if self.player.has_audio() {
                ui.separator();
                ui.label("Vol");
                let slider = ui.add(
                    egui::Slider::new(&mut self.project.volume, 0.0..=1.0)
                        .show_value(false)
                        .fixed_decimals(2),
                );
                crate::a11y::slider(&slider, "Volume");
                if slider.changed() {
                    self.player.set_volume(self.project.volume);
                }
                // Persist once the adjustment settles, not per drag tick.
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    let _ = prefs::save(EditorPrefs {
                        volume: self.project.volume,
                        looping: self.player.looping(),
                    });
                }
            }

            ui.separator();
            ui.label("Speed").on_hover_text(
                "Preview rate (0.25×–2×). Audio is held when not 1× so frames stay stable.",
            );
            let mut speed = self.project.speed;
            let speed_slider = ui.add(
                egui::Slider::new(&mut speed, 0.25..=2.0)
                    .fixed_decimals(2)
                    .suffix("×"),
            );
            crate::a11y::slider(&speed_slider, "Speed");
            if speed_slider.changed() {
                self.project.set_speed(speed);
                self.player.set_speed(self.project.speed);
            }
            for (label, s) in [("½×", 0.5f32), ("1×", 1.0), ("2×", 2.0)] {
                let name = match label {
                    "½×" => "Speed half",
                    "1×" => "Speed normal",
                    "2×" => "Speed double",
                    _ => label,
                };
                let btn = ui.selectable_label((self.project.speed - s).abs() < 0.01, label);
                crate::a11y::button(&btn, name);
                if btn.clicked() {
                    self.project.set_speed(s);
                    self.player.set_speed(self.project.speed);
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // NVDEC fell back to CPU decoding: say so, instead of leaving
                // "the editor is sometimes heavy" undiagnosable (F3 has detail).
                if self.player.decoder_kind() == DecKind::Software {
                    ui.label(
                        egui::RichText::new("sw decode")
                            .color(ui.visuals().weak_text_color())
                            .size(11.0),
                    )
                    .on_hover_text(
                        "Hardware (NVDEC) decoding is unavailable for this clip, so the \
                         preview decodes on the CPU and may use noticeably more of it.",
                    );
                }
                ui.monospace(human_duration_ms(self.player.duration()));
                ui.label(egui::RichText::new("/").color(ui.visuals().weak_text_color()));
                self.time_field_ui(ui, TimeField::Playhead, self.player.position());
            });
        });
    }

    /// The editing tools row: every keyboard action has a visible, labeled
    /// button (split / cut / marker / zoom), so nothing is keyboard-only.
    fn tools_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let ph = self.project.timeline.playhead();
            if ui
                .button("✂ Split")
                .on_hover_text(
                    "Split the clip into two pieces at the playhead (S). \
                     Drag a cut line to slide it; Backspace joins it again.",
                )
                .clicked()
            {
                self.edit_cuts(|s| s.split_at(ph));
            }
            let piece_cut = self
                .project
                .segments
                .index_at(ph)
                .map(|i| !self.project.segments.segments()[i].enabled)
                .unwrap_or(false);
            let toggle_label = if piece_cut {
                "↩ Keep piece"
            } else {
                "✕ Cut piece"
            };
            if ui
                .button(toggle_label)
                .on_hover_text(
                    "Cut or restore the piece under the playhead (X, or right-click a piece). \
                     Cut pieces are skipped during playback and joined out on export.",
                )
                .clicked()
            {
                self.edit_cuts(|s| s.toggle_at(ph));
            }
            let has_selection = !self.project.timeline.is_full();
            let cut_range = ui.add_enabled(has_selection, egui::Button::new("✕ Cut In→Out"));
            let cut_range = if has_selection {
                cut_range.on_hover_text(
                    "Remove the selected In→Out range in one action: splits at both \
                     ends, cuts the middle, then clears the selection so playback \
                     previews the join.",
                )
            } else {
                cut_range.on_disabled_hover_text(
                    "Select a range first: set In (I) and Out (O) around the part to remove",
                )
            };
            if cut_range.clicked() {
                let (a, b) = (
                    self.project.timeline.in_point(),
                    self.project.timeline.out_point(),
                );
                let dur = self.player.duration();
                self.edit_cuts(|s| s.cut_range(a, b));
                self.project.timeline.set_out(dur);
                self.project.timeline.set_in(0.0);
                self.seek_to(a);
            }
            ui.separator();
            ui.add_enabled_ui(self.project.history.can_undo(), |ui| {
                if ui
                    .button("↶ Undo")
                    .on_hover_text("Undo the last cut change (Ctrl+Z)")
                    .clicked()
                {
                    self.undo_cut_edit();
                }
            });
            ui.add_enabled_ui(self.project.history.can_redo(), |ui| {
                if ui
                    .button("↷ Redo")
                    .on_hover_text("Redo the last cut change (Ctrl+Shift+Z / Ctrl+Y)")
                    .clicked()
                {
                    self.project.redo_cut();
                }
            });
            if !self.project.segments.is_trivial()
                && ui
                    .button("Reset cuts")
                    .on_hover_text("Remove every split and keep the whole clip again")
                    .clicked()
            {
                self.edit_cuts(|s| {
                    s.reset();
                    true
                });
            }
            ui.separator();
            if ui
                .button("⚑ Marker")
                .on_hover_text(
                    "Drop a marker at the playhead (M). Shift+M removes the nearest; \
                     [ and ] jump between markers; trim handles snap to them.\n\
                     Chapters from `ord mark` show up here automatically.",
                )
                .clicked()
            {
                self.add_marker();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let fit = ui
                    .button("Fit")
                    .on_hover_text("Show the whole clip (zoom out fully)");
                crate::a11y::button(&fit, "Zoom fit");
                if fit.clicked() {
                    self.project.zoom = 1.0;
                    self.project.scroll = 0.0;
                }
                let zin = ui.button("+").on_hover_text("Zoom in (wheel or =)");
                crate::a11y::button(&zin, "Zoom in");
                if zin.clicked() {
                    self.project.zoom = (self.project.zoom * 1.5).min(60.0);
                }
                let zout = ui.button("−").on_hover_text("Zoom out (wheel or -)");
                crate::a11y::button(&zout, "Zoom out");
                if zout.clicked() {
                    self.project.zoom = (self.project.zoom / 1.5).max(1.0);
                }
                ui.label(
                    egui::RichText::new(if self.project.zoom > 1.01 {
                        format!("zoom {:.1}×", self.project.zoom)
                    } else {
                        "zoom 1×".to_string()
                    })
                    .size(11.0)
                    .color(ui.visuals().weak_text_color()),
                );
            });
        });
    }

    fn timeline_ui(&mut self, ui: &mut egui::Ui) {
        let accent = crate::theme::AI;
        let dur = self.player.duration().max(1e-6);
        // Taller track: filmstrip on top, waveform strip along the bottom.
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 100.0),
            egui::Sense::hover(),
        );

        // Mouse wheel over the timeline: plain scroll ZOOMS, anchored at the
        // pointer (the moment under the cursor stays put); Shift+wheel or a
        // horizontal wheel/trackpad pans when zoomed in.
        let hovered = ui.rect_contains_pointer(rect);
        if hovered {
            let (scroll_x, scroll_y, shift, pointer) = ui.input(|i| {
                (
                    i.raw_scroll_delta.x,
                    i.raw_scroll_delta.y,
                    i.modifiers.shift,
                    i.pointer.hover_pos(),
                )
            });
            let track_guess = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 4.0, rect.top() + 18.0),
                egui::pos2(rect.right() - 4.0, rect.bottom() - 4.0),
            );
            let pan = scroll_x + if shift { scroll_y } else { 0.0 };
            if pan != 0.0 && self.project.zoom > 1.0 {
                self.project.scroll =
                    (self.project.scroll - pan * 0.01 / self.project.zoom).clamp(0.0, 1.0);
            } else if scroll_y != 0.0 && !shift {
                let frac = pointer
                    .map(|p| {
                        ((p.x - track_guess.left()) / track_guess.width().max(1.0)).clamp(0.0, 1.0)
                    })
                    .unwrap_or(0.5);
                let (zoom, scroll) = timeline::zoom_anchored(
                    dur,
                    self.project.zoom,
                    self.project.scroll,
                    frac,
                    scroll_y,
                    60.0,
                );
                self.project.zoom = zoom;
                self.project.scroll = scroll;
            }
        }

        let view = View::new(dur, self.project.zoom, self.project.scroll);
        self.maybe_regen_filmstrip(&view, ui.ctx());
        let ruler =
            egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.right(), rect.top() + 16.0));
        let track = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 4.0, rect.top() + 18.0),
            egui::pos2(rect.right() - 4.0, rect.bottom() - 4.0),
        );
        // Filmstrip takes the upper band; waveform sits in a thin strip under it.
        let wave_h = if self.wave.as_ref().is_some_and(|w| !w.is_empty()) {
            22.0_f32
        } else {
            0.0
        };
        let film_track = if wave_h > 0.0 {
            egui::Rect::from_min_max(
                track.left_top(),
                egui::pos2(track.right(), track.bottom() - wave_h),
            )
        } else {
            track
        };
        let wave_track = egui::Rect::from_min_max(
            egui::pos2(track.left(), track.bottom() - wave_h),
            track.right_bottom(),
        );
        let painter = ui.painter_at(rect);

        let x_of = |t: f64| track.left() + view.frac_of(t) * track.width();
        let in_x = x_of(self.project.timeline.in_point());
        let out_x = x_of(self.project.timeline.out_point());
        let ph_x = x_of(self.project.timeline.playhead());

        // Track background.
        painter.rect_filled(track, 4.0, crate::theme::RAISED);

        // Filmstrip tiles for the current LOD window (or last cover while regen
        // is in flight), painted into the upper band.
        self.paint_filmstrip(&painter, film_track, &view);
        self.paint_waveform(&painter, wave_track, &view, dur);

        // Selection highlight + dimmed outside.
        let cl = |x: f32| x.clamp(track.left(), track.right());
        let sel = egui::Rect::from_min_max(
            egui::pos2(cl(in_x), track.top()),
            egui::pos2(cl(out_x), track.bottom()),
        );
        painter.rect_filled(sel, 0.0, accent.linear_multiply(0.22));
        let dim = crate::theme::SCRIM;
        painter.rect_filled(
            egui::Rect::from_min_max(track.left_top(), egui::pos2(cl(in_x), track.bottom())),
            0.0,
            dim,
        );
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(cl(out_x), track.top()), track.right_bottom()),
            0.0,
            dim,
        );

        // Cut segments: heavy dim + diagonal hatch + a vermilion edge strip on
        // pieces toggled off, and a draggable grip on every cut line.
        if !self.project.segments.is_trivial() {
            for seg in self.project.segments.segments() {
                if seg.enabled {
                    continue;
                }
                let x0 = cl(x_of(seg.start));
                let x1 = cl(x_of(seg.end));
                if x1 <= x0 {
                    continue;
                }
                let r = egui::Rect::from_min_max(
                    egui::pos2(x0, track.top()),
                    egui::pos2(x1, track.bottom()),
                );
                painter.rect_filled(r, 0.0, crate::theme::SCRIM_CUT);
                // Sparse diagonal hatch: reads as "removed" at a glance.
                let hatch = painter.with_clip_rect(r);
                let stroke = egui::Stroke::new(1.0, crate::theme::SHU.linear_multiply(0.30));
                let mut x = r.left() - r.height();
                while x < r.right() {
                    hatch.line_segment(
                        [
                            egui::pos2(x, r.bottom()),
                            egui::pos2(x + r.height(), r.top()),
                        ],
                        stroke,
                    );
                    x += 14.0;
                }
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x0, track.bottom() - 3.0),
                        egui::pos2(x1, track.bottom()),
                    ),
                    0.0,
                    crate::theme::SHU,
                );
                if r.width() > 42.0 {
                    let c = r.center();
                    let label = egui::Rect::from_center_size(c, egui::vec2(38.0, 14.0));
                    painter.rect_filled(label, 3.0, crate::theme::SCRIM_LABEL);
                    painter.text(
                        c,
                        egui::Align2::CENTER_CENTER,
                        "✕ cut",
                        egui::FontId::proportional(10.0),
                        crate::theme::SHU,
                    );
                }
            }
            // Cut lines get a grip handle mid-track: they look (and are)
            // draggable, like the trim handles. Right-click joins the pieces.
            for cut in self.project.segments.cuts() {
                let x = x_of(cut);
                if track.x_range().contains(x) {
                    painter.line_segment(
                        [egui::pos2(x, track.top()), egui::pos2(x, track.bottom())],
                        egui::Stroke::new(1.0, crate::theme::HAIRLINE_HI),
                    );
                    let grip = egui::Rect::from_center_size(
                        egui::pos2(x, track.center().y),
                        egui::vec2(7.0, 18.0),
                    );
                    painter.rect_filled(grip, 2.0, crate::theme::RAISED);
                    painter.rect_stroke(
                        grip,
                        2.0,
                        egui::Stroke::new(1.0, crate::theme::HAIRLINE_HI),
                    );
                    painter.line_segment(
                        [
                            egui::pos2(x, grip.top() + 4.0),
                            egui::pos2(x, grip.bottom() - 4.0),
                        ],
                        egui::Stroke::new(1.0, crate::theme::INK_2),
                    );
                }
            }
        }

        // Ruler ticks.
        self.paint_ruler(&painter, ruler, track, &view);

        // Markers: a gold line with a small flag head (click to jump to it).
        for &m in self.project.markers.as_slice() {
            let mx = x_of(m);
            if track.x_range().contains(mx) {
                painter.line_segment(
                    [egui::pos2(mx, track.top()), egui::pos2(mx, track.bottom())],
                    egui::Stroke::new(1.5, crate::theme::KIN),
                );
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        egui::pos2(mx - 4.0, track.top()),
                        egui::pos2(mx + 4.0, track.top()),
                        egui::pos2(mx, track.top() + 6.0),
                    ],
                    crate::theme::KIN,
                    egui::Stroke::NONE,
                ));
            }
        }

        // Selection edges + handles.
        for (x, label) in [(in_x, "IN"), (out_x, "OUT")] {
            if track.x_range().contains(x) {
                painter.line_segment(
                    [egui::pos2(x, track.top()), egui::pos2(x, track.bottom())],
                    egui::Stroke::new(2.0, accent),
                );
                let grip = egui::Rect::from_center_size(
                    egui::pos2(x, track.top() + 9.0),
                    egui::vec2(14.0, 18.0),
                );
                painter.rect_filled(grip, 3.0, accent);
                painter.text(
                    grip.center(),
                    egui::Align2::CENTER_CENTER,
                    label,
                    egui::FontId::proportional(8.0),
                    crate::theme::ON_ACCENT,
                );
            }
        }

        // Playhead: a line with a triangle head in the ruler.
        if track.x_range().contains(ph_x) {
            painter.line_segment(
                [
                    egui::pos2(ph_x, rect.top()),
                    egui::pos2(ph_x, track.bottom()),
                ],
                egui::Stroke::new(2.0, crate::theme::PLAYHEAD),
            );
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(ph_x - 5.0, rect.top()),
                    egui::pos2(ph_x + 5.0, rect.top()),
                    egui::pos2(ph_x, rect.top() + 7.0),
                ],
                crate::theme::PLAYHEAD,
                egui::Stroke::NONE,
            ));
        }

        // Hover feedback: a ghost line + time bubble under the pointer, so you
        // always know where a click/scrub will land (research: LosslessCut's
        // hover time, CapCut's scrub feedback).
        if hovered && self.drag.is_none() {
            if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
                if track.contains(p) {
                    let t = view
                        .time_at(((p.x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0))
                        .clamp(0.0, dur);
                    // With cuts present, lightly lift the piece under the
                    // pointer — the target of X / right-click.
                    if !self.project.segments.is_trivial() {
                        if let Some(i) = self.project.segments.index_at(t) {
                            let seg = self.project.segments.segments()[i];
                            let r = egui::Rect::from_min_max(
                                egui::pos2(cl(x_of(seg.start)), track.top()),
                                egui::pos2(cl(x_of(seg.end)), track.bottom()),
                            );
                            painter.rect_filled(r, 0.0, crate::theme::hover_lift());
                        }
                    }
                    painter.line_segment(
                        [
                            egui::pos2(p.x, track.top()),
                            egui::pos2(p.x, track.bottom()),
                        ],
                        egui::Stroke::new(1.0, crate::theme::ghost_line()),
                    );
                    let text = human_duration_ms(t);
                    let galley = painter.layout_no_wrap(
                        text,
                        egui::FontId::proportional(10.0),
                        crate::theme::ON_ACCENT,
                    );
                    let pad = egui::vec2(5.0, 3.0);
                    let mut pos = egui::pos2(p.x + 8.0, track.top() + 4.0);
                    if pos.x + galley.size().x + 2.0 * pad.x > track.right() {
                        pos.x = p.x - 8.0 - galley.size().x - 2.0 * pad.x;
                    }
                    let bubble = egui::Rect::from_min_size(pos, galley.size() + 2.0 * pad);
                    painter.rect_filled(bubble, 3.0, crate::theme::BUBBLE_BG);
                    painter.galley(bubble.min + pad, galley, crate::theme::ON_ACCENT);
                }
            }
        }

        self.timeline_interactions(ui, track, &view);
    }

    fn timeline_interactions(&mut self, ui: &mut egui::Ui, track: egui::Rect, view: &View) {
        let id = ui.id().with("tl");
        let resp = ui.interact(track, id, egui::Sense::click_and_drag());
        crate::a11y::slider(&resp, "Timeline");
        // Capture plain values so the closures don't borrow `self`.
        let dur = self.player.duration();
        let view = *view;
        let time_at = move |x: f32| {
            view.time_at(((x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0))
                .clamp(0.0, dur)
        };
        let x_of = move |t: f64| track.left() + view.frac_of(t) * track.width();

        // Cursor affordance: a horizontal-resize cursor near the trim handles
        // and cut lines says "this edge drags" before you commit to the drag.
        // The grab radius is bigger than the visual handle (fat-finger rule).
        const GRAB: f32 = 10.0;
        let cut_xs: Vec<(usize, f64, f32)> = self
            .project
            .segments
            .cut_points()
            .map(|(i, t)| (i, t, x_of(t)))
            .collect();
        let snap = |markers: &[f64], t: f64| {
            timeline::snap_to_marker(t, markers, &view, track.width(), SNAP_PX)
        };
        if resp.hovered() {
            if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
                let din = (p.x - x_of(self.project.timeline.in_point())).abs();
                let dout = (p.x - x_of(self.project.timeline.out_point())).abs();
                if din <= GRAB
                    || dout <= GRAB
                    || timeline::nearest_cut(p.x, &cut_xs, GRAB).is_some()
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
            }
        }

        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                let target = timeline::classify_drag(
                    p.x,
                    x_of(self.project.timeline.in_point()),
                    x_of(self.project.timeline.out_point()),
                    &cut_xs,
                    GRAB,
                );
                if let DragTarget::Cut(_) = target {
                    self.drag_undo = Some(self.project.segments.clone());
                }
                self.drag = Some(target);
                if self.player.is_playing() {
                    self.player.pause();
                }
                // Seek on grab so the first preview frame lands immediately.
                let t = snap(self.project.markers.as_slice(), time_at(p.x));
                match target {
                    DragTarget::In => {
                        self.project.timeline.set_in(t);
                        self.scrub_to(self.project.timeline.in_point(), true, ui.ctx());
                    }
                    DragTarget::Out => {
                        self.project.timeline.set_out(t);
                        self.scrub_to(self.project.timeline.out_point(), true, ui.ctx());
                    }
                    DragTarget::Playhead => self.scrub_to(t, true, ui.ctx()),
                    DragTarget::Cut(i) => {
                        if let Some(applied) = self.project.segments.move_cut(i, t) {
                            self.scrub_to(applied, true, ui.ctx());
                        }
                    }
                }
            }
        }
        if resp.dragged() {
            if let (Some(drag), Some(p)) = (self.drag, resp.interact_pointer_pos()) {
                // Handles, cut lines, and the playhead snap to nearby markers,
                // so a mark placed in-game becomes a precise edit point.
                let t = snap(self.project.markers.as_slice(), time_at(p.x));
                match drag {
                    DragTarget::In => {
                        self.project.timeline.set_in(t);
                        self.scrub_to(self.project.timeline.in_point(), false, ui.ctx());
                    }
                    DragTarget::Out => {
                        self.project.timeline.set_out(t);
                        self.scrub_to(self.project.timeline.out_point(), false, ui.ctx());
                    }
                    DragTarget::Playhead => self.scrub_to(t, false, ui.ctx()),
                    DragTarget::Cut(i) => {
                        // The preview follows the sliding cut, frame-accurate.
                        if let Some(applied) = self.project.segments.move_cut(i, t) {
                            self.scrub_to(applied, false, ui.ctx());
                        }
                    }
                }
            }
        }
        if resp.drag_stopped() {
            // Force a final seek so the paused preview matches the released
            // playhead (intermediate seeks were throttled during the drag).
            if self.drag.is_some() {
                let ph = self.project.timeline.playhead();
                self.scrub_to(ph, true, ui.ctx());
            }
            if let Some(snapshot) = self.drag_undo.take() {
                if snapshot != self.project.segments {
                    self.project.history.push(snapshot);
                }
            }
            self.drag = None;
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let t = snap(self.project.markers.as_slice(), time_at(p.x));
                self.seek_to(t);
            }
        }
        // Right-click: on a cut line, join the two pieces back together;
        // anywhere else, toggle that piece cut/kept (same as X).
        if resp.secondary_clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                if let Some((_, ct)) = timeline::nearest_cut(p.x, &cut_xs, GRAB) {
                    self.edit_cuts(|s| s.join_at(ct).is_some());
                } else {
                    let t = time_at(p.x);
                    self.edit_cuts(|s| s.toggle_at(t));
                }
            }
        }
    }

    /// Schedule a filmstrip rebuild when the visible window drifts from the
    /// tiles currently on screen. Debounced so continuous zoom/pan does not
    /// spawn a decode storm.
    fn maybe_regen_filmstrip(&mut self, view: &View, ctx: &egui::Context) {
        let desired = (view.start, view.span);
        let (cs, cv) = self.strip_cover;
        // ~half a tile of drift (or any span change) is enough to re-LOD.
        let tile = cv / FILMSTRIP_TILES as f64;
        let drifted = (desired.0 - cs).abs() > tile * 0.5
            || (desired.1 - cv).abs() > cv * 0.08
            || (desired.1 - cv).abs() > 0.05;
        if drifted {
            // First drift in a burst starts the timer; further drags just
            // update the target so we extract the *final* window once.
            if self.strip_pending.is_none() {
                self.strip_pending_at = Instant::now();
            }
            self.strip_pending = Some(desired);
        }
        let Some((start, span)) = self.strip_pending else {
            return;
        };
        if self.strip_pending_at.elapsed() < FILMSTRIP_DEBOUNCE {
            // Keep the UI alive so the debounce fires without a further event.
            ctx.request_repaint_after(FILMSTRIP_DEBOUNCE);
            return;
        }
        self.strip_pending = None;
        self.strip_gen = self.strip_gen.wrapping_add(1);
        self.strip_cover = (start, span.max(1e-6));
        self.strip = vec![None; FILMSTRIP_TILES];
        self.strip_rx = spawn_filmstrip(
            &self.clip,
            self.strip_gen,
            start,
            span.max(1e-6),
            FILMSTRIP_TILES,
            ctx.clone(),
        );
    }

    fn paint_filmstrip(&self, painter: &egui::Painter, track: egui::Rect, view: &View) {
        let n = self.strip.len();
        if n == 0 || track.height() < 2.0 {
            return;
        }
        let (cover_start, cover_span) = self.strip_cover;
        let tile_dur = cover_span / n as f64;
        for (i, slot) in self.strip.iter().enumerate() {
            let Some(tex) = slot else { continue };
            let t0 = cover_start + i as f64 * tile_dur;
            let x0 = track.left() + view.frac_of(t0) * track.width();
            let x1 = track.left() + view.frac_of(t0 + tile_dur) * track.width();
            if x1 < track.left() || x0 > track.right() {
                continue;
            }
            let r = egui::Rect::from_min_max(
                egui::pos2(x0.max(track.left()), track.top()),
                egui::pos2(x1.min(track.right()), track.bottom()),
            );
            if r.width() < 0.5 {
                continue;
            }
            painter.image(
                tex.id(),
                r,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                crate::theme::FILMSTRIP_TINT,
            );
        }
    }

    /// Paint the whole-clip peak envelope into `band`, clipped to the view.
    /// One vertical bar per peak (not a filled polygon — envelopes are not
    /// convex, and bars stay readable when zoomed).
    fn paint_waveform(&self, painter: &egui::Painter, band: egui::Rect, view: &View, dur: f64) {
        let Some(peaks) = self.wave.as_ref() else {
            return;
        };
        if peaks.is_empty() || band.height() < 2.0 || dur <= 0.0 {
            return;
        }
        let n = peaks.len();
        let mid_y = band.center().y;
        let half = (band.height() * 0.45).max(1.0);
        let color = crate::theme::WAVEFORM.linear_multiply(0.90);
        // Only walk peaks that can land in the visible window (plus a little
        // padding so edge bars aren't clipped mid-bar).
        let t0 = view.start - view.span * 0.02;
        let t1 = view.start + view.span * 1.02;
        let i0 = ((t0 / dur) * n as f64).floor().max(0.0) as usize;
        let i1 = (((t1 / dur) * n as f64).ceil() as usize).min(n);
        // Bar width from the denser of peak spacing and one pixel.
        let bar_w = (band.width() * (view.span / dur) as f32 / n as f32).clamp(1.0, 4.0);
        for (i, &peak) in peaks.iter().enumerate().take(i1).skip(i0) {
            let t = (i as f64 + 0.5) * dur / n as f64;
            let x = band.left() + view.frac_of(t) * band.width();
            if x < band.left() - bar_w || x > band.right() + bar_w {
                continue;
            }
            let h = peak.clamp(0.0, 1.0) * half;
            if h < 0.5 {
                continue;
            }
            painter.rect_filled(
                egui::Rect::from_center_size(egui::pos2(x, mid_y), egui::vec2(bar_w, h * 2.0)),
                0.0,
                color,
            );
        }
    }

    fn paint_ruler(
        &self,
        painter: &egui::Painter,
        ruler: egui::Rect,
        track: egui::Rect,
        view: &View,
    ) {
        let step = nice_step(view.span);
        let weak = crate::theme::RULER_TEXT;
        let mut t = (view.start / step).floor() * step;
        while t <= view.start + view.span {
            if t >= 0.0 {
                let x = track.left() + view.frac_of(t) * track.width();
                if track.x_range().contains(x) {
                    painter.line_segment(
                        [
                            egui::pos2(x, ruler.bottom() - 4.0),
                            egui::pos2(x, ruler.bottom()),
                        ],
                        egui::Stroke::new(1.0, weak),
                    );
                    painter.text(
                        egui::pos2(x + 2.0, ruler.top()),
                        egui::Align2::LEFT_TOP,
                        human_duration(t),
                        egui::FontId::proportional(9.5),
                        weak,
                    );
                }
            }
            t += step;
        }
    }

    /// Click-to-type numeric time (`m:ss.mmm` / plain seconds). Enter commits,
    /// Escape (handled in [`Self::keyboard`]) or focus loss without a parse
    /// cancels; a valid parse applies and seeks.
    fn time_field_ui(&mut self, ui: &mut egui::Ui, field: TimeField, value: f64) {
        let editing = self.time_edit.as_ref().map(|(f, _)| *f) == Some(field);
        if editing {
            let buf = self
                .time_edit
                .as_ref()
                .map(|(_, s)| s.clone())
                .unwrap_or_default();
            let mut edit = buf;
            let a11y = match field {
                TimeField::In => "In time",
                TimeField::Out => "Out time",
                TimeField::Playhead => "Playhead time",
            };
            let resp = ui.add(
                egui::TextEdit::singleline(&mut edit)
                    .desired_width(78.0)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("m:ss.mmm"),
            );
            crate::a11y::text_input(&resp, a11y);
            // Grab keyboard on the first paint of the edit field.
            if !resp.has_focus() && !resp.lost_focus() {
                resp.request_focus();
            }
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
            if resp.lost_focus() || enter {
                if let Some(t) = parse_duration(edit.trim()) {
                    match field {
                        TimeField::In => {
                            self.project.timeline.set_in(t);
                            self.seek_to(self.project.timeline.in_point());
                        }
                        TimeField::Out => {
                            self.project.timeline.set_out(t);
                            self.seek_to(self.project.timeline.out_point());
                        }
                        TimeField::Playhead => self.seek_to(t),
                    }
                }
                self.time_edit = None;
            } else {
                self.time_edit = Some((field, edit));
            }
        } else {
            let a11y = match field {
                TimeField::In => "In time",
                TimeField::Out => "Out time",
                TimeField::Playhead => "Playhead time",
            };
            let label = human_duration_ms(value);
            let resp = ui
                .add(
                    egui::Label::new(egui::RichText::new(label).monospace())
                        .sense(egui::Sense::click()),
                )
                .on_hover_text("Click to type a time (m:ss.mmm or seconds)");
            crate::a11y::clickable_label(&resp, a11y);
            if resp.clicked() {
                self.time_edit = Some((field, human_duration_ms(value)));
            }
        }
    }

    fn export_ui(&mut self, ui: &mut egui::Ui, action: &mut EditorAction) {
        let cuts_active = !self.project.segments.is_trivial();
        let kept = self.project.segments.kept_duration(
            self.project.timeline.in_point(),
            self.project.timeline.out_point(),
        );
        ui.horizontal(|ui| {
            let weak = ui.visuals().weak_text_color();
            ui.label(egui::RichText::new("In").color(weak));
            let in_t = self.project.timeline.in_point();
            self.time_field_ui(ui, TimeField::In, in_t);
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Out").color(weak));
            let out_t = self.project.timeline.out_point();
            self.time_field_ui(ui, TimeField::Out, out_t);
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(if cuts_active { "Kept" } else { "Selection" }).color(weak),
            );
            ui.monospace(human_duration(if cuts_active {
                kept
            } else {
                self.project.timeline.selection_duration()
            }));
            if cuts_active {
                ui.add_space(6.0);
                let removed = self
                    .project
                    .segments
                    .segments()
                    .iter()
                    .filter(|s| !s.enabled)
                    .count();
                ui.label(
                    egui::RichText::new(format!(
                        "{} piece{} cut out",
                        removed,
                        if removed == 1 { "" } else { "s" }
                    ))
                    .color(crate::theme::KIN)
                    .size(12.0),
                );
            }
            ui.add_space(12.0);
            ui.checkbox(&mut self.project.mute_export, "Mute audio");
            if let Some(p) = self.export_progress {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!("Exporting… {}%", (p * 100.0) as u32))
                        .color(crate::theme::KIN),
                );
                ui.add(
                    egui::ProgressBar::new(p.clamp(0.0, 1.0))
                        .desired_width(120.0)
                        .show_percentage(),
                );
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let trim = self.current_trim();
                let segments = self.export_segments();
                let mute = self.project.mute_export;
                let speed = self.project.speed;
                let empty_cuts = segments.as_ref().is_some_and(|s| s.is_empty());
                let can_save = !empty_cuts && self.export_progress.is_none();

                if ui
                    .add_enabled(can_save, egui::Button::new("Save as clip"))
                    .on_hover_text(
                        "Write the selection into the clips library (stream-copy when \
                         possible; re-encodes when cuts or non-1× speed require it)",
                    )
                    .on_disabled_hover_text(if empty_cuts {
                        "Every piece is cut — nothing to save"
                    } else {
                        "An export is already running"
                    })
                    .clicked()
                {
                    *action = EditorAction::SaveAsClip {
                        trim,
                        segments: segments.clone(),
                        mute,
                        speed,
                    };
                }

                ui.menu_button(egui::RichText::new("Export selection").strong(), |ui| {
                    let dur = if cuts_active {
                        kept
                    } else {
                        self.project.timeline.selection_duration()
                    } / self.project.speed.max(0.25) as f64;
                    if empty_cuts {
                        ui.label(
                            egui::RichText::new("Every piece is cut — nothing to export")
                                .color(crate::theme::KIN),
                        );
                        return;
                    }
                    for preset in Preset::ALL {
                        // Joining cuts re-encodes through the concat filter, so
                        // stream-copy / GIF / audio presets need a whole clip.
                        // Speed ≠ 1× also forces re-encode (setpts/atempo).
                        let unsupported = (segments.is_some()
                            && matches!(preset, Preset::Source | Preset::Gif | Preset::AudioOnly))
                            || ((speed - 1.0).abs() > 0.01
                                && matches!(
                                    preset,
                                    Preset::Source | Preset::Gif | Preset::AudioOnly
                                ));
                        // Size-predictable presets show an honest estimate from
                        // the planner's own bitrate math.
                        let mut profile = preset.profile();
                        profile.mute = mute;
                        profile.speed = speed as f64;
                        let label = match ord_export::estimated_output_mib(&profile, dur) {
                            Some(est) if est > 0.0 => {
                                format!("{}  (~{est:.1} MiB)", preset.label())
                            }
                            _ => preset.label().to_string(),
                        };
                        let btn = ui.add_enabled(!unsupported, egui::Button::new(label));
                        let btn = if unsupported {
                            btn.on_disabled_hover_text(
                                "Not available with cuts or non-1× speed (re-encode required)",
                            )
                        } else {
                            btn
                        };
                        if btn.clicked() {
                            *action = EditorAction::Export {
                                preset,
                                trim,
                                segments: segments.clone(),
                                mute,
                                speed,
                            };
                            ui.close_menu();
                        }
                    }
                });
            });
        });
    }

    fn current_trim(&self) -> Option<Trim> {
        if self.project.timeline.is_full() {
            None
        } else {
            Some(Trim {
                start_secs: self.project.timeline.in_point(),
                end_secs: self.project.timeline.out_point(),
            })
        }
    }

    /// The cut list for export (see [`timeline::export_spans`]).
    fn export_segments(&self) -> Option<Vec<Trim>> {
        timeline::export_spans(
            &self.project.segments,
            self.project.timeline.in_point(),
            self.project.timeline.out_point(),
        )
        .map(|spans| {
            spans
                .into_iter()
                .map(|(a, b)| Trim {
                    start_secs: a,
                    end_secs: b,
                })
                .collect()
        })
    }
}

/// A "nice" ruler step (seconds) so ~6-10 labels fit a span.
fn nice_step(span: f64) -> f64 {
    const STEPS: [f64; 11] = [
        0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    ];
    for s in STEPS {
        if span / s <= 10.0 {
            return s;
        }
    }
    600.0
}

/// Probe the clip's chapters (`ord mark` bookmarks become MKV chapters)
/// off-thread; they surface as timeline markers.
fn spawn_chapters(clip: &std::path::Path, ctx: egui::Context) -> Receiver<Vec<f64>> {
    let (tx, rx) = channel();
    let clip = clip.to_path_buf();
    std::thread::spawn(move || {
        if let Ok(chapters) = ord_export::probe::probe_chapters(&clip) {
            if !chapters.is_empty() && tx.send(chapters).is_ok() {
                ctx.request_repaint();
            }
        }
    });
    rx
}

/// Decode `n_tiles` evenly-spaced thumbnails covering `[start, start+span]`
/// off-thread. Each message is `(generation, tile_index, image)` so a newer
/// zoom/pan job can supersede an in-flight one.
fn spawn_filmstrip(
    clip: &std::path::Path,
    gen: u64,
    start: f64,
    span: f64,
    n_tiles: usize,
    ctx: egui::Context,
) -> Receiver<(u64, usize, egui::ColorImage)> {
    let (tx, rx) = channel();
    let clip = clip.to_path_buf();
    std::thread::spawn(move || {
        if span <= 0.0 || n_tiles == 0 {
            return;
        }
        for i in 0..n_tiles {
            let t = start + (i as f64 + 0.5) * span / n_tiles as f64;
            if let Some(img) = extract_thumb(&clip, t, 160) {
                if tx.send((gen, i, img)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        }
    });
    rx
}

/// Decode a whole-clip peak envelope off-thread for the timeline waveform.
fn spawn_waveform(
    clip: &std::path::Path,
    n_peaks: usize,
    ctx: egui::Context,
) -> Receiver<Vec<f32>> {
    let (tx, rx) = channel();
    let clip = clip.to_path_buf();
    std::thread::spawn(move || {
        let peaks = crate::meta::extract_audio_peaks(&clip, n_peaks);
        if !peaks.is_empty() && tx.send(peaks).is_ok() {
            ctx.request_repaint();
        }
    });
    rx
}

/// Extract one frame at `secs` (via the shared [`meta`](crate::meta) extractor)
/// and decode it to an egui image.
fn extract_thumb(clip: &std::path::Path, secs: f64, width: u32) -> Option<egui::ColorImage> {
    let jpeg = crate::meta::extract_frame_jpeg(clip, secs, width)?;
    let img = image::load_from_memory(&jpeg).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        img.as_raw(),
    ))
}
