//! Text formatting: word wrap, tab expansion, the discrete-message block, and
//! the start-up greeter header.

use std::borrow::Cow;

use console::{style, StyledObject};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Tab stop, in display columns, for expanding tabs at the display entry
/// points (`tool_row_contents`, the result panel, `panel_row`).
///
/// Tabs must go before any width math runs: every layer below measures with
/// unicode-width, where a tab counts **zero** columns, while the terminal
/// draws it as a run of spaces up to the next stop — so a tab-indented line
/// got padded as if its indentation were absent and the drawn row came out
/// `indent` columns over the panel width (a natural wrap the line-clear never
/// arms, leaking the frame below into the continuation). Expanding once, at
/// the entry, leaves plain spaces that wrap, pad, and draw all measure the
/// same. Stops of 4 rather than the terminal's 8 keep indented code compact.
pub const TABSTOP: usize = 4;

/// Expand every tab in `text` to spaces up to the next [`TABSTOP`] column
/// stop, counted per line and per **display** column (wide glyphs count two,
/// ANSI escapes count none — so a tab after an SGR lands on the same stop the
/// row would show).
///
/// Lines without tabs are returned borrowed — the common case costs nothing.
/// This is a *display* transform: the model-facing text keeps its tabs.
pub fn expand_tabs(text: &str) -> Cow<'_, str> {
    if !text.contains('\t') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + TABSTOP);
    let mut col = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // Escapes pass through whole and hold no columns — the same rule
            // `panel_row` measures with, so the stop stays put across them.
            let end = crate::ansi::escape_end(bytes, i);
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        i += ch.len_utf8();
        match ch {
            '\t' => {
                let spaces = TABSTOP - col % TABSTOP;
                for _ in 0..spaces {
                    out.push(' ');
                }
                col += spaces;
            }
            '\n' => {
                out.push('\n');
                col = 0;
            }
            _ => {
                out.push(ch);
                col += UnicodeWidthChar::width(ch).unwrap_or(0);
            }
        }
    }
    Cow::Owned(out)
}

/// Wrap `text` to fit `width` display columns per line.
///
/// Each logical line (split on `\n`) is wrapped independently; blank lines are
/// preserved. The leading whitespace of each logical line is captured and
/// re-emitted at the start of the first wrapped line *and* every continuation
/// line — including hard-split chunks — so indentation survives a reflow and
/// no continuation lands flush-left of the block it belongs to.
///
/// Budgets are measured in **display columns** (unicode-width), not chars: a
/// wide glyph counts two, so a CJK line wraps where the terminal would draw
/// it rather than half a line too late. Text reaching `wrap` must already be
/// tab-free ([`expand_tabs`]) — a tab would measure zero here and be drawn
/// as spaces. Callers are expected to pass raw, unstyled text; styling is
/// layered on after wrapping.
///
/// Tokenization inside the body uses `split_whitespace`, collapsing any run of
/// whitespace between words to a single space. A word wider than the available
/// budget (after subtracting the indent) is hard-split on column boundaries
/// (CJK-safe: a wide glyph moves whole unless the budget is one column).
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
        let indent_width = UnicodeWidthStr::width(indent.as_str());
        let content_width = width.saturating_sub(indent_width).max(1);
        let words: Vec<&str> = body.split_whitespace().collect();
        let mut line = String::from(&indent);
        for word in words {
            if UnicodeWidthStr::width(word) > content_width {
                // Flush any pending line, then hard-split the long word on
                // column boundaries. Every chunk keeps the indent — the
                // carried last chunk included, which is also what keeps the
                // `line - indent` arithmetic below in range (a bare tail
                // shorter than the indent used to underflow it).
                if UnicodeWidthStr::width(line.as_str()) > indent_width {
                    out.push(std::mem::take(&mut line));
                }
                let chunks = split_columns(word, content_width);
                let last = chunks.len() - 1;
                for c in &chunks[..last] {
                    out.push(format!("{indent}{c}"));
                }
                line = format!("{indent}{}", chunks[last]);
                continue;
            }
            let content_len = UnicodeWidthStr::width(line.as_str()).saturating_sub(indent_width);
            if content_len == 0 {
                line.push_str(word);
            } else if content_len + 1 + UnicodeWidthStr::width(word) > content_width {
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

/// Split `word` into chunks of at most `max` display columns each, cutting
/// only where a column actually overflows — a wide glyph therefore moves
/// whole to the next chunk, as the terminal would move it. A single glyph
/// wider than `max` gets a chunk to itself (one column cannot hold it, and
/// the bytes must still leave the head for progress).
fn split_columns(word: &str, max: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_width = 0usize;
    for ch in word.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if !cur.is_empty() && cur_width + w > max {
            chunks.push(std::mem::take(&mut cur));
            cur_width = 0;
        }
        cur.push(ch);
        cur_width += w;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// The `⏺ `-prefixed block for a discrete message (`display_text`), with a
/// trailing newline. Inline markdown is rewritten; measuring the bytes (not
/// the source) is what decides the height downstream.
pub(crate) fn system_message(text: &str) -> String {
    format!("{} {}\n", style("⏺").cyan(), termimad::inline(text))
}

/// The start-up greeter block — avatar over a status line — for
/// `display_header`, with a trailing newline. `width` centers the avatar.
pub(crate) fn header_block(
    provider: &str,
    model: &str,
    yolo: bool,
    limit: usize,
    persona: Option<&str>,
    width: usize,
) -> String {
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
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tabs_advances_to_the_next_stop() {
        // Stops at 4 columns: "a" at col 1 -> 3 spaces; "ab" at col 2 -> 2.
        assert_eq!(expand_tabs("a\tb"), "a   b");
        assert_eq!(expand_tabs("ab\tc"), "ab  c");
        assert_eq!(expand_tabs("abcd\te"), "abcd    e");
        // A tab exactly at a stop still advances a full stop.
        assert_eq!(expand_tabs("abcd\tX"), "abcd    X");
    }

    #[test]
    fn expand_tabs_indents_a_line_to_the_first_stop() {
        assert_eq!(expand_tabs("\tx"), "    x");
        assert_eq!(expand_tabs("\t\tmessages,"), "        messages,");
        assert_eq!(expand_tabs("\t};"), "    };");
    }

    #[test]
    fn expand_tabs_resets_the_column_at_each_newline() {
        assert_eq!(expand_tabs("a\t\nb\tc"), "a   \nb   c");
    }

    #[test]
    fn expand_tabs_counts_wide_glyphs_as_two_columns() {
        // 漢 sits at cols 0-1, so the tab at col 2 advances only 2.
        assert_eq!(expand_tabs("漢\t"), "漢  ");
    }

    #[test]
    fn expand_tabs_passes_escapes_through_without_counting_them() {
        // SGR holds no columns: the tab after it is at col 1, as if the
        // escape were not there.
        assert_eq!(expand_tabs("\x1b[31ma\tb"), "\x1b[31ma   b");
    }

    #[test]
    fn expand_tabs_borrows_when_there_is_nothing_to_expand() {
        assert!(matches!(
            expand_tabs("no tabs here"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(expand_tabs(""), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn wrap_hard_split_tail_keeps_indent_when_more_words_follow() {
        // A hard-split tail shorter than the indent used to leave `line`
        // without it, under-running `line - indent` (a debug panic) and
        // flushing the tail row flush-left (a release artifact).
        let r = wrap("    aaaaaaaaa x", 8);
        assert_eq!(
            r,
            vec![
                "    aaaa".to_string(),
                "    aaaa".to_string(),
                "    a x".to_string(),
            ]
        );
        for line in &r {
            assert!(line.starts_with("    "), "indent lost: {line:?}");
        }
    }

    #[test]
    fn wrap_budgets_are_display_columns_not_chars() {
        // Each 漢字 pair is 4 columns: at width 4 they must wrap as units.
        let r = wrap("漢字 漢字", 4);
        assert_eq!(r, vec!["漢字".to_string(), "漢字".to_string()]);
        // A mixed line must wrap before the wide glyphs overflow the column
        // budget, not after counting them as one cell each.
        let r = wrap("a 漢字 b", 4);
        assert_eq!(
            r,
            vec!["a".to_string(), "漢字".to_string(), "b".to_string()]
        );
        for line in &r {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 4,
                "row over budget: {line:?}"
            );
        }
    }

    #[test]
    fn wrap_hard_splits_wide_words_on_column_boundaries() {
        // 漢字 fits the 4-column budget; the trailing 漢 moves whole.
        let r = wrap("漢字漢", 4);
        assert_eq!(r, vec!["漢字".to_string(), "漢".to_string()]);
    }
}
