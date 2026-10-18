//! Markdown rendering: block rendering, syntax highlighting, language
//! detection, and the answer renderer's theme.
//!
//! Everything here renders as for a **terminal** — colours and OSC 8 links
//! on — unconditionally. Fidelity is the edge's decision: `TermIO` strips
//! escapes per stream at the write, so nothing above it asks whether there
//! is a terminal on the other end.

use markdown_terminal::Theme;

use crate::content::text::expand_tabs;

/// Render `src` as a styled terminal string via `codewandler-markdown-terminal`.
///
/// Uses GFM (`parse_gfm`) so tables / task-lists / strikethrough render as users
/// expect, with the default ANSI theme and OSC 8 clickable links.
pub(crate) fn render_markdown(src: &str, width: usize) -> String {
    let mut theme = Theme::default();
    theme.clickable_links = true;
    markdown_terminal::render_with(&markdown_stream::parse_gfm(src), &theme, width)
}

/// Highlight `text` (a whole code block, possibly multi-line, tab-expanded by
/// the caller) as `lang` using the *same* markdown code-block formatter the
/// answer/result path uses (`markdown_terminal` via [`render_markdown`]).
///
/// The renderer wraps code at the source: each line's first row carries a
/// two-space fence gutter and each wrap continuation four, hard-splitting any
/// token wider than the budget. That gutter is **kept**, not stripped — for
/// *results* the gutter is the row's indent (painted flush, the panel
/// background rendering it as seamless margin), and with nothing to strip a
/// whole block renders correctly in one call (a stripped gutter would have to
/// distinguish a continuation from a later line's own indentation, which the
/// flat output does not carry). Use [`highlight_label_fg`] instead for
/// labels, which sit flush. `width` is the row budget *including* the gutter,
/// so every returned row is at most `width` visible columns and the caller
/// pads each to the panel edge.
///
/// Returns one row per physical line, joined by `\n` with no trailing newline,
/// **foreground-only** ANSI so the caller can layer the grey panel on top via
/// `paint_panel`. The fence is as long as the content's longest backtick run
/// plus one, so a ``` in tool output (markdown, heredocs) stays literal content
/// instead of closing the block and letting the rest re-flow as prose.
pub(crate) fn highlight_code_fg(text: &str, lang: &str, width: usize) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let fence = "`".repeat(longest_backtick_run(text).max(2) + 1);
    let md = format!("{fence}{lang}\n{text}\n{fence}\n");
    render_markdown(&md, width)
        .trim_end_matches('\n')
        .to_string()
}

/// Highlight a command *label* — the tool name and its arguments (e.g.
/// `$ cd ~/src && sed …`, `read src/x.rs`) — with **no** fence gutter: a
/// label sits flush at the panel's left edge, unlike results, whose indent is
/// the gutter of [`highlight_code_fg`].
///
/// Each source line is rendered as its own fence block and its gutter removed
/// positionally — row 0 carries two leading spaces, each wrap continuation
/// four. That is exact *per source line*, which is why the split happens
/// before rendering: a multi-line block cannot be stripped after the fact (a
/// continuation row and a later line's own two-space indentation are the same
/// flat text).
///
/// Same contract as [`highlight_code_fg`] otherwise, except that tabs are
/// expanded *here*: a label is raw tool-argument text (a command, a path), so
/// any tab in it must not be measured at zero columns the way the terminal
/// draws it. `width` is the row budget; rows are foreground-only ANSI joined
/// by `\n` with no trailing newline.
pub(crate) fn highlight_label_fg(text: &str, lang: &str, width: usize) -> String {
    // The renderer's per-row code indent: two columns on a line's first row,
    // four on each wrap continuation.
    const GUTTER_FIRST: usize = 2;
    const GUTTER_WRAP: usize = 4;
    text.split('\n')
        .map(|line| {
            let line = expand_tabs(line);
            let rendered = highlight_code_fg(line.as_ref(), lang, width);
            rendered
                .split('\n')
                .enumerate()
                .map(|(i, row)| {
                    let gutter = if i == 0 { GUTTER_FIRST } else { GUTTER_WRAP };
                    row.strip_prefix(&" ".repeat(gutter)).unwrap_or(row)
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Longest run of consecutive backticks in `s` — used to size a code fence that
/// cannot be closed early by backticks inside the content.
fn longest_backtick_run(s: &str) -> usize {
    let (mut best, mut cur) = (0usize, 0usize);
    for c in s.chars() {
        if c == '`' {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
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
    use super::{highlight_code_fg, highlight_label_fg};
    use slashdot_tui::ansi::strip_ansi;
    use unicode_width::UnicodeWidthStr;

    /// Visible width of a rendered row, as the terminal draws it.
    fn vis(row: &str) -> usize {
        UnicodeWidthStr::width(strip_ansi(row).as_str())
    }

    /// A whole block is handed over at once (no per-line wrapping by the
    /// caller): every returned row must fit the budget including the fence
    /// gutter, and a line's own indentation must survive behind that gutter.
    #[test]
    fn a_block_fits_the_budget_and_keeps_own_indent() {
        let width = 40;
        let block = "aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll mmm nnn ooo ppp qqq rrr\n    indented();";
        let out = highlight_code_fg(block, "bash", width);
        for row in out.lines() {
            assert!(vis(row) <= width, "row over budget ({}): {row:?}", vis(row));
        }
        // The indented line keeps its four-space indent, behind the two-space
        // fence gutter (2 + 4 = 6 leading spaces).
        let plain = strip_ansi(&out);
        assert!(
            plain.lines().any(|r| r == "      indented();"),
            "own indent lost: {plain:?}"
        );
    }

    /// A blank line in the block keeps its own row (gutter-only), so vertical
    /// structure survives rather than collapsing.
    #[test]
    fn blank_lines_keep_their_row() {
        let out = highlight_code_fg("first\n\nlast", "bash", 40);
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows.len(), 3, "blank line lost: {out:?}");
        assert!(
            vis(rows[1]) <= 2,
            "blank row not gutter-only: {:?}",
            strip_ansi(rows[1])
        );
    }

    /// A ``` line in tool output must stay literal content, not close the
    /// fence and let everything after it re-flow as prose. The dynamic-length
    /// fence guarantees this.
    #[test]
    fn a_backtick_line_does_not_close_the_block() {
        let block = "echo hi\n```\nrm -rf /\necho bye";
        let out = highlight_code_fg(block, "bash", 40);
        assert_eq!(out.lines().count(), 4, "block broke at backticks: {out:?}");
        let plain = strip_ansi(&out);
        assert!(plain.contains("```"), "backtick line lost: {plain:?}");
        assert!(
            plain.contains("rm -rf /"),
            "later line re-flowed: {plain:?}"
        );
    }

    /// A command label sits flush at the panel edge — the fence gutter the
    /// result path keeps (and uses as the row's indent) is stripped here, on
    /// the first row and on every wrap continuation, so even a long command
    /// that wraps starts at column zero.
    #[test]
    fn a_label_renders_flush_without_the_fence_gutter() {
        let label = "$ cd ~/src/pi && sed -n '550,640p' packages/agent/src/agent-loop.ts";
        let out = highlight_label_fg(label, "bash", 40);
        let plain = strip_ansi(&out);
        assert!(plain.starts_with("$ cd"), "label lost: {plain:?}");
        for row in plain.lines() {
            assert!(!row.starts_with(' '), "label row not flush: {row:?}");
            assert!(vis(row) <= 40, "row over budget ({}): {row:?}", vis(row));
        }
        // Wrapping preserved the content: no characters dropped by the strip.
        let joined = plain.split_whitespace().collect::<Vec<_>>().join(" ");
        for word in ["cd", "~/src/pi", "sed", "packages/agent/src/agent-loop.ts"] {
            assert!(joined.contains(word), "{word:?} lost: {plain:?}");
        }
    }

    /// A label can itself span lines (a multi-line bash command). Each source
    /// line is rendered and stripped on its own, so each stays flush.
    #[test]
    fn a_multiline_label_is_flush_per_line() {
        let out = highlight_label_fg("echo one\necho two", "bash", 40);
        let plain = strip_ansi(&out);
        assert!(plain.starts_with("echo one"), "first line lost: {plain:?}");
        assert!(plain.contains("echo two"), "second line lost: {plain:?}");
        for row in plain.lines() {
            assert!(!row.starts_with(' '), "label row not flush: {row:?}");
        }
    }

    /// A tab in a label (raw argument text) must not survive to the panel: a
    /// terminal draws it as spaces to the next stop, which would overflow the
    /// padded row the same way an un-expanded result tab does.
    #[test]
    fn a_tab_in_a_label_is_expanded() {
        let out = highlight_label_fg("$ printf\ta", "bash", 40);
        let plain = strip_ansi(&out);
        assert!(!plain.contains('\t'), "tab survived: {plain:?}");
        assert!(plain.contains("printf"), "content lost: {plain:?}");
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
