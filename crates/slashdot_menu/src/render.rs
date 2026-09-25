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
}
