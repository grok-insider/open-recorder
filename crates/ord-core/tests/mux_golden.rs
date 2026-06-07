//! Golden test for the muxer: a synthetic Annex-B H.264 clip must produce a
//! Matroska file whose structure ffprobe accepts (codec, stream present, the
//! avcC extradata built from SPS/PPS). Runs only with the `mux` feature.
//!
//! This uses a tiny hand-built H.264-ish bitstream: real SPS/PPS NAL headers so
//! `build_avcc` succeeds, plus IDR/non-IDR slices. ffprobe is invoked to assert
//! the muxed container is well-formed.

#![cfg(feature = "mux")]

use std::process::Command;

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
        params: StreamParams {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            time_base_den: NANOS_PER_SEC,
        },
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
fn mux_rejects_clip_without_keyframe() {
    // No keyframe -> cannot build avcC -> error (never a silent bad file).
    let frames = vec![EncodedFrame::new(access_unit(false), false, 0, 1)];
    let clip = PreparedClip {
        frames,
        params: StreamParams {
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            time_base_den: NANOS_PER_SEC,
        },
    };
    let out = std::env::temp_dir().join(format!("ord-mux-nokf-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&out);
    assert!(ord_core::write_clip(&clip, &out).is_err());
    let _ = std::fs::remove_file(&out);
}
