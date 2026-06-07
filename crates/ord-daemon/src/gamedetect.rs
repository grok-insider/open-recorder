//! Best-effort game detection for clip filenames.
//!
//! Turns the foreground window/process into a short, filesystem-safe slug used in
//! clip names (e.g. `pathofexile-1780000000.mkv`). The OS-specific lookup is
//! isolated; the slug logic is pure and tested.

/// Slugify an application/window name into a filesystem-safe lowercase token.
/// Keeps ASCII alphanumerics, collapses everything else into single dashes, and
/// trims leading/trailing dashes. Returns `None` if nothing usable remains.
pub fn slugify(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Build a clip stem from an optional detected game name, falling back to
/// "clip". Never includes an extension.
pub fn clip_stem(game: Option<&str>) -> String {
    game.and_then(slugify).unwrap_or_else(|| "clip".to_string())
}

/// Detect the current foreground app name on Hyprland via `hyprctl activewindow`.
/// Returns `None` on any failure (this is best-effort cosmetics, never fatal).
#[cfg(target_os = "linux")]
pub fn detect_foreground() -> Option<String> {
    let out = std::process::Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    // Avoid a JSON dep: pull the "class" field value with a tiny scan.
    extract_json_string_field(&text, "class")
}

#[cfg(not(target_os = "linux"))]
pub fn detect_foreground() -> Option<String> {
    None
}

/// Minimal extractor for `"field": "value"` from a flat JSON object. Good enough
/// for hyprctl's output; avoids pulling a JSON parser into the daemon.
pub fn extract_json_string_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let colon = rest.find(':')? + 1;
    let rest = &rest[colon..];
    let q1 = rest.find('"')? + 1;
    let rest2 = &rest[q1..];
    let q2 = rest2.find('"')?;
    let val = &rest2[..q2];
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Path of Exile").as_deref(), Some("path-of-exile"));
        assert_eq!(
            slugify("Counter-Strike 2").as_deref(),
            Some("counter-strike-2")
        );
    }

    #[test]
    fn slugify_collapses_and_trims() {
        assert_eq!(
            slugify("  Hello___World!! ").as_deref(),
            Some("hello-world")
        );
        assert_eq!(
            slugify("steam_app_238960").as_deref(),
            Some("steam-app-238960")
        );
    }

    #[test]
    fn slugify_empty_or_symbols_only() {
        assert_eq!(slugify(""), None);
        assert_eq!(slugify("!!!"), None);
        assert_eq!(slugify("   "), None);
    }

    #[test]
    fn clip_stem_fallback() {
        assert_eq!(clip_stem(None), "clip");
        assert_eq!(clip_stem(Some("!!!")), "clip");
        assert_eq!(clip_stem(Some("Hades II")), "hades-ii");
    }

    #[test]
    fn extract_field_from_hyprctl_json() {
        let json = r#"{"address":"0x1","class":"steam_app_238960","title":"PoE"}"#;
        assert_eq!(
            extract_json_string_field(json, "class").as_deref(),
            Some("steam_app_238960")
        );
        assert_eq!(
            extract_json_string_field(json, "title").as_deref(),
            Some("PoE")
        );
    }

    #[test]
    fn extract_field_missing_or_empty() {
        let json = r#"{"class":"","title":"X"}"#;
        assert_eq!(extract_json_string_field(json, "class"), None);
        assert_eq!(extract_json_string_field(json, "nope"), None);
    }
}
