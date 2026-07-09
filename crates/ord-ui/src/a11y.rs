//! AccessKit helpers: stable names for interactive chrome so screen readers
//! and automation (wisp) can find controls that lack readable button text
//! (icon-only buttons, painted tracks, the video preview).
//!
//! Standard `ui.button("Label")` already exposes its text; call these only
//! where the painted control would otherwise be anonymous in the a11y tree.

use eframe::egui::{self, WidgetInfo, WidgetType};

/// Attach a button role + stable name to an existing response.
pub fn button(resp: &egui::Response, label: &str) {
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, resp.enabled(), label));
}

/// Attach a slider/progress-like role (timeline track, scrubbers).
pub fn slider(resp: &egui::Response, label: &str) {
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Slider, resp.enabled(), label));
}

/// Attach a text-input role (numeric time entry fields).
pub fn text_input(resp: &egui::Response, label: &str) {
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, resp.enabled(), label));
}

/// Attach a generic clickable label role (toggleable readouts).
pub fn clickable_label(resp: &egui::Response, label: &str) {
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Label, resp.enabled(), label));
}
