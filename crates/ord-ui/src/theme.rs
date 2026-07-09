//! The open-recorder design system (gui-only).
//!
//! Direction: Japanese corporate minimalism — *kanso* (austerity: ink-dark
//! greys, one restrained accent), *ma* (negative space: an 8-pt rhythm with
//! generous padding), and precision (hairline borders, small radii, quiet
//! states). Color is meaning, never decoration:
//!
//! * **Sumi scale** — five grey steps from window to hover; depth comes from
//!   value steps + hairlines, not shadows.
//! * **Shu (vermilion)** — exactly one warm accent, reserved for "recording /
//!   destructive / brand". Its visual weight is why it must stay rare.
//! * **Functional hues** — muted matcha green (ok/armed), kin gold (markers/
//!   warnings); both desaturated to sit quietly on dark grey.
//!
//! Every widget tint, spacing constant, and type size lives here so the
//! library, editor, and settings views cannot drift apart.

use eframe::egui::{self, Color32, Rounding, Stroke};

// ---- Sumi (ink) greys, darkest to lightest -------------------------------

/// Window background (near-black, slightly warm).
pub const BG: Color32 = Color32::from_rgb(12, 13, 16);
/// Panel / card surface.
pub const SURFACE: Color32 = Color32::from_rgb(19, 21, 25);
/// Raised / hovered surface.
pub const RAISED: Color32 = Color32::from_rgb(26, 29, 35);
/// Hairline borders.
pub const HAIRLINE: Color32 = Color32::from_rgb(38, 41, 48);
/// Hairline on hover / focus.
pub const HAIRLINE_HI: Color32 = Color32::from_rgb(62, 67, 78);

// ---- Ink (text) ----------------------------------------------------------

/// Primary text.
pub const INK: Color32 = Color32::from_rgb(229, 231, 235);
/// Secondary text (metadata, labels).
pub const INK_2: Color32 = Color32::from_rgb(148, 153, 163);
/// Tertiary text (hints, disabled).
pub const INK_3: Color32 = Color32::from_rgb(92, 97, 106);

// ---- Accents (sparingly) -------------------------------------------------

/// Shu vermilion: recording, destructive, the brand mark.
pub const SHU: Color32 = Color32::from_rgb(214, 93, 72);
/// Matcha green: ok / buffer armed.
pub const OK: Color32 = Color32::from_rgb(125, 169, 130);
/// Kin gold: markers / warnings.
pub const KIN: Color32 = Color32::from_rgb(201, 162, 75);
/// Quiet indigo for selection/focus fills.
pub const AI: Color32 = Color32::from_rgb(94, 111, 163);

// ---- Editor / grid surfaces ------------------------------------------------

/// Thumbnail placeholder background (a step below the window BG so the empty
/// frame reads as a screen, not a hole).
pub const THUMB_BG: Color32 = Color32::from_rgb(10, 11, 13);
/// Timeline playhead line + head.
pub const PLAYHEAD: Color32 = Color32::WHITE;
/// Text painted on accent fills and dark bubbles (trim-handle labels, the
/// hover time bubble).
pub const ON_ACCENT: Color32 = Color32::WHITE;
/// Filmstrip tile tint (dimmed so overlays stay readable on top).
pub const FILMSTRIP_TINT: Color32 = Color32::from_gray(150);
/// Timeline ruler tick + label color.
pub const RULER_TEXT: Color32 = Color32::from_gray(120);
/// Dim over the timeline outside the in/out selection.
pub const SCRIM: Color32 = Color32::from_black_alpha(130);
/// Heavy dim over cut-out timeline pieces.
pub const SCRIM_CUT: Color32 = Color32::from_black_alpha(190);
/// Small label plate on cut-out pieces.
pub const SCRIM_LABEL: Color32 = Color32::from_black_alpha(170);
/// Hover time-bubble background.
pub const BUBBLE_BG: Color32 = Color32::from_black_alpha(200);
/// Audio waveform fill under the filmstrip (muted indigo so it stays secondary
/// to the vermilion trim handles and gold markers).
pub const WAVEFORM: Color32 = Color32::from_rgb(94, 111, 163);

/// Faint white lift over the hovered timeline piece (compositing, so it can't
/// be a const — `from_white_alpha` is gamma-corrected at runtime).
pub fn hover_lift() -> Color32 {
    Color32::from_white_alpha(5)
}

/// Ghost line under the timeline pointer.
pub fn ghost_line() -> Color32 {
    Color32::from_white_alpha(70)
}

// ---- Rhythm ----------------------------------------------------------------

/// 8-pt spacing scale.
pub const SP_1: f32 = 4.0;
pub const SP_2: f32 = 8.0;
pub const SP_3: f32 = 12.0;
pub const SP_4: f32 = 16.0;
pub const SP_6: f32 = 24.0;

/// Corner radius: small and precise (corporate, not bubbly).
pub const RADIUS: f32 = 4.0;
/// Cards get one step more.
pub const RADIUS_CARD: f32 = 6.0;

// ---- Type scale ------------------------------------------------------------

pub const TEXT_TITLE: f32 = 19.0;
pub const TEXT_BODY: f32 = 13.5;
pub const TEXT_LABEL: f32 = 12.0;
pub const TEXT_MICRO: f32 = 11.0;

/// Install the theme on the egui context (idempotent; call once at startup).
pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = Color32::from_rgb(9, 10, 12);
    v.faint_bg_color = SURFACE;
    v.override_text_color = Some(INK);
    v.hyperlink_color = AI;

    v.selection.bg_fill = AI.linear_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, AI);

    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, HAIRLINE);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK_2);

    v.widgets.inactive.bg_fill = RAISED;
    v.widgets.inactive.weak_bg_fill = SURFACE;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, HAIRLINE);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, INK);
    v.widgets.inactive.rounding = Rounding::same(RADIUS);

    v.widgets.hovered.bg_fill = RAISED;
    v.widgets.hovered.weak_bg_fill = RAISED;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, HAIRLINE_HI);
    v.widgets.hovered.fg_stroke = Stroke::new(1.2, INK);
    v.widgets.hovered.rounding = Rounding::same(RADIUS);

    v.widgets.active.bg_fill = AI.linear_multiply(0.45);
    v.widgets.active.weak_bg_fill = AI.linear_multiply(0.30);
    v.widgets.active.bg_stroke = Stroke::new(1.0, AI);
    v.widgets.active.fg_stroke = Stroke::new(1.2, INK);
    v.widgets.active.rounding = Rounding::same(RADIUS);

    v.widgets.open.bg_fill = RAISED;
    v.widgets.open.bg_stroke = Stroke::new(1.0, HAIRLINE_HI);

    v.popup_shadow = egui::epaint::Shadow::NONE;
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.window_stroke = Stroke::new(1.0, HAIRLINE);
    v.window_rounding = Rounding::same(RADIUS_CARD);
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(SP_2, SP_2);
    style.spacing.button_padding = egui::vec2(SP_3, 5.0);
    style.spacing.menu_margin = egui::Margin::same(SP_2);
    style.spacing.window_margin = egui::Margin::same(SP_4);
    ctx.set_style(style);
}

/// A clip/settings card: flat surface, hairline border, quiet radius.
pub fn card() -> egui::Frame {
    egui::Frame::none()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, HAIRLINE))
        .rounding(RADIUS_CARD)
        .inner_margin(egui::Margin::same(SP_3))
}

/// A clip card carrying the keyboard-focus ring.
pub fn card_focused() -> egui::Frame {
    card().stroke(Stroke::new(1.5, AI))
}

/// The header/footer chrome strip.
pub fn chrome() -> egui::Frame {
    egui::Frame::none()
        .fill(BG)
        .inner_margin(egui::Margin::symmetric(SP_4, 10.0))
}

/// Section heading used by the settings page: a quiet small-caps label with a
/// hairline rule filling the rest of the row (a *noren* divider).
pub fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(SP_4);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(title.to_uppercase())
                .size(TEXT_MICRO)
                .color(INK_3)
                .strong(),
        );
        let line = ui.available_rect_before_wrap();
        let y = line.center().y;
        ui.painter().hline(
            (line.left() + SP_2)..=line.right(),
            y,
            Stroke::new(1.0, HAIRLINE),
        );
        ui.allocate_space(ui.available_size());
    });
    ui.add_space(SP_2);
}

/// A small status dot + label, e.g. the daemon badge.
pub fn status_dot(ui: &mut egui::Ui, color: Color32, text: &str, hover: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.2, color);
        ui.label(egui::RichText::new(text).size(TEXT_LABEL).color(INK_2))
            .on_hover_text(hover);
    });
}

/// The brand mark: a shu square + wordmark. Quiet, top-left, once.
pub fn brand(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
        ui.painter().rect_filled(rect.shrink(1.0), 3.0, SHU);
        ui.label(
            egui::RichText::new("open-recorder")
                .size(15.0)
                .strong()
                .color(INK),
        );
    });
}

/// A primary (filled) action button.
pub fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(TEXT_BODY).color(INK))
            .fill(AI.linear_multiply(0.55))
            .stroke(Stroke::new(1.0, AI)),
    )
}

/// A destructive (vermilion) action button.
pub fn danger_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(TEXT_BODY).color(INK))
            .fill(SHU.linear_multiply(0.35))
            .stroke(Stroke::new(1.0, SHU)),
    )
}
