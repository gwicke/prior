//! The execution half of a turn: `CodeAgent::prompt` and its companions.
//!
//! `prompt` runs one user input through the rig agent and reconciles the
//! session: it pumps the stream (assistant turns, tool results, reasoning)
//! into a [`TurnTranscript`], drives the auto-retry/back-off/Esc-cancel loop on
//! a classifiable provider error, and persists whatever actually completed —
//! nothing more — before the REPL retries. `run_once` is the one-shot variant
//! for piped/batch input; `summarize` and `summarize_with_session_context` are
//! the plain and session-scoped summarization requests.
//!
//! The turn-pump primitives (`dispatch_in_turn_event`, the back-off sleep) live
//! in `pumps`; the message shaping (`persist_turn`, `maybe_strip_reasoning`,
//! the edit-call splits) in `transcript`; error classification in `errors`.
//! This module is where they come together against a live `CodeAgent`.

use super::errors::{classify_prompt_error, classify_stream_error};
use super::pumps::dispatch_in_turn_event;
use super::transcript::{normalize_call_ids, reasoning_text, TurnTranscript};
use crate::Result;
use futures::StreamExt;
use rig_agent::agent::{MultiTurnStreamItem, StreamingError};
use rig_agent::completion::{Prompt, PromptError};
use rig_agent::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingPrompt};
use rig_core::message::{AssistantContent, Message, Reasoning, Text, UserContent};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::agent::build::{request_additional_params, LoggingHook};
use crate::agent::compaction::session_context_messages;

use crate::agent::CodeAgent;
use crate::agent::{estimate_ctx_with, TurnPersistence};
use crate::session::Session;
use std::sync::Mutex;

impl CodeAgent {
    /// `params` are the params to send *for this request*: the live ones, or
    /// the live ones with this turn's per-turn overrides applied. Passing them
    /// in rather than reading them off `AppState` is what makes an override
    /// per-request — nothing here mutates the live params, so there is nothing
    /// to restore and no path by which one leaks into the next turn.
    ///
    /// **This future is `!Send`; the summarizers below are not.** Since U4.3
    /// the turn carries the REPL's prompt session in, and that session is
    /// single-threaded by construction — `Arc` line windows, `Arc` menu views, a
    /// sync `mpsc` completer — and it never leaves the task `run_interactive`
    /// runs on. Requiring `Send` would mean rewriting that chain to `Arc` for a
    /// property nothing uses: `main` awaits `run_interactive`, every test
    /// awaits it, and no request is ever spawned. (`run_once` inherits it — it
    /// awaits this.)
    ///
    /// It is recorded here as a property of the future rather than as a bound
    /// on an interface, which is what the trait this replaces declared: an
    /// interface-wide `?Send` would cost every method its `Send`-ness to buy
    /// one of them, and would need updating by hand if the session ever became
    /// `Send`. A caller who wants to `tokio::spawn` a request makes the session
    /// chain `Send` first, and this is where that becomes visible.
    pub async fn prompt(
        &self,
        input: &str,
        session: Option<&Session>,
        turn: &TurnPersistence,
        params: &crate::params::ModelParams,
        in_turn: Option<crate::repl::InTurnInput<'_>>,
    ) -> Result<String> {
        // Fresh request: clear any stale cancel flag from a previous abort so a
        // prior `Esc` doesn't silently cancel this prompt. The turn's key
        // dispatcher — this select's key branch, or the back-off pump between
        // attempts — is what fires `cancel` on `<Esc>`; the in-flight reader
        // only routes keys (U4.3), and it is always joined (after `done` is
        // set) before the next `prompt()` begins. A leftover flag can therefore
        // only be one fired in the gap after the previous attempt ended, which
        // is exactly what this clear is for. See
        // doc/turn_cancellation_persistence_bugs.md §3.
        self.user_cancel.store(false, Ordering::SeqCst);

        // Capture this turn's base frame (session user-prompt count at entry)
        // once, so retries of the same turn keep the same reference point for
        // the persistence de-dup decision. Idempotent.
        turn.initialize_base(session).await;

        // Build prior context from session, or use empty vec as fallback.
        let prior: Vec<Message> = match session {
            Some(s) => match session_context_messages(
                s,
                self.reasoning_in_context,
                &self.provider,
                &self.model,
            )
            .await
            {
                Ok(m) => m,
                Err(e) => {
                    // Session index may be stale; log and fall back to empty.
                    eprintln!("Warning: session build_context failed: {}", e);
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        // The first turn has no reported usage at all (the first completion
        // call is still in flight), so the status line would have no context
        // number for exactly the turn a new session is most likely to be
        // watched. The estimate `/tree compact` already uses is computable from
        // the messages in hand — `prior` plus this turn's prompt — so record it
        // as an *estimate*; the provider's own count replaces it
        // through `context_figure` from the second turn on, with nothing
        // impersonating a report.
        let estimated_ctx = estimate_ctx_with(&prior, input);
        {
            let mut usage = self.usage.lock().unwrap();
            usage.estimated_input_tokens = estimated_ctx;
        }

        // `stream_prompt(...).await` yields the item stream (rig 0.40's
        // `IntoFuture` returns the stream itself, not a collected response).
        // The run-scoped transcript is shared with the hook and the stream
        // loop so a mid-turn failure can persist exactly what completed.
        let transcript = Arc::new(Mutex::new(TurnTranscript::default()));
        // Per-request params, set unconditionally from `params`.
        //
        // `replace_additional_params`, not `merge_`: a `/reasoning` override
        // must not leave a stale key behind, and anthropic's `thinking` block is
        // the case that matters — merging would carry the old `budget_tokens`
        // alongside a new `type`. Recomposing through `request_additional_params`
        // rather than patching the baked value keeps an override turn
        // byte-identical to a normal one, and the `without_*` arms mean an
        // explicit `None` clears instead of silently keeping the built value.
        //
        // With no overrides `params` *is* `live_params`, so this recomputes what
        // was baked at build time and the turn is unchanged.
        let request = self.agent.stream_prompt(input);
        let request = match params.temperature {
            Some(t) => request.temperature(t),
            None => request.without_temperature(),
        };
        let request = match request_additional_params(&self.provider, &self.model, params) {
            Some(v) => request.replace_additional_params(v),
            None => request.without_additional_params(),
        };
        let mut stream = request
            .add_hook(LoggingHook {
                content: self.content.clone(),
                user_cancel: self.user_cancel.clone(),
                message_logger: self.message_logger.clone(),
                reasoning_in_context: self.reasoning_in_context,
                provider: self.provider.clone(),
                model: self.model.clone(),
                transcript: transcript.clone(),
            })
            .max_turns(self.tool_call_limit)
            // Allow one in-turn recovery for the model calling a tool that
            // isn't registered or is not in the current turn's allowed set.
            // The hook (`LoggingHook::on_invalid_tool_call`) decides the
            // recovery action; this is the *budget* the rig-run policy
            // honors when the hook returns `Retry { feedback }`. Consumes
            // one `max_turns` slot on use. See doc/tool_error_handling.md
            // §Layer 1 Task 2.
            .max_invalid_tool_call_retries(1)
            .history(prior)
            .await;

        let mut output = String::new();
        let mut reasoning_buffer = String::new(); // Accumulate reasoning for aborted/errored turns
        let mut final_history: Option<Vec<Message>> = None;
        // Captured when the run is cancelled via `<Esc>`; rig's error carries the
        // in-flight *full* history (prior turns + this turn's user prompt + any
        // partial assistant text). Persisting it is root cause #2's fix.
        let mut cancelled_history: Option<Vec<Message>> = None;
        let mut aborted = false;
        // The transient status line (doc/row_accounting.md zone 2) is set before
        // the turn's first content write, because that is where the output
        // paints it — after each block, for the whole turn. The thinking level
        // is not in it: the agent carries none (it went with the hold-back in
        // B3b) and it only changes on an explicit commit, so it belongs to the
        // prompt-side carrier.
        self.content.begin_response();
        // U4.3: the turn's second pump, split out of `in_turn` into bindings the
        // two new futures below can borrow *disjointly* — one future per
        // binding, so neither is borrowed twice at once. `tokio::select!`
        // scopes its futures to their own block ("a scope to separate polling
        // from handling the output"), so by the time a handler runs these are
        // free again for the handler to take.
        let (mut it_keys, mut it_confirms, mut it_session, it_label, mut it_history) = match in_turn
        {
            Some(crate::repl::InTurnInput {
                session,
                keys,
                confirms,
                label,
                history,
            }) => (
                Some(keys),
                Some(confirms),
                Some(session),
                label,
                Some(history),
            ),
            None => (None, None, None, "", None),
        };
        // The area has to say the turn started *now*. `begin_response` above
        // flipped `streaming`, and the frame on screen was composed while it was
        // false — so until some keystroke happened to compose again, the input
        // row stayed drawn and the status dot stayed grey for the whole turn.
        //
        // One `settle` at the switch, and it is the same paint path both pumps
        // use rather than a second way to compose: the buffer was just emptied
        // by the submit, so its `eval` is a passive derive with nothing to
        // dispatch and the action it returns is empty by construction.
        if let (Some(sess), Some(hist)) = (it_session.as_deref_mut(), it_history.as_deref_mut()) {
            let _ = sess.settle(self.content.as_ref(), it_label, hist).await;
        }
        loop {
            // Prose's wake-up for this iteration, resolved *before* the select
            // rather than inside the timer's async block: `Option<Duration>` is
            // `Copy`, so the future below needs nothing from the session and
            // borrows nothing — one fewer thing held across every await in
            // here. Computed per iteration, which is as fresh as computing it
            // in the block would be, since the block is rebuilt each pass.
            let prose_deadline = it_session
                .as_deref()
                .and_then(crate::repl::PromptSession::prose_timeout);
            tokio::select! {
                item = stream.next() => {
                    match item {
                        Some(Ok(MultiTurnStreamItem::StreamAssistantItem(content))) => match content {
                            StreamedAssistantContent::Text(text) => {
                                let delta = text.text();
                                output.push_str(delta);
                                self.content.stream_text(delta);
                            }
                            StreamedAssistantContent::Reasoning { reasoning, .. } => {
                                let t = reasoning_text(&reasoning);
                                if !t.is_empty() {
                                    reasoning_buffer.push_str(&t); // Accumulate for partial persistence
                                    self.content.stream_thinking(&t);
                                }
                            }
                            StreamedAssistantContent::ReasoningDelta { reasoning, .. }
                                if !reasoning.is_empty() => {
                                    reasoning_buffer.push_str(&reasoning); // Accumulate for partial persistence
                                    self.content.stream_thinking(&reasoning);
                                }
                            _ => {}
                        },
                        Some(Ok(MultiTurnStreamItem::StreamUserItem(
                            StreamedUserContent::ToolResult {
                                tool_result, ..
                            },
                        ))) => {
                            // Tool-result stream items are the canonical
                            // `Message::User` content rig committed to its own
                            // `new_messages`. Accumulate them (batched by
                            // `TurnTranscript` into one user message per
                            // completed batch) so a mid-turn failure can
                            // persist the completed tool work.
                            if let Ok(mut t) = transcript.lock() {
                                t.push_tool_result(tool_result);
                            }
                        }
                        Some(Ok(MultiTurnStreamItem::CompletionCall(call))) => {
                            // Plumb per-request usage out of the stream (compaction's
                            // prerequisite — doc/compaction.md §2.7). The last call's
                            // `input_tokens` is the current context size; the sum across
                            // calls approximates session cost. Zero-valued usage is rig's
                            // sentinel for "provider reported nothing" (e.g. ollama), so
                            // skip it to keep the telemetry honest.
                            let u = call.usage;
                            if u.has_values() {
                                if let Ok(mut g) = self.usage.lock() {
                                    g.last_input_tokens = u.input_tokens;
                                    g.last_output_tokens = u.output_tokens;
                                    g.last_cached_input_tokens = u.cached_input_tokens;
                                    g.last_cache_creation_input_tokens = u.cache_creation_input_tokens;
                                    g.session_input_tokens += u.input_tokens;
                                    g.session_output_tokens += u.output_tokens;
                                    g.session_cached_input_tokens += u.cached_input_tokens;
                                    g.session_cache_creation_input_tokens += u.cache_creation_input_tokens;
                                    g.session_reasoning_tokens += u.reasoning_tokens;
                                }
                            }
                            // Commit the text so far before the tool call / result line.
                            self.content.end_response_segment();
                        }
                        Some(Ok(MultiTurnStreamItem::FinalResponse(resp))) => {
                            final_history = resp.messages().map(|h| h.to_vec());
                        }
                        Some(Ok(MultiTurnStreamItem::ToolExecutionCommitted { .. })) => {}
                        Some(Ok(MultiTurnStreamItem::ModelTurnRetried { .. })) => {}
                        Some(Err(e)) => {
                            // `<Esc>` (or any hook Stop/Terminate) terminates the rig run
                            // as a terminal `PromptCancelled` error that carries the
                            // in-flight full history (`prior + new_messages`). We do NOT
                            // persist that full history because (a) prior messages are
                            // already in the session — appending them again duplicates
                            // them, and (b) `new_messages` may end with an assistant
                            // message containing `tool_calls` without matching tool
                            // results (cancellation happened between model response and
                            // tool execution). Persisting such an incomplete turn would
                            // make the next LLM call fail with "assistant message with
                            // 'tool_calls' must be followed by tool messages".
                            //
                            // Instead, persist the user prompt and any partial
                            // assistant content (text + reasoning) that was streamed
                            // before the cancellation. Tool calls are not accumulated
                            // in `output` (they arrive as separate stream events), so
                            // the partial assistant message is safe for the next turn's
                            // context. When reasoning is enabled, include it so the
                            // model can resume its train of thought on a steering
                            // message (controlled by `reasoning_in_context` mode).
                            if let StreamingError::Prompt(prompt_err) = &e {
                                if let PromptError::PromptCancelled { .. } = prompt_err.as_ref() {
                                    // Cancelled mid-run (Esc or a hook Stop).
                                    // Persist the user prompt plus whatever
                                    // completed: the finalized transcript
                                    // (accepted turns + completed tool batches
                                    // + synthesized answers for interrupted
                                    // calls), or the streamed content when no
                                    // turn completed yet. The leading
                                    // `user(input)` is included only if this
                                    // turn's fresh dispatch isn't already in
                                    // the session (count vs. `turn.sent()`); a
                                    // resend whose prompt is already persisted
                                    // is skipped so we never duplicate it.
                                    let mut partial: Vec<Message> = Vec::new();
                                    let session_has_prompt = match session {
                                        Some(s) => {
                                            let c = s.count_persisted_user_prompts().await;
                                            turn.this_turn_persisted(c) >= turn.sent()
                                        }
                                        None => false,
                                    };
                                    if !session_has_prompt {
                                        partial.push(Message::user(input));
                                    }
                                    {
                                        let mut t = transcript
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                                        let has_progress = t.has_progress();
                                        if has_progress {
                                            partial.extend(t.finalize());
                                        }
                                        // Always include current turn's partial content (text + reasoning)
                                        // even when there was prior progress. This captures reasoning
                                        // from the current incomplete turn (e.g., after a tool call).
                                        if !output.is_empty() || !reasoning_buffer.is_empty() {
                                            let mut content = Vec::new();
                                            if !output.is_empty() {
                                                content.push(AssistantContent::Text(Text::new(std::mem::take(&mut output))));
                                            }
                                            if !reasoning_buffer.is_empty() {
                                                let reasoning_text = std::mem::take(&mut reasoning_buffer);
                                                content.push(AssistantContent::Reasoning(Reasoning::new(&reasoning_text)));
                                            }
                                            partial.push(Message::Assistant { id: None, content }); // id:None marks aborted turn
                                        }
                                    }
                                    cancelled_history = Some(partial);
                                    aborted = true;
                                    break;
                                }
                            }
                            // Phase 2a: preserve user prompt on non-cancellation
                            // errors. Commit the partial turn so the next prompt or
                            // steering message sees the user's input and any partial
                            // assistant text, rather than having it silently dropped.
                            // See doc/pi_steering_rig.md §6.5.3.
                            //
                            // EXCEPTION (auto-retryable errors): when the classified
                            // error is one the REPL will auto-retry (429, transport,
                            // upstream 5xx, stream protocol, malformed tool call),
                            // the SAME logical turn is re-attempted — the retry calls
                            // `prompt()` again with the same input, and rig re-sends
                            // the full context history for the turn (the same way it
                            // does for a tool-result continuation). Naively
                            // pre-persisting `user(input)` here would therefore
                            // duplicate the user's message in the session on every
                            // retry.
                            //
                            // De-dup strategy: the per-turn `TurnPersistence`
                            // record is the single source of truth for "was
                            // `user(input)` already written to the session
                            // within THIS turn, and with what text?" The REPL
                            // resets it (`TurnPersistence::new()`) at the start
                            // of every user prompt and reuses it across
                            // retries. The first failure of a turn persists
                            // `user(input)` and records the text; later
                            // failures that repeat the SAME text (and the
                            // cancel branch) skip. A retry that switches to a
                            // DIFFERENT prompt text (the steering message)
                            // persists that new text too. On success, `prompt()`
                            // drops the leading `user(input)` from
                            // `final_history` when the recorded text matches.
                            //
                            // This is strictly SAFER than deduping by comparing
                            // against the session's last user message: two
                            // successive turns with an IDENTICAL input (e.g.
                            // the user types "continue" twice) must NOT
                            // collide. A session-level comparison cannot tell
                            // "same turn, retry" from "different turn, same
                            // text", and would wrongly suppress the second
                            // turn's user prompt. The in-memory record is
                            // reset per turn, so it never conflates them.
                            //
                            //   * retryable-no-progress (the lost-context bug):
                            //     `user(input)` is persisted on the FIRST failure,
                            //     so a give-up after 50 retries still leaves the
                            //     turn visible in the session.
                            //   * retryable-with-progress: same — `user(input)` is
                            //     persisted if not already there.
                            //   * non-retryable: same as the original Phase 2a.
                            //   * retry-then-success: the success branch drops the
                            //     duplicate leading `user(input)` from
                            //     `final_history`.
                            let classified = classify_stream_error(&e);
                            let retryable = classified.is_auto_retryable();
                            // Finalize what completed (accepted turns + tool
                            // batches; never truncates). `has_accepted` is
                            // computed independently of the session: it drives
                            // the consecutive-failure back-off reset in the
                            // REPL, so a mid-turn failure *after* progress is
                            // not treated as a "consecutive failure" even when
                            // there is no session to persist to.
                            let (transcript_msgs, has_accepted) = {
                                let mut t = transcript
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                let has_accepted = t.has_progress();
                                let msgs = if has_accepted { t.finalize() } else { Vec::new() };
                                (msgs, has_accepted)
                            };
                            let made_progress = has_accepted || !output.is_empty();
                            // Persist completed work into the session when one
                            // exists. Non-retryable errors always persist (so
                            // the user's turn is never lost on give-up);
                            // retryable errors persist only when progress
                            // exists, to avoid duplicating the user's input on
                            // a blind retry.
                            if let Some(s) = session {
                                // Persist-decision (session is the source of
                                // truth): `this_turn_persisted` is how many of
                                // this turn's *new* dispatches are recorded in
                                // the session (current count minus the base
                                // frame captured at turn start). If it is
                                // LOWER than `turn.sent()`, the fresh dispatch
                                // for this turn is not yet persisted (first
                                // failure, or a steering dispatch whose prompt
                                // hasn't landed) — persist `user(input)` now.
                                // If it is already EQUAL, the prompt is
                                // persisted (this is a resend of a prior
                                // attempt) — skip.
                                let session_user_count =
                                    s.count_persisted_user_prompts().await;
                                let should_persist_input =
                                    turn.this_turn_persisted(session_user_count)
                                        < turn.sent();
                                let should_persist_progress =
                                    !retryable || has_accepted || !output.is_empty();
                                if should_persist_input {
                                    let _ = s.append_message(&Message::user(input)).await;
                                }
                                if should_persist_progress {
                                    if has_accepted {
                                        for m in &transcript_msgs {
                                            let _ = s.append_message(m).await;
                                        }
                                    } else if !output.is_empty() || !reasoning_buffer.is_empty() {
                                        let mut content = Vec::new();
                                        if !output.is_empty() {
                                            content.push(AssistantContent::Text(Text::new(std::mem::take(&mut output))));
                                        }
                                        if !reasoning_buffer.is_empty() {
                                            let reasoning_text = std::mem::take(&mut reasoning_buffer);
                                            content.push(AssistantContent::Reasoning(Reasoning::new(&reasoning_text)));
                                        }
                                        let _ = s
                                            .append_message(&Message::Assistant { id: None, content })
                                            .await;
                                    }
                                }
                            }
                            self.content.end_response();
                            // A retryable error that interrupted the run AFTER
                            // progress: signal the REPL (via `LlmInterrupted`)
                            // so it resets the back-off and continues from the
                            // partial transcript with a steering message. Gated
                            // on *progress*, not on persistence, so the
                            // back-off resets even without a session.
                            if retryable && made_progress {
                                return Err(crate::PicocodeError::LlmInterrupted {
                                    cause: Box::new(classified),
                                });
                            }
                            return Err(classified);
                        }
                        None => break,
                    }
                }
                _ = self.cancel_notify.notified() => {
                    if self.user_cancel.load(Ordering::SeqCst) {
                        // User pressed `<Esc>` — stop rendering and return control
                        // to the REPL immediately. The stream is dropped, which
                        // closes the HTTP connection; any running tool was already
                        // killed by the watchdog (Phase 1a-d tool-cancel channel).
                        //
                        // Capture partial turn for persistence: user prompt +
                        // finalized transcript (or streamed text + reasoning when no
                        // turn completed).
                        let mut partial = vec![Message::user(input)];
                        {
                            let mut t = transcript
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            let has_progress = t.has_progress();
                            if has_progress {
                                partial.extend(t.finalize());
                            }
                            // Always include current turn's partial content (text + reasoning)
                            // even when there was prior progress. This captures reasoning
                            // from the current incomplete turn (e.g., after a tool call).
                            if !output.is_empty() || !reasoning_buffer.is_empty() {
                                let mut content = Vec::new();
                                if !output.is_empty() {
                                    content.push(AssistantContent::Text(Text::new(std::mem::take(&mut output))));
                                }
                                if !reasoning_buffer.is_empty() {
                                    let reasoning_text = std::mem::take(&mut reasoning_buffer);
                                    content.push(AssistantContent::Reasoning(Reasoning::new(&reasoning_text)));
                                }
                                partial.push(Message::Assistant { id: None, content }); // id:None marks aborted turn
                            }
                        }
                        cancelled_history = Some(partial);
                        aborted = true;
                        break;
                    }
                }

                // A gated tool call parked on the port. The tool future is
                // inside the stream arm above, so this request is what makes
                // the arm go `Pending` while the loop stays free to serve keys
                // (doc/confirm_as_menu_261002.md). The session holds the
                // dialog as an overlay element; the tool waits on the reply.
                //
                // Only here, not in the back-off pump: a confirm blocks its
                // own tool, so by the time a retry back-off runs the dialog is
                // resolved and the tool has returned.
                Some(req) = async {
                    match it_confirms.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(sess) = it_session.as_deref_mut() {
                        sess.open_confirm(req, self.confirm_approvals.clone());
                        sess.sync_stack(self.content.as_ref(), it_label);
                        crate::repl::paint_prompt(self.content.as_ref());
                    }
                }

                // U4.3: a key typed during the turn, evaluated as it arrives —
                // no queue, no holding back (ui_draw_v2 §4.5). This is
                // `read_input`'s loop minus its wait: dispatch, then settle.
                // `route_event` takes `Ctrl-C`/`Ctrl-Z` inline before anything
                // reaches this channel, so what lands here is ordinary editing:
                // a submit (blocked, silently) and an `Esc` whose menu-first
                // call the shared dispatcher makes. One dispatcher, two pumps:
                // the back-off between attempts (`cancellable_sleep_pumped`)
                // runs the same body over the same queue, so a key means the
                // same thing while the turn streams and while it waits out a
                // retry delay.
                ev = async {
                    match it_keys.as_mut() {
                        Some(rx) => rx.recv().await,
                        // No in-flight reader (piped input): nothing can ever
                        // arrive, so the branch never completes instead of
                        // completing with `None` every time round.
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = dispatch_in_turn_event(
                        ev,
                        it_session.as_deref_mut(),
                        it_history.as_deref_mut(),
                        it_label,
                        self.content.as_ref(),
                        &self.user_cancel,
                        &self.cancel_notify,
                    )
                    .await;
                }

                // Prose needs a deadline even while the stream runs: its
                // debounce, its in-flight slices and its dialog all fire on it.
                // Without this arm prose would go inert for the whole turn —
                // the stream branch alone never yields for them — which is the
                // same reason `read_input`'s wait is a *poll* while prose is
                // armed and a block otherwise.
                _ = async {
                    match prose_deadline {
                        Some(d) => tokio::time::sleep(d).await,
                        // `None`: nothing pending, and no timer to wake for.
                        None => std::future::pending().await,
                    }
                } => {
                    if let (Some(sess), Some(history)) =
                        (it_session.as_deref_mut(), it_history.as_deref_mut())
                    {
                        let _ = sess
                            .settle(self.content.as_ref(), it_label, history)
                            .await;
                    }
                }
            }
        }
        self.content.end_response();

        // Persist the turn into the session (or fallback to no persistence).
        if let Some(s) = session {
            // Store pre-strip messages (strip is applied to LLM context above,
            // not at storage time — see doc/session_storage.md §4).
            if let Some(fh) = final_history {
                let mut to_commit = normalize_call_ids(fh);
                // De-dup against the session (persistence source of truth).
                // When the session already holds this turn's fresh dispatch
                // prompt (`session_user_count >= turn.sent()`, e.g. a prior
                // failed attempt pre-persisted it), `final_history[0]` is a
                // re-sent duplicate of that same prompt — drop it so the user
                // prompt appears exactly once. When the session count is
                // LOWER (the fresh dispatch's prompt isn't persisted yet — a
                // genuinely new user message/stear directive), keep
                // `final_history[0]` so it is committed.
                let session_user_count = s.count_persisted_user_prompts().await;
                if turn.this_turn_persisted(session_user_count) >= turn.sent() {
                    // Only drop when the leading message is this turn's user
                    // prompt (a plain text User message), never a tool-result
                    // batch or an assistant message.
                    if matches!(to_commit.first(), Some(Message::User { content }) if content.iter().any(|c| matches!(c, UserContent::Text(_))))
                    {
                        to_commit.remove(0);
                    }
                }
                for msg in to_commit {
                    let _ = s.append_message(&msg).await;
                }
            } else if let Some(ch) = cancelled_history {
                let to_commit = normalize_call_ids(ch);
                for msg in to_commit {
                    let _ = s.append_message(&msg).await;
                }
            }
        }

        if aborted {
            return Err(crate::PicocodeError::Cancelled("Aborted.".into()));
        }

        Ok(output)
    }

    pub async fn run_once(&self, input: String) -> Result<String> {
        self.content.display_header(
            &self.provider,
            &self.model,
            self.yolo,
            self.tool_call_limit,
            self.persona_name.as_deref(),
        );
        self.content.display_separator();
        // `prompt` now renders the response itself via the streaming Output
        // methods, so there is no separate `display_markdown` here.
        let turn = TurnPersistence::new();
        // run_once is sessionless but still a NEW user dispatch; mark it so
        // `turn.sent() == 1` and the success path does not mis-deduplicate
        // `final_history[0]` against an absent session.
        turn.mark_new_dispatch();
        // No override surface on this path, so the built live params.
        let response = self
            .prompt(&input, None, &turn, &self.live_params, None)
            .await?;
        Ok(response)
    }

    /// Generate a concise summary of the given text using the agent's model.
    /// Used by `/tree --summarize` to summarise abandoned branches.
    pub async fn summarize(&self, prompt: &str) -> Result<String> {
        self.agent
            .prompt(Message::user(prompt))
            .max_turns(SUMMARIZATION_MAX_TURNS)
            .await
            .map_err(|e| classify_prompt_error(&e, "summarize"))
    }

    /// Run a one-shot summarization whose **request context is the live
    /// conversation prefix** at the session's current leaf, plus
    /// `instruction` as the final user message.
    ///
    /// This is the cache-preserving compaction request shape: the leading
    /// history is built by exactly the same `Session::build_context()` (plus
    /// the same `maybe_strip_reasoning` pass) that a normal user prompt at
    /// that position would use, so the provider's prefix cache — warmed by
    /// the live turns — hits for the whole conversation span. Only the
    /// summarization instruction itself is a fresh (cache-write) tail.
    ///
    /// The session's leaf is READ but never MODIFIED here: callers that need
    /// to summarize a *prefix* of the conversation move the leaf to the
    /// desired position first (the `/tree <id> compact llm` leaf dance) and
    /// restore it on every return path afterwards.
    pub async fn summarize_with_session_context(
        &self,
        session: &Session,
        instruction: &str,
    ) -> Result<String> {
        // Leading history = the live conversation prefix at the current leaf,
        // built by the SAME helper a normal user prompt uses
        // (`session_context_messages` — `Session::build_context()` +
        // `maybe_strip_reasoning`), so this request shares the provider's
        // prefix cache (warmed by live turns) for the whole span.
        // `Session::build_context()` applies the compaction transforms, so a
        // prior compaction active at this position emits its summary as the
        // leading `<summary>` wrapper — implicit incremental-compaction
        // semantics from the tree structure alone.
        //
        // The leaf is READ but never MODIFIED here: callers that need to
        // summarize a *prefix* of the conversation move the leaf to the
        // desired position first (the `/tree <id> compact llm` leaf dance)
        // and restore it on every return path afterwards.
        let prior = session_context_messages(
            session,
            self.reasoning_in_context,
            &self.provider,
            &self.model,
        )
        .await?;

        self.agent
            .prompt(Message::user(instruction))
            .history(prior)
            .max_turns(SUMMARIZATION_MAX_TURNS)
            .await
            .map_err(|e| classify_prompt_error(&e, "summarize"))
    }
}

/// Model-call budget for a summarization / compaction request.
///
/// A summarizer is not a single-shot request in practice: the agent
/// carries the full tool set, so a summary that has to check a file,
/// re-read a span, or recover from a hallucinated call costs more than
/// one model call — and one call is all rig allows implicitly (an
/// unconfigured agent's budget), which fails the request outright on the
/// first tool call. Ten leaves room for that lookup-and-answer shape (a
/// tool call plus the model-authored summary) without letting a
/// summarizer wander.
///
/// Deliberately *not* `self.tool_call_limit`: that is the interactive
/// turn budget, and a summary is a background, user-initiated operation
/// whose cost is bounded by what the user asked for, not by the budget
/// they set for a coding turn.
const SUMMARIZATION_MAX_TURNS: usize = 10;
