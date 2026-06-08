//! Audio: encoded audio frames + a time-windowed buffer.
//!
//! Audio is correlated with video through a **microsecond capture timestamp**
//! (`timestamp_micros`), which both the video and audio paths can express on the
//! same monotonic clock. This sidesteps the video-pts (nanoseconds) vs
//! audio-pts (1/48000) time-base mismatch: clip selection picks the video window
//! by keyframe, derives its microsecond span, then selects the audio frames
//! whose capture timestamp falls inside that span.
//!
//! Audio frames have no keyframes (each Opus packet is independently decodable),
//! so the buffer is a plain time window with no keyframe logic.

use std::collections::VecDeque;

use crate::Micros;

/// One encoded (Opus) audio frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAudioFrame {
    /// Encoded packet bytes.
    pub data: Vec<u8>,
    /// Presentation timestamp in the encoder's own time base (e.g. 1/48000).
    pub pts: i64,
    /// Capture timestamp in microseconds, on the shared monotonic clock used to
    /// correlate with video.
    pub timestamp_micros: Micros,
}

impl EncodedAudioFrame {
    pub fn new(data: Vec<u8>, pts: i64, timestamp_micros: Micros) -> Self {
        Self {
            data,
            pts,
            timestamp_micros,
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Negotiated audio stream parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u16,
    pub codec: AudioCodec,
}

/// Audio codec a backend emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Opus,
}

const MICROS_PER_SEC: i64 = 1_000_000;

/// A bounded, time-windowed buffer of encoded audio frames, keyed by capture
/// timestamp (microseconds). Mirrors the video [`RingBuffer`](crate::ring) but
/// without keyframe handling.
#[derive(Debug)]
pub struct AudioRingBuffer {
    frames: VecDeque<EncodedAudioFrame>,
    capacity_micros: Micros,
    max_ts: Micros,
    bytes: usize,
}

impl AudioRingBuffer {
    /// Hold at most `capacity_seconds` of audio. Must be >= 1.
    pub fn new(capacity_seconds: u32) -> Self {
        debug_assert!(capacity_seconds >= 1);
        Self {
            frames: VecDeque::new(),
            capacity_micros: capacity_seconds as i64 * MICROS_PER_SEC,
            max_ts: i64::MIN,
            bytes: 0,
        }
    }

    /// Push a frame, ordered by timestamp, then evict frames older than the
    /// capacity window behind the newest seen timestamp.
    pub fn push(&mut self, frame: EncodedAudioFrame) {
        self.max_ts = self.max_ts.max(frame.timestamp_micros);
        self.bytes += frame.len();
        if self
            .frames
            .back()
            .map(|b| frame.timestamp_micros >= b.timestamp_micros)
            .unwrap_or(true)
        {
            self.frames.push_back(frame);
        } else {
            let pos = self
                .frames
                .iter()
                .position(|f| f.timestamp_micros > frame.timestamp_micros)
                .unwrap_or(self.frames.len());
            self.frames.insert(pos, frame);
        }
        let cutoff = self.max_ts - self.capacity_micros;
        while let Some(front) = self.frames.front() {
            if front.timestamp_micros < cutoff {
                let removed = self.frames.pop_front().expect("front exists");
                self.bytes -= removed.len();
            } else {
                break;
            }
        }
    }

    /// Frames whose capture timestamp falls within `[start_micros, end_micros]`,
    /// cloned, oldest first. Used to pick the audio for a video clip window.
    pub fn select_window(
        &self,
        start_micros: Micros,
        end_micros: Micros,
    ) -> Vec<EncodedAudioFrame> {
        self.frames
            .iter()
            .filter(|f| f.timestamp_micros >= start_micros && f.timestamp_micros <= end_micros)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
        self.max_ts = i64::MIN;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(ts_sec: f64) -> EncodedAudioFrame {
        let ts = (ts_sec * MICROS_PER_SEC as f64) as i64;
        EncodedAudioFrame::new(vec![0u8; 8], ts, ts)
    }

    #[test]
    fn empty_buffer() {
        let b = AudioRingBuffer::new(60);
        assert!(b.is_empty());
        assert_eq!(b.bytes(), 0);
        assert!(b.select_window(0, 1_000_000).is_empty());
    }

    #[test]
    fn evicts_old_audio() {
        let mut b = AudioRingBuffer::new(5);
        for i in 0..=10 {
            b.push(f(i as f64));
        }
        // newest = 10s, cutoff = 5s, keep 5..=10 -> 6 frames.
        assert_eq!(b.len(), 6);
    }

    #[test]
    fn select_window_picks_range() {
        let mut b = AudioRingBuffer::new(60);
        for i in 0..=10 {
            b.push(f(i as f64));
        }
        let sel = b.select_window(3 * MICROS_PER_SEC, 6 * MICROS_PER_SEC);
        // 3,4,5,6 -> 4 frames.
        assert_eq!(sel.len(), 4);
        assert_eq!(sel.first().unwrap().timestamp_micros, 3 * MICROS_PER_SEC);
        assert_eq!(sel.last().unwrap().timestamp_micros, 6 * MICROS_PER_SEC);
    }

    #[test]
    fn out_of_order_inserted_in_order() {
        let mut b = AudioRingBuffer::new(60);
        b.push(f(5.0));
        b.push(f(3.0));
        let ts: Vec<i64> = b
            .select_window(0, 100 * MICROS_PER_SEC)
            .iter()
            .map(|f| f.timestamp_micros)
            .collect();
        assert_eq!(ts, vec![3 * MICROS_PER_SEC, 5 * MICROS_PER_SEC]);
    }

    #[test]
    fn clear_resets() {
        let mut b = AudioRingBuffer::new(60);
        b.push(f(1.0));
        b.clear();
        assert!(b.is_empty());
        assert_eq!(b.bytes(), 0);
    }
}
