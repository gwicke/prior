# Incremental session writes — persist each completion turn, not just the whole turn

Status: design, pre-impl.

Decisions confirmed:

1. `TurnTranscript` is the persistence source of truth (append-only, so it needs no reconciliation
   machinery). rig's committed history is only a *test oracle*.
2. A completion turn is appended as **one batch**: one lock window, one split decision shared by the
   call and result halves, one leaf entry, seed-derived ids so a retry is idempotent.
3. Persist at the start of each completion call, plus at dispatch. End-of-turn persistence shrinks
   to the in-flight response and the partial content of an interrupted turn.
4. Two verbs: `prepare()` massages the open unit into final messages, in memory; `commit()` persists
   the uncommitted messages and advances the commit counter.

## Problem

Session JSONL persists only when a turn *ends*, fails, or is aborted (`src/agent/turn.rs:656`).
During a multi-tool-call turn — minutes long — assistant turns, calls, results and reasoning live
only in memory: grepping the file for a completed completion call's output finds nothing, prompt
included, and a mid-turn process death loses the whole turn.

## Goals / non-goals

- The user prompt on disk at dispatch, before the first request is sent.
- Each completion turn's `[assistant + its toolResults]` unit on disk by the start of the next
  completion call; the on-disk tail is always a *valid prefix* (every persisted call answered).
- Everything the model produced captured on any exit — `Esc`, tool-dispatch failure, provider error
  mid-stream — through one uniform path.
- Crash residue (a torn batch) repaired by one pure helper shared by write and load paths.

Non-goals: no fsync; no format change; no per-fragment writes; `summarize` / compaction requests
stay unpersisted (they build a request directly, add no `LoggingHook` — pinned by a test).

## Source of truth: the transcript

`TurnTranscript` (`src/agent/transcript.rs:129`) holds **only finished units** in `messages` — a
`Vec<Message>`, append-only, never rewritten. rig's committed history is not
(`abandon_streamed_turn` rolls a turn back, `Repair` substitutes a call name, `projection_start`
re-reports a resumed batch), so deltas from it would need boundary grouping and offsets. We do not
switch; rig's history is a **test oracle**.

> Invariant: `messages[..committed]` is durable; `messages[committed..]` is final but not yet
> written; both are immutable. The only mutable state is the **open unit** — an assistant half
> whose results have not arrived — and it stays out of `messages` until `prepare()` folds it in.

A tool call is not a message: it is an `AssistantContent::ToolCall` part of a `Message::Assistant`,
so a unit is `[assistant (may carry text, reasoning, N calls), user (N results)]`. On disk the
assistant half is **one** entry and the result half is **N** entries
(`rig_message_to_pi_entries`, `session.rs:2035`; `content_entry_id`, `session.rs:2003`), which is
why a torn write leaves a dangling *call* whose *result* entries are missing.

Progress is tracked at commit points only — the file's granularity. The transcript keeps two things
the file cannot: it holds the open unit, and it works with no session (`has_progress` drives the
`LlmInterrupted` back-off reset for `run_once` too, and must count the open unit). The one
divergence is *curation*: invisible turns are dropped (`assistant_content_is_visible`,
`transcript.rs:229`).

## The open unit and the two verbs

```rust
struct OpenUnit {
    /// The call half: the accepted assistant message, once one arrives. `None`
    /// is the `TurnAbandoned` shape — results with no assistant half.
    assistant: Option<Message>,
    /// The result half: one ToolResult per answered call.
    tool_results: Vec<UserContent>,
}

struct TurnTranscript {
    messages: Vec<Message>,        // finished messages; append-only
    committed: usize,              // count of `messages` durably appended
    pending_seed: Option<Uuid>,    // batch id seed, held until commit succeeds
    open: Option<OpenUnit>,        // the one unit under construction (mutable)
    partial_text: String,          // streamed text of the in-flight turn
    partial_reasoning: String,     // streamed reasoning of the in-flight turn
}
```

Not `pending_tool_calls`/`pending_tool_results`: the call half is a whole `Message::Assistant` (it
carries text and reasoning too), the result half only parts of one future `Message::User`.

- `prepare(&mut self, anchors, synthesize) -> Prepared` — **in memory only.** Folds the open unit
  into `messages` (the six steps below), consumes `partial_text`/`partial_reasoning` into a
  `Message::Assistant` with no response id (honest `responseId: None`) when non-empty, mints
  `pending_seed` if absent, and returns `{ batch: messages[committed..].to_vec(), seed, target:
  messages.len() }`. Idempotent for a fixed delta: entry ids are `{seed}-{i}` with `i` the index
  *within the delta*, so a batch that grew between a failed commit and the next `prepare` keeps the
  earlier ids.
- `commit(transcript, session, anchors, synthesize) -> Result<()>` — a free async fn, because a
  `std::sync::Mutex` guard cannot cross the `await`. Lock → `prepare()` → **drop the guard** (the
  batch and seed are owned, so they cross the await) → `session.append_messages(&batch, &seed)
  .await` → lock → `committed = target`, clear `pending_seed`. Nothing happens without a session
  except the advance.

`prepare()`'s order, on the *pair* — the one place all shaping happens:

1. minimize `edit` calls with `edit_anchors` (filled by `on_outcome` before the results project);
2. canonicalize `edit` arguments (block form → canonical array), single-file calls included
   (`canonicalize_edit_tool_calls`'s second job, `session.rs:64`);
3. **one split decision for the pair**: split both halves into per-file calls/results, or neither
   (`split_edit_call_and_result`, `transcript.rs:428`);
4. if the unit has calls but no results (abort, tool-dispatch failure, or `synthesize`), synthesize
   the missing ones — direction A below;
5. if results exist with no `open.assistant` (`TurnAbandoned`), synthesize the call half — direction
   B below;
6. push `[assistant, user(results)]` into `messages`.

Preparing *before* the push makes `messages` immutable absolutely, not merely "behind a cursor". So
**`append_messages` performs no transform**: write entries, one trailing leaf, update the index.
`append_message`'s canonicalize/split keep their remaining customers (`!` capture, metadata writers)
and become provable no-ops on transcript output.

Commit is not 2PC-atomic: `prepare` stages, the append is the single write, and `committed = target`
records what succeeded. A failed append changes nothing, so the next `commit()` re-emits the same
batch under the same ids.

The boundary point passes `synthesize = false`: at a call start the previous turn's results are
necessarily in hand, so a trailing unanswered call there is an invariant violation — log it; never
write "did not execute" for a call about to run.

## Persist points

1. **Dispatch** — early in `prompt()`, after `session_context_messages(prior)` is read (so the
   prompt is not in its own `prior`): append `Message::user(input)`, gated by the existing
   `TurnPersistence` count check (`turn.this_turn_persisted(c) < turn.sent()`, `src/agent.rs:205`),
   so retry/steer dedup is unchanged.
2. **`LoggingHook::on_completion_call`** (`src/agent/build.rs:697`) — `commit(…, synthesize:
   false)` *before* the `user_cancel` Stop check, so a cancel-stop still writes the turn that just
   completed. The `CompletionCall` *stream item* gets no arm: there the just-finished turn has not
   even committed to the transcript, so its delta is empty or one call late.
3. **Loop exit (normal)** — `commit()` for what remains: the final assistant turn, which gets no
   following completion call.
4. **Every interrupted exit** — the same call with `synthesize: true`; see "Uniform capture".

**Bug to fix in passing**: `turn.rs:568` builds `partial = vec![Message::user(input)]`
unconditionally, while the sibling arm at `turn.rs:381` gates on `session_has_prompt`. With dispatch
persistence the prompt is always already on disk, so this arm must use the same gate or every `Esc`
after a retry duplicates it.

## Uniform capture: one path for every exit

Every exit persists through `commit()`, which also logs append failures — today every site is a
`let _ = …`, and with ~25 per turn a disk-full must not vanish silently (`build.rs:765` precedent):

- **normal / `FinalResponse`** — the final assistant turn.
- **`<Esc>` / hook Stop / `Cancelled`, provider or protocol error mid-stream, tool-dispatch failure
  (`ToolExit::Fail`)** — delta + trailing synthesis + partial text/reasoning.
- **`MaxTurns`, `UnknownToolCall`, auto-retryable error** — delta + trailing synthesis (the last
  finished turn got no following hook), gated by the retry/steer dedup as today.

The partial is only ever the *current* turn, so an interrupted turn is never re-persisted as a
duplicate. An abort *before* the first assistant turn still writes it: delta may be empty while the
partial is not, so the old separate `else if !output.is_empty()` arm disappears.

### The shared repair: two directions, one pure function

`fn repair_pairing(messages: Vec<Message>) -> Vec<Message>` (`transcript.rs`, pure, idempotent)
resolves dangling pairing in both directions, and is what `prepare()`'s steps 4/5 call. The **load
path** calls it too, so the two cannot drift.

- **Direction A — assistant with unanswered calls** (crash between call and result; aborted turn;
  tool-dispatch failure): one user message of text results, one per unanswered call — "`X` was
  interrupted before execution; it did not run" — `is_error: false`, matching rig's
  `NO_RESULT_PROVIDED`. Today's `finalize` synthesis, lifted out (`transcript.rs:191`).
- **Direction B — orphan results** (a `ToolResult` whose `CallId` appears in no assistant part):
  synthesize the missing assistant turn *before* the results, from what the result carries — `call:
  CallId`, `name: ToolName` (`transcript.rs:87`), arguments `{}`. Left by rig paths that commit
  without firing `on_model_turn_finished`:
  - **`TurnAbandoned`** — prior's `on_invalid_tool_call` returns `Retry` (`build.rs:824`), so rig
    commits a rollback pair (partial assistant + rejected call, feedback + synthetic results)
    straight to history. The retry's `on_completion_call` fires on time — *timing* is fine — but
    only the feedback results reach the transcript. Direction B supplies the other half.
  - **Recovered / repaired-name turns** — `Repair` renames the call in place and
    `settle_model_turn(recovered=true)` skips `on_model_turn_finished` (`engine.rs:1170`). Same
    shape; unreachable today (prior returns only `Retry`; rig's fallback is `Fail`/`Ignore`).
  - Legacy files from today's error paths.

Direction B is a *reordering*, so it applies only where the orphan is still mutable (the open unit)
or in memory (load) — **never** as a file rewrite.

## Batch append: a pure serializer

`Session::append_messages(&self, msgs: &[Message], seed: &Uuid) -> crate::Result<()>` (refactoring
`append_message`, `session.rs:1243`) writes every entry plus **one** trailing leaf in one
storage-lock window and one index update. It transforms nothing — the pair was already minimized,
canonicalized and split in `prepare()`.

The bug it removes: `append_message` splits the call half and the result half **independently**
(`session.rs:1259-1261`) — `canonicalize_edit_tool_calls` splits whenever `per_file_blocks` parses a
>1-file blob, `split_edit_tool_results` only when `parse_file_results` yields >1 entries. When the
blob parses but the result is a plain argument-error string, you get N calls and one result: `t1`
(`split_id` returns the original for `i == 0`) looks answered, the derived `t1_f1nN …` do not.
Providers reject that on reload, and direction B cannot see it (it fixes orphan *results*). With the
split decided once in `prepare()` from both halves, this shape cannot arise.

Ids and atomicity:

- entry ids derive from the batch seed, `{seed}-{i}`; `pending_seed` is held in the transcript until
  `committed` advances, so a failed batch retried writes identical ids. `build_index` must skip an
  id already present when pushing to `order` (and `children`), making re-append idempotent instead
  of duplicating the turn in `/tree` — today a fresh `Uuid::now_v7()` (`session.rs:1262`) meets an
  unconditional `order.push` (`storage.rs:413`).
- Chain is `leaf → e1 → … → eN` with one trailing leaf at `eN`. A process death mid-write leaves at
  most a torn last line, which `build_index` already skips (`storage.rs:442`).

## Load paths

`extract_messages` (`session.rs:1733`) already groups buffered `toolResult` entries into one
trailing `Message::User`. Two hooks:

1. **In memory, at context build** — `build_context` / `session_context_messages`
   (`compaction.rs:75`) run `repair_pairing`, so a resumed session's first request is valid before
   anything is written.
2. **Persist direction A on first append** — if the raw branch tail ends on an assistant turn with
   unanswered calls, `append_messages` writes the synthesized results before the new batch. One-way;
   afterwards the tail ends on results. `toolName` comes from the dangling call in the assistant
   entry (no `tool_name_by_call_id` map on a fresh load). Direction B stays memory-only.

## Plumbing

- `LoggingHook` gains `session: Option<Arc<Session>>` (`build.rs:593`) and calls `commit()` in
  `on_completion_call`, with the `edit_anchors` ledger it already reads (`build.rs:723`).
- `CodeAgent::prompt`: `session: Option<&Session>` → `Option<Arc<Session>>` (interactive holds the
  `Arc`; `run_once` and tests pass `None`).
- Lock discipline: `prepare` under the guard, the owned batch outside it, re-lock to advance. Hook
  I/O now sits on the dispatch path — one window a turn, waiting for a `write_all`+flush.

## Edge cases

- Prompt dedup across retries/steers: unchanged count machinery; error/cancel arms keep their gate
  as a re-check (covers a dispatch append that failed on I/O).
- A first call that fails immediately leaves two adjacent user prompts; the lenient merge handles
  it, and today's error path already produces this shape.
- `summarize` / compaction build no hook → cannot write (test). `!` capture and metadata writers run
  at the REPL level; in-turn submits are blocked (`src/agent/pumps.rs:73`).
- `run_once`: sessionless, never persists; the transcript still accumulates for `has_progress`.
- `ModelTurnRetried` and `replace_accepted_turn_choice` are unreachable (prior's hook only returns
  Continue/Stop); the latter is the one case direction B cannot detect.
- Minimizer anchors are filled in `on_outcome` before the results project, so `prepare()` has them.
- Per-turn partials also fix a pre-existing wart: `output`/`reasoning_buffer` accumulated across
  calls, so an `Esc` re-persisted already-finished turns' text.

## Test plan

1. `prepare()` order (minimize → canonicalize → split → synthesize → push); the open unit is never
   visible in `messages`; `has_progress` true on an open unit alone.
2. Retry: failed append, then `commit()` again → same ids, one turn in `/tree`.
3. `repair_pairing`: direction A (trailing unanswered calls), direction B (orphan results in an open
   unit and on a loaded branch), idempotence.
4. Scripted provider (rig test-utils) with a hallucinated tool name → run `prompt()` → the retry's
   hook persists the synthesized assistant + feedback result as a pair.
5. Dispatch persist + retry/steer dedup (adapt `agent.rs` turn-persistence tests).
6. Edit split: multi-file call success → N/N; multi-file call with a text error result → 1/1;
   interrupted multi-file call synthesized → 1/1.
7. Parity: transcript vs `FinalResponse.messages` (minus prompt) for a normal multi-tool run.
8. Torn tail: truncated last line → `build_index` skips → context valid after repair.
9. Summarize / compaction write nothing.

## Follow-ups

- Update `doc/session_storage.md` §3.3/3.4 and the `turn.rs` persistence comments with the impl.
- fsync remains absent by decision.
