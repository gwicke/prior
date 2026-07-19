# Turn cancellation & conversation-history persistence bugs

Investigation into: *"aborting via `<esc>` seems to completely wipe context, so
subsequent submissions have no reference to the earlier conversation."*

This document records the root-cause analysis only. No code was changed; fixes
are **proposed** at the end.

All upstream references are to the patched rig fork pinned in `Cargo.toml`
(`rig-core = { git = ".../rig.git", rev = "87f3f5b7" }`), located at
`~/.cargo/git/checkouts/rig-794bcfddf11aa804/87f3f5b/crates/rig-core`.

---

## 1. How conversation context is threaded today

- `run_interactive` (`src/agent.rs:657`) owns a single session-wide
  `history: Vec<Message>`. Each user submission calls
  `prompt(&input, Some(&mut history))` (`src/agent.rs:807` / `:816`).
- `prompt()` (`src/agent.rs:44`):
  1. clears the shared cancel flag (`src/agent.rs:52`),
  2. **clones** the current history into `prior` (`src/agent.rs:56`),
  3. hands `prior` to rig via `.history(prior)` (`src/agent.rs:68`),
  4. drives the multi-turn stream, recording the `FinalResponse`'s messages into
     `final_history` (`src/agent.rs:101`),
  5. writes the result back to the caller's `history` (the "§2.4 fix",
     `src/agent.rs:110-116`).

The write-back is the only place the session `history` is mutated:

```rust
// src/agent.rs:110-116
// Persist history across turns (§2.4 fix): `history` is cloned into
// the request, so we must write the updated history back here.
if let Some(h) = history {
    if let Some(fh) = final_history {
        *h = fh;           // <-- REPLACE, not append
    }
}
```

---

## 2. Root cause #1 — the write-back replaces instead of appends

This is the primary cause of "subsequent submissions have no reference to the
earlier conversation." It is **not** abort-specific; it loses context on every
successful turn.

### What rig returns

In the rig fork, a run is constructed with the prior history and the new prompt
kept in **separate** fields:

- `AgentRun::new` seeds `new_messages = vec![prompt.into()]`
  (`rig-core/src/agent/run/mod.rs:332`).
- `AgentRun::with_history` sets `chat_history = Some(history)`
  (`rig-core/src/agent/run/mod.rs:344`) — distinct from `new_messages`.

On a normal `Done`, the `FinalResponse` is built from `new_messages` only:

- `rig-core/src/agent/run/mod.rs:731` and `:802` →
  `.with_messages(self.new_messages.clone())`.
- `rig-core/src/agent/prompt_request/streaming.rs:1475` →
  `final_item` surfaces `Some(response.messages.clone() …)` (the current turn
  only; `chat_history` is **not** included).

So `resp.messages()` returns just `[user, assistant]` for *this* prompt — it does
**not** contain the prior conversation.

### The mismatch

picocode's write-back does `*h = fh`, i.e. it **overwrites** the whole
conversation with only the latest turn. After every successful turn, all earlier
turns are dropped, so the model only ever sees the immediately preceding
exchange.

rig's own blocking API uses the correct contract — **append**, not replace:

```rust
// rig-core/src/agent/completion.rs:683-691  (blocking chat())
let response = PromptRequest::from_agent(self, prompt)
    .history(chat_history.clone())
    .extended_details()
    .await?;

if let Some(messages) = response.messages {
    chat_history.extend(messages);   // APPEND, not replace
}
```

picocode's streaming path diverges from that contract, which is the structural
source of the context loss.

---

## 3. Root cause #2 — what `<Esc>` actually does

`<Esc>` is caught by the background in-flight reader thread
(`src/repl.rs:107`) which sets the shared `cancel` `AtomicBool`. The
`LoggingHook` (`src/agent.rs:397-459`) returns `Stop` at every checkpoint
(completion call, completion response, model-turn finished, tool call, tool
result) whenever `user_cancel` is set.

In the rig streaming driver, **every** `Stop`/`Terminate` path terminates the
run as a terminal **error** and never emits a `FinalResponse`:

- `rig-core/src/agent/prompt_request/streaming.rs:521-524`
  (`CompletionCallOutcome::Terminate` → `yield Err(…cancel_error…); break`).
- `rig-core/src/agent/prompt_request/streaming.rs:1372-1392`
  (`ModelTurnAction::Stop` → `yield Err(…cancel_error…); return`).
- Same pattern for the observation / tool-call / tool-result `Stop` outcomes.

Because the stream yields `Err`, picocode's loop early-returns:

```rust
// src/agent.rs:74-75
while let Some(item) = stream.next().await {
    match item.map_err(|e| crate::PicocodeError::Other(e.to_string()))? {
```

The `?` propagates the error out of `prompt()`, so the write-back block
(`src/agent.rs:110-116`) is **never reached**.

Consequences of an abort:

- The aborted user prompt **and** any partial assistant text are never committed.
- Because the write-back is skipped, `history` keeps its previous value. Abort
  therefore does **not** wipe the *prior* turns; it only discards the in-flight
  one.

### Why abort *appears* to "completely wipe context"

The two bugs compound:

- Under root cause #1 the conversation is already a sliding window of a single
  turn. After an abort, the exchange you were actively working on (the most
  recent, most relevant context) is gone, and earlier turns were already being
  dropped — so continuity feels entirely lost.
- Literal total-wipe edge case: if you abort the **very first** prompt,
  `history` stays empty, so the next submission has **zero** context.

Note: rig's cancellation error actually carries the in-flight full history —
`PromptError::prompt_cancelled(self.full_history(), reason)`
(`rig-core/src/agent/run/mod.rs:573`) where `full_history()` = `chat_history +
new_messages`. picocode throws that away by mapping it to a plain
`PicocodeError::Other(e.to_string())` (`src/agent.rs:75`), losing the structured
history it could have persisted.

---

## 4. Proposed fixes (not implemented)

1. **Fix the persistence contract (root cause #1).** In `src/agent.rs:112-116`,
   append the returned turn to the existing history instead of replacing it,
   mirroring rig's blocking `chat()`:

   ```rust
   if let Some(h) = history {
       if let Some(fh) = final_history {
           h.extend(fh);   // prior + this turn, instead of *h = fh
       }
   }
   ```

   `fh` already includes the user message + assistant turns for this prompt, so
   `extend` yields the correct full history. This alone fixes cross-turn context
   loss and also means an abort (which skips the write-back) preserves everything
   up to the last successful turn.

2. **Make `<Esc>` preserve continuity (root cause #2).** Stop swallowing the
   cancellation error. Catch the `PromptError::prompt_cancelled { history, .. }`
   specifically, extract its `history` (the in-flight `full_history()`), and
   persist it (e.g. via `extend`) so the user can refine the aborted turn. Decide
   on the desired UX:
   - commit the partial turn (user prompt + partial assistant text) so the
     conversation continues seamlessly, **or**
   - at minimum commit the user's prompt so a first-turn abort isn't a total
     loss, and surface a clear "aborted" message instead of a generic `Error:`.

3. **Hygiene.** `user_cancel` is reset at the start of each `prompt()`
   (`src/agent.rs:52`), which already prevents a stale cancel from leaking into
   the next turn. Add a comment / test documenting that the in-flight reader's
   `done` flag and `cancel` are mutually consistent across abort → next prompt.

4. **Tests.** There are currently no tests asserting streaming-history
   persistence (`tests/` has none referencing `history`/`messages`/`FinalResponse`
   /abort). Add a regression test that:
   - runs two successful turns and asserts `history` contains both turns (catches
     root cause #1), and
   - aborts a turn and asserts the prior turns remain and the new prompt is at
     least retained (catches root cause #2).

---

## 5. Key references

picocode (`src/agent.rs`):
- `prompt()` body: `:44-119`
- cancel-flag reset: `:52`
- `prior` clone: `:56`
- `history(prior)`: `:68`
- `final_history` recorded: `:101`
- write-back (replace): `:110-116`
- `run_interactive` `history` owner: `:657`; `prompt(&input, Some(&mut history))`: `:807`, `:816`
- `LoggingHook` `Stop` outcomes: `:397-459`

picocode (`src/repl.rs`):
- in-flight reader sets `cancel` on `<Esc>`: `:107`

rig fork (`~/.cargo/git/.../rig-core`):
- blocking `chat()` extends history: `src/agent/completion.rs:683-691`
- `AgentRun::new` / `with_history`: `src/agent/run/mod.rs:300-346`
- `Done` builds `response.messages = new_messages` (current turn only):
  `:731`, `:802`
- `prompt_cancelled` carries `full_history()`: `:573`
- streaming driver yields `Err` on every `Stop`/`Terminate` (no `FinalResponse`):
  `src/agent/prompt_request/streaming.rs:521-524`, `:1372-1392`
- `final_item` surfaces current-turn `response.messages`: `:1455-1480`
