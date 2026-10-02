//! Diagnostic / test helper: runs a single `ConsoleOutput::confirm`
//! interaction on a real pty so integration tests can verify the
//! tool-confirmation dialog does not hang and returns the expected choice.
//!
//! This is not part of the shipping CLI; it exists only to give the
//! `confirm_raw_mode` integration test a child process whose controlling
//! terminal is the pty (crossterm's event reader reads `/dev/tty`, which is
//! only the pty when the process is spawned with the pty as its controlling
//! terminal — a plain `dup2` of fd 0 does not suffice).

use picocode::{Confirmation, ConsoleOutput, Output};

fn main() {
    let msg = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tool BASH call?".to_string());

    // Exercise the real raw-mode confirm path (mirrors the REPL's terminal).
    let _ = crossterm::terminal::enable_raw_mode();
    let out = ConsoleOutput::new(picocode::settings::default_show_reasoning_handle());
    let c: Confirmation = out.confirm(&msg);

    // ui_draw_v2 §9 latent issue 1: the dialog must release the in-flight
    // reader gate it raised on entry — a gate left raised would leave every
    // later in-flight reader born paused (Esc/Ctrl-C dead for the session).
    // Printed *before* RESULT so the harness's RESULT wait implies this
    // line has been captured too.
    let gate_released = out
        .confirm_gate()
        .is_none_or(|g| !g.load(std::sync::atomic::Ordering::SeqCst));
    println!("GATE_RELEASED={}", gate_released);

    // Print the resolved choice on its own line so the test can read it back.
    println!("RESULT={:?}", c);
}
