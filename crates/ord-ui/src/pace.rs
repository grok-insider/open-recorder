//! Pure demux-pacing and audio-clock-feed decisions for the preview player.
//!
//! The preview player's master clock counts audio samples actually played, so
//! the decode thread must keep the audio buffer following *stream time*: spans
//! where the stream simply has no audio (a track that starts late, ends before
//! the video, or has interior holes) are fed to the clock as silence. If the
//! clock is instead left to freeze, video turns into slow motion and the audio
//! callback emits crackling real/zero interleave; if the demuxer is instead
//! left to race ahead looking for audio that does not exist, it decodes and
//! discards video at full speed (>100% CPU, the runaway the editor used to hit
//! near clip ends, on loops, and on cut-piece skips).
//!
//! The decisions live here — free of ffmpeg, audio devices, and threads — so
//! the highest-risk playback logic is deterministic and exhaustively testable.

use std::collections::VecDeque;

/// Seconds of silence pushed per decision. Bounds each buffer-lock extend and
/// bounds how far the clock can run ahead of a real audio packet that shows up
/// later than its surrounding video (interleave skew).
pub const SILENCE_CHUNK_SECS: f64 = 0.25;
/// Smallest audio hole (seconds) worth filling. Spans below this are ordinary
/// interleave jitter; the next real packet covers them (or its own gap-fill
/// does, once it arrives and reveals the hole's true extent).
pub const MIN_HOLE_SECS: f64 = 0.01;
/// Audio look-ahead ceiling in seconds of *device* time. The cap must scale
/// with the device rate/channels: a fixed sample count would shrink to a
/// fraction of a second on 192 kHz or multi-channel outputs.
pub const AUDIO_BUF_SECS: f64 = 2.0;
/// While draining the post-EOF tail, top the buffer back up once it holds less
/// than this many seconds.
pub const EOF_REFILL_BELOW_SECS: f64 = 1.0;

/// Decode-thread inputs to one pacing decision (one demux-loop iteration).
#[derive(Debug, Clone, Copy)]
pub struct PaceInput {
    pub playing: bool,
    /// Whether the session is decoding an audio stream into the buffer.
    pub has_audio: bool,
    /// Decoded video frames queued for display.
    pub video_queued: usize,
    /// Look-ahead target for the video queue (playing vs paused depth).
    pub video_queue_target: usize,
    /// Interleaved samples sitting in the audio buffer.
    pub audio_buffered: usize,
    /// Audio buffer cap (samples).
    pub audio_buf_max: usize,
    /// Furthest decoded video pts (seconds): how far the demuxer has read.
    pub video_demux_pos: f64,
    /// Stream time (seconds) the audio buffer has been filled up to.
    pub audio_fill: f64,
}

/// What the demux loop should do this iteration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaceAction {
    /// Read and decode the next packet.
    Demux,
    /// Look-ahead queues are full: park briefly.
    Park,
    /// The clock is dry but video is already demuxed past the audio fill
    /// point — interleaving means the stream has no audio for that span. Feed
    /// the master clock this many seconds of silence so playback keeps pace.
    FillSilence(f64),
}

/// Decide what the demux loop does next. The silence check runs first: a full
/// video queue with a dry audio buffer would otherwise deadlock (frozen clock →
/// frames never pop → queue never drains → demux never resumes).
pub fn pace(i: PaceInput) -> PaceAction {
    if i.has_audio && i.playing && i.audio_buffered == 0 {
        let hole = i.video_demux_pos - i.audio_fill;
        if hole > MIN_HOLE_SECS {
            return PaceAction::FillSilence(hole.min(SILENCE_CHUNK_SECS));
        }
    }
    let video_full = i.video_queued >= i.video_queue_target;
    let audio_full = i.has_audio && i.audio_buffered >= i.audio_buf_max;
    if video_full || audio_full {
        PaceAction::Park
    } else {
        PaceAction::Demux
    }
}

/// Seconds of trailing silence to feed the clock at demuxer EOF, so an audio
/// track shorter than the video/container plays the tail out at speed — and
/// reaches the out-point for a clean stop or loop — instead of freezing with
/// video still queued. Returns `None` when nothing (more) is needed.
pub fn eof_silence(
    playing: bool,
    has_audio: bool,
    audio_fill: f64,
    media_end: f64,
    audio_buffered: usize,
    samples_per_sec: usize,
) -> Option<f64> {
    if !playing || !has_audio {
        return None;
    }
    let refill_below = (EOF_REFILL_BELOW_SECS * samples_per_sec as f64) as usize;
    if audio_buffered >= refill_below {
        return None;
    }
    let left = media_end - audio_fill;
    if left <= 1e-6 {
        return None;
    }
    Some(left.min(SILENCE_CHUNK_SECS))
}

/// Copy up to `out.len()` samples from the front of `buf` into `out`,
/// returning how many were copied. Two bulk memcpys (the deque's slices) plus
/// a front drain — no per-sample pops — so the realtime audio callback's hold
/// on the buffer lock stays minimal. The caller zero-fills the shortfall and
/// applies volume *after* releasing the lock.
pub fn drain_into(out: &mut [f32], buf: &mut VecDeque<f32>) -> usize {
    let n = out.len().min(buf.len());
    let (a, b) = buf.as_slices();
    let from_a = n.min(a.len());
    out[..from_a].copy_from_slice(&a[..from_a]);
    if n > from_a {
        out[from_a..n].copy_from_slice(&b[..n - from_a]);
    }
    buf.drain(..n);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> PaceInput {
        PaceInput {
            playing: true,
            has_audio: true,
            video_queued: 0,
            video_queue_target: 30,
            audio_buffered: 1000,
            audio_buf_max: 192_000,
            video_demux_pos: 5.0,
            audio_fill: 5.0,
        }
    }

    #[test]
    fn demuxes_when_queues_have_room() {
        assert_eq!(pace(base()), PaceAction::Demux);
    }

    #[test]
    fn parks_when_video_queue_full() {
        let i = PaceInput {
            video_queued: 30,
            ..base()
        };
        assert_eq!(pace(i), PaceAction::Park);
    }

    #[test]
    fn parks_when_audio_buffer_full() {
        let i = PaceInput {
            audio_buffered: 192_000,
            ..base()
        };
        assert_eq!(pace(i), PaceAction::Park);
    }

    #[test]
    fn audio_full_ignored_without_audio_stream() {
        let i = PaceInput {
            has_audio: false,
            audio_buffered: usize::MAX,
            ..base()
        };
        assert_eq!(pace(i), PaceAction::Demux);
    }

    #[test]
    fn fills_bounded_silence_when_dry_with_hole() {
        let i = PaceInput {
            audio_buffered: 0,
            video_demux_pos: 8.0,
            audio_fill: 5.0,
            ..base()
        };
        assert_eq!(pace(i), PaceAction::FillSilence(SILENCE_CHUNK_SECS));
    }

    #[test]
    fn fills_exactly_a_small_hole() {
        let i = PaceInput {
            audio_buffered: 0,
            video_demux_pos: 5.1,
            audio_fill: 5.0,
            ..base()
        };
        match pace(i) {
            PaceAction::FillSilence(s) => assert!((s - 0.1).abs() < 1e-9),
            other => panic!("expected fill, got {other:?}"),
        }
    }

    /// The regression this module exists for: a full video queue with a dry
    /// audio buffer must FEED THE CLOCK, never park (the frozen-clock deadlock)
    /// and never keep demuxing (the old full-speed decode-and-drop race).
    #[test]
    fn full_queue_with_dry_audio_fills_instead_of_parking_or_racing() {
        let i = PaceInput {
            video_queued: 30,
            audio_buffered: 0,
            video_demux_pos: 6.0,
            audio_fill: 5.5,
            ..base()
        };
        assert_eq!(pace(i), PaceAction::FillSilence(SILENCE_CHUNK_SECS));
    }

    #[test]
    fn no_fill_while_paused() {
        let i = PaceInput {
            playing: false,
            audio_buffered: 0,
            video_demux_pos: 8.0,
            ..base()
        };
        assert_eq!(pace(i), PaceAction::Demux);
    }

    #[test]
    fn no_fill_without_audio_stream() {
        let i = PaceInput {
            has_audio: false,
            audio_buffered: 0,
            video_demux_pos: 8.0,
            ..base()
        };
        assert_eq!(pace(i), PaceAction::Demux);
    }

    /// Seek run-up: video decoded so far sits *behind* the audio fill point
    /// (drop-until target). Dry audio there is expected — keep demuxing.
    #[test]
    fn no_fill_when_demux_is_behind_audio_fill() {
        let i = PaceInput {
            audio_buffered: 0,
            video_demux_pos: 4.0,
            audio_fill: 5.0,
            ..base()
        };
        assert_eq!(pace(i), PaceAction::Demux);
    }

    #[test]
    fn jitter_sized_hole_is_not_filled() {
        let i = PaceInput {
            audio_buffered: 0,
            video_demux_pos: 5.0 + MIN_HOLE_SECS / 2.0,
            audio_fill: 5.0,
            ..base()
        };
        assert_eq!(pace(i), PaceAction::Demux);
    }

    #[test]
    fn eof_fill_feeds_short_audio_tail() {
        let s = eof_silence(true, true, 3.0, 6.0, 0, 96_000);
        assert_eq!(s, Some(SILENCE_CHUNK_SECS));
    }

    #[test]
    fn eof_fill_final_chunk_is_partial() {
        let s = eof_silence(true, true, 5.9, 6.0, 0, 96_000).expect("fill");
        assert!((s - 0.1).abs() < 1e-9);
    }

    #[test]
    fn eof_fill_stops_at_media_end() {
        assert_eq!(eof_silence(true, true, 6.0, 6.0, 0, 96_000), None);
    }

    #[test]
    fn eof_fill_waits_while_buffer_holds_enough() {
        let buffered = 96_000; // 1s at 48k stereo == the refill threshold
        assert_eq!(eof_silence(true, true, 3.0, 6.0, buffered, 96_000), None);
    }

    #[test]
    fn eof_fill_resumes_below_refill_threshold() {
        let buffered = 96_000 - 1;
        assert!(eof_silence(true, true, 3.0, 6.0, buffered, 96_000).is_some());
    }

    #[test]
    fn eof_fill_only_while_playing_with_audio() {
        assert_eq!(eof_silence(false, true, 3.0, 6.0, 0, 96_000), None);
        assert_eq!(eof_silence(true, false, 3.0, 6.0, 0, 96_000), None);
    }

    #[test]
    fn drain_copies_and_consumes_exactly() {
        let mut buf: VecDeque<f32> = (1..=6).map(|v| v as f32).collect();
        let mut out = [0.0f32; 4];
        let n = drain_into(&mut out, &mut buf);
        assert_eq!(n, 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![5.0, 6.0]);
    }

    #[test]
    fn drain_underrun_reports_shortfall() {
        let mut buf: VecDeque<f32> = [1.0, 2.0].into_iter().collect();
        let mut out = [9.0f32; 5];
        let n = drain_into(&mut out, &mut buf);
        assert_eq!(n, 2);
        assert_eq!(&out[..2], &[1.0, 2.0]);
        // The tail is the caller's to zero — drain_into must not touch it.
        assert_eq!(&out[2..], &[9.0, 9.0, 9.0]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_handles_wrapped_deque() {
        // Force a discontiguous ring: fill to capacity, pop the front, wrap.
        let mut buf: VecDeque<f32> = VecDeque::with_capacity(4);
        for v in 0..4 {
            buf.push_back(v as f32);
        }
        buf.pop_front();
        buf.pop_front();
        buf.push_back(4.0);
        buf.push_back(5.0);
        assert!(
            !buf.as_slices().1.is_empty(),
            "test premise: deque must wrap"
        );
        let mut out = [0.0f32; 4];
        let n = drain_into(&mut out, &mut buf);
        assert_eq!(n, 4);
        assert_eq!(out, [2.0, 3.0, 4.0, 5.0]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_empty_buffer_is_a_noop() {
        let mut buf: VecDeque<f32> = VecDeque::new();
        let mut out = [7.0f32; 3];
        assert_eq!(drain_into(&mut out, &mut buf), 0);
        assert_eq!(out, [7.0, 7.0, 7.0]);
    }
}
