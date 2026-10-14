//! Terminal I/O — the `TermIO` layer (impl doc
//! `doc/261003_output_drawing_refactor_impl.md`, step 4a; the setup rework,
//! 4f-e).
//!
//! The closed `TermIO { Tty | Plain }` enum is the sole owner of the terminal:
//! the **Tty** variant folds the former `repl::TermGuard` (raw-mode lifecycle)
//! and owns the persistent stdin reader — the one thread that, for the process
//! lifetime, calls `event::poll`/`event::read` — plus the SIGCONT handler that
//! re-enters raw mode after a suspend. **Plain** (piped input, `--quiet`) has
//! no reader and no raw mode; its channels are closed at construction, the
//! shape piped mode had when it simply dropped the sender.
//!
//! Constructed first at setup (4f-e, in `main`), before `Content`: it probes
//! the caps once at construction ([`TermIO::caps`]) and owns the size watch —
//! one channel per instance, no process-wide sender. Its receivers feed
//! content (format width) and the render manager (frame dims) through
//! [`TermIO::subscribe_size`]; those who publish ([`TermIO::set_size`],
//! [`TermIO::probe_size`], the reader) write through the same instance, reached
//! via the render manager's front door once `Draw` owns it.
//!
//! Setup hands out [`TermHandles`] exactly once (`TermIO::handles`, called
//! through the manager once `Draw` holds it): the `input` channel to the
//! repl, plus fresh `size` / `caret` receivers.
//!
//! Not yet here (tracked in the impl doc): DSR answered inside `TermIO`
//! (4g). Since 4f-e-b every production byte — frames, the re-anchor, the
//! content choreography — leaves through [`TermIO::write`].

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
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

// The per-instance size watch lives on the variant structs below (one channel
// per `TermIO`, seeded from [`TermSize::probe`] at construction); its sender
// methods are [`TermIO::set_size`] / [`TermIO::subscribe_size`] /
// [`TermIO::probe_size`]. The process-wide `LazyLock` channel these used to
// wrap died in 4f-e.

/// A private watch seeded at `size`: the test harness's stand-in for a
/// subscription, so a test's geometry is per-instance state and needs no
/// test serialization. The sender is dropped — the value can never change,
/// and a closed watch still serves `borrow()`.
#[cfg(any(test, feature = "test-helpers"))]
pub fn fixed(size: TermSize) -> watch::Receiver<TermSize> {
    watch::channel(size).1
}

/// One event on `TermIO`'s input channel — what the repl consumes
/// (design doc §TermIO). The reader converts at the send edge
/// ([`InputEvent::from_event`]); crossterm stays an input-side detail, below
/// this channel and in the window pump that reads stdin directly while the
/// reader is paused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// A key press. Still a `crossterm` type "for now" (design doc §TermIO):
    /// the prompt's dispatcher speaks it directly.
    Key(KeyEvent),
    /// Bracketed paste payload — a bulk insert for the prompt.
    Paste(String),
    /// A settled resize — on the channel since 4f-e-d: the reader forwards
    /// what it also publishes to the size watch (both edges, design
    /// §TermIO), and the window pump parks one for the same arms to drain.
    /// Consumers republish idempotently and wait; nobody acts per event
    /// (see [`route_event`]).
    Resize(TermSize),
}

impl InputEvent {
    /// The send-edge conversion: `Key`/`Paste`/`Resize` map one-to-one;
    /// anything the prompt could never show (mouse, focus) has nowhere to go
    /// and returns `None` — dropped here, exactly as the reader's routing
    /// drops it before the channel and the park drops it at drain time
    /// (it was ignored there anyway).
    pub fn from_event(ev: Event) -> Option<Self> {
        match ev {
            Event::Key(key) => Some(InputEvent::Key(key)),
            Event::Paste(text) => Some(InputEvent::Paste(text)),
            Event::Resize(w, h) => Some(InputEvent::Resize(TermSize {
                width: w as usize,
                height: h as usize,
            })),
            _ => None,
        }
    }
}

/// Caret invalidation pushed toward the render manager: SIGCONT / resume
/// means the shell may have repainted, so the cached frame rows can no longer
/// be trusted (design doc §9). The reader produces it (after its own
/// raw-mode re-entry); the render manager is the one consumer — it drains
/// the channel in `poll_caret` and re-anchors via DSR (4g-c, design §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaretEvent {
    Invalidated,
}

/// Acquired once at setup, then distributed (design doc §3): the reader's
/// channel to the repl, `caret` to the render manager — its only consumer,
/// taking the receiver in `Interactive::new`'s hand-out (4g-c) — and `size`,
/// a fresh receiver on the instance's watch (content and the manager usually
/// already hold their own, taken at construction; the hand-out's third is a
/// spare). `TermIO` keeps the senders until `handles()` hands the receivers
/// out, once.
pub struct TermHandles {
    /// Keys / paste / (parked) resize — one consumer: the repl.
    pub input: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    /// The latest published geometry — a receiver on this instance's watch.
    pub size: watch::Receiver<TermSize>,
    /// Caret invalidation (SIGCONT / resume) — the render manager only.
    pub caret: tokio::sync::mpsc::UnboundedReceiver<CaretEvent>,
}

/// The stream token shared by Content → Render → Draw → TermIO (design doc
/// §Render layer): content picks the stream, below it the only vocabulary is
/// main / meta, so render and draw stay generic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Primary output (stdout).
    Main,
    /// Meta output (stderr) — reasoning when it is routed there.
    Meta,
}

/// Per-stream terminal capabilities, probed once per process.
///
/// `TermIO` picks the *input / lifecycle* side
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

// --- the TermIO type --------------------------------------------------------

/// Where the persistent stdin reader sends one terminal event (moved from
/// `repl.rs` with the reader). The *policy* is the part worth arguing about
/// and the part worth testing: which keys may wait, and which may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderRoute {
    /// Deal with it here — process/terminal/abort state, or an event with
    /// nobody to hand it to.
    Inline,
    /// Hand it to the prompt loop / turn's key branch on the channel.
    Forward,
}

/// Route one terminal event as it arrives on stdin.
///
/// **Inline — must not wait:**
/// - `Ctrl-C` / `Ctrl-Z`: process and terminal state (`exit`,
///   `killpg(SIGTSTP)`), handled where they land, never queued.
///
/// **Forwarded:** every other key, plus `Paste` **and `Resize`** — resize
/// travels *both* edges since 4f-e-d (design §TermIO: "resize → both the
/// `size` watch and the `input` channel"; the reader publishes the watch as
/// the event arrives, then forwards). The channel copy is the repl's, and
/// the repl consumes it *without acting*: `read_input`'s arms republish
/// idempotently and keep waiting, the turn's dispatch ignores it — so a
/// drag's stream never forces a per-event repaint (that was the whole
/// argument for keeping it inline; it survives as the watch half). The
/// repaint stays where it was: the loop-top probe's once-per-keystroke
/// compare, which runs the wipe + replay + re-anchor — a keystroke scrolls
/// the cursor into view first, where a timer cannot.
///
/// The rest of `Paste`/keys: content for the prompt; `read_input` already
/// treats `Event::Paste` as a bulk insert, so the turn's key branch reads
/// them the same way (U4.3). `Esc` is one of them, and that is a change:
///
/// `Esc` used to be Inline, and Inline meant *abort* — unconditionally, from
/// here. Settled U4 makes it menu-first: the ladder runs while a menu is
/// open, and it aborts only at root. This thread cannot make that call (it
/// has no session to look at), so the decision moves to the one that can,
/// and `Esc` rides the forwarded path with everything else. The cost is
/// explicit: a root `Esc` is no longer guaranteed to land ahead of a paste
/// backlog, where it was before. `Ctrl-C` still is, and it still never
/// queues — that is what "immediately reachable" now rests on.
///
/// Everything else — mouse, focus — returns [`ReaderRoute::Inline`] and is
/// ignored, exactly as it was before there was anywhere to forward to.
pub fn route_event(ev: &Event) -> ReaderRoute {
    match ev {
        Event::Resize(..) => ReaderRoute::Forward,
        Event::Key(key) => match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ReaderRoute::Inline
            }
            KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                ReaderRoute::Inline
            }
            _ => ReaderRoute::Forward,
        },
        Event::Paste(..) => ReaderRoute::Forward,
        _ => ReaderRoute::Inline,
    }
}

/// Set by the `SIGCONT` handler on resume from `Ctrl-Z`. Read by the
/// persistent stdin reader, which re-enters raw mode once it sees the flag
/// and then pushes the caret invalidation (a [`CaretEvent`]) toward the
/// render manager — the whole resume signal is this one flag plus that
/// event; the loop-local `RESUMED_REANCHOR` flag died in 4g-c.
pub static RESUMED: AtomicBool = AtomicBool::new(false);

/// `SIGCONT` handler: record that the process resumed.
/// It must stay async-signal-safe (no I/O, no allocation), so the actual
/// terminal re-entry and the invalidation push are performed by the reader
/// thread (a normal thread context), which polls [`RESUMED`].
extern "C" fn on_sigcont(_sig: i32) {
    RESUMED.store(true, Ordering::SeqCst);
}

/// Register the `SIGCONT` handler exactly once. Safe to call multiple
/// times; `libc::signal` just replaces the handler. Required so that
/// `Ctrl-Z` suspend/resume restores raw mode. Called by [`TermIO::tty`] —
/// raw mode is a Tty concern, so its resume signal is too.
pub fn register_sigcont() {
    unsafe {
        libc::signal(libc::SIGCONT, on_sigcont as *const () as usize);
    }
}

/// The closed terminal backend — "a closed type, not a trait: exactly two
/// runtime behaviors, and TermIO is not an extension point" (design doc
/// §TermIO). The variant picks the input / lifecycle side; caps adjust output
/// fidelity separately (see [`Caps`]).
///
/// Wiring (design doc §3): build `TermIO` first (4f-e, `main`), take its
/// caps and a size-watch receiver for `Content::new`, move the enum itself
/// under `Draw`, and call [`handles`](Self::handles) once through the render
/// manager — the one hand-out, at the pause-gated entry-DSR window. No layer
/// holds a reference into another.
pub enum TermIO {
    /// Interactive tty: raw mode, the sole stdin reader, the SIGCONT handler.
    Tty(TtyIo),
    /// Piped input or `--quiet`: line stdin, no raw mode, no reader, closed
    /// input channel. `--quiet` is not a backend (design doc §TermIO): it
    /// selects Plain, whose answer is printed from `run_once`'s return.
    Plain(PlainIo),
}

/// The Tty half: the folded mode guard (the former `TermGuard` — raw-mode
/// lifecycle, shared by every caller that must flip it: the reader, the
/// prompt loop, `$EDITOR`'s takeover), the reader's sending halves, and the
/// handles handed out once.
pub struct TtyIo {
    /// The canonical mode guard — raw-mode lifecycle, shared by every caller
    /// that must flip it: the reader, the prompt loop, `$EDITOR`'s takeover.
    /// The render manager's suspend/resume front door reaches it through
    /// `Draw`, which owns this enum since 4f-e (the `set_mode_guard` hand-off
    /// of 4f-d retired with it).
    guard: Arc<TermGuard>,
    /// Did this instance's [`TermIO::suspend`] raise the pause gate? Only
    /// that raise is released by the matching [`TermIO::resume`] — an outer
    /// holder's pause (the compaction window) outlives a nested
    /// suspend/resume pair (4f-e-c).
    pause_owned: AtomicBool,
    /// The per-instance size watch's sending half: the reader publishes
    /// resizes here, and [`TermIO::set_size`] / [`TermIO::probe_size`] write
    /// through it. Receivers clone off it ([`TermIO::subscribe_size`]).
    size_tx: watch::Sender<TermSize>,
    /// Probed once at construction ([`Caps::probe`]) — the caps `Content` is
    /// seeded from, so the probe site lives here and not above.
    caps: Caps,
    /// Sender half of [`TermHandles::input`], moved into the reader thread
    /// by [`TermIO::handles`].
    input_tx: Option<UnboundedSender<InputEvent>>,
    /// Sender half of [`TermHandles::caret`] — held by the reader, which
    /// pushes [`CaretEvent::Invalidated`] on resume.
    caret_tx: UnboundedSender<CaretEvent>,
    /// The receivers, taken exactly once by [`TermIO::handles`].
    handles: Option<TermHandles>,
}

/// The Plain half: no reader, no raw mode. Its only behavior is where
/// `write` lands — the real streams in production, or a test sinkhole /
/// memory writer via [`TermIO::null`] / [`TermIO::recording`].
pub struct PlainIo {
    sink: Sink,
    /// The per-instance size watch's sending half — same shape as Tty's;
    /// Plain has no reader to publish from it, but the front door can still
    /// probe (`--quiet` and piped runs answer from their own watch now, not
    /// a process-wide one).
    size_tx: watch::Sender<TermSize>,
    /// Probed once at construction — see [`TtyIo::caps`].
    caps: Caps,
    handles: Option<TermHandles>,
}

/// Where Plain's bytes go. The test constructors pick a non-stdio sink;
/// production picks [`Sink::Stdio`].
enum Sink {
    /// stdout / stderr, per stream.
    Stdio,
    /// In-memory, per stream — [`TermIO::recording`] (test builds only;
    /// nothing in production records).
    #[cfg(any(test, feature = "test-helpers"))]
    Recording { main: Vec<u8>, meta: Vec<u8> },
    /// Discard — [`TermIO::null`].
    Null,
}

/// Fresh input + size + caret channels for one instance. The input/caret
/// senders go to the reader (Tty) or are dropped outright (Plain and the
/// test constructors: no reader will ever send); the size watch's sender
/// stays with the variant, seeded from a live probe.
fn term_channels() -> (
    TermHandles,
    UnboundedSender<InputEvent>,
    UnboundedSender<CaretEvent>,
    watch::Sender<TermSize>,
) {
    let (input_tx, input_rx) = unbounded_channel();
    let (caret_tx, caret_rx) = unbounded_channel();
    let (size_tx, size_rx) = watch::channel(TermSize::probe());
    let handles = TermHandles {
        input: input_rx,
        size: size_rx,
        caret: caret_rx,
    };
    (handles, input_tx, caret_tx, size_tx)
}

impl TermIO {
    /// The interactive-tty backend: register the resume handler, fold the
    /// mode guard, build the channels. Does **not** start the reader —
    /// [`handles`] does, so the caller can raise the pause gate first and the
    /// entry DSR cannot be swallowed (the ordering `Interactive::new`
    /// documents).
    ///
    /// [`handles`]: Self::handles
    pub fn tty() -> Self {
        register_sigcont();
        let (handles, input_tx, caret_tx, size_tx) = term_channels();
        TermIO::Tty(TtyIo {
            guard: Arc::new(TermGuard::new()),
            pause_owned: AtomicBool::new(false),
            size_tx,
            caps: Caps::probe(),
            input_tx: Some(input_tx),
            caret_tx,
            handles: Some(handles),
        })
    }

    /// The non-interactive backend (piped stdin or `--quiet`): no reader, no
    /// raw mode, closed channels — the shape piped mode had when it dropped
    /// the sender half.
    pub fn plain() -> Self {
        let (handles, input_tx, caret_tx, size_tx) = term_channels();
        drop(input_tx);
        drop(caret_tx);
        TermIO::Plain(PlainIo {
            sink: Sink::Stdio,
            size_tx,
            caps: Caps::probe(),
            handles: Some(handles),
        })
    }

    /// Memory writers, per stream (design doc §Testing): what byte-edge
    /// assertions record through once `Draw` writes via `TermIO`. Stripped
    /// bytes with frames dropped by caps are Draw's decision, above this.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn recording() -> Self {
        let (handles, input_tx, caret_tx, size_tx) = term_channels();
        drop(input_tx);
        drop(caret_tx);
        TermIO::Plain(PlainIo {
            sink: Sink::Recording {
                main: Vec::new(),
                meta: Vec::new(),
            },
            size_tx,
            caps: Caps::probe(),
            handles: Some(handles),
        })
    }

    /// The sinkhole backend: bytes in, nothing out.
    pub fn null() -> Self {
        let (handles, input_tx, caret_tx, size_tx) = term_channels();
        drop(input_tx);
        drop(caret_tx);
        TermIO::Plain(PlainIo {
            sink: Sink::Null,
            size_tx,
            caps: Caps::probe(),
            handles: Some(handles),
        })
    }

    /// The caps probed at construction — what `Content::new` is seeded from
    /// (design §Draw: the probe site lives here, not above).
    pub fn caps(&self) -> Caps {
        match self {
            TermIO::Tty(t) => t.caps,
            TermIO::Plain(p) => p.caps,
        }
    }

    /// The design's `suspend()` (§TermIO: "stops the reader and drops raw
    /// mode"), complete since 4f-e-c: park the sole stdin owner — raise the
    /// pause gate, waiting out its last poll — then drop raw mode, the
    /// order `$EDITOR`'s hand-off used to run by hand before folding into
    /// here. Idempotent; a no-op on Plain. Nested against an outer holder
    /// (the compaction window, a DSR query): only the raise *this* call
    /// made is remembered to lower — [`Self::resume`] releases exactly that.
    pub fn suspend(&self) -> io::Result<()> {
        match self {
            TermIO::Tty(t) => {
                if raise_pause_gate() {
                    t.pause_owned.store(true, Ordering::SeqCst);
                }
                t.guard.stop()
            }
            TermIO::Plain(_) => Ok(()),
        }
    }

    /// Raw mode on — the reverse of [`Self::suspend`], and also the initial
    /// entry at prompt-loop startup (the guard's idempotent `start`). The
    /// reader-stop lifts after raw mode is back, and only if this instance
    /// raised it (see [`Self::suspend`]). A no-op on Plain.
    pub fn resume(&self) -> io::Result<()> {
        match self {
            TermIO::Tty(t) => {
                let started = t.guard.start();
                if t.pause_owned.swap(false, Ordering::SeqCst) {
                    PAUSE_GATE.store(false, Ordering::SeqCst);
                }
                started
            }
            TermIO::Plain(_) => Ok(()),
        }
    }

    /// Whether this terminal flips raw mode at all (Tty, not Plain) — what
    /// `$EDITOR` and the window pump ask before expecting a mode turnover.
    /// The predicate the old `mode_guard: Option<…>` field's mere presence
    /// answered (4f-d).
    pub fn is_tty(&self) -> bool {
        matches!(self, TermIO::Tty(_))
    }

    /// Publish a new terminal size on this instance's watch. A no-op when
    /// the size did not change, so a steady-state resize storm does not wake
    /// subscribers for nothing.
    pub fn set_size(&self, size: TermSize) {
        self.size_tx().send_if_modified(|current| {
            if *current == size {
                false
            } else {
                *current = size;
                true
            }
        });
    }

    /// A receiver on this instance's watch — what content (format width) and
    /// the render manager (frame dims) are seeded with at construction,
    /// before the channel handles are distributed.
    pub fn subscribe_size(&self) -> watch::Receiver<TermSize> {
        self.size_tx().subscribe()
    }

    /// The size watch's sending half: the reader's clone (inline resizes)
    /// and the front-door forwards.
    fn size_tx(&self) -> &watch::Sender<TermSize> {
        match self {
            TermIO::Tty(t) => &t.size_tx,
            TermIO::Plain(p) => &p.size_tx,
        }
    }

    /// Probe the terminal for its current size and publish the answer — the
    /// prompt loop's live query moved under this roof (4f-e): one live read
    /// for a resize that produced no event, written to the watch every
    /// reader consumes instead of only to the loop's local variables. Never
    /// falls back to the watch: the point of the probe is to notice what the
    /// watch has not.
    pub fn probe_size(&self) -> TermSize {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        let size = TermSize {
            width: w as usize,
            height: h as usize,
        };
        self.set_size(size);
        size
    }

    /// Acquire the channel handles — once, at setup (design doc §3). On Tty
    /// this also **starts the sole stdin reader** (the reason it is not the
    /// constructor: the caller raises the pause gate first, so the reader
    /// starts parked and cannot swallow the entry DSR's reply).
    ///
    /// Panics on a second call: wiring happens once per process, and a
    /// silently closed second set of handles would be harder to diagnose
    /// than the mistake.
    pub fn handles(&mut self) -> TermHandles {
        match self {
            TermIO::Tty(t) => {
                let handles = t
                    .handles
                    .take()
                    .expect("TermIO::handles() is called once at setup");
                let input_tx = t
                    .input_tx
                    .take()
                    .expect("TermIO::handles() is called once at setup");
                spawn_stdin_reader(
                    t.guard.clone(),
                    input_tx,
                    t.caret_tx.clone(),
                    t.size_tx.clone(),
                );
                handles
            }
            TermIO::Plain(p) => p
                .handles
                .take()
                .expect("TermIO::handles() is called once at setup"),
        }
    }

    // `guard()` — the 4f-d hand-off that published the `Arc` into
    // `RenderManager::set_mode_guard` — died with this move: `Draw` owns the
    // enum now, and [`Self::suspend`] / [`Self::resume`] / [`Self::is_tty`]
    // are the front door the manager forwards to.

    /// Bytes to the stream's sink (design doc §TermIO `write`). `Draw`'s
    /// every byte comes through here since 4f-e-b (the writer field died);
    /// the byte-edge tests read what [`recording`](Self::recording) holds at
    /// the same edge, and [`null`](Self::null) discards.
    pub fn write(&mut self, s: Stream, bytes: &[u8]) -> io::Result<()> {
        match self {
            TermIO::Tty(_) => write_stdio(s, bytes),
            TermIO::Plain(p) => match &mut p.sink {
                Sink::Stdio => write_stdio(s, bytes),
                #[cfg(any(test, feature = "test-helpers"))]
                Sink::Recording { main, meta } => {
                    match s {
                        Stream::Main => main.extend_from_slice(bytes),
                        Stream::Meta => meta.extend_from_slice(bytes),
                    }
                    Ok(())
                }
                Sink::Null => Ok(()),
            },
        }
    }

    /// What [`recording`](Self::recording) captured for `s` — empty for any
    /// other backend (they store nothing).
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn recorded(&self, s: Stream) -> Vec<u8> {
        match self {
            TermIO::Tty(_) => Vec::new(),
            TermIO::Plain(p) => match &p.sink {
                Sink::Recording { main, meta } => match s {
                    Stream::Main => main.clone(),
                    Stream::Meta => meta.clone(),
                },
                _ => Vec::new(),
            },
        }
    }
}

/// Production sink: the real streams, flushed per write (a frame must be on
/// screen before the next one is planned; there is no batching layer below
/// this yet).
fn write_stdio(s: Stream, bytes: &[u8]) -> io::Result<()> {
    match s {
        Stream::Main => {
            let mut out = io::stdout();
            out.write_all(bytes)?;
            out.flush()
        }
        Stream::Meta => {
            let mut err = io::stderr();
            err.write_all(bytes)?;
            err.flush()
        }
    }
}

/// The sole stdin owner: the reader thread (moved from `repl.rs` with the
/// `TermIO` type). What to do with each event is [`route_event`]'s call, not
/// a pile of branches here. The reader handles three things inline and
/// forwards the rest on `input` (ui_draw_v2 §4.5 U4.2); until U4.2 nothing
/// was forwarded and edit keys typed mid-request were simply dropped.
///
/// `input` is **unbounded** on purpose: a bounded channel that filled would
/// block this thread at precisely the moment it has to deliver `Ctrl-C`.
///
/// It is spawned once, when `TermIO::handles` runs on the Tty variant, and
/// runs for the process lifetime — it, and only it, calls
/// `event::poll`/`event::read`. The prompt loop and every in-flight turn
/// consume the events it forwards instead of touching stdin, which is what
/// removes the three-way race between the prompt's blocking reads, the turn's
/// reader, and the DSR query (the last of which is answered by pausing this
/// reader — see [`query_cursor_row`]).
///
/// In raw mode `event::read` is blocking, so a background thread is what sees
/// `Ctrl-C`/`Ctrl-Z` arriving while the main task is parked on a turn. `Esc`
/// is forwarded, not handled here: U4.3 made it menu-first, a decision this
/// thread cannot make without a session to look at; see [`route_event`].
///
/// `guard` is `Arc` and `stop()` is process-wide, so calling it from this
/// thread is safe. The process-wide pause gate ([`PAUSE_GATE`]) is raised
/// by the main task whenever *it* owns stdin — `$EDITOR`'s takeover, a DSR
/// query, or the compaction window — and while it is `true` this reader backs
/// off (sleeps, does not read) so it cannot swallow the bytes the main task is
/// waiting for; [`READER_PARKED`] signals that it has genuinely parked.
pub fn spawn_stdin_reader(
    guard: Arc<TermGuard>,
    input: UnboundedSender<InputEvent>,
    caret: UnboundedSender<CaretEvent>,
    size: watch::Sender<TermSize>,
) {
    // A gate-raiser only waits for a park when a reader exists to park; set it
    // before the thread starts so the entry-DSR's pause below sees it.
    READER_RUNNING.store(true, Ordering::SeqCst);
    let _ = thread::spawn(move || loop {
        if RESUMED.swap(false, Ordering::SeqCst) {
            let _ = guard.start();
            // Resume invalidated the screen (design doc §9): push it toward
            // the render manager, the channel's one consumer since 4g-c
            // (it drains the event in `poll_caret` and re-anchors via DSR).
            // Sent only after `guard.start()`, so the invalidation cannot
            // reach the manager before raw mode is back — and before any
            // later key this thread forwards, so the loop cannot wake on a
            // post-resume key with the event still queued.
            let _ = caret.send(CaretEvent::Invalidated);
        }
        // The main task owns stdin right now ($EDITOR, a DSR query, or the
        // compaction window); back off so we don't consume the bytes it is
        // waiting for. `READER_PARKED` tells a gate-raiser that we have
        // left any in-flight poll and are genuinely not reading.
        if PAUSE_GATE.load(Ordering::SeqCst) {
            READER_PARKED.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        READER_PARKED.store(false, Ordering::SeqCst);
        if let Ok(true) = event::poll(Duration::from_millis(50)) {
            if let Ok(ev) = event::read() {
                // Resize: *both* edges (4f-e-d, design §TermIO). The
                // watch first — geometry readers (the streaming
                // renderers, the loop-top compare) must see the new
                // size by the time anyone reacts to the channel copy —
                // then the forward. Deduped like every other publish:
                // a storm of identical values must not wake subscribers
                // for nothing, and the repl's republish of the same
                // size is a no-op.
                if let Event::Resize(w, h) = &ev {
                    let new = TermSize {
                        width: *w as usize,
                        height: *h as usize,
                    };
                    let _ = size.send_if_modified(|current| {
                        if *current == new {
                            false
                        } else {
                            *current = new;
                            true
                        }
                    });
                }
                // U4.2: edit keys this thread used to drop now go to the
                // prompt loop or the turn. `send` on an unbounded tokio mpsc
                // never blocks, so this is safe from a plain OS thread — and
                // being non-blocking is the whole reason the channel is
                // unbounded (see `route_event`).
                if route_event(&ev) == ReaderRoute::Forward {
                    if let Some(fwd) = InputEvent::from_event(ev) {
                        let _ = input.send(fwd);
                    }
                } else if let Event::Key(key) = ev {
                    if let KeyCode::Char('c') = key.code {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            let _ = guard.stop();
                            std::process::exit(1);
                        }
                    } else if let KeyCode::Char('z') = key.code {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            let _ = guard.stop();
                            unsafe {
                                libc::killpg(libc::getpgrp(), libc::SIGTSTP);
                            }
                        }
                    }
                }
                // Anything else Inline (mouse, focus): dropped, as
                // before.
            }
        }
    });
}

/// Owns terminal mode transitions — the folded former `repl::TermGuard`.
/// [`TermIO::Tty`] holds the canonical instance and keeps the only `Arc`
/// clones: its own and the reader's (resume / `Ctrl-C` / `Ctrl-Z`). The
/// prompt loop, `$EDITOR` and the back-off pump flip raw mode through the
/// render manager's suspend/resume front door, which reaches this enum
/// under `Draw` since 4f-e — no holder outside `TermIO` names the guard
/// (`TermIO::guard()` died with the `set_mode_guard` hand-off).
/// [`start`](Self::start) enters raw mode and negotiates capabilities;
/// [`stop`](Self::stop) (idempotent) restores cooked mode. Safe to call from
/// `Drop`, a panic hook, or a signal handler.
///
/// The design gives the whole lifecycle to `TermIO`: suspend/resume with
/// the reader-stop is the enum's since 4f-e-c, Ctrl-C/Ctrl-Z are handled
/// in the reader — DSR answered inside `TermIO` is 4g.
pub struct TermGuard {
    active: AtomicBool,
    saved_paste: AtomicBool,
}

impl TermGuard {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            saved_paste: AtomicBool::new(false),
        }
    }

    /// Enter raw mode and negotiate capabilities. Idempotent.
    pub fn start(&self) -> io::Result<()> {
        if self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.saved_paste.store(false, Ordering::SeqCst);
        enable_raw_mode()?;
        // crossterm's `enable_raw_mode` calls `cfmakeraw`, which clears `OPOST`.
        // With `OPOST` off the terminal does NOT translate `\n` -> `\r\n`, so
        // every bare `\n` (help text, separators, agent responses) only moves
        // down a row and leaves the cursor at the same column — producing the
        // staggered "indentation" seen on a full screen. Re-enable output
        // post-processing (specifically `ONLCR`) so line breaks return to
        // column 0, while keeping raw *input* semantics (no canonical line
        // editing, no echo). `disable_raw_mode` restores the original mode.
        enable_output_processing();
        Self::csi("?2004h")?; // bracketed paste on
                              // Kitty keyboard protocol: disambiguate keys + report all keys as CSI u,
                              // so Shift+Enter arrives as Enter+SHIFT (newline) and Alt+Letter as a
                              // distinct Alt-modified char. Honored by wezterm/alacritty/kitty.
        crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            )
        )?;
        self.active.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Restore cooked mode. Idempotent; safe to call multiple times / from `Drop`.
    pub fn stop(&self) -> io::Result<()> {
        if !self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Pop one level of Kitty keyboard enhancement (matches the single push).
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        if self.saved_paste.load(Ordering::SeqCst) {
            Self::csi("?2004h")?;
        } else {
            Self::csi("?2004l")?;
        }
        // The status line hides the cursor while a turn streams, and
        // `draw_frame` shows it again when the prompt returns. If the turn is
        // interrupted instead — a tool error, a confirm, a panic — nothing shows
        // it again, and an invisible cursor follows the user into their shell.
        // Unconditional and idempotent: showing a cursor that was never hidden
        // costs nothing, and this is the last writer before the tty is the
        // user's again.
        io::stdout().write_all(crate::ansi::SHOW_CURSOR)?;
        disable_raw_mode()?;
        self.active.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn csi(seq: &str) -> io::Result<()> {
        let mut s = String::from("\x1b[");
        s.push_str(seq);
        io::stdout().write_all(s.as_bytes())?;
        io::stdout().flush()
    }
}

impl Default for TermGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Re-enable terminal output post-processing (specifically `OPOST`/`ONLCR`)
/// on the stdout terminal. `crossterm::enable_raw_mode` runs `cfmakeraw`, which
/// disables `OPOST`; without it a bare `\n` does not return the cursor to
/// column 0, so agent responses and separators stagger. We operate on fd 1
/// (stdout), which is the pty slave when interactive and a pipe otherwise
/// (tcsetattr on a pipe harmlessly fails). Best-effort: errors are ignored.
fn enable_output_processing() {
    let fd = libc::STDOUT_FILENO;
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) != 0 {
            return;
        }
        termios.c_oflag |= libc::OPOST | libc::ONLCR;
        let _ = libc::tcsetattr(fd, libc::TCSANOW, &termios);
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
pub static PAUSE_GATE: AtomicBool = AtomicBool::new(false);

/// Set by the reader while it is backed off, cleared the moment it is about to
/// read stdin again. A gate-raiser waits on this to know the reader has left
/// any in-flight 50 ms poll — so the bytes it is about to read cannot be
/// swallowed. See [`ReaderPauseGuard`].
pub static READER_PARKED: AtomicBool = AtomicBool::new(false);

/// Set once the reader thread has started. A gate-raiser only waits on
/// [`READER_PARKED`] when a reader exists to park — in a test harness with no
/// reader there is nothing to wait for, and the wait would be a wasted timeout.
pub static READER_RUNNING: AtomicBool = AtomicBool::new(false);

/// Pauses the persistent reader for a window in which the main task owns stdin
/// and reads it directly (step 2 commit C) — a DSR query, `$EDITOR`'s
/// takeover, or the compaction window's key pump. Raising waits for the reader
/// to actually park ([`READER_PARKED`]); dropping lowers the gate again.
///
/// Nesting-safe: if the gate is already up, this neither waits nor lowers — the
/// outer holder owns the pause.
pub struct ReaderPauseGuard {
    /// Did *this* guard raise the gate? Only the raiser lowers it on drop.
    owned: bool,
}

impl ReaderPauseGuard {
    pub fn raise() -> Self {
        Self {
            owned: raise_pause_gate(),
        }
    }
}

/// Raise the pause gate and wait the reader out; `true` when *this* call
/// raised it — the raiser owns lowering it ([`ReaderPauseGuard`] drops its
/// own, [`TermIO::suspend`] remembers its own across the editor window).
/// Nesting-safe: a call on an already-up gate neither waits nor claims.
fn raise_pause_gate() -> bool {
    let owned = !PAUSE_GATE.swap(true, Ordering::SeqCst);
    if owned && READER_RUNNING.load(Ordering::SeqCst) {
        // The reader polls on a 50 ms deadline; wait out at most one poll
        // for it to leave the read path and set `READER_PARKED`.
        let start = Instant::now();
        while !READER_PARKED.load(Ordering::SeqCst) && start.elapsed() < Duration::from_millis(100)
        {
            thread::sleep(Duration::from_millis(2));
        }
    }
    owned
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
pub fn query_cursor_row() -> Option<usize> {
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

    /// The reader-stop half of `TermIO::suspend` (4f-e-c): suspend parks
    /// the reader before dropping raw mode, a repeated suspend keeps its
    /// claim, and the matching resume releases exactly that raise — an
    /// outer holder's pause (the compaction window) outlives a nested
    /// suspend/resume pair.
    ///
    /// Gated on a non-terminal stdin because `resume()` enters raw mode:
    /// flipping the harness's real tty mid-suite (and spraying CSI at the
    /// log) is what `a_pause_guard…`'s doc means by a unit test not touching
    /// the terminal — the same escape hatch the bwrap regression uses.
    #[test]
    fn termio_suspend_parks_the_reader_and_resume_releases_only_its_own_pause() {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            return;
        }
        let io = TermIO::tty();
        assert!(!reader_paused(), "gate starts clear");

        io.suspend().unwrap();
        assert!(reader_paused(), "suspend parks the reader");
        io.suspend().unwrap();
        let _ = io.resume(); // raw-mode flip may fail without a tty; the
                             // gate release runs regardless
        assert!(!reader_paused(), "resume lifts the pause suspend raised");

        // An outer holder: its pause must survive a nested pair.
        let outer = ReaderPauseGuard::raise();
        io.suspend().unwrap();
        let _ = io.resume();
        assert!(
            reader_paused(),
            "an outer holder keeps the gate up across a nested suspend/resume"
        );
        drop(outer);
        assert!(!reader_paused(), "the outer drop clears it");
    }

    // --- the routing policy, moved from `repl.rs` with the reader (4a) ------

    /// The in-flight reader's routing policy: which events may wait, and which
    /// may not.
    ///
    /// Pinned as a *correctness* property rather than a preference — "an abort
    /// cannot ride behind a paste backlog" (ui_draw_v2 §4.5) — and the
    /// channel's unboundedness follows from it: a bounded one that filled would
    /// block the reader at precisely the moment it has to deliver `Ctrl-C`.
    ///
    /// `Esc` used to be in this list and is deliberately gone from it now, for
    /// the reason [`esc_is_forwarded_so_the_turn_can_decide`] pins.
    #[test]
    fn an_abort_never_queues_behind_content() {
        let urgent = [
            (
                "Ctrl-C",
                Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ),
            (
                "Ctrl-Z",
                Event::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            ),
        ];
        for (what, ev) in urgent {
            assert_eq!(
                route_event(&ev),
                ReaderRoute::Inline,
                "{} must be handled inline, never queued",
                what
            );
        }
    }

    /// Resize travels *both* edges since 4f-e-d (design §TermIO): the watch
    /// for the geometry readers, the input channel for the repl — which
    /// consumes it without acting (the prompt's arms republish idempotently
    /// and keep waiting, the turn's dispatch ignores it), so forwarding is
    /// not the per-event repaint the Inline policy guarded against. That
    /// guard survives as the watch half: only the reader publishes it while
    /// the main task is parked.
    #[test]
    fn resize_travels_both_edges() {
        assert_eq!(
            route_event(&Event::Resize(120, 40)),
            ReaderRoute::Forward,
            "the channel copy is the repl's half of the split"
        );
    }

    /// `Esc` is forwarded, and that is the fix for the settled menu-first rule
    /// (ui_draw_v2 §4.5, "abort only at root").
    ///
    /// It could not be decided where it used to be decided: routing `Esc`
    /// Inline here meant routing it to *abort*, with no way to ask whether a
    /// menu was open, because this function has no session to look at. Moving
    /// it to the forwarded path is what lets the turn answer the question
    /// correctly — step the ladder when a menu is up, stop the turn when there
    /// is not. The trade is stated in [`route_event`]'s doc: a root `Esc` now
    /// rides the queue like any other key.
    #[test]
    fn esc_is_forwarded_so_the_turn_can_decide() {
        let ev = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(route_event(&ev), ReaderRoute::Forward);
    }

    /// Everything the prompt can hold is content, and content goes to the turn
    /// instead of being dropped the way it was before U4.2.
    #[test]
    fn what_the_prompt_can_hold_is_forwarded() {
        let content = [
            (
                "a character",
                Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            ),
            (
                "Enter",
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ),
            (
                "Shift+Enter (a newline)",
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            ),
            (
                "Up",
                Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            ),
            (
                "Backspace",
                Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            ),
            ("a paste", Event::Paste("hello world".to_string())),
        ];
        for (what, ev) in content {
            assert_eq!(
                route_event(&ev),
                ReaderRoute::Forward,
                "{} is content for the prompt",
                what
            );
        }
    }

    /// Events with nowhere to go keep their old fate instead of being
    /// forwarded, so the channel carries no more than the prompt could ever
    /// show.
    #[test]
    fn events_with_nowhere_to_go_stay_inline() {
        let ev = Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(route_event(&ev), ReaderRoute::Inline);
    }

    // --- the TermIO type itself (4a) ----------------------------------------

    /// The send-edge conversion: three variants map one-to-one, everything
    /// else (mouse, focus, whatever a window pump might park) has nowhere to
    /// go and is dropped — the fate it already had at the drain.
    #[test]
    fn from_event_maps_the_prompt_vocabulary_and_drops_the_rest() {
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(
            InputEvent::from_event(Event::Key(key)),
            Some(InputEvent::Key(key))
        );
        assert_eq!(
            InputEvent::from_event(Event::Paste("hi".into())),
            Some(InputEvent::Paste("hi".into()))
        );
        assert_eq!(
            InputEvent::from_event(Event::Resize(120, 40)),
            Some(InputEvent::Resize(TermSize {
                width: 120,
                height: 40
            }))
        );
        let mouse = Event::Mouse(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 1,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(InputEvent::from_event(mouse), None);
    }

    /// Plain's channels arrive **closed** — no reader exists to feed them —
    /// which is exactly the shape piped/quiet mode had when it dropped the
    /// sender half of `stdin_channel`. `size` still serves the live watch.
    #[test]
    fn plain_handles_have_no_reader_and_closed_channels() {
        let mut io = TermIO::plain();
        let mut handles = io.handles();
        assert!(
            handles.input.try_recv().is_err(),
            "no reader ever sends on Plain's input"
        );
        assert!(
            handles.caret.try_recv().is_err(),
            "no reader ever invalidates Plain's caret"
        );
        assert!(!io.is_tty(), "Plain has no raw mode to flip");
    }

    /// Wiring happens once: a second `handles()` would hand out a second
    /// receiver set nobody feeds, so the mistake is a panic, not silence.
    #[test]
    #[should_panic(expected = "called once at setup")]
    fn handles_are_handed_out_once() {
        let mut io = TermIO::plain();
        let first = io.handles();
        drop(first);
        let _second = io.handles();
    }

    /// The Tty variant folds the mode guard: `guard()` hands out the shared
    /// handle from its canonical instance, Plain has none. (No reader is
    /// spawned — that is `handles()`' job, and a unit test must not start a
    /// thread reading the harness's stdin.)
    #[test]
    fn the_mode_guard_lives_with_the_tty_variant() {
        let io = TermIO::tty();
        assert!(io.is_tty(), "Tty owns the guard");
        let plain = TermIO::plain();
        assert!(!plain.is_tty());
    }

    /// `recording()` captures bytes per stream and `null()` discards them —
    /// the two test constructors the byte-edge tests record through (design
    /// doc §Testing). Production writes are `Draw`'s writer today; these pin
    /// the `TermIO::write` surface that will carry them.
    #[test]
    fn recording_captures_per_stream_and_null_discards() {
        let mut rec = TermIO::recording();
        rec.write(Stream::Main, b"frame").unwrap();
        rec.write(Stream::Meta, b"reason").unwrap();
        rec.write(Stream::Main, b" more").unwrap();
        assert_eq!(rec.recorded(Stream::Main), b"frame more");
        assert_eq!(rec.recorded(Stream::Meta), b"reason");

        let mut sink = TermIO::null();
        sink.write(Stream::Main, b"gone").unwrap();
        assert_eq!(sink.recorded(Stream::Main), b"");
    }
}
