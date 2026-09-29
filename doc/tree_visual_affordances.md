# `/tree` visual affordances — showing connected-ness across an interleaved listing

## Status

**Geometry and colour decided; not yet implemented.** Follow-up to
`doc/tree_indent_refinement.md`, which settled the indentation (spine) rule
and explicitly deferred markers and box-drawing connectors to "one coherent
follow-up design". This is that follow-up.

The row anatomy (§5), the glyph set, the two connectors and the colour
scheme (§6) are settled in review; the split into *data* (the tree emits
fields) and *paint* (the generic gutter layer draws the prefix) is the
ownership model that falls out of the gutter being generic. What is left
is three tunables (§7 #10) and whether the main line's red anchor reads as
an anchor or as an alarm (§6.1).

One prerequisite landed in the meantime: `Table::fit` now truncates
ANSI-aware (commit `8657429`) — see §3.4.

---

## 1. The question

> With the potential interleaving, is there a good way to indicate
> connected-ness of chunks? Or would different background colors (per path,
> so main would have contiguous, color change at minor branch point) work
> better? Maybe combined with markers only at the branch point?

The premise is right and worth stating precisely, because it decides the
answer: **row order is chronological** (a decision, `tree_indent_refinement.md`
§10 #5), so two long lines of work that branched early alternate
vertically:

```
kick off                      ← main line
  reply 1
    experiment prompt         ← excursion A starts
    experiment reply
  second message              ← main line resumes
  reply 2
    a different idea          ← excursion B starts
    its reply
  third message               ← main line
```

Indent answers *"how far from the main line"* — a depth. It does **not**
answer *"which excursion am I in"*: two excursions at level 1 are
indistinguishable, and a single excursion's rows are scattered, so nothing
groups them.

So the missing information is:

1. **identity** — which excursion does this row belong to? (grouping across
   gaps)
2. **location of the split** — where did this line of work start?

Those are two different questions, and they want two different mechanisms.

---

## 2. The decisive constraint: adjacency

A vertical rail (`│` at the excursion's column) asserts *contiguity*: it
says "the rows above and below this one, in this column, are the same
thread". That assertion is true only if the rows of a branch are adjacent,
which chronological order deliberately breaks.

| | adjacent rows (spine-first order) | interleaved rows (chronological) |
|---|---|---|
| vertical rail | correct and legible | **lies** — the eye looks for a line that isn't there |
| per-row colour | works | **works** — colour is an attribute of the row, not a position |

This is why the box-drawing idea and the "chronological order" decision are
in tension, and why the classic tree drawing (and `git log --graph`) buys
its legibility *by reordering*. Connectors are the right tool for Option C
(spine-first ordering) and the wrong tool for what we have.

**Conclusion: adopt the box-drawing *characters*, but only as event markers
at branch points — not as continuous rails.** Combined with per-path colour
for identity, that covers both questions in §1 without contradicting the
ordering decision.

---

## 3. What the pipeline can and cannot carry

Facts from the code, since they decide where each mechanism has to live.

### 3.1 The data layer must stay ANSI-free

`Row::description` is not just display text:

- it is the **fuzzy haystack** — `rank_candidates` is fed
  `format!("{} {}", row.id, d)` (`cursor.rs:154`), and per-column match
  indices are computed over the display fields (`cursor.rs:103-147`);
- it is the **header source** — `TreeListNode::header` re-renders it
  (`src/v25_tree.rs:774`, `trim_start` + re-wrap);
- it is the **ellipsised cell** (`Table::fit`).

Putting SGR in the description would put escape bytes in the search text
and shift the match indices used for highlighting. So **colour cannot be
emitted from `data()`**; it has to be applied by the view, which is the
only layer that knows about selection, active path and match indices.

### 3.2 The view already owns row styling — and the row background

`render.rs:319-330` paints every data row:

```rust
let bg = if is_selected { "\x1b[48;5;239m" } else { "\x1b[48;5;235m" };
let fg = if is_selected { "\x1b[1;36m" } else { "" };
let marker = if is_active { "* " } else if is_selected { "> " } else { "  " };
```

Two consequences:

- **there is already a 2-column gutter** (`*` = active path, `>` =
  selection). A branch glyph and a colour band want to live there, or
  immediately left of it.
- **every row already has a background.** A per-path *full-row tint* would
  have to replace `48;5;235`, which is a much louder design and collides
  with the selection colour. A 1–2 column *band* does not touch it.

### 3.3 `ColumnDef` has no style hook

`fields.rs:225-241` — only `field`, `header`, `align`, `min_width`,
`priority`, `wrap`. There is no per-column styling concept, so any colour
work needs a small extension: a `tint`/`style` on the column, or a
`Row::style` side-channel consumed by `view.rs::rebuild_rows` at the point
where cells are built (after snippet extraction, before `add_row`).

### 3.4 Truncation is now ANSI-safe; the snippet path is not

| path | escape-aware? |
|---|---|
| `wrap::atoms` (wrapped columns) | yes, already |
| `Table::fit` (ellipsised columns) | **fixed** in `8657429` — was cutting mid-sequence and losing the reset |
| `extract_snippet` (fuzzy, `table.rs:476`) | **no** — per-char width table, `char_width('\x1b')` → 1 |

`extract_snippet` is the reason §3.1 matters: if styling is applied
*before* it, the snippet window is computed over a string with escape
bytes in it. Applied *after* (in the view), the cell it receives is already
the right length and the path is not exercised. Making `extract_snippet`
escape-aware is still worth doing defensively, but view-side application
avoids the problem entirely.

### 3.5 There is no colour-capability detection

Colour is emitted whenever stdout is a tty (`output.rs:1520` and friends);
the code already assumes 256-colour and truecolor (`48;5;235`,
`48;2;22;22;26`) with no probing, and no `NO_COLOR` support. A per-path
palette inherits that assumption, so `NO_COLOR` respect and a setting
(§7) are part of this work, not an afterthought.

---

## 4. Options

### Option A — Vertical rails (continuous box drawing)

`│` down the excursion's column, `├─`/`└─` at the fork.

- **Pro:** the most information; standard tree idiom; shows nesting
  *within* an excursion, which the flattened indent deliberately hides.
- **Con:** false under interleaving (§2). A rail with gaps reads as broken,
  which is worse than no rail.
- **Verdict: reject now.** Revisit only if row order ever becomes
  spine-first (Option C in `tree_indent_refinement.md`), where it becomes
  the obvious right answer.

### Option B — Event markers (box-drawing characters, no rails)

A glyph on the rows that *are* a structural event, with no claim of
continuity:

- `╰` or `├` on the **first visible row of each spur** — "this line of work
  starts here";
- optionally a dim `├` on the fork row itself (a row with ≥2 visible
  children) — "this row is where the split is";
- the existing `*` keeps meaning the active path, `>` selection.

- **Pro:** interleaving-safe (a marker is a fact about one row, not a line
  across rows); cheap (one column); answers question 2 of §1 exactly;
  additive to colour, and it is the non-colour cue that makes colour
  accessible.
- **Con:** the glyph is not positioned at the excursion's indent column if
  it lives in the left gutter, so "which column does this belong to" stays
  the indent's job. That is fine — the marker says *event*, the indent says
  *depth* — but it should be a conscious split, not an accident.
- **Verdict: yes.** This is the "box drawing, but compatible with
  chronology" reading of the original idea.

### Option C — Per-path colour (a left-edge band)

A 1–2 column band at the left of each row, coloured by *which line of work*
the row belongs to; the main line stays neutral.

- **Pro:** identity is exactly what colour is good at, and it survives
  interleaving; the eye tracks "the amber rows" through the interleaving,
  which nothing else does; it is a left-edge element, so it does not fight
  the existing row background or the selection colour.
- **Con:** colour-only encoding fails for colourblind users and in
  monochrome terminals — so it must ship **with** Option B's glyph, never
  alone; a long session with many excursions needs a palette policy (§6);
  it adds 1–2 columns of width.
- **Verdict: yes, paired with B.**

### Option D — Full-row background tint per path

- **Pro:** maximal salience; zero extra columns.
- **Con:** replaces the existing `48;5;235` row background, collides with
  the `48;5;239` selection, and turns a deliberately quiet listing loud.
  `Table::render_row` pads to width and the paint wraps the whole line, so
  a tint would also have to be re-applied per selected row.
- **Verdict: reject.** Option C gets the same information for 2 columns and
  no conflicts.

### Option E — Dotted rail in place of the indent spaces

Replace the leading spaces of a spur row with `·`/`╎` — positional, zero
extra width, and dotted so it does not claim contiguity.

- **Pro:** the rail *is* the indent, so it is correctly positioned and
  survives the fuzzy path (where the leading indent is cut off by
  `extract_snippet` — see `tree_indent_refinement.md` §7.7).
- **Con:** the description would no longer start with whitespace, breaking
  the invariants that `header()` (`trim_start`) and the indent tests rely
  on, and putting rail characters into the fuzzy haystack.
- **Verdict: reject as a description edit;** it is only viable as a
  view-side prefix column, which is Option C's band with a different glyph.

### Comparison

| | identity | split location | interleaving-safe | extra columns | a11y | verdict |
|---|---|---|---|---|---|---|
| A rails | yes | yes | **no** | 2 | good | reject (now) |
| B markers | no | **yes** | yes | 1 | good | **yes** |
| C colour band | **yes** | no | yes | 1–2 | needs B | **yes** |
| D row tint | yes | no | yes | 0 | needs B | reject |
| E dotted rail | weak | no | yes | 0 | good | reject |

B + C is the pair: colour for identity, glyphs for events, indent for
depth. Three mechanisms, three questions, no overlap.

---

## 5. Row anatomy (decided)

The bar is **not** a left-edge column: it is drawn **at the indentation
level of the branch it belongs to** — the main line's bar at the left edge,
each spur's bar at that spur's own indent, with the text immediately
after. All glyphs are box-drawing (no arrows, no block elements), heavy
weight to match the bar:

| role | glyph | codepoint |
|---|---|---|
| bar (user row, or any non-assistant) | `┃` | U+2503 |
| turn ladder, non-final assistant row | `┣` | U+2523 |
| turn ladder, **final** assistant row of a turn | `┗` | U+2517 |
| spur connector: vertical stub + horizontal run | `┗` + `━`×n | U+2517 + U+2501 |

A row is:

```
[indent: depth × SPUR_STEP spaces]
[spur-start connector, only on a spur's first visible row: ┗ at the parent's bar column, ━ filling up to this bar]
[bar cell: 1 column, glyph from the table above, background = branch hue faded by depth]
[1 space, assistant rows only]
[text]
```

and it renders as:

```
┃kick off                main,  user               bar@0
┗ reply 1                main,  assistant (final)  bar@0
┗━━━┃experiment prompt    spur1, user              ┗@0, ━@1..3, bar@4
    ┣ experiment reply    spur1, assistant         bar@4
┗━━━┃a different idea    spur2, user              ┗@0, ━@1..3, bar@4
    ┣ its reply           spur2, assistant         bar@4
    ┗━━━┃a nested thought  spur2.1, user            ┗@4, ━@5..7, bar@8
┃third message           main,  user               bar@0
```

### 5.1 The two connectors

**The turn ladder.** Assistant rows carry `┣` in the bar cell, the last
one in a run `┗`, so a turn's replies stack into a single vertical line
with arms pointing right at their text — and the last reply turns the line
off. The ladder *is* the bar, so the branch indicator and the turn
structure share one column instead of adding a second vertical next to it.
The `┣`/`┗` distinction is local and cheap to compute: an assistant row is
non-final exactly when the next row emitted from it is a visible assistant
row (i.e. its first child's subtree starts with one) — a property of the
node, not a scan of the listing.

**The spur connector.** A spur's first visible row draws `┗` in the
*parent's* bar column with `━` running right into its own bar cell, so the
line visibly comes down from the bar above and turns into the new branch.
This is exactly "a line from the colour bar above, an angle below the
colour bar, a horizontal line touching the new colour bar cell".

It is also structurally honest, which is a happy accident of pre-order
DFS: **a node's first child is always emitted on the row immediately after
it**, so the row above a spur's first row *is* its parent (children are
appended to the index in write order, and a child entry is written after
its parent). The connector therefore always starts from the real parent's
bar. The exception is a spur hanging off an *invisible* entry (a tool-call
segment), where the connector starts from the nearest visible row above
instead — visually harmless, since it is still the row immediately above.

Both connectors use `┗`; they cannot be confused because the turn `┗`
occupies the *bar cell* while the spur `┗` occupies the *indent* and is
always followed by a `━` run.

### 5.2 Column arithmetic

With `SPUR_STEP = 4`, the bar column is `depth × SPUR_STEP` and the text
column is `bar + 1` (user) or `bar + 2` (assistant, because of the 1-space
step after the junction):

| row | bar column | text column |
|---|---|---|
| main, user | 0 | 1 |
| main, assistant | 0 | 2 |
| depth-1, user | 4 | 5 |
| depth-1, assistant | 4 | 6 |
| depth-2, user | 8 | 9 |

Two consequences to accept explicitly:

- **The assistant step shrinks from 2 columns to 1.** The junction's arm is
  now the horizontal step. That is the "exclude the 2-space indent from the
  bar and draw a line instead" trade; a second space restores the old 2
  columns if the offset is wanted back.
- **A spur level costs `SPUR_STEP + 1` = 5 columns**, one more than the
  pure-space indent of the shipped layout. The alternative is to make the
  bar *replace* the last indent column (`bar = depth × STEP − 1` for
  depth ≥ 1), keeping 4 per level at the cost of an uglier formula. Given
  the point of the exercise is the right margin, the 4-column variant is
  worth preferring if the extra column is felt in practice.

### 5.3 Ink and wash

The bar cell is a 1-column paint area with two independent parts:

- **wash** — the background, = branch hue faded by depth (§6.2);
- **ink** — the glyph, drawn in **black** on the wash (luminance-picked to
  white if a hue ever washes out to near-black).

Fading the wash and *not* the ink means the lines stay crisp at every
depth while the colour recedes — which is the whole point of encoding depth
in saturation. It also means the spur-start `┗` inset in the bar cell is
black like every other glyph, as intended.

The main line's bar is washed in the anchor red at full saturation (§6.1)
rather than left unpainted: it is the fixed anchor the whole scheme
references.

### 5.4 The data contract, and why the description must stay bare

The prefix is full of non-whitespace glyphs. If they went into
`Row::description`, three things break:

1. `header()` strips the indent with `trim_start()` (`v25_tree.rs:774`) —
   it would have to start stripping glyphs, and the preview would carry
   bar-coloured noise;
2. the description is the **fuzzy haystack** (`cursor.rs:154`) — the glyphs
   would be searchable and would shift the match indices;
3. `extract_snippet` windows the cell around the match and drops whatever
   precedes it — the prefix is the first thing to go, so a filtered
   listing would lose the very thing the geometry is for.

So the tree emits **fields**, the render layer draws the prefix, and the
**description becomes bare text with no leading spaces**. Net
simplification of the current code: `header()` loses its `trim_start`, the
indent tests assert on `depth` instead of counting spaces, and the prefix
stops being data.

| field | type | meaning |
|---|---|---|
| `depth` | int | spur level; `0` = main line. Replaces `FlatRow.indent`. |
| `branch` | int | colour ordinal for this spur: `0` = main line, else its index in the descending-height ordering of its siblings (§6.1). Assigned by the tree, resolved to a hue by the paint layer. |
| `spur_start` | flag | first **visible** row of a spur — the row that draws the connector and the inset marker. |
| `junction` | int | `0` = none, `1` = `┣`, `2` = `┗` — the turn ladder (§5.1). |
| `role` | str | already emitted; retained for filters and `header()`. |

The tree stays colour-free and glyph-free: it decides *structure* (depth,
which spur, where it starts, where a turn ends) and the paint layer decides
*appearance* (hue, fade, glyph choice, ink colour).

---

## 6. Colour (decided)

### 6.1 Hue: fixed red anchor, rainbow per level, ordered by height

- **Main line (level 0)**: one fixed anchor hue — bright red, fully
  saturated — so "the main flow" is a single constant, whatever the tree
  looks like. (Worth a sanity check at implementation time: red also means
  "error" in many terminals' conventions. If it reads as an alarm, a deep
  crimson anchor keeps the role without the connotation.)
- **Spurs**: rotate through a warm-leaning rainbow so that siblings at the
  same level are maximally distinct. 24-bit, full-colour terminals
  assumed, no 16-colour budget:

```
red (anchor) │ #ffb000 amber  #ff6b35 orange  #ff4d6d rose  #d16bff violet
             │ #6b8cff azure  #2dd4bf teal    #7bd44a green  #c9b800 olive → wrap
```

- **Order**: within each level, siblings are coloured in **descending
  subtree height**, so the branch that holds the margin and the longest
  continuation gets the first colour in the cycle and the shortest
  abandoned spur gets the last. The ordering is already computed — it is
  the spine choice itself (`spine_children` picks `argmax height`), so the
  tree just numbers the non-spine siblings 1, 2, 3… in that same order and
  emits the number as `branch`.
- **Per level, not inherited**: each spur gets its own colour from the
  cycle rather than inheriting its parent's. This maximises local
  contrast (a nested spur is never confusable with the spur it hangs off)
  at the cost of "one hue per line of work" — a level-1 spur and its own
  child never share a hue. The alternative (inherit the top-level
  ancestor's hue, rotate only at level 1) reads as "one colour per
  expedition" instead of "one colour per branch"; the first is more
  discriminable, the second is more stable as depth increases. **The
  first is recommended**, and it is a one-line change in the assignment
  walk if the second is preferred.

Avoid hues near the selection cyan (`1;36`) or the match-highlight magenta
(`1;35`); with truecolor, "bold" is better expressed by setting the RGB
directly than by relying on the terminal's bold-attribute palette effects.

### 6.2 Fade: depth, anchored at full saturation on the main line

Saturation is the second, independent reading of the same cell: hue says
*which* branch, fade says *how far out*. Blend toward the row background
(`48;5;235` = `#262626` for unselected rows):

```
depth  0     1      2      3      4      5+
f      1.00  0.75   0.58   0.45   0.36   0.30
```

The main line is the most saturated thing on screen (it is the anchor);
each level in is 25-30% further toward the background, with a 0.30 floor
so deep spurs stay legible. Fade the wash only — the ink stays black
(§5.3). An explicit RGB blend is preferred over SGR `dim`, whose result
depends on the terminal's own palette handling.

### 6.3 Layout-relative hue (settled)

Hue follows the **layout**: the ordinal comes from the current spine
ordering, so colour, depth and the margin always agree. A branch that
outgrows the flow and takes the margin changes colour when it does — which
is the layout doing its job, and is visible information rather than a
glitch. The alternative (creation-relative: a stable hue per line of work
across the session) was considered and dropped.

---

## 7. Decisions

| # | Question | Decision |
|---|---|---|
| 1 | Rails, markers, or colour? | **Markers + colour, no rails** (§2: rails assert contiguity that interleaving breaks). |
| 2 | Gutter | Owned by the generic render layer; the tree emits fields and does not know the gutter exists (§5.4). |
| 3 | Bar geometry | Aligned to the branch's own indent, text immediately after (§5). |
| 4 | Glyphs | Box-drawing heavy weight: `┃` bar, `┣`/`┗` ladder, `┗━` spur connector. No arrows, no block elements (§5). |
| 5 | Assistant replies | Ladder in the bar cell: `┣` per reply, `┗` on the last of the turn, so a turn's replies share one vertical (§5.1). |
| 6 | Spur marker | A line from the bar above: `┗` under the parent's bar, `━` touching the new bar cell, inset glyph in the bar cell in black (§5.1, §5.3). |
| 7 | Hue | Bright red anchor for the main line; rainbow rotated per level, ordered by descending height; fade = depth; layout-relative (§6). |
| 8 | Opt-out | A `tree_color` setting (`auto | on | off`) + `NO_COLOR`; neither exists today (§3.5). |
| 9 | Defensive fix | Make `extract_snippet` escape-aware even though the recommended path does not reach it (§3.4). |
| 10 | Tunables | The 1-column assistant step, the 5-vs-4 column cost per spur level (§5.2), and the fade curve (§6.2) are all single constants. |

---

## 8. Implementation outline

Split by ownership — the tree side is implementable and testable now; the
paint side lands with the gutter work.

**Ours (`src/v25_tree.rs`, `src/settings.rs`)**

| Step | Change |
|---|---|
| 1 | `FlatRow`: `indent: usize` → `depth: usize`; add `branch: u8`, `spur_start: bool`, `junction: u8`. All derived from the spine set and the heights that `spine_children` already computes — no new traversal. |
| 2 | `flatten_turns`: number the non-spine siblings in descending-height order (the existing tie order) into `branch`; set `spur_start` on the first **visible** row of a spur; set `junction` from the first-child-is-visible-assistant test. |
| 3 | `data()`: stop padding the description; emit `depth` / `branch` / `spur_start` / `junction` as row fields. |
| 4 | `header()`: drop the `trim_start` (the description is bare now). |
| 5 | Tests: indent assertions move from counting spaces to `depth`; add `branch` ordering (tallest spur = 1, main = 0), `spur_start` placement (first *visible* row, skipping tool-result entries), `junction` (`┣` mid-turn, `┗` on the last reply), and stability under navigation. |
| 6 | `tree_color` setting in the `SETTINGS` registry (`src/settings.rs:176-274`). |

**Theirs (the generic gutter / render layer)**

| Step | Change |
|---|---|
| 7 | Draw the prefix `[indent][connector][bar cell][step][text]` from the row fields, after the table cell is built. |
| 8 | Wash = hue(`branch`) faded by `depth`; ink = black, luminance-picked; glyph per §5. |
| 9 | Keep the existing cursor (`>`) and active (`*`) indicators; the bar does not replace them. |

**Shared**

| Step | Change |
|---|---|
| 10 | `extract_snippet` escape-aware (defensive, §3.4). |
| 11 | `doc/slashdot/tree_design.md`: document the prefix contract and the settings key. |
