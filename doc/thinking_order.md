# Thinking / reasoning displayed after the final answer (OpenRouter)

Status: **Fixed (client-side, no rig fork change required).**

This document records the investigation into the bug where a model's reasoning
/ thinking content was occasionally displayed *below* the final answer, even
though the reasoning clearly described thinking that led up to that answer.
The provider in use is **OpenRouter** (which routes to many underlying models).

---

## 1. Symptom

With `/thinking` enabled, the dimmed reasoning block sometimes appears **after**
the assistant's answer text in the terminal, instead of above it. The trailing
reasoning is *unique* content (not a duplicate), just delivered late by the
stream.

---

## 2. Investigation findings

### 2.1 The prior rig fork fix does not apply to OpenRouter

The project pins a fork of `rig-core` at commit `f31c38c2`
("Avoid reasoning content duplication"), which de-duplicates a complete
`Reasoning` block that *replays* the already-streamed `ReasoningDelta`s.

That fix lives in `crates/rig-core/src/streaming.rs` and only intercepts the
`RawStreamingChoice::Reasoning` variant — a **complete** reasoning block. The
OpenRouter / OpenAI-compatible streaming path
(`crates/rig-core/src/providers/internal/openai_chat_completions_compatible.rs`)
**never yields `RawStreamingChoice::Reasoning`**; it only ever yields
`ReasoningDelta` and `Message`. A grep confirms there is no `RawStreamingChoice::Reasoning`
yield in that loop. OpenRouter is wired through the shared OpenAI Chat
Completions path (`openrouter/completion.rs` → `OpenAICompatibleProfile`), so
the fork's dedup logic is **unreachable for OpenRouter**. The fix was correct
for Anthropic's thinking-block replay and the OpenAI Responses-API reasoning
replay, but cannot touch OpenRouter's ordering.

### 2.2 Reasoning and text are forwarded to the client in provider order

In `crates/rig-core/src/agent/prompt_request/streaming.rs`, the agent runner
forwards `ReasoningDelta` / `Text` items to the consumer via
`StreamedTurnEvent::EmitIngested` **as-is** (no reordering for display). The
only place reasoning is moved *before* text is `canonical_choice` /
`ordered_streaming_assistant_content` (`crates/rig-core/src/agent/run/streamed.rs`),
and that reordering is used **only for the persisted turn history**, never for
what the consumer renders. So display order == provider chunk order.

### 2.3 picocode renders thinking inline, in stream order

`src/output.rs` renders thinking (`stream_thinking`) and answer text
(`stream_text`) immediately to stdout. `src/agent.rs` forwards `ReasoningDelta`
and `Text` in arrival order. Therefore, when the provider delivers reasoning
deltas *after* the content deltas — which some OpenRouter-routed models do
occasionally — the reasoning block is drawn below the answer.

### 2.4 Root cause

The OpenRouter aggregator forwards reasoning and content chunks in whatever
order the underlying model sends them. The client had no mechanism to *withhold*
the answer until it knew no more reasoning would arrive, so late reasoning
leaked below the answer. There is **no explicit provider flag** saying "a final
response was emitted but more reasoning may follow"; the only reliable signal
that no more reasoning can arrive is the **turn boundary** (`CompletionCall`
for tool-call turns, `FinalResponse` at the end of a turn).

### 2.5 Related secondary finding (separate bug)

OpenRouter's structured `reasoning_details` is dropped on the streaming path:
in `openai/completion/streaming.rs` those details are captured only as
`details` and consumed solely by `decorate_streaming_tool_call`
(`openrouter/completion.rs`), which acts only on `ReasoningDetails::Encrypted`
(tool-call signatures). So a model that streams reasoning *only* via
`reasoning_details` (and not `delta.reasoning`) would show **no** thinking at
all — a different symptom (missing, not misordered).

---

## 3. Fix

Implemented **entirely client-side in picocode** — no rig fork change required.

### 3.1 Design

When reasoning is expected for a response (thinking level `!= off`), the
assistant **answer text is buffered and only emitted at the turn boundary**.
Reasoning continues to render live (dimmed, inline). Because reasoning never
arrives after `CompletionCall` / `FinalResponse`, flushing the buffer there
guarantees the layout `[thinking][answer]` for both the normal
(reasoning → answer) and the late (answer → trailing reasoning) patterns.

This is the only way to be correct for the *late* case: streaming cannot peek
at the future, so the answer must be withheld proactively rather than reacting
to a (non-existent) "more reasoning coming" signal.

### 3.2 Changes

**`src/output.rs`**
- `Output` trait: added `fn set_answer_hold(&self, _hold: bool) {}` (default
  no-op).
- `ConsoleOutput`: added `answer_held: Mutex<bool>` and `answer_buffer:
  Mutex<String>`; initialized in `new()` and reset per response in
  `begin_response_inner`.
- `stream_text`: when `answer_held` is set, the delta is appended to
  `answer_buffer` instead of rendered (live path only; the non-TTY path is
  unchanged).
- New private `flush_held_answer() -> bool`: commits the thinking paragraph,
  turns dim off, and renders the buffered answer through a **local** parser so
  `self.parser` stays empty (the caller's per-segment answer-commit then does a
  no-op rather than re-rendering). Returns whether it flushed.
- `end_response_segment()` (the `CompletionCall`/tool boundary) and
  `end_response()` (turn end) now call `flush_held_answer()` and skip their own
  thinking-commit when it already flushed (avoiding a double render).

**`src/agent.rs`**
- Threaded `thinking_level` through `CodeAgent` (struct field + `new` +
  the `build!` macro in `create_agent`).
- In `prompt()`, after `begin_response()`, compute `reasoning_expected =
  thinking_level != Off` and call `self.output.set_answer_hold(reasoning_expected)`.

### 3.3 Tests

- `late_reasoning_answer_is_withheld_until_turn_end` — replays `text → late
  ReasoningDelta` with holding enabled; asserts the answer is buffered while
  reasoning streams and the buffer is empty after `end_response`.
- `answer_not_withheld_when_reasoning_disabled` — confirms non-reasoning runs
  are unaffected.

> Note: end-to-end emission-order is asserted via the buffering mechanism
> (which is what guarantees `[thinking][answer]`), not by capturing stdout —
> `std::io::set_output_capture` is test-crate-only unstable and unavailable
> from a normal crate's test module.

---

## 4. Tradeoff

For reasoning-enabled turns the answer is no longer incremental — it appears
once at each turn's end (before a tool call, or at the final `end_response`).
That is the unavoidable cost of guaranteeing correct ordering without peeking
ahead, and it matches how reasoning UIs typically behave (the thinking is the
live part; the answer lands when the turn completes). Non-reasoning runs are
completely unaffected.
