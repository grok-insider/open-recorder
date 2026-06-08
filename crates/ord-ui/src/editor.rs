//! Clip trim editor (behind the `gui` feature).
//!
//! Shows a scrubbable preview of one clip with draggable in/out handles, and
//! emits an [`EditorAction`] (Back, or Export the selection) for the app to act
//! on. Preview frames are decoded off the UI thread by a coalescing worker so
//! scrubbing stays responsive.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui;
use ord_export::{Preset, Trim};

use crate::format::human_duration;
use crate::preview;
use crate::timeline::Timeline;

const PREVIEW_W: u32 = 854; // extraction width; displayed scaled

/// What the editor wants the app to do after a frame.
pub enum EditorAction {
    None,
    Back,
    Export { preset: Preset, trim: Option<Trim> },
}

/// Editor state for a single clip.
pub struct EditorState {
    clip: PathBuf,
    label: String,
    timeline: Timeline,
    texture: Option<egui::TextureHandle>,
    req_tx: Sender<f64>,
    frame_rx: Receiver<egui::ColorImage>,
    last_req: f64,
}

impl EditorState {
    /// Open the editor for `clip`. `duration` is used if known; otherwise the
    /// clip is probed. Spawns the preview worker and requests the first frame.
    pub fn new(clip: PathBuf, label: String, duration: Option<f64>, ctx: &egui::Context) -> Self {
        let duration = duration
            .filter(|d| *d > 0.0)
            .or_else(|| {
                ord_export::probe::probe(&clip)
                    .ok()
                    .map(|i| i.duration_secs)
            })
            .unwrap_or(0.0);

        let (req_tx, req_rx) = channel::<f64>();
        let (frame_tx, frame_rx) = channel::<egui::ColorImage>();
        let worker_clip = clip.clone();
        let worker_ctx = ctx.clone();
        std::thread::spawn(move || {
            while let Ok(mut t) = req_rx.recv() {
                // Coalesce a burst of scrub requests down to the latest.
                while let Ok(newer) = req_rx.try_recv() {
                    t = newer;
                }
                if let Some(img) = preview::frame_at(&worker_clip, t, PREVIEW_W) {
                    if frame_tx.send(img).is_err() {
                        break;
                    }
                    worker_ctx.request_repaint();
                }
            }
        });

        let mut s = Self {
            clip,
            label,
            timeline: Timeline::new(duration),
            texture: None,
            req_tx,
            frame_rx,
            last_req: -1.0,
        };
        s.request_frame(0.0);
        s
    }

    fn request_frame(&mut self, t: f64) {
        if (t - self.last_req).abs() > 0.03 {
            let _ = self.req_tx.send(t);
            self.last_req = t;
        }
    }

    fn drain_frames(&mut self, ctx: &egui::Context) {
        let mut latest = None;
        while let Ok(img) = self.frame_rx.try_recv() {
            latest = Some(img);
        }
        if let Some(img) = latest {
            self.texture =
                Some(ctx.load_texture("editor-preview", img, egui::TextureOptions::LINEAR));
        }
    }

    /// Render the editor, returning the action the user took (if any).
    pub fn ui(&mut self, ctx: &egui::Context) -> EditorAction {
        self.drain_frames(ctx);
        let mut action = EditorAction::None;

        egui::TopBottomPanel::top("editor-top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("← Back").clicked() {
                    action = EditorAction::Back;
                }
                ui.separator();
                ui.label(egui::RichText::new(&self.label).strong().size(15.0));
            });
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("editor-bottom").show(ctx, |ui| {
            ui.add_space(8.0);
            self.timeline_ui(ui);
            ui.add_space(6.0);
            self.controls_ui(ui, &mut action);
            ui.add_space(8.0);
        });

        let panel_frame =
            egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(8.0));
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                self.preview_ui(ui);
            });

        action
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        match &self.texture {
            Some(tex) => {
                let [tw, th] = tex.size();
                let ar = (tw.max(1) as f32) / (th.max(1) as f32);
                let mut w = avail.x;
                let mut h = w / ar;
                if h > avail.y {
                    h = avail.y;
                    w = h * ar;
                }
                ui.vertical_centered(|ui| {
                    ui.add(
                        egui::Image::new(tex)
                            .fit_to_exact_size(egui::vec2(w, h))
                            .rounding(8.0),
                    );
                });
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("decoding preview…")
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            }
        }
    }

    fn timeline_ui(&mut self, ui: &mut egui::Ui) {
        let accent = egui::Color32::from_rgb(124, 92, 255);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 54.0), egui::Sense::hover());
        let track = rect.shrink2(egui::vec2(14.0, 18.0));
        let painter = ui.painter_at(rect);

        let frac = |t: f64| track.left() + self.timeline.fraction(t) * track.width();
        let x_in = frac(self.timeline.in_point());
        let x_out = frac(self.timeline.out_point());
        let x_ph = frac(self.timeline.playhead());

        // Track + selection.
        painter.rect_filled(track, 4.0, egui::Color32::from_rgb(38, 38, 48));
        let sel = egui::Rect::from_min_max(
            egui::pos2(x_in, track.top()),
            egui::pos2(x_out, track.bottom()),
        );
        painter.rect_filled(sel, 4.0, accent.linear_multiply(0.35));

        // Trimmed-away regions dimmed.
        let dim = egui::Color32::from_black_alpha(120);
        painter.rect_filled(
            egui::Rect::from_min_max(track.left_top(), egui::pos2(x_in, track.bottom())),
            0.0,
            dim,
        );
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x_out, track.top()), track.right_bottom()),
            0.0,
            dim,
        );

        // Playhead.
        painter.line_segment(
            [
                egui::pos2(x_ph, rect.top()),
                egui::pos2(x_ph, rect.bottom()),
            ],
            egui::Stroke::new(2.0, egui::Color32::WHITE),
        );

        // Handles.
        let handle = |x: f32| {
            egui::Rect::from_center_size(
                egui::pos2(x, rect.center().y),
                egui::vec2(10.0, rect.height()),
            )
        };
        let in_h = handle(x_in);
        let out_h = handle(x_out);
        painter.rect_filled(in_h, 3.0, accent);
        painter.rect_filled(out_h, 3.0, accent);

        // Capture only plain values so the closure doesn't borrow `self` (which
        // would clash with the mutable `set_*` / `request_frame` calls below).
        let track_left = track.left();
        let track_w = track.width().max(1.0);
        let duration = self.timeline.duration();
        let time_at_x =
            move |x: f32| (((x - track_left) / track_w).clamp(0.0, 1.0) as f64) * duration;

        // Interactions: handles first (on top), then the bar for the playhead.
        let r_in = ui.interact(in_h.expand(8.0), ui.id().with("tl_in"), egui::Sense::drag());
        if r_in.dragged() {
            if let Some(p) = r_in.interact_pointer_pos() {
                self.timeline.set_in(time_at_x(p.x));
                let t = self.timeline.in_point();
                self.request_frame(t);
            }
        }
        let r_out = ui.interact(
            out_h.expand(8.0),
            ui.id().with("tl_out"),
            egui::Sense::drag(),
        );
        if r_out.dragged() {
            if let Some(p) = r_out.interact_pointer_pos() {
                self.timeline.set_out(time_at_x(p.x));
                let t = self.timeline.out_point();
                self.request_frame(t);
            }
        }
        let r_bar = ui.interact(track, ui.id().with("tl_bar"), egui::Sense::click_and_drag());
        if (r_bar.clicked() || r_bar.dragged()) && !r_in.dragged() && !r_out.dragged() {
            if let Some(p) = r_bar.interact_pointer_pos() {
                self.timeline.set_playhead(time_at_x(p.x));
                let t = self.timeline.playhead();
                self.request_frame(t);
            }
        }
    }

    fn controls_ui(&mut self, ui: &mut egui::Ui, action: &mut EditorAction) {
        ui.horizontal(|ui| {
            let weak = ui.visuals().weak_text_color();
            ui.label(egui::RichText::new("In").color(weak));
            ui.monospace(human_duration(self.timeline.in_point()));
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Out").color(weak));
            ui.monospace(human_duration(self.timeline.out_point()));
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Selection").color(weak));
            ui.monospace(human_duration(self.timeline.selection_duration()));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("Export selection", |ui| {
                    let trim = self.current_trim();
                    if ui.button("Discord  (fits 10 MB, 1080p)").clicked() {
                        *action = EditorAction::Export {
                            preset: Preset::Discord,
                            trim,
                        };
                        ui.close_menu();
                    }
                    if ui.button("High quality  (AV1, source res)").clicked() {
                        *action = EditorAction::Export {
                            preset: Preset::HighQuality,
                            trim,
                        };
                        ui.close_menu();
                    }
                    if ui.button("Source  (lossless remux)").clicked() {
                        *action = EditorAction::Export {
                            preset: Preset::Source,
                            trim,
                        };
                        ui.close_menu();
                    }
                });
            });
        });
    }

    /// The trim window to export, or `None` if the whole clip is selected.
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

    pub fn clip(&self) -> &PathBuf {
        &self.clip
    }
}
