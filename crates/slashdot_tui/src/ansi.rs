//! Byte-level ANSI transforms and the `io::Write` wrappers of the content
//! write pipeline.
//!
//! Everything here is a pure transform over a byte stream — the width / escape
//! math ([`strip_ansi`], [`utf8_len`], `bytes_width`) plus the pipeline
//! wrappers ([`WrapLines`], [`LineClear`], [`StripLeading`], [`HoldTrailing`])
//! that `Content::with_content_writer` stacks around one content write.
//! No terminal is touched from here; the wrappers rewrite bytes on their way
//! to whatever writer sits beneath them, and the *policy* of when to stack
//! which — the boundary rule, the hold accounting — stays with that caller.
//!
//! Extracted from `output.rs`.

use std::io;

use unicode_width::UnicodeWidthChar;

/// A finished block with its trailing blank lines stripped, so what is emitted
/// ends at a line boundary with **no** blank row.
///
/// That is the state `boundary_spacer` needs: its single newline then lands as
/// exactly one blank row — never two, and never a newline spent just closing a
/// line that had no blank after it. The one line terminator the last line needs
/// is put back, which is the "strip, then add by rule" of the normalization.
pub fn at_line_start(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    while out.last() == Some(&b'\n') {
        out.pop();
    }
    if !out.is_empty() {
        out.push(b'\n');
    }
    out
}

/// Where inside an escape sequence the byte stream is, so escape bytes are
/// passed through without counting as cells. `St` is the string-terminator
/// tail (`ESC \`) of OSC/DCS families.
#[derive(PartialEq, Clone, Copy)]
pub enum EscState {
    None,
    /// The `ESC` itself has been seen; the next byte picks the family.
    Escape,
    Csi,
    Osc,
    Str,
    OscSt,
    StrSt,
}

/// Content's width guard — the safety net under the renderer's own wrapping
/// (`codewandler-markdown` wraps code and prose at the source) and the last
/// thing before the terminal would wrap on its own.
///
/// Any line wider than `width` gets a newline inserted at the cell where the
/// terminal would have wrapped, so the continuation row is **entered by a
/// byte** and the [`LineClear`] below arms its `EL` there (ui_draw_261001: a
/// natural wrap crosses rows with no byte between, and the continuation
/// keeps whatever the frame below painted — the status-bar remnant). The
/// break is cell-exact only: no word preference, no indent — that is the
/// renderer's job; this struct's job is that *nothing* over-wide escapes.
///
/// Colors survive by construction: escapes pass through whole (and are never
/// counted), SGR state belongs to the terminal, and an inserted newline does
/// not touch it. Sits **above the counter** (the inserted `\n` counts as the
/// row move it replaces — `rows_of` counts a wrap and a `\n` alike, so
/// `note_lines` does not move) and **above the line-clear**, **below the
/// hold** (held newlines reset `col`). A capture passes through raw
/// (`enabled == false`) — captured bytes are assertions against content, not
/// against a screen.
///
/// State is per write *call*: the content contract opens every call at line
/// start (blocks are line-terminated, and the renderer flushes only complete
/// lines — split-equivalence), so `col` seeds at 0.
pub struct WrapLines<'a, W: io::Write + ?Sized> {
    pub inner: &'a mut W,
    pub width: usize,
    pub enabled: bool,
    /// Display columns since the line's last newline.
    pub col: usize,
    /// Bytes of a multi-byte char split across writes. Held, not emitted:
    /// the wrap decision must be made before the char's first byte.
    pub partial: Vec<u8>,
    pub esc: EscState,
}

/// Display width of a complete (valid UTF-8) byte sequence: one char's
/// width, zero-width for anything undecodable.
fn bytes_width(b: &[u8]) -> usize {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.chars().next())
        .and_then(UnicodeWidthChar::width)
        .unwrap_or(0)
}

impl<W: io::Write + ?Sized> WrapLines<'_, W> {
    /// Emit one visible unit (already width-measured), inserting the wrap
    /// newline first when it would cross the edge.
    fn put(&mut self, bytes: &[u8], cw: usize) -> io::Result<()> {
        if self.col > 0 && self.col + cw > self.width {
            self.inner.write_all(b"\n")?;
            self.col = 0;
        }
        self.inner.write_all(bytes)?;
        self.col += cw;
        Ok(())
    }
}

impl<W: io::Write + ?Sized> io::Write for WrapLines<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.enabled {
            return self.inner.write(buf);
        }
        let mut i = 0usize;
        // A char held from the previous write completes here: decide the wrap
        // before its bytes move.
        if !self.partial.is_empty() {
            let need = utf8_len(self.partial[0]);
            while self.partial.len() < need && i < buf.len() {
                self.partial.push(buf[i]);
                i += 1;
            }
            if self.partial.len() < need {
                return Ok(i);
            }
            let cw = bytes_width(&self.partial);
            let done = std::mem::take(&mut self.partial);
            self.put(&done, cw)?;
        }
        while i < buf.len() {
            let b = buf[i];
            match self.esc {
                EscState::None => {
                    if b == 0x1b {
                        self.esc = EscState::Escape;
                    } else if b == b'\n' {
                        self.inner.write_all(&buf[i..i + 1])?;
                        self.col = 0;
                        i += 1;
                        continue;
                    } else if b < 0x80 {
                        let cw = UnicodeWidthChar::width(b as char).unwrap_or(0);
                        self.put(&buf[i..i + 1], cw)?;
                        i += 1;
                        continue;
                    } else {
                        let need = utf8_len(b);
                        if i + need > buf.len() {
                            self.partial.extend_from_slice(&buf[i..]);
                            return Ok(buf.len());
                        }
                        let cw = bytes_width(&buf[i..i + need]);
                        let end = i + need;
                        self.put(&buf[i..end], cw)?;
                        i = end;
                        continue;
                    }
                }
                EscState::Escape => {
                    self.esc = match b {
                        b'[' => EscState::Csi,
                        b']' => EscState::Osc,
                        b'P' | b'X' | b'^' | b'_' => EscState::Str,
                        _ => EscState::None,
                    };
                }
                EscState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.esc = EscState::None;
                    }
                }
                EscState::Osc => {
                    if b == 0x07 {
                        self.esc = EscState::None;
                    } else if b == 0x1b {
                        self.esc = EscState::OscSt;
                    }
                }
                EscState::OscSt => {
                    self.esc = if b == b'\\' {
                        EscState::None
                    } else {
                        EscState::Osc
                    };
                }
                EscState::Str => {
                    if b == 0x1b {
                        self.esc = EscState::StrSt;
                    }
                }
                EscState::StrSt => {
                    self.esc = if b == b'\\' {
                        EscState::None
                    } else {
                        EscState::Str
                    };
                }
            }
            self.inner.write_all(&buf[i..i + 1])?;
            i += 1;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Content's line-clear transform: emits `\x1b[K` on **row entry** — before
/// the first byte of every line the stream enters (at write start, and after
/// every newline written). The pair "clear, then bytes" is the overwrite rule
/// (ui_draw_261001): one pass, no separate clearing walk, and a content line
/// shorter than the terminal can never leave the row's old tail showing.
///
/// Never mid-line: a wrap-pending row must not be cleared into (`EL` from the
/// last written cell would eat it), so `pending` only arms at a line's start.
/// A capture bypasses it (`enabled == false`) — captured bytes are assertions
/// against content, not against a screen.
pub struct LineClear<'a, W: io::Write + ?Sized> {
    pub inner: &'a mut W,
    /// True while the row the next byte would land on has not been cleared.
    pub pending: bool,
    pub enabled: bool,
}

impl<W: io::Write + ?Sized> io::Write for LineClear<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.enabled || buf.is_empty() {
            return self.inner.write(buf);
        }
        let mut out = Vec::with_capacity(buf.len() + 6);
        let mut rest = buf;
        while !rest.is_empty() {
            if self.pending {
                out.extend_from_slice(b"\x1b[K");
                self.pending = false;
            }
            let take = match rest.iter().position(|&b| b == b'\n') {
                Some(i) => i + 1, // include the newline itself
                None => rest.len(),
            };
            out.extend_from_slice(&rest[..take]);
            self.pending = rest[take - 1] == b'\n';
            rest = &rest[take..];
        }
        self.inner.write_all(&out)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Drops the leading blank lines of the block being written, passing the rest
/// through untouched.
///
/// Stops at the first byte that is not a newline, so from that point on it is
/// inert: interior paragraph gaps, and a block written across several calls,
/// are unaffected — only the very first bytes of the block are examined.
/// Content that never starts (an all-newline chunk) is simply withheld, which
/// is the whole point: the spacer already supplied the row it would have
/// stacked on.
pub struct StripLeading<'a, W: io::Write + ?Sized> {
    pub inner: &'a mut W,
    /// Cleared once a non-newline byte has passed.
    pub armed: bool,
}

impl<W: io::Write + ?Sized> io::Write for StripLeading<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.armed {
            self.inner.write_all(buf)?;
        } else if let Some(i) = buf.iter().position(|b| *b != b'\n') {
            self.armed = true;
            self.inner.write_all(&buf[i..])?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Withholds the blank rows a write ends with, releasing them only if more
/// content arrives in the same block.
///
/// Not a trim — the separator still exists, it is merely written by the *next*
/// write rather than by this one. A kind change never asks for it (the caller
/// drops it, and `boundary_spacer` supplies that boundary's divider instead),
/// and the end of the stream never asks for it either, because there is no next
/// write. That is the whole contract in one structure: internal separators are
/// preserved, only *when* they are emitted moves, and the response's last one
/// disappears without anything having to know, as it writes, whether more
/// content follows.
///
/// One newline is never withheld: that is the line terminator closing the last
/// line of content, not a blank row — without it the stream would sit
/// mid-line.
pub struct HoldTrailing<'a, W: io::Write + ?Sized> {
    pub inner: &'a mut W,
    /// Newlines withheld since the last byte of content.
    pub pending: usize,
    /// Whether anything that is *not* a newline passed through — the
    /// difference between a write that still owes its line terminator and one
    /// that is nothing but separators.
    pub wrote_content: bool,
    /// Whether any byte at all passed — the difference between a write with
    /// nothing in it and one whose only content was newlines.
    pub wrote_any: bool,
}

impl<W: io::Write + ?Sized> HoldTrailing<'_, W> {
    /// Hand back everything still withheld, once the content of this write is
    /// in.
    ///
    /// The decision comes from the content alone — no reading of the stream —
    /// because that is the convention every emitter is held to: content ends
    /// with exactly one newline and no separator lines. Of `pending` trailing
    /// newlines the first is that terminator and stays (the block ends at a
    /// line boundary); the rest are empty lines and are held for whoever comes
    /// next.
    ///
    /// A write that produced no content owes no terminator. It is nothing but
    /// separators, and separators are written only when same-kind content
    /// arrives to sit below them — so writing one here, on a stream that by
    /// convention already ends at a line boundary, would be inventing a blank
    /// row from a write that emitted no content at all.
    pub fn finish(&mut self) -> usize {
        if self.pending > 0 && self.wrote_content {
            let _ = self.inner.write_all(b"\n");
            self.pending -= 1;
        }
        std::mem::take(&mut self.pending)
    }
}

impl<W: io::Write + ?Sized> io::Write for HoldTrailing<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !buf.is_empty() {
            self.wrote_any = true;
        }
        match buf.iter().rposition(|b| *b != b'\n') {
            // Ends with content: what was held turns out to be interior after
            // all, so release it, then take this chunk and hold its tail.
            Some(i) => {
                self.wrote_content = true;
                if self.pending > 0 {
                    let held = std::mem::take(&mut self.pending);
                    let blank = vec![b'\n'; held];
                    self.inner.write_all(&blank)?;
                }
                self.inner.write_all(&buf[..=i])?;
                self.pending += buf.len() - i - 1;
            }
            // All newlines: trailing until something says otherwise.
            None => self.pending += buf.len(),
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Visible width of `s`, ignoring ANSI SGR escapes (so styled cells size
/// correctly). Falls back to width 1 for control/ambiguous chars.
///
/// Moved here from `slashdot_menu::table`: the
/// frame layers measure rows with it (`pad_row`, `el_if_short`) and the menu
/// measures cells with it — one answer, below both.
pub fn display_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip an escape sequence up to and including the final letter.
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        w += char_width(c);
    }
    w
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1)
}

/// Remove ANSI/CSI escape sequences (e.g. `\x1b[1;36m`) so that
/// downstream width/wrap math only counts *visible* characters.
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            // ESC [ ... <final>  (CSI)  or  ESC ] ... BEL/ST  (OSC): skip to terminator.
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                } // consume final byte
                continue;
            }
            if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                i += 2;
                while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == 0x1b {
                    // ST is ESC \ — both bytes belong to the sequence, and
                    // leaving the backslash behind put a phantom column in
                    // every width measured from a hyperlink-terminated string
                    // (row_count::display_width_ansi is pinned against this).
                    i += 2;
                } else if i < bytes.len() {
                    i += 1; // consume BEL
                }
                continue;
            }
            if i + 1 < bytes.len() && (0x20..=0x7e).contains(&bytes[i + 1]) {
                // Any other two-character escape (a bare ST after a truncated
                // sequence, for instance): ESC plus its final byte, zero
                // width. Without this the ESC became U+FFFD and the final
                // byte a visible column.
                i += 2;
                continue;
            }
        }
        // bytes[i] is either ASCII or a UTF-8 lead byte; copy the
        // whole char to preserve multi-byte content.
        let ch_len = utf8_len(bytes[i]);
        if i + ch_len <= bytes.len() {
            out.push_str(std::str::from_utf8(&bytes[i..i + ch_len]).unwrap_or("\u{fffd}"));
        }
        i += ch_len.max(1);
    }
    out
}

/// [`strip_ansi`] over raw bytes: drop every CSI / OSC / two-byte escape and
/// keep the visible bytes, with no UTF-8 decode (the caller's bytes are
/// already valid UTF-8, and copying them verbatim is the point).
///
/// This is the non-tty output edge (design §TermIO): the bytes were composed
/// and measured as if a terminal would receive them, so removing the escapes
/// is **zero-width** — the geometry decided above is untouched, and a pipe
/// consumer sees only the text. A truncated trailing escape is consumed to
/// the end of the slice: half a sequence is not text.
pub fn strip_ansi_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = escape_end(bytes, i);
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// UTF-8 lead byte -> byte length (1..=4).
pub fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// Display width of a terminal row in columns: ANSI escapes are transparent,
/// wide characters count as two, combining marks as none.
///
/// The OSC-aware measure: unlike [`display_width`], an OSC 8 hyperlink's URL
/// is not counted as text (content rows carry them, so a CSI-only measure
/// reads a 9-column link as 39). Escape rules match [`strip_ansi`], without
/// allocating. Owned by this crate so UI and content share one measure below
/// both layers; picocode's `row_count` re-exports it.
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
pub fn escape_end(bytes: &[u8], start: usize) -> usize {
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

/// Wrap `text`'s lines at `width` display columns — the terminal's own wrap
/// points, made explicit as `\n` bytes.
///
/// The overwrite rule (ui_draw_261001) needs every terminal row a content
/// line occupies to be **entered by a byte**: [`LineClear`] arms on `\n` and
/// at write start only, so a row the terminal wraps into *without* a byte
/// never gets its entry `EL` and keeps whatever the frame before it painted.
/// Wrapping here is cell-identical to the natural wrap (same break columns,
/// wide glyphs moved whole, escapes never split), and the row count does not
/// move: `rows_of` counts a wrap and a `\n` the same way.
///
/// The batch form of the guard: production wraps incrementally in
/// [`WrapLines`] (the one choke `with_content_writer` puts under every
/// writer). This stays as the readable specification of the break rule and
/// the accounting pin both forms share.
pub fn wrap_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut rest = line;
        while display_width_ansi(rest) > width {
            let (head, tail) = split_at_width(rest, width);
            out.push_str(head);
            if tail.is_empty() {
                // The head swallowed the remainder — a glyph wider than the
                // budget. The line ends here; a break after it would invent
                // an empty row the terminal never shows.
                rest = "";
                break;
            }
            out.push('\n');
            rest = tail;
        }
        out.push_str(rest);
    }
    out
}

/// Byte index at which `s` first exceeds `width` display columns: escapes
/// are carried whole (they hold no width), and a wide glyph that would
/// straddle the edge moves to the next row whole — where the terminal would
/// move it, too. Never returns an empty head for non-empty input.
pub fn split_at_width(s: &str, width: usize) -> (&str, &str) {
    let bytes = s.as_bytes();
    let mut used = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = escape_end(bytes, i);
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or('\0');
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > width {
            if i == 0 {
                // Degenerate budget (a wide glyph against a 1-column edge):
                // take it anyway — a byte must leave the head for progress.
                i = ch.len_utf8();
            }
            break;
        }
        used += cw;
        i += ch.len_utf8();
    }
    (&s[..i], &s[i..])
}

/// DECTCEM show — emitted when the applied frame has a caret, because that is
/// where the user types. `TermGuard::stop` emits it unconditionally as the net
/// for exiting with it hidden.
pub const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

/// DECTCEM hide — the other half of [`SHOW_CURSOR`], for a frame with **no
/// caret**.
///
/// Mid-turn an empty input is no row at all (`input_hidden`), so there is
/// nowhere to put a caret, and showing it leaves a cursor blinking against a
/// row that was never drawn. `Frame::caret` is `None` in exactly those cases,
/// so the decision reads the frame's own answer instead of tracking input
/// state here.
///
/// Not the hide the transient bar used to do, long gone with it: that one hid
/// the cursor *around a content write*, to stop it flickering over text
/// arriving mid-render. This one describes the frame that was composed,
/// decided once per apply.
pub const HIDE_CURSOR: &[u8] = b"\x1b[?25l";

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The non-tty edge strip: CSI (SGR, the line clear) and OSC-8 links go,
    /// every visible byte stays, and the result is valid UTF-8.
    #[test]
    fn strip_ansi_bytes_keeps_the_text_and_drops_every_escape() {
        let input =
            b"\x1b[K\x1b[32mgreen\x1b[0m \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ caf\xc3\xa9\n";
        let out = strip_ansi_bytes(input);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "green link caf\u{e9}\n",
            "visible bytes and multi-byte chars survive; escapes do not"
        );
    }

    /// The width guard's own edges: escape bytes never count as cells, and a
    /// multi-byte char split across writes is held whole until the wrap
    /// decision it belongs to has been made.
    #[test]
    fn wrap_lines_counts_escapes_and_carries_split_chars() {
        let mut sink: Vec<u8> = Vec::new();
        let mut w = WrapLines {
            inner: &mut sink,
            width: 10,
            enabled: true,
            col: 0,
            partial: Vec::new(),
            esc: EscState::None,
        };
        w.write_all(b"\x1b[38;5;12maaaaaaaaaaaaaaa").unwrap();
        w.write_all(b"\n").unwrap();
        assert_eq!(
            String::from_utf8(sink.clone()).unwrap(),
            "\x1b[38;5;12maaaaaaaaaa\naaaaa\n",
            "escape params are zero cells; the wrap lands at 10"
        );

        let mut sink: Vec<u8> = Vec::new();
        let mut w = WrapLines {
            inner: &mut sink,
            width: 6,
            enabled: true,
            col: 0,
            partial: Vec::new(),
            esc: EscState::None,
        };
        let s = "日日日日".as_bytes();
        w.write_all(&s[..5]).unwrap(); // one char whole, the next split
        w.write_all(&s[5..]).unwrap();
        w.write_all(b"\n").unwrap();
        assert_eq!(
            String::from_utf8(sink).unwrap(),
            "日日日\n日\n",
            "the held char completes, then wraps before the fourth"
        );
    }
}
