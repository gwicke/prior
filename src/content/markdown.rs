//! Markdown rendering: block rendering, syntax highlighting, language
//! detection, and the answer renderer's theme.
//!
//! Everything here renders as for a **terminal** — colours and OSC 8 links
//! on — unconditionally. Fidelity is the edge's decision: `TermIO` strips
//! escapes per stream at the write, so nothing above it asks whether there
//! is a terminal on the other end.

use markdown_terminal::Theme;

/// Render `src` as a styled terminal string via `codewandler-markdown-terminal`.
///
/// Uses GFM (`parse_gfm`) so tables / task-lists / strikethrough render as users
/// expect, with the default ANSI theme and OSC 8 clickable links.
pub(crate) fn render_markdown(src: &str, width: usize) -> String {
    let mut theme = Theme::default();
    theme.clickable_links = true;
    markdown_terminal::render_with(&markdown_stream::parse_gfm(src), &theme, width)
}

/// Highlight a single physical line of `text` as `lang` using the *same*
/// markdown code-block formatter the answer/result path uses
/// (`markdown_terminal` driven by `markdown_stream` via `render_markdown`).
///
/// Returns **foreground-only** ANSI (no background) so the caller can layer the
/// dark-grey tool panel on top via `paint_panel`. The renderer's 2-space code
/// indent and trailing newline are stripped, leaving one line of styled text.
/// Each call builds a fresh fenced block, so the formatter is fully
/// isolated/flushed per line.
pub(crate) fn highlight_code_fg(text: &str, lang: &str, width: usize) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let md = format!("```{lang}\n{text}\n```\n");
    let rendered = render_markdown(&md, width);
    let body = rendered
        .strip_prefix("  ")
        .unwrap_or_else(|| rendered.strip_prefix(' ').unwrap_or(&rendered));
    body.trim_end_matches('\n').to_string()
}

/// Language id used to colorize a tool *command* label (e.g. `read …`,
/// `$ ls …`). Any non-empty id engages token highlighting; the highlighter is
/// language-agnostic for tokens, so `bash` is a safe, consistent default.
pub(crate) fn command_lang(_name: &str) -> &'static str {
    "bash"
}

/// Guess a syntax-highlight id from a file path's extension, for coloring
/// `read_file` output. Unknown extensions fall back to `text` (no token
/// highlighting, just the uniform code color).
pub(crate) fn guess_lang(path: &str) -> &'static str {
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

/// The theme the answer renderer is built with: colours and clickable links,
/// always — the same styling [`render_markdown`] produces. Shared with the
/// tests, whose "what did the renderer alone produce" reference has to agree
/// with it byte for byte or the comparison flags styling as host contribution.
pub(crate) fn answer_theme() -> Theme {
    Theme {
        clickable_links: true,
        ..Theme::default()
    }
}
