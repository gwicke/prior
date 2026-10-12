//! Terminal geometry and capabilities — the first slice of `TermIO`
//! (doc/ui_draw_v2.md, step 2, commit A).
//!
//! This commit lands the two pieces every other module needs *before* the
//! terminal-owning enum itself: the size **watch** that replaces the
//! `repl::TERMINAL_WIDTH` global and the scattered
//! `crossterm::terminal::size()` queries, and the per-stream **caps** probe
//! that replaces the scattered `is_terminal()` calls. The reader thread,
//! raw mode, DSR, and the `TermIO { Tty | Plain }` enum arrive with the
//! following commits.
//!
//! Ownership note: the watch's sender is process-wide for now (a
//! `LazyLock`), because callers like `v25_listing_node` have no handle to
//! thread through yet; step 4 moves the sender into the render manager and
//! the global dies with it.

use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Terminal geometry in logical units (columns / rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    /// Columns, as the terminal last reported them.
    pub width: usize,
    /// Rows, as the terminal last reported them.
    pub height: usize,
}

impl Default for TermSize {
    /// The conventional fallback when the terminal cannot be queried (a
    /// pipe, a test harness): 80×24.
    fn default() -> Self {
        TermSize {
            width: 80,
            height: 24,
        }
    }
}

impl TermSize {
    /// What the terminal reports right now, else the default.
    ///
    /// Under a test harness: always the default. The old `TERMINAL_WIDTH`
    /// global seeded 80×24 in tests (nobody ran the resize handlers), and
    /// every surviving expectation was written against that — probing the
    /// runner's real terminal would make the suite's geometry depend on
    /// where `cargo test` was launched from.
    #[cfg(not(test))]
    pub fn probe() -> Self {
        match crossterm::terminal::size() {
            Ok((w, h)) => TermSize {
                width: w as usize,
                height: h as usize,
            },
            Err(_) => TermSize::default(),
        }
    }

    #[cfg(test)]
    pub fn probe() -> Self {
        TermSize::default()
    }
}

/// The process-wide size channel. Seeded once from a live query (or the
/// default); thereafter written by the resize handlers — the in-flight
/// reader on `Event::Resize`, the parked-key path, and the prompt loop's
/// per-iteration probe (which catches a resize that produced no event) —
/// and read by the renderers through a subscribed receiver.
static SIZE: LazyLock<watch::Sender<TermSize>> =
    LazyLock::new(|| watch::channel(TermSize::probe()).0);

/// Publish a new terminal size. A no-op when the size did not change, so a
/// steady-state resize storm does not wake subscribers for nothing.
pub fn set_size(size: TermSize) {
    SIZE.send_if_modified(|current| {
        if *current == size {
            false
        } else {
            *current = size;
            true
        }
    });
}

/// The latest published size. Never blocks, never queries the terminal —
/// every caller gets the same answer, which is the whole point of replacing
/// the global.
pub fn size() -> TermSize {
    *SIZE.borrow()
}

/// A receiver on the process-wide channel — what `ConsoleOutput` and `Draw`
/// hold so a resize reaches them without another query.
pub fn subscribe() -> watch::Receiver<TermSize> {
    SIZE.subscribe()
}

/// A private watch seeded at `size`: the test harness's stand-in for
/// [`subscribe`], so a test's geometry is per-instance state and needs no
/// test serialization. The sender is dropped — the value can never change,
/// and a closed watch still serves `borrow()`.
#[cfg(test)]
pub(crate) fn fixed(size: TermSize) -> watch::Receiver<TermSize> {
    watch::channel(size).1
}

/// Per-stream terminal capabilities, probed once per process.
///
/// The future `TermIO` enum picks the *input / lifecycle* side
/// (`Tty`/`Plain`, from stdin); caps adjust output fidelity independently.
/// `picocode "q" | cat` is the case that splits them: a tty stdin with a
/// non-tty primary — frames dropped (and, later, output stripped) for the
/// stream, not for the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// The primary output stream (stdout) is a terminal: colour, clickable
    /// links, and — for the REPL — frames.
    pub primary: bool,
    /// The meta stream (stderr) is a terminal.
    pub meta: bool,
}

impl Caps {
    /// Probe the streams where they are. Called once at setup, not per call:
    /// a stream does not become (or stop being) a terminal mid-run, and one
    /// answer per process is what keeps the render path from forking on
    /// `live` per call (row_accounting_b3.md §4.1).
    pub fn probe() -> Self {
        use std::io::IsTerminal;
        Caps {
            primary: std::io::stdout().is_terminal(),
            meta: std::io::stderr().is_terminal(),
        }
    }
}

// --- stdin ownership: the pause handshake + DSR -----------------------------
//
// The persistent reader (the sole stdin owner) must stand down whenever the
// *main* task reads stdin itself: a DSR reply, `$EDITOR`'s keystrokes, or the
// compaction window's key pump. There is exactly one reader and one gate, so
// the gate is process-wide — this is a mutual-exclusion flag, not per-instance
// configuration like the size watch.

/// Raised while the main task owns stdin; the persistent reader backs off
/// (does not call `event::poll`/`read`) for as long as it is `true`.
pub(crate) static PAUSE_GATE: AtomicBool = AtomicBool::new(false);

/// Set by the reader while it is backed off, cleared the moment it is about to
/// read stdin again. A gate-raiser waits on this to know the reader has left
/// any in-flight 50 ms poll — so the bytes it is about to read cannot be
/// swallowed. See [`ReaderPauseGuard`].
pub(crate) static READER_PARKED: AtomicBool = AtomicBool::new(false);

/// Set once the reader thread has started. A gate-raiser only waits on
/// [`READER_PARKED`] when a reader exists to park — in a test harness with no
/// reader there is nothing to wait for, and the wait would be a wasted timeout.
pub(crate) static READER_RUNNING: AtomicBool = AtomicBool::new(false);

/// Pauses the persistent reader for a window in which the main task owns stdin
/// and reads it directly (step 2 commit C) — a DSR query, `$EDITOR`'s
/// takeover, or the compaction window's key pump. Raising waits for the reader
/// to actually park ([`READER_PARKED`]); dropping lowers the gate again.
///
/// Nesting-safe: if the gate is already up, this neither waits nor lowers — the
/// outer holder owns the pause.
pub(crate) struct ReaderPauseGuard {
    /// Did *this* guard raise the gate? Only the raiser lowers it on drop.
    owned: bool,
}

impl ReaderPauseGuard {
    pub(crate) fn raise() -> Self {
        let owned = !PAUSE_GATE.swap(true, Ordering::SeqCst);
        if owned && READER_RUNNING.load(Ordering::SeqCst) {
            // The reader polls on a 50 ms deadline; wait out at most one poll
            // for it to leave the read path and set `READER_PARKED`.
            let start = Instant::now();
            while !READER_PARKED.load(Ordering::SeqCst)
                && start.elapsed() < Duration::from_millis(100)
            {
                thread::sleep(Duration::from_millis(2));
            }
        }
        Self { owned }
    }
}

impl Drop for ReaderPauseGuard {
    fn drop(&mut self) {
        if self.owned {
            PAUSE_GATE.store(false, Ordering::SeqCst);
        }
    }
}

/// The cursor row from a DSR query, answered with the reader paused so the
/// reply cannot be swallowed (step 2 commit C). This is the *whole* of the
/// terminal-I/O-layer DSR: pause dispatch, write the query, match the reply.
pub(crate) fn query_cursor_row() -> Option<usize> {
    let _pause = ReaderPauseGuard::raise();
    query_real_cursor_row()
}

/// Whether the persistent stdin reader is currently paused (the main task owns
/// stdin). Exposed so the confirm probe can assert the confirm dialog never
/// touches the gate; internal coordination goes through [`ReaderPauseGuard`].
pub fn reader_paused() -> bool {
    PAUSE_GATE.load(Ordering::SeqCst)
}

/// Non-blocking Device Status Report: ask the terminal for the real cursor
/// row, timing out fast so we never hang on a terminal that does not answer
/// (e.g. a headless test pty). Returns a 1-indexed row. Reads the reply raw
/// (bypassing crossterm's event parser). Callers should already hold the pause
/// ([`query_cursor_row`]) — this is the raw read.
fn query_real_cursor_row() -> Option<usize> {
    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();
    // Make stdin non-blocking so a missing DSR reply cannot hang us.
    let old = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if old < 0 {
        return None;
    }
    unsafe { libc::fcntl(fd, libc::F_SETFL, old | libc::O_NONBLOCK) };
    let restore = |f: i32, o: i32| unsafe {
        libc::fcntl(f, libc::F_SETFL, o);
    };

    let mut out = io::stdout();
    if out.write_all(b"\x1b[6n").is_err() || out.flush().is_err() {
        restore(fd, old);
        return None;
    }

    let mut guard = stdin.lock();
    let start = Instant::now();
    let mut buf = Vec::new();
    let mut got = false;
    while start.elapsed() < Duration::from_millis(60) {
        let mut b = [0u8; 1];
        match guard.read(&mut b) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(b[0]);
                if b[0] == b'R' {
                    got = true;
                    break;
                }
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
    restore(fd, old);
    if got {
        parse_dsr(&buf)
    } else {
        None
    }
}

/// Parse a Device Status Report reply `ESC[<row>;<col>R` into the 1-indexed row.
fn parse_dsr(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 || buf[0] != 0x1b || buf[1] != b'[' || *buf.last()? != b'R' {
        return None;
    }
    let inner = &buf[2..buf.len() - 1];
    let mut it = inner.splitn(2, |&c| c == b';');
    let row = it.next()?;
    let row_s = std::str::from_utf8(row).ok()?;
    row_s.parse::<usize>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The private watch keeps serving its seed after the sender is gone —
    /// which is what lets `fixed` be a one-line constructor for tests.
    #[test]
    fn a_fixed_watch_serves_its_seed_forever() {
        let rx = fixed(TermSize {
            width: 40,
            height: 24,
        });
        assert_eq!(rx.borrow().width, 40);
        assert_eq!(rx.borrow().height, 24);
    }

    /// `ReaderPauseGuard` owns the pause gate for its lifetime and is
    /// nesting-safe — the replacement for the old `confirm_gate`/`ReaderHold`
    /// plumbing (step 2 commit C). No reader runs in a unit test
    /// (`READER_RUNNING` is false), so `raise` never blocks on a park and the
    /// assertions are purely about the flag's ownership.
    #[test]
    fn a_pause_guard_owns_and_nests_the_gate() {
        assert!(!reader_paused(), "gate starts clear");
        {
            let _outer = ReaderPauseGuard::raise();
            assert!(reader_paused(), "raised for the guard's lifetime");
            {
                let _inner = ReaderPauseGuard::raise();
                assert!(reader_paused(), "a nested raise keeps it up");
            }
            assert!(
                reader_paused(),
                "an inner drop must not lower an outer holder's pause"
            );
        }
        assert!(!reader_paused(), "dropped once, clear again");
    }
}
