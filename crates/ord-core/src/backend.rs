//! The capture/encode backend seam.
//!
//! A [`CaptureBackend`] produces a stream of **encoded** video frames from a
//! capture target. The real implementation (waycap-rs/NVENC) lives behind a
//! feature flag; [`MockBackend`] provides deterministic frames so the ring
//! buffer, clip selection, daemon, and engine can all be tested without a GPU or
//! a live Wayland session (per AGENTS.md, a mock is mandatory).

use std::sync::mpsc::{self, Receiver};

use crate::audio::{AudioCodec, AudioParams, EncodedAudioFrame};
use crate::ring::EncodedFrame;
use crate::{Micros, MICROS_PER_SEC};

/// The encoded streams a backend delivers once started: always video, optionally
/// a mixed audio track.
pub struct CaptureStreams {
    pub video: Receiver<EncodedFrame>,
    pub audio: Option<Receiver<EncodedAudioFrame>>,
}

/// Negotiated stream parameters reported by a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Codec fourcc-ish tag, e.g. "h264".
    pub codec: Codec,
    /// Ticks per second for the frame `pts`/`dts` (the time-base denominator).
    /// waycap-rs uses nanoseconds (1_000_000_000); the mock uses microseconds.
    pub time_base_den: i64,
}

/// Nanoseconds per second — waycap-rs frame pts time base.
pub const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Video codec a backend emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::Hevc => "hevc",
            Codec::Av1 => "av1",
        }
    }
}

/// Errors a backend can surface.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("backend is already running")]
    AlreadyRunning,
    #[error("backend is not running")]
    NotRunning,
    #[error("capture initialization failed: {0}")]
    Init(String),
}

/// A source of hardware-encoded frames. Implementations own the capture -> encode
/// pipeline. The hot path (delivering frames) must not block or copy beyond the
/// encoded packet itself.
pub trait CaptureBackend: Send {
    /// Begin capturing. Encoded frames are delivered on the returned receivers
    /// until [`stop`](CaptureBackend::stop) is called.
    fn start(&mut self) -> Result<CaptureStreams, BackendError>;

    /// Stop capturing and release resources.
    fn stop(&mut self) -> Result<(), BackendError>;

    /// Negotiated video stream parameters.
    fn params(&self) -> StreamParams;

    /// Negotiated audio stream parameters, if audio capture is active.
    fn audio_params(&self) -> Option<AudioParams> {
        None
    }

    /// Whether capture is currently running.
    fn is_running(&self) -> bool;
}

/// A deterministic, GPU-free backend for tests and development.
///
/// Emits `total_frames` frames at `fps`, marking every `keyframe_interval`-th
/// frame (starting at frame 0) as a keyframe. pts/dts are evenly spaced in
/// microseconds. All frames are produced up-front into the channel on `start`,
/// so consumers can drain deterministically with no timing dependence.
#[derive(Debug, Clone)]
pub struct MockBackend {
    params: StreamParams,
    total_frames: u32,
    keyframe_interval: u32,
    frame_bytes: usize,
    audio: bool,
    running: bool,
}

impl MockBackend {
    /// Build a mock emitting `total_frames` at `fps`, keyframe every
    /// `keyframe_interval` frames (must be >= 1). No audio by default.
    pub fn new(fps: u32, total_frames: u32, keyframe_interval: u32) -> Self {
        debug_assert!(fps >= 1);
        debug_assert!(keyframe_interval >= 1);
        Self {
            params: StreamParams {
                width: 2560,
                height: 1440,
                fps,
                codec: Codec::H264,
                time_base_den: MICROS_PER_SEC, // mock pts are microseconds
            },
            total_frames,
            keyframe_interval,
            frame_bytes: 32,
            audio: false,
            running: false,
        }
    }

    /// Also emit deterministic audio frames (one per 20 ms) spanning the same
    /// time as the video, so the audio path is testable without a GPU.
    pub fn with_audio(mut self) -> Self {
        self.audio = true;
        self
    }

    /// Set the per-frame encoded payload size. The default (32 bytes) keeps unit
    /// tests cheap; benchmarks set a realistic size (tens of KB) so the clip-copy
    /// cost is representative of a live capture.
    pub fn with_frame_bytes(mut self, bytes: usize) -> Self {
        self.frame_bytes = bytes;
        self
    }

    /// Microsecond spacing between video frames.
    fn frame_interval_micros(&self) -> Micros {
        MICROS_PER_SEC / self.params.fps as i64
    }

    /// Total captured span in microseconds.
    fn span_micros(&self) -> Micros {
        self.total_frames.saturating_sub(1) as i64 * self.frame_interval_micros()
    }
}

impl CaptureBackend for MockBackend {
    fn start(&mut self) -> Result<CaptureStreams, BackendError> {
        if self.running {
            return Err(BackendError::AlreadyRunning);
        }
        let (vtx, vrx) = mpsc::channel();
        let step = self.frame_interval_micros();
        for i in 0..self.total_frames {
            let pts = i as i64 * step;
            let is_keyframe = i % self.keyframe_interval == 0;
            let frame = EncodedFrame::new(vec![i as u8; self.frame_bytes], is_keyframe, pts, pts);
            // The receiver is alive in this scope; send cannot fail here.
            let _ = vtx.send(frame);
        }

        let audio = if self.audio {
            let (atx, arx) = mpsc::channel();
            // One 20 ms Opus-ish frame (960 samples @ 48k) across the span.
            let frame_us = 20_000;
            let span = self.span_micros();
            let mut ts = 0;
            let mut pts = 0;
            while ts <= span {
                let _ = atx.send(EncodedAudioFrame::new(vec![0u8; 16], pts, ts));
                ts += frame_us;
                pts += 960; // samples per 20ms @ 48kHz
            }
            Some(arx)
        } else {
            None
        };

        self.running = true;
        Ok(CaptureStreams { video: vrx, audio })
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        if !self.running {
            return Err(BackendError::NotRunning);
        }
        self.running = false;
        Ok(())
    }

    fn params(&self) -> StreamParams {
        self.params
    }

    fn audio_params(&self) -> Option<AudioParams> {
        self.audio.then_some(AudioParams {
            sample_rate: 48_000,
            channels: 2,
            codec: AudioCodec::Opus,
        })
    }

    fn is_running(&self) -> bool {
        self.running
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_emits_expected_frame_count() {
        let mut b = MockBackend::new(60, 120, 60);
        let streams = b.start().unwrap();
        let frames: Vec<_> = streams.video.iter().collect();
        assert_eq!(frames.len(), 120);
        assert!(b.is_running());
        assert!(streams.audio.is_none());
    }

    #[test]
    fn mock_keyframe_cadence() {
        let mut b = MockBackend::new(60, 10, 3);
        let streams = b.start().unwrap();
        let frames: Vec<_> = streams.video.iter().collect();
        let kf: Vec<usize> = frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.is_keyframe)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(kf, vec![0, 3, 6, 9]);
    }

    #[test]
    fn mock_pts_evenly_spaced() {
        let mut b = MockBackend::new(50, 3, 1); // 50fps -> 20_000us step
        let streams = b.start().unwrap();
        let frames: Vec<_> = streams.video.iter().collect();
        assert_eq!(frames[0].pts, 0);
        assert_eq!(frames[1].pts, 20_000);
        assert_eq!(frames[2].pts, 40_000);
    }

    #[test]
    fn mock_emits_audio_when_enabled() {
        // 60fps, 120 frames = ~2s span -> ~100 audio frames at 20ms each.
        let mut b = MockBackend::new(60, 120, 60).with_audio();
        let streams = b.start().unwrap();
        let audio: Vec<_> = streams.audio.expect("audio enabled").iter().collect();
        assert!(
            audio.len() >= 90 && audio.len() <= 110,
            "got {}",
            audio.len()
        );
        assert_eq!(audio[0].timestamp_micros, 0);
        assert_eq!(audio[1].timestamp_micros, 20_000);
        let params = b.audio_params().unwrap();
        assert_eq!(params.sample_rate, 48_000);
        assert_eq!(params.channels, 2);
    }

    #[test]
    fn double_start_errors() {
        let mut b = MockBackend::new(60, 1, 1);
        let _ = b.start().unwrap();
        assert_eq!(b.start().err(), Some(BackendError::AlreadyRunning));
    }

    #[test]
    fn stop_without_start_errors() {
        let mut b = MockBackend::new(60, 1, 1);
        assert_eq!(b.stop().err(), Some(BackendError::NotRunning));
    }

    #[test]
    fn params_reported() {
        let b = MockBackend::new(60, 1, 1);
        assert_eq!(b.params().fps, 60);
        assert_eq!(b.params().codec, Codec::H264);
        assert_eq!(b.params().codec.as_str(), "h264");
        assert!(b.audio_params().is_none());
    }
}
