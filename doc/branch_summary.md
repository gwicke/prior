# `/tree <id> switch summarize` — branch summaries

Status: **implemented** (`agent::generate_branch_summary` /
`apply_branch_summary`, the `switch` → `summarize` → mode nodes in
`v25_tree.rs`, and the `BranchSummary` → user-message projection in
`extract_messages`).

`/tree <id> switch` is a sub-menu:

```text
/tree <id> switch                     → listing [cursor] [summarize]
/tree <id> switch cursor              → move the leaf, no summary (pi's plain `branch()`)
/tree <id> switch summarize           → listing [manual] [text-only] [prompt]
/tree <id> switch summarize manual    → touched-files template of the spur
/tree <id> switch summarize text-only → deterministic transcript + files of the spur
/tree <id> switch summarize prompt    → LLM digest; trailing text = extra focus
```

The three modes are the *same* two-phase flow as `/tree <id> compact`
(`doc/editor_cleanup.md` §6): `prepare()` drafts → `$EDITOR` → the engine's
native confirm → `submit()` writes. Only the effect differs, and only in
`submit()`:

- span summarized
  - **`/tree <id> compact <mode>`**: entries **before** the target on its branch
  - **`/tree <id> switch summarize <mode>`**: the **spur** left behind by the
    switch
- entry written
  - **`/tree <id> compact <mode>`**:
    `Compaction { summary, first_kept_entry_id, tokens_before }`
  - **`/tree <id> switch summarize <mode>`**:
    `BranchSummary { from_id, summary }`
- leaf
  - **`/tree <id> compact <mode>`**: unchanged
  - **`/tree <id> switch summarize <mode>`**: moves to the target
- context after
  - **`/tree <id> compact <mode>`**: `[summary] + [first_kept …]`
  - **`/tree <id> switch summarize <mode>`**:
    `[digest] + [full branch from the target]`

`SummaryAction` (`v25_tree.rs`) is that one difference; `compact_prepare` /
`edit_draft` / `compact_submit` are shared verbatim.

## pi parity — verified against `~/src/pi`

The placement is the whole feature, and it is the opposite of what the deleted
prior code did (`git show 09fcbd4^:src/agent.rs`, which wrote the summary at
the **spur tip** with `from_id` = the **common ancestor**):

- `SessionManager.branchWithSummary` (`core/session-manager.ts:1302`) sets
  `leafId = branchFromId` **first**, then appends the entry with
  `parentId = branchFromId` and `fromId = branchFromId ?? "root"`. So the
  summary is **inline on the branch the conversation continues on** and is an
  ordinary ancestor of every later message.
- `navigateTree` (`core/agent-session.ts:2946`) calls it with `newLeafId`, with
  the comment *"Summary is attached at the navigation target position
  (newLeafId), not the old branch"*. No summary → plain `branch()` /
  `resetLeaf()`.
- `buildContextEntries` (`core/session-manager.ts:414`) walks **only** the leaf
  path; `sessionEntryToContextMessages` (`:397`) maps `branch_summary` (with a
  non-empty summary) to a `branchSummary` message, which `convertToLlm`
  (`core/messages.ts:170`) renders as a **user** message wrapped in
  `BRANCH_SUMMARY_PREFIX` / `BRANCH_SUMMARY_SUFFIX` (`:19-24`). Nothing ever
  searches side spurs for summaries — pi's own test
  (`test/session-manager/build-context.test.ts:247`) asserts a summary in a
  sibling subtree is **absent** from the main path's context.

So the summary reaches the model without any traversal: it is on the path
because of where it is written. Our `session::extract_messages` mirrors
`sessionEntryToContextMessages` (same non-empty guard, same position in the
path). The *wrapper text* is ours, not pi's: `78c7e7a` shortened
`COMPACTION_SUMMARY_PREFIX` from pi's sentence to `Compaction summary:`, and the
branch wrapper follows that house style (`Branch summary:`). The one detail
kept from pi is structural — the branch suffix has **no** leading newline while
the compaction suffix has one, so a nested compaction inside a serialized spur
still reads as two distinct wrappers.

### Summarization pipeline (ported)

From `core/compaction/branch-summarization.ts`:

- `collectEntriesForBranchSummary` → `agent::abandoned_spur`: the entries after
  the **deepest** common ancestor on the old leaf's path, chronological. The
  ancestor is excluded, and forward navigation (target is a descendant) yields
  an empty spur — pi then skips the entry entirely, which we surface as an
  error rather than a silent no-op.
- `BRANCH_SUMMARY_PREAMBLE` is prepended to the **stored** summary
  (`The user explored a different conversation branch before returning here.`),
  so the framing shows in `/tree` too, not only inside the LLM wrapper.
- `BRANCH_SUMMARY_PROMPT` → `DEFAULT_BRANCH_SUMMARY_PROMPT`, byte-exact
  (`## Goal / ## Constraints & Preferences / ## Progress (Done, In Progress,
  Blocked) / ## Key Decisions / ## Next Steps`). A trailing instruction is
  **appended** as `Additional focus: …` (pi's default), so a user instruction
  cannot break the required sections.
- `<conversation>…</conversation>` + `serializeConversation` role tags
  (`[User]:`, `[Assistant]:`, `[Assistant tool calls]:`) →
  `agent::serialize_spur`.
- Touched files are appended to the summary (`spur_files_section`), reusing the
  collector and renderer the text-only summary uses.

## Deliberate divergences

- **No system prompt.** pi sends `SUMMARIZATION_SYSTEM_PROMPT` alongside its
  instructions; `CodeAgent::summarize` is a single user-message request, and the
  instruction text is self-contained.
- **Tool results skipped** (as in pi — "context is in the assistant's tool
  call"), but **reasoning blocks omitted**, where pi serializes
  `[Assistant thinking]`. Consistent with `build_text_only_summary` and the
  `reasoning_in_context` setting.
- **No `details` / `fromHook` fields.** pi stores `{readFiles, modifiedFiles}`
  in `details` and re-harvests them from nested summaries on a later
  summarization; our entry type has no `details`, so cumulative file tracking
  across successive summaries is not reproduced. The current span's files still
  land in the summary text.
- **Token budget / `maxTokens`** (pi: `contextWindow − reserveTokens`, newest-
  first, `maxTokens: 2048`) is not implemented; `retry_summarization` handles
  retry/Es-cancel for the whole request.
- **UX**: when the selected entry is a *user message*, pi sets the new leaf to
  that message's **parent** and pre-fills the **input buffer** with its text
  (re-edit semantics). prior splits this off as its own row —
  `/tree <id> edit` — because "continue after this prompt" (`switch cursor`,
  leaf = the entry) and "replace this prompt" (`edit`, leaf = its parent) are
  both wanted; see below.
- **Scripted input**: `Repl::batch_slash` stops at a navigating Enter, so
  `/tree <id> switch` alone drills and stops; scripted callers use
  `/tree <id> switch cursor` (same as `/tree <id> compact` today).

## Staleness guard

`CompactStash` is keyed by `(epoch, action, target_id, source_leaf_id)`. The
source leaf is read from the **session** (`session_leaf_id`), not
`AppState::leaf_id()` — that is the menu's cached view, refreshed only at the
loop's convergence step. `apply_branch_summary` re-checks it and refuses rather
than attaching a digest that describes a different span.

## Sibling row: `/tree <id> edit`

pi's user-message re-edit (`core/agent-session.ts:2950`) is implemented as its
own leaf node rather than as a mode of `switch`:

```text
/tree <id> edit    → move the leaf to the prompt's parent, pre-fill its text
```

- `agent::reask_from_entry` — `move_leaf(entry.parent_id)` (`None` for a root
  prompt) + `AppState::set_prefill(text)`, the one-shot register the loop pops
  before the next `read_input`. Text extraction is pi's
  `_extractUserMessageText` (`session::user_prompt_text`): a plain string, else
  the `text` items concatenated; images/tool blocks excluded.
- `agent::is_reaskable_entry` gates the row: only a user `Message` or a
  `CustomMessage` gets it, read from the session so the row cannot go stale
  against the listing. Anything else that submits anyway errors instead of
  silently moving the cursor.
- The prefill replaces the buffer outright; pi only fills an empty one
  (`interactive-mode.ts` guards on `!editor.getText().trim()`). Unconditional is
  this repo's register contract for handler writes (`Repl::apply_prefill`).

## Replay

`ReplayUnit::BranchSummary { from_id, summary }` — the digest renders in
`/replay` and in the resume prefill (both go through `replay_units`) as a dim
system line followed by the markdown body, exactly like a compaction marker:

```text
[branch summary from u1]
## Goal
…
```

The id is opaque but copy-pasteable — `/tree <id>` switches right there. A
blank digest emits no marker at all (pi's context mapping skips it too, and
`apply_branch_summary` refuses to write one).

## Not done

- Nothing outstanding for the feature itself; see **Deliberate divergences**
  above for the ones that are deliberate.
