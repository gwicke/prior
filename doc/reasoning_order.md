# Reasoning vs. response ordering — how `pi` does it, and updated recommendations

Status: **Investigation complete. No code change in this pass — plan only.**

> **Superseded (row_accounting B3b).** The hold-back this document analyses and
> recommends against has since been **disabled and removed**: `set_answer_hold`,
> `answer_held`, `answer_buffer` and `flush_held_answer` are gone
> (2026-09-28), and the answer renders inline. Everything below is kept as the
> record of why — the ordering artefact is real, the analysis of `pi` still
> applies, and the conclusion (hold-back is the wrong shape) is what the
> removal acts on. Where this document names the removed symbols, read them as
> history.

This follows up on `thinking_order.md` (the client-side "hold the answer until the
turn boundary" fix) and the observed artifact: *a final message that mentions a
final tool call (e.g. a test run) reads as if it references a tool call that was
already shown earlier in the turn.* The user asked how the `pi` agent
(bind-mounted at `pi/`) handles the same problem, to inform a better picocode fix.

---

## 1. How `pi` renders reasoning vs. response

`pi` is a **full-screen, redraw-based TUI**. It keeps a complete in-memory
content model and re-renders the whole conversation viewport on every frame. It
does **not** withhold or buffer the answer. The reasoning/answer ordering problem
is handled implicitly by that architecture rather than by any explicit
"hold/reorder" logic.

### 1.1 Streaming layer — appends in arrival order (no reorder)

`packages/ai/src/api/openai-completions.ts` assembles one `AssistantMessage`
whose `content` array is filled as chunks arrive:

- `ensureTextBlock()` → `blocks.push(textBlock)` (`openai-completions.ts:248-255`)
- `ensureThinkingBlock()` → `blocks.push(thinkingBlock)` (`openai-completions.ts:256-267`)

So thinking and text land in the array **in provider arrival order**. There is
**no sorting or re-ordering** of thinking before text anywhere in the AI layer
(`transform-messages.ts` only downgrades/normalizes cross-model content; it does
not reorder). Consequence: in the rare "text then late reasoning" case, `pi`
will *also* render reasoning **below** the answer — it does not specially fix
that sub-problem. The difference vs. picocode is the render model (see §2), not
the streaming layer.

### 1.2 Render layer — renders the content array as-is

`packages/coding-agent/src/modes/interactive/components/assistant-message.ts`
iterates `message.content` **in array order** (`assistant-message.ts:98-146`),
laying out each `text` / `thinking` block top-to-bottom. No hold, no swap. The
component is re-rendered from the model each frame via `invalidate()` →
`updateContent()` (`assistant-message.ts:44-49, 83-179`).

### 1.3 Timeline assembly — one component per assistant message, full redraw

This is the key structural difference. In
`packages/coding-agent/src/modes/interactive/interactive-mode.ts`:

- `message_start` (assistant) creates a fresh `AssistantMessageComponent` and
  appends it to `chatContainer` (`interactive-mode.ts:2877-2889`).
- `message_update` mutates that component's content in place and calls
  `requestRender()` to redraw the whole screen (`interactive-mode.ts:2892-2924`).
- Each `toolCall` in the content becomes a separate `ToolExecutionComponent`
  interleaved into the same `chatContainer` (`interactive-mode.ts:2898-2914`).
- After tool results, the agent loop (`packages/agent/src/agent-loop.ts`) calls
  `streamAssistantResponse` **again** for the model's continuation. That produces
  a *new* `AssistantMessage` / component for the final answer, placed
  **chronologically after** the tool-call/result block.

So the final answer is **its own message block**, positioned right after the
tool results it refers to, with clear visual separation from the streaming
history. There is **no deferral to the bottom of the turn**, because `pi` never
needed to withhold anything — it just draws the timeline it already has.

### 1.4 Net effect in `pi`

- The artifact picocode shows (final message mentioning a tool call that was
  "shown earlier") **does not occur**, because the final answer is co-located in
  the timeline with the tool call it references and is visually its own block.
- `pi` performs **no reordering or cleanup** of reasoning vs. response. Its
  redraw loop only re-paints the existing `message.content` model in array
  (arrival) order; there is no code path that moves a late thinking block above
  the text it followed (verified: no `unshift`/`.sort`/reorder of thinking vs.
  text anywhere in `ai`/`tui`/`coding-agent`). So if `pi` *did* receive a
  text→late-reasoning stream, it would render reasoning **below** the answer
  exactly like the pre-fix picocode bug — it just wouldn't flicker or
  disappear, because a redraw model never retracts already-emitted content.

### 1.5 Why the misorder is unobserved in `pi` (empirical)

In practice the reasoning models `pi` exercises emit their reasoning **first**,
as a distinct phase, and only then the final answer — an ordered
`reasoning… → answer` stream. On such a stream there is nothing to reorder, so
neither `pi` nor picocode ever shows reasoning below the answer. The
"answer → late reasoning" interleaving that triggers picocode's `answer_hold`
(`thinking_order.md` §2.4) is a provider-specific quirk attributed to certain
OpenRouter-routed models and is *occasional*. The reason `pi` looks clean is the
**ordered stream**, not any redraw-time cleanup.

This bears directly on picocode: `answer_hold` exists solely to defend against
that rare out-of-order stream, yet it is applied to **every** reasoning turn and
is precisely what *causes* the coupling artifact. Defending a rare case by
degrading the common case argues for making the hold conditional (Rec A).

---

## 2. Why picocode is different

picocode renders to an **append-only streaming terminal** (`src/output.rs`): once
a line is written it cannot be moved or inserted above. The only reliable signal
that no more reasoning will arrive is the **turn boundary**
(`CompletionCall` / `FinalResponse`). Hence `thinking_order.md`'s fix *withholds*
the answer (`answer_held` / `answer_buffer`, flushed at
`end_response_segment` and `end_response`) so late reasoning still lands above it.

The cost of that fix — now observed in practice — is exactly the symptom: the
final assistant message of a multi-tool turn is deferred to the **bottom of the
turn**, below every tool call/result, so a summary that names a tool call
("as a final step I ran the test suite") reads as a misplaced/duplicate mention
of a `RunTests` line shown earlier.

`pi` avoids the coupling artifact because its timeline places each response in
its own chronologically-ordered block (and, in practice, the stream is already
ordered so nothing is deferred to begin with). picocode cannot adopt `pi`'s
timeline approach without becoming a full-screen redraw TUI (a large
architectural change, out of scope here).

---

## 3. Updated recommendations for picocode

The goal: keep `thinking_order.md`'s correctness for late reasoning, but remove
the "final message mentions an earlier tool call" coupling artifact.

### Rec A — Make holding *conditional on observed out-of-order reasoning* (primary)

Today `answer_hold` is set `true` for the **entire** turn whenever thinking ≠
`Off` (`src/agent.rs:89-95`). Instead, start `false` and flip it `true` only once
a `ReasoningDelta`/`Reasoning` is seen to arrive **after** a `Text` delta in that
turn (track a `saw_text_before_reasoning` flag in `prompt()`).

- Clean turns (reasoning→text, the common case) render the final answer **live**,
  with correct coupling and **no** deferral — matching `pi`'s behavior for those
  turns.
- Only genuinely mis-ordered turns engage the hold, exactly where the original
  fix is needed.
- Bounded residual leak: a small text slice emitted *before* the first
  out-of-order reasoning is already printed and can't be retracted. Acceptable
  and rare (same class of leak the doc already accepts).

### Rec B — Render the flushed final answer as a clearly delimited block (secondary)

For the turns that still require holding (Rec A's mis-ordered case), mimic `pi`'s
per-message visual separation: at each `flush_held_answer`
(`src/output.rs:601`) in a multi-tool turn, precede the flushed block with a dim
rule / label (e.g. `── answer ──`). This makes the bottom block read as "the
consolidated answer" rather than a trailing mention of an earlier tool call.
Purely cosmetic; zero correctness risk. (Corresponds to "Option A" in the prior
plan.)

### Rec C — Record the principled fix, but do not pursue it now

`pi` demonstrates the clean solution for the **coupling** artifact: a redrawable
**message timeline** where each assistant response is its own block placed
chronologically. (A redraw model would also make the rare late-reasoning
misorder trivially fixable by reordering the content model — but note `pi` does
**not** currently do this, and in practice the misorder is unobserved because
reasoning is emitted before the answer.) Adopting `pi`'s timeline in picocode
would mean moving from append-only streaming to a full-screen redraw renderer —
a significant rewrite, inconsistent with picocode's deliberately streaming
terminal design. Document it as the reference architecture; out of scope for an
incremental fix.

---

## 4. Summary

| Aspect | `pi` | picocode (current) | picocode (proposed) |
|---|---|---|---|
| Render model | Full-screen redraw of a message timeline | Append-only streaming terminal | unchanged |
| Answer withholding | None | Held to turn boundary for whole turn | Held only after out-of-order reasoning observed (Rec A) |
| Final answer placement | Its own block, right after tool result | Deferred to bottom of turn | Live on clean turns; held only on mis-ordered turns |
| "Mentions earlier tool call" artifact | Does not occur | Occurs | Removed on clean turns (Rec A) + visually separated when held (Rec B) |
| Reasoning-below-answer misorder | Not observed in practice (streams ordered); would misorder if it occurred, but no cleanup logic | Fixed by holding | Still fixed by holding; Rec A limits holding to the rare turns that need it |
| Effort | — | — | Rec A + Rec B: localized changes to `agent.rs` + `output.rs` |

**Recommended next step:** the recurring case is the **late-reasoning misorder**
(§5.1), with a confirmed root cause from `reasoning_order_issue.txt` (§5.1 + §5.2):
holding *is* engaged (`thinking_level = "high"`), but `end_response_segment`
(`output.rs:779`) flushes the held answer at every `CompletionCall` on the
broken premise that reasoning never arrives after the call. `tencent/hy3:free`
streams post-toolcall reasoning (text→reasoning), so at the `CompletionCall`
`was_thinking` is still `false` and the answer is emitted early, with the late
reasoning landing below it. Implement the §5.3 fix: in `end_response_segment`,
call `flush_held_answer` only when `was_thinking` is `true`; when it is `false`
(answer-then-tool with no reasoning yet), skip the flush and let the answer emit
at `end_response` after the late reasoning is committed. Keep the existing
`late_reasoning_*` tests and add one replaying `reasoning_order_issue.txt`'s
shape (answer text, then a `Reasoning`/`ReasoningDelta` after the final
`CompletionCall`).

---

## 5. Field observations

### 5.1 Recurrence + concrete repro (this investigation)

The out-of-order artifact **recurred in a live picocode run** while using the
**same models on OpenRouter** where `pi` renders cleanly. Most recent
occurrence, with specifics now known:

- **Provider / model:** `openrouter` / `tencent/hy3:free` (works fine in `pi`
  with the same provider + model).
- **Turn shape:** multi-tool turns.
- **Exact symptom:** reasoning content rendered **after the final response**
  — i.e. the late-reasoning misorder that `thinking_order.md` set out to fix
  (reasoning *below* its answer), **not** merely the "final answer deferred to
  the bottom, narrating an earlier tool call" coupling tradeoff.

So this is the *original* bug resurfacing, not the documented holding tradeoff.
That reframes the investigation: the holding fix (`thinking_order.md` §3) is
supposed to guarantee `[reasoning][answer]`, yet it is failing for this model.
The cause is therefore a flaw in the holding/flush strategy itself, not the
unavoidable tradeoff.

**What is established:**
- `pi` (same models, OpenRouter) → clean, no out-of-order output.
- picocode (same models, OpenRouter) → late-reasoning misorder, repeatedly.
- The rig fork's *yield* order is correct: `CompletionCall` is yielded in the
  `CallModel` step and `display_tool_call` fires later in `CallTools`, so the
  held-answer flush precedes the tool-call line (no flush/tool-call inversion).
- The buffering/flush code has no obvious intra-segment logic bug (no
  double-render, no cross-segment buffer leak; `flush_held_answer` uses
  `mem::take`).

**Root cause (confirmed; `thinking_level = "high"` in this session):** holding
*is* engaged — `set_answer_hold` is gated on `thinking_level != Off`
(`agent.rs:89-95`), and with `default_thinking_level = "high"` the answer is
buffered as designed. The bug is in the **flush timing**: `end_response_segment`
(`output.rs:779`) flushes the held answer at *every* `CompletionCall`, on the
assumption (`thinking_order.md` §2.4 / §3.1) that reasoning never arrives after
the call. `tencent/hy3:free` violates that: it emits its reasoning **after** the
answer/tool call (text→reasoning). At the `CompletionCall`, `was_thinking` is
still `false` (no reasoning has arrived in that segment yet), so
`flush_held_answer` emits the answer *immediately* — and the late reasoning then
streams in and renders **below** it. Result: `[answer][tool call][reasoning]`,
the observed late-reasoning misorder. (An earlier draft blamed an *unengaged*
hold; that is wrong — `thinking_level = "high"` keeps holding on, and the
premature `CompletionCall` flush alone explains the symptom.)

**Transcript evidence (`reasoning_order_issue.txt`, 1232 lines):** the last tool
call is at line 546; the final answer block is at lines 548–592 (right after the
last tool call — *not* at the end of the turn); the entire trailing reasoning
dump is lines 595–1232. This ordering is fully consistent with holding **on** plus
a premature `CompletionCall` flush: the answer is emitted at the `CompletionCall`
(line 548, above its tool call) while still `was_thinking == false`, then the late
reasoning (lines 595+) renders below. It is *not* evidence that holding was off
— an off hold would also render the answer live, but `thinking_level = "high"`
rules that out. The reported phase structure (normal early interleaving, a middle
stretch of tool calls with no visible reasoning, then final answer, then all the
late reasoning) is exactly what text→reasoning delivery produces when the answer
is flushed at the call and the reasoning lands after.

**The broken premise:** `thinking_order.md` §2.4 / §3.1 assumes *"reasoning never
arrives after `CompletionCall` / `FinalResponse`."* `tencent/hy3:free` (and
likely other OpenRouter-routed models) violate it by streaming post-toolcall
reasoning, so the `CompletionCall` flush in `end_response_segment` is premature
for these models.

Why `pi` is unaffected: `pi` does **not** use rig's assembler/turn-loop. It has
its own streaming parser (`packages/ai/src/api/openai-completions.ts`) and a
per-turn redraw timeline, so it never flushes an answer at a tool-call boundary
and the trailing reasoning is associated with the continuation message and placed
above its text. The divergence is in the **streaming stack**, not the provider —
which is exactly why the same model/provider behaves differently.

### 5.2 Status (repro captured)

- **Model + provider:** `openrouter/tencent/hy3:free` ✅ (repro).
- **Turn shape:** multi-tool ✅.
- **Exact symptom:** reasoning *after* the final response (late-reasoning
  misorder, type (a)) — **not** the coupling tradeoff (type (b)).
- **Repro artifact:** `reasoning_order_issue.txt` (1232-line session transcript;
  see §5.1 for the line-level evidence). A `PICOCODE_TRACE` run (§6) is no longer
  needed to *confirm* the cause, but remains useful to verify the fix and to
  confirm whether `Reasoning` items arrive after `CompletionCall` (the secondary
  issue).

### 5.3 Fix plan (root cause confirmed via transcript)

Holding is already engaged (`thinking_level = "high"`); the defect is the
**premature `CompletionCall` flush** in `end_response_segment` (`output.rs:779`).
One change fixes `tencent/hy3:free`:

- **Don't flush the held answer at a `CompletionCall` unless reasoning has
  already appeared in that segment.** At `end_response_segment`, call
  `flush_held_answer` only when `was_thinking` is `true` (a `Reasoning`/
  `ReasoningDelta` has been rendered in this segment, so `dim_on` is on). When
  `was_thinking` is `false` (the segment so far is answer-then-tool with no
  reasoning yet — the text→reasoning case), **skip the flush** and leave the
  answer buffered so it is emitted at `end_response` (turn end), after the late
  reasoning is committed. `end_response` already flushes any remaining buffer, so
  the deferred answer still renders, now above its reasoning.

  Effect on the two segment shapes:
  - *Clean* (reasoning → answer → tool call): `was_thinking` is `true` at the
    `CompletionCall`, so behavior is unchanged — the answer flushes above its
    tool call.
  - *Misordered* (answer → tool call → late reasoning, `tencent/hy3:free`):
    `was_thinking` is `false` at the `CompletionCall`, so the flush is skipped;
    the late reasoning streams in below the tool call and is committed, then the
    answer is flushed at `end_response` → `[reasoning][answer]`. Correct.

**Tradeoff:** for misordered segments the answer intro text is deferred from
above its tool call to the end of the turn (the "final answer deferred, narrating
an earlier tool call" coupling artifact, `thinking_order.md` §4). That is the
unavoidable cost of correct ordering when the model delivers reasoning after
tool calls; it is now bounded to the misordered segments rather than every
reasoning turn.

**Why not just always defer to `end_response` when holding?** Simpler, and also
correct, but it would move *every* segment's answer to the turn end (worse
coupling artifact) even for clean segments that currently render fine above their
tool calls. Gating on `was_thinking` preserves the clean path.

**Out of scope / not the cause:** `pi`'s cleanliness is because it uses its own
streaming parser + redraw timeline and never flushes at a tool boundary (Rec C);
the divergence is the streaming stack, not the provider.

---

## 6. Debugging strategy (timestamped stream trace)

Proposed instrumentation to confirm arrival-order vs. render-order. **Not yet
implemented.**

### 6.1 Env-gated trace
Add `PICOCODE_TRACE=<path>`. With it set:

- In `agent.rs::prompt`'s `while let Some(item)` loop, log each
  `MultiTurnStreamItem` with a monotonic nanosecond clock since turn start:
  `RECV <nanos> <variant> [<delta, truncated to ~80 chars>]` (and tool name for
  `CompletionCall`).
- In `src/output.rs`, log each render method with the same clock:
  - `stream_text` — note **buffered** vs **rendered** (depends on `answer_held`);
  - `stream_thinking`;
  - `end_response_segment` / `flush_held_answer` — buffer length + whether it
    flushed;
  - `end_response`;
  - `display_tool_call(name)`.

### 6.2 What the trace proves
- If every segment's `RECV` order is reasoning→text, the stream is **ordered**
  and holding is *unnecessary* for these models ⇒ the artifact is the holding
  tradeoff, fixed by Rec A (conditional hold).
- If any segment is text→reasoning, holding is earning its keep there and a
  deeper flush-ordering bug (if present) must be hunted separately.

### 6.3 Quick experiment without instrumentation
Temporarily force `set_answer_hold(false)` (or gate it behind a flag) for one
run. If the artifact disappears and final-answer placement becomes correct,
holding is conclusively the cause and a deeper flush bug is ruled out (the only
change is the hold).

### 6.4 Optional render-order cross-check
A lightweight trace of actual stdout writes (or a "render sequence" counter in
each `output` method) to catch any real inversion between the flush and the
tool-call print, in case the rig yield-order reading is wrong on some provider
path.
