//! Text wrapping for table cells (moved from picocode's `output::wrap` so the
//! crate stays dependency-free).

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_width() {
        assert_eq!(wrap("hello world", 5), vec!["hello", "world"]);
    }

    #[test]
    fn preserves_indent_on_continuations() {
        let lines = wrap("    alpha beta gamma", 14);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("    "));
        assert!(lines[1].starts_with("    "));
    }

    #[test]
    fn hard_splits_long_word() {
        let lines = wrap("abcdefghij", 4);
        assert_eq!(lines, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn keeps_punctuation_attached() {
        // `word:` is one whitespace-delimited token; the colon stays attached
        // and no stray space is inserted before it.
        let lines = wrap("word: another", 7);
        assert_eq!(lines, vec!["word:", "another"]);
    }

    #[test]
    fn zero_width_returns_lines_unchanged() {
        assert_eq!(wrap("a\nb", 0), vec!["a", "b"]);
    }
}
