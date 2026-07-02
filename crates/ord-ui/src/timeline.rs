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

    /// Interior cut boundaries with their index (for dragging). Boundary `i`
    /// sits between `segments()[i-1]` and `segments()[i]`.
    pub fn cut_points(&self) -> impl Iterator<Item = (usize, f64)> + '_ {
        self.segs
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, s)| (i, s.start))
    }

    /// Slide cut boundary `i` to `t`, clamped so neither neighbor collapses
    /// below the minimum piece size. Returns the time actually applied.
    pub fn move_cut(&mut self, i: usize, t: f64) -> Option<f64> {
        if i == 0 || i >= self.segs.len() {
            return None;
        }
        let lo = self.segs[i - 1].start + MIN_SEG;
        let hi = self.segs[i].end - MIN_SEG;
        if hi < lo {
            return None;
        }
        let t = t.clamp(lo, hi);
        self.segs[i - 1].end = t;
        self.segs[i].start = t;
        Some(t)
    }

    /// Remove the cut boundary nearest to `t`, joining its two pieces. The
    /// joined piece is kept if either side was kept (deleting a cut reads as
    /// "undo this cut", never as silently extending one). Returns the joined
    /// boundary's time.
    pub fn join_at(&mut self, t: f64) -> Option<f64> {
        let nearest = self
            .segs
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, s)| (i, (s.start - t).abs()))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
        let i = nearest.0;
        let boundary = self.segs[i].start;
        let right = self.segs.remove(i);
        let left = &mut self.segs[i - 1];
        left.end = right.end;
        left.enabled = left.enabled || right.enabled;
        Some(boundary)
    }

    /// Cut the range `[a, b]` out in one action: split at both ends and
    /// disable every piece inside. Splits too close to an existing boundary
    /// (or the clip edge) fall back to that boundary.
    pub fn cut_range(&mut self, a: f64, b: f64) -> bool {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let a = a.clamp(0.0, self.duration);
        let b = b.clamp(0.0, self.duration);
        if b - a < MIN_SEG {
            return false;
        }
        self.split_at(a);
        self.split_at(b);
        let mut changed = false;
        for s in &mut self.segs {
            if s.start >= a - MIN_SEG && s.end <= b + MIN_SEG && s.enabled {
                s.enabled = false;
                changed = true;
            }
        }
        changed
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

/// What a drag starting at an on-track pixel grabs. The grab radius is wider
/// than the visual handles (the fat-finger rule); in/out handles win over cut
/// lines, and everything else scrubs the playhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragTarget {
    In,
    Out,
    /// Sliding cut boundary `i` (see [`Segments::cut_points`]).
    Cut(usize),
    Playhead,
}

/// The cut line nearest `px` within `grab` px. `cuts` entries are
/// `(boundary index, boundary time, on-screen x)`; returns `(index, time)`.
pub fn nearest_cut(px: f32, cuts: &[(usize, f64, f32)], grab: f32) -> Option<(usize, f64)> {
    cuts.iter()
        .map(|&(i, t, x)| (i, t, (x - px).abs()))
        .filter(|(_, _, d)| *d <= grab)
        .min_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, t, _)| (i, t))
}

/// Classify what a drag starting at `px` grabs: the nearer of the in/out
/// handles when within `grab`, else a cut line within `grab`, else the
/// playhead (a scrub).
pub fn classify_drag(
    px: f32,
    in_x: f32,
    out_x: f32,
    cuts: &[(usize, f64, f32)],
    grab: f32,
) -> DragTarget {
    let din = (px - in_x).abs();
    let dout = (px - out_x).abs();
    if din <= grab && din <= dout {
        DragTarget::In
    } else if dout <= grab {
        DragTarget::Out
    } else if let Some((i, _)) = nearest_cut(px, cuts, grab) {
        DragTarget::Cut(i)
    } else {
        DragTarget::Playhead
    }
}

/// Snap `t` to the nearest marker within `snap_px` on screen, given the
/// current `view` over a track `track_w` px wide. Unsnapped `t` when no
/// marker is in radius.
pub fn snap_to_marker(t: f64, markers: &[f64], view: &View, track_w: f32, snap_px: f32) -> f64 {
    let px_per_sec = track_w as f64 / view.span.max(1e-9);
    let radius = snap_px as f64 / px_per_sec.max(1e-9);
    markers
        .iter()
        .copied()
        .map(|m| (m, (m - t).abs()))
        .filter(|(_, d)| *d <= radius)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(m, _)| m)
        .unwrap_or(t)
}

/// Pointer-anchored zoom: apply an exponential wheel `scroll_delta` to `zoom`
/// and re-derive `scroll` so the time under the pointer (at track fraction
/// `frac`) stays put. Returns `(new_zoom, new_scroll)`, zoom clamped to
/// `[1, max_zoom]`.
pub fn zoom_anchored(
    duration: f64,
    zoom: f32,
    scroll: f32,
    frac: f32,
    scroll_delta: f32,
    max_zoom: f32,
) -> (f32, f32) {
    let old = View::new(duration, zoom, scroll);
    let anchor = old.time_at(frac);
    let factor = (scroll_delta as f64 * 0.005).exp() as f32;
    let new_zoom = (zoom * factor).clamp(1.0, max_zoom);
    let span = duration / new_zoom as f64;
    let start = (anchor - frac as f64 * span).clamp(0.0, (duration - span).max(0.0));
    let max_start = (duration - span).max(1e-9);
    let new_scroll = (start / max_start).clamp(0.0, 1.0) as f32;
    (new_zoom, new_scroll)
}

/// The cut list for export: the kept spans inside `[in_p, out_p]`, or `None`
/// when nothing is actually cut out (a plain trim covers it — no re-encode
/// detour through the concat filter). `Some(vec![])` means every piece is cut.
pub fn export_spans(segments: &Segments, in_p: f64, out_p: f64) -> Option<Vec<(f64, f64)>> {
    if segments.is_trivial() {
        return None;
    }
    let spans = segments.kept_within(in_p, out_p);
    if spans.len() == 1 && spans[0].0 <= in_p + 1e-6 && spans[0].1 >= out_p - 1e-6 {
        return None; // splits exist but every piece is kept
    }
    Some(spans)
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
    fn segments_move_cut_clamps_to_neighbors() {
        let mut s = Segments::new(10.0);
        s.split_at(4.0);
        s.split_at(7.0);
        // Boundary 1 is between [0,4) and [4,7).
        assert!(approx(s.move_cut(1, 5.0).unwrap(), 5.0));
        assert!(approx(s.segments()[0].end, 5.0));
        assert!(approx(s.segments()[1].start, 5.0));
        // Clamped: cannot collapse a neighbor below the minimum piece.
        let hi = s.move_cut(1, 9.9).unwrap();
        assert!(hi <= 7.0 - 0.05 + 1e-9, "got {hi}");
        let lo = s.move_cut(1, -3.0).unwrap();
        assert!(lo >= 0.05 - 1e-9, "got {lo}");
        // Invalid indices are rejected.
        assert!(s.move_cut(0, 2.0).is_none());
        assert!(s.move_cut(9, 2.0).is_none());
    }

    #[test]
    fn segments_join_merges_and_keeps_if_either_kept() {
        let mut s = Segments::new(10.0);
        s.split_at(3.0);
        s.split_at(7.0);
        s.toggle_at(5.0); // middle cut out
                          // Join the boundary nearest 3.2 (the 3.0 cut): kept | cut -> kept.
        assert!(approx(s.join_at(3.2).unwrap(), 3.0));
        assert_eq!(s.segments().len(), 2);
        assert!(s.segments()[0].enabled);
        assert!(approx(s.segments()[0].end, 7.0));
        // Joining the last boundary restores a trivial timeline.
        s.join_at(7.0);
        assert!(s.is_trivial());
        assert!(s.join_at(5.0).is_none(), "no boundaries left");
    }

    #[test]
    fn segments_cut_range_in_one_action() {
        let mut s = Segments::new(30.0);
        assert!(s.cut_range(10.0, 20.0));
        assert_eq!(s.kept_within(0.0, 30.0), vec![(0.0, 10.0), (20.0, 30.0)]);
        // A range starting at the clip edge eats the first piece.
        let mut s = Segments::new(30.0);
        assert!(s.cut_range(0.0, 5.0));
        assert_eq!(s.kept_within(0.0, 30.0), vec![(5.0, 30.0)]);
        // Reversed and degenerate inputs are handled.
        let mut s = Segments::new(30.0);
        assert!(s.cut_range(20.0, 10.0));
        assert_eq!(s.kept_within(0.0, 30.0), vec![(0.0, 10.0), (20.0, 30.0)]);
        assert!(!s.cut_range(5.0, 5.01));
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

    #[test]
    fn export_spans_none_when_trivial_or_all_kept() {
        let s = Segments::new(10.0);
        assert_eq!(export_spans(&s, 0.0, 10.0), None);
        // Splits exist but every piece is kept -> a plain trim covers it.
        let mut s = Segments::new(10.0);
        s.split_at(4.0);
        assert_eq!(export_spans(&s, 0.0, 10.0), None);
        assert_eq!(export_spans(&s, 2.0, 8.0), None);
    }

    #[test]
    fn export_spans_reduces_cuts_to_kept_pieces() {
        let mut s = Segments::new(10.0);
        s.split_at(3.0);
        s.split_at(7.0);
        s.toggle_at(5.0);
        assert_eq!(
            export_spans(&s, 0.0, 10.0),
            Some(vec![(0.0, 3.0), (7.0, 10.0)])
        );
        // Narrower in/out intersects the kept spans.
        assert_eq!(
            export_spans(&s, 1.0, 8.0),
            Some(vec![(1.0, 3.0), (7.0, 8.0)])
        );
        // Everything cut -> Some(empty): the caller must not export.
        s.toggle_at(1.0);
        s.toggle_at(9.0);
        assert_eq!(export_spans(&s, 0.0, 10.0), Some(vec![]));
    }

    #[test]
    fn export_spans_cut_outside_window_is_a_plain_trim() {
        // The only cut piece lies entirely outside in/out, so the window is
        // one kept span covering it -> None (plain trim suffices).
        let mut s = Segments::new(10.0);
        s.split_at(8.0);
        s.toggle_at(9.0);
        assert_eq!(export_spans(&s, 0.0, 8.0), None);
    }

    #[test]
    fn snap_snaps_within_radius_only() {
        // Whole 10 s clip over a 100 px track: 10 px/s. snap_px 6 -> 0.6 s.
        let v = View::new(10.0, 1.0, 0.0);
        let markers = [5.0];
        assert!(approx(snap_to_marker(5.5, &markers, &v, 100.0, 6.0), 5.0));
        // Exactly at the radius edge still snaps (<=).
        assert!(approx(snap_to_marker(5.6, &markers, &v, 100.0, 6.0), 5.0));
        // Just beyond the radius does not.
        assert!(approx(snap_to_marker(5.61, &markers, &v, 100.0, 6.0), 5.61));
        // No markers -> unchanged.
        assert!(approx(snap_to_marker(5.5, &[], &v, 100.0, 6.0), 5.5));
    }

    #[test]
    fn snap_picks_nearest_marker_and_respects_zoom() {
        let v = View::new(10.0, 1.0, 0.0);
        let markers = [4.8, 5.3];
        assert!(approx(snap_to_marker(5.2, &markers, &v, 100.0, 6.0), 5.3));
        // Zoomed 10x: 100 px/s, radius shrinks to 0.06 s -> no snap at 0.5 s.
        let vz = View::new(10.0, 10.0, 0.0);
        assert!(approx(snap_to_marker(5.2, &markers, &vz, 100.0, 6.0), 5.2));
        assert!(approx(snap_to_marker(5.25, &markers, &vz, 100.0, 6.0), 5.3));
    }

    #[test]
    fn drag_classification_priorities() {
        const GRAB: f32 = 10.0;
        let cuts = [(1usize, 5.0f64, 50.0f32)];
        // Near the in handle.
        assert_eq!(classify_drag(12.0, 10.0, 90.0, &cuts, GRAB), DragTarget::In);
        // Near the out handle.
        assert_eq!(
            classify_drag(88.0, 10.0, 90.0, &cuts, GRAB),
            DragTarget::Out
        );
        // Overlapping handles: in wins the tie (din <= dout).
        assert_eq!(classify_drag(50.0, 50.0, 50.0, &cuts, GRAB), DragTarget::In);
        // On a cut line away from both handles.
        assert_eq!(
            classify_drag(52.0, 0.0, 100.0, &cuts, GRAB),
            DragTarget::Cut(1)
        );
        // Handle beats a cut line when both are in range.
        assert_eq!(
            classify_drag(52.0, 55.0, 100.0, &cuts, GRAB),
            DragTarget::In
        );
        // Away from every handle and cut line -> scrub.
        assert_eq!(
            classify_drag(30.0, 0.0, 100.0, &cuts, GRAB),
            DragTarget::Playhead
        );
    }

    #[test]
    fn drag_grab_radius_edges() {
        const GRAB: f32 = 10.0;
        // Exactly at the radius grabs; a hair beyond scrubs.
        assert_eq!(classify_drag(10.0, 0.0, 100.0, &[], GRAB), DragTarget::In);
        assert_eq!(
            classify_drag(10.5, 0.0, 100.0, &[], GRAB),
            DragTarget::Playhead
        );
        let cuts = [(1usize, 5.0f64, 50.0f32)];
        assert_eq!(
            classify_drag(60.0, 0.0, 100.0, &cuts, GRAB),
            DragTarget::Cut(1)
        );
        assert_eq!(
            classify_drag(60.5, 0.0, 100.0, &cuts, GRAB),
            DragTarget::Playhead
        );
    }

    #[test]
    fn nearest_cut_picks_closest() {
        let cuts = [(1usize, 3.0f64, 30.0f32), (2, 7.0, 70.0)];
        assert_eq!(nearest_cut(35.0, &cuts, 10.0), Some((1, 3.0)));
        assert_eq!(nearest_cut(65.0, &cuts, 10.0), Some((2, 7.0)));
        assert_eq!(nearest_cut(50.0, &cuts, 10.0), None);
        assert_eq!(nearest_cut(50.0, &[], 10.0), None);
    }

    #[test]
    fn zoom_anchored_keeps_pointer_time_fixed() {
        let dur = 30.0;
        let (zoom, scroll) = (2.0f32, 0.4f32);
        let frac = 0.7f32;
        let anchor = View::new(dur, zoom, scroll).time_at(frac);
        let (z2, s2) = zoom_anchored(dur, zoom, scroll, frac, 120.0, 60.0);
        assert!(z2 > zoom);
        let after = View::new(dur, z2, s2).time_at(frac);
        assert!((after - anchor).abs() < 1e-3, "{after} vs {anchor}");
        // And zooming back out re-anchors too.
        let (z3, s3) = zoom_anchored(dur, z2, s2, frac, -120.0, 60.0);
        let back = View::new(dur, z3, s3).time_at(frac);
        assert!((back - anchor).abs() < 1e-3);
    }

    #[test]
    fn zoom_anchored_clamps() {
        // Zooming out at 1x stays at 1x, scroll pinned to a valid value.
        let (z, s) = zoom_anchored(30.0, 1.0, 0.0, 0.5, -500.0, 60.0);
        assert!((z - 1.0).abs() < 1e-6);
        assert!((0.0..=1.0).contains(&s));
        // Zooming in hard clamps at max_zoom.
        let (z, _) = zoom_anchored(30.0, 50.0, 0.5, 0.5, 10_000.0, 60.0);
        assert!((z - 60.0).abs() < 1e-6);
        // At the clip edge the anchor clamp keeps scroll in range.
        let (_, s) = zoom_anchored(30.0, 4.0, 1.0, 1.0, 240.0, 60.0);
        assert!((0.0..=1.0).contains(&s));
    }
}
