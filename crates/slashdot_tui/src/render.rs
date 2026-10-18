//! Frame composition and the frame hub (plan `doc/ui_draw_v2.md` §3, design
//! `doc/261003_output_drawing_refactor.md` §Render layer). The pipeline is
//! compose → allocate → render → apply; this module owns the pure phases:
//!
//! * compose builds a list of [`Element`]s (the REPL draws
//!   `[Input, Divider, Overlay]`, dialogs `[Divider?, Overlay]`),
//! * [`allocate`] resolves each element's [`RowAsk`] against the frame
//!   height, then distributes the live-region budget — the manager's own,
//!   at most 2/3 of terminal rows by default — over the resolved floors
//!   and wants,
//! * [`render`] produces a [`Frame`] — a flat byte buffer with per-row
//!   extents and element spans.
//!
//! [`RenderManager`] is the hub that runs those phases and
//! owns the [`Draw`] it applies through. The pure phases themselves stay
//! pure and callable directly; the manager touches the terminal only
//! through its `Draw`. A frame's rows are self-contained: each carries the
//! escapes needed to repaint itself in place (trailing `EL` or padding to
//! `term_w`), so `apply` may rewrite any subset of rows unconditionally.
//!
//! The manager is also content's only way onto the screen: the
//! content path hands each formatted block over as a [`ContentWrite`]
//! (`render_content` / `render_batch`), and nothing above the manager
//! writes bytes of its own. It is batching's home (the guard, which defers
//! [`Draw`]'s reposition), with the design's shape complete:
//! [`RenderManager::configure`] is the membership verb — the owner of what
//! is on screen installs the element stack on each state transition — and
//! [`RenderManager::render`] prepares the frame from the *configured* stack
//! and hands it down with the optional [`ContentWrite`] to [`Draw::draw`],
//! which merges them (design §Draw). Nothing is handed in per paint:
//! membership changes on transitions, never at paint.

use std::ops::Range;
use std::sync::{Arc, Mutex};

use crate::ansi::{display_width, display_width_ansi, split_at_width};
use crate::draw::{el_if_short, ApplyPolicy, Draw};
use crate::termio::{CaretEvent, TermHandles, TermIO, TermSize};
use tokio::sync::watch;

/// The size watch handle (design §3): the manager reads its geometry from it,
/// so `render` composes and paints at the watched size — no caller-supplied
/// dims. Resize detection compares the watched size against what was painted
/// (ui_draw_v2 §9 #3).
pub type SizeWatch = watch::Receiver<TermSize>;

/// The fraction of the terminal height the frame area may occupy — the value
/// [`RenderManager::configure`] is seeded and wired with (design §Render:
/// the ratio is configuration, not a constant buried in the budget fn).
///
/// `f32`, the same width as the in-frame [`RowSpec::RatioAndNum`] ratio the
/// design sketch uses: both are rough preferences about a *share*, resolved
/// to integral rows either way, so there is no precision here worth a wider
/// type. The `f32` product `ceil`s identically to the exact `f64` one for
/// every height in `1..=500`, so the pinned `ceil(24·2/3) = 16` holds (see
/// `live_budget_caps_at_two_thirds`).
pub const FRAME_MAX_ROW_RATIO: f32 = 2.0 / 3.0;

/// Live-region budget for a terminal of `term_h` rows at the default ratio:
/// the frame may occupy at most this many rows; the rest of the screen is
/// guaranteed scrollback (plan §2). Matches the pop-up cap used throughout
/// v2.5 (`⌈2/3·term_h⌉`, at least 1).
///
/// The test twin's answer only: production budgets through
/// [`RenderManager::budget`], so that a ratio becomes a frame row count in
/// exactly one place — [`live_budget_ratio`], the resolver itself, is
/// private to this module for the same reason.
#[cfg(any(test, feature = "test-helpers"))]
pub fn live_budget(term_h: usize) -> usize {
    live_budget_ratio(term_h, FRAME_MAX_ROW_RATIO)
}

/// The budget at an arbitrary ratio — [`RenderManager`]'s configured one.
///
/// Private: the manager is the only caller, which is what rules
/// out two computations of the frame's row count — the configured ratio
/// resolving one way here and another way in a caller that re-derived it.
fn live_budget_ratio(term_h: usize, ratio: f32) -> usize {
    ((term_h as f32 * ratio).ceil() as usize).max(1)
}

/// One height bound, as a **spec** the allocator resolves against the frame
/// height (design §Render, §6): a [`RowAsk`] is the element's demand, and only
/// resolution turns it into rows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RowSpec {
    /// Exactly this many rows — the common case, and every element's answer
    /// today, which is what keeps the split the even one (§6: all-`FixedNum`
    /// weights evenly, i.e. today's semantics, pinned by `allocate_*`).
    FixedNum(usize),
    /// A fraction of the frame's height, capped at `rows` (design §Render:
    /// "Fraction of the frame area, capped at `rows`" — e.g. a future
    /// header/content split). No element asks for one today; the variant
    /// exists so one can without reopening the allocator.
    ///
    /// Approximate by design: an in-frame ratio is a *preference about
    /// a share*, resolved into integral rows either way, so the design
    /// sketch's `f32` is precision enough — and there is nothing for it to
    /// agree *with*: the frame's row count comes from the configured ratio
    /// inside [`RenderManager::budget`] and nowhere else, while this resolves
    /// an element's ask against the terminal height. One computation per
    /// number, so no two of them can disagree.
    RatioAndNum(f32, usize),
}

impl RowSpec {
    /// This bound in rows for a frame of `term_h`: a fixed spec is its own
    /// answer; a ratio resolves against the *frame height* (a share of the
    /// terminal, not of the budget) and caps at `rows`.
    pub fn resolve(&self, term_h: usize) -> usize {
        match *self {
            RowSpec::FixedNum(rows) => rows,
            RowSpec::RatioAndNum(ratio, rows) => {
                ((term_h as f32 * ratio).ceil() as usize).min(rows)
            }
        }
    }
}

/// Height demand of one element: a floor and a want, each a [`RowSpec`]
/// (design §Render; plan §3.2's `must`/`might` generalized) — the floor is
/// served before the split, the want caps the element's share of the surplus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowAsk {
    /// Floor: rows the element keeps before anything else is served.
    pub min: RowSpec,
    /// Want: rows the element takes when space allows.
    pub max: RowSpec,
}

impl RowAsk {
    /// The common ask: both bounds fixed rows.
    pub fn fixed(min: usize, max: usize) -> Self {
        RowAsk {
            min: RowSpec::FixedNum(min),
            max: RowSpec::FixedNum(max),
        }
    }

    /// Resolve against a frame of `term_h` rows: the [`Extent`] [`allocate`]
    /// splits over.
    pub fn resolve(&self, term_h: usize) -> Extent {
        let extent = Extent {
            must: self.min.resolve(term_h),
            might: self.max.resolve(term_h),
        };
        debug_assert!(
            extent.must <= extent.might,
            "a floor above the want: {self:?} at term_h {term_h}"
        );
        extent
    }
}

/// Height demand of one element, in screen rows, **resolved**: a [`RowAsk`]
/// after [`RowAsk::resolve`] has given its specs a frame height to mean
/// numbers (plan §3.2, whose `must`/`might` these are — the allocator's own
/// vocabulary, one step short of the element's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// Floor: rows the element keeps before anything else is served.
    pub must: usize,
    /// Want: rows the element takes when space allows.
    pub might: usize,
}

/// One drawable piece of a frame (plan §3.2, design §Render).
///
/// The sizing declaration is the *trait's*, not this struct's:
/// [`Element`] carries only the impl, and [`Self::ask`] reads the element's
/// own floor and want — which is what §3.1's two passes require (every ask
/// read before any of them is rendered), and what lets an element answer in
/// rows-at-`width` instead of at whatever width it was constructed for. Only
/// the rendering is polymorphic, and that is the whole point of the shape:
///
/// - `render` cannot accumulate a `match` over element kinds, which is how
///   squeeze knowledge leaked into this module in the first place. The two
///   opinions that had already leaked are out: `INPUT_CONTEXT_ROWS` now sits in
///   `repl.rs` beside the editor that holds it, and the overlay's `h / 2` in
///   `ui.rs` beside the dialog that holds it (§3.2.2). Only `DividerItem` and
///   `LinesItem`, which have no opinion to express, live here;
/// - each element holds whatever *it* needs to hold — an `Rc` of its rows, a
///   shared editor handle, anything — and the frame layer neither knows nor
///   copies it.
#[derive(Clone)]
pub struct Element {
    /// The element's own squeeze policy and painting, told what it got.
    ///
    /// `Arc`, not `Box`: an element set is minted once by the
    /// panel that owns it (an open menu) and *cloned into* every
    /// composition — the paint moves a copy, the set stays live and is
    /// updated in place. The shared ownership is the point. `Send + Sync`
    /// is not optional — `Output: Send + Sync` and `draw: Mutex<DrawCache>`
    /// put the composition behind both.
    render: Arc<dyn RenderElement + Send + Sync>,
}

/// Height in, rows out (plan §3.2.3, design §Render).
///
/// `width` is a *parameter*, not a dependency, so §3.1's "no terminal
/// dependency" holds: an impl can be unit-tested at any width with no terminal
/// anywhere in sight. Every element answers its own height — the
/// construction-time `must`/`might` fields [`Element`] used to carry are gone,
/// because only the element knows what it wants *at this width*.
pub trait RenderElement {
    /// Min/max rows of this element at `width` (design §Render). Resolved
    /// against the frame height by [`allocate`], never by the element: a spec
    /// is a share of the terminal, and the frame's height is not the element's
    /// to know.
    fn height(&self, width: usize) -> RowAsk;

    /// Produce the rows for an allocation of exactly `height` screen rows of
    /// `width`, and this element's caret if it has one.
    ///
    /// Must not return more than `height` rows. The frame builds its row
    /// ranges from what comes back, so an over-tall impl desynchronises
    /// `spans`, the caret and the allocator's own arithmetic — and unlike the
    /// old `match`, nothing downstream can clamp it, because nothing
    /// downstream knows what the rows mean.
    fn render(&self, height: usize, width: usize) -> Rendered;
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
    /// An element with an impl of its own rendering *and* of its own height.
    /// Every constructor below is one line of it, so a new kind is a
    /// `RenderElement` impl plus a constructor here — no `match` anywhere.
    pub fn new(render: impl RenderElement + Send + Sync + 'static) -> Self {
        Element {
            render: Arc::new(render),
        }
    }

    /// Dim horizontal rule spanning the full terminal width. All-or-nothing:
    /// a one-row rule that does not fit is a rule nobody wanted.
    pub fn divider() -> Self {
        Element::new(DividerItem)
    }

    /// Fixed text rows (commit lines, future status line). `min == max`, so
    /// it is fully served by pass 1 and never competes for the surplus — that
    /// is what the floor is for, and it is why a status row needs no protection
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
    pub fn lines(rows: Vec<String>) -> Self {
        Element::new(LinesItem { rows })
    }

    /// The [`RowAsk`] this element contributes to [`allocate`] at `width` —
    /// read from the element itself, which is what lets it answer
    /// in rows-at-this-width rather than in whatever it was built from.
    pub fn ask(&self, width: usize) -> RowAsk {
        self.render.height(width)
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

impl RenderElement for DividerItem {
    fn height(&self, _width: usize) -> RowAsk {
        RowAsk::fixed(1, 1)
    }

    fn render(&self, h: usize, tw: usize) -> Rendered {
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

impl RenderElement for LinesItem {
    fn height(&self, _width: usize) -> RowAsk {
        RowAsk::fixed(self.rows.len(), self.rows.len())
    }

    fn render(&self, h: usize, tw: usize) -> Rendered {
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

/// Resolve `asks` against a frame of `term_h` rows, then distribute `budget`
/// rows over them, honoring floors then splitting the surplus evenly among
/// flexible elements (plan §3.2, OQ1–OQ2; design §6).
///
/// * Resolution is the allocator's (§6): `FixedNum(n)` → `n`,
///   `RatioAndNum(r, n)` → `min(ceil(r × term_h), n)`. The ratio is a share
///   of the *terminal height*, so `term_h` is what it resolves against —
///   `budget` is what the frame may spend, not what it is.
/// * If every element fits (`Σ might ≤ budget`) each gets its `might` —
///   the frame is as tall as its content, never padded to the budget. That
///   branch is also the design's `budget = min(ratio × term_h, Σ resolved
///   max)`: the frame's height comes out as `min(budget, Σ max)` either way.
/// * Otherwise every element first receives its `must`. If the floors
///   themselves exceed `budget`, they are scaled down by the same even
///   division, in element order (so the input's floor is served before a
///   divider, which is served before an overlay).
/// * The remaining rows are split evenly across elements still below their
///   `might`; shares an element cannot use flow back and are re-split next
///   round (max-min fairness).
///
/// All-`FixedNum` — every element today — resolves to the same `(must,
/// might)` pairs the element's asks resolve to, so the split is the
/// even one the `allocate_*` tests pin.
pub fn allocate(asks: &[RowAsk], term_h: usize, budget: usize) -> Vec<usize> {
    if asks.is_empty() {
        return Vec::new();
    }
    let extents: Vec<Extent> = asks.iter().map(|a| a.resolve(term_h)).collect();
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
pub fn window(n: usize, focus: usize, budget: usize, prefer_context: usize) -> usize {
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
    pub buf: String,
    /// Byte range of each screen row; contiguous, covering all of `buf`.
    pub extents: Vec<Range<usize>>,
    /// `(element index, row range)` — which element painted which rows.
    /// Written on every render; reserved for the dirty-span refinement of
    /// the partial diff (§4.4) — the row diff is deliberately kept as the
    /// sole reader until profiling asks for more.
    #[allow(dead_code)]
    pub spans: Vec<(usize, Range<usize>)>,
    /// `(row, display_col)` — both 0-based; row is frame-relative, col is
    /// the on-screen column (prompt included on row 0). `None` when no input
    /// element is rendered or its caret fell outside the window.
    pub caret: Option<(usize, usize)>,
}

impl Frame {
    pub fn height(&self) -> usize {
        self.extents.len()
    }

    /// Bytes of screen row `i`.
    pub fn row(&self, i: usize) -> &str {
        &self.buf[self.extents[i].clone()]
    }
}

/// Render `elements` at their allocated `heights` into a [`Frame`]
/// (plan §3.1). `heights` must have one entry per element — the output of
/// [`allocate`] — and `term_w` is the terminal width in columns.
pub fn render(elements: &[Element], heights: &[usize], term_w: usize) -> Frame {
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
        // because it knew what every arm produced, and a `RenderElement` impl
        // is free to be wrong — so `render`'s contract says fit `h`, and the
        // module that could once enforce it no longer can. Worth it, because
        // enforcing it meant the module knowing what each element was.
        let out = el.render.render(h, tw);
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
/// `pub` because `OverlayItem`'s impl now lives with the dialogs
/// (`ui.rs`) — the padding is a property of the *row*, not of the element kind.
pub fn pad_row(row: &str, term_w: usize) -> String {
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
/// produced, plus the **kind** of write it is. On the chain route (`Content`)
/// the bytes are complete, pre-wrapped, line-cleared rows ending at a line
/// boundary; on the diverting route (`Reasoning` / `Error`, raw — see
/// [`Self::stream`]) they are as rendered.
///
/// The payload is the bytes alone — the `\n` split **is** the row structure,
/// so a separate count would be a second source of truth that can disagree.
/// Render owns the number it writes: it repairs the bytes (forces the line
/// terminator, re-wraps anything over the watched width) and reads the row
/// count back off the result, so the count can never diverge from the bytes
/// (see [`RenderManager::render`]).
///
/// Content-specific classification stops *above* this type: the only
/// vocabulary the design sends down with a write is `Stream` (the kind), which
/// rides with every write. `Content` is every write through the boundary chain
/// — screen content, choreography included. `Reasoning` is the hand-off that
/// skips the chain (straight to `TermIO`, no choreography, byte-identical to
/// the stderr bypass this replaced) when reasoning is diverted.
pub struct ContentWrite {
    /// The block's bytes as Content produced them.
    pub bytes: Vec<u8>,

    /// The kind of write — content's statement of *what* it is, not where it
    /// goes; the sink is `TermIO`'s decision from caps (design §TermIO).
    /// Below this the hub routes on the token and stays sink-generic (design
    /// §Render: `ContentWrite { lines, stream }`).
    pub stream: crate::termio::Stream,
}

/// Repair a write's bytes at the watched `term_w`: force the line
/// terminator, then re-wrap every row that exceeds `term_w`, entering each
/// continuation with `\n\x1b[K`. Returns the repaired bytes and Render's row
/// count — the newline count of the result, so Render's number cannot disagree
/// with what it writes.
///
/// The net applies to **every kind** (design §Render layer): a diverting write
/// lands on meta only when the primary is not a terminal — exactly when frames
/// are dropped and the screen cache is already fiction — so no screen sees the
/// bytes and the row count only matters where every write is a screen write.
/// Render needs the width, never the sink, which is `TermIO`'s alone.
///
/// This is the overwrite rule's last guard (ui_draw_261001): Draw clears per
/// *terminal line* (an `EL` on each row's entry), so the only thing that
/// defeats the clear is a row wide enough to wrap — the wrap crosses onto a
/// row no byte entered, which keeps whatever the frame left there (the
/// status-bar remnant in the right margin). With no row past `term_w`, no write
/// can cross onto an uncleared row; the per-row entry `EL` covers the rest, and
/// no padding is needed.
fn repair(w: &mut ContentWrite, term_w: usize) -> usize {
    let term_w = term_w.max(1);
    if w.bytes.is_empty() {
        return 0;
    }
    let text = String::from_utf8_lossy(&w.bytes);
    let mut out = String::with_capacity(text.len() + 1);
    let mut rows = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            rows += 1;
        }
        let mut rest = line;
        while display_width_ansi(rest) > term_w {
            let (head, tail) = split_at_width(rest, term_w);
            out.push_str(head);
            if tail.is_empty() {
                // A glyph wider than the budget: the head swallowed the
                // remainder, so the row ends here (no invented break).
                rest = "";
                break;
            }
            out.push('\n');
            rows += 1;
            out.push_str("\x1b[K"); // the continuation clears on entry
            rest = tail;
        }
        out.push_str(rest);
    }
    // A block ends at a line boundary: a text that did not carry its
    // terminator gets one here, and it counts as the row it moves to.
    if !text.ends_with('\n') {
        out.push('\n');
        rows += 1;
    }
    w.bytes = out.into_bytes();
    rows
}

// --- the frame hub -------------------------------------------------

/// The frame hub (design §Render layer): where a composition comes to be
/// budgeted, rendered, applied and remembered. It owns the drawing edge
/// (`Draw`) and is the content path's only door onto the screen (the
/// `ContentWrite` hand-off below), batching's and reposition's home. The
/// composition budgets each element's own [`RowAsk`] — resolved here against
/// the frame height — rather than a construction-time `(must, might)` pair.
/// The element stack is the *configured* one
/// ([`Self::configure`], membership changed by the repl on state
/// transitions) and [`Self::render`] composes it with no stack handed in —
/// paint renders whatever is configured, and the content write is the
/// other half of the same verb.
pub struct RenderManager {
    /// The drawing edge: the byte sink and its live-screen
    /// bookkeeping, owned — the design's `Draw { … }` under this roof. Plain
    /// fields inside; this manager's `Mutex` is what serializes them.
    draw: Draw,
    /// The configured long-lived element stack — bar, input, blank row, plus
    /// the overlay/confirm when one is open — installed by [`Self::configure`]
    /// (the repl owns membership, design §Render: "the manager's `stack` is
    /// configured **by the repl**"). [`Self::render`] composes exactly this,
    /// and [`Draw::reposition`] re-composes it when a content write
    /// brings the area back, so the frame is current by construction
    /// (re-rendering the configured elements is cheap and always reads the
    /// live state, where a snapshot of the rendered rows would freeze
    /// whatever the status said at the moment of compose).
    ///
    /// Membership changes only on a state transition — the repl calls
    /// [`Self::configure`] there, never at paint. A direct
    /// [`Self::draw_frame`] leaves it intact: it describes the frame the
    /// *next* [`Self::render`] builds, not the last composition a content
    /// write consumed.
    stack: Vec<Element>,
    /// The fraction of the frame's terminal height the area may occupy —
    /// [`configure`](Self::configure)'s, resolved against the render's
    /// height on every call.
    frame_max_row_ratio: f32,
    /// The watched geometry — the handle `TermIO` hands out at setup (design
    /// §3). `render` composes and paints at this size; `Content::frame_size`
    /// carries the same channel, and [`Self::painted_size`] reads through
    /// `draw`'s own clone.
    size: SizeWatch,
    /// The reader's caret-invalidation queue — the receiver half is this
    /// manager's alone (design §3: `caret` → "RenderManager
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
            stack: Vec::new(),
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
    /// Called at the prompt loop's top: the
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
            self.reanchor();
        }
    }

    /// The channel handles, once at setup (design §3): where the
    /// entry-DSR window in `Interactive::new` takes the input receiver for
    /// the repl. Reached through here so the setup never names `TermIO`,
    /// which by then lives inside [`Self::draw`].
    pub fn handles(&mut self) -> TermHandles {
        self.draw.handles()
    }

    /// Raw mode off — the front door (design §Render: "suspend() stops the
    /// reader and drops raw mode ($EDITOR, Ctrl-Z)"), pass-through to
    /// `Draw`/`TermIO` — the only real implementation is `TermIO::suspend`,
    /// The reader-stop rides with the raw-mode drop.
    /// Idempotent. Plain reports `Unsupported` (no tty to hand over) — the
    /// editor propagates it; teardown-style callers discard it.
    pub fn suspend(&self) -> std::io::Result<()> {
        self.draw.suspend()
    }

    /// Raw mode on — the reverse of [`Self::suspend`], and also the initial
    /// entry at prompt-loop startup. A no-op on Plain.
    pub fn resume(&self) -> std::io::Result<()> {
        self.draw.resume()
    }

    /// One line of non-REPL input — the front door to
    /// [`TermIO::read_line`](crate::termio::TermIO::read_line) for the
    /// piped/batch driver (design §Content). Plain answers; `Tty` reports
    /// `Unsupported` (its stdin belongs to the reader).
    pub fn read_line(&self) -> std::io::Result<String> {
        self.draw.read_line()
    }

    /// Publish a settled size on the geometry watch (parked-key drains and
    /// the prompt loop's defensive `Resize` arms), through the `TermIO`
    /// that owns the channel.
    pub fn set_size(&self, size: TermSize) {
        self.draw.set_size(size);
    }

    /// Probe the terminal for its live size and publish it — the prompt
    /// loop's loop-top refresh, under `TermIO`'s roof. Returns
    /// the probed size so the caller can compare it against
    /// [`Self::painted_size`] without a second read.
    pub fn probe_size(&self) -> TermSize {
        self.draw.probe_size()
    }

    /// The drawing edge, for the test seams that read *past* this manager
    /// (`cache.applied`, and `term_io.recorded` for the byte edge). Production
    /// reaches the edge through this manager's own verbs — `render`,
    /// `reanchor`, `painted_size`, `draw_frame` (content writes go through
    /// `render_content`). Test-only: the host's row-accounting and byte-edge
    /// tests reach through it.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn draw(&mut self) -> &mut Draw {
        &mut self.draw
    }

    /// Set the frame's max-height ratio **and** install the element stack
    /// (design §Render `configure(frame_max_row_ratio, frame_elements)`).
    ///
    /// The membership verb: the ratio is the one knob the budget
    /// resolves from, and the elements are the frame's whole membership —
    /// bar, input, blank row, plus the overlay/confirm when one is open. The
    /// **repl** calls this on every state transition that changes what is on
    /// screen (open/close/change an overlay, open/clear a confirm, submit
    /// clear, commit); [`Self::render`] then composes exactly what is
    /// configured, with nothing handed in per paint. [`Draw::reposition`]
    /// re-composes the same stack to bring the area back after a content
    /// write.
    pub fn configure(&mut self, frame_max_row_ratio: f32, frame_elements: Vec<Element>) {
        self.frame_max_row_ratio = frame_max_row_ratio;
        self.stack = frame_elements;
    }

    /// The live-region budget at this manager's configured ratio — the
    /// frame's row count, resolved **here and nowhere else** (design §Render:
    /// the manager "resolves `configure()`s frame max ratio into concrete
    /// rows"). A caller that needs the number the next [`Self::render`] will
    /// budget with — the page grant, which has to pre-compute the same
    /// allocation — takes it from here rather than re-deriving the ratio, so
    /// the grant and the paint cannot disagree. `pub` for exactly
    /// those callers; [`live_budget_ratio`], the resolver, is this module's
    /// alone.
    pub fn budget(&self, term_h: usize) -> usize {
        live_budget_ratio(term_h, self.frame_max_row_ratio)
    }

    /// The live-region budget at the **watched** height — the number the next
    /// [`Self::render`] will budget with, no `term_h` to thread (design §Render:
    /// the page grant "takes it from here rather than re-deriving"). For a
    /// caller that wants the pre-paint row count without naming dims.
    pub fn budget_now(&self) -> usize {
        self.budget(self.size.borrow().height.max(1))
    }

    /// Compose, then merge with the content — the design's round trip
    /// (§Render, §Draw): a UI paint is `render(None)`; a content write is
    /// `render(Some(w))`. The manager prepares the frame — the *configured*
    /// stack read at the watched size — and hands frame + content to
    /// [`Draw::draw`], which does the writing.
    ///
    /// No dims, no stack, no return: both halves read the watched size and the
    /// configured [`Self::stack`], so a resize applies lazily at the next
    /// `render` and a caller keys off the resize event plus
    /// [`Self::painted_size`], never off a size this verb returns.
    pub fn render(&mut self, content: Option<ContentWrite>) {
        // Nothing configured and nothing to write: no frame to compose, no
        // area to paint. (A content write with an empty stack still runs: it
        // has bytes to place.)
        if self.stack.is_empty() && content.is_none() {
            return;
        }
        let frame = self.compose();
        match content {
            None => self.draw.draw(&frame, None),
            Some(mut w) => {
                // Render owns the final guarantee: repair the bytes against the
                // watched width, then read the row count back off them.
                let rows = repair(&mut w, self.size.borrow().width);
                self.draw.draw(&frame, Some((&w, rows)));
            }
        }
    }

    /// Compose the configured [`Self::stack`] into a frame at the watched size
    /// — the one place [`allocate`] runs for a composition. A caller that has
    /// to know the rows element's grant *before* the frame is painted (the
    /// page-scroll path) re-runs `allocate` over the same asks rather than
    /// asking the painter to guess twice.
    fn compose(&self) -> Frame {
        let size = *self.size.borrow();
        let (tw, th) = (size.width.max(1), size.height.max(1));
        let asks: Vec<RowAsk> = self.stack.iter().map(|e| e.ask(tw)).collect();
        let heights = allocate(&asks, th, self.budget(th));
        render(&self.stack, &heights, tw)
    }

    /// Re-anchor the live-screen cache to the real cursor (DSR): the whole
    /// round-trip is Draw/TermIO's (the sole stdin owner answers the query),
    /// so this is a pure front-door pass-through — the answer never leaves
    /// the edge (design §TermIO).
    pub fn reanchor(&mut self) {
        self.draw.reanchor()
    }

    /// The size the **last frame applied through this hub** took — the one
    /// number resize detection compares the terminal against (ui_draw_v2
    /// §9 #3), read from the applied dims `render`/`draw_frame` record. Before
    /// anything has been applied: the watched geometry (floored) — a caller
    /// that has not painted yet starts from what the terminal is, not from a
    /// size nobody drew at. Distinct from the dims a paint is
    /// *composed* for (`Content::frame_size`, the current size — what a
    /// dialog wants when it opens): that is a belief, this is a memory.
    pub fn painted_size(&self) -> (usize, usize) {
        self.draw.painted_size()
    }

    /// Apply a frame that no composition produced *through this hub* —
    /// `ui::resolve`'s empty frame, the direct `Output::draw_frame` paths,
    /// tests. Unlike the old recipe-clearing version, the configured `stack`
    /// survives: it describes the frame the *next* [`Self::render`] builds,
    /// not the last composition a content write consumed, so a raw apply
    /// leaves it intact.
    pub fn draw_frame(
        &mut self,
        frame: &Frame,
        policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        self.draw.draw_frame(frame, policy, dims)
    }

    // --- the content hand-off (design §Content/§Render) -----------

    /// The content round trip — the design's `render(Some(w))` (§Render)
    /// content half, now folded into one verb. Every content write (streaming
    /// or replay) reaches the screen through here and never around it: the
    /// merge (consume → write → reposition) happens in [`Draw::draw`]. The
    /// batch deferral a replay write takes is the shared `batch_active` flag's
    /// — it stays open across the burst and the guard's drop emits the re-emit
    /// once (ui_draw_v2 R5).
    ///
    /// This is the content half of [`Self::render`] alone — the UI half is
    /// `render(None)`, which composes the configured stack and applies it.
    pub fn render_content(&mut self, content: ContentWrite) {
        self.render(Some(content));
    }

    /// One content block on the replay path (§Render): bulk writes, one per
    /// call inside a batch, with the frame's re-emit deferred to a final
    /// regular apply. Same round trip as [`Self::render_content`] — a replay
    /// write is a content write that happens to be inside an open batch, and
    /// the deferral is the flag's, not the verb's. Kept as its own name so
    /// the replay path reads as the design's `render_batch`, but the
    /// choreography it runs is the one round trip's.
    pub fn render_batch(&mut self, content: ContentWrite) {
        self.render(Some(content));
    }

    /// Is a content batch open — its writes defer the frame's re-emit to
    /// the guard's drop? The read side of the flag, for the content
    /// hand-off to pick its verb.
    pub fn batching(&self) -> bool {
        self.draw.cache.batch_active
    }

    /// Raise the batch flag if none is open; `true` means the caller's guard
    /// owns the batch (and must [`Self::end_batch`] on drop). Batching defers
    /// *reposition*, and reposition is the hub's.
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
    pub fn begin_batch(&mut self) -> bool {
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
    pub fn end_batch(&mut self) {
        if !self.draw.cache.batch_active {
            return;
        }
        self.draw.cache.batch_active = false;
        let Some(deferred) = self.draw.cache.deferred.take() else {
            return;
        };
        // Re-compose the configured stack at the watched size (the bar pulls
        // live state); fall back to the consumed frame when no stack is
        // configured — the invariant that matters is I5: an area that was
        // consumed is always put back.
        let frame = self.compose();
        let frame = if frame.height() == 0 {
            &deferred
        } else {
            &frame
        };
        self.draw.reposition(frame, deferred.height());
    }
}

/// Ends a content batch when dropped — see [`RenderManager::begin_batch`].
/// Batching defers *reposition*, and reposition is the hub's, so the guard
/// names the hub rather than a `Content` or an `Output`
/// ([`crate::output::BatchGuard`]). Both exist so that
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
    pub fn open(manager: &'a Arc<Mutex<RenderManager>>) -> Self {
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

    // ---- live_budget ----

    #[test]
    fn live_budget_caps_at_two_thirds() {
        assert_eq!(live_budget(24), 16);
        assert_eq!(live_budget(3), 2);
        assert_eq!(live_budget(1), 1);
        assert_eq!(live_budget(2), 2);
    }

    // ---- the manager ----

    /// The manager's default budget *is* the old `live_budget` answer, and
    /// `configure` is what moves it: same numbers, one knob.
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
        small.configure(0.1, Vec::new());
        assert_eq!(small.budget(24), 3);
        assert_eq!(small.budget(1), 1, "never below one row");
    }

    /// `render` applies through the manager's `Draw` and remembers the
    /// composition there (the applied size, where the content path reads it).
    /// Frame bytes read from a recording `TermIO` — the manager's own capture
    /// taps the *content* hand-off only, so a frame-byte assertion reads the
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
        // The configured stack is what `render` composes: install it,
        // then apply with no stack handed in.
        manager.configure(crate::render::FRAME_MAX_ROW_RATIO, stack);
        manager.render(None);
        let bytes = manager
            .draw
            .term_io
            .recorded(crate::termio::Stream::Content);

        assert_eq!(
            manager.painted_size(),
            (40, 24),
            "the applied dims are remembered"
        );
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
            manager.stack.len(),
            2,
            "the configured stack is kept on the hub, where reposition reads it"
        );
    }

    /// A real non-terminal primary drops the frame bytes wholesale (design
    /// §TermIO): the paint composes and the cache advances (the fiction Draw
    /// always-merges accepts), but no frame byte — bar, rows, walk — reaches the
    /// stream. Content is untouched by the drop: it is written and, at the
    /// edge, stripped to text.
    #[test]
    fn a_non_terminal_primary_paints_no_frame() {
        let size = crate::termio::fixed(TermSize {
            width: 40,
            height: 24,
        });
        // A real stream (degrades) whose primary is not a terminal.
        let mut manager = RenderManager::new(
            size,
            crate::termio::TermIO::recording_with(crate::termio::Caps {
                primary: false,
                meta: true,
            }),
        );
        manager.configure(
            crate::render::FRAME_MAX_ROW_RATIO,
            vec![Element::lines(vec!["hello".to_string()])],
        );
        manager.render(None);
        assert!(
            manager
                .draw
                .term_io
                .recorded(crate::termio::Stream::Content)
                .is_empty(),
            "a frame must not reach a piped primary"
        );
        assert_eq!(
            manager.draw.cache.applied,
            Some((40, 24)),
            "the cache advances even though the frame bytes were dropped (fiction)"
        );

        // Content still flows — stripped at the edge, text only.
        manager.render_content(crate::render::ContentWrite {
            bytes: b"\x1b[32mgreen\x1b[0m\x1b[K\n".to_vec(),
            stream: crate::termio::Stream::Content,
        });
        assert_eq!(
            manager
                .draw
                .term_io
                .recorded(crate::termio::Stream::Content),
            b"green\n",
            "content survives as stripped text"
        );
    }

    // ---- allocate ----
    //
    // The parity pins (design §6): these asked in `Extent`s before, and
    // ask in `RowAsk::fixed` now — all-`FixedNum` resolves to the same pairs
    // the `Element` fields used to carry, so the split is byte-identical.
    // `term_h` is inert for a fixed ask; it is what a ratio resolves against.

    #[test]
    fn allocate_full_fit_returns_mights_not_budget() {
        // OQ1: content smaller than the budget renders to its natural size;
        // the frame is never padded up to the budget.
        let e = [
            RowAsk::fixed(1, 3),
            RowAsk::fixed(1, 1),
            RowAsk::fixed(1, 5),
        ];
        assert_eq!(allocate(&e, 24, 12), vec![3, 1, 5]);
    }

    #[test]
    fn allocate_short_input_stays_single_row() {
        // OQ2: a 1-row input has min=max=1; the flexible overlay takes the
        // whole surplus.
        let e = [
            RowAsk::fixed(1, 1),  // short input
            RowAsk::fixed(1, 1),  // divider
            RowAsk::fixed(1, 20), // menu
        ];
        assert_eq!(allocate(&e, 24, 8), vec![1, 1, 6]);
    }

    #[test]
    fn allocate_splits_surplus_evenly_then_spills() {
        // Two flexible elements: even split first (3/3), then the input
        // caps at its might and the overlay absorbs the leftover.
        let e = [
            RowAsk::fixed(1, 4),
            RowAsk::fixed(1, 1),
            RowAsk::fixed(1, 10),
        ];
        // Σ might = 15 > 10; floors = 3; pool = 7.
        // Round 1: share 3 → input +3 (capped at 4), overlay +3 → pool 1.
        // Round 2: overlay +1.
        assert_eq!(allocate(&e, 24, 10), vec![4, 1, 5]);
    }

    #[test]
    fn allocate_even_split_without_caps() {
        let e = [RowAsk::fixed(1, 6), RowAsk::fixed(1, 6)];
        // Σ might = 12 > 8; floors 2; pool 6 → 3/3.
        assert_eq!(allocate(&e, 24, 8), vec![4, 4]);
    }

    #[test]
    fn allocate_scales_floors_evenly_when_they_overflow() {
        // Fixed 5-row commit block plus input/divider floors in a 3-row
        // budget: even division from zero caps each claimant at its floor,
        // so the small floors survive and the big one truncates.
        let e = [
            RowAsk::fixed(1, 1),
            RowAsk::fixed(1, 1),
            RowAsk::fixed(5, 5),
        ];
        assert_eq!(allocate(&e, 24, 3), vec![1, 1, 1]);
    }

    #[test]
    fn allocate_crippled_budget_serves_elements_in_order() {
        // Budget below the number of elements: one row each, element
        // order wins — input first, overlay last (plan §3.2).
        let e = [
            RowAsk::fixed(1, 1),
            RowAsk::fixed(1, 1),
            RowAsk::fixed(1, 40),
        ];
        assert_eq!(allocate(&e, 24, 2), vec![1, 1, 0]);
        assert_eq!(allocate(&e, 24, 1), vec![1, 0, 0]);
        assert_eq!(allocate(&e, 24, 0), vec![0, 0, 0]);
    }

    #[test]
    fn allocate_empty_asks() {
        assert!(allocate(&[], 24, 10).is_empty());
    }

    // ---- RowSpec resolution (design §6) ----

    #[test]
    fn a_fixed_spec_is_its_own_answer_at_any_height() {
        assert_eq!(RowAsk::fixed(1, 5).resolve(1), Extent { must: 1, might: 5 });
        assert_eq!(
            RowAsk::fixed(1, 5).resolve(50),
            Extent { must: 1, might: 5 }
        );
    }

    #[test]
    fn a_ratio_resolves_against_the_frame_height_and_caps_at_its_rows() {
        // A share of the *terminal*, not of the budget: `ceil(24 × 1/3) = 8`,
        // capped at the ask's 4; the want's half of the terminal is far below
        // its cap of 100. (Exact on purpose: 1/2 is representable, so the
        // assert does not depend on where an inexact ratio rounds — a ratio
        // ask is approximate by design, not pinned to the bit.)
        let ask = RowAsk {
            min: RowSpec::RatioAndNum(1.0 / 3.0, 4),
            max: RowSpec::RatioAndNum(0.5, 100),
        };
        assert_eq!(
            ask.resolve(24),
            Extent { must: 4, might: 12 },
            "floor capped at 4, want at half the terminal"
        );
        // The same ask on a short terminal: the ratio lands below the cap.
        assert_eq!(ask.resolve(6), Extent { must: 2, might: 3 });
    }

    /// Nothing resolves the frame's row count but the manager's own budget:
    /// `live_budget_ratio` is private and `live_budget` is test-only, so a
    /// production caller that wants the number has to ask
    /// [`RenderManager::budget`] for it. The point is by-construction, not
    /// by-agreement: there is one computation of the frame row count
    /// and one of an element's share, and no test has to keep them honest.
    #[test]
    fn a_ratio_ask_allocates_through_the_ordinary_split() {
        // A ratio want is just a resolved want: floors first, then the even
        // split, with the ratio's 12 as the cap on this element's share.
        let e = [
            RowAsk::fixed(1, 1),
            RowAsk {
                min: RowSpec::FixedNum(1),
                max: RowSpec::RatioAndNum(0.5, 100),
            },
        ];
        // Σ might = 1 + 12 = 13 > budget 10; floors 2; pool 8 → the menu
        // takes 8 of its 12.
        assert_eq!(allocate(&e, 24, 10), vec![1, 9]);
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

    // ---- repair (Render width enforcement) ----

    fn main_write(bytes: &[u8]) -> ContentWrite {
        ContentWrite {
            bytes: bytes.to_vec(),
            stream: crate::termio::Stream::Content,
        }
    }

    /// The payload's row structure is the `\n` split: Render forces the
    /// terminator a non-conforming write owes, then counts the newlines of the
    /// result. An empty write stays empty and moves nothing.
    #[test]
    fn repair_forces_the_terminator_and_counts_rows() {
        let mut w = main_write(b"ab");
        assert_eq!(repair(&mut w, 40), 1);
        assert_eq!(w.bytes, b"ab\n", "a `\\n`-less tail is terminated");

        let mut w = main_write(b"a\nb\n");
        assert_eq!(repair(&mut w, 40), 2);
        assert_eq!(w.bytes, b"a\nb\n", "a terminated block is unchanged");

        let mut w = main_write(b"\n");
        assert_eq!(repair(&mut w, 40), 1, "a blank row is still a row");

        let mut w = main_write(b"");
        assert_eq!(repair(&mut w, 40), 0);
        assert!(w.bytes.is_empty());
    }

    /// A row over `term_w` is re-wrapped at the cell edge, each continuation
    /// entered by `\n\x1b[K` so it clears on entry (the overwrite rule) — the
    /// leak this enforcement closes. Escapes ride whole.
    #[test]
    fn repair_rewraps_an_over_wide_row_and_re_arms_the_clear() {
        let mut w = main_write(b"\x1b[Kabcdef\n");
        assert_eq!(repair(&mut w, 3), 2);
        assert_eq!(w.bytes, b"\x1b[Kabc\n\x1b[Kdef\n");

        // Fits: no invented break.
        let mut w = main_write(b"\x1b[Kabc\n");
        assert_eq!(repair(&mut w, 3), 1);
        assert_eq!(w.bytes, b"\x1b[Kabc\n");
    }

    /// The net is kind-blind (design §Render layer): a diverting kind is
    /// repaired like any other, because when it lands on meta no screen sees it
    /// — the row count only matters where every write is a screen write.
    #[test]
    fn repair_is_kind_blind() {
        let mut w = ContentWrite {
            bytes: b"raw tail".to_vec(),
            stream: crate::termio::Stream::Reasoning,
        };
        assert_eq!(repair(&mut w, 40), 1);
        assert_eq!(
            w.bytes, b"raw tail\n",
            "the terminator is forced regardless"
        );
    }
}
