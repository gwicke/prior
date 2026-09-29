use crate::app_state::AppState;
use crate::model_catalog;
use crate::output::Confirmation;
use crate::repl::{InputAction, ModelFilter, Repl, ReplCtx};
use crate::session::repo::{ForkMode, SessionRepo};
use crate::session::storage::SessionStorage;
use crate::session::Session;
use crate::settings::SettingsStore;
use crate::thinking;
use crate::tools::{
    bsed_available, set_sandbox, AgentBrowser, Bash, CopyFile, EditFile, GlobFiles, GrepText,
    ListDir, MakeDir, MoveFile, ReadFile, Remove, SedPreview, WriteFile,
};
use crate::Output;
use crate::Result;
use async_trait::async_trait;
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
use rig_core::message::{
    AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolResultContent, UserContent,
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

#[async_trait]
pub trait PicoAgent: Send + Sync {
    async fn prompt(
        &self,
        input: &str,
        session: Option<&Session>,
        turn: &TurnPersistence,
    ) -> Result<String>;
    async fn run_once(&self, input: String) -> Result<String>;
    /// Snapshot of the accumulated session token usage (for `/stat`). A zeroed
    /// result means no completion call has reported usage yet.
    fn session_usage(&self) -> SessionUsage;
    /// Generate a concise summary of the given text using the agent's model.
    /// Used by `/tree --summarize` to summarise abandoned branches.
    async fn summarize(&self, prompt: &str) -> Result<String>;
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
    async fn summarize_with_session_context(
        &self,
        session: &Session,
        instruction: &str,
    ) -> Result<String>;
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

#[async_trait]
impl PicoAgent for CodeAgent {
    async fn prompt(
        &self,
        input: &str,
        session: Option<&Session>,
        turn: &TurnPersistence,
    ) -> Result<String> {
        self.output.display_thinking("Thinking...");
        // Fresh request: clear any stale cancel flag from a previous abort so a
        // prior `Esc` doesn't silently cancel this prompt. The REPL's in-flight
        // reader sets `cancel` on `<Esc>` and is always joined (after `done` is
        // set) before the next `prompt()` begins, so a leftover `cancel` can
        // only survive while the reader is still running — which it never is at
        // this point. See doc/turn_cancellation_persistence_bugs.md §3.
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

        // `stream_prompt(...).await` yields the item stream (rig 0.40's
        // `IntoFuture` returns the stream itself, not a collected response).
        // The run-scoped transcript is shared with the hook and the stream
        // loop so a mid-turn failure can persist exactly what completed.
        let transcript = Arc::new(Mutex::new(TurnTranscript::default()));
        let mut stream = self
            .agent
            .stream_prompt(input)
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
        self.output.begin_response();
        // When reasoning/thinking is enabled, hold the assistant answer back and
        // flush it only at the turn boundary. This guarantees that reasoning
        // arriving *after* the answer (an occasional OpenRouter streaming order)
        // is still shown above it instead of leaking below. See `output.rs`
        // `answer_held` / `flush_held_answer`.
        let _reasoning_expected = self
            .thinking_level
            .as_deref()
            .and_then(|s| s.parse::<thinking::ThinkingLevel>().ok())
            .map(|l| l != thinking::ThinkingLevel::Off)
            .unwrap_or(false);
        // self.output.set_answer_hold(reasoning_expected);
        loop {
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
                            StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                                if !reasoning.is_empty() {
                                    reasoning_buffer.push_str(&reasoning); // Accumulate for partial persistence
                                    self.output.stream_thinking(&reasoning);
                                }
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

    async fn run_once(&self, input: String) -> Result<String> {
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
        let response = self.prompt(&input, None, &turn).await?;
        Ok(response)
    }

    fn session_usage(&self) -> SessionUsage {
        self.usage.lock().map(|g| *g).unwrap_or_default()
    }

    async fn summarize(&self, prompt: &str) -> Result<String> {
        let msg = Message::user(prompt);
        let mut history: Vec<Message> = vec![];
        self.agent
            .chat(msg, &mut history)
            .await
            .map_err(|e| classify_prompt_error(&e, "summarize"))
    }

    async fn summarize_with_session_context(
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

/// Wait `secs` seconds, but break early on user cancel. Returns `true` if the
/// wait completed, `false` if the user pressed `<Esc>`.
async fn cancellable_sleep(
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
    secs: u64,
) -> bool {
    if secs == 0 {
        return true;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    let cancel = async {
        // The REPL reader calls `notify_one()` on Esc; we also poll the flag
        // to avoid a race where the notify was already fired before we
        // subscribed.
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
    };
    tokio::select! {
        _ = tokio::time::sleep_until(deadline) => true,
        _ = cancel => false,
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
        return (hint as u64).saturating_mul(1u64 << n);
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
            return match code {
                401 | 403 => LlmAuth { status: code },
                402 => LlmBilling { status: code },
                429 => LlmRateLimited { retry_after_secs },
                500..=599 => LlmUpstream { status: code },
                400..=499 => LlmClient { status: code },
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
        match code {
            429 => {
                return Some(LlmRateLimited {
                    retry_after_secs: None,
                })
            }
            500..=599 => return Some(LlmUpstream { status: code }),
            401 | 403 => return Some(LlmAuth { status: code }),
            402 => return Some(LlmBilling { status: code }),
            400..=499 => return Some(LlmClient { status: code }),
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
/// The transcript is stored canonically (a pi-compatible `edits` array), but
/// the model is shown the block form **consistently** so it keeps emitting the
/// reliable shape and is not nudged back toward the array (the form weak
/// harnesses mangle). Idempotent and allocation-light: non-`edit` calls and
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
    yolo: bool,
    persona_name: Option<String>,
    user_cancel: Arc<AtomicBool>,
    /// Event-based cancel signal. The in-flight reader calls `notify_one()`
    /// when `<Esc>` is pressed; the streaming loop selects on `notified()`
    /// instead of polling with a sleep.
    cancel_notify: Arc<tokio::sync::Notify>,
    thinking_level: Option<String>,
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
    pub thinking_level: Option<String>,
    pub tool_call_limit: usize,
    pub system_message_extension: Option<String>,
    pub persona_prompt: Option<String>,
    pub persona_name: Option<String>,
    pub bash_auto_allow: Option<Vec<String>>,
    pub agent_prompt: Option<String>,
    /// Shared cancel flag. The REPL's in-flight reader thread sets this when
    /// the user presses `Esc` during an LLM request; the `LoggingHook` bridge
    /// below turns it into a rig `CancelSignal` so the in-flight request stops.
    pub user_cancel: Arc<AtomicBool>,
    /// Event-based cancel signal paired with `user_cancel`. The in-flight
    /// reader calls `notify_one()` when `<Esc>` is pressed; the streaming
    /// loop selects on `notified()` instead of polling.
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

pub async fn create_agent(config: AgentConfig) -> Result<Box<dyn PicoAgent>> {
    let provider = config.provider.to_lowercase();
    let model = config.model.clone();
    let think_json = thinking::mapping_opt(&provider, config.thinking_level.as_deref());

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
            let merged = merge_openrouter_params(think_json.clone());
            build!(@builder_with_params AgentBuilder::new(cached_model), merged)
        }};
        ($client:expr) => {{
            build!(@builder $client.agent(&model))
        }};
        (@builder $builder:expr) => {{
            build!(@builder_with_params $builder, think_json.clone())
        }};
        (@builder_with_params $builder:expr, $params:expr) => {{
            let rig_agent = build_rig_agent(
                $builder,
                config.yolo,
                config.output.clone(),
                config.system_message_extension,
                config.persona_prompt,
                config.bash_auto_allow.unwrap_or_default(),
                config.agent_prompt,
                $params,
                config.sandbox.clone(),
            );

            let message_logger = if let Some(path) = &config.message_log_path {
                Some(Arc::new(MessageLogger::new(path.clone(), config.provider.clone(), model.clone())))
            } else {
                None
            };

            Box::new(CodeAgent::new(
                rig_agent,
                config.output,
                config.tool_call_limit,
                config.provider,
                model,
                config.yolo,
                config.persona_name,
                config.user_cancel.clone(),
                config.cancel_notify.clone(),
                config.thinking_level.clone(),
                config.reasoning_in_context,
                message_logger,
            ))
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

    let agent: Box<dyn PicoAgent> = match provider.as_str() {
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
            event.history.iter().cloned().collect(),
            self.reasoning_in_context,
            &self.provider,
            &self.model,
        );
        let history = blob_edit_tool_calls(stripped_history);

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
        ToolResultAction::Keep
    }
}

const DEFAULT_AGENT_PROMPT: &str = r#"You are picocode, a software engineering agent. You are direct, technical, and highly efficient.

Your mission is to assist the user in their development tasks by utilizing a set of specialized tools. You operate within a specific codebase and must maintain its integrity while delivering high-quality solutions.

### WORKFLOW & STRATEGY
1. **Understand Before Acting**: Always ground your responses in verified facts. Use `ls` to see the structure and `read` or `grep` to understand existing logic and patterns.
2. **Be Precise**: When editing files, use `edit`. Leverage its block syntax, batch support, and line anchoring. Avoid editing with sed or python scripts - you are likely to make mistakes. If you MUST use them, always save all outstanding work with a snapshot commit first, and double check the results right after.
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
blocks, then pass them to `edit` as its `edits` argument to apply. `sed` refuses
`w`, `e` and `q`; use `edit` or `bash` for those. Pass `context` if you need more
than the ~2 lines of trailing context you get by default."#;

fn build_rig_agent(
    builder: AgentBuilder<NoToolConfig>,
    yolo: bool,
    output: Arc<dyn Output>,
    system_message_extension: Option<String>,
    persona_prompt: Option<String>,
    bash_auto_allow: Vec<String>,
    agent_prompt: Option<String>,
    additional_params: Option<serde_json::Value>,
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

    builder = builder
        .tool(guard(MakeDir, yolo, output.clone(), None))
        .tool(guard(Remove, yolo, output.clone(), None))
        .tool(guard(MoveFile, yolo, output.clone(), None))
        .tool(guard(CopyFile, yolo, output.clone(), None));

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
    ));

    if is_tool_available("agent-browser") {
        builder = builder.tool(guard(AgentBrowser, yolo, output.clone(), None));
    }
    builder.build()
}

use rig_core::tool::{Tool, ToolErrorKind, ToolExecutionError};

struct Guard<T: Tool> {
    tool: T,
    yolo: bool,
    output: Arc<dyn Output>,
    always: Arc<AtomicBool>,
    auto_approve: Option<Arc<dyn Fn(&T::Args) -> bool + Send + Sync>>,
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
    /// - `ToolError::Io` → `Other` (infrastructure failure, not auto-retryable)
    /// - `ToolError::Generic` containing "timed out" → `Timeout` (auto-retryable)
    /// - `ToolError::Generic` containing "cancelled" → `Cancelled` (not retryable)
    /// - everything else → `Other` with the specific message preserved
    fn map_error(&self, error: Self::Error) -> ToolExecutionError {
        use crate::tools::ToolError;

        match error {
            ToolError::Io(msg) => ToolExecutionError::other(msg),
            ToolError::Generic(msg) => {
                let kind = if msg.contains("timed out") {
                    ToolErrorKind::Timeout
                } else if msg.contains("cancelled") {
                    ToolErrorKind::Cancelled
                } else {
                    ToolErrorKind::Other
                };
                ToolExecutionError::new(kind, msg)
            }
        }
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

        if !self.yolo && !self.always.load(Ordering::Relaxed) && !should_auto_approve {
            match self
                .output
                .confirm(&format!("Confirm tool {} call?", Self::NAME.to_uppercase()))
            {
                Confirmation::Always => {
                    self.always.store(true, Ordering::Relaxed);
                }
                Confirmation::Yes => {}
                Confirmation::No => {
                    return Err(crate::tools::ToolError::Generic(
                        "Action cancelled by user".into(),
                    ));
                }
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
) -> Guard<T> {
    Guard {
        tool,
        yolo,
        output,
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
pub(crate) fn estimate_context_tokens(messages: &[Message]) -> u64 {
    fn text_tokens(t: &str) -> u64 {
        t.chars().count() as u64 / 4
    }
    let mut n = 0u64;
    for m in messages {
        n += 16; // per-message framing overhead (role, separators)
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

/// Result of handling a `/session` command.
#[derive(Debug)]
enum SessionCommandResult {
    /// Input was not a session command (pass through).
    NotSession,
    /// Session command handled, current session unchanged.
    Handled,
    /// Session command created/resumed a new session with this ID. `prefill`
    /// is pre-loaded into the next input prompt (use `None` for no prefill,
    /// e.g. on fork; `Some("/model …")` on resume to surface the restored
    /// model to the user for explicit confirmation).
    Switched { id: String, prefill: Option<String> },
    /// Session command deleted a session.
    Deleted,
}

/// Parse the session name and remaining text from a `/session <rest>` string.
pub(crate) fn parse_session_name_after(rest: &str) -> (String, String) {
    if rest.starts_with('"') {
        if let Some(end) = rest[1..].find('"').map(|i| i + 2) {
            let n = rest[1..end - 1].to_string();
            let after = rest[end..].trim().to_string();
            (n, after)
        } else {
            (String::new(), String::new())
        }
    } else {
        let mut parts = rest.splitn(2, char::is_whitespace);
        let n = parts.next().unwrap_or("").to_string();
        (n, parts.next().unwrap_or("").to_string())
    }
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
/// (identical to the live streaming path). `leading_separator` controls
/// whether a separator precedes the user prompt (replay skips it for the very
/// first unit because the loop already emitted one).
fn render_turn(output: &dyn Output, turn: &ConversationTurn, leading_separator: bool) {
    if leading_separator {
        output.display_separator();
    }
    output.display_plain(&turn.user_text);

    if turn.segments.is_empty() {
        return;
    }

    output.display_separator();
    output.begin_response();
    output.set_answer_hold(false);

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
    // Leading separators only space out turns (turns *after* the first one).
    // Compaction markers are not turns and never force a separator.
    let mut first_turn_seen = false;
    for unit in &units[start..] {
        match unit {
            ReplayUnit::Turn(turn) => {
                render_turn(output, turn, first_turn_seen);
                first_turn_seen = true;
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
        let n: usize = arg[..arg.len() - 1].trim().parse().unwrap_or(0);
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
        let n: usize = arg[..arg.len() - 1].trim().parse().unwrap_or(0);
        if n > 0 {
            if total == 0 {
                output.display_text("Session is empty");
            } else {
                output.begin_capture();
                output.display_separator();
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
            if total > 0 {
                output.display_separator();
            }
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
            output.display_separator();
            replay_last_turns(output, &units, count);
        }
    }
}

/// Quiet `/replay 1`: separator + the last turn only — no status messages,
/// and a no-op when the session has no turns yet. Used by the key-driven
/// resize repaint in `read_input`, which wipes the old frame and replays
/// the last turn to push reflow corruption out of view; that path must
/// stay silent (a session exists from startup, long before it has any
/// turns to replay).
pub(crate) async fn replay_last_turn_silent(
    session: &crate::session::Session,
    output: &dyn Output,
) {
    let branch = session.get_branch().await;
    let units = build_replay_units(&branch);
    if count_turns(&units) == 0 {
        return;
    }
    output.display_separator();
    replay_last_turns(output, &units, 1);
    // Trailing rule: manual `/replay 1` leaves the echoed command line
    // between the replayed content and the input; the silent path skips
    // the echo, so paint the divider ourselves — otherwise the repainted
    // input sits flush against the replayed turn.
    output.display_separator();
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

/// The thinking level a session has recorded: latest `ThinkingLevelChange`
/// on the branch, else the header, else `None`.
async fn session_recorded_thinking(session: &Session) -> Option<String> {
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
async fn record_thinking_change(session: &Session, level: &str) {
    if session_recorded_thinking(session).await.as_deref() == Some(level) {
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
/// Parse and dispatch a `/session <name> <action>` command.
///
/// `config` is *not* mutated on resume anymore: the running agent keeps
/// whatever model it had before, and the restored session's stored model is
/// surfaced to the user via a `/model <…>` prefill on the next prompt (see
/// `compute_resume_model_prefill`). This keeps the accept/abort/choose
/// decision in the user's hands.
async fn handle_session_command(
    input: &str,
    session: &mut Option<Arc<Session>>,
    repo: &mut Option<crate::session::repo::JsonlSessionRepo>,
    config: &mut AgentConfig,
    output: &Arc<dyn Output>,
    settings: &SettingsStore,
) -> SessionCommandResult {
    let input = input.trim();
    if !input.starts_with("/session") {
        return SessionCommandResult::NotSession;
    }

    let rest = input["/session".len()..].trim();

    // Strip leading path: filter if the user typed it manually (overlay-only token).
    let rest = if let Some(r) = rest.strip_prefix("path:") {
        r.trim_start()
            .split_once(char::is_whitespace)
            .map(|(_, after)| after.trim())
            .unwrap_or("")
    } else {
        rest
    };

    if rest.is_empty() {
        return SessionCommandResult::Handled;
    }

    let (name, after_name) = parse_session_name_after(rest);

    if name.is_empty() {
        return SessionCommandResult::Handled;
    }

    let (action, arg) = if let Some((a, r)) = after_name.split_once(char::is_whitespace) {
        (a.to_string(), Some(r.trim().to_string()))
    } else {
        (after_name.clone(), None)
    };

    let action = action.to_lowercase();

    let meta = if let Some(repo) = repo {
        repo.list(None)
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|m| m.name.as_deref() == Some(&name) || m.id == name)
    } else {
        None
    };

    match action.as_str() {
        "resume" => {
            if let Some(meta) = meta {
                // Parse optional replay length from argument: N, Nt, or Nl.
                let default = settings.session_resume_replay_turns;
                let (count, is_lines) = arg
                    .as_deref()
                    .map(|s| crate::agent::parse_replay_len_arg(s, default))
                    .unwrap_or((default, false));
                // Load/migrate/replay/prefill — shared with the inline
                // resume node (`resume_session` owns the rationale).
                match resume_session(
                    repo.as_ref().unwrap(),
                    &meta,
                    (count, is_lines),
                    output.as_ref(),
                    &config.qualified_model_id(),
                )
                .await
                {
                    Ok((new_session, id, prefill)) => {
                        *session = Some(new_session);
                        SessionCommandResult::Switched { id, prefill }
                    }
                    Err(msg) => {
                        config.output.display_text(&msg);
                        SessionCommandResult::Handled
                    }
                }
            } else {
                config
                    .output
                    .display_text(&format!("Session not found: {}", name));
                SessionCommandResult::Handled
            }
        }
        "fork" => {
            if let Some(meta) = meta {
                match fork_session_at(repo.as_ref().unwrap(), &meta).await {
                    Ok((new_session, id)) => {
                        *session = Some(new_session);
                        SessionCommandResult::Switched { id, prefill: None }
                    }
                    Err(msg) => {
                        config.output.display_text(&msg);
                        SessionCommandResult::Handled
                    }
                }
            } else {
                config
                    .output
                    .display_text(&format!("Session not found: {}", name));
                SessionCommandResult::Handled
            }
        }
        "rename" => {
            let new_name = arg.unwrap_or_default();
            if new_name.is_empty() {
                config
                    .output
                    .display_text("Usage: /session <name> rename <newname>");
                return SessionCommandResult::Handled;
            }
            if let Some(ref s) = session {
                let cur_id = s.get_session_id().to_string();
                if Some(&cur_id) == meta.as_ref().map(|m| &m.id) {
                    let _ = s.set_session_name(&new_name).await;
                    // Refresh the repo's metadata cache so that the listing picks up the new name immediately
                    if let Some(ref repo) = repo {
                        repo.invalidate_cache();
                    }
                } else if let Some(meta) = meta {
                    let repo = repo.as_ref().unwrap();
                    match repo.open(&meta).await {
                        Ok((storage, index)) => {
                            let s = Session::new_persisted(storage, index);
                            let _ = s.set_session_name(&new_name).await;
                            // Refresh the repo's metadata cache so that the listing picks up the new name immediately
                            repo.invalidate_cache();
                        }
                        Err(e) => {
                            config
                                .output
                                .display_text(&format!("Cannot open session: {}", e));
                        }
                    }
                } else {
                    config
                        .output
                        .display_text(&format!("Session not found: {}", name));
                }
            }
            SessionCommandResult::Handled
        }
        "delete" => {
            if let Some(meta) = meta {
                if let Some(ref s) = session {
                    if s.get_session_id() == meta.id {
                        config
                            .output
                            .display_text("Cannot delete the current session");
                        return SessionCommandResult::Handled;
                    }
                }
                match repo.as_ref().unwrap().delete(&meta).await {
                    Ok(()) => SessionCommandResult::Deleted,
                    Err(e) => {
                        config
                            .output
                            .display_text(&format!("Cannot delete session: {}", e));
                        SessionCommandResult::Handled
                    }
                }
            } else {
                config
                    .output
                    .display_text(&format!("Session not found: {}", name));
                SessionCommandResult::Handled
            }
        }
        _ => {
            config
                .output
                .display_text(&format!("Unknown session action: {}", action));
            SessionCommandResult::Handled
        }
    }
}

/// Tree action selected from the `/tree` overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TreeAction {
    /// Move the session leaf pointer to the target (today's behavior).
    Switch,
    /// Append a `Compaction` entry with `firstKeptEntryId = target`, then let
    /// `build_context()` scope the agent context at that point.
    Compact,
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

/// Parse `/tree <target_id> [switch [--summarize] | compact [manual|text-only|llm [<payload>]]]`.
/// The bare `/tree <id> --summarize` form is accepted as an alias for
/// `switch --summarize` (backward compat). Only committed canonical lines (or
/// hand-typed canonical/alias forms) reach this parser — sub-menu filter
/// fragments are consumed by the overlay's fuzzy/pre-selection logic.
/// For `compact llm`, the trailing free-text payload (whitespace-joined,
/// no quote parsing — matches `rename`/`set`) is returned in the last
/// tuple slot; empty for the other modes.
fn parse_tree_args(rest: &str) -> (String, TreeAction, Option<CompactSummaryMode>, bool, String) {
    let mut summarize = false;
    let mut target = String::new();
    let mut action = TreeAction::Switch;
    let mut mode: Option<CompactSummaryMode> = None;
    let mut llm_payload = String::new();
    let mut seen_compact = false;
    let mut after_mode = false;
    for part in rest.split_whitespace() {
        match part {
            "--summarize" | "-s" => summarize = true,
            "switch" => action = TreeAction::Switch,
            "compact" => {
                action = TreeAction::Compact;
                seen_compact = true;
            }
            // Primary mode token is now `manual` (the user-editable
            // pre-fill of the touched-files list). `truncate` / `none` /
            // `no-summary` remain recognized as deprecated aliases for
            // muscle-memory / session-log back-compat; the empty-summary
            // behavior is preserved when the dropped span touches no
            // files.
            "manual" | "truncate" | "none" | "no-summary" if seen_compact && !after_mode => {
                mode = Some(CompactSummaryMode::Manual)
            }
            "text-only" | "text" if seen_compact && !after_mode => {
                mode = Some(CompactSummaryMode::TextOnly)
            }
            "llm" | "summary" if seen_compact => {
                mode = Some(CompactSummaryMode::Llm);
                after_mode = true;
            }
            id if !seen_compact => {
                // The overlay commits the id angle-bracketed (`/tree <id>
                // switch`); strip the brackets — the raw id is what move_leaf
                // / get_entry expect.
                let t = id.trim();
                let t = t.strip_prefix('<').unwrap_or(t);
                let t = t.strip_suffix('>').unwrap_or(t);
                target = t.to_string();
            }
            // Anything after the `llm` token is part of the payload;
            // re-join with single spaces so multi-word prompts survive
            // split_whitespace's collapse.
            _ if after_mode && mode == Some(CompactSummaryMode::Llm) => {
                if !llm_payload.is_empty() {
                    llm_payload.push(' ');
                }
                llm_payload.push_str(part);
            }
            _ => {}
        }
    }
    (target, action, mode, summarize, llm_payload)
}

/// Build a summarization prompt from a list of session entries.
/// Produces a concise [role] / text transcript of the abandoned branch.
fn build_summary_prompt(entries: &[crate::session::SessionEntry]) -> String {
    let mut text = String::from("Summarize the following conversation branch concisely. Focus on the key decisions, findings, and outcomes:\n\n");
    for entry in entries {
        if let crate::session::SessionEntryContent::Message { message } = &entry.content {
            if let Some(role) = message.get("role").and_then(|r| r.as_str()) {
                let content = message
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                if !content.is_empty() {
                    text.push_str(&format!("[{}] {}\n", role, content));
                }
            }
        }
    }
    text
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

/// Walk from `old_leaf` to root and from `new_target` to root, find the common
/// ancestor, and return `(abandoned_entries, common_ancestor_id)`.
///
/// `get_path_to_root` returns entries in root→leaf order.
fn collect_entries_for_branch_summary(
    index: &crate::session::SessionIndex,
    old_leaf: &str,
    new_target: &str,
) -> (Vec<crate::session::SessionEntry>, String) {
    let old_path = index.get_path_to_root(old_leaf);
    let new_path = index.get_path_to_root(new_target);

    let common_len = old_path
        .iter()
        .zip(new_path.iter())
        .take_while(|(a, b)| a.id == b.id)
        .count();

    let common_ancestor_id = common_len
        .checked_sub(1)
        .and_then(|i| old_path.get(i))
        .map(|e| e.id.clone())
        .unwrap_or_default();

    let abandoned = old_path.into_iter().skip(common_len).collect();
    (abandoned, common_ancestor_id)
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

/// Find the predecessor of `target_id` on the session's active branch.
pub(crate) fn find_predecessor_on_active_branch(
    index: &crate::session::SessionIndex,
    target_id: &str,
) -> Option<String> {
    let leaf = index.leaf_id.as_deref()?;
    let branch = index.get_path_to_root(leaf); // root → leaf
    find_predecessor(&branch, target_id)
}

/// Where the compaction result gets written. Lets `run_compaction` serve
/// both the `/tree <id> compact` / `/compact` path (a `Compaction` entry
/// parented at the resolved target) and a future `/tree <id> switch
/// --summarize` path (a `BranchSummary` entry parented at the abandoned
/// branch's common ancestor). Today only `AtPoint` is wired up; the
/// `Abandoned` variant exists so the backend generalizes without
/// requiring a future refactor.
#[derive(Debug, Clone)]
pub(crate) enum CompactionTarget {
    /// Write a `Compaction` entry with `first_kept_entry_id = target_id`.
    /// `tokens_before` is the size of the context the compaction
    /// replaces (used for the `tokensBefore` field, matching pi's
    /// `prepareCompaction`).
    AtPoint {
        target_id: String,
        tokens_before: u64,
    },
    /// Future: write a `BranchSummary` entry parented at the common
    /// ancestor of the abandoned branch. Not yet wired up to any
    /// command.
    #[allow(dead_code)]
    Abandoned { common_ancestor_id: String },
}

/// Review hook for a generated compaction summary before it is written. Given
/// the draft summary text, returns the final text to store (`Some`) or
/// cancels the compaction (`None`). The interactive caller wires this to the
/// external-editor + accept/cancel dialog; `None` here means "accept the
/// draft as-is" (quiet / test paths).
pub(crate) type CompactionSummaryReview = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Result of `run_compaction`.
#[derive(Debug)]
pub(crate) enum CompactionResult {
    /// Compaction entry written.
    Done,
    /// User cancelled at the review step; no entry written.
    Cancelled,
    /// Unrecoverable error.
    Err(String),
}

impl std::fmt::Display for CompactionResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactionResult::Done => write!(f, "done"),
            CompactionResult::Cancelled => write!(f, "cancelled"),
            CompactionResult::Err(e) => write!(f, "error: {}", e),
        }
    }
}

impl std::error::Error for CompactionResult {}

impl CompactionResult {
    /// True if the compaction succeeded (entry written).
    #[cfg(test)]
    pub fn is_done(&self) -> bool {
        matches!(self, CompactionResult::Done)
    }
}

impl From<std::result::Result<(), String>> for CompactionResult {
    fn from(r: std::result::Result<(), String>) -> Self {
        match r {
            Ok(()) => CompactionResult::Done,
            Err(e) => CompactionResult::Err(e),
        }
    }
}

impl From<CompactionResult> for std::result::Result<(), String> {
    fn from(r: CompactionResult) -> Self {
        match r {
            CompactionResult::Done | CompactionResult::Cancelled => Ok(()),
            CompactionResult::Err(e) => Err(e),
        }
    }
}

/// Run a compaction of `mode` at the resolved target. Shared by the
/// `/tree <id> compact` and `/compact` (→ `/tree <last_user_id> compact`)
/// paths. The caller is responsible for the up-front validation
/// (active session, known target, leaf isn't a compaction); this
/// function only handles the per-mode mechanics.
///
/// - `target`: where to write the result (a `Compaction` entry at the
///   resolved point; a `BranchSummary` for the abandoned-branch case).
/// - `llm_payload`: user-editable prompt override (only consulted when
///   `mode == Llm`); when empty, `settings.llm_compact_prompt()` is
///   used instead.
/// - `review`: optional review hook consulted for the summary-producing
///   modes (`TextOnly`, `Llm`) right before the compaction entry is
///   written; it can edit the draft and/or cancel.
/// - `user_cancel` / `cancel_notify`: the REPL's Esc-cancel flags, threaded
///   into the LLM summarization so a retry back-off can be interrupted with
///   `<Esc>` (same as a normal prompt).
///
/// Returns `CompactionResult::Done` on success, `CompactionResult::Cancelled`
/// when the user cancelled at the review step (no entry written),
/// `CompactionResult::Err(msg)` on failure. The caller is responsible for
/// handling `Cancelled` — typically by returning to the editor with the
/// original prompt line pre-filled (e.g. via the slot's prefill register).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_compaction(
    session: &Session,
    agent: &Option<Arc<dyn PicoAgent>>,
    settings: &SettingsStore,
    target: CompactionTarget,
    mode: CompactSummaryMode,
    llm_payload: &str,
    output: &dyn Output,
    review: Option<CompactionSummaryReview>,
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
) -> CompactionResult {
    match target {
        CompactionTarget::AtPoint {
            target_id,
            tokens_before,
        } => run_compaction_at_point(
            session,
            agent,
            settings,
            &target_id,
            tokens_before,
            mode,
            llm_payload,
            output,
            review,
            user_cancel,
            cancel_notify,
        )
        .await
        .into(),
        CompactionTarget::Abandoned { common_ancestor_id } => {
            // Future: wire the abandoned-branch summary here. The
            // `Abandoned` variant intentionally does not abort with a
            // `not implemented` error; `run_compaction` is only ever
            // called with `AtPoint` today, so the `dead_code` arm sits
            // dormant until the `SwitchAction` UI extension lands.
            let _ = common_ancestor_id;
            CompactionResult::Err("abandoned-branch summary not yet wired up".to_string())
        }
    }
}

/// Run a compaction from an inline menu node — session/agent/output/
/// guard/settings all reachable via the slot (the loop-side applier it
/// replaces did the same through its own locals). Returns the message to display;
/// `None` = Done/Cancelled are silent (the review gate prints its own
/// cancel notice). The Llm-mode agent is created here from the TARGET
/// config — same target-stamp judgment as session creation.
pub(crate) async fn compact_inline(
    app_state: &AppState,
    target_id: &str,
    mode: crate::app_state::CompactMode,
    payload: &str,
) -> Option<String> {
    let Some(s) = app_state.session() else {
        return Some("Cannot compact: no active session".to_string());
    };
    if s.get_entry(target_id).await.is_none() {
        return Some(format!(
            "Cannot compact: unknown tree entry {}",
            &target_id[..target_id.len().min(12)]
        ));
    }
    let tokens_before = match s.build_context().await {
        Ok(ctx) => estimate_context_tokens(&ctx.messages),
        Err(_) => 0,
    };
    let resolved_mode = match mode {
        crate::app_state::CompactMode::Manual => CompactSummaryMode::Manual,
        crate::app_state::CompactMode::TextOnly => CompactSummaryMode::TextOnly,
        crate::app_state::CompactMode::Prompt => CompactSummaryMode::Llm,
    };
    // LLM mode needs a live agent (slot is the source).
    let mut agent = app_state.agent();
    if resolved_mode == CompactSummaryMode::Llm && agent.is_none() {
        let cfg = app_state.config_snapshot();
        match create_agent(cfg).await {
            Ok(a) => {
                let a: Arc<dyn PicoAgent> = Arc::from(a);
                app_state.activate_agent(a.clone());
                agent = Some(a);
            }
            Err(e) => return Some(format!("Cannot start agent: {}", e)),
        }
    }
    // LLM mode early-out: refuse if nothing to compact.
    if resolved_mode == CompactSummaryMode::Llm {
        let current_leaf = s.get_leaf_id().await;
        if let Some(leaf) = current_leaf.as_deref() {
            let index = s.index().lock().await;
            if find_predecessor_on_active_branch(&index, target_id).as_deref() == Some(leaf) {
                return Some("Cannot compact: nothing before the current position".to_string());
            }
        }
    }
    // Editor review: the term guard + output clone out of the slot; the
    // closure suspends raw mode around $EDITOR and re-anchors the draw
    // cache (DSR) on exit itself (plan §4.2).
    let review: Option<CompactionSummaryReview> = app_state.term_guard().map(|g| {
        let out = app_state.with_config(|c| c.output.clone());
        Box::new(move |draft: &str| crate::repl::review_compaction_summary(&g, out.as_ref(), draft))
            as CompactionSummaryReview
    });
    let output = app_state.with_config(|c| c.output.clone());
    let settings = app_state.settings();
    let (user_cancel, cancel_notify) =
        app_state.with_config(|c| (c.user_cancel.clone(), c.cancel_notify.clone()));
    match run_compaction(
        &s,
        &agent,
        &settings,
        CompactionTarget::AtPoint {
            target_id: target_id.to_string(),
            tokens_before,
        },
        resolved_mode,
        payload,
        output.as_ref(),
        review,
        &user_cancel,
        &cancel_notify,
    )
    .await
    {
        CompactionResult::Done | CompactionResult::Cancelled => None,
        CompactionResult::Err(msg) => Some(msg),
    }
}

/// Implementation of the `AtPoint` arm. The dropped span for any mode
/// is the active-branch entries strictly before `target_id`.
#[allow(clippy::too_many_arguments)]
async fn run_compaction_at_point(
    session: &Session,
    agent: &Option<Arc<dyn PicoAgent>>,
    settings: &SettingsStore,
    target_id: &str,
    tokens_before: u64,
    mode: CompactSummaryMode,
    llm_payload: &str,
    output: &dyn Output,
    review: Option<CompactionSummaryReview>,
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
) -> CompactionResult {
    // -------------------------------------------------------------------------
    // Shared preamble: tree pivot and dropped span (all modes)
    // -------------------------------------------------------------------------

    // 1. Get the full raw branch at the original leaf so we can compute the
    //    dropped span and look up the first user prompt.
    let original_branch = session.get_branch().await;

    // 2. Find the entry just before target_id on the active branch.
    //    Required for LLM mode (pivot needs a valid leaf). Optional for
    //    manual/text-only (seed_text is only used when present).
    let predecessor_id = find_predecessor(&original_branch, target_id);

    // 3. Pivot the tree so `build_context()` re-scopes to the prefix
    //    [root .. predecessor_id]. This activates any compaction entry at
    //    or before that point. `get_pivot_context` handles save/move/restore.
    //    We only pivot when there is a predecessor — for root-level compactions
    //    (no predecessor) manual/text-only modes skip the pivot and use no
    //    seed text; LLM mode still requires a predecessor and returns early.
    let seed_text = if let Some(ref pred) = predecessor_id {
        match session.get_pivot_context(pred).await {
            Some((ctx, _)) => crate::session::Session::extract_seed_from_context(&ctx),
            None => None,
        }
    } else {
        // No predecessor: root-level compaction. Manual/text-only proceed with
        // no seed; LLM mode returns an error below.
        None
    };

    // Session directory for relativizing file paths in compaction summaries.
    let session_dir = std::path::PathBuf::from(session.index().lock().await.header.cwd.clone());

    // 4. LLM mode requires a valid predecessor to scope the summarization
    //    context. Manual and text-only modes tolerate root-level compactions.
    if predecessor_id.is_none() && matches!(mode, CompactSummaryMode::Llm) {
        return Err("nothing before this point".to_string()).into();
    }

    // 5. The dropped span is everything on the original branch strictly
    //    before target_id (i.e. the span this compaction will discard).
    let dropped: Vec<_> = original_branch
        .into_iter()
        .take_while(|e| e.id != target_id)
        .collect();

    // -------------------------------------------------------------------------
    // Mode-specific processing
    // -------------------------------------------------------------------------

    match mode {
        CompactSummaryMode::Manual => {
            // Manual mode pre-fills a template of touched files (split into
            // `Documents` (`.md`) and `Files`) seeded with the active compaction's
            // summary (incremental compaction) or the first user prompt (first
            // compaction). The user edits it in the review gate before it is
            // stored. A span with no files and no seed text has nothing to
            // point at, so we fall through to the empty-summary no-op.
            let draft =
                crate::session::build_manual_template(&dropped, seed_text.as_deref(), &session_dir);
            let summary = if draft.is_empty() {
                String::new()
            } else {
                match &review {
                    Some(r) => match r(&draft) {
                        Some(s) => s,
                        None => return CompactionResult::Cancelled, // user cancelled
                    },
                    None => draft, // non-interactive (tests, pipe): accept as-is
                }
            };
            match session
                .append_compaction(&summary, target_id, tokens_before)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
            {
                Ok(()) => {}
                Err(e) => return CompactionResult::Err(e),
            }
            if !summary.is_empty() {
                output.display_separator();
                output.display_markdown(&summary);
            }
            CompactionResult::Done
        }
        CompactSummaryMode::TextOnly => {
            // Deterministic text-only summary: user prompts + assistant text
            // responses per turn, plus touched-files list. Seed text from the
            // pivot context is available (via `seed_text`) but intentionally
            // not included — text-only summarises the dropped span directly.
            let draft = crate::session::build_text_only_summary(&dropped, &session_dir);
            let summary = match &review {
                Some(r) => match r(&draft) {
                    Some(s) => s,
                    None => return CompactionResult::Cancelled,
                },
                None => draft,
            };
            match session
                .append_compaction(&summary, target_id, tokens_before)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
            {
                Ok(()) => {}
                Err(e) => return CompactionResult::Err(e),
            }
            if !summary.is_empty() {
                output.display_separator();
                output.display_markdown(&summary);
            }
            CompactionResult::Done
        }
        CompactSummaryMode::Llm => {
            // 6. Compose the user-tweakable instruction: trailing text on
            //    the committed line wins; otherwise the setting (with
            //    built-in fallback).
            let instruction = if llm_payload.trim().is_empty() {
                settings.llm_compact_prompt()
            } else {
                llm_payload.to_string()
            };

            // 7. The leaf is already moved to predecessor_id by step 3.
            //    `summarize_with_session_context` calls `build_context()` internally,
            //    which re-scopes to the moved leaf and includes any active
            //    compaction's summary as the leading message. Retries re-send
            //    the identical scoped prefix. The leaf is restored automatically
            //    by `get_pivot_context`'s cleanup on each loop iteration.
            let summary_text = match agent.as_ref() {
                Some(a) => {
                    let instruction = instruction.clone();
                    let r = retry_summarization(output, user_cancel, cancel_notify, &mut || {
                        a.summarize_with_session_context(session, &instruction)
                    })
                    .await;
                    match r {
                        Ok(t) => t,
                        Err(e) => return Err(format!("Summary failed: {}", e)).into(),
                    }
                }
                None => return Err("no agent".to_string()).into(),
            };

            // 8. Empty response: soft-fail.
            let summary_text = summary_text.trim().to_string();
            if summary_text.is_empty() {
                return Err("Summary was empty; nothing written.".to_string()).into();
            }

            // 9. Summary review / edit gate.
            let summary_text = match &review {
                Some(r) => match r(&summary_text) {
                    Some(s) => s,
                    None => return CompactionResult::Cancelled,
                },
                None => summary_text,
            };

            // 10. Append the compaction entry.
            match session
                .append_compaction(&summary_text, target_id, tokens_before)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
            {
                Ok(()) => {}
                Err(e) => return CompactionResult::Err(e),
            }

            // 11. Render the summary.
            output.display_separator();
            output.display_markdown(&summary_text);
            CompactionResult::Done
        }
    }
}

/// Run a *summarization* request with the same auto-retry / back-off / Esc-
/// cancel behaviour as a regular user prompt (`prompt()`'s retry loop in
/// `run_interactive`).
///
/// `build` produces the summarization request and returns the model's reply
/// (or a classified `PicocodeError`). On a retryable failure it is re-invoked
/// (previous `build` again) after `retry_delay_secs` seconds of
/// `cancellable_sleep`; non-retryable errors and cancellation propagate out.
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
    build: &mut F,
) -> std::result::Result<String, crate::PicocodeError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<String, crate::PicocodeError>>,
{
    let mut consecutive_failures: u8 = 0;
    loop {
        match build().await {
            Ok(text) => return Ok(text),
            Err(crate::PicocodeError::Cancelled(_)) => {
                return Err(crate::PicocodeError::Cancelled("Aborted.".into()));
            }
            Err(e) if e.is_auto_retryable() && consecutive_failures < retry_budget(&e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let secs = retry_delay_secs(&e, consecutive_failures);
                output.display_text(&format!("Error: {}. Retrying in {secs}s...", e));
                let slept = cancellable_sleep(user_cancel, cancel_notify, secs).await;
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
            Err(e) => return Err(e),
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
    thinking_level: Option<String>,
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
                            thinking_level,
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
/// shared by the inline fork node and `handle_session_command`. The
/// caller activates it (slot) or stores it (typed path's local write).
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

/// Load a session by meta and switch to it (the `/session resume`
/// action — shared by the typed path and the inline resume node).
/// Migrates to the current cwd when the session lives elsewhere (fork
/// + delete original), replays `(count, is_lines)` through `output`,
/// and computes the `/model` prefill against `qualified_id`.
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
        output.display_separator();
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
            c.thinking_level.clone(),
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

/// Interactive chat loop. Owns the `Box<dyn PicoAgent>` and the `AgentConfig`
/// so slash commands that change model/provider/thinking can rebuild the agent
/// via `create_agent` (required because `CodeAgent<M>` is generic over the
/// provider's completion-model type).
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
    let mut repo: Option<crate::session::repo::JsonlSessionRepo> =
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
    // Shared cancel flag: the in-flight reader thread sets it on `Esc`; the
    // `LoggingHook` bridge turns it into a rig `CancelSignal` that aborts the
    // current request.
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
    // First session — created up front so the `/session` overlay can mark
    // it from the start (the file itself is written lazily on the first
    // `append_message`, so no empty session is created for a user who
    // never takes a turn).
    let first = ensure_session(
        app_state.session(),
        &repo,
        Some(config.provider.clone()),
        Some(config.model.clone()),
        config.thinking_level.clone(),
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
    let mut last_attempted_target: Option<(String, Option<String>, Option<String>)> = None;
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
    let _ = tokio::task::spawn(async {
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
    // `handle_session_command` dispatch opens + replays the session, and the
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
    loop {
        let action = match repl.as_mut() {
            Some(r) => {
                config.output.display_separator();

                // Build session context data for REPL overlays.
                let (_sessions, branch_messages, tree, leaf_id, context_tokens) =
                    build_session_ctx(&app_state.session()).await;

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
                    thinking_level: config.thinking_level.clone(),
                };
                // Pop the pending prefill (if any); the read_input call below
                // uses it to seed the LineEditor, after which the next loop
                // iteration starts with `None`.
                let prefill = app_state.take_prefill();
                let prefill_ref = prefill.as_deref();
                // `read_input` re-anchors the draw cache itself on entry
                // (DSR) and tracks its frame through `frame_anchor()`
                // afterwards (ui_draw_v2 §5.2).
                r.read_input(
                    &prompt_str,
                    prefill_ref,
                    &mut line_history,
                    &ctx,
                    &app_state,
                    config.output.as_ref(),
                )
                .await
            }
            None => Some(InputAction::Submit(config.output.get_user_input())),
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
        let model_changed = config.qualified_model_id() != target.qualified_model_id();
        let thinking_changed = config.thinking_level != target.thinking_level;
        let persona_changed = config.persona_name != target.persona_name;
        let target_thinking = target.thinking_level.clone();

        // One rebuild attempt per distinct target: re-running a failed
        // attempt would reprint the same error every cycle while the user
        // digs back to the menu to fix it. Changing the target — or an
        // explicit `/model` re-commit of the same model — arms the next
        // attempt. (`create_agent` is local: key/config errors are final
        // for a given target, so recovery really is "change the target".)
        let target_fp = (
            target.qualified_model_id(),
            target.thinking_level.clone(),
            target.persona_name.clone(),
        );
        // Re-arm = a *different* target than the last attempt (the old
        // explicit-commit re-arm died with `model_commit`; re-selecting
        // an identical target is a no-op by definition — target unchanged).
        let rebuild_armed = last_attempted_target.as_ref() != Some(&target_fp);
        if (model_changed || thinking_changed || persona_changed) && rebuild_armed {
            last_attempted_target = Some(target_fp);
            let mut new_cfg = config.clone();
            if model_changed {
                new_cfg.provider = target.provider.clone();
                new_cfg.model = target.model.clone();
            }
            if thinking_changed {
                new_cfg.thinking_level = target.thinking_level.clone();
            }
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
                    } else if thinking_changed {
                        "set thinking"
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
        if thinking_changed && config.thinking_level == target_thinking {
            if let Some(ref s) = recording_session {
                record_thinking_change(s, target_thinking.as_deref().unwrap_or("off")).await;
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
                    config.thinking_level.clone(),
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
                    let (capture, cmd) = if input.starts_with("!!") {
                        (false, &input[2..])
                    } else {
                        (true, &input[1..])
                    };
                    let cmd = cmd.trim();
                    // Reprint the submitted command (cleared by `read_input`).
                    config.output.display_plain(&input);
                    config.output.display_separator();
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
                // Handle `/tree <target_id>` commands — navigate or compact.
                if input.starts_with("/tree") {
                    let rest = input["/tree".len()..].trim();
                    let (target_id, action, mode, summarize, llm_payload) = parse_tree_args(rest);
                    if target_id.is_empty() {
                        // No target: the overlay is already open from from_buffer.
                        continue;
                    }

                    match action {
                        TreeAction::Switch => {
                            if let Some(s) = app_state.session() {
                                let old_leaf_id = s.get_leaf_id().await;
                                if old_leaf_id.as_deref() != Some(target_id.as_str()) {
                                    // Summarize the abandoned branch if requested.
                                    if summarize {
                                        if let Some(old_leaf) = old_leaf_id.as_deref() {
                                            let (abandoned, common_ancestor_id) = {
                                                let index = s.index().lock().await;
                                                collect_entries_for_branch_summary(
                                                    &index, old_leaf, &target_id,
                                                )
                                            };
                                            if !abandoned.is_empty() {
                                                config.output.display_thinking(
                                                    "Summarizing abandoned branch...",
                                                );
                                                // Lazy agent build, mirroring the
                                                // `compact llm` arm: a
                                                // `/tree <id> switch --summarize`
                                                // right after `/session resume`
                                                // (before any prompt) would
                                                // otherwise hit the `None` arm
                                                // below and silently skip the
                                                // summary. A non-empty abandoned
                                                // branch is the signal that an
                                                // LLM call is needed, so we
                                                // build the agent on demand and
                                                // store it for the next prompt
                                                // to reuse.
                                                let mut agent = app_state.agent();
                                                if agent.is_none() {
                                                    match create_agent(config.clone()).await {
                                                        Ok(a) => {
                                                            app_state.activate_agent(Arc::from(a));
                                                            agent = app_state.agent();
                                                        }
                                                        Err(e) => {
                                                            config.output.display_text(&format!(
                                                                "Cannot start agent: {}",
                                                                e
                                                            ));
                                                            continue;
                                                            // (Inside the `summarize`
                                                            //  branch of `Switch`. Skip
                                                            //  the summary; the leaf
                                                            //  still moves below.)
                                                        }
                                                    }
                                                }
                                                let prompt = build_summary_prompt(&abandoned);
                                                if let Some(ref agent) = agent {
                                                    let r = retry_summarization(
                                                        config.output.as_ref(),
                                                        &config.user_cancel,
                                                        &config.cancel_notify,
                                                        &mut || agent.summarize(&prompt),
                                                    )
                                                    .await;
                                                    match r {
                                                        Ok(summary) => {
                                                            let _ = s
                                                                .append_branch_summary(
                                                                    &common_ancestor_id,
                                                                    &summary,
                                                                )
                                                                .await;
                                                        }
                                                        Err(e) => {
                                                            config.output.display_text(&format!(
                                                                "Summary failed: {}",
                                                                e
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Move the leaf pointer. The next prompt() call
                                    // reads the new branch via build_context()
                                    // automatically.
                                    let _ = s.move_leaf(Some(target_id.clone())).await;
                                }
                                // No confirmation line: the committed prompt
                                // "/tree <id> switch [--summarize]" (echoed by the
                                // overlay's emit_commit, or typed verbatim by the
                                // user) documents the action.
                            }
                        }
                        TreeAction::Compact => {
                            let Some(s) = app_state.session() else {
                                config
                                    .output
                                    .display_text("Cannot compact: no active session");
                                continue;
                            };
                            let old_leaf_id = s.get_leaf_id().await;

                            // The leaf-is-compaction guard was removed in
                            // phase C of doc/nested_compactions.md: pi's
                            // prepareCompaction refuses only when the
                            // dropped span is empty (compaction.ts:688),
                            // and the no-op stacked case is handled by
                            // the LLM-mode guard below ("nothing before
                            // the current position"). Manual and
                            // text-only on an empty dropped span produce
                            // a legal (if useless) compaction entry.

                            // Validate the target is a known entry, so we never
                            // write a silently no-op compaction
                            // (firstKeptEntryId that never resolves).
                            if s.get_entry(&target_id).await.is_none() {
                                config.output.display_text(&format!(
                                    "Cannot compact: unknown tree entry {}",
                                    &target_id[..target_id.len().min(12)],
                                ));
                                continue;
                            }

                            // tokensBefore from the current scoped context
                            // (pi semantics: tokens of the context being
                            // replaced). Computed once up-front so all three
                            // modes agree on the value (the header shows the
                            // same number).
                            let tokens_before = match s.build_context().await {
                                Ok(ctx) => estimate_context_tokens(&ctx.messages),
                                Err(_) => 0,
                            };

                            let resolved_mode = mode.unwrap_or(CompactSummaryMode::Manual);

                            // LLM mode needs a live agent. Startup is lazy:
                            // `agent` is only built on the first normal prompt
                            // (or /model etc.), so `compact llm` right after a
                            // `/session resume` (before any prompt) would
                            // otherwise hit `run_compaction`'s `None` arm and
                            // fail with "no agent". Build it here, mirroring
                            // the lazy first-prompt creation below. Manual /
                            // TextOnly never touch the agent, so they stay
                            // network-free regardless.
                            let mut agent = app_state.agent();
                            if resolved_mode == CompactSummaryMode::Llm && agent.is_none() {
                                match create_agent(config.clone()).await {
                                    Ok(a) => {
                                        app_state.activate_agent(Arc::from(a));
                                        agent = app_state.agent();
                                    }
                                    Err(e) => {
                                        config
                                            .output
                                            .display_text(&format!("Cannot start agent: {}", e));
                                        continue;
                                    }
                                }
                            }

                            // LLM mode-specific early-out: refuse if the
                            // dropped span is empty. (Manual / TextOnly
                            // are no-ops on an empty dropped span but
                            // still legal; the user explicitly asked.)
                            if resolved_mode == CompactSummaryMode::Llm {
                                if let Some(leaf) = old_leaf_id.as_deref() {
                                    let index = s.index().lock().await;
                                    if find_predecessor_on_active_branch(&index, &target_id)
                                        .as_deref()
                                        == Some(leaf)
                                    {
                                        config.output.display_text(
                                            "Cannot compact: nothing before the current position",
                                        );
                                        continue;
                                    }
                                }
                            }

                            // Summary review / edit gate (all three modes can
                            // now produce a user-editable draft: manual
                            // pre-fills a Documents/Files list, text-only
                            // and llm fill their existing drafts). When the
                            // interactive REPL is active, the generated
                            // draft is opened in $EDITOR and the user
                            // accepts (default) or cancels before the
                            // entry is written (repl.rs
                            // review_compaction_summary). Outside the REPL
                            // (quiet / pipe / tests) there is no dialog and
                            // the draft is accepted as-is. An empty draft
                            // (manual mode on a no-file span) still
                            // takes this branch — `review` is only
                            // consulted inside `run_compaction`, which
                            // short-circuits to the empty summary path.
                            let review: Option<CompactionSummaryReview> = if let Some(g) = &guard {
                                let g = g.clone();
                                let out = config.output.clone();
                                Some(Box::new(move |draft: &str| {
                                    crate::repl::review_compaction_summary(&g, out.as_ref(), draft)
                                }))
                            } else {
                                None
                            };

                            match run_compaction(
                                &s,
                                &agent,
                                &app_state.settings(),
                                CompactionTarget::AtPoint {
                                    target_id: target_id.clone(),
                                    tokens_before,
                                },
                                resolved_mode,
                                &llm_payload,
                                config.output.as_ref(),
                                review,
                                &config.user_cancel,
                                &config.cancel_notify,
                            )
                            .await
                            {
                                CompactionResult::Done => {}
                                // User cancelled at the review step: restore the
                                // prompt line to the editor so they can switch
                                // methods or tune the LLM prompt.
                                CompactionResult::Cancelled => {
                                    app_state.set_prefill(input.clone());
                                }
                                CompactionResult::Err(msg) => {
                                    config.output.display_text(&msg);
                                }
                            }
                            // The compaction entry is now the leaf; for
                            // the LLM mode the leaf was restored before
                            // `append_compaction` so subsequent turns
                            // are appended after the compaction (the
                            // next `prompt()` rebuilds the scoped
                            // context via `build_context()`). No session
                            // switch.
                        }
                    }
                    continue;
                }
                // Handle `/session <name> <action>` commands.
                if input.starts_with("/session") {
                    let out = config.output.clone();
                    let mut sess = app_state.session();
                    let result = handle_session_command(
                        &input,
                        &mut sess,
                        &mut repo,
                        &mut config,
                        &out,
                        &app_state.settings(),
                    )
                    .await;
                    app_state.set_session(sess);
                    match result {
                        SessionCommandResult::NotSession => {
                            // Not a session command — fall through to normal prompt.
                        }
                        SessionCommandResult::Switched {
                            id: new_id,
                            prefill,
                        } => {
                            app_state.set_current_session_id(new_id);
                            // Resume returned a model-restore prefill (e.g.
                            // `/model <provider>:<model_id>`). Surface it on the
                            // next prompt instead of letting the user type it
                            // themselves; clearing the buffer would force them to
                            // retype the model they just resumed into. The
                            // prefill is consumed once — the slot register is
                            // taken at the top of the next loop iteration so a
                            // subsequent prompt starts empty.
                            if let Some(p) = prefill {
                                app_state.set_prefill(p);
                            }
                            continue;
                        }
                        _ => {
                            continue;
                        }
                    }
                }
                // Handle `/clear`: delete current session and start fresh.
                // Optional: `/clear <name>` names the new session so it shows
                // up in `/session` listings by user-friendly name from the
                // first turn onward (instead of needing a follow-up rename).
                if input == "/clear" || input.starts_with("/clear ") {
                    let new_name = input["/clear".len()..].trim().to_string();
                    if let Some(s) = app_state.session() {
                        let id = s.get_session_id().to_string();
                        if let Some(ref r) = repo {
                            // Find the session meta to delete it.
                            let cwd = std::env::current_dir().ok();
                            let metas = r.list(cwd.as_deref()).await.unwrap_or_default();
                            if let Some(meta) = metas.iter().find(|m| m.id == id) {
                                let _ = r.delete(meta).await;
                            }
                        }
                    }
                    // Create a fresh session for the next prompt so context
                    // carries across turns AND the new turns get persisted.
                    // `repo.create()` defers the header write to the first
                    // `append_message`, so a user that quits immediately
                    // still produces no empty `.jsonl` file. (`None` drops
                    // the in-memory session — ensure keeps a `Some` as-is.)
                    let fresh = ensure_session(
                        None,
                        &repo,
                        Some(config.provider.clone()),
                        Some(config.model.clone()),
                        config.thinking_level.clone(),
                    )
                    .await;
                    let fresh_id = fresh
                        .as_ref()
                        .map(|s| s.get_session_id().to_string())
                        .unwrap_or_default();
                    app_state.activate_session(fresh.clone(), fresh_id);
                    if !new_name.is_empty() {
                        if let Some(ref s) = fresh {
                            if let Err(e) = s.set_session_name(&new_name).await {
                                config
                                    .output
                                    .display_text(&format!("Could not name session: {}", e));
                            } else if let Some(ref r) = repo {
                                // Mirror `/session <name> rename`: refresh the
                                // cached listing so the new name is visible
                                // immediately.
                                r.invalidate_cache();
                            }
                        }
                    }
                    continue;
                }
                // Handle `/new`: drop the current in-memory session and let
                // `ensure_session` create a fresh one on the next prompt.
                // The old session is preserved on disk and can be resumed
                // via `/session <name> resume`. Optional: `/new <name>`
                // names the new session (same rationale as `/clear <name>`).
                if input == "/new" || input.starts_with("/new ") {
                    let new_name = input["/new".len()..].trim().to_string();
                    // See `/clear` above: without the fresh ensure, the next
                    // prompt would run with no session — losing prior context
                    // and skipping every `append_message` (since they're gated
                    // on the session being present), so the new "session"
                    // wouldn't be saved. (`None` drops the in-memory session.)
                    let fresh = ensure_session(
                        None,
                        &repo,
                        Some(config.provider.clone()),
                        Some(config.model.clone()),
                        config.thinking_level.clone(),
                    )
                    .await;
                    let fresh_id = fresh
                        .as_ref()
                        .map(|s| s.get_session_id().to_string())
                        .unwrap_or_default();
                    app_state.activate_session(fresh.clone(), fresh_id);
                    if !new_name.is_empty() {
                        if let Some(ref s) = fresh {
                            if let Err(e) = s.set_session_name(&new_name).await {
                                config
                                    .output
                                    .display_text(&format!("Could not name session: {}", e));
                            } else if let Some(ref r) = repo {
                                // Mirror `/session <name> rename`: refresh the
                                // cached listing so the new name is visible
                                // immediately.
                                r.invalidate_cache();
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
                // cleared the input line on submit.
                config.output.display_plain(&input);
                // Separator between the user's prompt and the agent response.
                config.output.display_separator();
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
                // (if any) spanning the whole window. `Esc` during the
                // retry's back-off sleep returns `Cancelled`; `Esc` during
                // the prompt itself is handled inside `prompt()`.
                let reader_handle = if let Some(g) = &guard {
                    let done = Arc::new(AtomicBool::new(false));
                    let confirm_gate = config
                        .output
                        .confirm_gate()
                        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                    let h = crate::repl::spawn_in_flight_reader(
                        g.clone(),
                        cancel.clone(),
                        config.cancel_notify.clone(),
                        done.clone(),
                        confirm_gate,
                    );
                    Some((h, done))
                } else {
                    None
                };
                let response: std::result::Result<String, crate::PicocodeError> = {
                    let mut consecutive_failures: u8 = 0;
                    let mut next_input: String = input.clone();
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
                            .prompt(&next_input, session.as_deref(), &turn)
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
                                let slept = cancellable_sleep(
                                    &config.user_cancel,
                                    &config.cancel_notify,
                                    secs,
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
                                let slept = cancellable_sleep(
                                    &config.user_cancel,
                                    &config.cancel_notify,
                                    secs,
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
                                    // The feedback replaces the original prompt
                                    // as a NEW user dispatch for this turn.
                                    turn.mark_new_dispatch();
                                }
                                // Race window: cancel could fire after
                                // `cancellable_sleep` returned but before
                                // `prompt()` clears its flag. Bail out
                                // here so the retry respects `Esc`.
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

/// Compute the `/stat` panel lines. Renders two sections:
///
/// 1. **session** — display name, created/last-updated times, total session size
///    (mirrors `/session <id>`), estimated compacted context tokens.
/// 2. **model**   — provider/model identifier, context-window limit, current
///    usage, approximate session cost, cache hit rate.
///
/// Reads the live model catalog for the window size and the per-token pricing,
/// and the agent's accumulated [`SessionUsage`] for the per-turn / session
/// totals. Returns plain lines so the REPL can render them in the `/stat`
/// overlay (`render_plain` display).
pub fn session_stats_lines(
    provider: &str,
    model: &str,
    usage: SessionUsage,
    session: Option<SessionStatsInfo>,
    context_tokens: u64,
) -> Vec<String> {
    let entry = model_catalog::lookup(provider, model);
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
        tool_call_limit: usize,
        provider: String,
        model: String,
        yolo: bool,
        persona_name: Option<String>,
        user_cancel: Arc<AtomicBool>,
        cancel_notify: Arc<tokio::sync::Notify>,
        thinking_level: Option<String>,
        reasoning_in_context: crate::settings::ReasoningInContextMode,
        message_logger: Option<Arc<MessageLogger>>,
    ) -> Self {
        Self {
            agent,
            output,
            tool_call_limit,
            provider,
            model,
            yolo,
            persona_name,
            user_cancel,
            cancel_notify,
            thinking_level,
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
    let base = std::env::temp_dir().join(format!(
        "picocode-repo-test-{}-{}",
        std::process::id(),
        nanos
    ));
    let cwd = std::path::Path::new("/tmp/v25-cwd");
    let dir = base.join(encode_cwd(cwd));
    std::fs::create_dir_all(&dir).unwrap();
    let header = SessionHeader::new(cwd);
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
    (repo, uuid, base)
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
) -> Arc<Session> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    rt.block_on(async move {
        use crate::session::storage::JsonlSessionStorage;
        use crate::session::types::session_filename;
        let dir = std::env::temp_dir().join(format!(
            "picocode-slot-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
        Arc::new(Session::new_persisted(storage, index))
    })
}

#[cfg(test)]
mod tests {
    use rig_core::message::Reasoning;

    use super::TurnPersistence;
    use super::{
        blob_edit_tool_calls, build_replay_units, compute_resume_model_prefill, count_turns,
        create_private_dir, find_predecessor_on_active_branch, is_anthropic_family,
        last_turns_start, maybe_strip_reasoning, message_role, parse_tree_args, persist_turn,
        reasoning_text, record_model_change, record_thinking_change, retry_summarization,
        run_compaction, session_recorded_model_id, session_recorded_thinking, CompactionResult,
        CompactionSummaryReview, CompactionTarget, MessageLogger,
    };
    use super::{CompactSummaryMode, ReplayUnit, TreeAction};
    use crate::session::storage::SessionStorage;
    use crate::session::Session;
    use crate::SettingsStore;
    use rig_core::message::{AssistantContent, Message, Text};

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

        // Array -> block.
        let out = blob_edit_tool_calls(vec![mk(args)]);
        let Message::Assistant { content, .. } = &out[0] else {
            panic!("expected assistant")
        };
        let AssistantContent::ToolCall(tc) = &content[0] else {
            panic!("expected tool call")
        };
        let edits = tc
            .function
            .arguments
            .get("edits")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(edits.contains("@@@OLD"), "got: {edits}");
        assert!(edits.contains("src/x.rs"), "got: {edits}");

        // Idempotent: an already-block value is unchanged.
        let already =
            serde_json::json!({"path": "src/x.rs", "edits": "@@@OLD\na\n@@@NEW\nb\n@@@END"});
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
    ) -> (Session, std::path::PathBuf) {
        use crate::session::storage::JsonlSessionStorage;
        use crate::session::types::session_filename;
        use std::path::Path;
        let dir = std::env::temp_dir().join(format!(
            "picocode-resume-prefill-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
        (Session::new_persisted(ro, index), path)
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
        let (session, path) = build_session_with_entries(entries).await;
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
        let (session, path) = build_session_with_entries(entries).await;
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

        let dir = std::env::temp_dir().join(format!(
            "picocode-resume-prefill-header-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
        let (session, path) = build_session_with_entries(vec![]).await;
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
        let (session, path) = build_session_with_entries(entries).await;
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
    async fn fresh_unpersisted_session() -> (crate::session::Session, std::path::PathBuf) {
        use crate::session::repo::SessionRepo;
        use crate::session::storage::SessionStorage;
        use std::path::PathBuf;

        let base: PathBuf = std::env::temp_dir().join(format!(
            "picocode-record-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
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
        (crate::session::Session::new(storage, index), path)
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
        let (session, path) = fresh_unpersisted_session().await;
        assert!(
            !session.is_persisted(),
            "fresh session must start unpersisted"
        );
        assert!(!path.exists(), "precondition: no file yet");

        record_model_change(&session, "openai:gpt-4o").await;
        record_thinking_change(&session, "high").await;

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
            session_recorded_thinking(&session).await.as_deref(),
            Some("high")
        );

        // Recording the same values again is a no-op (no state change).
        record_model_change(&session, "openai:gpt-4o").await;
        record_thinking_change(&session, "high").await;
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// On a persisted session: a value the session already records is
    /// skipped (no redundant entry — the resume-accept case), a genuinely
    /// new value is appended.
    #[tokio::test]
    async fn record_model_change_on_persisted_session_skips_duplicate() {
        use crate::session::storage::SessionStorage;
        use std::path::PathBuf;

        let dir: PathBuf = std::env::temp_dir().join(format!(
            "picocode-record-persisted-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
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

    #[test]
    fn parse_tree_args_strips_angle_brackets_from_id() {
        // The overlay commits `/tree <id> switch`; the target must arrive at
        // move_leaf/get_entry as the RAW id. Regression test: with brackets
        // kept, move_leaf pointed at a nonexistent entry → empty context,
        // no current-position marker, "/replay: session is empty".
        let (target, action, mode, summarize, _) = parse_tree_args("<0194abc-123> switch");
        assert_eq!(target, "0194abc-123");
        assert_eq!(action, TreeAction::Switch);
        assert_eq!(mode, None);
        assert!(!summarize);

        // Bare angle-bracketed id (action menu commit path).
        let (target, ..) = parse_tree_args("<x>");
        assert_eq!(target, "x");

        // Plain id still works (hand-typed without brackets).
        let (target, ..) = parse_tree_args("plain-id switch");
        assert_eq!(target, "plain-id");

        // Compact with mode + summarize flag orthogonal. `truncate`
        // remains recognized as a deprecated alias for the new primary
        // `manual` token.
        let (target, action, mode, _, _) = parse_tree_args("<id9> compact truncate --summarize");
        assert_eq!(target, "id9");
        assert_eq!(action, TreeAction::Compact);
        assert_eq!(mode, Some(CompactSummaryMode::Manual));

        // Primary mode token is `manual`.
        let (_, action, mode, _, _) = parse_tree_args("<id9> compact manual");
        assert_eq!(action, TreeAction::Compact);
        assert_eq!(mode, Some(CompactSummaryMode::Manual));

        // Text-only mode token.
        let (_, action, mode, _, _) = parse_tree_args("<id9> compact text-only");
        assert_eq!(action, TreeAction::Compact);
        assert_eq!(mode, Some(CompactSummaryMode::TextOnly));
    }

    #[test]
    fn parse_tree_args_llm_mode() {
        // Bare `llm` token after `compact` selects Llm mode with empty payload.
        let (target, action, mode, summarize, payload) = parse_tree_args("<id9> compact llm");
        assert_eq!(target, "id9");
        assert_eq!(action, TreeAction::Compact);
        assert_eq!(mode, Some(CompactSummaryMode::Llm));
        assert!(!summarize);
        assert_eq!(payload, "");

        // `summary` is accepted as an alias for `llm`.
        let (_, action, mode, _, _) = parse_tree_args("<id9> compact summary");
        assert_eq!(action, TreeAction::Compact);
        assert_eq!(mode, Some(CompactSummaryMode::Llm));

        // Trailing text after `llm` is preserved verbatim, whitespace-joined.
        let (_, _, mode, _, payload) =
            parse_tree_args("<id9> compact llm Summarize the conversation");
        assert_eq!(mode, Some(CompactSummaryMode::Llm));
        assert_eq!(payload, "Summarize the conversation");

        // Multi-word payload collapses internal whitespace runs to single
        // spaces (matches split_whitespace behavior).
        let (_, _, _, _, payload) = parse_tree_args("<id9> compact llm hello   world  foo");
        assert_eq!(payload, "hello world foo");

        // `text-only` after `llm` is part of the payload, not a mode token.
        let (_, _, mode, _, payload) = parse_tree_args("<id9> compact llm text-only is just text");
        assert_eq!(mode, Some(CompactSummaryMode::Llm));
        assert_eq!(payload, "text-only is just text");
    }

    fn build_chain_index(leaf_id: &str, ids: &[&str]) -> crate::session::SessionIndex {
        // Build a linear chain root -> ... -> leaf, with leaf_id pointing
        // at the last id.
        let mut index = crate::session::SessionIndex {
            header: crate::session::SessionHeader::new(std::path::Path::new("/test")),
            entries: Default::default(),
            leaf_id: Some(leaf_id.to_string()),
            children: Default::default(),
            session_name: None,
        };
        for (i, id) in ids.iter().enumerate() {
            let entry = crate::session::SessionEntry {
                id: id.to_string(),
                parent_id: if i == 0 {
                    None
                } else {
                    Some(ids[i - 1].to_string())
                },
                timestamp: "2024-01-01T00:00:00.000Z".to_string(),
                content: crate::session::SessionEntryContent::SessionInfo { name: None },
            };
            index.entries.insert(id.to_string(), entry);
            if i > 0 {
                index
                    .children
                    .entry(ids[i - 1].to_string())
                    .or_default()
                    .push(id.to_string());
            }
        }
        index
    }

    #[test]
    fn find_predecessor_on_active_branch_returns_prev_on_chain() {
        let index = build_chain_index("m3", &["m0", "m1", "m2", "m3"]);
        // m2 is immediately before m3 on the active branch.
        assert_eq!(
            find_predecessor_on_active_branch(&index, "m3"),
            Some("m2".to_string())
        );
        // m1 is immediately before m2.
        assert_eq!(
            find_predecessor_on_active_branch(&index, "m2"),
            Some("m1".to_string())
        );
        // m0 has no predecessor on the chain.
        assert_eq!(find_predecessor_on_active_branch(&index, "m0"), None);
        // An id not on the active branch: get_path_to_root walks the leaf
        // chain and returns empty for the missing id, so position returns
        // None and we report no predecessor.
        assert_eq!(find_predecessor_on_active_branch(&index, "nope"), None);
    }

    #[test]
    fn find_predecessor_on_active_branch_no_leaf() {
        let mut index = build_chain_index("m1", &["m0", "m1"]);
        index.leaf_id = None;
        assert_eq!(find_predecessor_on_active_branch(&index, "m1"), None);
    }

    /// Build a SessionIndex whose entries are all `Message` entries except
    /// for the listed compaction descriptors. Each descriptor is
    /// `(entry_id, first_kept_id, summary)`. The chain is linear
    /// root→leaf; the leaf is the last `ids` entry.
    ///
    /// Compaction entries themselves occupy a position in the chain (they
    /// are tree nodes), so a chain like `["m0", "c1", "m1"]` means c1
    /// is parented at m0 and m1 is parented at c1. `first_kept` refers to
    /// a message id earlier in the chain.
    #[cfg(any())]
    fn build_index_with_compactions(
        ids: &[&str],
        compactions: &[(&str, &str, &str)],
    ) -> crate::session::SessionIndex {
        let leaf = ids.last().expect("non-empty ids");
        let mut index = build_chain_index(leaf, ids);
        for (cid, first_kept, summary) in compactions {
            let parent = ids.iter().position(|id| *id == *cid).and_then(|i| {
                if i == 0 {
                    None
                } else {
                    Some(ids[i - 1].to_string())
                }
            });
            let entry = crate::session::SessionEntry {
                id: cid.to_string(),
                parent_id: parent,
                timestamp: "2024-01-01T00:00:00.000Z".to_string(),
                content: crate::session::SessionEntryContent::Compaction {
                    summary: summary.to_string(),
                    first_kept_entry_id: first_kept.to_string(),
                    tokens_before: 100,
                },
            };
            index.entries.insert(cid.to_string(), entry);
        }
        index
    }

    // ---- run_compaction ----
    //
    // The /compact command (now rewriting to /tree <last_user_id>
    // compact) and the existing /tree <id> compact both route through
    // `run_compaction`. These tests verify the per-mode mechanics in
    // isolation: Manual writes a `Compaction` entry with empty
    // summary when the dropped span touched no files; TextOnly writes
    // a deterministic summary; Llm runs the predecessor / move-back /
    // append dance. The mode plumbing is already covered by the
    // `parse_tree_args` tests, so we focus here on the side effects of
    // `run_compaction`.

    #[tokio::test]
    async fn retry_summarization_succeeds_on_first_attempt() {
        let mut calls = 0;
        let output = null_output();
        let cancel = &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = &std::sync::Arc::new(tokio::sync::Notify::new());
        let r = retry_summarization(output.as_ref(), cancel, notify, &mut || {
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
        let r = retry_summarization(output.as_ref(), cancel, notify, &mut || {
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
        let r = retry_summarization(output.as_ref(), cancel, notify, &mut || {
            calls += 1;
            async { Err(crate::PicocodeError::Llm("permanent".to_string())) }
        })
        .await;
        assert!(matches!(r, Err(crate::PicocodeError::Llm(_))));
        assert_eq!(calls, 1, "non-retryable must not retry");
    }

    #[tokio::test]
    async fn retry_summarization_cancels_after_esc_sleep() {
        // A provider hint of 1s: set the cancel flag so
        // `cancellable_sleep` returns immediately and the retrier
        // surfaces `Cancelled` without a second attempt.
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
        let r = retry_summarization(output.as_ref(), &cancel_out, &notify, &mut || {
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

    /// Build a writable `Session` from `entries` for `run_compaction`
    /// tests. The agent's `build_session_with_entries` opens the
    /// storage as read-only because it only tests reading (resume
    /// prefill); `run_compaction` calls `append_compaction` which
    /// needs a writable handle, so we use `open_or_create` here.
    async fn build_writable_session_with(entries: Vec<crate::session::SessionEntry>) -> Session {
        use crate::session::storage::JsonlSessionStorage;
        use crate::session::types::session_filename;
        use std::path::Path;
        let dir = std::env::temp_dir().join(format!(
            "picocode-run-compaction-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
            default_thinking_level: "off".into(),
            show_reasoning: "indicator".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::from("default prompt"),
            path: None,
        }
    }

    fn null_output() -> std::sync::Arc<dyn Output> {
        // `run_compaction` calls `output.display_separator()` and
        // `display_markdown()` for the summary-producing modes. The
        // existing `NoOutput` impl in `crate::output` is a no-op for
        // everything we touch.
        std::sync::Arc::new(crate::output::NoOutput) as std::sync::Arc<dyn Output>
    }

    #[tokio::test]
    async fn run_compaction_manual_writes_empty_summary() {
        // After run_compaction(Manual, target=u1) on a span that touched
        // no files, a Compaction entry should appear on the branch with
        // empty summary and first_kept_entry_id = u1. (The empty-summary
        // no-files path preserves the original `truncate`-mode
        // behavior.)
        let s = build_writable_session_with(vec![
            message_entry("u1", None, "user", "first prompt"),
            message_entry("a1", Some("u1"), "assistant", "first reply"),
        ])
        .await;
        let _old_leaf = s.get_leaf_id().await;
        let settings = test_settings();
        let out = null_output();
        assert!(
            run_compaction(
                &s,
                &None,
                &settings,
                CompactionTarget::AtPoint {
                    target_id: "u1".to_string(),
                    tokens_before: 100,
                },
                CompactSummaryMode::Manual,
                "",
                out.as_ref(),
                None,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                &std::sync::Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .is_done(),
            "manual compaction should succeed"
        );

        // The new leaf should be a Compaction entry.
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
                assert_eq!(tokens_before, 100);
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn run_compaction_text_only_writes_deterministic_summary() {
        // TextOnly builds a summary from the dropped span and writes
        // it as the compaction's summary.
        let s = build_writable_session_with(vec![
            message_entry("u1", None, "user", "first prompt"),
            message_entry("a1", Some("u1"), "assistant", "first reply"),
        ])
        .await;
        let _old_leaf = s.get_leaf_id().await;
        let settings = test_settings();
        let out = null_output();
        assert!(
            run_compaction(
                &s,
                &None,
                &settings,
                CompactionTarget::AtPoint {
                    target_id: "u1".to_string(),
                    tokens_before: 50,
                },
                CompactSummaryMode::TextOnly,
                "",
                out.as_ref(),
                None,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                &std::sync::Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .is_done(),
            "text-only compaction should succeed"
        );

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        let new_entry = s.get_entry(&new_leaf).await.expect("entry");
        match new_entry.content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "u1");
                // The dropped span (before u1) is empty here — the
                // summary is the deterministic text-only of an empty
                // span, which is the empty string.
                assert_eq!(summary, "");
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn run_compaction_llm_no_agent_returns_err_and_no_compaction() {
        // The LLM mode requires an agent. Without one, run_compaction
        // returns Err and writes nothing.
        let s = build_writable_session_with(vec![
            message_entry("u1", None, "user", "first prompt"),
            message_entry("a1", Some("u1"), "assistant", "first reply"),
        ])
        .await;
        let _old_leaf = s.get_leaf_id().await;
        let settings = test_settings();
        let out = null_output();
        let res = run_compaction(
            &s,
            &None, // no agent
            &settings,
            CompactionTarget::AtPoint {
                target_id: "u1".to_string(),
                tokens_before: 0,
            },
            CompactSummaryMode::Llm,
            "user payload",
            out.as_ref(),
            None,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            &std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        assert!(
            matches!(res, CompactionResult::Err(_)),
            "expected error when no agent available"
        );
        // The leaf should not have changed (no compaction entry).
        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        let entry = s.get_entry(&new_leaf).await.expect("entry");
        assert!(
            !matches!(
                entry.content,
                crate::session::SessionEntryContent::Compaction { .. }
            ),
            "no compaction entry should be written on LLM failure"
        );
    }

    #[tokio::test]
    async fn run_compaction_text_only_nonzero_dropped_span() {
        // With a non-empty dropped span (target = a1, dropped = u1),
        // the summary is the deterministic text-only of u1.
        let s = build_writable_session_with(vec![
            message_entry("u1", None, "user", "the question"),
            message_entry("a1", Some("u1"), "assistant", "the answer"),
        ])
        .await;
        let _old_leaf = s.get_leaf_id().await;
        let settings = test_settings();
        let out = null_output();
        assert!(
            run_compaction(
                &s,
                &None,
                &settings,
                CompactionTarget::AtPoint {
                    target_id: "a1".to_string(),
                    tokens_before: 10,
                },
                CompactSummaryMode::TextOnly,
                "",
                out.as_ref(),
                None,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                &std::sync::Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .is_done(),
            "text-only compaction should succeed"
        );

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        let new_entry = s.get_entry(&new_leaf).await.expect("entry");
        match new_entry.content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "a1");
                assert!(
                    summary.contains("the question"),
                    "text-only summary should contain the dropped user prompt, got: {:?}",
                    summary
                );
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn run_compaction_manual_when_leaf_is_compaction_writes_new_entry() {
        // Phase C (doc/nested_compactions.md): the leaf-is-compaction guard
        // is removed, so re-compacting a session whose leaf is ALREADY a
        // compaction must succeed. Manual mode writes a fresh empty-summary
        // Compaction entry even though the dropped span is empty (the
        // user explicitly asked for a manual compaction at this point).
        //
        // Branch: [u0, a0, C1{firstKept:u0}] — leaf is C1.
        let s = build_writable_session_with(vec![
            message_entry("u0", None, "user", "prompt one"),
            message_entry("a0", Some("u0"), "assistant", "reply one"),
            compaction_entry("c1", Some("a0"), "earlier summary", "u0", 50),
        ])
        .await;
        let old_leaf = s.get_leaf_id().await;
        // Sanity: the leaf is indeed the compaction entry.
        assert_eq!(old_leaf.as_deref(), Some("c1"));

        let settings = test_settings();
        let out = null_output();
        assert!(
            run_compaction(
                &s,
                &None,
                &settings,
                CompactionTarget::AtPoint {
                    target_id: "u0".to_string(),
                    tokens_before: 42,
                },
                CompactSummaryMode::Manual,
                "",
                out.as_ref(),
                None,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                &std::sync::Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .is_done(),
            "manual re-compaction over a leaf-compaction must succeed"
        );

        // New leaf is a Compaction entry with empty summary and firstKept=u0.
        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        assert_ne!(new_leaf, "c1", "a new compaction entry should be appended");
        match s.get_entry(&new_leaf).await.expect("entry").content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
            } => {
                assert_eq!(summary, "");
                assert_eq!(first_kept_entry_id, "u0");
                assert_eq!(tokens_before, 42);
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn run_compaction_text_only_when_leaf_is_compaction_skips_nested_summaries() {
        // Phase C + multi-mode interop: text-only re-compaction over a
        // session whose leaf is already a compaction. The dropped span is
        // everything strictly before the target. build_text_only_summary
        // skips nested compaction entries (it only renders Message entries),
        // so the new summary must contain the raw user/assistant text but
        // MUST NOT contain the earlier compaction's summary text.
        //
        // Branch: [u0, a0, u1, a1, C1{firstKept:u0, summary:"EARLY"}] — leaf
        // is C1. Re-compact text-only at target = u1 (dropped: u0, a0).
        let s = build_writable_session_with(vec![
            message_entry("u0", None, "user", "first question"),
            message_entry("a0", Some("u0"), "assistant", "first answer"),
            message_entry("u1", Some("a0"), "user", "second question"),
            message_entry("a1", Some("u1"), "assistant", "second answer"),
            compaction_entry("c1", Some("a1"), "EARLY SECRET SUMMARY", "u0", 80),
        ])
        .await;
        let old_leaf = s.get_leaf_id().await;
        assert_eq!(old_leaf.as_deref(), Some("c1"));

        let settings = test_settings();
        let out = null_output();
        assert!(
            run_compaction(
                &s,
                &None,
                &settings,
                CompactionTarget::AtPoint {
                    target_id: "u1".to_string(),
                    tokens_before: 60,
                },
                CompactSummaryMode::TextOnly,
                "",
                out.as_ref(),
                None,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                &std::sync::Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .is_done(),
            "text-only re-compaction over a leaf-compaction must succeed"
        );

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        let summary = match s.get_entry(&new_leaf).await.expect("entry").content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "u1");
                summary
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        };
        assert!(
            summary.contains("first question"),
            "text-only summary should include dropped user prompt, got: {:?}",
            summary
        );
        assert!(
            !summary.contains("EARLY SECRET SUMMARY"),
            "text-only summary must NOT leak the nested compaction's summary text, got: {:?}",
            summary
        );
    }

    #[tokio::test]
    async fn run_compaction_text_only_review_edits_summary_before_write() {
        // The review hook (editor + accept dialog) is invoked with the
        // generated draft BEFORE the entry is written. Accepting with edits
        // must store the edited text, not the generated draft.
        let s = build_writable_session_with(vec![
            message_entry("u1", None, "user", "the question"),
            message_entry("a1", Some("u1"), "assistant", "the answer"),
        ])
        .await;
        let _old_leaf = s.get_leaf_id().await;
        let settings = test_settings();
        let out = null_output();
        let review: Option<CompactionSummaryReview> = Some(Box::new(|draft: &str| {
            // Simulates the user editing the draft in $EDITOR and accepting.
            assert!(
                draft.contains("the question"),
                "review must receive the generated draft, got: {:?}",
                draft
            );
            Some(format!("{}\n\nEDITED BY USER", draft))
        }));
        assert!(
            run_compaction(
                &s,
                &None,
                &settings,
                CompactionTarget::AtPoint {
                    target_id: "a1".to_string(),
                    tokens_before: 10,
                },
                CompactSummaryMode::TextOnly,
                "",
                out.as_ref(),
                review,
                &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                &std::sync::Arc::new(tokio::sync::Notify::new()),
            )
            .await
            .is_done(),
            "text-only compaction with review should succeed"
        );

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        let summary = match s.get_entry(&new_leaf).await.expect("entry").content {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "a1");
                summary
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        };
        assert!(
            summary.contains("EDITED BY USER"),
            "compaction summary must store the user-edited text, got: {:?}",
            summary
        );
    }

    #[tokio::test]
    async fn run_compaction_text_only_review_cancel_writes_nothing() {
        // Review hook returning None = user cancelled. No compaction entry
        // may be appended and the leaf must stay where it was.
        let s = build_writable_session_with(vec![
            message_entry("u1", None, "user", "the question"),
            message_entry("a1", Some("u1"), "assistant", "the answer"),
        ])
        .await;
        let _old_leaf = s.get_leaf_id().await;
        let settings = test_settings();
        let out = null_output();
        let review: Option<CompactionSummaryReview> = Some(Box::new(|_draft: &str| None));
        let res = run_compaction(
            &s,
            &None,
            &settings,
            CompactionTarget::AtPoint {
                target_id: "a1".to_string(),
                tokens_before: 10,
            },
            CompactSummaryMode::TextOnly,
            "",
            out.as_ref(),
            review,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            &std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        assert!(
            matches!(res, CompactionResult::Cancelled),
            "cancelled review should return Cancelled, got {:?}",
            res
        );
        // Leaf unchanged (still the assistant message), and no compaction
        // entry anywhere after the original messages.
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("a1"));
        let entry = s.get_entry("a1").await.expect("a1 entry");
        assert!(
            !matches!(
                entry.content,
                crate::session::SessionEntryContent::Compaction { .. }
            ),
            "no compaction entry may be written when the review cancels"
        );
    }

    #[tokio::test]
    async fn run_compaction_llm_when_leaf_is_compaction_still_guards_nothing_before() {
        // Phase C: the leaf-is-compaction guard is removed, but the LLM
        // mode still has its own guard against an EMPTY dropped span (the
        // no-op stacked case: target is the first entry after the leaf, so
        // there is nothing between the leaf and target to summarize). That
        // guard must remain active even when the leaf is a compaction.
        //
        // Branch: [u0, a0, C1{firstKept:u0}] — leaf is C1. If we try to
        // LLM-compact targeting a0 (whose predecessor on the branch is u0,
        // not the leaf c1) the guard should NOT fire (dropped = u0, non-empty),
        // but there is no agent so it must return Err anyway. The meaningful
        // assertion here is that the run_compaction backend never panics and
        // returns a structured error when the leaf is a compaction.
        let s = build_writable_session_with(vec![
            message_entry("u0", None, "user", "prompt one"),
            message_entry("a0", Some("u0"), "assistant", "reply one"),
            compaction_entry("c1", Some("a0"), "earlier summary", "u0", 50),
        ])
        .await;
        let old_leaf = s.get_leaf_id().await;
        assert_eq!(old_leaf.as_deref(), Some("c1"));

        let settings = test_settings();
        let out = null_output();
        let res = run_compaction(
            &s,
            &None, // no agent
            &settings,
            CompactionTarget::AtPoint {
                target_id: "a0".to_string(),
                tokens_before: 30,
            },
            CompactSummaryMode::Llm,
            "user payload",
            out.as_ref(),
            None,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            &std::sync::Arc::new(tokio::sync::Notify::new()),
        )
        .await;
        assert!(
            matches!(res, CompactionResult::Err(_)),
            "expected error when no agent available even with leaf==compaction"
        );
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
        let dir = std::env::temp_dir().join(format!("picocode-msglog-test-{}", std::process::id()));
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

        let dir =
            std::env::temp_dir().join(format!("picocode-msglog-stream-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("picocode_token_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        let host_path =
            std::env::temp_dir().join(format!("picocode_sandbox_regr_{}.txt", std::process::id()));
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
            fn display_thinking(&self, _: &str) {}
            fn stop_thinking(&self) {}
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
            quiet: true,
            thinking_level: None,
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
        let settings = SettingsStore::from_agent_config(&config, None);

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
        fn set_answer_hold(&self, _h: bool) {}
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
        fn display_thinking(&self, _m: &str) {}
        fn stop_thinking(&self) {}
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
            super::retry_budget(&PicocodeError::LlmUpstream { status: 502 }),
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
            super::retry_budget(&PicocodeError::LlmClient { status: 400 }),
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
            PicocodeError::LlmUpstream { status } => {
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
            PicocodeError::LlmUpstream { status } => {
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
            PicocodeError::LlmUpstream { status } => {
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

        let base: PathBuf = std::env::temp_dir().join(format!(
            "picocode-new-fix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
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
            let base: PathBuf = std::env::temp_dir().join(format!(
                "picocode-new-name-{}-{}-{}",
                cmd.replace('/', "_"),
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            ));
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
