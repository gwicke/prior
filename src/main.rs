use clap::{Parser, Subcommand};
use picocode::{config::Config, create_agent, AgentConfig, ConsoleOutput};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(author, version, about = "Minimal coding assistant")]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Positional prompt (shortcut for 'input')
    #[arg(index = 1)]
    prompt: Option<String>,

    /// LLM provider (anthropic, openai, azure, cohere, deepseek, galadriel, gemini, groq, huggingface, hyperbolic, mira, mistral, moonshot, ollama, openrouter, perplexity, together, xai)
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
    #[arg(long, default_value = "50", global = true)]
    tool_call_limit: usize,

    /// Choose a persona for the agent
    #[arg(long, help = format!("Choose a persona for the agent. Available built-in personas:\n{}", picocode::persona::list_personas()), global = true)]
    persona: Option<String>,

    /// Path to config file (default: picocode.yaml or picocode.yml in current directory)
    #[arg(short, long, global = true)]
    config: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start an interactive chat session (default)
    Chat,
    /// Run a single prompt
    Input { prompt: String },
    /// Run a pre-defined recipe from picocode.yaml
    Recipe { name: String },
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

        eprintln!("\n\n--------------------------------------------------------------------------------");
        eprintln!("💥 Picocode encountered an unexpected error (panic).");
        eprintln!("Message: {}", message);
        if let Some(location) = info.location() {
            eprintln!("Location: {}:{}:{}", location.file(), location.line(), location.column());
        }
        eprintln!("--------------------------------------------------------------------------------");
        eprintln!("This is likely a bug in picocode or one of its dependencies.");
        eprintln!("Please report it at: https://github.com/jondot/picocode/issues");
        eprintln!("--------------------------------------------------------------------------------\n");
    }));

    if let Err(e) = run().await {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let config = Config::load(args.config.as_deref())?;

    let (command, prompt, recipe_name) = match (&args.command, &args.prompt) {
        (Some(Commands::Recipe { name }), _) => (
            Commands::Recipe { name: name.clone() },
            None,
            Some(name.clone()),
        ),
        (Some(Commands::Input { prompt }), _) => (
            Commands::Input { prompt: prompt.clone() },
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

    let provider = args
        .provider
        .or_else(|| recipe.as_ref().and_then(|r| r.provider.clone()))
        .unwrap_or_else(|| "anthropic".to_string());

    let model = args
        .model
        .or_else(|| recipe.as_ref().and_then(|r| r.model.clone()))
        .unwrap_or_else(|| default_model(&provider));

    let yolo = args
        .yolo
        .or_else(|| recipe.as_ref().and_then(|r| r.yolo))
        .unwrap_or(false);

    let persona_name = args
        .persona
        .or_else(|| recipe.as_ref().and_then(|r| r.persona.clone()));

    let quiet = args.quiet || recipe.as_ref().map(|r| r.quiet).unwrap_or(false);
    let output: Arc<dyn picocode::Output> = if quiet {
        Arc::new(picocode::QuietOutput::new())
    } else {
        Arc::new(ConsoleOutput::new())
    };

    let system_message_extension = picocode::agent::load_agents_md();
    let persona_prompt = persona_name
        .as_ref()
        .and_then(|p| picocode::persona::get_persona(p));

    let agent_config = AgentConfig {
        provider: provider.clone(),
        model,
        output,
        yolo,
        quiet,
        tool_call_limit: args.tool_call_limit,
        system_message_extension,
        persona_prompt,
        persona_name,
        bash_auto_allow: Some(config.get_bash_auto_allow()),
        agent_prompt: picocode::config::read_prompt(
            config.agent_prompt.clone(),
            config.agent_prompt_file.clone(),
        )?,
        thinking_level: None,
    };
    let agent = create_agent(agent_config.clone()).await?;

    match command {
        Commands::Recipe { name: _ } => {
            if let Some(r) = recipe {
                let prompt = picocode::config::read_prompt(r.prompt.clone(), r.prompt_file.clone())?
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
            let response = agent.run_once(prompt).await?;
            if args.quiet {
                println!("{}", response);
            }
        }
        Commands::Chat => {
            if let Some(p) = prompt {
                let response = agent.run_once(p).await?;
                if args.quiet {
                    println!("{}", response);
                }
            } else {
                picocode::agent::run_interactive(agent_config).await?;
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
        "deepseek" => "deepseek-chat".to_string(),
        "galadriel" => "llama3-70b".to_string(),
        "groq" => "llama3-70b-8192".to_string(),
        "huggingface" => "meta-llama/Llama-3-70b-chat-hf".to_string(),
        "hyperbolic" => "meta-llama/Llama-3-70b-instruct".to_string(),
        "mira" => "mira-v1".to_string(),
        "mistral" => "mistral-large-latest".to_string(),
        "moonshot" => "moonshot-v1-8k".to_string(),
        "ollama" => "llama3".to_string(),
        "openrouter" => "meta-llama/llama-3-70b-instruct".to_string(),
        "perplexity" => "llama-3-sonar-large-32k-online".to_string(),
        "together" => "meta-llama/Llama-3-70b-chat-hf".to_string(),
        "xai" => "grok-1".to_string(),
        "gemini" | "google" => "gemini-1.5-pro".to_string(),
        _ => "unknown".to_string(),
    }
}
