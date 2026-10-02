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
use crate::slashdot_menu::render::{
    paint_detail, paint_header, paint_rows, visual_height, window_heights, LogicalLine,
    PaintedSection, RenderPlan,
};
use crate::ui::frame::{pad_row, ApplyPolicy, Element, Frame, RenderAt, Rendered};
// Only the element tests drive the pipeline themselves; naming these
// unconditionally would trip `unused_imports` on a normal build.
#[cfg(test)]
use crate::ui::frame::{allocate, live_budget, render, Extent};
use std::sync::Arc;

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
    draw_dialog(out, &plan, sel, size);

    loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                if sel > 0 {
                    sel -= 1;
                    draw_dialog(out, &plan, sel, size);
                }
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::NONE,
                ..
            })) => {
                if sel + 1 < spec.options.len() {
                    sel += 1;
                    draw_dialog(out, &plan, sel, size);
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
                draw_dialog(out, &plan, sel, size);
            }
            _ => {}
        }
    }
}

// --- the overlay element ----------------------------------------------------
//
// The pop-up's own element, carrying the pop-up's own squeeze policy: half the
// allocation above the selection is the *menu's* preference about how much
// context to show. It used to live in `frame.rs` as `render`'s `h / 2` — where
// the frame layer had no business holding an opinion about what a menu wants
// (ui_draw_v2 §3.2.2).
//
// It is here rather than in the menu crate because both this module and
// `repl.rs` build overlays, and neither the menu crate nor `frame.rs` sits
// below both of them.
//
// An overlay is **three elements**, not one: the node's `header()` detail, the
// table's column-label line, and the scrollable rows. Three sections with
// three different jobs, and one element each is what lets `allocate` budget
// them — so the header is a header rather than the first row of something
// scrollable. Painted sections come from `slashdot_menu::render`'s
// `paint_detail` / `paint_header`; the rows stay unpainted and are windowed at
// render time.

impl Element {
    /// The table's column-label line. **Elastic** (`must` 0), deliberately:
    /// protecting it inverts the right priority when space runs out. Measured
    /// against `allocate`, a budget of 3 with a protected header gives the
    /// header its row and the menu *zero* rows — and given the choice, a row
    /// beats a header. So the header yields; see `header_element_extent`.
    fn table_header(section: PaintedSection) -> Self {
        Element::new(0, section.height(), SectionItem { section })
    }

    /// The node's `header()` output: context, above the table. Also elastic,
    /// for the same reason — detail is the first thing to lose to a visible
    /// row, and the last thing worth keeping when the rows have to be clipped.
    ///
    /// Within its allocation it keeps **whole logical lines**, dropping from
    /// the bottom: a detail line too wide for the terminal was split when
    /// painted, and it is kept or dropped whole rather than cut mid-line.
    fn detail(section: PaintedSection) -> Self {
        Element::new(0, section.height(), SectionItem { section })
    }

    /// The scrollable rows, held **unpainted** and windowed at render time.
    ///
    /// `selected` is the cursor's *logical* row index — it is what this element
    /// windows around, so there is no painted-index arithmetic anywhere: the
    /// selection is identified exactly the way the engine identified it, and
    /// nothing painted above it can move it.
    ///
    /// Holding the `Arc` rather than painted `String`s is what makes painting
    /// O(window) rather than O(table): a 1000-row menu paints the ~14 rows
    /// actually on screen instead of all 1000 (measured 2.5ms → 19µs). The rows
    /// are already `Arc`-cached by the engine, so this shares the pointer and
    /// repaints nothing a cursor move did not change.
    fn data_rows(rows: Arc<Vec<LogicalLine>>, selected: Option<usize>, active: Vec<usize>) -> Self {
        // `might` is the height *if every row were shown*, measured the way the
        // engine wrote them ([`visual_height`]: embedded `\n` only). Painting
        // can make a row taller — one too wide for the terminal is split — so
        // this is a lower bound on the painted height, which is the safe
        // direction: the allocator is never promised more rows than the element
        // can actually produce. `render_at` re-measures and fits `h` anyway.
        let might: usize = rows.iter().map(visual_height).sum();
        let must = usize::from(might > 0);
        Element::new(
            must,
            might,
            RowsItem {
                rows,
                selected,
                active,
            },
        )
    }
}

/// One elastic section: already-painted rows, clipped to the allocation.
///
/// The shape both `detail` and `table_header` want, and deliberately *not*
/// `frame::Element::lines`: that one has `must == might`, which is what keeps
/// the status row from ever competing. These two must compete — they yield.
struct SectionItem {
    section: PaintedSection,
}

impl RenderAt for SectionItem {
    fn render_at(&self, h: usize, tw: usize) -> Rendered {
        // Whole logical lines only: a line's painted sub-rows are kept or
        // dropped together, so the cut never lands inside one.
        let mut used = 0usize;
        let mut end = 0usize;
        for &height in &self.section.heights {
            if used + height > h {
                break;
            }
            used += height;
            end += height;
        }
        let rows = self.section.rows[..end]
            .iter()
            .map(|r| pad_row(r, tw))
            .collect();
        Rendered { rows, caret: None }
    }
}

/// `window_heights`, with the one rule it does not have: **the focus is always
/// inside the window**.
///
/// `window_heights` extends backward to fill a window that would otherwise
/// stop short, which is right for filling space and wrong here — when the
/// selection is *taller than the whole allocation*, that backward step pushes
/// the selection out of its own window, and the one row the reader needs is
/// the row that goes missing. Falling back to the focus alone keeps the cursor
/// on screen; the caller clips it to `h`.
fn window_keeps(heights: &[usize], focus: usize, budget: usize) -> (usize, usize) {
    let (start, count) = window_heights(heights, focus, budget);
    if heights.is_empty() || (start..start + count).contains(&focus) {
        (start, count)
    } else {
        (focus, 1)
    }
}

/// The scrollable rows: held unpainted, windowed around the selection at
/// render time.
struct RowsItem {
    rows: Arc<Vec<LogicalLine>>,
    selected: Option<usize>,
    active: Vec<usize>,
}

impl RenderAt for RowsItem {
    fn render_at(&self, h: usize, tw: usize) -> Rendered {
        if h == 0 || self.rows.is_empty() {
            return Rendered::default();
        }
        let selected = self.selected.filter(|s| *s < self.rows.len());
        // Window on the heights the engine wrote (embedded `\n`), paint only
        // that window, then re-window on the *painted* heights: painting can
        // make a row taller than it was written, and the window has to fit `h`
        // in the unit the frame counts in.
        let engine_heights: Vec<usize> = self.rows.iter().map(visual_height).collect();
        let (start, count) = window_keeps(&engine_heights, selected.unwrap_or(0), h);
        let count = count.min(self.rows.len().saturating_sub(start));
        if count == 0 {
            return Rendered::default();
        }
        let in_window = |i: &usize| (start..start + count).contains(i);
        let section = paint_rows(
            &self.rows[start..start + count],
            selected.filter(in_window).map(|s| s - start),
            &self
                .active
                .iter()
                .filter(|i| in_window(i))
                .copied()
                .collect::<Vec<_>>(),
            tw,
        );
        // The selection's index *within the painted window* — the thing this
        // element windows around. Nothing above it can move it, which is the
        // whole point: there is no global painted index to keep in sync.
        let focus = selected
            .map(|s| s - start)
            .filter(|f| *f < section.heights.len())
            .unwrap_or(0);
        let (win_start, win_count) = window_keeps(&section.heights, focus, h);
        // `take(h)` is the contract: `render_at` fits its allocation, even when
        // the selected row is taller than the whole thing — the marker is on
        // its first row, so the cursor is still visible.
        let rows = section
            .slice(win_start, win_count)
            .iter()
            .take(h)
            .map(|r| pad_row(r, tw))
            .collect();
        Rendered { rows, caret: None }
    }
}

/// The frame elements a menu overlay composes into, in paint order: the node's
/// detail, the table header, the scrollable rows.
///
/// One builder for both hosts (`repl.rs`'s slash menu and this module's
/// dialogs) so the two cannot drift on which sections exist, in what order, or
/// with what paint. Empty sections are dropped rather than contributed as
/// zero-height elements: a plan with no columns has no header, and an empty
/// element would still ask the allocator for a divider.
///
/// `selected` is passed separately from `plan.selected` so a dialog with its
/// own cursor (`draw_dialog`'s `sel`) does not have to clone the plan to move
/// it — cheap now that `rows` is an `Arc`, but clearer not to.
pub(crate) fn overlay_elements(
    plan: &RenderPlan,
    term_w: usize,
    selected: Option<usize>,
) -> Vec<Element> {
    let mut els = Vec::with_capacity(3);
    let detail = paint_detail(&plan.detail, term_w);
    if !detail.rows.is_empty() {
        els.push(Element::detail(detail));
    }
    let header = paint_header(plan.table_header.as_ref(), term_w);
    if !header.rows.is_empty() {
        els.push(Element::table_header(header));
    }
    if !plan.rows.is_empty() {
        els.push(Element::data_rows(
            Arc::clone(&plan.rows),
            selected.filter(|s| *s < plan.rows.len()),
            plan.active_rows.clone(),
        ));
    }
    els
}

/// Compose `[Divider?, Overlay]` and apply it as a frame: the first draw
/// anchors at the cursor row, every later one at the live frame's top
/// (an intact live frame → partial row-diff). Re-composed per key and per
/// resize — cheap, and the one place the dialog's bytes come from.
fn draw_dialog(out: &dyn Output, plan: &RenderPlan, sel: usize, size: (usize, usize)) {
    let (term_w, _) = size;
    // The dialog's cursor is its own (`sel`), so the selection is handed over
    // rather than written into a clone of the plan.
    let overlay = overlay_elements(plan, term_w, Some(sel));
    // Through the tail, not a second pipeline. This builder used to hand-roll
    // `extent → allocate → render` and call `draw_frame`, which meant two places
    // in the crate decided how the terminal's rows were divided (ui_draw_v2 §5.1.1).
    //
    // Pieces in, nothing back: the composer owns the divider rule ("a menu
    // brings a separator" — an overlay is present, so one appears) and the
    // status row. `ConfirmSpec.divider` is what this used to consult; the rule
    // is now the composer's, and the field is left unread.
    out.draw_elements(None, overlay, ApplyPolicy::Frame, size);
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

    let rows: Arc<Vec<LogicalLine>> = Arc::new(
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
        draw_dialog(&rec, &plan, 0, (42, 11));
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
    // --- the three overlay elements ----------------------------------------

    /// The header is a *section*, not the first row of a scrollable block: it
    /// stays put while the rows under it move.
    ///
    /// This is the regression `c3e4567` introduced. The overlay was one flat
    /// element, so the frame's second window — over the already-painted rows —
    /// treated the header as scrollable and cut it away, along with the detail
    /// line. The painter budgets against `ceil(2/3 * term_h)` while `allocate`
    /// shares that budget with the input and divider, so any table longer than
    /// about a dozen rows lost its header; only short menus kept it.
    #[test]
    fn table_header_survives_a_scrolling_row_section() {
        let els = vec![
            Element::detail(paint_detail(&[LogicalLine::new("pick one")], 60)),
            Element::table_header(paint_header(Some(&LogicalLine::new("MODEL")), 60)),
            Element::data_rows(
                Arc::new(
                    (0..30)
                        .map(|i| LogicalLine::new(format!("model-{i}")))
                        .collect(),
                ),
                Some(25),
                vec![],
            ),
        ];
        let extents: Vec<Extent> = els.iter().map(Element::extent).collect();
        let heights = allocate(&extents, live_budget(24));
        let f = render(&els, &heights, 60);

        // The cursor is deep in the list, so the frame really did window.
        assert!(f.height() < 30, "the rows were windowed: {}", f.height());
        assert!(
            f.row(0).trim_end().contains("pick one"),
            "the detail section is fixed too: {:?}",
            f.row(0)
        );
        assert!(
            f.row(1).trim_end().contains("MODEL"),
            "the header must not scroll with the rows; row 1 was {:?}",
            f.row(1)
        );
    }

    /// Under *real* pressure the header yields to a visible row. Measured
    /// against `allocate`: with a protected header, a budget of 3 gives the
    /// header its row and the menu zero rows — an inversion. The header is
    /// elastic precisely so that cannot happen.
    #[test]
    fn header_element_extent_yields_to_a_row_under_starvation() {
        let extents = |h_must, h_might| {
            vec![
                Extent { must: 1, might: 1 }, // input
                Extent { must: 1, might: 1 }, // divider
                Extent { must: 0, might: 5 }, // detail
                Extent {
                    must: h_must,
                    might: h_might,
                },
                Extent { must: 1, might: 20 }, // rows
            ]
        };
        // A protected header wins the tie-break and starves the rows.
        let protected = allocate(&extents(1, 1), 3);
        assert_eq!(protected[4], 0, "a protected header wins the last row");
        // The elastic one does not.
        let elastic = allocate(&extents(0, 1), 3);
        assert_eq!(elastic[4], 1, "a row beats a header");
        assert_eq!(elastic[3], 0, "the header yielded");
        // And with room to spare both behave the same — this is only about
        // starvation, not about hiding the header in normal use.
        for h in [5usize, 8, 16] {
            assert_eq!(allocate(&extents(0, 1), h)[3], 1, "budget {h}");
        }
    }

    /// A painted section row is exactly `term_w` columns wide, with the padding
    /// inside the styled span so a menu background reaches the right edge.
    #[test]
    fn section_rows_fill_the_width_inside_the_styled_span() {
        for (label, tw) in [("ab", 8usize), ("abcdefgh", 8), ("ab", 5)] {
            let el = Element::table_header(paint_header(Some(&LogicalLine::new(label)), tw));
            // A label wider than the terminal is *split*, so the section can be
            // two rows; allocate what it actually wants.
            let h = el.extent().might;
            let f = render(&[el], &[h], tw);
            assert_eq!(f.height(), h, "{label:?} at {tw}");
            for i in 0..f.height() {
                assert_eq!(
                    crate::slashdot_menu::table::display_width(f.row(i)),
                    tw,
                    "{label:?} at {tw} row {i} should fill the width: {:?}",
                    f.row(i)
                );
                // The reset is last, so the padding is inside the styled span.
                assert!(f.row(i).ends_with("\x1b[0m"), "{:?}", f.row(i));
            }
        }
    }

    /// The rows element windows around the cursor and reports the selection in
    /// the window — the job the old single overlay element did, now against
    /// unpainted rows.
    #[test]
    fn rows_element_windows_around_the_cursor() {
        let rows: Arc<Vec<LogicalLine>> = Arc::new(
            (0..20)
                .map(|i| LogicalLine::new(format!("item {i}")))
                .collect(),
        );
        let el = Element::data_rows(rows, Some(10), vec![]);
        let f = render(&[el], &[4], 20);
        assert_eq!(f.height(), 4);
        // `window_heights`' rule: walk back over at most half the budget, then
        // fill forward. Cursor 10, budget 4 → half 2 → start 9, four rows.
        assert!(f.row(0).contains("item 9"), "{:?}", f.row(0));
        assert!(f.row(3).contains("item 12"), "{:?}", f.row(3));
        assert!(f.row(1).contains("> item 10"), "cursor row: {:?}", f.row(1));
    }

    /// Moving the cursor moves the window and keeps the selection visible —
    /// the same `Rc` of rows, repainted only for the new window.
    #[test]
    fn rows_element_follows_the_cursor_without_moving_the_header() {
        let rows: Arc<Vec<LogicalLine>> = Arc::new(
            (0..30)
                .map(|i| LogicalLine::new(format!("item {i}")))
                .collect(),
        );
        let els = |sel| {
            vec![
                Element::table_header(paint_header(Some(&LogicalLine::new("HDR")), 20)),
                Element::data_rows(Arc::clone(&rows), Some(sel), vec![]),
            ]
        };
        for sel in [0usize, 15, 29] {
            let els = els(sel);
            let extents: Vec<Extent> = els.iter().map(Element::extent).collect();
            let heights = allocate(&extents, live_budget(24));
            let f = render(&els, &heights, 20);
            assert!(
                f.row(0).trim_end().contains("HDR"),
                "header fixed at cursor {sel}: {:?}",
                f.row(0)
            );
            let marked = (0..f.height())
                .find(|i| f.row(*i).contains('>'))
                .unwrap_or_else(|| panic!("no cursor row painted at {sel}"));
            assert!(f.row(marked).contains(&format!("item {sel}")), "{sel}");
        }
    }

    /// The rows element honours its contract even when a single row is taller
    /// than the whole allocation: it clips rather than overflowing.
    #[test]
    fn rows_element_fits_a_selection_taller_than_its_budget() {
        let rows: Arc<Vec<LogicalLine>> = Arc::new(vec![
            LogicalLine::new("short"),
            LogicalLine::new("tall\nrow\nof\nlines"),
        ]);
        let el = Element::data_rows(rows, Some(1), vec![]);
        let f = render(&[el], &[2], 20);
        assert!(f.height() <= 2, "never more than h: {}", f.height());
        assert!(f.row(0).contains("tall"), "{:?}", f.row(0));
    }

    /// An empty plan contributes no overlay elements at all, so the frame does
    /// not draw a divider for a menu that is not there.
    #[test]
    fn an_empty_plan_is_no_elements() {
        assert!(overlay_elements(&RenderPlan::default(), 60, None).is_empty());
    }
}
