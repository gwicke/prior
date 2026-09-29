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

| Commit | Work | Gate |
|---|---|---|
| **A1.1** | `plan_solve(cache, screen, w, h) -> String` in `ui/frame.rs`, where `Screen { frame: Option<(Frame, focus)>, content: Option<ContentWrite> }`. One solve: derive `cb`, place the frame (`min(cb + 1, term_h − H + 1)`), emit clear-old-frame → scroll → content → frame at the new top. `draw_frame`'s policy maps onto it; `ApplyPolicy` is unchanged in the public API. | **no visible change**: the existing vt100 frame tests and the REPL resize suite pass unchanged |
| **A1.2** | `write_content` promoted to the `Output` trait (`ContentBlock { bytes, kind }`) with a default that writes the bytes, so the five non-console impls do not change. B2's writers call it instead of writer + `note_lines`. | full suite |
| **A1.3** | The two-region clear (§2.2) and the partial-predicate condition: a frame may only take the row-diff path if the content did not move since the last apply, or `live`'s row indices are stale by the scroll delta. The resize-replay path goes through the solve. | full suite; vt100 case: a live frame + a height change, asserting the old region is cleared and the frame lands below the replayed content |
| **A1.4** | The cursor column (§4.1): `RenderContext { content_width }` + `RowCursor`, `paint_row` padding. Its own commit, byte-identical output. | existing paint tests byte-identical; the 12 `RenderContext` constructions updated |

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
