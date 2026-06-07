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

pub mod backend;
pub mod clip;
pub mod engine;
pub mod mux;
pub mod ring;

pub use backend::{BackendError, CaptureBackend, Codec, MockBackend, StreamParams};
pub use clip::{select_clip, ClipError, ClipSelection};
pub use engine::{Engine, PreparedClip};
pub use mux::{write_clip, MuxError};
pub use ring::{EncodedFrame, RingBuffer};

/// A presentation timestamp in microseconds (matches ffmpeg/waycap-rs `pts`).
pub type Micros = i64;

/// Microseconds per second.
pub const MICROS_PER_SEC: i64 = 1_000_000;
