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

### `CompletionCallAction` — Hook Output

| Variant               | Effect                                                           |
| --------------------- | ---------------------------------------------------------------- |
| `Continue`            | Send the baseline request unchanged                              |
| `Patch(RequestPatch)` | **Merge a per-turn patch** into the request (steering injection) |
| `Stop(String)`        | Stop the run entirely with a reason                              |

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

## Example: Injecting a Steering Message

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
steering behavior on retries and resumes.

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
- **Source of truth** for resume, display, and tool execution

The run commits at fixed protocol points (all inside `next_step()`):

| Trigger                    | What Gets Committed                              |
| -------------------------- | ------------------------------------------------ |
| Turn accepted (tool-free)  | `assistant_turn(head, items)`                    |
| Turn accepted (with tools) | `assistant_turn(head, items)`                    |
| Tool results complete      | `ToolResult` messages (user role)                |
| Invalid tool call retry    | `assistant_message` + `user_message` (feedback)  |
| Output tool reprompt       | `assistant_message` + `user_message` (feedback)  |
| Cancellation/abort         | `closure` (synthetic user message closing batch) |

**Prior does NOT call `commit()`** — the `AgentRun` owns all commits internally.

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

- Resets `AgentRun` state to `READY` (same `turn` number)
- **Same `AgentRun` instance** — turn counter does NOT increment
- `on_completion_call` fires again with **identical `event.turn`, `prompt`, `history`**
- Hook **cannot distinguish** retry from first attempt without external state

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
        "continue".to_string()             // Already steered once: short continue
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

- Creates **fresh `AgentRunner`** each retry → fresh `AgentRun` → **turn counter resets to 1**
- Sends **different prompt** (steering/feedback message)
- May have **different params** (e.g., temperature dropped for corrective feedback)
- Is a **new user dispatch** — distinct from rig's internal retry

---

## The Double-Steering Problem

Prior's steering hook (pattern: `if event.turn > 1 { inject }`) fires incorrectly on external
retries:

| Attempt               | `AgentRunner` | `event.turn` | Hook Sees  | Injects?                   |
| --------------------- | ------------- | ------------ | ---------- | -------------------------- |
| First                 | #1            | 1            | Turn 1     | No (turn ≤ 1)              |
| Tool call             | #1            | 2            | Turn 2     | **Yes** ("continue")       |
| **External retry**    | **#2 (new)**  | **1**        | **Turn 1** | **No** (turn ≤ 1)          |
| Retry makes tool call | #2            | 2            | Turn 2     | **Yes AGAIN** ("continue") |

The hook injects on **every retry's turn 2+** because each `prompt()` call creates a fresh runner
with turn counter starting at 1.

---

## Rig's Native Resume (`Agent::resume`)

Rig provides proper resume via persisted `AgentRun`:

```rust
// First run
let runner = agent.prompt("Fix the bug").add_hook(MyHook).run().await?;

// On error, get the AgentRun (requires API change to expose it)
// Later: resume the EXACT same run
let runner = agent.resume(persisted_run).add_hook(MyHook).run().await?;
```

### What Resume Preserves

|Component                           |Source               |Resume Behavior                        |
|------------------------------------|---------------------|---------------------------------------|
|`CommittedLog` (canonical history)  |Serialized `AgentRun`|**Loaded as-is**                       |
|`chat_history` (input context)      |Serialized `AgentRun`|**Loaded as-is**                       |
|`current_turn`                      |Serialized `AgentRun`|**Restored**                           |
|`previous_model`                    |Serialized `AgentRun`|**Restored**                           |
|`turn_tools` (advertised tool names)|Serialized `AgentRun`|**Restored**                           |
|Hook `entries` (asides)      |Serialized `AgentRun`|**Restored via `HookContext::seed_entries()`**|

### What Is Fresh on Resume

|Component             |Source                                       |Resume Behavior              |
|----------------------|---------------------------------------------|-----------------------------|
|Tool implementations|`ToolServerHandle` (current process registry)|**Fresh snapshot** at `CallTools` by name|
|Run config (`RunSpec`)|`AgentRunner.config.spec`                    |**Fresh** from new runner    |
|Hook instances        |`AgentRunner.config.hooks`           |**Fresh** — state from `entries` only|
|Model client          |`AgentRunner.config.handlers`                |**Fresh** from new runner    |

### Parameter Changes Across Resume — Supported

|Parameter                                             |Can Change on Resume?                      |
|------------------------------------------------------|-------------------------------------------|
|`preamble`, `temperature`, `tool_choice`, `max_tokens`|✅ Yes — runner config applied fresh       |
|`additional_params`                                   |✅ Yes — merged fresh per request          |
|`active_tools`                                        |✅ Yes — intersected fresh                 |
|Tool implementations                             |✅ Always fresh — rebound by name at `CallTools`|

**The runner config is applied fresh on every resume** — `build_run()` only uses persisted
`AgentRun` for history/turn state; config comes from the new `AgentRunner`.

---

## Why Prior Doesn't Use Rig Resume Today

- **Blocker**: **API Access**
  - **Details**: `prompt()` returns `String`, not `AgentRun`. Need to extract run from driver or
    error.
- **Blocker**: **Dual Persistence**
  - **Details**: Prior persists pi-format JSONL. Would need to also persist rig's `AgentRun`
    envelope.
- **Blocker**: **Hook Reconstruction**
  - **Details**: `LoggingHook` has `Arc<Mutex<TurnTranscript>>`, `Arc<Mutex<EditAnchorLedger>>`,
    `Arc<Session>`, `Arc<MessageLogger>` — must reconstruct from `entries` or session.
- **Blocker**: **Semantic Mismatch**
  - **Details**: Prior's retries = **new prompts** (corrective). Rig resume =
    **re-issue same request**.

Prior's external retries are fundamentally "corrective re-prompt," not "request re-issue." The
current architecture (reconstruct agent, send new steering prompt) matches the intent better.

---

## Solutions for Prior

### Option A: Use Rig Resume (Architecture Change)

Persist `AgentRun` on error, resume it. Requires API changes, dual persistence, hook reconstruction.
**Not recommended** — semantic mismatch.

### Option B: Track "Logical Turn" in Hook Scratchpad

```rust
async fn on_completion_call(&self, ctx: &HookContext, event: CompletionCallEvent<'_>) -> CompletionCallAction {
    let key = format!("logical_turn_{}_{}", session_id, user_prompt_count);
    let logical_turn = ctx.scratchpad().get::<u32>(&key).unwrap_or(0) + 1;
    ctx.scratchpad().insert(&key, logical_turn);
    
    if logical_turn > 1 && !already_steered_this_logical_turn { ... }
}
```

Works but fragile — scratchpad is in-memory only, lost on process restart.

### Option C: Pass Retry Context via `additional_params` (Recommended)

```rust
// In prompt(), when retrying:
let request = if is_retry {
    self.agent.prompt(input)
        .replace_additional_params(json!({"prior_retry": true, "logical_turn": 3}))
} else {
    self.agent.prompt(input)
};

// In hook:
let is_retry = event.additional_params
    .get("prior_retry").and_then(|v| v.as_bool()).unwrap_or(false);
let logical_turn = event.additional_params
    .get("logical_turn").and_then(|v| v.as_u64()).unwrap_or(event.turn as u64);

if logical_turn > 1 && !is_retry { inject }  // Skip on retry
```

Clean, explicit, survives process restart via `additional_params` in request.

### Option D: Disable Rig Steering, Use Prior's Explicit Steering (Simplest)

Prior already implements steering at application level (lines 961-992 in `interactive.rs`):

- Sends "continue" / synthetic feedback as **explicit user messages**
- Model sees them in history naturally
- No rig hook needed → no double-injection

**Remove the steering hook entirely.** The `extra_context` injection is redundant when Prior
controls the conversation flow explicitly.

---

## HookContext: Scratchpad & Entries

`HookContext` provides run-scoped state shared across all hook events:

```rust
pub struct HookContext {
    // Shared run scratchpad (in-memory, not persisted)
    scratchpad: Scratchpad,
    // Persisted entries (serialized in AgentRun, survive resume)
    entries: Vec<Aside>,
}

impl HookContext {
    // Scratchpad: in-memory key-value, lost on process exit
    pub fn scratchpad(&self) -> &Scratchpad { ... }
    // Scratchpad methods: insert<T>(), get<T>(), update<T, R>(), contains<T>(), remove<T>()
    
    // Entries: persisted asides, survive resume
    pub fn append_entry<T: Serialize>(&self, kind: impl Into<String>, value: &T) -> Result<()>
    pub fn entries(&self, kind: &str) -> Vec<Aside>
    pub fn last_entry(&self, kind: &str) -> Option<Aside>
}
```

**Use `entries` for resume-surviving state** (e.g., "which turns already steered"), `scratchpad` for
in-memory tracking within a run.

---

## Key Takeaways for Prior

|Goal                                           |Mechanism                                         |
|-----------------------------------------------|--------------------------------------------------|
|Inject steering before *every* model call    |`on_completion_call` + `RequestPatch::extra_context`|
|Inject only after tool results                 |Check `event.turn > 1`                            |
|Replace history entirely (per-request)         |`RequestPatch::history(Some(new_history))`        |
|Pass provider-specific params (e.g., reasoning)|`RequestPatch::additional_params`                 |
|Stop the run mid-stream                        |`CompletionCallAction::Stop("reason")`            |
|**Retry without re-injecting steering**|**Option C: `additional_params` flags** or **Option D: no rig hook**|
|**Resume without re-injecting**      |**Option C** or **Option D** (rig resume works but overkill)|
|Track state across turns/resumes |`HookContext::entries()` (persisted) or `scratchpad` (in-memory)|

The `extra_context` field is the intended mechanism for "steering messages" — documents appended
before the chat history on each turn, merged in hook registration order, non-sticky (only for the
current turn).

---

## Prior's Current Architecture Is Correct

Prior's design aligns with rig's intended pattern:

1. **Session stores canonical pi-format messages** (with reasoning, full tool calls)
2. `session_context_messages()` builds input history (may strip reasoning for context)
3. `LoggingHook::on_completion_call` builds **model-facing history** (strip → blob → minimize →
   split)
4. Sent via `RequestPatch::history()` — **run's canonical log untouched**

**On resume**: Same flow. Canonical history loaded → hook transforms fresh → request sent.

The run is the **source of truth**; hooks provide **per-request views**. The double-steering on
external retries is a consequence of Prior's corrective-reprompt retry model, not a rig bug.
