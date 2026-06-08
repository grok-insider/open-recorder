//! Decode a single preview frame from a clip at a timestamp (gui-only).
//!
//! Uses `ffmpeg` to seek and emit one MJPEG frame on stdout, decoded in memory
//! (no temp files). Driven off the UI thread by the editor's frame worker.

use std::path::Path;
use std::process::Command;

use eframe::egui;

fn ffmpeg_bin() -> String {
    std::env::var("ORD_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

/// Extract the frame nearest `secs`, scaled to `max_w` wide, as an egui image.
/// `-ss` before `-i` does a fast keyframe seek, so scrubbing stays responsive.
pub fn frame_at(clip: &Path, secs: f64, max_w: u32) -> Option<egui::ColorImage> {
    let out = Command::new(ffmpeg_bin())
        .args(["-v", "error", "-ss", &format!("{:.3}", secs.max(0.0)), "-i"])
        .arg(clip)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &format!("scale={max_w}:-2"),
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "-",
        ])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let img = image::load_from_memory(&out.stdout).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [w as usize, h as usize],
        img.as_raw(),
    ))
}
