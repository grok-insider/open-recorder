//! Undo/redo stacks for pure editor edits (no I/O).

use crate::timeline::Segments;

const DEFAULT_MAX: usize = 64;

/// Undo (and redo) history for multi-segment cut edits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SegmentHistory {
    undo: Vec<Segments>,
    redo: Vec<Segments>,
    max: usize,
}

impl SegmentHistory {
    pub fn new() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            max: DEFAULT_MAX,
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Record `previous` before applying a mutation. Clears the redo stack
    /// (a new edit branch).
    pub fn push(&mut self, previous: Segments) {
        self.undo.push(previous);
        if self.undo.len() > self.max {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Pop undo → restore that state; push `current` onto redo.
    pub fn undo(&mut self, current: Segments) -> Option<Segments> {
        let prev = self.undo.pop()?;
        self.redo.push(current);
        if self.redo.len() > self.max {
            self.redo.remove(0);
        }
        Some(prev)
    }

    /// Pop redo → restore that state; push `current` onto undo.
    pub fn redo(&mut self, current: Segments) -> Option<Segments> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        if self.undo.len() > self.max {
            self.undo.remove(0);
        }
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::Segments;

    #[test]
    fn undo_redo_round_trip() {
        let mut h = SegmentHistory::new();
        let a = Segments::new(10.0);
        let mut b = a.clone();
        assert!(b.split_at(5.0));
        h.push(a.clone());
        let restored = h.undo(b.clone()).unwrap();
        assert_eq!(restored, a);
        assert!(h.can_redo());
        let redone = h.redo(a).unwrap();
        assert_eq!(redone, b);
    }

    #[test]
    fn new_edit_clears_redo() {
        let mut h = SegmentHistory::new();
        let a = Segments::new(10.0);
        let mut b = a.clone();
        b.split_at(3.0);
        h.push(a.clone());
        let _ = h.undo(b.clone());
        assert!(h.can_redo());
        h.push(a);
        assert!(!h.can_redo());
    }
}
