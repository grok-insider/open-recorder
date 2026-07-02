//! Tiny persisted UI preferences (gui-only): the editor's volume and loop
//! toggle, written as a two-line `key=value` file in the XDG state dir.
//! eframe persistence is not enabled in this build, so a plain state file is
//! the reliable path; loading is pure and tested, I/O is best-effort.

use std::path::PathBuf;

/// Editor playback preferences, persisted across editor opens and restarts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EditorPrefs {
    pub volume: f32,
    pub looping: bool,
}

impl Default for EditorPrefs {
    fn default() -> Self {
        Self {
            volume: 1.0,
            looping: false,
        }
    }
}

/// `$XDG_STATE_HOME/open-recorder/ui-prefs` (falling back to the local data
/// dir on platforms without a state dir).
fn prefs_path() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("open-recorder/ui-prefs"))
}

/// Load the saved preferences; any missing/unreadable/garbled file yields the
/// defaults (preferences must never block the editor from opening).
pub fn load() -> EditorPrefs {
    prefs_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse(&s))
        .unwrap_or_default()
}

/// Persist the preferences (best-effort; callers ignore the error).
pub fn save(prefs: EditorPrefs) -> std::io::Result<()> {
    let path = prefs_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no state directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, render(prefs))
}

/// Parse the `key=value` lines; unknown keys and malformed values fall back
/// to the defaults, and volume is clamped to `[0, 1]`.
pub fn parse(s: &str) -> EditorPrefs {
    let mut prefs = EditorPrefs::default();
    for line in s.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "volume" => {
                if let Ok(v) = value.trim().parse::<f32>() {
                    if v.is_finite() {
                        prefs.volume = v.clamp(0.0, 1.0);
                    }
                }
            }
            "loop" => prefs.looping = value.trim() == "true",
            _ => {}
        }
    }
    prefs
}

/// Serialize to the `key=value` file format.
pub fn render(prefs: EditorPrefs) -> String {
    format!("volume={}\nloop={}\n", prefs.volume, prefs.looping)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for prefs in [
            EditorPrefs::default(),
            EditorPrefs {
                volume: 0.35,
                looping: true,
            },
            EditorPrefs {
                volume: 0.0,
                looping: false,
            },
        ] {
            assert_eq!(parse(&render(prefs)), prefs);
        }
    }

    #[test]
    fn garbage_and_missing_fields_yield_defaults() {
        assert_eq!(parse(""), EditorPrefs::default());
        assert_eq!(parse("not a prefs file\n\x00"), EditorPrefs::default());
        assert_eq!(parse("volume=abc\nloop=maybe"), EditorPrefs::default());
        let p = parse("loop=true");
        assert_eq!(p.volume, 1.0);
        assert!(p.looping);
    }

    #[test]
    fn volume_is_clamped_and_finite() {
        assert_eq!(parse("volume=7.5").volume, 1.0);
        assert_eq!(parse("volume=-2").volume, 0.0);
        assert_eq!(parse("volume=NaN").volume, 1.0);
        assert_eq!(parse("volume=inf").volume, 1.0);
    }
}
