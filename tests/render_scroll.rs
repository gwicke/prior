//! Integration test: the inline pop-up must not leave duplicated input frames
//! when it opens tall (causing a terminal scroll) and then shrinks as the user
//! types. This guards the `Repl::next_input_row` scroll-tracking fix.
//!
//! Strategy (see discussion in `doc/phase_1_plan.md` §3.7):
//! 1. Spawn `picocode` in a *real* pty (so it enters raw mode and believes it
//!    is on a TTY). The pty only transports bytes — it does not emulate a
//!    screen.
//! 2. Capture the raw output bytes and feed them to a *terminal emulator*
//!    (`vt100`), which faithfully implements scroll / clear / addressing.
//! 3. Assert on the emulator's visible screen: the latest input line must
//!    appear exactly once, with no stale earlier input frame left behind.
//!
//! We force the scroll path with a short (8-row) terminal: the greeter + input
//! occupy the top rows, the `/` command pop-up is tall enough to overflow the
//! bottom, so the first keystroke scrolls and subsequent keystrokes must track
//! that scroll (otherwise a second input frame is drawn lower down).

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
fn spawn_reader(mut reader: Box<dyn Read + Send>) -> (Arc<Mutex<Vec<u8>>>, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let bytes_c = bytes.clone();
    let stop_c = stop.clone();
    let handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            if stop_c.load(Ordering::SeqCst) {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => bytes_c.lock().unwrap().extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
    });
    (bytes, stop, handle)
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
    let (bytes, stop, reader_handle) = spawn_reader(reader);
    let mut writer = pair.master.take_writer().expect("take writer");

    // Wait for the REPL to print its first screen (greeter + status).
    assert!(
        wait_for(&bytes, "picocode -", Duration::from_secs(5)),
        "picocode did not start the REPL (no greeter); raw-mode/pty setup failed"
    );

    // Type `/`, then `m`, then `o` — the pop-up grows tall then shrinks.
    // Small delays let each keystroke be processed before the next.
    for (i, ch) in "/mo".chars().enumerate() {
        let _ = writer.write_all(&[ch as u8]);
        let _ = writer.flush();
        // After the first char the pop-up is open; give the redraw time.
        thread::sleep(Duration::from_millis(if i == 0 { 400 } else { 250 }));
    }
    // Let the final redraw settle.
    thread::sleep(Duration::from_millis(400));

    // Tear down: kill the child (closes the pty), stop the reader.
    let _ = child.kill();
    let _ = child.wait();
    stop.store(true, Ordering::SeqCst);
    let _ = reader_handle.join();

    let data = bytes.lock().unwrap().clone();

    // Feed the whole byte stream to a faithful terminal emulator.
    let mut parser = Parser::new(ROWS, COLS, 1000);
    parser.process(&data);
    let screen = parser.screen();
    let visible: Vec<String> = screen
        .rows(0, COLS)
        .map(|s| s.trim_end().to_string())
        .collect();

    let count_exact = |needle: &str| visible.iter().filter(|r| r.as_str() == needle).count();

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
