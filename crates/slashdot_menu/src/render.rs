//! Render plan (design §6): plain-data description of the overlay panel.
//!
//! The crate emits a [`RenderPlan`]; the host paints it with its UI toolkit
//! (ratatui today, anything else later). No ratatui types cross the boundary.
//!
//! Row styling uses plain ANSI SGR strings owned by the crate so host and
//! crate agree on the palette without sharing UI-framework types.

/// Style primitives (ANSI SGR sequences), crate-owned.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    pub bg: &'static str,
    pub fg: &'static str,
}

impl Style {
    pub const PANEL: Style = Style {
        bg: "\x1b[48;5;235m",
        fg: "",
    };
    pub const HEADER: Style = Style {
        bg: "\x1b[48;5;237m",
        fg: "",
    };
    pub const SELECTED: Style = Style {
        bg: "\x1b[48;5;239m",
        fg: "\x1b[1;36m",
    };
    pub const RESET: &'static str = "\x1b[0m";
}

/// One rendered line with its style.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderLine {
    pub text: String,
    pub style: Style,
    /// Non-selectable header line (BG_HEADER).
    pub is_header: bool,
    /// First visual line of a logical option (marker row).
    pub is_marker_row: bool,
    /// Index of the logical option this line belongs to.
    pub option: usize,
}

/// Marker for a logical option (v1 contract: `>` cursor, `*` active-state;
/// `*` wins when both).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// Cursor-selected row: `> `.
    Cursor,
    /// Current-state row: `* `.
    Active,
    /// Both (cursor on the active row): `* ` wins.
    CursorActive,
    /// Continuation / plain row: `  `.
    None,
}

impl Marker {
    pub fn as_str(&self) -> &'static str {
        match self {
            Marker::Cursor | Marker::None => if matches!(self, Marker::Cursor) { "> " } else { "  " },
            Marker::Active | Marker::CursorActive => "* ",
        }
    }
}

/// The complete overlay panel description.
#[derive(Debug, Clone, Default)]
pub struct RenderPlan {
    pub title: Option<String>,
    /// Rc-shared lines — zero-cost clone on cursor moves.
    pub lines: std::rc::Rc<Vec<RenderLine>>,
    /// Index of the cursor-selected logical option.
    pub selected: Option<usize>,
    /// Index of the current-state ("active") logical option (`*` marker,
    /// design §5.1). When it equals `selected`, `*` wins on the shared row.
    pub active: Option<usize>,
    /// Detail/banner lines: non-selectable header rows between
    /// the title and the candidate rows.
    pub detail: Vec<String>,
}

impl RenderPlan {
    /// Paint to ANSI text lines (matching picocode's `overlay_lines` shape:
    /// full-width padded rows, marker + content, per-line styles).
    pub fn paint(&self, term_width: usize) -> Vec<String> {
        let tw = term_width.max(1);
        let bg_line = |s: &str, bg: &str| format!("{}{:<width$}{}", bg, s, Style::RESET, width = tw);
        let mut out = Vec::new();
        if let Some(title) = &self.title {
            out.push(bg_line(title, Style::PANEL.bg));
        }
        // Detail/banner lines (design §5.1): non-selectable header
        // rows rendered between the title and the candidate rows —
        // `/replay` help text, `/stat` summary, etc.
        for d in &self.detail {
            out.push(bg_line(d, Style::HEADER.bg));
        }
        for l in self.lines.iter() {
            let sel = self.selected == Some(l.option);
            let act = self.active == Some(l.option);
            let bg = if l.is_header {
                Style::HEADER.bg
            } else if sel {
                Style::SELECTED.bg
            } else {
                Style::PANEL.bg
            };
            let fg = if sel { Style::SELECTED.fg } else { "" };
            // Marker slot: `>` cursor, `*` active; `*` wins when both.
            let marker = if !l.is_marker_row {
                "  "
            } else if l.is_header {
                "  "
            } else if act {
                Marker::Active.as_str()
            } else if sel {
                Marker::Cursor.as_str()
            } else {
                "  "
            };
            out.push(bg_line(&format!("{}{}{}", fg, marker, l.text), bg));
        }
        out
    }
}

/// Compose a [`RenderPlan`] from menu state parts.
/// PlanBuilder: composes a [`RenderPlan`] from menu state parts.
///
/// Detail lines are passed in at `build` time (design §5.1) so the
/// crate stays independent of host-provided text — the host resolves
/// the detail provider per frame and feeds the strings in.
pub struct PlanBuilder {
    pub title: String,
    pub child_header: Option<String>,
    pub row_header: Option<String>,
}

impl PlanBuilder {
    /// Build the plan for a menu level: title, optional child header
    /// (static `child_header`), optional per-row derived header (R8-4),
    /// then candidate rows with markers.
    /// `detail` are non-selectable header lines rendered between the
    /// title and the rows (design §5.1: `/replay` help, `/stat`
    /// summary, etc.). Empty ⇒ no detail section.
    pub fn build(
        &self,
        state: &crate::interpreter::MenuState,
        max_width: usize,
        detail: Vec<String>,
    ) -> RenderPlan {
        use crate::table::{Align, Table};

        let mut lines: Vec<RenderLine> = Vec::new();

        // (i) static child header (R8-4).
        if let Some(h) = &self.child_header {
            for sub in wrap_lines(h, max_width) {
                lines.push(RenderLine {
                    text: sub,
                    style: Style::HEADER,
                    is_header: true,
                    is_marker_row: false,
                    option: usize::MAX,
                });
            }
        }

        // (ii) per-row derived header for dynamic levels: the selected row's
        // description (or a child_description override, supplied via the
        // row's fields as `child_description`).
        if let Some(sel) = state.selected() {
            if state.drill_in {
                if let Some(cd) = sel
                    .field_value("child_description")
                    .and_then(|v| v.as_str())
                    .or(sel.description.as_deref())
                {
                    for sub in wrap_lines(cd, max_width) {
                        lines.push(RenderLine {
                            text: sub,
                            style: Style::HEADER,
                            is_header: true,
                            is_marker_row: false,
                            option: usize::MAX,
                        });
                    }
                }
            }
        }

        // Candidate rows via the table builder when fields are present,
        // otherwise a single column of ids+descriptions.
        let use_table = if !state.columns.is_empty() {
            true
        } else {
            state
                .candidates
                .first()
                .map(|r| !r.fields.is_empty())
                .unwrap_or(false)
        };
        let row_texts: Vec<String> = if use_table {
            // Declared columns (provider presentation hints) drive the
            // layout; fall back to the row's field order + an id "Model"
            // column when the provider declared none.
            let colnames: Vec<String> = if state.columns.is_empty() {
                let mut names = vec!["id".to_string()];
                names.extend(
                    state.candidates[0]
                        .fields
                        .iter()
                        .map(|(k, _)| k.clone()),
                );
                names
            } else {
                state
                    .columns
                    .iter()
                    .map(|c| {
                        if c.field.is_empty() {
                            "id".to_string()
                        } else {
                            c.field.clone()
                        }
                    })
                    .collect()
            };
            let headers: Vec<&str> = if state.columns.is_empty() {
                colnames.iter().map(String::as_str).collect()
            } else {
                state.columns.iter().map(|c| c.header.as_str()).collect()
            };
            let mut t = Table::new(&headers)
                .set_gap(2);
            if !state.columns.is_empty() {
                let aligns: Vec<Align> = state.columns.iter().map(|c| c.align).collect();
                let mins: Vec<usize> = state.columns.iter().map(|c| c.min_width).collect();
                let prios: Vec<usize> = state.columns.iter().map(|c| c.priority).collect();
                t = t.with_aligns(&aligns).with_min_widths(&mins).with_priorities(&prios);
            }
            for r in &state.candidates {
                let mut cells: Vec<String> = Vec::with_capacity(colnames.len());
                for k in &colnames {
                    // Display-string override wins; else render the value
                    // (design §8.1 "display string" — sort/filter still read
                    // the underlying value, not this).
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
            // Multi-line cells (long descriptions, model names) wrap; the
            // host renderer splits logical rows on `\n` so each candidate
            // stays one selectable option (matches v1's model table).
            t.render_multiline(max_width.saturating_sub(2))
        } else {
            state
                .candidates
                .iter()
                .map(|r| match &r.description {
                    Some(d) => format!("{} — {}", r.id, d),
                    None => r.id.clone(),
                })
                .collect()
        };

        // Logical options: one entry per candidate; multi-line rows from the
        // table split into visual lines sharing the option index.
        for (i, text) in row_texts.iter().enumerate() {
            for (j, sub) in text.split('\n').enumerate() {
                lines.push(RenderLine {
                    text: sub.to_string(),
                    style: Style::PANEL,
                    is_header: false,
                    is_marker_row: j == 0,
                    option: i,
                });
            }
        }

        // Wrap detail lines to the panel width (same as header rows) so
        // long help text breaks cleanly inside the panel.
        let detail = detail
            .iter()
            .flat_map(|d| wrap_lines(d, max_width))
            .collect::<Vec<_>>();

        RenderPlan {
            title: Some(self.title.clone()),
            lines: std::rc::Rc::new(lines),
            selected: if state.candidates.is_empty() {
                None
            } else {
                Some(state.active)
            },
            active: state.active_row_marker,
            detail,
        }
    }
}

fn wrap_lines(s: &str, width: usize) -> Vec<String> {
    crate::wrap::wrap(s, width.max(1))
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
    fn plan_has_title_header_and_rows() {
        let b = PlanBuilder {
            title: "Models".into(),
            child_header: Some("pick a model".into()),
            row_header: None,
        };
        let plan = b.build(&empty_state(), 80, Vec::new());
        assert_eq!(plan.title.as_deref(), Some("Models"));
        let headers = plan.lines.iter().filter(|l| l.is_header).count();
        assert_eq!(headers, 1);
        let rows = plan.lines.iter().filter(|l| !l.is_header).count();
        assert_eq!(rows, 2);
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
        // title + 2 rows (plus a leading marker column on each row)
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("T"));
        assert!(lines[1].contains("a"));
    }

    #[test]
    fn declared_columns_drive_layout() {
        use crate::provider::{ColumnDef, DataResult};
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
        // Two logical options; the active (current-state) row is `b`.
        assert_eq!(plan.selected, Some(0));
        assert_eq!(plan.active, Some(1));
        // Active precedence: `*` on the active row when the cursor is off it.
        let painted = plan.paint(60);
        let opt_b = painted.iter().find(|l| l.contains('b')).expect("row b present");
        assert!(opt_b.trim_start_matches("\x1b[48;5;239m").trim_start().starts_with('*') || opt_b.contains("* b"), "active row shows `*`: {opt_b:?}");
    }

    #[test]
    fn active_marker_beats_cursor_on_same_row() {
        let mut st = empty_state();
        st.candidates = vec![crate::fields::Row::new("x")];
        st.active = 0;
        st.active_row_marker = Some(0);
        let b = PlanBuilder { title: "T".into(), child_header: None, row_header: None };
        // selected == active → `*` wins (per §5.1).
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
        assert!(plan.lines.iter().any(|l| l.is_header && l.text.contains("long detail header")));
    }
}
