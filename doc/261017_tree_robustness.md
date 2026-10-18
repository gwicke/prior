# `/tree` robustness — chronological order, multiple roots, sparse layout

## Status

**Implemented (B1 + B2, glyph joins).** Fixes the `/tree` listing when a session
contains more than one root (a re-asked first prompt, a persisted `leaf{null}`, etc.)
by emitting rows in file/append order and deriving each row's colour from its own tree.

Implementation note: the row-order and root-hue behaviour below is in place. The
sparse branch-point forest (§2.3) is **not** required for correctness and is deferred;
the existing single post-order pass still supplies heights/ranked children, so the
layout is O(n) rather than O(branches). The write-side options (§6) remain deferred.

## 1. Problem

Some sessions render a second, much older tree at the end of the listing, painted in the
mainline style. Reproduced from a real session:

- two roots in one JSONL: an orphan opening tree (`21:49`) and the active tree (`22:07`)
  whose first prompt is byte-identical — `reask_from_entry` on a root prompt calls
  `move_leaf(None)` and the re-sent prompt lands with `parentId: null`;
- `build_tree` sorts the active root first (`src/session/types.rs:513-522`);
- `flatten_turns` is a pre-order DFS (`src/v25_tree.rs:579-584`, `:668`), and the listing
  applies no default sort, so the active tree is emitted first and the orphan trails at
  the end;
- every root is seeded at `(depth 0, branch 0)` (`:582-584`), and 0 is the anchor colour,
  so the orphan is painted indistinguishably from the mainline.

The fix is not to reorder trees. It is to make emission chronological and to give every
row its structure from its own tree.

## 2. Decisions

### 2.1 Row order is chronological; trees only supply the prefix

Emit **all** visible entries as one sequence in **file/append order** (which is the
chronological order the JSONL is written in). Do not sort by root, do not group by tree.
Trees may interleave; that is expected.

File order is used as-is (cheaper than parsing/sorting timestamps). Timestamps remain a
per-row display/filter field. If clock skew ever matters, a stable sort can be added
later without changing the model.

### 2.2 The mainline is the longest branch, always branch 0

- The deepest root (by visible depth) is the mainline and **always** gets `branch 0` /
  anchor colour, so the mainline colour never moves.
- Remaining roots are ranked by descending visible depth and get the next spur ordinals
  (`SPUR_RGB`).
- Within a tree the existing rule is unchanged: per fork, children ranked by
  `(height desc, write order asc)`; rank 0 continues the parent's `(branch, depth)`,
  the rest claim new spur ordinals and `depth + 1`.

This removes the `HashMap`-order nondeterminism for roots and the "orphan painted as
mainline" bug in one move. Hue is an attribute of the row, so it is interleaving-safe.

### 2.3 Sparse branch-point forest

Spine selection needs subtree heights, but height is additive over linear chains, so
only branch points matter.

- A **branch point** is a node with at least two *visible* children (meta/tool entries
  do not create forks).
- Collapse each linear arm into a weighted edge `p -> b` between consecutive branch
  points, weight = number of visible entries on the chain from `p` (exclusive) to `b`
  (inclusive). A dead-ending arm contributes its length as a leaf weight.
- Add a virtual root over the real roots so root ranking is the same fork ranking as
  any other.

Then `height(p) = max(edge_weight + height(b))` over the branch-point forest, which is
O(branches), not O(n). Branch ids and depths are assigned in the same walk.

### 2.4 Per-row join glyphs (no caps)

A row's bar connects upward iff the previous emitted row shares `(branch, depth)`, and
downward iff the next emitted row does. Assistant rows keep the rightward arm; user rows
use bare ticks. This leaves an ink gap wherever unrelated sequences meet.

| up | down | user | assistant |
|----|------|------|-----------|
| yes | yes | `┃` | `┣` |
| no | yes | `╻` | `┏` |
| yes | no | `╹` | `┗` |
| no | no | blank / short mark | `╸` |

`SpurStart` is the `(up=no, down=yes)` row plus the left run: `╻` (user) or `┏`
(assistant) in the bar cell. The connector's horizontal run is drawn only when the
parent's row is the immediately preceding emitted row; otherwise the turn is drawn
without the run. Today's `Bar::End` (`┻`) horizontal cap is dropped.

## 3. Pipeline

Input: `SessionIndex` with `order: Vec<id>` in append order (new field, filled while
scanning the JSONL) and the existing `children` map.

1. **Visibility / labels** — one forward scan over `order`: display text, role,
   label map, `visible` flag. O(n).
2. **Segment pass** — one forward pass over `order` (parent before child is guaranteed
   by file order) carrying `(nearest branch point, accumulated visible count)`. When the
   node is a branch point (visible child count >= 2, known from `children`), emit a
   weighted edge into the sparse forest and start a new segment. O(n), O(branches)
   output.
3. **Forest post-order** — heights, spine choice per fork, branch ids and depths.
   Roots ranked first (virtual root), rank 0 = branch 0. O(branches).
4. **Emission** — forward pass over `order`: stamp each entry with its segment's
   `(branch, depth)` and `spur_start`; compute `up`/`down` from the neighbouring rows;
   paint and push a row. O(n).

Active-path / leaf highlight still comes from one parent-pointer walk from the leaf;
labels/terminals come out of the same passes.

## 4. Data and caching

- `SessionIndex.order: Vec<String>` — append order.
- `TreeLayout` — immutable, shared via `Arc`:
  - rows aligned with `order` (or entry id -> index), each carrying `depth`, `branch`,
    `spur_start`, `visible`, `terminal`, plus display text/role/timestamp;
  - the sparse branch-point forest used to derive them (can be dropped after layout);
  - active-path set for the current leaf.
- `AppState` holds `Arc<TreeLayout>`; `TreeListNode` caches on that `Arc` (same pattern
  as today's `cached_tree_rc`).

`raw_tree` / `SessionTreeNode` are removed from the production path. The only real
consumer of `raw_tree` is `TreeListNode` (`src/v25_tree.rs:894`); the rest is plumbing
(`agent.rs:280`, `ReplCtx.tree` at `repl.rs:110`, `repl.rs:1503`, `AppState`
`update_cycle_view` at `app_state.rs:699`). `SessionTreeNode` and `build_tree` stay
temporarily only for the tests that build trees by hand, or those tests move to the
layout arena.

## 5. Code touch points

- `src/session/types.rs` — root/child ordering no longer decides display order; add
  `order`. Keep `build_tree` for tests until migrated.
- `src/session/storage.rs` — record append order while scanning.
- `src/v25_tree.rs` — replace `build_label_map` / `analyze_tree` / `flatten_turns` /
  `Bar` with the pass pipeline, `TreeLayout`, and the glyph table; `row_prefix` reads
  precomputed `(branch, depth, up, down, assistant, spur_start)`.
- `src/app_state.rs`, `src/agent.rs`, `src/repl.rs` — carry the layout `Arc` instead of
  `raw_tree`.
- Tests — multi-root ordering (orphan first, mainline last), mainline-is-longest /
  stable colour, leaf `None` fallback, gap glyphs between unrelated rows, connector only
  on adjacency, determinism across repeated loads.

## 6. Write side — deferred

Options to stop producing a second root in the first place, listed for later:

- **A1 — session-root anchor.** Write one invisible root node at session creation
  (`custom`/meta) and parent all top-level prompts, including re-asks, to it. Single root;
  the old tree becomes a spur at the top. Requires a small format addition and a
  compatibility check with pi's own tree view.
- **A2 — anchor to the old root + superseded marker.** On a root re-ask, parent the new
  prompt to the old root and mark the old prompt superseded so context building skips it.
  Single root, but adds a context-build special case and pi would still see the old prompt
  as history.
- **A3 — keep pi parity, record intent.** Leave the second root and emit a
  `branch_summary`/`custom` marker attached to it so the renderer can label the abandoned
  tree. Least invasive; still two roots.

None are needed for the renderer fix: B1/B2 handle any number of roots, including files
already on disk. Revisit A when format churn is acceptable.

## 7. Non-goals

- No change to context building or compaction.
- No change to timestamps or the `.sort:` machinery; `.sort:` can still override row
  order.
- No change to the palette constants or fade curve.
