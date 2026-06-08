//! Pure trim-timeline model: in/out points and a playhead over a clip's
//! duration. No I/O, fully tested; the editor view renders and drives it.

/// Smallest selectable window, so in/out can't cross or collapse.
const MIN_SEL: f64 = 0.1;

/// In/out trim points and a scrub playhead, all in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    duration: f64,
    in_point: f64,
    out_point: f64,
    playhead: f64,
}

impl Timeline {
    /// A timeline spanning a clip of `duration` seconds, selection = whole clip.
    pub fn new(duration: f64) -> Self {
        let d = if duration.is_finite() && duration > 0.0 {
            duration
        } else {
            0.0
        };
        Self {
            duration: d,
            in_point: 0.0,
            out_point: d,
            playhead: 0.0,
        }
    }

    pub fn duration(&self) -> f64 {
        self.duration
    }
    pub fn in_point(&self) -> f64 {
        self.in_point
    }
    pub fn out_point(&self) -> f64 {
        self.out_point
    }
    pub fn playhead(&self) -> f64 {
        self.playhead
    }
    /// Length of the trimmed selection.
    pub fn selection_duration(&self) -> f64 {
        (self.out_point - self.in_point).max(0.0)
    }
    /// Whether the selection still covers the whole clip (nothing trimmed).
    pub fn is_full(&self) -> bool {
        self.in_point <= 0.0 && self.out_point >= self.duration
    }

    fn clamp(&self, t: f64) -> f64 {
        t.clamp(0.0, self.duration)
    }

    /// Move the in-point, clamped to `[0, out - MIN_SEL]`. The playhead follows
    /// the handle so the preview shows the frame being trimmed to.
    pub fn set_in(&mut self, t: f64) {
        self.in_point = self.clamp(t).min(self.out_point - MIN_SEL).max(0.0);
        self.playhead = self.in_point;
    }

    /// Move the out-point, clamped to `[in + MIN_SEL, duration]`. The playhead
    /// follows the handle.
    pub fn set_out(&mut self, t: f64) {
        self.out_point = self
            .clamp(t)
            .max(self.in_point + MIN_SEL)
            .min(self.duration);
        self.playhead = self.out_point;
    }

    /// Move the playhead (free scrub), clamped to the whole clip — you can
    /// preview frames outside the trimmed selection.
    pub fn set_playhead(&mut self, t: f64) {
        self.playhead = self.clamp(t);
    }

    /// Fraction `[0,1]` of a time across the clip (for drawing).
    pub fn fraction(&self, t: f64) -> f32 {
        if self.duration <= 0.0 {
            0.0
        } else {
            (t / self.duration).clamp(0.0, 1.0) as f32
        }
    }

    /// Time in seconds at a fraction `[0,1]` of the clip.
    pub fn time_at(&self, frac: f32) -> f64 {
        (frac.clamp(0.0, 1.0) as f64) * self.duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn new_spans_whole_clip() {
        let t = Timeline::new(30.0);
        assert!(approx(t.in_point(), 0.0));
        assert!(approx(t.out_point(), 30.0));
        assert!(approx(t.playhead(), 0.0));
        assert!(approx(t.selection_duration(), 30.0));
        assert!(t.is_full());
    }

    #[test]
    fn invalid_duration_is_zero() {
        assert!(approx(Timeline::new(-1.0).duration(), 0.0));
        assert!(approx(Timeline::new(f64::NAN).duration(), 0.0));
    }

    #[test]
    fn set_in_clamps_below_out() {
        let mut t = Timeline::new(10.0);
        t.set_out(8.0);
        t.set_in(9.0); // past out -> clamps to out - MIN_SEL
        assert!(approx(t.in_point(), 8.0 - MIN_SEL));
    }

    #[test]
    fn set_out_clamps_above_in() {
        let mut t = Timeline::new(10.0);
        t.set_in(3.0);
        t.set_out(1.0); // before in -> clamps to in + MIN_SEL
        assert!(approx(t.out_point(), 3.0 + MIN_SEL));
    }

    #[test]
    fn playhead_follows_handles_then_scrubs_freely() {
        let mut t = Timeline::new(10.0);
        t.set_playhead(1.0);
        t.set_in(5.0);
        assert!(approx(t.playhead(), 5.0)); // follows the in handle
        t.set_out(7.0);
        assert!(approx(t.playhead(), 7.0)); // follows the out handle
        t.set_playhead(9.0);
        assert!(approx(t.playhead(), 9.0)); // bar scrub is free across the clip
    }

    #[test]
    fn fraction_and_time_roundtrip() {
        let t = Timeline::new(20.0);
        assert!((t.fraction(10.0) - 0.5).abs() < 1e-6);
        assert!(approx(t.time_at(0.25), 5.0));
        assert_eq!(t.fraction(-5.0), 0.0);
        assert_eq!(t.fraction(40.0), 1.0);
    }

    #[test]
    fn trimming_marks_not_full() {
        let mut t = Timeline::new(10.0);
        t.set_in(2.0);
        assert!(!t.is_full());
    }
}
