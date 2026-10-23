# Incremental session writes — persist each completion turn, not just the whole turn

Status: design, pre-impl.

Decisions confirmed:

1. `TurnTranscript` is the persistence source of truth (it is append-only, so it needs no
   reconciliation machinery). rig's committed history is used only as a *test oracle*.
2. A completion turn is appended as **one batch**: one lock window, one split decision shared by the
   call and result halves, one leaf entry, stable entry ids so a retry is idempotent.
3. Persist at the start of each completion call, plus at dispatch. End-of-turn persistence shrinks
   to the in-flight response and the partial content of an interrupted turn.

## Problem

Session JSONL persists only when a turn *ends*, fails, or is aborted (`src/agent/turn.rs:656`).
During a multi-tool-call turn — minutes long — assistant turns, calls, results and reasoning live
only in memory (`TurnTranscript`, `output`, `reasoning_buffer`): grepping the file for a completed
completion call's output finds nothing, prompt included, and a mid-turn process death loses the
whole turn — even tool work already written to disk.

## Goals / non-goals

- The user prompt on disk at dispatch, before the first request is sent.
- Each completion turn's `[assistant + its toolResults]` unit on disk by the start of the next
  completion call; the on-disk tail is always a *valid prefix* (every persisted call answered).
- Everything the model produced captured on any exit — `Esc`, tool-dispatch failure, provider error
  mid-stream — through one uniform path.
- Crash residue (a torn batch) repaired by one pure helper shared by write and load paths.

Non-goals: no fsync; no format change; no per-fragment writes; `summarize` /
`summarize_with_session_context` / compaction requests stay unpersisted (they build a request
directly and add no `LoggingHook` — pinned by a test).

## Source of truth: the transcript, and why it needs no reconciliation

`TurnTranscript` (`src/agent/transcript.rs:129`) is **append-only**: `push_assistant_turn`,
`push_tool_result`, `flush_results` and `finalize` only push to `committed`, so a cursor over it is
trivially stable. rig's committed history is *not*: `abandon_streamed_turn` rolls back a turn,
`Repair` substitutes a call name, `projection_start` re-reports a resumed batch. Sourcing deltas
from it would need assistant-boundary grouping, a `+1` for the excluded prompt, an `input_len`
offset and a skip-at-turn-1 rule — all costs of reconciling a rewriting source, and all absent if we
do not switch. We do not; rig's history is a **test oracle** (parity test).

> Invariant: the cursor covers a prefix of **paired units** (an assistant turn plus its tool
> results) and never advances past a unit whose results are not yet flushed. The uncommitted tail —
> the in-flight turn, the not-yet-flushed results of the last completed turn — is never covered, and
> may be repaired by synthesis on an interrupted exit.

Progress tracking is thus at commit points only — the file's granularity, both mirroring
`committed`. The transcript keeps two jobs the file cannot: it holds the uncommitted tail, and it
works with no session at all (`has_progress` drives the `LlmInterrupted` back-off reset for
`run_once` too). The only real divergence is *curation* — the transcript drops invisible turns
(`assistant_content_is_visible`, `transcript.rs:229`) where rig may keep a differently-shaped empty
turn — a test-time cost, not a runtime one.

## Persist points

1. **Dispatch** — early in `prompt()`, after `session_context_messages(prior)` is read (so the
   prompt is not in its own `prior`): append `Message::user(input)`, gated by the existing
   `TurnPersistence` count check (`turn.this_turn_persisted(c) < turn.sent()`, `src/agent.rs:205`),
   so retry/steer dedup is unchanged.
2. **`LoggingHook::on_completion_call`** (`src/agent/build.rs:697`) — persist the delta *before* the
   `user_cancel` Stop check, so a cancel-stop still writes the turn that just completed. The
   `CompletionCall` *stream item* gets no arm: there the just-finished turn has not even committed
   to the transcript, so its delta is empty or one call late.
3. **Loop exit (normal)** — the remaining delta: the final assistant turn, which gets no following
   completion call.
4. **Every interrupted exit** — the *same* call as (3) with trailing synthesis enabled; see "Uniform
   capture".

**Bug to fix in passing**: `turn.rs:568` builds `partial = vec![Message::user(input)]`
unconditionally, while the sibling arm at `turn.rs:381` gates on `session_has_prompt`. With dispatch
persistence the prompt is always already on disk, so this arm must use the same gate or every `Esc`
after a retry duplicates it.

## The delta cursor

- `partial_text` / `partial_reasoning: String` — the in-flight turn's streamed content, appended as
  the loop renders (`turn.rs:264,273`) and cleared by `push_assistant_turn`, since `event.content`
  is the canonical finalized turn and supersedes the fragments (an *invisible* turn must not linger
  either). `output` stays cumulative for `prompt()`'s return value; the partials are the only thing
  persisted from the tail.
- `cursor: usize` — index into `committed` of the first unpinned message.
- `prepare_delta(synthesize: bool) -> Vec<Message>` — flush `pending_results` (idempotent), run the
  shared repair on the tail window, then return `committed[cursor..]`, the partial assistant
  (`assistant_message(None, …)` — honest `responseId: None`) if non-empty, and — if `synthesize` —
  the "did not execute" results for a trailing unanswered turn. Never advances the cursor or clears
  the partial, so a failed append is retried whole.
- `commit_delta(synthesized: bool)` — only after the batch append succeeds: advance `cursor`, clear
  the partial, and push any synthesized results into `committed`, so a later boundary call does not
  re-synthesize them.

The boundary point passes `synthesize = false`: at a call start the previous turn's results are
necessarily in hand, so a trailing unanswered call there is an invariant violation — log it; never
write "did not execute" for a call about to run.

**Repairs never invalidate the cursor**: direction A appends past the last committed message,
direction B inserts an assistant just before the freshly flushed results (index `cursor` or later —
those results were never pinned). Nothing lands before the cursor or is removed behind it, so its
prefix stays fixed and valid. `finalize()`'s write-path role is retired; its synthesis moves into
the shared repair.

## Uniform capture: one repair shape for every exit

Every exit persists through *one* helper, `persist_delta(session, transcript, anchors, synthesize)`,
which also logs append failures — today every site is a `let _ = …`, and with ~25 per turn a
disk-full must not vanish silently (warn and continue, the `MessageLogger` precedent at
`build.rs:765`). What each exit carries:

- **normal / `FinalResponse`** — the final assistant turn.
- **`<Esc>` / hook Stop / `Cancelled`, provider or protocol error mid-stream, tool-dispatch failure
  (`ToolExit::Fail`)** — delta + trailing synthesis (a failing call may produce no result, which
  synthesis answers) + partial text/reasoning.
- **`MaxTurns`, `UnknownToolCall`, auto-retryable error** — delta + trailing synthesis (the last
  committed turn got no following hook); the retryable case gated by the retry/steer dedup exactly
  as today.

The partial is only ever the *current* turn, so an interrupted turn is never re-persisted as a
duplicate of a committed one. An abort *before* the first assistant turn still writes it — the old
code needed a separate `else if !output.is_empty()` arm for that, which the unified `prepare_delta`
subsumes: delta may be empty while the partial is not.

### The shared repair: two directions, one pure function

`fn repair_pairing(messages: Vec<Message>) -> Vec<Message>` (in `transcript.rs`, pure and
idempotent) fixes both shapes a non-atomic write can leave. It is the single place a dangling
pairing is resolved, called from `prepare_delta`, `build_context`, and the append-path repair:

- **Direction A — assistant with unanswered calls** (crash or abort between call and result, a
  tool-dispatch failure): append one user message of text results, one per unanswered call — "`X`
  was interrupted before execution; it did not run" — `is_error: false`, matching rig's
  `NO_RESULT_PROVIDED`. Today's `finalize` synthesis, lifted out (`transcript.rs:191`).
- **Direction B — orphan results** (a `toolResult` whose call id has no preceding call): synthesize
  the missing assistant turn *before* the results from what the result carries — `call: CallId`,
  `name: ToolName` (`transcript.rs:87`), arguments `{}`. Left by rig paths that commit without
  firing `on_model_turn_finished`:
  - **`TurnAbandoned`** — prior's `on_invalid_tool_call` returns `Retry` (`build.rs:824`), so rig
    commits a rollback pair (partial assistant + rejected call, feedback + synthetic results)
    straight to history. The retry's `on_completion_call` fires on time — *timing* is fine — but the
    transcript holds only the feedback results in `pending_results`, no assistant half. Direction B
    supplies it before the flush, so the retry's hook writes a correct pair with no special-casing.
  - **Recovered / repaired-name turns** — `Repair` renames the call in place and
    `settle_model_turn(recovered=true)` skips `on_model_turn_finished` (`engine.rs:1170`). Same
    shape; unreachable today (prior only returns `Retry`; rig's no-hook fallback is
    `Fail`/`Ignore`), covered for free.
  - Legacy files already written by today's error paths.

Because direction B is a *reordering*, it applies only where the orphan sits in the pending tail
(write path) or in memory (load path) — **never** as a file rewrite: the append path only appends
direction-A repairs, and legacy orphans are repaired in memory at context build.

## Batch append: one split decision for the pair

`Session::append_messages(&self, msgs: &[Message], seed: &Uuid) -> crate::Result<()>` (refactoring
`append_message`, `session.rs:1243`) writes all entries + **one** leaf entry in one storage-lock
window and one index update.

The split is currently buggy: `append_message` splits the call half and the result half
**independently** (`session.rs:1259-1261`). `canonicalize_edit_tool_calls` splits whenever
`per_file_blocks` parses a >1-file blob; `split_edit_tool_results` splits only when
`parse_file_results` yields >1 entries. When the blob parses but the result is a plain
argument-error string, you get N calls and one result: `t1` (`split_id` returns the original for
`i == 0`) looks answered, the derived `t1_f1nN …` do not. Providers reject that on reload, and
direction B cannot see it (it fixes orphan *results*, not orphan calls).

The batch is the fix because it sees both halves: reuse `split_edit_call_and_result`
(`transcript.rs:428`), which returns `Some` only when *both* are multi-file and the same length. One
decision per pair — both split consistently (N calls, N results, matching derived ids), or
**neither** does (one call `t1`, one result `t1`, which also fixes the `finalize`-then-split
hazard).

Ids and atomicity:

- `prepare_delta` mints one `seed`, held until `commit_delta`; each pi entry id derives from it
  deterministically (`{seed}-{i}`), so a failed batch retried with the same seed writes identical
  ids. `build_index` must skip an id already present when pushing to `order` (and `children`),
  making re-append idempotent instead of duplicating the turn in `/tree` — today a fresh
  `Uuid::now_v7()` (`session.rs:1262`) meets an unconditional `order.push` (`storage.rs:413`).
- Chain is `leaf → e1 → … → eN` with one trailing leaf at `eN`. A process death mid-write leaves at
  most a torn last line, which `build_index` already skips (`storage.rs:442`).

## Load paths

`extract_messages` (`session.rs:1733`) already flushes buffered `toolResult` entries into one
trailing `Message::User`. Two hook points:

1. **In memory, at context build** — `build_context` / `session_context_messages`
   (`compaction.rs:75`) run `repair_pairing`, so a resumed session's first request is valid before
   anything is written.
2. **Persist direction A on first append** — if the raw branch tail ends on an assistant turn with
   unanswered calls, `append_messages` writes the synthesized results before the new batch. One-way;
   afterwards the tail ends on results. `toolName` comes from the dangling call in the assistant
   entry (no `tool_name_by_call_id` map on a fresh load). Direction B stays memory-only.

## Plumbing

- `LoggingHook` gains `session: Option<Arc<Session>>` (`build.rs:593`) and does
  prepare/minimize/append/commit in `on_completion_call`, minimizing with the `edit_anchors` ledger
  it already reads (`build.rs:723`) — as today's success path does (`turn.rs:661`).
- `CodeAgent::prompt`: `session: Option<&Session>` → `Option<Arc<Session>>` (interactive already
  holds the `Arc`; `run_once` and tests pass `None`).
- Lock discipline: extract the delta, drop the transcript guard, then `.await` the writes. Hook I/O
  now sits on the dispatch path (the engine awaits the hook before sending): one lock window per
  turn, intentional and bounded.

## Edge cases

- Prompt dedup across retries/steers: unchanged count machinery; error/cancel arms keep their gate
  as a re-check (covers a dispatch append that failed on I/O).
- A first call that fails immediately leaves two adjacent user prompts; the lenient merge handles
  it, and today's error path already produces this shape.
- `summarize` / compaction build no hook → cannot write (test). `!` capture and metadata writers run
  at the REPL level; in-turn submits are blocked (`src/agent/pumps.rs:73`).
- `run_once`: sessionless, never persists; the transcript still accumulates for `has_progress`.
- `ModelTurnRetried` and `replace_accepted_turn_choice` are unreachable for prior (its hook only
  returns Continue/Stop); the latter would be the one case direction B cannot detect.
- Minimizer anchors are filled in `on_outcome` before the next `on_completion_call`, so the
  minimizer has what it needs at delta time.
- Per-turn partials also fix a pre-existing wart: `output`/`reasoning_buffer` accumulated across
  calls, so an `Esc` re-persisted already-committed turns' text.

## Test plan

1. Cursor/delta units: flush-before-slice, no double on retry (same seed → same ids), commit only on
   success, partial emitted once and cleared on commit.
2. `repair_pairing`: direction A (trailing unanswered calls), direction B (orphan results in a
   pending batch and on a loaded branch), idempotence.
3. Scripted provider (rig test-utils) with a hallucinated tool name → run `prompt()` → the retry's
   hook persists the synthesized assistant + feedback result as a pair.
4. Dispatch persist + retry/steer dedup (adapt `agent.rs` turn-persistence tests).
5. Batch append: partial batch retried with the same seed → one turn in `/tree`, no dup ids.
6. Edit split: multi-file call success → N/N; multi-file call with a text error result → 1/1;
   interrupted multi-file call synthesized → 1/1.
7. Parity: transcript vs `FinalResponse.messages` (minus prompt) for a normal multi-tool run.
8. Torn tail: truncated last line → `build_index` skips → context valid after repair.
9. Summarize / compaction write nothing.

## Follow-ups

- Update `doc/session_storage.md` §3.3/3.4 and the `turn.rs` persistence comments with the impl.
- fsync remains absent by decision.
