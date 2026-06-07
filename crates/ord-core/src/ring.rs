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

use crate::{Micros, MICROS_PER_SEC};

/// One encoded video frame in the buffer. Mirrors the fields we need from
/// `waycap_rs::types::video_frame::EncodedVideoFrame`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    /// Encoded packet bytes.
    pub data: Vec<u8>,
    /// Whether this frame is a keyframe (IDR). Required for clip selection.
    pub is_keyframe: bool,
    /// Presentation timestamp, microseconds.
    pub pts: Micros,
    /// Decode timestamp, microseconds.
    pub dts: Micros,
}

impl EncodedFrame {
    /// Convenience constructor.
    pub fn new(data: Vec<u8>, is_keyframe: bool, pts: Micros, dts: Micros) -> Self {
        Self {
            data,
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
    capacity_micros: Micros,
    bytes: usize,
}

impl RingBuffer {
    /// Create a buffer holding at most `capacity_seconds` of frames.
    /// `capacity_seconds` must be >= 1.
    pub fn new(capacity_seconds: u32) -> Self {
        debug_assert!(capacity_seconds >= 1, "buffer capacity must be >= 1s");
        Self {
            frames: VecDeque::new(),
            capacity_micros: capacity_seconds as i64 * MICROS_PER_SEC,
            bytes: 0,
        }
    }

    /// Push a frame, then evict any frames older than `capacity` behind the
    /// newest frame's pts.
    ///
    /// Frames are expected in non-decreasing pts order. A frame whose pts is
    /// before the current newest is dropped (out-of-order arrivals are not
    /// buffered), keeping the window well-formed.
    pub fn push(&mut self, frame: EncodedFrame) {
        if let Some(back) = self.frames.back() {
            if frame.pts < back.pts {
                return;
            }
        }
        let newest_pts = frame.pts;
        self.bytes += frame.len();
        self.frames.push_back(frame);
        self.evict_before(newest_pts - self.capacity_micros);
    }

    /// Remove frames whose pts is strictly less than `cutoff`.
    fn evict_before(&mut self, cutoff: Micros) {
        while let Some(front) = self.frames.front() {
            if front.pts < cutoff {
                let removed = self.frames.pop_front().expect("front exists");
                self.bytes -= removed.len();
            } else {
                break;
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

    /// The span (microseconds) from the oldest to the newest buffered frame.
    /// Zero if fewer than two frames.
    pub fn span_micros(&self) -> Micros {
        match (self.frames.front(), self.frames.back()) {
            (Some(f), Some(b)) => b.pts - f.pts,
            _ => 0,
        }
    }

    /// Buffered span rounded down to whole seconds (for status reporting).
    pub fn buffered_seconds(&self) -> u32 {
        (self.span_micros() / MICROS_PER_SEC) as u32
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
        assert_eq!(rb.span_micros(), 0);
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
    fn out_of_order_frame_is_dropped() {
        let mut rb = RingBuffer::new(60);
        rb.push(f(5.0, true));
        rb.push(f(3.0, false)); // earlier than newest -> dropped
        assert_eq!(rb.len(), 1);
        assert_eq!(rb.newest_pts(), Some(5 * MICROS_PER_SEC));
    }

    #[test]
    fn single_frame_span_is_zero() {
        let mut rb = RingBuffer::new(60);
        rb.push(f(7.0, true));
        assert_eq!(rb.span_micros(), 0);
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
    fn buffered_seconds_floors() {
        let mut rb = RingBuffer::new(60);
        rb.push(f(0.0, true));
        rb.push(f(3.5, false));
        assert_eq!(rb.buffered_seconds(), 3);
    }
}
