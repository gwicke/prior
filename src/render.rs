//! Frame composition and the frame hub (plan `doc/ui_draw_v2.md` §3, design
//! `doc/261003_output_drawing_refactor.md` §Render layer). The pipeline is
//! compose → allocate → render → apply; this module owns the pure phases:
//!
//! * compose builds a list of [`Element`]s (the REPL draws
//!   `[Input, Divider, Overlay]`, dialogs `[Divider?, Overlay]`),
//! * [`allocate`] distributes the live-region budget ([`live_budget`], at
//!   most 2/3 of terminal rows) over the elements' `(must, might)` extents,
//! * [`render`] produces a [`Frame`] — a flat byte buffer with per-row
//!   extents and element spans.
//!
//! [`RenderManager`] (steps 4b/4c) is the hub that runs those phases and
//! owns the [`Draw`] it applies through. The pure phases themselves stay
//! pure and callable directly; the manager touches the terminal only
//! through its `Draw`. A frame's rows are self-contained: each carries the
//! escapes needed to repaint itself in place (trailing `EL` or padding to
//! `term_w`), so `apply` may rewrite any subset of rows unconditionally.

use std::ops::Range;

use crate::draw::{el_if_short, ApplyPolicy, Draw};
use crate::slashdot_menu::table::display_width;
use crate::termio::TermSize;
use tokio::sync::watch;

/// The size watch handle (design §3): what the manager reads its geometry
/// from once the frame verbs re-home (step 4f). Today `render` paints at the
/// caller's `dims` — their *belief* about the terminal, which resize
/// detection compares against what was painted (ui_draw_v2 §9 #3).
pub type SizeWatch = watch::Receiver<TermSize>;

/// The fraction of the terminal height the frame area may occupy — the value
/// [`RenderManager::configure`] is seeded and wired with (design §Render:
/// the ratio is configuration, not a constant buried in the budget fn).
///
/// `f64`, not the design sketch's `f32`: the budget is a `ceil`, and `f32`'s
/// rounding of 2/3 lands *above* the exact fraction (`24·(2/3 as f32) >
/// 16`), which would turn the pinned `ceil(24·2/3) = 16` into 17.
pub const FRAME_MAX_ROW_RATIO: f64 = 2.0 / 3.0;

/// Live-region budget for a terminal of `term_h` rows at the default ratio:
/// the frame may occupy at most this many rows; the rest of the screen is
/// guaranteed scrollback (plan §2). Matches the pop-up cap used throughout
/// v2.5 (`⌈2/3·term_h⌉`, at least 1).
pub(crate) fn live_budget(term_h: usize) -> usize {
    live_budget_ratio(term_h, FRAME_MAX_ROW_RATIO)
}

/// The budget at an arbitrary ratio — [`RenderManager`]'s configured one.
pub(crate) fn live_budget_ratio(term_h: usize, ratio: f64) -> usize {
    ((term_h as f64 * ratio).ceil() as usize).max(1)
}

/// Height demand of one element, in screen rows (plan §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    /// Floor: rows the element keeps before anything else is served.
    pub must: usize,
    /// Want: rows the element takes when space allows.
    pub might: usize,
}

/// One drawable piece of a frame (plan §3.2).
///
/// The *sizing declaration* is data: `must` and `might` sit here so
/// [`allocate`] can read every element's floor and want before any of them is
/// rendered — which is what §3.1's two passes require. Only the rendering is
/// polymorphic, and that is the whole point of the shape:
///
/// - `render` cannot accumulate a `match` over element kinds, which is how
///   squeeze knowledge leaked into this module in the first place. The two
///   opinions that had already leaked are out: `INPUT_CONTEXT_ROWS` now sits in
///   `repl.rs` beside the editor that holds it, and the overlay's `h / 2` in
///   `ui.rs` beside the dialog that holds it (§3.2.2). Only `DividerItem` and
///   `LinesItem`, which have no opinion to express, live here;
/// - each element holds whatever *it* needs to hold — an `Rc` of its rows, a
///   `Layout`, anything — and the frame layer neither knows nor copies it.
pub struct Element {
    /// Floor: rows the element keeps before anything else is served.
    pub must: usize,
    /// Want: rows the element takes when space allows.
    pub might: usize,
    /// The element's own squeeze policy, told what it got.
    ///
    /// `Box` rather than `Rc`: an element is owned by exactly one composition
    /// and is only ever moved between them, so the shared ownership would buy
    /// nothing. `Send + Sync` is not optional — `Output: Send + Sync` and
    /// `draw: Mutex<DrawCache>` put the composition behind both.
    render: Box<dyn RenderAt + Send + Sync>,
}

/// Height in, rows out (plan §3.2.3).
///
/// `term_w` is a *parameter*, not a dependency, so §3.1's "no terminal
/// dependency" holds: an impl can be unit-tested at any width with no terminal
/// anywhere in sight.
pub trait RenderAt {
    /// Produce the rows for an allocation of exactly `h` screen rows of
    /// `term_w`, and this element's caret if it has one.
    ///
    /// Must not return more than `h` rows. The frame builds its row ranges
    /// from what comes back, so an over-tall impl desynchronises `spans`, the
    /// caret and the allocator's own arithmetic — and unlike the old `match`,
    /// nothing downstream can clamp it, because nothing downstream knows what
    /// the rows mean.
    fn render_at(&self, h: usize, term_w: usize) -> Rendered;
}

/// What an element painted, and where its caret was.
#[derive(Default)]
pub struct Rendered {
    /// Full screen rows, **already finished**: padded, `EL`-extended, styled.
    /// The frame concatenates them and does no formatting of its own, so each
    /// element decides whether it wants a background, a trailing `EL`, or
    /// neither.
    pub rows: Vec<String>,
    /// `(row, display_col)`, **element-relative**. `render` offsets it by the
    /// element's first row, so an impl never has to know where it landed.
    pub caret: Option<(usize, usize)>,
}

impl Element {
    /// An element with an explicit `(must, might)` and an impl of its own
    /// rendering. Every constructor below is one line of it, so a new kind is a
    /// `RenderAt` impl plus a constructor here — no `match` anywhere.
    pub(crate) fn new(
        must: usize,
        might: usize,
        render: impl RenderAt + Send + Sync + 'static,
    ) -> Self {
        Element {
            must,
            might,
            render: Box::new(render),
        }
    }

    /// Dim horizontal rule spanning the full terminal width. All-or-nothing:
    /// a one-row rule that does not fit is a rule nobody wanted.
    pub(crate) fn divider() -> Self {
        Element::new(1, 1, DividerItem)
    }

    /// Fixed text rows (commit lines, future status line). `must == might`, so
    /// it is fully served by pass 1 and never competes for the surplus — that
    /// is what `must` is for, and it is why a status row needs no protection
    /// from whatever else the frame is showing.
    ///
    /// Keeps the **first `h` rows** because it has no focus: an element whose
    /// reader has a current row is `input` (caret) or `overlay` (focus), both of
    /// which window around it. Focus-bearing content belongs in one of those
    /// instead — put it here and the cut is arbitrary (plan §3.2.2, which used to
    /// say "truncated from the top" while the code kept the front).
    // Test-only now: production's one fixed-row element is `Element::status`,
    // which resolves itself instead. Targeted allow, per this module's
    // convention for test-only users.
    #[allow(dead_code)]
    pub(crate) fn lines(rows: Vec<String>) -> Self {
        let n = rows.len();
        Element::new(n, n, LinesItem { rows })
    }

    /// The `(must, might)` this element contributes to [`allocate`].
    pub(crate) fn extent(&self) -> Extent {
        Extent {
            must: self.must,
            might: self.might,
        }
    }
}

// --- the four kinds ---------------------------------------------------------
//
// The kinds that have **no squeeze policy of their own**.
//
// The two that do moved to their owners (ui_draw_v2 §3.2.2): `InputItem` lives
// in `repl.rs` beside the editor that decides how much context to keep above a
// caret, and `OverlayItem` in `ui.rs` beside the dialog that decides what
// "half" means. What must not happen is those two *policies* coming back in
// here — which is exactly what they were doing while the four arms were inside
// `render`.

/// A horizontal rule: one row, or none.
struct DividerItem;

impl RenderAt for DividerItem {
    fn render_at(&self, h: usize, tw: usize) -> Rendered {
        if h == 0 {
            return Rendered::default();
        }
        Rendered {
            rows: vec![format!("\x1b[2m{}\x1b[0m", "─".repeat(tw))],
            caret: None,
        }
    }
}

/// Fixed rows in reading order — no focus by construction. See
/// [`Element::lines`] for why that is deliberate. Test-only alongside it.
#[allow(dead_code)]
struct LinesItem {
    rows: Vec<String>,
}

impl RenderAt for LinesItem {
    fn render_at(&self, h: usize, tw: usize) -> Rendered {
        let mut rows = Vec::with_capacity(h.min(self.rows.len()));
        for row in self.rows.iter().take(h) {
            debug_assert!(!row.contains('\n'), "line rows are single screen rows");
            let mut content = row.clone();
            el_if_short(&mut content, tw);
            rows.push(content);
        }
        Rendered { rows, caret: None }
    }
}

/// Distribute `budget` rows over `extents`, honoring floors then splitting
/// the surplus evenly among flexible elements (plan §3.2, OQ1–OQ2).
///
/// * If every element fits (`Σ might ≤ budget`) each gets its `might` —
///   the frame is as tall as its content, never padded to the budget.
/// * Otherwise every element first receives its `must`. If the floors
///   themselves exceed `budget`, they are scaled down by the same even
///   division, in element order (so the input's floor is served before a
///   divider, which is served before an overlay).
/// * The remaining rows are split evenly across elements still below their
///   `might`; shares an element cannot use flow back and are re-split next
///   round (max-min fairness).
pub(crate) fn allocate(extents: &[Extent], budget: usize) -> Vec<usize> {
    if extents.is_empty() {
        return Vec::new();
    }
    let mights: Vec<usize> = extents.iter().map(|e| e.might).collect();
    if mights.iter().sum::<usize>() <= budget {
        return mights;
    }
    let musts: Vec<usize> = extents.iter().map(|e| e.must).collect();
    let mut heights = vec![0usize; extents.len()];
    fill(&mut heights, &musts, budget);
    let granted: usize = heights.iter().sum();
    fill(&mut heights, &mights, budget.saturating_sub(granted));
    heights
}

/// Raise `heights` toward `targets`, distributing `pool` in even shares per
/// round; unused shares fall back into the pool for the next round.
fn fill(heights: &mut [usize], targets: &[usize], mut pool: usize) {
    loop {
        let pending: Vec<usize> = (0..heights.len())
            .filter(|&i| heights[i] < targets[i])
            .collect();
        if pool == 0 || pending.is_empty() {
            return;
        }
        let share = pool / pending.len();
        if share == 0 {
            // Pool smaller than the number of claimants: hand out one row
            // each in element order until it runs out.
            for &i in &pending {
                if pool == 0 {
                    return;
                }
                heights[i] += 1;
                pool -= 1;
            }
            return;
        }
        for &i in &pending {
            let take = share.min(targets[i] - heights[i]);
            heights[i] += take;
            pool -= take;
        }
    }
}

/// Start index of a window over `n` unit-height rows that keeps `focus`
/// visible, preferring `prefer_context` rows of context above it (plan §3.4).
/// Returns 0 when everything fits or `budget` is 0.
pub(crate) fn window(n: usize, focus: usize, budget: usize, prefer_context: usize) -> usize {
    if budget == 0 || n <= budget {
        return 0;
    }
    let focus = focus.min(n - 1);
    // Preferred start, then clamp into the range that keeps `focus` on
    // screen: `start ≤ focus < start + budget`, `start + budget ≤ n`.
    let want = focus.saturating_sub(prefer_context);
    let min_start = (focus + 1).saturating_sub(budget);
    let max_start = (n - budget).min(focus);
    want.clamp(min_start, max_start)
}

/// What [`render`] produces (plan §3.3): one flat buffer, one extent per
/// screen row, and the row range each element occupies.
///
/// Public (opaque — the fields are crate-private) so the [`crate::Output`]
/// frame surface can name it; this module is expected to graduate into a
/// companion UI crate alongside `slashdot_menu`.
#[derive(Clone)]
pub struct Frame {
    /// Rows concatenated **without** separators; `extents[i]` delimits row
    /// `i`. `apply` inserts line breaks (or absolute moves) as it sees fit.
    pub(crate) buf: String,
    /// Byte range of each screen row; contiguous, covering all of `buf`.
    pub(crate) extents: Vec<Range<usize>>,
    /// `(element index, row range)` — which element painted which rows.
    /// Written on every render; reserved for the dirty-span refinement of
    /// the partial diff (§4.4) — the row diff is deliberately kept as the
    /// sole reader until profiling asks for more.
    #[allow(dead_code)]
    pub(crate) spans: Vec<(usize, Range<usize>)>,
    /// `(row, display_col)` — both 0-based; row is frame-relative, col is
    /// the on-screen column (prompt included on row 0). `None` when no input
    /// element is rendered or its caret fell outside the window.
    pub(crate) caret: Option<(usize, usize)>,
}

impl Frame {
    pub(crate) fn height(&self) -> usize {
        self.extents.len()
    }

    /// Bytes of screen row `i`.
    pub(crate) fn row(&self, i: usize) -> &str {
        &self.buf[self.extents[i].clone()]
    }
}

/// Render `elements` at their allocated `heights` into a [`Frame`]
/// (plan §3.1). `heights` must have one entry per element — the output of
/// [`allocate`] — and `term_w` is the terminal width in columns.
pub(crate) fn render(elements: &[Element], heights: &[usize], term_w: usize) -> Frame {
    debug_assert_eq!(
        elements.len(),
        heights.len(),
        "render: one height per element"
    );
    let tw = term_w.max(1);
    let mut buf = String::new();
    let mut extents: Vec<Range<usize>> = Vec::new();
    let mut spans: Vec<(usize, Range<usize>)> = Vec::new();
    let mut caret: Option<(usize, usize)> = None;

    for (idx, (el, &h)) in elements.iter().zip(heights.iter()).enumerate() {
        let first_row = extents.len();
        // The element decides what to show at `h`; the frame lays its rows out
        // and translates its caret into frame coordinates.
        //
        // Nothing downstream clamps the height. That is the trade this shape
        // makes: the old `match` could have corrected an over-tall element
        // because it knew what every arm produced, and a `RenderAt` impl is
        // free to be wrong — so `render_at`'s contract says fit `h`, and the
        // module that could once enforce it no longer can. Worth it, because
        // enforcing it meant the module knowing what each element was.
        let out = el.render.render_at(h, tw);
        for row in &out.rows {
            push_row(&mut buf, &mut extents, row);
        }
        if let Some((r, c)) = out.caret {
            caret = Some((first_row + r, c));
        }
        spans.push((idx, first_row..extents.len()));
    }

    Frame {
        buf,
        extents,
        spans,
        caret,
    }
}

/// Append one screen row to the buffer, recording its extent.
fn push_row(buf: &mut String, extents: &mut Vec<Range<usize>>, row: &str) {
    let start = buf.len();
    buf.push_str(row);
    extents.push(start..buf.len());
}

/// Pad a pre-painted row to `term_w` visible columns. Padding goes *before*
/// a trailing reset so styled rows (menu backgrounds) extend to the right
/// edge; rows already full width are returned unchanged.
///
/// `pub(crate)` because `OverlayItem`'s impl now lives with the dialogs
/// (`ui.rs`) — the padding is a property of the *row*, not of the element kind.
pub(crate) fn pad_row(row: &str, term_w: usize) -> String {
    let w = display_width(row);
    if w >= term_w {
        return row.to_string();
    }
    let pad = " ".repeat(term_w - w);
    match row.strip_suffix("\x1b[0m") {
        Some(body) => format!("{body}{pad}\x1b[0m"),
        None => format!("{row}{pad}"),
    }
}

// --- the frame hub (step 4b) -------------------------------------------------

/// The frame hub (design §Render layer): where a composition comes to be
/// budgeted, rendered, applied and remembered. Step 4b moved the pipeline
/// here — what was `Output::draw_elements`' default body — and 4c moved the
/// drawing edge (`Draw`) under this roof. What remains of the design's
/// shape comes with the steps that give it meaning: the `ContentWrite`
/// hand-off and `render_batch` (4d), batching and the recipe field (4e,
/// when reposition stops reading it from `DrawCache`), and the frame verbs
/// off the watch (4f).
pub struct RenderManager {
    /// The drawing edge (step 4c): the byte sink and its live-screen
    /// bookkeeping, owned — the design's `Draw { … }` under this roof. Plain
    /// fields inside; this manager's `Mutex` is what serializes them.
    draw: Draw,
    /// The fraction of the frame's terminal height the area may occupy —
    /// [`configure`](Self::configure)'s, resolved against the render's
    /// height on every call.
    frame_max_row_ratio: f64,
    /// The watched geometry — the handle `TermIO` hands out at setup (design
    /// §3), cloned here so the manager owns it from the start. Nothing reads
    /// it yet: `render` paints at the caller's `dims` (their belief, §9 #3),
    /// and `frame_size`/`painted_size` re-home onto it in 4f.
    #[allow(dead_code)]
    size: SizeWatch,
}

impl RenderManager {
    /// A fresh hub over the watched geometry, with its own drawing edge. The
    /// budget ratio starts at [`FRAME_MAX_ROW_RATIO`] — the value the
    /// pipeline hard-coded before `configure` existed — so an unconfigured
    /// manager budgets exactly as the old code did; [`Self::configure`]
    /// changes it.
    pub fn new(size: SizeWatch) -> Self {
        Self {
            draw: Draw::new(size.clone()),
            frame_max_row_ratio: FRAME_MAX_ROW_RATIO,
            size,
        }
    }

    /// The drawing edge, for the content path and the frame verbs that still
    /// reach it directly (step 4c: `Content` and `ConsoleOutput` go through
    /// the manager's lock to get here).
    pub(crate) fn draw(&mut self) -> &mut Draw {
        &mut self.draw
    }

    /// Set the frame's max-height ratio (design §Render `configure`) — the
    /// one knob the budget resolves from. The element half of the design's
    /// signature (`frame_elements`) arrives with the element conversion (4h):
    /// nothing installs long-lived chrome while elements are per-compose
    /// boxes.
    pub fn configure(&mut self, frame_max_row_ratio: f64) {
        self.frame_max_row_ratio = frame_max_row_ratio;
    }

    /// The live-region budget at this manager's configured ratio.
    fn budget(&self, term_h: usize) -> usize {
        live_budget_ratio(term_h, self.frame_max_row_ratio)
    }

    /// Allocate → render → apply → remember: the pipeline that was
    /// `Output::draw_elements`' default body (step 4b), painting through the
    /// [`Draw`] this manager has owned since 4c.
    ///
    /// The compose head is *not* here: the status row is the console side's
    /// fact, so the caller composes (`Output::compose_area`) and hands the
    /// stack in — the status row becomes an installed element in 4h and the
    /// head moves in with it.
    pub(crate) fn render(
        &mut self,
        stack: Vec<Element>,
        policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        let (tw, th) = (dims.0.max(1), dims.1.max(1));
        let extents: Vec<Extent> = stack.iter().map(Element::extent).collect();
        // The page-scroll path needs the rows element's grant *before* the
        // frame is rendered, and this is the only place `allocate` runs for a
        // composition — so a caller that has to know re-runs it over the same
        // extents rather than asking the painter to guess twice.
        let heights = allocate(&extents, self.budget(th));
        let frame = render(&stack, &heights, tw);
        let applied = self.draw.draw_frame(&frame, policy, dims);
        // Moved, never copied: the recipe belongs to the composition that built
        // it, and `DrawCache` is where the content path reads it — until the
        // hub takes reposition (4e). Written together, so the recipe and the
        // geometry it was rendered at can never be read apart.
        self.draw.remember_compose(stack, applied);
        applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::LineEditor;

    // ---- live_budget ----

    #[test]
    fn live_budget_caps_at_two_thirds() {
        assert_eq!(live_budget(24), 16);
        assert_eq!(live_budget(3), 2);
        assert_eq!(live_budget(1), 1);
        assert_eq!(live_budget(2), 2);
    }

    // ---- the manager (step 4b) ----

    /// The manager's default budget *is* the old `live_budget` answer, and
    /// `configure` is what moves it: same numbers, one knob (step 4b moved
    /// the hard-coded 2/3 into `configure`).
    #[test]
    fn the_managers_default_budget_is_the_old_one_and_configure_moves_it() {
        let size = crate::termio::fixed(TermSize {
            width: 40,
            height: 24,
        });
        let manager = RenderManager::new(size.clone());
        assert_eq!(manager.budget(24), live_budget(24));
        assert_eq!(manager.budget(1), 1);

        let mut small = RenderManager::new(size);
        small.configure(0.1);
        assert_eq!(small.budget(24), 3);
        assert_eq!(small.budget(1), 1, "never below one row");
    }

    /// `render` is the pipeline that left `Output::draw_elements`: it applies
    /// through the `Draw` it has owned since 4c and remembers the composition
    /// there (recipe + applied size, where the content path reads it).
    /// Captured through the draw's own capture swap — the manager writes
    /// nothing of its own.
    #[test]
    fn manager_render_paints_its_own_draw_and_remembers() {
        let size = crate::termio::fixed(TermSize {
            width: 40,
            height: 24,
        });
        let mut manager = RenderManager::new(size);

        manager.draw.begin_capture();
        let stack = vec![
            Element::lines(vec!["hello".to_string()]),
            Element::divider(),
        ];
        let applied = manager.render(stack, ApplyPolicy::Frame, (40, 24));
        let bytes = manager.draw.end_capture();

        assert_eq!(applied, (40, 24));
        assert!(
            String::from_utf8_lossy(&bytes).contains("hello"),
            "the frame's rows must reach the manager's Draw"
        );
        assert_eq!(
            manager.draw.cache.applied,
            Some((40, 24)),
            "applied size remembered"
        );
        assert_eq!(
            manager.draw.cache.element_stack.len(),
            2,
            "the recipe is remembered where the content path reads it"
        );
    }

    // ---- allocate ----

    #[test]
    fn allocate_full_fit_returns_mights_not_budget() {
        // OQ1: content smaller than the budget renders to its natural size;
        // the frame is never padded up to the budget.
        let e = [
            Extent { must: 1, might: 3 },
            Extent { must: 1, might: 1 },
            Extent { must: 1, might: 5 },
        ];
        assert_eq!(allocate(&e, 12), vec![3, 1, 5]);
    }

    #[test]
    fn allocate_short_input_stays_single_row() {
        // OQ2: a 1-row input has min=max=1; the flexible overlay takes the
        // whole surplus.
        let e = [
            Extent { must: 1, might: 1 },  // short input
            Extent { must: 1, might: 1 },  // divider
            Extent { must: 1, might: 20 }, // menu
        ];
        assert_eq!(allocate(&e, 8), vec![1, 1, 6]);
    }

    #[test]
    fn allocate_splits_surplus_evenly_then_spills() {
        // Two flexible elements: even split first (3/3), then the input
        // caps at its might and the overlay absorbs the leftover.
        let e = [
            Extent { must: 1, might: 4 },
            Extent { must: 1, might: 1 },
            Extent { must: 1, might: 10 },
        ];
        // Σ might = 15 > 10; floors = 3; pool = 7.
        // Round 1: share 3 → input +3 (capped at 4), overlay +3 → pool 1.
        // Round 2: overlay +1.
        assert_eq!(allocate(&e, 10), vec![4, 1, 5]);
    }

    #[test]
    fn allocate_even_split_without_caps() {
        let e = [Extent { must: 1, might: 6 }, Extent { must: 1, might: 6 }];
        // Σ might = 12 > 8; floors 2; pool 6 → 3/3.
        assert_eq!(allocate(&e, 8), vec![4, 4]);
    }

    #[test]
    fn allocate_scales_floors_evenly_when_they_overflow() {
        // Fixed 5-row commit block plus input/divider floors in a 3-row
        // budget: even division from zero caps each claimant at its floor,
        // so the small floors survive and the big one truncates.
        let e = [
            Extent { must: 1, might: 1 },
            Extent { must: 1, might: 1 },
            Extent { must: 5, might: 5 },
        ];
        assert_eq!(allocate(&e, 3), vec![1, 1, 1]);
    }

    #[test]
    fn allocate_crippled_budget_serves_elements_in_order() {
        // Budget below the number of elements: one row each, element
        // order wins — input first, overlay last (plan §3.2).
        let e = [
            Extent { must: 1, might: 1 },
            Extent { must: 1, might: 1 },
            Extent { must: 1, might: 40 },
        ];
        assert_eq!(allocate(&e, 2), vec![1, 1, 0]);
        assert_eq!(allocate(&e, 1), vec![1, 0, 0]);
        assert_eq!(allocate(&e, 0), vec![0, 0, 0]);
    }

    #[test]
    fn allocate_empty_extents() {
        assert!(allocate(&[], 10).is_empty());
    }

    // ---- window ----

    #[test]
    fn window_returns_zero_when_everything_fits() {
        assert_eq!(window(5, 2, 10, 3), 0);
        assert_eq!(window(5, 2, 5, 3), 0);
    }

    #[test]
    fn window_keeps_focus_with_context_above() {
        // 10 rows, 4 visible, caret at 5, want 3 of context → start 2.
        assert_eq!(window(10, 5, 4, 3), 2);
    }

    #[test]
    fn window_clamps_to_screen_ends() {
        // Focus near the end: window pulled forward to stay in bounds.
        assert_eq!(window(10, 9, 4, 3), 6);
        // Focus on row 0: cannot show context that does not exist.
        assert_eq!(window(10, 0, 4, 8), 0);
        // Preferred context larger than the focus offset.
        assert_eq!(window(10, 2, 4, 8), 0);
    }

    #[test]
    fn window_zero_budget_and_degenerate_focus() {
        assert_eq!(window(10, 5, 0, 3), 0);
        // Focus clamped into range before clamping the start.
        assert_eq!(window(4, 99, 2, 1), 2);
    }

    // ---- render ----

    #[test]
    fn render_input_row_prompt_and_caret() {
        let mut e = LineEditor::new();
        e.insert_str("hi");
        let layout = e.layout(20, "> ");
        let els = [Element::input(layout, "> ".to_string())];
        let f = render(&els, &[1], 20);
        assert_eq!(f.height(), 1);
        assert_eq!(f.row(0), "> hi\x1b[K");
        // Prompt (2) + "hi" (2) = col 4; caret on the frame's only row.
        assert_eq!(f.caret, Some((0, 4)));
        assert_eq!(f.spans, vec![(0, 0..1)]);
        assert_eq!(f.extents, vec![0..7]);
    }

    #[test]
    fn render_windowed_input_keeps_caret_visible() {
        // 6-row input squeezed into 3 rows with the caret on row 5:
        // window keeps the caret with context above — rows 3..6.
        let mut e = LineEditor::new();
        e.insert_str("aa\nbb\ncc\ndd\nee\nff"); // caret ends at buffer end
        let layout = e.layout(20, "> ");
        assert_eq!(layout.row_count, 6);
        assert_eq!(layout.cursor_row, 5);
        let els = [Element::input(layout, "> ".to_string())];
        let f = render(&els, &[3], 20);
        assert_eq!(f.height(), 3);
        // Prompt row scrolled out: no "> " on the first visible row.
        assert_eq!(f.row(0), "dd\x1b[K");
        assert_eq!(f.row(2), "ff\x1b[K");
        // Caret at frame row 2, content col 2 (no prompt on this row).
        assert_eq!(f.caret, Some((2, 2)));
    }

    #[test]
    fn render_divider_spans_full_width() {
        let f = render(&[Element::divider()], &[1], 8);
        assert_eq!(f.row(0), "\x1b[2m────────\x1b[0m");
        assert_eq!(f.caret, None);
    }

    #[test]
    fn render_lines_keeps_the_first_h_rows() {
        let rows: Vec<String> = (1..=5).map(|i| format!("line {i}")).collect();
        let f = render(&[Element::lines(rows)], &[3], 20);
        assert_eq!(f.height(), 3);
        // The front, in reading order — the bottom is what is cut. This test
        // was called `render_lines_truncate_from_top`, which described the
        // opposite behaviour, while asserting exactly these three lines
        // (ui_draw_v2 §3.2.2).
        assert_eq!(f.row(0), "line 1\x1b[K");
        assert_eq!(f.row(2), "line 3\x1b[K");
        assert_eq!(f.caret, None);
    }

    /// The sizing declaration is *data* now (§3.2.3), which means it can be wrong
    /// without the allocator noticing: `allocate` reads the fields, not the impl.
    ///
    /// One assertion per kind, because this is the part the refactor moved from a
    /// `match` into constructors — and mutation-checking `must: 1 → 0` on the
    /// input sailed through the whole suite. A wrong `must`/`might` here shows up
    /// much later as a frame squeezed wrong on a short terminal, so it is named
    /// instead.
    #[test]
    fn each_kind_declares_the_floor_and_want_it_promised() {
        let mut e = LineEditor::new();
        e.insert_str("hi");
        assert_eq!(
            Element::input(e.layout(20, "> "), "> ".into()).extent(),
            Extent {
                must: 1,
                might: e.layout(20, "> ").row_count.max(1)
            },
            "the caret's own row is the floor: the input can never be squeezed away"
        );
        assert_eq!(
            Element::divider().extent(),
            Extent { must: 1, might: 1 },
            "a rule is all-or-nothing"
        );
        assert_eq!(
            Element::lines(vec!["a".to_string(), "b".to_string(), "c".to_string()]).extent(),
            Extent { must: 3, might: 3 },
            "fixed content is served whole in pass 1 — that is what `must` is for"
        );
    }

    /// The caret is **frame-relative**, and there is exactly one thing that can
    /// make that false: an input that is not the first element.
    ///
    /// Every caret test in this file puts the input at index 0, where
    /// `first_row` is 0 — so deleting the offset from `render` passed all 850.
    /// In production the input is *always* second, behind the status row, and
    /// `compose_frame`'s tests pass `status: None`, so the offset has never
    /// been exercised either. The refactor moved this arithmetic out of
    /// `render`'s `Input` arm into `Rendered::caret`; pin it where it is, and
    /// pin `spans` while there, since both come from the same `first_row`.
    #[test]
    fn the_input_caret_is_offset_by_its_element_first_row() {
        let mut e = LineEditor::new();
        e.insert_str("hi");
        let layout = e.layout(20, "> ");
        let els = [
            Element::lines(vec!["bar".to_string()]),
            Element::input(layout, "> ".into()),
        ];
        let f = render(&els, &[1, 1], 20);
        assert_eq!(f.height(), 2);
        // Frame row 1, not 0 — the status row occupies row 0.
        assert_eq!(
            f.caret,
            Some((1, 4)),
            "prompt (2) + \"hi\" (2) = col 4, but on the input's own row"
        );
        assert_eq!(f.spans, vec![(0, 0..1), (1, 1..2)]);
    }

    #[test]
    fn render_full_composition_extents_and_spans() {
        // REPL composition: input + divider + overlay, allocated heights
        // straight from `allocate`.
        let mut e = LineEditor::new();
        e.insert_str("ls -la");
        let layout = e.layout(80, "> ");
        let els = [
            Element::input(layout, "> ".to_string()),
            Element::divider(),
            Element::lines(vec!["one".to_string(), "two".to_string()]),
        ];
        let heights = allocate(
            &els.iter().map(Element::extent).collect::<Vec<_>>(),
            live_budget(24),
        );
        assert_eq!(heights, vec![1, 1, 2]); // all mights fit in 16
        let f = render(&els, &heights, 80);

        // Contiguous extents covering the buffer exactly.
        let mut expect = 0;
        for r in &f.extents {
            assert_eq!(r.start, expect);
            expect = r.end;
        }
        assert_eq!(expect, f.buf.len());
        // Spans tile the frame in element order.
        assert_eq!(f.spans, vec![(0, 0..1), (1, 1..2), (2, 2..4)]);
        assert_eq!(f.height(), 4);
        assert_eq!(f.row(0), "> ls -la\x1b[K");
        assert!(f.row(1).starts_with("\x1b[2m"));
        assert_eq!(f.row(3), "two\x1b[K", "`lines` ends a short row with EL");
        // Caret follows the input element, prompt-inclusive column.
        assert_eq!(f.caret, Some((0, 8)));
    }

    #[test]
    fn render_zero_height_element_contributes_no_rows() {
        let els = [Element::divider(), Element::divider()];
        let f = render(&els, &[1, 0], 4);
        assert_eq!(f.height(), 1);
        assert_eq!(f.spans, vec![(0, 0..1), (1, 1..1)]);
    }

    #[test]
    fn pad_row_handles_styled_and_unstyled() {
        assert_eq!(pad_row("x", 4), "x   ");
        assert_eq!(pad_row("x\x1b[0m", 4), "x   \x1b[0m");
        assert_eq!(
            pad_row("\x1b[48;5;239mab\x1b[0m", 4),
            "\x1b[48;5;239mab  \x1b[0m"
        );
        assert_eq!(pad_row("abcde", 4), "abcde"); // never truncates
    }
}
