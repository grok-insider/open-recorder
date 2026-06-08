//! egui clip-library window (behind the `gui` feature).
//!
//! Renders the [`library`](crate::library) model as a dark, card-based grid:
//! each clip shows an ffmpeg thumbnail, its label and metadata
//! (duration · resolution · size · relative time), and actions — Open, Export
//! (via [`ord_export`] presets), Reveal, and Delete. Metadata and thumbnails are
//! loaded off the UI thread so the window stays responsive.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use ord_export::{export, ExportSummary, Preset, Trim};

use crate::editor::{EditorAction, EditorState};
use crate::format::{human_duration, human_size, relative_time, resolution};
use crate::library::{scan_dir, Clip};
use crate::meta::{self, ClipMeta};

const CARD_INNER_W: f32 = 300.0;
const THUMB_W: f32 = 300.0;
const THUMB_H: f32 = 169.0; // 16:9

/// Message from the background loader thread.
enum Loaded {
    Meta {
        path: PathBuf,
        meta: Option<ClipMeta>,
    },
    Thumb {
        path: PathBuf,
        image: egui::ColorImage,
    },
}

/// Result of a background export.
struct ExportMsg {
    clip: PathBuf,
    result: Result<ExportSummary, String>,
}

/// Per-clip async-loaded state.
#[derive(Default)]
struct ClipState {
    meta: Option<ClipMeta>,
    meta_loaded: bool,
    texture: Option<egui::TextureHandle>,
    thumb_tried: bool,
}

/// The clip-library application state.
pub struct LibraryApp {
    clips_dir: PathBuf,
    clips: Vec<Clip>,
    states: HashMap<PathBuf, ClipState>,
    loader_rx: Receiver<Loaded>,
    loader_tx: Sender<Loaded>,
    export_rx: Receiver<ExportMsg>,
    export_tx: Sender<ExportMsg>,
    exporting: HashSet<PathBuf>,
    confirm_delete: Option<PathBuf>,
    status: Option<(String, Instant)>,
    styled: bool,
    loading: bool,
    /// When `Some`, the trim editor is shown instead of the library grid.
    editor: Option<EditorState>,
}

impl LibraryApp {
    /// Build the app, scanning `clips_dir` immediately.
    pub fn new(clips_dir: PathBuf) -> Self {
        let clips = scan_dir(&clips_dir);
        let (loader_tx, loader_rx) = channel();
        let (export_tx, export_rx) = channel();
        Self {
            clips_dir,
            clips,
            states: HashMap::new(),
            loader_rx,
            loader_tx,
            export_rx,
            export_tx,
            exporting: HashSet::new(),
            confirm_delete: None,
            status: None,
            styled: false,
            loading: false,
            editor: None,
        }
    }

    fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    /// (Re)scan the directory and kick off background loading.
    fn refresh(&mut self, ctx: &egui::Context) {
        self.clips = scan_dir(&self.clips_dir);
        self.states.clear();
        self.confirm_delete = None;
        self.start_loading(ctx);
    }

    /// Spawn the loader thread for the current clip set.
    fn start_loading(&mut self, ctx: &egui::Context) {
        let clips = self.clips.clone();
        let tx = self.loader_tx.clone();
        let ctx = ctx.clone();
        self.loading = true;
        std::thread::spawn(move || {
            for clip in clips {
                let meta = meta::load_meta(&clip.path);
                let _ = tx.send(Loaded::Meta {
                    path: clip.path.clone(),
                    meta,
                });
                ctx.request_repaint();
                if let Some(thumb) = meta::ensure_thumbnail(&clip.path) {
                    if let Some(image) = decode_image(&thumb) {
                        let _ = tx.send(Loaded::Thumb {
                            path: clip.path.clone(),
                            image,
                        });
                        ctx.request_repaint();
                    }
                }
            }
        });
    }

    fn drain_channels(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.loader_rx.try_recv() {
            match msg {
                Loaded::Meta { path, meta } => {
                    let st = self.states.entry(path).or_default();
                    st.meta = meta;
                    st.meta_loaded = true;
                }
                Loaded::Thumb { path, image } => {
                    let name = format!("thumb:{}", path.display());
                    let tex = ctx.load_texture(name, image, egui::TextureOptions::LINEAR);
                    let st = self.states.entry(path).or_default();
                    st.texture = Some(tex);
                    st.thumb_tried = true;
                }
            }
        }
        while let Ok(msg) = self.export_rx.try_recv() {
            self.exporting.remove(&msg.clip);
            match msg.result {
                Ok(s) => {
                    let name = s
                        .output
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    self.set_status(format!("Exported → {name}  ({})", human_size(s.size_bytes)));
                }
                Err(e) => self.set_status(format!("Export failed: {e}")),
            }
        }
    }

    fn start_export(&mut self, clip: &Clip, preset: Preset, ctx: &egui::Context) {
        self.run_export(
            &clip.path,
            &clip.stem,
            clip.label(),
            preset,
            None,
            false,
            ctx,
        );
    }

    /// Export `input` with `preset`, optionally trimmed/muted. Runs off-thread
    /// and reports via the export channel; ignores a duplicate in-flight export.
    #[allow(clippy::too_many_arguments)]
    fn run_export(
        &mut self,
        input: &Path,
        stem: &str,
        label: &str,
        preset: Preset,
        trim: Option<Trim>,
        mute: bool,
        ctx: &egui::Context,
    ) {
        if self.exporting.contains(input) {
            return;
        }
        let mut profile = preset.profile();
        profile.mute = mute;
        let ext = profile.container.extension();
        let preset_name = preset_label(preset);
        let suffix = if trim.is_some() { "-trim" } else { "" };
        let out =
            meta::exports_dir(&self.clips_dir).join(format!("{stem}-{preset_name}{suffix}.{ext}"));
        let input = input.to_path_buf();
        let tx = self.export_tx.clone();
        let ctx = ctx.clone();
        self.exporting.insert(input.clone());
        self.set_status(format!("Exporting {label} as {preset_name}…"));
        std::thread::spawn(move || {
            let result = export(&input, &out, &profile, trim).map_err(|e| e.to_string());
            let _ = tx.send(ExportMsg {
                clip: input,
                result,
            });
            ctx.request_repaint();
        });
    }

    fn delete_clip(&mut self, path: &Path, ctx: &egui::Context) {
        match std::fs::remove_file(path) {
            Ok(()) => {
                self.set_status("Clip deleted");
                self.refresh(ctx);
            }
            Err(e) => self.set_status(format!("Delete failed: {e}")),
        }
    }
}

fn preset_label(p: Preset) -> &'static str {
    match p {
        Preset::HighQuality => "high",
        Preset::Discord => "discord",
        Preset::Source => "source",
    }
}

fn decode_image(path: &Path) -> Option<egui::ColorImage> {
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        img.as_raw(),
    ))
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Dark theme with a violet accent, generous spacing and rounded widgets.
fn apply_theme(ctx: &egui::Context) {
    use egui::Color32;
    let accent = Color32::from_rgb(124, 92, 255);
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(16, 16, 20);
    v.window_fill = Color32::from_rgb(22, 22, 28);
    v.extreme_bg_color = Color32::from_rgb(12, 12, 15);
    v.override_text_color = Some(Color32::from_rgb(228, 228, 235));
    v.hyperlink_color = accent;
    v.selection.bg_fill = accent.linear_multiply(0.5);
    v.widgets.hovered.bg_fill = Color32::from_rgb(44, 44, 56);
    v.widgets.active.bg_fill = accent.linear_multiply(0.7);
    v.widgets.inactive.bg_fill = Color32::from_rgb(34, 34, 42);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(30, 30, 38);
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    ctx.set_style(style);
}

impl eframe::App for LibraryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.styled {
            apply_theme(ctx);
            self.styled = true;
        }
        self.drain_channels(ctx);

        // Trim editor takes over the whole window when open.
        if self.editor.is_some() {
            let action = self.editor.as_mut().unwrap().ui(ctx);
            match action {
                EditorAction::None => {}
                EditorAction::Back => self.editor = None,
                EditorAction::Export { preset, trim, mute } => {
                    let clip = self.editor.as_ref().unwrap().clip().clone();
                    let stem = clip
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "clip".to_string());
                    self.run_export(&clip, &stem, &stem, preset, trim, mute, ctx);
                    self.editor = None;
                }
            }
            return;
        }

        if !self.loading {
            self.start_loading(ctx);
        }

        let now = now_epoch();
        let total_size: u64 = self
            .states
            .values()
            .filter_map(|s| s.meta.as_ref().map(|m| m.size_bytes))
            .sum();

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("open-recorder");
                ui.label(
                    egui::RichText::new("clips")
                        .color(ui.visuals().weak_text_color())
                        .size(16.0),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh").clicked() {
                        self.refresh(ctx);
                    }
                    let summary = if total_size > 0 {
                        format!("{} clips · {}", self.clips.len(), human_size(total_size))
                    } else {
                        format!("{} clips", self.clips.len())
                    };
                    ui.label(egui::RichText::new(summary).color(ui.visuals().weak_text_color()));
                });
            });
            ui.add_space(6.0);
        });

        if let Some((msg, at)) = self.status.clone() {
            if at.elapsed() < Duration::from_secs(6) {
                egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("•").color(egui::Color32::from_rgb(124, 92, 255)),
                        );
                        ui.label(msg);
                    });
                    ui.add_space(4.0);
                });
                ctx.request_repaint_after(Duration::from_millis(500));
            } else {
                self.status = None;
            }
        }

        let panel_frame =
            egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(16.0));
        egui::CentralPanel::default()
            .frame(panel_frame)
            .show(ctx, |ui| {
                if self.clips.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(80.0);
                        ui.label(egui::RichText::new("No clips yet").size(20.0).strong());
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(
                                "Press ALT+R in a game to save the last 30 seconds.",
                            )
                            .color(ui.visuals().weak_text_color()),
                        );
                    });
                    return;
                }

                // Snapshot the clip list so we can mutate self inside the closures.
                let clips = self.clips.clone();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                        for clip in &clips {
                            self.card(ui, clip, now, ctx);
                        }
                    });
                    ui.add_space(8.0);
                });
            });
    }
}

impl LibraryApp {
    fn card(&mut self, ui: &mut egui::Ui, clip: &Clip, now: u64, ctx: &egui::Context) {
        let border = egui::Color32::from_rgb(48, 48, 60);
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(26, 26, 33))
            .stroke(egui::Stroke::new(1.0, border))
            .rounding(12.0)
            .inner_margin(egui::Margin::same(10.0))
            .show(ui, |ui| {
                ui.set_width(CARD_INNER_W);
                ui.vertical(|ui| {
                    if self.thumbnail(ui, clip) {
                        self.open_editor(clip, ctx);
                    }
                    ui.add_space(8.0);

                    ui.label(egui::RichText::new(clip.label()).strong().size(15.0));

                    // Metadata line.
                    let st = self.states.get(&clip.path);
                    let meta_line = match st.and_then(|s| s.meta.as_ref()) {
                        Some(m) => format!(
                            "{} · {} · {}",
                            human_duration(m.duration_secs),
                            resolution(m.width, m.height),
                            human_size(m.size_bytes),
                        ),
                        None if st.map(|s| s.meta_loaded).unwrap_or(false) => "—".to_string(),
                        None => "loading".to_string(),
                    };
                    ui.label(
                        egui::RichText::new(meta_line)
                            .color(ui.visuals().weak_text_color())
                            .size(12.5),
                    );
                    if let Some(epoch) = clip.epoch {
                        ui.label(
                            egui::RichText::new(relative_time(epoch, now))
                                .color(ui.visuals().weak_text_color())
                                .size(12.5),
                        );
                    }

                    ui.add_space(8.0);
                    self.actions(ui, clip, ctx);
                });
            });
    }

    /// Render the thumbnail; returns true if it was clicked (opens the editor).
    fn thumbnail(&self, ui: &mut egui::Ui, clip: &Clip) -> bool {
        let size = egui::vec2(THUMB_W, THUMB_H);
        match self.states.get(&clip.path).and_then(|s| s.texture.as_ref()) {
            Some(tex) => ui
                .add(
                    egui::Image::new(tex)
                        .fit_to_exact_size(size)
                        .rounding(8.0)
                        .sense(egui::Sense::click()),
                )
                .on_hover_text("Edit / trim")
                .clicked(),
            None => {
                let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                ui.painter()
                    .rect_filled(rect, 8.0, egui::Color32::from_rgb(18, 18, 23));
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "▶",
                    egui::FontId::proportional(28.0),
                    egui::Color32::from_rgb(70, 70, 86),
                );
                resp.on_hover_text("Edit / trim").clicked()
            }
        }
    }

    fn open_editor(&mut self, clip: &Clip, ctx: &egui::Context) {
        match EditorState::new(clip.path.clone(), clip.label().to_string(), ctx) {
            Ok(ed) => self.editor = Some(ed),
            Err(e) => self.set_status(format!("Can't open editor: {e}")),
        }
    }

    fn actions(&mut self, ui: &mut egui::Ui, clip: &Clip, ctx: &egui::Context) {
        // Row 1: primary actions.
        ui.horizontal(|ui| {
            if ui.button("▶  Open").clicked() {
                open_clip(&clip.path);
            }
            if ui.button("Edit").clicked() {
                self.open_editor(clip, ctx);
            }
            let exporting = self.exporting.contains(&clip.path);
            ui.add_enabled_ui(!exporting, |ui| {
                let label = if exporting { "Exporting" } else { "Export" };
                ui.menu_button(label, |ui| {
                    if ui.button("Discord  (fits 10 MB, 1080p)").clicked() {
                        self.start_export(clip, Preset::Discord, ctx);
                        ui.close_menu();
                    }
                    if ui.button("High quality  (AV1, source res)").clicked() {
                        self.start_export(clip, Preset::HighQuality, ctx);
                        ui.close_menu();
                    }
                    if ui.button("Source  (lossless remux)").clicked() {
                        self.start_export(clip, Preset::Source, ctx);
                        ui.close_menu();
                    }
                });
            });
        });

        ui.add_space(4.0);

        // Row 2: secondary actions.
        ui.horizontal(|ui| {
            if ui
                .button("Reveal")
                .on_hover_text("Show in file manager")
                .clicked()
            {
                reveal(&clip.path);
            }
            let confirming = self.confirm_delete.as_deref() == Some(clip.path.as_path());
            if confirming {
                let danger = egui::Color32::from_rgb(255, 110, 110);
                if ui
                    .button(egui::RichText::new("Confirm delete").color(danger))
                    .clicked()
                {
                    let path = clip.path.clone();
                    self.confirm_delete = None;
                    self.delete_clip(&path, ctx);
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_delete = None;
                }
            } else if ui.button("Delete").clicked() {
                self.confirm_delete = Some(clip.path.clone());
            }
        });
    }
}

fn open_clip(path: &Path) {
    if std::process::Command::new("mpv").arg(path).spawn().is_err() {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn reveal(path: &Path) {
    let dir = path.parent().unwrap_or(path);
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

/// Launch the library window.
pub fn run(clips_dir: PathBuf) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        // Set the Wayland app_id (X11 WM_CLASS) so compositors can match the
        // window — e.g. a Hyprland rule that moves it to a `special:clips`
        // workspace. Without this the class is empty and rules never match.
        viewport: eframe::egui::ViewportBuilder::default()
            .with_app_id("open-recorder")
            .with_title("open-recorder")
            .with_inner_size([920.0, 640.0])
            .with_min_inner_size([420.0, 320.0]),
        ..Default::default()
    };
    eframe::run_native(
        "open-recorder",
        options,
        Box::new(move |_cc| Ok(Box::new(LibraryApp::new(clips_dir)))),
    )
}
