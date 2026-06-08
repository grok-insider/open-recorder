//! Keyframe-aware "save the last N seconds" selection.
//!
//! This is the highest-risk logic in the project (per AGENTS.md). A saved clip
//! must be decodable via pure stream-copy (no re-encode), which means it MUST
//! begin on a keyframe. Given a desired window of the last `N` seconds ending at
//! the newest buffered frame, we select the range starting at the **newest
//! keyframe at or before the window start** and running to the end of the buffer.
//!
//! Consequence: the actual clip may be slightly longer than `N` (it reaches back
//! to the last keyframe before the window). That is correct and intended — it is
//! the shortest decodable clip that covers the requested window.

use crate::ring::{EncodedFrame, RingBuffer};
use crate::Micros;

/// The selected clip: indices into the buffer's frame sequence (oldest = 0) plus
/// derived timing. `start_index` always points at a keyframe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipSelection {
    /// Index of the first frame (a keyframe) to include.
    pub start_index: usize,
    /// Number of frames included (from `start_index` to the end of the buffer).
    pub frame_count: usize,
    /// pts of the first included frame.
    pub start_pts: Micros,
    /// pts of the last included frame.
    pub end_pts: Micros,
}

impl ClipSelection {
    /// The covered span in microseconds.
    pub fn span_micros(&self) -> Micros {
        self.end_pts - self.start_pts
    }
}

/// Why a clip could not be selected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClipError {
    /// The buffer holds no frames.
    #[error("the replay buffer is empty")]
    EmptyBuffer,
    /// No keyframe exists at or before the requested window start, so no
    /// decodable clip can be produced for the window.
    #[error("no keyframe found at or before the requested window")]
    NoKeyframeInWindow,
}

/// Select the frames to save for "the last `requested_seconds` seconds".
///
/// The window ends at the newest buffered frame and extends `requested_seconds`
/// back. The clip starts at the newest keyframe with pts <= the window start
/// (clamped to the oldest buffered frame), guaranteeing a decodable stream.
pub fn select_clip(
    buffer: &RingBuffer,
    requested_seconds: u32,
) -> Result<ClipSelection, ClipError> {
    let frames: Vec<&EncodedFrame> = buffer.frames().collect();
    if frames.is_empty() {
        return Err(ClipError::EmptyBuffer);
    }

    let newest_pts = frames.last().expect("non-empty").pts;
    // Convert the requested seconds into the buffer's pts time base (waycap uses
    // nanoseconds, the mock uses microseconds) — using the wrong base here makes
    // the window microscopic and the clip far too short.
    let requested_ticks = requested_seconds as i64 * buffer.ticks_per_sec();
    // The ideal start of the requested window (may be before the oldest frame).
    let window_start = newest_pts - requested_ticks;

    // Find the newest keyframe whose pts <= window_start. If none (the requested
    // window starts before the first keyframe), fall back to the oldest keyframe
    // in the buffer so we still produce a decodable clip covering as much of the
    // window as exists.
    let mut start_index: Option<usize> = None;
    for (i, frame) in frames.iter().enumerate() {
        if frame.is_keyframe && frame.pts <= window_start {
            start_index = Some(i);
        }
    }

    let start_index = match start_index {
        Some(i) => i,
        None => {
            // No keyframe at/before the window start. Use the earliest keyframe
            // available (covers part of the window). If there is no keyframe at
            // all, we cannot produce a decodable clip.
            frames
                .iter()
                .position(|f| f.is_keyframe)
                .ok_or(ClipError::NoKeyframeInWindow)?
        }
    };

    let start_pts = frames[start_index].pts;
    let end_pts = newest_pts;
    let frame_count = frames.len() - start_index;

    Ok(ClipSelection {
        start_index,
        frame_count,
        start_pts,
        end_pts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MICROS_PER_SEC;

    fn frame(sec: f64, keyframe: bool) -> EncodedFrame {
        let pts = (sec * MICROS_PER_SEC as f64) as i64;
        EncodedFrame::new(vec![0u8; 10], keyframe, pts, pts)
    }

    /// Build a buffer (capacity large enough not to evict) from (sec, keyframe).
    fn buffer_from(frames: &[(f64, bool)]) -> RingBuffer {
        let mut rb = RingBuffer::new(3600);
        for &(sec, kf) in frames {
            rb.push(frame(sec, kf));
        }
        rb
    }

    #[test]
    fn nanosecond_time_base_selects_correct_window() {
        // Regression: with a NANOSECOND-pts buffer (as waycap-rs delivers), a
        // "save last N seconds" must convert N into nanoseconds. A previous bug
        // used microseconds, making the window ~1000x too small so clips were a
        // fraction of a second. Build 10s of 60fps frames (nanos), keyframe every
        // 30 frames (~0.5s), and request the last 5s.
        let ns = crate::backend::NANOS_PER_SEC;
        let step = ns / 60;
        let mut rb = RingBuffer::with_time_base(60, ns);
        for i in 0..600 {
            let pts = i as i64 * step;
            rb.push(EncodedFrame::new(vec![0u8; 4], i % 30 == 0, pts, pts));
        }
        let sel = select_clip(&rb, 5).unwrap();
        // The clip must cover ~5s worth of frames (~300), not a handful.
        assert!(
            sel.frame_count >= 290,
            "expected ~300 frames for 5s, got {}",
            sel.frame_count
        );
        assert!(sel.span_micros() >= 4 * ns); // span is in pts ticks (nanos here)
    }

    #[test]
    fn empty_buffer_errors() {
        let rb = RingBuffer::new(60);
        assert_eq!(select_clip(&rb, 30), Err(ClipError::EmptyBuffer));
    }

    #[test]
    fn no_keyframe_at_all_errors() {
        // Frames exist but none is a keyframe -> cannot produce decodable clip.
        let rb = buffer_from(&[(0.0, false), (1.0, false), (2.0, false)]);
        assert_eq!(select_clip(&rb, 30), Err(ClipError::NoKeyframeInWindow));
    }

    #[test]
    fn single_keyframe_buffer() {
        let rb = buffer_from(&[(0.0, true)]);
        let sel = select_clip(&rb, 30).unwrap();
        assert_eq!(sel.start_index, 0);
        assert_eq!(sel.frame_count, 1);
        assert_eq!(sel.start_pts, 0);
        assert_eq!(sel.end_pts, 0);
        assert_eq!(sel.span_micros(), 0);
    }

    #[test]
    fn n_smaller_than_buffer_starts_at_keyframe_before_window() {
        // Keyframes at 0,2,4,6,8,10s. Newest = 10s. Request last 3s -> window
        // start = 7s. Newest keyframe <= 7s is 6s -> start_index at the 6s frame.
        let frames: Vec<(f64, bool)> = (0..=10).map(|i| (i as f64, i % 2 == 0)).collect();
        let rb = buffer_from(&frames);
        let sel = select_clip(&rb, 3).unwrap();
        assert_eq!(sel.start_pts, 6 * MICROS_PER_SEC);
        assert_eq!(sel.end_pts, 10 * MICROS_PER_SEC);
        // frames 6,7,8,9,10 -> 5 frames.
        assert_eq!(sel.frame_count, 5);
    }

    #[test]
    fn n_equal_to_buffer_span() {
        // Keyframes every 2s, 0..=10. Request exactly 10s -> window start = 0s,
        // keyframe at 0s qualifies.
        let frames: Vec<(f64, bool)> = (0..=10).map(|i| (i as f64, i % 2 == 0)).collect();
        let rb = buffer_from(&frames);
        let sel = select_clip(&rb, 10).unwrap();
        assert_eq!(sel.start_index, 0);
        assert_eq!(sel.start_pts, 0);
        assert_eq!(sel.frame_count, 11);
    }

    #[test]
    fn n_larger_than_buffer_uses_earliest_keyframe() {
        // Buffer spans 0..=4s, keyframe at 0 and 2. Request 60s (more than held)
        // -> window start way negative -> no keyframe <= window_start -> fall back
        // to earliest keyframe (0s).
        let rb = buffer_from(&[
            (0.0, true),
            (1.0, false),
            (2.0, true),
            (3.0, false),
            (4.0, false),
        ]);
        let sel = select_clip(&rb, 60).unwrap();
        assert_eq!(sel.start_index, 0);
        assert_eq!(sel.start_pts, 0);
        assert_eq!(sel.frame_count, 5);
    }

    #[test]
    fn keyframe_exactly_at_window_boundary_is_included() {
        // Newest = 10s, request 4s -> window start = 6s. Keyframe exactly at 6s
        // must be selected (<= boundary).
        let rb = buffer_from(&[
            (0.0, true),
            (6.0, true),
            (7.0, false),
            (8.0, false),
            (9.0, false),
            (10.0, false),
        ]);
        let sel = select_clip(&rb, 4).unwrap();
        assert_eq!(sel.start_pts, 6 * MICROS_PER_SEC);
        // frames at 6,7,8,9,10 -> 5 frames.
        assert_eq!(sel.frame_count, 5);
    }

    #[test]
    fn window_between_keyframes_reaches_back_to_previous_keyframe() {
        // Keyframes only at 0s and 5s. Newest = 9s. Request 2s -> window start =
        // 7s. Newest keyframe <= 7s is 5s. Clip reaches back to 5s (longer than
        // requested, but decodable — the documented, correct behavior).
        let rb = buffer_from(&[
            (0.0, true),
            (5.0, true),
            (6.0, false),
            (7.0, false),
            (8.0, false),
            (9.0, false),
        ]);
        let sel = select_clip(&rb, 2).unwrap();
        assert_eq!(sel.start_pts, 5 * MICROS_PER_SEC);
        assert!(sel.span_micros() >= 2 * MICROS_PER_SEC);
    }

    #[test]
    fn first_frame_not_keyframe_but_later_one_is() {
        // First frame is a delta frame; selection must still land on a keyframe.
        let rb = buffer_from(&[(0.0, false), (1.0, true), (2.0, false), (3.0, false)]);
        let sel = select_clip(&rb, 10).unwrap();
        assert_eq!(sel.start_pts, MICROS_PER_SEC);
        assert_eq!(sel.start_index, 1);
    }
}
