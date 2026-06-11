//! Engine: drains a [`CaptureBackend`] into the [`RingBuffer`] and turns a
//! "save last N" request into a selected, ordered set of frames ready to mux.
//!
//! This layer is GPU-free and fully testable with [`MockBackend`]. The daemon
//! owns an `Engine` and calls [`Engine::drain_available`] as frames arrive and
//! [`Engine::take_clip`] on a save request.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use ord_common::ClipDuration;

use crate::audio::{AudioParams, AudioRingBuffer, EncodedAudioFrame};
use crate::backend::{BackendError, CaptureBackend, StreamParams};
use crate::clip::{select_clip, ClipError};
use crate::mux::MuxError;
use crate::record::Recorder;
use crate::ring::{EncodedFrame, RingBuffer};
use crate::store::FrameStore;

/// A clip ready to be muxed: ordered encoded video frames (first is a keyframe),
/// the audio frames covering the same window (may be empty), and the stream
/// params they were captured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedClip {
    pub frames: Vec<EncodedFrame>,
    pub audio: Vec<EncodedAudioFrame>,
    pub params: StreamParams,
    pub audio_params: Option<AudioParams>,
    /// Marker positions inside this clip, as milliseconds from the clip start.
    /// Written as MKV chapters by the muxer; empty for unmarked clips.
    pub chapters: Vec<i64>,
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

/// Drives capture into the replay store and produces clips on demand.
///
/// Generic over the [`FrameStore`] so the replay window can live in RAM (the
/// default [`RingBuffer`]) or, in the future, on disk — without the engine,
/// daemon, or selection logic changing.
pub struct Engine<B: CaptureBackend, S: FrameStore = RingBuffer> {
    backend: B,
    ring: S,
    audio_ring: AudioRingBuffer,
    rx: Option<Receiver<EncodedFrame>>,
    audio_rx: Option<Receiver<EncodedAudioFrame>>,
    /// Active full-length recording, if any. Frames are tee'd here from
    /// `drain_available` in addition to the replay ring.
    recorder: Option<Recorder>,
    /// Marker positions ("clip that" bookmarks) in the video pts tick base,
    /// oldest first. Pruned alongside the ring's eviction window.
    markers: Vec<i64>,
}

impl<B: CaptureBackend> Engine<B> {
    /// Create an engine over `backend` with a `capacity_seconds` in-RAM replay
    /// buffer. The ring buffer uses the backend's pts time base so eviction
    /// keeps exactly `capacity_seconds` of footage regardless of whether pts
    /// are micro- or nanoseconds.
    pub fn new(backend: B, capacity_seconds: u32) -> Self {
        let ticks_per_sec = backend.params().time_base_den;
        Self::with_store(
            backend,
            RingBuffer::with_time_base(capacity_seconds, ticks_per_sec),
            capacity_seconds,
        )
    }
}

impl<B: CaptureBackend, S: FrameStore> Engine<B, S> {
    /// Create an engine over `backend` with a caller-provided replay store
    /// (`capacity_seconds` sizes the audio ring to match).
    pub fn with_store(backend: B, store: S, capacity_seconds: u32) -> Self {
        Self {
            ring: store,
            audio_ring: AudioRingBuffer::new(capacity_seconds),
            backend,
            rx: None,
            audio_rx: None,
            recorder: None,
            markers: Vec::new(),
        }
    }

    /// Begin a full-length recording to `path`, tee'd from the live capture in
    /// addition to the replay ring. The file starts at the next keyframe.
    pub fn start_recording(&mut self, path: PathBuf) -> Result<(), MuxError> {
        if self.recorder.is_some() {
            return Ok(());
        }
        let mut rec = Recorder::start(&path, self.backend.params(), self.backend.audio_params())?;
        // Seed the recorder's audio preroll from the replay ring: the audio
        // matching the upcoming first keyframe was captured (and pumped)
        // BEFORE this call — NVENC emits that keyframe with encode latency —
        // so without the backlog the recording would start with a silent hole
        // of roughly one pump interval plus the encoder delay. Refcounted
        // clones, no packet copies.
        let newest = self.audio_ring.newest_timestamp_micros();
        if let Some(end) = newest {
            const PREROLL_US: i64 = 5_000_000;
            for frame in self
                .audio_ring
                .select_window(end.saturating_sub(PREROLL_US), end)
            {
                rec.push_audio(&frame)?;
            }
        }
        self.recorder = Some(rec);
        Ok(())
    }

    /// Finalize the active recording and return its path. `None` if not recording.
    pub fn stop_recording(&mut self) -> Option<Result<PathBuf, MuxError>> {
        self.recorder.take().map(|r| r.finish())
    }

    /// Whether a full-length recording is currently active.
    pub fn is_recording(&self) -> bool {
        self.recorder.is_some()
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

    /// Restart the capture session with the same backend (watchdog recovery
    /// after a stall — suspend/resume, monitor change). Buffered frames are
    /// retained; the backend renegotiates its streams.
    pub fn restart(&mut self) -> Result<(), BackendError> {
        match self.stop() {
            Ok(()) | Err(BackendError::NotRunning) => {}
            Err(e) => return Err(e),
        }
        self.start()
    }

    /// Resize the replay window (video + audio) to `capacity_seconds`,
    /// evicting immediately when shrinking. Capture keeps running.
    pub fn set_capacity_seconds(&mut self, capacity_seconds: u32) {
        self.ring.set_capacity_seconds(capacity_seconds);
        self.audio_ring.set_capacity_seconds(capacity_seconds);
    }

    /// Place a marker at the newest buffered frame. Returns `false` (and
    /// records nothing) when the buffer is empty — there is nothing to mark.
    /// Markers within a later save's window become MKV chapters.
    pub fn mark(&mut self) -> bool {
        match self.ring.newest_pts() {
            Some(pts) => {
                self.markers.push(pts);
                true
            }
            None => false,
        }
    }

    /// Markers currently inside the buffered window (diagnostic).
    pub fn marker_count(&self) -> usize {
        self.markers.len()
    }

    /// Drop markers that have been evicted out of the buffered window.
    fn prune_markers(&mut self) {
        let oldest = self.ring.oldest_pts().unwrap_or(i64::MAX);
        self.markers.retain(|&m| m >= oldest);
    }

    /// Pull all currently-available video+audio frames from the backend into the
    /// ring buffers. Returns how many video frames were ingested. Non-blocking.
    pub fn drain_available(&mut self) -> usize {
        if let Some(arx) = self.audio_rx.as_ref() {
            // Collect first so the recorder (which needs `&mut self`) can be fed
            // without holding the receiver borrow.
            let frames: Vec<EncodedAudioFrame> = arx.try_iter().collect();
            for frame in frames {
                if let Some(rec) = self.recorder.as_mut() {
                    if let Err(e) = rec.push_audio(&frame) {
                        tracing::error!(error = %e, "recording audio write failed; stopping recording");
                        self.recorder = None;
                    }
                }
                self.audio_ring.push(frame);
            }
        }
        let frames: Vec<EncodedFrame> = match self.rx.as_ref() {
            Some(rx) => rx.try_iter().collect(),
            None => return 0,
        };
        let n = frames.len();
        for frame in frames {
            if let Some(rec) = self.recorder.as_mut() {
                if let Err(e) = rec.push_video(&frame) {
                    tracing::error!(error = %e, "recording video write failed; stopping recording");
                    self.recorder = None;
                }
            }
            self.ring.push(frame);
        }
        if n > 0 && !self.markers.is_empty() {
            self.prune_markers();
        }
        n
    }

    /// The configured replay-buffer capacity in whole seconds.
    pub fn capacity_seconds(&self) -> u32 {
        self.ring.capacity_seconds()
    }

    /// Select and copy the last `duration` of buffered frames into a
    /// [`PreparedClip`] (the clip starts on a keyframe). The audio track covering
    /// the same time window is included. The ring buffers are left intact.
    pub fn take_clip(&self, duration: ClipDuration) -> Result<PreparedClip, ClipError> {
        let selection = select_clip(&self.ring, duration.get())?;
        let frames = self
            .ring
            .window(selection.start_index, selection.frame_count);

        // Map the video window (in the video pts time base) to microseconds and
        // pull the audio frames that fall inside it, keeping A/V aligned. The
        // conversion is 128-bit-safe: waycap pts are raw monotonic nanoseconds.
        let den = self.backend.params().time_base_den;
        let start_us = crate::ticks_to_micros(selection.start_pts, den);
        let end_us = crate::ticks_to_micros(selection.end_pts, den);
        let audio = self.audio_ring.select_window(start_us, end_us);

        // Markers inside the clip window, rebased to milliseconds from the
        // clip start (the muxer's chapter time base).
        let chapters: Vec<i64> = self
            .markers
            .iter()
            .filter(|&&m| m >= selection.start_pts && m <= selection.end_pts)
            .map(|&m| (m - selection.start_pts) * 1000 / den.max(1))
            .collect();

        Ok(PreparedClip {
            frames,
            audio,
            params: self.backend.params(),
            audio_params: self.backend.audio_params(),
            chapters,
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
        self.ring.scan().filter(|m| m.is_keyframe).count()
    }

    /// Whether capture is running.
    pub fn is_running(&self) -> bool {
        self.backend.is_running()
    }

    /// Drop all buffered frames (video + audio) and any markers.
    pub fn clear(&mut self) {
        self.ring.clear();
        self.audio_ring.clear();
        self.markers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;
    use crate::MICROS_PER_SEC;

    /// Shorthand for a clip duration in tests.
    fn cd(seconds: u32) -> ClipDuration {
        ClipDuration::new(seconds).unwrap()
    }

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
        let clip = eng.take_clip(cd(3)).unwrap();
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
        let clip = eng.take_clip(cd(3)).unwrap();
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
        assert_eq!(eng.take_clip(cd(3)), Err(ClipError::EmptyBuffer));
    }

    #[test]
    fn clip_does_not_consume_buffer() {
        let mut eng = Engine::new(MockBackend::new(60, 300, 60), 60);
        eng.start().unwrap();
        eng.drain_available();
        let a = eng.take_clip(cd(2)).unwrap();
        let b = eng.take_clip(cd(2)).unwrap();
        assert_eq!(a, b); // replay buffer intact across saves
    }

    #[test]
    fn mark_lands_as_chapter_in_clip() {
        // 60fps, 600 frames = 10s, keyframe every second.
        let mut eng = Engine::new(MockBackend::new(60, 600, 60), 60);
        eng.start().unwrap();
        eng.drain_available();
        assert!(eng.mark()); // marker at newest pts (~9.98s)
        let clip = eng.take_clip(cd(3)).unwrap();
        assert_eq!(clip.chapters.len(), 1);
        // Chapter is inside the clip and near its end (clip covers >= last 3s).
        let span_ms = clip.span_ticks() * 1000 / clip.params.time_base_den;
        assert!(clip.chapters[0] <= span_ms);
        assert!(clip.chapters[0] >= span_ms - 1000, "{:?}", clip.chapters);
    }

    #[test]
    fn mark_on_empty_buffer_is_rejected() {
        let mut eng = Engine::new(MockBackend::new(60, 0, 1), 60);
        assert!(!eng.mark());
        assert_eq!(eng.marker_count(), 0);
    }

    #[test]
    fn markers_outside_window_are_excluded() {
        let mut eng = Engine::new(MockBackend::new(60, 600, 60), 60);
        eng.start().unwrap();
        // Drain in two stages: mark after the first half only.
        eng.drain_available();
        // Marker at 10s-end; ask for a clip of the last 2s -> marker at end is in.
        eng.mark();
        let clip = eng.take_clip(cd(2)).unwrap();
        assert_eq!(clip.chapters.len(), 1);
        // Clear wipes markers with the buffer.
        eng.clear();
        assert_eq!(eng.marker_count(), 0);
    }

    #[test]
    fn capacity_resize_evicts_immediately() {
        // 10s of footage in a 60s ring, then shrink to 3s.
        let mut eng = Engine::new(MockBackend::new(60, 600, 60), 60);
        eng.start().unwrap();
        eng.drain_available();
        assert!(eng.buffered_seconds() >= 9);
        eng.set_capacity_seconds(3);
        assert!(eng.buffered_seconds() <= 3, "{}", eng.buffered_seconds());
        assert_eq!(eng.capacity_seconds(), 3);
    }

    #[test]
    fn restart_recovers_capture() {
        let mut eng = Engine::new(MockBackend::new(60, 120, 60), 60);
        eng.start().unwrap();
        eng.drain_available();
        eng.restart().unwrap();
        assert!(eng.is_running());
        // The restarted mock emits a fresh batch of frames.
        assert_eq!(eng.drain_available(), 120);
        // Restarting from stopped also works (stop is tolerated).
        eng.stop().unwrap();
        eng.restart().unwrap();
        assert!(eng.is_running());
    }

    #[test]
    fn stop_retains_buffer() {
        let mut eng = Engine::new(MockBackend::new(60, 120, 60), 60);
        eng.start().unwrap();
        eng.drain_available();
        eng.stop().unwrap();
        assert!(!eng.is_running());
        // Buffer still holds frames -> clip still works.
        assert!(eng.take_clip(cd(1)).is_ok());
    }
}
