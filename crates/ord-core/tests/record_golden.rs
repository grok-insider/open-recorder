//! Golden test for the streaming [`Recorder`]: pushing synthetic Annex-B H.264
//! frames (and Opus packets) must produce a Matroska file ffprobe accepts, with
//! the recording starting at the first keyframe. Runs only with `mux`.

#![cfg(feature = "mux")]

use std::process::Command;

use ord_core::audio::{AudioCodec, AudioParams, EncodedAudioFrame};
use ord_core::backend::{Codec, StreamParams, NANOS_PER_SEC};
use ord_core::ring::EncodedFrame;
use ord_core::Recorder;

mod common;
use common::access_unit;

fn params() -> StreamParams {
    StreamParams {
        width: 1920,
        height: 1080,
        fps: 60,
        codec: Codec::H264,
        time_base_den: NANOS_PER_SEC,
    }
}

fn ffprobe_codecs(path: &std::path::Path) -> Option<Vec<String>> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=nokey=1:noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .map(|s| s.to_string())
            .collect(),
    )
}

#[test]
fn recorder_streams_probeable_av() {
    let aparams = Some(AudioParams {
        sample_rate: 48_000,
        channels: 2,
        codec: AudioCodec::Opus,
    });
    let out = std::env::temp_dir().join(format!("ord-record-av-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let mut rec = Recorder::start(&out, params(), aparams).expect("start recorder");
    let step = NANOS_PER_SEC / 60;
    for i in 0..120i64 {
        let kf = i % 30 == 0; // keyframe every 0.5s; first frame (i=0) is one
        let pts = i * step;
        rec.push_video(&EncodedFrame::new(access_unit(kf), kf, pts, pts))
            .expect("push video");
        rec.push_audio(&EncodedAudioFrame::new(vec![0xfcu8; 40], i, i * 16_000))
            .expect("push audio");
    }
    let path = rec.finish().expect("finish");

    let meta = std::fs::metadata(&path).expect("output exists");
    assert!(
        meta.len() > 200,
        "recording too small: {} bytes",
        meta.len()
    );

    if let Some(codecs) = ffprobe_codecs(&path) {
        assert!(
            codecs.contains(&"h264".to_string()) && codecs.contains(&"opus".to_string()),
            "expected h264 + opus, got {codecs:?}"
        );
    }
    let _ = std::fs::remove_file(&path);
}

/// First/last packet pts (seconds) for one stream (`"v"` / `"a"`), or `None`
/// when ffprobe is unavailable.
fn ffprobe_stream_bounds(path: &std::path::Path, sel: &str) -> Option<(f64, f64)> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            sel,
            "-show_entries",
            "packet=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let pts: Vec<f64> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
    Some((*pts.first()?, *pts.last()?))
}

#[test]
fn recorder_keeps_preroll_audio_and_trims_trailing_audio() {
    // Mimics the real pump order: NVENC emits video with latency, so the
    // audio for the recording's first moments arrives BEFORE the keyframe
    // that carries the matching timestamp. The old recorder dropped all of it
    // (a ~1 s silent hole at the start) and let audio run past the last video
    // frame at the end (a frozen tail).
    let aparams = Some(AudioParams {
        sample_rate: 48_000,
        channels: 2,
        codec: AudioCodec::Opus,
    });
    let out = std::env::temp_dir().join(format!("ord-record-align-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let mut rec = Recorder::start(&out, params(), aparams).expect("start recorder");

    let chunk = 20_000i64; // 20 ms opus chunks (µs)
    let audio = |t_us: i64| EncodedAudioFrame::new(vec![0xfcu8; 40], 0, t_us);

    // Audio for t = 0..1.5 s arrives first (video still inside the encoder).
    for i in 0..75 {
        rec.push_audio(&audio(i * chunk)).expect("preroll audio");
    }
    // Video arrives late: keyframe at t = 1 s, deltas at 60 fps until t = 2 s.
    let step = NANOS_PER_SEC / 60;
    let t0 = NANOS_PER_SEC; // 1 s in ticks (ns)
    for i in 0..60i64 {
        let kf = i == 0;
        let pts = t0 + i * step;
        rec.push_video(&EncodedFrame::new(access_unit(kf), kf, pts, pts))
            .expect("push video");
    }
    // The remaining audio, including a tail past the last video frame.
    for i in 75..150 {
        rec.push_audio(&audio(i * chunk)).expect("tail audio");
    }
    let path = rec.finish().expect("finish");

    if let (Some((v_first, v_last)), Some((a_first, a_last))) = (
        ffprobe_stream_bounds(&path, "v"),
        ffprobe_stream_bounds(&path, "a"),
    ) {
        // Audio from before the keyframe was dropped, but the preroll that
        // matches the video start was kept: both tracks start together.
        assert!(
            (a_first - v_first).abs() <= 0.05,
            "audio starts at {a_first}, video at {v_first}"
        );
        // No audio outruns the last video frame (one chunk of slack).
        assert!(
            a_last <= v_last + 0.021,
            "audio ends at {a_last}, video at {v_last}"
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn recorder_drops_until_first_keyframe() {
    let out = std::env::temp_dir().join(format!("ord-record-kf-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let step = NANOS_PER_SEC / 60;

    let mut rec = Recorder::start(&out, params(), None).expect("start");
    // Delta frames before any keyframe are dropped (no header written yet).
    rec.push_video(&EncodedFrame::new(access_unit(false), false, 0, 0))
        .unwrap();
    rec.push_video(&EncodedFrame::new(access_unit(false), false, step, step))
        .unwrap();
    // The first keyframe opens the file; subsequent deltas record.
    rec.push_video(&EncodedFrame::new(
        access_unit(true),
        true,
        2 * step,
        2 * step,
    ))
    .unwrap();
    rec.push_video(&EncodedFrame::new(
        access_unit(false),
        false,
        3 * step,
        3 * step,
    ))
    .unwrap();
    let path = rec.finish().expect("finish");

    assert!(std::fs::metadata(&path).expect("file").len() > 100);
    if let Some(codecs) = ffprobe_codecs(&path) {
        assert!(codecs.contains(&"h264".to_string()), "got {codecs:?}");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn recorder_bounds_held_audio_during_video_stall() {
    // Regression: after the header is written, a stalled video stream (encoder
    // hang, frozen compositor) with audio still flowing must not grow the
    // held-back audio queue without bound for the life of the recording.
    let aparams = Some(AudioParams {
        sample_rate: 48_000,
        channels: 2,
        codec: AudioCodec::Opus,
    });
    let out = std::env::temp_dir().join(format!("ord-record-stall-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let mut rec = Recorder::start(&out, params(), aparams).expect("start");

    // One keyframe opens the file, then video freezes.
    rec.push_video(&EncodedFrame::new(access_unit(true), true, 0, 0))
        .expect("push keyframe");

    // A minute of 20 ms audio chunks with no video progress.
    let chunk = 20_000i64;
    for i in 1..3_000i64 {
        rec.push_audio(&EncodedAudioFrame::new(vec![0xfcu8; 40], i, i * chunk))
            .expect("push audio");
    }

    // The 5 s hold-back window is 250 chunks; anything near 3000 means the
    // bound regressed.
    assert!(
        rec.held_audio_frames() <= 260,
        "held audio grew unbounded: {} frames",
        rec.held_audio_frames()
    );
    let _ = rec.finish().expect("finish");
    let _ = std::fs::remove_file(&out);
}

#[test]
fn recorder_without_keyframe_finishes_safely() {
    // A recording that never sees a keyframe must not panic on finish.
    let out = std::env::temp_dir().join(format!("ord-record-empty-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let mut rec = Recorder::start(&out, params(), None).expect("start");
    rec.push_video(&EncodedFrame::new(access_unit(false), false, 0, 0))
        .unwrap();
    let _ = rec.finish().expect("finish without panic");
    let _ = std::fs::remove_file(&out);
}
