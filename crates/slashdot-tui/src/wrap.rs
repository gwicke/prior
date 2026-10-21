//! Text wrapping and splitting by display width. Not menu-specific — it is
//! width arithmetic over `&str` — so it lives here next to the measurement
//! it must agree with. (For the *other* wrap in this crate:
//! [`ansi::wrap_to_width`] makes the terminal's own wrapping explicit as
//! `\n` bytes for the overwrite rule; this module reflows or hard-splits
//! content to fit a chosen width.)
//!
//! # Measurement contract
//!
//! All width math in this module uses [`ansi`]'s measurement — [`escape_end`]
//! to skip escape sequences whole (CSI final bytes, OSC terminated by BEL or
//! ST) and `unicode-width` for glyph widths. One semantic for "how wide is
//! this line": the check, the split, and the painter's padding must agree, or
//! a line the check passes would be cut at the wrong column by the splitter
//! (and vice versa).
//!
//! # Normalization contract
//!
//! Width accounting is only sound for text whose characters render the way
//! `unicode-width` measures them. Tabs do not: a tab is measured as one
//! column (or not at all) but rendered as an advance to the next tab stop.
//! Carriage returns, bells and backspaces move the cursor and overprint.
//! Before anything is measured or rendered, control characters are therefore
//! replaced at ingestion (the menu's `LogicalLine::new` and `Table::add_row`),
//! and defensively here:
//!
//! - `\t` becomes `TAB_SPACES` spaces (a fixed count — tab stops depend on
//!   the rendered column, which wrapping moves, so expansion can never be
//!   made correct, only consistent),
//! - a `\r` directly before a `\n` is dropped (CRLF line endings),
//! - any other control character outside an escape sequence becomes one
//!   space,
//! - escape sequences themselves are copied verbatim (an OSC payload may
//!   contain control bytes that belong to the sequence).
//!
//! [`escape_end`]: ansi::escape_end

use std::borrow::Cow;
use std::ops::Range;

use unicode_width::UnicodeWidthChar;

use crate::ansi::{display_width_ansi, escape_end};

/// Spaces a tab is normalized to. Fixed rather than tab-stop-aligned: a tab
/// stop depends on the column it renders at, and wrapping changes that
/// column — so no expansion can be correct after a reflow.
const TAB_SPACES: usize = 4;

/// One logical line, classified by the cheap pre-measurement scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Pure printable ASCII: no escapes, no control bytes, no multi-byte
    /// chars. Width equals byte length — no decoding needed.
    Ascii(usize),
    /// No control characters needing replacement (but has escapes and/or
    /// multi-byte chars). Measure with [`display_width_ansi`].
    Plain,
    /// Contains a tab or another control character outside an escape
    /// sequence: must be normalized before measurement.
    Dirty,
}

/// Single byte pass answering "does this line need attention, and how wide
/// is it if it doesn't?". Escape sequences are skipped whole (their payload
/// is not classified — control bytes inside an OSC belong to the sequence).
///
/// `Class::Ascii` carries the *width* of the line: its printable-ASCII byte
/// count, which equals its display width (each such byte is one column).
/// `\n` separators are excluded — they render as nothing.
fn classify(line: &str) -> Class {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut len = 0;
    let mut has_esc = false;
    let mut non_ascii = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            if b == 0x1b {
                has_esc = true;
                i = escape_end(bytes, i);
                continue;
            }
            if b == b'\n' {
                i += 1; // line separator — zero width, untouched by normalization
                continue;
            }
            if b < 0x20 || b == 0x7f {
                return Class::Dirty; // tab, CR, BEL, BS, ...
            }
            i += 1; // printable ASCII
            len += 1;
        } else {
            // C1 controls encode as U+0080..=U+009F: 0xc2 0x80..=0x9f.
            if b == 0xc2 && i + 1 < bytes.len() && (0x80..=0x9f).contains(&bytes[i + 1]) {
                return Class::Dirty;
            }
            non_ascii = true;
            i += 1; // continuation bytes are all >= 0x80; byte-wise is safe
        }
    }
    if has_esc || non_ascii {
        Class::Plain
    } else {
        Class::Ascii(len)
    }
}

/// Replace tab / control characters per the module's normalization contract.
/// A no-op (borrowed) unless the cheap classification scan says otherwise —
/// one pass, no allocation for clean text.
pub fn normalize(text: &str) -> Cow<'_, str> {
    if classify(text) == Class::Dirty {
        Cow::Owned(normalize_dirty(text))
    } else {
        Cow::Borrowed(text)
    }
}

/// [`normalize`] for callers that already hold a `String`: keeps the input
/// allocation when nothing needs replacing.
pub fn normalize_string(s: String) -> String {
    if classify(&s) == Class::Dirty {
        normalize_dirty(&s)
    } else {
        s
    }
}

/// Rebuild `text` with control characters replaced. Escape sequences are
/// copied verbatim; everything else follows the module contract.
fn normalize_dirty(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let end = escape_end(bytes, i);
            out.push_str(&text[i..end]);
            i = end;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        i += ch.len_utf8();
        match ch {
            '\n' => out.push('\n'),
            '\t' => {
                for _ in 0..TAB_SPACES {
                    out.push(' ');
                }
            }
            '\r' => {
                // CRLF: the \r belongs to the line ending — drop it.
                if i < bytes.len() && bytes[i] != b'\n' {
                    out.push(' ');
                }
            }
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Byte index just past the ANSI escape sequence starting at `start` (which
/// must be an ESC). Uses the shared [`escape_end`] so splitting, measuring
/// and normalizing always skip the same bytes.
fn escape_end_at(line: &str, start: usize) -> usize {
    escape_end(line.as_bytes(), start)
}

/// Visible width of one char under the shared semantic: `unicode-width`,
/// control characters (none survive normalization) as zero.
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Per-char visible widths of `text`, aligned with `text.chars()`: every byte
/// of an escape sequence gets a `0` entry (it renders nothing), visible chars
/// their `unicode-width`. One shared walk, so a caller that windows or sums
/// widths agrees with [`split_to_width`] — the truncation and snippet paths
/// in `slashdot_menu` measure with this instead of their own escape parsers.
pub fn char_widths(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            let end = escape_end(bytes, i);
            out.extend(std::iter::repeat_n(0, text[i..end].chars().count()));
            i = end;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(char_width(ch));
        i += ch.len_utf8();
    }
    out
}

/// Visible width of `s` under the shared measurement, led by the cheap
/// classification scan: pure printable ASCII is its byte length — one byte
/// pass, no decoding, no escape parsing. Anything else measures with
/// [`display_width_ansi`] after normalization. This is the check to use
/// before deciding whether [`split_to_width`] needs to cut at all.
pub fn width_of(s: &str) -> usize {
    match classify(s) {
        Class::Ascii(len) => len,
        Class::Plain => display_width_ansi(s),
        Class::Dirty => display_width_ansi(&normalize_dirty(s)),
    }
}

/// A byte range of the logical line covering one visible char plus any
/// escape sequences preceding it (escapes ride with the *following* char, so
/// a cut never strands them on the wrong side of a wrap).
#[derive(Clone, Copy)]
struct Atom {
    start: usize,
    end: usize,
    ch: char,
    w: usize,
}

/// Split `text` into atoms. Zero-copy: atoms are offsets into `text`, not
/// owned pieces. Assumes `text` is normalized (callers normalize first) and
/// pure `Plain` content — the ASCII fast path lives in the callers.
fn atoms(text: &str) -> Vec<Atom> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut pending = 0; // start of the current atom (its escape run)
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = escape_end_at(text, i);
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
        let w = char_width(ch);
        let end = i + ch.len_utf8();
        out.push(Atom {
            start: pending,
            end,
            ch,
            w,
        });
        pending = end;
        i = end;
    }
    if pending < bytes.len() {
        // Trailing escapes with no following visible char: their own
        // zero-width atom so a render still emits them.
        out.push(Atom {
            start: pending,
            end: bytes.len(),
            ch: '\0',
            w: 0,
        });
    }
    out
}

fn atom_width(a: &Atom) -> usize {
    a.w
}

fn total_width(atoms: &[Atom]) -> usize {
    atoms.iter().map(atom_width).sum()
}

/// Render a slice of atoms by slicing the logical line — no per-char push.
fn render<'a>(text: &'a str, atoms: &[Atom]) -> &'a str {
    match (atoms.first(), atoms.last()) {
        (Some(first), Some(last)) => &text[first.start..last.end],
        _ => "",
    }
}

/// Cut `rest` into (head, head-tail) at `width` display columns. Escapes
/// immediately preceding the overflowing char ride to the next line with it
/// (matching [`atoms`]: a cut never splits an escape run from its char).
/// Never returns an empty head for non-empty input.
fn cut_at(rest: &str, width: usize) -> (&str, &str) {
    let bytes = rest.as_bytes();
    let mut i = 0;
    let mut used = 0;
    let mut run: Option<usize> = None; // start of the current escape run
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            run.get_or_insert(i);
            i = escape_end_at(rest, i);
            continue;
        }
        let ch = rest[i..].chars().next().unwrap_or('\u{fffd}');
        let cw = char_width(ch);
        if used + cw > width && used > 0 {
            let at = run.unwrap_or(i);
            return (&rest[..at], &rest[at..]);
        }
        used += cw;
        i += ch.len_utf8();
        run = None;
    }
    (rest, "")
}

/// Whole-line pieces of `line` at `width` columns — the cold path, reached
/// only after the caller's cheap check says the line overflows.
fn split_pieces(line: &str, width: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let (head, tail) = cut_at(rest, width);
        if head.is_empty() {
            out.push(rest);
            break;
        }
        out.push(head);
        rest = tail;
    }
    out
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
    let normalized = normalize(text);
    let text = normalized.as_ref();
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
        let indent_str = render(logical, &logical_atoms[..indent_len]);
        let indent_w = total_width(&logical_atoms[..indent_len]);
        let body = &logical_atoms[indent_len..];
        if body.is_empty() {
            out.push(indent_str.to_string());
            continue;
        }
        let content_width = width.saturating_sub(indent_w).max(1);

        // Split body into words: contiguous ranges of non-whitespace atoms.
        let mut words: Vec<Range<usize>> = Vec::new();
        let mut cur: Option<usize> = None;
        for (i, a) in body.iter().enumerate() {
            if a.ch.is_whitespace() {
                if let Some(s) = cur.take() {
                    words.push(s..i);
                }
            } else if cur.is_none() {
                cur = Some(i);
            }
        }
        if let Some(s) = cur {
            words.push(s..body.len());
        }

        let mut line = indent_str.to_string();
        let mut line_w = indent_w;

        for word in words {
            let word_atoms = &body[word.clone()];
            let word_w = total_width(word_atoms);
            let word_str = render(logical, word_atoms);
            if word_w > content_width {
                // Flush any pending content, then hard-split the long word.
                if line_w > indent_w {
                    out.push(std::mem::take(&mut line));
                }
                // Emit chunks of `content_width` visible columns; keep the
                // last chunk pending as the current line.
                let mut chunk_start = word.start;
                let mut chunk_w = 0;
                let mut first_chunk = true;
                for k in word.clone() {
                    let a = body[k];
                    if chunk_w + a.w > content_width && chunk_w > 0 {
                        let piece = render(logical, &body[chunk_start..k]);
                        if first_chunk {
                            // First chunk keeps the indent; it is emitted,
                            // not pending — only the word's last chunk stays
                            // in `line`.
                            out.push(format!("{indent_str}{piece}"));
                            first_chunk = false;
                        } else {
                            out.push(piece.to_string());
                        }
                        chunk_start = k;
                        chunk_w = 0;
                    }
                    chunk_w += a.w;
                }
                let tail = render(logical, &body[chunk_start..]);
                if first_chunk {
                    line = format!("{indent_str}{tail}");
                    line_w = indent_w + chunk_w;
                } else {
                    line = tail.to_string();
                    line_w = chunk_w;
                }
                continue;
            }
            let content_len = line_w.saturating_sub(indent_w);
            if content_len == 0 {
                line.push_str(word_str);
                line_w += word_w;
            } else if content_len + 1 + word_w > content_width {
                out.push(std::mem::take(&mut line));
                line = format!("{indent_str}{word_str}");
                line_w = indent_w + word_w;
            } else {
                line.push(' ');
                line.push_str(word_str);
                line_w += 1 + word_w;
            }
        }
        if line_w > 0 && !line.is_empty() {
            out.push(line);
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
/// are preserved. A line that already fits is returned as a borrow of the
/// input — the common case is one classification pass and no allocation.
/// An over-long line is cut at `width` visible columns, hard: a word longer
/// than the line is cut mid-word rather than reflowed, and a space that
/// lands on the boundary simply ends the line it is in.
pub fn split_to_width(text: &str, width: usize) -> Vec<Cow<'_, str>> {
    let width = width.max(1);
    let mut out: Vec<Cow<'_, str>> = Vec::new();
    for logical in text.split('\n') {
        match classify(logical) {
            // Pure printable ASCII: width is byte length. Fits → one borrow;
            // overflows → cut at byte offsets, no decoding at all.
            Class::Ascii(len) => {
                if len <= width {
                    out.push(Cow::Borrowed(logical));
                    continue;
                }
                let bytes = logical.as_bytes();
                let mut i = 0;
                while i < bytes.len() {
                    let end = (i + width).min(bytes.len());
                    out.push(Cow::Borrowed(&logical[i..end]));
                    i = end;
                }
            }
            // Has tabs / control chars: replace first (rare), split the
            // normalized text — pieces are owned copies of it.
            Class::Dirty => {
                let norm = normalize_dirty(logical);
                for piece in split_pieces(&norm, width) {
                    out.push(Cow::Owned(piece.to_string()));
                }
            }
            // Escapes and/or multi-byte chars: one ANSI-aware measure, and
            // only if that overflows does the line get cut.
            Class::Plain => {
                if display_width_ansi(logical) <= width {
                    out.push(Cow::Borrowed(logical));
                    continue;
                }
                for piece in split_pieces(logical, width) {
                    out.push(Cow::Borrowed(piece));
                }
            }
        }
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
    fn split_returns_a_borrow_on_the_common_path() {
        // The fitting-line check must not allocate a rebuilt copy.
        let parts = split_to_width("short ascii row", 80);
        assert_eq!(parts.len(), 1);
        assert!(matches!(parts[0], Cow::Borrowed(_)));
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
    fn split_counts_an_escapable_width_as_visible_only() {
        // OSC 8 hyperlink: the URL is an escape payload, zero columns.
        let link = "\x1b]8;;https://example.com/a/very/long/url\x1b\\link\x1b]8;;\x1b\\";
        assert_eq!(display_width_ansi(link), 4);
        assert_eq!(split_to_width(link, 4), vec![link]);
    }

    #[test]
    fn split_cuts_at_escapes_keeping_them_with_their_char() {
        // Escape run precedes "wor"; the cut at 4 columns happens before
        // the whole atom, so the escape stays with "wor" on line two.
        let parts = split_to_width("abcd\x1b[1mwor", 4);
        assert_eq!(parts, vec!["abcd", "\x1b[1mwor"]);
    }

    #[test]
    fn split_replaces_tabs_with_fixed_spaces() {
        // "a\tb" normalizes to "a    b" (a + 4 spaces + b): fits at 10; at 3
        // it cuts after "a" + two of the tab's spaces.
        assert_eq!(split_to_width("a\tb", 10), vec!["a    b"]);
        assert_eq!(split_to_width("a\tb", 3), vec!["a  ", "  b"]);
    }

    #[test]
    fn width_of_is_the_byte_length_for_ascii() {
        assert_eq!(width_of("plain text"), 10);
        // Leading whitespace counts (it is not the indent-capturing wrap).
        assert_eq!(width_of("  x"), 3);
    }

    #[test]
    fn width_of_measures_escapes_and_wide_glyphs() {
        assert_eq!(width_of("\x1b[1;35mmod\x1b[0mel"), 5);
        let link = "\x1b]8;;https://example.com/a/very/long/url\x1b\\link\x1b]8;;\x1b\\";
        assert_eq!(width_of(link), 4, "the URL is escape payload");
        assert_eq!(width_of("日本語"), 6);
    }

    #[test]
    fn width_of_normalizes_controls_first() {
        assert_eq!(width_of("a\tb"), 6, "tab is TAB_SPACES spaces");
        assert_eq!(width_of("a\x07b"), 3);
    }

    #[test]
    fn char_widths_aligns_with_chars_and_zeroes_escapes() {
        let s = "\x1b[1mab";
        assert_eq!(char_widths(s), vec![0, 0, 0, 0, 1, 1]);
        assert_eq!(char_widths("日"), vec![2]);
    }

    #[test]
    fn normalize_is_a_noop_for_clean_text() {
        assert!(matches!(normalize("plain \u{30}text"), Cow::Borrowed(_)));
        assert!(matches!(normalize("\x1b[1mbold\x1b[0m"), Cow::Borrowed(_)));
    }

    #[test]
    fn normalize_replaces_controls_and_drops_crlf() {
        assert_eq!(normalize("a\rb").as_ref(), "a b");
        assert_eq!(normalize("a\r\nb").as_ref(), "a\nb");
        assert_eq!(normalize("a\x07b").as_ref(), "a b");
        // Escape sequences survive verbatim, payload control bytes included.
        assert_eq!(normalize("\x1b]8;;url\x07x").as_ref(), "\x1b]8;;url\x07x");
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
    fn wrap_replaces_leading_tabs_in_indent() {
        // A tab in the indent is measured (and rendered) as fixed spaces.
        let lines = wrap("\tabc def ghi", 8);
        assert!(lines[0].starts_with("    "), "lines: {lines:?}");
        assert!(lines.iter().all(|l| !l.contains('\t')));
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
