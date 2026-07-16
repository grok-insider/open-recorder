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
use ord_common::config::{
    CaptureCodec, ColorRange, Container, EncoderTune, ExportCodec, FpsMode, FramerateMode,
    PressedKeysPosition, Quality, ReplayStorage, Resolution,
};
use ord_common::{
    estimate_buffer_mib, minimum_bitrate_kbps, recommended_bitrate_kbps, BitrateTier, Config,
    OutputInfo,
};

use crate::settings::{capture_summary, ApplyTier, CaptureProfile, SettingsModel};
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
    /// Connected displays from the last `ListOutputs` reply (may be empty).
    pub outputs: Vec<OutputInfo>,
    /// An apply is in flight; disable the footer until the daemon replies.
    pub busy: bool,
    /// Last daemon error for this page, shown inline.
    pub error: Option<String>,
    /// Advanced capture knobs collapsed by default.
    advanced_open: bool,
    /// An external file/folder dialog in flight (its result lands here).
    browse: Option<(BrowseTarget, Receiver<BrowseMsg>)>,
}

/// Side-channel actions the form needs from the app (beyond Apply/Back).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsExtra {
    /// Re-probe displays (`Command::ListOutputs`).
    RefreshOutputs,
}

impl SettingsView {
    pub fn new() -> Self {
        Self {
            model: None,
            outputs: Vec::new(),
            busy: false,
            error: None,
            advanced_open: false,
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

    /// Feed a `Event::Outputs` reply.
    pub fn on_outputs(&mut self, outputs: Vec<OutputInfo>) {
        self.outputs = outputs;
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
    pub fn ui(&mut self, ctx: &egui::Context) -> (SettingsAction, Option<SettingsExtra>) {
        let mut action = SettingsAction::None;
        let mut extra = None;
        self.poll_browse();

        egui::TopBottomPanel::top("settings-top")
            .frame(theme::chrome())
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let back = ui.button("←  Library");
                    crate::a11y::button(&back, "Library");
                    if back.clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
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
        let outputs = self.outputs.clone();
        let mut advanced_open = self.advanced_open;
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

                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        // Responsive measure: grow on large displays, keep a
                        // readable column on small ones (see layout::form_column_width).
                        let pad = 2.0 * theme::SP_4;
                        let col =
                            crate::layout::form_column_width((ui.available_width() - pad).max(0.0));
                        ui.vertical_centered(|ui| {
                            ui.set_max_width(col);
                            ui.add_space(theme::SP_3);
                            let (b, e) = form(ui, model, &outputs, browsing, &mut advanced_open);
                            browse_request = b;
                            if e.is_some() {
                                extra = e;
                            }
                            ui.add_space(theme::SP_6); // breathing room at the end
                        });
                    });
            });
        self.advanced_open = advanced_open;
        if let Some(target) = browse_request {
            self.start_browse(target, ctx);
        }

        (action, extra)
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
                        crate::a11y::button(&apply.inner, "Apply");
                        if apply.inner.clicked() {
                            self.busy = true;
                            action = Some(SettingsAction::Apply(Box::new(model.draft.clone())));
                        }
                        if self.busy {
                            ui.spinner();
                        }
                        ui.add_enabled_ui(dirty && !self.busy, |ui| {
                            let rev = ui.button("Revert");
                            crate::a11y::button(&rev, "Revert");
                            if rev.clicked() {
                                model.revert();
                            }
                        });
                        ui.add_enabled_ui(!self.busy, |ui| {
                            let reset = ui.button("Reset to base").on_hover_text(
                                "Discard every runtime override and go back to the values \
                                 in config.toml (applies on Apply).",
                            );
                            crate::a11y::button(&reset, "Reset to base");
                            if reset.clicked() {
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

/// Replace a leading home-directory prefix with `~` (matches how the config
/// documents paths).
fn contract_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy();
        if !home.is_empty() && path.starts_with(home.as_ref()) {
            return format!("~{}", &path[home.len()..]);
        }
    }
    path.to_string()
}

/// One labeled form row: gold override dot, fixed-width label left, control
/// right-aligned — then a quiet caption underneath explaining the impact.
/// `overridden` is the per-frame list from [`SettingsModel::overridden`],
/// computed once by the form (recomputing the full config diff per row per
/// frame was measurable waste).
fn row(
    ui: &mut egui::Ui,
    overridden: &[String],
    path: &str,
    label: &str,
    caption: &str,
    control: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        let overridden = overridden.iter().any(|p| p == path);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
        if overridden {
            ui.painter().circle_filled(rect.center(), 2.2, theme::KIN);
            resp.on_hover_text("Overrides the base config");
        }
        // Fixed-width, left-aligned label column so every control starts on
        // the same vertical line.
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_W, 22.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(
                    egui::RichText::new(label)
                        .size(theme::TEXT_BODY)
                        .color(theme::INK_2),
                );
            },
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
///
/// Laid out as one tight fixed-size left-to-right group, so it stays
/// `[ value ][▴▾] suffix` even inside the form's right-to-left control area.
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

    let suffix_w = if suffix.is_empty() {
        0.0
    } else {
        4.0 + ui.fonts(|f| {
            f.layout_no_wrap(
                suffix.to_owned(),
                egui::FontId::proportional(theme::TEXT_LABEL),
                theme::INK_3,
            )
            .size()
            .x
        })
    };
    let size = egui::vec2(56.0 + 4.0 + 18.0 + suffix_w, 23.0);

    ui.allocate_ui_with_layout(
        size,
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(4.0, 0.0);

            let mut text = ui
                .data_mut(|d| d.get_temp::<String>(id))
                .unwrap_or_else(|| value.to_string());

            let resp = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .desired_width(56.0)
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
                // number; otherwise wait for Enter/blur (no mid-keystroke clamp).
                if let Ok(v) = text.trim().parse::<u32>() {
                    if range.contains(&v) {
                        *value = v;
                    }
                }
            }
            let commit = resp.lost_focus()
                || (resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if commit {
                let v = text.trim().parse::<u32>().unwrap_or(*value);
                *value = v.clamp(min, max);
                ui.data_mut(|d| d.remove::<String>(id));
            } else if resp.has_focus() {
                ui.data_mut(|d| d.insert_temp(id, text));
            } else {
                ui.data_mut(|d| d.remove::<String>(id));
            }

            // Spinner buttons (the keyboard arrows above do the same). The
            // arrows are painted, not glyphs — crisp at this tiny size.
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                // Buttons won't go below `interact_size`; shrink it so the
                // pair actually fits the field height instead of wrapping.
                ui.spacing_mut().interact_size = egui::vec2(8.0, 8.0);
                ui.spacing_mut().button_padding = egui::vec2(0.0, 0.0);
                let up = ui.add_sized([18.0, 11.0], egui::Button::new(""));
                let down = ui.add_sized([18.0, 11.0], egui::Button::new(""));
                for (resp, point_up) in [(&up, true), (&down, false)] {
                    let c = resp.rect.center();
                    let (w, h) = (3.5, 2.5);
                    let pts = if point_up {
                        vec![
                            egui::pos2(c.x - w, c.y + h),
                            egui::pos2(c.x + w, c.y + h),
                            egui::pos2(c.x, c.y - h),
                        ]
                    } else {
                        vec![
                            egui::pos2(c.x - w, c.y - h),
                            egui::pos2(c.x + w, c.y - h),
                            egui::pos2(c.x, c.y + h),
                        ]
                    };
                    ui.painter().add(egui::Shape::convex_polygon(
                        pts,
                        theme::INK_2,
                        egui::Stroke::NONE,
                    ));
                }
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
        },
    );
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

fn form(
    ui: &mut egui::Ui,
    model: &mut SettingsModel,
    outputs: &[OutputInfo],
    browsing: bool,
    advanced_open: &mut bool,
) -> (Option<BrowseTarget>, Option<SettingsExtra>) {
    // Computed once per frame; each row only scans this list for its path.
    let overridden = model.overridden();
    let mut browse = None;
    let mut extra = None;

    theme::section(ui, "Recording");
    // Profile chips
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("Profile")
                .size(theme::TEXT_LABEL)
                .color(theme::INK_2),
        );
        ui.add_space(theme::SP_2);
        let current = CaptureProfile::detect(&model.draft.capture);
        for p in CaptureProfile::ALL {
            let selected = p == current;
            let resp = ui.selectable_label(selected, p.label());
            crate::a11y::button(&resp, &format!("Profile {}", p.label()));
            if resp.clicked() && p != CaptureProfile::Custom {
                p.apply(&mut model.draft.capture);
            }
            if p != CaptureProfile::Custom {
                resp.on_hover_text(profile_hint(p));
            }
        }
    });
    ui.add_space(theme::SP_2);

    // Live summary banner
    let summary = capture_summary(&model.draft.capture, outputs);
    egui::Frame::none()
        .fill(theme::SURFACE)
        .stroke(egui::Stroke::new(1.0, theme::HAIRLINE))
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .rounding(theme::RADIUS)
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!(
                    "Will capture ≈ {} @ {} · {} · {} s buffer",
                    summary.resolution_label,
                    summary.fps_label,
                    summary.quality_label,
                    summary.buffer_secs
                ))
                .size(theme::TEXT_LABEL)
                .color(theme::INK),
            );
            ui.label(
                egui::RichText::new(
                    "Apply restarts capture when source, resolution, fps, codec, or quality change.",
                )
                .size(theme::TEXT_MICRO)
                .color(theme::INK_3),
            );
        });
    ui.add_space(theme::SP_3);

    row(
        ui,
        &overridden,
        "capture.target",
        "Source",
        "Portal opens the system share picker (restore token remembers your choice). \
         Named monitors list modes from the compositor when available.",
        |ui| {
            let mut target = model.draft.capture.target.clone();
            let display = if target == "portal" {
                "Portal (picker)".to_string()
            } else {
                target.clone()
            };
            egui::ComboBox::from_id_salt("capture-target")
                .selected_text(display)
                .width(220.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut target, "portal".into(), "Portal (picker)");
                    for o in outputs {
                        let label = format!(
                            "{} — {}×{} @ {} Hz",
                            o.name,
                            o.width,
                            o.height,
                            o.refresh_fps()
                        );
                        ui.selectable_value(&mut target, o.name.clone(), label);
                    }
                });
            model.draft.capture.target = target;
            let refresh = ui.small_button("↻").on_hover_text("Refresh display list");
            crate::a11y::button(&refresh, "Refresh displays");
            if refresh.clicked() {
                extra = Some(SettingsExtra::RefreshOutputs);
            }
        },
    );

    // Resolution
    row(
        ui,
        &overridden,
        "capture.resolution",
        "Resolution",
        "Native keeps the full monitor. Lower sizes reduce encode cost and file size \
         (downscale is applied when the capture backend supports it).",
        |ui| {
            let preset = res_preset(&model.draft.capture.resolution);
            let mut choice = preset;
            egui::ComboBox::from_id_salt("res-preset")
                .selected_text(res_preset_label(choice))
                .show_ui(ui, |ui| {
                    for p in ResPreset::ALL {
                        ui.selectable_value(&mut choice, p, res_preset_label(p));
                    }
                });
            if choice != preset {
                model.draft.capture.resolution = choice.to_resolution();
            }
            if choice == ResPreset::Custom {
                let mut w = model
                    .draft
                    .capture
                    .resolution
                    .map(|r| r.width)
                    .unwrap_or(1920);
                let mut h = model
                    .draft
                    .capture
                    .resolution
                    .map(|r| r.height)
                    .unwrap_or(1080);
                ui.label("W");
                stepper_u32(ui, "res-w", &mut w, 16..=16384, 2, "");
                ui.label("H");
                stepper_u32(ui, "res-h", &mut h, 16..=16384, 2, "");
                // Keep even for NVENC.
                w -= w % 2;
                h -= h % 2;
                model.draft.capture.resolution = Some(Resolution {
                    width: w.max(16),
                    height: h.max(16),
                });
            }
        },
    );

    // Frame rate
    row(
        ui,
        &overridden,
        "capture.fps_mode",
        "Frame rate",
        "Match display uses the selected/focused monitor refresh (like OBS). \
         Fixed locks a CFR target. Applying restarts capture.",
        |ui| {
            let mut auto = model.draft.capture.fps_mode == FpsMode::Auto;
            if ui.checkbox(&mut auto, "Match display refresh").changed() {
                model.draft.capture.fps_mode = if auto { FpsMode::Auto } else { FpsMode::Fixed };
            }
            if model.draft.capture.fps_mode == FpsMode::Fixed {
                let rates = [30_u32, 60, 120, 144, 165, 240];
                egui::ComboBox::from_id_salt("fps-preset")
                    .selected_text(format!("{} fps", model.draft.capture.fps))
                    .show_ui(ui, |ui| {
                        for r in rates {
                            ui.selectable_value(
                                &mut model.draft.capture.fps,
                                r,
                                format!("{r} fps"),
                            );
                        }
                    });
                stepper_u32(ui, "fps", &mut model.draft.capture.fps, 1..=240, 1, "fps");
            } else if let Some(o) = outputs.iter().find(|o| {
                let t = model.draft.capture.target.trim();
                if t != "portal" && !t.is_empty() {
                    o.name == t
                } else {
                    o.focused
                }
            }) {
                ui.label(
                    egui::RichText::new(format!("~{} fps from {}", o.refresh_fps(), o.name))
                        .size(theme::TEXT_LABEL)
                        .color(theme::INK_2),
                );
            } else {
                ui.label(
                    egui::RichText::new("no display probe — will use fallback fps")
                        .size(theme::TEXT_LABEL)
                        .color(theme::KIN),
                );
            }
        },
    );

    row(
        ui,
        &overridden,
        "capture.buffer_seconds",
        "Replay buffer",
        "How many seconds of the past are kept for `ord save`. Longer buffers use \
         more memory (or disk if advanced storage is set to Disk). Applies live.",
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
        &overridden,
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
        &overridden,
        "capture.codec",
        "Codec",
        "H.264 plays everywhere. HEVC and AV1 give smaller clips at the same \
         quality; AV1 needs an RTX 40/50-series card to encode.",
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
    let (rec_w, rec_h, rec_fps) = capture_geometry_hint(&model.draft.capture, outputs);
    let recommended = recommended_bitrate_kbps(
        rec_w,
        rec_h,
        rec_fps,
        model.draft.capture.codec,
        BitrateTier::from(model.draft.capture.quality),
    );
    let minimum = minimum_bitrate_kbps(rec_w, rec_h, rec_fps, model.draft.capture.codec);
    let buffer_hint = estimate_buffer_mib(recommended, model.draft.capture.buffer_seconds);
    let bitrate_caption = format!(
        "Lock the encoder to a fixed bitrate for predictable RAM use. Off = constant quality \
         (sharper). Recommended ~{recommended} kbps for {rec_w}×{rec_h} @ {rec_fps} · min {minimum} \
         · buffer ~{buffer_hint} MiB at recommended."
    );
    row(
        ui,
        &overridden,
        "capture.bitrate_kbps",
        "Constant bitrate",
        &bitrate_caption,
        |ui| {
            optional_u32(
                ui,
                "bitrate",
                &mut model.draft.capture.bitrate_kbps,
                recommended,
                1_000..=200_000,
                500,
                "kbps",
            );
        },
    );
    if let Some(kbps) = model.draft.capture.bitrate_kbps {
        if kbps < minimum {
            ui.horizontal(|ui| {
                ui.add_space(CAPTION_INDENT);
                ui.colored_label(
                    theme::KIN,
                    format!(
                        "⚠ {kbps} kbps is below the {minimum} kbps floor for this res/fps — \
                         Apply will raise it to ~{recommended} kbps so clips stay sharp."
                    ),
                );
            });
            ui.add_space(theme::SP_2);
        }
    }
    row(
        ui,
        &overridden,
        "capture.clear_on_save",
        "Clear buffer after save",
        "Drop everything buffered after each save, so back-to-back saves never \
         contain the same footage twice.",
        |ui| {
            ui.checkbox(&mut model.draft.capture.clear_on_save, "");
        },
    );

    // Advanced capture knobs
    ui.add_space(theme::SP_2);
    egui::CollapsingHeader::new("Advanced capture")
        .id_salt("adv-capture")
        .default_open(*advanced_open)
        .show(ui, |ui| {
            *advanced_open = true;
            row(
                ui,
                &overridden,
                "capture.keyframe_interval_ms",
                "Keyframe interval",
                "GOP length in milliseconds. Smaller = finer “save last N” reachability \
                 and seeking, at a small bitrate cost. Default 2000 ms.",
                |ui| {
                    stepper_u32(
                        ui,
                        "keyint",
                        &mut model.draft.capture.keyframe_interval_ms,
                        100..=10_000,
                        100,
                        "ms",
                    );
                },
            );
            row(
                ui,
                &overridden,
                "capture.framerate_mode",
                "Frame timing",
                "CFR is safest for editors. VFR skips static frames. Content syncs to \
                 screen updates (best under VRR; needs backend support).",
                |ui| {
                    let m = &mut model.draft.capture.framerate_mode;
                    egui::ComboBox::from_id_salt("frmode")
                        .selected_text(match *m {
                            FramerateMode::Cfr => "CFR (constant)",
                            FramerateMode::Vfr => "VFR (variable)",
                            FramerateMode::Content => "Content-sync",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(m, FramerateMode::Cfr, "CFR (constant)");
                            ui.selectable_value(m, FramerateMode::Vfr, "VFR (variable)");
                            ui.selectable_value(m, FramerateMode::Content, "Content-sync");
                        });
                },
            );
            row(
                ui,
                &overridden,
                "capture.color_range",
                "Color range",
                "Limited is most compatible. Full preserves studio range when the pipeline supports it.",
                |ui| {
                    let m = &mut model.draft.capture.color_range;
                    egui::ComboBox::from_id_salt("crange")
                        .selected_text(format!("{m:?}"))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(m, ColorRange::Limited, "Limited");
                            ui.selectable_value(m, ColorRange::Full, "Full");
                        });
                },
            );
            row(
                ui,
                &overridden,
                "capture.tune",
                "Encoder tune",
                "Performance = lowest overhead while gaming. Quality biases the encoder toward fidelity.",
                |ui| {
                    let m = &mut model.draft.capture.tune;
                    egui::ComboBox::from_id_salt("tune")
                        .selected_text(format!("{m:?}"))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(m, EncoderTune::Performance, "Performance");
                            ui.selectable_value(m, EncoderTune::Quality, "Quality");
                        });
                },
            );
            row(
                ui,
                &overridden,
                "capture.replay_storage",
                "Replay storage",
                "RAM is lowest latency. Disk spills encoded frames so long windows fit on low-RAM machines.",
                |ui| {
                    let m = &mut model.draft.capture.replay_storage;
                    egui::ComboBox::from_id_salt("rstore")
                        .selected_text(format!("{m:?}"))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(m, ReplayStorage::Ram, "RAM");
                            ui.selectable_value(m, ReplayStorage::Disk, "Disk");
                        });
                },
            );
            row(
                ui,
                &overridden,
                "capture.auto_arm",
                "Auto-arm on game",
                "Start the replay buffer when a game takes the foreground (Steam app or fullscreen).",
                |ui| {
                    ui.checkbox(&mut model.draft.capture.auto_arm, "");
                },
            );
            row(
                ui,
                &overridden,
                "capture.hdr",
                "HDR capture",
                "10-bit BT.2020/PQ. Requires HEVC or AV1 and a capture path that can carry HDR \
                 (portal tonemaps to SDR today).",
                |ui| {
                    ui.checkbox(&mut model.draft.capture.hdr, "");
                },
            );
        });

    theme::section(ui, "Audio");
    row(
        ui,
        &overridden,
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
        &overridden,
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
        &overridden,
        "overlay.show_status_dot",
        "Show status dot",
        "The small corner dot over your game: red while the replay buffer is \
         armed, grey when the daemon is offline. Turn off for an invisible \
         overlay — save/marker toasts still appear. Applies live.",
        |ui| {
            ui.checkbox(&mut model.draft.overlay.show_status_dot, "");
        },
    );
    row(
        ui,
        &overridden,
        "overlay.pressed_keys.enabled",
        "Show pressed keys",
        "Render the current keyboard shortcut in the recorded demo. Off by \
         default because enabling it reads raw keyboard input.",
        |ui| {
            ui.checkbox(&mut model.draft.overlay.pressed_keys.enabled, "");
        },
    );
    pressed_keys_layout_editor(ui, &overridden, model);
    row(
        ui,
        &overridden,
        "overlay.pressed_keys.timeout_ms",
        "Key visibility",
        "How long the last shortcut remains visible after the keys are released.",
        |ui| {
            stepper_u32(
                ui,
                "pressed-keys-timeout",
                &mut model.draft.overlay.pressed_keys.timeout_ms,
                250..=5000,
                250,
                "ms",
            );
        },
    );
    row(
        ui,
        &overridden,
        "overlay.pressed_keys.max_keys",
        "Max keys shown",
        "Caps very large chords so the strip stays compact.",
        |ui| {
            let mut max = model.draft.overlay.pressed_keys.max_keys as u32;
            stepper_u32(ui, "pressed-keys-max", &mut max, 1..=8, 1, "keys");
            model.draft.overlay.pressed_keys.max_keys = max as u8;
        },
    );

    theme::section(ui, "Storage");
    row(
        ui,
        &overridden,
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
        &overridden,
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
        &overridden,
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
        &overridden,
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
        &overridden,
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
        &overridden,
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
        &overridden,
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
        &overridden,
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

    (browse, extra)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResPreset {
    Native,
    P1080,
    P1440,
    P4k,
    Custom,
}

impl ResPreset {
    const ALL: [ResPreset; 5] = [
        ResPreset::Native,
        ResPreset::P1080,
        ResPreset::P1440,
        ResPreset::P4k,
        ResPreset::Custom,
    ];

    fn to_resolution(self) -> Option<Resolution> {
        match self {
            ResPreset::Native => None,
            ResPreset::P1080 => Some(Resolution {
                width: 1920,
                height: 1080,
            }),
            ResPreset::P1440 => Some(Resolution {
                width: 2560,
                height: 1440,
            }),
            ResPreset::P4k => Some(Resolution {
                width: 3840,
                height: 2160,
            }),
            ResPreset::Custom => Some(Resolution {
                width: 1920,
                height: 1080,
            }),
        }
    }
}

fn res_preset(res: &Option<Resolution>) -> ResPreset {
    match res {
        None => ResPreset::Native,
        Some(Resolution {
            width: 1920,
            height: 1080,
        }) => ResPreset::P1080,
        Some(Resolution {
            width: 2560,
            height: 1440,
        }) => ResPreset::P1440,
        Some(Resolution {
            width: 3840,
            height: 2160,
        }) => ResPreset::P4k,
        Some(_) => ResPreset::Custom,
    }
}

/// Best-effort width/height/fps for bitrate recommendations (probe-aware).
fn capture_geometry_hint(
    cap: &ord_common::config::CaptureConfig,
    outputs: &[OutputInfo],
) -> (u32, u32, u32) {
    let (w, h) = match cap.resolution {
        Some(r) => (r.width.max(1), r.height.max(1)),
        None => {
            let o = outputs
                .iter()
                .find(|o| o.name == cap.target)
                .or_else(|| outputs.iter().find(|o| o.focused))
                .or_else(|| outputs.first());
            match o {
                Some(o) => (o.width.max(1), o.height.max(1)),
                None => (2560, 1440),
            }
        }
    };
    let fps = match cap.fps_mode {
        FpsMode::Fixed => cap.fps.max(1),
        FpsMode::Auto => {
            let o = outputs
                .iter()
                .find(|o| o.name == cap.target)
                .or_else(|| outputs.iter().find(|o| o.focused))
                .or_else(|| outputs.first());
            o.map(|o| o.refresh_fps().max(1))
                .unwrap_or(cap.fps.max(1))
        }
    };
    (w, h, fps)
}

fn res_preset_label(p: ResPreset) -> &'static str {
    match p {
        ResPreset::Native => "Match display (native)",
        ResPreset::P1080 => "1080p (1920×1080)",
        ResPreset::P1440 => "1440p (2560×1440)",
        ResPreset::P4k => "4K (3840×2160)",
        ResPreset::Custom => "Custom…",
    }
}

fn profile_hint(p: CaptureProfile) -> &'static str {
    match p {
        CaptureProfile::Performance => "1080p60 H.264 Medium — lightest GPU cost",
        CaptureProfile::Balanced => "Native 60 HEVC High — good quality/size",
        CaptureProfile::Competitive => "1080p144 H.264 CBR 20 Mbps — high refresh",
        CaptureProfile::Quality => "Native @ display refresh, AV1 Ultra",
        CaptureProfile::Custom => "",
    }
}

fn pressed_keys_layout_editor(ui: &mut egui::Ui, overridden: &[String], model: &mut SettingsModel) {
    let paths = [
        "overlay.pressed_keys.position",
        "overlay.pressed_keys.x_ppm",
        "overlay.pressed_keys.y_ppm",
        "overlay.pressed_keys.scale_percent",
        "overlay.pressed_keys.opacity_percent",
        "overlay.pressed_keys.rotation_degrees",
    ];
    let any_override = paths
        .iter()
        .any(|path| overridden.iter().any(|p| p == path));

    ui.horizontal(|ui| {
        let (dot, resp) = ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
        if any_override {
            ui.painter().circle_filled(dot.center(), 2.2, theme::KIN);
            resp.on_hover_text("Overrides the base config");
        }
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_W, 22.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.label(
                    egui::RichText::new("Keycap layout")
                        .size(theme::TEXT_BODY)
                        .color(theme::INK_2),
                );
            },
        );
        ui.vertical(|ui| {
            let keys = &mut model.draft.overlay.pressed_keys;
            pressed_keys_preview(ui, keys);
            ui.add_space(theme::SP_2);
            ui.horizontal_wrapped(|ui| {
                for p in [
                    PressedKeysPosition::BottomCenter,
                    PressedKeysPosition::BottomLeft,
                    PressedKeysPosition::BottomRight,
                    PressedKeysPosition::TopCenter,
                ] {
                    if ui
                        .selectable_label(keys.position == p, pressed_keys_position_label(p))
                        .clicked()
                    {
                        keys.position = p;
                        let (x, y) = pressed_keys_preset_ppm(p);
                        keys.x_ppm = x;
                        keys.y_ppm = y;
                    }
                }
                if ui.button("Reset").clicked() {
                    let defaults = ord_common::PressedKeysConfig::default();
                    keys.position = defaults.position;
                    keys.x_ppm = defaults.x_ppm;
                    keys.y_ppm = defaults.y_ppm;
                    keys.scale_percent = defaults.scale_percent;
                    keys.opacity_percent = defaults.opacity_percent;
                    keys.rotation_degrees = defaults.rotation_degrees;
                }
            });
            let mut scale = keys.scale_percent as i32;
            transform_slider_i32(ui, "Size", &mut scale, 50..=250, "%");
            keys.scale_percent = scale as u16;
            let mut opacity = keys.opacity_percent as i32;
            transform_slider_i32(ui, "Opacity", &mut opacity, 35..=100, "%");
            keys.opacity_percent = opacity as u8;
            let mut rotation = keys.rotation_degrees as i32;
            transform_slider_i32(ui, "Rotation", &mut rotation, -30..=30, "deg");
            keys.rotation_degrees = rotation as i16;
        });
    });
    ui.horizontal(|ui| {
        ui.add_space(CAPTION_INDENT);
        ui.add(
            egui::Label::new(
                egui::RichText::new(
                    "Drag the preview to choose a custom on-screen position. Applies live.",
                )
                .size(theme::TEXT_MICRO)
                .color(theme::INK_3),
            )
            .wrap(),
        );
    });
    ui.add_space(theme::SP_2);
}

fn transform_slider_i32(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut i32,
    range: std::ops::RangeInclusive<i32>,
    suffix: &str,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme::SP_2;
        ui.label(
            egui::RichText::new(label)
                .size(theme::TEXT_LABEL)
                .color(theme::INK_2),
        );
        ui.add(
            egui::Slider::new(value, range)
                .suffix(suffix)
                .show_value(true)
                .text("")
                .clamping(egui::SliderClamping::Always),
        );
    });
}

fn pressed_keys_preview(ui: &mut egui::Ui, keys: &mut ord_common::PressedKeysConfig) {
    let width = ui.available_width().clamp(280.0, 720.0);
    let size = egui::vec2(width, width * 9.0 / 16.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    if (resp.clicked() || resp.dragged()) && resp.interact_pointer_pos().is_some() {
        if let Some(pos) = resp.interact_pointer_pos() {
            keys.position = PressedKeysPosition::Custom;
            keys.x_ppm =
                (((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0) * 1000.0).round() as u16;
            keys.y_ppm =
                (((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0) * 1000.0).round() as u16;
        }
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, theme::RADIUS_CARD, theme::THUMB_BG);
    painter.rect_stroke(
        rect,
        theme::RADIUS_CARD,
        egui::Stroke::new(1.0, theme::HAIRLINE),
    );
    let band = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.bottom() - rect.height() * 0.28),
        rect.right_bottom(),
    );
    painter.rect_filled(band, 0.0, egui::Color32::from_black_alpha(50));
    draw_preview_keycaps(ui, rect, keys);
}

fn draw_preview_keycaps(ui: &mut egui::Ui, rect: egui::Rect, keys: &ord_common::PressedKeysConfig) {
    let labels = ["Ctrl", "Shift", "R"];
    let preview_unit = rect.width() / 1280.0 * (keys.scale_percent.clamp(50, 250) as f32 / 100.0);
    let key_h = 58.0 * preview_unit;
    let gap = 10.0 * preview_unit;
    let font_id = egui::FontId::monospace((22.0 * preview_unit).max(8.0));
    let text_color = egui::Color32::from_rgb(244, 246, 249);
    let widths: Vec<f32> = labels
        .iter()
        .map(|label| {
            let text_w = ui.fonts(|f| {
                f.layout_no_wrap(label.to_string(), font_id.clone(), text_color)
                    .size()
                    .x
            });
            (text_w + 40.0 * preview_unit).max(72.0 * preview_unit)
        })
        .collect();
    let row_w = widths.iter().sum::<f32>() + gap * labels.len().saturating_sub(1) as f32;
    let angle = (keys.rotation_degrees.clamp(-30, 30) as f32).to_radians();
    let center = preview_key_center(keys, rect, row_w, key_h, preview_unit, angle);
    let opacity = keys.opacity_percent.clamp(35, 100) as f32 / 100.0;

    let mut x = -row_w / 2.0;
    for (label, key_w) in labels.iter().zip(widths.iter()) {
        let local =
            egui::Rect::from_min_size(egui::pos2(x, -key_h / 2.0), egui::vec2(*key_w, key_h));
        draw_rotated_preview_key(ui, local, center, angle, opacity, preview_unit);
        let galley = ui.fonts(|f| f.layout_no_wrap(label.to_string(), font_id.clone(), text_color));
        let local_text = egui::pos2(
            local.center().x - galley.size().x / 2.0,
            local.center().y - galley.size().y / 2.0,
        );
        let pos = rotate_preview_point(local_text, center, angle);
        ui.painter().add(
            egui::epaint::TextShape::new(pos, galley, text_color)
                .with_angle(angle)
                .with_opacity_factor(opacity),
        );
        x += *key_w + gap;
    }
}

fn preview_key_center(
    keys: &ord_common::PressedKeysConfig,
    rect: egui::Rect,
    row_w: f32,
    row_h: f32,
    unit: f32,
    angle: f32,
) -> egui::Pos2 {
    let margin = 54.0 * unit;
    let mut center = match keys.position {
        PressedKeysPosition::BottomCenter => {
            egui::pos2(rect.center().x, rect.bottom() - margin - row_h / 2.0)
        }
        PressedKeysPosition::BottomLeft => egui::pos2(
            rect.left() + margin + row_w / 2.0,
            rect.bottom() - margin - row_h / 2.0,
        ),
        PressedKeysPosition::BottomRight => egui::pos2(
            rect.right() - margin - row_w / 2.0,
            rect.bottom() - margin - row_h / 2.0,
        ),
        PressedKeysPosition::TopCenter => {
            egui::pos2(rect.center().x, rect.top() + margin + row_h / 2.0)
        }
        PressedKeysPosition::Custom => egui::pos2(
            rect.left() + rect.width() * keys.x_ppm.min(1000) as f32 / 1000.0,
            rect.top() + rect.height() * keys.y_ppm.min(1000) as f32 / 1000.0,
        ),
    };
    let (sin, cos) = angle.sin_cos();
    let half_w = (cos.abs() * row_w + sin.abs() * row_h) / 2.0 + 18.0 * unit;
    let half_h = (sin.abs() * row_w + cos.abs() * row_h) / 2.0 + 18.0 * unit;
    center.x = center.x.clamp(
        rect.left() + half_w.min(rect.width() / 2.0),
        rect.right() - half_w.min(rect.width() / 2.0),
    );
    center.y = center.y.clamp(
        rect.top() + half_h.min(rect.height() / 2.0),
        rect.bottom() - half_h.min(rect.height() / 2.0),
    );
    center
}

fn draw_rotated_preview_key(
    ui: &mut egui::Ui,
    local: egui::Rect,
    center: egui::Pos2,
    angle: f32,
    opacity: f32,
    unit: f32,
) {
    let points = rotated_preview_rect(local, center, angle);
    let shadow_points: Vec<egui::Pos2> = points
        .iter()
        .map(|p| egui::pos2(p.x, p.y + 4.0 * unit))
        .collect();
    ui.painter().add(egui::Shape::convex_polygon(
        shadow_points,
        alpha(egui::Color32::BLACK, 0.36 * opacity),
        egui::Stroke::NONE,
    ));
    ui.painter().add(egui::Shape::convex_polygon(
        points,
        alpha(egui::Color32::from_rgb(63, 67, 70), opacity),
        egui::Stroke::new(1.0, alpha(egui::Color32::WHITE, 0.16 * opacity)),
    ));
}

fn rotated_preview_rect(local: egui::Rect, center: egui::Pos2, angle: f32) -> Vec<egui::Pos2> {
    vec![
        rotate_preview_point(local.left_top(), center, angle),
        rotate_preview_point(local.right_top(), center, angle),
        rotate_preview_point(local.right_bottom(), center, angle),
        rotate_preview_point(local.left_bottom(), center, angle),
    ]
}

fn rotate_preview_point(local: egui::Pos2, center: egui::Pos2, angle: f32) -> egui::Pos2 {
    let (sin, cos) = angle.sin_cos();
    egui::pos2(
        center.x + cos * local.x - sin * local.y,
        center.y + sin * local.x + cos * local.y,
    )
}

fn pressed_keys_preset_ppm(position: PressedKeysPosition) -> (u16, u16) {
    match position {
        PressedKeysPosition::BottomCenter => (500, 900),
        PressedKeysPosition::BottomLeft => (160, 900),
        PressedKeysPosition::BottomRight => (840, 900),
        PressedKeysPosition::TopCenter => (500, 100),
        PressedKeysPosition::Custom => (500, 900),
    }
}

fn alpha(color: egui::Color32, factor: f32) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(
        color.r(),
        color.g(),
        color.b(),
        (color.a() as f32 * factor.clamp(0.0, 1.0)).round() as u8,
    )
}

fn codec_label(c: CaptureCodec) -> &'static str {
    match c {
        CaptureCodec::H264 => "H.264 (compatible)",
        CaptureCodec::Hevc => "HEVC",
        CaptureCodec::Av1 => "AV1 (best compression)",
    }
}

fn pressed_keys_position_label(p: PressedKeysPosition) -> &'static str {
    match p {
        PressedKeysPosition::BottomCenter => "Bottom center",
        PressedKeysPosition::BottomLeft => "Bottom left",
        PressedKeysPosition::BottomRight => "Bottom right",
        PressedKeysPosition::TopCenter => "Top center",
        PressedKeysPosition::Custom => "Custom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressed_key_presets_map_to_normalized_centers() {
        assert_eq!(
            pressed_keys_preset_ppm(PressedKeysPosition::BottomCenter),
            (500, 900)
        );
        assert_eq!(
            pressed_keys_preset_ppm(PressedKeysPosition::BottomLeft),
            (160, 900)
        );
        assert_eq!(
            pressed_keys_preset_ppm(PressedKeysPosition::BottomRight),
            (840, 900)
        );
        assert_eq!(
            pressed_keys_preset_ppm(PressedKeysPosition::TopCenter),
            (500, 100)
        );
    }

    #[test]
    fn custom_position_has_label() {
        assert_eq!(
            pressed_keys_position_label(PressedKeysPosition::Custom),
            "Custom"
        );
    }
}
