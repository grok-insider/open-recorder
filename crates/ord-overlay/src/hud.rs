//! The HUD model: a small set of transient toasts plus a persistent
//! buffer-active indicator. Pure and time-driven (the owner supplies a monotonic
//! millisecond clock), so the lifecycle is deterministic and fully testable.

use ord_common::config::{OverlayConfig, PressedKeysConfig, PressedKeysPosition};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hud {
    pub buffer_active: bool,
    /// The daemon is unreachable: the renderer shows a distinct hollow/grey
    /// indicator so "not recording because ordd is down" is visible at a
    /// glance instead of looking identical to "buffer off". (The silent-death
    /// failure mode every incumbent recorder is hated for.)
    pub daemon_offline: bool,
    /// Whether the persistent corner dot is drawn at all
    /// (`overlay.show_status_dot` in the config). Toasts are unaffected.
    pub show_status_dot: bool,
    toasts: Vec<Toast>,
    pressed_keys: PressedKeys,
}

impl Default for Hud {
    fn default() -> Self {
        Self {
            buffer_active: false,
            daemon_offline: false,
            show_status_dot: true,
            toasts: Vec::new(),
            pressed_keys: PressedKeys::default(),
        }
    }
}

/// Default toast lifetime in milliseconds.
pub const DEFAULT_TOAST_MS: u64 = 2500;

/// A raw keyboard transition from the Linux input stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressedKeyEvent {
    pub code: u32,
    pub pressed: bool,
}

/// The visible pressed-key keycap state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PressedKeys {
    enabled: bool,
    position: PressedKeysPosition,
    x_ppm: u16,
    y_ppm: u16,
    scale_percent: u16,
    opacity_percent: u8,
    rotation_degrees: i16,
    timeout_ms: u64,
    max_keys: usize,
    down: Vec<PressedKey>,
    labels: Vec<String>,
    expires_at_ms: Option<u64>,
}

/// Placement and visual transform for the pressed-key layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressedKeysLayout {
    pub position: PressedKeysPosition,
    pub x_ppm: u16,
    pub y_ppm: u16,
    pub scale_percent: u16,
    pub opacity_percent: u8,
    pub rotation_degrees: i16,
}

impl Default for PressedKeys {
    fn default() -> Self {
        let cfg = PressedKeysConfig::default();
        Self {
            enabled: cfg.enabled,
            position: cfg.position,
            x_ppm: cfg.x_ppm,
            y_ppm: cfg.y_ppm,
            scale_percent: cfg.scale_percent,
            opacity_percent: cfg.opacity_percent,
            rotation_degrees: cfg.rotation_degrees,
            timeout_ms: cfg.timeout_ms as u64,
            max_keys: cfg.max_keys as usize,
            down: Vec::new(),
            labels: Vec::new(),
            expires_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PressedKey {
    code: u32,
    label: String,
    order: u64,
}

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

    /// Show or hide the persistent status dot (`overlay.show_status_dot`).
    pub fn set_show_status_dot(&mut self, show: bool) {
        self.show_status_dot = show;
    }

    /// Apply all live overlay settings. Disabling pressed-key capture clears any
    /// visible key state immediately so stale chords are not left on screen.
    pub fn apply_overlay_config(&mut self, overlay: &OverlayConfig) {
        self.set_show_status_dot(overlay.show_status_dot);
        self.pressed_keys.apply_config(&overlay.pressed_keys);
    }

    /// Whether the renderer should draw the persistent corner dot this frame.
    pub fn status_dot_visible(&self) -> bool {
        self.show_status_dot && (self.buffer_active || self.daemon_offline)
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
        let toasts_changed = self.toasts.len() != before;
        let keys_changed = self.pressed_keys.tick(now_ms);
        toasts_changed || keys_changed
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
        self.status_dot_visible() || !self.toasts.is_empty() || self.pressed_keys.visible()
    }

    /// Process one raw keyboard transition. Returns true when the visible
    /// keycaps changed and the overlay should repaint.
    pub fn pressed_key_event(&mut self, event: PressedKeyEvent, now_ms: u64) -> bool {
        self.pressed_keys.event(event, now_ms)
    }

    /// Whether pressed-key display is currently enabled.
    pub fn pressed_keys_enabled(&self) -> bool {
        self.pressed_keys.enabled
    }

    /// Visible pressed-key labels, ordered for display.
    pub fn pressed_key_labels(&self) -> &[String] {
        &self.pressed_keys.labels
    }

    /// Placement preset for the pressed-key keycaps.
    pub fn pressed_keys_position(&self) -> PressedKeysPosition {
        self.pressed_keys.position
    }

    /// Layout transform for the pressed-key keycaps.
    pub fn pressed_keys_layout(&self) -> PressedKeysLayout {
        self.pressed_keys.layout()
    }

    /// Next non-animated deadline that should wake the HUD loop.
    pub fn next_expiry_ms(&self) -> Option<u64> {
        self.toasts
            .iter()
            .map(|t| t.expires_at_ms)
            .chain(self.pressed_keys.expires_at_ms)
            .min()
    }
}

impl PressedKeys {
    fn apply_config(&mut self, cfg: &PressedKeysConfig) {
        self.enabled = cfg.enabled;
        self.position = cfg.position;
        self.x_ppm = cfg.x_ppm;
        self.y_ppm = cfg.y_ppm;
        self.scale_percent = cfg.scale_percent;
        self.opacity_percent = cfg.opacity_percent;
        self.rotation_degrees = cfg.rotation_degrees;
        self.timeout_ms = cfg.timeout_ms as u64;
        self.max_keys = cfg.max_keys as usize;
        if !self.enabled {
            self.clear();
        }
    }

    fn layout(&self) -> PressedKeysLayout {
        PressedKeysLayout {
            position: self.position,
            x_ppm: self.x_ppm,
            y_ppm: self.y_ppm,
            scale_percent: self.scale_percent,
            opacity_percent: self.opacity_percent,
            rotation_degrees: self.rotation_degrees,
        }
    }

    fn event(&mut self, event: PressedKeyEvent, now_ms: u64) -> bool {
        if !self.enabled {
            return false;
        }
        if event.pressed {
            if self.down.iter().any(|k| k.code == event.code) {
                return false;
            }
            self.down.push(PressedKey {
                code: event.code,
                label: key_label(event.code).to_string(),
                order: now_ms
                    .saturating_mul(256)
                    .saturating_add(self.down.len() as u64),
            });
            self.labels = ordered_labels(&self.down, self.max_keys);
            self.expires_at_ms = Some(now_ms.saturating_add(self.timeout_ms));
            return true;
        }

        let before = self.down.len();
        self.down.retain(|k| k.code != event.code);
        if before == self.down.len() {
            return false;
        }
        if !self.labels.is_empty() {
            self.expires_at_ms = Some(now_ms.saturating_add(self.timeout_ms));
            true
        } else {
            false
        }
    }

    fn tick(&mut self, now_ms: u64) -> bool {
        if self
            .expires_at_ms
            .is_some_and(|expires| expires <= now_ms && !self.labels.is_empty())
        {
            self.labels.clear();
            self.expires_at_ms = None;
            return true;
        }
        false
    }

    fn visible(&self) -> bool {
        !self.labels.is_empty()
    }

    fn clear(&mut self) {
        self.down.clear();
        self.labels.clear();
        self.expires_at_ms = None;
    }
}

fn ordered_labels(keys: &[PressedKey], max_keys: usize) -> Vec<String> {
    let mut ordered: Vec<&PressedKey> = keys.iter().collect();
    ordered.sort_by_key(|k| (modifier_rank(&k.label), k.order));
    let mut labels = Vec::new();
    for key in ordered {
        if labels.iter().any(|label| label == &key.label) {
            continue;
        }
        labels.push(key.label.clone());
        if labels.len() >= max_keys {
            break;
        }
    }
    labels
}

fn modifier_rank(label: &str) -> u8 {
    match label {
        "Ctrl" => 0,
        "Alt" => 1,
        "Shift" => 2,
        "Meta" => 3,
        _ => 4,
    }
}

fn key_label(code: u32) -> &'static str {
    match code {
        1 => "Esc",
        2 => "1",
        3 => "2",
        4 => "3",
        5 => "4",
        6 => "5",
        7 => "6",
        8 => "7",
        9 => "8",
        10 => "9",
        11 => "0",
        12 => "-",
        13 => "=",
        14 => "Backspace",
        15 => "Tab",
        16 => "Q",
        17 => "W",
        18 => "E",
        19 => "R",
        20 => "T",
        21 => "Y",
        22 => "U",
        23 => "I",
        24 => "O",
        25 => "P",
        26 => "[",
        27 => "]",
        28 => "Enter",
        29 | 97 => "Ctrl",
        30 => "A",
        31 => "S",
        32 => "D",
        33 => "F",
        34 => "G",
        35 => "H",
        36 => "J",
        37 => "K",
        38 => "L",
        39 => ";",
        40 => "'",
        41 => "`",
        42 | 54 => "Shift",
        43 => "\\",
        44 => "Z",
        45 => "X",
        46 => "C",
        47 => "V",
        48 => "B",
        49 => "N",
        50 => "M",
        51 => ",",
        52 => ".",
        53 => "/",
        55 => "Num *",
        56 | 100 => "Alt",
        57 => "Space",
        58 => "Caps",
        59 => "F1",
        60 => "F2",
        61 => "F3",
        62 => "F4",
        63 => "F5",
        64 => "F6",
        65 => "F7",
        66 => "F8",
        67 => "F9",
        68 => "F10",
        69 => "Num",
        70 => "Scroll",
        71 => "Num 7",
        72 => "Num 8",
        73 => "Num 9",
        74 => "Num -",
        75 => "Num 4",
        76 => "Num 5",
        77 => "Num 6",
        78 => "Num +",
        79 => "Num 1",
        80 => "Num 2",
        81 => "Num 3",
        82 => "Num 0",
        83 => "Num .",
        87 => "F11",
        88 => "F12",
        98 => "Num /",
        99 => "Print",
        102 => "Home",
        103 => "Up",
        104 => "PgUp",
        105 => "Left",
        106 => "Right",
        107 => "End",
        108 => "Down",
        109 => "PgDn",
        110 => "Insert",
        111 => "Delete",
        119 => "Pause",
        125 | 126 => "Meta",
        127 => "Menu",
        183 => "F13",
        184 => "F14",
        185 => "F15",
        186 => "F16",
        187 => "F17",
        188 => "F18",
        189 => "F19",
        190 => "F20",
        191 => "F21",
        192 => "F22",
        193 => "F23",
        194 => "F24",
        _ => "Key",
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
    fn hidden_status_dot_is_not_content() {
        let mut hud = Hud::default();
        hud.set_buffer_active(true);
        assert!(hud.status_dot_visible());
        hud.set_show_status_dot(false);
        assert!(!hud.status_dot_visible());
        assert!(!hud.has_content(), "hidden dot must not force draws");
        // Toasts still show even with the dot hidden.
        hud.toast(ToastKind::Saved, "saved", 0);
        assert!(hud.has_content());
        // Offline indicator is also governed by the toggle.
        hud.tick(DEFAULT_TOAST_MS);
        hud.set_daemon_offline(true);
        assert!(!hud.status_dot_visible());
        hud.set_show_status_dot(true);
        assert!(hud.status_dot_visible());
    }

    #[test]
    fn ttl_saturates_without_overflow() {
        let mut hud = Hud::default();
        hud.toast_for(ToastKind::Saved, "x", u64::MAX - 1, 1000);
        // Should not panic; expiry saturates at u64::MAX.
        assert_eq!(hud.toasts()[0].expires_at_ms, u64::MAX);
    }

    fn enable_keys(hud: &mut Hud) {
        let mut cfg = OverlayConfig::default();
        cfg.pressed_keys.enabled = true;
        hud.apply_overlay_config(&cfg);
    }

    #[test]
    fn pressed_keys_are_hidden_by_default() {
        let mut hud = Hud::default();
        assert!(!hud.pressed_keys_enabled());
        assert!(!hud.pressed_key_event(
            PressedKeyEvent {
                code: 30,
                pressed: true,
            },
            0,
        ));
        assert!(hud.pressed_key_labels().is_empty());
    }

    #[test]
    fn pressed_keys_show_modifier_first_and_ignore_repeat() {
        let mut hud = Hud::default();
        enable_keys(&mut hud);
        assert!(hud.pressed_key_event(
            PressedKeyEvent {
                code: 19,
                pressed: true,
            },
            10,
        ));
        assert_eq!(hud.pressed_key_labels(), &["R".to_string()]);
        assert!(hud.pressed_key_event(
            PressedKeyEvent {
                code: 29,
                pressed: true,
            },
            20,
        ));
        assert_eq!(
            hud.pressed_key_labels(),
            &["Ctrl".to_string(), "R".to_string()]
        );
        assert!(!hud.pressed_key_event(
            PressedKeyEvent {
                code: 29,
                pressed: true,
            },
            30,
        ));
    }

    #[test]
    fn pressed_keys_keep_last_chord_until_timeout() {
        let mut hud = Hud::default();
        enable_keys(&mut hud);
        hud.pressed_key_event(
            PressedKeyEvent {
                code: 29,
                pressed: true,
            },
            0,
        );
        hud.pressed_key_event(
            PressedKeyEvent {
                code: 19,
                pressed: true,
            },
            10,
        );
        hud.pressed_key_event(
            PressedKeyEvent {
                code: 19,
                pressed: false,
            },
            20,
        );
        assert_eq!(
            hud.pressed_key_labels(),
            &["Ctrl".to_string(), "R".to_string()]
        );
        assert!(hud.has_content());
        assert!(!hud.tick(919));
        assert!(hud.tick(920));
        assert!(hud.pressed_key_labels().is_empty());
    }

    #[test]
    fn pressed_key_config_caps_and_disables() {
        let mut hud = Hud::default();
        let mut cfg = OverlayConfig::default();
        cfg.pressed_keys.enabled = true;
        cfg.pressed_keys.max_keys = 2;
        cfg.pressed_keys.position = PressedKeysPosition::Custom;
        cfg.pressed_keys.x_ppm = 420;
        cfg.pressed_keys.y_ppm = 760;
        cfg.pressed_keys.scale_percent = 135;
        cfg.pressed_keys.opacity_percent = 86;
        cfg.pressed_keys.rotation_degrees = -8;
        hud.apply_overlay_config(&cfg);
        for (idx, code) in [29, 42, 56, 19].into_iter().enumerate() {
            hud.pressed_key_event(
                PressedKeyEvent {
                    code,
                    pressed: true,
                },
                idx as u64,
            );
        }
        assert_eq!(hud.pressed_keys_position(), PressedKeysPosition::Custom);
        assert_eq!(
            hud.pressed_keys_layout(),
            PressedKeysLayout {
                position: PressedKeysPosition::Custom,
                x_ppm: 420,
                y_ppm: 760,
                scale_percent: 135,
                opacity_percent: 86,
                rotation_degrees: -8,
            }
        );
        assert_eq!(
            hud.pressed_key_labels(),
            &["Ctrl".to_string(), "Alt".to_string()]
        );
        cfg.pressed_keys.enabled = false;
        hud.apply_overlay_config(&cfg);
        assert!(!hud.pressed_keys_enabled());
        assert!(hud.pressed_key_labels().is_empty());
    }
}
