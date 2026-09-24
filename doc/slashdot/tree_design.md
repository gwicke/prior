# /tree v2.5 implementation plan

## Status

**Phases 1–2 complete.** 620 lib tests pass. `/tree` listing, switch, and
compact (manual/text-only/prompt) are fully wired through the v2.5 engine.

**Recent fixes:**
- Active entry cursor: `active_ids` now searches full tree (was only
  checking root-level entries). New `active_entry_marked_deep` test.
- Iterative DFS: all tree-walking functions (`flatten_turns`,
  `collect_active_path`, `build_label_map`, `find_entry`) converted to
  explicit stacks. Linear sessions with 1000+ entries no longer risk
  stack overflow.
- Duplicate separator on commit fixed (see `doc/slash_menu_v2.5_impl.md`).
- Action children (switch, compact) now display id + description columns.

---

## Overview

Port `/tree` to the v2.5 `SlashDotNode` engine. The tree overlay shows the
session history as an indented tree, allowing the user to navigate to any
entry and perform actions (switch, compact).

Shape per v2.5 design §10: **Dynamic listing → static action children**,
same pattern as `/session`.

---

## Node tree

```
root (StaticListingNode, empty)
  └── tree (StaticListingNode, "navigate session history…")
        columns: [description] (no wrap, no header label)
        └── TreeListNode (SlashDotNode — dynamic, cached)
              data()   → one row per tree entry (full text for fuzzy search)
              header() → entry detail from the same cached row data
              children:
                ├── switch (leaf → TreeSwitchNode)
                │     submit: AsyncOp::TreeSwitch { target_id }
                └── compact (listing)
                      ├── manual   (leaf → TreeCompactManualNode)
                      ├── text-only (leaf → TreeCompactTextNode)
                      └── prompt   (leaf → TreeCompactPromptNode)
                            suffix = custom LLM prompt
```

### Merged child set at `/tree` level

The "tree" listing shows the concatenated rows of all its children. The
only child that returns rows is `TreeListNode` — its `data()` returns one
row per visible session entry. The listing IS the tree.

### Drilling into an entry

When the user selects a tree entry (Tab/Enter), the engine drills into
`TreeListNode`. Now `TreeListNode` acts as a listing node: its own
children (switch/compact) are shown. `TreeListNode::header()` renders
the full message body for the selected entry.

### Drilling into an action

When the user selects an action (switch, or compact → manual/text-only/prompt),
the engine drills into the action leaf node. `header()` renders action-specific
help text. Enter triggers `submit()`.

---

## Data flow

### Single data source: raw tree in MenuShared

`MenuShared.raw_tree` stores the raw session tree. Both `ReplCtx` and
`MenuShared` share the same `Rc` — no deep clone on sync.

`ReplCtx.tree` is `Rc<Vec<SessionTreeNode>>`. In `build_session_ctx`:
```rust
let tree_rc = Rc::new(s.get_tree().await);
// ctx.tree = tree_rc.clone();        // O(1) — Rc bump
// shared.raw_tree = tree_rc.clone();  // O(1) — Rc bump
```

### Row building in data()

`TreeListNode::data()` flattens the raw tree into rows:
- **Description = full text** ("full text for fuzzy search"). Messages
  keep their newlines (`sanitise_multiline` strips control chars but
  keeps `\n`/`\t`). The tree column is wrap-disabled, so
  `Table::render` flattens `\n` to spaces and ellipsizes for display —
  fuzzy rank (`id` + `description`) and `header()` see the raw
  multi-line string.
- **Non-message markers** in the description: `[MODEL] provider/model`,
  `[THINK] level`, `[SUMMARY] …`, and for compactions
  `[COMPACT] from <kept context, ≤300 chars>` + `\n` + the summary
  (identifying line first, then the content).
- **Fields**: `role`, `timestamp` (ISO, filterable/sortable), the
  `on_path`/`leaf` position markers, and optional `tags` (below).
- Header format: `[USER ts]`, `[COMPACT ts] from … / summary`,
  `[MODEL] provider/model (ts)`.

### Header from the cached row

`header()` renders from the same cached `data()` row (design: "entry
detail from the same cached row data") — no raw-tree `find_entry`
walk, no second derivation. Role/timestamp come from the row's fields;
the body is the row description with the tree indent stripped. The
`[COMPACT …]`/`[SUMMARY …]` prefixes re-stamp the marker with the
timestamp; model/thinking keep their in-body marker and append ` (ts)`.

### Branch endpoints (`tags` field)

`visible_terminals()` (iterative post-order; deep sessions overflow
recursion) marks visible entries with no visible entries anywhere
below them — the endpoints you actually see: invisible meta entries
(tool results, session info) don't count, so a visible message above a
dead-end tool result is the terminal node. Those rows carry
`tags = "leaf"`; all other rows **omit** the field. Since a missing
filtered field drops the row (exact string match), `.tags:leaf` filters
the listing to terminal nodes with no filter code at all.

### Data caching (Rc::ptr_eq)

`TreeListNode` caches its `Rc<DataResponse>` internally. Freshness check:
`Rc::ptr_eq` on the tree Rc + same leaf ID = cached data is valid.

---

## Indentation

The indent rule in `flatten_turns` is:

```
assistant  → indent + 2
all others → indent     (user, compaction, model change, etc.)
```

Branch forks (nodes with >1 child) add +4 via `child_indent`.

**Key design decision:** Invisible entries (tool results, tool-call-only
assistant segments) do NOT affect turn state. They are skipped without
modifying any mutable state, so a chain of invisible entries between a
user message and the visible assistant text does not break the +2 indent.

This differs from the original v1 code, which mutated `in_turn` for
invisible entries (causing the assistant to lose its +2 indent after tool
chains).

---

## Switch

`TreeSwitchNode` is a leaf. On submit, it sets:
```
AsyncOp::TreeSwitch { target_id }
```

Handler in agent.rs moves the leaf pointer via `session.move_leaf()`.

---

## Compact (three modes)

### Node structure

- `TreeCompactManualNode` — submit sets `AsyncOp::TreeCompact { Manual }`
- `TreeCompactTextNode` — submit sets `AsyncOp::TreeCompact { TextOnly }`
- `TreeCompactPromptNode` — submit reads suffix as LLM prompt

All three share `AsyncOp::TreeCompact { target_id, mode, payload }`.

### AsyncOp::TreeCompact handler in agent.rs

Ports the existing `/tree <id> compact` handler. Validates target entry,
computes tokens_before, resolves mode, lazy-builds agent for LLM mode,
runs `run_compaction()`.

**Note:** Editor integration (`prepare()` opens `$EDITOR`) is deferred —
the nodes are currently single-phase. The agent loop handler does the
full compaction flow.

---

## Rendering

### Table header

The column label is `""` (empty), which suppresses the table header row.
The `has_table_header` check in `node_entry.rs` and `render.rs` skips
the header when all column labels are empty.

### Entry header (when drilled)

`header()` shows the full message body wrapped to terminal width with
`[ROLE ts]` prefix on the first line:
- User: `[USER ts] message text...`
- Assistant: `[ASSISTANT ts] response text...`
- Compaction: `[COMPACT ts] summary text...`
- Model change: `[MODEL] provider/model (ts)`
- Thinking change: `[THINK] level (ts)`

---

## File plan

| File | Action | Description |
|------|--------|-------------|
| `src/slash_menu_bridge.rs` | Modified | `raw_tree: Rc<...>`, `AsyncOp::TreeSwitch/TreeCompact`, `CompactMode` |
| `src/v25_tree.rs` | New | `TreeListNode`, `TreeSwitchNode`, compact nodes, `flatten_turns` |
| `src/v25_menu.rs` | Modified | Wire tree subtree into root menu |
| `src/lib.rs` | Modified | Register `v25_tree` module |
| `src/agent.rs` | Modified | `AsyncOp::TreeSwitch/TreeCompact` handlers |
| `src/repl.rs` | Modified | Sync `raw_tree` to `MenuShared` |

---

## Tests (27 tree tests)

Phase 1: listing, caching, header, switch, active entry (root + deep),
model change prefix, empty entry hidden, linear indent, render plan indent.

Phase 2: compact submit (manual/text-only/prompt), full traversal,
headers, empty suffix, model change branch indent.

Indent-specific: `assistant_after_tool_result_chain_gets_indent` —
verifies that invisible tool-call/tool-result entries between a user
message and the visible assistant text do not break the +2 indent.

---

## Deferred / follow-ups

### `switch --summarize`
Summarize abandoned branch when switching. Same compaction logic,
different span (old leaf → common ancestor).

### `/compact` root shortcut
Rewrites to `/tree <last-user> compact`. Deferred until
./prompt_rewrite_design.md.

### Double separator on AsyncOp submit
**Done.** `emit_commit` now passes `sep: false` for both `DoneSuccess`
and `DoneError`. The next prompt draws its own separator.

### Revisit editor integration in compact prepare()
`prepare()` opens `$EDITOR` for manual/text-only draft editing.
Requires session access from the node (currently deferred to agent loop).
This works, so this is low priority.

### Box-drawing characters
Branch visualization with box-drawing. Deferred — simple indentation
is sufficient for now.

### Iterative DFS for deep sessions
**Done.** All tree-walking functions use explicit stacks instead of
recursion. Linear sessions with 1000+ entries are safe.

### Vertical space allocation (detail vs rows)
**Done.** `compute_space_budget()` enforces 50:50 ratio when both
overflow, surplus flows to the other side, and minimum 3 rows are
always visible. Detail visual-line accounting correctly handles
wrapped LogicalLine items.

### Visual-line-aware row windowing
**Done.** `visual_row_window()` computes window (start, count) using
cumulative visual heights so wrapped multi-line rows scroll correctly.
