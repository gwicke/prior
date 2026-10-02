//! Render plan: plain-data description of the overlay panel.
//!
//! The crate emits a [`RenderPlan`]; the host renders it directly.
//! Row styling (markers, backgrounds) is applied by the host at render
//! time — the plan contains only data, not ANSI escapes.

use std::rc::Rc;

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
/// 3. `rows` — scrollable data (Rc-shared; same pointer on cursor moves)
#[derive(Debug, Clone)]
pub struct RenderPlan {
    /// Table column header (e.g. "Model   Ctx   Coding   Price in").
    /// Single line guaranteed. Changes only on terminal resize.
    pub table_header: Option<LogicalLine>,
    /// `header()` output: help text, stats, focused-row details.
    /// Changes on cursor move (detail updates). Truncatable vertically.
    pub detail: Vec<LogicalLine>,
    /// Scrollable data rows. Rc-shared — same pointer on cursor moves.
    ///
    /// Row text is laid out for the *content* width: `term_w - 2`, the
    /// terminal width minus the marker column the painter adds (see
    /// [`paint_row`]). A producer that laid a row out at the full width would
    /// have its last columns split onto a second terminal row. The engine's
    /// own tables are laid out that way (`render_multiline(tw - 2)`).
    pub rows: Rc<Vec<LogicalLine>>,
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
            rows: Rc::new(Vec::new()),
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

    /// Plan of `rows` data rows, each wrapping to `lines` visual rows.
    fn wrapped_plan(rows: usize, lines: usize) -> RenderPlan {
        let row = |i: usize| -> LogicalLine {
            let mut sub: Vec<String> = Vec::with_capacity(lines);
            sub.push(format!("r{i}"));
            for j in 1..lines {
                sub.push(format!("w{i}-{j}"));
            }
            LogicalLine::new(sub.join("\n"))
        };
        RenderPlan {
            table_header: None,
            detail: vec![],
            rows: Rc::new((0..rows).map(row).collect()),
            selected: Some(0),
            active_rows: vec![],
        }
    }

    /// The user-visible form of ui_draw_v2 §9 #7: painted lines must never
    /// *decrease* as the terminal grows. Budgeting wrapped rows as single
    /// lines made a bigger terminal show less — 15 painted lines at
    /// term_h 24 but only 9 at term_h 30, 40 and 80 for this plan.
    #[test]
    fn paint_overlay_shows_more_as_the_terminal_grows() {
        let plan = wrapped_plan(9, 3);
        let mut prev = 0;
        for term_h in [20usize, 24, 30, 40, 80] {
            let painted = paint_overlay(&plan, 60, term_h).rows.len();
            assert!(
                painted >= prev,
                "term_h {term_h} painted {painted} lines, fewer than the {prev} \
                 painted at a smaller height (budgets must be visual)"
            );
            prev = painted;
        }
        // A terminal tall enough for the whole plan shows all of it.
        assert_eq!(prev, 27, "9 rows x 3 visual rows");
    }

    /// §9 #11: a line wider than the terminal is *split* by the painter, so
    /// the painted row count is the number of terminal rows the overlay
    /// occupies — the frame's row accounting can trust it. Before, the line
    /// was emitted over-long (1 painted row, 2 terminal rows).
    #[test]
    fn over_long_detail_line_is_split_into_terminal_rows() {
        let long = "d".repeat(100);
        let plan = RenderPlan {
            table_header: None,
            detail: vec![LogicalLine::new(long)],
            rows: Rc::new(vec![LogicalLine::new("r0")]),
            selected: Some(0),
            active_rows: vec![],
        };
        let painted = paint_overlay(&plan, 60, 24);
        // 100 columns at a 58-column content width: 2 rows, then the data row.
        assert_eq!(painted.rows.len(), 3, "1 split detail row + 1 data row");
        assert_eq!(painted.data_heights, vec![1]);
    }

    /// Every painted row is exactly the terminal width wide, and holds no
    /// newline: the shape `ui::frame::Element::Overlay` requires and asserts.
    #[test]
    fn every_painted_row_is_one_full_width_terminal_row() {
        let plan = RenderPlan {
            table_header: Some(LogicalLine::new("col a     col b")),
            detail: vec![
                LogicalLine::new("short"),
                LogicalLine::new("a detail line that is quite a lot longer than the width"),
                LogicalLine::new(""),
            ],
            rows: Rc::new(vec![
                LogicalLine::new("laid out to the content width".to_string() + &" ".repeat(30)),
                LogicalLine::new("日本".repeat(40)),
            ]),
            selected: Some(0),
            active_rows: vec![],
        };
        for term_w in [20usize, 60, 80] {
            let painted = paint_overlay(&plan, term_w, 40);
            for row in &painted.rows {
                assert!(!row.contains('\n'), "row must be one screen row");
                assert_eq!(
                    display_width(row),
                    term_w,
                    "painted row is not exactly the terminal width"
                );
            }
        }
    }

    /// §9 #5: the focus is the *painted* row of the selection, whatever sits
    /// above it — detail, a table header, and the selection's own leading
    /// rows. The hosts used to re-derive this and got it wrong.
    #[test]
    fn focus_is_the_painted_row_of_the_selection() {
        let plan = RenderPlan {
            table_header: Some(LogicalLine::new("name   value")),
            detail: vec![
                LogicalLine::new("first detail line"),
                LogicalLine::new("second detail line"),
            ],
            rows: Rc::new(
                (0..6)
                    .map(|i| LogicalLine::new(format!("row {i}")))
                    .collect(),
            ),
            selected: Some(4),
            active_rows: vec![],
        };
        let painted = paint_overlay(&plan, 60, 40);
        let focus = painted.focus.expect("the selection is painted");
        // 2 detail rows + 1 table header + the 4 rows above the selection.
        assert_eq!(focus, 7, "the selection's painted row, not the plan's");
        assert!(
            painted.rows[focus].contains("row 4"),
            "focus row is the selection: {:?}",
            painted.rows[focus]
        );
    }

    /// A truncated detail section shifts every data row up — the focus must
    /// follow the paint, not the plan.
    #[test]
    fn focus_follows_a_truncated_detail_section() {
        let plan = RenderPlan {
            table_header: None,
            detail: (0..40)
                .map(|i| LogicalLine::new(format!("detail {i}")))
                .collect(),
            rows: Rc::new(vec![LogicalLine::new("row 0")]),
            selected: Some(0),
            active_rows: vec![],
        };
        // A short terminal: the detail is cut hard, the single row still shows.
        let painted = paint_overlay(&plan, 60, 12);
        let focus = painted.focus.expect("the selection is painted");
        assert_eq!(
            painted.rows.len(),
            painted.data_heights.iter().sum::<usize>() + focus
        );
        assert!(
            painted.rows[focus].contains("row 0"),
            "focus points at the selection: {:?}",
            painted.rows[focus]
        );
    }

    /// An active row shows `*` instead of `>`, so a host that sniffed the
    /// cursor marker out of the painted bytes found nothing and fell back to
    /// row 0. The painter knows which row it selected.
    #[test]
    fn focus_is_reported_even_when_the_active_marker_hides_the_cursor() {
        let plan = RenderPlan {
            table_header: None,
            detail: vec![],
            rows: Rc::new(
                (0..5)
                    .map(|i| LogicalLine::new(format!("row {i}")))
                    .collect(),
            ),
            selected: Some(2),
            active_rows: vec![2],
        };
        let painted = paint_overlay(&plan, 60, 40);
        assert_eq!(painted.focus, Some(2));
        assert!(
            painted.rows[2].contains("* row 2"),
            "the active marker replaced the cursor marker: {:?}",
            painted.rows[2]
        );
    }

    /// `data_heights` are the painted heights, in row order — the invariant
    /// hosts page against.
    #[test]
    fn data_heights_are_the_painted_heights() {
        let plan = RenderPlan {
            table_header: None,
            detail: vec![],
            rows: Rc::new(vec![
                LogicalLine::new("one"),
                LogicalLine::new("two\nwrapped\nlines"),
                LogicalLine::new("a row far too wide to fit the content width of this terminal"),
            ]),
            selected: Some(0),
            active_rows: vec![],
        };
        let painted = paint_overlay(&plan, 40, 40);
        assert_eq!(painted.data_heights[0], 1);
        assert_eq!(painted.data_heights[1], 3, "embedded newlines are rows");
        assert!(painted.data_heights[2] > 1, "the over-long row was split");
    }

    /// No selection, no rows, no focus — and the text wrapper is exactly the
    /// painted rows joined.
    #[test]
    fn empty_plan_paints_nothing() {
        let painted = paint_overlay(&RenderPlan::default(), 60, 24);
        assert!(painted.rows.is_empty());
        assert_eq!(painted.focus, None);
        assert!(painted.data_heights.is_empty());
        assert_eq!(paint_plan(&RenderPlan::default(), 60, 24), "");
    }

    #[test]
    fn paint_plan_is_the_painted_rows_joined() {
        let plan = wrapped_plan(3, 2);
        let painted = paint_overlay(&plan, 40, 24);
        assert_eq!(paint_plan(&plan, 40, 24), painted.rows.join("\n"));
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

    /// Composing the sections by hand is exactly what `paint_overlay` does, so
    /// a host building separate frame elements gets the same bytes.
    #[test]
    fn composing_sections_by_hand_matches_paint_overlay() {
        let plan = RenderPlan {
            table_header: Some(LogicalLine::new("MODEL")),
            detail: vec![LogicalLine::new("pick one")],
            rows: Rc::new(
                (0..20)
                    .map(|i| LogicalLine::new(format!("model-{i}")))
                    .collect(),
            ),
            selected: Some(15),
            active_rows: vec![3],
        };
        let detail = paint_detail(&plan.detail, 60);
        let header = paint_header(plan.table_header.as_ref(), 60);
        let rows = paint_rows(&plan.rows, plan.selected, &plan.active_rows, 60);
        // detail, then header, then the data rows from the top: the section
        // order, painted but not windowed.
        let mut hand: Vec<String> = Vec::new();
        hand.extend_from_slice(&detail.rows);
        hand.extend_from_slice(&header.rows);
        hand.extend_from_slice(&rows.rows);

        let painted = paint_overlay(&plan, 60, 24);
        assert_eq!(hand[..2], painted.rows[..2], "detail + header come first");
        assert!(hand[2].contains("model-0"), "the section starts at row 0");
        // The hand-built section has every row; `paint_overlay` windowed them
        // to the budget, so it is strictly shorter — and the window *contains*
        // the selection (it fills past it, which is `window_heights`' rule).
        assert_eq!(rows.height(), 20);
        assert!(painted.rows.len() < hand.len(), "the plan was windowed");
        let focus = painted.focus.expect("the selection is painted");
        assert!(
            painted.rows[focus].contains("> model-15"),
            "the focus row is the selection: {:?}",
            painted.rows[focus]
        );
        // Detail + header sit above the window, so the focus index carries them.
        assert_eq!(focus, 2 + 9, "2 fixed rows + 9 into the data window");
        assert!(
            painted.rows.last().unwrap().contains("model-19"),
            "the window reaches the last row: {:?}",
            painted.rows.last().unwrap()
        );
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
/// This is the *engine's* measure, for hosts reasoning about a plan they have
/// not painted. What goes on screen is [`PaintedOverlay::data_heights`] —
/// the same rows after the painter split anything too wide; budgets and
/// windows inside `paint_plan` use those.
pub fn visual_height(line: &LogicalLine) -> usize {
    line.0.split('\n').count().max(1)
}

/// Painted heights of the three overlay sections, in terminal rows — the
/// unit the space budget is computed in. `paint_plan` fills this in from
/// what it actually painted, so the budget, the window and the screen all
/// speak the same unit (ui_draw_v2 §9 #7, #11).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SectionHeights {
    /// The detail section (the node's `header()` output), as painted.
    pub detail: usize,
    /// The table-header line, as painted (0 when the plan has none).
    pub table_header: usize,
    /// All data rows together, as painted.
    pub data: usize,
}

/// Compute vertical space budgets for the detail and data sections within
/// the overlay. Returns `(detail_budget, rows_budget)`, **both in painted
/// terminal rows** — the unit `paint_plan` hands to [`window_heights`].
///
/// When both sections overflow, they split available space 50:50.
/// If one section fits in its half, the other gets the surplus.
/// Rows always get at least `min_row_lines` (capped at their total height).
///
/// Everything is measured in painted rows and supplied by the caller: an
/// earlier form took a *logical* row count and returned it as a *visual*
/// budget, so every wrapped row was budgeted as one line and the window
/// clipped plans the terminal had room for — a bigger terminal could then
/// show *less* (ui_draw_v2 §9 #7). `min_row_lines` is a row floor (3 rows
/// of 3 lines each get 9, not 3), the one semantic change beyond the unit
/// fix.
pub fn compute_space_budget(
    heights: SectionHeights,
    overlay_budget: usize, // total overlay rows (e.g. 2/3 terminal)
    min_row_lines: usize,
) -> (usize, usize) {
    let available = overlay_budget.saturating_sub(heights.table_header);
    let half = available / 2;
    let min_rows_guaranteed = min_row_lines.min(heights.data);

    // When rows overflow their half, detail is capped at half so rows
    // are not starved.  When rows fit, detail may use up to
    // `available - min_row_lines` (the surplus flows from rows' unused half).
    let detail_limit = if heights.data > half {
        // Both overflow (or rows do): detail capped at half.
        half
    } else {
        // Rows fit: detail may use their unused half.
        available.saturating_sub(min_rows_guaranteed)
    };
    let detail_budget = heights.detail.min(detail_limit);

    // Rows get the remaining space, at least `min_rows_guaranteed`.
    // When rows fit in their half, cap at their total height (the painter
    // shows `min(total, budget)` anyway, but a tighter budget is clearer
    // for callers).
    let rows_budget = if heights.data > half {
        available
            .saturating_sub(detail_budget)
            .max(min_rows_guaranteed)
    } else {
        heights.data
    };

    (detail_budget, rows_budget)
}

/// Compute the window of row indices to render from a set of row *heights*
/// (in terminal rows). Returns `(start, count)` — the first row index and how
/// many rows to render. `budget` is in the same rows.
///
/// This is the real form: callers that have painted (or can measure) their
/// rows pass the painted heights, so a row the painter split across three
/// terminal rows is budgeted as three. [`visual_row_window`] is the
/// `LogicalLine`-shaped wrapper for callers reasoning about a raw plan.
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

/// [`window_heights`] for a raw plan's rows, measured the way the *engine*
/// wrote them ([`visual_height`]). Hosts that have painted should use the
/// heights from [`PaintedOverlay::data_heights`] instead — a row the painter
/// split across several terminal rows is taller on screen than it is here.
pub fn visual_row_window(rows: &[LogicalLine], selected: usize, budget: usize) -> (usize, usize) {
    let heights: Vec<usize> = rows.iter().map(visual_height).collect();
    window_heights(&heights, selected, budget)
}

/// A painted overlay: the terminal rows to show, plus the geometry the host
/// needs to place and page them.
///
/// Every field is measured in *painted* terminal rows, so nothing here has to
/// be re-derived from the plan or sniffed back out of the painted bytes
/// (ui_draw_v2 §9 #5, #11).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaintedOverlay {
    /// The painted screen rows, one `String` per terminal row, each padded
    /// to `term_w` and free of embedded newlines — exactly what
    /// `ui::frame::Element::Overlay` requires, so a host never re-splits.
    pub rows: Vec<String>,
    /// Index into `rows` of the selected row's *first* visual row — the
    /// windowing focus. `None` when the plan has no selection or the
    /// selection did not fit the budget.
    pub focus: Option<usize>,
    /// Painted height of each logical data row, parallel to `plan.rows`.
    pub data_heights: Vec<usize>,
    /// Terminal rows the data section received — the page size a host
    /// paging the cursor should move by.
    pub data_budget: usize,
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

/// Paint an engine `RenderPlan` into terminal rows: detail lines, table
/// header, then data rows windowed around `selected` within the 2/3-height
/// overlay budget. Shared by the plan-carrying overlay variants (slash
/// `V25Menu`, prose `PathComplete`) — the one place a `RenderPlan` becomes
/// display rows, and the only place that knows the resulting geometry.
///
/// Composes the three section painters, so section order, styles and
/// measurement are defined once and shared with any caller that composes
/// sections itself. Budgeting happens here, in painted rows: the painter owns
/// both the fitting ([`paint_row`]) and the measurement, so `rows.len()` is the
/// number of terminal rows the overlay occupies, and every budget, window and
/// page size below is in that same unit.
pub fn paint_overlay(plan: &RenderPlan, term_w: usize, term_h: usize) -> PaintedOverlay {
    // `term_h` is the full terminal height; 2/3 is the overlay budget.
    let max_pop = ((term_h as f64 * 2.0 / 3.0).ceil() as usize).max(1);
    let min_rows = 3usize;

    let detail = paint_detail(&plan.detail, term_w);
    let header = paint_header(plan.table_header.as_ref(), term_w);
    let data = paint_rows(&plan.rows, plan.selected, &plan.active_rows, term_w);

    let (detail_budget, data_budget) = compute_space_budget(
        SectionHeights {
            detail: detail.height(),
            table_header: header.height(),
            data: data.height(),
        },
        max_pop,
        min_rows,
    );

    // --- Emit. ---
    let mut rows: Vec<String> = Vec::new();

    // 1. Detail, truncated from the bottom to its budget. Lines stay whole:
    //    the cut lands between detail lines, never inside one.
    let mut detail_used = 0usize;
    let mut detail_end = 0usize;
    for &height in &detail.heights {
        if detail_used + height > detail_budget {
            break;
        }
        detail_used += height;
        detail_end += height;
    }
    rows.extend_from_slice(&detail.rows[..detail_end]);
    // Detail that did not use its budget gives the surplus to the rows.
    let data_budget = data_budget + (detail_budget - detail_used);

    // 2. Table header.
    rows.extend_from_slice(&header.rows);

    // 3. Data rows, windowed around `selected` in painted rows. The focus is
    //    the painted index of the selection's first row, so it survives a
    //    truncated detail section, a table header, a multi-row selection, and
    //    the `*` active marker that hides the `>` cursor marker.
    let total = data.heights.len();
    let mut focus = None;
    if total > 0 {
        let sel_idx = plan.selected.unwrap_or(0);
        let (win_start, win_count) = window_heights(&data.heights, sel_idx, data_budget);
        let win_count = win_count.min(total.saturating_sub(win_start));
        for (i, height) in data
            .heights
            .iter()
            .enumerate()
            .skip(win_start)
            .take(win_count)
        {
            let start = data.offset_at(i);
            if plan.selected == Some(i) {
                focus = Some(rows.len());
            }
            rows.extend_from_slice(&data.rows[start..start + height]);
        }
    }

    PaintedOverlay {
        rows,
        focus,
        data_heights: data.heights,
        data_budget,
    }
}

/// The painted overlay as one ANSI string, rows joined by `\n` — the text
/// form of [`paint_overlay`], for callers that want to re-split it or keep
/// it for a diff. Hosts that place the rows in a frame should take
/// [`PaintedOverlay::rows`] instead: that is the form the frame wants, and
/// re-splitting a string is how a caller ended up with a row count that
/// disagreed with the screen.
pub fn paint_plan(plan: &RenderPlan, term_w: usize, term_h: usize) -> String {
    paint_overlay(plan, term_w, term_h).rows.join("\n")
}

/// Compute the number of logical rows to skip for a page scroll, given the
/// rows' *painted* heights. Returns a signed delta (positive = down, negative
/// = up) that moves `selected` by at most `budget` terminal rows, clamped to
/// the row range.
///
/// This is the real form: a host that has painted takes the heights from
/// [`PaintedOverlay::data_heights`] and the budget from
/// [`PaintedOverlay::data_budget`], so a page is exactly one screen of rows.
/// [`page_delta`] is the `LogicalLine`-shaped wrapper for callers reasoning
/// about a raw plan.
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
