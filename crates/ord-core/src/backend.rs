//! The capture/encode backend seam.
//!
//! A [`CaptureBackend`] produces a stream of **encoded** video frames from a
//! capture target. The real implementation (waycap-rs/NVENC) lives behind a
//! feature flag; [`MockBackend`] provides deterministic frames so the ring
//! buffer, clip selection, daemon, and engine can all be tested without a GPU or
//! a live Wayland session (per AGENTS.md, a mock is mandatory).

use std::sync::mpsc::{self, Receiver};

use crate::ring::EncodedFrame;
use crate::{Micros, MICROS_PER_SEC};

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
    /// Begin capturing. Encoded frames are delivered on the returned receiver
    /// until [`stop`](CaptureBackend::stop) is called.
    fn start(&mut self) -> Result<Receiver<EncodedFrame>, BackendError>;

    /// Stop capturing and release resources.
    fn stop(&mut self) -> Result<(), BackendError>;

    /// Negotiated stream parameters.
    fn params(&self) -> StreamParams;

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
    running: bool,
}

impl MockBackend {
    /// Build a mock emitting `total_frames` at `fps`, keyframe every
    /// `keyframe_interval` frames (must be >= 1).
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
            running: false,
        }
    }

    /// Microsecond spacing between frames.
    fn frame_interval_micros(&self) -> Micros {
        MICROS_PER_SEC / self.params.fps as i64
    }
}

impl CaptureBackend for MockBackend {
    fn start(&mut self) -> Result<Receiver<EncodedFrame>, BackendError> {
        if self.running {
            return Err(BackendError::AlreadyRunning);
        }
        let (tx, rx) = mpsc::channel();
        let step = self.frame_interval_micros();
        for i in 0..self.total_frames {
            let pts = i as i64 * step;
            let is_keyframe = i % self.keyframe_interval == 0;
            let frame = EncodedFrame::new(vec![i as u8; self.frame_bytes], is_keyframe, pts, pts);
            // The receiver is alive in this scope; send cannot fail here.
            let _ = tx.send(frame);
        }
        self.running = true;
        Ok(rx)
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
        let rx = b.start().unwrap();
        let frames: Vec<_> = rx.iter().collect();
        assert_eq!(frames.len(), 120);
        assert!(b.is_running());
    }

    #[test]
    fn mock_keyframe_cadence() {
        let mut b = MockBackend::new(60, 10, 3);
        let rx = b.start().unwrap();
        let frames: Vec<_> = rx.iter().collect();
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
        let rx = b.start().unwrap();
        let frames: Vec<_> = rx.iter().collect();
        assert_eq!(frames[0].pts, 0);
        assert_eq!(frames[1].pts, 20_000);
        assert_eq!(frames[2].pts, 40_000);
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
    }
}
