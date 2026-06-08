//! End-to-end export tests against real `ffmpeg`/`ffprobe` + NVENC.
//!
//! These are `#[ignore]`d so CI (no GPU/ffmpeg) skips them. Run in the devshell:
//!
//! ```sh
//! nix develop -c cargo test -p ord-export -- --ignored
//! ```

use std::path::PathBuf;
use std::process::Command;

use ord_common::config::Container;
use ord_export::profile::ExportProfile;
use ord_export::{export, probe, Trim};

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ord-export-e2e-{}-{}", std::process::id(), name))
}

/// Synthesize a 1440p60 clip with a tone so probe/scale/audio paths are real.
/// `tag` keeps each test's input distinct — cargo runs tests in parallel, so a
/// shared path would race.
fn make_input(tag: &str, secs: u32) -> PathBuf {
    let path = tmp(&format!("{tag}-input.mkv"));
    let _ = std::fs::remove_file(&path);
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size=2560x1440:rate=60:duration={secs}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={secs}"),
            "-c:v",
            "libx264",
            // Dense keyframes (every 0.5s) so the Source copy-trim lands close to
            // the requested window instead of snapping across a long GOP.
            "-g",
            "30",
            "-keyint_min",
            "30",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "libopus",
        ])
        .arg(&path)
        .status()
        .expect("spawn ffmpeg to build input");
    assert!(status.success(), "failed to synthesize input clip");
    path
}

#[test]
#[ignore = "needs ffmpeg + NVENC; run in devshell with --ignored"]
fn high_quality_av1_nvenc_roundtrip() {
    let input = make_input("hq", 4);
    let output = tmp("hq.mp4");
    let _ = std::fs::remove_file(&output);

    let summary = export(&input, &output, &ExportProfile::high_quality(), None)
        .expect("high quality export should succeed");

    assert!(summary.size_bytes > 0);
    let info = probe::probe(&output).expect("probe output");
    assert_eq!(info.width, 2560);
    assert_eq!(info.height, 1440);
    assert!(info.has_audio);

    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}

#[test]
#[ignore = "needs ffmpeg + NVENC; run in devshell with --ignored"]
fn discord_export_caps_size_and_height() {
    let input = make_input("discord", 6);
    let output = tmp("discord.mp4");
    let _ = std::fs::remove_file(&output);

    let summary = export(&input, &output, &ExportProfile::discord(), None)
        .expect("discord export should succeed");

    // Comfortably under the 10 MB free-tier ceiling.
    assert!(
        summary.size_bytes < 10 * 1024 * 1024,
        "discord export too big: {} bytes",
        summary.size_bytes
    );
    let info = probe::probe(&output).expect("probe output");
    assert_eq!(info.height, 1080, "should downscale 1440->1080");

    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}

#[test]
#[ignore = "needs ffmpeg; run in devshell with --ignored"]
fn source_remux_with_trim() {
    let input = make_input("source", 8);
    let output = tmp("src.mkv");
    let _ = std::fs::remove_file(&output);

    let trim = Trim {
        start_secs: 2.0,
        end_secs: 5.0,
    };
    let summary = export(
        &input,
        &output,
        &ExportProfile::source(Container::Mkv),
        Some(trim),
    )
    .expect("source remux should succeed");
    assert!(summary.encoder.is_empty(), "source preset is a copy");

    let info = probe::probe(&output).expect("probe output");
    // ~3s window (keyframe snapping may shift slightly).
    assert!(info.duration_secs > 1.5 && info.duration_secs < 4.5);

    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}
