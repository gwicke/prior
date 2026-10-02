//! Text wrapping for table cells (moved from picocode's `output::wrap` so the
//! crate stays dependency-free).

use unicode_width::UnicodeWidthChar;

/// One visible character plus any ANSI escape sequences that precede it.
/// `w` is the character's visible width (escapes have width 0).
#[derive(Clone)]
struct Atom {
    esc: String,
    ch: char,
    w: usize,
}

/// Split `text` into atoms, pulling any ANSI SGR escape sequences into the
/// following character's `esc` so escapes are never counted as width and
/// never split across a wrap boundary as their own token.
fn atoms(text: &str) -> Vec<Atom> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    let mut pending = String::new();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            pending.push(c);
            for e in chars.by_ref() {
                pending.push(e);
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let w = UnicodeWidthChar::width(c).unwrap_or(1);
        out.push(Atom {
            esc: std::mem::take(&mut pending),
            ch: c,
            w,
        });
    }
    // Trailing escapes with no following visible char.
    if !pending.is_empty() {
        out.push(Atom {
            esc: pending,
            ch: '\0',
            w: 0,
        });
    }
    out
}

fn atom_width(a: &Atom) -> usize {
    a.w
}

/// Render a slice of atoms to a string (escapes + chars).
fn render(atoms: &[Atom]) -> String {
    let mut s = String::new();
    for a in atoms {
        s.push_str(&a.esc);
        if a.ch != '\0' {
            s.push(a.ch);
        }
    }
    s
}

fn total_width(atoms: &[Atom]) -> usize {
    atoms.iter().map(atom_width).sum()
}

/// Wrap `text` to fit `width` display columns per line.
///
/// Each logical line (split on `\n`) is wrapped independently; blank lines are
/// preserved. The leading whitespace of each logical line is captured and
/// re-emitted at the start of the first wrapped line *and* every continuation
/// line, so indentation survives a reflow.
///
/// ANSI SGR escape sequences are transparent to width accounting (they are
/// carried alongside the following character), so highlighted cells wrap and
/// hard-split by visible columns without being mangled.
///
/// Tokenization inside the body uses whitespace, collapsing any run of
/// whitespace between words to a single space. A word longer than the available
/// width (after subtracting the indent) is hard-split into `width`-column
/// chunks (CJK-safe). The first chunk keeps the indent; subsequent chunks are
/// emitted plain so the overlay renderer can prepend its own marker.
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
        let logical_atoms = atoms(logical);
        // Capture leading whitespace so the indent survives wrap.
        let indent_len = logical_atoms
            .iter()
            .take_while(|a| a.ch.is_whitespace())
            .count();
        let indent_atoms = &logical_atoms[..indent_len];
        let indent_str = render(indent_atoms);
        let indent_w = total_width(indent_atoms);
        let body = &logical_atoms[indent_len..];
        if body.iter().all(|a| a.w == 0) && body.is_empty() {
            out.push(indent_str);
            continue;
        }
        if body.is_empty() {
            out.push(indent_str);
            continue;
        }
        let content_width = width.saturating_sub(indent_w).max(1);

        // Split body into words (runs of non-whitespace atoms) and spaces.
        let mut words: Vec<Vec<Atom>> = Vec::new();
        let mut cur: Vec<Atom> = Vec::new();
        for a in body {
            if a.ch.is_whitespace() {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            } else {
                cur.push(Atom {
                    esc: a.esc.clone(),
                    ch: a.ch,
                    w: a.w,
                });
            }
        }
        if !cur.is_empty() {
            words.push(cur);
        }

        let mut line: Vec<Atom> = indent_atoms
            .iter()
            .map(|a| Atom {
                esc: a.esc.clone(),
                ch: a.ch,
                w: a.w,
            })
            .collect();
        let mut line_w = indent_w;

        let new_indent = || -> Vec<Atom> {
            indent_atoms
                .iter()
                .map(|a| Atom {
                    esc: a.esc.clone(),
                    ch: a.ch,
                    w: a.w,
                })
                .collect()
        };
        for word in words {
            let word_w = total_width(&word);
            if word_w > content_width {
                // Flush any pending content, then hard-split the long word.
                if line_w > indent_w {
                    out.push(render(&line));
                }
                // Emit chunks of `content_width` visible columns; keep the
                // last chunk pending as the current line.
                let mut chunk: Vec<Atom> = Vec::new();
                let mut chunk_w = 0;
                let mut first_chunk = true;
                for a in word {
                    if chunk_w + a.w > content_width && chunk_w > 0 {
                        if first_chunk {
                            let mut l = new_indent();
                            l.append(&mut chunk);
                            out.push(render(&l));
                            first_chunk = false;
                        } else {
                            out.push(render(&chunk));
                            chunk.clear();
                        }
                        chunk_w = 0;
                    }
                    chunk_w += a.w;
                    chunk.push(a);
                }
                line = if first_chunk {
                    let mut l = new_indent();
                    l.extend(chunk);
                    line_w = total_width(&l);
                    l
                } else {
                    line_w = total_width(&chunk);
                    chunk
                };
                continue;
            }
            let content_len = line_w.saturating_sub(indent_w);
            if content_len == 0 {
                line.extend(word);
                line_w += word_w;
            } else if content_len + 1 + word_w > content_width {
                out.push(render(&line));
                line = indent_atoms
                    .iter()
                    .map(|a| Atom {
                        esc: a.esc.clone(),
                        ch: a.ch,
                        w: a.w,
                    })
                    .collect();
                line.extend(word);
                line_w = indent_w + word_w;
            } else {
                line.push(Atom {
                    esc: String::new(),
                    ch: ' ',
                    w: 1,
                });
                line.extend(word);
                line_w += 1 + word_w;
            }
        }
        if line_w > 0 {
            let rendered = render(&line);
            if !rendered.is_empty() {
                out.push(rendered);
            }
        }
    }
    out
}

/// Split `text` into lines of at most `width` display columns **without
/// changing any character** — every space survives, so column-aligned text
/// (table cells, the engine's own help headers) keeps its alignment.
///
/// This is the counterpart to [`wrap`], which reflows prose and collapses
/// runs of whitespace. The painter needs both behaviours but must not choose
/// between them per line: an overlay row is either laid-out text (split it)
/// or prose (wrap it), and the painter does not know which it holds. So the
/// painter splits, and the engine — which knows its content — keeps doing the
/// word-level wrapping when it is warranted.
///
/// Each logical line (`\n`-separated) is split independently; blank lines
/// are preserved. A line that already fits is returned unchanged, escapes
/// included. An over-long line is cut at `width` visible columns, hard: a
/// word longer than the line is cut mid-word rather than reflowed, and a
/// space that lands on the boundary simply ends the line it is in.
pub fn split_to_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        let logical_atoms = atoms(logical);
        if total_width(&logical_atoms) <= width {
            out.push(render(&logical_atoms));
            continue;
        }
        let mut chunk: Vec<Atom> = Vec::new();
        let mut chunk_w = 0;
        for a in &logical_atoms {
            if chunk_w + a.w > width && chunk_w > 0 {
                out.push(render(&chunk));
                chunk = Vec::new();
                chunk_w = 0;
            }
            chunk.push(a.clone());
            chunk_w += a.w;
        }
        out.push(render(&chunk));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_preserves_every_space() {
        // `wrap` would collapse this to "10 last 10 turns"; the split must not.
        assert_eq!(
            split_to_width("  10      last 10 turns", 30),
            vec!["  10      last 10 turns"]
        );
    }

    #[test]
    fn split_leaves_a_fitting_line_untouched() {
        assert_eq!(split_to_width("a  b", 10), vec!["a  b"]);
    }

    #[test]
    fn split_cuts_at_width_preserving_content() {
        let text = "abcdefghijklmno";
        let parts = split_to_width(text, 5);
        assert_eq!(parts, vec!["abcde", "fghij", "klmno"]);
        // Nothing lost, nothing reordered: the concatenation is the input.
        assert_eq!(parts.concat(), text);
    }

    #[test]
    fn split_breaks_after_a_boundary_space() {
        // "aaa bbb" cut at 4: the space lands on the boundary, so the first
        // line takes it and "bbb" starts the next.
        assert_eq!(split_to_width("aaa bbb", 4), vec!["aaa ", "bbb"]);
    }

    #[test]
    fn split_keeps_blank_lines() {
        assert_eq!(split_to_width("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn split_measures_display_width_not_chars() {
        // Three wide chars are 6 columns: 5 per line means 2 then 1.
        assert_eq!(split_to_width("日本語", 5), vec!["日本", "語"]);
    }

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

    #[test]
    fn ansi_escapes_are_zero_width_and_kept() {
        // Highlighted "model" is 5 visible cols; it must not be split when
        // the width comfortably fits it.
        let s = "\x1b[1;35mmod\x1b[0mel";
        let lines = wrap(s, 10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], s);
        // Escape bytes must not consume width: a 5-wide word in width 5 fits.
        let lines = wrap(s, 5);
        assert_eq!(lines.len(), 1, "lines: {lines:?}");
    }

    #[test]
    fn ansi_aware_hard_split_by_visible_columns() {
        let s = "\x1b[1;35mmod\x1b[0mel"; // visible "model"
        let lines = wrap(s, 3);
        // Visible columns: "mod" then "el".
        assert_eq!(lines.len(), 2, "lines: {lines:?}");
        let vis: Vec<String> = lines
            .iter()
            .map(|l| {
                let mut out = String::new();
                let mut it = l.chars().peekable();
                while let Some(c) = it.next() {
                    if c == '\x1b' {
                        for e in it.by_ref() {
                            if e.is_ascii_alphabetic() {
                                break;
                            }
                        }
                        continue;
                    }
                    out.push(c);
                }
                out
            })
            .collect();
        assert_eq!(vis, vec!["mod", "el"]);
    }
}
