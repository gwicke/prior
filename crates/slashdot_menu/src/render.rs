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

impl RenderPlan {
    /// Paint to ANSI text lines. Used by v2.0 overlays that haven't been
    /// ported to the new direct-rendering path. The v25 overlay renders
    /// directly from `LogicalLine` without calling this.
    pub fn paint(&self, term_width: usize) -> Vec<String> {
        let tw = term_width.max(1);
        let mut out = Vec::new();

        // Reconstruct flat line list from the three sections.
        let mut flat: Vec<FlatLine> = Vec::new();

        // detail (header() output) comes first, then table header.
        for d in &self.detail {
            for sub in d.0.split('\n') {
                flat.push(FlatLine {
                    text: sub.to_string(),
                    is_header: true,
                    has_bg: false,
                    is_marker_row: false,
                    option: usize::MAX,
                });
            }
        }

        if let Some(ref th) = self.table_header {
            flat.push(FlatLine {
                text: th.0.clone(),
                is_header: true,
                has_bg: true,
                is_marker_row: false,
                option: usize::MAX,
            });
        }

        for (i, row) in self.rows.iter().enumerate() {
            for (j, sub) in row.0.split('\n').enumerate() {
                flat.push(FlatLine {
                    text: sub.to_string(),
                    is_header: false,
                    has_bg: true,
                    is_marker_row: j == 0,
                    option: i,
                });
            }
        }

        for l in &flat {
            let sel = self.selected == Some(l.option);
            let act = self.active_rows.contains(&l.option);
            let bg = if !l.has_bg { "" }
                     else if l.is_header { "\x1b[48;5;237m" }
                     else if sel { "\x1b[48;5;239m" }
                     else { "\x1b[48;5;235m" };
            let fg = if sel { "\x1b[1;36m" } else { "" };
            let marker = if !l.is_marker_row {
                "  "
            } else if l.is_header {
                "  "
            } else if act {
                "* "
            } else if sel {
                "> "
            } else {
                "  "
            };
            let content = format!("{}{}", marker, l.text);
            let vis = crate::table::display_width(&content);
            let pad = tw.saturating_sub(vis);
            out.push(format!("{}{}{}{}{}", bg, fg, content, " ".repeat(pad), "\x1b[0m"));
        }

        out
    }
}

/// Helper for `paint()` — flat line with styling metadata.
struct FlatLine {
    text: String,
    is_header: bool,
    has_bg: bool,
    is_marker_row: bool,
    option: usize,
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
                if matches!(self, Marker::Cursor) { "> " } else { "  " }
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

    #[test]
    fn paint_matches_overlay_shape() {
        let plan = RenderPlan {
            rows: Rc::new(vec![LogicalLine::new("a"), LogicalLine::new("b")]),
            ..Default::default()
        };
        let lines = plan.paint(40);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("a"));
        assert!(lines[1].contains("b"));
    }

    /// Paint precedence: an active row shows `*` even when it is also the
    /// cursor row (`>` loses to `*`, Marker contract).
    #[test]
    fn active_marker_beats_cursor_on_same_row() {
        let plan = RenderPlan {
            rows: Rc::new(vec![LogicalLine::new("x")]),
            selected: Some(0),
            active_rows: vec![0],
            ..Default::default()
        };
        let painted = plan.paint(40);
        assert!(
            painted.iter().any(|l| l.contains("* x")),
            "shared row shows `*`, not `>`: {painted:?}"
        );
    }
}
