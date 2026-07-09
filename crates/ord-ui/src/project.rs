//! Pure editor project: timeline, cuts, markers, and history (no I/O).

use crate::edit_history::SegmentHistory;
use crate::markers::MarkerList;
use crate::timeline::{Segments, Timeline};

/// Domain state for one open clip in the trim editor.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorProject {
    pub timeline: Timeline,
    pub segments: Segments,
    pub markers: MarkerList,
    pub history: SegmentHistory,
    pub mute_export: bool,
    pub volume: f32,
    pub zoom: f32,
    pub scroll: f32,
}

impl EditorProject {
    /// Whole-clip selection, one segment, empty markers, default view.
    pub fn new(duration: f64, volume: f32) -> Self {
        Self {
            timeline: Timeline::new(duration),
            segments: Segments::new(duration),
            markers: MarkerList::new(),
            history: SegmentHistory::new(),
            mute_export: false,
            volume,
            zoom: 1.0,
            scroll: 0.0,
        }
    }

    /// Apply a cut mutation with undo: keeps a snapshot only when `f` reports
    /// a real change.
    pub fn edit_cuts(&mut self, f: impl FnOnce(&mut Segments) -> bool) {
        let snapshot = self.segments.clone();
        if f(&mut self.segments) {
            self.history.push(snapshot);
        }
    }

    pub fn undo_cut(&mut self) -> bool {
        if let Some(prev) = self.history.undo(self.segments.clone()) {
            self.segments = prev;
            true
        } else {
            false
        }
    }

    pub fn redo_cut(&mut self) -> bool {
        if let Some(next) = self.history.redo(self.segments.clone()) {
            self.segments = next;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_cuts_undo() {
        let mut p = EditorProject::new(10.0, 1.0);
        p.edit_cuts(|s| s.split_at(4.0));
        assert!(!p.segments.is_trivial());
        assert!(p.undo_cut());
        assert!(p.segments.is_trivial());
        assert!(p.redo_cut());
        assert!(!p.segments.is_trivial());
    }
}
