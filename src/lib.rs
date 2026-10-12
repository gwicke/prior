use thiserror::Error;

pub mod agent;
pub mod ansi;
pub mod config;
pub mod draw;
pub mod edit_blocks;
pub mod edit_match;
pub mod editor;
pub mod output;
pub mod persona;
pub mod platform;
pub mod tools;
pub use slashdot_menu;
pub mod app_state;
pub mod confirm;
pub mod interactive;
pub mod model_catalog;
pub mod model_settings;
pub mod params;
pub mod path_complete;
pub mod path_util;
pub mod prompt_complete;
pub mod render;
pub mod repl;
pub mod row_count;
pub mod sandbox_state;
pub mod session;
pub mod settings;
pub mod settings_editor;
pub mod termio;
pub mod turn_overrides;
pub mod v25_clear;
pub mod v25_commands;
pub mod v25_listing_node;
pub mod v25_menu;
pub mod v25_model;
pub mod v25_new;
pub mod v25_number_param;
pub mod v25_persona;
pub mod v25_quit;
pub mod v25_reasoning;
pub mod v25_replay;
pub mod v25_sandbox;
pub mod v25_session;
pub mod v25_set;
pub mod v25_stat;
pub mod v25_tree;

// Table + fuzzy primitives moved into the `slashdot_menu` crate; re-exported
// here so internal paths (`crate::table`, `crate::fuzzy`) keep working.
pub use slashdot_menu::{fuzzy, table};

pub use config::{Config, Recipe, ToolSettings};
pub use settings::{SettingKey, SettingsStore, ShowReasoning, ShowReasoningHandle};

// Re-export core rig types for library users
pub use rig_agent::agent::AgentBuilder;
pub use rig_core::client::{CompletionClient, ProviderClient};
pub use rig_core::completion::CompletionModel;
pub use rig_core::providers;

pub use agent::build::{create_agent, load_agents_md, AgentConfig};
pub use agent::CodeAgent;
pub use interactive::run_interactive;
pub use output::{Confirmation, ConsoleOutput, LogOutput, NoOutput, Output, QuietOutput};

/// A provider's own explanation of a failure, reduced to one readable line.
///
/// A newtype rather than a bare `Option<String>` so `Display` can render the
/// separator itself: the `#[error(...)]` attributes interpolate the suffix
/// unconditionally, and `Option` has no useful `Display`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ErrorDetail(pub Option<String>);

impl std::fmt::Display for ErrorDetail {
    /// Empty for `None`, so `(HTTP 400)` reads correctly when the body carried
    /// nothing actionable rather than trailing a bare `; `.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(d) => write!(f, "; {d}"),
            None => Ok(()),
        }
    }
}

#[derive(Error, Debug)]
pub enum PicocodeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("TOML error: {0}")]
    Toml(String),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("LLM error: {0}")]
    Llm(String),

    /// Provider returned 401 or 403: API key is missing, invalid, or expired.
    /// The REPL must not auto-retry this — the user has to fix their key.
    #[error("Authentication failed (HTTP {status}). Check your API key for this provider.")]
    LlmAuth {
        /// HTTP status code reported by the provider (401 or 403).
        status: u16,
    },

    /// Provider returned 402: account is out of credits or behind a paywall.
    /// The REPL must not auto-retry this.
    #[error("Billing/quota error (HTTP {status}). Add credits or switch providers.")]
    LlmBilling {
        /// HTTP status code reported by the provider.
        status: u16,
    },

    /// Provider returned 429 (rate-limited). When `retry_after_secs` is `Some`
    /// it is taken from the provider's `Retry-After` header (or the OpenRouter
    /// envelope's `metadata.retry_after_seconds`); when `None` the provider
    /// signalled rate-limit without a hint. The REPL uses `retry_after_secs`
    /// as the floor of the first back-off (then doubles per consecutive
    /// failure) and retries up to the per-turn budget (see [`retry_budget`]).
    /// [`retry_budget`]: crate::agent::retry_budget
    #[error(
        "Rate-limited by provider (HTTP 429){}.",
        match .retry_after_secs { Some(s) => format!("; retry-after {s}s"), None => String::new() }
    )]
    LlmRateLimited {
        /// `Retry-After` from the provider, when reported.
        retry_after_secs: Option<u64>,
    },

    /// Provider returned a 5xx. Transient by default — the REPL re-prompts once.
    ///
    /// Carries a detail for the same reason as [`Self::LlmClient`]: "model is
    /// overloaded" and "upstream is out of capacity" are both things the user
    /// can act on, and both arrive in the body rather than the status.
    // The period was inside the parens (`(HTTP {status}.)`), a long-standing
    // typo; `{detail}` now sits before it so the no-detail case still renders
    // exactly as it did.
    #[error("Upstream provider error (HTTP {status}){detail}.")]
    LlmUpstream {
        /// HTTP status code reported by the provider.
        status: u16,
        /// The provider's explanation, as for [`Self::LlmUpstream`].
        detail: ErrorDetail,
    },

    /// Provider returned a 4xx other than 401/402/403/429. Likely a malformed
    /// request — retrying won't help, so the provider's own explanation of
    /// *what* it rejected is carried alongside the status: for a 400 that is
    /// the whole diagnostic, and a bare status code sends the user guessing.
    /// The catalog cannot substitute for this — a model's published
    /// `supported_parameters` is advisory and often silent, so a rejected
    /// value is learned from the 400, not predicted from metadata.
    #[error("Provider rejected the request (HTTP {status}){detail}.")]
    LlmClient {
        /// HTTP status code reported by the provider.
        status: u16,
        /// The provider's explanation, as for [`Self::LlmUpstream`].
        detail: ErrorDetail,
    },

    /// Transport-level failure with no captured HTTP status: connection
    /// refused, DNS, TLS, mid-stream drop, request timeout. Transient by
    /// default — the REPL re-prompts once.
    #[error("Network/transport error: {0}.")]
    LlmTransport(String),

    /// The streaming protocol violated expectations but had no captured HTTP
    /// status: the provider stream ended without a terminal record, or the
    /// SSE format diverged. Transient by default.
    #[error("Streaming protocol error: {0}.")]
    LlmStreamProtocol(String),

    /// Provider stream assembled a tool call whose JSON arguments were
    /// malformed (rig's `tool call \`{name}\` arrived with malformed JSON
    /// input`). The model gets a synthetic user message describing the parse
    /// error so it can self-correct, and the REPL re-prompts. This includes
    /// both stochastic subclasses (truncation at an argument boundary,
    /// `expected , or }`, `EOF while parsing`) and the content-echo subclass
    /// (raw control characters inside a string — the model copied file or
    /// terminal content verbatim into an argument). All are recoverable by
    /// feeding the parse error back to the model, which mirrors the manual
    /// workflow of pasting the error and letting the model retry; the feedback
    /// text is what [`synthetic_retry_feedback`](Self::synthetic_retry_feedback)
    /// provides so that workflow happens automatically.
    #[error(
        "Provider returned malformed JSON for tool call `{tool_name}`: {parse_error}. Asking the model to retry."
    )]
    LlmMalformedToolCall {
        /// Tool name from the unparseable call.
        tool_name: String,
        /// `serde_json` error message identifying the parse failure.
        parse_error: String,
    },

    /// Model called a tool that isn't registered or allowed for this turn,
    /// and no `on_invalid_tool_call` hook recovered it. Surface the
    /// `available_tools` so the user can see what the model could have used.
    #[error("Model called unknown or disallowed tool `{tool_name}`. Available: {available:?}.")]
    LlmUnknownTool {
        /// Tool name emitted by the model.
        tool_name: String,
        /// Tools the runtime could dispatch on this turn.
        available: Vec<String>,
    },

    /// The configured total model-call budget was exhausted.
    #[error("Reached the configured max-turns limit ({max_turns}). Increase `tool_call_limit` or simplify the request.")]
    LlmMaxTurns {
        /// Configured total model-call budget.
        max_turns: usize,
    },

    /// A transient, auto-retryable error (429, transport, 5xx, malformed
    /// tool-call JSON, …) interrupted the run *after* some progress had
    /// already been persisted to the session as a canonical partial
    /// transcript. The REPL must continue from that transcript with a
    /// steering message rather than re-sending the original input — the
    /// input is already in the session as the persisted turn's first
    /// message, so re-sending it would duplicate the user's request.
    ///
    /// The wrapped [`cause`](Self::cause) carries the classification the
    /// user sees (e.g. `LlmRateLimited`) and the back-off hint
    /// (`retry_after_secs()`); the accessors on this variant delegate to it.
    #[error("{}", .cause)]
    LlmInterrupted {
        /// The underlying classified error.
        cause: Box<PicocodeError>,
    },

    #[error("Missing API key for provider {0}. Please set the {1} environment variable.")]
    MissingApiKey(String, String),

    #[error("Provider client error: {0}")]
    ProviderClient(#[from] rig_core::client::ProviderClientError),

    #[error("Regex error: {0}")]
    Regex(#[from] regex::Error),

    #[error("Other error: {0}")]
    Other(String),

    #[error("Cancelled: {0}")]
    Cancelled(String),
}

impl PicocodeError {
    /// Whether the REPL should automatically re-prompt once for this error.
    ///
    /// True for transient failures (rate-limit, transport, upstream 5xx,
    /// streaming protocol, malformed streamed tool-call JSON — including the
    /// raw-control-character content-echo subclass). False for permanent
    /// configuration issues (auth, billing, client 4xx, max-turns, unknown
    /// tool). Always check this *before* retrying so a single failure does not
    /// loop forever if the underlying problem is persistent.
    pub fn is_auto_retryable(&self) -> bool {
        match self {
            PicocodeError::LlmInterrupted { cause } => cause.is_auto_retryable(),
            _ => matches!(
                self,
                PicocodeError::LlmRateLimited { .. }
                    | PicocodeError::LlmUpstream { .. }
                    | PicocodeError::LlmTransport(_)
                    | PicocodeError::LlmStreamProtocol(_)
                    | PicocodeError::LlmMalformedToolCall { .. }
            ),
        }
    }

    /// The `Retry-After` hint for a rate-limited response, if the provider
    /// reported one. `None` for every other error variant. Delegates through
    /// [`LlmInterrupted`](Self::LlmInterrupted) to the wrapped cause.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            PicocodeError::LlmRateLimited { retry_after_secs } => *retry_after_secs,
            PicocodeError::LlmInterrupted { cause } => cause.retry_after_secs(),
            _ => None,
        }
    }

    /// Synthetic user feedback to inject into the session for the auto-retry
    /// on a [`LlmMalformedToolCall`](Self::LlmMalformedToolCall) — every
    /// subclass, including raw control characters in a string. The model
    /// emitted a tool call whose arguments failed JSON parsing at the wire's
    /// terminal marker, so the model never saw the error. Without this
    /// feedback a blind retry would likely reproduce the same broken call.
    /// Mirrors the rig-run `InvalidToolCallAction::Retry { feedback }`
    /// pattern, applied at the prompt-loop layer because rig's
    /// `on_invalid_tool_call` hook does not fire for parse errors — only for
    /// *name validation* failures.
    ///
    /// This automates the manual workflow of pasting the parse error into the
    /// next prompt and letting the model retry.
    pub fn synthetic_retry_feedback(&self) -> Option<String> {
        match self {
            PicocodeError::LlmInterrupted { cause } => cause.synthetic_retry_feedback(),
            PicocodeError::LlmMalformedToolCall {
                tool_name,
                parse_error,
            } => Some(format!(
                "Your previous response called `{tool_name}` with arguments that could not be \
                 parsed as JSON: {parse_error}. Please re-emit the call with a single, \
                 well-formed JSON arguments object. If the previous `old_text` was very long, \
                 prefer a shorter unique anchor."
            )),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, PicocodeError>;

/// Write-back callback for a menu node: takes the value the user selected and
/// commits it, `Ok(())` on success and an `Err` message to show on failure.
///
/// Every select-one node (model, persona, reasoning level) takes the same
/// shape, so it is spelled once here rather than repeated at each field.
pub type SubmitFn = Box<dyn FnMut(&str) -> std::result::Result<(), String>>;

pub mod ui;

#[cfg(test)]
mod test_hang_reader;
