//! Player benchmark: measures preview-decode sustain and seek latency across
//! source resolutions, so quality/perf changes are quantifiable. `#[ignore]`d
//! (needs ffmpeg + a few seconds). Run in the devshell:
//!
//! ```sh
//! nix develop -c cargo test -p ord-ui --features gui --test bench -- --ignored --nocapture
//! ```

#![cfg(feature = "gui")]

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use eframe::egui;
use ord_ui::player::Player;

fn gen_clip(w: u32, h: u32, fps: u32, secs: u32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("ord-bench-{w}x{h}-{}.mkv", std::process::id()));
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
            &format!("testsrc2=size={w}x{h}:rate={fps}:duration={secs}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:sample_rate=48000:duration={secs}"),
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            &fps.to_string(),
            "-c:a",
            "libopus",
        ])
        .arg(&path)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
    path
}

fn wait_first_frame(p: &mut Player, ctx: &egui::Context) -> Duration {
    let t0 = Instant::now();
    for _ in 0..200 {
        if p.frame(ctx).is_some() {
            return t0.elapsed();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    t0.elapsed()
}

/// Newest real recording, if any (real decode complexity), else None.
fn newest_real_clip() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ORD_BENCH_CLIP") {
        return Some(PathBuf::from(p));
    }
    let dir = PathBuf::from(std::env::var("HOME").ok()?).join("Videos/open-recorder");
    let mut clips: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("mkv"))
        .collect();
    clips.sort();
    clips.pop()
}

#[test]
#[ignore = "needs ffmpeg; run in devshell with --ignored --nocapture"]
fn bench_player_decode_and_seek() {
    let ctx = egui::Context::default();
    println!("\n=== ord-ui player benchmark ===");
    // A real recording (true H.264 decode load) plus synthetic baselines.
    let real = newest_real_clip();
    if let Some(c) = &real {
        println!("real clip: {}", c.display());
    } else {
        println!("(no real clip found; synthetic only — set ORD_BENCH_CLIP to test one)");
    }
    println!(
        "{:<14} {:>10} {:>10} {:>8} {:>9} {:>9} {:>11}",
        "source", "firstframe", "realtime", "decfps", "dropped", "abuf_ms", "seek_avg"
    );

    let mut cases: Vec<(String, PathBuf, bool)> = Vec::new();
    if let Some(c) = real {
        cases.push(("real".to_string(), c, false));
    }
    for &(w, h, fps) in &[(1280u32, 720u32, 60u32), (1920, 1080, 60), (2560, 1440, 60)] {
        cases.push((format!("{w}x{h}"), gen_clip(w, h, fps, 6), true));
    }

    for (label, clip, synthetic) in cases {
        let mut p = Player::open(&clip).expect("open");
        p.set_volume(0.0);

        let first = wait_first_frame(&mut p, &ctx);

        // Play ~2s and sample sustain.
        p.play();
        let start_pos = p.position();
        let t0 = Instant::now();
        let dec0 = p.stats().decoded;
        let mut min_abuf = f64::MAX;
        while t0.elapsed() < Duration::from_secs(2) {
            let _ = p.frame(&ctx);
            min_abuf = min_abuf.min(p.stats().audio_buf_ms);
            std::thread::sleep(Duration::from_millis(16));
        }
        let wall = t0.elapsed().as_secs_f64();
        let s = p.stats();
        let realtime = (p.position() - start_pos) / wall; // 1.0 == real-time
        let dec_fps = (s.decoded - dec0) as f64 / wall;
        p.pause();

        // Seek latency: time until the shown frame reaches the target.
        let mut seek_total = Duration::ZERO;
        let seeks = 8;
        for i in 0..seeks {
            let target = (i as f64 + 0.5) * p.duration() / seeks as f64;
            let t = Instant::now();
            p.seek(target);
            for _ in 0..200 {
                let _ = p.frame(&ctx);
                if (p.position() - target).abs() < 0.25 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            seek_total += t.elapsed();
        }
        let seek_avg = seek_total / seeks;

        println!(
            "{:<14} {:>8.0}ms {:>9.2}x {:>8.1} {:>9} {:>8.0} {:>9.0?}",
            label,
            first.as_secs_f64() * 1000.0,
            realtime,
            dec_fps,
            s.dropped,
            min_abuf,
            seek_avg
        );

        drop(p);
        if synthetic {
            let _ = std::fs::remove_file(&clip);
        }
    }
    println!("(realtime ~1.00x and abuf_ms > 0 => smooth playback)\n");
}
