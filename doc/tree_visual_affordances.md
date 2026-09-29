# `/tree` visual affordances — showing connected-ness across an interleaved listing

## Status

**Implemented** — data and prefix painting both landed in
`src/v25_tree.rs` (§8). Follow-up to `doc/tree_indent_refinement.md`, which
settled the indentation (spine) rule and explicitly deferred markers and
box-drawing connectors to "one coherent follow-up design". This is that
follow-up.

The row anatomy (§5) and the colour scheme (§6) are as reviewed. The
prefix painter is `row_prefix`, the seam the generic gutter work will
absorb; the tree itself only decides *structure* (depth, which spur, where
it starts, where a branch ends). Two ANSI-safety fixes were prerequisites
and are in the same series: `Table::fit` truncating escape-aware
(`8657429`) and `extract_snippet` measuring its window in visible columns
(§3.4).

Colour gating (`tree_color`, `NO_COLOR`) is deliberately **not** done:
every colour in the codebase is static today, so one opt-out in isolation
would not be useful (§7 #8).

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

A vertical rail (`┃` at the excursion's column) asserts *contiguity*: it
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

`┃` down the excursion's column, `┣━`/`┗━` at the fork.

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

- `┗` or `┣` on the **first visible row of each spur** — "this line of work
  starts here";
- optionally a dim `┣` on the fork row itself (a row with ≥2 visible
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
after. All glyphs are box-drawing (no arrows, no block elements), and all
heavy — over a saturated wash the thin strokes (U+2502 and friends) read
as too faint, so the bar is carried by the stroke rather than by the
colour behind it.

| role | glyph | ink |
|---|---|---|
| the line continues | `┃` U+2503 | branch |
| a reply, the line continues | `┣` U+2523 | branch |
| a reply on the branch's last row | `┗` U+2517 | branch |
| a non-reply on the branch's last row (the pipe ends) | `┻` U+253B | branch |
| a spur's first visible row | `┓` U+2513 | branch |
| connector: elbow at the parent's bar + run right | `┗` U+2517 + `━` U+2501 ×n | **neutral** |

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
┃kick off               main,  user               bar@0
┗ reply 1               main,  assistant           bar@0, branch ends
┗━━┓experiment prompt   spur1, user              ┗@0, ━@1..2, bar@3
   ┗ experiment reply   spur1, assistant         bar@3, branch ends
┗━━┓a different idea   spur2, user              ┗@0, ━@1..2, bar@3
   ┣ its reply          spur2, assistant         bar@3
   ┃last question       spur2, user              bar@3
   ┗ last reply         spur2, assistant         bar@3, branch ends
┃third message          main,  user               bar@0
```

(The connector is drawn in neutral dim ink with no wash; the bar cells in
branch ink on a branch wash — in a plain terminal they look identical.)

### 5.1 The two connectors

**The turn ladder.** The vertical runs per **branch**, not per turn: it
stacks a branch's rows into one line with an arm pointing right at each
reply, and turns off (`┗` for a reply, `┻` for anything else) on the
branch's *last* row. A branch that forks again in a new turn still shows
one continuous pipe, which is what makes the two connectors safe to read
together. The distinction is local and cheap — it is the `is_terminal`
computation `visible_terminals` already does.

**The spur connector.** A spur's first visible row draws `┗` in the
*parent's* bar column with `━` running right into its own bar cell, so the
line visibly comes down from the bar above and turns into the new branch.
This is exactly "a line from the colour bar above, an angle below the
colour bar, a horizontal line touching the new colour bar cell".

**Keeping two adjacent sub-trees from looking piped together.** This is
the hazard the review flagged, and it is real: two sibling spurs are
listed one after the other, each opening with a corner in the *same*
column, so two up-strokes stack and the eye can read a single vertical
running from one excursion into the next — i.e. spur A continuing into spur
B, when they are in fact siblings that both branch off the same parent.

One rule kills it, and it is free — it was originally two (stroke weight
and ink), but with every glyph drawn heavy the weight half went away:

**The connector is drawn in *neutral dim* ink, never a branch hue.** Colour
already means "this is branch X"; the connector is a structural annotation,
so leaving it uncoloured — and giving it no wash of its own — keeps the two
from blending into a single pipe when a connector sits directly above a
sibling's bar. A stacked pair of corners then reads as "two branches
leaving the same parent", which is exactly what it is.

It is also structurally honest, which is a happy accident of pre-order
DFS: **a node's first child is always emitted on the row immediately after
it**, so the row above a spur's first row *is* its parent (children are
appended to the index in write order, and a child entry is written after
its parent). The connector therefore always starts from the real parent's
bar. The exception is a spur hanging off an *invisible* entry (a tool-call
segment), where the connector starts from the nearest visible row above
instead — visually harmless, since it is still the row immediately above.

The two connectors cannot be confused: the turn-off `┗` occupies the
*bar cell*, while the spur `┗` occupies the *parent's* column and is
always followed by a `━` run into a `┓`.

### 5.2 Column arithmetic

The bar **replaces the last column of its branch's indent block**, so the
text column is exactly where the shipped layout puts it — the prefix costs
no width at all:

```
text_col(d) = d × SPUR_STEP          bar_col(d) = d × SPUR_STEP − 1  (d ≥ 1)
text_col(0) = 1                      bar_col(0) = 0
```

| row | bar column | text column |
|---|---|---|
| main, user | 0 | 1 |
| main, assistant | 0 | 2 |
| depth-1, user | 3 | 4 |
| depth-1, assistant | 3 | 5 |
| depth-2, user | 7 | 8 |

With `SPUR_STEP = 4` that is **4 columns per spur level, unchanged from the
layout already shipped** — the bar is drawn *in* the indent rather than
added to it. (The alternative, a bar per level *after* the indent, would
cost 5 columns per level; not worth it, especially now that the main line
sits at the margin and deep spurs are the only thing pushing right.)

The assistant step is **1 column** (confirmed): the junction's arm is the
horizontal step, so `┃` + text for a user row and `┣` + one space + text
for a reply.

### 5.3 Ink and wash

The bar cell is a 1-column paint area with two independent parts:

- **wash** — the background, = branch hue faded by depth (§6.2);
- **ink** — the glyph, drawn in **black** on the wash (luminance-picked to
  white if a hue ever washes out to near-black).

The connector of §5.1 is the exception: no wash, and a dim neutral ink.

Fading the wash and *not* the ink means the lines stay crisp at every
depth while the colour recedes — which is the whole point of encoding depth
in saturation. The spur-start `┓` in the bar cell is black like every
other glyph, as intended.

The main line's bar is washed in the anchor orange at full saturation
(§6.1) rather than left unpainted: it is the fixed anchor the whole scheme
references.

### 5.4 The data contract

The tree decides *structure*; the prefix painter decides *appearance*. The
split matters because the description is not only display text:

1. it is the **fuzzy haystack** (`cursor.rs:154` indexes
   `"{id} {description}"`) — escape bytes and box-drawing glyphs in it would
   be searchable and would shift the match indices used for highlighting;
2. it is the `header()` source, which re-wraps it to the terminal width.

So the row carries the *text* in one place and the *prefix* in another:

| field | type | meaning |
|---|---|---|
| `text` | str | the bare entry text — the haystack, the header, the filter index. |
| `depth` | int | spur level; `0` = main line. Replaces `FlatRow.indent`. |
| `branch` | int | colour ordinal: `0` = main line, else a **global** per-spur counter handed out in descending-height order (§8.1). |
| `spur_start` | flag | first **visible** row of a spur — the row that draws the connector and the `┓`. |
| `role` | str | already emitted; retained for filters and `header()`. |

The listing cell is `prefix + text`; `header()` reads `text`. `depth`,
`branch` and `spur_start` are also declared in `tree_fields()`, so they are
filterable (`.depth:2` finds every excursion two levels out) for free, the
same way `tags:leaf` already is.

`extract_snippet` is escape-aware (§3.4), so a filtered listing still
windows correctly around a match in a cell that carries a wash.

---

## 6. Colour (decided)

### 6.1 Hue: fixed orange anchor, rainbow per spur, ordered by height

- **Main line (level 0)**: one fixed anchor hue — **warm orange**,
  fully saturated — so "the main flow" is a constant whatever the tree
  looks like. Orange rather than red: red reads as an error in most
  terminal conventions, orange carries the same warmth without the alarm.
  Suggested value `#f97316`; the exact constant wants a look on a real
  terminal.
- **Spurs**: rotate through a rainbow that *starts just after the anchor
  and warms back up at the end*, so the common two-or-three-spur case gets
  warm, clearly-distinct hues, and the warm end of the wheel is reached
  last. 24-bit, full-colour terminals assumed, no 16-colour budget:

```
anchor #f97316 orange
   1 #f472b6 rose      2 #a78bfa violet   3 #60a5fa azure   4 #22d3ee cyan
   5 #2dd4bf teal      6 #6ee7a8 green    7 #a3e635 lime    8 #fbbf24 amber → wrap
```

- **Order**: at each fork, spurs take colours in **descending subtree
  height**, so the longest abandoned branch gets the first colour of the
  cycle. The ordering is already computed — it *is* the spine choice
  (`ranked_children` sorts by `(height, write order)`), so the tree numbers
  the spurs in that order and emits the number as `branch` (§8.1 on why the
  counter is global).
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
| 4 | Glyphs | Box-drawing only, no arrows, no block elements, and **all heavy** — thin strokes are too faint over a saturated wash. `┃` on the line, `┓` on a spur's first row. |
| 5 | Assistant replies | `┣` per reply, `┗` on the last of a turn; the vertical is per **branch**, so the ladder turns off (or the pipe ends with `┻`) on the branch's last row, not at every turn boundary (§5.1). |
| 6 | Spur marker | A line from the bar above: `┗` under the parent's bar, `━` touching the new bar cell, in neutral ink with no wash; the bar cell's own glyph stays in branch ink (§5.1, §5.3). |
| 7 | Hue | **Warm orange** anchor for the main line; rainbow rotated per level starting after the anchor, ordered by descending height; fade = depth; layout-relative (§6). |
| 8 | Opt-out | A `tree_color` setting (`auto | on | off`) + `NO_COLOR`; neither exists today (§3.5). |
| 9 | Defensive fix | Make `extract_snippet` escape-aware even though the recommended path does not reach it (§3.4). |
| 10 | Assistant step | **1 column** — the junction's arm is the step (§5.2). |
| 11 | Spur-level width | **4 columns per level, unchanged from the shipped layout**: the bar replaces the last indent column rather than being added after it (§5.2). |
| 12 | Tunables | The fade curve (§6.2), the palette constants and the connector's dim ink are single constants; the exact anchor orange wants a look on a real terminal (§6.1). |

---

## 8. Implementation — done (data + paint, in `v25_tree.rs`)

The gutter is generic and owned elsewhere, but the prefix cannot ship
without *something* painting it, and a data-only change would leave `/tree`
showing a flat list. So both sides landed here, with the paint isolated in
one function — `row_prefix` is the seam the generic gutter work absorbs.

**`src/v25_tree.rs`**

| Step | Change |
|---|---|
| 1 | `ranked_children()` replaces `spine_children()`: per node, children ranked by `(height, write order)`. Rank 0 *is* the spine, so there is no separate spine set. Same iterative post-order, same visible projection. |
| 2 | `flatten_turns` carries `(node, depth, branch, spur_start)` on the DFS stack and hands out hues from a **global** spur counter (see §8.1). Spine children inherit the parent's branch, so a spur's replies share its hue. |
| 3 | `row_prefix(depth, branch, bar, assistant)` builds connector + bar cell + reply step; `Bar` is the glyph enum of §5.1; the palette, `FADE` and `blend` are constants. |
| 4 | `FlatRow`: `indent` → `depth`, plus `branch` and `spur_start`. `data()` emits them as row fields (`text` carries the bare text), and `header()` reads `text` instead of `trim_start`-ing the description. |
| 5 | `tree_fields()` exposes `depth`, `branch` and `spur_start` as filterable fields. |
| 6 | Tests: every indent assertion moved from counting spaces to `depth` plus the visible prefix; new tests for branch identity, `spur_start` placement (including through invisible entries), the fade/ink split, and the connector's weight and ink. |

**`crates/slashdot_menu/src/table.rs`**

| Step | Change |
|---|---|
| 7 | `extract_snippet` is escape-aware (§3.4): escape characters occupy characters but no visible columns, so the snippet window is measured in columns and the escapes ride through the slice. Needed now that cells carry SGR. |

Not done, deliberately: the `tree_color` setting and `NO_COLOR` (§7 #8).
Every colour in the codebase is currently static, so a single opt-out in
isolation is not useful — colour gating (and the `*`/`>` cursor indicators)
stays as it was, and the whole affordance follows the rest of the styling
when that is revisited.

### 8.1 One correction the implementation forced

The `branch` field was specified as *the spur's index among its siblings*.
That is wrong for the purpose: a per-parent ordinal gives every
single-spur branch the number 1, so two different level-1 excursions get
the *same* colour and the encoding says nothing about identity. It is a
**global** counter now — one increment per spur, handed out in the ranked
(descending-height) order at each fork, starting at 1 because 0 is the
anchor. A spine inside a spur inherits its parent's branch (it is a
continuation, not a new line of work), which is why "is a spur" is decided
by the *rank*, never by the branch value.

Two more findings from building it:

- The `spur_start` marker has to be decided by **rank**, not by the branch
  value, for the same reason: a spine inside a spur has a non-zero branch
  but is not a spur root, and must not open a second connector.
- The marker must travel to the first **visible** row of a spur, which
  means propagating it through invisible nodes to the first child — a
  spur whose root is a tool-call segment draws its connector on the first
  row a reader can actually see.
