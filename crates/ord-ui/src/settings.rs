//! Settings panel model: a pure, fully-tested editing layer over the daemon's
//! layered configuration. The egui view renders this; the daemon applies it
//! (`SetConfig`). No I/O, no egui here.

use ord_common::Config;

/// Which apply tier a pending change lands in (drives the Apply button copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyTier {
    /// Nothing changed.
    None,
    /// Applies live (storage, hooks, markers, export, buffer length).
    Live,
    /// Restarts the capture session (encoder/audio fields) — a ~1 s gap.
    CaptureRestart,
}

/// The draft state of the settings panel.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsModel {
    /// The base layer (user/HM config file) — what "reset" returns to.
    pub base: Config,
    /// The effective config as last confirmed by the daemon.
    pub saved: Config,
    /// The config being edited.
    pub draft: Config,
}

impl SettingsModel {
    /// Build from a `GetConfig` reply.
    pub fn new(effective: Config, base: Config) -> Self {
        Self {
            saved: effective.clone(),
            draft: effective,
            base,
        }
    }

    /// Whether the draft differs from what the daemon is running.
    pub fn is_dirty(&self) -> bool {
        self.draft != self.saved
    }

    /// The tier the pending changes require.
    pub fn apply_tier(&self) -> ApplyTier {
        if !self.is_dirty() {
            return ApplyTier::None;
        }
        let (d, s) = (&self.draft.capture, &self.saved.capture);
        let encoder_changed = d.fps != s.fps
            || d.quality != s.quality
            || d.codec != s.codec
            || d.bitrate_kbps != s.bitrate_kbps
            || self.draft.audio != self.saved.audio;
        if encoder_changed {
            ApplyTier::CaptureRestart
        } else {
            ApplyTier::Live
        }
    }

    /// Validation problems in the draft, as user-facing messages. Empty = ok.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        let c = &self.draft.capture;
        if c.fps == 0 || c.fps > 240 {
            out.push("Frame rate must be between 1 and 240.".into());
        }
        if c.buffer_seconds == 0 || c.buffer_seconds > 3600 {
            out.push("Buffer length must be between 1 and 3600 seconds.".into());
        }
        if let Some(kbps) = c.bitrate_kbps {
            if !(1_000..=200_000).contains(&kbps) {
                out.push("Bitrate must be between 1,000 and 200,000 kbps.".into());
            }
        }
        if self.draft.storage.template.trim().is_empty() {
            out.push("Filename template cannot be empty.".into());
        }
        if let Some(secs) = self.draft.markers.auto_save_seconds {
            if secs == 0 {
                out.push("Auto-save on mark needs at least 1 second.".into());
            }
        }
        let keys = &self.draft.overlay.pressed_keys;
        if !(250..=5000).contains(&keys.timeout_ms) {
            out.push("Pressed-key visibility must be between 250 and 5000 ms.".into());
        }
        if !(1..=8).contains(&keys.max_keys) {
            out.push("Pressed-key max keys must be between 1 and 8.".into());
        }
        if keys.x_ppm > 1000 || keys.y_ppm > 1000 {
            out.push("Pressed-key position must stay inside the preview.".into());
        }
        if !(50..=250).contains(&keys.scale_percent) {
            out.push("Pressed-key size must be between 50% and 250%.".into());
        }
        if !(35..=100).contains(&keys.opacity_percent) {
            out.push("Pressed-key opacity must be between 35% and 100%.".into());
        }
        if !(-30..=30).contains(&keys.rotation_degrees) {
            out.push("Pressed-key rotation must be between -30 and 30 degrees.".into());
        }
        out
    }

    /// Dotted paths where the draft overrides the base config (shown as the
    /// "runtime override" markers in the panel).
    pub fn overridden(&self) -> Vec<String> {
        self.draft.overridden_fields(&self.base)
    }

    /// Whether one dotted field path currently carries an override.
    pub fn is_overridden(&self, path: &str) -> bool {
        self.overridden().iter().any(|p| p == path)
    }

    /// Throw away edits, back to the daemon's running config.
    pub fn revert(&mut self) {
        self.draft = self.saved.clone();
    }

    /// Reset the whole draft to the base layer (drops every runtime override;
    /// still needs an Apply to take effect).
    pub fn reset_to_base(&mut self) {
        self.draft = self.base.clone();
    }

    /// Record that the daemon accepted `effective` (an `Event::Config` reply).
    pub fn applied(&mut self, effective: Config, base: Config) {
        self.saved = effective.clone();
        self.draft = effective;
        self.base = base;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> SettingsModel {
        SettingsModel::new(Config::default(), Config::default())
    }

    #[test]
    fn clean_model_is_not_dirty() {
        let m = model();
        assert!(!m.is_dirty());
        assert_eq!(m.apply_tier(), ApplyTier::None);
        assert!(m.problems().is_empty());
        assert!(m.overridden().is_empty());
    }

    #[test]
    fn live_tier_for_storage_and_hooks() {
        let mut m = model();
        m.draft.storage.max_gib = Some(25);
        m.draft.hooks.on_clip_saved = Some("/bin/true".into());
        m.draft.overlay.pressed_keys.enabled = true;
        m.draft.overlay.pressed_keys.scale_percent = 135;
        m.draft.overlay.pressed_keys.rotation_degrees = -8;
        assert!(m.is_dirty());
        assert_eq!(m.apply_tier(), ApplyTier::Live);
    }

    #[test]
    fn buffer_resize_is_live_but_encoder_restarts() {
        let mut m = model();
        m.draft.capture.buffer_seconds = 90;
        assert_eq!(m.apply_tier(), ApplyTier::Live);
        m.draft.capture.fps = 120;
        assert_eq!(m.apply_tier(), ApplyTier::CaptureRestart);
    }

    #[test]
    fn audio_change_restarts_capture() {
        let mut m = model();
        m.draft.audio.mic = !m.draft.audio.mic;
        assert_eq!(m.apply_tier(), ApplyTier::CaptureRestart);
    }

    #[test]
    fn validation_catches_bad_values() {
        let mut m = model();
        m.draft.capture.fps = 0;
        m.draft.capture.bitrate_kbps = Some(50);
        m.draft.storage.template = "  ".into();
        m.draft.markers.auto_save_seconds = Some(0);
        m.draft.overlay.pressed_keys.timeout_ms = 100;
        m.draft.overlay.pressed_keys.max_keys = 0;
        m.draft.overlay.pressed_keys.x_ppm = 1200;
        m.draft.overlay.pressed_keys.scale_percent = 40;
        m.draft.overlay.pressed_keys.opacity_percent = 20;
        m.draft.overlay.pressed_keys.rotation_degrees = 45;
        let problems = m.problems();
        assert_eq!(problems.len(), 10, "{problems:?}");
    }

    #[test]
    fn revert_and_reset_to_base() {
        let mut base = Config::default();
        base.capture.fps = 30;
        let mut effective = base.clone();
        effective.capture.fps = 60; // a runtime override
        let mut m = SettingsModel::new(effective.clone(), base.clone());
        assert!(m.is_overridden("capture.fps"));

        m.draft.capture.fps = 120;
        m.revert();
        assert_eq!(m.draft, effective);

        m.reset_to_base();
        assert_eq!(m.draft.capture.fps, 30);
        assert!(!m.is_overridden("capture.fps"));
        // Resetting the draft is itself a pending (dirty) change.
        assert!(m.is_dirty());
    }

    #[test]
    fn applied_updates_all_layers() {
        let mut m = model();
        m.draft.capture.buffer_seconds = 45;
        let confirmed = m.draft.clone();
        m.applied(confirmed.clone(), m.base.clone());
        assert!(!m.is_dirty());
        assert_eq!(m.saved, confirmed);
    }
}
