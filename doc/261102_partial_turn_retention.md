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

1. Take `stream.partial()`, keep its text and drop its tool calls (a call with no result is a real
   pairing violation — `OrphanToolCall` — unlike the stop-reason case, which is only a replay rule).
2. If that text is non-empty, write it to the session immediately as an ordinary assistant message,
   holding a **hand-built** assistant turn (`stop: None`, §2).
3. If the session's last message is not already the synthetic continuation, write one (§5). If it
   is, keep it: **never append a second.** The text is a nudge and must not be empty; an empty
   continuation is dropped by the encoder and silently becomes a prefill (§7).
4. Rebuild: `AgentRun::from_spec(spec, last_message, Some(everything_before_it))`, taking both from
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

A turn can fail more than once. Each failure rebuilds, so the second rebuild starts from a session
whose last message is already the synthetic continuation — and the rule is that it stays the last
message: no second continuation is appended, and none is ever inserted before it.

Both cases fall out of step 4, which takes the session's last message as the prompt:

- **New partial text.** Session becomes `… [asst: partial] [continue]`; the prompt is `continue` and
  the partial is history. One continuation on the wire.
- **No new text** (failure before any delta, or an empty partial). The session still ends in
  `[continue]`, so the rebuild reuses it as the prompt. The wire is
  `[user: prompt] [asst: partial] [user: continue]` — identical to the first retry, with one
  continuation. Verified: reusing the trailing message yields exactly one `continue` at
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

**One gap to close.** `/tree <id> edit` treats a `CustomMessage` as re-askable
(`compaction::is_reaskable_entry` returns true for it), and `reask_from_entry` reads its text. A
synthetic continuation is not a user prompt and must not be offered for re-ask. The marker exists
precisely so this can be decided by `customType`: `prior_continue` is excluded.

**Loader gap.** `extract_messages` (`session/session.rs`) currently handles `Message`,
`BranchSummary` and buffers tool results; everything else is skipped. So a `CustomMessage` on disk
would not reach history today. Note the distinction: pi's comment says it renders as a user message,
prior's loader does not yet. The loader needs a `CustomMessage` arm (content → `UserContent::Text`,
as a user message), which is what makes §3's rebuild see its own continuation after a reload.

## 6. Consequences

- **Streaming is unchanged.** Deltas paint as they arrive; only persistence is deferred to the
  failure. Nothing is buffered until a response completes.
- **The retry visibly repaints.** The abandoned text stays on screen as a finished segment and the
  retried text paints below it. With the text persisted, the screen and the session agree — the
  repaint is honest rather than a divergence.
- **Cost accounting stays prior-side.** `AgentRun::usage` has no setter and a rebuilt run counts
  only its own segment. `turn.rs` already accumulates usage from `CompletionCall` items, so
  continuity is whatever prior keeps; nothing should read `PromptResponse::usage` as a whole-turn
  figure.
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

On a user abort there is no retry to splice into: the partial ends the session, and
`[assistant] [user]` needs no prefill, so the next typed prompt sees it once §5's loader gap is
closed. Worth an explicit test.

Beyond that, a **return with an empty prompt should resume the incomplete turn by default** — the
same path as a retry, when the last user turn did not complete. This changes the shape of §5's
question: the continuation written at retry time is what such a resume finds, so an aborted turn
needs no continuation written at abort time; typing nothing is the signal.

Note this is a behavioural change in the REPL, not only a retry detail. Today an empty submit is
dropped (`handle_submit` returns early on an empty string, and an overrides-only prompt is refused
with "nothing to send"). Those guards are the thing to revisit, not the retry machinery.

### Where the write-time guard lives

**In the projection, not the persister.** The persister and the projection already share one input,
the projected message slice, so a projection-level rule is seen by both writers and by the request
path, while a persister-level rule would be a second place that knows about continuations.

Concretely, the guard belongs beside `project`'s unit walk, which is already where adjacent messages
are related (a unit is a turn plus its results): drop a synthetic continuation that directly follows
another synthetic continuation, keeping the last. That is idempotent (projecting a projected slice
changes nothing), which is the property the whole transform requires.

**The marker must be flattened into the message, not threaded in beside `project`.** Widening the
signature looks cheap (seven internal call sites) but is the wrong fix, because `project` runs on
two different things: the run's log at the persist and request boundaries, which has no session
entries at all, and the loaded session history, which has them. A `&[EntryKind]` parameter would be
fabricated on the live path and real on the load path — one parameter meaning two things, which is
the class of problem the direct-drive work has been deleting all along.

Flattening gives the marker to the message instead, so the rule is identical whether the history
came from disk or from the run, which is also what keeps `project`'s idempotence true: the marker
rides the message, so projecting a projected slice preserves it.

Precedent already exists in this codebase: a tool's parts list rides its result as a reserved key
(`prior_call_parts`) that `project` unpacks and no provider ever sees. A continuation marker follows
the same rule — carried in the stored message, read by `project`, absent from every request.

**Test at the message count, not the content count.** The merge rule above means a wrong append
shows up as an extra content block in the last user message rather than an extra message, so a test
that counts messages catches a second *message* only. Assert that the rebuilt history's explicit
tail is `[assistant, continuation]` with nothing after it, which fails for both shapes of the
mistake.
