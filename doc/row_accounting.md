# Row accounting, the live footer, and what they unblock

**Status:** design discussion — nothing implemented yet
**Context:** follows `doc/ui_draw_v2.md` §9 #6 (content row counts drift) and #10
(streaming doesn't track rows). Those two are not two bugs: they are one missing
capability, and it is the gate on a live footer.
**Supersedes:** nothing. `ui_draw_v2` §4.5 ("contraction and fixed bottom
elements (future)") is the sketch this fleshes out.

---

## 1. Problem

`DrawCache` (ui_draw_v2 §4.1) knows two things: the cursor row and the last
applied frame. Everything drawn *underneath* them — streamed reasoning, streamed
answer text, rendered markdown, tool panels — moves the real cursor, and the
cache is supposed to follow. It follows for some writers and not others:

| Writer | Accounting | Verdict |
|---|---|---|
| `display_error` / `display_system` / `display_separator` | `note_lines(1)` | exact (single row) |
| `display_plain` | `wrap(strip_ansi(text), w).len()` | exact (it wraps first) |
| `paint_panel` / `tool_panel` (tool panels) | `note_lines(1)` per *pre-wrapped* line | exact for ASCII, undercounts wide chars |
| `display_markdown` | `rendered.lines().count()` | exact if nothing wraps, undercounts otherwise |
| `display_text` | `wrap(text, w).len()` — the bytes are `⏺ ` + `termimad::inline(text)` | **drifts** (§9 #6) |
| `stream_text` / `stream_thinking` / `commit_thinking` | *nothing* — 7 `Renderer::feed` sites | **untracked** (§9 #10) |

And the fallback that papers over the drift has a second defect: `note_lines`
**clamps** the tracked row to the terminal height (it models "the terminal
scrolled and kept the cursor on the bottom row"). So every writer that skips a
count does not just under-count — after enough unaccounted output the tracked
row is pinned at the bottom, and the *next* counted write puts the anchor in
the wrong place.

### 1.1 The artifact this explains

> *incomplete clear on session resume; a very short session left content above
> the replay visible, with a table header row painted on top of it.*

`/session … resume` ends with `replay_last_turn_silent` (`repl.rs:658-670`,
reused from the width-resize path): `\x1b[0J` from the cursor, replay the last
turn, then a DSR re-anchor. The reasoning, as reported:

1. The menu was mid-draw: `ui::confirm` does a `cursor_row()` (DSR) re-anchor on
   entry and then composes `[Divider?, Overlay]` **at the tracked cursor row**.
2. The turn had streamed, so the tracked row was the clamped bottom row — the
   real cursor was elsewhere. A stale *frame region* is tracked too
   (`live`), and a stale `caret` points the clear at the wrong place.
3. The menu was torn down with `Frame::empty()`, which clears the union of the
   old region and the rows below it. If the region was wrong, the clear is
   wrong — and the replay writes *over* whatever it did not clear.

So yes: content starting before the menu was fully closed, with the draw path
blind to where the cursor actually was. Row accounting removes the cause
(every writer, including the streaming ones, maintains the cache — ui_draw_v2
§4.2's contract, which the streaming writers do not honour), and a cheaper
guard removes the failure mode (a **scoped** re-anchor, below).

I have not reproduced it pixel-for-pixel yet; phase B pins it before it fixes
it, so the fix is verified against the report and not against my theory of it.

---

## 2. Why a footer needs *both* halves

A footer is a fixed row at the bottom of the screen that content must not
disturb. Two things have to be true.

**(a) The cursor must be tracked** — otherwise the frame's anchor after a turn
is wherever the clamped guess ended up, and the clear region is anchored to the
wrong row. This is §1.

**(b) The frame must know the footer exists** — and this is the part that is
easy to get wrong. Today `read_input` returns and the REPL applies an
*empty-input* frame before returning (ui_draw_v2 §5.2), which makes the input
row the last thing on screen. If content then streams, the terminal scrolls and
the input row — and any footer under it — is consumed. No amount of tracking
fixes that; the frame simply has nowhere reserved to put itself.

The invariant, then:

> The last `F` rows are reserved for the footer. The live region is
> `1 ..= term_h − F`; a frame apply may not write or clear below `term_h − F`,
> and the budget is computed against `term_h − F`, not `term_h`.

The consequence has to be stated plainly, because it decides the phase order:

> **Content can never scroll past the footer's top edge.** The live region is
> bounded; once the cursor reaches `term_h − F`, further output scrolls the live
> region while the footer row stays put. So the footer row is not "dipped into"
> and recovered — a turn long enough to reach the bottom *fills the live region
> and stays there*, and the footer's row is appended to.

That is what "the live region" already means in this codebase (ui_draw_v2 §4.3:
the top 1/3 is guaranteed scrollback; the bottom 2/3 is the working area), and
it is why the same mechanism serves the 2/3 budget and the footer: the frame
already scrolls itself into the live region via `delta`, it just needs the live
region to end at the right row.

---

## 3. Design

### 3.1 The `Output` surface is split (the foundation)

`Output` today is one trait carrying two unrelated kinds of byte: **content**
(assistant text, tool panels, `⏺ …` messages — written once, in order, and
never rewritten) and **UI** (the input frame, overlays, dialogs — a rectangle
that is repainted every keystroke). The frame pipeline is built entirely around
the second kind and knows nothing about the first, which is precisely why
content writers have to maintain the cache by hand.

```rust
pub trait ContentOut {
    // ... every display_*/stream_*/begin_response method, unchanged ...
}

pub trait UiOut {
    /// Draw a composed UI frame (ui_draw_v2 §5.1) — unchanged signature.
    fn draw_frame(&self, frame: &Frame, anchor: usize, policy: ApplyPolicy, dims: (usize, usize)) -> (usize, usize);
    fn frame_anchor(&self) -> usize;
    /// The live region's bottom edge (1-based, absolute). Today `term_h`;
    /// `term_h - footer_rows` once a footer is reserved. Content writers and
    /// the frame allocator both need this, so it is one method, not two
    /// copies of the arithmetic.
    fn live_bottom(&self) -> usize;
}

pub trait Output: ContentOut + UiOut {}
```

`draw_frame` moves to `UiOut`; `ConsoleOutput` is split into `content: ContentOut`
and `ui: UiOut` state so the one writer lock and the one `DrawCache` are shared
without a self-referential struct. Existing `&dyn Output` call sites keep
compiling — `dyn Output` is still a thing — so the split is mechanical and
reviewable in isolation.

The win is not tidiness. It is that with a separate object per stream, the
**anchor** each stream writes under is explicit rather than "wherever the shared
cursor happens to be":

- `UiOut` has its own `anchor` field: the row it owns. It moves only on
  `reanchor` (DSR) and through the scroll compensation below.
- `ContentOut` has its own `caret`: the row it last wrote, maintained by the
  counting writer.
- A `UiOut` apply that would *cross* the content's cursor (the content ended
  mid-screen, so the input frame would have to overwrite it) does not guess: it
  **scopes an anchor down** (below) and hands the reclaimed rows to the content.

The last point is what needs a decision; see §6.1.

### 3.2 Counting rows where the bytes are written

One helper, used by every writer that currently guesses:

```rust
/// A `Write` adapter that accumulates the terminal rows the bytes it
/// receives will occupy at `width`: `1 + (display_width(segment) - 1) / width`
/// per `\n`-separated segment, ANSI-transparent and display-width aware.
struct RowCounter { width: usize, rows: usize }
```

A writer renders into a local buffer, measures the buffer, and calls
`note_lines(rows)` with the truth. No second pass over the content, and the
upstream `markdown_terminal::Renderer` needs no change: the count comes from the
bytes, which is the only place the answer exists. Because the count is per
*block* and not per delta, the streaming hot path barely notices.

What that buys, concretely:

- §9 #6 disappears: `display_text` measures `⏺ ` + `termimad::inline(text)`.
- §9 #10 disappears: every `feed` site counts its own output.
- The compensating DSR at `repl.rs:770` (the `DoneError` path) can go.
- The entry DSRs in `read_input` and `ui::confirm` become removable once
  replay uses the scoped re-anchor (§3.3) — they are kept until then.

### 3.3 `ApplyPolicy::Reserve` — the footer's anchor

The footer is an `Element::Lines` (ui_draw_v2 §3.2, currently the crate's only
`#[allow(dead_code)]` variant — the §4.5 sketch was right about the first
production user) at the bottom of the frame, and the input/menu frame is
composed *above* it. Under the existing `Frame` policy that already works: the
frame is one rectangle, the footer is its last row, and the union clear covers
exactly what the frame painted.

What is missing is the *scroll* path. When a frame anchored at row `R` with
height `H` overflows the bottom, the apply scrolls by `delta` and repaints at
`R - delta` (ui_draw_v2 §4.3). With a footer the content that must not be
touched is not the terminal edge but the reserved band, so:

```rust
ApplyPolicy::Reserve { floor: usize }   // absolute row: the footer's top - 1
```

- The clear floor becomes `min(floor, screen_bottom)` instead of
  `min(term_h, screen_bottom)`.
- The budget is `⌈2/3 · (floor - anchor + 1)⌉`-ish — more precisely,
  `allocate(&extents, live_budget(floor - anchor + 1))` — so the frame fits
  *above* the footer, and if it cannot, the scroll compensates and the terminal
  pushes the reclaimed rows into the content above.
- `DrawCache` grows a `live_bottom: usize` (the footer's top − 1) alongside
  `live`, set by the same apply. Content writers clamp to it, so content stops
  exactly at the band.

`Reserve` is the policy §4.5 reserved `Append` for, minus the chunking: one
frame that owns the bottom rows. `Append` itself stays deferred (it has no
producer).

### 3.4 A first, static footer

Phase A2, deliberately the smallest thing that proves the tracking:

```
[my-session | anthropic:claude-sonnet-4 | high | ctx 34% · 78k]
```

- Composed as `Element::Lines` at the bottom of the frame; `Lines` is all-or-
  nothing in `allocate` (ui_draw_v2 §3.2), so a tight terminal gives up the
  footer before it gives up input rows. Correct default.
- Live data (session name, provider:model, thinking level, ctx %) has to be
  published by the agent as it runs: a `RenderContext`-like `StatusHandle` with
  atomics, read once per frame. That is a small new surface on the app state,
  and worth its own line in the design when we get there.
- Typed input mid-turn stays out of scope for the prototype — it is the same
  tracking plus a concurrent editor, which is a much bigger conversation.

### 3.5 Two sub-items this surfaces

**(a) The marker column should be the painter's, not the producer's.** Today's
contract is that row text is laid out for `term_w - 2` (the cursor column),
which every row producer has to remember; a new client that lays out at
`term_w` gets its last two columns split onto a second row. The fix is to hand
producers the *content* width and let the painter own the column:

```rust
pub struct RenderContext { pub content_width: usize }   // was term_width
```

- `paint_row` gains a `cursor: RowCursor` (columns today, background-only
  tomorrow) and pads the row to the full width itself.
- `Row`/`StaticRow` consumers stop subtracting 2; the engine's
  `render_multiline(tw - 2)` calls become `render_multiline(ctx.content_width)`.
- Nothing else moves: the painter's output is byte-identical for
  `cursor: Columns`, so this can land as its own commit and be verified by the
  existing paint tests.
- Cost: every `RenderContext { term_width }` construction (about a dozen, mostly
  tests), and one honest name change. The one decision: whether the *painter*
  also owns the padding (recommended) or producers still pad to
  `content_width` and the painter only owns the column.

**(b) Separator logic is distributed** across `set_blank`/`ended_blank`/
`last_was_tool` in `output.rs`, `ensure_dim` at the streaming/answer boundary,
per-commit separators in `repl.rs::emit_commit`, and the frame's own divider.
Each site decides "does this block need a blank line above it?" from partial
state. This is the same shape as (a) — knowledge that belongs in one place
re-derived in several — and the same fix: one function that says where a
separator belongs, called by every writer. Worth folding into the §3.2 sweep
while every writer is open, or as its own pass; not worth doing before it.

---

## 4. Phases

| Phase | Work | Gate |
|---|---|---|
| **B** | Row counting (§3.2) across all writers; drop the compensating DSR; the two entry DSRs stay. New `tests/` pty case reproducing the session-resume artifact, asserted against the report, then shown fixed. | full suite green; a `note_lines`-drift unit test (a wide-char and a boundary-width message) |
| **A1** | The `ContentOut`/`UiOut` split, the two anchors, the scoped re-anchor for replay, `live_bottom`. No visible change to the frame. | full suite green; the B artifact still fixed |
| **A2** | `ApplyPolicy::Reserve` + a static footer + the status handle. | full suite green; vt100 case pinning that a long turn stops above the footer and the footer survives |

B is the prerequisite for everything and is worth doing on its own merits: it
is §9 #6 and #10, plus the artifact.

---

## 5. Related, not here

- **Queued follow-up prompts** (typing mid-turn opens an input area that pushes
  output up) need A1's split objects *and* a concurrent editor, and a real
  decision about what a mid-turn prompt means for the turn that is running. Its
  own document.
- **`ApplyPolicy::Append`** stays deferred: no producer. `Reserve` is the policy
  §4.5 actually wanted.

---

## 6. Open questions

**6.1 (blocking A1) What happens when the input frame and the content would
overlap?** Today: DSR re-anchor, and the frame paints over whatever is below the
real cursor (or scrolls if it doesn't fit). The two honest alternatives:

- **(a) Scope the anchor down** — reclaim rows above the content's caret for the
  frame, pushing that content up. Costs a scroll (which is fine — the content is
  just scrollback by then) and needs a `UiOut`-side "content ended at row R"
  fact, which A1's split provides. Cost: one scroll, and the top-most lines of a
  long turn move out of view when a menu opens.
- **(b) Anchor at the content's caret as today** and accept that a menu opened
  right after a turn paints over the tail of the turn, pushing it into scrollback
  on the next scroll. Cost: the same class of surprise the resume artifact is,
  one interaction away from the REPL's steady state.
- (c) Keep the DSR for this case only (today's behavior), and let A1 own the
  tracking. Cheapest, honest, and still fixes the artifact.

Recommendation: (a) for the footer prototype — the reclaim is invisible, because
content that the frame displaces is content the user has already read — and (c)
until the queued-prompt work makes overlapping frames a first-class case.

**6.2 Does the REPL's input frame keep the reserved row, or move above it?**
With `F = 1` the input's last row is `term_h - 1` and the footer is the last row.
That means the input is never on the very bottom row, which is a small visual
change from today (today, after a short turn, the input sits right below the
output with empty space beneath it — so in the common case nothing moves; after
a long turn the input is at the bottom and would now be one row up).

**6.3 (§3.5a) Does the painter own the padding as well as the cursor column?**

---

## 7. Log

- 2026-09-28: written after the §9 #5/#11 painter work; #6 and #10 folded in as
  the phases of one capability. No code changed.
