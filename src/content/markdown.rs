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
/// `text` is already wrapped to `width` by the caller (see
/// [`crate::content::text::wrap`]); this styles it, it does not reflow it. The
/// renderer indents a fenced code block by two spaces and, if a line plus that
/// indent no longer fits `width`, wraps it — so a line handed in at full
/// `width` would come back as two. Two guards against that:
///
/// * the fenced block is rendered two columns **wider** than the panel row, so
///   the renderer's two-space indent eats the slack and a line already at
///   `width` still fits on one physical line;
/// * every *physical* line of the renderer's output has its leading code
///   indent stripped (the renderer indents continuations too), so no embedded
///   newline reaches [`crate::content::panel::panel_row`] — which pads its
///   argument as a single row, and a multi-line blob there came out one long
///   row that wrapped in the terminal and leaked the frame behind it.
///
/// Returns **foreground-only** ANSI (no background) so the caller can layer the
/// dark-grey tool panel on top via `paint_panel`. Each call builds a fresh
/// fenced block, so the formatter is fully isolated/flushed per line.
pub(crate) fn highlight_code_fg(text: &str, lang: &str, width: usize) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let md = format!("```{lang}\n{text}\n```\n");
    // +2: the renderer's per-line code indent. Keeps a `width`-wide line on
    // one physical line instead of letting the indent push it into a re-wrap.
    let rendered = render_markdown(&md, width.saturating_add(2));
    // Strip by position, not by leading-space count: the renderer indents the
    // first code line by two and each wrap-continuation by four, but a code
    // line's *own* leading whitespace can sit on the first line and must
    // survive. Line 0 loses the two-space code indent; every later line is a
    // wrap continuation and loses the four-space one. What remains is
    // foreground-only ANSI with no embedded newline for `panel_row` to
    // mis-pad as part of one row.
    let body: Vec<&str> = rendered
        .trim_end_matches('\n')
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                line.strip_prefix("  ")
                    .or_else(|| line.strip_prefix(' '))
                    .unwrap_or(line)
            } else {
                line.strip_prefix("    ")
                    .or_else(|| line.strip_prefix("  "))
                    .unwrap_or(line)
            }
        })
        .collect();
    body.join("\n")
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

#[cfg(test)]
mod tests {
    use super::highlight_code_fg;
    use slashdot_tui::ansi::strip_ansi;

    /// The highlighter must not reflow its input: a line already wrapped to
    /// the panel row's budget comes back as exactly one physical line, or the
    /// caller pads a multi-line blob into one over-wide row that wraps in the
    /// terminal and leaks the frame behind it.
    #[test]
    fn a_line_at_the_budget_stays_one_physical_line() {
        for width in [20usize, 40, 79] {
            let line = "x".repeat(width);
            let out = highlight_code_fg(&line, "bash", width);
            assert_eq!(out.lines().count(), 1, "wrapped at width {width}: {out:?}");
            let plain = strip_ansi(&out);
            assert_eq!(plain, line, "content altered at width {width}");
        }
    }

    /// The renderer indents each wrap-continuation by four spaces (so a wrapped
    /// block reads as one level). Those must be stripped so `panel_row` never
    /// sees an embedded newline whose continuation is still indented. A code
    /// line's *own* leading whitespace is a different thing and must survive.
    #[test]
    fn rewrapped_continuations_are_unindented() {
        // Deliberately over-wide for its budget: the renderer will wrap it.
        // No leading whitespace of its own, so every leading space on a
        // physical line is the renderer's continuation indent.
        let line = "word ".repeat(20);
        let line = line.trim_end();
        let out = highlight_code_fg(line, "bash", 12);
        assert!(out.lines().count() > 1, "fixture must wrap: {out:?}");
        for physical in out.lines().skip(1) {
            assert!(
                !physical.starts_with("  "),
                "continuation indent survived: {physical:?}"
            );
        }
        // The words themselves survive the round-trip, spaces collapsed at
        // wrap points only.
        let plain = strip_ansi(&out);
        assert_eq!(
            plain.split_whitespace().collect::<Vec<_>>(),
            line.split_whitespace().collect::<Vec<_>>()
        );
    }

    /// A code line's own leading whitespace is content, not the renderer's
    /// indent: it must survive on the first physical line. Stripping by
    /// leading-space *count* would eat a deeply-indented source line here.
    #[test]
    fn a_lines_own_indentation_survives() {
        let out = highlight_code_fg("        deep();", "bash", 40);
        assert_eq!(out.lines().count(), 1, "{out:?}");
        let plain = strip_ansi(&out);
        assert_eq!(plain, "        deep();", "own indent eaten: {plain:?}");
    }

    #[test]
    fn empty_input_renders_empty_and_ansi_is_foreground_only() {
        assert_eq!(highlight_code_fg("", "bash", 40), "");
        let out = highlight_code_fg("let x = 1;", "rust", 40);
        assert!(out.contains('\x1b'), "no styling emitted: {out:?}");
        assert!(
            !out.contains("\x1b[48;"),
            "highlighter must not paint a background: {out:?}"
        );
    }
}
