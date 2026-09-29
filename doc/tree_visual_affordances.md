# `/tree` visual affordances — showing connected-ness across an interleaved listing

## Status

**Geometry and colour decided; not yet implemented.** Follow-up to
`doc/tree_indent_refinement.md`, which settled the indentation (spine) rule
and explicitly deferred markers and box-drawing connectors to "one coherent
follow-up design". This is that follow-up.

The row anatomy (§5) and the colour scheme (§6) are settled in review; the
split into *data* (the tree emits fields) and *paint* (the generic gutter
layer draws the prefix) is the ownership model that falls out of the gutter
being generic. One open question remains — hue policy (§6.3).

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
level of the branch it belongs to**, so the main line's bar sits at the
left edge and every spur's bar sits at that spur's own indent, immediately
followed by the text. The assistant "response" slot sits *between* the bar
and the text, and is deliberately **not** counted inside the bar's indent.

A row is therefore:

```
[indent: depth × SPUR_STEP columns][bar: 1 col][turn slot: 0 or 2 cols][text]
```

with the bar occupying the **last** column of its branch's indent block,
so a spur still costs exactly `SPUR_STEP` and the text column does not
move from today's layout:

```
▌kick off               main,  user        bar@0, no slot
▌│ reply 1              main,  assistant   bar@0, slot '│ '
   ▌experiment prompt   spur1, user        bar@3, no slot
   ▌│ experiment reply   spur1, assistant   bar@3, slot '│ '
▌second message         main,  user        bar@0, no slot
▌│ reply 2              main,  assistant   bar@0, slot '│ '
   ╰a different idea     spur2, user        bar@3 + spur-start marker
   ▌│ its reply          spur2, assistant   bar@3, slot '│ '
▌third message          main,  user        bar@0, no slot
```

`╰` replaces the bar glyph on the spur's first row; the cell behind it is
still painted with the branch hue.

Column arithmetic, for `SPUR_STEP = 4`:

| row | bar column | text column |
|---|---|---|
| main, user | 0 | 1 |
| main, assistant | 0 | 3 |
| depth-1, user | 3 | 4 |
| depth-1, assistant | 3 | 6 |
| depth-2, user | 7 | 8 |

The assistant slot keeps today's "+2 relative to the user row above"
meaning, measured from *after* the bar rather than from the margin — which
is exactly "exclude the colour bar indent from the user/assistant 2-space
indent". The slot replaces the blank two spaces with a drawn line that
reads as "attached to the bar above", i.e. "this is a response within that
branch".

### 5.1 What the bar cell contains

The cell is a 1-column paint area, and every row has one so the geometry
is uniform:

- **main line** — drawn in the default (or dimmed) foreground, with **no
  hue fill**: colour is reserved for excursions, matching "the main line is
  the spine";
- **spur rows** — background = the branch hue, faded by depth (§6.2);
  glyph = `▌` (or empty, if a full-bleed cell reads better);
- **spur's first visible row** — the same background, with the marker glyph
  inset **in black** (or white, chosen by the faded background's
  luminance — a deep spur fades toward the row background, and black on a
  nearly-black fill is unreadable).

### 5.2 The data contract, and why the description must stay bare

This geometry puts non-whitespace glyphs where the indent used to be. If
they went into `Row::description`, three things break:

1. `header()` strips the indent with `trim_start()` (`v25_tree.rs:774`) —
   it would have to start stripping glyphs, and the preview would carry
   bar-coloured noise;
2. the description is the **fuzzy haystack** (`cursor.rs:154`) — the bar
   and line glyphs would be searchable and would shift the match indices;
3. `extract_snippet` windows the cell around the match and drops whatever
   precedes it — the prefix is the first thing to go, so a filtered
   listing would lose the very thing the geometry is for.

So the tree emits **fields**, and the prefix is drawn by the render layer:

| field | type | meaning |
|---|---|---|
| `depth` | int | spur level; replaces today's `FlatRow.indent` |
| `branch` | int | hue index; `0` = main line, else the line-of-work ordinal |
| `spur_start` | flag | first *visible* row of a spur (the marker row) |
| `role` | str | already emitted — the render layer needs it to decide the slot |

and the **description becomes bare text with no leading spaces**. That is
a net simplification of the current code: `header()` no longer needs
`trim_start`, the indent tests assert on `depth` instead of counting
leading spaces, and the prefix is a rendering concern rather than data.

The generic gutter work owns the painting (it is a generic concern, and
the tree should not know it is there). The tree's remaining job is the
four fields above and the tests that pin their values.

---

## 6. Colour: hue and fade (decided)

### 6.1 Hue = identity, at a single level

Hue identifies the *line of work*, not the depth: all rows of an excursion
share a hue at any depth. Sibling excursions **at the same depth** get
different hues, rotating through a warm-leaning rainbow (24-bit; full-colour
terminals assumed, no 16-colour budget):

```
1 #ffb000 amber    2 #ff6b35 orange   3 #ff4d6d rose    4 #d16bff violet
5 #6b8cff azure    6 #2dd4bf teal     7 #7bd44a green    8 #c9b800 olive  → wrap
```

Warm first (1-3), cool through the middle, warm again at the end; adjacent
assignments are far apart in the wheel. Avoid anything near the selection
cyan (`1;36`) or the match-highlight magenta (`1;35`).

With truecolor, "bold elementary colours" is better expressed by setting the
RGB directly than by relying on the bold attribute's palette effects.

### 6.2 Fade = depth

Brightness is a second, independent reading of the same cell: hue says
*which* branch, fade says *how far out*. The fade is a redundancy cue
(indent already encodes depth), which is why it can be gentle. Blend the
hue toward the row background (`48;5;235` = `#262626` for unselected rows):

```
f(1) = 1.00   f(2) = 0.78   f(3) = 0.60   f(4) = 0.46   f(5) = 0.36   f(6+) = 0.30
```

A 0.30 floor keeps deep excursions legible while making them recessive.
An explicit RGB blend is preferred over SGR `dim`, whose result depends on
the terminal's own palette handling.

### 6.3 One decision left: layout-relative or creation-relative

**Layout-relative** (hue = the highest-level spur ancestor, from the spine
set): colour and indent always agree — one mental model, and free, since
the spine set is already computed. It moves when the layout reflows.

**Creation-relative** (hue = the fork ordinal in time; main line = the
original chain): a stable label for a line of work, but a branch that grew
into the main line keeps its tint while sitting at the left edge.

Recommended: **layout-relative** — the reflow *is* the feature just
implemented, and a branch visibly taking over the margin is information.
Creation-relative becomes the better choice if `/tree` is ever used as a
stable map of a long session.

---

## 7. Decisions

| # | Question | Decision |
|---|---|---|
| 1 | Rails, markers, or colour? | **Markers + colour, no rails** (§2: rails assert contiguity that interleaving breaks). |
| 2 | Gutter | Owned by the generic render layer; the tree emits fields and does not know the gutter exists (§5.2). |
| 3 | Bar geometry | Aligned to the branch's own indent, last column of its indent block; text column unchanged (§5). |
| 4 | Assistant indent | Kept as +2, drawn as a line in the slot, measured from after the bar (§5). |
| 5 | Bar width / fade / hue | 1 column, hue = line of work, fade = depth, warm-leaning rainbow (§6). |
| 6 | Markers | Spur start only, inset in the bar cell, in black (luminance-picked) (§5.1). |
| 7 | Hue policy | **Open** — layout-relative (recommended) or creation-relative (§6.3). |
| 8 | Opt-out | A `tree_color` setting (`auto | on | off`) + `NO_COLOR`; neither exists today (§3.5). |
| 9 | Defensive fix | Make `extract_snippet` escape-aware even though the recommended path does not reach it (§3.4). |

---

## 8. Implementation outline

Split by ownership — the tree side is implementable and testable now; the
paint side lands with the gutter work.

**Ours (`src/v25_tree.rs`, `src/settings.rs`)**

| Step | Change |
|---|---|
| 1 | `FlatRow`: `indent: usize` → `depth: usize`; add `branch: u8`, `spur_start: bool`. Derive all three from the spine set that `spine_children` already produces (no new traversal). |
| 2 | `data()`: stop padding the description; emit `depth` / `branch` / `spur_start` as row fields. |
| 3 | `header()`: drop the `trim_start` (the description is bare now). |
| 4 | Tests: rewrite the indent assertions against `depth`; add `branch` identity (all rows of an excursion agree, main line is `0`), `spur_start` placement (first *visible* row, skipping invisible entries), and stability under navigation. |
| 5 | `tree_color` setting in the `SETTINGS` registry (`src/settings.rs:176-274`). |

**Theirs (the generic gutter / render layer)**

| Step | Change |
|---|---|
| 6 | Draw the prefix `[indent][bar][turn slot]` from the row fields, after the table cell is built. |
| 7 | Apply hue + fade + marker per §6; pick the marker fg by the faded background's luminance. |
| 8 | Keep the existing cursor (`>`) and active (`*`) indicators; the bar does not replace them. |

**Shared**

| Step | Change |
|---|---|
| 9 | `extract_snippet` escape-aware (defensive, §3.4). |
| 10 | `doc/slashdot/tree_design.md`: document the prefix contract and the settings key. |
