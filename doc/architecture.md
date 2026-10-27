# Prior — Architecture Summary

## 1. Overview

**Prior** is a small, self-contained coding agent written in Rust. It provides a TUI-based
interactive session with an LLM, offering file editing, shell execution, tree navigation, and
session management. It is Pi-compatible (shares session storage format with [picocode] and
[pi agent]) and built on the [rig] framework for LLM integration.

[picocode]: https://github.com/jondot/picocode
[pi agent]: https://github.com/jondot/pi-agent
[rig]: https://github.com/06chaynes/rig

**Package name**: `prior-agent` (crate name `prior`). Binary: `prior`.

## 2. Workspace Structure

```text
prior-agent          # Main crate (package name): TUI session, agent loop, tools, config
crates/
  slashdot-menu      # Fuzzy-filtered slash-command menu engine (table + fuzzy filter; see doc/slash_menu.md)
  slashdot-tui       # Terminal drawing / rendering crate (OS-independent)
  bsed-blockedit     # Edit-block parsing, matching, and application (shared core)
  bsed               # Standalone sed-preview binary (also library; separate package to avoid output
                     # filename collision; see ./bsed.md)
```

`bsed-blockedit` is the shared core: both the `edit` tool (used by the agent) and the `bsed` binary
link against it so they produce and consume identical block syntax.

## 3. Entry Points

Three modes, selected via CLI arguments or subcommands:

|Mode            |Entry                            |Behavior                                  |
|----------------|---------------------------------|------------------------------------------|
|Interactive chat|`prior` or `prior chat`          |Full TUI session, REPL loop               |
|One-shot        |`prior "prompt"` or `prior input`|Single prompt → completion → exit (no TUI)|
|Recipe          |`prior recipe <name>`            |Config-defined recipe → single turn → exit|

`main.rs` parses arguments, validates runtime deps (`bwrap`, `rg`), loads config + settings, then
delegates to `interactive::run_interactive()` for chat mode, or executes the one-shot/recipe paths
directly. The `prior-egress` binary (installed alongside `prior`) is the bwrap payload for network
egress. Test-only: `src/bin/confirm_probe.rs` (gated by `test-utils` feature).

## 4. Core Layers

### 4.1 Agent (`src/agent/`)

The LLM interaction engine. Key modules:

- **build.rs** — `create_agent()` constructs a `rig::Agent` from config: resolves model catalog,
  bakes provider reasoning params, wires the completion client (OpenRouter with prompt caching, or
  direct provider routing), registers tools with approval guards, and configures logging hooks.
- **turn.rs** — Single-turn execution: assembles messages, invokes the LLM, processes the response,
  dispatches tool calls, collects results, and loops back to the LLM.
- **compaction.rs** — Summarizes session history to bound context window. Produces summary text that
  is prepended to subsequent turns.
- **pumps.rs** — Back-off timers and cancellable sleep for turn retries and tool timeouts.
- **errors.rs** — Retry budget and delay computation.
- **replay.rs** — Replay existing turns (e.g., after `/replay`).
- **msglog.rs** — Message logging to JSONL for debugging.
- **transcript.rs** — Assistant message assembly, reasoning stripping.
- **status.rs** — Context limit resolution, usage reporting.

The `CodeAgent` type wraps a `rig::Agent` with Prior-specific hooks (token usage tracking,
compaction injection, turn persistence). Note: `src/agent.rs` (at crate root) re-exports and
consumes the `agent/` module.

### 4.2 Agent Root (`src/agent.rs`)

Re-exports `agent/` module, defines `CodeAgent` wrapper around `rig::Agent` with Prior-specific
hooks (token usage tracking, compaction injection, turn persistence), and contains `AgentConfig`
(the applied, frozen config for an agent instance).

### 4.3 Interactive Loop (`src/interactive.rs`)

The REPL's main loop. It drives the agent:

1. Reads user input (plain text or slash commands).
2. Converges the target config (`State.config`) against the applied `AgentConfig` — rebuilds the
   agent when the target changes (model, persona, sandbox settings).
3. Runs a turn (single prompt → LLM call → tools → response).
4. Renders output via the drawing stack.
5. Loops back.

The loop handles `/clear` (session reset), `/session` (list/switch/resume/delete), `/model` (model
selection), `/set` (settings), `/persona` (persona switching), `/tree` (session tree), `/stat`
(usage stats), and `/quit`.

### 4.4 Session (`src/session/`)

Persistent session storage, compatible with the Pi JSONL format.

- **repo.rs** — `SessionRepo` trait; `JsonlSessionRepo` implements CRUD on disk.
- **session.rs** — Session type with compaction helpers, touched-files tracking, branch
  summarization, and Pi-message conversion (`pi_message_to_rig`).
- **storage.rs** — `SessionStorage` trait; `JsonlSessionStorage` handles directory-level ops.
- **types.rs** — `SessionTreeNode`, `SessionHeader`, `SessionEntry`, `SessionMetadata`, and CWD
  encoding.

Sessions are stored in `~/.config/prior/sessions/` and organized as a tree by CWD. Also includes
`mod.rs` (re-exports) and `pi_compat_test.rs` (Pi format compatibility tests).

### 4.5 Tools (`src/tools.rs`, `src/confirm.rs`)

Six tools exposed to the LLM (`read`, `write`, `edit`, `grep`, `sed`, `bash`), each a `pub async fn`
registered with rig's tool framework:

| Category   | Tools                                                                |
| ---------- | -------------------------------------------------------------------- |
| Point I/O  | `read`, `write`, `edit` — single-path, resolved through `PathPolicy` |
| Search     | `grep` — ripgrep, run inside the namespace (`run_sandboxed_ro`)      |
| Batch edit | `sed` — GNU sed expressions through the `bsed` binary, read-only     |
| Execution  | `bash` — sandboxed shell execution, the only child-process tool      |

Implementation support: `src/bash_cmd.rs` (command construction and sandbox integration),
`src/turn_overrides.rs` (turn-level override handling for tool dispatch). `sed` is advertised only
when `bsed` is on `PATH`. Every tool is a *point* operation: tree traversal (`ls`, `find`, `mkdir`,
`mv`, `cp`, `rm`) and browser automation go through `bash`, where the namespace enforces the
bindings with no second implementation to drift (doc/sandbox.md → "Tree traversal belongs to the
shell"). `read` does its IO in-process; `grep`/`sed` authorize the host path and then hand the child
the *sandbox* path, so they share `bash`'s view of the filesystem.

### 4.6 Sandbox (`src/tools.rs`, `src/sandbox_state.rs`, `src/sandbox_tmp.rs`, `src/egress.rs`, `src/seccomp.rs`, `src/path_util.rs`, `src/platform.rs`)

Sandboxed tool execution using bubblewrap (`bwrap`):

- **Namespace isolation**: read-only cwd, private `/tmp` (per-session tmpfs), isolated network
  namespace (`--unshare-net`), dropped capabilities (`--cap-drop ALL`), `--die-with-parent`.
- **Egress control** (`egress.rs`): when `network: true`, a bridge process (`prior-egress`) listens
  inside the sandbox's netns, serves SOCKS5 and HTTP proxy on one port (first-byte sniffing), and
  enforces an allowlist of destinations. Direct TCP is blocked regardless.
- **Seccomp** (`seccomp.rs`): 12-instruction BPF program loaded via memfd, denies `AF_VSOCK` and
  `io_uring` syscalls.
- **Memory cap**: configurable max bytes, enforced by bwrap.
- **Timeout**: configurable per-tool, enforced by a watchdog thread.
- **Cleanup**: `/tmp` swept on `/clear`, session delete, and 7-day mtime sweep on startup.

## 5. TUI & Rendering

### 5.1 slashdot-tui (`crates/slashdot-tui/`)

Self-contained drawing crate. Five modules:

- **render.rs** — `RenderManager` hub; composes `Element`s into a `Frame`; manages repaint
  scheduling and row allocation.
- **draw.rs** — Screen management: terminal size changes, screen push/pop, cursor management.
- **termio.rs** — TTY lifecycle: raw-mode setup/restore (safe from panic/signal), one reader thread,
  DSR detection, size watch.
- **ansi.rs** — ANSI escape handling, byte-to-width transforms, write wrappers.
- **wrap.rs** — Line wrapping and reflow over `&str` (CJK-aware).

### 5.2 Content (`src/content/`)

The `Content` type abstracts the rendering pipeline: it owns a vector of `Element`s (text, markdown,
tables, status bar) and exposes `render()` / `clear_and_render()` to push to the screen. Streaming
content (LLM output, thinking) is accumulated via `push()` and flushed to the render manager.

Key modules: `handle.rs` (core rendering logic), `markdown.rs` (markdown rendering), `text.rs` (text
elements), `preview.rs` (sed preview rendering), `panel.rs` (status panel), `composition.rs`
(element composition).

### 5.3 v25 Menu System (`src/v25_*.rs`)

The slash-command menu layer. Each command has its own module:

| Module             | Command      | Purpose                           |
| ------------------ | ------------ | --------------------------------- |
| `v25_menu`         | `/`          | Root slash menu                   |
| `v25_tree`         | `/tree`      | Session tree navigation           |
| `v25_session`      | `/session`   | Session list/switch/resume/delete |
| `v25_model`        | `/model`     | Model selection                   |
| `v25_set`          | `/set`       | Settings viewer/edits             |
| `v25_stat`         | `/stat`      | Token usage stats                 |
| `v25_sandbox`      | `/sandbox`   | Sandbox configuration             |
| `v25_persona`      | `/persona`   | Persona selection                 |
| `v25_clear`        | `/clear`     | Clear session                     |
| `v25_quit`         | `/quit`      | Exit                              |
| `v25_reasoning`    | `/reasoning` | Toggle reasoning display          |
| `v25_replay`       | `/replay`    | Replay turns                      |
| `v25_commands`     | —            | Built-in command definitions      |
| `v25_listing_node` | —            | Generic table listing node        |
| `v25_new`          | `/new`       | New session creation              |

The menu system uses `slashdot-menu`'s table + fuzzy filter engine. Nodes define columns, sorting,
and filtering; the engine handles fuzzy matching, result ordering, and keyboard navigation.

`src/ui/` contains `bar.rs` (status bar rendering) and `mod.rs`. Completion support:
`src/path_complete.rs` (path completion), `src/prompt_complete.rs` (prompt/slash completion).

### 5.4 REPL Input (`src/repl.rs`)

Raw-mode line editor with multi-line support. Maintains a flat `Vec<char>` buffer with cursor.
Rendering wraps the buffer to visual rows via `layout` (word-boundary wrap, CJK char-wrap).
Integrates the v2.5 slash-menu frame for inline command completion.

## 6. Configuration

- **`Config`** (`src/config.rs`) — TOML file (~/.config/prior/settings.toml) with model defaults,
  persona definitions, recipe definitions, tool settings (limits, timeouts), and sandbox config.
  Includes `context_lines_tests` and extensive config tests.
- **`SettingsStore`** (`src/settings.rs`) — Runtime settings store. Live-watchable via `notify` —
  external edits to `settings.toml` are picked up without restart. Includes settings tests.
- **`SettingsWatch`** (`src/settings_watch.rs`) — File watcher using `notify` for live config
  reload.
- **`AppState`** (`src/app_state.rs`) — Single source of truth for menu-visible state. A
  `parking_lot::Mutex<State>` behind `Arc`, with strict lock rules (encapsulated handle, one lock
  per method, never hold across `.await`). Maintains target config vs. applied config separation.
- **CLI pins** — Flags like `--model`, `--yolo`, `--persona` override the settings file for the
  session lifetime.

## 7. Model Integration

- **Model Catalog** (`src/model_catalog.rs`) — Resolves available providers from the system, checks
  for required env vars, and filters models based on provider availability.
- **Provider support** — 22+ providers via rig-core: anthropic, openai, azure, cohere, chatgpt,
  copilot, deepseek, doubleword, gemini, groq, huggingface, hyperbolic, llamafile, mira, minimax,
  mistral, moonshot, ollama, openrouter, perplexity, together, xai, xiaomimimo, zai.
- **OpenRouter caching** — Default client routes through OpenRouter with prompt caching enabled.
- **Direct provider routing** — When model settings indicate it, bypasses OpenRouter for
  provider-specific features.
- **Model Settings** (`src/model_settings.rs`) — Per-model configuration (temperature, reasoning,
  etc.) with validation.
- **Provider Params** (`src/params.rs`) — Normalized parameter schema across 22+ providers.

## 8. Edit System

### 8.1 Edit Blocks (`src/edit_blocks.rs`, `crates/bsed-blockedit/`)

Block-based multi-file batch editing (shared with `bsed-blockedit` and `bsed` crates):

- **Parse**: `bsed-blockedit` parses the `@@@OLD`/`@@@NEW`/`@@@END` format.
- **Match**: Fuzzy line matching against file content, with line-anchored resolution for
  reliability.
- **Apply**: Atomic all-or-nothing application, overlap-checked, uniqueness-checked.

### 8.2 bsed Preview (`crates/bsed/`, `src/tools.rs::sed_preview`)

`bsed` is a sed-to-block compiler:

1. The agent writes GNU sed expressions.
2. `bsed` runs in a read-only bwrap sandbox (no file writes possible).
3. Outputs `@@@OLD`/`@@@NEW`/`@@@END` blocks.
4. The agent feeds blocks to the `edit` tool for application.

Three independent safety layers: AST deny-list (rejects `w`/`r`/`e`/`q`), process boundary (separate
binary), read-only bwrap namespace.

## 9. Data Flow

```text
CLI args
    │
    ▼
main.rs ──► Config::load() ──► SettingsStore::watch()
    │              │                    │
    │              ▼                    │
    │         AppState::new() ◄──────────┘
    │              │
    │              ▼
    │         interactive::run_interactive()
    │              │
    │         ┌────┴─────────────────────────┐
    │         │  Turn Loop (per prompt)       │
    │         │  1. Read input (repl.rs)      │
    │         │  2. Build/refresh agent       │
    │         │     (agent::build.rs)         │
    │         │     Creates CodeAgent (agent.rs)
    │         │  3. Execute turn (agent/turn) │
    │         │     - LLM call (rig)          │
    │         │     - Tool dispatch (tools.rs)│
    │         │     - Response rendering      │
    │         │  4. Render output (content)   │
    │         └──────────────────────────────┘
    │              │
    │              ▼
    │         Slash commands (v25_*)
    │         - /tree, /session, /model, /set, /stat
    │         - /sandbox, /persona, /clear, /quit
    │         - /new, /reasoning, /replay
    │              │
    │              ▼
    │         Session persistence (session/repo)
    │              │
    │         Drawing stack (slashdot-tui)
    │         - render::Element → Frame → terminal
    │              │
    ▼              ▼
    Output (markdown, tables, status bar)
```

## 10. Key Design Decisions

- **Token efficiency**: compact system prompts, minimal tool schemas, truncation for long outputs.
- **Pi compatibility**: JSONL session format, compatible tool names, shared config conventions.
- **Sandboxed execution**: all tool runs in bwrap namespaces; network egress via controlled bridge.
- **Event-driven config**: `notify` watches settings file for live updates during sessions.
- **One-way dependencies**: `interactive` → `agent`, never the reverse. Layering visible in imports.
- **State encapsulation**: `AppState::Mutex<State>` with enforced lock discipline; target vs.
  applied config separation.
- **Multi-binary workspace**: `bsed` is its own package (avoids output filename collision); prior
  shells out to it rather than wrapping.

## 11. Runtime Dependencies

External binaries required at startup:

- **bwrap** (bubblewrap) — sandbox namespace creation
- **rg** (ripgrep) — grep tool implementation

## 12. Related Documentation

- `doc/sandbox.md` — Full sandbox design and audit
- `doc/session_storage.md` — Session storage format
- `doc/streaming_mode.md` — Streaming output rendering
- `doc/slash_menu.md` — Slash menu system design
- `doc/bsed.md` — bsed tool design and implementation
- `doc/compaction.md` — Session compaction strategy
- `doc/tree.md` — Session tree navigation
- `doc/settings_reload.md` — Live settings reload
- `doc/slashdot/state_and_async.md` — AppState design (referenced in code)
- `doc/261003_output_drawing_refactor.md` — Drawing stack design
- `doc/pi_session_and_tool_compatibility.md` — Pi compatibility notes
- `doc/261021_sandbox_env_x11_audit.md` — Sandbox environment & X11 audit (R3b/R3i)
- `doc/slashdot/extra_columns.md` — Table column modifiers design
- `doc/path_leak_risks.md` — Tool error path leak risks
- `doc/todo.md` — Active backlog
