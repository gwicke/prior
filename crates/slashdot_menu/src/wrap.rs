//! Text wrapping for table cells (moved from picocode's `output::wrap` so the
//! crate stays dependency-free).

use unicode_width::UnicodeWidthChar;

/// One visible character plus any ANSI escape sequences that precede it.
/// `w` is the character's visible width (escapes have width 0).
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
                            l.extend(chunk.drain(..));
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
