//! Pure frame composition for the unified draw pipeline (plan
//! `doc/ui_draw_v2.md` §3). The pipeline is compose → allocate → render →
//! apply; this module owns the first three, all pure:
//!
//! * compose builds a list of [`Element`]s (the REPL draws
//!   `[Input, Divider, Overlay]`, dialogs `[Divider?, Overlay]`),
//! * [`allocate`] distributes the live-region budget ([`live_budget`], at
//!   most 2/3 of terminal rows) over the elements' `(must, might)` extents,
//! * [`render`] produces a [`Frame`] — a flat byte buffer with per-row
//!   extents and element spans.
//!
//! The `apply` phase is also pure here: [`plan_apply`] turns a frame plus the
//! live-screen [`DrawCache`] into the exact escape bytes to write and the
//! post-apply cache state (plan §4). `ConsoleOutput::draw_frame` is the only
//! caller that actually writes them.
//!
//! Nothing here touches a terminal. A frame's rows are self-contained: each
//! carries the escapes needed to repaint itself in place (trailing `EL` or
//! padding to `term_w`), so `apply` may rewrite any subset of rows
//! unconditionally.

use std::ops::Range;

use crate::output::ContentDisplayKind;
use crate::slashdot_menu::table::display_width;

/// Live-region budget for a terminal of `term_h` rows: the frame may occupy
/// at most this many rows; the rest of the screen is guaranteed scrollback
/// (plan §2). Matches the pop-up cap used throughout v2.5 (`⌈2/3·term_h⌉`,
/// at least 1).
pub(crate) fn live_budget(term_h: usize) -> usize {
    ((term_h as f64 * 2.0 / 3.0).ceil() as usize).max(1)
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
    /// A frame with no rows. Applying it with [`ApplyPolicy::Frame`] is the
    /// "erase the live region" operation: the union clear erases every row
    /// the previous frame painted and the caret parks on the region's first
    /// row — ready for the next write (dialog dismissal, plan §5.3).
    pub(crate) fn empty() -> Frame {
        render(&[], &[], 1)
    }

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

// ---------------------------------------------------------------------------
// Apply: the live-screen cache and the escape planner (plan §4).
//
// Kept pure (bytes out, cache in/out) so the REPL, the dialogs and the vt100
// tests all exercise the *same* sequence: `ConsoleOutput::draw_frame` only
// locks the writer and writes what [`plan_apply`] returns. Newlines are emitted
// bare — the tty's OPOST/ONLCR maps them to CRLF (the same convention as the
// REPL's redraw) — so any harness that bypasses the tty must do that mapping.
// ---------------------------------------------------------------------------

/// How [`plan_apply`] treats a frame's screen region (plan §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyPolicy {
    /// Frame UI (input / overlay / dialog): partial row-diff when the previous
    /// frame is intact and nothing moved, else a full scroll-then-paint with a
    /// bounded clear of the old frame region.
    Frame,
    /// Commit lines / append: write the rows at the anchor as content
    /// (scrolling if needed), no region clear; the caret lands just below.
    Append,
}

/// The last applied frame, while it is still intact on screen (plan §4.1).
pub(crate) struct LiveFrame {
    /// Absolute 1-based row of the frame's first row, post-scroll. The bottom
    /// is derived as `frame_top_row + frame.height() - 1`.
    pub(crate) frame_top_row: usize,
    /// Terminal size when the frame was applied (partial-apply predicate).
    pub(crate) term_w: usize,
    pub(crate) term_h: usize,
    /// The rows, for the partial-apply diff.
    pub(crate) frame: Frame,
}

/// Live screen bookkeeping, owned by `ConsoleOutput` (plan §4.1). Rows are
/// absolute; bottom-relative quantities are recomputed on every apply.
pub(crate) struct DrawCache {
    /// Absolute cursor position as we last left it: (1-based row, 0-based col).
    /// The row is authoritative; the col is best-effort (only an apply sets it,
    /// and only the row is ever consumed).
    caret: (usize, usize),
    /// Terminal height at the last apply. The full-apply clear floor is
    /// `min(term_h, screen_bottom)`, so a maximize's revealed rows below the old
    /// bottom survive. Never cleared by invalidation.
    screen_bottom: usize,
    live: Option<LiveFrame>,
    /// The last composition, kept as its **recipe** — the element list, not the
    /// rendered rows it produced (ui_draw_v2 §5.1.1). §4.5's content path
    /// re-draws this instead of recomposing: re-rendering is cheap and always
    /// current, where the rendered rows would be a snapshot of whatever the
    /// status said at the moment of compose.
    ///
    /// Deliberately *not* cleared by `note_lines`, `plan_consume` or an
    /// invalidation — those retire the frame's *position*, and the recipe is
    /// exactly what survives it. A later `draw_elements` overwrites it.
    pub(crate) element_stack: Vec<Element>,
    /// The size the last `draw_elements` actually applied (clamped), i.e. what
    /// the *next* compose can assume about what is on screen — the answer to
    /// "has the terminal changed shape since?" (ui_draw_v2 §9 #3).
    ///
    /// `None` until the first apply, so a caller asking before anything has
    /// been painted falls back to a live terminal query instead of being told
    /// a size nobody ever drew at.
    ///
    /// Owned here rather than by the caller that happens to paint (the REPL
    /// loop used to keep its own `Cell`): `read_input` and the turn's key
    /// branch both apply frames now, and two independent books of the same
    /// number are two ways for them to disagree about whether a repaint is due.
    pub(crate) applied: Option<(usize, usize)>,
    /// The logical kind of the **last transcript content** written.
    ///
    /// Not "was a rule just emitted" — *what was written*, so a change to how
    /// content is formatted does not have to change this. `None` means the
    /// last write was not classified (which is every path that does not opt
    /// in), and `with_content_writer` overwrites it on every content write
    /// rather than letting it go stale.
    ///
    /// `draw_frame` deliberately does **not** touch it: an area apply is not
    /// transcript content, and treating it as such would let a frame's last
    /// row decide what a later commit may collapse against.
    pub(crate) last_content_display_kind: Option<ContentDisplayKind>,
    /// True when that last write declared itself the last flush of its
    /// segment, so the next content write opens a new block even under the
    /// same kind. Read and cleared by `with_content_writer`, which is the only
    /// consumer: like `last_content_display_kind`, it is not transcript
    /// content and `draw_frame` leaves it alone.
    pub(crate) last_content_closes_segment: bool,
    /// True while a content batch is open (`Output::begin_batch`). While set a
    /// content write consumes the frame and does **not** put it back — one
    /// re-emit when the guard drops, instead of one per write (ui_draw_v2 R5).
    ///
    /// A flag rather than a nesting depth, because batches are **flat**: one
    /// at a time, opened and closed iteratively, never recursed. A counter
    /// would have made nesting *supported* and brought the failure mode that
    /// goes with it — an unbalanced enter/exit leaves a nonzero depth
    /// permanently, and then the area never comes back at all.
    ///
    /// A nested open therefore **collapses**: `begin_batch` returns a guard
    /// that owns nothing, and only the guard that raised this flag clears it.
    /// That makes nesting impossible rather than merely handled — and because
    /// the collapse is silent, `begin_batch` also `debug_assert`s, so it is
    /// loud in debug builds and merely correct in release. The worst a
    /// mistimed end does is redraw once too often.
    pub(crate) batch_active: bool,
    /// The frame taken by a batch's *first* consume, held until the batch
    /// ends.
    ///
    /// Needed because `reposition_area` falls back to `taken` when no recipe
    /// recorded itself — reachable in production by `draw_frame` called
    /// directly (`ui::resolve`'s empty frame), which applies a frame and
    /// clears `element_stack`. Without this stash a batch would consume that
    /// area and its final reposition would find an empty recipe and render
    /// nothing, so the area would never return.
    pub(crate) deferred: Option<Frame>,
}

impl DrawCache {
    pub(crate) fn new(term_h: usize) -> Self {
        DrawCache {
            caret: (1, 0),
            screen_bottom: term_h.max(1),
            live: None,
            element_stack: Vec::new(),
            applied: None,
            last_content_display_kind: None,
            last_content_closes_segment: false,
            batch_active: false,
            deferred: None,
        }
    }

    /// The tracked cursor row (1-based), for the host's row-accounting tests:
    /// they compare it against where a terminal emulator puts the cursor after
    /// replaying the same bytes (row_accounting §6 B2).
    #[cfg(test)]
    pub(crate) fn tracked_row(&self) -> usize {
        self.caret.0
    }

    /// Where the next frame belongs (plan §5.1): the intact frame's top row if
    /// there is one, else the tracked cursor row.
    pub(crate) fn anchor(&self) -> usize {
        self.live
            .as_ref()
            .map(|l| l.frame_top_row)
            .unwrap_or(self.caret.0)
    }

    /// Advance the tracked cursor by `n` lines, clamped to the screen, and
    /// invalidate the frame region: an ordinary content write moved the cursor,
    /// so the previous frame no longer describes the screen (plan §4.2).
    pub(crate) fn note_lines(&mut self, n: usize, term_h: usize) {
        let h = term_h.max(1);
        self.caret.0 = (self.caret.0 + n).clamp(1, h);
        self.live = None;
    }

    /// Record a re-anchor attempt (plan §4.2) and retire the frame region,
    /// returning the row to use and the bytes that erase it.
    ///
    /// This is the one sanctioned absolute path (row_accounting §6b). A resize
    /// or a foreign writer can leave the cursor *outside* the live block, and
    /// then no relative walk can reach the block's rows — the erase that
    /// precedes every paint walks up from the cursor, so it would erase the
    /// wrong rows entirely. Here the block's row is still known and a DSR has
    /// just confirmed where the cursor went, so the old region is addressed
    /// and the cursor parked where the anchor says.
    ///
    /// "Any uncertainty → clear it" still holds: with an answer (`Some`) the
    /// tracked row is replaced by it, and on fallback (`None` — a terminal that
    /// never answered, e.g. a headless pty) the tracked row stands and the old
    /// region is still erased, because a silent terminal offers no more
    /// guarantee than an answering one (plan §9 latent issue 2).
    pub(crate) fn reanchor(
        &mut self,
        row: Option<usize>,
        term_w: usize,
        term_h: usize,
    ) -> (usize, String) {
        let mut s = String::new();
        if let Some(r) = row {
            self.caret.0 = r.max(1);
        }
        // Copy out before mutating: the borrow of `self.live` has to end before
        // the refresh below re-takes it.
        let info = self.live.as_ref().map(|l| {
            (
                frame_caret(&l.frame).0,
                l.frame.height(),
                l.frame_top_row,
                l.term_w,
                l.term_h,
            )
        });
        if let Some((off, h, top, old_w, old_h)) = info {
            // **A narrowing resize wraps.** The area's own rows survive, but each
            // of its lines can now span several terminal rows, so clearing `h`
            // of them leaves wrapped remnants behind. Clear each line as many
            // rows as it now occupies.
            let wrap = if old_w > term_w && term_w > 0 {
                old_w.div_ceil(term_w)
            } else {
                1
            };
            let rows = h.saturating_mul(wrap);
            let reflowed = old_w != term_w || old_h != term_h;

            if reflowed && row.is_some() {
                // **The reflowed case: only the cursor is trustworthy.**
                // `frame_top_row` is a pre-resize absolute and addresses a row
                // the area no longer occupies, so the area's rows come from the
                // cursor — `top = cursor - offset` — and the clear walks up to
                // them and down again, all relative.
                //
                // This is also what stops the area drifting down a row per
                // re-anchor: the old code left `caret` on the cursor's row (the
                // area's *last*) and dropped `live`, so the next apply anchored
                // there. Refreshing `frame_top_row` instead keeps `anchor()`
                // reading the area's own top, which also unloads `cache.caret`
                // of the second job it was doing.
                //
                // `term_w`/`term_h` stay at the size the frame was *applied* at,
                // so the partial-apply predicate still sees the mismatch and
                // forces a full re-apply.
                cuu(&mut s, off);
                for i in 0..rows {
                    s.push('\r');
                    s.push_str(EL);
                    if i + 1 < rows {
                        cud(&mut s, 1);
                    }
                }
                if let Some(l) = self.live.as_mut() {
                    l.frame_top_row = self.caret.0.saturating_sub(off).max(1);
                }
            } else {
                // **No reflow, or a silent terminal.** The cursor may be
                // *outside* the block — a foreign writer put it there — so a
                // relative walk cannot reach the block's rows at all, and the
                // pre-resize row is the only thing left to address. This is the
                // one absolute address in a frame apply (row_accounting §6b).
                for i in 0..rows {
                    s.push_str(&cup(top + i, 1));
                    s.push_str(EL);
                }
                self.live = None;
            }
        }
        s.push_str(&cup(self.caret.0, self.caret.1 + 1));
        (self.caret.0, s)
    }
}

/// Plan the escape bytes that apply `frame` at the cursor under `policy`,
/// advancing `cache` to the post-apply state (plan §4.3–4.4). Pure: the caller
/// writes the returned bytes.
///
/// The bytes are **relative**: nothing in a frame apply addresses an absolute
/// row, so the plan cannot land on a row it did not mean to
/// (row_accounting §6b). The `top`/`delta` arithmetic that follows is
/// bookkeeping only — the tracked cursor row that row accounting compares
/// against a terminal, and the bounded clear below the block — and is never
/// used for addressing.
pub(crate) fn plan_apply(
    cache: &mut DrawCache,
    frame: &Frame,
    policy: ApplyPolicy,
    term_w: usize,
    term_h: usize,
) -> String {
    let tw = term_w.max(1);
    let th = term_h.max(1);
    // The block goes where the cursor is: the live frame's own top if one is
    // intact, else the tracked cursor row. That is `DrawCache::anchor`, which
    // used to be a parameter — a caller could name a row, and all it could do
    // with it was contradict the block's own position (row_accounting §6b).
    let anchor = cache.anchor().clamp(1, th);
    let h = frame.height();

    if policy == ApplyPolicy::Append {
        return plan_append(cache, frame, anchor, th);
    }

    // Bookkeeping only: the block's top row, for the tracked cursor row and the
    // partial predicate below. The `delta` this computes is no longer emitted —
    // step 2 of `plan_full` gets the terminal to do the scrolling instead.
    let delta = (anchor + h).saturating_sub(1).saturating_sub(th);
    let top = anchor.saturating_sub(delta).max(1);

    let partial = cache.live.as_ref().is_some_and(|live| {
        delta == 0 && top == live.frame_top_row && tw == live.term_w && th == live.term_h
    });
    if partial {
        plan_partial(cache, frame, top, th)
    } else {
        plan_full(cache, frame, top, tw, th)
    }
}

/// Full apply: retire the previous frame's rows, descend to the block's last
/// row, clear what the block leaves below, paint it bottom-up, place the caret.
/// Every step is a walk from wherever the cursor already is.
fn plan_full(cache: &mut DrawCache, frame: &Frame, top: usize, tw: usize, th: usize) -> String {
    let h = frame.height();
    let mut s = String::new();

    // 1. Retire the previous frame, if one is still live.
    //
    //    `live` is set exactly when the cursor is still at that frame's caret,
    //    so the walk to its first row is `CUU` by the caret's own offset — and
    //    it is what the new block's top is measured from. Without it the block
    //    creeps down by one row on every full apply, because the cursor rests
    //    at the caret and a caret on the last row is a row below the top the
    //    next apply would assume. (A cursor that has *left* the block is the
    //    re-anchor's case, and `reanchor` clears `live`.)
    //
    //    Erasing here, before the scroll, also removes the residue the absolute
    //    path reached by arithmetic: a scroll pushes the old rows *above* the
    //    new block's top, where nothing can reach them afterwards. That was the
    //    union clear's other job.
    let old_cur = match cache.live.as_ref() {
        Some(live) => {
            let h_old = live.frame.height();
            let (cur, _col) = frame_caret(&live.frame);
            cuu(&mut s, cur);
            erase_down(&mut s, h_old);
            cuu(&mut s, h_old.saturating_sub(1));
            cur
        }
        None => 0,
    };

    // **NOT the model — see `ui_draw_v2` §4/§4.5.7.** The area is not
    // placed against the screen's last row; it sits below the content and is
    // pushed down by it. This descend-to-fit belongs to the pending replacement.
    // 2. Make room for the block and land on its last row. `h-1` bare LFs do
    //    both: an LF at the bottom row scrolls, anywhere else it moves down
    //    one. So the block's last row is the cursor's row afterwards, and
    //    exactly `delta` lines reach scrollback — the same split the absolute
    //    path computed from the tracked row, except the *terminal* decides it,
    //    so the block fits even when the tracked row was wrong.
    for _ in 0..h.saturating_sub(1) {
        s.push('\n');
    }

    // 3. Clear the rest of the live region, floored so a maximize's revealed
    //    rows survive. Nothing the block paints is touched — these are the rows
    //    below its bottom. Done before the paint so the only walk back is the
    //    one that follows.
    let floor = th.min(cache.screen_bottom);
    let below = (floor + 1).saturating_sub(top + h);
    if below > 0 {
        // Step down first: the cursor is on the block's *last* row, and the row
        // about to be cleared is the one below it.
        cud(&mut s, 1);
        for i in 0..below {
            s.push_str(EL);
            if i + 1 < below {
                cud(&mut s, 1);
            }
        }
        cuu(&mut s, below);
    }

    // 4. Paint bottom-up: the cursor is on the block's last row, so every row
    //    is one `CUU 1` below the one to paint. Walking down instead would not
    //    work — `CUD` clamps at the bottom row rather than scrolling — and
    //    painting down would need a walk to the top first.
    for i in (0..h).rev() {
        if i + 1 < h {
            cuu(&mut s, 1);
        }
        s.push('\r');
        s.push_str(frame.row(i));
    }

    // 5. Caret (absolute row returned for bookkeeping only), then the cache.
    //    An empty frame is the "erase the region" operation: it parks the
    //    cursor on the region's first row, so whatever the caller appends next
    //    lands where the frame was (plan §5.3).
    let caret = if h == 0 {
        cuu(&mut s, old_cur);
        (top, 0)
    } else {
        push_caret(&mut s, frame, top, th)
    };
    cache.live = Some(LiveFrame {
        frame_top_row: top,
        term_w: tw,
        term_h: th,
        frame: frame.clone(),
    });
    cache.screen_bottom = th;
    cache.caret = caret;
    s
}

/// Partial apply: repaint only the rows whose bytes changed, clear rows an
/// orphaned (shrunken) frame left behind, and walk the caret back. One batched
/// write with no newlines — it can never scroll, and every row is reached from
/// the previous one rather than addressed.
fn plan_partial(cache: &mut DrawCache, frame: &Frame, top: usize, th: usize) -> String {
    let h_new = frame.height();
    // Where the previous apply left the cursor: *its* caret, which is a
    // frame-relative quantity — no absolute row, and no dependence on the
    // tracked row agreeing with the terminal. This is the walk's starting
    // point; the caret the new frame wants is where the walk must end, and the
    // two differ whenever the frame grew or shrank around the caret (a
    // Shift+Enter, an editor re-wrap).
    let (from, h_old) = {
        let live = cache.live.as_ref().expect("partial needs a live frame");
        (frame_caret(&live.frame), live.frame.height())
    };
    let (to_r, to_c) = frame_caret(frame);
    let mut s = String::new();
    let mut at = from.0;
    {
        let old = &cache
            .live
            .as_ref()
            .expect("partial needs a live frame")
            .frame;
        for i in 0..h_new.max(h_old) {
            let bytes: Option<&str> = match (i < h_new, i < h_old) {
                (true, true) if frame.row(i) != old.row(i) => Some(frame.row(i)),
                (true, false) => Some(frame.row(i)),
                // Frame shrank: erase the orphaned row.
                (false, true) => Some(EL),
                _ => None,
            };
            let Some(bytes) = bytes else { continue };
            move_rows(&mut s, at, i);
            s.push('\r');
            s.push_str(bytes);
            at = i;
        }
    }
    move_rows(&mut s, at, to_r);
    // `CR` first, for the same reason as `push_caret`: the last repainted row
    // left the cursor at its end, and `CUF` walks from the current column.
    s.push('\r');
    cuf(&mut s, to_c);
    let (term_w, term_h) = {
        let live = cache.live.as_ref().expect("partial needs a live frame");
        (live.term_w, live.term_h)
    };
    cache.live = Some(LiveFrame {
        frame_top_row: top,
        term_w,
        term_h,
        frame: frame.clone(),
    });
    cache.caret = ((top + to_r).clamp(1, th), to_c);
    s
}

/// Append apply (plan §2, commit lines): write the rows as content — the
/// natural form, a row and a newline each, so the terminal wraps and scrolls on
/// its own terms and no row is addressed. The caret lands just below the block.
/// No region clear; `live` is cleared so the next frame re-anchors at the
/// cursor.
fn plan_append(cache: &mut DrawCache, frame: &Frame, anchor: usize, th: usize) -> String {
    let h = frame.height();
    let delta = (anchor + h).saturating_sub(1).saturating_sub(th);
    let top = anchor.saturating_sub(delta).max(1);
    let mut s = String::new();
    for i in 0..h {
        s.push('\r');
        s.push_str(frame.row(i));
        // A bare LF at the last row scrolls the block into scrollback — exactly
        // the commit-line behavior. Bare, because the tty's OPOST/ONLCR maps it
        // to CRLF; the explicit `CR` above keeps this correct without it.
        s.push('\n');
    }
    cache.live = None;
    cache.screen_bottom = th;
    cache.caret = ((top + h).min(th), 0);
    s
}

/// Consume (§4.5 P1): a content write is about to start where the frame is.
/// Walk to the frame's first row, clear what it painted, and take the frame —
/// which comes back below the content via [`plan_reposition`].
///
/// "Consume" is about **ownership**, not pixels: the frame stops being *live*
/// (`live = None`) but is handed back rather than dropped, so it can be
/// re-emitted. What it replaces retires the frame outright, and that is the
/// difference that matters (§4.5.3).
///
/// The pixels *are* cleared, and that is load-bearing rather than a residue of
/// the old erase: the frame's rows are padded to full width, so a content line
/// shorter than the terminal would leave whatever the frame painted to its
/// right of it still showing — the other half of the §1.1 resume artefact.
///
/// What the retire-and-clear gets wrong is where the walk *ends*: at the
/// block's caret, so content starts mid-frame and strands the rows above it.
/// It ends at the block's first row, column 0, because content takes the
/// block's **whole height** — the status and input rows are content's to use
/// too, not a margin the write has to live below.
///
/// Returns the walk and the frame to re-emit, or `None` when no frame was live.
/// `None` was the state of a whole turn under the old scheme; now it is the
/// state before a turn's first frame and after one resolves.
///
/// **`CR` is part of the contract, not a detail.** `CUU` preserves the column,
/// and the frame's caret is out at the *input's* column, so without an explicit
/// `CR` the content would start a prompt-width to the right. It used to arrive
/// by accident — the transient bar's own clear ended in one — and that bar is
/// deleted with the rest of the transient machinery, so the byte is what
/// carries it.
pub(crate) fn plan_consume(cache: &mut DrawCache) -> Option<(String, Frame)> {
    let live = cache.live.take()?;
    let (caret_row, _) = frame_caret(&live.frame);
    let h = live.frame.height();
    let mut s = String::new();
    // Down to the block's last row, then erase bottom-up to its first — a walk
    // *down* out of a block is impossible: `CUD` clamps at the screen edge
    // instead of scrolling. The block is on screen, so its last row is
    // reachable from any row inside it.
    //
    // `CR` before every `EL`, and that is the whole trick: `EL` reaches only
    // from the cursor to the end of the line, and the cursor is out at the
    // input's caret — so without it the erase would clear what lies *past* the
    // prompt and leave the prompt itself standing, which is exactly the tail a
    // short content line would then inherit.
    //
    // Also see `ui_draw_v2` §5.2: this erases all `h` rows every write, but rows are
    // padded to `term_w`, so a content write already overwrites them. It is only
    // needed when the area *shrank*, and then just for `old_h - new_h` rows.
    if h > 0 {
        let cur = caret_row.min(h - 1);
        cud(&mut s, h - 1 - cur);
        for i in 0..h {
            s.push('\r');
            s.push_str(EL);
            if i + 1 < h {
                cuu(&mut s, 1);
            }
        }
    }
    // Postcondition: column 0 of the block's first row.
    s.push('\r');
    cache.caret = (live.frame_top_row, 0);
    Some((s, live.frame))
}

/// Reposition apply (§4.5 P1): the frame's top row has moved **down**, because
/// content was written over the rows it used to occupy, and those rows now hold
/// content that must not be erased.
///
/// This is `plan_full` with the retire step removed, and that removal is the
/// whole point. Neither existing path fits: `plan_partial` requires an unmoved
/// frame, and `plan_full` erases the old region — which here would erase the
/// content that has just landed there. So the frame is walked to where the
/// content left off and painted there, and nothing above is touched.
///
/// The contract is deliberately narrow: **the same frame, at a new top**. A
/// frame that *changed* (grew a row, lost one) still goes through `plan_apply`,
/// because only that path can retire the rows a shrunken frame orphaned. The
/// caller re-renders the *same recipe* the consume took, at the same dims, so
/// the height is fixed by construction and the narrowness costs nothing — what
/// changed is where it goes and what its status row says, not how tall it is.
///
/// No `CUP`: the anchor is the cache's own caret, which is where the content
/// write left the cursor.
pub(crate) fn plan_reposition(
    cache: &mut DrawCache,
    frame: &Frame,
    tw: usize,
    th: usize,
) -> String {
    let h = frame.height();
    debug_assert!(h > 0, "reposition needs rows to paint");
    let anchor = cache.caret.0.clamp(1, th);
    let delta = (anchor + h).saturating_sub(1).saturating_sub(th);
    let top = anchor.saturating_sub(delta).max(1);
    let mut s = String::new();
    // **NOT the model — see `ui_draw_v2` §4/§4.5.7 and the §5.2 known gap.**
    // The area is *not* placed by its bottom edge: it sits immediately below
    // the content and the content pushes it down, so it should paint forward
    // from a fresh row and never descend. What follows anchors the block to
    // the screen's last row instead — the common CLI idiom, but not this one.
    // Kept until the replacement lands; do not read it as the design.
    // Make room and land on the block's last row. `h-1` bare LFs do both: an LF
    // at the bottom row scrolls, anywhere else it moves down one. So exactly
    // `delta` of them reach scrollback and the block fits even when the tracked
    // row was wrong — the terminal decides, as in `plan_full`.
    for _ in 0..h.saturating_sub(1) {
        s.push('\n');
    }
    // **No clear of the rows below the block**, and the omission is load-bearing
    // enough to spell out. It is safe because those rows are provably clean:
    //
    // - With no scroll (`delta == 0`) they are the rows below the *old* frame,
    //   which P4 had already cleared, and the content write stopped at the old
    //   frame's top so it never reached them.
    // - With a scroll, the bottom `delta` rows the scroll reveals are blank by
    //   definition, and everything above them was the clean region from before.
    //
    // So the invariant P4 protects still holds without spending `below` erase
    // codes per chunk — which matters most exactly when the area is mid-screen
    // with free space beneath it, the case where `below` is largest.
    for i in (0..h).rev() {
        if i + 1 < h {
            cuu(&mut s, 1);
        }
        s.push('\r');
        s.push_str(frame.row(i));
    }
    let caret = push_caret(&mut s, frame, top, th);
    cache.live = Some(LiveFrame {
        frame_top_row: top,
        term_w: tw,
        term_h: th,
        frame: frame.clone(),
    });
    cache.screen_bottom = th;
    cache.caret = caret;
    s
}

/// Emit the caret move from the block's *first* row and return the absolute
/// `(row, col)` to store. The row is bookkeeping only — it is what row
/// accounting compares against a terminal, and it is never addressed.
fn push_caret(s: &mut String, frame: &Frame, top: usize, th: usize) -> (usize, usize) {
    let (r, c) = frame_caret(frame);
    // `CR` first: the paint left the cursor at the end of the block's first
    // row, and `CUF` is relative — so the column it walks from has to be known
    // rather than assumed.
    s.push('\r');
    cud(s, r);
    cuf(s, c);
    ((top + r).clamp(1, th), c)
}

/// Where the cursor rests once a block is painted: the frame's own caret, or
/// the block's last row column 0 for a frame without one (a dialog with no
/// caret, the input with the caret windowed off).
///
/// Both components are frame-relative, which is the point: they survive a
/// re-anchor, a resize, or any disagreement about the absolute row, because no
/// absolute row is involved. `cache.caret` is the absolute restatement, for
/// row accounting alone.
fn frame_caret(frame: &Frame) -> (usize, usize) {
    let h = frame.height();
    match frame.caret {
        Some((r, c)) if r < h => (r, c),
        _ => (h.saturating_sub(1), 0),
    }
}

// --- relative cursor moves -------------------------------------------------
//
// The frame is drawn from the cursor and never addressed (row_accounting §6b).
// A count of 0 emits nothing, so a caller can compute its walk unconditionally.
//
// `CUU` clamps at the top row and `CUD` at the bottom row — neither scrolls —
// so a walk that must stay inside a block is bounded by that block's height,
// and a walk that may leave it (the clear below) is bounded by the floor.

/// `CSI K` — erase from the cursor to the end of the row.
const EL: &str = "\x1b[K";

/// Append `EL` only while there is something to erase.
///
/// After a full-width row the cursor is *wrap-pending*, and `CSI K` would eat
/// the last written cell — so this is a rule about the terminal rather than
/// about any particular element, which is why it is shared instead of
/// duplicated wherever rows are produced. Element impls live outside this
/// module (`repl.rs`, `ui.rs`) and call it.
pub(crate) fn el_if_short(content: &mut String, term_w: usize) {
    if display_width(content) < term_w {
        content.push_str(EL);
    }
}

/// Absolute cursor position, `CSI {row};{col} H`, both 1-based.
///
/// The *only* addressing left in a frame apply, and it is confined to
/// [`DrawCache::reanchor`] — the one path where the cursor may have left the
/// live block and the block's row is the only way back to its rows. Everything
/// that paints, erases, or places a caret is a walk from the cursor, so a
/// tracked row that disagrees with the terminal costs a bookkeeping error and
/// never a wrong row on screen (row_accounting §6b).
fn cup(row: usize, col: usize) -> String {
    format!("\x1b[{row};{col}H")
}

// CSI {n} A — up `n` rows.
fn cuu(s: &mut String, n: usize) {
    if n > 0 {
        s.push_str(&format!("\x1b[{n}A"));
    }
}

// CSI {n} B — down `n` rows.
fn cud(s: &mut String, n: usize) {
    if n > 0 {
        s.push_str(&format!("\x1b[{n}B"));
    }
}

// CSI {n} C — right `n` columns.
fn cuf(s: &mut String, n: usize) {
    if n > 0 {
        s.push_str(&format!("\x1b[{n}C"));
    }
}

/// Walk the cursor from block row `from` to block row `to`.
fn move_rows(s: &mut String, from: usize, to: usize) {
    if to >= from {
        cud(s, to - from)
    } else {
        cuu(s, from - to)
    }
}

/// Erase `h` rows *downward* from the cursor's row, ending on the last of them.
fn erase_down(s: &mut String, h: usize) {
    for i in 0..h {
        s.push_str(EL);
        if i + 1 < h {
            cud(s, 1);
        }
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
    fn render_overlay_pads_short_rows_before_reset() {
        let rows = vec!["ab".to_string()];
        let els = [Element::overlay(rows, 0)];
        let f = render(&els, &[1], 5);
        // Padding lands inside the styled span so backgrounds reach the edge.
        assert_eq!(f.row(0), "ab   ");
        // Full-width row passes through untouched.
        let els = [Element::overlay(vec!["abcde".to_string()], 0)];
        let f = render(&els, &[1], 5);
        assert_eq!(f.row(0), "abcde");
    }

    #[test]
    fn render_overlay_windows_around_focus() {
        let rows: Vec<String> = (0..20).map(|i| format!("item {i}")).collect();
        let els = [Element::overlay(rows, 10)];
        let f = render(&els, &[4], 20);
        assert_eq!(f.height(), 4);
        // Centered: focus 10 with 4 visible → start 8 (prefer h/2 = 2).
        assert_eq!(f.row(0).trim_end(), "item 8");
        assert_eq!(f.row(3).trim_end(), "item 11");
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
            Element::overlay(vec!["a".to_string(), "b".to_string()], 0).extent(),
            Extent { must: 1, might: 2 },
            "an overlay always keeps a row and wants its painted height"
        );
        assert_eq!(
            Element::overlay(Vec::new(), 0).extent(),
            Extent { must: 0, might: 0 },
            "nothing to show asks for nothing"
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
            Element::overlay(vec!["one".to_string(), "two".to_string()], 0),
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
        assert_eq!(f.row(3).trim_end(), "two");
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

#[cfg(test)]
mod apply_tests {
    //! Apply-phase tests (plan §4). They drive [`plan_apply`]'s bytes through a
    //! real vt100 emulator — the same screen/caret checks the REPL's `redraw`
    //! tests use — so the cache transitions and the emitted bytes are pinned
    //! together.
    use super::*;
    use crate::repl::LineEditor;
    use vt100::Parser;

    /// The tty's OPOST/ONLCR: `plan_apply` emits bare `\n` (the line discipline
    /// maps it to CRLF before the emulator sees it).
    fn onlcr(s: &str) -> Vec<u8> {
        s.replace('\n', "\r\n").into_bytes()
    }

    /// A fresh emulator with the cursor `n` rows down, as a run of content
    /// lines leaves it.
    ///
    /// This is how a test now says "put the block at row n": a frame apply is
    /// relative to the cursor (row_accounting §6b), so the cursor has to be
    /// there first — and in production it gets there by content writing lines,
    /// which is what these newlines stand in for.
    fn at(rows: usize, cols: usize, n: usize) -> Parser {
        let mut p = Parser::new(rows as u16, cols as u16, 2000);
        p.process(&onlcr(&"\n".repeat(n)));
        p
    }

    /// Feed one more chunk to an existing emulator.
    fn feed(p: &mut Parser, chunk: &str) {
        p.process(&onlcr(chunk));
    }

    /// Screen contents as trimmed strings, one per row.
    fn screen(p: &Parser, rows: usize, cols: usize) -> Vec<String> {
        let s = p.screen();
        (0..rows)
            .map(|r| {
                (0..cols)
                    .map(|c| {
                        s.cell(r as u16, c as u16)
                            .map(|cell| cell.contents())
                            .unwrap_or(" ")
                            .chars()
                            .next()
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Put the tracked cursor on 1-based `row` — where a run of content lines
    /// would have left it.
    ///
    /// The frame path's block now follows the cursor, so this is how a test says
    /// "put the block at row n". It is `note_lines` rather than a test-only
    /// setter because that is the real mechanism: in production the cursor gets
    /// there by content writing lines, and a test seam that skipped that would
    /// not be testing the path anything takes.
    fn cursor_to(cache: &mut DrawCache, row: usize, th: usize) {
        cache.note_lines(row.saturating_sub(1), th);
    }

    /// A frame of fixed text rows (self-contained: `Lines` appends the `EL`).
    fn lines_frame(rows: &[&str], tw: usize) -> Frame {
        let rows: Vec<String> = rows.iter().map(|s| s.to_string()).collect();
        let h = rows.len();
        render(&[Element::lines(rows)], &[h], tw)
    }

    /// The parameters of every `CSI … H` (CUP) in a planned byte string.
    ///
    /// Scans for the real escape sequence rather than looking for an `H`, so
    /// frame *content* containing a letter `H` cannot pass for an address.
    fn cups(s: &str) -> Vec<String> {
        let b: Vec<char> = s.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == '\x1b' && b.get(i + 1) == Some(&'[') {
                let start = i + 2;
                let mut j = start;
                while j < b.len() && (b[j].is_ascii_digit() || b[j] == ';') {
                    j += 1;
                }
                if b.get(j) == Some(&'H') {
                    out.push(b[start..j].iter().collect());
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    /// A3.1's invariant: **a frame apply never addresses a row.** Every path
    /// — first paint, in-place repaint, shrink, scroll, the content-write
    /// clear, and the append — paints, erases and places its caret by walking
    /// from the cursor, so a tracked row that disagrees with the terminal costs
    /// a bookkeeping error and never a wrong row on screen
    /// (row_accounting §6b).
    #[test]
    fn no_apply_addresses_a_row() {
        let mut cache = DrawCache::new(24);
        let f3 = lines_frame(&["aa", "bb", "cc"], 20);
        let mut plans = Vec::new();
        // First paint, from the cursor.
        cursor_to(&mut cache, 5, 24);
        plans.push(plan_apply(&mut cache, &f3, ApplyPolicy::Frame, 20, 24));
        // In-place repaint of one row.
        let f3b = lines_frame(&["aa", "XX", "cc"], 20);
        plans.push(plan_apply(&mut cache, &f3b, ApplyPolicy::Frame, 20, 24));
        // Shrink: the orphaned rows are erased by a walk, not by addressing.
        let f1 = lines_frame(&["aa"], 20);
        plans.push(plan_apply(&mut cache, &f1, ApplyPolicy::Frame, 20, 24));
        // A frame too tall for the screen, so the apply must scroll.
        let mut tall = DrawCache::new(10);
        cursor_to(&mut tall, 8, 10);
        plans.push(plan_apply(
            &mut tall,
            &lines_frame(&["a", "b", "c", "d", "e"], 20),
            ApplyPolicy::Frame,
            20,
            10,
        ));
        // Append, which writes the rows as content.
        let mut app = DrawCache::new(24);
        cursor_to(&mut app, 5, 24);
        plans.push(plan_apply(&mut app, &f3, ApplyPolicy::Append, 20, 24));
        for p in &plans {
            assert!(cups(p).is_empty(), "apply addressed a row: {p:?}");
        }
    }

    /// The other half of the invariant: the re-anchor *is* allowed to address,
    /// and must keep doing so — a cursor that left the block puts its rows out
    /// of reach of every relative walk, so the erase that precedes every paint
    /// would clear the wrong rows (row_accounting §6b).
    #[test]
    fn reanchor_is_the_one_addressing_path() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 20);
        cursor_to(&mut cache, 5, 24);
        let mut p = at(24, 20, 4);
        feed(
            &mut p,
            &plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24),
        );
        let (_, clear) = cache.reanchor(Some(9), 20, 24);
        // Erase the block's rows where they are, then park on the new row.
        assert_eq!(cups(&clear), vec!["5;1", "6;1", "9;1"], "{clear:?}");
        // And with no frame live it is only the cursor park — which is why it is
        // safe to run on every `cursor_row()` call, including mid-stream.
        let mut idle = DrawCache::new(24);
        let (_, park) = idle.reanchor(Some(7), 20, 24);
        assert_eq!(cups(&park), vec!["7;1"], "{park:?}");
    }

    /// **After a reflow the area is derived from the cursor, not from its own
    /// stale absolute row.**
    ///
    /// `frame_top_row` was recorded before the resize, so the terminal has
    /// reflowed since and it addresses a row the area no longer occupies. The
    /// cursor is the only thing that survived, so the clear walks up from it and
    /// never addresses a row outright — which is what stops the area sliding one
    /// row further down on every re-anchor.
    #[test]
    fn reanchor_after_a_reflow_walks_from_the_cursor() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 20);
        cursor_to(&mut cache, 5, 24);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        assert_eq!(cache.live.as_ref().map(|l| l.frame_top_row), Some(5));

        // A *vertical* resize: the terminal shrank, everything reflowed, and the
        // cursor — which sat on the area's last row — now reports row 9.
        let (row, bytes) = cache.reanchor(Some(9), 20, 10);

        assert_eq!(row, 9, "the cursor's row is still the cursor's row");
        // No absolute address to the old rows: the only CUP left is the final
        // park, which is the sanctioned one.
        assert_eq!(
            cups(&bytes),
            vec!["9;1"],
            "a reflowed area is cleared relatively, not addressed: {bytes:?}"
        );
        // `live` survives, and its top is re-derived as `cursor - offset`. This
        // frame has no caret of its own, so it rests on its last row: offset 1.
        assert_eq!(
            cache.live.as_ref().map(|l| l.frame_top_row),
            Some(8),
            "the area's top follows the cursor instead of drifting down"
        );
        // The applied size stays behind, so the partial-apply predicate still
        // forces a full re-apply rather than trusting a frame laid out for the
        // old height.
        let l = cache.live.as_ref().expect("live survives a reflow");
        assert_eq!(
            (l.term_w, l.term_h),
            (20, 24),
            "the applied size is what it was"
        );
    }

    /// A **narrowing** resize wraps the area's rows, so clearing one row per line
    /// leaves remnants on the wrapped continuations.
    #[test]
    fn reanchor_after_a_narrowing_resize_clears_wrapped_rows() {
        let lines = 2;
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 40);
        cursor_to(&mut cache, 5, 24);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 40, 24);

        // 40 columns down to 20: every line occupies two terminal rows now, so
        // the area's two lines span four.
        let (_, bytes) = cache.reanchor(Some(9), 20, 10);
        assert_eq!(
            bytes.matches(EL).count(),
            lines * 2,
            "a halved width means two terminal rows per line: {bytes:?}"
        );

        // A pure height change does not wrap, so it still clears one row per line.
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 20);
        cursor_to(&mut cache, 5, 24);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let (_, bytes) = cache.reanchor(Some(9), 20, 10);
        assert_eq!(
            bytes.matches(EL).count(),
            lines,
            "a vertical-only resize does not wrap: {bytes:?}"
        );
    }

    /// The regression this exists for: the area must not walk down the screen by
    /// one row per re-anchor. Before the fix `caret` was left on the cursor's
    /// row — the area's *last* — with `live` dropped, so the next apply anchored
    /// there and the next re-anchor measured from the new one.
    #[test]
    fn repeated_reflow_reanchors_do_not_drift() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 20);
        cursor_to(&mut cache, 5, 24);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);

        let mut tops = Vec::new();
        for height in (12..=20).rev() {
            let row = cache.caret.0;
            let (seen, _) = cache.reanchor(Some(row), 20, height);
            assert_eq!(seen, row, "a re-anchor leaves the cursor where it was");
            tops.push(
                cache
                    .live
                    .as_ref()
                    .map(|l| l.frame_top_row)
                    .expect("a reflowed area survives to be re-anchored"),
            );
        }
        assert!(
            tops.windows(2).all(|w| w[0] == w[1]),
            "the area's top must not move on a pure re-anchor: {tops:?}"
        );
    }

    #[test]
    fn full_paints_rows_and_clears_below() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[4], "one");
        assert_eq!(rows[5], "two");
        assert_eq!(rows[6], "");
        // The frame region is now live at rows 5..6.
        assert_eq!(cache.anchor(), 5);
        // No caret in the frame → rest at the frame's bottom-left (1-based 6).
        assert_eq!(p.screen().cursor_position(), (5, 0));
    }

    #[test]
    fn full_clears_stale_rows_below() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        // Seed stale content the frame must erase, then put the cursor back on
        // row 5: the frame applies relative to wherever the cursor is.
        let seed = "\x1b[8;1Hstale8\x1b[9;1Hstale9\x1b[5;1H";
        let mut p = at(24, 20, 4);
        feed(&mut p, seed);
        feed(&mut p, &s);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[7], "");
        assert_eq!(rows[8], "");
    }

    #[test]
    fn partial_repaints_only_changed_rows() {
        let mut cache = DrawCache::new(24);
        let f1 = lines_frame(&["aa", "bb", "cc"], 20);
        cursor_to(&mut cache, 5, 24);
        let s1 = plan_apply(&mut cache, &f1, ApplyPolicy::Frame, 20, 24);
        let f2 = lines_frame(&["aa", "XX", "cc"], 20);
        let s2 = plan_apply(&mut cache, &f2, ApplyPolicy::Frame, 20, 24);
        assert!(s2.contains("XX"), "changed row repainted");
        assert!(!s2.contains("aa"), "unchanged rows skipped");
        let mut p = at(24, 20, 4);
        feed(&mut p, &s1);
        feed(&mut p, &s2);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[4], "aa");
        assert_eq!(rows[5], "XX");
        assert_eq!(rows[6], "cc");
    }

    #[test]
    fn partial_clears_orphaned_row_on_shrink() {
        let mut cache = DrawCache::new(24);
        let f1 = lines_frame(&["aa", "bb", "cc"], 20);
        cursor_to(&mut cache, 5, 24);
        let s1 = plan_apply(&mut cache, &f1, ApplyPolicy::Frame, 20, 24);
        let f2 = lines_frame(&["aa"], 20);
        let s2 = plan_apply(&mut cache, &f2, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s1);
        feed(&mut p, &s2);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[4], "aa");
        assert_eq!(rows[5], "");
        assert_eq!(rows[6], "");
    }

    /// The caret must follow the **new** frame, not the one it replaces.
    ///
    /// A frame that grows a row below the caret — Shift+Enter, an editor
    /// re-wrap — used to leave the cursor on the previous frame's caret row, so
    /// the next keystroke painted the *second* row where the first one was and
    /// the first row's text vanished. The pty test
    /// `shift_enter_inserts_newline_without_kitty` catches it end to end, but
    /// nothing in this module could, so it is pinned here in the three applies
    /// that actually happen: type, Shift+Enter, type again.
    #[test]
    fn a_frame_that_grows_below_the_caret_takes_the_caret_with_it() {
        let mut e = LineEditor::new();
        let input = |e: &LineEditor, rows: &[usize]| {
            render(
                &[Element::input(e.layout(20, "> "), "> ".to_string())],
                rows,
                20,
            )
        };
        let mut cache = DrawCache::new(24);
        e.insert_str("abc");
        cursor_to(&mut cache, 5, 24);
        let s1 = plan_apply(&mut cache, &input(&e, &[1]), ApplyPolicy::Frame, 20, 24);
        // Shift+Enter: a second row appears and the caret moves onto it. The
        // apply needs no row from anyone — it follows the live frame.
        e.insert_newline();
        let s2 = plan_apply(&mut cache, &input(&e, &[2]), ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s1);
        feed(&mut p, &s2);
        assert_eq!(screen(&p, 24, 20)[4], "> abc", "first row survives");
        assert_eq!(
            p.screen().cursor_position(),
            (5, 0),
            "the caret follows the caret onto the new row"
        );
        // The next keystroke lands on the new row, and the first keeps its text.
        e.insert_str("d");
        let s3 = plan_apply(&mut cache, &input(&e, &[2]), ApplyPolicy::Frame, 20, 24);
        feed(&mut p, &s3);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[4], "> abc");
        assert_eq!(rows[5], "d");
    }

    #[test]
    fn scrolls_frame_into_view() {
        let mut cache = DrawCache::new(10);
        let f = lines_frame(&["a", "b", "c"], 20);
        // anchor 9 + 3 rows = row 11 > 10 → delta 1.
        cursor_to(&mut cache, 9, 10);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        let mut p = at(10, 20, 8);
        feed(&mut p, &s);
        let rows = screen(&p, 10, 20);
        assert_eq!(rows[7], "a");
        assert_eq!(rows[8], "b");
        assert_eq!(rows[9], "c");
        assert_eq!(cache.anchor(), 8);
    }

    #[test]
    fn grow_preserves_revealed_rows_below_old_bottom() {
        // screen_bottom was 24; terminal is now 40 with no apply in between.
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["head"], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 40);
        // 19 rows below the block (6..=24) plus the painted row's own `EL` —
        // and not the 35 down to 40, which is what a clear to the new bottom
        // would have been. The absolute path expressed the same floor as
        // "does not touch row 25".
        assert_eq!(
            s.matches("\x1b[K").count(),
            20,
            "clear must stop at the old bottom (24): {s:?}"
        );
        let seed = "\x1b[25;1Hkeep25\x1b[30;1Hkeep30\x1b[5;1H";
        let mut p = at(40, 20, 4);
        feed(&mut p, seed);
        feed(&mut p, &s);
        let rows = screen(&p, 40, 20);
        assert_eq!(rows[24], "keep25");
        assert_eq!(rows[29], "keep30");
    }

    #[test]
    fn reanchor_to_new_row_clears_old_rows() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 20);
        cursor_to(&mut cache, 5, 24);
        let s1 = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        // A foreign writer moved the cursor to row 8. The re-anchor both records
        // where it went and erases the block's old rows — the one absolute
        // address in a frame apply, and the reason it exists: from row 8 a
        // relative walk cannot reach rows 5 and 6 at all.
        let (_row, clear) = cache.reanchor(Some(8), 20, 24);
        // The re-anchor's answer is now *only* bookkeeping — the apply reads the
        // cache, not a caller's row (A3.2).
        let s2 = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s1);
        feed(&mut p, &clear);
        feed(&mut p, &s2);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[4], "");
        assert_eq!(rows[5], "");
        assert_eq!(rows[7], "one");
        assert_eq!(rows[8], "two");
        // A block painted *over* the old one would leave nothing to assert: the
        // erase is what makes rows 5 and 6 blank rather than overwritten.
        assert_eq!(p.screen().cursor_position(), (8, 0));
    }

    #[test]
    fn width_change_forces_full_repaint() {
        let mut cache = DrawCache::new(24);
        let f1 = lines_frame(&["aa"], 20);
        cursor_to(&mut cache, 5, 24);
        let _ = plan_apply(&mut cache, &f1, ApplyPolicy::Frame, 20, 24);
        let f2 = lines_frame(&["aa"], 20);
        let s2 = plan_apply(&mut cache, &f2, ApplyPolicy::Frame, 40, 24);
        assert!(s2.contains("aa"), "width change repaints the row");
        let mut p = at(24, 20, 4);
        feed(&mut p, &s2);
    }

    #[test]
    fn append_writes_content_and_advances_caret() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["div", "line"], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Append, 20, 24);
        assert!(s.ends_with('\n'), "append ends on a fresh line");
        let mut p = at(24, 20, 4);
        feed(&mut p, &s);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[4], "div");
        assert_eq!(rows[5], "line");
        // live cleared → next frame anchors at the cursor (top + height = 7).
        assert_eq!(cache.anchor(), 7);
        assert_eq!(p.screen().cursor_position(), (6, 0));
    }

    /// §4.5 P1: content displaces the frame, and the frame comes back **below**
    /// the content with the content intact.
    ///
    /// The whole case in one test, because the interesting failure is a compound
    /// one: a reposition that erases the content it just made room for looks
    /// correct on the frame's own rows and destroys the transcript.
    ///
    /// Note the `cursor_to` calls. A frame apply anchors on the *cache's* row
    /// while the emulator is wherever the test put it, and a mismatch shows up
    /// as a frame painted in the wrong place for reasons that have nothing to do
    /// with the code under test — which is how the first draft of this test came
    /// to assert a frame at row 0 and call it row 5.
    #[test]
    fn reposition_moves_the_frame_below_the_content_without_erasing_it() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["bar", "> "], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s);
        let rows = screen(&p, 24, 20);
        assert_eq!(
            rows[4], "bar",
            "the frame starts below the content: {rows:?}"
        );
        assert_eq!(rows[5], ">");

        // Three content rows are written where the frame used to be. In
        // production this is `with_content_writer`: the bytes go out and
        // `note_lines` moves the caret, so the rows the frame occupied now hold
        // content and the frame is gone from the screen.
        feed(&mut p, "one\r\ntwo\r\nthree\r\n");
        cache.caret = (cache.caret.0 + 3, 0);
        cache.live = None; // the content write retired it, as today
        assert_eq!(
            p.screen().cursor_position(),
            (8, 0),
            "the content advanced it"
        );

        // The frame comes back, at the new fold — with the content above it.
        let s = plan_reposition(&mut cache, &f, 20, 24);
        feed(&mut p, &s);
        let rows = screen(&p, 24, 20);
        assert_eq!(
            rows[5], "one",
            "the content the frame vacated survives: {rows:?}"
        );
        assert_eq!(rows[6], "two");
        assert_eq!(rows[7], "three");
        assert_eq!(rows[8], "bar", "the frame is below the content: {rows:?}");
        assert_eq!(rows[9], ">");
        assert_eq!(cache.live.as_ref().unwrap().frame_top_row, 9);
    }

    /// …and it is relative. A `CUP` here would be the one absolute address in
    /// the steady state, and §4.5's I4 is the invariant it would break.
    #[test]
    fn reposition_addresses_no_row() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["bar", "> "], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s);
        cache.caret = (cache.caret.0 + 3, 0);
        cache.live = None;
        let s = plan_reposition(&mut cache, &f, 20, 24);
        assert!(
            cups(&s).is_empty(),
            "a reposition must not address a row: {s:?}"
        );
    }

    /// **NOT the model — see `ui_draw_v2` §4/§4.5.7.** This pins the area to
    /// the screen's last row, which the design explicitly rules out: the area
    /// follows the content and only *appears* to sit at the bottom once the
    /// screen is full. Renaming/reworking this test is part of the §5.2 change.
    /// The frame has to fit: when the content has pushed it past the bottom
    /// row, the same LFs scroll and the frame lands flush at the bottom.
    #[test]
    fn reposition_scrolls_so_the_frame_fits_at_the_bottom() {
        let mut cache = DrawCache::new(10);
        let f = lines_frame(&["bar", "> "], 20);
        cursor_to(&mut cache, 8, 10);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        let mut p = at(10, 20, 7);
        feed(&mut p, &s);
        // One more content row puts the cursor on the bottom row.
        feed(&mut p, "x\r\n");
        cache.caret = (10, 0);
        cache.live = None;
        let s = plan_reposition(&mut cache, &f, 20, 10);
        feed(&mut p, &s);
        let rows = screen(&p, 10, 20);
        assert_eq!(rows[8], "bar", "the frame fits at the bottom: {rows:?}");
        assert_eq!(rows[9], ">");
        assert_eq!(cache.live.as_ref().unwrap().frame_top_row, 9);
    }

    /// The rows below a repositioned frame are clean **without** being cleared,
    /// which is the omission P1's `plan_reposition` documents. This is the test
    /// that holds that claim up — `no EL emitted` for the erase, and the screen
    /// row that would have held a leftover to confirm it.
    #[test]
    fn the_rows_below_a_repositioned_frame_are_clean_without_being_cleared() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["bar", "> "], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s);
        feed(&mut p, "one\r\ntwo\r\nthree\r\n");
        cache.caret = (cache.caret.0 + 3, 0);
        cache.live = None;
        let s = plan_reposition(&mut cache, &f, 20, 24);
        // Not `!s.contains(EL)`: a `Lines` row carries its own trailing `EL` to
        // pad to the terminal width, so that check could never be about the
        // clear. A clear-below is a *walk* — `CUD` down past the block and `CUU`
        // back — so one `CUD`, the caret move, is the whole budget.
        assert_eq!(
            s.matches("\x1b[1B").count(),
            1,
            "only the caret moves down; no clear-below walk: {s:?}"
        );
        feed(&mut p, &s);
        let rows = screen(&p, 24, 20);
        assert_eq!(
            rows[10], "",
            "and the row below the new frame is blank: {rows:?}"
        );
    }

    /// §4.5 P1: consume leaves the cursor at the frame's first row, **column
    /// 0**, and clears the rows it is taking over.
    ///
    /// The column is the substance. `CUU`/`CUD` preserve it, the frame's caret
    /// is out at the input's column, and the content about to be written would
    /// start a prompt-width to the right. This makes it the plan's own
    /// postcondition rather than something inherited from the transient bar's
    /// trailing `CR`.
    ///
    /// The clear is what "consume" is *not*: it takes ownership, not pixels.
    /// The frame's rows are padded to full width, so a content line shorter
    /// than the terminal would otherwise strand whatever they painted to its
    /// right of it — the other half of the §1.1 resume artefact. The frame is
    /// still handed back, so it can be re-emitted below.
    #[test]
    fn consume_walks_to_the_frame_top_at_column_zero_and_clears_its_rows() {
        let mut cache = DrawCache::new(24);
        let mut e = LineEditor::new();
        e.insert_str("hi there");
        let layout = e.layout(20, "> ");
        let h = layout.row_count.max(1);
        let f = render(&[Element::input(layout, "> ".into())], &[h], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s);
        // The caret is on the input row, out at the prompt's width.
        // The caret is out in the input, not at column 0 — which is the whole
        // reason the walk needs its own `CR`. The exact column is the layout's
        // business (prompt marker + buffer), so assert the property.
        assert_eq!(cache.caret.0, 5, "the frame's caret row: {:?}", cache.caret);
        assert!(
            cache.caret.1 > 1,
            "and a column out in the input, which is why consume needs a `CR`: {:?}",
            cache.caret
        );
        let before = screen(&p, 24, 20);
        assert_eq!(before[4], "> hi there");

        let (walk, taken) = plan_consume(&mut cache).expect("a live frame");
        assert_eq!(taken.height(), f.height(), "the same frame comes back");
        assert!(
            walk.contains(EL),
            "the rows it takes over are cleared: {walk:?}"
        );
        feed(&mut p, &walk);
        assert_eq!(
            p.screen().cursor_position(),
            (4, 0),
            "the fold, at column 0 — not the input's column"
        );
        let after = screen(&p, 24, 20);
        assert_eq!(
            after[4], "",
            "the row it took over is clear to the end of the line: {after:?}"
        );
        assert_ne!(
            after, before,
            "and so the frame no longer stands as painted"
        );
        assert!(cache.live.is_none(), "it is no longer intact");
    }

    /// The whole of a turn today: no frame live, so there is nothing to consume
    /// and no walk to emit. This is why the clear `plan_consume` replaces was
    /// almost always a no-op, and it is the shape P1's wiring has to tolerate.
    #[test]
    fn consume_is_a_no_op_with_no_frame_live() {
        let mut cache = DrawCache::new(24);
        cursor_to(&mut cache, 9, 24);
        assert!(plan_consume(&mut cache).is_none());
        assert_eq!(cache.caret, (9, 0), "the caret is untouched");
    }

    /// Consume then reposition, with content in between: the round trip the
    /// content path performs, with the content written by hand so the test says
    /// what it is standing in for.
    #[test]
    fn consume_then_content_then_reposition_puts_the_frame_back_below() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["bar", "> "], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s);

        let (walk, taken) = plan_consume(&mut cache).expect("a live frame");
        feed(&mut p, &walk);
        // Two content rows, written at the fold — the bytes overwrite the frame.
        feed(&mut p, "one\r\ntwo\r\n");
        cache.caret = (cache.caret.0 + 2, 0);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[4], "one", "the content landed on the frame: {rows:?}");

        let s = plan_reposition(&mut cache, &taken, 20, 24);
        feed(&mut p, &s);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[4], "one", "the content survives: {rows:?}");
        assert_eq!(rows[5], "two");
        assert_eq!(rows[6], "bar", "and the frame is back below it: {rows:?}");
        assert_eq!(rows[7], ">");
        assert_eq!(cache.live.as_ref().unwrap().frame_top_row, 7);
    }

    #[test]
    fn clamps_anchor_to_screen() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["x"], 20);
        cursor_to(&mut cache, 999, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 23);
        feed(&mut p, &s);
        assert_eq!(screen(&p, 24, 20)[23], "x");
    }

    #[test]
    fn places_caret_for_input_frame() {
        let mut cache = DrawCache::new(24);
        let mut e = LineEditor::new();
        e.insert_str("hi");
        let layout = e.layout(20, "> ");
        let f = render(&[Element::input(layout, "> ".to_string())], &[1], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        let mut p = at(24, 20, 4);
        feed(&mut p, &s);
        // "> hi" at 1-based row 5; caret after "hi" → col 4.
        assert_eq!(p.screen().cursor_position(), (4, 4));
        assert_eq!(cache.anchor(), 5);
    }

    #[test]
    fn note_lines_advances_and_invalidates() {
        let mut cache = DrawCache::new(10);
        let f = lines_frame(&["x"], 20);
        cursor_to(&mut cache, 5, 10);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        assert_eq!(cache.anchor(), 5);
        cache.note_lines(3, 10);
        assert_eq!(cache.caret.0, 8);
        assert_eq!(cache.anchor(), 8, "live frame invalidated");
        cache.note_lines(100, 10);
        assert_eq!(cache.caret.0, 10, "clamped to the screen");
    }

    #[test]
    fn reanchor_sets_row_and_invalidates() {
        let mut cache = DrawCache::new(10);
        let f = lines_frame(&["x"], 20);
        cursor_to(&mut cache, 5, 10);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        assert_eq!(cache.reanchor(Some(3), 20, 10).0, 3);
        assert_eq!(cache.anchor(), 3);
    }

    /// Plan §9 latent issue 2: a DSR *timeout* is still a re-anchor — the
    /// tracked row stands (nothing better is known), but the live frame must
    /// be invalidated all the same ("any uncertainty → clear it"), so the
    /// next apply takes the full path instead of a partial diff against a
    /// screen that may have changed under us.
    #[test]
    fn reanchor_fallback_keeps_row_but_invalidates() {
        let mut cache = DrawCache::new(10);
        let f = lines_frame(&["x"], 20);
        cursor_to(&mut cache, 5, 10);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        assert_eq!(cache.reanchor(None, 20, 10).0, 5, "tracked row stands");
        assert!(cache.live.is_none(), "live frame must be invalidated");
        // Identical frame, same anchor: the partial path would emit nothing
        // but the caret; the full path clears the region below the frame.
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        assert!(s.contains("\x1b[K"), "full path required: {s:?}");
    }

    /// Ports the old `Repl::next_input_row` cases (folded into the apply
    /// phase by ui_draw_v2 P3): a frame that overflows the bottom scrolls
    /// itself into view and the cache tracks the scrolled top; the next,
    /// shorter frame anchors at that top and the screen never scrolls back
    /// down on its own.
    #[test]
    fn frame_tracks_scroll_then_stays_on_shrink() {
        let mut cache = DrawCache::new(40);
        let tall = lines_frame(&["a", "b", "c", "d"], 20);
        // anchor 38 + 4 rows - 1 = 41 > 40 → delta 1, top 37.
        cursor_to(&mut cache, 38, 40);
        let s1 = plan_apply(&mut cache, &tall, ApplyPolicy::Frame, 20, 40);
        assert_eq!(cache.anchor(), 37, "scroll must be tracked");
        // The next redraw follows the live frame: no scroll, same top — a
        // shrinking frame leaves the region where the tallest one put it. No
        // caller names a row (A3.2).
        let short = lines_frame(&["a"], 20);
        let s = plan_apply(&mut cache, &short, ApplyPolicy::Frame, 20, 40);
        assert_eq!(cache.anchor(), 37, "shrink must not scroll back down");
        let mut p = at(40, 20, 37);
        feed(&mut p, &s1);
        feed(&mut p, &s);
        let rows = screen(&p, 40, 20);
        assert_eq!(rows[36], "a");
        assert_eq!(rows[37], "", "shrunken rows cleared in place");
    }

    /// Ports the old `Repl::clear_rows_to(40, 24) == ""` case: with the
    /// frame anchored below the previous screen bottom (bottom-anchored
    /// grow), the clear floor sits above the frame — nothing is cleared, so
    /// the scrollback a grow revealed stays intact.
    #[test]
    fn full_clears_nothing_when_frame_sits_below_the_old_bottom() {
        let mut cache = DrawCache::new(24); // screen_bottom = 24
        let f = lines_frame(&["here"], 20);
        cursor_to(&mut cache, 40, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 40);
        // floor = min(40, 24) = 24 < frame top: only the frame row's own
        // trailing `EL` is emitted — no clear addresses any other row.
        assert_eq!(
            s.matches("\x1b[K").count(),
            1,
            "only the painted row erases itself: {s:?}"
        );
        let mut p = at(40, 20, 39);
        feed(&mut p, &s);
        assert_eq!(screen(&p, 40, 20)[39], "here");
    }
}
