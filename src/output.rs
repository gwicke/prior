use console::{style, StyledObject};
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::io::IsTerminal;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use termimad;
use markdown_terminal::Theme;
use libc;

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
}

pub struct QuietOutput {
    spinner: Mutex<Option<ProgressBar>>,
}

impl QuietOutput {
    pub fn new() -> Self {
        Self {
            spinner: Mutex::new(None),
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
    /// Buffered assistant text while a response streams (Phase 2: flushed
    /// via `display_markdown` at segment/response boundaries; Phase 4 switches
    /// to a live in-place renderer).
    text_buffer: Mutex<String>,
    /// Buffered reasoning/thinking while a response streams (surfaced at
    /// `end_response`; relocated into a dedicated panel in Phase 5).
    thinking_buffer: Mutex<String>,
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
    let theme = if color { Theme::default() } else { Theme::no_color() };
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
    pub fn new() -> Self {
        Self {
            spinner: Mutex::new(None),
            row: Mutex::new(1),
            text_buffer: Mutex::new(String::new()),
            thinking_buffer: Mutex::new(String::new()),
        }
    }

    /// Render and clear the buffered assistant text (Phase 2: render-once per
    /// segment). Phase 4 will replace this with a live in-place redraw.
    fn flush_text(&self) {
        let text = std::mem::take(&mut *self.text_buffer.lock().unwrap());
        if !text.is_empty() {
            self.display_markdown(&text);
        }
    }
}

impl Default for ConsoleOutput {
    fn default() -> Self {
        Self::new()
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
        *self.text_buffer.lock().unwrap() = String::new();
        *self.thinking_buffer.lock().unwrap() = String::new();
        self.stop_thinking();
    }

    fn stream_text(&self, delta: &str) {
        self.text_buffer.lock().unwrap().push_str(delta);
    }

    fn stream_thinking(&self, delta: &str) {
        self.thinking_buffer.lock().unwrap().push_str(delta);
    }

    fn end_response_segment(&self) {
        self.stop_thinking();
        self.flush_text();
    }

    fn end_response(&self) {
        self.stop_thinking();
        self.flush_text();
        let thinking = std::mem::take(&mut *self.thinking_buffer.lock().unwrap());
        if !thinking.is_empty() {
            // Interim surfacing (Phase 5 relocates this into a collapsible
            // panel above the answer). `display_text` keeps it plain so it is
            // not re-interpreted as markdown.
            self.display_text(&format!("Reasoning:\n{}", thinking));
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
        println!("\n{} {} [y/n/s]", style("⚠").yellow(), message);
        println!(
            "  {}es / {}o / {}ession",
            style("y").bold(),
            style("n").bold(),
            style("s").bold()
        );
        self.note_lines(2);
        let input = self.get_user_input().to_lowercase();
        match input.as_str() {
            "y" | "yes" => Confirmation::Yes,
            "s" | "session" => Confirmation::Always,
            _ => Confirmation::No,
        }
    }

    fn display_separator(&self) {
        self.stop_thinking();
        Self::separator();
        self.note_lines(1);
    }

    fn display_thinking(&self, message: &str) {
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
        let out = ConsoleOutput::new();
        out.begin_response();
        assert_eq!(out.text_buffer.lock().unwrap().clone(), "");
        assert_eq!(out.thinking_buffer.lock().unwrap().clone(), "");

        out.stream_text("hello ");
        out.stream_text("world");
        assert_eq!(out.text_buffer.lock().unwrap().clone(), "hello world");

        // A tool-call boundary commits (flushes + clears) the text buffer so
        // the next static line prints below the committed text.
        out.end_response_segment();
        assert_eq!(out.text_buffer.lock().unwrap().clone(), "");

        out.stream_thinking("let me think");
        assert_eq!(out.thinking_buffer.lock().unwrap().clone(), "let me think");

        // End of response flushes remaining buffers.
        out.end_response();
        assert_eq!(out.text_buffer.lock().unwrap().clone(), "");
        assert_eq!(out.thinking_buffer.lock().unwrap().clone(), "");
    }
}
