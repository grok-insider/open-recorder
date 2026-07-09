//! Clip metadata + thumbnail extraction (I/O; gui-only).
//!
//! Probing reuses [`ord_export::probe`]; thumbnails are extracted with `ffmpeg`
//! into the XDG cache so they persist across runs. Both are run off the UI
//! thread by the app's loader.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolved metadata for a clip.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipMeta {
    pub duration_secs: f64,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
}

/// Probe a clip's duration/resolution (via ffprobe) and size (via stat).
pub fn load_meta(path: &Path) -> Option<ClipMeta> {
    let size_bytes = std::fs::metadata(path).ok()?.len();
    let info = ord_export::probe::probe(path).ok()?;
    Some(ClipMeta {
        duration_secs: info.duration_secs,
        width: info.width,
        height: info.height,
        size_bytes,
    })
}

/// Thumbnail cache directory: the platform cache dir + `open-recorder/thumbs`
/// (`$XDG_CACHE_HOME` or `~/.cache` on Linux).
pub fn thumb_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("open-recorder/thumbs")
}

/// Cached thumbnail path for a clip (not necessarily existing yet).
pub fn thumbnail_path(clip_path: &Path) -> PathBuf {
    let stem = clip_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");
    thumb_cache_dir().join(format!("{stem}.jpg"))
}

/// Extract one frame at `secs` as JPEG bytes (ffmpeg → MJPEG pipe), scaled to
/// `width`. The single frame extractor behind both the library's cached
/// thumbnails and the editor's filmstrip — keep them on this one code path.
pub fn extract_frame_jpeg(clip_path: &Path, secs: f64, width: u32) -> Option<Vec<u8>> {
    let out = Command::new(ord_export::ffmpeg_bin())
        .args(["-v", "error", "-ss", &format!("{:.3}", secs.max(0.0)), "-i"])
        .arg(clip_path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &format!("scale={width}:-2"),
            "-q:v",
            "4",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "-",
        ])
        .output()
        .ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

/// Ensure a cached thumbnail exists, extracting one with ffmpeg if needed.
/// Returns the thumbnail path on success.
pub fn ensure_thumbnail(clip_path: &Path) -> Option<PathBuf> {
    let out = thumbnail_path(clip_path);
    if out.exists() {
        return Some(out);
    }
    std::fs::create_dir_all(out.parent()?).ok()?;
    // Prefer a frame ~1s in (avoids black intro frames); fall back to the very
    // first frame for sub-second clips.
    let jpeg = extract_frame_jpeg(clip_path, 1.0, 480)
        .or_else(|| extract_frame_jpeg(clip_path, 0.0, 480))?;
    std::fs::write(&out, jpeg).ok()?;
    Some(out)
}

/// Where exported files are written (`<clips>/exports`).
pub fn exports_dir(clips_dir: &Path) -> PathBuf {
    clips_dir.join("exports")
}

/// Decode mono PCM via ffmpeg and return `n_peaks` normalized peak values for
/// the whole clip. Empty when the file has no audio or ffmpeg fails.
pub fn extract_audio_peaks(clip_path: &Path, n_peaks: usize) -> Vec<f32> {
    if n_peaks == 0 {
        return Vec::new();
    }
    // 8 kHz mono is plenty for a scrub waveform and keeps the decode small
    // (a 5-minute clip is ~2.4 MB of f32).
    let out = Command::new(ord_export::ffmpeg_bin())
        .args(["-v", "error", "-i"])
        .arg(clip_path)
        .args(["-vn", "-ac", "1", "-ar", "8000", "-f", "f32le", "-"])
        .output()
        .ok();
    let Some(out) = out else {
        return Vec::new();
    };
    if !out.status.success() || out.stdout.is_empty() {
        return Vec::new();
    }
    let samples: Vec<f32> = out
        .stdout
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    crate::waveform::peaks_from_samples(&samples, n_peaks)
}
