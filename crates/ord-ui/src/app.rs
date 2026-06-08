//! egui clip-library window (behind the `gui` feature).
//!
//! Renders the [`library`](crate::library) model: a newest-first list of clips
//! with their labels. Designed to live in a compositor special workspace.

use std::path::PathBuf;

use eframe::egui;

use crate::library::{scan_dir, Clip};

/// The clip-library application state.
pub struct LibraryApp {
    clips_dir: PathBuf,
    clips: Vec<Clip>,
}

impl LibraryApp {
    /// Build the app, scanning `clips_dir` immediately.
    pub fn new(clips_dir: PathBuf) -> Self {
        let clips = scan_dir(&clips_dir);
        Self { clips_dir, clips }
    }

    fn refresh(&mut self) {
        self.clips = scan_dir(&self.clips_dir);
    }
}

impl eframe::App for LibraryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("open-recorder");
                if ui.button("Refresh").clicked() {
                    self.refresh();
                }
                ui.label(format!("{} clips", self.clips.len()));
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.clips.is_empty() {
                ui.label("No clips yet. Save one with `ord save --last 30`.");
                return;
            }
            egui::ScrollArea::vertical().show(ui, |ui| {
                for clip in &self.clips {
                    ui.horizontal(|ui| {
                        ui.label(clip.label());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Open").clicked() {
                                let _ = std::process::Command::new("mpv").arg(&clip.path).spawn();
                            }
                        });
                    });
                    ui.separator();
                }
            });
        });
    }
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
            .with_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "open-recorder",
        options,
        Box::new(move |_cc| Ok(Box::new(LibraryApp::new(clips_dir)))),
    )
}
