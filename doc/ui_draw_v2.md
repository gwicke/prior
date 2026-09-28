# UI Draw v2 — unified frame composition for REPL, menus & dialogs

**Status:** Design settled — implemented (P0 ✅, P1 ✅, P2 ✅, P3 ✅, P4 ✅, P5 ✅)
**Supersedes:** the ad-hoc dialog renderer in `src/ui.rs`, the `redraw` closure in `src/repl.rs`

---

## 1. Problem

Drawing is done in two unrelated code paths:

- **REPL** (`repl.rs` `redraw` closure, line 618): anchored at a tracked `input_row`,
  clears then rewrites input + divider + overlay, lets the terminal scroll on overflow,
  and adjusts a row counter (`next_input_row`). Carefully tuned (see the `\n` vs `CSI B`
  comments and the vt100 tests).
- **Dialogs** (`ui.rs` `confirm`): computes its own anchor, clears with `\x1b[0J`, writes
  lines, no scroll tracking, no shared budget handling.

Consequences: duplicated anchoring/scrolling logic, a second-class dialog path that can
corrupt scrollback (clearing above the cursor), no shared budget enforcement, and a test
suite (`vt100_*`) that re-implements emission instead of exercising it.

Goal: **one pipeline** that composes elements (input line, divider, overlay, dialog,
status line, …) into a frame, allocates vertical space, renders it, and applies it to the
terminal with bounded clearing and consistent scroll tracking. Dialogs become "frames
without an input segment".

---

## 2. Settled decisions (from design discussion)

| Topic | Decision |
|---|---|
| Draw pipeline | 4 phases: **compose → allocate → render → apply** |
| Composition | Elements declare `(must, might)` heights; allocator distributes |
| Budget | Live region ≤ **2/3 of terminal height**; top 1/3 guaranteed scrollback |
| Full apply | **Scroll-then-paint**: LF scroll by `delta` from the bottom row (never `CSI S` — it deletes rows instead of pushing scrollback), then absolute writes |
| Partial apply | Row-diff repaint when `delta == 0 ∧ top == live.frame_top_row ∧ term size (w, h) unchanged` |
| Clear policy | Full apply: clear the union of the old frame region (scroll-shifted) and the rows below the new frame, floored at `min(term_h, screen_bottom)`. Partial apply: row-diff + shrink remainder only. Append: no region clear (reserved for §4.5's chunked writes). **General invariant (P4):** a Frame apply clears the rows below the frame, so any later append-only render (and any row-diff) lands in already-clean space and never re-clears it |
| Buffer | Flat `String` + per-screen-row extents `Vec<Range<usize>>` |
| Caret | Absolute `(row, col)` in the cache (row authoritative, col best-effort); frame-relative `(row_idx, display_col)` inside `Frame`; applied with `CUP` + `CHA` (`\x1b[{col}G`) |
| Draw state | `DrawCache { caret, screen_bottom, live: Option<LiveFrame> }` in `ConsoleOutput`; all writers maintain it before returning |
| Screen position | Stored rows are absolute (everything emitted/consumed is); bottom-relative quantities (`delta`, 2/3 budget, clear floor) are computed per apply. No reflow-model invariance is assumed |
| UI surface | `Output::draw_frame(frame, anchor, policy)` + `frame_anchor()`; default drops UI drawing; `ConsoleOutput` overrides. Content-out (`display_*`) and UI-out stay separate |
| DSR | Only via the explicit `cursor_row()` re-anchor primitive (foreign-process / size-change / dialog-entry sites); steady state uses the cache |
| Dialogs | Same pipeline, no input segment; entry DSR re-anchor, then `frame_anchor()`; `ConfirmSpec.divider` separates mid-turn dialogs from streamed output. Resolve = `Frame::empty()` union-clear; the optional decision line is written at the cleared first row |
| Commit lines | Clear the region with a Frame apply, then write the line where the caret lands (`emit_commit`, dialog decision line). `Append` stays reserved for §4.5's chunked/fixed-bottom writes |

---

## 3. Architecture

### 3.1 Phases

```
compose(self, ctx)  →  Element list with (must, might)     [pure]
allocate(w, h, budget) →  Row ranges per element, extents  [pure]
render(alloc)       →  buffer + caret + element spans      [pure]
apply(f, anchor, pol) → escapes → stdout, update cache    [impure]
```

`compose`/`allocate`/`render` take no terminal dependency (w/h passed in) → unit-testable.
`apply` is the only phase that touches the terminal or the cache.

### 3.2 Element model

```rust
/// Heights in screen rows.
struct Extent { must: usize, might: usize }

enum Element {
    /// Prompt + editor content. `must` = 1 (or min visible), `might` = layout height.
    Input { layout: Layout, prompt: String, caret: (usize, usize) },
    /// Dim horizontal rule. must == might == 1.
    Divider,
    /// Pre-painted overlay rows (from `paint_plan`), focus row for windowing.
    Overlay { rows: Vec<String>, focus: usize },
    /// Fixed text rows (commit lines, future status line).
    Lines(Vec<String>),
}
```

`must` is the height the element cannot go below (visible floor); `might` is the height
it would take with unlimited space. Allocation gives each element `must`, then distributes
surplus up to `might` (same shape as the existing `compute_space_budget`, generalized from
2 to N elements).

### 3.3 Frame

```rust
struct Frame {
    buf: String,                     // flat, all rows concatenated
    extents: Vec<Range<usize>>,      // byte range per screen row
    spans: Vec<(usize, Range<usize>)>, // element idx → row range (for partial diff)
    caret: Option<(usize, usize)>,   // frame-relative (row_idx, display_col), 0-based
    height: usize,                   // == extents.len()
}
```

Renderers return `Vec<String>` (one string per **screen** row; multi-line `LogicalLine`s
already expand to screen rows inside `paint_plan`). The frame builder concatenates and
records ranges. Overlay rows are padded to `term_w`; input rows get a trailing `EL`.

### 3.4 Windowing

Both the input and the overlay may be taller than their allocation:

- **Overlay** — existing `visual_row_window` (centered on `focus`) — reuse as-is.
- **Input** — new: keep the caret row visible, preferring a few context rows above;
  clip to the allocation. This replaces the current "input grows unbounded and pushes
  scrollback off the top" behavior.

Extract the shared primitive: `window(heights, focus_row, budget, prefer_context)`.

---

## 4. ConsoleOutput cache & apply

### 4.1 Cache

```rust
/// Live screen bookkeeping in `ConsoleOutput`; one mutex pair with `writer`.
struct DrawCache {
    /// Absolute cursor position as we last left it: (1-based row, 0-based col).
    /// Row is authoritative (`cursor_row()`); col is best-effort.
    caret: (usize, usize),
    /// Terminal height at the last apply. The full-apply clear floor is
    /// `min(term_h, screen_bottom)`, so a maximize's revealed rows below the
    /// old bottom survive (today's `prev_term_h` / `clear_rows_to` bound).
    /// Set at construction; updated by every apply; never cleared.
    screen_bottom: usize,
    /// The last applied frame, while it is still intact on screen exactly as
    /// we drew it. `None` = invalidated → the next apply takes the full path.
    live: Option<LiveFrame>,
}

struct LiveFrame {
    frame_top_row: usize,     // absolute, 1-based, inclusive
    term_w: usize,            // size at apply time (partial predicate)
    term_h: usize,
    frame: Frame,             // rows for the diff; bottom = top + height - 1
}
```

The type lives with the rest of the pure drawing code in `src/ui/frame.rs`;
`ConsoleOutput` stores one as `draw: Mutex<DrawCache>` (replacing the old
`row: Mutex<usize>`) and `plan_apply(&mut cache, frame, anchor, policy, w, h)`
computes the bytes + the next cache state. `draw_frame` locks `writer` then
`draw` so the write and the cache advance are atomic (never the reverse — the
`display_*`/`note_lines` path is writer-then-cache too).

**Rows are stored absolute.** Everything emitted and consumed is an absolute row —
`CUP`, DSR replies, anchors — so the stored top/bottom are absolute too; the
bottom-relative quantities (`delta`, the `⌈2/3·term_h⌉` budget, the clear floor) are
computed per apply from `term_h`. Neither representation is invariant across a
vertical resize: terminals disagree on reflow anchoring (the `clear_rows_to_*` tests
pin both the top- and bottom-anchored models), so any size change invalidates `live`
and re-anchors via DSR rather than trusting either invariance.

**Caret is absolute** (not frame-relative): it must stay meaningful when no frame is
live — content writes advance it between frames — and every consumer of
`cursor_row()` wants an absolute row. When a frame is applied, the absolute caret is
derived as `frame_top_row + frame.caret.row`; never stored twice. `frame_anchor()`
(§5.1) returns `live.frame_top_row` while a frame is intact, else `caret.0`.

### 4.2 Invalidation contract

**Rule: every writer that can move the cursor maintains the cache before returning.**

| Writer | Action |
|---|---|
| Our own frame apply | updates `caret`, `live`, `screen_bottom` itself |
| `display_text`/`display_plain`/tool panels (`note_lines`) | advance `caret`; `live = None` |
| `emit_commit`-style frames (`Append`) | advance `caret` past the written rows; `live = None` |
| `$EDITOR` wrapper (`review_compaction_summary`, Ctrl-E) | `cursor_row()` (DSR) after exit → `caret`; `live = None` |
| Resize | `cursor_row()` (DSR) re-anchor; `live = None` |
| Ctrl-Z resume | `cursor_row()` (DSR) re-anchor; `live = None` |
| Dialog entry (`ui::confirm`) | `cursor_row()` (DSR) re-anchor — mid-turn the tracked caret does not follow streamed content; `live = None`. Afterwards the dialog itself maintains the cache through `draw_frame` |

`live = None` forces the full path on the next apply. Any uncertainty → clear it.

**`cursor_row()` is the explicit re-anchor primitive.** It keeps the historical
behavior — DSR with a tracked fallback — and additionally stores the answer into
`caret.0` (the tracked row when the DSR timed out) and clears `live` in *both*
cases: a caller asking to re-anchor cannot trust the frame region, and a
fallback is no more trustworthy than an answer (§9 latent issue 2).
Since P3 the REPL's steady-state reads go through `frame_anchor()`;
`cursor_row()` remains at the true re-anchor sites: entry into `read_input` and
`ui::confirm`, resize, editor exit, Ctrl-Z resume.

### 4.3 Full apply (scroll-then-paint)

```
delta = max(0, (anchor + frame.height - 1) - term_h)
top   = anchor - delta
```

All coordinates are computed **before** any emission; the whole sequence is one
`write_all`. There is no post-hoc caret-delta math — `top` is final.

1. If `delta > 0`: `CUP(term_h, 1)` then emit `delta` bare newlines. Precondition:
   newlines must start from the bottom row, otherwise leading LFs just walk the
   cursor down without scrolling. Same mechanism as the current `redraw` (bare `\n`
   → `\r\n` via ONLCR; scrolls at the bottom row); it pushes the scrolled-out top
   lines into scrollback — **never `CSI {delta} S`**, which deletes those lines
   instead of preserving them in some terminals (xterm.js#6010, codex#45091).
   `delta == 0` (steady state) skips the scroll entirely.
2. Paint and clear in one top-to-bottom pass over rows `from ..= to`:
   - `to = min(term_h, screen_bottom)` — the clear floor: a maximize's revealed
     rows below the old bottom stay intact (today's `prev_term_h` bound).
   - `from = min(top, old_top_after_scroll)` where `old_top_after_scroll =
     (live.frame_top_row − delta).max(1)`; with `live = None`, `from = top +
     height` (nothing above the new frame is known to be stale). This covers a
     re-anchor to a different row without invalidation — old frame bytes can
     never linger above the new top.
   - rows `top ..= top + height − 1`: `CUP r;1` + frame row bytes.
   - all other rows: `CUP r;1` + `EL` (absolute addressing; never scrolls).
3. `CUP` + `CHA` to the caret target.
4. Update cache: `caret` (absolute), `live = Some(LiveFrame { frame_top_row: top,
   term_w, term_h, frame })` (bottom derived as `top + height − 1`),
   `screen_bottom = term_h`.

Because `height ≤ ⌈2/3·term_h⌉` (allocator-guaranteed) and `delta` lands the frame
at `top ≥ term_h − height + 1`, the top 1/3 of the screen is never occupied by the
frame; the frame can always scroll itself fully into view.

### 4.4 Partial apply (default in steady state)

Predicate:

```
live.is_some()
  && delta == 0                       // this frame does not scroll
  && top == live.frame_top_row        // position stable
  && term_w == live.term_w            // layout stable
  && term_h == live.term_h            // size stable (reflow models differ)
```

Then diff `live.frame.extents[i]` vs the new extents: emit `CUP r` + row bytes for
changed rows only (one batched `write_all`, **no newlines** → can never scroll), clear
the shrink remainder if the frame shrank (`top+height ..= live.frame_top_row +
live.frame.height() − 1` — trusted here: the anchor is stable by the predicate),
reposition the caret. Identical frames emit nothing but the caret.

The `term_h` check matters: a size change we did not observe (no resize event
processed) must still force the full path, because the terminal may have reflowed the
frame region under us.

Row diff (not dirty flags) is the starting point — it catches input-growth shifts and
cross-element reflows that per-element flags miss; cost is negligible against I/O.
Dirty spans from `spans` are a later refinement if profiling demands it.

### 4.5 Contraction and fixed bottom elements (future)

The `Append` policy re-consumes the space *below* a block down to the terminal
bottom — the "contraction" mirror of the overlay/menu "expansion" of the bottom
drawing area. The natural generalization for a status line or a persistent input
line is to **compose those fixed parts into the same screen update** rather than
writing them ad hoc: a policy per chunk — **no-clear** on the content/scrollback
chunks, **clear** on the no-content chunks — with the fixed bottom elements
establishing the lower bound the cursor may write to (i.e. how far the screen is
scrolled back up before the non-scrollback elements are redrawn). Not needed for
P3/P4; recorded so the frame model grows in this direction instead of accumulating
side channels.

---

## 5. Wiring

### 5.1 UI surface

`Output` gains a small UI surface, separate from content-out (`display_*`; a later
step may unify content output under the same pipeline):

```rust
/// Draw a composed UI frame with its top row at `anchor` (1-based).
/// Default: drop it — non-console outputs carry no interactive UI.
fn draw_frame(&self, _frame: &Frame, _anchor: usize, _policy: ApplyPolicy) {}

/// Where the next frame belongs: the intact frame's top row when there is
/// one, else the current cursor row. Default: `cursor_row()`.
fn frame_anchor(&self) -> usize {
    self.cursor_row()
}
```

`ConsoleOutput` overrides both (`live.frame_top_row`, else `caret.0`; cache-aware
apply). The REPL and dialogs call `output.draw_frame(...)` through `&dyn Output`;
they never write frame bytes to stdout themselves.

The signature names `Frame`/`ApplyPolicy`, so those types are `pub` and the
`ui`/`ui::frame` modules are public (opaque: fields and the pure helpers stay
`pub(crate)`). The module is expected to graduate into a companion UI crate
alongside `slashdot_menu`, at which point the surface is public anyway; until
then it is the only way to keep `Output` (which `AgentConfig` exposes) clean.

### 5.2 REPL (`read_input`)

`redraw` becomes: compose `[Input, Divider, Overlay?]` → allocate → render →
`out.draw_frame(frame, out.frame_anchor(), ApplyPolicy::Frame)`. The `input_row` local
and `prev_term_h` `Cell` are retired; `frame_anchor()` replaces `next_input_row`'s
scroll tracking (the cache holds the post-scroll top). The REPL always applies an
empty-input frame before returning (as today's submit paths do), so no frame is live
when content streams under the next turn.

`next_input_row` / `clear_rows_to` folded into the apply implementation (P3); their
meaningful unit tests moved to `ui::frame::apply_tests`, the byte-mechanics ones
dropped with the helpers.

### 5.3 Dialogs (`ui.rs::confirm`)

Compose `[Divider?, Overlay]` — the divider is a `ConfirmSpec.divider` flag: mid-turn
dialogs (tool approval, compaction review) want a separation from the streamed output
above them; REPL-context dialogs pass `false` (scrollback already carries a separator
above the input region). Allocate, render, apply. The entry `cursor_row()` DSR is kept:
mid-turn the tracked caret does not follow streamed content, so `frame_anchor()` alone
would point into the stream; after that the dialog anchors via `frame_anchor()`, and if
it does not fit below, `delta` scrolls it into the live region. Key loop stays; each
key re-composes (cheap) and re-applies (partial). A resize re-composes at the new size
(full repaint; the dialog keeps its top).

On resolve the dialog is erased **through the pipeline**: applying `Frame::empty()`
under the Frame policy takes the union clear over the old (taller) dialog region and
parks the cursor on the region's first row — the general invariant (§2 clear policy):
the space below is already clean, so nothing needs a second clear and the next
append-only render can start right there. What is written at that row:

- **Tool approval** — one synthetic decision line, `accepted tool X` / `denied tool X`
  (subject derived from the `Confirm tool {NAME} call?` message convention; Esc
  records `denied`). No divider on the commit — scrollback already carries one above
  the input region.
- **Compaction review** — nothing: the committed `/tree <id> compact …` prompt is
  self-documenting; the dialog simply clears away.

Resolved P4 review item: the old `render_at` anchored the dialog *above* the cursor
and cleared with `\x1b[0J`; the frame pipeline anchors at the cursor and scrolls on
demand — `render_at` is deleted.

The zero-room case (cursor on the last row) is handled by the normal `delta` scroll —
the frame scrolls itself into the budget; it never overwrites above the cursor.

### 5.4 Stale doc reference

`repl.rs`'s module header still documents `Repl::select` as a single-column list
picker; the fn no longer exists. Fix the header while touching this file.
(Done in P3.)

---

## 6. Implementation phases

Each phase: green build + `cargo test` + `cargo fmt`.

| Phase | Work | Gate |
|---|---|---|
| **P0** ✅ | Test-safety net: make the vt100 tests consume the real emit path. Extracted `Repl::build_redraw(layout, prompt, overlay, sel, row, clear_to, w, h) -> (String, next_row)` from today's `redraw` (takes today's inputs, not a `Frame` — Frame arrives in P1). vt100 harness now feeds production bytes through an ONLCR shim (production emits bare `\n`; the tty maps to `\r\n`). Added 3 overlay tests pinning the scroll-delta caret math. | ✅ 674 tests green |
| **P1** ✅ | Frame model: `Element`, `Extent`, `Frame`, `allocate`, `render`, `window` (input windowing). Pure; lives in `src/ui/frame.rs` + `live_budget`. Unit tests: allocation distribution, windowing, extents/caret. | ✅ 21 unit tests; 695 total green |
| **P2** ✅ | Cache + apply in `ui::frame` (`plan_apply`, `DrawCache { caret, screen_bottom, live }`, `LiveFrame`, `ApplyPolicy`), stored as `ConsoleOutput.draw`. Trait surface `draw_frame(frame, anchor, policy)` + `frame_anchor()` (defaults drop UI; `Frame`/`ApplyPolicy` published — `ui` is a `pub mod`, the module is expected to become a companion UI crate). Full apply (scroll-then-paint, union clear, floor `min(term_h, screen_bottom)`), partial row-diff with the extended predicate, `Append` policy for commits. `note_lines` advances `caret` + invalidates `live`; `cursor_row()` = re-anchor primitive (DSR → `caret`, invalidates). Replaces the old `row: Mutex<usize>`. | ✅ 13 apply tests over `apply` output through vt100; 708 lib tests green |
| **P3** ✅ | REPL wiring: `redraw` → `Repl::compose_frame` (compose/allocate/render) → `Output::draw_frame(frame, frame_anchor(), Frame)`. Retired the `input_row`/`prev_term_*` locals (one `painted_size` cell remains for resize *detection* → DSR re-anchor + width wipe/replay; the clear bound lives in `plan_apply`'s `screen_bottom`) and the `initial_row` parameter (entry re-anchor via DSR inside `read_input`); Ctrl-Z resume re-anchors. `emit_commit` on the frame path without its own DSR — `$EDITOR` (`review_compaction_summary`) re-anchors on exit instead. Deleted `build_redraw`, `next_input_row`, `clear_rows_to`, `overlay_window`, `render_content`, `cursor_positioning` and their byte-copy tests; the vt100 suite now drives `compose_frame` + `plan_apply`. Behavior change: the 2/3 budget bounds the *whole* frame, so the pop-up gets `budget − input − divider` rows (the clamped-budget test was re-pinned: frame fits at its anchor, no scroll). | ✅ full suite green (702 lib + pty tests) |
| **P4** ✅ | Dialog wiring: `ui::confirm` composes `[Divider?, Overlay]` → `Output::draw_frame` (entry DSR re-anchor, then per-key/per-resize re-compose = partial applies); resolve = `Frame::empty()` union-clear, then a synthetic decision line for tool approval (`accepted\|denied tool X` via `ConfirmSpec.subject`) and nothing for compaction; `ConfirmSpec` gains `divider`/`subject`; `render_at` deleted. Also fixed the pre-existing `confirm_n_returns_no` miss (no `('n', 1)` quick key — the suite is `test-utils`-gated, so plain `cargo test` skips it). | ✅ `cargo test --features test-utils --test confirm_raw_mode` 7 green (incl. new paint → clear → decision screen test); full suite green |
| **P5** ✅ | Cleanup: deleted `overlay_lines`/`overlay_lines_with_bgs` (zero production callers) and ported their tests to `paint_plan` — marker tests (the `active == selected` case was already pinned by `paint_plan_active_marker_beats_cursor`), title/markers, wrap-continuation, and both `settings.rs` table-alignment tests; the per-row `row_bgs` test went with the removed feature (paint_plan has fixed backgrounds). Dropped `ui::frame`'s module `allow(dead_code)`: `Element::Lines` (§3.2's fixed rows, first user = §4.5) and `Frame::spans` (§4.4's dirty-span refinement) carry targeted allows instead. Stale `Repl::overlay_lines` reference in `slashdot_menu::table` docs fixed. (`next_input_row`/`clear_rows_to` folded into apply and `repl.rs` module docs updated in P3.) | ✅ full suite green (702 lib + pty tests; `--features test-utils` confirm suite 7/7); `overlay_lines` zero code hits |

P0 is not optional: it converts the existing carefully-tuned tests from "a copy of the
emit logic" into a real safety net for P2–P3.

---

## 7. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Byte-stream changes break subtle layout cases | P0 makes vt100 tests exercise production emission before anything changes |
| Scroll/clear regressions (the class of bug `next_input_row` exists to prevent) | Partial path has `delta == 0` guard; full path recomputes `top` from cache; vt100 tests assert final screen + caret |
| Cache goes stale after external writes | Explicit invalidation contract (§4.2); `live = None` on any uncertainty |
| DSR cost if used per-frame | Cache-first (`frame_anchor()`/`caret`); DSR only via the explicit `cursor_row()` re-anchor at foreign-process / size-change sites |
| Terminal reflow model (top- vs bottom-anchored) | No invariance is assumed: any size change invalidates `live`; clear floor preserves the reveal; DSR re-anchors |
| Behavioral change: input no longer pushes scrollback off the top | Intended; windowing keeps caret visible. Covered by a new test |
| Dialog divider in mid-turn context | `ConfirmSpec.divider` (true for both mid-turn dialogs; `false` reserved for REPL-context dialogs) |

---

## 8. Open questions (resolved)

1. **Scroll-on-demand cap.** `delta` lands the frame with its bottom at `term_h`; the top
   1/3 guarantee follows from `height ≤ 2/3`. Confirm no case needs a smaller `delta`
   (e.g. preferring to scroll less when the frame would fit higher).
   - Answer: If the sum of max-heights of individual elements is less than the
     budget, and the cursor position is at the bottom, then render to that
     sum, and only scroll to what is used. If the ask is for more than budget,
     then actual render height of elements will be scaled to fit.
2. **Input `must` height.** 1 row (caret only) vs. a small context floor (e.g. 3 rows).
   Affects tiny-terminal UX.
   - Input `must` is one row today. I would keep the min at that value for
     now, with the `max` scaled to fit actual wrapped contents. So in the
     typical menu situation with a short input, input min=max=1. The final
     assignment should balance between flexible elements. Maybe start with an
     even division of "flex rows" between flexible elements? If more tuning is
     required, weights could be added later - but keep it simple for now.
3. **Partial-repaint and the caret.** When only the caret moves (no row changes), is a
   caret-only emit worth a special case, or is it subsumed by the diff (both cheap)?
   - Up to you - if caret-only diffing is easy enough, then do so. If keeping
     it bundled with rows makes it simpler, then I think that would be fine
     too.

---

## 9. Follow-ups & take-aways (post-implementation)

Recorded while implementing P0–P5. None of these block the shipped pipeline;
the first two were latent bugs that pre-date ui_draw_v2 — both since fixed.

### Latent issues (both fixed)

1. **The confirm gate is never released.** `ui::confirm` stores `true` into
   `confirm_gate()` before drawing, and nothing ever stores `false` — but the
   in-flight reader checks that flag every tick. So after the *first*
   interactive confirm, every later in-flight reader is born paused and
   ignores Esc/Ctrl-C for the rest of the session (the mid-turn Esc handler
   died exactly this way). Fix: release the gate when the dialog resolves
   (every post-pause exit goes through `resolve()`). Pre-dates the draw work;
   orthogonal to it, hence untouched in P4.
   **Fixed:** `ui::resolve()` now stores `false` — restoring the release the
   pre-refactor `ConsoleOutput::confirm` had before the driver moved into
   `ui.rs`. Pinned by `confirm_releases_reader_gate_on_resolve`.

2. **DSR fallback leaves `live` intact.** `ConsoleOutput::cursor_row()`
   cleared `live` only when the terminal *answered*; on timeout it returned
   the tracked row without invalidating. A foreign writer on a silent
   terminal (headless pty) can therefore still take the partial path against
   a changed screen. The §4.2 contract says "any uncertainty → clear it" —
   the fallback should clear `live` too. Costs nothing (the next apply just
   takes the full path).
   **Fixed:** `DrawCache::reanchor` now takes the DSR answer as an
   `Option<usize>` — `Some(row)` records the row, `None` (timeout) keeps the
   tracked one — and clears `live` in both cases. Pinned by
   `reanchor_fallback_keeps_row_but_invalidates`.

### Small robustness cleanups

3. **Unify the size source.** `redraw`/`draw_dialog` compose with
   `crossterm::terminal::size()` while `ConsoleOutput::draw_frame` applies
   with `term_width()`/`term_height()` (width via the `TERMINAL_WIDTH`
   static). They agree at steady state, but a resize landing between the two
   reads fails the partial predicate's `term_w` check → one harmless extra
   full repaint. Passing the composed size into the apply (or reading one
   shared source) removes the class.

4. **`confirm_subject` parses a message convention.** The decision line's
   subject comes from stripping `Confirm … call?` off the tool-confirm
   header. If the `agent.rs` message wording ever changes, the line silently
   degrades to the full header (correct, just noisy). An explicit subject
   parameter threaded through `Output::confirm` would drop the coupling.

5. **`draw_dialog` sniffs the `> ` marker** to find the overlay's windowing
   focus. Fine for today's specs (no detail line starts with `> `), but a
   spec whose detail ever does would window around the wrong row. Robust
   form: `focus = plan.detail visual height + selected row's height offset`,
   the same arithmetic the REPL's pre-computed `focus` uses.

6. **`display_text` line counts can drift.** The count comes from
   `wrap(text)` while the bytes include the `⏺ ` prefix and
   `termimad::inline` output — which is the only reason the DoneError path
   re-anchors (DSR) after `display_text`. Accurate accounting would let that
   DSR go.

### Known limitations

7. **`paint_plan` under-buffers multi-line plans on large terminals**
   (found while porting the P5 tests; lives in `slashdot_menu`, not the draw
   pipeline). `compute_space_budget`'s "rows fit" branch returns `total_rows`
   — a *logical* count — as the *visual* budget, so every multi-line row is
   budgeted as one line and `visual_row_window` windows it even when the
   terminal has room to spare. Probe (9 rows × 3 visual lines, width 60):
   painted lines = **15 at term_h 24, but 9 at term_h 40 and 80** — a bigger
   terminal shows *less*. The fit branch should return the rows' visual
   height sum. Single-line rows (the normal menu case) are unaffected.

8. **Tiny terminals lose the pop-up, not the input.** Allocation serves
   floors in element order (input → divider → overlay), so on a ~2-row
   terminal the menu gets zero rows while the input keeps its row. Intended
   (budget ≥ 1, input floor first); just worth knowing when testing in tiny
   windows.

### Deferred design pieces (allowed for in code)

- **`Element::Lines`** — §3.2's fixed-row element carries a targeted
  `#[allow(dead_code)]`: commit lines go through `display_plain` today; the
  §4.5 fixed-bottom/status-line composition is its first production user.
- **`Frame::spans`** — §4.4's dirty-span refinement; the row diff is
  deliberately the sole reader until profiling asks for more.
- **`ApplyPolicy::Append`** — built and vt100-tested in P2, no production
  user yet; reserved for §4.5's chunked writes (no-clear on content chunks).
- **Per-row backgrounds (`row_bgs`)** — dropped with `overlay_lines` in P5;
  if per-row tinting is ever wanted it belongs in `RenderPlan` (engine
  side), not in the painter.

### Process take-aways

- **A feature-gated suite is not a gate.** `confirm_raw_mode` requires
  `--features test-utils`, so plain `cargo test` skipped it through P0–P3 —
  which is how the missing `('n', 1)` quick key survived until P4's gate run
  (it failed at baseline too). Run `cargo test --features test-utils` in CI,
  or drop the gate.
- **The P0 bet paid off.** Keeping the vt100 suite on production bytes meant
  the P3 pipeline swap was validated by the same carefully-tuned assertions.
  Exactly one intentional behavior change needed a re-pin — the frame-wide
  2/3 budget (pop-up gets `budget − input − divider` rows, scrolls less) —
  and manual testing has since confirmed it.
- **Streaming still doesn't track rows** (`stream_text`/`stream_thinking`
  bypass `note_lines`), so the cache's `caret` drifts mid-turn. That is why
  dialogs and `read_input` keep an entry DSR. Anything that wants a
  mid-turn `frame_anchor()` with no DSR (e.g. a live status line drawn
  while streaming) needs row accounting in the streaming writers first.
