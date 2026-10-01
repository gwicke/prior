# UI Draw v2 — unified frame composition for REPL, menus & dialogs

**Status:** Design settled — implemented (P0 ✅, P1 ✅, P2 ✅, P3 ✅, P4 ✅, P5 ✅;
§4.5 U1/U2 ✅ in the API fold, U3–U6 not started). One end-to-end layout
assertion is off by one row — see §5.2.
Post-implementation follow-ups (§9): 7 done, 3 open, 1 accepted limitation —
audited 2026-09-28, recommended order at the end of §9.
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
| Clear policy | Full apply: clear the union of the old frame region (scroll-shifted) and the rows below the new frame, floored at `min(term_h, screen_bottom)`. Partial apply: row-diff + shrink remainder only. Append: no region clear (reserved for §4.5's chunked writes). **General invariant (P4):** a Frame apply clears the rows below the frame, so any later append-only render (and any row-diff) lands in already-clean space and never re-clears it. **To be restated by §4.5 U1:** with the area live across a turn, content *consumes* the area's rows instead of landing below them, so the invariant becomes "content writes stop at the fold; the area's rows are re-emitted, not erased". That is a change to a settled decision and wants its own commit and test |
| Buffer | Flat `String` + per-screen-row extents `Vec<Range<usize>>` |
| Caret | Absolute `(row, col)` in the cache (row authoritative, col best-effort); frame-relative `(row_idx, display_col)` inside `Frame`; applied with `CUP` + `CHA` (`\x1b[{col}G`) |
| Draw state | `DrawCache { caret, screen_bottom, live: Option<LiveFrame> }` in `ConsoleOutput`; all writers maintain it before returning |
| Screen position | Stored rows are absolute (everything emitted/consumed is); bottom-relative quantities (`delta`, 2/3 budget, clear floor) are computed per apply. No reflow-model invariance is assumed |
| UI surface | `Output::draw_frame(frame, anchor, policy, dims)` + `frame_anchor()`; default drops UI drawing; `ConsoleOutput` overrides and applies exactly `dims` (the size the frame was composed for — §9 #3), returning what it applied. Content-out (`display_*`) and UI-out stay separate |
| DSR | Only via the explicit `cursor_row()` re-anchor primitive (foreign-process / size-change / dialog-entry sites); steady state uses the cache |
| Dialogs | Same pipeline, no input segment; entry DSR re-anchor, then `frame_anchor()`; the composer's divider rule separates mid-turn dialogs from streamed output. Resolve = `Frame::empty()` union-clear; the optional decision line is written at the cleared first row |
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
    /// Prompt + editor content. `must` = 1 (or min visible), `might` = layout
    /// height. The caret is *derived*, not stored: `layout.cursor_row` /
    /// `layout.cursor_col`.
    Input { layout: Layout, prompt: String },
    /// Dim horizontal rule. must == might == 1.
    Divider,
    /// Pre-painted overlay rows (from `paint_overlay`), focus row for windowing.
    Overlay { rows: Vec<String>, focus: usize },
    /// Fixed text rows (commit lines, future status line).
    Lines(Vec<String>),
}
```

`must` is the height the element cannot go below (visible floor); `might` is the height
it would take with unlimited space. Allocation gives each element `must`, then distributes
surplus up to `might` (same shape as the existing `compute_space_budget`, generalized from
2 to N elements).

#### 3.2.1 How the two passes decide

`allocate` is one early return and two rounds:

```
if sum(might) <= budget { return might }          // no pressure: everyone is served
fill(heights, musts, budget)                      // pass 1: floors
fill(heights, mights, budget − granted)           // pass 2: surplus toward wants
```

`fill` distributes the pool in **even shares per round**, with unused shares falling back
into the pool for the next round, and hands out one row each in element order when the
pool is smaller than the number of claimants.

Two consequences worth writing down rather than re-deriving:

- **There is no priority.** Nothing declares "the menu yields to the input": under
  pressure they compete on even shares, and the only ordering anywhere is that
  `share == 0` tie-break. It works out today because the overlay's `might` (~2/3·term_h)
  dwarfs the input's (1–3). If deliberate yielding is ever wanted — the input owns the
  caret — it is not expressible; it would mean lowering another element's `might` first,
  i.e. a second pass, which is the complexity `window` exists to avoid.
- **`Lines` never competes.** Its `must == might == rows.len()`, so it is fully served by
  pass 1 regardless of what else is asking. That is what `must` is *for*, and it is why the
  status row can be added without the composition having to protect it.

In `src/ui/frame.rs`, production code matches on the variants in exactly **two** places —
`extent()` and `render()`. Everything else (`Frame`, `spans`, `allocate`, all of `plan_*`)
works in rows and byte ranges and is variant-agnostic. (Other modules *construct* variants;
none pattern-matches one.)

#### 3.2.2 The squeeze policy is in the wrong layer

`render` hardcodes *how an element fits when it is squeezed*:

| element | policy when squeezed | whose knowledge |
|---|---|---|
| `Input` | `window(n, cursor_row, h, INPUT_CONTEXT_ROWS = 3)` | the editor's |
| `Overlay` | `window(n, focus, h, h / 2)` | the menu's |
| `Divider` | 0 or 1 | neither |
| `Lines` | `rows.iter().take(h)` | — |

Two findings:

- **`Lines` has no notion of focus.** `take(h)` keeps the *front* and drops the rest, so
  the row the reader was looking at can be the one cut. It does not bite today — `Lines`'
  planned users are the status row (one row) and commit lines (append-only), both
  focus-free — but the variant's doc claims the opposite ("truncated from the top"), so
  code and doc disagree about *which* end is cut, and about which should be.
- **`INPUT_CONTEXT_ROWS` and `h / 2` are opinions living in `frame.rs`.** The editor knows
  it wants three rows of context; the menu knows it wants half its budget above its focus.

The general form: **the enum invites the frame layer to accumulate squeeze knowledge that
its callers already hold**, and every new element kind adds another arm to a `match` in a
module that should only know about rows and byte ranges.

#### 3.2.3 The item splits into (data, polymorphic render)

The requirement that constrains any replacement:

> `allocate` needs `must`/`might` for **every** item before it renders **any** of them
> (§3.1's two passes). So the sizing declaration must be *data* on the item, and only the
> rendering may be polymorphic.

```rust
struct Item {
    must: usize,
    might: usize,
    render: Box<dyn RenderAt + Send + Sync>,
}

/// Height in, rows out. `term_w` is a parameter, not a dependency — §3.1's
/// "no terminal dependency" still holds.
trait RenderAt {
    fn render_at(&self, h: usize, term_w: usize) -> Rendered;
}

struct Rendered {
    rows: Vec<String>,
    /// Frame-relative (row, col), 0-based. Only the input carries one; every
    /// other item returns `None`.
    caret: Option<(usize, usize)>,
}
```

The caret is why the return type grows: today `render`'s `Input` arm sets `Frame.caret`
directly. Under polymorphism there is no arm, so each item answers for itself and items
with no caret return `None`.

Three ways to carry the render half:

| | enum (today) | `Arc<dyn Fn(usize) -> Rendered>` | trait object |
|---|---|---|---|
| squeeze policy | in `frame.rs` ✗ | with the owner ✓ | with the owner ✓ |
| new kind | edit both `match`es | drop in ✓ | drop in ✓ |
| exhaustiveness | compile error ✓ | none — silent | none — silent |
| sizing + render kept in step | one `match` | **two independent fields that can disagree** | one `impl` ✓ |
| storage | `Send` ✓ | needs `'static` + `Send + Sync` | needs `'static` + `Send + Sync` |

On storage: the last two rows are **equal**, and `Box` suffices for either — an item is
owned by exactly one composition at a time and is only ever moved (§5.1.1). `Arc` would
be needed only if a composition were shared across owners, which nothing does.

**Chosen: a trait.** It buys everything the closure buys — policy back with its owner, no
`match` in `frame.rs` — while keeping the sizing and the rendering in one `impl`, so
`must`/`might` cannot drift from what `render_at` will actually produce. Both require the
item to own what it captures: `Output: Send + Sync` and `draw: Mutex<DrawCache>` rule out
any borrow of a caller's editor, which is the same rule the element list already follows
(the REPL builds a fresh one per keystroke and moves it).

Not urgent. Four kinds cover the whole frame stack (§4.5: status, input, menu, divider)
and nothing in §4.5 needs a fifth. Do it *with* the status row rather than before it,
because the status row is the first item whose squeeze policy belongs to someone other
than `frame.rs`.

#### 3.2.4 What does not change

- **`allocate`** — both passes, even shares, the `musts`-first floor. It never looked at
  a variant.
- **`window(n, focus, budget, prefer_context)`** — unchanged; only *whose*
  `prefer_context` it is changes.
- **`Frame`, `spans`, and all of `plan_*`** — rows and byte ranges only.
- **Purity.** `render` is still `(items, heights, term_w) -> Frame` with no terminal
  dependency; `h` and `term_w` arrive as parameters, exactly as §3.1's `render(alloc)`
  says.

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
already expand to screen rows inside `paint_overlay`). The frame builder concatenates and
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
| `$EDITOR` wrapper (`run_editor`: Ctrl-E prompt edit, compaction draft edit) | `cursor_row()` (DSR) after exit → `caret`; `live = None` |
| Resize | `cursor_row()` (DSR) re-anchor; `live = None` |
| Ctrl-Z resume | `cursor_row()` (DSR) re-anchor; `live = None` |
| Dialog entry (`ui::confirm`) | `cursor_row()` (DSR) re-anchor; `live = None`. Afterwards the dialog itself maintains the cache through `draw_frame`. (The reason used to be "the tracked caret does not follow streamed content" — false since the streaming writers started counting, see #10. Kept as a cheap confirmation on a path where a wrong anchor strands the dialog; §4.5 U5 revisits it.) |

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

### 4.5 The fold: one composed, relatively-drawn area below the content

**Status: the direction, not a future phase.** The drivers are in-turn input
(typing during a turn) and menu operation during a turn. Both are impossible
today for one reason, and this section is about that reason.

**The end state.**

- **The REPL drives UI composition; `ui.rs` is where it belongs.** The REPL
  decides *when* to compose and supplies the pieces (its editor, its overlay);
  `ui.rs` is the natural owner of the composition logic and state, and the place
  to move things out of `repl.rs` — which at ~3900 lines still carries a line
  editor, terminal plumbing and the overlay enum alongside the loop itself.
  `Output` contributes only what it alone can read, the status row (§5.1.1). One
  owner sees everything, so there is no ID/addressing scheme and no shared
  mutable stack to add to or prune.
- **The area lives across the turn.** Content writes stop at the fold and the
  area is re-emitted below them — never erased. The content path does not
  compose: it re-draws the element stack the last composer stored — a recipe,
  not pixels, so the status row inside it is re-derived on every draw.
- **The status bar is a row of that area,** so its dot flips in place. No
  transient content-line bar, and no second bar anywhere.
- **The prompt stays live mid-turn:** the input row is part of the area while a
  turn runs, the menu opens over it (browsable, read-only), and `Esc` steps the
  menu back a level, stopping the turn only at root. **Submit is blocked
  mid-turn in v0** — pre-compose the next prompt and
  browse, but do not commit one. Steering is a later iteration, most likely a
  message injected at the next natural segment boundary.

**The steps:** contract the content path onto the fold, make the bar an area
row, fold the dialog into the same area, then keep the prompt live mid-turn —
P1, P2, P3, P4 below. P5 and P6 gate nothing.

#### 4.5.1 Terminology: the fold

The **fold** is the boundary between scrollback and the bottom drawing area: the
topmost row of the non-scrollback region. Everything at or below the fold is
*the area*, and everything above it is *content*.

The fold is **not** a fixed row and the area is **not** pinned to the terminal
bottom. The area sits immediately below the content, and content pushes it
down; when a menu opens the fold moves *up* (expansion) and when it closes the
area descends back into the space it freed (contraction). Rows below the area
are ordinary empty screen.

Expansion is **not** a separate mechanism: emitting the taller area from the
fold lets the terminal's own scroll push the content above up to make room. It
is the same emit-from-the-fold operation contraction uses, only taller — so
growth adds no apply case of its own.

An earlier draft of this section called the area "pinned", which is wrong, and
the distinction is worth stating precisely because the two designs converge:
**nothing is reserved.** The content region is not held at `th - h` rows from the
start, so at the first prompt of a session the area is right under the greeter,
mid-screen, with empty rows beneath it. As content accumulates the area walks
down; once the screen is full it stays at the bottom, because further content
scrolls rather than grows. Same steady state, different mechanism — and the
mechanism is what keeps the first prompt cheap.

The area is also **one frame, not several**: status, input, divider and overlay
are a single element list, allocated, rendered and applied once. "Several
disjointed writes" is what this whole section removes, and the mid-turn status bar
is already one — which is why it has to go rather than be extended.

#### 4.5.2 What "fully-relative" means here, precisely

Two readings, and the difference decides how much is achievable:

- **(a) No absolute addressing while we own the screen.** Every content write,
  every area apply, and every fold move is relative (`CR`, `EL`, `CUU`, `CUD`,
  `LF`) and no `CUP` is emitted between the start of a turn and its end. **This
  is achievable and is the target.**
- (b) No `CUP` ever, including after a foreign writer. **Not achievable** while
  `$EDITOR` (Ctrl-E, compaction drafts) and Ctrl-Z resume exist: a cache cannot
  know where a foreign writer left the cursor, and the only alternative is a
  full-screen repaint that itself needs to know the scroll offset — i.e. the same
  query by another name. OQ1 asks which reading is wanted.

So the honest end state is (a), and it is the settled target (4.5.5): absolute
addressing survives only at **startup** and in **foreign-writer** recovery, where
it is *information the cache does not have* rather than an architectural gap. The
three sites that stay are `read_input`'s very first anchor, `$EDITOR` exit, and
Ctrl-Z / width-change.

#### 4.5.3 What blocks it today

Three facts, all of which are consequences of one decision:

1. **The area is retired by the first content write of a turn.** The REPL applies
   an empty-input frame before returning (`redraw(&LineEditor::new(), …)` on the
   submit path), and `with_content_writer`'s `plan_clear_live_frame` then erases
   it and sets `live = None`. For the rest of the turn there is no area at all.
2. **Content and a live area therefore never coexist.** The only production
   `draw_frame` callers are the REPL's `redraw` and `ui::confirm`, and the
   latter blocks. So §4.5's no-clear content policy has had nothing to operate
   on: `plan_clear_live_frame` returns empty for essentially every write of a
   turn.
3. **The status bar is a content line, not an area row.** `paint_status` /
   `clear_status` / `StatusSpot` and the gap logic (`row_accounting` §3.5) exist
   *only* because there is no area during a turn. The bar is painted at the
   content's live edge, erased by the next content write, and jumps to the
   content's edge at submit. That jump is not a bug in the drawing — it is the
   area being replaced by the content's own bottom edge — but it is a symptom of
   the area not existing.

#### 4.5.4 Target invariants

Each is checkable, and each is what a phase below establishes.

- **I1 — One owner.** At every instant exactly one component composes the area.
- **I2 — Content and the area never claim the same row *at rest*.** At the end
  of every screen update the screen partitions into content above and the area at
  or below **the fold as of that moment** — and the fold is a *value that moves*,
  down as content is written and up as the area grows, so this is a statement
  about a moment, not about a fixed line. Within a content write the area's top
  rows are transiently overwritten and then re-emitted; that transient is the
  mechanism, and the partition is restored before the update ends. The thing most
  likely to regress is the restore: an area that is cleared underneath its own
  content rather than painted over it.
- **I3 — The fold is derived, not remembered.** It is a function of the tracked
  caret, the area's height and `term_h` — never a separate counter.
- **I4 — Steady state is relative.** Between a turn starting and ending, no
  `CUP` is emitted (4.5.2 (a)).
- **I5 — The area is never blank mid-turn.** Today the input region is erased at
  submit and rebuilt later; on a fast stream that is a visible flash. I5 says
  the area is continuously valid or is deliberately, visibly replaced.
- **I6 — Scrollback stays contiguous.** Scrolling back shows content with no
  gaps and no area rows interleaved.

#### 4.5.5 Settled: the four open questions, answered

Answers recorded 2026-09-30; OQ2 was the last to close. They changed the phase
list, so 4.5.6 reflects them.

- **OQ1 → (a).** Steady-state relative drawing: no `CUP` between a turn's start
  and end. Absolute addressing is excluded only at **startup** and after
  **foreign writers** (`$EDITOR`, Ctrl-Z, the width-change `ESC[0J` + replay) —
  the three `cursor_row()` sites that stay. `ui::confirm`'s and `read_input`'s
  entry DSRs are in scope for removal once the fold survives the turn (U5).
- **OQ2 → the prompt stays live mid-turn.** The input row is part of the area
  while a turn runs and the menu opens over it. What is *allowed* narrows:
  - **Submit is blocked in v0.** Pre-composing the next prompt and browsing the
    menu (`/stat`, `/tree`, …) are open; committing one is not. This gets the
    drawing right without first answering what a submitted prompt *means*
    mid-turn.
  - **The menu browses; changes defer.** Per-turn overrides already apply per
    request and never to the config (`model_settings_v2.md`), so a setting
    change would only reach a future request — deferred by policy, not danger.
  - **`Esc` stops the turn — at root.** U4's settled rule refines this: with a
    v2.5 menu open it steps the ladder instead (menu-first, abort only at
    root), which is why it is forwarded to the turn rather than handled in the
    reader. Either way it still reaches the running tool through
    `TOOL_CANCEL_SENDERS`; that registry survives as the *mechanism*, while
    "kill this child" stops being the whole meaning.
  - **Steering is a later iteration:** most likely a message injected at the
    next natural segment boundary — which is also the thing that would let
    submit unblock.
- **OQ3 → no pinning, and the area *descends*; and one frame, not several.**
  Both are 4.5.1's statement rather than exceptions to it: nothing is reserved,
  the composition is a single element list with one `allocate`/`render`/`apply`,
  and a content-line status bar is therefore a disjointed write — which is why
  it goes rather than being extended.
- **OQ4 → a dialog is just another menu.** A confirmation composes into the same
  area, below the status and the input row when there is one, so `ui::confirm`
  stops being "a frame without an input segment" (§5.3) and becomes the same
  area with a different element list; its DSR and its own anchor go with it.

#### 4.5.6 Phases

**U1–U3 are the drawing capability** and have to land before anything can be
*live* mid-turn: there is no area to keep open while content still retires it
(U1) or while the bar is still a content line (U2). **U4** is the in-turn
prompt, which OQ2 now answers. **U5** is the payoff and **U6** an optional
optimisation that gates nothing. Each unit is separately reviewable and
separately verifiable.

**Why `U` and not `P`.** The phase table above numbers the *capability* work
`P0`–`P5`, and this list numbered the *delivery* work `P1`–`P6` — so one label
meant two different things. `P1` was `Element`/`Frame`/`allocate` in the table and
contraction here; `P4` was dialog wiring in the table and in-turn input here.
Anyone planning against the table would have read in-turn input as done, which it
is not. `P` is capability, `U` is delivery.

**U1 — Contraction: the area establishes the content's lower bound.**
`with_content_writer` stops retiring the area. Content is written from the fold
downward, the terminal scrolls as the write reaches the bottom, and the area is
re-emitted **once**, as a single frame, at its new fold. `ConsoleOutput`
re-draws the element stack it already holds — the concrete form of "compose
those fixed parts into the same screen update", and why the cache keeps the
stack rather than the rendered `Frame`.

The apply this needs is a *reposition*: the frame's top row has moved down by the
content's row count, and the rows it used to occupy now hold content, so nothing
may erase them. That is neither of the existing two paths — `plan_partial`
requires an unmoved frame, and `plan_full` erases the old region, which would
erase the content that just landed there. It is a third case: walk to the new
top, paint, do not clear above.

**There are two write mechanisms, and which one is in play is the fold's job.**
This is the part that is easy to get wrong, because the obvious case is the
*transient* one:

- **Fold moves (screen not full).** Content is written *at* the fold, so it
  consumes the area's top rows, the fold moves down by the content's row count,
  and the area is re-emitted at its new position. This is OQ3's "moves gradually
  toward the bottom", and a reposition is unavoidable — there is nowhere else for
  the content to go.
- **Fold stationary (screen full).** The area is already at rows
  `[th-h+1, th]` and the content region is `[1, th-h]`. Here the re-emit *is*
  avoidable in principle, but **not by scrolling**: a terminal scroll moves every
  row up, so it displaces the area just as surely as the content did — the area
  ends at `[th-h, th-1]` with a blank row beneath, and has to be painted back
  down. Scrolling cannot preserve the area, because a scroll is not selective.

  What *can* preserve it is a **scroll region** (`DECSTBM`,
  `CSI top ; bottom r`): set it to the content region `[1, th-h]` and the
  terminal's own scrolling stays inside it, leaving the area's rows untouched.

  Note what that costs in shape as well as state: the content has to be written
  *inside* the region, so the write point is the region's **bottom** row
  (`th-h`), not the fold. The content flows upward because the terminal scrolls,
  rather than downward because we push it. So the stationary case has a different
  write point from the moving one, and choosing between them is a real decision
  rather than an optimisation of a single mechanism. In exchange the fold becomes
  self-maintaining — the terminal moves the content, not us — and a content chunk
  costs nothing but its own bytes.

  U1 as scoped does **not** do this: it writes at the fold in both cases, so it
  has one mechanism and one write point. The scroll region is a separate, later
  optimisation (U6) rather than a refinement of U1.

  **Deferred, not dismissed** (U6). The saving is real but bounded (one area
  re-emit per chunk: ~2 rows, a few hundred bytes), and the cost is a *global
  terminal mode* that every write path must honour: a resize, a full repaint, the
  dialog path and (later) in-turn input all have to re-issue or reset it, and a
  path that forgets scrolls the wrong part of the screen — a bug class that is
  miserable to diagnose from a transcript. Recommend measuring the re-emit cost
  first; the region does not change I1–I6, so it can land whenever the
  measurement justifies it. It is written down because U1 will *look* like it is
  re-emitting something avoidable, and the next reader deserves to know it was
  considered.
- Establishes I2, I3, I6, and the "one frame" half of I1.
- Verify: the post-turn screen is unchanged from today's (the transcript does not
  move), plus a new vt100 case where a multi-chunk stream never leaves the area
  blank (I5) and one where the area is re-emitted with no `CUP`.
- Risk: **P4** (the clear-policy invariant) has to be restated — content now
  *consumes* the area's rows rather than landing below them. The invariant
  becomes "content writes stop at the fold; the area's rows are re-emitted, not
  erased". That is a change to a settled decision (the P4 row in §2) and wants
  its own commit and its own test.
- **Scope: the `CR` cleanup is required — P2 first, or P1 and P2 combined.**
  `plan_consume`'s `CUU` preserves the column, so a turn's first write would
  land at the frame's caret rather than at column 0 — masked today only because
  the transient bar's own clear ends in `CR`. Once the bar is a frame row the
  accident is gone, which is what `plan_consume`'s "the bar question… is
  answered first" note means. Either land P2's bar move before P1's wiring or do
  the two as one change; there is no preference between them, but
  `plan_consume`'s explicit `CR` must carry column 0 on its own before P1's
  wiring goes in.

**U2 — The area survives the turn; the bar becomes a permanent row.**
The REPL stops applying an empty-input frame to retire, and the content path
stops clearing. The status bar moves out of the content stream and into the area
composition as `Element::Lines`, and its dot is driven by real turn state (grey at
the prompt, green in flight) — which is what makes the two states mean something
and removes the jump at submit. `StatusSpot`, `clear_status`, `paint_status`'s
cursor-hide/dim dance and the `row_accounting` §3.5 gap logic all become dead and
are deleted; §3.5's gap tests go with them, restated as "the area's first row is
preceded by a blank" if that is still wanted (it is: the gap is why the bar is
readable below a tool panel).
- Establishes I1, I4, I5.
- Verify: the dot flips on the *same row* with no second bar anywhere; a resize
  mid-turn re-flows the area; `plan_clear_live_frame` has no production callers
  and is removed.

**U3 — Dialogs are menus in the area.**
`ui::confirm` composes `[Status, Input?, Divider?, Overlay]` through the same path
as the REPL, below the status and below the input. The two areas of duplicated
logic §1 named — the dialog's own anchor and its `\x1b[0J` — go away, and so does
its entry DSR.
- Establishes I1 for dialogs, and closes half of P5.
- Verify: a mid-turn tool approval shows the input row above it; the dialog's
  rows are erased through the pipeline on resolve and the area comes back as
  `[Status, Input]`; **a positional assertion is required** — the confirm suite
  asserts behaviour (which option was chosen) and not the dialog's row, so
  "green" is not evidence for anything here.

**U4 — In-turn input. (Unblocked by OQ2; depends on U1–U3.)**
The read loop stays alive while the turn runs: the input row is part of the area,
the menu opens over it, `Esc` is menu-first (stopping the turn only at root), and
**submit is blocked in v0** (4.5.5). Two pieces of work, and the drawing is not
the hard one:

- **The prompt session has to leave `read_input`'s frame.** `editor`, `nav`,
  `v25_menu`, `v25_handle`, `prose` and `pending_key` are locals there, so a
  mid-turn key cannot reach any of them. Lifting them into something the turn's
  loop can drive is the actual work. The turn's `tokio::select!` (agent.rs)
  already pumps the stream and currently has exactly one branch, so adding an
  input branch is mechanical — moving the state is not.
- **I1 under concurrency.** The streaming path and the read loop both want the
  area. The resolution holds: the read loop is the area's sole composer, the
  content path writes above the fold and asks for a re-emit (U1's mechanism),
  and the two *interleave* in the `select!` rather than running in parallel.

Growing the area mid-turn (the input popping up) adds **no new apply case** —
see 4.5.1: emitting the taller area from the fold pushes the content above up
by the terminal's own scroll.

**Plan (2026-10-01). One pump, one composer.** The stream branch and the new key
branch are arms of the *same* `tokio::select!` in `prompt()` (agent.rs:380), so
they interleave by construction: no mutex, and the remembered `element_stack`
cannot be stale relative to a compose because there is no parallel compose.

**Reader: keep `spawn_in_flight_reader` as a thread, add a channel.** It already
drops edit keys ("cannot be typed into the prompt mid-request") — U4.2 makes that
arm send instead. Not `crossterm::EventStream`, though `event-stream` is already
enabled and it would delete the thread, the 50 ms tick, the channel and the
`done`/`join()` handoff:

> **U4 requires a pauseable reader, and `EventStream` cannot be paused.**
> `run_editor` (editor.rs) owns a stdin window covering *both* the `$EDITOR`
> child and the post-editor DSR re-anchor — "its reply arrives on stdin, so an
> unguarded reader would swallow it" — and it is reachable **mid-turn** through
> the v2.5 menu (`v25_listing_node`, `v25_tree`), which U4 exists to open.
> `ReaderHold` gates the thread with one `load()` before every `poll`; crossterm's
> only disarm is `Drop`. EventStream would be safe only while nobody polls it
> during the window — safe by timing, not by policy, and broken silently by
> making the arm `.await`ed or catching a key mid-window.

OQ4 makes `confirm` a menu, so it stops needing the gate — but the editor never
can: it hands the tty to a child and reads a DSR back. Unifying confirm retires
one gate user, not the gate. (Two further reasons the thread keeps its duties:
Ctrl-C / Ctrl-Z / Resize must stay *inline* so an abort cannot ride behind a
paste backlog — `Esc` was in that list until U4.3 gave it a decision to make,
which this thread cannot make without a session, so it forwards now — and
`done` + `join()` is what makes `read_input` the sole reader again — a
correctness property, not a detail. The 50 ms is only how long an *idle* `poll`
takes to notice those flags; keystroke latency is ~0.)

**Four units, separately green:**

- **U4.1 — extract `PromptSession`** out of `read_input`'s frame (`editor`,
  `nav`, `history`, `draft`, `prose`, `pending_key`, `v25_menu`, `v25_handle`,
  overlays). `read_input` keeps the blocking loop and the drawing and drives it.
  **`handle_key` draws nothing** — it mutates and returns `Option<InputAction>`,
  the caller decides what to draw. One dispatcher, two pumps; generalises the
  `edit_key` split (repl.rs:1631). Largest diff, **no behavior change**.
- **U4.2 — forward edit keys** to an unbounded mpsc. Urgent keys stay inline.
  Unbounded because a bounded channel that fills would block the reader at the
  moment it most needs to deliver Ctrl-C.
- **U4.3 — the key branch** in `prompt()`'s `select!`, plus a timer arm for
  `prose.poll_timeout()` (repl.rs:1031) or prose goes inert mid-turn. Submit
  behind a policy flag. First point at which mid-turn typing works.
- **U4.4 — re-emit ordering:** key branch → `draw_elements` (compose + remember);
  content branch → write + `reposition_area`.

**Settled.** All four open questions answered:

- **`Esc`** — menu-first; abort the turn only at root, which keeps "`Esc` aborts
  the turn" true at the prompt (agreed below).
- **Submit mid-turn** — *silent* while submit is blocked in v0: Enter does
  nothing and says nothing. A real actions mechanism comes later.
- **Prose mid-turn** — *live*, but with no submit: it may draft the next prompt
  and may open the menu, it may not send.
- **Session lifetime** — **one `PromptSession` carried across turns**, never
  reconstructed.

Two rules follow from the lifetime answer, and both are restraints rather than
mechanisms:

- **The `/` prompt *is* the menu state.** There is no parallel menu state to go
  stale across a turn boundary: whatever the buffer holds *is* the menu, so
  carrying the buffer is carrying the menu, and there is nothing for a
  prompt-boundary reset to protect. The epoch contract therefore stays exactly
  as it is — `get_handle` bumps on a fresh `/`-entry and every node's
  `EpochState` (armed confirm, `view_args`, cursor) resets there — and
  `confirm_prompt` is already keyed to the buffer, so a rewrite disarms.
  "Menu cursor carried or reset": carried; do not touch it.
- **No queuing, no holding back.** Keys are evaluated and dispatched
  *throughout* the turn as they arrive, one at a time. The turn's key branch
  evals after every key rather than parking a `pending_key` for a later
  iteration — which also makes the cross-boundary `pending_key` leak
  unrepresentable instead of merely theoretical.

- Verify: keys typed during a turn reach the editor; the area's row diff is
  correct under interleaving of chunks and keys — this wants a test that drives
  both from one thread on a deterministic schedule, not a pty race.
- gwicke: Agreed, only abort turn when at prompt (empty or not), not when
  overlay / menu is active.

**U5 — Retire the DSRs that are no longer load-bearing.**
With the fold maintained across turns and every content write counted (done: #10),
`read_input`'s entry DSR and `ui::confirm`'s (P3) stop being load-bearing.
Startup and the three foreign-writer sites stay, by OQ1.
- Verify: each removal is a separate commit with the suite green and a note on
  what now covers it.

**U6 — Scroll region, if the re-emit measures expensive. (Optional.)**
Set the terminal's scrolling region to the content area so the fold is maintained
by the terminal in the stationary case, and a content chunk costs only its own
bytes. Changes the content write point in that case (region bottom, not the fold).
Only after measuring; see the P1 note above.

#### 4.5.7 Not in scope

- **The area being pinned to the bottom row.** See 4.5.1 — settled as *not*
  pinned, with nothing reserved. The two designs converge in the steady state;
  they differ in what happens before the screen fills.
- **Per-row backgrounds in the area** (`row_bgs`, dropped in P5) — unrelated.
- **Scrollback rewriting.** The area is chrome and never enters the scrollback;
  I6 says the *content* stays contiguous, not that the area is preserved.
- **Steering and the submit queue.** v0 blocks submit mid-turn (4.5.5), so what
  a prompt *means* while a turn runs — queue depth, a message injected at a
  segment boundary — is deferred. The drawing does not depend on it: the input
  row is live either way.

---

## 5. Wiring

### 5.1 UI surface

`Output` gains a small UI surface, separate from content-out (`display_*`; a later
step may unify content output under the same pipeline):

```rust
/// Draw a composed UI frame with its top row at `anchor` (1-based).
/// `dims` is the (width, height) the frame was composed for: the apply
/// uses exactly these, so the frame's bounding box can never disagree
/// with its content (§9 #3). Returns the dims applied (clamped), for
/// the caller's repaint bookkeeping.
/// Default: drop it — non-console outputs carry no interactive UI.
fn draw_frame(
    &self,
    _frame: &Frame,
    _anchor: usize,
    _policy: ApplyPolicy,
    _dims: (usize, usize),
) -> (usize, usize) {
    (dims.0.max(1), dims.1.max(1))
}

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

#### 5.1.1 One pipeline, one composer

`draw_frame` shares the *apply* half, but the `allocate`+`render` half *was*
duplicated: `Repl::compose_frame` ran it, and `ui.rs::confirm`'s frame builder
hand-assembled `[Divider?, Overlay]` and called `allocate`+`render` itself
(ui.rs:184). A third is arriving with §4.5 — the content path, re-rendering the area
per chunk. Two pieces, then: one **tail** that every frame goes through, and one
**composer** that decides what the area is made of.

**Status: the composer is folded into the tail.** Callers pass *pieces* and get
nothing back — no `Vec<Element>` crosses a call site, because no caller ever
used one except to hand it straight on. The tail is still one place:
`items → allocate → render → `plan_apply` → write`, so §9 #3's "one source for
both phases" holds exactly where it did.

The composer owns the stack: it stores the recipe it built, and the stack is a
*recipe*, not pixels. That is what lets §4.5's content path re-draw it per chunk
without recomposing — and why the status row belongs *in* the stack as an
element that resolves itself at render time rather than as a string baked in at
compose time (below). The rule itself still lives in `output.rs`; §4.5's end
state moves composition into `ui.rs`, which is also where things come out of a
~3900-line `repl.rs`.

```rust
// One call: pieces in, nothing out. Composes, stores the recipe, draws it.
fn draw_elements(&self, input: Option<Item>, overlay: Option<Item>,
                 policy: ApplyPolicy, dims: (usize, usize)) -> (usize, usize);

// The rule, a free function so it can be tested without an Output.
fn area_body(input: Option<Item>, overlay: Option<Item>) -> Vec<Item>;

// §4.5 U1/U2 — what the content path re-runs (private to ConsoleOutput: it is
// the same object that holds the stack). Renders the stored stack and applies
// it, so a status row inside is re-resolved on every call.
fn draw_stack(&self, policy: ApplyPolicy, dims: (usize, usize)) -> (usize, usize);
```

Three call sites, and what each is:

| caller | pieces | then |
|---|---|---|
| `read_input`, per keystroke | `draw_elements(Some(input), overlay, …)` — both **moved** in; the REPL rebuilds them next keystroke, so nothing is preserved by copying. Compose stores the stack. | tail |
| `with_content_writer`, per chunk | none — the stored stack, re-drawn | `plan_consume` → content → `draw_stack` → `plan_reposition` |
| `ui::confirm` | `(None, Some(dialog))` — the composer's own rule supplies the divider, and it gains the status row a dialog is due (4.5.5, OQ4; the **P3** input row is still to come) | tail |

The dialog keeps its own composition because §5.3 treats it as a separate surface. What it
gains is that it stops hand-rolling the pipeline; that is a duplicate *decision* (§4.5's
"one place composes") versus a second *site* running `allocate`+`render`, which is
duplicated code.

**The status row is `must = might = 1`, and it resolves at render time.** §3.1's purity
rule is "no terminal dependency — `w`/`h` passed in → unit-testable", and a status element
that *owns* its data source (`Arc<Mutex<StatusText>>`, `Arc<AtomicBool>`) satisfies it:
you construct one with your own state and render it anywhere. What was rejected was an
element reaching back into `Output` from `render`, not an element holding its own bytes'
source. Resolving at render time is what makes §4.5's re-draw of the stored stack show the
*current* dot and context figure instead of the ones present at the last compose — compose
runs once per keystroke, the turn's `set_status` runs after.

**The terminal size is one fact, owned by the drawing layer.** Items flow up; the budget
flows down from `dims`. The REPL keeps reading it for two things that are *not*
composition — windowing the menu (`paint_overlay`, done before anything crosses, which is
what keeps bulk data out) and resize detection (`painted_size`). For that second one,
`draw_elements` still takes `dims` as the REPL's *belief* and returns the size it actually
painted; today those are the same value passed through, so nothing changes, but a
mismatch becomes a stale cache rather than something to clamp away.

### 5.2 REPL (`read_input`)

`redraw` becomes one call: `out.draw_elements(Some(input), overlay, policy, dims)` — the
compose/allocate/render tail is no longer this module's (§5.1.1), and the element list
never comes back to the caller. The `input_row`
local and `prev_term_h` `Cell` are retired; `frame_anchor()` replaces `next_input_row`'s
scroll tracking (the cache holds the post-scroll top). The REPL always applies an
empty-input frame before returning (as today's submit paths do), so no frame is live
when content streams under the next turn.

The compose list gains the **status row**, which the REPL no longer builds: the composer
prepends it, because it reads `status_text` and `streaming`, which only this layer may
know (§5.1.1). So `redraw` supplies `input` and `overlay` and gets *nothing back* — the
composition, and the stack it produced, belong to the output — and the divider is the
composer's too, since "a menu brings a separator" is a rule rather than a caller's choice.
`dims` carries the width: the status row has to be fitted for the same width the frame
renders at, and width is composition, not budget.

> **The data is still the REPL's; the row is not.** `read_input` pushes a `StatusText`
> built from `ctx` via `set_status`, once per cycle, before the first paint. Without
> that push the frame bar would read whatever the last turn's `set_status` left — and
> the very first prompt, which runs before any turn has started, would read an unset
> status and draw no bar at all (an unset status is *off*, and `status_row` returns
> `None` for off rather than an empty full-width row). What moved out is the rendering:
> width, the streaming dot, and the off-rule are `status_row`'s.

> **Superseded by §4.5, now landed.** The empty-input frame and the
> `plan_clear_live_frame` that retired it are gone: the area stays live across
> the turn, and the fold — not the frame boundary — is what content writes stop
> at. `plan_consume` walks to the fold and the content takes those rows over;
> `plan_reposition` puts the area back underneath. `plan_clear_live_frame` is
> deleted.
>
> **Known gap:** `end_to_end_input_mode_layout` reports **two** blank rows
> between a turn's last content and the status row, where it asserts one. Not
> fixed. What follows is the diagnosis and the proposed change; it supersedes an
> earlier note in this file that blamed the `cursor_row()` re-anchor, which is
> innocent.
>
> **The cause is that `plan_reposition` places the area by its bottom edge,
> which §4 and §4.5.7 rule out.** The model is settled: the area sits
> immediately below the content and content pushes it down; nothing is reserved;
> the rows beneath it are ordinary empty screen. The implementation instead
> descends `h-1` rows, paints the area **bottom-up**, and derives a `top` from
> the tracked cursor against the screen's last row — i.e. it anchors the area to
> the bottom of the screen. `plan_full` does the same.
>
> That is the shape of the bug, and it explains why fixing it has resisted a
> formula. Measuring the byte stream of the real binary, three things hold:
>
> 1. **`plan_consume` and `plan_reposition` use different vertical primitives for
>    the same move.** `plan_consume` descends with `CUD` (`CSI n B`, relative);
>    `plan_reposition` descends with bare `LF`. They agree everywhere except the
>    screen's last row, where `CUD` does nothing and `LF` **scrolls**. So a
>    single number — the LF count — means "advance the cursor" in one regime and
>    "scroll the transcript" in the other. That is the whole sensitivity.
> 2. **Cutting the LFs does not work**, which is why the arithmetic kept
>    failing. The content write is *into the area's own rows* (consume moves the
>    cursor there first), so the area's LFs are the scroll that lifts the
>    transcript back out. Reducing them makes the area paint over its own
>    transcript: the 429-retry scenario's two `Error:` lines are emitted and
>    never reach the screen, and no assertion catches it, because that content is
>    simply absent.
> 3. **`note_lines` clamps the tracked row at `term_h`,** discarding exactly the
>    quantity the placement needs. Once the transcript reaches the last row the
>    *logical* row keeps growing while the tracked one sticks, so no value
>    derived from `caret` can distinguish one row of overflow from two. Attempts
>    to compensate in the LF count (un-clamping `caret`, then `max(h-1,
>    scrolled)`) satisfy one regime and break the other, because the same count
>    also has to seat the area *below* the transcript when it is mid-screen.
>
> **Proposed change** — remove the bottom-edge placement rather than re-tune it.
>
> - **The area paints forward from a fresh row; it does not descend.** After
>   the content write, emit `\r` + the newlines for the rows just written + the
>   policy gap, then paint row 0, row 1, … in order. No `top`, no `delta`, no
>   `CUU`, no bottom-up pass — which retires the `CUD`/`LF` asymmetry above,
>   because no vertical motion is left to be ambiguous about at the screen edge.
> - **Spacing is the UI's, forced explicitly.** `\r\n` is well-defined from any
>   cursor position — `CR` cancels the wrap-pending state left by a full-width
>   row — so the UI never has to ask where the content left the cursor, and the
>   content contract needs no trailing newline. (Forcing one there would make it
>   impossible to render content to the last row, which is the whole point of
>   this change.) The gap count becomes the deliberate value
>   `end_to_end_input_mode_layout` asserts, rather than a residue of scroll
>   arithmetic.
> - **Wipe only what the write will not overwrite.** Frame rows are padded to
>   `term_w` (`pad_row`), so a content write already overwrites them wholesale.
>   The `EL` loop in `plan_consume` is redundant except when the area *shrank*,
>   and then only for `old_h - new_h` rows at the top — the delta, not `h` rows
>   every write.
>
> This is also why the test could not arbitrate the arithmetic: scenario 0 is a
> short turn that fills the screen, which is the **steady state**, and §4 notes
> that the reserved and unreserved designs converge there and differ only *before*
> the screen fills. A regression test for this change has to keep the area
> mid-screen — a long, low-output turn, or an area that changed height — since
> that is the only regime where the two differ. Until then the gap is expected
> to close from this change, and is not to be re-investigated as arithmetic.

`next_input_row` / `clear_rows_to` folded into the apply implementation (P3); their
meaningful unit tests moved to `ui::frame::apply_tests`, the byte-mechanics ones
dropped with the helpers.

### 5.3 Dialogs (`ui.rs::confirm`)

Compose through the same call as the REPL — `draw_elements(None, Some(dialog), …)` — so
the divider is the composer's rule ("a menu brings a separator", §5.2) rather than
`ConfirmSpec.divider`, and the dialog gains the status row a dialog is due (§4.5.5, OQ4).
The input row is **P3**'s remaining piece. Allocate, render, apply. The entry `cursor_row()` DSR is kept:
it used to be kept because mid-turn the tracked caret did not follow streamed content,
so `frame_anchor()` alone would point into the stream — that reason is **false** now
(#10: the streaming writers count from the bytes they emit, checked against a vt100
replay). It is kept as a cheap confirmation on a path where a wrong anchor strands the
dialog over the output, and because the confirm suite asserts *behaviour* rather than
the dialog's row, so "green" is not evidence either way. §4.5 U5 revisits it.
After the DSR the dialog anchors via `frame_anchor()`, and if
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

#### Open before the §5.2 change (bottom-edge placement)

The §5.2 diagnosis is settled; the change itself is not started. Three things
have to land with it, and none of them is optional:

1. **A regression test in the regime that distinguishes the two designs.**
   Blocking. Every existing scenario fills the screen — the steady state, where
   §4's reserved and unreserved designs are observationally identical, which is
   precisely why `end_to_end_input_mode_layout` could not arbitrate the LF
   arithmetic. The test has to hold the area **mid-screen**: a long, low-output
   turn, or an area that changes height. In progress.
2. **Does `CR` clear the wrap-pending state?** The proposed placement emits
   `\r` + N newlines unconditionally, which is position-independent only if it
   is. `frame.rs` already documents wrap-pending after a full-width row and
   suppresses `EL` for it. Pin this with a vt100 replay rather than assuming it
   — the whole no-descend design rests on this one question.
3. **What draws the area when the content fills to the last row?** With
   bottom-edge placement gone the area follows the content and the scroll is the
   terminal's, unmodelled. The old `delta` was doing explicit work there. It is
   not yet established that nothing is needed.

Two more notes that are easy to lose:

- The four `NOT the model` banners added in `ui::frame.rs` (§5.2) come out in
  the same commit that makes them true.
- **A green suite is not a sufficient signal for this change.** Cutting the LFs
  turned `end_to_end_input_mode_layout` green while silently dropping both
  `Error:` lines from the 429-retry scenario — the content was emitted and never
  reached the screen, and nothing asserted against it. Until there is an
  assertion covering that content, check it by eye.
---

#### Audit of U1–U6 against the tree

Every claim below was checked against the code, not against the phase list.

| | unit | state |
|---|---|---|
| ✅ | **U1** contraction | Landed. `with_content_writer` consumes the area instead of retiring it, and `plan_reposition` is the third apply case. **But** it places the area by its bottom edge, which §4.5.7 rules out — see §5.2's known gap. Implemented, and wrong in the one respect §4.5.7 settled. |
| ✅ | **U2** area survives the turn | Landed. The bar is a composed row resolved from `status_text` + `streaming`; the transient-bar machinery is gone. |
| ✅ | **U3** dialogs are menus | Landed. `ui::confirm` composes `[Divider?, Overlay]` through the same composer. |
| 🔶 | **U4** in-turn input | **U4.1–U4.3 landed; U4.4 open.** The plan is recorded under U4 itself. Landed: `PromptSession` owns `editor`, `nav`, `draft`, `sel`, `last_disc`, `prose`, `pending_key`, the `SlashDotMenu` and the overlay, and `read_input` takes it by `&mut`, so the state outlives the call; `handle_key` is the draw-free dispatcher (`Continue` / `RunEditor` / `Action`), generalising the `edit_key` split; `slashdot_menu` gained `continuation_handle` so a `/`-session keeps its epoch across evals. The in-flight reader no longer drops edit keys — `route_event` routes each one `Inline` (Ctrl-C, Ctrl-Z, Resize) or `Forward` (every other key, **`Esc` among them**, plus paste) onto an unbounded mpsc, unbounded precisely so a full queue can never block the thread at the moment it must deliver Ctrl-C. **The receiver is drained:** `prompt()`'s `select!` grew a key branch that runs `handle_key` → `settle` — eval, outcome, sync, paint, the sequence `read_input` runs minus its wait — plus a prose timer for `prose.poll_timeout()`, without which prose would go inert mid-turn. Submit is blocked and silent, as settled. `Esc` is menu-first: the ladder runs while a v2.5 menu is open and the turn stops only at root, which is *why* it moved to `Forward` — the reader has no session to look at, so routing it inline was routing it to "abort" unconditionally. `eval` is out of `read_input` and into `settle`, so the session is self-driving. **Still open: U4.4**, the ordering between the key branch's compose and the content branch's write + `reposition_area`. |
| ❌ | **U5** retire the entry DSRs | **Not started.** `read_input`'s entry DSR is live at `repl.rs:683` and `ui::confirm`'s at `ui.rs:92`, both marked for removal. The three sanctioned sites are correctly still there (Ctrl-Z resume `repl.rs:806`, the width-change `ESC[0J` + replay `repl.rs:841`, and the confirm error path `repl.rs:942`). |
| ⏸ | **U6** scroll region | Optional, gates nothing, not started. |

**Two things this audit changes about the plan.**

*U4 is the gate for everything the status-row and input proposals want.* R1 (an empty input is no row mid-turn) is written, tested and **was dormant**: it asks `is_streaming()` at compose time, and until U4.3 the only compose that happened during a turn was none — `read_input` returned on submit and the agent loop never composed an area. Its unit test passed without production ever reaching it, a test that could not have caught the real defect, which is why R1 was reported as done when it was not. **U4.3's key branch is the path that opens:** pressing a key mid-turn runs `settle` → `paint_prompt`, so a compose now happens with the stream live and R1 is reachable in production rather than only in its unit test.

*U5 cannot start before U4.* It retires `read_input`'s entry DSR on the grounds that the fold now survives the turn — but the fold surviving the turn is exactly what U4's in-turn composer has to keep true once the area starts changing shape mid-turn. Doing U5 first optimises away the safety net that U4 would be leaning on.

#### Reported rendering issues

Five issues found by hand, distinct from §5.2's placement — which they do not
affect, and which they should not be folded into. Triaged in this order.

**R1 — the input row is painted even when there is nothing to show.** Mid-turn
the area renders `[status, input]`, so an empty input still occupies a row and
the cursor rests visibly on it. It should occupy **zero** rows when empty and
appear on the first keystroke. `compose_elements` (`repl.rs:1269`) already takes
`Option<&str>` for the status; the input needs the same treatment.

OQ2 is not contradicted by this. It settled that *the REPL stays live mid-turn* —
it is a statement about activity, not about rows. The frame is composed and
applied either way; it just has no input row to draw until there is input. This
is also the "frame content changes in size" case at its most routine, so it is
worth settling before the placement work rather than after.

**R2 — consecutive committed prompts get a blank they should not have.**

```
────────              ────────              ────────
/session … resume     /reasoning med
────────              ────────              ────────
```

The blank is not `emit_commit`'s: `display_separator` and `display_plain` never
call `boundary()` (`output.rs:2856`, `output.rs:2532`). It comes from the
commit's *own trailing message* — `repl.rs:858` calls `display_text` right after
the commit, and `display_markdown` opens every block with `boundary()`
(`output.rs:2545`), which fires because the stream does not already end blank.
So a content rule — two paragraphs want a blank between them — is being applied
to two adjacent UI blocks, which want to abut. Same rule, wrong side of the
content/UI line.

**R3 — the same spacing between the status row and a separator above it.** Two
blanks where one is wanted. Same cause as R2, different pair of blocks.

**R4 — a regular prompt commit has no separator above it.** The committed line
sits with a rule *below* it — the input's own underline — and nothing above, so
it reads as content rather than as something the user committed. A slash commit
is bracketed on both sides; a prompt is bracketed on one.

The row to use already exists: it is the row that holds the status line, directly
above the input (`compose_elements` pushes it at `repl.rs:1282`). On a regular
commit that row becomes a divider instead, and the prompt commits beneath it.

**R5 — bulk output re-emits the area after every line, and `/replay` re-emits
its own menu between the lines of history.** Reported by hand: replaying a
session is slow, and `/replay` menus keep popping up in the middle of the
output they were invoked to produce.

Both come from one shape. `with_content_writer` (`output.rs:1652`) does, **per
write**:

```
lock writer → plan_consume (walk to the fold, take the frame) → write the
             content → reposition_area → write the frame back below → flush
```

and `reposition_area` (`output.rs:1734`) does not replay the frame it just
took — it re-*renders* `cache.element_stack`, the recipe the last compose
recorded. So a burst of `N` lines pays `N` recipe re-renders and `N` frame
re-emits, and the frame it re-emits is whatever that compose happened to
contain. During `/replay` that includes the menu, so the menu is drawn again
between each line of history, fighting the very output it should sit above.

**Ruled out:** past prompts are *not* re-entering the slash machinery.
`render_turn` (`agent.rs:3173`) hands the historical line to `display_plain`,
which is `write_content("{text}\n")` — no parsing. The only `starts_with('/')`
tests in the crate are on the live buffer (`repl.rs:943`), a session path
(`session.rs:279`), and live submit dispatch (`repl.rs:1860`).

**Open:** *which* recipe is in the stack. `read_input`'s submit paint passes
`&[]` for the overlay (`repl.rs:1681`), which should leave the recipe
menu-free by the time the command runs — so either something composed again
after it (a mid-turn invocation carries the menu through `settle`), or the
stack outlived the paint that should have replaced it. Distinguishing those
decides the fix: stale ⇒ drop or re-record the recipe when the menu closes;
merely expensive ⇒ the batch fast-path below removes the cost without touching
the content.

**The fast-path this wants.** Large batch output should defer the reposition:
consume the frame, let it stay consumed for the whole burst, and do **one**
`reposition_area` when the burst ends — the explicit "now redraw the frame"
rather than one per write. The invariant `with_content_writer`'s comment
guards still holds, because the final reposition is what restores `live` (a
missing `live` means the *next* write finds nothing to consume and the area
never returns); it only moves from every write to the end of the batch. Note
this is not only a replay problem — `stream_text` (`output.rs:2668`) enters
`with_content_writer` once per delta, so a live stream pays the same
consume/reposition cycle per token, where the frame is merely small.

##### The structural fix behind R2 and R3

The separation rule is in the wrong layer, and `StreamTail` is the proof:

- `StreamTail` (`row_count.rs:162`) is a two-bit atomic state machine folded per
  byte whose only production consumer is `needs_boundary()`
  (`output.rs:1770`) — *emit a blank line before the next block*. Its second
  accessor, `at_line_start()`, has **no production caller at all**; the only
  reference is an assertion in `output.rs:4564`. So the whole mechanism is
  presentation policy living in the row-counting module, and there is nothing
  content-ish about it.
- The rule leaks into eight call sites in `output.rs` (1798, 2223, 2545, 2558,
  2596, 2636, 2708, 2740). Every content type has to remember to call
  `boundary()` — the shape of a decision that belongs in one place and lives in
  nine.
- And it is simply wrong for UI blocks, which is R2 and R3.

The fix is to move the decision, not retune it. Content emitters emit *typed
blocks*; one owner in the composition layer decides separation, with the
distinction R2 actually wants: **content blocks separate, adjacent UI blocks
abut.** The fold is a scrolled rectangle in `allocate`/`render`, so this needs a
transcript-level composer rather than a change to the fold. Once the composer
sees every block and knows what it wrote, `StreamTail` and `ends_blank` can go —
a byte-level guess at a structural fact stops being necessary.

Worth doing while in there, though minor: `batch_commit` (`repl.rs:1482`) renders
a commit as a bare `display_plain` line with no rules and no boundary, while
`emit_commit` brackets it — so a startup replay looks different from the same
action taken live. Only implicit submits reach it, which is why it went unnoticed.
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
| Dialog divider in mid-turn context | Superseded — the composer adds a divider whenever an overlay is present (§5.1.1), so a mid-turn dialog gets one unconditionally. `ConfirmSpec.divider` is **deleted** |

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

**Audit 2026-09-30** (after the permanent status bar and the §4.5 rewrite): the
live work is §4.5 U1 → U2 → U3, which is a *drawing* track and is independent of
in-turn input. The stale item most likely to mislead is "streaming does not
track rows" — it appears in three places in this file and is false in all of
them; §4.2, §5.3 and §9 now each say so where they state it.

**Audit 2026-09-28** (after the editor cleanups, with the v2.5 engine now
driving every `/` command in the REPL): #1–#3 re-verified done, pins still
green; #5, #7, #9 and #11 audited and then **fixed** the same day (see below).
#4 is still open; #6 and #10 were on this list and are **done** (see their entries
below and in §4.2). Each carries a re-check with file/line
evidence, what a fix involves, and a verdict. #8 stays an accepted limitation.
A recommended order for what remains is at the end of the section.

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

3. **Unify the size source.** *(resolved)* `Output::draw_frame` now **takes**
   the composed `dims` and applies exactly them, returning them so callers
   record the engine's fact: the `redraw` closure threads the loop's single
   per-iteration read through compose → apply and records the report into
   `painted_size` (the loop's own `painted_size.set((cur_w, cur_h))` is
   gone); `draw_dialog`/`resolve` pass the dialog's tracked `size`; and
   `Output::frame_size()` is the shared source for callers without a read
   (dialog init). Original report: `redraw`/`draw_dialog` composed with
   `crossterm::terminal::size()` while `ConsoleOutput::draw_frame` applied
   with `term_width()`/`term_height()` (width via the `TERMINAL_WIDTH`
   static) — a resize landing between the reads failed the partial
   predicate's `term_w` check (one harmless extra full repaint), and could
   split a frame's content from its bounding box. Passing the composed size
   into the apply removes the class.

4. **`confirm_subject` parses a message convention.** The decision line's
   subject comes from stripping `Confirm … call?` off the tool-confirm
   header. If the `agent.rs` message wording ever changes, the line silently
   degrades to the full header (correct, just noisy). An explicit subject
   parameter threaded through `Output::confirm` would drop the coupling.
   **Still open** (audited 2026-09-28). There is exactly *one* production
   caller — `agent.rs:2075` builds `Confirm tool {NAME} call?` — and
   `ConsoleOutput::confirm` (`output.rs:2046`) strips the convention back off
   it. So the coupling is a two-site round trip, not a spread convention.
   Fix, cheapest first: add
   `fn confirm_with_subject(&self, message: &str, subject: Option<&str>) -> Confirmation`
   defaulting to `self.confirm(message)`, override it in `ConsoleOutput` to
   build the spec with the given subject, and have `agent.rs:2075` pass
   `Some(&format!("tool {}", Self::NAME.to_uppercase()))`;
   `confirm_subject` survives as the documented fallback. The thorough form
   changes `Output::confirm` itself: 4 impls + 1 delegating wrapper + 3 test
   impls + `src/bin/confirm_probe.rs`, all mechanical.
   **Verdict: bundle it.** The failure mode is cosmetic (a noisy line, never a
   wrong decision), so it does not earn a change on its own.

5. **`draw_dialog` sniffs the `> ` marker** to find the overlay's windowing
   focus. Fine for today's specs (no detail line starts with `> `), but a
   spec whose detail ever does would window around the wrong row. Robust
   form: `focus = plan.detail visual height + selected row's height offset`,
   the same arithmetic the REPL's pre-computed `focus` uses.
   **Fixed** (2026-09-28), together with #11. The painter now returns a
   `PaintedOverlay { rows, focus, data_heights, data_budget }` — the rows to
   show plus the geometry it produced — and `focus` is the *painted* index of
   the selection, recorded while painting. Ground truth by construction: it
   survives a truncated detail section, a table header, a multi-row selection,
   and the `*` active marker that hides the `>` cursor marker (the old sniff
   found nothing there and fell back to row 0). The REPL's own guess —
   `1 + Σ(1 + newlines(opts[..sel]))`, which assumed a one-row header and no
   table header — is deleted with the sniff, so neither host derives geometry
   anymore. `paint_plan` survives as the text wrapper (`rows.join("\n")`) for
   callers that want a string. Pinned by
   `focus_is_the_painted_row_of_the_selection`,
   `focus_follows_a_truncated_detail_section`, and
   `focus_is_reported_even_when_the_active_marker_hides_the_cursor`.
   Side effect worth keeping: the frame's overlay rows are now handed over as
   one `String` per terminal row — the shape `Element::Overlay` asserts — so
   no host re-splits a painted string into a row count that can disagree with
   the screen.

6. **`display_text` line counts can drift.** *Substantially fixed; the
   compensating DSR remains.* The count used to come from
   `wrap(text)` while the bytes include the `⏺ ` prefix and `termimad::inline`
   output — three drift sources: the 2-column `⏺ ` prefix never counted, `wrap`
   counting *chars* where the terminal wraps on *display columns* (CJK/emoji
   undercount), and `inline` rewriting the text before it reaches the terminal.
   `display_text` now measures the bytes it emits, through the same
   `RowCounter` every other content write uses, and
   `display_text_row_is_measured_not_predicted` pins it against a vt100 replay
   with a message that exactly fills the width and one a column wider.
   The DSR after the DoneError `display_text` (`repl.rs` → `display_text` →
   `cursor_row()`) is still there, but no longer as a correction — it is
   insurance on an error path, and its comment now says so. §4.5 U5 revisits
   whether it is still needed once the fold survives the turn.

7. **`paint_plan` under-buffers multi-line plans on large terminals**
   (found while porting the P5 tests; lives in `slashdot_menu`, not the draw
   pipeline). `compute_space_budget`'s "rows fit" branch returns `total_rows`
   — a *logical* count — as the *visual* budget, so every multi-line row is
   budgeted as one line and `visual_row_window` windows it even when the
   terminal has room to spare. Probe (9 rows × 3 visual lines, width 60):
   painted lines = **15 at term_h 24, but 9 at term_h 40 and 80** — a bigger
   terminal shows *less*. The fit branch should return the rows' visual
   height sum. Single-line rows (the normal menu case) are unaffected.
   **Fixed** (2026-09-28). The budget is now computed from *painted* row
   counts, supplied by the painter as a `SectionHeights { detail,
   table_header, data }`:
   `compute_space_budget(heights, overlay_budget, min_row_lines)`. The fit
   branch returns the rows' height sum, the overflow branch compares and
   budgets in the same unit, and `min_rows` became `min_row_lines` — a *row*
   floor, the one semantic change beyond the unit fix (single-line plans, the
   normal case, are unaffected). A shared `visual_height(line)` defines a row's
   height as the engine wrote it, for the wrappers that measure a raw plan;
   `window_heights` / `page_delta_heights` are the real forms the painter uses
   (they take heights), with `visual_row_window` / `page_delta` kept as
   `LogicalLine`-shaped wrappers for callers reasoning about an unpainted plan.
   Pinned by `budget_wrapped_rows_are_measured_visually` and, at the painter
   level, `paint_overlay_shows_more_as_the_terminal_grows` — painted lines must
   never *decrease* as the terminal grows (this plan: 12 at `term_h` 20, 15 at
   24, 18 at 30, 27 at 40 and 80; before the fix, 9 at 30+). The 7 pre-existing
   budget tests are single-row plans and pass unchanged, which is the point of
   having kept them; they now pass painted counts directly. One duplicate was
   removed in the rewrite:
   `budget_min_rows_enforced_even_when_detail_wants_more` had the same
   arguments *and* the same expectations as `budget_detail_overflows_rows_dont`
   (only the comments differed); its intent is folded into that test.
   *Follow-on (same day):* the page-scroll path stopped recomputing the budget
   and asks the painter for `data_budget` / `data_heights` instead, so a
   PageUp/PageDown moves exactly one screen of painted rows.

8. **Tiny terminals lose the pop-up, not the input.** Allocation serves
   floors in element order (input → divider → overlay), so on a ~2-row
   terminal the menu gets zero rows while the input keeps its row. Intended
   (budget ≥ 1, input floor first); just worth knowing when testing in tiny
   windows. Re-checked 2026-09-28: still the designed behavior, still the
   right trade. **No action** — recorded so a future reader doesn't mistake it
   for a bug when testing in a 2-row window.

### Added by the audit

9. **The feature-gated suite is still not in CI** (promoted from the process
   take-aways below, audited 2026-09-28). `.github/workflows/ci.yml` ran
   `cargo test --verbose` only, so `tests/confirm_raw_mode.rs` (8 tests,
   including the `confirm_releases_reader_gate_on_resolve` pin for latent issue
   #1) never ran on a push or PR.
   **Fixed** (2026-09-28): the test step is now
   `cargo test --verbose --features test-utils`. The feature is empty — it only
   carries the `[[bin]]`/`[[test]]` `required-features` — so it gates nothing
   else, and keeping both halves in one step is what stops them drifting apart
   again.

10. ~~**Streaming still doesn't track rows**~~ **DONE — the premise is stale,
    and it has been for some time.** `stream_text` / `stream_thinking` no longer
    hand completed blocks to `Renderer::feed` uncounted: every content write goes
    through `with_content_writer`, which wraps the writer in a `RowCounter` and
    calls `note_lines` with what the *bytes* moved the cursor by (row_accounting
    §6 phase B). `streamed_answer_is_counted` and `streamed_thinking_is_counted`
    compare the tracked row against a vt100 replay of the same bytes, so this is
    checked against a terminal emulator and not merely against itself.
    **The two entry DSRs are still here, for reasons that are not this one.**
    `read_input`'s establishes the *starting* row — the cache begins at row 1
    while the shell that launched picocode may have left its prompt further down,
    and every byte of the greeter would be counted from the wrong place. The one
    in `ui::confirm` is a cheap confirmation on a path where a wrong anchor
    strands the dialog, and whose tests assert behaviour rather than position.
    The other two re-anchors (Ctrl-Z resume, and the width-change `ESC[0J` +
    replay) are about *foreign* writers and were never about row accounting.
    All three comments that still blamed streaming drift have been corrected.

    **What this did not buy, and it is worth being explicit:** the status bar's
    hand-off at submit is *not* drift. The prompt-side bar is a row of the input
    frame and the mid-turn bar is a content line, so the bar legitimately moves
    from one to the other when the turn starts — §4.5's fixed-bottom composition
    is what would make it continuous, and that is still open.

11. **The painter does not wrap detail / table-header lines** (found while
    fixing #7 — same crate, same accounting). `render_logical_line` padded to
    `term_w` but never wrapped, so a header longer than the terminal width was
    emitted as one over-long line that the *terminal* wrapped — while
    `paint_plan` counted it once. Probe: a 100-column detail line at `term_w` 60
    painted as 1 line of 102 columns. Consequence: the frame's row accounting
    (and the REPL's focus math) understate the overlay by a row, so a stale
    wrap remainder could survive a partial apply.
    **Fixed** (2026-09-28), with #5. The painter now owns the fitting for
    every section: `paint_row` splits its content at the width via a new
    `wrap::split_to_width` — the content-preserving counterpart to `wrap`,
    which *reflows* and would collapse the column gaps of a table row and the
    alignment of an engine help header (`"  10      last 10 turns"`). Every
    painted row is therefore exactly `term_w` wide and holds no newline, and
    `rows.len()` is the number of terminal rows the overlay occupies. The
    budget, the row window and the page size are all computed from those
    painted counts, so nothing is measured in one unit and painted in another.
    `render_logical_line` is gone — `paint_row(content, RowStyle, tw)` replaces
    it, and `RowStyle` absorbs the three (marker, background) conventions the
    old flag arguments and the data-row painter each spelled out separately.
    Pinned by `over_long_detail_line_is_split_into_terminal_rows`,
    `every_painted_row_is_one_full_width_terminal_row` and
    `data_heights_are_the_painted_heights`, plus six unit tests on
    `split_to_width` (space preservation, blank lines, display width, the
    content-preserving cut).
    **Consequence found while fixing it — a contract, now documented:** the
    painter's marker column means row text must be laid out for
    `term_w - 2`, and the engine's tables already are
    (`render_multiline(tw - 2)`). Two P5 tests in `settings.rs` were pinning
    the *old* relationship (laid out at the full width, painted at the full
    width) — i.e. they pinned the over-wide row #11 is about — and were
    re-pinned to the real contract (lay out at `tw - 2`, paint at `tw`).

### Deferred design pieces (allowed for in code)

All four re-checked 2026-09-28: still deferred, still test-only users, allows
still targeted (no blanket `allow(dead_code)` came back).

- **`Element::Lines`** — no longer deferred: the prompt-side status bar is its
  first production user (`compose_frame` puts it in the divider's row, above the
  input, so the frame's own union clear takes it at submit). The `#[allow(dead_code)]`
  on the variant can go once the mid-turn bar moves here too — §4.5 U2.
- **`Frame::spans`** — §4.4's dirty-span refinement; the row diff is
  deliberately the sole reader until profiling asks for more.
- **`ApplyPolicy::Append`** — built and vt100-tested in P2, no production user
  yet. It is the *commit-line* append today (`plan_append` writes the rows at the
  anchor and retires the frame), which is not what §4.5 needs: P1 wants the area
  re-emitted at a new fold after a content chunk. Whether that is the same
  function with a different clear policy, or a second one, is an open detail of
  P1.
- **Per-row backgrounds (`row_bgs`)** — dropped with `overlay_lines` in P5;
  if per-row tinting is ever wanted it belongs in `RenderPlan` (engine
  side), not in the painter.

### Process take-aways

- **A feature-gated suite is not a gate.** `confirm_raw_mode` requires
  `--features test-utils`, so plain `cargo test` skipped it through P0–P3 —
  which is how the missing `('n', 1)` quick key survived until P4's gate run
  (it failed at baseline too). Run `cargo test --features test-utils` in CI,
  or drop the gate. *(Now #9, and done: the CI test step carries
  `--features test-utils`.)*
- **The P0 bet paid off.** Keeping the vt100 suite on production bytes meant
  the P3 pipeline swap was validated by the same carefully-tuned assertions.
  Exactly one intentional behavior change needed a re-pin — the frame-wide
  2/3 budget (pop-up gets `budget − input − divider` rows, scrolls less) —
  and manual testing has since confirmed it.
- **Streaming still doesn't track rows** (`stream_text`/`stream_thinking`
  bypass `note_lines`), so the cache's `caret` drifts mid-turn. That is why
  dialogs and `read_input` keep an entry DSR. *(Historical: this was #10, and it
  is **done** — the streaming writers count from the bytes they emit, checked
  against a vt100 replay by `streamed_answer_is_counted` /
  `streamed_thinking_is_counted`. The DSRs stayed, for the reasons §4.2 now
  records. §4.5 is the remaining work, and it was never gated on this.)*

### Recommended order (audit 2026-09-28)

#5, #7, #9 and #11 are done. What remains, in order:

1. **§4.5 U1 → U2** — contraction, then the area surviving the turn. This is the
   live work: it is what in-turn input and in-turn menus are waiting on, and it
   retires the transient-bar mechanism rather than adding to it.
  #6 (`display_text` accounting) and #10 (streaming accounting) were both long
  assumed to be this section's gate. Both are done, and the four open questions
  are now answered (4.5.5); what remains is restating the clear-policy invariant
  for a live area (§2's P4 row).
2. **#4 — explicit confirm subject.** Bundle with any of the above; on its
   own it only buys a quieter line.

One theme across the fixed ones: every fix moved a piece of knowledge to
where the truth is, instead of re-deriving it downstream. The painter learned
the geometry (#5, #7, #11) so neither host has to guess it; the next natural
step in that direction is #6 (let `display_text` measure the bytes it wrote
instead of predicting them) and #10 (let the streaming writers account for
their rows).
