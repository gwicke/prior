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

Two shapes, each stated at the moment it is knowable.

```rust
/// What the call is, as the tool reads it: paths relativized, a `cd <cwd> &&`
/// prefix stripped. Pure and idempotent. Knowable before execution, which is
/// what lets the display label a slow call with the same args the record will
/// carry.
fn preprocess_args(raw: &serde_json::Value) -> serde_json::Value;

/// One call as the tool understood it, with its result.
struct CallPart {
    /// `preprocess_args` of the model's arguments (an `edit` part: one file's,
    /// already split, minimised and relativized).
    args: serde_json::Value,
    /// The result for exactly this call.
    result: String,
    /// Whether this part failed. A part may fail while the call succeeded
    /// (one of five files could not be written).
    is_error: bool,
}
```

**`preprocess_args` is registered beside the tool, not on it**: `#[rig_tool]` generates the struct,
so a tool cannot implement a prior-side trait, and `Guard<Bash>` needs the same forwarding
`type Output` already gets. A name-keyed table in `build()` holds it, one entry per *live* tool —
unlike the deleted `tool_args::normalize`, whose matcher carried eight registry-dead names for
replay. Replay needs no preprocessing: what is stored is already this function's output.

It is called **once per live call, ever** — at dispatch, and the value is carried forward into the
part. `two places` is the wrong mental model and the source of a stale duplicate:

```rust
for call in calls {
    let understood = shape.preprocess(call.name(), call.arguments());  // once
    observer.dispatch(call.name(), &understood);                       // panel label
    let out = tool.execute(call).await;
    // The five single-result tools: the framework builds their one part here,
    // from the value computed above, so the tool itself says nothing new.
    let parts = match out.output().as_json() {
        Some(parts) => parts.clone(),          // `edit` already returned parts
        None => json!([CallPart {
            args: understood.clone(),
            result: out.output().render(),
            is_error: !out.is_success(),
        }]),
    };
    // The run records the parts, so `project` unpacks rather than re-derives.
    persist_answer(&call, &parts);
    run.answer(call.answer(out.with_output(ToolOutput::json(parts))))?;
}
```

`ToolResult::with_output(output)` replaces only the model-visible content and
**keeps the disposition**, so the failure path injects exactly as the success path does — `is_error`
is read before the wrap and travels in the part. `answer_all` then commits that block verbatim
(`feedback::tool_result_output` clones `output().into_content()`), so the parts land in
`run.messages()`, which is the store `project` reads.

`is_error: !out.is_success()` is not an invention: it is rig's own rule for the `is_error` it would
have written (`tool_result_output`, `rig-core/src/tool/feedback.rs:24`), so a skipped or refused
call lands in the part exactly as it landed in the message before. Beyond the parts, the disposition
still drives `executed`, telemetry and the fail-closed checks — two audiences, one record.

Therefore **`read`/`write`/`grep`/`sed`/`bash` keep `Result<String, ToolError>` unchanged**: nothing
to repeat, because the tool no longer constructs the part at all. Dispatch is where the shape is
learned, and it is learned once:

- the panel label and the recorded args are the *same bytes*, so they cannot disagree about a call
  that never changed — previously the argument, now the construction;
- `project` re-derives nothing over the history, which was the dominant cost (N calls, every
  request, for the life of the session) and the reason idempotence had to be defended;
- there is one unpack path for every call above the boundary, instead of a derivable-args branch for
  five tools and a parts branch for `edit`;
- the `(CallId, message index)` memo floated earlier is no longer worth considering — there is
  nothing left to memoise.

`edit` is the only tool that returns parts itself, and the one absent from the table:

- `type Output = Vec<CallPart>`; `IntoToolOutput` is blanket over `T: Serialize`, so the vec arrives
  as one `Json` content block holding a JSON array — no wrapper, no plumbing. `#[rig_tool]` derives
  `type Output` from the return type (`rig-derive/expand.rs:328`) and `Guard<T>` forwards
  `type Output = T::Output`, so the macro and hand-written paths agree.
- It has nothing to publish at dispatch: its shape is not knowable before execution (minimisation
  needs where each edit landed), which is why its panel already defers to the pair
  (`TurnObserver::dispatch` returns early for it; `outcome` paints the whole pair). The `as_json`
  arm above takes its result untouched, so it cannot be double-wrapped.
- Its args are not passed through `preprocess_args` at all — the per-file args it returns *are* the
  processed form, which is what makes the exclusion structural rather than a no-op entry.

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
`{args: object, result: string, is_error?: bool}`.

Injecting every answer means parts now live *inside* the run, so the shape test is what keeps
machine payload out of the wire. It needs no boundary: persist writes `part.result` as plain text,
so nothing on disk is a parts list, and `project` runs over the whole history on every request
without a `prior_history.len()` window. (An earlier draft proposed that bound. It is unnecessary,
and the code does not have it.)

Two facts make the unbounded projection safe:

- a loaded result is plain text — a `bash` output that printed a JSON array of
  `{args, result, is_error}` stays text, because a text block is never read as parts;
- a live call's result is a `Json` block only because the loop made it one — the tool returns
  `String` (a text block) and the wrap is the loop's own.

A parts list must never reach a provider or a session file. Both egresses go through `project`, and
the test is one assertion each way: no `{args, result, is_error}` array in a request history, and
none in a stored session.

### Three passes, three jobs

Pass 1 runs at both egresses; pass 2 only when building a request:

1. **`project`** — unpack parts, splice anchored steers. Structural only: no tool names, no `bsed`
   knowledge, no path rules — it unpacks what the loop injected, and derives nothing.
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
- **path relativisation** — `preprocess_args`, using the same one rule and `PathContext` the tool
  already uses for its result paths (`src/tools.rs` `rel_path`). `edit` relativizes each file
  *after* splitting, which is exactly why `tool_args::normalize` had to exclude `edit` before;
  per-file parts make the exclusion unnecessary.

That last point deletes `crate::tool_args::normalize` rather than porting it. Its twelve-name
matcher has eight registry-dead names (`ls`, `find`, `remove`, `make_dir`, `move_file`, `copy_file`,
plus the `list_dir`/`glob_files` spellings), kept only for replay — and replay does not need
rewriting, since what is stored is already the compact form. Of the live tools, `read`, `write`,
`grep` and `sed` relativize `path` in `preprocess_args`; `bash` strips its prefix there; `edit`
relativizes per file in what it returns.

The `cd <cwd> &&` strip stays in `Bash` for the additional reason that the shell must run what the
model meant, which is a different question from how the call reads.

**The completeness invariant, which must be tested.** A tool with no `preprocess_args` entry has its
call recorded — and labelled — verbatim, so a new path-taking tool would silently store absolute
paths and `doc/261026_tool_path_relativization.md`'s problem would return quietly. Turn that from a
review question into a test, walking the registry:

- every live tool taking a path argument has a `preprocess_args` entry, or is on a deliberate exempt
  list (`edit`, which publishes its own per-file args);
- `preprocess_args` is idempotent on a sample call: `f(f(x)) == f(x)` — cheap insurance for the
  loaded-history path, which feeds the table output back through it.

### What this deletes

Landed with phase 5: `EditLedger` (three readers no longer exist), `PreparedEdit` and its
prepared-form cache, `prepare_edit_call`, `paired_results`, `shape_edits`, `tool_args::normalize`
and `normalize_messages`, the derived-id agreement between egresses, and the shared mutex.

Still to delete: **`EditResolutions`** (and `FileResolution` with it). It survives because
`edit_files_with` still publishes it through `ToolContext`, but the ledger that read it is gone: the
only remaining readers are two tests asserting what the tool published. Nothing in the loop, the
projection, the wire or the display consumes it — the parts already carry the minimised args.

**Nothing is re-derived, so nothing can drift:**

- a live call's parts are built once, by the loop, and carried into the run;
- a loaded call has no parts list (persist wrote plain text), so unpack is a no-op;
- a loaded `edit` is already per-file, already minimised, already relative;
- array → blocks is not `project`'s job, so `project` cannot fight it;
- steer splice is keyed on aside entries already consumed.

This is stronger than the old three-reader arrangement: with the ledger gone there is no cache that
could be stale relative to the stored form — the stored form *is* the projected form. And with the
parts injected at execution rather than re-derived at projection, the display, the record and the
wire are three readings of one value instead of three functions that had to be proven to agree.

## Notes for the implementer

**Where the code landed.** `src/agent/parts.rs` (the part, the predicate, `unpack`),
`src/agent/tool_shape.rs` (the `preprocess_args` table), `src/agent/project.rs` (the projection and
its complete-prefix rule), `src/agent/persist.rs` (the cursor and the held answers),
`src/agent/drive.rs` (the loop, which builds each part from the dispatch-computed args),
`src/agent/observer.rs` (paint and log only). Phase 5's ordering advice is spent: the conversion is
done, and `tool_args::normalize` is gone.

Still worth knowing:

- **The injection is inside the loop, so a tool cannot do it.** A tool whose `call` also rewrote its
  own arguments would create the second producer this design exists to remove — and it could not,
  because the wrap happens after `execute` returns.
- **`with_output` is the failure path too**, and it keeps the disposition. `is_error` is read before
  the wrap; after it, `result.is_success()` still describes the execution, which is what telemetry
  and the fail-closed checks read. Two audiences, one record.
- **Malformed `edit` text is shaped where the part is built**, not in a hook. `as_text` no longer
  sees it: the answer is a `Json` block, so the rewrite moved to the loop's part construction.
- **The projection runs over the whole history on every request.** It is a fixed point on already
  projected messages (tested in `project`), so there is no boundary to track — and no
  `(CallId, message index)` memo to build. `unpack` finds nothing in loaded history because
  persistence wrote the parts unpacked.
- **A parts list must never reach a provider or a session file.** `project` is the only thing that
  can put a message either place, and `project` never emits one — asserted directly in both
  directions there, which is cheaper and stronger than a `--log-messages` capture.
- **`edit` stays hand-written.** It is `EditFile` in `src/tools.rs` rather than a `#[rig_tool]`
  function because it owns its description and the `ToolContext` slot; its `call` is where parse →
  split → minimise → relativize happens, and it returns `Vec<CallPart>`. Nothing about the macro
  path needed changing — the `as_json` arm picks it up without being told which tool it is.
- **Closing is not writing early.** A batch's answers are held until the close, so nothing the store
  holds is a message the run has not produced. If a future change wants durable per-answer writes,
  that is a new decision with a duplicate to reconcile, not a tuning of this one.
