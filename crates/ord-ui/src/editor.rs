//! Clip trim editor (behind the `gui` feature).
//!
//! An inline A/V editor built on [`Player`]: a video preview with real
//! play/pause/loop + audio, a draggable in/out timeline with a time ruler,
//! filmstrip and markers, keyboard shortcuts, and an "Export selection" that
//! hands the trim to `ord-export`.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use eframe::egui;
use ord_export::{Preset, Trim};

use crate::format::human_duration;
use crate::player::{Player, PreviewFrame};
use crate::timeline::{Timeline, View};

const FILMSTRIP_TILES: usize = 14;

/// What the editor wants the app to do after a frame.
pub enum EditorAction {
    None,
    Back,
    Export {
        preset: Preset,
        trim: Option<Trim>,
        mute: bool,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    In,
    Out,
    Playhead,
}

/// Editor state for a single clip.
pub struct EditorState {
    clip: PathBuf,
    label: String,
    player: Player,
    timeline: Timeline,
    zoom: f32,
    scroll: f32,
    markers: Vec<f64>,
    mute_export: bool,
    volume: f32,
    drag: Option<Drag>,
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
        let strip_rx = spawn_filmstrip(&clip, player.duration(), ctx.clone());
        Ok(Self {
            clip,
            label,
            player,
            timeline,
            zoom: 1.0,
            scroll: 0.0,
            markers: Vec::new(),
            mute_export: false,
            volume: 1.0,
            drag: None,
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

    /// Render the editor; returns the action the user took (if any).
    pub fn ui(&mut self, ctx: &egui::Context, wd: &crate::diag::Watchdog) -> EditorAction {
        // Pull in any decoded filmstrip tiles.
        while let Ok((i, img)) = self.strip_rx.try_recv() {
            if let Some(slot) = self.strip.get_mut(i) {
                *slot =
                    Some(ctx.load_texture(format!("strip-{i}"), img, egui::TextureOptions::LINEAR));
            }
        }

        // Player drives the playhead during playback; keep its range in sync.
        self.player
            .set_range(self.timeline.in_point(), self.timeline.out_point());
        if self.player.is_playing() {
            self.timeline.set_playhead(self.player.position());
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
                            "Space play · I/O set in-out · ←/→ seek · ,/. frame · M marker · scroll=zoom",
                        )
                        .color(ui.visuals().weak_text_color())
                        .size(12.0),
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
        let mut action = EditorAction::None;
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
        if mkey {
            self.markers.push(ph);
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F3)) {
            self.debug = !self.debug;
        }
        let _ = &mut action;
        action
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

    fn timeline_ui(&mut self, ui: &mut egui::Ui) {
        let accent = egui::Color32::from_rgb(124, 92, 255);
        let dur = self.player.duration().max(1e-6);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 84.0), egui::Sense::hover());

        // Zoom / scroll from the mouse wheel over the timeline.
        let hovered = ui.rect_contains_pointer(rect);
        if hovered {
            let scroll_y = ui.input(|i| i.raw_scroll_delta.y);
            if scroll_y != 0.0 {
                let zooming = ui.input(|i| i.modifiers.ctrl || i.modifiers.alt) || self.zoom > 1.0;
                if zooming && ui.input(|i| i.modifiers.ctrl || i.modifiers.alt) {
                    let f = if scroll_y > 0.0 { 1.1 } else { 1.0 / 1.1 };
                    self.zoom = (self.zoom * f).clamp(1.0, 60.0);
                } else if self.zoom > 1.0 {
                    self.scroll = (self.scroll - scroll_y * 0.01 / self.zoom).clamp(0.0, 1.0);
                }
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
        painter.rect_filled(track, 4.0, egui::Color32::from_rgb(30, 30, 38));

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

        // Ruler ticks.
        self.paint_ruler(&painter, ruler, track, &view);

        // Markers.
        for &m in &self.markers {
            let mx = x_of(m);
            if track.x_range().contains(mx) {
                painter.line_segment(
                    [egui::pos2(mx, track.top()), egui::pos2(mx, track.bottom())],
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 196, 0)),
                );
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

        // Playhead.
        if track.x_range().contains(ph_x) {
            painter.line_segment(
                [
                    egui::pos2(ph_x, rect.top()),
                    egui::pos2(ph_x, track.bottom()),
                ],
                egui::Stroke::new(2.0, egui::Color32::WHITE),
            );
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

        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                // Pick the nearest of in/out handle, else scrub the playhead.
                let din = (p.x - x_of(self.timeline.in_point())).abs();
                let dout = (p.x - x_of(self.timeline.out_point())).abs();
                self.drag = Some(if din <= 8.0 && din <= dout {
                    Drag::In
                } else if dout <= 8.0 {
                    Drag::Out
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
                let t = time_at(p.x);
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
                }
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }
        if resp.clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                self.seek_to(time_at(p.x));
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
        ui.horizontal(|ui| {
            let weak = ui.visuals().weak_text_color();
            ui.label(egui::RichText::new("In").color(weak));
            ui.monospace(human_duration(self.timeline.in_point()));
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Out").color(weak));
            ui.monospace(human_duration(self.timeline.out_point()));
            ui.add_space(6.0);
            ui.label(egui::RichText::new("Selection").color(weak));
            ui.monospace(human_duration(self.timeline.selection_duration()));
            ui.add_space(12.0);
            ui.checkbox(&mut self.mute_export, "Mute audio");

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button(egui::RichText::new("Export selection").strong(), |ui| {
                    let trim = self.current_trim();
                    let mute = self.mute_export;
                    let mib = ord_export::profile::ExportProfile::discord();
                    let est = est_discord_mib(self.timeline.selection_duration(), &mib);
                    if ui
                        .button(format!("Discord  (~{est:.1} MiB, 1080p)"))
                        .clicked()
                    {
                        *action = EditorAction::Export {
                            preset: Preset::Discord,
                            trim,
                            mute,
                        };
                        ui.close_menu();
                    }
                    if ui.button("High quality  (AV1, source res)").clicked() {
                        *action = EditorAction::Export {
                            preset: Preset::HighQuality,
                            trim,
                            mute,
                        };
                        ui.close_menu();
                    }
                    if ui.button("Source  (lossless remux)").clicked() {
                        *action = EditorAction::Export {
                            preset: Preset::Source,
                            trim,
                            mute,
                        };
                        ui.close_menu();
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

/// Rough finished-size estimate (MiB) for the Discord target over `dur`.
fn est_discord_mib(dur: f64, profile: &ord_export::profile::ExportProfile) -> f64 {
    if let ord_export::profile::RateControl::TargetSize { mib } = profile.rate_control {
        // Target size is duration-independent (the encoder hits the budget), but
        // very short clips can't exceed source; clamp to a sane floor.
        if dur <= 0.0 {
            0.0
        } else {
            mib.min(9.0)
        }
    } else {
        0.0
    }
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

/// Extract one MJPEG frame at `secs` via ffmpeg and decode it to an egui image.
fn extract_thumb(clip: &std::path::Path, secs: f64, width: u32) -> Option<egui::ColorImage> {
    let out = Command::new(ord_export::ffmpeg_bin())
        .args(["-v", "error", "-ss", &format!("{:.3}", secs.max(0.0)), "-i"])
        .arg(clip)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &format!("scale={width}:-2"),
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "-",
        ])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let img = image::load_from_memory(&out.stdout).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        img.as_raw(),
    ))
}
