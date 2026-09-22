# /tree v2.5 implementation plan

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
        columns: [description] (no wrap)
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

**Replace** the existing `MenuShared.tree` (pre-flattened v2.0 tuples)
with the raw session tree. Both `ReplCtx` and `MenuShared` share the
same `Rc` — no deep clone on sync.

```rust
pub struct MenuShared {
    // ... existing fields ...
    /// The session entry tree (source of truth for /tree listing).
    /// Immutable between rebuilds; shared via Rc with ReplCtx.
    pub raw_tree: Rc<RefCell<Option<Rc<Vec<crate::session::SessionTreeNode>>>>>,
    // Remove: pub tree: Rc<RefCell<Vec<(usize, String, String, bool, bool, usize)>>>,
}
```

`ReplCtx.tree` changes from `Vec<SessionTreeNode>` to
`Rc<Vec<SessionTreeNode>>`. In `build_session_ctx`:
```rust
let tree_rc = Rc::new(s.get_tree().await);
// ctx.tree = tree_rc.clone();        // O(1) — Rc bump
// shared.raw_tree = tree_rc.clone();  // O(1) — Rc bump
// No deep clone of the tree.
```

Synced in `Repl::new()` and `read_input()` (once per prompt cycle,
not per keystroke):
```rust
*shared.raw_tree.borrow_mut() = Some(ctx.tree.clone());
*shared.leaf_id.borrow_mut() = ctx.leaf_id.clone();
```

**Rationale:** The pre-flattened tuples were consumed by the v2.0
`TreeProvider`. With v2.5, `TreeListNode::data()` replaces the
provider. Storing the raw tree eliminates the dual-storage problem:
`data()` builds its own flattened rows with full-text descriptions
(see below), and `header()` looks up entries from the cached row data.

### Row building in data()

`TreeListNode::data()` flattens the raw tree into rows:
- Full message text as the description (not truncated to single-line)
- Fields: `indent` (for display), `on_path`, `is_current`, `branch_depth`
- Role/type prefix: `[COMPACT]`, `[MODEL]`, `[THINK]` for non-message entries

The `entry_display_label` logic is reused but extended to produce
multi-line full text for user messages (not just the first line).

The v2.5 table's `wrap(false)` keeps the display compact. The fuzzy
matcher operates on the full text, so "fix the bug in main.rs" matches
a user message even if only visible as "fix the…" in the display.

### Data caching (Rc::ptr_eq)

The `refresh_data` method (`node_entry.rs:528`) compares the previous
`Rc<DataResponse>` with the new one via `Rc::ptr_eq`. If they're the
same pointer, the rendering layer skips the entire view rebuild (filter,
rank, sort, table layout). This is critical for the tree listing, which
can have hundreds of rows.

`TreeListNode` caches its `Rc<DataResponse>` internally:

```rust
pub struct TreeListNode {
    pub shared: Rc<MenuShared>,
    cached_data: Option<Rc<DataResponse>>,
    /// The Rc we last used to build rows. If the shared raw_tree's
    /// Rc::ptr_eq matches, data hasn't changed.
    cached_tree_rc: Option<Rc<Vec<SessionTreeNode>>>,
    cached_leaf_id: Option<String>,
}
```

Freshness check: Since the tree is an `Rc<Vec<...>>` that is replaced
entirely on each rebuild (new Rc allocation), `Rc::ptr_eq` on the tree
Rc IS the freshness signal. No generation counter needed.

```rust
fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
    let tree_ref = self.shared.raw_tree.borrow();
    let tree_rc = tree_ref.as_ref().cloned().unwrap_or_else(|| Rc::new(Vec::new()));
    let leaf = self.shared.leaf_id.borrow().clone();

    let same_tree = self.cached_tree_rc.as_ref()
        .map_or(false, |old| Rc::ptr_eq(old, &tree_rc));
    let same_leaf = self.cached_leaf_id.as_ref() == Some(&leaf);

    if same_tree && same_leaf {
        if let Some(ref cached) = self.cached_data {
            return Ok(cached.clone());
        }
    }
    // ... rebuild rows from tree_rc (read-only) ...
    let response = Rc::new(DataResponse { rows, active_ids });
    self.cached_data = Some(response.clone());
    self.cached_tree_rc = Some(tree_rc);
    self.cached_leaf_id = Some(leaf);
    Ok(response)
}
```

### header() calls self.data() for cached rows

`header()` calls `self.data()` to get the `DataResponse`. This is
always a cheap `Rc::ptr_eq` return when the tree hasn't changed (the
parent's `refresh_data` already called `data()` during traversal, so
the cache is populated). Each row carries the full text as its
description plus metadata fields (`role`, `timestamp`).

`header()` finds the matching row by ID, formats the header from the
description (wrapped to terminal width) and metadata fields. No
separate tree walk.

Note: `refresh_data()` refreshes data from *children*, not the current
node's own rows. The node's own `data()` is called by the parent's
`refresh_data()`. So `header()` calling `self.data()` reuses the
already-cached result from that parent-driven refresh.

---

## Switch

`TreeSwitchNode` is a leaf. On submit, it sets:
```
AsyncOp::TreeSwitch { target_id }
```

### AsyncOp::TreeSwitch handler in agent.rs

Port the existing `/tree <id> switch` handler from the `InputAction::Submit`
match arm. It already handles:
- Moving the leaf pointer via `session.move_leaf()`
- (Future: summarize abandoned branch — deferred)

The handler reads `target_id` from the AsyncOp and runs the same logic.

---

## Compact (three modes)

### Node structure

The compact listing node shows three mode children:
- `manual` — user-editable summary: scanned file list opened in `$EDITOR`
- `text-only` — deterministic text extraction (user prompts + assistant
  text, no tool output/reasoning), opened in `$EDITOR` for review
- `prompt` — LLM-generated summary with customizable prompt, opened in
  `$EDITOR` for review

All three share the same AsyncOp:
```
AsyncOp::TreeCompact {
    target_id: String,
    mode: CompactMode,
    payload: String,     // draft (manual/text-only) or LLM prompt (prompt)
}
```

where `CompactMode` is:
```rust
pub(crate) enum CompactMode {
    Manual,
    TextOnly,
    Prompt,
}
```

### Editor in prepare() — direct blocking call

`edit_in_editor()` (`repl.rs:4917`) is a fully blocking synchronous call:
it writes a tempfile, launches `$EDITOR` via `Command::new().status()`,
waits for the editor process, and reads back the result. The event loop
is completely blocked while the user edits, which is fine — the terminal
is owned by the editor.

**No new `SubmitResult` variant needed.** The editor is called directly
from `prepare()`:

```rust
fn prepare(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
    let draft = self.compute_draft(path_args);  // sync: scan files / extract text
    match edit_in_editor(&draft) {
        Ok(edited) => {
            *self.edited.borrow_mut() = Some(edited);
            SubmitResult::Prepared   // → confirm dialog
        }
        Err(_) => SubmitResult::Done(Some("editor cancelled".into())),
    }
}

fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
    let draft = self.edited.borrow_mut().take().unwrap_or_default();
    let target_id = ...;  // from path_args
    let mode = ...;       // from node type
    *self.shared.pending_op.borrow_mut() =
        Some(AsyncOp::TreeCompact { target_id, mode, payload: draft });
    SubmitResult::Done(None)
}
```

The node holds `edited: RefCell<Option<String>>` to bridge prepare → submit.
The engine's confirm flow (`confirm_pending` flag) already separates the
two calls.

### Manual mode

```
prepare():
  1. Resolve the dropped span (entries before target_id on active branch)
  2. Scan for touched files (read/write/edit paths)
  3. Build a template:
       Documents: *.md files
       Files: everything else
  4. Open $EDITOR with template (blocking)
  5. Store edited text in self.edited
  6. Return SubmitResult::Prepared

submit():
  1. Read edited draft from self.edited
  2. Set AsyncOp::TreeCompact { target_id, mode: Manual, payload: draft }
```

### Text-only mode

```
prepare():
  1. Resolve the dropped span
  2. Extract: user prompts + assistant text responses per turn
     (skip reasoning, tool calls, tool output)
  3. Append touched-files list
  4. Open $EDITOR with extracted text (blocking)
  5. Store edited text in self.edited
  6. Return SubmitResult::Prepared

submit():
  1. Read edited draft from self.edited
  2. Set AsyncOp::TreeCompact { target_id, mode: TextOnly, payload: draft }
```

### Prompt mode

For prompt mode, the LLM call is async — the entire flow (LLM + editor
+ write) happens in the `AsyncOp` handler. No `prepare()` needed.

```
submit():
  1. Read the custom prompt from path_args.suffix
  2. Set AsyncOp::TreeCompact { target_id, mode: Prompt, payload: prompt }
```

**Agent loop handler:**
1. Compute `tokens_before` from current scoped context
2. Run `run_compaction()` with the prompt (this calls `agent.summarize()`)
3. The `run_compaction` flow already opens `$EDITOR` for review via the
   `CompactionSummaryReview` callback
4. Writes the compaction entry

### AsyncOp::TreeCompact handler in agent.rs

Port the existing `/tree <id> compact <mode>` handler from the
`InputAction::Submit` match arm. The handler reads `target_id`, `mode`,
and `payload` from the AsyncOp and runs the same `run_compaction()` flow.

For Manual and TextOnly modes, the draft is already in `payload`
(edited via editor in prepare()). The handler passes it directly to
`run_compaction()`.

For Prompt mode, `payload` is the LLM prompt. The handler calls
`run_compaction()` with `CompactSummaryMode::Llm` and the prompt.

---

## Rendering

### Tree listing rows

Each tree entry renders as a single row with:
- **Description column:** Indented label (full message text for user/assistant
  entries, `[COMPACT]` prefix for compaction entries, `[MODEL]` for model
  changes, `[THINK]` for thinking level changes)
- **No wrap:** Long messages are truncated in display but available for
  fuzzy search via the full-text description

Indentation follows the existing v1 convention:
- +2 spaces for assistant responses within a user turn
- +4 spaces at branching points (fork in the tree)
- Branch depth tracked for future visual distinction

### Entry header (when drilled)

`header()` shows the full message body wrapped to terminal width:
- User messages: full text, multi-line
- Assistant messages: text content only (no reasoning blocks, no tool calls)
- Compaction: `[COMPACT]` + summary text
- Model change: `model: provider/model_id`
- Thinking change: `thinking: level`

---

## File plan

| File | Action | Description |
|------|--------|-------------|
| `src/slash_menu_bridge.rs` | Modify | Replace `tree` with `raw_tree: Rc<RefCell<Option<Rc<Vec<SessionTreeNode>>>>>` in `MenuShared` |
| `src/v25_tree.rs` | New | `TreeListNode`, `TreeSwitchNode`, compact nodes |
| `src/v25_menu.rs` | Modify | Wire tree subtree into root menu |
| `src/lib.rs` | Modify | Register `v25_tree` module |
| `src/agent.rs` | Modify | Add `AsyncOp::TreeSwitch`, `AsyncOp::TreeCompact`, `CompactMode`; change `ReplCtx.tree` to `Rc<Vec<SessionTreeNode>>` |
| `src/repl.rs` | Modify | Sync `raw_tree` to `MenuShared` (O(1) Rc clone), remove v2.0 tree flatten sync |
| `doc/slashdot/tree_design.md` | Update | Replace with this plan |

---

## Phasing

### Phase 1: Tree listing + Switch

1. Replace `MenuShared.tree` with `raw_tree: Rc<...>`, change `ReplCtx.tree` to `Rc<Vec<SessionTreeNode>>`
2. Update `TreeProvider` (v2.0) to read from `raw_tree` (or remove if
   v2.0 tree path is dead)
3. Implement `TreeListNode` with data caching (`Rc::ptr_eq` on tree Rc = freshness)
4. Implement `TreeSwitchNode` (leaf, AsyncOp::TreeSwitch)
5. Add `AsyncOp::TreeSwitch` handler in agent.rs
6. Wire into v25_menu.rs
7. Tests: listing rows, caching behavior, header content, switch submit,
   full traversal, active entry marking

### Phase 2: Compact (all three modes) ✅

1. ✅ `CompactMode` enum in `slash_menu_bridge.rs` (Manual, TextOnly, Prompt)
2. ✅ `AsyncOp::TreeCompact` handler in agent.rs (ported from existing
   `/tree <id> compact` handler)
3. ✅ `TreeCompactManualNode` (submit sets AsyncOp::TreeCompact)
4. ✅ `TreeCompactTextNode` (submit sets AsyncOp::TreeCompact)
5. ✅ `TreeCompactPromptNode` (submit reads suffix as prompt)
6. ✅ 8 tests: submit for each mode, full traversal, header content,
   empty suffix

**Note:** Editor integration (`prepare()` opens `$EDITOR`) is deferred —
the nodes are single-phase (prepare delegates to submit). The agent loop
handler does the full compaction flow (validation, agent build, review
gate, `run_compaction()`).

---

## Test plan

### Phase 1 tests

- `tree_listing_shows_entries` — empty tree shows "(no session)"
- `tree_listing_shows_full_text` — user messages have full text in desc
- `tree_listing_caching_skips_rebuild` — same Rc returned when tree unchanged
- `tree_listing_caching_rebuilds_on_change` — new Rc when tree changes
- `tree_entry_header_shows_message_body` — drilled entry shows full text
- `tree_switch_submit_sets_async_op` — switch sets correct target_id
- `tree_switch_full_traversal` — listing → drill → switch → Enter → AsyncOp
- `tree_active_entry_marked` — current leaf is marked active in listing

### Phase 2 tests

- `compact_manual_prepare_opens_editor` — manual mode opens editor, stores draft
- `compact_manual_submit_sets_async_op` — manual sets correct op with draft
- `compact_text_only_prepare_opens_editor` — text-only opens editor
- `compact_text_only_submit_sets_async_op` — text-only sets correct op
- `compact_prompt_submit_sets_async_op` — prompt sets op with suffix
- `compact_full_traversal` — listing → drill → compact → manual → Enter
- `compact_esc_cancels_confirm` — Esc during confirm discards draft
- `compact_no_session_shows_error` — compact with no session
- `compact_unknown_entry_shows_error` — compact at invalid target

---

## Deferred / follow-ups

### `switch --summarize`

Summarize abandoned branch when switching. Reuses the same compaction
logic — the abandoned span is the branch from the old leaf to the common
ancestor. Deferred because it depends on the compaction infrastructure
being fully ported first. Same general idea as compact at a point, just
a different span to cover.

### `/compact` root shortcut

The existing `/compact` from root rewrites to `/tree <last-user> compact`.
Deferred until finalization of `doc/slashdot/prompt_rewrite_design.md`
(prompt pre-filling / customization in `/tree id compact prompt`).

### Box-drawing characters

The design doc proposes box-drawing characters for branch visualization.
Deferred — simple indentation (matching v1) is sufficient for the
initial port.
