# Driving rig's `AgentRun` directly

*Status: design. Supersedes the retry half of `doc/261026_rig_steering.md` and of
`doc/tool_error_handling.md` §"Mid-turn failure & resume".*

## Why

prior currently borrows rig's drive loop (`agent.prompt(..).stream()`, whose engine is `pub(crate)`)
and then reconstructs, outside it, everything the loop will not tell it:

- **Persistence.** rig commits canonical messages into `AgentRun`, but the run is consumed and
  dropped by `drive_agent`. prior therefore reassembles each completion turn from stream deltas plus
  hook callbacks (`TurnTranscript`/`LiveUnit`, `src/agent/transcript.rs`), proves the reassembly is
  a valid provider prefix (`answered_all`, `assemble`, `RunState::Ongoing|Ended`), and persists at
  the one boundary it can see — `on_completion_call`.
- **Retries.** A failure mid-turn has no run to resume, so a retry means a *new* `prompt(..)` with a
  synthetic user message. That is the `"continue"` injection, the `LlmInterrupted` variant, and
  `TurnPersistence`'s dedup bookkeeping.
- **Tool-call shape.** `edit` accepts one call covering N files and prior wants N stored pairs, so
  the split happens *after* execution at every egress (`shape_edits`, derived `split_id`s, the
  shared `EditLedger` that exists so three readers agree on one prepared form).
- **Steering.** The only mid-turn injection point is a hook, whose `RequestPatch` is per-turn and
  forgotten by the next turn.

None of this is rig's fault; rig simply offers no seam between "you own the state machine" and "you
own nothing". `AgentRun` is public and complete. This document takes it at its word: prior keeps the
run and drives it.

## What rig already exposes (no fork needed)

Verified against `rig-agent` 0.45.0. This is the whole dependency surface.

- `AgentRun` — `next_step`, `model_response`, `streamed_turn`, `record_streamed_completion_call`,
  `resolve_streamed_invalid_tool_call`, `ignore_streamed_invalid_tool_call`,
  `pending_invalid_tool_call`, `malformed_context`, `answer`, `answer_all`, `advertise_tools`,
  `retry_model_turn`, `accepted_turn_choice`, `messages`, `projection_start`, `usage`,
  `canonical_history`, `is_done`, `response`. `Serialize + Deserialize`, so a run survives an
  interruption, a process restart or a crash.
- `StreamedTurnAssembler` / `StreamedTurnEvent` / `StreamedTurn` / `StreamedInvalidToolCall` —
  `run/streamed.rs`, all public. `StreamedTurn`'s fields are public and it has no constructor, which
  is the hook the ingest rewrite uses (§4).
- `prepare_request(spec, capabilities, history, tools, committed_output_tool, patch)` →
  `PreparedRequest` → `apply(CompletionRequest)`. Pure, public, and already the single place request
  bytes are decided.
- `RunSpec`, `RequestPatch`, `TurnPolicy`, `InvalidToolCallAction`, `UnhandledInvalidToolCall`,
  `AgentRunStep`, `PendingToolCall`.
- `ModelHandle::{stream, call, capabilities, label}`; `Handlers::dispatch`, `dispatch_stream`,
  `bind`, `handle`.
- `ToolServerHandle::{snapshot, snapshot_with_dynamic}`, `ToolCatalog::get`,
  `RegisteredTool::execute(args, &mut ToolContext) -> ToolResult` — tool execution without the
  engine, including the published-context slot prior's `edit` relies on.
- `Agent::{run_spec, tool_server_handle, model_key, model_label, handlers, name}`.

Two gaps worth naming, both harmless:

- `HookContext::new` is `pub(crate)`, so a foreign driver cannot mint one. We drop rig's `AgentHook`
  entirely (see §2) rather than work around it.
- `Handlers::serve`/`serve_stream` are `pub(crate)`. Tool dispatch goes through
  `RegisteredTool::execute`, which means no rig effect ids and no recording. prior uses neither.

## Design

### 1. One module owns the loop

`src/agent/drive.rs` replaces `agent.prompt(..).stream()`:

```text
run = AgentRun::from_spec(spec, Message::user(input), Some(prior_history))
loop {
    persist_delta(&run)                       // §3
    match run.next_step()? {
        CallModel { prompt, history, turn } => {
            let patch = observer.before_call(&prompt, &history, turn);   // §5
            let prepared = prepare_request(spec, caps, &history, tools, .., patch)?;
            let mut stream = model.stream(prepared.apply(CompletionRequest::new(prompt)));
            // per-item: paint, feed the assembler, resolve invalid calls
            let assembled = assembler.finish(&response);
            let assembled = split_at_ingest(assembled)?;                 // §4
            run.record_streamed_completion_call(..)?;
            run.streamed_turn(assembled)?;
        }
        CallTools { calls } => {
            for call in calls { /* sequential; observer paints; cancel check */ }
            run.answer_all(answers)?;
        }
        Done(response) => break Ok(response),
    }
}
```

`AgentRun::next_step` already is the state machine, the budget, the invalid-call policy and the
output-tool policy. What we add is the seam between its steps.

Concurrency: prior runs `.tool_concurrency` at its default of 1, so the batch is a sequential loop
with a fail-fast check between calls. rig's concurrent batch machinery (`buffer_unordered`,
terminating flags, lowest-index-error arbitration) is not ported.

### 2. rig hooks become plain callbacks

`LoggingHook` is not really a hook: it paints, persists and logs, and its `on_completion_call`
doubles as the persistence boundary. With the loop in hand it becomes `TurnObserver` with explicit
call sites:

- `after_turn` — paint/record the accepted turn, log `--log-messages` "received" (was
  `on_model_turn_finished`);
- `before_call` — cancel check + build the `RequestPatch` for the pending request (was
  `on_completion_call`);
- `dispatch` / `outcome` — paint a tool call and its result around `RegisteredTool::execute` (was
  `on_dispatch` / `on_outcome`);
- `invalid_tool_call` — the recovery decision (unchanged logic, called directly).

Cancellation gets simpler: one `select!` on the completion stream and `cancel_notify`, plus a flag
check between tool calls. The three hook-side `user_cancel` checks disappear.

### 3. Persistence becomes a cursor

The canonical history is `run.messages()`, append-only, provider-exact, with every tool batch
committed as one user message and every accepted turn already in it. So:

- keep `cursor` (seeded from `run.projection_start()`);
- persist `run.messages()[cursor..]` whenever the committed tail is a complete prefix — no assistant
  turn with unanswered calls;
- advance `cursor`.

That is the whole of persistence. `TurnTranscript`, `LiveUnit`, `LivePart`, `prepare`, `assemble`,
`RunState::Ongoing|Ended`, `assistant_from_results` and the "did the reassembly survive" question
all go away. The two boundaries it covers are exactly today's: start of a completion call, and after
a tool batch.

What does *not* change: session storage, `repair_pairing` on load, the pi JSONL format, entry ids
and batch seeds. Those live behind `Session::append_messages` and stay.

### 4. Split `edit` batches at ingest

The split moves from *after* execution to *before* it:

1. the assembler yields `StreamedTurn { head, choice, .. }`;
2. for each `edit` call, parse its `blocks` payload into N `FileEdit`s
   (`bsed_blockedit::EditArgs::from_wire`);
3. mint N calls (`split_id`) carrying the canonical per-file array, one per file;
4. feed the rewritten `StreamedTurn` to `run.streamed_turn`.

The following `next_step()` then emits N `ExecCall`s for the same turn; each executes one file;
results pair one-to-one with no re-parsing of a joined result text. This is compatible with the
failure semantics because execution is already atomic per file, and a failed file is just that
call's error result.

Deleted by this: `shape_edits`, `paired_results`, `assistant_from_results`'s edit special case, and
the derived-id agreement between two later egresses (the id is now minted once, at the only place
that knows how many files there are).

**Not** solved by it: minimisation. The compact form of an `edit` call needs the line anchors
execution resolved, so the call cannot be finalised before it runs. Today the request-history render
reads `EditLedger` to show the minimised form while the stored call keeps the model's own. After
this change the same ledger-backed render can remain as a pure `Message -> Message` pass at
history-build time — one reader instead of three — or we drop retroactive minimisation and let the
diff in the *result* carry the information. Open question (§7).

### 5. Retry becomes resume, steering becomes a patch

One `AgentRun` per user prompt. A failed completion is an **attempt**, not a dispatch:

- the attempt's turn was never fed to `run.streamed_turn`/`model_response`, so the run is untouched
  — nothing to roll back, nothing to deduplicate;
- the partial text is display-only; the observer paints it, and the retry repaints;
- back-off and budget stay prior-side (`retry_budget`, `retry_delay_ms`) — rig budgets turns, not
  attempts, and a re-issued `next_step` consumes none;
- the run is in hand, so an interruption needs no `PromptError` variant carrying it.

This removes `LlmInterrupted`, the `"continue"` prompt, `steer_delivered`, `next_input` and
`TurnPersistence` outright.

Corrective feedback (malformed tool-call JSON, a 429 hint, a truncation retry) is delivered as
`RequestPatch::extra_context` documents rather than as prompt text: it steers the next request
without entering the transcript.

Mid-turn steering and settings become the same mechanism, which is the point:

- a `SteeringQueue` on `CodeAgent` collects queued `Message::user` steers and pending overrides
  (`temperature`, `max_tokens`, `additional_params`/reasoning) from the UI thread;
- `observer.before_call` drains it into the `RequestPatch` for that request (`history` with steers
  appended after the last tool result, plus the scalar overrides), and records the steers into the
  run so they persist exactly once;
- settings-only changes need no transcript write at all — they are per-request by construction.

The `RequestPatch::history` caveat in `doc/pi_steering_rig.md` (a patch is per-turn and the
canonical log never learns it) is handled by writing the steer into `run` at the same time, so the
next turn reads it back.

### 6. Module plan

- `src/agent/drive.rs` — the loop above.
- `src/agent/observe.rs` — `TurnObserver` (display, `--log-messages`, cancel, patch construction).
  Absorbs today's `LoggingHook` and most of `turn.rs`.
- `src/agent/ingest.rs` — accepted-turn rewrite: `edit` splitting, tool-name repair bookkeeping,
  argument normalisation **once** at ingest instead of at three egresses.
- `src/agent/persist.rs` — cursor sync.
- `src/agent/transcript.rs` — shrinks to the edit-arg shape helpers, the model-facing render, and
  load-path repair.
- `src/agent/turn.rs` — `prompt` becomes a thin wrapper over `drive`; `run_once` and the summarizers
  keep their signatures.

### 7. Phases

1. **Parity harness.** Point the existing `tests/mock_turn.rs`, `retry_backoff.rs` and
   `compaction_esc.rs` SSE mocks at a second, drive-based path behind a `PRIOR_DRIVE=direct` switch,
   and make every scenario pass on both. No behaviour change tolerated; this is the gate for the
   rest.
2. **Drive loop, single attempt.** Completion → assemble → accept → tools → done, with cursor
   persistence live. `transcript.rs`'s live-unit machinery deleted at the end of this phase.
3. **Retry by resume.** Back-off loop around the attempt, deleting `LlmInterrupted`, `"continue"`,
   `TurnPersistence`. `doc/tool_error_handling.md` §"Mid-turn failure" is retired with it.
4. **Steering and boundary settings.** `SteeringQueue`, `before_call` patches, the mid-turn UI entry
   points and `/reasoning`-style mid-turn commits.
5. **Split at ingest.** `edit` rewrite, removal of `shape_edits` and the split half of `EditLedger`.
   Decide the minimisation question at the start of this phase, not during it.
6. **Cleanup.** Delete `HookContext` usage, the hook structs' rig impls, and update
   `doc/261026_rig_steering.md` — its "Prior's Current Architecture Is Correct" section becomes the
   opposite of the truth.

Phases 1–3 are the payoff for the retry problem. 4 is the payoff for steering. 5 is the payoff for
tool handling. Any of them can stop the project without leaving the tree worse, except that 2
commits us to maintaining the loop.

## Open questions

1. **Minimisation of `edit` calls.** Keep the ledger-backed history render (one reader), or move the
   accepted form into the result and stop rewriting history? The second is cleaner but changes what
   the model sees, so it needs an eval.
2. **`edit` single-file shape.** With split-at-ingest, should `edit` stay a `blocks`-batch tool that
   happens to be split, or become one-path-per-call? The former keeps prompt cost and round trips;
   the latter removes the last reason for ingest rewriting other than minimisation.
3. **Version risk.** We now depend on `AgentRun`'s public API and step protocol rather than only on
   `agent.prompt`. A rig bump that changes the protocol is a real port, where today it is a rebase.
   Mitigation: pin exactly, and keep `drive.rs` the only module that names a rig step type.
4. **Unary surface.** The summarizers still use `agent.chat(..)`. Fine, and it stays: they want one
   call, not a loop.
5. **Retrieval.** `ToolServerHandle::retrieval_effects` is not ported. prior registers no retrieval
   index; if the tool set ever gains one, the dispatch belongs in `before_call` beside
   `prepare_request`.
