# picocode / rig-core Provider Caching

This documents whether picocode or its upstream `rig-core` (pinned at
`0.28.0`, see `Cargo.toml` / `Cargo.lock`) supply provider-side caching hints
comparable to the pi agent (see `doc/pi_provider_caching.md`).

## Summary

| pi capability | rig-core 0.28.0 | picocode app |
| --- | --- | --- |
| Anthropic `cache_control` markers (system + last msg) | **Yes** (`apply_cache_control`) | **Not enabled** — `prompt_caching` defaults off and picocode never turns it on |
| Anthropic `ttl: "1h"` (long retention) | **No** — only `Ephemeral`, no TTL variant | n/a |
| Anthropic cache on last tool definition | **No** | n/a |
| OpenAI/OpenRouter `cache_control` / `prompt_cache_key` / session-affinity | **No** (send-side) | n/a |
| Amazon Bedrock cache points | **No** (no Bedrock provider in rig-core) | n/a |
| Read-back of cache usage (input/output tokens) | **Yes** for Anthropic, OpenAI, DeepSeek, Gemini | **Not surfaced** — picocode ignores `response.usage` |

Verdict: the only write-side caching hint capability anywhere in the stack is
rig-core's automatic Anthropic `cache_control`. picocode does **not** enable it,
and for every other provider (OpenAI, OpenRouter, DeepSeek, Gemini, Bedrock)
there is **no** send-side caching hint at all — only optional read-back of cache
token counts, which picocode also ignores.

## 1. rig-core — Anthropic `cache_control` (analog of pi §1, narrower)

File: `rig-core-0.28.0/src/providers/anthropic/completion.rs`

- `CacheControl` enum (line ~140) has a single variant `Ephemeral`. There is
  **no** `ttl` field, so rig-core cannot emit `ttl: "1h"` for long retention
  (unlike pi's `Long` retention).
- `CompletionModel.prompt_caching` defaults to `false` in both `new()` and
  `with_model()`; `with_prompt_caching()` flips it to `true`
  (lines ~672–725).
- `apply_cache_control(system, messages)` (lines ~821–844) is the analog of
  pi's `getCacheControl()`. It marks exactly two breakpoints:
  1. the system prompt block (`SystemContent::Text.cache_control`), and
  2. the last content block of the **last** message.
- It does **not** mark the last tool definition, so it is narrower than pi
  (pi caches the last tool when `supportsCacheControlOnTools`). Tools carry an
  optional `cache_control` field in the structs, but `apply_cache_control`
  never sets it.
- Used by both `completion.rs` (`if self.prompt_caching { apply_cache_control(...) }`,
  ~line 905) and `streaming.rs` (~line 185–187).
- Read-back: `CompletionResponse` exposes `cache_read_input_tokens` /
  `cache_creation_input_tokens` (lines ~92–122).

## 2. rig-core — OpenAI / OpenRouter (analog of pi §2 & §3: absent)

- `src/providers/openai/completion/` and `src/providers/openai/responses_api/`
  contain **no** `cache_control`, `prompt_cache_key`, `session_id`,
  `x-session-affinity`, or `ttl` on the request/send path.
- OpenAI read-back only: `responses_api/mod.rs` exposes `cached_tokens`
  (line ~514–528) — purely a Usage field, never sent.
- `src/providers/openrouter/` (client/completion/streaming) has **no** cache
  code at all; it reuses the OpenAI-compatible request shape and adds nothing
  cache-related.

## 3. rig-core — DeepSeek (read-back only)

File: `src/providers/deepseek.rs`

- `Usage` exposes `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`
  (lines ~138–153) and `cached_tokens` (line ~170). These are parsed from the
  response. There is **no** send-side cache hint (no `cache_control`,
  `prompt_cache_key`, session headers).

## 4. rig-core — Gemini (read-back only)

Files: `src/providers/gemini/completion.rs`, `src/providers/gemini/streaming.rs`

- Only read-back: `cached_content_token_count: Option<i32>` (completion.rs
  ~line 1062, streaming.rs ~line 20) feeds into the output-token accounting.
- No send-side cached-content handle / cache config in requests (the lone
  `cachedContent: Optional<String>` note at completion.rs ~line 1655 is a
  commented-out, unused field).

## 5. Amazon Bedrock (analog of pi §4: absent)

- rig-core 0.28.0 ships **no** Bedrock provider (`ls src/providers/` has no
  `bedrock` directory). There are no `cachePoint` / `ttl: ONE_HOUR` constructs.

## 6. picocode application level

Files: `src/agent.rs`, `src/main.rs`

- `create_agent()` builds each provider via `$client.agent(&model)` and then
  `build_rig_agent(...)` (agent.rs ~108–118). The Anthropic builder is never
  given `.with_prompt_caching()`, so even the rig-core Anthropic capability
  stays disabled. A repo-wide search for `prompt_caching`, `cache_control`,
  `apply_cache`, `with_cache` in `src/` returns nothing.
- picocode never reads or surfaces cache `Usage`. `CodeAgent::prompt()`
  (agent.rs ~32–53) only does `response.to_string()`; cache token counts from
  rig-core are discarded.
- The only occurrences of "cache" in picocode source are an unrelated **local
  model-catalog cache** (`src/model_catalog.rs`: a 5-minute `OnceLock` TTL for
  the `/model` overlay), not provider caching.

## Notes

- The behavior above is for the rig-core version actually used by picocode
  (`0.28.0`, pinned in `Cargo.lock`). A newer `0.39.0` also exists in the local
  cargo registry cache, but it is not referenced by this crate.
- In short: picocode currently provides **no** provider-side caching hints to
  any LLM. The single upstream mechanism (rig-core Anthropic `cache_control`)
  exists but is off by default and unused here.
