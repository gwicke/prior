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

This reproduces `pi`'s contract precisely, and matches how pi handles
`continue()` — pi's `runAgentLoop`/`runAgentLoopContinue` are also called per
batch ([`agent.ts:396-409`](../pi/packages/agent/src/agent.ts)):

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

**Note:** pi does not use this hook-based optimization. pi's steering messages are
pushed directly into `currentContext.messages` and `newMessages` at the agent-loop
level ([`agent-loop.ts:186-187`](../pi/packages/agent/src/agent-loop.ts)), which is
isomorphic to the re-drive approach (the steering messages are in the canonical
transcript before the next LLM call). The re-drive is therefore the more faithful
translation of pi's design; the hook is a picocode-specific latency optimization
with additional bookkeeping.

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

## 6. Blocking dependency — prompt cancellation must be tackled first

Steering (injecting a message mid-turn) will be responsive only if the current turn
can be interrupted promptly.  pi actually separates these two concerns:
- [`steer()`](../pi/packages/agent/src/agent.ts:274-276) merely **enqueues** a
  message; the loop picks it up at the next `getSteeringMessages()` poll
  (`agent-loop.ts:259`).
- [`abort()`](../pi/packages/agent/src/agent.ts:310-312) stops the current run
  immediately by aborting the `AbortController`.

Steering works correctly without abort — the message is delivered after the
current tool finishes.  But a user who `<Esc>` expects the turn to stop *now*,
and if the response doesn't arrive for 60 seconds (a long `sleep` or a verbose
reasoning turn) the steering message is equally delayed.

For picocode, clean responsive steering requires fixing cancellation first.

### 6.1 How `<Esc>` cancellation works today

The chain is (`src/repl.rs:109`, `src/agent.rs:690-757`, rig `streaming.rs:512-638`):

```
User presses <Esc>
  → spawn_in_flight_reader thread sets cancel = true (repl.rs:110)
  → LoggingHook checks cancel at discrete hook points:
      - on_completion_call     (before an LLM call)
      - on_completion_response (after LLM response chunk)
      - on_model_turn_finished (after model turn finishes)
      - on_tool_call           (before a tool starts)
      - on_tool_result         (after a tool returns)
  → rig's drive_agent loop polls these hooks only at turn boundaries
  → the stream yields PromptCancelled / the prompt() error branch fires
```

**Three gaps prevent this from being immediate:**

| Gap | Why it's slow | Latency impact |
|-----|---------------|----------------|
| **LLM stream in flight** | `on_completion_response` fires on each streamed *chunk*, but the `CancelSignal` only prevents the *next* model call — it does not abort the in-flight HTTP request. | Up to the full model response time (seconds–minutes) |
| **Tool execution in flight** | `on_tool_call` fires *before* the tool starts. Once `run_single_tool` dispatches into `Bash::call` → `run_sandboxed` → `capture_bounded`, there is no mechanism to abort the running child process. The only stop mechanism is the wall-clock timeout. | Up to the full tool timeout |
| **No signal in tool context** | Unlike pi, which passes `AbortSignal` to `tool.execute()` ([`agent-loop.ts:677-680`](../pi/packages/agent/src/agent-loop.ts)) and lets each tool register an `abort` event listener ([`nodejs.ts:341-347`](../pi/packages/agent/src/harness/env/nodejs.ts), where it calls `killProcessTree(child.pid)`), rig's tool dispatch has no cancellation channel. | Structural gap |

### 6.2 Approach — `CancellationToken` + per‑loop polls

The fix has two halves, one for each long-running subsystem:

#### 6.2.1 Aborting an in‑flight LLM stream

The provider stream (`source.run_model_turn` in `streaming.rs`) runs inside rig's
async `drive_agent` loop.  The stream is consumed by:

```rust
// streaming.rs:588-597
while let Some(item) = turn_stream.next().await { … }
```

`next()` awaits the next provider chunk.  If the cancel flag is set, we can
**drop the stream** and let the `Err` propagate.  Concretely:

1. **Thread a `tokio_util::sync::CancellationToken`** through the agent/runner
   hierarchy so both picocode and rig's streaming loop can observe it.
2. **Check the token after every `turn_stream.next()`** in `drive_agent`
   (`streaming.rs:589`).  If cancelled, drop the turn stream and yield
   `PromptCancelled`.
3. **Provider adapters** that support request cancellation (e.g. `reqwest`-based
   ones) can register the token with the HTTP client so the TCP connection is
   dropped immediately rather than waiting for the full response.

Because picocode controls the rig fork (pinned in `Cargo.toml`), these changes
go into the fork's `rig-core/src/agent/` and `rig-core/src/client/` — no
upstream PR needed.

#### 6.2.2 Aborting a running tool (shell command)

`capture_bounded` (`src/tools.rs:439-532`) runs the child via
`duct::Expression::reader()` and reads stdout in a blocking loop.
Fortunately it already has a **watchdog thread** that can kill the child,
and a **channel** (`cancel_tx`/`cancel_rx`) that can wake the watchdog:

```rust
let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
let guard = thread::spawn(move || {
    // Blocks until timeout OR cancel_tx is dropped (command finished)
    if let Err(RecvTimeoutError::Timeout) =
        cancel_rx.recv_timeout(Duration::from_secs(secs))
    {
        // Only the timeout branch kills the child
        flag.store(true, Ordering::SeqCst);
        let _ = w.kill();
    }
    // Ok(()) / Disconnected → command finished normally, don't kill
});
// ... read loop ...
drop(cancel_tx);  // signals watchdog that command finished
```

`mpsc::Sender` implements `Clone`, so we can extend this **without any
polling loop**:

1. **Clone the sender** inside `capture_bounded`: `let cancel_on_esc =
   cancel_tx.clone();`

2. **Register the clone** in a process-wide slot so the `<Esc>` handler can
   send `()` on it:

   ```rust
   // In tools.rs
   static TOOL_CANCEL_SENDERS: LazyLock<Mutex<Vec<mpsc::Sender<()>>>> = ...;
   ```

   Registration is scoped: push on entry, pop on exit (via a drop guard).

3. **When `<Esc>` fires**, `spawn_in_flight_reader` sends `()` on all
   registered senders (in addition to setting `user_cancel = true`):

   ```rust
   // repl.rs:109 — Esc handler
   cancel.store(true, Ordering::SeqCst);
   for tx in TOOL_CANCEL_SENDERS.lock().unwrap().iter() {
       let _ = tx.send(());
   }
   ```

4. **The watchdog's `recv_timeout` returns `Ok(())` immediately** (not
   `Err(Timeout)`).  We change the match to kill the child on this branch
   too:

   ```rust
   match cancel_rx.recv_timeout(Duration::from_secs(secs)) {
       Err(RecvTimeoutError::Timeout) => {
           flag.store(true, Ordering::SeqCst);
           let _ = w.kill();
       }
       Ok(()) => {
           // Woken by cancel_on_esc.send(()) — user pressed <Esc>
           let _ = w.kill();
       }
       Err(RecvTimeoutError::Disconnected) => {
           // All senders dropped — command finished normally
       }
   }
   ```

   No flags to check, no loops, no polling.  The wakeup is purely
   event-driven through the existing channel.

5. **After the read loop** (or on error), `cancel_tx` is dropped as before.
   If the watchdog already fired (timeout or cancel), `join()` returns
   immediately because the thread exited.  If neither fired, the drop wakes
   the watchdog to exit without killing.

**Why this works without polling:**

- The watchdog thread blocks on `recv_timeout`.  The only way it progresses
  is: (a) timeout elapses, (b) `()` sent on a clone sender, or (c) all
  senders dropped.
- Sending on a `mpsc::Sender` is non-blocking (unbounded channel).  The
  in-flight reader thread sends `()` immediately when it sees `<Esc>`.
- The read loop (`reader.read()`) may still block until the child actually
  dies from the signal — that's OS-level and unavoidable, but the child is
  killed within microseconds of `<Esc>`.

**Comparison to pi:** pi does the same thing with `AbortSignal`:
`abortSignal.addEventListener("abort", onAbort)` → `killProcessTree(child.pid)`
([`nodejs.ts:341-347`](../pi/packages/agent/src/harness/env/nodejs.ts)).
The channel-based approach is isomorphic: instead of firing a callback, we
send on a channel; the watchdog's `recv_timeout` serves as both the timeout
timer AND the cancel listener in one blocking call.

### 6.3 What steering gets from this

Once prompt cancellation is immediate:

- **Queuing is responsive:** calling `session.steer(msg)` can abort the current
  run immediately, even mid-tool-call, so the steering message is visible to the
  model on the very next LLM call — not after the current tool finishes.
- **No new concurrency bugs:** the cancellation path already exists; we just
  make it faster.  The `SteeringSession`/`Arc<Mutex<…>>` shared-state design
  from §2 is unchanged.
- **Safety:** the `drive_agent` loop already handles `PromptCancelled` by
  preserving `full_history()` ([`turn_cancellation_persistence_bugs.md`](
  ./turn_cancellation_persistence_bugs.md), §4 fix), so aborting mid-tool does
  not lose context.

### 6.4 Implementation plan (three phases)

The work splits into three phases: (1) immediate cancellation, (2) context
preservation, (3) steering.  Each must be completed before the next begins.

---

#### Phase 1 — Immediate cancellation (rig fork + `capture_bounded`)

**Goal:** `<Esc>` kills an in-flight tool command or LLM stream within
milliseconds, not after the command/stream finishes.

- [ ] **1a. `capture_bounded` — clone `cancel_tx` into a process-wide registry.**
  `src/tools.rs:439-532`.  Before spawning the watchdog, clone `cancel_tx` and
  push the clone into a `static TOOL_CANCEL_SENDERS: LazyLock<Mutex<Vec<mpsc::Sender<()>>>>`.
  Wrap the push in a drop-guard that pops on scope exit (so a cancelled tool
  doesn't retain a stale sender that could accidentally kill the *next* tool).

  ```rust
  struct CancelRegistration;
  impl Drop for CancelRegistration {
      fn drop(&mut self) {
          TOOL_CANCEL_SENDERS.lock().unwrap().pop();
      }
  }
  let _reg = CancelRegistration;
  ```

- [ ] **1b. `capture_bounded` — watchdog kills on `Ok(())` too.**
  Change the `recv_timeout` match to also kill on `Ok(())` (woken by a
  `send(())` from the in-flight reader).  The kill in the timeout branch
  already sets `timed_out`; the cancel branch can just kill without
  setting the flag — the caller detects cancellation via the read error
  and the absence of `timed_out`.

  ```rust
  match cancel_rx.recv_timeout(Duration::from_secs(secs)) {
      Err(RecvTimeoutError::Timeout) => {
          timed_out.store(true, Ordering::SeqCst);
          let _ = w.kill();
      }
      Ok(()) => {
          // woken by cancel sender — user pressed <Esc>
          let _ = w.kill();
      }
      Err(RecvTimeoutError::Disconnected) => {
          // all senders dropped — command finished, no kill
      }
  }
  ```

- [ ] **1c. `spawn_in_flight_reader` — send on tool-cancel senders.**
  `src/repl.rs:109-111`.  After `cancel.store(true, ...)`, iterate
  `TOOL_CANCEL_SENDERS` and `send(())` on each.  Sending on an unbounded
  `mpsc` channel is non-blocking, so this is safe from the reader thread.

  ```rust
  } else if let KeyCode::Esc = key.code {
      cancel.store(true, Ordering::SeqCst);
      for tx in crate::tools::TOOL_CANCEL_SENDERS.lock().unwrap().iter() {
          let _ = tx.send(());
      }
  }
  ```

- [ ] **1d. Make `TOOL_CANCEL_SENDERS` visible.**
  Declare it `pub(crate)` in `tools.rs`.  Wire the import in `repl.rs`.

- [ ] **1e. Rig fork — add `CancellationToken` to `AgentRunner` / streaming loop.**
  `crates/rig-core/src/agent/runner.rs` and `prompt_request/streaming.rs`.
  Add an `Option<CancellationToken>` field to `AgentRunner`.  In
  `drive_agent` (`streaming.rs:589`), after each `turn_stream.next()`,
  check if the token is cancelled and if so break with `PromptCancelled`.
  This catches the case where `<Esc>` fires during the LLM response
  streaming (before any tool call).

  Expose a builder method `.cancel_token(token)` on `AgentRunner` so
  picocode can pass it when constructing the agent.

- [ ] **1f. Rig fork — wire `CancellationToken` to provider HTTP client.**
  Optionally: register the token as a `reqwest::Request::abort()` handler
  so the TCP connection is dropped immediately rather than waiting for
  the full HTTP response.  Bonus; the token check in the streaming loop
  (1e) already catches cancellation on the next chunk boundary.

- [ ] **1g. Picocode — thread the cancel signal through to the rig token.**
  In `run_interactive` (or `CodeAgent::prompt`), create a
  `CancellationToken` and pass it to both the rig agent and the in-flight
  reader thread.  When `<Esc>` fires, the reader sets `cancel` and sends
  on tool channels (1c); the cancellation token is also triggered (either
  by the reader or by the `LoggingHook` seeing `cancel == true` and
  calling `token.cancel()`).

- [ ] **1h. Verify — compile and smoke-test.**
  `cargo build` succeeds.  In the REPL, `<Esc>` during:
  - `sleep 60` → kills the shell immediately (no 60-second wait).
  - A long thinking/reasoning turn → drops the stream before the model
    finishes.
  - A file read → no regression (fast tools complete normally).

---

#### Phase 2 — Context preservation

**Goal:** The session `history` survives all turn-ending paths; errors preserve
the user's prompt; model switches don't break tool-call pairing.

- [x] **2a. Preserve user prompt on non-cancellation errors.**
  `src/agent.rs:198-216`.  Before `return Err(...)`, construct a partial
  turn from the still-live locals (`input`, `output`) and commit it via
  `h.extend()`:
  ```rust
  if let Some(ref mut h) = history {
      let mut partial = vec![Message::user(input)];
      if !output.is_empty() {
          partial.push(Message::assistant(&output));
      }
      let to_commit = maybe_strip_reasoning(partial, …);
      let to_commit = normalize_call_ids(to_commit);
      h.extend(to_commit);
  }
  self.output.end_response();
  return Err(crate::PicocodeError::Other(e.to_string()));
  ```
  This ensures a subsequent `prompt()` or steering message sees the
  user's input (and any partial assistant text) rather than having it
  silently dropped.

- [x] **2b. Normalise `call_id` at persist time for cross-provider safety.**
  Added `normalize_call_ids()` in `src/agent.rs:308-334`, a helper that
  walks committed messages and sets `call_id = Some(id)` on every
  `ToolResult` where it's `None`.  Wired into both branches of
  `persist_turn` (append and replace).  Also applied in the error-preservation
  path (2a) before the `extend()`.

  This fixes the OpenAI Responses API `require_call_id()` gap
  (§6.5.4) without needing pi's `transformMessages` layer.  The tool
  call + tool result pair stays valid because both use the same string.

- [ ] **2c. Confirm cross-provider switching works.**
  Test `/model` from `anthropic:claude-sonnet-4` to `openai:gpt-4o` and
  back.  The conversation history (including tool-call turns) should
  survive.  No `require_call_id` errors.

- [ ] **2d. Verify error-preservation paths.**
  - Force a network error (disconnect WiFi during a prompt) → the
    user's input is present in the next turn.
  - Normal successful turn → no regression (still appends once).
  - `<Esc>` cancellation → no regression (still replaces with
    `full_history`).

#### Learnings from implementation

- **`persist_turn` not reused in the error path.** The doc sketch called
  `persist_turn(&mut history, Some(partial), None, …)`, but the actual
  code constructs the partial turn from `input`/`output` locals and calls
  `maybe_strip_reasoning` + `normalize_call_ids` inline before extending.
  This is because `persist_turn`'s `final_history` branch appends the turn
  as-is, but reasoning stripping and call-id normalisation must also happen
  — and the code already has `self.reasoning_in_context`, `self.provider`,
  etc. accessible.  The inline approach is clearer than threading four
  extra args through `persist_turn` for this single call site.

- **Error path uses `vec![Message::user(input)]`, not `Vec::clone(h)`.**  
  Unlike the cancel path (which copies the full prior history because the
  `cancel_notify` branch fires before the `PromptCancelled` error arrives),
  the error path constructs a minimal turn from only the current `input`
  and `output`.  This is safe because `history` already holds the prior
  turns — they are not lost — so the partial turn is just appended to
  them.  Using `Vec::clone(h)` would duplicate the priors inside the
  partial, creating double-entry on every error.

- **`normalize_call_ids` only touches `Message::User`.**  Tool results in
  rig live inside `UserContent::ToolResult(...)` (the tool's output is
  returned as a user-role message).  `AssistantContent` never carries a
  `ToolResult`, so the helper only iterates `Message::User` variants.
  Assistant messages pass through unchanged.

- **Phase 2 does not actually depend on Phase 1.**  The doc claimed
  "cancellation path is also an error path; same `persist_turn` code,"
  but the error-preservation path (2a) operates on the non-cancellation
  error branch — it has no dependency on the tool-cancel channel or rig
  fork changes.  Phase 2 can be implemented and tested independently of
  Phase 1.

---

#### Phase 3 — Steering (responsive messaging)

**Goal:** `SteeringSession` with `PendingMessageQueue`, driven by a re-drive
loop (§3).  *Needs UI design work (stdin sharing, thread-safe queue access,
TUI integration) — deferred.*

- [ ] **3a. Design: UI-side steering interface.**
  - How does the user trigger a steering message?  (e.g. type while the
    agent is running, hit `<Enter>` to queue)
  - How does the REPL share stdin between the in-flight reader and the
    line editor?  (Steering text arrives while `prompt().await` is
    running — the in-flight reader currently drops non-`Esc` keys.)
  - Where does the `SteeringSession` live?  (`Arc<Mutex<…>>` shared
    between the UI thread and the driver, §2.)

- [ ] **3b. Implement `PendingMessageQueue` + `SteeringSession`.**
  Rust translation of pi's `PendingMessageQueue` (copy §2).  Owns the
  canonical `conversation`, two queues, `steer()`/`follow_up()` methods.

- [ ] **3c. Implement the re-drive loop (`drive`).**
  The outer loop from §3: drain steering → drain follow-up →
  `agent.runner(batch).history(conversation)`.  Record every stream item
  into `conversation`.

- [ ] **3d. Wire into `run_interactive`.**
  Replace the single `agent.prompt(&input, &mut history)` call with the
  re-drive loop.  The in-flight reader must still work — `<Esc>` should
  cancel the current run (Phase 1), and steering text from the UI should
  appear on the next iteration.

- [ ] **3e. Test.**
  - Queue a steering message during a `sleep 30` → the sleep is killed
    (Phase 1) and the steering message is the next thing the model sees.
  - Queue a follow-up → only delivered after the model stops issuing
    tool calls.
  - `QueueMode::OneAtATime` vs `QueueMode::All`.

---

### 6.5 Context preservation across cancellation, errors, and model switches

Because steering queues a message for injection at the *next* LLM call, the
session `history` must survive whatever interrupts the current turn — otherwise
the steering message lands in an empty conversation and the model has no context.

This section audits every path that can terminate a turn and what happens to
`history`.

#### 6.5.1 Successful completion (ideal path)

`prompt()` receives `FinalResponse` → `final_history` = `[user, assistant]` for
this turn only. `persist_turn` **appends** it to the session `history` (the
§4/root-cause-#1 fix, `agent.rs:293-295`). Result: prior turns + new turn.

#### 6.5.2 Cancellation (`<Esc>` / hook `Stop`/`Terminate`)

The streaming loop catches `PromptError::PromptCancelled { chat_history, .. }`
(`agent.rs:189-194`). rig's `chat_history` = `full_history()` = prior turns +
this turn's user prompt + any partial assistant text (`run/mod.rs:573`).
`persist_turn` **replaces** `history` with `chat_history` (the §4/root-cause-#2
fix, `agent.rs:296-298`), avoiding duplication of the priors.

**Status: context preserved.** The abort commits the partial turn and the
steering message that follows sees the full conversation.

#### 6.5.3 Non-cancellation streaming errors

Fixed in Phase 2a (`src/agent.rs:198-216`).  Any `StreamingError` that is *not*
`PromptCancelled` (network timeout, API error, malformed response, provider 5xx)
now commits the partial turn before returning the error:

```rust
if let Some(ref mut h) = history {
    let mut partial = vec![Message::user(input)];
    if !output.is_empty() {
        partial.push(Message::assistant(&output));
    }
    let to_commit = maybe_strip_reasoning(partial, …);
    let to_commit = normalize_call_ids(to_commit);
    h.extend(to_commit);
}
self.output.end_response();
return Err(crate::PicocodeError::Other(e.to_string()));
```

The `input` and `output` locals are still live at the error site — `input` is the
user's original prompt (passed to `stream_prompt` at line 103), `output` is any
partial assistant text accumulated during streaming (lines 136–138).  The partial
turn is appended to the existing session `history`, so prior turns survive intact.

**What about partial assistant text being garbled?**  It may be mid-sentence or
contain incomplete JSON from a failed tool-call parse.  Including it is still
strictly better than dropping it: the model can infer intent from context, and
the alternative (zero assistant text) makes the error look like a total loss.
The `reasoning_in_context` / `maybe_strip_reasoning` logic applies as usual, so
reasoning blocks embedded in the partial text are stripped per policy.

**What about duplicate user messages on retry?**  If the user's next input is
the same text, it will appear twice in history.  This is harmless — the model
has seen the same instruction before — and far better than losing the original.

**One caveat:** if the error occurred *during* tool execution (the tool ran but
its result delivery failed), the tool call + partial result are not captured by
`output` (which only holds assistant text).  The user prompt alone is still
preserved, so the model sees the question but not the tool's (possibly
successful) output.  This is safe: the steering re-drive loop (§3) would re-run
the tool anyway because the tool result is not in the canonical `conversation`.
The impact is a wasted tool call, which is acceptable for the error-recovery
path.

**Status: fixed.** Prior turns preserved, user prompt + partial text committed.

#### 6.5.4 Model / provider switch (`/model`, `/thinking`, `/persona`)

These commands call `create_agent(new_cfg)` which builds an entirely new
`CodeAgent<M'>` with a different generic type (`agent.rs:1015-1017`). The
session `history: Vec<Message>` in `run_interactive` is **untouched**
(`agent.rs:964`). The next `prompt()` passes it via `.history(prior)`.

**Status: conversation survives the switch.** Three cross-provider subtleties
apply:

1. **Tool call IDs.** rig's `ToolCall` has two fields: `id` (the provider-issued
   ID, e.g. `toolu_abc` for Anthropic) and `call_id` (a normalized internal ID,
   set by the provider adapter during serialization). When the provider changes:

   **How pi handles this:** pi calls `transformMessages()` on every turn
   ([`agent-loop.ts:295`](../pi/packages/agent/src/agent-loop.ts)), passing a
   provider-specific `normalizeToolCallId` callback.  The callback transforms
   tool call IDs in the whole conversation at once, building a
   `toolCallIdMap` ([`transform-messages.ts:69-70`](../pi/packages/ai/src/api/transform-messages.ts))
   that remaps both the tool call and its paired tool result — so the pair
   stays valid regardless of the target provider's ID format.  Each provider
   adapter passes its own normalizer:
   - **Anthropic** (`anthropic-messages.ts`): strips non-`[a-zA-Z0-9_-]`,
     caps at 64 chars.
   - **OpenAI Chat** (`openai-completions.ts`): splits on `|`, takes the
     first part, caps at 40 chars.
   - **OpenAI Responses** (`openai-responses-shared.ts`): preserves the
     `callId|itemId` format, hashes foreign item IDs with a `fc_` prefix
     via `buildForeignResponsesItemId`.

   **How rig handles it (no transform layer):** Each provider adapter
   normalizes IDs independently at serialization time, looking only at the
   fields on the individual message:
   - **OpenAI Chat** (`completion/mod.rs:919`): `call_id.unwrap_or(id)`.
     An Anthropic-origin tool call with `call_id=None` and
     `id="toolu_abc"` is sent as `{"id": "toolu_abc", ...}` — the tool
     result likewise uses `call_id.unwrap_or(id)` (`completion/mod.rs:624`),
     so the pair stays paired.
   - **OpenAI Responses** (`responses_api/mod.rs:454-472, 616-631`):
     **requires** `call_id` via `require_call_id()` on both tool calls and
     tool results.  An Anthropic-origin message with `call_id=None` would
     **fail** with a request error.
   - **Anthropic** uses the `id` verbatim — no constraint on `call_id`.

   **Current status for picocode:** picocode uses `openai::Client` which
   targets the Chat Completions API (not Responses API), so the `call_id`
   gap was **theoretical** — it worked before.  The mitigation described
   below is now implemented proactively (Phase 2b).

   **Mitigation implemented — `normalize_call_ids` at persist time**
   (`src/agent.rs:308-334`): Before committing any turn to the canonical
   `conversation`, every `ToolResult` is normalized so that a missing
   `call_id` is set to `Some(id)`.  This is simpler than pi's
   `transformMessages` because rig already pairs calls with results in the
   run — no need to track pending calls.  The helper is wired into both
   branches of `persist_turn` and into the error-preservation path (2a).

   **Summary:**

   | Provider pair | pi (transformMessages) | rig (serialization-only) |
   |---------------|------------------------|--------------------------|
   | Anthropic → Anthropic | Identity | Identity |
   | Anthropic → OpenAI Chat | `normalizeToolCallId` strips `|` etc. | `call_id.unwrap_or(id)` — works |
   | Anthropic → OpenAI Responses | `normalizeToolCallId` rewrites to `callId\|itemId` | **FAILS** — `call_id` required |
   | OpenAI Responses → Anthropic | `normalizeToolCallId` strips `|`, caps 64 | `call_id.unwrap_or(id)` — works |
   | OpenAI Responses → OpenAI Chat | `normalizeToolCallId` splits on `|`, takes first | `call_id.unwrap_or(id)` — works |

2. **Reasoning blocks.** `maybe_strip_reasoning` (`agent.rs:309-339`) is called
   at *persist time* with the *current* provider/model. Reasoning from an
   Anthropic tool-call turn is force-kept. After switching to OpenAI, those
   reasoning blocks sit in `history` as `AssistantContent::Reasoning(...)`. rig's
   OpenAI adapter strips them at wire-translation time (`responses_api/mod.rs:633-641`,
   `completion/mod.rs` handles reasoning per OpenAI's format), so they are sent
   as `reasoning` items or dropped. No rejection, but the reasoning tokens are
   still counted in the prompt — a minor efficiency cost.

3. **Provider-specific content** (e.g. Anthropic `server_tool_use`, OpenAI
   `file_search` call results): these are not emitted by picocode's agent (it
   only uses `Bash`, `ReadFile`, `WriteFile`, etc.), so this is not a concern
   here.

**Status: conversation survives.  The OpenAI Responses API `call_id` gap is
mitigated by `normalize_call_ids` at persist time (§2b).**

#### 6.5.5 Summary of context preservation

| Scenario | Prior turns kept? | Current turn kept? | Next prompt sees? |
|----------|-------------------|---------------------|-------------------|
| Successful completion | ✓ (append) | ✓ (committed) | Full history |
| `<Esc>` cancellation | ✓ (replaced) | ✓ (partial committed) | Full history incl. partial turn |
| Network/API error | ✓ (no mutation) | ✓ (user prompt + partial text committed) | Prior turns + failed prompt |
| `/model` switch (Chat Completions) | ✓ (unmodified) | N/A | Prior turns from old provider — works |
| `/model` switch (Responses API) | ✓ (unmodified) | N/A | Call_id gap **fixed** via `normalize_call_ids` at persist time (§2b) |

---

## 7. Summary — phased implementation

| Phase | What | Status | Depends on |
|-------|------|--------|------------|
| **1 — Immediate cancellation** | `capture_bounded` watchdog channel wiring (§6.2.2), rig fork `CancellationToken` (§6.2.1), in-flight reader plumbing (§1c–1g) | ❌ Not started | — |
| **2 — Context preservation** | User prompt on errors (§6.5.3 fix), `call_id` normalisation (§2b), cross-provider verification (§2c–2d) | ✅ 2a–2b coded; 2c–2d need manual test | (see note) |
| **3 — Steering** | `PendingMessageQueue` + `SteeringSession` (§2), re-drive loop (§3), UI design (stdin sharing, TUI integration) | ❌ Not started | Phase 1 + 2 |

Implementing `pi`-style steering on rig is mostly *orchestration* — rig already
provides every primitive `pi` had to build by hand.  The provider-agnostic
`Message` enum, the `AgentHook` system with `RequestPatch`, the runner that
always pairs `ToolCall` ↔ `ToolResult`, and per-provider adapters that
normalize IDs and handle reasoning are all present.

## 8. Key `rig` references (from the `~/.cargo` copy)

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
