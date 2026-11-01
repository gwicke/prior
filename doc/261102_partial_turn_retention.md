# Retaining a partial turn across a retry

A completion that fails mid-stream leaves text the user has already seen. This doc settles how that
text reaches the session, the next request, and the `/tree` view.

Related: `doc/261030_rig_direct_drive.md` (the drive loop and its four callbacks),
`doc/261030_rig_direct_drive_impl.md` (loop mechanics).

## 1. The problem

Phase 3 made a retry a re-issued `next_step()` on the same `AgentRun`. That removed the synthetic
`"continue"` message — the litter this project set out to delete — but it also removed the only
thing that had been persisting the partial turn.

Today a failed attempt's text lives in two places, neither durable:

- the return value, which the loop clears per attempt so a spliced answer cannot be returned;
- the screen, because `end_response()` closes the segment before the back-off.

The run's log never sees it: `streamed_turn` runs only when a stream finishes. So on a retry the
user watches the abandoned text, then the retried text below it, and the session holds neither.
Reload and the turn is missing its opening.

The invariant we want: **everything rendered is persisted, and the model sees it once.**

## 2. What rig allows

Verified against `rig-core`/`rig-agent` 0.45. Two behaviours decide the design.

**A failed turn is kept in history but dropped from the wire.** `StopReason::is_failure()` is true
for `Error` and `Aborted`; `completion/history.rs:678` skips such a turn while building the wire
form, and `operation/completion.rs:173` calls that adaptor on every request. This is rig's
deliberate rule, and it sits *below* `prepare_request`, so no driver can bypass it.

**The filter keys on the stop reason, not on provenance.** A hand-built
`AssistantMessage::new(content)` leaves `stop: None`, and `is_failure()` is false for `None`. Such a
turn survives the adaptor intact. `stream.partial()` is *not* usable directly, because it marks the
turn aborted — `assistant.message()` from a partial carries `StopReason::Error`/`Aborted` and is
therefore stripped.

This is the non-obvious fact that shapes everything below, and it is pinned by a test.

**`with_history` is unchecked.** Its own doc: "stays unchecked for callers that built the history
themselves." `with_validated_history` is the checked variant.

**The run's last message is its prompt.** `call_model_step` does `history.split_last()`. So a
rebuilt run's shape is `from_spec(spec, partial_as_prompt, Some(core))` — the partial *is* the
prompt and must not also appear in `core`, or the request carries it twice and `validate` rejects
the history as `consecutive assistant messages`.

## 3. Design

On a retryable mid-stream failure, before the back-off, prior writes what is new and rebuilds the
run with the session's exact history. Everything here uses one shape for every model, so nothing
depends on provider capability.

1. Take `stream.partial()`, keep its text **and its reasoning**, and drop its tool calls. The two
   exclusions differ: a call with no result is a real pairing violation (`OrphanToolCall`), while
   reasoning is kept and left to `reasoning_in_context` — whose `AbortedTurnsOnly` mode exists for
   exactly this turn, the trailing one with no provider `responseId`. (Kept-but-invisible is the
   setting's decision; dropped-at-write is prior's, and would lose tokens the user paid for.)
2. **If nothing of the model's own output survives, do nothing more** — re-issue the attempt in
   place, as phase 3 did. There is no partial to preserve, so a rebuild would only reset the budget
   and re-read the session for nothing. (A 429 before the first delta is the common case, and it
   must not grow the transcript.) Reasoning alone *is* something to preserve, so it takes the normal
   retry path: write, continuation, rebuild.
3. Write that content to the session immediately as an ordinary assistant message, holding a
   **hand-built** assistant turn (`stop: None`, §2).
4. If the session's last message is not already the synthetic continuation, write one (§5). If it
   is, keep it: **never append a second.** The text is a nudge and must not be empty; an empty
   continuation is dropped by the encoder and silently becomes a prefill (§7).
5. Rebuild: `AgentRun::from_spec(spec, last_message, Some(everything_before_it))`, taking both from
   the session's history. `call_model_step` treats the last message as the prompt, so whatever ends
   the session is the prompt and the rest is ordinary history. On the first retry that is
   `[user: prompt] [assistant: partial] [user: continue]`. The wire becomes:

   ```text
   [user: the original prompt] [assistant: the partial text] [user: continue]
   ```

The model sees its own text as its own turn and is asked to carry on. No prefill is involved, so the
shape is legal on every provider, and the partial is not duplicated — it appears once, in `core`,
and the continuation is what `split_last` takes as the prompt.

This is what prior did before phase 3 (capture the output, reconstruct the history) with one change:
the reconstruction now feeds a fresh `AgentRun` rather than a fresh `prompt()` call. The difference
from the old behaviour is the marker in §5, which is what makes the synthetic message identifiable
rather than a `"continue"` indistinguishable from a user prompt.

### Consecutive retries append nothing

A turn can fail more than once. After the first rebuild the session's last message is already the
synthetic continuation, and the rule is that it stays the last message: no second continuation is
appended, and none is ever inserted before it.

Both cases are handled without a second append:

- **New partial text.** Session becomes `… [asst: partial] [continue]`; the rebuild takes `continue`
  as its prompt and the partial is history. One continuation on the wire.
- **No new text** (failure before any delta, or an empty partial). Nothing is written at all — step
  2 — so the retry re-issues the attempt on the run it already has. After the *first* rebuild that
  run's history already ends in `[continue]`, so the wire is unchanged however many times the
  attempt fails. Verified: reusing the trailing message yields exactly one `continue` at
  `projection_start` of 1.

The alternative — appending a fresh continuation per attempt — grows the history by one message per
retry. **It fails quietly, not loudly.** rig merges consecutive user messages into one message
holding both content blocks, so nothing errors and a provider sees a single user turn. Measured: two
`"continue"` messages become one message whose content is `["continue", "continue"]`, and the OpenAI
chat encoder sends a multi-part user message as a `content` array rather than joined text. The
duplicate is therefore *structurally visible* in the request — a fourth message does not appear, but
an extra content block does — and it is plainly visible in the session, which replays what was
written.

A test can assert either way. Assert on the pre-wire history, because it names the intent ("the
retry appends nothing") rather than inferring it from a content count.

The session is append-only (`Session` has `move_leaf` but no truncate or delete), so a wrong append
cannot be undone. This must be a write-time guard, not a repair.

Two further consequences, both of which need care:

- **`Persister::new` is built for the dispatch case.** It seeds the cursor from
  `AgentRun::projection_start()`, documented as "everything below it is the prompt (persisted at
  dispatch) or loaded history (already stored)". For a rebuilt run that holds *only* the
  continuation in its log (`messages().len() == 1`, `projection_start() == 1`, with `core` in
  `chat_history`), the cursor starts at 1 and the continuation is correctly not written again — but
  only because prior wrote it. If step 3 is skipped *and* the partial is empty, the persister has
  nothing to skip past and the guard has to come from the write side.
- **Two failures with no new text must not write the partial twice.** An empty partial is skipped
  (step 2), so the session keeps the earlier partial and its continuation untouched.

### Why not splice the partial into the live run

`RequestPatch::history` would put the partial in front of the provider without touching
`run.messages()`. But `before_call` receives the run's log, so the splice is a one-shot with two
fatal properties: it exists for exactly one request, and re-appending on the next one places the
partial *after* turns that followed it, reordering the suffix as the turn progresses. Rebuilding
keeps the run's history and the model's context identical, which every other part of the
direct-drive design depends on.

## 4. Continuation, and the prefill question

A request ending in an assistant message is a *prefill*: the model continues from where it stops.
Not every model accepts one. rig records this per model as `Compat::prefill`, and rig's own catalog
(`catalog/models.json`) says:

| provider                                 | models                 | `prefill: true`              |
| ---------------------------------------- | ---------------------- | ---------------------------- |
| anthropic                                | 22                     | 12                           |
| openrouter                               | 400                    | 4 (all `anthropic/claude-*`) |
| deepseek                                 | 4                      | 0                            |
| openai                                   | 77                     | 0                            |
| xai, moonshot, zai, groq, gemini, ollama | 23, 14, 21, 17, 51, 48 | 0                            |

Unknown models default to `false` (`catalog/row.rs`: `prefill.unwrap_or(false)`). Anthropic rows
that are `true` include claude-sonnet-4-5, haiku-4-5 and opus-4-5; `claude-opus-4-6`,
`claude-opus-5-5` and `gpt-5` are `false`.

So a shape ending in an assistant turn works on Anthropic (and Anthropic through OpenRouter) and
essentially nothing else — not the common case for prior. §3 therefore ends in a user message and
never relies on prefill.

The cost is one short user message per retry, on models that could have continued in place. That is
the trade this doc accepts: one code path, no capability lookup, and no branch that goes untested on
most models. The message carries the §5 marker, so if a prefill path is ever added for Anthropic it
is additive — the two forms are distinguishable and cannot be confused.

One property to keep in mind: adding a user message after the partial does not by itself invite
conversation the way phase 3's `"continue"`-as-prompt did. That message was a *new dispatch* — a
fresh user turn the run treated as a first-class prompt and answered conversationally. This one is
inside a rebuilt continuation of the same attempt, expires with it on success, and is marked on
disk, so a reload can tell it apart from a real steer.

## 5. Marking the synthetic message

A `Message::User` carries only `Vec<UserContent>` — no metadata slot, and rig constructs provider
payloads from it directly. So the marker cannot live on the rig message. It lives in prior's own
record, using the entry type pi already defines for exactly this (`src/session/types.rs`):

```text
{"type":"custom_message","customType":"prior_continue","content":"…","id":"…", …}
```

- **It is context.** pi's own comment: "Custom message entry (DOES participate in LLM context,
  rendered as user message)" (`doc/tree_compaction.md`, `doc/session_storage.md` §entry types).
- **It is distinguishable structurally** — by entry `type`, never by matching content. A genuine
  user steer is `type: "message"` with `role: "user"`, so the two cannot collide.
- **It does not disturb turn accounting.** `user_message_count` counts `"type":"message"` lines only
  (`session/repo.rs`), so a synthetic continuation does not inflate the turn count shown by `/stat`.

`/tree` already renders it distinctly: `v25_tree.rs` prints `[custom msg] {customType}`, so the view
can filter or label it without parsing text.

**Two gaps, both closed.** `is_reaskable_entry` treated *every* `CustomMessage` as re-askable, and
`reask_from_entry` would have put `"continue"` in the input buffer as if the user had typed it; it
now returns false for `prior_continue`. And `extract_messages` skipped `CustomMessage` entirely, so
one on disk would not have reached history at all — it now has an arm pushing the content as a user
message, which is what makes §3's rebuild see its own continuation after a reload. Both are covered
by `a_continuation_is_written_once_and_reaches_reloaded_history`.

## 6. What landed

Implemented in `src/agent/continuation.rs` (the marker and its text), `Session`'s
`append_custom_message` / `ensure_custom_message` / `ends_mid_turn`, `drive.rs`'s
`capture_abandoned` / `take_abandoned` / `rebuild_from`, and `turn.rs`'s `persist_abandoned` /
`rebuild_after_partial`.

Two refinements over the plan above, both from constraints found while writing it:

- **The continuation is written when a retry is certain, not when the failure is seen.** A turn that
  is about to give up has nothing to continue into, so appending a nudge then would be litter with
  no way to remove it.
- **A failure with nothing streamed rebuilds nothing.** It re-issues the attempt in place, exactly
  as phase 3 did. There is no partial to preserve, so the rebuild would only reset the budget and
  re-read the session for no benefit. "Nothing" means no content of the model's own at all — a
  truncated stream that had only produced reasoning does rebuild, since the reasoning is kept (§3
  step 1) and is what the `AbortedTurnsOnly` setting exists to carry forward.
- **`take_abandoned` keeps reasoning and drops tool calls** (`9153331` fixed an earlier version that
  dropped both). Pinned by `an_abandoned_attempt_keeps_its_reasoning_and_drops_its_calls` in
  `drive.rs`.

Tests: `tests/partial_turn_retention.rs` drives the real binary through a broken stream (retry) and
a hung stream plus `Esc` (abort, then an empty-prompt resume), asserting on the session entries and
the request bodies. `Session`'s `a_continuation_is_written_once_and_reaches_reloaded_history` covers
the append-once guard, the loader arm and the re-ask exclusion. Each was verified to fail against
the behaviour it pins.

## 6a. Consequences

- **Streaming is unchanged.** Deltas paint as they arrive; only persistence is deferred to the
  failure. Nothing is buffered until a response completes.
- **Nothing redraws, and the retry path writes no bytes of its own.** "Repaint" was the wrong word:
  the abandoned text is committed as a finished segment by `end_response`, and the retried text is
  ordinary streamed content appended below it. Content reaches the screen only through
  `Draw::draw`'s consume-write-reposition (`draw.rs`), never by writing to stdout directly, and the
  only row-addressed output is the live frame (status bar, input) plus the re-anchor and a
  width-resize reflow. (A previous revision of this note cited `plan_append` for the append — that
  function is `ApplyPolicy::Append`, which no production caller uses; it is a test-only policy.)
  With the text now persisted, the screen and the session hold the same content, which is the
  property that matters.
- **Failures are reported on the error channel, not stderr.** Every persist or rebuild warning on
  the retention path goes through `Content::display_error`, which routes to the draw pipeline as
  `Stream::Error`. `eprintln!` would be wrong here for a concrete reason: a raw-mode turn owns the
  screen, and a stray stderr line corrupts the live frame *and* desynchronises the row accounting
  that the next frame's clear depends on. stderr stays reserved for the catastrophic — a panic, a
  failed startup.
- **Cost accounting stays prior-side, and in practice nothing is affected.** `AgentRun::usage` has
  no setter, so a rebuilt run counts only its own segment and `PromptResponse::usage` under-reports
  a rebuilt turn. But the final response is consumed for its *text* (`turn.rs`'s
  `FinalResponse(_resp)` arm discards it), and every figure the user sees comes from
  `CompletionCall` items, which arrive per attempt into `SessionUsage`. So the under-report is
  reachable only by code that read the response's usage directly — of which there is none. Recorded
  because it would be a trap for a future caller, not because it is currently wrong.
- **A rebuilt run's `current_turn` resets**, hence the `max_turns` reduction in §3.

## 7. Decisions and open questions

### Continuation text

The text is a nudge to continue. It must not restate the corrective feedback (that already rides
`extra_context` via the retry hint) and must not invite conversation — which is what the old
`"continue"` did. Starting from `"continue"` is fine.

**An empty continuation is not an option, and the reason is worth recording.** A `UserContent::Text`
holding `""` passes `CompletionRequest::validate_message_content`, but the request adaptor drops it
when it encodes the wire. Measured:

```text
""           -> 2 messages, last is the assistant turn
" "          -> 2 messages, last is the assistant turn
"continue"   -> 3 messages, last is a user message
```

So an empty (or whitespace-only) continuation does not mean "no instruction": it silently turns the
request into a **prefill**, the shape §4 established is illegal on most providers. It would fail on
deepseek and OpenAI while appearing to work on Anthropic, which is the worst combination. The
continuation must carry text.

### Partial tool calls

Dropped. They have no results, and a call without a result is a genuine pairing violation
(`OrphanToolCall`), unlike §2's replay rule. The retried attempt re-issues whatever it needs.

### Abort, and resuming without retyping

On a user abort there is no retry to continue into: the partial ends the session, and
`[assistant] [user]` needs no prefill, so the next typed prompt sees it. Tested.

A **return with an empty prompt resumes the incomplete turn by default** — the same path as a retry,
when the last user turn did not complete. `handle_submit` checks the session's tail
(`Session::ends_mid_turn`) before either of its old guards, so an empty submit is a continuation
rather than a no-op, and the overrides-only refusal is unchanged for prompts that *do* carry text.

**"Did not complete" is `responseId`, not the message role.** A finished turn also ends the branch
on an assistant message, so a role test would offer to continue a reply the model had already
finished — the first draft did exactly that. The two are distinguishable because prior writes a
partial through `assistant_message(None, ..)` (no origin, hence no id) while a completed turn is
stored from rig's committed log, where the head keeps the provider's origin. pi's schema has the
same absence for the same reason, so the signal survives a reload. Pinned by
`ends_mid_turn_discriminates_on_the_response_id`, verified to fail on the role-only test.

One refinement the implementation needed, and it settles the ordering question above:
**the continuation is written by the resume, not by the abort.** An aborted turn's branch ends on
the partial, which is what makes `[assistant] [user]` the next typed prompt's shape. A resume is
what needs the nudge, because without one the rebuilt run would take the partial as its *prompt* — a
prefill. `ensure_custom_message` makes that a no-op when a retry already wrote one, so resuming
after a retry is exactly a retry.

### Where the write-time guard lives

**In `Session`, at write time** — `ensure_custom_message`, backed by `ends_with_custom_message`. Not
in the projection, which this doc previously proposed. Two reasons, one of them decisive:

- **A projection rule runs too late.** The session is append-only (`move_leaf` exists, no truncate),
  so a duplicate that reached disk can never be removed. Projecting it away would leave the litter
  in the file and merely hide it from the request — the opposite of what the marker is for.
- **The projection has nothing to key on.** It sees a plain `Message::User`; the marker is a session
  entry kind. Widening `project` to take entry kinds is the trap §5's flattening discussion rejects:
  `project` runs on the run's log (no session entries) as well as on loaded history, so a
  `&[EntryKind]` parameter would be fabricated on one path and real on the other.

So the guard is stated where the fact is known: "does the branch already end with this marker". It
is one `matches!` on the branch tail, and it makes `ensure_custom_message` idempotent by
construction rather than by a downstream repair.

**Test at the message count, not the content count.** The merge rule above means a wrong append
shows up as an extra content block in the last user message rather than an extra message, so a test
that counts messages catches a second *message* only. Assert that the rebuilt history's explicit
tail is `[assistant, continuation]` with nothing after it, which fails for both shapes of the
mistake.

The test that pins this asserts on the persisted entries — one `custom_message` with
`customType == "prior_continue"` — and was verified to fail when the guard is removed.
