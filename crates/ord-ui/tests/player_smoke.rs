//! Headless smoke test for the inline preview [`Player`]. egui's `Context`
//! works without a window, so we can verify decode + audio + master clock + seek
//! without a display. Generates its own (ffmpeg-interleaved) clip so it doesn't
//! depend on recorded files. `#[ignore]`d (needs ffmpeg); run in the devshell:
//!
//! ```sh
//! nix develop -c cargo test -p ord-ui --features gui -- --ignored
//! ```

#![cfg(feature = "gui")]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use eframe::egui;
use ord_ui::player::Player;

/// Synthesize a short interleaved clip (video + tone) for the player to chew on.
fn make_clip() -> PathBuf {
    let path = std::env::temp_dir().join(format!("ord-player-smoke-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=30:duration=6",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=6",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "30",
            "-c:a",
            "libopus",
        ])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success(), "failed to synthesize clip");
    path
}

#[test]
#[ignore = "needs ffmpeg; run in devshell with --ignored"]
fn player_decodes_audio_advances_and_seeks() {
    let clip = make_clip();
    let ctx = egui::Context::default();
    let mut p = Player::open(&clip, &ctx).expect("open player");
    p.set_volume(0.0); // don't blip the speakers during the test
    assert!(p.duration() > 0.0, "duration should be known");
    assert!(p.has_audio(), "clip has audio");

    // First preview frame decodes shortly.
    let mut got = false;
    for _ in 0..100 {
        if p.frame(&ctx).is_some() {
            got = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(got, "no preview frame decoded");

    // Playback fills the audio buffer and advances the master clock.
    p.play();
    let mut audio_seen = false;
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(100));
        if p.stats().audio_buf_ms > 1.0 {
            audio_seen = true;
        }
    }
    assert!(
        audio_seen,
        "audio buffer never filled (interleaving/decoder bug)"
    );
    let pos = p.position();
    assert!(pos > 0.2, "clock did not advance during playback: {pos}");
    p.pause();

    // Seeking lands near the target.
    let mid = (p.duration() / 2.0).max(0.5);
    p.seek(mid);
    std::thread::sleep(Duration::from_millis(200));
    let pos2 = p.position();
    assert!(
        (pos2 - mid).abs() < 1.0,
        "seek landed at {pos2}, expected ~{mid}"
    );

    let _ = std::fs::remove_file(&clip);
}
