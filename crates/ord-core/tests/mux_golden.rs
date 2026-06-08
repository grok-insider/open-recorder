//! Golden test for the muxer: a synthetic Annex-B H.264 clip must produce a
//! Matroska file whose structure ffprobe accepts (codec, stream present, the
//! avcC extradata built from SPS/PPS). Runs only with the `mux` feature.
//!
//! This uses a tiny hand-built H.264-ish bitstream: real SPS/PPS NAL headers so
//! `build_avcc` succeeds, plus IDR/non-IDR slices. ffprobe is invoked to assert
//! the muxed container is well-formed.

#![cfg(feature = "mux")]

use std::process::Command;

use ord_core::audio::{AudioCodec, AudioParams, EncodedAudioFrame};
use ord_core::backend::{Codec, StreamParams, NANOS_PER_SEC};
use ord_core::engine::PreparedClip;
use ord_core::ring::EncodedFrame;

/// Build one Annex-B access unit. `keyframe` adds SPS+PPS before an IDR slice;
/// otherwise a single non-IDR slice. NAL payloads are minimal but carry valid
/// NAL headers so the avcC builder and muxer accept them.
fn access_unit(keyframe: bool) -> Vec<u8> {
    let mut d = Vec::new();
    let sc = [0u8, 0, 0, 1];
    if keyframe {
        // SPS (type 7): bytes after header are profile/constraint/level + payload.
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01]);
        // PPS (type 8).
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x68, 0xce, 0x3c, 0x80]);
        // IDR slice (type 5).
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x65, 0x88, 0x84, 0x00, 0x33, 0x44, 0x55]);
    } else {
        // Non-IDR slice (type 1).
        d.extend_from_slice(&sc);
        d.extend_from_slice(&[0x41, 0x9a, 0x00, 0x10, 0x20]);
    }
    d
}

fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn mux_produces_probeable_file() {
    // 30 frames at 60fps (nanosecond pts), keyframe every 10.
    let step = NANOS_PER_SEC / 60;
    let frames: Vec<EncodedFrame> = (0..30)
        .map(|i| {
            let kf = i % 10 == 0;
            let pts = i as i64 * step;
            EncodedFrame::new(access_unit(kf), kf, pts, pts)
        })
        .collect();

    let clip = PreparedClip {
        frames,
        audio: vec![],
        params: StreamParams {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            time_base_den: NANOS_PER_SEC,
        },
        audio_params: None,
    };

    let out = std::env::temp_dir().join(format!("ord-mux-golden-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);

    ord_core::write_clip(&clip, &out).expect("write_clip should succeed");

    // The file must exist and be non-trivial (header + frames).
    let meta = std::fs::metadata(&out).expect("output file exists");
    assert!(
        meta.len() > 200,
        "muxed file too small: {} bytes",
        meta.len()
    );

    // If ffprobe is available, assert the container is a valid H.264 mkv.
    if ffprobe_available() {
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "default=nokey=1:noprint_wrappers=1",
            ])
            .arg(&out)
            .output()
            .expect("run ffprobe");
        let codec = String::from_utf8_lossy(&probe.stdout);
        assert!(
            codec.trim() == "h264",
            "expected h264 stream, ffprobe said: {codec:?} (stderr: {})",
            String::from_utf8_lossy(&probe.stderr)
        );
    }

    let _ = std::fs::remove_file(&out);
}

#[test]
fn mux_with_audio_produces_two_streams() {
    // 30 video frames at 60fps (ns pts) + synthetic Opus packets every 20ms.
    let step = NANOS_PER_SEC / 60;
    let frames: Vec<EncodedFrame> = (0..30)
        .map(|i| {
            let kf = i % 10 == 0;
            let pts = i as i64 * step;
            EncodedFrame::new(access_unit(kf), kf, pts, pts)
        })
        .collect();
    // ~0.5s of audio (25 * 20ms), bytes are opaque to a copy-mux.
    let audio: Vec<EncodedAudioFrame> = (0..25)
        .map(|i| {
            let ts_us = i as i64 * 20_000;
            EncodedAudioFrame::new(vec![0xfcu8; 40], i as i64 * 960, ts_us)
        })
        .collect();

    let clip = PreparedClip {
        frames,
        audio,
        params: StreamParams {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            time_base_den: NANOS_PER_SEC,
        },
        audio_params: Some(AudioParams {
            sample_rate: 48000,
            channels: 2,
            codec: AudioCodec::Opus,
        }),
    };

    let out = std::env::temp_dir().join(format!("ord-mux-audio-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);
    ord_core::write_clip(&clip, &out).expect("write_clip with audio should succeed");

    if ffprobe_available() {
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "default=nokey=1:noprint_wrappers=1",
            ])
            .arg(&out)
            .output()
            .expect("run ffprobe");
        let codecs = String::from_utf8_lossy(&probe.stdout);
        let names: Vec<&str> = codecs.split_whitespace().collect();
        assert!(
            names.contains(&"h264") && names.contains(&"opus"),
            "expected h264 + opus, ffprobe said: {codecs:?} (stderr: {})",
            String::from_utf8_lossy(&probe.stderr)
        );
    }

    let _ = std::fs::remove_file(&out);
}

#[test]
fn mux_interleaves_audio_and_video() {
    // Video + audio covering the same 0.5s. A correct mux interleaves them by
    // timestamp; the old bug wrote [all video][all audio], so players had to
    // read the whole video block before any audio (audio appeared delayed).
    let step = NANOS_PER_SEC / 60;
    let frames: Vec<EncodedFrame> = (0..30)
        .map(|i| {
            let kf = i % 10 == 0;
            let pts = i as i64 * step;
            EncodedFrame::new(access_unit(kf), kf, pts, pts)
        })
        .collect();
    let audio: Vec<EncodedAudioFrame> = (0..25)
        .map(|i| EncodedAudioFrame::new(vec![0xfcu8; 40], i as i64 * 960, i as i64 * 20_000))
        .collect();

    let clip = PreparedClip {
        frames,
        audio,
        params: StreamParams {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            time_base_den: NANOS_PER_SEC,
        },
        audio_params: Some(AudioParams {
            sample_rate: 48000,
            channels: 2,
            codec: AudioCodec::Opus,
        }),
    };

    let out = std::env::temp_dir().join(format!("ord-mux-interleave-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);
    ord_core::write_clip(&clip, &out).expect("write_clip");

    if ffprobe_available() {
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "packet=stream_index",
                "-of",
                "csv=p=0",
                "-read_intervals",
                "%+#12",
            ])
            .arg(&out)
            .output()
            .expect("run ffprobe");
        let order: Vec<&str> = std::str::from_utf8(&probe.stdout)
            .unwrap()
            .split_whitespace()
            .collect();
        // Both streams must appear, and an audio packet (stream 1) must show up
        // early — not only after the entire video block.
        assert!(
            order.iter().any(|s| *s == "0"),
            "no video packets: {order:?}"
        );
        assert!(
            order.iter().take(6).any(|s| *s == "1"),
            "audio not interleaved early (got {order:?})"
        );
    }
    let _ = std::fs::remove_file(&out);
}

#[test]
fn mux_with_large_monotonic_base_stays_synced() {
    // waycap pts are raw CLOCK_MONOTONIC nanoseconds (~10^14). The clip's video
    // and audio share that epoch; muxing must (a) not overflow and (b) rebase
    // both to ~0 so the audio is not flung hours away from the video.
    let base_ns: i64 = 100_000_000_000_000; // ~27.7 hours of uptime
    let step = NANOS_PER_SEC / 60;
    let frames: Vec<EncodedFrame> = (0..30)
        .map(|i| {
            let kf = i % 10 == 0;
            let pts = base_ns + i as i64 * step;
            EncodedFrame::new(access_unit(kf), kf, pts, pts)
        })
        .collect();
    // Audio timestamps in microseconds on the same monotonic epoch.
    let base_us = base_ns / 1000;
    let audio: Vec<EncodedAudioFrame> = (0..25)
        .map(|i| {
            EncodedAudioFrame::new(
                vec![0xfcu8; 40],
                i as i64 * 960,
                base_us + i as i64 * 20_000,
            )
        })
        .collect();

    let clip = PreparedClip {
        frames,
        audio,
        params: StreamParams {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            time_base_den: NANOS_PER_SEC,
        },
        audio_params: Some(AudioParams {
            sample_rate: 48000,
            channels: 2,
            codec: AudioCodec::Opus,
        }),
    };

    let out = std::env::temp_dir().join(format!("ord-mux-bigbase-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);
    // Must not panic with "attempt to multiply with overflow".
    ord_core::write_clip(&clip, &out).expect("write_clip with large base should succeed");

    if ffprobe_available() {
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=start_time",
                "-of",
                "default=nokey=1:noprint_wrappers=1",
            ])
            .arg(&out)
            .output()
            .expect("run ffprobe");
        let start: f64 = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap_or(f64::MAX);
        // Rebased audio starts near 0, not hours in (the old ms/us bug).
        assert!(
            start.abs() < 2.0,
            "audio start_time should be ~0 after rebase, got {start}"
        );
    }
    let _ = std::fs::remove_file(&out);
}

#[test]
fn mux_rejects_clip_without_keyframe() {
    // No keyframe -> cannot build avcC -> error (never a silent bad file).
    let frames = vec![EncodedFrame::new(access_unit(false), false, 0, 1)];
    let clip = PreparedClip {
        frames,
        audio: vec![],
        params: StreamParams {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            time_base_den: NANOS_PER_SEC,
        },
        audio_params: None,
    };
    let out = std::env::temp_dir().join(format!("ord-mux-nokf-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);
    assert!(ord_core::write_clip(&clip, &out).is_err());
    let _ = std::fs::remove_file(&out);
}
