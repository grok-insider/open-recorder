//! Engine: drains a [`CaptureBackend`] into the [`RingBuffer`] and turns a
//! "save last N" request into a selected, ordered set of frames ready to mux.
//!
//! This layer is GPU-free and fully testable with [`MockBackend`]. The daemon
//! owns an `Engine` and calls [`Engine::drain_available`] as frames arrive and
//! [`Engine::take_clip`] on a save request.

use std::sync::mpsc::Receiver;

use crate::audio::{AudioParams, AudioRingBuffer, EncodedAudioFrame};
use crate::backend::{BackendError, CaptureBackend, StreamParams};
use crate::clip::{select_clip, ClipError};
use crate::ring::{EncodedFrame, RingBuffer};

/// A clip ready to be muxed: ordered encoded video frames (first is a keyframe),
/// the audio frames covering the same window (may be empty), and the stream
/// params they were captured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedClip {
    pub frames: Vec<EncodedFrame>,
    pub audio: Vec<EncodedAudioFrame>,
    pub params: StreamParams,
    pub audio_params: Option<AudioParams>,
}

impl PreparedClip {
    /// Covered span in the video pts time base (first to last frame pts).
    pub fn span_ticks(&self) -> i64 {
        match (self.frames.first(), self.frames.last()) {
            (Some(a), Some(b)) => b.pts - a.pts,
            _ => 0,
        }
    }

    /// Whether the clip carries an audio track.
    pub fn has_audio(&self) -> bool {
        !self.audio.is_empty() && self.audio_params.is_some()
    }
}

/// Drives capture into the ring buffers and produces clips on demand.
pub struct Engine<B: CaptureBackend> {
    backend: B,
    ring: RingBuffer,
    audio_ring: AudioRingBuffer,
    rx: Option<Receiver<EncodedFrame>>,
    audio_rx: Option<Receiver<EncodedAudioFrame>>,
}

impl<B: CaptureBackend> Engine<B> {
    /// Create an engine over `backend` with a `capacity_seconds` replay buffer.
    /// The ring buffer uses the backend's pts time base so eviction keeps exactly
    /// `capacity_seconds` of footage regardless of whether pts are micro- or
    /// nanoseconds.
    pub fn new(backend: B, capacity_seconds: u32) -> Self {
        let ticks_per_sec = backend.params().time_base_den;
        Self {
            ring: RingBuffer::with_time_base(capacity_seconds, ticks_per_sec),
            audio_ring: AudioRingBuffer::new(capacity_seconds),
            backend,
            rx: None,
            audio_rx: None,
        }
    }

    /// Start capture; frames begin flowing into the ring buffers on
    /// [`drain_available`](Engine::drain_available).
    pub fn start(&mut self) -> Result<(), BackendError> {
        let streams = self.backend.start()?;
        self.rx = Some(streams.video);
        self.audio_rx = streams.audio;
        Ok(())
    }

    /// Stop capture. Buffered frames are retained.
    pub fn stop(&mut self) -> Result<(), BackendError> {
        self.backend.stop()?;
        self.rx = None;
        self.audio_rx = None;
        Ok(())
    }

    /// Pull all currently-available video+audio frames from the backend into the
    /// ring buffers. Returns how many video frames were ingested. Non-blocking.
    pub fn drain_available(&mut self) -> usize {
        if let Some(arx) = self.audio_rx.as_ref() {
            while let Ok(frame) = arx.try_recv() {
                self.audio_ring.push(frame);
            }
        }
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
    /// [`PreparedClip`] (the clip starts on a keyframe). The audio track covering
    /// the same time window is included. The ring buffers are left intact.
    pub fn take_clip(&self, seconds: u32) -> Result<PreparedClip, ClipError> {
        let selection = select_clip(&self.ring, seconds)?;
        let frames: Vec<EncodedFrame> = self
            .ring
            .frames()
            .skip(selection.start_index)
            .take(selection.frame_count)
            .cloned()
            .collect();

        // Map the video window (in the video pts time base) to microseconds and
        // pull the audio frames that fall inside it, keeping A/V aligned. The
        // conversion is 128-bit-safe: waycap pts are raw monotonic nanoseconds.
        let den = self.backend.params().time_base_den;
        let start_us = crate::ticks_to_micros(selection.start_pts, den);
        let end_us = crate::ticks_to_micros(selection.end_pts, den);
        let audio = self.audio_ring.select_window(start_us, end_us);

        Ok(PreparedClip {
            frames,
            audio,
            params: self.backend.params(),
            audio_params: self.backend.audio_params(),
        })
    }

    /// Whole seconds currently buffered (for status).
    pub fn buffered_seconds(&self) -> u32 {
        self.ring.buffered_seconds()
    }

    /// Number of frames currently buffered (diagnostic).
    pub fn buffered_frames(&self) -> usize {
        self.ring.len()
    }

    /// Number of keyframes currently buffered. With few keyframes, "save last N"
    /// can only reach back to the newest one — useful for spotting GOP issues.
    pub fn buffered_keyframes(&self) -> usize {
        self.ring.frames().filter(|f| f.is_keyframe).count()
    }

    /// Whether capture is running.
    pub fn is_running(&self) -> bool {
        self.backend.is_running()
    }

    /// Drop all buffered frames (video + audio).
    pub fn clear(&mut self) {
        self.ring.clear();
        self.audio_ring.clear();
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
        assert!(clip.span_ticks() >= 3 * MICROS_PER_SEC);
        assert_eq!(clip.params.fps, 60);
        assert!(!clip.has_audio()); // no audio on a video-only mock
    }

    #[test]
    fn take_clip_includes_audio_window() {
        // 60fps, 600 frames = 10s, keyframe every 60 (1s), with audio.
        let mut eng = Engine::new(MockBackend::new(60, 600, 60).with_audio(), 60);
        eng.start().unwrap();
        eng.drain_available();
        let clip = eng.take_clip(3).unwrap();
        assert!(clip.has_audio());
        // ~3s of 20ms audio frames -> roughly 150 (allow slack for keyframe
        // window reaching back a little further).
        assert!(clip.audio.len() >= 140, "got {}", clip.audio.len());
        // Audio stays within the clip's microsecond window.
        let den = clip.params.time_base_den;
        let end_us = clip.frames.last().unwrap().pts * 1_000_000 / den;
        assert!(clip
            .audio
            .iter()
            .all(|a| a.timestamp_micros <= end_us + 20_000));
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
