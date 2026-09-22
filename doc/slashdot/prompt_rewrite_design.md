# Slash menu v2.5 prompt rewrite on drill

## Motivation

Several commands need to rewrite the prompt when a parent listing drills
into a child node. The rewrite must fire on drill (Tab/Enter from a
listing) but NOT when the user manually types the same path and presses
Enter/Tab.

| Command | Rewrite | Type |
|---------|---------|------|
| `/session ` + Tab | `/session .path:.` | Listing: add default filter |
| `/set tool_call_limit` + Tab | `/set tool_call_limit 42` | Leaf: pre-fill current value |
| `/compact ` + Tab | `/tree <id> compact` | Listing: redirect to different command |
| `/tree <id> compact prompt` + Tab | `/tree <id> compact <default>` | Leaf: pre-fill default prompt |
| `/session <id> rename` + Enter | `/session <id> rename MySession` | Leaf: pre-fill name, avoid submit |

**Must not trigger** when the prompt is entered manually (e.g.
backspacing from `/session a` to `/session ` then pressing Tab).

---

## Design

### New trait method

```rust
// On SlashDotNode (default impl returns None):
fn drill_rewrite(
    &mut self,
    path: &[String],
    prompt_suffix: &str,
) -> Option<String>
```

- Called in `node_traverse`, **after epoch gate, before parse** (Phase 0.5).
- Only called when the key is `DrillEnter` or `DrillTab` (engine-internal).
- Returns `Some(new_full_prompt)` to rewrite, or `None` for default behavior.
- `&mut self` gives full access to shared state (`MenuShared`, settings, etc.).
- `path` is the node's position in the tree. `prompt_suffix` is the raw
  prompt section.

**Level rule:** the rewrite must live on the node at the level the user
drills *into* — the node that provides the typed command's own row. For
`/session`, that's the parent listing (a standalone `SessionListingNode`),
not the dynamic `SessionListNode` one level below.

### Trigger: DrillEnter / DrillTab keys

Rename `MaybeSubmit` → `DrillEnter` (global rename, same semantics).
Add `DrillTab`. These are only sent by parent listings during drill:

```rust
// In handle_listing:
HostKey::Tab  → child.node_traverse(epoch, DrillTab, ...)
HostKey::Enter → child.node_traverse(epoch, DrillEnter, ...)
```

Key forwarding map (unchanged recursive path at line 308):

| Source | Key received by child |
|--------|----------------------|
| Parent listing Tab drill | `DrillTab` |
| Parent listing Enter drill | `DrillEnter` |
| User types path + Tab | `Tab` (forwarded) |
| User types path + Enter | `Enter` (forwarded) |
| No key action (re-eval) | `None` |

Drill keys are **never forwarded recursively** — each level of
`handle_listing` creates fresh Drill keys for its own children.

### Traversal flow

```text
Phase 0:   epoch gate
Phase 0.5: if key ∈ {DrillEnter, DrillTab}:
             if let Some(new_prompt) = self.drill_rewrite(path, suffix, key) {
               return MenuResult::Continue(RenderPlan::default(), new_prompt);
             }
Phase 1:   parse section
Phase 2:   refresh data
Phase 3:   restore trigger
Phase 4:   dispatch (listing / leaf)
```

The early return aborts traversal before any parse or data refresh.
The REPL receives `MenuResult::Continue`, updates the editor text
to `new_prompt`, and the next frame re-evaluates from root with
`HostKey::None` (no drill key → no rewrite → normal rendering).

### Return value: full prompt string

`drill_rewrite` returns a **complete prompt string** (e.g.
`"tree <id> compact"` or `"session .path:."`), not just a suffix.
This enables cross-command redirects (e.g. `/compact` → `/tree`).

Convenience: add `PathArgs::full_prompt(&self) -> String` that
reconstructs `"path.join(\" \") + suffix"`, so implementors can
build the prompt from a `PathArgs` they construct internally.

### Leaf behavior with drill_rewrite

In `handle_leaf`, when key is `DrillEnter`:

```rust
DrillEnter => {
    if let Some(new_prompt) = self.inner.drill_rewrite(path, prompt_suffix, key) {
        return MenuResult::Continue(RenderPlan::default(), new_prompt);
    }
    // No rewrite — fall through to normal prepare()/submit().
    // (Same as current MaybeSubmit behavior.)
    ...
}
```

When `drill_rewrite` returns `Some`: **show header, do not submit**.
When `drill_rewrite` returns `None`: proceed with normal submit logic.

This gives the leaf control: `/session <id> rename` pre-fills the
name and shows the header; a leaf without `drill_rewrite` submits
as before.

### Parent restore state

When a parent listing drills, it sets `epoch.should_restore_view_args = true`.
If the child's `drill_rewrite` aborts and restarts, the parent's restore
state is **intentionally not cleared**. On re-traversal:

- The rewritten prompt has a non-empty tail → parent's restore doesn't fire.
- When the user backspaces to the bare ID (empty tail, no trailing space),
  restore fires correctly, returning to the pre-drill view.

This is the correct behavior: backing out of a rewrite returns to the
listing the user drilled from.

---

## Implementation notes

### Status

**All 5 use cases implemented.** Core infrastructure (`DrillEnter`/`DrillTab`,
`drill_rewrite` Phase 0.5) plus all command implementations:

| Command | Status | Test |
|---------|--------|------|
| `/session ` + Tab → `.path:.` | Done (on `SessionListingNode`) | `session_list_drill_rewrite_adds_path_filter` |
| `/set tool_call_limit` + Tab → prefill value | Done | `setting_leaf_drill_rewrite_prefills_value` |
| `/compact ` + Tab → `/tree <id> compact ` | Done | `compact_node_drill_rewrite_redirects_to_tree` |
| `/tree <id> compact prompt` + Tab → prefill default | Done | `compact_prompt_drill_rewrite_fills_default` |
| `/session <id> rename` + Enter → prefill name | Done | `session_rename_drill_rewrite_prefills_name` |

Engine tests: 4 (fires on drill, doesn't fire on manual, None falls through).
Total: 628 lib tests.

### StaticListingNode stays simple

`StaticListingNode` keeps its default `drill_rewrite` (returns `None`).
Commands that need a rewrite use a regular `impl SlashDotNode` with
their own state (shared references, cached values, etc.). This avoids
over-engineering the convenience wrapper for the 90% case.

### Infinite loop guard: none needed

After a rewrite, the REPL re-evaluates with `HostKey::None`. Since
`None` is not `DrillEnter`/`DrillTab`, `drill_rewrite` never fires
on the re-traversal. No counter or same-prompt check needed.

### Interactive rewrites vs config-driven

Some rewrites are config-driven (pre-fill from settings) and could
be declared on the node statically. Others are interactive (the
`/compact` redirect reads the session tree). The `&mut self` signature
supports both: the node decides what to query in its `drill_rewrite`
implementation.

---

## Application roll-out

All complete. Implementation order was by complexity:

1. ~~**Simple leaf pre-fills**~~ **Done.** `/set tool_call_limit`,
   `/tree <id> compact prompt`, `/session <id> rename`.

2. ~~**Listing filter**~~ **Done.** `/session ` → `.path:.`.

3. ~~**Cross-command redirect**~~ **Done.** `/compact` → `/tree <id> compact `
   (new `CompactNode` root leaf, registered in `build_menu`).

---

## Key changes summary

| File | Change |
|------|--------|
| `crates/slashdot_menu/src/node_impl.rs` | Rename `MaybeSubmit` → `DrillEnter`, add `DrillTab`. Add `SlashDotNode::drill_rewrite()` default method. |
| `crates/slashdot_menu/src/node_entry.rs` | Phase 0.5 in `node_traverse`: check drill key, call `drill_rewrite`, early return. Update `handle_listing` Tab/Enter to send Drill keys. Update `handle_leaf` DrillEnter branch. |
| `src/v25_session.rs` | New `SessionListingNode` (standalone SlashDotNode) with `drill_rewrite` → `.path:.` filter. `SessionRenameNode::drill_rewrite` → prefill name. |
| `src/v25_set.rs` | `SettingLeafNode::drill_rewrite` → prefill current value. |
| `src/v25_tree.rs` | `CompactNode` (new root leaf): drill → `/tree <id> compact `, submit → NewPath redirect. `TreeCompactPromptNode::drill_rewrite` → prefill default prompt. |
| `src/v25_menu.rs` | Register `CompactNode` in root menu. |
