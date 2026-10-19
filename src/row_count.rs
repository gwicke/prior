//! Terminal row accounting: how many screen rows a byte stream occupies.
//!
//! The draw cache tracks the cursor so the next frame can anchor to it
//! (`draw::DrawCache`), and every content writer has to tell it how far
//! the cursor moved. Guessing that from the *source* text is where the drift
//! came from: the bytes that reach the terminal are a prefix plus rendered
//! markdown, ANSI styling, panels, and lines that wrap — none of which the
//! source knows about. So the count is taken at the only place the answer
//! exists: the bytes, on their way out.
//!
//! The quantity is **how far the cursor moves down**, which is what
//! `note_lines` needs: a line of `w` columns at `width` pushes the cursor down
//! `(w - 1) / width` times (it ends *on* the last row, not past it), and each
//! `\n` moves it down one more. An unterminated final line therefore counts
//! only its wraps — writing `abc` leaves the cursor on the same row, so the
//! caret delta is zero. Rows *occupied* would be a different number, and
//! using it would push every following anchor one row too low.
//!
//! Design notes (doc/row_accounting.md §6, phase B):
//! - Counting happens per *block*, not per delta, so the streaming path pays
//!   one scan per rendered block.
//! - Escape handling mirrors [`crate::ansi::strip_ansi`] — CSI, and OSC
//!   terminated by BEL or ST. The OSC case is load-bearing: the markdown
//!   renderer emits OSC 8 hyperlinks on a real tty
//!   (`Theme::clickable_links`), and a scanner that stopped at the first
//!   alphabetic byte would count the URL as visible text.
//! - A padded row is counted at its true width, so a writer that pads for a
//!   background and a writer that does not both report the same geometry.

use std::io::{self, Write};

use unicode_width::UnicodeWidthChar;

use crate::ansi::escape_end;

/// The OSC-aware width measure, owned by `slashdot_tui::ansi` and re-exported
/// here so prior's callers (`ui/bar`, `agent/status`, and this module's
/// tests) keep one path. See [`crate::ansi::display_width_ansi`].
pub use crate::ansi::display_width_ansi;
/// The explicit-wrap spec, owned by `slashdot_tui::ansi` and re-exported here.
/// See [`crate::ansi::wrap_to_width`].
pub use crate::ansi::wrap_to_width;

/// How far a run of `width_cols` columns moves the cursor down: once per wrap,
/// since the cursor ends *on* the run's last row.
fn wrap_rows(width_cols: usize, term_w: usize) -> usize {
    if width_cols == 0 {
        0
    } else {
        (width_cols - 1) / term_w
    }
}

/// Truncate `text` to `width` display columns, appending an ellipsis when it
/// does not fit. Display-width aware, so a wide tail is not cut mid-character.
pub fn truncate_to_width(text: &str, width: usize) -> String {
    if width == 0 || display_width_ansi(text) <= width {
        return text.to_string();
    }
    let budget = width.saturating_sub(1); // room for the ellipsis
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// How far the cursor moves down when `text` is written at `term_w` columns —
/// the count for a writer that has its bytes in hand (a message, a rendered
/// markdown block, a pre-wrapped panel line).
///
/// One row per `\n` (the newline moves off the row it ends) plus the wraps of
/// each line. A final line with no newline adds only its wraps: the cursor
/// comes to rest *on* it. Empty text moves nothing.
pub fn rows_of(text: &str, term_w: usize) -> usize {
    let term_w = term_w.max(1);
    let mut rows = 0;
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            rows += 1; // this newline closed the previous line
        }
        rows += wrap_rows(display_width_ansi(line), term_w);
    }
    rows
}

/// A writer that counts the terminal rows passing through it.
///
/// Wrap the real writer, render into this, and ask [`RowCounter::rows`] for
/// the geometry: the bytes went out exactly as they would have, and the count
/// is of *those* bytes. A writer cannot forget to count — it writes through
/// this, so counting is the path, not an extra step beside it.
///
/// The counter is fed a byte stream and measures it as a stream: a write may
/// split a character or an escape sequence, and the remainder is carried to
/// the next one. (If a caller ever handed the terminal a split character, the
/// terminal would render the replacement glyph — the carry is here so the count
/// stays right regardless.)
pub struct RowCounter<W: Write> {
    inner: W,
    term_w: usize,
    /// Rows closed so far: one per `\n`, plus the wraps of the lines they end.
    rows: usize,
    /// Display columns on the current, unterminated line.
    col: usize,
    /// Bytes of an incomplete character or escape sequence, carried forward.
    pending: Vec<u8>,
}

impl<W: Write> RowCounter<W> {
    pub fn new(inner: W, term_w: usize) -> Self {
        RowCounter {
            inner,
            term_w: term_w.max(1),
            rows: 0,
            col: 0,
            pending: Vec::new(),
        }
    }

    /// How far the cursor moved down over the whole stream. Call once, after
    /// the last write.
    pub fn rows(&self) -> usize {
        // The open line contributes its wraps: the cursor comes to rest on it,
        // it does not move past it.
        self.rows + wrap_rows(self.col, self.term_w)
    }

    /// The wrapped writer, for handing back after the count is taken.
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Consume as much of `buf` as can be measured, carrying the rest.
    fn count(&mut self, buf: &[u8]) {
        // Anything carried over is prepended, so the scan below always starts
        // at a character or escape boundary.
        let carried = if self.pending.is_empty() {
            Vec::new()
        } else {
            let mut all = std::mem::take(&mut self.pending);
            all.extend_from_slice(buf);
            all
        };
        let buf: &[u8] = if carried.is_empty() { buf } else { &carried };
        let mut i = 0;
        while i < buf.len() {
            if buf[i] == 0x1b {
                let end = escape_end(buf, i);
                if end >= buf.len() {
                    // Incomplete escape sequence: keep it for the next write.
                    self.pending = buf[i..].to_vec();
                    return;
                }
                i = end; // zero width
                continue;
            }
            match std::str::from_utf8(&buf[i..]) {
                Ok(rest) => {
                    let ch = rest.chars().next().unwrap_or('\0');
                    if ch == '\n' {
                        self.rows += 1 + wrap_rows(self.col, self.term_w);
                        self.col = 0;
                    } else {
                        self.col += UnicodeWidthChar::width(ch).unwrap_or(0);
                    }
                    i += ch.len_utf8();
                }
                Err(e) if e.error_len().is_none() => {
                    // Valid prefix, character cut short by the buffer end.
                    self.pending = buf[i..].to_vec();
                    return;
                }
                Err(_) => {
                    // Not UTF-8 at all. Count the byte as one column: the
                    // terminal will render something, and over-counting is the
                    // safe direction for a caret delta.
                    self.col += 1;
                    i += 1;
                }
            }
        }
    }
}

impl<W: Write> Write for RowCounter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.count(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero_rows() {
        assert_eq!(rows_of("", 80), 0);
    }

    #[test]
    fn a_line_without_a_newline_does_not_move_the_cursor_down() {
        // The distinction this whole module turns on: the text occupies a row,
        // but the cursor stays *on* it, so the caret delta is zero. Counting
        // the occupied row instead would push every later anchor one row low.
        assert_eq!(rows_of("hello", 80), 0);
        assert_eq!(rows_of("", 80), 0);
    }

    #[test]
    fn each_newline_moves_one_row_down() {
        assert_eq!(rows_of("a\n", 80), 1);
        assert_eq!(rows_of("a\n\n", 80), 2);
        assert_eq!(rows_of("\n", 80), 1, "a blank line is still a row");
        // "a\nb": one newline, and the cursor comes to rest on the b row.
        assert_eq!(rows_of("a\nb", 80), 1);
    }

    #[test]
    fn exact_fit_does_not_wrap() {
        assert_eq!(rows_of(&"x".repeat(80), 80), 0);
        assert_eq!(rows_of(&format!("{}\n", "x".repeat(80)), 80), 1);
        assert_eq!(rows_of(&"x".repeat(81), 80), 1, "one char wraps");
        assert_eq!(rows_of(&format!("{}\n", "x".repeat(81)), 80), 2);
    }

    #[test]
    fn wraps_are_counted_per_line() {
        // A full line + newline, then 81 columns: 1 + (1 wrap) = 2.
        let text = format!("{}\n{}", "x".repeat(80), "y".repeat(81));
        assert_eq!(rows_of(&text, 80), 2);
    }

    #[test]
    fn wide_tail_wraps_without_a_newline() {
        // 6 columns at width 5: the cursor wraps once and rests there.
        assert_eq!(rows_of("日本語", 5), 1);
        assert_eq!(rows_of("日本語\n", 5), 2);
    }

    #[test]
    fn escapes_are_zero_width() {
        let plain = "hello world";
        let styled = "\x1b[1;36mhello\x1b[0m \x1b[48;5;239mworld\x1b[0m";
        assert_eq!(display_width_ansi(styled), display_width_ansi(plain));
        assert_eq!(rows_of(styled, 80), rows_of(plain, 80));
    }

    #[test]
    fn osc8_hyperlinks_are_zero_width() {
        // What the markdown renderer emits on a real tty. A scanner that
        // stopped at the first alphabetic byte would count the URL as text
        // and report far too many rows.
        let s = "\x1b]8;;https://example.com/a-long-url\x07link text\x1b]8;;\x07";
        assert_eq!(display_width_ansi(s), "link text".len());
    }

    #[test]
    fn wide_characters_count_as_two_columns() {
        assert_eq!(display_width_ansi("日本"), 4);
        // Padded to a full row by a writer that wants a background: exactly 80
        // columns, so no wrap and no movement.
        assert_eq!(rows_of(&format!("日本{}", " ".repeat(76)), 80), 0);
    }

    #[test]
    fn width_matches_stripped_width() {
        // The cross-check that keeps this scanner in step with the
        // established one: same escape rules, no allocation here.
        for s in [
            "plain",
            "\x1b[1;36mcyan\x1b[0m",
            "\x1b[48;5;239m bg \x1b[0m tail",
            "\x1b]8;;https://example.com\x07link\x1b]8;;\x07",
            "\x1b]8;;https://example.com\x1b\\link\x1b\\",
            "\x1b]8;;url\x07link\x1b]8;;\x07tail",
            // well-formed ST-terminated hyperlink, as a real renderer emits
            "\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\",
            "日本語 mixed with ascii",
            "trailing spaces   ",
            "\x1b[2mdim\x1b[0m\x1b[0m",
            "",
        ] {
            assert_eq!(
                display_width_ansi(s),
                crate::ansi::strip_ansi(s)
                    .chars()
                    .map(|c| { unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) })
                    .sum::<usize>(),
                "width mismatch for {s:?}"
            );
        }
    }

    #[test]
    fn wrap_to_width_breaks_where_the_terminal_wraps() {
        // Short lines pass through untouched — no invented breaks.
        assert_eq!(wrap_to_width("abc", 5), "abc");
        // Exactly the budget: the terminal would leave the row wrap-pending,
        // and the caller's own `\n` resolves that — nothing to add.
        assert_eq!(wrap_to_width("abcde", 5), "abcde");
        // One byte over: the break is exactly where the terminal wraps.
        assert_eq!(wrap_to_width("abcdef", 5), "abcde\nf");
        // Existing newlines are line boundaries, not wrap points.
        assert_eq!(wrap_to_width("abc\nde", 5), "abc\nde");
        assert_eq!(wrap_to_width("abcdefgh\nij", 5), "abcde\nfgh\nij");
        // A wide glyph straddling the edge moves whole, leaving the gap the
        // terminal would leave.
        assert_eq!(wrap_to_width("abcd日", 5), "abcd\n日");
        assert_eq!(wrap_to_width("日日日", 4), "日日\n日");
        // Escapes ride with the head and are never split: zero-width, they
        // are consumed even at the budget, so the reset lands before the break
        // — same cells, same styles after it.
        let styled = "\x1b[1mabcde\x1b[0mf";
        assert_eq!(wrap_to_width(styled, 5), "\x1b[1mabcde\x1b[0m\nf");
    }

    /// The accounting contract: the row count a writer hands `note_lines`
    /// must not move when a line is wrapped at write time — `rows_of` counts
    /// a natural wrap and an inserted `\n` the same way.
    ///
    /// Invariance holds where `rows_of`'s column arithmetic and real cell
    /// breaks agree — narrow glyphs, which is every byte a renderer or a
    /// message actually emits. (Wide glyphs at a non-dividing width wrap
    /// earlier than `(cols - 1) / term_w` predicts: `rows_of` idealizes.
    /// Explicit wrapping moves the count *toward* the terminal, not away
    /// from it — that standing question is not this pin's to settle.)
    #[test]
    fn wrap_to_width_keeps_rows_of_honest() {
        let long = "x".repeat(100);
        let texts = [
            "abcdef",
            "abcdefghij",
            "a\nbcdefghijkl",
            long.as_str(),
            "\x1b[1mabcdefgh\x1b[0m and more",
            "",
            "exact",
        ];
        for w in [3usize, 5, 10, 80] {
            for text in texts {
                assert_eq!(
                    rows_of(text, w),
                    rows_of(&wrap_to_width(text, w), w),
                    "rows_of moved for {text:?} at width {w}"
                );
            }
        }
    }

    /// Every row the wrap produces fits the terminal — the whole point: a
    /// part wider than the budget would wrap naturally again, and land back
    /// on an uncleared row.
    #[test]
    fn wrap_to_width_never_emits_an_overwide_row() {
        for w in [1usize, 3, 4, 5, 10] {
            for text in [
                "x".repeat(97).as_str(),
                "abcd日日日ef",
                "\x1b[1mabc\x1b[0mdefghijklmn",
                "a\nbb\nccc\ndddd",
            ] {
                for part in wrap_to_width(text, w).split('\n') {
                    assert!(
                        crate::row_count::display_width_ansi(part) <= w.max(2),
                        "part wider than {w} for {text:?}: {part:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn counter_counts_what_it_writes() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut counter = RowCounter::new(&mut buf, 10);
            counter.write_all(b"abc\nde\n").unwrap();
            assert_eq!(counter.rows(), 2, "before the tail is written");
            counter.write_all(b"0123456789").unwrap();
            assert_eq!(counter.rows(), 2, "an exact-width tail does not wrap");
        }
        assert_eq!(String::from_utf8(buf).unwrap(), "abc\nde\n0123456789");
    }

    #[test]
    fn counter_sees_a_wrapped_tail() {
        let mut counter = RowCounter::new(Vec::new(), 4);
        counter.write_all(b"abcdefghij").unwrap();
        assert_eq!(counter.rows(), 2, "10 columns at width 4 wrap twice");
        counter.write_all(b"klmno").unwrap();
        assert_eq!(counter.rows(), 3, "16 columns at width 4 wrap three times");
    }

    #[test]
    fn counter_agrees_with_rows_of() {
        for (text, w) in [
            ("a\n", 80),
            ("abc", 80),
            ("x".repeat(81).as_str(), 80),
            ("a\nbb\nccc", 2),
            ("\x1b[1mx\x1b[0m\ny", 80),
        ] {
            let mut counter = RowCounter::new(Vec::new(), w);
            counter.write_all(text.as_bytes()).unwrap();
            assert_eq!(
                counter.rows(),
                rows_of(text, w),
                "counter and rows_of disagree on {text:?} at width {w}"
            );
        }
    }

    #[test]
    fn counter_handles_writes_split_inside_characters_and_escapes() {
        // A write boundary can fall anywhere: mid-character (the 3-byte CJK
        // glyph, the 2-byte box-drawing char) or mid-escape. The count must
        // not notice.
        let text =
            "\u{1b}[1m日\u{1b}[0m本\u{1b}]8;;https://x.example/y\u{1b}\\ok\u{1b}]8;;\u{1b}\\";
        for chunk in [1usize, 2, 3, 5, 7] {
            let mut counter = RowCounter::new(Vec::new(), 40);
            for part in text.as_bytes().chunks(chunk) {
                counter.write_all(part).unwrap();
            }
            assert_eq!(
                counter.rows(),
                rows_of(text, 40),
                "chunk size {chunk} disagrees"
            );
        }
    }

    #[test]
    fn counter_ignores_a_truncated_escape_until_it_completes() {
        // Width 3, so a phantom column from the partial escape would wrap and
        // show up in the count rather than hide in the slack.
        let mut counter = RowCounter::new(Vec::new(), 3);
        counter.write_all(b"ab\x1b[3").unwrap();
        assert_eq!(counter.rows(), 0, "the partial escape is not text yet");
        counter.write_all(b"1mcd\n").unwrap();
        assert_eq!(counter.rows(), 2, "\"abcd\\n\" is 2 rows at width 3");
        assert_eq!(rows_of("abcd\n", 3), counter.rows());
    }

    #[test]
    fn counter_and_rows_of_agree_on_long_text() {
        let text = "alpha beta\n\ngamma delta epsilon\nzeta";
        for w in [1usize, 2, 7, 40, 200] {
            let mut counter = RowCounter::new(Vec::new(), w);
            for chunk in text.as_bytes().chunks(3) {
                counter.write_all(chunk).unwrap();
            }
            assert_eq!(
                counter.rows(),
                rows_of(text, w),
                "disagreement at width {w} for {text:?}"
            );
        }
    }
}
