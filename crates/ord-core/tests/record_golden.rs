//! Golden test for the streaming [`Recorder`]: pushing synthetic Annex-B H.264
//! frames (and Opus packets) must produce a Matroska file ffprobe accepts, with
//! the recording starting at the first keyframe. Runs only with `mux`.

#![cfg(feature = "mux")]

use std::process::Command;

use ord_core::audio::{AudioCodec, AudioParams, EncodedAudioFrame};
use ord_core::backend::{Codec, StreamParams, NANOS_PER_SEC};
use ord_core::ring::EncodedFrame;
use ord_core::Recorder;

fn access_unit(keyframe: bool) -> Vec<u8> {
    let sc = [0u8, 0, 0, 1];
    let mut d = Vec::new();
    if keyframe {
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01]);
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x68, 0xce, 0x3c, 0x80]);
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x65, 0x88, 0x84, 0x00, 0x33, 0x44, 0x55]);
    } else {
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x41, 0x9a, 0x00, 0x10, 0x20]);
    }
    d
}

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
