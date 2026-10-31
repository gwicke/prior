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

On a retryable mid-stream failure, before the back-off, prior writes two messages and rebuilds the
run. Everything here uses one shape for every model, so nothing depends on provider capability.

1. Take `stream.partial()`, keep its text and drop its tool calls (a call with no result is a real
   pairing violation — `OrphanToolCall` — unlike the stop-reason case, which is only a replay rule).
2. Write the text to the session immediately as an ordinary assistant message, holding a
   **hand-built** assistant turn (`stop: None`, §2).
3. Write a synthetic continuation as a `CustomMessage` (§5), so the next request ends in a user
   message and the model is asked to go on rather than left holding an assistant turn.
4. Rebuild: `AgentRun::from_spec(spec, continue_message, Some(core))`, where `core` is the in-memory
   history the loop already projects, followed by the partial assistant turn. `call_model_step`
   treats the last message as the prompt, so the continuation is the prompt and the partial is
   ordinary history ahead of it. The wire becomes:

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

## 7. Open questions

1. **What should the continuation text say?** The minimum is a nudge to continue. It should not
   restate the corrective feedback (that already rides `extra_context` via the retry hint) and must
   not invite conversation — which is what the old `"continue"` did.
2. **Should the partial's tool calls be recorded anywhere?** Dropped from the request (they have no
   results), but a model that called a tool then failed may want to know it had. Leaning: drop, and
   let the retried attempt re-issue.
3. **Abort.** On a user abort there is no retry to splice into, but the partial must reach the
   *next* turn's history — which works through the session once the loader gap in §5 is closed,
   since `[assistant] [user]` needs no prefill. Worth an explicit test. Note the abort path writes
   no continuation: the partial is the last message until the user says something.
4. **Does the persister's cursor survive the rebuild cleanly?** The partial is written by prior
   before the rebuild, and the rebuilt run's log starts from `core`. If the cursor treats `core` as
   already written, the partial is not written twice; if it does not, it is. This is the most likely
   bug in the change and needs a test rather than reasoning.
