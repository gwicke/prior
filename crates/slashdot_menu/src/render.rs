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

// --- v2.0 compat: PlanBuilder kept for slash_menu_bridge ---

pub struct PlanBuilder {
    pub title: String,
    pub child_header: Option<String>,
    pub row_header: Option<String>,
}

impl PlanBuilder {
    pub fn build(
        &self,
        state: &crate::interpreter::MenuState,
        max_width: usize,
        detail: Vec<String>,
    ) -> RenderPlan {
        use crate::table::{Align, Table};

        let mut table_header_line: Option<LogicalLine> = None;
        let mut detail_lines: Vec<LogicalLine> = Vec::new();
        let mut row_lines: Vec<LogicalLine> = Vec::new();

        // child header → detail
        if let Some(h) = &self.child_header {
            detail_lines.push(LogicalLine::new(h.clone()));
        }

        // per-row derived header for dynamic levels
        if let Some(sel) = state.selected() {
            if state.drill_in {
                if let Some(cd) = sel
                    .field_value("child_description")
                    .and_then(|v| v.as_str())
                    .or(sel.description.as_deref())
                {
                    detail_lines.push(LogicalLine::new(cd.to_string()));
                }
            }
        }

        // Table column header
        let use_table = if !state.columns.is_empty() {
            true
        } else {
            state.candidates.first().map(|r| !r.fields.is_empty()).unwrap_or(false)
        };

        let row_texts: Vec<String> = if use_table {
            let colnames: Vec<String> = if state.columns.is_empty() {
                let mut names = vec!["id".to_string()];
                names.extend(state.candidates[0].fields.iter().map(|(k, _)| k.clone()));
                names
            } else {
                state.columns.iter().map(|c| {
                    if c.field.is_empty() { "id".to_string() } else { c.field.clone() }
                }).collect()
            };
            let headers: Vec<&str> = if state.columns.is_empty() {
                colnames.iter().map(String::as_str).collect()
            } else {
                state.columns.iter().map(|c| c.header.as_str()).collect()
            };
            let mut t = Table::new(&headers).set_gap(2);
            if !state.columns.is_empty() {
                let aligns: Vec<Align> = state.columns.iter().map(|c| c.align).collect();
                let mins: Vec<usize> = state.columns.iter().map(|c| c.min_width).collect();
                let prios: Vec<usize> = state.columns.iter().map(|c| c.priority).collect();
                t = t.with_aligns(&aligns).with_min_widths(&mins).with_priorities(&prios);
            }
            for r in &state.candidates {
                let mut cells: Vec<String> = Vec::with_capacity(colnames.len());
                for k in &colnames {
                    cells.push(
                        r.field_display(k)
                            .or_else(|| {
                                crate::fields::row_field_value(r, k).map(|v| match v {
                                    crate::value::Value::Str(s) => s.clone(),
                                    crate::value::Value::Num(n) => crate::value::fmt_num(n),
                                })
                            })
                            .unwrap_or_default(),
                    );
                }
                t.add_row(&cells);
            }
            let rendered = t.render_multiline(max_width.saturating_sub(2));
            // Table header — only if at least one column has a label.
            if state.columns.iter().any(|c| !c.header.is_empty()) {
                let header_text = t.render_header(max_width.saturating_sub(2));
                table_header_line = Some(LogicalLine::new(header_text));
            }
            rendered
        } else {
            state.candidates.iter().map(|r| match &r.description {
                Some(d) if !d.is_empty() => format!("{} — {}", r.id, d),
                _ => r.id.clone(),
            }).collect()
        };

        // Data rows: each row is one LogicalLine with embedded \n for wrapping
        for text in row_texts {
            row_lines.push(LogicalLine::new(text));
        }

        // Additional detail lines from caller (e.g. stat summary, replay help).
        for d in detail {
            detail_lines.push(LogicalLine::new(d));
        }

        let active_rows: Vec<usize> = if let Some(a) = state.active_row_marker {
            vec![a]
        } else {
            Vec::new()
        };

        RenderPlan {
            table_header: table_header_line,
            detail: detail_lines,
            rows: Rc::new(row_lines),
            selected: if state.candidates.is_empty() { None } else { Some(state.active) },
            active_rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::MenuState;

    fn empty_state() -> MenuState {
        MenuState {
            node_path: vec![],
            terms: String::new(),
            selection: None,
            drill_in: false,
            candidates: vec![crate::fields::Row::new("a"), crate::fields::Row::new("b")],
            columns: vec![],
            active: 0,
            pin_lost: false,
            level_is_dynamic: false,
            active_row_marker: None,
        }
    }

    #[test]
    fn plan_has_header_and_rows() {
        let b = PlanBuilder {
            title: "Models".into(),
            child_header: Some("pick a model".into()),
            row_header: None,
        };
        let plan = b.build(&empty_state(), 80, Vec::new());
        // detail has the child header
        assert_eq!(plan.detail.len(), 1);
        assert_eq!(plan.rows.len(), 2);
        assert_eq!(plan.selected, Some(0));
    }

    #[test]
    fn markers_cursor_active() {
        assert_eq!(Marker::Cursor.as_str(), "> ");
        assert_eq!(Marker::Active.as_str(), "* ");
        assert_eq!(Marker::CursorActive.as_str(), "* ");
        assert_eq!(Marker::None.as_str(), "  ");
    }

    #[test]
    fn paint_matches_overlay_shape() {
        let b = PlanBuilder {
            title: "T".into(),
            child_header: None,
            row_header: None,
        };
        let plan = b.build(&empty_state(), 40, Vec::new());
        let lines = plan.paint(40);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("a"));
        assert!(lines[1].contains("b"));
    }

    #[test]
    fn declared_columns_drive_layout() {
        use crate::provider::ColumnDef;
        use crate::value::Value;
        let mut st = empty_state();
        st.candidates[0] = crate::fields::Row::new("a")
            .field("price", Value::Num(2.5))
            .field("ctx", Value::Num(1000.0));
        st.candidates[1] = crate::fields::Row::new("b")
            .field("price", Value::Num(0.1234))
            .field("ctx", Value::Num(8000.0));
        st.columns = vec![
            ColumnDef::new("id", "Model").min(8).prio(3),
            ColumnDef::new("price", "$/M")
                .align(crate::table::Align::Right)
                .min(4)
                .prio(2),
            ColumnDef::new("ctx", "Ctx")
                .align(crate::table::Align::Right)
                .min(3)
                .prio(1),
        ];
        st.active_row_marker = Some(1);
        let b = PlanBuilder {
            title: "M".into(),
            child_header: None,
            row_header: None,
        };
        let plan = b.build(&st, 60, Vec::new());
        assert_eq!(plan.selected, Some(0));
        assert_eq!(plan.active_rows, vec![1]);
        let painted = plan.paint(60);
        let opt_b = painted.iter().find(|l| l.contains('b')).expect("row b present");
        assert!(opt_b.contains("* b"), "active row shows `*`: {opt_b:?}");
    }

    #[test]
    fn active_marker_beats_cursor_on_same_row() {
        let mut st = empty_state();
        st.candidates = vec![crate::fields::Row::new("x")];
        st.active = 0;
        st.active_row_marker = Some(0);
        let b = PlanBuilder { title: "T".into(), child_header: None, row_header: None };
        let plan = b.build(&st, 40, Vec::new());
        let painted = plan.paint(40);
        assert!(painted.iter().any(|l| l.contains("* x")), "shared row shows `*`, not `>`: {painted:?}");
    }

    #[test]
    fn drill_in_row_header_from_description() {
        let mut st = empty_state();
        st.drill_in = true;
        st.candidates[0] = crate::fields::Row::new("a").desc("long detail header");
        let b = PlanBuilder {
            title: "T".into(),
            child_header: None,
            row_header: None,
        };
        let plan = b.build(&st, 80, Vec::new());
        assert!(plan.detail.iter().any(|l| l.0.contains("long detail header")));
    }
}
