//! Building a rig `Agent` from an `AgentConfig`: the provider wiring, the
//! per-request params, the tool registration with approval guards, and the
//! `LoggingHook` that records the streaming turn.
//!
//! `create_agent` is the public entry point — the one place a `CodeAgent` is
//! constructed from a config. It resolves the model catalog, bakes the
//! provider's reasoning params, wires the completion client (OpenRouter with
//! prompt caching on; provider routing only when the model's settings say so),
//! registers the tools each with its approval `Guard`, and hands rig the
//! agent. `build_rig_agent` is the pure "given a client, assemble the agent"
//! half; `create_agent` adds the credential and tool-availability discovery
//! around it.
//!
//! `effective_params` is the per-turn variant: it takes the live params and any
//! user overrides (`/reasoning`) and reports a note when a requested level is
//! remapped. Split out from `create_agent` so it can be tested against a
//! support table directly.

use rig_agent::agent::{
    Agent, AgentBuilder, AgentHook, CompletionCallAction, CompletionCallEvent, DispatchAction,
    DispatchEvent, HookContext, InvalidToolCallAction, InvalidToolCallContext, ModelTurnAction,
    ModelTurnFinished, OutcomeAction, OutcomeEvent, RequestPatch,
};
use rig_agent::tool::ToolContext;

use rig_core::providers::{
    anthropic, azure, chatgpt, cohere, copilot, deepseek, doubleword, gemini, groq, huggingface,
    hyperbolic, minimax, mira, mistral, moonshot, ollama, openai, openrouter, perplexity, together,
    xai, xiaomimimo, zai,
};
use rig_core::tool::{Tool, ToolExecutionError};
use serde_json;

use crate::params;
use crate::Result;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::agent::msglog::MessageLogger;
use crate::agent::transcript::{
    assistant_message, maybe_strip_reasoning, model_facing_tool_text, reasoning_remap_note,
    render_model_facing, EditLedger, TurnTranscript,
};
use crate::agent::CodeAgent;
use crate::tools::{
    bsed_available, set_sandbox, Bash, EditFile, GrepText, ReadFile, SedPreview, WriteFile,
};

#[derive(Clone)]
pub struct AgentConfig {
    pub provider: String,
    pub model: String,
    /// The content handle: every `display_*` / `stream_*` /
    /// `begin|end_response` call goes here. In production it is the same
    /// `Content` the paint path holds; under `--quiet` it is
    /// [`Content::quiet`]; in a test it is a `Content` built for capture or
    /// scripted input.
    pub content: Arc<crate::content::Content>,
    pub yolo: bool,
    pub quiet: bool,
    /// The REPL's end of the confirm port, cloned into every guarded tool.
    ///
    /// `None` means "nobody to ask", and the tools deny — the correct behaviour
    /// for piped/batch input and for one-shot `run_once`, where there is no key
    /// pump to drive a dialog. Set once by `run_interactive` before the agent is
    /// built, so a
    /// `/model` rebuild (which clones this config) keeps the same port and the
    /// same session approvals.
    pub confirm_port: Option<crate::confirm::ConfirmPort>,
    /// The request-shaping knobs for this model: reasoning level, temperature,
    /// top_p. Each is `None` = "unspecified, leave the provider's default".
    /// Replaces the old `reasoning_level: Option<String>`
    /// (doc/model_settings_v2.md A.2); `PartialEq` here is what the rebuild
    /// gate compares.
    pub params: crate::params::ModelParams,
    /// OpenRouter provider preferences for this model, typed (not JSON) so the
    /// merge step can serialize once and a test can inspect the value the
    /// rebuild gate compares. Seeded from the per-model settings record on
    /// activation and refreshed by a Ctrl-E of the active model; `None` means
    /// no opinion — no `provider` preferences go out at all, so upstream
    /// choice is OpenRouter's.
    ///
    /// Carried but unused on every provider but `openrouter`
    /// (doc/261017_openrouter_provider_selection.md §1).
    pub routing: Option<crate::model_settings::Routing>,
    /// The `provider:model` this config's model is an *alias* for, if any
    /// (doc/261023_model_aliases.md). Seeded at activation like `routing` and
    /// refreshed by a Ctrl-E of the active model.
    ///
    /// `provider`/`model` stay the *identity* — the session header,
    /// `ModelChange`, the status bar and `default_model` all speak of the alias
    /// — while [`create_agent`] derives the *wire* pair from this with
    /// [`Self::wire_id`]. Settings are never inherited from the upstream: the
    /// alias's own params and routing are what is sent.
    pub alias_for: Option<String>,
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
    /// Shared, live-editable `show_reasoning` display mode. `Content` holds a
    /// clone and reads it on every thinking event, so `/set
    /// show_reasoning …` updates the rendering immediately (no agent rebuild).
    pub show_reasoning_handle: crate::settings::ShowReasoningHandle,
    /// How reasoning blocks are kept in the LLM context. Default `Never`.
    /// Forced `Always` for Anthropic-family providers (thinking+tool_use replay contract).
    pub reasoning_in_context: crate::settings::ReasoningInContextMode,
    /// Optional path to JSONL file for logging raw messages sent/received.
    /// When Some, messages are appended to this file in JSONL format.
    pub message_log_path: Option<String>,
    /// Optional custom directory for session storage. When None, the platform
    /// default is used (e.g., `~/.config/prior/sessions`).
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

    /// The `provider:model` that actually goes on the wire: the alias target
    /// when one is set, else the identity.
    ///
    /// One derivation, used by [`create_agent`] for the client, the params and
    /// the message logger — the same rule the menus apply to a resolved entry's
    /// implicit wire provider, so disclosure and request agree.
    pub fn wire_id(&self) -> (String, String) {
        match self.alias_for.as_deref().and_then(|t| t.split_once(':')) {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => (self.provider.clone(), self.model.clone()),
        }
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
        // The *wire* pair: an alias's reasoning is governed by its upstream's
        // effort list, so the note must describe the model the request actually
        // names (doc/261023_model_aliases.md).
        let (provider, model) = app_state.with_config(|c| c.wire_id());
        let support = crate::model_catalog::reasoning_support(&provider, &model);
        notes.extend(reasoning_remap_note(requested, &support));
    }
    (overrides.apply(&live), notes)
}

/// The provider-specific `additional_params` for one request: prior's
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
    routing: Option<&crate::model_settings::Routing>,
) -> Option<serde_json::Value> {
    let support = crate::model_catalog::reasoning_support(provider, model);
    let base = params::additional_params(&provider.to_lowercase(), params, &support);
    if provider.eq_ignore_ascii_case("openrouter") {
        // The openrouter branch in `create_agent` carries the same value, so
        // an override turn cannot lose the provider preference.
        merge_openrouter_params(base, routing)
    } else {
        base
    }
}

/// Merge OpenRouter `ProviderPreferences` into the existing additional_params
/// (typically thinking config).
///
/// Only an explicit `routing` table contributes. With none, prior sends no
/// provider preferences at all — there is deliberately no built-in upstream
/// pin, because a hidden default that silently narrows where requests may go
/// is worse than OpenRouter's own load balancing. A model that wants that
/// affinity says so in its settings:
/// `routing = { only = ["anthropic"], allow_fallbacks = true }`.
fn merge_openrouter_params(
    existing: Option<serde_json::Value>,
    routing: Option<&crate::model_settings::Routing>,
) -> Option<serde_json::Value> {
    // An empty table is no opinion — the same as an absent key (the record
    // normalizes it away; this is the belt to that).
    let Some(routing) = routing.filter(|r| !r.is_empty()) else {
        return existing;
    };
    let prefs_json = serde_json::json!({ "provider": routing });
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
    // The identity (`provider`/`model`) is what the session, the status bar and
    // `default_model` speak of; the *wire* pair is what the client, the params
    // and the message logger use. They differ only for an alias, whose own
    // settings (params, routing) are sent unchanged to the upstream model.
    let (wire_provider, wire_model) = config.wire_id();
    let provider = wire_provider.to_lowercase();
    let model = wire_model.clone();
    // What this model accepts, so a level it cannot serve is remapped instead
    // of sent and rejected. One shared composition, so the per-request path
    // (a `/reasoning` override) cannot produce a different shape than a normal
    // turn.
    let reasoning_json = request_additional_params(
        &wire_provider,
        &wire_model,
        &config.params,
        config.routing.as_ref(),
    );

    // OAuth credentials live in a `token` sub-directory of the settings dir
    // (see `platform::app_config_dir`), kept private so refresh tokens aren't
    // world-readable. Created up front so the per-provider subdirs inherit it.
    let token_base = crate::platform::app_config_dir().map(|d| d.join("token"));
    if let Some(base) = &token_base {
        let _ = create_private_dir(base);
    }

    macro_rules! build {
        // Anthropic: cache breakpoints on the static prefix (system prompt +
        // tool definitions) and a moving breakpoint on the last message, all
        // at a one-hour TTL, so the growing conversation is billed as cache
        // reads for the duration of an interactive session. With a request
        // cache retention set, rig advances the moving breakpoint itself and
        // the explicit markers take that TTL.
        (anthropic $client:expr) => {{
            let mut model = $client.completion(&model);
            model.wire = model.wire.with_prompt_caching();
            build!(@builder AgentBuilder::new(model)
                .cache(rig_core::completion::CacheRetention::Long))
        }};
        // ChatGPT / Copilot: an OAuth sign-in refreshed per request by rig's
        // `SignedInModel`, over the catalog entry for `vendor/model`.
        (signed_in $vendor:literal $auth:expr) => {{
            build!(@builder signed_in_builder($vendor, &model, $auth)?)
        }};
        // OpenRouter: the `cache_control` breakpoint the fork put on the system
        // prompt and last message, now a generation option. A one-hour marker
        // is preferred; OpenRouter's automatically-caching upstreams refuse one,
        // and `openrouter_cache_retention` asks rig which model this is rather
        // than keeping a copy of its upstream list.
        (cached_openrouter $client:expr) => {{
            let model: rig_core::driver::DynModel<rig_core::operation::Completion> =
                $client.completion(&model).into();
            let retention = openrouter_cache_retention(&model);
            build!(@builder AgentBuilder::new(model).cache(retention))
        }};
        // Every other provider. Z.AI (both endpoints) caches implicitly and has
        // no retention control, so there is nothing to send.
        ($client:expr) => {{
            build!(@builder AgentBuilder::new($client.completion(&model)))
        }};
        (@builder $builder:expr) => {{
            build!(@builder_with_params $builder, reasoning_json.clone())
        }};
        (@builder_with_params $builder:expr, $params:expr) => {{
            let rig_agent = build_rig_agent(
                $builder,
                config.yolo,
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
                Some(Arc::new(MessageLogger::new(
                    path.clone(),
                    wire_provider.clone(),
                    wire_model.clone(),
                )))
            } else {
                None
            };

            CodeAgent::new(
                rig_agent,
                config.content,
                config.tool_call_limit,
                // The agent carries the *wire* pair: it is what the per-turn
                // params, the Anthropic-family replay rule and the message log
                // all act on. Identity lives on `AgentConfig`, the session and
                // `AppState`.
                wire_provider,
                wire_model,
                config.params,
                config.routing.clone(),
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
                return Err(crate::PriorError::MissingApiKey(
                    provider.to_string(),
                    $var.to_string(),
                ));
            }
        };
        ($($var:expr),+ $(,)?) => {
            if !( $( std::env::var($var).is_ok() )||+ ) {
                return Err(crate::PriorError::MissingApiKey(
                    provider.to_string(),
                    vec![$($var.to_string()),+].join(" or "),
                ));
            }
        };
    }

    let agent: CodeAgent = match provider.as_str() {
        "anthropic" => {
            check_env!("ANTHROPIC_API_KEY");
            build!(anthropic anthropic::Anthropic::from_env()?)
        }
        "openai" => {
            check_env!("OPENAI_API_KEY");
            build!(openai::OpenAI::from_env()?)
        }
        "azure" => {
            check_env!("AZURE_OPENAI_API_KEY");
            check_env!("AZURE_OPENAI_ENDPOINT");
            build!(azure::from_env()?)
        }
        "cohere" => {
            check_env!("COHERE_API_KEY");
            build!(cohere::Cohere::from_env()?)
        }
        "chatgpt" => {
            check_env!("CHATGPT_ACCESS_TOKEN");
            let token = std::env::var("CHATGPT_ACCESS_TOKEN").unwrap();
            if token == OAUTH_TOKEN {
                let dir = token_base
                    .as_ref()
                    .map(|b| b.join("chatgpt"))
                    .unwrap_or_else(|| std::path::PathBuf::from(".prior-chatgpt-token"));
                let _ = create_private_dir(&dir);
                // The OAuth sign-in is refreshed per request by rig's
                // `SignedInModel`; the device-code prompt is shown by rig's
                // default handler. The token store is the settings `token`
                // sub-directory, so refresh tokens stay private.
                let auth = chatgpt::auth::Authenticator::new(
                    chatgpt::auth::AuthSource::OAuth,
                    chatgpt::auth::FileTokenStore::new(dir),
                    chatgpt::auth::DeviceCodeHandler::default(),
                    true,
                );
                build!(signed_in "chatgpt" auth)
            } else {
                build!(chatgpt::from_env()?)
            }
        }
        "copilot" => {
            check_env!("GITHUB_COPILOT_API_KEY", "COPILOT_GITHUB_ACCESS_TOKEN");
            let oauth = ["GITHUB_COPILOT_API_KEY", "COPILOT_GITHUB_ACCESS_TOKEN"]
                .iter()
                .any(|v| std::env::var(v).as_deref() == Ok(OAUTH_TOKEN));
            let dir = token_base
                .as_ref()
                .map(|b| b.join("copilot"))
                .unwrap_or_else(|| std::path::PathBuf::from(".prior-copilot-token"));
            if oauth {
                let _ = create_private_dir(&dir);
                let auth = copilot::auth::Authenticator::new(
                    copilot::auth::AuthSource::OAuth,
                    copilot::auth::FileTokenStore::new(dir),
                    copilot::auth::DeviceCodeHandler::default(),
                    true,
                );
                build!(signed_in "copilot" auth)
            } else if let Ok(session) = std::env::var("GITHUB_COPILOT_API_KEY") {
                // A Copilot session token: the Copilot dialect of the OpenAI
                // wire takes it as a key and derives the API endpoint from it.
                // The endpoint override the provider has always honoured is
                // not read by `with_key`, so apply it here.
                let mut config = openai::OpenAIConfig::with_key(&copilot::DIALECT, session);
                if let Ok(base_url) = std::env::var("GITHUB_COPILOT_API_BASE")
                    .or_else(|_| std::env::var("COPILOT_BASE_URL"))
                {
                    config = config.with_base_url(base_url);
                }
                build!(config.client())
            } else {
                // A GitHub access token is not a Copilot session: rig's
                // authenticator exchanges it and refreshes the session it gets
                // back, so the run outlives one session token.
                let token = std::env::var("COPILOT_GITHUB_ACCESS_TOKEN").map_err(|_| {
                    crate::PriorError::MissingApiKey(
                        provider.to_string(),
                        "GITHUB_COPILOT_API_KEY or COPILOT_GITHUB_ACCESS_TOKEN".to_string(),
                    )
                })?;
                let _ = create_private_dir(&dir);
                let auth = copilot::auth::Authenticator::new(
                    copilot::auth::AuthSource::GitHubAccessToken(token),
                    copilot::auth::FileTokenStore::new(dir),
                    copilot::auth::DeviceCodeHandler::default(),
                    true,
                );
                build!(signed_in "copilot" auth)
            }
        }
        "deepseek" => {
            check_env!("DEEPSEEK_API_KEY");
            build!(deepseek::from_env()?)
        }
        "doubleword" => {
            check_env!("DOUBLEWORD_API_KEY");
            build!(doubleword::from_env()?)
        }
        "gemini" | "google" => {
            check_env!("GOOGLE_API_KEY");
            build!(gemini::Gemini::from_env()?)
        }
        "groq" => {
            check_env!("GROQ_API_KEY");
            build!(groq::from_env()?)
        }
        "huggingface" => {
            check_env!("HF_TOKEN");
            build!(huggingface::from_env()?)
        }
        "hyperbolic" => {
            check_env!("HYPERBOLIC_API_KEY");
            build!(hyperbolic::from_env()?)
        }
        "llamafile" | "llamacpp" => {
            // A local `llama-server` needs no credential: rig 0.45's
            // `llamacpp::from_env` nonetheless demands `LLAMACPP_API_KEY`, so
            // build the config directly with an optional key. The dialect
            // defaults to `http://localhost:8080/v1`; `LLAMACPP_API_BASE_URL`
            // overrides it.
            let client = openai::OpenAIConfig::with_key(
                &openai::wire::LLAMACPP,
                std::env::var("LLAMACPP_API_KEY").unwrap_or_default(),
            );
            let client = match std::env::var("LLAMACPP_API_BASE_URL") {
                Ok(base_url) => client.with_base_url(llamacpp_base_url(&base_url)),
                Err(_) => client,
            };
            build!(client.client())
        }
        "mira" => {
            check_env!("MIRA_API_KEY");
            build!(mira::from_env()?)
        }
        "minimax" => {
            check_env!("MINIMAX_API_KEY");
            build!(minimax::from_env()?)
        }
        "mistral" => {
            check_env!("MISTRAL_API_KEY");
            build!(mistral::from_env()?)
        }
        "moonshot" => {
            check_env!("MOONSHOT_API_KEY");
            build!(moonshot::from_env()?)
        }
        "ollama" => {
            if std::env::var("OLLAMA_API_BASE_URL").is_err() {
                std::env::set_var("OLLAMA_API_BASE_URL", "http://localhost:11434");
            }
            build!(ollama::Ollama::from_env()?)
        }
        "openrouter" => {
            check_env!("OPENROUTER_API_KEY");
            build!(cached_openrouter openrouter::from_env()?)
        }
        "perplexity" => {
            check_env!("PERPLEXITY_API_KEY");
            build!(perplexity::from_env()?)
        }
        "together" => {
            check_env!("TOGETHER_API_KEY");
            build!(together::from_env()?)
        }
        "xai" => {
            check_env!("XAI_API_KEY");
            build!(xai::from_env()?)
        }
        "xiaomimimo" => {
            check_env!("XIAOMI_MIMO_API_KEY");
            build!(xiaomimimo::from_env()?)
        }
        "zai" => {
            check_env!("ZAI_API_KEY");
            build!(zai::from_env()?)
        }
        // ZAI Anthropic-compatible endpoint (ANTHROPIC_API_BASE_URL). Uses
        // the Anthropic provider with automatic prompt caching. Enable by
        // setting `ZAI_API_BASE=https://api.z.ai/api/anthropic` in the env
        // (or the config provider's `env` block) and using provider `zai`.
        // The `zai-anthropic` alias uses the Anthropic-compatible client
        // directly so automatic caching is available regardless of base URL.
        "zai-anthropic" => {
            check_env!("ZAI_API_KEY");
            // Z.AI's Anthropic-compatible endpoint caches implicitly: rig's
            // Z.AI dialect places no markers and refuses a long retention, so
            // the request is built without either.
            build!(zai::anthropic_from_env()?)
        }
        _ => {
            return Err(crate::PriorError::Other(format!(
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
    /// Content only: this hook's every verb is a `display_*`.
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
    /// The session to persist each completed completion turn into, if any.
    /// `None` for `run_once` (sessionless) and tests.
    pub(crate) session: Option<Arc<crate::session::Session>>,
    /// What the `edit` producer saw and rendered this run, shared with the
    /// persist and history paths so all three use one shape. See [`EditLedger`].
    pub(crate) edit_anchors: Arc<Mutex<EditLedger>>,
}

/// An [`AgentBuilder`] over rig's [`SignedInModel`]: the catalog entry for
/// `vendor/model`, served with a credential `auth` resolves and refreshes per
/// request, so a long session outlives its access token.
///
/// rig 0.45 models a subscription sign-in this way rather than as a client
/// with an OAuth mode. The model's catalog entry (or, when prior serves an id
/// rig's catalog does not list, a synthesized one) names the wire.
///
/// [`SignedInModel`]: rig_core::client::signed_in::SignedInModel
fn signed_in_builder<A>(vendor: &str, model: &str, auth: A) -> Result<AgentBuilder>
where
    A: rig_core::client::signed_in::CredentialSource,
{
    use rig_core::catalog::{Catalog, ModelSpec};
    use rig_core::client::signed_in::SignedInModel;
    use rig_core::effect::HandlerKey;
    use rig_core::providers::registry::ProviderId;

    let reference = format!("{vendor}/{model}");
    // prior ships its own model list and serves ids rig's catalog does not
    // list (Copilot's `gpt-5-codex`, for one). The signed-in model needs a
    // spec only to name its wire, so fall back to a synthesized one: the
    // vendor's format, tool calling on, and no other facts, which refuses no
    // option.
    let spec = match Catalog::builtin().resolve(&reference) {
        Ok(resolved) => resolved.spec.clone(),
        Err(_) => {
            let provider = ProviderId::catalog(vendor).ok_or_else(|| {
                crate::PriorError::Other(format!("`{vendor}` is not a provider rig knows"))
            })?;
            ModelSpec::new(provider, model).with_tools(true)
        }
    };
    let handlers = rig_agent::handlers::Handlers::default();
    let key = HandlerKey::from(SIGNED_IN_MODEL_KEY);
    let signed_in = SignedInModel::new(spec, auth, rig_reqwest::shared())
        .map_err(|error| crate::PriorError::Other(error.to_string()))?;
    handlers
        .register(key.clone(), signed_in)
        .map_err(|error| crate::PriorError::Other(error.to_string()))?;
    Ok(AgentBuilder::over_handlers(handlers, "prior", key))
}

/// The handler key the signed-in model is registered under, inside the
/// builder's own table.
const SIGNED_IN_MODEL_KEY: &str = "model:signed-in";

/// The cache retention to ask an OpenRouter model for.
///
/// A one-hour `cache_control` marker is preferred. The upstreams that cache
/// automatically (`openai/`, `deepseek/`, `x-ai/`, `groq/`) refuse one, and a
/// refusal fails the run, so `check` is asked which this model is: a dry run
/// that sends nothing and reads no message. Only the cache option is set on
/// that request, so a refusal is the cache's.
fn openrouter_cache_retention(
    model: &rig_core::driver::DynModel<rig_core::operation::Completion>,
) -> rig_core::completion::CacheRetention {
    use rig_core::completion::{CacheRetention, CheckError, CompletionRequest, GenerationOptions};

    let long = CompletionRequest::new("cache check")
        .options(GenerationOptions::default().cache(CacheRetention::Long));
    match model.check(&long) {
        Err(CheckError::Unsupported(_)) => CacheRetention::Short,
        _ => CacheRetention::Long,
    }
}

/// The base URL a `llama-server` client sends to.
///
/// rig 0.45's `LLAMACPP` dialect carries `/v1` in its default endpoint, and a
/// `LLAMACPP_API_BASE_URL` override replaces it wholesale, so a bare
/// `http://host:port` would lose the version prefix. prior has always accepted
/// both the bare host `llama-server` prints in its banner and the
/// OpenAI-conventional host plus `/v1`, so add the prefix unless the override
/// already carries it. This is the rule the provider itself applied before
/// 0.45 made it the dialect's job.
fn llamacpp_base_url(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/v1")
    }
}

impl AgentHook for LoggingHook {
    async fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        // Persist the completion turn that just finished, *before* the cancel
        // check below so a cancel-stop still writes it. The turn whose response
        // is about to be requested is complete and valid (every tool call
        // answered), so this is the incremental persist point: mid-turn
        // progress reaches disk by the start of the next call.
        {
            let ledger = self
                .edit_anchors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let _ = crate::agent::transcript::commit(
                &self.transcript,
                self.session.as_deref(),
                &ledger,
                crate::agent::transcript::RunState::Ongoing,
            )
            .await;
            if let Ok(mut t) = self.transcript.lock() {
                t.start_live();
            }
        }
        if self.user_cancel.load(Ordering::SeqCst) {
            return CompletionCallAction::Stop("cancelled by user".into());
        }
        // Build the history to send: strip reasoning when configured, then
        // render every tool call's arguments and every `edit` call's block form
        // from the ledger the producer filled. The composition order is
        // `strip -> normalize -> edit-blocks`; nothing is re-parsed or
        // re-minimized here, so the history cannot drift from what was
        // persisted (both read the same `PreparedEdit`).
        //
        // The transcript is stored canonically (a pi-compatible array); the model is
        // shown the block form *consistently* so it keeps emitting the reliable
        // shape. This hook is the one place that sees the whole request history,
        // so it covers prior and within-turn messages alike.
        let stripped_history = maybe_strip_reasoning(
            event.history.iter().map(|m| (**m).clone()).collect(),
            self.reasoning_in_context,
            &self.provider,
            &self.model,
        );
        let ledger = self
            .edit_anchors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // `strip -> normalize + edit-blocks`: drop the redundant
        // `cd <cwd> && …` prefix on `bash` calls, shorten path arguments (the
        // same pass the execution path and the transcript use), and render each
        // `edit` call in the block form the model should see.
        let history = render_model_facing(
            stripped_history,
            &ledger,
            &crate::tool_args::PathContext::sandbox(),
        );

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
                let msg =
                    assistant_message(event.identity.response_id.clone(), event.content.clone());
                let turn_id = logger.current_turn_id();
                logger.log_message("received", &msg, turn_id).await;
            }
            // Accumulate the accepted turn into the run transcript so a
            // mid-turn failure can persist exactly what completed (see
            // `TurnTranscript` / doc/tool_error_handling.md). This branch
            // returns `Continue`, i.e. the turn was accepted — matching rig's
            // own history commit for the same content.
            if let Ok(mut t) = self.transcript.lock() {
                t.accept(event.identity.response_id.clone(), event.content.clone());
            }
            ModelTurnAction::Continue
        }
    }

    /// A tool call is about to be dispatched: paint it (for the in-flight
    /// panel) and let a pending `<Esc>` cancel the run before the tool body
    /// starts. This replaces rig 0.42's `on_tool_call`.
    async fn on_dispatch(&self, _ctx: &HookContext, event: DispatchEvent<'_>) -> DispatchAction {
        let Some(tool_name) = event.tool_name() else {
            // A completion dispatch (or another family): nothing to paint.
            return DispatchAction::proceed();
        };
        if self.user_cancel.load(Ordering::SeqCst) {
            return DispatchAction::stop("cancelled by user");
        }
        // `edit` paints nothing here: its panel is a *pair* (label + diff, then
        // result), and the call half is not complete until the edits have
        // resolved. `on_outcome` paints the whole thing from the prepared
        // form, so a single-file call now waits for its result too. Editing is
        // fast, and the alternative — a label then a diff then a result in
        // three writes — is what a multi-file call already could not do.
        if tool_name == "edit" {
            return DispatchAction::proceed();
        }
        let args = event.tool_args().unwrap_or("{}");
        let mut args_json =
            serde_json::from_str(args).unwrap_or(serde_json::Value::String(args.to_string()));
        // The `bash` prefix strip and path relativization, so the label reads
        // the same as the stored call and the next turn's history.
        crate::tool_args::normalize(
            tool_name,
            &mut args_json,
            &crate::tool_args::PathContext::sandbox(),
        );
        self.content.display_tool_call(tool_name, &args_json);
        DispatchAction::proceed()
    }

    /// Model called a tool that isn't registered or isn't in the current
    /// turn's allowed set. Rig gates this *before* `on_tool_call`, so the
    /// hook is the only place that can recover without burning a fresh
    /// model call. We re-prompt the model with the available tool set so
    /// a hallucinated name (the common case) self-corrects on the first
    /// retry. The recovery budget is configured on the streaming request
    /// via `.max_invalid_tool_call_retries(1)`; if the model hallucinates
    /// the same wrong name twice, rig exhausts the budget and the run
    /// surfaces a `LlmUnknownTool` `PriorError` to the user.
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

    /// A dispatched effect resolved. Completion outcomes are where a pending
    /// `<Esc>` stops the run; tool outcomes paint the result — and, for `edit`,
    /// prepare the call and paint the whole pair. This replaces rig 0.42's
    /// `on_completion_response` and `on_tool_result`.
    async fn on_outcome(&self, _ctx: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if self.user_cancel.load(Ordering::SeqCst) {
            return OutcomeAction::stop("cancelled by user");
        }
        if event.tool_result().is_none() {
            // A completion (or another family): nothing to paint here.
            return OutcomeAction::proceed();
        }
        let Some(tool_name) = event.tool_name() else {
            return OutcomeAction::proceed();
        };
        let Some(result) = event.tool_result() else {
            return OutcomeAction::proceed();
        };

        // `edit` is the one tool whose panel needs both halves of the call at
        // once (a multi-file call is N call-result pairs, not "N labels now,
        // N results later"), and `on_outcome` is the only place holding all of
        // its inputs: the call id, the raw arguments, the host-only
        // `EditResolutions` and the result text. So this is where the call is
        // prepared — parsed once, minimized, relativized, and rendered into the
        // array (persist/display) and block (history) forms — and everything
        // downstream is a ledger lookup.
        if tool_name == "edit" {
            let resolutions = event
                .tool_context()
                .map(|ctx| ctx.result::<crate::tools::EditResolutions>())
                .unwrap_or(Ok(None))
                .unwrap_or_default();
            let ctx = crate::tool_args::PathContext::sandbox();
            let args = event
                .tool_args()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or(serde_json::Value::Null);
            let text = result.output().as_text();
            let prepared = match event.call_id {
                Some(id) => {
                    let mut ledger = self
                        .edit_anchors
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Insert only on success: the tool attaches
                    // `EditResolutions` after a write.
                    if let Some(res) = resolutions {
                        ledger.insert_resolutions(id.to_string(), res);
                    }
                    let prepared = crate::agent::transcript::prepare_edit_call(
                        id,
                        &args,
                        ledger.get_resolutions(&id.to_string()),
                        text,
                        &ctx,
                    );
                    if let Some(p) = &prepared {
                        ledger.insert_prepared(id.to_string(), p.clone());
                    }
                    prepared
                }
                None => None,
            };
            match prepared {
                Some(prepared) => {
                    self.content
                        .display_prepared_edit(&prepared, &result.output().render());
                }
                // Arguments that do not parse: no panel is honest here beyond
                // the raw call and its error, which is the malformed-call
                // rendering.
                None => {
                    self.content.display_tool_call(tool_name, &args);
                    self.content.display_tool_result(&result.output().render());
                }
            }
            if let Some(text) = text {
                if let Some(cleaned) = model_facing_tool_text(tool_name, text) {
                    return OutcomeAction::rewrite_tool_result(&event, cleaned);
                }
            }
            return OutcomeAction::proceed();
        }

        self.content.display_tool_result(&result.output().render());
        if let Some(text) = result.output().as_text() {
            if let Some(cleaned) = model_facing_tool_text(tool_name, text) {
                return OutcomeAction::rewrite_tool_result(&event, cleaned);
            }
        }
        OutcomeAction::proceed()
    }
}

const DEFAULT_AGENT_PROMPT: &str = r#"You are prior, a coding agent. You are direct, technical, and highly efficient.

## WORKFLOW & STRATEGY
1. **Understand Before Acting**: Always ground your responses in verified facts.
2. **Tool Mastery**:
   - `read`, `edit` and `sed_preview`, : Use to read and edit code.
   - `bash`: Your window to the system: compilation, testing, tree traversal
   - Prefer `rg` over `grep`, and `rg --files | rg <pattern>` over `find`; it is .gitignore-aware and
     much faster.
   - `w3m -dump` for web browsing: Search using https://duckduckgo.com/html/?q=<query>.
   - `pdftotext` for extracting text from PDFs.
3. **Context**: You are working in the directory provided below. Every shell opens in this directory.
   Prefer relative paths for token economy and session portability.
4. Ask questions and propose plans and changes. Do not change plans / designs without approval.
5. Do not repeat yourself. The code and git commit messages are the source of truth for done work.
   Commit after each logical piece of work. Describe the what and why in commit messages.
   Focus implementation docs on *future* work, drop old implementaton plans. Consult `git log` for
   checking on past work.

### GUIDING PRINCIPLES
- **Clean Code**: Follow established patterns in the codebase. Write idiomatic, readable, and well-documented code.
- **Security First**: Be vigilant about security vulnerabilities. Sanitize inputs, avoid hardcoded secrets, and follow least-privilege principles.
- **Minimalism**: Don't add unnecessary dependencies or over-engineer solutions. However, do not
  reinvent the wheel, and propose good library solutions where they exist.
- **Communication**: Keep explanations brief and focused on the "how" and "why" of your technical decisions. If you are unsure, do not guess or over-analyze - ask.
"#;

fn build_rig_agent(
    builder: AgentBuilder,
    yolo: bool,
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

    let mut builder = builder
        .preamble(&system_message)
        // rig 0.45 fails a run whose finish reason is outside its normalized
        // vocabulary. prior serves local servers (llama.cpp, Ollama) and
        // gateways that emit their own codes, and has always treated an
        // unrecognised stop as a normal one, so keep that leniency.
        .accept_unknown_finish_reasons(true)
        // Every surviving tool does a *point* lookup: one path in, resolved
        // through `PathPolicy::resolve_host_arg`, so the sandbox bindings are
        // enforced by construction. Tree traversal is not a tool: it belongs
        // to `bash`, where the namespace makes the bindings authoritative
        // (doc/sandbox.md -> "Tree traversal belongs to the shell"). That
        // removed `ls`, `find`, `remove`, `make_dir`, `move_file` and
        // `copy_file` -- their in-process walkers saw host truth, which under
        // a remapping binding is not what the shell sees.
        .tool(ReadFile)
        .tool(WriteFile)
        .tool(EditFile)
        .tool(GrepText);

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

    let auto_allow = bash_auto_allow.clone();
    // Publish the resolved sandbox config for the shell tools. prior runs a
    // single agent per process, so a process-wide slot is sufficient and keeps
    // the macro-generated tool structs untouched.
    set_sandbox(sandbox.clone());
    builder = builder.tool(guard(
        Bash,
        yolo,
        Some(Arc::new(move |args| {
            bash_auto_allowed(&auto_allow, &args.command)
        })),
        confirm_port.clone(),
    ));
    builder.build()
}

struct Guard<T: Tool> {
    tool: T,
    yolo: bool,
    /// The REPL's end of the confirm port, `None` where there is no REPL to
    /// ask (piped/batch, one-shot `run_once`) — a gated call is then denied.
    ///
    /// With a port, a gated call *awaits* the REPL's answer and the turn's
    /// `select!` keeps running throughout (doc/confirm_as_menu_261002.md).
    confirm: Option<crate::confirm::ConfirmPort>,
    auto_approve: Option<Arc<dyn Fn(&T::Args) -> bool + Send + Sync>>,
}

impl<T: Tool> Guard<T> {
    /// Whether this tool is already cleared for the session.
    ///
    /// Only a port can record an *always* choice (the REPL's `Approvals`
    /// store); without one nothing is ever approved, so every gated call is
    /// asked and — with no port to ask — denied.
    fn already_approved(&self) -> bool {
        match &self.confirm {
            Some(port) => port.approvals().is_approved(T::NAME),
            None => false,
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

    /// Map prior's domain errors into rig's typed error taxonomy so the
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
                // No REPL: nobody to ask, so deny. A piped session's "s"
                // (session) has no port to record it in, and an interactive one
                // reaches this only if the port failed to install — a bug worth
                // seeing rather than papering over (doc/confirm_as_menu_261002.md).
                None => crate::confirm::ConfirmDecision::Deny,
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

/// Whether `command` is covered by the operator's `auto_allow` patterns.
///
/// Each pattern is a regex that must match the **whole** command. Anchoring is
/// deliberate: `ToolSettings::auto_allow` is an *allow*list, so a partial match
/// would approve a command that merely starts with an approved prefix —
/// `^ls -la` would approve `ls -la; rm -rf ~`, which is the opposite of what an
/// operator writing that pattern intends. Patterns are therefore wrapped as
/// `\A(?:…)\z`, so `^`/`$` inside a pattern keep their usual line meaning
/// while the whole string must still be consumed.
///
/// An empty command, a malformed pattern, or a non-match all deny: the failure
/// direction of an allowlist is "ask".
fn bash_auto_allowed(patterns: &[String], command: &str) -> bool {
    let command = command.trim();
    if command.is_empty() {
        return false;
    }
    patterns.iter().any(
        |pattern| match regex::Regex::new(&format!("\\A(?:{pattern})\\z")) {
            Ok(re) => re.is_match(command),
            Err(_) => false,
        },
    )
}

fn guard<T: Tool>(
    tool: T,
    yolo: bool,
    auto_approve: Option<Arc<dyn Fn(&T::Args) -> bool + Send + Sync>>,
    confirm: Option<crate::confirm::ConfirmPort>,
) -> Guard<T> {
    Guard {
        tool,
        yolo,
        confirm,
        auto_approve,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::msglog::MessageLogger;
    use crate::agent::transcript::reasoning_remap_note;
    // ---- the wire alias (doc/261023_model_aliases.md) ----

    fn cfg_with_alias(alias_for: Option<&str>) -> AgentConfig {
        AgentConfig {
            provider: "openrouter".into(),
            model: "claude-fast".into(),
            content: Arc::new(crate::content::Content::quiet()),
            yolo: false,
            quiet: true,
            confirm_port: None,
            params: Default::default(),
            routing: None,
            alias_for: alias_for.map(str::to_string),
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

    /// With no alias the wire pair is the identity.
    #[test]
    fn wire_id_is_the_identity_without_an_alias() {
        let cfg = cfg_with_alias(None);
        assert_eq!(cfg.wire_id(), ("openrouter".into(), "claude-fast".into()));
    }

    /// With an alias the identity stays put and only the wire moves — split at
    /// the first colon so a model id's own colons survive.
    #[test]
    fn wire_id_is_the_alias_target() {
        let cfg = cfg_with_alias(Some("openrouter:anthropic/claude-sonnet-4"));
        assert_eq!(cfg.qualified_model_id(), "openrouter:claude-fast");
        assert_eq!(
            cfg.wire_id(),
            ("openrouter".into(), "anthropic/claude-sonnet-4".into())
        );
        // Cross-vendor: the provider moves too.
        let cfg = cfg_with_alias(Some("anthropic:claude-sonnet-4"));
        assert_eq!(
            cfg.wire_id(),
            ("anthropic".into(), "claude-sonnet-4".into())
        );
    }

    // ---- per-turn overrides: the request path (doc/model_settings_v2.md A.10) ----

    /// prior serves model ids rig's catalog does not list (its Copilot default
    /// is `gpt-5-codex`), so the signed-in path must synthesize a spec rather
    /// than refuse them.
    #[test]
    fn signed_in_builder_accepts_a_model_rig_does_not_list() {
        let auth = copilot::auth::Authenticator::new(
            copilot::auth::AuthSource::ApiKey("test-token".to_string()),
            copilot::auth::MemoryTokenStore::default(),
            copilot::auth::DeviceCodeHandler::default(),
            false,
        );
        assert!(signed_in_builder("copilot", "gpt-5-codex", auth).is_ok());
    }

    /// OpenRouter takes a one-hour cache marker for the upstreams that cache on
    /// request, but refuses one for the upstreams that cache automatically;
    /// asking `check` is what keeps prior from failing those runs (or shipping a
    /// copy of rig's upstream list).
    #[test]
    fn openrouter_asks_for_the_longest_cache_the_upstream_takes() {
        use rig_core::completion::CacheRetention;
        use rig_core::driver::DynModel;
        use rig_core::operation::Completion;

        let retention = |model: &str| {
            let model: DynModel<Completion> = openrouter::new("test-key").completion(model).into();
            openrouter_cache_retention(&model)
        };

        // Anthropic-style upstreams take an explicit breakpoint.
        assert_eq!(
            retention("anthropic/claude-sonnet-4.5"),
            CacheRetention::Long
        );
        // These cache automatically and refuse a one-hour marker.
        assert_eq!(retention("openai/gpt-5.5"), CacheRetention::Short);
        assert_eq!(retention("deepseek/deepseek-chat"), CacheRetention::Short);
    }

    #[test]
    fn llamacpp_base_url_adds_v1_only_when_missing() {
        // A bare host is what `llama-server` prints in its banner.
        assert_eq!(
            llamacpp_base_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            llamacpp_base_url("http://127.0.0.1:8080/"),
            "http://127.0.0.1:8080/v1"
        );
        // The OpenAI-conventional spelling already carries it.
        assert_eq!(
            llamacpp_base_url("http://127.0.0.1:8080/v1"),
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            llamacpp_base_url("http://127.0.0.1:8080/v1/"),
            "http://127.0.0.1:8080/v1"
        );
        // A mount point keeps its path, with the version appended after it,
        // as the provider has always done.
        assert_eq!(
            llamacpp_base_url("https://gw.example/llama"),
            "https://gw.example/llama/v1"
        );
    }

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
        let baked = request_additional_params("openai", "gpt-4o", &params, None);
        // ...and what the per-request path computes for the same params.
        let per_request = request_additional_params("openai", "gpt-4o", &params, None);
        assert_eq!(baked, per_request);
    }

    /// OpenRouter with no routing adds no `provider` object at all: the old
    /// built-in anthropic pin is gone, so the reasoning block is all that
    /// goes out.
    #[test]
    fn the_openrouter_branch_adds_no_preference_without_routing() {
        let params = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::High),
            ..Default::default()
        };
        let v = request_additional_params("openrouter", "anthropic/claude", &params, None)
            .expect("the reasoning block still goes out");
        let obj = v.as_object().expect("an object");
        assert!(
            !obj.contains_key("provider"),
            "no routing means no provider preferences: {obj:?}"
        );
    }

    // ---- user routing (doc/261017_openrouter_provider_selection.md §2) ----

    fn routing() -> crate::model_settings::Routing {
        crate::model_settings::Routing {
            only: Some(vec!["deepinfra/fp8/flex".into()]),
            allow_fallbacks: Some(false),
            ..Default::default()
        }
    }

    /// An explicit routing table reaches the wire as `provider`, verbatim —
    /// nothing is merged into it.
    #[test]
    fn explicit_routing_reaches_the_wire() {
        let params = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::High),
            ..Default::default()
        };
        let r = routing();
        let v = request_additional_params("openrouter", "anthropic/claude", &params, Some(&r))
            .expect("routing is a preference");
        let provider = &v["provider"];
        assert_eq!(provider["only"][0], "deepinfra/fp8/flex");
        assert_eq!(provider["allow_fallbacks"], false);
        assert!(
            provider.get("order").is_none(),
            "nothing may be added on top of explicit routing: {provider:?}"
        );
    }

    /// No routing and no params to send: nothing at all goes on the wire, so
    /// no hidden upstream pin can reach the request.
    #[test]
    fn absent_routing_sends_no_provider_preferences() {
        let params = crate::params::ModelParams::default();
        assert_eq!(
            request_additional_params("openrouter", "anthropic/claude", &params, None),
            None
        );
    }

    /// An empty table is no opinion: identical to an absent key (the settings
    /// record normalizes it away before it can get here).
    #[test]
    fn an_empty_routing_table_behaves_like_an_absent_one() {
        let params = crate::params::ModelParams::default();
        let empty = crate::model_settings::Routing::default();
        let with_empty =
            request_additional_params("openrouter", "anthropic/claude", &params, Some(&empty));
        let without = request_additional_params("openrouter", "anthropic/claude", &params, None);
        assert_eq!(with_empty, without);
    }

    /// Routing is carried but unused off openrouter: the request must be
    /// byte-identical to one with no routing at all.
    #[test]
    fn routing_is_inert_off_openrouter() {
        let params = crate::params::ModelParams::default();
        let r = routing();
        assert_eq!(
            request_additional_params("openai", "gpt-4o", &params, Some(&r)),
            request_additional_params("openai", "gpt-4o", &params, None)
        );
    }

    /// The bake/per-turn parity invariant with routing present: both paths
    /// recompose from the same typed routing, so an override turn cannot lose
    /// the user's preference.
    #[test]
    fn routing_survives_recomposition_on_an_override_turn() {
        let params = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::High),
            ..Default::default()
        };
        let r = routing();
        let baked = request_additional_params("openrouter", "anthropic/claude", &params, Some(&r));
        let per_request =
            request_additional_params("openrouter", "anthropic/claude", &params, Some(&r));
        assert_eq!(baked, per_request);
        assert_eq!(baked.unwrap()["provider"]["only"][0], "deepinfra/fp8/flex");
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
        let with_override = request_additional_params("openai", "gpt-4o", &effective, None);
        let without = request_additional_params("openai", "gpt-4o", &live, None);
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
        use rig_agent::streaming::{Item, StreamEvent};
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

        let hook_content = Arc::new(crate::content::Content::quiet());
        let mut stream = agent
            .prompt("hi")
            .add_hook(LoggingHook {
                content: hook_content,
                user_cancel: Arc::new(AtomicBool::new(false)),
                message_logger: Some(logger.clone()),
                reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
                provider: "test".to_string(),
                model: "test-model".to_string(),
                session: None,
                transcript: Arc::new(std::sync::Mutex::new(super::TurnTranscript::default())),
                edit_anchors: Arc::new(std::sync::Mutex::new(Default::default())),
            })
            .max_turns(1)
            .stream();

        let mut final_text = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Text {
                    text,
                    ..
                }))) => final_text.push_str(&text),
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

    // ---- `auto_allow` is a whole-command allowlist ----

    fn pats(p: &[&str]) -> Vec<String> {
        p.iter().map(|s| s.to_string()).collect()
    }

    /// A pattern must cover the command *entirely*. Approving on a prefix is
    /// the bug this guards: `^ls -la` approving `ls -la; rm -rf ~` would turn
    /// a read-only-looking allowlist into unrestricted shell.
    #[test]
    fn auto_allow_requires_a_whole_command_match() {
        let allow = pats(&["^ls -la$"]);
        assert!(bash_auto_allowed(&allow, "ls -la"));
        assert!(
            bash_auto_allowed(&allow, "  ls -la  "),
            "outer space is trimmed"
        );
        assert!(
            !bash_auto_allowed(&allow, "ls -la; rm -rf ~"),
            "a trailing injection must not ride an approved prefix"
        );
        assert!(!bash_auto_allowed(&allow, "ls -la /etc"));
    }

    /// The historical example spelling `^cmd` keeps working, but as an *exact*
    /// match: the wrapper's `\z` still applies, so `^` no longer smuggles in
    /// "matches at the start". Operators who want a family spell it out
    /// (`^cargo (test|check|build)`).
    #[test]
    fn auto_allow_anchors_a_pattern_that_looks_partial() {
        let allow = pats(&["^git status"]);
        assert!(bash_auto_allowed(&allow, "git status"));
        assert!(
            !bash_auto_allowed(&allow, "git status --short"),
            "`^git status` is exact; a prefix match is not what it means"
        );
        assert!(!bash_auto_allowed(&allow, "git status && curl evil"));

        // A family is expressible, still exactly.
        let family = pats(&["^cargo (test|check|build)( .*)?$"]);
        assert!(bash_auto_allowed(&family, "cargo test"));
        assert!(bash_auto_allowed(&family, "cargo test --lib"));
        assert!(!bash_auto_allowed(&family, "cargo test; rm -rf ~"));
    }

    /// Failing direction is "ask": no patterns, an empty command, or a
    /// malformed regex all deny.
    #[test]
    fn auto_allow_fails_closed() {
        assert!(!bash_auto_allowed(&[], "ls"));
        assert!(!bash_auto_allowed(&pats(&["^ls$"]), "   "));
        assert!(!bash_auto_allowed(&pats(&["^ls("]), "ls"));
    }

    /// Alternation must not escape the wrapper: `a|b` stays inside the
    /// non-capturing group, so it means "a or b", not "… or b…anything".
    #[test]
    fn auto_allow_alternation_stays_grouped() {
        let allow = pats(&["^ls$|^pwd$"]);
        assert!(bash_auto_allowed(&allow, "ls"));
        assert!(bash_auto_allowed(&allow, "pwd"));
        assert!(!bash_auto_allowed(&allow, "ls; curl evil"));
    }
}
