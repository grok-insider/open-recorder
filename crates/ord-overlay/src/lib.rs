//! HUD overlay abstraction.
//!
//! The clip *library* window is a normal window (shown via a compositor special
//! workspace) and does NOT use this crate. This is only for the transient HUD —
//! "buffer active", "Clip saved!" — that must float over fullscreen games.
//!
//! Platform surfaces (wlr-layer-shell / X11 / Win32) implement [`Overlay`]. This
//! crate ships the trait, the pure [`hud`] toast-lifecycle state machine (fully
//! tested), and a [`NoopOverlay`] for headless/dev use. The real layer-shell
//! surface is added behind a feature in a later step.

pub mod apply;
pub mod hud;
#[cfg(all(feature = "pressed-keys", target_os = "linux"))]
pub mod key_source;
#[cfg(feature = "layershell")]
pub mod layershell;

pub use apply::apply;
pub use hud::{Hud, PressedKeyEvent, PressedKeysLayout, Toast, ToastKind};
#[cfg(feature = "layershell")]
pub use layershell::LayerShellOverlay;

/// Errors creating or driving an overlay surface.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("overlay surface creation failed: {0}")]
    Create(String),
    #[error("overlay is not supported on this platform/session")]
    Unsupported,
}

/// A transparent, always-on-top, click-through HUD surface.
///
/// Deliberately minimal: the HUD renders every state from [`Hud`], so the trait
/// is create/render/destroy. (A `set_visible` toggle was removed as speculative
/// API — an empty `Hud` renders nothing, which is "hidden".)
pub trait Overlay {
    /// Create the surface on the active output.
    fn create(&mut self) -> Result<(), OverlayError>;
    /// Render the current HUD state (called each tick by the owner). `now_ms` is
    /// the same monotonic clock the toasts were created with, so the renderer can
    /// drive fade/slide animations.
    fn render(&mut self, hud: &Hud, now_ms: u64);
    /// Tear down the surface.
    fn destroy(&mut self);
}

/// A no-op overlay: useful in headless tests and when no overlay backend is
/// available. Records calls so behavior can be asserted.
#[derive(Debug, Default)]
pub struct NoopOverlay {
    pub created: bool,
    pub renders: u32,
}

impl Overlay for NoopOverlay {
    fn create(&mut self) -> Result<(), OverlayError> {
        self.created = true;
        Ok(())
    }
    fn render(&mut self, _hud: &Hud, _now_ms: u64) {
        self.renders += 1;
    }
    fn destroy(&mut self) {
        self.created = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_overlay_tracks_calls() {
        let mut o = NoopOverlay::default();
        o.create().unwrap();
        assert!(o.created);
        let hud = Hud::default();
        o.render(&hud, 0);
        o.render(&hud, 16);
        assert_eq!(o.renders, 2);
        o.destroy();
        assert!(!o.created);
    }
}
