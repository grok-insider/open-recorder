//! Export profile: a declarative description of *how* to transcode a clip.
//!
//! A [`ExportProfile`] is pure data — codec, container, scaling, frame rate and
//! rate control. It is turned into an ffmpeg invocation by
//! [`build_plan`](crate::plan::build_plan); nothing here touches the filesystem
//! or spawns a process, so the whole policy layer is unit-testable.
//!
//! Presets ([`ExportProfile::discord`], [`high_quality`](ExportProfile::high_quality),
//! [`x_twitter`](ExportProfile::x_twitter), [`gif`](ExportProfile::gif), …) capture
//! the common cases; constructing an [`ExportProfile`] field-by-field is the
//! HandBrake-style escape hatch where the caller sets every knob.

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
    /// Center-crop to 9:16 portrait and scale to `height` (e.g. 1920 →
    /// 1080×1920) — the TikTok/Shorts/Reels reframe.
    Vertical { height: u32 },
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

/// What kind of file the export produces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Output {
    /// A normal video file — codec/scale/rate-control apply.
    #[default]
    Video,
    /// Audio only (`-vn`): extract the soundtrack to the container's audio codec.
    AudioOnly,
    /// An animated GIF via a generated palette (codec/audio are ignored; `scale`
    /// and `fps` size it).
    Gif,
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
    /// Drop the audio track entirely (`-an`).
    pub mute: bool,
    /// What kind of file to produce (video / audio-only / gif).
    #[serde(default)]
    pub output: Output,
    /// Apply EBU R128 loudness normalization (`-af loudnorm`) to the audio.
    #[serde(default)]
    pub normalize_audio: bool,
    /// Playback rate for the export (`0.25`…`2.0`, default `1.0`). Applied via
    /// `setpts`/`atempo` and forces a re-encode when not 1.0.
    #[serde(default = "default_speed")]
    pub speed: f64,
}

fn default_speed() -> f64 {
    1.0
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
            mute: false,
            output: Output::Video,
            normalize_audio: false,
            speed: 1.0,
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
            mute: false,
            output: Output::Video,
            normalize_audio: false,
            speed: 1.0,
        }
    }

    /// Vertical 9:16 for TikTok / Shorts / Reels: center-crop + 1080×1920,
    /// H.264 (every mobile platform ingests it) at a quality good enough for
    /// the re-encode those platforms apply anyway.
    pub fn vertical() -> Self {
        Self {
            codec: ExportCodec::H264,
            container: Container::Mp4,
            scale: Scale::Vertical { height: 1920 },
            fps: FrameRate::Source,
            rate_control: RateControl::Quality(21),
            hardware: true,
            audio_kbps: 128,
            mute: false,
            output: Output::Video,
            normalize_audio: false,
            speed: 1.0,
        }
    }

    /// X (Twitter)-friendly: H.264 High in MP4, capped 1080p, source fps, at a
    /// quality that keeps clips comfortably uploadable. H.264 because X re-encodes
    /// and AV1/HEVC uploads play back unreliably there.
    pub fn x_twitter() -> Self {
        Self {
            codec: ExportCodec::H264,
            container: Container::Mp4,
            scale: Scale::MaxHeight(1080),
            fps: FrameRate::Source,
            rate_control: RateControl::Quality(23),
            hardware: true,
            audio_kbps: 128,
            mute: false,
            output: Output::Video,
            normalize_audio: false,
            speed: 1.0,
        }
    }

    /// 1080p60 keep-quality: HEVC NVENC at CQ, downscaled to 1080p but keeping the
    /// source frame rate — a smaller, more widely-playable alternative to the
    /// full-res AV1 "high quality".
    pub fn hq_1080p60() -> Self {
        Self {
            codec: ExportCodec::Hevc,
            container: Container::Mp4,
            scale: Scale::MaxHeight(1080),
            fps: FrameRate::Source,
            rate_control: RateControl::Quality(21),
            hardware: true,
            audio_kbps: 160,
            mute: false,
            output: Output::Video,
            normalize_audio: false,
            speed: 1.0,
        }
    }

    /// Audio only: extract the soundtrack (AAC in MP4 / Opus in MKV), no video.
    pub fn audio_only(container: Container) -> Self {
        Self {
            codec: ExportCodec::H264, // unused
            container,
            scale: Scale::Source,
            fps: FrameRate::Source,
            rate_control: RateControl::Quality(0), // unused
            hardware: false,
            audio_kbps: 192,
            mute: false,
            output: Output::AudioOnly,
            normalize_audio: false,
            speed: 1.0,
        }
    }

    /// Animated GIF via a generated palette, capped to a modest height + frame
    /// rate so the file stays shareable. Codec/audio are ignored.
    pub fn gif() -> Self {
        Self {
            codec: ExportCodec::H264,  // unused
            container: Container::Mp4, // unused (output is .gif)
            scale: Scale::MaxHeight(480),
            fps: FrameRate::Fixed(15),
            rate_control: RateControl::Quality(0), // unused
            hardware: false,
            audio_kbps: 0,
            mute: true,
            output: Output::Gif,
            normalize_audio: false,
            speed: 1.0,
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
            mute: false,
            output: Output::Video,
            normalize_audio: false,
            speed: 1.0,
        }
    }

    /// Whether this profile re-encodes the video (vs. a pure stream copy).
    pub fn reencodes(&self) -> bool {
        self.output == Output::Video
            && (self.rate_control != RateControl::Copy || (self.speed - 1.0).abs() > 0.01)
    }

    /// The output file extension (no dot) this profile should write to. Video
    /// uses the container; audio-only and GIF override it.
    pub fn output_extension(&self) -> &'static str {
        match self.output {
            Output::Video => self.container.extension(),
            Output::AudioOnly => match self.container {
                Container::Mkv => "opus",
                Container::Mp4 => "m4a",
            },
            Output::Gif => "gif",
        }
    }
}

/// A named preset, for the UI's preset picker and the `--preset` CLI flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    HighQuality,
    Discord,
    XTwitter,
    Vertical,
    Hq1080p60,
    AudioOnly,
    Gif,
    Source,
}

impl Preset {
    /// Every preset, in the order UIs should offer them. Menus iterate this so
    /// a new preset can never be forgotten in one of them.
    pub const ALL: [Preset; 8] = [
        Preset::Discord,
        Preset::HighQuality,
        Preset::Hq1080p60,
        Preset::XTwitter,
        Preset::Vertical,
        Preset::AudioOnly,
        Preset::Gif,
        Preset::Source,
    ];

    /// A filename-safe slug for export naming (`<stem>-<slug>.<ext>`).
    pub fn slug(self) -> &'static str {
        match self {
            Preset::HighQuality => "high",
            Preset::Discord => "discord",
            Preset::XTwitter => "x",
            Preset::Vertical => "vertical",
            Preset::Hq1080p60 => "1080p60",
            Preset::AudioOnly => "audio",
            Preset::Gif => "gif",
            Preset::Source => "source",
        }
    }

    /// Materialize the preset into a concrete profile.
    pub fn profile(self) -> ExportProfile {
        match self {
            Preset::HighQuality => ExportProfile::high_quality(),
            Preset::Discord => ExportProfile::discord(),
            Preset::XTwitter => ExportProfile::x_twitter(),
            Preset::Vertical => ExportProfile::vertical(),
            Preset::Hq1080p60 => ExportProfile::hq_1080p60(),
            Preset::AudioOnly => ExportProfile::audio_only(Container::Mp4),
            Preset::Gif => ExportProfile::gif(),
            Preset::Source => ExportProfile::source(Container::Mkv),
        }
    }

    /// A short human label for menus.
    pub fn label(self) -> &'static str {
        match self {
            Preset::HighQuality => "High quality",
            Preset::Discord => "Discord (≤10 MB)",
            Preset::XTwitter => "X / Twitter",
            Preset::Vertical => "Vertical 9:16 (Shorts)",
            Preset::Hq1080p60 => "1080p60 HQ",
            Preset::AudioOnly => "Audio only",
            Preset::Gif => "GIF",
            Preset::Source => "Source (remux)",
        }
    }

    /// Parse a preset name (case-insensitive). Used by the CLI.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" | "highquality" | "high-quality" | "high_quality" => Some(Preset::HighQuality),
            "discord" => Some(Preset::Discord),
            "x" | "twitter" | "x-twitter" | "x_twitter" => Some(Preset::XTwitter),
            "1080p60" | "1080p" | "hq1080p60" | "hq-1080p60" => Some(Preset::Hq1080p60),
            "audio" | "audio-only" | "audio_only" | "audioonly" => Some(Preset::AudioOnly),
            "gif" => Some(Preset::Gif),
            "vertical" | "shorts" | "tiktok" | "reels" | "9x16" => Some(Preset::Vertical),
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

    #[test]
    fn output_extensions() {
        assert_eq!(ExportProfile::high_quality().output_extension(), "mp4");
        assert_eq!(
            ExportProfile::source(Container::Mkv).output_extension(),
            "mkv"
        );
        assert_eq!(
            ExportProfile::audio_only(Container::Mp4).output_extension(),
            "m4a"
        );
        assert_eq!(
            ExportProfile::audio_only(Container::Mkv).output_extension(),
            "opus"
        );
        assert_eq!(ExportProfile::gif().output_extension(), "gif");
    }

    #[test]
    fn non_video_outputs_are_not_reencodes() {
        assert!(!ExportProfile::gif().reencodes());
        assert!(!ExportProfile::audio_only(Container::Mp4).reencodes());
        assert!(ExportProfile::high_quality().reencodes());
    }

    #[test]
    fn new_presets_parse_and_label() {
        assert_eq!(Preset::parse("x"), Some(Preset::XTwitter));
        assert_eq!(Preset::parse("1080p60"), Some(Preset::Hq1080p60));
        assert_eq!(Preset::parse("audio"), Some(Preset::AudioOnly));
        assert_eq!(Preset::parse("gif"), Some(Preset::Gif));
        assert!(!Preset::Gif.label().is_empty());
    }
}
