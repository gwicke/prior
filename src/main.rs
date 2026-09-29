use clap::{Parser, Subcommand};
use picocode::{config::Config, create_agent, AgentConfig, ConsoleOutput, SettingsStore};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(author, version, about = "Minimal coding assistant")]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Positional prompt (shortcut for 'input')
    #[arg(index = 1)]
    prompt: Option<String>,

    /// LLM provider (anthropic, openai, azure, cohere, chatgpt, copilot, deepseek, doubleword, gemini, groq, huggingface, hyperbolic, llamafile, mira, minimax, mistral, moonshot, ollama, openrouter, perplexity, together, xai, xiaomimimo, zai).
    /// For chatgpt/copilot, set the token env var to `oauth` to sign in interactively (device-code flow); the cached refresh token is stored under the settings `token/` directory.
    #[arg(short, long, global = true)]
    provider: Option<String>,

    /// LLM model name
    #[arg(short, long, global = true)]
    model: Option<String>,

    /// Run destructive tools without confirmation
    #[arg(long, global = true)]
    yolo: Option<bool>,

    /// Run in quiet mode
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Maximum number of tool calls per prompt
    #[arg(long, global = true)]
    tool_call_limit: Option<usize>,

    /// Choose a persona for the agent
    #[arg(long, help = format!("Choose a persona for the agent. Available built-in personas:\n{}", picocode::persona::list_personas()), global = true)]
    persona: Option<String>,

    /// Path to config file (default: ~/.config/picocode/settings.toml; --config overrides)
    #[arg(short, long, global = true)]
    config: Option<String>,

    /// Log available providers and debug model catalog extrapolation/filtering
    #[arg(long, global = true)]
    debug_providers: bool,

    /// Log raw messages to a JSONL file for debugging
    #[arg(long, global = true, value_name = "FILE")]
    log_messages: Option<String>,

    /// Auto-resume the most recent session for the current directory at
    /// startup. Equivalent to opening the interactive chat, typing `/session`,
    /// pressing Tab to pick the top entry, and Enter to confirm `resume`.
    /// If no session exists yet for this CWD, a fresh one is started and
    /// a one-line note is printed (so first-launch flows don't hard-error).
    #[arg(short = 'r', long, global = true)]
    resume: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start an interactive chat session (default)
    Chat,
    /// Run a single prompt
    Input { prompt: String },
    /// Run a pre-defined recipe from the config file
    Recipe { name: String },
}

/// External binaries picocode depends on at runtime. We fail closed at
/// startup (rather than degrading) because both are integral to the sandboxed
/// tool-execution model: `bwrap` provides the command namespace, `rg` backs
/// the grep tool.
const REQUIRED_BINARIES: &[(&str, &str)] =
    &[("bwrap", "bubblewrap (bwrap)"), ("rg", "ripgrep (rg)")];

/// Returns `Ok(())` if every required binary is present and runnable on PATH,
/// otherwise an error describing the missing ones with install hints.
fn check_runtime_deps() -> Result<(), String> {
    let missing: Vec<&str> = REQUIRED_BINARIES
        .iter()
        .filter(|(bin, _)| !is_executable(bin))
        .map(|(_, label)| *label)
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Missing required dependencies: {}.\n\nInstall them and retry:\n  - bubblewrap (bwrap): https://github.com/containers/bwrap#installation\n  - ripgrep (rg):        https://github.com/BurntSushi/ripgrep#installation",
            missing.join(", ")
        ))
    }
}

/// True if `name` resolves to an executable on PATH, probed via `--version`.
fn is_executable(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::main]
async fn main() {
    std::panic::set_hook(Box::new(|info| {
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic".to_string()
        };

        eprintln!(
            "\n\n--------------------------------------------------------------------------------"
        );
        eprintln!("💥 Picocode encountered an unexpected error (panic).");
        eprintln!("Message: {}", message);
        if let Some(location) = info.location() {
            eprintln!(
                "Location: {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            );
        }
        eprintln!(
            "--------------------------------------------------------------------------------"
        );
        eprintln!("This is likely a bug in picocode or one of its dependencies.");
        eprintln!("Please report it at: https://github.com/jondot/picocode/issues");
        eprintln!(
            "--------------------------------------------------------------------------------\n"
        );
    }));

    if let Err(e) = run().await {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Enable debug-providers mode if requested.
    if args.debug_providers {
        picocode::model_catalog::set_debug_providers(true);
    }

    // Log available providers at startup (before any network calls).
    {
        let providers = picocode::model_catalog::list_available_providers();
        let configured: Vec<_> = providers.iter().filter(|(_, avail, _)| *avail).collect();
        let unconfigured: Vec<_> = providers.iter().filter(|(_, avail, _)| !*avail).collect();
        eprintln!("── Providers ──────────────────────────────────");
        eprintln!("  Configured ({}):", configured.len());
        for (name, _, envs) in &configured {
            eprintln!("    {}  ({})", name, envs.join(", "));
        }
        if args.debug_providers {
            eprintln!("  Unconfigured ({}):", unconfigured.len());
            for (name, _, envs) in &unconfigured {
                eprintln!("    {}  (needs: {})", name, envs.join(", "));
            }
        }
        eprintln!("── Providers ──────────────────────────────────");
    }

    // Fail closed: required external tools must be present before we do any
    // work. Degrading (e.g. running unsandboxed) is not safe once `rg` and
    // `bwrap` are assumed by the tool layer.
    if let Err(msg) = check_runtime_deps() {
        eprintln!("{}", msg);
        std::process::exit(1);
    }

    let config = Config::load(args.config.as_deref())?;

    // `--resume` only makes sense in interactive chat mode: it's a session
    // pickup for the REPL. Combining it with `input`/`recipe`/positional
    // prompt would mix two execution semantics (resume implies entering the
    // REPL after the replay; the others run a single turn and exit).
    if args.resume
        && (matches!(
            &args.command,
            Some(Commands::Input { .. } | Commands::Recipe { .. })
        ) || args.prompt.is_some())
    {
        eprintln!("Error: --resume can only be used with the interactive chat (no `input`/`recipe` subcommand and no positional prompt).");
        std::process::exit(2);
    }

    let (command, prompt, recipe_name) = match (&args.command, &args.prompt) {
        (Some(Commands::Recipe { name }), _) => (
            Commands::Recipe { name: name.clone() },
            None,
            Some(name.clone()),
        ),
        (Some(Commands::Input { prompt }), _) => (
            Commands::Input {
                prompt: prompt.clone(),
            },
            Some(prompt.clone()),
            None,
        ),
        (Some(Commands::Chat), _) => (Commands::Chat, None, None),
        (None, Some(p)) => (Commands::Input { prompt: p.clone() }, Some(p.clone()), None),
        (None, None) => (Commands::Chat, None, None),
    };

    let recipe = recipe_name
        .as_ref()
        .and_then(|name| config.recipes.get(name).cloned());

    // Provider/model precedence (CLI > recipe > file `default_model` > built-in):
    // `settings.toml`'s `default_model = "provider:model"` is the file-level
    // fallback that the `/set default_model` menu writes. `default_model` is a
    // single combined string, so when only one of provider/model is overridden
    // (via CLI/recipe) the other falls back to the built-in default model for
    // the resolved provider.
    let file_default = config
        .default_model
        .as_deref()
        .and_then(|s| s.split_once(':'))
        .filter(|(p, m)| !p.trim().is_empty() && !m.trim().is_empty())
        .map(|(p, m)| (p.trim().to_string(), m.trim().to_string()));

    let provider = args
        .provider
        .or_else(|| recipe.as_ref().and_then(|r| r.provider.clone()))
        .or_else(|| file_default.as_ref().map(|(p, _)| p.clone()))
        .unwrap_or_else(|| "anthropic".to_string());

    let model = args
        .model
        .or_else(|| recipe.as_ref().and_then(|r| r.model.clone()))
        .or_else(|| file_default.as_ref().map(|(_, m)| m.clone()))
        .unwrap_or_else(|| default_model(&provider));

    let yolo = if let Some(y) = args.yolo {
        y
    } else {
        config.security_mode.as_deref() == Some("yolo")
    };
    let tool_call_limit = args
        .tool_call_limit
        .or(config.tool_call_limit)
        .unwrap_or(50);
    let thinking_level = config.default_thinking_level.clone();
    let settings_path = picocode::platform::resolve_config_path(args.config.as_deref());

    let persona_name = args
        .persona
        .or_else(|| recipe.as_ref().and_then(|r| r.persona.clone()))
        .or(config.default_persona.clone());

    let quiet = args.quiet || recipe.as_ref().map(|r| r.quiet).unwrap_or(false);
    let show_reasoning = config
        .show_reasoning
        .as_deref()
        .and_then(picocode::settings::ShowReasoning::parse_opt)
        .unwrap_or_default();
    let show_reasoning_handle: picocode::settings::ShowReasoningHandle =
        std::sync::Arc::new(std::sync::Mutex::new(show_reasoning));
    let reasoning_in_context = config.reasoning_in_context.unwrap_or_default();
    let output: Arc<dyn picocode::Output> = if quiet {
        Arc::new(picocode::QuietOutput::new())
    } else {
        Arc::new(ConsoleOutput::new(show_reasoning_handle.clone()))
    };

    let system_message_extension = picocode::agent::load_agents_md();
    let persona_prompt = persona_name
        .as_ref()
        .and_then(|p| picocode::persona::get_persona(p));

    // Resolve the sandbox config: merge user settings with built-in defaults.
    // The sandbox is always active (there is no enable switch), so we must
    // always produce a config — even when the user supplied no
    // `[tool_config.bash.sandbox]` block. If we only mapped over
    // `bash_sandbox_raw()` (which returns `None` when that block is absent), a
    // missing block would leave commands with no sandbox config at all.
    // Starting from a default config guarantees `with_defaults` is reached and
    // the built-in defaults are applied. `bwrap` is required at startup (see
    // `check_runtime_deps`), so the namespace is always available.
    let sandbox = Some(
        config
            .bash_sandbox_raw()
            .cloned()
            .unwrap_or_default()
            .with_defaults(),
    );

    let agent_config = AgentConfig {
        provider: provider.clone(),
        model,
        output,
        yolo,
        quiet,
        tool_call_limit,
        system_message_extension,
        persona_prompt,
        persona_name,
        bash_auto_allow: Some(config.get_bash_auto_allow()),
        agent_prompt: picocode::config::read_prompt(
            config.agent_prompt.clone(),
            config.agent_prompt_file.clone(),
        )?,
        thinking_level,
        user_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        sandbox,
        show_reasoning_handle,
        reasoning_in_context,
        message_log_path: args.log_messages.clone(),
        sessions_dir: None,
    };
    let settings = SettingsStore::from_agent_config(&agent_config, settings_path.as_deref());
    match command {
        Commands::Recipe { name: _ } => {
            let agent = create_agent(agent_config.clone()).await?;
            if let Some(r) = recipe {
                let prompt =
                    picocode::config::read_prompt(r.prompt.clone(), r.prompt_file.clone())?
                        .ok_or("Recipe must have either 'prompt' or 'prompt_file'")?;
                let response = agent.run_once(prompt).await?;
                if r.is_error(&response)? {
                    return Err(Box::new(picocode::PicocodeError::Other(
                        "Response matched error_if pattern".to_string(),
                    )));
                }
                if args.quiet || r.quiet {
                    println!("{}", response);
                }
            } else {
                eprintln!("Error: Recipe not found");
                std::process::exit(1);
            }
        }
        Commands::Input { prompt } => {
            let agent = create_agent(agent_config.clone()).await?;
            let response = agent.run_once(prompt).await?;
            if args.quiet {
                println!("{}", response);
            }
        }
        Commands::Chat => {
            if let Some(p) = prompt {
                let agent = create_agent(agent_config.clone()).await?;
                let response = agent.run_once(p).await?;
                if args.quiet {
                    println!("{}", response);
                }
            } else {
                picocode::agent::run_interactive(agent_config, settings, args.resume).await?;
            }
        }
    }

    Ok(())
}

fn default_model(provider: &str) -> String {
    match provider {
        "anthropic" => "claude-3-5-sonnet-20241022".to_string(),
        "openai" => "gpt-4o-mini".to_string(),
        "azure" => "gpt-4o".to_string(),
        "cohere" => "command-r-plus".to_string(),
        "chatgpt" => "gpt-5".to_string(),
        "copilot" => "gpt-5-codex".to_string(),
        "deepseek" => "deepseek-chat".to_string(),
        "doubleword" => "Qwen/Qwen3.5-4B".to_string(),
        "groq" => "llama3-70b-8192".to_string(),
        "huggingface" => "meta-llama/Llama-3-70b-chat-hf".to_string(),
        "hyperbolic" => "meta-llama/Llama-3-70b-instruct".to_string(),
        "llamafile" => "LLaMA_CPP".to_string(),
        "mira" => "mira-v1".to_string(),
        "minimax" => "MiniMax-M2.7".to_string(),
        "mistral" => "mistral-large-latest".to_string(),
        "moonshot" => "moonshot-v1-8k".to_string(),
        "ollama" => "llama3".to_string(),
        "openrouter" => "meta-llama/llama-3-70b-instruct".to_string(),
        "perplexity" => "llama-3-sonar-large-32k-online".to_string(),
        "together" => "meta-llama/Llama-3-70b-chat-hf".to_string(),
        "xai" => "grok-1".to_string(),
        "xiaomimimo" => "mimo-v2.5-pro".to_string(),
        "zai" => "glm-4.6".to_string(),
        "gemini" | "google" => "gemini-1.5-pro".to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_executable_false_for_missing_binary() {
        assert!(!is_executable("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn check_runtime_deps_ok_when_both_present() {
        // Both binaries are now hard requirements, so this holds in any
        // environment where picocode is expected to run.
        assert!(check_runtime_deps().is_ok());
    }
}
