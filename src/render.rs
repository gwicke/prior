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
//!
//! Since 4d the manager is also content's only way onto the screen: the
//! content path hands each formatted block over as a [`ContentWrite`]
//! (`render_content` / `render_batch`), and nothing above the manager
//! writes bytes of its own; 4e made it batching's and reposition's home
//! too (the guard, the recipe). What remains of the design's shape comes
//! with the steps that give it meaning: the compose + content round trip
//! of `render(Some(w))` with the frame verbs (4f), and the element stack
//! (4h).

use std::ops::Range;
use std::sync::{Arc, Mutex};

use crate::draw::{el_if_short, plan_consume, plan_reposition, ApplyPolicy, Draw};
use crate::slashdot_menu::table::display_width;
use crate::termio::{CaretEvent, TermHandles, TermIO, TermSize};
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

/// A content write handed to the hub (design §Render): the bytes one block
/// produced, plus the facts needed to place them — the rows they occupy
/// (measured from the bytes by content's `RowCounter`, not predicted from
/// the source), the width they were rendered at (the reposition
/// re-renders at it), and the stream they ride. On the screen route
/// (`Main`) the bytes are complete, pre-wrapped, line-cleared rows ending
/// at a line boundary; on the `Meta` route they are raw (see
/// [`Self::stream`]).
///
/// Content-specific classification stops *above* this type: the only
/// vocabulary the design sends down with a write is `Stream` (main/meta),
/// which rides with every write since 4f-f. `Main` is every write through
/// the boundary chain — screen content, choreography included. `Meta` is
/// the reasoning hand-off when content routed reasoning to the meta sink
/// at setup — straight to `TermIO`, no choreography, byte-identical to
/// the stderr bypass this replaced.
pub(crate) struct ContentWrite {
    /// The block's bytes as Content produced them.
    pub(crate) bytes: Vec<u8>,
    /// Screen rows the bytes occupy at the watched width (a measured
    /// count: a long line wraps, which the byte length alone cannot say).
    /// Zero on the `Meta` route: stderr never moves the screen's cursor,
    /// so the rows are not the route's to report.
    pub(crate) rows: usize,
    /// The width the block was rendered for — the reposition's width.
    /// `Meta` carries the watched width too (honest wherever the payload
    /// is read); the reposition never runs for it.
    pub(crate) width: usize,
    /// Which stream these bytes ride — the stream decision is content's,
    /// made once at setup; below this the hub routes on the token and
    /// stays stream-generic (design §Render: `ContentWrite { lines,
    /// stream }`).
    pub(crate) stream: crate::termio::Stream,
}

// --- the frame hub (step 4b) -------------------------------------------------

/// The frame hub (design §Render layer): where a composition comes to be
/// budgeted, rendered, applied and remembered. Step 4b moved the pipeline
/// here — what was `Output::draw_elements`' default body — 4c moved the
/// drawing edge (`Draw`) under this roof, 4d made this the content
/// path's only door onto the screen (the `ContentWrite` hand-off below),
/// and 4e moved batching and reposition here with the recipe they re-
/// draw. What remains of the design's shape comes with the steps that
/// give it meaning: the frame verbs off the watch and the compose +
/// content round trip (4f), and the element stack (4h).
pub struct RenderManager {
    /// The drawing edge (step 4c): the byte sink and its live-screen
    /// bookkeeping, owned — the design's `Draw { … }` under this roof. Plain
    /// fields inside; this manager's `Mutex` is what serializes them.
    draw: Draw,
    /// The capture tap (step 4d, design §Content): while open, each
    /// [`ContentWrite`]'s bytes stop here instead of reaching the screen.
    /// Content bytes only — frame bytes can never be caught, which is the
    /// deliberate narrowing over the old writer swap.
    captured: Option<Vec<u8>>,
    /// The last composition, kept as its **recipe** — the element list, not
    /// the rendered rows it produced (ui_draw_v2 §5.1.1). [`Self::reposition_area`]
    /// re-draws this instead of recomposing: re-rendering is cheap and always
    /// current, where the rendered rows would be a snapshot of whatever the
    /// status said at the moment of compose.
    ///
    /// The hub's since 4e (it moved out of `DrawCache` with reposition): the
    /// recipe belongs to the composition `render` applied, so a direct
    /// [`Self::draw_frame`] — one no composition produced — clears it, and a
    /// later `render` overwrites it. Deliberately *not* retired by a
    /// consume or note: those retire the frame's *position*, and the recipe
    /// is exactly what survives it.
    recipe: Vec<Element>,
    /// The fraction of the frame's terminal height the area may occupy —
    /// [`configure`](Self::configure)'s, resolved against the render's
    /// height on every call.
    frame_max_row_ratio: f64,
    /// The watched geometry — the handle `TermIO` hands out at setup (design
    /// §3), cloned here so the manager owns it from the start. `render`
    /// paints at the caller's `dims` (their belief, §9 #3); the frame-size
    /// reads went elsewhere in 4f-c — `Content::frame_size` carries the same
    /// channel, and [`Self::painted_size`] reads through `draw`'s own clone —
    /// so nothing reads this field itself yet.
    #[allow(dead_code)]
    size: SizeWatch,
    /// The reader's caret-invalidation queue — the receiver half is this
    /// manager's alone since 4g-c (design §3: `caret` → "RenderManager
    /// only"). `Interactive::new` hands it over from the one `handles()`
    /// distribution; `None` until then (Plain never spawns a reader).
    caret: Option<tokio::sync::mpsc::UnboundedReceiver<CaretEvent>>,
}

impl RenderManager {
    /// A fresh hub over the watched geometry, with its own drawing edge. The
    /// budget ratio starts at [`FRAME_MAX_ROW_RATIO`] — the value the
    /// pipeline hard-coded before `configure` existed — so an unconfigured
    /// manager budgets exactly as the old code did; [`Self::configure`]
    /// changes it.
    pub fn new(size: SizeWatch, term_io: TermIO) -> Self {
        Self {
            draw: Draw::new(size.clone(), term_io),
            captured: None,
            recipe: Vec::new(),
            frame_max_row_ratio: FRAME_MAX_ROW_RATIO,
            size,
            caret: None,
        }
    }

    /// The caret channel, distributed once at setup (design §3): the
    /// `TermHandles.caret` receiver `Interactive::new` took from the one
    /// [`Self::handles`] hand-out moves in here — the manager is the event's
    /// only consumer.
    pub fn set_caret_receiver(&mut self, caret: tokio::sync::mpsc::UnboundedReceiver<CaretEvent>) {
        self.caret = Some(caret);
    }

    /// Drain the caret channel; a pending invalidation (SIGCONT / resume —
    /// the shell may have repainted, so the cached frame rows can no longer
    /// be trusted, plan §4.2) re-anchors the live-screen cache via DSR.
    /// Called at the prompt loop's top, where the reader-independent
    /// `RESUMED_REANCHOR` flag check used to be (4g-c killed the flag): the
    /// reader produces the event only after its own raw-mode re-entry, so
    /// raw mode is back by the time the DSR runs — and since the same
    /// reader thread sends the caret event *before* it forwards any later
    /// key, the loop cannot wake on a post-resume key with the invalidation
    /// still queued.
    pub fn poll_caret(&mut self) {
        let mut invalidated = false;
        if let Some(caret) = &mut self.caret {
            while let Ok(event) = caret.try_recv() {
                if matches!(event, CaretEvent::Invalidated) {
                    invalidated = true;
                }
            }
        }
        if invalidated {
            let _ = self.cursor_row();
        }
    }

    /// The channel handles, once at setup (design §3 step 1–2): where the
    /// entry-DSR window in `Interactive::new` takes the input receiver for
    /// the repl. Reached through here so the setup never names `TermIO`,
    /// which by then lives inside [`Self::draw`].
    pub fn handles(&mut self) -> TermHandles {
        self.draw.handles()
    }

    /// Whether this terminal flips raw mode at all (TermIO's Tty, not Plain)
    /// — what `$EDITOR` and the window pump ask before expecting a mode
    /// turnover. The predicate the 4f-d `mode_guard: Option<…>` field
    /// answered; since 4f-e the enum itself is the answer.
    pub fn is_tty(&self) -> bool {
        self.draw.is_tty()
    }

    /// Raw mode off — the front door (design §Render: "suspend() stops the
    /// reader and drops raw mode ($EDITOR, Ctrl-Z)"), pass-through to
    /// `Draw`/`TermIO` — the only real implementation is `TermIO::suspend`,
    /// complete since 4f-e-c: the reader-stop rides with the raw-mode drop.
    /// Idempotent; a no-op on Plain.
    pub fn suspend(&self) {
        self.draw.suspend();
    }

    /// Raw mode on — the reverse of [`Self::suspend`], and also the initial
    /// entry at prompt-loop startup. The design makes resume implicit in
    /// the next render; until the DSR answers inside `TermIO` (4g) callers
    /// re-enter explicitly. A no-op on Plain.
    pub fn resume(&self) {
        self.draw.resume();
    }

    /// Publish a settled size on the geometry watch (parked-key drains and
    /// the prompt loop's defensive `Resize` arms), through the `TermIO`
    /// that owns the channel since 4f-e-a.
    pub fn set_size(&self, size: TermSize) {
        self.draw.set_size(size);
    }

    /// Probe the terminal for its live size and publish it — the prompt
    /// loop's loop-top refresh, under `TermIO`'s roof since 4f-e-a. Returns
    /// the probed size so the caller can compare it against
    /// [`Self::painted_size`] without a second read.
    pub fn probe_size(&self) -> TermSize {
        self.draw.probe_size()
    }

    /// The drawing edge, for the test seams that read *past* this manager
    /// (`cache.applied`, and `term_io.recorded` for the byte edge since
    /// 4f-e-b). Production reaches the edge through this manager's own
    /// verbs — `render`, `cursor_row`, `painted_size`, `draw_frame`
    /// (content writes go through `render_content`); the last production
    /// forwarder was `Output::painted_size`, which 4f-c took. Test-only
    /// since.
    #[cfg(test)]
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
    /// The compose head is *not* here: the status row is content's facts
    /// (raw store + streaming flag), so the caller composes
    /// (`repl::compose_area`) and hands the stack in — the status row
    /// becomes an installed element in 4h and the head moves in with it.
    /// `pub` as the hub's face verb (step 4f-b): out-of-crate drivers (the
    /// confirm probe) apply frames through it directly.
    pub fn render(
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
        // Moved, never copied: the recipe belongs to the composition that
        // built it, and the hub is where reposition reads it (step 4e).
        // Written after the apply, so the recipe that described the *last*
        // paint is never the one a concurrent content write re-renders.
        self.recipe = stack;
        self.draw.cache.applied = Some(applied);
        applied
    }

    /// Re-anchor the live-screen cache to the real cursor (DSR): the query
    /// goes through `termio` — the sole stdin owner pauses around the reply
    /// (step 2 commit C), so it is answered *inside* the terminal layer as
    /// the design's §TermIO asks — and the answer syncs the draw cache.
    /// The frame verb's home since 4f-b; the `Output::cursor_row` forward
    /// that used to reach it died with the verb in 4g-a.
    pub(crate) fn cursor_row(&mut self) -> usize {
        let row = crate::termio::query_cursor_row();
        self.draw.reanchor_to_cursor(row)
    }

    /// The size the **last frame applied through this hub** took — the one
    /// number resize detection compares the terminal against (ui_draw_v2
    /// §9 #3), read from the applied dims `render`/`draw_frame` record. Before
    /// anything has been applied: the watched geometry (floored) — a caller
    /// that has not painted yet starts from what the terminal is, not from a
    /// size nobody drew at. `painted_size`'s home since 4f-c (what
    /// `Output::painted_size` forwarded); distinct from the dims a paint is
    /// *composed* for (`Content::frame_size`, the current size — what a
    /// dialog wants when it opens): that is a belief, this is a memory.
    pub(crate) fn painted_size(&self) -> (usize, usize) {
        self.draw.painted_size()
    }

    /// Apply a frame that no composition produced *through this hub* —
    /// `ui::resolve`'s empty frame, the direct `Output::draw_frame` paths,
    /// tests. Whatever recipe recorded itself no longer describes the
    /// screen, so it is cleared — the counterpart of [`Self::render`]
    /// storing its own right after. (The clear moved here from
    /// `Draw::draw_frame` with the recipe, step 4e.)
    pub(crate) fn draw_frame(
        &mut self,
        frame: &Frame,
        policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        self.recipe.clear();
        self.draw.draw_frame(frame, policy, dims)
    }

    // --- the content hand-off (step 4d, design §Content/§Render) -----------

    /// One content block on the streaming path — the hand-off the design
    /// spells `render(Some(w))` (§Render) once this manager owns the
    /// compose as well (4f/4h). Today the caller still composes
    /// separately, so this is the content half alone: every streaming
    /// write reaches the screen through here and never around it.
    pub(crate) fn render_content(&mut self, content: ContentWrite) {
        self.put_content(content);
    }

    /// One content block on the replay path (§Render): bulk writes, one
    /// per call inside a batch, with the frame's re-emit deferred to a
    /// final regular apply. The guard that opens such a batch is this
    /// module's too ([`ContentBatchGuard`], step 4e); the deferral itself
    /// is still the shared `batch_active` flag inside [`Self::put_content`]
    /// — the two verbs remain one choreography under two names until the
    /// design's final regular `render()` — compose + content in one call —
    /// replaces the guard's `end_batch` reposition (4f/4h).
    pub(crate) fn render_batch(&mut self, content: ContentWrite) {
        self.put_content(content);
    }

    /// Is a content batch open — its writes defer the frame's re-emit to
    /// the guard's drop? The read side of the flag, for the content
    /// hand-off to pick its verb.
    pub(crate) fn batching(&self) -> bool {
        self.draw.cache.batch_active
    }

    /// Raise the batch flag if none is open; `true` means the caller's guard
    /// owns the batch (and must [`Self::end_batch`] on drop). The hub's
    /// since 4e — batching defers *reposition*, and reposition is the hub's.
    ///
    /// Flat, not recursive: a nested open adds no state, because the outer
    /// batch already covers everything written inside it. So it collapses to
    /// `false` — only the guard that raised the flag ever clears it, which is
    /// what keeps a stray inner scope from ending the outer batch's
    /// repositioning early.
    ///
    /// The assert exists because that collapse is *silent*: `false` ends
    /// nothing, and someone who wrote an inner batch almost certainly meant an
    /// independent one. Loud where it is free (debug builds), and harmless
    /// where it is not (release keeps batching correctly) — rather than a
    /// missing redraw to chase at runtime.
    pub(crate) fn begin_batch(&mut self) -> bool {
        debug_assert!(
            !self.draw.cache.batch_active,
            "batches do not nest: this begin_batch is a no-op, because the \
             open batch above already covers everything written inside it"
        );
        if self.draw.cache.batch_active {
            return false;
        }
        self.draw.cache.batch_active = true;
        true
    }

    /// End the open batch: emit the deferred frame once, at this manager's
    /// watched width (ui_draw_v2 R5). Called by [`ContentBatchGuard`]'s
    /// drop; a no-op when no batch is open or nothing was deferred.
    pub(crate) fn end_batch(&mut self) {
        // The width the deferred frame re-renders at is this manager's own
        // watched geometry — the same channel `Content` formats at, with
        // the same 20-column floor as `Content::width` and the
        // `term_width` free function.
        let width = self.size.borrow().width.max(20);
        if !self.draw.cache.batch_active {
            return;
        }
        self.draw.cache.batch_active = false;
        let Some(taken) = self.draw.cache.deferred.take() else {
            return;
        };
        let bytes = self.reposition_area(width, taken);
        self.draw.write_bytes(bytes.as_bytes());
    }

    /// True while a capture is open on the content hand-off (design
    /// §Content: the tap reads each `ContentWrite` as handed over).
    pub(crate) fn is_capturing(&self) -> bool {
        self.captured.is_some()
    }

    /// Open a capture: from here each [`ContentWrite`]'s bytes stop at the
    /// hub instead of reaching the screen. Content bytes only — frame
    /// bytes (`draw_frame`, the reposition) can never be caught, which is
    /// the deliberate narrowing over the old writer swap; byte-edge tests
    /// read `TermIO::recording()`, which took that seam wholesale (4f-e-b).
    pub(crate) fn begin_capture(&mut self) {
        self.captured = Some(Vec::new());
    }

    /// End the capture and return what the content path handed over.
    pub(crate) fn end_capture(&mut self) -> Vec<u8> {
        self.captured
            .take()
            .expect("end_capture without begin_capture")
    }

    /// The area's re-emit: re-render the stored recipe (§5.1.1) and walk it
    /// to where the content write left off. Moved here from `Draw` in 4e —
    /// the hub owns reposition, and the recipe is the hub's.
    ///
    /// Re-render rather than replay `taken`: a `Frame` is a snapshot of the
    /// status as of the last compose, and the bar moves without the REPL
    /// composing — `set_status` at turn start, the dot at
    /// `begin_response`/`end_response`. The recipe is the thing that stays
    /// current, which is why the hub keeps it and not the rendered rows.
    ///
    /// `taken` is not a formality: it is the fallback when no recipe recorded
    /// itself — a frame applied without a composition behind it (a test, or
    /// a direct `draw_frame`). Falling back keeps the invariant that matters
    /// more than freshness, I5: an area that was consumed is always put
    /// back, so a half-consumed frame is never left stranded on screen.
    pub(crate) fn reposition_area(&mut self, width: usize, taken: Frame) -> String {
        let tw = width.max(1);
        let th = self.draw.size.borrow().height.max(1);
        // The consumed frame's height is the `old_h` of the below-frame clear
        // (`old_h - h_new - consumed_rows`) — read before `taken` moves into
        // the fallback below.
        let old_h = taken.height();
        let frame = if self.recipe.is_empty() {
            taken
        } else {
            let heights = {
                let extents: Vec<Extent> = self.recipe.iter().map(Element::extent).collect();
                // The hub's own budget (step 4e): the recipe re-renders at
                // the same knob `render` composes under, so a configured
                // ratio cannot make the two disagree.
                allocate(&extents, self.budget(th))
            };
            render(&self.recipe, &heights, tw)
        };
        // **A frame with no rows has nothing to put back.** `ui::resolve` clears
        // a dialog by applying `Frame::empty()`, and `plan_apply` records that
        // as a live frame of height 0 — so a content write right after a
        // resolve consumes a zero-height area. Repositioning it would ask
        // `plan_reposition` to paint nothing, which is a no-op at best and a
        // `debug_assert!(h > 0)` panic in a debug build at worst. Returning
        // empty is the honest answer: there is no area to re-emit, and the
        // tracked row already moved with the content that consumed it. The
        // accumulator goes with it — a stale count would misplace the *next*
        // reposition's below-frame clear.
        if frame.height() == 0 {
            self.draw.cache.consumed_rows = 0;
            return String::new();
        }
        plan_reposition(&mut self.draw.cache, &frame, old_h, tw, th)
    }

    /// The choreography every `Main`-stream content write takes (moved from
    /// `Content::with_content_writer` in 4d): consume the live frame, put
    /// the block down at the fold, advance the tracked cursor by its
    /// measured rows, and bring the frame back below — deferred while a
    /// batch is open, whose guard's drop emits it once (ui_draw_v2 R5).
    /// A `Meta` write takes the other route inside: `TermIO`'s meta sink,
    /// no choreography (4f-f).
    fn put_content(&mut self, content: ContentWrite) {
        let ContentWrite {
            bytes,
            rows,
            width,
            stream,
        } = content;
        // The meta route (4f-f): the meta sink, no choreography — no
        // consume, no cursor advance, no reposition, and no capture
        // interception (a capture taps *screen* content; diverted reasoning
        // was never in one — `reasoning_is_routed_by_the_output_layer`
        // pins that). Byte-identical to the stderr bypass it replaced:
        // the same renderer bytes, written and flushed on the meta stream.
        if stream == crate::termio::Stream::Meta {
            self.draw.write_meta(&bytes);
            return;
        }
        // **Consume, then write** (ui_draw_v2 §4.5 P1 / ui_draw_261001): the
        // walk goes ahead of the bytes — cursor movement, straight to the
        // writer, never through content's row counter — and the content
        // follows, already carrying its per-line clears. No erase with the
        // walk: each line clears the row it enters, and rows the content
        // never reaches are the reposition's `old_h` tail to clear in its
        // own pass.
        let (walk, taken) = match plan_consume(&mut self.draw.cache) {
            Some((walk, frame)) => (walk, Some(frame)),
            // No live area: the cursor is wherever the last write left it,
            // which is where the content belongs. Nothing to walk and no
            // `CR` to emit — the column is the content path's own
            // bookkeeping here, not something to reset.
            None => (String::new(), None),
        };
        if !walk.is_empty() {
            self.draw.write_bytes(walk.as_bytes());
        }
        // The tap (design §Content): with a capture open the block's bytes
        // stop here — exactly as Content produced them, content only. The
        // choreography runs either way: the cursor advance and the consume
        // are facts about the screen, and a capture does not pause it.
        match self.captured.as_mut() {
            Some(cap) => cap.extend_from_slice(&bytes),
            None => self.draw.write_bytes(&bytes),
        }
        // Zero rows means nothing moved, and a frame stays live.
        if rows > 0 {
            self.draw.note_lines(rows);
        }
        // The area comes back below what was just written — re-rendered
        // rather than replayed, so its status row is whatever it is *now*:
        // the turn's `set_status` and the dot's flip both happen after the
        // REPL composed. The consume's rows ride along for the below-frame
        // clear (`old_h - h_new - consumed_rows` in `paint_forward`),
        // accumulated across a batch's writes and taken by whichever
        // reposition follows.
        //
        // Inside a batch the reposition waits: the frame stays consumed for
        // the whole burst and the guard's drop emits it once (ui_draw_v2 R5).
        let taken = {
            let cache = &mut self.draw.cache;
            if taken.is_some() || cache.deferred.is_some() {
                cache.consumed_rows += rows;
            }
            match taken {
                Some(taken) if !cache.batch_active => Some(taken),
                Some(taken) => {
                    // Keep the *first* frame taken: it is `reposition_area`'s
                    // fallback when no recipe recorded itself, and a later
                    // write finds nothing to consume and so contributes none.
                    if cache.deferred.is_none() {
                        cache.deferred = Some(taken);
                    }
                    None
                }
                None => None,
            }
        };
        if let Some(taken) = taken {
            let bytes = self.reposition_area(width, taken);
            self.draw.write_bytes(bytes.as_bytes());
        }
    }
}

/// Ends a content batch when dropped — see [`RenderManager::begin_batch`].
/// The hub's since 4e: batching defers *reposition*, and reposition is the
/// hub's, so the guard names the hub rather than a `Content` (its step-3d
/// shape) or an `Output` ([`crate::output::BatchGuard`]). Both exist so that
/// forgetting to end a batch — which would suppress a reposition forever,
/// stranding a consumed area on screen — is unreachable rather than merely
/// documented.
pub struct ContentBatchGuard<'a> {
    /// `Some` when this guard owns the batch (raised the flag); `None` when
    /// it collapsed into an open one, whose guard owns it.
    manager: Option<&'a Arc<Mutex<RenderManager>>>,
}

impl<'a> ContentBatchGuard<'a> {
    /// Open a batch over `manager`: the flag goes up, and the returned
    /// guard's drop emits the deferred frame once. A nested open collapses
    /// (see [`RenderManager::begin_batch`]).
    pub(crate) fn open(manager: &'a Arc<Mutex<RenderManager>>) -> Self {
        if manager.lock().unwrap().begin_batch() {
            ContentBatchGuard {
                manager: Some(manager),
            }
        } else {
            ContentBatchGuard { manager: None }
        }
    }
}

impl Drop for ContentBatchGuard<'_> {
    fn drop(&mut self) {
        if let Some(manager) = self.manager {
            manager.lock().unwrap().end_batch();
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
        let manager = RenderManager::new(size.clone(), crate::termio::TermIO::null());
        assert_eq!(manager.budget(24), live_budget(24));
        assert_eq!(manager.budget(1), 1);

        let mut small = RenderManager::new(size, crate::termio::TermIO::null());
        small.configure(0.1);
        assert_eq!(small.budget(24), 3);
        assert_eq!(small.budget(1), 1, "never below one row");
    }

    /// `render` is the pipeline that left `Output::draw_elements`: it applies
    /// through the `Draw` it has owned since 4c and remembers the composition
    /// there (recipe + applied size, where the content path reads it).
    /// Frame bytes read from a recording `TermIO` (4f-e-b: `recording()`
    /// took the byte edge wholesale) — the manager's own capture taps the
    /// *content* hand-off only (4d), so a frame-byte assertion reads the
    /// byte edge.
    #[test]
    fn manager_render_paints_its_own_draw_and_remembers() {
        let size = crate::termio::fixed(TermSize {
            width: 40,
            height: 24,
        });
        let mut manager = RenderManager::new(size, crate::termio::TermIO::recording());

        let stack = vec![
            Element::lines(vec!["hello".to_string()]),
            Element::divider(),
        ];
        let applied = manager.render(stack, ApplyPolicy::Frame, (40, 24));
        let bytes = manager.draw.term_io.recorded(crate::termio::Stream::Main);

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
            manager.recipe.len(),
            2,
            "the recipe is remembered on the hub, where reposition reads it"
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
