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

/// `$XDG_CACHE_HOME/open-recorder/thumbs`.
pub fn thumb_cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".cache")
        });
    base.join("open-recorder/thumbs")
}

/// Cached thumbnail path for a clip (not necessarily existing yet).
pub fn thumbnail_path(clip_path: &Path) -> PathBuf {
    let stem = clip_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("clip");
    thumb_cache_dir().join(format!("{stem}.jpg"))
}

fn extract(clip_path: &Path, out: &Path, seek: &str) -> bool {
    Command::new(ord_export::ffmpeg_bin())
        .args(["-v", "error", "-y", "-ss", seek, "-i"])
        .arg(clip_path)
        .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "4"])
        .arg(out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
        && out.exists()
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
    if extract(clip_path, &out, "1") || extract(clip_path, &out, "0") {
        Some(out)
    } else {
        None
    }
}

/// Where exported files are written (`<clips>/exports`).
pub fn exports_dir(clips_dir: &Path) -> PathBuf {
    clips_dir.join("exports")
}
