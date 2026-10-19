//! Terminal I/O — the `TermIO` layer (design
//! `doc/261003_output_drawing_refactor.md`, §TermIO).
//!
//! The closed `TermIO { Tty | Plain }` enum is the sole owner of the terminal:
//! the **Tty** variant folds the former `repl::TermGuard` (raw-mode lifecycle)
//! and owns the persistent stdin reader — the one thread that, for the process
//! lifetime, calls `event::poll`/`event::read` — plus the SIGCONT handler that
//! re-enters raw mode after a suspend. **Plain** (piped input, `--quiet`) has
//! no reader and no raw mode; its channels are closed at construction, the
//! shape piped mode had when it simply dropped the sender.
//!
//! Constructed first at setup (in `main`), before `Content`: it probes
//! the caps once at construction (they stay **inside** this layer — strip
//! and frame-drop decisions only) and owns the size watch —
//! one channel per instance. Its receivers feed
//! content (format width) and the render manager (frame dims) through
//! [`TermIO::subscribe_size`]; those who publish ([`TermIO::set_size`],
//! [`TermIO::probe_size`], the reader) write through the same instance, reached
//! via the render manager's front door once `Draw` owns it.
//!
//! Setup hands out [`TermHandles`] exactly once (`TermIO::handles`, called
//! through the manager once `Draw` holds it): the `input` channel to the
//! repl, plus fresh `size` / `caret` receivers.
//!
//! DSR queries are answered inside `TermIO` (the sole stdin owner). Every
//! production byte — frames, the re-anchor, the content choreography — leaves
//! through [`TermIO::write`].

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
// [`TermIO::probe_size`].

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
/// this channel. The window pump consumes this channel too — diverted at the
/// reader while a window is open ([`divert_window`]) — so nothing above the
/// edge ever reads stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// A key press. Still a `crossterm` type "for now" (design doc §TermIO):
    /// the prompt's dispatcher speaks it directly.
    Key(KeyEvent),
    /// Bracketed paste payload — a bulk insert for the prompt.
    Paste(String),
    /// A settled resize: the reader forwards what it also publishes to the
    /// size watch (both edges, design §TermIO), and the window pump parks one
    /// for the same arms to drain.
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
/// the channel in `poll_caret` and re-anchors via DSR (design §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaretEvent {
    Invalidated,
}

/// Acquired once at setup, then distributed (design doc §3): the reader's
/// channel to the repl, `caret` to the render manager — its only consumer,
/// taking the receiver in `Interactive::new`'s hand-out — and `size`,
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

/// The **kind** of an output write, shared by Content → Render → Draw → TermIO
/// (design doc §Render layer). Content says *what* a write is; the sink — the
/// primary stream or meta (stderr) — is TermIO's decision from caps (design
/// §TermIO), so nothing above TermIO forks on a sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Primary content: screen content and its choreography. Always the
    /// primary stream, terminal or not (a piped consumer still sees it).
    Content,
    /// Reasoning — the primary stream when it is a terminal, else meta.
    Reasoning,
    /// Error / system text — routed like [`Stream::Reasoning`], named apart so
    /// the two can diverge later.
    Error,
}

impl Stream {
    /// Whether this kind's bytes land on the meta (stderr) sink under `caps`:
    /// only the diverting kinds, and only when the primary is not a terminal.
    /// Content always rides the primary, so a pipe still receives it.
    fn targets_meta(self, caps: Caps) -> bool {
        !matches!(self, Stream::Content) && !caps.primary
    }
}

/// Per-stream terminal capabilities, probed once per process.
///
/// `TermIO` picks the *input / lifecycle* side
/// (`Tty`/`Plain`, from stdin); caps adjust output fidelity independently.
/// `prior "q" | cat` is the case that splits them: a tty stdin with a
/// non-tty primary — frames dropped and output stripped for the stream, not
/// for the process (see [`TermIO::write`] and [`TermIO::paints_frames`]).
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
/// travels *both* edges (design §TermIO: "resize → both the
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
/// render manager — the whole resume signal is this one flag plus that event.
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
/// Wiring (design doc §3): build `TermIO` first (`main`), take a
/// size-watch receiver for `Content::new`, move the enum itself
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
    /// `Draw`, which owns this enum.
    guard: Arc<TermGuard>,
    /// Did this instance's [`TermIO::suspend`] raise the pause gate? Only
    /// that raise is released by the matching [`TermIO::resume`] — an outer
    /// holder's pause (the compaction window) outlives a nested
    /// suspend/resume pair.
    pause_owned: AtomicBool,
    /// The per-instance size watch's sending half: the reader publishes
    /// resizes here, and [`TermIO::set_size`] / [`TermIO::probe_size`] write
    /// through it. Receivers clone off it ([`TermIO::subscribe_size`]).
    size_tx: watch::Sender<TermSize>,
    /// Probed once at construction ([`Caps::probe`]) — edge-internal: the
    /// strip and frame-drop decisions read them here, and the probe site
    /// lives here so nothing above has to ask.
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
    /// Whether this backend stands in for a **real** output stream, and so
    /// applies the design's degradation (§TermIO): a non-terminal stream is
    /// stripped at the edge, and a non-terminal primary loses its frames.
    /// [`TermIO::plain`] — the production non-REPL backend — and
    /// [`TermIO::recording_with`] (a real backend with pinned caps) say yes;
    /// only the [`TermIO::null`] sinkhole says no, because it observes nothing.
    degrade: bool,
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
        let caps = Caps::probe();
        let (handles, input_tx, caret_tx, size_tx) = term_channels();
        let guard = Arc::new(TermGuard::new(caps.primary));
        // The panic hook's handle: `main` cannot reach the guard (it lives
        // under `Draw`, deep inside the agent config), and the `Drop` backstop
        // never fires at process exit — the reader thread holds an `Arc`
        // clone for the process lifetime. Registration replaces any previous
        // entry: exactly one Tty backend is live per process.
        *EMERGENCY.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard.clone());
        TermIO::Tty(TtyIo {
            guard,
            pause_owned: AtomicBool::new(false),
            size_tx,
            caps,
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
            degrade: true,
            handles: Some(handles),
        })
    }

    /// [`Self::recording`] with the **screen** caps (both streams terminals):
    /// the byte-edge suite's double, so it sees the composed screen — no strip,
    /// frames painted.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn recording() -> Self {
        Self::recording_with(Caps {
            primary: true,
            meta: true,
        })
    }

    /// A memory writer standing in for a **real** backend with `caps` pinned:
    /// it degrades exactly as [`Self::plain`] would — a non-terminal stream is
    /// stripped, a non-terminal primary drops frames — but records the result
    /// so a test can assert the degradation. The caps are the caller's, so the
    /// test does not inherit the runner's terminal. Screen caps make it the
    /// byte-edge double ([`Self::recording`]); pipe caps exercise degradation.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn recording_with(caps: Caps) -> Self {
        let (handles, input_tx, caret_tx, size_tx) = term_channels();
        drop(input_tx);
        drop(caret_tx);
        TermIO::Plain(PlainIo {
            sink: Sink::Recording {
                main: Vec::new(),
                meta: Vec::new(),
            },
            size_tx,
            caps,
            degrade: true,
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
            degrade: false,
            handles: Some(handles),
        })
    }

    /// The caps probed at construction — **edge-internal**: strip and
    /// frame-drop decisions read them here, and nothing above `TermIO` may
    /// (design §TermIO: fidelity is the edge's alone; content always renders
    /// styled and the kind → sink map is this layer's).
    fn caps(&self) -> Caps {
        match self {
            TermIO::Tty(t) => t.caps,
            TermIO::Plain(p) => p.caps,
        }
    }

    /// The design's `suspend()` (§TermIO: "stops the reader and drops raw
    /// mode"): park the sole stdin owner — raise the
    /// pause gate, waiting out its last poll — then drop raw mode, the
    /// order `$EDITOR`'s hand-off used to run by hand before folding into
    /// here. Idempotent. Plain has no tty to hand over, so it reports
    /// `Unsupported` — the answer the editor gate's caller-side `is_tty`
    /// check used to give, now asked once at the edge.
    pub fn suspend(&self) -> io::Result<()> {
        match self {
            TermIO::Tty(t) => {
                if raise_pause_gate() {
                    t.pause_owned.store(true, Ordering::SeqCst);
                }
                t.guard.stop()
            }
            TermIO::Plain(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no interactive terminal to suspend",
            )),
        }
    }

    /// One line of non-REPL input — the piped/batch driver's read, the
    /// `else` arm of the interactive loop (design §Content). Plain reads
    /// stdin through the edge, so nothing outside this layer touches the
    /// fd; `Tty` reports `Unsupported` — its stdin belongs to the sole
    /// reader and the REPL reads keys through the channel, not lines.
    pub fn read_line(&self) -> io::Result<String> {
        match self {
            TermIO::Plain(_) => {
                let mut line = String::new();
                io::stdin().lock().read_line(&mut line)?;
                Ok(line)
            }
            TermIO::Tty(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "line input is the piped/batch driver's read",
            )),
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
    /// prompt loop's live query: one live read
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

    // `Draw` owns the enum; [`Self::suspend`] / [`Self::resume`] /
    // [`Self::is_tty`] are the front door the manager forwards to.

    /// Bytes of kind `s` to their sink (design doc §TermIO `write`). Every byte
    /// `Draw` writes comes through here; the byte-edge tests read what
    /// [`recording`](Self::recording) holds at the same edge, and
    /// [`null`](Self::null) discards. The kind → sink map is here alone
    /// ([`Stream::targets_meta`]): `Content` to the primary, `Reasoning` /
    /// `Error` to the primary when it is a terminal else meta.
    ///
    /// **The degradation edge** (design §TermIO): on a *real* stream that is
    /// not a terminal, every CSI / OSC-8 escape is stripped — zero-width, so
    /// the geometry decided above is untouched and a pipe consumer sees only
    /// text. Frames never reach here in that case (they are dropped at
    /// [`Draw`](crate::draw::Draw); see [`Self::paints_frames`]), so what is
    /// stripped is content. The `recording`/`null` doubles do not degrade.
    pub fn write(&mut self, s: Stream, bytes: &[u8]) -> io::Result<()> {
        let stripped;
        let bytes = if self.degrades() && !self.stream_is_terminal(s) {
            stripped = crate::ansi::strip_ansi_bytes(bytes);
            stripped.as_slice()
        } else {
            bytes
        };
        let meta = s.targets_meta(self.caps());
        match self {
            TermIO::Tty(_) => write_stdio(meta, bytes),
            TermIO::Plain(p) => match &mut p.sink {
                Sink::Stdio => write_stdio(meta, bytes),
                #[cfg(any(test, feature = "test-helpers"))]
                Sink::Recording {
                    main,
                    meta: meta_buf,
                } => {
                    if meta {
                        meta_buf.extend_from_slice(bytes);
                    } else {
                        main.extend_from_slice(bytes);
                    }
                    Ok(())
                }
                Sink::Null => Ok(()),
            },
        }
    }

    /// Frame bytes — the frame-side half of the kind vocabulary (design §Draw):
    /// the consume walk, the frame planner, the caret show/hide, the re-anchor.
    /// The primary sink, and **dropped wholesale** on a real stream that is not
    /// a terminal (see [`Self::paints_frames`]): frames are screen choreography
    /// a pipe consumer cannot use, and their row text would otherwise leak.
    /// Content bytes never come through here — they keep their kind and go via
    /// [`Self::write`], so the drop leaves them intact.
    pub fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        if !self.paints_frames() {
            return Ok(());
        }
        self.write(Stream::Content, bytes)
    }

    /// Whether frames belong on this output (design §TermIO: "non-tty
    /// primaries additionally drop frames wholesale at Draw"). A real stream
    /// that is not a terminal gets none — they are screen choreography a pipe
    /// consumer cannot use, and their row text would otherwise leak. A
    /// recording / sinkhole double always paints: it observes the composed
    /// bytes, not the degradation.
    pub fn paints_frames(&self) -> bool {
        if !self.degrades() {
            return true;
        }
        self.caps().primary
    }

    /// Whether this backend stands in for a real output stream (see
    /// `PlainIo::degrade`). `Tty` and [`Self::plain`] always do.
    fn degrades(&self) -> bool {
        match self {
            TermIO::Tty(_) => true,
            TermIO::Plain(p) => p.degrade,
        }
    }

    /// Whether `s` is a terminal on this backend — the per-stream half of the
    /// degradation decision.
    fn stream_is_terminal(&self, s: Stream) -> bool {
        let caps = self.caps();
        if s.targets_meta(caps) {
            caps.meta
        } else {
            caps.primary
        }
    }

    /// What [`recording`](Self::recording) captured for `s` — empty for any
    /// other backend (they store nothing).
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn recorded(&self, s: Stream) -> Vec<u8> {
        match self {
            TermIO::Tty(_) => Vec::new(),
            TermIO::Plain(p) => match &p.sink {
                Sink::Recording { main, meta } => {
                    if s.targets_meta(self.caps()) {
                        meta.clone()
                    } else {
                        main.clone()
                    }
                }
                _ => Vec::new(),
            },
        }
    }
}

/// Production sink: the real streams, flushed per write (a frame must be on
/// screen before the next one is planned; there is no batching layer below
/// this yet).
fn write_stdio(meta: bool, bytes: &[u8]) -> io::Result<()> {
    if meta {
        let mut err = io::stderr();
        err.write_all(bytes)?;
        err.flush()
    } else {
        let mut out = io::stdout();
        out.write_all(bytes)?;
        out.flush()
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
/// removes the three-way race between the prompt's reads, the turn's
/// consumption, and the DSR query — the last of which this thread answers
/// itself between polls (see [`TermIO::query_cursor_row`]), so no second
/// reader ever exists to swallow a reply.
///
/// In raw mode `event::read` is blocking, so a background thread is what sees
/// `Ctrl-C`/`Ctrl-Z` arriving while the main task is parked on a turn. `Esc`
/// is forwarded, not handled here: U4.3 made it menu-first, a decision this
/// thread cannot make without a session to look at; see [`route_event`].
///
/// `guard` is `Arc` and `stop()` is process-wide, so calling it from this
/// thread is safe. The process-wide pause gate ([`PAUSE_GATE`]) is raised
/// by the main task whenever *it* owns stdin — `$EDITOR`'s takeover or the
/// compaction window — and while it is `true` this reader backs off (sleeps,
/// does not read) so it cannot swallow the bytes the main task is waiting
/// for; [`READER_PARKED`] signals that it has genuinely parked. DSR queries
/// need no pause: this thread answers them itself between polls.
pub fn spawn_stdin_reader(
    guard: Arc<TermGuard>,
    input: UnboundedSender<InputEvent>,
    caret: UnboundedSender<CaretEvent>,
    size: watch::Sender<TermSize>,
) {
    // Inbox first, then the running flag: a query that sees the flag must
    // find the inbox (they are paired — the flag *is* "requests are
    // serviceable here").
    let (dsr_tx, dsr_rx) = std::sync::mpsc::channel::<DsrRequest>();
    *DSR_INBOX.lock().unwrap_or_else(|e| e.into_inner()) = Some(dsr_tx);
    // A gate-raiser only waits for a park when a reader exists to park; set it
    // before the thread starts so the first pause sees it.
    READER_RUNNING.store(true, Ordering::SeqCst);
    let _ = thread::spawn(move || loop {
        if RESUMED.swap(false, Ordering::SeqCst) {
            let _ = guard.start();
            // Resume invalidated the screen (design doc §9): push it toward
            // the render manager, the channel's one consumer (it drains the
            // event in `poll_caret` and re-anchors via DSR).
            // Sent only after `guard.start()`, so the invalidation cannot
            // reach the manager before raw mode is back — and before any
            // later key this thread forwards, so the loop cannot wake on a
            // post-resume key with the event still queued.
            let _ = caret.send(CaretEvent::Invalidated);
        }
        // The main task owns stdin right now ($EDITOR or the compaction
        // window); back off so we don't consume the bytes it is waiting
        // for. `READER_PARKED` tells a gate-raiser that we have left any
        // in-flight poll and are genuinely not reading.
        if PAUSE_GATE.load(Ordering::SeqCst) {
            READER_PARKED.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        READER_PARKED.store(false, Ordering::SeqCst);
        // Service pending DSR requests: the sole stdin owner answers its own
        // queries inline, between polls — no pause, no second reader. A
        // request past its TTL is dropped unasked: the caller stopped
        // waiting ([`TermIO::query_cursor_row`]'s budget), and answering
        // now would only monopolize stdin for nobody.
        while let Ok(req) = dsr_rx.try_recv() {
            if req.created.elapsed() < DSR_REQUEST_TTL {
                let row = query_real_cursor_row();
                let _ = req.reply.send(row);
            }
        }
        if let Ok(true) = event::poll(Duration::from_millis(50)) {
            if let Ok(ev) = event::read() {
                // Resize: *both* edges (design §TermIO). The
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
                // prompt loop or the turn — or, while a window is open, to
                // the window's divert ([`send_input`]). `send` on an unbounded
                // tokio mpsc never blocks, so this is safe from a plain OS
                // thread — and being non-blocking is the whole reason the
                // channel is unbounded (see `route_event`).
                if route_event(&ev) == ReaderRoute::Forward {
                    if let Some(fwd) = InputEvent::from_event(ev) {
                        send_input(&input, fwd);
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

/// Owns terminal mode transitions. [`TermIO::Tty`] holds the canonical
/// instance and keeps the only `Arc` clones: its own and the reader's
/// (resume / `Ctrl-C` / `Ctrl-Z`). The prompt loop, `$EDITOR` and the
/// back-off pump flip raw mode through the render manager's suspend/resume
/// front door, which reaches this enum under `Draw` — no holder outside
/// `TermIO` names the guard. [`start`](Self::start) enters raw mode and
/// negotiates capabilities; [`stop`](Self::stop) (idempotent) restores cooked
/// mode. Safe to call from `Drop`, a panic hook, or a signal handler.
///
/// The design gives the whole lifecycle to `TermIO`: suspend/resume includes
/// the reader-stop, Ctrl-C/Ctrl-Z are handled in the reader, and DSR is
/// answered inside `TermIO`.
pub struct TermGuard {
    active: AtomicBool,
    saved_paste: AtomicBool,
    /// Whether the primary output is a terminal — the guard's escape
    /// sequences (bracketed paste, kitty keyboard, cursor) are the primary
    /// stream's, so on a real non-terminal primary they are **not emitted at
    /// all** (design §TermIO: the frame drop's rule, applied to this writer).
    /// Raw-mode flipping still runs — it is a stdin concern, and
    /// `prior > out.txt` over an interactive stdin still wants it.
    primary: bool,
}

/// The panic hook's handle on the live [`TermGuard`], registered by
/// [`TermIO::tty`]. The hook cannot reach the guard through the config (it
/// lives under `Draw`), and the `Drop` backstop never fires at process exit —
/// the persistent stdin reader holds an `Arc` clone for the process
/// lifetime — so this static is the one way a panic can restore cooked mode.
/// Replaced (not appended) on each `tty()`; one Tty backend per process.
static EMERGENCY: std::sync::Mutex<Option<Arc<TermGuard>>> = std::sync::Mutex::new(None);

/// Restore cooked mode from the panic hook (or anywhere that cannot reach
/// the live `TermIO`): stop the registered guard, idempotently. A no-op
/// when no Tty backend was ever built. The mutex guard tolerates poisoning —
/// a panic while registering must not leave the hook unable to restore the
/// terminal.
pub fn emergency_stop() {
    let guard = EMERGENCY.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(g) = guard {
        let _ = g.stop();
    }
}

impl TermGuard {
    pub fn new(primary: bool) -> Self {
        Self {
            active: AtomicBool::new(false),
            saved_paste: AtomicBool::new(false),
            primary,
        }
    }

    /// Enter raw mode and negotiate capabilities. Idempotent. The capability
    /// escapes are emitted only when the primary is a terminal; raw mode
    /// itself is unconditional (a stdin concern).
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
        if self.primary {
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
        }
        self.active.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Restore cooked mode. Idempotent; safe to call multiple times / from `Drop`.
    pub fn stop(&self) -> io::Result<()> {
        if !self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        // The stream escapes are the primary's: skipped on a non-terminal
        // primary so nothing leaks into the redirected file.
        if self.primary {
            // Pop one level of Kitty keyboard enhancement (matches the push).
            let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
            if self.saved_paste.load(Ordering::SeqCst) {
                Self::csi("?2004h")?;
            } else {
                Self::csi("?2004l")?;
            }
            // The status line hides the cursor while a turn streams, and the
            // prompt's frame apply shows it again when the prompt returns. If the turn is
            // interrupted instead — a tool error, a confirm, a panic — nothing shows
            // it again, and an invisible cursor follows the user into their shell.
            // Unconditional and idempotent: showing a cursor that was never hidden
            // costs nothing, and this is the last writer before the tty is the
            // user's again.
            io::stdout().write_all(crate::ansi::SHOW_CURSOR)?;
        }
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
    /// A guard that emits its capability escapes — the interactive default.
    /// [`TermIO::tty`] builds the real one from the probed caps.
    fn default() -> Self {
        Self::new(true)
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

// --- stdin ownership: the pause handshake -----------------------------------
//
// The persistent reader (the sole stdin owner) must stand down whenever the
// *main* task reads stdin itself: `$EDITOR`'s keystrokes or the compaction
// window's key pump. DSR replies need no handshake — the reader answers its
// own queries inline between polls. There is exactly one reader and one gate,
// so the gate is process-wide — this is a mutual-exclusion flag, not
// per-instance configuration like the size watch.

/// Raised while the main task owns stdin; the persistent reader backs off
/// (does not call `event::poll`/`read`) for as long as it is `true`.
pub static PAUSE_GATE: AtomicBool = AtomicBool::new(false);

/// Set by the reader while it is backed off, cleared the moment it is about to
/// read stdin again. [`raise_pause_gate`] waits on this to know the reader
/// has left any in-flight 50 ms poll — so the bytes it is about to read
/// cannot be swallowed.
pub static READER_PARKED: AtomicBool = AtomicBool::new(false);

/// Set once the reader thread has started. A gate-raiser only waits on
/// [`READER_PARKED`] when a reader exists to park — in a test harness with no
/// reader there is nothing to wait for, and the wait would be a wasted timeout.
pub static READER_RUNNING: AtomicBool = AtomicBool::new(false);

/// The open window's inbox (generation, sender), registered by
/// [`divert_window`]. While set, the reader diverts forwarded events here
/// instead of the REPL channel — the window pump's source (design §TermIO:
/// the reader stays the sole stdin owner; the window consumes its *output*,
/// not stdin). Process-wide because the reader is: one reader, one divert.
static WINDOW_DIVERT: std::sync::Mutex<Option<(u64, UnboundedSender<InputEvent>)>> =
    std::sync::Mutex::new(None);

/// Next [`WindowDivert`] generation — pairs with [`WINDOW_DIVERT`] so a
/// dropped guard clears only its own registration, never a newer window's.
static WINDOW_DIVERT_GEN: AtomicU64 = AtomicU64::new(0);

/// A registered window divert: while alive, forwarded events reach
/// [`divert_window`]'s receiver instead of the REPL channel. On drop the
/// registration clears (only if no newer window re-registered) — after which
/// the reader sends to the REPL channel again. Buffered events a send raced
/// in before the clear stay on the window side; the pump drains them
/// ([`Drop`](crate) parks leftovers for `read_input`).
pub struct WindowDivert(u64);

impl Drop for WindowDivert {
    fn drop(&mut self) {
        let mut slot = WINDOW_DIVERT.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().map(|(gen, _)| *gen) == Some(self.0) {
            *slot = None;
        }
    }
}

/// Open a window divert: returns the receiver the window pump selects on and
/// the guard that keeps the diversion registered. While the guard lives the
/// reader sends every forwarded event ([`route_event::Forward`]) here;
/// without a reader (Plain, a recording double, the pre-spawn entry window)
/// nothing ever feeds the receiver — the window is inert, exactly the old
/// `window_active() == false` shape, and stdin is untouched either way.
pub fn divert_window() -> (
    tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    WindowDivert,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let gen = WINDOW_DIVERT_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    *WINDOW_DIVERT.lock().unwrap_or_else(|e| e.into_inner()) = Some((gen, tx));
    (rx, WindowDivert(gen))
}

/// Deliver one event: to the open window's divert if there is one, else to
/// the REPL channel. The send runs *under the registration lock* — an
/// unbounded send never blocks, and holding the lock across it is what makes
/// window teardown race-free: the guard's drop takes the same lock, so it
/// either precedes this send (slot empty → channel) or follows it (event
/// already buffered → the window's `Drop` drains and parks it).
fn send_input(input: &UnboundedSender<InputEvent>, ev: InputEvent) {
    let slot = WINDOW_DIVERT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, tx)) = slot.as_ref() {
        // Cloned so the fallback below still holds `ev` if the send fails
        // (receiver died mid-registration — shouldn't happen, the guard
        // clears the slot first, but losing an event on a shouldn't is not
        // an option).
        if tx.send(ev.clone()).is_ok() {
            return;
        }
    }
    drop(slot);
    let _ = input.send(ev);
}

/// Raise the pause gate and wait the reader out; `true` when *this* call
/// raised it — the raiser owns lowering it ([`TermIO::suspend`] remembers
/// its own across the editor window and lowers it in [`TermIO::resume`]).
/// Nesting-safe: a call on an already-up gate neither waits nor claims.
/// The pause's only production raiser is `$EDITOR`'s takeover; the
/// compaction window no longer pauses (it consumes the reader's
/// [`divert_window`] instead — the reader never stands down for it).
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

/// How long a DSR request may sit before the reader must not service it
/// anymore: the caller's [`Self::query_cursor_row`] gives up by then, so a
/// late round-trip would only monopolize stdin for nobody.
const DSR_REQUEST_TTL: Duration = Duration::from_millis(150);

/// The caller's wait budget — one reader poll (50 ms) plus one scan
/// (60 ms) plus slack. Generous over [`DSR_REQUEST_TTL`] so an expired
/// request is always discarded before or as the caller stops caring.
const DSR_REPLY_WAIT: Duration = Duration::from_millis(220);

/// A pending DSR request from the main task. The reader services it inline
/// between polls (design §TermIO: the sole stdin owner answers its own
/// queries), discarding it if [`DSR_REQUEST_TTL`] has run out — the caller
/// has stopped waiting by then.
struct DsrRequest {
    created: Instant,
    reply: std::sync::mpsc::Sender<Option<usize>>,
}

/// The reader thread's DSR inbox, registered by [`spawn_stdin_reader`].
/// Process-wide because the reader is: one reader, one inbox.
static DSR_INBOX: std::sync::Mutex<Option<std::sync::mpsc::Sender<DsrRequest>>> =
    std::sync::Mutex::new(None);

impl TermIO {
    /// The cursor row from a DSR query — edge-internal (design §TermIO):
    /// the answer syncs the draw cache and never travels beyond it.
    ///
    /// Two shapes, and which one runs is the sole-ownership invariant:
    ///
    /// - **Reader running (the normal interactive case):** a request into
    ///   the reader's [`DSR_INBOX`]; the reader writes the query and reads
    ///   its own reply between polls, so no second stdin reader exists to
    ///   swallow the answer. The main task just waits on the channel.
    /// - **No reader** — Plain, a recording double, a test state, or the
    ///   entry window before [`Self::handles`] spawns the thread: Plain and
    ///   the doubles answer `None` (no terminal behind this edge, and a
    ///   pipe must never receive a stray `ESC[6n`); Tty scans directly, the
    ///   main task being the one and only stdin reader until the spawn.
    ///
    /// A gated reader (an `$EDITOR` takeover or a compaction window owns
    /// stdin) does not service requests — it cannot read them — so the
    /// caller times out to `None`, the same "no new information" a silent
    /// terminal produces.
    pub(crate) fn query_cursor_row(&self) -> Option<usize> {
        match self {
            TermIO::Plain(_) => None,
            TermIO::Tty(_) => {
                if !READER_RUNNING.load(Ordering::SeqCst) {
                    return query_real_cursor_row();
                }
                let (tx, rx) = std::sync::mpsc::channel();
                let queued = DSR_INBOX
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .and_then(|inbox| {
                        inbox
                            .send(DsrRequest {
                                created: Instant::now(),
                                reply: tx,
                            })
                            .ok()
                    })
                    .is_some();
                if !queued {
                    return None;
                }
                rx.recv_timeout(DSR_REPLY_WAIT).ok().flatten()
            }
        }
    }
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
/// (bypassing crossterm's event parser). The caller must be the sole stdin
/// reader for the duration: the reader thread itself (between polls — no
/// pause exists or is needed), or the main task in the pre-spawn entry
/// window, when no reader exists yet.
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

    /// The reader-stop half of `TermIO::suspend`: suspend parks
    /// the reader before dropping raw mode, a repeated suspend keeps its
    /// claim, and the matching resume releases exactly that raise.
    ///
    /// Gated on a non-terminal stdin because `resume()` enters raw mode:
    /// flipping the harness's real tty mid-suite (and spraying CSI at the
    /// log) is what a unit test not touching the terminal means — the same
    /// escape hatch the bwrap regression uses.
    #[test]
    fn termio_suspend_parks_the_reader_and_resume_releases_its_pause() {
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
    }

    /// The window divert routes forwarded events to the registered
    /// receiver while the guard lives, and back to the REPL channel once it
    /// drops — the window pump's whole source contract, with the reader
    /// never leaving the read path.
    #[test]
    fn window_divert_routes_then_releases() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut win_rx, guard) = divert_window();
        send_input(
            &tx,
            InputEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(
            rx.try_recv().is_err(),
            "the REPL channel must not see a diverted event"
        );
        let ev = win_rx
            .blocking_recv()
            .expect("the window receiver gets the diverted event");
        assert_eq!(
            ev,
            InputEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        );

        drop(guard);
        send_input(
            &tx,
            InputEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        );
        assert!(
            matches!(rx.try_recv(), Ok(InputEvent::Key(k)) if k.code == KeyCode::Char('x')),
            "after the guard drops, events flow to the REPL channel again"
        );
    }

    /// A newer window's registration survives an older guard's drop — the
    /// generation check: a dropped guard clears only its own slot, never a
    /// newer window's.
    #[test]
    fn window_divert_generations_clear_only_their_own() {
        let (rx1, guard1) = divert_window();
        let (rx2, guard2) = divert_window();
        drop(guard1);
        assert!(
            WINDOW_DIVERT.lock().unwrap().is_some(),
            "an older guard's drop must not clear the newer registration"
        );
        drop(guard2);
        assert!(
            WINDOW_DIVERT.lock().unwrap().is_none(),
            "the newer guard's drop clears the slot"
        );
        let _ = (rx1, rx2);
    }

    // --- the routing policy ------

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

    /// Resize travels *both* edges (design §TermIO): the watch
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

    // --- the TermIO type itself ----------------------------------------

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

    /// A recording double captures bytes per sink and `null()` discards them —
    /// the two test constructors the byte-edge tests record through (design doc
    /// §Testing). Under a non-terminal primary the diverting kinds land in the
    /// meta bucket, so the kind → sink map is visible across the two buckets.
    #[test]
    fn recording_captures_per_sink_and_null_discards() {
        let mut rec = TermIO::recording_with(Caps {
            primary: false,
            meta: true,
        });
        rec.write(Stream::Content, b"frame").unwrap();
        rec.write(Stream::Reasoning, b"reason").unwrap();
        rec.write(Stream::Content, b" more").unwrap();
        assert_eq!(rec.recorded(Stream::Content), b"frame more");
        assert_eq!(rec.recorded(Stream::Reasoning), b"reason");

        // A screen double (tty caps) keeps everything on the primary.
        let mut screen = TermIO::recording();
        screen.write(Stream::Reasoning, b"on screen").unwrap();
        assert_eq!(screen.recorded(Stream::Content), b"on screen");

        let mut sink = TermIO::null();
        sink.write(Stream::Content, b"gone").unwrap();
        assert_eq!(sink.recorded(Stream::Content), b"");
    }

    /// The degradation edge (design §TermIO): a *real* non-terminal stream is
    /// stripped per target — the primary when stdout is a pipe, meta when
    /// stderr is, each independently. Content rides the primary; Reasoning
    /// diverts to meta (the primary is not a terminal).
    #[test]
    fn the_edge_strips_a_non_terminal_stream_and_keeps_a_terminal_one() {
        let mut io = TermIO::recording_with(Caps {
            primary: false,
            meta: true,
        });
        io.write(Stream::Content, b"\x1b[32mgreen\x1b[0m\x1b[K\n")
            .unwrap();
        io.write(Stream::Reasoning, b"\x1b[2mdim\x1b[0m\n").unwrap();
        assert_eq!(
            io.recorded(Stream::Content),
            b"green\n",
            "a non-terminal primary is stripped"
        );
        assert_eq!(
            io.recorded(Stream::Reasoning),
            b"\x1b[2mdim\x1b[0m\n",
            "a terminal meta stream is left as composed"
        );
    }

    /// A recording double on a **terminal** primary observes the composed
    /// bytes — the screen caps make strip and frame-drop inert — so the
    /// byte-edge suite reads raw ANSI at the observation edge regardless of the
    /// runner's terminal.
    #[test]
    fn a_recording_double_on_a_terminal_primary_keeps_composed_bytes() {
        let mut rec = TermIO::recording();
        rec.write(Stream::Content, b"\x1b[32mgreen\x1b[0m\n")
            .unwrap();
        assert_eq!(
            rec.recorded(Stream::Content),
            b"\x1b[32mgreen\x1b[0m\n",
            "raw bytes at the observation edge"
        );
        assert!(rec.paints_frames(), "a terminal primary paints its frames");
    }

    /// The frame half (design §TermIO): a real non-terminal primary drops
    /// frames; a terminal primary keeps them, and the sinkhole paints.
    #[test]
    fn a_non_terminal_primary_drops_frames() {
        assert!(
            !TermIO::recording_with(Caps {
                primary: false,
                meta: false,
            })
            .paints_frames(),
            "a piped primary gets no frames"
        );
        assert!(
            TermIO::recording_with(Caps {
                primary: true,
                meta: false,
            })
            .paints_frames(),
            "a terminal primary keeps them"
        );
        assert!(TermIO::null().paints_frames(), "the sinkhole paints");
    }
}
