//! Terminal row accounting: how many screen rows a byte stream occupies.
//!
//! The draw cache tracks the cursor so the next frame can anchor to it
//! (`ui::frame::DrawCache`), and every content writer has to tell it how far
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
//! - Escape handling mirrors [`crate::output::strip_ansi`] — CSI, and OSC
//!   terminated by BEL or ST. The OSC case is load-bearing: the markdown
//!   renderer emits OSC 8 hyperlinks on a real tty
//!   (`Theme::clickable_links`), and a scanner that stopped at the first
//!   alphabetic byte would count the URL as visible text.
//! - A padded row is counted at its true width, so a writer that pads for a
//!   background and a writer that does not both report the same geometry.

use std::io::{self, Write};

use unicode_width::UnicodeWidthChar;

/// Display width of a terminal row in columns: ANSI escapes are transparent,
/// wide characters count as two, combining marks as none.
///
/// Same escape rules as `strip_ansi`, without allocating. See
/// `width_matches_stripped_width` for the cross-check that keeps the two in
/// step.
pub fn display_width_ansi(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut width = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = escape_end(bytes, i);
            continue;
        }
        // Multi-byte UTF-8: decode one char so a wide glyph counts as one
        // character with width 2, not two characters of width 1.
        let rest = &s[i..];
        let ch = rest.chars().next().unwrap_or('\0');
        i += ch.len_utf8();
        width += UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    width
}

/// Index just past the escape sequence starting at `start` (which must be an
/// ESC): CSI (`ESC [ … final`), or OSC / other string sequences (`ESC ] …`
/// terminated by BEL or ST). Mirrors `strip_ansi`'s rules.
fn escape_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    if i < bytes.len() && bytes[i] == b'[' {
        // CSI: parameters/intermediates, then one final byte in 0x40..=0x7e.
        i += 1;
        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
            i += 1;
        }
        return (i + 1).min(bytes.len());
    }
    if i < bytes.len() && bytes[i] == b']' {
        // OSC: terminated by BEL, or by ST (the two bytes ESC \).
        i += 1;
        while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == 0x1b {
            return (i + 2).min(bytes.len());
        }
        return (i + 1).min(bytes.len());
    }
    // Any other two-character escape (ESC + a final byte, e.g. a bare ST):
    // zero width, and skipping both keeps a truncated sequence from leaving a
    // phantom column behind.
    if start + 1 < bytes.len() && (0x20..=0x7e).contains(&bytes[start + 1]) {
        return start + 2;
    }
    // Truncated sequence: keep it for the next write.
    bytes.len()
}

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

/// The tail of the content stream: does it currently end with a blank line?
///
/// This is the separator state, and it is *derived from the bytes* rather than
/// maintained by hand (doc/row_accounting_b3.md, B3d). A writer cannot forget
/// to update it, and it cannot disagree with the screen — which matters
/// because the markdown renderer also ends blocks with blank lines, so a
/// hand-kept flag drifts out of step with what the terminal shows and the
/// boundary writes a second blank.
///
/// A stream that has not written anything does not yet end in a blank line, so
/// the first `\n` a response writes is its leading separator rather than a
/// second one.
///
/// Shared and lock-free on purpose: the counter updates it *per byte*, so a
/// reader inside the same block — a boundary between two sub-blocks of one
/// counted write — sees the state the bytes have actually produced. Holding
/// it in a mutex would either deadlock or hand back the value from before the
/// block, which is how a committed paragraph ended up with no blank after it.
#[derive(Debug)]
pub struct StreamTail {
    /// bit 0: the previous byte was a newline. bit 1: the stream ends blank.
    bits: std::sync::atomic::AtomicU8,
}

impl Default for StreamTail {
    fn default() -> Self {
        StreamTail::new()
    }
}

impl StreamTail {
    pub const fn new() -> Self {
        StreamTail {
            // An unwritten stream counts as "previous byte was a newline", so
            // its first `\n` reads as the leading separator.
            bits: std::sync::atomic::AtomicU8::new(0b01),
        }
    }

    /// Fold one byte into the tail.
    fn note(&self, b: u8) {
        use std::sync::atomic::Ordering::SeqCst;
        let last_nl = u8::from(b == b'\n');
        let blank = if last_nl == 1 {
            // A newline leaves a blank line behind only if the line it closed
            // was itself empty, i.e. the previous byte was one too.
            (self.bits.load(SeqCst) & 0b01) != 0
        } else {
            false
        };
        self.bits.store(last_nl | u8::from(blank) << 1, SeqCst);
    }

    /// Does the stream end with a blank line — i.e. is the cursor sitting on a
    /// row nothing was written to?
    pub fn ends_blank(&self) -> bool {
        (self.bits.load(std::sync::atomic::Ordering::SeqCst) & 0b10) != 0
    }

    /// Is the cursor at the start of a line? True when nothing has been written
    /// yet, or the last byte was a newline.
    ///
    /// The precondition for writing a *transient* line (the status readout):
    /// it is erased by the next content write, and erasing a line that holds
    /// part of a block would destroy that block. So the check is the whole
    /// guard, and it is free — it is the same bit the counter maintains.
    pub fn at_line_start(&self) -> bool {
        (self.bits.load(std::sync::atomic::Ordering::SeqCst) & 0b01) != 0
    }
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
pub struct RowCounter<'a, W: Write> {
    inner: W,
    term_w: usize,
    /// Rows closed so far: one per `\n`, plus the wraps of the lines they end.
    rows: usize,
    /// Display columns on the current, unterminated line.
    col: usize,
    /// Bytes of an incomplete character or escape sequence, carried forward.
    pending: Vec<u8>,
    /// The stream's blank-line tail, shared with whoever asks about it: the
    /// counter updates it per byte, and a reader inside the same block sees
    /// what the bytes have actually produced.
    tail: &'a StreamTail,
}

impl<'a, W: Write> RowCounter<'a, W> {
    pub fn new(inner: W, term_w: usize, tail: &'a StreamTail) -> Self {
        RowCounter {
            inner,
            term_w: term_w.max(1),
            rows: 0,
            col: 0,
            pending: Vec::new(),
            tail,
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
                i = end; // zero width, and no effect on the tail
                continue;
            }
            self.tail.note(buf[i]);
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

impl<W: Write> Write for RowCounter<'_, W> {
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
                crate::output::strip_ansi(s)
                    .chars()
                    .map(|c| { unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) })
                    .sum::<usize>(),
                "width mismatch for {s:?}"
            );
        }
    }

    #[test]
    fn tail_tracks_whether_the_stream_ends_blank() {
        let tail = StreamTail::new();
        let mut counter = RowCounter::new(Vec::new(), 80, &tail);
        assert!(
            !tail.ends_blank(),
            "an empty stream is not yet a blank line"
        );
        counter.write_all(b"text\n").unwrap();
        assert!(!tail.ends_blank(), "one newline closes the line");
        counter.write_all(b"\n").unwrap();
        assert!(tail.ends_blank(), "two in a row leave a blank");
        counter.write_all(b"more").unwrap();
        assert!(!tail.ends_blank(), "text closes the blank");
        // An escape is zero width, so it must not close a blank line either.
        counter.write_all(b"\x1b[0m").unwrap();
        assert!(!tail.ends_blank());
    }

    #[test]
    fn a_fresh_streams_first_newline_is_its_leading_separator() {
        // A response that starts with `\n` (its leading blank) must leave the
        // tail blank, so the first block's boundary adds nothing on top.
        let tail = StreamTail::new();
        let mut counter = RowCounter::new(Vec::new(), 80, &tail);
        counter.write_all(b"\n").unwrap();
        assert!(tail.ends_blank());
    }

    #[test]
    fn the_tail_is_shared_across_writes_and_readable_mid_block() {
        // The property the host relies on: a second write continues the same
        // tail, and a reader *between* the two writes sees what the first
        // produced. Copying the tail in and out at block boundaries is what
        // made a committed paragraph lose the blank after it.
        let tail = StreamTail::new();
        let mut first = RowCounter::new(Vec::new(), 80, &tail);
        first.write_all(b"a\n\n").unwrap();
        assert!(tail.ends_blank(), "carried across writes");

        let mut second = RowCounter::new(Vec::new(), 80, &tail);
        second.write_all(b"b\n").unwrap();
        assert!(!tail.ends_blank(), "the second block closed the blank line");
    }

    #[test]
    fn counter_counts_what_it_writes() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let tail = StreamTail::new();
            let mut counter = RowCounter::new(&mut buf, 10, &tail);
            counter.write_all(b"abc\nde\n").unwrap();
            assert_eq!(counter.rows(), 2, "before the tail is written");
            counter.write_all(b"0123456789").unwrap();
            assert_eq!(counter.rows(), 2, "an exact-width tail does not wrap");
        }
        assert_eq!(String::from_utf8(buf).unwrap(), "abc\nde\n0123456789");
    }

    #[test]
    fn counter_sees_a_wrapped_tail() {
        let tail = StreamTail::new();
        let mut counter = RowCounter::new(Vec::new(), 4, &tail);
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
            let tail = StreamTail::new();
            let mut counter = RowCounter::new(Vec::new(), w, &tail);
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
            let tail = StreamTail::new();
            let mut counter = RowCounter::new(Vec::new(), 40, &tail);
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
        let tail = StreamTail::new();
        let mut counter = RowCounter::new(Vec::new(), 3, &tail);
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
            let tail = StreamTail::new();
            let mut counter = RowCounter::new(Vec::new(), w, &tail);
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
