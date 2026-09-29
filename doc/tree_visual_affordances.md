# `/tree` visual affordances — showing connected-ness across an interleaved listing

## Status

**Exploration. Nothing implemented.** Follow-up to
`doc/tree_indent_refinement.md`, which settled the indentation (spine) rule
and explicitly deferred markers and box-drawing connectors to "one coherent
follow-up design". This is that follow-up's investigation.

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

## 5. Recommended shape

```
 col 0        col 1-2   col 3+
┌──────────┬──────────┬──────────────────────────────┐
│ * > (2ch)│ band(1ch)│ description (indent = spaces)│
└──────────┴──────────┴──────────────────────────────┘
   existing    new       existing
```

or fold the band into the existing 2-column gutter, which is cheaper and
avoids a new column. Both are open (§7).

Rendering sketch (the `╰` case):

```
▎* kick off
▎  reply 1
▎╰ experiment prompt          ▎ = excursion A band
▎  experiment reply
▎  second message             ▎ = main line band (or none)
▎╰ a different idea           ▎ = excursion B band (different hue)
▎  its reply
▎  third message
```

Where the pieces come from:

- **structure** — `spine_children` already produces the spine set, which is
  exactly "is this child a spur". No new traversal is needed to find spur
  first-rows or fork rows.
- **band hue** — a per-row field (`branch`) emitted by
  `TreeListNode::data()` as a *plain* small integer, per §3.1.
- **paint** — a style hook consumed in `view.rs::rebuild_rows`, after
  snippet extraction, so it never meets `extract_snippet` (§3.4).

---

## 6. Hue policy — the one real design fork

**Layout-relative** (hue = the highest-level spur ancestor, derived from
the spine set): main line neutral, every excursion tinted, and the colour
always agrees with the indent. It moves when the layout reflows — a branch
that outgrows the main flow changes colour when it takes the margin.

**Creation-relative** (hue = the fork ordinal in time; main line = the
original chain): a stable label for a line of work across the session. It
can disagree with the indent — a branch that became the main line keeps its
tint while sitting at column 0.

- **Pro layout-relative (recommended):** one mental model — "indent *and*
  colour both say this is an excursion, and this one is further out" — and
  it is free, since the spine set is already computed. The reflow is not a
  bug: it is the layout doing the job we just built, and a branch visibly
  *taking over* the margin is information.
- **Pro creation-relative:** if `/tree` is ever used as a *stable map* of a
  long session ("show me everything from the Postgres migration attempt"),
  a colour that never moves is more useful. Worth revisiting if people
  report losing track of a branch.

Palette: colour by *line of work*, not by level, so all rows of an
excursion share a hue at any depth. Three or four hues cover the realistic
case (one main line plus a few explorations); beyond that, cycle and let the
glyph (§4 B) disambiguate. Mid-tone hues on the existing near-black rows
(`48;5;235`) read well; avoid anything near the selection cyan
(`1;36`) or the match highlight magenta (`1;35`).

---

## 7. Decisions needed

1. **Placement** — a separate 1–2 column band, or fold the colour into the
   existing 2-column gutter (`*`/`>`)? The latter is cheaper; the former
   keeps selection styling and branch styling independent.
2. **Hue policy** — layout-relative (recommended) or creation-relative
   (§6).
3. **Markers** — spur-start glyph only, or spur-start *and* a dim marker on
   the fork row? The second answers "where exactly did it split", the first
   is enough to navigate.
4. **Opt-out** — a `tree_color` setting (`auto | on | off`, default `auto`)
   in the `SETTINGS` registry (`src/settings.rs:176-274`), plus `NO_COLOR`
   respect. §3.5 says neither exists today; colour-only affordances need
   both.
5. **Defensive fix** — make `extract_snippet` escape-aware even though the
   recommended application path does not reach it (§3.4).

## 8. Implementation outline (if approved)

| Step | File | Change |
|---|---|---|
| 1 | `crates/slashdot_menu/src/table.rs` | `extract_snippet` escape-aware (defensive) |
| 2 | `crates/slashdot_menu/src/fields.rs` | `ColumnDef` style hook (tint by field value) + `Value::Str` → cell styling |
| 3 | `crates/slashdot_menu/src/view.rs` | Apply the tint in `rebuild_rows` after snippet extraction; keep it out of the fuzzy haystack |
| 4 | `src/v25_tree.rs` | `FlatRow` gains `branch: u8` + `spur_start: bool`; derive both from `spine_children`; emit as row fields; declare the gutter column |
| 5 | `src/settings.rs` | `tree_color` setting (`auto | on | off`) + `NO_COLOR` |
| 6 | `src/output.rs` | tilde-colour gate shared by the paint paths |
| 7 | `doc/slashdot/tree_design.md` | Document the gutter, the hue policy and the settings key |

Tests: hue/identity invariants (all rows of an excursion share a band;
main line is neutral), marker placement (first visible row of a spur, not
the invisible ones), stability under navigation (the band must not change
when the leaf moves), and the existing truncation tests extended to a
coloured gutter column.
