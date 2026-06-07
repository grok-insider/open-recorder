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

fn main() {
    let path = socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ord-hud: cannot reach ordd at {} ({e})", path.display());
            std::process::exit(1);
        }
    };
    if let Err(e) = write_frame(&mut stream, &Command::Subscribe.encode().unwrap()) {
        eprintln!("ord-hud: subscribe failed: {e}");
        std::process::exit(1);
    }

    // Read events on a background thread; the main thread owns the Wayland
    // connection and renders (Wayland client objects are not Send).
    let (tx, rx) = mpsc::channel::<Event>();
    std::thread::spawn(move || {
        let mut s = stream;
        // Reads frames until the daemon closes (read_frame Err) or the main
        // thread drops the receiver (send Err). Dropping tx signals EOF.
        while let Ok(bytes) = read_frame(&mut s) {
            if let Ok(ev) = Event::decode(&bytes) {
                if tx.send(ev).is_err() {
                    break;
                }
            }
        }
    });

    let mut overlay = LayerShellOverlay::new();
    if let Err(e) = overlay.create() {
        eprintln!("ord-hud: overlay unavailable: {e}");
        std::process::exit(1);
    }

    let mut hud = Hud::default();
    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;

    loop {
        // Drain any pending events; stop the HUD if the daemon disconnected.
        let mut disconnected = false;
        while let Ok(ev) = rx.try_recv() {
            apply(&mut hud, &ev, now_ms());
        }
        if let Err(TryRecvError::Disconnected) = rx.try_recv() {
            disconnected = true;
        }
        if disconnected {
            overlay.destroy();
            return;
        }
        hud.tick(now_ms());
        overlay.render(&hud);
        std::thread::sleep(Duration::from_millis(100));
    }
}
