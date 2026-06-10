//! The replay-buffer storage seam.
//!
//! [`FrameStore`] abstracts *where* the encoded replay window lives so the
//! engine and clip selection don't care. [`RingBuffer`](crate::ring::RingBuffer)
//! is the in-RAM implementation; a disk-backed store (larger windows, low-RAM
//! boxes — gpu-screen-recorder's `-replay-storage disk`) implements the same
//! trait with an in-RAM metadata index and spilled payloads.
//!
//! The contract is deliberately copy-out, not borrow-in: [`FrameStore::scan`]
//! yields cheap metadata for selection math, and [`FrameStore::window`]
//! materializes the chosen range. For the RAM store that materialization is a
//! refcount bump per frame; a disk store reads payloads back only for the
//! frames actually saved.

use crate::ring::{EncodedFrame, RingBuffer};
use crate::Ticks;

/// Per-frame metadata, enough for keyframe-aware clip selection without
/// touching payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    /// Position in the store, oldest = 0.
    pub index: usize,
    /// Presentation timestamp in the store's tick base.
    pub pts: Ticks,
    /// Whether the frame is a keyframe (a legal clip start).
    pub is_keyframe: bool,
}

/// A bounded, time-windowed store of encoded frames.
///
/// Implementations must keep frames ordered by pts (oldest first) and evict
/// beyond `capacity_seconds`, exactly like [`RingBuffer`]. `push` sits on the
/// capture drain path: it must never block on I/O — a disk store stages writes
/// asynchronously.
pub trait FrameStore: Send {
    /// Insert a frame in pts order and evict frames older than the capacity
    /// window behind the newest pts seen.
    fn push(&mut self, frame: EncodedFrame);

    /// Drop all buffered frames and reset the eviction anchor.
    fn clear(&mut self);

    /// Change the capacity window, evicting immediately if needed.
    fn set_capacity_seconds(&mut self, capacity_seconds: u32);

    /// Number of frames currently buffered.
    fn len(&self) -> usize;

    /// Whether the store holds no frames.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total encoded payload bytes currently buffered.
    fn bytes(&self) -> usize;

    /// Ticks per second of the frame pts (the time-base denominator).
    fn ticks_per_sec(&self) -> i64;

    /// The configured capacity in whole seconds.
    fn capacity_seconds(&self) -> u32;

    /// Buffered span rounded down to whole seconds (for status reporting).
    fn buffered_seconds(&self) -> u32;

    /// pts of the newest buffered frame, if any.
    fn newest_pts(&self) -> Option<Ticks>;

    /// pts of the oldest buffered frame, if any.
    fn oldest_pts(&self) -> Option<Ticks>;

    /// Metadata for every buffered frame, oldest first. Must be cheap (no
    /// payload access).
    fn scan(&self) -> Box<dyn Iterator<Item = FrameMeta> + '_>;

    /// Materialize `count` frames starting at `start` (oldest = 0), in order.
    /// For the RAM store this is a refcount bump per frame, never a copy.
    fn window(&self, start: usize, count: usize) -> Vec<EncodedFrame>;
}

impl FrameStore for RingBuffer {
    fn push(&mut self, frame: EncodedFrame) {
        RingBuffer::push(self, frame)
    }

    fn clear(&mut self) {
        RingBuffer::clear(self)
    }

    fn set_capacity_seconds(&mut self, capacity_seconds: u32) {
        RingBuffer::set_capacity_seconds(self, capacity_seconds)
    }

    fn len(&self) -> usize {
        RingBuffer::len(self)
    }

    fn bytes(&self) -> usize {
        RingBuffer::bytes(self)
    }

    fn ticks_per_sec(&self) -> i64 {
        RingBuffer::ticks_per_sec(self)
    }

    fn capacity_seconds(&self) -> u32 {
        RingBuffer::capacity_seconds(self)
    }

    fn buffered_seconds(&self) -> u32 {
        RingBuffer::buffered_seconds(self)
    }

    fn newest_pts(&self) -> Option<Ticks> {
        RingBuffer::newest_pts(self)
    }

    fn oldest_pts(&self) -> Option<Ticks> {
        RingBuffer::oldest_pts(self)
    }

    fn scan(&self) -> Box<dyn Iterator<Item = FrameMeta> + '_> {
        Box::new(self.frames().enumerate().map(|(index, f)| FrameMeta {
            index,
            pts: f.pts,
            is_keyframe: f.is_keyframe,
        }))
    }

    fn window(&self, start: usize, count: usize) -> Vec<EncodedFrame> {
        self.frames().skip(start).take(count).cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MICROS_PER_SEC;

    fn frame(sec: f64, keyframe: bool) -> EncodedFrame {
        let pts = (sec * MICROS_PER_SEC as f64) as i64;
        EncodedFrame::new(vec![0u8; 10], keyframe, pts, pts)
    }

    #[test]
    fn ring_buffer_satisfies_store_contract() {
        let mut rb = RingBuffer::new(60);
        FrameStore::push(&mut rb, frame(0.0, true));
        FrameStore::push(&mut rb, frame(1.0, false));

        let store: &dyn FrameStore = &rb;
        assert_eq!(store.len(), 2);
        assert_eq!(store.bytes(), 20);
        assert_eq!(store.newest_pts(), Some(MICROS_PER_SEC));
        let meta: Vec<FrameMeta> = store.scan().collect();
        assert_eq!(meta.len(), 2);
        assert_eq!(meta[0].index, 0);
        assert!(meta[0].is_keyframe);
        assert_eq!(meta[1].pts, MICROS_PER_SEC);

        let win = store.window(1, 1);
        assert_eq!(win.len(), 1);
        assert_eq!(win[0].pts, MICROS_PER_SEC);
    }

    #[test]
    fn window_is_refcount_not_copy() {
        let mut rb = RingBuffer::new(60);
        FrameStore::push(&mut rb, frame(0.0, true));
        let win = FrameStore::window(&rb, 0, 1);
        let original = rb.frames().next().unwrap();
        // Same allocation: materializing a window must not copy payloads.
        assert_eq!(win[0].data.as_ptr(), original.data.as_ptr());
    }
}
