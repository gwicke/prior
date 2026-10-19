//! `/quit` must leave the terminal in cooked mode.
//!
//! The REPL flips the pty into raw mode for the process lifetime. Teardown
//! cannot rely on `TermGuard`'s `Drop`: the persistent stdin reader holds an
//! `Arc` clone of the guard, so the strong count never reaches zero before
//! the process exits — and `main` returning runs no other destructor path
//! that reaches it. `run_interactive` therefore tears down explicitly on
//! every loop exit (design §TermIO: the guard's lifecycle belongs to the
//! terminal layer).
//!
//! This is the regression test for that: spawn the real binary on a real
//! pty, confirm raw mode goes *on* (so a startup that silently skips raw
//! mode cannot make this pass vacuously), type `/quit`, let the process
//! exit on its own, and read the termios back off the master fd — the pty's
//! line discipline keeps its settings after the slave side is gone, so
//! whatever the child last left behind is exactly what the next process
//! (the user's shell) would inherit.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// The pty's line-discipline flags, read off the master fd. `cfmakeraw`
/// clears `ICANON`/`ECHO`; `disable_raw_mode` restores them.
fn lflag(master_fd: i32) -> libc::tcflag_t {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::tcgetattr(master_fd, &mut termios) };
    assert_eq!(rc, 0, "tcgetattr on the pty master failed");
    termios.c_lflag
}

/// Spawn the real binary on a pty, wait for raw mode, type `/quit`, wait
/// for the child to exit on its own, and return `(captured output,
/// post-exit line-discipline flags)`. The flags are sampled while the pty
/// pair is still alive (the master fd dies with it), after the child is
/// gone — exactly what the user's shell would inherit.
fn run_quit_on_pty() -> (String, libc::tcflag_t) {
    let home = tempfile::tempdir().expect("temp home");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    let master_fd = pair.master.as_raw_fd().expect("master fd");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_prior"));
    // Isolate from the developer's real config, sessions and tokens — the
    // test only needs the prompt loop to start and `/quit` to leave it.
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join(".config"));
    cmd.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(cmd).expect("spawn prior");

    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut writer = pair.master.take_writer().expect("take writer");

    // Capture everything the child emits, with a hard deadline so a blocked
    // read can never hang the test process.
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let bytes_c = bytes.clone();
    let stop_c = stop.clone();
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

    // Raw mode on: poll the termios rather than sleeping — the assertion is
    // that raw mode really was entered, so a startup that never flips it
    // fails here instead of letting the post-exit check pass vacuously.
    let raw_deadline = Instant::now() + Duration::from_secs(15);
    while lflag(master_fd) & libc::ICANON != 0 {
        assert!(
            Instant::now() < raw_deadline,
            "prior never entered raw mode (output so far: {:?})",
            String::from_utf8_lossy(&bytes.lock().unwrap())
        );
        thread::sleep(Duration::from_millis(20));
    }
    // Raw mode goes on before the entry DSR window closes (a 60 ms direct
    // stdin read that would swallow typed keys) and before the persistent
    // reader owns stdin. Wait for the first frame to settle — no new
    // output for a quarter second — so the keystrokes land at the prompt.
    let settle_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let seen = bytes.lock().unwrap().len();
        thread::sleep(Duration::from_millis(250));
        if bytes.lock().unwrap().len() == seen {
            break;
        }
        assert!(
            Instant::now() < settle_deadline,
            "output never settled (so far: {:?})",
            String::from_utf8_lossy(&bytes.lock().unwrap())
        );
    }

    let _ = writer.write_all(b"/quit\r");
    let _ = writer.flush();

    // The child must exit on its own; a `/quit` that hangs fails here.
    let exit_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                assert!(
                    Instant::now() < exit_deadline,
                    "prior did not exit after /quit (output so far: {:?})",
                    String::from_utf8_lossy(&bytes.lock().unwrap())
                );
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("waiting for prior failed: {e}"),
        }
    }

    // Post-exit sample, before the pair (and the fd) goes away.
    let flags = lflag(master_fd);
    stop.store(true, Ordering::SeqCst);
    drop(writer);
    let out = String::from_utf8_lossy(&bytes.lock().unwrap()).into_owned();
    (out, flags)
}

#[test]
fn quit_restores_cooked_mode_on_exit() {
    // The primary claim: the pty the shell would inherit is back in
    // canonical mode with echo on. Teardown that runs too late — only on
    // process death, or not at all — leaves raw mode here.
    let (_out, flags) = run_quit_on_pty();
    assert!(
        flags & libc::ICANON != 0,
        "/quit left the terminal in raw mode: ICANON still clear"
    );
    assert!(
        flags & libc::ECHO != 0,
        "/quit left echo disabled on the terminal"
    );
}

#[test]
fn quit_emits_stream_teardown_escapes() {
    // Secondary, escape-level evidence in the captured bytes: the bracketed
    // paste disable is the last stream write of `TermGuard::stop`. Split
    // from the termios claim so a stream-only regression (escapes gated off
    // by mistake) cannot mask or be masked by a mode regression.
    let (out, _flags) = run_quit_on_pty();
    assert!(
        out.contains("\x1b[?2004l"),
        "no bracketed-paste teardown escape in output: {out:?}"
    );
}
