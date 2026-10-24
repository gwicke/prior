use prior::{create_agent, AgentConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Setup a quiet content for silent execution: nothing reaches the
    //    screen, and errors still go to stderr.
    let content = Arc::new(prior::Content::quiet());

    // 2. Create the agent
    let agent = create_agent(AgentConfig {
        provider: "anthropic".into(),
        model: "claude-3-5-sonnet-latest".into(),
        content,
        yolo: true,         // Auto-confirm everything since there's no output
        confirm_port: None, // No REPL to ask, so nothing drives a confirm dialog
        context_limit: None,
        routing: None,
        alias_for: None,
        tool_call_limit: 5,
        quiet: false,
        params: Default::default(),
        system_message_extension: None,
        persona_prompt: None,
        persona_name: None,
        bash_auto_allow: None,
        sandbox: None,
        agent_prompt: None,
        user_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cancel_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        show_reasoning_handle: prior::settings::default_show_reasoning_handle(),
        reasoning_in_context: prior::settings::ReasoningInContextMode::Never,
        message_log_path: None,
        sessions_dir: None,
    })
    .await?;

    println!("Running agent in silent mode...");

    // 3. Run a task and get the response!
    let response = agent
        .run_once("What is 2+2? Return only the number.".into())
        .await?;

    println!("Agent response: {}", response);

    Ok(())
}
