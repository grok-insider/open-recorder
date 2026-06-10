//! The settings page (gui-only): renders [`SettingsModel`] as a quiet,
//! single-column form in the design system, and emits the user's intent
//! (apply/back) for the app to execute over the daemon socket.
//!
//! The page is data-first: every control binds straight to `model.draft`, the
//! footer derives entirely from the model (dirty/tier/problems), and a small
//! gold dot marks fields that override the base (HM/file) config. Every row
//! carries a one-line caption explaining the real impact of the choice, and
//! numbers are classic spinners (type, arrow keys, or the ▴/▾ buttons).

use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use ord_common::config::{CaptureCodec, Container, ExportCodec, Quality};
use ord_common::Config;

use crate::settings::{ApplyTier, SettingsModel};
use crate::theme;

/// Fixed label column so every control starts on the same vertical line.
const LABEL_W: f32 = 200.0;
/// Indent that puts captions flush under their label (dot + spacing).
const CAPTION_INDENT: f32 = 14.0;

/// What the settings page wants the app to do this frame.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsAction {
    None,
    /// Close the page.
    Back,
    /// Send this config to the daemon (`SetConfig`).
    Apply(Box<Config>),
}

/// Which draft field an open file/folder dialog will fill.
#[derive(Debug, Clone, Copy, PartialEq)]
enum BrowseTarget {
    ClipsDir,
    Hook,
}

/// Result of a (threaded) external picker dialog.
enum BrowseMsg {
    Picked(String),
    Cancelled,
    /// No dialog tool found on the system.
    Unavailable,
}

/// Settings page state: the model arrives asynchronously (`GetConfig` reply).
pub struct SettingsView {
    pub model: Option<SettingsModel>,
    /// An apply is in flight; disable the footer until the daemon replies.
    pub busy: bool,
    /// Last daemon error for this page, shown inline.
    pub error: Option<String>,
    /// An external file/folder dialog in flight (its result lands here).
    browse: Option<(BrowseTarget, Receiver<BrowseMsg>)>,
}

impl SettingsView {
    pub fn new() -> Self {
        Self {
            model: None,
            busy: false,
            error: None,
            browse: None,
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

    /// Drain a finished browse dialog into the draft.
    fn poll_browse(&mut self) {
        let Some((target, rx)) = self.browse.as_ref() else {
            return;
        };
        let target = *target;
        match rx.try_recv() {
            Ok(BrowseMsg::Picked(path)) => {
                let path = contract_home(&path);
                if let Some(m) = self.model.as_mut() {
                    match target {
                        BrowseTarget::ClipsDir => m.draft.storage.clips_dir = Some(path),
                        BrowseTarget::Hook => m.draft.hooks.on_clip_saved = Some(path),
                    }
                }
                self.browse = None;
            }
            Ok(BrowseMsg::Cancelled) => self.browse = None,
            Ok(BrowseMsg::Unavailable) => {
                self.error =
                    Some("No file dialog found — install `zenity` (or type the path)".into());
                self.browse = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.browse = None,
        }
    }

    fn start_browse(&mut self, target: BrowseTarget, ctx: &egui::Context) {
        if self.browse.is_some() {
            return;
        }
        let (tx, rx) = channel();
        self.browse = Some((target, rx));
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let msg = match target {
                BrowseTarget::ClipsDir => pick_path(true),
                BrowseTarget::Hook => pick_path(false),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Render the page; returns the action for the app to perform.
    pub fn ui(&mut self, ctx: &egui::Context) -> SettingsAction {
        let mut action = SettingsAction::None;
        self.poll_browse();

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

        let mut browse_request: Option<BrowseTarget> = None;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(0.0)),
            )
            .show(ctx, |ui| {
                let browsing = self.browse.is_some();
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
                    // One centered column, ~620px: a form wants a measure, not
                    // the full window width.
                    let col = 620.0_f32.min(ui.available_width() - 2.0 * theme::SP_4);
                    let pad = ((ui.available_width() - col) / 2.0).max(theme::SP_4);
                    ui.horizontal(|ui| {
                        ui.add_space(pad);
                        ui.vertical(|ui| {
                            ui.set_width(col);
                            ui.add_space(theme::SP_3);
                            browse_request = form(ui, model, browsing);
                            ui.add_space(96.0); // room above the sticky footer
                        });
                    });
                });
            });
        if let Some(target) = browse_request {
            self.start_browse(target, ctx);
        }

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

/// Open an external folder (`dir = true`) or file picker, blocking the worker
/// thread until the user answers. Tries `zenity` then `kdialog`; if neither is
/// installed the caller shows an actionable message (typing the path always
/// works).
fn pick_path(dir: bool) -> BrowseMsg {
    use std::process::Command;
    let attempts: [(&str, Vec<&str>); 2] = if dir {
        [
            (
                "zenity",
                vec!["--file-selection", "--directory", "--title=Choose folder"],
            ),
            ("kdialog", vec!["--getexistingdirectory", "."]),
        ]
    } else {
        [
            ("zenity", vec!["--file-selection", "--title=Choose program"]),
            ("kdialog", vec!["--getopenfilename", "."]),
        ]
    };
    for (bin, args) in attempts {
        match Command::new(bin).args(&args).output() {
            Ok(out) if out.status.success() => {
                let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                return if path.is_empty() {
                    BrowseMsg::Cancelled
                } else {
                    BrowseMsg::Picked(path)
                };
            }
            // The dialog ran and the user dismissed it.
            Ok(_) => return BrowseMsg::Cancelled,
            // Tool not installed; try the next one.
            Err(_) => continue,
        }
    }
    BrowseMsg::Unavailable
}

/// Replace a leading `$HOME` with `~` (matches how the config documents paths).
fn contract_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

/// One labeled form row: gold override dot, fixed-width label left, control
/// right-aligned — then a quiet caption underneath explaining the impact.
fn row(
    ui: &mut egui::Ui,
    model: &SettingsModel,
    path: &str,
    label: &str,
    caption: &str,
    control: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        let overridden = model.is_overridden(path);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
        if overridden {
            ui.painter().circle_filled(rect.center(), 2.2, theme::KIN);
            resp.on_hover_text("Overrides the base config");
        }
        ui.add_sized(
            [LABEL_W, 20.0],
            egui::Label::new(
                egui::RichText::new(label)
                    .size(theme::TEXT_BODY)
                    .color(theme::INK_2),
            )
            .halign(egui::Align::LEFT),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
    if !caption.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(CAPTION_INDENT);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(caption)
                        .size(theme::TEXT_MICRO)
                        .color(theme::INK_3),
                )
                .wrap(),
            );
        });
    }
    ui.add_space(theme::SP_2);
}

/// A classic number input: a typed field with ▴/▾ spinner buttons, stepping
/// with the keyboard arrows while focused. Commits valid values as you type
/// and clamps into `range` on Enter / focus loss.
fn stepper_u32(
    ui: &mut egui::Ui,
    id_salt: &str,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    step: u32,
    suffix: &str,
) {
    let id = ui.id().with(id_salt);
    let (min, max) = (*range.start(), *range.end());

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;

        let mut text = ui
            .data_mut(|d| d.get_temp::<String>(id))
            .unwrap_or_else(|| value.to_string());

        let resp = ui.add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(48.0)
                .horizontal_align(egui::Align::RIGHT)
                .font(egui::TextStyle::Body),
        );

        let mut bump = 0i64;
        if resp.has_focus() {
            let (up, down) = ui.input(|i| {
                (
                    i.key_pressed(egui::Key::ArrowUp),
                    i.key_pressed(egui::Key::ArrowDown),
                )
            });
            if up {
                bump += step as i64;
            }
            if down {
                bump -= step as i64;
            }
        }

        if resp.changed() {
            // Commit as-you-type when the text is already a valid in-range
            // number; otherwise wait for Enter/blur (no clamping mid-keystroke).
            if let Ok(v) = text.trim().parse::<u32>() {
                if range.contains(&v) {
                    *value = v;
                }
            }
        }
        let commit = resp.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter));
        if commit {
            let v = text.trim().parse::<u32>().unwrap_or(*value);
            *value = v.clamp(min, max);
            ui.data_mut(|d| d.remove::<String>(id));
        } else if resp.has_focus() {
            ui.data_mut(|d| d.insert_temp(id, text));
        } else {
            ui.data_mut(|d| d.remove::<String>(id));
        }

        // ▴/▾ spinner buttons (and the arrow-key steps from above).
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.spacing_mut().button_padding = egui::vec2(3.0, 0.0);
            let up = ui.add_sized(
                [16.0, 10.0],
                egui::Button::new(egui::RichText::new("▴").size(7.5)),
            );
            let down = ui.add_sized(
                [16.0, 10.0],
                egui::Button::new(egui::RichText::new("▾").size(7.5)),
            );
            if up.clicked() {
                bump += step as i64;
            }
            if down.clicked() {
                bump -= step as i64;
            }
        });

        if bump != 0 {
            let v = (*value as i64 + bump).clamp(min as i64, max as i64) as u32;
            *value = v;
            ui.data_mut(|d| d.remove::<String>(id));
        }

        if !suffix.is_empty() {
            ui.label(
                egui::RichText::new(suffix)
                    .size(theme::TEXT_LABEL)
                    .color(theme::INK_3),
            );
        }
    });
}

/// Checkbox + spinner for an `Option<u32>` field.
#[allow(clippy::too_many_arguments)]
fn optional_u32(
    ui: &mut egui::Ui,
    id_salt: &str,
    value: &mut Option<u32>,
    default_when_on: u32,
    range: std::ops::RangeInclusive<u32>,
    step: u32,
    suffix: &str,
) {
    let mut on = value.is_some();
    if ui.checkbox(&mut on, "").changed() {
        *value = on.then_some(default_when_on);
    }
    if let Some(v) = value.as_mut() {
        stepper_u32(ui, id_salt, v, range, step, suffix);
    } else {
        ui.label(
            egui::RichText::new("off")
                .size(theme::TEXT_LABEL)
                .color(theme::INK_3),
        );
    }
}

/// A path text input with a Browse button (in a right-to-left parent the
/// button lands right of the field). Returns `Some(target)` when Browse was
/// clicked.
fn path_input(
    ui: &mut egui::Ui,
    target: BrowseTarget,
    value: &mut Option<String>,
    hint: &str,
    browsing: bool,
) -> Option<BrowseTarget> {
    let mut clicked = None;
    ui.add_enabled_ui(!browsing, |ui| {
        if ui
            .button("Browse…")
            .on_hover_text("Pick with a file dialog (zenity/kdialog)")
            .clicked()
        {
            clicked = Some(target);
        }
    });
    let mut text = value.clone().unwrap_or_default();
    let resp = ui.add(
        egui::TextEdit::singleline(&mut text)
            .hint_text(hint)
            .desired_width(240.0),
    );
    if resp.changed() {
        *value = (!text.trim().is_empty()).then(|| text.trim().to_string());
    }
    clicked
}

fn form(ui: &mut egui::Ui, model: &mut SettingsModel, browsing: bool) -> Option<BrowseTarget> {
    // Read-only snapshot for override dots while the closures borrow draft.
    let snapshot = model.clone();
    let mut browse = None;

    theme::section(ui, "Capture");
    row(
        ui,
        &snapshot,
        "capture.fps",
        "Frame rate",
        "Frames per second the buffer records at. Higher is smoother but costs \
         more GPU encode time, RAM, and disk per clip. Applying restarts capture.",
        |ui| {
            stepper_u32(ui, "fps", &mut model.draft.capture.fps, 1..=240, 5, "fps");
        },
    );
    row(
        ui,
        &snapshot,
        "capture.buffer_seconds",
        "Replay buffer",
        "How many seconds of the past are kept in RAM for `ord save`. Longer \
         buffers use proportionally more memory. Applies live.",
        |ui| {
            stepper_u32(
                ui,
                "buffer",
                &mut model.draft.capture.buffer_seconds,
                5..=3600,
                5,
                "s",
            );
        },
    );
    row(
        ui,
        &snapshot,
        "capture.quality",
        "Quality preset",
        "Encoder constant-quality level. Higher looks better and makes bigger \
         clips. Ignored while a constant bitrate is set below.",
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
        "H.264 plays everywhere. HEVC and AV1 give noticeably smaller clips at \
         the same quality, but AV1 encoding needs an RTX 40/50-series card.",
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
        "Lock the encoder to a fixed bitrate: predictable RAM use and clip sizes \
         even in high-motion scenes. Off = constant quality (sharper, variable size).",
        |ui| {
            optional_u32(
                ui,
                "bitrate",
                &mut model.draft.capture.bitrate_kbps,
                12_000,
                1_000..=200_000,
                500,
                "kbps",
            );
        },
    );
    row(
        ui,
        &snapshot,
        "capture.clear_on_save",
        "Clear buffer after save",
        "Drop everything buffered after each save, so back-to-back saves never \
         contain the same footage twice.",
        |ui| {
            ui.checkbox(&mut model.draft.capture.clear_on_save, "");
        },
    );

    theme::section(ui, "Audio");
    row(
        ui,
        &snapshot,
        "audio.desktop",
        "Desktop audio",
        "Record the game/desktop output — including friends' voices playing \
         through your speakers.",
        |ui| {
            ui.checkbox(&mut model.draft.audio.desktop, "");
        },
    );
    row(
        ui,
        &snapshot,
        "audio.mic",
        "Microphone",
        "Record your own voice, mixed with desktop audio into one track.",
        |ui| {
            ui.checkbox(&mut model.draft.audio.mic, "");
        },
    );

    theme::section(ui, "Overlay");
    row(
        ui,
        &snapshot,
        "overlay.show_status_dot",
        "Show status dot",
        "The small corner dot over your game: red while the replay buffer is \
         armed, grey when the daemon is offline. Turn off for an invisible \
         overlay — save/marker toasts still appear. Applies live.",
        |ui| {
            ui.checkbox(&mut model.draft.overlay.show_status_dot, "");
        },
    );

    theme::section(ui, "Storage");
    row(
        ui,
        &snapshot,
        "storage.clips_dir",
        "Clips folder",
        "Where saved clips and recordings land. Empty = ~/Videos/open-recorder; \
         `~` expands to your home.",
        |ui| {
            let picked = path_input(
                ui,
                BrowseTarget::ClipsDir,
                &mut model.draft.storage.clips_dir,
                "~/Videos/open-recorder",
                browsing,
            );
            if picked.is_some() {
                browse = picked;
            }
        },
    );
    row(
        ui,
        &snapshot,
        "storage.template",
        "Filename template",
        "Tokens: {game} {rec} {epoch} {date} {time}. A `/` creates subfolders — \
         e.g. {date}/{game}-{epoch} groups clips into date folders.",
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
        "Delete the oldest clips once the library exceeds this size. Files in \
         exports/ are never touched.",
        |ui| {
            optional_u32(
                ui,
                "max-gib",
                &mut model.draft.storage.max_gib,
                25,
                1..=4096,
                1,
                "GiB",
            );
        },
    );
    row(
        ui,
        &snapshot,
        "storage.max_age_days",
        "Auto-prune older than",
        "Delete clips older than this many days, regardless of library size.",
        |ui| {
            optional_u32(
                ui,
                "max-age",
                &mut model.draft.storage.max_age_days,
                90,
                1..=3650,
                1,
                "days",
            );
        },
    );

    theme::section(ui, "Markers");
    row(
        ui,
        &snapshot,
        "markers.auto_save_seconds",
        "Auto-save on mark",
        "`ord mark` bookmarks the moment (a chapter in the next save). With \
         this on it also saves the last N seconds — bookmark and clip in one key.",
        |ui| {
            optional_u32(
                ui,
                "auto-save",
                &mut model.draft.markers.auto_save_seconds,
                30,
                1..=600,
                5,
                "s",
            );
        },
    );

    theme::section(ui, "Hooks");
    row(
        ui,
        &snapshot,
        "hooks.on_clip_saved",
        "After every save, run",
        "Program run with the clip path as $1 after each verified save — use it \
         for notifications, uploads, or renames. Asynchronous; failures only log.",
        |ui| {
            let picked = path_input(
                ui,
                BrowseTarget::Hook,
                &mut model.draft.hooks.on_clip_saved,
                "~/bin/clip-hook",
                browsing,
            );
            if picked.is_some() {
                browse = picked;
            }
        },
    );

    theme::section(ui, "Export defaults");
    row(
        ui,
        &snapshot,
        "export.codec",
        "Codec",
        "Default codec for exports. AV1 compresses best and plays in modern \
         browsers and Discord; H.264 is the safest for old devices.",
        |ui| {
            egui::ComboBox::from_id_salt("export-codec")
                .selected_text(format!("{:?}", model.draft.export.codec))
                .show_ui(ui, |ui| {
                    for c in [ExportCodec::Av1, ExportCodec::Hevc, ExportCodec::H264] {
                        ui.selectable_value(&mut model.draft.export.codec, c, format!("{c:?}"));
                    }
                });
        },
    );
    row(
        ui,
        &snapshot,
        "export.container",
        "Container",
        "MP4 is the most shareable (Discord inline, phones); MKV is more robust \
         and keeps chapters.",
        |ui| {
            egui::ComboBox::from_id_salt("export-container")
                .selected_text(format!("{:?}", model.draft.export.container))
                .show_ui(ui, |ui| {
                    for c in [Container::Mp4, Container::Mkv] {
                        ui.selectable_value(&mut model.draft.export.container, c, format!("{c:?}"));
                    }
                });
        },
    );

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

    browse
}

fn codec_label(c: CaptureCodec) -> &'static str {
    match c {
        CaptureCodec::H264 => "H.264 (compatible)",
        CaptureCodec::Hevc => "HEVC",
        CaptureCodec::Av1 => "AV1 (best compression)",
    }
}
