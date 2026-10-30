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
    persist_delta(&run)                       // §3, project then append
    match run.next_step()? {
        CallModel { prompt, history, turn } => {
            let at_call = observer.before_call(&prompt, turn);  // §2: cancel, drain steers
            let unpacked = project(&history, &run.entries());    // §4 pass 1: unpack, splice
            let wire = model_facing(unpacked, &at_call);         // §4 pass 2: strip, blocks
            let prepared = prepare_request(spec, caps, &wire, tools, .., at_call.patch.as_ref())?;
            let mut stream = model.stream(prepared.apply(CompletionRequest::new(prompt)));
            // per-item: paint, feed the assembler, resolve invalid calls
            let turn = assembler.finish(&response);
            run.record_streamed_completion_call(..)?;
            run.streamed_turn(turn)?;          // the model's own call shapes, unmodified
        }
        CallTools { calls } => {
            for call in calls { /* sequential; observer paints; cancel check */ }
            // an answer's output may be a `prior_call_envelope`; the run keeps
            // it verbatim, and pass 1 unpacks it at the next egress (§4)
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

The run stores *what the model sent and what the tools returned*. The projection (`project`) turns
that into provider and store shapes, and it makes exactly **one** decision per tool call: whether
the call was replaced by an envelope of parts. Everything else about a call it takes verbatim.

This is possible because rig never sends anything itself. The driver hands `prepare_request` the
history it chooses, which is what `render_model_facing` already does inside `on_completion_call`
today. The run's history is the record; the projection is the wire.

What rig actually constrains (verified in `run/mod.rs`):

- `ToolAnswer` is unforgeable (private fields, plus `compile_fail` tests in `calls.rs`) and
  `answer_all` validates each answer's `(turn, index, call id)` against the pending batch. A driver
  cannot hand the run a finished call+result exchange; there is no public append.
- `streamed_turn` rejects a call only when its **name** is not allowed by the turn's `TurnPolicy`.
  Count and arguments are not inspected.

So the assistant turn is frozen at `streamed_turn`, and shape changes are free either side of it —
but nothing needing execution output can be part of the accepted turn. That leaves exactly one
constraint on the design: a call's per-file shape must be decided *after* it runs.

#### The call envelope

A tool returns either what it returns today, or an envelope:

```json
{
  "prior_call_envelope": {
    "parts": [
      { "args": { "path": "a.rs", "edits": [ … ] }, "result": "…", "is_error": false },
      { "args": { "path": "b.rs", "edits": [ … ] }, "result": "…", "is_error": true }
    ]
  }
}
```

`IntoToolOutput` is blanket-implemented for `T: Serialize`, so a tool opts in by declaring
`type Output = EditBatch` (or by returning the JSON value directly) and nothing else changes. A tool
that returns text is untouched, and the reserved key is absent, so the unpack is a no-op.

The projection replaces the original call and result with `parts.len()` call/result pairs:

- **ids are minted by the framework**, not carried in parts: `split_id(orig, i, n)` per index, so
  `i == 0` keeps the id the provider issued (some providers require it answered within its turn).
  That rule stays in one place instead of becoming a convention each tool can get wrong.
- **`is_error` is per part**, so "one of five files failed" is expressible while the call itself
  succeeded. The execution's own disposition still drives telemetry and the fail-closed paths; the
  parts are the model's record. Two audiences, two records.
- **pairing is structural.** Today `paired_results` parses the joined result text and refuses the
  split unless `parsed.len() == n`; with parts carrying their own results, a dangling half is
  unrepresentable. `n == 1` collapses to the single call, so the common case is the same code path.

**The one invariant to defend:** an envelope must never reach a provider. Both egresses — persist
and request — go through `project`, and a test asserts the reserved key never appears in either.
Restore is safe by construction: the stored form is already unpacked, so there is nothing left to
recognise.

#### Three passes, three jobs

The bytes the model sees are built by three functions with disjoint responsibilities, run in this
order at both egresses:

1. **`project`** — unpack envelopes, splice anchored steers. Structural only: no tool names, no
   `bsed` knowledge, no rendering. `Message`-level, pure.
2. **`edit` block rendering** — array form → the model's `blocks` form. Stays exactly where it is
   today, in the completion-call callback beside reasoning-strip and argument normalisation, because
   that is the pipeline that builds a *request* rather than a *record*. Persistence takes the array
   form (pi's canonical), the wire takes blocks.
3. **steer/override assembly** — the `RequestPatch`, §5.

Splitting the passes this way means `project` never has to know that `edit` is special, and the
block pass never has to know that splitting happened — it sees N single-file arrays either way.

#### Where the `edit` parse happens

Once, in the tool. `EditArgs` implements `Deserialize` via `from_wire`, so `RegisteredTool::execute`
→ `EditFile::call` receives parsed `FileEdit`s; execution needs them to apply the edits, so the
parse cannot move earlier, and re-deriving them per projection would parse the whole history on
every request.

Minimisation also stays in the tool, and this is the reason the envelope earns its keep: the compact
anchored form needs the line each edit *landed* on (`matched_lines`), which only execution knows. So
the tool builds already-minimised arrays and puts them in the envelope. That deletes
`EditResolutions` (it existed only to carry placements to a later frame) and `EditLedger` (it
existed only to let three readers agree on one prepared form), and with them `prepared-form` caching
and the shared mutex.

What does *not* cross the boundary, and should not: paths (already relativised when the call was
persisted), splitting (derivable from `args`), and per-file results (already in the parts).

**Idempotence, and why it now holds by construction:** history loaded from a session is already
projected output, fed back in as `from_spec`'s history, so `project` runs over its own past results.

- **no envelope** in a loaded call → unpack is a no-op, which is the normal answer;
- **already-split** stored calls are single-file arrays, so there is nothing to split again;
- **already-minimised** arrays carry their own anchors, so there is nothing to minimise again;
- **blocks → array** is not `project`'s job at all (pass 2 owns it), so `project` cannot fight it;
- **steer splice** is keyed on aside entries already consumed.

This is stronger than the old three-reader arrangement: the ledger is gone, so there is no cache
that could be stale relative to the stored form — the stored form *is* the projected form.

**Generalising it:** the envelope is generic from the start, not an `edit` hook. Any tool whose one
call is really several may return parts, and `project` never learns which tool that was. `edit` is
simply the first and today the only user. A tool cannot mint *identities* — the framework does that
— but it does mint its own parts, which is the whole point.

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

Mid-turn steering is the *same* projection as §4 (its steer-splice half), which is why it needs no
mechanism of its own:

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
- `src/agent/project.rs` — **the** projection: envelope unpack and steer splice, nothing else. Pure
  `(&[Message], &[Aside]) -> Vec<Message>`, called by persist and by request building.
- `src/agent/wire.rs` — pass 2: reasoning strip, argument normalise, `edit` array → blocks. The
  request-only shape, holding the one `edit` special case. Absorbs today's `render_model_facing`.
- `src/agent/envelope.rs` — `prior_call_envelope` parse/emit, `CallPart`, and `split_id` minting.
  The only module naming the reserved key.
- `src/agent/persist.rs` — cursor over raw messages, projecting each delta.
- `src/agent/transcript.rs` — shrinks to load-path repair; the ledger, `PreparedEdit` and
  `prepare_edit_call` go.
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
5. **`edit` returns an envelope.** `type Output = EditBatch`; it minimises with the anchors it
   already has and emits parts. `project` unpacks; `wire.rs` renders blocks. Removal of
   `shape_edits`, `PreparedEdit`, `prepare_edit_call`, `paired_results` and `EditLedger`, and of
   `EditResolutions` once it has no reader. (Phase 2 can carry the old three-reader path behind the
   projection temporarily; this phase deletes it.)
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
- **A tool may return call/result *parts*, not identities.** Its return value may carry a
  `prior_call_envelope` whose parts the projection unpacks into N pairs; the ids are minted by the
  framework (`split_id`) because the provider issued one id and that rule must live in one place.
  Distinct from the run's own `ToolAnswer`, which stays unforgeable: a part never enters
  `answer_all`.

*Open:*

1. **Version risk.** We now depend on `AgentRun`'s public API and step protocol rather than only on
   `agent.prompt`. A rig bump that changes the protocol is a real port, where today it is a rebase.
   Mitigation: pin exactly, and keep `drive.rs` the only module that names a rig step type.
2. **Unary surface.** The summarizers still use `agent.chat(..)`. Fine, and it stays: they want one
   call, not a loop.
3. **Retrieval.** `ToolServerHandle::retrieval_effects` is not ported. prior registers no retrieval
   index; if the tool set ever gains one, the dispatch belongs in `before_call` beside
   `prepare_request`.
4. **Does `edit` still need `EditResolutions`?** Once the envelope carries minimised arrays, the
   only remaining reader would be nothing — phase 5 should confirm and delete it. If some path still
   wants the resolved placements (a replay that re-minimises?), keep them in the envelope instead of
   the context.
5. **Partial-batch persist.** A tool batch is one user message in the run, so a crash mid-batch
   persists nothing for that batch and the results are lost on restart. Today's
   `repair_tail_unanswered` writes "did not execute" placeholders instead. Keep that as a load-time
   repair, or persist answers as they land (which the projection can do, at the cost of writing a
   message the run has not yet committed)?
