//! The encoded-frame ring buffer.
//!
//! Holds the last `capacity` seconds of **encoded** video packets in RAM. Frames
//! are pushed in capture order (monotonically non-decreasing `pts`). When the
//! span from the oldest frame to the newest exceeds the capacity, the oldest
//! frames are evicted.
//!
//! Eviction is span-based (time), not count-based: what matters is "keep the last
//! N seconds", and frame rate may vary.

use std::collections::VecDeque;

use bytes::Bytes;

use crate::{Micros, MICROS_PER_SEC};

/// One encoded video frame in the buffer. Mirrors the fields we need from
/// `waycap_rs::types::video_frame::EncodedVideoFrame`.
///
/// The payload is a [`Bytes`] handle, not an owned `Vec<u8>`: clip selection
/// (`take_clip`) clones the selected window on every save, and a `Bytes` clone is
/// an atomic refcount bump rather than a copy of the encoded packet. Building one
/// from the `Vec<u8>` the encoder hands us is O(1) (it takes ownership).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Encoded packet bytes.
    pub data: Bytes,
    /// Whether this frame is a keyframe (IDR). Required for clip selection.
    pub is_keyframe: bool,
    /// Presentation timestamp, microseconds.
    pub pts: Micros,
    /// Decode timestamp, microseconds.
    pub dts: Micros,
}

impl EncodedFrame {
    /// Convenience constructor. Accepts anything convertible into [`Bytes`]
    /// (e.g. a `Vec<u8>` from the encoder, which is moved in without copying).
    pub fn new(data: impl Into<Bytes>, is_keyframe: bool, pts: Micros, dts: Micros) -> Self {
        Self {
            data: data.into(),
            is_keyframe,
            pts,
            dts,
        }
    }

    /// Encoded size in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the frame carries no data.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// A bounded, time-windowed buffer of encoded frames.
#[derive(Debug)]
pub struct RingBuffer {
    frames: VecDeque<EncodedFrame>,
    capacity_ticks: Micros,
    ticks_per_sec: i64,
    max_pts: Micros,
    bytes: usize,
}

impl RingBuffer {
    /// Create a buffer holding at most `capacity_seconds` of frames, with frame
    /// `pts` expressed in **microseconds** (the default used by the mock backend
    /// and tests). `capacity_seconds` must be >= 1.
    pub fn new(capacity_seconds: u32) -> Self {
        Self::with_time_base(capacity_seconds, MICROS_PER_SEC)
    }

    /// Create a buffer where frame `pts` are expressed in `ticks_per_sec` units
    /// (e.g. `NANOS_PER_SEC` for waycap-rs). This is essential: eviction is a
    /// time window, so the buffer must know the pts time base or it will keep the
    /// wrong amount of footage.
    pub fn with_time_base(capacity_seconds: u32, ticks_per_sec: i64) -> Self {
        debug_assert!(capacity_seconds >= 1, "buffer capacity must be >= 1s");
        debug_assert!(ticks_per_sec >= 1);
        Self {
            frames: VecDeque::new(),
            capacity_ticks: capacity_seconds as i64 * ticks_per_sec,
            ticks_per_sec,
            max_pts: i64::MIN,
            bytes: 0,
        }
    }

    /// Push a frame, then evict any frames older than `capacity` behind the
    /// newest seen pts.
    ///
    /// Frames may arrive slightly out of order (waycap-rs reorders for B-frames),
    /// so the eviction window is anchored to the maximum pts seen, and frames are
    /// inserted by pts to keep the buffer ordered.
    pub fn push(&mut self, frame: EncodedFrame) {
        self.max_pts = self.max_pts.max(frame.pts);
        self.bytes += frame.len();
        // Insert keeping the deque ordered by pts (frames usually arrive in
        // order; this corrects the occasional reorder without dropping frames).
        if self
            .frames
            .back()
            .map(|b| frame.pts >= b.pts)
            .unwrap_or(true)
        {
            self.frames.push_back(frame);
        } else {
            let pos = self
                .frames
                .iter()
                .position(|f| f.pts > frame.pts)
                .unwrap_or(self.frames.len());
            self.frames.insert(pos, frame);
        }
        self.evict_before(self.max_pts - self.capacity_ticks);
    }

    /// Remove frames whose pts is strictly less than `cutoff`.
    fn evict_before(&mut self, cutoff: Micros) {
        while let Some(front) = self.frames.front() {
            if front.pts >= cutoff {
                break;
            }
            // `front()` just confirmed an element; the hot path must not panic, so
            // fold the pop into the same branch rather than `.expect`.
            if let Some(removed) = self.frames.pop_front() {
                self.bytes -= removed.len();
            }
        }
    }

    /// Number of frames currently buffered.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Total encoded bytes currently buffered.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The span (in pts ticks) from the oldest to the newest buffered frame.
    /// Zero if fewer than two frames.
    pub fn span_ticks(&self) -> Micros {
        match (self.frames.front(), self.frames.back()) {
            (Some(f), Some(b)) => b.pts - f.pts,
            _ => 0,
        }
    }

    /// Buffered span rounded down to whole seconds (for status reporting).
    pub fn buffered_seconds(&self) -> u32 {
        (self.span_ticks() / self.ticks_per_sec) as u32
    }

    /// Ticks per second for this buffer's frame pts (the time-base denominator).
    pub fn ticks_per_sec(&self) -> i64 {
        self.ticks_per_sec
    }

    /// The pts of the newest buffered frame, if any.
    pub fn newest_pts(&self) -> Option<Micros> {
        self.frames.back().map(|f| f.pts)
    }

    /// The pts of the oldest buffered frame, if any.
    pub fn oldest_pts(&self) -> Option<Micros> {
        self.frames.front().map(|f| f.pts)
    }

    /// Read-only view of the buffered frames, oldest first.
    pub fn frames(&self) -> impl Iterator<Item = &EncodedFrame> {
        self.frames.iter()
    }

    /// Clear all buffered frames.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
        // Reset the eviction anchor too (audio's clear() already does). Otherwise
        // a stale `max_pts` survives the clear and a lower-pts epoch after a buffer
        // toggle (capture restart) is evicted on arrival — wiping the new buffer.
        self.max_pts = i64::MIN;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a frame at second `s` (pts in micros), keyframe optional, 10 bytes.
    fn f(sec: f64, keyframe: bool) -> EncodedFrame {
        let pts = (sec * MICROS_PER_SEC as f64) as i64;
        EncodedFrame::new(vec![0u8; 10], keyframe, pts, pts)
    }

    #[test]
    fn empty_buffer_state() {
        let rb = RingBuffer::new(60);
        assert!(rb.is_empty());
        assert_eq!(rb.len(), 0);
        assert_eq!(rb.bytes(), 0);
        assert_eq!(rb.span_ticks(), 0);
        assert_eq!(rb.buffered_seconds(), 0);
        assert_eq!(rb.newest_pts(), None);
        assert_eq!(rb.oldest_pts(), None);
    }

    #[test]
    fn push_accumulates_until_capacity() {
        let mut rb = RingBuffer::new(10);
        for i in 0..10 {
            rb.push(f(i as f64, i == 0));
        }
        // 0..=9s all within a 10s window behind newest (9s): cutoff = -1s.
        assert_eq!(rb.len(), 10);
        assert_eq!(rb.bytes(), 100);
        assert_eq!(rb.oldest_pts(), Some(0));
        assert_eq!(rb.newest_pts(), Some(9 * MICROS_PER_SEC));
    }

    #[test]
    fn eviction_drops_old_frames() {
        let mut rb = RingBuffer::new(5);
        // Push 0..=10s, one frame per second.
        for i in 0..=10 {
            rb.push(f(i as f64, false));
        }
        // Newest is 10s; cutoff = 5s; frames with pts < 5s are gone.
        // Remaining: 5,6,7,8,9,10 -> 6 frames.
        assert_eq!(rb.len(), 6);
        assert_eq!(rb.oldest_pts(), Some(5 * MICROS_PER_SEC));
        assert_eq!(rb.newest_pts(), Some(10 * MICROS_PER_SEC));
        assert_eq!(rb.bytes(), 60);
    }

    #[test]
    fn out_of_order_frame_is_inserted_in_order() {
        // waycap-rs can reorder frames (B-frames); the buffer inserts by pts
        // rather than dropping, keeping the deque ordered and the frame retained.
        let mut rb = RingBuffer::new(60);
        rb.push(f(5.0, true));
        rb.push(f(3.0, false)); // earlier than newest -> inserted before it
        assert_eq!(rb.len(), 2);
        let ptss: Vec<i64> = rb.frames().map(|f| f.pts).collect();
        assert_eq!(ptss, vec![3 * MICROS_PER_SEC, 5 * MICROS_PER_SEC]);
        // newest (max pts) is still 5s.
        assert_eq!(rb.newest_pts(), Some(5 * MICROS_PER_SEC));
    }

    #[test]
    fn single_frame_span_is_zero() {
        let mut rb = RingBuffer::new(60);
        rb.push(f(7.0, true));
        assert_eq!(rb.span_ticks(), 0);
        assert_eq!(rb.buffered_seconds(), 0);
    }

    #[test]
    fn clear_resets() {
        let mut rb = RingBuffer::new(60);
        rb.push(f(1.0, true));
        rb.push(f(2.0, false));
        rb.clear();
        assert!(rb.is_empty());
        assert_eq!(rb.bytes(), 0);
    }

    #[test]
    fn clear_resets_eviction_anchor() {
        // Regression: clear() must reset max_pts. Buffer a high-pts epoch, clear,
        // then push a fresh low-pts epoch (as a capture restart after a buffer
        // toggle delivers). Without the reset, the stale max_pts evicts the new
        // frames on arrival and the buffer stays empty.
        let mut rb = RingBuffer::new(60);
        rb.push(f(100.0, true)); // max_pts -> 100s
        rb.clear();
        rb.push(f(0.0, true));
        rb.push(f(1.0, false));
        assert_eq!(rb.len(), 2);
        assert_eq!(rb.oldest_pts(), Some(0));
        assert_eq!(rb.newest_pts(), Some(MICROS_PER_SEC));
    }

    #[test]
    fn buffered_seconds_floors() {
        let mut rb = RingBuffer::new(60);
        rb.push(f(0.0, true));
        rb.push(f(3.5, false));
        assert_eq!(rb.buffered_seconds(), 3);
    }
}
