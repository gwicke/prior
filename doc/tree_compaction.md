# `/tree` Compaction — In-Session Compaction Entry

## Status

**Phases 1–5 implemented.** Extends the existing `/tree` navigator (see
`doc/tree.md`) so that selecting a tree point can append a **compaction entry**
to the *current* session, prefix-limiting the agent context at that point —
instead of forking a new session. The session file stays in-place and remains
bidirectionally compatible with pi's session format.

**Scope:** `truncate` (empty `summary`) and `text-only` (deterministic
text summary) compaction. LLM-generated summarized compaction remains future
work (Phase 6).

**Text-only mode** (`/tree <entry-id> compact text-only`) builds a summary
from the dropped span containing every **user prompt**, the assistant's
**text responses per turn**, and a trailing **files-touched list**
(`read`/`write`/`edit` paths, modified files marked) — no reasoning, no tool
calls, no tool output. Built deterministically by
`crate::session::build_text_only_summary` (no LLM), written as the
`Compaction` entry's `summary`, rendered as markdown right after the
compaction, and replayed at the compaction marker in `/replay`. Per the pi
contract it reaches the LLM as the FIRST context message via the
`compactionSummary` wrapper (`apply_compaction_transforms`).

**ID syntax:** a tree point is referenced as `/tree <entry-id>` (angle
brackets). A bare `/tree someterm` remains a fuzzy **search** over the entry
list — this preserves the existing content filtering while making id targets
unambiguous.

## Goal

Today, `/tree` lets the user select a tree point and `<Enter>` **switches** the
session's leaf pointer (via `InputAction::NavigateTree`). We want:

1. After selecting a tree point, show an **action menu overlay**: `switch`
   (today's behavior) and `compact`.
2. `switch` opens a **sub-menu** offering `just switch` and `summarize`
   (summarize = the existing abandoned-branch summarization, committed as
   `/tree <entry-id> switch --summarize`). `compact` opens a **sub-menu** for
   summarization choice, offering only `truncate` (drop the prefix, keep no
   summary) for now (more options later).
3. Choosing `compact` appends a pi-compatible `Compaction` entry at the end of
   the current session:
   - `summary = ""` (truncate mode),
   - `firstKeptEntryId = <selected tree point id>`,
   - `tokensBefore = <estimated tokens of the current context>`.
   The compaction entry becomes the new leaf; **subsequent turns are appended
   after it** in the same session.
4. The **agent session context** is rebuilt to pi's expectation:
   `<summary (from cmp entry, if available)><first message>…<last message>`.
   The compaction entry itself is never sent to the LLM; with an empty summary
   the context is simply `<first message>…<last message>`.
5. **Replay** (`/replay`, resume replay) shows compaction entries as visible
   markers but continues **all the way to the beginning** (full branch), while
   the agent context stays prefix-limited at the closest compaction scope.
6. Loading a session with existing compaction entries (e.g. written by pi, or
   resumed later) applies the same scoping on `build_context()`.
7. **Output documentation follows the slash-menu pattern**: the nested menus are
   reflected in the prompt itself (`/tree <entry-id> switch`, `/tree <entry-id> compact
   truncate`), and the completed prompt is preserved in the output via
   `Repl::emit_commit` — exactly like `/model <model>` or
   `/session <id> resume`. This **replaces** the current non-compliant
   `/tree navigated (<id>)` confirmation line (and the proposed
   `/tree compacted at …` line): the committed prompt is the record of the
   action taken.
8. **No informational output is printed inline.** Details such as token counts
   and dropped/kept entry counts are shown in the `compact` sub-menu's **header
   section** (like `SessionAction`'s per-session info header), never echoed
   after the committed prompt.

---

## Pi compatibility contract (verified against pi source)

Read from `~/src/pi` (`packages/coding-agent/src/core/session-manager.ts`,
`core/compaction/compaction.ts`, `core/messages.ts`) and
`@earendil-works/pi-coding-agent/docs/{compaction.md,session-format.md}`.

### 1. Serialized entry format — already compatible ✓

```json
{"type":"compaction","id":"f6g7h8i9","parentId":"e5f6g7h8","timestamp":"2024-12-03T14:10:00.000Z","summary":"…","firstKeptEntryId":"c3d4e5f6","tokensBefore":50000}
```

picocode's `SessionEntryContent::Compaction` (`src/session/types.rs:204`)
serializes exactly this: `type: "compaction"` (serde tag), `firstKeptEntryId`,
`tokensBefore`. Round-trip test exists (`session_entry_roundtrip_all_variants`).
**No schema change needed.** Optional pi fields `details` / `fromHook` are
absent when `None` in pi too (they are optional), so writing without them is
valid pi JSONL.

### 2. Tree semantics — already compatible ✓

pi `appendCompaction` (`session-manager.ts:1028`) appends the entry with
`parentId = leafId` and advances the leaf (`_appendEntry` sets
`leafId = entry.id`); subsequent `appendMessage` calls parent to the compaction
entry. The compaction entry **is a tree node** (in `byId`, appears in
`getTree()`).

picocode's `Session::append_compaction` (`src/session/session.rs:442`) does the
same: `parent_id = leaf_id`, inserts into `entries`/`children`, advances the
leaf. `build_subtree` (`types.rs:518`) skips only `Leaf`/`SessionInfo` nodes, so
compaction entries appear in the `/tree` overlay — matching pi's
tree-selector, which renders them as `[compaction: Nk tokens]`
(`tree-selector.ts:822`). picocode currently renders `[C] …` / `[C] (compacted)`
(`repl.rs:2983`); cosmetic only, see Phase 2.

### 3. Context scoping — pi's `buildContextEntries` (`session-manager.ts:414`)

Given the leaf path (root→leaf):

1. Find the **latest** compaction entry on the path.
2. If none → the full path is the context.
3. Otherwise the context entry list is:
   `[compaction] + [entries from firstKeptEntryId (inclusive) up to the
   compaction entry] + [entries after the compaction entry]`.

Then `buildSessionContext` (`session-manager.ts:457`) converts each entry:

- `message` → the stored `AgentMessage`
- `compaction` → a `compactionSummary` message (`role: "compactionSummary"`,
  fields `summary`, `tokensBefore`, `timestamp`) — **the compaction entry is
  never sent as itself**
- `branch_summary` → `branchSummary` message
- `custom_message` → custom message; `custom`/meta → nothing

`convertToLlm` (`core/messages.ts:176`) turns `compactionSummary` into a user
message with pi's wrapper:

```
COMPACTION_SUMMARY_PREFIX = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n"
COMPACTION_SUMMARY_SUFFIX = "\n</summary>"
```

So the LLM-visible context is:

```
[system]  [summary from cmp (if any)]  [firstKeptEntryId … compaction)  [post-compaction …]
```

**Our empty-summary divergence (documented):** pi *always* emits the wrapper
message (even for `summary = ""`). Per the requirement — "`<summary (from cmp
entry, if available)>`" — picocode emits **no summary message at all** when
`summary` is empty. This is strictly cleaner for the LLM and does not affect
serialization compatibility (the entry still carries `summary:""`). Once real
summaries exist (Phase 6) we emit pi's exact wrapper.

### 4. Repeated compactions

pi (`prepareCompaction`, `compaction.ts:633`):

- The next compaction's summary span starts at the **previous** compaction's
  `firstKeptEntryId` (fallback: the entry after the previous compaction entry).
- `tokensBefore` = `estimateContextTokens(buildSessionContext(path).messages)`
  — tokens of the *already-scoped* context being replaced.
- Context scoping always uses the **latest** compaction; older compaction
  entries that fall inside the kept range are converted to their **own**
  summary messages (in path order), so context can contain several
  `compactionSummary` messages.

picocode's `apply_transforms` must match this (see Phase 1).

### 5. Replay

pi's TUI renders the **full session** from disk (all entries, including
compaction nodes) when resuming/replaying; only the LLM context is scoped.
That is exactly the requested split: **replay full, context scoped.**

---

## Current state in picocode (verified @ `9e4a481`)

- `SessionEntryContent::Compaction { summary, first_kept_entry_id, tokens_before }`
  with pi-exact serde (`types.rs:204`); `SessionEntry::new_compaction`
  (`types.rs:397`); `Session::append_compaction` (`session.rs:442`, advances
  leaf, tree node — pi-compatible).
- `apply_transforms` (`session.rs:159`) already scopes context to the most
  recent compaction: synthetic user message `[Compacted: {summary}]` +
  `entries[first_kept_idx..]`. **Diverges from pi** in two ways:
  1. always emits a summary message even when `summary` is empty;
  2. text format is `[Compacted: …]`, not pi's wrapper.
- `build_context` (`session.rs:131`) applies `apply_transforms` then
  `extract_messages`; `PicoAgent::prompt` (`agent.rs:101`) calls it fresh every
  turn, so scoping automatically applies to normal turns **and** after resume.
- Replay paths (`/replay` at `agent.rs:2283`, resume at `agent.rs:1538`) all go
  through `build_context().await` → `group_into_turns` → `replay_recent_turns`.
  They currently inherit whatever `build_context` returns — so once scoping
  lands, replay would **stop showing the beginning** unless switched to the raw
  branch. (This is the main replay change.)
- `/tree` UI: `ActiveOverlay::Tree` Enter emits `NavigateTree(target_id)`
  (`repl.rs:1341`); main loop `move_leaf` (`agent.rs:2089`) and `/tree <entry-id>
  [--summarize]` Submit path (`agent.rs:2185`) both handle switching.
- **Branch summarization already exists** (`parse_tree_args`, `agent.rs:1674`
  + the Submit handler `agent.rs:2199-2228`): pi's "Branch Summarization"
  (pi `compaction.md`; `session_before_tree` `userWantsSummary`). It
  summarizes the **abandoned branch** (old leaf → common ancestor), appends a
  `BranchSummary` entry at the old leaf, then `move_leaf(target)` —
  pi-identical. Its canonical command becomes `/tree <entry-id> switch --summarize`
  (the bare `/tree <entry-id> --summarize` stays accepted as a backward-compatible
  alias). It is **orthogonal to `compact`**: branch-summarization preserves
  the branch you're leaving when switching; `compact` truncates the *prefix*
  of the current context. pi has both mechanisms.
- No token estimator exists (no `estimate_tokens` anywhere) — Phase 3 adds one.

---

## Design

### Command grammar

```
/tree <entry-id> [switch [--summarize] | compact [truncate|text-only]]
```

- Default action = `switch`; default switch mode = plain (`just switch`).
- `switch --summarize` = switch **and** summarize the abandoned branch
  (existing branch-summarization; committed as `/tree <entry-id> switch --summarize`).
  The bare `/tree <entry-id> --summarize` form remains accepted as an alias.
- `compact` appends a compaction entry; its sub-menu selects the summary mode
  (`truncate` = `summary ""`; `text-only` = deterministic text summary).
  Default mode = `truncate`.
- `--summarize` applies only to `switch`; for `compact` it is ignored (or
  rejected with a notice — decide in review).

### Output documentation (slash-menu convention)

Every action completed through the overlays is committed via
`Repl::emit_commit` (same as the `/model`, `/thinking`, `/session` overlays,
`repl.rs:1095/1280`), so the finished prompt — `/tree <entry-id> switch`,
`/tree <entry-id> switch --summarize`, or `/tree <entry-id> compact truncate` — is echoed
into the session output and stands as the record of the action. Consequently:

- the main loop **no longer prints** `/tree navigated (<id>)`
  (`agent.rs:2096`, `agent.rs:2234`) or any `/tree compacted at …` equivalent;
- **no informational output is printed inline after the committed prompt.**
  Informational details (what will be dropped / kept, token estimates) live in
  the `compact` sub-menu's **header section** (Phase 5.2), mirroring how
  `SessionAction` renders per-session info header rows. Nothing else is echoed
  to the scrollback;
- failure/error notices (`Cannot compact: …`, `Session not found: …`) are
  still printed via `display_text` — errors are the only inline additions.

### UI flow

```
/tree                       → tree list overlay
  ↓ Enter on a point
/tree <entry-id>            → TreeAction overlay: [switch*] [compact]
  ↓ Enter on "switch"       → buffer "/tree <entry-id> switch"   (sub-menu opens)
  ↓ Enter on "compact"      → buffer "/tree <entry-id> compact"  (sub-menu opens)
  ↓ Esc                     → back to "/tree " (list)
/tree <entry-id> switch     → SwitchAction overlay: [just switch*] [summarize]
  ↓ Enter on "just switch"  → commit & submit "/tree <entry-id> switch"
  ↓ Enter on "summarize"    → commit & submit "/tree <entry-id> switch --summarize"
  ↓ Esc                     → back to "/tree <entry-id>" (TreeAction)
/tree <entry-id> compact    → CompactSummary overlay:
                             header: [selected entry] [drops N entries ~Xk tok]
                                     [keeps M entries] [written into this session]
                             options: [truncate*]
  ↓ Enter on "truncate"     → commit & submit "/tree <entry-id> compact truncate"
  ↓ Esc                     → back to "/tree <entry-id>" (TreeAction)
```

Mirrors the existing `/session <name>` → `SessionAction` → `Confirm` pattern:
Enter completes the buffer, the next `from_buffer` pass promotes to the next
overlay, Enter on a leaf option commits the completed line into the output
(`Repl::emit_commit`) and submits it. The committed line stays in the
scrollback as the documentation of the action.

---

## Phase 1 — Context scoping: rewrite `apply_transforms` (`src/session/session.rs`)

Replace the current transform with pi-exact semantics:

```rust
/// pi's compaction summary message wrapper (core/messages.ts).
const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Build the synthetic user message that stands in for a Compaction entry.
fn compaction_summary_message(summary: &str) -> SessionEntry {
    SessionEntry {
        id: uuid::Uuid::now_v7().to_string(),
        parent_id: None,
        timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        content: SessionEntryContent::Message {
            message: serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": format!("{}{}{}", COMPACTION_SUMMARY_PREFIX, summary, COMPACTION_SUMMARY_SUFFIX)
                }]
            }),
        },
    }
}

fn apply_transforms(&self, entries: Vec<SessionEntry>) -> Vec<SessionEntry> {
    // Latest compaction on the branch (closest to leaf) — matches pi.
    let compaction = entries.iter().rev().find_map(|entry| match &entry.content {
        SessionEntryContent::Compaction { summary, first_kept_entry_id, .. } =>
            Some((entry.id.clone(), summary.clone(), first_kept_entry_id.clone())),
        _ => None,
    });

    let Some((compaction_id, summary, first_kept_id)) = compaction else {
        return entries; // no compaction → full branch
    };

    let keep_idx = match entries.iter().position(|e| e.id == first_kept_id) {
        Some(i) => i,
        None => return entries, // defensive fallback
    };

    let mut result: Vec<SessionEntry> = Vec::new();

    // The latest compaction's summary goes FIRST (pi: contextEntries = [compaction] + …).
    // Empty summary → emit nothing ("if available").
    if !summary.is_empty() {
        result.push(compaction_summary_message(&summary));
    }

    // Kept range: firstKeptEntryId .. end, with the latest compaction entry
    // itself excluded (already represented above; never sent as itself).
    for entry in &entries[keep_idx..] {
        if entry.id == compaction_id {
            continue;
        }
        match &entry.content {
            // Older compaction inside the kept range → its own summary message
            // (pi behavior for repeated compactions); empty → skipped.
            SessionEntryContent::Compaction { summary, .. } if !summary.is_empty() => {
                result.push(compaction_summary_message(summary));
            }
            SessionEntryContent::Compaction { .. } => {}
            _ => result.push(entry.clone()),
        }
    }
    result
}
```

Notes:

- `extract_messages` (`session.rs:510`) already skips non-message variants, so
  any residual `Compaction` entry would never reach the LLM; the explicit
  `continue` for the latest compaction keeps ordering correct (its summary is
  at the front, not duplicated at its path position).
- This exactly reproduces pi's `buildContextEntries` message order for the
  single-compaction case: `[summary?] [firstKept…compaction) [post-compaction…]`.
- Because `PicoAgent::prompt` calls `build_context()` fresh each turn
  (`agent.rs:101`), **resume and subsequent turns get the scoped context for
  free** — this satisfies "compactions handled correctly when loading session
  context on session resume."
- Keep the `Compaction { tokens_before, .. }` field available for the future
  summary prompt / display; it is metadata, not used by scoping.

**Test:** extend `session.rs` tests with a branch
`[m0,m1,m2, C{firstKept:m1,summary:""}, m3]` →
`build_context().messages == [m1,m2,m3]`; with `summary:"S"` →
`[user("The conversation history …<summary>\nS\n</summary>"), m1,m2,m3]`; with a
second older compaction inside the kept range → both summary messages in pi
order. Serialization round-trip with pi's exact JSON (from `session-format.md`).

---

## Phase 2 — Replay: full branch + compaction markers (`src/agent.rs`)

Requirement: replay shows compaction entries but continues **all the way to the
beginning**; only the agent context is scoped.

Change replay to operate on the raw branch instead of the scoped context:

1. New replay-unit type and builder:

```rust
/// One replayable item: a conversation turn, or a compaction boundary marker.
enum ReplayUnit {
    Turn(ConversationTurn),
    Compaction { tokens_before: u64 },
}

/// Walk the raw branch (root→leaf). Converts message entries via the same
/// pi→rig path as `extract_messages`, but records compaction entries as
/// markers interleaved with turns (toolResult batching preserved).
fn build_replay_units(branch: &[SessionEntry]) -> Vec<ReplayUnit>
```

   Implementation: walk entries in order; maintain the same state machine as
   `group_into_turns` (buffer `toolResult`s, open a turn on a user-text
   message, append assistant segments), and push a `ReplayUnit::Compaction`
   whenever a `Compaction` entry is crossed. Reuse the existing
   `group_into_turns` by first producing `(messages, marker_positions)` and
   merging — keeps one grouping implementation.

2. New renderer (or extend `replay_recent_turns`):

```rust
/// Replay units from `start..`; compaction markers render as a dim notice row
/// ("[compaction: Nk tokens]") between turns; turns render exactly as today.
fn replay_units(output: &Arc<dyn Output>, units: &[ReplayUnit], start: usize)
```

   Marker rendering: `output.display_thinking(&format!("[compaction: {}k tokens]", t/1000))`
   (or `display_system`/`display_plain` — pick the dimmest available; see
   `src/output.rs` trait). A marker is **not** a turn: `Nt`/`Nl` limits count
   turns, and markers inside the kept window still render.

3. Update call sites to pass the raw branch:

   - `/replay` handler (`agent.rs:2283`, all four branches: `Nl` capture, `0l`,
     turn count, no arg) — replace `s.build_context().await` with
     `s.get_branch().await` and `build_replay_units`.
   - Resume replay (`handle_session_command` "resume", `agent.rs:1538`) — same.
   - `replay_recent_turns` can be reimplemented on top of `replay_units` (or
     kept for the no-compaction fast path; prefer one code path).

4. Tree/replay label parity (cosmetic, optional): align the `/tree` overlay's
   compaction row with pi's `[compaction: Nk tokens]`
   (`tree-selector.ts:822`) instead of `[C] (compacted)` (`repl.rs:2983`).
   Uses `tokens_before`, so it also motivates a correct value in Phase 3.

**Why this is safe:** with no compaction entries on the branch,
`build_replay_units` produces exactly today's turns — replay output is
byte-identical for uncompacted sessions.

---

## Phase 3 — Session write path & `tokensBefore` (`src/session/session.rs`, `src/agent.rs`)

`Session::append_compaction(summary, first_kept_entry_id, tokens_before)`
already exists and is pi-compatible (tree node, advances leaf). The `/tree
compact` handler needs a `tokens_before` value.

No token estimator exists. Add a cheap deterministic heuristic (clearly labeled
approximation; pi uses a real tokenizer):

```rust
/// Approximate tokens for the current scoped context, for `tokensBefore`.
/// Rough heuristic (chars/4 + per-message overhead); documented as an
/// estimate, good enough for display "[compaction: Nk tokens]" and for
/// future iterative-summary prompts.
fn estimate_context_tokens(messages: &[Message]) -> u64 {
    let mut n = 0u64;
    for m in messages {
        n += 16; // per-message overhead (role + framing)
        match m {
            Message::User { content } => for c in content {
                if let UserContent::Text(t) = c { n += t.text.chars().count() as u64 / 4; }
            },
            Message::Assistant { content, .. } => for c in content {
                match c {
                    AssistantContent::Text(t) => n += t.text.chars().count() as u64 / 4,
                    AssistantContent::Reasoning(r) => n += /* serialized text */,
                    AssistantContent::ToolCall(tc) =>
                        n += tc.function.name.len() as u64 / 4
                           + tc.function.arguments.to_string().len() as u64 / 4,
                    _ => {}
                }
            },
            Message::System { content } => n += content.chars().count() as u64 / 4,
        }
    }
    n.max(1)
}
```

(Place next to the replay helpers in `src/agent.rs`, or in `session.rs` so it
can be unit-tested against a fixture.)

The compaction handler computes `tokens_before` from
`build_context().await` **before** appending, exactly like pi
(`estimateContextTokens(buildSessionContext(path).messages)`).

### Surfacing the estimate to the REPL (for the sub-menu header)

The same estimate is surfaced to the `compact` sub-menu header (Phase 5.2), so
**no informational output is printed inline** after the committed prompt:

- `ReplCtx` gains a `context_tokens: u64` field (`repl.rs:252`, next to
  `branch_messages`/`tree`/`leaf_id`).
- `build_session_ctx` (`agent.rs:1164`) computes it once per loop iteration
  while it is already building the tree: `s.build_context().await` →
  `estimate_context_tokens(&ctx.messages)` (0 when no session). Cost is one
  extra `build_context` per keystroke-visible loop pass — same order as the
  existing `get_branch()`/`get_tree()` calls.
- The sub-menu header therefore shows the exact `tokensBefore` value the
  handler will write (both call `estimate_context_tokens` on the same
  `build_context()` output).
- Dropped/kept **entry counts** for a specific target are derived cheaply from
  the flattened tree + active path in `from_buffer` (no extra I/O).

---

## Phase 4 — Agent dispatch (`src/agent.rs`)

### 4.1 Parsing

Replace `parse_tree_args` with an action-aware version:

```rust
/// Tree action selected from the `/tree` overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreeAction {
    Switch,          // move the leaf pointer (today's behavior)
    Compact,         // append a Compaction entry, then scope the context
}

/// Summary mode for `/tree compact` (future: more modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactSummaryMode {
    Truncate,      // summary = "" — drop the prefix, keep no summary
}

/// Parse `/tree <entry-id> [switch [--summarize] | compact [truncate|text-only]]`.
/// The bare `/tree <entry-id> --summarize` form is accepted as an alias for
/// `switch --summarize` (backward compat).
/// IMPORTANT: the angle brackets around the id are STRIPPED here —
/// move_leaf/get_entry need the raw entry id. Regression-tested
/// (parse_tree_args_strips_angle_brackets_from_id): with the brackets kept,
/// the leaf pointer was written to a nonexistent entry → empty context,
/// no current-position marker, "/replay: session is empty".
fn parse_tree_args(rest: &str) -> (String, TreeAction, Option<CompactSummaryMode>, bool) {
    let mut summarize = false;
    let mut target = String::new();
    let mut action = TreeAction::Switch;
    let mut mode: Option<CompactSummaryMode> = None;
    for part in rest.split_whitespace() {
        match part {
            "--summarize" | "-s" => summarize = true,
            "switch" => action = TreeAction::Switch,
            "compact" => action = TreeAction::Compact,
            "truncate" | "none" | "no-summary" => mode = Some(CompactSummaryMode::Truncate),
            id => target = id.to_string(),
        }
    }
    (target, action, mode, summarize)
}
```

**Note:** `parse_tree_args` only ever sees the **committed canonical line**
(from the overlay's `handle_enter`) or a hand-typed canonical/alias form. The
raw sub-menu filter text (`su`, `just`, `summarize` without `--`) is consumed
by `from_buffer`'s fuzzy/pre-selection logic (Phase 5.2) and never reaches
this parser — the committed line always normalizes the choice to `switch` or
`switch --summarize`.

### 4.2 Main-loop `/tree` block (`agent.rs:2185`)

```rust
if input.starts_with("/tree") {
    let rest = input["/tree".len()..].trim();
    let (target_id, action, mode, summarize) = parse_tree_args(rest);
    if target_id.is_empty() {
        continue; // overlay already open via from_buffer
    }

    match action {
        TreeAction::Switch => {
            // Existing behavior: if `summarize`, summarize the abandoned
            // branch (collect_entries_for_branch_summary + agent.summarize +
            // append_branch_summary), then move_leaf(target). Also apply
            // thinking_level and model from build_context() (doc/tree.md
            // critique #3).
            //
            // NO confirmation line here — the committed prompt
            // "/tree <entry-id> switch [--summarize]" (echoed by the overlay's
            // emit_commit) is the documentation. Remove the old
            // display_plain("/tree navigated (…)") at agent.rs:2234.
        }
        TreeAction::Compact => {
            let Some(ref s) = session else { continue };
            let old_leaf_id = s.get_leaf_id().await;

            // pi: refuse to compact when the leaf IS a compaction entry.
            if let Some(id) = &old_leaf_id {
                if let Some(entry) = s.get_entry(id).await {
                    if matches!(entry.content, SessionEntryContent::Compaction { .. }) {
                        config.output.display_text("Cannot compact: the session was already compacted at this point");
                        continue;
                    }
                }
            }

            // tokensBefore from the current scoped context (pi semantics).
            let tokens_before = match s.build_context().await {
                Ok(ctx) => estimate_context_tokens(&ctx.messages),
                Err(_) => 0,
            };

            let summary = match mode.unwrap_or(CompactSummaryMode::Truncate) {
                CompactSummaryMode::Truncate => String::new(),
            };

            match s.append_compaction(&summary, &target_id, tokens_before).await {
                Ok(_) => {
                    // No "/tree compacted at …" line and no inline token
                    // count: the committed prompt "/tree <entry-id> compact truncate"
                    // documents the action. Informational details (what was
                    // dropped/kept, token estimate) were already shown in the
                    // sub-menu header before committing (Phase 5.2).
                }
                Err(e) => config.output.display_text(&format!("Cannot compact session: {}", e)),
            }
            // Next prompt() rebuilds the scoped context via build_context();
            // subsequent turns are appended after the compaction entry because
            // append_message() parents to the new leaf. No session switch.
            continue;
        }
    }
    continue;
}
```

Notes:

- The leaf-is-compaction guard mirrors pi's `prepareCompaction` early return
  (`compaction.ts:637`): don't stack a compaction directly on a compaction.
- `append_compaction` advances the leaf; the very next turn's
  `append_message` uses `index.leaf_id` as parent, so **subsequent turns are
  appended in the session after the compaction entry** (requirement ✓).
- No agent rebuild needed: `prompt()` re-derives context each turn. (If a
  future refactor caches `state.messages`, set it from the new
  `build_context()` here, as the `--summarize` switch path does.)
- If `target_id` is not on the current branch, `append_compaction` still writes
  a valid entry but `firstKeptEntryId` won't resolve — `apply_transforms`
  falls back to the full branch (defensive). Prefer validating
  `s.get_entry(&target_id)` first and erroring, to avoid silently writing a
  no-op compaction.

---

## Phase 5 — REPL UI (`src/repl.rs`)

### 5.1 New overlay variants

```rust
/// `/tree <entry-id>` action menu (switch / compact).
TreeAction {
    target_id: String,
    header: Vec<String>,        // selected entry label + id
    opts: Vec<String>,          // ["switch", "compact"]
    active: Option<usize>,      // default: Some(0) = switch
    _filter: String,
},

/// `/tree <entry-id> switch` sub-menu (just switch / summarize branch).
SwitchAction {
    target_id: String,
    header: Vec<String>,        // selected entry label + id (shared style)
    opts: Vec<String>,          // ["just switch", "summarize"]
    active: Option<usize>,      // default: Some(0) = just switch
    _filter: String,
},

/// `/tree <entry-id> compact` summary-mode sub-menu.
/// `header` carries the informational rows (selected entry, dropped/kept
/// counts, token estimate) — the in-menu equivalent of `SessionAction`'s
/// per-session info header. No such info is ever printed inline.
CompactSummary {
    target_id: String,
    header: Vec<String>,        // info rows (Phase 5.2)
    opts: Vec<String>,          // ["truncate", "text only"]
    active: Option<usize>,      // Some(0)
    _filter: String,
},
```

### 5.2 `from_buffer` promotion (`repl.rs`, `"tree"` arm)

In the `"tree"` arm, before the list path, promote based on the buffer — the
buffer itself encodes the nesting depth (mirrors how `/session <name> <action>`
promotes). **Implemented** with the angle-bracket id syntax:

- `filter` is empty, or a **search term** (no angle brackets) → tree list
  (`ActiveOverlay::Tree`), fuzzy-filtered as before — `/tree someterm`
  searches entry contents.
- `filter` is `<entry-id>` (angle-bracketed; id must exist in the tree) →
  `ActiveOverlay::TreeAction` (options `switch`, `compact`;
  `active = Some(0)`).
- `filter` parses as `<entry-id> switch` (id + trailing `switch` token) →
  `ActiveOverlay::SwitchAction` directly (options `just switch`, `summarize`).
- `filter` parses as `<entry-id> compact` (id + trailing `compact` token) →
  `ActiveOverlay::CompactSummary` directly (options `truncate`;
  `active = Some(0)`).
- `filter` is `<entry-id> switch --summarize` or `<entry-id> compact
  truncate` → the corresponding overlay with the trailing mode **pre-selected**
  (`summarize` / `truncate`), so Enter re-commits the full line verbatim.
- An angle-bracketed id that matches no tree entry falls through to the list
  (which will show no matches for that filter — same behavior as today's
  unknown-id list state).

**Tree list cursor start:** when the list opens, `sel` starts on the **current
position row** (the leaf, reported as the overlay's `active` index) rather
than the top — `overlay_window` then scrolls the list so that row is in view.
No search needed to find where you are. Implemented in the `read_input`
sel-reset: `ActiveOverlay::Tree { active: Some(a), .. } => sel = *a`.

Implementation notes (`Repl::parse_tree_id`, `Repl::filter_tree_options`):
`parse_tree_id` returns `Some(id)` only for `<…>` (≥ 1 char, trimmed); any
other argument is treated as a search term. The text after the id token is
split into the action token and the sub-menu's fuzzy filter.

#### Pervasive fuzzy search (established slash-menu pattern)

Every option list above is **fuzzy-filterable by the trailing buffer text**,
rebuilt on each keystroke exactly like the `/persona` and `/model` overlays:

- `TreeAction`: the text after `/tree <entry-id> ` filters `switch`/`compact`
  via `Repl::filter_tree_options` (fuzzy rank + exact pre-selection); e.g.
  `/tree <entry-id> co` narrows to `compact`. An unknown single token after
  the id (`/tree <entry-id> sw`) is treated as this filter — the id is
  selected, the action menu opens pre-narrowed.
- `SwitchAction`: text after `/tree <entry-id> switch ` filters `just switch` /
  `summarize`; e.g. `/tree <entry-id> switch su` → `summarize` active,
  `/tree <entry-id> switch j` → `just switch` active.
- `CompactSummary`: text after `/tree <entry-id> compact ` filters
  `truncate` / `text only` (e.g. `/tree <entry-id> compact te` → `text only`).
- Exact known tokens (`--summarize`, `truncate`, `just-switch`, `switch`,
  `compact`) additionally **pre-select** their option regardless of fuzzy
  rank — this is the "direct correspondence with the slash prompt": the
  buffer you typed *is* the selection, and Enter commits it verbatim (as
  `SessionAction` re-commits `/session <id> resume`).

  This pre-selection logic runs on the **raw trailing text** and is
  independent of `parse_tree_args` (which only parses committed canonical
  lines). `just switch` (two tokens) is recognized by fuzzy-matching the
  trailing text against the option labels — typing `just` or `j` selects it;
  typing `su`, `summarize`, or `--summarize` selects `summarize`. The
  committed line is always the canonical form (`switch` / `switch
  --summarize`), so hand-typed filter fragments never leak into the parser.

Selection → committed line mapping (canonical, filter text dropped):

| Overlay | Option | Committed line |
| --- | --- | --- |
| TreeAction | `switch` | `/tree <entry-id> switch` (then sub-menu) |
| TreeAction | `compact` | `/tree <entry-id> compact` (then sub-menu) |
| SwitchAction | `just switch` | `/tree <entry-id> switch` |
| SwitchAction | `summarize` | `/tree <entry-id> switch --summarize` |
| CompactSummary | `truncate` | `/tree <entry-id> compact truncate` |
| CompactSummary | `text only` | `/tree <entry-id> compact text-only` |

#### Header rows for `CompactSummary` (the "per-session info" equivalent)

Build the header with a dedicated helper, mirroring
`session_action_overlay_entries` (`repl.rs:2632`):

```rust
/// Fill the info header for the compact sub-menu: what this compaction
/// will drop / keep, and the token estimate that becomes `tokensBefore`.
fn compact_summary_overlay_entries(
    target_id: &str,
    ctx: &ReplCtx,           // tree, leaf_id, context_tokens
    header: &mut Vec<String>,
    opts: &mut Vec<String>,
    active: &mut Option<usize>,
) {
    // 1. Selected entry row (label + short id), from flatten_tree_nodes +
    //    entry_display_label (as in TreeAction's header).
    // 2. Stats rows, via a single-column Table like SessionAction's detail
    //    rows (full-width, non-selectable, BG_HEADER background):
    //      - "drops N entries before this point"   (count of flat entries
    //        strictly before target_id on the active path)
    //      - "keeps M entries to the current leaf" (count from target_id
    //        through the leaf)
    //      - "~Xk tokens before" — ctx.context_tokens / 1000, the same
    //        estimate the handler writes as `tokensBefore`
    //      - "written into this session" (reminds the user this is an
    //        in-place compaction entry, not a fork)
    // 3. opts = ["truncate"], active = Some(0).
}
```

The dropped/kept counts come from the flattened tree + active path already
used by `tree_overlay_entries` (`repl.rs:2831`): walk the flat list, count
entries before/after `target_id`. No new I/O, no token estimation in the
overlay — the token number is `ReplCtx::context_tokens` (Phase 3).

The same helper style applies to `TreeAction`'s and `SwitchAction`'s smaller
headers (selected entry row only).

### 5.3 `handle_enter`

- `Tree`: replace `NavigateTree(target_id)` (`repl.rs:1341`) with
  `editor.set(&format!("/tree {}", target_id))` + `Continue` → list Enter opens
  the action menu (mirrors `Session` overlay Enter).
- `TreeAction` `switch`: `editor.set("/tree <entry-id> switch")` + `Continue`
  (sub-menu opens on the next `from_buffer` pass).
- `TreeAction` `compact`: `editor.set("/tree <entry-id> compact")` + `Continue`
  (sub-menu opens on the next `from_buffer` pass).
- `SwitchAction` `just switch`: commit + submit via `Repl::emit_commit`
  (`emit_commit(output, editor, redraw, sel, input_row, &line, "", false)`, as
  the `SessionAction` resume arm does at `repl.rs:1280`) with
  `line = "/tree <entry-id> switch"`, then
  `return OverlayAction::Action(InputAction::Submit(line))`.
- `SwitchAction` `summarize`: commit + submit `/tree <entry-id> switch --summarize`
  (same `emit_commit` + `Submit` pattern). The committed line is preserved in
  the output — this is the action's documentation.
- `CompactSummary` `truncate`: commit + submit
  `/tree <entry-id> compact truncate` (same `emit_commit` + `Submit` pattern).
- `CompactSummary` `text only`: commit + submit
  `/tree <entry-id> compact text-only`. After the handler appends the entry,
  the generated summary is rendered as markdown below the committed line —
  the one intentional inline output, present only for summary-producing
  modes.

### 5.4 `handle_tab` / `handle_esc` / `display_rows` / `overlay_disc`

- `Tree` Tab: fill `/tree <entry-id>` into the buffer (**implemented**;
  previously a no-op).
- `TreeAction` Tab: fill `/tree <entry-id> <action>` (e.g.
  `/tree <entry-id> compact`) — **implemented**.
- `SwitchAction` Tab: fill `/tree <entry-id> switch --summarize` when `summarize`
  is selected, else leave `/tree <entry-id> switch` — **implemented**.
- `CompactSummary` Tab: fill `/tree <entry-id> compact truncate` — **implemented**.
- `TreeAction` Esc → `/tree ` (back to list); `SwitchAction` /
  `CompactSummary` Esc → `/tree <entry-id>` (back to TreeAction) — **implemented**.
- `display_rows`: "Tree actions  (up/down, Tab/Enter select, Esc back)",
  "Switch actions  (up/down, Tab/Enter select, Esc back)", and
  "Compact summary  (up/down, Tab/Enter select, Esc back)".
- New discriminants (e.g. 16, 17, 18) so `sel` resets on promotion.
- Add `TreeAction`/`SwitchAction`/`CompactSummary` to the `overlay_text` match
  that forwards `header` (`repl.rs:1681`), exactly like `SessionAction`/
  `Confirm`: all render with `Self::overlay_lines(effective_title, &opts,
  Some(header.as_slice()), sel, active, wrap)`, so the info rows appear as
  non-selectable BG_HEADER lines above the options.

### 5.5 Cleanup — **done**

`InputAction::NavigateTree` and its main-loop handler are **removed**: the
`Tree` Enter arm now completes `/tree <entry-id>` into the buffer, and the
committed `/tree <entry-id> switch [--summarize]` prompt (Phase 5.3) replaces
the old `display_plain("/tree navigated (…)")` line. Verified no other
emitters remain.

---

## Phase 6 — Future: summarized compaction

When a summary mode beyond `truncate` is added:

1. Add `CompactSummaryMode::Summarized` (sub-menu gains the option) and a
   user-tweakable prompt setting (`SettingKey::TreeSummaryPrompt`, default
   prompt asking for a standalone recap of the dropped prefix; `/set` value-pick
   overlay + `settings.toml.example`).
2. In the `Compact` handler: compute the dropped prefix = branch entries before
   `firstKeptEntryId` (or, for repeated compaction, from the previous
   compaction's boundary — pi `prepareCompaction` semantics), serialize via the
   existing `build_summary_prompt`/`serialize` helpers, call
   `agent.summarize(&prompt)`, and pass the result as `summary` to
   `append_compaction`.
3. `tokens_before` is computed from the scoped context as in Phase 3; iterative
   summarization can pass the previous compaction's summary as context (pi
   `previousSummary`).
4. `apply_transforms` already emits the pi summary wrapper for non-empty
   summaries (Phase 1) — no further context change needed.

### Future UI exposure (branch summarization)

Branch summarization is now **exposed in the menu** as the `SwitchAction`
sub-menu's `summarize` option, committed as `/tree <entry-id> switch --summarize`
(Phase 5) and reusing the existing implementation
(`collect_entries_for_branch_summary` + `agent.summarize` +
`append_branch_summary`). No separate future step needed — pi's
`session_before_tree` `userWantsSummary` behavior is covered.

---

## Edge cases & risks

| Case | Behavior |
| --- | --- |
| `target_id` not on the current branch | Validate with `get_entry` before writing; error message, session unchanged (avoids a silently no-op compaction). |
| `target_id` == current leaf | Appending a compaction with `firstKeptEntryId` = leaf scopes to the last entry only; allowed but pointless. Consider warning "nothing before this point" — decide in review. |
| Leaf is already a compaction entry | Refuse (pi parity: `prepareCompaction` early return). |
| Empty `summary` in context | No summary message emitted (documented divergence from pi's empty wrapper). |
| Repeated compactions | Latest compaction scopes; older ones inside kept range become their own summary messages (pi parity). |
| Session written by pi, read by picocode (and vice versa) | Schema identical; `build_index`/`get_path_to_root` treat compaction as a normal node; `build_context` scopes. Verify with round-trip test (below). |
| `firstKeptEntryId` missing on branch | `apply_transforms` falls back to the full branch (defensive, matches pi's graceful handling). |
| `tokensBefore` accuracy | Heuristic only; used for the sub-menu header display and future prompts — no correctness dependency. Header and written value come from the same `estimate_context_tokens(build_context())` call. |
| Replay of a compacted session | Markers render, full history from root is replayed; `Nt`/`Nl` limits count turns (markers don't count). |
| `NavigateTree` removal | Only emitter is `repl.rs:1344`; grep after editing. |

---

## Test plan

1. **`apply_transforms` unit tests** (`session.rs`) — **done**: empty-summary scoping;
   non-empty pi-wrapper message; repeated compaction (older summary kept);
   missing `firstKeptEntryId` fallback; compaction entry never in output.
2. **Serialization cross-compat**: assert picocode writes
   `{"type":"compaction",…,"firstKeptEntryId":…,"tokensBefore":…}` byte-compatible
   with the pi sample in `session-format.md`; parse pi's sample line into
   `SessionEntry` and back. (Extend `session_entry_roundtrip_all_variants`.)
3. **Pi round-trip (manual/integration)**: write a compacted session with
   picocode, open it in pi (`pi --session <file>` or via the TUI) and confirm
   the TUI shows the compaction node and the LLM context starts at
   `firstKeptEntryId`; then write a compacted session with pi and resume it in
   picocode, confirming `build_context` scopes identically.
4. **Replay tests** — **done** (`build_replay_units_marks_compaction_and_keeps_all_turns`, `build_replay_units_uncompacted_has_all_turns` + renderer tests): uncompacted branch → identical output to today; compacted
   branch → markers rendered, full history present, `Nt`/`Nl` limits correct.
5. **`parse_tree_args` tests**: default `switch`; `switch --summarize` and the
   bare `--summarize` alias; `compact`/`compact truncate` (and `no-summary`
   alias); last action token wins.
6. **REPL tests** — **partially done**: `parse_tree_id` (angle-bracket vs
   search-term) and `filter_tree_options` (empty/exact/fuzzy/no-match) are
   unit-tested. Remaining: `from_buffer` promotion cases
   (`/tree <entry-id>` → `TreeAction`, `+ switch` → `SwitchAction`,
   `+ compact` → `CompactSummary`), trailing-mode pre-selection, `CompactSummary`
   header contents, Enter commit lines, Esc unwinding (TUI-heavy; cover in
   manual smoke).
7. **Manual smoke**: `/tree` → pick a point → `switch` sub-menu shows
   `just switch`/`summarize`; `summarize` commits `/tree <entry-id> switch --summarize`
   and appends a `BranchSummary` entry while moving the leaf; `compact`
   sub-menu shows the info header (`drops N`, `~Xk tokens`, `written into
   this session`) + `truncate`; after `truncate`, the next prompt only sees
   context from that point; `/replay` still shows the whole conversation with
   `[compaction: …]` markers; `/session` list unaffected; `!`/`!!` turns append
   after the compaction entry.

---

## File-by-file checklist

| File | Change |
| --- | --- |
| `src/session/session.rs` | **Done.** `apply_transforms` rewritten (pi semantics, empty-summary skip, pi summary wrapper, repeated-compaction handling); `compaction_summary_message` helper + `COMPACTION_SUMMARY_PREFIX/SUFFIX` constants; unit tests. |
| `src/agent.rs` | **Done.** `TreeAction`/`CompactSummaryMode` enums + `parse_tree_args` (canonical `switch [--summarize]`, bare `--summarize` alias); `/tree` dispatch (switch + optional branch-summarize, no `/tree navigated` line / compact, no inline token output); `estimate_context_tokens` (+ `build_session_ctx` computing `ReplCtx::context_tokens`); replay: `ReplayUnit`, `build_replay_units`, `replay_units` (+ `count_turns`/`last_turns_start`/`replay_last_turns`); `/replay` + resume replay on the raw branch; `NavigateTree` handling **removed**. |
| `src/repl.rs` | **Done.** `ReplCtx::context_tokens` field; `TreeAction` + `SwitchAction` + `CompactSummary` (with `header`) overlay variants; `from_buffer` promotion with the angle-bracket id syntax (`Repl::parse_tree_id`) + fuzzy option filtering (`Repl::filter_tree_options`); `tree_entry_header` / `compact_summary_header` builders; `handle_enter` (commit+submit via `emit_commit` for leaves, buffer promotion otherwise); `handle_tab`/`handle_esc` (one level up); `display_rows` titles; `overlay_disc` (16/17/18); header-aware render arm; Tree Enter/Tab → `/tree <entry-id>` completion; `InputAction::NavigateTree` **removed**. |
| `src/slash.rs` | (optional) `/tree` help: "Navigate session history — switch, compact, or summarize branches". |
| `doc/tree_compaction.md` | this doc (update status once implemented). |
| Future | `SettingKey::TreeSummaryPrompt` in `src/settings.rs` + `settings.toml.example`; `CompactSummaryMode::Summarized` UI + `agent.summarize` wiring. |
