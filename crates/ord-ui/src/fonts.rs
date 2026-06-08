//! Embedded UI fonts (gui-only).
//!
//! Installs IBM Plex Sans (proportional) and IBM Plex Mono (monospace) as the
//! primary families, with Noto Sans Symbols 2 as a fallback so media-control and
//! arrow glyphs (`⏮ ⏸ ⏭ ▶`) render instead of tofu boxes. egui's bundled fonts
//! stay as further fallbacks (covering `·`, emoji, etc.). Fonts are vendored
//! under `assets/fonts` — see `LICENSES.md`.

use eframe::egui;

const PLEX_SANS: &[u8] = include_bytes!("../assets/fonts/IBMPlexSans-Regular.ttf");
const PLEX_MONO: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf");
const SYMBOLS2: &[u8] = include_bytes!("../assets/fonts/NotoSansSymbols2-Regular.otf");

/// Register the embedded fonts and make them the default families. Idempotent;
/// call once at startup.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "plex-sans".to_owned(),
        egui::FontData::from_static(PLEX_SANS),
    );
    fonts.font_data.insert(
        "plex-mono".to_owned(),
        egui::FontData::from_static(PLEX_MONO),
    );
    fonts
        .font_data
        .insert("symbols2".to_owned(), egui::FontData::from_static(SYMBOLS2));

    // Proportional: Plex Sans, then symbol fallback, then egui's defaults.
    if let Some(prop) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        prop.insert(0, "plex-sans".to_owned());
        prop.insert(1, "symbols2".to_owned());
    }
    // Monospace: Plex Mono, then symbol fallback, then egui's defaults.
    if let Some(mono) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
        mono.insert(0, "plex-mono".to_owned());
        mono.insert(1, "symbols2".to_owned());
    }

    ctx.set_fonts(fonts);
}
