use picocode::{create_agent, AgentConfig, ConsoleOutput};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Setup the output handler (ConsoleOutput provides pretty terminal printing)
    let output = Arc::new(ConsoleOutput::new(
        picocode::settings::default_show_reasoning_handle(),
    ));

    // 2. Create the agent with a simple config struct
    // You no longer need to manually initialize Rig clients or builders.
    let agent = create_agent(AgentConfig {
        provider: "anthropic".into(),
        model: "claude-3-5-sonnet-latest".into(),
        output,
        yolo: false,
        context_limit: None,
        tool_call_limit: 10,
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

    println!("--- Picocode Library Example ---");
    println!("Asking the agent to analyze the current directory...\n");

    // 3. Run a task and get the response!
    let response = agent
        .run_once(
            "List the files in the current directory and explain what this project seems to be."
                .into(),
        )
        .await?;

    println!("\nFinal Response captured in library mode:\n{}", response);

    Ok(())
}
