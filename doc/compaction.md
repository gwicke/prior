# Context Compaction — Design (v1: "cheap, frequent, minimal")

Status: **partially implemented**. The "Level 0" eager-drop compaction — "Option
D" in §3.5.6 — **shipped** as the `reasoning_in_context` setting (default
`false`): when `false`, `Reasoning` blocks are stripped from the stored context
at persist time (`maybe_strip_reasoning` in `src/agent.rs`, applied in
`persist_turn`). The graduated "Level 1" `drop_reasoning` compaction and the
rest of this design are **not yet implemented**. This document proposes the
*first* compaction strategy for prior and grounds every claim in the actual
rig fork and prior code.

Implementation status (verified against `git` history and `src/` as of the
latest commit, `Add /stat command about current usage etc`):

- **Shipped:** `reasoning_in_context` setting (Level 0 / Option D) — wired
  through `Config` → `AgentConfig`/`CodeAgent` →
  `SettingKey::ReasoningInContext` in `src/settings.rs` with `/set` candidates,
  `settings.toml.example` entry, and unit tests. Default `false`.
- **Shipped:** per-request usage capture (`SessionUsage` via `Mutex` in
  `CodeAgent`, §2.7 option A) and the `/stat` overlay (`session_stats_lines` in
  `src/agent.rs`), which report the context-window limit, current usage as a %
  of that limit, approximate session cost, and cache hit rate. This is the
  measurement primitive Level 1 will reuse.
- **Not implemented:** `src/compaction.rs` (`maybe_compact` / `drop_reasoning` /
  `estimate_tokens` / `context_window_for` / config & stats), the
  `run_interactive` hook, the `compaction_strategy` and
  `compaction_reserve_tokens` settings, debounce/circuit-breaker, the compaction
  UI note, the `with_automatic_caching_1h()` switch, and the Strategy-C rig
  change (see §5 checklist).
- **Behavioral caveat (corrected below):** the Anthropic-family override of
  Level 0 is currently **silent** — `reasoning_in_context` is passed through
  unchanged and `maybe_strip_reasoning` no-ops via `is_anthropic_family`
  **gated on `turn_has_tool_calls`** (only tool-use turns force reasoning to be
  kept), and **no warning is emitted**. The doc previously claimed a warning was
  emitted and that the setting was resolved to `true`; both are incorrect today.

Upstream rig is the pinned git fork in `Cargo.lock`
(`rig-core = { git = "…/rig.git", rev = "f31c38c…" }`), checked out at
`~/.cargo/git/checkouts/rig-794bcfddf11aa804/f31c38c/crates/rig-core`.

Related reading:

- `doc/turn_cancellation_persistence_bugs.md` — how the session `history` is
  threaded (`persist_turn`, the `&mut Vec<Message>` ownership in
  `run_interactive`).
- `doc/ultra_compaction_notes.md` — the pi "ultra-compact" algorithms. We
  **deliberately do not** copy its structured-summary / headline language
  (`#G:`, `#D:`, `#E:`…). We take only the *shape* of a cheap, deterministic,
  graduated eviction — and implement just the first, safest level.

---

## 0. TL;DR

- **Trigger**: after each turn, measure the real context size reported by the
  provider. If `context_size > context_window − reserve`, compact. The reserve
  is the token budget we keep free so the *next* turn still fits → "only compact
  as much as necessary to guarantee the next turn can complete."
- **First (and only) compaction level**: drop `AssistantContent::Reasoning`
  blocks from historical assistant messages. Keep **all** tool output, system
  prompt, user text, and full assistant text + tool calls. Deterministic, O(n),
  **no LLM, no network** → cheap.
- **Primary tuning knobs** (new settings):
  - `reasoning_in_context` — `bool` (default `false`). **Implemented.** "Level
    0" compaction: when `false`, `Reasoning` blocks are dropped from `history`
    *at store time* (they never enter the context). Reduces token usage; **has
    no effect for Anthropic-family models on turns that contain tool calls** —
    reasoning is force-kept there via a silent no-op in `maybe_strip_reasoning`
    (to honor the thinking+tool_use replay contract; §3.5.6/§3.5.7). On
    tool-free Anthropic turns and all other providers the setting is honored
    normally. This is the cache-stable "Option D" from §3.5.6.
  - `compaction_strategy` — `none | drop_reasoning` (default `drop_reasoning`).
  - `compaction_reserve_tokens` — default **100_000**. The headline parameter.

---

## 1. Research findings

### 1.1 rig message model — what we can actually mutate

The shared `history` is `Vec<rig_core::message::Message>` (`src/agent.rs:39`,
`:780`). The enum (`completion/message.rs:22`) is exactly:

```rust
pub enum Message {
    System      { content: String },
    User        { content: OneOrMany<UserContent> },
    Assistant   { id: Option<String>, content: OneOrMany<AssistantContent> },
}
```

Key facts that shape the design:

- **There is NO `tool` role.** Tool output is carried as
  `UserContent::ToolResult(ToolResult)` *inside* `User` messages
  (`completion/message.rs:60`). So "tool output" lives in `User` messages,
  interleaved with real user text. Any future level that trims tool output must
  edit `User` messages, and must **preserve user/assistant alternation**
  (deleting a `User` message can create two consecutive users, which some
  providers reject). Flagged in §3c.
- `AssistantContent` is `#[serde(untagged)]` (`completion/message.rs:68`):
  `Text(Text) | ToolCall(ToolCall) | Reasoning(Reasoning) | Image(Image)`.
  **`Reasoning` is a first-class variant**, so dropping reasoning is a single
  `.filter()` — no serialization games.
- `OneOrMany<T>` (`one_or_many.rs`) exposes public `iter()`, `iter_mut()`,
  `into_iter()`, `many(iter) -> Result<_, EmptyListError>`, `one(item)`,
  `push()`. We collect the assistant content into a `Vec`, filter out
  `Reasoning`, and rebuild with `OneOrMany::many(vec)`. If the filter leaves an
  assistant message *empty* (it had only reasoning and no text/tool call), we
  replace it with a placeholder `Text("[reasoning stripped]")` so the message
  stays well-formed and alternation holds.
- **Confirmed: reasoning survives into `final_history`.** The streaming run
  reconstructs assistant content via `ordered_streaming_assistant_content`
  (`agent/run/streamed.rs`), which maps
  `reasoning_items -> AssistantContent::Reasoning` *before* text/tool calls.
  `final_response_from_content` (`agent/prompt_request/streaming.rs:126`) stores
  that content on the `PromptResponse` whose `messages()` prior clones into
  `history` (`src/agent.rs:111`). So historical assistant messages **do**
  contain reasoning blocks, and Level 1 has real content to remove.

### 1.2 Token usage IS exposed — but the context *window* is NOT

`Usage` (`completion/request.rs:496`) is rich:

```rust
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub tool_use_prompt_tokens: u64,
    pub reasoning_tokens: u64,   // exactly what Level 1 will reclaim
}
```

It is surfaced two ways during a run:

1. `FinalResponse(PromptResponse).usage()` — **aggregate** across the whole run
   (`agent/prompt_request/mod.rs:496`).
2. `MultiTurnStreamItem::CompletionCall(CompletionCall).usage` —
   **per completion request** (`agent/prompt_request/mod.rs:324`). A multi-turn
   agent run issues *many* completion calls; each re-sends the entire history +
   system + tools. The rig doc comment is explicit: *"Use the last entry's usage
   to inspect the final completion request's prompt/context length."* So the
   true current context size = **`last_completion_call. input_tokens`** (or the
   max across calls), **not** the aggregate (which ≈ `context × num_calls` and
   would over-trigger).

`usage.reasoning_tokens` is a precise cross-check for how much Level 1
reclaimed.

**Crucially, rig exposes NO context-window / max-input-token limit.** Every
`max_tokens` in the builder/hook/runner is the *output* cap per request, not the
window. There is no `context_window()` / `max_input_tokens()` on the model. So
we cannot ask rig "how big can the context be?" — we must own that number (see
§1.3).

### 1.3 prior already knows the context window

`model_catalog.rs` already fetches `ModelEntry::context_length: Option<u64>`
from OpenRouter (and a curated `DIRECT_PRICING` table + live `/models` pruning).
Look it up by `provider:model` via `catalog()` / `catalog_cached()` +
`ModelEntry::qualified_parts()` (`model_catalog.rs:417`, `:61`).
**This is our source of truth** for the window — far better than ultra-compact's
hardcoded `detectContextWindow` table (`ultra_compaction_notes.md §3`). Caveat:
`context_length` can be `None` for unconfigured direct providers; we need a
fallback constant (e.g. 128_000) + a warning when it's unknown.

### 1.4 Gap: prior never captures usage today

`src/agent.rs:106` matches `MultiTurnStreamItem::CompletionCall(_)` and only
ends a UI segment — it **discards the `Usage`**. `:110-111` captures
`resp.messages()` but not `resp.usage()`. Net: today prior has
**zero awareness of how big the context is**. Compaction's first prerequisite is
to plumb usage out of `prompt()` (§2.7). This is the single most important "off"
finding (§3a).

---

## 2. Design

### 2.1 Hook point — "after each turn in a long-running session"

Session `history` is owned in `run_interactive`: `let mut history = Vec::new();`
(`src/agent.rs:780`), and `prompt(&input, Some(&mut history))` is called at
`:930`/`:939`. After each `prompt()` returns, `persist_turn` has already written
the updated history back (`src/agent.rs:139`). So the compaction step goes
**right after `prompt()` returns and before the next loop iteration**:

```rust
let out = agent.prompt(&input, Some(&mut history)).await?;
// NEW: cheap post-turn compaction (§2.3–§2.4)
maybe_compact(&mut history, &compaction_ctx, &mut last_usage, &mut stats);
```

**Level 0 (`reasoning_in_context = false`) runs *earlier*, at persist time.**
Before `history` is written back by `persist_turn` (`src/agent.rs:139`), filter
`Reasoning` out of the cloned `resp.messages()` (reuse `drop_reasoning`, §2.4)
when `reasoning_in_context` is `false`. Because it acts on the just-produced
turn *before* it enters `history`, no already-cached prefix ever contains
reasoning → zero cache invalidation (Option D, §3.5.6) and `maybe_compact`
(Level 1) then runs on an already-reasoning-free history. For Anthropic-family
providers `maybe_strip_reasoning` is a silent no-op **on tool-use turns** (it
retains reasoning regardless of the setting value there, to satisfy the
thinking+tool_use replay contract), so Level 0 is a no-op there; tool-free
Anthropic turns honor the setting (§2.5, §3.5.6).

Keep
`maybe_compact(&mut Vec<Message>, &CompactionCtx, &Usage, &mut CompactionStats)`
a pure, agent-free function so it is unit-testable, mirroring the `persist_turn`
regression tests in `doc/turn_cancellation_persistence_bugs.md §4`.

**Scope**: restrict to `run_interactive`. `run_once` / recipes call
`prompt(input, None)` (`src/agent.rs:159`) — there is no retained history, so
nothing to compact. Flagged in §3.

### 2.2 Measuring `context_size`

1. **Primary (preferred):** `last_completion_call.input_tokens` for the
   just-finished turn (plumbed out per §2.7). This already includes system
   prompt + full history + tools + this turn's tool results — i.e. exactly what
   the *next* turn would resend.
2. **Fallback:** when the provider reports zero-valued usage
   (`Usage::has_values() == false`), estimate with a char-based
   `estimate_tokens(&history)` (a slimmed version of ultra-compact's 4-ratio
   heuristic, `ultra_compaction_notes.md §2`). The estimator must
   **also add the system prompt + tool-definition size**, which the provider
   usage includes for free but the estimator would otherwise miss. Flagged in
   §3h.

### 2.3 The reserve and the trigger

```text
context_window = context_length(provider, model)  // model_catalog, else fallback constant
reserve        = compaction_reserve_tokens         // default 100_000
compact when:  context_size > context_window − reserve
```

`reserve` represents the *variable* cost of the next turn (next user prompt +
next tool calls + next tool results + next output `max_tokens` + a safety
margin). The fixed cost (system + current history) is already in `context_size`.
So the rule literally means:
*"don't start a turn unless there is `reserve` headroom for it to finish."* That
is the "only compact as much as necessary to guarantee the next turn can be
completed" requirement.

Because Level 1 is the only level, after dropping reasoning we re-measure and
stop as soon as `context_size ≤ context_window − reserve`. If it's *still* over,
Level 1 cannot satisfy the guarantee — see the big caveat in §3f.

### 2.4 Level 1 — drop reasoning (the only level in v1)

```rust
for msg in history.iter_mut() {
    if let Message::Assistant { content, .. } = msg {
        let kept: Vec<AssistantContent> = content.iter()
            .filter(|c| !matches!(c, AssistantContent::Reasoning(_)))
            .cloned()
            .collect();
        *content = match OneOrMany::many(kept) {
            Ok(c) => c,
            // assistant message had ONLY reasoning → keep a placeholder so the
            // message stays well-formed and user/assistant alternation holds.
            Err(_) => OneOrMany::one(AssistantContent::Text(Text::new("[reasoning stripped]"))),
        };
    }
}
```

Invariants (all cheap, deterministic, no model call):

- system prompt: untouched.
- `User` messages (incl. every `ToolResult`): untouched →
  **all tool output kept**.
- assistant text + `ToolCall` blocks: untouched →
  **full response content kept**.
- only `Reasoning` blocks removed.

### 2.5 Policy knobs

- **Setting**: `compaction_strategy`
  - **Type**: enum
  - **Values**: `none`, `drop_reasoning`
  - **Default**: `drop_reasoning`
- **Setting**: `compaction_reserve_tokens`
  - **Type**: usize
  - **Values**: ≥ some min (e.g. 8192)
  - **Default**: `100_000`
- **Setting**: `reasoning_in_context` *(implemented)*
  - **Type**: bool
  - **Values**: `true` / `false`
  - **Default**: `false`

`none` doubles as the user opt-out and as the circuit-breaker escape hatch
(§3i).

`reasoning_in_context` is **orthogonal** to `compaction_strategy`: it is the
"Level 0" compaction that runs at *persist time* (§2.1), dropping `Reasoning`
blocks from `history` *before* they ever enter the context.
`compaction_strategy` (Level 1) instead acts *post-hoc* on already-stored
history. **Level 0 has shipped** (the `reasoning_in_context` setting, wired
through `Config` / `AgentConfig` / `SettingKey::ReasoningInContext` in
`src/settings.rs` and `main.rs`); Level 1 (`drop_reasoning`) is not yet
implemented. Level 0 is the cheapest possible reclaim and is exactly the
cache-stable Option D from §3.5.6. Default `false` ⇒ Option D is on by default.
For Anthropic-family models (direct Anthropic, and OpenRouter→`anthropic/*`) the
stored setting has **no effect only on turns that contain tool calls**:
`maybe_strip_reasoning` returns the messages unchanged when
`is_anthropic_family( provider, model)` is true
**and the turn includes at least one `ToolCall`** (via `turn_has_tool_calls`),
so reasoning is force-kept there regardless of the `reasoning_in_context` value.
On Anthropic turns *without* tool calls — and on all other providers — the flag
is honored and reasoning is stripped normally. **No warning is emitted and the
setting's stored value is not flipped to `true`** — the override is a silent
no-op at persist time on tool-use turns, because dropping reasoning there would
violate the thinking+tool_use replay contract (§3.5.6/§3.5.7). On DeepSeek and
generic OpenAI-compatible providers it is safe and recommended (§3.5.7).

### 2.6 Settings + `/set` wiring

Mirror the existing `default_thinking_level` plumbing (it is the closest analog
— an enum setting with a `/set` sub-command):

1. `Config` (`src/config.rs:7`) — add `compaction_strategy: Option<String>`,
   `compaction_reserve_tokens: Option<usize>`,
   `reasoning_in_context: Option<bool>`.
2. `AgentConfig` (`src/agent.rs:228`) — add resolved `compaction_strategy` +
   `compaction_reserve_tokens`, `reasoning_in_context: bool`, threaded from
   CLI/file/defaults in `main.rs`. **As implemented**, `reasoning_in_context` is
   threaded through *unchanged*; the Anthropic-family override is handled later
   as a **silent no-op** inside `maybe_strip_reasoning` (via
   `is_anthropic_family`), so no value is flipped and no warning is emitted. The
   design note below (and §2.6 step 4) still lists "coerce + warn" as a
   desirable enhancement, but it is **not** what shipped.
3. `SettingKey` + `SETTINGS` registry + `ValueKind` (`src/settings.rs:16`,`:50`)
   — add `CompactionStrategy` (`ValueKind::Enum(&["none","drop_reasoning"])`),
   `CompactionReserve` (`ValueKind::Int`), and `ThinkingInContext`
   (`ValueKind::Bool`).
4. `SettingsStore` (`src/settings.rs:86`) — add fields + `from_config` /
   `from_agent_config` / `set_value` handling; validate on set (enum membership;
   warn if reserve > window).
   NOTE: the design's "coerce + warn `reasoning_in_context` to `true` for Anthropic-family
   providers" is **not** what shipped — the override is instead a silent no-op
   inside `maybe_strip_reasoning` (via `is_anthropic_family`, gated on
   `turn_has_tool_calls`); no value is flipped and no warning is emitted. The
   no-op applies only to Anthropic turns that contain tool calls; tool-free
   Anthropic turns honor the setting like any other provider.
5. `repl.rs` — extend `parse_settings` / `setting_value_candidates`
   (`src/repl.rs:1075`, `:1094`) so `/set compaction_strategy drop_reasoning`,
   `/set compaction_reserve_tokens 100000`, and
   `/set reasoning_in_context false` (or `true`) work.
6. `settings.toml.example` — document all three keys, including the
   `reasoning_in_context` description:
   *"Reduces token usage by omitting reasoning from the context; for Anthropic-family models this is honored on turns without tool calls, but reasoning is force-kept on any turn that involves tool calls (thinking+tool_use replay contract, no warning emitted)."*

(Optional, out of scope for v1: a manual `/compact` command for on-demand runs.)

### 2.7 Plumbing usage out of `prompt()` (without breaking callers)

`prompt()` is a trait method returning `Result<String>` (`src/agent.rs:39`). Two
options:

- **(A) Interior mutability (recommended, non-breaking):** add
  `last_usage: std::sync::Mutex<Option<Usage>>` to `CodeAgent`
  (`src/agent.rs:217`); update it each turn from the captured
  `CompletionCall`/`FinalResponse` usage, read it in `run_interactive` after
  `prompt()` returns. No trait signature change.
- **(B) Out-param:** change the trait to
  `async fn prompt(&self, input, history, stats: &mut TurnStats) -> Result<String>`.
  Cleaner but touches the trait + every caller.

Either way, capture the **last** `CompletionCall` usage (not only
`FinalResponse`'s aggregate) and prefer it.

### 2.8 Proposed module sketch (design only, not implemented)

```rust
// src/compaction.rs
pub enum CompactionStrategy { None, DropReasoning }   // graduated later: + TrimToolOutput, …
pub struct CompactionConfig { pub strategy: CompactionStrategy, pub reserve_tokens: u64, pub context_window: u64 }
pub struct CompactionStats { pub triggered: bool, pub before_tokens: u64, pub after_tokens: u64,
                             pub reasoning_tokens_reclaimed: u64, pub level: u8 }
pub fn maybe_compact(history: &mut Vec<Message>, cfg: &CompactionConfig,
                     last_input_tokens: u64, stats: &mut CompactionStats);
pub fn drop_reasoning(history: &mut Vec<Message>) -> u64;          // returns reclaimed (estimate)
pub fn estimate_tokens(messages: &[Message], system_len: usize) -> u64;  // fallback only
pub fn context_window_for(provider: &str, model: &str) -> u64;     // model_catalog, else fallback

// Level 0 ("reasoning_in_context = false"): applied at persist time (§2.1), BEFORE the turn
// enters `history`. Reuses `drop_reasoning` on the just-produced `resp.messages()` so reasoning
// never enters a cached prefix. No-op on Anthropic-family turns that include tool calls (coerced
// to keep reasoning, §2.5); tool-free Anthropic turns honor the setting.
pub fn strip_reasoning_on_store(messages: &mut Vec<Message>) { drop_reasoning(messages); }
```

`maybe_compact` returns the level it stopped at so future graduated levels slot
in and so a circuit breaker can react (§3i).

---

## 3. What's off / risks / things to flag

- **(a) Usage is never captured** (`src/agent.rs:106`,`:110`). This is the
  prerequisite gap for any measurement-based compaction. Fix first.
- **(b) rig has no context-window API.** We depend on
  `model_catalog::context_length`, which can be `None` for unconfigured direct
  providers → fallback constant + warn. The trigger is only as good as this
  number.
- **(c) No `tool` role — tool output is in `User` messages.** Future "trim tool
  output" levels must edit `User` content and must never break user/assistant
  alternation. (Not needed for Level 1, but the data model makes it easy to get
  wrong later.)
- **(d) Anthropic thinking constraint.** The Anthropic API requires `thinking`
  blocks (and their `signature`) to be preserved *unmodified* when
  **continuing** a thinking turn. Dropping reasoning from the **most-recent**
  assistant message that is the immediate prefix of a continued tool-use turn
  could violate provider rules. Mitigations: only drop reasoning from turns
  *older* than the last assistant message, or always keep reasoning on the last
  assistant message. With the default `thinking_level = off` this is moot
  initially, but it matters the moment thinking is enabled. **Validate against
  the anthropic cassettes
  (`tests/providers/anthropic/cassette/messages_thinking.rs`) before shipping
  Level 1 with thinking on.** Flagged as an assumption to validate.
- **(e) Prompt-cache interaction — cost is on the *whole history prefix*, not
  the thinned bytes.** `anthropic`/`openrouter` are cached, but
  **the breakpoint shape differs by backend**, and the difference matters a lot
  for compaction (see the deep-dive in §3.5):
  - *Anthropic-direct* uses `with_automatic_caching()` (`src/agent.rs:288`),
    which sets `automatic_caching=true, prompt_caching=false`. In rig's
    `apply_prompt_cache_control` (`providers/anthropic/completion.rs:2128-2171`)
    the manual breakpoint block is guarded by `if prompt_caching { … }`, so it
    is **skipped** — rig sends *only* a top-level `cache_control` and lets the
    API auto-place the single breakpoint at the
    **last cacheable block of the entire conversation**. The cached prefix is
    therefore the **whole request** (system + tools + all history). Any edit to
    any historical message (dropping one reasoning block from an old turn)
    changes that prefix and invalidates the **entire** cached context → it must
    be **re-created** at the cache-write rate on the single turn after
    compaction. It is *not* charged only on the removed bytes; later turns
    re-read cheaply.
  - *OpenRouter* uses `with_prompt_caching()` (`src/agent.rs:298`). rig's
    `apply_prompt_caching` (`providers/openrouter/completion.rs:1253-1299`)
    marks **only the system-prompt block**; it never marks the conversation
    history. So compaction of history touches **no** cached block →
    **no cache invalidation** for OpenRouter. (The "anthropic-family-specific"
    cost in the original note is correct, but it is specifically the
    *Anthropic-direct automatic* path that is at risk; OpenRouter is effectively
    immune.)
  - Net: compaction is cache-safe on OpenRouter, unsafe only on
    Anthropic-direct. The conflict is real but narrow. **Debounce hard**
    regardless (only compact when reclaim exceeds a meaningful threshold, e.g. >
    2_000 tokens or > 2% of the window, and prefer dropping the *largest*
    reasoning chunks first) — see §3.5 for the full reconciliation and whether a
    rig change is needed.
- **(f) Level 1 cannot satisfy the "guarantee" on its own.** This is the biggest
  caveat. Reasoning is often a *small* fraction of the context, and on a model
  with `thinking = off` (the default) there is **nothing to drop at all**. A
  long session dominated by huge tool outputs will never be reduced by Level 1,
  so the "next turn completes" guarantee is **not** achievable by Level 1 alone
  — the next turn can still overflow the real provider limit. Options: (i) treat
  Level 1 as best-effort and add a hard guard that *warns / refuses* when
  `context_size` would still exceed the window after Level 1; or (ii) scope v1
  to "drop reasoning when present" and plan Level 2+ (tool-output trimming) for
  a later phase. Recommend being explicit about this limitation in the UI.
- **(g) Aggregate vs per-call usage.** Use the *last*
  `CompletionCall.input_tokens`, never the `FinalResponse` aggregate (which
  multiplies by the number of sub-requests).
- **(h) Estimator must include the system prompt + tool definitions**, or it
  under-estimates and the trigger fires too late. When provider usage is
  available we don't have this problem (it already counts them).
- **(i) Circuit breaker / lossy fallback (recommended).** Mirror ultra-compact
  §12: wrap compaction in a clone-before-mutate + try/catch; on panic/error,
  keep the original history untouched and (worst case) set `strategy = none` so
  a bad compaction can never kill the session. Cheap insurance.

---

## 3.5 Caching deep-dive — reconciling cache hits with incremental cheap compaction

The tension is simple to state and subtle to solve: **compaction mutates
historical tokens, and Anthropic's prefix cache is keyed on the exact token
sequence up to a breakpoint.** On the Anthropic-direct path the breakpoint is
the *end of the entire conversation* (§3e), so *any* mutation invalidates the
whole cached context. Yet the whole point of "incremental cheap compaction" is
to mutate history (drop reasoning) cheaply and often. Cache efficiency wants
history to be **immutable**; compaction wants it **mutable**. This section works
out how far we can go without touching rig, and exactly what a rig change would
buy.

### 3.5.1 What the pi research implies (and where prior differs)

`doc/pi_context_caching.md` shows pi emits cache hints with very different
*granularity* per backend: a single `prompt_cache_key`+`session_id` for
OpenAI/Responses (server picks the prefix), `cache_control` breakpoints at
system / last-tool / last-message for Anthropic-direct,
`x-affinity`/`promptCacheKey` for Mistral, etc. The recurring pattern is
**one logical cache boundary at the end of a stable prefix** plus a
*session/sticky* key so the provider can reuse a previous prefix computation.

prior does **not** use pi's backend map — it rides rig. The equivalent of
pi's "stable prefix + session affinity" in rig is the Anthropic auto-breakpoint.
But unlike pi, rig gives prior
**no handle to pin that boundary anywhere but the end of the conversation**
(automatic mode) or the end of the last message (manual mode). There is no
"anchor the cache at turn N and keep everything after it volatile" primitive.
That missing primitive is the entire crux of the caching/compaction conflict.
(OpenRouter is the exception: rig only caches the system prompt there, so
history is already fully volatile and compaction is free — see §3e.)

### 3.5.2 The cost model, made precise

Anthropic billing for a cached prefix (5-min TTL): cache *write* ≈ 1.25× base
input rate for the written tokens, cache *read* ≈ 0.1× base rate for the hit
tokens. With `with_automatic_caching_1h()` the write is ≈ 2× but reads stay 0.1×
and the prefix survives an hour instead of five minutes.

On a normal (non-compacting) turn under automatic caching, the previously cached
prefix is a **prefix** of the new request, so every prior token is a cache
*read* and only the newly appended turn is written. That is why automatic
caching is so efficient for *growing* sessions. The moment we edit an old
message (compaction), the old prefix is **no longer a prefix** of the new
request, so the whole context is billed at write rate *once* (the next turn),
then read-rate thereafter. So a single compaction costs one full-context write;
the savings resume on the following turns. This is tolerable **iff** compaction
is rare relative to the number of turns between compactions.

TTL matters here in a way the original design under-weighted: if the gap between
turns exceeds the TTL (5 min default), the cache expires *regardless* of
compaction, so the "compaction invalidated my cache" penalty is often moot after
any pause. For long interactive sessions where compaction is likely to be
followed by more turns within the TTL, prefer `with_automatic_caching_1h()` so
the post-compaction re-creation pays off across more subsequent read turns.

### 3.5.3 Four reconciliation strategies (cheapest → best)

**Strategy D — Eager drop (never let reasoning enter the cached prefix).** Strip
`Reasoning` from `resp.messages()` *before* cloning into `history` each turn (a
variant of `drop_reasoning`, §2.4, applied at persist time rather than lazily).
The cached prefix then never contained reasoning, so compaction-style edits
never happen → **zero cache invalidation, ever**. Needs **no rig change**. Cost:
reasoning is irretrievably lost for the session, so any future Level-2
summarizer that wanted to read reasoning can't. Also, you give up keeping
reasoning for *recent* turns. Good as an opt-in
`reasoning_retention: keep | drop_eagerly` knob; not a substitute for true
compaction.

**Strategy A — Debounced lazy drop (ship v1 as-is, but corrected).** Keep the
§2.4 `drop_reasoning`, but (1) switch the Anthropic model to
`with_automatic_caching_1h()` so the post-compaction write is amortized over
more read turns, and (2) debounce so compaction fires only when reclaim is large
(§3e threshold). Between compactions you get pure cache reads; you pay one
full-context write per compaction event. Needs **no rig change**. This is the
recommended v1: it is cheap, deterministic, and the cache cost is bounded and
rare. The only downside is the unavoidable one-time full-context re-creation
each time we compact on Anthropic-direct — acceptable given debounce.

**Strategy C — Pinned stable prefix + volatile tail (the optimal design; needs a
rig change).** The canonical way to have *both* cache hits and editable history:
maintain a contiguous **stable prefix** `[0..=anchor]` of the conversation that
we *promise never to mutate once sent*, mark it with a `cache_control`
breakpoint at its end, and allow compaction to drop reasoning
**only in the volatile tail `[anchor+1..]`**. The stable prefix is a cache *hit*
every turn (read rate); the tail is re-sent at base input rate each turn (small,
since the anchor sits near the end). When the tail grows large,
**advance the anchor forward** by re-pinning — this re-creates the now-larger
stable prefix once, but it is infrequent and bounded (and you can debounce the
advance too). This is exactly how rolling-summary agents keep a cache-hit
"so-far" region while freely editing the recent window.

- Why this needs rig: the public rig
  `Message`/`UserContent`/`AssistantContent`/`Text` types expose
  **no `cache_control`** (`completion/message.rs` — zero `cache_control`
  fields), and in manual mode `apply_prompt_cache_control` *unconditionally*
  clears all message markers and forces the breakpoint onto the last message
  (`providers/anthropic/completion.rs:2099-2112, 2138-2171`). So prior cannot
  today create a mid-history breakpoint or prevent the auto-last-message
  breakpoint. We must either (a) change rig, or (b) bypass rig's request builder
  (heavy).
- Minimal rig change that unlocks it (pick one):
    1. **Expose `cache_control` on content/message types** and make
       `apply_prompt_cache_control` *preserve* caller-set markers instead of
       wiping them (`clear_message_cache_control` → skip blocks that already
       carry one). prior then pins a breakpoint on `history[anchor]`'s last
       block. Smallest surface-area change.
    2. **Add a dedicated anchor API**, e.g.
       `CompletionModel::with_prompt_caching_anchor()` or a
       `CompletionRequest { cached_prefix_messages: Option<usize> }` field,
       where rig places the message-breakpoint at `messages[n-1]` and skips the
       auto-last-message placement. Cleaner contract, no public `cache_control`
       leakage.
    3. **Reuse the `additional_params` top-level hook** is *not* sufficient: it
       only carries a single top-level breakpoint (the whole-context one), so it
       cannot express a middle anchor. Ruled out.
- Budget: Anthropic allows up to 4 breakpoints. Strategy C uses system + tools +
  1 anchor = 3, leaving 1 spare. Fine.
- Caveats carry over: respect §3d (don't drop reasoning from the last assistant
  message that is a continued-thinking prefix) and §3c (preserve user/assistant
  alternation) — both apply only to edits *within the volatile tail*, so the
  stable prefix is never disturbed.

**Strategy B — Abuse mid-conversation `system` messages as an anchor (fragile,
do not ship).** rig's `split_system_messages_from_history` pulls
`Message::System` variants out of the history into the `system` array, and in
manual `with_prompt_caching` mode `apply_system_cache_control` marks the *last*
system block. A `Message::System` inserted at turn N would therefore split the
cached system block from the volatile tail. But this is unsound:
mid-conversation system prompts are only supported on `claude-opus-4-8`
(`supports_mid_conversation_system_messages`, `completion.rs:1704`), they change
how the model weights the prefix, and they are discouraged by Anthropic. Listed
only to show *why* a proper rig anchor API (Strategy C) is the right path rather
than this hack.

### 3.5.4 Decision: are rig changes required?

- **For v1 caching efficiency: NO rig change required.** Strategy A (debounced
  lazy drop + 1h TTL) keeps cache reads between compactions and bounds the rare
  full-context re-creation; Strategy D (eager drop) is cache-neutral with no rig
  change. OpenRouter is unaffected entirely (system-only cache, §3e).
- **For the *optimal* "cache-hit stable prefix + freely editable tail" design:
  YES, a small, well-scoped rig change is required** (Strategy C) — either
  expose `cache_control` on message content and stop force-clearing it, or add a
  dedicated anchor API. Without it, prior cannot create a cache-hit region
  that survives history edits, so the best we can do on Anthropic-direct is "pay
  one full-context write per compaction" (Strategy A).

Recommendation: ship v1 with **Strategy A + 1h TTL** (no rig dependency, cache
cost bounded and rare), and **file an upstream rig issue / local fork patch**
for the Strategy C anchor primitive so a later phase can get zero-invalidation
incremental compaction. The `maybe_compact` returning `level`/`stats` (§2.8) and
the debounce hook (§3e) already leave room to slot Strategy C in without API
churn.

### 3.5.5 New tuning / wiring implied by the caching analysis

- `reasoning_in_context: bool` (setting, default `false`) — the "Level 0"
  compaction (Option D, §3.5.6). **Implemented** (shipped as
  `SettingKey::ReasoningInContext` in `src/settings.rs`). When `false`,
  `strip_reasoning_on_store` drops `Reasoning` from the turn before it enters
  `history` (§2.1/§2.8). Description for docs/`settings.toml.example`:
  - Reduces token usage by omitting reasoning from the context; on
    Anthropic-family models this is honored on turns without tool calls, but
    reasoning is force-kept on any turn that involves tool calls
    (thinking+tool_use replay contract, no warning emitted). For
    Anthropic-family providers the stored setting has no effect
    **only on tool-use turns**: `maybe_strip_reasoning` is a silent no-op (see
    §2.1/§2.5) on those turns, and **no warning is emitted**; safe and
    recommended for DeepSeek and generic OpenAI-compatible providers (§3.5.7).
    Distinct from `compaction_strategy`, which acts post-hoc. (Supersedes the
    earlier `reasoning_retention: keep|drop_eagerly` sketch.)
- Default the Anthropic model builder to `with_automatic_caching_1h()` instead
  of `with_automatic_caching()` (`src/agent.rs:288`) once compaction is enabled,
  so the post-compaction write is amortized over more read turns. (Confirm
  1h-TTL write cost vs. session length; for short sessions the 5-min default may
  still win.)
- `compaction_debounce_tokens` / `compaction_debounce_fraction` (new settings)
  to implement the §3e threshold; default `> 2_000 tokens` *and*
  `> 2% of window`, whichever is larger.
- A cache-impact line in the compaction UI note (§5 last bullet): report not
  just tokens reclaimed but the predicted one-time re-creation cost on
  Anthropic-direct.

### 3.5.6 Option D re-examined — pros/cons, and does pi use it?

**Pros of omitting reasoning from the context (eagerly, at store time):**

- **Zero cache invalidation, ever, with no rig change.** Because reasoning never
  enters *any* cached prefix, there are no later edits to that prefix → the §3.5
  cache/compaction conflict disappears entirely. This is strictly better for
  cache stability than Strategy A: A still pays one full-context write per
  compaction, D pays none.
- **Cheapest possible operation.** No LLM, no network, no token counting at
  compaction time; reasoning is simply never cloned into `history` (a variant of
  `drop_reasoning`, §2.4, applied at persist time). O(1) per turn.
- Deterministic and trivial to unit-test (it is just a filter at one call site).

**Cons (the reasons it is not a free win):**

- **Anthropic thinking + tool-use replay constraint (the big one).** When
  thinking is enabled and the model emits tool calls *inside* a thinking turn,
  Anthropic requires the `thinking` block to precede those `tool_use` blocks on
  replay, and forbids a thinking assistant message followed by a non-thinking
  one. Blindly dropping reasoning from every assistant message (option D)
  therefore risks API rejection on the next turn. So option D **cannot** drop
  reasoning from an assistant message that contains tool calls produced during
  thinking — it must at minimum preserve reasoning on those messages (and
  respect the contiguity rule). This is the §3d concern, but sharper: §3d only
  governs *compaction of old turns*; option D governs *storage of every turn*,
  so the constraint is in force the moment thinking is on. The robust answer
  most harnesses take is "just keep thinking" — which is exactly what undercuts
  option D.
- **No-op under the default config.** With `thinking_level = off` (the prior
  default, §3d) there is no reasoning to omit, so option D does nothing. Its
  benefit materializes *only* when thinking is enabled — i.e. exactly the regime
  where the replay constraint bites. It is "free" only when it is a no-op, and
  "costly" exactly when it would help.
- **Permanent loss of signal.** Reasoning is irretrievable once omitted. If a
  future Level-2 summarizer (or any retrospective step) wanted the reasoning, it
  is gone. `ultra_compaction_notes.md` asserts
  *"reasoning is not needed after the turn"* (its `stripReasoningBlocks`,
  §below) — that is the optimistic assumption underpinning option D; fine for
  token-reclaim, but it discards potentially useful signal.
- **Doesn't solve the guarantee alone.** Reasoning is frequently a *small*
  fraction of the context (see §3f); option D alone rarely makes "the next turn
  completes" true any more than Level 1 does. It must pair with tool-output
  trimming for the hard guarantee — same limitation as Level 1.

**Does pi employ option D? No.** Verified against the pi checkout
(`pi/packages/ai`, `pi/packages/coding-agent`):

- pi **keeps** `thinking` blocks in context for same-model replay.
  `transform-messages.ts:101-116` preserves the thinking block (with signature)
  when `isSameModel`, converting empty thinking to text only for *cross*-model;
  it drops *redacted* thinking only when switching models.
- pi **writes thinking back to Anthropic with its signature** on replay
  (`anthropic-messages.ts:1180-1185`), satisfying the API's thinking+tool_use
  contract.
- pi's context compaction is **LLM summarization**, not reasoning-omission:
  `compaction.ts` calls `completeSimple` with `SUMMARIZATION_SYSTEM_PROMPT`, and
  `utils.ts` `serializeConversation` *folds thinking text into the summary*
  (`[Assistant thinking]: …`, `utils.ts:124-142`). So pi actually **preserves**
  reasoning content (as text) in the compaction summary rather than discarding
  it.
- pi even **counts thinking toward context usage** (`compaction.ts:255-256`),
  confirming it carries reasoning in the live context rather than omitting it.

Net: pi is a concrete counter-example — it accepts the token cost of reasoning
in context and relies on summarization compaction instead of eager omission.
Option D's cache-stability win is real, but the Anthropic replay contract (and
the thinking-off default) make it a niche, constraint-bound choice rather than a
drop-in default.

**Prior art:**

- **In-repo:** `doc/ultra_compaction_notes.md` (`stripReasoningBlocks`, ~lines
  245-247) drops `type === "thinking"` blocks as the
  *cheapest first eviction level* during `evictGradually`, asserting
  *"reasoning is not needed after the turn."* That is the conceptual twin of
  option D, but applied **post-hoc during compaction** rather than eagerly at
  store time. So option D = "move `stripReasoningBlocks` to store-time."
  (prior's v1 Level 1, §2.4, is the same idea as the post-hoc variant.)
- **Anthropic's own API replay rule** (thinking block must precede tool_use on
  replay; no thinking→non-thinking assistant transition) is the constraint that
  makes eager-omission subtle, and is the reason production harnesses that use
  tool-use-during-thinking (pi, Claude-Code-style loops) retain reasoning for
  same-model turns rather than omit it.
- **Industry pattern:** treating reasoning as
  *ephemeral / stream-only, not persisted* is a known cheap optimization, but in
  practice most agents that enable thinking keep the reasoning for same-model
  replay because of the replay contract; the common safe compromise is "keep
  reasoning on the trailing assistant message that issued tool calls," not "drop
  it everywhere."

### 3.5.7 Option D across providers — OpenRouter & OpenAI-compatible (DeepSeek)

The safety of Option D is governed by the
**model family's reasoning-replay rule, not by the gateway or wire protocol**.
OpenRouter is a proxy, so its constraint is that of the *upstream* model it
routes to.

- **DeepSeek / generic OpenAI-compatible (Chat Completions): Option D works and
  is safe.** These models do **not** require reasoning to be replayed in
  multi-turn context; the convention (and DeepSeek's own guidance) is to carry
  `content` only and treat `reasoning_content` as response-only. pi confirms
  this: for DeepSeek it sets `reasoning_content = ""` when none is present
  (`openai-completions.ts:1007-1011`,
  `requiresReasoningContentOnAssistantMessages`), proving empty/missing
  reasoning is accepted. Omitting reasoning entirely is therefore valid — and is
  in fact the recommended practice to keep history lean. (Caveat: a minority of
  OpenAI-compatible reasoning models key budget continuity off a persisted
  `reasoning`/`reasoning_details`; DeepSeek and mainstream ones do not.)
- **OpenRouter — upstream-dependent:**
  - `openrouter/anthropic/*`: the
    **Anthropic thinking+tool_use replay constraint applies** (§3.5.6), so
    Option D is unsafe unless reasoning is preserved on any assistant message
    that issued tool calls during thinking. Same §3d caveat.
  - `openrouter/deepseek-*`, `openrouter/openai/*`, and other OpenAI-compatible
    upstreams: no replay requirement → Option D works, identical to direct
    DeepSeek.
  - OpenRouter itself adds no replay requirement; it forwards
    `cache_control`/`reasoning` as the upstream expects.
- **Cache impact on these providers (mostly moot for the cache motivation):**
  - *DeepSeek* does server-side prefix caching (~5 min, reported via
    `cached_tokens`). Option D keeps the prefix reasoning-free and therefore
    stable → cache hits, no compaction-driven re-creation. So Option D still
    yields the cache-stability win here.
  - *OpenRouter* (prior's `with_prompt_caching` path) caches
    **only the system prompt** (§3e/§3.5.1); conversation history is never
    cached, so history edits (compaction) never invalidate OpenRouter's cache
    regardless. Hence for OpenRouter the cache motivation for Option D is
    essentially irrelevant — the only reasons to use Option D there are token
    savings and (for anthropic/* upstreams) avoiding the replay-constraint
    violation that *editing* a stored thinking block would otherwise cause.

**Net:** Option D is safe and recommended for DeepSeek and generic
OpenAI-compatible providers, and for OpenRouter whenever it is *not* routing to
an Anthropic-model upstream. It is only the Anthropic-family replay contract
(direct Anthropic, and OpenRouter→`anthropic/*`) that forces Option D to
preserve reasoning on tool-calling assistant messages. The cache-stability
benefit of Option D is real for DeepSeek's prefix cache but largely moot for
OpenRouter (system-only cache).

---

## 4. Clarifying questions / assumptions to challenge

I'm questioning the following rather than assuming them — please steer:

1. **Is a 100k *absolute* reserve the right default?** For a 200k window it
   leaves only 100k for history (aggressive); for a 128k window, even more so.
   Should `reserve` instead be a **fraction of the window** (e.g. 50%) or be
   derived from the output `max_tokens` budget? The assumption that one constant
   fits all models is worth challenging.
2. **"Guarantee the next turn completes" — hard or soft?** Providers hard-error
   at the real limit regardless of our reserve. Is the reserve purely a safety
   margin (soft, best-effort compaction), or should prior *refuse/error* when
   even Level 1 can't make room (hard)? Tied directly to caveat §3f.
3. **Most-recent-turn reasoning:** keep it (Anthropic safety, §3d) or drop it
   too (max reclamation)? Default proposal: drop everywhere for v1, validate
   with cassettes.
4. **Providers that report zero usage** (some OpenAI-compatible/ollama setups):
   in estimator-only mode the trigger is approximate. Is best-effort acceptable,
   or should we refuse to auto-compact when `context_length`/usage are both
   unknown?
5. **Graduated architecture now or later?** Recommend returning the level-used +
   stats from `maybe_compact` even though v1 has one level, so Levels 2–4
   (tool-output trim, then full removal) slot in without API churn.
6. **Scope confirmation:** compaction = REPL `run_interactive` only (§2.1)?
   Non-interactive / recipe paths retain no history, so there's nothing to
   compact there.
7. **No headline/summary duplication:** I'm explicitly *not* porting
   ultra-compact's `#G:`/`#D:`/`#E:` structured-summary language, per your
   instruction. Confirm Level 1 (reasoning drop) is the intended v1 ceiling and
   that summarization stays out of scope.
8. **Rig-change appetite (new, from §3.5):** v1 can ship cache-efficient
   compaction with **no** rig change (Strategy A: debounced lazy drop + 1h TTL;
   or Strategy D: eager drop). The *optimal* "stable-prefix + volatile-tail"
   design (Strategy C, §3.5.3) requires a small rig change (expose
   `cache_control` on message content, or add an anchor API). Do we (a) ship v1
   on Strategy A/D and file the rig change upstream for a later phase, or (b)
   take the rig fork patch now to get Strategy C from day one?
9. **Default TTL:** switch the Anthropic builder to
   `with_automatic_caching_1h()` (§3.5.4) when compaction is on, or keep the
   5-min default? Trade-off: 1h pays 2× write but survives idle gaps so the
   post-compaction re-creation amortizes; 5-min is cheaper per write but expires
   during pauses.

---

## 5. Implementation checklist (when approved)

- [x] Capture last `CompletionCall` + `FinalResponse` usage → `CodeAgent` (§2.7,
      option A). **Done** — captured into the `SessionUsage` telemetry
      (`src/agent.rs`), which backs `/stat` and is the measurement primitive
      `maybe_compact` will reuse.
- [ ] `src/compaction.rs`: `maybe_compact`, `drop_reasoning`, `estimate_tokens`,
      `context_window_for`, `CompactionConfig`/`Stats`/enum. Unit tests for
      each.
- [ ] Hook `maybe_compact` into `run_interactive` after `prompt()` (§2.1).
- [x] Settings: `reasoning_in_context` (Config + AgentConfig +
  `SettingKey::ReasoningInContext`
      + `SettingsStore` + `settings.toml.example`, §2.6). **Done** — this is
      Option D / Level 0. Remaining: `compaction_strategy` +
      `compaction_reserve_tokens` (§2.6).
- [ ] Debounce threshold + circuit breaker (§3e, §3i).
- [ ] Anthropic thinking validation via cassettes (§3d).
- [ ] UI note when compaction fires (tokens reclaimed, level, cache impact).
- [ ] **Caching wiring (new, §3.5):** default Anthropic builder to
      `with_automatic_caching_1h()` when compaction is enabled (§3.5.4) —
      *not yet done*; `reasoning_in_context` (Level 0 / Option D, §2.5, §3.5.5)
      **done**. Add `compaction_debounce_tokens`/ `compaction_debounce_fraction`
      (§3.5.5) settings — *not yet done*.
- [ ] **Rig change (only if Strategy C chosen, §3.5.3):** upstream issue / fork
      patch to expose a mid-history cache anchor (expose `cache_control` on
      message content *and* stop force-clearing it, or add a dedicated anchor
      API); then pin the stable prefix and limit compaction to the volatile
      tail.

```text
```
