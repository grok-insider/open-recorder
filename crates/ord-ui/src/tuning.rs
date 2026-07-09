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
//! | `ORD_OPEN=<path>` | Auto-open a clip in the editor on launch (dev/QA). |
//! | `ORD_SETTINGS` | Open the Settings page on launch (dev/QA; skips the click). |
//! | `ORD_AUTOPLAY` | Start playback immediately when the editor opens (dev/QA). |
//! | `ORD_A11Y` | Force-enable the session AT-SPI `IsEnabled` flag so AccessKit publishes a tree (needed for wisp marks when no screen reader is running). |
//! | `ORD_DEBUG_LOG=<path>` | Override the diagnostics log path (see [`crate::diag::log_path`]). |
//!
//! ## Wisp / nested sandbox QA
//!
//! AccessKit on Linux only **publishes** the AT-SPI tree after the session bus
//! property `org.a11y.Status.IsEnabled` is true (a screen reader normally sets
//! this). Without that, wisp reports `atspi: no elements` even though the
//! binary is built with AccessKit. QA launches should set `ORD_A11Y=1` (or any
//! of the entry-point vars below, which also flip the flag via `busctl`):
//!
//! ```text
//! ORD_A11Y=1 ORD_SETTINGS=1 ord-ui          # Settings + live AT-SPI tree
//! ORD_A11Y=1 ORD_OPEN=/path/to/clip.mkv ord-ui
//! ORD_OPEN=… ORD_AUTOPLAY=1 ORD_A11Y=1 ord-ui
//! ```
//!
//! Nested sandboxes still share the user a11y bus; once `IsEnabled` is on,
//! AccessKit names (Back, Play, Timeline, Apply, profiles, …) become wisp marks.

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
/// against an inactive AccessKit tree.
pub fn auto_settings() -> bool {
    std::env::var("ORD_SETTINGS").is_ok()
}

/// Whether to start playback as soon as the editor opens (`ORD_AUTOPLAY`). A
/// dev/QA aid that exercises the full decode/clock/EOF path without driving
/// input (off by default).
pub fn autoplay() -> bool {
    std::env::var("ORD_AUTOPLAY").is_ok()
}

/// Whether QA env vars ask us to force-enable the AT-SPI session flag so
/// AccessKit publishes a tree (no screen reader required).
pub fn force_a11y() -> bool {
    std::env::var("ORD_A11Y").is_ok() || auto_settings() || auto_open().is_some() || autoplay()
}

/// Flip `org.a11y.Status.IsEnabled` on the session bus when QA env vars are
/// set. AccessKit stays inactive until this is true; without it, wisp sees an
/// empty AT-SPI tree. Best-effort (`busctl` missing is a no-op).
pub fn ensure_a11y_bus() {
    if !force_a11y() {
        return;
    }
    let _ = std::process::Command::new("busctl")
        .args([
            "--user",
            "set-property",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Status",
            "IsEnabled",
            "b",
            "true",
        ])
        .status();
}
