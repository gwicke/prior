//! Confirmation dialog driver using the v2.5 RenderPlan renderer.
//!
//! This module provides a shared driver for standalone confirmation
//! dialogs (tool confirm, compaction review). It builds a RenderPlan from a
//! simple spec, renders it via the crate's `paint_plan`, and handles key
//! input via crossterm's blocking `event::read()` (compatible with the
//! sync `Output::confirm` trait method).

use std::sync::Arc;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use crate::output::{Confirmation, Output};
use crate::slashdot_menu::node_impl::StaticRow;
use crate::slashdot_menu::render::{paint_plan, LogicalLine, RenderPlan};
use std::rc::Rc;

/// Specification for a confirmation dialog.
pub struct ConfirmSpec<'a> {
    /// Header text shown above the options (can be multi-line).
    pub header: &'a str,
    /// Options as StaticRow (id + description).
    pub options: &'a [StaticRow],
    /// Optional quick-key overrides: `(char, option_index)`.
    /// If None, derives from first letter of each id.
    pub quick_keys: Option<&'a [(char, usize)]>,
    /// Initial cursor position (default selection).
    pub default_sel: usize,
}

/// Result of a confirmation dialog.
pub enum ConfirmResult {
    /// User selected an option (index into spec.options).
    Selected(usize),
    /// User dismissed with Esc.
    Dismissed,
}

/// Run a confirmation dialog (synchronous, blocking).
///
/// Returns `ConfirmResult::Selected(idx)` on Enter/quick-key, or
/// `ConfirmResult::Dismissed` on Esc.
///
/// For non-interactive outputs (where `confirm_gate()` is `None`), returns
/// `Selected(out.confirm_default(header))` immediately without rendering.
pub fn confirm(out: &dyn Output, spec: &ConfirmSpec<'_>) -> ConfirmResult {
    // Non-interactive: delegate to trait's sync policy.
    if out.confirm_gate().is_none() {
        let c = out.confirm_default(spec.header);
        let idx = spec
            .options
            .iter()
            .position(|o| match c {
                Confirmation::Yes => o.id == "yes",
                Confirmation::No => o.id == "no",
                Confirmation::Always => o.id == "always",
            })
            .unwrap_or(0);
        return ConfirmResult::Selected(idx);
    }

    // Build the RenderPlan.
    let plan = build_confirm_plan(spec);
    let (term_w, term_h) = crossterm::terminal::size().unwrap_or((80, 24));
    let term_w = term_w as usize;
    let term_h = term_h as usize;

    // Render the plan to get the visual line count for positioning.
    let painted = paint_plan(&plan, term_w, term_h);
    let line_count = painted.lines().count();

    // Position anchor: just below current cursor, clamped so it never scrolls.
    let start_row = out.cursor_row().min(term_h);
    let top = start_row
        .saturating_sub(line_count + 1)
        .min(term_h.saturating_sub(line_count + 1));

    // Pause the background reader.
    if let Some(gate) = out.confirm_gate() {
        gate.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    out.stop_thinking();

    // Initial render.
    render_at(out, top, &painted);

    let mut sel: usize = spec.default_sel;

    loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                if sel > 0 {
                    sel -= 1;
                    let painted = paint_plan(&update_plan_sel(&plan, sel), term_w, term_h);
                    render_at(out, top, &painted);
                }
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                if sel + 1 < spec.options.len() {
                    sel += 1;
                    let painted = paint_plan(&update_plan_sel(&plan, sel), term_w, term_h);
                    render_at(out, top, &painted);
                }
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                if let Some(idx) = quick_key_index(spec, c) {
                    if idx < spec.options.len() {
                        return ConfirmResult::Selected(idx);
                    }
                }
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                return ConfirmResult::Selected(sel);
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                return ConfirmResult::Dismissed;
            }
            Ok(Event::Resize(w, h)) => {
                let new_term_w = w as usize;
                let new_term_h = h as usize;
                let painted = paint_plan(&plan, new_term_w, new_term_h);
                render_at(out, top, &painted);
            }
            _ => {}
        }
    }
}

/// Build a RenderPlan from a ConfirmSpec.
fn build_confirm_plan(spec: &ConfirmSpec<'_>) -> RenderPlan {
    let detail = spec
        .header
        .split('\n')
        .map(LogicalLine::new)
        .collect::<Vec<_>>();

    let rows: Rc<Vec<LogicalLine>> = Rc::new(
        spec.options
            .iter()
            .map(|r| LogicalLine::new(format!("{}  {}", r.id, r.desc)))
            .collect(),
    );

    RenderPlan {
        table_header: None,
        detail,
        rows,
        selected: Some(spec.default_sel),
        active_rows: vec![],
    }
}

/// Create a new RenderPlan with updated selection.
fn update_plan_sel(plan: &RenderPlan, sel: usize) -> RenderPlan {
    RenderPlan {
        selected: Some(sel),
        ..plan.clone()
    }
}

/// Resolve quick-key char to option index.
fn quick_key_index(spec: &ConfirmSpec<'_>, c: char) -> Option<usize> {
    let c = c.to_ascii_lowercase();
    if let Some(keys) = spec.quick_keys {
        keys.iter()
            .find(|(k, _)| k.to_ascii_lowercase() == c)
            .map(|(_, i)| *i)
    } else {
        spec.options
            .iter()
            .position(|o| o.id.chars().next().map(|fc| fc.to_ascii_lowercase()) == Some(c))
    }
}

/// Render the painted plan at the given top row.
fn render_at(out: &dyn Output, top: usize, painted: &str) {
    use std::io::{self, Write};
    let mut s = String::new();
    s.push_str(&format!("\x1b[{};1H", top));
    s.push_str("\x1b[0J");
    for line in painted.lines() {
        s.push_str(line);
        s.push('\n');
    }
    let _ = io::stdout().write_all(s.as_bytes());
    let _ = io::stdout().flush();
}
