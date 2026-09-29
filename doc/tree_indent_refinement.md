# Tree indentation refinement — spine-first layout

## Status

**Implemented** (Option A1, see §10). `spine_children()` + `SPUR_STEP` in
`src/v25_tree.rs`; `flatten_turns` no longer looks at child counts. Full
suite: **745 lib tests + all integration tests pass**, no new clippy
warnings. `doc/slashdot/tree_design.md` §Indentation rewritten to match.

The one behaviour change to an existing expectation is
`tree_with_model_changes_indent_render`, which pinned the old rule by name
(see §6). Nine tests were added for the rules decided here.

This document is the design record — the investigation, the options
considered, and the measurements behind each decision. It is kept as the
"why"; §10 holds the final key order.

The change requested was:

> prioritise indenting the *longest path to root* the least — treat the
> longest sub-tree as the "root" session, then do the same recursively with
> sub-trees, so short experiment spurs end up furthest right while the main
> flow stays at the left margin.

The layout as implemented, in full:

```
spine(v)        = the child c maximising (height(c), write_index(c))
indent(v)       = number of spur edges on the path from the root
row_column(v)   = indent(v) * SPUR_STEP + (2 if assistant else 0)
```

All layouts quoted below were produced by running the real code (the
measurement harness is in §9); the "before" ones are the pre-change rule,
the "after" ones are what ships.

Still open, and deliberately not implemented: markers and box-drawing
connectors (Option E/F), and whether `active` should be promoted *ahead*
of `recency` for exact height ties (§10) — that one is a single boolean
in the key tuple.

---

## 1. The problem

`/tree` indents sub-trees by nesting depth, and every fork pushes the
*whole continuation* to the right — including the branch the user is
actually on. There is no notion of a "main line": after N forks the
conversation itself has drifted N × 4 columns from the left margin, and the
margin is exactly where the useful text is.

Measured on a small session (main flow `u1 → a1 → u2 → a2 → u3 → a3`, one
2-row experiment spur off `a1`, one 2-row side quest off `u2`):

```
indent=0 kick off
indent=2 reply 1
indent=4 experiment prompt      <- spur
indent=6 experiment reply       <- spur
indent=4 second message         <- main flow, already 4 in
indent=8 side quest             <- spur
indent=10 side reply            <- spur
indent=10 reply 2
indent=8 third message
indent=10 reply 3
```

Two things are wrong with this:

1. **The main flow drifts.** The longest path in the tree is
   `kick off → reply 1 → second message → reply 2 → third message →
   reply 3`; it should occupy column 0, and it does not.
2. **A spur is not cheaper than the flow.** `experiment prompt` (2 rows)
   costs the same 4 columns as the 6-row main flow beneath it. Nesting depth
   is a proxy for "how deep is this conversation", and a 500-turn linear
   session with 3 forks looks identical to a 5-turn session with 3 forks.

The requested rule inverts the proxy: indentation measures **distance from
the main line**, not distance from the root.

---

## 2. Current implementation (facts)

Everything in this section is the code as it stands.

| Piece | Location |
|---|---|
| Row building | `src/v25_tree.rs:340` `flatten_turns` |
| Fork indent rule | `src/v25_tree.rs:361-366` |
| Row indent rule (turn) | `src/v25_tree.rs:377-381` |
| Invisible-entry descent | `src/v25_tree.rs:368-374` |
| Indent → leading spaces | `src/v25_tree.rs:627` `format!("{:indent$}{}", "", r.desc, …)` |
| Tree snapshot source | `src/agent.rs:2144` → `AppState::raw_tree` (`src/app_state.rs:361`) |
| Sibling order | insertion order from `SessionIndex::children` (`src/session/types.rs:537` `build_subtree`); only the **root** vec is re-sorted, by active path (`build_tree`, `src/session/types.rs:512`) |
| Caching | `TreeListNode::data()` caches on `Arc::ptr_eq` of the tree + leaf id |

The two rules, verbatim:

```rust
// src/v25_tree.rs:361
let is_branch = node.children.len() > 1;
let child_indent = if is_branch { cur_indent + 4 } else { cur_indent };

// src/v25_tree.rs:377
let row_indent = if is_assistant(&node.entry) { cur_indent + 2 } else { cur_indent };
```

So `cur_indent` is a scalar pushed down the DFS, and the row's own column is
`cur_indent` (+2 for an assistant row). Two latent consequences worth
knowing before changing anything:

- **A fork costs 4 columns to *both* sides of the fork.** There is no
  "which child is the continuation" concept.
- **`node.children.len() > 1` counts invisible children.** A parent whose
  children are `[invisible tool-call assistant, visible user message]`
  counts as a fork and indents the user message by 4 even though there is
  no fork the user can see. Any replacement rule should measure the
  *visible* projection, not raw nodes.

Rendering side: the indent is baked into the row `description` as leading
spaces, the column is `wrap(false)` (`tree_listing_columns`,
`src/v25_tree.rs:479`), and `Table::fit` (`crates/slashdot_menu/src/table.rs:380`)
truncates with an ellipsis from the left, keeping the leading spaces. Indent
therefore eats directly out of the text budget, and fuzzy-match snippet
extraction (`extract_snippet`, `table.rs:419`) runs over the *padded* cell,
so a deep spur also gets a narrower snippet window. Lower indents are
strictly better on this path — another argument for the change.

---

## 3. Terminology and the shape of the fix

- **main line / spine** — the single path through the tree that stays at the
  left margin. It is a *chain*, not a subtree: at every fork, exactly one
  child continues the chain and the others hang off it.
- **spur** — any child that is not its parent's spine. Spurs get one extra
  indentation step; everything below a spur stays at that level unless that
  spur forks again.
- **level** — the number of spur edges on the path from the root, i.e. the
  column the subtree is drawn at.
- **step** — columns added per spur edge (today: 4, the old branch step).

The whole change is one line of policy:

```
old:  every child of a forking node        → parent column + STEP
new:  the spine child of a forking node   → parent column + 0
      every other child                   → parent column + STEP
```

The question that remains is **how the spine is chosen**. That is the rest
of the document.

---

## 4. What "longest" means — and why the answer is cheap

Two metrics are defensible, and they induce *different* local rules. Both
are greedy and both are **provably optimal** for their objective, so this
is not a heuristic trade-off — it is a choice of objective.

**First, a clarification that matters** (it came up when this was reviewed):
*height vs size has nothing to do with visibility.* There are three
independent axes and they are easy to conflate:

| Axis | Question | Where it is handled |
|---|---|---|
| **visibility** | Which entries produce a row at all? | `entry_display_label` — tool results, tool-call-only assistant segments, `SessionInfo`/`Leaf` are **invisible**; user text, assistant text, `[MODEL]`, `[THINK]`, `[SUMMARY]`, `[COMPACT]` are visible. |
| **height** | How *deep* is the subtree? (longest chain) | §4.1 |
| **size** | How *many rows* does the subtree hold? (total mass) | §4.2 |

Both height and size are measured **over the visible projection** — a spur
made only of tool results has height 0 and size 0, and never out-ranks a
real conversation. That is a separate decision (and the one that fixes the
`children.len() > 1` artefact from §2), and it applies identically no matter
which of the two metrics ends up primary.

### 4.1 Minimise the *deepest* indentation → rank children by height

`height(v)` = number of visible rows on the longest downward path starting
at `v` (0 for an invisible node whose subtree is empty of visible rows).

At a node `v` with children `c₁…c_k`, if `c*` is the spine then the deepest
column inside `v`'s subtree is
`max(level(c*), 1 + max_{c ≠ c*} level(c))`, i.e. with the spine at level 0:
`max(H(c*), 1 + max_{c≠c*} H(c))` where `H` is the subtree's own max level.
Picking `c* = argmax H` yields `max(H_max, 1 + H_second)`. Picking anything
else yields `1 + H_max ≥ 1 + H_second ≥ max(H_max, 1 + H_second)`. So
**argmax-height is optimal by induction**, and the minimum achievable
deepest column is produced by it.

### 4.2 Minimise *total* indentation → rank children by size

`size(v)` = number of visible rows in `v`'s subtree. Total cost is
`Σ_rows level(row)`; at `v` the incremental cost of a child is
`level_offset(c) · size(c)`, and only the spine has offset 0. So
`Σ_{c≠c*} size(c)` is minimised by `c* = argmax size`. Also optimal by
induction.

#### What a "row" is here (the two are *not* the same unit, and that is the whole point)

Both metrics are counted in **listing rows** — the `FlatRow`s
`flatten_turns` emits and that `DataResponse.rows` carries
(`src/v25_tree.rs:204`):

- one **row = one visible entry** = one line in the listing. A user
  message, an assistant text message, a `[MODEL]`/`[THINK]`/`[SUMMARY]`/
  `[COMPACT]` marker row: one entry, one row.
- **not** terminal lines: the tree column is `wrap(false)`, so a row whose
  `description` holds newlines (compaction summaries, multi-line messages)
  is flattened to one displayed line (`Table::render`, `table.rs:165`).
  Fuzzy rank and `header()` see the raw multi-line text; the *listing* does
  not.
- **not** raw entries: tool results, tool-call-only assistant segments,
  `SessionInfo` and `Leaf` entries are entries in the JSONL but produce no
  row, so they are not counted by either metric.

Height and size differ because one is a **max** over downward paths and the
other is a **sum** over all nodes:

```
h(v) = vis(v) + max over children c of h(c)        (0 if no children)
s(v) = vis(v) + Σ      over children c of s(c)
```

For any subtree that does not fork — a plain continuation — the two are
**identical**, which is exactly why they agree in almost every real
session. They part ways the first time a subtree branches: the bush in
§4.3 has `s = 7` (explore, try 1, tried 1, try 2, tried 2, try 3, tried 3)
but `h = 3` (explore → try 1 → tried 1), because two of its three
explorations are counted in the sum and ignored by the max.

### 4.3 Where they disagree (measured)

A **chain** is deep and narrow; a **bush** is shallow and wide. When the
two metrics are compared on siblings of different shape they can pick
different spines. Session: after `reply 1`, one branch continues as a
5-row chain (`main 1 … main 3`), the other is a 7-row bush of three
2-row experiments (`explore → try 1/2/3`).

```
=== height-primary (longest path leftmost) ===   === size-primary (most rows leftmost) ===
indent=0 kick off                               indent=0 kick off
indent=2 reply 1                                indent=2 reply 1
indent=0 main 1          <- chain, at margin     indent=4 main 1        <- chain pushed right
indent=2 main reply 1                           indent=6 main reply 1
indent=0 main 2                                indent=4 main 2
indent=2 main reply 2                           indent=6 main reply 2
indent=0 main 3                                indent=4 main 3
indent=4 explore          <- bush, stepped      indent=0 explore        <- bush at margin
indent=4 try 1                                 indent=0 try 1
indent=6 tried 1                                indent=2 tried 1
indent=8 try 2                                 indent=4 try 2
indent=10 tried 2                               indent=6 tried 2
indent=8 try 3                                 indent=4 try 3
indent=10 tried 3                               indent=6 tried 3
```

Both layouts have the same maximum indent (10). The difference is *which*
side of the fork is the margin — and with size-primary the branch going
**right** is the *deeper* one, which is the exact opposite of the property
the change is meant to establish:

> spurs and paths get shorter the further right we go

That property is guaranteed by height-primary and **not** guaranteed by
size-primary: with height-primary, at every fork the left branch is at
least as deep as any right branch (a consequence of §4.1), so the reading
"indentation grows ⇒ the conversation gets shorter ⇒ the right side is
the excursion" always holds.

Note also that size-primary is *not* what fixes the "long linear session
drifts right" complaint either — it merely re-attributes the drift to
whichever branch has more rows. The complaint in §1 is about a long path
being pushed right, and only the height objective addresses the path.

Real sessions rarely produce this shape (a fork is usually "short
abandoned turn" vs "long continued conversation", where both metrics
agree), which is why this is a cheap decision: the disagreement needs an
unusually bushy side branch to show up at all.

### 4.4 The secondary key: `size` vs `recency` (decided: `recency`)

`height` primary is **decided**. The secondary key was debated between
`size` (most rows wins) and `recency` (newest branch wins).
**`recency` was chosen**, for reasons that are worth recording next to the
objections, because three of the four objections below turned out not to
apply:

**Objection 1 — loses the min-total-indent guarantee. Holds, bounded.**
`(height, size)` was optimal for *both* objectives: §4.1 makes
height-tied siblings free choices for the max objective, and among those,
argmax-size is the total-optimal one. Demoting `size` gives that up.
The cost is bounded and local: only at forks whose children are exactly
height-tied, and at most `Δsize × SPUR_STEP` columns at each such fork.

**Objection 2 — a smaller branch can hold the margin. Holds, mild.**
At an exact height tie, `recency` ignores size, so a 6-row branch can
hold the margin while an 8-row sibling steps right. This is a weak cousin
of the §4.3 inversion: it needs an exact height tie, and the effect is
one step on a subset of forks.

**Objection 3 — `recency` swallows the `active` key. Holds, and it is a
design signal, not a bug.** `recency` is keyed on the child's position in
`children`, and positions are distinct — so once height and recency are
compared, the key is a *total order* and `active` can never fire. The
honest options are therefore:

- `(height, recency)` — literal "longest path, else newest";
- `(height, active, recency)` — "longest path, else the branch you are
  on, else the newest".

The second is Option B restricted to exact height ties, which is a much
weaker form of it than the full-priority version rejected in §5
(Option B): it can only ever move a spur by one step, and only where the
two branches are the same depth. If the intent behind "newest" is really
"the branch I care about stays at the margin", this ordering expresses it
directly.

**Objection 4 — churn / flapping. Does not apply.** `recency` is *more*
stable than `size` with respect to navigation: a `switch` that writes
nothing does not change which branch was last written, so the layout does
not move when you navigate. `size`-first and `active`-first both move.
Against writes, the two are equivalent — one new row in a branch breaks
the height tie in the same instant it breaks the size tie. The only
divergence is the height-tied/size-*un*equal case, where `size` and
`active` flip while `recency` need not.

**No cost either way.** `recency` is free: `SessionIndex::children`
vectors are built in write order (`index.children.entry(pid).or_default()
.push(id)`, replayed from JSONL in file order), so "newest child" ==
"later index in the slice" — no timestamp parsing or comparison, and
distinct indexes make the key total.

**Recommendation: `(height, recency)`** if "newest" is meant literally,
**`(height, active, recency)`** if the intent is reader-oriented. Both are
decided-away-from-`size` and both keep the §4.3 shortening guarantee; the
only cost either pays is Objections 1 and 2, which are bounded.

Everything is computed in **one iterative post-order pass**, mirroring the
existing `visible_terminals` (`src/v25_tree.rs:303`): heights bottom-up,
one `HashSet<&str>` of spine child IDs out.

**There is no cost difference between the height and size metrics**, either.
Both are O(1) integer work per node inside the same single pass and both
need the same map. Choosing a primary key never changes what is computed
— sizes stay in the pass if the tie-break might need them, and drop out
entirely if the key order is `(height, recency)`.

---

## 5. Options

### Option A — Greedy spine (recommended)

Pick, for every node, the child that continues the main line; the others
become spurs.

* **A1 — `(height, recency)`** ← **chosen** (see §4.4): longest path, else
  the newest branch. Total order, so nothing else is needed.
* **A2 — `(height, active, recency)`** — the reader-oriented variant:
  longest path, else the branch you are on, else the newest.
* **A3 — `(height, size, recency, active)`** — the earlier recommendation;
  keeps the min-total-indent guarantee, demoted in favour of `recency`.

Tie-break mechanics (in order, first difference wins):

1. `height(child)` — deepest visible path. This is the objective and it is
   what guarantees "spurs get shorter the further right you go" (§4.3);
2. `active(child)` — `active_path.contains(child.id)`, the set
   `flatten_turns` already receives (A2 only); or
3. `recency(child)` — the later child in the slice is the newer branch
   (free: child order == write order, §4.4).

Nothing after step 1 can change the deepest-column objective; they only
resolve exact height ties (§7.4).

Properties:

- A linear session (no forks) renders **exactly as today** — every node is
  its parent's only child, hence the spine, hence level 0 forever. Indent
  becomes a function of *abandoned branches on the path*, not of
  conversation length.
- Maximum indent is bounded by the number of forks on the path, not by
  turns.
- Deterministic, O(n), no new state, no new config. The key is a total order
  (heights differ, or slice indexes do), so the layout is a pure function of
  the tree — it does not move when you navigate, only when a branch grows.
- Layout lives entirely in the view layer (`src/v25_tree.rs`); the session
  layer (`build_tree`, sibling order) is untouched.

Measured on the §1 session (spur step 4, the same scale as the old branch
step):

```
indent=0 kick off
indent=2 reply 1
indent=4 experiment prompt      <- spur, right
indent=6 experiment reply       <- spur, right
indent=0 second message         <- main flow back at the margin
indent=4 side quest             <- spur
indent=6 side reply             <- spur
indent=2 reply 2
indent=0 third message
indent=2 reply 3
```

Same content, same row order, same turn (+2) cue — only the columns moved.

Implementation as shipped (this is the code in `src/v25_tree.rs`;
`flatten_turns` kept its signature and call sites):

```rust
// src/v25_tree.rs — new, next to `visible_terminals`

/// Columns added when a node hangs off the main line instead of continuing
/// it. Same scale as the pre-refinement fork step, so a one-level spur looks
/// exactly like a fork did.
const SPUR_STEP: usize = 4;

/// Entry IDs that continue their parent's main line: for every node, the
/// child with the deepest visible path downward, ties going to the newest
/// branch — the same policy applied recursively at every level, so the
/// leftmost branch at each fork is the longest one and spurs shorten the
/// further right you go. One ID per forking node; empty for a purely
/// linear tree.
///
/// Iterative post-order — a 1000-entry linear session overflows recursion
/// (same constraint as `visible_terminals`). Heights are computed over the
/// *visible* projection: invisible entries (tool results, tool-call-only
/// assistant segments) must not make a spur look deep.
fn spine_children<'a>(nodes: &'a [SessionTreeNode], labels: &HashMap<String, String>) -> HashSet<&'a str> {
    let visible = |n: &SessionTreeNode| {
        !entry_display_label_ext(&n.entry, n.label.as_deref(), labels).is_empty()
    };
    let mut height: HashMap<&'a str, usize> = HashMap::new();
    let mut spine: HashSet<&'a str> = HashSet::new();

    let mut stack: Vec<(&'a SessionTreeNode, bool)> = Vec::new();
    for n in nodes.iter().rev() {
        stack.push((n, false));
    }
    while let Some((node, expanded)) = stack.pop() {
        if !expanded {
            stack.push((node, true));
            for c in node.children.iter().rev() {
                stack.push((c, false));
            }
            continue;
        }
        let vis = usize::from(visible(node));
        let mut h = vis;
        // (height, slice position): slice position IS recency — siblings
        // are stored in write order, so a later child is the newer branch.
        // The two components are distinct per sibling, so the key is a
        // total order and the layout is fully deterministic.
        let mut best: Option<(usize, usize, &'a str)> = None;
        for (i, c) in node.children.iter().enumerate() {
            let id = c.entry.id.as_str();
            let ch = height.get(id).copied().unwrap_or(0);
            h = h.max(vis + ch);
            let key = (ch, i);
            if best.is_none_or(|(bh, bi, _)| key > (bh, bi)) {
                best = Some((ch, i, id));
            }
        }
        height.insert(node.entry.id.as_str(), h);
        if let Some((_, _, id)) = best {
            spine.insert(id);
        }
    }
    spine
}
```

Variant A2 (`active` first, recency second) adds one map lookup and one
comparison per child — the `active_path: &HashSet<String>` that
`flatten_turns` already receives is threaded in and `best` becomes
`(height, active, index, id)`. Variant A3 additionally carries a `size`
map and inserts `size(child)` into the key. Nothing else differs.
```

and in `flatten_turns` (`src/v25_tree.rs:340`) replace the fork rule:

```rust
    let spine = spine_children(nodes, labels);
    // The spine child continues the parent's line; every other child is a
    // spur and steps right. Single-child nodes are always the spine, so a
    // linear chain stays at column 0 regardless of length.
    let child_indent = |child: &SessionTreeNode, cur: usize| {
        if spine.contains(child.entry.id.as_str()) {
            cur
        } else {
            cur + SPUR_STEP
        }
    };
```

then replace the two push sites (`:371` for the invisible-entry descent and
`:403` for the normal one):

```rust
    for child in node.children.iter().rev() {
        stack.push((child, child_indent(child, cur_indent)));
    }
```

Nothing else changes: `data()` already passes `labels`, the row `desc` and
`FlatRow.indent` plumbing is untouched, and the `Arc::ptr_eq` cache still
covers the (slightly more expensive) rebuild.

Complexity: one extra O(n) pass, O(n) scratch (`height`/`size`), O(forks)
persistent (`spine`). The prototype used recursion and a `HashSet<String>`;
the shipped version must use the iterative form and borrowed `&str` as
sketched, to match the file's stack-safety convention.

Choice of `SPUR_STEP`:

| Step | Effect |
|---|---|
| **4 (recommended)** | Same scale as today's fork step; keeps the spur column visually distinct from the assistant +2 cue (2). A level-1 spur starts at 4, a level-2 at 8. |
| 2 | Denser; a level-1 spur's first column (2) coincides with the assistant reply column, which reads as "part of the turn above". |
| 3 | Middle ground; breaks the even-column rhythm of the +2 cue. |

`SPUR_STEP = 4` also means a two-level spur lands exactly where a two-level
fork lands today, so muscle memory transfers.

---

### Option B — Active-path spine (current branch wins)

Same machinery, different key: if any child of a node is on the active
path (root→leaf), *that* child is the spine; fall back to Option A's rule
otherwise.

Trade-off, measured on the same session with the leaf moved into the
2-row experiment spur:

```
=== Variant A (longest path) ===
indent=0 kick off            indent=0 kick off
indent=2 reply 1             indent=2 reply 1
indent=4 experiment prompt   indent=0 experiment prompt   <- current branch
indent=6 experiment reply    indent=2 experiment reply
indent=0 second message      indent=4 second message
indent=4 side quest          indent=8 side quest
indent=6 side reply          indent=10 side reply
indent=2 reply 2             indent=6 reply 2
indent=0 third message       indent=4 third message
indent=2 reply 3             indent=6 reply 3
```

Variant B keeps *the branch you are standing on* at the margin, but it
reproduces exactly the complaint that motivated this change for every
session where the current branch is the short one: the six-row main flow
(`kick off` … `reply 3`) is pushed to columns 4/6 and the right margin is
eaten by rows you are not reading. It also makes the layout depend on
navigation, so `/tree` looks different before and after a `switch`.

Verdict: **rejected as the primary rule** (decision, this round). The
active path is kept only as the *last* tie-break inside Option A's key
order, where it costs nothing when heights or sizes already differ.

---

### Option C — Reorder rows so the spine is contiguous

Change the *display order* too: render the spine child (and its subtree)
first at each fork, so the main flow reads as one unbroken column and the
spurs hang off it in a block below.

- Pro: the main flow is a single left-aligned block; the listing becomes a
  "spine + excursions" document rather than a chronological log.
- Con: destroys chronology within a fork, which is currently the only
  reliable signal for "which of these two branches did I take first".
- Con: it is a bigger behavioural change than the user asked for. It can be
  done purely in the view layer (reorder the `flatten_turns` push), but the
  sibling order it would override is produced by `build_subtree`
  (`src/session/types.rs:537`) and is shared with everything else that
  reads `raw_tree` — so it belongs in a separate, later change.

Verdict: **rejected** (decision, this round): row order stays
**chronological**; the tree structure determines the indent column only.
One consequence to keep in mind when reading the result, and the reason the
two are independent: two major lines of work that branch early on
*alternate vertically* in larger blocks (the listing stays a timeline), and
the indent alone tells you which of the two is the long one. That is the
intended reading, not an artefact.

---

### Option D — Clamp the old rule (cheap mitigation, not the fix)

Keep `is_branch → +4` and cap the accumulated indent at, say, 12 columns
(`cur_indent.min(12)`), or drop the indent to a fixed 2 for anything past
the first fork.

- Pro: three-line change, no new pass, no new mental model.
- Con: does not deliver the request — the main flow is *still* pushed right
  once per fork, and after the clamp every spur and the main flow share a
  column, so the tree structure stops being readable.
- Con: still counts invisible children as forks (§2).

Verdict: fallback if Option A turns out to be unwanted, not a substitute.

---

### Option E — Box-drawing connectors (complement, not alternative)

Render `├─` / `└─` / `│` prefixes instead of blank indent, keeping
Option A's spine rule. This makes *nesting within a spur* visible again
(which indentation deliberately flattens) and is the natural way to show
"this spur is 3 forks deep".

- Pro: strictly more information; deferral already recorded in
  `doc/slashdot/tree_design.md` ("Box-drawing characters … Deferred").
- Con: it is a rendering feature, not an indentation fix; it interacts with
  `Table::fit`'s width accounting and with the wrap-disabled column.
- Con: doubles the columns consumed (step 2 + 2 connector chars).

Verdict: **deferred to a later phase** (decision, this round), and to be
designed together with Option F as one coherent follow-up — markers at
branch points are most legible when the connector is there to anchor them.
Option A makes that pass cheaper: the spine set is exactly the information
a connector renderer needs (`is this the spine child? is this node a fork
point?`), which is one more argument for computing it explicitly rather
than inferring it from child counts.

---

### Option F — Compensating markers (cheap, orthogonal)

Option A flattens nesting: everything under a spur is at the spur's level
regardless of how deep it forks. The information that compensates for it
is the same information a connector renderer would consume, so the two
should be specced together rather than separately:

- a gutter marker at **branch points** and on the active path (`▸`, `├`),
  so "where did this split" survives the flattened nesting;
- a filterable `depth` field (the spur level) so `.depth:2` finds
  second-level excursions — the `tags:leaf` precedent shows this needs no
  filter code, just an optional field on the row.

Verdict: **deferred**, bundled with Option E into one follow-up design
(decision, this round). Not in this change — the diff stays indentation
only. The follow-up design should specify the marker set, the connector
step width, and the `Table::fit` interaction together, since they share
the same gutter budget.

---

## 6. Behaviour on the existing test corpus

Before the change, `cargo test --lib` against a prototype of the rule
gave **736 passed, 1 failed** — `tree_with_model_changes_indent_render`,
the only test that pinned the old fork rule by name. That is the whole
behaviour change, and it is not a bug:

```
row 4: indent=0 "[MODEL] openai/gpt-4o"      (mc1, fork point)
row 5: indent=4 "[MODEL] openai/gpt-4o"      (mc2 — now a spur)
row 6: indent=4 "[MODEL] anthropic/claude-3" (mc3 — now a spur)
row 7: indent=0 "[MODEL] openai/gpt-4o"      (mc4 — the spine)
```

The three children of `mc1` are all leaves, so their heights tie and the
recency tie-break hands the margin to `mc4`, the newest. (The prototype
used first-child-wins and gave it to `mc2`; the difference is the tie-break
decided in §4.4, and the test now pins the shipped behaviour.)

Everything else — every linear session test, the tool-chain indent test,
the render-plan tests, the `Arc::ptr_eq` cache test — passed unchanged.
After the change: **745 lib tests, all integration tests, no new clippy
warnings.**

The test pins "all children of a fork step right" by name. Under the new
rule exactly one child stays. Everything else — including every linear
session test, the tool-chain indent test, the render-plan tests and the
`Arc::ptr_eq` cache test — passes unchanged, which is the strongest
available evidence that the blast radius is one function.

---

## 7. Cross-cutting concerns

### 7.1 The assistant +2 turn rule

Unchanged, and it still works, because the two rules are computed at
different points: `cur_indent` (spine level) is what is pushed to children,
while the row's own column adds +2 for assistant rows. The consequence is
the same as today — leaving a turn de-indents back to the spine level, so a
main-flow user message is never indented under the assistant reply above
it. No change in semantics, only in which column the spine sits at.

### 7.2 Invisible entries

`spine_children` measures the visible projection (`entry_display_label_ext`
is already computed for every node by `build_label_map`). Consequences:

- a spur made only of tool results does not out-rank a real conversation;
- a parent with an invisible child and a visible child is no longer treated
  as a fork by *inference* — it is measured, and the visible child usually
  wins the spine. This fixes the latent `children.len() > 1` artefact
  described in §2.

The invisible-entry descent path (`src/v25_tree.rs:368-374`) must use the
same `child_indent` helper as the normal path; forgetting it is the easiest
way to produce an inconsistent layout (it is a bug the prototype hit during
development).

### 7.3 Compaction entries

Compactions are linear: `apply_compaction` → `Session::append_compaction`
parents the new entry at the current leaf, so it never creates a second
child. Nothing special is required. A compaction row is a visible row, so it
counts toward the height like any other — which is correct, it is a line of
history the user can see. (Compacting from an *earlier* point after
navigating back does create a real second child; that is a genuine fork and
the spine rule handles it like any other.)

### 7.4 Layout stability ("flapping")

The spine is recomputed on every rebuild, and heights grow as branches are
extended, so a fork can change hands. With the chosen key
`(height, recency)` the behaviour is bounded in two ways:

- **A flip on a non-tie is always caused by the objective crossing over.**
  If the spine moves from `A` to `B` while their heights differ, then
  `B`'s height has just become strictly greater — the layout follows the
  objective, it does not jitter.
- **A flip on a tie is caused by the user writing in the other branch.**
  That is the moment recency changes hands, and it is also the moment the
  user demonstrably cares about the other branch. Nothing else moves the
  layout.

**Navigation does not move the layout at all.** A `switch` writes no
entry, so the recency ranking is unchanged and `/tree` renders identically
before and after navigating. This is the property that makes A1 preferable
to both A2 (`active` first) and to Option B — those two *do* re-lay out on
every navigation across a tie, which is the "the tree changes under me"
feeling that Option B was rejected for.

If the layout still moves too often in practice, the options are (a) switch
to `size` as the secondary key, which flips later (only when a branch
gains a row) but can then move on navigation too, or (b) remember the
previous spine in the `TreeListNode` data cache — stateful, makes the
layout depend on view history, not recommended.

### 7.5 Determinism and multiple roots

`spine_children` compares the whole key `(height, slice index)` with a
strict `>`. Slice indexes are distinct, so the key is a total order and the
layout is fully deterministic for a given tree — there is no "first child
wins" fallback to reason about, and no dependence on iteration order.
Multiple roots (orphan entries, the `build_tree` root vec) each start at
column 0 and are not ranked against each other; the existing active-path
sort of the root vec is left alone.

### 7.6 Stack safety and cost

`flatten_turns` is iterative precisely because 1000-entry linear sessions
overflow recursion (`doc/slashdot/tree_design.md`). `spine_children` must
be iterative for the same reason. The prototype's recursive version is fine
for a 10-node demo and overflows at the scale the codebase already cares
about. Cost: one extra linear pass per tree rebuild, which happens only when
the tree `Arc` or the leaf changes.

### 7.7 Width budget

Indent is leading spaces inside a wrap-disabled, ellipsised cell
(`table.rs:380`), and fuzzy snippets are extracted from the padded cell
(`table.rs:419`). Lower indents give strictly more text and a wider match
window, so Option A improves this axis too. No rendering change needed.

One pre-existing quirk worth knowing before reviewing the diff: while a
fuzzy filter is active, a matched row **loses its leading indent** —
`extract_snippet` windows the cell around the match and prefixes `…` when
the window does not start at column 0 (`table.rs:531-536`), so the
indent is cut off. This already happens today with the `+2`/`+4` indents;
it is not introduced here, but it is the first thing that looks like a
regression when you type a filter to check the new layout. Fixing it
means re-indenting the snippet window in the view layer, which is out of
scope for this change.

---

## 8. Implementation — done

| Step | File | Change |
|---|---|---|
| 1 ✅ | `src/v25_tree.rs` | Added `SPUR_STEP` + `spine_children()` (iterative post-order, `HashSet<&str>` out, key `(height, slice index)`) |
| 2 ✅ | `src/v25_tree.rs` | `flatten_turns`: dropped `is_branch`/`child_indent`, added the `child_indent` closure, used it at both push sites (the invisible-entry descent and the normal one) |
| 3 ✅ | `src/v25_tree.rs` | `tree_with_model_changes_indent_render` updated to the spine layout (see §6) |
| 4 ✅ | `src/v25_tree.rs` | Nine tests added — the list below, plus `spine_indent_survives_render_plan` |
| 5 ✅ | `doc/slashdot/tree_design.md` | §Indentation rewritten to the spine rule, with the properties and the pointer back to this document |

Not touched: `src/session/types.rs` (`build_tree`, sibling order),
`AppState::raw_tree`, the row/field plumbing, the engine.

**Test surface.** No file under `tests/` references the tree or its
indentation, so the whole blast radius is the `src/v25_tree.rs` unit-test
module — `cargo test --lib` is the meaningful gate (plus a full
`cargo test` before committing, since the row `description` is shared
plumbing). With A1 the implementation needs only the `height` map; the
`size` map and the `active_path` parameter drop out entirely.

### Tests (all added)

1. `linear_session_indent_unchanged` — a fork-free chain still renders
   0/2/0/2/… (guards the "spine is the only child" property at 1000 nodes,
   which also exercises stack safety).
2. `longest_branch_stays_at_left_margin` — the §1 session: main flow at
   0/2/0/2/0/2, both spurs at ≥ 4, spurs to the right of the flow.
3. `short_spur_does_not_outrank_long_flow` — the regression that motivated
   the change: adding one row to a spur must not move the main flow.
4. `invisible_children_do_not_fork` — `[invisible tool-call assistant,
   visible user]` stays at column 0.
5. `spine_prefers_newest_on_height_tie` — equal heights: the newer child
   holds column 0, even when the older one has more rows (this is the
   `recency`-over-`size` decision, §4.4, pinned as a test).
6. `layout_survives_navigation` — moving the leaf to the other branch of a
   height tie does **not** change any indent (the `switch`-stability
   property, §7.4).
7. `chain_beats_bush_recursively` — a chain-vs-bush fork (§4.3) asserts
   the height property: the deeper sibling is at column 0 and no row to
   its right is deeper.
8. `layout_is_deterministic` — same tree built twice ⇒ identical indents.
9. Update `tree_with_model_changes_indent_render` (see §6).

If A2 (`active` first) is chosen instead of A1, test 6 inverts and test 5
becomes "equal heights, leaf on the older branch ⇒ the *active* child
holds column 0".

The existing suite already covers: assistant +2 after tool chains, model
change rows, compaction rows, the `Arc::ptr_eq` cache, `.tags:leaf`, and
render-plan round-tripping of leading spaces.

---

## 9. Reproducing the measurements

The layouts in §1, §4.3, §5 (Option A) and §5 (Option B) were produced by
a throwaway `#[test]` in `src/v25_tree.rs` that builds the demo tree, calls
`TreeListNode::data()`, and prints `desc.len() - desc.trim_start().len()`
per row — the same measurement the existing indent tests use. Those
harnesses were scratch and have been removed; the same measurement now
lives in the `row_indents` / `indent_of` test helpers, so the numbers
above can be re-derived by running the corresponding test.

```rust
// the measurement, as it now lives in the test module
fn row_indents(resp: &DataResponse) -> Vec<(String, usize)> {
    resp.rows
        .iter()
        .map(|r| {
            let desc = r.description.clone().unwrap_or_default();
            (r.id.clone(), desc.len() - desc.trim_start().len())
        })
        .collect()
}
```

---

## 10. Decisions

Recorded from review of this document:

| # | Question | Decision |
|---|---|---|
| 1 | Primary key — `height` or `size`? | **`height`.** It is the only variant that guarantees the shortening property the change exists to create (§4.3). |
| 2 | Secondary key — `size` or `recency`? | **`recency`** (newest branch wins exact height ties). Gives up the min-total-indent guarantee; the cost is bounded to exact height ties (§4.4). |
| 3 | Active path — tie-break only, or full priority (Option B)? | **Tie-break only**, and *not* after `recency`: with recency keyed on slice index the key is already a total order, so a trailing `active` key can never fire. Either drop it (A1) or put it in front (A2). **Dropped** — see the open question below. |
| 4 | Step — 4 or 2? | **4.** Same scale as the old fork step; keeps the spur column distinct from the assistant +2 cue. |
| 5 | Row order — chronological or spine-first (Option C)? | **Chronological.** The tree structure determines the indent column only; early forks keep alternating vertically in blocks, with the indent saying which line is the long one. |
| 6 | Markers (`▸`, `depth` field) in this change? | **No.** Deferred and merged with Option E (box drawing) into one coherent follow-up design. |
| 7 | Recursion policy | **Apply the same key at every node** (already implied by the bottom-up pass): the first level in from the margin holds the longest top-level branches, and spurs shorten the further right you go. |

So the layout function is, in full:

```
spine(v) = the child c maximising (height(c), write_index(c))     // A1
indent(v) = number of spur edges on the path from the root        // × SPUR_STEP
row_column(v) = indent(v) + (2 if assistant else 0)
```

### The one question left: A1 or A2 as the tie-break

`(height, recency)` vs `(height, active, recency)`. The difference is one
look-up and one comparison per child, and it decides one thing: when two
branches are *exactly* the same depth, does the margin go to the branch you
last wrote in (A1) or to the branch the leaf currently points at (A2)?

They differ only after a `switch` that writes nothing — A1 leaves the
layout alone, A2 moves one step. Since the user's stated goal is a
*stable* margin that the main flow owns, and A2 is Option B in miniature
(a change that only ever applies to exact ties, but is still a change that
happens when you navigate), **A1 is recommended and is the default in the
sketch**. A2 is one boolean in the key tuple if the "put the branch I am
on at the margin" reading is preferred.
