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
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
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
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
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
    let thinking = after.iter().filter(|l| l.contains("reasoning")).count();
    assert_eq!(
        thinking,
        1,
        "keystroke repaint after resize duplicated the menu:\n{}",
        after.join("\n"),
    );
}

/// The area's status row: the only row carrying the **qualified** model id
/// (`provider:model`). The greeter spells the same model `provider (model)`,
/// so matching the colon separates the live area from the session header above.
fn is_status_row(row: &str) -> bool {
    row.contains("openrouter:")
}

/// A vertical resize followed by typing must not stack the area.
///
/// The keystroke is the repaint trigger — the drag policy is silent — so this is
/// where a stale frame becomes visible: the redraw re-anchors via DSR and has to
/// clear the reflowed area instead of stacking a second copy of it.
///
/// All three reported symptoms come from this one interaction, and the damage is
/// progressive, so each round is checked on its own rather than only at the end.
///
/// **Currently failing**, and the failure is the report reproduced rather than a
/// guess: each resize→type cycle adds one blank row above the status row, so
/// round 2 sits three rows clear of the content above it. The status row itself
/// is never duplicated in this harness — the two pre-existing resize tests, and
/// the DSR re-anchor they rely on, still hold. Duplication is reported only when
/// the input row is on the *last* screen row, which needs a screen-filling
/// transcript; this harness starts with a short greeter, so the area sits
/// mid-terminal with room beneath it and there is nothing to overwrite. That
/// half still needs a repro, and getting one is the next step rather than
/// assuming the same fix covers it.
#[test]
fn vertical_resize_then_type_does_not_stack_the_area() {
    let mut t = Term::spawn(ROWS, COLS);
    // Between-turn state: empty input, no menu. `start_with_menu` types `/`,
    // which grows the frame and changes what has to be cleared.
    t.pump(Duration::from_millis(1500));
    assert!(
        t.rows().iter().any(|r| !r.trim().is_empty()),
        "no REPL content on screen"
    );

    for round in 0..3 {
        // Shrink vertically so the fold is mid-terminal on the next round
        // instead of sitting on the last row, then type.
        t.resize(ROWS - 3 - round as u16, COLS);
        t.pump(Duration::from_millis(250));
        t.type_bytes(b"-");
        t.pump(Duration::from_millis(500));

        let rows = t.rows();
        let bars: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| is_status_row(r))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            bars.len(),
            1,
            "round {round}: the status row must appear exactly once, found at {bars:?}: {rows:?}"
        );
        let bar = bars[0];

        // No growing run of blanks above it, and the character that was typed
        // is the input row immediately below.
        let blanks = rows[..bar]
            .iter()
            .rev()
            .take_while(|r| r.trim().is_empty())
            .count();
        assert!(
            blanks <= 2,
            "round {round}: {blanks} blank rows above the status row: {rows:?}"
        );
        assert_eq!(
            rows[bar + 1].trim(),
            "-".repeat(round + 1),
            "round {round}: the typed characters belong on the row below the status \
             row, all of them: {rows:?}"
        );
    }
}

/// **Duplicate the area, not just the drift.** The other resize test starts from
/// a short greeter, so the area lands mid-terminal with room beneath it and a
/// stale row has nowhere obvious to land. Duplication was reported only with the
/// input row on the *last* screen row, which needs the content to fill the
/// screen so the area is pushed there.
///
/// Spawning small rather than typing a transcript keeps this a REPL-only test:
/// under the correct model (`ui_draw_v2` §4) the area sits immediately below the
/// content, so a terminal too short to hold the content *plus* the area leaves
/// the area against the last rows on its own. The test asserts that precondition
/// rather than assuming it, so it cannot pass vacuously. Thirteen rows is not
/// enough — the area still lands mid-screen there; eight is.
///
/// **Passes with and without the reanchor fix, so it does not yet reproduce the
/// reported duplication.** It is kept as a fence for the bottom-anchored regime,
/// which nothing else covers. Two leads remain, and neither is guessable from
/// the report: the duplicate lands in *scrollback*, so whatever produces it also
/// scrolls, and this harness has too little content to scroll; and
/// "re-focusing" has no counterpart here — picocode handles no focus event at
/// all, so it must be the terminal re-emitting something on focus.
#[test]
fn vertical_resize_then_type_at_the_screen_bottom_does_not_duplicate() {
    const SHORT: u16 = 8;

    let mut t = Term::spawn(SHORT, COLS);
    t.pump(Duration::from_millis(1500));

    // Precondition: the area really is on the last rows. Without this, a pass
    // would only be saying the mid-screen case still works.
    let rows = t.rows();
    let bar = rows
        .iter()
        .position(|r| is_status_row(r))
        .expect("the status row should be on screen");
    assert_eq!(
        bar + 2,
        rows.len(),
        "precondition: the area should start against the bottom of a {SHORT}-row \
         terminal, so duplication has somewhere to show up; bar={bar} \
         Screen:\n{}",
        rows.join("\n"),
    );

    for round in 0..3 {
        // Alternate the direction so both reflow shapes are exercised: growing
        // pulls content up out of scrollback, shrinking pushes it back down.
        let height = if round % 2 == 0 { SHORT + 1 } else { SHORT - 1 };
        t.resize(height, COLS);
        t.pump(Duration::from_millis(250));
        t.type_bytes(b"-");
        t.pump(Duration::from_millis(500));

        let rows = t.rows();
        let bars: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| is_status_row(r))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            bars.len(),
            1,
            "round {round} (height {height}): the status row must appear exactly \
             once, found at {bars:?}. A stale row left behind by the re-anchor \
             plus a fresh paint is exactly a duplicate.\nScreen:\n{}",
            rows.join("\n"),
        );
    }
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
    // just below the input ("reasoning") must appear exactly once.
    let check = |t: &Term, when: &str| {
        let after = t.rows();
        let thinking = after.iter().filter(|l| l.contains("reasoning")).count();
        assert_eq!(
            thinking,
            1,
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
