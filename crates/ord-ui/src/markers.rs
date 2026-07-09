//! Pure marker list for the clip editor (no I/O, no egui).

/// Times (seconds) on the clip timeline — chapters from `ord mark` and
/// user-placed markers. Always kept sorted and de-duplicated.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarkerList {
    markers: Vec<f64>,
}

impl MarkerList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_slice(&self) -> &[f64] {
        &self.markers
    }

    pub fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }

    /// Insert `t` if no existing marker is within 1 ms. Returns whether added.
    pub fn add(&mut self, t: f64) -> bool {
        if !t.is_finite() {
            return false;
        }
        if self.markers.iter().any(|m| (m - t).abs() < 1e-3) {
            return false;
        }
        self.markers.push(t);
        self.markers
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        true
    }

    /// Merge many times (e.g. chapters), de-duplicating against existing.
    pub fn extend(&mut self, times: impl IntoIterator<Item = f64>) {
        for t in times {
            self.add(t);
        }
    }

    /// Nearest marker strictly before `t`.
    pub fn prev(&self, t: f64) -> Option<f64> {
        self.markers
            .iter()
            .copied()
            .filter(|m| *m < t - 1e-3)
            .fold(None, |acc: Option<f64>, m| {
                Some(acc.map_or(m, |a| a.max(m)))
            })
    }

    /// Nearest marker strictly after `t`.
    pub fn next(&self, t: f64) -> Option<f64> {
        self.markers
            .iter()
            .copied()
            .filter(|m| *m > t + 1e-3)
            .fold(None, |acc: Option<f64>, m| {
                Some(acc.map_or(m, |a| a.min(m)))
            })
    }

    /// Remove the marker closest to `t` if within `max_dist` seconds.
    pub fn remove_nearest(&mut self, t: f64, max_dist: f64) -> bool {
        let nearest = self
            .markers
            .iter()
            .enumerate()
            .map(|(i, m)| (i, (m - t).abs()))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((i, d)) = nearest {
            if d <= max_dist {
                self.markers.remove(i);
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_dedupes_and_sorts() {
        let mut m = MarkerList::new();
        assert!(m.add(2.0));
        assert!(m.add(1.0));
        assert!(!m.add(1.0005)); // within 1 ms
        assert_eq!(m.as_slice(), &[1.0, 2.0]);
    }

    #[test]
    fn prev_next_and_remove() {
        let mut m = MarkerList::new();
        m.extend([1.0, 3.0, 5.0]);
        assert_eq!(m.prev(3.0), Some(1.0));
        assert_eq!(m.next(3.0), Some(5.0));
        assert!(m.remove_nearest(3.1, 1.0));
        assert_eq!(m.as_slice(), &[1.0, 5.0]);
        assert!(!m.remove_nearest(3.0, 0.1));
    }
}
