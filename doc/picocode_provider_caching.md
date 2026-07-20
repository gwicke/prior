# picocode / rig-core Provider Caching

This documents whether picocode or its upstream `rig-core` (resolved from
**gwicke/rig** at `f31c38c`, patched via `[patch.crates-io]` in `Cargo.toml`;
effectively a 0.40-based fork) supply provider-side caching hints and read-back
comparable to the pi agent (see `doc/pi_provider_caching.md`).

## Summary

| pi capability | rig-core fork (gwicke/rig @ f31c38c) | picocode app |
| --- | --- | --- |
| Anthropic `cache_control` markers (system + last msg) | **Yes** (`apply_cache_control` / `apply_prompt_cache_control`) | **Enabled** — `with_automatic_caching()` (top-level `cache_control`, API auto-advances) |
| Anthropic `ttl: "1h"` (long retention) | **Yes** — `CacheTtl::OneHour` variant plus `with_automatic_caching_1h()` builder | **Not used** — picocode calls `with_automatic_caching()` (5 min default) |
| Anthropic cache on last tool definition | **Yes** — `apply_tool_cache_control()` marks the final non-deferred tool | **Covered** — automatic mode sets the breakpoint server-side on the last cacheable block |
| OpenAI/OpenRouter `cache_control` / `prompt_cache_key` / session-affinity | **Partial** — OpenRouter `with_prompt_caching()` adds `cache_control: {"type": "ephemeral"}` to system prompt; OpenAI responses_api reads `cached_tokens` back but has no send-side hints | **Enabled** for OpenRouter (`with_prompt_caching()`) |
| Amazon Bedrock cache points | **Yes** — `rig-bedrock` crate has `CachePoint` support in Converse API requests | **Not used** — picocode does not build a Bedrock agent |
| Read-back of cache usage (input/output tokens) | **Yes** for Anthropic, OpenAI, DeepSeek, Gemini, OpenRouter, Bedrock | **Captured and surfaced** — `SessionUsage` tracks cached/creation tokens; `/stat` displays cache hit rate |

Verdict: picocode now **actively enables** caching for the two most important
providers (Anthropic via `with_automatic_caching()`, OpenRouter via
`with_prompt_caching()`), and **reads back and displays** cache metrics for all
providers that report them. The only missing send-side piece is OpenAI's native
chat-completions API (no `prompt_cache_key` / `session_id` / `x-session-affinity`).

## 1. rig-core fork — Anthropic `cache_control`

File: `crates/rig-core/src/providers/anthropic/completion.rs`
(inside the gwicke/rig checkout at `~/.cargo/git/checkouts/rig-794bcfddf11aa804/f31c38c/`)

- `CacheControl` enum has a single variant `Ephemeral { ttl: Option<CacheTtl> }`.
  `CacheTtl` has two variants: `FiveMinutes` and `OneHour`. This means rig-core
  **can** emit `ttl: "1h"` for long retention (unlike pi's `Long` retention, but
  functionally equivalent).
- Two caching modes exist:
  1. **Manual prompt caching** (`with_prompt_caching()`, sets
     `self.prompt_caching = true`): `apply_prompt_cache_control()` marks three
     breakpoints — the final non-deferred tool, the system prompt's last content
     block, and the last content block of the last message. Budgets up to 4 total
     `cache_control` markers, accounting for any pre-existing tool markers.
  2. **Automatic caching** (`with_automatic_caching()`, sets
     `self.automatic_caching = true`): Adds a top-level `cache_control` field to
     the request. The Anthropic API automatically places the breakpoint on the
     last cacheable block and advances it as history grows. This is the
     recommended approach for multi-turn conversations.
- `with_automatic_caching_1h()` enables automatic caching with a 1-hour TTL.
- `apply_tool_cache_control()` (line ~2042) marks the **final non-deferred tool**
  with `cache_control`. Tools carry an optional `cache_control` field in
  `ToolDefinition`; existing markers are preserved and counted toward the 4-marker
  budget.
- `validate_cache_control_ttl_order()` ensures `1h` markers appear before `5m`
  markers (Anthropic API constraint).
- Read-back: `CompletionResponse` exposes `cache_read_input_tokens` /
  `cache_creation_input_tokens`, mapped into `Usage.cached_input_tokens` /
  `Usage.cache_creation_input_tokens`.

### Usage in picocode

picocode calls `.with_automatic_caching()` (the top-level `cache_control`
approach) for Anthropic (see `src/agent.rs` lines ~458–463). This requires no
beta header; the API manages the breakpoint automatically.

## 2. rig-core fork — OpenAI / OpenRouter

### OpenAI chat completions

- `src/providers/openai/completion/mod.rs` — reads `prompt_tokens_details.cached_tokens`
  back from responses and maps into `Usage.cached_input_tokens`. No send-side
  cache hints (`cache_control`, `prompt_cache_key`, etc.).

### OpenAI responses API

- `src/providers/openai/responses_api/mod.rs` — exposes `cached_tokens` in the
  usage struct (read-back only) and a `prompt_cache_key` / `prompt_cache_retention`
  field on the request struct (lines ~1657–1662). picocode currently does not
  set these fields.

### OpenRouter

- `src/providers/openrouter/completion.rs` — has `with_prompt_caching()` builder
  (line ~1514) which adds `cache_control: {"type": "ephemeral"}` to the system
  prompt's last content block. This tells upstream models that support explicit
  cache breakpoints (e.g. `anthropic/...`) to cache the system prompt prefix.
- `src/providers/openrouter/client.rs` — reads back `cached_tokens` and
  `cache_write_tokens` from the OpenRouter usage response, mapping into
  `Usage.cached_input_tokens` / `Usage.cache_creation_input_tokens`.

### Usage in picocode

picocode calls `.with_prompt_caching()` for OpenRouter (`src/agent.rs` lines
~468–471). No caching hints are sent for native OpenAI chat completions or
responses API.

## 3. rig-core fork — DeepSeek (read-back only)

File: `src/providers/deepseek.rs`

- `Usage` exposes `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`
  and `cached_tokens`. These are parsed from the response and mapped into
  `Usage.cached_input_tokens`. There is **no** send-side cache hint.

## 4. rig-core fork — Gemini (read-back only)

Files: `src/providers/gemini/completion.rs`, `src/providers/gemini/streaming.rs`

- Only read-back: `cached_content_token_count: Option<i32>` and
  `cache_tokens_details: Option<Vec<ModalityTokenCount>>` feed into
  `Usage.cached_input_tokens`. No send-side cached-content handle in requests
  (the lone `cachedContent: Optional<String>` at completion.rs ~line 2114 is a
  commented-out, unused field).

## 5. Amazon Bedrock

- The gwicke/rig fork ships a `rig-bedrock` crate (`crates/rig-bedrock/`).
- `src/completion.rs` — has a `prompt_caching` option that inserts `CachePoint`
  blocks into Converse API requests (lines ~165–192).
- `src/streaming.rs` — reads back `cache_read_input_tokens` and
  `cache_write_input_tokens`, mapped into `Usage.cached_input_tokens` /
  `Usage.cache_creation_input_tokens`.
- picocode does **not** build a Bedrock agent (no `"bedrock"` arm in
  `create_agent()`), so these features are unused here.

## 6. picocode application level

Files: `src/agent.rs`

- **Anthropic**: `create_agent()` calls `.with_automatic_caching()` on the
  completion model builder (lines ~458–463). The top-level `cache_control` field
  is sent on every request; the API manages the breakpoint automatically.
- **OpenRouter**: `create_agent()` calls `.with_prompt_caching()` on the
  completion model builder (lines ~468–471). Marks the system prompt with
  `cache_control: {"type": "ephemeral"}`.
- **Other providers**: No caching hints are set (the default `build!($client)`
  arm at line ~474 uses plain `$client.agent(&model)`).
- picocode **reads and surfaces** cache `Usage`. `CodeAgent::prompt()`
  (agent.rs ~155–171) captures `cached_input_tokens` and
  `cache_creation_input_tokens` per-request into `SessionUsage`. The `/stat`
  command (via `session_stats_lines()`, lines ~1285–1351) displays a cache hit
  rate line:
  ```
  cache hit rate       : 45.2% (read 4520 / written 350 / fresh 5500 tokens)
  ```
  When no cache metrics are reported, it shows:
  ```
  cache hit rate       : n/a (provider reported no cache metrics)
  ```

## 7. Providers with no caching support at all

These providers have no send-side cache hints and no cache read-back in their
rig provider implementations: Azure, Cohere, ChatGPT, Copilot, Groq, HuggingFace,
Hyperbolic, Llamafile, Mira, Minimax, Mistral, Moonshot, Ollama, Perplexity,
Together, xAI, XiaomiMimo, Doubleword.

**ZAI** is an exception: the OpenAI-compatible path now benefits from the generic
`with_prompt_caching()` added to the shared `GenericCompletionModel` (see §8.5),
and the Anthropic-compatible endpoint (`zai-anthropic`) uses
`with_automatic_caching()`.

## Notes

- The behavior above is for the rig-core fork used by picocode
  (gwicke/rig, resolved via `[patch.crates-io]` in `Cargo.toml`,
  tracked as version `0.40.0` in `Cargo.lock`). Older rig-core versions
  (0.28.0, 0.38.2, 0.39.0) also exist in `~/.cargo/registry/` but are not
  referenced by this crate.
- As of the latest round of changes the only upstream mechanisms picocode does
  **not** exploit are:
  - OpenAI responses API `prompt_cache_key` / `prompt_cache_retention`
  - Bedrock `CachePoint` (provider not integrated)

## 8. Implemented improvements and remaining recommendations

### 8.1 ✅ OpenRouter — session affinity via provider ordering

**Status: Implemented.** `merge_openrouter_params()` in `src/agent.rs` creates a
`ProviderPreferences` with `.order(["anthropic"]).allow_fallbacks(true)` and
merges it into `additional_params` on every request. This pins OpenRouter to the
`anthropic` upstream first, so cache state accumulates across turns instead of
being invalidated by load-balancing. Falls back to other upstreams if `anthropic`
is unavailable.

### 8.2 ✅ OpenRouter — `cache_control` on conversation history

**Status: Implemented.** The shared `apply_prompt_caching()` function in
`crates/rig-core/src/providers/openai/completion/mod.rs` now marks **both** the
system prompt **and** the last non-system message with
`cache_control: {"type": "ephemeral"}`. OpenRouter uses this shared function via
its `finalize_openrouter_request_body` hook (the old OpenRouter-specific
`apply_prompt_caching` has been removed).

### 8.3 ✅ ZAI — Anthropic-compatible automatic caching

**Status: Implemented.** Two changes:

1. **OpenAI-compatible endpoint** (`zai::Client`): now calls
   `.with_prompt_caching()` on the model builder, which adds `cache_control`
   markers via the shared generic path.

2. **Anthropic-compatible endpoint** (`zai::AnthropicClient`): exposed as a new
   provider alias `"zai-anthropic"` in `create_agent()` that uses
   `.with_automatic_caching()` (inherited from the Anthropic provider).

### 8.4 DeepSeek — send-side caching is not available (no action needed)

**Unchanged.** DeepSeek's API does not support `cache_control` markers, but
read-back of `cached_tokens` from `prompt_tokens_details` is already surfaced
in `/stat`.

### 8.5 ✅ Generic `with_prompt_caching()` on OpenAI-compatible path

**Status: Implemented.** Added to the rig fork in
`crates/rig-core/src/providers/openai/completion/mod.rs`:

- **`GenericCompletionModel::with_prompt_caching()`**: sets `self.prompt_caching`
  on any OpenAI-compatible model handle.
- **Default `finalize_request_body_with_options()`**: now calls
  `apply_prompt_caching(body)` when `options.prompt_caching` is true.
- **Shared `apply_prompt_caching()`**: a `pub(crate)` function that marks both
  the system prompt and the last conversation message with
  `cache_control: {"type": "ephemeral"}`. Used by both the OpenAI default path
  and the OpenRouter override.

This means any OpenAI-compatible provider (Groq, DeepSeek, Together, Perplexity,
xAI, ZAI, etc.) can now enable prompt caching by calling `.with_prompt_caching()`
on the model builder. Providers that do not support the `cache_control` field
silently ignore it.

### 8.6 ✅ `additional_params` plumbing through the agent layer

**Status: Already implemented upstream.** Rig's `AgentBuilder`, `Agent`, and
`AgentRunner` already plumb `additional_params` through to every
`CompletionRequest`. The `AgentBuilder::additional_params()` method and
`AgentRunner::merge_additional_params()` / `replace_additional_params()` methods
provide fine-grained control. No rig fork changes were needed for this.

picocode uses this plumbing to inject:
- `think_json` (thinking/reasoning configuration) for all providers
- OpenRouter `ProviderPreferences` (provider pinning) merged with `think_json`

### 8.7 ✅ Anthropic — 1-hour cache TTL

**Status: Implemented.** Switched Anthropic from `with_automatic_caching()`
(5-minute default TTL) to `with_automatic_caching_1h()` (1-hour TTL) in
`src/agent.rs`. This keeps the server-side cache warm across longer pauses
in interactive sessions.

### 8.8 Prioritised action plan

All items from the original plan have been implemented:

| Priority | What | Where | Status |
| --- | --- | --- | --- |
| P0 | ZAI Anthropic-compatible `with_automatic_caching()` | `src/agent.rs` | ✅ Done |
| P1 | Generic `with_prompt_caching()` on OpenAI-compatible path | rig fork | ✅ Done |
| P1 | Plumb `additional_params` through `AgentBuilder` | rig fork (already existed) | ✅ Already done |
| P2 | OpenRouter provider pinning via `ProviderPreferences` | `src/agent.rs` + rig fork | ✅ Done |
| P2 | Switch Anthropic to 1-hour TTL | `src/agent.rs` | ✅ Done |
| P3 | OpenRouter conversation-history `cache_control` | rig fork | ✅ Done |
| P3 | OpenAI responses API `prompt_cache_key` | rig fork + `src/agent.rs` | ❌ Not implemented (picocode doesn't use responses API) |
