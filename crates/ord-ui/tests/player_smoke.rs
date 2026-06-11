//! Headless smoke test for the inline preview [`Player`]. egui's `Context`
//! works without a window, so we can verify decode + audio + master clock + seek
//! without a display. Generates its own (ffmpeg-interleaved) clips so it doesn't
//! depend on recorded files. `#[ignore]`d (needs ffmpeg + an audio device); run
//! in the devshell:
//!
//! ```sh
//! nix develop -c cargo test -p ord-ui --features gui -- --ignored
//! ```

#![cfg(feature = "gui")]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use eframe::egui;
use ord_ui::player::Player;

/// Synthesize a short interleaved clip (video + tone). `audio_secs` shorter
/// than `video_secs` reproduces real recordings whose audio track ends before
/// the container does.
fn make_clip(tag: &str, video_secs: u32, audio_secs: u32) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("ord-player-smoke-{tag}-{}.mkv", std::process::id()));
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
            &format!("testsrc2=size=1280x720:rate=30:duration={video_secs}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:sample_rate=48000:duration={audio_secs}"),
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
    let clip = make_clip("av", 6, 6);
    let ctx = egui::Context::default();
    let mut p = Player::open(&clip).expect("open player");
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

    // Opens PAUSED: without play(), the master clock must not advance (no
    // auto-play, even though cpal's stream.pause() is a no-op on some hosts).
    let p0 = p.position();
    for _ in 0..15 {
        let _ = p.frame(&ctx);
        std::thread::sleep(Duration::from_millis(20));
    }
    let p1 = p.position();
    assert!(!p.is_playing(), "should not be playing on open");
    assert!(
        (p1 - p0).abs() < 0.05,
        "clock advanced while paused (auto-play): {p0} -> {p1}"
    );

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

/// Audio track ends 3s before the video: the master clock must be fed trailing
/// silence so the video tail PLAYS OUT at speed and playback ends cleanly —
/// the old behavior froze the clock and demux-raced through the remaining
/// video at full speed, decoding and dropping everything (>100% CPU, slow
/// video, crackling audio) until EOF, forever when looping.
#[test]
#[ignore = "needs ffmpeg; run in devshell with --ignored"]
fn audio_shorter_than_video_plays_tail_and_stops() {
    let clip = make_clip("short-audio", 6, 3);
    let ctx = egui::Context::default();
    let mut p = Player::open(&clip).expect("open player");
    p.set_volume(0.0);
    assert!(p.has_audio(), "clip has audio");
    let duration = p.duration();
    assert!(duration > 5.0, "container should be ~6s, got {duration}");

    // Start inside the audio portion, just before it runs out.
    p.seek(2.5);
    p.play();

    // Drive the UI loop until playback ends on its own. From 2.5s the
    // remaining 3.5s of media must finish in near-realtime — a frozen clock
    // (the old bug) would still sit near 3.0 when the deadline hits.
    let t0 = Instant::now();
    let deadline = Duration::from_secs(15);
    while p.is_playing() && t0.elapsed() < deadline {
        let _ = p.frame(&ctx);
        std::thread::sleep(Duration::from_millis(16));
    }
    let s = p.stats();
    assert!(
        !p.is_playing(),
        "playback never ended: clock stuck at {:.2} (audio tail not silence-fed)",
        s.position
    );
    assert!(
        s.position >= duration - 0.3,
        "ended early: position {:.2} of {duration:.2}",
        s.position
    );
    assert!(
        s.shown_pts >= duration - 1.0,
        "video tail did not play out: last shown frame at {:.2} of {duration:.2}",
        s.shown_pts
    );
    // The fix removes the decode-and-drop race: steady playback should drop
    // (at most) a handful of frames, not the entire tail.
    assert!(
        s.dropped < 30,
        "decode race regression: {} frames decoded-and-dropped",
        s.dropped
    );

    let _ = std::fs::remove_file(&clip);
}

/// Seeking to the very end (out-point == container duration) must still
/// display a frame: the precise-seek target lies past the last real frame's
/// pts, so the decode thread re-runs the tail GOP at EOF and presents its
/// final frame — the old behavior left the UI requesting repaints forever for
/// a frame that could never arrive.
#[test]
#[ignore = "needs ffmpeg; run in devshell with --ignored"]
fn seek_to_end_shows_final_frame() {
    let clip = make_clip("end-seek", 6, 6);
    let ctx = egui::Context::default();
    let mut p = Player::open(&clip).expect("open player");
    p.set_volume(0.0);
    let duration = p.duration();

    p.seek(duration);
    let t0 = Instant::now();
    let mut shown = f64::MIN;
    while t0.elapsed() < Duration::from_secs(10) {
        let _ = p.frame(&ctx);
        shown = p.stats().shown_pts;
        if shown >= duration - 0.5 {
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(
        shown >= duration - 0.5,
        "no final frame shown after seeking to the end: shown_pts {shown:.2} of {duration:.2}"
    );

    let _ = std::fs::remove_file(&clip);
}
