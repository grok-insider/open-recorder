//! Engine: drains a [`CaptureBackend`] into the [`RingBuffer`] and turns a
//! "save last N" request into a selected, ordered set of frames ready to mux.
//!
//! This layer is GPU-free and fully testable with [`MockBackend`]. The daemon
//! owns an `Engine` and calls [`Engine::drain_available`] as frames arrive and
//! [`Engine::take_clip`] on a save request.

use std::sync::mpsc::Receiver;

use crate::backend::{BackendError, CaptureBackend, StreamParams};
use crate::clip::{select_clip, ClipError};
use crate::ring::{EncodedFrame, RingBuffer};

/// A clip ready to be muxed: ordered encoded frames (first is a keyframe) plus
/// the stream params they were captured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedClip {
    pub frames: Vec<EncodedFrame>,
    pub params: StreamParams,
}

impl PreparedClip {
    /// Covered span in microseconds (first to last frame pts).
    pub fn span_micros(&self) -> i64 {
        match (self.frames.first(), self.frames.last()) {
            (Some(a), Some(b)) => b.pts - a.pts,
            _ => 0,
        }
    }
}

/// Drives capture into the ring buffer and produces clips on demand.
pub struct Engine<B: CaptureBackend> {
    backend: B,
    ring: RingBuffer,
    rx: Option<Receiver<EncodedFrame>>,
}

impl<B: CaptureBackend> Engine<B> {
    /// Create an engine over `backend` with a `capacity_seconds` replay buffer.
    pub fn new(backend: B, capacity_seconds: u32) -> Self {
        Self {
            backend,
            ring: RingBuffer::new(capacity_seconds),
            rx: None,
        }
    }

    /// Start capture; frames begin flowing into the ring buffer on
    /// [`drain_available`](Engine::drain_available).
    pub fn start(&mut self) -> Result<(), BackendError> {
        let rx = self.backend.start()?;
        self.rx = Some(rx);
        Ok(())
    }

    /// Stop capture. Buffered frames are retained.
    pub fn stop(&mut self) -> Result<(), BackendError> {
        self.backend.stop()?;
        self.rx = None;
        Ok(())
    }

    /// Pull all currently-available frames from the backend into the ring buffer.
    /// Returns how many frames were ingested. Non-blocking.
    pub fn drain_available(&mut self) -> usize {
        let Some(rx) = self.rx.as_ref() else {
            return 0;
        };
        let mut n = 0;
        while let Ok(frame) = rx.try_recv() {
            self.ring.push(frame);
            n += 1;
        }
        n
    }

    /// Select and copy the last `seconds` of buffered frames into a
    /// [`PreparedClip`] (the clip starts on a keyframe). The ring buffer is left
    /// intact (replay continues).
    pub fn take_clip(&self, seconds: u32) -> Result<PreparedClip, ClipError> {
        let selection = select_clip(&self.ring, seconds)?;
        let frames: Vec<EncodedFrame> = self
            .ring
            .frames()
            .skip(selection.start_index)
            .take(selection.frame_count)
            .cloned()
            .collect();
        Ok(PreparedClip {
            frames,
            params: self.backend.params(),
        })
    }

    /// Whole seconds currently buffered (for status).
    pub fn buffered_seconds(&self) -> u32 {
        self.ring.buffered_seconds()
    }

    /// Whether capture is running.
    pub fn is_running(&self) -> bool {
        self.backend.is_running()
    }

    /// Drop all buffered frames.
    pub fn clear(&mut self) {
        self.ring.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;
    use crate::MICROS_PER_SEC;

    #[test]
    fn drains_frames_into_ring() {
        // 60fps, 120 frames = 2s of capture, keyframe every 60.
        let mut eng = Engine::new(MockBackend::new(60, 120, 60), 60);
        eng.start().unwrap();
        let n = eng.drain_available();
        assert_eq!(n, 120);
        assert!(eng.is_running());
    }

    #[test]
    fn take_clip_starts_on_keyframe() {
        // 60fps, 600 frames = 10s, keyframe every 60 frames (every 1s).
        let mut eng = Engine::new(MockBackend::new(60, 600, 60), 60);
        eng.start().unwrap();
        eng.drain_available();
        let clip = eng.take_clip(3).unwrap();
        assert!(clip.frames.first().unwrap().is_keyframe);
        // Covers at least the requested 3s.
        assert!(clip.span_micros() >= 3 * MICROS_PER_SEC);
        assert_eq!(clip.params.fps, 60);
    }

    #[test]
    fn take_clip_on_empty_errors() {
        let eng = Engine::new(MockBackend::new(60, 0, 1), 60);
        assert_eq!(eng.take_clip(3), Err(ClipError::EmptyBuffer));
    }

    #[test]
    fn clip_does_not_consume_buffer() {
        let mut eng = Engine::new(MockBackend::new(60, 300, 60), 60);
        eng.start().unwrap();
        eng.drain_available();
        let a = eng.take_clip(2).unwrap();
        let b = eng.take_clip(2).unwrap();
        assert_eq!(a, b); // replay buffer intact across saves
    }

    #[test]
    fn stop_retains_buffer() {
        let mut eng = Engine::new(MockBackend::new(60, 120, 60), 60);
        eng.start().unwrap();
        eng.drain_available();
        eng.stop().unwrap();
        assert!(!eng.is_running());
        // Buffer still holds frames -> clip still works.
        assert!(eng.take_clip(1).is_ok());
    }
}
