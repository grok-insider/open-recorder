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
    pub storage: StorageConfig,
    pub markers: MarkersConfig,
    pub overlay: OverlayConfig,
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

    /// Parse the layered configuration: `base` (the user/HM-managed
    /// `config.toml`) with `overrides` (the daemon-written runtime overrides,
    /// see [`overrides_path`]) deep-merged on top. Either layer may be empty.
    ///
    /// This is what lets an in-app settings panel coexist with a declarative,
    /// read-only base config: the base stays the source of truth, overrides
    /// are a sparse TOML document containing only the fields the user changed
    /// at runtime.
    pub fn from_layers(base: &str, overrides: &str) -> Result<Self, ConfigError> {
        let mut base_v: toml::Value =
            toml::from_str(base).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let over_v: toml::Value =
            toml::from_str(overrides).map_err(|e| ConfigError::Parse(e.to_string()))?;
        merge_toml(&mut base_v, over_v);
        base_v
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Parse(e.to_string()))
    }

    /// The sparse overrides document that turns `base` into `desired`: only
    /// leaves that differ are emitted. Returns an empty string when nothing
    /// differs (callers may then delete the overrides file).
    pub fn diff_overrides(base: &Config, desired: &Config) -> Result<String, ConfigError> {
        let base_v =
            toml::Value::try_from(base).map_err(|e| ConfigError::Serialize(e.to_string()))?;
        let desired_v =
            toml::Value::try_from(desired).map_err(|e| ConfigError::Serialize(e.to_string()))?;
        match diff_toml(&base_v, &desired_v) {
            Some(d) => {
                toml::to_string_pretty(&d).map_err(|e| ConfigError::Serialize(e.to_string()))
            }
            None => Ok(String::new()),
        }
    }

    /// Dotted paths (`section.field`) of every leaf where `self` differs from
    /// `base` — what a settings UI marks as "overridden".
    pub fn overridden_fields(&self, base: &Config) -> Vec<String> {
        let (Ok(a), Ok(b)) = (toml::Value::try_from(base), toml::Value::try_from(self)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        collect_diff_paths(&a, &b, String::new(), &mut out);
        out.sort();
        out
    }
}

/// Recursively overlay `overlay` onto `base`: tables merge per key, any other
/// value replaces.
fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge_toml(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

/// The sparse table of leaves where `desired` differs from `base`; `None` when
/// they are identical. Keys present only in `desired` are kept verbatim.
fn diff_toml(base: &toml::Value, desired: &toml::Value) -> Option<toml::Value> {
    match (base, desired) {
        (toml::Value::Table(b), toml::Value::Table(d)) => {
            let mut out = toml::map::Map::new();
            for (k, dv) in d {
                match b.get(k) {
                    Some(bv) => {
                        if let Some(changed) = diff_toml(bv, dv) {
                            out.insert(k.clone(), changed);
                        }
                    }
                    None => {
                        out.insert(k.clone(), dv.clone());
                    }
                }
            }
            (!out.is_empty()).then_some(toml::Value::Table(out))
        }
        (b, d) => (b != d).then(|| d.clone()),
    }
}

fn collect_diff_paths(
    base: &toml::Value,
    desired: &toml::Value,
    prefix: String,
    out: &mut Vec<String>,
) {
    match (base, desired) {
        (toml::Value::Table(b), toml::Value::Table(d)) => {
            for (k, dv) in d {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                match b.get(k) {
                    Some(bv) => collect_diff_paths(bv, dv, path, out),
                    None => out.push(path),
                }
            }
        }
        (b, d) => {
            if b != d {
                out.push(prefix);
            }
        }
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
    /// Output resolution. `None` (default) captures at the monitor's native
    /// resolution; when set, capture is scaled to these dimensions (needs a
    /// waycap-rs build that honors capture scaling — otherwise a container hint).
    pub resolution: Option<Resolution>,
    /// Keyframe (GOP) interval in milliseconds. Smaller = finer "save last N"
    /// reachability and seeking, at a small bitrate cost. Default 2000 (2 s),
    /// matching gpu-screen-recorder's `-keyint`.
    pub keyframe_interval_ms: u32,
    /// Frame-timing mode: `cfr` (constant; safest for editing — the default),
    /// `vfr` (variable; lower encode load on static scenes), or `content` (sync
    /// capture to screen updates; smoothest under VRR/G-SYNC, works on portal).
    pub framerate_mode: FramerateMode,
    /// Encoded color range: `limited` (default, most compatible) or `full`.
    pub color_range: ColorRange,
    /// NVENC encoder tune: `performance` (default, lowest overhead) or `quality`.
    pub tune: EncoderTune,
    /// Where the replay buffer lives: `ram` (default, lowest latency) or `disk`
    /// (spill encoded frames to a file so the window can far exceed RAM, at the
    /// cost of a disk read per saved frame). gpu-screen-recorder's
    /// `-replay-storage`.
    pub replay_storage: ReplayStorage,
    /// Capture target: `portal` (default — pick via the XDG screencast dialog,
    /// reusing the saved restore token) or a monitor name like `DP-1`. Named
    /// monitors need a waycap-rs build with direct output capture; until then a
    /// name falls back to the portal.
    pub target: String,
    /// Auto-arm the replay buffer when a game takes the foreground (a Steam app
    /// or any fullscreen window). Off by default — the "set and forget" mode.
    pub auto_arm: bool,
    /// Capture HDR (10-bit, BT.2020/PQ). Requires an HEVC or AV1 codec and a
    /// KMS capture path (the XDG portal tonemaps to SDR), so HDR depends on the
    /// direct-monitor capture spike. Off by default.
    pub hdr: bool,
    /// Drop the whole buffer after a successful save, so consecutive saves
    /// never overlap (and the pre-save footage is gone — a privacy choice).
    pub clear_on_save: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fps: 60,
            buffer_seconds: 60,
            quality: Quality::High,
            codec: CaptureCodec::H264,
            bitrate_kbps: None,
            resolution: None,
            keyframe_interval_ms: 2000,
            framerate_mode: FramerateMode::Cfr,
            color_range: ColorRange::Limited,
            tune: EncoderTune::Performance,
            replay_storage: ReplayStorage::Ram,
            target: "portal".to_string(),
            auto_arm: false,
            hdr: false,
            clear_on_save: false,
        }
    }
}

/// Where the replay buffer's encoded frames are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplayStorage {
    /// In RAM (the default): lowest latency, bounded by available memory.
    Ram,
    /// Spilled to a file: long windows on low-RAM machines.
    Disk,
}

/// An explicit output resolution (pixels). NVENC requires even dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

/// How capture frame timing is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FramerateMode {
    /// Constant frame rate — one frame per tick, safest for editors.
    Cfr,
    /// Variable frame rate — skip unchanged frames, lower encode load.
    Vfr,
    /// Sync capture to on-screen content updates (best under VRR/G-SYNC).
    Content,
}

/// Encoded luma/chroma value range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorRange {
    Limited,
    Full,
}

/// NVENC rate-distortion tuning bias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncoderTune {
    Performance,
    Quality,
}

/// Where and how clips land on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Clips directory. `~` expands to `$HOME`. `None` = `~/Videos/open-recorder`.
    pub clips_dir: Option<String>,
    /// Directory for full-length manual recordings (`ord record`), kept separate
    /// from replay clips so simultaneous replay + recording never collide —
    /// gpu-screen-recorder's `-ro`. `~` expands to `$HOME`. `None` = same as
    /// `clips_dir`. Recordings here are never auto-pruned (they are deliberate).
    pub recordings_dir: Option<String>,
    /// Clip filename template (no extension). Tokens: `{game}` (detected
    /// foreground app or "clip"), `{rec}` (`""` for saved clips, `"-rec"` for
    /// full recordings), `{epoch}` (unix seconds), `{date}` (YYYY-MM-DD),
    /// `{time}` (HHMMSS). May contain `/` to create subfolders, e.g.
    /// `"{date}/{game}-{epoch}"` for date folders.
    pub template: String,
    /// Auto-prune: delete oldest clips when the library exceeds this many GiB.
    /// Exports (`exports/` subdirectory) are never touched. `None` = no limit.
    pub max_gib: Option<u32>,
    /// Auto-prune: delete clips older than this many days. `None` = no limit.
    pub max_age_days: Option<u32>,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            clips_dir: None,
            recordings_dir: None,
            template: "{game}{rec}-{epoch}".to_string(),
            max_gib: None,
            max_age_days: None,
        }
    }
}

/// In-buffer markers ("clip that" bookmarks placed with `ord mark`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarkersConfig {
    /// When set, placing a marker also saves the last N seconds immediately
    /// (marker + clip in one keypress). `None` = markers only annotate.
    pub auto_save_seconds: Option<u32>,
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

/// The on-screen HUD overlay (`ord-hud`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OverlayConfig {
    /// Show the persistent status dot in the screen corner (red = buffer
    /// armed, grey = daemon offline). Toasts are unaffected. Applies live.
    pub show_status_dot: bool,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            show_status_dot: true,
        }
    }
}

/// Audio capture settings.
///
/// The simple model is the `desktop`/`mic` booleans (mixed into one Opus track,
/// the historical behavior). For separate tracks and per-application audio,
/// populate `tracks`: each entry becomes its own output track mixing the listed
/// [`AudioSource`]s. When `tracks` is non-empty it takes precedence over the
/// booleans (see [`AudioConfig::effective_tracks`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Capture desktop/game output (the default sink monitor) — this also picks
    /// up friends' voices from a Discord/TeamSpeak call playing through speakers.
    pub desktop: bool,
    /// Capture the microphone (the default source) — your own voice.
    pub mic: bool,
    /// Explicit per-track capture (gpu-screen-recorder style). Each track is a
    /// separate audio stream in the output, mixing its sources. Enables
    /// separate game/mic tracks and per-application audio. Empty = use the
    /// `desktop`/`mic` booleans as a single mixed track.
    pub tracks: Vec<AudioTrack>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            desktop: true,
            mic: true,
            tracks: Vec::new(),
        }
    }
}

impl AudioConfig {
    /// Whether any audio capture is enabled.
    pub fn any(&self) -> bool {
        if self.tracks.is_empty() {
            self.desktop || self.mic
        } else {
            self.tracks.iter().any(|t| !t.sources.is_empty())
        }
    }

    /// The tracks to actually capture: the explicit `tracks` if any, otherwise a
    /// single track synthesized from the `desktop`/`mic` booleans (the legacy
    /// one-mixed-track behavior). An all-off legacy config yields no tracks.
    pub fn effective_tracks(&self) -> Vec<AudioTrack> {
        if !self.tracks.is_empty() {
            return self.tracks.clone();
        }
        let mut sources = Vec::new();
        if self.desktop {
            sources.push(AudioSource::DefaultOutput);
        }
        if self.mic {
            sources.push(AudioSource::DefaultInput);
        }
        if sources.is_empty() {
            Vec::new()
        } else {
            vec![AudioTrack {
                name: None,
                sources,
            }]
        }
    }
}

/// One output audio track: a set of sources mixed together.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioTrack {
    /// Optional label, carried into track metadata.
    pub name: Option<String>,
    /// Sources mixed into this track.
    pub sources: Vec<AudioSource>,
}

/// A selectable audio source, in gpu-screen-recorder's `-a` syntax. Serialized
/// as a string: `default_output`, `default_input`, `device:NAME`, `app:NAME`,
/// or `app-inverse:NAME`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum AudioSource {
    /// The default sink monitor (desktop/game audio).
    DefaultOutput,
    /// The default source (microphone).
    DefaultInput,
    /// A specific device by name.
    Device(String),
    /// A specific application's audio by name (case-insensitive).
    App(String),
    /// All applications EXCEPT the named one.
    AppInverse(String),
}

impl std::fmt::Display for AudioSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioSource::DefaultOutput => f.write_str("default_output"),
            AudioSource::DefaultInput => f.write_str("default_input"),
            AudioSource::Device(n) => write!(f, "device:{n}"),
            AudioSource::App(n) => write!(f, "app:{n}"),
            AudioSource::AppInverse(n) => write!(f, "app-inverse:{n}"),
        }
    }
}

impl std::str::FromStr for AudioSource {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s {
            "default_output" => Ok(AudioSource::DefaultOutput),
            "default_input" => Ok(AudioSource::DefaultInput),
            _ => {
                if let Some(n) = s.strip_prefix("device:") {
                    Ok(AudioSource::Device(n.to_string()))
                } else if let Some(n) = s.strip_prefix("app-inverse:") {
                    Ok(AudioSource::AppInverse(n.to_string()))
                } else if let Some(n) = s.strip_prefix("app:") {
                    Ok(AudioSource::App(n.to_string()))
                } else {
                    Err(format!(
                        "invalid audio source '{s}' (use default_output, default_input, device:NAME, app:NAME, app-inverse:NAME)"
                    ))
                }
            }
        }
    }
}

impl TryFrom<String> for AudioSource {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<AudioSource> for String {
    fn from(s: AudioSource) -> Self {
        s.to_string()
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

/// Default config path: the platform config dir + `open-recorder/config.toml`
/// (`$XDG_CONFIG_HOME` or `~/.config` on Linux, `~/Library/Application Support`
/// on macOS, `%APPDATA%` on Windows). Pure path construction — no I/O.
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("open-recorder/config.toml")
}

/// Runtime overrides path: the platform state dir (or data dir where there is no
/// distinct state dir) + `open-recorder/overrides.toml` (`$XDG_STATE_HOME` or
/// `~/.local/state` on Linux). The daemon is the only writer; the base config
/// (possibly a read-only Home Manager symlink) is never touched. Pure path
/// construction — no I/O.
pub fn overrides_path() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("open-recorder/overrides.toml")
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
        assert_eq!(c.capture.resolution, None);
        assert_eq!(c.capture.keyframe_interval_ms, 2000);
        assert_eq!(c.capture.framerate_mode, FramerateMode::Cfr);
        assert_eq!(c.capture.color_range, ColorRange::Limited);
        assert_eq!(c.capture.tune, EncoderTune::Performance);
        assert_eq!(c.capture.replay_storage, ReplayStorage::Ram);
        assert_eq!(c.capture.target, "portal");
        assert!(!c.capture.auto_arm);
        assert!(!c.capture.hdr);
        assert!(c.audio.desktop);
        assert!(c.audio.mic);
        assert_eq!(c.export.codec, ExportCodec::Av1);
        assert_eq!(c.export.container, Container::Mp4);
        assert_eq!(c.hooks.on_clip_saved, None);
        assert!(!c.capture.clear_on_save);
        assert_eq!(c.storage.clips_dir, None);
        assert_eq!(c.storage.template, "{game}{rec}-{epoch}");
        assert_eq!(c.storage.max_gib, None);
        assert_eq!(c.markers.auto_save_seconds, None);
        assert!(c.overlay.show_status_dot);
    }

    #[test]
    fn layered_overrides_win_and_rest_is_base() {
        let base = r#"
            [capture]
            fps = 30
            quality = "low"
        "#;
        let overrides = r#"
            [capture]
            quality = "ultra"
            [storage]
            max_gib = 50
        "#;
        let c = Config::from_layers(base, overrides).unwrap();
        assert_eq!(c.capture.fps, 30); // from base
        assert_eq!(c.capture.quality, Quality::Ultra); // overridden
        assert_eq!(c.storage.max_gib, Some(50)); // override-only section
        assert_eq!(c.capture.buffer_seconds, 60); // untouched default
    }

    #[test]
    fn empty_layers_yield_defaults() {
        assert_eq!(Config::from_layers("", "").unwrap(), Config::default());
    }

    #[test]
    fn diff_overrides_is_sparse_and_round_trips() {
        let base = Config::default();
        let mut desired = base.clone();
        desired.capture.fps = 120;
        desired.markers.auto_save_seconds = Some(20);

        let overrides = Config::diff_overrides(&base, &desired).unwrap();
        // Sparse: unchanged sections are absent.
        assert!(!overrides.contains("buffer_seconds"), "{overrides}");
        assert!(overrides.contains("fps"), "{overrides}");
        // Re-layering reproduces the desired config exactly.
        let base_toml = base.to_toml_string().unwrap();
        assert_eq!(
            Config::from_layers(&base_toml, &overrides).unwrap(),
            desired
        );
    }

    #[test]
    fn diff_overrides_empty_when_identical() {
        let c = Config::default();
        assert_eq!(Config::diff_overrides(&c, &c).unwrap(), "");
    }

    #[test]
    fn overridden_fields_are_dotted_paths() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.capture.fps = 144;
        changed.audio.mic = false;
        assert_eq!(
            changed.overridden_fields(&base),
            vec!["audio.mic".to_string(), "capture.fps".to_string()]
        );
        assert!(base.overridden_fields(&base).is_empty());
    }

    #[test]
    fn overrides_path_uses_state_home() {
        // Pure construction: ends with the well-known suffix either way.
        assert!(overrides_path().ends_with("open-recorder/overrides.toml"));
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
                resolution: Some(Resolution {
                    width: 1920,
                    height: 1080,
                }),
                keyframe_interval_ms: 1000,
                framerate_mode: FramerateMode::Content,
                color_range: ColorRange::Full,
                tune: EncoderTune::Quality,
                replay_storage: ReplayStorage::Disk,
                target: "DP-1".to_string(),
                auto_arm: true,
                hdr: true,
                clear_on_save: true,
            },
            audio: AudioConfig {
                desktop: false,
                mic: true,
                tracks: vec![
                    AudioTrack {
                        name: Some("game".into()),
                        sources: vec![AudioSource::DefaultOutput],
                    },
                    AudioTrack {
                        name: Some("voice".into()),
                        sources: vec![
                            AudioSource::App("discord".into()),
                            AudioSource::DefaultInput,
                        ],
                    },
                ],
            },
            export: ExportConfig {
                codec: ExportCodec::H264,
                container: Container::Mkv,
            },
            hooks: HooksConfig {
                on_clip_saved: Some("/usr/bin/notify-clip".into()),
            },
            storage: StorageConfig {
                clips_dir: Some("~/Clips".into()),
                recordings_dir: Some("~/Recordings".into()),
                template: "{date}/{game}-{epoch}".into(),
                max_gib: Some(25),
                max_age_days: Some(90),
            },
            markers: MarkersConfig {
                auto_save_seconds: Some(30),
            },
            overlay: OverlayConfig {
                show_status_dot: false,
            },
        };
        let toml = c.to_toml_string().unwrap();
        assert_eq!(Config::from_toml_str(&toml).unwrap(), c);
    }

    #[test]
    fn overlay_section_parses_and_diffs() {
        let c = Config::from_toml_str("[overlay]\nshow_status_dot = false").unwrap();
        assert!(!c.overlay.show_status_dot);
        let base = Config::default();
        assert_eq!(
            c.overridden_fields(&base),
            vec!["overlay.show_status_dot".to_string()]
        );
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
    fn capture_knobs_parse_and_default() {
        let c = Config::from_toml_str(
            r#"
            [capture]
            keyframe_interval_ms = 500
            framerate_mode = "vfr"
            color_range = "full"
            tune = "quality"

            [capture.resolution]
            width = 1920
            height = 1080
            "#,
        )
        .unwrap();
        assert_eq!(c.capture.keyframe_interval_ms, 500);
        assert_eq!(c.capture.framerate_mode, FramerateMode::Vfr);
        assert_eq!(c.capture.color_range, ColorRange::Full);
        assert_eq!(c.capture.tune, EncoderTune::Quality);
        assert_eq!(
            c.capture.resolution,
            Some(Resolution {
                width: 1920,
                height: 1080
            })
        );
        // Unset knobs keep defaults.
        assert_eq!(Config::default().capture.framerate_mode, FramerateMode::Cfr);
    }

    #[test]
    fn audio_source_string_round_trips() {
        use std::str::FromStr;
        let cases = [
            ("default_output", AudioSource::DefaultOutput),
            ("default_input", AudioSource::DefaultInput),
            (
                "device:alsa_output.x",
                AudioSource::Device("alsa_output.x".into()),
            ),
            ("app:Firefox", AudioSource::App("Firefox".into())),
            ("app-inverse:obs", AudioSource::AppInverse("obs".into())),
        ];
        for (s, want) in cases {
            assert_eq!(AudioSource::from_str(s).unwrap(), want);
            assert_eq!(want.to_string(), s);
        }
        assert!(AudioSource::from_str("bogus").is_err());
    }

    #[test]
    fn audio_tracks_parse_and_take_precedence() {
        let c = Config::from_toml_str(
            r#"
            [audio]
            desktop = true
            mic = true

            [[audio.tracks]]
            name = "game"
            sources = ["default_output"]

            [[audio.tracks]]
            sources = ["app:discord", "default_input"]
            "#,
        )
        .unwrap();
        assert_eq!(c.audio.tracks.len(), 2);
        // Explicit tracks win over the desktop/mic booleans.
        let eff = c.audio.effective_tracks();
        assert_eq!(eff.len(), 2);
        assert_eq!(eff[0].name.as_deref(), Some("game"));
        assert_eq!(eff[1].sources[0], AudioSource::App("discord".into()));
        assert!(c.audio.any());
    }

    #[test]
    fn effective_tracks_falls_back_to_booleans() {
        // No explicit tracks: synthesize one mixed track from desktop/mic.
        let mut a = AudioConfig::default();
        let eff = a.effective_tracks();
        assert_eq!(eff.len(), 1);
        assert_eq!(
            eff[0].sources,
            vec![AudioSource::DefaultOutput, AudioSource::DefaultInput]
        );
        // All off -> no tracks, no audio.
        a.desktop = false;
        a.mic = false;
        assert!(a.effective_tracks().is_empty());
        assert!(!a.any());
    }

    #[test]
    fn replay_storage_parses() {
        let c = Config::from_toml_str("[capture]\nreplay_storage = \"disk\"").unwrap();
        assert_eq!(c.capture.replay_storage, ReplayStorage::Disk);
        assert_eq!(Config::default().capture.replay_storage, ReplayStorage::Ram);
    }

    #[test]
    fn recordings_dir_defaults_none_and_overrides() {
        // Absent by default (recordings fall back to the clips dir).
        assert_eq!(Config::default().storage.recordings_dir, None);
        // Parses and is reported as a sparse override against the default.
        let c = Config::from_toml_str("[storage]\nrecordings_dir = \"~/Recordings\"").unwrap();
        assert_eq!(c.storage.recordings_dir.as_deref(), Some("~/Recordings"));
        assert_eq!(
            c.overridden_fields(&Config::default()),
            vec!["storage.recordings_dir".to_string()]
        );
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
