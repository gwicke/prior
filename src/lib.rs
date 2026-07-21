use thiserror::Error;

pub mod agent;
pub mod output;
pub mod platform;
pub mod tools;
pub mod persona;
pub mod config;
pub mod fuzzy;
pub mod slash;
pub mod repl;
pub mod model_catalog;
pub mod thinking;
pub mod table;
pub mod sandbox_bindings;
pub mod settings;
pub mod path_util;

pub use config::{Config, Recipe, ToolSettings};
pub use settings::{SettingKey, SettingsStore, ShowReasoning, ShowReasoningHandle};

// Re-export core rig types for library users
pub use rig_core::agent::AgentBuilder;
pub use rig_core::client::{CompletionClient, ProviderClient};
pub use rig_core::completion::CompletionModel;
pub use rig_core::providers;

pub use agent::{create_agent, load_agents_md, run_interactive, AgentConfig, CodeAgent, PicoAgent};
pub use output::{Confirmation, ConsoleOutput, LogOutput, NoOutput, Output, QuietOutput};

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

pub type Result<T> = std::result::Result<T, PicocodeError>;
#[cfg(test)] mod test_hang_reader;
