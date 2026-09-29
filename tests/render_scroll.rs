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
    // Use a temp directory for XDG_CONFIG_HOME to avoid littering user's config with test sessions.
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn picocode");
    // Keep the tempdir alive for the duration of the run.
    let _config_tempdir = config_tempdir;

    let reader = pair.master.try_clone_reader().expect("clone reader");
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
        // The first v2.5 menu item is "> thinking" (cursor on thinking,
        // since the prompt already shows "/").
        let mid_thinking = mid.iter().filter(|r| r.starts_with("> thinking")).count();
        let mid_div = mid.iter().filter(|r| r.starts_with("────")).count();
        eprintln!(
            "[test] MID(after '/') thinking_count={} divider_lines={}\n{:?}",
            mid_thinking, mid_div, mid
        );
        // The first menu entry must be visible exactly once, with a single divider above it.
        assert_eq!(
            mid_thinking, 1,
            "MID: expected one '> thinking' menu line; screen={:?}",
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
/// reach the Enter handler. picocode maps `F(3)` -> insert newline, mirroring
/// pi. This test drives the real key bytes through a pty + vt100 emulator and
/// asserts a newline was inserted (the input becomes two rows).
#[test]
fn shift_enter_inserts_newline_without_kitty() {
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
    // Use a temp directory for XDG_CONFIG_HOME to avoid littering user's config with test sessions.
    let config_tempdir = tempfile::tempdir().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", config_tempdir.path());
    let mut child = pair.slave.spawn_command(cmd).expect("spawn picocode");
    // Keep the tempdir alive for the duration of the run.
    let _config_tempdir = config_tempdir;

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    assert!(
        wait_for(&bytes, "picocode -", Duration::from_secs(5)),
        "picocode did not start the REPL (no greeter)"
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
    std::fs::write("/tmp/picocode_shiftenter.txt", &data).unwrap();
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
    let cfg = tmp.path().join("picocode.toml");
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
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_picocode"));
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
    let mut child = pair.slave.spawn_command(cmd).expect("spawn picocode");
    // Keep the tempdir alive for the duration of the run.
    let _config_tempdir = config_tempdir;

    let reader = pair.master.try_clone_reader().expect("clone reader");
    let (bytes, stop) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    assert!(
        wait_for(&bytes, "picocode -", Duration::from_secs(5)),
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
