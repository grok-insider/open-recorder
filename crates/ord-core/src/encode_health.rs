//! Rolling encode-bitrate health: detects CBR undershoot (the class of bug
//! that produced ~1.5 Mbps AV1 clips when 12–50 Mbps was configured).

use std::time::{Duration, Instant};

/// Sustained encode rate below this fraction of the CBR target is "undershooting".
const UNDERSHOOT_RATIO: f64 = 0.5;
/// How long the rate must stay low before we raise an alarm.
const UNDERSHOOT_HOLD: Duration = Duration::from_secs(5);
/// Sample window length in stream time (seconds of pts coverage).
const WINDOW_SECS: i64 = 2;
/// Don't re-fire the alarm more often than this.
const ALARM_COOLDOWN: Duration = Duration::from_secs(30);

/// Tracks recent encoded video bytes vs pts and compares to an optional CBR target.
#[derive(Debug)]
pub struct EncodeHealth {
    target_kbps: Option<u32>,
    ticks_per_sec: i64,
    window_bytes: u64,
    window_start_pts: Option<i64>,
    last_pts: Option<i64>,
    /// Last computed rate over a completed window.
    last_kbps: Option<u32>,
    undershoot_since: Option<Instant>,
    last_alarm: Option<Instant>,
    pending_alarm: Option<String>,
}

impl EncodeHealth {
    /// Create a tracker. `target_kbps` is `Some` only in CBR mode.
    pub fn new(target_kbps: Option<u32>, ticks_per_sec: i64) -> Self {
        Self {
            target_kbps,
            ticks_per_sec: ticks_per_sec.max(1),
            window_bytes: 0,
            window_start_pts: None,
            last_pts: None,
            last_kbps: None,
            undershoot_since: None,
            last_alarm: None,
            pending_alarm: None,
        }
    }

    /// CBR target, if any.
    pub fn target_kbps(&self) -> Option<u32> {
        self.target_kbps
    }

    /// Most recent measured encode rate (kbps).
    pub fn encode_bitrate_kbps(&self) -> Option<u32> {
        self.last_kbps
    }

    /// Observe one encoded video frame.
    pub fn observe(&mut self, pts: i64, nbytes: usize) {
        if self.window_start_pts.is_none() {
            self.window_start_pts = Some(pts);
        }
        self.window_bytes = self.window_bytes.saturating_add(nbytes as u64);
        self.last_pts = Some(pts);

        let start = self.window_start_pts.unwrap_or(pts);
        let span_ticks = pts.saturating_sub(start);
        let window_ticks = WINDOW_SECS.saturating_mul(self.ticks_per_sec);
        if span_ticks < window_ticks {
            return;
        }

        // bits/s = bytes * 8 / seconds
        let seconds = span_ticks as f64 / self.ticks_per_sec as f64;
        if seconds > 0.0 {
            let kbps = ((self.window_bytes as f64) * 8.0 / seconds / 1000.0).round() as u32;
            self.last_kbps = Some(kbps);
            self.eval_undershoot(kbps);
        }

        // Start a fresh window from this frame.
        self.window_start_pts = Some(pts);
        self.window_bytes = 0;
    }

    fn eval_undershoot(&mut self, kbps: u32) {
        let Some(target) = self.target_kbps else {
            self.undershoot_since = None;
            return;
        };
        if target == 0 {
            return;
        }
        let threshold = (target as f64 * UNDERSHOOT_RATIO) as u32;
        if kbps >= threshold {
            self.undershoot_since = None;
            return;
        }
        let now = Instant::now();
        match self.undershoot_since {
            None => self.undershoot_since = Some(now),
            Some(since) if now.duration_since(since) >= UNDERSHOOT_HOLD => {
                let cool = self
                    .last_alarm
                    .map(|t| now.duration_since(t) >= ALARM_COOLDOWN)
                    .unwrap_or(true);
                if cool {
                    self.pending_alarm = Some(format!(
                        "Encoder under-delivering ({kbps} vs {target} kbps target) — clips will look blocky"
                    ));
                    self.last_alarm = Some(now);
                    self.undershoot_since = Some(now);
                }
            }
            Some(_) => {}
        }
    }

    /// Take a pending undershoot alarm message (if any).
    pub fn take_alarm(&mut self) -> Option<String> {
        self.pending_alarm.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_rate_over_window() {
        let mut h = EncodeHealth::new(Some(50_000), 1_000_000);
        // 2+ seconds of pts at 60 fps with 12.5 MB total → ~50 Mbps
        let n = 130u64;
        let bytes_per_frame = (12_500_000u64 / n) as usize;
        for i in 0..=n {
            h.observe((i * 1_000_000 / 60) as i64, bytes_per_frame);
        }
        let kbps = h.encode_bitrate_kbps().expect("rate");
        assert!(
            (35_000..=65_000).contains(&kbps),
            "expected ~50 Mbps, got {kbps}"
        );
    }

    #[test]
    fn no_alarm_without_cbr_target() {
        let mut h = EncodeHealth::new(None, 1_000_000);
        for i in 0..300 {
            h.observe(i * 10_000, 100); // tiny frames
        }
        assert!(h.take_alarm().is_none());
    }

    #[test]
    fn low_rate_marks_undershoot_tracking() {
        let mut h = EncodeHealth::new(Some(50_000), 1_000_000);
        // Complete a window at ~1 Mbps (far below 50 Mbps target).
        for i in 0..=150 {
            h.observe(i * 16_667, 2_000);
        }
        assert!(h.encode_bitrate_kbps().unwrap_or(u32::MAX) < 5_000);
        // Hold has not elapsed yet — no alarm on the first sample.
        assert!(h.take_alarm().is_none());
        // But undershoot is being tracked.
        assert!(h.undershoot_since.is_some());
    }
}
