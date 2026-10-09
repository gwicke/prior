# Upgrade rig-core 0.28.0 -> 0.38.2

Goal: move to the `0.38.x` line. Latest in line = `0.38.2`. This is a large
minor-version jump (10 minor versions) and includes several breaking API
changes in rig-core / rig-derive.

## Snapshot

The pre-upgrade `Cargo.lock` was snapshotted by the user (current `Cargo.lock`
already has the 0.28 pin reverted to a working state: `rig-derive = "=0.1.10"`).

## Dependency changes (already applied to Cargo.toml / Cargo.lock)

- `rig-core` `0.28.0` -> `0.38` (resolves to `0.38.2`)
- `rig-derive` `=0.1.10` -> `0.38` (resolves to `0.38.2`)
- `rig-core 0.38.2` re-exports `schemars` (`pub use schemars;`) and
  `rig_derive::{Embed, rig_tool as tool_macro}`. This is the key fix for the
  earlier `rig_tool` helper-attribute break: the macro emits
  `#[derive(... ::rig_core::schemars::JsonSchema)]` +
  `#[schemars(crate = "::rig_core::schemars")]`, which only works because
  `rig_core::schemars` now exists.
- `rig-core 0.38.2` requires `schemars ^1.0.4`, `reqwest ^0.13` (project keeps
  its own `reqwest 0.12`; both coexist). Single `schemars 1.2.1` is shared.

## API changes that affect this codebase

### 1. `ProviderClient::from_env()` is now fallible

Old: `fn from_env() -> Self`. New: `fn from_env() -> Result<Self, Self::Error>`
where `Self::Error = rig::client::ProviderClientError` for all standard
providers.

Fix: unwrap at every call site with `?` (inside `create_agent`, which returns
`Result<...>`), and add `From<ProviderClientError>` to `PicocodeError`.

### 2. `Agent<M>` -> `Agent<M, P>` (hook carried in the type)

`Agent<M>` still works via default `P = ()`, and `()` implements `PromptHook`.
The per-request `with_hook(...)` still overrides `P` on the prompt request, so
`CodeAgent<M>` can keep wrapping `Agent<M>` and apply `LoggingHook` per call.

### 3. `PromptHook` trait changed (RPITIT + `HookAction`, no `CancelSignal`)

- All methods now return `impl Future<Output = HookAction | ToolCallHookAction>`
  (implemented with `async fn`).
- `on_completion_call(&self, prompt: &Message, history: &[Message]) -> HookAction`
- `on_completion_response(&self, prompt: &Message, response: &CompletionResponse<M::Response>) -> HookAction`
- `on_tool_call(&self, tool_name: &str, tool_call_id: Option<String>, internal_call_id: &str, args: &str) -> ToolCallHookAction`
  (gained `internal_call_id`)
- `on_tool_result(&self, tool_name: &str, tool_call_id: Option<String>, internal_call_id: &str, args: &str, result: &str) -> HookAction`
  (gained `internal_call_id`)
- New optional streaming hooks (`on_text_delta`, etc.) have defaults; not
  needed.
- `CancelSignal` is gone. Hooks now control flow by returning `HookAction`:
  `Continue` or `Terminate { reason }` (and `ToolCallHookAction::Continue |
  Skip { reason } | Terminate { reason }`).

Migration of cancel semantics: the old code called `cancel_sig.cancel()` to
abort the in-flight request when the user pressed Esc. That mid-request abort is
no longer exposed. Replace with returning `HookAction::Terminate { reason }`
from the hook callbacks when `user_cancel` is set. Behavior: Esc stops the
agentic loop at the next checkpoint (after the current completion/tool step)
instead of aborting the HTTP request mid-flight. Acceptable degradation; note in
code comment.

### 4. Prompt request builder method renames

- `.multi_turn(n)` -> `.max_turns(n)`
- `.with_history(h)` unchanged signature
  (`H: IntoIterator<Item: Into<Message>>`); `&mut Vec<Message>` still works.
- `.with_hook(hook)` unchanged (overrides per-request hook).
- `.await` on the standard `PromptRequest` now yields
  `Result<String, PromptError>` directly (previously `Result<PromptResponse, _>`
  and we called `.to_string()`). So `Ok(response)` instead of
  `Ok(response.to_string())`.

### 5. `Tool` trait unchanged shape (RPITIT)

`const NAME`, `type Args/Output/Error`,
`definition(&self, prompt: String) -> impl Future<ToolDefinition>`,
`call(&self, args) -> impl Future<Result<Output, Error>>`. Our `Guard<T>` and
the manual `Bash` impl use `async fn`, which remains compatible.
`ToolDefinition { name, description, parameters }` fields unchanged.

### 6. Re-exports still present

`rig::agent::{Agent, AgentBuilder, PromptHook}`,
`rig::client::{CompletionClient, ProviderClient}`,
`rig::completion::{CompletionModel, Prompt, ToolDefinition, CompletionResponse, Message}`,
`rig::message::Message` (still aliased via `pub use completion::message`),
`rig::providers::{...}` (all 18 used providers exist), `rig::tool::Tool`.
`CompletionClient::agent(model)` unchanged.

## Files to change

- `Cargo.toml` — done (rig-core 0.38, rig-derive 0.38).
- `src/lib.rs` — add `ProviderClient(#[from] rig::client::ProviderClientError)`
  variant.
- `src/agent.rs` —
  - drop `CancelSignal` from imports; import `HookAction`, `ToolCallHookAction`.
  - rewrite `LoggingHook` `PromptHook` impl for new signatures + `HookAction`.
  - `prompt()`: `max_turns`, `with_history`, `Ok(response)` (no `.to_string()`).
  - `create_agent`: `build!(X::Client::from_env()?)` for every provider.

## Status

- Cargo.toml / lock: rig-core 0.38.2, rig-derive 0.38.2 (done).
- src/lib.rs: added ProviderClient variant (done).
- src/agent.rs + src/tools.rs + src/lib.rs: `rig::` -> `rig_core::` (lib rename
  0.28 `rig` -> 0.38 `rig_core`) (done).
- src/agent.rs: imports, LoggingHook PromptHook impl, prompt(), from_env()?
  (done).
- Build: clean, no warnings. `cargo test`: 72 passed. `cargo run -- --help`: OK.

## Behavior notes

- Esc-to-cancel now terminates the agentic loop at the next checkpoint
  (rig 0.38 removed the mid-request `CancelSignal`). Before it aborted the
  in-flight HTTP request immediately.
- Two `reqwest` versions coexist (project 0.12, rig-core 0.38 0.13); harmless.
