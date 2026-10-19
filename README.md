<div align="center">
  <img src="prior.gif" width="600"/>
  <br/>
  <br/>
  <h1>⚡️ prior</h1>
  <p><b>The tiny coding agent that speaks every LLM.</b></p>
  <br/>
</div>

**prior** is a minimal, high-performance Rust-based coding agent. It's a small, robust tool designed for developers who want a lightweight assistant that is easy to reason about, safe to use, and highly hackable.

## 🚀 Quick Start

Get up and running in seconds:

```bash
# 1. Install
curl -sSfL https://raw.githubusercontent.com/jondot/prior/main/install.sh | sh

# 2. Set your API Key (pick your provider)
export ANTHROPIC_API_KEY=your_key_here # For Claude
# or
export OPENAI_API_KEY=your_key_here    # For GPT-4o
# or
export GOOGLE_API_KEY=your_key_here    # For Gemini
# or
export DEEPSEEK_API_KEY=your_key_here  # For DeepSeek

# 3. Start coding
prior "Analyze this project and suggest improvements"
```

Or, download directly from [releases](https://github.com/jondot/prior/releases)

---

## 🦀 Why prior?

- **Tiny & Fast**: A single, compact binary written in Rust. No heavy dependencies, no bloat.
- **Multi-LLM Sovereignty**: Works with Anthropic, OpenAI, DeepSeek, Google (Gemini), Ollama, and many more via [Rig](https://github.com/0xPlayground/rig).
- **Interactive & Scriptable**: Use it as an interactive CLI, pipe it into scripts, or run automated **Recipes**.
- **Persona-driven**: Switch between different expert personalities (Architect, Security, Zen Master, etc.) to change how the agent thinks and speaks.
- **Safety First**: Destructive actions (like deleting files or running shell commands) require manual confirmation by default.
- **Extensible**: Use it as a CLI tool or integrate it as a Rust library in your own projects.

## 🎭 The Persona Gallery

Prior isn't just a tool; it has character. Use `--persona` to change the agent's expertise and "vibe":

| Persona      | Description                           | "Voice"                                                        |
| :----------- | :------------------------------------ | :------------------------------------------------------------- |
| `architect`  | High-level software architect.        | _"This abstraction needs more Cowbell. Let's refactor."_       |
| `strict`     | Swiss-clock precision engineer.       | _"Zero tolerance for fluff. Applying optimal logic."_          |
| `security`   | Bruce Schneier fan, paranoid analyst. | _"Searching for vulnerabilities... trust nothing."_            |
| `zen`        | Minimalist, focused on simplicity.    | _"Code is a form of meditation. Let's find the path."_         |
| `hacker`     | Chaotic good, assembly dreamer.       | _"I found a 2ms optimization. Applying now."_                  |
| `guru`       | Visionary Silicon Valley disruptor.   | _"Let's move the needle and scale this to infinity."_          |
| `sysadmin`   | Grumpy, old-school server legend.     | _"Back in my day, we didn't need these fancy LLMs..."_         |
| `academic`   | Formal professor, theory first.       | _"As per the 1974 paper by Knuth, this is suboptimal."_        |
| `hustler`    | MVP-focused startup survivor.         | _"Ship it! We'll fix the debt after the Series A."_            |
| `craftsman`  | Accessibility & semantic HTML purist. | _"Semantic HTML is the foundation of a healthy web."_          |
| `sre`        | Reliability and observability ninja.  | _"But how will we monitor this in production?"_                |
| `maintainer` | Patient, docs-loving OSS saint.       | _"Could you add a test case and update the README?"_           |
| `tester`     | Destructive edge-case finder.         | _"I'm going to try passing a null to this and watch it burn."_ |

> [!TIP]
> You can even add a local `AGENTS.md` file to give the agent custom codebase-specific instructions!

## ⚙️ Recipes & Automation

Prior supports named **Recipes** in a `settings.toml` config file for non-interactive execution (CI/CD, automation).

```toml
# settings.toml
[recipes.review-security]
prompt = "Review the codebase for security issues."
persona = "security"
model = "claude-3-5-sonnet-latest"

[recipes.review-from-file]
prompt_file = "prompts/security_review.txt"
persona = "security"
```

Run it with:

```bash
prior recipe review-security
```

## ⚙️ CLI Modes & Flags

Prior is designed to be flexible, whether you're using it for a quick question or a complex automation task.

### Main Commands

- **Interactive Chat**: `prior` or `prior chat` (Default)
- **Single Prompt**: `prior "your prompt"` or `prior input "your prompt"`
- **Recipes**: `prior recipe <name>` (Runs a pre-defined task from the config file)

### Common Flags

- `-p, --provider <PROVIDER>`: Override the default LLM provider (e.g., `openai`, `anthropic`, `ollama`).
- `-m, --model <MODEL>`: Specify a specific model (e.g., `claude-3-5-sonnet-latest`, `gpt-4o`).
- `--yolo`: Disable all confirmation prompts. **Use with caution.**
- `-q, --quiet`: Minimal output, useful for piping into other tools.
- `--persona <NAME>`: Launch with a specific expert persona.
- `--tool-call-limit <N>`: Maximum number of tool calls allowed per turn (Default: 50).
- `-r, --resume`: Auto-resume the most recent session for the current directory at startup — equivalent to opening the interactive chat, typing `/session`, pressing Tab to pick the top entry, and Enter to confirm `resume`. If no session exists yet, a new one is started with a note. Implies the interactive chat (cannot be combined with `input`/`recipe`/positional prompt).

## 🛠 Available Tools

Prior gives the AI a comprehensive set of tools to interact with your environment:

- **Filesystem**: `read_file`, `write_file`, `edit_file` (atomic search-replace), `list_dir`, `make_dir`, `remove`, `move_file`, `copy_file`.
- **Search**: `grep_text` (regex search), `glob_files` (find files by pattern).
- **System**: `bash` (run any shell command).
- **Web**: `agent_browser` (full browser automation via [agent-browser](https://github.com/jondot/agent-browser) if installed).

## 🔒 Sandboxing (`bash` & file tools)

Shell commands run inside a [Bubblewrap](https://github.com/containers/bwrap) (bwrap) namespace when `bwrap` is installed: the process gets its own mount, PID, and network namespaces, all capabilities dropped, and an optional virtual-memory cap. Commands are also killed if they exceed `timeout_secs`.

Configure it under `tool_config.bash.sandbox` in `settings.toml` (see `settings.toml.example`):

```toml
[tool_config.bash.sandbox]
# The sandbox is always active when `bwrap` is present (required at startup),
# so there is no `enabled` switch to toggle it off.
timeout_secs = 120
network = false         # unshare network by default
memory_mb = 1024        # optional RLIMIT_AS cap
# Deny-by-default: only the listed paths are visible. Split into read-only
# (`ro`) and read-write (`rw`) lists; each entry is a bare path or a remap.
rw = [
  ".",
  { from = "./cache", to = "/var/cache/app" },  # remap
]
ro = []
```

- **Deny-by-default filesystem**: when either `rw`/`ro` list is set, only the listed bindings are visible (plus read-only `/usr`, `/bin`, `/lib`, `/lib64`, `/etc` so tools can execute). The non-shell file tools (`read_file`, `write_file`, `edit_file`, `glob_files`, `grep_text`, `list_dir`, `make_dir`, `remove`, `move_file`, `copy_file`) enforce the **same** policy, so a remap like `/tmp` is seen identically by the shell and by the file tools. Writes require an `rw` binding; paths outside all bindings are denied.
- `bwrap` is required at startup; if it is missing the process fails closed. There is no unsandboxed fallback — a command is never run outside the sandbox, and a disabled/unset sandbox config causes the command to be refused rather than executed by a plain shell.
- This is defense-in-depth for an LLM-driven shell, not a jail against root — run prior as an unprivileged user.

See `doc/sandbox.md` for the full design.

## 🛠 Hacking on prior

Prior is built with Rust and the [Rig](https://github.com/0xPlayground/rig) library. It's designed to be extremely easy to extend.

### Prerequisites

- [Rust](https://rustup.rs/) (latest stable)
- API Keys for your preferred provider (e.g., `ANTHROPIC_API_KEY`)

### Local Setup

```bash
# 1. Clone the repo
git clone https://github.com/jondot/prior.git
cd prior

# 2. Build and run
cargo run -- "Analyze src/main.rs"
```

### Adding a New Tool

1. Open `src/tools.rs`.
2. Use the `#[rig_tool]` macro to define your function.
3. Register the tool in `src/agent.rs` within the `build_rig_agent` function.

### Project Structure

- `src/main.rs`: CLI entry point and argument parsing.
- `src/agent.rs`: Agent creation and system prompt logic.
- `src/tools.rs`: Implementation of all AI-accessible tools.
- `src/output.rs`: Terminal UI and progress indicators.

## 📚 Use as a Library

Prior is structured as a library (`lib.rs`) and a binary (`main.rs`).

Example usage:

```rust
use prior::{create_agent, AgentConfig, ConsoleOutput};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = Arc::new(ConsoleOutput::new(prior::settings::default_show_reasoning_handle()));

    let agent = create_agent(AgentConfig {
        provider: "anthropic".into(),
        model: "claude-3-5-sonnet-latest".into(),
        output,
        yolo: false,
        tool_call_limit: 50,
        system_message_extension: None,
        persona_prompt: None,
        persona_name: None,
        bash_auto_allow: None,
        agent_prompt: None,
        user_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        sandbox: None,
        show_reasoning_handle: prior::settings::default_show_reasoning_handle(),
    }).await?;

    let response = agent.run_once("Analyze the current project".into()).await?;
    println!("Response: {}", response);
    Ok(())
}
```

---

Built for speed, safety, and simplicity. MIT Licensed.
