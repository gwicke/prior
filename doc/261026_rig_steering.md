# rig Steering / Injection Mechanism

## Overview

rig 0.45 provides steering and context injection through the **`AgentHook`** trait in `rig_agent::agent::hook`. The primary entry point for injecting content before model calls is the `on_completion_call` hook method.

## Core Types

### `AgentHook` Trait

```rust
pub trait AgentHook: WasmCompatSend + WasmCompatSync {
    async fn on_completion_call(
        &self,
        ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction;
    // ... other lifecycle methods
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

| Variant | Effect |
|---------|--------|
| `Continue` | Send the baseline request unchanged |
| `Patch(RequestPatch)` | **Merge a per-turn patch** into the request (steering injection) |
| `Stop(String)` | Stop the run entirely with a reason |

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

> "Per `CallModel` step, hook resolution is ordered: **completion-call hooks run first** and their `RequestPatch`es merge in registration order. Only when every completion-call hook proceeds does `ModelSelection` run... after which request preparation inspects the selected model's captured `ProviderCapabilities` and the attempt is issued."

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

Multiple hooks form a `HookStack` — they run in **registration order**; their `RequestPatch`es merge per the rules above.

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

## Key Takeaways for Prior

| Goal | Mechanism |
|------|-----------|
| Inject steering before *every* model call | `on_completion_call` + `RequestPatch::extra_context` |
| Inject only after tool results | Check `event.turn > 1` |
| Replace history entirely | `RequestPatch::history(Some(new_history))` |
| Pass provider-specific params (e.g., reasoning) | `RequestPatch::additional_params` |
| Stop the run mid-stream | `CompletionCallAction::Stop("reason")` |

The `extra_context` field is the intended mechanism for "steering messages" — documents appended before the chat history on each turn, merged in hook registration order, non-sticky (only for the current turn).