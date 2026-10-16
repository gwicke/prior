//! Diagnostic / test helper: drives **one** confirmation to a decision on a
//! real pty, so integration tests can verify the dialog does not hang in raw
//! mode and that its keys resolve the way they used to.
//!
//! This is not part of the shipping CLI; it exists only to give the
//! `confirm_raw_mode` integration test a child process whose controlling
//! terminal is the pty (crossterm's event reader reads `/dev/tty`, which is
//! only the pty when the process is spawned with the pty as its controlling
//! terminal — a plain `dup2` of fd 0 does not suffice).
//!
//! **What it drives is production code**, not a copy: the request goes through
//! the real port, the key is reduced by the real
//! [`ConfirmPrompt::handle_key`], and the frame is applied through the real
//! element builder. The only thing it does not exercise is the session's
//! *append below an open menu*, which needs an `AppState` a probe cannot have —
//! that is pinned by a session-level test instead. See
//! `doc/confirm_as_menu_261002.md` §9.

use crossterm::event::{self, Event, KeyEvent, KeyEventKind, KeyModifiers};
use picocode::confirm::{ConfirmBridge, ConfirmDecision};
use picocode::termio::TermIO;
use picocode::ConsoleOutput;

#[tokio::main]
async fn main() {
    let msg = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tool BASH call?".to_string());

    // Exercise the real raw-mode confirm path (mirrors the REPL's terminal).
    let _ = crossterm::terminal::enable_raw_mode();
    // Plain is enough for an out-of-crate driver: caps probe the real
    // streams either way, no reader is wanted (the probe reads stdin
    // itself), and the frames apply through the hub's writer.
    let out = ConsoleOutput::new(
        TermIO::plain(),
        picocode::settings::default_show_reasoning_handle(),
    );
    // The frame dims come from the content handle's `frame_size`:
    // the watched geometry, read fresh per paint — the same watch
    // `ConsoleOutput`'s answer came from, so the probe composes exactly
    // what it did before.

    let mut bridge = ConfirmBridge::new();
    let tool = msg
        .strip_prefix("Confirm tool ")
        .and_then(|s| s.strip_suffix(" call?"))
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "bash".to_string());
    let port = bridge.port.clone();

    // The caller waits on the reply, exactly as a guarded tool call does.
    let ask = tokio::spawn({
        let port = port.clone();
        let tool = tool.clone();
        let msg = msg.clone();
        async move { port.request(&tool, msg).await }
    });

    let Some(req) = bridge.requests.recv().await else {
        println!("RESULT=NoRequest");
        return;
    };
    let mut confirm = picocode::confirm::ConfirmPrompt::new(req, bridge.port.approvals().clone());
    picocode::confirm::paint(&confirm, &out.hub());

    // The dialog no longer takes the reader gate — that was the whole reason it
    // had to be a blocking loop. Sampled while it is up, so the test can assert
    // the gate is untouched rather than merely released on the way out.
    //
    // Sampled here, *printed* at the end: a `println!` now would land on the
    // pty's own rows and overwrite the frame the positional assertions measure.
    let gate_up = picocode::termio::reader_paused();

    let decision = loop {
        // Raw mode delivers `Enter` as CR and no line discipline translates it,
        // so a blocking `read_line` would wait forever here. The dialog reads
        // whole key events instead, which is the property this test exists for.
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                ..
            })) => {
                if modifiers == KeyModifiers::NONE {
                    if let Some(decided) = confirm.handle_key(KeyEvent::new(code, modifiers)) {
                        break decided;
                    }
                    picocode::confirm::paint(&confirm, &out.hub());
                }
            }
            Ok(Event::Resize(_, _)) => {
                picocode::confirm::paint(&confirm, &out.hub());
            }
            Ok(_) => {}
            Err(_) => break ConfirmDecision::Deny,
        }
    };

    // The answer the *caller* gets is the one that matters: it is what
    // unblocks the tool call, and a probe whose own copy disagreed with it
    // would pin the copy.
    let answered = ask.await.unwrap_or(ConfirmDecision::Deny);
    // Same resolve the REPL's dispatcher runs: repaint without the dialog,
    // then write the decision line where it was.
    picocode::confirm::resolve(
        out.content().as_ref(),
        || {
            // The empty membership: with nothing configured the area is
            // nothing, and applying it is the erase (dropping the element is
            // the clear). Configure-then-apply, the same two-step
            // `confirm::paint` above runs — `configure` is the membership
            // verb, so a paint composes nothing.
            let hub = out.hub();
            let mut hub = hub.lock().unwrap();
            hub.configure(picocode::render::FRAME_MAX_ROW_RATIO, Vec::new());
            hub.render(None);
        },
        confirm.subject(),
        answered,
    );
    // The reducer's own answer and the one the waiting call received are two
    // paths to the same fact. Disagreement would mean the dialog resolved one
    // way and the tool was told another, which is the worst possible failure
    // here, so the probe reports it rather than the test having to guess.
    println!("MISMATCH={}", decision != answered);
    let approved = port.approvals().is_approved(&tool);
    println!(
        "RESULT={}",
        if answered == ConfirmDecision::Accept {
            "Yes"
        } else {
            "No"
        }
    );
    println!("APPROVED={approved}");
    // Printed after RESULT so a harness waiting on RESULT has captured it too.
    println!("GATE_WHILE_UP={gate_up}");
    println!("GATE_AFTER={}", picocode::termio::reader_paused());
}
