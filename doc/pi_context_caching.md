# pi — Context / Prompt Caching Support

This documents how the `pi` coding agent (TypeScript, `packages/ai`) emits
provider-specific caching hints for prefix-cache reuse across consecutive
turns. It was verified against the source in `pi/packages/ai` (the checkout in
this repo). The earlier `pi_provider_caching.md` is a closely related note;
this file reorganizes the findings by backend and adds the generic-OpenAI vs.
DeepSeek detail called out in the request.

## Global knobs (apply to every backend)

- **`CacheRetention`** — `"none" | "short" | "long"`, resolved by
  `resolveCacheRetention()` (defined in both `api/openai-responses.ts:55` and
  `api/openai-completions.ts:551`).
  - Defaults to `"short"`.
  - Set to `"long"` via the `PI_CACHE_RETENTION=long` env var, or per-request
    via `options.cacheRetention`.
  - `"none"` disables all cache hints (no `prompt_cache_key`, no `cache_control`,
    no session-affinity headers).
- **`options.sessionId`** — the per-conversation session id. It drives both the
  `prompt_cache_key` request body param and the session-affinity headers.
- **Usage read-back** — `cacheRead`, `cacheWrite`, `cacheWrite1h` in
  `api/types.ts`, plus the cost tables in `api/models.ts`.
- **`compat` flags** — `cacheControlFormat`, `sendSessionAffinityHeaders`,
  `sessionAffinityFormat`, `supportsLongCacheRetention`,
  `supportsCacheControlOnTools`. Auto-detected per provider/URL in each API
  module, overridable per-model in `providers/*.models.ts`. Field docs live in
  `api/types.ts:527-558`.

There is **no blanket "cache everything"** switch; hints are only emitted via
the paths below.

---

## 1. Generic OpenAI — `openai-responses` API

Provider: `openai` → `providers/openai.ts` uses `openAIResponsesApi()`,
base URL `https://api.openai.com/v1`. (This is the path the default `openai`
provider takes. `azure-openai-responses` and `openai-codex-responses` are also
Responses-API backends and behave the same way for caching.)

File: `api/openai-responses.ts`

- **`prompt_cache_key`** — sent whenever retention ≠ `"none"` (i.e. on the
  default `"short"` *and* `"long"`). Value is `clampOpenAIPromptCacheKey(sessionId)`,
  the session id clamped to **64 chars** (`api/openai-prompt-cache.ts:1`).
  `buildParams` line 245.
- **`prompt_cache_retention: "24h"`** — sent only when retention is `"long"`
  *and* `supportsLongCacheRetention` (default `true` for OpenAI; line 69/78).
- **Session-affinity headers** — when a `sessionId` is present, the Responses
  path sends them **unconditionally** (not gated by `sendSessionAffinityHeaders`):
  - `openai` format (auto-detected for non-OpenRouter base URLs):
    `session_id`, `x-client-request-id` (line 213-216).
  - `openrouter` format: `x-session-id`.
- **No Anthropic-style `cache_control` markers** — the Responses API performs
  automatic prefix caching keyed off `prompt_cache_key` + `prompt_cache_retention`;
  pi does not attach `cache_control` blocks here.

Net effect: the default OpenAI backend gets cache hints on *every* non-`none`
turn via `prompt_cache_key`, with 24h retention when `PI_CACHE_RETENTION=long`.

> **Azure OpenAI Responses & OpenAI Codex Responses:** both also ride the
> Responses API, so they share the `prompt_cache_retention`/`session-affinity`
> behavior above. One difference: they include `prompt_cache_key` on *every*
> request unconditionally (`azure-openai-responses.ts:264`,
> `openai-codex-responses.ts:501`), passing `undefined` when no `sessionId` is
> set, rather than omitting the field when retention is `"none"` like the
> generic Responses path does. Functionally identical when a session id exists.

---

## 2. DeepSeek — `openai-completions` API

Provider: `deepseek` → `providers/deepseek.ts` uses `openAICompletionsApi()`,
base URL `https://api.deepseek.com`. Model catalog `providers/deepseek.models.ts`
(add `deepseek-v4-flash`, `deepseek-v4-pro`) sets only reasoning-related compat
(`requiresReasoningContentOnAssistantMessages`, `thinkingFormat:"deepseek"`) —
**no caching override**, so the generic completions defaults apply.

File: `api/openai-completions.ts`

Because DeepSeek rides the **Completions** API (not the Responses API), the
caching behavior is materially different from generic OpenAI:

- **`prompt_cache_key`** — emitted **only** when
  `cacheRetention === "long" && compat.supportsLongCacheRetention`
  (`buildParams` line 560-564). On the default `"short"` retention, DeepSeek
  receives **no** `prompt_cache_key`. (`supportsLongCacheRetention` is `true`
  for DeepSeek — it is only forced `false` for Together / Cloudflare Workers /
  Cloudflare AI Gateway / NVIDIA / Ant-Ling; line 1260.)
- **`prompt_cache_retention: "24h"`** — same gate: only on `"long"` retention
  (line 565).
- **No `cache_control` markers** — `cacheControlFormat` is `undefined` for
  DeepSeek (only set to `"anthropic"` for OpenRouter routing to `anthropic/*`
  models, line 1228), so `applyAnthropicCacheControl()` is a no-op
  (`getCompatCacheControl` early-returns at line 743).
- **No session-affinity headers** — `sendSessionAffinityHeaders` defaults to
  `false` for the completions path (line 521 gate, 1258), and DeepSeek's model
  catalog does not override it.

**Caveat:** `prompt_cache_key` / `prompt_cache_retention` are OpenAI *Responses*
semantics; the chat-completions surface caches prefixes automatically on the
DeepSeek side (server-side, ~5-minute TTL). So pi's `long`-retention params are
passed to the completions endpoint but are effectively advisory — DeepSeek's real
prefix caching happens regardless and is reported back via cached-token usage
(the DeepSeek model entries do carry `cost.cacheRead` pricing).

Takeaway: to get any cache hint through to DeepSeek you must run with
`PI_CACHE_RETENTION=long`; with the default `short` retention DeepSeek gets no
explicit hints from pi at all.

---

## 3. OpenRouter — `openai-completions` API

Provider: `openrouter` → `providers/openrouter.ts` uses `openAICompletionsApi()`,
base URL `https://openrouter.ai/api/v1`.

File: `api/openai-completions.ts`

- **Anthropic-style `cache_control`** — only when the model id starts with
  `anthropic/` (e.g. `anthropic/claude-...`): `cacheControlFormat` is set to
  `"anthropic"` (line 1228), which enables `applyAnthropicCacheControl()`
  (line 751). It attaches `{ type: "ephemeral", ttl?: "1h" }` to:
  - the system/developer message (`addCacheControlToSystemPrompt`),
  - the last tool definition (`addCacheControlToLastTool`),
  - the last user/assistant text block (`addCacheControlToLastConversationMessage`).
  `ttl: "1h"` is added for `"long"` retention.
  Non-Anthropic OpenRouter models get no `cache_control` markers.
- **`prompt_cache_key` / `prompt_cache_retention: "24h"`** — same as DeepSeek:
  emitted only on `"long"` retention (`supportsLongCacheRetention` defaults
  `true` for OpenRouter).
- **Session-affinity header** — `sessionAffinityFormat: "openrouter"` →
  `x-session-id` (line 522-523), but only when `sendSessionAffinityHeaders` is
  true, which defaults to `false` on the completions path (line 521/1258).
  Override per-model if your OpenRouter setup benefits from sticky sessions.

Net: OpenRouter through pi is most useful for caching when you route to
`anthropic/*` models (explicit `cache_control` breakpoints) and/or run with
`PI_CACHE_RETENTION=long` (for `prompt_cache_key` + 24h retention).

---

## 4. Anthropic — `anthropic-messages` API

File: `api/anthropic-messages.ts`

- `cache_control` markers `{ type: "ephemeral", ttl?: "1h" }` are attached as
  `ni_control` to:
  - the system prompt block,
  - the last tool definition (only when `compat.supportsCacheControlOnTools`,
    default `true`),
  - the last user/assistant message block.
  `ttl: "1h"` is added for `"long"` retention (line 66).
- Usage read back from `ni_read_input_tokens`, `ni_creation_input_tokens`,
  and `ni_creation.ephemeral_1h_input_tokens`.
- `sendSessionAffinityHeaders` defaults to `false` for Anthropic, so the
  `x-session-affinity` header is not sent unless a model overrides it (line 898).

---

## 5. Amazon Bedrock — `bedrock-converse-stream` API

File: `api/bedrock-converse-stream.ts`

- `supportsPromptCaching()` (line 662) returns true for Claude 3.5 Sonnet/Haiku,
  3.7 Sonnet, 4.x, and 5.x; otherwise false *unless* `AWS_BEDROCK_FORCE_CACHE=1`
  is set (line 669). Amazon Nova models auto-cache and need no explicit points
  (note at line 660).
- When enabled and retention ≠ `"none"`, `cachePoint: { type: DEFAULT, ttl?: ONE_HOUR }`
  is inserted after the system prompt (`buildSystemPrompt` line 706) and on the
  last message (line 893). `ttl: ONE_HOUR` is added for `"long"` retention.

---

## 6. Mistral — `mistral-conversations` API

File: `api/mistral-conversations.ts`

- `x-affinity` header set to `sessionId` for KV-cache / prefix-cache reuse when
  retention ≠ `"none"` and a session id exists (line 227-231). Caller-provided
  `x-affinity` is respected.
- `promptCacheKey = sessionId` body param emitted under the same condition
  (`shouldUsePromptCaching`, line 258/270-272).
- Cached tokens are read back from `cached_tokens` / `cachedTokens` in usage and
  mapped to `usage.cacheRead` (line 339). `cacheWrite` is left at 0.

---

## 7. Gateways / providers that force session-affinity headers

These backends set `sendSessionAffinityHeaders: true` in their model catalogs,
so the completions path (line 521) emits `session_id` + `x-client-request-id`
(+ `x-session-affinity`) for `openai` format, or `x-session-id` for an
OpenRouter-formatted gateway. They pair the header with a `prompt_cache_key`
when retention is `long`.

- **Cloudflare AI Gateway** — `providers/cloudflare-ai-gateway.models.ts`:
  `sendSessionAffinityHeaders: true`; most models also set
  `supportsLongCacheRetention: false` (so `prompt_cache_retention: "24h"` is not
  sent; only `"long"` still drives `prompt_cache_key`).
- **Cloudflare Workers AI** — `providers/cloudflare-workers-ai.models.ts`:
  `sendSessionAffinityHeaders: true`, `supportsLongCacheRetention: false`.
- **Fireworks** — `providers/fireworks.models.ts`:
  `sendSessionAffinityHeaders: true`, `supportsLongCacheRetention: false`,
  `supportsCacheControlOnTools: false`. Fireworks exposes explicit prompt-cache
  breakpoints, so the session-affinity header is the active hint here.

---

## 8. Backends with no explicit hints from pi

`google` / `google-vertex` (Gemini), `xai` (Grok), `together`, `groq`,
`moonshotai` / `moonshotai-cn`, `zai`, `nvidia`, `cerebras`, `ant-ling`,
`minimax`, `kimi`, `moonshot`, `xiaomi`, etc. — pi does **not** emit
`cache_control` markers, session-affinity headers, or `prompt_cache_key` for
these by default (they fall into the generic completions defaults with
`cacheControlFormat: undefined`, `sendSessionAffinityHeaders: false`). Any
prefix caching they do is provider-side and automatic; it is reported back only
if the provider returns cached-token usage that pi's usage mapping picks up.

---

## Quick reference table

| Backend (API) | Cache hint(s) emitted | Triggered by |
|---|---|---|
| OpenAI (`openai-responses`) | `prompt_cache_key` + `prompt_cache_retention:24h` + session-affinity headers | key/headers on any non-`none`; 24h on `long` |
| Azure / Codex Responses | `prompt_cache_key` (always) + `prompt_cache_retention:24h` + session-affinity headers | key present every request (undefined if no session); 24h on `long` |
| DeepSeek (`openai-completions`) | `prompt_cache_key` + `prompt_cache_retention:24h` | **only `long` retention** |
| OpenRouter (`openai-completions`) | `cache_control` (anthropic/* only) + `prompt_cache_key` + 24h | markers always for anthropic/*; key/24h on `long` |
| Anthropic (`anthropic-messages`) | `ni_control` markers | retention ≠ `none` (ttl 1h on `long`) |
| Bedrock (`bedrock-converse-stream`) | `cachePoint` blocks | Claude 3.5+/3.7/4/5 or `AWS_BEDROCK_FORCE_CACHE=1`; ttl 1h on `long` |
| Mistral (`mistral-conversations`) | `x-affinity` header + `promptCacheKey` | retention ≠ `none` |
| Cloudflare GW / Workers / Fireworks | session-affinity headers (+ `prompt_cache_key` on `long`) | retention ≠ `none` (GW/Workers/Fireworks set `sendSessionAffinityHeaders`) |
| Others (Gemini, Grok, Together, …) | none from pi | relies on provider auto-caching |

### Bottom line for the two backends you asked about

- **Generic OpenAI (Responses API):** gets `prompt_cache_key` (and thus cache
  reuse) on *every* turn by default, with 24h retention once `PI_CACHE_RETENTION=long`.
- **DeepSeek (Completions API):** gets *no* cache hint on the default `short`
  retention; only `prompt_cache_key` + `prompt_cache_retention:24h` when run
  with `PI_CACHE_RETENTION=long`, and those params are advisory on the
  chat-completions surface since DeepSeek caches prefixes server-side anyway.
