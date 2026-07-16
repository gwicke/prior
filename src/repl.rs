//! Raw-mode REPL input layer (Stages 2–4).
//!
//! - [`TermGuard`] owns terminal raw-mode lifecycle + capability negotiation
//!   (plan §3.5.2) and is safe to call from `Drop`/panic hook/signal handler.
//! - [`Repl`] is the raw-mode driver. [`Repl::read_input`] is a multi-line
//!   line editor; [`Repl::select`] is a single-column list picker.
//!
//! The line editor keeps a single flat `Vec<char>` buffer (with `'\n'` as a
//! hard line break) plus a char-index cursor. Rendering wraps the buffer to
//! visual rows via [`layout`] (word-boundary wrap, CJK char-wrap) and positions
//! the cursor. All editor *logic* is pure and unit-tested; only the final
//! escape-sequence emission touches the terminal.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crate::output::wrap;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, Write};
use std::path::PathBuf;

/// Owns terminal mode transitions. [`TermGuard::start`] enters raw mode and
/// negotiates capabilities; [`TermGuard::stop`] (idempotent) restores cooked
/// mode. Safe to call from `Drop`, a panic hook, or a signal handler.
pub struct TermGuard {
    active: bool,
    saved_paste: bool,
}

impl TermGuard {
    pub fn new() -> Self {
        Self {
            active: false,
            saved_paste: false,
        }
    }

    /// Enter raw mode and negotiate capabilities. Idempotent.
    pub fn start(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        self.saved_paste = false;
        enable_raw_mode()?;
        // crossterm's `enable_raw_mode` calls `cfmakeraw`, which clears `OPOST`.
        // With `OPOST` off the terminal does NOT translate `\n` -> `\r\n`, so
        // every bare `\n` (help text, separators, agent responses) only moves
        // down a row and leaves the cursor at the same column — producing the
        // staggered "indentation" seen on a full screen. Re-enable output
        // post-processing (specifically `ONLCR`) so line breaks return to
        // column 0, while keeping raw *input* semantics (no canonical line
        // editing, no echo). `disable_raw_mode` restores the original mode.
        enable_output_processing();
        Self::csi("?2004h")?; // bracketed paste on
        // Kitty keyboard protocol: disambiguate keys + report all keys as CSI u,
        // so Shift+Enter arrives as Enter+SHIFT (newline) and Alt+Letter as a
        // distinct Alt-modified char. Honored by wezterm/alacritty/kitty.
        crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            )
        )?;
        self.active = true;
        Ok(())
    }

    /// Restore cooked mode. Idempotent; safe to call multiple times / from `Drop`.
    pub fn stop(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        // Pop one level of Kitty keyboard enhancement (matches the single push).
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        if self.saved_paste {
            Self::csi("?2004h")?;
        } else {
            Self::csi("?2004l")?;
        }
        disable_raw_mode()?;
        self.active = false;
        Ok(())
    }

    fn csi(seq: &str) -> io::Result<()> {
        let mut s = String::from("\x1b[");
        s.push_str(seq);
        io::stdout().write_all(s.as_bytes())?;
        io::stdout().flush()
    }
}

/// Re-enable terminal output post-processing (specifically `OPOST`/`ONLCR`) on
/// the controlling terminal. `crossterm::enable_raw_mode` runs `cfmakeraw`,
/// which disables `OPOST`; without it a bare `\n` does not return the cursor
/// to column 0. Best-effort: errors are ignored (output simply stays raw).
fn enable_output_processing() {
    use std::os::unix::io::IntoRawFd;
    let fd = match std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(f) => f.into_raw_fd(),
        Err(_) => return,
    };
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) != 0 {
            libc::close(fd);
            return;
        }
        termios.c_oflag |= libc::OPOST | libc::ONLCR;
        let _ = libc::tcsetattr(fd, libc::TCSANOW, &termios);
        libc::close(fd);
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The interactive REPL driver.
pub struct Repl {
    guard: TermGuard,
}

impl Repl {
    pub fn new() -> Self {
        Self {
            guard: TermGuard::new(),
        }
    }

    /// Enter raw mode (idempotent). Best-effort; `Drop` still restores.
    pub fn start(&mut self) -> io::Result<()> {
        self.guard.start()
    }

    /// Read a multi-line input in raw mode.
    ///
    /// Returns `Some(text)` with the submitted text (newlines preserved), or
    /// `Some("")` on `Esc` (cancel the current line), or `None` only on a read
    /// error. `Ctrl-C` restores the terminal and exits the process (plan §3.4.1).
    ///
    /// `history` backs `Up`/`Down` navigation and is persisted to disk.
    pub fn read_input(&mut self, prompt: &str, history: &mut History) -> Option<String> {
        let mut editor = LineEditor::new();
        let mut nav: Option<usize> = None;
        let mut draft = String::new();
        // Screen rows occupied by the previously drawn input, so each redraw can
        // Track the previously drawn region so each redraw can clear exactly
        // those rows. We clear relative to the *current* cursor, which (by the
        // editor's invariant) sits at `top + prev_cursor_row`, so moving up
        // `prev_cursor_row` rows returns to the region's top. This avoids
        // DECSC/DECRC save/restore, which some terminals honor inconsistently.
        let mut prev_rows: usize = 0;
        let mut prev_cursor_row: usize = 0;
        let redraw = |editor: &LineEditor, prev_rows: &mut usize, prev_cursor_row: &mut usize| {
            let (w, _) = crossterm::terminal::size().unwrap_or((80, 24));
            let w = w as usize;
            let layout = editor.layout(w, prompt);
            let rc = layout.row_count.max(1);
            let cur = layout.cursor_row;
            let mut s = String::new();
            // Return to the top of the previously drawn region.
            if *prev_cursor_row > 0 {
                s.push_str(&format!("\x1b[{}A", *prev_cursor_row));
            }
            // Clear each previously drawn row (clear-to-EOL is universally
            // supported; we avoid erase-down after a flaky save/restore).
            s.push_str("\r\x1b[K");
            for _ in 1..(*prev_rows).max(1) {
                s.push('\n');
                s.push_str("\x1b[K");
            }
            // Back to the top to write the new content.
            if *prev_rows > 1 {
                s.push_str(&format!("\x1b[{}A", *prev_rows - 1));
            }
            s.push('\r');
            s.push_str(&render_content(&layout, prompt));
            s.push_str(&cursor_positioning(&layout));
            let _ = io::stdout().write_all(s.as_bytes());
            let _ = io::stdout().flush();
            *prev_rows = rc;
            *prev_cursor_row = cur;
        };
        redraw(&editor, &mut prev_rows, &mut prev_cursor_row);

        loop {
            match event::read() {
                Ok(Event::Key(key)) => match key.code {
                    KeyCode::Enter => {
                        if key.modifiers.contains(KeyModifiers::SHIFT) {
                            editor.insert_newline();
                            redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                        } else {
                            let _ = io::stdout().write_all(b"\n");
                            let _ = io::stdout().flush();
                            return Some(editor.text());
                        }
                    }
                    KeyCode::Esc => {
                        let _ = io::stdout().write_all(b"\n");
                        let _ = io::stdout().flush();
                        return Some(String::new());
                    }
                    KeyCode::Char(c) => {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            match c {
                                'c' => {
                                    let _ = self.guard.stop();
                                    std::process::exit(1);
                                }
                                'u' => {
                                    editor.delete_line_start();
                                    redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                                }
                                'k' => {
                                    editor.delete_line_end();
                                    redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                                }
                                'j' => {
                                    // Ctrl-J (LF) is a reliable, terminal-agnostic
                                    // newline even where Shift+Enter isn't
                                    // disambiguated (Kitty protocol off).
                                    editor.insert_newline();
                                    redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                                }
                                _ => {}
                            }
                        } else if key.modifiers.contains(KeyModifiers::ALT) {
                            match c {
                                'b' => {
                                    editor.word_left();
                                    redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                                }
                                'f' => {
                                    editor.word_right();
                                    redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                                }
                                _ => {}
                            }
                        } else {
                            editor.insert(c);
                            redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                        }
                    }
                    KeyCode::Backspace => {
                        editor.backspace();
                        redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                    }
                    KeyCode::Delete => {
                        editor.delete();
                        redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                    }
                    KeyCode::Left => {
                        editor.left();
                        redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                    }
                    KeyCode::Right => {
                        editor.right();
                        redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                    }
                    KeyCode::Home => {
                        editor.home();
                        redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                    }
                    KeyCode::End => {
                        editor.end();
                        redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                    }
                    KeyCode::Up => {
                        if !editor.move_up_line() {
                            if let Some(s) = history.prev(&editor.text(), &mut nav, &mut draft) {
                                editor.set(&s);
                            }
                        }
                        redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                    }
                    KeyCode::Down => {
                        if !editor.move_down_line() {
                            if let Some(s) = history.next(&mut nav, &mut draft) {
                                editor.set(&s);
                            }
                        }
                        redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                    }
                    _ => {}
                },
                Ok(Event::Paste(text)) => {
                    editor.insert_str(&text);
                    redraw(&editor, &mut prev_rows, &mut prev_cursor_row);
                }
                Ok(_) => {} // resize/mouse: ignored (Stage 6 redraw)
                Err(_) => return None,
            }
        }
    }

    /// Render a single-column selection list and let the user pick one entry.
    /// `options` are display strings. Returns `Some(index)` on Enter, `None` on
    /// Esc (cancel). Used by `/model` (and any future picker).
    /// Pure renderer for the selection list (testable without a TTY). Produces the
    /// visible text only — the caller prepends the clear-from-top escape sequence.
    /// `title` appears exactly once; each option is wrapped to `width` with
    /// continuation lines aligned under the option text; long lists scroll so the
    /// selection stays in view. `sel` is the currently highlighted index.
    pub fn render_select(
        title: &str,
        options: &[String],
        sel: usize,
        width: usize,
        height: usize,
    ) -> String {
        let tw = width.max(1);
        let th = height.max(1);
        let prefix_w = 2usize; // "> " for the selection, "  " otherwise
        let opt_w = tw.saturating_sub(prefix_w);
        // Precompute wrapped display rows for each option. `first` marks the
        // option's first (marker) row so continuation lines stay aligned.
        let mut rows: Vec<(usize, String, bool)> = Vec::new();
        for (i, o) in options.iter().enumerate() {
            let wrapped = wrap(o, opt_w);
            for (j, line) in wrapped.into_iter().enumerate() {
                rows.push((i, line, j == 0));
            }
        }
        let max_rows = th.saturating_sub(1).max(1); // title consumes one row
        let sel_row = rows.iter().position(|(i, _, _)| *i == sel).unwrap_or(0);
        let start = if rows.len() <= max_rows {
            0
        } else {
            sel_row.saturating_sub(max_rows / 2).min(rows.len() - max_rows)
        };
        let mut s = String::new();
        s.push_str(title);
        s.push('\n');
        for (i, line, first) in rows.iter().skip(start).take(max_rows) {
            let marker = if *first && *i == sel { "> " } else { "  " };
            s.push_str(marker);
            s.push_str(line);
            s.push('\n');
        }
        s
    }

    /// Clear `n` screen rows starting at the current row (moving up to the top
    /// row, then clearing each with clear-to-EOL). Does not rely on DECSC/DECRC
    /// save/restore, which some terminals honor inconsistently.
    fn clear_n_rows(n: usize) {
        let mut s = String::new();
        if n > 1 {
            s.push_str(&format!("\x1b[{}A", n - 1));
        }
        s.push_str("\r\x1b[K");
        // Bare `\n`: `OPOST`/`ONLCR` (re-enabled in `TermGuard::start`) converts it to
        // `\r\n`; writing `\r\n` here would yield `\r\r\n`.
        for _ in 1..n.max(1) {
            s.push('\n');
            s.push_str("\x1b[K");
        }
        let _ = io::stdout().write_all(s.as_bytes());
        let _ = io::stdout().flush();
    }

    /// Render a single-column selection list and let the user pick one entry.
    /// `options` are display strings. Returns `Some(index)` on Enter, `None` on
    /// Esc (cancel). Used by `/model` (and any future picker).
    ///
    /// Rendering model (mirrors pi's bounded, scrolling dropdown): on every
    /// redraw, clear exactly the rows the previous render occupied (counted
    /// clear-to-EOL per row) and reprint. This prevents the list from being
    /// *appended* on each navigation (the old bug) without depending on
    /// DECSC/DECRC save/restore + erase-down, which wezterm honors
    /// inconsistently.
    pub fn select(&mut self, title: &str, options: &[String]) -> Option<usize> {
        if options.is_empty() {
            return None;
        }
        let (tw, th) = crossterm::terminal::size().unwrap_or((80, 24));
        let tw = tw as usize;
        let th = th as usize;

        let mut sel: usize = 0;
        let mut prev_rows: usize = 0;
        let redraw = |sel: usize, prev_rows: &mut usize| -> std::io::Result<()> {
            let rendered = Self::render_select(title, options, sel, tw, th);
            let rc = rendered.matches('\n').count(); // ends with '\n' → row count
            Self::clear_n_rows(*prev_rows);
            // After clearing, cursor is on the bottom cleared row; return to top.
            let mut s = String::new();
            if *prev_rows > 1 {
                s.push_str(&format!("\x1b[{}A", *prev_rows - 1));
            }
            s.push('\r');
            s.push_str(&rendered);
            io::stdout().write_all(s.as_bytes())?;
            io::stdout().flush()?;
            *prev_rows = rc;
            Ok(())
        };
        let _ = redraw(sel, &mut prev_rows);
        loop {
            match event::read() {
                Ok(Event::Key(key)) => match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        if sel + 1 < options.len() {
                            sel += 1;
                            let _ = redraw(sel, &mut prev_rows);
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if sel > 0 {
                            sel -= 1;
                            let _ = redraw(sel, &mut prev_rows);
                        }
                    }
                    KeyCode::Enter => {
                        // Clear the menu region before returning.
                        Self::clear_n_rows(prev_rows);
                        return Some(sel);
                    }
                    KeyCode::Esc => {
                        Self::clear_n_rows(prev_rows);
                        return None;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    /// Print a block of REPL-originated text (help, status messages) below the
    /// current input region, clearing that region first so output doesn't
    /// overlap the prompt. Used in TTY mode; the next `read_input` re-saves a
    /// fresh top. Falls back to the cooked `Output` when there is no REPL.
    ///
    /// Text is printed **verbatim** (its own newlines and column alignment are
    /// preserved). Only lines longer than the terminal width are wrapped, so
    /// pre-formatted output like `/help` keeps its alignment instead of being
    /// reflowed into ragged columns.
    pub fn print_block(&mut self, text: &str) {
        let (w, _) = crossterm::terminal::size().unwrap_or((80, 24));
        let w = (w as usize).max(1);
        let mut body = String::new();
        for line in text.split('\n') {
            if line.chars().count() <= w {
                body.push_str(line);
            } else {
                for wl in crate::output::wrap(line, w) {
                    body.push_str(&wl);
                    body.push('\n');
                }
                continue;
            }
            body.push('\n');
        }
        let s = format!("\x1b[u\r\x1b[0J{}", body);
        let _ = io::stdout().write_all(s.as_bytes());
        let _ = io::stdout().flush();
    }
}

/// A flat character buffer + cursor for multi-line editing. All methods are pure
/// (no terminal access) and unit-tested in [`tests`].
#[derive(Default)]
pub struct LineEditor {
    buf: Vec<char>,
    cursor: usize, // char index into `buf`
}

impl LineEditor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole buffer (used by history navigation). Cursor → end.
    pub fn set(&mut self, s: &str) {
        self.buf = s.chars().collect();
        self.cursor = self.buf.len();
    }

    /// Current buffer as a `String` (newlines preserved).
    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }

    pub fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.buf.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buf.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor += 1;
        }
    }

    /// Move to the start of the current logical line.
    pub fn home(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.buf[i - 1] != '\n' {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Move to the end of the current logical line.
    pub fn end(&mut self) {
        let n = self.buf.len();
        let mut i = self.cursor;
        while i < n && self.buf[i] != '\n' {
            i += 1;
        }
        self.cursor = i;
    }

    /// Move back one word (stopping at whitespace/line boundaries).
    pub fn word_left(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Move forward one word.
    pub fn word_right(&mut self) {
        let n = self.buf.len();
        let mut i = self.cursor;
        while i < n && self.buf[i].is_whitespace() {
            i += 1;
        }
        while i < n && !self.buf[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
    }

    pub fn insert_newline(&mut self) {
        self.buf.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Delete from the cursor to the start of the logical line (`Ctrl-U`).
    /// When the cursor is already at the start of a line (right after a `'\n'`),
    /// this clears the preceding newline and the whole previous line, so
    /// successive `Ctrl-U` presses walk backward one logical line at a time.
    pub fn delete_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Start of the current logical line (after the preceding '\n', or buffer start).
        let line_start = {
            let mut i = self.cursor;
            while i > 0 && self.buf[i - 1] != '\n' {
                i -= 1;
            }
            i
        };
        if line_start < self.cursor {
            // There is content before the cursor on this line: delete it.
            self.buf.drain(line_start..self.cursor);
            self.cursor = line_start;
            return;
        }
        // Cursor is at the start of a line that follows a '\n': clear the
        // preceding newline and the whole previous line so the next press can
        // continue backward.
        let prev_start = {
            let mut i = self.cursor - 1; // step past the '\n'
            while i > 0 && self.buf[i - 1] != '\n' {
                i -= 1;
            }
            i
        };
        self.buf.drain(prev_start..self.cursor);
        self.cursor = prev_start;
    }

    /// Delete from the cursor to the end of the logical line (`Ctrl-K`).
    pub fn delete_line_end(&mut self) {
        let n = self.buf.len();
        let mut end = self.cursor;
        while end < n && self.buf[end] != '\n' {
            end += 1;
        }
        if end > self.cursor {
            self.buf.drain(self.cursor..end);
        }
    }

    /// Move up one logical line (to the end of the previous line). Returns
    /// `false` when already on the first line (caller may navigate history).
    pub fn move_up_line(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let mut i = self.cursor;
        while i > 0 && self.buf[i - 1] != '\n' {
            i -= 1;
        }
        if i == 0 {
            return false;
        }
        self.cursor = i - 1; // just before the preceding '\n'
        true
    }

    /// Move down one logical line (to the end of the next line). Returns
    /// `false` when already on the last line (caller may navigate history).
    pub fn move_down_line(&mut self) -> bool {
        let n = self.buf.len();
        if self.cursor >= n {
            return false;
        }
        let mut i = self.cursor;
        while i < n && self.buf[i] != '\n' {
            i += 1;
        }
        if i == n {
            return false;
        }
        // jump to the end of the next logical line
        let mut j = i + 1;
        while j < n && self.buf[j] != '\n' {
            j += 1;
        }
        self.cursor = j;
        true
    }

    /// Compute the visual layout of the buffer at `width` columns, with `prompt`
    /// occupying the first visual row's prefix. Pure + unit-tested.
    pub fn layout(&self, width: usize, prompt: &str) -> Layout {
        let width = width.max(1);
        let pcols = prompt.chars().count();
        let mut rows: Vec<String> = Vec::new();
        let mut cursor_row = 0usize;
        let mut cursor_col = 0usize;
        let mut cursor_set = false;
        let n = self.buf.len();
        let mut line_start = 0usize;
        let mut i = 0usize;
        let mut row_offset = 0usize;
        while i <= n {
            if i == n || self.buf[i] == '\n' {
                let line: Vec<char> = self.buf[line_start..i].to_vec();
                let first_avail = if line_start == 0 {
                    width.saturating_sub(pcols)
                } else {
                    width
                };
                let coff = if !cursor_set && self.cursor >= line_start && self.cursor <= i {
                    Some(self.cursor - line_start)
                } else {
                    None
                };
                let (lrows, cpos) = wrap_line(&line, first_avail, width, coff);
                for (ri, lr) in lrows.iter().enumerate() {
                    rows.push(lr.clone());
                    if let Some((cr, cc)) = cpos {
                        if cr == ri {
                            cursor_row = row_offset + cr;
                            cursor_col = if row_offset + cr == 0 { pcols + cc } else { cc };
                            cursor_set = true;
                        }
                    }
                }
                row_offset += lrows.len();
                if i == n {
                    break;
                }
                i += 1; // consume '\n'
                line_start = i;
            } else {
                i += 1;
            }
        }
        let row_count = rows.len();
        Layout {
            rows,
            cursor_row,
            cursor_col,
            row_count,
        }
    }
}

/// Visual layout of a [`LineEditor`] buffer.
pub struct Layout {
    /// Content-only visual rows (the prompt is rendered separately on row 0).
    pub rows: Vec<String>,
    /// Visual row (0-based) the cursor is on.
    pub cursor_row: usize,
    /// Absolute column (0-based) the cursor is on, counting the prompt on row 0.
    pub cursor_col: usize,
    /// Total visual rows.
    pub row_count: usize,
}

/// Word-boundary wrap of one logical line (no `'\n'`). Returns the visual rows
/// and, if `cursor` is `Some`, the (row, col) where that char index lands.
/// Long words and CJK (no spaces) are char-wrapped; spaces are break
/// opportunities and dropped at row starts.
fn wrap_line(
    line: &[char],
    first_width: usize,
    width: usize,
    cursor: Option<usize>,
) -> (Vec<String>, Option<(usize, usize)>) {
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut col = 0usize;
    let mut row = 0usize;
    let mut cursor_pos: Option<(usize, usize)> = None;
    let avail = |r: usize| if r == 0 { first_width } else { width };
    let n = line.len();
    let mut i = 0usize;
    while i < n {
        let c = line[i];
        if c == ' ' {
            if cursor == Some(i) {
                cursor_pos = Some((row, col));
            }
            if col == 0 {
                i += 1;
                continue;
            }
            if col + 1 > avail(row) {
                // The trailing space is dropped and the next row starts here.
                if cursor == Some(i) {
                    cursor_pos = Some((row, col));
                }
                rows.push(std::mem::take(&mut cur));
                row += 1;
                col = 0;
                i += 1;
                continue;
            }
            cur.push(' ');
            col += 1;
            i += 1;
            continue;
        }
        // Measure the next word (run of non-space).
        let mut j = i;
        while j < n && line[j] != ' ' {
            j += 1;
        }
        let wlen = j - i;
        if col == 0 {
            if wlen <= avail(row) {
                // Cursor may sit anywhere inside the word.
                if let Some(cu) = cursor {
                    if i <= cu && cu < j {
                        cursor_pos = Some((row, col + (cu - i)));
                    }
                }
                for k in i..j {
                    cur.push(line[k]);
                }
                col += wlen;
                i = j;
            } else {
                let mut k = i;
                while k < j {
                    if col >= avail(row) {
                        rows.push(std::mem::take(&mut cur));
                        row += 1;
                        col = 0;
                    }
                    if cursor == Some(k) {
                        cursor_pos = Some((row, col));
                    }
                    cur.push(line[k]);
                    col += 1;
                    k += 1;
                }
                i = j;
            }
        } else if col + wlen <= avail(row) {
            // Cursor may sit anywhere inside the word.
            if let Some(cu) = cursor {
                if i <= cu && cu < j {
                    cursor_pos = Some((row, col + (cu - i)));
                }
            }
            for k in i..j {
                cur.push(line[k]);
            }
            col += wlen;
            i = j;
        } else {
            rows.push(std::mem::take(&mut cur));
            row += 1;
            col = 0;
            continue; // re-process the word at the start of the new row
        }
    }
    if cursor == Some(n) {
        cursor_pos = Some((row, col));
    }
    rows.push(cur);
    (rows, cursor_pos)
}

/// Build the escape sequence that restores the saved input-top, clears down,
/// redraws `prompt` + rows, and positions the cursor. Pure string builder.
/// Content of the input region (prompt + visual rows), CRLF-separated, with
/// no cursor positioning or other terminal escapes. Composed by the editor
/// redraw and by [`render_input`].
fn render_content(layout: &Layout, prompt: &str) -> String {
    let mut s = String::new();
    let rc = layout.row_count.max(1);
    for r in 0..rc {
        if r > 0 {
            // Bare `\n`: `OPOST`/`ONLCR` (re-enabled in `TermGuard::start`) turns it
            // into `\r\n`, so writing `\r\n` here would become `\r\r\n`.
            s.push('\n');
        }
        if r == 0 {
            s.push_str(prompt);
        }
        if r < layout.rows.len() {
            s.push_str(&layout.rows[r]);
        }
    }
    s
}

/// Cursor-positioning suffix that places the caret at `layout.cursor_row` /
/// `layout.cursor_col` after [`render_content`] has been written.
fn cursor_positioning(layout: &Layout) -> String {
    let rc = layout.row_count.max(1);
    let last_row_len = if rc == 1 {
        layout
            .rows
            .first()
            .map(|x| x.chars().count())
            .unwrap_or(0)
    } else {
        layout.rows.last().map(|x| x.chars().count()).unwrap_or(0)
    };
    let mut s = String::new();
    let move_up = (rc - 1).saturating_sub(layout.cursor_row);
    if move_up > 0 {
        s.push_str(&format!("\x1b[{}A", move_up));
        s.push('\r');
        s.push_str(&format!("\x1b[{}C", layout.cursor_col));
    } else {
        let d = last_row_len.saturating_sub(layout.cursor_col);
        if d > 0 {
            s.push_str(&format!("\x1b[{}D", d));
        }
    }
    s
}

/// `render_input` is only used by the unit tests below; gate it so the
/// non-test build doesn't warn about an unused function.
#[cfg(test)]
fn render_input(layout: &Layout, prompt: &str) -> String {
    let mut s = String::from("\x1b[u\r\x1b[0J");
    s.push_str(&render_content(layout, prompt));
    s.push_str(&cursor_positioning(layout));
    s
}

/// Persistent input-line history (in-memory + appended to
/// `~/.config/picocode/history`). `Up`/`Down` navigate; the in-progress line is
/// restored when navigating back past the newest entry.
pub struct History {
    entries: Vec<String>,
    file: Option<PathBuf>,
}

impl History {
    pub fn load() -> Self {
        let file = history_path();
        let mut entries = Vec::new();
        if let Some(f) = &file {
            if let Ok(s) = std::fs::read_to_string(f) {
                for line in s.lines() {
                    if !line.trim().is_empty() {
                        entries.push(line.to_string());
                    }
                }
            }
        }
        Self { entries, file }
    }

    /// Append a submitted line (skips empty) to memory + disk.
    pub fn add(&mut self, line: &str) {
        let line = line.trim_end_matches('\n').to_string();
        if line.trim().is_empty() {
            return;
        }
        self.entries.push(line.clone());
        if let Some(f) = &self.file {
            if let Some(parent) = f.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            use std::io::Write;
            if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
                let _ = fh.write_all(line.as_bytes());
                let _ = fh.write_all(b"\n");
            }
        }
    }

    /// Step backward through history; first call stashes `current` as the draft.
    pub fn prev(&self, current: &str, nav: &mut Option<usize>, draft: &mut String) -> Option<String> {
        let n = self.entries.len();
        if n == 0 {
            return None;
        }
        if nav.is_none() {
            *draft = current.to_string();
            *nav = Some(n); // point just past the newest entry
        }
        let i = nav.unwrap();
        if i == 0 {
            return None;
        }
        let new_i = i - 1;
        *nav = Some(new_i);
        Some(self.entries[new_i].clone())
    }

    /// Step forward; past the newest entry restores the stashed `draft`.
    pub fn next(&self, nav: &mut Option<usize>, draft: &mut String) -> Option<String> {
        let n = self.entries.len();
        match nav {
            None => None,
            Some(i) => {
                if *i + 1 >= n {
                    let d = std::mem::take(draft);
                    *nav = None;
                    return Some(d);
                }
                let new_i = *i + 1;
                *nav = Some(new_i);
                Some(self.entries[new_i].clone())
            }
        }
    }
}

fn history_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("picocode")
                .join("history"),
        );
    }
    if let Some(app) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(app).join("picocode").join("history"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_text() {
        let mut e = LineEditor::new();
        for c in "ab".chars() {
            e.insert(c);
        }
        assert_eq!(e.text(), "ab");
        assert_eq!(e.cursor, 2);
    }

    #[test]
    fn backspace_at_boundary_noop() {
        let mut e = LineEditor::new();
        e.insert('a');
        e.insert('b');
        e.home();
        assert_eq!(e.cursor, 0);
        e.backspace(); // at start -> no change
        assert_eq!(e.cursor, 0);
        assert_eq!(e.text(), "ab");
    }

    #[test]
    fn delete_at_boundary_noop() {
        let mut e = LineEditor::new();
        e.insert('a');
        e.delete(); // at end -> no change
        assert_eq!(e.text(), "a");
        assert_eq!(e.cursor, 1);
    }

    #[test]
    fn backspace_middle() {
        let mut e = LineEditor::new();
        e.insert_str("abc");
        e.left();
        e.backspace(); // deletes 'b', the char before the cursor
        assert_eq!(e.text(), "ac");
        assert_eq!(e.cursor, 1);
    }

    #[test]
    fn cursor_move_across_lines() {
        let mut e = LineEditor::new();
        e.insert_str("a\nb");
        assert_eq!(e.cursor, 3);
        assert!(e.move_up_line());
        assert_eq!(e.cursor, 1); // end of "a"
        assert!(e.move_down_line());
        assert_eq!(e.cursor, 3);
        // already on last line -> false
        assert!(!e.move_down_line());
    }

    #[test]
    fn word_boundaries() {
        let mut e = LineEditor::new();
        e.insert_str("hello world");
        assert_eq!(e.cursor, 11);
        e.word_left();
        assert_eq!(e.cursor, 6); // start of "world"
        e.word_left();
        assert_eq!(e.cursor, 0);
        e.word_right();
        assert_eq!(e.cursor, 5); // end of "hello"
    }

    #[test]
    fn newline_and_line_delete() {
        let mut e = LineEditor::new();
        e.insert_str("foo\nbar"); // cursor 7 (end of "bar")
        e.home(); // start of "bar" line -> 4
        e.right(); // 5 (between b|a)
        e.delete_line_start(); // "foo\nar", cursor 4
        assert_eq!(e.text(), "foo\nar");
        assert_eq!(e.cursor, 4);
        e.right(); // 5 (between a|r)
        e.delete_line_end(); // "foo\na", cursor at end (5)
        assert_eq!(e.text(), "foo\na");
        assert_eq!(e.cursor, 5);
    }

    #[test]
    fn delete_line_start_walks_back_across_newlines() {
        // "line1\nline2" with the cursor at the end of line 2.
        let mut e = LineEditor::new();
        e.insert_str("line1\nline2"); // cursor 11
        e.delete_line_start(); // clears line2 -> "line1\n", cursor 6
        assert_eq!(e.text(), "line1\n");
        assert_eq!(e.cursor, 6);
        e.delete_line_start(); // at boundary: clears preceding '\n' + line1 -> "", cursor 0
        assert_eq!(e.text(), "");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn delete_line_start_keeps_partial_line() {
        // "a\nbc|d" -> first Ctrl-U clears "bc", second clears "a\n".
        let mut e = LineEditor::new();
        e.insert_str("a\nbcd"); // 0-4, cursor 5
        e.left(); // cursor 4 (a\nbc|d)
        e.delete_line_start(); // "a\nd"
        assert_eq!(e.text(), "a\nd");
        assert_eq!(e.cursor, 2);
        e.delete_line_start(); // at boundary -> clears "a\n" -> "d"
        assert_eq!(e.text(), "d");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn paste_insertion_preserves_newlines() {
        let mut e = LineEditor::new();
        e.insert_str("foo\nbar");
        assert_eq!(e.cursor, 7);
        assert_eq!(e.text(), "foo\nbar");
    }

    #[test]
    fn layout_cursor_tracks_mid_word() {
        let mut e = LineEditor::new();
        e.insert_str("abcdef");
        e.left();
        e.left();
        e.left(); // cursor at position 3 ('d')
        let layout = e.layout(100, "");
        assert_eq!(layout.rows, vec!["abcdef"]);
        // Cursor must follow the actual position, not snap to column 0.
        assert_eq!((layout.cursor_row, layout.cursor_col), (0, 3));
    }

    #[test]
    fn layout_word_wrap_breaks_long_line() {
        let e = LineEditor::from_text("aaa bbb ccc");
        // width 7: "aaa bbb" (7) wraps, "ccc" on row 2
        let layout = e.layout(7, "");
        assert_eq!(layout.rows, vec!["aaa bbb", "ccc"]);
        assert_eq!(layout.row_count, 2);
        // cursor at end -> row 1, col 3
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));
    }

    #[test]
    fn layout_prompt_offset_on_row_zero() {
        let e = LineEditor::from_text("hi");
        let layout = e.layout(80, "> ");
        assert_eq!(layout.rows, vec!["hi"]);
        // cursor at end; col includes the 2-char prompt prefix
        assert_eq!((layout.cursor_row, layout.cursor_col), (0, 4));
    }

    #[test]
    fn layout_hard_newline_splits_rows() {
        let e = LineEditor::from_text("ab\ncd");
        let layout = e.layout(80, "");
        assert_eq!(layout.rows, vec!["ab", "cd"]);
        // cursor at end of "cd" (row 1, col 2)
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 2));
    }

    #[test]
    fn render_input_roundtrips_empty() {
        let e = LineEditor::new();
        let layout = e.layout(80, "> ");
        let s = render_input(&layout, "> ");
        // restores input-top, clears down, writes prompt, no trailing newline,
        // and positions the cursor at the prompt end (col 2, no move needed).
        assert!(s.starts_with("\x1b[u\r\x1b[0J> "));
        assert!(!s.ends_with('\n'));
    }

    #[test]
    fn render_input_multiline_positions_cursor() {
        // cursor on the last row -> no vertical move needed
        let e = LineEditor::from_text("aaa\nbbb");
        let layout = e.layout(80, "");
        assert_eq!(layout.rows, vec!["aaa", "bbb"]);
        let s = render_input(&layout, "");
        assert!(s.contains("aaa\nbbb"), "{}", s);

        // cursor on row 0 -> render must move up one row to reposition
        let mut e2 = LineEditor::from_text("aaa\nbbb");
        e2.left(); // cursor 5 -> 4 (on row 1)
        e2.left();
        e2.left();
        e2.left(); // cursor 1 (row 0)
        let layout2 = e2.layout(80, "");
        assert_eq!(layout2.cursor_row, 0);
        let s2 = render_input(&layout2, "");
        assert!(s2.contains("\x1b[1A"), "expected move-up, got {}", s2);
    }

    #[test]
    fn history_navigation_with_draft_restore() {
        let mut h = History {
            entries: Vec::new(),
            file: None,
        };
        h.add("first");
        h.add("second");
        let mut nav: Option<usize> = None;
        let mut draft = String::new();
        assert_eq!(h.prev("draft", &mut nav, &mut draft), Some("second".to_string()));
        assert_eq!(h.prev("draft", &mut nav, &mut draft), Some("first".to_string()));
        assert_eq!(h.prev("draft", &mut nav, &mut draft), None);
        // stepping forward restores the stashed draft past the newest entry
        assert_eq!(h.next(&mut nav, &mut draft), Some("second".to_string()));
        assert_eq!(h.next(&mut nav, &mut draft), Some("draft".to_string()));
        assert!(nav.is_none());
    }

    #[test]
    fn render_select_title_appears_once() {
        let opts = vec![
            "a: alpha model ctx 128000 $0.10/0.20 per M".to_string(),
            "b: beta model ctx 200000 $0.30/0.40 per M".to_string(),
        ];
        let out = Repl::render_select("Select a model:", &opts, 0, 80, 24);
        assert_eq!(out.matches("Select a model:").count(), 1);
        assert!(out.contains("> a: alpha"));
        assert!(out.contains("  b: beta"));
    }

    #[test]
    fn render_select_wraps_long_option_with_aligned_continuation() {
        let long = "deepseek:deepseek-v4-flash ctx 1048576 $0.000000435/0.00000087 per M (est) and some extra trailing text to force a wrap";
        let opts = vec![long.to_string()];
        let out = Repl::render_select("Select a model:", &opts, 0, 40, 24);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].starts_with("> "), "first row is the marker row");
        let cont: Vec<&&str> = lines
            .iter()
            .filter(|l| l.starts_with("  ") && !l.starts_with("> "))
            .collect();
        assert!(
            !cont.is_empty(),
            "expected a wrapped continuation line indented under the option text"
        );
    }

    #[test]
    fn render_select_scrolls_selection_into_view() {
        let opts: Vec<String> = (0..50).map(|i| format!("model-{} ctx 1000 $1/1 per M", i)).collect();
        let out = Repl::render_select("Select:", &opts, 49, 80, 10);
        assert!(
            out.contains("model-49"),
            "last option must be visible when selected"
        );
        assert!(
            !out.contains("model-0"),
            "early options should be scrolled out of the window"
        );
        assert_eq!(out.matches("Select:").count(), 1);
    }

    impl LineEditor {
        fn from_text(s: &str) -> Self {
            let mut e = LineEditor::new();
            e.insert_str(s);
            e
        }
    }
}
