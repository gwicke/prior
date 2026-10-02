//! Two-column rendering of the `edit` tool's `Expected:` / `Actual:` argument
//! error, for the human-facing tool panel.
//!
//! # Why a template, and why it bypasses the panel's wrapping
//!
//! [`crate::wire::EditArgs::invalid`] builds one message and it is used twice:
//! the *model* receives it verbatim, and the terminal panel shows it. Those two
//! readers want opposite things.
//!
//! The model reads a vertical stack — `Expected:` above `Actual:` — because a
//! model parsing whitespace-padded columns is measurably worse at spotting which
//! line differs, and the vertical form is the same shape the tool's own
//! arguments use. The model-facing text is therefore unchanged, and this module
//! only ever runs on the display path.
//!
//! For the human, side by side is the point: the correct line and the line the
//! model actually sent land on the same row, and the block is half as tall, so
//! `Actual:` is not pushed below the fold.
//!
//! # The alignment problem, and why the template solves it
//!
//! The obvious implementation — pad each cell to the column's natural width —
//! produces perfect output from this module and *broken* output on screen,
//! because the panel pipeline re-wraps every line through [`crate`]'s host
//! `wrap()`, which joins words with `split_whitespace` and therefore **collapses
//! interior runs of spaces**. `@@@OLD          |` comes back as `@@@OLD |`, and
//! the pipes land wherever each left cell happens to end. No amount of care in
//! the padding survives that.
//!
//! The fix is to not need padding at all. The `Expected` side is a *fixed
//! template* — the same six lines every time — so its width is a constant
//! rather than something derived from variable content. Only the `Actual` side
//! varies, and it is a trailing cell, so it needs no trailing pad. The pipe
//! therefore sits at a compile-time-known column, and the emitted lines are
//! already ≤ the terminal width, so the panel renders them as-is (via
//! `paint_panel`, not the wrapping `tool_panel`).
//!
//! Truncation is width-aware, and when the terminal cannot fit the template plus
//! a usable `Actual` column the renderer returns `None` so the caller falls back
//! to the stacked message rather than emit a mangled table.
//!
//! # Which messages get a table at all
//!
//! The table pairs *block markers* against the lines the model sent, so it only
//! applies when `Expected:` is the block example. A single-edit failure carries
//! [`crate::wire`]'s one-line JSON example instead, and there the stacked form
//! is better anyway: `Expected:` / `Actual:` are then two JSON values, a
//! definition list, where the table would set five `@@@OLD`-style rows against
//! one row of JSON. `render_arg_error_table` returns `None` for those, so one
//! decision covers both the too-narrow and the wrong-shape case.

use unicode_width::UnicodeWidthStr;

/// The shape the model should have written, shown verbatim beside what it did.
///
/// A constant, and the reason the layout below can be exact: these six lines
/// are the same width on every invocation, so the gutter's column is known.
const TEMPLATE: &[&str] = &[
    "some/path:1234",
    "@@@OLD",
    "old text",
    "@@@NEW",
    "new text",
    "@@@END",
];

/// Separator drawn between the two columns.
const GUTTER: &str = " | ";

/// Narrowest `Actual` column still worth showing. Below this the table stops
/// being readable and the stacked form is better.
const MIN_RIGHT: usize = 16;

/// Display width of the template's widest line.
fn template_width() -> usize {
    TEMPLATE
        .iter()
        .map(|l| UnicodeWidthStr::width(*l))
        .max()
        .unwrap_or(0)
}

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

/// Pad `s` to exactly `w` display columns. Only ever called on template lines,
/// which are ASCII and short enough to fit the constant width.
fn pad(s: &str, w: usize) -> String {
    let vis = UnicodeWidthStr::width(s);
    format!("{s}{}", " ".repeat(w.saturating_sub(vis)))
}

/// Wrap `s` to `width` display columns, breaking on whitespace. A word longer
/// than the width is hard-cut, so one unbreakable token cannot overflow.
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

/// Split a rendered argument error into its problem statement, the `Expected:`
/// section, and the `Actual:` lines.
///
/// The `Expected:` side is returned but deliberately *not* rendered from: the
/// table regenerates it from [`TEMPLATE`] so the column width cannot drift with
/// the content. Returns `None` unless the message has the `Expected:` / `Actual:`
/// shape this module renders, so an unrelated tool result is left alone.
pub fn split_arg_error(msg: &str) -> Option<(&str, &str, Vec<&str>)> {
    let (why, rest) = msg.split_once("\n\nExpected:\n")?;
    let (expected, actual) = rest.split_once("\n\nActual:\n")?;
    let actual: Vec<&str> = actual.lines().collect();
    if actual.is_empty() {
        return None;
    }
    Some((why, expected, actual))
}

/// Render the message as a two-column table sized to fit `avail` display cells.
///
/// Every returned line is at most `avail` cells wide and must be emitted
/// *without* re-wrapping (see the module docs on `wrap` collapsing padding).
///
/// Returns `None` — leaving the caller the stacked message — in both of the
/// cases where this table would be wrong rather than merely wide:
///
/// - the `Expected:` section is not the block template, so this is a
///   single-edit failure carrying [`crate::wire`]'s one-line JSON example.
///   Putting that beside block markers would be a row of `@@@OLD` against a row
///   of JSON; the stacked form is a definition list instead, which is what that
///   message is.
/// - the terminal is too narrow for the template plus a usable `Actual` column.
pub fn render_arg_error_table(msg: &str, avail: usize) -> Option<Vec<String>> {
    let (why, expected, actual) = split_arg_error(msg)?;
    if expected.trim_end() != TEMPLATE.join("\n") {
        return None;
    }

    let w = template_width();
    let right = avail.checked_sub(w + GUTTER.chars().count())?;
    if right < MIN_RIGHT {
        return None;
    }

    // The problem statement spans the full width and wraps; it carries no
    // alignment, so the wrapper may freely collapse its spaces.
    let mut out = wrap_to_width(why, avail);
    out.push(String::new());

    // Header row, so the columns are not two anonymous stacks of text.
    let head = format!("{}{}{}", pad("Expected", w), GUTTER, "Actual");
    out.push(head.trim_end().to_string());

    let rows = TEMPLATE.len().max(actual.len());
    for i in 0..rows {
        let left = pad(TEMPLATE.get(i).copied().unwrap_or(""), w);
        let right_cell = actual.get(i).map(|s| cell(s, right)).unwrap_or_default();
        let line = format!("{left}{GUTTER}{right_cell}");
        out.push(line.trim_end().to_string());
    }
    Some(out)
}

/// The display column (in cells, excluding the panel indent) at which the
/// gutter's `|` is drawn.
///
/// Constant by construction — the template never changes width, which is the
/// whole point of rendering from a template. Derived from the gutter itself so
/// it stays correct if the gutter's spacing is ever tweaked.
pub fn gutter_column() -> usize {
    template_width() + GUTTER.find('|').expect("gutter has no pipe")
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
    fn every_pipe_lands_in_the_same_column() {
        // The whole point. Padding-based tables lose this the moment the host
        // `wrap()` collapses interior spaces, so it is asserted directly.
        let table = render_arg_error_table(MSG, 80).expect("table");
        let columns: Vec<usize> = table
            .iter()
            .filter(|l| l.contains('|'))
            .map(|l| l.find('|').unwrap())
            .collect();
        assert!(columns.len() >= 6, "{columns:?}");
        assert!(
            columns.iter().all(|c| *c == gutter_column()),
            "pipes at {columns:?}, expected all at {}",
            gutter_column()
        );
    }

    #[test]
    fn the_good_and_bad_markers_share_a_row() {
        let table = render_arg_error_table(MSG, 80).expect("table");
        let row = table
            .iter()
            .find(|l| l.contains('|') && l.contains("@@@NEWEND"))
            .expect("closing-marker row");
        assert!(row.contains("@@@END"), "{row}");
    }

    #[test]
    fn every_line_fits_the_given_width() {
        for avail in [34usize, 40, 60, 80, 120, 200] {
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
        // One below the floor (template + gutter + MIN_RIGHT).
        assert!(render_arg_error_table(MSG, 32).is_none());
        // Exactly at the floor it renders.
        assert!(render_arg_error_table(MSG, 33).is_some());
    }

    #[test]
    fn a_long_actual_value_is_truncated_to_its_column() {
        let long = format!(
            "Invalid\n\nExpected:\n{}\n\nActual:\nsrc/very/long/path/{}.rs",
            TEMPLATE.join("\n"),
            "a".repeat(300)
        );
        let table = render_arg_error_table(&long, 60).expect("table");
        for line in &table {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 60, "{line:?}");
        }
        // And the pipe is still aligned on the truncated row.
        let columns: Vec<usize> = table
            .iter()
            .filter(|l| l.contains('|'))
            .map(|l| l.find('|').unwrap())
            .collect();
        assert!(columns.iter().all(|c| *c == gutter_column()), "{columns:?}");
    }

    /// A single-edit failure carries the JSON example, and gets no table: the
    /// stacked `Expected:` / `Actual:` is two JSON values side by side in
    /// reading order, where a table would be five `@@@OLD` rows against one row
    /// of JSON.
    #[test]
    fn a_json_shaped_message_gets_no_table() {
        let json_msg = "missing required field 'path'\n\nExpected:\n\
            {\"path\": \"some/path.rs\", \"startLine\": 1234, \"oldText\": \"old text\", \
             \"newText\": \"new text\"}\n\nActual:\n{\"newText\": \"b\", \"oldText\": \"a\"}";
        assert!(render_arg_error_table(json_msg, 80).is_none());
        // It is still an argument error — it splits, it just has no table.
        let (why, expected, actual) = split_arg_error(json_msg).expect("splits");
        assert_eq!(why, "missing required field 'path'");
        assert!(expected.starts_with("{\"path\""), "{expected}");
        assert_eq!(actual, vec!["{\"newText\": \"b\", \"oldText\": \"a\"}"]);
    }

    /// The gate is the *content* of `Expected:`, not its presence: any message
    /// whose example is not the block template is left stacked, so a future
    /// third shape inherits the behavior instead of getting a mismatched table.
    #[test]
    fn an_unrecognized_expected_section_gets_no_table() {
        let other = "why\n\nExpected:\nsomething else entirely\n\nActual:\na";
        assert!(render_arg_error_table(other, 80).is_none());
    }

    #[test]
    fn cell_truncation_is_width_aware() {
        assert_eq!(cell("abc", 10), "abc");
        assert_eq!(cell("abcdef", 4), "abc…");
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
