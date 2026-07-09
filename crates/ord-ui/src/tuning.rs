//! Centralized runtime tuning knobs read from the environment. Keeping the
//! `ORD_*` overrides in one documented place — instead of scattered
//! `std::env::var` calls across the player, editor, and app — makes them
//! discoverable and consistent.
//!
//! | Variable | Effect |
//! |----------|--------|
//! | `ORD_RENDER=cpu` | Force the egui-texture preview path (default: GPU NV12 shader). |
//! | `ORD_DECODE=sw\|nvdec\|gl\|hw` | Force software or hardware decode (default: auto). |
//! | `ORD_DEBUG` | Enable the editor debug overlay + watchdog logging. |
//! | `ORD_OPEN=<path>` | Auto-open a clip in the editor on launch (dev aid). |
//! | `ORD_SETTINGS` | Open the Settings page on launch (dev/QA; skips the click). |
//! | `ORD_AUTOPLAY` | Start playback immediately when the editor opens (dev/QA). |
//! | `ORD_DEBUG_LOG=<path>` | Override the diagnostics log path (see [`crate::diag::log_path`]). |

/// Whether to use the GPU NV12 shader preview path. `ORD_RENDER=cpu` opts out to
/// the egui-texture path with a CPU colour-convert.
pub fn render_gl() -> bool {
    !matches!(std::env::var("ORD_RENDER").as_deref(), Ok("cpu"))
}

/// The decode preference from `ORD_DECODE` (empty string = auto): `sw` forces
/// software; `nvdec`/`gl`/`zerocopy`/`hw` force-or-warn hardware.
pub fn decode_pref() -> String {
    std::env::var("ORD_DECODE").unwrap_or_default()
}

/// Whether the editor debug overlay is enabled (`ORD_DEBUG` set to anything).
pub fn debug_overlay() -> bool {
    std::env::var("ORD_DEBUG").is_ok()
}

/// A clip path to auto-open in the editor on launch (`ORD_OPEN`), for dev/QA.
pub fn auto_open() -> Option<String> {
    std::env::var("ORD_OPEN").ok()
}

/// Open Settings on launch (`ORD_SETTINGS` set to anything). Used by wisp/QA so
/// the settings form can be inspected without relying on pointer hit-testing
/// against egui's (historically empty) AT-SPI tree.
pub fn auto_settings() -> bool {
    std::env::var("ORD_SETTINGS").is_ok()
}

/// Whether to start playback as soon as the editor opens (`ORD_AUTOPLAY`). A
/// dev/QA aid that exercises the full decode/clock/EOF path without driving
/// input (off by default).
pub fn autoplay() -> bool {
    std::env::var("ORD_AUTOPLAY").is_ok()
}
