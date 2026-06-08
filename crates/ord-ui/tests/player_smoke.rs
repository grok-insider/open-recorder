//! Headless smoke test for the inline preview [`Player`]. egui's `Context`
//! works without a window, so we can verify decode + master clock + seek without
//! a display. `#[ignore]`d (needs ffmpeg + a real clip); run in the devshell:
//!
//! ```sh
//! nix develop -c cargo test -p ord-ui --features gui -- --ignored
//! ```

#![cfg(feature = "gui")]

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;
use ord_ui::player::Player;

fn a_clip() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("HOME").ok()?).join("Videos/open-recorder");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            matches!(
                p.extension().and_then(|x| x.to_str()),
                Some("mkv") | Some("mp4")
            )
        })
}

#[test]
#[ignore = "needs ffmpeg + a real clip; run in devshell with --ignored"]
fn player_decodes_advances_and_seeks() {
    let Some(clip) = a_clip() else {
        eprintln!("no clip in ~/Videos/open-recorder; skipping");
        return;
    };
    let ctx = egui::Context::default();
    let mut p = Player::open(&clip, &ctx).expect("open player");
    p.set_volume(0.0); // don't blip the speakers during the test
    assert!(p.duration() > 0.0, "duration should be known");

    // First preview frame should decode shortly.
    let mut got = false;
    for _ in 0..100 {
        if p.frame(&ctx).is_some() {
            got = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(got, "no preview frame decoded");

    // Playback advances the master clock.
    p.play();
    std::thread::sleep(Duration::from_millis(800));
    let pos = p.position();
    assert!(pos > 0.2, "clock did not advance during playback: {pos}");
    p.pause();

    // Seeking lands near the target.
    let mid = (p.duration() / 2.0).max(0.5);
    p.seek(mid);
    std::thread::sleep(Duration::from_millis(150));
    let pos2 = p.position();
    assert!(
        (pos2 - mid).abs() < 1.0,
        "seek landed at {pos2}, expected ~{mid}"
    );
}
