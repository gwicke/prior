# Thinking Audit

Status: **Partial.** The codebase can *request* thinking/reasoning from some LLM
providers, but it does **not** parse, capture, or surface the thinking *content*
that providers return.

## What the codebase does (request side — sending thinking params)

- `src/thinking.rs`
  - Defines `ThinkingLevel` (`Off`, `Low`, `Med`, `High`) and a `FromStr`
    parser accepting aliases (`off/none/0/false`, `low/min/1`, `med/medium/2`,
    `high/max/3`).
  - `mapping(provider, level)` maps a level to provider-specific
    `additional_params` (`serde_json::Value` merged into the request body),
    because `rig` 0.28 has no typed thinking-level API:
    - `anthropic` → `{"thinking": {"type": "enabled", "budget_tokens": 1024}}`
    - `openai`    → `{"reasoning_effort": "low"|"medium"|"high"}`
    - `openrouter`→ `{"reasoning": {"effort": "low"|"medium"|"high"}}`
    - anything else → `None` (no-op; provider unsupported)
  - `mapping_opt(provider, level: Option<&str>)` resolves an optional level
    string (from `AgentConfig::thinking_level`) to params; returns `None` when
    off/unsupported/unparseable.

- `src/agent.rs`
  - `AgentConfig::thinking_level: Option<String>` carries the selected level.
  - `create_agent` builds `think_json` via `thinking::mapping_opt` and injects
    it into the rig `Agent` through `build_rig_agent(..., think_json)`.
  - The agent is rebuilt on `/thinking` / `/model` changes so params take effect.

- `src/model_catalog.rs`
  - Tracks a `reasoning: bool` capability flag per model (from
    `supported_parameters` containing `"reasoning"`), shown in `/model`. This is
    catalog **metadata only** — it does not affect response handling.

- `src/repl.rs`
  - Exposes a `/thinking` slash command plus an overlay (`thinking_overlay_entries`)
    to set the level (off/low/med/high).

## What the codebase does NOT do (response side — surfacing thinking content)

- `Output::display_thinking(message)` (`src/output.rs`) is used **only as a
  generic "Thinking..." spinner**. It is never fed actual model thinking text;
  `agent.rs` calls `self.output.display_thinking("Thinking...")` and clears it
  with `stop_thinking()` once the response arrives. The `QuietOutput` and other
  `Output` impls treat it as spinner bookkeeping (`src/output.rs:101`, `:138`,
  `:184`, `:625`, `:763`).

- In `src/agent.rs` (`prompt()`), the provider response is consumed as
  `response.to_string()` and returned as plain text. There is **no extraction**
  of provider-specific thinking/reasoning fields, e.g.:
  - Anthropic `thinking` blocks (including `signature` / `redacted` variants)
  - OpenAI `reasoning` content
  - OpenRouter reasoning output

- A repository-wide search for `reasoning`, `"thinking"`, `signature`,
  `redacted` confirms the only hits are the request params, the catalog flag,
  and the spinner. None of them parse returned thinking content.

## Summary

| Concern                         | Supported? | Notes                                              |
|---------------------------------|------------|----------------------------------------------------|
| Request thinking (level)        | Yes        | Anthropic / OpenAI / OpenRouter via `additional_params` |
| Catalog `reasoning` capability  | Yes        | Display-only metadata in `/model`                  |
| `/thinking` command + overlay   | Yes        | Set level off/low/med/high                         |
| Surface returned thinking text  | **No**     | `display_thinking` is a spinner, not real content  |
| Extract provider thinking blocks| **No**     | Response handled as `response.to_string()` only    |

## If surfacing thinking content is desired

1. Capture the raw provider response (not just `response.to_string()`) in
   `src/agent.rs::prompt`.
2. Extract provider-specific thinking fields (Anthropic `thinking`/`signature`,
   OpenAI `reasoning`, OpenRouter reasoning, etc.).
3. Route that text through a new `Output` method (e.g. `display_thinking_content`)
   that is **gated by `thinking_level`**, replacing/augmenting the current
   spinner-only `display_thinking`.
