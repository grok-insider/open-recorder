//! Manual smoke test for the layer-shell HUD.
//!
//! Run inside a Wayland session:
//!   nix develop -c cargo run -p ord-overlay --features layershell --example hud_demo
//!
//! Shows a buffer indicator + a few toasts for ~3 seconds, then exits. The HUD
//! should float top-right over everything and be click-through.

#[cfg(feature = "layershell")]
fn main() {
    use ord_overlay::hud::{Hud, ToastKind};
    use ord_overlay::{LayerShellOverlay, Overlay};
    use std::time::{Duration, Instant};

    let mut overlay = LayerShellOverlay::new();
    if let Err(e) = overlay.create() {
        eprintln!("hud_demo: could not create overlay: {e}");
        std::process::exit(1);
    }

    let mut hud = Hud::default();
    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;
    hud.toast_for(ToastKind::Saved, "Clip saved (30s)", now_ms(), 8000);
    hud.toast_for(ToastKind::Recording, "Replay buffer on", now_ms(), 8000);
    hud.toast_for(ToastKind::Stopped, "Recording stopped", now_ms(), 8000);
    hud.toast_for(
        ToastKind::Error,
        "Capture stalled — restart failed (screen share or encoder init)",
        now_ms(),
        8000,
    );

    while start.elapsed() < Duration::from_secs(9) {
        hud.tick(now_ms());
        overlay.render(&hud, now_ms());
        std::thread::sleep(Duration::from_millis(16));
    }
    overlay.destroy();
    eprintln!("hud_demo: done");
}

#[cfg(not(feature = "layershell"))]
fn main() {
    eprintln!("build with --features layershell to run this demo");
}
