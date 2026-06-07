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

pub mod hud;
#[cfg(feature = "layershell")]
pub mod layershell;

pub use hud::{Hud, Toast, ToastKind};
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
pub trait Overlay {
    /// Create the surface on the active output.
    fn create(&mut self) -> Result<(), OverlayError>;
    /// Show or hide without destroying the surface.
    fn set_visible(&mut self, visible: bool);
    /// Render the current HUD state (called each tick by the owner).
    fn render(&mut self, hud: &Hud);
    /// Tear down the surface.
    fn destroy(&mut self);
}

/// A no-op overlay: useful in headless tests and when no overlay backend is
/// available. Records calls so behavior can be asserted.
#[derive(Debug, Default)]
pub struct NoopOverlay {
    pub created: bool,
    pub visible: bool,
    pub renders: u32,
}

impl Overlay for NoopOverlay {
    fn create(&mut self) -> Result<(), OverlayError> {
        self.created = true;
        Ok(())
    }
    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }
    fn render(&mut self, _hud: &Hud) {
        self.renders += 1;
    }
    fn destroy(&mut self) {
        self.created = false;
        self.visible = false;
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
        o.set_visible(true);
        assert!(o.visible);
        let hud = Hud::default();
        o.render(&hud);
        o.render(&hud);
        assert_eq!(o.renders, 2);
        o.destroy();
        assert!(!o.created);
        assert!(!o.visible);
    }
}
