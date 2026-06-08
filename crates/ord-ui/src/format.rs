//! Pure human-readable formatters for clip metadata. No I/O, fully tested.

/// Format a byte count as a binary-prefixed size, e.g. `12.3 MiB`.
pub fn human_size(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.0} KiB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MiB", b / (K * K))
    } else {
        format!("{:.2} GiB", b / (K * K * K))
    }
}

/// Format a duration in seconds as `m:ss` (or `h:mm:ss` past an hour).
pub fn human_duration(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "—".to_string();
    }
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Format an epoch-seconds timestamp relative to `now`, e.g. `3 min ago`.
/// Returns `—` for a missing (zero) timestamp.
pub fn relative_time(epoch: u64, now: u64) -> String {
    if epoch == 0 {
        return "—".to_string();
    }
    // Clocks/filenames can drift slightly into the future; treat as "just now".
    let d = now.saturating_sub(epoch);
    match d {
        0..=4 => "just now".to_string(),
        5..=59 => format!("{d}s ago"),
        60..=3599 => format!("{} min ago", d / 60),
        3600..=86399 => format!("{} h ago", d / 3600),
        86400..=172_799 => "yesterday".to_string(),
        _ => format!("{} d ago", d / 86400),
    }
}

/// Compact resolution label, e.g. `2560×1440` or `—` if unknown.
pub fn resolution(width: u32, height: u32) -> String {
    if width == 0 || height == 0 {
        "—".to_string()
    } else {
        format!("{width}×{height}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KiB");
        assert_eq!(human_size(15 * 1024 * 1024 + 300 * 1024), "15.3 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.00 GiB");
    }

    #[test]
    fn durations() {
        assert_eq!(human_duration(5.0), "0:05");
        assert_eq!(human_duration(83.0), "1:23");
        assert_eq!(human_duration(3661.0), "1:01:01");
        assert_eq!(human_duration(-1.0), "—");
        assert_eq!(human_duration(f64::NAN), "—");
    }

    #[test]
    fn relative_times() {
        let now = 1_000_000;
        assert_eq!(relative_time(0, now), "—");
        assert_eq!(relative_time(now, now), "just now");
        assert_eq!(relative_time(now - 30, now), "30s ago");
        assert_eq!(relative_time(now - 120, now), "2 min ago");
        assert_eq!(relative_time(now - 7200, now), "2 h ago");
        assert_eq!(relative_time(now - 90_000, now), "yesterday");
        assert_eq!(relative_time(now - 300_000, now), "3 d ago");
        // Future timestamp clamps to "just now" rather than underflowing.
        assert_eq!(relative_time(now + 50, now), "just now");
    }

    #[test]
    fn resolutions() {
        assert_eq!(resolution(2560, 1440), "2560×1440");
        assert_eq!(resolution(0, 0), "—");
    }
}
