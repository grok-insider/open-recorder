//! `ord-hud` — subscribes to the open-recorder daemon and shows its events as a
//! click-through wlr-layer-shell HUD over everything (including fullscreen
//! games). Run it from a compositor `exec-once`.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use ord_common::{read_frame, write_frame, Command, Event};
use ord_overlay::hud::{Hud, ToastKind};
use ord_overlay::{LayerShellOverlay, Overlay};

fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join("open-recorder.sock")
}

/// Map a daemon event onto a HUD update.
fn apply(hud: &mut Hud, event: &Event, now_ms: u64) {
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
        Event::RecordState { recording } => {
            let (kind, text) = if *recording {
                (ToastKind::Recording, "Recording started")
            } else {
                (ToastKind::Stopped, "Recording stopped")
            };
            hud.toast(kind, text, now_ms);
        }
        Event::Status { buffer_enabled, .. } => hud.set_buffer_active(*buffer_enabled),
        Event::Error { message } => hud.toast(ToastKind::Error, message.clone(), now_ms),
    }
}

/// Connect to the daemon and subscribe, returning a receiver of events. The
/// reader thread ends (dropping the sender) when the daemon disconnects.
fn subscribe(path: &PathBuf) -> Option<mpsc::Receiver<Event>> {
    let mut stream = UnixStream::connect(path).ok()?;
    write_frame(&mut stream, &Command::Subscribe.encode().ok()?).ok()?;
    let (tx, rx) = mpsc::channel::<Event>();
    // Read events on a background thread; the main thread owns the Wayland
    // connection and renders (Wayland client objects are not Send).
    std::thread::spawn(move || {
        let mut s = stream;
        while let Ok(bytes) = read_frame(&mut s) {
            if let Ok(ev) = Event::decode(&bytes) {
                if tx.send(ev).is_err() {
                    break;
                }
            }
        }
    });
    Some(rx)
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

    // Outer loop: (re)connect to ordd and stream its events. The daemon restarts
    // on every rebuild, so the HUD must survive a dropped connection by
    // reconnecting rather than exiting (which systemd's on-failure would ignore).
    loop {
        let Some(rx) = subscribe(&path) else {
            // Daemon not up yet; keep the overlay alive and retry shortly.
            hud.tick(now_ms());
            overlay.render(&hud, now_ms());
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };

        // Inner loop: render + drain events until the daemon disconnects.
        loop {
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(ev) => apply(&mut hud, &ev, now_ms()),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                break; // reconnect, keeping the overlay up
            }
            hud.tick(now_ms());
            overlay.render(&hud, now_ms());
            // ~60fps while toasts are on screen (smooth fade/slide); idle-slow
            // otherwise to keep CPU near zero when there's nothing to show.
            let frame_ms = if hud.has_content() { 16 } else { 100 };
            std::thread::sleep(Duration::from_millis(frame_ms));
        }

        // Brief backoff before reconnecting to the (restarting) daemon.
        std::thread::sleep(Duration::from_millis(500));
    }
}
