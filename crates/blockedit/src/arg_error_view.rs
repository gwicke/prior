//! Two-column rendering of the `edit` tool's `Expected:` / `Actual:` argument
//! error, for the human-facing tool panel.
//!
//! # Why a table, and only for the panel
//!
//! [`crate::wire::EditArgs::invalid`] builds one message and it is used twice:
//! the *model* receives it verbatim, and the terminal panel shows it. Those two
//! readers want opposite things from layout.
//!
//! The model reads a vertical stack — `Expected:` above `Actual:` — because a
//! model parsing whitespace-padded columns is measurably worse at spotting that
//! one line differs, and the vertical form is the same shape the tool's own
//! arguments use. So the model-facing text is unchanged and stays vertical.
//!
//! The human reads a table, for the two reasons that motivate this: the
//! correct line and the actual line end up on the *same row* (a diff you can
//! take in at a glance rather than scroll between), and the block is half as
//! tall, so `Actual:` is not pushed below the fold of a terminal panel.
//!
//! When the terminal is too narrow for two honest columns, the cells are
//! truncated to fit; if even that cannot work, the caller falls back to the
//! stacked form rather than emit a mangled table.

use unicode_width::UnicodeWidthStr;

/// Gap between the two columns.
const GUTTER: &str = "  |  ";

/// Never let one column squeeze the other below this, in display cells.
const MIN_COL: usize = 12;

/// Truncate `s` to at most `max` display columns, ending with `…` when cut.
///
/// Width-aware, so a multi-byte or full-width character is never split and the
/// column arithmetic stays honest.
fn cell(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthStr::width(ch.to_string().as_str());
        // Reserve one cell for the ellipsis.
        if used + w > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Pad `s` to exactly `w` display columns (with `s` already truncated to `w`).
fn pad(s: &str, w: usize) -> String {
    let vis = UnicodeWidthStr::width(s);
    format!("{s}{}", " ".repeat(w.saturating_sub(vis)))
}

/// Wrap `s` to `width` display columns, breaking on whitespace. A word longer
/// than the width (a long marker, a URL) is hard-cut with an ellipsis, so one
/// unbreakable token cannot loop or overflow.
fn wrap_to_width(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut lines = Vec::new();
    for para in s.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if UnicodeWidthStr::width(candidate.as_str()) <= width {
                line = candidate;
            } else {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                line = cell(word, width);
            }
        }
        lines.push(line);
    }
    lines
}

/// Split a rendered argument error into `(why, expected_lines, actual_lines)`.
///
/// Returns `None` unless the message has exactly the `Expected:` / `Actual:`
/// shape this module renders, so an unrelated tool result is left alone.
pub fn split_arg_error(msg: &str) -> Option<(&str, Vec<&str>, Vec<&str>)> {
    let (why, rest) = msg.split_once("\n\nExpected:\n")?;
    let (expected, actual) = rest.split_once("\n\nActual:\n")?;
    let expected: Vec<&str> = expected.lines().collect();
    let actual: Vec<&str> = actual.lines().collect();
    if expected.is_empty() || actual.is_empty() {
        return None;
    }
    Some((why, expected, actual))
}

/// Render the message as a two-column table sized to fit `avail` display cells.
///
/// Returns `None` when the terminal is too narrow to show two columns without
/// both being cut to the point of being useless — the caller should then fall
/// back to the stacked message.
pub fn render_arg_error_table(msg: &str, avail: usize) -> Option<Vec<String>> {
    let (why, expected, actual) = split_arg_error(msg)?;

    // Natural widths, floored so a short column still reads as a column.
    let nat_e = expected
        .iter()
        .map(|l| UnicodeWidthStr::width(*l))
        .max()
        .unwrap_or(0)
        .max(MIN_COL);
    let nat_a = actual
        .iter()
        .map(|l| UnicodeWidthStr::width(*l))
        .max()
        .unwrap_or(0)
        .max(MIN_COL);

    // Give every column its natural width if the terminal allows it; otherwise
    // shave columns down to fit, always from the wider one so the columns stay
    // balanced and neither is cut to nothing.
    let mut w_e = nat_e;
    let mut w_a = nat_a;
    let need = w_e + GUTTER.chars().count() + w_a;
    if need > avail {
        let mut excess = need - avail;
        while excess > 0 {
            let room_e = w_e.saturating_sub(MIN_COL);
            let room_a = w_a.saturating_sub(MIN_COL);
            if room_e == 0 && room_a == 0 {
                // Neither column can give up any more: two honest columns do not
                // fit, so let the caller fall back to the stacked form.
                return None;
            }
            if room_a == 0 || (room_e > 0 && w_e >= w_a) {
                w_e -= 1;
            } else {
                w_a -= 1;
            }
            excess -= 1;
        }
    }

    // The problem statement spans the full width, so it wraps to the terminal
    // rather than running off the right edge.
    let mut out: Vec<String> = wrap_to_width(why, avail);
    out.push(String::new());
    // Header, so the columns are not just two anonymous stacks of text.
    out.push(format!("{}{}{}", pad("Expected", w_e), GUTTER, "Actual"));
    let rows = expected.len().max(actual.len());
    for i in 0..rows {
        let e = expected.get(i).map_or(String::new(), |s| cell(s, w_e));
        let a = actual.get(i).map_or(String::new(), |s| cell(s, w_a));
        let line = format!("{}{}{}", pad(&e, w_e), GUTTER, a);
        // Trailing spaces carry no information; the panel pads to width anyway.
        out.push(line.trim_end().to_string());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSG: &str = "Invalid block: malformed `@@@NEW` marker `@@@NEWEND`: a marker must be \
alone on its line.

Expected:
some/path:1234
@@@OLD
old text
@@@NEW
new text
@@@END

Actual:
src/x.rs
@@@OLD
… (2 lines elided) …
@@@NEW
… (2 lines elided) …
@@@NEWEND";

    #[test]
    fn splits_the_three_parts() {
        let (why, e, a) = split_arg_error(MSG).expect("split");
        assert!(why.starts_with("Invalid block:"));
        assert_eq!(e.first(), Some(&"some/path:1234"));
        assert_eq!(e.last(), Some(&"@@@END"));
        assert_eq!(a.last(), Some(&"@@@NEWEND"));
    }

    #[test]
    fn table_puts_the_bad_marker_beside_the_good_one() {
        let table = render_arg_error_table(MSG, 80).expect("table");
        // Search only the table rows (those carrying the gutter), so the
        // problem statement — which also quotes the bad marker — is not mistaken
        // for the row under test.
        let row = table
            .iter()
            .find(|l| l.contains(GUTTER) && l.contains("@@@NEWEND"))
            .expect("closing-marker row");
        // The correct closing marker and the malformed one share a row.
        assert!(row.contains("@@@END"), "{row}");
    }

    #[test]
    fn every_line_fits_the_given_width() {
        for avail in [40usize, 60, 80, 120] {
            let table = render_arg_error_table(MSG, avail).expect("table");
            for line in &table {
                assert!(
                    UnicodeWidthStr::width(line.as_str()) <= avail,
                    "width {} > {avail}: {line:?}",
                    UnicodeWidthStr::width(line.as_str())
                );
            }
        }
    }

    #[test]
    fn too_narrow_returns_none_rather_than_a_mangled_table() {
        assert!(render_arg_error_table(MSG, 10).is_none());
    }

    #[test]
    fn cell_truncation_is_width_aware() {
        assert_eq!(cell("abc", 10), "abc");
        assert_eq!(cell("abcdef", 4), "abc…");
        // A full-width char occupies two columns and must not overflow the budget.
        let wide = "日本語テキスト";
        let out = cell(wide, 5);
        assert!(UnicodeWidthStr::width(out.as_str()) <= 5, "{out:?}");
        assert!(out.ends_with('…'));
    }

    #[test]
    fn unrelated_messages_are_not_rendered() {
        assert!(split_arg_error("Replaced 1 block(s) in src/x.rs").is_none());
        assert!(render_arg_error_table("Replaced 1 block(s)", 80).is_none());
    }
}
