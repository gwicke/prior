use console::{style, StyledObject};
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::io::IsTerminal;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;
use termimad;
use markdown_stream::{Parser, StreamParser};
use markdown_terminal::{Renderer, Theme};
use libc;
use crossterm::event::{self, Event, KeyCode};
use crate::repl::Repl;
use crate::settings::{ShowReasoning, ShowReasoningHandle};
use crate::tools::EditOp;

#[derive(Debug, PartialEq)]
pub enum Confirmation {
    Yes,
    No,
    Always,
}

pub trait Output: Send + Sync {
    fn display_text(&self, text: &str);
    /// Render a model *response* as markdown. Defaults to `display_text` so
    /// non-console impls (quiet/log/no-op) need no change; `ConsoleOutput`
    /// overrides it with full block-markdown rendering (code blocks, tables,
    /// headings, lists) via `codewandler-markdown-terminal`. Error/status text
    /// should still go through `display_text` (inline/plain), not this.
    fn display_markdown(&self, text: &str) {
        self.display_text(text);
    }
    // ---- Response event sequence ----
    // Both the live streaming path (`PicoAgent::prompt`) and the session
    // replay path (`replay_units`) drive these methods in the same
    // order, so `ConsoleOutput` only needs one rendering implementation and
    // replay output is guaranteed to match live output.
    //
    // Required call order per agent response:
    //
    //   begin_response()
    //   set_answer_hold(bool)
    //     ( stream_thinking(block) | stream_text(block) )*    ← segment N
    //     [ end_response_segment()                            ← before each tool block
    //         ( display_tool_call(name, args)
    //           display_tool_result(result) )+
    //         ( stream_thinking(block) | stream_text(block) )*  ← segment N+1
    //     ]*
    //   end_response()
    //
    // In the live path, blocks arrive as streaming deltas; in replay they
    // arrive as complete strings. Default no-ops keep non-console impls
    // (quiet / log / no-op) compiling without change.
    /// Called once when an agent response begins; sets up per-response state.
    fn begin_response(&self) {}
    /// A delta of assistant answer text (live path) or a complete text block
    /// (replay path). Called once or more between `begin_response` and
    /// `end_response` / `end_response_segment`.
    fn stream_text(&self, _delta: &str) {}
    /// A delta of model reasoning/thinking (live) or a complete reasoning
    /// block (replay). Already gated by `show_reasoning` level upstream;
    /// implementations may render or suppress unconditionally.
    fn stream_thinking(&self, _delta: &str) {}
    /// Flush the current text/reasoning segment before a tool block. Called
    /// once per tool-using segment, immediately before `display_tool_call`.
    /// Mirrors the `CompletionCall` stream event that fires before tool
    /// execution in the live path.
    fn end_response_segment(&self) {}
    /// Called once when the whole response is complete; flush the final
    /// segment and reset per-response state.
    fn end_response(&self) {}
    /// Enable/disable answer holding for a reasoning-capable turn. When
    /// `true`, answer text is buffered until the turn ends so that reasoning
    /// arriving after the answer (a live-stream ordering artefact) still
    /// renders above it. Always `false` in replay — content is already in
    /// the correct sequence. Default no-op for non-console outputs.
    fn set_answer_hold(&self, _hold: bool) {}
    fn display_tool_call(&self, name: &str, args: &Value);
    fn display_tool_result(&self, result: &str);
    /// Render a tool execution failure in a dedicated error panel.
    ///
    /// `kind` is a stable, machine-readable classification from
    /// `rig_core::tool::ToolErrorKind::as_str()` (e.g. `"invalid_args"`,
    /// `"timeout"`, `"not_found"`, `"permission_denied"`, `"rate_limited"`,
    /// `"provider"`, `"network"`, `"cancelled"`, `"other"`). `message` is
    /// the operator-facing `ToolExecutionError::message()` — redaction-safe
    /// because rig strips operator-only content into the kind-level
    /// `model_feedback()` separately.
    ///
    /// Default impl formats `{tool_name} failed [{kind}]: {message}` and
    /// forwards to `display_error`. `ConsoleOutput` overrides this with
    /// a styled error panel using the same `paint_panel` /
    /// `tool_panel` helpers as the tool-call and tool-result panels
    /// (`src/output.rs:1388` / `~1423`).
    fn display_tool_error(&self, tool_name: &str, kind: &str, message: &str) {
        self.display_error(&format!("{tool_name} failed [{kind}]: {message}"));
    }
    fn get_user_input(&self) -> String;
    fn display_error(&self, error: &str);
    fn display_system(&self, text: &str);
    fn confirm(&self, message: &str) -> Confirmation;
    /// Ask the user to accept or cancel a pending compaction summary after
    /// they have reviewed/edited it in an external editor. `true` = accept
    /// (write the entry with the possibly-edited summary); `false` = cancel
    /// (write nothing). Default implementations accept immediately — there is
    /// no dialog outside the interactive `ConsoleOutput`.
    fn confirm_compaction_summary(&self, _message: &str) -> bool {
        true
    }
    fn display_separator(&self);
    fn display_thinking(&self, message: &str);
    fn stop_thinking(&self);
    fn display_header(
        &self,
        provider: &str,
        model: &str,
        yolo: bool,
        limit: usize,
        persona: Option<&str>,
    );
    /// Begin capturing rendered output into a byte buffer.  Returns `None`
    /// for non-console implementations.  Must be paired with a subsequent
    /// call to [`end_capture`] which returns the captured bytes.
    fn begin_capture(&self) {}
    /// End a previous [`begin_capture`] and return the captured bytes.
    /// Returns an empty vec for non-console implementations.
    fn end_capture(&self) -> Vec<u8> { Vec::new() }
    /// Print plain text with no decorative prefix (used for the startup
    /// hint/status line and REPL-originated confirmations). Advances the
    /// tracked cursor by the wrapped line count.
    fn display_plain(&self, _text: &str) {}
    /// Current cursor row (1-based). The REPL uses it to position the input
    /// line at the natural cursor rather than forcing it to the screen
    /// bottom (which would leave a gap in the scrollback). The default
    /// returns the terminal height (correct for non-console outputs, which
    /// don't run the raw-mode REPL).
    fn cursor_row(&self) -> usize {
        crossterm::terminal::size().map(|(_, h)| h as usize).unwrap_or(24)
    }

    /// Gate the background in-flight reader should honor. While it is `true`,
    /// the reader must not read stdin, because a confirmation dialog on the
    /// main task owns stdin. Returns `None` for outputs that don't run the
    /// raw-mode REPL (so there is no background reader to coordinate with).
    fn confirm_gate(&self) -> Option<Arc<AtomicBool>> {
        None
    }
}

pub struct QuietOutput {
    spinner: Mutex<Option<ProgressBar>>,
    show_reasoning: ShowReasoningHandle,
}

impl QuietOutput {
    pub fn new(show_reasoning: ShowReasoningHandle) -> Self {
        Self {
            spinner: Mutex::new(None),
            show_reasoning,
        }
    }

    fn create_spinner(message: &str) -> ProgressBar {
        let pb = ProgressBar::new_spinner();
        pb.enable_steady_tick(Duration::from_millis(120));
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.blue} {msg}")
                .unwrap(),
        );
        pb.set_message(message.to_string());
        pb
    }
}

impl Output for QuietOutput {
    fn display_text(&self, _text: &str) {}
    fn display_tool_call(&self, _name: &str, _args: &Value) {}
    fn display_tool_result(&self, _result: &str) {}
    fn get_user_input(&self) -> String {
        String::new()
    }
    fn display_error(&self, error: &str) {
        self.stop_thinking();
        eprintln!("Error: {}", error);
    }
    fn display_system(&self, _text: &str) {}
    fn confirm(&self, message: &str) -> Confirmation {
        self.stop_thinking();
        eprintln!("Confirm: {} [y/n/s]", message);
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
        match input.trim().to_lowercase().as_str() {
            "y" | "yes" => Confirmation::Yes,
            "s" | "session" => Confirmation::Always,
            _ => Confirmation::No,
        }
    }
    fn display_separator(&self) {}
    fn display_thinking(&self, message: &str) {
        // Indicator mode => show the spinner; otherwise suppress it.
        if *self.show_reasoning.lock().unwrap() != ShowReasoning::Indicator {
            return;
        }
        let mut spinner_lock = self.spinner.lock().unwrap();
        if spinner_lock.is_none() {
            *spinner_lock = Some(Self::create_spinner(message));
        }
    }
    fn stop_thinking(&self) {
        if let Some(pb) = self.spinner.lock().unwrap().take() {
            pb.finish_and_clear();
        }
    }
    fn display_header(
        &self,
        _provider: &str,
        _model: &str,
        _yolo: bool,
        _limit: usize,
        _persona: Option<&str>,
    ) {
    }
}

pub struct NoOutput;

impl Output for NoOutput {
    fn display_text(&self, _text: &str) {}
    fn display_tool_call(&self, _name: &str, _args: &Value) {}
    fn display_tool_result(&self, _result: &str) {}
    fn get_user_input(&self) -> String {
        String::new()
    }
    fn display_error(&self, _error: &str) {}
    fn display_system(&self, _text: &str) {}
    fn confirm(&self, _message: &str) -> Confirmation {
        Confirmation::Yes
    }
    fn display_separator(&self) {}
    fn display_thinking(&self, _message: &str) {}
    fn stop_thinking(&self) {}
    fn display_header(
        &self,
        _provider: &str,
        _model: &str,
        _yolo: bool,
        _limit: usize,
        _persona: Option<&str>,
    ) {
    }
}

pub struct LogOutput;

impl Output for LogOutput {
    fn display_text(&self, text: &str) {
        tracing::info!(target: "picocode", "{}", text);
    }

    fn display_tool_call(&self, name: &str, args: &Value) {
        tracing::info!(target: "picocode", "Tool call: {} with args: {:?}", name, args);
    }

    fn display_tool_result(&self, result: &str) {
        tracing::info!(target: "picocode", "Tool result: {}", result);
    }

    fn get_user_input(&self) -> String {
        String::new()
    }

    fn display_error(&self, error: &str) {
        tracing::error!(target: "picocode", "{}", error);
    }

    fn display_system(&self, text: &str) {
        tracing::debug!(target: "picocode", "System: {}", text);
    }

    fn confirm(&self, _message: &str) -> Confirmation {
        Confirmation::No
    }

    fn display_separator(&self) {}

    fn display_thinking(&self, _message: &str) {}

    fn stop_thinking(&self) {}

    fn display_header(
        &self,
        provider: &str,
        model: &str,
        yolo: bool,
        limit: usize,
        persona: Option<&str>,
    ) {
        tracing::info!(target: "picocode", "picocode | {} | {} | persona:{} | yolo:{} limit:{}", provider, model, persona.unwrap_or("default"), yolo, limit);
    }
}

pub struct ConsoleOutput {
    spinner: Mutex<Option<ProgressBar>>,
    row: Mutex<usize>,
    /// Set `true` while `confirm` is reading a keystroke, so the background
    /// in-flight reader thread backs off and doesn't swallow the input.
    reader_paused: Arc<AtomicBool>,
    /// Buffered assistant text while a response streams (non-TTY / pipe path;
    /// rendered once at `end_response` via `display_markdown`). The TTY live
    /// path uses the incremental `parser`/`renderer` instead.
    text_buffer: Mutex<String>,
    /// Whether the current response is being rendered live (TTY) or buffered
    /// (non-TTY). Decided once per response in `begin_response`.
    live: Mutex<bool>,
    /// The destination for rendered output.  Defaults to `std::io::stdout()`;
    /// a `Vec<u8>` writer is used by the `/replay Nl` capture path so the
    /// fully-rendered ANSI output can be tailed and re-printed.
    writer: Mutex<Box<dyn io::Write + Send>>,
    /// Incremental GFM markdown parser for the current response (TTY only).
    /// Fed the streamed deltas; emits events for *completed* blocks so the
    /// renderer can print them once, avoiding whole-document re-renders.
    parser: Mutex<StreamParser>,
    /// Incremental terminal renderer (TTY only): prints each completed block
    /// once as it closes, so code blocks stream line-by-line and paragraphs
    /// appear on close — coarse-grained, with clean scrollback.
    renderer: Mutex<Renderer>,
    /// Incremental parser/renderer for *thinking* content (TTY only). Kept
    /// separate from the answer parser/renderer so thinking and answer blocks
    /// have independent state; thinking is rendered inline and dimmed as it
    /// streams (Phase 5).
    thinking_parser: Mutex<StreamParser>,
    thinking_renderer: Mutex<Renderer>,
    /// Whether a dim SGR is currently active on stdout. Toggled at thinking /
    /// answer boundaries so streamed thinking reads faded *without* re-rendering
    /// any already-emitted content.
    dim_on: Mutex<bool>,
    /// When true (a reasoning-capable turn), the assistant answer text is
    /// buffered instead of rendered, and only flushed once the turn ends
    /// (`end_response_segment` / `end_response`). This keeps reasoning that
    /// arrives *after* the answer pinned *above* it rather than leaking below.
    /// Set via `set_answer_hold` at the start of each response.
    answer_held: Mutex<bool>,
    /// Buffered assistant answer text while `answer_held` is true. Flushed at
    /// the turn boundary.
    answer_buffer: Mutex<String>,
    /// Live `show_reasoning` display mode. Read on every thinking event so a
    /// `/set show_reasoning …` change applies immediately (no agent rebuild).
    show_reasoning: ShowReasoningHandle,
    /// Whether the most recently emitted block was a *tool* panel (call or
    /// result) as opposed to ordinary text/answer. Lets `display_tool_call`
    /// insert a single blank-line divider between consecutive tool calls
    /// without doubling the divider that a preceding text segment already
    /// printed.
    last_was_tool: Mutex<bool>,
    /// The most recent tool call's `(name, args)`, captured in
    /// `display_tool_call` and read back in `display_tool_result` so the result
    /// can be rendered tool-aware (e.g. strip `read_file` line-number gutters
    /// and pick a syntax-highlight language). The hook always fires a call
    /// immediately before its result, so this is safe to read on the result.
    last_tool: Mutex<Option<(String, Value)>>,
    /// Whether the most recent write to the terminal ended with a *blank*
    /// line. Blank-emitting calls (`end_response_segment`, `end_response`,
    /// `flush_held_answer`, `display_text`, `display_markdown`) consult and
    /// update it so two consecutive blank-emitters collapse to a single empty
    /// line instead of stacking. Rig emits a terminal `CompletionCall` item for
    /// the final text-only turn, so `end_response_segment` then `end_response`
    /// would otherwise print two blank lines before the loop's separator; the
    /// error path (`end_response` then `display_text`) would do the same above
    /// the error line.
    last_blank: Mutex<bool>,
    /// Stores the previous writer during a capture session (see
    /// [`ConsoleOutput::begin_capture`] / [`ConsoleOutput::end_capture`]).
    saved_writer: Mutex<Option<Box<dyn io::Write + Send>>>,
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        format!("{}...", s.chars().take(max_len).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Terminal width in columns, or 80 if it cannot be determined.
pub fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

/// Terminal height in rows, or 24 if it cannot be determined. Symmetric to
/// [`term_width`] — callers use it to budget vertical space for pop-up
/// overlays (e.g. reserving room for menu items below a multi-line header).
pub fn term_height() -> usize {
    crossterm::terminal::size()
        .map(|(_, h)| h as usize)
        .unwrap_or(24)
}

/// Render `src` as a styled terminal string via `codewandler-markdown-terminal`.
///
/// Uses GFM (`parse_gfm`) so tables / task-lists / strikethrough render as users
/// expect. `color` selects the default ANSI theme (TTY) vs `no_color` (pipes,
/// logs, CI) so non-terminal output stays clean.
pub(crate) fn render_markdown(src: &str, width: usize, color: bool) -> String {
    // In callers, `color` is `is_terminal()`, so enabling OSC 8 clickable
    // links here gates them to a real TTY (pipes/CI stay escape-free).
    let mut theme = if color { Theme::default() } else { Theme::no_color() };
    theme.clickable_links = color;
    markdown_terminal::render_with(&markdown_stream::parse_gfm(src), &theme, width)
}

/// Wrap `text` to fit `width` characters per line.
///
/// Each logical line (split on `\n`) is wrapped independently; blank lines are
/// preserved. The leading whitespace of each logical line is captured and
/// re-emitted at the start of the first wrapped line *and* every continuation
/// line, so indentation survives a reflow.
///
/// Tokenization inside the body uses `split_whitespace`, collapsing any run of
/// whitespace between words to a single space. A word longer than the available
/// width (after subtracting the indent) is hard-split into `width`-sized
/// character chunks (CJK-safe). The first chunk keeps the indent; subsequent
/// chunks are emitted plain so the overlay renderer can prepend its own marker.
///
/// Unlike a grapheme-boundary splitter, this keeps trailing punctuation
/// attached to its word (e.g. `"Commands:"` stays `"Commands:"`, not
/// `"Commands :"`) and never inserts a space adjacent to existing whitespace
/// between words.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.lines().map(|l| l.to_string()).collect();
    }
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        // Capture leading whitespace so the indent survives wrap.
        let indent: String = logical.chars().take_while(|c| c.is_whitespace()).collect();
        let body = &logical[indent.len()..];
        if body.is_empty() {
            // All-whitespace line: preserve the indent itself.
            out.push(indent);
            continue;
        }
        let indent_chars = indent.chars().count();
        let content_width = width.saturating_sub(indent_chars).max(1);
        let words: Vec<&str> = body.split_whitespace().collect();
        let mut line = String::from(&indent);
        for word in words {
            if word.chars().count() > content_width {
                // Flush any pending line, then hard-split the long word.
                if line.chars().count() > indent_chars {
                    out.push(std::mem::take(&mut line));
                }
                let chunks: Vec<String> = word
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(content_width)
                    .map(|c| c.iter().collect())
                    .collect();
                let last_idx = chunks.len().saturating_sub(1);
                for (i, c) in chunks.iter().enumerate() {
                    if i == 0 {
                        out.push(format!("{}{}", indent, c));
                    } else if i < last_idx {
                        out.push(c.clone());
                    }
                }
                line = chunks.last().cloned().unwrap_or_default();
                continue;
            }
            let content_len = line.chars().count() - indent_chars;
            if content_len == 0 {
                line.push_str(word);
            } else if content_len + 1 + word.chars().count() > content_width {
                out.push(std::mem::take(&mut line));
                line.push_str(&indent);
                line.push_str(word);
            } else {
                line.push(' ');
                line.push_str(word);
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// Like [`wrap`], but recognizes a leading `- ` / `+ ` as a diff marker: the
/// marker is stripped, the body is wrapped (which preserves the body's
/// leading indent), and the marker is re-prepended to the first wrapped
/// line. Continuation lines are prefixed with two spaces so they visually
/// align under the marker column.
///
/// `wrap()` alone would collapse the run of whitespace between the `-` and
/// the original line's first non-whitespace character — for an indented
/// source line such as `    if foo {`, `format!("- {l}")` produces
/// `-     if foo {`, which `split_whitespace` then reduces to `- if foo {`,
/// losing the indentation. Routing diff lines through this helper instead
/// keeps the original indentation visible.
fn wrap_with_diff_markers(text: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        // A diff line is exactly `<marker><space><original line>` with the
        // marker being `-` or `+`. Anything else (e.g. `---` from a header
        // divider or `--foo`) is treated as plain text.
        let is_diff = matches!(logical.as_bytes(), [b'-' | b'+', b' ', ..]);
        if is_diff {
            let marker = &logical[..2];
            let rest = &logical[2..];
            // Reserve two columns for the marker + the space that follows it
            // on the first line. Continuation lines also need two leading
            // spaces to keep their content aligned under the marker column.
            let content_width = width.saturating_sub(2).max(1);
            if rest.is_empty() {
                // Marker with no body (degenerate but possible): emit the
                // marker on its own.
                out.push(marker.to_string());
                continue;
            }
            let wrapped = wrap(rest, content_width);
            for (i, line) in wrapped.into_iter().enumerate() {
                if i == 0 {
                    out.push(format!("{}{}", marker, line));
                } else {
                    out.push(format!("  {}", line));
                }
            }
        } else {
            out.extend(wrap(logical, width));
        }
    }
    out
}

/// Highlight a single physical line of `text` as `lang` using the *same*
/// markdown code-block formatter the answer/result path uses
/// (`markdown_terminal` driven by `markdown_stream` via `render_markdown`).
///
/// Returns **foreground-only** ANSI (no background) so the caller can layer the
/// dark-grey tool panel on top via `paint_panel`. `color` off (non-TTY) returns
/// the line untouched. The renderer's 2-space code indent and trailing newline
/// are stripped, leaving one line of styled text. Each call builds a fresh
/// fenced block, so the formatter is fully isolated/flushed per line.
fn highlight_code_fg(text: &str, lang: &str, color: bool) -> String {
    if !color || text.is_empty() {
        return text.to_string();
    }
    let md = format!("```{lang}\n{text}\n```\n");
    let rendered = render_markdown(&md, term_width(), true);
    let body = rendered
        .strip_prefix("  ")
        .unwrap_or_else(|| rendered.strip_prefix(' ').unwrap_or(&rendered));
    body.trim_end_matches('\n').to_string()
}

/// Language id used to colorize a tool *command* label (e.g. `read …`,
/// `$ ls …`). Any non-empty id engages token highlighting; the highlighter is
/// language-agnostic for tokens, so `bash` is a safe, consistent default.
fn command_lang(_name: &str) -> &'static str {
    "bash"
}

/// Guess a syntax-highlight id from a file path's extension, for coloring
/// `read_file` output. Unknown extensions fall back to `text` (no token
/// highlighting, just the uniform code color).
fn guess_lang(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("ts") => "typescript",
        Some("tsx") => "tsx",
        Some("jsx") => "jsx",
        Some("go") => "go",
        Some("java") => "java",
        Some("c" | "h") => "c",
        Some("cpp" | "cc" | "cxx" | "c++" | "hpp") => "cpp",
        Some("rb") => "ruby",
        Some("sh" | "bash" | "zsh") => "bash",
        Some("json") => "json",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("md" | "markdown") => "markdown",
        Some("html" | "htm") => "html",
        Some("css") => "css",
        Some("sql") => "sql",
        Some("xml") => "xml",
        Some("php") => "php",
        Some("swift") => "swift",
        Some("kt" | "kts") => "kotlin",
        Some("diff") => "diff",
        _ => "text",
    }
}

/// Drop the `NNNN| ` line-number gutter that `read_file` prepends to each line,
/// so the displayed file content is clean (the model still receives the numbers
/// in the raw tool result). Lines without a numeric gutter are returned as-is.
fn strip_read_lineno(line: &str) -> &str {
    if let Some(pos) = line.find('|') {
        let (gutter, rest) = line.split_at(pos);
        let g = gutter.trim();
        if !g.is_empty() && g.chars().all(|c| c.is_ascii_digit()) {
            return rest
                .strip_prefix("| ")
                .unwrap_or_else(|| rest.strip_prefix('|').unwrap_or(rest));
        }
    }
    line
}

/// Build a human-readable one-line *label* for a tool call plus any optional
/// *trailing* lines (e.g. a diff preview for edits).
///
/// The label replaces the old generic first-argument dump with purpose-built
/// summaries:
///   - `read_file`  -> `read <path> [start..end]`   (line range optional)
///   - `bash`       -> the full command line, untruncated
///   - `edit_file`  -> `edit <path> [start..end]`   (+ a best-effort diff)
///   - anything else falls back to `Name <first-arg>` (truncated to 50 cols).
fn get_preview(name: &str, args: &Value) -> (String, Vec<String>) {
    let obj = args.as_object();
    let get_str = |key: &str| obj.and_then(|o| o.get(key)).and_then(|v| v.as_str());
    let get_u64 = |key: &str| obj.and_then(|o| o.get(key)).and_then(|v| v.as_u64());

    match name.to_lowercase().as_str() {
        "read" | "read_file" => {
            let path = get_str("path").unwrap_or("");
            let offset = get_u64("offset").unwrap_or(0);
            let limit = get_u64("limit").unwrap_or(0);
            let mut s = format!("read {}", path);
            // `offset` is 0-based; `limit` is a count. Display is 1-based, so the
            // affected window is [offset+1, offset+limit]. A zero `limit` means
            // "to end of file" -> omit the (optional) range.
            if limit > 0 {
                s.push_str(&format!(" [{}..{}]", offset + 1, offset + limit));
            } else if offset > 0 {
                s.push_str(&format!(" [{}..]", offset + 1));
            }
            (s, Vec::new())
        }
        "bash" => {
            // Show the *entire* command line; never shorten it. Prefix with `$`
            // so it reads like a shell prompt. The canonical field is "command";
            // old sessions may have "cmd".
            let cmd = get_str("command")
                .or_else(|| get_str("cmd"))
                .unwrap_or("")
                .trim();
            (format!("$ {}", cmd), Vec::new())
        }
        s if s.starts_with("grep") => {
            // Grep tool: show pattern (from "pattern" or "pat") and path.
            let pat = get_str("pattern")
                .or_else(|| get_str("pat"))
                .unwrap_or("");
            let path = get_str("path").unwrap_or("");
            (format!("grep {} {}", pat, path), Vec::new())
        }
        s if s == "ls" || s.starts_with("list") => {
            // `ls` (canonical) or `list_dir` / `List*` (old): shorten to `ls`.
            let path = get_str("path").unwrap_or("");
            (format!("ls {}", path), Vec::new())
        }
        "edit" | "edit_file" => {
            let path = get_str("path").unwrap_or("");
            // Try canonical format (edits array) first, fall back to old format.
            let edits: Vec<EditOp> = if let Some(edits_array) =
                obj.and_then(|o| o.get("edits")).and_then(|v| v.as_array())
            {
                edits_array
                    .iter()
                    .filter_map(|e| {
                        Some(EditOp {
                            old_text: e.get("oldText")?.as_str()?.to_string(),
                            new_text: e.get("newText")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            } else {
                vec![]
            };
            let (range, diff) = if !edits.is_empty() {
                edit_preview(path, &edits)
            } else {
                // Old format: old/new fields.
                let old = get_str("old").unwrap_or("");
                let new_str = get_str("new").unwrap_or("");
                edit_preview(path, &[EditOp { old_text: old.to_string(), new_text: new_str.to_string() }])
            };
            let mut s = format!("edit {}", path);
            if let Some(r) = range {
                s.push_str(&format!(" [{}]", r));
            }
            (s, diff)
        }
        _ => {
            let first = obj
                .and_then(|o| o.values().next())
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    _ => v.to_string(),
                })
                .unwrap_or_else(|| args.to_string());
            let cap = name
                .chars()
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + &name[1..])
                .unwrap_or_else(|| name.to_string());
            (format!("{} {}", cap, truncate(&first.replace('\n', " "), 50)), Vec::new())
        }
    }
}

/// Best-effort line range + diff preview for an edit tool call.
///
/// Uses the first edit in the `edits` slice. Locates `old_text` in the current
/// file contents (the edit has not run yet, so the text still matches) to
/// derive the 1-based `[start..end]` line window, and emits a compact `-`/`+`
/// preview of the change. A failed read, missing old_text, or empty edits all
/// degrade gracefully to `(None, [])` so the caller just prints `edit <path>`
/// with no range/diff.
fn edit_preview(path: &str, edits: &[EditOp]) -> (Option<String>, Vec<String>) {
    let first = match edits.first() {
        Some(e) => e,
        None => return (None, Vec::new()),
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return (None, Vec::new());
    };
    let Some(idx) = content.find(&first.old_text) else {
        return (None, Vec::new());
    };
    // 1-based line number of the first line of `old_text`.
    let start = content[..idx].matches('\n').count() + 1;
    let end = start + first.old_text.matches('\n').count();
    let range = if start == end {
        format!("{start}")
    } else {
        format!("{start}..{end}")
    };

    // Compact diff preview: removed lines, then added lines, capped so huge
    // replacements don't flood the scrollback.
    const MAX: usize = 8;
    let old_lines: Vec<&str> = first.old_text.split('\n').collect();
    let new_lines: Vec<&str> = first.new_text.split('\n').collect();
    let clip = |lines: &[&str]| -> Vec<String> {
        if lines.len() > MAX {
            lines[..MAX - 1]
                .iter()
                .map(|l| l.to_string())
                .chain(std::iter::once("…".to_string()))
                .collect()
        } else {
            lines.iter().map(|l| l.to_string()).collect()
        }
    };
    let mut diff = Vec::new();
    for l in clip(&old_lines) {
        diff.push(format!("- {l}"));
    }
    for l in clip(&new_lines) {
        diff.push(format!("+ {l}"));
    }
    (Some(range), diff)
}

impl ConsoleOutput {
    pub fn new(show_reasoning: ShowReasoningHandle) -> Self {
        Self {
            spinner: Mutex::new(None),
            row: Mutex::new(1),
            reader_paused: Arc::new(AtomicBool::new(false)),
            text_buffer: Mutex::new(String::new()),
            live: Mutex::new(false),
            writer: Mutex::new(Box::new(io::stdout())),
            parser: Mutex::new(StreamParser::new_gfm()),
            renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            thinking_parser: Mutex::new(StreamParser::new_gfm()),
            thinking_renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            dim_on: Mutex::new(false),
            answer_held: Mutex::new(false),
            answer_buffer: Mutex::new(String::new()),
            show_reasoning,
            last_was_tool: Mutex::new(false),
            last_tool: Mutex::new(None),
            last_blank: Mutex::new(false),
            saved_writer: Mutex::new(None),
        }
    }

    /// Toggle the global dim SGR on stdout to match `on`, emitting the escape
    /// only when the state actually changes. Used so streamed thinking reads
    /// faded without re-rendering any already-emitted content.
    fn ensure_dim(&self, on: bool, out: &mut dyn io::Write) {
        let mut dim = self.dim_on.lock().unwrap();
        if *dim != on {
            let _ = if on {
                out.write_all(b"\x1b[2m")
            } else {
                out.write_all(b"\x1b[0m")
            };
            *dim = on;
        }
    }

    /// Render and clear the buffered assistant text (non-TTY / pipe path:
    /// `live` is false so the TTY live renderer isn't used; this renders once
    /// via `display_markdown`, matching the Phase 1/6 clean-output behavior).
    fn flush_text(&self) {
        let text = std::mem::take(&mut *self.text_buffer.lock().unwrap());
        if !text.is_empty() {
            self.display_markdown(&text);
        }
    }

    /// Shared body of `begin_response`. Takes the live/TTY decision explicitly
    /// so the non-TTY (buffered) path can be forced in tests without depending
    /// on whether the test process's stdout is attached to a terminal.
    fn begin_response_inner(&self, live: bool) {
        *self.live.lock().unwrap() = live;
        // Reset per-response answer-holding state. `set_answer_hold` is called
        // afterwards (from the agent) to re-enable it for reasoning turns.
        *self.answer_held.lock().unwrap() = false;
        *self.answer_buffer.lock().unwrap() = String::new();
        if live {
            // Reset the incremental parser/renderer for a fresh response and
            // print a leading blank to separate from the user prompt.
            let mut parser = self.parser.lock().unwrap();
            *parser = StreamParser::new_gfm();
            let mut theme = Theme::default();
            theme.clickable_links = true; // real TTY path
            let mut renderer = self.renderer.lock().unwrap();
            *renderer = Renderer::new(theme, term_width());
            // Reset the thinking parser/renderer and dim state for this response.
            let mut tparser = self.thinking_parser.lock().unwrap();
            *tparser = StreamParser::new_gfm();
            let mut trenderer = self.thinking_renderer.lock().unwrap();
            *trenderer = Renderer::new(Theme::dimmed(), term_width());
            *self.dim_on.lock().unwrap() = false;
            let mut w = self.writer.lock().unwrap();
            let _ = writeln!(w);
            drop(w);
            self.note_lines(1);
            self.set_blank(true);
            self.note_text();
        } else {
            *self.text_buffer.lock().unwrap() = String::new();
        }
    }

    /// Swap the output writer for a `Cursor<Vec<u8>>` capture buffer.
    /// Call [`end_capture`] to restore the original writer and retrieve
    /// the captured bytes.
    fn begin_capture_inner(&self) {
        let mut writer = self.writer.lock().unwrap();
        let old = std::mem::replace(&mut *writer, Box::new(std::io::Cursor::new(Vec::new())));
        *self.saved_writer.lock().unwrap() = Some(old);
    }

    /// End a capture session: restore the original writer and return the
    /// captured bytes.
    fn end_capture_inner(&self) -> Vec<u8> {
        let mut writer = self.writer.lock().unwrap();
        let saved = self.saved_writer.lock().unwrap().take();
        let old = saved.expect("end_capture without begin_capture");
        let current = std::mem::replace(&mut *writer, old);
        // Safety: we know current is a Box<Cursor<Vec<u8>>> because
        // begin_capture_inner always creates one.
        let raw = Box::into_raw(current);
        let cursor = unsafe { Box::from_raw(raw as *mut std::io::Cursor<Vec<u8>>) };
        cursor.into_inner()
    }
}

impl Default for ConsoleOutput {
    fn default() -> Self {
        Self::new(crate::settings::default_show_reasoning_handle())
    }
}

/// Remove ANSI/CSI escape sequences (e.g. `\x1b[1;36m`) so that
/// downstream width/wrap math only counts *visible* characters.
fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // ESC [ ... <final>  (CSI)  or  ESC ] ... BEL/ST  (OSC): skip to terminator.
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() { i += 1; } // consume final byte
                continue;
            }
            if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                i += 2;
                while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                    i += 1;
                }
                if i < bytes.len() { i += 1; } // consume BEL or ST
                continue;
            }
        }
        // bytes[i] is either ASCII or a UTF-8 lead byte; copy the
        // whole char to preserve multi-byte content.
        let ch_len = utf8_len(bytes[i]);
        if i + ch_len <= bytes.len() {
            out.push_str(std::str::from_utf8(&bytes[i..i + ch_len]).unwrap_or("\u{fffd}"));
        }
        i += ch_len.max(1);
    }
    out
}

/// UTF-8 lead byte -> byte length (1..=4).
fn utf8_len(b: u8) -> usize {
    if b < 0x80 { 1 } else if b >> 5 == 0b110 { 2 } else if b >> 4 == 0b1110 { 3 } else if b >> 3 == 0b11110 { 4 } else { 1 }
}

/// Non-blocking Device Status Report: ask the terminal for the real
/// cursor row, timing out fast so we never hang on a terminal that
/// does not answer (e.g. a headless test pty). Returns a 1-indexed
/// row. Reads the reply raw (bypassing crossterm's event parser).
fn query_real_cursor_row() -> Option<usize> {
    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    // Make stdin non-blocking so a missing DSR reply cannot hang us.
    let old = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if old < 0 {
        return None;
    }
    unsafe { libc::fcntl(fd, libc::F_SETFL, old | libc::O_NONBLOCK) };
    let restore = |f: i32, o: i32| unsafe { libc::fcntl(f, libc::F_SETFL, o); };

    let mut out = io::stdout();
    if out.write_all(b"\x1b[6n").is_err() || out.flush().is_err() {
        restore(fd, old);
        return None;
    }

    let mut guard = stdin.lock();
    let start = Instant::now();
    let mut buf = Vec::new();
    let mut got = false;
    while start.elapsed() < Duration::from_millis(60) {
        let mut b = [0u8; 1];
        match guard.read(&mut b) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(b[0]);
                if b[0] == b'R' {
                    got = true;
                    break;
                }
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
    restore(fd, old);
    if got {
        parse_dsr(&buf)
    } else {
        None
    }
}

/// Parse a Device Status Report reply `ESC[<row>;<col>R` into the 1-indexed row.
fn parse_dsr(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4
        || buf[0] != 0x1b
        || buf[1] != b'['
        || *buf.last()? != b'R'
    {
        return None;
    }
    let inner = &buf[2..buf.len() - 1];
    let mut it = inner.splitn(2, |&c| c == b';');
    let row = it.next()?;
    let row_s = std::str::from_utf8(row).ok()?;
    row_s.parse::<usize>().ok()
}

impl ConsoleOutput {
    /// Advance the tracked cursor by `n` physical lines. The real terminal
    /// scrolls and keeps the cursor on the bottom row when content overflows,
    /// so we *clamp* to the height rather than wrapping (a modulo wrap would
    /// teleport the tracked row to the top of the screen after a long session).
    fn note_lines(&self, n: usize) {
        let (_, h) = crossterm::terminal::size().unwrap_or((80, 24));
        let h = h.max(1) as usize;
        let mut r = self.row.lock().unwrap();
        *r = (*r + n - 1).min(h - 1) + 1;
    }

    /// Commit all buffered *thinking* to the (dimmed) thinking renderer and
    /// start a fresh thinking parser.
    ///
    /// Uses `StreamParser::flush()` — **not** `write(b"\n\n")` — so
    /// lookahead-dependent blocks (bullet/numbered lists, tables, fenced code)
    /// are emitted *now*. A blank line cannot close a GFM list (the parser must
    /// see a non-list line or EOF), so `write(b"\n\n")` left such reasoning
    /// stranded in the parser until `end_response`'s `flush()` — which runs
    /// *after* the answer parser, so the reasoning printed *below* the answer.
    /// The thinking *renderer* is kept so post-tool thinking reads as one
    /// continuous faded stream. See `doc/reasoning_order_opus.md` §4.
    fn commit_thinking<W: io::Write>(&self, out: &mut W) {
        let events = {
            let mut tparser = self.thinking_parser.lock().unwrap();
            let events = tparser.flush();
            *tparser = StreamParser::new_gfm();
            events
        };
        let mut trenderer = self.thinking_renderer.lock().unwrap();
        let _ = trenderer.feed(&events, out);
        if !events.is_empty() {
            // Committed thinking is content, not a blank separator.
            self.set_blank(false);
        }
    }

    /// Render any answer text held back while reasoning was streaming, after
    /// committing the thinking paragraph so the layout reads `[thinking][answer]`.
    /// Renders the answer through a *local* parser so `self.parser` stays empty
    /// and the caller's own per-segment answer-commit (a no-op on an empty
    /// parser) does not re-render it. Returns true when a held answer was
    /// flushed on the live path.
    fn flush_held_answer(&self) -> bool {
        let live = *self.live.lock().unwrap();
        if !live {
            return false;
        }
        let buffer = std::mem::take(&mut *self.answer_buffer.lock().unwrap());
        if buffer.is_empty() {
            return false;
        }
        let mut out = self.writer.lock().unwrap();
        // Commit the (dimmed) thinking paragraph first, mirroring the
        // thinking→answer boundary handling in `stream_text`.
        let was_thinking = *self.dim_on.lock().unwrap();
        if was_thinking {
            self.commit_thinking(&mut *out);
        }
        // Turn off dimming and emit one blank line so the answer starts on a
        // fresh paragraph. The blank line MUST come *after* `ensure_dim(false)`
        // (the dim-reset SGR): a GFM list/table emits no trailing newline of its
        // own, so a `\n` placed before the reset would be swallowed as the
        // block's closing newline instead of a blank separator. Ordering this
        // exactly like `stream_text`'s transition is what guarantees the gap.
        self.ensure_dim(false, &mut *out);
        // Blank line between reasoning and answer, written to `out` (the same
        // handle the reasoning/answer use) so it lands in the right place.
        // Skipped when the previous write already ended with a blank (a prior
        // segment or response boundary), so the gap stays a single empty line.
        if !self.ended_blank() {
            let _ = out.write_all(b"\n");
            self.note_lines(1);
            self.set_blank(true);
        }
        // Render the answer with a local parser so `self.parser` is left empty
        // for the caller's renderer reset. `flush()` (not just `write`) so the
        // answer's final, blank-line-unterminated paragraph is emitted rather
        // than dropped (a `write`-only parser holds the last block back).
        let mut local_parser = StreamParser::new_gfm();
        let mut events = local_parser.write(buffer.as_bytes());
        events.extend(local_parser.flush());
        let mut renderer = self.renderer.lock().unwrap();
        let _ = renderer.feed(&events, &mut *out);
        self.set_blank(false);
        let _ = writeln!(out);
        self.note_lines(1);
        self.set_blank(true);
        self.note_text();
        drop(out);
        true
    }

    fn separator(&self) {
        let width = term_width();
        let mut out = self.writer.lock().unwrap();
        let _ = writeln!(out, "{}", style("─".repeat(width)).dim());
        drop(out);
        // The rule is content, not a blank separator.
        self.set_blank(false);
    }

    /// Record that the most recent output block was ordinary text/answer (not a
    /// tool panel), so the next tool call will not insert a redundant blank-line
    /// divider — a text segment already emits its own leading blank line.
    fn note_text(&self) {
        *self.last_was_tool.lock().unwrap() = false;
    }

    /// Record that the most recent output block was a tool panel.
    fn note_tool(&self) {
        *self.last_was_tool.lock().unwrap() = true;
    }

    /// Whether the last write to the terminal ended with a blank line. Used by
    /// blank-emitting methods to collapse consecutive blanks to one.
    fn ended_blank(&self) -> bool {
        *self.last_blank.lock().unwrap()
    }

    /// Record whether the last write to the terminal ended with a blank line.
    /// Content lines (text, tool panels, the separator rule) set `false`;
    /// deliberately-emitted empty lines set `true`.
    fn set_blank(&self, blank: bool) {
        *self.last_blank.lock().unwrap() = blank;
    }

    /// Paint the dark-grey tool-panel background behind `content`, which already
    /// carries any *foreground* ANSI it needs — dim text, red/green diff
    /// markers, or syntax-highlight tokens from `highlight_code_fg`. The
    /// background is re-applied after every internal `\x1b[0m` the highlighter
    /// emitted, so the grey stays solid across a whole line even when the
    /// content changes foreground color several times. `indent` leaves one
    /// leading column so the line reads as a command's *output*; padding uses
    /// display-column width so wide/CJK characters keep the panel edge aligned.
    fn paint_panel(&self, content: &str, indent: bool) {
        const BG: &str = "\x1b[48;2;22;22;26m";
        const RESET: &str = "\x1b[0m";
        // Prepend the background and re-apply it after each reset, so a token's
        // own `\x1b[0m` cannot drop the grey behind the rest of the line.
        let painted = format!("{BG}{}", content.replace(RESET, &format!("{RESET}{BG}")));
        let vis = UnicodeWidthStr::width(strip_ansi(&painted).as_str());
        let width = term_width();
        let pad = if width == 0 {
            0
        } else if indent {
            width.saturating_sub(vis + 1)
        } else {
            width.saturating_sub(vis)
        };
        let row = format!(
            "{}{}{}{}",
            if indent { format!("{BG} ") } else { String::new() },
            painted,
            format!("{BG}"),
            format!("{}{}", " ".repeat(pad), RESET),
        );
        let mut out = self.writer.lock().unwrap();
        let _ = writeln!(out, "{}", row);
        drop(out);
        self.note_lines(1);
        self.note_tool();
        // A panel row is content, not a blank separator.
        self.set_blank(false);
    }

    /// Render one full-width dark-grey panel row for plain `text` (no ANSI).
    /// `text` is wrapped to the available width and painted dim (or red for
    /// errors); `-`/`+` diff markers keep their red/green. For pre-styled /
    /// syntax-highlighted content, call `paint_panel` directly.
    fn tool_panel(&self, text: &str, is_error: bool, indent: bool) {
        let width = term_width();
        // When indented, leave one column for the indent; keep the background
        // full-width in both cases.
        let inner = if indent {
            width.saturating_sub(1).max(1)
        } else {
            width.max(1)
        };
        let lines = if text.is_empty() {
            vec![String::new()]
        } else {
            wrap_with_diff_markers(text, inner)
        };
        for line in lines {
            // Foreground-only styling here; `paint_panel` adds the grey
            // background (and re-applies it across any internal resets).
            let content = if is_error {
                style(line.clone()).red().to_string()
            } else if let Some(rest) = line.strip_prefix("- ") {
                format!("{}{}", style("-").red(), style(format!(" {rest}")).dim())
            } else if let Some(rest) = line.strip_prefix("+ ") {
                format!("{}{}", style("+").green(), style(format!(" {rest}")).dim())
            } else {
                style(line.clone()).dim().to_string()
            };
            self.paint_panel(&content, indent);
        }
    }

    fn create_spinner(message: &str) -> ProgressBar {
        let pb = ProgressBar::new_spinner();
        pb.enable_steady_tick(Duration::from_millis(120));
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.blue} {msg}")
                .unwrap(),
        );
        pb.set_message(message.to_string());
        pb
    }
}

impl Output for ConsoleOutput {
    fn cursor_row(&self) -> usize {
        // Prefer the real terminal cursor position (robust to wrapping,
        // ANSI escapes, and termimad-rendered responses). Fall back
        // to the tracked/ANSI-stripped prediction if the terminal
        // does not answer the Device Status Report (e.g. a headless pty).
        match query_real_cursor_row() {
            Some(r) => {
                *self.row.lock().unwrap() = r;
                r
            }
            None => *self.row.lock().unwrap(),
        }
    }

    fn display_plain(&self, text: &str) {
        self.stop_thinking();
        let w = term_width();
        let n = if text.is_empty() {
            1
        } else {
            wrap(&strip_ansi(text), w).len().max(1)
        };
        let mut out = self.writer.lock().unwrap();
        let _ = writeln!(out, "{}", text);
        drop(out);
        self.note_lines(n);
        // Plain text is content, not a blank separator.
        self.set_blank(false);
    }

    fn display_markdown(&self, text: &str) {
        self.stop_thinking();
        let color = std::io::stdout().is_terminal();
        let rendered = render_markdown(text, term_width(), color);
        let mut out = self.writer.lock().unwrap();
        // Leading blank separates the block from the content above; skipped when
        // the previous write already ended with a blank line so consecutive
        // blocks (e.g. an error right after `end_response`) stay one line apart.
        let skipped_blank = self.ended_blank();
        if !skipped_blank {
            let _ = writeln!(out);
            self.set_blank(true);
        }
        let _ = write!(out, "{}", rendered);
        let _ = writeln!(out);
        drop(out);
        // +2 for the leading/trailing blank lines (minus the skipped leading
        // blank); the DSR query corrects drift.
        let n = rendered.lines().count().max(1) + 2 - usize::from(skipped_blank);
        self.note_lines(n);
        // Rendered markdown is content, not a blank separator.
        self.set_blank(false);
    }

    fn display_text(&self, text: &str) {
        self.stop_thinking();
        let mut out = self.writer.lock().unwrap();
        // Leading blank separates the message from the content above; skipped
        // when the previous write already ended with a blank line (e.g. the
        // error path, where `end_response` printed its trailing blank first), so
        // the message sits exactly one empty line below the prior block.
        let skipped_blank = self.ended_blank();
        if !skipped_blank {
            let _ = writeln!(out);
            self.set_blank(true);
        }
        let _ = write!(out, "{} ", style("⏺").cyan());
        let _ = write!(out, "{}", termimad::inline(text));
        let _ = writeln!(out);
        drop(out);
        let n = wrap(text, term_width()).len() + usize::from(!skipped_blank);
        self.note_lines(n);
        // The message line is content, not a blank separator.
        self.set_blank(false);
    }

    fn begin_response(&self) {
        self.begin_response_inner(std::io::stdout().is_terminal());
    }

    fn set_answer_hold(&self, hold: bool) {
        *self.answer_held.lock().unwrap() = hold;
    }

    fn stream_text(&self, delta: &str) {
        let live = *self.live.lock().unwrap();
        if !live {
            self.text_buffer.lock().unwrap().push_str(delta);
            return;
        }
        // While a turn is answer-held (reasoning-capable), buffer the answer so
        // it can be flushed *below* any reasoning that streams after it. The
        // buffer is emitted at the turn boundary (see `flush_held_answer`).
        if *self.answer_held.lock().unwrap() {
            self.answer_buffer.lock().unwrap().push_str(delta);
            return;
        }
        // Coarse-grained live rendering: feed the delta to the incremental
        // parser and emit only the *completed* blocks. Each block prints once
        // (code lines line-by-line, paragraphs on close), so there is no whole
        // document re-render, no in-place cursor gymnastics, and scrollback
        // stays clean.
        let mut out = self.writer.lock().unwrap();
        let was_thinking = *self.dim_on.lock().unwrap();
        if was_thinking {
            // Thinking just ended: normalize the thinking→answer boundary to a
            // single paragraph gap. Commit all still-buffered thinking (while dim
            // is still on) and emit one blank line, so the answer starts on a
            // fresh paragraph regardless of whether a tool call intervened in
            // between (the tool path already prints its own gap).
            self.commit_thinking(&mut *out);
            self.ensure_dim(false, &mut *out);
            // Blank line between reasoning and answer, written to `out` so it
            // lands in the right place (a `println!` here can land in a
            // different write buffer and be lost).
            if !self.ended_blank() {
                let _ = out.write_all(b"\n");
                self.note_lines(1);
                self.set_blank(true);
            }
            self.note_text();
        } else {
            self.ensure_dim(false, &mut *out);
        }
        let mut parser = self.parser.lock().unwrap();
        let events = parser.write(delta.as_bytes());
        let mut renderer = self.renderer.lock().unwrap();
        let _ = renderer.feed(&events, &mut *out);
        drop(out);
        // Emitted text is content, not a blank separator.
        if !events.is_empty() {
            self.set_blank(false);
        }
    }

    fn stream_thinking(&self, delta: &str) {
        // Content mode => stream the reasoning text; otherwise suppress it.
        if *self.show_reasoning.lock().unwrap() != ShowReasoning::Content {
            return;
        }
        let live = *self.live.lock().unwrap();
        if !live {
            // Non-TTY: stream thinking to stderr so consumers can drop it
            // (`2>/dev/null`) or merge it (`2>&1`) without touching stdout.
            let mut err = std::io::stderr();
            let _ = err.write_all(delta.as_bytes());
            return;
        }
        // Always-dim inline: render thinking through its own incremental
        // renderer using a faded theme whose `reset` re-applies the dim SGR,
        // so inline formatting (bold/code) does not clear the fading. The
        // initial dim is turned on once per run via `ensure_dim`.
        let mut out = self.writer.lock().unwrap();
        self.ensure_dim(true, &mut *out);
        let mut parser = self.thinking_parser.lock().unwrap();
        let events = parser.write(delta.as_bytes());
        let mut renderer = self.thinking_renderer.lock().unwrap();
        let _ = renderer.feed(&events, &mut *out);
        drop(out);
        // Streamed thinking is content, not a blank separator.
        if !events.is_empty() {
            self.set_blank(false);
        }
    }

    fn end_response_segment(&self) {
        self.stop_thinking();
        let live = *self.live.lock().unwrap();
        if live {
            // Call flush_held_answer BEFORE locking the writer (it locks it
            // internally).
            let flushed = self.flush_held_answer();
            let mut out = self.writer.lock().unwrap();
            // Commit any open answer paragraph so the tool call/result prints
            // below, then start a *fresh* answer renderer for the next segment.
            // This drops the renderer's `pending_gap`, which otherwise fires at
            // the next answer segment's first block and would double the blank
            // line at the thinking→answer boundary in multi-turn turns (the
            // transition already emits one blank).
            {
                let mut parser = self.parser.lock().unwrap();
                // `flush()` (not `write("\n\n")`) so a segment whose answer text
                // ends in a list/table is emitted now rather than stranded; then
                // start a fresh answer parser + renderer for the next segment.
                let events = parser.flush();
                *parser = StreamParser::new_gfm();
                let mut renderer = self.renderer.lock().unwrap();
                let _ = renderer.feed(&events, &mut *out);
                if !events.is_empty() {
                    // Committed answer text is content, not a blank separator.
                    self.set_blank(false);
                }
                let mut theme = Theme::default();
                theme.clickable_links = true; // real TTY path
                *renderer = Renderer::new(theme, term_width());
            }
            if !flushed {
                // Commit thinking (the thinking renderer is kept — post-tool
                // thinking continues as one continuous faded stream; only the
                // parser is reset, inside `commit_thinking`).
                self.commit_thinking(&mut *out);
                self.ensure_dim(false, &mut *out);
                // Blank line between the answer/reasoning block and the tool
                // panel below. Skipped when the stream already ends with a blank
                // (e.g. a held answer was just flushed, or an empty terminal
                // `CompletionCall` follows a prior segment's blank), so stacked
                // segment boundaries collapse to one empty line.
                if !self.ended_blank() {
                    let _ = out.write_all(b"\n");
                    self.note_lines(1);
                    self.set_blank(true);
                }
                self.note_text();
            }
            drop(out);
        } else {
            self.flush_text();
        }
    }

    fn end_response(&self) {
        self.stop_thinking();
        let live = *self.live.lock().unwrap();
        if live {
            // Call flush_held_answer BEFORE locking the writer (it locks it
            // internally).
            let flushed = self.flush_held_answer();
            let mut out = self.writer.lock().unwrap();
            if !flushed {
                // Finalize the answer document.
                {
                    let mut parser = self.parser.lock().unwrap();
                    let events = parser.flush();
                    let mut renderer = self.renderer.lock().unwrap();
                    let _ = renderer.feed(&events, &mut *out);
                    if !events.is_empty() {
                        // Flushed answer text is content, not a blank separator.
                        self.set_blank(false);
                    }
                }
                // Finalize thinking (it was rendered inline + dimmed as it streamed).
                {
                    let mut parser = self.thinking_parser.lock().unwrap();
                    let events = parser.flush();
                    let mut renderer = self.thinking_renderer.lock().unwrap();
                    let _ = renderer.feed(&events, &mut *out);
                    if !events.is_empty() {
                        // Flushed thinking is content, not a blank separator.
                        self.set_blank(false);
                    }
                }
                self.ensure_dim(false, &mut *out);
                // Trailing blank so the separator that opens the next loop
                // iteration does not butt against the last line of the response.
                // Skipped when the turn already ended with a blank (the terminal
                // `CompletionCall`'s `end_response_segment` — or a held-answer
                // flush — printed one), which is what collapses the previously
                // observed double empty line to a single one.
                if !self.ended_blank() {
                    let _ = writeln!(out);
                    self.note_lines(1);
                    self.set_blank(true);
                }
                self.note_text();
            }
            drop(out);
        } else {
            self.flush_text();
            // Non-TTY thinking already streamed to stderr; stdout stays clean.
        }
    }

    fn display_tool_call(&self, name: &str, args: &Value) {
        self.stop_thinking();
        // Purpose-built label (+ optional trailing lines such as an edit diff)
        // from `get_preview`. A preceding *text* segment already printed its
        // own leading blank line, so we only add a blank divider when the
        // previous block was also a tool (call or result) — that separates
        // consecutive tool calls without doubling the segment's divider.
        let (label, extra) = get_preview(name, args);
        // Remember the tool so `display_tool_result` can render the result
        // tool-aware (strip `read_file` line-number gutters, pick a highlight
        // language). The hook always fires the call immediately before its
        // result, so this is safe to read back on the result.
        *self.last_tool.lock().unwrap() = Some((name.to_string(), args.clone()));
        if *self.last_was_tool.lock().unwrap() {
            let mut out = self.writer.lock().unwrap();
            let _ = writeln!(out);
            drop(out);
            self.note_lines(1);
            // The divider is a blank line; the panel content below resets it.
            self.set_blank(true);
        }
        // Dark-grey panel box (no `⏺`, no tree line-art). The command label is
        // the first line and is left flush; its output — the trailing
        // diff/preview lines — is indented one column so the command's output
        // reads as a distinct block. The command itself is syntax-highlighted
        // through the same formatter the answer/result path uses (flushed per
        // block via a fresh fenced code render).
        let color = std::io::stdout().is_terminal();
        let label_styled = highlight_code_fg(&label, command_lang(name), color);
        self.paint_panel(&label_styled, false);
        for line in &extra {
            self.tool_panel(line, false, true);
        }
    }

    fn display_tool_result(&self, result: &str) {
        self.stop_thinking();

        // Edit-tool failures are emitted as a structured JSON object with
        // separate `ui_view` (short, no file text) and `model_view` (rich,
        // with diff). The UI must render only the short view so we don't
        // leak file content into scrollback; the full result string is
        // still passed back to the model via the tool-result message.
        if let Ok(v) = serde_json::from_str::<Value>(result) {
            if let (Some(ui), Some(_model)) = (
                v.get("ui_view").and_then(|x| x.as_str()),
                v.get("model_view").and_then(|x| x.as_str()),
            ) {
                self.tool_panel(ui, true, true);
                return;
            }
        }

        let unquoted = serde_json::from_str::<Value>(result)
            .ok()
            .and_then(|v| match v {
                Value::String(s) => Some(s),
                Value::Array(arr) => Some(
                    arr.iter()
                        .map(|v| v.as_str().unwrap_or(&v.to_string()).to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .unwrap_or_else(|| result.to_string());

        let mut cleaned = unquoted.as_str();
        while cleaned.starts_with("Toolset error: ") || cleaned.starts_with("ToolCallError: ") {
            if let Some(stripped) = cleaned.strip_prefix("Toolset error: ") {
                cleaned = stripped;
            } else if let Some(stripped) = cleaned.strip_prefix("ToolCallError: ") {
                cleaned = stripped;
            }
        }

        let is_error =
            unquoted.starts_with("Toolset error") || unquoted.starts_with("ToolCallError");
        let lines: Vec<_> = cleaned.lines().collect();

        if lines.is_empty() {
            self.tool_panel("(empty)", false, true);
            return;
        }

        // Which tool produced this result (set by `display_tool_call`). Drives
        // read-specific line-number stripping and language guessing.
        let tool = self.last_tool.lock().unwrap().clone();
        let is_read = tool
            .as_ref()
            .map(|(n, _)| n.eq_ignore_ascii_case("read") || n.eq_ignore_ascii_case("read_file"))
            .unwrap_or(false);
        let read_path = tool
            .as_ref()
            .and_then(|(_, a)| a.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let color = std::io::stdout().is_terminal();
        // Error output is shown verbatim (red). Everything else is highlighted:
        // `read_file` output is colored by the file's guessed language, bash
        // command output gets token highlighting, and other tools' output is
        // colored uniformly (no meaningful grammar to tokenize).
        let lang = if is_read {
            guess_lang(read_path)
        } else {
            match tool.as_ref().map(|(n, _)| n.as_str()) {
                Some("bash") => "bash",
                _ => "text",
            }
        };

        // Read results drop the `NNNN| ` line-number gutter and show up to 10
        // lines; other results keep the previous 4-line default. Errors show
        // everything.
        let show_max = if is_error {
            usize::MAX
        } else if is_read {
            10
        } else {
            4
        };

        let width = term_width();
        let inner = width.saturating_sub(1).max(1);
        for (printed, line) in lines.iter().enumerate() {
            if printed >= show_max {
                break;
            }
            // `read_file` renders `NNNN| content`; drop the gutter for display
            // (the model still gets the numbers in the raw tool result).
            let visible = if is_read { strip_read_lineno(line) } else { line };
            for wline in wrap(visible, inner) {
                if is_error {
                    self.tool_panel(&wline, true, true);
                } else {
                    // Syntax-highlight through the result-path formatter, then
                    // layer the grey tool panel on top (flushed per line).
                    let styled = highlight_code_fg(&wline, lang, color);
                    self.paint_panel(&styled, true);
                }
            }
        }

        if lines.len() > show_max && !is_error {
            self.tool_panel(
                &format!("... +{} lines", lines.len() - show_max),
                false,
                true,
            );
        }
    }

    fn get_user_input(&self) -> String {
        self.stop_thinking();
        use std::io::{self, Write as _};
        {
            let mut out = self.writer.lock().unwrap();
            let _ = write!(out, "{} ", style("❯").bold().blue());
            let _ = out.flush();
            // The prompt is content, not a blank separator.
            self.set_blank(false);
        }
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        input.trim().to_string()
    }

    fn confirm_gate(&self) -> Option<Arc<AtomicBool>> {
        Some(self.reader_paused.clone())
    }

    fn display_error(&self, error: &str) {
        self.stop_thinking();
        let mut out = self.writer.lock().unwrap();
        let _ = writeln!(out, "{} Error: {}", style("⏺").red(), error);
        drop(out);
        self.note_lines(1);
    }

    fn display_system(&self, text: &str) {
        self.stop_thinking();
        let mut out = self.writer.lock().unwrap();
        let _ = writeln!(out, "{}", style(text).bold().dim());
        drop(out);
        self.note_lines(1);
    }

    fn confirm(&self, message: &str) -> Confirmation {
        self.stop_thinking();
        // The REPL runs in raw mode while a request is in flight and a
        // background reader thread polls stdin (Esc/Ctrl-C/Ctrl-Z). Two things
        // go wrong if we read with `std::io::read_line` here:
        //   1. `cfmakeraw` disables ICRNL, so the Enter key delivers `\r`
        //      (0x0D) and never a `\n` (0x0A); `read_line` waits forever for a
        //      newline that never arrives -> the client hangs with no echoed
        //      input (verified: after `enable_raw_mode`, ICRNL==false).
        //   2. The background reader also reads stdin and would consume the
        //      keystrokes meant for this dialog.
        // So (1) ask the reader to stand down, and (2) render the choice as an
        // overlay (consistent with the `/` dialogs) and read a single
        // navigation/selection key via crossterm's raw-mode event reader.
        self.reader_paused.store(true, Ordering::SeqCst);

        let (_w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        let h = h.max(1) as usize;
        // Anchor just below the current cursor. Clamp so this small overlay
        // never scrolls the screen (which would make it impossible to clear
        // precisely afterwards).
        let start_row = self.cursor_row().min(h);
        let n = 4usize; // title + 3 options
        let top = start_row.min(h.saturating_sub(n));

        // Options in display order. Default selection = No (safe: Enter -> No,
        // matching the previous `read_line("")` default).
        let options = vec![
            "Yes".to_string(),
            "No".to_string(),
            "Always (session)".to_string(),
        ];
        let mut sel: usize = 1;

        let draw = |sel: usize| {
            let panel = Repl::overlay_lines(message, &options, None, sel, None, true);
            let mut s = String::new();
            s.push_str(&format!("\x1b[{};1H", top));
            s.push_str("\x1b[0J");
            for line in panel.lines() {
                s.push_str(line);
                s.push('\n');
            }
            let _ = io::stdout().write_all(s.as_bytes());
            let _ = io::stdout().flush();
        };
        draw(sel);

        // Navigate with up/down, confirm with Enter, or use quick keys.
        let choice = loop {
            match event::read() {
                Ok(Event::Key(key)) => match key.code {
                    KeyCode::Up => {
                        if sel > 0 {
                            sel -= 1;
                            draw(sel);
                        }
                    }
                    KeyCode::Down => {
                        if sel + 1 < options.len() {
                            sel += 1;
                            draw(sel);
                        }
                    }
                    KeyCode::Char(c) => {
                        break match c.to_ascii_lowercase() {
                            'y' => 0,
                            's' => 2,
                            _ => 1, // n / anything else -> No
                        };
                    }
                    KeyCode::Enter => break sel,
                    KeyCode::Esc => break 1, // No
                    _ => {}
                },
                _ => {}
            }
        };

        // Clear the overlay region.
        let mut s = String::new();
        s.push_str(&format!("\x1b[{};1H", top));
        s.push_str("\x1b[0J");
        let _ = io::stdout().write_all(s.as_bytes());
        let _ = io::stdout().flush();
        self.reader_paused.store(false, Ordering::SeqCst);

        // Echo the resolved choice (raw mode has no terminal echo).
        println!("  {}", options[choice]);
        self.note_lines(1);

        match choice {
            0 => Confirmation::Yes,
            2 => Confirmation::Always,
            _ => Confirmation::No,
        }
    }

    fn confirm_compaction_summary(&self, message: &str) -> bool {
        self.stop_thinking();
        // The reader (if any) must not swallow the dialog keystrokes; the
        // accept/cancel dialog is owned by the main task, mirroring the
        // tool-confirmation gate.
        self.reader_paused.store(true, Ordering::SeqCst);

        let (_w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        let h = h.max(1) as usize;
        let start_row = self.cursor_row().min(h);
        let n = 3usize; // title + 2 options
        let top = start_row.min(h.saturating_sub(n));

        // Default selection = Accept (safe for an explicit review step the
        // user reached by editing the summary and pressing a key).
        let options = vec![
            "Accept".to_string(),
            "Cancel".to_string(),
        ];
        let mut sel: usize = 0;

        let draw = |sel: usize| {
            let panel = Repl::overlay_lines(message, &options, None, sel, None, true);
            let mut s = String::new();
            s.push_str(&format!("\x1b[{};1H", top));
            s.push_str("\x1b[0J");
            for line in panel.lines() {
                s.push_str(line);
                s.push('\n');
            }
            let _ = io::stdout().write_all(s.as_bytes());
            let _ = io::stdout().flush();
        };
        draw(sel);

        let choice = loop {
            match event::read() {
                Ok(Event::Key(key)) => match key.code {
                    KeyCode::Up => {
                        if sel > 0 {
                            sel -= 1;
                            draw(sel);
                        }
                    }
                    KeyCode::Down => {
                        if sel + 1 < options.len() {
                            sel += 1;
                            draw(sel);
                        }
                    }
                    KeyCode::Char(c) => {
                        break match c.to_ascii_lowercase() {
                            'a' => 0,
                            'c' => 1,
                            _ => 0, // anything else -> Accept (default)
                        };
                    }
                    KeyCode::Enter => break sel,
                    KeyCode::Esc => break 1, // Cancel
                    _ => {}
                },
                _ => {}
            }
        };

        // Clear the overlay region.
        let mut s = String::new();
        s.push_str(&format!("\x1b[{};1H", top));
        s.push_str("\x1b[0J");
        let _ = io::stdout().write_all(s.as_bytes());
        let _ = io::stdout().flush();
        self.reader_paused.store(false, Ordering::SeqCst);

        // Echo the resolved choice (raw mode has no terminal echo).
        // Only echo on Accept; on Cancel / Esc print nothing so the
        // return to the editor is clean (no extra line before the
        // next prompt).
        if choice == 0 {
            println!("  {}", options[choice]);
            self.note_lines(1);
        }

        choice == 0
    }

    fn display_separator(&self) {
        self.stop_thinking();
        self.separator();
        self.note_lines(1);
    }

    fn display_thinking(&self, message: &str) {
        // Indicator mode => show the spinner; otherwise suppress it (content
        // mode shows the reasoning text, disabled shows nothing).
        if *self.show_reasoning.lock().unwrap() != ShowReasoning::Indicator {
            return;
        }
        let mut spinner_lock = self.spinner.lock().unwrap();
        if spinner_lock.is_none() {
            *spinner_lock = Some(Self::create_spinner(message));
        }
    }

    fn stop_thinking(&self) {
        if let Some(pb) = self.spinner.lock().unwrap().take() {
            pb.finish_and_clear();
        }
    }

    fn display_header(
        &self,
        provider: &str,
        model: &str,
        yolo: bool,
        limit: usize,
        persona: Option<&str>,
    ) {
        let width = term_width();
        let avatar = [
            "    ▄     ▄    ",
            "   ███   ███   ",
            "  ███████████  ",
            " ███ █   █ ███ ",
            " █████████████ ",
            "   ███   ███   ",
            "  ██       ██  ",
        ];

        let mut out = self.writer.lock().unwrap();
        let _ = writeln!(out);
        for line in avatar {
            let padding = width.saturating_sub(15) / 2;
            let _ = writeln!(out, "{}{}", " ".repeat(padding), style(line).cyan());
        }
        let _ = writeln!(out);

        let status = |active, label, color: fn(StyledObject<String>) -> StyledObject<String>| {
            let s = style(format!("[{}] {}", if active { "x" } else { " " }, label));
            if active {
                color(s)
            } else {
                s.dim()
            }
        };

        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".into());

        let _ = write!(
            out, "{} | {} ({})",
            style("picocode").bold(),
            style(provider).cyan(),
            style(model).blue(),
        );

        if let Some(p) = persona {
            let _ = write!(out, " | {}", style(p).magenta());
        }

        let _ = writeln!(
            out, " | {} | {} | {}",
            status(yolo, "yolo", |s| s.red()),
            style(format!("limit:{}", limit)).yellow(),
            style(cwd).dim()
        );
        self.note_lines(9);
        drop(out);
        // The header is content, not a blank separator.
        self.set_blank(false);
    }

    fn begin_capture(&self) {
        self.begin_capture_inner();
    }

    fn end_capture(&self) -> Vec<u8> {
        self.end_capture_inner()
    }
}

/// An `Output` wrapper no longer used: the REPL relies on the terminal's
/// native scrollback (the session scrolls there naturally during the session),
/// so a separate ring buffer is unnecessary.
#[allow(dead_code)]
pub struct ScrollbackOutput {
    inner: Arc<dyn Output>,
    sb: Arc<Mutex<VecDeque<String>>>,
}

#[allow(dead_code)]
impl ScrollbackOutput {
    pub fn new(inner: Arc<dyn Output>, sb: Arc<Mutex<VecDeque<String>>>) -> Self {
        Self { inner, sb }
    }

    fn append(&self, text: &str) {
        let mut sb = self.sb.lock().unwrap();
        for line in text.split('\n') {
            sb.push_back(line.to_string());
        }
        const CAP: usize = 1000;
        while sb.len() > CAP {
            sb.pop_front();
        }
    }
}

impl Output for ScrollbackOutput {
    fn cursor_row(&self) -> usize {
        self.inner.cursor_row()
    }
    fn display_plain(&self, text: &str) {
        self.append(text);
        self.inner.display_plain(text);
    }
    fn display_text(&self, text: &str) {
        self.append(text);
        self.inner.display_text(text);
    }
    fn display_tool_call(&self, name: &str, args: &Value) {
        self.inner.display_tool_call(name, args);
    }
    fn display_tool_result(&self, result: &str) {
        self.append(result);
        self.inner.display_tool_result(result);
    }
    fn get_user_input(&self) -> String {
        self.inner.get_user_input()
    }
    fn display_error(&self, error: &str) {
        self.append(error);
        self.inner.display_error(error);
    }
    fn display_system(&self, text: &str) {
        self.append(text);
        self.inner.display_system(text);
    }
    fn confirm(&self, message: &str) -> Confirmation {
        self.inner.confirm(message)
    }
    fn confirm_gate(&self) -> Option<Arc<AtomicBool>> {
        self.inner.confirm_gate()
    }
    fn display_separator(&self) {
        let line = "─".repeat(term_width());
        self.append(&line);
        self.inner.display_separator();
    }
    fn display_thinking(&self, message: &str) {
        self.inner.display_thinking(message);
    }
    fn stop_thinking(&self) {
        self.inner.stop_thinking();
    }
    fn display_header(
        &self,
        provider: &str,
        model: &str,
        yolo: bool,
        limit: usize,
        persona: Option<&str>,
    ) {
        self.inner.display_header(provider, model, yolo, limit, persona);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_word_boundary() {
        assert_eq!(wrap("hello world", 5), vec!["hello", "world"]);
    }

    #[test]
    fn wrap_long_no_space() {
        assert_eq!(
            wrap("abcdefghij", 3),
            vec!["abc", "def", "ghi", "j"]
        );
    }

    #[test]
    fn wrap_cjk_respects_width_and_preserves_content() {
        let s = "汉字测试文本";
        let r = wrap(s, 2);
        assert!(
            r.iter().all(|l| l.chars().count() <= 2),
            "a line exceeded the wrap width: {:?}",
            r
        );
        // No spaces are inserted between CJK segments, so concatenation is lossless.
        assert_eq!(r.concat(), s);
    }

    #[test]
    fn wrap_empty_width_returns_lines() {
        assert_eq!(wrap("a\nb", 0), vec!["a", "b"]);
    }

    #[test]
    fn wrap_empty_input() {
        assert_eq!(wrap("", 10), vec![""]);
        // A pure-whitespace logical line keeps its whitespace (it has
        // visible width, unlike a truly empty line).
        assert_eq!(wrap("   ", 10), vec!["   "]);
    }

    #[test]
    fn wrap_preserves_blank_line() {
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn wrap_multiple_spaces_collapsed() {
        assert_eq!(wrap("hello   world", 5), vec!["hello", "world"]);
    }

    #[test]
    fn wrap_keeps_trailing_punctuation_attached() {
        assert_eq!(wrap("Commands:", 100), vec!["Commands:"]);
        assert_eq!(wrap("(provider:name)", 100), vec!["(provider:name)"]);
    }

    #[test]
    fn wrap_single_spaces_around_emdash() {
        assert_eq!(wrap("— Show available commands", 100), vec!["— Show available commands"]);
    }

    #[test]
    fn wrap_preserves_indent_while_collapsing_inter_word_runs() {
        // Leading whitespace is preserved so indentation survives a reflow,
        // but inter-word whitespace runs are still collapsed to a single
        // space (the same behavior as before for the body of the line).
        let help = "  /help      — Show available commands\n  /model      — Select a model";
        let r = wrap(help, 100);
        assert_eq!(
            r,
            vec![
                "  /help — Show available commands".to_string(),
                "  /model — Select a model".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_preserves_indent_on_continuation_lines() {
        // When a logical line is wrapped to multiple physical lines, every
        // continuation line keeps the original indent so the wrapped block
        // reads as one indented paragraph.
        let text = "    one two three four five six seven eight nine ten";
        // Width 15 - 4 (indent) = 11 chars for content per line.
        let r = wrap(text, 15);
        for line in &r {
            assert!(
                line.starts_with("    "),
                "every wrapped line should keep the 4-space indent, got {:?}",
                line
            );
        }
        assert_eq!(
            r,
            vec![
                "    one two".to_string(),
                "    three four".to_string(),
                "    five six".to_string(),
                "    seven eight".to_string(),
                "    nine ten".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_long_word_first_chunk_keeps_indent_subsequent_chunks_plain() {
        // A word longer than the available content width is hard-split. The
        // FIRST chunk keeps the indent (it's the first visual line of that
        // word); the renderer will prepend its own marker to subsequent
        // chunks, so emitting them plain keeps continuation lines aligned.
        let text = "  abcdefghij";
        // Width 5 - 2 (indent) = 3 chars content per chunk.
        let r = wrap(text, 5);
        assert_eq!(
            r,
            vec![
                "  abc".to_string(),
                "def".to_string(),
                "ghi".to_string(),
                "j".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_with_diff_markers_preserves_indent_on_minus_line() {
        // Regression: a `- ` diff line wrapping an indented source line
        // used to collapse the indent (e.g. `-     if foo {` → `- if foo {`),
        // because plain `wrap()` saw the `-` as a normal word and
        // `split_whitespace` collapsed the run of whitespace between the
        // marker and the first token. The diff-marker helper now routes
        // diff lines through a marker-aware path that preserves the body's
        // leading indent.
        let r = wrap_with_diff_markers("-     if foo {", 80);
        assert_eq!(r, vec!["-     if foo {".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_preserves_indent_on_plus_line() {
        let r = wrap_with_diff_markers("+         let x = 1;", 80);
        assert_eq!(r, vec!["+         let x = 1;".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_continuation_aligns_under_marker() {
        // Long diff line: the body wraps, the marker stays on the first
        // line, and subsequent lines indent by two columns to align under
        // the marker column.
        let text = "- foo bar baz qux quux quuz corge grault garply waldo fred";
        // Width 20 - 2 (marker column) = 18 chars for the body per line.
        let r = wrap_with_diff_markers(text, 20);
        // The first line must keep the marker; continuation lines must be
        // prefixed with two spaces to align under `- `.
        assert!(r[0].starts_with("- "), "first line must keep the marker, got {:?}", r);
        for line in &r[1..] {
            assert!(
                line.starts_with("  "),
                "continuation lines must indent by two columns, got {:?}",
                line
            );
        }
        // And the body content must survive intact (no collapsing).
        let joined: String = r
            .iter()
            .flat_map(|l| l.chars())
            .collect();
        // Every original character must appear in the wrapped output.
        assert!(joined.contains("foo"));
        assert!(joined.contains("garply"));
    }

    #[test]
    fn wrap_with_diff_markers_passes_plain_lines_through_unchanged() {
        // Lines that aren't `- ` / `+ ` markers (e.g. a context line that
        // happens to start with `-`) are wrapped via the regular `wrap`,
        // which keeps the indent-preservation behavior for normal text.
        let r = wrap_with_diff_markers("    hello world", 80);
        assert_eq!(r, vec!["    hello world".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_empty_input() {
        assert_eq!(wrap_with_diff_markers("", 80), vec!["".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_multiple_lines_mixed() {
        // A multi-line input that mixes diff markers and a blank logical line.
        let text = "-     if foo {\n+     if bar {\n";
        let r = wrap_with_diff_markers(text, 80);
        assert_eq!(
            r,
            vec![
                "-     if foo {".to_string(),
                "+     if bar {".to_string(),
                "".to_string(),
            ]
        );
    }

    #[test]
    fn truncate_multibyte_no_panic() {
        assert_eq!(truncate("hello", 3), "hel...");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn get_preview_read_file_range() {
        let args = serde_json::json!({"path": "src/output.rs", "offset": 100, "limit": 50});
        assert_eq!(get_preview("read_file", &args).0, "read src/output.rs [101..150]");
        // zero limit => whole file, no range
        let args = serde_json::json!({"path": "src/output.rs", "offset": 0, "limit": 0});
        assert_eq!(get_preview("read_file", &args).0, "read src/output.rs");
        // offset-only (limit 0) => open-ended range
        let args = serde_json::json!({"path": "src/output.rs", "offset": 19, "limit": 0});
        assert_eq!(get_preview("read_file", &args).0, "read src/output.rs [20..]");
    }

    #[test]
    fn get_preview_bash_full_cmdline() {
        let args = serde_json::json!({"cmd": "cargo build --release && echo done"});
        // No truncation: the full command is preserved verbatim, prefixed with
        // `$` so it reads like a shell prompt.
        assert_eq!(
            get_preview("bash", &args).0,
            "$ cargo build --release && echo done"
        );
    }

    #[test]
    fn get_preview_list_dir_shortened_to_ls() {
        // `list_dir` (and any `List_*` variant) collapses to `ls <path>`.
        for name in ["list_dir", "ListDir", "ListFiles"] {
            let (label, diff) = get_preview(name, &serde_json::json!({"path": "src"}));
            assert_eq!(label, "ls src");
            assert!(diff.is_empty());
        }
    }

    #[test]
    fn get_preview_edit_file_falls_back_without_file() {
        let args = serde_json::json!({
            "path": "/no/such/file.rs",
            "old": "a",
            "new": "b",
            "all": false
        });
        let (label, diff) = get_preview("edit_file", &args);
        assert_eq!(label, "edit /no/such/file.rs");
        assert!(diff.is_empty());
    }

    #[test]
    fn edit_preview_preserves_indent_on_diff_lines() {
        // Regression: edit_preview builds `- ` and `+ ` diff lines from the
        // raw source, preserving any leading indentation. Each line is then
        // routed through `wrap_with_diff_markers` (via `tool_panel`), which
        // must not collapse that indentation when reflowing.
        let dir = std::env::temp_dir().join("picocode_edit_indent_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("sample.rs");
        // Use a unique content so a leftover file from a previous run
        // can't change the diff shape. Clean up before and after.
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "fn main() {\n    let x = 1;\n    println!(\"{}\", x);\n}\n",
        )
        .unwrap();
        let old_text = "    let x = 1;\n    println!(\"{}\", x);".to_string();
        let new_text = "    let x = 2;\n    println!(\"{}!\", x);".to_string();
        let edits = vec![crate::tools::EditOp {
            old_text: old_text.clone(),
            new_text: new_text.clone(),
        }];
        let (_range, diff) = edit_preview(path.to_str().unwrap(), &edits);

        // First, the raw edit_preview output must keep the indent: each
        // diff line carries the source line verbatim, with the original
        // leading spaces intact.
        assert!(
            diff.iter().any(|l| l == "-     let x = 1;"),
            "old first line should keep its indent in the raw diff, got {:?}",
            diff
        );
        assert!(
            diff.iter().any(|l| l == "+     let x = 2;"),
            "new first line should keep its indent in the raw diff, got {:?}",
            diff
        );

        // Second, when each diff line is routed through the same wrap path
        // that tool_panel uses, the indent must still survive. This is the
        // visible regression: previously, the indent collapsed to a single
        // space because plain `wrap()` treated `-` as a normal word and
        // `split_whitespace` collapsed the run between `-` and the first
        // source-line token.
        for line in &diff {
            let wrapped = wrap_with_diff_markers(line, 80);
            assert_eq!(wrapped, vec![line.clone()], "indent should survive wrap_with_diff_markers for {:?}", line);
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn get_preview_unknown_tool_truncates() {
        let args = serde_json::json!({"query": "a very very very very very long value that exceeds fifty characters for sure"});
        let (label, diff) = get_preview("Search", &args);
        assert!(label.starts_with("Search "));
        assert!(label.ends_with("..."));
        assert!(diff.is_empty());
    }

    #[test]
    fn get_preview_grep_full_pattern_and_path() {
        // Matches the `grep_text` tool name (and any `Grep_*` family member).
        let args = serde_json::json!({
            "pat": "fn (main|run)\\(",
            "path": "/home/gabriel/src/picocode2/src"
        });
        for name in ["grep_text", "GrepText", "GrepFiles"] {
            let (label, diff) = get_preview(name, &args);
            // Label is shortened to just `grep`, and both pattern and path are
            // shown in full (no 50-col truncation).
            assert_eq!(label, "grep fn (main|run)\\( /home/gabriel/src/picocode2/src");
            assert!(diff.is_empty());
        }
    }

    #[test]
    fn render_markdown_gfm_table_is_ansi_free_and_boxed() {
        let out = render_markdown("| a | b |\n|---|---|\n| 1 | 2 |\n", 80, false);
        assert!(!out.contains('\x1b'), "no_color output must be ANSI-free");
        assert!(out.contains('│'), "table needs column separators");
        assert!(out.contains('├'), "table needs a header rule");
        assert!(out.contains('┤'));
        assert!(out.contains('1') && out.contains('2'));
    }

    /// Strip ANSI SGR sequences so we can assert the *highlighted* output still
    /// contains the original source text (highlighting must not drop content).
    fn strip_ansi(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b {
                if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                    continue;
                }
                i += 1;
                continue;
            }
            let ch_len = utf8_len(bytes[i]);
            if i + ch_len <= bytes.len() {
                out.push_str(std::str::from_utf8(&bytes[i..i + ch_len]).unwrap_or("\u{fffd}"));
            }
            i += ch_len.max(1);
        }
        out
    }

    #[test]
    fn render_markdown_code_is_syntax_highlighted_when_colored() {
        let src = "```rust\nfn main() {}\n```\n";
        let colored = render_markdown(src, 80, true);
        // Highlighting enabled on the terminal path: SGR codes wrap the tokens.
        assert!(
            colored.contains('\x1b'),
            "color path must enable syntax highlighting (ANSI present)"
        );
        // Stripping ANSI must recover the source text (no content lost).
        assert!(
            strip_ansi(&colored).contains("fn main()"),
            "highlighting must preserve the source code"
        );
    }

    #[test]
    fn render_markdown_code_has_no_ansi_when_plain() {
        let src = "```rust\nfn main() {}\n```\n";
        let plain = render_markdown(src, 80, false);
        assert!(!plain.contains('\x1b'), "non-terminal path must stay ANSI-free");
        assert!(plain.contains("fn main()"));
    }

    #[test]
    fn render_markdown_heading_and_blockquote_keep_text() {
        let out = render_markdown("# Title\n> quoted", 80, false);
        assert!(!out.contains('\x1b'));
        assert!(out.contains("Title"));
        assert!(out.contains('│') && out.contains("quoted"));
    }

    #[test]
    fn console_output_streaming_buffers_and_flushes() {
        let out = ConsoleOutput::default();
        // Force the buffered (non-TTY) path this test exercises, so it does not
        // depend on whether the test process's stdout is attached to a terminal.
        out.begin_response_inner(false);
        assert_eq!(out.text_buffer.lock().unwrap().clone(), "");

        out.stream_text("hello ");
        out.stream_text("world");
        assert_eq!(out.text_buffer.lock().unwrap().clone(), "hello world");

        // A tool-call boundary commits (flushes + clears) the text buffer so
        // the next static line prints below the committed text.
        out.end_response_segment();
        assert_eq!(out.text_buffer.lock().unwrap().clone(), "");

        out.stream_thinking("let me think");
        // Non-TTY thinking is streamed to stderr, never buffered into the
        // stdout answer path (so stdout stays clean for piping).
        assert!(!out.text_buffer.lock().unwrap().contains("let me think"));

        // End of response flushes remaining buffers.
        out.end_response();
        assert_eq!(out.text_buffer.lock().unwrap().clone(), "");
    }

    /// The TTY live path relies on the crate's *incremental* `Renderer` + `StreamParser`:
    /// completed blocks are emitted once (so scrollback stays clean) and code
    /// lines stream as they arrive, while an open paragraph is held back until
    /// it closes (coarse-grained). These tests pin that contract.
    #[test]
    fn incremental_renderer_streams_code_lines_and_coarsens_paragraphs() {
        let mut parser = StreamParser::new_gfm();
        let mut renderer = Renderer::new(Theme::no_color(), 80);
        let mut buf: Vec<u8> = Vec::new();

        // Code block streams line-by-line *before* the fence closes.
        let _ = renderer.feed(&parser.write("```python\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("def f():\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("    return 1\n".as_bytes()), &mut buf);
        let mid = String::from_utf8(buf.clone()).unwrap();
        assert!(
            mid.contains("def f():"),
            "code line should render before the fence closes"
        );
        let _ = renderer.feed(&parser.write("```\n".as_bytes()), &mut buf);

        // An open paragraph is NOT emitted until it closes (coarse-grained).
        let before = buf.len();
        let _ = renderer.feed(
            &parser.write("Intro sentence without a trailing blank line".as_bytes()),
            &mut buf,
        );
        assert_eq!(buf.len(), before, "open paragraph must not be emitted yet");

        // Finalizing the document flushes the trailing paragraph.
        let _ = renderer.feed(&parser.flush(), &mut buf);
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("Intro sentence without a trailing blank line"),
            "flush must emit the trailing paragraph"
        );
    }

    #[test]
    fn incremental_renderer_gfm_table_renders() {
        let mut parser = StreamParser::new_gfm();
        let mut renderer = Renderer::new(Theme::no_color(), 80);
        let mut buf: Vec<u8> = Vec::new();
        let evs = parser.write("| a | b |\n|---|---|\n| 1 | 2 |\n\n".as_bytes());
        let _ = renderer.feed(&evs, &mut buf);
        let _ = renderer.feed(&parser.flush(), &mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains('│'), "GFM table must render with box chars");
    }

    #[test]
    fn render_markdown_emits_osc8_links_only_when_colored() {
        // `color` is `is_terminal()` in callers, so OSC 8 clickable links must
        // appear on a TTY (color = true) and stay absent for pipes/CI.
        let tty = render_markdown("[text](https://example.com/a)", 80, true);
        assert!(tty.contains("\x1b]8;;https://example.com/a\x1b\\"), "TTY render must wrap link in OSC 8");
        assert!(tty.contains("text"));
        let piped = render_markdown("[text](https://example.com/a)", 80, false);
        assert!(
            !piped.contains("\x1b]8;;"),
            "non-TTY render must not emit OSC 8 hyperlink sequences"
        );
    }

    #[test]
    fn thinking_streams_inline_and_dimmed() {
        // Mirrors Phase 5's TTY thinking path: a faint SGR + a faded theme whose
        // `reset` re-applies the dim keep thinking faded even through inline
        // formatting (bold/code), without re-rendering past content.
        let mut parser = StreamParser::new_gfm();
        let mut renderer = Renderer::new(Theme::dimmed(), 80);
        let mut buf: Vec<u8> = Vec::new();
        let _ = write!(buf, "\x1b[2m"); // initial dim (mirrors `ensure_dim(true)`)
        let _ = renderer.feed(&parser.write("let me **think** step by step\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.flush(), &mut buf);
        let _ = write!(buf, "\x1b[0m"); // reset (mirrors `ensure_dim(false)`)
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\x1b[2m"), "thinking must be dimmed");
        assert!(
            out.contains("\x1b[0m\x1b[2m"),
            "inline formatting must re-apply dim (reset re-dim), not clear it"
        );
    }

    #[test]
    fn multi_turn_answer_boundary_single_blank() {
        // Reproduces the TTY orchestration with *persistent* renderers across a
        // multi-turn turn (thinking1 -> answer1 -> tool -> thinking2 -> answer2),
        // exactly as the `ConsoleOutput` fields behave. Before the fix, the
        // answer renderer's `pending_gap` (set when the answer paragraph was
        // committed at the tool boundary) fired at answer2's first block, on
        // top of the one blank the thinking→answer transition emits — TWO
        // blank lines. Resetting the answer renderer at the tool boundary
        // (as `end_response_segment` now does) drops that pending gap.
        let mut tparser = StreamParser::new_gfm();
        let mut trenderer = Renderer::new(Theme::dimmed(), 80);
        let mut aparser = StreamParser::new_gfm();
        let mut arenderer = Renderer::new(Theme::default(), 80);
        let mut out: Vec<u8> = Vec::new();
        let w = |out: &mut Vec<u8>, s: &[u8]| out.extend_from_slice(s);

        // thinking1 -> answer1
        let _ = trenderer.feed(&tparser.write(b"think one\n"), &mut out);
        let _ = trenderer.feed(&tparser.write(b"\n\n"), &mut out); // transition
        w(&mut out, b"\n"); // separator
        let _ = arenderer.feed(&aparser.write(b"\n\nanswer one\n"), &mut out);
        w(&mut out, b"\n");

        // tool call boundary: commit the answer paragraph, then RESET the
        // answer renderer (mirrors the fix) so no pending gap survives into
        // answer2. The model's leading blank lines stay in answer2 below.
        let _ = arenderer.feed(&aparser.write(b"\n\n"), &mut out);
        arenderer = Renderer::new(Theme::default(), 80);
        w(&mut out, b"\n");

        // thinking2 -> answer2 (model sends leading blank lines)
        let _ = trenderer.feed(&tparser.write(b"think two\n"), &mut out);
        let _ = trenderer.feed(&tparser.write(b"\n\n"), &mut out); // transition
        w(&mut out, b"\n"); // separator
        let _ = arenderer.feed(&aparser.write(b"\n\nanswer two\n"), &mut out);

        // end_response: flush both documents once.
        let _ = arenderer.feed(&aparser.flush(), &mut out);
        let _ = trenderer.feed(&tparser.flush(), &mut out);
        w(&mut out, b"\n");

        let s = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = s.split('\n').collect();
        let idx = match lines.iter().position(|l| l.contains("answer two")) {
            Some(i) => i,
            None => {
                for (i, l) in lines.iter().enumerate() {
                    eprintln!("[{i}] {:?}", l);
                }
                panic!("answer two not found");
            }
        };
        assert!(
            lines[idx - 1].trim().is_empty(),
            "line immediately before answer two must be blank"
        );
        assert!(
            !lines[idx - 2].trim().is_empty(),
            "exactly ONE blank line before answer two (no double gap)"
        );
    }

    #[test]
    #[serial_test::serial]
    fn late_reasoning_answer_is_withheld_until_turn_end() {
        // Regression test for the OpenRouter ordering quirk: when reasoning is
        // enabled, answer text that arrives *before* a trailing reasoning delta
        // must be *withheld* (buffered) until the turn ends, so it can be
        // flushed *below* the reasoning rather than above it. Reasoning renders
        // inline immediately (see `stream_thinking`), so withholding the answer
        // is what guarantees the `[thinking][answer]` emission order.
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response_inner(true); // force the live/TTY path
        out.set_answer_hold(true); // reasoning enabled → hold the answer
        // Answer arrives first, then a trailing reasoning delta.
        out.stream_text("ZQANSWERZQ");
        out.stream_thinking("ZQTHINKZQ");
        // While held, the answer lives in the buffer, not yet emitted.
        assert_eq!(
            out.answer_buffer.lock().unwrap().as_str(),
            "ZQANSWERZQ",
            "answer must be buffered (withheld) while reasoning is still streaming"
        );
        out.end_response();
        // At the turn boundary the answer is flushed and the buffer cleared.
        assert!(
            out.answer_buffer.lock().unwrap().is_empty(),
            "held answer must be flushed at end_response"
        );
    }

    #[test]
    #[serial_test::serial]
    fn answer_not_withheld_when_reasoning_disabled() {
        // Without answer-holding, the answer renders inline (not buffered), so
        // this path is unaffected by the fix.
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response_inner(true);
        out.set_answer_hold(false);
        out.stream_text("ZQANSWERZQ");
        assert!(
            out.answer_buffer.lock().unwrap().is_empty(),
            "answer must render inline (not buffered) when holding is off"
        );
    }

    #[test]
    #[serial_test::serial]
    fn reasoning_list_before_answer_has_blank_line() {
        // Regression for two related bugs (see `doc/reasoning_order_opus.md`
        // §4 and §10):
        //  (a) reasoning whose last block is a GFM list was stranded below the
        //      answer because the thinking parser was committed with
        //      `write(b"\n\n")` (a blank line can't close a list) instead of
        //      `flush()`;
        //  (b) in the held-answer (reasoning-enabled) path the answer started
        //      immediately after the reasoning with no separating blank line.
        //
        // Using StreamParser+Renderer directly (not ConsoleOutput) avoids
        // fd-level stdout capture, which is fragile under parallel test
        // execution (the test harness writes "test ... ok" lines to stdout
        // from concurrent threads, polluting the capture).
        let mut thinking_parser = StreamParser::new_gfm();
        let mut thinking_renderer = Renderer::new(Theme::dimmed(), 80);
        let mut answer_parser = StreamParser::new_gfm();
        let mut answer_renderer = Renderer::new(Theme::default(), 80);
        let mut out: Vec<u8> = Vec::new();

        // Simulate reasoning that ends with a bullet list (the stranding trigger).
        // Using `flush()` (not `write(b"\n\n")`) so the list closes properly.
        out.extend_from_slice(b"\x1b[2m"); // initial dim (mirrors ensure_dim(true))
        let _ = thinking_renderer.feed(&thinking_parser.write(b"Plan:\n- step one\n- step two\n"), &mut out);
        let _ = thinking_renderer.feed(&thinking_parser.flush(), &mut out);
        out.extend_from_slice(b"\x1b[0m"); // reset dim (mirrors ensure_dim(false))

        // Separator: one blank line between reasoning and answer.
        out.extend_from_slice(b"\n");

        // Simulate the answer text.
        let _ = answer_renderer.feed(&answer_parser.write(b"done.\n"), &mut out);
        let _ = answer_renderer.feed(&answer_parser.flush(), &mut out);
        out.extend_from_slice(b"\n");

        let plain = strip_ansi(&String::from_utf8_lossy(&out));
        let lines: Vec<&str> = plain.split('\n').collect();

        // Assert (a): list items appear before the answer.
        let list_idx = lines.iter().position(|l| l.contains("step one")).unwrap();
        let ans_idx = lines.iter().position(|l| l.contains("done.")).unwrap();
        assert!(list_idx < ans_idx, "reasoning (list) must render BEFORE the answer");

        // Assert (b): exactly one blank line before answer.
        assert!(
            ans_idx > 0 && lines[ans_idx - 1].trim().is_empty(),
            "answer must be separated from reasoning by a blank line; got:\n{plain}"
        );
        assert!(
            ans_idx < 2 || !lines[ans_idx - 2].trim().is_empty(),
            "exactly ONE blank line before answer (no double gap)"
        );
    }

    #[test]
    fn tmp_verify_tool_render_helpers() {
        // line-number gutter stripping
        assert_eq!(strip_read_lineno("   1| fn main()"), "fn main()");
        assert_eq!(strip_read_lineno("  12| let x = a | b;"), "let x = a | b;");
        assert_eq!(strip_read_lineno("no gutter here"), "no gutter here");
        assert_eq!(strip_read_lineno("   5| "), "");
        // language guessing
        assert_eq!(guess_lang("src/output.rs"), "rust");
        assert_eq!(guess_lang("a/b.py"), "python");
        assert_eq!(guess_lang("x.unknown"), "text");
        // highlighting: colored path keeps the source text and emits ANSI
        let colored = highlight_code_fg("let x = 1;", "rust", true);
        // ANSI escapes break the literal substring, so strip them first
        let plain = strip_ansi(&colored);
        assert!(plain.contains("let x = 1;"), "highlighting must not drop content; got plain={plain:?} from colored={colored:?}");
        assert!(colored.contains('\x1b'), "color path must emit ANSI");
        // non-color path returns the line untouched
        assert_eq!(highlight_code_fg("let x = 1;", "rust", false), "let x = 1;");
    }

    #[test]
    fn tmp_repro_separator_blanks() {
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response_inner(true); // live path
        out.set_answer_hold(false);
        out.begin_capture_inner();

        // Case A: turn ends with a tool result (no trailing answer text).
        out.stream_text("Let me sleep.\n");
        out.end_response_segment();
        out.display_tool_call("bash", &serde_json::json!({"command": "sleep 30"}));
        out.display_tool_result("\"2. sleep 30 — done\"");
        out.end_response();
        out.display_separator();
        let a = out.end_capture_inner();
        let a = strip_ansi(&String::from_utf8_lossy(&a));
        eprintln!("=== Case A (tool-result end) ===\n{:?}", a);

        // Case B: turn ends with plain text answer.
        out.begin_response_inner(true);
        out.set_answer_hold(false);
        out.begin_capture_inner();
        out.stream_text("All done.\n");
        out.end_response();
        out.display_separator();
        let b = out.end_capture_inner();
        let b = strip_ansi(&String::from_utf8_lossy(&b));
        eprintln!("=== Case B (text end) ===\n{:?}", b);

        // Case C: turn ends with trailing thinking after a tool result.
        out.begin_response_inner(true);
        out.set_answer_hold(false);
        out.begin_capture_inner();
        out.stream_text("Let me sleep.\n");
        out.end_response_segment();
        out.display_tool_call("bash", &serde_json::json!({"command": "sleep 30"}));
        out.display_tool_result("\"2. sleep 30 — done\"");
        out.stream_thinking("wrapping up\n");
        out.end_response();
        out.display_separator();
        let c = out.end_capture_inner();
        let c = strip_ansi(&String::from_utf8_lossy(&c));
        eprintln!("=== Case C (tool + trailing thinking) ===\n{:?}", c);
    }
}
