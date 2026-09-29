# The screen is a composition: content, status, input

**Status:** design discussion — nothing implemented yet
**Context:** follows `doc/ui_draw_v2.md` §9 #6 (content row counts drift) and #10
(streaming doesn't track rows). Those two are not two bugs: they are one missing
capability, and it is the gate on a live status line.
**Revises:** `doc/ui_draw_v2.md` §2 (apply policies) and §4.5 (fixed bottom
elements) — see §3.4. My own first draft of this doc proposed a narrower
"reserved band at the bottom" and deferred `ApplyPolicy::Append` as having no
producer. Both were wrong in the same way: `Append` is not a chunked-write
policy, it is the *content* policy, and the content is a first-class part of the
screen's composition.

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
scrolled and kept the cursor on the bottom row"). So a writer that skips a count
does not just under-count — after enough unaccounted output the tracked row is
pinned at the bottom, and the *next* counted write puts the anchor in the wrong
place.

### 1.1 The artifact this explains

> *incomplete clear on session resume; a very short session left content above
> the replay visible, with a table header row painted on top of it.*

`/session … resume` ends with `replay_last_turn_silent` (`repl.rs:658-670`,
reused from the width-resize path): `\x1b[0J` from the cursor, replay the last
turn, then a DSR re-anchor. The chain, as reported:

1. `ui::confirm` DSR-re-anchors on entry, then composes `[Divider?, Overlay]`
   **at the tracked cursor row**.
2. The turn had streamed, so the tracked row was the clamped bottom row — the
   real cursor was elsewhere, and `live` tracked a frame region that was no
   longer where it had been painted.
3. The menu resolved with `Frame::empty()`, whose union clear is anchored to that
   region. Wrong region → wrong clear → the replay paints over what it did not
   clear.

Row accounting removes the cause; a scoped re-anchor removes the failure mode.
Phase B pins the artifact against the report before fixing it.

---

## 2. The screen is a composition

Not "a frame painted at an anchor over some content that happens to be there",
but:

```
┌──────────────────────────────┐  row 1
│ scrollback content            │  … the LLM turn, in order, written once
├──────────────────────────────┤  content_bottom + 1
│ status line   (min 1, max 2)  │  ┐
├──────────────────────────────┤  ├ the *frame*: an ordered stack
│ menu / overlay  (variable)    │  │ of elements, each with (must, might)
├──────────────────────────────┤  ┘
│ input         (1 … n rows)    │  the caret lives here
└──────────────────────────────┘  row term_h
```

One solve answers where the frame goes and whether the content moves. With
`H` = the frame's height after allocation and `cb` = the content's last row:

```
frame_top = min(cb + 1, term_h - H + 1)        // following (default)
frame_top = term_h - H + 1                      // pinned
content_bottom ≤ frame_top - 1                  // always
```

Both regimes are the same rule with one term dropped, and both descriptions in
the design discussion are its two ends:

- **Following** — the frame sits directly below the content and the empty rows
  are at the bottom. After a tall menu closes, the content is mid-terminal with
  the status line just below it; each later append pushes both down until the
  content reaches the bottom of the live region.
- **Pinned** — the frame is always the bottom `H` rows; a short turn leaves a
  gap between the content and the frame. This is "a status line in the last
  row", and it is what the streaming prototype wants: the line is *stable*
  instead of sliding down after every block.

Which is the default is an open question (§5.1). Everything below is the same
machinery either way.

### 2.1 Appending content

`write_content` of `n` rows, with the current `H`:

1. `cb' = cb + n`.
2. If `cb' > frame_top - 1`, the content must move up by `delta = cb' - (frame_top - 1)`.
   Scroll first (bare LFs from the bottom row — never `CSI S`, ui_draw_v2 §4.3),
   so the reclaimed rows come from the top of the content into scrollback.
3. **Clear the frame's old region first**, if the scroll moved the frame (see
   §2.2) — otherwise the first `delta` rows of the old frame survive *above* the
   new content, which is exactly the class of artifact in §1.1.
4. Write the bytes; `cb = min(cb', frame_top - 1)`.
5. Re-apply the frame if the solve moved it (row-diff against `live`).

The cost of a footer is now explicit and small: step 5 is one `CUP` + one row
for a static status line, `H` rows for a growing input area.

### 2.2 The frame moves in two directions, and that is new

Today the frame is a lone rectangle above the cursor, so `plan_apply`'s union
clear is one region. With the composition there are two regions that can swap
order:

- **The frame grows** (a menu opens, the input wraps, a compose area appears):
  the content gives way upward. The frame's top moves *down* past the content's
  bottom; the content scrolls by the difference.
- **The frame shrinks** (the menu closes, the input goes away): the frame's top
  moves *up*, back to just below the content, and the rows it vacated are
  cleared — while the *new* content appended after that can be mid-screen with
  blank rows beneath it.

Both are the same solve with a different `H`; the delta emission is
`clear(old frame region)`, `scroll(delta)`, `write(content)`, `paint(frame at
the new top)`. The ordering constraint is that the old frame region is cleared
*before* the content is written, and that the clear never reaches below the new
frame's bottom. That is a two-region union clear where today there is one.

### 2.3 A dynamic input field makes the cursor interesting

When the input is a real editor rather than a prompt line:

- Its height changes as the user types, so `H` changes per keystroke *and* per
  streamed delta during a mid-turn compose. Every content append re-solves.
- The terminal cursor is the input's caret, but a content write moves it. Each
  append therefore ends with a `CUP` back to the input's caret — one escape per
  block, not a redraw.
- The input must be able to *take* rows from the content while the content is
  still arriving, and the content must be able to push the input down. Neither
  is a special case of the solve; they are the solve, run more often.
- Queued follow-up prompts add the semantic question (what does a mid-turn
  prompt *mean*) on top of the layout. Its own document.

---

## 3. API

### 3.1 Two mutators, one solve, one diff

The `ApplyPolicy` enum existed to say *what kind of apply is this*. Once the
screen is a composition, that question dissolves: the two things that can change
are the content and the frame, and each has one mutator.

```rust
/// Content changed by `n` terminal rows (measured, never guessed — §4).
fn write_content(&self, block: &ContentBlock<'_>);

/// The frame changed: recompose from the elements and re-solve.
fn set_frame(&self, elements: &[Element], focus: usize, dims: (usize, usize));
```

Each re-solves the composition and emits only the delta. `plan_apply` becomes
the solve plus the delta computation, with the content region as a second
participant; `DrawCache` grows `content_bottom` and `frame_top` next to `live`.
`live_bottom()` stays the one place that knows where the live region ends, so
callers do not re-derive it.

### 3.2 One content entry point

Yes — and it is the point of the whole design. Every content writer funnels
through `write_content`, so the layout is consulted in exactly one place:

```rust
pub struct ContentBlock<'a> {
    /// The bytes to write, ANSI and all.
    pub bytes: &'a str,
    /// What the block is, for the separator rule (§4.2).
    pub kind: BlockKind,     // Text | Reasoning | ToolCall | ToolResult | Error | …
}
```

`display_text` builds `⏺ ` + `termimad::inline(text)` and calls it.
`stream_text` renders into a local buffer, calls it, and the count comes from
the bytes. `paint_panel` calls it per line. Footers and menus are injected at
the same point, which is what makes "one place to inject them" true rather than
aspirational: they are part of the composition, not side effects of a write.

### 3.3 What this does to the `Output` split

The frame's anchor and the content's caret are different facts about the same
screen, and today they share one field. The composition separates them — with an
explicit relationship (`frame_top = min(cb + 1, …)`) and `live` invalidated
whenever either moves — and that separation is what `write_content` on the
trait exposes. The full `ContentOut` / `UiOut` trait split is *not* part of this
work (D4): the composition carries the two anchors internally, so the split's
remaining value is explicitness rather than capability.

### 3.4 Revisions to settled design (ui_draw_v2)

- **§4.5's `Append`** is the content policy, not a chunked-write policy. It gets
  a producer (every content write) instead of staying deferred. The "fixed
  bottom elements establish the lower bound the cursor may write to" in §4.5 is
  precisely `content_bottom ≤ frame_top - 1`.
- **§2's "Commit lines" row** ("clear the region with a Frame apply, then write
  the line where the caret lands") is the `write_content` path: the commit line
  is content, and the frame is re-applied under it.
- **§4.3's full-apply clear floor** (`min(term_h, screen_bottom)`) becomes
  `min(frame_top - 1, screen_bottom)`: the live region ends at the frame, not at
  the terminal edge.
- **§4.4's partial predicate** gains a condition: the frame may only take the
  partial path if the content did not move since the last apply (otherwise the
  row indices in `live` are stale by the scroll delta).

### 3.5 The status line's data

Live values (session name, provider:model, thinking level, ctx %) come from a
`StatusHandle` of atomics the agent updates as it runs, read once per solve.
A separate, small surface; its own paragraph when we get to phase A2.

---

## 4. Padding, the cursor column, and ANSI

### 4.1 The cursor column belongs to the painter

Row text must currently be laid out for `term_w - 2` because the painter adds a
two-column marker — a contract every producer has to remember, and one that will
be multiplied by the status line and the compose area. It should be the
painter's:

```rust
pub struct RenderContext { pub content_width: usize }   // was term_width

/// How a row's cursor is indicated, and how many columns it costs.
pub enum RowCursor { Columns, BackgroundOnly }           // 2 cols, 0 cols
```

`paint_row` gains a `cursor: RowCursor` and pads the row itself. Changing the
cursor indication later is then a one-line change with no producer touched, and
`render_multiline(tw - 2)` becomes `render_multiline(ctx.content_width)`. Output
is byte-identical for `Columns`, so it can land as its own commit and be
verified against the existing paint tests.

### 4.2 The ANSI interaction (the question that prompted this)

The painter owns padding **for the rows it paints** — overlay rows, the status
line, the input. Content rows are not painted by the frame at all (the frame
knows only their height), so syntax-highlighted content — tool panels, diffs,
highlighted code — keeps its own full-width padding and nothing about
highlighting changes.

For the rows the painter does paint, the composition order is
`[bg][fg][cursor][content][pad][reset]`, and three rules make that safe against
ANSI:

1. **The pad goes inside the style**, after the content and before the reset, so
   a background or an underline reaches the right edge instead of stopping at
   the text.
2. **A content-internal `\x1b[0m` re-opens the style.** Highlighted content
   carries its own resets; a reset mid-row would drop the background for the
   rest of the line. `paint_panel` already solves this by substituting
   `RESET` → `RESET + BG` (output.rs:1374-1381); the painter needs the same
   rule, and it is the reason that trick exists.
3. **Width is measured ANSI-transparently and in display columns**, not chars or
   bytes: `display_width` (already in `slashdot_menu::table`) skips escapes and
   counts wide chars as 2. A highlighted token's escapes must not inflate the
   pad, and a CJK cell must not under-pad.

None of this is new machinery; it is `paint_panel`'s contract generalised to
every row the frame paints, and it is why the painter — not the producer — has
to own the pad.

### 4.3 Separators

Folded into phase B (agreed): the counts would be wrong otherwise, since a
separator is a content block of height 1 or 0. The decision "does this block
need a blank line above it?" currently lives in five places (`set_blank` /
`ended_blank` / `last_was_tool`, `ensure_dim` at the streaming boundary, and
per-commit separators in `repl.rs::emit_commit`). With `write_content` as the
choke point and `BlockKind` as the input, it becomes one function
(`needs_separator(prev, next)`) called by every writer.

---

## 5. Settled decisions (2026-09-28, after review)

| # | Question | Decision |
|---|---|---|
| D1 | Following or pinned? | **Following** — the frame is painted top-down from the scrollback and sits directly below the content; menus appear below the status line. `frame_top = min(cb + 1, term_h − H + 1)`. |
| D2 | Keep the frame's 2/3 budget? | **Keep** (`allocate(elements, ⌈2/3·term_h⌉)`). The solve places the frame; the budget still bounds its height, and the top 1/3 stays scrollback. |
| D3 | API shape | **Additive.** `write_content` is added to `Output`; `draw_frame` and `ApplyPolicy` stay, with the policy mapping onto the internal solve. A full migration to `set_frame`/`write_content` is a recorded follow-up, not this work. |
| D4 | `ContentOut`/`UiOut` trait split | **Deferred** — the composition carries the two anchors internally; the split's payoff is explicitness, not capability. Revisit with the queued-prompt work. |
| D5 | Order | **The row counter first.** `content_bottom` has to be *true* before a composition can be built on it, and the counter is also §9 #6 and §9 #10 plus the resume artifact. |
| D6 | Frame element order | `[Status, Input?, Divider?, Menu?]`. The status line is the frame's top element. The input is present only when not streaming. The divider only when something is visible below it (a menu). The menu's own `detail` + `rows` are vertically balanced inside the painter and the menu as a whole is a flexible element of the frame's stack (`allocate`'s even surplus division) — both already true, now written down. |
| D7 | Status line tone | The table-header background (`\x1b[48;5;237m`), not the data-row grey. |
| D8 | Padding | The painter pads frame rows that arrive unpadded; content writers keep padding their own rows for consistent backgrounds, and the painter never touches content rows. |
| D9 | Entry DSRs | **Keep** `read_input`'s and `ui::confirm`'s after the counter lands. They become unnecessary, but they are the documented guard for *foreign* writers (a tool's subprocess on the tty), and they cost one round trip per prompt. Removing them is a follow-up with its own justification. |

### 5.1 What following buys

`content_bottom` is a *derivation*, not a field:

- `live.is_some()` (only right after a frame apply) → `cb = live.frame_top_row - 1`
- `live.is_none()` (after any content write or re-anchor) → `cb = caret.0`

so `DrawCache` keeps `caret`, `live`, `screen_bottom` and the §4.2 invalidation
contract is unchanged. A *pinned* frame would break the derivation — a gap
between content and frame means `cb ≠ live.top − 1` — and would need a third
fact. D1 is therefore load-bearing for the state model, not just the look.

### 5.2 Still open (A2, cheap to settle)

- **Status line cadence.** Repaint on every streamed block (free under the
  composition, since `write_content` re-applies the frame) or on a timer
  (~10 Hz, cheaper, allows a spinner). Recommendation: per block; a 1-row
  repaint is invisible and the code is simpler.
- **Where ctx% comes from** mid-turn — the agent's usage accounting must be
  published as it runs, not only at turn end.
- **The exact status content** — `[session | provider:model | level | ctx XX%
  · 234k]` is a starting point; which fields are always present, and whether
  the line truncates or elides when the terminal is narrow.

---

## 6. Plan

### Phase B — the row counter (§9 #6, §9 #10, the resume artifact)

| Commit | Work | Gate |
|---|---|---|
| **B1** | `RowCounter`: a `Write` adapter that accumulates the terminal rows its bytes occupy at a width — `1 + (display_width(segment) − 1) / width` per `\n`-separated segment, ANSI-transparent, display-column aware. Lives next to the existing text measurement in `output.rs`. Pure; unit tests for wrapping, wide chars, escapes, and a trailing newline. | crate unit tests |
| **B2** | Every content writer renders into a local buffer and counts the bytes it is about to write: `display_text` (#6), `display_markdown`, `paint_panel`/`tool_panel` (per painted row), the 7 `Renderer::feed` sites (`stream_text`, `stream_thinking`, `commit_thinking`, the replay paths). One private `ConsoleOutput::write_content(bytes, kind)` becomes the single choke point. The compensating DSR at `repl.rs:770` goes. | full suite; **a screen-truth test**: drive one of each writer through a real `ConsoleOutput` and assert the tracked row equals the vt100 screen's cursor after each (this is the real safety net — the count is either right or the next frame lands wrong) |
| **B3** | Separators: `BlockKind` + one `needs_separator(prev, next)` replacing `set_blank`/`ended_blank`/`last_was_tool` and the `ensure_dim` boundary. *Last in the phase*, so a diff in blank lines cannot obscure B2's fix. | full suite; before/after transcript check for a few shapes (text → tool → text, reasoning → answer, error after content) |
| **B4** | The resume artifact, as a regression test. Most likely shape: a pty test that puts a dialog up, tears it down, replays a short turn and asserts no residue — which may need a test seam (an injectable writer on `ConsoleOutput`); the pty suites in `tests/` show the pattern. | the new test fails before B2 and passes after |

### Phase A1 — the composition (additive)

Design below (§6a); the staging is:

| Commit | Work | Gate |
|---|---|---|
| **A1.1** | `plan_solve(cache, screen, w, h) -> String` in `ui/frame.rs`, with `plan_apply` kept as the frame-only adapter so the byte stream is *identical*. The shape lands; nothing consults it yet. | **no visible change, byte for byte**: the vt100 frame tests and the REPL resize suite pass unchanged |
| **A1.2** | `write_content` promoted to the `Output` trait (`ContentBlock { bytes, kind }`, default = write the bytes, so the other impls do not change) and routed through the solve. Content now places itself *below a live frame* instead of over it. | full suite; the resume-artifact case (§1.1) as a regression test — this is the commit that closes it |
| **A1.3** | The two-region clear (§2.2) and the partial-predicate condition: a frame may only take the row-diff path if the content did not move since the last apply, or `live`'s row indices are stale by the scroll delta. The resize-replay path goes through the solve. | full suite; vt100 case: a live frame + a height change, asserting the old region is cleared and the frame lands below the replayed content |
| **A1.4** | The cursor column (§4.1): `RenderContext { content_width }` + `RowCursor`, `paint_row` padding. Independent of the rest; its own commit, byte-identical output. | existing paint tests byte-identical; the `RenderContext` constructions updated |

### Phase A3 — the frame without `CUP` (approved 2026-09-28)

The frame is drawn from the cursor, relatively, so it can never address a row it
did not mean to. This retires the whole *drift* class at once and collapses two
planned designs into one.

**Why it works.** A block does not need a row, only a starting point. Relative
primitives (`CR`/`LF`, `CUU`, `CUD`, `CUF`, `CUB`, `EL`, `ED`) are enough:

```
paint row 1;  CUD 1;  paint row 2;  …  CUU (h-1-caret_row); CR; CUF col   ← the caret
```

Two cases carry a wrinkle, and both are handled:

- **The erase walks bottom-up.** `CUD` does not scroll at the bottom row, so a
  region clear cannot walk *down* out of the block. From the block's bottom:
  `EL`, `CUU 1`, ×h. `CUU` clamps at row 1, so the loop is safe when the block
  starts at the top.
- **The block that does not fit.** Only reachable when the cursor *is* at the
  bottom row, so `\n` ×k scrolls and `CUU k` reaches the top of the freed rows.
  The scroll is relative too.

**What stays absolute: one thing.** A resize reflows the screen and a foreign
writer can move the cursor, so the frame asks where it is (`CPR`) and moves
relatively from the answer. That is a *question*, not an address, and the two
entry DSRs in `read_input` are already exactly that. `ED 0` covers the height
change without addressing anything.

**What it retires.**

- `frame_anchor()` and `live.frame_top_row`. The frame is wherever the cursor is,
  so its row is never computed, stored, or re-derived.
- The **tracker-vs-real-cursor drift class** — the resume artifact, the
  DSR/tracked disagreement, and the `llamacpp:testLet me run a command.` class
  from A2.1 all come from the same root: addressing a row derived from tracking.
  No address, no wrong row. B1's row counter stays (the content still needs it,
  and the tests compare against it), but the frame stops being a second opinion.
- **A2.2 and the streaming-input feature become configuration.** The block is
  `[input (+ status on the right), divider?, menu?]` at the cursor — always,
  streaming or not. No second render mode, no mode switch, and the "a frame owns
  the tail rows" suppression in `paint_status` disappears, because the status
  simply rides on the input row.

**What it costs.**

- **Arbitrary placement is gone.** The frame lives at the cursor. That is the
  point, and it is what lets the input follow a stream — but `ApplyPolicy::Frame`
  ("draw here") has nowhere to point, so the policy retires with the anchor.
- **The block's height must be known** to walk and to erase. It is.
- **A foreign writer's cursor is invisible to us** without the DSR. Concretely:
  **indicatif's spinner is a foreign writer** — it writes `\r` + text at the
  cursor 8×/s. The frame path and the spinner must be mutually exclusive (the
  spinner stopped before any frame apply, or restored at the end), or the
  relative erase will eat the rows above wherever the cursor actually is.
- **That last point is the real trade-off, stated plainly**: relative addressing
  is robust against *drift* for painting, but a *cursor-position* error now
  propagates into the erase (h rows above wherever the cursor is) instead of
  hitting a bounded, computed row range. The DSR escape hatch is what covers it,
  which makes "when do we re-anchor" a real question rather than a formality.

#### A3.1 — the frame path goes relative (done)

`plan_full`, `plan_partial`, `plan_clear_live_frame` and `plan_append` emit no
absolute address. `cup` survives in exactly one function, `DrawCache::reanchor`,
and `no_apply_addresses_a_row` / `reanchor_is_the_one_addressing_path` hold both
halves of that: every apply path is scanned for a real `CSI … H` (scanning the
escape, not the letter, so frame content containing an `H` cannot pass for an
address), and the re-anchor is asserted to keep using one.

Five things had to be worked out, and each was a bug first:

- **The scroll is `h-1` bare LFs.** An LF scrolls at the bottom row and moves
  down one anywhere else, so `h-1` of them put the block's last row on the
  cursor and pushed exactly `delta` lines into scrollback — the same split
  `plan_full` used to compute from the tracked row, except the *terminal*
  decides it, so the block fits even when the tracked row was wrong. This is
  what removes the last need to know the row in order to place a block.
- **The invariant is `live` set ⟺ the cursor is at that frame's caret**, which
  holds because every other exit (`plan_clear_live_frame`, a content write's
  `note_lines`, `reanchor`) drops `live`. It is what makes the walks
  well-defined — and it is load-bearing, because a full apply *starts* by
  walking to the block's top. Missing that walk made the block creep down one
  row per redraw whenever the caret sat on the block's last row.
- **Erase directions are not interchangeable.** `CUD` clamps at the bottom row
  (no scroll) and `CUU` clamps at the top, so a block's own rows must be erased
  bottom-up from its last row, while a caller already standing on the first row
  walks down. Factoring the two into one helper is how `erase_block` briefly
  erased the wrong rows — hence `erase_up` / `erase_down` as separate names. A
  downward clear also has to step down *before* its first `EL`, or it erases the
  block's own last row.
- **`CUF` is relative, so the column has to be known.** After painting a row
  the cursor is at that row's end, so every caret move is `CR` first. Guessing
  put the caret at the end of the row *plus* the caret's column.
- **A partial apply must follow the *new* frame's caret, not the old one's.**
  A frame that grows a row below the caret (Shift+Enter, a re-wrap) left the
  cursor on the previous caret's row, so the next keystroke painted the second
  row where the first was and the first row's text vanished. The pty test caught
  it; nothing in the module could, so
  `a_frame_that_grows_below_the_caret_takes_the_caret_with_it` pins the three
  applies that produce it.

`frame_caret` is the piece the rest leans on: the frame's caret as a
frame-relative `(row, col)`, or the block's last row column 0 when there is
none. Every walk is anchored to it, so it survives a re-anchor, a resize, or any
disagreement about the absolute row — `cache.caret` remains the absolute
restatement, for row accounting alone.

**The re-anchor changed shape, and it fixed a hole.** It now returns the bytes
that erase the live block's rows, because a cursor that has *left* the block
puts those rows out of reach of every relative walk — the erase that precedes
every paint walks up from the cursor and would clear the wrong rows entirely.
The old code had the same gap and did not close it: `reanchor` cleared `live`,
and the next full apply's union clear measured from `live`, so the old rows were
never reached. `cursor_row()` writes those bytes, which is why it is the one
`Output` method with a side effect beyond its name — and it costs nothing when
no frame is live, since the bytes are then just the cursor park, so it is right
at every one of its thirteen call sites.

Not yet done, deliberately: `frame_anchor` and `live.frame_top_row` still exist
and still carry the bookkeeping, so `plan_apply` still takes an `anchor` and
still computes `top`/`delta` — none of it is used for addressing, and
`no_apply_addresses_a_row` is what keeps that true. A3.2 removes them.


**Performance.** In bytes and in latency this is close to a wash, and the
difference is not where intuition suggests:

| | bytes | syscalls | cells painted |
|---|---|---|---|
| absolute, dense update | `k × (8 + row)` | 1 | same |
| relative, dense update | `k × (6 + row)` + `O(h)` walk | 1 | same |
| absolute, sparse in a tall block | `k × (8 + row)` | 1 | same |
| relative, sparse in a tall block | `k × (6 + row)` + `O(h) × 4` | 1 | same |

Addressing is 4–8 bytes against a row of 80–200, so it is 3–8% of a repaint, and
every repaint is already a single buffered write (one syscall, flushed once).
Relative is *better* for dense updates and worse only for a sparse update in a
tall block — e.g. two changed rows in a 30-row menu costs ~116 bytes of walking
against 16 bytes of `CUP`. Menu navigation is dense (a selection move repaints
the list), so this is the rare case, and it is bounded.

**The one real performance *gain*** is the caret fast path for typing, which is
the reviewer’s own idea and worth building deliberately: the cursor already sits
at the caret, so a plain insertion at the end of a single-line input needs
nothing but the inserted characters — 1 byte per keystroke, against a full-row
repaint today. Its preconditions are what make it an optimisation rather than
the default: no wrap (we know the column and the width, so we can predict it),
no styling in the row (a partial write would inherit the previous `SGR` state —
or emit a reset), no selection, and no layout change (`Shift+Enter`, re-wrap).
When any of those fails, fall back to repainting the changed rows and walking the
caret back: `CUU`/`CUD` to the row, repaint, `CUU`/`CUD` back to the layout
caret. That fallback is always correct and costs ~12 bytes of walking on top of
what a repaint costs today.

The caret is tracked as two positions, which is exactly the reviewer’s framing:
the *layout* caret `(row, col)` the editor computes, and the terminal's cursor,
which after a paint **is** the layout caret. A keystroke moves the editor's;
the delta is the walk back.

## 6a. A1 in detail

### The screen, as the solve sees it

```rust
/// What changed since the last solve. Either side may be absent: a frame-only
/// update is a keystroke, a content-only update is a streamed block, and both
/// at once is a menu commit (content already written, frame about to be torn
/// down and replaced).
pub(crate) struct Screen<'a> {
    pub content: Option<ContentWrite<'a>>,
    pub frame: Option<&'a Frame>,
}

/// Content bytes and the height they occupy — measured by `row_count` at the
/// point of writing, never predicted.
pub(crate) struct ContentWrite<'a> {
    pub bytes: &'a [u8],
    pub rows: usize,
}
```

`focus` is already inside the frame (`Element::Overlay { focus }`), so it does
not travel separately.

### One solve

```
cb        = content_bottom(cache)            // derivation, §5.1
H         = frame.map_or(0, Frame::height)
frame_top = min(cb + 1, term_h - H + 1)      // D1: following
live_end  = cb + H
over      = live_end.saturating_sub(term_h) // rows the content must give up
delta     = over + content_rows              // the content always lands whole
```

then, in order:

1. **Clear the frame's old region** if it is moving or shrinking. It is
   contiguous and directly above the content, and it may extend *below* the new
   content start (§2.2), so this has to happen before the content is written.
2. **Scroll `delta`** — bare LFs from the bottom row, never `CSI S`.
3. **Write the content** at `cb' = min(cb + content_rows, live_end)` and
   `content_bottom = cb'`.
4. **Paint the frame** at `frame_top`, or take the row-diff path when nothing
   below the caret moved: same `frame_top`, same size, and `content_bottom`
   unchanged since the last apply.
5. **Update the cache**: `live` (with the new top), `caret`, `screen_bottom`.

Steps 1–2 are the *only* place the composition emits clearing and scrolling;
`plan_full`'s existing `from..=floor` union clear and `plan_partial`'s shrink
remainder both fall out of step 1 + step 4. `floor` becomes
`min(term_h, screen_bottom)` for now and `min(frame_top - 1, screen_bottom)`
once `Reserve` exists (A2.3) — one term.

### `content_bottom` is a derivation, not a field

```
fn content_bottom(cache: &DrawCache) -> usize {
    match cache.live {
        // A frame is always a contiguous block directly below the content
        // (D1), so its top row *is* the content's bottom + 1.
        Some(live) => live.frame_top_row.saturating_sub(1),
        // No frame: the cursor is where the content ended.
        None => cache.caret.0,
    }
}
```

So `DrawCache` keeps `caret`, `screen_bottom`, `live` and the §4.2 invalidation
contract is untouched. A *pinned* frame would break this (a gap between content
and frame means `cb ≠ live.top − 1`) and would need a third fact; that is D1
again, and it is why "following" is load-bearing rather than cosmetic.

### Two mutators, and what A1.2 changes on screen

```rust
fn write_content(&self, block: &ContentBlock<'_>)   // → solve(Screen { content, frame: None })
fn draw_frame(&self, frame: &Frame, anchor: usize, policy: ApplyPolicy, dims) -> (usize, usize)
```

`draw_frame` keeps its signature for now (D3): it builds
`Screen { content: None, frame: Some(frame) }` and the policy maps onto the
path, so the public `Output` surface is unchanged by A1.1–A1.3.

**A1.2 is the first commit that changes what is on screen, and deliberately
so.** Today a content write lands wherever the cursor is, *over* a live frame
(the REPL's empty input frame is live at the prompt), and the frame is
invalidated — which is how a replay after a menu can paint over content that
was never cleared. With the composition, the content goes below the frame and
the frame is repainted beneath it: no overlap is possible, and the old frame's
rows are cleared first. That is the resume artifact (§1.1) closing at the
structural level rather than by re-anchoring harder.

It also *moves* content in that one case: after a menu commit, the replayed
turn starts one row lower than it used to (below the frame's height) and the
frame's rows are repainted rather than overwritten. Nothing in the current test
suite observes that; the artifact is the thing being fixed, so the change is
visible only where the old behaviour was wrong.

### What this does *not* change

- The 2/3 budget stays on the frame's own elements (D2), and
  `allocate`/`window`/`render` are untouched — A1 changes where the frame goes,
  not how it is composed.
- `ApplyPolicy` keeps its public meaning; internally the solve picks the path.
- Nothing re-anchors by DSR. The entry DSRs stay (D9).
- The content's own rendering is untouched: B1–B3e made the bytes and the
  separators correct, and the solve only decides *where* they land.

### A2's rendering model: three zones, and only one of them may use `CUP`

Settled by review (2026-09-28), and it replaces the reserved-band design of §2
for the status line. The three zones:

| Zone | What | How it writes |
|---|---|---|
| **1. Scrollback** | assistant text, reasoning, tool panels | append-only, LF-driven — unchanged, and unchanged by A1 |
| **2. The status line** | one transient readout line at the cursor | **cursor-relative only** |
| **3. Below it** | the input line, its divider, a menu | absolute (`CUP`) — and only re-rendered on a keystroke, a resize, or a dialog |

**Why zone 2 cannot be absolute.** `CUP row;colH` addresses the *viewport*. A
user scrolled back is looking at older rows, so an absolute status repaint
overwrites what they are reading — and most terminals pull the viewport to the
bottom anyway. A cursor-relative write always targets the logical cursor, which
when scrolled back is *below* the viewport: invisible, never corrupting. That
degradation is the whole point, and it is why the rule is "relative or nothing".

**Why zone 3 is safe.** Its repaints are user-driven: the frame changes when the
user types, resizes, or opens a menu. A user who has scrolled back is not
typing, so no absolute write happens — *except* for a **confirm dialog**, which
pops up because the *agent* asked, not the user. That is zone 3's one exception
and it is not new (dialogs have always used the frame path); worth knowing,
not worth redesigning.

**The status line is transient, not a region.** It occupies the content's last
row and is overwritten by the next content write:

```
content block … LF        cursor at column 0 of a fresh line
write_status(text)        relative: the text, no trailing LF   → cursor at its end
  … the turn ends, the user reads, the frame is drawn …
next content write:  CR + ESC[2K   (erase the status line)
                     boundary()   (one blank, if the stream owes one)
                     the content
```

Nothing is reserved, so the "live region ends above the footer" invariant of §2
—and with it `ApplyPolicy::Reserve`, the footer as a frame element, and the
placement half of the solve — all disappear. The status is a line the content
stream owns, not a rectangle the screen reserves.

Two preconditions, both already available:

- **The status may only be written with the cursor at column 0**, or the next
  content write's line-erase would destroy a partial content line.
  `StreamTail` already tracks exactly that bit (`last_nl`), so the check is
  free, and `write_status` is a no-op when it does not hold.
- **Erase before the boundary check.** The status text has no newline, so the
  tail reads "not blank"; if the next write did not erase it first, the
  boundary would emit a blank and leave the status stranded as its own line in
  the scrollback. Erasing first also puts the tail back where the boundary
  expects it.

**Cadence and data.** `ctx` only moves at end of turn, so the text is set once
at the start of a turn and painted by the output after *each* content block —
the readout is live for the whole turn, and the context figure is the previous
turn's (the only one available then). `CodeAgent::prompt` sets it from the
session name, `provider:model` and `usage.last_input_tokens`, reusing
`model_catalog` for the window the way `session_stats_lines` does so the two
cannot disagree about what "ctx" means. The thinking level is *not* in it: the
agent carries none (it went with the hold-back in B3b), and it only changes on
an explicit commit, so it belongs to the prompt-side carrier.

**The status is transparent to the blank-line rule.** It sits on the row the
content's trailing newline left, so the next block must not find that row "not
blank" and add a second blank above itself. That is why it is written straight
to the writer, *outside* the row counter: routed through the counter it became
opaque, and every block grew a blank line (visible in the mock transcript as
`end_to_end_separator_layout` going from 1 blank to 2). Its one real row
movement — a status wider than the terminal wrapping, which the terminal does
without a newline — is reported with `rows_of`.

**The erase is relative too**, `CR` + `EL` (+ `CUU` per wrapped row), and it
needs no row at all: the cursor has not moved since the status was painted. An
earlier version addressed the *tracked* row, which is the wrong row whenever the
tracker and the real cursor disagree by one — and then the content landed beside
the status (`llamacpp:testLet me run a command.`) instead of over it. Relative
is also what keeps the erase out of a scrolled-back viewport.

A frame suppresses the paint, and the suppression is *sticky across a burst*:
one block is often two writes (a boundary, then a body) and only the first
erases the frame, so a per-write "is a frame live?" would let the second paint
onto the frame's rows.

**This removes most of A1** — and the piece that remains turned out to be the
resume artifact itself. The composition's purpose was to reserve space for a
bottom element; with a transient line there is nothing to reserve, so the
placement solve, `Reserve`, and the status as an `Element::Lines` all go.

What survived is the part that was never about placement, and it is **done**
(2026-09-28): a content write while a frame is live now erases that frame
first (`ui::frame::plan_clear_live_frame`). Reproduced before the fix — a menu
frame plus one `display_text` left

```
⏺ Cannot start agent: Missing API key fo
r provider.      CTX    PRICE
  gpt-5          400k   1.00
```

i.e. the message with the menu's table header and rows *underneath* it. The
replay that runs while `/session`'s menu is still up is the real path:
`emit_commit`'s frame collapse happens *after* the node's submit, so it is too
late. The clear is the whole region (EL per row) plus a caret restore, because
a content line shorter than the terminal also leaves the frame's background to
its right; and it costs nothing while a turn runs, since streaming has no live
frame to clear.

The **partial-predicate condition** turned out to be unnecessary: `note_lines`
already cleared `live` on every content write, so the row-diff path was
unreachable once content moved. The condition was implied by the very
invalidation that made this bug possible in the other direction.

**And an input line cannot join zone 2**, which is the right answer to the
uncertainty in the review: an input needs a stable caret that editing redraws
around, and rewriting a line *above* the cursor needs absolute moves. The input
stays in zone 3, where its only redraw trigger is the user — so it inherits the
yank-on-type that already exists and adds nothing.

### Open, for A1 specifically

1. **A scroll moves content that nobody asked to move.** In the
   frame-live + content case, the content above gives up `delta` rows. Today
   that only happens when the frame does not fit; with the composition it also
   happens when a short frame sits above a lot of content. Visually: the top of
   a long turn scrolls off slightly earlier. The alternative — refuse to push
   content and let the frame overlay it — is the bug. Recommendation: accept,
   and pin it with a vt100 case so the amount is visible in a test rather than
   discovered by eye.
2. **Does `Frame::empty()` (the dialog resolve) still work through the
   solve?** It is a zero-height frame; `H = 0` makes `frame_top = min(cb + 1,
   th + 1) = cb + 1`, and step 1 clears the old region. That is exactly the
   current union-clear behaviour, so it should be a no-op change — worth
   confirming against the confirm pty suite rather than assuming.

### Phase A2 — the status line

| Commit | Work | Gate |
|---|---|---|
| **A2.1** | `StatusHandle`: atomics the agent publishes as it runs (session, provider:model, thinking level, ctx used/total), read once per solve. | unit tests on the handle |
| **A2.2** | `Element::Lines` gains a style (`RowStyle` from the painter, header tone) so the status line is a real frame element; `compose_frame` builds `[Status, Input?, Divider?, Menu?]` with the D6 rules. | full suite; `Element::Lines`'s `#[allow(dead_code)]` retired |
| **A2.3** | The status line during streaming: the frame is `[Status]` alone, repainted by `write_content` after each block — the case §2.2's two-region clear exists for. | **vt100 case**: a long streamed turn stops above the line, the line survives, and no residue remains above the content |

### Follow-ups (recorded, not in this work)

- Full migration of `draw_frame` → `set_frame`, and retiring `ApplyPolicy` (D3).
- The `ContentOut`/`UiOut` split (D4).
- Removing the two entry DSRs, with the foreign-writer argument retired (D9).
- Queued follow-up prompts: a real editor while the turn runs, the cursor
  policy (§5.3 of the previous revision, now the compose-area question), and
  what a mid-turn prompt means semantically.
- The separator/divider sweep beyond `needs_separator`: one place that decides
  *all* vertical chrome.

## 7. Log

- 2026-09-28: written after the §9 #5/#11 painter work; #6 and #10 folded in as
  the phases of one capability. No code changed.
- 2026-09-28: revised after review. The first draft modelled a reserved band at
  the bottom and deferred `ApplyPolicy::Append`; the composition model above
  subsumes it, explains `Append` as the content policy, and answers the cursor
  question the reserved band left open. Added §4 on the painter-owned cursor
  column and its ANSI rules.
- 2026-09-28: plan finalised. Decisions D1–D9 recorded in §5, commits in §6:
  the row counter first (it is also §9 #6, §9 #10 and the resume artifact),
  then the composition, then the status line. A scoped re-anchor was dropped
  from the scope: with the counter in place the entry DSRs are simply
  unnecessary, and they are kept anyway as the documented guard for foreign
  writers (D9) — one fewer primitive to design.
