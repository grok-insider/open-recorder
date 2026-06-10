//! open-recorder capture/encode engine.
//!
//! This crate owns the two pieces of logic that must be correct and are testable
//! without a GPU:
//!
//! * [`ring`] — the bounded in-RAM buffer of **encoded** frames (the ShadowPlay
//!   "instant replay" buffer).
//! * [`clip`] — keyframe-aware "save the last N seconds" selection: a saved clip
//!   must begin on the newest keyframe at or before the start of the window so
//!   the result is decodable with a pure stream-copy (no re-encode).
//!
//! The actual capture/encode (waycap-rs / NVENC) is wired in later behind a
//! `CaptureBackend` trait; it feeds [`EncodedFrame`]s into the [`ring::RingBuffer`].

pub mod audio;
pub mod backend;
pub mod clip;
pub mod engine;
pub mod mux;
pub mod record;
pub mod ring;
pub mod store;
#[cfg(feature = "waycap")]
pub mod waycap_backend;

pub use audio::{AudioCodec, AudioParams, AudioRingBuffer, EncodedAudioFrame};
pub use backend::{BackendError, CaptureBackend, CaptureStreams, Codec, MockBackend, StreamParams};
pub use clip::{select_clip, ClipError, ClipSelection};
pub use engine::{Engine, PreparedClip};
pub use mux::{verify_clip, write_clip, ClipCheck, MuxError};
pub use record::Recorder;
pub use ring::{EncodedFrame, RingBuffer};
pub use store::{FrameMeta, FrameStore};

/// A timestamp expressed in the stream's time base — **ticks**, not a fixed
/// unit. The tick length is defined by the backend's
/// [`time_base_den`](backend::StreamParams::time_base_den): nanoseconds for
/// waycap-rs capture, microseconds for the mock. (The old name `Micros` was a
/// lie — waycap delivers nanoseconds — which once made the save window 1000×
/// too small.) Convert to real microseconds with [`ticks_to_micros`] when
/// correlating audio and video.
pub type Ticks = i64;

/// Microseconds per second (the mock backend's tick rate, and the audio
/// correlation unit).
pub const MICROS_PER_SEC: i64 = 1_000_000;

/// Convert a timestamp expressed in `den` ticks-per-second into microseconds.
///
/// Uses a 128-bit intermediate so it never overflows for large values — video
/// pts from waycap-rs are raw `CLOCK_MONOTONIC` nanoseconds (often ~10^14+),
/// and `ticks * 1_000_000` would overflow an `i64`.
pub(crate) fn ticks_to_micros(ticks: i64, den: i64) -> i64 {
    let den = den.max(1) as i128;
    ((ticks as i128 * MICROS_PER_SEC as i128) / den) as i64
}

#[cfg(test)]
mod conv_tests {
    use super::ticks_to_micros;

    #[test]
    fn nanos_to_micros_no_overflow() {
        // ~28 hours of uptime in nanoseconds — overflows the naive i64 path.
        let ns: i64 = 100_000_000_000_000;
        assert_eq!(ticks_to_micros(ns, 1_000_000_000), ns / 1000);
    }

    #[test]
    fn micros_passthrough() {
        // den already in microseconds (MockBackend) -> identity.
        assert_eq!(ticks_to_micros(12_345_678, 1_000_000), 12_345_678);
    }

    #[test]
    fn zero_den_is_safe() {
        assert_eq!(ticks_to_micros(1000, 0), 1000 * 1_000_000);
    }
}
