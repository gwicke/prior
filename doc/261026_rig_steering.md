# rig Steering / Injection Mechanism

## Overview

rig 0.45 provides steering and context injection through the **`AgentHook`** trait in
`rig_agent::agent::hook`. The primary entry point for injecting content before model calls is the
`on_completion_call` hook method.

This document covers the hook mechanism, the `AgentRun` state machine that drives turns, how retries
work (both rig-internal and external), and how Prior's architecture interacts with both.

## Core Types

### `AgentHook` Trait

```rust
pub trait AgentHook: WasmCompatSend + WasmCompatSync {
    async fn on_completion_call(
        &self,
        ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction;
    // ... other lifecycle methods (on_model_turn_finished, on_run_start, etc.)
}
```

### `CompletionCallEvent` — Input to the Hook

```rust
pub struct CompletionCallEvent<'a> {
    pub prompt: &'a Message,           // The user prompt for this turn
    pub history: &'a [Arc<Message>],   // Conversation history preceding the prompt
    pub turn: usize,                   // 1-based model-call index (initial call = 1)
}
```

> **Note**: `CompletionCallEvent` does **not** expose `additional_params` or the merged
> `RequestPatch`. Hooks only see `prompt`, `history`, and `turn`. To pass retry context to a hook,
> use `HookContext::scratchpad()` (in-memory, same run) or `HookContext::entries()` (persisted,
> survives process restart via pi session).

### `CompletionCallAction` — Hook Output

|Variant              |Effect                                                                      |
|---------------------|----------------------------------------------------------------------------|
|`Continue`           |Send the baseline request unchanged                                         |
|`Patch(RequestPatch)`|**Merge a per-turn patch** into the request (history transform, context injection, param override)|
|`Stop(String)`       |Stop the run entirely with a reason                                         |

### `RequestPatch` — What Can Be Injected

```rust
pub struct RequestPatch {
    pub preamble: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub active_tools: Option<Vec<String>>,
    pub additional_params: Option<Value>,           // Provider-specific params
    pub extra_context: Vec<Document>,               // **Appended in order**
    pub history: Option<Vec<Arc<Message>>>,         // Replaces history (last-writer-wins)
}
```

**Merge rules** (hook registration order):

- `extra_context`: **documents appended in order** → ideal for steering messages
- `additional_params`: shallow-merged (later top-level keys win)
- `active_tools`: allow-lists intersected
- Scalar fields + `history`: last-writer-wins (warning if multiple hooks set same field)

### `Document` — Steering Message Format

```rust
pub struct Document {
    pub id: String,
    pub text: String,
    pub additional_props: HashMap<String, String>,
}
```

> "A document sent with a request as context, **rendered before the chat history**."

## Execution Timing

From `HookStack` and `CompletionCallEvent` docs:

> "Per `CallModel` step, hook resolution is ordered: **completion-call hooks run first** and their
> `RequestPatch`es merge in registration order. Only when every completion-call hook proceeds does
> `ModelSelection` run... after which request preparation inspects the selected model's captured
> `ProviderCapabilities` and the attempt is issued."

**The injection happens BEFORE each completion request is sent** — i.e., before every model turn:

1. Initial user prompt → turn 1
2. After tool results → turn 2 (continuation)
3. After next tool results → turn 3
4. ...

> **Internal rig retry detail**: When `ModelTurnAction::Retry` is returned from
> `on_model_turn_finished`, the run state resets to `READY` (`PreparingRequest`). The next
> `next_step()` call increments `current_turn` (see `rig-agent/src/run/mod.rs:867`). Thus the hook
> sees an **incremented `turn` value** on retry — not the same `turn` number. The retry consumes the
> total model-call budget (`max_turns`).

## Hook Registration

Hooks are attached to a run via:

```rust
// On AgentRunner (per-run)
let runner = agent.prompt("...").add_hook(MySteeringHook);

// Or on AgentBuilder (all runs from this agent)
let agent = AgentBuilder::new(client).hook(MySteeringHook).build();
```

Multiple hooks form a `HookStack` — they run in **registration order**; their `RequestPatch`es merge
per the rules above.

## Example: Generic rig Steering Hook (Not Used by Prior)

The following shows a typical rig steering pattern using `extra_context` —
**Prior does not use this**. Prior's `LoggingHook` transforms history via `RequestPatch::history()`
instead.

```rust
use rig_agent::agent::hook::{AgentHook, CompletionCallEvent, CompletionCallAction, HookContext};
use rig_agent::run::patch::RequestPatch;
use rig_agent::completion::Document;

struct SteeringHook;

impl AgentHook for SteeringHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        // Inject on every turn after the first (i.e., after tool results)
        if event.turn > 1 {
            let steering = Document {
                id: "steering".into(),
                text: "Continue from where you left off. Prefer editing over bash.".into(),
                additional_props: HashMap::new(),
            };
            return CompletionCallAction::Patch(
                RequestPatch::new().extra_context(vec![steering])
            );
        }
        CompletionCallAction::Continue
    }
}
```

## The AgentRun State Machine

rig's execution is driven by a sans-I/O `AgentRun` state machine. Understanding it is essential for
steering behavior on retries and in-memory continuation.

### CommittedLog — Append-Only Canonical History

`AgentRun` maintains a `CommittedLog` — an append-only log of canonical messages:

```rust
pub(crate) struct CommittedLog(Vec<Arc<Message>>);

impl CommittedLog {
    pub(crate) fn commit(&mut self, messages: impl IntoIterator<Item = Message>) { ... }
    // Only other mutator: unstarted_prompt() — ONLY before first turn
}
```

**Key properties:**

- **Append-only** — no rewrite, delete, or modify of committed messages
- **Canonical** — stores provider-exact messages (full reasoning, raw tool calls, exact tool
  results)
- **Source of truth** for in-memory continuation, display, and tool execution

The run commits at fixed protocol points:

- **Trigger**: Turn accepted (tool-free)
  - **Where Committed**: `next_step()` → `AwaitingAdvance`
  - **What Gets Committed**: `assistant_turn(head, items)`
- **Trigger**: Turn accepted (with tools)
  - **Where Committed**: `next_step()` → `AwaitingAdvance`
  - **What Gets Committed**: `assistant_turn(head, items)`
- **Trigger**: Tool results complete
  - **Where Committed**: `answer_all()` → `ExecutingTools` complete
  - **What Gets Committed**: `ToolResult` messages (user role)
- **Trigger**: Invalid tool call retry
  - **Where Committed**: `resolve_invalid_tool_call()`
  - **What Gets Committed**: `assistant_message` + `user_message` (feedback)
- **Trigger**: Output tool reprompt
  - **Where Committed**: `reprompt_for_output()`
  - **What Gets Committed**: `assistant_message` + `user_message` (feedback)
- **Trigger**: Cancellation/abort
  - **Where Committed**: `fail()` / `cancel_error()`
  - **What Gets Committed**: `closure` (synthetic user message closing batch)

**Prior does NOT call `commit()`** — the `AgentRun` owns all commits internally. Prior's
`LoggingHook` calls its own `transcript::commit()` for session persistence (pi-format JSONL),
separate from rig's `CommittedLog`.

### History Transformation: Canonical → Model-Facing

The canonical `CommittedLog` is **never rewritten**. Instead, hooks transform it per-request:

```rust
// In on_completion_call:
let canonical_history = event.history;  // From AgentRun's CommittedLog

// Build model-facing version (strip reasoning, normalize tool calls, etc.)
let model_facing_history = transform(canonical_history);

// Return patch — AgentRun's log UNCHANGED
CompletionCallAction::Patch(RequestPatch::new().history(model_facing_history))
```

`prepare_request()` uses the patch's history when present:

```rust
let messages_history = request_patch
    .and_then(|o| o.history.as_deref())
    .unwrap_or(chat_history);  // Hook's version REPLACES run's for this request
```

**Separation of concerns:**

| Layer                       | History Shape                              |
| --------------------------- | ------------------------------------------ |
| `AgentRun` / `CommittedLog` | Canonical (full reasoning, raw tool calls) |
| `on_completion_call` hook   | Model-facing (stripped, minimized, split)  |
| Provider request            | Per-turn transformed (non-sticky)          |

---

## Retries: Two Different Mechanisms

### 1. Internal Rig Retry (`ModelTurnAction::Retry`)

Triggered from `on_model_turn_finished` when a turn is tool-free and the hook wants a redo:

```rust
async fn on_model_turn_finished(&self, ctx: &HookContext, event: ModelTurnFinished<'_>) -> ModelTurnAction {
    if should_retry {
        ModelTurnAction::Retry(RetryRequest::Repeat)  // or Feedback(...)
    } else {
        ModelTurnAction::Continue
    }
}
```

**Behavior:**

- Resets `AgentRun` state to `READY` (`PreparingRequest { rollback_owed: false }`)
- **Same `AgentRun` instance** — but `next_step()` increments `current_turn` when leaving
  `PreparingRequest` (see `mod.rs:867`)
- `on_completion_call` fires again with **incremented `event.turn`**, same `prompt`, same `history`
- Hook **can distinguish** retry from first attempt via `turn` value (but not "is this a retry?")
- The retry consumes the total model-call budget (`max_turns`)

**Constraint:** Only works for **tool-free turns**. Tool-bearing turns cannot be retried this way
(would create unanswered calls in provider history).

### 2. External Prior Retry (Prior-Level Loop)

Prior catches `LlmInterrupted` (mid-stream error after progress) and re-calls `prompt()`:

```rust
// interactive.rs:928-1001
Err(LlmInterrupted { cause }) => {
    // Back-off sleep...
    next_input = if session.is_none() {
        input.clone()                      // No session: re-send original
    } else if steer_delivered {
        "continue".to_string()             // Already steered once: short nudge
    } else {
        steer_delivered = true;
        cause.synthetic_retry_feedback()   // First retry: full feedback
            .unwrap_or("continue")
    };
    turn.mark_new_dispatch();              // Counts as NEW user message
    continue;                              // Loops to call prompt() AGAIN
}
```

**Behavior:**

- Creates **fresh `AgentRunner`** each retry → fresh `AgentRun` → **turn counter starts at 1**
  (first `next_step()` increments to 1)
- Sends **different prompt** (steering/feedback message)
- May have **different params** (e.g., temperature dropped for corrective feedback)
- Is a **new user dispatch** (`turn.mark_new_dispatch()`) — distinct from rig's internal retry
- Prior's `LoggingHook` transforms history unconditionally on every turn (no `turn > 1` check)

---

## In-Memory Continuation (Same AgentRun)

For mid-stream failures **within the same turn** (network error, timeout), the `AgentRun` is already
in `AwaitingModel { recorded: false }`. Calling `next_step()` again **re-issues the same attempt**
without incrementing `turn`:

```rust
// mod.rs:856-867 — re-issue path
RunState::PreparingRequest { .. } | RunState::AwaitingModel { .. } => {
    if let RunState::PreparingRequest { .. } = state {
        self.current_turn += 1;  // Only increments when LEAVING PreparingRequest
    }
    let step = self.call_model_step()?;  // Same turn, same prompt, same history
    self.state = RunState::AwaitingModel { recorded: false };
    Ok(step)
}
```

This is the **preferred way to handle mid-stream retries**: keep the `AgentRun` alive and call
`next_step()` again. No new history sent, no turn budget consumed, no steering re-applied.

### When to Use Which Retry

- **Failure Type**: Mid-stream (network, timeout)
  - **Mechanism**: Re-issue `next_step()` on same `AgentRun`
  - **Turn Counter**: Same
  - **History Re-sent**: No
- **Failure Type**: Tool-free turn rejected by hook
  - **Mechanism**: `ModelTurnAction::Retry` → `retry_model_turn()`
  - **Turn Counter**: Increments
  - **History Re-sent**: No (same prompt/history)
- **Failure Type**: Mid-stream with progress (`LlmInterrupted`)
  - **Mechanism**: Fresh `AgentRunner` + steering feedback
  - **Turn Counter**: Resets to 1
  - **History Re-sent**: Yes (with pi session context)
- **Failure Type**: New user prompt after `Done`
  - **Mechanism**: Fresh `AgentRun` (required)
  - **Turn Counter**: Starts at 1
  - **History Re-sent**: Yes (full pi session)

**`AgentRun` is designed for ONE user prompt** — it reaches `Done` and stops. A follow-up user
prompt always requires a fresh `AgentRun` seeded with the pi session.

---

## HookContext: Scratchpad & Entries

`HookContext` provides run-scoped state shared across all hook events:

```rust
pub struct HookContext {
    // Shared run scratchpad (in-memory, not persisted)
    scratchpad: Scratchpad,
    // Persisted entries (serialized in AgentRun, survive in-memory resume)
    entries: Vec<Aside>,
}

impl HookContext {
    // Scratchpad: in-memory key-value, lost when AgentRun drops
    pub fn scratchpad(&self) -> &Scratchpad { ... }
    // Scratchpad methods: insert<T>(), get<T>(), update<T, R>(), contains<T>(), remove<T>()
    
    // Entries: persisted asides, survive in-memory continuation
    pub fn append_entry<T: Serialize>(&self, kind: impl Into<String>, value: &T) -> Result<()>
    pub fn entries(&self, kind: &str) -> Vec<Aside>
    pub fn last_entry(&self, kind: &str) -> Option<Aside>
}
```

**Use `entries` for state that must survive rig-internal retries and in-memory continuation** (e.g.,
"which turns already steered"), `scratchpad` for ephemeral in-memory tracking within a single
`next_step()` cycle.

> **Important**: `CompletionCallEvent` does not expose `additional_params`. To pass context to hooks
> across retries/continuation, use `HookContext::entries()` (survives rig retries) or `scratchpad()`
> (single step only).

---

## Key Takeaways for Prior

|Goal                                           |Mechanism                                        |
|-----------------------------------------------|-------------------------------------------------|
|Transform canonical → model-facing history     |`on_completion_call` + `RequestPatch::history()` |
|Inject per-turn context documents              |`RequestPatch::extra_context` (appended in order)|
|Replace history entirely (per-request)         |`RequestPatch::history(Some(new_history))`       |
|Pass provider-specific params (e.g., reasoning)|`RequestPatch::additional_params`                |
|Stop the run mid-stream                        |`CompletionCallAction::Stop("reason")`           |
|Retry tool-free turn with feedback             |`ModelTurnAction::Retry(RetryRequest::Feedback)` |
|Retry mid-stream failure (same turn)           |Re-issue `next_step()` on same `AgentRun`        |
|Retry after progress with steering             |Fresh `AgentRunner` + pi session + feedback      |
|Track state across rig retries/continuation    |`HookContext::entries()`                         |
|Track state within one `next_step()` cycle     |`HookContext::scratchpad()`                      |

The `extra_context` field is the intended mechanism for **steering documents** — appended before the
chat history on each turn, merged in hook registration order, non-sticky (only for the current
turn). **Prior does not use this for steering**; it sends steering as explicit user messages at the
application layer.

---

## Prior's Current Architecture Is Correct

Prior's design aligns with rig's intended pattern:

1. **Session stores canonical pi-format messages** (with reasoning, full tool calls)
2. `session_context_messages()` builds input history (may strip reasoning for context)
3. `LoggingHook::on_completion_call` builds **model-facing history** (strip reasoning → normalize
   tool calls → render `edit` blocks)
4. Sent via `RequestPatch::history()` — **run's canonical log untouched**

**On in-memory continuation**: Same flow. Canonical history from `AgentRun` → hook transforms fresh
→ request sent.

**On fresh run for new prompt/steering**: Pi session loaded → fresh `AgentRun` with `with_history()`
→ hook transforms fresh → request sent.

The run is the **source of truth** for its prompt; hooks provide **per-request views**. Prior's
external retries are corrective re-prompts (new user messages), not rig-internal retries. No
double-injection occurs because Prior does not use conditional `extra_context` injection.
