//! The overlay elements a menu — or a confirmation — is built from, and the
//! frame pipeline they render through.
//!
//! This module used to hold a standalone confirmation *driver* as well: a
//! blocking `event::read()` loop with its own entry DSR, its own reader gate
//! and its own `Frame::empty()` erase. All of that was overhead rather than
//! drawing, and it existed only because a gated tool call runs inline inside
//! the turn's `select!` and could not let the loop run. It no longer blocks, so
//! the dialog is session state owned by the REPL and composed by the very same
//! [`overlay_elements`] the slash menu uses
//! ([`doc/confirm_as_menu_261002.md`](../../doc/confirm_as_menu_261002.md)).

pub mod frame;

use crate::slashdot_menu::render::{
    paint_detail, paint_header, paint_rows, visual_height, window_heights, LogicalLine,
    PaintedSection, RenderPlan,
};
use crate::ui::frame::{pad_row, Element, RenderAt, Rendered};
// The apply vocabulary is the tests' — they drive the pipeline directly rather
// than through `Output::draw_elements`, which is the only production caller.
#[cfg(test)]
use crate::ui::frame::ApplyPolicy;
// Only the element tests drive the pipeline themselves; naming these
// unconditionally would trip `unused_imports` on a normal build.
#[cfg(test)]
use crate::ui::frame::{allocate, live_budget, render, Extent};
use std::sync::Arc;

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
    /// The menu's row element: show something when there is anything to show.
    ///
    /// `#[cfg(test)]` because production reaches the rows through
    /// [`overlay_elements_with`], which chooses the floor per host. The window
    /// tests here want the bare element, not a composed overlay.
    #[cfg(test)]
    fn data_rows(rows: Arc<Vec<LogicalLine>>, selected: Option<usize>, active: Vec<usize>) -> Self {
        Self::rows_pinned_to(rows, selected, active, 1)
    }

    /// Rows that must survive a starved budget, with `min` as the floor.
    ///
    /// A menu asks for `1`; a confirmation asks for its option count, because
    /// a dialog whose options have been squeezed away is not a consent prompt.
    /// `allocate` serves every `must` before any `might`
    /// (`ui/frame.rs:219`), so this floor holds under pressure — with the
    /// caveat that below the summed `must`s `fill` degrades to one row each in
    /// element order, and this element is last.
    fn rows_pinned_to(
        rows: Arc<Vec<LogicalLine>>,
        selected: Option<usize>,
        active: Vec<usize>,
        min: usize,
    ) -> Self {
        // `might` is the height *if every row were shown*, measured the way the
        // engine wrote them ([`visual_height`]: embedded `\n` only). Painting
        // can make a row taller — one too wide for the terminal is split — so
        // this is a lower bound on the painted height, which is the safe
        // direction: the allocator is never promised more rows than the element
        // can actually produce. `render_at` re-measures and fits `h` anyway.
        let might: usize = rows.iter().map(visual_height).sum();
        let must = min.min(might);
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
                // Rebased like the cursor, not just filtered: `active_rows`
                // index the *plan's* rows, while `paint_rows` is handed the
                // window and reads them as offsets into it. Without the
                // `- start` an active row at index 7 in a window starting at
                // 5 marks offset 7 — i.e. plan row 12.
                .map(|i| i - start)
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
/// One builder for both hosts (`repl.rs`'s slash menu and the confirmation
/// dialog in `confirm.rs`) so the two cannot drift on which sections exist, in
/// what order, or with what paint. Empty sections are dropped rather than
/// contributed as zero-height elements: a plan with no columns has no header,
/// and an empty element would still ask the allocator for a divider.
///
/// `selected` is passed separately from `plan.selected` so a dialog with its
/// own cursor does not have to clone the plan to move it — cheap now that
/// `rows` is an `Arc`, but clearer not to.
pub(crate) fn overlay_elements(
    plan: &RenderPlan,
    term_w: usize,
    selected: Option<usize>,
) -> Vec<Element> {
    overlay_elements_with(plan, term_w, selected, 1)
}

/// [`overlay_elements`] with a floor on the rows element's `must`.
///
/// `1` is the menu's answer — show something if there is anything to show. A
/// confirmation passes its option count, because `allocate` will happily
/// starve a dialog down to nothing and a consent prompt with no visible options
/// is not one.
pub(crate) fn overlay_elements_with(
    plan: &RenderPlan,
    term_w: usize,
    selected: Option<usize>,
    row_min: usize,
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
        els.push(Element::rows_pinned_to(
            Arc::clone(&plan.rows),
            selected.filter(|s| *s < plan.rows.len()),
            plan.active_rows.clone(),
            row_min,
        ));
    }
    els
}

#[cfg(test)]
mod tests {
    /// An active row keeps its `*` after the rows have scrolled.
    ///
    /// The active indices are *absolute* — they index the plan's rows —
    /// while `paint_rows` is handed the window and reads them as offsets
    /// into it, so they have to be rebased by the window's start exactly
    /// as the cursor is. With a short menu the window starts at 0 and the
    /// two agree by accident; only a list long enough to scroll shows the
    /// difference.
    ///
    /// Driven through [`overlay_elements`] rather than `Element::data_rows`
    /// directly, so the composition path is covered too — a plan whose
    /// active rows are mis-wired on the way in would pass a test that
    /// skipped it.
    #[test]
    fn active_markers_survive_a_scrolled_window() {
        let n = 40usize;
        let rows: Vec<LogicalLine> = (0..n)
            .map(|i| LogicalLine::new(format!("item {i}")))
            .collect();
        // Visible text of a painted row: drop the SGR runs, then trim.
        let plain = |row: &str| {
            let mut out = String::new();
            let mut chars = row.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for e in chars.by_ref() {
                        if e.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out.trim().to_string()
        };
        let stars = |sel: usize, active: Vec<usize>| {
            let plan = RenderPlan {
                rows: Arc::new(rows.clone()),
                selected: Some(sel),
                active_rows: active,
                ..Default::default()
            };
            let els = overlay_elements(&plan, 20, Some(sel));
            let extents: Vec<Extent> = els.iter().map(Element::extent).collect();
            let heights = allocate(&extents, live_budget(24));
            let f = render(&els, &heights, 20);
            (0..f.height())
                .filter(|i| f.row(*i).contains('*'))
                .map(|i| plain(f.row(i)))
                .collect::<Vec<_>>()
        };
        // Cursor deep in the list, so the window starts well above row 0.
        // Active row 31 is inside it; active row 7 is not, and must not be
        // marked — an unmarked row outside the window cannot be mistaken for a
        // mis-placed one.
        assert_eq!(
            stars(30, vec![31]),
            vec!["* item 31".to_string()],
            "the `*` must land on the row it names"
        );
        assert_eq!(
            stars(30, vec![7]),
            Vec::<String>::new(),
            "an active row outside the window must not be marked in it"
        );
    }
    use super::*;
    use crate::output::{NoOutput, Output};

    // --- composed dims reach the apply (ui_draw_v2 §9 #3) ---

    #[test]
    fn default_draw_frame_reports_clamped_dims() {
        // `NoOutput` doesn't override `draw_frame`, so this exercises the
        // trait default: it applies nothing but still reports the clamped
        // dims it was asked to apply.
        // `Frame::empty()` went with the blocking dialog that was its only
        // caller (its erase is now "drop the element and repaint"), so this
        // drives the pipeline for an empty stack instead.
        let empty = render(&[], &[], 1);
        assert_eq!(
            NoOutput.draw_frame(&empty, ApplyPolicy::Frame, (0, 7)),
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
    /// The three sections arrive in paint order — detail, header, rows — and
    /// a section with nothing in it is dropped rather than contributed as a
    /// zero-height element (which would still ask the composer for a divider).
    #[test]
    fn overlay_elements_are_in_paint_order_and_skip_empty_sections() {
        let plan = RenderPlan {
            table_header: Some(LogicalLine::new("MODEL")),
            detail: vec![LogicalLine::new("pick one")],
            rows: Arc::new(vec![LogicalLine::new("m0"), LogicalLine::new("m1")]),
            selected: Some(1),
            active_rows: vec![],
        };
        let els = overlay_elements(&plan, 40, plan.selected);
        let extents: Vec<Extent> = els.iter().map(Element::extent).collect();
        let heights = allocate(&extents, live_budget(24));
        let f = render(&els, &heights, 40);
        assert_eq!(f.height(), 4, "detail + header + 2 rows: {:?}", f.row(0));
        assert!(f.row(0).contains("pick one"), "{:?}", f.row(0));
        assert!(f.row(1).contains("MODEL"), "{:?}", f.row(1));
        assert!(
            f.row(3).contains("> m1"),
            "cursor on the last row: {:?}",
            f.row(3)
        );

        // No header, no detail: just the rows.
        let bare = RenderPlan {
            rows: Arc::new(vec![LogicalLine::new("only")]),
            selected: Some(0),
            ..Default::default()
        };
        let els = overlay_elements(&bare, 40, Some(0));
        assert_eq!(els.len(), 1, "only the rows element");
        let extents: Vec<Extent> = els.iter().map(Element::extent).collect();
        let heights = allocate(&extents, live_budget(24));
        assert_eq!(render(&els, &heights, 40).height(), 1);
    }
}
