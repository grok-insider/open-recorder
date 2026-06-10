//! Probe an input file's properties via `ffprobe` (JSON output).
//!
//! Isolated from [`crate::plan`] so the planner stays pure: tests build
//! [`SourceInfo`] by hand, while real exports call [`probe`] to fill it in.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::plan::SourceInfo;
use crate::{ffprobe_bin, ExportError};

#[derive(Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<Stream>,
    #[serde(default)]
    format: Format,
}

#[derive(Deserialize, Default)]
struct Format {
    #[serde(default)]
    duration: Option<String>,
}

#[derive(Deserialize)]
struct Stream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    duration: Option<String>,
}

/// Parse an ffprobe ratio like `"60/1"` into frames per second.
fn parse_fps(r: &str) -> f64 {
    match r.split_once('/') {
        Some((n, d)) => {
            let n: f64 = n.parse().unwrap_or(0.0);
            let d: f64 = d.parse().unwrap_or(0.0);
            if d > 0.0 {
                n / d
            } else {
                0.0
            }
        }
        None => r.parse().unwrap_or(0.0),
    }
}

/// Run `ffprobe` on `input` and parse the result into [`SourceInfo`].
pub fn probe(input: &Path) -> Result<SourceInfo, ExportError> {
    let out = Command::new(ffprobe_bin())
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(input)
        .output()
        .map_err(|e| ExportError::Probe(format!("spawn ffprobe: {e}")))?;

    if !out.status.success() {
        return Err(ExportError::Probe(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }

    parse_probe_json(&out.stdout)
}

/// Parse ffprobe JSON bytes into [`SourceInfo`] (split out so it is testable
/// without invoking ffprobe).
pub fn parse_probe_json(json: &[u8]) -> Result<SourceInfo, ExportError> {
    let parsed: ProbeOutput =
        serde_json::from_slice(json).map_err(|e| ExportError::Probe(format!("parse json: {e}")))?;

    let video = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"));
    let audio = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"));

    let (width, height, fps, vid_duration) = match video {
        Some(v) => (
            v.width.unwrap_or(0),
            v.height.unwrap_or(0),
            v.r_frame_rate.as_deref().map(parse_fps).unwrap_or(0.0),
            v.duration.as_deref().and_then(|d| d.parse::<f64>().ok()),
        ),
        None => return Err(ExportError::Probe("no video stream".into())),
    };

    let duration_secs = parsed
        .format
        .duration
        .as_deref()
        .and_then(|d| d.parse::<f64>().ok())
        .or(vid_duration)
        .unwrap_or(0.0);

    Ok(SourceInfo {
        duration_secs,
        width,
        height,
        fps,
        has_audio: audio.is_some(),
        audio_codec: audio.and_then(|a| a.codec_name.clone()),
    })
}

#[derive(Deserialize)]
struct ChaptersOutput {
    #[serde(default)]
    chapters: Vec<ChapterJson>,
}

#[derive(Deserialize)]
struct ChapterJson {
    start_time: Option<String>,
}

/// Chapter start times (seconds) embedded in `input` — e.g. the markers
/// `ord mark` writes into saved clips. Missing/none is an empty list.
pub fn probe_chapters(input: &Path) -> Result<Vec<f64>, ExportError> {
    let out = Command::new(ffprobe_bin())
        .args(["-v", "error", "-print_format", "json", "-show_chapters"])
        .arg(input)
        .output()
        .map_err(|e| ExportError::Probe(format!("spawn ffprobe: {e}")))?;
    if !out.status.success() {
        return Err(ExportError::Probe(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    parse_chapters_json(&out.stdout)
}

/// Parse `ffprobe -show_chapters` JSON into start times (testable offline).
pub fn parse_chapters_json(json: &[u8]) -> Result<Vec<f64>, ExportError> {
    let parsed: ChaptersOutput =
        serde_json::from_slice(json).map_err(|e| ExportError::Probe(format!("parse json: {e}")))?;
    let mut starts: Vec<f64> = parsed
        .chapters
        .iter()
        .filter_map(|c| c.start_time.as_deref().and_then(|s| s.parse::<f64>().ok()))
        .filter(|t| t.is_finite() && *t >= 0.0)
        .collect();
    starts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Ok(starts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_probe() {
        let json = br#"{
            "streams": [
                {"codec_type":"video","codec_name":"h264","width":2560,"height":1440,"r_frame_rate":"60/1"},
                {"codec_type":"audio","codec_name":"opus"}
            ],
            "format": {"duration":"30.500000"}
        }"#;
        let info = parse_probe_json(json).unwrap();
        assert_eq!(info.width, 2560);
        assert_eq!(info.height, 1440);
        assert_eq!(info.fps, 60.0);
        assert!(info.has_audio);
        assert_eq!(info.audio_codec.as_deref(), Some("opus"));
        assert!((info.duration_secs - 30.5).abs() < 1e-6);
    }

    #[test]
    fn handles_no_audio() {
        let json = br#"{
            "streams":[{"codec_type":"video","width":1920,"height":1080,"r_frame_rate":"30000/1001","duration":"10.0"}],
            "format":{}
        }"#;
        let info = parse_probe_json(json).unwrap();
        assert!(!info.has_audio);
        assert!(info.audio_codec.is_none());
        assert!((info.fps - 29.97).abs() < 0.01);
        // Falls back to the video stream duration when format has none.
        assert!((info.duration_secs - 10.0).abs() < 1e-6);
    }

    #[test]
    fn errors_without_video() {
        let json = br#"{"streams":[{"codec_type":"audio","codec_name":"aac"}],"format":{}}"#;
        assert!(parse_probe_json(json).is_err());
    }

    #[test]
    fn fps_ratio_parsing() {
        assert_eq!(parse_fps("60/1"), 60.0);
        assert_eq!(parse_fps("0/0"), 0.0);
        assert_eq!(parse_fps("24"), 24.0);
    }

    #[test]
    fn parses_chapters_sorted() {
        let json = br#"{"chapters":[
            {"id":1,"start_time":"12.500","end_time":"12.6","tags":{"title":"m2"}},
            {"id":0,"start_time":"3.000","end_time":"3.1"},
            {"id":2,"start_time":"bogus"}
        ]}"#;
        assert_eq!(parse_chapters_json(json).unwrap(), vec![3.0, 12.5]);
    }

    #[test]
    fn no_chapters_is_empty() {
        assert!(parse_chapters_json(br#"{}"#).unwrap().is_empty());
        assert!(parse_chapters_json(br#"{"chapters":[]}"#)
            .unwrap()
            .is_empty());
    }
}
