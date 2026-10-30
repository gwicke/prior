# Driving `AgentRun` directly — implementation notes

Companion to [`261030_rig_direct_drive.md`](261030_rig_direct_drive.md), which states the design.
This file holds the mechanics: the part type, the recognition rule, the render pass, the per-tool
work, and what gets deleted.

## rig surface in use

Verified against `rig-agent` 0.45.0. This is the whole dependency surface.

- `AgentRun` — `next_step`, `model_response`, `streamed_turn`, `record_streamed_completion_call`,
  `resolve_streamed_invalid_tool_call`, `ignore_streamed_invalid_tool_call`,
  `pending_invalid_tool_call`, `malformed_context`, `answer`, `answer_all`, `advertise_tools`,
  `append_entry`, `entries_of`, `retry_model_turn`, `accepted_turn_choice`, `messages`,
  `projection_start`, `usage`, `canonical_history`, `is_done`, `response`.
  `Serialize + Deserialize`, so a run survives an interruption, a process restart or a crash.
- `Aside` / `Author` — host records carried by the run, serialized with it and
  **excluded from provider requests**. Exactly the slot a mid-turn steer needs (design §5).
- `StreamedTurnAssembler` / `StreamedTurnEvent` / `StreamedTurn` / `StreamedInvalidToolCall` —
  `run/streamed.rs`, all public. `StreamedTurn`'s fields are public and it has no constructor, which
  is the hook the projection uses to render a turn it owns (design §4).
- `prepare_request(spec, capabilities, history, tools, committed_output_tool, patch)` →
  `PreparedRequest` → `apply(CompletionRequest)`. Pure, public, and already the single place request
  bytes are decided.
- `RunSpec`, `RequestPatch`, `TurnPolicy`, `InvalidToolCallAction`, `UnhandledInvalidToolCall`,
  `AgentRunStep`, `PendingToolCall`.
- `ModelHandle::{stream, call, capabilities, label}`; `Handlers::dispatch`, `dispatch_stream`,
  `bind`, `handle`.
- `ToolServerHandle::{snapshot, snapshot_with_dynamic}`, `ToolCatalog::get`,
  `RegisteredTool::execute(args, &mut ToolContext) -> ToolResult` — tool execution without the
  engine. A `ToolResult` carries the tool's `ToolOutput`, which is where a parts list arrives.
- `IntoToolOutput` — blanket over `T: Serialize`, so `Vec<CallPart>` needs no trait work; it lands
  as one `Json` block.
- `Agent::{run_spec, tool_server_handle, model_key, model_label, handlers, name}`.

Two `pub(crate)` gaps, both routed around rather than worked around:

- `HookContext::new` is `pub(crate)`, so a foreign driver cannot mint one. We drop rig's `AgentHook`
  entirely (design §2).
- `Handlers::serve`/`serve_stream` are `pub(crate)`. Tool dispatch goes through
  `RegisteredTool::execute`, which means no rig effect ids and no recording. prior uses neither.

## Mechanics

### The part

```rust
/// One call as the tool understood it, with its result.
struct CallPart {
    /// The call's arguments, rewritten by the tool (paths relativized, a
    /// `cd <cwd> &&` prefix stripped, an `edit` split per file).
    args: serde_json::Value,
    /// The result for exactly this call.
    result: String,
    /// Whether this part failed. A part may fail while the call succeeded
    /// (one of five files could not be written).
    is_error: bool,
}
```

A tool returns `Vec<CallPart>`. `IntoToolOutput` is blanket-implemented over `T: Serialize`, so the
vec arrives as one `Json` content block holding a JSON array, with no wrapper object and no
plumbing. `#[rig_tool]` derives `type Output` from the function's return type
(`rig-derive/expand.rs:328`), and `Guard<T>` forwards `type Output = T::Output`, so hand-written and
macro-generated tools declare it the same way.

The tool's return value is therefore its whole answer:
**the call as it understood it, plus its result** — once, or once per file. That single change is
what removes the rest of this section.

`project` replaces each call and result with `parts.len()` pairs:

- **ids are minted by the framework**, not carried in parts: `split_id(orig, i, n)` per index, so
  `i == 0` keeps the id the provider issued (some providers require it answered within its turn).
  That rule stays in one place rather than becoming a convention each tool can get wrong.
- **`is_error` is per part**, so a partial batch failure rides a successful call. rig commits the
  result with the call's own disposition in `is_error`
  (`rig_core::tool::feedback::tool_result_output`); the projection replaces it with the part's,
  which is why a *successful* `edit` whose third file failed still shows one failed result. Worth a
  comment at the discard site — it reads like a contradiction.
- **pairing is structural.** `paired_results`' length guard and its re-parse of joined result text
  both go; a dangling half is unrepresentable. `n == 1` collapses to the single call, so the common
  case takes the same path.
- **the stored form is the part's own text.** Persist writes `part.result` as plain text, exactly
  today's shape, so session files are unchanged and restore needs no recognition logic: there is
  never a parts list on disk, only below the run.

### Recognising parts without a marker

No wrapper means no reserved key, so the check is structural: a `Json` array whose every element is
`{args: object, result: string, is_error?: bool}`. That is safe because of where it runs, not
because the shape is unlikely:

- `project` only examines results **at or above `prior_history.len()`** — the messages this drive
  loop appended. Everything below that is loaded history, and loaded history is already unpacked.
- text can never collide: a tool returning `String` yields a text block, never `Json`. `bash`
  returns `String`, so shell output stays text even when it looks like JSON.
- the live tool set is six (`read`, `write`, `edit`, `grep`, `sed`, `bash`), all prior-owned, and
  none returns a parts-shaped array for any other reason.

If a future tool ever wants to return a genuinely part-shaped array as *data*, that is the moment to
reintroduce a key — not before. One place decides, so it stays a one-line change.

**The invariant to defend:** no `Json` array in a request history and none in a stored session. Both
egresses go through `project`, and that single assertion covers the "parts must never reach a
provider" case for every tool at once.

### Three passes, three jobs

Pass 1 runs at both egresses; pass 2 only when building a request:

1. **`project`** — unpack parts, splice anchored steers. Structural only: no tool names, no `bsed`
   knowledge, no path rules. `Message`-level, pure.
2. **model-facing render** — reasoning strip, then `edit` array → the `blocks` form the model is
   asked for. Stays in the completion-call callback beside where it runs today, because that
   pipeline builds a *request* rather than a *record*. Persistence takes the array form (pi's
   canonical); the wire takes blocks.
3. **steer/override assembly** — the `RequestPatch`, design §5.

Splitting the passes this way means `project` never learns that `edit` is special, and pass 2 never
learns that splitting happened — it sees N single-file arrays either way.

### Tool-specific shaping lives in the tool

Everything about how a call is spelled belongs to the tool that understands it, because that is the
only frame holding the facts:

- **parsing** — `EditArgs::from_wire` already runs at dispatch (`EditArgs` implements
  `Deserialize`), so `EditFile::call` receives `FileEdit`s. It cannot move earlier, and does not
  need to move later.
- **splitting** — the tool knows how many files it was given; it returns one part per file.
- **minimisation** — the compact anchored form needs the line each edit *landed* on
  (`matched_lines`), which only execution knows. So the tool returns already-minimised arrays.
- **path relativisation** — the tool relativizes the args it returns, using the same one rule and
  `PathContext` it already uses for its result paths (`src/tools.rs` `rel_path`). For `edit` this
  happens *after* splitting per file, which is exactly why `tool_args::normalize` had to exclude
  `edit` before; with per-file parts the exclusion is unnecessary.

That last point deletes `crate::tool_args::normalize` rather than porting it. Its twelve-name
matcher has eight registry-dead names (`ls`, `find`, `remove`, `make_dir`, `move_file`, `copy_file`,
plus the `list_dir`/`glob_files` spellings), kept only for replay — and replay does not need
rewriting, since what is stored is already the compact form. Of the live tools, `read`, `write`,
`grep` and `sed` relativize `path`; `edit` relativizes each file after splitting; `bash` strips the
prefix. Each now does so in its own `call`, on the args it returns.

The `cd <cwd> &&` strip stays in `Bash` for the additional reason that the shell must run what the
model meant, which is a different question from how the call reads.

**The completeness invariant, which must be tested:** a tool that returns no parts has its call
recorded verbatim. So every live tool taking a path must return parts. Turn that from a review
question into a test: walk the registry, assert each tool either returns parts or is in a deliberate
exempt list. Without it, a new path-taking tool silently stores absolute paths and
`doc/261026_tool_path_relativization.md`'s problem returns quietly.

### What this deletes

`EditResolutions` (nothing reads it once the tool formats its own results), `EditLedger` (three
readers no longer exist), `PreparedEdit` and its prepared-form cache, `prepare_edit_call`,
`paired_results`, `shape_edits`, `assistant_from_results`'s edit case, `tool_args::normalize` and
`normalize_messages`, the derived-id agreement between egresses, and the shared mutex.

**Idempotence now holds by construction**, because there is nothing to re-derive:

- a loaded call has no parts list (it was never stored), so unpack is a no-op — the normal answer;
- a loaded `edit` is already per-file, already minimised, already relative;
- array → blocks is not `project`'s job, so `project` cannot fight it;
- steer splice is keyed on aside entries already consumed.

This is stronger than the old three-reader arrangement: with the ledger gone there is no cache that
could be stale relative to the stored form — the stored form *is* the projected form.

## Notes for the implementer

- **Order inside phase 5.** `CallPart` and the unpack land first, with all six tools still returning
  text: unpack is then a no-op and nothing changes. Convert `read`/`write`/`grep`/`sed` next (one
  `relativize_arg` call each on the args they return), then `bash`, then `edit` last — `edit` is the
  only one whose conversion removes other code, so doing it last keeps the tree green throughout.
  `tool_args::normalize` is deleted only when the last tool stops relying on it.
- **`sed` already takes a path** and `normalize` covers it; confirm it is in the converted set, or
  its calls start storing absolute paths the moment `normalize` goes.
- **The parity harness (phase 1) must include an `edit` case** with a multi-file call, a single-file
  call, and a mixed-success batch. Those three are the shapes whose pairing and `is_error` handling
  change; the SSE mocks already cover retry and cancel.
- **`--log-messages`** records the request history, so it is the cheapest assertion for the
  no-parts-list invariant: capture a turn that ran `edit` and assert no `Json` array is present.
- **`edit` stays hand-written.** It is `EditFile` in `src/tools.rs` rather than a `#[rig_tool]`
  function because it owns its description and the `ToolContext` slot; its `call` is where parse →
  split → minimise → relativize happens, and it returns `Vec<CallPart>` like the rest. Nothing about
  the macro path needs changing.
