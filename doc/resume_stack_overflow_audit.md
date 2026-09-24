# Audit: Session Loading & Replay — Stack-Overflow Risk

Date: 2025-07-08

Symptom: `/session <id> resume` on larger sessions sometimes produces stack
overflows in debug builds.

## 1. ROOT CAUSE (high severity): Recursive tree traversal in REPL overlay rendering

The `SessionIndex::build_subtree` method was already fixed to be iterative
(see `src/session/types.rs:512-513`), but **five downstream consumers of
`SessionTreeNode` still recurse directly on the tree's child chain**. In a
typical (non-branching) session the tree is a degenerate linked-list: every
entry has exactly one child, so **tree depth ≈ number of entries**.

| Function | File:Line | Recursion depth |
|---|---|---|
| `collect_labels` | `src/repl.rs:4381` | tree depth |
| `flatten_turns` | `src/repl.rs:4482` | tree depth |
| `collect_active_path` | `src/repl.rs:5755` | tree depth |
| `walk` (inside `last_user_message_id_on_active_branch`) | `src/repl.rs:4429` | tree depth |
| `walk` (inside `find_parent_id_in_tree`) | `src/repl.rs:4021` | tree depth |

**Why this triggers on `/session <id> resume` specifically:** the resume
handler (`handle_session_command` in `src/agent.rs:2600`) sets
`*session = Some(new_session)` and returns `Switched`. The **very next
iteration** of the `run_interactive` loop (`src/agent.rs:3640`) calls
`build_session_ctx(&session).await`, which builds the tree via `get_tree()`
→ `build_tree()` → the iterative `build_subtree`. The returned
`Vec<SessionTreeNode>` is stored in `ReplCtx` and passed into `read_input`.
Inside `read_input`, the session overlay rendering calls `flatten_tree_nodes`
(`src/repl.rs:746`, `872`, `3996`, `4167`, `4287`), which immediately
invokes the recursive `collect_labels` + `flatten_turns`. Other overlay
paths (`/compact`, `/tree`, prev/next navigation) also hit
`collect_active_path` and the `walk` helpers.

**Concrete blowup estimate:** each recursive frame is ≈200–500 bytes in a
debug build (no inlining, no dead-variable elimination, full debug info). A
session with 10,000 entries (≈5,000 turns) produces a tree depth of
≈10,000. Two nested recursions (`collect_labels` then `flatten_turns`,
called sequentially but both touching the same deep chain) consume ≈4–10 MB
of stack — exceeding the default 8 MB thread stack on Linux. Larger sessions
make it worse; `collect_active_path` or the `walk` helpers add a third
traversal for overlay sub-commands.

### Recommended fix

Make `collect_labels`, `flatten_turns`, `collect_active_path`, and the
`walk` helpers iterative (explicit stack or queue), matching the pattern
already established by `build_subtree`.

## 2. Double `get_branch()` in `build_session_ctx` (medium severity, performance)

Every iteration of the main REPL loop calls `build_session_ctx`
(`src/agent.rs:2018`), which:

1. Calls `s.get_branch().await` directly at line 2030 to build
   `branch_messages`.
2. Then calls `s.build_context().await` at line 2060, which **internally
   calls `self.get_branch().await` again** at `src/session/session.rs:826`.

Each `get_branch()` call walks the `parent_id` chain via `get_path_to_root`
and **clones every `SessionEntry`** (including its `serde_json::Value`
payload) into a new `Vec`. For a 10K-entry session, this produces two
independent `Vec<SessionEntry>` allocations per loop tick, each with 10K
`serde_json::Value` clones. Not a stack issue, but it roughly doubles the
peak memory and CPU of every idle REPL iteration.

### Recommended fix

Either pass the branch from `build_session_ctx` into `build_context` (adding
a parameter or caching it in a `RefCell`/`OnceCell` on `Session`), or have
`build_context` accept a pre-fetched branch.

## 3. `read_all()` loads the entire JSONL file into a single `String`
(medium severity, memory)

`JsonlSessionStorage::build_index` (`src/session/storage.rs:232`) calls
`read_all()` which does `tokio::fs::read_to_string`. For a large session
(e.g. 10K turns with verbose tool results), this can be a hundreds-of-MB
string. It is then split into a `Vec<&str>` (line 244) and iterated to
populate a `HashMap<String, SessionEntry>`. All three representations — the
raw `String`, the `Vec<&str>` references, and the `HashMap` with cloned
`serde_json::Value`s — coexist in memory simultaneously. There is no
streaming/chunked parse.

### Recommended fix

Consider a line-by-line streaming reader (e.g. `BufReader` over the file)
that deserialises each line and discards the raw text immediately, avoiding
the peak of (raw file + line vec + entry map) in memory at once.

## 4. `get_branch()` clones every entry even when only the tail is needed
(low–medium severity, performance)

`build_replay_units` (`src/agent.rs:2680`) receives the **full** branch and
walks every entry, even though only the last N turns are replayed (controlled
by `session_resume_replay_turns`, default 3). The full branch is materialised
at line 2679 (`new_session.get_branch().await`), all entries are cloned, and
then `replay_last_turns` only renders a suffix. Similarly,
`compute_resume_model_prefill` at line 2523 calls
`session.get_branch().await` and scans from the tail — the head entries are
cloned and immediately discarded.

### Recommended fix

Add a `get_tail_branch(n)` or similar that walks only the last N parent
hops, avoiding the full clone. Alternatively, `get_path_to_root` could
return an `Cow` or `Arc` wrapper to share entries instead of cloning.

## 5. No recursive patterns in the core session code (positive finding)

The following core paths are **correctly iterative**:

- `SessionIndex::get_path_to_root` (`src/session/types.rs:436`) —
  loop-based parent chain walk.
- `SessionIndex::build_subtree` (`src/session/types.rs:523`) — manual stack
  (`Vec<Frame>`), explicitly documented as iterative to avoid stack overflow.
- `build_replay_units` (`src/agent.rs:2226`) — `for entry in branch` loop.
- `replay_units` / `render_turn` (`src/agent.rs:2429`, `2376`) — iteration
  over units/segments.
- `build_index` (`src/session/storage.rs:231`) — line-by-line iteration.
- `apply_compaction_transforms` (`src/session/session.rs:1344`) —
  single-pass filter.

## Summary

| # | Issue | Severity | Category |
|---|---|---|---|
| 1 | Recursive tree traversal in REPL overlay | **High** | Stack overflow |
| 2 | Double `get_branch()` per loop iteration | Medium | Performance / memory |
| 3 | Full-file `read_all()` in `build_index` | Medium | Memory |
| 4 | Full-branch clone for short replay window | Low–medium | Performance / memory |

The **stack overflow on resume of larger sessions** is caused by recursive
tree traversal in the REPL overlay rendering layer (`src/repl.rs`), not in
the session loading or replay logic itself. The tree is built iteratively but
consumed recursively. The fix is to make `collect_labels`, `flatten_turns`,
`collect_active_path`, and the `walk` helpers iterative, matching the
pattern already established by `build_subtree`.
