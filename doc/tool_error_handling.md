# Turn Error Handling & Auto-Retry (Design)

*Status: design + implementation. Cross-checked against the pinned rig
checkout (`gwicke/rig` rev `1c2f4405`, `rig-agent`/`rig-run`/`rig-core` 0.42)
— see `Cargo.toml`. Line numbers below refer to `src/agent.rs` /
`src/lib.rs` / `src/output.rs` at the time of writing and drift; anchors are
named functions.*

This document is the design for how picocode handles failures that
interrupt an agent turn, and how it retries them without losing progress. It
replaces the earlier "tool error handling" content: the *tool-execution* error
story is settled and summarized here for reference, while the mid-turn
provider/transport failure problem is the subject of the design and of the
implementation in `src/agent.rs` / `src/lib.rs`.

## The two failure classes

1. **Tool execution errors** — a registered tool runs and fails. Rig models
   these richly (`ToolResult` / `ToolErrorKind` / `ToolExecutionError`) and
   already feeds the failure text back to the model in the same run; the model
   sees the error and can correct. Picocode's only job here is *display*
   (render the failure distinctly) and *name recovery* (`on_invalid_tool_call`).
   This class is **done** — see [Tool execution errors](#tool-execution-errors).
2. **Turn-level provider / transport errors** — the provider's HTTP response
   or SSE stream fails mid-turn: 429, 401, 5xx, network drop, malformed
   streamed tool-call JSON. These kill the whole rig run. Picocode classifies
   them (`classify_stream_error`) and the REPL auto-retries — but the current
   retry **re-runs the whole turn from the session**, losing any tool work the
   turn had already completed. This class is the **open design** — see
   [Mid-turn failure & resume](#mid-turn-failure--resume).

---

## Tool execution errors (settled)

### Rig surface (verified against rev `1c2f4405`)

```rust
// rig_agent::agent
pub struct ToolCall<'a> {
    pub tool_name: &'a str,
    pub tool_call_id: Option<&'a str>,     // provider id or rig-minted handle
    pub internal_call_id: InternalCallId,
    pub args: &'a str,                     // ALWAYS valid JSON (re-serialized Value)
}
pub struct ToolResultEvent<'a> {
    pub tool_name: &'a str,
    pub tool_call_id: Option<&'a str>,
    pub internal_call_id: InternalCallId,
    pub args: &'a str,
    pub presentation: &'a ToolOutput,      // running presentation rewrite
    pub raw_result: &'a ToolResult,        // rig_core::tool::ToolResult — structured
    pub tool_context: &'a ToolContext,
}
pub enum ToolCallAction { Run, Rewrite(serde_json::Value), Skip(String), Stop(String) }
pub enum ToolResultAction { Keep, Rewrite(ToolOutput), Stop(String) }
```

- `ToolCall.args` is always syntactically valid JSON: rig parses the provider
  payload into `ToolFunction.arguments: serde_json::Value` and re-serializes
  it. The `.unwrap_or(Value::String(..))` fallback in picocode's
  `on_tool_call` is dead display code — **kept** as a future-proofing shim,
  never changed.
- Malformed *typed* args (wrong types/missing fields) become
  `ToolErrorKind::InvalidArgs` inside rig dispatch → failed `ToolResult` →
  model sees the error text in the same run and can correct. The model always
  learns about failed tool calls.
- No picocode-local `ToolErrorKind` exists; rig's
  `InvalidArgs | Timeout | Cancelled | NotFound | PermissionDenied |
  RateLimited | Provider | Network | Other` is the taxonomy.

### Implemented in picocode

- **Display (Task 1).** `Output::display_tool_error(tool_name, kind, message)`
  added with a default impl → `display_error`; `ConsoleOutput` overrides it to
  render a panel using the existing `paint_panel`/`tool_panel(is_error=true)`
  helpers. `LoggingHook::on_tool_result` (`src/agent.rs`) branches on
  disposition: `is_error()` → error panel; `is_refused()` → "denied" panel;
  success/skipped → `display_tool_result`. The legacy `"Toolset error:"` /
  `"ToolCallError:"` sniffing in `display_tool_result` is **kept as a compat
  shim** for replaying pre-0.40 sessions (no-op for new runs).
- **Name recovery (Task 2).** `LoggingHook::on_invalid_tool_call`
  (`src/agent.rs`) returns `InvalidToolCallAction::retry(available-tools
  feedback)` when a model calls an unknown/disallowed tool; the streaming
  request sets `.max_invalid_tool_call_retries(1)`. The model recovers
  in-run. On budget exhaustion, `classify_stream_error` maps the terminal
  `PromptError::UnknownToolCall` to `PicocodeError::LlmUnknownTool` (escape
  hatch, `is_auto_retryable() == false`).

---

## Mid-turn failure & resume (implemented)

### The problem with the current retry

The current layer-2 design (previous version of this doc) has a **fatal
flaw** for mid-turn failures: it treats a retry as "re-run the whole prompt
from the session." Concretely:

1. `prompt(input)` builds `prior` from `s.build_context()` and runs the full
   agentic turn (model call → tool calls → tool results → …).
2. A 429 can hit at **any** model call inside that turn — after several tool
   executions have already completed and produced results.
3. When that happens, rig surfaces a `StreamingError` with **no transcript**:
   only `PromptCancelled`, `MaxTurnsError`, and `UnknownToolCall` carry
   `chat_history` (`rig-run/src/error.rs`). The partial turn's
   `new_messages` (assistant tool calls + tool results) die with the run.
4. The REPL retry re-invokes `prompt(input)`. `prior` now comes from the
   session, which has **no record of the partial turn** — so the retry
   restarts from the very first model call and **re-executes every tool call**
   the failed attempt already completed.
5. On an overloaded service (which is *why* we got a 429), re-running the
   whole turn from scratch is the worst possible strategy: it burns the
   provider budget and the user's time, and is likely to hit the same wall
   again *after* redoing all the tool work.

Worse, the earlier "fix" for duplication (skip intermediate persistence on
auto-retryable errors) means the session has **nothing** from the failed turn;
if the retry budget is then exhausted, the user's entire turn is lost —
including completed tool results that should have been preserved.

### Requirements

- R1 — **Preserve progress.** Completed model turns (text + reasoning) and
  completed tool-call→result pairs that happened before the failure must
  survive into the session, so the model on the retry knows what it already
  did and does not redo it.
- R2 — **Resume, don't restart.** The retry must continue from the last safe
  transcript boundary, not replay the whole turn from the user's original
  input.
- R3 — **No duplication.** The user's input, the partial transcript, and the
  retry's steering message must each appear exactly once in the session.
- R4 — **All agent output persists.** Cancelled turns and failed turns get the
  same treatment: whatever completed safely is committed to the session. (This
  is already the direction of the `Esc` cancel path.)
- R5 — **Retry budget with back-off for 429.** Raise the budget to ~20
  attempts per *logical turn* (not per session), exponential back-off honoring
  `Retry-After`, cancel-safe, and a final give-up error that says how long we
  waited.
- R6 — **Generalize.** The same mechanism must cover malformed-tool-call
  recovery, not just 429s.

### Rejected alternative: transport-level retry

Retrying at the HTTP/SSE transport layer (wrap the client so a 429/5xx is
retried before rig sees it) would keep rig's `AgentRun` alive and its
`new_messages` intact — "preserve progress by construction." Rejected because:

- It does not generalize: malformed streamed tool-call JSON is a *parser*
  error, not a transport error; tool-call errors live at the run layer. A
  transport retry can't fix or even see them.
- It fights rig's design: rig deliberately treats a rejected HTTP response as
  terminal (`http_client/sse.rs`: "A rejected response is terminal: the retry
  policy governs transport failures, not a server that answered").
- It hides policy (when to stop retrying, what to tell the user) in a layer
  that can't render or steer.

### Chosen architecture: picocode-side canonical transcript + resume-by-context

The session is already the context carrier — `build_context()` feeds rig's
`.history()`, compaction and session-resume already seed context from it, and
the `on_completion_call` hook can patch the exact history a request sends
(`RequestPatch::history`, already used for reasoning stripping). So the design
is:

**Accumulate a canonical transcript of the run in picocode as it streams;
on a mid-turn failure, persist the transcript up to the last *safe* boundary;
retry by continuing from that boundary — with a steering message — rather than
re-sending the original input.**

This mirrors exactly what rig does internally when it resumes a run or
continues after tool results: the next completion request is
`history = prior (session + partial transcript)`, `prompt = steering message`.

### What picocode can already observe (the accumulation primitives)

Verified against the stream surface:

- **The user prompt** (`input`) is the run's first message
  (`new_messages[0]` in rig-run).
- **Every accepted model turn**, including tool-only and reasoning-only turns,
  fires `LoggingHook::on_model_turn_finished` with canonical
  `event.content: &Vec<AssistantContent>` (text, reasoning, tool calls) —
  `src/agent.rs`. This is the same content rig parks in
  `Message::Assistant { content }`.
- **Completed tool-result batches** arrive as stream items after rig has
  committed them to `new_messages` (`run.tool_results(...)` is called *before*
  the items are yielded): `MultiTurnStreamItem::ToolExecutionCommitted` then
  `StreamUserItem(StreamedUserContent::ToolResult { tool_result,
  internal_call_id })`. Each becomes part of a `Message::User` tool-result
  message (rig-run `run.rs` pushes the batch as one `Message::User`).
- **Final success** yields `MultiTurnStreamItem::FinalResponse(resp)` whose
  `resp.messages()` is the *complete* run transcript (`new_messages`):
  `[user(input), assistant(turn1), user(tool-results-1), assistant(turn2), …]`.
  This is the ground truth the accumulator must match on the happy path —
  an excellent cross-check for tests.
- **The error path** yields `Err(StreamingError)` with no transcript. The
  accumulator is what survives.

### The transcript accumulator

Introduce a small run-scoped struct, e.g. `TurnTranscript`:

```rust
/// Canonical messages of the in-flight run, mirroring rig's `new_messages`
/// shape: [user(input), assistant, user(tool-results), assistant, …].
#[derive(Default)]
struct TurnTranscript {
    /// Completed messages in canonical order (safe to persist).
    committed: Vec<Message>,
    /// Whether any model turn has fully completed (distinguishes
    /// "429 before any progress" from "429 mid-turn").
    has_progress: bool,
    /// Call ids of the most recent assistant turn's tool calls that are still
    /// awaiting results (for synthesis at finalize).
    pending_tool_calls: BTreeSet<String>,
}
```

Sources:
- `input` → seed `committed[0] = Message::user(input)` only when the run
  actually starts (first model call begins); do not pre-seed.
- `on_model_turn_finished` → append `Message::Assistant { content }`;
  record `pending_tool_calls` from any `ToolCall` parts.
- Tool-result stream items → group consecutive `ToolResult`s into the
  pending batch; when the batch is complete (all `pending_tool_calls`
  answered, or a `ToolExecutionCommitted`+`StreamUserItem` group closes),
  append one `Message::User { content: [ToolResult…] }` and clear
  `pending_tool_calls`.

Because hooks run inside rig's `prompt` machinery and the stream loop lives in
picocode's `prompt()`, the accumulator is shared via `Arc<Mutex<…>>` cloned
into the `LoggingHook` instance that `prompt()` itself constructs
(`src/agent.rs`, the `stream_prompt(...).add_hook(LoggingHook { … })` site) —
no cross-run state, no new agent fields.

### Safe-boundary finalization on failure (R1, G1/G8) — no truncation

When a retryable error interrupts the run, decide what to persist. The rule is
**never drop or truncate progress** — an interrupted state that is unsafe to
persist verbatim is *converted into text* instead, so the user and the model
see everything that happened:

- **No completed model turn** (the 429 hit the *first* model call, or the
  malformed-tool-call error hit while the first turn was still assembling):
  there is no partial transcript to preserve. Persist **nothing**; the retry
  re-sends the original `input` as the prompt. (This is the current behavior
  and is correct — see R3.)
- **Safe boundary** (ends on an assistant message with no unanswered tool
  calls, or on a user tool-result message): persist `committed` as-is.
- **Trailing assistant turn with tool calls that never got results**: keep
  the whole assistant message (text + reasoning + tool calls) and **synthesize
  a text tool result for every unanswered call**, exactly like rig's own
  `invalid_tool_retry_user_message` (`rig-run/src/transcript.rs`): each
  unanswered `ToolCall` becomes a `Message::User` containing
  `tool_result_message(call, provider, name, "…interrupted; not executed…")`.
  The transcript stays canonical (`validate_canonical` passes: every
  assistant tool call is answered by a tool result in the very next message),
  and the model sees precisely what it planned to do and that the calls were
  interrupted — no loss, no truncation.
- **In-flight pure-text turn** (text was streaming when the error hit, turn
  never accepted by rig): persist the partial text as a text-only assistant
  message. This is safe (no tool calls) and preserves the reasoning/text
  progress — matching the `Esc` cancel path's existing text preservation.

This mirrors the `Esc` cancel path's existing text preservation and extends it:
completed tool batches are kept (they are `Message::User` results answering
earlier assistant turns), and an *unanswered trailing* tool turn is kept too —
its calls are answered with synthesized text results instead of being dropped.

### Where persistence happens and how the retry continues (R2/R3)

`prompt()` currently has two persistence sites: the `Some(Err)` branch
(Phase 2a, `src/agent.rs`) and the success tail (`final_history` /
`cancelled_history`). The new design:

1. **On a retryable mid-turn error with progress**: in the `Some(Err)`
   branch, run the finalization above and `append_message` each message of the
   finalized `committed` to the session **once**. Then return the classified
   error, but tag it so the REPL knows progress was persisted and a
   *continuation* is required (not a fresh `input` re-send).
2. **On a retryable error with no progress**: persist nothing; the REPL
   re-sends `input` (current behavior).
3. **The REPL retry loop** (`src/agent.rs`, `run_interactive`): for the
   "progress persisted" case, re-prompt with a *steering/continuation* message
   instead of `input` — the short steer `"continue"` (or the synthetic
   malformed-tool-call feedback). The original `input` is already in the
   session as `committed[0]`, so it is not duplicated; the steering message is
   the new prompt and becomes the next turn's user message on success.
4. **Malformed-tool-call recovery** unifies: the error typically arrives with
   **no** completed turn (the assembler failed mid-first-turn), so nothing is
   persisted; the synthetic feedback message (see `synthetic_retry_feedback`)
   already carries the correction. If the malformed call happened on a *later*
   turn (some progress existed), that progress is persisted per the rule above
   and the feedback becomes the continuation prompt.
5. **Success tail stays as-is**: when a retry finally succeeds,
   `resp.messages()` is the *retry run's* transcript `[user(steering),
   assistant(…)]`; it is appended after the persisted partial — yielding the
   correct full history `[user(input), assistant(partial)…, user(steering),
   assistant(final)…]` with no duplication.

### Steering message vs. re-sending input (why not re-send)

Re-sending `input` after partial progress would duplicate `committed[0]` (the
input is already in the session from step 1) *and* restate a request the model
already worked on. A steering message (the short "continue", or the
synthetic parse-error feedback) is the correct continuation token: it is a new
user message that points at the existing context without repeating it. This is
the same pattern rig uses for `InvalidToolCallAction::Retry { feedback }` — the
feedback is appended as a user message and the model continues from there.

### Consecutive-user and alternation constraint (G4 — verify empirically)

Rig's canonical model **permits consecutive user messages**
(`validate_canonical` only rejects assistant-assistant and unanswered tool
calls), so `[user(input), user(steering)]` is valid to *rig*. But some provider
adapters (notably Anthropic's legacy API, which requires strict
user↔assistant alternation) may reject `user,user` on the wire. The Anthropic
adapter in the pinned rev is a 1:1 message conversion with no visible
merge-consecutive-users pass, so this is a **real, unverified risk**.

Mitigation options to validate during implementation:
- If `user,user` is rejected: when the partial transcript ends on a `user`
  tool-result message, *merge* the steering text into that same
  `Message::User` (append a text item alongside the tool results) instead of
  sending a separate user prompt. This is what rig itself does when it appends
  a tool batch to a trailing user message (`run.rs:1532`).
- If accepted: keep the steering message as its own prompt.

The safe default is to **merge when the transcript ends on a user message** —
which is precisely the case after a completed tool batch (the common
mid-turn-failure shape). A pure `prompt(input)` after no progress ends on an
assistant boundary, so the normal first-turn retry is unaffected.

### Cancel (`Esc`) unifies with the same path

The existing `Esc` branch (`src/agent.rs`) persists `user(input)` + text-only
partial. Under this design it becomes: finalize the accumulator (synthesizing
answers for any unanswered trailing tool calls) and persist. This satisfies
R4 ("all agent output should be persisted") and upgrades the current
text-only fallback to keep completed tool work on cancellation too. The only
difference from the retryable-error path: `Esc` does **not** auto-retry — it
persists and returns `Cancelled`.

### Retry budget & back-off (R5)

Current REPL loop is single-shot (`attempt == 1`). New policy, per **logical
turn** (the counter lives in the REPL's per-prompt `loop`, so a fresh user
prompt resets it — never session-global):

- Budget: **up to 50 retries**, applied **uniformly** to every auto-retryable
  class — `LlmRateLimited`, `LlmUpstream` (5xx), `LlmTransport`,
  `LlmStreamProtocol`, and `LlmMalformedToolCall`. They are all transient
  failures that should clear with a retry; the budget exists to cap a runaway,
  not to second-guess the policy per class. Non-retryable classes
  (`LlmAuth`, `LlmBilling`, `LlmClient` 4xx, `LlmMaxTurns`, `LlmUnknownTool`)
  get 0 retries. Post-MVP: expose via `AgentConfig`.
- Delay: pure exponential growth, **no upper bound**. Effective wait =
  `2^(consecutive_failures - 1)` seconds. When the provider reports
  `Retry-After` (or OpenRouter's `metadata.retry_after_seconds`), it is the
  **floor of the first attempt** and the base of the exponential schedule
  (`hint`, `hint*2`, `hint*4`, …). The per-turn budget (50) is the implicit
  cap on how many attempts we will ever make, so the delay growth is bounded
  in practice.
- **Consecutive-failure reset**: the `consecutive_failures` counter resets to
  0 whenever the underlying prompt made progress (the agent returned more
  content or tool calls) before failing. That is, a failure that happened
  *after* a successful sub-run inside the same turn restarts the back-off at
  `2^0 = 1` s (or the provider's `Retry-After`). This way, two consecutive
  failures with a successful retry in between are not punished with
  compounding delays.
- `cancellable_sleep` (existing, `src/agent.rs`) keeps each wait `<Esc>`-
  abortable; the in-flight reader spans the whole budget window.
- Give-up: after the budget, surface a final error naming the class and the
  **total time waited** so the user can decide whether to keep going or switch
  providers. Because progress was persisted (R1), give-up is lossless — the
  partial turn is in the session and the user's next prompt continues from it.

### Lost-context guard: always persist `user(input)` on first failure

A pre-fix bug: when `prompt()` failed on a retryable error with **no
progress** (e.g. a 429 on the first model call of a turn, before any
assistant turn or streamed text had been produced), nothing was persisted
to the session. If the REPL exhausted the retry budget (50 attempts) on
the same transient condition, the user's turn was lost entirely — the
session had no record of the user prompt, and a follow-up "continue"
prompt had zero context. The user's actual incident:

> "the *full turn context* was lost, including the user prompt. When
> asking the model to continue with a simple "continue" prompt, it had
> zero context from the previous failed turn."

The fix (R7) is symmetric and de-duped with an in-memory per-turn guard:

- **`TurnPersistence`** (`src/agent.rs`): the REPL owns one instance per
  user prompt and reuses it across retries. It tracks **how many NEW user
  messages have been dispatched this turn** (`turns_sent`: the user's
  original prompt plus each steering/continuation directive) and captures a
  **base frame** (`base_session_count`: the session's user-prompt count at
  this turn's first `prompt()` entry). This is entirely **text-independent**.
- **Session is the persistence source of truth**
  (`Session::count_persisted_user_prompts`): walks the active branch and
  counts `role: "user"` entries (tool results are `role: "toolResult"` and
  are never counted). The decision in `prompt()` computes
  `this_turn_persisted = current_session_count − base` and compares it to
  `turns_sent`:
  1. **this_turn_persisted < turns_sent** → this turn's latest dispatch is
     not yet persisted → persist `user(input)` (first failure, or a steering
     dispatch whose prompt hasn't landed yet).
  2. **this_turn_persisted >= turns_sent** → it's already persisted (a
     resend of a prior attempt) → skip, and on success drop the leading
     `user(input)` from `final_history`.
- This is correct across turn boundaries because the *base* is subtracted:
  two identical inputs in successive turns (e.g. "continue" twice) get
  distinct base frames, so turn 2's `this_turn_persisted` starts at 0 and
  its "continue" is persisted as fresh. A steering "continue" within one
  turn is a distinct dispatch (`turns_sent` incremented), so it is never
  elided against the original. No text comparison anywhere.
- **REPL retry loop**: `turn.mark_new_dispatch()` is called for the initial
  prompt and for each steering/continuation directive; a plain retry that
  re-sends an identical prompt does NOT bump the count. The removed manual
  `s.append_message(&Message::user(input))` for synthetic feedback stays
  owned by `prompt()`'s de-dup logic.

Net effect: the user prompt is always in the session after the first
failure of a turn. The 50-attempt retry budget can be exhausted without
losing the user's input — give-up is lossless in all cases, not just
when progress was made. Because the decision reads the session, a REPL
counter drift is harmless: the outcome always reconciles to what is
really persisted.

### Malformed-tool-call (R6) — already covered

`LlmMalformedToolCall` classification, `synthetic_retry_feedback`, and the
REPL injection exist today (`src/lib.rs`, `src/agent.rs`). Under the new
design the only change is that when it occurs after partial progress, the
partial transcript is persisted first (finalization rule) and the feedback is
the continuation prompt; when it occurs on the first turn, behavior is
unchanged.

---

## Implementation plan

### Phase 1 — Accumulator (`TurnTranscript`) + hook wiring

1. Add `TurnTranscript` (fields as above) + `Arc<Mutex<TurnTranscript>>` into
   `CodeAgent::prompt` and clone it into the `LoggingHook` instance.
2. `LoggingHook::on_model_turn_finished`: append
   `Message::Assistant { content }` and record pending tool-call ids.
3. `LoggingHook::on_tool_result`: accumulate the result; when the batch
   closes, append `Message::User { [ToolResult…] }` and clear pending ids.
   (Batch close detection: `on_tool_result` fires per tool; a batch closes
   when the next model turn begins or the run ends. The `ToolExecutionCommitted`
   / `StreamUserItem` items in the stream loop can also drive this — pick one
   source of truth and unit-test it.)
4. On `FinalResponse`, optionally assert the accumulator matches
   `resp.messages()` (debug cross-check; drop in release).

### Phase 2 — Truncation + persistence on failure

5. Implement `fn finalize(t: &TurnTranscript) -> Vec<Message>`
   implementing the rules in *Safe-boundary finalization* (pure function,
   unit-testable; synthesizes text tool results for unanswered trailing calls,
   never truncates).
6. In `prompt()`'s `Some(Err)` branch: compute `classified`; if
   `classified.is_auto_retryable()`:
   - if `transcript.has_progress` → append finalized `committed` to session;
     return a variant/code that tells the REPL "progress persisted → steer".
   - else → persist nothing (current behavior); return the classified error.
   Non-retryable errors keep the existing Phase 2a persistence
   (`user(input)` + text partial) so the user's turn is never lost on give-up.
7. Cancel branch: persist the finalized transcript (now including completed
tool batches and synthesized answers for any interrupted calls) instead of the
text-only partial.

### Phase 3 — REPL retry loop upgrade

8. In `run_interactive`, replace the `attempt == 1` guard with the
   per-turn budget policy (R5). Distinguish the two retry shapes via the error:
   - no progress → re-send `input`;
   - progress persisted → re-prompt with the steering message
     (merge-into-trailing-user when the transcript ends on a user message —
     see *Consecutive-user*).
9. Track total elapsed; on give-up, display the final error + total wait.
10. Add `retry_budget: u8` to `AgentConfig` (default 50, currently hardcoded)
    and thread through `create_agent` / `run_interactive`.

### Phase 4 — Tests & cross-checks

11. Unit: finalization rules (each boundary case — synthesis of unanswered
    trailing calls, no data loss), accumulator ordering vs.
    `resp.messages()` on the happy path (scripted model), steering-message
    shape.
12. Integration (scripted model + injected error): mid-turn 429 after a tool
    batch → assert the partial transcript is in the session, the retry prompt
    is the steering message (not `input`), and the model's next call sees the
    tool results in history.
13. Integration: 429 before any progress → retry re-sends `input`, no
    duplication in the final session.
14. Regression: first-turn malformed tool call still recovers via synthetic
    feedback with no session duplication.
15. Regression: `Esc` mid-turn persists completed tool batches (upgraded from
    text-only) and returns `Cancelled`.
16. Regression: all existing classification + display tests pass unchanged.

### Open questions to resolve during implementation

- **Q1 (alternation):** Does the Anthropic wire reject `user,user`?
  Empirically test with the real adapter; decide merge-vs-separate steering
  accordingly. (Default: merge when transcript ends on a user message.)
- **Q2 (batch-close source):** Drive tool-result batching from
  `on_tool_result` (hook) or from the `StreamUserItem` items (stream loop)?
  Pick one source of truth; the other cross-checks in tests.
- **Q3 (partial mixed turn):** For an in-flight turn with both text and tool
  calls that never resolved, the whole assistant message is kept and the
  unanswered calls are answered with synthesized text results (no truncation,
  no split). Confirmed as the design default; no open question remains.
- **Q4 (steering text):** Exact wording of the 429 continuation message, and
  whether it should echo the original request id/context. Default text above.

---

## Testing strategy (summary)

As in *Phase 4* above, plus the settled layer-1 tests (display panels, name
recovery) which exist and stay. Existing classification tests
(`classify_*`), `retry_after_secs_extracted_from_provider_response_headers`,
and `classify_prompt_error_429_preserves_retry_after` continue to pin the
taxonomy and the `Retry-After` extraction.

## Risk assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| `user,user` rejected by a provider adapter (esp. Anthropic) | High if present | Merge steering into trailing `Message::User` when transcript ends on a user message (the common mid-turn shape); verify empirically (Q1) |
| Accumulator diverges from rig's `new_messages` shape | Medium | Cross-check against `resp.messages()` on the happy path (Phase 1.4); validate with `rig_run`'s `validate_canonical` in tests |
| Unanswered trailing tool-call turn persisted → invalid history | Medium | Finalization synthesizes a text tool result per unanswered call (G1), so the transcript stays canonical (`validate_canonical` passes); no truncation |
| Duplicate `user(input)` on retry | Medium | Persist-once rule: no progress → no persist + re-send; progress → persist finalized + steering prompt (never re-send `input`) |
| 429 budget burns `max_turns` | Medium | Per-turn counter only; each retry is a fresh `prompt()` consuming 1 turn — document that a 20-retry burst may exhaust a small `tool_call_limit`; keep budget configurable |
| Long back-off with no user feedback | Low | REPL already prints `Retrying in Ns…`; add cumulative total on give-up |
| Regression: replay of old sessions | Low | Compat shim kept in `display_tool_result`; no session-format change (messages persist via existing `append_message`) |

## Acceptance criteria

1. ✅ Failed/refused/skipped tool results render distinctly (settled).
2. ✅ Unknown-tool calls recover in-run via `on_invalid_tool_call` (settled).
3. ✅ Rate-limit/auth/network/5xx errors classified, not `Other` (settled).
4. ✅ Mid-turn failure persists completed model turns and tool-call→result
   pairs to the session (`TurnTranscript` + `prompt()` error branch) (R1).
5. ✅ Retry after mid-turn failure continues from the persisted boundary via a
   steering message — the model sees prior tool results and does not redo them
   (`LlmInterrupted` + REPL steer) (R2).
6. ✅ No duplication of the user input, the partial transcript, or the
   steering/feedback message across retries and final success (persist-once +
   steer-once guard) (R3).
7. ✅ `Esc` persists completed tool batches, not just text (cancel branch uses
   the same finalized transcript) (R4).
8. ✅ Unified retry budget: hardcoded at **50 per logical turn** for every
   auto-retryable class (`LlmRateLimited`, `LlmUpstream`, `LlmTransport`,
   `LlmStreamProtocol`, `LlmMalformedToolCall`) via `retry_budget()`, with
   `2^(consecutive_failures - 1)` second back-off (no upper bound — the
   budget is the implicit cap), provider `Retry-After` as the floor, and
   `<Esc>`-abortable waits. The `consecutive_failures` counter resets to 0
   on a `prompt()` that made progress (so a failure *after* a successful
   sub-run inside the same turn restarts at 1 s). Remaining: expose the
   budget as an `AgentConfig` field (default 50) and report the total wait
   time on give-up.
9. ☐ Malformed-tool-call recovery shares the persist-and-steer machinery
   where progress exists (R6). Unknown-tool recovery remains owned by the rig
   hook (`on_invalid_tool_call`), which is correct — it recovers in-run
   without the REPL layer.
10. ✅ Lost-context guard (R7): `user(input)` is persisted on the FIRST
    failure of a turn regardless of retryability or progress, de-duped
    via the per-turn `TurnPersistence` record (count-based; never
    conflates identical inputs across turns — the session is the source
    of truth). A retryable failure with
    no progress (e.g. a sustained 429 on the very first model call) no
    longer loses the turn when the retry budget is exhausted.
11. ✅ OpenRouter 429 mid-stream JSON envelope (the `ProviderResponseError`
    shape with `error.code: 429` and `error_type: "rate_limit_exceeded"`)
    classifies as `LlmRateLimited`, so the `Retry-After`-floor + 50-attempt
    budget applies. Same for 5xx codes in the envelope (`LlmUpstream`).
12. ✅ Existing classification + display + name-recovery tests pass (335 lib
    tests; +3 new tests covering the OpenRouter 429/502 mid-stream JSON
    envelope classification and the no-code fall-through to
    `LlmStreamProtocol`, plus 3 tests pinning the count-based
    `TurnPersistence` dispatch semantics and the session
    `count_persisted_user_prompts` source of truth).

## File changes (projected)

- `src/agent.rs` — `TurnTranscript`, `LoggingHook` wiring, finalize fn,
  `Some(Err)`/cancel persistence changes (de-duped via the count-based
  `TurnPersistence` record over the session source-of-truth),
  REPL retry-loop budget + steering (short
  "continue" message), OpenRouter mid-stream JSON envelope detection in
  `detect_response_error`, tests.
- `src/session/session.rs` — new
  `Session::count_persisted_user_prompts()` used as the persistence
  source-of-truth for the retry de-dup guard, tests.
- `src/lib.rs` — `PicocodeError` steering signal (new variant or flag) if the
  REPL needs to distinguish "no progress" from "progress persisted";
  `AgentConfig` re-export unchanged.
- `src/output.rs` — unchanged (display already done).
- `doc/tool_error_handling.md` — this document.

---

*Supersedes the earlier "tool error handling" drafts. The tool-execution half
is settled; the mid-turn failure half is the design above — accumulate a
canonical transcript, persist at the last safe boundary on failure, and resume
by steering from that boundary instead of restarting the turn.*
