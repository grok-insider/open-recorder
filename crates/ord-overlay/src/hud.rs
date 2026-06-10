//! The HUD model: a small set of transient toasts plus a persistent
//! buffer-active indicator. Pure and time-driven (the owner supplies a monotonic
//! millisecond clock), so the lifecycle is deterministic and fully testable.

/// What a toast represents (drives its icon/color in the renderer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Saved,
    Recording,
    Stopped,
    Error,
    /// A marker ("clip that" bookmark) was placed.
    Marked,
}

/// A transient on-screen message with creation + expiry times (monotonic ms).
/// `created_at_ms` lets the renderer drive a fade/slide-in on appear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

/// HUD state: a persistent buffer indicator and a queue of timed toasts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Hud {
    pub buffer_active: bool,
    /// The daemon is unreachable: the renderer shows a distinct hollow/grey
    /// indicator so "not recording because ordd is down" is visible at a
    /// glance instead of looking identical to "buffer off". (The silent-death
    /// failure mode every incumbent recorder is hated for.)
    pub daemon_offline: bool,
    toasts: Vec<Toast>,
}

/// Default toast lifetime in milliseconds.
pub const DEFAULT_TOAST_MS: u64 = 2500;

impl Hud {
    /// Set the persistent "replay buffer active" indicator.
    pub fn set_buffer_active(&mut self, active: bool) {
        self.buffer_active = active;
    }

    /// Set the daemon-offline indicator. Going offline also clears the buffer
    /// indicator (we no longer know its state — claiming "armed" would be the
    /// silent-failure lie this HUD exists to prevent).
    pub fn set_daemon_offline(&mut self, offline: bool) {
        self.daemon_offline = offline;
        if offline {
            self.buffer_active = false;
        }
    }

    /// Push a toast that expires `DEFAULT_TOAST_MS` after `now_ms`.
    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>, now_ms: u64) {
        self.toast_for(kind, text, now_ms, DEFAULT_TOAST_MS);
    }

    /// Push a toast with an explicit lifetime. **Coalesces**: if the newest toast
    /// is the same kind and text and still on screen, its lifetime is simply
    /// extended (and re-animated) instead of stacking a duplicate card — so
    /// spamming the save key shows one refreshing toast, not five identical ones.
    pub fn toast_for(
        &mut self,
        kind: ToastKind,
        text: impl Into<String>,
        now_ms: u64,
        ttl_ms: u64,
    ) {
        let text = text.into();
        if let Some(last) = self.toasts.last_mut() {
            if last.kind == kind && last.text == text && last.expires_at_ms > now_ms {
                last.expires_at_ms = now_ms.saturating_add(ttl_ms);
                last.created_at_ms = now_ms;
                return;
            }
        }
        self.toasts.push(Toast {
            kind,
            text,
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(ttl_ms),
        });
    }

    /// Drop toasts whose expiry is at or before `now_ms`. Call each tick.
    /// Returns `true` if any toast was removed (i.e. the visible set changed and
    /// the renderer should repaint once more).
    pub fn tick(&mut self, now_ms: u64) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|t| t.expires_at_ms > now_ms);
        self.toasts.len() != before
    }

    /// Currently-visible toasts (oldest first).
    pub fn toasts(&self) -> &[Toast] {
        &self.toasts
    }

    /// Whether a transient toast is on screen, so the renderer should run its
    /// fade/slide animation at full frame rate. Distinct from [`has_content`]:
    /// the persistent buffer indicator is **static** and needs no per-frame
    /// redraw, so it must not by itself pin the HUD at 60fps.
    ///
    /// [`has_content`]: Hud::has_content
    pub fn is_animating(&self) -> bool {
        !self.toasts.is_empty()
    }

    /// Whether there is anything to draw (indicator or toasts).
    pub fn has_content(&self) -> bool {
        self.buffer_active || self.daemon_offline || !self.toasts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_expires_after_ttl() {
        let mut hud = Hud::default();
        hud.toast(ToastKind::Saved, "saved", 1000);
        assert_eq!(hud.toasts().len(), 1);
        // Just before expiry.
        hud.tick(1000 + DEFAULT_TOAST_MS - 1);
        assert_eq!(hud.toasts().len(), 1);
        // At expiry it's gone.
        hud.tick(1000 + DEFAULT_TOAST_MS);
        assert_eq!(hud.toasts().len(), 0);
    }

    #[test]
    fn multiple_toasts_expire_independently() {
        let mut hud = Hud::default();
        hud.toast_for(ToastKind::Saved, "a", 0, 100);
        hud.toast_for(ToastKind::Error, "b", 0, 300);
        hud.tick(150);
        assert_eq!(hud.toasts().len(), 1);
        assert_eq!(hud.toasts()[0].text, "b");
        hud.tick(300);
        assert_eq!(hud.toasts().len(), 0);
    }

    #[test]
    fn buffer_indicator_persists() {
        let mut hud = Hud::default();
        assert!(!hud.has_content());
        hud.set_buffer_active(true);
        assert!(hud.has_content());
        // Ticking doesn't clear the persistent indicator.
        hud.tick(1_000_000);
        assert!(hud.buffer_active);
        assert!(hud.has_content());
    }

    #[test]
    fn has_content_reflects_toasts() {
        let mut hud = Hud::default();
        hud.toast(ToastKind::Recording, "rec", 0);
        assert!(hud.has_content());
        hud.tick(DEFAULT_TOAST_MS);
        assert!(!hud.has_content());
    }

    #[test]
    fn tick_reports_change_and_animating() {
        let mut hud = Hud::default();
        assert!(!hud.is_animating());
        hud.toast_for(ToastKind::Saved, "a", 0, 100);
        assert!(hud.is_animating());
        assert!(!hud.tick(50)); // nothing expired yet -> no change
        assert!(hud.tick(100)); // toast expired -> changed
        assert!(!hud.is_animating());
        assert!(!hud.tick(200)); // empty -> no change
    }

    #[test]
    fn identical_toasts_coalesce() {
        let mut hud = Hud::default();
        hud.toast(ToastKind::Saved, "Clip saved (3s)", 0);
        hud.toast(ToastKind::Saved, "Clip saved (3s)", 500);
        hud.toast(ToastKind::Saved, "Clip saved (3s)", 1000);
        // All three coalesce into one refreshed card, not three stacked.
        assert_eq!(hud.toasts().len(), 1);
        assert_eq!(hud.toasts()[0].expires_at_ms, 1000 + DEFAULT_TOAST_MS);
    }

    #[test]
    fn different_toasts_do_not_coalesce() {
        let mut hud = Hud::default();
        hud.toast(ToastKind::Saved, "Clip saved (3s)", 0);
        hud.toast(ToastKind::Saved, "Clip saved (5s)", 100); // different text
        hud.toast(ToastKind::Error, "Clip saved (3s)", 200); // different kind
        assert_eq!(hud.toasts().len(), 3);
    }

    #[test]
    fn buffer_indicator_alone_is_not_animating() {
        // The persistent indicator is static: it must not force 60fps redraws.
        let mut hud = Hud::default();
        hud.set_buffer_active(true);
        assert!(hud.has_content());
        assert!(!hud.is_animating());
    }

    #[test]
    fn offline_clears_buffer_indicator_and_draws() {
        let mut hud = Hud::default();
        hud.set_buffer_active(true);
        hud.set_daemon_offline(true);
        assert!(!hud.buffer_active, "offline must not claim an armed buffer");
        assert!(hud.has_content(), "offline state itself is drawn");
        assert!(!hud.is_animating(), "offline indicator is static");
        hud.set_daemon_offline(false);
        assert!(!hud.has_content());
    }

    #[test]
    fn ttl_saturates_without_overflow() {
        let mut hud = Hud::default();
        hud.toast_for(ToastKind::Saved, "x", u64::MAX - 1, 1000);
        // Should not panic; expiry saturates at u64::MAX.
        assert_eq!(hud.toasts()[0].expires_at_ms, u64::MAX);
    }
}
