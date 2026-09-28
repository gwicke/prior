//! Regression test for the tool-confirmation hang.
//!
//! Root cause: while the interactive REPL is in raw mode, a tool confirmation
//! dialog (`ConsoleOutput::confirm`) used `std::io::read_line`, which waits
//! for a `\n` (0x0A). But `cfmakeraw` disables `ICRNL`, so the Enter key
//! delivers `\r` (0x0D) and `read_line` blocks forever — the client appears
//! frozen with no echoed input. The fix renders the choice as an overlay and
//! reads a single key via crossterm's raw-mode event reader (which understands
//! raw-mode Enter), and asks the background in-flight reader to stand down so
//! it doesn't swallow the keystrokes.
//!
//! This test spawns the `confirm_probe` child through a *real* pty (so the
//! child's controlling terminal — and thus crossterm's `/dev/tty` event
//! reader — is the pty), feeds a key from the pty master, and asserts the
//! dialog returns promptly with the right answer (i.e. it does not hang).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// Emulator screen rows (trimmed) for a captured byte dump.
fn screen_rows(data: &[u8]) -> Vec<String> {
    let mut p = vt100::Parser::new(ROWS, COLS, 1000);
    p.process(data);
    let s = p.screen();
    (0..ROWS)
        .map(|r| {
            (0..COLS)
                .map(|c| {
                    s.cell(r, c)
                        .and_then(|cell| cell.contents().chars().next())
                        .unwrap_or(' ')
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// Spawn `confirm_probe` in a pty, feed `key` from the master after the dialog
/// has drawn (and the DSR query window has passed), and return
/// `(RESULT=..., bytes captured while the dialog was up, final bytes)`.
fn run_confirm(key: &[u8]) -> (String, Vec<u8>, Vec<u8>) {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_confirm_probe"));
    cmd.args(["tool BASH call?"]);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn probe");

    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut writer = pair.master.take_writer().expect("take writer");

    // Best-effort capture of everything the child emits, with a hard deadline
    // so a blocked read can never hang the test process.
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let bytes_c = bytes.clone();
    let stop_c = stop.clone();
    let deadline = Instant::now() + Duration::from_secs(10);
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

    // Wait until the overlay is drawn and the DSR query window (first ~60ms of
    // `confirm`) has passed before typing, so the key reaches `event::read`
    // rather than the DSR read. Snapshot the bytes first: this is the screen
    // *while the dialog is up* (ui_draw_v2 P4 pins it below).
    thread::sleep(Duration::from_millis(400));
    let mid = bytes.lock().unwrap().clone();
    let _ = writer.write_all(key);
    let _ = writer.flush();

    // Wait for the `RESULT=...` line.
    let result = {
        let d2 = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let b = bytes.lock().unwrap();
                if let Some(pos) = b.windows(7).position(|w| w == b"RESULT=") {
                    let rest = &b[pos + 7..];
                    let end = rest
                        .iter()
                        .position(|&c| c == b'\n' || c == b'\r')
                        .unwrap_or(rest.len());
                    break String::from_utf8_lossy(&rest[..end]).to_string();
                }
            }
            if Instant::now() > d2 {
                break "<timeout>".to_string();
            }
            thread::sleep(Duration::from_millis(20));
        }
    };

    // Final bytes = everything the child emitted before teardown.
    let final_bytes = bytes.lock().unwrap().clone();
    drop(writer);
    let _ = child.kill();
    stop.store(true, Ordering::SeqCst);
    (result, mid, final_bytes)
}

/// Find `marker` in the captured bytes; return the rest of its line.
fn find_field(data: &[u8], marker: &str) -> Option<String> {
    let pos = data
        .windows(marker.len())
        .position(|w| w == marker.as_bytes())?;
    let rest = &data[pos + marker.len()..];
    let end = rest
        .iter()
        .position(|&c| c == b'\n' || c == b'\r')
        .unwrap_or(rest.len());
    Some(String::from_utf8_lossy(&rest[..end]).to_string())
}

#[test]
fn confirm_yes_returns_without_hang() {
    assert_eq!(run_confirm(b"y").0, "Yes");
}

#[test]
fn confirm_session_returns_always() {
    assert_eq!(run_confirm(b"s").0, "Always");
}

#[test]
fn confirm_enter_defaults_to_no() {
    // Enter (raw mode sends CR, 0x0D) -> No (preserves prior read_line("") default)
    assert_eq!(run_confirm(b"\r").0, "No");
}

#[test]
fn confirm_n_returns_no() {
    assert_eq!(run_confirm(b"n").0, "No");
}

/// ui_draw_v2 §9 latent issue 1: on resolve the dialog must release the
/// in-flight reader gate it raised on entry. A gate left raised makes every
/// *later* in-flight reader born paused — it would ignore Esc/Ctrl-C for the
/// rest of the session (the mid-turn Esc handler died exactly this way when
/// the confirm driver moved to `ui::confirm` without the release).
#[test]
fn confirm_releases_reader_gate_on_resolve() {
    let (result, _, final_bytes) = run_confirm(b"y");
    assert_eq!(result, "Yes");
    assert_eq!(
        find_field(&final_bytes, "GATE_RELEASED=").as_deref(),
        Some("true"),
        "confirm gate left raised after resolve"
    );
}

#[test]
fn confirm_arrow_down_then_enter_selects_always() {
    // Down arrow moves the selection from the default (No) to Always.
    assert_eq!(run_confirm(b"\x1b[B\r").0, "Always");
}

#[test]
fn confirm_arrow_up_then_enter_selects_yes() {
    // Up arrow moves the selection from the default (No) to Yes.
    assert_eq!(run_confirm(b"\x1b[A\r").0, "Yes");
}

/// ui_draw_v2 P4: the dialog is drawn through the frame pipeline (divider +
/// header + options at the cursor row) and, on resolve, its whole region is
/// cleared through the same pipeline — leaving only the synthetic decision
/// line where the dialog was.
#[test]
fn confirm_dialog_paints_then_clears_with_decision_line() {
    let (result, mid, final_bytes) = run_confirm(b"y");
    assert_eq!(result, "Yes");

    // While up: divider, header and the option rows are on screen.
    let mid_rows = screen_rows(&mid);
    assert!(
        mid_rows
            .iter()
            .any(|r| !r.is_empty() && r.chars().all(|c| c == '─')),
        "dialog divider missing: {mid_rows:?}"
    );
    assert!(
        mid_rows.iter().any(|r| r.contains("tool BASH call?")),
        "dialog header missing: {mid_rows:?}"
    );
    assert!(
        mid_rows.iter().any(|r| r.contains("Always (session)")),
        "dialog options missing: {mid_rows:?}"
    );

    // After resolve: options and divider cleared, decision line recorded.
    let rows = screen_rows(&final_bytes);
    assert!(
        rows.iter().any(|r| r == "accepted tool BASH"),
        "decision line missing: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("Always (session)")),
        "dialog residue after resolve: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r == "tool BASH call?"),
        "header residue after resolve: {rows:?}"
    );
    assert!(
        !rows
            .iter()
            .any(|r| !r.is_empty() && r.chars().all(|c| c == '─')),
        "divider residue after resolve: {rows:?}"
    );
}
