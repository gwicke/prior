//! Regression test for the tool-confirmation hang.
//!
//! Root cause: while the interactive REPL is in raw mode, a tool confirmation
//! dialog used `std::io::read_line`, which waits for a `\n` (0x0A). But
//! `cfmakeraw` disables `ICRNL`, so the Enter key delivers `\r` (0x0D) and
//! `read_line` blocks forever — the client appears frozen with no echoed input.
//! The dialog now reads whole key events through crossterm's raw-mode reader.
//!
//! The dialog is no longer a blocking driver of its own, so this file's subject
//! changed shape rather than disappearing: what it pins now is that the *real*
//! reducer resolves every key the way it used to, on a real pty, and that the
//! frame it applies is the real one. See doc/confirm_as_menu_261002.md §9.
//!
//! The probe drives production code — the port, `ConfirmPrompt::handle_key`
//! and `confirm::paint` — so a divergence between the dialog and the drawing
//! shows up here rather than only in the REPL.

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

    // Wait until the dialog is drawn before typing. There is no DSR query
    // window any more — the dialog no longer re-anchors, it is an overlay in
    // the area the pump already maintains — so the sleep only has to cover the
    // frame. Snapshot the bytes first: this is the screen *while the dialog is
    // up* (ui_draw_v2 P4 pins it below).
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
fn confirm_session_answers_accept() {
    // The *decision* is one bit now; "always" is an approval the REPL records,
    // not a value the agent sees (pinned separately below).
    assert_eq!(run_confirm(b"s").0, "Yes");
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

/// The old dialog had to pause the in-flight reader, because it was a blocking
/// loop reading stdin itself — a second consumer. That made "a gate left raised"
/// a live hazard: every *later* in-flight reader would be born paused and ignore
/// Esc/Ctrl-C for the rest of the session.
///
/// The dialog is session state in the REPL's pump now, so there is one consumer
/// and the gate is never touched at all. That is a stronger claim than the old
/// "released it on the way out", so it is asserted as one: sampled while the
/// dialog is *up*, not after it resolved.
#[test]
fn confirm_never_touches_the_reader_gate() {
    // Read from `final_bytes`, not `mid`: the probe samples the gate while the
    // dialog is up but prints after it resolves, because a write now would land
    // on the dialog's own rows.
    let (result, _, final_bytes) = run_confirm(b"y");
    assert_eq!(result, "Yes");
    assert_eq!(
        find_field(&final_bytes, "GATE_WHILE_UP=").as_deref(),
        Some("false"),
        "the dialog must not pause the in-flight reader while it is up"
    );
    assert_eq!(
        find_field(&final_bytes, "GATE_AFTER=").as_deref(),
        Some("false"),
        "and must not have raised it either"
    );
}

/// *Always* is recorded by the REPL and answered as a plain accept, so the
/// waiting tool call cannot tell it from *yes* — and the tool name is derived
/// from the header, not guessed at paint time.
#[test]
fn confirm_always_is_recorded_and_answers_accept() {
    let (result, _, final_bytes) = run_confirm(b"s");
    assert_eq!(result, "Yes", "always answers the same bit as yes");
    assert_eq!(
        find_field(&final_bytes, "APPROVED=").as_deref(),
        Some("true"),
        "the approval should be recorded under the tool's own name"
    );
}

/// A plain *yes* must not leave an approval behind, or one acceptance would
/// silently authorise the rest of the session.
#[test]
fn confirm_yes_does_not_record_an_approval() {
    let (result, _, final_bytes) = run_confirm(b"y");
    assert_eq!(result, "Yes");
    assert_eq!(
        find_field(&final_bytes, "APPROVED=").as_deref(),
        Some("false"),
        "yes is a one-off; only always approves for the session"
    );
}

/// The dialog's reducer and the answer the waiting tool call receives are two
/// paths to the same fact; disagreement is the worst failure available here.
#[test]
fn the_reducers_answer_and_the_callers_answer_agree() {
    let (result, _, final_bytes) = run_confirm(b"\r");
    assert_eq!(result, "No");
    assert_eq!(
        find_field(&final_bytes, "MISMATCH=").as_deref(),
        Some("false"),
        "the dialog resolved differently from the answer it sent"
    );
}

#[test]
fn confirm_arrow_down_then_enter_answers_accept() {
    // Down arrow moves the selection from the default (No) to Always, which is
    // the *accepting* bit — the approval it records is pinned separately.
    assert_eq!(run_confirm(b"\x1b[B\r").0, "Yes");
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
