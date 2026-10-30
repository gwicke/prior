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
- **Tool-call shape.** `edit` accepts one call covering N files and prior wants N stored pairs. With
  no single place that produces the model's view of history, the split is re-applied at every egress
  (`shape_edits` on the conversation, the same split again in the request hook, derived `split_id`s
  agreeing across both, and the shared `EditLedger` that exists so three readers agree on one
  prepared form).
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
  `append_entry`, `entries_of`, `retry_model_turn`, `accepted_turn_choice`, `messages`,
  `projection_start`, `usage`, `canonical_history`, `is_done`, `response`.
  `Serialize + Deserialize`, so a run survives an interruption, a process restart or a crash.
- `Aside` / `Author` — host records carried by the run, serialized with it and
  **excluded from provider requests**. Exactly the slot a mid-turn steer needs (§5).
- `StreamedTurnAssembler` / `StreamedTurnEvent` / `StreamedTurn` / `StreamedInvalidToolCall` —
  `run/streamed.rs`, all public. `StreamedTurn`'s fields are public and it has no constructor, which
  is the hook the projection uses to render a turn it owns (§4).
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

The premise throughout: **the model sees what we tell it, not what it sent.** Its memory is the
context we build. Nothing in rig requires the request history and the recorded history to be the
same bytes; `prepare_request` takes whatever history the driver hands it, and `AgentRun`'s log only
has to be internally pair-valid. Every awkward reshape in today's code comes from conflating the
two.

### 1. One module owns the loop

`src/agent/drive.rs` replaces `agent.prompt(..).stream()`:

```text
run = AgentRun::from_spec(spec, Message::user(input), Some(prior_history))
loop {
    persist_delta(&run)                       // §3
    match run.next_step()? {
        CallModel { prompt, history, turn } => {
            let at_call = observer.before_call(&prompt, turn);     // §2: cancel, drain steers
            let wire = project(&history, &resolutions, &at_call);   // §4, the only shaper
            let prepared = prepare_request(spec, caps, &wire, tools, .., at_call.patch.as_ref())?;
            let mut stream = model.stream(prepared.apply(CompletionRequest::new(prompt)));
            // per-item: paint, feed the assembler, resolve invalid calls
            let turn = assembler.finish(&response);
            run.record_streamed_completion_call(..)?;
            run.streamed_turn(turn)?;          // the model's own call shapes, unmodified
        }
        CallTools { calls } => {
            for call in calls { /* sequential; observer paints; cancel check */ }
            run.answer_all(answers)?;
            // `EditResolutions` the executor published are what the
            // projection minimises this batch's calls with (§4)
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
- `before_call` — cancel check, drain the steering queue, build the `RequestPatch` for the pending
  request (was `on_completion_call`);
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

The persisted slice is `project(&run.messages()[cursor..])`, not the raw slice — see §4. The cursor
indexes raw messages; the projection changes shape and count, and is stable once the batch that
produced the resolutions is answered, which is the only state a persist boundary can observe.

What does *not* change: session storage, `repair_pairing` on load, the pi JSONL format, entry ids
and batch seeds. Those live behind `Session::append_messages` and stay.

### 4. One projection, no ingest phase

The run stores *what the model sent and what the tools returned*. Every reshaping — split, minimise,
relativize, reasoning strip, steer insertion — is **one function**, applied at the two egresses that
need it: persist (§3) and the request built in §2.

This is possible because rig never sends anything itself. The driver hands `prepare_request` the
history it chooses, which is what `render_model_facing` already does inside `on_completion_call`
today. The run's history is the record; the projection is the wire.

What rig actually constrains (verified in `run/mod.rs`):

- [`ToolAnswer`] is unforgeable (private fields, plus `compile_fail` tests in `calls.rs`) and
  `answer_all` validates each answer's `(turn, index, call id)` against the pending batch. A driver
  cannot hand the run a finished call+result exchange; there is no public append.
- `streamed_turn` rejects a call only when its **name** is not allowed by the turn's `TurnPolicy`.
  Count and arguments are not inspected.

So the assistant turn is frozen at `streamed_turn`, and shape changes are free either side of it —
but nothing needing execution output can be part of the accepted turn. Which means:

- **the run keeps the model's own single multi-file `edit` call.** `next_step` yields one
  `ExecCall`; `edit_files_with` already executes atomically per file and returns one array result.
  No N-call batching in the loop, no re-parse of a joined result text, no derived-id agreement
  because there is only ever one id;
- the split to pi's one-call-per-file shape happens in the projection, on the assistant message and
  the results message together — the pairing hazard is the existing one, one function, one guard (a
  multi-file call whose result is not a per-file array stays whole);
- minimisation happens in the same projection, reading a `CallId -> EditResolutions` map the
  executor fills. Because persist projects too, the stored form is the minimised one: a reloaded
  session shows the model the same compact calls it saw live, and `prepare_edit_call`'s no-ledger
  fallback covers exactly that reload path.
- the only irreducible pre-accept work is the invalid-call path (name repair, ignored calls,
  malformed arguments), where rig derives the batch from `choice` and asks the driver mid-stream.

Deleted by this: `shape_edits` as a separate conversation rewrite, `PreparedEdit`'s prepared-form
cache (the projection is the one producer, so it cannot drift from itself), the argument
normalisation at three egresses, and the `EditLedger` mutex — a per-run map keyed by call id, filled
by the executor, read by the projection, is all the state that is left.

**The invariant that makes it safe:** the projection must be *idempotent*, because history loaded
from a session is already projected output being fed back in as `from_spec`'s history. Each step
satisfies this for a specific reason, and having one function is what makes it checkable in one
place:

- split — a stored per-file call is `{path, edits:[…]}`, which `EditArgs::from_wire` parses as *one*
  file, so it does not split again;
- minimise — an anchored edit's own anchor is its base, and with no ledger entry (a reloaded
  session) the step is a no-op;
- relativize — a no-op on already-relative paths;
- reasoning strip, steer splice — keyed on content, so a second pass finds nothing to change.

This is the property `render_model_facing` documents today and that `shape_edits` relies on the
ledger to uphold across three readers. One producer, one proof.

**Tool-handling nicety this enables, not required by it:** with the projection owning the call
shape, a tool that wants to reshape its own calls can publish that through its result context (as
`edit` already does with `EditResolutions`) instead of prior special-casing its name. Whether a tool
could go further and return whole call/result pairs is settled above: rig's pairing is unforgeable,
so a tool may only describe a *projection* of its call, never mint an exchange.

### 5. Retry becomes resume, steering becomes projection content

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

Mid-turn steering is the *same* projection as §4, which is why it needs no mechanism of its own:

- a `SteeringQueue` on `CodeAgent` collects queued `Message::user` steers from the UI thread;
- the run already has the right slot for one: `AgentRun::append_entry(Aside)` takes a host record
  with `kind`, `author`, a verbatim JSON `value`, and an `after: Option<MessageId>` anchor. Asides
  are serialized with the run and **excluded from provider requests**, so recording a steer as
  `kind: "prior.steer"`, `author: Author::user()`, `value: <the Message>`,
  `after: <the last message id>` costs nothing until we choose to send it, and survives a restart.
  The projection reads `run.entries_of("prior.steer")` and splices them in, so persist and the wire
  show the steer at the same position, once;
- that closes the `RequestPatch::history` caveat in `doc/pi_steering_rig.md` §3.1 (a per-request
  patch is forgotten by the next turn) *without* a `history` patch: the steer enters the session
  through persist and comes back through the next turn's history, so nothing has to remember it a
  second time;
- placement rule: after the last tool result of the batch, never between an assistant turn and its
  results. A steer arriving mid-batch is therefore delivered with the next request, which is the
  only point where history is a valid provider prefix anyway;
- setting overrides (`temperature`, `max_tokens`, `additional_params`/reasoning) are not history at
  all; they ride `RequestPatch` for the one request and, if the user meant them to stick, are
  committed to the live settings the next `prompt()` builds from. Two lifetimes, stated
  deliberately: a steer is a message, a setting is not.

One position, one function: the transcript and the wire cannot disagree about where the user spoke.

### 6. Module plan

- `src/agent/drive.rs` — the loop above.
- `src/agent/observe.rs` — `TurnObserver` (display, `--log-messages`, cancel, steering drain).
  Absorbs today's `LoggingHook` and most of `turn.rs`.
- `src/agent/project.rs` — **the** projection: split, minimise, relativize, strip reasoning, splice
  steer asides. Pure, over `(&[Message], &Resolutions, &[Aside])`, called by persist and by request
  building and by nothing else.
- `src/agent/persist.rs` — cursor over raw messages, projecting each delta.
- `src/agent/transcript.rs` — shrinks to the `CallId -> EditResolutions` map, the edit-arg shape
  helpers, and load-path repair.
- `src/agent/turn.rs` — `prompt` becomes a thin wrapper over `drive`; `run_once` and the summarizers
  keep their signatures.

### 7. Phases

1. **Parity harness.** Point the existing `tests/mock_turn.rs`, `retry_backoff.rs` and
   `compaction_esc.rs` SSE mocks at a second, drive-based path behind a `PRIOR_DRIVE=direct` switch,
   and make every scenario pass on both. No behaviour change tolerated; this is the gate for the
   rest.
2. **Drive loop, single attempt.** Completion → assemble → accept → tools → done, with cursor
   persistence live and the projection the only shaper. `transcript.rs`'s live-unit machinery
   deleted at the end of this phase.
3. **Retry by resume.** Back-off loop around the attempt, deleting `LlmInterrupted`, `"continue"`,
   `TurnPersistence`. `doc/tool_error_handling.md` §"Mid-turn failure" is retired with it.
4. **Steering and boundary settings.** `SteeringQueue`, the projection's steer insertion and scalar
   overrides, the mid-turn UI entry points and `/reasoning`-style mid-turn commits.
5. **Projection takes over `edit`.** Removal of `shape_edits`, `PreparedEdit`'s cache and the
   `EditLedger` mutex; the split and minimisation move into the projection. (Phase 2 can carry the
   old path in the projection temporarily; this phase deletes it.)
6. **Cleanup.** Delete `HookContext` usage, the hook structs' rig impls, and update
   `doc/261026_rig_steering.md` — its "Prior's Current Architecture Is Correct" section becomes the
   opposite of the truth.

Phases 1–3 are the payoff for the retry problem. 4 is the payoff for steering. 5 is the payoff for
tool handling. Any of them can stop the project without leaving the tree worse, except that 2
commits us to maintaining the loop.

## Open questions

*Settled (was open):*

- **Minimisation stays.** It is a token budget and a teaching device: the model sees the compact
  anchored form of every call it has already made. Because persist projects, the stored form and the
  wire form are the same by construction, so this needs no eval and no second reader.
- **The `edit` API does not change.** Still `blocks`-based, still one call covering several files.
  The multi-file call is what the model emits and what the run records; only the stored/wire shape
  is per-file, and that is the projection's business.
- **A steer is recorded on the run, not in a side table.** `append_entry` exists for host records
  and excludes them from requests, so there is one durable place a steer lives and one function that
  decides when it becomes context.
- **A tool may not mint call/result pairs.** rig's `ToolAnswer` is unforgeable and `answer_all`
  validates identity, so an exchange can only come from rig's own `CallTools` step. A tool that
  wants its calls reshaped publishes that as result context (the `EditResolutions` pattern), which
  the projection reads.

*Open:*

1. **Version risk.** We now depend on `AgentRun`'s public API and step protocol rather than only on
   `agent.prompt`. A rig bump that changes the protocol is a real port, where today it is a rebase.
   Mitigation: pin exactly, and keep `drive.rs` the only module that names a rig step type.
2. **Unary surface.** The summarizers still use `agent.chat(..)`. Fine, and it stays: they want one
   call, not a loop.
3. **Retrieval.** `ToolServerHandle::retrieval_effects` is not ported. prior registers no retrieval
   index; if the tool set ever gains one, the dispatch belongs in `before_call` beside
   `prepare_request`.
4. **Tool-context projection hook.** Should the projection ask each tool how to render its own calls
   (`tool_facing_context`), or keep the `edit` special case with a generic fallback? The former is
   the honest end state; the latter is a smaller phase 5.
5. **Partial-batch persist.** A tool batch is one user message in the run, so a crash mid-batch
   persists nothing for that batch and the results are lost on restart. Today's
   `repair_tail_unanswered` writes "did not execute" placeholders instead. Keep that as a load-time
   repair, or persist answers as they land (which the projection can do, at the cost of writing a
   message the run has not yet committed)?
