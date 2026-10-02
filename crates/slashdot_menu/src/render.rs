//! Render plan: plain-data description of the overlay panel.
//!
//! The crate emits a [`RenderPlan`]; the host renders it directly.
//! Row styling (markers, backgrounds) is applied by the host at render
//! time — the plan contains only data, not ANSI escapes.

use std::sync::Arc;

/// A single logical row. May contain embedded `\n` from table wrapping.
/// The overlay splits on `\n` at render time and applies markers only to
/// the first sub-line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalLine(pub String);

impl LogicalLine {
    pub fn new(s: impl Into<String>) -> Self {
        LogicalLine(s.into())
    }
}

/// The complete overlay panel description.
///
/// v2.5 panel structure (design §6):
/// 1. `detail` — `header()` output (truncatable from bottom on tight space)
/// 2. `table_header` — single-line column headings (always shown, never truncated)
/// 3. `rows` — scrollable data (Arc-shared; same pointer on cursor moves)
#[derive(Debug, Clone)]
pub struct RenderPlan {
    /// Table column header (e.g. "Model   Ctx   Coding   Price in").
    /// Single line guaranteed. Changes only on terminal resize.
    pub table_header: Option<LogicalLine>,
    /// `header()` output: help text, stats, focused-row details.
    /// Changes on cursor move (detail updates). Truncatable vertically.
    pub detail: Vec<LogicalLine>,
    /// Scrollable data rows. `Arc`-shared — same pointer on cursor moves.
    ///
    /// Row text is laid out for the *content* width: `term_w - 2`, the
    /// terminal width minus the marker column the painter adds (see
    /// [`paint_row`]). A producer that laid a row out at the full width would
    /// have its last columns split onto a second terminal row. The engine's
    /// own tables are laid out that way (`render_multiline(tw - 2)`).
    ///
    /// `Arc`, not `Rc`: a host holds these rows in a frame element, and
    /// elements are stashed in the draw cache behind a `Mutex` under an
    /// `Output: Send + Sync`, so an `Rc` here would have to be smuggled across
    /// a `Send` boundary. Sharing is still one pointer either way — only the
    /// refcount is atomic, and it is touched once per painted frame.
    pub rows: Arc<Vec<LogicalLine>>,
    /// Index of the cursor-selected logical row among `rows`.
    pub selected: Option<usize>,
    /// Indices of active (`*`-marked) rows (design §5.2).
    pub active_rows: Vec<usize>,
}

impl Default for RenderPlan {
    fn default() -> Self {
        RenderPlan {
            table_header: None,
            detail: Vec::new(),
            rows: Arc::new(Vec::new()),
            selected: None,
            active_rows: Vec::new(),
        }
    }
}

/// Marker for a logical option (v1 contract: `>` cursor, `*` active-state;
/// `*` wins when both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    Cursor,
    Active,
    CursorActive,
    None,
}

impl Marker {
    pub fn as_str(&self) -> &'static str {
        match self {
            Marker::Cursor | Marker::None => {
                if matches!(self, Marker::Cursor) {
                    "> "
                } else {
                    "  "
                }
            }
            Marker::Active | Marker::CursorActive => "* ",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_cursor_active() {
        assert_eq!(Marker::Cursor.as_str(), "> ");
        assert_eq!(Marker::Active.as_str(), "* ");
        assert_eq!(Marker::CursorActive.as_str(), "* ");
        assert_eq!(Marker::None.as_str(), "  ");
    }

    /// `rows` data rows, each wrapping to `lines` visual rows.
    #[allow(dead_code)]
    fn wrapped_rows(rows: usize, lines: usize) -> Vec<LogicalLine> {
        (0..rows)
            .map(|i| {
                let mut sub: Vec<String> = Vec::with_capacity(lines);
                sub.push(format!("r{i}"));
                for j in 1..lines {
                    sub.push(format!("w{i}-{j}"));
                }
                LogicalLine::new(sub.join("\n"))
            })
            .collect()
    }

    /// §9 #11: a line wider than the terminal is *split*, so a section's row
    /// count is the number of terminal rows it occupies and the frame's
    /// accounting can trust it. Before, the line was emitted over-long (1
    /// painted row, 2 terminal rows).
    #[test]
    fn an_over_long_line_is_split_into_terminal_rows() {
        let s = paint_detail(&[LogicalLine::new("d".repeat(100))], 60);
        // 100 columns at a 58-column content width: two terminal rows.
        assert_eq!(s.heights, vec![2]);
        assert_eq!(s.height(), 2);
    }

    /// Every painted row is exactly the terminal width and holds no newline —
    /// the shape `ui::frame::Element::Overlay` requires and asserts. Checked
    /// across all three sections at three widths.
    #[test]
    fn every_painted_row_is_one_full_width_terminal_row() {
        let detail = vec![
            LogicalLine::new("short"),
            LogicalLine::new("a detail line that is quite a lot longer than the width"),
            LogicalLine::new(""),
        ];
        let rows = vec![
            LogicalLine::new("laid out to the content width".to_string() + &" ".repeat(30)),
            LogicalLine::new("\u{65e5}\u{672c}".repeat(40)),
        ];
        for term_w in [20usize, 60, 80] {
            let sections = [
                paint_detail(&detail, term_w),
                paint_header(Some(&LogicalLine::new("col a     col b")), term_w),
                paint_rows(&rows, Some(0), &[], term_w),
            ];
            for section in &sections {
                for row in &section.rows {
                    assert!(!row.contains('\n'), "row must be one screen row");
                    assert_eq!(
                        display_width(row),
                        term_w,
                        "painted row is not exactly the terminal width"
                    );
                }
            }
        }
    }

    /// An active row shows `*` where a merely selected row shows `>`. A host
    /// that sniffed the cursor marker out of the painted bytes used to find
    /// nothing and fall back to row 0 — which is why the selection is carried
    /// as an index now rather than sniffed.
    #[test]
    fn the_active_marker_replaces_the_cursor_marker() {
        let rows: Vec<LogicalLine> = (0..5)
            .map(|i| LogicalLine::new(format!("row {i}")))
            .collect();
        let s = paint_rows(&rows, Some(2), &[2], 60);
        assert!(s.rows[2].contains("* row 2"), "{:?}", s.rows[2]);
        assert!(!s.rows[2].contains('>'), "{:?}", s.rows[2]);
        // Its neighbours keep their own markers: selected-but-not-active is `>`,
        // and neither is two spaces.
        let s = paint_rows(&rows, Some(2), &[0], 60);
        assert!(s.rows[2].contains("> row 2"), "{:?}", s.rows[2]);
        assert!(s.rows[0].contains("* row 0"), "{:?}", s.rows[0]);
        assert!(s.rows[3].contains("  row 3"), "{:?}", s.rows[3]);
    }

    /// A section's `heights` are the *painted* heights, in row order: embedded
    /// newlines are rows, and a row too wide to fit was split. This is the
    /// invariant every window, budget and page size is computed in.
    #[test]
    fn section_heights_are_the_painted_heights() {
        let rows = vec![
            LogicalLine::new("one"),
            LogicalLine::new("two\nwrapped\nlines"),
            LogicalLine::new("a row far too wide to fit the content width of this terminal"),
        ];
        let s = paint_rows(&rows, None, &[], 40);
        assert_eq!(s.heights[0], 1);
        assert_eq!(s.heights[1], 3, "embedded newlines are rows");
        assert!(s.heights[2] > 1, "the over-long row was split");
        assert_eq!(s.height(), s.rows.len());
    }

    /// Nothing to paint is an empty section, not a row of blanks — which is
    /// what lets a host drop the element instead of asking for space.
    #[test]
    fn empty_sections_paint_nothing() {
        assert_eq!(paint_detail(&[], 60), PaintedSection::default());
        assert_eq!(paint_header(None, 60), PaintedSection::default());
        assert_eq!(paint_rows(&[], None, &[], 60), PaintedSection::default());
    }

    // --- the three section painters ---------------------------------------
    //
    // Each section paints on its own, from a width and nothing else — no
    // budget, no `term_h`, no knowledge of what will be shown. That is what
    // lets a host compose the sections as separate frame elements and let the
    // frame do the budgeting once.

    /// A section's rows are already split, and `heights` is parallel to the
    /// source lines: `sum(heights) == rows.len()`, and `offset_at` is the
    /// running sum. The invariant every windowing caller leans on.
    #[test]
    fn section_heights_are_parallel_to_the_source_lines() {
        let lines = vec![
            LogicalLine::new("one"),
            LogicalLine::new("two\nwrapped\nlines"),
            LogicalLine::new("three"),
        ];
        let s = paint_detail(&lines, 40);
        assert_eq!(s.heights, vec![1, 3, 1]);
        assert_eq!(s.height(), s.rows.len());
        assert_eq!(s.height(), s.heights.iter().sum::<usize>());
        assert_eq!(s.offset_at(0), 0);
        assert_eq!(s.offset_at(1), 1, "row 1 starts after row 0");
        assert_eq!(s.offset_at(2), 4, "row 2 starts after the 3-line row");
        assert_eq!(s.offset_at(99), 5, "past the end clamps to the total");
    }

    /// A multi-line logical row stays *whole*: `slice` hands back exactly that
    /// row's painted rows, so a window can never cut one in half.
    #[test]
    fn section_slice_returns_whole_logical_rows() {
        let lines = vec![
            LogicalLine::new("a"),
            LogicalLine::new("b1\nb2\nb3"),
            LogicalLine::new("c"),
            LogicalLine::new("d"),
        ];
        let s = paint_detail(&lines, 40);
        assert_eq!(s.heights, vec![1, 3, 1, 1]);
        assert_eq!(s.slice(1, 1).len(), 3, "the wrapped row is all 3 rows");
        assert!(s.slice(1, 1)[0].contains("b1"), "{:?}", s.slice(1, 1)[0]);
        assert!(s.slice(1, 1)[2].contains("b3"), "{:?}", s.slice(1, 1)[2]);
        assert_eq!(s.slice(2, 2).len(), 2, "two one-line rows");
        assert_eq!(s.slice(0, 99).len(), 6, "an over-long count clamps");
        assert!(s.slice(99, 1).is_empty(), "an empty slice past the end");
    }

    /// The table header is one logical row in and one terminal row out — the
    /// property that lets a host make it a frame element whose whole height is
    /// known up front, and so budget it as one row. `None` paints nothing, so a
    /// plan without columns contributes no rows.
    #[test]
    fn header_paints_one_row_or_nothing() {
        let none = paint_header(None, 60);
        assert!(none.rows.is_empty() && none.heights.is_empty());

        let some = paint_header(Some(&LogicalLine::new("MODEL      CTX")), 60);
        assert_eq!(some.heights, vec![1]);
        assert_eq!(some.height(), 1);
        assert_eq!(display_width(&some.rows[0]), 60, "padded to full width");
    }

    /// Rows get their markers from `selected` / `active_rows`, and the section
    /// measures each one — an over-long row is taller here too.
    #[test]
    fn row_section_carries_markers_and_measures_split_rows() {
        let rows = vec![
            LogicalLine::new("plain"),
            LogicalLine::new("active"),
            LogicalLine::new("selected"),
            LogicalLine::new("w".repeat(200)),
        ];
        let s = paint_rows(&rows, Some(2), &[1], 60);
        assert!(s.rows[0].contains("  plain"));
        assert!(
            s.rows[1].contains("* active"),
            "active marker: {:?}",
            s.rows[1]
        );
        assert!(s.rows[2].contains("> selected"));
        assert_eq!(s.heights[3], 4, "the over-long row was split and measured");
        assert_eq!(s.height(), s.rows.len());
    }

    /// A selected *and* active row shows `*` — active wins. Pinned on the
    /// section painter because that is where the style is now decided.
    #[test]
    fn row_section_active_marker_beats_cursor() {
        let rows = vec![LogicalLine::new("both")];
        let s = paint_rows(&rows, Some(0), &[0], 60);
        assert!(s.rows[0].contains("* both"), "{:?}", s.rows[0]);
        assert!(
            !s.rows[0].contains(">"),
            "no cursor marker: {:?}",
            s.rows[0]
        );
    }

    /// The detail section does not truncate — a caller budgets it. Every line
    /// survives painting; an over-long one is split so the caller can still cut
    /// at a line boundary.
    #[test]
    fn detail_paints_every_line_and_leaves_the_cut_to_the_caller() {
        let long = "d".repeat(150);
        let lines = vec![
            LogicalLine::new("first"),
            LogicalLine::new(long.clone()),
            LogicalLine::new("last"),
        ];
        let s = paint_detail(&lines, 60);
        assert_eq!(s.heights.len(), 3, "all three logical lines kept");
        assert_eq!(s.heights[0], 1);
        assert!(s.heights[1] > 1, "the long line was split");
        assert_eq!(s.heights[2], 1);
        assert_eq!(s.height(), s.rows.len());
    }

    // --- window_heights ----------------------------------------------------
    //
    // The rows element's windowing primitive (`ui::RowsItem`). Heights, not row
    // counts: a wrapped row is several terminal rows and so does the budget.

    #[test]
    fn window_centres_on_a_single_line_selection() {
        let heights = vec![1usize; 10];
        let (s, c) = window_heights(&heights, 5, 4);
        assert_eq!(c, 4);
        assert!(s <= 5 && s + c > 5, "selection must be in the window");
        assert!(
            heights[s..s + c].iter().sum::<usize>() <= 4,
            "fits the budget"
        );
    }

    /// A three-row logical row counts for three rows of budget, so the window
    /// holds fewer *rows* than the budget has lines.
    #[test]
    fn window_measures_a_multi_line_row_in_rows_not_lines() {
        let heights = vec![1usize, 3, 1, 1, 1];
        let (s, c) = window_heights(&heights, 2, 4);
        assert!(s <= 2 && s + c > 2, "selection must be in the window");
        assert!(
            heights[s..s + c].iter().sum::<usize>() <= 4,
            "fits the budget: {:?}",
            &heights[s..s + c]
        );
    }

    #[test]
    fn window_shows_everything_when_it_fits() {
        assert_eq!(window_heights(&[1, 1, 1], 0, 10), (0, 3));
    }

    /// At least one row is always painted: a row taller than the whole budget
    /// still has to appear, or a tall option would make the menu look empty.
    /// (It may overflow the budget — the element clips to `h`.)
    #[test]
    fn window_always_shows_at_least_one_row() {
        let heights = vec![1usize, 9];
        let (s, c) = window_heights(&heights, 1, 4);
        assert_eq!(s, 0);
        assert_eq!(c, 1, "the tall row is included");
    }

    #[test]
    fn window_of_nothing_is_nothing() {
        assert_eq!(window_heights(&[], 0, 10), (0, 0));
        assert_eq!(window_heights(&[1, 1], 0, 0), (0, 0));
    }

    /// Near the end the window starts early enough to use the whole budget
    /// instead of leaving a gap below the selection.
    #[test]
    fn window_near_the_end_fills_backward() {
        let heights = vec![1usize; 100];
        let (s, c) = window_heights(&heights, 99, 20);
        assert_eq!(c, 20, "must use the full budget");
        assert_eq!(s, 80, "window starts 20 rows before the end");
        assert!(s <= 99 && s + c > 99, "selection in window");
    }

    /// ...and near the start it simply runs forward.
    #[test]
    fn window_near_the_start_fills_forward() {
        let heights = vec![1usize; 100];
        let (s, c) = window_heights(&heights, 0, 20);
        assert_eq!(c, 20, "must use the full budget");
        assert_eq!(s, 0, "starts at the top");
    }
}

use crate::table::display_width;
use crate::wrap::split_to_width;

/// Width of the marker column every painted row starts with: `"> "`, `"* "`,
/// or two spaces.
const MARKER: usize = 2;

/// How one painted overlay row is styled, and what the marker on its first
/// visual row says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowStyle {
    /// Detail lines (the node's `header()` output): plain text, no
    /// background, no marker — but still indented by [`MARKER`] so the
    /// sections line up.
    Detail,
    /// Column headings: the header background, no marker.
    TableHeader,
    /// A data row: the data background; `*` when the row is active, `>`
    /// when it is merely selected, two spaces when neither. An active row
    /// that is also selected shows `*` — active wins.
    Data { selected: bool, active: bool },
}

/// Paint `content` as terminal rows of exactly `tw` columns, joined by
/// `\n` (so a short line still yields one row).
///
/// The painter, not the caller, is what makes content *fit*: `content` is
/// split at the width with every character preserved (see
/// [`split_to_width`]), the style's marker goes on the first visual row and
/// two spaces on its continuations, and every visual row is padded to `tw`.
/// Splitting rather than reflowing is deliberate — a table row's column gaps
/// and an engine help header's alignment (`"  10      last 10 turns"`) must
/// survive, which word wrapping would collapse; the engine, which knows its
/// content, already wraps at word boundaries where it wants to.
///
/// This is what makes the painted row count equal the number of terminal
/// rows the overlay occupies. Trusting the caller to pre-wrap under-counted
/// long lines, so the frame reserved too few rows and a wrapped remainder
/// could go stale (ui_draw_v2 §9 #11).
pub fn paint_row(content: &str, style: RowStyle, tw: usize) -> String {
    let tw = tw.max(1);
    let (bg, fg, marker) = match style {
        RowStyle::Detail => ("", "", "  "),
        RowStyle::TableHeader => ("\x1b[48;5;237m", "", "  "),
        RowStyle::Data { selected, active } => (
            if selected {
                "\x1b[48;5;239m"
            } else {
                "\x1b[48;5;235m"
            },
            if selected { "\x1b[1;36m" } else { "" },
            if active {
                "* "
            } else if selected {
                "> "
            } else {
                "  "
            },
        ),
    };
    let content_w = tw.saturating_sub(MARKER).max(1);
    let mut out: Vec<String> = Vec::new();
    for (i, piece) in split_to_width(content, content_w).iter().enumerate() {
        let text = format!("{}{}", if i == 0 { marker } else { "  " }, piece);
        let pad = tw.saturating_sub(display_width(&text));
        out.push(format!("{bg}{fg}{text}{}\x1b[0m", " ".repeat(pad)));
    }
    out.join("\n")
}

/// Visual rows a `LogicalLine` occupies *as the engine wrote it*: its
/// `\n`-separated sub-lines (table wrapping puts them in the line's text).
/// One shared definition, so a multi-line row is never counted as one line in
/// one place and three in another.
///
/// This is the *engine's* measure, for a caller choosing a window before
/// painting — which is what [`Element::data_rows`] does, so it paints the
/// window rather than the table. What goes on screen is
/// [`PaintedSection::heights`]: the same rows after the painter split anything
/// too wide, which is what the window is then re-measured in.
pub fn visual_height(line: &LogicalLine) -> usize {
    line.0.split('\n').count().max(1)
}

/// Compute the window of row indices to render from a set of row *heights*
/// (in terminal rows). Returns `(start, count)` — the first row index and how
/// many rows to render. `budget` is in the same rows.
///
/// This is the real form: a row the painter split across three terminal rows is
/// budgeted as three. Callers window on [`visual_height`] first, then re-window
/// on the painted heights from [`PaintedSection::heights`] — see
/// [`Element::data_rows`], which is the one production user.
pub fn window_heights(heights: &[usize], selected: usize, budget: usize) -> (usize, usize) {
    let total = heights.len();
    if total == 0 || budget == 0 {
        return (0, 0);
    }
    let total_vis: usize = heights.iter().sum();
    if total_vis <= budget {
        return (0, total);
    }
    let sel = selected.min(total - 1);
    let half = budget / 2;

    // Walk backward from sel, using at most half the budget for rows above.
    let mut start = sel;
    let mut vis = heights[sel]; // always include the selected row
    while start > 0 && vis + heights[start - 1] <= half {
        start -= 1;
        vis += heights[start];
    }

    // Walk forward from start, filling the full budget.
    let mut count = 0;
    let mut vis_total = 0;
    for i in start..total {
        if vis_total + heights[i] > budget {
            break;
        }
        vis_total += heights[i];
        count += 1;
    }

    // If we hit the end of the list before filling the budget,
    // extend backward to use remaining space (avoids empty gap
    // below the window when selected is near the end).
    if start > 0 && vis_total < budget {
        while start > 0 {
            let h = heights[start - 1];
            if vis_total + h > budget {
                break;
            }
            start -= 1;
            vis_total += h;
            count += 1;
        }
    }

    // At least the selected row (may exceed budget if it's very tall).
    if count == 0 {
        count = 1;
    }

    (start, count)
}

/// One overlay section painted into terminal rows, with the per-logical-row
/// heights that let a caller window or truncate it afterwards.
///
/// The rows are **already split** — one `String` per terminal row, no embedded
/// newline — so a caller never re-derives a row count, and
/// `heights.iter().sum() == rows.len()` holds by construction. `heights` is
/// what keeps a multi-line logical row (a wrapped description, or a row the
/// painter split because it was too wide) *together*: `offset_at(i)` is where
/// logical row `i` starts, so `rows[offset_at(i) .. offset_at(i) + heights[i]]`
/// is that row and nothing else.
///
/// Keeping the height bookkeeping beside the painted rows is what lets the
/// three painters below take a width and nothing else — no budget, no
/// `term_h`, no knowledge of what will be shown. Budgeting and windowing stay
/// with the caller, which is where they already live ([`window_heights`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaintedSection {
    /// Painted terminal rows, one per screen row, in order.
    pub rows: Vec<String>,
    /// Painted height of each logical row this section was built from;
    /// parallel to the source `LogicalLine`s.
    pub heights: Vec<usize>,
}

impl PaintedSection {
    /// Terminal-row offset at which logical row `i` starts — the running sum
    /// of the heights before it. An `i` past the end clamps to `rows.len()`.
    pub fn offset_at(&self, i: usize) -> usize {
        self.heights[..i.min(self.heights.len())].iter().sum()
    }

    /// The painted rows of logical rows `[start, start + count)`, clamped to
    /// what exists — the window a caller chose out of [`Self::heights`].
    pub fn slice(&self, start: usize, count: usize) -> &[String] {
        let from = self.offset_at(start);
        let to = self.offset_at(start.saturating_add(count));
        &self.rows[from..to]
    }

    /// Total painted height — the same number as `rows.len()`, named for what
    /// it is used for: space budgets.
    pub fn height(&self) -> usize {
        self.rows.len()
    }
}

/// Paint a slice of logical lines, one `style(i)` per line, and measure each
/// line's painted height. The shared body of the three painters below — they
/// differ only in which style a line gets.
fn paint_section(
    lines: &[LogicalLine],
    tw: usize,
    style: impl Fn(usize) -> RowStyle,
) -> PaintedSection {
    let mut rows = Vec::new();
    let mut heights = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let block = paint_row(&line.0, style(i), tw);
        heights.push(block.split('\n').count());
        for r in block.split('\n') {
            rows.push(r.to_string());
        }
    }
    PaintedSection { rows, heights }
}

/// Paint the **detail** section — the node's `header()` output: plain text, no
/// background, indented to line up with the data rows.
///
/// Nothing is dropped here. A detail line wider than the terminal is *split*
/// (so the section can be cut at a line boundary); deciding *how many* detail
/// lines fit is the caller's budget, not the painter's.
pub fn paint_detail(detail: &[LogicalLine], term_w: usize) -> PaintedSection {
    paint_section(detail, term_w.max(1), |_| RowStyle::Detail)
}

/// Paint the **table header** — the column-label line, on its own background.
///
/// At most one logical row in, and one terminal row out (a label line too wide
/// for the terminal is split, not wrapped, per [`paint_row`]). `None` — a plan
/// with no columns to label — paints nothing.
pub fn paint_header(table_header: Option<&LogicalLine>, term_w: usize) -> PaintedSection {
    let tw = term_w.max(1);
    match table_header {
        Some(th) => paint_section(std::slice::from_ref(th), tw, |_| RowStyle::TableHeader),
        None => PaintedSection::default(),
    }
}

/// Paint the **data rows**, each with its own marker (`>` selected, `*` active,
/// two spaces otherwise) and background.
///
/// Every row is painted, including rows no window will show: this is
/// O(rows), not O(window). A caller that wants only the window should window
/// the `LogicalLine`s first and paint that — which is what [`PaintedSection`]
/// makes expressible.
pub fn paint_rows(
    rows: &[LogicalLine],
    selected: Option<usize>,
    active_rows: &[usize],
    term_w: usize,
) -> PaintedSection {
    let tw = term_w.max(1);
    paint_section(rows, tw, |i| RowStyle::Data {
        selected: selected == Some(i),
        active: active_rows.contains(&i),
    })
}

/// Compute the number of logical rows to skip for a page scroll, given the
/// rows' *painted* heights. Returns a signed delta (positive = down, negative
/// = up) that moves `selected` by at most `budget` terminal rows, clamped to
/// the row range.
///
/// This is the real form: a caller that has painted measures the rows from
/// [`PaintedSection::heights`], so a page is exactly one screen of rows.
/// [`page_delta`] is the `LogicalLine`-shaped wrapper, and is what the REPL's
/// page keys use — they page over rows it has not painted yet.
pub fn page_delta_heights(
    heights: &[usize],
    selected: usize,
    budget: usize,
    direction: i32,
) -> i32 {
    let total = heights.len();
    if total == 0 || budget == 0 {
        return 0;
    }
    let mut visual_used = 0usize;
    let mut delta = 0i32;
    if direction > 0 {
        // PageDown: count forward from selected+1
        let mut i = selected + 1;
        while i < total {
            let h = heights[i];
            if visual_used + h > budget && delta > 0 {
                break;
            }
            visual_used += h;
            delta += 1;
            i += 1;
        }
    } else {
        // PageUp: count backward from selected
        let mut i = selected;
        while i > 0 {
            let h = heights[i - 1];
            if visual_used + h > budget && delta < 0 {
                break;
            }
            i -= 1;
            visual_used += h;
            delta -= 1;
        }
    }
    delta
}

/// [`page_delta_heights`] for a raw plan's rows, measured the way the
/// *engine* wrote them ([`visual_height`]).
pub fn page_delta(rows: &[LogicalLine], selected: usize, budget: usize, direction: i32) -> i32 {
    let heights: Vec<usize> = rows.iter().map(visual_height).collect();
    page_delta_heights(&heights, selected, budget, direction)
}
