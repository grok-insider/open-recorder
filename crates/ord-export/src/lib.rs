//! open-recorder smart export.
//!
//! Turns a recorded clip into a shareable file: AV1/HEVC/H.264 via NVENC by
//! default (royalty-free, best compression, hardware-fast on RTX 40/50-series),
//! with HandBrake-style manual control and a software fallback.
//!
//! The crate is split into a **pure policy layer** and a thin **I/O layer**:
//!
//! * [`profile`] — declarative [`ExportProfile`] + presets (no I/O).
//! * [`plan`] — [`build_plan`](plan::build_plan): profile + [`SourceInfo`] ->
//!   exact ffmpeg args (no I/O, fully unit-tested).
//! * [`probe`] — run `ffprobe` to fill in [`SourceInfo`].
//! * [`run`] — [`export`](run::export): probe, then spawn `ffmpeg`, with an
//!   automatic software fallback if NVENC initialization fails.
//!
//! This keeps every encoding decision testable without a GPU or ffmpeg present;
//! only the actual transcode needs the `ffmpeg`/`ffprobe` binaries at runtime.

pub mod plan;
pub mod probe;
pub mod profile;
pub mod run;

pub use plan::{build_plan, FfmpegPlan, SourceInfo, Trim};
pub use profile::{ExportProfile, FrameRate, Preset, RateControl, Scale};
pub use run::{export, export_with, ExportSummary};

/// The `ffmpeg` binary, overridable via `ORD_FFMPEG` (e.g. a Nix store path).
/// The single resolver shared by the runner, the probe path, and the GUI so the
/// override is honored everywhere.
pub fn ffmpeg_bin() -> String {
    std::env::var("ORD_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

/// The `ffprobe` binary, overridable via `ORD_FFPROBE`.
pub fn ffprobe_bin() -> String {
    std::env::var("ORD_FFPROBE").unwrap_or_else(|_| "ffprobe".to_string())
}

/// Errors during export.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("invalid trim window")]
    InvalidTrim,
    #[error("clip has zero duration")]
    ZeroDuration,
    #[error("failed to probe input: {0}")]
    Probe(String),
    #[error("failed to launch ffmpeg: {0}")]
    Spawn(String),
    #[error("ffmpeg exited with {code:?}:\n{stderr_tail}")]
    Ffmpeg {
        code: Option<i32>,
        stderr_tail: String,
    },
    #[error("export cancelled")]
    Cancelled,
    #[error("i/o error: {0}")]
    Io(String),
}
