//! Integration test: the inline pop-up must not delete the input line when
//! it opens while the input sits on the *bottom* row of the terminal.
//!
//! Strategy (see discussion in `doc/phase_1_plan.md` §3.7):
//! 1. Spawn `prior` in a *real* pty (so it enters raw mode and believes it
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
//! the cursor to the last row by answering `prior`'s cursor-position query
//! (a DSR, `ESC[6n`) with `ESC[<ROWS>;1R` before it reads it. That makes the
//! input sit on the bottom row, exactly the reported condition.
//!
//! Two `/set` integration tests live here. `settings_string_hierarchy_saves_and_errors`
//! was revived after the v2.5 port (its ignore was stale): the *driving
//! sequence* and the expected error rendering were v2.0-shaped, not the
//! behaviour. Its sibling `settings_change_triggers_deferred_rebuild` was
//! removed rather than revived: its subject — a `/set` change setting a
//! `dirty` flag, with the agent rebuilt at the *next* `Submit` — no longer
//! exists. A v2.5 menu commit is `MenuDone` and the convergence step applies
//! the config snapshot in the same cycle, and `/set default_model` opens a
//! model picker instead of accepting a typed id, so the "unsupported provider"
//! rebuild failure it used to provoke is not reachable through `/set` at all.
//! What it uniquely asserted — that a commit reaches the config file — is now
//! asserted at the end of `settings_string_hierarchy_saves_and_errors`, and the
//! apply/validate behaviour itself is covered by the `v25_set` unit tests.

mod common;

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::Dumps;
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
        if bytes
            .lock()
            .unwrap()
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
        {
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
    let dumps = Dumps::new("popup-scroll");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
    cmd.args(["-p", "openrouter", "-m", "tencent/hy3:free"]);
    // Use a temp directory for XDG_CONFIG_HOME to avoid littering user's config with test sessions.
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn prior");
    // Keep the tempdir alive for the duration of the run.
    let _config_tempdir = config_tempdir;

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    // Wait for the REPL to print its first screen (greeter + status).
    assert!(
        wait_for(&bytes, "prior -", Duration::from_secs(5)),
        "prior did not start the REPL (no greeter); raw-mode/pty setup failed"
    );
    eprintln!("[test] greeter seen");

    // `prior` is now inside `read_input` and has just issued a cursor-position
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

    // `Content`'s cursor re-anchor blocks up to ~60ms on the DSR reply we just
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
    dumps.write("raw.txt", &data);
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
        // What matters is that the selection marker is on **exactly one** row,
        // and that it is the first menu row — not which command that row names.
        //
        // This used to assert `> thinking`, and the first item was renamed to
        // `reasoning` (`8356189`, "Thinking: support `max` …"), so the test failed
        // on a label change with no behaviour change at all. The label is a
        // display string and has now moved once; the marker is the contract.
        let selected: Vec<&String> = mid.iter().filter(|r| r.starts_with("> ")).collect();
        let mid_div = mid.iter().filter(|r| r.starts_with("────")).count();
        eprintln!(
            "[test] MID(after '/') selected={} divider_lines={}\n{:?}",
            selected.len(),
            mid_div,
            mid
        );
        assert_eq!(
            selected.len(),
            1,
            "MID: exactly one selected menu row; screen={:?}",
            mid
        );
        // It is the row right below the divider, i.e. the cursor sits on the
        // first item — which is what "the pop-up opened on the command the user
        // typed" looks like on screen.
        let first_item = mid
            .iter()
            .position(|r| r.starts_with("> "))
            .expect("a selected row");
        assert!(
            mid[..first_item].iter().any(|r| r.starts_with("────")),
            "MID: the selection should be the first item, below the divider; screen={:?}",
            mid
        );
        // And the item is a level list, which is the menu's actual content.
        assert!(
            selected[0].contains("off") && selected[0].contains("max"),
            "MID: the first item should offer the reasoning levels; screen={:?}",
            mid
        );
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

/// wezterm (and xterm-style terminals) send Shift+Enter as the legacy CSI
/// sequence `\x1b[13;2~` even when the Kitty keyboard protocol is *disabled*.
/// crossterm 0.29 parses that as `KeyCode::F(3)` (+SHIFT), which does NOT
/// reach the Enter handler. prior maps `F(3)` -> insert newline, mirroring
/// pi. This test drives the real key bytes through a pty + vt100 emulator and
/// asserts a newline was inserted (the input becomes two rows).
#[test]
fn shift_enter_inserts_newline_without_kitty() {
    let dumps = Dumps::new("shift-enter");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
    cmd.args(["-p", "openrouter", "-m", "tencent/hy3:free"]);
    // Use a temp directory for XDG_CONFIG_HOME to avoid littering user's config with test sessions.
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn prior");
    // Keep the tempdir alive for the duration of the run.
    let _config_tempdir = config_tempdir;

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    assert!(
        wait_for(&bytes, "prior -", Duration::from_secs(5)),
        "prior did not start the REPL (no greeter)"
    );
    // Don't force the input to the bottom row; keep it mid-screen so the
    // multi-line input has room to grow. Answer the cursor-position DSR with
    // a sane row so the startup read doesn't eat our first keystroke.
    let _ = writer.write_all(b"\x1b[3;1R");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(400));

    // Type "abc", send the legacy Shift+Enter sequence, then "d".
    for ch in "abc".chars() {
        let _ = writer.write_all(&[ch as u8]);
        let _ = writer.flush();
        thread::sleep(Duration::from_millis(120));
    }
    let _ = writer.write_all(b"\x1b[13;2~");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(300));
    let _ = writer.write_all(b"d");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(300));

    drop(writer);
    let _ = child.kill();
    stop.store(true, Ordering::SeqCst);

    let data = bytes.lock().unwrap().clone();
    dumps.write("shiftenter.txt", &data);
    let mut parser = Parser::new(ROWS, COLS, 1000);
    parser.process(&data);
    let visible: Vec<String> = parser
        .screen()
        .rows(0, COLS)
        .map(|s| s.trim_end().to_string())
        .collect();
    eprintln!("[test] visible (shift+enter w/o kitty):\n{:?}", visible);

    let has_abc = visible.iter().any(|r| r.trim() == "abc");
    let has_d = visible.iter().any(|r| r.trim() == "d");
    assert!(
        has_abc,
        "expected an input row with 'abc'; visible={:?}",
        visible
    );
    // If `\x1b[13;2~` were ignored (F-key no-op) the buffer would be 'abcd' on
    // one line (no standalone 'd' row). If it submitted, 'abc' would be gone.
    assert!(
        has_d,
        "expected a separate input row with 'd' (Shift+Enter inserted a newline); visible={:?}",
        visible
    );
}

/// On submit the committed prompt is content written *through* the consumed
/// frame — the status bar's row among them. Every line the write enters is
/// cleared on entry (ui_draw_261001's overwrite rule), so a row holding the
/// committed echo cannot also hold bar cells from the frame that used to be
/// there. This pins it for a **multi-line** input with the prompt high in the
/// terminal (the consume regime, not the bottom-row append one): type two
/// lines, submit, and read the screen.
#[test]
fn committed_multiline_prompt_clears_the_bar_row() {
    let dumps = Dumps::new("commit-clear");
    const ROWS: u16 = 16;
    const COLS: u16 = 60;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
    cmd.args(["-p", "openrouter", "-m", "tencent/hy3:free"]);
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn prior");
    let _config_tempdir = config_tempdir;

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    assert!(
        wait_for(&bytes, "prior -", Duration::from_secs(5)),
        "prior did not start the REPL (no greeter)"
    );
    // Prompt high in the terminal: the frame starts at row 3, so submit runs
    // in the consume regime with the bar's row inside the committed span.
    let _ = writer.write_all(b"\x1b[3;1R");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(400));

    // Two input lines (legacy Shift+Enter = newline), then submit.
    let _ = writer.write_all(b"one");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
    let _ = writer.write_all(b"\x1b[13;2~");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(250));
    let _ = writer.write_all(b"two");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
    // The commit writes synchronously on Enter and brackets the echo with a
    // full-width rule — the greeter's dashes are far shorter, so COLS-many
    // `─` in a row only exist once the commit landed.
    let rule_marker = "─".repeat(COLS as usize);
    assert!(
        wait_for(&bytes, &rule_marker, Duration::from_secs(5)),
        "the commit's rule never appeared"
    );
    // Let the turn start (no key — it errors or streams; either way the bar
    // settles below the content) before reading the screen.
    thread::sleep(Duration::from_millis(1500));

    drop(writer);
    let _ = child.kill();
    stop.store(true, Ordering::SeqCst);

    let data = bytes.lock().unwrap().clone();
    dumps.write("commit_clear.txt", &data);
    let mut parser = Parser::new(ROWS, COLS, 1000);
    parser.process(&data);
    let visible: Vec<String> = parser
        .screen()
        .rows(0, COLS)
        .map(|s| s.to_string())
        .collect();
    eprintln!("[test] final screen:\n{:#?}", visible);

    // The committed echo rows are content now: they must hold no bar cells.
    // The bar's identity is its model group text.
    for (i, row) in visible.iter().enumerate() {
        let committed = row.contains("one") || row.contains("two");
        let bar_cells = row.contains("tencent/hy3:free") || row.contains('●');
        assert!(
            !(committed && bar_cells),
            "row {i} mixes the committed prompt with status-bar remnants: {row:?}\n{visible:#?}"
        );
    }
    // And the bar exists exactly once (its current home, below the content) —
    // a second appearance is the old row surviving.
    let bar_rows: Vec<usize> = visible
        .iter()
        .enumerate()
        .filter(|(_, r)| r.contains("tencent/hy3:free"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        bar_rows.len(),
        1,
        "exactly one status bar on screen: rows {bar_rows:?}\n{visible:#?}"
    );
}

/// The reported bug shape: a **sharing** commit (ui_draw_v2 R4) with a first
/// line wider than the terminal. A quiet `/set` commit leaves
/// `last_content_display_kind == Prompt`, so the next commit shares its rule
/// and the block starts *directly with the line* — at the frame's gap row.
/// The line's natural wrap lands on the status bar's row with **no byte
/// between the rows**, and `LineClear` arms on `\n` only: before the fix the
/// bar's right margin survived into the committed input; the write now wraps
/// long lines itself (`row_count::wrap_to_width`), so every row it occupies
/// is entered — and cleared — by a byte.
#[test]
fn sharing_commit_with_a_wrapped_line_still_clears_the_bar_row() {
    let dumps = Dumps::new("commit-share");
    const ROWS: u16 = 16;
    const COLS: u16 = 60;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    // A config file the `/set` leaf can write to (settings-test pattern).
    let cfg_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    let cfg = cfg_tempdir.path().join("prior.toml");
    std::fs::write(&cfg, "\n").expect("write temp config");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
    cmd.args(["-p", "openrouter", "-m", "tencent/hy3:free", "-c"]);
    cmd.arg(cfg.to_str().unwrap());
    cmd.env("XDG_CONFIG_HOME", cfg_tempdir.path());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn prior");
    let _cfg_tempdir = cfg_tempdir;

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    assert!(
        wait_for(&bytes, "prior -", Duration::from_secs(5)),
        "prior did not start the REPL (no greeter)"
    );
    // Prompt high in the terminal (consume regime, bar's row inside the
    // committed span).
    let _ = writer.write_all(b"\x1b[3;1R");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(400));

    // Quiet slash commit: success emits no message, so the last content kind
    // stays `Prompt` and the commit below will share its rule.
    for ch in "/set security_mode yolo".chars() {
        let _ = writer.write_all(&[ch as u8]);
        let _ = writer.flush();
        thread::sleep(Duration::from_millis(80));
    }
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
    // The commit brackets itself with a full-width rule (the greeter's dashes
    // are far shorter), and nothing after it writes content — so this marker
    // lands exactly on the quiet commit whose `Prompt` kind the next commit
    // will share.
    let rule_marker = "─".repeat(COLS as usize);
    assert!(
        wait_for(&bytes, &rule_marker, Duration::from_secs(5)),
        "the quiet slash commit never landed"
    );
    thread::sleep(Duration::from_millis(400));

    // Multi-line prompt whose first line is wider than the terminal.
    let long: String = "x".repeat(COLS as usize + 10);
    let _ = writer.write_all(long.as_bytes());
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(250));
    let _ = writer.write_all(b"\x1b[13;2~");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(250));
    let _ = writer.write_all(b"two");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(150));
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(1500));

    drop(writer);
    let _ = child.kill();
    stop.store(true, Ordering::SeqCst);

    let data = bytes.lock().unwrap().clone();
    dumps.write("commit_share.txt", &data);
    let mut parser = Parser::new(ROWS, COLS, 1000);
    parser.process(&data);
    let visible: Vec<String> = parser
        .screen()
        .rows(0, COLS)
        .map(|s| s.to_string())
        .collect();
    eprintln!("[test] sharing-commit final screen:\n{:#?}", visible);

    for (i, row) in visible.iter().enumerate() {
        let committed = row.contains('x') || row.contains("two");
        let bar_cells = row.contains("tencent/hy3:free") || row.contains('●');
        assert!(
            !(committed && bar_cells),
            "row {i} mixes the committed prompt with status-bar remnants: {row:?}\n{visible:#?}"
        );
    }
    let bar_rows: Vec<usize> = visible
        .iter()
        .enumerate()
        .filter(|(_, r)| r.contains("tencent/hy3:free"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        bar_rows.len(),
        1,
        "exactly one status bar on screen: rows {bar_rows:?}\n{visible:#?}"
    );
}

/// Phase 2 integration: the `/set` string hierarchy — the list
/// appears, a valid value is saved (and the overlay reverts to top
/// `/set`), and an invalid value shows the error message
/// (and is NOT saved). Uses a temp config so the save side-effect
/// never touches the repo.
///
/// Revived after the v2.5 port (the ignore was stale): the driving
/// sequence and the expected error rendering were v2.0-shaped, not the
/// behaviour. A v2.5 menu commit is `MenuDone`, so the editor is empty
/// afterwards and the next command needs its own `/set ` prefix; and a
/// leaf failure comes back as a plain `⏺ …` message rather than a red
/// `error: …` header.
#[test]
fn settings_string_hierarchy_saves_and_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = tmp.path().join("prior.toml");
    // Pre-create so `-c` resolves to an existing (empty) file; the
    // save side-effect then lands here instead of the repo CWD.
    std::fs::write(&cfg, "\n").expect("write temp config");

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 20,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
    cmd.args([
        "-p",
        "openrouter",
        "-m",
        "tencent/hy3:free",
        "-c",
        cfg.to_str().unwrap(),
    ]);
    // Use a temp directory for XDG_CONFIG_HOME to avoid littering user's config with test sessions.
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn prior");
    // Keep the tempdir alive for the duration of the run.
    let _config_tempdir = config_tempdir;

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    assert!(
        wait_for(&bytes, "prior -", Duration::from_secs(5)),
        "REPL did not start"
    );
    // Answer the cursor-position DSR with a sane mid-screen row.
    let _ = writer.write_all(b"\x1b[3;1R");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(400));

    // Open the settings list.
    for ch in "/set ".chars() {
        let _ = writer.write_all(&[ch as u8]);
        let _ = writer.flush();
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        wait_for(&bytes, "default_model", Duration::from_secs(3)),
        "settings list not shown"
    );
    {
        let data = bytes.lock().unwrap().clone();
        let mut p = Parser::new(20, 100, 1000);
        p.process(&data);
        let vis: Vec<String> = p
            .screen()
            .rows(0, 100)
            .map(|s| s.trim().to_string())
            .collect();
        assert!(
            vis.iter().any(|r| r.contains("security_mode")),
            "security_mode row missing: {:?}",
            vis
        );
    }

    // Type a valid value and press Enter -> saves + reverts to top.
    for ch in "security_mode yolo".chars() {
        let _ = writer.write_all(&[ch as u8]);
        let _ = writer.flush();
        thread::sleep(Duration::from_millis(80));
    }
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(400));
    // The commit returns to the prompt with an empty editor (a v2.5 menu
    // commit is `MenuDone`, doc/editor_cleanup.md §10.9), so there is no list
    // to re-open — and the next command must not start on top of a leftover
    // character, or `/set …` becomes ` /set …` and is submitted as a prompt.
    let _ = writer.write_all(b"\x1b");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(300));
    // security_mode should now show `yolo`.
    {
        let data = bytes.lock().unwrap().clone();
        let mut p = Parser::new(20, 100, 1000);
        p.process(&data);
        let vis: Vec<String> = p
            .screen()
            .rows(0, 100)
            .map(|s| s.trim().to_string())
            .collect();
        let sec = vis.iter().find(|r| r.contains("security_mode"));
        assert!(
            sec.map(|r| r.contains("yolo")).unwrap_or(false),
            "security_mode value not saved as yolo: {:?}",
            vis
        );
    }

    // Invalid value -> red error, NOT saved. The commit above left
    // `read_input` (a v2.5 menu commit is `MenuDone`, doc/editor_cleanup.md
    // §10.9), so this command needs its own `/set ` prefix — the v2.0 flow
    // stayed inside the menu and could type the bare setting name.
    for ch in "/set tool_call_limit 0".chars() {
        let _ = writer.write_all(&[ch as u8]);
        let _ = writer.flush();
        thread::sleep(Duration::from_millis(70));
    }
    let _ = writer.write_all(b"\r");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(400));
    {
        let data = bytes.lock().unwrap().clone();
        // v2.0 rendered this as a red `error: …` header, which the old
        // assertion could find in the raw byte stream because only the
        // `error:` word was styled. A v2.5 leaf failure comes back through the
        // `DoneError` path as an ordinary `⏺ …` message, styled around the
        // marker, so the assertion reads the *screen* instead of the bytes.
        let mut p = Parser::new(20, 100, 1000);
        p.process(&data);
        let visible: Vec<String> = p
            .screen()
            .rows(0, 100)
            .map(|r| r.trim().to_string())
            .collect();
        assert!(
            visible
                .iter()
                .any(|r| r.contains("tool_call_limit must be greater than 0")),
            "invalid value did not show the error message: {visible:?}"
        );
    }
    // The error is a committed message, so it stays in scrollback by design —
    // what must *not* happen is a second copy lingering in the input region
    // after the editor is cleared. (v2.0 tied the error to its "value mode"
    // and could clear it with Esc; there is no value mode any more.)
    let _ = writer.write_all(b"\x1b");
    let _ = writer.flush();
    thread::sleep(Duration::from_millis(400));
    {
        let data = bytes.lock().unwrap().clone();
        let mut p = Parser::new(20, 100, 1000);
        p.process(&data);
        let visible: Vec<String> = p
            .screen()
            .rows(0, 100)
            .map(|r| r.trim().to_string())
            .collect();
        let first = visible
            .iter()
            .position(|r| r.contains("must be greater than 0"))
            .unwrap_or_else(|| panic!("error line vanished from scrollback: {visible:?}"));
        assert!(
            !visible[first + 1..]
                .iter()
                .any(|r| r.contains("must be greater")),
            "error must not reappear below itself in the input region: {visible:?}"
        );
    }

    drop(writer);
    let _ = child.kill();
    stop.store(true, Ordering::SeqCst);

    // The value reached the temp config file, not just the in-memory state:
    // this is the end-to-end save proof (it used to live in
    // `settings_change_triggers_deferred_rebuild`, deleted with that test —
    // see the note at the top of this file).
    let saved = std::fs::read_to_string(&cfg).expect("read temp config");
    assert!(
        saved.contains("security_mode = \"yolo\""),
        "setting not persisted to the temp config: {saved}"
    );
}
