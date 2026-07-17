use crate::output::Confirmation;
use crate::repl::{InputAction, Repl, ReplCtx};
use crate::tools::{
    AgentBrowser, Bash, CopyFile, EditFile, GlobFiles, GrepText, ListDir, MakeDir, MoveFile,
    ReadFile, Remove, WriteFile,
};
use crate::Output;
use crate::Result;
use crate::model_catalog;
use crate::thinking;
use console::style;
use rig::agent::{Agent, AgentBuilder, CancelSignal, PromptHook};
use rig::client::{CompletionClient, ProviderClient};
use rig::completion::{CompletionModel, Prompt, ToolDefinition};
use rig::message::Message;
use rig::providers::{
    anthropic, azure, cohere, deepseek, galadriel, gemini, groq, huggingface, hyperbolic, mira,
    mistral, moonshot, ollama, openai, openrouter, perplexity, together, xai,
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
    async fn prompt(&self, input: &str, history: Option<&mut Vec<Message>>) -> Result<String> {
        self.output.display_thinking("Thinking...");
        // Fresh request: clear any stale cancel flag from a previous abort so a
        // prior `Esc` doesn't silently cancel this one.
        self.user_cancel.store(false, Ordering::SeqCst);
        let mut builder = self
            .agent
            .prompt(input)
            .with_hook(LoggingHook {
                output: self.output.clone(),
                user_cancel: self.user_cancel.clone(),
            })
            .multi_turn(self.tool_call_limit);
        if let Some(h) = history {
            builder = builder.with_history(h);
        }
        let response = builder
            .await
            .map_err(|e| crate::PicocodeError::Other(e.to_string()))?;
        self.output.stop_thinking();
        Ok(response.to_string())
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
        let response = self.prompt(&input, None).await?;
        self.output.display_text(&response);
        Ok(response)
    }
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
}

pub async fn create_agent(config: AgentConfig) -> Result<Box<dyn PicoAgent>> {
    let provider = config.provider.to_lowercase();
    let model = config.model.clone();
    let think_json = thinking::mapping_opt(&provider, config.thinking_level.as_deref());

    macro_rules! build {
        ($client:expr) => {{
            let builder = $client.agent(&model);
            let rig_agent = build_rig_agent(
                builder,
                config.yolo,
                config.output.clone(),
                config.system_message_extension,
                config.persona_prompt,
                config.bash_auto_allow.unwrap_or_default(),
                config.agent_prompt,
                think_json,
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
    }

    let agent: Box<dyn PicoAgent> = match provider.as_str() {
        "anthropic" => {
            check_env!("ANTHROPIC_API_KEY");
            build!(anthropic::Client::from_env())
        }
        "openai" => {
            check_env!("OPENAI_API_KEY");
            build!(openai::Client::from_env())
        }
        "azure" => {
            check_env!("AZURE_OPENAI_API_KEY");
            check_env!("AZURE_OPENAI_ENDPOINT");
            build!(azure::Client::from_env())
        }
        "cohere" => {
            check_env!("COHERE_API_KEY");
            build!(cohere::Client::from_env())
        }
        "deepseek" => {
            check_env!("DEEPSEEK_API_KEY");
            build!(deepseek::Client::from_env())
        }
        "galadriel" => {
            check_env!("GALADRIEL_API_KEY");
            build!(galadriel::Client::from_env())
        }
        "gemini" | "google" => {
            check_env!("GOOGLE_API_KEY");
            build!(gemini::Client::from_env())
        }
        "groq" => {
            check_env!("GROQ_API_KEY");
            build!(groq::Client::from_env())
        }
        "huggingface" => {
            check_env!("HF_TOKEN");
            build!(huggingface::Client::from_env())
        }
        "hyperbolic" => {
            check_env!("HYPERBOLIC_API_KEY");
            build!(hyperbolic::Client::from_env())
        }
        "mira" => {
            check_env!("MIRA_API_KEY");
            build!(mira::Client::from_env())
        }
        "mistral" => {
            check_env!("MISTRAL_API_KEY");
            build!(mistral::Client::from_env())
        }
        "moonshot" => {
            check_env!("MOONSHOT_API_KEY");
            build!(moonshot::Client::from_env())
        }
        "ollama" => {
            if std::env::var("OLLAMA_API_BASE_URL").is_err() {
                std::env::set_var("OLLAMA_API_BASE_URL", "http://localhost:11434");
            }
            build!(ollama::Client::from_env())
        }
        "openrouter" => {
            check_env!("OPENROUTER_API_KEY");
            build!(openrouter::Client::from_env())
        }
        "perplexity" => {
            check_env!("PERPLEXITY_API_KEY");
            build!(perplexity::Client::from_env())
        }
        "together" => {
            check_env!("TOGETHER_API_KEY");
            build!(together::Client::from_env())
        }
        "xai" => {
            check_env!("XAI_API_KEY");
            build!(xai::Client::from_env())
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

impl<M: CompletionModel> PromptHook<M> for LoggingHook {
    async fn on_completion_call(
        &self,
        _prompt: &Message,
        _history: &[Message],
        cancel_sig: CancelSignal,
    ) {
        if self.user_cancel.load(Ordering::SeqCst) {
            cancel_sig.cancel();
        }
    }

    async fn on_completion_response(
        &self,
        _prompt: &Message,
        _response: &rig::completion::CompletionResponse<M::Response>,
        cancel_sig: CancelSignal,
    ) {
        if self.user_cancel.load(Ordering::SeqCst) {
            cancel_sig.cancel();
        }
    }

    async fn on_tool_call(
        &self,
        tool_name: &str,
        _tool_call_id: Option<String>,
        args: &str,
        cancel_sig: CancelSignal,
    ) {
        if self.user_cancel.load(Ordering::SeqCst) {
            cancel_sig.cancel();
        }
        let args_json =
            serde_json::from_str(args).unwrap_or(serde_json::Value::String(args.to_string()));
        self.output.display_tool_call(tool_name, &args_json);
    }

    async fn on_tool_result(
        &self,
        _tool_name: &str,
        _tool_call_id: Option<String>,
        _args: &str,
        result: &str,
        cancel_sig: CancelSignal,
    ) {
        if self.user_cancel.load(Ordering::SeqCst) {
            cancel_sig.cancel();
        }
        self.output.display_tool_result(result);
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

use rig::tool::Tool;

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

    async fn definition(&self, prompt: String) -> ToolDefinition {
        self.tool.definition(prompt).await
    }

    async fn call(&self, args: Self::Args) -> std::result::Result<Self::Output, Self::Error> {
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
        self.tool.call(args).await
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
pub async fn run_interactive(config: AgentConfig) -> Result<()> {
    let mut config = config;
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
                r.read_input(&prompt_str, &mut line_history, &ctx, config.output.cursor_row())
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
                        config.output.display_plain(&format!(
                            "model -> {}/{}",
                            config.provider, config.model
                        ));
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
                                let msg = format!("thinking level -> {}", level.as_str());
                                match repl.as_mut() {
                                    Some(_) => config.output.display_plain(&msg),
                                    None => config.output.display_plain(&msg),
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
                        let handle = crate::repl::spawn_in_flight_reader(
                            g.clone(),
                            cancel.clone(),
                            done.clone(),
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
                    Ok(r) => {
                        config.output.display_text(&r);
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
