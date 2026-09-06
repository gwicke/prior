# `/tree` Compaction — In-Session Compaction Entry

## Status

**Phases 1–7 implemented.** Extends the existing `/tree` navigator (see
`doc/tree.md`) so that selecting a tree point can append a **compaction entry**
to the *current* session, prefix-limiting the agent context at that point —
instead of forking a new session. The session file stays in-place and remains
bidirectionally compatible with pi's session format.

**Phase 7** added a `/compact` slash command that surfaces the same
three-mode compaction sub-menu (`truncate` / `text-only` / `llm`) and
auto-resolves the target to the **last user message** on the active branch.
The command is fully driven by a shared `run_compaction` backend that also
serves `/tree <id> compact`; the LLM mode's prompt customization, the
`llm_compact_prompt` setting, and the predecessor / move-back dance are
all reused unchanged. A `CompactionTarget::Abandoned` variant is wired
up in the backend (currently `#[allow(dead_code)]`) so a future
`/tree <id> switch --summarize` UI extension can drop in without
touching the shared logic.

**Scope:** `truncate` (empty `summary`), `text-only` (deterministic
text summary), and `llm` (LLM-generated summary) compaction. The LLM mode
runs the dropped span through `agent.summarize()` after temporarily
moving the leaf to the entry just before the compaction point; the leaf is
restored before the compaction entry is appended, leaving the session
file byte-compatible with the other modes.

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
COMPACTION_SUMMARY_PREFIX = "Conversation before this point:\n\n<summary>\n"
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

**picocode's divergence (documented in `doc/nested_compactions.md`):**

- **Runtime context steps over nested compactions.** `apply_compaction_transforms`
  drops **every** `Compaction` entry inside the kept range silently — both the
  latest (represented by the front summary message) and any older nested
  compactions. Only the latest compaction's `summary` appears as a
  `compactionSummary` user message at the front of the context. Older
  summaries are not re-emitted mid-turn.
- **LLM compaction mode merges `previousSummary`.** When re-compacting, the
  summarizer receives the deepest prior compaction's `summary` whose
  `first_kept_entry_id` is *strictly before* the new target (strict-before
  rule — see `doc/nested_compactions.md` phase B), as a `--- PREVIOUS SUMMARY ---`
  section. This makes iterative re-compaction lossless.
- **The leaf-is-compaction guard is removed.** Compacting a session whose
  leaf is already a `Compaction` entry is allowed; the LLM-mode
  "nothing before the current position" guard still covers the empty-dropped-
  span no-op case.

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
    // NOTE (superseded): the sketch below emits older nested compactions as
    // their own summary messages (pi behavior). The implemented
    // `apply_compaction_transforms` now steps over ALL nested compactions
    // silently — see doc/nested_compactions.md phase A.
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

            // NOTE (superseded): the leaf-is-compaction guard below was part
            // of the original design but has been REMOVED — see
            // doc/nested_compactions.md phase C. Compacting a session whose
            // leaf is already a Compaction entry is now allowed (the
            // LLM-mode empty-dropped-span guard covers the no-op case).
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

- The leaf-is-compaction guard in the code sketch above is **superseded**:
  it was removed in doc/nested_compactions.md phase C (pi's
  `prepareCompaction` at `compaction.ts:637` refuses only when the dropped
  span is empty; the picocode LLM-mode guard covers that no-op case).
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

## Phase 6 — LLM-summarized compaction (compact `llm`)

This is the **third** summary mode for `/tree <id> compact`. It piggy-backs on
the same tree-forking mechanism `/tree <id> switch` already uses, but instead
of *leaving* the result in place as the new session leaf it captures the LLM
response as a *summary* and continues from the **original** session position.

### 6.1 The user-facing flow

```
/tree                       → tree list overlay
  ↓ Enter on a point
/tree <entry-id>            → TreeAction overlay: [switch*] [compact]
  ↓ Enter on "compact"      → buffer "/tree <entry-id> compact"  (sub-menu opens)
  ↓ Esc                     → back to "/tree " (list)
/tree <entry-id> compact    → CompactSummary overlay:
                              header: [selected entry] [drops N entries ~Xk tok]
                                      [keeps M entries] [written into this session]
                              options: [truncate*] [text only] [llm]
  ↓ Enter on "llm"          → buffer "/tree <entry-id> compact llm "
                              (a default prompt is appended for editing)
```

The `llm` option behaves differently from `truncate` / `text only`. Instead of
committing the canonical line on Enter, it **leaves the prompt open for
editing** and only commits when the user presses `<Enter>` on the now-edited
prompt — exactly like `/tree <entry-id> switch --summarize`'s neighbor path or
any other agent call: the user can tweak it, then submit.

**Default appended prompt** (lives in `SettingsStore::llm_compact_prompt`,
editable via `/set`):

> Summarize the conversation so far in less than 2000 words. Preserve the
> key technical decisions, file paths touched, current task state, and any
> unresolved questions.

After picking `llm`, the buffer becomes:

```
/tree <entry-id> compact llm Summarize the conversation so far in less than 2000 words. Preserve…
```

The user can edit the text (Backspace, arrows, etc. — the standard prompt
edit surface). Pressing `<Enter>` on the final line submits it as a regular
prompt. The committed line is preserved in the scrollback via `emit_commit`,
e.g.:

```
/tree <entry-id> compact llm Summarize the conversation so far in less than 2000 words…
```

**Quoting convention:** consistent with other slash commands that take a
free-text payload (`/session <id> rename <newname>`, `/set <key> <value>`,
etc.) — **no quoting is required or stripped**. The trailing text after
`llm` is taken verbatim, joined with single spaces. The default prompt
itself contains no whitespace-collapsing characters so it round-trips
cleanly. If a user wants literal double-quotes in their prompt they type
them as-is (they will appear in the LLM message). This matches the existing
slash-menu convention; introducing a one-off quote-stripping rule just for
this command would be inconsistent.

### 6.2 Why this uses the tree-fork mechanism

The committed line is `/tree <id> compact llm <prompt>` — *not* a plain prompt.
The trailing `<prompt>` is the user-tweakable payload that the compaction
handler will hand to the LLM. The prompt's **structural** form (everything
before the trailing free text) tells the main loop this is an LLM-compaction
request, the way `/tree <id> switch` tells it to move the leaf.

The handler reuses the existing fork-and-flip dance instead of inventing a
parallel one:

1. The handler **moves the leaf pointer to the entry just *before*
   `<entry-id>`** (the immediately preceding entry on the current branch — not
   `<entry-id>` itself, see §6.4 below) and remembers the original leaf id
   (`old_leaf_id`) before doing so.
2. From that earlier position, the handler runs the user's prompt through
   `agent.prompt(&payload, Some(s))` (or the `summarize()` non-streaming
   surface, see §6.5). The resulting assistant message is the **summary**.
3. The handler **moves the leaf pointer back** to `old_leaf_id` — the
   original session position is restored unchanged.
4. The handler **appends a `Compaction` entry** as the new leaf (parent =
   `old_leaf_id`, exactly like the `truncate` / `text only` paths today),
   with `firstKeptEntryId = <entry-id>`, `summary = <llm response>`, and
   `tokensBefore` computed from the same `build_context()` value the header
   showed. The session file is in exactly the same shape as a `truncate` /
   `text only` compaction — only the `summary` field carries different
   content.

Because the only session writes are (1) the final `append_compaction`, this
preserves the bidirectionally-compatible compaction entry shape and is
indistinguishable in storage from a non-LLM compaction to anyone reading the
session later (including pi).

### 6.3 The "switch back" semantics are a no-op for the user

Switching the leaf and then switching it back is **purely a side effect of
how the LLM gets the right context**. The user sees:

1. Their prompt is committed to the scrollback.
2. The "Thinking…" indicator appears (already emitted by
   `PicoAgent::prompt`).
3. The LLM streams its response.
4. **Nothing else happens on screen**. No `/tree navigated (…)` line, no
   "forked from <id>", no "switched back to <id>". The committed prompt is
   the entire record of the action — the same self-documenting convention
   as `/model`, `/session <id> resume`, `/tree <id> switch`, and `/tree <id>
   compact truncate` (see `doc/slash_menu.md` §5).
5. The compaction entry is appended (in-memory + on disk) but **not echoed
   in the scrollback**. Only `text only` and now `llm` produce visible
   summary content; in both cases the summary itself is rendered as
   markdown directly after the committed prompt — the single
   intentional inline output (§5.3 of this doc).

If the LLM call fails (network error, rate limit, refusal), the leaf pointer
is **still** restored before the error notice is shown, so the session is
left in a state identical to before the user pressed Enter — nothing was
appended, nothing was branched. The error itself prints via `display_text`
/ `display_error` (the only inline output besides the summary markdown).

### 6.4 Compaction point vs. leaf target

The `truncate` / `text only` modes keep `firstKeptEntryId = <entry-id>`
exactly. For LLM mode, the prompt is **run** at the entry **before**
`<entry-id>` on the current branch — call it `predecessor_id`. That way:

- The LLM sees everything *up to but not including* `<entry-id>` in its
  prior context (because `build_context()` is rooted at the current leaf,
  and we just moved the leaf to `predecessor_id`).
- The summary therefore describes the conversation *before* `<entry-id>`,
  which is exactly the span the compaction entry is dropping.
- The compaction entry's `firstKeptEntryId = <entry-id>` is unchanged, so
  the kept range starts at the same point the user asked for.

If `<entry-id>` is the root of the branch (no predecessor), refuse with
`Cannot compact: nothing before this point` — the same guard already in
`truncate` / `text only`. If `<entry-id>` is the **current leaf** (the
trivial case where the user just picked where they are), refuse with
`Cannot compact: nothing before the current position` — same rationale.

The predecessor lookup is a single `SessionIndex::children` traversal:
take `ctx.leaf_id`, find `target_id` in the flattened active-path branch
list (already available in `ReplCtx::tree` + the active-path helper used by
`compact_summary_header`), and pick the entry immediately preceding it on
that path. This is the same logic the header already uses to count
`drops N entries` — we just don't pay for it twice.

### 6.5 Streamed vs. non-streamed summary

`PicoAgent::summarize` (the existing non-streaming `chat()` surface used by
`/tree switch --summarize`) is a natural fit and produces a cleaner UX:

- It already runs a single user turn against the model with no hooks,
  reasoning-strip toggle, or tool-call loop.
- It returns the final response string directly — no `output.begin_response()`
  / streaming dance, which would otherwise interleave with the user's
  just-committed prompt in the scrollback.

Use `summarize`, **not** `prompt`. `prompt` is meant for live agent turns;
it would emit "Thinking…", stream the response, possibly run tools — all of
which is wrong for a summary step whose output is destined to be wrapped
in `<summary>…</summary>` in the compaction entry.

### 6.6 The summary request shape

The handler moves the leaf to the entry immediately before the compaction
point, then runs `summarize_with_session_context(session, user_payload)`:

- The **request prefix** is the live conversation context at that position,
  built by the same `session_context_messages` helper a normal user prompt
  uses (`Session::build_context()` + `maybe_strip_reasoning`) — so the
  request shares the provider's prefix cache with the live turns.
- The **final user message** is `<user_payload>` (the summarization
  instruction). No flattened transcript is embedded: the real messages ARE
  the context.

`<user_payload>` is, in order of precedence:

1. the trailing text after `llm` on the committed line (verbatim, no quote
   parsing — see §6.7), if non-empty;
2. otherwise the value of `llm_compact_prompt` (editable via
   `/set llm_compact_prompt …`), which itself defaults to the built-in
   `DEFAULT_LLM_COMPACT_PROMPT` from §6.9 when the setting is empty.

A prior compaction active at the prefix re-activates as the leading
`<summary>` wrapper via the compaction transform — the incremental-compaction
merge behaviour is structural (see `doc/nested_compactions.md` phase B). The
model returns a free-form `String`, which is written verbatim into the
compaction entry's `summary`.

If the LLM response is empty or whitespace-only, treat it as a soft failure:
display `Summary was empty; nothing written.` and do not append the
compaction entry (or append it with `summary = ""` = truncate-equivalent,
decided in review — empty summary reverts to `truncate` semantics, which is
already a documented divergence from pi's empty-wrapper behavior).

### 6.7 UI changes

#### New overlay variant

A new prompt-driven surface is needed: the prompt is the agent message, so
the user must be able to edit it freely. Reuse `CompactSummary` for option
selection (`truncate` / `text only` / `llm`); only the Enter handler for
`llm` differs from the other two.

```rust
/// `/tree <entry-id> compact` summary-mode sub-menu. Now with three modes.
/// `header` carries the informational rows (selected entry, dropped/kept
/// counts, token estimate) — the in-menu equivalent of `SessionAction`'s
/// per-session info header. No such info is ever printed inline after the
/// commit.
CompactSummary {
    target_id: String,
    header: Vec<String>,
    /// Fuzzy-filtered mode labels (truncate, text only, llm).
    opts: Vec<String>,
    active: Option<usize>,
    _filter: String,
},
```

The `opts` list is now `["truncate", "text only", "llm"]`. The
`filter_tree_options` helper that already powers this overlay handles the
new label unchanged.

#### `handle_enter` — three branches

```rust
ActiveOverlay::CompactSummary { target_id, opts, .. } => {
    if let Some(label) = opts.get(sel) {
        match label.as_str() {
            "truncate"  => commit_and_submit(format!("/tree <{}> compact truncate", target_id)),
            "text only" => commit_and_submit(format!("/tree <{}> compact text-only", target_id)),
            "llm" => {
                let buf = editor.text().to_string();
                let prefix = format!("/tree <{}> compact llm", target_id);
                let already_typed = buf
                    .strip_prefix(&prefix)
                    .map(|rest| !rest.trim().is_empty())
                    .unwrap_or(false);
                if already_typed {
                    // Buffer IS the canonical line — commit + submit verbatim.
                    // Covers the case where the user has typed (or pasted)
                    // `/tree <id> compact llm <prompt>` directly, and the
                    // overlay re-opened on Enter; pressing Enter again must
                    // actually run the compaction, not re-stage the line.
                    commit_and_submit(buf);
                } else {
                    // Stage the default prompt for editing.
                    editor.set(&format!("{} {}", prefix, settings.llm_compact_prompt()));
                }
            }
            _ => {}
        }
    }
}
```

This is the **only** point where the slash-menu convention (`emit_commit` +
`Submit` on leaf) deviates — and it deviates *outward*, into the standard
prompt edit surface, which is what the prompt-editing convention expects.
The `llm` option is special because the line carries an editable payload;
this is the same shape as, e.g., the `/model` flow's "type to filter then
Enter to commit" pattern, except that here the trailing text *is* the
payload. As soon as the buffer ends with non-empty text after `compact
llm `, the line is treated as canonical and Enter commits it — so the
second Enter (after staging) runs the compaction rather than re-staging.

The `already_typed` check is what prevents the "Enter just re-stages the
default" loop: the user picks `llm` from the menu → buffer becomes
`/tree <id> compact llm <default>` → `from_buffer` re-opens the overlay
(filtering by `llm <default>`); pressing Enter detects the trailing text
and submits.

#### `from_buffer` — first-word promotion for `CompactSummary`

When the buffer is `/tree <id> compact llm <payload>`, the existing
`filter_tree_options` fuzzy pre-selection can return the wrong active row:
SkimMatcherV2 scores the long payload against the short mode labels
(`truncate`, `text only`, `llm`) and frequently returns an empty score
list (none of the labels are a superstring of the payload), so the
function falls through to `(all_labels, Some(0))` — i.e. `truncate` is
highlighted even though the user typed `llm`. To keep the highlighted row
in sync with what the user actually typed, `from_buffer` post-processes
the active position: if the first whitespace-delimited token of
`sub_filter` matches a known mode label (with `text only` ↔ `text-only`
normalisation), that mode becomes the active row regardless of the fuzzy
result. Empty `sub_filter` (the user just typed `compact`) keeps the
default `Some(0)` (truncate). This is purely a UI affordance; the handler
in `handle_enter` also self-checks the buffer via
`llm_compact_buffer_is_canonical` so the staged line is always honored.

`from_buffer`'s first-word promotion sets the overlay's `active` row. The
`read_input` loop then **moves the actual cursor to `active`** for the tree
action sub-menus (`TreeAction`/`SwitchAction`/`CompactSummary`) whenever the
menu opens or the buffer re-derives a different `active`
(`Repl::cursor_sel_policy`). This closes the gap where restoring
`/tree <id> compact llm <payload>` from history (or typing it) left the
cursor on row 0 (`truncate`) even though `llm` was marked `*` — Enter would
have committed `truncate`. Now the cursor follows the prompt's spelled-out
mode, so Enter on a restored `/tree <id> compact llm <payload>` runs the
LLM compaction. Arrow keys still win: they change only `sel`, not `active`,
so a stable `active` leaves the cursor where the user moved it.

#### `handle_tab`

Tab on the `llm` row fills `/tree <{}> compact llm <default prompt>` —
identical to `handle_enter`'s `llm` branch, so Tab and Enter on the `llm`
row do the same thing (stage the editable prompt). Tab on `truncate` /
`text only` stays a one-line commit-fill as today.

#### `handle_esc` — unchanged

Esc still rewinds one level: `CompactSummary` → `/tree <{}>`, → `/tree `,
→ empty. The LLM-prompt staging lives in the editor buffer and is
preserved across Esc unless the user keeps rewinding (in which case the
staged line is lost, same as any other edit — no special handling).

#### `parse_tree_args` — new optional tail

The `compact` branch of `parse_tree_args` grows a third mode plus a free-text
tail:

```rust
fn parse_tree_args(rest: &str) -> (String, TreeAction, Option<CompactSummaryMode>, bool, String) {
    let mut summarize = false;
    let mut target = String::new();
    let mut action = TreeAction::Switch;
    let mut mode: Option<CompactSummaryMode> = None;
    let mut llm_payload = String::new();
    let mut seen_compact = false;
    let mut after_mode = false;
    for part in rest.split_whitespace() {
        match part {
            "--summarize" | "-s" => summarize = true,
            "switch"   => action = TreeAction::Switch,
            "compact"  => { action = TreeAction::Compact; seen_compact = true; }
            "truncate" | "none" | "no-summary"
                if seen_compact => mode = Some(CompactSummaryMode::Truncate),
            "text-only" | "text"
                if seen_compact => mode = Some(CompactSummaryMode::TextOnly),
            "llm" | "summary"
                if seen_compact => {
                    mode = Some(CompactSummaryMode::Llm);
                    after_mode = true;
                }
            id if !seen_compact => {
                // The overlay commits the id angle-bracketed; strip the
                // brackets — the raw id is what move_leaf/get_entry expect.
                let t = id.trim();
                let t = t.strip_prefix('<').unwrap_or(t);
                let t = t.strip_suffix('>').unwrap_or(t);
                target = t.to_string();
            }
            _ if after_mode && mode == Some(CompactSummaryMode::Llm) => {
                // Anything after the `llm` token is part of the payload;
                // re-join with single spaces so multi-word prompts survive
                // split_whitespace's collapse.
                if !llm_payload.is_empty() { llm_payload.push(' '); }
                llm_payload.push_str(part);
            }
            _ => {}
        }
    }
    (target, action, mode, summarize, llm_payload)
}
```

The trailing `String` is empty for `truncate` and `text only` (today's
behavior, no caller breakage), and carries the user's prompt tail for
`llm`. The branch-summarize payload for `switch --summarize` is **not**
modeled here — `switch --summarize` keeps its fixed prompt and remains the
unrelated path described in `agent.rs:2902`.

**No quote parsing.** The trailing text is taken verbatim (whitespace-joined
by the `split_whitespace` re-join); no quote stripping, no escaping. This
matches the rest of the slash menu (`/session <id> rename <newname>`,
`/set <key> <value>`, etc.).

#### Header (unchanged, plus one new informational line for `llm`)

The header still shows `drops N`, `keeps M`, `~Xk tokens before`, and
`written into this session`. For `llm` mode, an additional line is
appended: `summary generated by LLM (~default prompt)`. This is the
in-menu equivalent of the existing inline summary markdowns for `text only`
— keeps the user oriented about what is about to happen without any inline
output after the commit.

### 6.7b Summary review & edit (before the entry is written)

For the summary-producing modes (`text only`, `llm`), the generated draft
is **reviewed by the user before** the `Compaction` entry is appended:

1. `run_compaction` builds the draft (deterministic text-only summary, or
   the LLM summary after the leaf dance), then calls the caller-supplied
   review hook (`CompactionSummaryReview` in `src/agent.rs`).
2. In the interactive REPL, the hook drops raw mode and opens the draft in
   `$EDITOR` (reuses `edit_in_editor` — the Ctrl-E prompt editor), re-enters
   raw mode, then shows an **Accept / Cancel** overlay via
   `Output::confirm_compaction_summary` with **Accept as the Enter default**
   (Esc / `c` cancels).
3. On **Accept**, the (possibly edited) text is stored as the entry's
   `summary`. On **Cancel** (or editor failure), `run_compaction` returns
   `Ok(())` without appending — no compaction entry is written and the
   session is untouched (for `llm` mode the leaf was already restored).

Outside the interactive REPL (quiet / pipe / tests) the hook is absent and
the draft is stored as-is — behavior is unchanged from before this step.
`truncate` (empty summary) never opens the editor.

### 6.8 Handler implementation sketch (`src/agent.rs`)

The `TreeAction::Compact` arm of the main-loop `/tree` block gains an `Llm`
mode branch, slotted into the same `match mode.unwrap_or(CompactSummaryMode::Truncate)`
that already picks between `Truncate` and `TextOnly`:

```rust
TreeAction::Compact => {
    let Some(ref s) = session else { /* error: no session */ continue };
    let old_leaf_id = s.get_leaf_id().await;

    // NOTE (superseded): the pi-parity leaf-is-compaction refusal was
    // removed in doc/nested_compactions.md phase C. The LLM-mode
    // empty-dropped-span guard below covers the no-op case.
    // pi parity: refuse if the leaf IS a compaction entry.
    if let Some(id) = &old_leaf_id {
        if let Some(entry) = s.get_entry(id).await {
            if matches!(entry.content, SessionEntryContent::Compaction { .. }) {
                config.output.display_text(
                    "Cannot compact: the session was already compacted at this point");
                continue;
            }
        }
    }

    if s.get_entry(&target_id).await.is_none() {
        config.output.display_text(/* unknown target */);
        continue;
    }

    // Compute tokensBefore from the CURRENT scoped context, exactly like
    // the other two modes (pi semantics). The value is the same regardless
    // of which summary mode is selected — the header and the written entry
    // agree on it.
    let tokens_before = match s.build_context().await {
        Ok(ctx) => estimate_context_tokens(&ctx.messages),
        Err(_) => 0,
    };

    match mode.unwrap_or(CompactSummaryMode::Truncate) {
        CompactSummaryMode::Truncate => { /* unchanged path */ }
        CompactSummaryMode::TextOnly => { /* unchanged path */ }
        CompactSummaryMode::Llm => {
            // 1. Find the entry just before target_id on the active branch.
            let predecessor_id = {
                let index = s.index().lock().await;
                find_predecessor_on_active_branch(&index, &target_id)
            };
            let Some(predecessor_id) = predecessor_id else {
                config.output.display_text(
                    "Cannot compact: nothing before this point");
                continue;
            };
            if Some(&predecessor_id) == old_leaf_id.as_ref() {
                // target_id is the current leaf (trivially), nothing to
                // compact — refuse.
                config.output.display_text(
                    "Cannot compact: nothing before the current position");
                continue;
            }

            // 2. Build the dropped span = branch entries strictly BEFORE
            //    target_id, using the same path-walk the text-only path
            //    already uses. Reuse build_summary_prompt for consistency.
            let dropped = {
                let leaf = old_leaf_id.clone().unwrap_or_default();
                let index = s.index().lock().await;
                let branch = index.get_path_to_root(&leaf);
                branch.into_iter()
                    .take_while(|e| e.id != target_id)
                    .collect::<Vec<_>>()
            };
            let span = build_summary_prompt(&dropped);

            // 3. Compose the user-tweakable payload:
            //    <settings.llm_compact_prompt>\n\n--- BEGIN DROPPED SPAN ---\n
            //    <span>\n--- END DROPPED SPAN ---
            //    The committed-line `llm_payload` from parse_tree_args is
            //    empty when the user accepted the default; fall back to the
            //    setting in that case.
            let user_prompt = if llm_payload.is_empty() {
                settings.llm_compact_prompt()
            } else {
                llm_payload.clone()
            };
            let composed = format!(
                "{}\n\n--- BEGIN DROPPED SPAN ---\n{}\n--- END DROPPED SPAN ---",
                user_prompt, span);

            // 4. Switch the leaf to the predecessor. Remember the original
            //    position (old_leaf_id was captured above).
            let _ = s.move_leaf(Some(predecessor_id.clone())).await;

            // 5. Run the LLM. Use summarize(), not prompt(), for the
            //    reasons in §6.5.
            let summary_text = match agent.as_ref() {
                Some(a) => match a.summarize(&composed).await {
                    Ok(s) => s,
                    Err(e) => {
                        // 5a. Failure: switch the leaf BACK to
                        // old_leaf_id, surface the error, do not append.
                        let _ = s.move_leaf(old_leaf_id.clone()).await;
                        config.output.display_text(&format!(
                            "Summary failed: {}", e));
                        continue;
                    }
                },
                None => {
                    let _ = s.move_leaf(old_leaf_id.clone()).await;
                    config.output.display_text("Cannot compact: no agent");
                    continue;
                }
            };

            // 6. Empty response: soft-fail like §6.6 (decided in review:
            //    either skip or fall through to truncate semantics).
            let summary_text = summary_text.trim().to_string();
            if summary_text.is_empty() {
                let _ = s.move_leaf(old_leaf_id.clone()).await;
                config.output.display_text(
                    "Summary was empty; nothing written.");
                continue;
            }

            // 7. Switch the leaf back to old_leaf_id.
            let _ = s.move_leaf(old_leaf_id.clone()).await;

            // 8. Append the compaction entry — same shape as the other
            //    modes. The session file ends up indistinguishable from a
            //    non-LLM compaction with the same `summary` content.
            match s.append_compaction(
                &summary_text, &target_id, tokens_before).await {
                Ok(_) => {
                    // 9. Render the summary as markdown below the
                    //    committed prompt (the single intentional inline
                    //    output for summary-producing modes; see Phase 5.3).
                    config.output.display_separator();
                    config.output.display_markdown(&summary_text);
                }
                Err(e) => {
                    config.output.display_text(&format!(
                        "Cannot compact session: {}", e));
                }
            }
        }
    }
    continue;
}
```

**Helper, on `SessionIndex`:**

```rust
/// Walk the active branch (root → leaf) and return the entry id that
/// immediately precedes `target_id`. Returns `None` if `target_id` is not
/// on the active branch or is the root.
fn find_predecessor_on_active_branch(
    index: &SessionIndex,
    target_id: &str,
) -> Option<String> {
    let leaf = index.leaf_id.as_deref()?;
    let branch = index.get_path_to_root(leaf); // root → leaf
    let pos = branch.iter().position(|e| e.id == target_id)?;
    if pos == 0 {
        None
    } else {
        Some(branch[pos - 1].id.clone())
    }
}
```

This is O(branch length) and uses the already-cached path; it does not
touch the disk or hold the index lock across awaits.

### 6.9 Settings — `llm_compact_prompt`

A new managed setting, alongside `default_model`, `security_mode`, etc.,
following the existing pattern in `src/settings.rs`:

```rust
pub enum SettingKey {
    // …existing…
    /// User-tweakable prompt appended to `/tree <id> compact llm`. Empty
    /// or unset falls back to a built-in default
    /// ("Summarize the conversation so far in less than 2000 words…").
    /// Free-text.
    LlmCompactPrompt,
}

// In SETTINGS:
SettingMeta {
    key: SettingKey::LlmCompactPrompt,
    name: "llm_compact_prompt",
    description: "Prompt prepended to the LLM-driven compaction summary",
    kind: ValueKind::FreeText,
},

// In SettingsStore:
pub llm_compact_prompt: String,

// Built-in default constant in settings.rs:
pub const DEFAULT_LLM_COMPACT_PROMPT: &str =
    "Summarize the conversation so far in less than 2000 words. \
     Preserve the key technical decisions, file paths touched, current \
     task state, and any unresolved questions. Drop reasoning traces \
     and tool-call boilerplate.";

// Methods: value(), validate() (always Ok for FreeText), apply(), set_value(),
// save() — all the same shape as the existing FreeText-style setting fields.

// In Config (src/config.rs):
#[serde(default)]
pub llm_compact_prompt: Option<String>,
```

`SettingsStore::llm_compact_prompt()` returns the stored value, falling
back to `DEFAULT_LLM_COMPACT_PROMPT` when empty.

The settings menu gets a new row automatically (it iterates `SETTINGS`); the
existing `parse_settings` / `filtered_value_candidates` / `set_value` paths
handle `ValueKind::FreeText` already (or gain the trivial arm if not — the
existing `ReasoningInContext` and `SecurityMode` are the smallest analogs to
copy).

### 6.10 Why this is *not* another slash command

A `/compact` command (separate from `/tree`) was considered and rejected:

- `/compact` would have to re-implement the tree point selection that
  `/tree` already owns. Two ways to do the same selection is a UX regression.
- The slash-menu convention is that **nested menus are visible in the
  prompt** — `/tree <id> compact llm <prompt>` reads exactly like "compact
  this tree point using an LLM with this prompt", which is what the user
  means.
- The compaction entry shape (a `Compaction` node with `firstKeptEntryId`)
  is intrinsically tree-pointed. Decoupling from `/tree` would just shift
  the same id-resolution code into a new command.

The LLM-payload tail (`<prompt>` after `llm`) is the *one* way this
differs from the other `compact` modes, and that fits naturally inside the
prompt-edit surface that the slash-menu convention already expects users
to use (`/set <key> <free-text-value>` is the closest precedent).

---

## Edge cases & risks

| Case | Behavior |
| --- | --- |
| `target_id` not on the current branch | Validate with `get_entry` before writing; error message, session unchanged (avoids a silently no-op compaction). |
| `target_id` == current leaf | Appending a compaction with `firstKeptEntryId` = leaf scopes to the last entry only; allowed but pointless. Consider warning "nothing before this point" — decide in review. |
| Leaf is already a compaction entry | Allowed (guard removed — see doc/nested_compactions.md phase C). `Truncate`/`TextOnly` write a fresh compaction; `Llm` on an empty dropped span is still refused by the "nothing before the current position" guard. |
| Empty `summary` in context | No summary message emitted (documented divergence from pi's empty wrapper). |
| Repeated compactions | Latest compaction scopes the context; older nested compactions inside the kept range are **stepped over** (not re-emitted — see doc/nested_compactions.md phase A). LLM compaction mode injects the prior summary as `--- PREVIOUS SUMMARY ---` (phase B). |
| Session written by pi, read by picocode (and vice versa) | Schema identical; `build_index`/`get_path_to_root` treat compaction as a normal node; `build_context` scopes. Verify with round-trip test (below). |
| `firstKeptEntryId` missing on branch | `apply_transforms` falls back to the full branch (defensive, matches pi's graceful handling). |
| `tokensBefore` accuracy | Heuristic only; used for the sub-menu header display and future prompts — no correctness dependency. Header and written value come from the same `estimate_context_tokens(build_context())` call. |
| Replay of a compacted session | Markers render, full history from root is replayed; `Nt`/`Nl` limits count turns (markers don't count). |
| `NavigateTree` removal | Only emitter is `repl.rs:1344`; grep after editing. |
| Phase 6 — LLM compaction: `<entry-id>` has no predecessor | **Done.** Refuse: `Cannot compact: nothing before this point`. |
| Phase 6 — LLM compaction: `<entry-id>` == current leaf | **Done.** Refuse: `Cannot compact: nothing before the current position`. |
| Phase 6 — LLM compaction: leaf is already a compaction | **Superseded.** The refusal was removed (doc/nested_compactions.md phase C); the empty-dropped-span guard still protects the no-op case. |
| Phase 6 — LLM compaction: `agent.summarize` fails | **Done.** Restore `old_leaf_id` before surfacing the error; session unchanged (no compaction entry appended). |
| Phase 6 — LLM compaction: empty/whitespace response | **Done.** Soft-fail: `Summary was empty; nothing written.`; restore leaf; do not append. |
| Phase 6 — LLM compaction: panic/unwind during `summarize` | Documented failure path is the only safety net; no Drop guard implemented. |
| Phase 7 — `/compact` with no user message on the active branch | `from_buffer` returns `ActiveOverlay::None` (silent). Mirrors the no-leaf behavior; user can `!cmd` or `/new` to bootstrap. |
| Phase 7 — `/compact` mid-turn (leaf is a user prompt, no response yet) | `last_user_message_id_on_active_branch` returns the leaf id; dropped span = everything before the leaf; kept range starts with the prompt. |
| Phase 7 — `/compact` at the root (single user message, no response) | `Truncate` / `TextOnly` are legal no-ops on an empty dropped span; `Llm` refuses (`Err("nothing before this point")`). |
| Phase 7 — last user message is itself a compaction | **Superseded.** The compaction-refuse check was removed (doc/nested_compactions.md phase C); `/compact` now resolves normally and may write a new compaction over a compaction leaf. |

## Phase 7 — Compaction unification: shared `run_compaction` + `/compact`

**Goal:** extract the per-mode mechanics of `/tree <id> compact` into a
shared `run_compaction` helper so the same backend serves both the
explicit-target path and the new `/compact` slash command (which
auto-resolves to "compact at the start of the current turn"). A
`CompactionTarget::Abandoned` variant is wired up but currently dormant,
ready for a future `/tree <id> switch --summarize` UI extension.

### 7.1 What changed

- `src/agent.rs`:
  - New `CompactionTarget` enum (`AtPoint { target_id, tokens_before }`
    for `/tree <id> compact` and `/compact`; `Abandoned { common_ancestor_id }`
    for future switch-summarize). Marked `#[allow(dead_code)]` until the
    abandoned-branch UI lands.
  - New `pub(crate) async fn run_compaction(session, agent, settings, target, old_leaf_id, mode, llm_payload, output) -> Result<(), String>`
    — shared backend that takes a `CompactionTarget` and one of the
    three `CompactSummaryMode` variants and produces a `Compaction`
    entry (or, eventually, a `BranchSummary`).
  - The `TreeAction::Compact` arm in the main loop is now a thin
    validation block followed by a single `run_compaction(...)` call.
    The LLM-mode predecessor lookup, the move-to-predecessor /
    summarize / move-back dance, the empty-response check, the
    restore-on-failure path — all of it moved into the helper.
- `src/repl.rs`:
  - `compact` arm in `from_buffer` now resolves `target_id` via the
    new `Repl::last_user_message_id_on_active_branch(&ctx.tree, leaf_id)`
    helper (instead of using the current leaf). If the active branch
    has no user message, the overlay returns `ActiveOverlay::None`
    (silent — mirrors the existing no-leaf behavior).
  - `CompactPrefix` enum removed; `CompactSummary` no longer carries a
    `prefix` field. Both `/tree <id> compact` and `/compact` commit the
    same canonical line: `/tree <{target_id}> compact <mode> [payload]`.
  - `llm_compact_buffer_is_canonical(buffer, target_id)` simplified:
    one prefix (`/tree <id> compact llm`) since the `AtCurrent`
    variant is gone.
  - `Repl::is_user_message(entry)` and
    `Repl::last_user_message_id_on_active_branch(tree, leaf_id)` added
    to walk the active branch leaf→root and return the most recent
    user-role entry id.
  - `"compact"` added to the "open without trailing space" list in
    `from_buffer` (so `/compact` without a trailing space opens the
    overlay instead of the `CommandList`).

### 7.2 The `/compact` rewrite

`/compact` is a single shortcut that surfaces the compaction menu at the
current position. The two paths to a `Compaction` entry are now
indistinguishable from the session file's perspective:

| User action | Committed line | `target_id` | Dropped span |
|---|---|---|---|
| `/tree <id> compact truncate` | (echoed in scrollback) | `<id>` (as picked) | branch entries strictly before `<id>` |
| `/tree <id> compact text-only` | (echoed) | `<id>` | same |
| `/tree <id> compact llm [payload]` | (echoed; payload staged for editing) | `<id>` | same |
| `/compact truncate` | `/tree <last_user_id> compact truncate` | last user message on active branch | same as the explicit form would produce with that id |
| `/compact text-only` | same | same | same |
| `/compact llm [payload]` | same | same | same |

The committed line is what `emit_commit` echoes to scrollback. Because
the line is the same canonical form for both paths, there's no
distinction in the session file: `append_compaction` is called with
the same arguments, and the same `Compaction` entry shape is written.

### 7.3 The shared `run_compaction` signature

```rust
pub(crate) enum CompactionTarget {
    AtPoint {
        target_id: String,
        tokens_before: u64,
    },
    Abandoned {
        #[allow(dead_code)]
        common_ancestor_id: String,
    },
}

pub(crate) async fn run_compaction(
    session: &Session,
    agent: &Option<Box<dyn PicoAgent>>,
    settings: &SettingsStore,
    target: CompactionTarget,
    old_leaf_id: &str,
    mode: CompactSummaryMode,
    llm_payload: &str,
    output: &dyn Output,
) -> std::result::Result<(), String>;
```

The caller is responsible for the up-front validation (active session,
known target, leaf isn't a compaction, LLM-mode predecessor check).
`run_compaction` only handles the per-mode mechanics:

- `Truncate` → `append_compaction("", first_kept_entry_id, tokens_before)`.
- `TextOnly` → build deterministic summary, `append_compaction`, render inline.
- `Llm` → find predecessor (returns `Err("nothing before this point")` if missing),
  build the dropped span, compose `<user-prompt>\n\n--- BEGIN DROPPED SPAN
  ---\n...\n--- END DROPPED SPAN ---`, `agent.summarize`, move leaf back,
  `append_compaction`, render inline. On any failure (no agent, summarize
  Err, empty response) restore the leaf, surface the error, return
  `Err(msg)`.

### 7.4 Why `/compact` rewrites to `/tree <last_user_id> compact` (not `/compact <mode>`)

The original `a1409ab` commit added `/compact` as a separate canonical
form (`/compact <mode>`), but:

- The main loop had no `/compact` arm — the line fell through to the
  normal prompt path and was sent to the LLM as a regular message.
  The command was effectively broken.
- The committed line was harder to spot in scrollback and obscured the
  underlying mechanism.

Rewriting at the overlay layer collapses both into the same canonical
form. The user typing `/compact` and pressing Enter sees
`> /tree <0194abc> compact truncate` in scrollback (where `<0194abc>` is
the resolved last user message) — which doubles as a discoverable hint
toward the more general "compact at an arbitrary tree point" capability.
The main loop stays single-entry-point (one `TreeAction::Compact` arm),
the parser is unchanged (`parse_tree_args` already handles
`<id> compact <mode> [payload]`), and `CompactPrefix` is removed.

The `target_id` resolution (last user message, not current leaf) is the
key UX point. Under the original `/tree <leafid> compact` semantics,
the assistant's last response is kept and the user's last prompt is in
the dropped span (and gets summarized). For `/compact` "summarize the
conversation so far" the natural reading is the opposite: keep the
user's last prompt, drop everything older. The overlay layer makes
this choice (the "auto-resolve to the start of the current turn" rule)
and the resulting canonical line `/tree <last_user_id> compact <mode>`
is what the user sees and what the main loop executes.

### 7.5 The `Abandoned` variant — future use

The `CompactionTarget::Abandoned` variant exists so the same
`run_compaction` helper can serve `/tree <id> switch --summarize` when
the UI extension lands. The `dead_code` arm sits dormant and is the
only safety net: a `run_compaction(.., Abandoned { .. }, ..)` call today
returns `Err("abandoned-branch summary not yet wired up")`. The future
`SwitchAction` UI work will:

- Replace the binary `just switch` / `summarize` options with a
  three-way choice matching `CompactSummary`'s labels (or a nested
  sub-menu).
- Resolve the dropped span as the abandoned-branch entries (already
  computed by `collect_entries_for_branch_summary` in the current
  `TreeAction::Switch` arm).
- Compute `common_ancestor_id` from the existing helper and call
  `run_compaction(.., Abandoned { common_ancestor_id }, ..)`.
- Wire the new `Abandoned` arm in `run_compaction` to call
  `append_branch_summary` instead of `append_compaction`.

The backend change is intentionally small so the UI work is the only
follow-up.

### 7.6 Edge cases specific to `/compact`

| Case | Behavior |
|---|---|
| Active branch has no user message (empty session) | `from_buffer` returns `ActiveOverlay::None` (silent; same as the no-leaf path). |
| Current leaf is a user message (mid-turn, no response yet) | `last_user_message_id_on_active_branch` returns the leaf id. The dropped span = everything before the leaf (the prior turns). The kept range starts with the user's prompt. |
| Current leaf is the assistant's last response | Resolves to the user prompt immediately before it. The dropped span = everything before that prompt; the kept range = [user prompt, assistant response, ..., leaf]. |
| Last user message is itself a compaction entry | **Superseded.** The refusal was removed (doc/nested_compactions.md phase C); compaction over a compaction leaf resolves normally. |
| `/compact llm` at the start of the conversation | Empty dropped span → `Err("nothing before this point")` (same as `/tree <root> compact llm`). The other two modes are no-ops on an empty dropped span but still legal. |

---

## Test plan

1. **`apply_transforms` unit tests** (`session.rs`) — **done**: empty-summary scoping;
   non-empty pi-wrapper message; nested compaction stepped over; triple-nested
   only-latest-summary; missing `firstKeptEntryId` fallback; compaction entry
   never in output.
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
8. **Phase 6 tests (LLM compaction)** — **done**: `parse_tree_args_llm_mode`
   covers bare `llm`, the `summary` alias, multi-word payloads, and
   `text-only`-after-`llm`-is-payload; `find_predecessor_on_active_branch`
   covers chain predecessor lookup, root (no predecessor), missing target,
   and `leaf_id = None`. Handler-level failure-path and unit-level success
   coverage is left for manual smoke (the LLM call requires a model).
9. **Phase 7 tests (compaction unification)** — **done**:
   `last_user_message_id_on_active_branch` covers just-before-leaf
   assistant, mid-turn leaf-is-user, single-user root, no-user-message
   (returns `None`), and walking past compaction entries. The `/compact`
   overlay tests cover the leaf-is-user case, the leaf-is-assistant case
   (resolves to the just-before-leaf user), and the no-user-message
   case (overlay returns `None`). `run_compaction` tests cover
   `Truncate` (writes empty `Compaction` entry), `TextOnly` on empty
   span (empty summary) and non-empty span (contains the dropped
   user prompt), and `Llm` with no agent (returns `Err` and writes
   nothing). Manual smoke: the committed `/tree <id> compact <mode>`
   line should be byte-identical to the corresponding `/compact`
   rewrite (same `Compaction` entry in the session file).

---

## File-by-file checklist

| File | Change |
| --- | --- |
| `src/session/session.rs` | **Done.** `apply_transforms` rewritten (pi semantics, empty-summary skip, pi summary wrapper, repeated-compaction handling); `compaction_summary_message` helper + `COMPACTION_SUMMARY_PREFIX/SUFFIX` constants; unit tests. |
| `src/agent.rs` | **Done.** `TreeAction`/`CompactSummaryMode` enums + `parse_tree_args` (canonical `switch [--summarize]`, bare `--summarize` alias; `compact llm <payload>` with trailing free-text payload); `/tree` dispatch (switch + optional branch-summarize, no `/tree navigated` line / compact, no inline token output); `estimate_context_tokens` (+ `build_session_ctx` computing `ReplCtx::context_tokens`); replay: `ReplayUnit`, `build_replay_units`, `replay_units` (+ `count_turns`/`last_turns_start`/`replay_last_turns`); `/replay` + resume replay on the raw branch; `NavigateTree` handling **removed**; LLM compaction arm (predecessor lookup + `agent.summarize` + move-back + `append_compaction`); `find_predecessor_on_active_branch` helper. **Phase 7:** `run_compaction` helper + `CompactionTarget` enum (`AtPoint` / `Abandoned`); `TreeAction::Compact` arm refactored to call the helper (the per-mode inline LLM / text-only / truncate code moved into the helper); 4 new tests (`run_compaction_truncate_writes_empty_summary`, `_text_only_writes_deterministic_summary`, `_llm_no_agent_returns_err_and_no_compaction`, `_text_only_nonzero_dropped_span`). |
| `src/repl.rs` | **Done.** `ReplCtx::context_tokens` field; `TreeAction` + `SwitchAction` + `CompactSummary` (with `header`) overlay variants; `from_buffer` promotion with the angle-bracket id syntax (`Repl::parse_tree_id`) + fuzzy option filtering (`Repl::filter_tree_options`); `tree_entry_header` / `compact_summary_header` builders; `handle_enter` (commit+submit via `emit_commit` for leaves, buffer promotion otherwise); `handle_tab`/`handle_esc` (one level up); `display_rows` titles; `overlay_disc` (16/17/18); header-aware render arm; Tree Enter/Tab → `/tree <entry-id>` completion; `InputAction::NavigateTree` **removed**; `CompactSummary` opts gain `"llm"`; `handle_enter`/`handle_tab` branch on `llm` to stage the editable prompt (with `settings.llm_compact_prompt()` as the default). **Phase 7:** `last_user_message_id_on_active_branch` + `is_user_message` helpers; `compact` arm in `from_buffer` resolves `target_id` to the last user message on the active branch; `CompactPrefix` enum + the `prefix` field on `CompactSummary` **removed** (both paths now commit the same `/tree <id> compact <mode>` line); `llm_compact_buffer_is_canonical` simplified to take a `target_id` instead of an arbitrary `head`; `"compact"` added to the "open without trailing space" list in `from_buffer`; 8 new tests (5 for `last_user_message_id_on_active_branch`, 3 for `/compact` overlay resolution). |
| `src/settings.rs` | **Done.** `SettingKey::LlmCompactPrompt` + `SettingsStore::llm_compact_prompt` field; `DEFAULT_LLM_COMPACT_PROMPT` constant; `llm_compact_prompt()` method with default fallback; `value`/`validate`/`apply`/`save` arms; `from_agent_config`/`from_config` constructors; `SettingsStore` test fixtures updated. |
| `src/config.rs` | **Done.** `Config.llm_compact_prompt: Option<String>` field with `#[serde(default)]`. |
| `settings.toml.example` | **Done.** `llm_compact_prompt` example line (commented-out). |
| `src/slash.rs` | (optional) `/tree` help: "Navigate session history — switch, compact, or summarize branches". |
| `doc/tree_compaction.md` | this doc (status updated: Phases 1–6 implemented). |
