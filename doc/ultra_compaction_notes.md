# Ultra-Compact Compaction — Algorithm Notes

Reference summary of the algorithms implemented in `pi-ultra-compact` (`extensions/engine.ts`,
`extensions/index.ts`, `extensions/utils.ts`, `extensions/types/index.ts`). Written for evaluation /
porting into a separate Rust project.

The whole package is a **cheap "compaction" strategy** for an LLM agent's conversation history: it
keeps the context window under a target size while preserving *critical* information (goals,
decisions, errors, file paths, next steps). The cheapness comes from the fact that the heavy lifting
is done with **deterministic, regex- and heuristic-based transforms** — no LLM call is required
unless the operator explicitly opts in (`useLLM`).

---

## 1. Data model

```ts
interface Message {
  id: string;
  role: "user" | "assistant" | "tool" | "system";
  content: string | ({ type: "text"; text: string } |
                     { type: "image"; source?: ... })[];
  timestamp: number;
  tokens?: number;
}
```

`messageContent(msg)` normalizes content to a string:

- `string` → returned as-is.
- array → text blocks concatenated (non-text blocks rendered as `[type]`).

This scalar text is the unit every downstream algorithm operates on.

---

## 2. Token estimation (content-aware counting)

`estimateTokens(messages): number` — a fast proxy for token count, *not* a real tokenizer. It is
purely character-length based with four content-type ratios.

For each message:

1. `len = text.length`.
2. `codeBlockCount = count of "```"` (fence) occurrences; `hasCodeBlocks = count > 1`.
3. `nonSpace = count of non-whitespace chars`.
4. `whitespaceRatio = (len - nonSpace) / len` (0 when none).
5. Choose a chars-per-token ratio `r`:
   - `hasCodeBlocks` → `3.5` (dense special chars)
   - `whitespaceRatio > 0.3` → `6.0` (logs, tables, lots of spacing)
   - `nonSpace / len > 0.85` → `3.5` (dense keys / code-like)
   - else → `4.5` (standard prose)
6. `total += ceil(len / r)`.

**Why it matters:** every other threshold (tier selection, eviction budget, importance scaling,
circuit-breaker fallback) is expressed in these estimated tokens, so the ratio table *is* the
policy. Swapping in a real BPE tokenizer is the most impactful accuracy upgrade for a Rust port, but
the relative logic does not change.

---

## 3. Context-window detection & threshold derivation

`contextWindow` is the source of truth for every percentage.

- `DEFAULT_CONTEXT_WINDOW = 128000` (conservative fallback).
- `detectContextWindow(modelName?)`:
  1. If no name → fallback `128000`.
  2. Lowercase the name; first match against an **exact model table** (e.g. `claude-sonnet-4` →
     200000, `gemini-3.5-flash` → 1e6, `deepseek-r1` → 65536, `codestral` → 256000, …). Matching is
     substring `name.includes(model)`.
  3. Else **family fallback** (`claude`→200000, `gpt`→128000, `gemini`→1e6, `deepseek`→128000,
     `llama`/`mistral`/`codestral`→128000).
  4. Else `128000`.
- `detectModelFamily(name)` — same substring approach → provider string
  (anthropic/openai/google/deepseek/meta/mistral/alibaba/…). Used only for recommendations, not for
  compaction behavior.

Threshold derivation in the constructor:

- `thresholdTokens = config.thresholdTokens ?? 100000`.
- If the caller did **not** set `thresholdTokens` *and* a `contextWindow` is known (from Pi metadata
  or constructor), then `thresholdTokens = floor(contextWindow * 0.8)`.
- `reconfigure(modelName, contextWindow)` recomputes the same way, preserving the caller's override
  if one exists. This is what makes thresholds follow the active model at `session_start` /
  `model_select` time.

> Note (discrepancy to be aware of): `hardWatermark` **default in code is `0.5`** (see
> `types/index.ts` comment and `engine.ts` `?? 0.5`), even though `README` documents `0.95`. Mirror
> whichever the Rust port intends and make it explicit.

---

## 4. Triggering — when to compact

There are **three** independent gate definitions in the code; a port should pick one consistent
policy rather than carry all three.

### 4a. `shouldCompact(currentTokens)` — dual gate

- Gate 1 (percentage): `percentThreshold = floor(contextWindow * 0.6)`. Fires if
  `currentTokens >= percentThreshold`.
- Gate 2 (hard cap): `hardCap = floor(contextWindow * hardWatermark)` (default `0.5`). Fires if
  `currentTokens >= hardCap`.
- Either gate true → compact.

With the default `hardWatermark = 0.5`, **Gate 2 dominates** (fires at 50%, earlier than the 60% of
Gate 1).

### 4b. `determineTier(messages)` — generative tier

- `ratio = estimateTokens(messages) / contextWindow`.
- `ratio >= 0.9` → `CompactionTier.FULL`
- `ratio >= 0.6` → `CompactionTier.MICRO`
- else → `CompactionTier.NONE`

### 4c. `shouldCompactDefaultThreshold()` — preemptive cap

- If a `contextWindow` was explicitly supplied (Pi metadata), returns the raw `thresholdTokens`.
- Otherwise returns `min(thresholdTokens, floor(preemptiveWatermark * contextWindow))` where
  `preemptiveWatermark = 0.7`.

### 4d. Preemptive projection (in `index.ts`)

At the auto-compaction hook, the *next* turn is projected:
`projectedTokens = currentTokens + outputHeadroom` (default `4096`).

- Manual `/ultracompact` uses `currentTokens` directly (always compacts).
- Auto uses `projectedTokens` as the argument to `shouldCompact`, i.e. it fires **before** the next
  turn's response headroom would overflow the window.

---

## 5. Message importance scoring

`calculateMessageImportance(message, seenPatterns): number` → score in `[0, 1]`. This single score
drives classification (Section 7) and level-4 eviction (Section 8.4). It combines four signals:

### 5a. Keyword-signal (max, not sum)

`IMPORTANCE_SIGNALS.keywords` is an ordered list of `{ pattern, weight }`: GOAL(1.0),
DECISION(0.95), ERROR(0.9), SOLUTION(0.85), DISCOVERY(0.8), CONSTRAINT(0.75), FILE(0.7),
CHANGE(0.65), NEXT(0.6). Each pattern is a regex like
`/\b(?:GOAL|OBJECTIVE|TARGET|WANT TO|TRYING TO)\b:?\s*(.+)/i`. The score is the **maximum** weight
among all matching patterns (not additive).

### 5b. Redundancy penalty (entropy / lost-in-the-middle mitigation)

`extractPatternKey(text)` returns a key for *repeatable* content:

- tool file-read → `"read:" + filePath`
- command output → `"cmd:" + commandName` (or `cmd:generic`)
- else `null`.

If the key is non-null and already in `seenPatterns`, `score *= 0.4`. The key is then inserted. This
is the "entropy / redundancy penalty" from the README: repeated reads and repeated commands are
progressively de-valued, fighting context bloat in long histories.

### 5c. Content-type multipliers (multiply the running score)

- contains ```` ``` ```` → `* 1.3` (codeBlock)
- matches `[\w./\\-]+\.(ts|js|tsx|jsx|py|rs|go)` → `* 1.2` (filePath)
- `role === "tool"` → `* 1.15` (toolCall)
- contains `"Error:"` or `"error"` → `* 1.25` (errorLog)

### 5d. Length decay & floor

- `text.length > 2000` → `* 0.9` (multiLine).
- Floor boost: if `role === "tool"` or contains ```` ``` ````, `score = max(score, 0.5)` — tool
  output and code are never scored below 0.5.
- Final: `score = min(1, score)`.

**Rust note:** `seenPatterns` must be threaded **in order** through a message scan (it is a `Set`
mutated as a side effect), so importance is positional — the first occurrence of a read/command
scores higher than later ones.

---

## 6. Pre-processing pipeline (`preprocessMessages`)

Runs before classification / summarization. Three ordered steps:

### 6a. Deduplicate (`deduplicateMessages`)

Key = `role + ":" + content.substring(0, 100)`. Keep the first occurrence; if a later duplicate
appears, **remove the earlier one and keep the latest** (it splices out the earlier index, then
pushes the new message). Worst case O(n²) due to `findIndex` per duplicate — fine for agent
histories, but a Rust port should consider a doubly-linked removal or two-pass approach.

### 6b. Tool-output pruning (`pruneToolOutputs`)

- `protectRecentCount = max(5, floor(len * 0.2))`. A tail window is always kept verbatim.
- For each message `i`:
  - **tool role or `isToolOutput(content)`** (matches `result|output|response| tool_result` prefix,
    `$ | > | bash|shell|terminal|command` prefix, or `[N lines output]`):
    - recent (`i >= len - protectRecentCount`) → keep;
    - old → `summarizeToolOutput(content)` (one line).
  - **file read** (`isFileRead`): dedupe via `fileReads` map; keep only the **first** read of a
    path, or any re-read **more than 10 messages after** the previous one; otherwise drop.
  - everything else → keep.
- `summarizeToolOutput`: reports `lineCount`, exit-code if present, and first 80 chars; variant text
  for *error*, *success* (`✓`/`success`), or generic *output*.

### 6c. Empty removal

Drop messages whose normalized content is blank.

---

## 7. Classification (`classifyMessages`)

Splits preprocessed messages into `protected` vs `compressible` (a `discardable` bucket exists but
is currently unused / always empty).

- The **first** `system` message is always protected (`systemProtected` latch).
- A message is protected if any of:
  - it is that first system message;
  - `calculateMessageImportance(...) > 0.7`;
  - `shouldProtectContent(content)`:
    - two or more code fences (`split("```").length > 2`),
    - a path matching `\b(?:src|lib|extensions|tests?)\/[\w./\\-]+\.(ts|js|tsx|jsx)`,
    - `"Error:"` **and** `"at "` (a stack trace).
- Otherwise → `compressible`.

### 7a. Recency budget protection (`protectRecentByTokenBudget`)

From the `compressible` set, walk **backwards from the most recent** message, accumulating
`estimateTokens` until a `tokenBudget` (hard-coded `20000`) is hit. Those recent messages are pulled
back into `protected`. Final: `protected = [...system/importance/pattern, ...recentProtected]`;
`compressible = remaining`.

This is a cheap stand-in for "lost in the middle": it guarantees the most recent context is always
retained regardless of importance score.

---

## 8. Graduated eviction — the core cheap strategy

`evictGradually(messages, tokenBudget, maxLevel)` strips content in
**four ordered, increasingly destructive levels**, stopping as soon as the estimated token count is
at or below `tokenBudget`. Each level re-measures and early returns with `levelUsed`. This is what
makes the strategy "cheap": it prefers removing *cheap-to-lose* tokens (reasoning, bulk logs) before
dropping real conversation.

Levels (`EvictionLevel` enum 1–4):

### Level 1 — `STRIP_REASONING`

`stripReasoningBlocks`: for `assistant` messages with array content, drop blocks where
`type === "thinking"`. If that leaves no content, replace with `"[reasoning stripped]"`. (Harmless:
reasoning is not needed after the turn.)

### Level 2 — `STRIP_BULK_OUTPUT`

`stripBulkToolOutputs`: for `tool` messages **only**, and **only if** `content.length >= 5000`
**and** `lines > 100`, replace with a marker keeping the first 3000 chars:
`[tool output truncated: N lines, M chars]\n<first 3000 chars>`. The dual condition avoids
truncating compact single-line tool results.

### Level 3 — `STRIP_ARTIFACTS`

`stripArtifactToolOutputs`: for `tool` messages, if `containsErrorIndicators`
(`Error:`/`error:`/`failed`/`Failed`/`exit code`/`exit status`/`SyntaxError`/ `TypeError`) →
**keep** (errors are critical). Otherwise collapse to
`[tool result stripped: N lines | <first 120 chars>]`.

### Level 4 — `FULL_REMOVAL`

`removeOldCompressibleMessages(messages, tokenBudget)`:

1. Partition into `protected` (role `user`/`system`, or `importance > 0.7`) and `candidates`
   (everything else).
2. Start from protected; then re-add candidates **newest-first** while the running `estimateTokens`
   total stays ≤ `tokenBudget`.
3. Restore original ordering via a `keptIds` set filter.

- **Inviolable roles:** `user` and `system` are never candidates, so they can never be evicted here.

If even Level 4 does not reach the budget, the (still over-budget) result is returned with
`levelUsed = maxLevel`.

`microCompact(messages)` is a thin wrapper that calls `evictGradually` with
`maxLevel = STRIP_BULK_OUTPUT` (levels 1–2 only) and a `targetBudget = floor(contextWindow * 0.5)`.
It runs with **no LLM** and is the MICRO tier.

---

## 9. Full summarization (FULL tier)

`generateSummary(messages, previousSummary?)`:

1. `preprocessMessages` → `classifyMessages`.
2. `tokensBefore = estimateTokens(messages)`.
3. `targetBudget = calculateKeepTokens(tokensBefore)`
   `= min(floor(tokensBefore * keepPercentage), maxKeepTokens)` (defaults `keepPercentage = 0.3`,
   `maxKeepTokens = 30000`).
4. Pre-condition input with graduated eviction **on the compressible set**
   (`evictGradually(compressible, targetBudget, maxEvictionLevel)`), then call
   `generateSummaryFromRemaining(evicted, protected, previousSummary)`. (If everything was
   classified protected, summarize the protected set directly; if nothing, reuse `previousSummary`.)
5. `tokensAfter = estimateTokens(protected) + estimateTokens(summary)`.
6. Record the run in `compressionHistory` (capped at the last 5 entries).

`generateSummaryFromRemaining`:

- If `useLLM` and an `llmSummarize(text) => Promise<string>` callback is set, build the prompt from
  `compressible` (`[role] <first 500 chars>`, joined by `\n---\n`), call it, and prepend
  `previousSummary` under a `## Previous Context` header.
  **Any failure falls back to the heuristic below.**
- Else → `generateStructuredSummary` (Section 10).

> Important cheap-data nuance: eviction is applied to `compressible` only; `protected` messages are
> passed through untouched to the summary builder so no critical content is ever fed into a
> destructive transform.

---

## 10. Structured summary & "Token-Compressed Language"

`generateStructuredSummary` emits a dense, symbol-led markdown using **LLM shorthand** (very high
token density). Sections:

- `#Prev` — previous summary (iterative update, if any).
- `#G:` goals (`extractGoals`).
- `#D:` decisions (`extractDecisions`).
- `#E:` errors → solutions (`extractErrors`, joined as `error → solution`).
- `#F:` file ops (`R:` reads, `M:` modified, comma-joined).
- `#N:` next steps (`extractNextSteps`, capped at 5).
- `#C` conversation digest (`compressConversation`).

Each literal is run through `compressText`:

- strip stopwords (`the a an and is are was were be been being`) and fillers
  (`basically actually really just simply essentially literally`);
- replace `\s+and\s+` → `" + "`;
- collapse whitespace.

### Information extraction (regex)

- `extractGoals` / `extractDecisions` / `extractNextSteps`: scan all messages for their category
  keyword pattern, capture group 1, dedupe via `Set`, slice.
- `extractErrors`: match `errorPattern` + optionally `solutionPattern` on the same message; emit
  `error → solution` or just `error`; dedupe.
- `extractFileOperations`: two `readPatterns` and two `modifiedPatterns` (regex arrays using
  `matchAll`), collect captured paths, dedupe. This is the canonical source of `readFiles` /
  `modifiedFiles` in the result.
- `compressConversation`: segment by `extractTopic` (explicit `TOPIC|SUBJECT| ABOUT:` or the first
  sentence if `< 100` chars); each message reduced by `compressMessage` (code fences →
  `[code: <first 50 chars of line 2>]`, whitespace collapsed, cap 200 chars + `...`).

All extractor patterns are centralized in `utils.ts` (`KEYWORD_PATTERNS`, `extractByPattern`,
`containsErrorIndicators`) and mirrored in `engine.ts` (`IMPORTANCE_SIGNALS`). A Rust port should
keep a single source of truth for these patterns.

---

## 11. Iterative / cache-aware summaries

`index.ts` implements **cache-aware compaction** to keep the prompt-cache prefix stable (saves
prefill cost on the API):

- If `cacheAware` and a `previousSummary` exists, that summary becomes an immutable
  `cacheAwarePrefix` and `snapshotPreviousSummary` is set to `undefined` so the engine does **not**
  re-summarize old content.
- For MICRO tier: output is `cacheAwarePrefix + "\n\n## Chat\n" + conversationText`.
- For FULL tier: `result.summary = cacheAwarePrefix + "\n\n" + result.summary`.
- Net effect: only the *new* delta is paid for; the historical prefix is reused verbatim by the
  provider's prompt cache.

---

## 12. Safety systems

### 12a. Snapshot / rollback

At the compact hook, `messagesToCompact` is deep-cloned via `JSON.parse(JSON.stringify(...))`. The
engine operates on the snapshot; the original array is never mutated, so any thrown error leaves the
session intact.

### 12b. Circuit breaker (module-level, per session)

State: `compactionFailures`, `breakerTrippedAtTurn`, `currentTurn` (incremented every hook entry).

- On entry, if `breakerTrippedAtTurn` is set **and**
  `currentTurn - tripped < circuitBreakerCooldown` (default 5) → return `undefined` (use Pi's
  default compaction) without attempting ultra-compact.
- On a successful compaction → `compactionFailures = 0`.
- On exception → `compactionFailures++`; when it reaches `circuitBreakerMaxFailures` (default 3):
  - set `breakerTrippedAtTurn = currentTurn`;
  - perform **lossy truncation** (last resort): keep all `system` messages + the last
    `tailKeep = 10` non-system messages, each truncated to 500 chars, with a
    `[earlier history truncated — circuit breaker engaged]` marker.
- Otherwise (failure below threshold) → return `undefined` (fall back to Pi default).

The design guarantees the session **never dies** from a bad compaction: worst case is a crude
truncation that preserves the system prompt and recent tail.

### 12c. Inviolable content

`user` and `system` roles are excluded from eviction/removal at every level (Section 8.4,
classification). Empty-summary results throw (caught by the breaker).

---

## 13. End-to-end flow (auto hook in `index.ts`)

```text
session_before_compact(event, ctx)
  ├─ increment currentTurn; check circuit-breaker cooldown → maybe bail
  ├─ capture model from ctx; reconfigure engine (follow active model)
  ├─ snapshot messagesToCompact (deep clone)
  ├─ project tokens (current + outputHeadroom) for auto; raw for manual
  ├─ if auto and !shouldCompact(projected) → return (no compaction)
  ├─ cache-aware: lift previousSummary to immutable prefix
  ├─ tier =
  │    manual                       → FULL
  │    determineTier == MICRO (1)   → microCompact (no LLM)
  │    else                         → generateSummary (FULL)
  ├─ validate summary non-empty
  ├─ on success → return { compaction: { summary, … } }
  └─ on failure → circuit breaker; lossy truncation or Pi default
```

---

## 14. Notes for a Rust port

- **No dependency on a real tokenizer is required to reproduce behavior** — the four-ratio
  `estimateTokens` is sufficient and intentionally cheap. A Rust version can keep that, or plug in
  `tiktoken`-style counts behind the same trait (`fn estimate_tokens(&[Message]) -> usize`).
- **Data structures:** `Message` with an `enum Role` and `Content` as either `String` or
  `Vec<ContentBlock>`. Replace the `JSON.parse(JSON.stringify)` snapshot with a `Clone` (derive
  `Clone` on all message types).
- **Side-effecting scoring:** `calculateMessageImportance` mutates a `HashSet` (`seenPatterns`) as
  it scans — preserve ordering (iterate in message order, not parallel). The "entropy/redundancy
  penalty" is simply "second+ occurrence of a read/command key × 0.4".
- **Regex:** centralize the `KEYWORD_PATTERNS` / `IMPORTANCE_SIGNALS` / `TOOL_OUTPUT_PATTERNS` in
  one module (the JS code duplicates them across `engine.ts` and `utils.ts`). Use the `regex` crate
  with `RegexSet` where multiple alternatives are tested, and `Captures` for group extraction.
- **Complexity:** `deduplicateMessages` is O(n²) in `index.ts`; replace with a two-pass or
  linked-removal approach. Everything else is O(n) per scan with `estimateTokens` called repeatedly
  inside eviction loops — cache per-message token estimates across the eviction passes (the README
  mentions an LRU token cache that is **not present in the current code**, so add one if repeating
  estimates is hot).
- **Config surface to expose:** `contextWindow` (auto from model metadata or explicit),
  `keepPercentage`, `maxKeepTokens`, `preemptiveWatermark`, `hardWatermark`, `outputHeadroom`,
  `maxEvictionLevel`, `cacheAware`, `circuitBreakerMaxFailures`, `circuitBreakerCooldown`,
  `useLLM` + `llm_summarize` callback.
- **Single policy decision:** reconcile the three trigger definitions (`shouldCompact` 60%/50%,
  `determineTier` 60%/90%, `shouldCompactDefaultThreshold` 70%). They are not fully consistent in
  the source; pick one coherent rule (recommend: percentage gate + hard-cap, plus projected-headroom
  for preemption).
- **Safety you must keep:** deep-clone before mutating, never evict `user`/ `system`, and always
  have a lossy-truncation fallback so compaction can never crash the session.

```text
```
