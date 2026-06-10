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
}

/// Smallest piece a split may produce (avoids degenerate slivers).
const MIN_SEG: f64 = 0.05;

/// One contiguous piece of the clip between cut points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    /// Disabled segments are skipped during playback and dropped on export.
    pub enabled: bool,
}

/// A multi-segment cut list over a clip: split at the playhead, toggle pieces
/// off, and the kept pieces play (and export) joined back together. Pure —
/// the editor view renders and drives it.
#[derive(Debug, Clone, PartialEq)]
pub struct Segments {
    duration: f64,
    segs: Vec<Segment>,
}

impl Segments {
    /// One enabled segment covering the whole clip.
    pub fn new(duration: f64) -> Self {
        let d = if duration.is_finite() && duration > 0.0 {
            duration
        } else {
            0.0
        };
        Self {
            duration: d,
            segs: vec![Segment {
                start: 0.0,
                end: d,
                enabled: true,
            }],
        }
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segs
    }

    /// No cuts and nothing disabled — playback/export can ignore the model.
    pub fn is_trivial(&self) -> bool {
        self.segs.len() == 1 && self.segs[0].enabled
    }

    /// Interior cut boundaries (for drawing).
    pub fn cuts(&self) -> impl Iterator<Item = f64> + '_ {
        self.segs.iter().skip(1).map(|s| s.start)
    }

    /// Index of the segment containing `t`.
    pub fn index_at(&self, t: f64) -> Option<usize> {
        if self.segs.is_empty() || t < 0.0 || t > self.duration {
            return None;
        }
        match self.segs.iter().position(|s| t < s.end) {
            Some(i) => Some(i),
            None => Some(self.segs.len() - 1), // t == duration
        }
    }

    /// Split the segment containing `t` in two. No-op too close to an edge.
    pub fn split_at(&mut self, t: f64) -> bool {
        let Some(i) = self.index_at(t) else {
            return false;
        };
        let seg = self.segs[i];
        if t - seg.start < MIN_SEG || seg.end - t < MIN_SEG {
            return false;
        }
        self.segs[i].end = t;
        self.segs.insert(
            i + 1,
            Segment {
                start: t,
                end: seg.end,
                enabled: seg.enabled,
            },
        );
        true
    }

    /// Toggle the segment containing `t` between kept and cut.
    pub fn toggle_at(&mut self, t: f64) -> bool {
        let Some(i) = self.index_at(t) else {
            return false;
        };
        self.segs[i].enabled = !self.segs[i].enabled;
        true
    }

    /// Drop every cut, back to one enabled full-clip segment.
    pub fn reset(&mut self) {
        *self = Self::new(self.duration);
    }

    /// The kept spans intersected with `[in_p, out_p]`, adjacent spans merged —
    /// exactly what plays and what an export concatenates.
    pub fn kept_within(&self, in_p: f64, out_p: f64) -> Vec<(f64, f64)> {
        let mut out: Vec<(f64, f64)> = Vec::new();
        for s in self.segs.iter().filter(|s| s.enabled) {
            let a = s.start.max(in_p);
            let b = s.end.min(out_p);
            if b - a <= MIN_SEG / 2.0 {
                continue;
            }
            match out.last_mut() {
                Some(last) if (a - last.1).abs() < 1e-9 => last.1 = b,
                _ => out.push((a, b)),
            }
        }
        out
    }

    /// Total kept duration within `[in_p, out_p]`.
    pub fn kept_duration(&self, in_p: f64, out_p: f64) -> f64 {
        self.kept_within(in_p, out_p)
            .iter()
            .map(|(a, b)| b - a)
            .sum()
    }

    /// Where playback should jump when the playhead sits in a cut segment:
    /// the start of the next kept segment, or `out_p` when nothing kept
    /// remains (the caller then stops/loops). `None` = no skip needed.
    pub fn skip_target(&self, t: f64, out_p: f64) -> Option<f64> {
        let i = self.index_at(t)?;
        if self.segs[i].enabled {
            return None;
        }
        let next = self.segs[i + 1..]
            .iter()
            .find(|s| s.enabled && s.end > t)
            .map(|s| s.start.max(t));
        Some(next.unwrap_or(out_p).min(out_p))
    }
}

/// A zoom/scroll window over the clip, mapping time <-> on-track fraction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub start: f64,
    pub span: f64,
}

impl View {
    /// Window for `duration` at `zoom` (>=1; 1 = whole clip) scrolled to
    /// `scroll_frac` in `[0,1]` of the available range.
    pub fn new(duration: f64, zoom: f32, scroll_frac: f32) -> Self {
        let zoom = (zoom as f64).max(1.0);
        let span = (duration / zoom).max(1e-6);
        let max_start = (duration - span).max(0.0);
        let start = (scroll_frac.clamp(0.0, 1.0) as f64) * max_start;
        Self { start, span }
    }

    /// Fraction across the view for a time (may be <0 or >1 if off-screen).
    pub fn frac_of(&self, t: f64) -> f32 {
        ((t - self.start) / self.span) as f32
    }

    /// Time at a fraction across the view.
    pub fn time_at(&self, frac: f32) -> f64 {
        self.start + (frac as f64) * self.span
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn view_zoom_and_scroll() {
        let v = View::new(30.0, 1.0, 0.0);
        assert!(approx(v.start, 0.0) && approx(v.span, 30.0));
        let v = View::new(30.0, 2.0, 0.0);
        assert!(approx(v.start, 0.0) && approx(v.span, 15.0));
        let v = View::new(30.0, 2.0, 1.0);
        assert!(approx(v.start, 15.0) && approx(v.span, 15.0));
        // round-trip
        let v = View::new(30.0, 3.0, 0.5);
        assert!(approx(v.time_at(v.frac_of(12.0)), 12.0));
        assert!((v.frac_of(v.start) - 0.0).abs() < 1e-6);
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
    fn trimming_marks_not_full() {
        let mut t = Timeline::new(10.0);
        t.set_in(2.0);
        assert!(!t.is_full());
    }

    #[test]
    fn segments_start_trivial_and_split() {
        let mut s = Segments::new(10.0);
        assert!(s.is_trivial());
        assert!(s.split_at(4.0));
        assert!(!s.is_trivial());
        assert_eq!(s.segments().len(), 2);
        assert!(approx(s.segments()[0].end, 4.0));
        assert!(approx(s.segments()[1].start, 4.0));
        assert_eq!(s.cuts().collect::<Vec<_>>(), vec![4.0]);
        // Splitting on (or too near) an existing edge is a no-op.
        assert!(!s.split_at(4.0));
        assert!(!s.split_at(0.01));
        assert!(!s.split_at(9.99));
        assert_eq!(s.segments().len(), 2);
    }

    #[test]
    fn segments_toggle_and_kept_spans() {
        let mut s = Segments::new(10.0);
        s.split_at(3.0);
        s.split_at(7.0);
        assert!(s.toggle_at(5.0)); // disable the middle piece
        assert!(!s.segments()[1].enabled);
        assert_eq!(s.kept_within(0.0, 10.0), vec![(0.0, 3.0), (7.0, 10.0)]);
        assert!(approx(s.kept_duration(0.0, 10.0), 6.0));
        // Intersected with a narrower in/out window.
        assert_eq!(s.kept_within(1.0, 8.0), vec![(1.0, 3.0), (7.0, 8.0)]);
        // Re-enabling merges adjacent kept spans back into one.
        s.toggle_at(5.0);
        assert_eq!(s.kept_within(0.0, 10.0), vec![(0.0, 10.0)]);
    }

    #[test]
    fn segments_skip_target_jumps_over_cuts() {
        let mut s = Segments::new(10.0);
        s.split_at(3.0);
        s.split_at(7.0);
        s.toggle_at(5.0);
        assert_eq!(s.skip_target(1.0, 10.0), None); // in a kept piece
        assert!(approx(s.skip_target(4.0, 10.0).unwrap(), 7.0));
        // A trailing cut with nothing kept after lands on out.
        s.toggle_at(8.0);
        assert!(approx(s.skip_target(4.0, 10.0).unwrap(), 10.0));
        assert!(approx(s.skip_target(8.0, 9.0).unwrap(), 9.0));
    }

    #[test]
    fn segments_reset_restores_full_clip() {
        let mut s = Segments::new(10.0);
        s.split_at(5.0);
        s.toggle_at(6.0);
        s.reset();
        assert!(s.is_trivial());
        assert_eq!(s.kept_within(0.0, 10.0), vec![(0.0, 10.0)]);
    }

    #[test]
    fn segments_invalid_duration_is_safe() {
        let mut s = Segments::new(f64::NAN);
        assert!(!s.split_at(1.0));
        assert!(s.kept_within(0.0, 1.0).is_empty());
    }
}
