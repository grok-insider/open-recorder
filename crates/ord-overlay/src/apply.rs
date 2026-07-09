//! Pure daemon-event → HUD mapping. Lives in the library (not the `ord-hud`
//! binary) so the mapping every toast depends on is unit-tested.

use ord_common::Event;

use crate::hud::{Hud, ToastKind};

/// Map a daemon event onto a HUD update.
pub fn apply(hud: &mut Hud, event: &Event, now_ms: u64) {
    match event {
        Event::ClipSaved { duration, .. } => {
            hud.toast(
                ToastKind::Saved,
                format!("Clip saved ({}s)", duration.get()),
                now_ms,
            );
        }
        Event::BufferState { enabled } => {
            hud.set_buffer_active(*enabled);
            let kind = if *enabled {
                ToastKind::Recording
            } else {
                ToastKind::Stopped
            };
            let text = if *enabled {
                "Replay buffer on"
            } else {
                "Replay buffer off"
            };
            hud.toast(kind, text, now_ms);
        }
        Event::RecordState { recording, .. } => {
            let (kind, text) = if *recording {
                (ToastKind::Recording, "Recording started")
            } else {
                (ToastKind::Stopped, "Recording stopped")
            };
            hud.toast(kind, text, now_ms);
        }
        Event::Status { buffer_enabled, .. } => hud.set_buffer_active(*buffer_enabled),
        Event::Error { message } => hud.toast(ToastKind::Error, message.clone(), now_ms),
        Event::Marked { auto_saving } => {
            let text = if *auto_saving {
                "Marked — saving clip"
            } else {
                "Marked"
            };
            hud.toast(ToastKind::Marked, text, now_ms);
        }
        Event::CaptureRestarted => {
            hud.toast(ToastKind::Recording, "Capture recovered", now_ms);
        }
        Event::ScreenshotSaved { .. } => {
            hud.toast(ToastKind::Saved, "Screenshot saved", now_ms);
        }
        // Pushed on every settings apply: the overlay section governs us.
        Event::Config { effective, .. } => {
            hud.apply_overlay_config(&effective.overlay);
        }
        // Point-to-point probe reply; never broadcast to the HUD.
        Event::Outputs { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ord_common::{ClipDuration, Config};

    fn duration(secs: u32) -> ClipDuration {
        match ClipDuration::new(secs) {
            Some(d) => d,
            None => unreachable!("test durations are valid"),
        }
    }

    #[test]
    fn clip_saved_shows_saved_toast() {
        let mut hud = Hud::default();
        apply(
            &mut hud,
            &Event::ClipSaved {
                path: "/clips/game-1.mkv".into(),
                duration: duration(30),
            },
            100,
        );
        assert_eq!(hud.toasts().len(), 1);
        assert_eq!(hud.toasts()[0].kind, ToastKind::Saved);
        assert_eq!(hud.toasts()[0].text, "Clip saved (30s)");
    }

    #[test]
    fn record_state_maps_to_start_stop_toasts() {
        let mut hud = Hud::default();
        apply(
            &mut hud,
            &Event::RecordState {
                recording: true,
                path: Some("/clips/rec.mkv".into()),
            },
            0,
        );
        assert_eq!(hud.toasts()[0].kind, ToastKind::Recording);
        assert_eq!(hud.toasts()[0].text, "Recording started");
        apply(
            &mut hud,
            &Event::RecordState {
                recording: false,
                path: None,
            },
            10,
        );
        assert_eq!(hud.toasts()[1].kind, ToastKind::Stopped);
        assert_eq!(hud.toasts()[1].text, "Recording stopped");
    }

    #[test]
    fn buffer_state_arms_dot_and_toasts() {
        let mut hud = Hud::default();
        apply(&mut hud, &Event::BufferState { enabled: true }, 0);
        assert!(hud.buffer_active);
        assert_eq!(hud.toasts()[0].kind, ToastKind::Recording);
        assert_eq!(hud.toasts()[0].text, "Replay buffer on");
        apply(&mut hud, &Event::BufferState { enabled: false }, 10);
        assert!(!hud.buffer_active);
        assert_eq!(hud.toasts()[1].kind, ToastKind::Stopped);
        assert_eq!(hud.toasts()[1].text, "Replay buffer off");
    }

    #[test]
    fn status_snapshot_sets_buffer_without_toasting() {
        let mut hud = Hud::default();
        apply(
            &mut hud,
            &Event::Status {
                buffer_enabled: true,
                recording: false,
                buffered_seconds: 12,
                buffered_frames: 720,
                buffered_keyframes: 6,
            },
            0,
        );
        assert!(hud.buffer_active);
        assert!(hud.toasts().is_empty());
    }

    #[test]
    fn error_shows_error_toast() {
        let mut hud = Hud::default();
        apply(
            &mut hud,
            &Event::Error {
                message: "buffer is disabled".into(),
            },
            0,
        );
        assert_eq!(hud.toasts()[0].kind, ToastKind::Error);
        assert_eq!(hud.toasts()[0].text, "buffer is disabled");
    }

    #[test]
    fn config_governs_status_dot() {
        let mut hud = Hud::default();
        assert!(hud.show_status_dot);
        let mut effective = Config::default();
        effective.overlay.show_status_dot = false;
        apply(
            &mut hud,
            &Event::Config {
                effective: Box::new(effective),
                base: Box::new(Config::default()),
            },
            0,
        );
        assert!(!hud.show_status_dot);
        assert!(hud.toasts().is_empty());
    }

    #[test]
    fn config_governs_pressed_keys() {
        let mut hud = Hud::default();
        assert!(!hud.pressed_keys_enabled());
        let mut effective = Config::default();
        effective.overlay.pressed_keys.enabled = true;
        apply(
            &mut hud,
            &Event::Config {
                effective: Box::new(effective),
                base: Box::new(Config::default()),
            },
            0,
        );
        assert!(hud.pressed_keys_enabled());
    }

    #[test]
    fn marked_and_recovery_and_screenshot_toasts() {
        let mut hud = Hud::default();
        apply(&mut hud, &Event::Marked { auto_saving: true }, 0);
        assert_eq!(hud.toasts()[0].kind, ToastKind::Marked);
        assert_eq!(hud.toasts()[0].text, "Marked — saving clip");
        apply(&mut hud, &Event::Marked { auto_saving: false }, 10);
        assert_eq!(hud.toasts()[1].text, "Marked");
        apply(&mut hud, &Event::CaptureRestarted, 20);
        assert_eq!(hud.toasts()[2].kind, ToastKind::Recording);
        apply(
            &mut hud,
            &Event::ScreenshotSaved {
                path: "/x.png".into(),
            },
            30,
        );
        assert_eq!(hud.toasts()[3].kind, ToastKind::Saved);
        assert_eq!(hud.toasts()[3].text, "Screenshot saved");
    }
}
