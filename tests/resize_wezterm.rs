//! Integration tests driven through **wezterm's own terminal emulator core**
//! (`wezterm-term`), i.e. the terminal where the resize corruption was
//! reproduced. The difference from `repl_resize.rs` (vt100): wezterm *reflows*
//! scrollback on width changes — lines that were wrapped at the old width are
//! joined and re-wrapped — while vt100/xterm do not. That reflow is what moved
//! picocode's input/overlay frames around and left stale copies on screen.
//!
//! Strategy (same as `repl_resize.rs`): drive the real `picocode` binary in a
//! pty, feed its output into a real emulator, resize pty + emulator together.
//! The emulator answers the child's cursor-position queries (`ESC[6n`) itself
//! through its response writer, wired back into the pty — exactly what a real
//! wezterm does.

use std::io::{self, Read, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use wezterm_term::color::ColorPalette;
use wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Minimal terminal configuration for the emulator (only `color_palette` is
/// required by the trait).
#[derive(Debug)]
struct Cfg;
impl TerminalConfiguration for Cfg {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

/// A `Write` shared between the emulator's response writer (DSR replies etc.)
/// and the test's keystroke injection — both go into the pty master.
#[derive(Clone)]
struct SharedPty(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for SharedPty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// A spawned `picocode` in a pty plus wezterm's emulator as the screen.
struct Term {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: SharedPty,
    rx: mpsc::Receiver<Vec<u8>>,
    term: Terminal,
    all: Vec<u8>,
}

impl Term {
    fn spawn(rows: u16, cols: u16) -> Self {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .expect("open pty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_picocode"));
        cmd.args(["-p", "openrouter", "-m", "tencent/hy3:free"]);
        let cfg = tempfile::tempdir().expect("tempdir");
        cmd.env("XDG_CONFIG_HOME", cfg.path());
        // Keep the config tempdir alive for the whole run by leaking it: the
        // test process is short-lived and this avoids threading an owner around.
        std::mem::forget(cfg);
        let child = pair.slave.spawn_command(cmd).expect("spawn picocode");
        let mut reader = pair.master.try_clone_reader().expect("clone reader");
        let writer: SharedPty =
            SharedPty(Arc::new(Mutex::new(pair.master.take_writer().expect("take writer"))));
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let term = Terminal::new(
            TerminalSize {
                rows: rows as usize,
                cols: cols as usize,
                pixel_width: cols as usize * 8,
                pixel_height: rows as usize * 16,
                dpi: 0,
            },
            Arc::new(Cfg),
            "WezTerm",
            "test",
            Box::new(writer.clone()),
        );
        Self {
            master: pair.master,
            child,
            writer,
            rx,
            term,
            all: Vec::new(),
        }
    }

    /// Drain child output for `dur`, feeding the emulator. DSR replies are
    /// produced by the emulator itself (into the shared writer -> pty master).
    fn pump(&mut self, dur: Duration) {
        let until = Instant::now() + dur;
        while Instant::now() < until {
            if let Ok(chunk) = self.rx.recv_timeout(Duration::from_millis(10)) {
                self.all.extend_from_slice(&chunk);
                self.term.advance_bytes(&chunk);
            }
        }
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        let mut w = self.writer.clone();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .expect("resize");
        self.term.resize(TerminalSize {
            rows: rows as usize,
            cols: cols as usize,
            pixel_width: cols as usize * 8,
            pixel_height: rows as usize * 16,
            dpi: 0,
        });
    }

    fn rows(&self) -> Vec<String> {
        // `Screen::visible_lines` is `#[cfg(test)]` inside wezterm-term, so
        // build the visible window from the public physical-line iteration:
        // the viewport is the last `physical_rows` physical lines.
        let screen = self.term.screen();
        let mut all: Vec<String> = Vec::new();
        screen.for_each_phys_line(|_, line| {
            all.push(line.as_str().trim_end().to_string());
        });
        let skip = all.len().saturating_sub(screen.physical_rows);
        all.into_iter().skip(skip).collect()
    }

    fn screen_dump(&self) -> String {
        self.rows()
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:2}|{l}", i + 1))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Start the REPL, open the slash menu, and return the topmost session line
    /// ("history") currently on screen.
    fn start_with_menu(&mut self) -> String {
        self.pump(Duration::from_millis(1500));
        self.type_bytes(b"/");
        self.pump(Duration::from_millis(500));
        let before = self.rows();
        let top = before
            .iter()
            .find(|l| !l.trim().is_empty())
            .cloned()
            .unwrap_or_default();
        assert!(!top.is_empty(), "no session content on screen:\n{}", self.screen_dump());
        top
    }

    fn assert_history_present(&self, top: &str) {
        let after = self.rows();
        assert!(
            after.iter().any(|l| l == top),
            "resize drag lost session history / left a stale frame: top line \
             {top:?} is gone.\nafter:\n{}",
            self.screen_dump(),
        );
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Narrow one column at a time, then widen back — the horizontal drag.
fn hdrag(t: &mut Term) {
    for cols in (50u16..COLS).rev().chain(51u16..=COLS) {
        t.resize(ROWS, cols);
        t.pump(Duration::from_millis(30));
    }
    t.pump(Duration::from_millis(800));
}

/// Keystroke-only repaint contract: the child writes nothing during the drag;
/// the keystroke after it is the repaint trigger, and that repaint must clear
/// the (reflowed-by-wezterm) frame rather than stack a duplicate.
fn assert_silent_during_drag(t: &Term, before: usize) {
    let delta = t.all.len() - before;
    assert!(
        delta < 64,
        "child wrote {delta} bytes during the resize drag (should repaint only on keystroke)"
    );
}

#[test]
fn wezterm_horizontal_drag_menu_frame_not_duplicated() {
    let mut t = Term::spawn(ROWS, COLS);
    let top = t.start_with_menu();

    let before = t.all.len();
    hdrag(&mut t);
    assert_silent_during_drag(&t, before);

    t.assert_history_present(&top);
    // A stranded overlay frame would duplicate the menu: the item that sits
    // just below the input ("thinking") must appear exactly once.
    let check = |t: &Term, when: &str| {
        let thinking = t.rows().iter().filter(|l| l.contains("thinking")).count();
        assert_eq!(
            thinking, 1,
            "{when}: stale overlay frames left on screen (duplicated menu):\n{}",
            t.screen_dump(),
        );
    };
    check(&t, "after drag");

    // Keystroke repaint: re-anchor + clear must not strand the reflowed frame.
    t.type_bytes(b"\x1b[D"); // Left
    t.pump(Duration::from_millis(600));
    t.assert_history_present(&top);
    check(&t, "after keystroke repaint");
}

#[test]
fn wezterm_vertical_drag_menu_frame_not_duplicated() {
    let mut t = Term::spawn(ROWS, COLS);
    let top = t.start_with_menu();

    let before = t.all.len();
    for rows in (16u16..ROWS).rev().chain(17u16..=ROWS) {
        t.resize(rows, COLS);
        t.pump(Duration::from_millis(30));
    }
    t.pump(Duration::from_millis(800));
    assert_silent_during_drag(&t, before);

    t.assert_history_present(&top);
    let thinking = t.rows().iter().filter(|l| l.contains("thinking")).count();
    assert_eq!(
        thinking, 1,
        "stale overlay frames left on screen (duplicated menu):\n{}",
        t.screen_dump(),
    );

    t.type_bytes(b"\x1b[D"); // Left: keystroke triggers the repaint
    t.pump(Duration::from_millis(600));
    t.assert_history_present(&top);
    let thinking = t.rows().iter().filter(|l| l.contains("thinking")).count();
    assert_eq!(
        thinking, 1,
        "keystroke repaint after resize duplicated the menu:\n{}",
        t.screen_dump(),
    );
}

#[test]
fn wezterm_horizontal_drag_input_not_duplicated() {
    let mut t = Term::spawn(ROWS, COLS);
    t.pump(Duration::from_millis(1500));
    // The user-reported input: a slash command with a field assignment.
    t.type_bytes(b"/session .path:.");
    t.pump(Duration::from_millis(500));

    let before = t.all.len();
    hdrag(&mut t);
    assert_silent_during_drag(&t, before);

    // The outstanding input text must appear exactly once (the input row).
    let dupes = |t: &Term| t.rows().iter().filter(|l| l.contains(".path:.")).count();
    assert_eq!(
        dupes(&t),
        1,
        "input text duplicated by the horizontal resize drag:\n{}",
        t.screen_dump(),
    );

    // Keystroke repaint after the width changed: re-anchor + clear.
    t.type_bytes(b"\x1b[D"); // Left
    t.pump(Duration::from_millis(600));
    assert_eq!(
        dupes(&t),
        1,
        "keystroke repaint after resize duplicated the input:\n{}",
        t.screen_dump(),
    );
}
