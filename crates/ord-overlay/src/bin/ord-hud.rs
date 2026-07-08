//! `ord-hud` — subscribes to the open-recorder daemon and shows its events as a
//! click-through wlr-layer-shell HUD over everything (including fullscreen
//! games). Run it from a compositor `exec-once`.

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use ord_common::{socket_path, Event};
use ord_overlay::hud::Hud;
use ord_overlay::{apply, LayerShellOverlay, Overlay};

#[cfg(all(feature = "pressed-keys", target_os = "linux"))]
use ord_overlay::key_source::{PressedKeyMessage, PressedKeyReader};

/// One-shot fetch of the effective config (for `overlay.*`) on (re)connect.
/// Best-effort: an unreachable daemon just leaves the current HUD settings.
fn apply_overlay_config(hud: &mut Hud, path: &PathBuf) {
    let reply = ord_common::connect(path)
        .ok()
        .and_then(|mut c| c.request(&ord_common::Command::GetConfig).ok());
    if let Some(Event::Config { effective, .. }) = reply {
        hud.apply_overlay_config(&effective.overlay);
    }
}

/// Connect to the daemon and subscribe, returning a receiver of events. The
/// reader thread ends (dropping the sender) when the daemon disconnects.
fn subscribe(path: &PathBuf) -> Option<mpsc::Receiver<Event>> {
    let events = ord_common::connect(path).ok()?.subscribe().ok()?;
    let (tx, rx) = mpsc::channel::<Event>();
    // Read events on a background thread; the main thread owns the Wayland
    // connection and renders (Wayland client objects are not Send).
    std::thread::spawn(move || {
        for ev in events {
            if tx.send(ev).is_err() {
                break;
            }
        }
    });
    Some(rx)
}

fn idle_timeout(hud: &Hud, now_ms: u64) -> Duration {
    hud.next_expiry_ms()
        .map(|deadline| Duration::from_millis(deadline.saturating_sub(now_ms).min(1000)))
        .unwrap_or_else(|| Duration::from_secs(1))
}

#[cfg(all(feature = "pressed-keys", target_os = "linux"))]
fn sync_pressed_reader(
    hud: &mut Hud,
    reader: &mut Option<PressedKeyReader>,
    failed: &mut bool,
    _now_ms: u64,
) {
    if !hud.pressed_keys_enabled() {
        let _ = reader.take();
        *failed = false;
        return;
    }
    if reader.is_none() && !*failed {
        *reader = Some(PressedKeyReader::spawn());
    }
}

#[cfg(not(all(feature = "pressed-keys", target_os = "linux")))]
fn sync_pressed_reader(hud: &mut Hud, _reader: &mut (), failed: &mut bool, now_ms: u64) {
    if !hud.pressed_keys_enabled() {
        *failed = false;
        return;
    }
    if !*failed {
        hud.toast(
            ord_overlay::ToastKind::Error,
            "pressed keys require a Linux ord-hud built with the pressed-keys feature",
            now_ms,
        );
        *failed = true;
    }
}

#[cfg(all(feature = "pressed-keys", target_os = "linux"))]
fn drain_pressed_reader(
    hud: &mut Hud,
    reader: &mut Option<PressedKeyReader>,
    failed: &mut bool,
    now_ms: u64,
) -> bool {
    let mut dirty = false;
    let mut stop_reader = false;
    if let Some(reader) = reader.as_mut() {
        loop {
            match reader.try_recv() {
                Ok(PressedKeyMessage::Event(event)) => {
                    dirty |= hud.pressed_key_event(event, now_ms);
                }
                Ok(PressedKeyMessage::Error(message)) => {
                    hud.toast(ord_overlay::ToastKind::Error, message, now_ms);
                    dirty = true;
                    *failed = true;
                    stop_reader = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    stop_reader = true;
                    break;
                }
            }
        }
    }
    if stop_reader {
        let _ = reader.take();
    }
    dirty
}

#[cfg(not(all(feature = "pressed-keys", target_os = "linux")))]
fn drain_pressed_reader(
    _hud: &mut Hud,
    _reader: &mut (),
    _failed: &mut bool,
    _now_ms: u64,
) -> bool {
    false
}

fn main() {
    let path = socket_path();

    // The overlay is created once and persists for the whole process; only the
    // daemon connection is re-established. A missing overlay is fatal.
    let mut overlay = LayerShellOverlay::new();
    if let Err(e) = overlay.create() {
        eprintln!("ord-hud: overlay unavailable: {e}");
        std::process::exit(1);
    }

    let mut hud = Hud::default();
    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;
    #[cfg(all(feature = "pressed-keys", target_os = "linux"))]
    let mut key_reader: Option<PressedKeyReader> = None;
    #[cfg(not(all(feature = "pressed-keys", target_os = "linux")))]
    let mut key_reader = ();
    let mut key_reader_failed = false;

    // Outer loop: (re)connect to ordd and stream its events. The daemon restarts
    // on every rebuild, so the HUD must survive a dropped connection by
    // reconnecting rather than exiting (which systemd's on-failure would ignore).
    loop {
        let Some(rx) = subscribe(&path) else {
            // Daemon not up yet; show the offline indicator (grey dot) so the
            // user can see at a glance that nothing is being buffered, keep
            // the overlay alive, and retry shortly.
            hud.set_daemon_offline(true);
            let now = now_ms();
            let _ = drain_pressed_reader(&mut hud, &mut key_reader, &mut key_reader_failed, now);
            hud.tick(now);
            overlay.render(&hud, now_ms());
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        hud.set_daemon_offline(false);
        // The subscription only pushes config *changes*; fetch the current
        // overlay settings once per (re)connect so a restart-with-overrides
        // daemon is honored immediately.
        apply_overlay_config(&mut hud, &path);
        sync_pressed_reader(&mut hud, &mut key_reader, &mut key_reader_failed, now_ms());

        // Inner loop: render + drain events until the daemon disconnects. We only
        // repaint when something actually changed (an event arrived or a toast
        // expired) or while a toast is mid-animation. Crucially, a buffer-on but
        // no-toast session — i.e. *all of normal gameplay* — is NOT "animating":
        // the static buffer indicator needs no per-frame redraw, so we block on
        // the event channel and spend ~zero CPU instead of a 60fps invisible
        // clear+rasterize+commit+roundtrip over the fullscreen game.
        let mut dirty = true; // force an initial paint
        loop {
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(ev) => {
                        apply(&mut hud, &ev, now_ms());
                        sync_pressed_reader(
                            &mut hud,
                            &mut key_reader,
                            &mut key_reader_failed,
                            now_ms(),
                        );
                        dirty = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                // Show offline immediately; the outer loop reconnects.
                hud.set_daemon_offline(true);
                overlay.render(&hud, now_ms());
                break;
            }
            if drain_pressed_reader(&mut hud, &mut key_reader, &mut key_reader_failed, now_ms()) {
                dirty = true;
            }
            if hud.tick(now_ms()) {
                dirty = true; // a toast just expired -> repaint the new state once
            }
            let animating = hud.is_animating();
            if dirty || animating {
                overlay.render(&hud, now_ms());
                dirty = false;
            }
            if animating {
                // Smooth fade/slide while a toast is visible.
                std::thread::sleep(Duration::from_millis(16));
            } else {
                // Idle: block until the next event (instant toast) or a periodic
                // wake. No CPU spent while nothing is on screen.
                match rx.recv_timeout(idle_timeout(&hud, now_ms())) {
                    Ok(ev) => {
                        apply(&mut hud, &ev, now_ms());
                        sync_pressed_reader(
                            &mut hud,
                            &mut key_reader,
                            &mut key_reader_failed,
                            now_ms(),
                        );
                        dirty = true;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        }

        // Backoff before reconnecting to the (restarting) daemon — but keep
        // ticking + rendering so a toast that fired right before the restart
        // keeps animating instead of freezing on screen.
        let backoff_until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < backoff_until {
            let now = now_ms();
            let keys_changed =
                drain_pressed_reader(&mut hud, &mut key_reader, &mut key_reader_failed, now);
            let changed = hud.tick(now) || keys_changed;
            if changed || hud.is_animating() {
                overlay.render(&hud, now_ms());
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    }
}
