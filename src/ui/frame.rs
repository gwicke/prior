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

use crate::slashdot_menu::table::display_width;

/// Rows of context preferred above the caret when an [`Element::Input`]
/// window must clip its wrapped height (plan §3.4).
const INPUT_CONTEXT_ROWS: usize = 3;

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
pub(crate) enum Element {
    /// Prompt + editor content. `layout` is the editor's visual layout
    /// (prompt rendered on row 0); the caret lives at `layout.cursor_row` /
    /// `layout.cursor_col` — derived, not stored (plan's `caret` field).
    Input {
        layout: crate::repl::Layout,
        prompt: String,
    },
    /// Dim horizontal rule spanning the full terminal width.
    Divider,
    /// Pre-painted overlay rows (from `paint_overlay`), windowed around the
    /// selection `focus`. Rows must be single-line and are padded to
    /// `term_w` at render time.
    Overlay { rows: Vec<String>, focus: usize },
    /// Fixed text rows (commit lines, future status line). Never flexed;
    /// truncated from the top when the budget cannot hold them all.
    // Not constructed outside tests yet: commit lines currently go through
    // `display_plain` (P4), and the §4.5 fixed-bottom/status-line
    // composition is this variant's first production user.
    #[allow(dead_code)]
    Lines(Vec<String>),
}

impl Element {
    /// The `(must, might)` this element contributes to [`allocate`].
    pub(crate) fn extent(&self) -> Extent {
        match self {
            // Input keeps one row always (the caret); wraps grow `might`.
            Element::Input { layout, .. } => Extent {
                must: 1,
                might: layout.row_count.max(1),
            },
            // Dividers are all-or-nothing cosmetics.
            Element::Divider => Extent { must: 1, might: 1 },
            // A composed overlay is always meant to be seen (callers omit
            // the element when there is nothing to show).
            Element::Overlay { rows, .. } => Extent {
                must: usize::from(!rows.is_empty()),
                might: rows.len(),
            },
            // Fixed content: all rows or (under pressure) fewer.
            Element::Lines(rows) => Extent {
                must: rows.len(),
                might: rows.len(),
            },
        }
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
        match el {
            Element::Input { layout, prompt } => {
                let n = layout.row_count.max(1);
                if h > 0 {
                    let start = window(n, layout.cursor_row, h, INPUT_CONTEXT_ROWS);
                    let count = h.min(n.saturating_sub(start));
                    for r in start..start + count {
                        // The prompt renders on row 0; later rows are
                        // content-only.
                        let body = if r == 0 { prompt } else { "" };
                        let text = layout.rows.get(r).map(String::as_str).unwrap_or("");
                        let mut content = String::with_capacity(body.len() + text.len() + 3);
                        content.push_str(body);
                        content.push_str(text);
                        // `EL` only while there is something to erase —
                        // after a full-width row the cursor is wrap-pending
                        // and `CSI K` would eat the last written cell.
                        if display_width(&content) < tw {
                            content.push_str("\x1b[K");
                        }
                        push_row(&mut buf, &mut extents, &content);
                    }
                    if layout.cursor_row >= start && layout.cursor_row < start + count {
                        caret = Some((first_row + layout.cursor_row - start, layout.cursor_col));
                    }
                }
            }
            Element::Divider => {
                if h > 0 {
                    let row = format!("\x1b[2m{}\x1b[0m", "─".repeat(tw));
                    push_row(&mut buf, &mut extents, &row);
                }
            }
            Element::Overlay { rows, focus } => {
                if h > 0 && !rows.is_empty() {
                    let start = window(rows.len(), *focus, h, h / 2);
                    let count = h.min(rows.len().saturating_sub(start));
                    for r in start..start + count {
                        debug_assert!(
                            !rows[r].contains('\n'),
                            "overlay rows are single screen rows"
                        );
                        push_row(&mut buf, &mut extents, &pad_row(&rows[r], tw));
                    }
                }
            }
            Element::Lines(rows) => {
                // Fixed content: show the first `h` rows (reading order).
                for row in rows.iter().take(h) {
                    debug_assert!(!row.contains('\n'), "line rows are single screen rows");
                    let mut content = row.clone();
                    if display_width(&content) < tw {
                        content.push_str("\x1b[K");
                    }
                    push_row(&mut buf, &mut extents, &content);
                }
            }
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
fn pad_row(row: &str, term_w: usize) -> String {
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
}

impl DrawCache {
    pub(crate) fn new(term_h: usize) -> Self {
        DrawCache {
            caret: (1, 0),
            screen_bottom: term_h.max(1),
            live: None,
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
    pub(crate) fn reanchor(&mut self, row: Option<usize>) -> (usize, String) {
        let mut s = String::new();
        if let Some(live) = self.live.as_ref() {
            let top = live.frame_top_row;
            let bottom = top + live.frame.height().saturating_sub(1);
            for r in top..=bottom {
                s.push_str(&cup(r, 1));
                s.push_str(EL);
            }
        }
        if let Some(r) = row {
            self.caret.0 = r.max(1);
        }
        self.live = None;
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
/// (row_accounting §6b). `anchor` and the `top`/`delta` arithmetic that follows
/// it survive only to keep `cache`'s row bookkeeping — the tracked cursor row
/// row accounting compares against a terminal, and the partial-apply predicate
/// — and are never used for addressing.
pub(crate) fn plan_apply(
    cache: &mut DrawCache,
    frame: &Frame,
    anchor: usize,
    policy: ApplyPolicy,
    term_w: usize,
    term_h: usize,
) -> String {
    let tw = term_w.max(1);
    let th = term_h.max(1);
    let anchor = anchor.clamp(1, th);
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

/// Erase the live frame's rows so a content write cannot leave any of it behind,
/// and leave the caret where it was.
///
/// A content write starts at the cursor, and the cursor is *inside* the live
/// frame (the REPL's input row, or a dialog's caret). The content grows
/// downward from there, overwriting the frame from the caret down — and
/// stranding the rest, which nothing else reclaims: the next frame apply's
/// clear only reaches the previous frame's rows while `live` is set, and this
/// write is what clears it. That is the shape of the reported resume artefact
/// (ui_draw_v2 §1.1): a menu's table header and rows left on screen underneath
/// replayed content, because the replay wrote while the menu's frame was still
/// live.
///
/// Whole-region rather than "the rows the content will miss": a content line
/// shorter than the terminal leaves the frame's background to its right, and
/// the input row is padded to the full width. Empty when no frame is live,
/// which is the case for every streamed block, so this costs nothing while a
/// turn runs.
///
/// The rows are walked from the cursor, so this is correct regardless of where
/// the terminal thinks the block is — there is no row to get wrong
/// (row_accounting §6b).
pub(crate) fn plan_clear_live_frame(cache: &mut DrawCache) -> String {
    let Some(live) = cache.live.as_ref() else {
        return String::new();
    };
    let h = live.frame.height();
    let (cur, col) = frame_caret(&live.frame);
    let mut s = String::new();
    erase_block(&mut s, h, cur, col);
    cache.live = None;
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

/// Erase the `h` rows of the block the cursor is inside — the cursor sitting at
/// block row `cur`, column `col` — and leave it back at that caret.
///
/// Bottom-up, because a walk *down* out of the block is impossible: `CUD`
/// clamps at the bottom row instead of scrolling. Up is safe, and the count is
/// the block's own height, so the walk stops on its first row without clamping
/// at row 1.
fn erase_block(s: &mut String, h: usize, cur: usize, col: usize) {
    if h == 0 {
        return;
    }
    let cur = cur.min(h - 1);
    cud(s, h - 1 - cur);
    erase_up(s, h);
    cud(s, cur);
    // `CR` before the `CUF`: the walks above preserve the column, and the
    // postcondition is the caret's column whatever the cursor arrived on.
    s.push('\r');
    cuf(s, col);
}

/// Erase `h` rows *upward* from the cursor's row, ending on the first of them.
fn erase_up(s: &mut String, h: usize) {
    for i in 0..h {
        s.push_str(EL);
        if i + 1 < h {
            cuu(s, 1);
        }
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
        let els = [Element::Input {
            layout,
            prompt: "> ".to_string(),
        }];
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
        let els = [Element::Input {
            layout,
            prompt: "> ".to_string(),
        }];
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
        let f = render(&[Element::Divider], &[1], 8);
        assert_eq!(f.row(0), "\x1b[2m────────\x1b[0m");
        assert_eq!(f.caret, None);
    }

    #[test]
    fn render_overlay_pads_short_rows_before_reset() {
        let rows = vec!["ab".to_string()];
        let els = [Element::Overlay { rows, focus: 0 }];
        let f = render(&els, &[1], 5);
        // Padding lands inside the styled span so backgrounds reach the edge.
        assert_eq!(f.row(0), "ab   ");
        // Full-width row passes through untouched.
        let els = [Element::Overlay {
            rows: vec!["abcde".to_string()],
            focus: 0,
        }];
        let f = render(&els, &[1], 5);
        assert_eq!(f.row(0), "abcde");
    }

    #[test]
    fn render_overlay_windows_around_focus() {
        let rows: Vec<String> = (0..20).map(|i| format!("item {i}")).collect();
        let els = [Element::Overlay { rows, focus: 10 }];
        let f = render(&els, &[4], 20);
        assert_eq!(f.height(), 4);
        // Centered: focus 10 with 4 visible → start 8 (prefer h/2 = 2).
        assert_eq!(f.row(0).trim_end(), "item 8");
        assert_eq!(f.row(3).trim_end(), "item 11");
    }

    #[test]
    fn render_lines_truncate_from_top() {
        let rows: Vec<String> = (1..=5).map(|i| format!("line {i}")).collect();
        let f = render(&[Element::Lines(rows)], &[3], 20);
        assert_eq!(f.height(), 3);
        assert_eq!(f.row(0), "line 1\x1b[K");
        assert_eq!(f.row(2), "line 3\x1b[K");
        assert_eq!(f.caret, None);
    }

    #[test]
    fn render_full_composition_extents_and_spans() {
        // REPL composition: input + divider + overlay, allocated heights
        // straight from `allocate`.
        let mut e = LineEditor::new();
        e.insert_str("ls -la");
        let layout = e.layout(80, "> ");
        let els = [
            Element::Input {
                layout,
                prompt: "> ".to_string(),
            },
            Element::Divider,
            Element::Overlay {
                rows: vec!["one".to_string(), "two".to_string()],
                focus: 0,
            },
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
        let els = [Element::Divider, Element::Divider];
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

    /// A frame of fixed text rows (self-contained: `Lines` appends the `EL`).
    fn lines_frame(rows: &[&str], tw: usize) -> Frame {
        let rows: Vec<String> = rows.iter().map(|s| s.to_string()).collect();
        let h = rows.len();
        render(&[Element::Lines(rows)], &[h], tw)
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
        plans.push(plan_apply(&mut cache, &f3, 5, ApplyPolicy::Frame, 20, 24));
        // In-place repaint of one row.
        let f3b = lines_frame(&["aa", "XX", "cc"], 20);
        plans.push(plan_apply(&mut cache, &f3b, 5, ApplyPolicy::Frame, 20, 24));
        // Shrink: the orphaned rows are erased by a walk, not by addressing.
        let f1 = lines_frame(&["aa"], 20);
        plans.push(plan_apply(&mut cache, &f1, 5, ApplyPolicy::Frame, 20, 24));
        // A frame too tall for the screen, so the apply must scroll.
        let mut tall = DrawCache::new(10);
        plans.push(plan_apply(
            &mut tall,
            &lines_frame(&["a", "b", "c", "d", "e"], 20),
            8,
            ApplyPolicy::Frame,
            20,
            10,
        ));
        // The content-write clear.
        plans.push(plan_clear_live_frame(&mut cache));
        // Append, which writes the rows as content.
        let mut app = DrawCache::new(24);
        plans.push(plan_apply(&mut app, &f3, 5, ApplyPolicy::Append, 20, 24));
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
        let mut p = at(24, 20, 4);
        feed(
            &mut p,
            &plan_apply(&mut cache, &f, 5, ApplyPolicy::Frame, 20, 24),
        );
        let (_, clear) = cache.reanchor(Some(9));
        // Erase the block's rows where they are, then park on the new row.
        assert_eq!(cups(&clear), vec!["5;1", "6;1", "9;1"], "{clear:?}");
        // And with no frame live it is only the cursor park — which is why it is
        // safe to run on every `cursor_row()` call, including mid-stream.
        let mut idle = DrawCache::new(24);
        let (_, park) = idle.reanchor(Some(7));
        assert_eq!(cups(&park), vec!["7;1"], "{park:?}");
    }

    #[test]
    fn full_paints_rows_and_clears_below() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 20);
        let s = plan_apply(&mut cache, &f, 5, ApplyPolicy::Frame, 20, 24);
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
        let s = plan_apply(&mut cache, &f, 5, ApplyPolicy::Frame, 20, 24);
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
        let s1 = plan_apply(&mut cache, &f1, 5, ApplyPolicy::Frame, 20, 24);
        let f2 = lines_frame(&["aa", "XX", "cc"], 20);
        let s2 = plan_apply(&mut cache, &f2, 5, ApplyPolicy::Frame, 20, 24);
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
        let s1 = plan_apply(&mut cache, &f1, 5, ApplyPolicy::Frame, 20, 24);
        let f2 = lines_frame(&["aa"], 20);
        let s2 = plan_apply(&mut cache, &f2, 5, ApplyPolicy::Frame, 20, 24);
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
                &[Element::Input {
                    layout: e.layout(20, "> "),
                    prompt: "> ".to_string(),
                }],
                rows,
                20,
            )
        };
        let mut cache = DrawCache::new(24);
        e.insert_str("abc");
        let s1 = plan_apply(&mut cache, &input(&e, &[1]), 5, ApplyPolicy::Frame, 20, 24);
        // Shift+Enter: a second row appears and the caret moves onto it.
        e.insert_newline();
        let anchor = cache.anchor();
        let s2 = plan_apply(
            &mut cache,
            &input(&e, &[2]),
            anchor,
            ApplyPolicy::Frame,
            20,
            24,
        );
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
        let anchor = cache.anchor();
        let s3 = plan_apply(
            &mut cache,
            &input(&e, &[2]),
            anchor,
            ApplyPolicy::Frame,
            20,
            24,
        );
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
        let s = plan_apply(&mut cache, &f, 9, ApplyPolicy::Frame, 20, 10);
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
        let s = plan_apply(&mut cache, &f, 5, ApplyPolicy::Frame, 20, 40);
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
        let s1 = plan_apply(&mut cache, &f, 5, ApplyPolicy::Frame, 20, 24);
        // A foreign writer moved the cursor to row 8. The re-anchor both records
        // where it went and erases the block's old rows — the one absolute
        // address in a frame apply, and the reason it exists: from row 8 a
        // relative walk cannot reach rows 5 and 6 at all.
        let (row, clear) = cache.reanchor(Some(8));
        let s2 = plan_apply(&mut cache, &f, row, ApplyPolicy::Frame, 20, 24);
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
        let _ = plan_apply(&mut cache, &f1, 5, ApplyPolicy::Frame, 20, 24);
        let f2 = lines_frame(&["aa"], 20);
        let s2 = plan_apply(&mut cache, &f2, 5, ApplyPolicy::Frame, 40, 24);
        assert!(s2.contains("aa"), "width change repaints the row");
        let mut p = at(24, 20, 4);
        feed(&mut p, &s2);
    }

    #[test]
    fn append_writes_content_and_advances_caret() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["div", "line"], 20);
        let s = plan_apply(&mut cache, &f, 5, ApplyPolicy::Append, 20, 24);
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

    #[test]
    fn clamps_anchor_to_screen() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["x"], 20);
        let s = plan_apply(&mut cache, &f, 999, ApplyPolicy::Frame, 20, 24);
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
        let f = render(
            &[Element::Input {
                layout,
                prompt: "> ".to_string(),
            }],
            &[1],
            20,
        );
        let s = plan_apply(&mut cache, &f, 5, ApplyPolicy::Frame, 20, 24);
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
        let _ = plan_apply(&mut cache, &f, 5, ApplyPolicy::Frame, 20, 10);
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
        let _ = plan_apply(&mut cache, &f, 5, ApplyPolicy::Frame, 20, 10);
        assert_eq!(cache.reanchor(Some(3)).0, 3);
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
        let _ = plan_apply(&mut cache, &f, 5, ApplyPolicy::Frame, 20, 10);
        assert_eq!(cache.reanchor(None).0, 5, "tracked row stands");
        assert!(cache.live.is_none(), "live frame must be invalidated");
        // Identical frame, same anchor: the partial path would emit nothing
        // but the caret; the full path clears the region below the frame.
        let anchor = cache.anchor();
        let s = plan_apply(&mut cache, &f, anchor, ApplyPolicy::Frame, 20, 10);
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
        let s1 = plan_apply(&mut cache, &tall, 38, ApplyPolicy::Frame, 20, 40);
        assert_eq!(cache.anchor(), 37, "scroll must be tracked");
        // The next redraw anchors where the live frame is (the REPL's
        // `frame_anchor()`): no scroll, same top — a shrinking frame leaves
        // the region where the tallest one put it.
        let short = lines_frame(&["a"], 20);
        let anchor = cache.anchor();
        let s = plan_apply(&mut cache, &short, anchor, ApplyPolicy::Frame, 20, 40);
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
        let s = plan_apply(&mut cache, &f, 40, ApplyPolicy::Frame, 20, 40);
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
