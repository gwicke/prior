use crate::output::Confirmation;
use crate::repl::{InputAction, Repl, ReplCtx};
use crate::settings::SettingsStore;
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
    HookContext, ModelTurnAction, ModelTurnFinished, ObservationAction, ToolCall, ToolCallAction,
    ToolResultAction, ToolResultEvent,
};
use rig_core::tool::ToolContext;
use rig_core::client::{CompletionClient, ProviderClient};
use rig_core::completion::CompletionModel;
use rig_core::message::Message;
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
use std::io::IsTerminal;
use std::sync::Arc;
use async_trait::async_trait;

#[async_trait]
pub trait PicoAgent: Send + Sync {
    async fn prompt(&self, input: &str, history: Option<&mut Vec<Message>>) -> Result<String>;
    async fn run_once(&self, input: String) -> Result<String>;
}

#[async_trait]
impl<M: CompletionModel + 'static> PicoAgent for CodeAgent<M> {
    async fn prompt(
        &self,
        input: &str,
        history: Option<&mut Vec<Message>>,
    ) -> Result<String> {
        self.output.display_thinking("Thinking...");
        // Fresh request: clear any stale cancel flag from a previous abort so a
        // prior `Esc` doesn't silently cancel this one.
        self.user_cancel.store(false, Ordering::SeqCst);

        // Clone the prior history (owned) so we can still write the updated
        // history back through the `&mut` borrow afterwards.
        let prior: Vec<Message> = history.as_ref().map(|h| (*h).clone()).unwrap_or_default();

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
        self.output.begin_response();
        while let Some(item) = stream.next().await {
            match item.map_err(|e| crate::PicocodeError::Other(e.to_string()))? {
                MultiTurnStreamItem::StreamAssistantItem(content) => match content {
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
                MultiTurnStreamItem::StreamUserItem(_) => {}
                MultiTurnStreamItem::CompletionCall(_) => {
                    // Commit the text so far before the tool call / result line.
                    self.output.end_response_segment();
                }
                MultiTurnStreamItem::FinalResponse(resp) => {
                    final_history = resp.messages().map(|h| h.to_vec());
                }
                MultiTurnStreamItem::ToolExecutionCommitted { .. } => {}
                MultiTurnStreamItem::ModelTurnRetried { .. } => {}
                _ => {}
            }
        }
        self.output.end_response();

        // Persist history across turns (§2.4 fix): `history` is cloned into
        // the request, so we must write the updated history back here.
        if let Some(h) = history {
            if let Some(fh) = final_history {
                *h = fh;
            }
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
    /// Resolved sandbox configuration for shell tool execution. `None` means
    /// sandboxing is disabled (plain `sh_dangerous`, timeout still enforced).
    pub sandbox: Option<crate::config::SandboxConfig>,
}

pub async fn create_agent(config: AgentConfig) -> Result<Box<dyn PicoAgent>> {
    let provider = config.provider.to_lowercase();
    let model = config.model.clone();
    let think_json = thinking::mapping_opt(&provider, config.thinking_level.as_deref());

    macro_rules! build {
        // Anthropic: enable automatic prompt caching so the static prefix
        // (system prompt + tool definitions + growing conversation history) is
        // cached server-side and billed as cache reads instead of full input
        // tokens on every turn. Requires no beta header; the API advances the
        // cache breakpoint automatically as the history grows.
        (anthropic $client:expr) => {{
            let cached_model = $client
                .completion_model(&model)
                .with_automatic_caching();
            build!(@builder AgentBuilder::new(cached_model))
        }};
        // OpenRouter: enable prompt caching so the system-prompt prefix is
        // marked with `cache_control: {"type": "ephemeral"}` and billed at the
        // cache-hit rate on subsequent turns. Only effective when the routed
        // upstream model supports explicit cache breakpoints (e.g. `anthropic/...`).
        (openrouter $client:expr) => {{
            let cached_model = $client
                .completion_model(&model)
                .with_prompt_caching();
            build!(@builder AgentBuilder::new(cached_model))
        }};
        ($client:expr) => {{
            build!(@builder $client.agent(&model))
        }};
        (@builder $builder:expr) => {{
            let rig_agent = build_rig_agent(
                $builder,
                config.yolo,
                config.output.clone(),
                config.system_message_extension,
                config.persona_prompt,
                config.bash_auto_allow.unwrap_or_default(),
                config.agent_prompt,
                think_json,
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
            build!(chatgpt::Client::from_env()?)
        }
        "copilot" => {
            check_env!("GITHUB_COPILOT_API_KEY", "COPILOT_GITHUB_ACCESS_TOKEN");
            build!(copilot::Client::from_env()?)
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
            build!(zai::Client::from_env()?)
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
   - `agent_browser`: Use for external documentation, searching for solutions, or web-related debugging.
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

/// Interactive chat loop. Owns the `Box<dyn PicoAgent>` and the `AgentConfig`
/// so slash commands that change model/provider/thinking can rebuild the agent
/// via `create_agent` (required because `CodeAgent<M>` is generic over the
/// provider's completion-model type).
pub async fn run_interactive(config: AgentConfig, settings: &mut SettingsStore) -> Result<()> {
    let mut config = config;
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

    let mut history = Vec::new();
    let mut line_history = crate::repl::History::load();
    let mut recent_models: Vec<String> = Vec::new();
    // Shared cancel flag: the in-flight reader thread sets it on `Esc`; the
    // `LoggingHook` bridge turns it into a rig `CancelSignal` that aborts the
    // current request.
    let cancel = Arc::new(AtomicBool::new(false));
    // So `/model` etc. embed the same flag into the rebuilt agent.
    config.user_cancel = cancel.clone();
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
                let ctx = ReplCtx {
                    provider: config.provider.clone(),
                    model: config.model.clone(),
                    recent: recent_models.clone(),
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
                        recent_models.push(format!("{}/{}", config.provider, config.model));
                        // No confirmation line: the `/model <provider:model>` input
                        // line (emitted by `read_input`) is the record of the change.
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
                                // No confirmation line: the `/thinking <level>` input is self-documenting.
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
            InputAction::Submit(input) => {
                if input.is_empty() {
                    continue;
                }
                line_history.add(&input);
                if input == "/q" || input == "exit" {
                    break;
                }
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
                            done.clone(),
                            confirm_gate,
                        );
                        let resp = agent
                            .as_ref()
                            .unwrap()
                            .prompt(&input, Some(&mut history))
                            .await;
                        done.store(true, Ordering::SeqCst);
                        let _ = handle.join();
                        resp
                    } else {
                        agent
                            .as_ref()
                            .unwrap()
                            .prompt(&input, Some(&mut history))
                            .await
                    }
                };
                match response {
                    Ok(_) => {}
                    Err(e) => {
                        config.output.display_text(&format!("Error: {}", e));
                    }
                }
            }
        }
    }
    Ok(())
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
        }
    }
}

#[cfg(test)]
mod tests {
    use rig_core::message::Reasoning;

    use super::reasoning_text;

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
}
