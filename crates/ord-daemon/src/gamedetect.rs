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
    // Avoid a JSON dep: pull the fields with a tiny scan.
    pick_name(
        extract_json_string_field(&text, "class"),
        extract_json_string_field(&text, "title"),
    )
}

/// Heuristic for auto-arm: is the foreground window a game we should start the
/// replay buffer for? A Steam app (`steam_app_<id>` class) always counts;
/// otherwise a fullscreen window does (the common case for games on Hyprland).
/// Pure + tested; the live probe feeds it `class`/`fullscreen` from hyprctl.
pub fn is_game_window(class: Option<&str>, fullscreen: bool) -> bool {
    class.is_some_and(is_steam_app_class) || fullscreen
}

/// Probe the foreground window and decide whether it looks like a game (for
/// `capture.auto_arm`). Best-effort: any failure returns `false`.
#[cfg(target_os = "linux")]
pub fn foreground_is_game() -> bool {
    let Ok(out) = std::process::Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let Ok(text) = String::from_utf8(out.stdout) else {
        return false;
    };
    let class = extract_json_string_field(&text, "class");
    let fullscreen = extract_json_int_field(&text, "fullscreen").unwrap_or(0) != 0;
    is_game_window(class.as_deref(), fullscreen)
}

#[cfg(not(target_os = "linux"))]
pub fn foreground_is_game() -> bool {
    false
}

/// Whether a window class is a cryptic Steam app id (`steam_app_<digits>`),
/// whose human name lives in the title instead.
fn is_steam_app_class(class: &str) -> bool {
    class
        .strip_prefix("steam_app_")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Choose the best human-facing name from a window's class and title. Steam games
/// have a `steam_app_<id>` class, so their real name (e.g. "Path of Exile 2") is
/// in the title; everything else uses the (usually descriptive) class.
pub fn pick_name(class: Option<String>, title: Option<String>) -> Option<String> {
    match class {
        Some(c) if is_steam_app_class(&c) => title.or(Some(c)),
        Some(c) => Some(c),
        None => title,
    }
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

/// Minimal extractor for a numeric `"field": N` from a flat JSON object (e.g.
/// hyprctl's `fullscreen`). Returns `None` if absent or non-numeric.
pub fn extract_json_int_field(json: &str, field: &str) -> Option<i64> {
    let needle = format!("\"{field}\"");
    let start = json.find(&needle)? + needle.len();
    let rest = &json[start..];
    let colon = rest.find(':')? + 1;
    let rest = rest[colon..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
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

    #[test]
    fn extract_int_field_reads_fullscreen() {
        let json = r#"{"class":"steam_app_1","fullscreen":2,"title":"X"}"#;
        assert_eq!(extract_json_int_field(json, "fullscreen"), Some(2));
        assert_eq!(extract_json_int_field(json, "missing"), None);
        let zero = r#"{"fullscreen":0}"#;
        assert_eq!(extract_json_int_field(zero, "fullscreen"), Some(0));
    }

    #[test]
    fn game_window_heuristic() {
        // Steam app -> game regardless of fullscreen.
        assert!(is_game_window(Some("steam_app_1145360"), false));
        // Fullscreen non-steam window -> treated as a game.
        assert!(is_game_window(Some("gamescope"), true));
        // Windowed browser -> not a game.
        assert!(!is_game_window(Some("firefox"), false));
        // Nothing focused -> not a game.
        assert!(!is_game_window(None, false));
    }

    #[test]
    fn steam_app_class_prefers_title() {
        // A Steam game: cryptic class -> use the human title.
        assert_eq!(
            pick_name(
                Some("steam_app_2694490".into()),
                Some("Path of Exile 2".into())
            )
            .as_deref(),
            Some("Path of Exile 2")
        );
        // No title -> fall back to the class.
        assert_eq!(
            pick_name(Some("steam_app_2694490".into()), None).as_deref(),
            Some("steam_app_2694490")
        );
        // Non-Steam app -> the class is already descriptive.
        assert_eq!(
            pick_name(Some("brave-browser".into()), Some("Some Tab".into())).as_deref(),
            Some("brave-browser")
        );
        // Only a title available.
        assert_eq!(pick_name(None, Some("mpv".into())).as_deref(), Some("mpv"));
    }
}
