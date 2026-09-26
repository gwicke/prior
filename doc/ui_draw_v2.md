# UI Draw v2 — unified frame composition for REPL, menus & dialogs

**Status:** Design settled — implementation in progress (P0 ✅, P1 ✅, P2 next)
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
| Partial apply | Row-diff repaint when `delta == 0 ∧ top unchanged ∧ width unchanged` |
| Clear policy | Full apply: clear `top..screen bottom` (conservative — anchor may have moved). Partial apply: row-diff + shrink remainder only (`top+height..=cache.bottom`) |
| Buffer | Flat `String` + per-screen-row extents `Vec<Range<usize>>` |
| Caret | `(row_idx, display_col)`, applied with `CUP` + `CHA` (`\x1b[{col}G`) |
| Anchor state | Owned by `ConsoleOutput`; all writers maintain it before returning |
| DSR | Only in foreign-process wrappers (`$EDITOR`); everything else uses the cache |
| Dialogs | Same pipeline, no input segment; anchor = cached input bottom |
| Commit lines | Compose `[divider, line]`, apply **without clear** → scrolls into history |

---

## 3. Architecture

### 3.1 Phases

```
compose(self, ctx)  →  Element list with (must, might)     [pure]
allocate(w, h, budget) →  Row ranges per element, extents  [pure]
render(alloc)       →  buffer + caret + element spans      [pure]
apply(frame)        →  escapes → stdout, update cache      [impure]
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
    caret: Option<(usize, usize)>,   // (row_idx, display_col)
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
struct DrawCache {
    row: usize,             // current frame top (input row), post-scroll
    bottom: usize,          // last row the previous frame wrote (clear boundary)
    caret: (usize, usize),  // maintained by our writes
    last_frame: Option<Frame>, // for partial repaint
}
```

Lives in `ConsoleOutput` (it already owns `row: Mutex<usize>` and the `writer: Mutex<...>`).
Apply takes both locks together so the write and the cache update are atomic.

### 4.2 Invalidation contract

**Rule: every writer that can move the cursor maintains the cache before returning.**

| Writer | Action |
|---|---|
| Our own frame apply | updates `row`/`bottom`/`caret`/`last_frame` itself |
| `display_text`/`display_plain`/tool panels (`note_lines`) | advance `row`; clear `last_frame` |
| `emit_commit`-style lines | advance `row`; clear `last_frame` |
| `$EDITOR` wrapper (`review_compaction_summary`, Ctrl-E) | **DSR** after exit → set `row`; clear `last_frame` |
| Resize | re-anchor via DSR; clear `last_frame` |
| Ctrl-Z resume | re-anchor via DSR; clear `last_frame` |

`last_frame = None` forces the full path on the next apply. Any uncertainty → clear it.

### 4.3 Full apply (scroll-then-paint)

```
delta = max(0, (anchor.row + frame.height - 1) - term_h)
top   = anchor.row - delta
```

All coordinates are computed **before** any emission; the whole sequence is one
`write_all`. There is no post-hoc caret-delta math — `top` is final.

1. If `delta > 0`: `CUP(term_h, 1)` then emit `delta` bare newlines. Precondition:
   newlines must start from the bottom row, otherwise leading LFs just walk the
   cursor down without scrolling. This is the same mechanism the current `redraw`
   uses (bare `\n` → `\r\n` via ONLCR; scrolls at the bottom row) and pushes the
   scrolled-out top lines into scrollback — **never `CSI {delta} S`**, which deletes
   those lines instead of preserving them in some terminals (xterm.js#6010,
   codex#45091). `delta == 0` (steady state) skips the scroll entirely.
2. `CUP top`, write frame rows — **no further scroll possible** since
   `top + height - 1 ≤ term_h` by construction.
3. Clear `top+frame.height ..= screen bottom`, `CUP r` + `EL` per row (absolute
   addressing; never scrolls). Conservative: the full apply is the rare path and the
   anchor may have moved (DSR re-anchor after resize/editor), so the old frame's
   cached coordinates are not trusted — clear the whole live region below the frame,
   as today's `clear_rows_to(row, clear_to)` does. Rows below the frame are the live
   region by definition; clearing them is always safe.
4. `CUP` + `CHA` to the caret target.
5. Update cache (`row = top`, `bottom = top + height - 1`, `caret`, `last_frame`).

Because `height ≤ ⌈2/3·term_h⌉` (allocator-guaranteed), and `delta` lands the frame at
`top ≥ term_h - height + 1`, the top 1/3 of the screen is never occupied by the frame.

### 4.4 Partial apply (default in steady state)

Predicate:

```
last_frame.is_some()
  && delta == 0            // this frame does not scroll
  && top == cache.row      // position stable
  && term_w unchanged      // layout stable
```

Then diff `last_frame.extents[i]` vs the new extents: emit `CUP r` + row bytes for changed
rows only (one batched `write_all`, **no newlines** → can never scroll), clear the shrink
remainder if the frame shrank (`top+height ..= cache.bottom` — trusted here: the anchor
is stable by the predicate), reposition the caret. Identical frames emit nothing but the
caret.

Row diff (not dirty flags) is the starting point — it catches input-growth shifts and
cross-element reflows that per-element flags miss; cost is negligible against I/O.
Dirty spans from `spans` are a later refinement if profiling demands it.

---

## 5. Wiring

### 5.1 REPL (`read_input`)

`redraw` becomes: compose `[Input, Divider, Overlay?]` → allocate → render → `apply`.
The `input_row` local and `prev_term_h` `Cell` are replaced by cache reads/writes
(`out.row()`, `out.caret()`). `emit_commit` keeps calling the same frame path.

`next_input_row` / `clear_rows_to` (currently `pub(crate)` in `repl.rs`) fold into the
apply implementation; their unit tests move with them.

### 5.2 Dialogs (`ui.rs::confirm`)

Compose `[Divider?, Overlay]` (divider conditional per context — tool confirm wants a
separation from the streamed output above), allocate, render, apply. Anchor =
`cache.row + cache.input_extent` (input already on screen; not redrawn). Key loop stays;
each key re-composes (cheap) and re-applies (partial). On resolve, a **commit frame**
(`[divider, decision line]`, apply-without-clear) records the choice in scrollback.

The zero-room case (input on the last row) is handled by the normal `delta` scroll —
the frame scrolls itself into the budget; it never overwrites above the cursor.

### 5.3 Stale doc reference

`repl.rs`'s module header still documents `Repl::select` as a single-column list
picker; the fn no longer exists. Fix the header while touching this file.

---

## 6. Implementation phases

Each phase: green build + `cargo test` + `cargo fmt`.

| Phase | Work | Gate |
|---|---|---|
| **P0** ✅ | Test-safety net: make the vt100 tests consume the real emit path. Extracted `Repl::build_redraw(layout, prompt, overlay, sel, row, clear_to, w, h) -> (String, next_row)` from today's `redraw` (takes today's inputs, not a `Frame` — Frame arrives in P1). vt100 harness now feeds production bytes through an ONLCR shim (production emits bare `\n`; the tty maps to `\r\n`). Added 3 overlay tests pinning the scroll-delta caret math. | ✅ 674 tests green |
| **P1** ✅ | Frame model: `Element`, `Extent`, `Frame`, `allocate`, `render`, `window` (input windowing). Pure; lives in `src/ui/frame.rs` + `live_budget`. Unit tests: allocation distribution, windowing, extents/caret. | ✅ 21 unit tests; 695 total green |
| **P2** | Cache + apply in `ConsoleOutput`: `DrawCache`, full apply (scroll-then-paint), clear-remainder, partial apply, invalidation contract. Route `note_lines`/`display_*` through cache maintenance. | vt100 tests over `apply` output |
| **P3** | REPL wiring: `redraw` → compose/apply; retire `input_row`/`prev_term_h` locals; `emit_commit` on the new path; `$EDITOR` wrapper DSR. | `repl_resize`, `popup_scroll_*`, vt100 suites green |
| **P4** | Dialog wiring: `ui.rs` on the frame path; divider policy; commit frame for the decision line; delete ad-hoc `render_at`. | `confirm_raw_mode` pty test + manual check |
| **P5** | Cleanup: delete `overlay_lines`/`overlay_lines_with_bgs` and port their tests to `paint_plan`; fold `next_input_row`/`clear_rows_to` into apply; update `repl.rs` module docs. | Full suite; `overlay_lines` zero hits |

P0 is not optional: it converts the existing carefully-tuned tests from "a copy of the
emit logic" into a real safety net for P2–P3.

---

## 7. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Byte-stream changes break subtle layout cases | P0 makes vt100 tests exercise production emission before anything changes |
| Scroll/clear regressions (the class of bug `next_input_row` exists to prevent) | Partial path has `delta == 0` guard; full path recomputes `top` from cache; vt100 tests assert final screen + caret |
| Cache goes stale after external writes | Explicit invalidation contract (§4.2); `last_frame = None` on any uncertainty |
| DSR cost if used per-frame | Cache-first; DSR only in foreign-process wrappers and resize |
| Behavioral change: input no longer pushes scrollback off the top | Intended; windowing keeps caret visible. Covered by a new test |
| Dialog divider in mid-turn context | Divider is a composable element; tool confirm adds it, menu-context dialogs don't |

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
