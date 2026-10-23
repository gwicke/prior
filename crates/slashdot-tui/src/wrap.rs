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

use crate::ansi::{display_width_ansi, escape_end, escape_end_line, is_printable_ascii};

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
    /// multi-byte chars). The carried width is `Some` when every byte is
    /// printable ASCII or escape payload — the width [`classify_line`] already
    /// counted — and `None` when multi-byte chars need [`display_width_ansi`].
    Plain(Option<usize>),
    /// Contains a tab or another control character outside an escape
    /// sequence: must be normalized before measurement.
    Dirty,
}

/// One line as the classification walk saw it: where it ends and what it is.
struct LineClass {
    /// The line's bytes, `\n` terminator excluded.
    line: Range<usize>,
    /// Offset just past the line — the next line's start, or the text's end.
    next: usize,
    class: Class,
}

/// Single byte pass over the line starting at `start`: **finds** the line's end
/// and classifies it in the same walk, so a caller that needs both never
/// traverses the bytes twice.
///
/// Escape sequences are skipped whole (their payload is not classified —
/// control bytes inside an OSC belong to the sequence), and the skip is bounded
/// by the line: an unterminated sequence ends at the `\n` rather than eating
/// the next line. `\n` separators render as nothing.
fn classify_line(text: &str, start: usize) -> LineClass {
    let bytes = text.as_bytes();
    let mut i = start;
    let mut len = 0;
    let mut has_esc = false;
    let mut non_ascii = false;
    let class = loop {
        if i >= bytes.len() {
            break finish(len, has_esc, non_ascii);
        }
        let b = bytes[i];
        if b == b'\n' {
            break finish(len, has_esc, non_ascii);
        }
        if is_printable_ascii(b) {
            // A run of printable ASCII: one column per byte, no per-byte work.
            let from = i;
            while i < bytes.len() && is_printable_ascii(bytes[i]) {
                i += 1;
            }
            len += i - from;
        } else if b == 0x1b {
            has_esc = true;
            i = escape_end_line(bytes, i);
        } else if b < 0x20 || b == 0x7f {
            // tab, CR, BEL, BS, ...: the line is dirty, so stop classifying —
            // but its *range* still has to be found, and the rest of a dirty
            // line is the rare case a SIMD `memchr` is for.
            i = memchr::memchr(b'\n', &bytes[i..]).map_or(bytes.len(), |n| i + n);
            break Class::Dirty;
        } else {
            // C1 controls encode as U+0080..=U+009F: 0xc2 0x80..=0x9f.
            if b == 0xc2 && i + 1 < bytes.len() && (0x80..=0x9f).contains(&bytes[i + 1]) {
                i = memchr::memchr(b'\n', &bytes[i..]).map_or(bytes.len(), |n| i + n);
                break Class::Dirty;
            }
            non_ascii = true;
            i += 1; // continuation bytes are all >= 0x80; byte-wise is safe
        }
    };
    let terminated = i < bytes.len();
    LineClass {
        line: start..i,
        next: if terminated { i + 1 } else { i },
        class,
    }
}

/// `Class::Ascii` carries the *width* of the line: its printable-ASCII byte
/// count, which equals its display width (each such byte is one column). An
/// escape-only line is `Plain` with the same number, since escapes hold no
/// width — so a styled row is measured by the scan that classified it.
fn finish(len: usize, has_esc: bool, non_ascii: bool) -> Class {
    if has_esc || non_ascii {
        Class::Plain(if non_ascii { None } else { Some(len) })
    } else {
        Class::Ascii(len)
    }
}

/// Whole-text classification — the fold of [`classify_line`] over every line.
/// `\n` separators are excluded (they render as nothing), and one dirty line
/// makes the whole text dirty.
fn classify(text: &str) -> Class {
    let mut at = 0;
    let mut total = 0;
    let mut has_esc = false;
    let mut non_ascii = false;
    loop {
        let scan = classify_line(text, at);
        match scan.class {
            Class::Dirty => return Class::Dirty,
            Class::Ascii(w) => total += w,
            Class::Plain(w) => {
                has_esc = true;
                match w {
                    Some(w) => total += w,
                    None => non_ascii = true,
                }
            }
        }
        if scan.next >= text.len() {
            return finish(total, has_esc, non_ascii);
        }
        at = scan.next;
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
        // The scan already measured it when the line is ASCII plus escapes —
        // the common shape of a styled row — so no second pass.
        Class::Plain(Some(w)) => w,
        Class::Plain(None) => display_width_ansi(s),
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

/// One cut of an over-wide line: the piece's byte range and how far the walk
/// got — so the next cut **resumes** instead of re-measuring the remainder.
/// Escapes immediately preceding the overflowing char ride to the next line
/// with it (matching [`atoms`]: a cut never splits an escape run from its
/// char). Never returns an empty piece for non-empty input.
fn cut_once(bytes: &[u8], start: usize, width: usize) -> Range<usize> {
    let mut i = start;
    let mut used = 0;
    let mut run: Option<usize> = None; // start of the current escape run
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            run.get_or_insert(i);
            i = escape_end(bytes, i);
            continue;
        }
        let ch = char_width_of(bytes, i);
        if used + ch.1 > width && used > 0 {
            return start..run.unwrap_or(i);
        }
        used += ch.1;
        i += ch.0;
        run = None;
    }
    start..bytes.len()
}

/// The char at `i` and its width, without slicing the `str` — the walk in
/// [`cut_once`] only ever needs the two.
fn char_width_of(bytes: &[u8], i: usize) -> (usize, usize) {
    let len = crate::ansi::utf8_len(bytes[i]);
    let w = std::str::from_utf8(&bytes[i..(i + len).min(bytes.len())])
        .ok()
        .and_then(|s| s.chars().next())
        .map(char_width)
        .unwrap_or(0);
    (len, w)
}

/// Whole-line pieces of `line` at `width` columns — the cold path, reached
/// only after the caller's cheap check says the line overflows. Each cut
/// resumes where the last one stopped: the line is walked once, not once per
/// piece.
fn split_pieces(line: &str, width: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let range = cut_once(bytes, at, width);
        out.push(&line[range.clone()]);
        at = range.end;
    }
    if out.is_empty() {
        out.push(line);
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
    let mut at = 0;
    loop {
        // One walk per line: it finds the line's end *and* classifies it (and
        // measures it when the line is ASCII plus escapes), so a fitting line
        // costs a single traversal and no second pass.
        let scan = classify_line(text, at);
        let terminated = scan.next > scan.line.end;
        let logical = &text[scan.line.clone()];
        match scan.class {
            // Pure printable ASCII: width is byte length. Fits → one borrow;
            // overflows → cut at byte offsets, no decoding at all.
            Class::Ascii(len) => {
                if len <= width {
                    out.push(Cow::Borrowed(logical));
                } else {
                    let bytes = logical.as_bytes();
                    let mut i = 0;
                    while i < bytes.len() {
                        let end = (i + width).min(bytes.len());
                        out.push(Cow::Borrowed(&logical[i..end]));
                        i = end;
                    }
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
            // Escapes and/or multi-byte chars: the classification scan already
            // measured an ASCII-plus-escape line, so the fit check is free; only
            // a real multi-byte line is measured again, and only an overflowing
            // line is ever cut.
            Class::Plain(known) => {
                let w = known.unwrap_or_else(|| display_width_ansi(logical));
                if w <= width {
                    out.push(Cow::Borrowed(logical));
                } else {
                    for piece in split_pieces(logical, width) {
                        out.push(Cow::Borrowed(piece));
                    }
                }
            }
        }
        // A `\n`-terminated line has a successor: the next line, even if it is
        // the empty one a trailing terminator implies (the `split('\n')` shape
        // callers rely on — blank lines are preserved). Only the unterminated
        // last line ends the walk.
        if !terminated {
            break;
        }
        at = scan.next;
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

    /// `split('\n')` bounds N+1 lines for N separators: a trailing newline
    /// therefore implies a final empty line, and the fused walk must reproduce
    /// that exactly (it stops at the last terminator, not at the text's end).
    #[test]
    fn split_keeps_the_empty_line_a_trailing_newline_implies() {
        assert_eq!(split_to_width("a\n", 10), vec!["a", ""]);
        assert_eq!(split_to_width("\n", 10), vec!["", ""]);
        assert_eq!(split_to_width("a\n\n", 10), vec!["a", "", ""]);
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

    /// The fused walk keeps a dirty line's *range* whole: the classification
    /// bails at the offending byte, but the line still ends at its `\n` — so a
    /// tab mid-line never truncates the text the normalizer then sees.
    #[test]
    fn split_keeps_a_dirty_line_whole_before_normalizing() {
        assert_eq!(split_to_width("a\tb", 10), vec!["a    b"]);
        assert_eq!(split_to_width("a\tb\nc\td", 10), vec!["a    b", "c    d"]);
        // A dirty byte after a styled run: the escape is not re-classified, and
        // the line's end is still found.
        assert_eq!(
            split_to_width("\x1b[1ma\tb\x1b[0m", 10),
            vec!["\x1b[1ma    b\x1b[0m"]
        );
    }

    /// A fitting styled line — the shape most content rows have — is one
    /// borrow: the classification walk already measured it, so nothing is
    /// re-measured and nothing is copied.
    #[test]
    fn split_borrows_a_fitting_styled_line() {
        let s = "\x1b[1;36mhighlighted\x1b[0m and plain";
        let parts = split_to_width(s, 80);
        assert_eq!(parts.len(), 1);
        assert!(matches!(parts[0], Cow::Borrowed(_)));
        assert_eq!(parts[0], s);
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
