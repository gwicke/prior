use crate::app_state::{AppState, WindowKeySource};
use crate::output::Confirmation;
use crate::params;
use crate::session::repo::{ForkMode, SessionRepo};
use crate::session::storage::SessionStorage;
use crate::session::Session;
use crate::tools::{
    bsed_available, set_sandbox, AgentBrowser, Bash, CopyFile, EditFile, GlobFiles, GrepText,
    ListDir, MakeDir, MoveFile, ReadFile, Remove, SedPreview, WriteFile,
};
use crate::Output;
use crate::Result;

use futures::StreamExt;
use rig_agent::agent::{
    Agent, AgentBuilder, AgentHook, CompletionCallAction, CompletionCallEvent,
    CompletionResponseEvent, HookContext, InvalidToolCallAction, InvalidToolCallContext,
    ModelTurnAction, ModelTurnFinished, MultiTurnStreamItem, NoToolConfig, ObservationAction,
    RequestPatch, StreamingError, ToolCall, ToolCallAction, ToolResultAction, ToolResultEvent,
};
use rig_agent::client::AgentClientExt;
use rig_agent::completion::{Chat, PromptError};
use rig_agent::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingPrompt};
use rig_agent::tool::ToolContext;
use rig_core::client::CompletionClient;
use std::collections::HashMap;

use rig_core::message::{
    AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolCallId, ToolFunction,
    ToolResult, ToolResultContent, UserContent,
};
use rig_core::providers::{
    anthropic, azure, chatgpt, cohere, copilot, deepseek, doubleword, gemini, groq, huggingface,
    hyperbolic, llamacpp, minimax, mira, mistral, moonshot, ollama, openai, openrouter, perplexity,
    together, xai, xiaomimimo, zai,
};
use rig_reqwest::client::{DefaultTransportBuilder, DefaultTransportClient};
use serde_json;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

pub(crate) mod errors;
pub(crate) mod msglog;
pub(crate) mod pumps;
pub(crate) mod status;

use errors::{classify_prompt_error, classify_stream_error, retry_budget, retry_delay_secs};
#[cfg(test)]
use msglog::message_role;
use msglog::MessageLogger;
use pumps::{
    cancel_observed, cancellable_sleep_windowed, dispatch_in_turn_event, next_window_key,
    pump_window_key,
};
use status::{first_user_text, status_text, BarModel};

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

/// Canonical transcript of the in-flight run, mirroring rig's `new_messages`
/// shape: `[assistant, user(tool-results), assistant, …]` (the user prompt is
/// added at persist time by `prompt()`, which owns `input`). picocode
/// accumulates this from the stream events so a mid-turn failure can persist
/// exactly what completed — and nothing more — before the REPL retries.
///
/// * [`LoggingHook::on_model_turn_finished`] appends each *accepted* assistant
///   turn (`event.content` is the canonical, provider-normalized
///   `AssistantContent` rig parked for the turn — the same content a
///   successful run persists via `FinalResponse.messages()`).
/// * Tool-result stream items (`StreamUserItem(ToolResult)`) are batched into
///   one `Message::User` and flushed on the next assistant turn or at
///   [`finalize`](Self::finalize) — matching rig, which pushes each completed
///   batch as a single `Message::User`.
///
/// [`finalize`](Self::finalize) returns a canonical transcript for
/// persistence: a trailing assistant turn whose tool calls were never answered
/// (the run was interrupted mid-batch) is *completed* with synthesized text
/// tool results — one per unanswered call, marked "did not execute" — so
/// nothing is dropped or truncated and rig's `validate_canonical` passes.
/// This mirrors rig's own `invalid_tool_retry_user_message`
/// (`rig-run/src/transcript.rs`), which answers rejected calls with text
/// results rather than removing them.
#[derive(Default)]
struct TurnTranscript {
    /// Completed messages in canonical order.
    committed: Vec<Message>,
    /// Accepted assistant turns. `> 0` means the run got past the first model
    /// call — i.e. there is progress worth persisting on a mid-turn failure.
    assistant_turns: usize,
    /// Tool results of the in-flight batch (following an assistant tool
    /// turn); flushed as one `Message::User` on the next assistant turn or at
    /// finalize.
    pending_results: Vec<UserContent>,
}

impl TurnTranscript {
    /// Append an accepted assistant turn. Any pending tool-result batch is
    /// flushed first so the transcript stays canonical (assistant → user
    /// results → …). Turns with no visible content (rig curates empty turns
    /// out of history) are skipped.
    fn push_assistant_turn(&mut self, id: Option<String>, content: Vec<AssistantContent>) {
        self.flush_results();
        if !assistant_content_is_visible(&content) {
            return;
        }
        self.committed.push(Message::Assistant { id, content });
        self.assistant_turns += 1;
    }

    /// Accumulate one tool result of the in-flight batch.
    fn push_tool_result(&mut self, result: rig_core::message::ToolResult) {
        self.pending_results.push(UserContent::ToolResult(result));
    }

    /// Whether any model turn completed (progress worth persisting).
    fn has_progress(&self) -> bool {
        self.assistant_turns > 0
    }

    /// The canonical transcript to persist on interruption. Flushes any
    /// pending tool-result batch, then — if the transcript still ends on an
    /// assistant turn with tool calls that never ran — answers each call with
    /// a synthesized text result. Never truncates or drops progress.
    fn finalize(&mut self) -> Vec<Message> {
        self.flush_results();
        let needs_synthesis = matches!(
            self.committed.last(),
            Some(Message::Assistant { content, .. })
                if content.iter().any(|c| matches!(c, AssistantContent::ToolCall(_)))
        );
        if needs_synthesis {
            let calls: Vec<&rig_core::message::ToolCall> = match self.committed.last() {
                Some(Message::Assistant { content, .. }) => content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::ToolCall(tc) => Some(tc),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let synthesized: Vec<UserContent> = calls
                .iter()
                .map(|tc| {
                    UserContent::ToolResult(rig_core::message::ToolResult {
                        call: tc.id.clone(),
                        provider: tc.provider.clone(),
                        name: tc.function.name.clone(),
                        content: vec![ToolResultContent::text(format!(
                            "[tool call `{}` was interrupted before execution; \
                             it did not run. Continue from the conversation above.]",
                            tc.function.name
                        ))],
                    })
                })
                .collect();
            if !synthesized.is_empty() {
                self.committed.push(Message::User {
                    content: synthesized,
                });
            }
        }
        self.committed.clone()
    }

    /// Flush a pending tool-result batch as one `Message::User`.
    fn flush_results(&mut self) {
        if self.pending_results.is_empty() {
            return;
        }
        let results = std::mem::take(&mut self.pending_results);
        self.committed.push(Message::User { content: results });
    }
}

/// Whether an accepted assistant turn carries anything the user/model should
/// see (non-empty text, reasoning, a tool call, or any other content part).
/// Mirrors rig's curation of empty turns out of history.
fn assistant_content_is_visible(content: &[AssistantContent]) -> bool {
    content.iter().any(|c| match c {
        AssistantContent::Text(t) => !t.text.trim().is_empty(),
        AssistantContent::Reasoning(r) => !reasoning_text(r).is_empty(),
        AssistantContent::ToolCall(_) => true,
        _ => true,
    })
}

/// Flatten a rig `Reasoning` payload into plain text. Provider-specific
/// encrypted/redacted blocks are kept verbatim; other shapes are ignored.
fn reasoning_text(r: &rig_core::message::Reasoning) -> String {
    let mut result = String::new();
    for c in &r.content {
        let s = match c {
            ReasoningContent::Text { text, .. } => text.as_str(),
            ReasoningContent::Summary(s) => s.as_str(),
            ReasoningContent::Encrypted(s) => s.as_str(),
            ReasoningContent::Redacted { data } => data.as_str(),
        };
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(s);
    }
    result
}

/// Render `edit` tool-call arguments in the model-facing block form.
///
/// The transcript is stored canonically (a pi-compatible `edits` array) and the
/// wire *accepts* that array too, but the schema does not advertise it — so the
/// history normalizes it back to blocks and the model sees **one** shape. That
/// is still the right call with two shapes accepted: the block form is the one
/// weak harnesses can encode, and a history that mixes both teaches the model to
/// alternate between them. Idempotent and allocation-light: non-`edit` calls and
/// already-block arguments pass through unchanged.
fn blob_edit_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|m| match m {
            Message::Assistant { id, content } => {
                let content = content
                    .into_iter()
                    .map(|c| match c {
                        AssistantContent::ToolCall(tc) if tc.function.name == "edit" => {
                            let mut tc = tc;
                            tc.function.arguments =
                                crate::edit_blocks::array_to_blob_value(&tc.function.arguments);
                            AssistantContent::ToolCall(tc)
                        }
                        other => other,
                    })
                    .collect();
                Message::Assistant { id, content }
            }
            other => other,
        })
        .collect()
}

/// The text of a tool result, as the model sees it.
fn tool_result_text(tr: &ToolResult) -> Option<String> {
    let joined = tr
        .content
        .iter()
        .filter_map(|rc| match rc {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

/// The per-file halves of one multi-file `edit` call and its result.
///
/// `per_file` is one blob per file (the synthetic call arguments);
/// `results` is one result string per file. Both are in the same first-seen
/// order, because the result array is built in the same order the files were
/// applied — so a synthetic call and its result pair up by index.
struct SplitEditCall {
    per_file: Vec<String>,
    results: Vec<String>,
}

/// Split a multi-file `edit` call and its result into per-file halves.
///
/// Returns `None` unless *both* halves are present, multi-file, and the same
/// length: a call with no array result (mid-turn, or a call that errored before
/// producing one) and a single-file call are left exactly as they are, so a
/// partially-saved history never renders a split with a dangling result. The
/// length check is a belt-and-braces guard — the two are built from the same
/// file list, so a mismatch would mean a corrupt array, and dropping the whole
/// call to the original single form is the safe reading of that.
fn split_edit_call_and_result(args: &serde_json::Value, result: &str) -> Option<SplitEditCall> {
    let per_file = crate::edit_blocks::per_file_blocks(args)?;
    let results: Vec<String> = blockedit::parse_file_results(result)?
        .iter()
        .map(|f| match &f.result {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    (per_file.len() == results.len()).then_some(SplitEditCall { per_file, results })
}

/// The note shown in place of one multi-file call, so the model is not left
/// wondering why it is looking at N calls it did not make.
fn split_note(n: usize) -> String {
    format!(
        "Note: the edit call above edited {n} files. It is shown here as {n} \
         per-file calls with one result each, so each file's outcome is \
         attributed to a file."
    )
}

/// Rewrite a multi-file `edit` call and its result into per-file calls and
/// per-file results, so the model sees one file per call — the shape it would
/// have produced by calling the tool N times, and the shape a reloaded session
/// replays (the persist half of this, W8, writes the same per-file form).
///
/// This is a **view** transform on the request history. It must not alter what
/// is stored: the per-file result array is the durable record, and rewriting it
/// here would leave the session unsplittable. See doc/batch_editing_v2.md,
/// "The result must survive to history" — the reason `on_tool_result` is left
/// alone.
///
/// Idempotent by construction: re-applying finds only single-file calls (the
/// synthetic ones) and non-array results, and leaves the history alone.
fn split_multi_file_edit_calls(messages: Vec<Message>) -> Vec<Message> {
    // The call (assistant message) and its result (a user message) are
    // different messages, and the result follows the call, so build the whole
    // plan before rebuilding any message.
    let mut results: HashMap<ToolCallId, String> = HashMap::new();
    for m in &messages {
        let Message::User { content } = m else {
            continue;
        };
        for c in content {
            if let UserContent::ToolResult(tr) = c {
                if let Some(text) = tool_result_text(tr) {
                    results.insert(tr.call.clone(), text);
                }
            }
        }
    }

    let mut plan: HashMap<ToolCallId, SplitEditCall> = HashMap::new();
    for m in &messages {
        let Message::Assistant { content, .. } = m else {
            continue;
        };
        for c in content {
            let AssistantContent::ToolCall(tc) = c else {
                continue;
            };
            if tc.function.name != "edit" {
                continue;
            }
            if let Some(result) = results.get(&tc.id) {
                if let Some(split) = split_edit_call_and_result(&tc.function.arguments, result) {
                    plan.insert(tc.id.clone(), split);
                }
            }
        }
    }
    if plan.is_empty() {
        return messages;
    }

    // Rebuild. Every synthetic message clears `provider` and `signature`: the
    // provider issued one id and one signature, and Anthropic's wire requires
    // `tool_result.tool_use_id` to equal a `tool_use.id` it issued — N
    // messages claiming the original provider id would be unmatchable.
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            Message::Assistant { id, content } => {
                let mut new_content = Vec::with_capacity(content.len());
                for c in content {
                    let AssistantContent::ToolCall(tc) = &c else {
                        new_content.push(c);
                        continue;
                    };
                    let Some(split) = plan.get(&tc.id) else {
                        new_content.push(c);
                        continue;
                    };
                    let n = split.per_file.len();
                    for (i, blocks) in split.per_file.iter().enumerate() {
                        new_content.push(AssistantContent::ToolCall(rig_core::message::ToolCall {
                            id: crate::edit_blocks::split_id(&tc.id, i, n),
                            provider: None,
                            function: ToolFunction::new(
                                "edit".into(),
                                serde_json::json!({ "blocks": blocks }),
                            ),
                            signature: None,
                            additional_params: None,
                        }));
                    }
                }
                out.push(Message::Assistant {
                    id,
                    content: new_content,
                });
            }
            Message::User { content } => {
                let mut new_content = Vec::with_capacity(content.len());
                for c in content {
                    let UserContent::ToolResult(tr) = &c else {
                        new_content.push(c);
                        continue;
                    };
                    let Some(split) = plan.get(&tr.call) else {
                        new_content.push(c);
                        continue;
                    };
                    let n = split.per_file.len();
                    // The note rides with the results it explains, as leading
                    // text in the same user message — not a message of its own,
                    // which would perturb the turn structure.
                    new_content.push(UserContent::Text(Text::new(split_note(n))));
                    for (i, result) in split.results.iter().enumerate() {
                        new_content.push(UserContent::ToolResult(ToolResult {
                            call: crate::edit_blocks::split_id(&tr.call, i, n),
                            provider: None,
                            name: tr.name.clone(),
                            content: vec![ToolResultContent::text(result.clone())],
                        }));
                    }
                }
                out.push(Message::User {
                    content: new_content,
                });
            }
            other => out.push(other),
        }
    }
    out
}

/// No-op in rig ≥1c2f440: `ToolResult::call` (rig's correlation handle) is
/// always present — minted at the provider boundary when the provider
/// issued no id — so the old cross-provider `call_id` normalisation is
/// inherently satisfied. Kept as a named passthrough so the persist path
/// reads the same. See doc/pi_steering_rig.md §6.5.4.
fn normalize_call_ids(messages: Vec<Message>) -> Vec<Message> {
    messages
}

/// Merge a finished (or aborted) turn into the session `history`.
///
/// * `final_history` — messages for a *successfully completed* turn (`[user,
///   assistant]`). Appended, because `history` already holds the prior turns;
///   this mirrors rig's blocking `chat()`, which does `chat_history.extend(
///   messages)`. This is the root-cause-#1 fix: the previous code *replaced*
///   the whole history with just this turn, so earlier context was lost every
///   turn.
/// * `cancelled_history` — the in-flight *full* history rig attaches to a
///   `PromptCancelled` error (prior turns + the aborted user prompt + any
///   partial assistant text). Replaces `history` rather than appending, since
///   it already contains the prior turns; appending would duplicate them. This
///   is the root-cause-#2 fix.
/// * `reasoning_in_context` — when `false`, `Reasoning` blocks are stripped
///   from assistant messages before they enter the stored context (a cheap,
///   deterministic "Level 0" compaction that saves tokens). Ignored for
///   Anthropic-family providers (`provider`/`model` satisfy
///   [`is_anthropic_family`]) **on turns that contain tool calls**, which
///   require the leading reasoning to be replayed verbatim on the next turn
///   (thinking+tool_use contract); reasoning is force-kept there. On
///   Anthropic turns *without* tool calls, and on all other providers when
///   the flag is `false`, reasoning is stripped normally.
#[allow(dead_code)]
fn persist_turn(
    history: &mut Option<&mut Vec<Message>>,
    final_history: Option<Vec<Message>>,
    cancelled_history: Option<Vec<Message>>,
    reasoning_in_context: crate::settings::ReasoningInContextMode,
    provider: &str,
    model: &str,
) {
    if let Some(h) = history {
        if let Some(fh) = final_history {
            let to_commit = maybe_strip_reasoning(fh, reasoning_in_context, provider, model);
            let to_commit = normalize_call_ids(to_commit);
            h.extend(to_commit);
        } else if let Some(ch) = cancelled_history {
            let to_commit = maybe_strip_reasoning(ch, reasoning_in_context, provider, model);
            let to_commit = normalize_call_ids(to_commit);
            **h = to_commit;
        }
    }
}

/// Strip `AssistantContent::Reasoning` blocks based on the
/// `ReasoningInContextMode`. Returns messages unchanged when:
/// - Mode is `Always`
/// - Provider is Anthropic-family **and** the turn contains tool calls
///   (the thinking+tool_use replay contract)
///
/// When mode is `AbortedTurnsOnly`, reasoning is preserved for assistant
/// messages with `id: None` (aborted/errored turns created locally without
/// a provider responseId), but stripped from successful turns.
///
/// Assistant messages left empty by the filter get a placeholder `Text` so
/// the message stays well-formed and user/assistant alternation holds.
fn maybe_strip_reasoning(
    messages: Vec<Message>,
    reasoning_in_context: crate::settings::ReasoningInContextMode,
    provider: &str,
    model: &str,
) -> Vec<Message> {
    let keep_all = matches!(
        reasoning_in_context,
        crate::settings::ReasoningInContextMode::Always
    ) || (is_anthropic_family(provider, model) && turn_has_tool_calls(&messages));
    if keep_all {
        return messages;
    }
    let keep_aborted = matches!(
        reasoning_in_context,
        crate::settings::ReasoningInContextMode::AbortedTurnsOnly
    );
    messages
        .into_iter()
        .map(|m| match m {
            Message::Assistant { id, content } => {
                let is_aborted = id.is_none(); // No provider responseId = aborted/errored turn
                let kept: Vec<AssistantContent> = content
                    .into_iter()
                    .filter(|c| {
                        if keep_aborted && is_aborted && matches!(c, AssistantContent::Reasoning(_))
                        {
                            true // Preserve reasoning for aborted turns
                        } else {
                            !matches!(c, AssistantContent::Reasoning(_))
                        }
                    })
                    .collect();
                let content = if kept.is_empty() {
                    vec![AssistantContent::Text(Text::new("[reasoning stripped]"))]
                } else {
                    kept
                };
                Message::Assistant { id, content }
            }
            other => other,
        })
        .collect()
}

/// Whether the turn's persisted messages include at least one assistant
/// `ToolCall` block. Used to decide whether Anthropic-family reasoning must
/// be force-kept: the thinking+tool_use replay contract only applies on
/// turns that actually involve tool calls.
fn turn_has_tool_calls(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m {
        Message::Assistant { content, .. } => content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_))),
        _ => false,
    })
}

/// Whether `provider`/`model` refers to an Anthropic-family model, for which
/// reasoning must be kept in context (the thinking+tool_use replay contract).
/// Covers direct `anthropic` and OpenRouter routing to `anthropic/*` models.
fn is_anthropic_family(provider: &str, model: &str) -> bool {
    provider.eq_ignore_ascii_case("anthropic")
        || (provider.eq_ignore_ascii_case("openrouter") && model.starts_with("anthropic/"))
}

fn is_tool_available(tool: &str) -> bool {
    std::process::Command::new("which")
        .arg(tool)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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

#[derive(Clone)]
pub struct AgentConfig {
    pub provider: String,
    pub model: String,
    pub output: Arc<dyn Output>,
    pub yolo: bool,
    pub quiet: bool,
    /// The REPL's end of the confirm port, cloned into every guarded tool.
    ///
    /// `None` means "nobody to ask", and the tools fall back to the output's
    /// synchronous confirm policy — the correct behaviour for piped/batch input
    /// and for one-shot `run_once`, where there is no key pump to drive a
    /// dialog. Set once by `run_interactive` before the agent is built, so a
    /// `/model` rebuild (which clones this config) keeps the same port and the
    /// same session approvals.
    pub confirm_port: Option<crate::confirm::ConfirmPort>,
    /// The request-shaping knobs for this model: reasoning level, temperature,
    /// top_p. Each is `None` = "unspecified, leave the provider's default".
    /// Replaces the old `reasoning_level: Option<String>`
    /// (doc/model_settings_v2.md A.2); `PartialEq` here is what the rebuild
    /// gate compares.
    pub params: crate::params::ModelParams,
    /// The target model's context window, *resolved* — the catalog overlaid
    /// with any stored per-model override.
    ///
    /// Carried here rather than looked up per status-bar paint, for two
    /// reasons: `status_text` is a pure function with no store to read, and a
    /// raw-catalog lookup there divided the context percentage by
    /// OpenRouter's number even when the user had overridden it. Set by
    /// `run_interactive` from `AppState::resolved_models`; `None` where there
    /// is no session to draw a bar for.
    pub context_limit: Option<u64>,
    pub tool_call_limit: usize,
    pub system_message_extension: Option<String>,
    pub persona_prompt: Option<String>,
    pub persona_name: Option<String>,
    pub bash_auto_allow: Option<Vec<String>>,
    pub agent_prompt: Option<String>,
    /// Shared cancel flag. The turn's key dispatcher sets this when a root
    /// `Esc` is dispatched during an LLM request (or its retry back-off);
    /// the `LoggingHook` bridge below turns it into a rig `CancelSignal` so
    /// the in-flight request stops.
    pub user_cancel: Arc<AtomicBool>,
    /// Event-based cancel signal paired with `user_cancel`. The turn's key
    /// dispatcher calls `notify_one()` when a root `Esc` is dispatched; the
    /// streaming loop selects on `notified()` instead of polling.
    pub cancel_notify: Arc<tokio::sync::Notify>,
    /// Resolved sandbox configuration for shell tool execution. `None` means
    /// the sandbox was never configured, in which case shell commands are
    /// refused rather than run unsandboxed. In normal operation `main.rs`
    /// always resolves this to a `Some(..)` (and `bwrap` is required at
    /// startup), so commands always run inside the namespace. There is no
    /// enable switch — the sandbox is always active when a config is present.
    pub sandbox: Option<crate::config::SandboxConfig>,
    /// Shared, live-editable `show_reasoning` display mode. The `Output` impl
    /// holds a clone and reads it on every thinking event, so `/set
    /// show_reasoning …` updates the rendering immediately (no agent rebuild).
    pub show_reasoning_handle: crate::settings::ShowReasoningHandle,
    /// How reasoning blocks are kept in the LLM context. Default `Never`.
    /// Forced `Always` for Anthropic-family providers (thinking+tool_use replay contract).
    pub reasoning_in_context: crate::settings::ReasoningInContextMode,
    /// Optional path to JSONL file for logging raw messages sent/received.
    /// When Some, messages are appended to this file in JSONL format.
    pub message_log_path: Option<String>,
    /// Optional custom directory for session storage. When None, the platform
    /// default is used (e.g., `~/.config/picocode/sessions`).
    pub sessions_dir: Option<std::path::PathBuf>,
}

impl AgentConfig {
    /// Canonical `provider:model` id for the model this config runs.
    ///
    /// This is the single canonical form used for equality checks against
    /// the `AppState` target model and session-recorded model ids — compare whole
    /// strings, never split segments: model ids themselves may contain
    /// colons (e.g. openrouter `…:free`), and a `split(':').nth(1)` style
    /// truncation reads a different value than what was configured.
    /// Mirrors `ModelEntry::qualified()` for catalog entries.
    pub fn qualified_model_id(&self) -> String {
        format!("{}:{}", self.provider, self.model)
    }
}

/// Sentinel env-var value that triggers the interactive device-code OAuth
/// flow instead of using a literal token. Consistent with the other providers:
/// the env var must be set; its value decides the auth method.
const OAUTH_TOKEN: &str = "oauth";

/// Create `dir` (and parents) with restrictive `0o700` permissions on Unix so a
/// cached OAuth refresh token is not world/group readable. On other platforms we
/// just create it.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(())
}

/// Merge OpenRouter `ProviderPreferences` into the existing additional_params
/// (typically thinking config). Pins requests to a preferred upstream provider
/// so that server-side cache state accumulates across turns instead of being
/// invalidated by load-balancing between multiple upstreams for the same model.
/// The params to send for this turn, plus any notes about them.
///
/// A `/reasoning` override is deliberately *not* remapped here. The remap lives
/// in [`crate::params::additional_params`] — the one place that knows the
/// provider's spelling of each level and the model's own effort table — and
/// doing it here too would be a second implementation free to disagree with the
/// first. So the requested level goes through untouched and this *reports* the
/// move, comparing ranks so a pure spelling difference (our `med`, their
/// `medium`) is not announced as a downgrade.
/// The note to show when a `/reasoning` override asks for a level `support`
/// cannot serve, or `None` when it can.
///
/// Split out of [`effective_params`] so it is testable against a support table
/// directly, with no catalog cache involved — the same reason
/// `model_catalog::support_for_entry` is split out.
///
/// The comparison is on **rank**, not on the string. `resolve` returns the
/// model's own spelling, so a model publishing `medium` for our `med` would
/// otherwise be reported as a downgrade that never happened.
fn reasoning_remap_note(
    requested: crate::params::ReasoningLevel,
    support: &crate::params::ReasoningSupport,
) -> Option<String> {
    let resolved = support.resolve(requested)?;
    let moved = resolved
        .parse::<crate::params::ReasoningLevel>()
        .map(|l| l.rank())
        != Ok(requested.rank());
    moved.then(|| {
        format!(
            "note: reasoning '{}' → '{resolved}' (this model does not support it)",
            requested.as_str()
        )
    })
}

/// The params to send for this turn, plus any notes about them.
///
/// A `/reasoning` override is deliberately *not* remapped here. The remap lives
/// in [`crate::params::additional_params`] — the one place that knows the
/// provider's spelling of each level and the model's own effort table — and
/// doing it here too would be a second implementation free to disagree with the
/// first. So the requested level goes through untouched and this *reports* the
/// move, via [`reasoning_remap_note`].
pub(crate) fn effective_params(
    app_state: &crate::app_state::AppState,
    overrides: &crate::turn_overrides::TurnOverrides,
) -> (crate::params::ModelParams, Vec<String>) {
    let live = app_state.params();
    if overrides.is_empty() {
        return (live, Vec::new());
    }
    let mut notes = Vec::new();
    if let Some(requested) = overrides.reasoning {
        let (provider, model) = app_state.with_config(|c| (c.provider.clone(), c.model.clone()));
        let support = crate::model_catalog::reasoning_support(&provider, &model);
        notes.extend(reasoning_remap_note(requested, &support));
    }
    (overrides.apply(&live), notes)
}

/// The provider-specific `additional_params` for one request: picocode's
/// reasoning/top_p composition, then any provider post-processing.
///
/// The single place both the build path and the per-turn override path go
/// through. They must agree exactly — the per-request path *replaces* the baked
/// value, because rig's merging setter would leave a stale key behind on, say,
/// anthropic's `thinking` block — so any divergence would show up as a request
/// shape that depends on whether the turn happened to carry an override.
fn request_additional_params(
    provider: &str,
    model: &str,
    params: &crate::params::ModelParams,
) -> Option<serde_json::Value> {
    let support = crate::model_catalog::reasoning_support(provider, model);
    let base = params::additional_params(&provider.to_lowercase(), params, &support);
    if provider.eq_ignore_ascii_case("openrouter") {
        // The openrouter branch in `create_agent` pins a provider preference
        // into the same object; an override turn must keep it.
        merge_openrouter_params(base)
    } else {
        base
    }
}

/// Merge OpenRouter `ProviderPreferences` into the existing additional_params
/// (typically thinking config). Pins requests to a preferred upstream provider
/// so that server-side cache state accumulates across turns instead of being
/// invalidated by load-balancing between multiple upstreams for the same model.
fn merge_openrouter_params(existing: Option<serde_json::Value>) -> Option<serde_json::Value> {
    use rig_core::providers::openrouter::ProviderPreferences;
    let prefs = ProviderPreferences::new()
        .order(["anthropic"])
        .allow_fallbacks(true);
    let prefs_json = prefs.to_json();
    match existing {
        Some(mut base) => {
            // Merge provider preferences into the existing params manually.
            if let (Some(base_obj), Some(prefs_obj)) =
                (base.as_object_mut(), prefs_json.as_object())
            {
                for (k, v) in prefs_obj {
                    base_obj.insert(k.clone(), v.clone());
                }
            }
            Some(base)
        }
        None => {
            let obj = prefs_json.as_object()?;
            if obj.is_empty() {
                None
            } else {
                Some(prefs_json)
            }
        }
    }
}

pub async fn create_agent(config: AgentConfig) -> Result<CodeAgent> {
    let provider = config.provider.to_lowercase();
    let model = config.model.clone();
    // What this model accepts, so a level it cannot serve is remapped instead
    // of sent and rejected. One shared composition, so the per-request path
    // (a `/reasoning` override) cannot produce a different shape than a normal
    // turn.
    let reasoning_json = request_additional_params(&config.provider, &model, &config.params);

    // OAuth credentials live in a `token` sub-directory of the settings dir
    // (see `platform::app_config_dir`), kept private so refresh tokens aren't
    // world-readable. Created up front so the per-provider subdirs inherit it.
    let token_base = crate::platform::app_config_dir().map(|d| d.join("token"));
    if let Some(base) = &token_base {
        let _ = create_private_dir(base);
    }

    macro_rules! build {
        // Anthropic: enable automatic prompt caching with 1-hour TTL so the
        // static prefix (system prompt + tool definitions + growing conversation
        // history) is cached server-side for the duration of an interactive
        // session and billed as cache reads instead of full input tokens on
        // every turn. The API advances the cache breakpoint automatically.
        (anthropic $client:expr) => {{
            let cached_model = $client
                .completion_model(&model)
                .with_automatic_caching_1h();
            build!(@builder AgentBuilder::new(cached_model))
        }};
        // OpenRouter: enable prompt caching so the system-prompt prefix is
        // marked with `cache_control: {"type": "ephemeral"}` and billed at the
        // cache-hit rate on subsequent turns. Also pins to a preferred upstream
        // provider so cache state accumulates across turns instead of being
        // invalidated by load-balancing between multiple providers for the same
        // model slug (e.g. `anthropic/claude-sonnet-4` routed to different
        // upstreams on different turns).
        (openrouter $client:expr) => {{
            let cached_model = $client
                .completion_model(&model)
                .with_prompt_caching();
            let merged = merge_openrouter_params(reasoning_json.clone());
            build!(@builder_with_params AgentBuilder::new(cached_model), merged)
        }};
        ($client:expr) => {{
            build!(@builder $client.agent(&model))
        }};
        (@builder $builder:expr) => {{
            build!(@builder_with_params $builder, reasoning_json.clone())
        }};
        (@builder_with_params $builder:expr, $params:expr) => {{
            let rig_agent = build_rig_agent(
                $builder,
                config.yolo,
                config.output.clone(),
                config.confirm_port.clone(),
                config.system_message_extension,
                config.persona_prompt,
                config.bash_auto_allow.unwrap_or_default(),
                config.agent_prompt,
                $params,
                config.params.temperature,
                config.sandbox.clone(),
            );

            let message_logger = if let Some(path) = &config.message_log_path {
                Some(Arc::new(MessageLogger::new(path.clone(), config.provider.clone(), model.clone())))
            } else {
                None
            };

            CodeAgent::new(
                rig_agent,
                config.output,
                config.context_limit,
                config.tool_call_limit,
                config.provider,
                model,
                config.params,
                config.yolo,
                config.persona_name,
                config.user_cancel.clone(),
                config
                    .confirm_port
                    .as_ref()
                    .map(|p| p.approvals().clone())
                    .unwrap_or_default(),
                config.cancel_notify.clone(),
                config.reasoning_in_context,
                message_logger,
            )
        }};
    }

    macro_rules! check_env {
        ($var:expr) => {
            if std::env::var($var).is_err() {
                return Err(crate::PicocodeError::MissingApiKey(
                    provider.to_string(),
                    $var.to_string(),
                ));
            }
        };
        ($($var:expr),+ $(,)?) => {
            if !( $( std::env::var($var).is_ok() )||+ ) {
                return Err(crate::PicocodeError::MissingApiKey(
                    provider.to_string(),
                    vec![$($var.to_string()),+].join(" or "),
                ));
            }
        };
    }

    let agent: CodeAgent = match provider.as_str() {
        "anthropic" => {
            check_env!("ANTHROPIC_API_KEY");
            build!(anthropic anthropic::Client::from_env()?)
        }
        "openai" => {
            check_env!("OPENAI_API_KEY");
            build!(openai::Client::from_env()?)
        }
        "azure" => {
            check_env!("AZURE_OPENAI_API_KEY");
            check_env!("AZURE_OPENAI_ENDPOINT");
            build!(azure::Client::from_env()?)
        }
        "cohere" => {
            check_env!("COHERE_API_KEY");
            build!(cohere::Client::from_env()?)
        }
        "chatgpt" => {
            check_env!("CHATGPT_ACCESS_TOKEN");
            let token = std::env::var("CHATGPT_ACCESS_TOKEN").unwrap();
            if token == OAUTH_TOKEN {
                let dir = token_base.as_ref().map(|b| b.join("chatgpt"));
                if let Some(d) = &dir {
                    let _ = create_private_dir(d);
                }
                // Device-code prompt is surfaced by rig's default handler (the
                // `DeviceCodePrompt` type is private, so a custom stderr handler
                // can't be passed without a rig change). `token_dir` routes the
                // cached refresh token into the settings `token` sub-directory.
                let client = match dir {
                    Some(d) => chatgpt::Client::builder()
                        .oauth()
                        .token_dir(d)
                        .allow_device_flow(true)
                        .build()
                        .map_err(rig_core::client::ProviderClientError::from)?,
                    None => chatgpt::Client::builder()
                        .oauth()
                        .allow_device_flow(true)
                        .build()
                        .map_err(rig_core::client::ProviderClientError::from)?,
                };
                build!(client)
            } else {
                build!(chatgpt::Client::from_env()?)
            }
        }
        "copilot" => {
            check_env!("GITHUB_COPILOT_API_KEY", "COPILOT_GITHUB_ACCESS_TOKEN");
            let oauth = ["GITHUB_COPILOT_API_KEY", "COPILOT_GITHUB_ACCESS_TOKEN"]
                .iter()
                .any(|v| std::env::var(v).as_deref() == Ok(OAUTH_TOKEN));
            if oauth {
                let dir = token_base.as_ref().map(|b| b.join("copilot"));
                if let Some(d) = &dir {
                    let _ = create_private_dir(d);
                }
                let client = match dir {
                    Some(d) => copilot::Client::builder()
                        .oauth()
                        .token_dir(d)
                        .allow_device_flow(true)
                        .build()
                        .map_err(rig_core::client::ProviderClientError::from)?,
                    None => copilot::Client::builder()
                        .oauth()
                        .allow_device_flow(true)
                        .build()
                        .map_err(rig_core::client::ProviderClientError::from)?,
                };
                build!(client)
            } else {
                build!(copilot::Client::from_env()?)
            }
        }
        "deepseek" => {
            check_env!("DEEPSEEK_API_KEY");
            build!(deepseek::Client::from_env()?)
        }
        "doubleword" => {
            check_env!("DOUBLEWORD_API_KEY");
            build!(doubleword::Client::from_env()?)
        }
        "gemini" | "google" => {
            check_env!("GOOGLE_API_KEY");
            build!(gemini::Client::from_env()?)
        }
        "groq" => {
            check_env!("GROQ_API_KEY");
            build!(groq::Client::from_env()?)
        }
        "huggingface" => {
            check_env!("HF_TOKEN");
            build!(huggingface::Client::from_env()?)
        }
        "hyperbolic" => {
            check_env!("HYPERBOLIC_API_KEY");
            build!(hyperbolic::Client::from_env()?)
        }
        "llamafile" | "llamacpp" => {
            // llamacpp's client defaults to `http://localhost:8080`; the
            // `LLAMACPP_API_BASE_URL` env var is optional.
            build!(llamacpp::Client::from_env()?)
        }
        "mira" => {
            check_env!("MIRA_API_KEY");
            build!(mira::Client::from_env()?)
        }
        "minimax" => {
            check_env!("MINIMAX_API_KEY");
            build!(minimax::Client::from_env()?)
        }
        "mistral" => {
            check_env!("MISTRAL_API_KEY");
            build!(mistral::Client::from_env()?)
        }
        "moonshot" => {
            check_env!("MOONSHOT_API_KEY");
            build!(moonshot::Client::from_env()?)
        }
        "ollama" => {
            if std::env::var("OLLAMA_API_BASE_URL").is_err() {
                std::env::set_var("OLLAMA_API_BASE_URL", "http://localhost:11434");
            }
            build!(ollama::Client::from_env()?)
        }
        "openrouter" => {
            check_env!("OPENROUTER_API_KEY");
            build!(openrouter openrouter::Client::from_env()?)
        }
        "perplexity" => {
            check_env!("PERPLEXITY_API_KEY");
            build!(perplexity::Client::from_env()?)
        }
        "together" => {
            check_env!("TOGETHER_API_KEY");
            build!(together::Client::from_env()?)
        }
        "xai" => {
            check_env!("XAI_API_KEY");
            build!(xai::Client::from_env()?)
        }
        "xiaomimimo" => {
            check_env!("XIAOMI_MIMO_API_KEY");
            build!(xiaomimimo::Client::from_env()?)
        }
        "zai" => {
            check_env!("ZAI_API_KEY");
            // OpenAI-compatible endpoint — enable prompt caching so the
            // system prompt and last message are marked with cache_control.
            // Effective when the upstream model/gateway supports it.
            let cached_model = zai::Client::from_env()?
                .completion_model(&model)
                .with_prompt_caching();
            build!(@builder AgentBuilder::new(cached_model))
        }
        // ZAI Anthropic-compatible endpoint (ANTHROPIC_API_BASE_URL). Uses
        // the Anthropic provider with automatic prompt caching. Enable by
        // setting `ZAI_API_BASE=https://api.z.ai/api/anthropic` in the env
        // (or the config provider's `env` block) and using provider `zai`.
        // The `zai-anthropic` alias uses the Anthropic-compatible client
        // directly so automatic caching is available regardless of base URL.
        "zai-anthropic" => {
            check_env!("ZAI_API_KEY");
            let cached_model = zai::AnthropicClient::from_env()?
                .completion_model(&model)
                .with_automatic_caching();
            build!(@builder AgentBuilder::new(cached_model))
        }
        _ => {
            return Err(crate::PicocodeError::Other(format!(
                "Unsupported provider: {}",
                provider
            )))
        }
    };

    Ok(agent)
}

pub fn load_agents_md() -> Option<String> {
    let path = std::path::Path::new("AGENTS.md");
    if path.exists() {
        return std::fs::read_to_string(path).ok();
    }
    None
}

#[derive(Clone)]
struct LoggingHook {
    output: Arc<dyn Output>,
    user_cancel: Arc<AtomicBool>,
    message_logger: Option<Arc<MessageLogger>>,
    reasoning_in_context: crate::settings::ReasoningInContextMode,
    provider: String,
    model: String,
    /// Run-scoped canonical transcript accumulator; see [`TurnTranscript`].
    /// Hooks and the prompt loop both append under this mutex in stream
    /// order, so the transcript mirrors rig's own `new_messages`.
    transcript: Arc<Mutex<TurnTranscript>>,
}

impl AgentHook for LoggingHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        if self.user_cancel.load(Ordering::SeqCst) {
            return CompletionCallAction::Stop("cancelled by user".into());
        }
        // Build the history to send: strip reasoning when configured, then
        // render `edit` tool calls in the model-facing block form. The
        // transcript is stored canonically (a pi-compatible array); the model is
        // shown the block form *consistently* so it keeps emitting the reliable
        // shape. This hook is the one place that sees the whole request history,
        // so it covers prior and within-turn messages alike.
        let stripped_history = maybe_strip_reasoning(
            event.history.to_vec(),
            self.reasoning_in_context,
            &self.provider,
            &self.model,
        );
        let history = split_multi_file_edit_calls(blob_edit_tool_calls(stripped_history));

        // `--log-messages`: capture the exact context sent to the model on this
        // call — the (stripped, block-form) history plus the current prompt —
        // under one turn id shared with the matching response. Logging `history`
        // reflects what the provider actually receives.
        if let Some(logger) = &self.message_logger {
            let turn_id = logger.advance_turn();
            for m in &history {
                logger.log_message("sent", m, turn_id).await;
            }
            logger.log_message("sent", event.prompt, turn_id).await;
        }

        // Always send via a patch: it replaces the run's history with exactly
        // `history`. `maybe_strip_reasoning` also handles the Anthropic-family
        // thinking+tool_use replay contract (reasoning is force-kept on turns
        // with tool calls). Identity for every message without an `edit` call.
        CompletionCallAction::Patch(RequestPatch::new().history(history))
    }

    async fn on_completion_response(
        &self,
        _ctx: &HookContext,
        _event: CompletionResponseEvent<'_>,
    ) -> ObservationAction {
        // Note: rig only fires `on_completion_response` on the *blocking*
        // surface. The streaming surface (which picocode uses via
        // `stream_prompt`) fires `on_model_turn_finished` instead, so the
        // received-message logging lives there — see below.
        if self.user_cancel.load(Ordering::SeqCst) {
            ObservationAction::Stop("cancelled by user".into())
        } else {
            ObservationAction::Continue
        }
    }

    async fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        if self.user_cancel.load(Ordering::SeqCst) {
            ModelTurnAction::Stop("cancelled by user".into())
        } else {
            // `--log-messages`: capture the assistant content received for
            // this turn, tagged with the same turn id as the request that
            // produced it. Fires on both the blocking and streaming surfaces
            // (the streaming one never calls `on_completion_response`), so
            // this is the single place the received side is logged.
            if let Some(logger) = &self.message_logger {
                let msg = Message::Assistant {
                    id: event.identity.message_id.clone(),
                    content: event.content.clone(),
                };
                let turn_id = logger.current_turn_id();
                logger.log_message("received", &msg, turn_id).await;
            }
            // Accumulate the accepted turn into the run transcript so a
            // mid-turn failure can persist exactly what completed (see
            // `TurnTranscript` / doc/tool_error_handling.md). This branch
            // returns `Continue`, i.e. the turn was accepted — matching rig's
            // own history commit for the same content.
            if let Ok(mut t) = self.transcript.lock() {
                t.push_assistant_turn(event.identity.message_id.clone(), event.content.clone());
            }
            ModelTurnAction::Continue
        }
    }

    async fn on_tool_call(&self, _ctx: &HookContext, event: ToolCall<'_>) -> ToolCallAction {
        if self.user_cancel.load(Ordering::SeqCst) {
            return ToolCallAction::Stop("cancelled by user".into());
        }
        let mut args_json = serde_json::from_str(event.args)
            .unwrap_or(serde_json::Value::String(event.args.to_string()));
        // Canonicalize `edit` arguments to the array form for display, so
        // `get_preview` sees the same shape it gets on replay (the parser stays
        // out of `output.rs`). No-op for other tools / already-canonical args.
        if event.tool_name == "edit" {
            args_json = crate::edit_blocks::canonicalize_edit_value(&args_json);
        }
        self.output.display_tool_call(event.tool_name, &args_json);
        ToolCallAction::Run
    }

    /// Model called a tool that isn't registered or isn't in the current
    /// turn's allowed set. Rig gates this *before* `on_tool_call`, so the
    /// hook is the only place that can recover without burning a fresh
    /// model call. We re-prompt the model with the available tool set so
    /// a hallucinated name (the common case) self-corrects on the first
    /// retry. The recovery budget is configured on the streaming request
    /// via `.max_invalid_tool_call_retries(1)`; if the model hallucinates
    /// the same wrong name twice, rig exhausts the budget and the run
    /// surfaces a `LlmUnknownTool` `PicocodeError` to the user.
    ///
    /// Reference: rig `AgentHook::on_invalid_tool_call` (hook.rs:1218) +
    /// `InvalidToolCallAction::{Retry, Repair, Skip, Stop, Fail}` (run
    /// `policy.rs:49`).
    async fn on_invalid_tool_call(
        &self,
        _ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        if self.user_cancel.load(Ordering::SeqCst) {
            return Some(InvalidToolCallAction::stop("cancelled by user"));
        }
        // Surface the issue to the user *before* the recovery attempt so
        // the in-flight panel shows what the model tried. `display_error`
        // is the existing method; layer 1 Task 1 will add a dedicated
        // `display_tool_error` panel that we can swap in later.
        self.output.display_error(&format!(
            "Model called tool `{}` which is not available. Available tools: {:?}. \
             Asking the model to retry with a valid name.",
            event.tool_name, event.available_tools
        ));
        Some(InvalidToolCallAction::retry(format!(
            "tool `{}` is not available. Available tools: {:?}. \
             Please re-emit the call with one of the available tool names.",
            event.tool_name, event.available_tools
        )))
    }

    async fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> ToolResultAction {
        if self.user_cancel.load(Ordering::SeqCst) {
            return ToolResultAction::Stop("cancelled by user".into());
        }
        self.output
            .display_tool_result(&event.raw_result.output().render());
        if let Some(text) = event.raw_result.output().as_text() {
            if let Some(cleaned) = model_facing_tool_text(event.tool_name, text) {
                return ToolResultAction::rewrite(cleaned);
            }
        }
        ToolResultAction::Keep
    }
}

/// The text the model should see for a tool result, or `None` for "leave it
/// alone".
///
/// The `edit` tool's argument errors are a self-correcting message meant to be
/// read by the model, but they arrive wrapped in two redundant prefixes (rig's
/// `"failed to parse tool arguments: "` and the tool's own `"Invalid edit tool
/// arguments: "`). Stripping them leaves the model only the problem, the
/// expected block shape, and the offending structure — a weak model is
/// measurably likelier to self-correct from a terse message than from one buried
/// in ceremony, and the "something went wrong, let me try python" fallback starts
/// with exactly such a wall.
///
/// Only the *text* changes. The result's kind, retryability, and raw structured
/// form are untouched, and a non-`edit` tool (or a message without the known
/// prefixes) is returned unchanged, so this can never swallow content.
fn model_facing_tool_text<'a>(tool_name: &str, text: &'a str) -> Option<&'a str> {
    if tool_name != "edit" {
        return None;
    }
    let cleaned = blockedit::strip_arg_error_prefixes(text);
    (cleaned != text).then_some(cleaned)
}

const DEFAULT_AGENT_PROMPT: &str = r#"You are picocode, a software engineering agent. You are direct, technical, and highly efficient.

Your mission is to assist the user in their development tasks by utilizing a set of specialized tools. You operate within a specific codebase and must maintain its integrity while delivering high-quality solutions.

### WORKFLOW & STRATEGY
1. **Understand Before Acting**: Always ground your responses in verified facts. Use `ls` to see the structure and `read` or `grep` to understand existing logic and patterns.
2. **Be Precise**: When editing files, use `edit`. Leverage its block syntax, batch support, and line anchoring. For batch edits, use the `sed preview` tool - it gives you clean, disambiguated blocks to review and modify, which you then feed to the edit tool. Avoid editing with ad-hoc sed or python scripts - you are likely to make mistakes. If you MUST use them, work on copies of files, or take git snapshots to protect existing work.
3. **Verify Everything**: After modifying code, verify the results. Run tests or build commands via `bash`. Read the modified file to ensure the change was applied correctly.
4. **Tool Mastery**:
   - `read`: Use to read code. Note that it provides line numbers (e.g., `  10| code`). These are for your reference only; do not include them in your output or when writing files.
   - `bash`: Your window to the system. Use it for compilation, testing, and complex automation.
   - `w3m in the shell`: Use for external documentation, searching for solutions (use duckduckgo.com), or web-related debugging.
5. **Context**: You are working in the directory provided below. All paths are relative to this directory. Use relative paths in bash and other tools for token economy and session portability.
6. Ask questions, and do not change designs without approval.

### GUIDING PRINCIPLES
- **Clean Code**: Follow established patterns in the codebase. Write idiomatic, readable, and well-documented code.
- **Security First**: Be vigilant about security vulnerabilities. Sanitize inputs, avoid hardcoded secrets, and follow least-privilege principles.
- **Minimalism**: Don't add unnecessary dependencies or over-engineer solutions.
- **Communication**: Keep explanations brief and focused on the "how" and "why" of your technical decisions. If you are unsure, do not guess or over-analyze - ask.
"#;

/// Appended to the system message when the `sed` tool is available.
///
/// This replaces, rather than supplements, the standing advice to avoid sed: a
/// preview tool makes `sed -i` unnecessary for the cases that advice was guarding
/// against (line ranges computed against a stale read), while `sed -i` itself
/// remains exactly as dangerous as it was.
const SED_TOOL_PROMPT: &str = r#"### Bulk edits: preview with `sed`, apply with `edit`
For a change that spans many lines or the whole file (renames, deletions, range
rewrites), do NOT run `sed -i` and do NOT hand-write one `edit` block per line.
Instead call `sed` with one GNU sed expression per line. It never writes: it
returns the change as `@@@OLD`/`@@@NEW` blocks, each anchored to a line. Read the
blocks, then pass them to `edit` as its `blocks` argument to apply. `sed` refuses
`w`, `e` and `q`; use `edit` or `bash` for those. Pass `context` if you need more
than the ~2 lines of trailing context you get by default."#;

fn build_rig_agent(
    builder: AgentBuilder<NoToolConfig>,
    yolo: bool,
    output: Arc<dyn Output>,
    confirm_port: Option<crate::confirm::ConfirmPort>,
    system_message_extension: Option<String>,
    persona_prompt: Option<String>,
    bash_auto_allow: Vec<String>,
    agent_prompt: Option<String>,
    additional_params: Option<serde_json::Value>,
    temperature: Option<f64>,
    sandbox: Option<crate::config::SandboxConfig>,
) -> Agent {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut system_message = agent_prompt.unwrap_or_else(|| {
        format!(
            "{}\n\nCurrent working directory: {}",
            DEFAULT_AGENT_PROMPT, cwd
        )
    });
    if let Some(persona) = persona_prompt {
        system_message = format!("{}\n\n{}", persona, system_message);
    }
    if let Some(ext) = system_message_extension {
        system_message.push_str("\n\n");
        system_message.push_str(&ext);
    }
    // Only describe the `sed` tool when it is actually registered below, so the
    // prompt never advertises a tool the model cannot call.
    if bsed_available() {
        system_message.push_str("\n\n");
        system_message.push_str(SED_TOOL_PROMPT);
    }

    let mut builder = builder
        .preamble(&system_message)
        .tool(ReadFile)
        .tool(WriteFile)
        .tool(EditFile)
        .tool(GlobFiles)
        .tool(GrepText)
        .tool(ListDir);

    // `sed` previews changes; it never writes. Advertised only when the `bsed`
    // binary is present, because that binary is what runs the script -- a tool
    // that cannot run is worse than no tool. A separate tool, not another mode
    // of `edit`, so the model has an explicit review step between "describe the
    // change" and "apply it".
    if bsed_available() {
        builder = builder.tool(SedPreview);
    }

    if let Some(params) = additional_params {
        builder = builder.additional_params(params);
    }
    // Temperature rides the typed `CompletionRequest` field, so rig spells it
    // per provider (flat for OpenAI/Anthropic, `generationConfig.temperature`
    // for Gemini). `None` leaves the provider's own default in place — we
    // never send a default we merely know about.
    if let Some(t) = temperature {
        builder = builder.temperature(t);
    }

    let confirm_port = confirm_port.clone();
    builder = builder
        .tool(guard(
            MakeDir,
            yolo,
            output.clone(),
            None,
            confirm_port.clone(),
        ))
        .tool(guard(
            Remove,
            yolo,
            output.clone(),
            None,
            confirm_port.clone(),
        ))
        .tool(guard(
            MoveFile,
            yolo,
            output.clone(),
            None,
            confirm_port.clone(),
        ))
        .tool(guard(
            CopyFile,
            yolo,
            output.clone(),
            None,
            confirm_port.clone(),
        ));

    let auto_allow = bash_auto_allow.clone();
    // Publish the resolved sandbox config for the shell tools. picocode runs a
    // single agent per process, so a process-wide slot is sufficient and keeps
    // the macro-generated tool structs untouched.
    set_sandbox(sandbox.clone());
    builder = builder.tool(guard(
        Bash,
        yolo,
        output.clone(),
        Some(Arc::new(move |args| {
            auto_allow.iter().any(|pattern| {
                regex::Regex::new(pattern)
                    .map(|re| re.is_match(&args.command))
                    .unwrap_or(false)
            })
        })),
        confirm_port.clone(),
    ));

    if is_tool_available("agent-browser") {
        builder = builder.tool(guard(
            AgentBrowser,
            yolo,
            output.clone(),
            None,
            confirm_port.clone(),
        ));
    }
    builder.build()
}

use rig_core::tool::{Tool, ToolExecutionError};

struct Guard<T: Tool> {
    tool: T,
    yolo: bool,
    output: Arc<dyn Output>,
    /// The REPL's end of the confirm port, `None` where there is no REPL to
    /// ask (piped/batch, one-shot `run_once`).
    ///
    /// With a port, a gated call *awaits* the REPL's answer and the turn's
    /// `select!` keeps running throughout (doc/confirm_as_menu_261002.md).
    confirm: Option<crate::confirm::ConfirmPort>,
    /// Session approval for the no-port path only — see [`Self::call`]. The
    /// port carries its own (`Approvals`), so with a REPL this is never read.
    always: Arc<AtomicBool>,
    auto_approve: Option<Arc<dyn Fn(&T::Args) -> bool + Send + Sync>>,
}

impl<T: Tool> Guard<T> {
    /// Whether this tool is already cleared for the session.
    ///
    /// With a port that is the REPL's `Approvals` store — the one place an
    /// *always* choice is recorded, so the agent never sees the third option.
    /// Without one it falls back to this guard's own flag, which is what keeps
    /// the piped `Always` answer meaningful now that there is no dialog.
    fn already_approved(&self) -> bool {
        match &self.confirm {
            Some(port) => port.approvals().is_approved(T::NAME),
            None => self.always.load(Ordering::Relaxed),
        }
    }
}

impl<T: Tool<Error = crate::tools::ToolError>> Tool for Guard<T> {
    type Args = T::Args;
    type Output = T::Output;
    type Error = T::Error;

    const NAME: &'static str = T::NAME;

    fn description(&self) -> String {
        self.tool.description()
    }

    fn parameters(&self) -> serde_json::Value {
        self.tool.parameters()
    }

    /// Map picocode's domain errors into rig's typed error taxonomy so the
    /// model and hooks receive a meaningful classification instead of always
    /// `ToolErrorKind::Other` (which renders as the generic "the tool failed").
    ///
    /// One table, shared with the tools that are registered without a guard
    /// (`EditFile`): a wrapped and an unwrapped tool must not classify the same
    /// error differently.
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        crate::tools::map_tool_error(error)
    }

    async fn call(
        &self,
        context: &mut ToolContext,
        args: Self::Args,
    ) -> std::result::Result<Self::Output, Self::Error> {
        let should_auto_approve = self
            .auto_approve
            .as_ref()
            .map(|f| f(&args))
            .unwrap_or(false);

        if !self.yolo && !should_auto_approve && !self.already_approved() {
            let message = format!("Confirm tool {} call?", Self::NAME.to_uppercase());
            let decision = match &self.confirm {
                // The REPL decides, and it answers `Accept` for *both* `yes`
                // and `always` — the approval it records never reaches this
                // side. Nothing here blocks: the turn's loop keeps pumping
                // keys while this call is parked.
                Some(port) => port.request(Self::NAME, message).await,
                // No REPL: the output's own synchronous policy, and *this* flag
                // is the only thing that can remember an `Always` — a piped
                // session's "s" (session) has no port to record it in.
                None => match self.output.confirm_default(&message) {
                    Confirmation::Always => {
                        self.always.store(true, Ordering::Relaxed);
                        crate::confirm::ConfirmDecision::Accept
                    }
                    Confirmation::Yes => crate::confirm::ConfirmDecision::Accept,
                    Confirmation::No => crate::confirm::ConfirmDecision::Deny,
                },
            };
            if decision == crate::confirm::ConfirmDecision::Deny {
                return Err(crate::tools::ToolError::Generic(
                    "Action cancelled by user".into(),
                ));
            }
        }
        self.tool.call(context, args).await
    }
}

fn guard<T: Tool>(
    tool: T,
    yolo: bool,
    output: Arc<dyn Output>,
    auto_approve: Option<Arc<dyn Fn(&T::Args) -> bool + Send + Sync>>,
    confirm: Option<crate::confirm::ConfirmPort>,
) -> Guard<T> {
    Guard {
        tool,
        yolo,
        output,
        confirm,
        always: Arc::new(AtomicBool::new(false)),
        auto_approve,
    }
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

// ---------------------------------------------------------------------------
// Session replay helpers
// ---------------------------------------------------------------------------

/// One assistant segment: the content streamed between tool executions (or the
/// whole final answer when there are no more tools to run in this turn).
pub(crate) struct AssistantSegment {
    /// Reasoning/thinking blocks in message order.
    thinking: Vec<String>,
    /// Answer text blocks in message order.
    text: Vec<String>,
    /// Tool calls from the assistant message: `(call_id, name, args)`.
    /// Populated during grouping; matched against results when the following
    /// `User { ToolResult }` message is processed.
    tool_calls: Vec<(String, String, serde_json::Value)>,
    /// Tool results keyed by call ID: `(call_id, text)`.
    /// Populated when the subsequent `User { ToolResult }` message is seen.
    tool_results: Vec<(String, String)>,
}

impl AssistantSegment {
    fn new() -> Self {
        Self {
            thinking: Vec::new(),
            text: Vec::new(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }
}

/// One complete conversation turn: the user's text prompt followed by the
/// full chain of assistant segments (`text/reasoning → tool block → …`).
pub(crate) struct ConversationTurn {
    user_text: String,
    segments: Vec<AssistantSegment>,
}

/// One replayable item from the raw session branch.
///
/// Replay walks the FULL branch (root→leaf) so compaction entries render as
/// visible markers while the conversation replays all the way to the
/// beginning; only the agent context is prefix-limited at the compaction
/// scope (see `Session::build_context` / `apply_compaction_transforms`).
pub(crate) enum ReplayUnit {
    Turn(ConversationTurn),
    /// A compaction boundary marker. Everything on the branch after this
    /// entry is what the agent sees (it was written with `firstKeptEntryId`).
    /// `summary` is the compaction's summary text (empty for `manual`
    /// mode on a no-file span); when non-empty it is rendered as part
    /// of the marker.
    Compaction {
        tokens_before: u64,
        summary: String,
    },
    /// A branch-summary marker: the digest `/tree <id> switch summarize`
    /// writes when it moves the cursor off a spur. Not a turn — the marker
    /// renders in path order with `summary` as markdown below it, exactly
    /// like a compaction. `from_id` is the branch-from position (pi's
    /// `fromId`), surfaced in the marker so the id is copy-pasteable into
    /// `/tree <id>`.
    BranchSummary {
        from_id: String,
        summary: String,
    },
}

/// Build replay units from the raw session branch (root->leaf).
///
/// Replay walks the FULL branch so compaction entries become
/// `ReplayUnit::Compaction` markers interleaved with turns, giving the user
/// the whole history while only the agent context is prefix-limited at the
/// compaction scope. Tool-result entries are batched into the preceding
/// assistant segment exactly like `extract_messages`/`group_into_turns`, so an
/// uncompacted branch produces the same turns as the old context-based path.
fn build_replay_units(branch: &[crate::session::SessionEntry]) -> Vec<ReplayUnit> {
    use crate::session::SessionEntryContent;

    let mut units: Vec<ReplayUnit> = Vec::new();
    let mut open: Option<ConversationTurn> = None;
    // Pending tool-result pi messages (batched into the next assistant flush).
    let mut pending_tools: Vec<serde_json::Value> = Vec::new();

    fn flush_pending_tools(
        pending: &mut Vec<serde_json::Value>,
        open: &mut Option<ConversationTurn>,
    ) {
        if pending.is_empty() {
            return;
        }
        if let Some(turn) = open.as_mut() {
            let mut seg_vec = Vec::new();
            if let Some(seg) = turn.segments.last_mut() {
                let trs = std::mem::take(pending);
                for tr in trs {
                    // Reuse the pi -> rig tool-result conversion (pub(crate)).
                    if let Some(Message::User { content }) =
                        crate::session::pi_toolresults_to_rig_message(vec![tr])
                    {
                        seg_vec.extend(content);
                    }
                }
                for item in seg_vec {
                    if let UserContent::ToolResult(tr) = item {
                        let text = tr
                            .content
                            .iter()
                            .filter_map(|rc| match rc {
                                ToolResultContent::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        seg.tool_results.push((tr.call.clone().into_string(), text));
                    }
                }
            }
        }
        pending.clear();
    }

    for entry in branch {
        match &entry.content {
            SessionEntryContent::Message { message } => {
                let role = message.get("role").and_then(|r| r.as_str());
                match role {
                    Some("toolResult") => pending_tools.push(message.clone()),
                    Some("user") | Some("assistant") => {
                        // End of a turn boundary? A user text message starts a
                        // new turn; flush pending tool results first.
                        if role == Some("user") {
                            flush_pending_tools(&mut pending_tools, &mut open);
                            if let Some(txt) = user_text_of(message) {
                                if let Some(t) = open.take() {
                                    units.push(ReplayUnit::Turn(t));
                                }
                                open = Some(ConversationTurn {
                                    user_text: txt,
                                    segments: Vec::new(),
                                });
                            }
                        } else {
                            // Assistant: attach to current turn (flush tools first).
                            flush_pending_tools(&mut pending_tools, &mut open);
                            if let Some(msg) = crate::session::pi_message_to_rig(message) {
                                feed_turn_into(&mut open, &msg);
                            }
                        }
                    }
                    _ => {}
                }
            }
            SessionEntryContent::Compaction {
                tokens_before,
                summary,
                ..
            } => {
                // A compaction boundary comes AFTER the turn it concludes:
                // flush any open turn first, then emit the marker.
                flush_pending_tools(&mut pending_tools, &mut open);
                if let Some(t) = open.take() {
                    units.push(ReplayUnit::Turn(t));
                }
                units.push(ReplayUnit::Compaction {
                    tokens_before: *tokens_before,
                    summary: summary.clone(),
                });
            }
            // Same placement rule as a compaction: the marker sits after the
            // turn it follows, before the prompt that continues on this branch.
            // A blank digest matches no arm — pi's context mapping skips an
            // empty summary, a marker with no body would be a fiction, and
            // `apply_branch_summary` refuses a blank draft anyway.
            SessionEntryContent::BranchSummary { from_id, summary }
                if !summary.trim().is_empty() =>
            {
                flush_pending_tools(&mut pending_tools, &mut open);
                if let Some(t) = open.take() {
                    units.push(ReplayUnit::Turn(t));
                }
                units.push(ReplayUnit::BranchSummary {
                    from_id: from_id.clone(),
                    summary: summary.clone(),
                });
            }
            _ => {}
        }
    }
    flush_pending_tools(&mut pending_tools, &mut open);
    if let Some(t) = open.take() {
        units.push(ReplayUnit::Turn(t));
    }
    units
}

/// Extract the first text block of a pi user AgentMessage.
fn user_text_of(message: &serde_json::Value) -> Option<String> {
    message
        .pointer("/content/0/text")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Feed one assistant rig Message into the open turn (assistant segments
/// always belong to the current turn).
fn feed_turn_into(open: &mut Option<ConversationTurn>, msg: &Message) {
    let Some(turn) = open.as_mut() else { return };
    let Message::Assistant { content, .. } = msg else {
        return;
    };
    let mut seg = AssistantSegment::new();
    for item in content.iter() {
        match item {
            AssistantContent::Reasoning(r) => {
                let t: String = r
                    .content
                    .iter()
                    .filter_map(|rc| match rc {
                        ReasoningContent::Text { text, .. } => Some(text.as_str()),
                        ReasoningContent::Summary(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !t.is_empty() {
                    seg.thinking.push(t);
                }
            }
            AssistantContent::Text(t) if !t.text.is_empty() => {
                seg.text.push(t.text.clone());
            }
            AssistantContent::ToolCall(tc) => {
                seg.tool_calls.push((
                    tc.id.clone().into_string(),
                    tc.function.name.clone(),
                    tc.function.arguments.clone(),
                ));
            }
            _ => {}
        }
    }
    turn.segments.push(seg);
}

/// Render a single `ConversationTurn` through the Output event sequence
/// (identical to the live streaming path).
///
/// The prompt's rules are the committed prompt's own, so this no longer
/// decides whether a separator precedes it: a prompt straight after another
/// commit shares the rule it is already closed with, and one that opens a
/// turn draws its own. The rule that used to sit between the prompt and the
/// response is that closing rule too — `begin_response` then puts the blank
/// line on it, which is the spacing a turn's prompt and its output want.
fn render_turn(output: &dyn Output, turn: &ConversationTurn) {
    output.display_committed_prompt(&turn.user_text);

    if turn.segments.is_empty() {
        return;
    }

    output.begin_response();

    let seg_count = turn.segments.len();
    for (seg_idx, segment) in turn.segments.iter().enumerate() {
        let is_last_segment = seg_idx + 1 == seg_count;

        for thinking in &segment.thinking {
            output.stream_thinking(thinking);
        }
        for text in &segment.text {
            output.stream_text(text);
        }

        if !segment.tool_calls.is_empty() {
            output.end_response_segment();
            for (call_id, name, args) in &segment.tool_calls {
                output.display_tool_call(name, args);
                if let Some((_, result_text)) =
                    segment.tool_results.iter().find(|(rid, _)| rid == call_id)
                {
                    output.display_tool_result(result_text);
                }
            }
            // Mirror the live path, where each `CompletionCall` — including one
            // that closes a tool round and is followed by more assistant text —
            // fires `end_response_segment`. Flushing after the tool block here
            // guarantees a blank line separates the tool result from any text
            // segment that follows it in the replay, exactly as in live output.
            if !is_last_segment {
                output.end_response_segment();
            }
        } else if !is_last_segment {
            output.end_response_segment();
        }
    }
    output.end_response();
}

/// Replay a slice of units starting at `start`. Compaction markers render as a
/// dim system notice between turns; `Nt`/`Nl` limits count TURNS (markers are
/// not turns and are always drawn when inside the selected window).
fn replay_units(output: &dyn Output, units: &[ReplayUnit], start: usize) {
    // One frame re-emit for the whole replay, not one per line of history
    // (ui_draw_v2 R5). All three callers — `/replay`, resume, and the silent
    // Ctrl-Z restore — arrive here, so this is the one place the batching has
    // to be declared.
    //
    // Deliberately *not* around live streaming: `stream_text` enters the same
    // writer once per delta, and there the per-write re-emit is what keeps the
    // input row visible while text flows past it.
    let _batch = output.begin_batch();
    // No leading-separator bookkeeping: a turn's prompt brackets itself, and
    // the rule that used to be forced between turns *after* the first is
    // the closing rule of the turn above. Compaction markers are not turns
    // and never forced one.
    for unit in &units[start..] {
        match unit {
            ReplayUnit::Turn(turn) => {
                render_turn(output, turn);
            }
            ReplayUnit::Compaction {
                tokens_before,
                summary,
            } => {
                output.display_system(&format!("[compaction: {}k tokens]", tokens_before / 1000,));
                // A text-only compaction carries its summary — render it as
                // markdown right below the marker so the compaction point is
                // self-documenting in the replay, exactly as it was shown
                // immediately after the compaction was performed.
                if !summary.is_empty() {
                    output.display_markdown(summary);
                }
            }
            ReplayUnit::BranchSummary { from_id, summary } => {
                // The id is opaque but copy-pasteable: `/tree <id>` switches
                // there. Same shape as a compaction marker — dim system line,
                // then the digest as markdown.
                output.display_system(&format!("[branch summary from {}]", from_id));
                output.display_markdown(summary);
            }
        }
    }
}

/// Count the conversation turns in a unit list (compaction markers are not
/// turns).
pub(crate) fn count_turns(units: &[ReplayUnit]) -> usize {
    units
        .iter()
        .filter(|u| matches!(u, ReplayUnit::Turn(_)))
        .count()
}

/// Return the unit index where the last `n` turns begin (0 when `n` >= the
/// total). Markers in that window are included in the render — only the
/// leading turns before the window are dropped, matching `/replay N` semantics
/// where the limit applies to turns.
pub(crate) fn last_turns_start(units: &[ReplayUnit], n: usize) -> usize {
    if n == 0 {
        return units.len();
    }
    let mut seen = 0usize;
    let mut idx = units.len();
    while idx > 0 {
        idx -= 1;
        if matches!(units[idx], ReplayUnit::Turn(_)) {
            seen += 1;
            if seen == n {
                break;
            }
        }
    }
    if seen < n {
        0
    } else {
        idx
    }
}

/// Replay the last `n` turns (plus any markers inside that window) from a
/// unit list.
pub(crate) fn replay_last_turns(output: &dyn Output, units: &[ReplayUnit], n: usize) {
    let start = last_turns_start(units, n);
    replay_units(output, units, start);
}

/// Parse a replay-length argument: plain number `N`, `Nt` (turns), or `Nl`
/// (lines).  Returns `(count, is_lines)`.  An empty/whitespace input returns
/// `(default, false)`.
pub(crate) fn parse_replay_len_arg(arg: &str, default: usize) -> (usize, bool) {
    let arg = arg.trim();
    if arg.is_empty() {
        return (default, false);
    }
    if arg.ends_with('l') {
        let n: usize = arg.trim_end_matches('l').trim().parse().unwrap_or(0);
        if n == 0 {
            (usize::MAX, false)
        } else {
            (n, true)
        }
    } else {
        let clean = arg.trim_end_matches('t').trim();
        let n: usize = clean.parse().unwrap_or(default);
        (n, false)
    }
}

/// Execute a `/replay` command: re-render session history.
///
/// Called from both the agent's submit handler and the v2.5 `ReplayNode`
/// (via `block_on`).
pub(crate) async fn handle_replay(
    arg: &str,
    session: &crate::session::Session,
    output: &dyn Output,
) {
    let branch = session.get_branch().await;
    let units = build_replay_units(&branch);
    let total = count_turns(&units);
    if arg.ends_with('l') {
        let n: usize = arg.trim_end_matches('l').trim().parse().unwrap_or(0);
        if n > 0 {
            if total == 0 {
                output.display_text("Session is empty");
            } else {
                output.begin_capture();
                replay_units(output, &units, 0);
                let bytes = output.end_capture();
                let captured = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = captured.lines().collect();
                let start = lines.len().saturating_sub(n);
                use std::io::Write as _;
                let stdout = std::io::stdout();
                let mut handle = stdout.lock();
                for line in &lines[start..] {
                    let _ = writeln!(handle, "{}", line);
                }
                let _ = handle.flush();
            }
        } else {
            replay_units(output, &units, 0);
        }
    } else {
        let (count, _is_lines) = if arg.is_empty() {
            (0, false)
        } else {
            parse_replay_len_arg(arg, 0)
        };
        if total == 0 {
            output.display_text("Session is empty");
        } else {
            let count = if count == 0 { total } else { count.min(total) };
            replay_last_turns(output, &units, count);
        }
    }
}

/// Quiet `/replay 1`: the last turn only — no status messages, no divider of
/// its own, and a no-op when the session has no turns yet. Used by the
/// key-driven resize repaint in `read_input`, which wipes the old frame and
/// replays the last turn to push reflow corruption out of view; that path must
/// stay silent (a session exists from startup, long before it has any turns to
/// replay).
pub(crate) async fn replay_last_turn_silent(
    session: &crate::session::Session,
    output: &dyn Output,
) {
    let branch = session.get_branch().await;
    let units = build_replay_units(&branch);
    if count_turns(&units) == 0 {
        return;
    }
    // No rule of our own: the replayed turn's prompt brackets itself, and an
    // implicit replay after a resize has nothing above it worth separating
    // from. Its closing rule is also what the trailing `display_separator()`
    // used to paint — otherwise the repainted input sat flush against the
    // replayed turn — so the prompt's own bracket covers it.
    replay_last_turns(output, &units, 1);
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

/// Summary mode for `/tree compact`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactSummaryMode {
    /// User-editable summary: the dropped span is scanned for touched files
    /// (split into `Documents` = `*.md` and `Files` = everything else) and
    /// that list is opened in the user's `$EDITOR` for them to edit before
    /// being stored. The leaf-dance / pre-fill machinery that used to be
    /// called "truncate" (because it just wrote `summary = ""`) now produces
    /// a meaningful pre-filled summary. When the dropped span touches no
    /// files the template is empty and we fall back to `summary = ""` for
    /// back-compat with the no-op compaction.
    Manual,
    /// Deterministic text-only summary: user prompts + assistant text
    /// responses per turn, plus a touched-files list (read/write/edit
    /// paths, also split into Documents / Files). No reasoning, no tool
    /// calls, no tool output.
    TextOnly,
    /// LLM-generated summary: the dropped span is serialized into a prompt,
    /// sent through `agent.summarize()` (after temporarily moving the leaf
    /// to the entry *before* the compaction point and remembering the
    /// original leaf), and the response is written as `summary`.
    /// Accepts an optional trailing free-text payload (the user-editable
    /// instruction); when empty, `settings.llm_compact_prompt()` is used.
    Llm,
}

/// Build the LLM-bound conversation prefix for the session's current leaf.
///
/// Shared by the normal agent turn (`prompt`) and the session-context
/// summarizer (`summarize_with_session_context`) so the two requests build
/// their leading history identically — this is what preserves the provider
/// prefix-cache property: a summarization at a given position sends the same
/// bytes as a user prompt at that position, so the cached span (warmed by live
/// turns) hits. `Session::build_context()` applies the compaction transforms,
/// which re-activates a prior compaction active at the leaf as the leading
/// `<summary>` wrapper (see `doc/nested_compactions.md`).
///
/// Unlike `prompt` (which falls back to an empty vec on a stale index), the
/// summarizer treats an index failure as fatal — summarizing an empty prefix
/// would silently drop the whole conversation.
async fn session_context_messages(
    session: &Session,
    reasoning_in_context: crate::settings::ReasoningInContextMode,
    provider: &str,
    model: &str,
) -> crate::Result<Vec<Message>> {
    let ctx = session.build_context().await?;
    Ok(maybe_strip_reasoning(
        ctx.messages,
        reasoning_in_context,
        provider,
        model,
    ))
}

/// Walk the active branch (root → leaf) and return the entry id that
/// immediately precedes `target_id`. Returns `None` if `target_id` is not
/// on the active branch or is the root. Used by the `compact llm` handler
/// to pick the position from which to run the LLM (so the LLM's prior
/// context equals the span the compaction entry is dropping).
/// Find the predecessor of `target_id` on an arbitrary branch (root→leaf order).
/// Returns the ID of the entry immediately before `target_id`, or `None` if
/// `target_id` is not on the branch or is the first entry.
fn find_predecessor(branch: &[crate::session::SessionEntry], target_id: &str) -> Option<String> {
    let pos = branch.iter().position(|e| e.id == target_id)?;
    if pos == 0 {
        None
    } else {
        Some(branch[pos - 1].id.clone())
    }
}

/// The generated compaction summary plus the context snapshot a later
/// [`apply_compaction`] needs (doc/editor_cleanup.md §6).
///
/// Produced by [`generate_compaction`] with **no** session writes and no
/// rendering: the caller — a menu node's `prepare()` or the typed
/// `/tree … compact` entry — owns the review step (typically `$EDITOR`)
/// and hands the possibly-edited draft back to `apply_compaction`.
#[derive(Debug, Clone)]
pub(crate) struct CompactionDraft {
    /// Summary text as generated. Callers replace it with the reviewed
    /// (possibly edited) text before applying; empty is legal — manual
    /// mode on a span that touched no files.
    pub summary: String,
    /// Estimate of the context the compaction replaces, taken at the
    /// original leaf before the pivot — the same
    /// `estimate_context_tokens(build_context())` call the `/tree
    /// compact` sub-menu header shows, so header and stored value agree
    /// by construction.
    pub tokens_before: u64,
}

/// Generate a compaction summary draft for `mode` at `target_id` — the
/// ONE code path every compaction mode shares (doc/editor_cleanup.md §6):
///
/// 1. validate: active session, known target, non-empty span for `Llm`;
/// 2. snapshot `tokens_before` at the original leaf;
/// 3. **pivot**: move the leaf to the predecessor of the target so
///    `build_context()` scopes to `[root .. target)` — a previous
///    compaction at or before that point re-enters as its `<summary>`
///    wrapper, which is what makes iterative compaction lossless;
/// 4. build the mode's draft against that context (`Llm` summarizes
///    *while pivoted*, so retries re-send the identical scoped prefix);
/// 5. **restore** the original leaf on every return path.
///
/// No special cases: when the target sits at the current leaf (or the
/// branch root) the pivot is just a move. Pure data — no session writes,
/// no display; errors come back as strings and all UX (editor, confirm)
/// belongs to the caller.
pub(crate) async fn generate_compaction(
    app_state: &AppState,
    target_id: &str,
    mode: CompactSummaryMode,
    llm_payload: &str,
) -> std::result::Result<CompactionDraft, String> {
    // 1. Validation — one place; the typed arm and the menu nodes
    //    previously kept near-verbatim copies of this preamble.
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot compact: no active session".to_string())?;
    if session.get_entry(target_id).await.is_none() {
        return Err(format!(
            "Cannot compact: unknown tree entry {}",
            &target_id[..target_id.len().min(12)]
        ));
    }

    // 2. tokens_before at the original leaf (pre-pivot).
    let tokens_before = match session.build_context().await {
        Ok(ctx) => estimate_context_tokens(&ctx.messages),
        Err(_) => 0,
    };

    // Branch facts, pre-pivot: the predecessor scopes the pivot; the
    // dropped span (everything strictly before the target) feeds the
    // deterministic modes.
    let branch = session.get_branch().await;
    let predecessor_id = find_predecessor(&branch, target_id);
    let dropped: Vec<_> = branch
        .into_iter()
        .take_while(|e| e.id != target_id)
        .collect();

    // Llm has nothing to summarize without a predecessor (root-level
    // target). Manual/TextOnly tolerate it: the draft comes out empty
    // and the user can write it in the editor.
    if predecessor_id.is_none() && matches!(mode, CompactSummaryMode::Llm) {
        return Err("Cannot compact: nothing before this point".to_string());
    }

    // Llm needs a live agent, created lazily from the target config
    // (same target-stamp judgment as session creation). Manual/TextOnly
    // never touch the agent, so they stay network-free.
    let mut agent = app_state.agent();
    if matches!(mode, CompactSummaryMode::Llm) && agent.is_none() {
        let cfg = app_state.config_snapshot();
        match create_agent(cfg).await {
            Ok(a) => {
                let a: Arc<CodeAgent> = Arc::from(a);
                app_state.activate_agent(a.clone());
                agent = Some(a);
            }
            Err(e) => return Err(format!("Cannot start agent: {}", e)),
        }
    }

    // Session directory for relativizing file paths in the drafts.
    let session_dir = std::path::PathBuf::from(session.index().lock().await.header.cwd.clone());

    // 3./5. The pivot dance — one path for every mode: move → draft →
    // restore, with the restore running on every return path.
    let old_leaf = session.get_leaf_id().await;
    session
        .move_leaf(predecessor_id)
        .await
        .map_err(|e| e.to_string())?;
    let generated = draft_at_pivot(
        &session,
        &dropped,
        &session_dir,
        mode,
        llm_payload,
        &agent,
        app_state,
    )
    .await;
    // Restore unconditionally (the await below runs before anything is
    // returned); a restore failure is fatal even when the draft
    // succeeded — the leaf must never be left on the pivot.
    let restored = session
        .move_leaf(old_leaf)
        .await
        .map_err(|e| format!("compaction pivot restore failed: {}", e));
    match generated {
        Ok(summary) => restored.map(|_| CompactionDraft {
            summary,
            tokens_before,
        }),
        Err(draft_err) => Err(match restored {
            Err(restore_err) => format!("{} ({})", draft_err, restore_err),
            Ok(()) => draft_err,
        }),
    }
}

/// Build the mode's draft against the **pivoted** leaf (step 4 of
/// [`generate_compaction`]): the seed comes from the same scoped
/// `build_context()` the Llm summarization reads, so previous
/// compactions are accounted for exactly once, on one path.
async fn draft_at_pivot(
    session: &Session,
    dropped: &[crate::session::SessionEntry],
    session_dir: &std::path::Path,
    mode: CompactSummaryMode,
    llm_payload: &str,
    agent: &Option<Arc<CodeAgent>>,
    app_state: &AppState,
) -> std::result::Result<String, String> {
    let seed_text = session
        .build_context()
        .await
        .ok()
        .as_ref()
        .and_then(crate::session::Session::extract_seed_from_context);
    match mode {
        CompactSummaryMode::Manual => Ok(crate::session::build_manual_template(
            dropped,
            seed_text.as_deref(),
            session_dir,
        )),
        CompactSummaryMode::TextOnly => Ok(crate::session::build_text_only_summary(
            dropped,
            session_dir,
        )),
        CompactSummaryMode::Llm => {
            // Compose the user-tweakable instruction: trailing text on
            // the committed line wins; otherwise the setting (with
            // built-in fallback).
            let settings = app_state.settings();
            let instruction = if llm_payload.trim().is_empty() {
                settings.llm_compact_prompt()
            } else {
                llm_payload.to_string()
            };
            // The leaf sits at the predecessor, so this reads the scoped
            // prefix — a previous compaction at or before the point
            // enters as its leading <summary> wrapper, and retries
            // re-send the identical prefix.
            let Some(a) = agent.as_ref() else {
                return Err("no agent".to_string());
            };
            let output = app_state.with_config(|c| c.output.clone());
            let (user_cancel, cancel_notify) =
                app_state.with_config(|c| (c.user_cancel.clone(), c.cancel_notify.clone()));
            let instruction = instruction.clone();
            let reply = retry_summarization(
                output.as_ref(),
                &user_cancel,
                &cancel_notify,
                Some(app_state),
                &mut || a.summarize_with_session_context(session, &instruction),
            )
            .await
            .map_err(|e| format!("Summary failed: {}", e))?;
            let summary = reply.trim().to_string();
            if summary.is_empty() {
                return Err("Summary was empty; nothing written.".to_string());
            }
            Ok(summary)
        }
    }
}

/// Write the compaction entry for a reviewed [`CompactionDraft`] — the
/// apply half of doc/editor_cleanup.md §6. Data layer only: no display
/// (the user has just seen the summary; the committed input line
/// documents the write) and no confirm (the caller's UI owns that).
pub(crate) async fn apply_compaction(
    app_state: &AppState,
    target_id: &str,
    draft: &CompactionDraft,
) -> std::result::Result<(), String> {
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot compact: no active session".to_string())?;
    session
        .append_compaction(&draft.summary, target_id, draft.tokens_before)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Branch summarization — `/tree <id> switch summarize`
//
// pi parity, verified against ~/src/pi @ this change:
//   - `collectEntriesForBranchSummary` (core/compaction/branch-summarization.ts)
//     collects the spur; `navigateTree` (core/agent-session.ts) summarizes it
//     and calls `branchWithSummary(newLeafId, …)` (core/session-manager.ts),
//     which sets `leafId = target` FIRST and then appends the
//     `branch_summary` entry with `parentId = target`, `fromId = target`.
//     The entry is therefore *inline on the branch the conversation continues
//     on* — not parked on the abandoned spur — and `buildSessionContext`'s
//     per-entry mapping turns it into a user message (see
//     `session::branch_summary_message_text`). pi's context build walks only
//     the leaf path, so nothing has to go looking for summaries in side spurs.
// ---------------------------------------------------------------------------

/// Prepended to the **stored** summary text by pi's `generateBranchSummary`,
/// so the framing ("…before returning here") travels with the entry and is
/// visible in `/tree` too, not only inside the LLM wrapper.
const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

/// pi's `BRANCH_SUMMARY_PROMPT` (core/compaction/branch-summarization.ts),
/// byte-exact. The `## Goal / ## Constraints / ## Progress / ## Key Decisions
/// / ## Next Steps` shape is what makes the stored summary useful as *context*
/// later — a prose recap does not tell the next turn what is unfinished.
/// pi sends it with `maxTokens: 2048` alongside `SUMMARIZATION_SYSTEM_PROMPT`;
/// our summarizer (`CodeAgent::summarize`) is a single user-message request with
/// no system prompt, and the text above is self-contained, so it all lives in
/// the prompt instead.
pub(crate) const DEFAULT_BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// The entries a switch from `old_leaf` to `target` leaves behind: everything
/// after the deepest common ancestor on the old leaf's path, in chronological
/// order (pi `collectEntriesForBranchSummary`). Empty when the target is
/// already on the current path (i.e. navigating forward/down) — pi skips
/// summarization in that case and just branches.
///
/// Iterative (two `get_path_to_root` walks); no recursion, so a 1000-entry
/// linear session cannot overflow the stack. The caller holds the index lock.
pub(crate) fn abandoned_spur(
    index: &crate::session::SessionIndex,
    old_leaf: &str,
    target: &str,
) -> Vec<crate::session::SessionEntry> {
    let old_path = index.get_path_to_root(old_leaf);
    let target_path = index.get_path_to_root(target);
    let target_ids: std::collections::HashSet<&str> =
        target_path.iter().map(|e| e.id.as_str()).collect();
    // The **deepest** shared entry is the branch point (pi walks
    // `oldLeafId → …` *until* it, so the ancestor itself is not in the span).
    // `rposition`, not `position`: when the target is a *descendant* of the
    // old leaf, the deepest shared entry IS the old leaf, which makes the spur
    // empty — forward navigation abandons nothing. And the ancestor is
    // excluded by `skip(i + 1)`: skipping `i` would fold the shared prefix into
    // the spur and have the summarizer re-summarize what the new branch keeps.
    match old_path
        .iter()
        .rposition(|e| target_ids.contains(e.id.as_str()))
    {
        Some(i) => old_path.into_iter().skip(i + 1).collect(),
        // No shared ancestor (an id that is not in this tree): the whole old
        // path is the spur. Callers validate the target first.
        None => old_path,
    }
}

/// Serialize spur entries to the text body of pi's `<conversation>` block
/// (`serializeConversation`, core/compaction/utils.ts): one line per message,
/// role-tagged, with tool calls rendered as `name(arg=value, …)`.
///
/// Deliberate divergences, both consistent with `build_text_only_summary`:
/// tool *results* are skipped (their content is already in the assistant's
/// tool call) and reasoning blocks are omitted (pi includes them; our
/// `reasoning_in_context` setting says reasoning does not belong in context).
/// `Compaction` / `BranchSummary` entries inside the spur are serialized as
/// the user message pi's conversion would produce, so a nested summary
/// survives into the outer summary.
fn serialize_spur(entries: &[crate::session::SessionEntry]) -> String {
    use crate::session::SessionEntryContent as C;
    let mut parts: Vec<String> = Vec::new();
    for entry in entries {
        match &entry.content {
            C::Message { message } => {
                let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
                let content = message.get("content").and_then(|c| c.as_array());
                let Some(items) = content else { continue };
                match role {
                    "user" => {
                        let text = items
                            .iter()
                            .filter_map(|i| {
                                (i.get("type").and_then(|t| t.as_str()) == Some("text"))
                                    .then(|| i.get("text").and_then(|t| t.as_str()))
                                    .flatten()
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        if !text.is_empty() {
                            parts.push(format!("[User]: {}", text));
                        }
                    }
                    "assistant" => {
                        let text = items
                            .iter()
                            .filter_map(|i| {
                                (i.get("type").and_then(|t| t.as_str()) == Some("text"))
                                    .then(|| i.get("text").and_then(|t| t.as_str()))
                                    .flatten()
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            parts.push(format!("[Assistant]: {}", text));
                        }
                        let calls = items
                            .iter()
                            .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("toolCall"))
                            .map(|call| {
                                let name =
                                    call.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                                let args = match call.get("arguments").and_then(|a| a.as_object()) {
                                    Some(map) if !map.is_empty() => map
                                        .iter()
                                        .map(|(k, v)| {
                                            format!(
                                                "{}={}",
                                                k,
                                                serde_json::to_string(v).unwrap_or_default()
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    _ => String::new(),
                                };
                                format!("{}({})", name, args)
                            })
                            .collect::<Vec<_>>();
                        if !calls.is_empty() {
                            parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                        }
                    }
                    // toolResult: context is in the assistant's tool call.
                    _ => {}
                }
            }
            C::Compaction { summary, .. } if !summary.is_empty() => {
                parts.push(format!(
                    "[User]: {}{}{}",
                    crate::session::COMPACTION_SUMMARY_PREFIX,
                    summary,
                    crate::session::COMPACTION_SUMMARY_SUFFIX
                ));
            }
            C::BranchSummary { summary, .. } if !summary.is_empty() => {
                parts.push(format!(
                    "[User]: {}",
                    crate::session::branch_summary_message_text(summary)
                ));
            }
            // Meta entries (model/thinking/labels/custom): no conversation content.
            _ => {}
        }
    }
    parts.join("\n\n")
}

/// The `Documents` / `Files` section pi appends to every generated branch
/// summary (its `formatFileOperations`), built from the same collector the
/// text-only summary uses. `None` when the spur touched no files — nothing to
/// append.
fn spur_files_section(
    entries: &[crate::session::SessionEntry],
    session_dir: &Path,
) -> Option<String> {
    let mut files: Vec<crate::session::TouchedFile> = Vec::new();
    for entry in entries {
        if let crate::session::SessionEntryContent::Message { message } = &entry.content {
            crate::session::collect_touched_files(message, &mut files, session_dir);
        }
    }
    if files.is_empty() {
        return None;
    }
    let mut out = String::new();
    crate::session::render_files_section(&mut out, &files, "Documents");
    crate::session::render_files_section(&mut out, &files, "Files");
    (!out.is_empty()).then_some(out)
}

/// Generate a branch summary draft for the spur left behind by switching from
/// the current leaf to `target_id` — the `/tree <id> switch summarize` twin of
/// [`generate_compaction`], with the same three modes and the same "no writes,
/// errors as strings" contract (doc/editor_cleanup.md §6).
///
/// 1. validate: active session, known target, target ≠ leaf, non-empty spur;
/// 2. build the mode's draft from the spur entries (never from the live
///    conversation prefix — a spur is off-path, so there is no prefix to
///    summarize; pi sends it as a standalone request too);
/// 3. `Llm` only: prepend pi's preamble and append the touched-files section,
///    so the stored summary carries its own framing.
///
/// `tokens_before` is left at 0: a `BranchSummary` entry has no such field
/// (pi's is `fromId` + `summary`), so nothing consumes it.
pub(crate) async fn generate_branch_summary(
    app_state: &AppState,
    target_id: &str,
    mode: CompactSummaryMode,
    llm_payload: &str,
) -> std::result::Result<CompactionDraft, String> {
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot summarize: no active session".to_string())?;
    if session.get_entry(target_id).await.is_none() {
        return Err(format!(
            "Cannot summarize: unknown tree entry {}",
            &target_id[..target_id.len().min(12)]
        ));
    }
    let old_leaf = session
        .get_leaf_id()
        .await
        .ok_or_else(|| "Cannot summarize: no current position".to_string())?;
    if old_leaf == target_id {
        return Err("Already at this entry — nothing to summarize.".to_string());
    }
    // One index lock, one collection — the spur is a snapshot; nothing below
    // mutates the index (the LLM request is the only await).
    let spur: Vec<crate::session::SessionEntry> = {
        let index = session.index().lock().await;
        abandoned_spur(&index, &old_leaf, target_id)
    };
    if spur.is_empty() {
        return Err("Nothing to summarize: this entry is ahead on the current branch.".to_string());
    }
    let session_dir = std::path::PathBuf::from(session.index().lock().await.header.cwd.clone());

    let mut agent = app_state.agent();
    if matches!(mode, CompactSummaryMode::Llm) && agent.is_none() {
        let cfg = app_state.config_snapshot();
        match create_agent(cfg).await {
            Ok(a) => {
                let a: Arc<CodeAgent> = Arc::from(a);
                app_state.activate_agent(a.clone());
                agent = Some(a);
            }
            Err(e) => return Err(format!("Cannot start agent: {}", e)),
        }
    }

    let summary = match mode {
        // The deterministic modes are the compaction ones verbatim — they
        // take the dropped entries and touch no LLM. The manual template's
        // seed (a previous compaction summary) has no analogue here: a spur
        // carries no compaction boundary, so the file list stands alone.
        CompactSummaryMode::Manual => {
            crate::session::build_manual_template(&spur, None, &session_dir)
        }
        CompactSummaryMode::TextOnly => {
            crate::session::build_text_only_summary(&spur, &session_dir)
        }
        CompactSummaryMode::Llm => {
            let conversation = serialize_spur(&spur);
            // pi appends custom instructions rather than replacing the
            // structured-format prompt (`Additional focus:`), so a user
            // instruction can never break the required `##` sections.
            let instructions = if llm_payload.trim().is_empty() {
                DEFAULT_BRANCH_SUMMARY_PROMPT.to_string()
            } else {
                format!(
                    "{}\n\nAdditional focus: {}",
                    DEFAULT_BRANCH_SUMMARY_PROMPT,
                    llm_payload.trim()
                )
            };
            let prompt = format!(
                "<conversation>\n{}\n</conversation>\n\n{}",
                conversation, instructions
            );
            let Some(a) = agent.as_ref() else {
                return Err("no agent".to_string());
            };
            let output = app_state.with_config(|c| c.output.clone());
            let (user_cancel, cancel_notify) =
                app_state.with_config(|c| (c.user_cancel.clone(), c.cancel_notify.clone()));
            let reply = retry_summarization(
                output.as_ref(),
                &user_cancel,
                &cancel_notify,
                Some(app_state),
                &mut || a.summarize(&prompt),
            )
            .await
            .map_err(|e| format!("Summary failed: {}", e))?;
            let reply = reply.trim();
            if reply.is_empty() {
                return Err("Summary was empty; nothing written.".to_string());
            }
            let mut summary = format!("{}{}", BRANCH_SUMMARY_PREAMBLE, reply);
            if let Some(files) = spur_files_section(&spur, &session_dir) {
                summary.push_str(&files);
            }
            summary
        }
    };

    Ok(CompactionDraft {
        summary,
        tokens_before: 0,
    })
}

/// Whether `target_id` names an entry `/tree <id> edit` can re-ask: a user
/// `Message` or a `CustomMessage` — pi's two cases
/// (`agent-session.ts:2950`/`:2954`). Read from the **session**, not the tree
/// listing, so the node's row cannot go stale against it.
pub(crate) async fn is_reaskable_entry(session: &Session, target_id: &str) -> bool {
    let Some(entry) = session.get_entry(target_id).await else {
        return false;
    };
    match &entry.content {
        crate::session::SessionEntryContent::Message { message } => {
            message.get("role").and_then(|r| r.as_str()) == Some("user")
        }
        crate::session::SessionEntryContent::CustomMessage { .. } => true,
        _ => false,
    }
}

/// `/tree <id> edit` — pi's user-message re-edit: move the leaf to the entry's
/// **parent** (so the prompt leaves the active branch) and put its text in the
/// input buffer for re-asking (`agent-session.ts:2950-2953`).
///
/// Distinct from `/tree <id> switch cursor`, which moves the leaf *to* the
/// entry — that keeps the prompt in context and continues after it. Both are
/// useful, so they are separate menu rows rather than one overloaded action.
///
/// The prefill goes through the one-shot register the loop pops before the next
/// `read_input`; it replaces the buffer outright (unlike pi, which only fills an
/// empty one) because that is the register's contract for handler writes.
pub(crate) async fn reask_from_entry(
    app_state: &AppState,
    target_id: &str,
) -> std::result::Result<(), String> {
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot edit: no active session".to_string())?;
    let entry = session.get_entry(target_id).await.ok_or_else(|| {
        format!(
            "Cannot edit: unknown tree entry {}",
            &target_id[..target_id.len().min(12)]
        )
    })?;
    let text = match &entry.content {
        crate::session::SessionEntryContent::Message { message } => {
            if message.get("role").and_then(|r| r.as_str()) != Some("user") {
                return Err("Only a user prompt can be edited — use `switch` to move here.".into());
            }
            crate::session::user_prompt_text(message)
        }
        crate::session::SessionEntryContent::CustomMessage { content, .. } => content
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        _ => return Err("Only a user prompt can be edited — use `switch` to move here.".into()),
    };
    let Some(text) = text else {
        return Err("That prompt has no text to edit.".into());
    };
    // Parent, not the entry: re-asking forks a new turn in place of the old
    // one, so the original prompt leaves the active branch (pi does the same,
    // `parentId: null` for a root prompt).
    session
        .move_leaf(entry.parent_id.clone())
        .await
        .map_err(|e| e.to_string())?;
    app_state.set_prefill(text);
    Ok(())
}

/// Write a reviewed branch summary and move the leaf — the submit half of
/// `/tree <id> switch summarize`, and the one place the two summary flows
/// differ in their effect.
///
/// Order matters and follows pi's `navigateTree`: **move first, then append**.
/// `append_branch_summary` parents to the current leaf and advances it, so
/// moving to `target_id` first yields pi's exact shape — a `branch_summary`
/// entry with `parentId = fromId = target_id`, sitting inline on the branch the
/// conversation continues on, where `build_session_context` projects it into
/// the LLM context.
///
/// The leaf-moved check is the guard against a draft prepared for a different
/// spur: the summary only describes what was behind the leaf at generation
/// time, so a leaf that moved since (the user switched elsewhere and came
/// back mid-review) makes the draft stale.
pub(crate) async fn apply_branch_summary(
    app_state: &AppState,
    target_id: &str,
    expected_old_leaf: &str,
    draft: &CompactionDraft,
) -> std::result::Result<(), String> {
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot summarize: no active session".to_string())?;
    let current = session
        .get_leaf_id()
        .await
        .ok_or_else(|| "Cannot summarize: no current position".to_string())?;
    if current != expected_old_leaf {
        return Err(
            "The session moved since this summary was generated — press Enter to regenerate."
                .to_string(),
        );
    }
    if draft.summary.trim().is_empty() {
        return Err("Summary is empty; nothing written.".to_string());
    }
    // pi: leaf = branchFromId, then append parented to it.
    session
        .move_leaf(Some(target_id.to_string()))
        .await
        .map_err(|e| e.to_string())?;
    session
        .append_branch_summary(target_id, &draft.summary)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Run a *summarization* request with the same auto-retry / back-off / Esc-
/// cancel behaviour as a regular user prompt (`prompt()`'s retry loop in
/// `run_interactive`).
///
/// `keys` is the window pump's source (see [`WindowKeySource`]): while the
/// caller awaits this future, the REPL loop is suspended inside `settle` —
/// no `read_input`, no in-flight reader — so *nothing* reads stdin unless
/// this future polls it. The pump runs beside both phases: a root `Esc`
/// cancels the in-flight attempt (the select arm fires the signal; the rig
/// hook and this loop's `cancel_observed` both watch it) *and* any back-off
/// (`cancellable_sleep_windowed`); other keys are parked for `read_input`'s
/// next iteration. `None` (tests) and inactive windows (piped/batch) leave
/// the arms pending — the signal-only behaviour, stdin untouched.
///
/// `build` produces the summarization request and returns the model's reply
/// (or a classified `PicocodeError`). On a retryable failure it is re-invoked
/// (previous `build` again) after `retry_delay_secs` seconds of
/// [`cancellable_sleep_windowed`]; non-retryable errors and cancellation
/// propagate out.
///
/// The leaf dance (if any) is the caller's responsibility: `build` reads the
/// session's *current* leaf to assemble context but must NOT move it, so the
/// caller can restore the original leaf on every return path. This keeps the
/// helper agnostic between the compaction path (leaf moved to the predecessor
/// for a conversation prefix) and the abandoned-branch path (no move).
pub(crate) async fn retry_summarization<F, Fut>(
    output: &dyn Output,
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
    keys: Option<&dyn WindowKeySource>,
    build: &mut F,
) -> std::result::Result<String, crate::PicocodeError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<String, crate::PicocodeError>>,
{
    // Fresh operation: a stale cancel flag from a previous abort must not
    // cancel this one before the pump has read anything. `prompt()` applies
    // the same fresh-request rule; nothing else clears the flag on this
    // path (compaction runs between turns).
    user_cancel.store(false, Ordering::SeqCst);
    let mut consecutive_failures: u8 = 0;
    loop {
        // One attempt with the pump live beside it. The attempt future is
        // pinned: a key press polls *it* again rather than dropping the
        // in-flight request on the floor.
        let attempt = build();
        tokio::pin!(attempt);
        let outcome: Option<std::result::Result<String, crate::PicocodeError>> = loop {
            tokio::select! {
                r = &mut attempt => break Some(r),
                _ = cancel_observed(user_cancel, cancel_notify) => break None,
                ev = next_window_key(keys) => {
                    if let Some(src) = keys {
                        if pump_window_key(ev, src, user_cancel, cancel_notify) {
                            break None;
                        }
                    }
                }
            }
        };
        match outcome {
            // Cancelled — a window `Esc` (signalled above) or the flag from
            // elsewhere. Nothing in flight to persist: a summarization has
            // no partial turn.
            None => return Err(crate::PicocodeError::Cancelled("Aborted.".into())),
            Some(Ok(text)) => return Ok(text),
            Some(Err(crate::PicocodeError::Cancelled(_))) => {
                return Err(crate::PicocodeError::Cancelled("Aborted.".into()));
            }
            Some(Err(e)) if e.is_auto_retryable() && consecutive_failures < retry_budget(&e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let secs = retry_delay_secs(&e, consecutive_failures);
                output.display_text(&format!("Error: {}. Retrying in {secs}s...", e));
                let slept =
                    cancellable_sleep_windowed(user_cancel, cancel_notify, secs, keys).await;
                if !slept {
                    return Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                }
                // `build` is re-invoked verbatim on the retry (re-sends the
                // same conversation prefix from the session leaf; cheap, and
                // identical to a normal prompt retry).
                if user_cancel.load(Ordering::SeqCst) {
                    return Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                }
                continue;
            }
            Some(Err(e)) => return Err(e),
        }
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
    use super::*;
    use crate::output::ConsoleOutput;
    use serial_test::serial;

    /// One turn: reasoning, an answer, a tool round, then a second answer.
    fn two_segment_turn() -> ConversationTurn {
        ConversationTurn {
            user_text: "what is the answer?".into(),
            segments: vec![
                AssistantSegment {
                    thinking: vec!["Weighing the options.".into()],
                    text: vec!["Let me check.".into()],
                    tool_calls: vec![(
                        "c1".into(),
                        "Bash".into(),
                        serde_json::json!({"command": "echo hi"}),
                    )],
                    tool_results: vec![("c1".into(), "hi\n".into())],
                },
                AssistantSegment {
                    thinking: vec![],
                    text: vec!["The answer is 42.".into()],
                    tool_calls: vec![],
                    tool_results: vec![],
                },
            ],
        }
    }

    #[test]
    #[serial]
    fn replay_paints_what_the_live_path_paints() {
        let turn = two_segment_turn();
        let reasoning = crate::settings::default_show_reasoning_handle();
        *reasoning.lock().unwrap() = crate::settings::ShowReasoning::Content;

        // The live path, exactly as the streaming loop drives it. Reasoning is
        // asked onto the captured stream so the comparison sees it (a test's
        // stdout is not a terminal, and without this it would be routed to
        // stderr by both sides and the test would prove nothing).
        let mut live = ConsoleOutput::new(reasoning.clone());
        live.reasoning_on_stderr = false;
        live.begin_capture();
        live.begin_response();
        for thinking in &turn.segments[0].thinking {
            live.stream_thinking(thinking);
        }
        for text in &turn.segments[0].text {
            live.stream_text(text);
        }
        live.end_response_segment();
        live.display_tool_call("Bash", &serde_json::json!({"command": "echo hi"}));
        live.display_tool_result("hi\n");
        live.end_response_segment();
        for text in &turn.segments[1].text {
            live.stream_text(text);
        }
        live.end_response();
        let live_bytes = live.end_capture();

        // The same turn through replay. `render_turn` draws the user prompt
        // (bracketed) and then the response, so the live output should be the
        // *tail* of what replay paints — byte for byte, reasoning included.
        let mut replay = ConsoleOutput::new(reasoning);
        replay.reasoning_on_stderr = false;
        replay.begin_capture();
        render_turn(&replay, &turn);
        let replay_bytes = replay.end_capture();

        let live_text = String::from_utf8_lossy(&live_bytes).to_string();
        let replay_text = String::from_utf8_lossy(&replay_bytes).to_string();
        assert!(
            replay_text.ends_with(&live_text),
            "replay did not end with the live output.\n--- live ---\n{live_text:?}\n--- replay ---\n{replay_text:?}"
        );
        assert!(
            replay_text.contains("Weighing the options"),
            "replay lost the reasoning; the two render paths diverged again"
        );
    }

    use rig_core::message::Reasoning;

    use super::TurnPersistence;
    use super::{
        apply_compaction, blob_edit_tool_calls, build_replay_units, compute_resume_model_prefill,
        count_turns, create_private_dir, estimate_context_tokens, generate_compaction,
        is_anthropic_family, last_turns_start, maybe_strip_reasoning, message_role, persist_turn,
        reasoning_text, record_model_change, record_reasoning_change, retry_summarization,
        session_recorded_model_id, session_recorded_reasoning, MessageLogger,
    };
    use super::{model_facing_tool_text, CompactSummaryMode, ReplayUnit};
    use crate::session::storage::SessionStorage;
    use crate::session::Session;
    use crate::SettingsStore;
    use rig_core::message::{AssistantContent, Message, Text};

    /// The model-facing text for a malformed `edit` block must lead with the
    /// problem and drop the two redundant wrappers. This is the single highest-
    /// value behavior in the change: it is what a weak model actually reads when
    /// it gets a marker wrong.
    #[test]
    fn model_text_strips_redundant_arg_error_wrappers() {
        let raw = "failed to parse tool arguments: Invalid edit tool arguments: \
Invalid block: malformed `@@@NEW` marker `@@@NEWEND`: a marker must be alone on its line";
        let cleaned = model_facing_tool_text("edit", raw).expect("edit errors are rewritten");
        assert_eq!(
            cleaned,
            "Invalid block: malformed `@@@NEW` marker `@@@NEWEND`: a marker must be alone on its line"
        );
        assert!(!cleaned.contains("failed to parse tool arguments"));
        assert!(!cleaned.contains("Invalid edit tool arguments"));
    }

    #[test]
    fn model_text_rewrites_nothing_else() {
        // A non-edit tool is never rewritten.
        let args_err = "failed to parse tool arguments: bad integer";
        assert_eq!(model_facing_tool_text("bash", args_err), None);
        // An edit success (or any message without the wrappers) is untouched.
        let ok = "Replaced 1 block(s) in src/x.rs";
        assert_eq!(model_facing_tool_text("edit", ok), None);
        // A call-time error the model sees is passed through as-is.
        let call_err = "the tool failed";
        assert_eq!(model_facing_tool_text("edit", call_err), None);
    }

    /// A three-file call plus its array result, as the stored history holds it.
    fn multi_file_history() -> Vec<Message> {
        let blocks = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                      b.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END\n\n\
                      c.rs\n@@@OLD\nq\n@@@NEW\nQ\n@@@END";
        let result = blockedit::render_file_results(&[
            blockedit::FileResult {
                path: "a.rs".into(),
                result: serde_json::json!("Replaced 1 block(s) in ./a.rs"),
            },
            blockedit::FileResult {
                path: "b.rs".into(),
                result: serde_json::json!({
                    "ui_view": "edit failed: nope", "model_view": "long diff"
                }),
            },
            blockedit::FileResult {
                path: "c.rs".into(),
                result: serde_json::json!("Replaced 1 block(s) in ./c.rs"),
            },
        ]);
        vec![
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                    id: ToolCallId::new("t1").unwrap(),
                    provider: None,
                    function: ToolFunction::new(
                        "edit".into(),
                        serde_json::json!({ "blocks": blocks }),
                    ),
                    signature: None,
                    additional_params: None,
                })],
            },
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult {
                    call: ToolCallId::new("t1").unwrap(),
                    provider: None,
                    name: "edit".into(),
                    content: vec![ToolResultContent::text(result)],
                })],
            },
        ]
    }

    fn tool_calls_of(m: &Message) -> Vec<rig_core::message::ToolCall> {
        let Message::Assistant { content, .. } = m else {
            panic!("expected an assistant message")
        };
        content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect()
    }

    fn results_of(m: &Message) -> Vec<(String, String)> {
        let Message::User { content } = m else {
            panic!("expected a user message")
        };
        content
            .iter()
            .filter_map(|c| match c {
                UserContent::ToolResult(tr) => Some((
                    tr.call.to_string(),
                    tool_result_text(tr).unwrap_or_default(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_multi_file_call_becomes_n_per_file_calls_and_results() {
        let out = split_multi_file_edit_calls(multi_file_history());

        // Three calls, each naming exactly one file.
        let calls = tool_calls_of(&out[0]);
        assert_eq!(calls.len(), 3, "{:?}", calls);
        for (i, name) in ["a.rs", "b.rs", "c.rs"].iter().enumerate() {
            let args = &calls[i].function.arguments;
            assert_eq!(calls[i].function.name, "edit");
            let blocks = args.get("blocks").and_then(|v| v.as_str()).unwrap();
            assert!(blocks.contains(name), "call {i} must name {name}: {blocks}");
            assert_eq!(
                blocks.matches("@@@OLD").count(),
                1,
                "call {i} must carry one file only: {blocks}"
            );
            // The id is derived, not minted, so both split sites agree.
            assert_eq!(calls[i].id.to_string(), format!("t1~f{i}n3"));
            // A synthesized call has no provider id or signature: Anthropic
            // keys tool_result on the id it issued, and N copies of one would
            // be unmatchable.
            assert!(calls[i].provider.is_none());
            assert!(calls[i].signature.is_none());
        }

        // Three results, one per call, paired by that same derived id.
        let results = results_of(&out[1]);
        assert_eq!(results.len(), 3, "{:?}", results);
        assert_eq!(results[0].0, "t1~f0n3");
        assert_eq!(results[1].0, "t1~f1n3");
        assert_eq!(results[2].0, "t1~f2n3");
        assert!(results[0].1.contains("Replaced 1 block(s) in ./a.rs"));
        // A failing file keeps its full envelope: the model reads model_view.
        assert!(results[1].1.contains("long diff"), "{}", results[1].1);
        assert!(results[2].1.contains("./c.rs"));
    }

    #[test]
    fn a_split_explains_itself_in_one_note_carrying_no_aggregate() {
        let out = split_multi_file_edit_calls(multi_file_history());
        let Message::User { content } = &out[1] else {
            panic!("expected a user message")
        };
        let notes: Vec<&Text> = content
            .iter()
            .filter_map(|c| match c {
                UserContent::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(notes.len(), 1, "exactly one note per split: {content:?}");
        let n = notes[0].text.as_str();
        assert!(n.contains('3'), "the note must name the file count: {n}");
        // The note rides inside the results' message, not as its own.
        assert_eq!(content[0], UserContent::Text(Text::new(n.to_string())));
    }

    #[test]
    fn splitting_twice_changes_nothing() {
        let once = split_multi_file_edit_calls(multi_file_history());
        let twice = split_multi_file_edit_calls(once.clone());
        assert_eq!(once, twice, "the transform must be idempotent");
    }

    #[test]
    fn a_single_file_call_is_left_byte_identical() {
        let call = Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                id: ToolCallId::new("s1").unwrap(),
                provider: None,
                function: ToolFunction::new(
                    "edit".into(),
                    serde_json::json!({"blocks": "solo.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END"}),
                ),
                signature: None,
                additional_params: None,
            })],
        };
        let result = Message::User {
            content: vec![UserContent::ToolResult(ToolResult {
                call: ToolCallId::new("s1").unwrap(),
                provider: None,
                name: "edit".into(),
                content: vec![ToolResultContent::text("Replaced 1 block(s) in ./solo.rs")],
            })],
        };
        let hist = vec![call, result];
        let out = split_multi_file_edit_calls(hist.clone());
        assert_eq!(out, hist, "a single-file call must be untouched");
    }

    #[test]
    fn a_call_with_no_result_yet_is_not_split() {
        // Mid-turn: the model made the call, the result has not come back.
        // Splitting now would show a call with a dangling result.
        let pending = vec![multi_file_history().remove(0)];
        let out = split_multi_file_edit_calls(pending.clone());
        assert_eq!(out, pending, "a call without its result stays whole");
    }

    #[test]
    fn blob_edit_tool_calls_renders_block_form_and_is_idempotent() {
        let args = serde_json::json!({
            "path": "src/x.rs",
            "edits": [{"oldText": "a", "newText": "b"}]
        });
        let mk = |args: serde_json::Value| Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                id: rig_core::message::ToolCallId::new("t1").unwrap(),
                provider: None,
                function: rig_core::message::ToolFunction::new("edit".into(), args),
                signature: None,
                additional_params: None,
            })],
        };

        // Canonical array -> blob, under the model-facing field name.
        let out = blob_edit_tool_calls(vec![mk(args)]);
        let Message::Assistant { content, .. } = &out[0] else {
            panic!("expected assistant")
        };
        let AssistantContent::ToolCall(tc) = &content[0] else {
            panic!("expected tool call")
        };
        let batches = tc
            .function
            .arguments
            .get("blocks")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(batches.contains("@@@OLD"), "got: {batches}");
        assert!(batches.contains("src/x.rs"), "got: {batches}");
        // The canonical array must not survive into the model-facing form.
        assert!(
            tc.function.arguments.get("edits").is_none(),
            "model form must not carry the array: {:?}",
            tc.function.arguments
        );

        // Idempotent: an already-block value is unchanged.
        let already =
            serde_json::json!({"path": "src/x.rs", "blocks": "@@@OLD\na\n@@@NEW\nb\n@@@END"});
        let out2 = blob_edit_tool_calls(vec![mk(already.clone())]);
        let Message::Assistant { content, .. } = &out2[0] else {
            panic!("expected assistant")
        };
        let AssistantContent::ToolCall(tc) = &content[0] else {
            panic!("expected tool call")
        };
        assert_eq!(tc.function.arguments, already);
    }

    #[test]
    fn turn_persistence_counts_dispatch_not_text() {
        // The per-turn record counts NEW dispatches (the user's original
        // prompt plus each steering/continuation directive), never the
        // prompt TEXT. This is what lets two identical "continue" messages
        // in one turn be treated as distinct dispatches.
        let turn = TurnPersistence::new();
        assert_eq!(turn.sent(), 0);

        // The user's first prompt is dispatch 1.
        turn.mark_new_dispatch();
        assert_eq!(turn.sent(), 1);

        // A retry that re-sends the SAME prompt does NOT bump the count:
        // callers only call mark_new_dispatch when next_input actually
        // changes to a new message.
        // (No-op here; the no-resend case is just "don't call it".)

        // A steering "continue" (same text as a possible original) is a NEW
        // dispatch and must bump.
        turn.mark_new_dispatch();
        assert_eq!(turn.sent(), 2);

        // A second "continue" steering is yet another dispatch.
        turn.mark_new_dispatch();
        assert_eq!(turn.sent(), 3);
    }

    #[test]
    fn turn_persistence_is_fresh_per_new_turn() {
        // Two successive user turns must have independent dispatch counts
        // (e.g. the user types "continue" twice). The REPL mints a fresh
        // TurnPersistence per user prompt, so turn 2 starts at 0 even
        // though turn 1 dispatched and persisted a "continue".
        let turn1 = TurnPersistence::new();
        turn1.mark_new_dispatch();
        assert_eq!(turn1.sent(), 1);

        let turn2 = TurnPersistence::new();
        assert_eq!(turn2.sent(), 0);
    }

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

    // ---- compaction: retry + generate/apply ----
    //
    // The compaction data path is `generate_compaction` (validate →
    // pivot to the predecessor → per-mode draft at the scoped context
    // → restore the leaf) and `apply_compaction` (append the reviewed
    // draft — data only, no display). `retry_summarization` is the
    // Llm arm's retrier (Esc-cancellable back-off). The tests below
    // verify the per-mode mechanics in isolation: Manual writes a
    // `Compaction` entry with an empty summary when the dropped span
    // touched no files; TextOnly writes a deterministic summary; Llm
    // fails cleanly without a live model. Mode plumbing itself is
    // covered by the node two-phase tests in `v25_tree.rs` — here we
    // focus on the side effects of the two halves, including that
    // generation restores the leaf it pivoted.

    #[tokio::test]
    async fn retry_summarization_succeeds_on_first_attempt() {
        let mut calls = 0;
        let output = null_output();
        let cancel = &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = &std::sync::Arc::new(tokio::sync::Notify::new());
        let r = retry_summarization(output.as_ref(), cancel, notify, None, &mut || {
            calls += 1;
            async { Ok::<_, crate::PicocodeError>("summary".to_string()) }
        })
        .await;
        assert_eq!(r.unwrap(), "summary");
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn retry_summarization_retries_on_rate_limit_then_succeeds() {
        // First call fails with a retryable 429; the back-off is cancellable
        // and the second attempt succeeds. Verify the retrier runs build
        // twice. (The sleep is 1s; acceptable for a focused test.)
        let mut calls = 0;
        let output = null_output();
        let cancel = &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = &std::sync::Arc::new(tokio::sync::Notify::new());
        let r = retry_summarization(output.as_ref(), cancel, notify, None, &mut || {
            calls += 1;
            async move {
                if calls == 1 {
                    Err(crate::PicocodeError::LlmRateLimited {
                        retry_after_secs: Some(1),
                    })
                } else {
                    Ok("second".to_string())
                }
            }
        })
        .await;
        assert_eq!(r.unwrap(), "second");
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn retry_summarization_propagates_non_retryable() {
        let mut calls = 0;
        let output = null_output();
        let cancel = &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = &std::sync::Arc::new(tokio::sync::Notify::new());
        let r = retry_summarization(output.as_ref(), cancel, notify, None, &mut || {
            calls += 1;
            async { Err(crate::PicocodeError::Llm("permanent".to_string())) }
        })
        .await;
        assert!(matches!(r, Err(crate::PicocodeError::Llm(_))));
        assert_eq!(calls, 1, "non-retryable must not retry");
    }

    #[tokio::test]
    async fn retry_summarization_cancels_after_esc_sleep() {
        // A provider hint of 5s: set the cancel flag so the windowed
        // sleep's signal arm ends the wait and the retrier surfaces
        // `Cancelled` without a second attempt.
        let mut calls = 0;
        let output = null_output();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let cancel_out = cancel.clone();
        // Cancel fires right after the first (failed) call, via a spawned
        // task that races the sleep.
        let cancel2 = cancel.clone();
        let notify2 = notify.clone();
        let started = std::time::Instant::now();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel2.store(true, std::sync::atomic::Ordering::SeqCst);
            notify2.notify_one();
        });
        let r = retry_summarization(output.as_ref(), &cancel_out, &notify, None, &mut || {
            calls += 1;
            async {
                Err(crate::PicocodeError::LlmRateLimited {
                    retry_after_secs: Some(5),
                })
            }
        })
        .await;
        let _ = handle.await;
        assert!(matches!(r, Err(crate::PicocodeError::Cancelled(_))));
        assert_eq!(calls, 1, "cancelled during back-off must not retry");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the 5s back-off ran its course instead of yielding to the signal"
        );
    }

    /// Build a writable `Session` from `entries` for the compaction
    /// generate/apply tests. The agent's `build_session_with_entries`
    /// opens the storage as read-only because it only tests reading
    /// (resume prefill); `apply_compaction` calls `append_compaction`,
    /// which needs a writable handle, so we use `open_or_create` here.
    async fn build_writable_session_with(
        dir: &Path,
        entries: Vec<crate::session::SessionEntry>,
    ) -> Session {
        use crate::session::storage::JsonlSessionStorage;
        use crate::session::types::session_filename;
        // The caller owns the directory (and keeps it alive for the whole test):
        // the returned `Session` re-opens by path, so the dir must outlive it.
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);
        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .expect("create storage");
        let header = crate::session::SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.expect("write header");
        for entry in &entries {
            storage.append_entry(entry).await.expect("append");
        }
        // Set the leaf to the last entry so `get_branch()` returns the full path.
        if let Some(last) = entries.last() {
            let leaf = crate::session::SessionEntry::new_leaf(Some(last.id.clone()), None);
            storage.append_entry(&leaf).await.expect("append leaf");
        }
        let index = storage.build_index().await.expect("build index");
        Session::new_persisted(storage, index)
    }

    fn message_entry(
        id: &str,
        parent: Option<&str>,
        role: &str,
        text: &str,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: crate::session::SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": role,
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    /// Helper: a Compaction entry with the given summary / first-kept / tokens.
    fn compaction_entry(
        id: &str,
        parent: Option<&str>,
        summary: &str,
        first_kept: &str,
        tokens: u64,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: crate::session::SessionEntryContent::Compaction {
                summary: summary.to_string(),
                first_kept_entry_id: first_kept.to_string(),
                tokens_before: tokens,
            },
        }
    }

    fn test_settings() -> SettingsStore {
        SettingsStore {
            default_model: "p:m".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_reasoning_level: None,
            models: Default::default(),
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::from("default prompt"),
            path: None,
        }
    }

    fn null_output() -> std::sync::Arc<dyn Output> {
        // `retry_summarization` surfaces retry notices through the
        // output; the existing `NoOutput` impl in `crate::output` is a
        // no-op for everything we touch.
        std::sync::Arc::new(crate::output::NoOutput) as std::sync::Arc<dyn Output>
    }

    /// Test app state for the compaction data path: writable session and
    /// provider `test` (so `create_agent` fails deterministically — an
    /// Llm generate without an injected agent errors offline).
    fn compaction_app_state(
        s: std::sync::Arc<Session>,
    ) -> std::sync::Arc<crate::app_state::AppState> {
        let app_state = crate::app_state::AppState::for_test(test_settings());
        app_state.set_session(Some(s));
        app_state
    }

    /// True when the session has any `BranchSummary` entry.
    async fn has_branch_summary_entry(s: &Session) -> bool {
        let index = s.index().lock().await;
        index.entries.values().any(|e| {
            matches!(
                e.content,
                crate::session::SessionEntryContent::BranchSummary { .. }
            )
        })
    }

    /// True when the session has any `Compaction` entry.
    async fn has_compaction_entry(s: &Session) -> bool {
        let index = s.index().lock().await;
        index.entries.values().any(|e| {
            matches!(
                e.content,
                crate::session::SessionEntryContent::Compaction { .. }
            )
        })
    }

    /// Pre-pivot estimate of the session's current context — the value
    /// `generate_compaction` snapshots into `tokens_before`.
    async fn expected_tokens(s: &Session) -> u64 {
        s.build_context()
            .await
            .map(|c| estimate_context_tokens(&c.messages))
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn generate_apply_manual_writes_empty_summary() {
        // Manual on a span that touched no files: generate → empty draft;
        // apply → Compaction entry with empty summary, firstKept=u1, and
        // the tokens_before snapshot generate took pre-pivot.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_compaction(&app_state, "u1", CompactSummaryMode::Manual, "")
            .await
            .expect("manual generate");
        assert_eq!(draft.summary, "", "no touched files → empty template");
        let tokens = expected_tokens(&s).await;
        assert_eq!(
            draft.tokens_before, tokens,
            "tokens_before is the pre-pivot estimate of the same context"
        );
        apply_compaction(&app_state, "u1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        let new_entry = s.get_entry(&new_leaf).await.expect("entry");
        match new_entry.content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
            } => {
                assert_eq!(summary, "");
                assert_eq!(first_kept_entry_id, "u1");
                assert_eq!(tokens_before, draft.tokens_before);
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_apply_text_only_writes_deterministic_summary() {
        // TextOnly over an empty dropped span (target = first entry)
        // yields the empty deterministic summary; apply still writes a
        // legal entry.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_compaction(&app_state, "u1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("text-only generate");
        assert_eq!(
            draft.summary, "",
            "empty dropped span → empty deterministic summary"
        );
        apply_compaction(&app_state, "u1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        match s.get_entry(&new_leaf).await.expect("entry").content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "u1");
                assert_eq!(summary, "");
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_compaction_llm_root_target_refuses() {
        // Llm at the branch root has nothing before the target: the one
        // "nothing before" guard refuses before any agent work.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let err = generate_compaction(&app_state, "u1", CompactSummaryMode::Llm, "payload")
            .await
            .expect_err("root-level Llm must refuse");
        assert!(err.contains("nothing before this point"), "err: {err}");
        assert_eq!(
            s.get_leaf_id().await.as_deref(),
            Some("a1"),
            "leaf untouched"
        );
        assert!(!has_compaction_entry(&s).await, "no write");
    }

    // ── Branch summarization (`/tree <id> switch summarize`) ─────────────────

    /// A spur session with the leaf at `a2`:
    ///
    /// ```text
    /// u1 ─ a1 ─ a2 ─ a3
    ///     └─ u2
    /// ```
    ///
    /// Switching to `u1` abandons `a1` + `a2` (a spur); `a3` is *ahead* of the
    /// leaf, so switching to it abandons nothing. The builder parks the leaf on
    /// the last entry, so it is moved to `a2` explicitly.
    async fn branch_summarize_session(dir: &std::path::Path) -> std::sync::Arc<Session> {
        let s = std::sync::Arc::new(
            build_writable_session_with(
                dir,
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                    message_entry("u2", Some("u1"), "user", "sibling path"),
                    message_entry("a2", Some("a1"), "assistant", "abandoned work"),
                    message_entry("a3", Some("a2"), "assistant", "the next turn"),
                ],
            )
            .await,
        );
        s.move_leaf(Some("a2".to_string())).await.expect("leaf");
        s
    }

    fn spur_ids(entries: &[crate::session::SessionEntry]) -> Vec<String> {
        entries.iter().map(|e| e.id.clone()).collect()
    }

    #[tokio::test]
    async fn abandoned_spur_is_what_the_switch_leaves_behind() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let index = s.index().lock().await;
        // Switching to a sibling: everything after the common ancestor (u1).
        assert_eq!(
            spur_ids(&abandoned_spur(&index, "a2", "u2")),
            vec!["a1", "a2"],
            "chronological, common ancestor excluded"
        );
        // Switching to the branch point abandons the whole spur.
        assert_eq!(spur_ids(&abandoned_spur(&index, "a2", "u1")), ["a1", "a2"]);
        // Switching forward/down abandons nothing — pi skips the summary here.
        assert!(abandoned_spur(&index, "a2", "a3").is_empty());
        assert!(abandoned_spur(&index, "a2", "a2").is_empty(), "no move");
        // An id outside the tree has no shared ancestor: the whole old path is the
        // spur (no panic) — callers reject the unknown target first.
        assert_eq!(
            spur_ids(&abandoned_spur(&index, "a2", "nope")),
            ["u1", "a1", "a2"]
        );
    }

    #[tokio::test]
    async fn abandoned_spur_handles_a_long_linear_session_without_recursing() {
        // Large sessions are a linked list of depth N; the collection must be
        // iterative (explicit walks, no recursion) to survive the stack.
        let n = 20_000;
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let parent = if i == 0 {
                None
            } else {
                Some(format!("e{}", i - 1))
            };
            entries.push(message_entry(
                &format!("e{}", i),
                parent.as_deref(),
                "user",
                "x",
            ));
        }
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(build_writable_session_with(_g.path(), entries).await);
        let index = s.index().lock().await;
        let spur = abandoned_spur(&index, &format!("e{}", n - 1), "e0");
        assert_eq!(spur.len(), n - 1, "the whole tail past e0");
    }

    #[tokio::test]
    async fn generate_branch_summary_text_only_and_apply_moves_the_leaf() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        let draft = generate_branch_summary(&app_state, "u1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("generate");
        assert!(
            draft.summary.contains("abandoned work"),
            "the spur is what got summarized: {:?}",
            draft.summary
        );
        assert!(
            !draft.summary.contains("first prompt"),
            "the kept prefix must not be summarized: {:?}",
            draft.summary
        );
        assert_eq!(
            draft.tokens_before, 0,
            "BranchSummary entries carry no tokensBefore"
        );
        // Nothing written yet, and the leaf has not moved.
        assert!(!has_branch_summary_entry(&s).await);
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("a2"));

        apply_branch_summary(&app_state, "u1", "a2", &draft)
            .await
            .expect("apply");

        // pi's `branchWithSummary`: parentId = fromId = the switch target, and
        // the summary entry is the new leaf — inline on the branch the
        // conversation continues on.
        let leaf = s.get_leaf_id().await.expect("leaf");
        let entry = s.get_entry(&leaf).await.expect("entry");
        match entry.content {
            crate::session::SessionEntryContent::BranchSummary { from_id, summary } => {
                assert_eq!(from_id, "u1", "fromId is the branch-from position");
                assert_eq!(summary, draft.summary);
            }
            other => panic!("expected BranchSummary, got {other:?}"),
        }
        assert_eq!(entry.parent_id.as_deref(), Some("u1"));
    }

    #[tokio::test]
    async fn branch_summary_reaches_the_context_of_the_branch_it_was_written_on() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        let draft = generate_branch_summary(&app_state, "u1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("generate");
        apply_branch_summary(&app_state, "u1", "a2", &draft)
            .await
            .expect("apply");

        let ctx = s.build_context().await.expect("context");
        let joined: String = ctx
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User { content } => match content.first() {
                    Some(UserContent::Text(t)) => Some(t.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("Branch summary:"),
            "the wrapper must be the LLM-visible form: {joined}"
        );
        assert!(joined.contains("abandoned work"), "{joined}");
        // The abandoned spur itself is NOT in the new branch's context.
        assert!(
            !joined.contains("abandoned work\n"),
            "spur dropped: {joined}"
        );
    }

    #[tokio::test]
    async fn generate_branch_summary_refuses_without_a_spur_or_a_move() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        for (target, needle) in [
            ("a2", "Already at this entry"), // the leaf itself
            ("a3", "ahead"),                 // forward navigation: nothing abandoned
        ] {
            let err = generate_branch_summary(&app_state, target, CompactSummaryMode::TextOnly, "")
                .await
                .expect_err("nothing to summarize");
            assert!(err.contains(needle), "target {target}: {err}");
        }
        let err = generate_branch_summary(&app_state, "nope", CompactSummaryMode::TextOnly, "")
            .await
            .expect_err("unknown entry");
        assert!(err.contains("unknown tree entry"), "{err}");
        assert!(!has_branch_summary_entry(&s).await, "no write");
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("a2"), "leaf kept");
    }

    #[tokio::test]
    async fn apply_branch_summary_refuses_a_moved_leaf_and_an_empty_summary() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        let draft = CompactionDraft {
            summary: "digest".into(),
            tokens_before: 0,
        };
        let err = apply_branch_summary(&app_state, "u1", "a1", &draft)
            .await
            .expect_err("leaf moved since generation");
        assert!(err.contains("moved"), "{err}");
        assert!(!has_branch_summary_entry(&s).await, "no write");
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("a2"), "leaf kept");

        // An empty digest writes nothing either — the user discarded it.
        let empty = CompactionDraft {
            summary: "   ".into(),
            tokens_before: 0,
        };
        let err = apply_branch_summary(&app_state, "u1", "a2", &empty)
            .await
            .expect_err("empty summary");
        assert!(err.contains("empty"), "{err}");
        assert!(!has_branch_summary_entry(&s).await, "no write");
    }

    #[tokio::test]
    async fn generate_branch_summary_manual_mode_lists_the_spur_files() {
        // A `write` tool call in the spur is the file provenance the manual
        // template surfaces.
        let mut assistant = message_entry("a1", Some("u1"), "assistant", "editing");
        if let crate::session::SessionEntryContent::Message { message } = &mut assistant.content {
            message["content"] = serde_json::json!([
                {"type": "text", "text": "editing"},
                {"type": "toolCall", "name": "write",
                 "arguments": {"path": "notes.md", "content": "hi"}},
            ]);
        }
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    assistant,
                    message_entry("a2", Some("a1"), "assistant", "abandoned work"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_branch_summary(&app_state, "u1", CompactSummaryMode::Manual, "")
            .await
            .expect("generate");
        assert!(draft.summary.contains("notes.md"), "{:?}", draft.summary);
        assert!(
            draft.summary.contains("modified"),
            "a write is a modification: {:?}",
            draft.summary
        );
    }

    // ── `/tree <id> edit` (pi's user-message re-edit) ─────────────────────

    /// A session whose leaf is at `a1`: `u1` (user) ─ `a1` (assistant).
    async fn reask_session(dir: &std::path::Path) -> std::sync::Arc<Session> {
        let s = std::sync::Arc::new(
            build_writable_session_with(
                dir,
                vec![
                    message_entry("u1", None, "user", "the original prompt"),
                    message_entry("a1", Some("u1"), "assistant", "the reply"),
                ],
            )
            .await,
        );
        s.move_leaf(Some("a1".to_string())).await.expect("leaf");
        s
    }

    #[tokio::test]
    async fn reask_moves_to_the_parent_and_prefills_the_prompt() {
        let _g = tempfile::tempdir().unwrap();
        let s = reask_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        assert!(is_reaskable_entry(&s, "u1").await, "a user prompt");
        assert!(
            !is_reaskable_entry(&s, "a1").await,
            "not an assistant reply"
        );
        assert!(!is_reaskable_entry(&s, "nope").await, "unknown entry");

        reask_from_entry(&app_state, "u1").await.expect("re-ask");

        // The prompt leaves the active branch — re-asking replaces it.
        assert_eq!(
            s.get_leaf_id().await,
            None,
            "a root prompt's parent is the root, not the prompt"
        );
        assert_eq!(
            app_state.take_prefill().as_deref(),
            Some("the original prompt"),
            "the prompt rides the one-shot prefill register"
        );
    }

    #[tokio::test]
    async fn reask_refuses_a_non_prompt_entry_without_moving_the_leaf() {
        let _g = tempfile::tempdir().unwrap();
        let s = reask_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        let err = reask_from_entry(&app_state, "a1")
            .await
            .expect_err("assistant replies cannot be re-asked");
        assert!(err.contains("user prompt"), "{err}");
        assert_eq!(
            s.get_leaf_id().await.as_deref(),
            Some("a1"),
            "leaf untouched"
        );
        assert!(app_state.take_prefill().is_none(), "no prefill");
        let err = reask_from_entry(&app_state, "nope")
            .await
            .expect_err("unknown entry");
        assert!(err.contains("unknown tree entry"), "{err}");
    }

    #[tokio::test]
    async fn serialize_spur_uses_pi_role_tags_and_skips_tool_results() {
        let mut assistant = message_entry("a1", Some("u1"), "assistant", "reply text");
        if let crate::session::SessionEntryContent::Message { message } = &mut assistant.content {
            message["content"] = serde_json::json!([
                {"type": "text", "text": "reply text"},
                {"type": "toolCall", "name": "read", "arguments": {"path": "src/lib.rs"}},
            ]);
        }
        let mut tool_result = message_entry("t1", Some("a1"), "user", "");
        if let crate::session::SessionEntryContent::Message { message } = &mut tool_result.content {
            *message = serde_json::json!({
                "role": "toolResult",
                "toolCallId": "c1",
                "toolName": "read",
                "content": [{"type": "text", "text": "file contents"}],
            });
        }
        let spur = vec![
            message_entry("u1", None, "user", "do the thing"),
            assistant,
            tool_result,
            crate::session::SessionEntry::new_branch_summary(
                "x".into(),
                "earlier digest".into(),
                Some("a1".into()),
            ),
        ];
        let text = serialize_spur(&spur);
        assert!(text.contains("[User]: do the thing"), "{text}");
        assert!(text.contains("[Assistant]: reply text"), "{text}");
        assert!(
            text.contains("[Assistant tool calls]: read(path=\"src/lib.rs\")"),
            "{text}"
        );
        assert!(
            !text.contains("file contents"),
            "tool results are skipped (their content is in the call): {text}"
        );
        assert!(
            text.contains("[User]: Branch summary:"),
            "a nested digest survives as the wrapped user message: {text}"
        );
    }

    #[tokio::test]
    async fn generate_compaction_llm_without_agent_errors() {
        // A non-root target passes the guard; agent creation then fails
        // (provider `test` is unsupported) → structured error, no write.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let err = generate_compaction(&app_state, "a1", CompactSummaryMode::Llm, "user payload")
            .await
            .expect_err("no agent can be created in tests");
        assert!(err.contains("Cannot start agent"), "err: {err}");
        assert!(!has_compaction_entry(&s).await, "no write on failure");
    }

    #[tokio::test]
    async fn generate_apply_text_only_nonzero_dropped_span() {
        // target a1, dropped = [u1]: the summary carries the dropped
        // user prompt; apply stores it with firstKept=a1.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "the question"),
                    message_entry("a1", Some("u1"), "assistant", "the answer"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_compaction(&app_state, "a1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("text-only generate");
        assert!(
            draft.summary.contains("the question"),
            "draft should contain the dropped user prompt, got: {:?}",
            draft.summary
        );
        apply_compaction(&app_state, "a1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        match s.get_entry(&new_leaf).await.expect("entry").content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "a1");
                assert!(
                    summary.contains("the question"),
                    "stored summary should contain the dropped user prompt, got: {:?}",
                    summary
                );
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_restores_leaf_manual_over_leaf_compaction() {
        // Branch [u0, a0, C1{firstKept:u0}] with leaf C1: manual generate
        // at u0 (no predecessor — root-level pivot) must restore the leaf
        // it pivoted before returning; apply then appends a fresh entry.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u0", None, "user", "prompt one"),
                    message_entry("a0", Some("u0"), "assistant", "reply one"),
                    compaction_entry("c1", Some("a0"), "earlier summary", "u0", 50),
                ],
            )
            .await,
        );
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("c1"));
        let tokens = expected_tokens(&s).await;
        let app_state = compaction_app_state(s.clone());

        let draft = generate_compaction(&app_state, "u0", CompactSummaryMode::Manual, "")
            .await
            .expect("manual re-compaction over a leaf-compaction");
        assert_eq!(
            s.get_leaf_id().await.as_deref(),
            Some("c1"),
            "generate must restore the leaf it pivoted"
        );
        assert_eq!(draft.tokens_before, tokens);

        apply_compaction(&app_state, "u0", &draft)
            .await
            .expect("apply");
        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        assert_ne!(new_leaf, "c1", "a new compaction entry must be appended");
        match s.get_entry(&new_leaf).await.expect("entry").content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
            } => {
                assert_eq!(summary, "");
                assert_eq!(first_kept_entry_id, "u0");
                assert_eq!(tokens_before, tokens);
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_apply_text_only_skips_nested_summaries() {
        // [u0, a0, u1, a1, C1{firstKept:u0, "EARLY SECRET SUMMARY"}],
        // leaf C1; target u1 → dropped [u0, a0]: the new summary includes
        // the raw dropped text but never the earlier compaction's text.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u0", None, "user", "first question"),
                    message_entry("a0", Some("u0"), "assistant", "first answer"),
                    message_entry("u1", Some("a0"), "user", "second question"),
                    message_entry("a1", Some("u1"), "assistant", "second answer"),
                    compaction_entry("c1", Some("a1"), "EARLY SECRET SUMMARY", "u0", 80),
                ],
            )
            .await,
        );
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("c1"));
        let app_state = compaction_app_state(s.clone());

        let draft = generate_compaction(&app_state, "u1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("text-only re-compaction");
        assert!(
            draft.summary.contains("first question"),
            "draft should include dropped user prompt, got: {:?}",
            draft.summary
        );
        assert!(
            !draft.summary.contains("EARLY SECRET SUMMARY"),
            "draft must NOT leak the nested compaction's summary, got: {:?}",
            draft.summary
        );
        apply_compaction(&app_state, "u1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        match s.get_entry(&new_leaf).await.expect("entry").content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "u1");
                assert!(
                    summary.contains("first question"),
                    "stored summary should include dropped user prompt, got: {:?}",
                    summary
                );
                assert!(
                    !summary.contains("EARLY SECRET SUMMARY"),
                    "stored summary must NOT leak the nested compaction's text, got: {:?}",
                    summary
                );
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn apply_stores_edited_draft() {
        // The UI review step hands an edited draft to apply: generate →
        // (the node's $EDITOR swaps the text) → apply stores the EDITED
        // summary, not the generated one.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "the question"),
                    message_entry("a1", Some("u1"), "assistant", "the answer"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let mut draft = generate_compaction(&app_state, "a1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("generate");
        assert!(
            draft.summary.contains("the question"),
            "generate must receive the dropped span, got: {:?}",
            draft.summary
        );
        draft.summary = format!("{}\n\nEDITED BY USER", draft.summary);
        apply_compaction(&app_state, "a1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        match s.get_entry(&new_leaf).await.expect("entry").content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "a1");
                assert!(
                    summary.contains("EDITED BY USER"),
                    "stored summary must be the edited text, got: {:?}",
                    summary
                );
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_without_apply_writes_nothing() {
        // Cancel (Esc at the engine's confirm, i.e. never reaching
        // submit) = simply not applying: generate leaves the session
        // byte-identical — leaf unchanged, no compaction entry, branch
        // length untouched.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "the question"),
                    message_entry("a1", Some("u1"), "assistant", "the answer"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_compaction(&app_state, "a1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("generate");
        assert!(!draft.summary.is_empty(), "draft was generated");

        assert_eq!(s.get_leaf_id().await.as_deref(), Some("a1"));
        assert!(!has_compaction_entry(&s).await, "no entry without apply");
        assert_eq!(s.get_branch().await.len(), 2, "branch untouched");
    }

    #[tokio::test]
    async fn generate_compaction_llm_leaf_compaction_errors_cleanly() {
        // [u0, a0, C1] with leaf C1, target a0 (predecessor u0 — passes
        // the guard): generation must return a structured error (no
        // agent can be built in tests) and never panic, even when the
        // leaf is already a compaction.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u0", None, "user", "prompt one"),
                    message_entry("a0", Some("u0"), "assistant", "reply one"),
                    compaction_entry("c1", Some("a0"), "earlier summary", "u0", 50),
                ],
            )
            .await,
        );
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("c1"));
        let app_state = compaction_app_state(s.clone());
        let err = generate_compaction(&app_state, "a0", CompactSummaryMode::Llm, "user payload")
            .await
            .expect_err("no agent can be created in tests");
        assert!(err.contains("Cannot start agent"), "err: {err}");
        assert_eq!(
            s.get_leaf_id().await.as_deref(),
            Some("c1"),
            "failed generation must leave the leaf where it was"
        );
        // The fixture already contains c1 — failure must not add another.
        let count = s
            .index()
            .lock()
            .await
            .entries
            .values()
            .filter(|e| {
                matches!(
                    e.content,
                    crate::session::SessionEntryContent::Compaction { .. }
                )
            })
            .count();
        assert_eq!(count, 1, "only the fixture's c1 — no new entry");
    }

    #[test]
    fn reasoning_text_extracts_text_block() {
        let r = Reasoning::new("think step by step");
        assert_eq!(reasoning_text(&r), "think step by step");
    }

    #[test]
    fn reasoning_text_extracts_text_with_signature() {
        let r = Reasoning::new_with_signature("with sig", Some("sig".to_string()));
        assert_eq!(reasoning_text(&r), "with sig");
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

    /// End-to-end: a real streaming run through `LoggingHook` must log both the
    /// context sent (`on_completion_call`) and the assistant content received
    /// (`on_model_turn_finished`) — the streaming surface never calls
    /// `on_completion_response`, so the received side must come from the
    /// per-turn hook. Regression test for "all messages marked sent".
    #[tokio::test]
    async fn streaming_run_logs_sent_and_received() {
        use futures::StreamExt;
        use rig_agent::agent::{AgentBuilder, MultiTurnStreamItem};
        use rig_agent::streaming::StreamingPrompt;
        use rig_core::completion::Usage;
        use rig_core::test_utils::{mock_final, MockCompletionModel, MockStreamEvent};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use super::LoggingHook;

        let _tmp = tempfile::tempdir().unwrap();
        let dir = _tmp.path().to_path_buf();
        let path = dir.join("messages.jsonl");
        let _ = tokio::fs::remove_file(&path).await;

        let logger = Arc::new(MessageLogger::new(
            path.display().to_string(),
            "test".to_string(),
            "test-model".to_string(),
        ));

        // One scripted streaming turn: text + terminal record.
        let model = MockCompletionModel::from_stream_turns([[
            MockStreamEvent::Text("hello there".to_string()),
            MockStreamEvent::FinalResponse(mock_final(Usage::new())),
        ]]);
        let agent = AgentBuilder::new(model).build();

        let mut stream = agent
            .stream_prompt("hi")
            .add_hook(LoggingHook {
                output: Arc::new(crate::NoOutput),
                user_cancel: Arc::new(AtomicBool::new(false)),
                message_logger: Some(logger.clone()),
                reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
                provider: "test".to_string(),
                model: "test-model".to_string(),
                transcript: Arc::new(std::sync::Mutex::new(super::TurnTranscript::default())),
            })
            .max_turns(1)
            .await;

        let mut final_text = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(
                    rig_agent::streaming::StreamedAssistantContent::Text(t),
                )) => final_text.push_str(t.text()),
                Ok(_) => {}
                Err(e) => panic!("stream error: {e}"),
            }
        }
        assert_eq!(final_text, "hello there");

        let raw = tokio::fs::read_to_string(&path)
            .await
            .expect("log file must be created");
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        let sent: Vec<_> = lines.iter().filter(|l| l["direction"] == "sent").collect();
        let received: Vec<_> = lines
            .iter()
            .filter(|l| l["direction"] == "received")
            .collect();

        // One user prompt sent, one assistant reply received.
        assert_eq!(sent.len(), 1, "expected exactly one sent message: {raw}");
        assert_eq!(
            received.len(),
            1,
            "expected exactly one received message: {raw}"
        );
        assert_eq!(received[0]["role"], "assistant");
        assert_eq!(received[0]["message"]["content"][0]["text"], "hello there");
        // Both sides of the same turn share a turn id.
        assert_eq!(sent[0]["turn_id"], received[0]["turn_id"]);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(unix)]
    #[test]
    fn create_private_dir_sets_0700() {
        use std::os::unix::fs::PermissionsExt;
        let _tmp = tempfile::tempdir().unwrap();
        // A child that does not exist yet: `create_private_dir` creates it with
        // mode 0700, so the directory must be absent before the call.
        let dir = _tmp.path().join("private_0700");
        create_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Regression test for root cause #1: a successful turn must *append* to the
    // existing history, preserving prior turns. The old code replaced the whole
    // history with just the latest turn, so earlier exchanges were lost every
    // turn.
    #[test]
    fn successful_turn_appends_to_history() {
        let mut hist: Vec<Message> = vec![Message::user("prior-1")];
        persist_turn(
            &mut Some(&mut hist),
            Some(vec![
                Message::user("turn-2-prompt"),
                Message::assistant("turn-2-reply"),
            ]),
            None,
            crate::settings::ReasoningInContextMode::Always,
            "openai",
            "",
        );

        // Prior turn preserved, new turn appended (not wiped/replaced).
        assert_eq!(hist.len(), 3);
        assert!(hist.contains(&Message::user("prior-1")));
        assert!(hist.contains(&Message::user("turn-2-prompt")));
        assert!(hist.contains(&Message::assistant("turn-2-reply")));
        // Prior must come before the new turn.
        let prior_idx = hist
            .iter()
            .position(|m| *m == Message::user("prior-1"))
            .unwrap();
        let new_idx = hist
            .iter()
            .position(|m| *m == Message::user("turn-2-prompt"))
            .unwrap();
        assert!(prior_idx < new_idx);
    }

    // Regression test for root cause #2: an aborted turn must keep the prior
    // turns and commit the in-flight full history *without* duplicating the
    // priors (rig's cancellation error already includes them).
    #[test]
    fn aborted_turn_keeps_prior_and_no_duplication() {
        let mut hist: Vec<Message> = vec![Message::user("prior-1")];
        // rig's `PromptCancelled.chat_history` == full_history() == prior + this turn.
        let cancelled = vec![
            Message::user("prior-1"),
            Message::user("aborted-prompt"),
            Message::assistant("aborted-partial"),
        ];
        use crate::settings::ReasoningInContextMode;
        persist_turn(
            &mut Some(&mut hist),
            None,
            Some(cancelled),
            ReasoningInContextMode::Always,
            "openai",
            "",
        );

        // Replaced (not appended): exactly the in-flight full history, with no
        // duplicate of `prior-1`.
        assert_eq!(hist.len(), 3);
        assert_eq!(
            hist.iter()
                .filter(|m| **m == Message::user("prior-1"))
                .count(),
            1
        );
        assert!(hist.contains(&Message::user("aborted-prompt")));
        assert!(hist.contains(&Message::assistant("aborted-partial")));
    }

    // `reasoning_in_context=Never` must drop reasoning blocks from assistant
    // messages for non-Anthropic providers, while keeping text (and keeping
    // reasoning entirely for Anthropic-family turns that involve tool calls, or
    // when the mode is Always). On an Anthropic turn *without* tool calls, the
    // mode is honored and reasoning is stripped.
    #[test]
    fn strips_reasoning_for_non_anthropic_when_never() {
        use crate::settings::ReasoningInContextMode;
        let assistant = Message::Assistant {
            id: None,
            content: vec![
                AssistantContent::Reasoning(Reasoning::new("hidden thought")),
                AssistantContent::Text(Text::new("answer")),
            ],
        };
        let msgs = vec![Message::user("q"), assistant];

        // Never + non-Anthropic: reasoning dropped, text preserved.
        let out = maybe_strip_reasoning(msgs.clone(), ReasoningInContextMode::Never, "openai", "");
        if let Message::Assistant { content, .. } = &out[1] {
            assert!(!content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))));
            assert!(content
                .iter()
                .any(|c| matches!(c, AssistantContent::Text(_))));
        } else {
            panic!("expected assistant message");
        }

        // Always: reasoning kept.
        let kept =
            maybe_strip_reasoning(msgs.clone(), ReasoningInContextMode::Always, "openai", "");
        if let Message::Assistant { content, .. } = &kept[1] {
            assert!(content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))));
        } else {
            panic!("expected assistant message");
        }

        // Anthropic-family WITHOUT tool calls: mode honored, reasoning stripped.
        let stripped = maybe_strip_reasoning(
            msgs.clone(),
            ReasoningInContextMode::Never,
            "anthropic",
            "claude-3-5-sonnet",
        );
        if let Message::Assistant { content, .. } = &stripped[1] {
            assert!(!content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))));
            assert!(content
                .iter()
                .any(|c| matches!(c, AssistantContent::Text(_))));
        } else {
            panic!("expected assistant message");
        }
    }

    /// Test AbortedTurnsOnly mode: preserves reasoning for aborted turns (id: None)
    /// but strips from successful turns (id: Some).
    #[test]
    fn aborted_turns_only_preserves_reasoning_for_aborted() {
        use crate::settings::ReasoningInContextMode;
        // Successful turn: has responseId
        let successful_assistant = Message::Assistant {
            id: Some("resp-123".into()),
            content: vec![
                AssistantContent::Reasoning(Reasoning::new("successful thought")),
                AssistantContent::Text(Text::new("successful answer")),
            ],
        };
        // Aborted turn: no responseId
        let aborted_assistant = Message::Assistant {
            id: None,
            content: vec![
                AssistantContent::Reasoning(Reasoning::new("aborted thought")),
                AssistantContent::Text(Text::new("aborted answer")),
            ],
        };
        let msgs = vec![
            Message::user("q1"),
            successful_assistant,
            Message::user("q2"),
            aborted_assistant,
        ];

        // AbortedTurnsOnly: successful reasoning stripped, aborted reasoning kept
        let out = maybe_strip_reasoning(
            msgs.clone(),
            ReasoningInContextMode::AbortedTurnsOnly,
            "openai",
            "",
        );
        let contents: Vec<&Vec<AssistantContent>> = out
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(contents.len(), 2);
        // First (successful) should have reasoning stripped
        assert!(!contents[0]
            .iter()
            .any(|c| matches!(c, AssistantContent::Reasoning(_))));
        assert!(contents[0]
            .iter()
            .any(|c| matches!(c, AssistantContent::Text(t) if t.text == "successful answer")));
        // Second (aborted) should have reasoning preserved
        assert!(contents[1]
            .iter()
            .any(|c| matches!(c, AssistantContent::Reasoning(_))));
        assert!(contents[1]
            .iter()
            .any(|c| matches!(c, AssistantContent::Text(t) if t.text == "aborted answer")));
    }

    // Anthropic-family turn WITH a tool call: the leading reasoning must be
    // force-kept (thinking+tool_use replay contract), even with the flag off.
    #[test]
    fn anthropic_tool_call_turn_forces_reasoning_kept() {
        let assistant = Message::Assistant {
            id: None,
            content: vec![
                AssistantContent::Reasoning(Reasoning::new("hidden thought")),
                AssistantContent::Text(Text::new("answer")),
                AssistantContent::ToolCall(rig_core::message::ToolCall::new(
                    rig_core::message::ToolCallId::new("tool-1").unwrap(),
                    rig_core::message::ToolFunction::new(
                        "Bash".to_string(),
                        serde_json::json!({"cmd": "ls"}),
                    ),
                )),
            ],
        };
        let msgs = vec![Message::user("q"), assistant];

        let forced = maybe_strip_reasoning(
            msgs,
            crate::settings::ReasoningInContextMode::Never,
            "anthropic",
            "claude-3-5-sonnet",
        );
        if let Message::Assistant { content, .. } = &forced[1] {
            assert!(content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))));
        } else {
            panic!("expected assistant message");
        }
    }

    #[test]
    fn is_anthropic_family_classifies_providers() {
        assert!(is_anthropic_family("anthropic", "claude-x"));
        assert!(is_anthropic_family(
            "openrouter",
            "anthropic/claude-3.5-sonnet"
        ));
        assert!(!is_anthropic_family("openrouter", "openai/gpt-4o"));
        assert!(!is_anthropic_family("openai", "gpt-4o"));
    }

    // ── Non-REPL input path (editor cleanup E5) ───────────────────────

    // ── Phase 2: replay units ─────────────────────────────────────────

    fn pi_user_entry(id: &str, parent: Option<&str>, text: &str) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|p| p.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".to_string(),
            content: crate::session::SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    fn pi_assistant_entry(
        id: &str,
        parent: Option<&str>,
        text: &str,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|p| p.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".to_string(),
            content: crate::session::SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "assistant",
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    fn pi_branch_summary_entry(
        id: &str,
        parent: Option<&str>,
        from_id: &str,
        summary: &str,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|p| p.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".to_string(),
            content: crate::session::SessionEntryContent::BranchSummary {
                from_id: from_id.to_string(),
                summary: summary.to_string(),
            },
        }
    }

    fn pi_compaction_entry(
        id: &str,
        parent: Option<&str>,
        tokens: u64,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|p| p.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".to_string(),
            content: crate::session::SessionEntryContent::Compaction {
                summary: String::new(),
                first_kept_entry_id: "m0".to_string(),
                tokens_before: tokens,
            },
        }
    }

    #[test]
    fn build_replay_units_marks_compaction_and_keeps_all_turns() {
        let branch = vec![
            pi_user_entry("m0", None, "hello"),
            pi_assistant_entry("a0", Some("m0"), "hi there"),
            pi_compaction_entry("c1", Some("a0"), 42_000),
            pi_user_entry("m1", Some("c1"), "continue"),
        ];
        let units = build_replay_units(&branch);
        // [Turn(hello,hi), Compaction, Turn(continue)]
        assert_eq!(units.len(), 3);
        assert!(matches!(units[0], ReplayUnit::Turn(_)));
        match &units[1] {
            ReplayUnit::Compaction {
                tokens_before,
                summary,
            } => {
                assert_eq!(*tokens_before, 42_000);
                assert_eq!(summary, ""); // manual-mode compaction (no files touched)
            }
            _ => panic!("expected compaction marker"),
        }
        assert!(matches!(units[2], ReplayUnit::Turn(_)));
        // Both turns present → full history replays to the beginning.
        assert_eq!(count_turns(&units), 2);
    }

    #[test]
    fn build_replay_units_marks_a_branch_summary_in_path_order() {
        // A switch-summarize session: the abandoned turn, then the digest at
        // the switch target, then the prompt that continues on that branch.
        let branch = vec![
            pi_user_entry("m0", None, "try the side quest"),
            pi_assistant_entry("a0", Some("m0"), "working on it"),
            pi_branch_summary_entry("s1", Some("a0"), "m0", "## Goal\nship the thing"),
            pi_user_entry("m1", Some("s1"), "back to the main line"),
        ];
        let units = build_replay_units(&branch);
        assert_eq!(units.len(), 3, "[Turn, BranchSummary, Turn]");
        assert!(matches!(units[0], ReplayUnit::Turn(_)));
        match &units[1] {
            ReplayUnit::BranchSummary { from_id, summary } => {
                assert_eq!(from_id, "m0", "fromId is the branch-from position");
                assert_eq!(summary, "## Goal\nship the thing");
            }
            _ => panic!("expected a branch-summary marker"),
        }
        assert!(matches!(units[2], ReplayUnit::Turn(_)));
        // A marker is not a turn: the Nt/Nl windows count turns only.
        assert_eq!(count_turns(&units), 2);
    }

    #[test]
    fn build_replay_units_skips_an_empty_branch_summary() {
        // Nothing to say → no marker, matching pi's context mapping (which
        // skips an empty summary) rather than showing an empty notice.
        let branch = vec![
            pi_user_entry("m0", None, "hello"),
            pi_branch_summary_entry("s1", Some("m0"), "m0", "   "),
            pi_user_entry("m1", Some("s1"), "again"),
        ];
        let units = build_replay_units(&branch);
        assert_eq!(units.len(), 2, "an empty digest emits no marker");
        assert!(units.iter().all(|u| matches!(u, ReplayUnit::Turn(_))));
    }

    #[test]
    fn replay_units_renders_the_branch_summary_marker_and_digest() {
        let branch = vec![
            pi_user_entry("m0", None, "hello"),
            pi_branch_summary_entry("s1", Some("m0"), "m0", "## Goal\nship it"),
            pi_user_entry("m1", Some("s1"), "continue"),
        ];
        let units = build_replay_units(&branch);
        let (out, lines) = crate::output::RecordingOutput::new();
        replay_units(out.as_ref(), &units, 0);
        let captured = crate::output::RecordingOutput::joined(&lines);
        assert!(
            captured.contains("[branch summary from m0]"),
            "marker carries the copy-pasteable id: {captured}"
        );
        assert!(
            captured.contains("ship it"),
            "the digest renders below the marker: {captured}"
        );
    }

    #[test]
    fn build_replay_units_uncompacted_has_all_turns() {
        let branch = vec![
            pi_user_entry("m0", None, "q1"),
            pi_assistant_entry("a0", Some("m0"), "r1"),
            pi_user_entry("m1", Some("a0"), "q2"),
            pi_assistant_entry("a1", Some("m1"), "r2"),
        ];
        let units = build_replay_units(&branch);
        assert_eq!(units.len(), 2);
        assert!(matches!(units[0], ReplayUnit::Turn(_)));
        assert!(matches!(units[1], ReplayUnit::Turn(_)));
        assert_eq!(count_turns(&units), 2);
        // last_turns_start: the last turn begins at unit index 1.
        assert_eq!(last_turns_start(&units, 1), 1);
    }

    #[test]
    fn last_turns_start_clamps_and_honors_markers() {
        // [T0] [C] [T1] [T2]
        let branch = vec![
            pi_user_entry("m0", None, "q0"),
            pi_compaction_entry("c1", Some("m0"), 10_000),
            pi_user_entry("m1", Some("c1"), "q1"),
            pi_user_entry("m2", Some("m1"), "q2"),
        ];
        let units = build_replay_units(&branch);
        assert_eq!(count_turns(&units), 3);
        // Last 2 turns: [T1, T2] → they start at unit index 2 (after [T0][C]).
        assert_eq!(last_turns_start(&units, 2), 2);
        // More turns than exist → clamp to 0.
        assert_eq!(last_turns_start(&units, 99), 0);
        // Zero turns requested → empty range (start == len).
        assert_eq!(last_turns_start(&units, 0), units.len());
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
    // ---- TurnTranscript accumulation & finalization ----
    //
    // The transcript mirrors rig's `new_messages` shape so a mid-turn failure
    // can persist exactly what completed (doc/tool_error_handling.md
    // §Mid-turn failure). These tests pin the accumulation order and the
    // no-truncation finalization: unanswered trailing tool calls are answered
    // with synthesized text results, never dropped.

    #[test]
    fn transcript_batches_tool_results_between_assistant_turns() {
        use super::TurnTranscript;
        use rig_core::message::{ToolCall, ToolFunction, ToolResultContent};

        let tc = ToolCall::from_wire(
            "call_1",
            ToolFunction::new("read".to_string(), serde_json::json!({})),
        );
        let call_id = tc.id.clone();
        let mut t = TurnTranscript::default();
        // Assistant turn with a tool call.
        t.push_assistant_turn(None, vec![AssistantContent::ToolCall(tc)]);
        assert!(t.has_progress());
        // Tool result streams back (batch accumulates).
        t.push_tool_result(rig_core::message::ToolResult {
            call: call_id,
            provider: None,
            name: "read".to_string(),
            content: vec![ToolResultContent::text("ok")],
        });
        // Next assistant turn flushes the batch as one Message::User.
        t.push_assistant_turn(None, vec![AssistantContent::text("done")]);
        let f = t.finalize();
        assert_eq!(f.len(), 3, "assistant + user(results) + assistant: {f:?}");
        assert!(
            matches!(&f[1], Message::User { .. }),
            "batch must be one user msg"
        );
    }

    #[test]
    fn transcript_finalize_answers_interrupted_calls_with_text_results() {
        use super::TurnTranscript;
        use rig_core::message::{ToolCall, ToolFunction, ToolResultContent, UserContent};

        let tc = ToolCall::from_wire(
            "call_1",
            ToolFunction::new("read".to_string(), serde_json::json!({})),
        );
        let mut t = TurnTranscript::default();
        t.push_assistant_turn(None, vec![AssistantContent::ToolCall(tc)]);
        // Run interrupted before any tool result: finalize must synthesize a
        // text result per unanswered call — never truncate.
        let f = t.finalize();
        assert_eq!(f.len(), 2, "assistant + synthesized user: {f:?}");
        let Message::User { content } = &f[1] else {
            panic!("expected a user message");
        };
        let UserContent::ToolResult(tr) = &content[0] else {
            panic!("expected a tool result");
        };
        assert_eq!(tr.name, "read");
        let ToolResultContent::Text(txt) = &tr.content[0] else {
            panic!("expected text content");
        };
        assert!(txt.text.contains("interrupted"), "got: {}", txt.text);
    }

    #[test]
    fn transcript_skips_turns_without_visible_content() {
        use super::TurnTranscript;

        let mut t = TurnTranscript::default();
        t.push_assistant_turn(None, vec![AssistantContent::text("   ")]);
        assert!(
            !t.has_progress(),
            "blank-text turn must not count as progress"
        );
        assert!(t.finalize().is_empty());
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

    // ---- per-turn overrides: the request path (doc/model_settings_v2.md A.10) ----

    fn store_with(default_reasoning: &str) -> crate::settings::SettingsStore {
        let mut s = crate::settings::SettingsStore::from_config(&crate::Config::default(), None);
        s.default_reasoning_level = Some(default_reasoning.to_string());
        s
    }

    fn app_with(params: crate::params::ModelParams) -> Arc<crate::app_state::AppState> {
        let app = crate::app_state::AppState::for_test(store_with("low"));
        app.update_config(|c| c.params = params);
        app
    }

    /// The load-bearing invariant. The per-request path *replaces* the value
    /// baked at build time, so if the two compositions ever disagreed, a turn
    /// with an override would send a different shape than a turn without one.
    /// Both now go through `request_additional_params`, and this asserts the
    /// identity that makes that safe.
    #[test]
    fn a_turn_with_no_override_recomputes_exactly_what_was_baked() {
        let params = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::High),
            temperature: Some(0.7),
            top_p: Some(0.9),
        };
        // What `create_agent` bakes...
        let baked = request_additional_params("openai", "gpt-4o", &params);
        // ...and what the per-request path computes for the same params.
        let per_request = request_additional_params("openai", "gpt-4o", &params);
        assert_eq!(baked, per_request);
    }

    /// The same identity for openrouter, whose branch pins a provider
    /// preference into the same object. Recomputing without that step would
    /// drop the preference on every override turn.
    #[test]
    fn the_openrouter_branch_keeps_its_provider_preference() {
        let params = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::High),
            ..Default::default()
        };
        let v = request_additional_params("openrouter", "anthropic/claude", &params)
            .expect("openrouter always has preferences");
        let obj = v.as_object().expect("an object");
        assert!(
            obj.contains_key("provider"),
            "the preference must survive recomposition: {obj:?}"
        );
    }

    /// An override replaces rather than merges: the reason key must carry the
    /// new value, not the old one alongside it.
    #[test]
    fn an_override_replaces_the_baked_reasoning() {
        let live = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::Low),
            ..Default::default()
        };
        let overrides = crate::turn_overrides::TurnOverrides {
            reasoning: Some(crate::params::ReasoningLevel::High),
            ..Default::default()
        };
        let effective = overrides.apply(&live);
        assert_eq!(
            effective.reasoning,
            Some(crate::params::ReasoningLevel::High)
        );
        let with_override = request_additional_params("openai", "gpt-4o", &effective);
        let without = request_additional_params("openai", "gpt-4o", &live);
        assert_ne!(
            with_override, without,
            "the override must actually change what goes on the wire"
        );
    }

    /// Overrides never touch the live params — the reason there is no restore
    /// step, and the reason one cannot reach the next turn or the per-model
    /// memory.
    #[test]
    fn an_override_leaves_the_live_params_alone() {
        let app = app_with(crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::Med),
            temperature: Some(0.7),
            ..Default::default()
        });
        let overrides = crate::turn_overrides::TurnOverrides {
            reasoning: Some(crate::params::ReasoningLevel::Max),
            temperature: Some(0.1),
            top_p: Some(0.2),
        };
        let (effective, _) = effective_params(&app, &overrides);
        assert_eq!(
            effective.reasoning,
            Some(crate::params::ReasoningLevel::Max)
        );
        assert_eq!(effective.temperature, Some(0.1));
        // The live params — and therefore the per-model memory — are unchanged.
        let live = app.params();
        assert_eq!(live.reasoning, Some(crate::params::ReasoningLevel::Med));
        assert_eq!(live.temperature, Some(0.7));
        assert_eq!(live.top_p, None);
    }

    #[test]
    fn no_overrides_means_no_notes_and_the_live_params() {
        let app = app_with(crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::Med),
            ..Default::default()
        });
        let (effective, notes) = effective_params(&app, &Default::default());
        assert!(notes.is_empty());
        assert_eq!(effective, app.params());
    }

    /// A level the model cannot serve is reported, because a silent downgrade
    /// the user cannot account for is worse than a note.
    #[test]
    fn a_level_the_model_lacks_is_reported() {
        // A model that publishes only `low`, so `max` has to move down.
        let support = crate::params::ReasoningSupport::new(vec!["low".to_string()]);
        let note = reasoning_remap_note(crate::params::ReasoningLevel::Max, &support)
            .expect("max cannot be served by a low-only model");
        assert!(note.contains("max"), "{note}");
        assert!(note.contains("low"), "{note}");
    }

    /// A pure spelling difference is not a downgrade, and must not be reported
    /// as one: our `med` and their `medium` are the same rank.
    #[test]
    fn a_spelling_difference_is_not_reported_as_a_remap() {
        let support = crate::params::ReasoningSupport::new(vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ]);
        assert_eq!(
            reasoning_remap_note(crate::params::ReasoningLevel::Med, &support),
            None,
            "med == medium, so nothing moved"
        );
    }

    /// An exactly-supported level is silent.
    #[test]
    fn a_supported_level_is_silent() {
        let support =
            crate::params::ReasoningSupport::new(vec!["low".to_string(), "high".to_string()]);
        assert_eq!(
            reasoning_remap_note(crate::params::ReasoningLevel::High, &support),
            None
        );
    }

    /// No data at all must not be reported as a remap: `resolve` returns `None`
    /// when the model publishes nothing, and silence is the honest answer.
    #[test]
    fn an_unknown_support_table_is_silent() {
        let support = crate::params::ReasoningSupport::unknown();
        assert_eq!(
            reasoning_remap_note(crate::params::ReasoningLevel::Max, &support),
            None
        );
    }
}
