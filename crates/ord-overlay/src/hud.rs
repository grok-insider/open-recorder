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
    toasts: Vec<Toast>,
}

/// Default toast lifetime in milliseconds.
pub const DEFAULT_TOAST_MS: u64 = 2500;

impl Hud {
    /// Set the persistent "replay buffer active" indicator.
    pub fn set_buffer_active(&mut self, active: bool) {
        self.buffer_active = active;
    }

    /// Push a toast that expires `DEFAULT_TOAST_MS` after `now_ms`.
    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>, now_ms: u64) {
        self.toast_for(kind, text, now_ms, DEFAULT_TOAST_MS);
    }

    /// Push a toast with an explicit lifetime.
    pub fn toast_for(
        &mut self,
        kind: ToastKind,
        text: impl Into<String>,
        now_ms: u64,
        ttl_ms: u64,
    ) {
        self.toasts.push(Toast {
            kind,
            text: text.into(),
            created_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(ttl_ms),
        });
    }

    /// Drop toasts whose expiry is at or before `now_ms`. Call each tick.
    pub fn tick(&mut self, now_ms: u64) {
        self.toasts.retain(|t| t.expires_at_ms > now_ms);
    }

    /// Currently-visible toasts (oldest first).
    pub fn toasts(&self) -> &[Toast] {
        &self.toasts
    }

    /// Whether there is anything to draw (indicator or toasts).
    pub fn has_content(&self) -> bool {
        self.buffer_active || !self.toasts.is_empty()
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
    fn ttl_saturates_without_overflow() {
        let mut hud = Hud::default();
        hud.toast_for(ToastKind::Saved, "x", u64::MAX - 1, 1000);
        // Should not panic; expiry saturates at u64::MAX.
        assert_eq!(hud.toasts()[0].expires_at_ms, u64::MAX);
    }
}
