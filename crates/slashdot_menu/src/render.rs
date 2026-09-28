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
}

use crate::table::display_width;

/// Render a LogicalLine as a single ANSI-styled string. Used for
/// table_header and detail lines (not data rows, which are rendered
/// inline with markers). Splits on `\n` for multi-line logical lines.
pub fn render_logical_line(
    line: &LogicalLine,
    tw: usize,
    is_selected: bool,
    is_active: bool,
    is_header: bool,
) -> String {
    let bg = if is_header {
        "\x1b[48;5;237m"
    } else if is_selected {
        "\x1b[48;5;239m"
    } else {
        ""
    };
    let fg = if is_selected { "\x1b[1;36m" } else { "" };
    let mut parts = Vec::new();
    for (j, sub) in line.0.split('\n').enumerate() {
        let marker = if j == 0 {
            if is_active {
                "* "
            } else if is_selected {
                "> "
            } else {
                "  "
            }
        } else {
            "  "
        };
        let content = format!("{}{}", marker, sub);
        let vis = display_width(&content);
        let pad = tw.saturating_sub(vis);
        parts.push(format!(
            "{}{}{}{}{}",
            bg,
            fg,
            content,
            " ".repeat(pad),
            "\x1b[0m"
        ));
    }
    parts.join("\n")
}

/// Compute vertical space budgets for detail and data rows within the
/// overlay. Returns `(detail_budget, rows_budget)` in visual lines.
///
/// When both sections overflow, they split available space 50:50.
/// If one section fits in its half, the other gets the surplus.
/// Rows always get at least `min_rows` (capped at `total_rows`).
pub fn compute_space_budget(
    overlay_budget: usize, // total overlay lines (e.g. 2/3 terminal)
    table_header_lines: usize,
    total_detail_vis: usize, // visual lines all detail items consume
    total_rows: usize,
    min_rows: usize,
) -> (usize, usize) {
    let available = overlay_budget.saturating_sub(table_header_lines);
    let half = available / 2;
    let min_rows_guaranteed = min_rows.min(total_rows);

    // When rows overflow their half, detail is capped at half so rows
    // are not starved.  When rows fit, detail may use up to
    // `available - min_rows` (the surplus flows from rows' unused half).
    let detail_limit = if total_rows > half {
        // Both overflow (or rows do): detail capped at half.
        half
    } else {
        // Rows fit: detail may use their unused half.
        available.saturating_sub(min_rows_guaranteed)
    };
    let detail_budget = total_detail_vis.min(detail_limit);

    // Rows get the remaining space, at least `min_rows_guaranteed`.
    // When rows fit in their half, cap at total_rows (the renderer
    // shows `min(total, budget)` anyway, but a tighter budget is
    // clearer for callers).
    let rows_budget = if total_rows > half {
        available
            .saturating_sub(detail_budget)
            .max(min_rows_guaranteed)
    } else {
        total_rows
    };

    (detail_budget, rows_budget)
}

/// Compute the window of row indices to render, accounting for multi-line
/// `LogicalLine` rows.  Returns `(start, count)` — the first row index and
/// how many logical rows to render.  `budget` is in visual lines.
pub fn visual_row_window(rows: &[LogicalLine], selected: usize, budget: usize) -> (usize, usize) {
    let total = rows.len();
    if total == 0 || budget == 0 {
        return (0, 0);
    }
    let heights: Vec<usize> = rows
        .iter()
        .map(|r| r.0.split('\n').count().max(1))
        .collect();
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

/// Paint an engine `RenderPlan` to ANSI overlay text: detail lines,
/// table header, then data rows windowed around `selected` within
/// the 2/3-height overlay budget. Shared by the plan-carrying
/// overlay variants (slash `V25Menu`, prose `PathComplete`) — the
/// one place a `RenderPlan` becomes display lines. Height is a
/// parameter; width is passed by the host (the plan is already
/// built at that width).
pub fn paint_plan(plan: &RenderPlan, term_w: usize, term_h: usize) -> String {
    let tw = term_w.max(1);
    let mut result: Vec<String> = Vec::new();

    // --- Space allocation ---
    // `term_h` is the full terminal height; 2/3 is the overlay budget.
    let max_pop = ((term_h as f64 * 2.0 / 3.0).ceil() as usize).max(1);
    let table_header_lines = if plan.table_header.is_some() { 1 } else { 0 };
    let min_rows = 3usize;

    // Count visual lines each detail item occupies (LogicalLine
    // may contain embedded \n for wrapped sub-lines).
    let detail_visual: Vec<usize> = plan
        .detail
        .iter()
        .map(|d| d.0.split('\n').count())
        .collect();
    let total_detail_vis: usize = detail_visual.iter().sum();
    let total_rows = plan.rows.len();

    let (detail_budget, data_budget) = compute_space_budget(
        max_pop,
        table_header_lines,
        total_detail_vis,
        total_rows,
        min_rows,
    );

    // 1. Detail (header() output — no background, stands out).
    let mut detail_vis_used = 0usize;
    for (d, &vis) in plan.detail.iter().zip(&detail_visual) {
        if detail_vis_used + vis > detail_budget {
            break;
        }
        result.push(render_logical_line(d, tw, false, false, false));
        detail_vis_used += vis;
    }

    // 2. Table header (column labels, grey background).
    if let Some(ref th) = plan.table_header {
        result.push(render_logical_line(th, tw, false, false, true));
    }

    // 3. Data rows, windowed around `selected` (visual-line-aware).
    let total = plan.rows.len();
    if total > 0 {
        let sel_idx = plan.selected.unwrap_or(0);
        let (win_start, win_count) = visual_row_window(&plan.rows, sel_idx, data_budget);
        for i in win_start..(win_start + win_count).min(total) {
            let is_selected = plan.selected == Some(i);
            let is_active = plan.active_rows.contains(&i);
            let row = &plan.rows[i];
            let sub_lines: Vec<&str> = row.0.split('\n').collect();
            for (j, sub) in sub_lines.iter().enumerate() {
                let first = j == 0;
                let marker = if first {
                    if is_active {
                        "* "
                    } else if is_selected {
                        "> "
                    } else {
                        "  "
                    }
                } else {
                    "  "
                };
                let bg = if is_selected {
                    "\x1b[48;5;239m"
                } else {
                    "\x1b[48;5;235m"
                };
                let fg = if is_selected { "\x1b[1;36m" } else { "" };
                let content = format!("{}{}", marker, sub);
                let vis = display_width(&content);
                let pad = tw.saturating_sub(vis);
                result.push(format!(
                    "{}{}{}{}{}",
                    bg,
                    fg,
                    content,
                    " ".repeat(pad),
                    "\x1b[0m"
                ));
            }
        }
    }
    result.join("\n")
}

/// Compute the number of logical rows to skip for a page scroll,
/// accounting for multi-line (wrapped) rows. Returns a signed delta
/// (positive = down, negative = up) that moves `selected` by at most
/// `budget` visual lines, clamped to [0, total_rows).
pub fn page_delta(rows: &[LogicalLine], selected: usize, budget: usize, direction: i32) -> i32 {
    let total = rows.len();
    if total == 0 || budget == 0 {
        return 0;
    }
    let mut visual_used = 0usize;
    let mut delta = 0i32;
    if direction > 0 {
        // PageDown: count forward from selected+1
        let mut i = selected + 1;
        while i < total {
            let h = rows[i].0.lines().count().max(1);
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
            let h = rows[i - 1].0.lines().count().max(1);
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
