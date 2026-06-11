//! Clip trim editor (behind the `gui` feature).
//!
//! An inline A/V editor built on [`Player`]: a video preview with real
//! play/pause/loop + audio, a draggable in/out timeline with a time ruler,
//! filmstrip, markers (the clip's `ord mark` chapters load automatically) and
//! multi-segment cuts (split at the playhead, toggle pieces off — playback and
//! export skip them), keyboard shortcuts, and an "Export selection" that hands
//! the result to `ord-export`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use ord_export::{Preset, Trim};

use crate::format::human_duration;
use crate::player::{Player, PreviewFrame};
use crate::timeline::{Segments, Timeline, View};

const FILMSTRIP_TILES: usize = 14;
/// Snap radius (px) for markers under the in/out/playhead drags and clicks.
const SNAP_PX: f32 = 6.0;

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
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    In,
    Out,
    Playhead,
    /// Sliding cut boundary `i` (see [`Segments::cut_points`]).
    Cut(usize),
}

/// Editor state for a single clip.
pub struct EditorState {
    clip: PathBuf,
    label: String,
    player: Player,
    timeline: Timeline,
    segments: Segments,
    /// Snapshots for Ctrl+Z over cut edits (split/toggle/join/move/reset).
    undo: Vec<Segments>,
    zoom: f32,
    scroll: f32,
    markers: Vec<f64>,
    /// Chapters probed off-thread land here (the clip's `ord mark` bookmarks).
    chapters_rx: Receiver<Vec<f64>>,
    mute_export: bool,
    volume: f32,
    drag: Option<Drag>,
    /// Pre-drag snapshot for a cut-line drag (one undo entry per drag).
    drag_undo: Option<Segments>,
    strip_rx: Receiver<(usize, egui::ColorImage)>,
    strip: Vec<Option<egui::TextureHandle>>,
    debug: bool,
    dbg_log_at: Instant,
}

impl EditorState {
    /// Open the editor for `clip`. Returns an error if the media can't be opened.
    pub fn new(clip: PathBuf, label: String, ctx: &egui::Context) -> Result<Self, String> {
        let player = Player::open(&clip)?;
        let timeline = Timeline::new(player.duration());
        let segments = Segments::new(player.duration());
        let strip_rx = spawn_filmstrip(&clip, player.duration(), ctx.clone());
        let chapters_rx = spawn_chapters(&clip, ctx.clone());
        Ok(Self {
            clip,
            label,
            player,
            timeline,
            segments,
            undo: Vec::new(),
            zoom: 1.0,
            scroll: 0.0,
            markers: Vec::new(),
            chapters_rx,
            mute_export: false,
            volume: 1.0,
            drag: None,
            drag_undo: None,
            strip_rx,
            strip: vec![None; FILMSTRIP_TILES],
            debug: crate::tuning::debug_overlay(),
            dbg_log_at: Instant::now(),
        })
    }

    pub fn clip(&self) -> &PathBuf {
        &self.clip
    }

    /// Pause playback (used when the window loses focus / is hidden).
    pub fn pause_player(&mut self) {
        if self.player.is_playing() {
            self.player.pause();
        }
    }

    fn seek_to(&mut self, t: f64) {
        self.timeline.set_playhead(t);
        self.player.seek(t);
    }

    fn push_undo(&mut self, snapshot: Segments) {
        const MAX_UNDO: usize = 64;
        self.undo.push(snapshot);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
    }

    /// Run a cut mutation with undo: the previous state is kept only when the
    /// mutation reports an actual change.
    fn edit_cuts(&mut self, f: impl FnOnce(&mut Segments) -> bool) {
        let snapshot = self.segments.clone();
        if f(&mut self.segments) {
            self.push_undo(snapshot);
        }
    }

    /// Revert the most recent cut edit (Ctrl+Z / the Undo button).
    fn undo_cut_edit(&mut self) {
        if let Some(prev) = self.undo.pop() {
            self.segments = prev;
        }
    }

    /// Place a marker at the playhead (deduplicated, kept sorted).
    fn add_marker(&mut self) {
        let ph = self.timeline.playhead();
        if !self.markers.iter().any(|m| (m - ph).abs() < 1e-3) {
            self.markers.push(ph);
            self.markers
                .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }
    }

    /// Render the editor; returns the action the user took (if any).
    pub fn ui(&mut self, ctx: &egui::Context, wd: &crate::diag::Watchdog) -> EditorAction {
        // Pull in any decoded filmstrip tiles.
        while let Ok((i, img)) = self.strip_rx.try_recv() {
            if let Some(slot) = self.strip.get_mut(i) {
                *slot =
                    Some(ctx.load_texture(format!("strip-{i}"), img, egui::TextureOptions::LINEAR));
            }
        }
        // The clip's chapters (`ord mark` bookmarks) arrive as markers.
        while let Ok(chapters) = self.chapters_rx.try_recv() {
            for t in chapters {
                if !self.markers.iter().any(|m| (m - t).abs() < 1e-3) {
                    self.markers.push(t);
                }
            }
            self.markers
                .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }

        // Player drives the playhead during playback; keep its range in sync.
        self.player
            .set_range(self.timeline.in_point(), self.timeline.out_point());
        if self.player.is_playing() {
            self.timeline.set_playhead(self.player.position());
            // Cut segments are skipped live, so the preview plays exactly what
            // an export would contain.
            if !self.segments.is_trivial() {
                let pos = self.timeline.playhead();
                if let Some(target) = self.segments.skip_target(pos, self.timeline.out_point()) {
                    self.seek_to(target);
                }
            }
        }

        let mut action = self.keyboard(ctx);

        egui::TopBottomPanel::top("editor-top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("← Back").clicked() {
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
        // The editor owns the keyboard — it has no text fields. Drop any
        // lingering widget focus (a clicked button/slider keeps egui focus)
        // so Space can never double-trigger the last-clicked control.
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
        let (split, toggle_seg, prev_marker, next_marker, join, undo_key) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::S),
                i.key_pressed(egui::Key::X),
                i.key_pressed(egui::Key::OpenBracket),
                i.key_pressed(egui::Key::CloseBracket),
                i.key_pressed(egui::Key::Backspace) || i.key_pressed(egui::Key::Delete),
                i.modifiers.command && i.key_pressed(egui::Key::Z),
            )
        });

        if esc {
            return EditorAction::Back;
        }
        if space {
            self.player.toggle();
        }
        let ph = self.timeline.playhead();
        if key_i {
            self.timeline.set_in(ph);
            self.seek_to(self.timeline.in_point());
        }
        if key_o {
            self.timeline.set_out(ph);
            self.seek_to(self.timeline.out_point());
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
            self.seek_to(self.timeline.in_point());
        }
        if end {
            self.seek_to(self.timeline.out_point());
        }
        if plus {
            self.zoom = (self.zoom * 1.5).min(60.0);
        }
        if minus {
            self.zoom = (self.zoom / 1.5).max(1.0);
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
        if ctx.input(|i| i.key_pressed(egui::Key::F3)) {
            self.debug = !self.debug;
        }
        EditorAction::None
    }

    /// The nearest marker strictly before `t`.
    fn prev_marker(&self, t: f64) -> Option<f64> {
        self.markers
            .iter()
            .copied()
            .filter(|m| *m < t - 1e-3)
            .fold(None, |acc: Option<f64>, m| {
                Some(acc.map_or(m, |a| a.max(m)))
            })
    }

    /// The nearest marker strictly after `t`.
    fn next_marker(&self, t: f64) -> Option<f64> {
        self.markers
            .iter()
            .copied()
            .filter(|m| *m > t + 1e-3)
            .fold(None, |acc: Option<f64>, m| {
                Some(acc.map_or(m, |a| a.min(m)))
            })
    }

    /// Remove the marker closest to `t` (within a second), e.g. a misplaced M.
    fn remove_nearest_marker(&mut self, t: f64) {
        let nearest = self
            .markers
            .iter()
            .enumerate()
            .map(|(i, m)| (i, (m - t).abs()))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((i, d)) = nearest {
            if d <= 1.0 {
                self.markers.remove(i);
            }
        }
    }

    /// Snap `t` to the nearest marker within [`SNAP_PX`] on screen.
    fn snap_to_marker(&self, t: f64, view: &View, track_w: f32) -> f64 {
        let px_per_sec = track_w as f64 / view.span.max(1e-9);
        let radius = SNAP_PX as f64 / px_per_sec.max(1e-9);
        self.markers
            .iter()
            .copied()
            .map(|m| (m, (m - t).abs()))
            .filter(|(_, d)| *d <= radius)
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(m, _)| m)
            .unwrap_or(t)
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
                        self.zoom
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
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/ord-ui-debug.log")
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
                    if ui.add(img).clicked() {
                        self.player.toggle();
                    }
                });
            }
            PreviewFrame::Gl => {
                let [tw, th] = self.player.video_size().unwrap_or([16, 9]);
                let size = fit(tw, th);
                ui.vertical_centered(|ui| {
                    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
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
            if ui
                .button("⏮ In")
                .on_hover_text("Jump to in (Home)")
                .clicked()
            {
                self.seek_to(self.timeline.in_point());
            }
            if ui
                .button("−1f")
                .on_hover_text("Previous frame (,)")
                .clicked()
            {
                self.seek_to(self.timeline.playhead() - frame);
            }
            let play_label = if self.player.is_playing() {
                "⏸ Pause"
            } else {
                "▶ Play"
            };
            if ui
                .button(egui::RichText::new(play_label).strong())
                .clicked()
            {
                self.player.toggle();
            }
            if ui.button("+1f").on_hover_text("Next frame (.)").clicked() {
                self.seek_to(self.timeline.playhead() + frame);
            }
            if ui
                .button("Out ⏭")
                .on_hover_text("Jump to out (End)")
                .clicked()
            {
                self.seek_to(self.timeline.out_point());
            }

            ui.separator();
            let mut looping = self.player.looping();
            if ui.selectable_label(looping, "↻ Loop").clicked() {
                looping = !looping;
                self.player.set_loop(looping);
            }

            if self.player.has_audio() {
                ui.separator();
                ui.label("Vol");
                if ui
                    .add(
                        egui::Slider::new(&mut self.volume, 0.0..=1.0)
                            .show_value(false)
                            .fixed_decimals(2),
                    )
                    .changed()
                {
                    self.player.set_volume(self.volume);
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let pos = self.player.position();
                ui.monospace(format!(
                    "{} / {}",
                    human_duration(pos),
                    human_duration(self.player.duration())
                ));
            });
        });
    }

    /// The editing tools row: every keyboard action has a visible, labeled
    /// button (split / cut / marker / zoom), so nothing is keyboard-only.
    fn tools_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let ph = self.timeline.playhead();
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
                .segments
                .index_at(ph)
                .map(|i| !self.segments.segments()[i].enabled)
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
            let has_selection = !self.timeline.is_full();
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
                let (a, b) = (self.timeline.in_point(), self.timeline.out_point());
                let dur = self.player.duration();
                self.edit_cuts(|s| s.cut_range(a, b));
                self.timeline.set_out(dur);
                self.timeline.set_in(0.0);
                self.seek_to(a);
            }
            ui.separator();
            ui.add_enabled_ui(!self.undo.is_empty(), |ui| {
                if ui
                    .button("↶ Undo")
                    .on_hover_text("Undo the last cut change (Ctrl+Z)")
                    .clicked()
                {
                    self.undo_cut_edit();
                }
            });
            if !self.segments.is_trivial()
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
                if ui
                    .button("Fit")
                    .on_hover_text("Show the whole clip (zoom out fully)")
                    .clicked()
                {
                    self.zoom = 1.0;
                    self.scroll = 0.0;
                }
                if ui
                    .button("+")
                    .on_hover_text("Zoom in (wheel or =)")
                    .clicked()
                {
                    self.zoom = (self.zoom * 1.5).min(60.0);
                }
                if ui
                    .button("−")
                    .on_hover_text("Zoom out (wheel or -)")
                    .clicked()
                {
                    self.zoom = (self.zoom / 1.5).max(1.0);
                }
                ui.label(
                    egui::RichText::new(if self.zoom > 1.01 {
                        format!("zoom {:.1}×", self.zoom)
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
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 84.0), egui::Sense::hover());

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
            if pan != 0.0 && self.zoom > 1.0 {
                self.scroll = (self.scroll - pan * 0.01 / self.zoom).clamp(0.0, 1.0);
            } else if scroll_y != 0.0 && !shift {
                let old = View::new(dur, self.zoom, self.scroll);
                let frac = pointer
                    .map(|p| {
                        ((p.x - track_guess.left()) / track_guess.width().max(1.0)).clamp(0.0, 1.0)
                    })
                    .unwrap_or(0.5);
                let anchor = old.time_at(frac);
                let factor = (scroll_y as f64 * 0.005).exp() as f32;
                self.zoom = (self.zoom * factor).clamp(1.0, 60.0);
                // Re-derive scroll so `anchor` stays under the pointer.
                let span = dur / self.zoom as f64;
                let start = (anchor - frac as f64 * span).clamp(0.0, (dur - span).max(0.0));
                let max_start = (dur - span).max(1e-9);
                self.scroll = (start / max_start).clamp(0.0, 1.0) as f32;
            }
        }

        let view = View::new(dur, self.zoom, self.scroll);
        let ruler =
            egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.right(), rect.top() + 16.0));
        let track = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 4.0, rect.top() + 18.0),
            egui::pos2(rect.right() - 4.0, rect.bottom() - 4.0),
        );
        let painter = ui.painter_at(rect);

        let x_of = |t: f64| track.left() + view.frac_of(t) * track.width();
        let in_x = x_of(self.timeline.in_point());
        let out_x = x_of(self.timeline.out_point());
        let ph_x = x_of(self.timeline.playhead());

        // Track background.
        painter.rect_filled(track, 4.0, crate::theme::RAISED);

        // Filmstrip tiles spread across the (whole-clip) track, clipped to view.
        self.paint_filmstrip(&painter, track, &view, dur);

        // Selection highlight + dimmed outside.
        let cl = |x: f32| x.clamp(track.left(), track.right());
        let sel = egui::Rect::from_min_max(
            egui::pos2(cl(in_x), track.top()),
            egui::pos2(cl(out_x), track.bottom()),
        );
        painter.rect_filled(sel, 0.0, accent.linear_multiply(0.22));
        let dim = egui::Color32::from_black_alpha(130);
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
        if !self.segments.is_trivial() {
            for seg in self.segments.segments() {
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
                painter.rect_filled(r, 0.0, egui::Color32::from_black_alpha(190));
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
                    painter.rect_filled(label, 3.0, egui::Color32::from_black_alpha(170));
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
            for cut in self.segments.cuts() {
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
        for &m in &self.markers {
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
                    egui::Color32::WHITE,
                );
            }
        }

        // Playhead: a white line with a triangle head in the ruler.
        if track.x_range().contains(ph_x) {
            painter.line_segment(
                [
                    egui::pos2(ph_x, rect.top()),
                    egui::pos2(ph_x, track.bottom()),
                ],
                egui::Stroke::new(2.0, egui::Color32::WHITE),
            );
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(ph_x - 5.0, rect.top()),
                    egui::pos2(ph_x + 5.0, rect.top()),
                    egui::pos2(ph_x, rect.top() + 7.0),
                ],
                egui::Color32::WHITE,
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
                    if !self.segments.is_trivial() {
                        if let Some(i) = self.segments.index_at(t) {
                            let seg = self.segments.segments()[i];
                            let r = egui::Rect::from_min_max(
                                egui::pos2(cl(x_of(seg.start)), track.top()),
                                egui::pos2(cl(x_of(seg.end)), track.bottom()),
                            );
                            painter.rect_filled(r, 0.0, egui::Color32::from_white_alpha(5));
                        }
                    }
                    painter.line_segment(
                        [
                            egui::pos2(p.x, track.top()),
                            egui::pos2(p.x, track.bottom()),
                        ],
                        egui::Stroke::new(1.0, egui::Color32::from_white_alpha(70)),
                    );
                    let text = human_duration(t);
                    let galley = painter.layout_no_wrap(
                        text,
                        egui::FontId::proportional(10.0),
                        egui::Color32::WHITE,
                    );
                    let pad = egui::vec2(5.0, 3.0);
                    let mut pos = egui::pos2(p.x + 8.0, track.top() + 4.0);
                    if pos.x + galley.size().x + 2.0 * pad.x > track.right() {
                        pos.x = p.x - 8.0 - galley.size().x - 2.0 * pad.x;
                    }
                    let bubble = egui::Rect::from_min_size(pos, galley.size() + 2.0 * pad);
                    painter.rect_filled(bubble, 3.0, egui::Color32::from_black_alpha(200));
                    painter.galley(bubble.min + pad, galley, egui::Color32::WHITE);
                }
            }
        }

        self.timeline_interactions(ui, track, &view);
    }

    fn timeline_interactions(&mut self, ui: &mut egui::Ui, track: egui::Rect, view: &View) {
        let id = ui.id().with("tl");
        let resp = ui.interact(track, id, egui::Sense::click_and_drag());
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
        let cut_pts: Vec<(usize, f64)> = self.segments.cut_points().collect();
        let nearest_cut = move |px: f32| -> Option<(usize, f64)> {
            cut_pts
                .iter()
                .map(|&(i, t)| (i, t, (x_of(t) - px).abs()))
                .filter(|(_, _, d)| *d <= GRAB)
                .min_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(i, t, _)| (i, t))
        };
        if resp.hovered() {
            if let Some(p) = ui.input(|i| i.pointer.hover_pos()) {
                let din = (p.x - x_of(self.timeline.in_point())).abs();
                let dout = (p.x - x_of(self.timeline.out_point())).abs();
                if din <= GRAB || dout <= GRAB || nearest_cut(p.x).is_some() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
            }
        }

        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                // Pick the nearest in/out handle, else a cut line, else scrub.
                let din = (p.x - x_of(self.timeline.in_point())).abs();
                let dout = (p.x - x_of(self.timeline.out_point())).abs();
                self.drag = Some(if din <= GRAB && din <= dout {
                    Drag::In
                } else if dout <= GRAB {
                    Drag::Out
                } else if let Some((i, _)) = nearest_cut(p.x) {
                    self.drag_undo = Some(self.segments.clone());
                    Drag::Cut(i)
                } else {
                    Drag::Playhead
                });
                if self.player.is_playing() {
                    self.player.pause();
                }
            }
        }
        if resp.dragged() {
            if let (Some(drag), Some(p)) = (self.drag, resp.interact_pointer_pos()) {
                // Handles, cut lines, and the playhead snap to nearby markers,
                // so a mark placed in-game becomes a precise edit point.
                let t = self.snap_to_marker(time_at(p.x), &view, track.width());
                match drag {
                    Drag::In => {
                        self.timeline.set_in(t);
                        self.seek_to(self.timeline.in_point());
                    }
                    Drag::Out => {
                        self.timeline.set_out(t);
                        self.seek_to(self.timeline.out_point());
                    }
                    Drag::Playhead => self.seek_to(t),
                    Drag::Cut(i) => {
                        // The preview follows the sliding cut, frame-accurate.
                        if let Some(applied) = self.segments.move_cut(i, t) {
                            self.seek_to(applied);
                        }
                    }
                }
            }
        }
        if resp.drag_stopped() {
            if let Some(snapshot) = self.drag_undo.take() {
                if snapshot != self.segments {
                    self.push_undo(snapshot);
                }
            }
            self.drag = None;
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let t = self.snap_to_marker(time_at(p.x), &view, track.width());
                self.seek_to(t);
            }
        }
        // Right-click: on a cut line, join the two pieces back together;
        // anywhere else, toggle that piece cut/kept (same as X).
        if resp.secondary_clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                if let Some((_, ct)) = nearest_cut(p.x) {
                    self.edit_cuts(|s| s.join_at(ct).is_some());
                } else {
                    let t = time_at(p.x);
                    self.edit_cuts(|s| s.toggle_at(t));
                }
            }
        }
    }

    fn paint_filmstrip(&self, painter: &egui::Painter, track: egui::Rect, view: &View, dur: f64) {
        let n = self.strip.len();
        if n == 0 {
            return;
        }
        let tile_dur = dur / n as f64;
        for (i, slot) in self.strip.iter().enumerate() {
            let Some(tex) = slot else { continue };
            let t0 = i as f64 * tile_dur;
            let x0 = track.left() + view.frac_of(t0) * track.width();
            let x1 = track.left() + view.frac_of(t0 + tile_dur) * track.width();
            if x1 < track.left() || x0 > track.right() {
                continue;
            }
            let r = egui::Rect::from_min_max(
                egui::pos2(x0.max(track.left()), track.top()),
                egui::pos2(x1.min(track.right()), track.bottom()),
            );
            painter.image(
                tex.id(),
                r,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::from_gray(150),
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
        let weak = egui::Color32::from_gray(120);
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

    fn export_ui(&mut self, ui: &mut egui::Ui, action: &mut EditorAction) {
        let cuts_active = !self.segments.is_trivial();
        let kept = self
            .segments
            .kept_duration(self.timeline.in_point(), self.timeline.out_point());
        ui.horizontal(|ui| {
            let weak = ui.visuals().weak_text_color();
            ui.label(egui::RichText::new("In").color(weak));
            ui.monospace(human_duration(self.timeline.in_point()));
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Out").color(weak));
            ui.monospace(human_duration(self.timeline.out_point()));
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(if cuts_active { "Kept" } else { "Selection" }).color(weak),
            );
            ui.monospace(human_duration(if cuts_active {
                kept
            } else {
                self.timeline.selection_duration()
            }));
            if cuts_active {
                ui.add_space(6.0);
                let removed = self
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
            ui.checkbox(&mut self.mute_export, "Mute audio");

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(egui::RichText::new("Export selection").strong(), |ui| {
                    let trim = self.current_trim();
                    let segments = self.export_segments();
                    let mute = self.mute_export;
                    let dur = if cuts_active {
                        kept
                    } else {
                        self.timeline.selection_duration()
                    };
                    if segments.as_ref().is_some_and(|s| s.is_empty()) {
                        ui.label(
                            egui::RichText::new("Every piece is cut — nothing to export")
                                .color(crate::theme::KIN),
                        );
                        return;
                    }
                    for preset in Preset::ALL {
                        // Joining cuts re-encodes through the concat filter, so
                        // stream-copy / GIF / audio presets need a whole clip.
                        let unsupported = segments.is_some()
                            && matches!(preset, Preset::Source | Preset::Gif | Preset::AudioOnly);
                        // Size-predictable presets show an honest estimate from
                        // the planner's own bitrate math.
                        let mut profile = preset.profile();
                        profile.mute = mute;
                        let label = match ord_export::estimated_output_mib(&profile, dur) {
                            Some(est) if est > 0.0 => {
                                format!("{}  (~{est:.1} MiB)", preset.label())
                            }
                            _ => preset.label().to_string(),
                        };
                        let btn = ui.add_enabled(!unsupported, egui::Button::new(label));
                        let btn = if unsupported {
                            btn.on_disabled_hover_text(
                                "Not available with cuts (joining pieces re-encodes video)",
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
                            };
                            ui.close_menu();
                        }
                    }
                });
            });
        });
    }

    fn current_trim(&self) -> Option<Trim> {
        if self.timeline.is_full() {
            None
        } else {
            Some(Trim {
                start_secs: self.timeline.in_point(),
                end_secs: self.timeline.out_point(),
            })
        }
    }

    /// The cut list for export: the kept spans inside in/out, or `None` when
    /// nothing is actually cut out (a plain trim covers it — no re-encode
    /// detour through the concat filter).
    fn export_segments(&self) -> Option<Vec<Trim>> {
        if self.segments.is_trivial() {
            return None;
        }
        let (i, o) = (self.timeline.in_point(), self.timeline.out_point());
        let spans = self.segments.kept_within(i, o);
        if spans.len() == 1 && spans[0].0 <= i + 1e-6 && spans[0].1 >= o - 1e-6 {
            return None; // splits exist but every piece is kept
        }
        Some(
            spans
                .into_iter()
                .map(|(a, b)| Trim {
                    start_secs: a,
                    end_secs: b,
                })
                .collect(),
        )
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

/// Decode `count` evenly-spaced thumbnails for the timeline filmstrip off-thread.
fn spawn_filmstrip(
    clip: &std::path::Path,
    duration: f64,
    ctx: egui::Context,
) -> Receiver<(usize, egui::ColorImage)> {
    let (tx, rx) = channel();
    let clip = clip.to_path_buf();
    std::thread::spawn(move || {
        if duration <= 0.0 {
            return;
        }
        for i in 0..FILMSTRIP_TILES {
            let t = (i as f64 + 0.5) * duration / FILMSTRIP_TILES as f64;
            if let Some(img) = extract_thumb(&clip, t, 160) {
                if tx.send((i, img)).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
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
