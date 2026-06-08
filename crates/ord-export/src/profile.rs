//! Export profile: a declarative description of *how* to transcode a clip.
//!
//! A [`ExportProfile`] is pure data — codec, container, scaling, frame rate and
//! rate control. It is turned into an ffmpeg invocation by
//! [`build_plan`](crate::plan::build_plan); nothing here touches the filesystem
//! or spawns a process, so the whole policy layer is unit-testable.
//!
//! Presets ([`ExportProfile::discord`], [`high_quality`](ExportProfile::high_quality),
//! [`source`](ExportProfile::source)) capture the common cases; [`manual`] is the
//! HandBrake-style escape hatch where the caller sets every field.

use ord_common::config::{Container, ExportCodec};
use serde::{Deserialize, Serialize};

/// How the output is sized relative to the source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scale {
    /// Keep the source resolution untouched.
    #[default]
    Source,
    /// Downscale so the height is at most this many pixels, preserving aspect
    /// ratio. Never upscales: a source already shorter than the cap is left
    /// as-is.
    MaxHeight(u32),
    /// Force an exact width × height (may change aspect ratio).
    Exact { width: u32, height: u32 },
}

/// How the encoder decides bitrate/quality.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateControl {
    /// Constant quality (NVENC `-cq`, software `-crf`). Lower is better quality
    /// and a bigger file; ~20 is visually near-lossless for AV1/HEVC.
    Quality(u8),
    /// Target average video bitrate in kilobits per second (VBR).
    Bitrate { kbps: u32 },
    /// Aim for a finished file of about this many mebibytes. The planner derives
    /// a video bitrate from the clip duration and the audio bitrate.
    TargetSize { mib: f64 },
    /// Stream copy — no re-encode of the video. Only valid for the
    /// [`ExportProfile::source`] preset; fast and lossless but trims snap to the
    /// nearest keyframe.
    Copy,
}

/// Frame-rate handling for the output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameRate {
    /// Keep the source frame rate.
    #[default]
    Source,
    /// Force a specific frame rate.
    Fixed(u32),
}

/// A complete, declarative export recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportProfile {
    pub codec: ExportCodec,
    pub container: Container,
    pub scale: Scale,
    pub fps: FrameRate,
    pub rate_control: RateControl,
    /// Prefer the NVENC hardware encoder. When false, the software encoder
    /// (libsvtav1 / libx265 / libx264) is used directly. When true, the runner
    /// may still fall back to software if NVENC initialization fails.
    pub hardware: bool,
    /// Audio output bitrate in kbps (used when transcoding to AAC). Opus copy
    /// ignores this.
    pub audio_kbps: u32,
}

impl Default for ExportProfile {
    fn default() -> Self {
        ExportProfile::high_quality()
    }
}

impl ExportProfile {
    /// Best quality for keeping: AV1 NVENC at constant quality, full source
    /// resolution and frame rate, in an MP4. Sensible default for "Export".
    pub fn high_quality() -> Self {
        Self {
            codec: ExportCodec::Av1,
            container: Container::Mp4,
            scale: Scale::Source,
            fps: FrameRate::Source,
            rate_control: RateControl::Quality(20),
            hardware: true,
            audio_kbps: 192,
        }
    }

    /// Sized to fit Discord's 10 MB free-tier upload: AV1 NVENC targeting ~9 MiB,
    /// capped at 1080p, source frame rate, MP4 with faststart.
    pub fn discord() -> Self {
        Self {
            codec: ExportCodec::Av1,
            container: Container::Mp4,
            scale: Scale::MaxHeight(1080),
            fps: FrameRate::Source,
            rate_control: RateControl::TargetSize { mib: 9.0 },
            hardware: true,
            audio_kbps: 128,
        }
    }

    /// Lossless remux: stream-copy video and audio into the chosen container.
    /// Instant and bit-exact, but any trim snaps to the nearest keyframe.
    pub fn source(container: Container) -> Self {
        Self {
            codec: ExportCodec::H264, // unused for a copy, but a valid value
            container,
            scale: Scale::Source,
            fps: FrameRate::Source,
            rate_control: RateControl::Copy,
            hardware: false,
            audio_kbps: 0,
        }
    }

    /// Whether this profile re-encodes the video (vs. a pure stream copy).
    pub fn reencodes(&self) -> bool {
        self.rate_control != RateControl::Copy
    }
}

/// A named preset, for the UI's preset picker and the `--preset` CLI flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    HighQuality,
    Discord,
    Source,
}

impl Preset {
    /// Materialize the preset into a concrete profile.
    pub fn profile(self) -> ExportProfile {
        match self {
            Preset::HighQuality => ExportProfile::high_quality(),
            Preset::Discord => ExportProfile::discord(),
            Preset::Source => ExportProfile::source(Container::Mkv),
        }
    }

    /// Parse a preset name (case-insensitive). Used by the CLI.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" | "highquality" | "high-quality" | "high_quality" => Some(Preset::HighQuality),
            "discord" => Some(Preset::Discord),
            "source" | "copy" | "remux" => Some(Preset::Source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_high_quality() {
        assert_eq!(ExportProfile::default(), ExportProfile::high_quality());
        assert!(ExportProfile::default().reencodes());
    }

    #[test]
    fn source_is_a_copy() {
        let p = ExportProfile::source(Container::Mp4);
        assert_eq!(p.rate_control, RateControl::Copy);
        assert!(!p.reencodes());
    }

    #[test]
    fn discord_targets_size_and_caps_height() {
        let p = ExportProfile::discord();
        assert_eq!(p.scale, Scale::MaxHeight(1080));
        assert!(matches!(p.rate_control, RateControl::TargetSize { .. }));
    }

    #[test]
    fn preset_parsing_is_lenient() {
        assert_eq!(Preset::parse("Discord"), Some(Preset::Discord));
        assert_eq!(Preset::parse("high-quality"), Some(Preset::HighQuality));
        assert_eq!(Preset::parse("REMUX"), Some(Preset::Source));
        assert_eq!(Preset::parse("nope"), None);
    }

    #[test]
    fn presets_materialize() {
        assert_eq!(Preset::Discord.profile(), ExportProfile::discord());
        assert_eq!(Preset::HighQuality.profile(), ExportProfile::high_quality());
    }
}
