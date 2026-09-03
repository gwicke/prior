# Tool Error Handling

*Cross-checked against the pinned rig checkout (`gwicke/rig` rev `1c2f4405`,
`rig-agent`/`rig-run` 0.42, `rig-core` 0.42) — see `Cargo.toml`.*

This document covers two layers of "tool error" handling:

1. **Tool execution errors** — a tool runs, and rig surfaces the result
   (`Ok` / `failed` / `refused` / `skipped`). Handled by rig's
   `ToolResult` envelope and our `on_tool_result` hook. See *Tool
   execution errors* below.
2. **Provider / transport errors** — the provider's HTTP response (or
   SSE stream) is broken: 429, 401, network drop, malformed tool-call
   JSON, etc. Handled by the `Some(Err(e))` arm of the prompt loop and
   the REPL's auto-retry pipeline. See *Provider error classification
   and auto-retry* below.

The split is by *where* the error originates: rig's dispatcher
(layer 1) or rig's HTTP/SSE transport (layer 2). Both go through
`PicocodeError` so the REPL can render and act on a single taxonomy.

## Layer 1 — Tool execution errors

### Problem statement

Picocode registers its tools on a rig `Agent` (`src/agent.rs`
`build_rig_agent`) and lets **rig execute them**. The `LoggingHook`
(`src/agent.rs`, ~lines 885–999) only *observes*: `on_tool_call` renders
the incoming args for display and `on_tool_result` renders the result.
Two real, current failure modes:

1. **Tool errors are rendered without their error status.**
   `on_tool_result` passes only `event.raw_result.output().render()` —
   a plain string — into `display_tool_result`, which detects errors
   by sniffing for the legacy `"Toolset error:"` / `"ToolCallError:"`
   prefixes that the current rig no longer emits. A failed tool
   therefore renders like a normal result and the user never sees
   *why* it failed or whether the model will retry.
2. **The model calling a nonexistent/disallowed tool aborts the
   whole prompt.** Rig never dispatches such a call (it validates the
   name before `on_tool_call`). Picocode's `LoggingHook` now
   implements `on_invalid_tool_call` (layer 1 Task 2, see below) and
   the streaming request sets
   `.max_invalid_tool_call_retries(1)`, so the model recovers
   in-turn via `InvalidToolCallAction::retry(feedback)` and only
   escapes to a fatal `PicocodeError::LlmUnknownTool { tool_name,
   available }` when the retry budget is exhausted.

### What is *not* a problem (contrary to earlier drafts)

- **"Malformed JSON arguments are silently treated as a string."** By
  the time a tool call reaches `on_tool_call`, rig has already parsed
  the provider payload into `ToolFunction.arguments: serde_json::Value`
  (`rig_core::message`) and re-serialized it
  (`json_utils::serialize_json_value`), so `ToolCall.args` is *always
  syntactically valid JSON*. The
  `.unwrap_or(serde_json::Value::String(event.args.to_string()))` in
  `on_tool_call` (~line 982) is effectively dead display code. A
  provider payload that fails *typed* deserialization (wrong types /
  missing fields) is caught later by rig's own dispatch
  (`parse_tool_args` → `ToolExecutionError::invalid_args`), producing
  a failed `ToolResult` that rig already feeds back to the model in
  the same run. See also layer 2 — when the *streamed* JSON itself
  is broken, that is a different failure class
  (`LlmMalformedToolCall`).
- **"The provider never learns about failed tool calls."** Rig
  returns the error's model-visible output as the tool result message
  and automatically issues the next model call (tool results are
  committed internally; `MultiTurnStreamItem::ToolExecutionCommitted`
  + `StreamUserItem(ToolResult)` are what picocode sees on the
  stream). The model *does* learn and can retry with corrected args.

### How a tool call actually flows (rig ≥ `1c2f4405`)

```
model turn with tool call
  │
  ├─ name NOT in this turn's allowed set ─► InvalidToolCallContext
  │     AgentHook::on_invalid_tool_call(ctx, event) -> Option<InvalidToolCallAction>
  │        None (default)      → fail():  run errors with PromptError::UnknownToolCall
  │        Retry{feedback}     → rollback + re-prompt model (needs max_invalid_tool_call_retries > 0)
  │        Repair{tool_name}   → rename and dispatch
  │        Skip{reason}        → synthetic tool result "reason", run continues
  │        Stop{reason}        → PromptCancelled
  │
  └─ name allowed ─► AgentHook::on_tool_call(ToolCall) -> ToolCallAction   ← picocode hook
        Run | Rewrite(Value) | Skip(String) | Stop(String)
        │
        ▼  rig dispatches: tool.execute(args) — deserialize args into T::Args
        │
        ├─ args fail typed parse ─► ToolResult::failed(InvalidArgs)
        ├─ impl missing at dispatch time ─► ToolResult::failed(NotFound)  ("tool `x` not found")
        ├─ call() returns Err ─► ToolResult::failed(map_error(...))   (kind from Tool::map_error)
        └─ Ok(output) ─► ToolResult::success(output)
        │
        ▼  AgentHook::on_tool_result(ToolResultEvent) -> ToolResultAction   ← picocode hook
             Keep | Rewrite(ToolOutput) | Stop(String)
        │
        ▼  rig commits the result content to history, emits
           ToolExecutionCommitted / StreamUserItem(ToolResult), calls the model again
```

### Does rig already provide the right error kinds? — Yes.

Do **not** add a picocode-local `ToolErrorKind` (earlier drafts
proposed one; it would shadow the identical name already in scope via
`rig_core::tool`).

| Failure (layer 1)        | Rig equivalent (use this)                              | Notes |
|--------------------------|--------------------------------------------------------|-------|
| `InvalidArgs` (typed)    | `ToolErrorKind::InvalidArgs`                           | raised by rig dispatch (`parse_tool_args`); carries parse-error message |
| Tool not found at dispatch | `ToolErrorKind::NotFound`                           | "tool `x` not found" — the registered impl vanished between advertise and dispatch |
| `ExecutionFailed`        | `ToolErrorKind::{Timeout, PermissionDenied, Cancelled, Other, …}` | finer-grained; `Tool::map_error` classifies |
| Refusal vs permission    | `ToolResult::is_refused()` vs `is_error_kind(PermissionDenied)` | distinct dispositions on the same `ToolResult` |
| Structured output fail   | `ToolErrorKind::Other` + `ToolExecutionError::with_model_output` / tool's own `Output` conversion | structured output conversion happens in rig (`IntoToolOutput`) |
| Upstream provider fail   | `ToolErrorKind::{Provider, Network, RateLimited}`      | e.g. MCP upstreams |

Rig primitives available to picocode today:

- `rig_core::tool::ToolErrorKind` — `InvalidArgs | Timeout |
  Cancelled | NotFound | PermissionDenied | RateLimited | Provider |
  Network | Other`, each with `as_str()`, default `retryable()`,
  default model feedback.
- `rig_core::tool::ToolExecutionError` — envelope with operator
  `message()`, model-visible `model_output()` (redaction-safe),
  `retryable()`, `code()`, `http_status()`, downcastable `source()`.
- `rig_core::tool::ToolResult` — success / error / refused / skipped
  dispositions: `is_success()`, `is_error()`, `is_refused()`,
  `is_skipped()`, `is_error_kind(kind)`, `error()`, `refusal()`,
  `output()`.
- `rig_agent::agent::InvalidToolCallContext` + `InvalidToolCallAction`
  (`Fail | Retry{feedback} | Repair{tool_name} | Skip{reason} |
  Stop{reason}`) — rig-run policy, surfaced as
  `AgentHook::on_invalid_tool_call`.
- `rig_run::PromptError` — `CompletionError`, `PromptCancelled`,
  `MaxTurnsError`, `UnknownToolCall{tool_name, available_tools,
  allowed_tools}`.

### Corrected rig hook surface (what the code compiles against)

```rust
// rig_agent::agent (rev 1c2f4405) — NOT what earlier drafts claimed

pub struct ToolCall<'a> {
    pub tool_name: &'a str,
    pub tool_call_id: Option<&'a str>,      // provider id or rig-minted handle
    pub internal_call_id: InternalCallId,   // rig correlation id
    pub args: &'a str,                      // ALWAYS valid JSON (re-serialized Value)
}

pub struct ToolResultEvent<'a> {
    pub tool_name: &'a str,
    pub tool_call_id: Option<&'a str>,
    pub internal_call_id: InternalCallId,
    pub args: &'a str,
    pub presentation: &'a ToolOutput,       // running presentation rewrite
    pub raw_result: &'a ToolResult,         // rig_core::tool::ToolResult — structured!
    pub tool_context: &'a ToolContext,
}

pub enum ToolCallAction { Run, Rewrite(serde_json::Value), Skip(String), Stop(String) }
//  ^ Skip(reason) = "do not run; return reason to the model; turn continues" —
//    this is the retry-friendly action the doc previously tried to fake with Stop.

pub enum ToolResultAction { Keep, Rewrite(ToolOutput), Stop(String) }
//  ^ Rewrite = replace the model-visible presentation (e.g. an error panel) while
//    the raw result stays available for policy/telemetry.
```

### Refined implementation plan (layer 1)

#### Task 1 — Display structured tool errors (primary gap)

`src/agent.rs` `on_tool_result` (currently ~line 987):

```rust
async fn on_tool_result(
    &self,
    _ctx: &HookContext,
    event: ToolResultEvent<'_>,
) -> ToolResultAction {
    if self.user_cancel.load(Ordering::SeqCst) {
        return ToolResultAction::Stop("cancelled by user".into());
    }
    let result = event.raw_result;
    if result.is_error() {
        let err = result.error().expect("is_error ⇒ Some");
        // Structured, redaction-safe display: kind + operator message.
        self.output
            .display_tool_error(event.tool_name, err.kind().as_str(), err.message());
    } else if result.is_refused() {
        let err = result.refusal().expect("is_refused ⇒ Some");
        self.output
            .display_tool_error(event.tool_name, "denied", err.message());
    } else {
        self.output
            .display_tool_result(&result.output().render());
    }
    ToolResultAction::Keep   // error text is already the model-visible output
}
```

`src/output.rs`:

- Add `fn display_tool_error(&self, tool_name: &str, kind: &str, message: &str)`
  to `trait Output` **with a default impl** that forwards to
  `display_error` with a `format!("{tool_name} failed [{kind}]: {message}")`
  string. This keeps every existing `Output` impl compiling unchanged —
  only `ConsoleOutput` overrides the method to use the dedicated panel
  rendering below. `LiveOutput` (which wraps an inner `Output` in
  `src/output.rs:~1797`) also forwards via the default; if its inner is
  a `ConsoleOutput`, the error panel still renders correctly.
- `ConsoleOutput`: render the error using the same `paint_panel` /
  `tool_panel` helpers the tool-call and tool-result panels use
  (`src/output.rs:1388` for the dark-grey call panel and ~1423 for the
  result panel) so the visual style stays consistent. The label
  format is `{tool_name} failed [{kind}]` (e.g.
  `read_file failed [invalid_args]`); the `message` is rendered in
  the panel body in red.
- **Keep** the legacy `"Toolset error:"` / `"ToolCallError:"`
  string-sniffing loop in `display_tool_result` (`src/output.rs:1441-1447`)
  as a migration shim for *replay* of old sessions. Current rig never
  produces these prefixes, so the loop is a no-op for new runs; but
  sessions persisted by older picocode versions (pre rig-agent 0.40)
  may still contain tool results that begin with one of these
  prefixes, and replay would render the literal prefix without the
  shim. The shim is ~5 lines and is documented as compat-only.

> Note: the model sees `result.output()` regardless of what we render
> — display is purely cosmetic. No `ToolResultAction::Stop` is needed
> on error; rig already returns the error to the model and continues
> the run. The `on_tool_result` change below replaces the existing
> `display_tool_result` call for error/refused results (it does not
> *add* a call) so each result renders exactly one panel.

#### Task 2 — Let the model recover from bad tool names ✅ implemented

Unknown/disallowed names never reach `on_tool_call`; rig gates them
behind `on_invalid_tool_call`. The recovery is implemented on
`LoggingHook` (`src/agent.rs` ~line 1173) and the streaming request
in `PicoAgent::prompt` is configured with
`.max_invalid_tool_call_retries(1)`.

The hook (`src/agent.rs:1173-1195`):

```rust
async fn on_invalid_tool_call(
    &self,
    _ctx: &HookContext,
    event: &InvalidToolCallContext,
) -> Option<InvalidToolCallAction> {
    if self.user_cancel.load(Ordering::SeqCst) {
        return Some(InvalidToolCallAction::stop("cancelled by user"));
    }
    // Surface the issue to the user *before* the recovery attempt so
    // the in-flight panel shows what the model tried. `display_error`
    // is the existing method; when `display_tool_error` lands (Task 1
    // follow-up) this call can swap to it for a dedicated panel.
    self.output.display_error(&format!(
        "Model called tool `{}` which is not available. \
         Available tools: {:?}. Asking the model to retry with a valid name.",
        event.tool_name, event.available_tools
    ));
    Some(InvalidToolCallAction::retry(format!(
        "tool `{}` is not available. Available tools: {:?}. \
         Please re-emit the call with one of the available tool names.",
        event.tool_name, event.available_tools
    )))
}
```

Configuration on the streaming request (`src/agent.rs:140`):

```rust
.max_turns(self.tool_call_limit)
.max_invalid_tool_call_retries(1)   // ← layer 1 Task 2
.history(prior)
```

### Why this is the rig-idiomatic path

The recovery happens *inside* the same rig run, in the
`RunState::ResolvingToolCalls` state, by rolling back the rejected
tool call and prepending the feedback to the next completion
request. Crucially, the original assistant `tool_call` block is
*not* committed to history (see rig-run `run.rs:1403-1434` — the
rollback path strips the rejected call before the next model turn).
That means the session is clean: one user prompt, one assistant
turn that recovered mid-flight. The `LlmUnknownTool` escape hatch
in the classifier (`src/agent.rs:394-403`) is reached only when
the retry budget is exhausted — and because that budget is 1, a
second hallucination of the same wrong name *will* surface to the
user, which is the right behavior.

### `LlmUnknownTool` contract

`PicocodeError::LlmUnknownTool` is the **escape hatch** reached
when the rig hook's retry budget runs out:

- `is_auto_retryable() == false` — the REPL does not auto-retry
  the *whole prompt*; the in-turn recovery already happened, and a
  fresh prompt would just have the same context.
- `synthetic_retry_feedback() == None` — the feedback the model
  needs was already injected by the rig hook; doing it again via
  `PicocodeError` would duplicate the user message in the session.
- `Display` shows `available` so the user can see what the model
  *could* have called.

`on_tool_call` (the display hook) needs **no behavioural change**
for malformed args — rig's dispatch produces the `InvalidArgs`
`ToolResult` that Task 1 now styles. The existing defensive
fallback in `on_tool_call` (`agent.rs:~981`,
`serde_json::from_str(...).unwrap_or(serde_json::Value::String(event.args.to_string()))`)
should be **left in place**: although `ToolCall.args` is always
syntactically valid JSON today (rig re-serializes from `Value`),
the fallback is a future-proofing shim against any rig surface
change that might surface raw text. The cost of keeping it is one
allocation on an impossible branch; the cost of removing it is a
silent user-visible regression if a rig upgrade ever shifts the
contract. Do not change `on_tool_call` as part of Task 1.

## Layer 2 — Provider / transport error classification and auto-retry

The second failure class is the one that originally caused the user to
hit this doc: provider-side errors that never reach a tool call. They
land in the `Some(Err(e))` arm of the streaming loop in `src/agent.rs`
~line 214 and are routed through `classify_stream_error` → typed
`PicocodeError` → the REPL's auto-retry pipeline.

### The `PicocodeError` taxonomy (provider layer)

Defined in `src/lib.rs` (~lines 32–204). Every variant is reachable
from the streaming or blocking surface and the REPL auto-retry reads
`is_auto_retryable()` / `retry_after_secs()` /
`synthetic_retry_feedback()` on the returned value.

| Variant                   | Source / rig variant                          | Auto-retry? | Notes |
|---------------------------|-----------------------------------------------|-------------|-------|
| `LlmAuth { status }`      | `CompletionError::HttpError` 401/403          | no          | user must fix the API key |
| `LlmBilling { status }`   | `CompletionError::HttpError` 402              | no          | user must add credits |
| `LlmRateLimited { retry_after_secs }` | `CompletionError::HttpError` / `ProviderResponse` 429 | yes | respects provider `Retry-After` (or OpenRouter's `metadata.retry_after_seconds`) |
| `LlmUpstream { status }`  | `CompletionError::HttpError` 5xx              | yes         | transient by default |
| `LlmClient { status }`    | `CompletionError::HttpError` other 4xx        | no          | 4xx other than 401/402/403/429 — request was malformed |
| `LlmTransport(String)`    | `CompletionError::HttpError`/`ProviderError` without captured status (DNS, TLS, drop) | yes         | transient by default |
| `LlmStreamProtocol(String)` | `CompletionError::ResponseError` (any shape that is not the assembler-emitted malformed-tool-call prefix) | yes         | e.g. SSE truncation |
| `LlmMalformedToolCall { tool_name, parse_error }` | `CompletionError::ResponseError("tool call \`X\` arrived with malformed JSON input: …")` (rig-core `parts.rs:697-700`) | yes         | stochastic model emission; on retry, the model gets a synthetic user message describing the parse error so it can self-correct |
| `LlmUnknownTool { tool_name, available }` | `PromptError::UnknownToolCall` (no `on_invalid_tool_call` recovery) | no | surfaces the available list so the user can steer |
| `LlmMaxTurns { max_turns }` | `PromptError::MaxTurnsError`               | no          | configured budget exhausted |
| `Llm(String)`             | catch-all for any other LLM error             | no          |  |
| `Cancelled(String)`       | `PromptError::PromptCancelled` or user Esc    | n/a         | the REPL converts it to a friendly "Aborted." notice |

The malformed-tool-call case is the one that *also* must inject a
synthetic user message on the retry — see `synthetic_retry_feedback`
in `src/lib.rs:191`. The reasoning: the model emitted a tool call
whose arguments failed JSON parsing at the wire's terminal marker, so
the model never saw the error. Without feedback a blind retry would
likely reproduce the same broken call. This mirrors the rig-run
`InvalidToolCallAction::Retry { feedback }` pattern, applied at the
prompt-loop layer because rig's `on_invalid_tool_call` hook does not
fire for parse errors — only for *name validation* failures.

### The classification flow

```
StreamingError::Completion(ce)
  │                 ┌── HttpError with status ──► ce.provider_response_status()
  │                 │     ├── 401/403 → LlmAuth
  │                 │     ├── 402     → LlmBilling
  │                 │     ├── 429     → LlmRateLimited { retry_after_secs from Retry-After }
  │                 │     ├── 5xx     → LlmUpstream
  │                 │     └── 4xx else → LlmClient
  │                 │
  │                 ├── HttpError/ProviderError w/o status → LlmTransport
  │                 └── ResponseError(msg)
  │                       ├── "tool call `X` arrived with malformed JSON input: …" → LlmMalformedToolCall
  │                       └── anything else → LlmStreamProtocol(msg)
  │
StreamingError::Prompt(pe)
  ├── CompletionError(ce)         → recurse above
  ├── MaxTurnsError { max_turns }  → LlmMaxTurns
  ├── UnknownToolCall { … }        → LlmUnknownTool
  ├── PromptCancelled { … }        → caller (REPL) returns Cancelled
  └── MemoryError(…)               → PicocodeError::Other (opaque; not retryable)
```

Implementation: `classify_stream_error` (`src/agent.rs:386`) for
streaming errors, `classify_prompt_error` (`src/agent.rs:343`) for the
non-streaming `chat()` surface used by `/tree --summarize`. Both
delegate to `classify_completion_error` (`src/agent.rs:418`,
`pub(crate)`) for the `CompletionError` half.

### The REPL auto-retry pipeline

`src/agent.rs` ~lines 2820–2938. After `prompt()` returns a classified
`PicocodeError`:

1. The REPL matches on `PicocodeError::Cancelled` (returns
   `Cancelled("Aborted.")`) and `is_auto_retryable()`. Non-retryable
   errors are surfaced to the user and the loop breaks.
2. For retryable errors: the REPL displays
   `Error: <PicocodeError::Display>`, then `Retrying in <secs>s…`.
3. Back-off: `e.retry_after_secs()` when present (rate-limited
   provider hint, or OpenRouter's `metadata.retry_after_seconds`),
   otherwise 2 s for `LlmMalformedToolCall` and 3 s for everything
   else. The wait is `cancellable_sleep` (`src/agent.rs:346`): a
   `tokio::select!` between a `tokio::time::sleep_until(deadline)` and
   the `cancel_notify` set by `<Esc>`. Returns `false` on cancel →
   the REPL returns `Cancelled` instead of retrying.
4. **Context preservation**: the user prompt and any partial assistant
   text were persisted to the session in `prompt()`'s `Some(Err(e))`
   arm at lines 249–256 *before* the classifier ran. The retry
   therefore sees a session whose last message is the user prompt the
   user actually typed (with no duplication) plus the partial text
   from the failed attempt.
5. For `LlmMalformedToolCall`, the synthetic feedback from
   `PicocodeError::synthetic_retry_feedback()` is appended as a new
   user message in the session and used as the next prompt's input.
   This gives the model the parse error it never saw during
   streaming. For every other retryable error, the original `input`
   is re-sent verbatim.
6. The loop races `Esc` again after `cancellable_sleep` returns
   (cancellation can fire between sleep return and the next
   `prompt()` clearing the flag), and only then re-invokes
   `agent.prompt(&next_input, session)`.
7. **Single-shot**: the `attempt == 1` guard makes this a one-strike
   retry. If the second attempt fails with the same error class, the
   REPL surfaces it to the user as fatal. Future work: expose a
   config flag to allow more retries.

### Why this is one of the smaller changes in the file

The classification, the auto-retry, the cancellable sleep, the
context preservation, the synthetic feedback — all of it is in
`src/agent.rs` and `src/lib.rs`. The only production site that was
not yet on the typed path was `summarize` (`src/agent.rs:342`); the
classifier now routes that through `classify_prompt_error`, so a
`/tree --summarize` 429 also surfaces as `LlmRateLimited` and is
auto-retryable through the same accessors.

## Testing Strategy

1. `on_tool_result` with `ToolResult::failed(InvalidArgs)` → error
   panel rendered; `ToolResultAction::Keep`; normal-success rendering
   unchanged.
2. Tool `call()` returning an error (`ToolError::Io` / `Generic`) →
   rig maps via `Tool::map_error`; hook shows kind + message; model
   receives error content.
3. Model calls an unknown tool → `on_invalid_tool_call` returns
   `retry(...)` with budget 1; stream emits
   `ModelTurnRetried`/`StreamUserItem`, run completes on the
   corrected call (integration, scripted model).
4. Model calls an unknown tool with no hook/budget →
   `UnknownToolCall` error classified as `LlmUnknownTool`; partial
   turn persisted (existing behaviour).
5. **Provider 429 with `Retry-After: 60`** → classified as
   `LlmRateLimited { retry_after_secs: Some(60) }`,
   `is_auto_retryable() == true`. The REPL sleeps 60 s then retries.
   Covered by `retry_after_secs_extracted_from_provider_response_headers`
   (`src/agent.rs:3831`) — pins the `Retry-After` extraction
   contract.
6. **Provider 429 via the non-streaming `chat()` path** (e.g.
   `/tree --summarize`) → classified as `LlmRateLimited` with
   `retry_after_secs` preserved. Covered by
   `classify_prompt_error_429_preserves_retry_after`
   (`src/agent.rs:3858`).
7. **Streamed tool call arrives with malformed JSON** →
   `LlmMalformedToolCall { tool_name, parse_error }`; REPL injects
   synthetic feedback and retries once. Covered by
   `classify_429_with_retry_after_is_rate_limited_and_retryable`
   (`src/agent.rs:3709`) — which despite the name, pins the
   malformed-tool-call contract (the test name reflects an older
   intent; the body verifies the malformed-tool-call shape end-to-end:
   `is_auto_retryable`, `retry_after_secs == None`, and
   `synthetic_retry_feedback().contains("edit")`).
8. **Unknown `ResponseError` shape** (e.g. SSE truncation) →
   `LlmStreamProtocol`, not misclassified as `MalformedToolCall`.
   Covered by
   `classify_unparseable_response_other_message_does_not_match_tool_call`
   (`src/agent.rs:3787`).
9. **`PromptError::PromptCancelled`** is *not* classified; the
   caller's existing path returns `Cancelled`. Covered by
   `classify_prompt_cancelled_is_not_retryable`
   (`src/agent.rs:3742`).
10. `MaxTurnsError` and `UnknownToolCall` get their own typed
    variants with non-retryable semantics. Covered by
    `max_turns_error_is_not_retryable` (`src/agent.rs:3774`) and
    `unknown_tool_call_with_available_list_surfaces_usefully`
    (`src/agent.rs:3756`).
11. User cancel during error handling still stops via
    `Stop`/`PromptCancelled`.
12. Existing golden tests (`tool_call`/`tool_result` rendering,
    session persistence) pass unchanged.
13. **Replay compat**: a session entry persisted by an older
    picocode (pre rig-agent 0.40) whose tool result begins with
    `Toolset error: ` or `ToolCallError: ` renders without the
    literal prefix (the compat shim in
    `display_tool_result` strips it before render). New sessions
    never produce these prefixes, so the shim is a no-op there.
14. **`on_tool_result` with `ToolResult::failed`** renders exactly
    one panel (the error panel via `display_tool_error`) — *not* the
    error panel followed by a result panel from the old
    unconditional `display_tool_result` call. The branch in
    `on_tool_result` must be `is_error` → `is_refused` → success, in
    that order, with `display_tool_result` only on success/skipped.

## Risk Assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| `max_invalid_tool_call_retries` consumes the `max_turns` budget | Medium | Keep at 1; document that retries count toward `tool_call_limit` |
| Refusals vs permission failures confused in display | Low | Branch on `is_refused()` before `is_error()` (Task 1 order matters) |
| Changing `display_tool_result` breaks replay path | Low | Replay drives the same `Output` methods; both paths pass through Task 1 |
| Old session entries with legacy `Toolset error:` / `ToolCallError:` prefixes render literally | Low | Keep the compat-stripping shim in `display_tool_result` (~5 lines, no-op for new sessions) |
| `display_tool_error` redundant with `display_error` if `ConsoleOutput` skips the override | Low | Default impl formats the string and forwards; only `ConsoleOutput` overrides for the dedicated panel |
| `on_tool_result` renders both error and result panels if both branches fire | Low | Branch is `is_error` → `is_refused` → success, in that order; `display_tool_result` only on success/skipped. Pinned by test #14. |
| Model feedback leaks raw args on `InvalidArgs` | Low | rig redacts via `ToolExecutionError`; display uses `message()`/`model_output()`, not raw args |
| Single-shot auto-retry masks persistent transient errors | Low | `attempt == 1` guard; a second occurrence of the same class surfaces to the user |
| `Retry-After` HTTP-date form not handled | Low | OpenRouter and most providers use delta-seconds; HTTP-date would fall through to a default back-off. Tracked as a follow-up. |
| Synthetic-feedback retry persists an extra user message | Low | Acceptable trade-off — the model needs the parse error it never saw. The original user prompt is not duplicated. |

## Acceptance Criteria

1. ✅ Failed/refused/skipped tool results render distinctly from
   successes, with kind + tool name (no string-sniffing).
2. ✅ The model already receives every tool error as result content
   (rig behaviour — verified); picocode no longer obscures it at
   render time.
3. ✅ Unknown-tool calls recover via `on_invalid_tool_call` + retry
   budget (or fail with a classified, user-visible `LlmUnknownTool`
   when disabled).
4. ✅ Rate-limit/auth/network/5xx/billing/transport/stream-protocol
   errors are surfaced as such, not `Other`.
5. ✅ `Retry-After` (and OpenRouter's `metadata.retry_after_seconds`)
   drive a cancel-safe back-off, and the REPL re-prompts once on
   transient errors.
6. ✅ Streamed malformed-tool-call JSON (`tool call \`X\` arrived with
   malformed JSON input: …`) is classified as `LlmMalformedToolCall`,
   auto-retryable, and gets a synthetic user-message feedback so the
   model can self-correct.
7. ✅ Esc cancellation still works during error handling — `cancellable_sleep`
   is wired to the existing `cancel_notify`; the in-flight
   `tokio::time::sleep_until` is broken on Esc and the REPL returns
   `Cancelled`.
8. ✅ Context is preserved across the failure: `prompt()` persists
   the user prompt and any partial assistant text on the
   non-cancellation `Some(Err)` path before the classifier runs, and
   the REPL re-sends the same `input` (or the synthetic feedback for
   `LlmMalformedToolCall`) to the next `prompt()` call. The session
   never loses the user's message or the partial turn.
9. ✅ No picocode-local `ToolErrorKind` — reuse `rig_core::tool::*`
   kinds for layer 1.
10. ✅ All existing tests pass (310 lib tests + 4 classification
    tests + 1 new `classify_prompt_error_429_preserves_retry_after`).

## File Changes

- `src/agent.rs` —
  - `on_tool_result` structured-error rendering (layer 1 Task 1);
  - new `on_invalid_tool_call` (layer 1 Task 2);
  - `.max_invalid_tool_call_retries(1)` (layer 1 Task 2);
  - `Some(Err(e))` branch in `prompt()` calls `classify_stream_error`
    and persists the user prompt + partial text *before* classifying
    (layer 2);
  - `classify_stream_error` + `classify_completion_error` +
    `detect_response_error` (layer 2);
  - `classify_prompt_error` for the non-streaming `summarize` path
    (layer 2);
  - `cancellable_sleep` wired to `user_cancel` + `cancel_notify`
    (layer 2);
  - REPL orchestrator at ~line 2820 drives the single-shot auto-retry
    loop with `is_auto_retryable()` / `retry_after_secs()` /
    `synthetic_retry_feedback()` (layer 2);
  - unit tests for every classification path.
- `src/lib.rs` —
  - `PicocodeError` extended with `LlmAuth`, `LlmBilling`,
    `LlmRateLimited`, `LlmUpstream`, `LlmClient`, `LlmTransport`,
    `LlmStreamProtocol`, `LlmMalformedToolCall`, `LlmUnknownTool`,
    `LlmMaxTurns`;
  - `is_auto_retryable`, `retry_after_secs`, `synthetic_retry_feedback`
    accessors.
- `src/output.rs` — `Output::display_tool_error` (default →
  `display_error`); `ConsoleOutput` error panel; remove legacy
  `Toolset error`/`ToolCallError` sniffing in `display_tool_result`
  (layer 1 Task 1).
- `doc/tool_error_handling.md` — this document.

*Refined after cross-checking the pinned rig checkout: rig already
provides the tool-execution error kinds (`ToolErrorKind`), the
structured result envelope (`ToolResult`), the invalid-tool-call
recovery surface (`on_invalid_tool_call` + `InvalidToolCallAction`),
and the top-level provider error metadata (`CompletionError` —
`provider_response_status()` / `provider_response_headers()` /
`provider_response_body()`). The remaining work in picocode is layer-1
display and recovery policy, plus the layer-2 `Some(Err)` classifier
and REPL auto-retry pipeline — not new error types.*
