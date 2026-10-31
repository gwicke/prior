# Driving rig's `AgentRun` directly

*Status: design, ready for implementation. Supersedes the retry half of `doc/261026_rig_steering.md`
and of `doc/tool_error_handling.md` §"Mid-turn failure & resume". Mechanics:
[`261030_rig_direct_drive_impl.md`](261030_rig_direct_drive_impl.md).*

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

Everything needed is public in `rig-agent` 0.45.0: `AgentRun` (the whole step protocol, plus
`Serialize`), `StreamedTurnAssembler`/`StreamedTurn`, `prepare_request`/`RequestPatch`,
`RegisteredTool::execute`, `Aside` for host records, and `IntoToolOutput` (blanket over
`Serialize`), so a tool can return a parts list with no trait work. The itemised surface, and the
two `pub(crate)` gaps we route around, are in
[`261030_rig_direct_drive_impl.md`](261030_rig_direct_drive_impl.md).

## Design

The premise throughout: **the model sees what we tell it, not what it sent.** Its memory is the
context we build. Nothing in rig requires the request history and the recorded history to be the
same bytes; `prepare_request` takes whatever history the driver hands it, and `AgentRun`'s log only
has to be internally pair-valid. Every awkward reshape in today's code comes from conflating the
two.

### 1. One module owns the loop

`src/agent/drive.rs` replaces `agent.prompt(..).stream()`:

```text
// `prior_history.len()` is the parts/verbatim boundary: everything at or
// above it was appended by this loop and may carry a parts list (§4).
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
            for call in calls {
                // sequential; observer paints; cancel check
                let out = execute(call);          // may be a parts list (§4)
                persist_answer(&call, &out);      // §3: the boundary inside a batch
                run.answer(ToolAnswer::executed(call, out))?;
            }
            // The run still commits the batch as one message on the last
            // answer; `persist_answer` is what makes an interrupted batch
            // survive (idempotent per call id, §3).
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

The canonical history is `run.messages()`, append-only, provider-exact, with every accepted turn
already in it and every tool batch committed as one user message. So:

- keep `cursor` (seeded from `run.projection_start()`);
- persist `project(&run.messages()[cursor..])` and advance `cursor`.

That is the whole of persistence. `TurnTranscript`, `LiveUnit`, `LivePart`, `prepare`, `assemble`,
`RunState::Ongoing|Ended`, `assistant_from_results` and the "did the reassembly survive" question
all go away.

**Bounds.** The projection changes shape and count, so the cursor indexes *raw* messages while the
written form is projected. Bounds are only ever chosen where the committed tail is a complete prefix
— every assistant turn's calls answered — which is the only state that can be observed at a boundary
and the only one whose projection is stable.

**Granularity: the held answer, written at the close.** `answer_all` is all-or-none — the run
commits the batch as one user message only once every call is answered — so a cursor over
`run.messages()` alone would lose every result of a batch that ends early. `AgentRun::answer` (the
single-answer form) does not lift that: the batch still commits at completion.

Early writes were considered and dropped. Writing each answer as it lands would put a results
message in the store that the run has not produced, and the later committed batch would then have to
recognise and skip it — a second writer, and a duplicate to reconcile. The requirement is weaker and
cheaper to meet: *nothing that ran may be lost*. So the loop **holds** each answer (`note_answer`)
and the close turns the held answers plus rig's own "did not execute" text for the rest into one
results message, in **call order** so a partial batch has the shape of a complete one.

That is the write-path half. The read path stays: a process that dies mid-batch never runs the
close, so load still repairs (`repair_pairing`).

A partially-answered batch is therefore valid at every moment.

What does *not* change: session storage, the pi JSONL format, entry ids and batch seeds. Those live
behind `Session::append_messages` and stay.

### 4. A tool states its own call shape

Every fact about how a call should be spelled is held by exactly one frame: the tool that
understands it. It parsed the arguments, it knows how many files it was given, it knows where each
edit landed, and it knows what each file's result was. So a tool states
**the call as it understood it, plus that call's result** — once, or once per file.

That inverts the current arrangement. Today the tool returns text, and a separate pass later
re-parses the call and the joined result to reconstruct a shape the tool already had in hand. With
the tool stating the shape, no later pass has anything to reconstruct, and `project` makes exactly
**one** decision per call: whether the result carries parts. Everything else it takes verbatim.

The statement is split in two by what is *knowable when*:

- **`preprocess_args(&Value) -> Value`** — what the call is, as the tool reads it: `read`/`write`/
  `grep`/`sed` relativize `path`, `bash` strips its `cd <cwd> &&` prefix. Pure and idempotent, and
  knows nothing of results — so it is also what the display calls at dispatch, before anything runs.
  Without that, a slow `bash` would be labelled with the model's absolute path and then have its
  result recorded under a relative one: the label and the record would disagree about a call that
  never changed, and there is no second copy to catch it.
- **the parts** — that value plus the result, once the call has run.

For the five single-result tools there is nothing to repeat and no signature to change: the loop
computes `preprocess_args` at dispatch, then wraps the returned `ToolResult` output into the one
part and answers the run with it. So the shape is learned once per live call, and one value serves
the panel, the record and the wire. Only `edit` builds parts itself, because only it has N results
needing N calls — and only its shape is unknowable before execution (minimisation needs where each
edit landed), which is why its panel already defers to the pair.

Rig's protocol agrees, and its one hard limit says where the boundary is. `ToolAnswer` is
unforgeable (private fields, plus `compile_fail` tests in `calls.rs`), and `answer_all` validates
each answer's `(turn, index, call id)` against the pending batch — so a driver cannot hand the run a
finished call+result exchange, and there is no public append. But `streamed_turn` rejects a call
only when its **name** is not allowed by the turn's `TurnPolicy`; count and arguments are not
inspected. So the *assistant* turn is frozen at `streamed_turn`, while what the answer carries is
ours: `ToolResult::with_output` replaces the model-visible content and keeps the disposition, and
`answer_all` commits that content verbatim into the run's messages.

That is what makes the split work rather than just being a division of labour. The parts go into the
run, so `project` unpacks what execution stated instead of re-deriving it over the whole history on
every request — and the cost that re-derivation implied (N calls, every turn, for the life of the
session) disappears rather than being optimised.

The consequence to hold onto: parts now live *inside* the run, so `project` must unpack results only
at or above `prior_history.len()`. Above it, everything is the loop's own output; below it,
everything was written already unpacked, and a `bash` result that happens to look like parts must
stay text.

The mechanics — `CallPart`, `preprocess_args`, the injection, how a parts list is recognised without
a marker, the pass-2 render, and the per-tool checklist with its deletions — are in
[`261030_rig_direct_drive_impl.md`](261030_rig_direct_drive_impl.md).

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
- a steer is recorded on the run as an `Aside` (`append_entry`): `kind: "prior.steer"`,
  `author: Author::user()`, `value: <the Message>`, `after: <the last message id>`. Asides are
  serialized with the run and **excluded from provider requests**, so it costs nothing until we
  choose to send it, and survives a restart. `project` splices them in, so persist and the wire show
  the steer at the same position, once;
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
- `src/agent/observer.rs` — `TurnObserver` (display, `--log-messages`, cancel; the steering drain is
  phase 4's addition). Absorbs today's `LoggingHook`; `turn.rs` keeps the pump.
- `src/agent/project.rs` — **the** projection: parts unpack and steer splice, nothing else. Pure
  `(&[Message], &[Aside]) -> Vec<Message>`, called by persist and by request building.
- `src/agent/parts.rs` — `CallPart`, the single-part builder (`preprocess_args` + `ToolResult`), the
  parts-list predicate, and `split_id` minting. The only module that decides what a part looks like.
- `src/agent/tool_shape.rs` — the `preprocess_args` table, one entry per live tool. Registered
  beside the tools in `build()` because `#[rig_tool]`'s generated struct cannot implement a
  prior-side trait. Decides only what a call reads as, never what runs.
- `src/agent/wire.rs` — pass 2: reasoning strip, then `edit` array → blocks. The request-only shape,
  holding the one `edit` special case. Absorbs today's `render_model_facing`.
- `src/agent/persist.rs` — the cursor over `run.messages()`, the held answers of a batch in flight,
  and the close that writes a batch whose results never all landed.
- `src/agent/project.rs` — the projection: unpack the parts a tool answered with, and the complete
  prefix rule persistence bounds on. Pure, tool-agnostic, and a fixed point on its own output.
- `src/agent/transcript.rs` — shrinks to message construction, the model-facing render, the
  reasoning strip and the load-path repair. The live-unit machinery, `prepare`, `assemble`,
  `RunState`, `preprocess_calls` and the batch-shaping half are deleted.
- `src/tools.rs` — `read`/`write`/`grep`/`sed`/`bash` keep `Result<String, ToolError>` and gain a
  `preprocess_args` function (`sed`/`read`/`write`/`grep` relativize `path`, `bash` strips its `cd`
  prefix). Only `edit` changes its return type, to `Vec<CallPart>`: parse → split → minimise →
  relativize.
- `src/agent/turn.rs` — `prompt` becomes a thin wrapper over `drive`; `run_once` and the summarizers
  keep their signatures.

### 7. Phases

1. **Parity harness. Done.** `src/agent/drive.rs` (the step-wise loop) and `src/agent/observer.rs`
   (what `LoggingHook` did, as explicit callbacks) are live behind `PRIOR_DRIVE=direct`. Every
   scenario in `mock_turn.rs`, `retry_backoff.rs`, `compaction_esc.rs` and the rest of the suite
   passes on both paths, and the two byte streams for the layout probe diff clean — which is what
   the gate asks for: the items are rig's own, so the turn pump is untouched.
   - **The loop is step-wise, not one future.** `DriveLoop::next` yields one item at a time because
     the turn pump selects on it alongside the key queue, the confirm port and the cancel signal. A
     single drive future would have to re-implement that `select!`.
   - **`run_stream` is a generator, not a `poll_fn`.** A `poll_fn` builds a fresh `next` future on
     every poll and loses the in-flight provider stream; the attempt restarts at each pause. This
     cost an afternoon to find, so it is written down here rather than in a comment nobody reads.
   - **`Done` is terminal.** rig's `next_step` keeps answering `Done` from its terminal state, so
     the loop marks itself finished; without that the final response is emitted forever.
   - `TurnTranscript` is still the writing path (phase 2 replaces it with the cursor). The
     observer's `accept` therefore still records the accepted turn, which is what keeps the two
     paths' persisted output identical.
2. **Drive loop, single attempt, and the cursor. Done.** The loop takes the whole turn (`CallModel`
   → stream → accept → `CallTools` → answer → `Done`), and it is now the **only** path:
   `LoggingHook` and the `PRIOR_DRIVE` switch are gone. Persistence is `src/agent/persist.rs`, a
   cursor over `run.messages()`, projected by `src/agent/project.rs`. `TurnTranscript`'s live-unit
   machinery, `prepare`, `assemble`, `RunState`, `preprocess_calls` and the batch-shaping half of
   `transcript.rs` are deleted (net −926 lines). What stays in `transcript.rs` is what it should
   always have been: message construction, the model-facing render, the reasoning strip, and the
   load-path repair.
   - **`preprocess_args` runs once per live call.** The loop computes it at dispatch, hands it to
     the panel, then builds the part from the same value and wraps it into the answer
     (`ToolResult::with_output`). `project` unpacks; it never re-derives. The phase-5 debt is
     closed.
   - **The projection runs over the whole history, not a window.** It is a fixed point on
     already-projected messages (tested), so a boundary inside the history is unnecessary — which
     also removes the `prior_history.len()` guard as a correctness dependency. A stored result is
     plain text, so `unpack` finds nothing there.
   - **A turn that ends early still records what ran.** rig commits a batch as one message only when
     every call is answered, so the persister holds each answer as it lands (`note_answer`) and
     `close` writes the unfinished batch from those answers plus rig's own "did not execute" for the
     rest — in **call order**, so a partially executed batch has the same shape as a complete one.
     That is the write-path half; `repair_pairing` remains the read path for a process that died
     before it could run.
   - **`close` takes no run.** The abort path drops the loop, so the persister is shared with a
     `PersistHandle` the pump keeps; both closes are idempotent.
   - **A malformed `edit` block still gets its terse text.** That rewrite used to live in the hook's
     outcome path on `as_text`; it now happens when the part is built, where the model-visible text
     is decided.
3. **Retry by resume. Done.** The attempt loop lives in `prompt` (`src/agent/turn.rs`), over a
   `DriveLoop` built once per user prompt and borrowed by `run_stream`. A failed attempt breaks the
   pump loop, waits out the back-off with the key pump still running, and polls the *same* loop
   again: `next_step` from the run's untouched `AwaitingModel` re-emits the identical `CallModel`.
   `LlmInterrupted`, `TurnPersistence`, the `"continue"` prompt and the whole `next_input` /
   `steer_delivered` / `turn_params` dance are deleted.
   - **`run_stream` yields the error and keeps going.** It does not `return`: the retry policy is
     the caller's (back-off, budget, the cancellable wait where `Esc` and the key queue already
     live), so dropping the stream is how the caller gives up and polling again is how it retries.
   - **Retry correctness had a persistence precondition.** `close` must not re-write a unit the
     cursor already wrote, and a retried turn hits exactly that shape (a batch completes, the
     continuation fails, the turn exits). `set_tail` now records the turn's index and
     `write_committed` clears the held copy once the cursor passes it.
   - **The corrective hint is a context document, not prompt text.** `Drive::retry_hint` is a
     one-slot mailbox the loop drains into `RequestPatch::extra_context` at the next `before_call` —
     `take`, not `clone`, so it reaches exactly one attempt. The model sees why its call failed; the
     transcript never keeps the reminder. This is the first user of the steering machinery §5
     describes, and it is why a malformed tool call no longer costs a synthetic user message.
   - **Asserted end-to-end**, not just structurally: `tests/retry_as_attempt.rs` drives the real
     binary through a mid-turn 429 and asserts the session holds exactly one user message, and that
     no `"continue"` ever reached the provider. Verified to fail if a synthetic message is
     re-introduced.
   - `doc/tool_error_handling.md` §"Mid-turn failure" is retired with it.
4. **Steering and boundary settings.** `SteeringQueue`, the projection's steer insertion and scalar
   overrides, the mid-turn UI entry points and `/reasoning`-style mid-turn commits.
5. **Tools state their own call shape.** `CallPart`, the injection and the parts-unpack in `project`
   land first with an identity `preprocess_args`, which is a pass-through: the run's messages change
   shape, and unpack restores today's bytes exactly. Then `preprocess_args` for the five
   single-result tools, one at a time — safe incrementally only while `tool_args::normalize` still
   covers the unconverted ones, which is why the impl notes order it with `edit` last and delete
   `normalize` only at the end. The registry-completeness test lands with the deletion, since that
   is when a missed tool starts storing absolute paths. `edit` gains parse → split → minimise →
   relativize and switches to `Vec<CallPart>`; the other five only gain `preprocess_args`.
   Deletions: `shape_edits`, `PreparedEdit`, `prepare_edit_call`, `paired_results`, `EditLedger`,
   `EditResolutions`, `tool_args::normalize` and `normalize_messages`. `wire.rs` renders blocks.
6. **Cleanup.** Delete `HookContext` usage and the hook structs' rig impls; update
   `doc/261026_rig_steering.md` (its "Prior's Current Architecture Is Correct" section becomes the
   opposite of the truth), `doc/tool_error_handling.md` and `doc/261026_tool_path_relativization.md`
   (relativisation is now a tool-side property).

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
- **A tool states its own call shape, in two halves.** `preprocess_args` states what the call *is*
  (pure, knowable before execution, so the display uses it too); the parts state args *and* results
  (only `edit`, only after execution). Single-result tools return `String` unchanged — the framework
  builds their one part. The projection unpacks `edit`'s N parts into N pairs; ids are minted by the
  framework (`split_id`) because the provider issued one id and that rule must live in one place.
  Distinct from the run's own `ToolAnswer`, which stays unforgeable: a part never enters
  `answer_all`.
- **Normalisation is a tool property, not a pass.** `preprocess_args` relativizes path arguments and
  `bash` strips its own `cd` prefix. Called **once at dispatch**, with the result carried into the
  part, so the panel, the record and the wire read one value rather than three functions that had to
  be proven to agree. `tool_args::normalize` is deleted; replay needs no rewriting because the
  stored form is already compact.
- **`EditResolutions` has no reader once the tool formats its own results.** Delete it with
  `EditLedger` rather than repurposing it.
- **A turn that ends early keeps what ran — by holding the answers, not by writing them early.**
  `answer_all` commits a batch only once every call is answered, so the loop holds each answer as it
  lands and the close writes the unfinished batch from those plus rig's own "did not execute" for
  the rest, in call order. Writing each answer as it landed was considered and dropped: it would put
  a results message in the store the run has not produced, and the committed batch would then have
  to recognise and skip it. The requirement is only that nothing be lost. Repair runs on both sides
  still: the write path closes the trailing unit, the read path does the same on load.

*Open:*

1. **Version risk.** We now depend on `AgentRun`'s public API and step protocol rather than only on
   `agent.prompt`. A rig bump that changes the protocol is a real port, where today it is a rebase.
   Mitigation: pin exactly, and keep `drive.rs` the only module that names a rig step type.
2. **Unary surface.** The summarizers still use `agent.chat(..)`. Fine, and it stays: they want one
   call, not a loop.
3. **Retrieval.** `ToolServerHandle::retrieval_effects` is not ported. prior registers no retrieval
   index; if the tool set ever gains one, the dispatch belongs in `before_call` beside
   `prepare_request`.
4. **Parts-shape false positives.** The predicate is shape-based. It is safe because only the loop
   puts parts in a message and persisted results are plain text — but with the projection now
   running over the *whole* history rather than a window, the argument rests entirely on that second
   fact, not on a boundary. Tested as a fixed point (`project`) and as "no parts list in the
   output". If a tool ever needs to return a genuinely part-shaped array as data, that is the moment
   to reintroduce a marker key. Reversible in one place (`src/agent/parts.rs`).
5. **Display and parts.** The `edit` panel paints from the same parts the model sees. Worth
   confirming during phase 5 that no per-file panel detail (a skipped file, an unchanged one) was
   only ever carried by `PreparedEdit` and needs to become a part field.
