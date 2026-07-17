# Streaming, Thinking, and Richer Response Formatting

Status: **Phase 1 implemented** — `codewandler-markdown-terminal` now renders the
model response on the non-streaming path (see Phase 1). Phases 2–7 are still
planned. Findings below are grounded in the current source
(`src/agent.rs`, `src/output.rs`, `src/thinking.rs`) and the vendored
`rig-core 0.38.2` + `codewandler-markdown-terminal 0.2.1` crates already present
in the local cargo registry cache.

Related docs:
- `doc/thinking_audit.md` — already documents the *surfacing thinking content*
  gap thoroughly; this plan builds on it.
- `doc/picocode_rendering.md` — documents the current `Output` trait and
  `termimad` usage.

---

## 1. Goal

Three improvements, implemented together because they all touch the
agent→`Output` response path:

1. **Streaming responses** — print assistant text token-by-token as it arrives
   instead of buffering the whole turn and printing once at the end.
2. **Surface thinking / reasoning** — show the model's reasoning tokens (not
   just request that they be produced). Today `/thinking` only *asks* providers
   for reasoning; the returned content is discarded.
3. **Richer formatting** — replace the inline-only `termimad::print_inline`
   with `codewandler-markdown-terminal`, which renders headings, fenced code
   blocks with syntax highlighting, tables, lists, and blockquotes, and
   supports live/incremental rendering (ideal for streaming).

---

## 2. Current State (findings, with evidence)

### 2.1 No streaming — the response is fully buffered

`CodeAgent::prompt` (`src/agent.rs`, `impl PicoAgent`, the `async fn prompt`)
builds a non-streaming request and `.await`s the whole thing:

```rust
let response = builder      // self.agent.prompt(input).with_hook(...).max_turns(...)
    .await
    .map_err(...)?;
self.output.stop_thinking();
Ok(response)                // full String, only returned AFTER the turn completes
```

`builder` here is a rig `PromptRequest`; `.await` (its `IntoFuture`) yields a
`Result<String, PromptError>` (`rig-core/src/agent/prompt_request/mod.rs`,
`impl IntoFuture for PromptRequest<Standard,…>` → `send().map(|r| r.output)`).
The agent never sees bytes until the entire multi-turn agentic loop is done.
The only "live" feedback is the `indicatif` spinner from
`display_thinking("Thinking...")` / `stop_thinking()`, which is **not** real
content.

### 2.2 Thinking is requested but never surfaced

`src/thinking.rs` maps a level (`off|low|med|high`) to provider
`additional_params` (`anthropic` → `thinking` block; `openai` →
`reasoning_effort`; `openrouter` → `reasoning.effort`). `agent.rs` injects that
into the rig `Agent` and `/thinking` rebuilds the agent. But:

- `output.display_thinking(message)` is used **only as a spinner label**
  (`src/output.rs:101,138,184` and the `ConsoleOutput` impl). It is never fed
  real reasoning text.
- The response is consumed as `response.to_string()`; provider reasoning fields
  are never read. `doc/thinking_audit.md` covers this in full and recommends
  capturing the raw response and extracting provider thinking blocks.

### 2.3 Formatting is inline-markdown only

`ConsoleOutput::display_text` (`src/output.rs`, the `display_text` method):

```rust
println!();
print!("{} ", style("⏺").cyan());
termimad::print_inline(text);   // inline markdown, default width, no custom skin
println!();
```

`termimad::print_inline` renders inline markdown (bold/emphasis/code spans/links)
but **does not** render fenced code blocks, tables, headings, or lists as
block elements, and does no syntax highlighting. Code fences therefore print as
raw text. Block formatting elsewhere is hand-rolled (ASCII avatar header, `─`
separators, `│`/`└` tree prefix for tool results).

### 2.4 Bonus finding: conversation history is not persisted across turns

`CodeAgent::prompt` receives `history: Option<&mut Vec<Message>>` and calls
`builder = builder.with_history(&*h)`. In rig 0.38, `with_history`
(`src/agent/prompt_request/mod.rs`) **clones** the history into the request
(`self.chat_history = Some(history.into_iter().map(Into::into).collect())`); it
does **not** mutate the caller's `Vec`. `build_rig_agent` also sets **no**
`ConversationMemory` (grep for `.memory(` in `src/agent.rs` returns nothing).
Net effect: across separate user turns the previous assistant answer is
discarded — each turn sees only the system preamble + the single new prompt.

The streaming `FinalResponse` (see §3.1) returns the full updated history via
`FinalResponse::history()`, so fixing §3.1 also *fixes this latent bug*.

### 2.5 The building blocks already exist in the toolchain

- **rig-core 0.38.2** ships first-class streaming: `Agent::stream_prompt`
  (`src/agent/completion.rs`) returns a `StreamingPromptRequest`; awaiting it
  yields `StreamingResult<R>` — a `Stream` of `MultiTurnStreamItem<R>` that
  carries both text **and** reasoning deltas.
- **`codewandler-markdown-terminal 0.2.1`** is already in the registry cache
  (alongside `codewandler-markdown-stream 0.2.1`). It provides `LiveRenderer`
  (in-place redraw on a TTY) and an incremental `Renderer` + `markdown_stream`
  parser, with a built-in dependency-free syntax highlighter.

---

## 3. Target Design

### 3.1 Streaming via rig `stream_prompt`

Replace the non-streaming `.await` with the streaming variant and iterate the
stream. From the rig source:

```rust
use rig_core::streaming::{StreamingPrompt, StreamedAssistantContent, StreamedUserContent};
use futures::StreamExt;

// build the SAME request shape as today, but with stream_prompt:
let mut stream = self
    .agent
    .stream_prompt(input)                       // -> StreamingPromptRequest<M, P>
    .with_hook(LoggingHook { ... })             // unchanged
    .max_turns(self.tool_call_limit)
    .with_history(&*h)                          // still clones; we re-read history below
    .await?;                                    // -> StreamingResult<R>

let mut final_res = None;
let mut full_text = String::new();
while let Some(item) = stream.next().await {
    match item {
        Ok(MultiTurnStreamItem::StreamAssistantItem(
            StreamedAssistantContent::Text(Text { text, .. }))) => {
            full_text.push_str(&text);
            self.output.stream_text(&text);          // live token-by-token
        }
        Ok(MultiTurnStreamItem::StreamAssistantItem(
            StreamedAssistantContent::Reasoning(reasoning))) => {
            self.output.stream_thinking(&reasoning.display_text());   // thinking panel
        })
        Ok(MultiTurnStreamItem::CompletionCall(call)) => {
            // provider call finished (with usage). Finalize any live text block
            // before printing static tool-call lines (see §4 layout note).
            self.output.end_response_segment();
        }
        Ok(MultiTurnStreamItem::StreamUserItem(_)) => { /* tool results handled by hook */ }
        Ok(MultiTurnStreamItem::FinalResponse(res)) => { final_res = Some(res); }
        Err(e) => { /* surface error via Output */ }
    }
}
self.output.end_response();                      // commit final live block
// Persist history (FIXES §2.4): rig returns the full exchange when with_history was used.
if let (Some(h), Some(res)) = (history, &final_res) {
    if let Some(new_history) = res.history() {
        *h = new_history.to_vec();
    }
}
let response = final_res.map(|r| r.response().to_string()).unwrap_or(full_text);
```

Key rig types (all in `rig-core`):
- `MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::{Text,Reasoning})`
  — text and reasoning deltas.
- `MultiTurnStreamItem::CompletionCall(CompletionCall)` — one provider call
  finished; `completion_calls()` on the final response gives per-call usage.
- `MultiTurnStreamItem::FinalResponse(FinalResponse)` — `response()` (concat
  text), `content()` (structured), `usage()`, `completion_calls()`,
  `history()` (full `Vec<Message>` when `with_history` was used).
- `StreamedAssistantContent::Reasoning::display_text()` returns the reasoning
  string to print.

Tool calls still flow through `LoggingHook` (`on_tool_call` / `on_tool_result`)
exactly as today — the hook runs *during* stream iteration, so tool calls print
inline as the stream progresses. No change needed there. The `user_cancel`
`Esc` path also still works: `LoggingHook::on_completion_call` returns
`HookAction::terminate` at the next provider-call checkpoint, which stops the
loop. (rig 0.38 removed mid-request cancel; this checkpoint behavior is
unchanged from today.)

`rig_core::agent::prompt_request::streaming::stream_to_stdout` exists but is not
reusable here: it prints raw text+reasoning to stdout with no hooks, no
`Output` abstraction, no thinking gating, and no markdown rendering. We write
our own loop.

### 3.2 Surface thinking content (gated by level)

`stream_thinking(delta)` receives each reasoning delta. Gating: only render it
when the user opted into thinking. Reuse the existing decision — the agent was
built with `thinking::mapping_opt(provider, level)`; if that returned `Some`,
the provider was asked for reasoning. Carry a `bool thinking_enabled` (or the
`ThinkingLevel`) into `CodeAgent`/`Output` so `stream_thinking` is a no-op when
`Off`/unsupported. This directly implements the recommendation in
`doc/thinking_audit.md`.

### 3.3 Richer formatting via `codewandler-markdown-terminal`

Replace `termimad::print_inline` with `markdown_terminal`. Two modes:

- **TTY (live):** `LiveRenderer::new(theme, width, live = true)`.
  `push(delta, &mut w)` re-renders the accumulated markdown in place via
  cursor-up escapes (`\x1b[{n}A\r\x1b[J`). `finish(&mut w)` commits it.
- **Non-TTY / quiet:** `LiveRenderer::new(theme, width, live = false)`
  accumulates and renders once at `finish` (clean for `| cat`, logs, pipes).

The crate renders headings, fenced code with syntax highlighting, tables, lists,
and blockquotes, width-aware. Theme via `Theme::default()` / `Theme::no_color()`.
(Its highlighter is dependency-free — no `syntect` to add.)

For *fully* incremental (no O(n²) re-parse per `push`), combine the incremental
`markdown_stream::StreamParser::write(chunk)` + `markdown_terminal::Renderer::feed`
instead of `LiveRenderer::push`. Both are available; `LiveRenderer` is simpler
and fine for typical agent responses. See §5 risk on re-parse cost.

### 3.4 Adopt the formatter early, on the non-streaming path

Recommendation: land `codewandler-markdown-terminal` **first** as a drop-in for
the existing non-streaming `display_text`, *before* building streaming. Rationale:

- `ConsoleOutput::display_text` is the **current production** render path — the
  REPL `Submit` handler and `run_once` both call `display_text(&response)` after
  `prompt`. Swapping it validates the new formatter on **100% of today's real
  usage** with zero streaming complexity attached.
- It isolates the riskiest *formatting* questions — how the crate renders the
  markdown an LLM actually emits (code fences, GFM tables, nested lists, CJK
  width, color/theme in pipe mode), and whether the young crate (v0.2.1) is
  stable — from the riskiest *streaming* questions (live cursor-up redraw, the
  in-flight reader thread, tool-call interleaving). Debug one at a time.
- It forces the **cursor-row tracking fix** (`note_lines` must count the
  rendered output's lines, not the raw text's wrapped estimate) in the simple
  render-once path, so Phase 4's live redraw starts from a correct baseline.
- It establishes the `Theme` / GFM / `no_color`-for-pipes conventions the live
  renderer will reuse, shrinking Phase 4 to "wire the `LiveRenderer`" only.

Caveat to design around: `markdown-terminal` parses input as a **block** markdown
**document**, whereas `termimad::print_inline` is **inline**-only. Text that is
not a model response (error/status strings like `"Cannot switch model: …"`)
could be reinterpreted if it contains `#`, `|`, or indented lines. Mitigation: add
 a distinct `Output::display_markdown` for the model response and keep
`display_text` as the plain/inline printer for system/error text (see Phase 1).

---

## 4. Implementation Plan (phased)

### Phase 0 — Dependencies (`Cargo.toml`)
Add (currently only in registry cache, not in `Cargo.toml`):
```toml
codewandler-markdown-terminal = "0.2.1"
codewandler-markdown-stream = "0.2.1"   # provides parse_gfm for the render-once path
```
`futures` is already a transitive dep of `rig-core` but add it explicitly for
`StreamExt`. Keep `termimad` for now (used by header/tool-result rendering) or
migrate those too in a later pass.

### Phase 1 — Adopt `markdown-terminal` for the non-streaming path FIRST (de-risk)
**Status: implemented** (2025-07-17). `Output::display_markdown` added (default
delegates to `display_text`); `ConsoleOutput` renders via
`markdown_terminal::render_with(&markdown_stream::parse_gfm(..), theme, width)`
with `Theme::default()` on a TTY and `Theme::no_color()` otherwise; the REPL
`Submit` handler and `run_once` now call `display_markdown` for the model
response. Unit tests in `src/output.rs` assert GFM table/code/heading/blockquote
rendering and that the color path enables syntax highlighting (ANSI present)
while the plain path stays ANSI-free. Full suite green, no build warnings.

> Prerequisite discovered while implementing (**now resolved**): the crate did **not
> compile** because `src/lib.rs` declares `pub mod table;` (commit `a06a0e1`) but
> `src/table.rs` was missing. A temporary scaffold unblocked the build; the real
> `src/table.rs` is now committed and the full suite is green.

This is the **current production** render path: every interactive session today
renders the model response through `ConsoleOutput::display_text` (the REPL `Submit`
handler and `run_once` both call `display_text(&response)` after `prompt`). Swapping
it now validates the new formatter on 100% of real usage **before** any streaming
machinery exists — exactly the early formatting-issue detection requested (§3.4).
- Add `Output::display_markdown(&str)` (default delegates to `display_text`) for
  the **model response**; keep `display_text` as the **plain/inline** printer for
  error/status strings (`"Error: …"`, `"Cannot switch model: …"`, etc.). This split
  prevents non-response text containing `#` / `|` / indented lines from being
  misinterpreted as markdown (inline `termimad` vs block `markdown-terminal`).
- Implement `ConsoleOutput::display_markdown` with a **render-once** call:
  `render_with(&markdown_stream::parse_gfm(src), &theme, term_width())` (GFM so
  tables / task-lists / strikethrough render as users expect). Use
  `Theme::no_color()` when `!stdout().is_terminal()` (pipes/logs).
- **Cursor-row tracking fix (real integration point):** `display_text` currently
  does `note_lines(wrap(text, term_width()).len() + 1)` — an estimate from the raw
  text. With block rendering the rendered line count differs (code fences, tables,
  headings add lines). Update `note_lines` to count the **rendered** string's `\n`
  count so the REPL input line still positions correctly. Solving this in the
  simple path de-risks Phase 4's live redraw.
- **Validate early** (focused test/snapshot suite, no async needed): extend
  `tests/render_scroll.rs` / add `tests/markdown_render.rs` with snapshots for
  fenced code (+ language, + no language), GFM tables, nested ordered/unordered
  lists, blockquotes, heading levels, inline bold/italic/code/link, and CJK-width
  wrapping; assert `no_color`/pipe output is ANSI-free; assert the rendered line
  count matches `note_lines`. Also probes crate maturity (v0.2.1) — panics or
  instability surface here, not buried under streaming.
- `QuietOutput`/`NoOutput`/`LogOutput`/`ScrollbackOutput` unchanged (defaults).

### Phase 2 — Streaming plumbing (`src/agent.rs`)
- Change `prompt` to use `stream_prompt` + the loop in §3.1.
- Drive the stream and call the new `Output` streaming methods (below) instead of
  `display_text(&response)`; keep `prompt` returning `Result<String>` (final text)
  for callers/tests — rendering becomes a side effect.
- In the REPL `Submit` handler and `run_once`, **remove** the
  `display_text(&response)` call that follows `prompt` (today at
  `agent.rs` `config.output.display_text(&r)` and the `run_once` equivalent) —
  `prompt` now renders itself via streaming methods. Non-streaming callers (if
  any survive) use `display_markdown` from Phase 1.
- Persist history from `FinalResponse::history()` (§2.4 fix).

### Phase 3 — Extend the `Output` trait (`src/output.rs`)
Add streaming methods **with default no-op bodies** so existing impls keep
compiling without change:
```rust
trait Output: Send + Sync {
    // ... existing ...
    /// Called once when an agent response starts streaming.
    fn begin_response(&self) {}
    /// A delta of assistant text. Live renderers print incrementally.
    fn stream_text(&self, _delta: &str) {}
    /// A delta of model reasoning/thinking (already gated by level upstream).
    fn stream_thinking(&self, _delta: &str) {}
    /// Commit the current text/Thinking segment before a static line
    /// (tool call, tool result, completion call) is printed.
    fn end_response_segment(&self) {}
    /// Called when the whole response finishes; commit final live block.
    fn end_response(&self) {}
}
```
`QuietOutput`, `NoOutput`, `LogOutput`, `ScrollbackOutput` need **no change**
(defaults apply). `LogOutput` may optionally override `stream_text` to
`tracing::info!` the deltas; `QuietOutput` stays silent (no-op) so quiet mode
remains minimal.

### Phase 4 — `ConsoleOutput` live rendering
Reuses the `markdown-terminal` + `Theme` + GFM conventions established in Phase 1;
only the *live* `LiveRenderer` path is new:
- Hold a `markdown_terminal::LiveRenderer` (and a second one for the thinking
  panel) in `ConsoleOutput`, created in `begin_response` with
  `term_width()` and `std::io::stdout().is_terminal()` for `live`.
- `stream_text(delta)` → `live.push(delta, &mut stdout)`.
- `stream_thinking(delta)` → push to the thinking `LiveRenderer` (dim/prefixed,
  e.g. `⏺ Thinking:` header), gated off when thinking level is `Off`/unsupported.
- `end_response_segment()` → `live.finish(&mut stdout)` (and finish the thinking
  panel) so the next static tool-call/result line prints *below* the committed
  text instead of being overwritten by a cursor-up redraw. This resolves the
  layout conflict between in-place redraw and interleaved tool output.
- `end_response()` → final `live.finish` and update cursor-row tracking
  (see §5).

### Phase 5 — Thinking panel UX
- A collapsible, dim "thinking" region above the final answer. Toggle visibility
  with the existing `/thinking` level (and/or a `/showthinking` flag).
- For non-thinking levels, `stream_thinking` is a no-op (no panel).

### Phase 6 — Quiet / non-interactive (`run_once`, pipes)
- `run_once` and piped stdin use `ConsoleOutput` with `live = false`, so the
  whole markdown renders once at `end_response` (clean for `| cat`/CI). The
  render-once call is the same code path validated in Phase 1.
- `QuietOutput` keeps the spinner and stays silent for streamed content
  (`--quiet` = minimal).

### Phase 7 — Token usage footer (optional, cheap win)
`FinalResponse::completion_calls()` and `usage()` give per-call and aggregate
token counts. Print a dim footer (e.g. `tokens: 1234 in / 567 out · 3 calls`)
after `end_response` when verbose. Reuses the existing `note_lines`/width logic.

---

## 5. Risks & Open Questions

- **Cursor-row tracking + live redraw.** `ConsoleOutput` tracks the cursor row
  for REPL positioning (`note_lines`, `query_real_cursor_row` DSR). `LiveRenderer`
  redraws via `\x1b[{n}A\r\x1b[J`, leaving the cursor at the bottom of the
  committed block. After `end_response`, call `note_lines(rendered_line_count)`
  using the rendered string's `\n` count so the REPL input line positions
  correctly. Verify with `tests/render_scroll.rs`-style assertions.
- **Layout conflict (tool calls vs in-place rewrite).** Addressed by
  `end_response_segment()` (finish the live block before any static
  tool-call/result line). Confirm the visual order: reasoning → final text →
  tool calls → results → (final answer segment) renders sensibly; adjust the
  "finish before static line" rule if needed.
- **Re-parse cost.** `LiveRenderer::push` re-parses the whole accumulated source
  each delta (O(n²) in response length). Fine for typical agent answers; if it
  shows lag on long outputs, switch to the incremental
  `markdown_stream::StreamParser` + `markdown_terminal::Renderer::feed` path
  (documents are split-equivalence-safe across `write` calls, so it's robust to
  arbitrary chunk boundaries).
- **Markdown in error/status strings.** `display_text` is also used for plain
  strings (`"Cannot switch model: {}"`, `"Error: {}"`). Rendering those through
  `markdown_terminal` is safe (plain text → plain text), but audit callers to
  avoid surprising backtick interpretation; keep `display_text` for these and
  use the new streaming methods only for the model response.
- **Reasoning gating correctness.** Only render `stream_thinking` when the
  provider was actually asked for reasoning (`thinking::mapping_opt(...) ==
  Some`). Some providers ignore the flag; rendering empty/garbage is avoided by
  the gate. Verify per-provider (anthropic `thinking`, openai
  `reasoning_effort`, openrouter `reasoning`) that `StreamedAssistantContent::Reasoning`
  is actually emitted — add a mocked streaming test per provider shape.
- **History persistence side effect (§2.4).** Switching to
  `FinalResponse::history()` makes the agent *stateful across turns* for the
  first time. This is likely desired, but confirm it doesn't blow up context
  windows or surprise users; consider capping/stripping tool-result payloads
  from persisted history (the existing `display_tool_result` already truncates
  *display* to 4 lines — mirror that for history too, or store full and truncate
  on render).

---

## 6. Testing Strategy

Per `AGENTS.md`: cover tricky code with tests; prefer integration/e2e, unit to
isolate.

- **Unit — streaming parse/format:** extend `src/output.rs` tests (alongside the
  existing `wrap_*` suite) with `markdown_terminal` rendering tests: code fence
  + highlight, table alignment, nested list marker-once (the crate already tests
  this internally; add a couple of picocode-specific snapshots).
- **Unit — streaming loop:** rig provides `test_utils::MockCompletionModel` and
  `MockStreamEvent` (incl. reasoning events) in `rig-core`. Build a mock
  streaming agent and assert `Output` receives `stream_text` deltas equal to the
  concatenated `FinalResponse::response()`, and `stream_thinking` deltas when
  reasoning events are emitted. Capture via a test `Output` impl that records
  calls.
- **Integration — thinking gate:** a mocked provider that emits reasoning only
  when `additional_params` requests it; assert `stream_thinking` is a no-op when
  level is `Off`.
- **Integration — history persistence:** after two mocked turns, assert the
  captured `history` from `FinalResponse` grows (regression test for §2.4).
- **Scroll/redraw:** extend/copy `tests/render_scroll.rs` to assert cursor-row
  math after a `LiveRenderer` commit (using a `vt100` pty, as the existing test
  does).

---

## 7. Rollout / Config

- Streaming on by default for TTY; `live = false` for pipes/quiet.
- Keep `/thinking` (already present) as the reasoning gate; optionally add
  `/showthinking` to toggle the thinking panel independently of the *request*
  level.
- No breaking change to the `Output` trait for third parties (new methods have
  defaults). Non-`ConsoleOutput` behavior is unchanged except `LogOutput`
  optionally logging deltas.

---

## 8. Queued Steering Messages (longer term)

Supporting steering messages depends on the streaming work above: once responses
stream token-by-token (Phase 1–3) there is a visible, multi-second "mid-turn"
window in which the user may want to type a follow-up while the agent is still
answering. The capture side already exists — the in-flight input bridge — but it
currently discards all typed text. Before designing picocode's version, we
checked how the **pi** coding agent (`/home/gabriel/src/picocode3/pi`, a TypeScript
agent) handles this; its model is the reference design.

### 8.1 How pi implements steering (reference findings)

pi keeps **two** queues and injects them via **pull-based callbacks polled at
safe loop checkpoints** — the TUI/session *owns* the buffer, the agent loop *pulls*
it. This decouples capture (async, while streaming) from consumption (at loop
boundaries).

- **Two queues** (`packages/coding-agent/src/core/agent-session.ts`):
  `_steeringMessages` (mid-run) and `_followUpMessages` (after-run), exposed via
  `getSteeringMessages()` / `getFollowUpMessages()` and drained by `clearQueue()`.
  When a queued user message is actually emitted, the session removes it from the
  queue and fires a `queue_update` event so the UI stays in sync.
- **Agent loop pulls at checkpoints** (`packages/agent/src/agent-loop.ts`,
  `runLoop`):
  - `let pendingMessages = (await config.getSteeringMessages?.()) || []` at the
    start of the run.
  - Inner loop `while (hasMoreToolCalls || pendingMessages.length > 0)`: after
    each tool-call turn, if `pendingMessages` is non-empty it pushes each as a
    **user message into `currentContext.messages`** (emitting `message_start`/
    `message_end`) *before the next `streamAssistantResponse`*. So steering is
    injected **between the agent's internal tool-call turns, within the same
    agent invocation** — true mid-run steering, not post-turn queueing.
  - After the inner loop would stop, `getFollowUpMessages?.()` is polled; if
    non-empty it becomes the next `pendingMessages` and the **outer** loop
    `continue`s — i.e. the run continues with the follow-up as a new user turn.
  - Steering is also re-polled at the end of every inner-loop iteration, so a
    message typed late in the turn is still picked up before the next response.
- **TUI surfacing** (`packages/coding-agent/src/modes/interactive/interactive-mode.ts`):
  `getAllQueuedMessages()` / `clearAllQueues()` read both session queues (plus a
  compaction queue); `updatePendingMessagesDisplay()` renders live
  `"Steering: …"` / `"Follow-up: …"` lines; `restoreQueuedMessagesToEditor()`
  lets the user **dequeue and edit** queued text before it is sent. A **steering
  mode** setting (`"all"` vs `"one-at-a-time"`, in the settings selector:
  *"Enter while streaming queues steering messages. 'one-at-a-time': deliver one,
  wait for response. 'all': deliver all at once."*) controls delivery.

Net: pi does **true mid-run steering** because it runs its *own* agent loop
(`runLoop`), not a black-box provider agent — so it can splice user messages
into the context between tool calls. The queue is pull-based and drains on
injection, with live TUI feedback and a dequeue/edit affordance.

### 8.2 Current bridge in picocode (reuse, don't rebuild)

While `prompt().await` blocks the main task, `run_interactive` (`src/agent.rs`)
spawns a background thread via `repl::spawn_in_flight_reader(guard, cancel, done)`
and joins it once the await returns. Today that thread (`src/repl.rs`,
`spawn_in_flight_reader`):
- handles `Esc` → `cancel.store(true)` (the rig `PromptHook` aborts the in-flight
  request),
- `Ctrl-C` → hard exit, `Ctrl-Z` → suspend,
- and **ignores every other key** — the comment is explicit: "other keys →
  ignored (cannot be typed into the prompt mid-request)".

So the raw-mode stdin reader, the `TermGuard` lifecycle, suspend/resume, and
`Esc`/`Ctrl-C`/`Ctrl-Z` are already wired. What's missing is (a) a line editor in
that thread, (b) rendering the input line mid-turn, (c) the two queues, and (d) a
way to feed them back into the loop.

### 8.3 Design for picocode (mirror pi's two-queue, pull-based model)

Adopt pi's shape: a session/bridge owns `steering` + `followUp` queues; the agent
loop *pulls* them at the checkpoints picocode actually has.

1. **Shared queues.** Add `Arc<Mutex<VecDeque<String>>>` for `steering` and
   `followUp` (plus `has_draft: AtomicBool` for cheap polling), created in
   `run_interactive`, passed into `spawn_in_flight_reader` alongside
   `guard`/`cancel`/`done`. `cancel`/`done` are already `Arc` and process-wide;
   the queues are the same shape.
2. **Line editor in the reader thread.** Reuse the existing pure `LineEditor`
   (`src/repl.rs`): capture `Char`/`Backspace`/`Enter` (preserving `Esc`→cancel,
   `Ctrl-C`→exit, `Ctrl-Z`→suspend). On `Enter`, push the line into `followUp`
   (it can't be injected into the current rig turn — see §8.4) and clear the
   editor. Partial text at turn end can be kept as a pre-filled draft — see
   §8.5.
3. **Render the input line mid-turn.** Entangled with the Phase 3 live-render
   layout. While `LiveRenderer` redraws the response via cursor-up escapes, the
   steering prompt must not be clobbered. Two viable strategies:
   - *Reserved bottom region*: bound the markdown render area to the top
     `term_h − R` rows and draw the steering prompt on the reserved bottom
     line(s); `LiveRenderer` cursor-up math then never reaches it. (Likely use the
     incremental `markdown_terminal::Renderer` in that region, or extend
     `LiveRenderer` with a row cap — it has none today.)
   - *Re-emit after commit*: after every `end_response_segment()` /
     `end_response()` (and after each `push` that leaves the cursor at
     end-of-markdown), redraw the steering line below; manage the cursor so the
     next `push`'s cursor-up starts from end-of-markdown, not the steering line.
   Until streaming lands, design around the reserved-region approach. Mirror pi's
   `"Steering: …"` / `"Follow-up: …"` live display and a dequeue/edit affordance.
4. **Pull at the checkpoints picocode has.**
   - *Follow-up (easy, within rig):* after `prompt` returns and the reader thread
     is joined, check `followUp`. If non-empty, **do not** call `read_input`;
     feed each entry back as `InputAction::Submit` (re-drive the loop) in FIFO
     order, through the normal `prompt(input, Some(&mut history))` path — so it
     lands in the now-persisted `history` (§2.4 fix).
   - *Steering (true mid-run):* picocode **cannot** inject into rig's *current*
     internal tool loop (see §8.4). Until that changes, treat typed-during-turn
     text the same as `followUp` (apply after the turn). When a custom agent loop
     exists (§8.4), poll a `get_steering_messages()` callback at each rig
     `CompletionCall` checkpoint (the streaming `MultiTurnStreamItem::CompletionCall`
     item / `LoggingHook::on_completion_call`) and splice the message into the
     loop's context before the next provider call.
5. **Control-key invariants preserved.** `Esc` still cancels; optionally also
   clears the draft. `Ctrl-C`/`Ctrl-Z` unchanged. The reader thread remains the
   sole raw-mode reader during the await, so there is no double-read race.

### 8.4 pi vs. picocode: why "true steering" needs a loop change

An in-flight provider completion cannot be interrupted mid-stream, so a steering
message is always applied at a *loop boundary*, never mid-token. pi achieves true
mid-run steering because it runs its **own** `runLoop` and can push user messages
into `currentContext.messages` before the next `streamAssistantResponse`.

picocode uses rig's `Agent`, which runs its **own** internal multi-turn tool
loop. rig 0.38's `LoggingHook` exposes only `on_completion_call`,
`on_completion_response`, `on_tool_call`, `on_tool_result` checkpoints, and its
`HookAction` is just `cont` / `terminate` — there is **no** "inject user message"
hook or action. So picocode cannot splice a steering message into rig's *current*
internal loop; the rig turn finishes with the original prompt, and any steering
text can only be applied on the *next* `prompt` call.

**Two realistic tiers:**
- *Tier 1 (short/medium term, within rig):* implement only the `followUp` half —
  queue during the turn, apply after it completes. Matches pi's `followUp` queue
  and the original §8 design. Delivers the user-visible benefit (type while it
  thinks; it picks it up next).
- *Tier 2 (longer term, to match pi's true steering):* replace rig's `Agent`
  internal loop with a custom pi-style loop in picocode that drives the rig
  `CompletionModel`/`Chat` directly, polls `get_steering_messages()` at each
  tool-call boundary, and injects into context. This is the only path to
  mid-run steering and is a larger refactor — flag it as a separate epic, not
  part of the streaming MVP.

### 8.5 Open questions
- **Steering mode (from pi).** Adopt `"all"` (deliver every queued steering
  message at once) vs `"one-at-a-time"` (deliver one, wait for the response, then
  the next). Default `one-at-a-time` matches pi's safer behavior.
- **Auto-submit vs. confirm.** Send queued text as the next turn, or surface for
  confirmation (highlighted, sent on Enter)? Propose auto-submit with `Esc`-to-
  discard-draft; mirror pi's dequeue/edit affordance.
- **Multiple queued lines.** Process FIFO as separate turns (preserves per-
  message history granularity) — prefer separate.
- **Draft-at-turn-end.** If the user is mid-typing when the turn ends, keep the
  draft as the pre-filled prompt rather than enqueuing a half sentence.
- **Non-interactive / quiet.** Steering is TTY-only; `QuietOutput` and piped runs
  skip the queues.

### 8.6 Testing
- **Unit:** the drain loop — given a `followUp` queue `[a, b]`, assert it emits
  two `Submit` actions in order and the queue empties. Pure, no TTY.
- **Integration (`vt100` pty, like `tests/render_scroll.rs`):** simulate an
  in-flight turn, type a line mid-turn, assert the queue contains it after the
  turn and the next `Submit` carries that text; assert `Esc` still cancels and
  the draft is cleared; assert the live `"Steering: …"`/`"Follow-up: …"` line is
  rendered.
- **Tier-2 (custom loop) contract test:** a mocked steering message injected at
  a `CompletionCall` checkpoint must appear in the *next* provider request's
  messages (parity with pi's `runLoop` injection).

---

## 9. References

- `rig-core 0.38.2`: `src/agent/completion.rs` (`stream_prompt`),
  `src/agent/prompt_request/streaming.rs` (`MultiTurnStreamItem`,
  `StreamedAssistantContent`, `stream_to_stdout`, `FinalResponse`),
  `src/agent/prompt_request/mod.rs` (`with_history`, `PromptRequest::send`),
  `src/streaming.rs` (`StreamingPrompt`, `StreamingResult`).
- `codewandler-markdown-terminal 0.2.1`: `src/lib.rs` (`render`, `Renderer`,
  `LiveRenderer` re-export, `Theme`), `src/live.rs` (`LiveRenderer::push`/
  `finish`), `src/highlight.rs` (dependency-free highlighter).
- `codewandler-markdown-stream 0.2.1`: `src/parser.rs` (`Parser` trait:
  `write`/`flush`/`reset`), `src/lib.rs` (`parse`, `StreamParser`).
- Source: `src/agent.rs` (`prompt`, `run_once`, `LoggingHook`, `build_rig_agent`),
  `src/output.rs` (`Output`, `ConsoleOutput::display_text`), `src/thinking.rs`.
- Docs: `doc/thinking_audit.md`, `doc/picocode_rendering.md`.
