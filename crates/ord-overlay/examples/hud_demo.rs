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
    hud.set_buffer_active(true);

    let start = Instant::now();
    let now_ms = || start.elapsed().as_millis() as u64;
    hud.toast(ToastKind::Saved, "Clip saved", now_ms());
    hud.toast(ToastKind::Recording, "Recording", now_ms());

    while start.elapsed() < Duration::from_secs(3) {
        hud.tick(now_ms());
        overlay.render(&hud);
        std::thread::sleep(Duration::from_millis(100));
    }
    overlay.destroy();
    eprintln!("hud_demo: done");
}

#[cfg(not(feature = "layershell"))]
fn main() {
    eprintln!("build with --features layershell to run this demo");
}
