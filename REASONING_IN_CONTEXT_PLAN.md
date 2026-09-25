# Implementation Plan: reasoning_in_context "aborted_turns_only" Mode

## Overview
Add a third option to `reasoning_in_context` setting: `aborted_turns_only` (in addition to `true`/`false`).
When set, preserve reasoning content from aborted/errored turns in the LLM context, allowing the model to resume its thinking.

## Files to Modify

### 1. settings.rs
- Create `ReasoningInContextMode` enum: `Always`, `Never`, `AbortedTurnsOnly`
- Update `SettingsStore.reasoning_in_context` from `bool` to `ReasoningInContextMode`
- Update `SettingKey::ReasoningInContext` validation to accept three values
- Update `overlay_rows`, `save`, `apply`, `value` methods
- Update `from_agent_config`, `from_config`, `apply_to_config`

### 2. config.rs
- Change `reasoning_in_context: Option<bool>` to `Option<ReasoningInContextMode>`
- Update serde deserialization

### 3. agent.rs
- Update `AgentConfig.reasoning_in_context` to use the enum
- Update `CodeAgent.reasoning_in_context` to use the enum
- Modify `prompt()` streaming loop:
  - Add `reasoning_buffer: String` alongside `output`
  - Accumulate reasoning from `StreamedAssistantContent::Reasoning` and `ReasoningDelta`
- On abort (Esc) and non-cancellation errors:
  - Create partial assistant message with BOTH text AND reasoning when buffer not empty
  - Use `Message::Assistant { id: None, content: [...] }` (id:None marks aborted turns)
- Modify `maybe_strip_reasoning()`:
  - Accept `ReasoningInContextMode` instead of `bool`
  - When mode is `AbortedTurnsOnly`: preserve reasoning for messages with `id: None`
  - When mode is `Always`: preserve all reasoning
  - When mode is `Never`: strip all reasoning (except Anthropic+tools)
- Update all call sites of `maybe_strip_reasoning()`

### 4. main.rs / repl.rs / v25_*.rs
- Update all `AgentConfig` constructions to use new enum

## Detection Strategy for Aborted Turns
- Successful turns: Have provider `responseId` → `Message::Assistant { id: Some(...), ... }`
- Aborted/errored turns: Created locally with `Message::assistant(&text)` → `id: None`
- This is the reliable marker to distinguish them in `maybe_strip_reasoning()`

## Context Filtering Logic

| Mode | Successful Turns | Aborted Turns |
|------|------------------|---------------|
| `never` | Strip reasoning | Strip reasoning |
| `aborted_turns_only` | Strip reasoning | **Keep reasoning** |
| `always` | Keep reasoning | Keep reasoning |

## Testing
- Verify storage always preserves reasoning (unchanged)
- Verify context filtering works per mode
- Verify aborted turn reasoning appears in next turn's context
- Test all three modes with `/set reasoning_in_context <value>`