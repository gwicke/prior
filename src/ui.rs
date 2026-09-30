//! Confirmation dialog driver using the v2.5 RenderPlan renderer.
//!
//! This module provides a shared driver for standalone confirmation
//! dialogs (tool confirm, compaction review). It builds a RenderPlan from a
//! simple spec and draws it through the unified frame pipeline
//! (`doc/ui_draw_v2.md` §5.3): compose `[Divider?, Overlay]` → allocate →
//! render → `Output::draw_frame`, key input via crossterm's blocking
//! `event::read()` (compatible with the sync `Output::confirm` trait
//! method). The dialog has no input segment; on resolve its region is
//! erased through the same pipeline and the decision (if the spec asks for
//! one) is written where the dialog was.

pub mod frame;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use crate::output::{Confirmation, Output};
use crate::slashdot_menu::node_impl::StaticRow;
use crate::slashdot_menu::render::{paint_overlay, LogicalLine, RenderPlan};
use crate::ui::frame::{ApplyPolicy, Element, Frame};
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
    /// Draw a dim divider above the dialog: mid-turn confirmations want a
    /// separation from the streamed output above them. Dialogs drawn inside
    /// the REPL pass `false` — scrollback already carries a separator above
    /// the input region (and none is re-drawn on commit either).
    pub divider: bool,
    /// Subject of the decision line recorded on resolve
    /// (`accepted <subject>` / `denied <subject>`), or `None` to record
    /// nothing: the compaction review records nothing because the committed
    /// `/tree … compact` prompt is already self-documenting (plan §5.3).
    pub subject: Option<String>,
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

    // Pause the background reader; `resolve()` releases it on every exit
    // from the key loop (plan §9 latent issue 1: a gate left raised freezes
    // every later in-flight reader).
    if let Some(gate) = out.confirm_gate() {
        gate.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    // Re-anchor (DSR) so the dialog starts at the real cursor row — right below
    // the output above it — and scrolls itself into the live region if it does
    // not fit (plan §4.2/§5.3).
    //
    // The comment here used to say "mid-turn the draw cache's caret does not
    // follow streamed content row by row". That stopped being true when the
    // streaming path started counting from the bytes it emits; the streamed
    // answer and streamed thinking are both checked against a vt100 replay. The
    // DSR is kept as the cheap confirmation on a path where a wrong anchor
    // strands the dialog over the output — the confirm tests assert *behaviour*
    // (which option was chosen) and not the dialog's row, so a regression here
    // would not be caught by them.
    let _ = out.cursor_row();

    let plan = build_confirm_plan(spec);
    // Initial size from the output's shared source (ui_draw_v2 §9 #3); every
    // apply below gets this same pair (Resize replaces it and re-composes).
    let mut size = out.frame_size();

    // Initial render.
    let mut sel: usize = spec.default_sel;
    draw_dialog(out, spec, &plan, sel, size);

    loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                if sel > 0 {
                    sel -= 1;
                    draw_dialog(out, spec, &plan, sel, size);
                }
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                if sel + 1 < spec.options.len() {
                    sel += 1;
                    draw_dialog(out, spec, &plan, sel, size);
                }
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                if let Some(idx) = quick_key_index(spec, c) {
                    if idx < spec.options.len() {
                        return resolve(out, spec, size, ConfirmResult::Selected(idx));
                    }
                }
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                return resolve(out, spec, size, ConfirmResult::Selected(sel));
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                return resolve(out, spec, size, ConfirmResult::Dismissed);
            }
            Ok(Event::Resize(w, h)) => {
                // Re-compose for the new size; the cached size mismatch
                // forces a full repaint (no DSR: the dialog keeps its place).
                size = (w.max(1) as usize, h.max(1) as usize);
                draw_dialog(out, spec, &plan, sel, size);
            }
            _ => {}
        }
    }
}

/// Compose `[Divider?, Overlay]` and apply it as a frame: the first draw
/// anchors at the cursor row, every later one at the live frame's top
/// (an intact live frame → partial row-diff). Re-composed per key and per
/// resize — cheap, and the one place the dialog's bytes come from.
fn draw_dialog(
    out: &dyn Output,
    spec: &ConfirmSpec<'_>,
    plan: &RenderPlan,
    sel: usize,
    size: (usize, usize),
) {
    let (term_w, term_h) = size;
    // The painter reports the geometry it produced: `rows` are the terminal
    // rows (each already a full-width screen row) and `focus` is where the
    // selected option landed among them. Neither is re-derived here — the
    // old `> `-marker sniff broke if a header line began with `> `, and the
    // REPL's own arithmetic missed table headers and multi-row options
    // (ui_draw_v2 §9 #5). `focus` is only consulted when the frame budget
    // squeezes the overlay below its painted height.
    let painted = paint_overlay(&update_plan_sel(plan, sel), term_w, term_h);
    let mut elements = Vec::new();
    if spec.divider {
        elements.push(Element::Divider);
    }
    elements.push(Element::Overlay {
        rows: painted.rows,
        focus: painted.focus.unwrap_or(0),
    });
    // Through the tail, not a second pipeline. This builder used to hand-roll
    // `extent → allocate → render` and call `draw_frame`, which meant two places
    // in the crate decided how the terminal's rows were divided (ui_draw_v2 §5.1.1).
    out.draw_elements(elements, ApplyPolicy::Frame, size);
}

/// Resolve the dialog: erase its region through the frame pipeline — a
/// Frame apply's union clear takes care of the rows the (taller) dialog
/// painted, so nothing needs a second clear, and the bottom space stays
/// clean for whatever the caller appends next (plan §4.3/§5.3). The clear
/// parks the cursor on the region's first row, so the optional decision
/// line lands exactly where the dialog was.
fn resolve(
    out: &dyn Output,
    spec: &ConfirmSpec<'_>,
    size: (usize, usize),
    result: ConfirmResult,
) -> ConfirmResult {
    // Release the in-flight reader gate raised on entry: the dialog is done
    // with stdin, and a gate left raised would leave every later in-flight
    // reader born paused — Esc/Ctrl-C dead for the rest of the session
    // (plan §9 latent issue 1). Idempotent for dialogs that never paused
    // anything (non-interactive outputs return before this point anyway).
    if let Some(gate) = out.confirm_gate() {
        gate.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    out.draw_frame(&Frame::empty(), ApplyPolicy::Frame, size);
    if let Some(subject) = &spec.subject {
        // `yes` / `always` / `accept` record; `no` / `cancel` and a dismiss
        // record the refusal.
        let accepted = match &result {
            ConfirmResult::Selected(i) => !matches!(
                spec.options.get(*i).map(|o| o.id.as_str()),
                Some("no") | Some("cancel")
            ),
            ConfirmResult::Dismissed => false,
        };
        out.display_plain(&format!(
            "{} {subject}",
            if accepted { "accepted" } else { "denied" }
        ));
    }
    result
}

/// Subject of a tool-confirmation decision line: the caller's message
/// follows the `Confirm tool {NAME} call?` convention (the confirm gate in
/// `agent.rs`) → `tool {NAME}`. Surrounding convention furniture is stripped
/// independently; anything left over passes through unchanged.
pub(crate) fn confirm_subject(header: &str) -> String {
    let s = header.strip_prefix("Confirm ").unwrap_or(header);
    s.strip_suffix(" call?").unwrap_or(s).to_string()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::NoOutput;
    use std::sync::Mutex;

    #[test]
    fn confirm_subject_follows_the_tool_confirm_convention() {
        assert_eq!(confirm_subject("Confirm tool BASH call?"), "tool BASH");
        assert_eq!(confirm_subject("Confirm tool EDIT call?"), "tool EDIT");
    }

    #[test]
    fn confirm_subject_strips_convention_furniture_independently() {
        // Suffix only (the probe's message), prefix only, neither.
        assert_eq!(confirm_subject("tool BASH call?"), "tool BASH");
        assert_eq!(confirm_subject("Confirm tool BASH"), "tool BASH");
        assert_eq!(confirm_subject("something else"), "something else");
    }

    // --- composed dims reach the apply (ui_draw_v2 §9 #3) ---

    /// Records the dims it was asked to apply; everything else no-ops.
    struct SizeRecorder(Mutex<Option<(usize, usize)>>);

    impl Output for SizeRecorder {
        fn draw_frame(
            &self,
            _frame: &Frame,
            _policy: ApplyPolicy,
            dims: (usize, usize),
        ) -> (usize, usize) {
            *self.0.lock().unwrap() = Some(dims);
            (dims.0.max(1), dims.1.max(1))
        }
        fn display_text(&self, _text: &str) {}
        fn display_tool_call(&self, _name: &str, _args: &serde_json::Value) {}
        fn display_tool_result(&self, _result: &str) {}
        fn get_user_input(&self) -> String {
            String::new()
        }
        fn display_error(&self, _error: &str) {}
        fn display_system(&self, _text: &str) {}
        fn confirm(&self, _message: &str) -> Confirmation {
            Confirmation::No
        }
        fn display_separator(&self) {}
        fn display_header(
            &self,
            _provider: &str,
            _model: &str,
            _yolo: bool,
            _limit: usize,
            _persona: Option<&str>,
        ) {
        }
    }

    fn spec<'a>(opts: &'a [StaticRow]) -> ConfirmSpec<'a> {
        ConfirmSpec {
            header: "Confirm?",
            options: opts,
            quick_keys: None,
            default_sel: 0,
            divider: false,
            subject: None,
        }
    }

    fn recorder() -> SizeRecorder {
        SizeRecorder(Mutex::new(None))
    }

    #[test]
    fn draw_dialog_applies_at_the_dims_it_composed_for() {
        let opts = [StaticRow::new("yes", "do it")];
        let s = spec(&opts);
        let plan = build_confirm_plan(&s);
        let rec = recorder();
        draw_dialog(&rec, &s, &plan, 0, (42, 11));
        assert_eq!(*rec.0.lock().unwrap(), Some((42, 11)));
    }

    #[test]
    fn resolve_clears_at_the_dims_the_dialog_used() {
        let opts = [StaticRow::new("yes", "do it")];
        let s = spec(&opts);
        let rec = recorder();
        resolve(&rec, &s, (42, 11), ConfirmResult::Dismissed);
        assert_eq!(*rec.0.lock().unwrap(), Some((42, 11)));
    }

    #[test]
    fn default_draw_frame_reports_clamped_dims() {
        // `NoOutput` doesn't override `draw_frame`, so this exercises the
        // trait default: it applies nothing but still reports the clamped
        // dims it was asked to apply.
        assert_eq!(
            NoOutput.draw_frame(&Frame::empty(), ApplyPolicy::Frame, (0, 7)),
            (1, 7)
        );
    }
}
