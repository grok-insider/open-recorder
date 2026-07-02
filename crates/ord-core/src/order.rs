//! Shared timestamp-ordered insertion for the replay buffers.
//!
//! All three time-windowed buffers (video ring, disk index, audio ring) keep a
//! `VecDeque` ordered by timestamp with an append fast path. The correction
//! path scans from the **back**: B-frame reordering displaces a frame by only
//! a couple of positions from the tail, while a front-first scan walks the
//! whole window (thousands of comparisons at 60 s @ 60 fps) for every
//! reordered frame. One implementation, so the placement semantics (equal
//! timestamps keep arrival order) can never drift between the buffers.

use std::collections::VecDeque;

/// Insert `item` keeping `q` ordered by `ts`. Appends when in order; otherwise
/// back-scans to the first position whose predecessor is `<=` the new
/// timestamp (equal-ts items land after existing ones, preserving arrival
/// order).
pub(crate) fn insert_ts_ordered<T>(q: &mut VecDeque<T>, item: T, ts: impl Fn(&T) -> i64) {
    let t = ts(&item);
    if q.back().map(|b| t >= ts(b)).unwrap_or(true) {
        q.push_back(item);
        return;
    }
    let mut idx = q.len();
    while idx > 0 && ts(&q[idx - 1]) > t {
        idx -= 1;
    }
    q.insert(idx, item);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(q: &mut VecDeque<(i64, u8)>, ts: i64, tag: u8) {
        insert_ts_ordered(q, (ts, tag), |e| e.0);
    }

    #[test]
    fn appends_in_order() {
        let mut q = VecDeque::new();
        insert(&mut q, 1, 0);
        insert(&mut q, 2, 0);
        insert(&mut q, 3, 0);
        assert_eq!(q.iter().map(|e| e.0).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[test]
    fn corrects_reorder_near_the_back() {
        let mut q = VecDeque::new();
        for t in [10, 20, 30, 50] {
            insert(&mut q, t, 0);
        }
        insert(&mut q, 40, 0); // the B-frame case
        assert_eq!(
            q.iter().map(|e| e.0).collect::<Vec<_>>(),
            vec![10, 20, 30, 40, 50]
        );
    }

    #[test]
    fn equal_timestamps_keep_arrival_order() {
        let mut q = VecDeque::new();
        insert(&mut q, 10, 1);
        insert(&mut q, 30, 2);
        insert(&mut q, 10, 3); // equal to an existing ts, arrives later
        assert_eq!(q.iter().map(|e| e.1).collect::<Vec<_>>(), vec![1, 3, 2]);
    }

    #[test]
    fn inserts_at_the_front_when_oldest() {
        let mut q = VecDeque::new();
        insert(&mut q, 20, 0);
        insert(&mut q, 30, 0);
        insert(&mut q, 5, 0);
        assert_eq!(q.iter().map(|e| e.0).collect::<Vec<_>>(), vec![5, 20, 30]);
    }
}
