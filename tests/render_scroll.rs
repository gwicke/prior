//! Integration test: the inline pop-up must not delete the input line when
//! it opens while the input sits on the *bottom* row of the terminal.
//!
//! Strategy (see discussion in `doc/phase_1_plan.md` §3.7):
//! 1. Spawn `picocode` in a *real* pty (so it enters raw mode and believes it
//!    is on a TTY). The pty only transports bytes — it does not emulate a
//!    screen.
//! 2. Capture the raw output bytes and feed them to a *terminal emulator*
//!    (`vt100`), which faithfully implements scroll / clear / addressing.
//! 3. Assert on the emulator's visible screen: after typing `/`, the input
//!    line `/` must still be present (the bug deleted it, overwriting it with
//!    the pop-up's divider).
//!
//! The bug only triggers when the input line is on the bottom row. With an
//! 8-row terminal the short greeter leaves the input at row 3, so we *force*
//! the cursor to the last row by answering `picocode`'s cursor-position query
//! (a DSR, `ESC[6n`) with `ESC[<ROWS>;1R` before it reads it. That makes the
//! input sit on the bottom row, exactly the reported condition.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use vt100::Parser;

const ROWS: u16 = 8;
const COLS: u16 = 80;

/// Read everything the child emits (best-effort) on a background thread until
/// the child is killed (which closes the pty and ends the read).
fn spawn_reader(mut reader: Box<dyn Read + Send>) -> (Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let bytes_c = bytes.clone();
    let stop_c = stop.clone();
    // Hard deadline so a read() blocked after teardown can never hang us: we
    // never join this thread, it exits on its own once the pty closes (or the
    // deadline passes) and is reaped when the test process exits.
    let deadline = Instant::now() + Duration::from_secs(20);
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            if stop_c.load(Ordering::SeqCst) || Instant::now() > deadline {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => bytes_c.lock().unwrap().extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    });
    (bytes, stop)
}

fn wait_for(bytes: &Arc<Mutex<Vec<u8>>>, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if bytes.lock().unwrap().windows(needle.len()).any(|w| w == needle.as_bytes()) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn popup_scroll_does_not_duplicate_input() {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_picocode"));
    cmd.args(["-p", "openrouter", "-m", "tencent/hy3:free"]);
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("spawn picocode");

    let reader = pair
        .master
        .try_clone_reader()
        .expect("clone reader");
    let (bytes, stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    // Wait for the REPL to print its first screen (greeter + status).
    assert!(
        wait_for(&bytes, "picocode -", Duration::from_secs(5)),
        "picocode did not start the REPL (no greeter); raw-mode/pty setup failed"
    );
    eprintln!("[test] greeter seen");

    // `picocode` is now inside `read_input` and has just issued a cursor-position
    // query (a DSR, `ESC[6n`); it blocks up to ~60ms waiting for the reply
    // `ESC[<row>;<col>R`. Answer with `ESC[<ROWS>;1R` so it believes the input
    // line sits on the *bottom* row — exactly the condition that triggers the
    // reported bug (a `/` pop-up opening while the input is at the bottom of the
    // term). Writing right after the greeter lands inside that 60ms window, and
    // raw mode is already active so the reply is not echoed into the output.
    let _ = writer.write_all(format!("\x1b[{};1R", ROWS).as_bytes());
    let _ = writer.flush();

    // Capture the pre-'/' screen to confirm the cursor was forced to the
    // bottom row (input line at the last row, rows above it empty).
    {
        let data = bytes.lock().unwrap().clone();
        let mut p = Parser::new(ROWS, COLS, 1000);
        p.process(&data);
        let pre: Vec<String> = p
            .screen()
            .rows(0, COLS)
            .map(|s| s.trim_end().to_string())
            .collect();
        eprintln!("[test] PRE('/') screen ({} rows):\n{:?}", pre.len(), pre);
    }

    // `ConsoleOutput::cursor_row()` blocks up to ~60ms on the DSR reply we just
    // supplied. Wait it out before typing so the first `/` reaches the editor
    // rather than the DSR read, then give the opening redraw time to settle.
    thread::sleep(Duration::from_millis(400));

    // Type `/`, then `m`, then `o` — the pop-up grows tall then shrinks.
    // Small delays let each keystroke be processed before the next.
    let mut mid_screen: Option<Vec<String>> = None;
    for (i, ch) in "/mo".chars().enumerate() {
        let _ = writer.write_all(&[ch as u8]);
        let _ = writer.flush();
        eprintln!("[test] typed '{}'", ch);
        // After the first char the pop-up is open; give the redraw time.
        thread::sleep(Duration::from_millis(if i == 0 { 400 } else { 250 }));
        if i == 0 {
            // Capture the screen right after `/` to check the input line is
            // present (not overwritten by the pop-up divider — the bug).
            let data = bytes.lock().unwrap().clone();
            let mut p = Parser::new(ROWS, COLS, 1000);
            p.process(&data);
            mid_screen = Some(
                p.screen()
                    .rows(0, COLS)
                    .map(|s| s.trim_end().to_string())
                    .collect(),
            );
        }
    }
    // Let the final redraw settle.
    thread::sleep(Duration::from_millis(400));
    eprintln!("[test] typing done, tearing down");

    // Tear down: kill the child (closes the pty); the reader thread self-exits.
    // We deliberately do NOT join it: `kill` closing the slave does not always
    // unblock the cloned master read(), and joining would hang. The thread
    // exits on its own deadline and is reaped when the test process exits.
    drop(writer);
    let _ = child.kill();
    stop.store(true, Ordering::SeqCst);

    let data = bytes.lock().unwrap().clone();
    std::fs::write("/tmp/picocode_raw.txt", &data).unwrap();
    let tail = &data[data.len().saturating_sub(400)..];
    eprintln!(
        "[test] raw tail: {}",
        String::from_utf8_lossy(tail).replace('\x1b', "ESC")
    );

    // Feed the whole byte stream to a faithful terminal emulator.
    let mut parser = Parser::new(ROWS, COLS, 1000);
    parser.process(&data);
    let screen = parser.screen();
    let visible: Vec<String> = screen
        .rows(0, COLS)
        .map(|s| s.trim_end().to_string())
        .collect();

    let count_exact = |needle: &str| visible.iter().filter(|r| r.as_str() == needle).count();

    // Diagnostic assertions on the mid-sequence screen (after `/`).
    if let Some(ref mid) = mid_screen {
        let mid_slash = mid.iter().filter(|r| r.starts_with("> /")).count();
        let mid_div = mid.iter().filter(|r| r.starts_with("────")).count();
        eprintln!(
            "[test] MID(after '/') input_lines={} divider_lines={}\n{:?}",
            mid_slash, mid_div, mid
        );
        // The input `/` line (prompt-prefixed as `> /`) must be visible exactly
        // once, with a single divider above it (the pre-input separator).
        assert_eq!(mid_slash, 1, "MID: expected one '> /' input line; screen={:?}", mid);
    }

    // The final input buffer is "/mo" => exactly one such line.
    assert_eq!(
        count_exact("/mo"),
        1,
        "expected exactly one '/mo' input line in the visible screen; got {:?}",
        visible
    );
    // A regression (fixed input row) would leave the earlier frames stacked:
    // a stale "/" and/or "/m" input line still visible below the latest.
    assert_eq!(
        count_exact("/m"),
        0,
        "stale '/m' input frame should be cleared; visible={:?}",
        visible
    );
    assert_eq!(
        count_exact("/"),
        0,
        "stale '/' input frame should be cleared; visible={:?}",
        visible
    );
}
