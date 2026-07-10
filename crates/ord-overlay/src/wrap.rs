//! Pure text wrapping for HUD toasts (no I/O).
//!
//! Callers supply a per-character advance (fontdue cache, monospace estimate,
//! …). Layout stays testable offline without a Wayland session.

/// Maximum lines drawn on a single toast card (keeps the surface bounded).
pub const MAX_TOAST_LINES: usize = 4;

/// Word-aware wrap of `text` into lines that each measure ≤ `max_width`.
///
/// - Prefer breaks on whitespace.
/// - An overlong token is hard-split by glyph width.
/// - Empty input yields a single empty line (one-row card still lays out).
/// - Output is capped at [`MAX_TOAST_LINES`]; remaining text is dropped with an
///   ellipsis on the last line when truncated.
pub fn wrap_text(text: &str, max_width: f32, mut advance: impl FnMut(char) -> f32) -> Vec<String> {
    let max_width = max_width.max(1.0);
    let text = text.trim();
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0.0_f32;

    let flush = |lines: &mut Vec<String>, cur: &mut String, cur_w: &mut f32| {
        if !cur.is_empty() {
            lines.push(std::mem::take(cur));
            *cur_w = 0.0;
        }
    };

    for word in text.split_whitespace() {
        let mut word_w: f32 = word.chars().map(&mut advance).sum();
        let space_w = if cur.is_empty() { 0.0 } else { advance(' ') };

        if !cur.is_empty() && cur_w + space_w + word_w <= max_width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += space_w + word_w;
            continue;
        }

        if cur.is_empty() && word_w <= max_width {
            cur.push_str(word);
            cur_w = word_w;
            continue;
        }

        // Word doesn't fit on the current line.
        flush(&mut lines, &mut cur, &mut cur_w);
        if lines.len() >= MAX_TOAST_LINES {
            break;
        }

        if word_w <= max_width {
            cur.push_str(word);
            cur_w = word_w;
            continue;
        }

        // Hard-break an overlong token.
        for ch in word.chars() {
            if lines.len() >= MAX_TOAST_LINES {
                break;
            }
            let w = advance(ch);
            if !cur.is_empty() && cur_w + w > max_width {
                flush(&mut lines, &mut cur, &mut cur_w);
                if lines.len() >= MAX_TOAST_LINES {
                    break;
                }
            }
            cur.push(ch);
            cur_w += w;
            word_w = 0.0; // silence unused after first use path
        }
    }
    if lines.len() < MAX_TOAST_LINES {
        flush(&mut lines, &mut cur, &mut cur_w);
    } else if !cur.is_empty() {
        // Truncated mid-token: append ellipsis to last line if present.
        if let Some(last) = lines.last_mut() {
            if !last.ends_with('…') {
                last.push('…');
            }
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    // If we filled MAX lines but still had more words, mark ellipsis.
    let consumed: usize = lines
        .iter()
        .map(|l| l.chars().filter(|c| !c.is_whitespace()).count())
        .sum();
    let total_non_ws = text.chars().filter(|c| !c.is_whitespace()).count();
    if lines.len() >= MAX_TOAST_LINES && consumed < total_non_ws {
        if let Some(last) = lines.last_mut() {
            if !last.ends_with('…') {
                last.push('…');
            }
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(c: char) -> f32 {
        if c == ' ' {
            4.0
        } else {
            8.0
        }
    }

    #[test]
    fn empty_is_one_blank_line() {
        assert_eq!(wrap_text("", 100.0, mono), vec![""]);
        assert_eq!(wrap_text("   ", 100.0, mono), vec![""]);
    }

    #[test]
    fn short_text_stays_one_line() {
        let lines = wrap_text("Clip saved (30s)", 400.0, mono);
        assert_eq!(lines, vec!["Clip saved (30s)"]);
    }

    #[test]
    fn wraps_on_word_boundaries() {
        // "hello" = 40, space = 4, "world" = 40 → 84; max 50 forces two lines.
        let lines = wrap_text("hello world", 50.0, mono);
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn hard_breaks_overlong_token() {
        // 10 chars × 8 = 80; max 24 → 3 glyphs per line.
        let lines = wrap_text("abcdefghij", 24.0, mono);
        assert!(lines.len() >= 3);
        assert_eq!(lines.iter().map(|l| l.len()).sum::<usize>(), 10);
        assert!(lines
            .iter()
            .all(|l| l.chars().map(mono).sum::<f32>() <= 24.0 + 0.01));
    }

    #[test]
    fn caps_at_max_lines_with_ellipsis() {
        let text = "one two three four five six seven eight nine ten";
        let lines = wrap_text(text, 40.0, mono); // ~5 chars/line
        assert!(lines.len() <= MAX_TOAST_LINES);
        assert!(
            lines.last().is_some_and(|l| l.ends_with('…')),
            "expected ellipsis on last line, got {lines:?}"
        );
    }
}
