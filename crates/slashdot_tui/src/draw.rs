//! The apply phase of the unified draw pipeline (plan `doc/ui_draw_v2.md`
//! §4): the live-screen cache, the escape planner, and the sink that emits
//! what the planner returns.
//!
//! Planning is pure: [`plan_apply`] turns a [`Frame`] plus the [`DrawCache`]
//! into the exact escape bytes and the post-apply cache state — bytes out,
//! cache in/out — so the REPL, the dialogs and the vt100 tests all exercise
//! the *same* sequence. [`Draw`] is the impure edge: it owns the `TermIO`
//! sink, and is the only thing that writes. Newlines are emitted
//! bare — the tty's OPOST/ONLCR maps them to CRLF (the same convention as
//! the REPL's redraw) — so any harness that bypasses the tty must do that
//! mapping.
//!
//! A frame's rows are self-contained: each carries the escapes needed to
//! repaint itself in place (trailing `EL` or padding to `term_w`), so apply
//! may rewrite any subset of rows unconditionally. Composition is
//! [`crate::render`].

use crate::ansi::{display_width, HIDE_CURSOR, SHOW_CURSOR};
use crate::render::{ContentWrite, Frame};
use crate::termio::{TermHandles, TermIO, TermSize};
use tokio::sync::watch;

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
pub struct LiveFrame {
    /// Absolute 1-based row of the frame's first row, post-scroll. The bottom
    /// is derived as `frame_top_row + frame.height() - 1`.
    pub frame_top_row: usize,
    /// Terminal size when the frame was applied (partial-apply predicate).
    pub term_w: usize,
    pub term_h: usize,
    /// The rows, for the partial-apply diff.
    pub frame: Frame,
}

/// Live screen bookkeeping, owned by [`Draw`] (plan §4.1). Rows are
/// absolute; bottom-relative quantities are recomputed on every apply.
pub struct DrawCache {
    /// Absolute cursor position as we last left it: (1-based row, 0-based col).
    /// The row is authoritative; the col is best-effort (only an apply sets it,
    /// and only the row is ever consumed).
    caret: (usize, usize),
    /// Terminal height at the last apply. The full-apply clear floor is
    /// `min(term_h, screen_bottom)`, so a maximize's revealed rows below the old
    /// bottom survive. Never cleared by invalidation.
    screen_bottom: usize,
    live: Option<LiveFrame>,
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
    pub applied: Option<(usize, usize)>,
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
    pub batch_active: bool,
    /// Content rows written since a consume — the `content_rows` term of the
    /// below-frame clear (`old_h - h_new - consumed`), accumulated across a
    /// batch's writes and taken by the next `paint_forward`. Only meaningful
    /// while a consume is outstanding (deferred or awaiting reposition);
    /// `paint_forward` zeroes it either way.
    pub consumed_rows: usize,
    /// The frame taken by a batch's *first* consume, held until the batch
    /// ends.
    ///
    /// Needed because [`Draw::reposition`] falls back to `taken` when
    /// no element stack is configured — reachable in production by a direct
    /// `draw_frame` (`ui::resolve`'s empty frame), which applies a frame with
    /// no composition behind it. Without this stash a batch would consume
    /// that area and its final reposition would find no stack to re-render and
    /// render nothing, so the area would never return.
    pub deferred: Option<Frame>,
}

impl DrawCache {
    pub fn new(term_h: usize) -> Self {
        DrawCache {
            caret: (1, 0),
            screen_bottom: term_h.max(1),
            live: None,
            applied: None,
            batch_active: false,
            consumed_rows: 0,
            deferred: None,
        }
    }

    /// The tracked cursor row (1-based), for the host's row-accounting tests:
    /// they compare it against where a terminal emulator puts the cursor after
    /// replaying the same bytes (row_accounting §6 B2).
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn tracked_row(&self) -> usize {
        self.caret.0
    }

    /// Where the next frame belongs (plan §5.1): the intact frame's top row if
    /// there is one, else the tracked cursor row.
    pub fn anchor(&self) -> usize {
        self.live
            .as_ref()
            .map(|l| l.frame_top_row)
            .unwrap_or(self.caret.0)
    }

    /// Advance the tracked cursor by `n` lines, clamped to the screen, and
    /// invalidate the frame region: an ordinary content write moved the cursor,
    /// so the previous frame no longer describes the screen (plan §4.2).
    pub fn note_lines(&mut self, n: usize, term_h: usize) {
        let h = term_h.max(1);
        self.caret.0 = (self.caret.0 + n).clamp(1, h);
        self.live = None;
    }

    /// Record a re-anchor attempt (plan §4.2): sync the tracked row with the
    /// terminal's answer and decide what the frame does about it, returning
    /// the row to use and any bytes the decision needs.
    ///
    /// This is the one sanctioned absolute path (row_accounting §6b). A resize
    /// or a foreign writer can leave the cursor *outside* the live block, and
    /// then no relative walk can reach the block's rows — the erase that
    /// precedes every paint walks up from the cursor, so it would erase the
    /// wrong rows entirely. Here the block's row is still known and a DSR has
    /// just confirmed where the cursor went, so when the answer *disagrees*
    /// the old region is addressed and the cursor parked where the answer says.
    ///
    /// **Invalidation is on disagreement only** (ui_draw_261001): an answer
    /// that is not where the frame left the cursor (foreign writer), or a
    /// reflow — dims changed, so `frame_top_row` is a pre-resize absolute.
    /// A silent terminal (`None`) with no reflow is *no new information*: the
    /// tracked row stands, the frame stays live, and the unverified cursor is
    /// parked at the frame's caret, which is what makes `live`'s contract
    /// hold. Plan §9 latent issue 2 recorded the old rule ("a silent terminal
    /// offers no more guarantee than an answering one" → clear it); the
    /// guarantee comes from *when* DSR runs instead — exceptional events only,
    /// so between calls nothing external has run.
    pub fn reanchor(
        &mut self,
        row: Option<usize>,
        term_w: usize,
        term_h: usize,
    ) -> (usize, String) {
        let mut s = String::new();
        let prev = self.caret.0;
        if let Some(r) = row {
            self.caret.0 = r.max(1);
        }
        let mut skip_park = false;
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
            } else if !reflowed && (row.is_none() || row == Some(prev)) {
                // **Frame intact: leave it alone.** The answer confirms where
                // the frame left the cursor (its caret), or there is no answer
                // at all — a silent DSR is *no new information*, not bad
                // information: between turns nothing external has run, and DSR
                // belongs to exceptional events (startup, resume, resize,
                // editor return, error/dialog entries), not to regular drawing
                // (ui_draw_261001). No erase — the next paint overwrites the
                // frame's rows as it enters them — and `live` stays, so
                // `anchor()` keeps reading the frame's *top* and the next
                // compose diffs in place instead of anchoring on the caret
                // row: that anchoring is the down-a-row-per-re-anchor drift
                // the reflow branch above fixes, and the region clear was a
                // second wrong answer on top of it, at whatever rows the
                // bookkeeping last believed.
                //
                // Park only when unverified: with no answer, parking puts the
                // cursor at the frame's caret, which is what makes `live`'s
                // contract true; a verified cursor is already there.
                skip_park = row.is_some();
            } else {
                // **A cursor outside the block, or a reflow with no answer.**
                // The answer *disagreed* with the frame — a foreign writer put
                // the cursor elsewhere — or the dims changed with the terminal
                // silent, where `frame_top_row` is a pre-resize absolute that
                // addresses a row the area no longer occupies. Either way a
                // relative walk cannot reach the block's rows reliably, and the
                // believed row is the only thing left to address. This is the
                // one absolute address in a frame apply (row_accounting §6b).
                for i in 0..rows {
                    s.push_str(&cup(top + i, 1));
                    s.push_str(EL);
                }
                self.live = None;
            }
        }
        // The park is for the paths that moved or lost the frame (the reflow
        // path walked to the region to clear it; a silent intact frame parks
        // to *make* the contract true) — a *verified* intact cursor is already
        // where the frame says it is, and parking it would be the move, not
        // the sync.
        if !skip_park {
            s.push_str(&cup(self.caret.0, self.caret.1 + 1));
        }
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
pub fn plan_apply(
    cache: &mut DrawCache,
    frame: &Frame,
    policy: ApplyPolicy,
    term_w: usize,
    term_h: usize,
) -> String {
    let tw = term_w.max(1);
    let th = term_h.max(1);
    let h = frame.height();
    // Where the next frame belongs: the intact frame's top, else the cursor.
    let anchor = cache.anchor().clamp(1, th);

    if policy == ApplyPolicy::Append {
        return plan_append(cache, frame, anchor, th);
    }

    // Diff path (ui_draw_261001). A live frame with the dims it was painted
    // at *is* at its own position: content can only move a frame by consuming
    // it, and that clears `live` — so position identity is `live` itself, and
    // the rows that differ (plus the rows an orphaned shrunken frame left) are
    // the only bytes written. Rows appended by growth have no old counterpart
    // and are written outright.
    let dims_ok = cache
        .live
        .as_ref()
        .is_some_and(|live| live.term_w == tw && live.term_h == th);
    // The diff path writes without newlines — it can never scroll — so the
    // frame must *fit* where it stands: a grow past the screen's bottom row
    // would write the new rows off the end (`CUD` clamps, and they land on the
    // rows below the frame instead of below the content). That grow goes
    // through the full path, whose forward paint emits the `LF`s that scroll
    // the frame into view — the same gate the old `delta == 0` predicate was.
    let fits = cache
        .live
        .as_ref()
        .is_some_and(|live| live.frame_top_row + h <= th + 1);
    if dims_ok && fits {
        return plan_partial(cache, frame, anchor, th);
    }

    // Full path: paint forward from the cursor — no descend, no bottom-up, no
    // `delta` (ui_draw_261001). A live frame with changed dims still sits
    // under the cursor, so walk to its top first; its height is what this pass
    // may have to clear below the new block.
    let mut s = String::new();
    let (top, old_h) = match cache.live.take() {
        Some(live) => {
            let (cur, _) = frame_caret(&live.frame);
            cuu(&mut s, cur);
            s.push('\r');
            (live.frame_top_row, live.frame.height())
        }
        // No frame: the cursor is wherever the last write left it, and that is
        // where the block belongs.
        None => (anchor, 0),
    };
    if h == 0 {
        // The empty frame is "clear the region and park": the rows the old
        // frame covered, erased in one downward pass, cursor back on the
        // region's first row so whatever comes next lands where the frame was
        // (plan §5.3). With nothing live there is nothing to clear — the park
        // is the whole operation.
        if old_h > 0 {
            erase_down(&mut s, old_h);
            cuu(&mut s, old_h - 1);
        }
        cache.live = Some(LiveFrame {
            frame_top_row: top,
            term_w: tw,
            term_h: th,
            frame: frame.clone(),
        });
        cache.screen_bottom = th;
        cache.caret = (top.clamp(1, th), 0);
        return s;
    }
    s.push_str(&paint_forward(cache, frame, top, tw, th, old_h));
    s
}

/// Paint the block forward from the cursor's row: row 0 first, a line clear on
/// each row's entry, `LF` between rows. At the bottom row the `LF`s scroll —
/// which is the room the block needs, decided by the terminal rather than by a
/// computed `delta` (ui_draw_261001: the fold is placed by content, never by
/// the screen's bottom edge).
///
/// `old_h` is the height of the frame this pass replaces, `0` when there was
/// none. Rows the old frame covered that neither this block's `h` rows nor the
/// content that consumed it (`DrawCache::consumed_rows`, taken by this call)
/// cover are the old frame's tail — erased here, in the same pass, after the
/// block's rows and before the caret.
///
/// Precondition: the cursor is on row `top`. The column doesn't matter — row
/// 0's own `CR` establishes it.
fn paint_forward(
    cache: &mut DrawCache,
    frame: &Frame,
    top: usize,
    tw: usize,
    th: usize,
    old_h: usize,
) -> String {
    let h = frame.height();
    debug_assert!(h > 0, "paint_forward needs rows to paint");
    let consumed = std::mem::take(&mut cache.consumed_rows);
    let orphan = old_h.saturating_sub(h + consumed);
    let mut s = String::new();
    // The block. `CR` + `EL` on entry is the overwrite rule: one line clear,
    // immediately followed by the row's bytes (the row is padded, so the pair
    // replaces the whole line). `CR` also cancels a wrap-pending left by the
    // row above.
    for i in 0..h {
        if i > 0 {
            s.push('\n');
        }
        s.push('\r');
        s.push_str(EL);
        s.push_str(frame.row(i));
    }
    // The old frame's tail: rows it covered that nothing overwrote. These sit
    // at or above the screen bottom by construction (they were on screen), so
    // these `LF`s never scroll — a nonzero `orphan` and a paint-induced scroll
    // are mutually exclusive (orphan > 0 implies content did not push the old
    // frame's bottom past the screen).
    for _ in 0..orphan {
        s.push('\n');
        s.push('\r');
        s.push_str(EL);
    }
    // Scroll bookkeeping: an `LF` at the bottom row scrolls instead of moving
    // down, so the block may have landed `scroll` rows higher than `top`. The
    // terminal decided it; the tracked row follows it. The tracked row is the
    // *cursor*, and the caret walk below is relative — so this number is the
    // only place the scroll has to be known.
    let scroll = (top + h - 1).saturating_sub(th);
    let top_post = top.saturating_sub(scroll).max(1);
    // Caret: from the last written row (block bottom or orphan tail) up to the
    // frame's caret row, then across. `CR` first — the last written row is
    // full width, hence wrap-pending, and `CUU` preserves the column.
    let (r, c) = frame_caret(frame);
    s.push('\r');
    cuu(&mut s, (h + orphan).saturating_sub(1).saturating_sub(r));
    cuf(&mut s, c);
    cache.live = Some(LiveFrame {
        frame_top_row: top_post,
        term_w: tw,
        term_h: th,
        frame: frame.clone(),
    });
    cache.screen_bottom = th;
    cache.caret = ((top_post + r).clamp(1, th), c);
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
    // Did this apply move a single byte? With `touched` false the cursor is
    // still *at* the old caret — row `at`, column `from.1` — which is what
    // makes the caret-only emit below worth deciding on.
    let mut touched = false;
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
            touched = true;
        }
    }
    move_rows(&mut s, at, to_r);
    // Caret-only moves (ui_draw_261001's open question, answered): the diff
    // subsumes them — nothing painted means the column is still `from.1`, so
    // a caret that didn't move writes **zero bytes** (an apply that changes
    // nothing is a no-op on the wire), and one that only moved across columns
    // writes this `CR` + `CUF` and nothing else. `CR` first, for the same
    // reason as `push_caret`: a repainted row left the cursor at its end, and
    // `CUF` walks from the current column.
    if touched || to_c != from.1 {
        s.push('\r');
        cuf(&mut s, to_c);
    }
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
/// Walk to the frame's first row and take the frame — which comes back below
/// the content via [`plan_reposition`].
///
/// "Consume" is about **ownership**: the frame stops being *live*
/// (`live = None`) but is handed back rather than dropped, so it can be
/// re-emitted (§4.5.3). **No pixels are cleared here** (ui_draw_261001): the
/// content write that follows emits a line clear with each line it enters —
/// overwrite, one pass — and rows it never reaches are the reposition's
/// `old_h` tail (`old_h - h_new - consumed_rows`, cleared in the same forward
/// pass as the block).
///
/// The walk ends at the block's first row, column 0, because content takes
/// the block's **whole height** — the status and input rows are content's to
/// use too, not a margin the write has to live below.
///
/// Returns the walk and the frame to re-emit, or `None` when no frame was live.
///
/// **`CR` is part of the contract, not a detail.** `CUU` preserves the column,
/// and the frame's caret is out at the *input's* column, so without an explicit
/// `CR` the content would start a prompt-width to the right.
pub fn plan_consume(cache: &mut DrawCache) -> Option<(String, Frame)> {
    let live = cache.live.take()?;
    let (caret_row, _) = frame_caret(&live.frame);
    let mut s = String::new();
    // Up to the block's first row. **No erase** (ui_draw_261001): the content
    // write that follows clears each line as it enters it, and the rows it
    // never reaches are the reposition's `old_h` tail to clear — overwrite
    // only, one pass, no separate clearing walk.
    cuu(&mut s, caret_row);
    // Postcondition: column 0 of the block's first row. `CR` is part of the
    // contract, not a detail: `CUU` preserves the column, and the frame's
    // caret is out at the *input's* column, so without it content would start
    // a prompt-width to the right.
    s.push('\r');
    cache.caret = (live.frame_top_row, 0);
    Some((s, live.frame))
}

/// Reposition: the frame comes back **below the content** — the cursor is at
/// the seam content left it on (one row below its last line, column 0), and
/// the block paints forward from there. The old frame's height (`old_h`, from
/// the frame the consume took) plus the content rows since then decide how
/// many of the old frame's rows nothing overwrote; `paint_forward` clears
/// them in the same pass.
pub fn plan_reposition(
    cache: &mut DrawCache,
    frame: &Frame,
    old_h: usize,
    tw: usize,
    th: usize,
) -> String {
    let h = frame.height();
    debug_assert!(h > 0, "reposition needs rows to paint");
    let top = cache.caret.0.clamp(1, th);
    paint_forward(cache, frame, top, tw, th, old_h)
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
pub fn el_if_short(content: &mut String, term_w: usize) {
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

// The DSR query lives in `crate::termio` (the sole stdin owner answers it
// inline between polls, or no reader exists yet to race).
// `Draw::reanchor_to_cursor` takes the answer as a parameter.

// --- the live screen --------------------------------------------------------

/// The live screen: the byte sink, and what is believed to be on it.
///
/// Plain fields, no locks of its own: `Draw` is reached only
/// through `RenderManager`'s `Mutex`, which is what serializes byte emission
/// with the bookkeeping advance — the old writer-then-cache lock order,
/// encapsulated one layer up. Methods that write or advance therefore take
/// `&mut self`.
pub struct Draw {
    /// The terminal backend — the design's `Draw { term_io, cache }`. Every
    /// byte leaves through [`Self::write_bytes`]: the mode guard and the
    /// size watch reach the layers above through this one field; the caps
    /// stop *inside* it (strip and frame-drop are the edge's decisions).
    /// Content bytes never name it directly — they arrive through
    /// the manager's `ContentWrite` hand-off.
    pub term_io: TermIO,
    /// The size watch: the latest `TermSize`, without a
    /// per-call terminal query. The same channel the manager holds — a
    /// subscribed receiver, so a test's private watch pins both.
    pub size: watch::Receiver<TermSize>,
    /// Live-screen bookkeeping (plan §4.1): tracked cursor row, clear floor,
    /// last applied frame. (The content-path state lives in `content/`.)
    pub cache: DrawCache,
}

impl Draw {
    pub fn new(size: watch::Receiver<TermSize>, term_io: TermIO) -> Self {
        let term_h = size.borrow().height;
        Draw {
            term_io,
            cache: DrawCache::new(term_h),
            size,
        }
    }

    /// The frame-side choke point for byte emission: the frame planner,
    /// the re-anchor and the consume walk all land here, and this goes out
    /// `TermIO`'s `Frame` target — the primary sink, dropped wholesale when the
    /// primary is a real non-terminal (design §Draw). Written and flushed per
    /// call (a frame must be on screen before the next one is planned; the sink
    /// flushes itself, where the old `Box<dyn Write>` needed a separate
    /// `flush`). A byte-edge test's recording constructor intercepts at the
    /// same edge, which is what `TermIO::recording()` taking the seam
    /// wholesale means.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        let _ = self.term_io.write_frame(bytes);
    }

    /// Content bytes of kind `s`, routed by `TermIO` to the sink the kind and
    /// the caps pick (design §TermIO): `Content` to the primary, the diverting
    /// kinds to the primary when it is a terminal else meta. The one
    /// kind-carrying write; the frame-side bytes go through [`Self::write_bytes`].
    pub fn write_kind(&mut self, s: crate::termio::Stream, bytes: &[u8]) {
        let _ = self.term_io.write(s, bytes);
    }

    // --- the frame + content merge (design §Draw) ---------------------

    /// The one verb the hub calls (design §Draw): apply `frame` alone for a UI
    /// paint (`content == None`), or append `content` to scrollback —
    /// consuming the live frame's rows — then put `frame` back below it.
    ///
    /// The manager composes `frame` first, repairs the write and reads its row
    /// count, and hands both down; Draw does the writing but never the
    /// measuring. `rows` is the count Render derived from the (repaired) bytes;
    /// it is passed rather than recomputed here so a capture-emptied write —
    /// bytes gone, screen still to be choreographed — keeps its advance.
    ///
    /// The write's bytes carry their kind and are routed by `TermIO`; nothing
    /// here forks on the sink (design §Draw: Draw always merges, and a diverting
    /// write that lands on meta is one whose primary is not a terminal — frames
    /// are dropped and the screen cache is already fiction, so the merge is
    /// inert and its frame-side bytes never leave).
    pub fn draw(&mut self, frame: &Frame, content: Option<(&ContentWrite, usize)>) {
        let Some((w, rows)) = content else {
            self.apply(frame);
            return;
        };
        let th = self.size.borrow().height.max(1);
        // Consume, then write (ui_draw_v2 §4.5 P1): the walk goes ahead of the
        // bytes, and the block follows carrying its own per-line clears.
        let (walk, taken) = match plan_consume(&mut self.cache) {
            Some((walk, frame)) => (walk, Some(frame)),
            None => (String::new(), None),
        };
        if !walk.is_empty() {
            self.write_bytes(walk.as_bytes());
        }
        // The write's bytes as handed down by the manager. A capture is the
        // manager's call (it hands an empty write instead), so Draw stays
        // capture-agnostic — and a write the tap emptied still advances the
        // cursor by the row count the manager passed.
        self.write_kind(w.stream, &w.bytes);
        if rows > 0 {
            self.cache.note_lines(rows, th);
        }
        // The area comes back below what was just written, deferred while a
        // batch is open (ui_draw_v2 R5). The consume's rows ride along for the
        // below-frame clear (`old_h - h_new - consumed_rows`).
        if taken.is_some() || self.cache.deferred.is_some() {
            self.cache.consumed_rows += rows;
        }
        let Some(taken) = taken else {
            return;
        };
        if self.cache.batch_active {
            // Keep the *first* frame taken: it is the fallback when no stack
            // re-composes, and a later write finds nothing to consume.
            if self.cache.deferred.is_none() {
                self.cache.deferred = Some(taken);
            }
            return;
        }
        // `frame` is the manager's fresh composition; a raw `draw_frame` with
        // no composition behind it leaves it empty, and the consumed frame is
        // then the honest thing to put back.
        let frame = if frame.height() == 0 { &taken } else { frame };
        self.reposition(frame, taken.height());
    }

    /// Apply `frame` as a UI paint (plan §4.3–4.4): plan the bytes, write them,
    /// follow the frame's caret, and remember the size that was applied. The
    /// frame bytes are dropped at [`TermIO`]'s edge on a real non-terminal
    /// primary, but the cache still advances (design §Draw: Draw always merges;
    /// the fiction the drop leaves is accepted).
    fn apply(&mut self, frame: &Frame) {
        let (tw, th) = self.watched_dims();
        let applied = self.draw_frame(frame, ApplyPolicy::Frame, (tw, th));
        self.cache.applied = Some(applied);
    }

    /// Paint a frame **below the cursor** — a reposition: the content write left
    /// the cursor on the seam, and the block paints forward from there. `old_h`
    /// is the height of the frame the consume took, so `paint_forward` clears
    /// the rows nothing overwrote.
    ///
    /// A frame with no rows has nothing to put back: `ui::resolve` clears a
    /// dialog by applying `Frame::empty()`, and a content write right after it
    /// consumes a zero-height area. Returning early — and clearing the
    /// accumulator, so a stale count cannot misplace the next reposition's
    /// below-frame clear — is the honest answer.
    pub fn reposition(&mut self, frame: &Frame, old_h: usize) {
        if frame.height() == 0 {
            self.cache.consumed_rows = 0;
            return;
        }
        let (tw, th) = self.watched_dims();
        let bytes = plan_reposition(&mut self.cache, frame, old_h, tw, th);
        self.write_bytes(bytes.as_bytes());
    }

    /// The watched geometry, floored the way every frame path floors it.
    fn watched_dims(&self) -> (usize, usize) {
        let s = *self.size.borrow();
        (s.width.max(1), s.height.max(1))
    }

    // --- the TermIO pass-throughs (design §3: content / UI never name it) --

    /// The channel handles, once at setup — the render manager's forward, so
    /// the pause-gated entry-DSR window in `Interactive::new` stays the one
    /// place `handles()` runs.
    pub fn handles(&mut self) -> TermHandles {
        self.term_io.handles()
    }

    /// The full `TermIO::suspend` — reader parked, raw mode off.
    /// Idempotent. Plain reports `Unsupported` (no tty to hand over) — the
    /// editor propagates it; teardown-style callers discard it.
    pub fn suspend(&self) -> std::io::Result<()> {
        self.term_io.suspend()
    }

    /// Raw mode on — the reverse of [`Self::suspend`].
    pub fn resume(&self) -> std::io::Result<()> {
        self.term_io.resume()
    }

    /// One line of non-REPL input — the forward to
    /// [`TermIO::read_line`](crate::termio::TermIO::read_line), so the
    /// piped/batch driver's read stays under the edge's roof.
    pub fn read_line(&self) -> std::io::Result<String> {
        self.term_io.read_line()
    }

    /// Publish a settled size on the instance's watch (parked-key drains,
    /// defensive `Resize` arms).
    pub fn set_size(&self, size: TermSize) {
        self.term_io.set_size(size);
    }

    /// Probe + publish the live terminal size (the prompt loop's loop-top
    /// refresh, under TermIO's roof).
    pub fn probe_size(&self) -> TermSize {
        self.term_io.probe_size()
    }

    /// Apply `frame` at the cursor under `policy` (plan §4.3–4.4): plan the
    /// bytes, write them, return the dims actually applied.
    ///
    /// On a real non-terminal primary the frame bytes are dropped at
    /// [`TermIO`]'s edge (design §TermIO), but the plan still runs and the
    /// cache advances (design §Draw: Draw always merges).
    pub fn draw_frame(
        &mut self,
        frame: &Frame,
        policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        // Paint exactly the size the caller composed for (`dims`) — one
        // source for both phases of the frame (ui_draw_v2 §9 #3). Clamped
        // to what `plan_apply` stores as the live frame's `term_w`/`term_h`.
        let (tw, th) = (dims.0.max(1), dims.1.max(1));
        let bytes = plan_apply(&mut self.cache, frame, policy, tw, th);
        self.write_bytes(bytes.as_bytes());
        // The cursor follows the frame's own caret. A composed frame with one is
        // where the user types; a frame whose input is hidden mid-turn (R1) has
        // none to place, so hiding is the honest reading of the same fact rather
        // than a second piece of input state kept in step by hand. Either way
        // this is the cheapest place to be sure — `TermGuard` and the shell can
        // also leave it hidden.
        self.write_bytes(if frame.caret.is_some() {
            SHOW_CURSOR
        } else {
            HIDE_CURSOR
        });
        (tw, th)
    }

    /// Advance the tracked cursor by `n` physical lines. The real terminal
    /// scrolls and keeps the cursor on the bottom row when content overflows,
    /// so we *clamp* to the height rather than wrapping (a modulo wrap would
    /// teleport the tracked row to the top of the screen after a long session).
    pub fn note_lines(&mut self, n: usize) {
        let h = self.size.borrow().height;
        // Advance the tracked cursor and drop the frame region — the write that
        // called us moved the cursor off the frame (plan §4.2).
        self.cache.note_lines(n, h);
    }

    /// Sync the tracked row with the terminal's real cursor and write any
    /// re-anchor bytes — the DSR round-trip lives entirely at the edge: the
    /// query goes to [`TermIO`](crate::termio::TermIO) (the sole stdin owner
    /// answers it, or no reader exists to race), and the answer is consumed
    /// by the cache here. It never travels beyond this type.
    pub fn reanchor(&mut self) {
        let row = self.term_io.query_cursor_row();
        self.reanchor_to_cursor(row);
    }

    /// Apply a DSR answer to the cache and write the re-anchor bytes.
    /// `None` (a terminal that did not answer) is "no new information":
    /// `reanchor` invalidates only on disagreement (a foreign cursor, a
    /// reflow), never on a silent terminal (ui_draw_261001).
    ///
    /// The re-anchor erases the live frame's rows, which is why it writes
    /// bytes: a resize or a foreign writer can leave the cursor outside the
    /// block, and the relative walks the frame path uses cannot reach rows
    /// above the cursor from there. This is the one absolute address in a
    /// frame apply (row_accounting §6b), and it costs nothing when no frame
    /// is live — every streamed block, and every commit — because the bytes
    /// are then just the cursor park. So it lives here, with the sink: every
    /// caller treats it as a side-effecting re-anchor, and none of them writes.
    fn reanchor_to_cursor(&mut self, row: Option<usize>) {
        let (_, bytes) = {
            // Same floors the `output::term_width` free function applies: the
            // 20-column floor is load-bearing for `plan_apply` when the
            // geometry is still uninitialized.
            let size = *self.size.borrow();
            self.cache
                .reanchor(row, size.width.max(20), size.height.max(1))
        };
        self.write_bytes(bytes.as_bytes());
    }

    /// The size of the last frame actually applied — what a painter compares
    /// the terminal against to notice a change made since, including one that
    /// happened while someone else (a turn) held the screen. Before the first
    /// apply, a fresh terminal query: a caller that has not painted yet should
    /// start from what is actually on screen, not from a size nobody drew at.
    pub fn painted_size(&self) -> (usize, usize) {
        self.cache.applied.unwrap_or_else(|| {
            let size = *self.size.borrow();
            (size.width.max(1), size.height.max(1))
        })
    }
}

#[cfg(test)]
mod apply_tests {
    //! Apply-phase tests (plan §4). They drive [`plan_apply`]'s bytes through a
    //! real vt100 emulator — the same screen/caret checks the REPL's `redraw`
    //! tests use — so the cache transitions and the emitted bytes are pinned
    //! together.
    use super::*;
    use crate::render::{render, Element};
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
        // safe to run on every `reanchor()` call, including mid-stream.
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

    /// A full apply clears **nothing below itself**: the rows below hold
    /// content this frame never painted (here, a foreign writer's), and erase
    /// is only ever for prior *frame* rows (ui_draw_261001). The paths that
    /// own foreign rows are the empty-frame clear and `reanchor`.
    #[test]
    fn full_clears_nothing_below() {
        let mut cache = DrawCache::new(24);
        let f = lines_frame(&["one", "two"], 20);
        cursor_to(&mut cache, 5, 24);
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 24);
        // Seed rows the frame does not own, then put the cursor back on row 5:
        // the frame applies relative to wherever the cursor is.
        let seed = "\x1b[8;1Hstale8\x1b[9;1Hstale9\x1b[5;1H";
        let mut p = at(24, 20, 4);
        feed(&mut p, seed);
        feed(&mut p, &s);
        let rows = screen(&p, 24, 20);
        assert_eq!(rows[4], "one");
        assert_eq!(rows[5], "two");
        assert_eq!(rows[7], "stale8", "rows the frame does not own survive");
        assert_eq!(rows[8], "stale9");
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

    /// The caret-only question (ui_draw_261001) — the steady half: an apply
    /// that changes no row bytes and wants the caret where it already is
    /// writes nothing at all.
    #[test]
    fn identical_apply_with_a_steady_caret_emits_nothing() {
        let mut cache = DrawCache::new(10);
        let f = lines_frame(&["row a", "row b"], 20);
        cursor_to(&mut cache, 5, 10);
        let first = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        assert!(!first.is_empty(), "the first paint writes: {first:?}");
        let second = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        assert_eq!(
            second, "",
            "identical frame, steady caret: nothing to emit: {second:?}"
        );
    }

    /// The caret-only question — the moving half: same rows, caret in a new
    /// column, is only its relative walk — no row bytes, no clears.
    #[test]
    fn caret_only_change_walks_without_row_bytes() {
        let mut cache = DrawCache::new(10);
        let mut f = lines_frame(&["row a"], 20);
        f.caret = Some((0, 1));
        cursor_to(&mut cache, 5, 10);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        let mut g = lines_frame(&["row a"], 20);
        g.caret = Some((0, 4));
        let s = plan_apply(&mut cache, &g, ApplyPolicy::Frame, 20, 10);
        assert_eq!(s, "\r\x1b[4C", "CR + CUF and nothing else: {s:?}");
        assert_eq!(cache.caret, (5, 4), "and the tracked caret follows");
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
        // There is no floor to clear *to* anymore: a first paint owns only its
        // own rows — rows below are not this frame's (ui_draw_261001). The
        // painted row shows two clears and no more: the entry `EL` (the
        // overwrite rule) plus the row's own trailing `el_if_short` (kept for
        // diff mode, where rows are written without an entry clear).
        assert_eq!(
            s.matches("\x1b[K").count(),
            2,
            "only the painted row clears itself: {s:?}"
        );
        let seed = "\x1b[25;1Hkeep25\x1b[30;1Hkeep30\x1b[5;1H";
        let mut p = at(40, 20, 4);
        feed(&mut p, seed);
        feed(&mut p, &s);
        let rows = screen(&p, 40, 20);
        assert_eq!(rows[4], "head");
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
        let s = plan_reposition(&mut cache, &f, f.height(), 20, 24);
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
        let s = plan_reposition(&mut cache, &f, f.height(), 20, 24);
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
        let s = plan_reposition(&mut cache, &f, f.height(), 20, 10);
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
        let s = plan_reposition(&mut cache, &f, f.height(), 20, 24);
        // Not `!s.contains(EL)`: a `Lines` row carries its own trailing `EL` to
        // pad to the terminal width, and each row entry clears anyway — the
        // check that matters is walks. A clear-below is a walk down past the
        // block and back; paint-forward never descends at all (its caret move
        // is up, from the block's bottom), so zero `CUD`s is the whole budget.
        assert_eq!(
            s.matches("\x1b[1B").count(),
            0,
            "no descent at all, so no clear-below walk: {s:?}"
        );
        feed(&mut p, &s);
        let rows = screen(&p, 24, 20);
        assert_eq!(
            rows[10], "",
            "and the row below the new frame is blank: {rows:?}"
        );
    }

    /// §4.5 P1: consume leaves the cursor at the frame's first row, **column
    /// 0** — and erases nothing: it takes ownership, not pixels
    /// (ui_draw_261001). The content write that follows clears each line as
    /// it enters it, and the rows it never reaches are the reposition's
    /// `old_h` tail — overwrite, one pass, no erase walk here.
    ///
    /// The column is the substance. `CUU` preserves it, the frame's caret
    /// is out at the input's column, and the content about to be written would
    /// start a prompt-width to the right. This makes it the plan's own
    /// postcondition rather than something inherited from a trailing `CR`.
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

        let s = plan_reposition(&mut cache, &taken, taken.height(), 20, 24);
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

    /// Plan §9 latent issue 2, revised for paint-forward (ui_draw_261001): a
    /// DSR *timeout* is no new information — DSR runs for exceptional events
    /// only, so between calls nothing external has run, and an intact frame
    /// stands. The tracked row is kept, `live` survives (the next apply still
    /// diffs against it), and the unverified cursor is parked at the frame's
    /// caret, which is what makes `live`'s contract true. An answer that
    /// *agrees* is the same decision, minus the park.
    #[test]
    fn reanchor_fallback_keeps_an_intact_frame() {
        let mut cache = DrawCache::new(10);
        let f = lines_frame(&["x"], 20);
        cursor_to(&mut cache, 5, 10);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        let (row, park) = cache.reanchor(None, 20, 10);
        assert_eq!(row, 5, "tracked row stands");
        assert!(
            park.contains("\x1b[5;1H"),
            "the unverified cursor is parked at the frame's caret: {park:?}"
        );
        assert!(
            cache.live.is_some(),
            "silent DSR: no new information, the frame stands"
        );
        // Identical frame, same anchor: the partial path diffs — no region
        // clear, nothing but the caret walk.
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        assert!(!s.contains("\x1b[K"), "partial path required: {s:?}");
        // The answered case that agrees: verified, so nothing to park.
        assert!(
            cache.reanchor(Some(5), 20, 10).1.is_empty(),
            "verified cursor: the park would be a move, not a sync"
        );
        assert!(cache.live.is_some(), "and the frame still stands");
    }

    /// What *does* invalidate the frame: disagreement (an answer that is not
    /// the frame's caret — a foreign writer) and a reflow with no answer
    /// (`frame_top_row` is a pre-resize absolute). Both fall back to the
    /// absolute path — region addressed, cursor parked, `live` dropped so the
    /// next apply repaints from a full path instead of diffing against rows
    /// that may have moved under us.
    #[test]
    fn reanchor_invalidates_on_disagreement_or_silent_reflow() {
        let mut cache = DrawCache::new(10);
        let f = lines_frame(&["x"], 20);
        cursor_to(&mut cache, 5, 10);
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        let (row, s) = cache.reanchor(Some(8), 20, 10);
        assert_eq!(row, 8, "the answer replaces the tracked row");
        assert!(cache.live.is_none(), "a foreign cursor invalidates");
        assert!(s.contains("\x1b["), "the old region is addressed: {s:?}");

        // Width changed, terminal silent: the frame's top addresses rows the
        // area no longer occupies, so the believed row is all there is — and
        // it is not trusted for position either.
        let _ = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 10);
        assert!(cache.live.is_some());
        let (_row, s) = cache.reanchor(None, 12, 10);
        assert!(cache.live.is_none(), "a silent reflow invalidates");
        assert!(s.contains("\x1b["), "region addressed absolutely: {s:?}");
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

    /// A grow with the cursor mid-screen: the frame follows the cursor, not
    /// the screen's bottom (there is no bottom-edge placement to clear
    /// around), and only its own row clears.
    #[test]
    fn full_paints_at_the_cursor_on_a_grow_and_clears_only_itself() {
        let mut cache = DrawCache::new(24); // screen_bottom = 24
        let f = lines_frame(&["here"], 20);
        cursor_to(&mut cache, 40, 24); // clamps: the cursor sits at the old bottom
        let s = plan_apply(&mut cache, &f, ApplyPolicy::Frame, 20, 40);
        assert_eq!(
            s.matches("\x1b[K").count(),
            2,
            "the painted row's entry clear + its own trailing, and nothing else: {s:?}"
        );
        let seed = "\x1b[30;1Hkeep30\x1b[24;1H";
        let mut p = at(40, 20, 23);
        feed(&mut p, seed);
        feed(&mut p, &s);
        let rows = screen(&p, 40, 20);
        assert_eq!(rows[23], "here", "the frame follows the cursor: {rows:?}");
        assert_eq!(rows[29], "keep30", "and the grow's scrollback is intact");
    }
}
