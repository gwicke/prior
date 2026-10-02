use picocode::{create_agent, AgentConfig, NoOutput};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Setup the output handler with NoOutput for silent execution
    let output = Arc::new(NoOutput);

    // 2. Create the agent
    let agent = create_agent(AgentConfig {
        provider: "anthropic".into(),
        model: "claude-3-5-sonnet-latest".into(),
        output,
        yolo: true, // Auto-confirm everything since there's no output
        context_limit: None,
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
        show_reasoning_handle: picocode::settings::default_show_reasoning_handle(),
        reasoning_in_context: picocode::settings::ReasoningInContextMode::Never,
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
