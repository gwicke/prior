//! Raw-mode REPL input layer (Stage 2 scaffold).
//!
//! Owns terminal raw-mode lifecycle via [`TermGuard`] (start/stop per plan §3.5.2)
//! and reads a single input line key-by-key. Multi-line editing, the slash menu,
//! signal handling, and resize redraw arrive in later stages.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, Write};

/// Owns terminal mode transitions. [`TermGuard::start`] enters raw mode and
/// negotiates capabilities; [`TermGuard::stop`] (idempotent) restores cooked
/// mode. Safe to call from `Drop`, a panic hook, or a signal handler.
pub struct TermGuard {
    active: bool,
    saved_paste: bool,
    pushed_kitty: bool,
}

impl TermGuard {
    pub fn new() -> Self {
        Self {
            active: false,
            saved_paste: false,
            pushed_kitty: false,
        }
    }

    /// Enter raw mode and negotiate capabilities. Idempotent.
    pub fn start(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        // 1. snapshot prior state. Stage 2 assumes bracketed paste was off;
        //    prior-paste preservation is a later enhancement (plan §3.5.2 step 1).
        self.saved_paste = false;
        // 2. enter raw mode (no smcup — history stays on primary scrollback)
        enable_raw_mode()?;
        // 3. enable bracketed paste
        Self::csi("?2004h")?;
        // 4. keyboard enhancement: modifyOtherKeys (Kitty `PushKeyboardEnhancementFlags`
        //    push is deferred to Stage 4 where Shift-Enter disambiguation matters).
        Self::csi(">1;1u")?;
        Self::csi(">4;1u")?;
        self.active = true;
        Ok(())
    }

    /// Restore cooked mode. Idempotent; safe to call multiple times / from `Drop`.
    pub fn stop(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        // 7. pop keyboard enhancement in reverse order of push (modifyOtherKeys off)
        if self.pushed_kitty {
            Self::csi("<u")?;
        } else {
            Self::csi(">1;0u")?;
        }
        // 8. disable bracketed paste (restore prior state if it was on)
        if self.saved_paste {
            Self::csi("?2004h")?;
        } else {
            Self::csi("?2004l")?;
        }
        // 9. restore cooked mode
        disable_raw_mode()?;
        self.active = false;
        self.pushed_kitty = false;
        Ok(())
    }

    fn csi(seq: &str) -> io::Result<()> {
        let mut s = String::from("\x1b[");
        s.push_str(seq);
        io::stdout().write_all(s.as_bytes())?;
        io::stdout().flush()
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The interactive REPL. Stage 2 supports single-line editing only.
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

    /// Read a single line in raw mode, returning the line (no trailing newline).
    /// `None` only on a read error (callers should re-prompt). `Esc` returns an
    /// empty string (cancel the current line). `Ctrl-C` restores the terminal
    /// and exits the process (plan §3.4.1).
    pub fn read_input(&mut self, prompt: &str) -> Option<String> {
        let _ = io::stdout().write_all(prompt.as_bytes());
        let _ = io::stdout().flush();
        let mut buf: Vec<char> = Vec::new();
        let mut cursor: usize = 0;
        loop {
            match event::read() {
                Ok(Event::Key(key)) => match key.code {
                    KeyCode::Enter => {
                        let _ = io::stdout().write_all(b"\n");
                        let _ = io::stdout().flush();
                        return Some(buf.iter().collect());
                    }
                    KeyCode::Esc => return Some(String::new()),
                    KeyCode::Char(c) => {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            if c == 'c' {
                                let _ = self.guard.stop();
                                std::process::exit(1);
                            }
                            continue; // other control combos ignored in Stage 2
                        }
                        buf.insert(cursor, c);
                        cursor += 1;
                        draw(prompt, &buf, cursor);
                    }
                    KeyCode::Backspace => {
                        if cursor > 0 {
                            cursor -= 1;
                            buf.remove(cursor);
                            draw(prompt, &buf, cursor);
                        }
                    }
                    KeyCode::Delete => {
                        if cursor < buf.len() {
                            buf.remove(cursor);
                            draw(prompt, &buf, cursor);
                        }
                    }
                    KeyCode::Left => {
                        if cursor > 0 {
                            cursor -= 1;
                            draw(prompt, &buf, cursor);
                        }
                    }
                    KeyCode::Right => {
                        if cursor < buf.len() {
                            cursor += 1;
                            draw(prompt, &buf, cursor);
                        }
                    }
                    KeyCode::Home => {
                        cursor = 0;
                        draw(prompt, &buf, cursor);
                    }
                    KeyCode::End => {
                        cursor = buf.len();
                        draw(prompt, &buf, cursor);
                    }
                    _ => {}
                },
                Ok(_) => {}    // resize/mouse: ignore in Stage 2
                Err(_) => return None,
            }
        }
    }
}

/// Write the current `prompt` + `buf` to the terminal, placing the visible
/// cursor at `cursor` (char index into `buf`). Pure string builder; [`draw`]
/// performs the actual write.
fn render_line(prompt: &str, buf: &[char], cursor: usize) -> String {
    let line: String = buf.iter().collect();
    let back = buf.len().saturating_sub(cursor);
    format!("\r\x1b[K{}{}\x1b[{}D", prompt, line, back)
}

fn draw(prompt: &str, buf: &[char], cursor: usize) {
    let s = render_line(prompt, buf, cursor);
    let _ = io::stdout().write_all(s.as_bytes());
    let _ = io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_line_cursor_at_end() {
        assert_eq!(render_line("> ", &['a', 'b', 'c'], 3), "\r\x1b[K> abc\x1b[0D");
    }

    #[test]
    fn render_line_cursor_mid_buffer() {
        assert_eq!(render_line("> ", &['a', 'b', 'c'], 1), "\r\x1b[K> abc\x1b[2D");
    }

    #[test]
    fn render_line_empty_buffer() {
        assert_eq!(render_line("> ", &[], 0), "\r\x1b[K> \x1b[0D");
    }
}
