# Incremental session writes — persist each completion turn, not just the whole turn

Status: design, pre-impl.

Decisions confirmed:

1. `TurnTranscript` is the persistence source of truth. rig's committed history is only a
   *test oracle*.
2. A completion turn is persisted as **one batch**: one lock window, one split decision shared by
   the call and result halves, one leaf entry, seeded ids so a retry is idempotent.
3. Persist at the start of each completion call, plus at dispatch. End-of-turn persistence shrinks
   to the in-flight response.
4. Two verbs: `prepare()` shapes the live unit into messages, in memory; `commit()` persists the
   prepared batch and folds it into `messages`.
5. A tool call whose argument JSON never completed is dropped, with no synthesized note.

## Problem

Session JSONL persists only when a turn *ends*, fails, or is aborted (`src/agent/turn.rs:679`).
During a multi-tool-call turn — minutes long — assistant turns, calls, results and reasoning live
only in memory: grepping the file for a completed completion call's output finds nothing, prompt
included, and a mid-turn process death loses the whole turn.

## Goals / non-goals

- The user prompt on disk at dispatch, before the first request is sent.
- Each completion turn's `[assistant + its toolResults]` unit on disk by the start of the next
  completion call; the on-disk tail is always a *valid prefix* (every persisted call answered).
- Everything the model produced captured on any exit — `Esc`, tool-dispatch failure, provider error
  mid-stream — through one uniform path.
- Non-goals: no fsync; no format change; no per-fragment writes; `summarize` / compaction stay
  unpersisted (they build a request directly, add no `LoggingHook` — pinned by a test).

## Source of truth: the transcript

```rust
struct TurnTranscript {
    /// Persisted messages: every element is on disk. Never rewritten.
    messages: Vec<Message>,
    /// Prepared batch awaiting its append. Retried verbatim on failure.
    prepared: Option<PreparedBatch>,
    /// The current completion call: the only accumulating state.
    live: LiveUnit,
}

struct PreparedBatch {
    messages: Vec<Message>,
    /// Batch id seed; entry ids are `{seed}-{i}`.
    seed: Uuid,
}
```

No commit cursor: `messages` is durable by construction, so there is no "prepared but unwritten"
region to address. `prepare()` is the only molder, `commit()` the only writer.

A tool call is not a message: it is an `AssistantContent::ToolCall` part of a `Message::Assistant`,
so a unit is `[assistant (may carry text, reasoning, N calls), user (N results)]`. On disk the
assistant half is **one** entry and the result half is **N** entries (`rig_message_to_pi_entries`,
`session.rs:2035`; `content_entry_id`, `session.rs:2003`), which is why a torn write leaves a
dangling *call* whose *result* entries are missing.

rig's committed history is not the source: `abandon_streamed_turn` rolls a turn back, `Repair`
substitutes a call name, `projection_start` re-reports a resumed batch — deltas from it would need
boundary grouping and offsets. The one divergence is *curation* (`assistant_content_is_visible`,
`transcript.rs:229`), left as a test-time cost.

## The live unit

One completion call is one assistant message, hence one `LiveUnit` per call:

```rust
struct LiveUnit {
    /// Parts in provider order, as they stream: `{ part, fragments, ended }`.
    /// A part's `End` supersedes its fragments; fragments survive only for a
    /// part that never ended.
    streaming: Vec<LivePart>,
    /// `event.content` from `on_model_turn_finished`: canonical, in provider
    /// part order, and supersedes `streaming` wholesale.
    accepted: Option<Message>,
    /// One `ToolResult` per answered call.
    results: Vec<UserContent>,
}
```

Separate fields keep **order observed rather than assumed**. Two `String`s
(`partial_text`/`partial_reasoning`) would encode a `reasoning → text → calls` shape, silently
reorder an interleaved turn (Gemini/Ollama legitimately emit text after a call), and lose text's
position. They also lose reasoning: `maybe_strip_reasoning` can key off provenance — streamed
`StreamEvent::Reasoning` is `reasoning_optional` and stripped, `AssistantContent::Reasoning` inside
`accepted`/`End` is `reasoning_conditional` and kept.

Writers:

| moment                                                 | writer                              |
| ------------------------------------------------------ | ----------------------------------- |
| `on_completion_call` — call N starts, after `commit()` | `start_live()`                      |
| `Start`/`Text`/`Reasoning`/`Arguments`/`End` events    | `push_event(..)`, **from the loop** |
| `on_model_turn_finished` — turn accepted               | `accept(response_id, content)`      |
| `StreamUserItem(ToolResult)` items                     | `push_tool_result(result)`          |

Fragments reach the **loop**, not the hook: each exists once, in the current poll's
`StreamAssistantItem`, and the hook sees only the accepted turn. The loop is thus the single drain
site, and one invariant follows: **the live unit dies with the stream future filling it**
(abort-on-drop), so a re-created stream inherits no fragments.

`LiveUnit::finish()` flattens the call half — `accepted` when present, else `streaming` in part
order, dropping any part that never ended. `complete()` ⟺ `accepted.is_some()` and `results` covers
every `call_ids()`. A predicate only; the commit decision is the caller's (see `report` below).

Two progress questions, not one method:

- `has_progress()` — `accepted.is_some()`. *Could* be used in alternate retry back-off semantics
  requiring models to complete a turn for reset. *NOT* our present semantics.
- `has_content()` — `prepared` or `live` non-empty: is there anything to write? Drives the
  `LlmInterrupted` back-off reset (`run_once` included, needs no session). A cut before acceptance
  means the provider misbehaved, but is generally working. Keep making progress without back-off.

### Unfinished call parts

A call part that opened but never ended was never dispatched, so rig projects no result and no
`ToolResult` will carry its id. Its argument JSON is incomplete, so it is **dropped**, with no
synthesized note — a fabricated call would enter the session as a real, answered request for a tool
that never ran. The model can re-emit it on retry, from its context prefix.

## `prepare()` and `commit()`

`prepare(&mut self, anchors, report) -> Option<PreparedBatch>` — in memory only, `None` when there
is nothing to write. It stores the batch in `self.prepared` and returns a clone, so the transcript
keeps its own copy for the retry:

1. **Retry-first**: while `prepared` is `Some`, return it unchanged. An I/O failure must not let a
   later turn take its place; ordering on disk is the whole point.
2. take `live` (`finish()`), then, on the pair: a. minimize `edit` calls with `edit_anchors` (filled
   by `on_outcome` before results project, `build.rs:852`); b. canonicalize `edit` arguments (block
   form → canonical array), single-file calls included; c. **one split decision for the pair** —
   split both halves into per-file calls/results, or neither (`split_edit_call_and_result`,
   `transcript.rs:428`). It needs both halves, so it cannot be decided when the call alone is known;
   d. calls with no results: synthesize them (direction A) only when the caller says the run is over
   (`report: Ended | Aborted`, below). While in a call a dangling one is a violation: log it, never
   write "did not execute" for a call about to run; e. results with no accepted turn
   (`TurnAbandoned`): synthesize the call half (direction B); f. package
   `[assistant, user(results)]` in provider part order.
3. Mint a fresh seed and `{seed}-{i}` ids for the batch.

`commit(transcript, session, anchors, report)` — a free async fn, because a `std::sync::Mutex` guard
cannot cross the `await`:

1. lock → `prepare(..)` → **drop the guard** (the clone is owned);
2. `session.append_messages(&batch.messages, &batch.seed).await`;
3. lock → on `Ok`: `messages.extend(batch.messages)`, `prepared = None`; on `Err`: change nothing
   and **log**.

`live` is cleared only by `prepare()`, never by `commit()`: after a failed append it belongs to the
next call.

Properties:

- Failure is a no-op, so the retry re-emits a byte-identical batch under identical ids: idempotent,
  not a duplicate turn.
- Not 2PC-atomic, and it does not pretend to be: `prepare` molds, the append is the single write,
  the fold records success. Nothing can dangle: the batch is complete before the write, and retried
  whole.
- Append failures are logged, not swallowed: today every site is `let _ = …`, and with ~25 per turn
  a disk-full must not vanish (`build.rs:765` precedent).

Staging before the append makes `messages` immutable absolutely, so `append_messages` transforms
nothing.

## Persist points

1. **Dispatch** — early in `prompt()`, after `session_context_messages(prior)` is read: append
   `Message::user(input)`, gated by the existing `TurnPersistence` count check
   (`turn.this_turn_persisted(c) < turn.sent()`, `src/agent.rs:205`), so retry/steer dedup is
   unchanged.
2. **`LoggingHook::on_completion_call`** (`src/agent/build.rs:697`) — `commit(..)` *before* the
   `user_cancel` Stop check, so a cancel-stop still writes the turn that just completed; then
   `start_live()`. The `CompletionCall` *stream item* gets no arm: it fires when a call *finishes*,
   before that turn has entered the transcript, so its delta is empty or a call behind.
3. **`on_model_turn_finished`** — `accept()` only, no I/O. The final assistant turn gets no
   following completion call, so it is prepared by the exit `commit()` — the only site that knows
   the run ended.
4. **Loop exit (normal and interrupted)** — `commit(.., report)`, where `report: Ended | Aborted` is
   the one thing `prepare()` reads to decide synthesis. Every earlier site passes `Ended`: a call
   start is not the run's end.

**Bug fixed in passing**: `turn.rs:568` builds `partial = vec![Message::user(input)]`
unconditionally, while the sibling arm (`turn.rs:381`) gates on `session_has_prompt`. With dispatch
persistence the prompt is always on disk, so this arm needs the same gate or every `Esc` after a
retry duplicates it.

## Uniform capture: one path for every exit

Every exit persists through `commit()`:

- **normal / `FinalResponse`** — the final assistant turn.
- **`<Esc>` / hook Stop / `Cancelled`, provider or protocol error mid-stream, tool-dispatch failure
  (`ToolExit::Fail`)** — `report: Aborted`. rig answers settled calls before aborting (`answer_all`,
  then `batch.interrupt()`), so real results are captured where they exist and synthesis fills only
  the genuinely unanswered slots: rig's `NO_RESULT_PROVIDED` semantics.
- **`MaxTurns`, `UnknownToolCall`, auto-retryable error** — same path, gated by the retry/steer
  dedup as today.

An abort *before* acceptance still writes: `live.streaming` is non-empty while nothing is accepted,
so the old `else if !output.is_empty()` arm disappears, and `output` is only the *returned* text.

### A turn nothing reports at all

Two rig paths commit an assistant turn without firing `on_model_turn_finished`: `TurnAbandoned`
(prior's `on_invalid_tool_call` returns `Retry`, `build.rs:824`) and repaired-name turns
(`settle_model_turn(recovered = true)`, `engine.rs:1170`, unreachable today). Their *results* still
project, so direction B rebuilds both halves from `call` + `name`. The remaining gap is a turn with
no `ToolResult` at all (no calls; last call discarded; `Retry` where the turn never surfaced). If a
recorded `TurnAbandoned` stream shows one, route the existing `ToolCall` projection item into a
prepared-call list answered by `push_tool_result` or by `prepare()`'s synthesis. Verify direction B
suffices before building it.

## Repair: directions A/B, load path only

`repair_pairing(messages: Vec<Message>) -> Vec<Message>` (pure, idempotent):

- **Direction A — assistant with unanswered calls** (crash between call and result; legacy files):
  one user message of text results, one per unanswered call — "`X` was interrupted before execution;
  it did not run", `is_error: false`, matching rig's `NO_RESULT_PROVIDED`. Today's `finalize`
  synthesis, lifted out (`transcript.rs:191`).
- **Direction B — orphan results** (a `ToolResult` whose `CallId` appears in no assistant part):
  synthesize the missing assistant turn *before* the results, from what the result carries —
  `call: CallId`, `name: ToolName` (`transcript.rs:87`), arguments `{}`.

The **write path never calls `repair_pairing`**: `prepare()` runs A/B on the live unit, where both
halves are still mutable, so a half-unit never reaches `messages`. Direction B's reordering is
memory-only — never a file rewrite.

## Batch append: a pure serializer

`Session::append_messages(&self, msgs: &[Message], seed: &Uuid) -> crate::Result<()>` (refactoring
`append_message`, `session.rs:1243`) writes every entry plus **one** trailing leaf in one
storage-lock window and one index update. It transforms nothing — minimize, canonicalize and split
already happened in `prepare()`.

The bug it removes: `append_message` splits the two halves **independently**
(`session.rs:1259-1261`) — `canonicalize_edit_tool_calls` splits whenever `per_file_blocks` parses a
>1-file blob, `split_edit_tool_results` only when `parse_file_results` yields >1 entries. When the
blob parses but the result is a plain argument-error string, you get N calls and one result: `t1`
(`split_id` returns the original for `i == 0`) looks answered, the derived `t1_f1nN …` do not.
Providers reject that on reload, and direction B cannot see it. Deciding the split once, from both
halves, makes the shape unrepresentable.

Ids and atomicity:

- entry ids derive from the batch seed, `{seed}-{i}`; the seed lives *inside* `PreparedBatch`, so a
  retried batch writes identical ids. `build_index` must skip an id already present when pushing to
  `order`/`children`, making re-append idempotent instead of duplicating the turn in `/tree` — today
  a fresh `Uuid::now_v7()` (`session.rs:1262`) meets an unconditional `order.push`
  (`storage.rs:413`).
- chain is `leaf → e1 → … → eN` with one trailing leaf at `eN`; a process death mid-write leaves at
  most a torn last line, which `build_index` already skips (`storage.rs:442`).

## Load paths

`extract_messages` (`session.rs:1733`) already groups buffered `toolResult` entries into one
trailing `Message::User`. Two hooks:

1. **In memory, at context build** — `build_context` / `session_context_messages`
   (`compaction.rs:75`) run `repair_pairing`, so a resumed session's first request is valid before
   anything is written. Rig also repairs on load (`pairing::repair`); this is prior's own.
2. **Persist direction A on first append** — if the raw branch tail ends on an assistant turn with
   unanswered calls, `append_messages` writes the synthesized results before the new batch. One-way;
   afterwards the tail ends on results. `toolName` comes from the dangling call in the assistant
   entry (no `tool_name_by_call_id` map on a fresh load). Direction B stays memory-only.

## Plumbing

- `LoggingHook` already holds `transcript: Arc<Mutex<TurnTranscript>>` (`build.rs:604`), so
  `accept`/`push_tool_result` need no new field. It gains `session: Option<Arc<Session>>` only, for
  the `on_completion_call` commit. Stream events never reach the hook, so `push_event` stays in the
  loop.
- `CodeAgent::prompt`: `session: Option<&Session>` → `Option<Arc<Session>>` (interactive holds the
  `Arc`; `run_once` and tests pass `None`).
- Lock discipline: `prepare` under the guard, the owned batch outside it, re-lock to fold. Hook I/O
  now sits on the dispatch path — one window a turn, waiting for a `write_all` + flush. Intentional
  and bounded. `commit()` is *not* one lock window end to end (a `std::sync::Mutex` guard cannot
  cross the await); the release-and-reacquire is deliberate, not to be "fixed".

## Edge cases

- Prompt dedup across retries/steers: unchanged count machinery; the error/cancel arms keep their
  gate as a re-check (covers a dispatch append that failed on I/O).
- A first call that fails immediately leaves two adjacent user prompts; the lenient merge covers it.
- `summarize` / compaction build no hook → cannot write (test). `!` capture and metadata writes run
  at the REPL level; in-turn submits are blocked (`src/agent/pumps.rs:73`).
- `run_once`: sessionless, never persists; the transcript still accumulates for `has_content` and
  `has_progress`.
- `replace_accepted_turn_choice` is the one path no repair can detect: rig substitutes a turn before
  it enters history, and the transcript would still look well-formed. Unreachable today (prior's
  hook returns only Continue/Stop).

## Test plan

1. `LiveUnit`: `End` supersedes its part's fragments; `accept` supersedes `streaming`; an
   interleaved turn (`call, text`) keeps provider part order through `prepare()`.
2. Fragments cut mid-`Arguments`: the call is dropped, no note is written, the rest persists.
3. Retry: failed append, then `commit()` again → same ids, one turn in `/tree`; a turn prepared
   while a failed batch is pending → ordering preserved.
4. `repair_pairing`: direction A, direction B, idempotence.
5. Recorded `TurnAbandoned` stream → direction B suffices (else the prepared-call list is
   justified).
6. Dispatch persist + retry/steer dedup (adapt the `agent.rs` turn-persistence tests).
7. Edit split: multi-file call success → N/N; multi-file call with a text error result → 1/1;
   interrupted multi-file call synthesized → 1/1.
8. Parity: transcript vs `FinalResponse.messages` (minus prompt), a normal multi-tool run.
9. Torn tail: truncated last line → `build_index` skips → context valid after repair.
10. Summarize / compaction write nothing.

## Follow-ups

- Update `doc/session_storage.md` §3.3/3.4 and the `turn.rs` persistence comments with the impl.
- fsync remains absent by decision.
