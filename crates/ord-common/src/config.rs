//! User configuration for open-recorder.
//!
//! These are pure types: [`Config::from_toml_str`] parses, [`Config::default`]
//! supplies defaults, and every field has a serde default so a partial or
//! missing config still loads. Reading the file from disk is left to the
//! binaries (keeping `ord-common` I/O-free); [`default_config_path`] only builds
//! the path.
//!
//! These enums are the on-disk representation. `ord-core` / the binaries map them
//! onto internal types (e.g. capture quality, export codec) — `ord-common` does
//! not depend on those crates.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Top-level configuration, loaded from `~/.config/open-recorder/config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub capture: CaptureConfig,
    pub audio: AudioConfig,
    pub export: ExportConfig,
    pub hooks: HooksConfig,
}

impl Config {
    /// Parse a config from TOML text. Unknown keys are rejected so typos surface
    /// instead of being silently ignored.
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        toml::from_str(s).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Serialize back to TOML (used to write a default config on first run).
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|e| ConfigError::Serialize(e.to_string()))
    }
}

/// Capture/replay-buffer settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Target capture frame rate.
    pub fps: u32,
    /// Replay buffer length in seconds.
    pub buffer_seconds: u32,
    /// Encoder quality preset (ignored when `bitrate_kbps` is set).
    pub quality: Quality,
    /// Capture codec (NVENC): `h264` (default, most compatible), `hevc`, or
    /// `av1` (best compression; needs an RTX 40/50-series card to encode).
    pub codec: CaptureCodec,
    /// Constant-bitrate mode: target bitrate in kbit/s. `None` (the default)
    /// records in constant-quality mode via `quality`. CBR keeps the replay
    /// buffer's RAM use predictable in high-motion scenes.
    pub bitrate_kbps: Option<u32>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fps: 60,
            buffer_seconds: 60,
            quality: Quality::High,
            codec: CaptureCodec::H264,
            bitrate_kbps: None,
        }
    }
}

/// Hook scripts run by the daemon on events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HooksConfig {
    /// Program run (asynchronously, off the capture path) after a clip is
    /// saved. Receives the clip path as its first argument — use it for
    /// notifications, renaming, uploads, or re-encodes.
    pub on_clip_saved: Option<String>,
}

/// Audio capture settings. Both sources are mixed into one track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Capture desktop/game output (the default sink monitor) — this also picks
    /// up friends' voices from a Discord/TeamSpeak call playing through speakers.
    pub desktop: bool,
    /// Capture the microphone (the default source) — your own voice.
    pub mic: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            desktop: true,
            mic: true,
        }
    }
}

impl AudioConfig {
    /// Whether any audio capture is enabled.
    pub fn any(&self) -> bool {
        self.desktop || self.mic
    }
}

/// Default settings for the "Export Video File" action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExportConfig {
    pub codec: ExportCodec,
    pub container: Container,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            // AV1 (NVENC) is royalty-free, best compression, hardware-fast on
            // RTX 40/50-series, and decoded by Chrome/Firefox/Edge/Safari-17+
            // and Discord. The UI's HandBrake mode lets users override.
            codec: ExportCodec::Av1,
            container: Container::Mp4,
        }
    }
}

/// Encoder quality preset (maps to waycap-rs `QualityPreset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Low,
    Medium,
    High,
    Ultra,
}

/// Capture (NVENC) codec the replay buffer records in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureCodec {
    H264,
    Hevc,
    Av1,
}

/// Export video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportCodec {
    Av1,
    Hevc,
    H264,
}

/// Output container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    Mp4,
    Mkv,
}

impl Container {
    /// File extension (no dot).
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
        }
    }
}

/// Default config path: `$XDG_CONFIG_HOME/open-recorder/config.toml`, falling
/// back to `~/.config/...`. Pure path construction — no I/O.
pub fn default_config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".config")
        });
    base.join("open-recorder/config.toml")
}

/// Errors loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to parse config: {0}")]
    Parse(String),
    #[error("failed to serialize config: {0}")]
    Serialize(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_sane() {
        let c = Config::default();
        assert_eq!(c.capture.fps, 60);
        assert_eq!(c.capture.buffer_seconds, 60);
        assert_eq!(c.capture.quality, Quality::High);
        assert_eq!(c.capture.codec, CaptureCodec::H264);
        assert_eq!(c.capture.bitrate_kbps, None);
        assert!(c.audio.desktop);
        assert!(c.audio.mic);
        assert_eq!(c.export.codec, ExportCodec::Av1);
        assert_eq!(c.export.container, Container::Mp4);
        assert_eq!(c.hooks.on_clip_saved, None);
    }

    #[test]
    fn empty_toml_yields_defaults() {
        assert_eq!(Config::from_toml_str("").unwrap(), Config::default());
    }

    #[test]
    fn partial_toml_overrides_only_given_fields() {
        let c = Config::from_toml_str(
            r#"
            [capture]
            fps = 30

            [audio]
            mic = false
            "#,
        )
        .unwrap();
        assert_eq!(c.capture.fps, 30);
        // Untouched fields keep their defaults.
        assert_eq!(c.capture.buffer_seconds, 60);
        assert!(c.audio.desktop);
        assert!(!c.audio.mic);
        assert_eq!(c.export.codec, ExportCodec::Av1);
    }

    #[test]
    fn full_round_trips() {
        let c = Config {
            capture: CaptureConfig {
                fps: 120,
                buffer_seconds: 30,
                quality: Quality::Ultra,
                codec: CaptureCodec::Hevc,
                bitrate_kbps: Some(20_000),
            },
            audio: AudioConfig {
                desktop: false,
                mic: true,
            },
            export: ExportConfig {
                codec: ExportCodec::H264,
                container: Container::Mkv,
            },
            hooks: HooksConfig {
                on_clip_saved: Some("/usr/bin/notify-clip".into()),
            },
        };
        let toml = c.to_toml_string().unwrap();
        assert_eq!(Config::from_toml_str(&toml).unwrap(), c);
    }

    #[test]
    fn capture_codec_and_hook_parse() {
        let c = Config::from_toml_str(
            r#"
            [capture]
            codec = "av1"
            bitrate_kbps = 12000

            [hooks]
            on_clip_saved = "~/bin/clip-hook"
            "#,
        )
        .unwrap();
        assert_eq!(c.capture.codec, CaptureCodec::Av1);
        assert_eq!(c.capture.bitrate_kbps, Some(12_000));
        assert_eq!(c.hooks.on_clip_saved.as_deref(), Some("~/bin/clip-hook"));
    }

    #[test]
    fn unknown_key_is_rejected() {
        assert!(Config::from_toml_str("[capture]\nnope = 1").is_err());
        assert!(Config::from_toml_str("bogus = true").is_err());
    }

    #[test]
    fn enum_names_are_lowercase() {
        let c = Config::from_toml_str(
            r#"
            [capture]
            quality = "ultra"
            [export]
            codec = "hevc"
            container = "mkv"
            "#,
        )
        .unwrap();
        assert_eq!(c.capture.quality, Quality::Ultra);
        assert_eq!(c.export.codec, ExportCodec::Hevc);
        assert_eq!(c.export.container, Container::Mkv);
    }

    #[test]
    fn container_extension() {
        assert_eq!(Container::Mp4.extension(), "mp4");
        assert_eq!(Container::Mkv.extension(), "mkv");
    }
}
