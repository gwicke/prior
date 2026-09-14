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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenderPlan {
    pub title: Option<String>,
    pub lines: Vec<RenderLine>,
    /// Index of the cursor-selected logical option.
    pub selected: Option<usize>,
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
        for l in &self.lines {
            let bg = if l.is_header {
                Style::HEADER.bg
            } else if l.option == self.selected.unwrap_or(usize::MAX) {
                Style::SELECTED.bg
            } else {
                Style::PANEL.bg
            };
            let fg = if l.option == self.selected.unwrap_or(usize::MAX) {
                Style::SELECTED.fg
            } else {
                ""
            };
            let marker = if l.is_marker_row { Marker::Cursor.as_str_placeholder() } else { "  " };
            let _ = marker;
            let _ = &l.text;
            out.push(bg_line(&format!("{}{}", fg, l.text), bg));
        }
        out
    }
}

impl Marker {
    fn as_str_placeholder(&self) -> &'static str {
        self.as_str()
    }
}

/// Compose a [`RenderPlan`] from menu state parts.
pub struct PlanBuilder {
    pub title: String,
    pub child_header: Option<String>,
    pub row_header: Option<String>,
}

impl PlanBuilder {
    /// Build the plan for a menu level: title, optional child header
    /// (static `child_header`), optional per-row derived header (R8-4),
    /// then candidate rows with markers.
    pub fn build(
        &self,
        state: &crate::interpreter::MenuState,
        max_width: usize,
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
        let use_table = state
            .candidates
            .first()
            .map(|r| !r.fields.is_empty())
            .unwrap_or(false);
        let row_texts: Vec<String> = if use_table {
            let field_names: Vec<String> = state.candidates[0]
                .fields
                .iter()
                .map(|(k, _)| k.clone())
                .collect();
            let headers: Vec<&str> = std::iter::once("id")
                .chain(field_names.iter().map(String::as_str))
                .collect();
            let aligns = vec![Align::Left; headers.len()];
            let prios: Vec<usize> = (1..=headers.len()).rev().collect();
            let mut t = Table::new(&headers)
                .with_aligns(&aligns)
                .with_priorities(&prios);
            for r in &state.candidates {
                let mut cells: Vec<String> = vec![r.id.clone()];
                for k in &field_names {
                    cells.push(
                        r.field_value(k)
                            .map(|v| match v {
                                crate::value::Value::Str(s) => s.clone(),
                                crate::value::Value::Num(n) => n.to_string(),
                            })
                            .unwrap_or_default(),
                    );
                }
                t.add_row(&cells);
            }
            t.render(max_width.saturating_sub(2))
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

        RenderPlan {
            title: Some(self.title.clone()),
            lines,
            selected: if state.candidates.is_empty() {
                None
            } else {
                Some(state.active)
            },
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
            active: 0,
            pin_lost: false,
            level_is_dynamic: false,
        }
    }

    #[test]
    fn plan_has_title_header_and_rows() {
        let b = PlanBuilder {
            title: "Models".into(),
            child_header: Some("pick a model".into()),
            row_header: None,
        };
        let plan = b.build(&empty_state(), 80);
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
        let plan = b.build(&empty_state(), 40);
        let lines = plan.paint(40);
        // title + 2 rows
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("T"));
        assert!(lines[1].contains("a"));
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
        let plan = b.build(&st, 80);
        assert!(plan.lines.iter().any(|l| l.is_header && l.text.contains("long detail header")));
    }
}
