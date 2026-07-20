# Implementing `pi`-style steering on top of `rig`

Companion to [`pi_steering.md`](./pi_steering.md). That doc explains how `pi`
(`packages/agent`) queues and injects steering messages, and how a single
`transformMessages` boundary keeps injection valid across providers. This doc maps
that design onto **`rig`** (the Rust LLM backend), using the copy in
`~/.cargo/git/checkouts/rig-*/crates/rig-core`.

**Headline:** rig already provides every primitive `pi` had to build by hand:

- A **provider-agnostic `Message`** enum (`rig_core::completion::Message`) — the
  exact analog of `pi`'s neutral `Message` returned by `convertToLlm`.
- An **`AgentHook`** system (`on_completion_call`, `on_model_turn_finished`, …) with a
  **`RequestPatch`** you can use to inject messages before a model call.
- A **runner that owns the tool loop** and *always* pairs an assistant `ToolCall` with a
  `ToolResult` before the run continues — so the "synthetic orphaned tool result"
  machinery in `pi`'s `transformMessages` is **unnecessary** for normal steering.
- **Per-provider adapters** (Anthropic, OpenAI, Gemini, Mistral, Bedrock, …) that
  translate `Message` → wire format, normalize tool-call IDs, and handle reasoning —
  i.e. the entire provider-specific half of `pi`'s `transformMessages` already exists.

So the work is mostly *orchestration*: bring back the `PendingMessageQueue` + driver
loop, and let rig do the provider translation.

## 1. Capability mapping

| `pi` concept | `rig` equivalent |
|--------------|------------------|
| `AgentMessage[]` / `convertToLlm` → neutral `Message[]` | `rig_core::completion::Message` (already neutral) |
| `Agent.steer()` / `followUp()` + `PendingMessageQueue` | your `SteeringSession` holding `Vec<Message>` + two `PendingMessageQueue`s |
| `QueueMode` (`one-at-a-time` / `all`) | same enum, same `drain()` logic |
| `agent-loop.ts` `runLoop` inner/outer loop | **you** drive this at the `AgentRunner` granularity |
| `getSteeringMessages` polled after tool calls | `AgentHook::on_completion_call` (fires before each model call) |
| injection as a plain `user` message | `RequestPatch::history` or a re-driven `runner(steer_msg).history(...)` |
| `shouldStopAfterTurn` | `AgentHook::on_model_turn_finished` → `ModelTurnAction::Stop` |
| `transformMessages` (orphan synthetic results, ID norm, reasoning, images) | handled inside rig's provider adapters — **no reimplementation** |
| `continue()` draining steering/follow-up into a new turn | `agent.runner(msg).history(conversation).run()` re-drive |

## 2. The queue layer (copy `pi` verbatim)

`pi`'s `PendingMessageQueue` (`packages/agent/src/agent.ts:123-157`) is plain Rust.
`steer()`/`followUp()` enqueue; ` drain()` honors `QueueMode`:

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum QueueMode { OneAtATime, All }

pub struct PendingMessageQueue {
    messages: Vec<Message>,
    pub mode: QueueMode,
}

impl PendingMessageQueue {
    pub fn enqueue(&mut self, m: Message) { self.messages.push(m); }
    pub fn has_items(&self) -> bool { !self.messages.is_empty() }
    pub fn clear(&mut self) { self.messages.clear(); }

    /// Mirror of pi's drain(): `All` returns everything; `OneAtATime` returns one.
    pub fn drain(&mut self) -> Vec<Message> {
        match self.mode {
            QueueMode::All => std::mem::take(&mut self.messages),
            QueueMode::OneAtATime => match self.messages.split_first() {
                Some((first, rest)) => { self.messages = rest.to_vec(); vec![first.clone()] }
                None => Vec::new(),
            },
        }
    }
}
```

Unlike `pi`, the queue is owned by a `SteeringSession` that also owns the **canonical
conversation** — `pi` keeps this in `Agent._state.messages`; rig's `AgentRun` holds it
internally and (per `agent/completion.rs:437-443`) a `RequestPatch::history` is
*per-turn only* and never feeds back into canonical state. So for persistence and
compaction we keep the transcript ourselves:

```rust
pub struct SteeringSession {
    pub conversation: Vec<Message>,        // canonical, mirrors pi Agent._state.messages
    pub steering: PendingMessageQueue,
    pub followup: PendingMessageQueue,
}

impl SteeringSession {
    pub fn steer(&mut self, text: impl Into<String>) {
        self.steering.enqueue(Message::user(text));
    }
    pub fn follow_up(&mut self, text: impl Into<String>) {
        self.followup.enqueue(Message::user(text));
    }
}
```

The live, shareable handle is `Arc<Mutex<SteeringSession>>` (or `RwLock`), shared
between the UI thread (which calls `steer()`) and the driver thread described below.
It must be `Send + Sync`, which `Mutex<SteeringSession>` is.

## 3. Driver loop — the outer/inner `runLoop`

`pi` drives everything in one opaque `runLoop` and polls `getSteeringMessages`
between turns. rig's `AgentRunner::run()`/`stream()` is also one opaque multi-turn
exchange, but the only mid-run injection point is the hook. The simplest robust design
is to **re-drive at run granularity**, exactly like `pi`'s `continue()`: each steer /
follow-up becomes a *new* `runner(...)` seeded with the accumulated `conversation` via
`.history(...)`. Because rig already executed every tool call to completion inside the
previous run, the `conversation` we carry forward contains only well-formed
`assistant(toolCall)` + `user(toolResult)` pairs — never orphans.

```rust
use rig_core::{
    agent::{AgentHook, CompletionCall, CompletionCallAction, ModelTurnFinished, ModelTurnAction},
    completion::{Message, Prompt},
    agents::Agent,
};

async fn drive(agent: &Agent<impl CompletionModel>, session: &Arc<Mutex<SteeringSession>>) {
    loop {
        // 1. Pick the next prompt: steering first (mid-run), then follow-up (post-run).
        let next: Option<Vec<Message>> = {
            let mut s = session.lock().unwrap();
            if s.steering.has_items() {
                Some(s.steering.drain())          // pi: injected after current turn's tool calls
            } else if s.followup.has_items() {
                Some(s.followup.drain())          // pi: only after the agent would stop
            } else {
                None
            }
        };
        let Some(batch) = next else { break };

        // 2. Start a fresh run seeded with the full canonical transcript.
        let history = session.lock().unwrap().conversation.clone();
        let runner = agent.runner(batch)
            .history(history)
            .add_hook(SteeringHook { session: session.clone() });

        // 3. Consume the multi-turn stream, recording every item into our transcript
        //    (mirrors pi's processEvents / newMessages).
        let mut stream = runner.stream();
        while let Some(item) = stream.next().await {
            match item {
                MultiTurnStreamItem::StreamAssistantItem(c) =>
                    record_assistant(session, c),
                MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult { .. }) =>
                    record_tool_result(session, item),
                MultiTurnStreamItem::FinalResponse(_) => break,
                _ => {}
            }
        }
    }
}
```

This reproduces `pi`'s contract precisely:

- **Steering** queued during a run is drained and injected as a `user` message right
  after the previous turn's tool results — before the model's *next* call.
- **Follow-up** only fires once the model has stopped issuing tool calls (the run
  ended with `FinalResponse`), so it never interrupts a tool-flow turn.
- `QueueMode::All` drains the whole batch into one `runner(batch)`; `OneAtATime`
  drains one message → one run → re-loop.

### 3.1 Immediate injection inside a single run (optional optimization)

The re-drive above waits for the current run to *finish* before injecting steering. To
be as responsive as `pi` (inject on the very next model call, even mid-run), use a hook:

```rust
pub struct SteeringHook { pub session: Arc<Mutex<SteeringSession>> }

impl AgentHook for SteeringHook {
    async fn on_completion_call(
        &self, _ctx: &HookContext, event: CompletionCall<'_>,
    ) -> CompletionCallAction {
        // event.history already contains the prior turn's tool results.
        let steer: Vec<Message> = self.session.lock().unwrap().steering.drain();
        if steer.is_empty() {
            return CompletionCallAction::continue_run();
        }
        // Append steering to what the provider sees this turn.
        let mut patched = event.history.to_vec();
        patched.extend(steer.clone());
        CompletionCallAction::patch(RequestPatch::new().history(patched))
    }

    async fn on_model_turn_finished(
        &self, _ctx: &HookContext, _event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        // pi's shouldStopAfterTurn: stop early (e.g. context full) before next call.
        ModelTurnAction::continue_run()
    }
}
```

Caveat (and why the re-drive is the primary design): `RequestPatch::history` is
**per-turn only** (`agent/completion.rs:437-443`) — it changes what the model sees for
that one call but is not written back into canonical state, so a later turn within the
*same* run would "forget" the steering, and `full_history()` would omit it. Use the
hook only for latency-critical, single-turn injection, and **also** push the steering
message into `SteeringSession::conversation` at injection time (e.g. via the shared
`Arc<Mutex<…>>` or `HookContext::scratchpad`) so the transcript stays correct.

## 4. Provider handling — what rig gives you for free

`pi`'s `transformMessages` (`packages/ai/src/api/transform-messages.ts`) exists to keep
a mid-run `user` message legal for each provider. rig already does all of this inside
its provider adapters, so a steering `Message::User` works uniformly:

| `pi` `transformMessages` concern | Status in rig |
|----------------------------------|---------------|
| Orphaned tool calls → synthetic `isError` tool result | **Not needed.** rig's runner resolves every tool call to a `ToolResult` *before* the next model call, and we re-drive with the resolved `conversation`. No orphan is ever produced. (Synthetic results still exist in rig, but only for hook-*skipped* tool calls — `agent/run/mod.rs:928`, `prompt_request/streaming.rs`.) |
| Per-provider tool-call ID normalization (`toolu_` / `fc_` / `callId\|itemId`) | Handled by each adapter's `Message` → wire translation. `ToolCall.id`/`call_id` are normalized internally. |
| Reasoning/thinking (redacted kept same-model, else text) | `Message` carries `AssistantContent::Reasoning`; adapters keep redacted blocks only for the same model and downgrade otherwise. |
| Image downgrade for non-vision models | User-message images go through each adapter's translation; they are fine for a steering `user` message. Note: OpenAI rejects images *inside tool results* (`providers/openai/completion/mod.rs:613`), so keep images out of tool results — not a steering concern. |
| Dropping errored/aborted turns | rig canonicalizes turns before they enter tool execution; a failed turn is not replayed as a broken `toolCall`. |

Concretely: `agent.runner(steer_msg).history(conversation)` with `conversation`
containing `assistant(toolCall)` + `user(toolResult)` pairs produces a valid request on
Anthropic, OpenAI (chat *and* responses API), Gemini, Mistral, and Bedrock with **zero
provider-specific code on your side** — isomorphic to `pi`'s "no per-provider branching
on steering itself."

## 5. Notes, edge cases, and limitations

- **Images in steering:** build `Message::User { content: UserContent::Image(...) }`
  if you want them; rig's user-image handling is uniform. Don't put images in a tool
  result if you ever target OpenAI chat completions.
- **Model switching mid-conversation:** if a steer changes the model, redacted
  reasoning from the prior model is dropped by the adapter (same as `pi`'s
  `isSameModel` check). Acceptable; just be aware the transcript may lose encrypted
  thinking.
- **`max_turns` / budgets:** rig's `AgentRunner::max_turns` caps model calls per run.
  Because each steer/follow-up is a *new* run, budget per run, not globally — mirror
  `pi`'s per-run semantics or track a global counter in `SteeringSession`.
- **Concurrency:** `steer()` from the UI and the driver loop share `SteeringSession`
  under a `Mutex`. The hook receives `&self`, so it must read the queue through the
  same `Arc<Mutex<…>>` (it is `Send + Sync`, satisfying `WasmCompatSend`/
  `WasmCompatSync`).
- **Stop semantics:** to honor `pi`'s `shouldStopAfterTurn`, return
  `ModelTurnAction::Stop(reason)` from `on_model_turn_finished` (or check the queue in
  the driver loop before re-driving).

## 6. Summary

Implementing `pi`-style steering on rig is mostly *deletion* relative to `pi`:

1. Keep `PendingMessageQueue` + `QueueMode` (trivial).
2. Keep a canonical `Vec<Message>` transcript in `SteeringSession`.
3. Drive rig with a re-drive loop (`runner(batch).history(conversation)`), optionally
   accelerated by an `AgentHook` that injects via `RequestPatch::history`.
4. Drop `transformMessages` entirely — rig's adapters already normalize IDs, pair tool
   calls, translate reasoning, and downgrade images per provider.

The result is provider-agnostic by construction, with all provider differences confined
to rig's existing adapters, exactly as `pi_steering.md` recommends but without the
custom transform layer.

## 7. Key `rig` references (from the `~/.cargo` copy)

- `rig-core/src/completion/message.rs` — `Message`, `UserContent`, `AssistantContent`,
  `ToolCall`, `ToolResult`, `Message::user/system/assistant` (`message.rs:631-672`).
- `rig-core/src/agent/hook.rs` — `AgentHook`, `CompletionCall`, `CompletionCallAction`
  (`patch`/`continue_run`/`stop`, `hook.rs:726-747`), `RequestPatch` with
  `history`/`extra_context` (`hook.rs:579-696`), `ModelTurnFinished`/`ModelTurnAction`
  (`hook.rs:416-476`), `HookContext::scratchpad` (`hook.rs:347-349`).
- `rig-core/src/agent/completion.rs:437-443` — `RequestPatch::history` is per-turn only.
- `rig-core/src/agent/runner.rs` — `AgentRunner::run`/`stream` (e.g. `runner.rs:1125`),
  `.history()`/`.add_hook()`.
- `rig-core/src/agent/run/mod.rs` — `AgentRun::full_history()` (`run/mod.rs:550`),
  `.messages()`, synthetic tool results on skip (`run/mod.rs:928`).
- `rig-core/src/agent/prompt_request/streaming.rs` — `MultiTurnStreamItem`
  (`streaming.rs:47-121`): `StreamAssistantItem`, `StreamUserItem`,
  `ToolExecutionCommitted`, `FinalResponse`.
- Provider adapters that translate `Message` → wire (the equivalent of `pi`'s
  `transformMessages`): `providers/anthropic/completion.rs`,
  `providers/openai/completion/mod.rs`, `providers/openai/responses_api/mod.rs`,
  `providers/gemini/completion.rs`, `providers/mistral/completion.rs`,
  `providers/azure.rs`, `providers/bedrock` (separate crate `rig-bedrock`).
