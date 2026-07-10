//! Pure layout math for adaptive UI on large displays (no I/O).

/// Settings form column width from available content width.
///
/// Keeps a readable measure on small windows and grows toward ~960 px on
/// ultrawide so Settings is not a thin strip in a sea of empty chrome.
pub fn form_column_width(available: f32) -> f32 {
    const MIN: f32 = 520.0;
    const MAX: f32 = 960.0;
    // Prefer ~70% of width when there is room, but never exceed MAX.
    let target = (available * 0.70).clamp(MIN, MAX);
    target.min(available.max(0.0))
}

/// Library grid: `(columns, card_inner_width)` that fills `available` width.
///
/// Picks as many columns as fit near a ~320 px target card, then grows each
/// card to fill the row (clamped 260–400 px) so large displays get bigger
/// thumbs instead of a thin strip of fixed 300 px cards with empty gutters.
pub fn library_grid(available: f32, spacing: f32, frame_pad: f32) -> (usize, f32) {
    const MIN_INNER: f32 = 260.0;
    const MAX_INNER: f32 = 400.0;
    const TARGET_INNER: f32 = 320.0;
    let avail = available.max(0.0);
    let target_outer = TARGET_INNER + frame_pad;
    let cols = (((avail + spacing) / (target_outer + spacing)).floor() as usize).max(1);
    let gaps = spacing * cols.saturating_sub(1) as f32;
    let outer = if cols == 1 {
        avail
    } else {
        ((avail - gaps) / cols as f32).max(0.0)
    };
    let inner = (outer - frame_pad).clamp(MIN_INNER, MAX_INNER);
    // Never wider than the panel (narrow windows).
    (cols, inner.min(avail.max(MIN_INNER)))
}

/// Library card inner width from available panel width (single-card helper).
pub fn library_card_width(available: f32) -> f32 {
    const SPACING: f32 = 12.0;
    const FRAME_PAD: f32 = 2.0 * 12.0 + 8.0;
    library_grid(available, SPACING, FRAME_PAD).1
}

/// 16:9 thumbnail height for a given card/thumb width.
pub fn thumb_height(thumb_w: f32) -> f32 {
    (thumb_w * 9.0 / 16.0).round()
}

/// Whether a scrub motion should emit a decode seek (throttle intermediate
/// seeks so keyframe run-up can finish and the preview keeps updating).
///
/// - `force`: always seek (drag end, click).
/// - Else: seek when enough time elapsed **or** the playhead moved ≥ `min_dt`.
pub fn should_scrub_seek(
    last: Option<(f64, std::time::Instant)>,
    now: std::time::Instant,
    t: f64,
    force: bool,
    min_interval: std::time::Duration,
    min_dt: f64,
) -> bool {
    if force {
        return true;
    }
    let Some((prev_t, prev_at)) = last else {
        return true;
    };
    if now.duration_since(prev_at) >= min_interval {
        return true;
    }
    (t - prev_t).abs() >= min_dt
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn form_column_grows_then_caps() {
        assert!((form_column_width(500.0) - 500.0).abs() < 0.01);
        assert!(
            (form_column_width(700.0) - 520.0).abs() < 1.0 || form_column_width(700.0) >= 520.0
        );
        assert!(form_column_width(800.0) >= 520.0);
        assert!(form_column_width(2000.0) <= 960.0 + 0.01);
        assert!((form_column_width(2000.0) - 960.0).abs() < 0.01);
    }

    #[test]
    fn library_grid_fills_and_clamps() {
        let (c1, w1) = library_grid(400.0, 12.0, 32.0);
        assert_eq!(c1, 1);
        assert!((260.0..=400.0).contains(&w1));

        let (cols, inner) = library_grid(1400.0, 12.0, 32.0);
        assert!(cols >= 3);
        assert!((260.0..=400.0).contains(&inner));

        let (wide_cols, wide_inner) = library_grid(2560.0, 12.0, 32.0);
        assert!(wide_cols >= 5);
        assert!((260.0..=400.0).contains(&wide_inner));
        // Larger panels should not keep the old fixed 300 px cards.
        assert!(wide_inner >= 300.0 || wide_cols >= 6);

        assert!((thumb_height(320.0) - 180.0).abs() < 0.01);
        assert!((library_card_width(2560.0) - library_grid(2560.0, 12.0, 32.0).1).abs() < 0.01);
    }

    #[test]
    fn scrub_seek_throttle() {
        let t0 = Instant::now();
        assert!(should_scrub_seek(
            None,
            t0,
            1.0,
            false,
            Duration::from_millis(50),
            0.04
        ));
        assert!(should_scrub_seek(
            Some((1.0, t0)),
            t0,
            1.0,
            true,
            Duration::from_millis(50),
            0.04
        ));
        // Same time, tiny motion: hold.
        assert!(!should_scrub_seek(
            Some((1.0, t0)),
            t0,
            1.01,
            false,
            Duration::from_millis(50),
            0.04
        ));
        // Large motion: seek even if interval not elapsed.
        assert!(should_scrub_seek(
            Some((1.0, t0)),
            t0,
            1.1,
            false,
            Duration::from_millis(50),
            0.04
        ));
    }
}
