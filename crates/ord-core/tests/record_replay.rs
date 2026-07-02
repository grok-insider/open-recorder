//! Integration test for record-while-replay (gpu-screen-recorder's `-ro`): a
//! full-length recording is tee'd off the live capture *in addition to* the
//! replay ring, so the engine can save replay clips while a recording runs and
//! both come from a single capture/encode pass. Runs only with `mux`.

#![cfg(feature = "mux")]

use std::process::Command;
use std::sync::mpsc::{self, Receiver};

use ord_common::ClipDuration;
use ord_core::audio::AudioParams;
use ord_core::backend::{
    BackendError, CaptureBackend, CaptureStreams, Codec, StreamParams, NANOS_PER_SEC,
};
use ord_core::ring::EncodedFrame;
use ord_core::Engine;

mod common;
use common::access_unit;

/// A GPU-free backend that emits real, ffmpeg-decodable H.264 access units (the
/// stock `MockBackend` emits opaque bytes the muxer would reject). Frames are
/// produced up-front so the engine drains deterministically.
struct ValidFrameBackend {
    fps: u32,
    total: u32,
    keyframe_interval: u32,
    running: bool,
}

impl CaptureBackend for ValidFrameBackend {
    fn start(&mut self) -> Result<CaptureStreams, BackendError> {
        if self.running {
            return Err(BackendError::AlreadyRunning);
        }
        let (vtx, vrx): (_, Receiver<EncodedFrame>) = mpsc::channel();
        let step = NANOS_PER_SEC / self.fps as i64;
        for i in 0..self.total as i64 {
            let kf = (i as u32).is_multiple_of(self.keyframe_interval);
            let pts = i * step;
            let _ = vtx.send(EncodedFrame::new(access_unit(kf), kf, pts, pts));
        }
        self.running = true;
        Ok(CaptureStreams {
            video: vrx,
            audio: None,
        })
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        if !self.running {
            return Err(BackendError::NotRunning);
        }
        self.running = false;
        Ok(())
    }

    fn params(&self) -> StreamParams {
        StreamParams {
            width: 1920,
            height: 1080,
            fps: self.fps,
            codec: Codec::H264,
            time_base_den: NANOS_PER_SEC,
        }
    }

    fn audio_params(&self) -> Option<AudioParams> {
        None
    }

    fn is_running(&self) -> bool {
        self.running
    }
}

fn ffprobe_has_h264(path: &std::path::Path) -> Option<bool> {
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
    Some(String::from_utf8_lossy(&out.stdout).contains("h264"))
}

#[test]
fn records_full_length_while_replay_buffer_stays_live() {
    // 60 fps, 300 frames (5 s), keyframe every 30 (0.5 s).
    let backend = ValidFrameBackend {
        fps: 60,
        total: 300,
        keyframe_interval: 30,
        running: false,
    };
    let mut eng = Engine::new(backend, 60);
    eng.start().unwrap();

    let rec_path = std::env::temp_dir().join(format!("ord-ro-{}.mkv", std::process::id()));
    let _ = std::fs::remove_file(&rec_path);

    // Start a full recording tee'd off the same capture, then ingest frames.
    eng.start_recording(rec_path.clone())
        .expect("start recording");
    assert!(eng.is_recording());
    let drained = eng.drain_available();
    assert_eq!(drained, 300);

    // The replay buffer filled in parallel: a clip can still be saved mid-record.
    let clip = eng
        .take_clip(ClipDuration::new(2).unwrap())
        .expect("save replay clip while recording");
    assert!(clip.frames.first().unwrap().is_keyframe);
    assert!(eng.buffered_frames() > 0, "replay buffer must stay live");

    // Finalize the recording: a valid, probeable file.
    let path = eng
        .stop_recording()
        .expect("was recording")
        .expect("finish");
    assert!(!eng.is_recording());
    assert!(
        std::fs::metadata(&path).expect("recording exists").len() > 200,
        "recording file too small"
    );
    if let Some(has) = ffprobe_has_h264(&path) {
        assert!(has, "recording should contain an h264 stream");
    }
    let _ = std::fs::remove_file(&path);
}
