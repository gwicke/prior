use crate::app_state::AppState;
use crate::session::repo::{ForkMode, SessionRepo};
use crate::session::storage::SessionStorage;
use crate::session::Session;
use crate::Output;
use crate::Result;
use build::{request_additional_params, LoggingHook};

use futures::StreamExt;
use rig_agent::agent::{Agent, MultiTurnStreamItem, StreamingError};
use rig_agent::completion::{Chat, PromptError};
use rig_agent::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingPrompt};

use rig_core::message::{
    AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolResultContent, UserContent,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

pub(crate) mod build;
pub(crate) mod compaction;
pub(crate) mod errors;
pub(crate) mod msglog;
pub(crate) mod pumps;
pub(crate) mod replay;
pub(crate) mod status;
pub(crate) mod transcript;

use compaction::session_context_messages;
use errors::{classify_prompt_error, classify_stream_error};
#[cfg(test)]
use msglog::message_role;
use msglog::MessageLogger;
use pumps::dispatch_in_turn_event;
use replay::{build_replay_units, count_turns, replay_last_turns, replay_units};
use status::{first_user_text, status_text, BarModel};
use transcript::{maybe_strip_reasoning, normalize_call_ids, reasoning_text, TurnTranscript};

/// Token-usage telemetry accumulated by an agent across a session.
///
/// The `last_*` fields reflect the most recent completion request and back the
/// "current context size" reported by `/stat`; the `session_*` fields are the
/// running sum across every completion call (each call re-sends the full
/// history, so this approximates the billed token cost). A zeroed field means
/// the provider reported no value (rig's `Usage` sentinel for missing metrics).
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionUsage {
    /// Input ("prompt") tokens of the most recent completion request — the
    /// size of the context that would be re-sent on the next turn.
    pub last_input_tokens: u64,
    /// Output ("completion") tokens of the most recent completion request.
    pub last_output_tokens: u64,
    /// Cache-read tokens of the most recent completion request.
    pub last_cached_input_tokens: u64,
    /// Cache-write (creation) tokens of the most recent completion request.
    pub last_cache_creation_input_tokens: u64,
    /// Cumulative input tokens across the session.
    pub session_input_tokens: u64,
    /// Cumulative output tokens across the session.
    pub session_output_tokens: u64,
    /// Cumulative cache-read tokens across the session.
    pub session_cached_input_tokens: u64,
    /// Cumulative cache-write tokens across the session.
    pub session_cache_creation_input_tokens: u64,
    /// Cumulative reasoning tokens across the session.
    pub session_reasoning_tokens: u64,
}

/// Per-turn persistence state threaded across `prompt()` retries inside one
/// REPL turn. The REPL owns one instance per user prompt and reuses it across
/// retries.
///
/// It tracks two quantities:
///
/// * `turns_sent` — how many **new user messages** have been dispatched this
///   turn (the user's original prompt, plus each steering/continuation
///   directive the REPL introduces). A plain retry that re-sends the same
///   prompt (unchanged `next_input`) does NOT increment it.
/// * `base_session_count` — the number of user prompts already in the session
///   at the moment this turn's FIRST `prompt()` ran. Captured lazily and once
///   (`initialize_base`), so retries of the same turn keep the original
///   frame.
///
/// The persistence decision is made IN `prompt()` by reading the **session**
/// (the persistence source of truth) and answering the question:
///
/// `this_turn_persisted = current_session_count − base`
/// * if `this_turn_persisted < turns_sent` → this turn's latest dispatch is
///   not yet persisted → persist `user(input)` now (first failure, or a
///   steering dispatch whose prompt hasn't landed yet).
/// * if `this_turn_persisted >= turns_sent` → it's already persisted (a
///   resend of a prior attempt) → skip, and on success drop the leading
///   `user(input)` from `final_history`.
///
/// Comparing the session-derived *delta* (base-subtracted) against the
/// per-turn `turns_sent` is what makes this correct across turn boundaries:
///
/// * **Identical inputs across turns** (e.g. "continue" twice): turn 2's
///   base is the session count AFTER turn 1 (which committed "continue"), so
///   turn 2's own dispatch (sent = 1) still has `this_turn_persisted = 0` and
///   is persisted as a fresh message. No text comparison involved.
/// * **"continue" → "continue" within a turn**: the steering dispatch
///   increments `turns_sent`, so it is never elided against the original.
/// * **REPL/session drift**: the session is authoritative; the decision
///   reconciles to what is really persisted.
#[derive(Debug, Default)]
pub struct TurnPersistence {
    turns_sent: std::sync::Mutex<u64>,
    /// Session user-prompt count at this turn's first `prompt()` entry.
    /// `None` until [`Self::initialize_base`] runs once.
    base_session_count: std::sync::Mutex<Option<u64>>,
}

impl TurnPersistence {
    /// Start a fresh logical turn. The REPL calls this once per user prompt
    /// submission, NOT per retry. Subsequent retries reuse it.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many *new* user messages have been dispatched in this turn so far.
    pub fn sent(&self) -> u64 {
        *self
            .turns_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record that a NEW user message (the user's prompt, or a steering/
    /// continuation directive) is being dispatched. The REPL calls this when
    /// it sends a fresh message — NOT on a plain retry that re-sends an
    /// identical prompt.
    pub fn mark_new_dispatch(&self) {
        *self
            .turns_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
    }

    /// Capture the turn's base frame: the number of user prompts already in
    /// the session when the turn's first `prompt()` ran. Only the first call
    /// sets it; retries keep the original frame. A `None` session means 0.
    pub async fn initialize_base(&self, session: Option<&Session>) {
        {
            let base = self
                .base_session_count
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if base.is_some() {
                return;
            }
        } // guard dropped before the await
        let count: u64 = match session {
            Some(s) => s.count_persisted_user_prompts().await as u64,
            None => 0,
        };
        *self
            .base_session_count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(count);
    }

    /// How many of this turn's dispatches are actually persisted in the
    /// session right now: `current_session_count − base`. Saturates at 0.
    pub fn this_turn_persisted(&self, current_session_count: usize) -> u64 {
        let base = self
            .base_session_count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unwrap_or(0);
        (current_session_count as u64).saturating_sub(base)
    }
}

impl CodeAgent {
    /// `params` are the params to send *for this request*: the live ones, or
    /// the live ones with this turn's per-turn overrides applied. Passing them
    /// in rather than reading them off `AppState` is what makes an override
    /// per-request — nothing here mutates the live params, so there is nothing
    /// to restore and no path by which one leaks into the next turn.
    ///
    /// **This future is `!Send`; the summarizers below are not.** Since U4.3
    /// the turn carries the REPL's prompt session in, and that session is
    /// single-threaded by construction — `Rc` line windows, `Rc` menu views, a
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
        // The stand-in label for a session with no name: its opening user
        // message. `prior` holds everything before this turn, so from the second
        // turn onward that *is* the first message; on the first turn it is empty
        // and this turn's own prompt is. Captured here, before `prior` moves
        // into the agent.
        let first_message: Option<String> = first_user_text(&prior)
            .or_else(|| (!input.trim().is_empty()).then_some(input))
            .map(str::to_string);
        // The same shape, for the ctx figure: the first turn has no reported
        // usage at all (the first completion call is still in flight), so the
        // status line would have no context number for exactly the turn a new
        // session is most likely to be watched. The estimate `/tree compact`
        // already uses is computable from the messages in hand — `prior` plus
        // this turn's prompt — so use it, and let the provider's own count
        // replace it from the second turn on.
        let estimated_ctx = estimate_ctx_with(&prior, input);

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
                output: self.output.clone(),
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
        // paints it — after each block, for the whole turn. The context figure
        // is the *previous* turn's, which is the best available at this point
        // and honest about being one turn behind; the next turn's start
        // refreshes it. The thinking level is not in it: the agent carries none
        // (it went with the hold-back in B3b) and it only changes on an
        // explicit commit, so it belongs to the prompt-side carrier.
        let session_name = session.and_then(|s| {
            let index = s.index.try_lock().ok()?;
            index.header.name.clone()
        });
        let reported_ctx = self.usage.lock().unwrap().last_input_tokens;
        self.output.set_status(&status_text(
            session_name.as_deref(),
            first_message.as_deref(),
            BarModel {
                provider: &self.provider,
                model: &self.model,
                // The status bar's own field is upstream's `thinking`; the
                // value it reads is now the renamed reasoning level.
                thinking: self.live_params.reasoning_str().as_deref(),
                context_limit: self.context_limit,
                // Reported wins: it is the provider's own count of what it was
                // sent. The estimate only stands in while there is none — turn
                // one, where the alternative is no figure at all.
                context_tokens: if reported_ctx > 0 {
                    reported_ctx
                } else {
                    estimated_ctx
                },
            },
        ));
        self.output.begin_response();
        // U4.3: the turn's second pump, split out of `in_turn` into bindings the
        // two new futures below can borrow *disjointly* — one future per
        // binding, so neither is borrowed twice at once. `tokio::select!`
        // scopes its futures to their own block ("a scope to separate polling
        // from handling the output"), so by the time a handler runs these are
        // free again for the handler to take.
        let (mut it_keys, mut it_confirms, mut it_session, it_label, mut it_history, it_guard) =
            match in_turn {
                Some(crate::repl::InTurnInput {
                    session,
                    keys,
                    confirms,
                    label,
                    history,
                    guard,
                }) => (
                    Some(keys),
                    Some(confirms),
                    Some(session),
                    label,
                    Some(history),
                    Some(guard),
                ),
                None => (None, None, None, "", None, None),
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
            let _ = sess
                .settle(
                    self.output.as_ref(),
                    it_label,
                    hist,
                    self.output.frame_size(),
                )
                .await;
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
                                self.output.stream_text(delta);
                            }
                            StreamedAssistantContent::Reasoning { reasoning, .. } => {
                                let t = reasoning_text(&reasoning);
                                if !t.is_empty() {
                                    reasoning_buffer.push_str(&t); // Accumulate for partial persistence
                                    self.output.stream_thinking(&t);
                                }
                            }
                            StreamedAssistantContent::ReasoningDelta { reasoning, .. }
                                if !reasoning.is_empty() => {
                                    reasoning_buffer.push_str(&reasoning); // Accumulate for partial persistence
                                    self.output.stream_thinking(&reasoning);
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
                            self.output.end_response_segment();
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
                            self.output.end_response();
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
                        sess.paint(self.output.as_ref(), it_label, self.output.frame_size());
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
                        it_guard,
                        self.output.as_ref(),
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
                        let dims = self.output.frame_size();
                        let _ = sess
                            .settle(self.output.as_ref(), it_label, history, dims)
                            .await;
                    }
                }
            }
        }
        self.output.end_response();

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
        self.output.display_header(
            &self.provider,
            &self.model,
            self.yolo,
            self.tool_call_limit,
            self.persona_name.as_deref(),
        );
        self.output.display_separator();
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

    /// Snapshot of the accumulated session token usage (for `/stat`). A zeroed
    /// result means no completion call has reported usage yet.
    pub fn session_usage(&self) -> SessionUsage {
        self.usage.lock().map(|g| *g).unwrap_or_default()
    }

    /// Generate a concise summary of the given text using the agent's model.
    /// Used by `/tree --summarize` to summarise abandoned branches.
    pub async fn summarize(&self, prompt: &str) -> Result<String> {
        let msg = Message::user(prompt);
        let mut history: Vec<Message> = vec![];
        self.agent
            .chat(msg, &mut history)
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
            .chat(Message::user(instruction), &mut prior.clone())
            .await
            .map_err(|e| classify_prompt_error(&e, "summarize"))
    }
}

pub struct CodeAgent {
    agent: Agent,
    output: Arc<dyn Output>,
    tool_call_limit: usize,
    provider: String,
    model: String,
    /// The active reasoning level, for the status readout. A snapshot taken when
    /// the agent is built: the level only changes on an explicit commit, and the
    /// bar is painted once per content block, so reading the config per block
    /// would only add a lock to a path that has no reason to be fresh.
    /// The live params this agent was built with: the default for a turn with
    /// no per-turn override, and the status bar's reasoning level.
    live_params: crate::params::ModelParams,
    /// The resolved context window, so the per-turn status bar divides by the
    /// number in force rather than looking one up per paint.
    context_limit: Option<u64>,
    yolo: bool,
    persona_name: Option<String>,
    user_cancel: Arc<AtomicBool>,
    /// Session tool approvals, the REPL's half of the confirm port.
    ///
    /// Held here so the turn can hand the *same* store to the dialog it opens
    /// for a gated call — an *always* answer is recorded by the REPL, and the
    /// guards read it back through the port (`Guard::already_approved`).
    /// Empty without a port (piped/batch), where nothing is ever approved.
    confirm_approvals: crate::confirm::Approvals,
    /// Event-based cancel signal. The turn's key dispatcher — the prompt's
    /// key branch, or the back-off pump between attempts — calls
    /// `notify_one()` when a root `<Esc>` is dispatched; the streaming loop
    /// selects on `notified()` instead of polling with a sleep.
    cancel_notify: Arc<tokio::sync::Notify>,
    reasoning_in_context: crate::settings::ReasoningInContextMode,
    /// Optional path to JSONL file for logging raw messages sent/received.
    /// Passed as `Some(Arc<MessageLogger>)` to `LoggingHook` if set.
    message_logger: Option<Arc<MessageLogger>>,
    /// Accumulated token-usage telemetry captured from the streaming run
    /// (see [`SessionUsage`]). Backs `/stat` and is the measurement primitive
    /// compaction will rely on (doc/compaction.md §2.7).
    usage: Mutex<SessionUsage>,
}

/// Build session context data for the REPL overlays.
///
/// Returns `(sessions list, branch message previews, session tree, leaf_id,
/// context_tokens)`.
pub(crate) async fn build_session_ctx(
    session: &Option<Arc<Session>>,
) -> (
    Vec<crate::repl::SessionMeta>,
    Vec<(String, String)>,
    std::sync::Arc<Vec<crate::session::SessionTreeNode>>,
    Option<String>,
    u64,
) {
    let sessions = Vec::new(); // loaded on demand by the `/session` overlay

    let branch_messages = if let Some(s) = session {
        let branch = s.get_branch().await;
        let mut msgs: Vec<(String, String)> = Vec::new();
        for entry in &branch {
            if let crate::session::SessionEntryContent::Message { message } = &entry.content {
                if message.get("role").and_then(|r| r.as_str()) == Some("user") {
                    let preview = message
                        .pointer("/content/0/text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(80)
                        .collect();
                    msgs.push((entry.id.clone(), preview));
                }
            }
        }
        msgs
    } else {
        Vec::new()
    };

    let (tree, leaf_id, context_tokens) = match session {
        Some(s) => {
            let tree = s.get_tree().await;
            let leaf_id = s.get_leaf_id().await;
            // Estimated tokens of the current (compaction-scoped) context —
            // shown in the `/tree compact` sub-menu header and written as
            // `tokensBefore` when the user compacts. Same call the compact
            // handler makes, so header and written value always agree.
            let context_tokens = s
                .build_context()
                .await
                .map(|c| estimate_context_tokens(&c.messages))
                .unwrap_or(0);
            (std::sync::Arc::new(tree), leaf_id, context_tokens)
        }
        None => (std::sync::Arc::new(Vec::new()), None, 0),
    };

    (sessions, branch_messages, tree, leaf_id, context_tokens)
}

/// Approximate token count for a context message list.
///
/// Rough deterministic heuristic (chars/4 + per-message framing overhead);
/// pi uses a real tokenizer. Good enough for the `[compaction: Nk tokens]`
/// display and the `tokensBefore` field — no correctness dependency.
/// The per-message framing overhead [`estimate_context_tokens`] charges each
/// message, named so [`estimate_ctx_with`] can account for the one message it
/// is adding.
const MESSAGE_FRAMING_TOKENS: u64 = 16;

/// Text tokens at [`estimate_context_tokens`]'s rate (chars/4), shared so the
/// one-message estimate below cannot drift from the whole-context one.
fn estimate_text_tokens(t: &str) -> u64 {
    t.chars().count() as u64 / 4
}

/// The estimated context for `prior` plus one more user message.
///
/// The status line's first-turn figure: no completion call has reported usage
/// yet, but the messages in hand *are* the whole context. Adding the two
/// figures rather than building a temporary `Vec` of `prior` plus a synthetic
/// message keeps a long context from being cloned once per turn — and keeps the
/// estimate identical to the one `/tree compact` reports, since both come from
/// the same arithmetic.
fn estimate_ctx_with(prior: &[Message], input: &str) -> u64 {
    let n = estimate_context_tokens(prior);
    if input.trim().is_empty() {
        return n;
    }
    n + MESSAGE_FRAMING_TOKENS + estimate_text_tokens(input)
}

pub(crate) fn estimate_context_tokens(messages: &[Message]) -> u64 {
    let text_tokens = estimate_text_tokens;
    let mut n = 0u64;
    for m in messages {
        n += MESSAGE_FRAMING_TOKENS; // per-message framing overhead (role, separators)
        match m {
            Message::User { content } => {
                for c in content.iter() {
                    match c {
                        UserContent::Text(t) => n += text_tokens(&t.text),
                        UserContent::ToolResult(tr) => {
                            for rc in tr.content.iter() {
                                if let ToolResultContent::Text(t) = rc {
                                    n += text_tokens(&t.text);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for c in content.iter() {
                    match c {
                        AssistantContent::Text(t) => n += text_tokens(&t.text),
                        AssistantContent::Reasoning(r) => {
                            for rc in r.content.iter() {
                                match rc {
                                    ReasoningContent::Text { text, .. } => n += text_tokens(text),
                                    ReasoningContent::Summary(s) => n += text_tokens(s),
                                    _ => {}
                                }
                            }
                        }
                        AssistantContent::ToolCall(tc) => {
                            n += text_tokens(&tc.function.name)
                                + text_tokens(&tc.function.arguments.to_string());
                        }
                        _ => {}
                    }
                }
            }
            Message::System { content } => n += text_tokens(content),
        }
    }
    n.max(1)
}

/// The canonical `provider:model` id a session has recorded, or `None`
/// when it records no model at all. Branch first (a `ModelChange` entry is
/// authoritative for mid-session `/model` switches), then the session
/// header (`with_model_context` sessions that never recorded an entry —
/// the header is written once, at first persistence).
async fn session_recorded_model_id(session: &Session) -> Option<String> {
    let branch = session.get_branch().await;
    if let Some((provider, model_id)) = crate::session::session::derive_model_change(&branch) {
        return Some(format!("{}:{}", provider, model_id));
    }
    let index = session.index.lock().await;
    match (&index.header.provider, &index.header.model_id) {
        (Some(provider), Some(model_id)) => Some(format!("{}:{}", provider, model_id)),
        _ => None,
    }
}

/// The reasoning level a session has recorded: latest `ThinkingLevelChange`
/// on the branch, else the header, else `None`.
async fn session_recorded_reasoning(session: &Session) -> Option<String> {
    let branch = session.get_branch().await;
    if let Some(level) = crate::session::session::derive_thinking_level(&branch) {
        return Some(level);
    }
    session.index.lock().await.header.thinking_level.clone()
}

/// Record an explicit `/model <id>` commit in the session — but only when
/// the session does not already record exactly this model.
///
/// For a session that has never been persisted (no file on disk yet) the
/// value is folded into the in-memory header instead of appended: metadata
/// alone must never create a session file (that is what littered the
/// `/session` listing with header+marker-only "empty" sessions). The
/// first real message persists the updated header.
pub(crate) async fn record_model_change(session: &Session, qualified_id: &str) {
    if session_recorded_model_id(session).await.as_deref() == Some(qualified_id) {
        return; // already recorded — no redundant entry
    }
    let Some((provider, model_id)) = qualified_id.split_once(':') else {
        return; // malformed id — refuse to record rather than persist junk
    };
    if !session.is_persisted() {
        let mut index = session.index.lock().await;
        index.header.provider = Some(provider.to_string());
        index.header.model_id = Some(model_id.to_string());
        return;
    }
    let _ = session.append_model_change(provider, model_id).await;
}

/// Record a thinking-level change — same rules as [`record_model_change`]:
/// skip when already recorded; fold into the in-memory header (no file)
/// while the session is still unpersisted.
pub(crate) async fn record_reasoning_change(session: &Session, level: &str) {
    if session_recorded_reasoning(session).await.as_deref() == Some(level) {
        return;
    }
    if !session.is_persisted() {
        session.index.lock().await.header.thinking_level = Some(level.to_string());
        return;
    }
    let _ = session.append_thinking_level_change(level).await;
}

/// Build the `/model <provider>:<model_id>` prefill to surface the model
/// the resumed session was last using — but only when it DIFFERS from the
/// model the agent is currently running (`live_model_id`, the canonical
/// `config.qualified_model_id()`): when they already match there is
/// nothing to accept and no nudge is due, and committing a matching
/// prefill would only churn the menu sync.
///
/// Looks at the active leaf→root branch first (a `ModelChange` entry is
/// authoritative for mid-session `/model` switches); falls back to the
/// session header's `provider`/`modelId`; returns `None` when the session
/// records no model, or when it matches the live model *and* ran the level it
/// would run anyway.
///
/// The full `provider:model_id` form is intentional: pre-filling the
/// unqualified `model_id` might surface multiple catalog entries that
/// share a short name under different providers.
///
/// `level_on_activate` is what activating `recorded` would install as its
/// reasoning level (call it with `AppState::peek_model_reasoning`). The
/// session's level joins the prefill **only** when it differs from that: a
/// session whose level came from this model's own record — the common case, as
/// most sessions never change it — is already reproduced by the activation, and
/// spelling it out would be a longer line saying nothing. The parameter rides
/// the `activate` leaf, which is where `/model <id> activate <level>` exists,
/// so the short `/model <id>` stays for the no-level case.
async fn compute_resume_model_prefill(
    session: &Session,
    live_model_id: &str,
    level_on_activate: Option<crate::params::ReasoningLevel>,
) -> Option<String> {
    let recorded = session_recorded_model_id(session).await?;
    // A level the frontend cannot name is dropped rather than pre-filled: the
    // `activate` leaf would refuse it, and a prefill that only errors is worse
    // than no prefill.
    let recorded_level = session_recorded_reasoning(session)
        .await
        .and_then(|l| l.trim().parse::<crate::params::ReasoningLevel>().ok());
    let level = match recorded_level {
        Some(level) if Some(level) != level_on_activate => Some(level),
        _ => None,
    };
    match (recorded == live_model_id, level) {
        // Session already runs the live model at the live level — nothing to
        // accept, and committing a matching prefill would only churn the
        // menu sync.
        (true, None) => None,
        (_, Some(level)) => Some(format!("/model {recorded} activate {}", level.as_str())),
        (false, None) => Some(format!("/model {recorded}")),
    }
}

/// Ensure a session exists, creating one lazily if needed.
/// Called on first actual turn (Submit or ! command), not at startup.
/// Pure-ish transform: returns `current` unchanged when it is `Some`,
/// otherwise creates a fresh session. The caller writes the result back
/// into the AppState slot (`set_session`/`activate_session`) — helpers
/// never touch the slot themselves.
pub(crate) async fn ensure_session(
    current: Option<Arc<Session>>,
    repo: &Option<crate::session::repo::JsonlSessionRepo>,
    provider: Option<String>,
    model: Option<String>,
    reasoning_level: Option<String>,
) -> Option<Arc<Session>> {
    if current.is_some() {
        return current;
    }
    let repo = match repo {
        Some(r) => r,
        None => return None,
    };
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return None,
    };
    match repo.create(&cwd).await {
        Ok(storage) => {
            let index =
                storage
                    .build_index()
                    .await
                    .unwrap_or_else(|_| crate::session::SessionIndex {
                        header: crate::session::SessionHeader::with_model_context(
                            &cwd,
                            provider,
                            model,
                            reasoning_level,
                        ),
                        entries: std::collections::HashMap::new(),
                        leaf_id: None,
                        children: std::collections::HashMap::new(),
                        session_name: None,
                    });
            Some(Arc::new(Session::new(storage, index)))
        }
        Err(e) => {
            eprintln!("Warning: cannot create session: {}", e);
            None
        }
    }
}

/// Fork a session (full tree, current dir) into a fresh `Arc<Session>` —
/// used by the session node's fork action; the caller activates it (slot).
pub(crate) async fn fork_session_at(
    repo: &crate::session::repo::JsonlSessionRepo,
    meta: &crate::session::SessionMetadata,
) -> std::result::Result<(Arc<Session>, String), String> {
    use crate::session::repo::SessionRepo;
    let cwd =
        std::env::current_dir().map_err(|_| "Cannot determine current directory".to_string())?;
    match repo.fork(meta, "", &cwd, ForkMode::FullTree).await {
        Ok(new_storage) => {
            let index = new_storage.build_index().await.unwrap_or_else(|_| {
                let header = crate::session::SessionHeader::new(&cwd);
                crate::session::SessionIndex {
                    header,
                    entries: std::collections::HashMap::new(),
                    leaf_id: None,
                    children: std::collections::HashMap::new(),
                    session_name: None,
                }
            });
            let new_session = Session::new_persisted(new_storage, index);
            let id = new_session.get_session_id().to_string();
            Ok((Arc::new(new_session), id))
        }
        Err(e) => Err(format!("Cannot fork session: {}", e)),
    }
}

/// Load a session by meta and switch to it (the `/session <id> resume`
/// action — used by the session node's `submit`).
///
/// Migrates to the current cwd when the session lives elsewhere (fork
/// + delete original), replays `(count, is_lines)` through `output` and then computes the `/model` prefill against `qualified_id`.
///
/// No silent restore of provider/model/thinking on resume: mutating
/// config and rebuilding the agent behind the user's back would remove
/// the chance to accept/abort/choose a model. The decision is surfaced
/// via the returned prefill instead when the session records a model
/// that DIFFERS from `qualified_id` (full `provider:model_id` form —
/// what the v2.5 `/model` node resolves directly), when it ran a
/// reasoning level that activating `qualified_id` would not reproduce,
/// or when both hold; when neither does, no prefill is returned.
/// `level_on_activate` is what activating `qualified_id` would install
/// (`AppState::peek_model_reasoning`). Errors are caller-displayed
/// messages (same texts the old arm printed).
pub(crate) async fn resume_session(
    repo: &crate::session::repo::JsonlSessionRepo,
    meta: &crate::session::SessionMetadata,
    replay: (usize, bool),
    output: &dyn Output,
    qualified_id: &str,
    level_on_activate: Option<crate::params::ReasoningLevel>,
) -> std::result::Result<(Arc<Session>, String, Option<String>), String> {
    use crate::session::repo::SessionRepo;
    let (count, is_lines) = replay;
    let cwd =
        std::env::current_dir().map_err(|_| "Cannot determine current directory".to_string())?;
    let (storage, index) = if meta.cwd != cwd {
        // Resume in a different directory → fork the session to the new
        // cwd and delete the original (migrates the file, sets the
        // header's `cwd` field).
        match repo.fork(meta, "", &cwd, ForkMode::FullTree).await {
            Ok(forked_storage) => {
                let index = match forked_storage.build_index().await {
                    Ok(i) => i,
                    Err(e) => return Err(format!("Cannot build session index: {}", e)),
                };
                if let Err(e) = repo.delete(meta).await {
                    return Err(format!("Cannot remove old session: {}", e));
                }
                (forked_storage, index)
            }
            Err(e) => return Err(format!("Cannot fork session: {}", e)),
        }
    } else {
        match repo.open(meta).await {
            Ok(r) => r,
            Err(e) => return Err(format!("Cannot resume session: {}", e)),
        }
    };
    let new_session = Session::new_persisted(storage, index);
    let id = new_session.get_session_id().to_string();
    if count > 0 {
        // Replay from the RAW branch so compaction entries render as
        // markers while the whole history is available; only the agent
        // context is scoped.
        let branch = new_session.get_branch().await;
        let units = build_replay_units(&branch);
        let total = count_turns(&units);
        if is_lines {
            // Line-based: capture full replay via writer, tail lines.
            output.begin_capture();
            replay_units(output, &units, 0);
            let bytes = output.end_capture();
            let captured = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = captured.lines().collect();
            let start = lines.len().saturating_sub(count);
            use std::io::Write as _;
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            for line in &lines[start..] {
                let _ = writeln!(handle, "{}", line);
            }
            let _ = handle.flush();
        } else {
            replay_last_turns(output, &units, count.min(total));
        }
    }
    let prefill = compute_resume_model_prefill(&new_session, qualified_id, level_on_activate).await;
    Ok((Arc::new(new_session), id, prefill))
}

/// Create a fresh session (dropping the in-memory one), activate it in
/// the slot, and apply an optional name — shared by the inline `/new`
/// and `/clear` nodes. Provider/model/thinking come from the **target**
/// config (handler-side stamping: recorded judgment — diverges from the
/// applied layer only in the failed-rebuild state; a later `ModelChange`
/// record corrects the journal).
pub(crate) async fn start_fresh_session(app_state: &AppState, name: &str) {
    let (provider, model, thinking) = app_state.with_config(|c| {
        (
            c.provider.clone(),
            c.model.clone(),
            c.params.reasoning_str(),
        )
    });
    let repo = app_state.repo();
    let fresh = ensure_session(None, &repo, Some(provider), Some(model), thinking).await;
    let id = fresh
        .as_ref()
        .map(|s| s.get_session_id().to_string())
        .unwrap_or_default();
    app_state.activate_session(fresh.clone(), id);
    if !name.is_empty() {
        if let Some(ref s) = fresh {
            let out = app_state.with_config(|c| c.output.clone());
            if let Err(e) = s.set_session_name(name).await {
                out.display_text(&format!("Could not name session: {}", e));
            } else if let Some(r) = app_state.repo() {
                r.invalidate_cache();
            }
        }
    }
}

impl CodeAgent {
    pub fn new(
        agent: Agent,
        output: Arc<dyn Output>,
        context_limit: Option<u64>,
        tool_call_limit: usize,
        provider: String,
        model: String,
        live_params: crate::params::ModelParams,
        yolo: bool,
        persona_name: Option<String>,
        user_cancel: Arc<AtomicBool>,
        confirm_approvals: crate::confirm::Approvals,
        cancel_notify: Arc<tokio::sync::Notify>,
        reasoning_in_context: crate::settings::ReasoningInContextMode,
        message_logger: Option<Arc<MessageLogger>>,
    ) -> Self {
        Self {
            agent,
            output,
            context_limit,
            tool_call_limit,
            provider,
            model,
            live_params,
            yolo,
            persona_name,
            user_cancel,
            confirm_approvals,
            cancel_notify,
            reasoning_in_context,
            message_logger,
            usage: Mutex::new(SessionUsage::default()),
        }
    }
}

/// Test helper: a temp-dir repo holding one session file (header + user
/// message + leaf, written directly so listings see it immediately).
/// Returns `(repo, session id, base dir)`; leaks the dir unless the
/// caller removes it.
#[cfg(test)]
pub(crate) fn test_repo_with_session() -> (
    tempfile::TempDir,
    crate::session::repo::JsonlSessionRepo,
    String,
    std::path::PathBuf,
) {
    use crate::session::repo::JsonlSessionRepo;
    use crate::session::types::{encode_cwd, session_filename, SessionHeader};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // A `TempDir` (returned first so the caller keeps it alive past the repo
    // and session). Removed on drop instead of leaking an empty directory.
    let base_guard = tempfile::tempdir().unwrap();
    let base = base_guard.path().to_path_buf();
    // A synthetic cwd: nothing creates it, and only `encode_cwd` and the
    // session header ever see it. Named per-run so that if anything did
    // resolve or create it, two checkouts sharing a `/tmp` still could not
    // collide.
    let cwd = std::path::Path::new("/tmp").join(format!("v25-cwd-{}-{nanos}", std::process::id()));
    let dir = base.join(encode_cwd(&cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let header = SessionHeader::new(&cwd);
    let uuid = header.id.clone();
    let path = dir.join(session_filename(&chrono::Utc::now()));
    let user_entry = serde_json::json!({
        "id": "e1",
        "timestamp": "2025-01-01T00:00:00.000Z",
        "type": "message",
        "message": {"role": "user", "content": [{"type": "text", "text": "hello world"}]},
    });
    let leaf = serde_json::json!({
        "id": "leaf1",
        "timestamp": "2025-01-01T00:00:01.000Z",
        "type": "leaf",
        "targetId": "e1",
    });
    let contents = format!(
        "{}\n{}\n{}\n",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&user_entry).unwrap(),
        serde_json::to_string(&leaf).unwrap(),
    );
    std::fs::write(&path, contents).unwrap();
    let repo = JsonlSessionRepo::new(base.clone());
    (base_guard, repo, uuid, base)
}

/// Test helper: drive a future on a fresh current-thread tokio runtime.
/// Node submits touch `tokio::fs`/the blocking pool, which futures'
/// plain `block_on` cannot host — use this for any test that awaits
/// repo/session IO.
#[cfg(test)]
pub(crate) fn block_on_test<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(f)
}

/// Test helper: a writable session built from `entries` (temp dir,
/// header written, leaf set to the last entry) wrapped in an `Arc` —
/// for tests that need a session in the AppState slot. Sync facade with
/// an internal current-thread runtime (`tokio::fs` needs one); leaks its
/// temp dir (same policy as the in-module compaction helper).
#[cfg(test)]
pub(crate) fn test_session_with_entries(
    entries: Vec<crate::session::SessionEntry>,
) -> (tempfile::TempDir, Arc<Session>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    rt.block_on(async move {
        use crate::session::storage::JsonlSessionStorage;
        use crate::session::types::session_filename;
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let path = dir.join(session_filename(&chrono::Utc::now()));
        let storage = JsonlSessionStorage::open_or_create(path)
            .await
            .expect("create storage");
        let header = crate::session::SessionHeader::new(std::path::Path::new("/test"));
        storage.write_header(&header).await.expect("write header");
        for entry in &entries {
            storage.append_entry(entry).await.expect("append");
        }
        if let Some(last) = entries.last() {
            let leaf = crate::session::SessionEntry::new_leaf(Some(last.id.clone()), None);
            storage.append_entry(&leaf).await.expect("append leaf");
        }
        let index = storage.build_index().await.expect("build index");
        (dir_guard, Arc::new(Session::new_persisted(storage, index)))
    })
}

#[cfg(test)]
mod tests {
    // B3a: live vs replay (row_accounting_b3.md §1). The two paths drive the
    // same Output methods, so they should paint the same bytes; today they do
    // not, because `begin_response` picks the buffered path whenever stdout is
    // not a tty — which is every test — and the buffered path frames the output
    // differently. B3c deletes the branch and this test becomes the gate that
    // keeps them identical.

    use super::{
        compute_resume_model_prefill, message_role, record_model_change, record_reasoning_change,
        session_recorded_model_id, session_recorded_reasoning, MessageLogger,
    };
    use crate::session::storage::SessionStorage;
    use crate::session::Session;
    use rig_core::message::{AssistantContent, Message, Text};

    /// Build a `Session` from `entries` for `compute_resume_model_prefill`
    /// tests. Mirrors `session::session::tests::build_session_for_test` —
    /// duplicated here because the agent's test module does not import the
    /// session test helpers (different visibility / layering).
    async fn build_session_with_entries(
        entries: Vec<crate::session::SessionEntry>,
    ) -> (tempfile::TempDir, Session, std::path::PathBuf) {
        use crate::session::storage::JsonlSessionStorage;
        use crate::session::types::session_filename;
        use std::path::Path;
        let dir_guard = tempfile::tempdir().unwrap();
        let dir = dir_guard.path().to_path_buf();
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);

        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = crate::session::SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.unwrap();
        for entry in &entries {
            storage.append_entry(entry).await.unwrap();
        }
        drop(storage);

        let ro = JsonlSessionStorage::open_readonly(path.clone())
            .await
            .unwrap();
        let index = ro.build_index().await.unwrap();
        (dir_guard, Session::new_persisted(ro, index), path)
    }

    #[tokio::test]
    async fn resume_model_prefill_uses_branch_model_change() {
        // Branch-derived: a `ModelChange` entry on the active path is the
        // authoritative source (the header is only written at session
        // creation and never updated on `/model`).
        let entries = vec![
            crate::session::SessionEntry {
                id: "u1".into(),
                parent_id: None,
                timestamp: "t1".into(),
                content: crate::session::SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "user",
                        "content": [{"type": "text", "text": "hi"}]
                    }),
                },
            },
            crate::session::SessionEntry {
                id: "m1".into(),
                parent_id: Some("u1".into()),
                timestamp: "t2".into(),
                content: crate::session::SessionEntryContent::ModelChange {
                    provider: "anthropic".into(),
                    model_id: "claude-sonnet-4-5".into(),
                },
            },
        ];
        let (_guard, session, path) = build_session_with_entries(entries).await;
        // Live model differs from the session's recorded model → prefill.
        let prefill = compute_resume_model_prefill(&session, "openai:gpt-4o", None).await;
        assert_eq!(
            prefill.as_deref(),
            Some("/model anthropic:claude-sonnet-4-5")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resume_model_prefill_picks_latest_model_change() {
        // When multiple `ModelChange` entries exist, the leaf-closest wins
        // (matches `derive_model_change`'s rev()-iteration contract).
        let entries = vec![
            crate::session::SessionEntry {
                id: "m1".into(),
                parent_id: None,
                timestamp: "t1".into(),
                content: crate::session::SessionEntryContent::ModelChange {
                    provider: "anthropic".into(),
                    model_id: "claude-3-5-sonnet".into(),
                },
            },
            crate::session::SessionEntry {
                id: "m2".into(),
                parent_id: Some("m1".into()),
                timestamp: "t2".into(),
                content: crate::session::SessionEntryContent::ModelChange {
                    provider: "openai".into(),
                    model_id: "gpt-4o".into(),
                },
            },
        ];
        let (_guard, session, path) = build_session_with_entries(entries).await;
        let prefill =
            compute_resume_model_prefill(&session, "anthropic:claude-3-5-sonnet", None).await;
        assert_eq!(prefill.as_deref(), Some("/model openai:gpt-4o"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resume_model_prefill_falls_back_to_header_when_no_model_change() {
        // No `ModelChange` on the branch — fall back to the session header's
        // provider/model_id (carried forward by `ensure_session` via
        // `with_model_context`). Both fields are required; a header with
        // only one returns None.
        use crate::session::storage::JsonlSessionStorage;
        use std::path::Path;

        let _tmp = tempfile::tempdir().unwrap();
        let dir = _tmp.path().to_path_buf();
        let filename = crate::session::types::session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);
        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = crate::session::SessionHeader::with_model_context(
            Path::new("/test"),
            Some("openrouter".into()),
            Some("anthropic/claude-sonnet-4".into()),
            None,
        );
        storage.write_header(&header).await.unwrap();
        drop(storage);

        let ro = JsonlSessionStorage::open_readonly(path.clone())
            .await
            .unwrap();
        let index = ro.build_index().await.unwrap();
        let session = Session::new_persisted(ro, index);
        let prefill = compute_resume_model_prefill(&session, "openai:gpt-4o", None).await;
        assert_eq!(
            prefill.as_deref(),
            Some("/model openrouter:anthropic/claude-sonnet-4")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resume_model_prefill_returns_none_when_nothing_known() {
        // No `ModelChange`, no header metadata → empty prompt (no nudge).
        let (_guard, session, path) = build_session_with_entries(vec![]).await;
        let prefill =
            compute_resume_model_prefill(&session, "anthropic:claude-3-5-sonnet-20241022", None)
                .await;
        assert_eq!(prefill, None);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resume_model_prefill_none_when_session_matches_live() {
        // Spec: the prefill only opens when the session's last model
        // differs from the running model. Session records
        // `anthropic:claude-sonnet-4-5`; the agent already runs it → no
        // prefill (no nudge, and committing it would churn the sync).
        let entries = vec![crate::session::SessionEntry {
            id: "m1".into(),
            parent_id: None,
            timestamp: "t1".into(),
            content: crate::session::SessionEntryContent::ModelChange {
                provider: "anthropic".into(),
                model_id: "claude-sonnet-4-5".into(),
            },
        }];
        let (_guard, session, path) = build_session_with_entries(entries).await;
        let prefill =
            compute_resume_model_prefill(&session, "anthropic:claude-sonnet-4-5", None).await;
        assert_eq!(prefill, None);
        let _ = std::fs::remove_file(&path);
    }

    /// A session that switched model *and* left a level behind: both halves of
    /// the decision ride one line, on the `activate` leaf (the only place a
    /// level parameter exists).
    #[tokio::test]
    async fn resume_prefill_carries_the_level_when_activation_would_not() {
        use crate::params::ReasoningLevel;
        let entries = vec![
            crate::session::SessionEntry {
                id: "m1".into(),
                parent_id: None,
                timestamp: "t1".into(),
                content: crate::session::SessionEntryContent::ModelChange {
                    provider: "anthropic".into(),
                    model_id: "claude-3-5-sonnet".into(),
                },
            },
            crate::session::SessionEntry {
                id: "t2".into(),
                parent_id: Some("m1".into()),
                timestamp: "t2".into(),
                content: crate::session::SessionEntryContent::ThinkingLevelChange {
                    thinking_level: "high".into(),
                },
            },
        ];
        let (_guard, session, path) = build_session_with_entries(entries).await;
        // Activating that model would seed `med` (its record), so `high` is
        // only in the prefill because the session ran it deliberately.
        let prefill =
            compute_resume_model_prefill(&session, "openai:gpt-4o", Some(ReasoningLevel::Med))
                .await;
        assert_eq!(
            prefill.as_deref(),
            Some("/model anthropic:claude-3-5-sonnet activate high")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The common case stays short: the session's level is what activating its
    /// model would produce anyway (almost always — the record, else the global
    /// default), and spelling it out would be a longer line saying nothing.
    #[tokio::test]
    async fn resume_prefill_omits_a_level_the_activation_reproduces() {
        use crate::params::ReasoningLevel;
        let entries = vec![
            crate::session::SessionEntry {
                id: "m1".into(),
                parent_id: None,
                timestamp: "t1".into(),
                content: crate::session::SessionEntryContent::ModelChange {
                    provider: "anthropic".into(),
                    model_id: "claude-3-5-sonnet".into(),
                },
            },
            crate::session::SessionEntry {
                id: "t2".into(),
                parent_id: Some("m1".into()),
                timestamp: "t2".into(),
                content: crate::session::SessionEntryContent::ThinkingLevelChange {
                    thinking_level: "high".into(),
                },
            },
        ];
        let (_guard, session, path) = build_session_with_entries(entries).await;
        let prefill =
            compute_resume_model_prefill(&session, "openai:gpt-4o", Some(ReasoningLevel::High))
                .await;
        assert_eq!(
            prefill.as_deref(),
            Some("/model anthropic:claude-3-5-sonnet")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The model matches but the level does not: resuming still has something
    /// to decide, and `activate` is the leaf that carries it — re-activating the
    /// running model installs no level of its own.
    #[tokio::test]
    async fn resume_prefill_nudges_for_a_level_alone() {
        use crate::params::ReasoningLevel;
        let entries = vec![
            crate::session::SessionEntry {
                id: "m1".into(),
                parent_id: None,
                timestamp: "t1".into(),
                content: crate::session::SessionEntryContent::ModelChange {
                    provider: "anthropic".into(),
                    model_id: "claude-3-5-sonnet".into(),
                },
            },
            crate::session::SessionEntry {
                id: "t2".into(),
                parent_id: Some("m1".into()),
                timestamp: "t2".into(),
                content: crate::session::SessionEntryContent::ThinkingLevelChange {
                    thinking_level: "max".into(),
                },
            },
        ];
        let (_guard, session, path) = build_session_with_entries(entries).await;
        let prefill = compute_resume_model_prefill(
            &session,
            "anthropic:claude-3-5-sonnet",
            Some(ReasoningLevel::Med),
        )
        .await;
        assert_eq!(
            prefill.as_deref(),
            Some("/model anthropic:claude-3-5-sonnet activate max")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A level the frontend cannot name is dropped, not pre-filled: the
    /// `activate` leaf would refuse it, and a prefill that only errors is worse
    /// than none. The model half still stands.
    #[tokio::test]
    async fn resume_prefill_drops_an_unnameable_level() {
        let entries = vec![
            crate::session::SessionEntry {
                id: "m1".into(),
                parent_id: None,
                timestamp: "t1".into(),
                content: crate::session::SessionEntryContent::ModelChange {
                    provider: "anthropic".into(),
                    model_id: "claude-3-5-sonnet".into(),
                },
            },
            crate::session::SessionEntry {
                id: "t2".into(),
                parent_id: Some("m1".into()),
                timestamp: "t2".into(),
                content: crate::session::SessionEntryContent::ThinkingLevelChange {
                    thinking_level: "turbo".into(),
                },
            },
        ];
        let (_guard, session, path) = build_session_with_entries(entries).await;
        let prefill = compute_resume_model_prefill(&session, "openai:gpt-4o", None).await;
        assert_eq!(
            prefill.as_deref(),
            Some("/model anthropic:claude-3-5-sonnet")
        );
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // Session metadata recording (menu sync band-aids)
    // ------------------------------------------------------------------

    /// Build an UNPERSISTED fresh session (no file on disk) via
    /// `repo.create()` + the `ensure_session` fallback header, like
    /// `run_interactive` does at startup. Returns `(session, file path)`.
    async fn fresh_unpersisted_session() -> (
        tempfile::TempDir,
        crate::session::Session,
        std::path::PathBuf,
    ) {
        use crate::session::repo::SessionRepo;
        use crate::session::storage::SessionStorage;
        use std::path::PathBuf;

        let base_guard = tempfile::tempdir().unwrap();
        let base: PathBuf = base_guard.path().to_path_buf();
        let repo = crate::session::repo::JsonlSessionRepo::new(base);
        let cwd = std::path::Path::new("/home/test/record");
        let storage = repo.create(cwd).await.unwrap();
        let path = storage.path().clone();
        let index = storage
            .build_index()
            .await
            .unwrap_or_else(|_| crate::session::SessionIndex {
                header: crate::session::SessionHeader::with_model_context(
                    cwd,
                    Some("anthropic".into()),
                    Some("claude-3-5-sonnet-20241022".into()),
                    Some("off".into()),
                ),
                entries: std::collections::HashMap::new(),
                leaf_id: None,
                children: std::collections::HashMap::new(),
                session_name: None,
            });
        (
            base_guard,
            crate::session::Session::new(storage, index),
            path,
        )
    }

    /// The core "empty session" regression: recording model/thinking
    /// metadata on a session that has never been persisted must NOT create
    /// a `.jsonl` file. Metadata is folded into the in-memory header (to be
    /// persisted by the first real message) instead of appended — the old
    /// path (`append_*` → `ensure_persisted`) wrote a header+marker-only
    /// file that littered `/session` with undeletable `(empty)` sessions
    /// (e.g. on `/quit`, whose DoneSuccess ran the menu sync one last time).
    #[tokio::test]
    async fn record_metadata_on_unpersisted_session_creates_no_file() {
        let (_guard, session, path) = fresh_unpersisted_session().await;
        assert!(
            !session.is_persisted(),
            "fresh session must start unpersisted"
        );
        assert!(!path.exists(), "precondition: no file yet");

        record_model_change(&session, "openai:gpt-4o").await;
        record_reasoning_change(&session, "high").await;

        assert!(
            !path.exists(),
            "metadata must never create a session file: {} exists",
            path.display()
        );
        assert!(!session.is_persisted(), "recording must not flip persisted");
        // The values landed in the in-memory header instead.
        assert_eq!(
            session_recorded_model_id(&session).await.as_deref(),
            Some("openai:gpt-4o")
        );
        assert_eq!(
            session_recorded_reasoning(&session).await.as_deref(),
            Some("high")
        );

        // Recording the same values again is a no-op (no state change).
        record_model_change(&session, "openai:gpt-4o").await;
        record_reasoning_change(&session, "high").await;
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// On a persisted session: a value the session already records is
    /// skipped (no redundant entry — the resume-accept case), a genuinely
    /// new value is appended.
    #[tokio::test]
    async fn record_model_change_on_persisted_session_skips_duplicate() {
        use crate::session::storage::SessionStorage;

        let _tmp = tempfile::tempdir().unwrap();
        let dir = _tmp.path().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");

        let storage = crate::session::storage::JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let header = crate::session::SessionHeader::with_model_context(
            std::path::Path::new("/t"),
            Some("anthropic".into()),
            Some("claude-3-5-sonnet-20241022".into()),
            None,
        );
        storage.write_header(&header).await.unwrap();
        drop(storage);

        // Writable handle — mirrors `repo.open()` (resume path); a
        // read-only handle would make `append_entry` fail silently.
        let rw = crate::session::storage::JsonlSessionStorage::open_or_create(path.clone())
            .await
            .unwrap();
        let index = rw.build_index().await.unwrap();
        let session = crate::session::Session::new_persisted(rw, index);
        assert!(session.is_persisted());

        let count_model_changes = || -> usize {
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter(|l| l.contains("\"type\":\"model_change\""))
                .count()
        };

        // Already recorded (header fallback) → skipped.
        record_model_change(&session, "anthropic:claude-3-5-sonnet-20241022").await;
        assert_eq!(count_model_changes(), 0, "duplicate must not be appended");

        // Genuinely new model → appended once.
        record_model_change(&session, "openai:gpt-4o").await;
        assert_eq!(count_model_changes(), 1, "new model must be appended");
        // And now it IS recorded — same value again is skipped.
        record_model_change(&session, "openai:gpt-4o").await;
        assert_eq!(count_model_changes(), 1, "second identical append skipped");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whole-string canonical model ids: model ids may contain colons
    /// (openrouter `ns/model:free`), so equality checks must compare the
    /// full `provider:model` string. The old sync split on ':' and took
    /// `nth(1)` — for `openrouter:tencent/hy3:free` that read
    /// `tencent/hy3`, never matching the configured id, so EVERY menu
    /// DoneSuccess (including `/quit`) looked like a model change.
    #[test]
    fn colon_model_id_roundtrips_through_qualified_form() {
        let provider = "openrouter";
        let model = "tencent/hy3:free";
        // The canonical id keeps the model's own colon intact…
        let canonical = format!("{}:{}", provider, model);
        assert_eq!(canonical, "openrouter:tencent/hy3:free");
        // …and the model part recovered via split_once is the FULL model id
        // (everything after the FIRST colon), unlike split(':').nth(1).
        let (p, m) = canonical.split_once(':').unwrap();
        assert_eq!(p, provider);
        assert_eq!(m, model);
    }

    // ---- display_tool_error plumbing ----
    //
    // Layer-1 Task 1 added `Output::display_tool_error` as a default-impl
    // method. The default impl formats a string and forwards to
    // `display_error`. This test pins the default-impl contract so a future
    // refactor that, say, calls `display_error` twice (once with the
    // formatted string, once with the message) or forgets to include the
    // tool name in the formatted output is caught.

    use crate::output::{Confirmation, Output};
    use serde_json::Value as JsonValue;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    struct ErrorRecorder {
        errors: StdArc<StdMutex<Vec<String>>>,
    }
    impl Output for ErrorRecorder {
        fn display_text(&self, _t: &str) {}
        fn display_markdown(&self, _t: &str) {
            self.display_text(_t);
        }
        fn begin_response(&self) {}
        fn stream_text(&self, _d: &str) {}
        fn stream_thinking(&self, _d: &str) {}
        fn end_response_segment(&self) {}
        fn end_response(&self) {}
        fn display_tool_call(&self, _n: &str, _a: &JsonValue) {}
        fn display_tool_result(&self, _r: &str) {}
        fn get_user_input(&self) -> String {
            String::new()
        }
        fn display_error(&self, e: &str) {
            self.errors.lock().unwrap().push(e.to_string());
        }
        fn display_system(&self, _t: &str) {}
        fn confirm(&self, _m: &str) -> Confirmation {
            Confirmation::No
        }
        fn display_separator(&self) {}
        fn display_header(&self, _p: &str, _m: &str, _y: bool, _l: usize, _pn: Option<&str>) {}
    }
    #[test]
    fn display_tool_error_default_impl_forwards_to_display_error_with_tool_name() {
        // The default impl on the `Output` trait is the path used by every
        // `Output` impl that doesn't override `display_tool_error` (e.g.
        // `QuietOutput`, `NoOutput` would no-op via their own method, but
        // any future `Output` impl that forgets to override gets this
        // default). Format check: tool name + kind + message all present.
        let errors = StdArc::new(StdMutex::new(Vec::new()));
        let recorder = ErrorRecorder {
            errors: errors.clone(),
        };
        // Call through the trait so we exercise the default impl, not a
        // concrete method.
        <dyn Output>::display_tool_error(
            &recorder,
            "edit",
            "invalid_args",
            "expected `,` or `}` at line 1",
        );
        let recorded = errors.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1, "expected exactly one display_error call");
        let msg = &recorded[0];
        assert!(msg.contains("edit"), "tool name missing: {msg}");
        assert!(msg.contains("invalid_args"), "kind missing: {msg}");
        assert!(
            msg.contains("expected `,` or `}` at line 1"),
            "message missing: {msg}"
        );
    }
    // Regression test for the `/new` (and `/clear`) bug: those commands set
    // the in-memory `session = None`, but the prior implementation never
    // recreated a session before the next Submit. As a result
    //   - `prompt(input, None, ...)` built an empty prior context, so
    //     turn-to-turn context was lost across prompts inside the new
    //     "session";
    //   - every `s.append_message(...)` is gated on `session.is_some()`,
    //     so the new turns were never persisted to disk — `/session` showed
    //     only the old (kept) session, and a quit produced no `.jsonl`.
    //
    // The fix calls `ensure_session()` after `/new` and `/clear` (and again
    // at the top of the Submit arm as a safety net). This test verifies
    // that the `ensure_session` mechanism the fix relies on correctly
    // produces a fresh, writable session after the in-memory session is
    // dropped, and that the new session's prior-context view contains
    // only its OWN history (not the dropped one's) — so the model would
    // see turn 2's prior = [turn 2-1].
    //
    // We invoke `repo.create()` directly (which is the same call
    // `ensure_session` makes once it has captured the cwd) so the test is
    // hermetic — independent of `std::env::current_dir()` at test-run time.
    #[tokio::test]
    async fn ensure_session_after_drop_creates_fresh_persistent_session() {
        use crate::session::repo::SessionRepo;
        use crate::session::storage::SessionStorage;
        use rig_core::message::Message;
        use std::path::PathBuf;

        let _tmp = tempfile::tempdir().unwrap();
        let base: PathBuf = _tmp.path().to_path_buf();
        let repo = crate::session::repo::JsonlSessionRepo::new(base.clone());
        let cwd = std::path::Path::new("/home/test/regression");

        // First session (the "before /new" one). Persist a message so the
        // session file is written and is distinct from a bare header.
        let storage1 = repo.create(cwd).await.unwrap();
        let header1 = crate::session::SessionHeader::new(cwd);
        let index1 =
            storage1
                .build_index()
                .await
                .unwrap_or_else(|_| crate::session::SessionIndex {
                    header: header1,
                    entries: std::collections::HashMap::new(),
                    leaf_id: None,
                    children: std::collections::HashMap::new(),
                    session_name: None,
                });
        let first = crate::session::Session::new(storage1, index1);
        let first_id = first.get_session_id().to_string();
        first
            .append_message(&Message::user("hello old session"))
            .await
            .unwrap();

        // Simulate `/new`: drop the in-memory session (the on-disk file is
        // preserved, matching `/new`'s contract).
        drop(first);

        // With the fix, the new handler now calls `ensure_session()` again,
        // producing a SECOND session with a fresh UUID and a writable storage.
        // We replicate that by calling `repo.create()` again with the same cwd.
        let storage2 = repo.create(cwd).await.unwrap();
        let header2 = crate::session::SessionHeader::new(cwd);
        let index2 =
            storage2
                .build_index()
                .await
                .unwrap_or_else(|_| crate::session::SessionIndex {
                    header: header2,
                    entries: std::collections::HashMap::new(),
                    leaf_id: None,
                    children: std::collections::HashMap::new(),
                    session_name: None,
                });
        let second = crate::session::Session::new(storage2, index2);
        let second_id = second.get_session_id().to_string();

        assert_ne!(
            first_id, second_id,
            "/new must yield a fresh session UUID; otherwise the new prompts would land in the kept session"
        );

        // The new session must be writable: appending the first turn should
        // succeed (pre-fix, session was still None here and append_message
        // was unreachable).
        second
            .append_message(&Message::user("hello new session t1"))
            .await
            .unwrap();
        second
            .append_message(&Message::assistant("t1 reply"))
            .await
            .unwrap();

        // And its prior-context view must reflect its OWN history only —
        // i.e. the model would see "t1, t1 reply" on the next turn, not the
        // old session's "hello old session".
        let ctx = second.build_context().await.unwrap();
        assert_eq!(
            ctx.messages.len(),
            2,
            "second session's context must hold its own two turns, not the first session's"
        );

        // Listing must surface both sessions (the kept old one and the new
        // persisted one) — pre-fix, only the old one was on disk.
        let metas = repo.list(Some(cwd)).await.unwrap_or_default();
        let ids: std::collections::HashSet<_> = metas.iter().map(|m| m.id.clone()).collect();
        assert!(
            ids.contains(&first_id) && ids.contains(&second_id),
            "both sessions must be on disk after /new; got {:?}",
            ids
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    // Regression test for `/new <name>` and `/clear <name>`: the optional
    // `<name>` argument must be applied to the freshly-created session via
    // `set_session_name`, so the user sees the chosen name in `/session`
    // listings and `set_session_name` round-trips through `build_index`
    // (i.e. the name is durable, not just an in-memory field).
    //
    // We exercise the same primitives the handler uses (drop -> repo.create
    // -> set_session_name) without spinning up the full REPL, so the test
    // stays hermetic and fast. The handler-side guarantee is that
    // `input["/new".len()..].trim()` (or `"/clear"`) flows into the
    // `set_session_name` call below; a one-line parser smoke test in
    // `slash.rs` covers that end.
    #[tokio::test]
    async fn new_and_clear_with_name_set_session_name_on_fresh_session() {
        use crate::session::repo::SessionRepo;
        use crate::session::storage::SessionStorage;
        use rig_core::message::Message;
        use std::path::PathBuf;

        for cmd in ["/new", "/clear"] {
            let _tmp = tempfile::tempdir().unwrap();
            let base: PathBuf = _tmp.path().to_path_buf();
            let repo = crate::session::repo::JsonlSessionRepo::new(base.clone());
            let cwd = std::path::Path::new("/home/test/regression");

            // Seed a previous session so the post-{cmd} state is a *new* one.
            let s0 = repo.create(cwd).await.unwrap();
            let h0 = crate::session::SessionHeader::new(cwd);
            let i0 = s0
                .build_index()
                .await
                .unwrap_or_else(|_| crate::session::SessionIndex {
                    header: h0,
                    entries: std::collections::HashMap::new(),
                    leaf_id: None,
                    children: std::collections::HashMap::new(),
                    session_name: None,
                });
            let prev = crate::session::Session::new(s0, i0);
            prev.append_message(&Message::user("seed")).await.unwrap();
            let prev_id = prev.get_session_id().to_string();
            drop(prev);
            // `/clear` deletes; `/new` keeps. Mirror that here so the listing
            // assertions below match each command's actual contract.
            if cmd == "/clear" {
                let metas = repo.list(Some(cwd)).await.unwrap_or_default();
                if let Some(m) = metas.iter().find(|m| m.id == prev_id) {
                    let _ = repo.delete(m).await;
                }
            }

            // Simulate the new dispatch path: drop in-memory session, create
            // fresh, then call set_session_name with the parsed `<name>`.
            // (For `/new` the previous session is still on disk; for `/clear`
            // it isn't — the new session is created identically either way.)
            let s1 = repo.create(cwd).await.unwrap();
            let h1 = crate::session::SessionHeader::new(cwd);
            let i1 = s1
                .build_index()
                .await
                .unwrap_or_else(|_| crate::session::SessionIndex {
                    header: h1,
                    entries: std::collections::HashMap::new(),
                    leaf_id: None,
                    children: std::collections::HashMap::new(),
                    session_name: None,
                });
            let fresh = crate::session::Session::new(s1, i1);
            let fresh_id = fresh.get_session_id().to_string();
            assert_ne!(fresh_id, prev_id, "{} must mint a new session id", cmd);

            let requested = "some project";
            fresh
                .set_session_name(requested)
                .await
                .expect("set_session_name should succeed on a fresh session");

            // The in-memory index must reflect the new name immediately:
            // `set_session_name` updates `index.session_name` so a UI that
            // reads the name off the live index (without forcing a
            // `build_index` rebuild) sees the user's choice without lag.
            let idx = fresh.index().lock().await;
            assert_eq!(
                idx.session_name.as_deref(),
                Some(requested),
                "{} <name> must populate the in-memory session_name",
                cmd
            );
            drop(idx);

            // And it must round-trip through the on-disk JSONL: re-opening
            // the same storage and rebuilding the index must surface the
            // same name (so a later process — or `/session` listing — sees
            // the user-chosen label, not a UUID).
            let metas = repo.list(Some(cwd)).await.unwrap_or_default();
            let meta = metas
                .iter()
                .find(|m| m.id == fresh_id)
                .expect("fresh session must be listed");
            let (storage_ro, index_ro) = repo.open(meta).await.unwrap();
            let _ = storage_ro; // currently unused; opened for parity with the real path
            assert_eq!(
                index_ro.session_name.as_deref(),
                Some(requested),
                "{} <name> must persist to the on-disk index",
                cmd
            );

            // `/clear` must have removed the old session; `/new` must have
            // kept it. Both must list the new one.
            let ids: std::collections::HashSet<_> = metas.iter().map(|m| m.id.clone()).collect();
            if cmd == "/clear" {
                assert!(
                    !ids.contains(&prev_id) && ids.contains(&fresh_id),
                    "/clear: old session must be gone, new session must be listed; got {:?}",
                    ids
                );
            } else {
                assert!(
                    ids.contains(&prev_id) && ids.contains(&fresh_id),
                    "/new: both sessions must be listed; got {:?}",
                    ids
                );
            }

            let _ = tokio::fs::remove_dir_all(&base).await;
        }
    }

    /// `--log-messages` must actually create the file and write one valid JSONL
    /// line per message, tagged with direction/role/turn. Regression test for
    /// the logger being wired but never called (no file created).
    #[tokio::test]
    async fn message_logger_writes_jsonl_file() {
        let _tmp = tempfile::tempdir().unwrap();
        let dir = _tmp.path().to_path_buf();
        let path = dir.join("messages.jsonl");
        let _ = tokio::fs::remove_file(&path).await;

        let logger = MessageLogger::new(
            path.display().to_string(),
            "test".to_string(),
            "test-model".to_string(),
        );

        // One round trip: history + prompt sent, assistant reply received,
        // all under the same turn id.
        let turn = logger.advance_turn();
        assert_eq!(turn, 1);
        logger
            .log_message("sent", &Message::user("prior"), turn)
            .await;
        logger
            .log_message("sent", &Message::user("hello"), turn)
            .await;
        logger
            .log_message(
                "received",
                &Message::Assistant {
                    id: Some("resp-1".into()),
                    content: vec![AssistantContent::Text(Text::new("hi"))],
                },
                logger.current_turn_id(),
            )
            .await;

        let raw = tokio::fs::read_to_string(&path)
            .await
            .expect("log file must be created on first write");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3, "three messages logged: {raw}");

        let sent: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(sent["direction"], "sent");
        assert_eq!(sent["role"], "user");
        assert_eq!(sent["turn_id"], 1);
        // rig `Message` serializes with an internal `role` tag.
        assert_eq!(sent["message"]["role"], "user");
        assert_eq!(sent["message"]["content"][0]["type"], "text");
        assert_eq!(sent["message"]["content"][0]["text"], "prior");

        let received: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(received["direction"], "received");
        assert_eq!(received["role"], "assistant");
        assert_eq!(received["turn_id"], 1);
        assert_eq!(received["message"]["role"], "assistant");
        assert_eq!(received["message"]["content"][0]["type"], "text");
        assert_eq!(received["message"]["content"][0]["text"], "hi");

        // message_role helper agrees with the serialized role.
        assert_eq!(message_role(&Message::user("x")), "user");
        assert_eq!(
            message_role(&Message::Assistant {
                id: None,
                content: vec![],
            }),
            "assistant"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
