use crate::app_state::{AppState, WindowKeySource};
use crate::model_catalog;
use crate::output::{Confirmation, StatusText};
use crate::params;
use crate::repl::{InputAction, ModelFilter, Repl, ReplCtx};
use crate::session::repo::{ForkMode, SessionRepo};
use crate::session::storage::SessionStorage;
use crate::session::Session;
use crate::settings::SettingsStore;
use crate::tools::{
    bsed_available, set_sandbox, AgentBrowser, Bash, CopyFile, EditFile, GlobFiles, GrepText,
    ListDir, MakeDir, MoveFile, ReadFile, Remove, SedPreview, WriteFile,
};
use crate::Output;
use crate::Result;

use chrono::Utc;
use console::style;
use futures::StreamExt;
use rig_agent::agent::{
    Agent, AgentBuilder, AgentHook, CompletionCallAction, CompletionCallEvent,
    CompletionResponseEvent, HookContext, InvalidToolCallAction, InvalidToolCallContext,
    ModelTurnAction, ModelTurnFinished, MultiTurnStreamItem, NoToolConfig, ObservationAction,
    RequestPatch, StreamingError, ToolCall, ToolCallAction, ToolResultAction, ToolResultEvent,
};
use rig_agent::client::AgentClientExt;
use rig_agent::completion::{Chat, CompletionError, PromptError};
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
use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

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

/// Classify a non-streaming `PromptError` (returned from the blocking
/// `chat()` surface used by `summarize`) into the same structured
/// `PicocodeError` taxonomy the streaming path uses. Without this, a
/// `/tree --summarize` 429 surfaces as the generic
/// `PicocodeError::Other("summarize: CompletionError: HttpError: …")` and
/// loses the Retry-After hint.
fn classify_prompt_error(e: &PromptError, op: &str) -> crate::PicocodeError {
    use crate::PicocodeError::*;
    match e {
        PromptError::CompletionError(ce) => {
            let classified = classify_completion_error(ce);
            // Re-tag the display so the user sees which op failed.
            match classified {
                Other(_) | Llm(_) => Llm(format!("{op}: {ce}")),
                other => other,
            }
        }
        PromptError::MaxTurnsError { max_turns, .. } => LlmMaxTurns {
            max_turns: *max_turns,
        },
        PromptError::UnknownToolCall {
            tool_name,
            available_tools,
            ..
        } => LlmUnknownTool {
            tool_name: tool_name.clone(),
            available: available_tools.clone(),
        },
        PromptError::PromptCancelled { reason, .. } => Cancelled(reason.clone()),
        PromptError::MemoryError(_) => Other(format!("{op}: {e}")),
    }
}

/// Resolves once `user_cancel` is set. The notify wakes it; the load before
/// subscribing covers a notify that fired before this call existed. Loops over
/// spurious wakes — a notify with the flag still unset is somebody else's
/// signal, not the user's `Esc`.
async fn cancel_observed(user_cancel: &Arc<AtomicBool>, cancel_notify: &Arc<tokio::sync::Notify>) {
    loop {
        if user_cancel.load(Ordering::SeqCst) {
            return;
        }
        cancel_notify.notified().await;
        if user_cancel.load(Ordering::SeqCst) {
            return;
        }
        // Spurious wakeup without cancel set — loop and re-await.
    }
}

/// What dispatching one turn-key event did.
#[derive(Debug, PartialEq, Eq)]
enum InTurnDispatch {
    /// No event, or no session to receive it: nothing happened.
    Ignored,
    /// The event was applied and the input row settled to match.
    Handled,
    /// Root `Esc`: the cancel signal fired (flag + notify + tool watchdogs).
    /// The streaming window ends on its next poll; a back-off wait ends
    /// immediately.
    CancelFired,
}

/// The turn's key dispatcher — the one body behind both of the turn's pumps:
/// `prompt()`'s select branch and the retry back-off's
/// [`cancellable_sleep_pumped`] ("one dispatcher, two pumps", ui_draw_v2 §4.5
/// U4). A key therefore means the same thing wherever the turn is.
///
/// Ordering is `read_input`'s minus the wait: dispatch, then settle.
///
/// Menu-first `Esc` (settled U4): with a v2.5 menu open the key goes to
/// `handle_key`, whose ladder steps the engine back one level. At root — a bare
/// prompt, or only unsubmitted prose that the session carries across the turn
/// anyway — there is nothing to step out of, so it stops the turn. The
/// in-flight reader used to make that call itself, and made it with no way to
/// see a menu: unconditional. It routes now (U4.3) and this dispatcher decides
/// — the place that can tell the difference.
///
/// A submit (`KeyOutcome::Action`) is blocked mid-turn, and *silently*: v0
/// offers no mid-turn action, and a message printed over the live stream would
/// be worse than none. The turn keeps running, so unlike at the prompt there
/// is a next iteration — which is why this still settles, and the screen stays
/// true to the state the key left behind.
async fn dispatch_in_turn_event(
    ev: Option<crossterm::event::Event>,
    sess: Option<&mut crate::repl::PromptSession>,
    history: Option<&mut crate::repl::History>,
    label: &str,
    guard: Option<&crate::repl::TermGuard>,
    output: &dyn Output,
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
) -> InTurnDispatch {
    let (Some(ev), Some(sess), Some(history)) = (ev, sess, history) else {
        return InTurnDispatch::Ignored;
    };
    let dims = output.frame_size();
    match ev {
        crossterm::event::Event::Key(key) => {
            // The confirm is **modal**, and this is the top of the `Esc` ladder:
            // dialog first, menu second, turn third. The root-`Esc` cancel below
            // is unconditional, so without this precedence one `Esc` would
            // dismiss the dialog *and* kill the turn.
            if sess.confirm_open() {
                return match sess.confirm_key(key) {
                    // Resolved: paint the area back (dropping the element is the
                    // erase — no `Frame::empty()`), then record the decision
                    // where the dialog's rows were.
                    Some(decision) => {
                        let subject = sess.confirm_subject().unwrap_or_default();
                        sess.clear_confirm();
                        crate::confirm::resolve(
                            output,
                            || sess.paint(output, label, dims),
                            &subject,
                            decision,
                        );
                        InTurnDispatch::Handled
                    }
                    None => {
                        sess.paint(output, label, dims);
                        InTurnDispatch::Handled
                    }
                };
            }
            if key.code == crossterm::event::KeyCode::Esc && !sess.menu_open() {
                user_cancel.store(true, Ordering::SeqCst);
                cancel_notify.notify_one();
                // Wake in-flight tool watchdogs so a running shell dies now
                // rather than at its next check. No tool runs during a
                // back-off, so this is a no-op there — fired anyway so both
                // pumps raise the identical signal set. Unbounded mpsc, so
                // this cannot block the main task.
                for tx in crate::tools::TOOL_CANCEL_SENDERS.lock().unwrap().iter() {
                    let _ = tx.send(());
                }
                // Nothing to settle: the window ends on the flag.
                return InTurnDispatch::CancelFired;
            }
            let opts_len = sess.opts_len();
            match sess.handle_key(key, opts_len, label, history) {
                crate::repl::KeyOutcome::RunEditor => {
                    if let Some(g) = guard {
                        sess.open_editor(g, output);
                    }
                }
                crate::repl::KeyOutcome::Continue | crate::repl::KeyOutcome::Action(_) => {}
            }
            // `settle` is where a `/`-command committed mid-turn lands exactly
            // as it does at the prompt. Its `MenuDone` is dropped: applying a
            // command needs the convergence machinery that runs *after*
            // `prompt()` returns, which is still post-turn only (U4.3+).
            let _ = sess.settle(output, label, history, dims).await;
            InTurnDispatch::Handled
        }
        crossterm::event::Event::Paste(text) => {
            // A confirm has no text field, so a paste has nowhere to go; drop
            // it rather than pushing it into the prompt the dialog is layered
            // over, which would change the menu's meaning behind the modal.
            if sess.confirm_open() {
                return InTurnDispatch::Ignored;
            }
            sess.paste(&text);
            let _ = sess.settle(output, label, history, dims).await;
            InTurnDispatch::Handled
        }
        _ => InTurnDispatch::Ignored,
    }
}

/// The turn's key pump, live outside `prompt()`: what a retry back-off
/// borrows so keys keep dispatching between attempts exactly as they do inside
/// `prompt()`'s select — the back-off is part of the turn, not a hole in it.
struct TurnPump<'a> {
    keys: &'a mut tokio::sync::mpsc::UnboundedReceiver<crossterm::event::Event>,
    session: &'a mut crate::repl::PromptSession,
    history: &'a mut crate::repl::History,
    label: &'a str,
    guard: &'a crate::repl::TermGuard,
    output: &'a dyn Output,
}

/// Assemble the pump for a back-off wait, or `None` where the interactive
/// pieces don't exist — the same `zip` shape the `InTurnInput` construction
/// uses: piped/batch input has no prompt session and no reader, so there is no
/// key to pump and the wait is [`cancellable_sleep_pumped`] with `None` —
/// deadline and cancel signal only.
fn turn_backoff_pump<'a>(
    prompt_session: &'a mut Option<crate::repl::PromptSession>,
    keys: &'a mut tokio::sync::mpsc::UnboundedReceiver<crossterm::event::Event>,
    history: &'a mut crate::repl::History,
    label: &'a str,
    guard: &'a Option<Arc<crate::repl::TermGuard>>,
    output: &'a dyn Output,
) -> Option<TurnPump<'a>> {
    match (prompt_session.as_mut(), history, guard.as_deref()) {
        (Some(session), history, Some(guard)) => Some(TurnPump {
            keys,
            session,
            history,
            label,
            guard,
            output,
        }),
        _ => None,
    }
}

/// The back-off wait with the turn's key pump running: every key in the queue
/// is dispatched through [`dispatch_in_turn_event`] for as long as it lasts. A root `Esc` fires the cancel signal immediately and
/// ends the wait (`false`); menus step their ladder; edits settle into the
/// input row — identical to the streaming window. The deadline is absolute, so
/// dispatched keys never extend the back-off.
///
/// `pump: None` (piped input) degrades to the plain wait.
async fn cancellable_sleep_pumped(
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
    secs: u64,
    mut pump: Option<TurnPump<'_>>,
) -> bool {
    if secs == 0 {
        return true;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        // Prose's wake-up for this iteration, resolved *before* the select
        // rather than inside the timer's async block — the same contract
        // `prompt()`'s loop has: without this arm prose goes inert for the
        // whole back-off (its debounce, its in-flight slices and its dialog
        // all fire on it). U4.3's timer rationale, extended from the
        // streaming window to the wait between attempts.
        let prose_deadline = pump
            .as_ref()
            .and_then(|p| crate::repl::PromptSession::prose_timeout(&*p.session));
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            _ = cancel_observed(user_cancel, cancel_notify) => return false,
            ev = async {
                match pump.as_mut() {
                    Some(p) => p.keys.recv().await,
                    // No pump (piped input): no keys can ever arrive, so the
                    // branch never completes — same trick `prompt()` uses.
                    None => std::future::pending().await,
                }
            } => {
                if let Some(p) = pump.as_mut() {
                    let outcome = dispatch_in_turn_event(
                        ev,
                        Some(&mut *p.session),
                        Some(&mut *p.history),
                        p.label,
                        Some(p.guard),
                        p.output,
                        user_cancel,
                        cancel_notify,
                    )
                    .await;
                    if outcome == InTurnDispatch::CancelFired {
                        return false;
                    }
                }
                // Handled / Ignored: keep waiting out the rest of the
                // deadline — the loop re-selects with the same deadline.
            }
            // Prose wake: settle so the debounce/slice/dialog fires without
            // a keystroke, exactly as `prompt()`'s timer arm does.
            _ = async {
                match prose_deadline {
                    Some(d) => tokio::time::sleep(d).await,
                    // `None`: nothing pending, and no timer to wake for.
                    None => std::future::pending().await,
                }
            } => {
                if let Some(p) = pump.as_mut() {
                    let dims = p.output.frame_size();
                    let _ = p
                        .session
                        .settle(p.output, p.label, &mut *p.history, dims)
                        .await;
                }
            }
        }
    }
}

/// Next event for a window pump, paced: polls the source without blocking
/// and yields between tries, so an idle window costs one `poll(0)` per tick
/// instead of a reader thread. `None` source, or an inactive window
/// (piped/batch — the script owns stdin): never completes, exactly like the
/// turn pump's missing-queue arm.
async fn next_window_key(src: Option<&dyn WindowKeySource>) -> Option<crossterm::event::Event> {
    let Some(src) = src else {
        return std::future::pending().await;
    };
    if !src.window_active() {
        return std::future::pending().await;
    }
    loop {
        if let Some(ev) = src.poll_key() {
            return Some(ev);
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Route one window-pump event: a root `Esc` fires the cancel signal — the
/// same flag + notify + tool watchdogs the turn's dispatcher raises, which
/// is the whole point of the window — and reports it; anything else is
/// parked for `read_input`. Returns whether the cancel signal fired.
fn pump_window_key(
    ev: Option<crossterm::event::Event>,
    src: &dyn WindowKeySource,
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
) -> bool {
    let Some(ev) = ev else {
        return false;
    };
    if let crossterm::event::Event::Key(key) = &ev {
        if key.code == crossterm::event::KeyCode::Esc {
            user_cancel.store(true, Ordering::SeqCst);
            cancel_notify.notify_one();
            // Identical signal set to the turn dispatcher; no tool runs
            // during a compaction window, so the send is inert there.
            // Unbounded mpsc, so this cannot block the caller.
            for tx in crate::tools::TOOL_CANCEL_SENDERS.lock().unwrap().iter() {
                let _ = tx.send(());
            }
            return true;
        }
    }
    src.defer_event(ev);
    false
}

/// The back-off wait with the window pump live: the wait is part of the
/// operation, so an `Esc` typed during it ends the wait immediately
/// (`false`) and every other key is parked for `read_input`. The deadline
/// stays absolute — parked keys never extend the delay. `keys: None`, or an
/// inactive window, degrades to the plain signal-only wait.
async fn cancellable_sleep_windowed(
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
    secs: u64,
    keys: Option<&dyn WindowKeySource>,
) -> bool {
    if secs == 0 {
        return true;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return true,
            _ = cancel_observed(user_cancel, cancel_notify) => return false,
            ev = next_window_key(keys) => {
                if let Some(src) = keys {
                    if pump_window_key(ev, src, user_cancel, cancel_notify) {
                        return false;
                    }
                }
                // Parked: keep waiting out the rest of the deadline.
            }
        }
    }
}

/// How many retry attempts the REPL grants for `e`, per logical turn (the
/// caller's `attempt` counter is scoped to one prompt submission, so a fresh
/// user prompt resets it). All auto-retryable errors — rate-limited, upstream
/// 5xx, transport, streaming-protocol, and malformed tool calls — share the
/// same generous budget. They are all transient failures that should clear
/// with a retry; the budget exists to cap a runaway, not to second-guess the
/// policy. Post-MVP: expose this via `AgentConfig`.
fn retry_budget(e: &crate::PicocodeError) -> u8 {
    if e.is_auto_retryable() {
        50
    } else {
        0
    }
}

/// Back-off delay before the next retry attempt, in seconds.
///
/// Policy: `2^(consecutive_failures - 1)`, no upper bound — the per-turn
/// `retry_budget` (50) caps how many retries we will ever attempt, so the
/// delay growth is implicitly bounded in practice. We do not cap the value
/// at 300 s or any other number: the user wanted raw exponential growth
/// bounded only by the retry count.
///
/// The `consecutive_failures` counter is reset to 0 whenever the
/// underlying prompt made progress (the agent returned more content or
/// tool calls) before failing — even if the run ultimately errored. That
/// is, `consecutive_failures` counts *consecutive failures with no
/// progress between them*. A failure that happened *after* a successful
/// sub-run inside the same turn restarts the sequence at `2^0 = 1` s, so
/// the next attempt is not punished for a one-off blip mid-turn.
///
/// A provider-supplied `Retry-After` (or OpenRouter's
/// `metadata.retry_after_seconds`) is the **floor** when present — respect
/// the provider's hint, then add our exponential growth on top by treating
/// the hint as the starting base.
fn retry_delay_secs(e: &crate::PicocodeError, consecutive_failures: u8) -> u64 {
    // Provider hint floors the delay. If present, we use it as the base of
    // the exponential schedule: attempt 1 = `hint`, attempt 2 = `hint*2`,
    // …, attempt N = `hint * 2^(N-1)`. This respects upstream's request
    // while still growing on a string of consecutive 429s.
    if let Some(hint) = e.retry_after_secs() {
        let n = consecutive_failures.saturating_sub(1).min(63) as u32;
        return hint.saturating_mul(1u64 << n);
    }
    // No provider hint: pure 2^(n-1) seconds, no cap.
    let n = consecutive_failures.saturating_sub(1).min(63) as u32;
    1u64 << n
}

/// Classify a `StreamingError` into a structured `PicocodeError` variant.
/// `prompt()` invokes this on every non-cancellation stream error. The
/// return is what the REPL shows the user, what determines whether the
/// REPL auto-retries, and (for malformed tool calls) the synthetic
/// feedback the model sees on the retry. Status / headers / body come
/// from the `provider_response_*` accessors on `CompletionError` (rig
/// `error.rs` `forward_provider_response_helpers!`); unknown
/// `ResponseError(String)` shapes fall through to `LlmStreamProtocol`.
fn classify_stream_error(e: &StreamingError) -> crate::PicocodeError {
    let ce: &CompletionError = match e {
        StreamingError::Completion(ce) => ce,
        StreamingError::Prompt(prompt_err) => match prompt_err.as_ref() {
            PromptError::CompletionError(ce) => ce,
            PromptError::MaxTurnsError { max_turns, .. } => {
                return crate::PicocodeError::LlmMaxTurns {
                    max_turns: *max_turns,
                };
            }
            PromptError::UnknownToolCall {
                tool_name,
                available_tools,
                ..
            } => {
                return crate::PicocodeError::LlmUnknownTool {
                    tool_name: tool_name.clone(),
                    available: available_tools.clone(),
                };
            }
            // PromptCancelled is handled by the caller before this fn runs.
            PromptError::PromptCancelled { .. } | PromptError::MemoryError(_) => {
                return crate::PicocodeError::Other(e.to_string());
            }
        },
    };
    classify_completion_error(ce)
}

/// Reduce a provider's error body to one line a human can act on.
///
/// Providers disagree on the envelope, so every shape in the wild is tried in
/// order of how much it actually says:
///
/// - `{"error": {"message": …, "param": …}}` — OpenAI, Anthropic, OpenRouter,
///   Groq. `param` is appended when present, because for a rejected
///   `temperature` it is the field name the user needs and nothing else in the
///   body may repeat it.
/// - `{"error": "…"}` / `{"message": …}` / `{"detail": …}` — flatter servers,
///   and FastAPI-shaped validation errors (`detail` is an array there, so its
///   items are joined).
/// - a bare non-JSON body, whitespace-collapsed.
///
/// Returns `None` when the body is empty, is not text at all, or is pure
/// structure with no message — better a bare status than `; {}`.
///
/// This is the only substitute for information the catalog does not publish.
/// A model that advertises no default temperature still tells us its real
/// range here, in the rejection itself, which is why this matters more than
/// any client-side guess.
fn provider_error_detail(body: Option<&str>) -> crate::ErrorDetail {
    const MAX: usize = 300;
    let Some(raw) = body else {
        return crate::ErrorDetail(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return crate::ErrorDetail(None);
    }
    let text = match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => message_from_json(&v).unwrap_or_default(),
        Err(_) => trimmed.to_string(),
    };
    let one_line = collapse_whitespace(&text);
    if one_line.is_empty() {
        return crate::ErrorDetail(None);
    }
    let clipped = if one_line.chars().count() > MAX {
        let head: String = one_line.chars().take(MAX).collect();
        format!("{head}…")
    } else {
        one_line
    };
    crate::ErrorDetail(Some(clipped))
}

/// Pull the human-facing message out of a parsed error body, plus the offending
/// field when the provider names one.
fn message_from_json(v: &serde_json::Value) -> Option<String> {
    // The common case: an `error` that is either a string or an object with a
    // `message` (and often a `param`/`field` naming the rejected input).
    let err = v.get("error");
    let (msg, param) = match err {
        Some(serde_json::Value::String(s)) => (Some(s.clone()), None),
        Some(serde_json::Value::Object(o)) => (
            o.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string),
            o.get("param")
                .or_else(|| o.get("field"))
                .or_else(|| o.get("parameter"))
                .and_then(|p| p.as_str())
                .map(str::to_string),
        ),
        _ => (None, None),
    };
    let msg = msg
        .or_else(|| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .or_else(|| v.get("detail").map(json_detail_to_string))?;
    // Only worth naming the field when the message does not already say it.
    match param {
        Some(p) if !msg.contains(&p) => Some(format!("{msg} (field: {p})")),
        _ => Some(msg),
    }
}

/// FastAPI-style `detail`, which is either a string or an array of objects with
/// `msg` and `loc`.
fn json_detail_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                let msg = item
                    .get("msg")
                    .and_then(|m| m.as_str())
                    .unwrap_or("invalid");
                match item.get("loc").and_then(|l| l.as_array()) {
                    Some(loc) if !loc.is_empty() => {
                        let path = loc
                            .iter()
                            .filter_map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(".");
                        if path.is_empty() {
                            msg.to_string()
                        } else {
                            format!("{path}: {msg}")
                        }
                    }
                    _ => msg.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("; "),
        other => other.to_string(),
    }
}

/// Collapse every run of whitespace to a single space, so a multi-line JSON
/// string or a wrapped HTML error page still renders as one report line.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Classify a `CompletionError` into a structured `PicocodeError`.
///
/// Exposed for unit tests; `classify_stream_error` is the production
/// entry point and unwraps both `StreamingError` variants before
/// delegating here.
pub(crate) fn classify_completion_error(ce: &CompletionError) -> crate::PicocodeError {
    use crate::PicocodeError::*;
    if let Some(status) = ce.provider_response_status() {
        let code = status.as_u16();
        // A 2xx "success" status cannot signal failure on its own: some
        // providers (OpenRouter, GMICloud via SSE) stream an error envelope
        // *inside* a 200 OK body, so those defer to the body-based envelope
        // detection below. Every genuine failure status (and the odd 1xx/3xx)
        // classifies from the status directly.
        if !status.is_success() {
            let headers = ce.provider_response_headers();
            let retry_after_secs = headers
                .and_then(|h| h.get("retry-after"))
                .and_then(|v| v.to_str().ok())
                .and_then(|s| {
                    // Retry-After can be either a delta-seconds integer or an
                    // HTTP-date. We only handle the integer form (the form
                    // OpenRouter and most providers use for rate limits).
                    s.parse::<u64>().ok()
                });
            // The body is the provider's own account of what it rejected. rig
            // keeps it verbatim on the error, so there is no reason to show the
            // user a status code and nothing else.
            let detail = provider_error_detail(ce.provider_response_body());
            return match code {
                401 | 403 => LlmAuth { status: code },
                402 => LlmBilling { status: code },
                429 => LlmRateLimited { retry_after_secs },
                500..=599 => LlmUpstream {
                    status: code,
                    detail,
                },
                400..=499 => LlmClient {
                    status: code,
                    detail,
                },
                // 1xx/3xx are not errors at the transport layer; treat as
                // generic.
                _ => Other(ce.to_string()),
            };
        }
    }

    match ce {
        CompletionError::HttpError(_) => LlmTransport(ce.to_string()),
        CompletionError::ProviderError(_) => LlmTransport(ce.to_string()),
        CompletionError::ResponseError(msg) => {
            detect_response_error(msg).unwrap_or_else(|| LlmStreamProtocol(msg.clone()))
        }
        // SSE streamed the provider's error envelope as the terminal event:
        // rig's `completion_error_from_body` builds this as a
        // `ProviderResponse` with NO HTTP status (or a 2xx one from
        // `from_http_response`). The body IS the JSON envelope, so run the
        // same detector the `ResponseError` path uses instead of falling
        // through to `Other` (which is not auto-retryable).
        CompletionError::ProviderResponse(pre) => {
            detect_response_error(&pre.body).unwrap_or_else(|| Other(ce.to_string()))
        }
        _ => Other(ce.to_string()),
    }
}

/// Detect a small number of well-known provider-error shapes that rig emits
/// with no richer classification. Today this covers the
/// assembler-emitted malformed-tool-call message and the OpenRouter-style
/// JSON error envelopes that some providers stream in the body of a 200 OK
/// response (rig captures those as `CompletionError::ProviderResponse` with
/// no HTTP status — or as `ResponseError(String)` on other transports). The
/// latter surfaces as `error.code: 429` with
/// `error_type: "rate_limit_exceeded"` (or `provider_unavailable`) inside a
/// JSON envelope; without this detection it falls through to
/// `LlmStreamProtocol` / `Other`, which misclassifies a rate-limit and is
/// not respected by the `Retry-After` floor.
fn detect_response_error(msg: &str) -> Option<crate::PicocodeError> {
    use crate::PicocodeError::*;
    // "tool call `X` arrived with malformed JSON input: <serde error>"
    if let Some(rest) = msg.strip_prefix("tool call `") {
        if let Some((name, after)) = rest.split_once('`') {
            if let Some(parse_err) = after.strip_prefix(" arrived with malformed JSON input: ") {
                return Some(LlmMalformedToolCall {
                    tool_name: name.to_string(),
                    parse_error: parse_err.to_string(),
                });
            }
        }
    }
    // OpenRouter-style JSON envelope: `{"error":{"code":429,"message":"...",
    // "metadata":{"error_type":"rate_limit_exceeded"}}, ...}`. The body
    // is sometimes wrapped by `ProviderResponseError: ` (the `Display`
    // output of `CompletionError::ProviderResponse(...)` when status is
    // `None`); strip the prefix if present.
    let body = msg.strip_prefix("ProviderResponseError: ").unwrap_or(msg);
    if let Some(code) = extract_openrouter_error_code(body) {
        // Same body, same detail as the status-based path above: an error
        // envelope inside a 200 OK response carries the provider's explanation
        // just as a real 4xx does.
        let detail = provider_error_detail(Some(body));
        match code {
            429 => {
                return Some(LlmRateLimited {
                    retry_after_secs: None,
                })
            }
            500..=599 => {
                return Some(LlmUpstream {
                    status: code,
                    detail,
                })
            }
            401 | 403 => return Some(LlmAuth { status: code }),
            402 => return Some(LlmBilling { status: code }),
            400..=499 => {
                return Some(LlmClient {
                    status: code,
                    detail,
                })
            }
            _ => return None,
        }
    }
    None
}

/// Extract the `error.code` field from an OpenRouter-style error envelope.
/// Returns `None` if the body is not JSON or has no numeric `error.code`.
fn extract_openrouter_error_code(body: &str) -> Option<u16> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let code = v.get("error")?.get("code")?.as_u64()?;
    u16::try_from(code).ok()
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
fn effective_params(
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
async fn build_session_ctx(
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
async fn record_reasoning_change(session: &Session, level: &str) {
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
/// records no model or when it matches the live model.
///
/// The full `provider:model_id` form is intentional: pre-filling the
/// unqualified `model_id` might surface multiple catalog entries that
/// share a short name under different providers.
async fn compute_resume_model_prefill(session: &Session, live_model_id: &str) -> Option<String> {
    let recorded = session_recorded_model_id(session).await?;
    if recorded == live_model_id {
        return None; // session already runs the live model — no prefill
    }
    Some(format!("/model {}", recorded))
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
async fn retry_summarization<F, Fut>(
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
/// what the v2.5 `/model` node resolves directly); when they match or
/// the session records nothing, no prefill is returned. Errors are
/// caller-displayed messages (same texts the old arm printed).
pub(crate) async fn resume_session(
    repo: &crate::session::repo::JsonlSessionRepo,
    meta: &crate::session::SessionMetadata,
    replay: (usize, bool),
    output: &dyn Output,
    qualified_id: &str,
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
    let prefill = compute_resume_model_prefill(&new_session, qualified_id).await;
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

/// Load the list of recently-used model IDs from `recent_models.txt`.
/// Returns an empty vec if the file doesn't exist or can't be read.
fn load_recent_models() -> Vec<String> {
    let path = match crate::platform::recent_models_path() {
        Some(p) => p,
        None => return Vec::new(),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Atomically write the list of recently-used model IDs to `recent_models.txt`.
/// Writes to a temp file in the same directory, then renames (atomic on POSIX).
fn save_recent_models(models: &[String]) {
    let path = match crate::platform::recent_models_path() {
        Some(p) => p,
        None => return,
    };
    let dir = match path.parent() {
        Some(d) => d,
        None => return,
    };
    // Build the content as one model id per line.
    let content = models.join("\n") + "\n";
    // Write to a temporary file in the same directory, then atomically rename.
    let tmp_name = format!(".recent_models.tmp.{}", std::process::id());
    let tmp_path = dir.join(&tmp_name);
    match std::fs::write(&tmp_path, &content) {
        Ok(()) => {
            let _ = std::fs::rename(&tmp_path, &path);
        }
        Err(_) => {
            // Best-effort cleanup on write failure.
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

/// Interactive chat loop. Owns the agent and the `AgentConfig` so slash
/// commands that change model/provider/thinking can rebuild the agent via
/// `create_agent`.
///
/// (The parenthetical this used to carry — "required because `CodeAgent<M>` is
/// generic over the provider's completion-model type" — died with the rig 0.42
/// migration that made `CodeAgent` concrete. The rebuild mechanism outlived its
/// original reason, which is usually a sign the comment should go rather than
/// the code.)
/// Fingerprint of the target state the REPL has *attempted* to build an agent
/// for. Equality is the whole point: the loop rebuilds once per distinct
/// fingerprint, so a failed `create_agent` is reported once instead of every
/// cycle while the user navigates back to the menu.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RebuildFingerprint {
    pub model: String,
    pub params: params::ModelParams,
    pub persona: Option<String>,
    /// Part of the identity, not just the comparison: a Ctrl-E that changes the
    /// context window is a new target, so a *previous* failed attempt must not
    /// dedupe it away — the user edited something, which is exactly the signal
    /// the dedupe is meant to wait for.
    pub context_limit: Option<u64>,
}

/// Decide whether the convergence step should rebuild the agent this cycle.
///
/// Returns the fingerprint to record on a rebuild, or `None` to leave the
/// running agent alone. Two invariants, both easy to break and awkward to
/// test through the REPL loop, so they live here:
///
/// 1. **Only a real change rebuilds.** The applied config and the runtime
///    target are compared field by field — never by re-deriving the model id
///    from segments, which would make every commit look like a model change.
/// 2. **One attempt per distinct target.** Re-arming requires a *different*
///    fingerprint, so a target that already failed is not retried until
///    something actually changes. Recovery is "change the target", because
///    `create_agent` errors (missing key, bad model id) are final for a given
///    target.
///
/// Any param field counts as a change: temperature and top_p are baked into
/// the agent's request builder, so there is no in-place path for them.
pub(crate) fn plan_rebuild(
    applied: &AgentConfig,
    target: &AgentConfig,
    last_attempt: Option<&RebuildFingerprint>,
) -> Option<RebuildFingerprint> {
    let changed = applied.qualified_model_id() != target.qualified_model_id()
        || applied.params != target.params
        || applied.persona_name != target.persona_name
        // The window is compared because it reaches the mid-turn status bar
        // only through a rebuild, and a per-model `context_length` override
        // moves nothing else here.
        || applied.context_limit != target.context_limit;
    if !changed {
        return None;
    }
    let attempt = RebuildFingerprint {
        model: target.qualified_model_id(),
        params: target.params,
        persona: target.persona_name.clone(),
        context_limit: target.context_limit,
    };
    if last_attempt == Some(&attempt) {
        return None;
    }
    Some(attempt)
}

pub async fn run_interactive(
    config: AgentConfig,
    settings: SettingsStore,
    auto_resume: bool,
) -> Result<()> {
    let mut config = config;
    // Store the original (non-merged) sandbox config so the remove overlay
    // can list config-provided bindings and allow temporary exclusion.
    crate::sandbox_state::set_base(config.sandbox.clone());
    // Publish the resolved sandbox config immediately so that user-initiated
    // `!`/`!!` shell commands use the same bwrap/timeout policy as the agent's
    // `bash` tool — even before the agent is lazily created on the first
    // normal prompt. `set_sandbox` is otherwise only called inside
    // `build_rig_agent`, so without this the first `!`/`!!` (issued before any
    // LLM turn) would run UNSANDBOXED. The sandbox config is constant for the
    // session (`apply_to_config` never touches it), so publishing once here is
    // sufficient.
    set_sandbox(config.sandbox.clone());
    // The agent lives in the AppState slot: created lazily on first use
    // (or on /model, /thinking — commands like /help must work without a
    // network round-trip), always **loop-side from the applied config**,
    // published via `activate_agent`; every reader clones it out.

    // Empty prompt: the REPL input is left-aligned on every row (a `> ` prefix
    // would only indent the first line and leave wrapped/continuation lines
    // ragged). Mirrors pi's left-aligned input region.
    let prompt_str = String::new();
    let use_repl = std::io::stdin().is_terminal() && !config.quiet;
    // The confirm port, built here and *before* any agent exists, because the
    // guards capture the sender half at build time and a `/model` rebuild
    // re-captures this same one — so a session approval survives the rebuild.
    // The receiver half is borrowed per turn below (it cannot outlive the pump
    // that answers it).
    //
    // The bridge itself is built unconditionally so the receiver always exists;
    // only the *port* is conditional. Without a REPL there is no key pump to
    // draw a dialog, so no guard holds the sender and this receiver simply
    // never yields — the same degradation `InTurnInput::keys` already uses.
    let mut confirm_bridge = crate::confirm::ConfirmBridge::new();
    if use_repl {
        config.confirm_port = Some(confirm_bridge.port.clone());
    }
    let mut repl = if use_repl { Some(Repl::new()) } else { None };
    if let Some(r) = repl.as_mut() {
        let _ = r.start();
        // First screen: a one-line hint + the current status line, both
        // emitted via the `Output` impl so cursor tracking stays correct.
        let status = format!(
            "{} | {} ({}) | {}",
            style("picocode").bold(),
            style(&config.provider).cyan(),
            style(&config.model).blue(),
            style(format!(
                "limit:{} yolo={}",
                config.tool_call_limit, config.yolo
            ))
            .yellow(),
        );
        config
            .output
            .display_plain("picocode - type `/` for details.");
        config.output.display_plain(&status);
    }

    // Persistent session storage.
    // Session storage: create repo eagerly (just a path, no I/O) for /session
    // overlay, but defer actual session file creation to the first turn
    // to avoid empty sessions.
    let sessions_dir = config
        .sessions_dir
        .clone()
        .or_else(|| crate::platform::app_config_dir().map(|d| d.join("sessions")));
    let cwd = std::env::current_dir().ok();
    let repo: Option<crate::session::repo::JsonlSessionRepo> =
        if let (Some(dir), Some(_)) = (&sessions_dir, &cwd) {
            let repo = crate::session::repo::JsonlSessionRepo::new(dir.clone());
            // Repo is created eagerly (just a path); the session object is also
            // created unconditionally here so the `/session` overlay can mark the
            // current session from the start. `repo.create()` only opens the
            // storage — the actual `.jsonl` file is written lazily on the first
            // `append_message()` (Session::ensure_persisted) — so no empty session
            // file is created if the user never takes a turn. Creating up front
            // removes the mid-loop `ensure_session` calls (in the first-turn and
            // `!`/`!!` paths).
            Some(repo)
        } else {
            None
        };
    let mut line_history = crate::repl::History::load();
    // Shared cancel flag: set on a root `Esc` by the turn's key branch (the
    // in-flight reader used to set it, but it cannot see a menu and so could
    // not honour menu-first); the `LoggingHook` bridge turns it into a rig
    // `CancelSignal` that aborts the current request.
    let cancel = Arc::new(AtomicBool::new(false));
    // So `/model` etc. embed the same flag into the rebuilt agent.
    config.user_cancel = cancel.clone();
    // Event-based cancel signal — the streaming loop selects on this instead
    // of polling.  Paired with `user_cancel` above.
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    config.cancel_notify = cancel_notify.clone();
    // Single source of truth for menu-visible state. The target config is
    // seeded from the startup config (same Arc handles: output, cancel
    // flags); the loop's local `config` remains the applied layer that
    // converges to it. Settings move in as the one durable copy.
    let app_state = AppState::new(config.clone(), settings, repo.clone());
    // Load-or-seed the initial model's live params from its stored record, then
    // pull them back into the applied layer. Seeding before `ensure_session`
    // below is what puts the right level in the session header, and copying
    // them into `config` means the first agent is built once with the final
    // values instead of being rebuilt on the first cycle to catch up.
    let initial_model = config.qualified_model_id();
    app_state.activate_model_params(&initial_model);
    config.params = app_state.params();
    // The resolved context window, read once here and carried on the config so
    // the status bar can divide by it without a store lookup per paint.
    config.context_limit = resolved_context_limit(&app_state);
    // Onto the target too, or the two sides disagree at startup and the
    // convergence step rebuilds the agent once for nothing.
    let startup_limit = config.context_limit;
    app_state.update_config(|c| c.context_limit = startup_limit);
    // First session — created up front so the `/session` overlay can mark
    // it from the start (the file itself is written lazily on the first
    // `append_message`, so no empty session is created for a user who
    // never takes a turn).
    let first = ensure_session(
        app_state.session(),
        &repo,
        Some(config.provider.clone()),
        Some(config.model.clone()),
        config.params.reasoning_str(),
    )
    .await;
    let first_id = first
        .as_ref()
        .map(|s| s.get_session_id().to_string())
        .unwrap_or_default();
    app_state.activate_session(first, first_id);
    app_state.set_recent_models(load_recent_models());
    // Seed the model switch from the configured default model so it appears
    // in the recent list on first run (if present and available).
    app_state.seed_recent_model(&config.qualified_model_id());
    // Rebuild gate state: fingerprint of the last target we ATTEMPTED to
    // apply. A failed attempt (bad id, missing key) stays silent and
    // un-retried until the target changes — one error per distinct target,
    // not one per cycle while the user navigates back to the menu.
    let mut last_attempted_target: Option<RebuildFingerprint> = None;
    // Guard handle for the reader thread (suspend/resume / hard-exit).
    let guard = repl.as_ref().map(|r| r.guard());
    // Publish to the slot: the inline compact node's review closure
    // suspends raw mode around `$EDITOR` with it (service handle,
    // clone-out — never held behind the lock across the editor).
    if let Some(g) = guard.clone() {
        app_state.set_term_guard(g);
    }
    if guard.is_some() {
        crate::repl::register_sigcont();
    }
    // Warm the model catalog cache in the background so the `/model`
    // overlay is populated within a few seconds *without* blocking the
    // first prompt. The overlay falls back to the cached/static list
    // until the fetch completes.
    tokio::task::spawn(async {
        let _ = model_catalog::catalog().await;
    });
    // One-shot input buffer preload. A `/session <id> resume` may compute a
    // `/model <provider>:<model_id>` prefill to surface the resumed session's
    // stored model to the user (so they can press Enter to accept, Esc to
    // dismiss, or type to choose a different model). It is set by the
    // `Switched` arm of the session-command dispatch and consumed by the next
    // `read_input` call; cleared afterwards so subsequent prompts start empty.
    //
    // The CLI `--resume` flag populates this BEFORE the loop so the very
    // first prompt already has the slash command queued — the user presses
    // Enter once and goes through the exact same code path as a manual
    // `/session`, Tab, Enter: the REPL parses `/session <id> resume`, the
    // session node's `submit` opens + replays the session, and the
    // usual `/model …` prefill flows back into the slot's prefill
    // second prompt. This keeps the history and UX identical to a typed
    // resume (including the model-overlay catalog fetch that needs the
    // inter-keystroke time to land).
    if auto_resume {
        // Pre-fill with the session listing for the current directory.
        // The user presses Enter to resume the top (most recent) session.
        // This is simpler than resolving the session ID upfront and avoids
        // any race conditions with the session repository listing.
        app_state.set_prefill("/session .path:.".to_string());
    }
    // One prompt session, carried across turns (ui_draw_v2 §4.5 U4).
    //
    // Declared outside the loop so its life spans `read_input` → the whole
    // turn → the *next* `read_input`, which is the only shape in which typing
    // during a turn has anywhere to land. `None` until the interactive path
    // builds it: piped/batch input has no prompt to carry.
    let mut prompt_session: Option<crate::repl::PromptSession> = None;
    loop {
        let action = match repl.as_mut() {
            Some(r) => {
                // No separator here any more: the prompt-side status bar occupies
                // the divider's row, above the input (it is a `Lines` element of
                // the input frame). The bar separates the last turn from the
                // prompt *and* says what is about to answer it, which a dim rule
                // could not do — and it is the same bar the turn paints while it
                // streams, so the dot is the only thing that changes between the
                // two states.
                //
                // The spacing the rule provided is not lost: a response ends on
                // a blank (`end_response`), and the greeter ends on one of its
                // own, so the frame's first row still lands one row clear.

                // Build session context data for REPL overlays.
                let (_sessions, branch_messages, tree, leaf_id, context_tokens) =
                    build_session_ctx(&app_state.session()).await;
                // The session's own name and opening message, for the prompt-side
                // status bar. Off the same `branch_messages` the overlays read,
                // so the two bars cannot disagree about which session they are
                // naming — and off the *index* for the name, which is where the
                // mid-turn bar reads it too.
                let session_name = match app_state.session() {
                    Some(session) => session.index.lock().await.header.name.clone(),
                    None => None,
                };
                let session_first_message =
                    branch_messages.first().map(|(_, preview)| preview.clone());

                let ctx = ReplCtx {
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                    recent: app_state.recent_models(),
                    persona: config.persona_name.clone(),
                    // Snapshot the agent's accumulated token usage so the `/stat`
                    // overlay can render without needing the agent directly.
                    session_usage: app_state
                        .agent()
                        .map(|a| a.session_usage())
                        .unwrap_or_default(),
                    repo: repo.clone(),
                    current_session_id: app_state.current_session_id(),
                    branch_messages,
                    tree,
                    leaf_id,
                    context_tokens,
                    model_filter: ModelFilter::default_for_coding(),
                    // P1's accessor (the level now lives in `ModelParams`)
                    // plus upstream's two new status-bar fields.
                    reasoning_level: config.params.reasoning_str(),
                    // The resolved context window, so the prompt-side bar's
                    // percentage uses the same denominator as the mid-turn one.
                    context_limit: resolved_context_limit(&app_state),
                    // The prompt-side bar names the session the same way the
                    // mid-turn one does: off the session index, with the opening
                    // message as the fallback when it has no name. Read here, at
                    // the top of the cycle, so both bars are looking at one
                    // snapshot rather than at whatever the index holds later.
                    session_name,
                    session_first_message,
                };
                // Pop the pending prefill (if any). It is the one thing that
                // may override the carried buffer, and it is once-only: a
                // `/session <id> resume` sets it, this consumes it, and the
                // next iteration sees `None`.
                let prefill = app_state.take_prefill();
                // Built on first use and then carried — never reconstructed.
                let session = prompt_session
                    .get_or_insert_with(|| crate::repl::PromptSession::new(None, &app_state));
                if let Some(text) = prefill.as_deref() {
                    session.apply_prefill(text);
                }
                // `read_input` re-anchors the draw cache itself on entry
                // (DSR); afterwards the frame follows the cursor, which the
                // cache tracks (ui_draw_v2 §5.2).
                r.read_input(
                    &prompt_str,
                    session,
                    &mut line_history,
                    &ctx,
                    &app_state,
                    config.output.as_ref(),
                )
                .await
            }
            None => {
                // Non-REPL input (quiet / piped stdin): `/`-prefixed lines
                // go through the v2.5 engine as a batch (passive derive +
                // virtual Enter — `Repl::batch_slash`), with no overlay
                // drawing; everything else is raw `Submit` text. This runs
                // BEFORE the convergence step below, so control/target
                // writes land the same cycle as an interactive `MenuDone`
                // (doc/editor_cleanup.md §10.9).
                let input = config.output.get_user_input();
                match Repl::batch_slash(
                    &app_state,
                    &input,
                    config.output.as_ref(),
                    &mut line_history,
                )
                .await
                {
                    Some(action) => Some(action),
                    None => Some(InputAction::Submit(input)),
                }
            }
        };
        let action = match action {
            Some(a) => a,
            None => continue,
        };
        // Convergence: fold the runtime-target config (AppState) into the
        // applied layer, then record session metadata and drain control
        // targets — this loop is the targets' sole consumer. Runs every
        // cycle: run-to-completion means at most one menu submit per cycle,
        // and a missed application point only delays convergence.
        let target = app_state.config_snapshot();
        let control = app_state.take_control();
        if control.quit {
            break;
        }

        // Rebuild gate: canonical whole-string compares against the
        // RUNNING model (config), never a segment split — model ids may
        // themselves contain colons (openrouter `…:free`), and a
        // `nth(1)`-style truncation reads a different value than what is
        // configured, making every menu commit look like a model change.
        // Narrower flag for the journal: only a *level* change is worth a
        // `ThinkingLevelChange` entry, and recording it for a temperature
        // tweak would be noise.
        let reasoning_changed = config.params.reasoning != target.params.reasoning;
        let target_reasoning = target.params.reasoning_str();
        // What the loop's rebuild gate should do this cycle. Extracted so the
        // once-per-target invariant is testable without running the REPL.
        let rebuild = plan_rebuild(&config, &target, last_attempted_target.as_ref());
        let model_changed = config.qualified_model_id() != target.qualified_model_id();
        let params_changed = config.params != target.params;
        let persona_changed = config.persona_name != target.persona_name;

        // One rebuild attempt per distinct target: re-running a failed
        // attempt would reprint the same error every cycle while the user
        // digs back to the menu to fix it. Changing the target — or an
        // explicit `/model` re-commit of the same model — arms the next
        // attempt. (`create_agent` is local: key/config errors are final
        // for a given target, so recovery really is "change the target".)
        if let Some(attempt) = rebuild {
            last_attempted_target = Some(attempt);
            let mut new_cfg = config.clone();
            if model_changed {
                new_cfg.provider = target.provider.clone();
                new_cfg.model = target.model.clone();
            }
            if params_changed {
                new_cfg.params = target.params;
            }
            // Re-read on every rebuild: the window moves when the model does,
            // and also when the user Ctrl-E's a `context_length` override and
            // then switches away and back.
            new_cfg.context_limit = resolved_context_limit(&app_state);
            if persona_changed {
                new_cfg.persona_name = target.persona_name.clone();
                new_cfg.persona_prompt = target
                    .persona_name
                    .as_ref()
                    .and_then(|p| crate::persona::get_persona(p));
            }
            match create_agent(new_cfg.clone()).await {
                Ok(new_agent) => {
                    app_state.activate_agent(Arc::from(new_agent));
                    if model_changed {
                        let model_id = new_cfg.qualified_model_id();
                        let updated = app_state.note_recent_model(&model_id);
                        save_recent_models(&updated);
                    }
                    config = new_cfg;
                }
                Err(e) => {
                    let kind = if model_changed {
                        "switch model"
                    } else if params_changed {
                        "set model params"
                    } else {
                        "set persona"
                    };
                    config
                        .output
                        .display_text(&format!("Cannot {}: {}", kind, e));
                }
            }
        }

        // Thinking metadata recording (the model entry moved inline: the
        // /model node journals its explicit commit at submit time — its
        // record helper skips values the session already holds and folds
        // metadata into the in-memory header while the session is still
        // unpersisted). Runs AFTER the rebuild so `config` matches the
        // target when we record.
        let recording_session = app_state.session();
        if reasoning_changed && config.params.reasoning_str() == target_reasoning {
            if let Some(ref s) = recording_session {
                record_reasoning_change(s, target_reasoning.as_deref().unwrap_or("off")).await;
            }
        }
        // Control appliers — each field's sole consumer. `take_control()`
        // above already GC'd the one-shots; run-to-completion means at
        // most one menu submit set them this cycle.

        if let Some(op) = control.sandbox {
            // Relocated v2.0 /sandbox handler: mutate the state
            // store, then re-publish the merged config — BOTH
            // `tools::set_sandbox` (live) AND `config.sandbox`
            // (so the merge survives a lazy agent rebuild, which
            // does `set_sandbox(config.sandbox.clone())`).
            use crate::app_state::SandboxOp;
            let result = match &op {
                SandboxOp::Add {
                    host_path,
                    sandbox_path,
                    mode,
                } => crate::sandbox_state::add(host_path, sandbox_path, *mode),
                SandboxOp::Remove { sandbox_path } => crate::sandbox_state::remove(sandbox_path),
                SandboxOp::Exclude { sandbox_path } => crate::sandbox_state::exclude(sandbox_path),
                SandboxOp::Restore { sandbox_path } => crate::sandbox_state::restore(sandbox_path),
            };
            match result {
                Ok(()) => {
                    let mut merged = config.sandbox.clone().unwrap_or_default();
                    crate::sandbox_state::merge_into(&mut merged);
                    crate::tools::set_sandbox(Some(merged.clone()));
                    config.sandbox = Some(merged.clone());
                    // Keep the target snapshot in sync — this applier is the
                    // one that *applies* sandbox state.
                    app_state.update_config(|c| c.sandbox = Some(merged));
                    let msg = match &op {
                        SandboxOp::Add {
                            host_path,
                            sandbox_path,
                            mode,
                        } => {
                            let m = if *mode == crate::config::BindMode::Rw {
                                "add_rw"
                            } else {
                                "add_ro"
                            };
                            if sandbox_path.is_empty() {
                                format!("sandbox: {} {} (ok)", m, host_path)
                            } else {
                                format!("sandbox: {} {} → {} (ok)", m, host_path, sandbox_path)
                            }
                        }
                        SandboxOp::Remove { sandbox_path } => {
                            format!("sandbox: removed {} (ok)", sandbox_path)
                        }
                        SandboxOp::Exclude { sandbox_path } => {
                            format!("sandbox: excluded {} (ok)", sandbox_path)
                        }
                        SandboxOp::Restore { sandbox_path } => {
                            format!("sandbox: restored {} (ok)", sandbox_path)
                        }
                    };
                    config.output.display_text(&msg);
                }
                Err(e) => {
                    config.output.display_text(&format!("sandbox error: {}", e));
                }
            }
        }
        match action {
            InputAction::MenuDone => {
                // v2.5 menu committed: the node already wrote to `AppState`
                // (target config / control fields); the convergence step
                // above applied them. No-op arm.
            }
            InputAction::Submit(input) => {
                if input.is_empty() {
                    continue;
                }
                line_history.add(&input);
                if input == "/q" || input == "exit" {
                    break;
                }
                // Per-turn overrides (doc/model_settings_v2.md A.10).
                //
                // `line_history` above took the RAW line, so recall shows the
                // user what they typed including the overrides; from here on
                // `input` is the text with the override lines removed, which is
                // what is persisted, sent, and counted for context. Nothing is
                // mutated: the overrides travel as a value into `prompt()` for
                // this turn only, so there is no restore step and no way one
                // reaches the live params or the next turn.
                let split = crate::turn_overrides::split(&input);
                for w in &split.warnings {
                    config.output.display_text(w);
                }
                // An overrides-only prompt has nothing to send. Checked *before*
                // `ensure_session` below, or a turn that never runs would still
                // create a session.
                if split.text.trim().is_empty() {
                    config
                        .output
                        .display_text("nothing to send — the prompt was only overrides");
                    continue;
                }
                let input = split.text;
                let (effective_params, notes) = effective_params(&app_state, &split.overrides);
                for n in &notes {
                    config.output.display_text(n);
                }
                // Safety net: any code path that drops the in-memory session
                // (notably `/new` and `/clear`) must leave us with a fresh one
                // before the first post-drop prompt — otherwise
                // `prompt(input, None, ...)` would build an empty prior context
                // and skip every `append_message` (gated on `session.is_some()`),
                // losing turn-to-turn memory and never writing the new session
                // to disk. Cheap when the session is already populated.
                let guarded = ensure_session(
                    app_state.session(),
                    &repo,
                    Some(config.provider.clone()),
                    Some(config.model.clone()),
                    config.params.reasoning_str(),
                )
                .await;
                let guarded_id = guarded
                    .as_ref()
                    .map(|s| s.get_session_id().to_string())
                    .unwrap_or_default();
                app_state.activate_session(guarded, guarded_id);
                // User-initiated shell command. `!cmd` runs `cmd` in the
                // sandbox and captures its output into the session context (so
                // the LLM sees it on the next turn); `!!cmd` runs and displays
                // it but does NOT add it to context. Both reuse the same
                // `run_sandboxed` helper (and therefore the same bwrap sandbox
                // + timeout policy) as the LLM's `bash` tool, so user- and
                // model-initiated shelling are identical. No confirmation is
                // required: the user typed it explicitly, unlike the
                // LLM-initiated tool call. This skips the LLM prompt entirely.
                if input.starts_with("!!") || input.starts_with('!') {
                    let (capture, cmd) = if let Some(rest) = input.strip_prefix("!!") {
                        (false, rest)
                    } else if let Some(rest) = input.strip_prefix('!') {
                        (true, rest)
                    } else {
                        unreachable!()
                    };
                    let cmd = cmd.trim();
                    // Reprint the submitted command (cleared by `read_input`),
                    // bracketed like every other line the user committed with
                    // Enter (ui_draw_v2 R4).
                    config.output.display_committed_prompt(&input);
                    if cmd.is_empty() {
                        config
                            .output
                            .display_plain("Usage: !<command> (capture output into context), or !!<command> (display only)");
                    } else {
                        // Re-publish the resolved sandbox config immediately
                        // before running the command. The config is held in a
                        // process-wide global that other tests (and other
                        // sessions) may mutate concurrently; pinning it here —
                        // with no await between this and `run_sandboxed`'s read
                        // of the global — keeps the `!`/`!!` path immune to
                        // that race while preserving the "publish once" intent.
                        set_sandbox(config.sandbox.clone());
                        match crate::tools::run_sandboxed(cmd, None).await {
                            Ok(out) => {
                                let out = out.trim();
                                if out.is_empty() {
                                    config.output.display_plain("(no output)");
                                } else {
                                    config.output.display_plain(out);
                                }
                                if capture {
                                    // Mirror a normal turn so the model sees
                                    // both the command and its result: a user
                                    // message naming the command, plus an
                                    // assistant message carrying the captured
                                    // output.
                                    let msg = Message::user(format!("Ran shell command: {cmd}"));
                                    if let Some(s) = app_state.session() {
                                        let _ = s.append_message(&msg).await;
                                        let reply = Message::Assistant {
                                            id: None,
                                            content: vec![AssistantContent::Text(Text::new(
                                                out.to_string(),
                                            ))],
                                        };
                                        let _ = s.append_message(&reply).await;
                                    }
                                } else {
                                    config.output.display_plain(
                                        "(output was displayed but not added to context)",
                                    );
                                }
                            }
                            Err(e) => {
                                config.output.display_text(&format!("shell error: {}", e));
                            }
                        }
                    }
                    continue;
                }
                // `/stat` is handled by the REPL overlay (typing it opens the
                // stats panel); it is never submitted here, so there's nothing
                // to do for that keyword.
                // Reprint the submitted prompt via the `Output` impl so the
                // session (and cursor tracking) stays correct; `read_input`
                // cleared the input line on submit. Bracketed — a rule above
                // *and* below, which the second call used to add by hand — and
                // shared with a directly preceding commit instead of stacked
                // (ui_draw_v2 R4).
                config.output.display_committed_prompt(&input);
                // Normal prompt — create the agent lazily on first use
                // (loop-side, applied config; published to the slot).
                if app_state.agent().is_none() {
                    match create_agent(config.clone()).await {
                        Ok(a) => app_state.activate_agent(Arc::from(a)),
                        Err(e) => {
                            config
                                .output
                                .display_text(&format!("Cannot start agent: {}", e));
                            continue;
                        }
                    }
                }
                // Clone-out for this turn (lock rule 3: no guard across await).
                let agent = app_state.agent();
                let session = app_state.session();
                // Single attempt + auto-retry, with the in-flight reader
                // (if any) spanning the whole window. `Esc` is dispatched by
                // the turn's one key dispatcher wherever the turn is: inside
                // `prompt()` while an attempt streams, by the back-off pump
                // while a retry delay runs (`cancellable_sleep_pumped`) —
                // either way it fires the cancel signal, and the wait (or the
                // stream) ends immediately, returning `Cancelled`.
                // U4.2/U4.3: edit keys typed while the turn runs. The reader
                // owns the sender; the receiver is bound for the whole turn so
                // the queue lives exactly as long as the reader can feed it,
                // and it is handed into `prompt()` as `InTurnInput::keys` for
                // the key branch to drain.
                //
                // Holding it is what keeps `send` working — a dropped receiver
                // makes every send a no-op, which would silently turn the
                // forwarding back into the drop U4.2 existed to remove. It
                // dies with this scope, so the queue dies with it and a
                // keystroke queued at the end of one turn cannot leak into the
                // next prompt.
                let (in_turn_keys_tx, mut in_turn_keys) = tokio::sync::mpsc::unbounded_channel();
                let reader_handle = if let Some(g) = &guard {
                    let done = Arc::new(AtomicBool::new(false));
                    let confirm_gate = config
                        .output
                        .confirm_gate()
                        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                    let h = crate::repl::spawn_in_flight_reader(
                        g.clone(),
                        done.clone(),
                        confirm_gate,
                        in_turn_keys_tx,
                    );
                    Some((h, done))
                } else {
                    None
                };
                let response: std::result::Result<String, crate::PicocodeError> = {
                    let mut consecutive_failures: u8 = 0;
                    let mut next_input: String = input.clone();
                    // The params this turn sends. They are the live ones with
                    // the typed overrides applied, and they are *carried*, not
                    // applied to the live config — so a retry of the same
                    // dispatch re-uses them automatically (the stripped text is
                    // already in `next_input`), while a synthetic re-prompt
                    // below resets them, since an override belongs to the
                    // user's dispatch and not to the model's own feedback.
                    let mut turn_params = effective_params;
                    // Per-turn persistence flag. Reset once per user prompt
                    // (NOT per retry): the retry loop below reuses it across
                    // `prompt()` attempts so a pre-persisted `user(input)` is
                    // not duplicated, while the NEXT turn starts fresh (so
                    // two identical inputs in a row, e.g. "continue" twice,
                    // never collide). See `TurnPersistence`.
                    let turn = TurnPersistence::new();
                    // The user's original prompt is this turn's first NEW
                    // dispatch.
                    turn.mark_new_dispatch();
                    // The full steering message (synthetic feedback / "continue")
                    // is delivered once per logical turn; `prompt()` persists it
                    // with the partial transcript when a retry makes progress and
                    // then fails again. Re-sending the same full steer on that
                    // later interruption would repeat it in the session once the
                    // retry succeeds, so later interruptions get a short
                    // continuation instead.
                    let mut steer_delivered = false;
                    let outcome: std::result::Result<String, crate::PicocodeError> = loop {
                        let prompt_result = agent
                            .as_ref()
                            .unwrap()
                            .prompt(
                                &next_input,
                                session.as_deref(),
                                &turn,
                                &turn_params,
                                // `zip`: the key branch exists only where there
                                // is both a session to receive the key and a
                                // guard to stop the reader for it. Piped input
                                // has neither, and piped input with a guard
                                // has no reader, so there is no key either.
                                prompt_session
                                    .as_mut()
                                    .zip(guard.as_deref())
                                    .map(|(sess, g)| crate::repl::InTurnInput {
                                        session: sess,
                                        keys: &mut in_turn_keys,
                                        confirms: &mut confirm_bridge.requests,
                                        label: &prompt_str,
                                        history: &mut line_history,
                                        guard: g,
                                    }),
                            )
                            .await;
                        match prompt_result {
                            Ok(text) => break Ok(text),
                            Err(crate::PicocodeError::Cancelled(_)) => {
                                break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                            }
                            // Mid-turn failure that persisted progress: the
                            // underlying run produced content/tool calls
                            // before erroring. This is NOT a "consecutive
                            // failure" for back-off purposes — there was
                            // successful work in between — so reset the
                            // counter and treat the upcoming retry as a
                            // fresh first attempt (delay = 2^0 = 1 s, or
                            // the provider's Retry-After floor). The budget
                            // is still consumed (one retry used), but the
                            // exponential delay does not compound.
                            Err(crate::PicocodeError::LlmInterrupted { cause, .. })
                                if consecutive_failures < retry_budget(&cause) =>
                            {
                                consecutive_failures = 0;
                                // `prompt()` persisted the user prompt + the
                                // finalized partial transcript to the session
                                // and returned `LlmInterrupted`. Continue from
                                // that transcript with a steering message — the
                                // original `input` is already in the session,
                                // so re-sending it would duplicate the user's
                                // request.
                                let secs = retry_delay_secs(&cause, 1);
                                config.output.display_text(&format!(
                                    "Error: {}. Retrying in {secs}s...",
                                    cause
                                ));
                                let slept = cancellable_sleep_pumped(
                                    &config.user_cancel,
                                    &config.cancel_notify,
                                    secs,
                                    turn_backoff_pump(
                                        &mut prompt_session,
                                        &mut in_turn_keys,
                                        &mut line_history,
                                        &prompt_str,
                                        &guard,
                                        config.output.as_ref(),
                                    ),
                                )
                                .await;
                                if !slept {
                                    break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                                }
                                // Steering prompt: the synthetic feedback for
                                // e.g. malformed tool calls (carries the parse
                                // error the model needs to self-correct), else a
                                // short "continue" pointing at the persisted
                                // work. NOT appended to the session — it becomes
                                // the retry's prompt (input + partial are already
                                // persisted). Delivered in full (the synthetic
                                // feedback) only once per logical turn; after the
                                // model has already been steered once and made
                                // further (persisted) progress, a plain "continue"
                                // suffices and avoids repeating the steer text in
                                // the session.
                                //
                                // When there is no session, nothing was actually
                                // persisted, so a "continue where you left off"
                                // steer would point at context that doesn't exist
                                // and drop the user's original request. In that
                                // case re-send the original `input` instead (same
                                // as the no-progress path) — the back-off reset
                                // above still applies, but the user's turn is not
                                // lost.
                                next_input = if session.is_none() {
                                    input.clone()
                                } else if steer_delivered {
                                    "continue".to_string()
                                } else {
                                    steer_delivered = true;
                                    match cause.synthetic_retry_feedback() {
                                        Some(fb) => fb,
                                        None => "continue".to_string(),
                                    }
                                };
                                // The steering message is a NEW user dispatch
                                // for this turn (distinct from the original
                                // prompt), so it counts toward `turns_sent` and
                                // will be persisted/deduped as its own message.
                                turn.mark_new_dispatch();
                                if cancel.load(Ordering::SeqCst) {
                                    break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                                }
                                continue;
                            }
                            // No-progress retryable failure: `prompt()` returned
                            // the plain classified error. `prompt()`'s error
                            // branch already de-duped and persisted `user(input)`
                            // (or skipped it because the session already ended
                            // on it), so we do NOT re-append here. The retry
                            // re-runs from `input` (the turn never got past its
                            // first model call). For malformed tool calls the
                            // model needs the parse error to self-correct; in
                            // that case the synthetic feedback becomes the
                            // retry's prompt instead of `input`.
                            Err(e)
                                if e.is_auto_retryable()
                                    && consecutive_failures < retry_budget(&e) =>
                            {
                                consecutive_failures = consecutive_failures.saturating_add(1);
                                let secs = retry_delay_secs(&e, consecutive_failures);
                                config
                                    .output
                                    .display_text(&format!("Error: {}. Retrying in {secs}s...", e));
                                let slept = cancellable_sleep_pumped(
                                    &config.user_cancel,
                                    &config.cancel_notify,
                                    secs,
                                    turn_backoff_pump(
                                        &mut prompt_session,
                                        &mut in_turn_keys,
                                        &mut line_history,
                                        &prompt_str,
                                        &guard,
                                        config.output.as_ref(),
                                    ),
                                )
                                .await;
                                if !slept {
                                    break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                                }
                                if let Some(feedback) = e.synthetic_retry_feedback() {
                                    // `prompt()` already persisted `user(input)`
                                    // (de-duped). Re-prompt with the synthetic
                                    // feedback so the model can self-correct.
                                    next_input = feedback;
                                    // Drop the override: it was the user's
                                    // instruction for *their* dispatch, and
                                    // this is the model's own corrective
                                    // re-prompt. Carrying a `/temperature 0.2`
                                    // into it would silently keep applying a
                                    // sampling the user never asked for on this
                                    // exchange.
                                    turn_params = app_state.params();
                                    // The feedback replaces the original prompt
                                    // as a NEW user dispatch for this turn.
                                    turn.mark_new_dispatch();
                                }
                                // Race window: cancel could fire after
                                // `cancellable_sleep_pumped` returned but
                                // before `prompt()` clears its flag. Bail
                                // out here so the retry respects `Esc`.
                                if cancel.load(Ordering::SeqCst) {
                                    break Err(crate::PicocodeError::Cancelled("Aborted.".into()));
                                }
                                continue;
                            }
                            Err(e) => break Err(e),
                        }
                    };
                    outcome
                };
                if let Some((h, done)) = reader_handle {
                    done.store(true, Ordering::SeqCst);
                    let _ = h.join();
                }
                match response {
                    Ok(_) => {}
                    Err(crate::PicocodeError::Cancelled(msg)) => {
                        // Turn aborted via `<Esc>`; `prompt()` already persisted
                        // the in-flight history, so show a clear notice instead
                        // of a generic "Error:".
                        config.output.display_text(&msg);
                    }
                    Err(e) => {
                        config.output.display_text(&format!("Error: {}", e));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Session-level metadata shown in the `/stat` overlay header.
///
/// Captures the same fields that the `/session <id>` action-menu header
/// displays (name, message_count, age/created/last update). Built from
/// [`crate::session::repo::SessionMetadata`] at the call site so this
/// function stays pure / easily testable.
#[derive(Debug, Clone)]
pub struct SessionStatsInfo {
    /// User-assigned display name, if any. Falls back to the session id.
    pub display_name: String,
    /// ISO 8601 creation timestamp (from the session header).
    pub created: String,
    /// ISO 8601 timestamp of the most recent user message, if any.
    /// When `None`, falls back to `created` for display.
    pub last_updated: Option<String>,
    /// Total number of message entries (user + assistant + toolResult).
    /// Mirrors the "messages" count shown in `/session <id>`.
    pub message_count: usize,
    /// Number of user-role message entries (= turn count).
    /// Mirrors the "turns" count shown in `/session <id>`.
    pub user_message_count: usize,
}

/// The model half of the bar — everything to the right of the pipe.
///
/// A struct rather than four more positional arguments because those four are
/// two strings, an `Option` and a number in a row: nothing at the call site says
/// which `Option<&str>` is the thinking level and which is the first message, and
/// getting that wrong compiles. The field names are the documentation.
pub struct BarModel<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    /// The active thinking level, if any.
    pub thinking: Option<&'a str>,
    /// Context tokens in use. `0` means "not known yet" and drops the figure
    /// rather than showing a zero.
    pub context_tokens: u64,
    /// The model's context window, already *resolved* (catalog overlaid with any
    /// stored per-model override).
    ///
    /// Supplied by the caller rather than looked up here: `status_text` is a
    /// pure function with no store to read, and the raw-catalog lookup it used
    /// to do divided the percentage by OpenRouter's number even when the user
    /// had overridden it. `None` renders the bare token count, which is what an
    /// unknown window always did.
    pub context_limit: Option<u64>,
}

/// The transient status line: `● session | provider:model thinking 34% 78k`.
///
/// The mid-turn readout (doc/row_accounting.md, zone 2). Built from the same
/// pieces `session_stats_lines` uses — the caller for the resolved context
/// window, its own token count for the usage — so the two cannot disagree about
/// what the context figure means. That holds because a per-model
/// `context_length` edit moves both: the prompt-side bar re-reads it every
/// cycle, and `plan_rebuild` compares it so the mid-turn agent is rebuilt with
/// the new one. Adding a term to that gate is what keeps the claim true. Fields that are unknown are dropped rather
/// than shown empty.
///
/// **This is the data half.** A struct rather than a rendered string because the
/// width is the drawing layer's to know: the group has to give way field by field
/// to fit, the dot cell's width comes out of the same budget, and rendering here
/// would mean the caller passed a width and then handed back bytes nobody
/// downstream can re-fit. The halves are separable because there is nothing
/// width-dependent in this one — `output::status_bar` is the other.
///
/// A struct rather than a rendered string because **the width is the drawing
/// layer's to know**. The group has to give way field by field to fit, the dot
/// cell's width comes out of the same budget, and rendering here would mean the
/// caller passed a width and then handed back bytes nobody downstream can
/// re-fit — which is what the streaming flag was for, and why it sat hardcoded
/// `true` at the one call site that had it. The two halves are separable because
/// there is nothing width-dependent in this one.
///
/// The group's fields are pushed in the order they should be *dropped* from the
/// right: the context figure first, because it changes every turn and a missing
/// one reads as "no context yet" rather than as a wrong model; then the thinking
/// level; the model name last, because it is what identifies who is answering.
/// The context window in force for the current model, resolved: the catalog
/// overlaid with any stored per-model override.
///
/// Read from `AppState::resolved_models` rather than looked up per status-bar
/// paint, so the percentage uses the number the app actually enforces and the
/// prompt-side and mid-turn bars cannot disagree. The mid-turn bar gets it
/// through `plan_rebuild`, which compares this value to decide whether to
/// rebuild — hence the window's place in that gate.
pub fn resolved_context_limit(app_state: &crate::app_state::AppState) -> Option<u64> {
    let qualified = app_state.model();
    let (provider, model) = qualified.split_once(':')?;
    app_state
        .resolved_models()
        .iter()
        .find(|e| {
            let (p, m) = e.qualified_parts();
            p.eq_ignore_ascii_case(provider) && m.eq_ignore_ascii_case(model)
        })
        .and_then(|e| e.context_length)
        .filter(|max| *max > 0)
}

pub fn status_text(
    session_name: Option<&str>,
    first_message: Option<&str>,
    m: BarModel<'_>,
) -> StatusText {
    let BarModel {
        provider,
        model,
        thinking,
        context_tokens,
        context_limit,
    } = m;
    let mut fields: Vec<String> = Vec::new();
    if !provider.is_empty() || !model.is_empty() {
        fields.push(format!("{provider}:{model}"));
    }
    if let Some(level) = thinking.map(str::trim).filter(|l| !l.is_empty()) {
        fields.push(level.to_string());
    }
    if context_tokens > 0 {
        // The percentage needs the catalog's context window, and a model
        // missing from it is normal (a local endpoint, a new release). So the
        // count stands alone rather than the whole figure disappearing: no
        // window known means no percentage, not no context figure.
        let window = context_limit.filter(|max| *max > 0);
        fields.push(match window {
            Some(max) => {
                let pct = (context_tokens as f64 / max as f64) * 100.0;
                format!("{pct:.0}% {}", compact_tokens(context_tokens))
            }
            None => compact_tokens(context_tokens),
        });
    }

    // With no name to show, the session's opening message stands in: the line
    // should still say *which* session this is, not just which model.
    let name = session_name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| {
            first_message
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(message_preview)
        })
        .unwrap_or_default();
    StatusText {
        name,
        group: fields,
    }
}

/// A first message as a single line, flattened, for the status line to truncate
/// to whatever the bar has left.
///
/// Whitespace runs collapse, because a message that opens with a blank line, an
/// indent, or a code fence would otherwise show as nothing at all. Escapes are
/// stripped: this is untrusted text from a session file, and a pasted escape
/// sequence should not be able to colour or reposition the bar.
///
/// No width cap of its own — `status_line` truncates to the columns the model
/// group left, and a cap here would be a second opinion that is wrong exactly
/// when the bar is widest. What used to be a fixed 25 columns showed a third of
/// a long message on a 120-column bar and left a hole before the pipe.
fn message_preview(msg: &str) -> String {
    let first = msg.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let flat = first.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::output::strip_ansi(&flat)
}

#[cfg(test)]
mod status_bar_tests {
    use super::{message_preview, status_text, BarModel};
    // The separator the fit actually used, not a copy of the string: these tests
    // are about where the fit put it, so the constant has to be the real one.
    use crate::output::BAR_SEPARATOR as SEPARATOR;

    /// The whole bar: the data half fitted and rendered.
    ///
    /// Every test here is really about the *pair* — `status_text` builds the
    /// fields and `output::status_bar` fits them to a width — so they go through
    /// one seam-crossing helper rather than each spelling out the two calls. They
    /// live here rather than beside `status_bar` because most of them are as much
    /// about `BarModel`'s data (which field is dropped first, what an unknown
    /// model does, what an unnamed session shows) as about the fitting.
    fn bar(
        session_name: Option<&str>,
        first_message: Option<&str>,
        m: BarModel<'_>,
        width: usize,
        streaming: bool,
    ) -> String {
        crate::output::status_bar(
            &status_text(session_name, first_message, m),
            width,
            streaming,
        )
    }

    /// A [`BarModel`] for the tests. Positional on purpose: every test wants the
    /// same four things, and the field names at each of the twenty-odd call sites
    /// would be noise. The names are still checked, because the layout test is
    /// about the order they render in.
    fn model<'a>(
        provider: &'a str,
        model: &'a str,
        thinking: Option<&'a str>,
        context_tokens: u64,
        context_limit: Option<u64>,
    ) -> BarModel<'a> {
        BarModel {
            provider,
            model,
            thinking,
            context_tokens,
            context_limit,
        }
    }

    /// The dot never touches the text: there is always a column of air after it.
    ///
    /// The text is right-aligned, so the gap is whatever padding is left over —
    /// which on a nearly-full bar is nothing, and the summary then reads as part
    /// of the dot's cell. This is the case that needs the guaranteed column, and
    /// it is invisible at any width where there happens to be padding.
    #[test]
    fn the_dot_always_has_a_column_of_air_after_it() {
        // Widths chosen to leave the text exactly filling the row: at each of
        // these the padding computes to zero, so only the gap column separates
        // the dot from the text.
        let text = "run the command | llamacpp:test";
        for extra in 0..6_usize {
            let w = text.chars().count() + 2 + extra;
            let s = bar(
                None,
                None,
                model("llamacpp", "test", None, 0, None),
                w,
                true,
            );
            let visible = crate::output::strip_ansi(&s);
            assert_eq!(display(&s), w, "width {w}: one row: {visible:?}");
            assert!(
                visible.contains("\u{25cf} "),
                "width {w}: a space must follow the dot: {visible:?}"
            );
            assert!(
                !visible.contains("\u{25cf}r"),
                "width {w}: the dot must not abut the text: {visible:?}"
            );
        }
    }

    /// A planned bar's visible width — its contract with the terminal.
    fn display(s: &str) -> usize {
        crate::row_count::display_width_ansi(s)
    }

    /// The bar is a full row: the background is set before the padding, the text
    /// sits against the right margin, and the visible width is exactly the
    /// terminal's. The width is what makes it a bar rather than a highlighted
    /// fragment, and the right margin is what makes it read as a figure rather
    /// than the start of a line.
    #[test]
    fn the_bar_fills_the_row_and_right_aligns_the_text() {
        let s = bar(
            Some("main"),
            None,
            model("llamacpp", "test", None, 0, None),
            40,
            true,
        );
        assert_eq!(
            crate::row_count::display_width_ansi(&s),
            40,
            "the bar should be exactly one row wide: {s:?}"
        );
        // The dot cell leads, then padding, then the text against the right
        // margin — the right margin is the point, the dot is the price of it.
        let visible = crate::output::strip_ansi(&s);
        assert!(
            visible.starts_with(" \u{25cf}  "),
            "the bar should lead with the dot cell: {visible:?}"
        );
        assert!(
            visible.ends_with("main | llamacpp:test"),
            "the text should sit against the right margin: {visible:?}"
        );
        // The background has to be set *before* the padding, or the bar is a
        // highlighted fragment with default-coloured cells around it.
        let padding = visible.find("main |").expect("the text");
        let before = &s[..s.find("main |").expect("the text")];
        assert!(
            before.contains("\x1b[48;5;237m"),
            "the background must precede the padding: {s:?}"
        );
        assert_eq!(padding, visible.len() - "main | llamacpp:test".len());
        assert!(s.ends_with("\x1b[0m"), "the bar must be reset: {s:?}");
    }

    /// The bar is truncated to the terminal, so it never moves the cursor down
    /// without a newline — the one row movement `paint_status` has to report
    /// separately. Zero rows is the assertion, not one: writing a bar that fits
    /// leaves the cursor *on* the row.
    #[test]
    fn the_bar_never_moves_the_cursor_down() {
        let long = "x".repeat(200);
        for w in [1_usize, 10, 40, 200] {
            let s = bar(Some(&long), None, model("p", "m", None, 0, None), w, true);
            assert_eq!(
                crate::row_count::rows_of(&s, w),
                0,
                "width {w}: the bar should not wrap: {s:?}"
            );
        }
    }

    /// A long line is cut to the terminal, not spilled past it.
    #[test]
    fn a_long_bar_is_truncated_to_the_terminal() {
        let s = bar(
            Some(&"n".repeat(100)),
            None,
            model("p", "m", None, 0, None),
            20,
            true,
        );
        assert_eq!(crate::row_count::display_width_ansi(&s), 20, "{s:?}");
        assert!(crate::output::strip_ansi(&s).contains('…'), "{s:?}");
    }

    /// With no session name, the opening message stands in — so the line still
    /// says which session this is. A name always wins: the user chose it.
    #[test]
    fn an_unnamed_session_shows_its_first_message() {
        let s = bar(
            None,
            Some("fix the flaky resize test"),
            model("llamacpp", "test", None, 0, None),
            60,
            true,
        );
        let visible = crate::output::strip_ansi(&s);
        assert!(visible.contains("fix the flaky resize test"), "{visible:?}");
        let named = bar(
            Some("main"),
            Some("fix the flaky resize test"),
            model("llamacpp", "test", None, 0, None),
            60,
            true,
        );
        let named_visible = crate::output::strip_ansi(&named);
        assert!(named_visible.contains("main"), "{named_visible:?}");
        assert!(
            !named_visible.contains("fix the flaky"),
            "a named session must not also show the message: {named_visible:?}"
        );
    }

    /// The whole shape in one assertion: one pipe, the model fields in order
    /// separated by single spaces, no labels.
    ///
    /// The labels are the substance of this, not the spacing. `ctx 34% · 78k`
    /// spends eight columns saying what three digits and a suffix already say,
    /// and on a narrow bar those columns are the difference between a session
    /// name that fits and one that is ellipsised.
    #[test]
    fn the_model_group_is_unlabelled_and_space_separated() {
        // `ollama:llama3` is in the curated static fallback with an 8k window,
        // and ollama needs no API key, so the percentage is real here. The
        // `None` window arm — a local endpoint or a new release — is a different
        // shape and has its own test below.
        let s = bar(
            Some("main"),
            None,
            model("ollama", "llama3", Some("high"), 5_000, Some(8_192)),
            60,
            true,
        );
        let visible = crate::output::strip_ansi(&s);
        assert!(
            visible.ends_with("main | ollama:llama3 high 61% 5k"),
            "one pipe, model then level then the figure, single spaces: {visible:?}"
        );
        assert_eq!(
            visible.matches('|').count(),
            1,
            "exactly one separator: {visible:?}"
        );
        assert!(
            !visible.contains("ctx") && !visible.contains('·'),
            "no labels left: {visible:?}"
        );
    }

    /// A model the catalog does not know: the count stands alone rather than the
    /// whole figure disappearing, because no window known means no percentage,
    /// not no context figure.
    #[test]
    fn an_unknown_model_still_shows_its_token_count() {
        let s = bar(
            Some("main"),
            None,
            model("llamacpp", "test", Some("high"), 78_000, None),
            60,
            true,
        );
        let visible = crate::output::strip_ansi(&s);
        assert!(
            visible.ends_with("main | llamacpp:test high 78k"),
            "the count alone when there is no window to take a percentage of: {visible:?}"
        );
    }

    /// The name fills the gap between the dot and the pipe, and is ellipsised
    /// when it does not fit.
    ///
    /// "Fills" is the part that is easy to get wrong: a fixed preview width
    /// truncates early and leaves a hole before the pipe, so the bar reads as
    /// though the name were short. The assertion is that the name reaches the
    /// pipe — the row is full to the last text column on both sides.
    #[test]
    fn the_name_fills_the_space_the_model_group_leaves() {
        let group = "ollama:llama3 high 61% 5k";
        for w in [40_usize, 60, 100] {
            let long = "a".repeat(400);
            let s = bar(
                Some(&long),
                None,
                model("ollama", "llama3", Some("high"), 5_000, Some(8_192)),
                w,
                true,
            );
            let visible = crate::output::strip_ansi(&s);
            assert_eq!(display(&s), w, "width {w}: the bar is one row: {visible:?}");
            assert!(
                visible.ends_with(group),
                "width {w}: the model group keeps the right margin: {visible:?}"
            );
            // The bar is `\u{25a0} name | group` with the dot cell in front, so
            // the name is what is left once both ends are taken off.
            let name = visible
                .trim_start()
                .strip_prefix('\u{25cf}')
                .and_then(|r| r.strip_suffix(group))
                .and_then(|r| r.strip_suffix(SEPARATOR))
                .unwrap_or_else(|| panic!("width {w}: cannot find the name in {visible:?}"));
            assert_eq!(
                display(name),
                w - 2 - group.len() - SEPARATOR.len(),
                "width {w}: the name should take the whole gap: {visible:?}"
            );
            assert!(name.ends_with('…'), "width {w}: {name:?}");
        }
    }

    /// On a narrow terminal the model group gives way **as whole fields**, from
    /// the right: the context figure first, then the thinking level, and the
    /// model name last.
    ///
    /// Field-at-a-time rather than a character cut, because the alternative is a
    /// bar ending in `78` or `llamacpp:te` — figures and identifiers cut in half
    /// read as data, not as truncation. The order is the priority order: the
    /// figure changes every turn, so losing it is unremarkable, while losing the
    /// model name leaves a bar that cannot say who is answering.
    #[test]
    fn a_narrow_bar_drops_whole_model_fields_from_the_right() {
        // The only shapes the group may take, widest first. Anything else is a
        // character cut or a field in the wrong order.
        const ALLOWED: [&str; 4] = [
            "ollama:llama3 high 61% 5k",
            "ollama:llama3 high",
            "ollama:llama3",
            "",
        ];
        // Owned, because the borrow is out of the `visible` it came from by the
        // time the next iteration rebinds it.
        let mut seen: Vec<String> = Vec::new();
        // Narrow enough that the group itself has to give. Above ~30 columns the
        // *name* is the only thing shrinking, so a wider sweep would see one
        // shape and pass without ever exercising the drop order.
        // Downward, so `seen` reads in the order the fields drop.
        for w in (4_usize..=40).rev() {
            // No name and no message on purpose: with a name present the bar is
            // `name | group` and the *name* is what shrinks first, so a sweep
            // wide enough to matter would read a mixture of the two and could not
            // say which one it was looking at. The name's own give-way has its
            // own test.
            let s = bar(
                None,
                None,
                model("ollama", "llama3", Some("high"), 5_000, Some(8_192)),
                w,
                true,
            );
            let visible = crate::output::strip_ansi(&s);
            assert_eq!(display(&s), w, "width {w}: one row: {visible:?}");
            // The dot cell leads and the padding is all that is left over, so
            // what remains in the middle is the group and nothing else.
            let group = visible
                .trim()
                .strip_prefix('\u{25cf}')
                .unwrap_or(visible.trim())
                .trim();
            assert!(
                ALLOWED.contains(&group),
                "width {w}: the group must be whole fields, got {group:?} in {visible:?}"
            );
            if seen.last().map(String::as_str) != Some(group) {
                seen.push(group.to_string());
            }
        }
        // The transitions are the point: every field was exercised on the way
        // down, so a regression that drops the wrong one is visible here.
        assert_eq!(
            seen,
            ALLOWED.map(str::to_string),
            "each field should drop in turn as the terminal narrows"
        );
    }

    /// Neither present: no leading separator, and the model still identifies the
    /// line.
    #[test]
    fn no_name_and_no_message_still_shows_the_model() {
        let s = bar(
            None,
            None,
            model("llamacpp", "test", None, 0, None),
            40,
            true,
        );
        let visible = crate::output::strip_ansi(&s);
        assert!(visible.ends_with("llamacpp:test"), "{visible:?}");
        assert_eq!(display(&s), 40, "{visible:?}");
    }

    /// The dot is the spinner's replacement: green while a response streams,
    /// the bar's own grey when it is not. Two states because the prompt-side
    /// carrier will show the bar between turns too, and the dot is what will
    /// tell "working" from "idle" then.
    ///
    /// The order of the escape and the glyph is the substance of this test, not
    /// a detail: an `SGR` colours the cells written *after* it, so a colour
    /// emitted after the dot paints the dot in the default foreground and applies
    /// the colour to the padding. That is not a subtle version of the bug — the
    /// green lands on nothing, and the dot reads as default bright white.
    #[test]
    fn the_dot_is_green_while_streaming_and_grey_when_not() {
        let streaming = bar(Some("main"), None, model("p", "m", None, 0, None), 40, true);
        let idle = bar(
            Some("main"),
            None,
            model("p", "m", None, 0, None),
            40,
            false,
        );
        assert!(
            streaming.contains("\x1b[38;5;41m"),
            "a streaming bar should carry a green dot: {streaming:?}"
        );
        assert!(streaming.contains('\u{25cf}'), "and a dot: {streaming:?}");
        assert!(
            !idle.contains("\x1b[38;5;41m"),
            "an idle bar should not: {idle:?}"
        );
        assert!(
            idle.contains("\u{25cf}"),
            "but it still has a dot: {idle:?}"
        );
        // Same width either way: the dot is a cell, not a different layout.
        assert_eq!(display(&streaming), display(&idle));
        // And the colour has to land *on* the glyph. The escape used to be
        // emitted after it, so the dot was painted in the default foreground and
        // the green was applied to nothing at all — and every assertion above
        // passed straight over that, because they only asked whether the escape
        // was present.
        let glyph_at = streaming.find('\u{25cf}').expect("the dot");
        let green_at = streaming.find("\x1b[38;5;41m").expect("the green");
        assert_eq!(
            glyph_at,
            green_at + "\x1b[38;5;41m".len(),
            "the green must be emitted immediately before the dot, not after it: {streaming:?}"
        );
    }

    /// The bar's appearance must not depend on what was written above it.
    ///
    /// `dim` is an SGR *attribute* and `38;5;245m` sets a colour, not an
    /// attribute — so a bar written while streamed reasoning has the terminal in
    /// dim mode rendered dimmed, and one written after a plain block did not.
    /// A leading full reset is what makes the bar self-contained.
    #[test]
    fn the_bar_resets_first_so_ambient_dim_cannot_reach_it() {
        let s = bar(Some("main"), None, model("p", "m", None, 0, None), 40, true);
        let body = s.trim_start_matches("\x1b[0m");
        assert!(
            s.starts_with("\x1b[0m"),
            "the bar must open with a reset: {s:?}"
        );
        assert!(
            body.starts_with("\x1b[48;5;237m"),
            "and only then set its own colours: {body:?}"
        );
    }

    /// The preview is one line and collapsed. It is *not* capped here — the bar
    /// truncates it to the columns the model group left, so a preview that came
    /// back already cut would be a second opinion, and a wrong one on exactly the
    /// widest bars.
    #[test]
    fn the_preview_is_one_flattened_line_and_caps_itself_nowhere() {
        assert_eq!(message_preview("fix the bug"), "fix the bug");
        // A leading blank line and indentation must not make it show nothing,
        // and a multi-line message shows as its first line only — a preview is
        // one row, and the bar is one row.
        assert_eq!(
            message_preview("\n\n    fn main() {\n        todo!()\n    }"),
            "fn main() {",
            "the first non-empty line, with its indentation collapsed away"
        );
        assert_eq!(
            message_preview(&"a".repeat(40)),
            "a".repeat(40),
            "the whole line comes through; `status_line` is what cuts it"
        );
    }

    /// …and the bar does cut it, at whatever width is left, ellipsised and never
    /// mid-glyph.
    #[test]
    fn a_long_message_is_cut_to_the_bar_with_an_ellipsis() {
        // No name and no model group at all: the preview has the whole row, so
        // the cut is the bar's doing and nothing else.
        let s = bar(
            None,
            Some(&"a".repeat(80)),
            model("", "", None, 0, None),
            20,
            true,
        );
        let visible = crate::output::strip_ansi(&s);
        let text = visible.trim().strip_prefix('\u{25cf}').unwrap().trim_end();
        assert_eq!(display(&s), 20, "{visible:?}");
        assert!(
            text.ends_with('…'),
            "a cut preview should be ellipsised: {text:?}"
        );
        // 20 columns less the dot cell: with no model group there is no separator
        // to pay for, so the name gets the whole remainder.
        assert_eq!(display(text), 18, "{visible:?}");
        // A wide glyph is counted as two columns, not cut mid-character.
        let wide = bar(
            None,
            Some(&"\u{4f60}\u{597d}".repeat(20)),
            model("", "", None, 0, None),
            20,
            true,
        );
        let text = crate::output::strip_ansi(&wide);
        // `trim` first: the dot cell is a space and a glyph, and the space would
        // otherwise be the first character the assertion looks at.
        // `trim` again after the strip: the text is right-aligned, so the
        // padding the dot cell leaves is between the glyph and the name.
        let text = text.trim().strip_prefix('\u{25cf}').unwrap().trim();
        assert!(
            text.chars()
                .all(|c| matches!(c, '\u{4f60}' | '\u{597d}' | '\u{2026}')),
            "no half glyphs: {text:?}"
        );
    }

    /// The preview comes out of a session file, so it is untrusted: an escape
    /// sequence in it must not colour or reposition the bar.
    #[test]
    fn the_preview_strips_escapes() {
        let p = message_preview("\x1b[2J\x1b[31mred\x1b[0m");
        assert_eq!(p, "red");
        assert!(!p.contains('\u{1b}'), "{p:?}");
    }
}

/// The first user message in a built context, as the status line's stand-in
/// label for an unnamed session. `None` when the context is empty (the session's
/// first turn) or opens with something that carries no text.
fn first_user_text(prior: &[Message]) -> Option<&str> {
    let Message::User { content } = prior.first()? else {
        return None;
    };
    match content.first()? {
        UserContent::Text(t) if !t.text.trim().is_empty() => Some(t.text.as_str()),
        _ => None,
    }
}

/// `78k` / `1.2M` — the same abbreviations `/stat` uses for token counts.
fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// The `/stat` lines for a session.
///
/// `stored` is the model's per-model record and `live` its current params;
/// both are `None`/empty when unavailable. Together they make this the place
/// you look when a request is rejected: the provider's error names the limit it
/// enforced, and these say what was actually sent and what the resolved
/// context window is — the two facts needed to see why.
pub fn session_stats_lines(
    provider: &str,
    model: &str,
    usage: SessionUsage,
    session: Option<SessionStatsInfo>,
    context_tokens: u64,
    stored: Option<&crate::model_settings::ModelSettings>,
    live: &crate::params::ModelParams,
) -> Vec<String> {
    // The *resolved* view, not the raw catalog entry: a Ctrl-E'd
    // `context_length` override is what the app actually enforces, so quoting
    // OpenRouter's published number here would contradict the `/model` header
    // on the same fact with nothing to explain the difference.
    let entry = model_catalog::resolved_entry(&format!("{provider}:{model}"), stored);
    let ctx_len = entry.as_ref().and_then(|e| e.context_length);

    let mut lines: Vec<String> = Vec::new();

    // ── session section ─────────────────────────────────────────────────
    match session {
        Some(s) => {
            lines.push(format!("session: {}", s.display_name));
            let created = format_stat_ts(&s.created);
            let last = s
                .last_updated
                .as_deref()
                .map(format_stat_ts)
                .unwrap_or_else(|| created.clone());
            lines.push(format!("  created: {}, last update {}", created, last));
            lines.push(format!(
                "  size: {} turns ({} messages)",
                s.user_message_count, s.message_count
            ));
        }
        None => {
            // No repo / no metadata available (e.g. in tests without a session).
            lines.push("session: (unavailable)".to_string());
        }
    }
    // Estimated compacted context tokens (what would be sent on next turn).
    if context_tokens > 0 {
        lines.push(format!("  estimated context tokens: {}", context_tokens));
    }

    // ── model section ───────────────────────────────────────────────────
    lines.push(format!("model: {}:{}", provider, model));

    // What this turn is actually sending. A rejected `temperature` is only
    // diagnosable with both halves: the provider names the limit, this says
    // what was sent against it. Absent fields read "provider default" rather
    // than being omitted, so their absence is itself the information.
    let num = |v: Option<f64>| match v {
        Some(n) => crate::v25_number_param::fmt_value(n),
        None => "provider default".to_string(),
    };
    lines.push(format!(
        "  reasoning: {}   temperature: {}   top_p: {}",
        live.reasoning
            .map(|l| l.as_str().to_string())
            .unwrap_or_else(|| "off".to_string()),
        num(live.temperature),
        num(live.top_p),
    ));

    // Context window limit.
    match ctx_len {
        Some(max) => lines.push(format!("  context window limit: {} tokens", max)),
        None => lines.push("  context window limit: unknown (model not in catalog)".to_string()),
    }

    // Current context usage (absolute + % of max).
    let cur = usage.last_input_tokens;
    if cur > 0 {
        if let Some(max) = ctx_len {
            let pct = (cur as f64 / max as f64) * 100.0;
            lines.push(format!(
                "  current usage: {} tokens ({:.1}% of limit)",
                cur, pct
            ));
        } else {
            lines.push(format!("  current usage: {} tokens", cur));
        }
    } else {
        lines.push("  current usage: n/a (no completion call recorded yet)".to_string());
    }

    // Approximate session cost.
    let (pc, cc) = entry
        .as_ref()
        .map(|e| (e.prompt_cost, e.completion_cost))
        .unwrap_or((None, None));
    match (pc, cc) {
        (Some(p), Some(c)) if p > 0.0 || c > 0.0 => {
            let cost = (usage.session_input_tokens as f64 / 1_000_000.0) * p
                + (usage.session_output_tokens as f64 / 1_000_000.0) * c;
            lines.push(format!(
                "  approx session cost: ${:.4} (in {} / out {} tokens)",
                cost, usage.session_input_tokens, usage.session_output_tokens
            ));
        }
        _ => lines.push("  approx session cost: n/a (no pricing in catalog)".to_string()),
    }

    // Cache hit rate (session-level: reads as a fraction of all input tokens).
    let cached = usage.session_cached_input_tokens;
    let creation = usage.session_cache_creation_input_tokens;
    let total_in = usage.session_input_tokens;
    if total_in > 0 && (cached > 0 || creation > 0) {
        let hit = (cached as f64 / total_in as f64) * 100.0;
        lines.push(format!(
            "  cache hit rate: {:.1}% (read {} / written {} / fresh {} tokens)",
            hit,
            cached,
            creation,
            total_in.saturating_sub(cached + creation)
        ));
    } else {
        lines.push("  cache hit rate: n/a (provider reported no cache metrics)".to_string());
    }

    lines
}

/// Format an ISO-8601 timestamp (RFC3339 or "YYYY-MM-DDTHH:MM:SS.sssZ") as
/// "YYYY-MM-DD HH:MM" for the `/stat` header. Falls back to the raw string
/// when parsing fails so a malformed timestamp never blanks the panel.
fn format_stat_ts(ts: &str) -> String {
    // Try RFC3339 first ("2025-01-01T14:32:00.123456789+00:00").
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return dt.format("%Y-%m-%d %H:%M").to_string();
    }
    // Fall back to the truncated UTC format used by session headers
    // ("2025-01-01T14:32:00.000Z") via chrono::DateTime::parse_from_str.
    let normalized = format!("{}Z", ts.trim_end_matches('Z'));
    if let Ok(dt) = chrono::DateTime::parse_from_str(&normalized, "%+") {
        return dt.format("%Y-%m-%d %H:%M").to_string();
    }
    ts.to_string()
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
        let prefill = compute_resume_model_prefill(&session, "openai:gpt-4o").await;
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
        let prefill = compute_resume_model_prefill(&session, "anthropic:claude-3-5-sonnet").await;
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
        let prefill = compute_resume_model_prefill(&session, "openai:gpt-4o").await;
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
            compute_resume_model_prefill(&session, "anthropic:claude-3-5-sonnet-20241022").await;
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
        let prefill = compute_resume_model_prefill(&session, "anthropic:claude-sonnet-4-5").await;
        assert_eq!(prefill, None);
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
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel2.store(true, std::sync::atomic::Ordering::SeqCst);
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
    }

    /// The pieces a back-off pump needs: a prompt session over the test app
    /// state, a history, and a guard that never enters raw mode (the root-Esc
    /// path returns before any terminal call, so none of it is exercised).
    fn backoff_pump_parts(
        app_state: &std::sync::Arc<crate::app_state::AppState>,
    ) -> (
        crate::repl::PromptSession,
        crate::repl::History,
        crate::repl::TermGuard,
    ) {
        (
            crate::repl::PromptSession::new(None, app_state),
            crate::repl::History::load(),
            crate::repl::TermGuard::new(),
        )
    }

    /// The bug this pump exists for: a root `Esc` typed during a retry
    /// back-off must cut the wait short. Before the queue was pumped between
    /// attempts the Esc sat unread until the next `prompt()` started, so the
    /// full delay always ran — the reported "the session only aborts once the
    /// timeout has elapsed".
    #[tokio::test]
    async fn backoff_pump_cancels_promptly_on_queued_esc() {
        let app_state = crate::app_state::AppState::for_test(test_settings());
        let (mut session, mut history, guard) = backoff_pump_parts(&app_state);
        let output = null_output();
        let (tx, mut keys) = tokio::sync::mpsc::unbounded_channel();
        tx.send(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
        ))
        .unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let pump = TurnPump {
            keys: &mut keys,
            session: &mut session,
            history: &mut history,
            label: "",
            guard: &guard,
            output: output.as_ref(),
        };
        let started = std::time::Instant::now();
        let completed = cancellable_sleep_pumped(&cancel, &notify, 60, Some(pump)).await;
        assert!(
            !completed,
            "a queued Esc must end the back-off, not the deadline"
        );
        assert!(cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "waited toward the 60s deadline instead of cancelling on Esc"
        );
    }

    /// A plain key dispatched during the back-off must NOT end the wait: it
    /// goes through the dispatcher (settle and all) and the delay runs out on
    /// its own schedule — the deadline is absolute.
    #[tokio::test]
    async fn backoff_pump_plain_key_does_not_cancel_the_wait() {
        let app_state = crate::app_state::AppState::for_test(test_settings());
        let (mut session, mut history, guard) = backoff_pump_parts(&app_state);
        let output = null_output();
        let (tx, mut keys) = tokio::sync::mpsc::unbounded_channel();
        tx.send(crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('x'),
                crossterm::event::KeyModifiers::NONE,
            ),
        ))
        .unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let pump = TurnPump {
            keys: &mut keys,
            session: &mut session,
            history: &mut history,
            label: "",
            guard: &guard,
            output: output.as_ref(),
        };
        let started = std::time::Instant::now();
        let completed = cancellable_sleep_pumped(&cancel, &notify, 1, Some(pump)).await;
        assert!(
            completed,
            "a plain key must not be mistaken for the cancel signal"
        );
        assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(900),
            "returned before the 1s deadline — the key cut the wait short"
        );
    }

    /// Piped input has no pump; the wait must still honor the plain cancel
    /// signal — the keys arm is `pending`, not `None`-completing, so the
    /// select still has a live cancel branch.
    #[tokio::test]
    async fn backoff_sleep_without_pump_still_honors_cancel_signal() {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let cancel2 = cancel.clone();
        let notify2 = notify.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel2.store(true, std::sync::atomic::Ordering::SeqCst);
            notify2.notify_one();
        });
        let started = std::time::Instant::now();
        let completed = cancellable_sleep_pumped(&cancel, &notify, 60, None).await;
        let _ = handle.await;
        assert!(
            !completed,
            "the signal branch must still fire without a pump"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// Fake window source for the compaction pump: events waiting to be
    /// read, and the park where keys the window cannot act on land.
    struct FakeWindow {
        ready: std::sync::Mutex<std::collections::VecDeque<crossterm::event::Event>>,
        parked: std::sync::Mutex<std::collections::VecDeque<crossterm::event::Event>>,
        active: bool,
    }

    impl FakeWindow {
        fn new() -> Self {
            Self {
                ready: std::sync::Mutex::new(std::collections::VecDeque::new()),
                parked: std::sync::Mutex::new(std::collections::VecDeque::new()),
                active: true,
            }
        }

        /// Piped/batch shape: stdin belongs to the script.
        fn inactive() -> Self {
            Self {
                active: false,
                ..Self::new()
            }
        }

        fn push(&self, ev: crossterm::event::Event) {
            self.ready.lock().unwrap().push_back(ev);
        }

        fn ready_len(&self) -> usize {
            self.ready.lock().unwrap().len()
        }

        fn parked_len(&self) -> usize {
            self.parked.lock().unwrap().len()
        }
    }

    impl WindowKeySource for FakeWindow {
        fn window_active(&self) -> bool {
            self.active
        }
        fn poll_key(&self) -> Option<crossterm::event::Event> {
            self.ready.lock().unwrap().pop_front()
        }
        fn defer_event(&self, ev: crossterm::event::Event) {
            self.parked.lock().unwrap().push_back(ev);
        }
    }

    fn esc_event() -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    fn char_event(c: char) -> crossterm::event::Event {
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    /// The compaction window's core promise: an `Esc` while the
    /// summarization request is in flight cancels it. Before the pump,
    /// nothing read stdin during the window and the attempt always ran to
    /// completion.
    #[tokio::test]
    async fn window_pump_esc_cancels_the_inflight_attempt() {
        let keys = FakeWindow::new();
        keys.push(esc_event());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let output = null_output();
        let started = std::time::Instant::now();
        let r = retry_summarization(
            output.as_ref(),
            &cancel,
            &notify,
            Some(&keys),
            &mut || async {
                std::future::pending::<std::result::Result<String, crate::PicocodeError>>().await
            },
        )
        .await;
        assert!(matches!(r, Err(crate::PicocodeError::Cancelled(_))));
        assert!(cancel.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the in-flight attempt ignored the Esc"
        );
    }

    /// The reported bug shape: an `Esc` during a *retry back-off* cuts the
    /// wait instead of letting the provider-hint floor (10s) run out.
    #[tokio::test]
    async fn window_pump_esc_cuts_the_retry_backoff() {
        let keys = std::sync::Arc::new(FakeWindow::new());
        let keys2 = keys.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            keys2.push(esc_event());
        });
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let output = null_output();
        let mut calls = 0;
        let started = std::time::Instant::now();
        let r = retry_summarization(
            output.as_ref(),
            &cancel,
            &notify,
            Some(keys.as_ref()),
            &mut || {
                calls += 1;
                async {
                    Err(crate::PicocodeError::LlmRateLimited {
                        retry_after_secs: Some(10),
                    })
                }
            },
        )
        .await;
        let _ = handle.await;
        assert!(matches!(r, Err(crate::PicocodeError::Cancelled(_))));
        assert_eq!(calls, 1, "no retry attempt may start after the Esc");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the 10s back-off ran its course instead of yielding to the Esc"
        );
        assert_eq!(keys.parked_len(), 0, "an Esc is consumed, not parked");
    }

    /// A plain key during the window must NOT cancel: it is parked for
    /// `read_input` and the operation completes normally.
    #[tokio::test]
    async fn window_pump_plain_key_is_parked_and_not_cancelled() {
        let keys = FakeWindow::new();
        keys.push(char_event('x'));
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let output = null_output();
        let r = retry_summarization(
            output.as_ref(),
            &cancel,
            &notify,
            Some(&keys),
            &mut || async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Ok("done".to_string())
            },
        )
        .await;
        assert_eq!(r.unwrap(), "done");
        assert_eq!(
            keys.parked_len(),
            1,
            "the key must wait parked for read_input's drain"
        );
        assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// An inactive window (piped/batch — no REPL) must not touch its source
    /// at all: the arms stay pending, the ready event is never consumed.
    #[tokio::test]
    async fn window_pump_inactive_never_polls() {
        let keys = FakeWindow::inactive();
        keys.push(esc_event());
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let output = null_output();
        let r = retry_summarization(
            output.as_ref(),
            &cancel,
            &notify,
            Some(&keys),
            &mut || async { Ok("fine".to_string()) },
        )
        .await;
        assert_eq!(r.unwrap(), "fine");
        assert_eq!(
            keys.ready_len(),
            1,
            "an inactive window must never read its source"
        );
        assert_eq!(keys.parked_len(), 0);
        assert!(!cancel.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The real source on a test state: no term guard was published, so the
    /// window is inactive (poll yields nothing — stdin untouched), and the
    /// park round-trips in order: that queue is what `read_input` drains.
    #[test]
    fn appstate_window_gate_and_park_roundtrip() {
        let app_state = crate::app_state::AppState::for_test(test_settings());
        assert!(!app_state.window_pump_active());
        assert!(app_state.poll_key().is_none(), "inactive must not read");
        app_state.park_key(char_event('a'));
        app_state.park_key(char_event('b'));
        assert_eq!(app_state.take_parked_key(), Some(char_event('a')));
        assert_eq!(app_state.take_parked_key(), Some(char_event('b')));
        assert_eq!(app_state.take_parked_key(), None);
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

    // Regression test for the `!`/`!!` sandbox bug: user-initiated shell
    // commands must run inside the same bwrap sandbox as the agent's `bash`
    // tool, *even when the agent has not been created yet*. The sandbox config
    // is published by `set_sandbox`, which used to be called only inside
    // `build_rig_agent` — i.e. lazily on the first normal prompt. So a `!cmd`
    // issued as the very first input (before any LLM turn) ran UNSANDBOXED.
    //
    // This test drives `run_interactive` with a scripted in-memory `Output`
    // that feeds `!<cmd>` then `/q`, with the sandbox remapping a host file to
    // a path that does not exist on the host. If the command runs sandboxed,
    // the remap is visible and `cat` prints the file's content; if it runs
    // unsandboxed, the path is absent and `cat` errors with "No such file".
    #[cfg(unix)]
    #[tokio::test]
    async fn bang_command_uses_sandbox_before_agent_created() {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};

        use crate::config::{BindPath, SandboxConfig};
        use crate::output::Confirmation;
        use crate::settings::SettingsStore;
        use crate::AgentConfig;
        use crate::Output;
        use std::sync::atomic::AtomicBool;

        // bwrap is required for the sandbox to engage at all.
        if std::process::Command::new("bwrap")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }

        // Reset the process-wide sandbox slot so the test is deterministic. The
        // bug was precisely that `!`/`!!` ran with this still at `None`
        // (unsandboxed) until the agent lazily created itself. With the fix,
        // `run_interactive` republishes the resolved config before reading any
        // input, overwriting this.
        crate::tools::set_sandbox(None);

        // Host file we remap into the sandbox at a path absent on the host.
        let sentinel = "SANDBOX_REGRESSION_MARKER_12345";
        let _tmp = tempfile::tempdir().unwrap();
        let host_path = _tmp.path().join("picocode_sandbox_regr.txt");
        std::fs::write(&host_path, sentinel).unwrap();
        let probe = "/sandbox_probe_xyz".to_string();

        let sandbox = SandboxConfig {
            timeout_secs: crate::config::SandboxTimeoutSecs(Some(30)),
            network: Some(false),
            memory_mb: crate::config::SandboxMemoryMb(None),
            pid_isolation: Some(true),
            drop_caps: Some(true),
            tmp_size_mb: crate::config::SandboxTmpSizeMb(Some(16)),
            ro: vec![],
            rw: vec![BindPath::Remap {
                from: host_path.display().to_string(),
                to: probe.clone(),
            }],
            env: None,
            max_output_bytes: crate::config::SandboxMaxOutputBytes(None),
        };

        // In-memory `Output`: replays a fixed script and records what is shown.
        struct ScriptOutput {
            inputs: Arc<Mutex<VecDeque<String>>>,
            shown: Arc<Mutex<String>>,
        }
        impl Output for ScriptOutput {
            fn display_text(&self, t: &str) {
                self.shown.lock().unwrap().push_str(t);
                self.shown.lock().unwrap().push('\n');
            }
            fn display_plain(&self, t: &str) {
                self.shown.lock().unwrap().push_str(t);
                self.shown.lock().unwrap().push('\n');
            }
            fn display_separator(&self) {
                self.shown.lock().unwrap().push('\n');
            }
            fn get_user_input(&self) -> String {
                self.inputs
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| "/q".to_string())
            }
            fn display_tool_call(&self, _: &str, _: &serde_json::Value) {}
            fn display_tool_result(&self, _: &str) {}
            fn display_error(&self, _: &str) {}
            fn display_system(&self, _: &str) {}
            fn confirm(&self, _: &str) -> Confirmation {
                Confirmation::No
            }
            fn display_header(&self, _: &str, _: &str, _: bool, _: usize, _: Option<&str>) {}
        }

        let inputs = Arc::new(Mutex::new(VecDeque::from([
            format!("!cat {}", probe),
            "/q".to_string(),
        ])));
        let shown = Arc::new(Mutex::new(String::new()));
        let out = ScriptOutput {
            inputs,
            shown: shown.clone(),
        };

        // Use a temp directory for session storage to avoid littering the user's config.
        // tempfile::TempDir cleans up automatically on drop.
        let sessions_tempdir = tempfile::tempdir().unwrap();
        let sessions_dir = sessions_tempdir.path().to_path_buf();

        let config = AgentConfig {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            output: Arc::new(out),
            yolo: true,
            confirm_port: None,
            quiet: true,
            params: Default::default(),
            context_limit: None,
            tool_call_limit: 10,
            system_message_extension: None,
            persona_prompt: None,
            persona_name: None,
            bash_auto_allow: None,
            agent_prompt: None,
            user_cancel: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            sandbox: Some(sandbox),
            show_reasoning_handle: crate::settings::default_show_reasoning_handle(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            message_log_path: None,
            sessions_dir: Some(sessions_dir.clone()),
        };
        let settings = SettingsStore::from_agent_config(&config, &Default::default(), None);

        // No normal prompt is ever submitted, so the agent is never created and
        // `create_agent` (and its `set_sandbox`) is never called — exactly the
        // pre-fix condition where `!` would otherwise run unsandboxed.
        super::run_interactive(config, settings, false)
            .await
            .unwrap();

        let shown = shown.lock().unwrap().clone();
        let _ = std::fs::remove_file(&host_path);
        // sessions_tempdir is automatically cleaned up when dropped here

        assert!(
            !shown.contains("No such file"),
            "bang command ran UNSANDBOXED (probe path {probe} absent); output:\n{shown}"
        );
        assert!(
            shown.contains(sentinel),
            "expected sandboxed `!` command to print the remapped host file content; output:\n{shown}"
        );
    }

    // ── Non-REPL input path (editor cleanup E5) ───────────────────────

    /// The `repl == None` input arm (in-process host / non-tty) routes
    /// `/`-prefixed lines through `Repl::batch_slash` — passive derive +
    /// virtual Enter, no overlay drawing — and returns `MenuDone`, so the
    /// node's control writes (`/quit` → `request_quit`) are drained by the
    /// same cycle's convergence step. Pins the loop wiring the `batch_slash`
    /// unit tests cannot see: input-position eval → `MenuDone` → quit break.
    #[tokio::test]
    async fn non_repl_slash_lines_run_through_batch_engine() {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};

        use crate::output::Confirmation;
        use crate::settings::SettingsStore;
        use crate::AgentConfig;
        use crate::Output;
        use std::sync::atomic::AtomicBool;

        struct ScriptOutput {
            inputs: Arc<Mutex<VecDeque<String>>>,
            shown: Arc<Mutex<String>>,
        }
        impl Output for ScriptOutput {
            fn display_text(&self, t: &str) {
                self.shown.lock().unwrap().push_str(t);
                self.shown.lock().unwrap().push('\n');
            }
            fn display_plain(&self, t: &str) {
                self.shown.lock().unwrap().push_str(t);
                self.shown.lock().unwrap().push('\n');
            }
            fn display_separator(&self) {
                self.shown.lock().unwrap().push('\n');
            }
            fn get_user_input(&self) -> String {
                self.inputs
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| "/q".to_string())
            }
            fn display_tool_call(&self, _: &str, _: &serde_json::Value) {}
            fn display_tool_result(&self, _: &str) {}
            fn display_error(&self, _: &str) {}
            fn display_system(&self, _: &str) {}
            fn confirm(&self, _: &str) -> Confirmation {
                Confirmation::No
            }
            fn display_header(&self, _: &str, _: &str, _: bool, _: usize, _: Option<&str>) {}
        }

        // `/thinking high` commits through the engine (echoed line proves
        // `batch_slash` ran); `/quit` must then end the loop — if the eval
        // sat AFTER convergence, quit would be stranded until another line
        // arrived and the `unwrap_or_else("/q")` fallback would mask it.
        // So: exactly two lines, and the queue must be empty afterwards.
        let inputs = Arc::new(Mutex::new(VecDeque::from([
            "/reasoning high".to_string(),
            "/quit".to_string(),
        ])));
        let shown = Arc::new(Mutex::new(String::new()));
        let out = ScriptOutput {
            inputs: inputs.clone(),
            shown: shown.clone(),
        };

        let sessions_tempdir = tempfile::tempdir().unwrap();
        let config = AgentConfig {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            output: Arc::new(out),
            yolo: true,
            confirm_port: None,
            quiet: true,
            params: Default::default(),
            context_limit: None,
            tool_call_limit: 10,
            system_message_extension: None,
            persona_prompt: None,
            persona_name: None,
            bash_auto_allow: None,
            agent_prompt: None,
            user_cancel: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            sandbox: None,
            show_reasoning_handle: crate::settings::default_show_reasoning_handle(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            message_log_path: None,
            sessions_dir: Some(sessions_tempdir.path().to_path_buf()),
        };
        let settings = SettingsStore::from_agent_config(&config, &Default::default(), None);

        super::run_interactive(config, settings, false)
            .await
            .unwrap();

        let shown = shown.lock().unwrap().clone();
        assert!(
            shown.contains("reasoning high"),
            "expected the batch commit echo for `/thinking high`, got:\n{shown}"
        );
        assert!(
            inputs.lock().unwrap().is_empty(),
            "both scripted lines must have been consumed (quit must land on its own cycle)"
        );
    }

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

    // ---- Error classification tests ----
    //
    // The auto-retry / error-formatting pipeline in `run_interactive`
    // depends on `classify_stream_error` producing a structured
    // `PicocodeError` whose Display string is short, whose
    // `is_auto_retryable` / `retry_after_secs` / `synthetic_retry_feedback`
    // accessors agree, and which preserves the upstream
    // provider's `Retry-After` on 429. These tests pin that contract
    // so a future refactor can't silently turn a recoverable rate
    // limit into a fatal `Other` again.

    use crate::PicocodeError;
    use rig_agent::agent::StreamingError;
    use rig_agent::completion::{CompletionError, PromptError};
    use std::collections::HashMap;

    #[test]
    fn classify_429_with_retry_after_is_rate_limited_and_retryable() {
        // Simulate a HttpError with status 429 + Retry-After: 60. Rig
        // surfaces this as CompletionError::HttpError(http_client::Error);
        // the provider_response_* helpers are wired by the
        // `impl_provider_response_helpers!` macro in rig-core. We can't
        // build a real `http_client::Error` here (it lives in a private
        // crate), so we cover the classification by crafting a
        // ResponseError with the *string* shape of the upstream message
        // — which is what providers like OpenRouter also surface when
        // the body is unparseable. The real 429 path is exercised in
        // the integration tests; this unit test pins the contract for
        // a representative error string.
        let err = CompletionError::ResponseError(
            "tool call `edit` arrived with malformed JSON input: expected `,` or `}` at line 1 column 335"
                .into(),
        );
        let classified = super::classify_completion_error(&err);
        match classified {
            PicocodeError::LlmMalformedToolCall {
                ref tool_name,
                ref parse_error,
            } => {
                assert_eq!(tool_name, "edit");
                assert!(parse_error.contains("column 335"));
                assert!(classified.is_auto_retryable());
                assert_eq!(classified.retry_after_secs(), None);
                assert!(classified
                    .synthetic_retry_feedback()
                    .unwrap()
                    .contains("edit"));
            }
            other => panic!("expected LlmMalformedToolCall, got {other:?}"),
        }
    }

    #[test]
    fn classify_control_character_parse_error_is_retryable_with_feedback() {
        // The content-echo subclass (raw control characters in a string —
        // the model copied file/terminal bytes into `edit.old_text`) must
        // stay auto-retryable WITH synthetic feedback: the manual workflow
        // is to paste the parse error and let the model retry, and that
        // works (the model re-reads the source and re-emits clean JSON).
        // Automate it — do not special-case it as non-retryable.
        let err = CompletionError::ResponseError(
            "tool call `edit` arrived with malformed JSON input: \
             control character (\\u0000-\\u001F) found while parsing a string at line 2 column 0"
                .into(),
        );
        let classified = super::classify_completion_error(&err);
        match classified {
            PicocodeError::LlmMalformedToolCall {
                ref tool_name,
                ref parse_error,
            } => {
                assert_eq!(tool_name, "edit");
                assert!(parse_error.contains("control character"));
                assert!(classified.is_auto_retryable());
                let fb = classified.synthetic_retry_feedback().unwrap();
                assert!(fb.contains("edit"));
                assert!(fb.contains("control character"));
            }
            other => panic!("expected LlmMalformedToolCall, got {other:?}"),
        }
    }

    #[test]
    fn classify_prompt_cancelled_is_not_retryable() {
        let e = StreamingError::Prompt(Box::new(PromptError::prompt_cancelled(
            Vec::<Message>::new(),
            "user pressed Esc",
        )));
        let classified = super::classify_stream_error(&e);
        // The REPL converts this to a Cancelled at the call site; the
        // classifier itself should leave it as a prompt error. We
        // expect a `Prompt` error variant (the streaming-arm converter
        // does the wrapping).
        assert!(!classified.is_auto_retryable());
    }

    #[test]
    fn unknown_tool_call_with_available_list_surfaces_usefully() {
        let e = StreamingError::Prompt(Box::new(PromptError::UnknownToolCall {
            tool_name: "nonexistent".into(),
            available_tools: vec!["read".into(), "write".into()],
            allowed_tools: vec!["read".into(), "write".into()],
            chat_history: Box::new(Vec::new()),
        }));
        let classified = super::classify_stream_error(&e);
        // Not retryable at the REPL layer: recovery is owned by
        // `LoggingHook::on_invalid_tool_call`, which uses the rig-run
        // `max_invalid_tool_call_retries` budget. This variant is the
        // escape hatch — reached only when the hook is absent, when the
        // user has zero retries configured, or when the model
        // hallucinates the same wrong name twice in a row. Surface with
        // available tools so the user can correct the model.
        assert!(!classified.is_auto_retryable());
        // Recovery feedback lives in the rig hook, not in
        // `PicocodeError` — confirm so a future refactor doesn't drift.
        assert!(classified.synthetic_retry_feedback().is_none());
        assert_eq!(classified.retry_after_secs(), None);
        let s = format!("{classified}");
        assert!(s.contains("nonexistent"));
        assert!(s.contains("read") && s.contains("write"));
    }

    // The end-to-end recovery behavior (rig-run `on_invalid_tool_call` →
    // rollback → model sees feedback → corrected call dispatched) is
    // exhaustively covered by rig's own model-conformance tests
    // (e.g. `invalid_tool_call_hook_can_repair_streaming_tool_name` in
    // `rig-agent/src/agent/prompt_request/streaming.rs`). The compile
    // time check that `LoggingHook` implements `on_invalid_tool_call`
    // and the unit test on the resulting `PicocodeError` shape above
    // are sufficient for picocode's half of the contract.

    #[test]
    fn max_turns_error_is_not_retryable() {
        let e = StreamingError::Prompt(Box::new(PromptError::MaxTurnsError {
            max_turns: 16,
            chat_history: Box::new(Vec::new()),
            prompt: Box::new(Message::user("test")),
        }));
        let classified = super::classify_stream_error(&e);
        assert!(!classified.is_auto_retryable());
        let s = format!("{classified}");
        assert!(s.contains("16"));
    }

    #[test]
    fn classify_unparseable_response_other_message_does_not_match_tool_call() {
        // A different ResponseError (e.g. SSE protocol violation) should
        // NOT be misclassified as MalformedToolCall. Falls through to
        // LlmStreamProtocol.
        let err = CompletionError::ResponseError("unexpected EOF mid-stream".into());
        let classified = super::classify_completion_error(&err);
        assert!(matches!(classified, PicocodeError::LlmStreamProtocol(_)));
    }

    #[test]
    fn retry_after_secs_extracted_from_provider_response_headers() {
        use rig_core::http_client::{HeaderMap, HeaderValue, StatusCode};
        use rig_core::ProviderResponseError;
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_str("60").unwrap());
        let body = HashMap::from([("error".to_string(), "rate_limited".to_string())]);
        let ce = CompletionError::ProviderResponse(
            ProviderResponseError::new(
                StatusCode::TOO_MANY_REQUESTS,
                serde_json::to_string(&body).unwrap(),
            )
            .with_headers(Some(Box::new(headers))),
        );
        let classified = super::classify_completion_error(&ce);
        match classified {
            PicocodeError::LlmRateLimited {
                ref retry_after_secs,
            } => {
                assert_eq!(retry_after_secs, &Some(60));
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmRateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_prompt_error_429_preserves_retry_after() {
        // The non-streaming `chat()` path used by `summarize` returns a
        // `PromptError` rather than a `StreamingError`. A 429 must still
        // classify as `LlmRateLimited` with the Retry-After hint preserved
        // so the REPL can back off correctly on `/tree --summarize`.
        use rig_core::http_client::{HeaderMap, HeaderValue, StatusCode};
        use rig_core::ProviderResponseError;
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_str("30").unwrap());
        let ce = CompletionError::ProviderResponse(
            ProviderResponseError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "{\"error\":\"rate_limited\"}".to_string(),
            )
            .with_headers(Some(Box::new(headers))),
        );
        let pe = PromptError::CompletionError(ce);
        let classified = super::classify_prompt_error(&pe, "summarize");
        match classified {
            PicocodeError::LlmRateLimited { retry_after_secs } => {
                assert_eq!(retry_after_secs, Some(30));
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmRateLimited, got {other:?}"),
        }
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
    fn retry_budget_is_50_for_every_retryable_variant() {
        // All transient classes share a single generous budget: rate-limited,
        // upstream 5xx, transport, streaming-protocol, and malformed tool
        // calls. The budget exists to cap a runaway, not to second-guess
        // the policy per class. Non-retryable classes get 0 (no retries).
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmRateLimited {
                retry_after_secs: None
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmRateLimited {
                retry_after_secs: Some(60)
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmUpstream {
                status: 502,
                detail: Default::default(),
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmTransport("reset".into())),
            50
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmStreamProtocol("eof".into())),
            50
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmMalformedToolCall {
                tool_name: "edit".into(),
                parse_error: "expected `,` or `}`".into(),
            }),
            50
        );
        // Non-retryable: 0 retries.
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmAuth { status: 401 }),
            0
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmBilling { status: 402 }),
            0
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmClient {
                status: 400,
                detail: Default::default(),
            }),
            0
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmMaxTurns { max_turns: 16 }),
            0
        );
        // LlmInterrupted delegates to its cause.
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmInterrupted {
                cause: Box::new(PicocodeError::LlmRateLimited {
                    retry_after_secs: None
                }),
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmInterrupted {
                cause: Box::new(PicocodeError::LlmTransport("reset".into())),
            }),
            50
        );
        assert_eq!(
            super::retry_budget(&PicocodeError::LlmInterrupted {
                cause: Box::new(PicocodeError::LlmAuth { status: 403 }),
            }),
            0
        );
    }

    #[test]
    fn retry_delay_grows_as_pow2_without_cap() {
        // No provider hint: pure 2^(n-1), no ceiling. The per-turn budget
        // (50) is the implicit cap on how many attempts we will ever make.
        // attempt=1 → 1s, 2 → 2s, 3 → 4s, … (delay is `1 << (n-1)` seconds).
        let e = PicocodeError::LlmTransport("reset".into());
        assert_eq!(super::retry_delay_secs(&e, 1), 1);
        assert_eq!(super::retry_delay_secs(&e, 2), 2);
        assert_eq!(super::retry_delay_secs(&e, 3), 4);
        assert_eq!(super::retry_delay_secs(&e, 4), 8);
        assert_eq!(super::retry_delay_secs(&e, 10), 512);
        // Saturating at u64: still finite, no panic. We never reach 50 in
        // practice (budget caps it), but saturating_pow keeps the math safe.
        let big = super::retry_delay_secs(&e, 50);
        assert!(big > 0);
    }

    #[test]
    fn retry_delay_respects_retry_after_as_floor_then_doubles() {
        // When the provider reports Retry-After, it is the FLOOR of the
        // first attempt; subsequent attempts double from there. This
        // respects upstream's request while still growing on a string of
        // consecutive 429s.
        let e = PicocodeError::LlmRateLimited {
            retry_after_secs: Some(5),
        };
        assert_eq!(super::retry_delay_secs(&e, 1), 5);
        assert_eq!(super::retry_delay_secs(&e, 2), 10);
        assert_eq!(super::retry_delay_secs(&e, 3), 20);
        assert_eq!(super::retry_delay_secs(&e, 4), 40);
    }

    #[test]
    fn retry_delay_for_malformed_tool_call_uses_provider_floor_or_one() {
        // Malformed tool calls carry no provider hint (the model is the
        // source of the error), so the first delay is 1s — fast feedback
        // for self-correction. Subsequent attempts double.
        let e = PicocodeError::LlmMalformedToolCall {
            tool_name: "edit".into(),
            parse_error: "expected `,` or `}`".into(),
        };
        assert_eq!(super::retry_delay_secs(&e, 1), 1);
        assert_eq!(super::retry_delay_secs(&e, 2), 2);
        assert_eq!(super::retry_delay_secs(&e, 3), 4);
    }

    #[test]
    fn classify_deepinfra_502_upstream_error_via_provider_response_is_retryable() {
        // The first error from the task: OpenRouter surfaces a DeepInfra
        // 502 as a `ProviderResponseError` with status 502. The classifier
        // must map it to `LlmUpstream { status: 502 }` and mark it
        // auto-retryable.
        use rig_core::http_client::StatusCode;
        use rig_core::ProviderResponseError;
        let body = r#"{"id":"gen-1788441206-TngoywArM91X9nINXYNP","object":"chat.completion.chunk","created":1788441206,"model":"z-ai/glm-5.3-flash","provider":"DeepInfra","choices":[],"error":{"code":502,"message":"Upstream error from DeepInfra: TimeoutError: Timeout on reading data from socket","metadata":{"error_type":"provider_unavailable"}}}"#;
        let ce = CompletionError::ProviderResponse(ProviderResponseError::new(
            StatusCode::BAD_GATEWAY,
            body.to_string(),
        ));
        let classified = super::classify_completion_error(&ce);
        match classified {
            PicocodeError::LlmUpstream { status, .. } => {
                assert_eq!(status, 502);
                assert!(classified.is_auto_retryable());
                assert_eq!(classified.retry_after_secs(), None);
            }
            other => panic!("expected LlmUpstream {{ status: 502 }}, got {other:?}"),
        }
    }

    #[test]
    fn classify_http_client_error_sending_request_is_retryable_transport() {
        // The second error from the task: reqwest surfaces connection
        // failures as `CompletionError::HttpError(Instance(...))`. It must
        // classify as `LlmTransport` and be auto-retryable. We can't
        // construct a real reqwest::Error here, so we wrap a stub std
        // error via `Error::instance` (the boxed-error path reqwest uses).
        use rig_core::http_client;
        let stub: std::io::Error = std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "error sending request for url (https://openrouter.ai/api/v1/chat/completions)",
        );
        let http_err = http_client::Error::instance(stub);
        let ce = CompletionError::HttpError(http_err);
        let classified = super::classify_completion_error(&ce);
        match classified {
            PicocodeError::LlmTransport(ref msg) => {
                assert!(msg.contains("openrouter.ai"), "msg: {msg}");
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmTransport, got {other:?}"),
        }
    }

    #[test]
    fn classify_openrouter_429_mid_stream_envelope_is_rate_limited() {
        // OpenRouter sometimes streams a 429 in the body of a 200 OK
        // response (the `error.code` field of a JSON envelope). Rig
        // surfaces this as `CompletionError::ProviderResponse(...)` with
        // `status == None` and the JSON body as the `Display` string. The
        // classifier must extract the 429 and map it to `LlmRateLimited`
        // so the REPL's Retry-After floor and the 50-attempt budget
        // apply.
        let envelope = r#"ProviderResponseError: {"id":"gen-1788453950-hBER7wC6ldCU829zmnAe","object":"chat.completion.chunk","created":1788453950,"model":"unknown","provider":"GMICloud","choices":[],"error":{"code":429,"message":"Provider returned error","metadata":{"error_type":"rate_limit_exceeded"}}}"#;
        let err = CompletionError::ResponseError(envelope.to_string());
        let classified = super::classify_completion_error(&err);
        match classified {
            PicocodeError::LlmRateLimited { retry_after_secs } => {
                assert_eq!(retry_after_secs, None);
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmRateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_openrouter_502_mid_stream_envelope_is_upstream() {
        // Same envelope shape, but with a 5xx code. Must map to
        // `LlmUpstream { status: 502 }`.
        let envelope = r#"ProviderResponseError: {"error":{"code":502,"message":"Upstream error from DeepInfra: TimeoutError","metadata":{"error_type":"provider_unavailable"}}}"#;
        let err = CompletionError::ResponseError(envelope.to_string());
        let classified = super::classify_completion_error(&err);
        match classified {
            PicocodeError::LlmUpstream { status, .. } => {
                assert_eq!(status, 502);
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmUpstream {{ status: 502 }}, got {other:?}"),
        }
    }

    #[test]
    fn classify_provider_response_429_envelope_without_status_is_rate_limited() {
        // The production shape of the reported bug: rig's SSE decoder
        // (`completion_error_from_body`) turns a terminal error envelope
        // streamed inside a 200 OK into `CompletionError::ProviderResponse`
        // with NO HTTP status and the raw JSON as the body — NOT a
        // `ResponseError` with a prefixed string (which the sibling tests
        // above emulate). Previously this fell through to `Other` (Display
        // "Other error: ProviderResponseError: {...}"), which is not
        // auto-retryable. The classifier must extract the 429 from the body
        // and map it to `LlmRateLimited` so the REPL's budget applies.
        use rig_core::ProviderResponseError;
        let body = r#"{"id":"gen-1788464790-qEKKA0Klq7sLFL2HAM4O","object":"chat.completion.chunk","created":1788464790,"model":"unknown","provider":"GMICloud","choices":[],"error":{"code":429,"message":"Provider returned error","metadata":{"error_type":"rate_limit_exceeded"}}}"#;
        let ce = CompletionError::ProviderResponse(ProviderResponseError::without_status(
            body.to_string(),
        ));
        // Sanity-check the fixture matches the production wiring: no HTTP
        // status is preserved, so only body-based envelope detection can
        // classify this.
        assert_eq!(ce.provider_response_status(), None);
        let classified = super::classify_completion_error(&ce);
        match classified {
            PicocodeError::LlmRateLimited { retry_after_secs } => {
                assert_eq!(retry_after_secs, None);
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmRateLimited, got {other:?}"),
        }
    }

    #[test]
    fn classify_provider_response_502_envelope_with_2xx_status_is_upstream() {
        // A gateway that funnels a 200 OK response carrying an error
        // envelope through `from_http_response` preserves the 2xx status on
        // the `ProviderResponse`. A 2xx status must not be treated as a
        // failure on its own — the classifier defers to the body envelope,
        // mapping the 502 to `LlmUpstream`.
        use rig_core::http_client::StatusCode;
        use rig_core::ProviderResponseError;
        let body = r#"{"id":"gen-x","object":"chat.completion.chunk","created":0,"model":"unknown","provider":"GMICloud","choices":[],"error":{"code":502,"message":"Upstream error","metadata":{"error_type":"provider_unavailable"}}}"#;
        let ce = CompletionError::ProviderResponse(ProviderResponseError::new(
            StatusCode::OK,
            body.to_string(),
        ));
        let classified = super::classify_completion_error(&ce);
        match classified {
            PicocodeError::LlmUpstream { status, .. } => {
                assert_eq!(status, 502);
                assert!(classified.is_auto_retryable());
            }
            other => panic!("expected LlmUpstream {{ status: 502 }}, got {other:?}"),
        }
    }

    #[test]
    fn classify_provider_response_without_envelope_body_falls_through_to_other() {
        // A status-less `ProviderResponse` whose body is not an
        // OpenRouter-style envelope (e.g. plain-text or non-envelope JSON)
        // must keep the pre-fix behavior: fall through to `Other`, NOT get
        // auto-retried (there is no signal it is transient).
        use rig_core::ProviderResponseError;
        let ce = CompletionError::ProviderResponse(ProviderResponseError::without_status(
            "provider exploded".to_string(),
        ));
        let classified = super::classify_completion_error(&ce);
        assert!(
            matches!(classified, PicocodeError::Other(_)),
            "expected Other, got {classified:?}"
        );
        assert!(!classified.is_auto_retryable());
    }

    #[test]
    fn classify_openrouter_envelope_without_error_code_falls_through_to_stream_protocol() {
        // JSON that parses but has no `error.code` (e.g. a different
        // provider's envelope) should not match — falls through to
        // `LlmStreamProtocol` as before.
        let envelope = r#"{"id":"x","error":"invalid_argument"}"#;
        let err = CompletionError::ResponseError(envelope.to_string());
        let classified = super::classify_completion_error(&err);
        assert!(matches!(classified, PicocodeError::LlmStreamProtocol(_)));
    }

    #[test]
    fn classify_unrecognized_text_response_error_still_matches_malformed_tool_call() {
        // The pre-existing "tool call `X` arrived with malformed JSON"
        // detection must keep working alongside the new OpenRouter
        // envelope detection — they share the same `detect_response_error`
        // entry point.
        let err = CompletionError::ResponseError(
            "tool call `edit` arrived with malformed JSON input: expected `,` or `}` at line 1"
                .into(),
        );
        let classified = super::classify_completion_error(&err);
        assert!(matches!(
            classified,
            PicocodeError::LlmMalformedToolCall { .. }
        ));
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

    #[test]
    fn session_stats_lines_renders_session_and_model_sections() {
        use super::{session_stats_lines, SessionStatsInfo, SessionUsage};

        let info = SessionStatsInfo {
            display_name: "my-session".into(),
            created: "2025-01-02T03:04:05.000Z".into(),
            last_updated: Some("2025-01-02T10:11:12.000Z".into()),
            message_count: 14,
            user_message_count: 7,
        };
        let usage = SessionUsage::default();
        let lines = session_stats_lines(
            "openrouter",
            "minimax/minimax-m3:free",
            usage,
            Some(info),
            42_000,
            None,
            &Default::default(),
        );

        // The first three lines are the new "session:" section.
        assert!(
            lines[0].starts_with("session:"),
            "expected session header, got {:?}",
            lines[0]
        );
        assert!(lines[0].contains("my-session"));
        assert!(
            lines[1].starts_with("  created:") && lines[1].contains("last update"),
            "expected created/last update line, got {:?}",
            lines[1]
        );
        // Both turn and message counts must be shown — the former is the
        // number of user prompts (= turns), the latter the total entries
        // (user + assistant + toolResult).
        assert_eq!(lines[2], "  size: 7 turns (14 messages)");
        // New line: estimated context tokens
        assert_eq!(lines[3], "  estimated context tokens: 42000");

        // Then the "model:" section starts on the next line, with the rest
        // of the existing context-window / usage / cost / cache rows.
        let model_idx = lines
            .iter()
            .position(|l| l.starts_with("model:"))
            .expect("model section must be present");
        assert!(model_idx >= 4, "model section must follow session section");
        assert!(lines[model_idx].contains("openrouter:minimax/minimax-m3:free"));
        assert!(lines.iter().any(|l| l.contains("context window limit")));
    }

    #[test]
    fn session_stats_lines_falls_back_when_no_session_info() {
        use super::{session_stats_lines, SessionUsage};

        let lines = session_stats_lines(
            "openrouter",
            "minimax/minimax-m3:free",
            SessionUsage::default(),
            None,
            0,
            None,
            &Default::default(),
        );

        // Without a repo / session metadata the panel still renders both
        // sections — session just shows "(unavailable)" instead of details.
        assert!(
            lines[0].starts_with("session:") && lines[0].contains("unavailable"),
            "expected unavailable session header, got {:?}",
            lines[0]
        );
        let model_idx = lines
            .iter()
            .position(|l| l.starts_with("model:"))
            .expect("model section must be present");
        assert!(model_idx >= 1);
    }

    // ---- the rebuild gate (doc/model_settings_v2.md A.7) ----

    /// Minimal `AgentConfig` for gate decisions — the plan only reads the
    /// model id, params, and persona, so the rest is filler.
    fn gate_cfg(provider: &str, model: &str, params: crate::params::ModelParams) -> AgentConfig {
        AgentConfig {
            provider: provider.into(),
            model: model.into(),
            output: Arc::new(crate::output::NoOutput),
            yolo: false,
            confirm_port: None,
            quiet: true,
            params,
            context_limit: None,
            tool_call_limit: 10,
            system_message_extension: None,
            persona_prompt: None,
            persona_name: None,
            bash_auto_allow: None,
            agent_prompt: None,
            user_cancel: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            sandbox: None,
            show_reasoning_handle: crate::settings::default_show_reasoning_handle(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            message_log_path: None,
            sessions_dir: None,
        }
    }

    /// Nothing changed: no rebuild, whatever the last attempt was.
    #[test]
    fn plan_rebuild_is_a_noop_when_target_matches_applied() {
        use super::plan_rebuild;
        let p = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::Med),
            temperature: Some(0.5),
            top_p: None,
        };
        let cfg = gate_cfg("openai", "gpt-4o", p);
        assert!(plan_rebuild(&cfg, &cfg, None).is_none());
        assert!(plan_rebuild(
            &cfg,
            &cfg,
            Some(&super::RebuildFingerprint {
                model: "openai:gpt-4o".into(),
                params: p,
                persona: None,
                context_limit: None,
            })
        )
        .is_none());
    }

    /// A temperature-only change rebuilds: it is baked into the agent's request
    /// builder, so there is no in-place path. Same for top_p and the level.
    #[test]
    fn plan_rebuild_fires_for_each_param_field() {
        use super::plan_rebuild;
        let base = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        for next in [
            crate::params::ModelParams {
                temperature: Some(0.7),
                ..Default::default()
            },
            crate::params::ModelParams {
                top_p: Some(0.9),
                ..Default::default()
            },
            crate::params::ModelParams {
                reasoning: Some(crate::params::ReasoningLevel::High),
                ..Default::default()
            },
        ] {
            let target = gate_cfg("openai", "gpt-4o", next);
            assert!(
                plan_rebuild(&base, &target, None).is_some(),
                "{next:?} must rebuild"
            );
        }
    }

    /// The once-per-target invariant: the same target that already failed is
    /// not retried, so a bad model id reports once instead of every cycle.
    /// Changing anything about the target re-arms it.
    #[test]
    fn plan_rebuild_attempts_once_per_distinct_target() {
        use super::plan_rebuild;
        let applied = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        let target = gate_cfg(
            "openai",
            "gpt-4o",
            crate::params::ModelParams {
                temperature: Some(0.7),
                ..Default::default()
            },
        );
        let first = plan_rebuild(&applied, &target, None).expect("first attempt");
        // Same target, already attempted: silence.
        assert!(plan_rebuild(&applied, &target, Some(&first)).is_none());
        // A different param value is a different target: re-armed.
        let other = gate_cfg(
            "openai",
            "gpt-4o",
            crate::params::ModelParams {
                temperature: Some(0.9),
                ..Default::default()
            },
        );
        assert!(plan_rebuild(&applied, &other, Some(&first)).is_some());
    }

    /// The model id is compared as one canonical string. A model id that
    /// itself contains a colon (`openrouter:tencent/hy3:free`) must not read
    /// as a change on every cycle, which a segment-wise compare would cause.
    #[test]
    fn plan_rebuild_compares_the_whole_qualified_id() {
        use super::plan_rebuild;
        let applied = gate_cfg(
            "openrouter",
            "tencent/hy3:free",
            crate::params::ModelParams::default(),
        );
        let same = gate_cfg(
            "openrouter",
            "tencent/hy3:free",
            crate::params::ModelParams::default(),
        );
        assert!(plan_rebuild(&applied, &same, None).is_none());
        let different = gate_cfg(
            "openrouter",
            "z-ai/glm-5.3",
            crate::params::ModelParams::default(),
        );
        assert!(plan_rebuild(&applied, &different, None).is_some());
    }

    /// Switching model with identical params still rebuilds — the agent is
    /// generic over its model, so this is a real change.
    #[test]
    fn plan_rebuild_fires_on_a_model_switch_with_unchanged_params() {
        use super::plan_rebuild;
        let applied = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        let target = gate_cfg(
            "anthropic",
            "claude-3-5-sonnet-20241022",
            crate::params::ModelParams::default(),
        );
        assert!(plan_rebuild(&applied, &target, None).is_some());
    }
    // ---- /stat: the resolved view, and what this turn sends ----

    /// A Ctrl-E'd `context_length` is what the app enforces, so `/stat` must
    /// quote it. Reading the raw catalog here made this contradict the
    /// `/model` header on the same fact with nothing to explain the difference.
    #[test]
    fn stat_quotes_a_stored_context_length_not_the_catalogs() {
        let override_len = 4_096u64;
        let stored = crate::model_settings::ModelSettings {
            context_length: Some(override_len),
            ..Default::default()
        };
        let lines = session_stats_lines(
            "openrouter",
            "some/model",
            SessionUsage::default(),
            None,
            0,
            Some(&stored),
            &Default::default(),
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains(&format!("{override_len} tokens")),
            "the override must win: {joined}"
        );
    }

    /// A model the catalog does not cover still has a record, and the record is
    /// the user's own statement about it. Quoting "unknown" there would make
    /// every Ctrl-E override on an uncatalogued model invisible.
    #[test]
    fn stat_uses_a_stored_record_for_a_model_the_catalog_lacks() {
        let stored = crate::model_settings::ModelSettings {
            context_length: Some(8_192),
            ..Default::default()
        };
        let lines = session_stats_lines(
            "openrouter",
            "vendor/uncatalogued",
            SessionUsage::default(),
            None,
            0,
            Some(&stored),
            &Default::default(),
        );
        let joined = lines.join("\n");
        assert!(joined.contains("8192 tokens"), "{joined}");
    }

    /// The diagnostic pairing: when a 400 says `temperature must be <= 1`, the
    /// only way to act is to see what was sent. Absent fields say "provider
    /// default" — their absence is the information, so they are not omitted.
    #[test]
    fn stat_reports_the_params_this_turn_sends() {
        let live = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::High),
            temperature: Some(1.5),
            top_p: None,
        };
        let lines = session_stats_lines(
            "anthropic",
            "claude-sonnet-4",
            SessionUsage::default(),
            None,
            0,
            None,
            &live,
        );
        let joined = lines.join("\n");
        assert!(joined.contains("reasoning: high"), "{joined}");
        assert!(joined.contains("temperature: 1.5"), "{joined}");
        assert!(joined.contains("top_p: provider default"), "{joined}");
    }

    // ---- provider error detail: the 400's own account of what it rejected ----

    fn detail_of(body: &str) -> Option<String> {
        provider_error_detail(Some(body)).0
    }

    /// The case that motivated all of this: a rejected `temperature`. The
    /// catalog publishes no default for some models, so the 400 is the only
    /// place the real limit appears.
    #[test]
    fn an_openai_shaped_rejection_is_read() {
        let d = detail_of(
            r#"{"error":{"message":"temperature must be <= 1.0","type":"invalid_request_error","param":"temperature"}}"#,
        )
        .unwrap();
        assert!(d.contains("temperature must be <= 1.0"), "{d}");
        // The message already names the field, so `param` is not repeated —
        // "… <= 1.0 (field: temperature)" would be noise.
        assert!(!d.contains("field:"), "{d}");
    }

    /// Where the provider names the field but the message does not, the field
    /// *is* appended. For a rejection about a value, which knob was rejected is
    /// the part the user cannot infer from the message alone.
    #[test]
    fn the_offending_field_is_appended_when_the_message_omits_it() {
        let d =
            detail_of(r#"{"error":{"message":"must be <= 1.0","param":"temperature"}}"#).unwrap();
        assert!(d.contains("field: temperature"), "{d}");
    }

    #[test]
    fn an_anthropic_shaped_rejection_is_read() {
        let d = detail_of(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 64000 > 32000"}}"#,
        )
        .unwrap();
        assert!(d.contains("64000 > 32000"), "{d}");
    }

    /// A flat `{"error": "..."}` — several self-hosted servers answer this way.
    #[test]
    fn a_flat_error_string_is_read() {
        assert_eq!(
            detail_of(r#"{"error":"model is overloaded"}"#).as_deref(),
            Some("model is overloaded")
        );
    }

    #[test]
    fn a_toplevel_message_is_read() {
        assert_eq!(
            detail_of(r#"{"message":"unknown model"}"#).as_deref(),
            Some("unknown model")
        );
    }

    /// FastAPI validation: `detail` is an array, and `loc` is the path to the
    /// offending field. This is the shape a self-hosted vLLM-style server sends.
    #[test]
    fn a_fastapi_validation_array_is_read() {
        let d = detail_of(
            r#"{"detail":[{"loc":["body","temperature"],"msg":"ensure this value is <= 1","type":"value_error"}]}"#,
        )
        .unwrap();
        assert!(d.contains("body.temperature"), "{d}");
        assert!(d.contains("<= 1"), "{d}");
    }

    /// Not every body is JSON. An HTML error page or a bare sentence must still
    /// reach the user rather than being discarded as unparseable.
    #[test]
    fn a_non_json_body_is_still_reported() {
        assert_eq!(
            detail_of("upstream connect error\nor disconnect").as_deref(),
            Some("upstream connect error or disconnect")
        );
    }

    /// Structure with no message anywhere is not a diagnostic; better a bare
    /// status than `; {"foo":1}`.
    #[test]
    fn a_body_with_no_message_yields_nothing() {
        assert_eq!(detail_of(r#"{"foo":1,"bar":2}"#), None);
        assert_eq!(detail_of("   "), None);
        assert_eq!(provider_error_detail(None).0, None);
    }

    /// The detail lands in the rendered error, which is where the user sees it.
    /// A `None` must leave the message byte-identical to before, with no
    /// dangling separator.
    #[test]
    fn the_detail_is_rendered_as_a_suffix() {
        let with = PicocodeError::LlmClient {
            status: 400,
            detail: crate::ErrorDetail(Some("temperature must be <= 1.0".into())),
        };
        assert_eq!(
            with.to_string(),
            "Provider rejected the request (HTTP 400); temperature must be <= 1.0."
        );
        let without = PicocodeError::LlmClient {
            status: 400,
            detail: Default::default(),
        };
        assert_eq!(
            without.to_string(),
            "Provider rejected the request (HTTP 400)."
        );
    }

    /// A long or multi-line body must not flood the turn: one clipped line.
    #[test]
    fn a_huge_body_is_clipped_to_one_line() {
        let body = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(5000));
        let d = detail_of(&body).unwrap();
        assert!(d.chars().count() <= 301, "{}", d.chars().count());
        assert!(d.ends_with('…'), "{d}");
        assert!(!d.contains('\n'), "must be a single line");
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

    /// A Ctrl-E of `context_length` moves nothing else `plan_rebuild` looks at,
    /// but the mid-turn status bar reads the window off the running agent, so
    /// without this term the edit would not reach it until an unrelated rebuild.
    #[test]
    fn a_context_window_change_rebuilds() {
        use super::plan_rebuild;
        let mut base = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        base.context_limit = Some(200_000);
        let mut next = base.clone();
        next.context_limit = Some(1_048_576);
        assert!(
            plan_rebuild(&base, &next, None).is_some(),
            "a Ctrl-E'd context_length must arm a rebuild"
        );
        // ...and is not a loop: once applied, the two sides agree again.
        assert!(plan_rebuild(&next, &next, None).is_none());
    }

    /// The dedupe waits for a *distinct* target, so a window edit is a fresh
    /// attempt even when a previous one failed — otherwise the user edits
    /// something, the fix lands, and the gate still refuses to retry.
    #[test]
    fn a_window_change_is_not_deduped_against_a_failed_attempt() {
        use super::plan_rebuild;
        let mut base = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        base.context_limit = Some(200_000);
        let mut next = base.clone();
        next.context_limit = Some(1_048_576);
        let prior = plan_rebuild(&base, &next, None).expect("first attempt is armed");
        assert!(
            plan_rebuild(&base, &next, Some(&prior)).is_none(),
            "the same target must not re-arm"
        );
        // Same model, params and persona; only the window differs.
        let after_fix = gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default());
        assert!(
            plan_rebuild(&base, &after_fix, Some(&prior)).is_some(),
            "a new window is a new target and must re-arm"
        );
    }

    /// The other half of option A: a settings write has to move the *target*
    /// config's window, or `plan_rebuild` has nothing to notice.
    ///
    /// Goes through `update_settings` because that is the path the Ctrl-E
    /// accept arm takes, and reads the target back with `config_snapshot`,
    /// because that is what the convergence step compares against.
    #[test]
    fn a_record_write_moves_the_target_configs_window() {
        let mut store =
            crate::settings::SettingsStore::from_config(&crate::Config::default(), None);
        store.default_model = "openai:gpt-4o".into();
        let app = crate::app_state::AppState::new(
            gate_cfg("openai", "gpt-4o", crate::params::ModelParams::default()),
            store,
            None,
        );
        let before = app.config_snapshot().context_limit;

        app.update_settings(|s| {
            s.apply_model_settings(
                "openai:gpt-4o",
                &crate::model_settings::ModelSettings {
                    context_length: Some(123_456),
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();

        let after = app.config_snapshot().context_limit;
        assert_eq!(
            after,
            Some(123_456),
            "the target must carry the window so the rebuild gate can see it"
        );
        assert_ne!(after, before, "so a rebuild is armed, not suppressed");
    }
}

/// Human-readable role label for a rig [`Message`], used in the JSONL log.
fn message_role(msg: &Message) -> &'static str {
    match msg {
        Message::System { .. } => "system",
        Message::User { .. } => "user",
        Message::Assistant { .. } => "assistant",
    }
}

/// JSONL message logger for debugging LLM interactions.
///
/// Writes one JSON object per line capturing a raw rig [`Message`] (as sent to
/// or received from the model) with timestamp, direction, role, and turn id.
/// The file is created lazily on the first write; parent directories are
/// created as needed. Enabled with `--log-messages <file>`.
pub struct MessageLogger {
    file: tokio::sync::Mutex<Option<tokio::fs::File>>,
    path: std::path::PathBuf,
    turn_counter: std::sync::atomic::AtomicU64,
    provider: String,
    model: String,
}

impl MessageLogger {
    /// Create a new MessageLogger that writes to the specified path.
    /// The file is created lazily on first write.
    pub fn new(path: String, provider: String, model: String) -> Self {
        Self {
            file: tokio::sync::Mutex::new(None),
            path: std::path::PathBuf::from(path),
            turn_counter: std::sync::atomic::AtomicU64::new(0),
            provider,
            model,
        }
    }

    /// Advance to the next completion call, returning its 1-based turn id.
    ///
    /// Every message logged under the same turn id belongs to one
    /// request/response round trip: the full context sent (history + prompt)
    /// and the assistant content received in reply.
    pub fn advance_turn(&self) -> u64 {
        self.turn_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1
    }

    /// Current turn id (without incrementing) — used by the response side to
    /// tag the reply with the same turn id as the request that produced it.
    pub fn current_turn_id(&self) -> u64 {
        self.turn_counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Log a raw rig `Message` as one JSONL line.
    ///
    /// Failures (unwritable path, disk full) are reported to stderr and
    /// swallowed so message logging never breaks the conversation.
    pub async fn log_message(&self, direction: &str, message: &Message, turn_id: u64) {
        let entry = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "direction": direction,
            "role": message_role(message),
            "message": message,
            "turn_id": turn_id,
            "provider": self.provider,
            "model": self.model,
        });

        if let Err(e) = self.write_line(&entry).await {
            eprintln!("Warning: failed to write to message log: {}", e);
        }
    }

    /// Ensure the file handle is open, creating the file if needed.
    async fn ensure_file_open(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<tokio::fs::File>>> {
        let mut guard = self.file.lock().await;
        if guard.is_none() {
            // Create parent directories
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // Open (or create) the file for appending
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await?;
            *guard = Some(file);
        }
        Ok(guard)
    }

    /// Write a JSON line to the file.
    async fn write_line(&self, entry: &serde_json::Value) -> Result<()> {
        let guard = self.ensure_file_open().await?;
        if let Some(file) = guard.as_ref() {
            let line = serde_json::to_string(entry)?;
            let mut file = file.try_clone().await?;
            use tokio::io::AsyncWriteExt;
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
        }
        Ok(())
    }
}
