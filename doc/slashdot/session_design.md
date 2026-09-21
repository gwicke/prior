# /session implementation plan

## Overview

/session provides the entry point for listing sessions and performing actions
on them. Actions include resuming (with replay), forking, renaming, deleting.

Shape per v2.5 design §10: **Dynamic → static action children → confirm on
delete (§9).** Same pattern as `/tree`.

---

## Node tree

```
root (StaticListingNode, empty)
  └── session (StaticListingNode)
        row: "session" / "list, rename, resume, fork, or delete sessions"
        columns: [id, description, msgs, age]
        └── SessionListNode (SlashDotNode)
              data() → one row per session (from repo.list_sessions_sync)
              header() → session info table (when drilled into a session)
              children:
                ├── SessionResumeNode (leaf)
                ├── SessionForkNode (leaf)
                ├── SessionRenameNode (leaf)
                └── SessionDeleteNode (leaf, prepare() for confirm)
```

### Merged child set at `/session` level

The "session" listing shows the concatenated rows of all its children. The
only child that returns rows is `SessionListNode` — its `data()` returns
one row per session from the repo. So the listing is the session list.

### Drilling into a session

When the user selects a session UUID (Tab/Enter), the engine drills into
`SessionListNode`. Now `SessionListNode` acts as a listing node: its own
children (resume/fork/rename/delete) are shown. `SessionListNode::header()`
renders the session info table for the drilled session.

### Drilling into an action

When the user selects an action (e.g. "resume"), the engine drills into the
action leaf node. `header()` renders action-specific help text. Enter triggers
`prepare()` → `submit()` (with confirm for delete).

---

## Row format

**Row ID**: Always the session UUID (not the name — names can contain spaces).

**Row description**: `"<name> — <first_prompt>"` when a name is set;
`<first_prompt>` alone when no name is set. First prompt is truncated to ~60
chars.

**Fields** (for table columns):

| Field | Type | Source | Description |
|-------|------|--------|-------------|
| id | string (implicit) | `meta.id` | Session UUID |
| description | string (implicit) | name + first prompt | Display text |
| msgs | number | `meta.user_message_count` | Turn count |
| age | string | `meta.last_prompt_timestamp` or `meta.timestamp` | Relative time ("2h ago") |

**Active row** (`*` marker): The current session (from `shared.current_session_id`).

---

## Columns on the parent listing

Declared on the "session" `StaticListingNode`:

| Field | Header | Align | Min | Priority |
|-------|--------|-------|-----|----------|
| id | Session | Left | 10 | 3 |
| description | Description | Left | 20 | 2 |
| msgs | Turns | Right | 5 | 1 |
| age | Age | Right | 8 | 1 |

---

## Node implementations

### SessionListNode

**File**: `src/v25_session.rs`

```rust
pub struct SessionListNode {
    pub shared: Rc<MenuShared>,
    cached_sessions: Option<Rc<DataResponse>>,
}
```

**`data()`**: Reads `shared.repo.list_sessions_sync(None)`, builds rows.

**`header()`**: Called when drilled into a session. Looks up the session
UUID from `path_args.ids` (the drilled row ID). Loads the session metadata
from the repo (same as `StatNode`). Renders:
1. Table row: session name/id, turns/messages, age, parent/dir
2. Detail rows: first prompt timestamp + text, last prompt timestamp + text

If the UUID doesn't match any session, returns a "Session not found" error
line.

**`columns()`**: Returns the column definitions for the table header.

### SessionResumeNode

Leaf node. `data()` returns a single row: `("resume", "continue the session")`.

**Guard**: `data()` checks if the drilled session UUID equals
`shared.current_session_id`. If so, returns empty rows (resume is hidden
for the current session).

**`header()`**: Returns help text:
```
Resume the session and replay the last N turns.
Optional length limits: 10 (turns), 10t (turns), 10l (lines).
Type the limit after "resume" and press Enter.
```

**`submit()`**: Sets `AsyncOp::Resume { session_id, replay_arg }` on
`shared.pending_op`. The `session_id` comes from `path_args.ids`
(the drilled UUID). The `replay_arg` comes from `path_args.suffix.trim()`.

Returns `SubmitResult::Done(None)` — the menu closes and the agent loop
picks up the async op.

### SessionForkNode

Leaf node. `data()` returns a single row: `("fork", "branch from this point")`.

**`header()`**: Returns help text:
```
Fork this session into a new one in the current directory.
The new session starts with the same history up to this point.
```

**`submit()`**: Sets `AsyncOp::Fork { session_id }` on `shared.pending_op`.
Returns `SubmitResult::Done(None)`.

### SessionRenameNode

Leaf node. `data()` returns a single row: `("rename", "rename the session")`.

**`header()`**: Returns help text showing the current session name:
```
Rename the session. Type the new name and press Enter.
Leave empty to clear the name.
Current name: <name or "(unnamed)">
```

**`submit()`**: Reads `path_args.suffix.trim()` as the new name.

- If **empty**: clears the name. Sets `AsyncOp::Rename { session_id, new_name: "" }`.
- If **non-empty**: renames to that value.

Both paths set `AsyncOp::Rename` on `shared.pending_op` and return
`SubmitResult::Done(None)`.

### SessionDeleteNode

Leaf node with `prepare()` override for two-phase confirm.

**`data()`**: Returns a single row: `("delete", "delete from disk")`.

**Guard**: If `session_id == shared.current_session_id`, `data()` still
returns the row (user should see it exists), but `prepare()` returns an
error message: "Cannot delete the current session."

**`header()`**: Returns help text:
```
Permanently delete this session and its history.
This action cannot be undone.
```

**`prepare()`**: Returns `SubmitResult::Prepared` — the engine renders
the yea/nay confirm dialog (§9). If this is the current session, returns
`Done(Some("Cannot delete the current session"))` instead.

**`submit()`**: Sets `AsyncOp::Delete { session_id }` on
`shared.pending_op`. Returns `Done(None)`.

---

## AsyncOp extensions

```rust
pub enum AsyncOp {
    Replay(String),
    Resume { session_id: String, replay_arg: String },
    Fork { session_id: String },
    Rename { session_id: String, new_name: String },
    Delete { session_id: String },
}
```

---

## Agent loop dispatch

The agent loop (in `run_interactive`, `agent.rs`) already checks
`pending_op` after menu submit. Extend the match:

```rust
if let Some(op) = pending_op.borrow_mut().take() {
    match op {
        AsyncOp::Replay(arg) => { /* existing */ }
        AsyncOp::Resume { session_id, replay_arg } => {
            // Find session meta by id from repo
            // Delegate to existing resume logic (open/fork/replay/switch)
            // Set current_session_id, prefill
        }
        AsyncOp::Fork { session_id } => {
            // Find session meta by id from repo
            // Fork to current cwd, switch session
            // Set current_session_id
        }
        AsyncOp::Rename { session_id, new_name } => {
            // If session_id == current session, use live session object
            // Otherwise, open session from repo, set name, close
            // Invalidate repo cache
        }
        AsyncOp::Delete { session_id } => {
            // Find session meta by id, delete via repo
            // If deleted the current session, set session = None
            // Invalidate repo cache
        }
    }
    continue;
}
```

Key: the agent loop already has `session`, `repo`, `config`, `settings`,
`current_session_id`, and `pending_prefill` in scope. All session mutations
happen here, where the full async context is available.

---

## MenuShared additions

No new fields needed. The existing fields are sufficient:
- `repo` — session listing, fork, delete
- `current_session_id` — current-session guards, active row
- `pending_op` — async dispatch
- `settings.session_resume_replay_turns` — default replay count

---

## Wiring in v25_menu.rs

```rust
// Session
{
    let mut sl: SlashDotMenuNode = StaticListingNode::new(&[
        StaticRow::new("session", "list, rename, resume, fork, or delete sessions"),
    ])
    .with_columns(session_columns())
    .into();
    sl.add_child(SessionListNode { shared: shared.clone() }.into());
    root.add_child(sl);
}
```

`SessionListNode` is the child whose `data()` returns session rows and
whose own children are the action nodes. The action nodes are added
inside `SessionListNode`'s constructor (or in a helper function) to keep
the tree build clean.

---

## Removal of v2.0 code

After the v2.5 session nodes are working:
1. Remove `ActiveOverlay::Session` variant
2. Remove `ActiveOverlay::SessionAction` variant
3. Remove `ActiveOverlay::Confirm` variant (delete confirm is now §9)
4. Remove `session_action_overlay_entries()` and related v2.0 methods
5. Remove `SessionProvider` (replaced by `SessionListNode::data()`)
6. The `handle_session_command()` function in agent.rs stays — its logic
   is reused by the AsyncOp dispatch. The string-parsing entry point is
   removed (no longer called from the REPL input path).

---

## Prerequisites: traversal context in NodeEntry

The action nodes (resume, fork, rename, delete) need the traversal path
to function:

- `SessionResumeNode::data()` needs the drilled UUID to check the
  current-session guard.
- `SessionListNode::header()` needs the UUID to load session metadata.
- `SessionRenameNode::header()` needs the UUID to show the current name.
- `SessionDeleteNode::prepare()` needs the UUID to check the guard.
- All action `submit()` handlers need the UUID for the `AsyncOp`.

Currently `refresh_data()` constructs empty `PathArgs` for children,
and `build_render_plan()` / `plan_from_cache()` construct empty `PathArgs`
for `header()`. Three call sites affected:

1. `refresh_data` (line 520–524): `PathArgs { ids: vec![], suffix: "", ... }`
2. `build_render_plan` (line 830–835): `PathArgs { ids: vec![], suffix: "", ... }`
3. `plan_from_cache` (line 931–936): `PathArgs { ids: vec![], suffix: "", ... }`

### Two PathArgs contexts

The traversal has two distinct PathArgs contexts:

1. **Current-node context** (`build_path_args` in `handle_leaf`): used by
   `prepare()`/`submit()`. The `path` includes the current node's own ID
   (pushed by parent at drill time, line 307/457) plus local modifiers
   from the parsed section. The same `path` is the right input for
   `build_render_plan`/`plan_from_cache` when calling `header()` — the
   current node needs its own ID for context (e.g. `SessionListNode` needs
   the UUID to load session metadata).

2. **Parent-calling-child context** (`refresh_data`): the parent building
   its listing. The parent's `path` does NOT include the child's own ID —
   that's a row ID from `row_index`, matched at the parent level. When
   `refresh_data` calls `child.node_data()`, the `PathArgs.ids` should be
   the **parent's** `path`. For action children of `SessionListNode`, this
   means `path_args.ids = ["session", uuid]` — they can read the UUID for
   guards and AsyncOp.

Both contexts use the same `path` value from `node_traverse` — the
current node's path (parent path + own ID). `refresh_data` passes this
to children (parent context for them), and `build_render_plan` passes it
to `header()` (current-node context for self).

### Fix: thread path through function parameters

1. Change `refresh_data(&mut self, epoch)` to
   `refresh_data(&mut self, epoch, path: &[String], suffix: &str)`.
   When calling `child.node_data()`, build PathArgs from `path` + `suffix`.

2. Change `build_render_plan(&mut self)` to
   `build_render_plan(&mut self, path: &[String], suffix: &str)`.
   When calling `self.inner.header()`, build PathArgs from `path` + `suffix`.
   Same for `plan_from_cache`.

3. Update all callers:
   - `node_traverse` (line 264): `self.refresh_data(epoch, &path, prompt_suffix)`
   - `node_data` value-forwarding (line 197): `self.refresh_data(epoch, &path_args.ids, &path_args.suffix)`
   - `handle_leaf` (lines 378, 420): `self.plan_from_cache(path, prompt_suffix)`
   - `handle_listing` (line 474): `self.build_render_plan(path, prompt_suffix)`
   - Restore path (line 290): `self.build_render_plan(&path, &restore_to)`
   - Test sites: pass `&[]` and `""` (existing behavior).

This does NOT change the `SlashDotNode` trait — only internal `NodeEntry`
method signatures.

---

## Implementation steps

0. **Traversal context in NodeEntry**: thread path + suffix through
   `refresh_data`, `build_render_plan`, `plan_from_cache`. (Prerequisite
   for steps 1–2.) **Done** (`2df0332`).
1. **Create `src/v25_session.rs` + extend `AsyncOp`**: `SessionListNode`
   with `data()` + `header()` + `columns()`, action nodes (Resume, Fork,
   Rename, Delete), and `AsyncOp` variants. Agent loop stubs.
   **Done** (`4c94dfb`).
2. **Wire in `v25_menu.rs`**: build the session subtree, register in root.
   **Done** (`98ead98`).
3. **Agent loop dispatch**: extend the `pending_op` match in `agent.rs`
   to handle the new `AsyncOp` variants, reusing the existing
   `handle_session_command` logic. **Done** (`dd41348`).
4. **Remove v2.0 session overlay code**: strip `ActiveOverlay::Session`,
   `SessionAction`, `Confirm` and related methods from `repl.rs`.
5. **Tests**: unit tests for each node (`data()`, `submit()`, `header()`,
   guards), integration test for full traversal (listing → drill → action
   → submit).

---

## Findings / changes during implementation

### Action-node path semantics (found during manual testing)

An action node's own path is `["session", "<uuid>", "<action>"]`, so the
session UUID is the **second-to-last** element — `ids.last()` returns the
action name ("resume", …), which caused `Session not found: resume`.

Two contexts, two helpers:

- `drilled_session_id()` = `ids.last()`. Used by `SessionListNode::header()`
  and by action `data()` — the latter because `refresh_data` calls a child's
  `data()` with the *parent's* path (`["session", "<uuid>"]`).
- `action_session_id()` = `ids[len-2]`. Used by action `header()` /
  `submit()` / `prepare()` — these receive the node's own path.

Locked in by `path_id_helpers_pick_the_right_element` and the end-to-end
`session_resume_full_traversal_sets_op_with_uuid` (builds a real temp repo,
drills session → resume through the engine, asserts the `AsyncOp` carries
the UUID).

### Menu submit flow (found during manual testing)

The agent loop originally dispatched `pending_op` **inside** the
`InputAction::Submit` arm, and the v2.5 `DoneSuccess` handler committed
the command text and then `continue`d inside `read_input` (staying in the
overlay). Net effect: a menu action's `submit()` set `pending_op`, but the
op was never dispatched — resume/replay silently did nothing.

Fix (commit `b747728`):

- `read_input` returns a new `InputAction::MenuDone` on `DoneSuccess` /
  `DoneError`, dismissing the overlay.
- The agent loop runs the `menu_submitted` LiveState sync **and** the
  `pending_op` dispatch **before** the `action` match, so they fire for
  `MenuDone` as well.
- The duplicated sync/dispatch block inside the `Submit` arm was removed.

This also makes `/thinking` / `/model` / `/persona` apply immediately
(previously they relied on a later `read_input` return to sync LiveState).

### Listing vs action columns

`session_columns()` was used for both the parent `/session` listing and
`SessionListNode`'s drilled action list. Split into:

- `session_listing_columns()` — parent listing, no `id` column (UUIDs not
  useful).
- `session_action_columns()` — `SessionListNode` drilled, `id` (Action) +
  description.

### Active marker

`SessionListNode::data()` marks the current session active via
`active_ids`, but only when that session actually appears in the listing.

---

## Later refinements

- Default filter added from `/sess`+tab: `/session .path:.` — current
  directory. Requires `prompt_rewrite_design.md`.
- Prefill for rename (buffer manipulation from submit).
- Path filter support (`.path:` modifier on session listing).
