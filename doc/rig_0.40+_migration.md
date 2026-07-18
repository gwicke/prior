# Rig 0.40+ migration assessment

Status: **Implemented** against the unreleased git HEAD (`87f3f5b7`, 2026-07-18) via
`[patch.crates-io]` → `https://github.com/gwicke/rig.git` (rev `87f3f5b7`), with
`rig-core`/`rig-derive` bumped to `0.40`. Goal: upgrade `rig-core` from `0.38.2`
to the unreleased HEAD to fix the "reasoning-only turns look like missing
responses" issue more cleanly, and pick up the refined hooks/tools/caching API.
Findings below are grounded in the actual `rig/` source.

**Two references were checked:** the released tag `v0.40.0`
(`2f37dfcd`, released 2026-07-11) and the current `HEAD` (post-0.40, includes
significant changes). The **benefits** (§1–§3) are all present in the *released*
`v0.40.0`. The **migration API** in §4 was first written against `HEAD`; the
concrete divergences for the *released* `v0.40.0` are called out in §4.6 — they
are material (the hook API and history-passing differ), so if we target the
released 0.40.0 the migration is a different shape than the HEAD plan.

## 1. Verdict

- **Upgrading fixes the root cause of the degenerate reasoning loop for
  OpenAI-compatible providers automatically.** 0.40's provider message
  conversion *drops assistant turns that have neither text nor tool calls*
  (i.e. pure-reasoning turns) when history is replayed. That is exactly what
  was causing the model to see its own prior reasoning and keep emitting
  reasoning-only turns — so real text responses come back. This is cleaner than
  manually stripping reasoning from history, because rig does it per-provider
  and only for pure-reasoning turns (reasoning attached to a text/tool-call
  turn still round-trips).
- **It does NOT change how picocode *displays* a reasoning-only turn.** Such a
  turn is still rendered dimmed (TTY) / on stderr (non-TTY) when it does occur
  (e.g. Anthropic, which keeps `thinking` blocks). The earlier option (B) —
  render reasoning-only turns as the visible answer — is still relevant for that
  residual case.
- **Context caching is preserved and slightly improved.** Anthropic automatic
  caching remains; OpenAI Responses API `prompt_cache_key` /
  `prompt_cache_retention` exist in `AdditionalParameters`
  (`responses_api/mod.rs:1431`/`1434` in **both** `v0.40.0` and HEAD — set via
  `additional_params`); streaming usage is generic so DeepSeek/Mistral cache
  counters survive streaming. **(Correction to an earlier verbal summary: these
  Responses-API params are in the released 0.40.0, not post-0.40 — they sit at
  CHANGELOG line 137, which is inside the `[0.40.0]` section whose header is at
  line 127, not in `[Unreleased]`.)**
- **Breaking changes are moderate but mechanical** (call-site renames,
  `PromptHook` → `AgentHook`, `Tool::call` signature, `FinalResponse` →
  `PromptResponse`, new `MultiTurnStreamItem` variants). No architectural
  redesign of picocode is required.

## 2. How 0.40 fixes the reasoning/history issue

### Root cause (recap from `doc/streaming_mode.md` §Phase 5)
picocode persists `resp.history()` (rig `Message`s that include `Reasoning`)
and feeds it back next turn via `with_history(...)`. The model therefore sees
its own prior reasoning and, after a reasoning-heavy turn, continues reasoning
instead of emitting a text reply → a reasoning-only turn that reads as "no
response."

### What 0.40 changes
The shared OpenAI-compatible history sanitizer **removes reasoning content and
then drops assistant messages left without text**:

- `sanitize_plain_text_history` (`rig/crates/rig-core/src/providers/openai/completion/mod.rs:1661`)
  does `object.remove("reasoning_content")` and `object.remove("tool_calls")`
  for assistant messages, then `retain`s only assistant messages whose
  `content` is a non-empty string. A pure-reasoning turn (content empty,
  reasoning in `reasoning_content` which was stripped) is therefore dropped.
- CHANGELOG (`rig/CHANGELOG.md`) line 228: *"(moonshot) reasoning-only assistant
  history turns are no longer preserved: the shared conversion drops assistant
  messages with neither text nor tool calls. Reasoning attached to text or
  tool-call turns still round-trips via `reasoning_content`."* The same shared
  helper sanitizes perplexity / mira / hyperbolic / openrouter.

So for **OpenAI-compatible providers** (openai, deepseek, groq, mistral,
together, moonshot, perplexity, hyperbolic, mira, azure, huggingface,
openrouter), a reasoning-only turn is **not** sent back to the model. The loop
stops without any picocode-side reasoning stripping.

### Scope / caveat
- **Anthropic keeps `thinking` blocks** (reasoning round-trips as part of the
  API), so the loop may *persist* for Anthropic unless we also strip reasoning
  from history there. The earlier "strip reasoning from persisted history"
  option is still worth adding as a uniform toggle (cheap, and it also cuts
  history token cost).
- The fix addresses the *loop*, not the *display*: when a reasoning-only turn
  still happens, picocode still shows it dimmed (TTY) / on stderr (non-TTY). If
  we want those shown as the answer, implement option (B) from the Phase 5
  discussion (render the turn's reasoning as the visible response when no text
  was emitted).

## 3. Context caching in 0.40

| Capability | 0.38 | 0.40 | Notes |
|---|---|---|---|
| Anthropic automatic caching | `with_automatic_caching()` | `with_automatic_caching()` + `with_automatic_caching_1h()` (`anthropic/completion.rs:1649`, `:1665`) | Preserved; up to 4 `cache_control` markers. |
| OpenAI chat / OpenRouter prompt caching | `with_prompt_caching()` | same (`openrouter/completion.rs:1514`, `anthropic/completion.rs:1610`) | Preserved. |
| OpenAI **Responses API** cache params | not present | `AdditionalParameters.prompt_cache_key` / `prompt_cache_retention` (`openai/responses_api/mod.rs:1659`) | From #1830. No typed builder; set via `AgentBuilder::additional_params(serde_json::to_value(AdditionalParameters { prompt_cache_key: Some(...), prompt_cache_retention: Some(...), .. })?.into())`. The "draft a wrapper" idea is feasible through `additional_params`. |
| Streaming usage shape | narrowed to OpenAI `Usage` | generic over provider payload (`StreamingCompletionResponse<U>`) | DeepSeek cache hit/miss + Mistral cached-token counters survive streaming (CHANGELOG line 222). |

Our thinking-level mapping already uses `AgentBuilder::additional_params`, so
adding Responses-API cache params is purely additive.

## 4. Breaking changes affecting picocode

> **Scope note.** The subsections below (§4.1–§4.5) describe the API at **HEAD**
> (post-0.40). The **released `v0.40.0`** differs in three material ways — see
> **§4.7** for the exact `v0.40.0` shape. If we target the released tag (which is
> what the question is about), the migration is a *different* (and in some spots
> smaller) change than the HEAD plan.

### 4.1 Agent execution call-site (rename, mechanical)
`StreamingPromptRequest` keeps `history()`, `max_turns()`, `add_hook()` and
`IntoFuture` (`streaming.rs:299`, `:319`, `:342`, `:1565`), but the old names
are renamed:

```rust
// 0.38
let mut stream = self.agent.stream_prompt(input)
    .with_hook(LoggingHook { .. })
    .multi_turn(self.tool_call_limit)
    .with_history(prior)
    .await;

// HEAD (0.40+, equivalent)
let mut stream = self.agent.stream_prompt(input)
    .add_hook(LoggingHook { .. })          // with_hook -> add_hook
    .max_turns(self.tool_call_limit)       // multi_turn -> max_turns
    .history(prior)                        // with_history -> history
    .await;
// or the recommended runner form:
let mut stream = self.agent.runner(input)
    .add_hook(LoggingHook { .. })
    .history(prior)
    .max_turns(self.tool_call_limit)
    .stream()
    .await?;
```

`AgentRunner::stream` returns the same `Stream<Item = Result<MultiTurnStreamItem<R>, StreamingError>>`
(`runner.rs:1485`), and `StreamingChat::stream_chat` is also available
(`completion.rs:~720`) but takes history by value (no caller mutation), so our
manual history write-back stays.

> **Released `v0.40.0` differs:** `StreamingPromptRequest` has **no** `history()`
> / `with_history` method. Pass history via `agent.stream_chat(input, prior)`
> (the `StreamingChat` trait), which internally calls `.history(chat_history)`.
> So the 0.40.0 call-site is `stream_chat(input, prior).add_hook(h).max_turns(n).await`
> — `stream_prompt(...).with_history(...)` does not compile on 0.40.0.

### 4.2 Final response type + history accessor
- `MultiTurnStreamItem::FinalResponse(FinalResponse)` →
  `FinalResponse(PromptResponse)` (`streaming.rs:47`–`138`).
- `resp.history()` → **`resp.messages()`** (`prompt_request/mod.rs:501`);
  `resp.content()` (`:506`) replaces `resp.content()`.
- Our `prompt` write-back (`*h = fh`) is unchanged except the accessor rename.

### 4.3 Hooks: `PromptHook` → `AgentHook`
`AgentHook` (`hook.rs:916`) replaces `PromptHook` with **event-specific**
methods and renamed action enums. Our `LoggingHook` must implement it:

| 0.38 `PromptHook` | 0.40 `AgentHook` | Action type |
|---|---|---|
| `on_completion_call(&self, &Message, &[Message]) -> HookAction` | `on_completion_call(&self, &HookContext, CompletionCall) -> CompletionCallAction` | `Continue` / `Stop(String)` |
| `on_completion_response(...)` | `on_completion_response(&self, &HookContext, CompletionResponse) -> ObservationAction` | `Continue` / `Stop(String)` |
| `on_tool_call(&self, name, id, internal_id, args) -> ToolCallHookAction` | `on_tool_call(&self, &HookContext, ToolCall) -> ToolCallAction` | `Run` / `Stop(String)` / `Rewrite` |
| `on_tool_result(...)` | `on_tool_result(&self, &HookContext, ToolResultEvent) -> ToolResultAction` | `Keep` / `Stop(String)` |
| — | `on_model_turn_finished(... ModelTurnFinished) -> ModelTurnAction` | `Continue` / `Stop(String)` / `Retry` |
| — | `on_text_delta(... TextDelta) -> ObservationAction` | new |
| — | `on_tool_call_delta(...)` | new |
| — | `on_invalid_tool_call(... InvalidToolCallContext) -> Option<InvalidToolCallAction>` | new |

Our user-cancel bridge (`HookAction::terminate("cancelled by user")`) maps to
returning e.g. `CompletionCallAction::Stop("cancelled by user".into())` (and
the analogous `Stop` for `on_tool_call` / `on_completion_response` /
`on_model_turn_finished`). `ToolResultEvent` (`hook.rs:496`) exposes
`tool_name`, `args`, `raw_result` so the result string is still reachable for
`display_tool_result`.

> **Released `v0.40.0` differs:** `AgentHook` is **event-based**, not
> method-per-event. It is `pub trait AgentHook<M>` (generic over `M`) with
> `fn on_event(&self, &HookContext, StepEvent<'_, M>) -> impl Future<Output = Flow>`
> + `fn observes(&self, StepEventKind) -> bool` (hook.rs:1151). `Flow` is the
> action enum (`Continue` / `Terminate` / `Skip` / `PatchRequest` / …,
> hook.rs:905). So `LoggingHook` for 0.40.0 is:
> ```rust
> impl<M: CompletionModel> AgentHook<M> for LoggingHook {
>     fn observes(&self, k: StepEventKind) -> bool {
>         matches!(k, StepEventKind::ToolCall | StepEventKind::ToolResult
>                     | StepEventKind::CompletionResponse)
>     }
>     async fn on_event(&self, _ctx: &HookContext, ev: StepEvent<'_, M>) -> Flow {
>         match ev {
>             StepEvent::ToolCall { tool_name, args, .. } => { display_tool_call(tool_name, args); Flow::Continue }
>             StepEvent::ToolResult { tool_name, raw_result, .. } => { display_tool_result(tool_name, raw_result); Flow::Continue }
>             _ => Flow::Continue,
>         }
>     }
> }
> ```
> Cancel = `Flow::Terminate` when `user_cancel` is set. This is conceptually
> closer to the 0.38 `PromptHook` (single dispatch) than to HEAD's
> method-per-event form.

### 4.4 Tools: `Tool::call` signature
`Tool::call` gains a `ToolContext` (`tool/mod.rs:175`):

```rust
// 0.38
fn call(&self, args: Self::Args) -> impl Future<Output = Result<Self::Output, Self::Error>>;
// 0.40
fn call(&self, context: &mut ToolContext, args: Self::Args)
    -> impl Future<Output = Result<Self::Output, Self::Error>>;
```

`ToolContext` (`tool/extensions.rs:159`) replaces `ToolCallExtensions` /
`ToolResultExtensions`. `Tool::definition` is now a **provided default**
(`tool/mod.rs:337`) — remove our override in `Guard<T>`. `ToolExecutionError`
is the normalized error type. `Guard<T>` only needs the new
`context: &mut ToolContext` parameter added (unused). The wrapped tools
(`ReadFile`, `WriteFile`, …) update with rig.

> **Released `v0.40.0` differs:** `Tool::call` is **unchanged** — still
> `fn call(&self, args: Self::Args) -> impl Future<...>` (tool/mod.rs:159). There
> is no `ToolContext` param; per-call runtime values use
> `call_with_extensions(&self, args, &ToolCallExtensions)` (tool/mod.rs:181).
> So `Guard<T>` needs **no change** on 0.40.0 (unlike HEAD). Keep the existing
> `call(&self, args)` impl; drop any plan to add a `ToolContext` parameter.

### 4.5 `MultiTurnStreamItem` variants
- `StreamAssistantItem(StreamedAssistantContent<R>)` — unchanged; `Text`,
  `Reasoning`, `ReasoningDelta`, `ToolCall(RawStreamingToolCall)` variants
  still exist (`streaming.rs:~100`).
- `CompletionCall(CompletionCall)` — **retained**, but now means *"one completed
  completion request"* (emitted when a provider call finishes), **not** the tool
  call.
- **New:** `ToolExecutionCommitted { tool_call, internal_call_id }` (a tool was
  executed + committed), `ModelTurnRetried { turn }`, and
  `StreamUserItem(StreamedUserContent)` for tool results.
- Our `end_response_segment()` currently fires on `CompletionCall`. Move it to
  **`ToolExecutionCommitted`** (the commit event) — tool call/result display is
  already handled by the `AgentHook` (`on_tool_call` / `on_tool_result`), and
  the streamed `ToolCall` delta is ignored via `_ => {}`.

- Our `end_response_segment()` currently fires on `CompletionCall`. Move it to
  **`ToolExecutionCommitted`** (the commit event) — tool call/result display is
  already handled by the `AgentHook` (`on_tool_call` / `on_tool_result`), and
  the streamed `ToolCall` delta is ignored via `_ => {}`.

> **Released `v0.40.0` differs:** there is **no** `ToolExecutionCommitted`
> variant (the `MultiTurnStreamItem` enum only has `FinalResponse(R)` plus
> assistant-content items — streaming.rs:129). Tool calls/results are observed
> exclusively through `AgentHook::on_event(StepEvent::ToolCall / ToolResult)`
> (§4.3). So commit the answer paragraph from `on_event` on `StepEvent::ToolCall`
> (or keep `end_response_segment` keyed off the same event), not off a stream
> item that does not exist on 0.40.0.

### 4.6 Removed / unaffected
- Raw `Completion` / `StreamingCompletion` traits removed (CHANGELOG line 107) —
  picocode only uses `Agent::stream_prompt`, unaffected.
- `AgentBuilder::dynamic_context` removed (line 32) — picocode uses static
  builder context, unaffected.
- `Chat` now appends to caller history (#1733): `Agent::chat(prompt, &mut
  history)` extends `history`. Streaming `stream_chat` takes history by value,
  so the manual write-back from `PromptResponse::messages()` remains.
- `Reasoning` / `ReasoningContent` types are unchanged — `reasoning_text` and
  its tests need no change.

### 4.7 Released `v0.40.0` vs HEAD — what actually changes for us

If we target the released tag `v0.40.0` (`2f37dfcd`), the migration is **not**
identical to the HEAD plan above. Summary of divergences (verified in
`git show v0.40.0:...`):

| Area | Released `v0.40.0` | HEAD (post-0.40) |
|---|---|---|
| Reasoning-only history drop | **present** (sanitize_plain_text_history:1644) | present |
| Generic streaming usage (DeepSeek/Mistral cache) | **present** | present |
| OpenAI Responses `prompt_cache_key`/`retention` | **present** (responses_api/mod.rs:1431/1434) | present |
| `Tool::call` signature | **unchanged** (`(&self, args)`) | gains `ToolContext` param |
| History passing | `agent.stream_chat(input, prior)` (no `with_history`/`history` on `StreamingPromptRequest`) | `stream_prompt(...).history(prior)` / `runner(...).history(...)` |
| `AgentHook` shape | **event-based** `on_event(StepEvent) -> Flow` + `observes` (generic `<M>`) | method-per-event (`on_completion_call`, `on_tool_call`, …) |
| `MultiTurnStreamItem` tool commit | **no** `ToolExecutionCommitted`; tool call/result via `on_event` | new `ToolExecutionCommitted` variant |
| Final response type | `PromptResponse` with `messages()` (variant still `FinalResponse(R)`) | same |
| `add_hook` / `max_turns` renames | present | present |

**Net for released 0.40.0:** the upgrade still delivers all three benefits
(reasoning-loop fix, generic streaming usage, Responses-API cache params). The
code changes are: (1) `stream_prompt(input).with_history(prior)` →
`stream_chat(input, prior)`; (2) `with_hook`→`add_hook`, `multi_turn`→`max_turns`;
(3) `PromptHook` (message-based) → event-based `AgentHook<M>` (`on_event`/
`observes`, returning `Flow`); (4) `FinalResponse::history()` →
`PromptResponse::messages()`; (5) **`Guard<T>` / `Tool::call` need no change**.
This is a smaller, more contained diff than the HEAD plan (no `ToolContext`
plumbing, no `ToolExecutionCommitted` match arm). The trade-off: HEAD has more
refinements (method-per-event hooks, `ToolContext`, typed Responses-API
builder) that we'd forgo by pinning to the release.

## 5. Migration effort

- **Size:** moderate, mechanical. No picocode architectural change.
- **Files (HEAD plan):** `src/agent.rs` (`prompt` call-site renames;
  `LoggingHook` → `AgentHook`; `FinalResponse` → `PromptResponse` +
  `messages()`; match arms → `ToolExecutionCommitted`; `Reasoning`/
  `ReasoningContent` unchanged); `src/tools.rs` (`Guard::call` adds
  `context: &mut ToolContext`; drop the `definition` override); `Cargo.toml`
  version bump (`0.38` → `0.40`) + re-pin the `[patch.crates-io]` / path to the
  new rig.
- **Files (released `v0.40.0`):** `src/agent.rs` (`stream_prompt`+`with_history`
  → `stream_chat(input, prior)`; `with_hook`→`add_hook`, `multi_turn`→`max_turns`;
  `PromptHook` → event-based `AgentHook<M>` (`on_event`/`observes`);
  `FinalResponse::history()` → `PromptResponse::messages()`; commit answer
  paragraph from `on_event(StepEvent::ToolCall)`); `src/tools.rs` **needs no
  change** (`Tool::call` unchanged on 0.40.0); `Cargo.toml` bump + re-pin.
- **Tests:** `reasoning_text` test unaffected; output rendering tests
  (`output.rs`) unaffected (rendering layer unchanged); add a regression test
  that a reasoning-only turn is not persisted into the next prompt for
  OpenAI-compatible providers.
- **Risk:** the main behavioral difference is Anthropic keeping reasoning in
  history. Recommend pairing the upgrade with a small "strip reasoning from
  persisted history" toggle (esp. for Anthropic) for uniformity, and keeping
  option (B) for the residual display case.

## 6. Recommendation

**Upgrading to 0.40.0 realizes all three benefits** (reasoning-loop fix,
generic streaming usage, Responses-API cache params) — verified in the released
`v0.40.0` tag, not just HEAD. Which commit to pin:

- **Released `v0.40.0` (`2f37dfcd`)** — smaller, more contained migration
  (no `ToolContext` plumbing, event-based `AgentHook<M>`, history via
  `stream_chat`). Recommended if we want the benefits now with minimal churn.
- **HEAD (`87f3f5b7`)** — larger migration (method-per-event `AgentHook`,
  `ToolContext`, `ToolExecutionCommitted`, typed Responses-API builder) but more
  refined hooks/tools API.

**Decision: implement the HEAD migration** (per user instruction). Done — see §7.

Sequencing for the released 0.40.0 target (kept for reference):

1. Bump `rig-core` to `0.40` and fix the compile errors (`stream_chat`, `add_hook`,
   `max_turns`, event-based `AgentHook<M>`, `PromptResponse::messages()`).
2. Commit the answer paragraph from `on_event(StepEvent::ToolCall)` (no
   `ToolExecutionCommitted` on 0.40.0).
3. Add a "strip reasoning from persisted history" toggle (cheap; covers
   Anthropic + token savings).
4. Optionally implement option (B) (render reasoning-only turns as the visible
   answer) for the residual display case.
5. Add a regression test for reasoning-only history drop.

## 7. Implementation (done against HEAD `87f3f5b7`)

All changes are working-tree only (uncommitted). Validation: `cargo build`
clean (no warnings); `cargo test` → **141 lib + 6 + 4 integration + 1 doc tests
pass, 1 ignored**.

### 7.1 `Cargo.toml`
- `rig-core = "0.38"` → `"0.40"` (features `["derive"]` kept); `rig-derive =
  "0.38"` → `"0.40"`.
- `[patch.crates-io]` now also pins both to the public repo:
  `rig-core = { git = "https://github.com/gwicke/rig.git", rev = "87f3f5b7" }`
  and the same for `rig-derive`. (Per instruction: reference the public
  `gwicke/rig` repo, not a local path.)

### 7.2 `src/agent.rs`
- **Call-site renames** (per §4.1 HEAD): `stream_prompt(input)`
  `.with_hook(LoggingHook{..})` → `.add_hook(..)`; `.multi_turn(n)` →
  `.max_turns(n)`; `.with_history(prior)` → `.history(prior)`.
- **Final response** (§4.2): `resp.history()` → `resp.messages()`
  (`PromptResponse`). Added match arms `ToolExecutionCommitted { .. } => {}`
  and `ModelTurnRetried { .. } => {}` (kept `CompletionCall(_) =>
  end_response_segment()` for correct tool-line timing — see §4.5 note).
- **`LoggingHook`: `PromptHook<M>` → `AgentHook`** (§4.3 method-per-event form):
  `on_completion_call`/`on_completion_response`/`on_model_turn_finished`/
  `on_tool_call`/`on_tool_result`, returning the new action enums
  (`CompletionCallAction`, `ObservationAction`, `ModelTurnAction`,
  `ToolCallAction`, `ToolResultAction`). User-cancel bridge now returns the
  appropriate `Stop("cancelled by user")` per event (was `HookAction::terminate`).
  `on_tool_call` parses `event.args` (a `&str` JSON string) into a `Value` for
  `display_tool_call`; `on_tool_result` renders via
  `event.raw_result.output().render()` for `display_tool_result`.
- **`Guard<T>` (generic `Tool` wrapper, §4.4):** `call(&self, args)` →
  `call(&self, context: &mut ToolContext, args)` (delegates
  `self.tool.call(context, args).await`); dropped the manual `definition`
  override and added `description()`/`parameters()` delegation to the inner
  tool (the trait now requires those and provides `definition()` as default).
- Imports updated: dropped `HookAction`/`PromptHook`/`ToolCallHookAction`/
  `ToolDefinition`; added `AgentHook`, the action enums, `CompletionCallEvent`/
  `CompletionResponseEvent`/`ModelTurnFinished`/`ToolCall`/`ToolResultEvent`/
  `HookContext`, and `rig_core::tool::ToolContext`.

### 7.3 `src/tools.rs`
- **`Bash` manual `Tool` impl (§4.4):** same `call` signature change
  (`context: &mut ToolContext`); dropped manual `definition(&self, _prompt)`
  (trait default builds it from `description()`/`parameters()`); added
  `description()`/`parameters()`.
- The `#[rig_tool]`-generated tools (ReadFile, WriteFile, EditFile, GlobFiles,
  GrepText, ListDir, MakeDir, MoveFile, CopyFile, Remove, AgentBrowser) needed
  **no manual change** — `rig-derive` (bumped to HEAD) generates the matching
  `call(context, args)` impl.

### 7.4 Provider list changes (vs rig HEAD)

- **Removed `galadriel`** — rig 0.40 dropped it entirely (commit `695490d9`
  `refactor(providers)!: remove galadriel provider (#2041)`, not a rename). Removed
  its `providers` import, the `"galadriel"` arm in `agent.rs`'s client builder,
  its `model_catalog.rs` entry + env-var arm, and the `main.rs` default-model arm
  + doc comment.
- **Added (non-OAuth, chat-capable) — rig 0.40 providers picocode now wires:**
  `doubleword`, `llamafile`, `minimax`, `xiaomimimo`, `zai`. Each got a client-
  builder arm (`check_env!` + `build!(provider::Client::from_env()?)`), a
  `model_catalog.rs` `required_env_vars` arm + `DIRECT_PRICING` entry (context
  windows are best-effort estimates; prices left at `0.0` as placeholders —
  fill in real numbers later), a `main.rs` `default_model` arm, and the doc
  comment. Env vars: `DOUBLEWORD_API_KEY`, `LLAMAFILE_API_BASE_URL`,
  `MINIMAX_API_KEY`, `XIAOMI_MIMO_API_KEY`, `ZAI_API_KEY`. Default models:
  `Qwen/Qwen3.5-4B`, `LLaMA_CPP`, `MiniMax-M2.7`, `mimo-v2.5-pro`, `glm-4.6`.
- **Excluded `voyageai`** — embeddings-only provider (no `completion_model`/
  `AgentClient` chat path), so it cannot go through the `build!` chat builder.
  Out of scope until picocode has an embeddings feature.
- **Deferred (OAuth): `chatgpt`, `copilot`** — see §7.6. They implement
  `from_env()` + `AgentClient`, so the token-env-var path is as easy as the
  others above; the interactive device-code OAuth is moderate (rig does the
  protocol). Not done in this migration.

### 7.5 Pre-existing test fix (not caused by this migration)
- `settings.rs::apply_to_config_maps_the_five_settings` built `AgentConfig`
  without the committed `sandbox` field → added `sandbox: None`. (The `sandbox`
  field predates this migration; the test simply wasn't updated when the field
  was added.)

### 7.6 Adding the OAuth providers (`chatgpt`, `copilot`)

Both already implement `from_env()` + `ProviderClient` (so `AgentClient`/`.agent()`
works — they wire in like the non-OAuth providers). Two tiers:

- **Tier 1 — token env var (trivial, ~15 LoC/provider, same as §7.4):**
  `chatgpt` reads `CHATGPT_ACCESS_TOKEN` (+ optional `CHATGPT_ACCOUNT_ID`);
  `copilot` reads `COPILOT_GITHUB_ACCESS_TOKEN`/`GITHUB_TOKEN` (or an API key
  `GITHUB_COPILOT_API_KEY`/`COPILOT_API_KEY`). Add the client-builder arm +
  catalog/default-model entries; the user supplies a browser-obtained token.
- **Tier 2 — in-app interactive device-code OAuth (moderate, ~50–150 LoC):**
  rig's `from_env()` with no token falls back to `ChatGPTAuth::OAuth` /
  `CopilotAuth::OAuth`, which runs a **device-authorization flow** with
  `on_device_code(handler)` (surfaces URL + user code) and `token_dir`/
  `auth_file` (caches + auto-refreshes the token). picocode would: (a) register a
  device-code handler that prints the verification URL + code (optionally opens
  a browser); (b) wire `on_device_code(handler).token_dir(<picocode config dir>)`
  into a **provider-specific `build!` arm** (like the anthropic/openrouter
  caching arms, since the generic arm can't pass those); (c) optionally add a
  `picocode login <provider>` subcommand. picocode does **not** implement the
  OAuth protocol — rig's `auth` module handles token exchange, polling, and
  refresh. Set `allow_device_flow(false)` for non-interactive/CI use so it errors
  instead of hanging on a device code.

**Recommendation:** do Tier 1 now (consistent with the other providers, low
risk); defer Tier 2 unless passwordless `picocode login` is wanted.

### 7.7 Open follow-ups (not required for the upgrade)
- "Strip reasoning from persisted history" toggle (esp. for Anthropic, which
  keeps `thinking` blocks) — §5 risk.
- Option (B): render reasoning-only turns as the visible answer for the residual
  display case.
- Regression test asserting a reasoning-only turn is not replayed into the next
  prompt for OpenAI-compatible providers (sanitize_plain_text_history drop).
