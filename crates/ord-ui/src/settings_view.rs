//! The settings page (gui-only): renders [`SettingsModel`] as a quiet,
//! single-column form in the design system, and emits the user's intent
//! (apply/back) for the app to execute over the daemon socket.
//!
//! The page is data-first: every control binds straight to `model.draft`, the
//! footer derives entirely from the model (dirty/tier/problems), and a small
//! gold dot marks fields that override the base (HM/file) config.

use eframe::egui;
use ord_common::config::{CaptureCodec, Container, ExportCodec, Quality};
use ord_common::Config;

use crate::settings::{ApplyTier, SettingsModel};
use crate::theme;

/// What the settings page wants the app to do this frame.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsAction {
    None,
    /// Close the page.
    Back,
    /// Send this config to the daemon (`SetConfig`).
    Apply(Box<Config>),
}

/// Settings page state: the model arrives asynchronously (`GetConfig` reply).
pub struct SettingsView {
    pub model: Option<SettingsModel>,
    /// An apply is in flight; disable the footer until the daemon replies.
    pub busy: bool,
    /// Last daemon error for this page, shown inline.
    pub error: Option<String>,
}

impl SettingsView {
    pub fn new() -> Self {
        Self {
            model: None,
            busy: false,
            error: None,
        }
    }

    /// Feed a `Event::Config` reply (initial load or post-apply confirmation).
    pub fn on_config(&mut self, effective: Config, base: Config) {
        self.busy = false;
        self.error = None;
        match self.model.as_mut() {
            Some(m) => m.applied(effective, base),
            None => self.model = Some(SettingsModel::new(effective, base)),
        }
    }

    /// Feed a daemon error that arrived while this page was waiting.
    pub fn on_error(&mut self, message: String) {
        self.busy = false;
        self.error = Some(message);
    }

    /// Render the page; returns the action for the app to perform.
    pub fn ui(&mut self, ctx: &egui::Context) -> SettingsAction {
        let mut action = SettingsAction::None;

        egui::TopBottomPanel::top("settings-top")
            .frame(theme::chrome())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("←  Library").clicked()
                        || ui.input(|i| i.key_pressed(egui::Key::Escape))
                    {
                        action = SettingsAction::Back;
                    }
                    ui.add_space(theme::SP_2);
                    ui.label(
                        egui::RichText::new("Settings")
                            .size(theme::TEXT_TITLE)
                            .strong()
                            .color(theme::INK),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new("base config + runtime overrides")
                                .size(theme::TEXT_MICRO)
                                .color(theme::INK_3),
                        );
                    });
                });
            });

        if let Some(act) = self.footer(ctx) {
            action = act;
        }

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(0.0)),
            )
            .show(ctx, |ui| {
                let Some(model) = self.model.as_mut() else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(120.0);
                        ui.label(
                            egui::RichText::new(match &self.error {
                                Some(e) => format!("Cannot load settings: {e}"),
                                None => "Loading settings from the daemon…".to_string(),
                            })
                            .color(theme::INK_2),
                        );
                    });
                    return;
                };

                egui::ScrollArea::vertical().show(ui, |ui| {
                    // One centered column, ~560px: a form wants a measure, not
                    // the full window width.
                    let col = 560.0_f32.min(ui.available_width() - 2.0 * theme::SP_4);
                    let pad = ((ui.available_width() - col) / 2.0).max(theme::SP_4);
                    ui.horizontal(|ui| {
                        ui.add_space(pad);
                        ui.vertical(|ui| {
                            ui.set_width(col);
                            ui.add_space(theme::SP_3);
                            form(ui, model);
                            ui.add_space(96.0); // room above the sticky footer
                        });
                    });
                });
            });

        action
    }

    /// Sticky footer: problems, override summary, Revert / Reset / Apply.
    fn footer(&mut self, ctx: &egui::Context) -> Option<SettingsAction> {
        let mut action = None;
        egui::TopBottomPanel::bottom("settings-foot")
            .frame(theme::chrome())
            .show(ctx, |ui| {
                let Some(model) = self.model.as_mut() else {
                    return;
                };
                let problems = model.problems();
                let tier = model.apply_tier();

                ui.horizontal(|ui| {
                    if let Some(err) = &self.error {
                        ui.label(
                            egui::RichText::new(err)
                                .size(theme::TEXT_LABEL)
                                .color(theme::SHU),
                        );
                    } else if let Some(p) = problems.first() {
                        ui.label(
                            egui::RichText::new(p)
                                .size(theme::TEXT_LABEL)
                                .color(theme::KIN),
                        );
                    } else {
                        let n = model.overridden().len();
                        let text = match n {
                            0 => "no runtime overrides — base config as-is".to_string(),
                            1 => "1 field overrides the base config".to_string(),
                            n => format!("{n} fields override the base config"),
                        };
                        theme::status_dot(
                            ui,
                            if n == 0 { theme::INK_3 } else { theme::KIN },
                            &text,
                            "Overrides live in $XDG_STATE_HOME/open-recorder/overrides.toml; \
                             the base config file is never modified.",
                        );
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let dirty = model.is_dirty();
                        let can_apply = dirty && problems.is_empty() && !self.busy;
                        let label = match tier {
                            ApplyTier::CaptureRestart => "Apply  (restarts capture)",
                            _ => "Apply",
                        };
                        let apply = ui.add_enabled_ui(can_apply, |ui| {
                            if tier == ApplyTier::CaptureRestart {
                                theme::danger_button(ui, label)
                            } else {
                                theme::primary_button(ui, label)
                            }
                        });
                        if apply.inner.clicked() {
                            self.busy = true;
                            action = Some(SettingsAction::Apply(Box::new(model.draft.clone())));
                        }
                        if self.busy {
                            ui.spinner();
                        }
                        ui.add_enabled_ui(dirty && !self.busy, |ui| {
                            if ui.button("Revert").clicked() {
                                model.revert();
                            }
                        });
                        ui.add_enabled_ui(!self.busy, |ui| {
                            if ui
                                .button("Reset to base")
                                .on_hover_text(
                                    "Discard every runtime override and go back to the values \
                                     in config.toml (applies on Apply).",
                                )
                                .clicked()
                            {
                                model.reset_to_base();
                            }
                        });
                    });
                });
            });
        action
    }
}

impl Default for SettingsView {
    fn default() -> Self {
        Self::new()
    }
}

/// One labeled form row: gold override dot, quiet label left, control right.
fn row(
    ui: &mut egui::Ui,
    model: &SettingsModel,
    path: &str,
    label: &str,
    hover: &str,
    control: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        let overridden = model.is_overridden(path);
        let (rect, _) = ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
        if overridden {
            ui.painter().circle_filled(rect.center(), 2.2, theme::KIN);
        }
        let resp = ui.add_sized(
            [190.0, 18.0],
            egui::Label::new(
                egui::RichText::new(label)
                    .size(theme::TEXT_BODY)
                    .color(theme::INK_2),
            )
            .halign(egui::Align::LEFT),
        );
        if !hover.is_empty() {
            resp.on_hover_text(hover);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
    ui.add_space(2.0);
}

/// Checkbox + value editor for an `Option<u32>` field.
fn optional_u32(
    ui: &mut egui::Ui,
    value: &mut Option<u32>,
    default_when_on: u32,
    range: std::ops::RangeInclusive<u32>,
    suffix: &str,
) {
    let mut on = value.is_some();
    if ui.checkbox(&mut on, "").changed() {
        *value = on.then_some(default_when_on);
    }
    if let Some(v) = value.as_mut() {
        ui.add(egui::DragValue::new(v).range(range).suffix(suffix));
    } else {
        ui.label(
            egui::RichText::new("off")
                .size(theme::TEXT_LABEL)
                .color(theme::INK_3),
        );
    }
}

fn form(ui: &mut egui::Ui, model: &mut SettingsModel) {
    // Read-only snapshot for override dots while the closures borrow draft.
    let snapshot = model.clone();

    theme::section(ui, "Capture");
    row(ui, &snapshot, "capture.fps", "Frame rate", "", |ui| {
        ui.add(
            egui::DragValue::new(&mut model.draft.capture.fps)
                .range(1..=240)
                .suffix(" fps"),
        );
    });
    row(
        ui,
        &snapshot,
        "capture.buffer_seconds",
        "Replay buffer",
        "How many seconds are held in RAM for `ord save`. Applies live.",
        |ui| {
            ui.add(
                egui::DragValue::new(&mut model.draft.capture.buffer_seconds)
                    .range(5..=3600)
                    .suffix(" s"),
            );
        },
    );
    row(
        ui,
        &snapshot,
        "capture.quality",
        "Quality preset",
        "",
        |ui| {
            egui::ComboBox::from_id_salt("quality")
                .selected_text(format!("{:?}", model.draft.capture.quality))
                .show_ui(ui, |ui| {
                    for q in [Quality::Low, Quality::Medium, Quality::High, Quality::Ultra] {
                        ui.selectable_value(&mut model.draft.capture.quality, q, format!("{q:?}"));
                    }
                });
        },
    );
    row(
        ui,
        &snapshot,
        "capture.codec",
        "Codec",
        "H.264 is the most compatible; AV1 compresses best (RTX 40/50).",
        |ui| {
            egui::ComboBox::from_id_salt("codec")
                .selected_text(codec_label(model.draft.capture.codec))
                .show_ui(ui, |ui| {
                    for c in [CaptureCodec::H264, CaptureCodec::Hevc, CaptureCodec::Av1] {
                        ui.selectable_value(&mut model.draft.capture.codec, c, codec_label(c));
                    }
                });
        },
    );
    row(
        ui,
        &snapshot,
        "capture.bitrate_kbps",
        "Constant bitrate",
        "Predictable RAM use in high-motion scenes; off = constant quality.",
        |ui| {
            optional_u32(
                ui,
                &mut model.draft.capture.bitrate_kbps,
                12_000,
                1_000..=200_000,
                " kbps",
            );
        },
    );
    row(
        ui,
        &snapshot,
        "capture.clear_on_save",
        "Clear buffer after save",
        "Consecutive saves never overlap; pre-save footage is dropped.",
        |ui| {
            ui.checkbox(&mut model.draft.capture.clear_on_save, "");
        },
    );

    theme::section(ui, "Audio");
    row(ui, &snapshot, "audio.desktop", "Desktop audio", "", |ui| {
        ui.checkbox(&mut model.draft.audio.desktop, "");
    });
    row(
        ui,
        &snapshot,
        "audio.mic",
        "Microphone",
        "Mixed with desktop audio into one track, on the same clock.",
        |ui| {
            ui.checkbox(&mut model.draft.audio.mic, "");
        },
    );

    theme::section(ui, "Storage");
    row(
        ui,
        &snapshot,
        "storage.clips_dir",
        "Clips folder",
        "Empty = ~/Videos/open-recorder. `~` expands.",
        |ui| {
            let mut text = model.draft.storage.clips_dir.clone().unwrap_or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .hint_text("~/Videos/open-recorder")
                    .desired_width(240.0),
            );
            if resp.changed() {
                model.draft.storage.clips_dir =
                    (!text.trim().is_empty()).then(|| text.trim().to_string());
            }
        },
    );
    row(
        ui,
        &snapshot,
        "storage.template",
        "Filename template",
        "Tokens: {game} {rec} {epoch} {date} {time}. `/` makes subfolders — \
         e.g. {date}/{game}-{epoch} for date folders.",
        |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut model.draft.storage.template).desired_width(240.0),
            );
        },
    );
    row(
        ui,
        &snapshot,
        "storage.max_gib",
        "Auto-prune over",
        "Oldest clips are deleted past this size. Exports are never touched.",
        |ui| {
            optional_u32(ui, &mut model.draft.storage.max_gib, 25, 1..=4096, " GiB");
        },
    );
    row(
        ui,
        &snapshot,
        "storage.max_age_days",
        "Auto-prune older than",
        "",
        |ui| {
            optional_u32(
                ui,
                &mut model.draft.storage.max_age_days,
                90,
                1..=3650,
                " days",
            );
        },
    );

    theme::section(ui, "Markers");
    row(
        ui,
        &snapshot,
        "markers.auto_save_seconds",
        "Auto-save on mark",
        "`ord mark` also saves the last N seconds — bookmark and clip in one key.",
        |ui| {
            optional_u32(
                ui,
                &mut model.draft.markers.auto_save_seconds,
                30,
                1..=600,
                " s",
            );
        },
    );

    theme::section(ui, "Hooks");
    row(
        ui,
        &snapshot,
        "hooks.on_clip_saved",
        "After every save, run",
        "Receives the clip path as $1. Asynchronous; failures only log.",
        |ui| {
            let mut text = model.draft.hooks.on_clip_saved.clone().unwrap_or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .hint_text("~/bin/clip-hook")
                    .desired_width(240.0),
            );
            if resp.changed() {
                model.draft.hooks.on_clip_saved =
                    (!text.trim().is_empty()).then(|| text.trim().to_string());
            }
        },
    );

    theme::section(ui, "Export defaults");
    row(ui, &snapshot, "export.codec", "Codec", "", |ui| {
        egui::ComboBox::from_id_salt("export-codec")
            .selected_text(format!("{:?}", model.draft.export.codec))
            .show_ui(ui, |ui| {
                for c in [ExportCodec::Av1, ExportCodec::Hevc, ExportCodec::H264] {
                    ui.selectable_value(&mut model.draft.export.codec, c, format!("{c:?}"));
                }
            });
    });
    row(ui, &snapshot, "export.container", "Container", "", |ui| {
        egui::ComboBox::from_id_salt("export-container")
            .selected_text(format!("{:?}", model.draft.export.container))
            .show_ui(ui, |ui| {
                for c in [Container::Mp4, Container::Mkv] {
                    ui.selectable_value(&mut model.draft.export.container, c, format!("{c:?}"));
                }
            });
    });

    theme::section(ui, "Keybinds");
    ui.label(
        egui::RichText::new("Hotkeys are compositor keybinds calling `ord`. For Hyprland:")
            .size(theme::TEXT_LABEL)
            .color(theme::INK_2),
    );
    ui.add_space(theme::SP_1);
    let snippet = "bind = ALT, R, exec, ord save --last 30\n\
                   bind = ALT, M, exec, ord mark\n\
                   bind = ALT SHIFT, R, exec, ord record";
    theme::card().show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.monospace(egui::RichText::new(snippet).size(theme::TEXT_LABEL));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if ui.button("Copy").clicked() {
                    ui.output_mut(|o| o.copied_text = snippet.to_string());
                }
            });
        });
    });
}

fn codec_label(c: CaptureCodec) -> &'static str {
    match c {
        CaptureCodec::H264 => "H.264 (compatible)",
        CaptureCodec::Hevc => "HEVC",
        CaptureCodec::Av1 => "AV1 (best compression)",
    }
}
