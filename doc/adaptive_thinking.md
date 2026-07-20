────────────────────────────────────────────────────────────

Core Architecture

The extension is a single-tool, lifecycle-aware plugin that lets the LLM change its own reasoning
effort mid-session. It has three layers:

1. Configuration – JSON file loading with project/global cascade
2. Thinking-level management – validation, persistence, temporary vs. permanent changes
3. Runtime state machine – lifecycle hooks and a tool registration

────────────────────────────────────────────────────────────

1. Configuration Layer

Source: loadConfig (config-loader.ts, lines 4113–4137)

• 1. Loads config from two paths in priority order:
     <cwd>/.pi/adaptive-thinking.json (project)
  2. <homeDir>/.pi/agent/adaptive-thinking.json (global)

• Falls back to built-in defaults if neither exists.
• Schema (validated with TypeBox, analogous to schemars in Rust):

  interface Config {
    enabled: boolean;        // default: true
    quiet: boolean;          // default: false — suppress UI notifications
    toolName: string;        // default: "set_thinking_level"
    toolDescription: string; // default: "Set your thinking level"
    systemPrompt: string;    // guidance text injected into the system prompt
  }

Rig analogue: A #[derive(Deserialize, JsonSchema)] struct, loaded via serde_json::from_reader() with a
two-path search.

────────────────────────────────────────────────────────────

2. Thinking-Level Management

Source: thinking-levels.ts (lines 4140–4153)

• Defines canonical levels: ["off", "minimal", "low", "medium", "high", "xhigh"]
• isThinkingLevel(level: string): boolean — validates a level string
• resolveSupportedThinkingLevels(model) — queries the model's actual capabilities via
  getSupportedThinkingLevels() and intersects with the canonical set

Session-Only vs. Persistent Changes

This is the cleverest part of the design. The pi agent stores a defaultThinkingLevel in its global
settings.json. When the extension makes a temporary change, it:

1. Reads the current defaultThinkingLevel from settings.json
2. Calls pi.setThinkingLevel(level) — effectively changing the model's active level
3. Immediately restores the original defaultThinkingLevel in settings.json so the change is
   session-only (the agent will reset after this turn)

  // lines 4222-4229
  const withSessionOnlyThinkingLevelChange = (changeThinkingLevel) => {
    const settingsPath = globalSettingsPath();
    return withSettingsLock(settingsPath, () => {
      const previousDefaultThinkingLevel = readDefaultThinkingLevel(settingsPath);
      changeThinkingLevel();
      restoreDefaultThinkingLevel(settingsPath, previousDefaultThinkingLevel);
    });
  };

A persistent change writes the new level back as the defaultThinkingLevel and clears
temporaryResetLevel.

Rig analogue: Rig has no built-in "thinking level" concept. In rig, this maps to a provider-specific
additional parameter (e.g., reasoning_effort for OpenAI). The Rust implementation would store a
session-level current_thinking_level in Arc<Mutex<AgentState>> and manipulate the additional_params
field on CompletionRequestBuilder before each call.

────────────────────────────────────────────────────────────

3. Runtime State Machine

Source: adaptiveThinking(pi: ExtensionAPI) (lines 4244–4344)

The extension registers four lifecycle handlers and one tool.

Runtime State

  interface RuntimeState {
    config: Config;
    currentLevel?: string;                         // active thinking level
    persistedLevel?: string;                       // last persistently-set level
    temporaryResetLevel?: string;                  // level to restore after the turn
    lastToolCallWasReasoningTool: boolean;         // back-to-back guard
    reasoningToolCallBackToBackById: Map<string, boolean>; // per-call-id tracking
  }

Lifecycle Hooks

│ Hook                  │ Purpose                                                        │ Rig Equivalent                                                      │
├───────────────────────┼────────────────────────────────────────────────────────────────┼─────────────────────────────────────────────────────────────────────┤
│ thinking_level_select │ User manually changed thinking level in UI — sync currentLevel │ No direct equivalent; would be an external event channel            │
│ tool_call             │ Track whether consecutive calls are both set_thinking_level    │ PromptHook::on_tool_call                                            │
│ before_agent_start    │ Injects adaptive-thinking guidance into system prompt          │ PromptHook::on_completion_call — modify the prompt's system message │
│ agent_end             │ Reset temporary level and clear back-to-back tracking          │ PromptHook::on_completion_response or PromptHook::on_tool_result    │
│ session_start         │ Load config, initialize state, register tool                   │ Agent builder setup                                                 │

The Tool Itself

Registered with dynamic name from config (config.toolName, default "set_thinking_level"):

  const ToolParameters = {
    level: string,   // the target thinking level
    persist: boolean // optional, default false
  };

Tool execution logic:

1. Validate level against supported levels for the current model
2. Reject if currentLevel === level (no-op guard)
3. Reject if two consecutive calls to this tool (back-to-back guard)
4. Save current level as temporaryResetLevel (unless persist)
5. Apply the level via withSessionOnlyThinkingLevelChange()
6. If persist, store as persistedLevel and clear temporaryResetLevel

System Prompt Injection

On every agent turn, the extension appends a guidance block to the system prompt:

  <config.systemPrompt> Current thinking level: <level>. Valid thinking levels for this session: <levels>.
  To change the thinking level, use the `<toolName>` tool with one of the valid levels. ...

Rig analogue: The PromptHook::on_completion_call hook has access to the prompt message and can append
to the system message content.

────────────────────────────────────────────────────────────

Mapping to Rig Framework

What maps directly:

│ pi-adaptive-thinking Concept                │ Rig Concept                                                                                             │
├─────────────────────────────────────────────┼─────────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ Tool registration (pi.registerTool)         │ Implement Tool trait on a struct, add to ToolSet on the agent                                           │
│ Tool parameters (JSON schema)               │ #[derive(Deserialize, JsonSchema)] on Args struct                                                       │
│ Tool execution                              │ Tool::call(&self, args: Self::Args) -> Result<Self::Output, Self::Error>                                │
│ Tool definition (name, description, params) │ Tool::definition(&self, _prompt: String) -> ToolDefinition                                              │
│ System prompt manipulation                  │ PromptHook::on_completion_call(&self, prompt, history) — return HookAction::Modify to mutate the prompt │
│ Tool call interception                      │ PromptHook::on_tool_call(&self, tool_name, tool_call_id, internal_call_id, args)                        │
│ Tool call result                            │ PromptHook::on_tool_result(&self, tool_name, tool_call_id, internal_call_id, args, result)              │
│ Session-level state                         │ Attach Arc<Mutex<ThinkingState>> to both the Tool impl and the PromptHook impl                          │

What requires custom work in Rig:

1. Thinking level as a model parameter: Rig's CompletionRequestBuilder has
   additional_params(serde_json::Value) — the thinking level is passed as json!({"reasoning_effort":
   "high"}). The implementation must merge this into every completion request.

2. Temporary vs. persistent semantics: There's no settings.json equivalent in rig. The Rust
   implementation would keep state in memory only:

   • Temporary: Apply level, but save the prior level; restore after the tool result is consumed
   • Persistent: Apply level and store as the new baseline

3. Back-to-back call guard: Track via on_tool_call / on_tool_result hooks — if two consecutive calls
   target the thinking-level tool, skip the second.

4. Configuration loading: File system, but with serde_json instead of TypeBox. The cascade logic
   (project → global → defaults) is trivially portable.
