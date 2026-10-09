# Provider Caching Hints

The pi agent sends several provider-specific caching hints to improve
turn-to-turn cache reuse (prefix caching across consecutive requests).

All of this is driven by:

- per-provider `compat` configuration in `packages/ai/src/types.ts`, and
- a `CacheRetention` preference (`"none" | "short" | "long"`), resolved per
  request via `resolveCacheRetention()`, which defaults to `"short"` but honors
  the `PI_CACHE_RETENTION=long` environment variable.

The read-side counterparts are the `Usage` fields `cacheRead`, `cacheWrite`,
`cacheWrite1h` (`packages/ai/src/types.ts`) and the cost model in
`packages/ai/src/models.ts`.

## 1. Anthropic Messages API — `ni_control` markers

File: `packages/ai/src/api/anthropic-messages.ts`

`getCacheControl()` emits `{ type: "ephemeral", ttl?: "1h" }`. These are
attached as `ni_control` to:

- the system prompt block,
- the last tool definition (only when `compat.supportsCacheControlOnTools`),
- the last user/assistant message block.

Long retention adds `ttl: "1h"`. Usage is read back from
`ni_read_input_tokens`, `ni_creation_input_tokens`, and
`ni_creation.ephemeral_1h_input_tokens`.

## 2. OpenAI-Compatible Completions — Anthropic-style `cache_control`

File: `packages/ai/src/api/openai-completions.ts`

When `compat.cacheControlFormat === "anthropic"` and retention is not `"none"`,
`applyAnthropicCacheControl()` attaches `{ type: "ephemeral", ttl?: "1h" }` to:

- the system/developer message (`addCacheControlToSystemPrompt`),
- the last tool definition (`addCacheControlToLastTool`),
- the last user/assistant text content block
  (`addCacheControlToLastConversationMessage`).

`ttl: "1h"` is added for long retention.

## 3. Session-affinity headers + `prompt_cache_key`

Files: `packages/ai/src/api/openai-responses.ts`,
`packages/ai/src/api/openai-completions.ts`,
`packages/ai/src/api/openai-prompt-cache.ts`

For OpenAI Responses/Completions, when retention is not `"none"`, a
`sessionId` is forwarded as:

- OpenAI: `session_id`, `x-client-request-id`, `x-session-affinity`
  (or the `openai-nosession` variant without `session_id`),
- OpenRouter: `x-session-id` (plus `ni-id` / `ni-affinity` on the completions
  path),

along with body params:

- `prompt_cache_key` — the session id, clamped to 64 chars by
  `clampOpenAIPromptCacheKey`,
- `prompt_cache_retention: "24h"` for long retention (only if
  `compat.supportsLongCacheRetention`).

The `sessionAffinityFormat` is auto-detected from the provider/base URL or set
explicitly in each provider's `compat` config.

## 4. Amazon Bedrock Converse — cache points

File: `packages/ai/src/api/bedrock-converse-stream.ts`

When `supportsPromptCaching` is true, `convertMessages` / `buildSystemPrompt`
insert `cachePoint: { type: DEFAULT, ttl?: ONE_HOUR }` after the system prompt
and on the last message. Long retention adds the 1-hour TTL.

## Notes

- There is no blanket "cache everything" marker. Hints are only applied via the
  `compat.*` flags and the `ni` / `cacheControlFormat` paths described above.
- The test `packages/ai/test/openrouter-cache-write-repro.test.ts` injects
  `cache_control` manually for a repro, but that is not production behavior.
- `CacheRetention` is defined in `packages/ai/src/types.ts`; the related
  `compat` flags are `cacheControlFormat`, `sendSessionAffinityHeaders`,
  `sessionAffinityFormat`, and `supportsLongCacheRetention`.
