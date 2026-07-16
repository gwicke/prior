use console::{style, StyledObject};
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;
use termimad;

#[derive(Debug, PartialEq)]
pub enum Confirmation {
    Yes,
    No,
    Always,
}

pub trait Output: Send + Sync {
    fn display_text(&self, text: &str);
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
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        format!("{}...", s.chars().take(max_len).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Terminal width in columns, or 80 if it cannot be determined.
fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
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
        }
    }
}

impl Default for ConsoleOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleOutput {
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
    fn display_text(&self, text: &str) {
        eprintln!(
            "[picocode-dbg] ConsoleOutput::display_text first_line={:?}",
            text.lines().next().unwrap_or("")
        );
        self.stop_thinking();
        println!();
        print!("{} ", style("⏺").cyan());
        termimad::print_inline(text);
        println!();
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
            return;
        }

        let show_max = if is_error { usize::MAX } else { 4 };
        let wrap_width = term_width().saturating_sub(4); // account for "  │  " prefix
        let mut printed = 0;
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
            } else {
                let wrapped = if wrap_width == 0 {
                    vec![line.to_string()]
                } else {
                    wrap(line, wrap_width)
                };
                for (j, wline) in wrapped.into_iter().enumerate() {
                    let sym = if j == 0 { symbol } else { " " };
                    println!("  {}  {}", style(sym).dim(), style(wline).dim());
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
        }
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
    }

    fn display_system(&self, text: &str) {
        self.stop_thinking();
        println!("{}", style(text).bold().dim());
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
}
