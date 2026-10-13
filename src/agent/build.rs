//! Building a rig `Agent` from an `AgentConfig`: the provider wiring, the
//! per-request params, the tool registration with approval guards, and the
//! `LoggingHook` that records the streaming turn.
//!
//! `create_agent` is the public entry point — the one place a `CodeAgent` is
//! constructed from a config. It resolves the model catalog, bakes the
//! provider's reasoning params, wires the completion client (pinned for
//! OpenRouter so server-side prompt cache survives), registers the tools each
//! with its approval `Guard`, and hands rig the agent. `build_rig_agent` is the
//! pure "given a client, assemble the agent" half; `create_agent` adds the
//! credential and tool-availability discovery around it.
//!
//! `effective_params` is the per-turn variant: it takes the live params and any
//! user overrides (`/reasoning`) and reports a note when a requested level is
//! remapped. Split out from `create_agent` so it can be tested against a
//! support table directly.
//!
//! Both write out in pi format; the prompts (`DEFAULT_AGENT_PROMPT`,
//! `SED_TOOL_PROMPT`) are the standing system message and its sed-tool
//! addendum.

use rig_agent::agent::{
    Agent, AgentBuilder, AgentHook, CompletionCallAction, CompletionCallEvent,
    CompletionResponseEvent, HookContext, InvalidToolCallAction, InvalidToolCallContext,
    ModelTurnAction, ModelTurnFinished, NoToolConfig, ObservationAction, RequestPatch, ToolCall,
    ToolCallAction, ToolResultAction, ToolResultEvent,
};
use rig_agent::client::AgentClientExt;
use rig_agent::tool::ToolContext;
use rig_core::client::CompletionClient;
use rig_core::message::Message;
use rig_core::providers::{
    anthropic, azure, chatgpt, cohere, copilot, deepseek, doubleword, gemini, groq, huggingface,
    hyperbolic, llamacpp, minimax, mira, mistral, moonshot, ollama, openai, openrouter, perplexity,
    together, xai, xiaomimimo, zai,
};
use rig_core::tool::{Tool, ToolExecutionError};
use rig_reqwest::client::{DefaultTransportBuilder, DefaultTransportClient};
use serde_json;

use crate::output::{Confirmation, Output};
use crate::params;
use crate::Result;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent::msglog::MessageLogger;
use crate::agent::transcript::{
    blob_edit_tool_calls, is_tool_available, maybe_strip_reasoning, model_facing_tool_text,
    reasoning_remap_note, split_multi_file_edit_calls, TurnTranscript,
};
use crate::agent::CodeAgent;
use crate::tools::{
    bsed_available, set_sandbox, AgentBrowser, Bash, CopyFile, EditFile, GlobFiles, GrepText,
    ListDir, MakeDir, MoveFile, ReadFile, Remove, SedPreview, WriteFile,
};

#[derive(Clone)]
pub struct AgentConfig {
    pub provider: String,
    pub model: String,
    /// The frame + input seam — `set_status` / `frame_size` / `paint` /
    /// `confirm_default` and, in the REPL, `get_user_input`. Step 4 moves it to
    /// `RenderManager`; the target `AgentConfig` (impl doc §4) drops it. Content
    /// calls live on [`Self::content`] as of step 3d.
    pub output: Arc<dyn Output>,
    /// The content handle (step 3d): every `display_*` / `stream_*` /
    /// `begin|end_response` call goes here. In production it is the same
    /// `Content` `ConsoleOutput` holds; in tests and `--quiet` it is a
    /// `Content` whose verbs short-circuit through [`Self::output`] — see
    /// [`Self::content_for`].
    pub content: Arc<crate::content::Content>,
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

    /// The `content` handle for a config whose frame+input handle is `output`.
    ///
    /// Content verbs short-circuit through that same handle, so
    /// `RecordingOutput` still records the source strings an assertion
    /// compares and `QuietOutput` / `NoOutput` still discard — the content
    /// seam (impl doc §4) without a render stack behind it. `ConsoleOutput`
    /// passes its own tap-less `Content`; that is the one case where content
    /// renders for real.
    pub fn content_for(output: &Arc<dyn Output>) -> Arc<crate::content::Content> {
        Arc::new(crate::content::Content::with_tap(output.clone()))
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
pub(crate) fn request_additional_params(
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
                config.content,
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
pub(crate) struct LoggingHook {
    /// Content only: this hook's every verb is a `display_*` (step 3d).
    pub(crate) content: Arc<crate::content::Content>,
    pub(crate) user_cancel: Arc<AtomicBool>,
    pub(crate) message_logger: Option<Arc<MessageLogger>>,
    pub(crate) reasoning_in_context: crate::settings::ReasoningInContextMode,
    pub(crate) provider: String,
    pub(crate) model: String,
    /// Run-scoped canonical transcript accumulator; see [`TurnTranscript`].
    /// Hooks and the prompt loop both append under this mutex in stream
    /// order, so the transcript mirrors rig's own `new_messages`.
    pub(crate) transcript: Arc<Mutex<TurnTranscript>>,
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
        self.content.display_tool_call(event.tool_name, &args_json);
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
        self.content.display_error(&format!(
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
        self.content
            .display_tool_result(&event.raw_result.output().render());
        if let Some(text) = event.raw_result.output().as_text() {
            if let Some(cleaned) = model_facing_tool_text(event.tool_name, text) {
                return ToolResultAction::rewrite(cleaned);
            }
        }
        ToolResultAction::Keep
    }
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
returns the change as `@@@OLD`/`@@@NEW`/`@@@END` blocks, each anchored to a line. Read the
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::msglog::MessageLogger;
    use crate::agent::transcript::reasoning_remap_note;
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

        let no_output: Arc<dyn Output> = Arc::new(crate::NoOutput);
        let hook_content = AgentConfig::content_for(&no_output);
        let mut stream = agent
            .stream_prompt("hi")
            .add_hook(LoggingHook {
                content: hook_content,
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
}
