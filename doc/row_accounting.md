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

The split sketched in the first draft (`ContentOut` / `UiOut`, two anchors) is
still right, and this design is *why*: the frame's anchor and the content's
caret are different facts about the same screen, and today they share one
field. With the composition they are separate values with an explicit
relationship (`frame_top = min(cb + 1, …)`), and `live` is invalidated whenever
either moves.

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

## 5. Open questions

**5.1 (blocking A2) Following or pinned?** `frame_top = min(cb + 1, …)` keeps
today's look — the prompt sits directly under the last line of output, and a
static status line slides down as a long turn accumulates, then pins. Dropping
the `min` puts the frame at the bottom always: the status line is stable and the
prompt sits at the bottom of the screen, with a gap after a short turn. The
design discussion asked for a status line "in the last row" (pinned) and
described the following behaviour for appends; both are the same solve, so the
only cost of choosing is the look. Recommendation: pinned — a status line that
moves is distracting, and it is what every comparable CLI does — but it is a
visible change to where the prompt appears, so it is your call.

**5.2 Does the menu case need the content's history, or only its bottom?**
A tall menu pushes the content up; the solve needs to know how far to scroll,
which needs the content's *height* (or just that it exceeds the live region —
scroll by the difference). I believe the bottom row plus `live` is enough, but
a menu taller than the whole live region needs the 2/3 budget to still hold
(ui_draw_v2 §8 OQ1), and that is an argument for keeping the budget on the
frame's *own* elements, not on the composition.

**5.3 What is the cursor during a mid-turn compose?** The input's caret, by
definition — but then the status line and the content are painted *around* a
cursor the reader is not using, and Esc/Ctrl-C have to reach the compose area
rather than the (absent) REPL. The confirm gate (§1 of ui_draw_v2 §9) is the
existing precedent for "something else owns stdin", so the mechanism exists; the
policy does not. Deferred to the queued-prompt document, but the phase A2
prototype should at least not paint over a live caret.

---

## 6. Phases

| Phase | Work | Gate |
|---|---|---|
| **B** | `write_content` + the row counter across all writers (§9 #6, #10); the separator rule (§4.3); drop the compensating DSR; the two entry DSRs stay. A `tests/` pty case reproducing the session-resume artifact, asserted against the report, then shown fixed. | full suite green; a `note_lines`-drift unit test (wide-char and boundary-width messages) |
| **A1** | The composition: `ContentOut`/`UiOut`, `content_bottom` + `frame_top` in the cache, the two-mutator solve, the two-region clear, the §3.4 revisions. A scoped re-anchor for replay. Cursor-column change (§4.1) as its own commit. | full suite green; the B artifact still fixed; existing paint tests byte-identical |
| **A2** | `StatusHandle` + a static status line as the frame's bottom element, following or pinned per §5.1. | full suite green; a vt100 case pinning that a long turn stops above the line and the line survives |

A2 is what proves the tracking end to end; B is worth doing on its own merits —
it is §9 #6 and #10 plus the artifact.

---

## 7. Log

- 2026-09-28: written after the §9 #5/#11 painter work; #6 and #10 folded in as
  the phases of one capability. No code changed.
- 2026-09-28: revised after review. The first draft modelled a reserved band at
  the bottom and deferred `ApplyPolicy::Append`; the composition model above
  subsumes it, explains `Append` as the content policy, and answers the cursor
  question the reserved band left open. Added §4 on the painter-owned cursor
  column and its ANSI rules.
