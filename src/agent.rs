use crate::output::Confirmation;
use crate::repl::{InputAction, Repl, ReplCtx};
use crate::settings::SettingsStore;
use crate::session::Session;
use crate::session::repo::SessionRepo;
use crate::session::storage::SessionStorage;
use crate::tools::{
    set_sandbox, AgentBrowser, Bash, CopyFile, EditFile, GlobFiles, GrepText, ListDir, MakeDir,
    MoveFile, ReadFile, Remove, WriteFile,
};
use crate::Output;
use crate::Result;
use crate::model_catalog;
use crate::thinking;
use console::style;
use rig_core::agent::{
    Agent, AgentBuilder, AgentHook, CompletionCallAction, CompletionCallEvent, CompletionResponseEvent,
    HookContext, ModelTurnAction, ModelTurnFinished, ObservationAction, StreamingError, ToolCall,
    ToolCallAction, ToolResultAction, ToolResultEvent,
};
use rig_core::tool::ToolContext;
use rig_core::client::{CompletionClient, ProviderClient};
use rig_core::completion::{CompletionModel, PromptError};
use rig_core::message::Message;
use rig_core::message::{AssistantContent, Text, ToolResult, ToolResultContent, UserContent};
use rig_core::OneOrMany;
use rig_core::agent::MultiTurnStreamItem;
use rig_core::message::ReasoningContent;
use rig_core::streaming::{StreamedAssistantContent, StreamingPrompt};
use futures::StreamExt;
use rig_core::providers::{
    anthropic, azure, chatgpt, cohere, copilot, doubleword, deepseek, gemini, groq, huggingface, hyperbolic, llamafile, mira,
    minimax, mistral, moonshot, ollama, openai, openrouter, perplexity, together, xai, xiaomimimo, zai,
};
use serde_json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::io::IsTerminal;
use std::path::Path;
use std::sync::Arc;
use async_trait::async_trait;

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
    async fn prompt(&self, input: &str, session: Option<&Session>) -> Result<String>;
    async fn run_once(&self, input: String) -> Result<String>;
    /// Snapshot of the accumulated session token usage (for `/stat`). A zeroed
    /// result means no completion call has reported usage yet.
    fn session_usage(&self) -> SessionUsage;
    /// Generate a concise summary of the given text using the agent's model.
    /// Used by `/tree --summarize` to summarise abandoned branches.
    async fn summarize(&self, prompt: &str) -> Result<String>;
}

#[async_trait]
impl<M: CompletionModel + 'static> PicoAgent for CodeAgent<M> {
    async fn prompt(
        &self,
        input: &str,
        session: Option<&Session>,
    ) -> Result<String> {
        self.output.display_thinking("Thinking...");
        // Fresh request: clear any stale cancel flag from a previous abort so a
        // prior `Esc` doesn't silently cancel this prompt. The REPL's in-flight
        // reader sets `cancel` on `<Esc>` and is always joined (after `done` is
        // set) before the next `prompt()` begins, so a leftover `cancel` can
        // only survive while the reader is still running — which it never is at
        // this point. See doc/turn_cancellation_persistence_bugs.md §3.
        self.user_cancel.store(false, Ordering::SeqCst);

        // Build prior context from session, or use empty vec as fallback.
        let prior: Vec<Message> = if let Some(s) = session {
            match s.build_context().await {
                Ok(ctx) => {
                    // Apply reasoning stripping to the LLM-bound context.
                    // The session stores pre-strip messages; stripping is
                    // applied here before sending to the service.
                    maybe_strip_reasoning(
                        ctx.messages,
                        self.reasoning_in_context,
                        &self.provider,
                        &self.model,
                    )
                }
                Err(e) => {
                    // Session index may be stale; log and fall back to empty.
                    eprintln!("Warning: session build_context failed: {}", e);
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        // `stream_prompt(...).await` yields the item stream (rig 0.40's
        // `IntoFuture` returns the stream itself, not a collected response).
        let mut stream = self
            .agent
            .stream_prompt(input)
            .add_hook(LoggingHook {
                output: self.output.clone(),
                user_cancel: self.user_cancel.clone(),
            })
            .max_turns(self.tool_call_limit)
            .history(prior)
            .await;

        let mut output = String::new();
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
        let reasoning_expected = self
            .thinking_level
            .as_deref()
            .and_then(|s| s.parse::<thinking::ThinkingLevel>().ok())
            .map(|l| l != thinking::ThinkingLevel::Off)
            .unwrap_or(false);
        self.output.set_answer_hold(reasoning_expected);
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
                            StreamedAssistantContent::Reasoning(reasoning) => {
                                let t = reasoning_text(&reasoning);
                                if !t.is_empty() {
                                    self.output.stream_thinking(&t);
                                }
                            }
                            StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                                if !reasoning.is_empty() {
                                    self.output.stream_thinking(&reasoning);
                                }
                            }
                            _ => {}
                        },
                        Some(Ok(MultiTurnStreamItem::StreamUserItem(_))) => {}
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
                        Some(Ok(_)) => {}
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
                            // Instead, persist only the user prompt and any partial
                            // assistant TEXT that was streamed before the cancellation.
                            // Tool calls are not accumulated in `output` (they arrive
                            // as separate stream events), so the partial assistant
                            // message is guaranteed to be text-only — safe for the
                            // next turn's context.
                            if let StreamingError::Prompt(prompt_err) = &e {
                                if let PromptError::PromptCancelled { .. } = prompt_err.as_ref() {
                                    let mut partial = vec![Message::user(input)];
                                    if !output.is_empty() {
                                        partial.push(Message::assistant(&output));
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
                            if let Some(s) = session {
                                let user_msg = Message::user(input);
                                let _ = s.append_message(&user_msg).await;
                                if !output.is_empty() {
                                    let partial = Message::assistant(&output);
                                    let _ = s.append_message(&partial).await;
                                }
                            }
                            self.output.end_response();
                            return Err(crate::PicocodeError::Other(e.to_string()));
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
                        // Capture partial turn for persistence.
                        let mut partial = vec![Message::user(input)];
                        if !output.is_empty() {
                            partial.push(Message::assistant(&output));
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
                let to_commit = normalize_call_ids(fh);
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
        let response = self.prompt(&input, None).await?;
        Ok(response)
    }

    fn session_usage(&self) -> SessionUsage {
        self.usage
            .lock()
            .map(|g| *g)
            .unwrap_or_default()
    }

    async fn summarize(&self, prompt: &str) -> Result<String> {
        use rig_core::completion::Chat;
        let msg = Message::user(prompt);
        let mut history: Vec<Message> = vec![];
        self.agent
            .chat(msg, &mut history)
            .await
            .map_err(|e| crate::PicocodeError::Other(format!("summarize: {}", e)))
    }
}

/// Flatten a rig `Reasoning` payload into plain text. Provider-specific
/// encrypted/redacted blocks are kept verbatim; other shapes are ignored.
fn reasoning_text(r: &rig_core::message::Reasoning) -> String {
    r.content
        .iter()
        .map(|c| match c {
            ReasoningContent::Text { text, .. } => text.clone(),
            ReasoningContent::Summary(s) => s.clone(),
            ReasoningContent::Encrypted(s) => s.clone(),
            ReasoningContent::Redacted { data } => data.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

    /// Ensure every `ToolResult` in the given messages has `call_id` set.
    ///
    /// If `call_id` is `None`, set it to the tool result's own `id`. This is
    /// the Phase 2b fix for cross-provider safety: the OpenAI Responses API
    /// requires `call_id` to be present on every tool result, and normalising
    /// at persist time avoids the need for pi's `transformMessages` layer.
    /// See doc/pi_steering_rig.md §6.5.4.
    fn normalize_call_ids(messages: Vec<Message>) -> Vec<Message> {
        messages.into_iter().map(|m| match m {
            Message::User { content } => {
                let items: Vec<UserContent> = content.into_iter().map(|c| match c {
                    UserContent::ToolResult(tr) => {
                        UserContent::ToolResult(ToolResult {
                            call_id: tr.call_id.or_else(|| Some(tr.id.clone())),
                            ..tr
                        })
                    }
                    other => other,
                }).collect();
                Message::User {
                    content: OneOrMany::many(items)
                        .expect("OneOrMany never empty after iterating over non-empty content"),
                }
            }
            other => other,
        }).collect()
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
        reasoning_in_context: bool,
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

    /// Strip `AssistantContent::Reasoning` blocks when reasoning should *not* be
    /// kept in context. Returns the messages unchanged when `reasoning_in_context`
    /// is `true` or the provider is Anthropic-family **and the turn contains tool
    /// calls** (the thinking+tool_use replay contract). Assistant messages left
    /// empty by the filter get a placeholder `Text` so the message stays
    /// well-formed and user/assistant alternation holds.
    fn maybe_strip_reasoning(
        messages: Vec<Message>,
        reasoning_in_context: bool,
        provider: &str,
        model: &str,
    ) -> Vec<Message> {
        if reasoning_in_context
            || (is_anthropic_family(provider, model) && turn_has_tool_calls(&messages))
        {
            return messages;
        }
        messages
            .into_iter()
            .map(|m| match m {
                Message::Assistant { id, content } => {
                    let kept: Vec<AssistantContent> = content
                        .into_iter()
                        .filter(|c| !matches!(c, AssistantContent::Reasoning(_)))
                        .collect();
                    let content = if kept.is_empty() {
                        OneOrMany::one(AssistantContent::Text(Text::new("[reasoning stripped]")))
                    } else {
                        OneOrMany::many(kept)
                            .expect("assistant content non-empty after filtering reasoning")
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

pub struct CodeAgent<M: CompletionModel> {
    agent: Agent<M>,
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
    reasoning_in_context: bool,
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
    /// Whether reasoning blocks are kept in the stored session context. `false`
    /// drops them at store time (cheap token savings); forced `true` for
    /// Anthropic-family providers (thinking+tool_use replay contract).
    pub reasoning_in_context: bool,
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
                        .build().map_err(rig_core::client::ProviderClientError::from)?,
                    None => chatgpt::Client::builder()
                        .oauth()
                        .allow_device_flow(true)
                        .build().map_err(rig_core::client::ProviderClientError::from)?,
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
                        .build().map_err(rig_core::client::ProviderClientError::from)?,
                    None => copilot::Client::builder()
                        .oauth()
                        .allow_device_flow(true)
                        .build().map_err(rig_core::client::ProviderClientError::from)?,
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
        "llamafile" => {
            check_env!("LLAMAFILE_API_BASE_URL");
            build!(llamafile::Client::from_env()?)
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
}

impl AgentHook for LoggingHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        _event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        if self.user_cancel.load(Ordering::SeqCst) {
            CompletionCallAction::Stop("cancelled by user".into())
        } else {
            CompletionCallAction::Continue
        }
    }

    async fn on_completion_response(
        &self,
        _ctx: &HookContext,
        _event: CompletionResponseEvent<'_>,
    ) -> ObservationAction {
        if self.user_cancel.load(Ordering::SeqCst) {
            ObservationAction::Stop("cancelled by user".into())
        } else {
            ObservationAction::Continue
        }
    }

    async fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        _event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        if self.user_cancel.load(Ordering::SeqCst) {
            ModelTurnAction::Stop("cancelled by user".into())
        } else {
            ModelTurnAction::Continue
        }
    }

    async fn on_tool_call(
        &self,
        _ctx: &HookContext,
        event: ToolCall<'_>,
    ) -> ToolCallAction {
        if self.user_cancel.load(Ordering::SeqCst) {
            return ToolCallAction::Stop("cancelled by user".into());
        }
        let args_json = serde_json::from_str(event.args)
            .unwrap_or(serde_json::Value::String(event.args.to_string()));
        self.output.display_tool_call(event.tool_name, &args_json);
        ToolCallAction::Run
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

const DEFAULT_AGENT_PROMPT: &str = r#"You are picocode, a world-class software engineering agent. You are direct, technical, and highly efficient.

Your mission is to assist the user in their development tasks by utilizing a set of specialized tools. You operate within a specific codebase and must maintain its integrity while delivering high-quality solutions.

### WORKFLOW & STRATEGY
1. **Understand Before Acting**: Always start by exploring the codebase. Use `list_dir` to see the structure and `read_file` or `grep_text` to understand existing logic and patterns.
2. **Be Precise**: When editing files, use `edit_file` with enough context in `old_string` to ensure a unique match. Avoid replacing large blocks if a small change suffices.
3. **Verify Everything**: After modifying code, verify the results. Run tests or build commands via `bash`. Read the modified file to ensure the change was applied correctly.
4. **Tool Mastery**:
   - `read_file`: Use to read code. Note that it provides line numbers (e.g., `  10| code`). These are for your reference only; do not include them in your output or when writing files.
   - `bash`: Your window to the system. Use it for compilation, testing, and complex automation.
   - `w3m in the shell`: Use for external documentation, searching for solutions, or web-related debugging.
5. **Context**: You are working in the directory provided below. All paths are relative to this directory.

### GUIDING PRINCIPLES
- **Clean Code**: Follow established patterns in the codebase. Write idiomatic, readable, and well-documented code.
- **Security First**: Be vigilant about security vulnerabilities. Sanitize inputs, avoid hardcoded secrets, and follow least-privilege principles.
- **Minimalism**: Don't add unnecessary dependencies or over-engineer solutions.
- **Communication**: Keep explanations brief and focused on the "how" and "why" of your technical decisions.
"#;

fn build_rig_agent<M: CompletionModel>(
    builder: AgentBuilder<M>,
    yolo: bool,
    output: Arc<dyn Output>,
    system_message_extension: Option<String>,
    persona_prompt: Option<String>,
    bash_auto_allow: Vec<String>,
    agent_prompt: Option<String>,
    additional_params: Option<serde_json::Value>,
    sandbox: Option<crate::config::SandboxConfig>,
) -> Agent<M> {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut system_message = agent_prompt.unwrap_or_else(|| {
        format!("{}\n\nCurrent working directory: {}", DEFAULT_AGENT_PROMPT, cwd)
    });
    if let Some(persona) = persona_prompt {
        system_message = format!("{}\n\n{}", persona, system_message);
    }
    if let Some(ext) = system_message_extension {
        system_message.push_str("\n\n");
        system_message.push_str(&ext);
    }

    let mut builder = builder
        .preamble(&system_message)
        .tool(ReadFile)
        .tool(WriteFile)
        .tool(EditFile)
        .tool(GlobFiles)
        .tool(GrepText)
        .tool(ListDir);

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
                    .map(|re| re.is_match(&args.cmd))
                    .unwrap_or(false)
            })
        })),
    ));

    if is_tool_available("agent-browser") {
        builder = builder.tool(guard(AgentBrowser, yolo, output.clone(), None));
    }
    builder.build()
}

use rig_core::tool::Tool;

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
/// Returns `(sessions list, branch message previews, session tree, leaf_id)`.
async fn build_session_ctx(
    session: &Option<Session>,
    repo: &Option<crate::session::repo::JsonlSessionRepo>,
) -> (
    Vec<crate::repl::SessionMeta>,
    Vec<(String, String)>,
    Vec<crate::session::SessionTreeNode>,
    Option<String>,
) {
    let sessions = if let Some(repo) = repo {
        let cwd = std::env::current_dir().ok();
        repo.list(cwd.as_deref())
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| crate::repl::SessionMeta {
                id: m.id,
                name: m.name,
                timestamp: m.timestamp,
                cwd: m.cwd,
                parent_session: m.parent_session.map(|p| p.display().to_string()),
                message_count: m.message_count,
                first_prompt_preview: m.first_prompt_preview,
            })
            .collect()
    } else {
        Vec::new()
    };

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

    let (tree, leaf_id) = match session {
        Some(s) => (s.get_tree().await, s.get_leaf_id().await),
        None => (Vec::new(), None),
    };

    (sessions, branch_messages, tree, leaf_id)
}

/// Result of handling a `/session` command.
enum SessionCommandResult {
    /// Input was not a session command (pass through).
    NotSession,
    /// Session command handled, current session unchanged.
    Handled,
    /// Session command created/resumed a new session with this ID.
    Switched(String),
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
struct AssistantSegment {
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
struct ConversationTurn {
    user_text: String,
    segments: Vec<AssistantSegment>,
}

/// Group a flat `Vec<Message>` (from `SessionContext::messages`) into replay
/// turns.
///
/// A `User { Text }` opens a new `ConversationTurn`. A `User { ToolResult }`
/// populates `tool_results` on the most recent assistant segment — these
/// always follow the assistant message that issued the calls. An `Assistant`
/// message appends a new segment to the current turn.
fn group_into_turns(messages: &[Message]) -> Vec<ConversationTurn> {
    let mut turns: Vec<ConversationTurn> = Vec::new();

    for msg in messages {
        match msg {
            Message::User { content } => {
                let mut user_text: Option<String> = None;
                let mut tool_results: Vec<(String, String)> = Vec::new();

                for item in content.iter() {
                    match item {
                        UserContent::Text(t) if user_text.is_none() => {
                            user_text = Some(t.text.clone());
                        }
                        UserContent::ToolResult(tr) => {
                            let text = tr
                                .content
                                .iter()
                                .filter_map(|rc| match rc {
                                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            tool_results.push((tr.id.clone(), text));
                        }
                        _ => {}
                    }
                }

                if let Some(txt) = user_text {
                    // New conversation turn.
                    turns.push(ConversationTurn { user_text: txt, segments: Vec::new() });
                } else if !tool_results.is_empty() {
                    // Attach to the most recent segment of the current turn.
                    if let Some(turn) = turns.last_mut() {
                        if let Some(seg) = turn.segments.last_mut() {
                            seg.tool_results.extend(tool_results);
                        }
                    }
                }
            }
            Message::Assistant { content, .. } => {
                if let Some(turn) = turns.last_mut() {
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
                                    tc.id.clone(),
                                    tc.function.name.clone(),
                                    tc.function.arguments.clone(),
                                ));
                            }
                            _ => {}
                        }
                    }
                    turn.segments.push(seg);
                }
            }
            _ => {}
        }
    }

    turns
}

/// Replay stored session turns through the same Output event sequence the live
/// streaming path uses (`PicoAgent::prompt`). Groups the flat message list
/// into conversation turns, then drives:
///
///   `begin_response` / `set_answer_hold` /
///   `stream_thinking` / `stream_text` /
///   `end_response_segment` / `display_tool_call` / `display_tool_result` /
///   `end_response`
///
/// in the identical order the live path produces, so `ConsoleOutput` renders
/// replayed content exactly as it rendered the original live session.
fn replay_recent_turns(
    output: &Arc<dyn Output>,
    ctx: &crate::session::SessionContext,
    replay_turns: usize,
) {
    let turns = group_into_turns(&ctx.messages);
    // Take the N most recent complete turns (accurate count, not a raw-message
    // estimate like the old `len - replay_turns * 2 + 1` heuristic).
    let start = turns.len().saturating_sub(replay_turns);

    for (turn_idx, turn) in turns[start..].iter().enumerate() {
        // Separator before each user prompt mirrors the top-of-loop separator
        // in `run_interactive`. Skip it for the very first replayed turn: the
        // loop already emitted one before the `/session resume` input was read.
        if turn_idx > 0 {
            output.display_separator();
        }
        output.display_plain(&turn.user_text);

        if turn.segments.is_empty() {
            continue;
        }

        // Pre-response separator: mirrors `run_interactive`'s separator between
        // the submitted user prompt and the start of the agent response.
        output.display_separator();

        output.begin_response();
        // Replay is non-streaming; content is already in the correct order.
        // Answer-hold exists to reorder live-stream content that arrives out of
        // sequence (reasoning after answer); it is never needed in replay.
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
                // Flush the current text/reasoning block before the tool panel.
                // Mirrors the `CompletionCall` stream event that fires in the
                // live path immediately before `LoggingHook::on_tool_call`.
                output.end_response_segment();
                for (call_id, name, args) in &segment.tool_calls {
                    output.display_tool_call(name, args);
                    // Emit the matching result immediately after its call, so
                    // `ConsoleOutput::last_tool` is always set when the result
                    // renders (drives language guessing and gutter stripping).
                    // A cancelled turn may have calls with no stored result;
                    // skip `display_tool_result` rather than showing garbage.
                    if let Some((_, result_text)) =
                        segment.tool_results.iter().find(|(rid, _)| rid == call_id)
                    {
                        output.display_tool_result(result_text);
                    }
                }
            } else if !is_last_segment {
                // Defensive: a segment with no tool calls but more segments
                // follow (shouldn't occur in a well-formed session). Flush to
                // keep the renderer state clean.
                output.end_response_segment();
            }
        }

        output.end_response();
    }
}

/// Parse and dispatch a `/session <name> <action>` command.

/// Parse and dispatch a `/session <name> <action>` command.
async fn handle_session_command(
    input: &str,
    session: &mut Option<Session>,
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
        let cwd = std::env::current_dir().ok();
        repo.list(cwd.as_deref())
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
                // Parse optional replay turn count from argument.
                let replay_turns = arg
                    .as_deref()
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(settings.session_resume_replay_turns);

                match repo.as_ref().unwrap().open(&meta).await {
                    Ok((storage, index)) => {
                        let new_session = Session::new_persisted(storage, index);
                        let id = new_session.get_session_id().to_string();

                        // Re-play recent turns through the full output path.
                        if replay_turns > 0 {
                            if let Ok(ctx) = new_session.build_context().await {
                                // Separator between the echoed command line and
                                // the replayed content, matching the separator
                                // run_interactive emits before each agent response.
                                output.display_separator();
                                replay_recent_turns(output, &ctx, replay_turns);
                            }
                        }

                        *session = Some(new_session);
                        SessionCommandResult::Switched(id)
                    }
                    Err(e) => {
                        config.output.display_text(&format!("Cannot resume session: {}", e));
                        SessionCommandResult::Handled
                    }
                }
            } else {
                config.output.display_text(&format!("Session not found: {}", name));
                SessionCommandResult::Handled
            }
        }
        "fork" => {
            if let Some(meta) = meta {
                let cwd = match std::env::current_dir() {
                    Ok(c) => c,
                    Err(_) => {
                        config.output.display_text("Cannot determine current directory");
                        return SessionCommandResult::Handled;
                    }
                };
                match repo.as_ref().unwrap().fork(&meta, "", &cwd).await {
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
                        config.output.display_plain(&format!(
                            "/session {} fork",
                            meta.name.as_deref().unwrap_or(&meta.id)
                        ));
                        *session = Some(new_session);
                        SessionCommandResult::Switched(id)
                    }
                    Err(e) => {
                        config.output.display_text(&format!("Cannot fork session: {}", e));
                        SessionCommandResult::Handled
                    }
                }
            } else {
                config.output.display_text(&format!("Session not found: {}", name));
                SessionCommandResult::Handled
            }
        }
        "rename" => {
            let new_name = arg.unwrap_or_default();
            if new_name.is_empty() {
                config.output.display_text("Usage: /session <name> rename <newname>");
                return SessionCommandResult::Handled;
            }
            if let Some(ref s) = session {
                let cur_id = s.get_session_id().to_string();
                if Some(&cur_id) == meta.as_ref().map(|m| &m.id) {
                    let _ = s.set_session_name(&new_name).await;
                    config.output.display_plain(&format!(
                        "/session {} rename {}",
                        meta.as_ref().and_then(|m| m.name.as_deref()).unwrap_or(&cur_id),
                        new_name
                    ));
                } else if let Some(meta) = meta {
                    match repo.as_ref().unwrap().open(&meta).await {
                        Ok((storage, _index)) => {
                            let s = Session::new_persisted(storage, crate::session::SessionIndex {
                                header: crate::session::SessionHeader::new(
                                    &std::env::current_dir().unwrap_or_default()
                                ),
                                entries: std::collections::HashMap::new(),
                                leaf_id: None,
                                children: std::collections::HashMap::new(),
                                session_name: None,
                            });
                            let _ = s.set_session_name(&new_name).await;
                            config.output.display_plain(&format!(
                                "/session {} rename {}",
                                meta.name.as_deref().unwrap_or(&meta.id),
                                new_name
                            ));
                        }
                        Err(e) => {
                            config.output.display_text(&format!("Cannot open session: {}", e));
                        }
                    }
                } else {
                    config.output.display_text(&format!("Session not found: {}", name));
                }
            }
            SessionCommandResult::Handled
        }
        "delete" => {
            if let Some(meta) = meta {
                if let Some(ref s) = session {
                    if s.get_session_id() == meta.id {
                        config.output.display_text("Cannot delete the current session");
                        return SessionCommandResult::Handled;
                    }
                }
                match repo.as_ref().unwrap().delete(&meta).await {
                    Ok(()) => {
                        config.output.display_plain(&format!(
                            "/session {} delete",
                            meta.name.as_deref().unwrap_or(&meta.id)
                        ));
                        SessionCommandResult::Deleted
                    }
                    Err(e) => {
                        config.output.display_text(&format!("Cannot delete session: {}", e));
                        SessionCommandResult::Handled
                    }
                }
            } else {
                config.output.display_text(&format!("Session not found: {}", name));
                SessionCommandResult::Handled
            }
        }
        _ => {
            config.output.display_text(&format!("Unknown session action: {}", action));
            SessionCommandResult::Handled
        }
    }
}

/// Parse `/tree <target_id> [--summarize]` arguments.
fn parse_tree_args(rest: &str) -> (String, bool) {
    let mut summarize = false;
    let mut target = String::new();
    for part in rest.split_whitespace() {
        match part {
            "--summarize" | "-s" => summarize = true,
            id => target = id.to_string(),
        }
    }
    (target, summarize)
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

    let common_len = old_path.iter()
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

/// Ensure a session exists, creating one lazily if needed.
/// Called on first actual turn (Submit or ! command), not at startup.
pub(crate) async fn ensure_session(
    session: &mut Option<Session>,
    repo: &Option<crate::session::repo::JsonlSessionRepo>,
) {
    if session.is_some() {
        return;
    }
    let repo = match repo {
        Some(r) => r,
        None => return,
    };
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return,
    };
    match repo.create(&cwd).await {
        Ok(storage) => {
            let index = storage.build_index().await
                .unwrap_or_else(|_| crate::session::SessionIndex {
                    header: crate::session::SessionHeader::new(&cwd),
                    entries: std::collections::HashMap::new(),
                    leaf_id: None,
                    children: std::collections::HashMap::new(),
                    session_name: None,
                });
            *session = Some(Session::new(storage, index));
        }
        Err(e) => {
            eprintln!("Warning: cannot create session: {}", e);
        }
    }
}

/// Interactive chat loop. Owns the `Box<dyn PicoAgent>` and the `AgentConfig`
/// so slash commands that change model/provider/thinking can rebuild the agent
/// via `create_agent` (required because `CodeAgent<M>` is generic over the
/// provider's completion-model type).
pub async fn run_interactive(config: AgentConfig, settings: &mut SettingsStore) -> Result<()> {
    let mut config = config;
    // Store the original (non-merged) sandbox config so the remove overlay
    // can list config-provided bindings and allow temporary exclusion.
    crate::sandbox_bindings::set_base(config.sandbox.clone());
    // Publish the resolved sandbox config immediately so that user-initiated
    // `!`/`!!` shell commands use the same bwrap/timeout policy as the agent's
    // `bash` tool — even before the agent is lazily created on the first
    // normal prompt. `set_sandbox` is otherwise only called inside
    // `build_rig_agent`, so without this the first `!`/`!!` (issued before any
    // LLM turn) would run UNSANDBOXED. The sandbox config is constant for the
    // session (`apply_to_config` never touches it), so publishing once here is
    // sufficient.
    set_sandbox(config.sandbox.clone());
    // Pending `/settings` changes that need an agent rebuild before the next
    // prompt. `read_input` only flips this flag; the rebuild stays here so it
    // can call the async `create_agent`.
    let mut dirty = false;
    // Agent is created lazily on first use (or on /model, /thinking) so that
    // commands like /help work without a network round-trip.
    let mut agent: Option<Box<dyn PicoAgent>> = None;

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
            style(format!("limit:{} yolo={}", config.tool_call_limit, config.yolo)).yellow(),
        );
        config.output.display_plain("picocode - /menu for details.  (or type /help)");
        config.output.display_plain(&status);
    }

    // Persistent session storage.
    // Session storage: create repo eagerly (just a path, no I/O) for /session
    // overlay, but defer actual session file creation to the first turn
    // to avoid empty sessions.
    let sessions_dir = crate::platform::app_config_dir().map(|d| d.join("sessions"));
    let cwd = std::env::current_dir().ok();
    let (mut session, mut repo) = if let (Some(dir), Some(_)) = (&sessions_dir, &cwd) {
        let repo = crate::session::repo::JsonlSessionRepo::new(dir.clone());
        // Repo is created eagerly (just a path); the session object is also
        // created unconditionally here so the `/session` overlay can mark the
        // current session from the start. `repo.create()` only opens the
        // storage — the actual `.jsonl` file is written lazily on the first
        // `append_message()` (Session::ensure_persisted) — so no empty session
        // file is created if the user never takes a turn. Creating up front
        // removes the mid-loop `ensure_session` calls (in the first-turn and
        // `!`/`!!` paths).
        (None, Some(repo))
    } else {
        (None, None)
    };
    ensure_session(&mut session, &repo).await;
    let mut current_session_id = session
        .as_ref()
        .map(|s| s.get_session_id().to_string())
        .unwrap_or_default();

    let mut line_history = crate::repl::History::load();
    let mut recent_models: Vec<String> = Vec::new();
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
    // Guard handle for the reader thread (suspend/resume / hard-exit).
    let guard = repl.as_ref().map(|r| r.guard());
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
    loop {
        let action = match repl.as_mut() {
            Some(r) => {
                config.output.display_separator();

                // Build session context data for REPL overlays.
                let (sessions, branch_messages, tree, leaf_id) = build_session_ctx(
                    &session, &repo,
                ).await;

                let ctx = ReplCtx {
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                    recent: recent_models.clone(),
                    persona: config.persona_name.clone(),
                    // Snapshot the agent's accumulated token usage so the `/stat`
                    // overlay can render without needing the agent directly.
                    session_usage: agent
                        .as_ref()
                        .map(|a| a.session_usage())
                        .unwrap_or_default(),
                    sessions,
                    current_session_id: current_session_id.clone(),
                    branch_messages,
                    tree,
                    leaf_id,
                };
                r.read_input(&prompt_str, &mut line_history, &ctx, settings, &mut dirty, config.output.cursor_row(), config.output.as_ref())
            }
            None => Some(InputAction::Submit(config.output.get_user_input())),
        };
        let action = match action {
            Some(a) => a,
            None => continue,
        };
        match action {
            InputAction::Quit => break,
            InputAction::SetModel(provider, model) => {
                let mut new_cfg = config.clone();
                new_cfg.provider = provider;
                new_cfg.model = model;
                match create_agent(new_cfg.clone()).await {
                    Ok(new_agent) => {
                        agent = Some(new_agent);
                        config = new_cfg;
                        recent_models.push(format!("{}:{}", config.provider, config.model));
                        // Log model change to session.
                        if let Some(ref s) = session {
                            let _ = s.append_model_change(&config.provider, &config.model).await;
                        }
                    }
                    Err(e) => config
                        .output
                        .display_text(&format!("Cannot switch model: {}", e)),
                }
            }
            InputAction::SetThinking(level) => {
                match level.parse::<thinking::ThinkingLevel>() {
                    Ok(level) => {
                        let mut new_cfg = config.clone();
                        new_cfg.thinking_level = Some(level.as_str().to_string());
                        match create_agent(new_cfg.clone()).await {
                            Ok(new_agent) => {
                                agent = Some(new_agent);
                                config = new_cfg;
                                // Log thinking change to session.
                                if let Some(ref s) = session {
                                    let _ = s.append_thinking_level_change(config.thinking_level.as_deref().unwrap_or("off")).await;
                                }
                            }
                            Err(e) => {
                                let msg = format!("Cannot set thinking: {}", e);
                                config.output.display_plain(&msg);
                            }
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        config.output.display_plain(&msg);
                    }
                }
            }
            InputAction::SetPersona(name) => {
                let mut new_cfg = config.clone();
                // Empty name (the "none" entry) clears the runtime persona.
                new_cfg.persona_name = if name.trim().is_empty() {
                    None
                } else {
                    // Resolve now so an unknown name is rejected before rebuild.
                    match crate::persona::get_persona(&name) {
                        Some(prompt) => {
                            new_cfg.persona_prompt = Some(prompt);
                            Some(name.clone())
                        }
                        None => {
                            config.output.display_plain(&format!(
                                "Unknown persona '{}' (see /persona for the list)",
                                name
                            ));
                            // Skip the agent rebuild; keep the old persona.
                            None
                        }
                    }
                };
                if new_cfg.persona_name.is_some() || name.trim().is_empty() {
                    match create_agent(new_cfg.clone()).await {
                        Ok(new_agent) => {
                            agent = Some(new_agent);
                            config = new_cfg;
                            // No confirmation line: the `/persona <name>` input is self-documenting.
                        }
                        Err(e) => {
                            let msg = format!("Cannot set persona: {}", e);
                            config.output.display_plain(&msg);
                        }
                    }
                }
            }
            InputAction::Sandbox(sub, path, sandbox_path) => {
                let result = match sub.as_str() {
                    "add_ro" => crate::sandbox_bindings::add_ro(&path, &sandbox_path),
                    "add_rw" => crate::sandbox_bindings::add_rw(&path, &sandbox_path),
                    "remove" => crate::sandbox_bindings::remove(&path),
                    "exclude" => crate::sandbox_bindings::exclude(&path),
                    _ => Err(format!("unknown sandbox subcommand: {}", sub)),
                };
                match result {
                    Ok(()) => {
                        // Re-publish the sandbox config with session bindings merged,
                        // AND update `config.sandbox` so the merged version survives
                        // a subsequent lazy agent creation (which calls
                        // `set_sandbox(config.sandbox.clone())` inside
                        // `build_rig_agent`).
                        let mut merged = config.sandbox.clone().unwrap_or_default();
                        crate::sandbox_bindings::merge_into(&mut merged);
                        crate::tools::set_sandbox(Some(merged.clone()));
                        config.sandbox = Some(merged);
                        let msg = if sub == "exclude" {
                            format!("sandbox: excluded {} (ok)", path)
                        } else if sandbox_path.is_empty() {
                            format!("sandbox: {} {} (ok)", sub, path)
                        } else {
                            format!("sandbox: {} {} → {} (ok)", sub, path, sandbox_path)
                        };
                        config.output.display_plain(&msg);
                    }
                    Err(e) => {
                        let msg = format!("sandbox error: {}", e);
                        config.output.display_plain(&msg);
                    }
                }
            }
            InputAction::NavigateTree(target_id) => {
                if let Some(ref s) = session {
                    let old_leaf_id = s.get_leaf_id().await;
                    if old_leaf_id.as_deref() == Some(target_id.as_str()) {
                        continue;
                    }
                    let _ = s.move_leaf(Some(target_id.clone())).await;
                    config.output.display_plain(&format!(
                        "/tree navigated ({})",
                        &target_id[..target_id.len().min(8)],
                    ));
                }
            }
            InputAction::Submit(input) => {
                if input.is_empty() {
                    continue;
                }
                line_history.add(&input);
                if input == "/q" || input == "exit" {
                    break;
                }
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
                                    let msg = Message::user(format!(
                                        "Ran shell command: {cmd}"
                                    ));
                                    if let Some(ref s) = session {
                                        let _ = s.append_message(&msg).await;
                                        let reply = Message::Assistant {
                                            id: None,
                                            content: OneOrMany::one(AssistantContent::Text(
                                                Text::new(out.to_string()),
                                            )),
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
                                config
                                    .output
                                    .display_text(&format!("shell error: {}", e));
                            }
                        }
                    }
                    continue;
                }
                // Handle `/tree <target_id>` commands — navigate session tree.
                if input.starts_with("/tree") {
                    let rest = input["/tree".len()..].trim();
                    let (target_id, summarize) = parse_tree_args(rest);
                    if target_id.is_empty() {
                        // No target: the overlay is already open from from_buffer.
                        continue;
                    }

                    if let Some(ref s) = session {
                        let old_leaf_id = s.get_leaf_id().await;
                        if old_leaf_id.as_deref() == Some(target_id.as_str()) {
                            continue; // already at this position
                        }

                        // Summarize the abandoned branch if requested.
                        if summarize {
                            if let Some(old_leaf) = old_leaf_id.as_deref() {
                                let (abandoned, common_ancestor_id) = {
                                    let index = s.index().lock().await;
                                    collect_entries_for_branch_summary(&index, old_leaf, &target_id)
                                };
                                if !abandoned.is_empty() {
                                    config.output.display_thinking("Summarizing abandoned branch...");
                                    let prompt = build_summary_prompt(&abandoned);
                                    if let Some(ref agent) = agent {
                                        match agent.summarize(&prompt).await {
                                            Ok(summary) => {
                                                let _ = s.append_branch_summary(
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

                        // Move the leaf pointer. The next prompt() call reads the
                        // new branch via build_context() automatically.
                        let _ = s.move_leaf(Some(target_id.clone())).await;

                        config.output.display_plain(&format!(
                            "/tree {} ({})",
                            if summarize { "navigated + summarized" } else { "navigated" },
                            &target_id[..target_id.len().min(8)],
                        ));
                    }
                    continue;
                }
                // Handle `/session <name> <action>` commands.
                if input.starts_with("/session") {
                    let out = config.output.clone();
                    match handle_session_command(
                        &input,
                        &mut session,
                        &mut repo,
                        &mut config,
                        &out,
                        settings,
                    ).await {
                        SessionCommandResult::NotSession => {
                            // Not a session command — fall through to normal prompt.
                        }
                        SessionCommandResult::Switched(new_id) => {
                            current_session_id = new_id;
                            continue;
                        }
                        _ => {
                            continue;
                        }
                    }
                }
                // Handle `/clear`: delete current session and start fresh.
                if input == "/clear" || input.starts_with("/clear ") {
                    if let Some(ref s) = session {
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
                    session = None;
                    current_session_id = String::new();
                    config.output.display_plain("/clear");
                    continue;
                }
                // Handle `/replay [N[t|l]]`: re-render session history.
                if input == "/replay" || input.starts_with("/replay ") {
                    if let Some(ref s) = session {
                        let arg = input["/replay".len()..].trim();
                        if arg.ends_with('l') {
                            // Line-based limit: build markdown of the full session
                            // content, tail the lines, and render via display_markdown.
                            let n: usize = arg[..arg.len()-1].trim().parse().unwrap_or(0);
                            if n > 0 {
                                match s.build_context().await {
                                    Ok(ctx) => {
                                        let turns = group_into_turns(&ctx.messages);
                                        if turns.is_empty() {
                                            config.output.display_text("Session is empty");
                                        } else {
                                            // Build a markdown string of the full session.
                                            let mut md = String::new();
                                            for turn in &turns {
                                                md.push_str("## User\n\n");
                                                md.push_str(&turn.user_text);
                                                md.push_str("\n\n");
                                                if !turn.segments.is_empty() {
                                                    md.push_str("## Assistant\n\n");
                                                    for seg in &turn.segments {
                                                        for t in &seg.text {
                                                            md.push_str(t);
                                                            md.push('\n');
                                                        }
                                                        for t in &seg.thinking {
                                                            md.push_str(&format!("> {}\n\n", t));
                                                        }
                                                        for (_, name, args) in &seg.tool_calls {
                                                            md.push_str(&format!("**Tool Call:** `{}`\n\n```json\n{}\n```\n\n", name, args));
                                                        }
                                                        for (_, result) in &seg.tool_results {
                                                            md.push_str(&format!("```\n{}\n```\n\n", result));
                                                        }
                                                    }
                                                }
                                            }
                                            let lines: Vec<&str> = md.lines().collect();
                                            let start = lines.len().saturating_sub(n);
                                            let tail_md = lines[start..].join("\n");
                                            config.output.display_separator();
                                            config.output.display_markdown(&tail_md);
                                        }
                                    }
                                    Err(e) => {
                                        config.output.display_text(&format!("Cannot replay session: {}", e));
                                    }
                                }
                            } else {
                                // `0l` — replay all (same as bare `/replay`).
                                match s.build_context().await {
                                    Ok(ctx) => {
                                        let turns = group_into_turns(&ctx.messages);
                                        let total = turns.len();
                                        if total > 0 {
                                            config.output.display_separator();
                                        }
                                        replay_recent_turns(&config.output, &ctx, total);
                                    }
                                    Err(e) => {
                                        config.output.display_text(&format!("Cannot replay session: {}", e));
                                    }
                                }
                            }
                        } else {
                            // Turn-based limit: N or Nt (or no arg).
                            let replay_turns: Option<usize> = if arg.is_empty() {
                                None
                            } else {
                                let clean = arg.trim_end_matches('t').trim();
                                Some(clean.parse::<usize>().ok().unwrap_or(usize::MAX))
                            };
                            match s.build_context().await {
                                Ok(ctx) => {
                                    let turns = group_into_turns(&ctx.messages);
                                    let total = turns.len();
                                    let count = replay_turns.unwrap_or(total).min(total);
                                    if count > 0 {
                                        config.output.display_separator();
                                    }
                                    replay_recent_turns(&config.output, &ctx, count);
                                }
                                Err(e) => {
                                    config.output.display_text(&format!("Cannot replay session: {}", e));
                                }
                            }
                        }
                    } else {
                        config.output.display_text("No active session to replay");
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
                // Live-apply pending `/settings` changes (deferred agent
                // rebuild). `read_input` only flips `dirty`; the rebuild stays
                // here so it can call the async `create_agent` and mirror the
                // `SetModel`/`SetThinking` error handling.
                if dirty {
                    let mut new_cfg = config.clone();
                    settings.apply_to_config(&mut new_cfg);
                    match create_agent(new_cfg.clone()).await {
                        Ok(new_agent) => {
                            agent = Some(new_agent);
                            config = new_cfg;
                            dirty = false;
                        }
                        Err(e) => {
                            config
                                .output
                                .display_text(&format!("Cannot apply settings: {}", e));
                            // Keep `dirty` set so the next Submit retries; `config`
                            // and the agent stay in sync (both old).
                        }
                    }
                }
                // Normal prompt — create the agent lazily on first use.
                if agent.is_none() {
                    match create_agent(config.clone()).await {
                        Ok(a) => agent = Some(a),
                        Err(e) => {
                            config
                                .output
                                .display_text(&format!("Cannot start agent: {}", e));
                            continue;
                        }
                    }
                }
                let response = {
                    if let Some(g) = &guard {
                        // While the main task is blocked awaiting the LLM, spawn a
                        // background reader so `Esc`/`Ctrl-C`/`Ctrl-Z` are still
                        // handled. Signal the thread to stop, then join, once the
                        // request returns.
                        let done = Arc::new(AtomicBool::new(false));
                        // Gate the reader backs off during a tool confirmation
                        // dialog (which reads stdin on the main task).
                        let confirm_gate = config
                            .output
                            .confirm_gate()
                            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
                        let handle = crate::repl::spawn_in_flight_reader(
                            g.clone(),
                            cancel.clone(),
                            config.cancel_notify.clone(),
                            done.clone(),
                            confirm_gate,
                        );
                        let resp = agent
                            .as_ref()
                            .unwrap()
                            .prompt(&input, session.as_ref())
                            .await;
                        done.store(true, Ordering::SeqCst);
                        let _ = handle.join();
                        resp
                    } else {
                        agent
                            .as_ref()
                            .unwrap()
                            .prompt(&input, session.as_ref())
                            .await
                    }
                };
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

/// Compute the `/stat` panel lines: context-window limit + current usage, an
/// approximate session cost, and the cache hit rate (when the provider reports
/// cache metrics). Reads the live model catalog for the window size and the
/// per-token pricing, and the agent's accumulated [`SessionUsage`] for the
/// per-turn / session totals. Returns plain lines so the REPL can render them
/// in the `/stat` overlay (`render_plain` display).
pub fn session_stats_lines(provider: &str, model: &str, usage: SessionUsage) -> Vec<String> {
    let entry = model_catalog::lookup(provider, model);
    let ctx_len = entry.as_ref().and_then(|e| e.context_length);

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("Session stats — {}:{}", provider, model));

    // Context window limit.
    match ctx_len {
        Some(max) => lines.push(format!("  context window limit : {} tokens", max)),
        None => lines.push("  context window limit : unknown (model not in catalog)".to_string()),
    }

    // Current context usage (absolute + % of max).
    let cur = usage.last_input_tokens;
    if cur > 0 {
        if let Some(max) = ctx_len {
            let pct = (cur as f64 / max as f64) * 100.0;
            lines.push(format!(
                "  current usage        : {} tokens ({:.1}% of limit)",
                cur, pct
            ));
        } else {
            lines.push(format!("  current usage        : {} tokens", cur));
        }
    } else {
        lines.push("  current usage        : n/a (no completion call recorded yet)".to_string());
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
                "  approx session cost : ${:.4} (in {} / out {} tokens)",
                cost, usage.session_input_tokens, usage.session_output_tokens
            ));
        }
        _ => lines.push("  approx session cost : n/a (no pricing in catalog)".to_string()),
    }

    // Cache hit rate (session-level: reads as a fraction of all input tokens).
    let cached = usage.session_cached_input_tokens;
    let creation = usage.session_cache_creation_input_tokens;
    let total_in = usage.session_input_tokens;
    if total_in > 0 && (cached > 0 || creation > 0) {
        let hit = (cached as f64 / total_in as f64) * 100.0;
        lines.push(format!(
            "  cache hit rate       : {:.1}% (read {} / written {} / fresh {} tokens)",
            hit,
            cached,
            creation,
            total_in.saturating_sub(cached + creation)
        ));
    } else {
        lines.push(
            "  cache hit rate       : n/a (provider reported no cache metrics)".to_string(),
        );
    }

    lines
}

impl<M: CompletionModel + 'static> CodeAgent<M> {
    pub fn new(
        agent: Agent<M>,
        output: Arc<dyn Output>,
        tool_call_limit: usize,
        provider: String,
        model: String,
        yolo: bool,
        persona_name: Option<String>,
        user_cancel: Arc<AtomicBool>,
        cancel_notify: Arc<tokio::sync::Notify>,
        thinking_level: Option<String>,
        reasoning_in_context: bool,
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
            usage: Mutex::new(SessionUsage::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use rig_core::message::Reasoning;

    use super::{create_private_dir, is_anthropic_family, maybe_strip_reasoning, persist_turn, reasoning_text};
    use rig_core::message::Message;
use rig_core::message::{AssistantContent, Text};
use rig_core::OneOrMany;

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

    #[cfg(unix)]
    #[test]
    fn create_private_dir_sets_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("picocode_token_test_{}", std::process::id()));
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
            true,
            "openai",
            "",
        );

        // Prior turn preserved, new turn appended (not wiped/replaced).
        assert_eq!(hist.len(), 3);
        assert!(hist.contains(&Message::user("prior-1")));
        assert!(hist.contains(&Message::user("turn-2-prompt")));
        assert!(hist.contains(&Message::assistant("turn-2-reply")));
        // Prior must come before the new turn.
        let prior_idx = hist.iter().position(|m| *m == Message::user("prior-1")).unwrap();
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
        persist_turn(&mut Some(&mut hist), None, Some(cancelled), true, "openai", "");

        // Replaced (not appended): exactly the in-flight full history, with no
        // duplicate of `prior-1`.
        assert_eq!(hist.len(), 3);
        assert_eq!(
            hist.iter().filter(|m| **m == Message::user("prior-1")).count(),
            1
        );
        assert!(hist.contains(&Message::user("aborted-prompt")));
        assert!(hist.contains(&Message::assistant("aborted-partial")));
    }

    // `reasoning_in_context=false` must drop reasoning blocks from assistant
    // messages for non-Anthropic providers, while keeping text (and keeping
    // reasoning entirely for Anthropic-family turns that involve tool calls, or
    // when the flag is true). On an Anthropic turn *without* tool calls, the
    // flag is honored and reasoning is stripped.
    #[test]
    fn strips_reasoning_for_non_anthropic_when_disabled() {
        let assistant = Message::Assistant {
            id: None,
            content: OneOrMany::many(vec![
                AssistantContent::Reasoning(Reasoning::new("hidden thought")),
                AssistantContent::Text(Text::new("answer")),
            ])
            .unwrap(),
        };
        let msgs = vec![Message::user("q"), assistant];

        // Disabled + non-Anthropic: reasoning dropped, text preserved.
        let out = maybe_strip_reasoning(msgs.clone(), false, "openai", "");
        if let Message::Assistant { content, .. } = &out[1] {
            assert!(!content.iter().any(|c| matches!(c, AssistantContent::Reasoning(_))));
            assert!(content.iter().any(|c| matches!(c, AssistantContent::Text(_))));
        } else {
            panic!("expected assistant message");
        }

        // Enabled: reasoning kept.
        let kept = maybe_strip_reasoning(msgs.clone(), true, "openai", "");
        if let Message::Assistant { content, .. } = &kept[1] {
            assert!(content.iter().any(|c| matches!(c, AssistantContent::Reasoning(_))));
        } else {
            panic!("expected assistant message");
        }

        // Anthropic-family WITHOUT tool calls: flag honored, reasoning stripped.
        let stripped = maybe_strip_reasoning(msgs.clone(), false, "anthropic", "claude-3-5-sonnet");
        if let Message::Assistant { content, .. } = &stripped[1] {
            assert!(!content.iter().any(|c| matches!(c, AssistantContent::Reasoning(_))));
            assert!(content.iter().any(|c| matches!(c, AssistantContent::Text(_))));
        } else {
            panic!("expected assistant message");
        }
    }

    // Anthropic-family turn WITH a tool call: the leading reasoning must be
    // force-kept (thinking+tool_use replay contract), even with the flag off.
    #[test]
    fn anthropic_tool_call_turn_forces_reasoning_kept() {
        let assistant = Message::Assistant {
            id: None,
            content: OneOrMany::many(vec![
                AssistantContent::Reasoning(Reasoning::new("hidden thought")),
                AssistantContent::Text(Text::new("answer")),
                AssistantContent::ToolCall(rig_core::message::ToolCall::new(
                    "tool-1".into(),
                    rig_core::message::ToolFunction::new(
                        "Bash".to_string(),
                        serde_json::json!({"cmd": "ls"}),
                    ),
                )),
            ])
            .unwrap(),
        };
        let msgs = vec![Message::user("q"), assistant];

        let forced = maybe_strip_reasoning(msgs, false, "anthropic", "claude-3-5-sonnet");
        if let Message::Assistant { content, .. } = &forced[1] {
            assert!(content.iter().any(|c| matches!(c, AssistantContent::Reasoning(_))));
        } else {
            panic!("expected assistant message");
        }
    }

    #[test]
    fn is_anthropic_family_classifies_providers() {
        assert!(is_anthropic_family("anthropic", "claude-x"));
        assert!(is_anthropic_family("openrouter", "anthropic/claude-3.5-sonnet"));
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
        use crate::Output;
        use crate::output::Confirmation;
        use crate::settings::SettingsStore;
        use crate::AgentConfig;
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
        let host_path = std::env::temp_dir().join(format!(
            "picocode_sandbox_regr_{}.txt",
            std::process::id()
        ));
        std::fs::write(&host_path, sentinel).unwrap();
        let probe = "/sandbox_probe_xyz".to_string();

        let sandbox = SandboxConfig {
            timeout_secs: Some(30),
            network: Some(false),
            memory_mb: None,
            pid_isolation: Some(true),
            drop_caps: Some(true),
            tmp_size_mb: Some(16),
            ro: vec![],
            rw: vec![BindPath::Remap {
                from: host_path.display().to_string(),
                to: probe.clone(),
            }],
            env: None,
            max_output_bytes: None,
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
            fn display_header(
                &self,
                _: &str,
                _: &str,
                _: bool,
                _: usize,
                _: Option<&str>,
            ) {
            }
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
            reasoning_in_context: false,
        };
        let mut settings = SettingsStore::from_agent_config(&config, None);

        // No normal prompt is ever submitted, so the agent is never created and
        // `create_agent` (and its `set_sandbox`) is never called — exactly the
        // pre-fix condition where `!` would otherwise run unsandboxed.
        super::run_interactive(config, &mut settings)
            .await
            .unwrap();

        let shown = shown.lock().unwrap().clone();
        let _ = std::fs::remove_file(&host_path);

        assert!(
            !shown.contains("No such file"),
            "bang command ran UNSANDBOXED (probe path {probe} absent); output:\n{shown}"
        );
        assert!(
            shown.contains(sentinel),
            "expected sandboxed `!` command to print the remapped host file content; output:\n{shown}"
        );
    }
}
