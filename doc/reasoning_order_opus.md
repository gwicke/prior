# Reasoning rendered out of order with tool calls / final answer — ROOT CAUSED

Provider **OpenRouter**, model **tencent/hy3:free**. prior renders a turn's
reasoning *after* the final answer (and after the tool calls it discusses); the
same model orders correctly in `pi`.

Status: **root-caused, fixed, and regression-tested.** Proven end-to-end with a
live trace, a faithful replay of the real stream, and isolation down to the
exact markdown-parser behavior. Two independent code bugs were identified
(§4 primary, §5 secondary) and **both are now fixed** in `src/output.rs`
(see §10).

Everything below is **[VERIFIED]** — reproduced against the live trace
(`/tmp/pico.log`), the real captured turn (`reasoning_order_issue.txt`), or the
exact source — unless marked **[NOTE]**.

---

## 1. The bug in one line

prior renders streamed reasoning through a **separate GFM markdown parser**
and commits it at segment boundaries with `StreamParser::write(b"\n\n")`, which
**does not close a trailing list** (GFM lists need lookahead to close). That
reasoning is stranded in the thinking parser until `end_response` calls
`thinking_parser.flush()` — which runs **after** the answer parser is flushed.
So list-structured reasoning lands **below** the answer.

---

## 2. Ground truth from the live trace (`PRIOR_TRACE_STREAM`) [VERIFIED]

A temporary env-gated trace (since removed — see §9) logged every consumed
`MultiTurnStreamItem` and hook call in arrival order (15023 items). Structure:

- **Every model turn** emits, in order: `ReasoningDelta`… then `Text`… then
  `ToolCallDelta`… then `CompletionCall` (→ `end_response_segment`), then the
  `CallTools` step where `HOOK on_tool_call`/`on_tool_result` print. So the
  model streams reasoning **before** text **before** the tool call, every turn.
- **The final turn** = `532 × ReasoningDelta`, then `665 × Text`, then
  `CompletionCall`, `FinalResponse`. Reasoning is streamed **first**, the answer
  **second**.

Yet the *rendering* ends with the reasoning ("…Let me write it."). The final
turn's reasoning (streamed first) is displayed last. That is the bug, captured
against the live provider — **not** a provider ordering quirk (rig forwards
reasoning live, §6).

---

## 3. Faithful replay reproduces it, minimal replay does not [VERIFIED]

Replaying the **real** final-turn reasoning then text (extracted from the trace)
through the real `ConsoleOutput` under a PTY: the reasoning's tail ("Let me
write it.") renders at the very end, after the whole answer — in **both** hold
states.

A **minimal** single-turn replay (reasoning = two plain paragraphs, then answer)
renders **correctly** (reasoning above answer). So the trigger is the *content
structure* of the reasoning, not the flow. This is why standalone no-tool
prose turns look fine while tool-task turns (whose reasoning is long and
**list-structured** — "Files changed: - …", "Structure: - Headline - Table …")
misorder. The dependency is on markdown structure, which correlates with tool
work, not on tool calls per se. **[NOTE]** so "never happens without tool calls"
is a strong correlation, not a hard rule.

---

## 4. PRIMARY ROOT CAUSE: `write("\n\n")` can't close a list; flush order dumps it after the answer [VERIFIED]

Feeding the real reasoning to `markdown_stream::StreamParser` in chunks:

```text
events from write():        22      # rendered live while streaming
events from "\n\n" commit:   0      # what end_response_segment / stream_text emit
events ONLY from flush():   101      # the stranded bulk
```

Construct isolation (same parser):

```text
plain paragraph              write=0  after \n\n=4   flush-only=1
trailing bullet list         write=4  after \n\n=0   flush-only=12   ← culprit
list then paragraph          write=4  after \n\n=11  flush-only=1
```

A **trailing bullet/numbered list** emits nothing on `write("\n\n")` and is only
released by `flush()` — because GFM must see a non-list line (or EOF) to know
the list ended and whether it's loose/tight
(`codewandler-markdown/crates/markdown-stream`).

prior commits the *thinking* parser with `write(b"\n\n")` in three places and
**never** `flush()` mid-response:

- `stream_text` reasoning→answer transition (`src/output.rs:738`),
- `end_response_segment` (`src/output.rs:...` thinking-commit branch),
- `flush_held_answer` (`src/output.rs:616`).

So when a turn's reasoning ends in a list, its buffered blocks stay in the
thinking parser. They are finally released only by `end_response`:

```rust
// src/output.rs:833-843
{ let events = parser.flush();          renderer.feed(&events, &mut out); }  // ANSWER first
{ let events = thinking_parser.flush(); trenderer.feed(&events, &mut out); } // THINKING second
```

The answer is flushed **before** the thinking parser (line 835 before 842), so
the stranded reasoning is emitted **after** the answer. That is the exact
mechanism producing the reported layout.

**Fix directions:** commit the thinking parser with `flush()` + re-init a fresh
`StreamParser`/`Renderer` at each segment boundary (as the answer *renderer* is
already reset in `end_response_segment`), instead of `write(b"\n\n")`; and/or in
`end_response` flush the **thinking** parser before the answer. Flushing per
segment is the robust fix — it guarantees nothing lookahead-dependent (lists,
tables, fences) is ever stranded across the answer.

---

## 5. SECONDARY BUG: `flush_held_answer` drops the answer's last paragraph [VERIFIED]

`src/output.rs:623-626` renders the held answer with
`local_parser.write(buffer)` and **never** `local_parser.flush()`. By the same
parser semantics, an answer whose final paragraph lacks a trailing blank line is
never emitted (PTY-confirmed in HOLD mode: the answer's last paragraph
vanishes). Independent of §4. Fix: add `local_parser.flush()` and feed those
events too.

Because of this, and because `flush_held_answer` always commits thinking *above*
the answer, the answer-hold path (`thinking_level != Off`) both mis-drops
content and cannot produce the reported "reasoning below a complete answer" —
consistent with the run behaving as hold-off. (Whether the hold engaged is now
moot: §4 is the real cause and occurs regardless of hold state, as shown in §3.)

---

## 6. rig does not reorder the consumer stream [VERIFIED]

- OpenRouter reuses the shared OpenAI chat streamer
  (`openai_chat_completions_compatible.rs`), which yields only
  `ReasoningDelta` + `Message`, never a complete
  `RawStreamingChoice::Reasoning`; the fork's dedup commit `f31c38c2` is
  unreachable here.
- The assembler (`agent/run/streamed.rs::ingest`) forwards `Reasoning` /
  `ReasoningDelta` as `EmitIngested` (live); tool calls are buffered and
  surfaced in the next `CallTools` step. Its reasoning-before-text reordering
  (`ordered_streaming_assistant_content`) applies **only to persisted history**.

Confirmed by §2: reasoning items arrive live, before text, per turn. The
misordering is entirely in `ConsoleOutput` (§4).

---

## 7. Corrections to earlier drafts of this doc [VERIFIED]

An accuracy review (against the current tree) flagged real errors in my earlier
draft; corrected here:

1. **False claim retracted:** the earlier draft said `thinking_order.md` "cites
   unit tests that do not exist." They **do exist** —
   `late_reasoning_answer_is_withheld_until_turn_end` (`src/output.rs:1569`) and
   `answer_not_withheld_when_reasoning_disabled` (`src/output.rs:1597`), added
   in `d554663`. (`thinking_order.md`'s *"guarantees ordering"* claim is still
   wrong — §5 shows the hold path is broken — but the tests exist. Note also
   neither test covers §5's dropped-paragraph case or §4 at all.)
2. **Overstatement fixed:** the hold does not commit thinking "unconditionally";
   `flush_held_answer` gates the thinking commit on `was_thinking` (dim state,
   `output.rs:614`) — true in a reasoning run, so substantively but not
   literally unconditional.
3. **Imprecision fixed:** `LoggingHook` calls `display_tool_call` /
   `display_tool_result` (which print and call `stop_thinking`), not
   `stop_thinking` directly.
4. **Retracted guess:** "hy3 emits reasoning even when thinking is off" was
   never verified and is moot (thinking was high; the `reasoning` param was
   sent).

---

## 8. Why `pi` is unaffected [NOTE]

Not verified from pi's source. Consistent with §4: pi renders reasoning and
answer in one arrival-ordered flow (a single parser / no separate deferred
thinking document), so nothing is stranded and re-emitted after the answer.
prior's separate thinking parser + `write("\n\n")` commit + answer-first
final flush is the specific combination that reorders.

---

## 9. Instrumentation (removed)

Root-causing used a temporary env-gated trace (`PRIOR_TRACE_STREAM`) in
`src/agent.rs` that logged every consumed `MultiTurnStreamItem` and hook call in
arrival order — that is how the §2 timeline was captured. It has since been
**removed** now that the bug is fixed. To reproduce the capture, re-add an
`eprintln!` in `prompt()`'s stream-consume `match` (item kind) and in
`LoggingHook::on_tool_call`/`on_tool_result`, gated on an env var, and run with
stdout/stderr redirected to separate files.

---

## 10. Fix implemented [DONE]

`src/output.rs`:

- **New `commit_thinking(&self, out)`** — flushes the thinking parser with
  `StreamParser::flush()` and starts a fresh parser (keeping the dimmed
  renderer). Replaces the three `thinking_parser.write(b"\n\n")` commit sites
  (`stream_text` transition, `end_response_segment`, `flush_held_answer`). This
  releases lookahead-dependent blocks (lists/tables/fences) at the boundary
  instead of stranding them until the answer-first final flush — the §4 fix.
- **`flush_held_answer`** now `flush()`es the local answer parser (not just
  `write`), so a held answer's final unterminated paragraph is emitted rather
  than dropped — the §5 fix.
- **`end_response_segment`** answer commit now `flush()`es + resets the answer
  parser too (was `write(b"\n\n")`), for the same robustness on a segment
  whose answer text ends in a list/table.
- **Blank-line separator between reasoning and answer.** At every
  reasoning→answer boundary (`stream_text` transition, `end_response_segment`,
  `flush_held_answer`) the one blank line is emitted by writing `\n` to the
  *same* `out` handle the reasoning/answer use — placed **after**
  `ensure_dim(false)` (the dim-reset SGR). Ordering matters: a GFM list/table
  emits no trailing newline of its own, so a `\n` placed before the reset is
  swallowed as the block's closing newline (no blank); placing it after the
  `\x1b[0m` makes it a real blank line. Using `out.write_all(b"\n")` (not the
  `println!` macro) is required so the blank lands in the right write buffer.
  This is the fix for the follow-up report that the answer started on the line
  immediately after the reasoning with no empty line.

**Verification:**

- Replaying the real captured final turn through the fixed `ConsoleOutput`
  (PTY): the reasoning (incl. its trailing list "…Let me write it.") now renders
  **before** the answer; nothing leaks below it; the held answer's last
  paragraph is present; and a blank line separates the reasoning from the
  answer.
- New regression test
  `output::tests::reasoning_before_answer_has_blank_line_separator` captures
  stdout at the fd level and asserts a trailing-list reasoning block renders
  **before** the answer **and** is separated from it by a blank line. It
  **fails** against the old `write(b"\n\n")` commit / missing blank and
  **passes** with `flush()` + the `out`-written blank. Full suite: 158 passed.
