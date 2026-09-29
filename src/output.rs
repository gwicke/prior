use crate::repl::current_term_width;
use crate::settings::{ShowReasoning, ShowReasoningHandle};
use crate::tools::EditOp;
use crate::ui::frame::{plan_apply, plan_clear_live_frame, ApplyPolicy, DrawCache, Frame};
use console::{style, StyledObject};
use libc;
use markdown_stream::{Parser, StreamParser};
use markdown_terminal::{Renderer, Theme};
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use termimad;
use unicode_width::UnicodeWidthStr;

use crate::row_count::{RowCounter, StreamTail};

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
    /// Non-interactive policy for confirm dialogs. Used by the async confirm
    /// driver when `confirm_gate()` is `None`. Default implementations
    /// return their existing `confirm` behavior (e.g. `Yes` for QuietOutput,
    /// `No` for LogOutput).
    fn confirm_default(&self, message: &str) -> Confirmation {
        self.confirm(message)
    }
    fn display_separator(&self);
    /// Set the transient status line the output should paint after each content
    /// block while a turn runs — the session / model / context readout.
    ///
    /// Transient and *relative* (never `CUP`, so a viewport scrolled back into
    /// the scrollback is never written into — the write lands below it,
    /// invisible). It occupies the content's last row and the next content
    /// write erases it, so the policy of when to paint it lives in the output
    /// and the caller only supplies the text. Empty disables it. A live frame
    /// (the prompt's input, a dialog) owns the last rows instead, so nothing is
    /// painted then.
    ///
    /// Default no-op: outputs with no cursor to write at have no status line.
    fn set_status(&self, _text: &str) {}
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
    fn end_capture(&self) -> Vec<u8> {
        Vec::new()
    }
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
        crossterm::terminal::size()
            .map(|(_, h)| h as usize)
            .unwrap_or(24)
    }

    /// The size frames are composed and applied for (ui_draw_v2 §9 #3) — the
    /// one shared source for callers that don't carry their own read (e.g. a
    /// dialog's initial size). Callers that *do* hold a read (the REPL loop's
    /// per-iteration size) pass it into [`Self::draw_frame`] instead.
    /// Default: a fresh terminal query.
    fn frame_size(&self) -> (usize, usize) {
        crossterm::terminal::size()
            .map(|(w, h)| (w.max(1) as usize, h.max(1) as usize))
            .unwrap_or((80, 24))
    }

    /// Draw a composed UI frame (ui_draw_v2 §5.1) with its top row at absolute
    /// `anchor` (1-based). `dims` is the (width, height) the frame was
    /// composed for: the apply uses exactly these, so the frame's bounding
    /// box can never disagree with its content (§9 #3). Returns the dims
    /// actually applied (clamped), for the caller's repaint bookkeeping.
    /// Default: drop the frame — non-console outputs carry no interactive
    /// UI, so the raw-mode REPL never runs against them. This is the *UI*
    /// surface; content output (`display_*`) stays separate.
    fn draw_frame(
        &self,
        _frame: &Frame,
        _policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        (dims.0.max(1), dims.1.max(1))
    }

    /// Gate the background in-flight reader should honor. While it is `true`,
    /// the reader must not read stdin, because a confirmation dialog on the
    /// main task owns stdin. `ui::confirm` raises it on entry and releases it
    /// in `resolve()` — every post-pause exit — so a gate left raised would
    /// freeze every later in-flight reader (plan §9 latent issue 1). Returns
    /// `None` for outputs that don't run the raw-mode REPL (so there is no
    /// background reader to coordinate with).
    fn confirm_gate(&self) -> Option<Arc<AtomicBool>> {
        None
    }
}

/// Output for a non-terminal consumer (`picocode "q" | cat`): no interactive UI
/// and no reasoning text, because there is no screen to show it on and the
/// answer is what the caller piped for. It therefore has no state at all — it
/// used to hold a `show_reasoning` handle for the spinner, and the spinner is
/// gone (row_accounting §3.6).
#[derive(Default)]
pub struct QuietOutput;

impl QuietOutput {
    pub fn new() -> Self {
        Self
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
        eprintln!("Error: {}", error);
    }
    fn display_system(&self, _text: &str) {}
    fn confirm(&self, message: &str) -> Confirmation {
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

/// Test helper: an `Output` that captures every `display_text` line for
/// assertions; everything else is a no-op (mirroring `NoOutput`).
/// Construct the handle, then feed it into
/// `AppState::for_test_with_output`.
#[cfg(test)]
pub struct RecordingOutput {
    pub lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[cfg(test)]
impl RecordingOutput {
    pub fn new() -> (
        std::sync::Arc<Self>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            std::sync::Arc::new(RecordingOutput {
                lines: lines.clone(),
            }),
            lines,
        )
    }

    /// Joined contents of everything displayed so far.
    pub fn joined(lines: &std::sync::Mutex<Vec<String>>) -> String {
        lines.lock().unwrap().join("\n")
    }
}

#[cfg(test)]
impl Output for RecordingOutput {
    fn display_text(&self, text: &str) {
        self.lines.lock().unwrap().push(text.to_string());
    }
    fn display_markdown(&self, text: &str) {
        self.lines.lock().unwrap().push(text.to_string());
    }
    fn display_plain(&self, text: &str) {
        self.lines.lock().unwrap().push(text.to_string());
    }
    fn display_tool_call(&self, _name: &str, _args: &Value) {}
    fn display_tool_result(&self, _result: &str) {}
    fn get_user_input(&self) -> String {
        String::new()
    }
    fn display_error(&self, _error: &str) {}
    fn display_system(&self, text: &str) {
        self.lines.lock().unwrap().push(text.to_string());
    }
    fn confirm(&self, _message: &str) -> Confirmation {
        Confirmation::Yes
    }
    fn display_separator(&self) {}
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
    /// Live-screen bookkeeping for the frame pipeline (plan §4): tracked cursor
    /// row, the clear floor, and the last applied frame. Always updated together
    /// with a write under the same lock order (writer, then `draw`).
    draw: Mutex<DrawCache>,
    /// Set `true` while `confirm` is reading a keystroke, so the background
    /// in-flight reader thread backs off and doesn't swallow the input.
    reader_paused: Arc<AtomicBool>,
    /// Whether the current response is being rendered live (TTY) or buffered
    /// (non-TTY). Decided once per response in `begin_response`.
    /// Where reasoning goes: `true` = stderr, so a consumer of
    /// `picocode "q" | cat` can drop it (`2>/dev/null`) or merge it (`2>&1`)
    /// without touching stdout. Decided once, here, because stdout does not
    /// become a terminal mid-run — and because a per-call `is_terminal()`
    /// scattered across the reasoning path is how the render path forked on
    /// `live` in the first place (row_accounting_b3.md §4.1). `pub(crate)` so
    /// the tests that pin the routing can ask for the other stream.
    pub(crate) reasoning_on_stderr: bool,
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
    /// Live `show_reasoning` display mode. Read on every thinking event so a
    /// `/set show_reasoning …` change applies immediately (no agent rebuild).
    show_reasoning: ShowReasoningHandle,

    /// The most recent tool call's `(name, args)`, captured in
    /// `display_tool_call` and read back in `display_tool_result` so the result
    /// can be rendered tool-aware (e.g. strip `read_file` line-number gutters
    /// and pick a syntax-highlight language). The hook always fires a call
    /// immediately before its result, so this is safe to read on the result.
    last_tool: Mutex<Option<(String, Value)>>,
    /// Whether the content stream currently ends with a *blank* line —
    /// derived from the bytes every write passes through, not set by hand
    /// (doc/row_accounting_b3.md B3d). [`Self::boundary`] consults it so two
    /// consecutive separators collapse into one empty line: the rule is one
    /// blank between non-blank lines, never two.
    ///
    /// Derived rather than kept because the markdown renderer ends its own
    /// blocks with a blank line. A flag written by hand disagrees with the
    /// screen at exactly those points, and the boundary then emits a second
    /// blank — which is the class of bug the separator matrix pinned.
    stream_tail: StreamTail,
    /// Where the transient status line currently sits, so the next content
    /// write can erase it (`None` when none is showing).
    status: Mutex<Option<StatusSpot>>,
    /// The status text to paint after each content block (empty = off).
    status_text: Mutex<String>,
    /// A frame has claimed the tail rows since the last status paint.
    ///
    /// Sticky on purpose: one content *burst* is often several writes (a
    /// block's boundary, then its body), and the first erases the frame, so a
    /// per-write "is a frame live?" would let the second paint a status onto
    /// rows the frame owned. The flag clears on the first successful paint,
    /// which is by definition after the content has moved past the frame.
    frame_owns_tail: std::sync::atomic::AtomicBool,
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

/// Terminal width in columns. Prefers the width tracked by the in-flight
/// reader (updated on resize), falls back to querying the terminal, then 80.
pub fn term_width() -> usize {
    let w = crate::repl::current_term_width();
    if w > 20 {
        return w;
    }
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
    let mut theme = if color {
        Theme::default()
    } else {
        Theme::no_color()
    };
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
        "sed" | "sed_preview" => {
            // Like `bash`, show what was asked for: the path, then the script on
            // one line, `$`-prefixed so it reads as a command. The long "review
            // these and pass them to `edit`" sentence stays in the tool result,
            // where the model reads it — putting it in the panel spent a human's
            // screen on instructions meant for the model.
            let path = get_str("path").unwrap_or("");
            let one_line = get_str("expressions")
                .unwrap_or("")
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            let mut label = format!("sed {path}");
            if !one_line.is_empty() {
                label.push_str(&format!("  $ {}", truncate(&one_line, 56)));
            }
            // The body needs the *result* (the blocks), which this function does
            // not have; `display_tool_result` renders those instead.
            (label, Vec::new())
        }
        s if s.starts_with("grep") => {
            // Grep tool: show pattern (from "pattern" or "pat") and path.
            let pat = get_str("pattern").or_else(|| get_str("pat")).unwrap_or("");
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
            // Try canonical format (edits array) first, then the legacy
            // single-edit top-level shape, then the old "old/new" fields.
            let edits: Vec<EditOp> = if let Some(edits_array) =
                obj.and_then(|o| o.get("edits")).and_then(|v| v.as_array())
            {
                edits_array
                    .iter()
                    .filter_map(|e| {
                        Some(EditOp {
                            old_text: e.get("oldText")?.as_str()?.to_string(),
                            new_text: e.get("newText")?.as_str()?.to_string(),
                            start_line: e
                                .get("startLine")
                                .and_then(|v| v.as_u64())
                                .map(crate::config::EditStartLine),
                        })
                    })
                    .collect()
            } else if let (Some(old), Some(new)) = (get_str("oldText"), get_str("newText")) {
                // Legacy single-edit shape (accepted but undocumented).
                vec![EditOp {
                    old_text: old.to_string(),
                    new_text: new.to_string(),
                    start_line: None,
                }]
            } else {
                vec![]
            };
            let (range, diff) = if !edits.is_empty() {
                edit_preview(path, &edits)
            } else {
                // Old format: old/new fields.
                let old = get_str("old").unwrap_or("");
                let new_str = get_str("new").unwrap_or("");
                edit_preview(
                    path,
                    &[EditOp {
                        old_text: old.to_string(),
                        new_text: new_str.to_string(),
                        start_line: None,
                    }],
                )
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
            (
                format!("{} {}", cap, truncate(&first.replace('\n', " "), 50)),
                Vec::new(),
            )
        }
    }
}

/// Best-effort line range + diff preview for an edit tool call.
///
/// The refined unified diff for **every** edit in the batch is derived from
/// each edit's `old_text` vs `new_text`, so all diffs render even when the file
/// can't be read or `old_text` has drifted (e.g. session replay after the edit
/// already applied). Each unioned `@@` hunk header already conveys the change
/// regions, so small batches concatenate their diffs directly with no
/// separators or labels; very large batches are truncated so the preview
/// doesn't flood the scrollback. The 1-based line *range* in the label
/// reflects the first edit and requires reading the file and finding its
/// `old_text`; otherwise the range is omitted but the diffs are still shown.
/// Empty edits yield `(None, [])`.
fn edit_preview(path: &str, edits: &[EditOp]) -> (Option<String>, Vec<String>) {
    let first = match edits.first() {
        Some(e) => e,
        None => return (None, Vec::new()),
    };

    // The refined diffs are derived purely from each edit's oldText vs newText,
    // so they are always available — even during replay or after the target text
    // has drifted in the on-disk file. Only the 1-based line *range* requires
    // reading the file and finding `old_text` in its current contents; a
    // missing/read-error file or a no-longer-present oldText degrades to a
    // `None` range while still yielding the diff previews.
    const MAX_OUT: usize = 80;
    let mut diff: Vec<String> = Vec::new();
    let mut truncated = false;

    for edit in edits {
        if diff.len() >= MAX_OUT {
            truncated = true;
            break;
        }
        for line in refined_diff_lines(&edit.old_text, &edit.new_text) {
            if diff.len() >= MAX_OUT {
                truncated = true;
                break;
            }
            diff.push(line);
        }
        if truncated {
            break;
        }
    }

    if truncated {
        diff.push("    … (diff truncated)".to_string());
    }

    let range = (|| {
        let Ok(content) = std::fs::read_to_string(path) else {
            return None;
        };
        let Some(idx) = content.find(&first.old_text) else {
            return None;
        };
        // 1-based line number of the first line of `old_text`.
        let start = content[..idx].matches('\n').count() + 1;
        let end = start + first.old_text.matches('\n').count();
        Some(if start == end {
            format!("{start}")
        } else {
            format!("{start}..{end}")
        })
    })();

    (range, diff)
}

/// Render a `sed` preview result as diff lines, or `None` if it is not one.
///
/// The banner is kept verbatim (it is the part that says how many blocks are
/// shown, whether growth ran, and what was *not* included) and each block's
/// contents become `-`/`+` pairs under a hunk header carrying its anchor. An
/// empty `NEW` — a deletion — simply has no `+` lines.
fn sed_preview_body(result: &str) -> Option<Vec<String>> {
    if !result.contains("@@@OLD") {
        return None;
    }
    let parsed = crate::edit_blocks::parse_blocks(result).ok()?;

    let mut out: Vec<String> = Vec::new();
    let mut summary = String::new();
    // The banner is model-facing; only the parts a human needs are lifted out —
    // the counts as a closing line, and any NOTE, which is a warning about what
    // is *missing* from the preview and must not be scrolled past.
    if let Some(first) = result.find("@@@OLD") {
        for line in result[..first].lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if t.starts_with("NOTE:") {
                out.push(t.to_string());
            } else if summary.is_empty() {
                // "sed preview: 3 blocks for src/x.rs (1 expression). Review ..."
                // → "3 blocks in src/x.rs from 1 expression"
                summary = short_banner(t);
            }
        }
    }

    for e in &parsed.edits {
        let old_n = e.old_text.lines().count().max(1) as u64;
        let new_n = e.new_text.lines().count().max(1) as u64;
        let at = e.start_line.unwrap_or(0);
        out.push(format!("@@ -{at},{old_n} +{at},{new_n} @@"));
        for line in e.old_text.lines() {
            out.push(format!("- {line}"));
        }
        for line in e.new_text.lines() {
            out.push(format!("+ {line}"));
        }
    }

    if !summary.is_empty() {
        out.push(String::new());
        out.push(summary);
    }
    Some(out)
}

/// Condense the model-facing banner into one line for a human.
///
/// The banner is written for the model — it ends with an instruction about
/// passing the blocks to `edit`, which is noise to someone reading a panel —
/// so keep the counts and the file, drop the instruction.
fn short_banner(banner: &str) -> String {
    // "sed preview: 3 blocks for src/x.rs (1 expression). Review the blocks ..."
    let head = banner
        .split(". ")
        .next()
        .unwrap_or(banner)
        .trim_end_matches('.');
    let head = head.strip_prefix("sed preview: ").unwrap_or(head);
    let (counts, file) = match head.find(" for ") {
        Some(i) => (&head[..i], head[i + 5..].trim()),
        None => (head, ""),
    };
    // The parenthetical is moved out of the path rather than left to be printed
    // twice.
    let (file, exprs) = match file.split_once(" (") {
        Some((f, rest)) => (f, rest.strip_suffix(')').unwrap_or(rest)),
        None => (file, ""),
    };
    let mut out = String::new();
    if !counts.is_empty() {
        out.push_str(counts);
        // "3 blocks for x" → "3 blocks in x"; keep the counts wording otherwise.
        if !file.is_empty() {
            out.push_str(" in");
        }
    }
    if !file.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(file);
    }
    if !exprs.is_empty() {
        out.push_str(&format!(", from {exprs}"));
    }
    out
}

#[cfg(test)]
mod sed_preview_body_tests {
    use super::{get_preview, sed_preview_body, short_banner};

    const RESULT: &str = "sed preview: 2 blocks for src/x.rs (1 expression). Review the blocks below, then pass them to the `edit` tool as its `edits` argument.
src/x.rs:5
@@@OLD
    run();
@@@NEW
    walk();
@@@END

src/x.rs:9
@@@OLD
    drop();
    // tail
@@@NEW
    // tail
@@@END
";

    #[test]
    fn renders_blocks_as_a_diff_with_anchors() {
        let lines = sed_preview_body(RESULT).expect("body");
        let joined = lines.join("\n");
        // The counts survive as a short closing line; the instruction telling the
        // model to pass the blocks to `edit` is not something a human needs.
        assert!(
            joined.contains("2 blocks in src/x.rs, from 1 expression"),
            "{joined}"
        );
        assert!(!joined.contains("pass them to the `edit` tool"), "{joined}");
        // Each block becomes a -/+ pair under its anchor.
        assert!(joined.contains("@@ -5,1 +5,1 @@"), "{joined}");
        assert!(joined.contains("-     run();"), "{joined}");
        assert!(joined.contains("+     walk();"), "{joined}");
        assert!(joined.contains("@@ -9,2 +9,1 @@"), "{joined}");
        // A deletion keeps its removed lines and has no additions.
        assert!(joined.contains("-     drop();"), "{joined}");
        assert!(!joined.contains("+     drop();"), "{joined}");
    }

    #[test]
    fn notes_survive_because_they_say_what_is_missing() {
        // `bsed` emits NOTEs straight after the banner, before the first block
        // (see the banner builder in crates/bsed/src/lib.rs).
        let note = "NOTE: only 37 of 152 blocks are shown. The other 115 are NOT included.";
        let with_note = RESULT.replacen("\nsrc/x.rs:5", &format!("\n{note}\nsrc/x.rs:5"), 1);
        assert!(
            with_note.contains(note),
            "fixture must place the note in the banner"
        );
        let joined = sed_preview_body(&with_note).expect("body").join("\n");
        assert!(joined.contains("only 37 of 152 blocks"), "{joined}");
        // It is kept, and it is not mistaken for the one-line summary.
        assert!(
            joined.contains("2 blocks in src/x.rs, from 1 expression"),
            "{joined}"
        );
    }

    #[test]
    fn short_banner_drops_the_model_instruction() {
        assert_eq!(
            short_banner(
                "sed preview: 3 blocks for src/x.rs (2 expressions). Review the blocks below, \
                 then pass them to the `edit` tool as its `edits` argument."
            ),
            "3 blocks in src/x.rs, from 2 expressions"
        );
        // A no-change banner has nothing to condense.
        assert_eq!(
            short_banner("no lines matched \u{2014} src/x.rs is unchanged by this 1 expression."),
            "no lines matched \u{2014} src/x.rs is unchanged by this 1 expression"
        );
    }

    #[test]
    fn the_label_carries_the_expressions_on_one_line() {
        let args = serde_json::json!({
            "path": "./test_engine.rs",
            "expressions": "s/pub fn /fn /\n    s/^    /        /",
        });
        let (label, extra) = get_preview("sed", &args);
        assert!(label.starts_with("sed ./test_engine.rs  $ "), "{label}");
        // Newlines collapse, so the whole script stays on the label's one line.
        assert!(!label.contains('\n'), "{label}");
        assert!(
            label.contains("s/pub fn /fn /; s/^    /        /"),
            "{label}"
        );
        assert!(extra.is_empty(), "{extra:?}");
    }

    #[test]
    fn a_long_script_is_truncated_rather_than_wrapping_the_panel() {
        let args = serde_json::json!({
            "path": "x.rs",
            "expressions": format!("s/{}/y/", "a".repeat(200)),
        });
        let (label, _) = get_preview("sed", &args);
        assert!(label.ends_with("..."), "{label}");
        assert!(label.lines().count() == 1, "{label}");
    }

    #[test]
    fn leaves_other_results_alone() {
        // Not a sed preview: the caller falls through to the generic renderer.
        assert!(sed_preview_body("Replaced 1 block(s) in src/x.rs").is_none());
        assert!(sed_preview_body("").is_none());
    }
}

/// Build a refined unified-diff preview of `old -> new`.
///
/// Uses Myers diff (via `similar`) so only genuinely changed regions show up,
/// each with two lines of context. Changed lines are emitted as `<marker>
/// <content>` (`-` removed, `+` added) and unchanged context as `<marker>
/// <content>` with `<marker>` a single space — the space after the marker keeps
/// all content columns aligned and matches `tool_panel`'s `- ` / `+ ` diff
/// detection. Lines are free of trailing newlines, so `tool_panel` can reflow
/// them without inserting spurious blank rows. A cap keeps pathological
/// replacements (e.g. replacing the whole file) from flooding the scrollback.
fn refined_diff_lines(old: &str, new: &str) -> Vec<String> {
    const MAX: usize = 60;
    const CONTEXT: usize = 2;

    let diff = TextDiff::from_lines(old, new);
    let mut unified = diff.unified_diff();
    unified.context_radius(CONTEXT);

    let mut out: Vec<String> = Vec::new();
    let mut truncated = false;

    for hunk in unified.iter_hunks() {
        for (idx, change) in hunk.iter_changes().enumerate() {
            if idx == 0 {
                // Emit a hunk header as its own neutral context line.
                out.push(format!("{}", hunk.header()));
                if out.len() >= MAX {
                    truncated = true;
                    break;
                }
            }
            if out.len() >= MAX {
                truncated = true;
                break;
            }
            let marker = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            // `from_lines` includes the trailing newline on each value; strip it
            // so `tool_panel` wraps the line as one physical row. The space
            // after the marker keeps `- ` / `+ ` recognizable to `tool_panel`
            // and aligns context lines with changed lines.
            let value = change.value().trim_end_matches('\n');
            out.push(format!("{marker} {value}"));
        }
        if truncated {
            break;
        }
    }

    if truncated {
        out.push("    … (diff truncated)".to_string());
    }
    out
}

impl ConsoleOutput {
    pub fn new(show_reasoning: ShowReasoningHandle) -> Self {
        Self {
            draw: Mutex::new(DrawCache::new(term_height())),
            reader_paused: Arc::new(AtomicBool::new(false)),
            reasoning_on_stderr: !std::io::stdout().is_terminal(),
            writer: Mutex::new(Box::new(io::stdout())),
            parser: Mutex::new(StreamParser::new_gfm_stream()),
            renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            thinking_parser: Mutex::new(StreamParser::new_gfm_stream()),
            thinking_renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            dim_on: Mutex::new(false),
            show_reasoning,

            last_tool: Mutex::new(None),
            stream_tail: StreamTail::new(),
            status: Mutex::new(None),
            status_text: Mutex::new(String::new()),
            frame_owns_tail: std::sync::atomic::AtomicBool::new(false),
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
pub(crate) fn strip_ansi(s: &str) -> String {
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
                if i < bytes.len() {
                    i += 1;
                } // consume final byte
                continue;
            }
            if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                i += 2;
                while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == 0x1b {
                    // ST is ESC \ — both bytes belong to the sequence, and
                    // leaving the backslash behind put a phantom column in
                    // every width measured from a hyperlink-terminated string
                    // (row_count::display_width_ansi is pinned against this).
                    i += 2;
                } else if i < bytes.len() {
                    i += 1; // consume BEL
                }
                continue;
            }
            if i + 1 < bytes.len() && (0x20..=0x7e).contains(&bytes[i + 1]) {
                // Any other two-character escape (a bare ST after a truncated
                // sequence, for instance): ESC plus its final byte, zero
                // width. Without this the ESC became U+FFFD and the final
                // byte a visible column.
                i += 2;
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
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
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
    let restore = |f: i32, o: i32| unsafe {
        libc::fcntl(f, libc::F_SETFL, o);
    };

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
    if buf.len() < 4 || buf[0] != 0x1b || buf[1] != b'[' || *buf.last()? != b'R' {
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
        // Advance the tracked cursor and drop the frame region — the write that
        // called us moved the cursor off the frame (plan §4.2).
        self.draw.lock().unwrap().note_lines(n, h as usize);
    }

    /// A `Write` for one block of reasoning. The bytes are routed when the
    /// block is committed: to the screen (and counted, like any other
    /// content) when there is a terminal, to stderr when there is not, so a
    /// consumer of `picocode "q" | cat` can drop reasoning
    /// (`2>/dev/null`) or merge it (`2>&1`) without touching stdout.
    ///
    /// The decision is made once, here, rather than at every reasoning call
    /// site — which is what let the render path fork on `live` in the first
    /// place (row_accounting_b3.md §4.1).
    fn reasoning_sink(&self) -> ReasoningSink {
        ReasoningSink {
            buf: Vec::new(),
            to_stderr: self.reasoning_on_stderr,
        }
    }

    /// Write content bytes, then advance the draw cache by the number of
    /// terminal rows they occupy.
    ///
    /// The count is *measured from the bytes*, not predicted from the source:
    /// a message is a prefix plus rendered markdown, a panel row is padded to
    /// the full width, a long line wraps — none of which the caller knows
    /// (doc/row_accounting.md §6, phase B). Everything content-related goes
    /// through here or [`Self::with_content_writer`], so the count cannot be
    /// forgotten; a writer that bypassed both is a bug the frame tests find.
    fn write_content(&self, bytes: &[u8]) {
        let width = term_width();
        self.with_content_writer(width, |out| {
            let _ = out.write_all(bytes);
        });
    }

    /// Run `f` with the writer lock wrapped in a [`RowCounter`], then advance
    /// the draw cache by the rows `f` wrote. For the writers that produce
    /// their own bytes — the streaming markdown renderers, which want a
    /// `Write` to feed.
    ///
    /// `f` gets a `&mut dyn io::Write`, which keeps the counter's type (and
    /// its borrow of the lock) out of the signature. A caller handing it to
    /// something generic over `W: Write` — `Renderer::feed`,
    /// `commit_thinking` — passes `&mut out`, which is itself a `Write` and
    /// satisfies the `Sized` bound that `&mut dyn Write` cannot.
    ///
    /// `width` is the width the content is being rendered for; pass the
    /// renderer's own width so a line that wrapped at render time is counted
    /// at that width.
    fn with_content_writer<T, F>(&self, width: usize, f: F) -> T
    where
        F: FnOnce(&mut dyn io::Write) -> T,
    {
        // A transient status line goes before anything is written over it, and
        // the cursor returns to where it was — which is where this write
        // starts. Ahead of the writer lock: `clear_status` takes it.
        self.clear_status();
        let mut out = self.writer.lock().unwrap();
        // The counter updates the shared tail per byte, so a boundary
        // *inside* this block (a committed paragraph before the block that
        // follows it) sees the state those bytes produced, not the state from
        // before the block.
        let mut counter = RowCounter::new(&mut **out, width, &self.stream_tail);
        // Every content write starts at the cursor, which is inside the live
        // frame when there is one. Erase that frame first, or the write strands
        // the rows it does not cover (`plan_clear_live_frame`).
        let clear = {
            let mut cache = self.draw.lock().unwrap();
            plan_clear_live_frame(&mut cache)
        };
        if !clear.is_empty() {
            let _ = counter.write_all(clear.as_bytes());
        }
        let r = f(&mut counter);
        let rows = counter.rows();
        let _ = counter.flush();
        drop(counter);
        drop(out);
        // Zero rows means nothing moved, and a frame stays live.
        if rows > 0 {
            self.note_lines(rows);
        }
        // The status line rides at the tail of the content, so it is live for
        // the whole turn rather than only at its end. After the writer lock: it
        // writes to the writer itself, deliberately outside the counter.
        self.paint_status();
        r
    }

    /// Paint the transient status line at the cursor, if it is time.
    ///
    /// Called at the tail of every content write, so the readout is live for
    /// the whole turn rather than only at its end. Three conditions are the
    /// whole contract:
    ///
    /// - **Relative only** — the bytes are the text, no `CUP` — so a viewport
    ///   scrolled back into the scrollback is never written into; the write
    ///   lands below it, invisible. That is why this is not a frame element
    ///   (doc/row_accounting.md, zone 2).
    /// - **At column 0** — the line is transient and the next content write
    ///   erases it, so starting it mid-line would take a partially written
    ///   content line with it. `StreamTail::at_line_start` is that check, free.
    /// - **No frame was live** — a frame owns the last rows (the prompt's
    ///   input, a dialog), and the prompt-side status is a frame concern.
    fn paint_status(&self) {
        let text = self.status_text.lock().unwrap().clone();
        if text.is_empty() || !self.stream_tail.at_line_start() {
            return;
        }
        if self
            .frame_owns_tail
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            // A frame claimed these rows, and this write only just erased them
            // (see `plan_clear_live_frame`) — so the status waits for the next
            // one, which lands below the content rather than among them. Held
            // across a *burst*: the boundary write and the body write are two
            // calls, and only the second is past the frame.
            return;
        }
        // Straight to the writer, **not** through the row counter: the status is
        // transparent to the blank-line rule, and now to the row count too (see
        // the gap note below).
        //
        // The **gap**: one blank row between the last content line and the bar,
        // so the two are never adjacent — which is what makes the bar readable
        // below a grey tool panel.
        //
        // `ends_blank()` is the blank-line rule's own bit, and it already answers
        // the only question there is: it is true when the row the stream *just
        // closed* was itself empty. When it is, that row **is** the gap and the
        // bar takes the current row — the status reuses the blank the content
        // path already wrote instead of stacking a second one on top of it. When
        // it is false the last line of the block is directly above, so the status
        // opens a blank row of its own.
        //
        // Reusing the rule's bit rather than keeping a second opinion is the whole
        // trick. The two features used to disagree and the screen showed it: a
        // tool call's boundary blank and the gap were two different rows, so a
        // boundary followed by a bar left *two* blank lines where one was meant.
        // One source of truth cannot disagree with itself.
        //
        // The status therefore *owns* the gap when it needs one, instead of
        // borrowing the content's trailing newline: a chunk that does not end on
        // a newline does not leave a row to borrow.
        //
        // The write is relative and needs no row: `at_line_start` above is what
        // admits us here, so the status starts a fresh line.
        let gap = !self.stream_tail.ends_blank();
        let mut out = self.writer.lock().unwrap();
        if gap {
            let _ = out.write_all(b"\n");
        }
        let _ = out.write_all(text.as_bytes());
        // The bar is exactly the terminal's width, so writing it leaves the
        // cursor *wrap-pending* in the last column. A `CR` cancels that, and
        // without it whatever writes next starts in a state it does not expect:
        // a dialog's frame apply opens with bare LFs, and a pending wrap makes
        // the terminal complete the line first and move a row further than the
        // block asked for. `rows_of` is computed on the bar without this byte,
        // which adds no row.
        let _ = out.write_all(b"\r");
        // Put back the dim the bar's own reset just cleared.
        //
        // `ensure_dim` emits `\x1b[2m` / `\x1b[0m` only when the tracked
        // `dim_on` *changes*, so a status bar in the middle of streamed
        // reasoning used to break it in both directions: the bar inherited the
        // terminal's dim (its `38;5;245m` sets a colour, not an attribute, so it
        // did not clear dim) and its trailing reset then silenced the *next*
        // reasoning write, which saw no change to re-assert. The bar opens with
        // its own reset now, but the ambient state is still ours to restore, and
        // it is the flag, not the terminal, that decides.
        if *self.dim_on.lock().unwrap() {
            let _ = out.write_all(b"\x1b[2m");
        }
        // Hide the cursor for as long as the bar is up. The bar annotates a turn
        // in flight — there is nothing to type into — and a block cursor parked
        // at column 0 of a dark full-width bar is the brightest thing on the
        // screen. `draw_frame` shows it again when the input row comes back, and
        // `TermGuard::stop` covers exiting while it is hidden.
        let _ = out.write_all(HIDE_CURSOR);
        let _ = out.flush();
        drop(out);
        // The gap row, when the paint opened one, plus the bar's own row. The
        // bar is truncated to the terminal, so it cannot wrap and the `max(1)`
        // is its single row — kept explicit so a future change that lets it wrap
        // widens the erase instead of leaving a row behind.
        let rows = usize::from(gap) + crate::row_count::rows_of(&text, term_width()).max(1);
        *self.status.lock().unwrap() = Some(StatusSpot { rows });
        // **No `note_lines` here**, and the gap is why that is now structural
        // rather than incidental. The cursor physically sits on the bar, one row
        // below where the stream logically is; `clear_status` walks it back to
        // the gap row before the next write, so every writer sees the row the
        // content actually ended on. Counting the gap would push the tracked row
        // one too far and the *next* block's blank-line rule would then see a
        // row that is not blank.
    }

    /// Erase a transient status line, if one is showing, leaving the cursor at
    /// its first cell — which is where the next content write starts. Called at
    /// the head of every content write, so the status never outlives the block
    /// it annotated.
    ///
    /// Relative, like the write: the cursor has not moved since the status was
    /// painted, so `CR` + `EL` is the whole of it and `CUU` covers the rows a
    /// wrapped status took. No row is addressed at all, which matters twice
    /// over — it cannot land on the wrong row, and it cannot write into a
    /// viewport the user has scrolled back into.
    fn clear_status(&self) {
        let Some(spot) = self.status.lock().unwrap().take() else {
            return;
        };
        let mut bytes = String::from("\r\x1b[K");
        for _ in 1..spot.rows.max(1) {
            bytes.push_str("\x1b[1A\r\x1b[K");
        }
        let mut out = self.writer.lock().unwrap();
        let _ = out.write_all(bytes.as_bytes());
        let _ = out.flush();
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
            *tparser = StreamParser::new_gfm_stream();
            events
        };
        let mut trenderer = self.thinking_renderer.lock().unwrap();
        let _ = trenderer.feed(&events, out);
    }

    fn separator(&self) {
        let width = term_width();
        self.write_content(format!("{}\n", style("─".repeat(width)).dim()).as_bytes());
    }

    /// Emit the blank line that separates this block from what came before —
    /// unless the stream already ends with one.
    ///
    /// This is the whole separator rule (doc/row_accounting_b3.md §1): one
    /// blank line at every boundary, and inside a block nothing but the
    /// markdown renderer's own output. Asking the *stream* rather than a flag is
    /// what makes it hold: the renderer ends a paragraph with a blank line of
    /// its own, so a hand-kept "did I just print a blank" flag disagrees with
    /// the screen at exactly those points and the boundary adds a second one.
    fn boundary(&self) {
        if self.needs_boundary() {
            self.write_content(b"\n");
        }
    }

    /// Does the next block need a blank line above it?
    fn needs_boundary(&self) -> bool {
        !self.stream_tail.ends_blank()
    }

    /// Start the renderers fresh, so the next block either of them renders
    /// carries no gap of its own.
    ///
    /// A long-lived `Renderer` remembers that it has written something
    /// (`wrote_any`) and opens its next block with a blank line. Inside one
    /// document that is right; at a boundary the *host* already separated it is
    /// a second blank. `end_response_segment` has always reset the answer
    /// renderer for this reason; the tool boundary needs it for the *thinking*
    /// renderer, which is where the doubled blank came from — a turn with
    /// reasoning before the tool left that renderer mid-document, and its next
    /// block (the post-tool reasoning) arrived with a gap of its own.
    ///
    /// The renderers hold no state the parser does not, so replacing them is
    /// free: the parser keeps the document, the renderer starts a block.
    fn reset_renderer_gaps(&self) {
        let width = current_term_width();
        let mut renderer = self.renderer.lock().unwrap();
        *renderer = Renderer::new(answer_theme(), width);
        let mut trenderer = self.thinking_renderer.lock().unwrap();
        *trenderer = Renderer::new(Theme::dimmed(), width);
    }

    /// Close a tool block: the blank line above whatever comes next, and the
    /// renderers starting fresh for it.
    fn end_tool_block(&self) {
        self.boundary();
        self.reset_renderer_gaps();
    }

    /// [`Self::boundary`] for inside a [`Self::with_content_writer`] block,
    /// where the separator has to land in the *same* counted write as the block
    /// it opens — the lock is already held, and one write is one row count.
    fn boundary_into(&self, out: &mut dyn io::Write) {
        if self.needs_boundary() {
            let _ = out.write_all(b"\n");
        }
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
            if indent {
                format!("{BG} ")
            } else {
                String::new()
            },
            painted,
            format!("{BG}"),
            format!("{}{}", " ".repeat(pad), RESET),
        );
        self.write_content(format!("{row}\n").as_bytes());
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
}

/// How many rows the transient status line took — more than one when it wrapped
/// at a narrow terminal. That is all the erase needs: the cursor is still where
/// the status ended, so the erase is relative, exactly like the write.
struct StatusSpot {
    rows: usize,
}

/// DECTCEM show/hide. The status line hides the cursor while a turn streams —
/// the bar is an annotation, not a place to type — and a frame apply shows it
/// again. `TermGuard::stop` emits `SHOW_CURSOR` unconditionally as the net for
/// exiting while it is hidden.
pub(crate) const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";

/// The theme the answer renderer is built with: colours and clickable links on
/// a terminal, plain otherwise. Shared with the tests, whose "what did the
/// renderer alone produce" reference has to agree with it byte for byte or the
/// comparison flags styling as host contribution.
fn answer_theme() -> Theme {
    if std::io::stdout().is_terminal() {
        Theme {
            clickable_links: true,
            ..Theme::default()
        }
    } else {
        Theme::no_color()
    }
}

/// Buffered reasoning bytes awaiting routing. See
/// [`ConsoleOutput::reasoning_sink`].
pub(crate) struct ReasoningSink {
    buf: Vec<u8>,
    to_stderr: bool,
}

impl ReasoningSink {
    /// Route what has been written: stderr when there is no terminal, else the
    /// screen through the same counter every other content write uses (so the
    /// draw cache learns the height). Idempotent — the buffer is taken.
    fn commit(&mut self, out: &ConsoleOutput) {
        if self.buf.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.buf);
        if self.to_stderr {
            let mut err = std::io::stderr();
            let _ = err.write_all(&bytes);
            let _ = err.flush();
        } else {
            out.write_content(&bytes);
        }
    }
}

impl io::Write for ReasoningSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Output for ConsoleOutput {
    fn cursor_row(&self) -> usize {
        // Prefer the real terminal cursor position (robust to wrapping,
        // ANSI escapes, and termimad-rendered responses). Fall back
        // to the tracked/ANSI-stripped prediction if the terminal
        // does not answer the Device Status Report (e.g. a headless pty).
        // Both paths re-anchor and retire the frame region — the
        // fallback too: a silent terminal offers no more guarantee than an
        // answering one, so §4.2's "any uncertainty → clear it" applies to
        // it as well (plan §9 latent issue 2).
        let row = query_real_cursor_row();
        // The re-anchor erases the live frame's rows, which is why it returns
        // bytes: a resize or a foreign writer can leave the cursor outside the
        // block, and the relative walks the frame path uses cannot reach rows
        // above the cursor from there. This is the one absolute address in a
        // frame apply (row_accounting §6b), and it costs nothing when no frame
        // is live — every streamed block, and every commit — because the bytes
        // are then just the cursor park. So it belongs here rather than at the
        // call sites: all of them already treat this as a side-effecting
        // re-anchor, and none of them writes.
        //
        // Writer before cache, as in `draw_frame`: one atomic step for the
        // emission and the cache advance, and never `draw` held while
        // acquiring `writer`.
        let mut out = self.writer.lock().unwrap();
        let (row, bytes) = {
            let mut cache = self.draw.lock().unwrap();
            cache.reanchor(row)
        };
        let _ = out.write_all(bytes.as_bytes());
        let _ = out.flush();
        // A frame no longer owns the tail rows: it was just retired, so the
        // transient status may paint there again.
        self.frame_owns_tail
            .store(false, std::sync::atomic::Ordering::Relaxed);
        row
    }

    fn frame_size(&self) -> (usize, usize) {
        (term_width(), term_height())
    }

    fn draw_frame(
        &self,
        frame: &Frame,
        policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        // Paint exactly the size the caller composed for (`dims`) — one
        // source for both phases of the frame (ui_draw_v2 §9 #3). Clamped
        // to what `plan_apply` stores as the live frame's `term_w`/`term_h`.
        let (tw, th) = (dims.0.max(1), dims.1.max(1));
        // A transient status goes before the frame does: the frame is about to
        // own these rows, and a full-width status bar left showing under a
        // dialog is the most visible thing on screen. Content writes already do
        // this (in `with_content_writer`); a frame apply is the other way a row
        // can be taken. Ahead of the writer lock: `clear_status` takes it.
        self.clear_status();
        // Lock the writer before the cache so the byte emission and the cache
        // advance are one atomic step, and so we never hold `draw` while
        // acquiring `writer` (matching `display_*`: writer, then `note_lines`).
        let mut out = self.writer.lock().unwrap();
        let bytes = {
            let mut cache = self.draw.lock().unwrap();
            plan_apply(&mut cache, frame, policy, tw, th)
        };
        let _ = out.write_all(bytes.as_bytes());
        // The status line hid the cursor; a frame is where the user types again,
        // so this is where it comes back.
        let _ = out.write_all(SHOW_CURSOR);
        let _ = out.flush();
        // A frame now owns the tail rows: the transient status must not paint
        // over them until content has moved past (see `frame_owns_tail`).
        self.frame_owns_tail
            .store(true, std::sync::atomic::Ordering::Relaxed);
        (tw, th)
    }

    fn display_plain(&self, text: &str) {
        // The count comes from the bytes: `writeln!` adds the newline, and a
        // caller-supplied line is padded or not as it likes.
        self.write_content(format!("{}\n", text).as_bytes());
    }

    fn display_markdown(&self, text: &str) {
        let color = std::io::stdout().is_terminal();
        let rendered = render_markdown(text, term_width(), color);
        // One blank line above the block, unless the stream already ends with
        // one — the same rule every other block uses, and the reason two
        // consecutive blocks (e.g. an error right after `end_response`) stay
        // one line apart without either of them knowing about the other.
        self.boundary();
        let mut block = String::with_capacity(rendered.len() + 1);
        block.push_str(&rendered);
        block.push('\n');
        // Measured, not predicted: a long rendered line wraps, and `rendered`
        // is a count of *logical* lines, not screen rows (§9 #6/#10).
        self.write_content(block.as_bytes());
    }

    fn display_text(&self, text: &str) {
        // One blank line above the message, unless the stream already ends with
        // one (e.g. the error path, where `end_response` left a trailing blank),
        // so the message sits exactly one empty line below the prior block.
        self.boundary();
        // The bytes, not the source, decide the height: the `⏺ ` prefix is two
        // columns the old `wrap(text).len()` count never saw, inline markdown
        // rewrites the text, and a message that exactly fills the width wraps
        // onto another row (§9 #6). Measuring the bytes removes the guess *and*
        // the DSR that used to correct for it.
        let block = format!("{} {}\n", style("⏺").cyan(), termimad::inline(text));
        self.write_content(block.as_bytes());
    }

    fn begin_response(&self) {
        // One render path for every run (B3c). There used to be a second one
        // for a non-tty stdout: buffer the whole response and render it as one
        // markdown block at the end. Two paths meant every replay test — a
        // test's stdout is never a terminal — exercised the *other* one, so the
        // incremental renderer, where the separator bugs live, went untested.
        // Colour still follows the terminal: that is a property of the bytes,
        // not of the render path.
        let theme = answer_theme();
        let current_width = current_term_width();
        let mut parser = self.parser.lock().unwrap();
        *parser = StreamParser::new_gfm_stream();
        let mut renderer = self.renderer.lock().unwrap();
        *renderer = Renderer::new(theme, current_width);
        // Reset the thinking parser/renderer and dim state for this response.
        let mut tparser = self.thinking_parser.lock().unwrap();
        *tparser = StreamParser::new_gfm_stream();
        let mut trenderer = self.thinking_renderer.lock().unwrap();
        *trenderer = Renderer::new(Theme::dimmed(), current_width);
        *self.dim_on.lock().unwrap() = false;
        // One blank line between the prompt (or whatever preceded) and the
        // response — the same rule the blocks below use, so a response that
        // starts right after a trailing blank does not gain a second one.
        self.boundary();
    }

    fn stream_text(&self, delta: &str) {
        // Sync terminal width (updated by in-flight reader on resize) so
        // renderers reflow on the next feed.
        let current_width = current_term_width();
        {
            let mut renderer = self.renderer.lock().unwrap();
            if renderer.width() != current_width {
                renderer.set_width(current_width);
            }
            let mut trenderer = self.thinking_renderer.lock().unwrap();
            if trenderer.width() != current_width {
                trenderer.set_width(current_width);
            }
        }
        // Coarse-grained live rendering: feed the delta to the incremental
        // parser and emit only the *completed* blocks. Each block prints once
        // (code lines line-by-line, paragraphs on close), so there is no whole
        // document re-render, no in-place cursor gymnastics, and scrollback
        // stays clean.
        // One counted block: the boundary blank (if any) and the rendered
        // blocks. The renderer produced the bytes, so the counter measures
        // them rather than the delta text (§9 #10).
        self.with_content_writer(current_width, |mut out| {
            // Thinking just ended: commit whatever is still buffered (while dim
            // is still on) so it lands above the answer, then turn the dim off
            // and put one blank line between them. The dim reset MUST come
            // before the blank: a GFM list/table emits no trailing newline of
            // its own, so a `\n` before the reset would be swallowed as the
            // block's closing newline instead of a separator.
            //
            // The tool→text boundary is *not* handled here: a completed tool
            // block ends with its own separator (`display_tool_result`), so the
            // stream already ends blank and this has nothing to add. That is
            // what the `last_was_tool` flag used to be for.
            if *self.dim_on.lock().unwrap() {
                self.commit_thinking(&mut out);
                self.ensure_dim(false, &mut out);
                self.boundary_into(out);
            }
            let mut parser = self.parser.lock().unwrap();
            let events = parser.write(delta.as_bytes());
            let mut renderer = self.renderer.lock().unwrap();
            let _ = renderer.feed(&events, &mut out);
        });
    }

    fn stream_thinking(&self, delta: &str) {
        // Content mode => stream the reasoning text; otherwise suppress it.
        if *self.show_reasoning.lock().unwrap() != ShowReasoning::Content {
            return;
        }
        // Sync terminal width (updated by in-flight reader on resize) so
        // renderers reflow on the next feed.
        let current_width = current_term_width();
        {
            let mut trenderer = self.thinking_renderer.lock().unwrap();
            if trenderer.width() != current_width {
                trenderer.set_width(current_width);
            }
        }
        // Always-dim inline: render thinking through its own incremental
        // renderer using a faded theme whose `reset` re-applies the dim SGR,
        // so inline formatting (bold/code) does not clear the fading. The
        // initial dim is turned on once per run via `ensure_dim`.
        //
        // The dim toggle travels with the reasoning, not with the answer: it
        // belongs to whichever stream the thinking went to, and the boundary
        // that follows turns it off there.
        let mut sink = self.reasoning_sink();
        self.ensure_dim(true, &mut sink);
        let mut parser = self.thinking_parser.lock().unwrap();
        let events = parser.write(delta.as_bytes());
        let mut renderer = self.thinking_renderer.lock().unwrap();
        let _ = renderer.feed(&events, &mut sink);
        sink.commit(self);
    }

    fn end_response_segment(&self) {
        let current_width = current_term_width();
        // One counted block: the answer commit, the thinking commit, the
        // boundary blank and the dim reset.
        self.with_content_writer(current_width, |mut out| {
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
                *parser = StreamParser::new_gfm_stream();
                let mut renderer = self.renderer.lock().unwrap();
                let _ = renderer.feed(&events, &mut out);
                let mut theme = Theme::default();
                theme.clickable_links = true; // real TTY path
                *renderer = Renderer::new(theme, current_width);
            }
            // Commit thinking (the thinking renderer is kept — post-tool
            // thinking continues as one continuous faded stream; only the
            // parser is reset, inside `commit_thinking`).
            self.commit_thinking(&mut out);
            self.ensure_dim(false, &mut out);
            // One blank line between this segment's content and the tool panel
            // below. Stacked boundaries — an empty terminal `CompletionCall`
            // after a segment that already left a blank — collapse to one.
            self.boundary_into(out);
        });
    }

    fn end_response(&self) {
        let current_width = current_term_width();
        // One counted block: the answer and thinking flushes and the
        // trailing blank.
        self.with_content_writer(current_width, |mut out| {
            // Finalize the answer document.
            {
                let mut parser = self.parser.lock().unwrap();
                let events = parser.flush();
                let mut renderer = self.renderer.lock().unwrap();
                let _ = renderer.feed(&events, &mut out);
            }
            // Finalize thinking (it was rendered inline + dimmed as it streamed).
            {
                let mut parser = self.thinking_parser.lock().unwrap();
                let events = parser.flush();
                let mut renderer = self.thinking_renderer.lock().unwrap();
                let _ = renderer.feed(&events, &mut out);
            }
            self.ensure_dim(false, &mut out);
            // A trailing blank, so the separator that opens the next loop
            // iteration does not butt against the last line of the response.
            // Skipped when the turn already ended with a blank (the terminal
            // `CompletionCall`'s `end_response_segment` left one).
            self.boundary_into(out);
        });
    }

    fn display_tool_call(&self, name: &str, args: &Value) {
        // Purpose-built label (+ optional trailing lines such as an edit diff)
        // from `get_preview`. One blank line above the panel, unless the stream
        // already ends with one — which is what used to need a "was the last
        // block a tool" flag: a text segment leaves the renderer's own trailing
        // blank behind, a tool panel does not, and the stream knows which.
        let (label, extra) = get_preview(name, args);
        // Remember the tool so `display_tool_result` can render the result
        // tool-aware (strip `read_file` line-number gutters, pick a highlight
        // language). The hook always fires the call immediately before its
        // result, so this is safe to read back on the result.
        *self.last_tool.lock().unwrap() = Some((name.to_string(), args.clone()));
        self.boundary();
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

    /// A tool block ends without a blank line: the panel rows are the last
    /// thing written, so whatever comes next — the next segment's text, another
    /// tool call, the turn's trailing blank — opens with one. Every exit below
    /// ends the block with it, which is the knowledge the `last_was_tool` flag
    /// used to carry, stated by the block that has it.
    fn display_tool_result(&self, result: &str) {
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
                self.end_tool_block();
                return;
            }
        }

        // A `sed` preview result is a block blob behind a one-line banner.
        // Render the blocks as a diff so the human reviewing the panel reads
        // `-`/`+` lines rather than @@@ markers; the model still gets the
        // verbatim blob in its tool message.
        if let Some(lines) = sed_preview_body(result) {
            // Not an error: a preview is a *proposal*, and colouring the whole
            // body red (as the edit-failure path does) says the opposite. The
            // `- `/`+ ` prefixes carry the red/green, matching an applied edit.
            for line in &lines {
                self.tool_panel(line, false, true);
            }
            return;
        }

        // Check for successful edit_file result and render a diff preview
        // Success format: "Replaced N block(s) in <path>"

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
        // Same treatment as the model: drop the redundant arg-error wrappers so
        // the panel shows the problem, the expected block, and the offending
        // structure — not a wall of prefixes. (Display is secondary to the model
        // here, but the same text, so the same cleanup.)
        cleaned = blockedit::strip_arg_error_prefixes(cleaned);

        let is_error =
            unquoted.starts_with("Toolset error") || unquoted.starts_with("ToolCallError");

        // An `edit` argument error is one self-correcting message with a fixed
        // `Expected:` / `Actual:` shape. Show it as two columns so the correct
        // line and the line the model actually sent are on the same row, and so
        // the block is half as tall (`Actual:` no longer falls below the fold).
        // The *model* still receives the vertical form — see arg_error_view.
        //
        // Painted rather than `tool_panel`ed: these lines are already sized to
        // the terminal and their columns align by construction, and
        // `tool_panel` re-wraps through `wrap()`, which collapses the interior
        // spaces that alignment depends on.
        {
            let width = term_width();
            let avail = width.saturating_sub(1).max(1);
            if let Some(table) = blockedit::arg_error_view::render_arg_error_table(cleaned, avail) {
                for line in &table {
                    self.paint_panel(&style(line.clone()).dim().to_string(), true);
                }
                return;
            }
        }

        let lines: Vec<_> = cleaned.lines().collect();

        if lines.is_empty() {
            self.tool_panel("(empty)", false, true);
            self.end_tool_block();
            return;
        }

        // Which tool produced this result (set by `display_tool_call`). Drives
        // read/write-specific rendering and language guessing.
        let tool = self.last_tool.lock().unwrap().clone();
        let is_read = tool
            .as_ref()
            .map(|(n, _)| n.eq_ignore_ascii_case("read") || n.eq_ignore_ascii_case("read_file"))
            .unwrap_or(false);
        let is_write = tool
            .as_ref()
            .map(|(n, _)| n.eq_ignore_ascii_case("write") || n.eq_ignore_ascii_case("write_file"))
            .unwrap_or(false);

        // Handle structured write result: {"path": "...", "content": "...", "lines": N, "bytes": N}
        if is_write {
            if let Ok(v) = serde_json::from_str::<Value>(result) {
                if let (Some(path), Some(content), Some(total_lines)) = (
                    v.get("path").and_then(|x| x.as_str()),
                    v.get("content").and_then(|x| x.as_str()),
                    v.get("lines").and_then(|x| x.as_u64()),
                ) {
                    // Show the path as a header with line count
                    let color = std::io::stdout().is_terminal();
                    let lang = guess_lang(path);
                    let header = format!("write {} [{} lines]", path, total_lines);
                    let header_styled = highlight_code_fg(&header, "bash", color);
                    self.paint_panel(&header_styled, true);

                    // Render the content with syntax highlighting (like read_file)
                    let show_max = 10; // Limit preview to 10 lines (match read_file)
                    let width = term_width();
                    let inner = width.saturating_sub(1).max(1);
                    let content_lines: Vec<_> = content.lines().collect();
                    for (printed, line) in content_lines.iter().enumerate() {
                        if printed >= show_max {
                            break;
                        }
                        for wline in wrap(line, inner) {
                            let styled = highlight_code_fg(&wline, lang, color);
                            self.paint_panel(&styled, true);
                        }
                    }
                    if content_lines.len() > show_max {
                        self.tool_panel(
                            &format!("... +{} lines", content_lines.len() - show_max),
                            false,
                            true,
                        );
                    }
                    self.end_tool_block();
                    return;
                }
            }
        }

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
            8
        };

        let width = term_width();
        let inner = width.saturating_sub(1).max(1);
        for (printed, line) in lines.iter().enumerate() {
            if printed >= show_max {
                break;
            }
            // `read_file` renders `NNNN| content`; drop the gutter for display
            // (the model still gets the numbers in the raw tool result).
            let visible = if is_read {
                strip_read_lineno(line)
            } else {
                line
            };
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
        self.end_tool_block();
    }

    fn get_user_input(&self) -> String {
        use std::io;
        // Through the counter like every other content write: it measures zero
        // rows (the prompt does not move the cursor down) but it does close any
        // blank line, so a block that follows does not inherit one.
        self.write_content(format!("{} ", style("❯").bold().blue()).as_bytes());
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        input.trim().to_string()
    }

    fn confirm_gate(&self) -> Option<Arc<AtomicBool>> {
        Some(self.reader_paused.clone())
    }

    fn display_error(&self, error: &str) {
        // Measured: a long error wraps, and the message carries a 2-column
        // prefix the row count has to include.
        self.write_content(format!("{} Error: {}\n", style("⏺").red(), error).as_bytes());
    }

    fn display_system(&self, text: &str) {
        self.write_content(format!("{}\n", style(text).bold().dim()).as_bytes());
    }

    fn confirm(&self, message: &str) -> Confirmation {
        use crate::slashdot_menu::node_impl::StaticRow;
        use crate::ui::ConfirmSpec;

        let spec = ConfirmSpec {
            header: message,
            options: &[
                StaticRow::new("yes", "Yes"),
                StaticRow::new("no", "No"),
                StaticRow::new("always", "Always (session)"),
            ],
            quick_keys: Some(&[('y', 0), ('n', 1), ('s', 2)]),
            default_sel: 1, // No
            // Mid-turn: a divider separates the dialog from the streamed
            // output above; the resolved choice is recorded as a synthetic
            // decision line (`accepted tool X` / `denied tool X`).
            divider: true,
            subject: Some(crate::ui::confirm_subject(message)),
        };

        // Run the sync dialog.
        let result = crate::ui::confirm(self, &spec);

        // Map result to Confirmation. Esc/dismiss -> No (safe default).
        match result {
            crate::ui::ConfirmResult::Selected(0) => Confirmation::Yes,
            crate::ui::ConfirmResult::Selected(2) => Confirmation::Always,
            _ => Confirmation::No,
        }
    }

    fn display_separator(&self) {
        // `separator` writes through the row counter; noting again here would
        // count the same row twice.
        self.separator();
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

        // One counted block: the greeter's height used to be a hardcoded 9
        // (blank + 3 avatar lines + blank + status), which is a guess like the
        // ones this phase exists to remove.
        let mut block = String::from("\n");
        for line in avatar {
            let padding = width.saturating_sub(15) / 2;
            block.push_str(&format!("{}{}\n", " ".repeat(padding), style(line).cyan()));
        }
        block.push('\n');

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

        block.push_str(&format!(
            "{} | {} ({})",
            style("picocode").bold(),
            style(provider).cyan(),
            style(model).blue(),
        ));

        if let Some(p) = persona {
            block.push_str(&format!(" | {}", style(p).magenta()));
        }

        block.push_str(&format!(
            " | {} | {} | {}\n",
            status(yolo, "yolo", |s| s.red()),
            style(format!("limit:{}", limit)).yellow(),
            style(cwd).dim()
        ));
        self.write_content(block.as_bytes());
    }

    fn begin_capture(&self) {
        self.begin_capture_inner();
    }

    /// Remember the status line to paint after each content block; see
    /// [`ConsoleOutput::paint_status`] for the three conditions that decide
    /// whether it actually goes out.
    fn set_status(&self, text: &str) {
        *self.status_text.lock().unwrap() = text.to_string();
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

    fn display_header(
        &self,
        provider: &str,
        model: &str,
        yolo: bool,
        limit: usize,
        persona: Option<&str>,
    ) {
        self.inner
            .display_header(provider, model, yolo, limit, persona);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `TERMINAL_WIDTH` is a process-global (the in-flight reader owns it in
    // production), so every test that pins the width must run alone.
    use serial_test::serial;

    // --- Row accounting: the tracked cursor vs. the real one (phase B) ---
    //
    // A wrong row count does not show up as a wrong number in isolation: the
    // *next* frame anchors one row off, and the visible damage is a clear that
    // lands in the wrong place (row_accounting §6 B2). So the assertion is
    // against the screen: run a writer, replay its bytes through a terminal
    // emulator, and require the tracked row to be the row the cursor is on.

    /// A `ConsoleOutput` writing into a capture buffer, with the shared width
    /// pinned so the expected geometry does not depend on the test runner's
    /// terminal. Height is not pinned (the clamp in `note_lines` needs a real
    /// terminal size), so the cases stay well under one screen.
    fn capturing_output(width: u16) -> (ConsoleOutput, crate::settings::ShowReasoningHandle) {
        crate::repl::TERMINAL_WIDTH.store(width as usize, std::sync::atomic::Ordering::SeqCst);
        let reasoning = crate::settings::default_show_reasoning_handle();
        (ConsoleOutput::new(reasoning.clone()), reasoning)
    }

    /// Tracked cursor row of `out` (0-based, as the screen reports it).
    fn tracked_row(out: &ConsoleOutput) -> usize {
        out.draw.lock().unwrap().tracked_row() - 1
    }

    /// Row the cursor ends on when `bytes` are replayed into a `width`x24
    /// terminal.
    ///
    /// The ONLCR shim is the one ui_draw_v2 P0 established: production emits
    /// bare `\n` and the tty line discipline turns it into CRLF, so the
    /// emulator has to be fed what the terminal would actually receive.
    /// Without it every LF leaves the column untouched and the whole model
    /// drifts sideways instead of down.
    fn screen_row(bytes: &[u8], width: u16) -> usize {
        let mut p = vt100::Parser::new(24, width, 100_000);
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        p.screen().cursor_position().0 as usize
    }

    #[test]
    #[serial]
    fn display_text_row_is_measured_not_predicted() {
        // A message that exactly fills the width, and one that is a column
        // wider: the old `wrap(text).len()` count called both one row.
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.display_text(&"x".repeat(36)); // + "⏺ " = 38 columns
        out.display_text(&"y".repeat(39)); // + "⏺ " = 41 -> wraps
        let bytes = out.end_capture();
        assert_eq!(
            tracked_row(&out),
            screen_row(&bytes, 40),
            "a message that wraps must be counted as two rows"
        );
    }

    #[test]
    #[serial]
    fn display_text_counts_a_wide_character_as_two_columns() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        // 19 CJK chars = 38 columns, plus the 2-column marker: wraps at 40.
        out.display_text(&"漢".repeat(19));
        let bytes = out.end_capture();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    #[test]
    #[serial]
    fn streamed_answer_is_counted() {
        // The untracked path (§9 #10): a streamed paragraph used to leave the
        // tracked row where it was.
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.stream_text("First paragraph of the answer.\n\n");
        out.stream_text("Second paragraph, streamed.\n\n");
        out.end_response();
        let bytes = out.end_capture();
        assert_eq!(
            tracked_row(&out),
            screen_row(&bytes, 40),
            "streamed content is not accounted for"
        );
    }

    #[test]
    #[serial]
    fn streamed_thinking_is_counted() {
        let (out, reasoning) = capturing_output(40);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.begin_capture();
        out.begin_response();
        out.stream_thinking("Thinking about the problem.\n\n");
        let bytes = out.end_capture();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    #[test]
    #[serial]
    fn tool_panel_is_counted() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.display_tool_call("Bash", &serde_json::json!({"command": "ls -la"}));
        out.display_tool_result("a\nb\nc");
        let bytes = out.end_capture();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    #[test]
    #[serial]
    fn markdown_and_error_and_plain_are_counted() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.display_markdown("# Title\n\nA paragraph long enough to wrap at forty columns.\n");
        out.display_error("an error long enough to wrap onto a second row at forty");
        out.display_plain("plain text that is long enough to wrap at forty columns ok");
        out.display_system("a system line long enough to wrap at forty columns");
        let bytes = out.end_capture();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    // --- B3a: the separator matrix (characterization) ---
    //
    // The rule under test (row_accounting_b3.md §1): one blank line at every
    // segment boundary, and inside a segment nothing but what the markdown
    // renderer wrote. The second half is the checkable one — the renderer is
    // available directly, so the *host's* contribution is whatever the painted
    // bytes have that a fresh renderer does not produce. This suite measures
    // today's behavior; it changes nothing.

    /// One step of a response, in the order the live path delivers it.
    #[derive(Clone, Copy)]
    enum Step {
        /// A reasoning delta (one chunk of it).
        Think(&'static str),
        /// An answer delta.
        Say(&'static str),
        Tool(&'static str),
        Result(&'static str),
        /// `end_response_segment` — the segment boundary.
        SegmentEnd,
        /// `end_response`.
        ResponseEnd,
    }

    /// How the deltas are split on their way in. `stream_text` sees deltas live
    /// and whole blocks in replay, so any rule that depends on how *often* it
    /// is called is a bug waiting for a different chunk boundary.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Chunking {
        /// One call per step.
        Whole,
        /// One call per byte.
        Bytes,
        /// One call per word, whitespace dropped.
        Words,
    }

    impl Chunking {
        fn all() -> [Chunking; 3] {
            [Chunking::Whole, Chunking::Bytes, Chunking::Words]
        }
        fn name(self) -> &'static str {
            match self {
                Chunking::Whole => "whole",
                Chunking::Bytes => "bytes",
                Chunking::Words => "words",
            }
        }
    }

    /// Deliver one `Say`/`Think` delta in this chunking.
    fn feed(out: &ConsoleOutput, step: Step, chunk: Chunking) {
        let (text, stream): (&str, fn(&ConsoleOutput, &str)) = match step {
            Step::Say(t) => (t, |o, t| o.stream_text(t)),
            Step::Think(t) => (t, |o, t| o.stream_thinking(t)),
            _ => return,
        };
        match chunk {
            Chunking::Whole => stream(out, text),
            Chunking::Bytes => {
                for b in text.as_bytes() {
                    stream(out, std::str::from_utf8(&[*b]).unwrap_or(""));
                }
            }
            Chunking::Words => {
                // Split at whitespace *runs*, keeping them: a chunk boundary
                // must not eat the paragraph breaks, or the test would be
                // measuring the harness instead of the code.
                let mut rest = text;
                while !rest.is_empty() {
                    let end = rest
                        .find(char::is_whitespace)
                        .map(|i| {
                            rest[i..]
                                .find(|c: char| !c.is_whitespace())
                                .map_or(i, |j| i + j)
                        })
                        .unwrap_or(rest.len());
                    if end == 0 {
                        let ws = rest
                            .find(|c: char| !c.is_whitespace())
                            .unwrap_or(rest.len());
                        stream(out, &rest[..ws]);
                        rest = &rest[ws..];
                    } else {
                        stream(out, &rest[..end]);
                        rest = &rest[end..];
                    }
                }
            }
        }
    }

    /// Run a scripted response through the live render path; return the bytes.
    fn run_script(steps: &[Step], chunk: Chunking) -> Vec<u8> {
        let (mut out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        // A test's stdout is not a terminal, so reasoning would be routed to
        // stderr — out of the capture. Ask for it on the captured stream; the
        // routing itself is what `reasoning_on_stderr` decides, and it is
        // exercised by `reasoning_goes_to_stderr_without_a_terminal`.
        out.reasoning_on_stderr = false;
        out.begin_capture();
        // Forced, because a test's stdout is not a tty and `begin_response`
        // would take the buffered path — which is why no replay test has ever
        // exercised the incremental renderer, and where this morning's spacing
        // bug lived. B3c removes the branch and this line with it.
        out.begin_response();
        for step in steps {
            match step {
                Step::Tool(name) => out.display_tool_call(name, &serde_json::json!({})),
                Step::Result(text) => out.display_tool_result(text),
                Step::SegmentEnd => out.end_response_segment(),
                Step::ResponseEnd => out.end_response(),
                other => feed(&out, *other, chunk),
            }
        }
        out.end_capture()
    }

    /// What the markdown renderer alone produces for `text`: the reference for
    /// "the host adds nothing inside a segment".
    fn renderer_alone(text: &str) -> String {
        let mut parser = StreamParser::new_gfm_stream();
        let mut events = parser.write(text.as_bytes());
        events.extend(parser.flush());
        // The same theme production builds the answer renderer with, or its
        // styling would look like host contribution.
        let mut r = Renderer::new(answer_theme(), 80);
        let mut out: Vec<u8> = Vec::new();
        let _ = r.feed(&events, &mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Visible lines of painted output, ANSI stripped, *without* the row the
    /// cursor rests on: a stream ending in `\n` leaves the cursor on a row
    /// nothing was written to, and that row reads as blank.
    fn painted_lines(bytes: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes).replace("\r", "");
        let mut lines: Vec<String> = text
            .split('\n')
            .map(|l| strip_ansi(l).trim_end().to_string())
            .collect();
        if text.ends_with('\n') {
            lines.pop();
        }
        lines
    }

    /// Indices of blank lines, and the longest run of consecutive ones.
    fn blank_runs(lines: &[String]) -> (Vec<usize>, usize) {
        let mut runs: Vec<usize> = Vec::new();
        let mut longest = 0;
        let mut cur = 0;
        for (i, l) in lines.iter().enumerate() {
            if l.trim().is_empty() {
                runs.push(i);
                cur += 1;
                longest = longest.max(cur);
            } else {
                cur = 0;
            }
        }
        (runs, longest)
    }

    /// The core of the rule: no two consecutive blank lines, ever. One blank
    /// at a boundary, and the renderer's own spacing nowhere doubled.
    #[track_caller]
    fn assert_no_doubled_blank(name: &str, steps: &[Step], chunk: Chunking) {
        let bytes = run_script(steps, chunk);
        let lines = painted_lines(&bytes);
        let (_, longest) = blank_runs(&lines);
        assert_eq!(
            longest,
            1,
            "{name} [{}]: {longest} consecutive blank lines\n{:#?}",
            chunk.name(),
            lines
        );
    }

    /// One segment, one answer block: the painted output is the leading blank
    /// from `begin_response` and then *exactly* what the renderer produced.
    /// Anything else is a byte the host contributed on its own.
    #[test]
    #[serial]
    fn host_adds_nothing_inside_a_segment() {
        for text in [
            "A single paragraph answer.\n\n",
            "Two paragraphs.\n\nThe second one.\n\n",
            "A paragraph, then a list:\n\n- one\n- two\n\n",
            "Text with `inline code` and a [link](https://x.example).\n\n",
        ] {
            for chunk in Chunking::all() {
                let bytes = run_script(&[Step::Say(text), Step::ResponseEnd], chunk);
                let painted = String::from_utf8_lossy(&bytes).to_string();
                // `begin_response`'s leading blank, the renderer's own output,
                // and `end_response`'s trailing blank. Nothing else.
                let alone = renderer_alone(text);
                assert_eq!(
                    painted,
                    format!("\n{alone}\n"),
                    "host contributed extra bytes for {text:?} [{}]",
                    chunk.name()
                );
            }
        }
    }

    /// The same answer split across deltas must paint identically to the
    /// one-call version: chunking is not allowed to be visible.
    #[test]
    #[serial]
    fn chunking_is_not_visible() {
        let text = "First paragraph of the answer.\n\nSecond paragraph here.\n\n";
        let whole = run_script(&[Step::Say(text), Step::ResponseEnd], Chunking::Whole);
        for chunk in [Chunking::Bytes, Chunking::Words] {
            assert_eq!(
                run_script(&[Step::Say(text), Step::ResponseEnd], chunk),
                whole,
                "[{}] painted differently from the one-call version",
                chunk.name()
            );
        }
    }

    /// At a segment boundary, exactly one blank line — not zero, not two.
    #[test]
    #[serial]
    fn one_blank_line_at_a_segment_boundary() {
        for chunk in Chunking::all() {
            assert_no_doubled_blank(
                "answer, boundary, answer",
                &[
                    Step::Say("The first answer.\n\n"),
                    Step::SegmentEnd,
                    Step::Say("After the tool.\n\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
        }
    }

    /// §4.1 of the plan: reasoning is routed at the output layer, so a
    /// consumer of `picocode "q" | cat` can drop it, and the render path does
    /// not fork on whether there is a terminal.
    #[test]
    #[serial]
    fn reasoning_is_routed_by_the_output_layer() {
        let script = [Step::Think("Weighing the options.\n\n"), Step::ResponseEnd];

        // No terminal (the default for a test): reasoning leaves stdout — it
        // shows up on the test runner's stderr, which is the divert working.
        let (mut out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.begin_capture();
        out.begin_response();
        feed(&out, script[0], Chunking::Whole);
        feed(&out, script[1], Chunking::Whole);
        let piped = out.end_capture();
        assert!(
            !String::from_utf8_lossy(&piped).contains("Weighing the options"),
            "reasoning must not reach stdout without a terminal"
        );

        // With a terminal: it is on the screen, and counted like any content.
        let (mut out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.reasoning_on_stderr = false;
        out.begin_capture();
        out.begin_response();
        feed(&out, script[0], Chunking::Whole);
        feed(&out, script[1], Chunking::Whole);
        let tty = out.end_capture();
        assert!(
            String::from_utf8_lossy(&tty).contains("Weighing the options"),
            "reasoning belongs on the screen when there is one"
        );
    }

    /// A tool round then the next segment's answer: one blank at the
    /// boundary, with the panel rows above it.
    #[test]
    #[serial]
    fn one_blank_line_after_a_tool_block() {
        for chunk in Chunking::all() {
            assert_no_doubled_blank(
                "tool, boundary, answer",
                &[
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::SegmentEnd,
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
            assert_no_doubled_blank(
                "tool, tool",
                &[
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Tool("Bash"),
                    Step::Result("ok2\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
        }
    }

    /// Reasoning then answer: the boundary blank and the dim reset, in the
    /// order the renderer can accept (a `\n` before the reset would be
    /// swallowed as a block's closing newline).
    #[test]
    #[serial]
    fn reasoning_then_answer_has_one_blank() {
        for chunk in Chunking::all() {
            assert_no_doubled_blank(
                "reasoning then answer",
                &[
                    Step::Think("Weighing the options.\n\n"),
                    Step::Say("The answer is 42.\n\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
        }
    }

    #[test]
    fn wrap_word_boundary() {
        assert_eq!(wrap("hello world", 5), vec!["hello", "world"]);
    }

    #[test]
    fn wrap_long_no_space() {
        assert_eq!(wrap("abcdefghij", 3), vec!["abc", "def", "ghi", "j"]);
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
        assert_eq!(
            wrap("— Show available commands", 100),
            vec!["— Show available commands"]
        );
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
        assert!(
            r[0].starts_with("- "),
            "first line must keep the marker, got {:?}",
            r
        );
        for line in &r[1..] {
            assert!(
                line.starts_with("  "),
                "continuation lines must indent by two columns, got {:?}",
                line
            );
        }
        // And the body content must survive intact (no collapsing).
        let joined: String = r.iter().flat_map(|l| l.chars()).collect();
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
        assert_eq!(
            get_preview("read_file", &args).0,
            "read src/output.rs [101..150]"
        );
        // zero limit => whole file, no range
        let args = serde_json::json!({"path": "src/output.rs", "offset": 0, "limit": 0});
        assert_eq!(get_preview("read_file", &args).0, "read src/output.rs");
        // offset-only (limit 0) => open-ended range
        let args = serde_json::json!({"path": "src/output.rs", "offset": 19, "limit": 0});
        assert_eq!(
            get_preview("read_file", &args).0,
            "read src/output.rs [20..]"
        );
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
        // The refined diff is derived purely from oldText/newText, so it is
        // shown even when the file cannot be read (e.g. during replay).
        assert_eq!(
            diff,
            vec!["@@ -1 +1 @@", "- a", "+ b"],
            "diff should come from old/new: {diff:?}"
        );
    }

    #[test]
    fn get_preview_edit_legacy_single_edit_shape() {
        // The legacy top-level oldText/newText shape (accepted by the edit
        // tool but undocumented) must still produce a diff preview.
        let args = serde_json::json!({
            "path": "/no/such/file.rs",
            "oldText": "a",
            "newText": "b"
        });
        let (label, diff) = get_preview("edit_file", &args);
        assert_eq!(label, "edit /no/such/file.rs");
        // File doesn't exist, so no range, but the diff is still rendered from
        // oldText/newText (this is what guarantees replay shows the edits).
        assert_eq!(
            diff,
            vec!["@@ -1 +1 @@", "- a", "+ b"],
            "diff should come from oldText/newText: {diff:?}"
        );
    }

    #[test]
    fn get_preview_edit_multiple_edits_in_batch() {
        // A multi-edit batch should render a diff for each edit, concatenated
        // directly (the `@@` headers already distinguish the change regions),
        // so a single tool call previews every change it makes.
        let args = serde_json::json!({
            "path": "/no/such/file.rs",
            "edits": [
                {"oldText": "a", "newText": "b"},
                {"oldText": "x", "newText": "y"}
            ]
        });
        let (label, diff) = get_preview("edit_file", &args);
        assert_eq!(label, "edit /no/such/file.rs");
        assert_eq!(
            diff,
            vec!["@@ -1 +1 @@", "- a", "+ b", "@@ -1 +1 @@", "- x", "+ y",],
            "each batch edit should produce its own diff: {diff:?}"
        );
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
            start_line: None,
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
            assert_eq!(
                wrapped,
                vec![line.clone()],
                "indent should survive wrap_with_diff_markers for {:?}",
                line
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refined_diff_lines_shows_changes_without_trailing_newlines() {
        let old_text = "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x);\n}\n";
        let new_text =
            "fn main() {\n    let x = 42;\n    let y = 2;\n    println!(\"{}\", x);\n}\n";
        let lines = refined_diff_lines(old_text, new_text);

        // Hunk header present.
        assert!(
            lines.iter().any(|l| l.contains("@@")),
            "should contain hunk header: {lines:?}"
        );
        // Show the changed lines with marker + space + indented content.
        assert!(
            lines.iter().any(|l| l == "-     let x = 1;"),
            "removed line: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "+     let x = 42;"),
            "added line: {lines:?}"
        );
        // Context lines use a space marker + space then the source indent.
        assert!(
            lines.iter().any(|l| l == "      let y = 2;"),
            "context line: {lines:?}"
        );
        // No line may carry a trailing newline (would split into blank rows).
        for l in &lines {
            assert!(
                !l.ends_with('\n'),
                "line must not end with newline: {:?}",
                l
            );
        }
    }

    #[test]
    fn refined_diff_lines_handles_multiple_hunks() {
        // Changes separated by more than the context radius form distinct
        // hunks (close changes would merge into a single hunk).
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..30 {
            if i == 2 {
                old.push_str("    a = 1;\n");
                new.push_str("    a = 10;\n");
            } else if i == 25 {
                old.push_str("    b = 2;\n");
                new.push_str("    b = 20;\n");
            } else {
                old.push_str(&format!("    line {i};\n"));
                new.push_str(&format!("    line {i};\n"));
            }
        }
        let lines = refined_diff_lines(&old, &new);

        let hunk_count = lines.iter().filter(|l| l.contains("@@")).count();
        assert_eq!(hunk_count, 2, "should have two hunks, got: {lines:?}");
        assert!(lines.iter().any(|l| l == "-     a = 1;"));
        assert!(lines.iter().any(|l| l == "+     a = 10;"));
        assert!(lines.iter().any(|l| l == "-     b = 2;"));
        assert!(lines.iter().any(|l| l == "+     b = 20;"));
    }

    #[test]
    fn refined_diff_lines_no_changes_is_empty() {
        let text = "fn main() {\n    let x = 1;\n}\n";
        let lines = refined_diff_lines(text, text);
        assert!(
            lines.is_empty(),
            "identical text yields no diff lines: {lines:?}"
        );
    }

    #[test]
    fn refined_diff_lines_caps_very_large_replacements() {
        // A huge replacement must be capped and flagged as truncated rather
        // than flooding the output with the entire file.
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..500 {
            old.push_str(&format!("line {i}\n"));
            new.push_str(&format!("line {i} changed\n"));
        }
        let lines = refined_diff_lines(&old, &new);
        assert!(
            lines.len() < 200,
            "should cap huge diffs, got {}",
            lines.len()
        );
        assert!(
            lines.iter().any(|l| l.contains("truncated")),
            "should flag truncation: {lines:?}"
        );
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
            assert_eq!(
                label,
                "grep fn (main|run)\\( /home/gabriel/src/picocode2/src"
            );
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
        assert!(
            !plain.contains('\x1b'),
            "non-terminal path must stay ANSI-free"
        );
        assert!(plain.contains("fn main()"));
    }

    #[test]
    fn render_markdown_heading_and_blockquote_keep_text() {
        let out = render_markdown("# Title\n> quoted", 80, false);
        assert!(!out.contains('\x1b'));
        assert!(out.contains("Title"));
        assert!(out.contains('│') && out.contains("quoted"));
    }

    /// The renderer is the crate's *incremental* `Renderer` + `StreamParser`:
    /// completed blocks are emitted once (so scrollback stays clean) and code
    /// lines stream as they arrive, while an open paragraph is held back until
    /// it closes (coarse-grained). These tests pin that contract.
    #[test]
    fn incremental_renderer_streams_code_lines_and_coarsens_paragraphs() {
        let mut parser = StreamParser::new_gfm_stream();
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
        let mut parser = StreamParser::new_gfm_stream();
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
        assert!(
            tty.contains("\x1b]8;;https://example.com/a\x1b\\"),
            "TTY render must wrap link in OSC 8"
        );
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
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::dimmed(), 80);
        let mut buf: Vec<u8> = Vec::new();
        let _ = write!(buf, "\x1b[2m"); // initial dim (mirrors `ensure_dim(true)`)
        let _ = renderer.feed(
            &parser.write("let me **think** step by step\n".as_bytes()),
            &mut buf,
        );
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
    fn thinking_flushes_on_paragraph_boundary() {
        // Regression test: thinking content should emit on paragraph boundaries
        // (blank line) without waiting for end_response.
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::dimmed(), 80);
        let mut buf: Vec<u8> = Vec::new();
        let _ = write!(buf, "\x1b[2m");

        // First paragraph - no flush yet (no blank line)
        let _ = renderer.feed(&parser.write("\n\n[test] first ".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("\nparagraph \n".as_bytes()), &mut buf);
        let before_flush = String::from_utf8(buf.clone()).unwrap();
        assert!(
            !before_flush.contains("first paragraph"),
            "content held before boundary"
        );

        // Add blank line - should flush
        let _ = renderer.feed(&parser.write("\n".as_bytes()), &mut buf);
        let after_flush = String::from_utf8(buf.clone()).unwrap();
        eprintln!("after newline: {}", after_flush);
        assert!(
            after_flush.contains("first paragraph"),
            "content emitted after boundary"
        );

        // Second paragraph with explicit blank line
        let _ = renderer.feed(&parser.write("second paragraph\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("\n".as_bytes()), &mut buf);
        // let _ = renderer.feed(&parser.write("\n".as_bytes()), &mut buf);
        let after_blank = String::from_utf8(buf.clone()).unwrap();
        assert!(
            after_blank.contains("second paragraph"),
            "content emitted on blank line"
        );

        let _ = write!(buf, "\x1b[0m");
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

    /// The transient status line: painted after each content block, relative
    /// only, and erased by the next content write.
    #[test]
    #[serial]
    fn status_line_is_transient_and_relative() {
        const STATUS: &str = "[sess | openrouter:some/model | ctx 12% · 8k]";
        let (out, _reasoning) = capturing_output(60);
        out.begin_capture();
        out.begin_response();
        out.set_status(STATUS);
        out.stream_text("first block.\n\n");
        out.stream_text("second block.\n\n");
        let bytes = out.end_capture();
        let text = String::from_utf8_lossy(&bytes).replace('\r', "");

        // Painted after each block, so the readout tracks the whole turn.
        assert_eq!(
            text.matches(STATUS).count(),
            2,
            "the status should follow each content block: {text:?}"
        );
        // *Relative*: the status goes out as contiguous text with nothing
        // injected — an absolute write would put a `CUP` in the middle of it.
        // (The erase that follows it is absolute, and lands on the same
        // physical line, which is why this is a containment check.)
        assert!(
            text.contains(STATUS),
            "the status must be written as plain text: {text:?}"
        );
    }

    /// The bar never sits directly against the content: a blank row separates
    /// them.
    ///
    /// A fenced block is the case that shows it, because a fence emits line by
    /// line and its last byte is a single newline — the last line of text sits
    /// directly above where the bar goes. A paragraph cannot show this: the
    /// blank-line rule *swallows* the second newline and records the blank as
    /// owed rather than written, so `"text\n\n"` reaches the screen as one
    /// newline, and the text is above the bar there too.
    #[test]
    #[serial]
    fn the_status_leaves_one_blank_row_above_itself() {
        /// Escapes out: the bar is painted behind a cursor-hide, and the count
        /// of newlines is the whole point.
        fn visible(bytes: &[u8]) -> String {
            strip_ansi(&String::from_utf8_lossy(bytes)).replace('\r', "")
        }

        let (out, _reasoning) = capturing_output(60);
        out.begin_capture();
        out.begin_response();
        out.set_status("[bar]");
        out.stream_text("```\nalpha\n```\n");
        let text = visible(&out.end_capture());
        assert!(
            text.contains("alpha\n\n[bar]"),
            "one blank row between the last line of content and the bar: {text:?}"
        );
        assert!(!text.contains("alpha\n\n\n[bar]"), "and only one: {text:?}");
    }

    /// The status **reuses** a blank that is already on screen instead of
    /// stacking a second one on top of it.
    ///
    /// `ends_blank()` is the blank-line rule's own bit, and it is the right
    /// signal precisely because the rule swallows the second newline: a blank
    /// line exists on screen only once a second `\n` has really been written, so
    /// the bit flips exactly when a row above the bar is genuinely empty. The
    /// two features used to keep separate opinions and the screen showed it — a
    /// boundary blank and a gap of the bar's own are two different rows, so a
    /// boundary followed by a bar left *two* blank lines where one was meant.
    ///
    /// The blank is written raw, because that is the only way to get two
    /// consecutive newlines onto the screen: every high-level entry point routes
    /// through the rule, which eats the second one.
    #[test]
    #[serial]
    fn the_status_reuses_a_blank_that_is_already_on_screen() {
        let (out, _reasoning) = capturing_output(60);
        out.begin_capture();
        out.begin_response();
        out.set_status("[bar]");
        out.write_content(b"alpha\n");
        out.write_content(b"\n"); // the blank line, really written
        let text = String::from_utf8_lossy(&out.end_capture()).replace('\r', "");
        assert!(
            text.contains("alpha\n\n[bar]"),
            "the blank already on screen is the gap: {text:?}"
        );
        assert!(
            !text.contains("alpha\n\n\n[bar]"),
            "the bar must not add a second one: {text:?}"
        );
    }

    /// The erase has to cover the gap as well as the bar, and it has to leave the
    /// cursor where the next block writes.
    ///
    /// The gap is the status's own row when it opened one, and the next content
    /// write lands *there* — so an erase that only covered the bar would leave
    /// the blank line behind, once per block, for the rest of the session.
    #[test]
    #[serial]
    fn erasing_the_status_also_erases_its_gap() {
        let (out, _reasoning) = capturing_output(60);
        out.begin_capture();
        out.begin_response();
        out.set_status("[bar]");
        out.stream_text("```\nalpha\n```\n");
        out.stream_text("```\nbeta\n```\n");
        let text = String::from_utf8_lossy(&out.end_capture()).into_owned();
        // The erase walks up over the bar and the gap: two erase-in-line, and
        // the `CUU` is what makes it a walk rather than a single row.
        assert_eq!(
            text.matches("\x1b[K").count(),
            2,
            "the bar and its gap, one erase each: {text:?}"
        );
        assert_eq!(
            text.matches("\x1b[1A").count(),
            1,
            "one row up, from the bar to the gap: {text:?}"
        );
    }

    /// The bar must put back the dim its own reset clears.
    ///
    /// `ensure_dim` emits `\x1b[2m` / `\x1b[0m` only when the tracked `dim_on`
    /// *changes*, so a bar written mid-reasoning used to break it in both
    /// directions: the bar inherited the terminal's dim (a colour is not an
    /// attribute, so `38;5;245m` did not clear it), and its trailing reset then
    /// silenced the *next* reasoning write — which saw no change to re-assert.
    /// So the bar looked dimmed or not depending on what was above it, and the
    /// reasoning after it flickered back to full intensity.
    ///
    /// The flag is set directly rather than by streaming reasoning: the
    /// reasoning goes to its own sink, which this harness captures apart, and
    /// the contract under test is about the flag, not about the stream that
    /// sets it.
    #[test]
    #[serial]
    fn the_status_bar_puts_back_the_dim_it_clears() {
        let (out, _reasoning) = capturing_output(60);
        *out.dim_on.lock().unwrap() = true; // mid-reasoning
        out.begin_capture();
        out.set_status("\x1b[0m\x1b[48;5;237mdim-test\x1b[0m");
        // Painted directly rather than through a content write: the answer
        // renderer turns dim *off* before it writes, so a content write can
        // never leave the flag on for the status to find. Reasoning can, and
        // that is the case under test.
        out.paint_status();
        let bytes = out.end_capture();
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        let last_bar = text.rfind("dim-test").expect("the bar should be painted");
        let after = &text[last_bar..];
        let reset = after.find("\x1b[0m").expect("the bar resets");
        assert!(
            // reset, CR, then the dim back — and only then the cursor hide,
            // which is about the cursor rather than the colours.
            after[reset..].starts_with("\x1b[0m\r\x1b[2m\x1b[?25l"),
            "the dim must come straight back after the bar's reset: {after:?}"
        );
        // And the flag is untouched — the bar restores, it does not toggle.
        assert!(
            *out.dim_on.lock().unwrap(),
            "the tracked dim should still be on"
        );
    }

    /// The cursor is hidden while the bar is up and shown when the input frame
    /// comes back — a block cursor parked at column 0 of a dark full-width bar is
    /// the brightest thing on the screen, and there is nothing to type into
    /// mid-turn.
    #[test]
    #[serial]
    fn the_bar_hides_the_cursor_and_a_frame_shows_it_again() {
        let (out, _reasoning) = capturing_output(60);
        out.begin_capture();
        out.begin_response();
        out.set_status("bar");
        out.stream_text("a block\n\n");
        out.draw_frame(
            &crate::repl::Repl::compose_frame(
                &crate::repl::LineEditor::new(),
                "",
                None,
                &[],
                0,
                60,
                20,
            ),
            crate::ui::frame::ApplyPolicy::Frame,
            (60, 20),
        );
        let bytes = out.end_capture();
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        let hide = text.find("\x1b[?25l").expect("the bar should hide it");
        let show = text.find("\x1b[?25h").expect("the frame should show it");
        assert!(hide < show, "hide then show, in that order: {text:?}");
    }

    /// The next content write erases the status, so it never survives as a line
    /// of its own in the scrollback.
    #[test]
    #[serial]
    fn the_next_content_write_erases_the_status() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.set_status("[sess | m | ctx 5%]");
        out.stream_text("alpha.\n\n");
        out.stream_text("beta.\n\n");
        let bytes = out.end_capture();
        let mut p = vt100::Parser::new(24, 40, 1000);
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        let rows: Vec<String> = (0..12)
            .map(|r| {
                (0..40)
                    .map(|c| {
                        p.screen()
                            .cell(r, c)
                            .and_then(|x| x.contents().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        // The status sits on the last written row, and no earlier row repeats it.
        assert!(
            rows.iter().any(|r| r.contains("[sess |")),
            "the status should be on screen: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains("[sess |")).count(),
            1,
            "exactly one status line, not one per block: {rows:?}"
        );
    }

    /// A status only goes out at the start of a line, so this is asserted
    /// where the fact lives — the tail the counter maintains. (No current
    /// writer can produce a mid-line tail: every content write ends with a
    /// newline. It is the guard for a writer that does not, and for the status
    /// itself, which by design has no trailing newline.)
    #[test]
    fn the_tail_reports_whether_the_cursor_is_at_a_line_start() {
        let tail = crate::row_count::StreamTail::new();
        let mut counter = RowCounter::new(Vec::new(), 40, &tail);
        counter.write_all(b"alpha").unwrap();
        assert!(!tail.at_line_start(), "mid-line: the status must wait");
        counter.write_all(b"\n").unwrap();
        assert!(tail.at_line_start());
    }

    /// The status never lands among a frame's rows. A frame owns the tail, so
    /// the first content write suppresses the status and the next one paints it
    /// *below* the content — the frame's region is already erased by then.
    #[test]
    #[serial]
    fn status_lands_below_a_frame_rather_than_among_its_rows() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.set_status("[sess | m]");
        let frame = crate::repl::Repl::compose_frame(
            &crate::repl::LineEditor::new(),
            "",
            None,
            &[],
            0,
            40,
            10,
        );
        out.draw_frame(&frame, ApplyPolicy::Frame, (40, 24));
        out.display_text("a message");
        out.display_plain("more content");
        let bytes = out.end_capture();

        let mut p = vt100::Parser::new(24, 40, 1000);
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        let rows: Vec<String> = (0..12)
            .map(|r| {
                (0..40)
                    .map(|c| {
                        p.screen()
                            .cell(r, c)
                            .and_then(|x| x.contents().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        let status_row = rows
            .iter()
            .position(|r| r.contains("[sess |"))
            .unwrap_or_else(|| panic!("the status should be on screen: {rows:?}"));
        let content_row = rows
            .iter()
            .position(|r| r.contains("more content"))
            .unwrap_or_else(|| panic!("the content should be on screen: {rows:?}"));
        assert!(
            status_row > content_row,
            "the status belongs below the content, not among the frame's rows: {rows:?}"
        );
    }

    /// Content written while a frame is live must not leave any of it behind
    /// (ui_draw_v2 §1.1). This is the reported resume artefact: a menu's table
    /// header and rows left on screen *underneath* replayed content, because the
    /// content started at a cursor inside the still-live frame and only
    /// overwrote the rows below it.
    #[test]
    #[serial]
    fn content_over_a_live_frame_erases_the_frame() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        // A menu frame, as `/session` has open when a commit happens: the
        // input, a divider, and overlay rows — the table header is what the
        // report saw survive.
        let overlay = vec![
            "MODEL            CTX    PRICE".to_string(),
            "  gpt-5          400k   1.00".to_string(),
            "  claude-opus    200k   3.00".to_string(),
        ];
        let frame = crate::repl::Repl::compose_frame(
            &crate::repl::LineEditor::new(),
            "",
            None,
            &overlay,
            0,
            40,
            10,
        );
        out.draw_frame(&frame, ApplyPolicy::Frame, (40, 24));
        out.display_text("Cannot start agent: Missing API key for provider.");
        let bytes = out.end_capture();

        let mut p = vt100::Parser::new(24, 40, 1000);
        // The capture starts at the terminal's first row, but the frame apply is
        // relative to the cursor (row_accounting §6b) and is told the anchor is
        // row 3 — which in production is where a run of content lines left the
        // cursor. Stand those two lines in before replaying.
        p.process(b"\r\n\r\n");
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        let rows: Vec<String> = (0..10)
            .map(|r| {
                (0..40)
                    .map(|c| {
                        p.screen()
                            .cell(r, c)
                            .and_then(|x| x.contents().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect()
            })
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("Missing API key")),
            "the message should be on screen: {rows:?}"
        );
        for leftover in ["MODEL", "CTX", "gpt-5", "claude-opus", "PRICE"] {
            assert!(
                !rows.iter().any(|r| r.contains(leftover)),
                "frame row {leftover:?} survived the content write: {rows:?}"
            );
        }
    }

    /// The clear only applies when a frame is live: streamed blocks just
    /// append, so the tracked row stays in step with the screen and a
    /// scrolled-back viewport is never written into.
    #[test]
    #[serial]
    fn content_without_a_live_frame_is_untouched() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.stream_text("a streamed paragraph.\n\n");
        out.stream_text("another one.\n\n");
        out.end_response();
        let bytes = out.end_capture();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("\x1b[K"),
            "nothing to clear while streaming: {:?}",
            String::from_utf8_lossy(&bytes).escape_debug().to_string()
        );
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    /// A tool block followed by reasoning showed **two** blank lines: the host
    /// wrote the boundary blank, and the long-lived thinking renderer — which
    /// remembered that it had written something — opened its next block with a
    /// gap of its own. `end_tool_block` resets the renderers, so the host's
    /// blank is the only one. Reasoning *before* the tool is what leaves the
    /// renderer mid-document; without it the gap never fires and the bug hides,
    /// which is why this case exists.
    #[test]
    #[serial]
    fn one_blank_line_between_a_tool_block_and_reasoning() {
        for (name, steps) in [
            (
                "tool, result, say, end",
                vec![
                    Step::Say("First.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
            (
                "tool, result, think, say, end",
                vec![
                    Step::Say("First.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Think("Weighing again.\n\n"),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
            (
                "tool, result, PENDING think, say, end",
                vec![
                    Step::Say("First.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    // No trailing newlines: the paragraph is still open when
                    // the answer arrives, so it is committed at the boundary.
                    Step::Think("Weighing again."),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
            (
                "think, say, tool, result, think, say, end",
                vec![
                    // Reasoning *before* the tool: the thinking renderer has
                    // written something, so it will want a leading gap for the
                    // next block it renders.
                    Step::Think("Before the tool.\n\n"),
                    Step::Say("Running it.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Think("After the tool.\n\n"),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
        ] {
            for chunk in Chunking::all() {
                assert_no_doubled_blank(name, &steps, chunk);
            }
        }
    }

    #[test]
    #[serial]
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
        let _ = thinking_renderer.feed(
            &thinking_parser.write(b"Plan:\n- step one\n- step two\n"),
            &mut out,
        );
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
        assert!(
            list_idx < ans_idx,
            "reasoning (list) must render BEFORE the answer"
        );

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
    fn tmp_verify_tool_rendering_helpers() {
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
        assert!(
            plain.contains("let x = 1;"),
            "highlighting must not drop content; got plain={plain:?} from colored={colored:?}"
        );
        assert!(colored.contains('\x1b'), "color path must emit ANSI");
        // non-color path returns the line untouched
        assert_eq!(highlight_code_fg("let x = 1;", "rust", false), "let x = 1;");
    }

    #[test]
    #[serial_test::serial]
    fn edit_arg_error_pipes_align_in_the_rendered_panel() {
        // Regression. The renderer emits correct padding, but the panel pipeline
        // re-wraps lines through `wrap()`, which collapses interior runs of
        // spaces — so a padding-based table rendered fine in unit tests and came
        // out ragged on screen. Assert on the *rendered* columns, not on the
        // renderer's output, or this slips through again.
        let msg = "failed to parse tool arguments: Invalid edit tool arguments: \
Invalid block: an edit block is missing its `@@@NEW` section\n\
\n\
Expected:\nsome/path:1234\n@@@OLD\nold text\n@@@NEW\nnew text\n@@@END\n\
\n\
Actual:\nsrc/output.rs\n@@@OLD\n… (5 lines elided) …\n@@@NEW\nself.draw.lock().unwrap().anchor()\n";
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.display_tool_call("edit", &serde_json::json!({"path": "src/x.rs"}));
        out.begin_capture_inner();
        out.display_tool_result(msg);
        let bytes = out.end_capture_inner();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));

        let columns: Vec<usize> = text
            .lines()
            .filter(|l| l.contains('|'))
            .map(|l| l.find('|').unwrap())
            .collect();
        assert!(columns.len() >= 6, "expected a table, got: {text}");
        let first = columns[0];
        assert!(
            columns.iter().all(|c| *c == first),
            "pipes at {columns:?} should all be at {first}:\n{text}"
        );
        // Still a table, and still the two markers that differ on one row.
        let row = text
            .lines()
            .find(|l| l.contains('|') && l.contains("@@@NEW"))
            .unwrap();
        assert!(
            text.contains("Expected") && text.contains("Actual"),
            "{text}"
        );
        let _ = row;
    }

    #[test]
    fn tmp_repro_separator_blanks() {
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response();
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
        out.begin_response();
        out.begin_capture_inner();
        out.stream_text("All done.\n");
        out.end_response();
        out.display_separator();
        let b = out.end_capture_inner();
        let b = strip_ansi(&String::from_utf8_lossy(&b));
        eprintln!("=== Case B (text end) ===\n{:?}", b);

        // Case C: turn ends with trailing thinking after a tool result.
        out.begin_response();
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

    #[test]
    fn tool_result_followed_by_text_has_blank_separator() {
        // Regression (replay path): render_turn must fire `end_response_segment`
        // after a tool block when more text follows, so replay output matches
        // the live path (where each CompletionCall flushes) — the trailing text
        // is separated from the tool-result line by a blank line.
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response();
        out.begin_capture_inner();
        out.stream_text("Let me edit.\n");
        out.end_response_segment();
        out.display_tool_call(
            "edit_file",
            &serde_json::json!({"path": "/no/such.rs", "oldText": "a", "newText": "b"}),
        );
        out.display_tool_result("\"Replaced 1 block(s) in ./no_such.rs\"");
        // The fix: segment boundary after the tool block (mirrors live path).
        out.end_response_segment();
        out.stream_text("Done.\n");
        out.end_response();
        let d = out.end_capture_inner();
        let d = strip_ansi(&String::from_utf8_lossy(&d));
        let after = d.find("Replaced 1 block(s) in ./no_such.rs").unwrap()
            + "Replaced 1 block(s) in ./no_such.rs".len();
        let between = &d[after..d.find("Done.").unwrap()];
        assert!(
            between.contains("\n\n"),
            "expected a blank line between the tool result and trailing text, got {:?}",
            between
        );
    }

    #[test]
    fn write_tool_renders_content_like_read() {
        // Write tool result should render with path header and syntax-highlighted
        // content preview (first 10 lines), matching read_file's rendering.
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response();
        out.begin_capture_inner();

        // Simulate a write tool call with structured result (as write_file returns)
        out.display_tool_call(
            "write_file",
            &serde_json::json!({"path": "test.rs", "content": "fn main() {}"}),
        );
        let result = r#"{"path":"./test.rs","content":"fn main() {\n    let x = 42;\n    println!(\"Hello, world! {}\", x);\n}","lines":4,"bytes":72}"#;
        out.display_tool_result(result);

        out.end_response();
        let captured = out.end_capture_inner();
        let output = strip_ansi(&String::from_utf8_lossy(&captured));

        // Should contain the path header with line count
        assert!(
            output.contains("write ./test.rs [4 lines]"),
            "missing path header with line count: {}",
            output
        );
        // Should contain syntax-highlighted content (first 10 lines)
        assert!(output.contains("fn main()"), "missing fn main: {}", output);
        assert!(
            output.contains("let x = 42"),
            "missing let x = 42: {}",
            output
        );
        assert!(output.contains("println!"), "missing println!: {}", output);
        // Should NOT contain line numbers (unlike read_file which has them in raw result but stripped for display)
        assert!(
            !output.contains("|"),
            "should not contain line number gutter: {}",
            output
        );
    }
}
