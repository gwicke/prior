//! Integration tests: a *drag*-style resize (a stream of sizes, as from a mouse
//! drag) must neither consume session history from above the input nor leave the
//! previous overlay frame stranded on screen.
//!
//! Strategy (same as `render_scroll.rs`): drive the real `picocode` binary in a
//! pty, capture its raw output, and feed it to a real `vt100` terminal emulator.
//! We additionally act as a competent terminal: whenever the child issues a
//! cursor-position query (`ESC[6n`, a DSR) we answer it from the emulator's
//! cursor, and we resize both the pty and the emulator together.
//!
//! Regressions guarded here:
//! * vertical: the overlay was allowed to overflow the bottom, scrolling session
//!   output off the top; the input then stayed shifted up, so a shrink/grow cycle
//!   ate history a little at a time.
//! * horizontal: a width reflow can push the input block DOWN, and clearing only
//!   from the new (lower) row left the previous overlay frame stranded above it.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use vt100::Parser;

const COLS: u16 = 80;
const ROWS: u16 = 24;

fn count(hay: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || hay.len() < needle.len() {
        return 0;
    }
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// Drain child output for `dur`, feeding the emulator and answering any new DSR
/// queries from the emulator's current cursor position.
fn pump<W: Write>(
    rx: &mpsc::Receiver<Vec<u8>>,
    dur: Duration,
    parser: &mut Parser,
    all: &mut Vec<u8>,
    answered: &mut usize,
    writer: &mut W,
) {
    let until = Instant::now() + dur;
    while Instant::now() < until {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(10)) {
            all.extend_from_slice(&chunk);
            parser.process(&chunk);
            let total = count(all, b"\x1b[6n");
            while *answered < total {
                *answered += 1;
                let (r, c) = parser.screen().cursor_position();
                let _ = writer.write_all(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
                let _ = writer.flush();
            }
        }
    }
}

/// A spawned `picocode` in a pty plus a `vt100` emulator that answers the
/// child's cursor queries, so the child sees a real, reflowing terminal.
struct Term {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    parser: Parser,
    all: Vec<u8>,
    answered: usize,
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
        let writer = pair.master.take_writer().expect("take writer");
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
        Self {
            master: pair.master,
            child,
            writer,
            rx,
            parser: Parser::new(rows, cols, 100_000),
            all: Vec::new(),
            answered: 0,
        }
    }

    fn pump(&mut self, dur: Duration) {
        pump(
            &self.rx,
            dur,
            &mut self.parser,
            &mut self.all,
            &mut self.answered,
            &mut self.writer,
        );
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .expect("resize");
        self.parser.screen_mut().set_size(rows, cols);
    }

    fn rows(&self) -> Vec<String> {
        let (rows, cols) = self.parser.screen().size();
        self.parser
            .screen()
            .rows(0, cols)
            .map(|s| s.trim_end().to_string())
            .take(rows as usize)
            .collect()
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
        assert!(!top.is_empty(), "no session content on screen: {before:?}");
        top
    }

    fn assert_history_present(&self, top: &str) {
        let after = self.rows();
        assert!(
            after.iter().any(|l| l == top),
            "resize drag lost session history / left a stale frame: top line \
             {top:?} is gone.\nafter:\n{}",
            after.join("\n"),
        );
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[test]
fn vertical_resize_drag_does_not_consume_history() {
    let mut t = Term::spawn(ROWS, COLS);
    let top = t.start_with_menu();

    // Keystroke-only repaint policy: during the drag the child must not write
    // a single byte — resize events are consumed silently, and the painted
    // frame is left for the terminal to reflow.
    let before = t.all.len();
    // Drag down one row at a time, then back up.
    for rows in (16u16..ROWS).rev().chain(17u16..=ROWS) {
        t.resize(rows, COLS);
        t.pump(Duration::from_millis(30));
    }
    t.pump(Duration::from_millis(800));
    let delta = t.all.len() - before;
    assert!(
        delta < 64,
        "child wrote {delta} bytes during the resize drag (should repaint only on keystroke)"
    );

    t.assert_history_present(&top);

    // The keystroke is the repaint trigger: it must re-anchor (DSR) and clear
    // the reflowed frame instead of stacking a duplicate.
    t.type_bytes(b"\x1b[D"); // Left: moves the caret, buffer unchanged
    t.pump(Duration::from_millis(600));
    t.assert_history_present(&top);
    let after = t.rows();
    let thinking = after.iter().filter(|l| l.contains("thinking")).count();
    assert_eq!(
        thinking, 1,
        "keystroke repaint after resize duplicated the menu:\n{}",
        after.join("\n"),
    );
}

#[test]
fn horizontal_resize_drag_leaves_no_stale_frames() {
    let mut t = Term::spawn(ROWS, 80);
    let top = t.start_with_menu();

    // Keystroke-only repaint policy: silent during the drag.
    let before = t.all.len();
    // Narrow one column at a time, then widen back.
    for cols in (50u16..COLS).rev().chain(51u16..=COLS) {
        t.resize(ROWS, cols);
        t.pump(Duration::from_millis(30));
    }
    t.pump(Duration::from_millis(800));
    let delta = t.all.len() - before;
    assert!(
        delta < 64,
        "child wrote {delta} bytes during the resize drag (should repaint only on keystroke)"
    );

    t.assert_history_present(&top);
    // A stranded overlay frame would duplicate the menu: the item that sits
    // just below the input ("thinking") must appear exactly once.
    let check = |t: &Term, when: &str| {
        let after = t.rows();
        let thinking = after.iter().filter(|l| l.contains("thinking")).count();
        assert_eq!(
            thinking, 1,
            "{when}: stale overlay frames left on screen (duplicated menu):\n{}",
            after.join("\n"),
        );
    };
    check(&t, "after drag");

    // Keystroke repaint after the width changed: re-anchor + clear must not
    // strand the reflowed frame.
    t.type_bytes(b"\x1b[D"); // Left
    t.pump(Duration::from_millis(600));
    t.assert_history_present(&top);
    check(&t, "after keystroke repaint");
}
