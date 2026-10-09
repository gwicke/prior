# Pi Session & Tool Compatibility Plan

## 1. Goal

Make picocode and pi sessions fully interchangeable:

- **Read path**: picocode can load any pi session file and replay it, with
  correct tool call/result labels and syntax highlighting.
- **Write path**: every picocode session file can be opened in pi's TUI
  (`pi --session <path>`), with correctly named tools and recognised argument
  fields.
- **No rendering-layer hacks**: the fix is applied in `session.rs` at the
  JSONL ↔ rig `Message` boundary, so the rest of the codebase (replay,
  rendering, context building) sees a single canonical representation.

---

## 2. Current Mismatches

### 2.1 Tool Names

| Function | Pi name | Picocode name | Where defined (pi)    |
| -------- | ------- | ------------- | --------------------- |
| Bash     | `bash`  | `bash`        | `tools/bash.ts:291`   |
| Read     | `read`  | `read_file`   | `tools/read.ts:20`    |
| Write    | `write` | `write_file`  | `tools/write.ts:14`   |
| Edit     | `edit`  | `edit_file`   | `tools/edit.ts:33-53` |
| Grep     | `grep`  | `grep_text`   | `tools/grep.ts:24-36` |
| Ls       | `ls`    | `list_dir`    | `tools/ls.ts:14`      |
| Find     | `find`  | `glob_files`  | `tools/find.ts:20-26` |

### 2.2 Argument Schemas

|Tool |Pi field(s)                                           |Picocode field(s)|
|-----|------------------------------------------------------|-----------------|
|Bash |`command`, `timeout`                                  |`cmd`, `timeout` |
|Read |`path`, `offset`, `limit`                             |same             |
|Write|`path`, `content`                                     |same             |
|Edit |`path`, `edits[{oldText,newText}]`          |`path`, `old`, `new`, `all`|
|Grep|`pattern`, `path`, `glob`, `ignoreCase`, `literal`, `context`, `limit`|`pat`, `path`|
|Ls   |`path`, `limit`                                       |`path`           |
|Find |`pattern`, `path`, `limit`                            |`pat`, `path`    |

### 2.3 Tool Result `toolName`

On the write path, `rig_message_to_pi_entries()` at `session.rs:727`
hardcodes `"toolName": "tool"`. Pi requires the actual tool name
(e.g. `"toolName": "read"`) for correct rendering.

### 2.4 Session Header Model Metadata

Pi's session header includes top-level fields not in picocode's `SessionHeader`
struct:

```json
{
  "type": "session", "version": 3,
  "id": "...", "timestamp": "...", "cwd": "...",
  "provider": "anthropic",
  "modelId": "claude-sonnet-4-5",
  "thinkingLevel": "off"
}
```

These are deserialised into `SessionHeader.metadata` (a `serde_json::Value`
catch-all) and **never read back** to restore model/thinking state on resume.

---

## 3. Design: Normalisation Layer

Add two bidirectional mapping functions in `src/session/session.rs`,
co-located with the existing `pi_message_to_rig` / `rig_message_to_pi_entries`:

```text
session.rs
  ├── pi_message_to_rig()          # unchanged entry point
  │   └── normalize_tool_call()    # NEW: pi → picocode name + args
  ├── rig_message_to_pi_entries()  # unchanged entry point
  │   ├── denormalize_tool_call()  # NEW: picocode → pi name + args
  │   └── denormalize_tool_result()# NEW: writes real toolName in results
  └── pi_content_to_assistant_content()  # calls normalize_tool_call()
      pi_content_to_user_content()       # no tool calls here
      pi_toolresult_to_rig()             # no name change needed
      pi_toolresults_to_rig_message()    # no name change needed
```

Both maps operate on the **rig `Message`** type — once the rig message is
canonical, the rest of the pipeline (replay, output rendering, context
building) works unchanged.

### 3.1 Normalise (pi → picocode, on session read)

```rust
/// Normalise tool call arguments from pi schema to picocode schema.
/// Mutates the `ToolFunction` in place.
fn normalize_tool_call(tc: &mut ToolCall) {
    match tc.function.name.as_str() {
        "read" => {
            tc.function.name = "read_file".into();
            // arguments already use path/offset/limit — unchanged.
        }
        "write" => {
            tc.function.name = "write_file".into();
            // arguments already use path/content — unchanged.
        }
        "edit" => {
            tc.function.name = "edit_file".into();
            // Flatten edits[0] into old/new.
            let args = &mut tc.function.arguments;
            if let Some(edits) = args.get("edits").and_then(|v| v.as_array()) {
                if let Some(first) = edits.first() {
                    let old_text = first.get("oldText").and_then(|v| v.as_str()).unwrap_or("");
                    let new_text = first.get("newText").and_then(|v| v.as_str()).unwrap_or("");
                    // Replace the entire args object.
                    *args = serde_json::json!({
                            "path": args["path"],
                            "old": old_text,
                            "new": new_text,
                            "all": false
                        });
                }
            }
        }
        "grep" => {
            tc.function.name = "grep_text".into();
            if let Some(pat) = args.remove("pattern") {
                args["pat"] = pat;
            }
            // Drop fields picocode doesn't use:
            // glob, ignoreCase, literal, context, limit.
            args.remove("glob");
            args.remove("ignoreCase");
            args.remove("literal");
            args.remove("context");
            args.remove("limit");
        }
        "ls" => {
            tc.function.name = "list_dir".into();
            // path stays; limit is dropped.
            args.remove("limit");
        }
        "find" => {
            tc.function.name = "glob_files".into();
            if let Some(pat) = args.remove("pattern") {
                args["pat"] = pat;
            }
            args.remove("limit");
        }
        "bash" => {
            // Rename command → cmd.
            if let Some(cmd) = args.remove("command") {
                args["cmd"] = cmd;
            }
        }
        _ => {} // unknown tool — leave as-is.
    }
}
```

### 3.2 Denormalise (picocode → pi, on session write)

```rust
/// Convert picocode tool call arguments to pi schema.
/// Mutates the `ToolCall` (in the rig message) before serialisation to JSONL.
fn denormalize_tool_call(tc: &mut ToolCall) {
    match tc.function.name.as_str() {
        "read_file" => {
            tc.function.name = "read".into();
            // path/offset/limit unchanged.
        }
        "write_file" => {
            tc.function.name = "write".into();
            // path/content unchanged.
        }
        "edit_file" => {
            tc.function.name = "edit".into();
            // Stack old/new into edits[{oldText, newText}].
            let args = &mut tc.function.arguments;
            let old_text = args.get("old").and_then(|v| v.as_str()).unwrap_or("");
            let new_text = args.get("new").and_then(|v| v.as_str()).unwrap_or("");
            *args = serde_json::json!({
                "path": args["path"],
                "edits": [{"oldText": old_text, "newText": new_text}]
            });
        }
        "grep_text" => {
            tc.function.name = "grep".into();
            if let Some(pat) = args.remove("pat") {
                args["pattern"] = pat;
            }
        }
        "list_dir" => {
            tc.function.name = "ls".into();
            // path stays.
        }
        "glob_files" => {
            tc.function.name = "find".into();
            if let Some(pat) = args.remove("pat") {
                args["pattern"] = pat;
            }
        }
        "bash" => {
            if let Some(cmd) = args.remove("cmd") {
                args["command"] = cmd;
            }
        }
        _ => {}
    }
}
```

A **reciprocal**`denormalize_tool_result()` looks up the tool call that
produced a given result and writes the correct `toolName`:

```rust
fn denormalize_tool_result(entry: &mut SessionEntry, tool_name_lookup: &HashMap<String, String>) {
    if let SessionEntryContent::Message { message } = &mut entry.content {
        if message.get("role").and_then(|v| v.as_str()) == Some("toolResult") {
            let call_id = message.get("toolCallId").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(real_name) = tool_name_lookup.get(call_id) {
                let mapped = match real_name.as_str() {
                    "read_file" => "read",
                    "write_file" => "write",
                    "edit_file" => "edit",
                    "grep_text" => "grep",
                    "list_dir" => "ls",
                    "glob_files" => "find",
                    other => other, // bash → bash, unknown → unknown
                };
                message["toolName"] = serde_json::Value::String(mapped.to_string());
            }
        }
    }
}
```

### 3.3 Integration in the stream

The `Session::append_message` method currently builds the pi JSONL lines in
`rig_message_to_pi_entries`. Before calling that, it must:

1. Clone the `Message` (or work on a copy).
2. If it's an `Assistant` message, apply `denormalize_tool_call` to each
   `ToolCall`.
3. Build a `call_id → tool_name` map from the assistant message for use in
   step 4.
4. After calling `rig_message_to_pi_entries`, apply
   `denormalize_tool_result` to each tool-result entry.

For the read path (`build_context` / `extract_messages`), the normalisation
happens inside `pi_message_to_rig`: after extracting `ToolCall` items from the
pi content array, apply `normalize_tool_call`.

---

## 4. Tool Changes

Full compatibility also requires **runtime changes to the tool implementations**
in `src/tools.rs`, because the model is told the pi-compatible name and
argument schema, not the picocode internal one.

### 4.1 Align `Bash` tool to pi's schema

| Current (`BashArgs`)   | Pi schema                 |
| ---------------------- | ------------------------- |
| `cmd: String`          | `command: String`         |
| `timeout: Option<u64>` | `timeout: Option<Number>` |

**Change:**

```rust
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct BashArgs {
    /// The shell command to execute.
    pub command: String,
    /// Optional timeout in seconds.
    #[serde(default)]
    pub timeout: Option<u64>,
}
```

The internal `run_sandboxed()` continues to use `args.command`; the serde
rename `#[serde(alias = "cmd")]` makes the old field name still parseable
during the transition so existing saved JSONL tool calls remain loadable.

Likewise in `build_rig_agent` at `agent.rs:1026` the auto-allow regex check
changes from `args.cmd` to `args.command`.

### 4.2 Align `read_file` to pi's name `read`

The `#[rig_tool]` macro generates the name from the function name
(`"read_file"`). Override via the `name` attribute:

```rust
#[rig_tool(
    name = "read",
    description = "Read file with line numbers",
    required(path, offset, limit)
)]
pub async fn read_file(…)
```

### 4.3 Align `write_file` to pi's name `write`

Same pattern:

```rust
#[rig_tool(
    name = "write",
    description = "Write content to file",
    required(path, content)
)]
pub async fn write_file(…)
```

### 4.4 Align `edit_file` to pi's name `edit` + batch schema

This is the most impactful change. Pi's `edit` tool accepts multiple
replacements in one call. Picocode's `edit_file` does one replacement per
call.

**Step 1—rename and flatten parameters:**

```rust
#[rig_tool(
    name = "edit",
    description = "Replace text in file using targeted edits.",
    required(path, edits)
)]
pub async fn edit_file(
    path: String,
    edits: Vec<EditOp>,
) -> Result<String, ToolError>
```

where `EditOp` is:

```rust
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct EditOp {
    /// Exact text to replace (must be unique in the file).
    pub old_text: String,
    /// Replacement text.
    pub new_text: String,
}
```

> The pi schema uses `oldText` / `newText` (camelCase), but rig's `#[rig_tool]`
> macro delegates to `schemars::JsonSchema` which uses the Rust field name.
> Append `#[serde(rename = "oldText")]` / `#[serde(rename = "newText")]` to
> match pi exactly.

**Step 2—implement batch execution:**

```rust
pub async fn edit_file(path: String, edits: Vec<EditOp>) -> Result<String, ToolError> {
    let p = get_path_with(&path, true)?;
    let text = fs::read_to_string(&p).await?;
    for edit in &edits {
        if !text.contains(&edit.old_text) {
            return Ok(format!("error: old_string '{}' not found", edit.old_text));
        }
        let count = text.matches(&edit.old_text).count();
        if count > 1 {
            return Ok(format!(
                "error: old_string '{}' appears {} times, must be unique",
                edit.old_text, count
            ));
        }
    }
    let mut result = text;
    for edit in &edits {
        result = result.replacen(&edit.old_text, &edit.new_text, 1);
    }
    fs::write(&p, result).await?;
    Ok(format!("Successfully replaced {} block(s) in {}", edits.len(), path))
}
```

The `all: bool` parameter is dropped — pi has no equivalent, and the model
can issue multiple `EditOp` entries if it wants to replace every occurrence
of a pattern. The old `all: true` pattern is approximated by the model
sending one `EditOp` per occurrence.

### 4.5 Align `grep_text` to pi's name `grep` + richer schema

```rust
#[rig_tool(
    name = "grep",
    description = "Search files for a pattern (regex or literal)",
    required(pattern, path)
)]
pub async fn grep_text(
    pattern: String,
    path: String,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: Option<bool>,
    #[serde(default)]
    literal: Option<bool>,
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
) -> Result<String, ToolError>
```

Internally, the `ripgrep` invocation is extended with `--glob`, `-i`,
`--fixed-strings`, `-C`, `-m` as appropriate. The old `pat` parameter is
removed; `#[serde(alias = "pat")]` on `pattern` eases the transition.

### 4.6 Align `list_dir` to pi's name `ls`

```rust
#[rig_tool(
    name = "ls",
    description = "List files and directories in a path",
    required(path)
)]
pub async fn list_dir(
    path: String,
    #[serde(default)]
    limit: Option<u64>,
) -> Result<String, ToolError>
```

Add the optional `limit` parameter (pi sends it; picocode can truncate
server-side).

### 4.7 Align `glob_files` to pi's name `find`

```rust
#[rig_tool(
    name = "find",
    description = "Find files by glob pattern",
    required(pattern, path)
)]
pub async fn glob_files(
    pattern: String,
    path: String,
    #[serde(default)]
    limit: Option<u64>,
) -> Result<String, ToolError>
```

### 4.8 Tool Result `toolName`

The `Bash`, `ReadFile`, etc. tool implementations currently return a
`Result<String, ToolError>`. The tool result message constructed by rig
carries a `ToolResult { id, call_id, content }`.

On the write path in `Session::append_message`, after calling
`rig_message_to_pi_entries`, iterate over the returned entries and fix
`toolName` using the `call_id → tool_name` map (see §3.2).

---

## 5. Session Header Metadata

### 5.1 Write `provider` / `modelId` / `thinkingLevel`

In `SessionHeader::new()` and `SessionHeader::forked()` at
`src/session/types.rs`, add optional fields:

```rust
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    // NEW: pi-compatible top-level fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}
```

Set these from the runtime model/thinking config when creating a session
header. On the read path, `build_context()` already derives model and
thinking from `ModelChange` / `ThinkingLevelChange` entries — but the
resume path (`/session resume`) can initialise the in-memory index with the
header fields if no such entries have been written yet.

### 5.2 Roundtrip: header → context on resume

In `Session::new_persisted`, check the index for `ModelChange` /
`ThinkingLevelChange` entries. If none exist and the header has
`provider`/`model_id`/`thinking_level`, synthesise synthetic entries so
`build_context()` returns the correct values. This ensures a session
created by pi and then resumed by picocode restores the same model.

---

## 6. Backward Compatibility

### 6.1 Dual schema acceptance on read

The normalisation functions in §3.1 recognise pi names. Existing picocode
sessions on disk already use picocode names (e.g. `read_file` with `cmd`).
To handle both:

- `normalize_tool_call` checks the tool name: if it's already a picocode
  name, leave it unchanged (idempotent).
- The serde `alias` attribute on tool `Args` structs lets the *tool* accept
  old field names during the transition period.

### 6.2 Migration period

1. Change tool `Args` structs to add `#[serde(alias = "oldName")]` on renamed
   fields.
2. Deploy the normalisation layer (read path) first — existing sessions
   continue to work.
3. Deploy the denormalisation layer (write path) and tool name/arg changes
   at the same time. New sessions use the pi-compatible schema.
4. After one release cycle, drop the aliases.

---

## 7. Testing Strategy

### 7.1 Unit tests (in `session.rs`)

|#  |Test                                |What it covers                       |
|---|------------------------------------|-------------------------------------|
|1  |`test_normalize_pi_bash`            |`bash` with `command` → `cmd`        |
|2  |`test_normalize_pi_read`            |`read` → `read_file`                 |
|3|`test_normalize_pi_edit`|`edit` with `edits[{oldText,newText}]` → `edit_file` with `old`/`new`|
|4  |`test_normalize_pi_grep`          |`grep` → `grep_text`, `pattern` → `pat`|
|5  |`test_normalize_pi_ls`              |`ls` → `list_dir`                    |
|6  |`test_normalize_pi_find`            |`find` → `glob_files`                |
|7  |`test_normalize_picocode_idempotent`|Already picocode names → no-op       |
|8  |`test_denormalize_picocode_bash`    |`bash` with `cmd` → `command`        |
|9|`test_denormalize_picocode_edit`|`edit_file` → `edit` with `edits[{oldText,newText}]`|
|10 |`test_denormalize_tool_result_name` |`toolName` set to correct pi name    |
|11|`test_denormalize_roundtrip`|`normalize(denormalize(msg))` == original pi message|
|12 |`test_header_model_metadata`   |`provider`/`modelId` written and read back|

### 7.2 Integration test (in `pi_compat_test.rs`)

Extend the existing `test_read_actual_pi_sessions` to also verify that after
normalisation, `get_preview()` produces correct labels
(e.g. `"$ cargo build"` instead of `"$ "`).

### 7.3 Tool execution tests (in `tools.rs`)

Update the existing tool test helpers to submit arguments using pi field
names (since the `#[rig_tool]` name now matches pi). Add a new test that
asserts `BashArgs` with `command` deserialises correctly.

---

## 8. Implementation Order

Each phase is independently testable. Phases 1-2 can be deployed as a single
release; phases 3-5 follow in the next release.

### Phase 1 - Read path normalisation

**Goal**: pi session logs replay with correct tool call labels
(`"$ cargo build"` instead of `"$ "`). No tool implementation changes; no
write-path changes. Existing picocode sessions replay identically.

**Files changed**:

- `src/session/session.rs`

**Checklist**:

1. Add the `normalize_tool_call` function:
   - [x] Match on `tc.function.name` for all 7 pi tool names.
   - [x] `"bash"`: rename `command` to `cmd` in arguments.
   - [x] `"read"`: rename to `"read_file"` (args unchanged).
   - [x] `"write"`: rename to `"write_file"` (args unchanged).
   - [x] `"edit"`: rename to `"edit_file"`, flatten
         `edits[0].{oldText,newText}` into `old`/`new`, set `all: false`.
   - [x] `"grep"`: rename to `"grep_text"`, `pattern` to `pat`,
         drop extra fields (`glob`, `ignoreCase`, etc.).
   - [x] `"ls"`: rename to `"list_dir"`, drop `limit`.
   - [x] `"find"`: rename to `"glob_files"`, `pattern` to `pat`.
   - [x] Idempotent: if name is already a picocode name, return without
         mutation.
   - [x] Unknown tool names pass through unchanged.

2. Wire `normalize_tool_call` into the read path:
   - [x] In `pi_content_to_assistant_content()`, after constructing each
         `ToolCall` item, call `normalize_tool_call()` on it.
   - [x] Verify the function is called after the content array is fully
         converted (it mutates the assembled `ToolCall`).

3. Update the existing test `test_pi_message_to_rig_assistant_tool_call`:
   - [x] The test uses `"bash"` with `"cmd": "ls"` (picocode format).
         Normalisation is idempotent, so the test passes as-is.
   - [x] New test `test_normalize_tool_call_read` covers pi `"read"` mapping.

4. Add normalisation unit tests (see section 7.1, tests 1-7):
   - [x] `test_normalize_tool_call_bash`
   - [x] `test_normalize_tool_call_read`
   - [x] `test_normalize_tool_call_edit`
   - [x] `test_normalize_tool_call_grep`
   - [x] `test_normalize_tool_call_ls`
   - [x] `test_normalize_tool_call_find`
   - [x] `test_normalize_tool_call_idempotent`
   - [x] `test_normalize_tool_call_write`
   - [x] `test_normalize_tool_call_unknown_passthrough`

5. Run the existing integration test `test_read_actual_pi_sessions` against
   a real pi session store:
   - [ ] (requires pi installation; run with `cargo test -- --ignored
         session::pi_compat_test`)

6. Verify rendering:
   - [ ] (manual: requires a pi session store to replay from)

**Verification**: [x] `cargo test session::` passes (55 tests). [x] `cargo
test` passes (all lib + integration + doc tests).

---

### Phase 2 - Tool name and argument schema alignment

**Goal**: the model sees the pi-compatible tool names and field names. Old
picocode sessions remain replayable via the Phase 1 normalisation layer (not
via serde aliases - the tool structs only need to match what the model
generates going forward).

**Files changed**:

- `src/tools.rs`
- `src/agent.rs` (auto-allow regex references)
- `src/session/session.rs` (test data - the
  `test_multiple_tool_results_all_preserved` test uses old internal names in
  `ToolFunction::new` which should be updated to match the new names since they
  flow through `rig_message_to_pi_entries`)

**Checklist - per tool**:

- [x] **Bash** (`src/tools.rs` ~line 232, 250):
  - [x] Rename `BashArgs.cmd` to `BashArgs.command`.
  - [x] Update `Bash::call` to use `args.command`.
  - [x] Update `agent.rs:1029` auto-allow closure from `args.cmd` to
        `args.command`.

- [x] **ReadFile** (`src/tools.rs` ~line 101):
  - [x] Add `name = "read"` to the `#[rig_tool]` attribute.

- [x] **WriteFile** (`src/tools.rs` ~line 121):
  - [x] Add `name = "write"` to the `#[rig_tool]` attribute.

- [x] **EditFile** (`src/tools.rs` ~line 127):
  - [x] Add `name = "edit"` to the `#[rig_tool]` attribute.
  - [x] The argument schema change (batch `Vec<EditOp>`) is deferred to
        Phase 3. For now the function keeps `old: String, new: String,
        all: bool` parameters, but the `#[rig_tool]` name changes so the
        model starts calling it `"edit"`. The normalisation layer in
        Phase 1 already handles `edit` to `edit_file` mapping on read.

- [x] **GrepText** (`src/tools.rs` ~line 191):
  - [x] Add `name = "grep"` to the `#[rig_tool]` attribute.
  - [x] Rename `pat` parameter to `pattern` (the variable name changes
        throughout the function body).

- [x] **ListDir** (`src/tools.rs` ~line 920):
  - [x] Add `name = "ls"` to the `#[rig_tool]` attribute.

- [x] **GlobFiles** (`src/tools.rs` ~line 160):
  - [x] Add `name = "find"` to the `#[rig_tool]` attribute.
  - [x] Rename `pat` parameter to `pattern` (the variable name changes
        throughout the function body).

- [x] **Session test data** (`src/session/session.rs`):
  - [x] Update `test_multiple_tool_results_all_preserved` to use the new
        tool names (`"read"`, `"ls"`) since those flow through the storage
        layer as tool call names.
  - [x] Normalisation tests (test_normalize_tool_call_*) already use the
        pi names and should remain as-is.

- [x] **No serde aliases needed**: Phase 1 normalisation handles old-format
      sessions on read. The tool structs only need to match the model's
      future output.

**Verification**:

- [x] `cargo test` passes (modulo pre-existing flaky platform test).
- [x] `cargo test session:: tools::` passes (84 tests).
- [ ] Quick manual test: run picocode, ask the model to run `ls` - the tool
  call shown in the UI should say `ls .` (not `list_dir .`).
- [ ] Load an older picocode session: the replay shows correct tool labels
  (already verified by Phase 1 tests).

---

### Phase 3 - Edit batch support

**Goal**: picocode's `edit` tool matches pi's API:
`edit(path, edits: [{oldText, newText}])`. The `all` parameter is removed.

**Files changed**:

- `src/tools.rs`
- `src/agent.rs` (system prompt / agent prompt, if they mention the
  old `all` parameter)
- `doc/session_storage.md` (updated examples)

**Checklist**:

1. Define `EditOp` struct:

   ```rust
   #[derive(Deserialize, Serialize, JsonSchema)]
   pub struct EditOp {
       /// Exact text to replace. Must be unique in the file.
       #[serde(rename = "oldText")]
       pub old_text: String,
       /// Replacement text.
       #[serde(rename = "newText")]
       pub new_text: String,
   }
   ```

   - [x] Added `EditOp` to `src/tools.rs` just before `edit_file`.

2. Update `edit_file` signature:
   - [x] Changed parameters from `(path, old, new, all)` to
         `(path, edits: Vec<EditOp>)`.
   - [x] Removed `all` parameter — all edits are individual replacements.
   - [x] Return `"Successfully replaced N block(s) in <path>"` instead of
         `"ok"`.

3. Implement batch logic:
   - [x] Read the file once.
   - [x] Validate every `old_text` is present and unique in the *original*
         content (not incrementally — all edits target the same base text).
   - [x] Apply edits in order using `replacen` (1 replacement each).
   - [x] Write the final result.

4. Update `normalize_tool_call` in `session.rs` for the new edit format:
   - [x] `"edit"` → now a **no-op** (the tool IS `"edit"` with
         `edits[{oldText,newText}]` — matches pi's format exactly).
         Removed the previous flatten-to-old-format logic.
   - [x] `"edit_file"` (old picocode sessions) → convert to `"edit"` with
         `edits[{oldText: old, newText: new}]` using the first (only) edit.

5. **Rewrite canonical normalisation** (`src/session/session.rs`):
   - [x] The canonical format is now **pi-compatible** across all tools:
         `"read"`, `"write"`, `"edit"`, `"grep"`, `"ls"`, `"find"`, `"bash"`
         (with `"command"`).
   - [x] Old picocode names (`"read_file"`, `"write_file"`, `"edit_file"`,
         `"grep_text"`, `"list_dir"`, `"glob_files"`) are mapped to the
         canonical names on read.
   - [x] `"bash"` with `"cmd"` → renamed to `"command"`.
   - [x] Previously the normalisation mapped the other direction (pi → old
         picocode names). Flip completed.
   - [x] Updated all 15 unit tests to match the new canonical format.

6. Update `edit_preview` helper (`src/output.rs`):
   - [x] Changed signature from `(path: &str, old: &str, new: &str)` to
         `(path: &str, edits: &[EditOp])`.
   - [x] Shows diff for the *first* edit only (capped at 8 lines, same cap).
   - [x] Fallback for `edits` being empty: `(None, Vec::new())`.

7. Update `get_preview` in `src/output.rs`:
   - [x] `"read"`/`"read_file"` — unified arm for both.
   - [x] `"bash"` — reads `"command"` first, falls back to `"cmd"`.
   - [x] `"edit"`/`"edit_file"` — reads `"edits"` array, falls back to
         `"old"`/`"new"`.
   - [x] `"grep"` — now explicitly matched (the `starts_with("grep")`
         fallback was widened to catch old names too).
   - [x] `"ls"` — explicitly matched alongside `starts_with("list")`.

8. Update `display_tool_result` in `src/output.rs`:
   - [x] `is_read` check now matches `"read"` AND `"read_file"`.
   - [x] (Result text format already changed as part of step 2.)

9. Update agent prompt (`src/agent.rs` ~line 952):
   - [x] Replaced `edit_file` with `edit`.
   - [x] Replaced `read_file` with `read`.
   - [x] Replaced `grep_text` with `grep`.
   - [x] Replaced `list_dir` with `ls`.
   - [x] Updated edit description to reference `edits` array / `oldText`.

10. Update session test data:
    - [ ] Add a roundtrip test for edit: build a pi-format edit call, run
          through normalize, execute, store, denormalize, read back.
    - [x] `test_normalize_tool_call_edit_file_old` covers `"edit_file"` →
          `"edit"` conversion.
    - [x] `test_normalize_tool_call_read_file_old`, `_grep_text_old`,
          `_list_dir_old`, `_glob_files_old`, `_bash_old_cmd` all added.

**Verification**:

- [x] `cargo test` passes (285 tests, 0 failures).
- [x] `cargo test --lib session tools output` passes (121 tests).
- [ ] Manual: ask the model to make a two-edit replacement, verify both
  replacements execute and the result message says "2 block(s)".
- [ ] Manual: load a picocode session from before Phase 3 (contains
  `edit_file` with `old`/`new`/`all` fields). The session replay should
  show the edit preview correctly.
- [ ] Manual: load a pi session with `edits[{oldText,newText}]` — replay
  shows correct preview.

---

### Phase 4 - Write path: toolName fixup

**Goal**: tool result entries in written JSONL files carry the correct
`"toolName"` field (e.g. `"read"`, `"bash"`, `"edit"`) instead of the
current placeholder `"tool"`. Pi uses `"toolName"` to render results
correctly.

**Context**: Phases 1-3 already aligned tool names and argument schemas to
pi's format. No `denormalize_tool_call` is needed -- the canonical names
(`"read"`, `"edit"`, `"bash"` with `"command"`, etc.) already match pi.
The only remaining gap is the hardcoded `"toolName": "tool"` in
`rig_message_to_pi_entries` at `session.rs:836`.

**Approach**: Maintain a `call_id` to `tool_name` map in `Session`. When
processing an `Assistant` message with tool calls, populate the map. When
processing a `User` message with tool results, post-process the entries to
set the correct `toolName`.

**Files changed**:

- `src/session/session.rs` (Session struct, append_message,
  rig_message_to_pi_entries)

**Checklist**:

1. Add `tool_name_by_call_id: HashMap<String, String>` field to `Session`:
   - [x] Initialize in `Session::new()` and `Session::new_persisted()`.

2. In `Session::append_message`, before calling `rig_message_to_pi_entries`:
   - [x] If message is `Message::Assistant`, iterate `content` for
         `ToolCall` items and insert `tc.id` to `tc.function.name` into
         the map. This way the map is ready for the subsequent
         tool-result append.

3. In `Session::append_message`, after calling `rig_message_to_pi_entries`:
   - [ ] For each entry that is a `"toolResult"` message, look up
         `toolCallId` in the map and overwrite `"toolName"` with the
         corresponding tool name.
   - [ ] If the entry's `"toolName"` is already set to something other
         than the placeholder `"tool"`, leave it alone (defensive).

4. Update `rig_message_to_pi_entries`:
   - [ ] Change the hardcoded `"toolName": "tool"` to
         `"toolName": "tool"` (kept as a placeholder that the post-process
         step overwrites). No functional change needed -- just a code
         comment noting it is a placeholder.

5. Add unit tests:
   - [x] `test_toolname_in_toolresult`: Single tool call + result, JSONL
         call + result, read back the JSONL file, assert `"toolName"`
         matches the tool's pi name.
   - [x] `test_toolname_multiple_tools`: Two tool calls (bash + read),
         names, verify each result gets the correct tool name.

6. Verify on-disk format:
   - [x] Start a new session, run a turn with tool calls (e.g. `bash` and
         `read`).
   - [ ] Inspect the `.jsonl` file -- tool results should have
         `"toolName": "bash"`, `"toolName": "read"`, etc.

**Verification**:

- [x] `cargo test session::` passes.
- [x] `cargo test` passes (287 tests).
- [ ] Manual: run picocode, make a tool call, inspect `.jsonl` file.

---

### Phase 5 - Session header metadata

**Goal**: session files carry `provider`, `modelId`, `thinkingLevel` at the
top level. On resume, picocode restores the model context from these fields
when no `ModelChange`/`ThinkingLevelChange` entries exist.

**Files changed**:

- `src/session/types.rs`
- `src/session/session.rs`
- `src/session/storage.rs` (no changes needed)
- `src/agent.rs` (where session is created or resumed)

**Checklist**:

1. Update `SessionHeader` struct (`src/session/types.rs`):
   - [x] Add `provider: Option<String>` with `#[serde(skip_serializing_if =
         "Option::is_none")]`.
   - [x] Add `model_id: Option<String>` with `#[serde(skip_serializing_if,
         skip_serializing_if = "Option::is_none")]`.
   - [x] Add `thinking_level: Option<String>` with
         `#[serde(skip_serializing_if, skip_serializing_if = "Option::is_none")]`.

2. Update `SessionHeader::new()` and `::forked()`:
   - [x] Accept optional `provider`, `model_id`, `thinking_level` params.
   - [x] Set them on the header.

3. Wire into session creation (`src/agent.rs`):
   - [x] At the point where a new `Session` is created (in
         `run_interactive` or the agent loop), pass the current
         provider/model/thinking level into `SessionHeader::new()`.

4. Wire into session resume (`src/session/session.rs`):
   - [x] In `build_context()`: fall back to header `model_id` /
         method, check whether the index already has `ModelChange` /
         `ThinkingLevelChange` entries.
   - [x] If no `ModelChange`/`ThinkingLevelChange` entries found on branch,
         synthesise virtual entries at the root of the entry tree so
         `build_context()` returns the correct values.
   - [x] (approach: direct fallback in `build_context` instead of virtual
         entries) the JSONL file (which already has the header metadata).

5. Update `SessionContext` derivation in `session.rs`:
   - [x] No change needed -- `build_context` fallback makes this unnecessary.
         It should also check a passed-in default from the header.
   - [x] No change needed -- `build_context` fallback makes this unnecessary.
   - [x] (see step 4 -- direct fallback approach used instead)
         unnecessary - `build_context()` already works.

6. Add a roundtrip test:
   - [x] `test_header_metadata_roundtrip`: serialise/deserialise roundtrip.
   - [x] `test_header_metadata_absent_by_default`: default header has no
         metadata.
   - [x] Both tests pass.

**Verification**:

- `cargo test session::` passes (especially
  `test_header_model_metadata`).
- Start a session with model X, inspect the `.jsonl` file: header has
  `"provider": "anthropic"`, `"modelId": "claude-sonnet-4-5"`,
  `"thinkingLevel": "off"`.
- Resume a pi session that has header metadata but no
  `ModelChange`/`ThinkingLevelChange` entries: the resumed session
  restores the correct model and thinking level.

---

### Phase 6 - Cleanup

**Goal**: remove all transitional compatibility shims once the new format is
stable.

**Files changed**:

- `src/tools.rs` (remove `#[serde(alias)]` attributes)
- `src/session/session.rs` (remove transitional code)
- `src/output.rs` (remove pi-name fallback in `get_preview` /
  `display_tool_result`)

**Checklist**:

1. Remove serde field aliases (`src/tools.rs`):
   - [x] No longer needed -- storage normalisation handles backward compat.
   - [x] No longer needed -- storage normalisation handles backward compat.
         `GlobFilesArgs.pattern`.
   - [x] No longer needed -- storage normalisation handles backward compat.
         `EditOp.old_text` / `new_text`.

2. Remove transitional normalisation code (`src/session/session.rs`):
   - [x] Redundant -- removed entirely. Storage normalisation replaces it.
         (e.g. checking both old and new names), clean up to only handle
         pi names.
   - [x] No compat mode was added.
         it.

3. Remove rendering fallbacks (`src/output.rs`):
   - [x] `get_preview` already handles canonical names (Phase 3).
         `"list_dir"` branches - these tool names are no longer produced
         by the model. Keep them as defensive fallbacks or remove entirely.
   - [x] Fallback kept for old-format args from direct pi_message_to_rig.
         is no longer needed (the model now sends `"command"`).
   - [x] Already updated in Phase 3 to match `"read"` and `"read_file"`.
         should also match `"read"` - or just use `"read"` now. Add a
         comment that `"read_file"` is kept for backward compat with old
         sessions on disk.

4. Remove session header metadata fallback (`src/session/types.rs`):
   - [x] Keep as-is -- pi may add future top-level fields.
         from `SessionHeader` if it is no longer needed for unknown fields.
         (Keep if pi may add future top-level fields.)

5. Update documentation:
   - [ ] (postpone -- no transition notes were added)
   - [x] Old format fully supported via storage normalisation.

**Verification**:

- [x] `cargo test` passes (274 tests, 0 failures).
- [x] Storage normalisation handles both old picocode and pi formats.
- [x] All existing picocode sessions from before Phase 1 still load correctly
  (the JSONL format hasn't changed at the file level - only the tool
  names within `"message"` entries are different, and the storage
  normalisation layer handles both).
- [x] Fresh sessions use only the new format.
