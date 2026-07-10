//! Compositor output probe — enumerate monitors for auto FPS / settings UI.
//!
//! Hyprland-first via `hyprctl monitors -j` (same family as game detect). Parse
//! is pure and unit-tested against fixtures so CI needs no compositor.

use ord_common::OutputInfo;

/// How long a `hyprctl monitors` probe may run before it is killed.
#[cfg(target_os = "linux")]
const HYPRCTL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Enumerate connected displays. Empty when the probe is unavailable
/// (non-Linux, missing hyprctl, timeout, bad JSON).
pub fn list_outputs() -> Vec<OutputInfo> {
    #[cfg(target_os = "linux")]
    {
        hyprctl_monitors_json()
            .and_then(|j| parse_hyprctl_monitors(&j).ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Upper bound for encode FPS when `fps_mode = auto`.
///
/// Matching a 240 Hz display 1:1 is rarely sustainable for portal + NVENC and
/// tends to produce a stalled session (watchdog restart loop). Cap auto to a
/// practical encode rate; users who want higher set `fps_mode = fixed`.
pub const MAX_AUTO_ENCODE_FPS: u32 = 144;

/// Resolve the integer FPS to use when `fps_mode = auto`.
///
/// - Named target (`DP-1`): match that connector; fallback 60.
/// - `portal` / unknown: focused output, else highest refresh, else 60.
/// - Always clamped to [`MAX_AUTO_ENCODE_FPS`].
pub fn resolve_auto_fps(outputs: &[OutputInfo], target: &str) -> u32 {
    const FALLBACK: u32 = 60;
    if outputs.is_empty() {
        return FALLBACK.min(MAX_AUTO_ENCODE_FPS);
    }
    let t = target.trim();
    let raw = if t != "portal" && !t.is_empty() {
        outputs
            .iter()
            .find(|o| o.name == t)
            .map(OutputInfo::refresh_fps)
            .unwrap_or(FALLBACK)
    } else if let Some(o) = outputs.iter().find(|o| o.focused) {
        o.refresh_fps()
    } else {
        outputs
            .iter()
            .max_by_key(|o| o.refresh_mhz)
            .map(OutputInfo::refresh_fps)
            .unwrap_or(FALLBACK)
    };
    raw.clamp(1, MAX_AUTO_ENCODE_FPS)
}

/// Pick a monitor for a native-resolution container hint when the target is a
/// named output. `None` for portal / unknown (size known only after arm).
pub fn resolve_native_hint(outputs: &[OutputInfo], target: &str) -> Option<(u32, u32)> {
    let t = target.trim();
    if t == "portal" || t.is_empty() {
        return None;
    }
    outputs
        .iter()
        .find(|o| o.name == t)
        .map(|o| (o.width, o.height))
}

/// Parse `hyprctl monitors -j` JSON into [`OutputInfo`]s.
pub fn parse_hyprctl_monitors(json: &str) -> Result<Vec<OutputInfo>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("hyprctl monitors json: {e}"))?;
    let arr = v
        .as_array()
        .ok_or_else(|| "hyprctl monitors: expected a JSON array".to_string())?;
    let mut out = Vec::with_capacity(arr.len());
    for mon in arr {
        let name = mon
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let width = mon.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        let height = mon.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        // Hyprland uses `refreshRate` as a float (e.g. 144.003) or occasionally
        // an integer. Convert to milli-Hz for stable integer math.
        let refresh_mhz = parse_refresh_mhz(mon.get("refreshRate"));
        let focused = mon
            .get("focused")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        out.push(OutputInfo {
            name,
            width,
            height,
            refresh_mhz,
            focused,
        });
    }
    Ok(out)
}

fn parse_refresh_mhz(v: Option<&serde_json::Value>) -> u32 {
    match v {
        Some(serde_json::Value::Number(n)) => {
            if let Some(f) = n.as_f64() {
                (f * 1000.0).round().max(0.0) as u32
            } else if let Some(i) = n.as_u64() {
                // Integer Hz → milli-Hz.
                i.saturating_mul(1000) as u32
            } else {
                0
            }
        }
        Some(serde_json::Value::String(s)) => s
            .parse::<f64>()
            .ok()
            .map(|f| (f * 1000.0).round().max(0.0) as u32)
            .unwrap_or(0),
        _ => 0,
    }
}

#[cfg(target_os = "linux")]
fn hyprctl_monitors_json() -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let mut child = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + HYPRCTL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = rx
                    .recv_timeout(std::time::Duration::from_millis(200))
                    .ok()?;
                return status.success().then_some(text);
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"[
      {
        "id": 0,
        "name": "DP-1",
        "description": "Monitor",
        "width": 2560,
        "height": 1440,
        "refreshRate": 165.002,
        "focused": true
      },
      {
        "id": 1,
        "name": "HDMI-A-1",
        "width": 1920,
        "height": 1080,
        "refreshRate": 59.940,
        "focused": false
      }
    ]"#;

    #[test]
    fn parses_multi_monitor_fixture() {
        let outs = parse_hyprctl_monitors(FIXTURE).unwrap();
        assert_eq!(outs.len(), 2);
        assert_eq!(outs[0].name, "DP-1");
        assert_eq!(outs[0].width, 2560);
        assert_eq!(outs[0].height, 1440);
        assert!(outs[0].focused);
        assert_eq!(outs[0].refresh_fps(), 165);
        assert_eq!(outs[1].name, "HDMI-A-1");
        assert_eq!(outs[1].refresh_fps(), 60);
    }

    #[test]
    fn resolve_auto_fps_prefers_named_then_focused() {
        let outs = parse_hyprctl_monitors(FIXTURE).unwrap();
        assert_eq!(resolve_auto_fps(&outs, "HDMI-A-1"), 60);
        // DP-1 is 165 Hz focused; auto encode caps at MAX_AUTO_ENCODE_FPS.
        assert_eq!(
            resolve_auto_fps(&outs, "portal"),
            MAX_AUTO_ENCODE_FPS.min(165)
        );
        assert_eq!(resolve_auto_fps(&outs, "missing"), 60);
        assert_eq!(resolve_auto_fps(&[], "portal"), 60);
    }

    #[test]
    fn resolve_auto_fps_caps_high_refresh_for_encode() {
        let json =
            r#"[{"name":"DP-1","width":2560,"height":1440,"refreshRate":239.99,"focused":true}]"#;
        let outs = parse_hyprctl_monitors(json).unwrap();
        assert_eq!(outs[0].refresh_fps(), 240);
        assert_eq!(resolve_auto_fps(&outs, "portal"), MAX_AUTO_ENCODE_FPS);
        assert_eq!(resolve_auto_fps(&outs, "DP-1"), MAX_AUTO_ENCODE_FPS);
    }

    #[test]
    fn resolve_native_hint_only_for_named() {
        let outs = parse_hyprctl_monitors(FIXTURE).unwrap();
        assert_eq!(resolve_native_hint(&outs, "DP-1"), Some((2560, 1440)));
        assert_eq!(resolve_native_hint(&outs, "portal"), None);
    }

    #[test]
    fn integer_refresh_rate_is_hz() {
        let j = r#"[{"name":"eDP-1","width":1920,"height":1080,"refreshRate":60,"focused":true}]"#;
        let outs = parse_hyprctl_monitors(j).unwrap();
        assert_eq!(outs[0].refresh_mhz, 60_000);
        assert_eq!(outs[0].refresh_fps(), 60);
    }
}
