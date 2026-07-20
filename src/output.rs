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
use termimad;
use markdown_stream::{Parser, StreamParser};
use markdown_terminal::{Renderer, Theme};
use libc;
use crossterm::event::{self, Event, KeyCode};
use crate::repl::Repl;
use crate::settings::{ShowReasoning, ShowReasoningHandle};

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
    // ---- Streaming (Phase 2+) ----
    // Default no-ops so existing impls keep compiling without change.
    // `ConsoleOutput` overrides these to render incrementally (Phase 4 adds
    // live in-place redraw; Phase 2 uses a render-once-per-segment buffer).
    /// Called once when an agent response begins streaming.
    fn begin_response(&self) {}
    /// A delta of assistant text.
    fn stream_text(&self, _delta: &str) {}
    /// A delta of model reasoning/thinking (already gated by level upstream).
    fn stream_thinking(&self, _delta: &str) {}
    /// Commit the current text/thinking segment before a static line (tool
    /// call, tool result, completion call) is printed.
    fn end_response_segment(&self) {}
    /// Called when the whole response finishes; commit the final live block.
    fn end_response(&self) {}
    /// Enable/disable *answer holding*: when enabled (a reasoning-capable
    /// turn), the assistant answer text is buffered and only emitted once the
    /// turn ends, so that reasoning arriving *after* the answer is still shown
    /// above it. Default no-op for outputs that don't stream incrementally.
    fn set_answer_hold(&self, _hold: bool) {}
    fn display_tool_call(&self, name: &str, args: &Value);
    fn display_tool_result(&self, result: &str);
    fn get_user_input(&self) -> String;
    fn display_error(&self, error: &str);
    fn display_system(&self, text: &str);
    fn confirm(&self, message: &str) -> Confirmation;
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

/// Wrap `text` to `width` columns.
///
/// Tokenizes into maximal non-whitespace "words", collapsing any run of
/// whitespace between them to a single space. A word longer than `width` is
/// hard-split into `width`-sized character chunks (CJK-safe). Logical lines
/// (split on `\n`) are wrapped independently, and blank lines are preserved.
///
/// Unlike a grapheme-boundary splitter, this keeps trailing punctuation
/// attached to its word (e.g. `"Commands:"` stays `"Commands:"`, not
/// `"Commands :"`) and never inserts a space adjacent to existing whitespace
/// (so `"a  b"` reflows to `"a b"`).
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.lines().map(|l| l.to_string()).collect();
    }
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for logical in text.split('\n') {
        if logical.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let words: Vec<&str> = logical.split_whitespace().collect();
        let mut line = String::new();
        for word in words {
            if word.chars().count() > width {
                // Flush any pending line, then hard-split the long word.
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                let chunks: Vec<String> = word
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(width)
                    .map(|c| c.iter().collect())
                    .collect();
                for c in &chunks[..chunks.len().saturating_sub(1)] {
                    out.push(c.clone());
                }
                line = chunks.last().cloned().unwrap_or_default();
                continue;
            }
            if line.is_empty() {
                line.push_str(word);
            } else if line.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
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

fn get_preview(args: &Value) -> String {
    let s = if let Some(obj) = args.as_object() {
        obj.values()
            .next()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                _ => v.to_string(),
            })
            .unwrap_or_default()
    } else {
        args.to_string()
    };
    truncate(&s.replace('\n', " "), 50)
}

impl ConsoleOutput {
    pub fn new(show_reasoning: ShowReasoningHandle) -> Self {
        Self {
            spinner: Mutex::new(None),
            row: Mutex::new(1),
            reader_paused: Arc::new(AtomicBool::new(false)),
            text_buffer: Mutex::new(String::new()),
            live: Mutex::new(false),
            parser: Mutex::new(StreamParser::new_gfm()),
            renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            thinking_parser: Mutex::new(StreamParser::new_gfm()),
            thinking_renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            dim_on: Mutex::new(false),
            answer_held: Mutex::new(false),
            answer_buffer: Mutex::new(String::new()),
            show_reasoning,
        }
    }

    /// Toggle the global dim SGR on stdout to match `on`, emitting the escape
    /// only when the state actually changes. Used so streamed thinking reads
    /// faded without re-rendering any already-emitted content.
    fn ensure_dim(&self, on: bool, out: &mut io::Stdout) {
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
            println!();
            self.note_lines(1);
        } else {
            *self.text_buffer.lock().unwrap() = String::new();
        }
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
    fn commit_thinking(&self, out: &mut io::Stdout) {
        let events = {
            let mut tparser = self.thinking_parser.lock().unwrap();
            let events = tparser.flush();
            *tparser = StreamParser::new_gfm();
            events
        };
        let mut trenderer = self.thinking_renderer.lock().unwrap();
        let _ = trenderer.feed(&events, out);
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
        let mut out = std::io::stdout();
        // Commit the (dimmed) thinking paragraph first, mirroring the
        // thinking→answer boundary handling in `stream_text`.
        let was_thinking = *self.dim_on.lock().unwrap();
        if was_thinking {
            self.commit_thinking(&mut out);
        }
        self.ensure_dim(false, &mut out);
        // Render the answer with a local parser so `self.parser` is left empty
        // for the caller's renderer reset. `flush()` (not just `write`) so the
        // answer's final, blank-line-unterminated paragraph is emitted rather
        // than dropped (a `write`-only parser holds the last block back).
        let mut local_parser = StreamParser::new_gfm();
        let mut events = local_parser.write(buffer.as_bytes());
        events.extend(local_parser.flush());
        let mut renderer = self.renderer.lock().unwrap();
        let _ = renderer.feed(&events, &mut out);
        println!();
        self.note_lines(1);
        true
    }

    fn separator() {
        let width = term_width();
        println!("{}", style("─".repeat(width)).dim());
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
        // Count *visible* lines only; `text` may carry ANSI color escapes
        // that must not be treated as visible width (or the cursor-row
        // tracker over-advances and wraps to the top of the screen).
        let w = term_width();
        let n = if text.is_empty() {
            1
        } else {
            wrap(&strip_ansi(text), w).len().max(1)
        };
        println!("{}", text);
        self.note_lines(n);
    }

    fn display_markdown(&self, text: &str) {
        self.stop_thinking();
        let color = std::io::stdout().is_terminal();
        let rendered = render_markdown(text, term_width(), color);
        println!();
        print!("{}", rendered);
        println!();
        // +2 for the leading/trailing blank lines; the DSR query corrects drift.
        let n = rendered.lines().count().max(1) + 2;
        self.note_lines(n);
    }

    fn display_text(&self, text: &str) {
        self.stop_thinking();
        println!();
        print!("{} ", style("⏺").cyan());
        termimad::print_inline(text);
        println!();
        let n = wrap(text, term_width()).len() + 1;
        self.note_lines(n);
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
        let mut out = std::io::stdout();
        let was_thinking = *self.dim_on.lock().unwrap();
        if was_thinking {
            // Thinking just ended: normalize the thinking→answer boundary to a
            // single paragraph gap. Commit all still-buffered thinking (while dim
            // is still on) and emit one blank line, so the answer starts on a
            // fresh paragraph regardless of whether a tool call intervened in
            // between (the tool path already prints its own gap).
            self.commit_thinking(&mut out);
            self.ensure_dim(false, &mut out);
            println!();
            self.note_lines(1);
        } else {
            self.ensure_dim(false, &mut out);
        }
        let mut parser = self.parser.lock().unwrap();
        let events = parser.write(delta.as_bytes());
        let mut renderer = self.renderer.lock().unwrap();
        let _ = renderer.feed(&events, &mut out);
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
        let mut out = std::io::stdout();
        self.ensure_dim(true, &mut out);
        let mut parser = self.thinking_parser.lock().unwrap();
        let events = parser.write(delta.as_bytes());
        let mut renderer = self.thinking_renderer.lock().unwrap();
        let _ = renderer.feed(&events, &mut out);
    }

    fn end_response_segment(&self) {
        self.stop_thinking();
        let live = *self.live.lock().unwrap();
        if live {
            let mut out = std::io::stdout();
            // Flush any answer held back while reasoning was streaming. When it
            // flushes, it has already committed the thinking paragraph and
            // printed the gap, so we skip that part below.
            let flushed = self.flush_held_answer();
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
                let _ = renderer.feed(&events, &mut out);
                let mut theme = Theme::default();
                theme.clickable_links = true; // real TTY path
                *renderer = Renderer::new(theme, term_width());
            }
            if !flushed {
                // Commit thinking (the thinking renderer is kept — post-tool
                // thinking continues as one continuous faded stream; only the
                // parser is reset, inside `commit_thinking`).
                self.commit_thinking(&mut out);
                self.ensure_dim(false, &mut out);
                println!();
                self.note_lines(1);
            }
        } else {
            self.flush_text();
        }
    }

    fn end_response(&self) {
        self.stop_thinking();
        let live = *self.live.lock().unwrap();
        if live {
            let mut out = std::io::stdout();
            // Flush any answer held back while reasoning was streaming. When it
            // flushes, the answer and thinking are already finalized below, so
            // skip the redundant finalize.
            let flushed = self.flush_held_answer();
            if !flushed {
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
                println!();
                self.note_lines(1);
            }
        } else {
            self.flush_text();
            // Non-TTY thinking already streamed to stderr; stdout stays clean.
        }
    }

    fn display_tool_call(&self, name: &str, args: &Value) {
        self.stop_thinking();
        let preview = get_preview(args);
        let capitalized_name = name
            .chars()
            .next()
            .map(|c| c.to_uppercase().collect::<String>() + &name[1..])
            .unwrap_or_else(|| name.to_string());
        println!(
            "\n{} {}({})",
            style("⏺").green(),
            style(capitalized_name).bold(),
            style(preview).dim()
        );
        self.note_lines(2);
    }

    fn display_tool_result(&self, result: &str) {
        self.stop_thinking();

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
            println!("  {}  {}", style("└").dim(), style("(empty)").dim());
            self.note_lines(1);
            return;
        }

        let show_max = if is_error { usize::MAX } else { 4 };
        let wrap_width = term_width().saturating_sub(4); // account for "  │  " prefix
        let mut printed = 0;
        let mut printed_count = 0;
        for (i, line) in lines.iter().enumerate() {
            if printed >= show_max {
                break;
            }
            let symbol = if i == lines.len() - 1 && lines.len() <= show_max {
                "└"
            } else {
                "│"
            };
            if is_error {
                println!("  {}  {}", style(symbol).dim(), style(line.to_string()).red());
                printed_count += 1;
            } else {
                let wrapped = if wrap_width == 0 {
                    vec![line.to_string()]
                } else {
                    wrap(line, wrap_width)
                };
                for (j, wline) in wrapped.into_iter().enumerate() {
                    let sym = if j == 0 { symbol } else { " " };
                    println!("  {}  {}", style(sym).dim(), style(wline).dim());
                    printed_count += 1;
                }
            }
            printed += 1;
        }

        if lines.len() > show_max {
            println!(
                "  {}  {}",
                style("└").dim(),
                style(format!("... +{} lines", lines.len() - show_max)).dim()
            );
            printed_count += 1;
        }
        self.note_lines(printed_count.max(1));
    }

    fn get_user_input(&self) -> String {
        self.stop_thinking();
        use std::io::{self, Write};
        print!("{} ", style("❯").bold().blue());
        let _ = io::stdout().flush();
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        input.trim().to_string()
    }

    fn confirm_gate(&self) -> Option<Arc<AtomicBool>> {
        Some(self.reader_paused.clone())
    }

    fn display_error(&self, error: &str) {
        self.stop_thinking();
        println!("{} Error: {}", style("⏺").red(), error);
        self.note_lines(1);
    }

    fn display_system(&self, text: &str) {
        self.stop_thinking();
        println!("{}", style(text).bold().dim());
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
            let panel = Repl::overlay_lines(message, &options, sel, None, true);
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

    fn display_separator(&self) {
        self.stop_thinking();
        Self::separator();
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

        println!();
        for line in avatar {
            let padding = width.saturating_sub(15) / 2;
            println!("{}{}", " ".repeat(padding), style(line).cyan());
        }
        println!();

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

        print!(
            "{} | {} ({})",
            style("picocode").bold(),
            style(provider).cyan(),
            style(model).blue(),
        );

        if let Some(p) = persona {
            print!(" | {}", style(p).magenta());
        }

        println!(
            " | {} | {} | {}",
            status(yolo, "yolo", |s| s.red()),
            style(format!("limit:{}", limit)).yellow(),
            style(cwd).dim()
        );
        self.note_lines(9);
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
        assert_eq!(wrap("   ", 10), vec![""]);
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
    fn wrap_preserves_alignment_free_help() {
        // `wrap` reflows: leading/padding whitespace is collapsed to single
        // spaces, punctuation stays attached, em-dash gets one space.
        let help = "  /help      — Show available commands\n  /model      — Select a model";
        let r = wrap(help, 100);
        assert_eq!(r, vec!["/help — Show available commands", "/model — Select a model"]);
    }

    #[test]
    fn truncate_multibyte_no_panic() {
        assert_eq!(truncate("hello", 3), "hel...");
        assert_eq!(truncate("hi", 5), "hi");
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
    fn reasoning_with_trailing_list_renders_before_answer() {
        // Regression for the reasoning-below-answer bug (see
        // `doc/reasoning_order_opus.md` §4). A GFM list cannot be closed by a
        // blank line — the streaming parser must see a non-list line or EOF —
        // so committing the thinking parser with `write(b"\n\n")` stranded
        // list-structured reasoning until `end_response`'s `flush()`, which runs
        // *after* the answer parser, printing the reasoning below the answer.
        // The fix commits thinking with `flush()` (`commit_thinking`). Here we
        // capture stdout at the fd level and assert the reasoning list item is
        // emitted BEFORE the answer text.
        use std::io::Write;
        use std::os::unix::io::AsRawFd;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let saved = unsafe { libc::dup(1) };
        unsafe { libc::dup2(tmp.as_file().as_raw_fd(), 1) };

        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response_inner(true); // live/TTY path
        out.set_answer_hold(false); // NOHOLD: reasoning renders inline
        // Markers are unique to this test: the fd-level redirect is process-wide,
        // so parallel tests' stdout lands in the same capture file — a shared
        // marker (e.g. "ANSWER") could false-match another test's output.
        // Reasoning that ENDS in a bullet list (the stranding trigger):
        out.stream_thinking("Plan:\n- step PICOCODE_RTLIST_MARK one\n- step two");
        // Then the answer text:
        out.stream_text("PICOCODE_RTANSWER_MARK done.");
        out.end_response();

        let _ = std::io::stdout().flush();
        unsafe {
            libc::dup2(saved, 1);
            libc::close(saved);
        }

        let captured = std::fs::read_to_string(tmp.path()).unwrap();
        let plain = strip_ansi(&captured);
        let list_pos = plain
            .find("PICOCODE_RTLIST_MARK")
            .unwrap_or_else(|| panic!("reasoning list item must be rendered; got:\n{plain}"));
        let ans_pos = plain
            .find("PICOCODE_RTANSWER_MARK")
            .unwrap_or_else(|| panic!("answer must be rendered; got:\n{plain}"));
        assert!(
            list_pos < ans_pos,
            "reasoning (trailing list) must render BEFORE the answer; got:\n{plain}"
        );
    }
}
