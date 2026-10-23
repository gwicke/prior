//! The `Content` handle: the agent-
//! facing owner of the transcript's content. It holds the content boundary
//! state, the width watch and the streaming renderers, formats content at the
//! watched width, and writes through the byte sink — which lives inside the
//! frame hub it holds.
//!
//! It also holds the frame hub: `RenderManager` runs the frame
//! pipeline and owns the `Draw` sink. Every write
//! this layer produces leaves as a `ContentWrite` handed to that hub
//! (`with_content_writer` → `render_content`/`render_batch`) — the sink is
//! never named here. Every hand-off also carries its **kind**:
//! `Content` from the boundary chain, `Reasoning` from the reasoning
//! writes, `Error` from the error path — `TermIO` routes kind → sink, so
//! nothing above it forks on where the bytes land.
//! Of the frame *verbs*, `frame_size` lives here (the watch this handle
//! carries *is* the dims source), `painted_size` and `reanchor` are the
//! manager's, and the test drivers' edge to the apply phase is
//! `RenderManager::draw()` — Draw itself, not a verb around it.

use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use console::style;
use serde_json::Value;
use tokio::sync::watch;

use crate::ansi::{at_line_start, EscState, HoldTrailing, LineClear, StripLeading, WrapLines};
use crate::content;
use crate::content::markdown::{
    answer_theme, command_lang, guess_lang, highlight_code_fg, highlight_label_fg, render_markdown,
};
use crate::content::preview::{get_preview, multi_file_blobs, sed_preview_body, strip_read_lineno};
use crate::content::text::expand_tabs;
use crate::render::ContentWrite;

use crate::settings::{ShowReasoning, ShowReasoningHandle};
use crate::termio::{TermIO, TermSize};
use markdown_stream::{Parser, StreamParser};
use markdown_terminal::{Renderer, Theme};

use super::composition::ContentBoundary;
use super::{boundary_spacer, separator_line, ContentDisplayKind};

/// The agent-facing content owner: the display verbs, the width watch, the
/// streaming renderers and the frame hub, all in one handle the agent and the
/// paint path share.
pub struct Content {
    /// The frame hub: where the frame pipeline *and* the drawing edge live —
    /// allocate → render → apply → remember, with the `Draw` sink under the
    /// manager's roof. `Arc<Mutex<…>>` is the design's handle shape (§Render:
    /// UI and content both hold one). Every content write reaches the screen
    /// as a `ContentWrite` handed to this hub (`render_content`/`render_batch`)
    /// — nothing here touches the sink. Everything that writes locks this
    /// first.
    pub(crate) manager: Arc<Mutex<crate::render::RenderManager>>,
    /// This instance's view of the size watch — the same channel `draw`
    /// holds (it came from the same `TermIO`, seeded at construction), so
    /// the content path's width reads and the frame path's geometry cannot
    /// disagree. In tests, a private seeded one.
    pub(crate) size: watch::Receiver<TermSize>,
    /// Incremental GFM markdown parser for the current response (TTY only).
    pub(crate) parser: Mutex<StreamParser>,
    /// Incremental terminal renderer (TTY only) for the answer.
    pub(crate) renderer: Mutex<Renderer>,
    /// Incremental parser/renderer for *thinking* content, kept separate from
    /// the answer pair so the two have independent state.
    pub(crate) thinking_parser: Mutex<StreamParser>,
    pub(crate) thinking_renderer: Mutex<Renderer>,
    /// Live `show_reasoning` display mode, read on every thinking event.
    pub(crate) show_reasoning: ShowReasoningHandle,
    /// The most recent tool call's `(name, args)`, captured in
    /// `display_tool_call` and read back in `display_tool_result` so the
    /// result can be rendered tool-aware.
    pub(crate) last_tool: Mutex<Option<(String, Value)>>,
    /// The content boundary/separator state: the last declared kind,
    /// its segment-closed flag, and the held blank rows.
    pub(crate) boundary: ContentBoundary,
    /// Whether a turn is in flight — the bar dot's only state, flipped in
    /// `begin_response`/`end_response`. Owned by content (the path that knows
    /// when a response starts/ends); the frame reads it via a shared handle.
    pub(crate) streaming: Arc<AtomicBool>,
    /// The formatting span's tripwire: raised while a write builds
    /// its `ContentWrite` outside the hub lock, cleared when the write ends
    /// (`SpanGuard`). The proof that the unlocked span is safe — one writer
    /// per content, `f` never re-entering — is recorded at
    /// [`Self::with_content_writer`]; this keeps a violation loud, where the
    /// held lock used to make it a self-deadlock. Debug-asserted only.
    building: AtomicBool,
    /// The running agent's token-usage store, shared with the turn that writes
    /// it in place. The bar's context figure and `/stat`'s read
    /// *live* here, so a mid-turn completion's count lands on the next paint
    /// with no push to keep in step. `None` until the agent is built (and again
    /// briefly across a rebuild); the bar then shows no context figure rather
    /// than a stale one.
    ///
    /// `Mutex`, not a bare `Option`, because it is written after construction
    /// through a shared handle ([`Self::set_usage`]) while the bar's element
    /// reads it concurrently.
    pub(crate) usage: Mutex<Option<Arc<Mutex<crate::agent::SessionUsage>>>>,
    /// Whether content is suppressed — the `--quiet` flag, which is a property
    /// of the content, not a backend (design §Content). The bytes are still
    /// built through the boundary/wrap chain, then dropped at the hub
    /// hand-off, so nothing reaches the screen; `display_error` is the one
    /// verb that survives, to stderr.
    quiet: bool,
    /// Where the non-REPL input read ([`Self::get_user_input`]) gets a line —
    /// a `Mutex` so a test can install a script after construction.
    input: Mutex<InputSource>,
    /// The capture buffer (design §Content): `Some` while a capture session is
    /// open. Content taps its own write *before* the hub hand-off, so a
    /// capturing write never reaches `render()` — content bytes only, never
    /// frame bytes.
    captured: Mutex<Option<Vec<u8>>>,
}

/// Where [`Content::get_user_input`] gets a line — the non-REPL (piped/batch)
/// input read, the `else` arm of the interactive loop.
enum InputSource {
    /// Production: the process's stdin, one line per read.
    Stdin,
    /// Quiet: no input to read; the driver answers nothing.
    Empty,
    /// A test's script, shared so the test can watch it drain. Falls back to
    /// `/q` when exhausted, so a script that omits a terminator still stops.
    #[cfg(test)]
    Script(Arc<Mutex<std::collections::VecDeque<String>>>),
}

impl Content {
    /// Build the content half from its per-process inputs: the terminal
    /// backend (constructed first at setup, so *it* owns the size watch this
    /// handle subscribes to). The frame hub is built from the same watch and
    /// takes the `TermIO` under `Draw` — one wiring point, one geometry
    /// source; the renderers start blank at a placeholder width. Caps never
    /// reach this layer: content always renders styled, and `TermIO` strips
    /// per stream at the edge.
    pub fn new(term_io: TermIO, show_reasoning: ShowReasoningHandle) -> Self {
        let size = term_io.subscribe_size();
        Self::new_from(term_io, size, show_reasoning)
    }

    /// The frame hub behind this content: what the paint path composes and
    /// renders through. Out-of-crate drivers (the confirm probe) reach it
    /// here; the in-crate path goes through `Content::manager`.
    pub fn hub(&self) -> Arc<Mutex<crate::render::RenderManager>> {
        self.manager.clone()
    }

    /// Point the bar at a running agent's live usage store, so
    /// its context figure is read at render rather than snapshotted at turn
    /// start. Call on the transitions that change who is answering: agent
    /// build/rebuild.
    pub(crate) fn set_usage(&self, usage: Option<Arc<Mutex<crate::agent::SessionUsage>>>) {
        *self.usage.lock().unwrap() = usage;
    }

    /// The bar's live usage handle, as the element's constructor wants it.
    ///
    /// Cloned out here rather than at every call site so the `Mutex` guarding
    /// it stays this module's business — the bar element never touches the
    /// outer lock, it holds the inner `Arc` and reads the turn's in-place
    /// writes.
    pub(crate) fn usage_handle(&self) -> Option<Arc<Mutex<crate::agent::SessionUsage>>> {
        self.usage.lock().unwrap().clone()
    }

    /// [`Self::new`] with the geometry channel chosen by the caller — the
    /// test seam behind [`Self::with_size`], where a privately seeded watch
    /// replaces the instance's own (the manager and `Draw` read the seeded
    /// one; publishes to `term_io`'s watch go where they always did relative
    /// to a pinned test: nowhere the test reads).
    pub(crate) fn new_from(
        term_io: TermIO,
        size: watch::Receiver<TermSize>,
        show_reasoning: ShowReasoningHandle,
    ) -> Self {
        // The hub is born with its geometry and its budget ratio:
        // `configure` is the ratio's home, and this is the one wiring point.
        let manager = Arc::new(Mutex::new(crate::render::RenderManager::new(
            size.clone(),
            term_io,
        )));
        manager
            .lock()
            .unwrap()
            // Seed the ratio only; membership arrives later. `configure` is the
            // membership verb (design §Render) — the repl calls it on each state
            // transition (`sync_stack`) to install the bar/input/overlay stack the
            // next `render(None)` composes. At construction there is no membership
            // yet, so the stack starts empty and the first transition fills it.
            .configure(crate::render::FRAME_MAX_ROW_RATIO, Vec::new());
        Self {
            manager,
            size,
            parser: Mutex::new(StreamParser::new_gfm_stream()),
            renderer: Mutex::new(Renderer::new(answer_theme(), 80)),
            thinking_parser: Mutex::new(StreamParser::new_gfm_stream()),
            thinking_renderer: Mutex::new(Renderer::new(Theme::dimmed(), 80)),
            show_reasoning,
            last_tool: Mutex::new(None),
            boundary: ContentBoundary::new(),
            streaming: Arc::new(AtomicBool::new(false)),
            building: AtomicBool::new(false),
            usage: Mutex::new(None),
            quiet: false,
            input: Mutex::new(InputSource::Stdin),
            captured: Mutex::new(None),
        }
    }

    /// The `--quiet` content: no output, no input; errors survive and ride
    /// `Stream::Error`'s routing — meta on a pipe, the primary when it is a
    /// terminal (design §Content: quiet is a flag on the content, not a
    /// backend; moving the always-meta decision into `TermIO` is deferred).
    /// Content code runs unchanged — the bytes are built and then dropped at
    /// the hub hand-off, so nothing else reaches the screen.
    ///
    /// Also the content for tests that build a config whose display is never
    /// asserted: [`Self::new`] would reach a real terminal, and there is
    /// nothing to draw.
    pub fn quiet() -> Self {
        let mut content = Self::new(
            TermIO::plain(),
            crate::settings::default_show_reasoning_handle(),
        );
        content.quiet = true;
        *content.input.lock().unwrap() = InputSource::Empty;
        content
    }

    /// A live `Content` on a null backend for tests that drive its verbs
    /// directly and read the result through [`Self::begin_capture`] — no
    /// terminal is touched, and the bytes captured are the content's own.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new(
            TermIO::null(),
            crate::settings::default_show_reasoning_handle(),
        )
    }

    /// Test seam: drive the non-REPL input read from `script` rather than
    /// stdin. The queue is shared, so the test can watch it drain; it falls
    /// back to `/q` when exhausted.
    #[cfg(test)]
    pub(crate) fn set_input_script(&self, script: Arc<Mutex<std::collections::VecDeque<String>>>) {
        *self.input.lock().unwrap() = InputSource::Script(script);
    }

    /// A `Content` on a privately seeded size watch — the test harness's
    /// answer to [`Self::new`]. The geometry becomes per-instance state, so
    /// tests that pin a width need no serialization. The backend underneath is
    /// a sinkhole: these tests pin geometry, not bytes, and the byte-edge
    /// tests record through `TermIO::recording()`.
    #[cfg(test)]
    pub(crate) fn with_size(show_reasoning: ShowReasoningHandle, size: TermSize) -> Self {
        Self::with_size_at(show_reasoning, size).0
    }

    /// [`Self::with_size`] that also hands back the private size watch's
    /// sending half. The watch `render()` composes at is per-instance state
    /// (the manager reads it directly), and `fixed()` drops its sender, so a
    /// test that needs the geometry *under test* to differ from the width it
    /// seeded — both paths composing at one watched size, not a width the
    /// harness hardcoded — keeps this to move it. `set_size` cannot: that
    /// publishes to `term_io`'s watch, which the private one replaced.
    #[cfg(test)]
    pub(crate) fn with_size_at(
        show_reasoning: ShowReasoningHandle,
        size: TermSize,
    ) -> (Self, tokio::sync::watch::Sender<TermSize>) {
        let term_io = TermIO::null();
        let (size_tx, size_rx) = tokio::sync::watch::channel(size);
        let content = Self::new_from(term_io, size_rx, show_reasoning);
        (content, size_tx)
    }

    /// The size frames are composed and applied for (ui_draw_v2 §9 #3) — the
    /// watched geometry, floored at 20 columns / 1 row (`plan_apply`
    /// misbehaves under 20 columns; an uninitialized geometry must not get
    /// there). It is also the out-of-crate driver's answer (the confirm
    /// probe's `dims`), hence `pub`. This handle's receiver is the source, the
    /// same
    /// channel `RenderManager::new` clones at construction, so a paint's
    /// dims and the manager's own view of the geometry cannot disagree.
    pub fn frame_size(&self) -> (usize, usize) {
        (self.width(), self.height())
    }

    /// This instance's width — the retired `output::term_width` free
    /// function's answer, now reached through a handle that has one.
    pub fn width(&self) -> usize {
        self.size.borrow().width.max(20)
    }

    /// This instance's height — symmetric to [`Self::width`] (the retired
    /// `output::term_height`).
    pub fn height(&self) -> usize {
        self.size.borrow().height.max(1)
    }
}

/// Clears [`Content::building`] when a formatting span ends — every exit,
/// the empty-write return included. See `with_content_writer`'s span doc;
/// the flag is a debug tripwire, not a lock.
struct SpanGuard<'a>(&'a AtomicBool);

impl Drop for SpanGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Content {
    /// Write content bytes, then advance the draw cache by the number of
    /// terminal rows they occupy.
    ///
    /// The count is *measured from the bytes*, not predicted from the source:
    /// a message is a prefix plus rendered markdown, a panel row is padded to
    /// the full width, a long line wraps — none of which the caller knows
    /// (doc/row_accounting.md §6, phase B). Everything content-related goes
    /// through here or [`Self::with_content_writer`], so the count cannot be
    /// forgotten; a writer that bypassed both is a bug the frame tests find.
    pub(crate) fn write_content(&self, bytes: &[u8]) {
        self.write_content_kinded(bytes, None);
    }

    /// [`Self::write_content`] that also says what was written, so
    /// `with_content_writer` can record it as the transcript's last kind.
    ///
    /// Most content is not classified — a message, a tool result, streamed
    /// text — and passes `None`, which simply clears the record. The point of
    /// the parameter is that a *caller* declares its own kind; the choke point
    /// never has to guess from the bytes.
    pub(crate) fn write_content_kinded(&self, bytes: &[u8], kind: Option<ContentDisplayKind>) {
        self.write_content_block(bytes, kind, false);
    }

    /// [`Self::write_content_kinded`] for the write that ends a segment — an
    /// answer flush after which a tool panel, or the next segment's text,
    /// arrives. The flag is what lets the next write to declare the *same* kind
    /// still count as an opener: the two blocks are two, and only the writer
    /// knows that.
    pub(crate) fn write_content_closing_segment(
        &self,
        bytes: &[u8],
        kind: Option<ContentDisplayKind>,
    ) {
        self.write_content_block(bytes, kind, true);
    }

    pub(crate) fn write_content_block(
        &self,
        bytes: &[u8],
        kind: Option<ContentDisplayKind>,
        closes: bool,
    ) {
        self.write_content_block_as(bytes, kind, closes, crate::termio::Stream::Content);
    }

    /// [`Self::write_content_block`] with an explicit write kind — the error
    /// path's `Stream::Error`.
    pub(crate) fn write_content_block_as(
        &self,
        bytes: &[u8],
        kind: Option<ContentDisplayKind>,
        closes: bool,
        stream: crate::termio::Stream,
    ) {
        // No wrapping here: `with_content_writer`'s `WrapLines` is the one
        // choke every writer passes — batch strings, streamed renderer
        // output, everything — so a line can no longer reach the terminal
        // wider than the terminal.
        let width = self.width();
        self.with_content_writer_stream(width, kind, closes, stream, |out| {
            let _ = out.write_all(bytes);
        });
    }

    /// Run `f` with this write's bytes collected through the boundary/hold/
    /// wrap/line-clear chain, then hand the result to the hub as a
    /// [`ContentWrite`] (design §Content) — the one way content
    /// reaches the screen. For the writers that produce their own bytes —
    /// the streaming markdown renderers, which want a `Write` to feed.
    ///
    /// `f` gets a `&mut dyn io::Write` (the chain's head), which keeps the
    /// counter's type out of the signature. A caller handing it to
    /// something generic over `W: Write` — `Renderer::feed`,
    /// `commit_thinking` — passes `&mut out`, which is itself a `Write` and
    /// satisfies the `Sized` bound that `&mut dyn Write` cannot. `f` is the
    /// *build* and must stay inside `out`: a `write_all` or a
    /// parser/renderer feed, never a content write and never the hub (the
    /// span below runs unlocked; the tripwire asserts this).
    ///
    /// `width` is the width the content is being rendered for; pass the
    /// renderer's own width so a line that wrapped at render time is counted
    /// at that width.
    ///
    /// `kind` is what this write *is* (see [`ContentDisplayKind`]). It is
    /// recorded as the transcript's last kind once the write has landed —
    /// recorded last so a wrapper that makes several of these calls can leave
    /// the kind it wants standing, rather than having its own first call clear
    /// it. `None` clears the record: "unclassified" is the honest answer for
    /// content that did not declare itself.
    ///
    /// `closes_segment` says this write is the last flush of its segment, so
    /// the *next* write opens a new block even under this same kind — the
    /// second dimension of the boundary rule alongside the pair of kinds. It is
    /// the writer telling the choke point what it is doing; the stream is never
    /// asked, because a row held back has not reached the stream and a question
    /// about a row it cannot see is what the old boundary logic kept getting
    /// wrong.
    // `drop_non_drop`: the `drop()`s on `hold`/`counter`/`clear` below are
    // deliberate borrow-shortening, not disposal — each of those wrappers holds
    // a `&mut` into `buf` that has to be released at an exact point in the
    // sequence, well before the end of the function. Clippy's advice (that the
    // call "only extends its contained lifetimes") describes the opposite of
    // the intent: `drop()` moves the value and ends the borrow, which is the
    // only way to release it early. Scoped here rather than at crate level so
    // an accidental `drop()` elsewhere is still reported.
    pub(crate) fn with_content_writer<T, F>(
        &self,
        width: usize,
        kind: Option<ContentDisplayKind>,
        closes_segment: bool,
        f: F,
    ) -> T
    where
        F: FnOnce(&mut dyn io::Write) -> T,
    {
        self.with_content_writer_stream(
            width,
            kind,
            closes_segment,
            crate::termio::Stream::Content,
            f,
        )
    }

    /// [`Self::with_content_writer`] with an explicit write **kind** (design
    /// §TermIO): the block's `ContentWrite` carries it and `TermIO` routes it
    /// to the sink. Only the error path asks for anything but `Content`.
    #[allow(clippy::drop_non_drop)]
    pub(crate) fn with_content_writer_stream<T, F>(
        &self,
        width: usize,
        kind: Option<ContentDisplayKind>,
        closes_segment: bool,
        stream: crate::termio::Stream,
        f: F,
    ) -> T
    where
        F: FnOnce(&mut dyn io::Write) -> T,
    {
        // The formatting span runs **unlocked** (design §Content:
        // "formats … before each block — no hub round-trip"): boundary, chain
        // and `f` touch content's own state or a private buffer, and the
        // hand-off at the end takes the hub's lock for exactly the one round
        // trip. What the removed lock guaranteed, the proof keeps:
        //
        //   1. *One writer.* Every content write runs inside the one REPL
        //      task — the turn is inline in `prompt()`'s `select!` with no
        //      spawn on the path (confirm.rs' module doc), and prompt-time
        //      verbs (commit echo, menu messages, replay, compaction's
        //      retry line) run between turns on that same task. No two
        //      spans can overlap, `begin_capture`/`end_capture` included —
        //      which is also why the capture read below can be its own
        //      short lock and still stand for the whole span.
        //   2. *`f` does not re-enter.* Every closure is a `write_all` or a
        //      parser/renderer feed into `out` (enumerable at the doc).
        //      The tripwire below is what keeps a future violation loud:
        //      under the held lock it was a self-deadlock; unlocked it
        //      would corrupt the boundary state silently.
        //
        // The tripwire is debug-only by design: release relies on 1 and 2,
        // which are structural, and the suite runs where the tripwire
        // lives.
        debug_assert!(
            !self
                .building
                .swap(true, std::sync::atomic::Ordering::Relaxed),
            "content write re-entered (or a second writer started) while a \
             formatting span was open: the build runs outside the hub lock \
             and only the single-writer + non-reentrancy \
             proof keeps the boundary state correct\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        let _span = SpanGuard(&self.building);
        // The *caller* declares the kind — the choke point never guesses from
        // bytes — and the previous one is read first, because the whole
        // boundary rule is a statement about the pair. Recording before `f`
        // runs is deliberate: an emitter drawing several things inside this one
        // block then reads back its own kind, so its internal separators are
        // (kind, kind) and contribute nothing.
        let (old, closed) = self.boundary.record(kind, closes_segment);
        // A write opens a new block when its kind differs from the last, or
        // when the last one said it closed its segment. Everything the opener
        // does — drawing the boundary row, stripping this block's leading
        // blanks, refusing to release what the last block held — follows from
        // that one answer.
        let opener = old != kind || closed;
        // Blank rows the previous write held past its line terminator. One is
        // a paragraph gap the renderer left where no further content arrived,
        // and at a boundary the row `boundary_spacer` draws is that same row:
        // it is consumed, not stacked. More than one is an emitter asking for
        // separators it is not entitled to, and the backtrace below is how the
        // offender is found — a diagnostic, not a policy decision about what to
        // do with them, which stays "drop".
        let held = self.boundary.take_held();
        // Two distinct findings, both about what the *previous* write left
        // behind, and both no more than a backtrace with which to find the
        // emitter:
        //
        // 1. Trailing separator lines at a kind or segment boundary — the block
        //    that just ended had no further content to put them to work for.
        //    They are dropped (the row at this boundary is the matrix's), but
        //    the emitter that asked for them is named.
        // 2. More than one blank separator line anywhere — one is the most that
        //    can ever be legitimate as held interior spacing, so two means the
        //    content emitted a gap no following row was ever going to explain.
        debug_assert!(
            !(opener && held > 0),
            "content left trailing separator lines at a content boundary: \
             {old:?} -> {kind:?} held {held} row(s)\n{}",
            std::backtrace::Backtrace::force_capture()
        );

        // Content goes to a buffer first, through the line-clear transform
        // (a line clear with each line the write enters — overwrite, one pass,
        // ui_draw_261001). Nothing is handed over until the write is known
        // to *have* bytes, which defers the consume: a write that renders
        // nothing (every empty delta) takes no frame and moves nothing, so the
        // frame stays live and the next compose can still diff it.
        //
        // Captures get the raw bytes — they are assertions against content,
        // not against a screen — appended at the hand-off below, which is why
        // the transforms step aside while one is open. Same-task with writes
        // (proof above), so this one read stands for the span.
        let capturing = self.is_capturing();
        let mut buf: Vec<u8> = Vec::new();
        let mut clear = LineClear {
            inner: &mut buf,
            pending: true,
            enabled: !capturing,
        };
        // The width guard sits between the hold and the line-clear: held
        // newlines reset its column, and its inserted newline reaches the
        // line-clear (armed for the continuation row). See `WrapLines`. No
        // counter rides here any more — the row count is Render's, read back
        // off these bytes at the hand-off — so the clear writes straight to
        // the buffer. The consume's walk, when there is one, still goes
        // straight to the writer, ahead of all of this: it is cursor movement,
        // not content, and never passes through the clear.
        let mut wrap = WrapLines {
            inner: &mut clear,
            width: width.max(1),
            enabled: !capturing,
            col: 0,
            partial: Vec::new(),
            esc: EscState::None,
        };
        // The rows that belong *between* blocks — the matrix's boundary line,
        // and whatever the previous block held back — are pending content, not
        // something drawn on the way in. They go to the same mechanism that
        // holds a block's own trailing newlines, so they materialise only if
        // content follows to sit under them: a write with nothing in it (a
        // segment flush with nothing left, a delta that rendered nothing) draws
        // no row at all, and its rows ride on to the next write, which either
        // releases them before its content or *is* the boundary that wanted one
        // row in the first place.
        //
        // One boundary gets at most one row: an opener that wants a separator
        // absorbs whatever was held rather than stacking on it, and a write
        // that is not a boundary releases what was held as interior spacing.
        let rows_due = if opener {
            usize::from(boundary_spacer(old, closed, kind).is_some())
        } else {
            0
        };
        // Normalized to at most one: a boundary gets one row, and one piece of
        // held interior spacing gets one row. Two held lines can only be
        // content that over-emitted (blamed above), never something to draw.
        let pending = rows_due.max(held).min(1);
        let mut hold = HoldTrailing {
            inner: &mut wrap,
            pending: 0,
            wrote_content: false,
            wrote_any: false,
        };
        // Seeded as pending state rather than written through: a boundary row
        // is not content, and a write that adds nothing of its own must still
        // read as empty afterwards.
        hold.pending = pending;
        let r = if opener {
            // Strip a block's leading blank lines: the boundary line above put
            // exactly one row there, and content that opens with newlines
            // would turn it into two. Stops at the first byte that is not a
            // newline, so everything after it — interior paragraph gaps
            // included — is untouched.
            let mut strip = StripLeading {
                inner: &mut hold,
                armed: false,
            };
            f(&mut strip)
        } else {
            f(&mut hold)
        };
        // Did anything of substance cross? Empty and newline-only writes are
        // both "no": the leading strip has already removed a block's own blank
        // lines, so what is left is either content or nothing.
        let wrote_any = hold.wrote_any;
        let wrote_content = hold.wrote_content;
        let carried = hold.finish();
        drop(hold);
        // Blame where the content is written, not where it lands: a block that
        // ends in two or more blank lines is over, whatever comes next, and the
        // backtrace here names the emitter rather than the innocent boundary
        // that later meets its row.
        debug_assert!(
            !(wrote_content && carried > 1),
            "content left more than one blank separator line: {old:?} -> \
             {kind:?} carried {carried} row(s)\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        // What is stored for the next write. A write that wrote nothing at all
        // restores what it found: the boundary row it seeded is re-derived from
        // a boundary that still stands (its kind was rolled back too), so
        // carrying it would only make it look like the *previous* block's
        // trailing — and get that block blamed for a row it never emitted. Any
        // write that did write carries on, capped at one row: one separator is
        // all a gap ever is, and anything past it was already blamed above.
        let held_next = if wrote_any { carried.min(1) } else { held };
        self.boundary.store_held(held_next);
        if !wrote_content {
            // A write with no content in it changes no boundary: its new kind
            // never happened, so the predecessor still stands and the next
            // write still gets the row that boundary owed it.
            //
            // INVARIANT 1 (content write): wrote_content == true
            //   - last_content_display_kind = kind (new kind stands)
            //   - last_content_closes_segment = closes_segment (new closed stands)
            //
            // INVARIANT 2 (empty write): wrote_content == false
            //   - last_content_display_kind = old (kind unchanged)
            //   - last_content_closes_segment = closed || genuinely_closes
            //     where genuinely_closes = closes_segment && old == kind && !closed
            //     i.e. an empty write only propagates closed=true when it GENUINELY
            //     closes an open segment of the SAME kind (old == kind && !closed).
            //     This prevents an empty turn end (old=None, kind=ResponseText) from
            //     spuriously setting closed=true, which would inject blank lines
            //     before subsequent retry messages.
            self.boundary
                .restore_empty(old, closed, kind, closes_segment);
        }
        drop(clear);
        // Nothing to hand over: no rows moved, nothing consumed, no
        // reposition owed — and the frame stays *live*, so the next compose
        // can still diff it. The kind/hold bookkeeping above already ran: an
        // empty write is still a statement about the boundary.
        if buf.is_empty() {
            return r;
        }
        // Quiet drops the hand-off, not the work: the bytes were built through
        // the chain above — the boundary state advanced with them — and are
        // discarded here, so nothing reaches the screen (design §Content:
        // quiet is a flag on the content, not a backend). Errors are the one
        // kind that survive: they keep going and ride `Stream::Error`'s
        // routing (meta on a pipe, the primary when it is a terminal).
        if self.quiet && stream != crate::termio::Stream::Error {
            return r;
        }
        // A capturing write is Content's to keep (design §Content): append the
        // raw bytes and stop — the hub is never reached, so no frame is
        // composed, consumed or re-emitted for it.
        if capturing {
            self.captured
                .lock()
                .unwrap()
                .as_mut()
                .expect("capturing but no capture buffer open")
                .extend_from_slice(&buf);
            return r;
        }
        // The hand-off (design §Content/§Render): the block's bytes and the
        // stream it rides — everything the screen choreography needs, and
        // nothing about *what* it is. Content never touches the sink from here
        // on; `RenderManager` repairs the bytes, reads the row count off them,
        // consumes the live frame, writes the block, advances the cursor and
        // brings the frame back below.
        let write = ContentWrite {
            bytes: buf,
            // The kind the caller declared: `Content` for the chain (the
            // screen's path), `Reasoning` from the reasoning writes,
            // `Error` for the error path — `TermIO` routes kind → sink.
            stream,
        };
        // One choreography, two design verbs (§Render): the replay path
        // writes inside a batch and hands off as `render_batch`, the
        // streaming path as `render_content` — today one body under both
        // names, the batch flag inside deferring the reposition (ui_draw_v2
        // R5) either way. The guard belongs to the hub; the deferral stays the
        // shared flag's until a final `render()` round trip gives
        // `render_batch` its own end.
        //
        // The one hub round trip: lock, route, release — the
        // serialization the span used to carry whole is exactly this now.
        {
            let mut mgr = self.manager.lock().unwrap();
            if mgr.batching() {
                mgr.render_batch(write);
            } else {
                mgr.render_content(write);
            }
        }
        r
    }

    /// Commit all buffered *thinking* to the (dimmed) thinking renderer and
    /// start a fresh thinking parser.
    ///
    /// Uses `StreamParser::flush()` — **not** `write(b"\n\n")` — so
    /// lookahead-dependent blocks (bullet/numbered lists, tables, fenced code)
    /// are emitted *now*. A blank line cannot close a GFM list (the parser must
    /// see a non-list line or EOF), so `write(b"\n\n")` left such reasoning
    /// stranded in the parser until `end_response`'s `flush()` — which runs
    /// *after* the answer parser, so the reasoning printed *below* the answer.
    /// The thinking *renderer* is kept so post-tool thinking reads as one
    /// continuous faded stream. See `doc/reasoning_order_opus.md` §4.
    pub(crate) fn commit_thinking<W: io::Write>(&self, out: &mut W) {
        let events = {
            let mut tparser = self.thinking_parser.lock().unwrap();
            let events = tparser.flush();
            *tparser = StreamParser::new_gfm_stream();
            events
        };
        let mut trenderer = self.thinking_renderer.lock().unwrap();
        let _ = trenderer.feed(&events, out);
    }

    pub(crate) fn separator(&self) {
        self.write_content(separator_line(self.width()).as_bytes());
    }

    /// Start the renderers fresh, so the next block either of them renders
    /// carries no gap of its own.
    ///
    /// A long-lived `Renderer` remembers that it has written something
    /// (`wrote_any`) and opens its next block with a blank line. Inside one
    /// document that is right; at a boundary the *host* already separated it is
    /// a second blank. `end_response_segment` has always reset the answer
    /// renderer for this reason; the tool boundary needs it for the *thinking*
    /// renderer, which is where the doubled blank came from — a turn with
    /// reasoning before the tool left that renderer mid-document, and its next
    /// block (the post-tool reasoning) arrived with a gap of its own.
    ///
    /// The renderers hold no state the parser does not, so replacing them is
    /// free: the parser keeps the document, the renderer starts a block.
    pub(crate) fn reset_renderer_gaps(&self) {
        let width = self.width();
        let mut renderer = self.renderer.lock().unwrap();
        *renderer = Renderer::new(answer_theme(), width);
        let mut trenderer = self.thinking_renderer.lock().unwrap();
        *trenderer = Renderer::new(Theme::dimmed(), width);
    }

    /// Emit a segment's two flushes — the reasoning the thinking renderer
    /// still holds, and its answer — as the *two* blocks they are, each
    /// normalized to a line boundary with no blank row behind it, and with
    /// whichever is last declaring the segment closed.
    ///
    /// Reasoning first, always: it precedes the answer it was written for, so
    /// the pair reads the way the model produced it, and the boundary between
    /// them is stated as (reasoning, text) — `boundary_spacer`'s row, not a
    /// newline smuggled inside a single mixed-kind block. A half with nothing
    /// in it writes nothing at all; a segment with neither still writes its
    /// closing declaration, empty, so the next write knows a new block starts.
    ///
    /// The closing flag on whichever block is *last* is what makes the next
    /// write open a block: a tool panel arriving next already differs in kind,
    /// but the next segment's text does not, and two text blocks with a fresh
    /// parser and renderer between them are two blocks.
    pub(crate) fn write_segment_tail(&self, width: usize, answer: &[u8], thinking: &[u8]) {
        use ContentDisplayKind::{ResponseReasoning, ResponseText};
        let answer = at_line_start(answer);
        let thinking = at_line_start(thinking);
        match (answer.is_empty(), thinking.is_empty()) {
            (false, true) => {
                self.with_content_writer(width, Some(ResponseText), true, |out| {
                    let _ = out.write_all(&answer);
                });
            }
            (true, false) => {
                self.with_content_writer_stream(
                    width,
                    Some(ResponseReasoning),
                    true,
                    crate::termio::Stream::Reasoning,
                    |out| {
                        let _ = out.write_all(&thinking);
                    },
                );
            }
            (false, false) => {
                self.with_content_writer_stream(
                    width,
                    Some(ResponseReasoning),
                    false,
                    crate::termio::Stream::Reasoning,
                    |out| {
                        let _ = out.write_all(&thinking);
                    },
                );
                self.with_content_writer(width, Some(ResponseText), true, |out| {
                    let _ = out.write_all(&answer);
                });
            }
            (true, true) => {
                self.with_content_writer(width, Some(ResponseText), true, |_out| {});
            }
        }
    }

    /// Close a tool block: declare its last chunk, and start the renderers
    /// fresh for whatever comes next.
    ///
    /// No newline here — a tool block ends at a line boundary and no further.
    /// The knowledge this carries is that the block *ended*, which no panel row
    /// can express: every row of a tool block looks like every other, so two
    /// blocks in a row would otherwise read as one. Declaring the last chunk is
    /// what makes the next write open a block even under this same kind, and so
    /// what puts `boundary_spacer`'s row between two tool panels.
    pub(crate) fn end_tool_block(&self) {
        self.write_content_closing_segment(b"", Some(ContentDisplayKind::ResponseTool));
        self.reset_renderer_gaps();
    }

    /// Emit one dark-grey tool-panel row: [`content::panel::panel_row`] builds
    /// the grey background around `content` (which already carries its own
    /// foreground ANSI), and this writes it with the `ResponseTool` kind.
    pub(crate) fn paint_panel(&self, content: &str, indent: bool) {
        let row = content::panel::panel_row(content, indent, self.width());
        // Hard-coded `ResponseTool`: a panel is a tool block, and tool blocks
        // only ever occur between `begin_response` and `end_response`. Nothing
        // here is ever drawn as anything else, so there is no caller to pass a
        // kind down from — the one place that rule lets a utility decide.
        // Call and result panels share the kind on purpose: the two halves of a
        // pair are one block, and the row between them would be a second copy
        // of the one `boundary_spacer` draws when the *tool* arrives.
        self.write_content_kinded(
            format!("{row}\n").as_bytes(),
            Some(ContentDisplayKind::ResponseTool),
        );
    }

    /// Emit a full-width panel for plain `text`: each wrapped+styled line (see
    /// [`content::panel::tool_row_contents`]) becomes one [`Self::paint_panel`]
    /// row.
    pub(crate) fn tool_panel(&self, text: &str, is_error: bool, indent: bool) {
        let contents = content::panel::tool_row_contents(text, is_error, indent, self.width());
        for content in &contents {
            self.paint_panel(content, indent);
        }
    }

    /// Per-file call arguments for the multi-file `edit` recorded in `last_tool`
    /// and the given result, paired by index, or `None` when this result is not
    /// a multi-file pair.
    ///
    /// The pairing is by index because both halves are produced from the same
    /// file list, in first-seen order, by `edit_files` — the same derivation the
    /// persist and model-facing splits use. A length mismatch means the two
    /// halves disagree, which is not something to guess at, so it degrades to
    /// rendering the result alone.
    pub(crate) fn multi_file_pairs(&self, result: &str) -> Option<Vec<(String, Value, String)>> {
        let files = bsed_blockedit::parse_file_results(result)?;
        let (name, args) = self.last_tool.lock().unwrap().clone()?;
        if !name.eq_ignore_ascii_case("edit") {
            return None;
        }
        let blobs = multi_file_blobs(&args)?;
        (blobs.len() == files.len()).then(|| {
            blobs
                .into_iter()
                .zip(files)
                .map(|(blob, fr)| {
                    let single = match &fr.result {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (name.clone(), blob, single)
                })
                .collect()
        })
    }

    /// The call half of one tool call-result pair: boundary, label, and the
    /// trailing diff/preview lines.
    ///
    /// Exactly one file's batch per invocation — a single-file call invokes this
    /// once, and a multi-file call once per file — so the two cases cannot drift
    /// apart in the view layer.
    pub(crate) fn render_call_panel(&self, name: &str, args: &Value) {
        // Purpose-built label (+ optional trailing lines such as an edit diff)
        // from `get_preview`. One blank line above the panel, unless the stream
        // already ends with one — which is what used to need a "was the last
        // block a tool" flag: a text segment leaves the renderer's own trailing
        // blank behind, a tool panel does not, and the stream knows which.
        let (label, extra) = get_preview(name, args);
        // Dark-grey panel box (no `⏺`, no tree line-art). The command label's
        // first line — tool name and arguments — sits flush at the panel's
        // left edge; a multi-line input's later lines carry the result indent
        // (pre-pended before rendering, so e.g. bash lines align under `$ `).
        // Everything under the label (the trailing diff/preview lines here,
        // the result panel that follows) is output and carries the result
        // indent.
        // The label is syntax-highlighted through the same formatter the
        // result path uses, rendered without the code-fence gutter: that
        // gutter *is* the result indent, and a label is not a result.
        // A label can wrap to several rows (long command); paint each.
        let label_styled = highlight_label_fg(&label, command_lang(name), self.width());
        for row in label_styled.split('\n') {
            self.paint_panel(row, false);
        }
        for line in &extra {
            self.tool_panel(line, false, true);
        }
    }

    /// The result half of one tool call-result pair: everything from the
    /// structured-envelope check to `end_tool_block`.
    ///
    /// Expects the *single-file* result text — a bare success note or the
    /// `{ui_view, model_view}` envelope — which is what one entry of a
    /// multi-file result is, so a per-file batch reaches it unchanged.
    pub(crate) fn render_result_panel(&self, result: &str) {
        // Edit-tool failures are emitted as a structured JSON object with
        // separate `ui_view` (short, no file text) and `model_view` (rich,
        // with diff). The UI must render only the short view so we don't
        // leak file content into scrollback; the full result string is
        // still passed back to the model via the tool-result message.
        if let Ok(v) = serde_json::from_str::<Value>(result) {
            if let (Some(ui), Some(_model)) = (
                v.get("ui_view").and_then(|x| x.as_str()),
                v.get("model_view").and_then(|x| x.as_str()),
            ) {
                self.tool_panel(ui, true, true);
                self.end_tool_block();
                return;
            }
        }

        // A `sed` preview result is a block blob behind a one-line banner.
        // Render the blocks as a diff so the human reviewing the panel reads
        // `-`/`+` lines rather than @@@ markers; the model still gets the
        // verbatim blob in its tool message.
        if let Some(lines) = sed_preview_body(result) {
            // Not an error: a preview is a *proposal*, and colouring the whole
            // body red (as the edit-failure path does) says the opposite. The
            // `- `/`+ ` prefixes carry the red/green, matching an applied edit.
            for line in &lines {
                self.tool_panel(line, false, true);
            }
            self.end_tool_block();
            return;
        }

        // Check for successful edit_file result and render a diff preview
        // Success format: "Replaced N block(s) in <path>"

        let unquoted = serde_json::from_str::<Value>(result)
            .ok()
            .and_then(|v| match v {
                Value::String(s) => Some(s),
                Value::Array(arr) => Some(
                    arr.iter()
                        .map(|v| v.as_str().unwrap_or(&v.to_string()).to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .unwrap_or_else(|| result.to_string());

        let mut cleaned = unquoted.as_str();
        while cleaned.starts_with("Toolset error: ") || cleaned.starts_with("ToolCallError: ") {
            if let Some(stripped) = cleaned.strip_prefix("Toolset error: ") {
                cleaned = stripped;
            } else if let Some(stripped) = cleaned.strip_prefix("ToolCallError: ") {
                cleaned = stripped;
            }
        }
        // Same treatment as the model: drop the redundant arg-error wrappers so
        // the panel shows the problem, the expected block, and the offending
        // structure — not a wall of prefixes. (Display is secondary to the model
        // here, but the same text, so the same cleanup.)
        cleaned = bsed_blockedit::strip_arg_error_prefixes(cleaned);

        let is_error =
            unquoted.starts_with("Toolset error") || unquoted.starts_with("ToolCallError");

        // An `edit` argument error is one self-correcting message with a fixed
        // `Expected:` / `Actual:` shape. Show it as two columns so the correct
        // line and the line the model actually sent are on the same row, and so
        // the block is half as tall (`Actual:` no longer falls below the fold).
        // The *model* still receives the vertical form — see arg_error_view.
        //
        // Painted rather than `tool_panel`ed: these lines are already sized to
        // the terminal and their columns align by construction, and
        // `tool_panel` re-wraps through `wrap()`, which collapses the interior
        // spaces that alignment depends on.
        {
            let width = self.width();
            let avail = width.saturating_sub(content::panel::RESULT_INDENT).max(1);
            if let Some(table) =
                bsed_blockedit::arg_error_view::render_arg_error_table(cleaned, avail)
            {
                for line in &table {
                    self.paint_panel(&style(line.clone()).dim().to_string(), true);
                }
                self.end_tool_block();
                return;
            }
        }

        let lines: Vec<_> = cleaned.lines().collect();

        if lines.is_empty() {
            self.tool_panel("(empty)", false, true);
            self.end_tool_block();
            return;
        }

        // Which tool produced this result (set by `display_tool_call`). Drives
        // read/write-specific rendering and language guessing.
        let tool = self.last_tool.lock().unwrap().clone();
        let is_read = tool
            .as_ref()
            .map(|(n, _)| n.eq_ignore_ascii_case("read") || n.eq_ignore_ascii_case("read_file"))
            .unwrap_or(false);
        let is_write = tool
            .as_ref()
            .map(|(n, _)| n.eq_ignore_ascii_case("write") || n.eq_ignore_ascii_case("write_file"))
            .unwrap_or(false);

        // Handle structured write result: {"path": "...", "content": "...", "lines": N, "bytes": N}
        if is_write {
            if let Ok(v) = serde_json::from_str::<Value>(result) {
                if let (Some(path), Some(content), Some(total_lines)) = (
                    v.get("path").and_then(|x| x.as_str()),
                    v.get("content").and_then(|x| x.as_str()),
                    v.get("lines").and_then(|x| x.as_u64()),
                ) {
                    // Show the path as a header with line count. Like any
                    // command label it sits flush; the content below is the
                    // result, indented by the fence gutter.
                    let lang = guess_lang(path);
                    let width = self.width();
                    let header = format!("write {} [{} lines]", path, total_lines);
                    for row in highlight_label_fg(&header, "bash", width).split('\n') {
                        self.paint_panel(row, false);
                    }

                    // Render the content with syntax highlighting (like
                    // read_file): a whole block in one fenced render, wrapped
                    // by the formatter. Tab-expanded up front so indentation is
                    // measured in the columns the terminal will draw. The
                    // fence gutter *is* the row's indent, so the rows are
                    // painted flush.
                    let show_max = 10; // Limit preview to 10 lines (match read_file)
                    let content_lines: Vec<_> = content.lines().collect();
                    let shown: Vec<String> = content_lines
                        .iter()
                        .take(show_max)
                        .map(|l| expand_tabs(l).into_owned())
                        .collect();
                    let block = shown.join("\n");
                    for row in highlight_code_fg(&block, lang, width).split('\n') {
                        self.paint_panel(row, false);
                    }
                    if content_lines.len() > show_max {
                        self.tool_panel(
                            &format!("... +{} lines", content_lines.len() - show_max),
                            false,
                            true,
                        );
                    }
                    self.end_tool_block();
                    return;
                }
            }
        }

        let read_path = tool
            .as_ref()
            .and_then(|(_, a)| a.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Error output is shown verbatim (red). Everything else is highlighted:
        // `read_file` output is colored by the file's guessed language, bash
        // command output gets token highlighting, and other tools' output is
        // colored uniformly (no meaningful grammar to tokenize).
        let lang = if is_read {
            guess_lang(read_path)
        } else {
            match tool.as_ref().map(|(n, _)| n.as_str()) {
                Some("bash") => "bash",
                _ => "text",
            }
        };

        // Read results drop the `NNNN| ` line-number gutter and show up to 10
        // lines; other results keep the previous 4-line default. Errors show
        // everything.
        let show_max = if is_error {
            usize::MAX
        } else if is_read {
            10
        } else {
            8
        };

        let width = self.width();
        // `read_file` renders `NNNN| content`; drop the gutter for display (the
        // model still gets the numbers in the raw tool result). Expand tabs
        // (tabstop 4) so indentation is measured in the columns the terminal
        // will draw, not counted as zero-width.
        let visible: Vec<String> = lines
            .iter()
            .take(show_max)
            .map(|line| {
                let v = if is_read {
                    strip_read_lineno(line)
                } else {
                    *line
                };
                expand_tabs(v).into_owned()
            })
            .collect();

        if is_error {
            // Errors are shown verbatim (red); `tool_panel` wraps and styles
            // each line.
            for line in &visible {
                self.tool_panel(line, true, true);
            }
        } else {
            // Non-error results: one fenced render of the whole visible block
            // at the full width — the fence gutter *is* the row's indent, so
            // the rows are painted flush — then one grey panel row per
            // returned line.
            let block = visible.join("\n");
            for row in highlight_code_fg(&block, lang, width).split('\n') {
                self.paint_panel(row, false);
            }
        }

        if lines.len() > show_max && !is_error {
            self.tool_panel(
                &format!("... +{} lines", lines.len() - show_max),
                false,
                true,
            );
        }
        self.end_tool_block();
    }
}

impl Content {
    pub(crate) fn display_committed_prompt(&self, line: &str) {
        let shares_rule = self.boundary.last_kind() == Some(ContentDisplayKind::Prompt);
        let rule = separator_line(self.width());
        let block = if shares_rule {
            format!("{line}\n{rule}")
        } else {
            format!("{rule}{line}\n{rule}")
        };
        self.write_content_kinded(block.as_bytes(), Some(ContentDisplayKind::Prompt));
    }

    pub(crate) fn display_plain(&self, text: &str) {
        // The count comes from the bytes: `writeln!` adds the newline, and a
        // caller-supplied line is padded or not as it likes. Then normalized —
        // a caller whose text already ends in a newline would otherwise emit a
        // trailing blank row, which the contract forbids: synthetic content
        // ends at a line boundary, never with a separator.
        let block = format!("{}\n", text);
        self.write_content_kinded(
            &at_line_start(block.as_bytes()),
            Some(ContentDisplayKind::SystemMessage),
        );
    }

    pub(crate) fn display_markdown(&self, text: &str) {
        let rendered = render_markdown(text, self.width());
        let mut block = String::with_capacity(rendered.len() + 1);
        block.push_str(&rendered);
        block.push('\n');
        // Measured, not predicted: a long rendered line wraps, and `rendered`
        // is a count of *logical* lines, not screen rows (§9 #6/#10).
        // The blank above the block is not this method's business any more:
        // `boundary_spacer` puts it there when the kind changes, which is the
        // pair this block opens. The trailing one is ours to suppress: the
        // renderer often ends with a blank of its own, and a synthetic block
        // may not.
        self.write_content_kinded(
            &at_line_start(block.as_bytes()),
            Some(ContentDisplayKind::SystemContent),
        );
    }

    pub(crate) fn display_text(&self, text: &str) {
        // The bytes, not the source, decide the height: the `⏺ ` prefix is two
        // columns the old `wrap(text).len()` count never saw, inline markdown
        // rewrites the text, and a message that exactly fills the width wraps
        // onto another row (§9 #6). Measuring the bytes removes the guess *and*
        // the DSR that used to correct for it.
        let block = content::text::system_message(text);
        self.write_content_kinded(
            &at_line_start(block.as_bytes()),
            Some(ContentDisplayKind::SystemMessage),
        );
    }

    /// One line of non-REPL input — the piped/batch `/`-driver's read. The
    /// line comes from [`Self::input`]: stdin in production, a script in
    /// tests, nothing under quiet. The read writes the `❯ ` prompt through the
    /// chain — zero rows, but it closes any blank line.
    pub(crate) fn get_user_input(&self) -> String {
        match &*self.input.lock().unwrap() {
            InputSource::Empty => String::new(),
            InputSource::Stdin => {
                self.write_content(format!("{} ", style("❯").bold().blue()).as_bytes());
                // The read itself is the edge's (design §TermIO: all IO under
                // one roof) — `TermIO::read_line`, through the hub's front
                // door like every other edge verb.
                let input = self.manager.lock().unwrap().read_line().unwrap_or_default();
                input.trim().to_string()
            }
            #[cfg(test)]
            InputSource::Script(script) => script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "/q".to_string()),
        }
    }

    pub(crate) fn begin_response(&self) {
        // The bar's dot goes green for the turn. Set here rather than at the
        // call site that builds the text: this layer draws the bar, and a turn
        // starting is exactly when its state changes.
        self.streaming
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // One render path for every run (B3c). There used to be a second one
        // for a non-tty stdout: buffer the whole response and render it as one
        // markdown block at the end. Two paths meant every replay test — a
        // test's stdout is never a terminal — exercised the *other* one, so the
        // incremental renderer, where the separator bugs live, went untested.
        // Colour still follows the terminal: that is a property of the bytes,
        // not of the render path.
        let theme = answer_theme();
        let current_width = self.width();
        let mut parser = self.parser.lock().unwrap();
        *parser = StreamParser::new_gfm_stream();
        let mut renderer = self.renderer.lock().unwrap();
        *renderer = Renderer::new(theme, current_width);
        // Reset the thinking parser/renderer for this response. Its dimming
        // needs no reset: the renderer fades each row as it emits it, so no
        // attribute outlives a block and there is no state to carry between
        // turns.
        let mut tparser = self.thinking_parser.lock().unwrap();
        *tparser = StreamParser::new_gfm_stream();
        let mut trenderer = self.thinking_renderer.lock().unwrap();
        *trenderer = Renderer::new(Theme::dimmed(), current_width);
        // No blank line written here: the row between the prompt and the
        // response is `boundary_spacer`'s, on the first `ResponseText` write —
        // `begin_response` writes nothing, so it has nothing to ask the stream
        // about. Its leading blank lines are stripped by that same write,
        // which is this half of "begin/end_response strip".
    }

    pub(crate) fn stream_text(&self, delta: &str) {
        // Sync terminal width (updated by in-flight reader on resize) so
        // renderers reflow on the next feed.
        let current_width = self.width();
        {
            let mut renderer = self.renderer.lock().unwrap();
            if renderer.width() != current_width {
                renderer.set_width(current_width);
            }
            let mut trenderer = self.thinking_renderer.lock().unwrap();
            if trenderer.width() != current_width {
                trenderer.set_width(current_width);
            }
        }
        // Coarse-grained live rendering: feed the delta to the incremental
        // parser and emit only the *completed* blocks. Each block prints once
        // (code lines line-by-line, paragraphs on close), so there is no whole
        // document re-render, no in-place cursor gymnastics, and scrollback
        // stays clean.
        // One counted block: the boundary blank (if any) and the rendered
        // blocks. The renderer produced the bytes, so the counter measures
        // them rather than the delta text (§9 #10).
        // Thinking just ended. Whatever the thinking renderer still holds is
        // committed here, as its own counted block under its own kind, *before*
        // the answer's block opens — so the pair recorded for the boundary is
        // (reasoning, text) and the blank row between them is
        // `boundary_spacer`'s, drawn when this block starts. Committed from
        // inside the text block instead, the reasoning would be recorded under
        // the text kind, the pair would read (text, text), and the row would
        // have to be invented here by asking the stream what it ended with — a
        // question the held-newline rule exists to stop asking.
        //
        // The signal is the flush itself, not a dim flag: dimming is a property
        // of the rows the thinking renderer emits and ends with them, so there
        // is nothing to turn off, and a flush that produces nothing opens no
        // block and draws no row.
        let mut thinking: Vec<u8> = Vec::new();
        self.commit_thinking(&mut thinking);
        if !thinking.is_empty() {
            self.write_content_block_as(
                &at_line_start(&thinking),
                Some(ContentDisplayKind::ResponseReasoning),
                true,
                crate::termio::Stream::Reasoning,
            );
        }
        self.with_content_writer(
            current_width,
            Some(ContentDisplayKind::ResponseText),
            false,
            |mut out| {
                let mut parser = self.parser.lock().unwrap();
                let events = parser.write(delta.as_bytes());
                let mut renderer = self.renderer.lock().unwrap();
                let _ = renderer.feed(&events, &mut out);
            },
        );
    }

    pub(crate) fn stream_thinking(&self, delta: &str) {
        // Content mode => stream the reasoning text; otherwise suppress it.
        if *self.show_reasoning.lock().unwrap() != ShowReasoning::Content {
            return;
        }
        // Sync terminal width (updated by in-flight reader on resize) so
        // renderers reflow on the next feed.
        let current_width = self.width();
        {
            let mut trenderer = self.thinking_renderer.lock().unwrap();
            if trenderer.width() != current_width {
                trenderer.set_width(current_width);
            }
        }
        // Render thinking through its own incremental renderer with a faded
        // theme, which brackets every row it emits in the faint attribute —
        // fading is a property of a row, not a mode the stream has to be put
        // into and taken out of, so this writer emits no escapes of its own and
        // the boundary after the reasoning has nothing to close. The rendered
        // bytes are buffered; `commit_reasoning` lands them through the chain
        // marked `Stream::Reasoning`, and `TermIO` picks the sink (the
        // parser/renderer locks are still held through the landing).
        let mut rendered: Vec<u8> = Vec::new();
        let mut parser = self.thinking_parser.lock().unwrap();
        let events = parser.write(delta.as_bytes());
        let mut renderer = self.thinking_renderer.lock().unwrap();
        let _ = renderer.feed(&events, &mut rendered);
        self.commit_reasoning(rendered);
    }

    /// Land a rendered reasoning block: the chain like any other block, its
    /// write marked `Stream::Reasoning` so `TermIO` routes it — the primary
    /// when it is a terminal (the reasoning is on screen, choreographed with
    /// everything around it), meta otherwise (a pipe consumer drops it with
    /// `2>/dev/null` or merges it with `2>&1`, without stdout ever seeing
    /// it). Content states the kind; the sink is the edge's alone.
    ///
    /// Empty means the delta rendered nothing: no write at all — even a
    /// zero-byte hand-off would open a block.
    fn commit_reasoning(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        // Quiet suppresses the hand-off too — nothing reaches either stream.
        if self.quiet {
            return;
        }
        // `closes = false`: a streamed delta is never a block end. The thinking
        // renderer's state carries across deltas — a list or fence inside the
        // reasoning is still open between two `stream_thinking` calls — so
        // declaring each chunk closed would make every following chunk an
        // opener (`old == kind && closed`) and `boundary_spacer` would draw a
        // blank row between consecutive lines of one continuous stream, live
        // only: replay feeds the whole reasoning in one call and never sees
        // those interior boundaries. The genuine ends declare themselves
        // where they happen: `write_segment_tail` (segment/turn), the
        // supersession commit in `stream_text` (thinking → answer), and
        // `end_tool_block` for tool panels.
        self.write_content_block_as(
            &bytes,
            Some(ContentDisplayKind::ResponseReasoning),
            false,
            crate::termio::Stream::Reasoning,
        );
    }

    pub(crate) fn end_response_segment(&self) {
        let current_width = self.width();
        // One counted block: the answer commit, the thinking commit, the
        // boundary blank and the dim reset. Inside the turn, so `Response`:
        // the separators between a segment and the tool panel below are the
        // response's own, not a content boundary.
        // One counted block, accumulated and emitted at the end so the
        // normalization below sees the whole segment: the answer commit, the
        // thinking commit and the dim reset, ending at a line boundary with no
        // blank row behind it. The row between this segment and the tool panel
        // below is `boundary_spacer`'s, drawn when that panel arrives as
        // (text, tool) — it is not this block's to end with.
        // Commit any open answer paragraph so the tool call/result prints
        // below, then start a *fresh* answer renderer for the next segment.
        // This drops the renderer's `pending_gap`, which otherwise fires at
        // the next answer segment's first block and would double the blank
        // line at the thinking→answer boundary in multi-turn turns (the
        // transition already emits one blank).
        let mut answer: Vec<u8> = Vec::new();
        {
            let mut parser = self.parser.lock().unwrap();
            // `flush()` (not `write("\n\n")`) so a segment whose answer text
            // ends in a list/table is emitted now rather than stranded; then
            // start a fresh answer parser + renderer for the next segment.
            let events = parser.flush();
            *parser = StreamParser::new_gfm_stream();
            let mut renderer = self.renderer.lock().unwrap();
            let _ = renderer.feed(&events, &mut answer);
            let theme = Theme {
                clickable_links: true, // real TTY path
                ..Default::default()
            };
            *renderer = Renderer::new(theme, current_width);
        }
        // Commit thinking (the thinking renderer is kept — post-tool
        // thinking continues as one continuous faded stream; only the
        // parser is reset, inside `commit_thinking`).
        let mut thinking: Vec<u8> = Vec::new();
        self.commit_thinking(&mut thinking);
        self.write_segment_tail(current_width, &answer, &thinking);
    }

    pub(crate) fn end_response(&self) {
        // Before the flush, so the last bar of the turn is still the streaming
        // one; the bar painted after this is the idle one.
        self.streaming
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let current_width = self.width();
        // Streaming is over, so each remainder comes back as one buffer: strip
        // it whole rather than carry a "might still be trailing" state across
        // chunks `end_response` will never see again. Each half is then emitted
        // as its own normalized block by `write_segment_tail`, which also
        // declares the turn's last flush closed — the row *between* this
        // response and whatever comes next is `boundary_spacer`'s, and writing
        // one here would make two.
        let mut answer: Vec<u8> = Vec::new();
        {
            // Finalize the answer document.
            let mut parser = self.parser.lock().unwrap();
            let events = parser.flush();
            let mut renderer = self.renderer.lock().unwrap();
            let _ = renderer.feed(&events, &mut answer);
        }
        let mut thinking: Vec<u8> = Vec::new();
        // Finalize thinking (it was rendered as it streamed, already faded).
        {
            let mut parser = self.thinking_parser.lock().unwrap();
            let events = parser.flush();
            let mut renderer = self.thinking_renderer.lock().unwrap();
            let _ = renderer.feed(&events, &mut thinking);
        }
        self.write_segment_tail(current_width, &answer, &thinking);
    }

    pub(crate) fn display_tool_call(&self, name: &str, args: &Value) {
        // Remember the tool so `display_tool_result` can render the result
        // tool-aware (strip `read_file` line-number gutters, pick a highlight
        // language). The hook always fires the call immediately before its
        // result, so this is safe to read back on the result.
        *self.last_tool.lock().unwrap() = Some((name.to_string(), args.clone()));
        // A multi-file `edit` renders nothing here: its pair is not complete
        // until the result arrives, and splitting it into "all labels now, all
        // notes later" would produce two runs rather than one block per file.
        // `display_tool_result` renders the whole sequence of pairs then, which
        // is what makes a multi-file batch byte-identical to N single-file ones.
        if name.eq_ignore_ascii_case("edit") && multi_file_blobs(args).is_some() {
            return;
        }
        self.render_call_panel(name, args);
    }

    pub(crate) fn display_tool_result(&self, result: &str) {
        // A multi-file result renders as N complete call-result pairs — label,
        // diff, note — one per file, through the very same renderers a
        // sequence of single-file calls uses. The call half is deferred to here
        // because it is not complete until the result exists: rendering the
        // labels at the call event would give two contiguous runs instead of
        // one block per file, and the block is the unit.
        if let Some(pairs) = self.multi_file_pairs(result) {
            for (name, args, single) in &pairs {
                self.render_call_panel(name, args);
                self.render_result_panel(single);
            }
            return;
        }
        // A result with no matching call context (replayed on its own): the
        // call half, and so the diff, is gone, but the report is still per
        // file and still needs its path to attribute a failure.
        if let Some(files) = bsed_blockedit::parse_file_results(result) {
            for fr in files {
                let label = highlight_label_fg(&format!("edit {}", fr.path), "bash", self.width());
                for row in label.split('\n') {
                    self.paint_panel(row, false);
                }
                let single = match &fr.result {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                self.render_result_panel(&single);
            }
            self.end_tool_block();
            return;
        }
        self.render_result_panel(result);
    }

    pub(crate) fn display_error(&self, error: &str) {
        // Quiet's one surviving verb, no longer special-cased here: the write
        // rides `Stream::Error` and the edge picks the sink (meta on a pipe,
        // the primary when it is a terminal), while the quiet drop above
        // spares the kind (design §Content). Measured: a long error wraps,
        // and the message carries a 2-column prefix the row count has to
        // include.
        let block = format!("{} Error: {}\n", style("⏺").red(), error);
        self.write_content_block_as(
            &at_line_start(block.as_bytes()),
            Some(ContentDisplayKind::SystemMessage),
            false,
            crate::termio::Stream::Error,
        );
    }

    pub(crate) fn display_system(&self, text: &str) {
        let block = format!("{}\n", style(text).bold().dim());
        self.write_content_kinded(
            &at_line_start(block.as_bytes()),
            Some(ContentDisplayKind::SystemContent),
        );
    }

    pub(crate) fn display_separator(&self) {
        // `separator` writes through the row counter; noting again here would
        // count the same row twice.
        self.separator();
    }

    pub(crate) fn display_header(
        &self,
        provider: &str,
        model: &str,
        yolo: bool,
        limit: usize,
        persona: Option<&str>,
    ) {
        let block =
            content::text::header_block(provider, model, yolo, limit, persona, self.width());
        self.write_content(block.as_bytes());
    }

    /// True while a capture session is open. Same-task with writes (the
    /// single-writer proof at [`Self::with_content_writer`]), so the read needs
    /// no lock held across the span.
    fn is_capturing(&self) -> bool {
        self.captured.lock().unwrap().is_some()
    }

    /// Begin capturing what Content hands over (design §Content: capture is
    /// Content's — the block's bytes are tapped at the output, before the hub
    /// hand-off, so a capturing write never reaches `render()`; content bytes
    /// only, never frame bytes).
    pub(crate) fn begin_capture(&self) {
        *self.captured.lock().unwrap() = Some(Vec::new());
    }

    /// End a capture session and return what Content produced. See
    /// [`Self::begin_capture`].
    pub(crate) fn end_capture(&self) -> Vec<u8> {
        self.captured
            .lock()
            .unwrap()
            .take()
            .expect("end_capture without begin_capture")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    // The test-local `strip_ansi` helper below needs the lead-byte table.
    use crate::ansi::utf8_len;
    // `wrap` is unit-tested here but no longer imported in production (the
    // highlight path defers wrapping to the markdown formatter).
    use crate::content::text::wrap;
    // The pure pipeline the manager's render runs (and the composer tests
    // re-run).
    use crate::render::{allocate, live_budget, render, Element, RowAsk};
    // Preview helpers these tests exercise directly, and the apply policy a
    // hand-rolled frame path names.
    use crate::content::preview::{
        edit_preview, refined_diff_lines, truncate, wrap_with_diff_markers,
    };

    // --- Row accounting: the tracked cursor vs. the real one (phase B) ---
    //
    // A wrong row count does not show up as a wrong number in isolation: the
    // *next* frame anchors one row off, and the visible damage is a clear that
    // lands in the wrong place (row_accounting §6 B2). So the assertion is
    // against the screen: run a writer, replay its bytes through a terminal
    // emulator, and require the tracked row to be the row the cursor is on.

    /// A `Content` writing into a capture buffer, with the shared width
    /// pinned so the expected geometry does not depend on the test runner's
    /// terminal. Height is not pinned (the clamp in `note_lines` needs a real
    /// terminal size), so the cases stay well under one screen.
    ///
    /// The reasoning routing is pinned too, and for the same reason: a capture
    /// harness that inherits `!stdout().is_terminal()` sends reasoning to
    /// *stderr* whenever the runner's stdout is a pipe, so the test sees nothing
    /// and concludes "reasoning was dropped" — when in fact it was diverted.
    /// Under a runner that hands the binary a tty the same value is `false`, the
    /// reasoning lands in the capture, and a test that assumed the pipe case
    /// fails for a reason that has nothing to do with the code. A capturing
    /// harness should capture everything the code might write, so this pins the
    /// answer to the question every test in this file is asking.
    ///
    /// An `AppState` whose bar carries exactly `name` as the session label, for
    /// the marker the painting tests look for on screen. The bar reads its
    /// facts off `AppState`, so the tests hand it one rather than
    /// pushing a status; the model is a fixed unknown so no window shows and
    /// the name renders verbatim between the dot and the padding.
    ///
    /// What these tests are for is the painting (transience, the gap, the
    /// erase, the dim). The fitting — which field gives way to what, and the
    /// ellipsis — is a different contract and has its own tests beside
    /// `status_bar`.
    fn bar_app(name: &str) -> Arc<AppState> {
        let app = crate::app_state::AppState::for_test(crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_reasoning_level: None,
            show_reasoning: String::new(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            models: Default::default(),
            path: None,
        });
        app.set_model("llamacpp:test");
        app.update_cycle_view(
            String::new(),
            Some(name.to_string()),
            None,
            Arc::new(Vec::new()),
            None,
        );
        app
    }

    /// The dot's colour is the bar's only state, and it is **derived** from the
    /// response lifecycle rather than passed in at a call site.
    ///
    /// Composed **once**, deliberately. The recipe is what `DrawCache` keeps, and
    /// the two renders below differ only in `streaming` — which is the whole of
    /// §5.1.1's split: presence is compose's, content is the element's, resolved
    /// when it is drawn. Asserting on the *rendered* row is what makes this a
    /// check that the element reads the flag; a flag set and never consulted
    /// would pass a state assertion alone.
    #[test]
    fn the_dot_follows_the_response_lifecycle() {
        let (out, _reasoning) = capturing_output(60);
        assert!(
            !out.streaming.load(std::sync::atomic::Ordering::Relaxed),
            "no turn in flight, so the bar is idle"
        );
        // Composed while idle — the REPL composes once per keystroke, and the
        // turn's flag flips *after* that (agent.rs `begin_response`), so a row
        // baked at compose time would show the previous turn's dot.
        let area = compose_area(&out, None, Vec::new());
        out.begin_response();
        let during = compose_rows(&area, 60);
        out.end_response();
        let after = compose_rows(&area, 60);
        assert!(
            during.join("\n").contains("\x1b[38;5;41m\u{25cf}"),
            "the same recipe, mid-turn, renders a green dot: {during:?}"
        );
        assert!(
            !after.join("\n").contains("\x1b[38;5;41m\u{25cf}"),
            "and idle again once the turn is over: {after:?}"
        );
        assert!(
            !out.streaming.load(std::sync::atomic::Ordering::Relaxed),
            "and the flag went back down"
        );
    }

    /// The rows a composed list would paint, without touching a terminal.
    ///
    /// Asks alone cannot identify a row: the status and the divider both ask
    /// `min == max == 1`, so a test about *what* the composer produced has to
    /// look at the bytes rather than the budgets.
    fn compose_rows(items: &[Element], term_w: usize) -> Vec<String> {
        let asks: Vec<RowAsk> = items.iter().map(|e| e.ask(term_w)).collect();
        let heights = allocate(&asks, 24, live_budget(24));
        let frame = render(items, &heights, term_w);
        frame
            .extents
            .iter()
            .map(|r| frame.buf[r.start..r.end].to_string())
            .collect()
    }

    /// The composer supplies only what changes: the input, and the menu when one
    /// is open. The status row and the divider are its own to resolve, because a
    /// caller that could add either would make it a second place deciding what
    /// the area is made of (ui_draw_v2 §5.1.1).
    #[test]
    fn compose_area_resolves_every_row_it_owns() {
        let (out, _reasoning) = capturing_output(60);
        let input = || crate::repl::test_input(&shared_editor(|_| {}), "> ");

        // Nothing supplied: the gap and the status, and they come first.
        assert_eq!(
            compose_rows(&compose_area(&out, None, Vec::new()), 60).len(),
            2
        );

        // Input only: no separator, because a separator is for a menu.
        let rows = compose_rows(&compose_area(&out, Some(input()), Vec::new()), 60);
        assert_eq!(rows.len(), 3, "gap, status, input: {rows:?}");

        // Both: the divider arrives *with* the overlay, not from the caller.
        let overlay = crate::repl::test_overlay(&["row"], 0);
        let rows = compose_rows(&compose_area(&out, Some(input()), overlay), 60);
        assert_eq!(
            rows.len(),
            5,
            "gap, status, input, divider, overlay: {rows:?}"
        );
        assert!(
            !rows[0].contains("[bar]") && !rows[0].contains('─'),
            "row 0 is the empty gap above the bar: {:?}",
            rows[0]
        );
        assert!(
            rows[1].contains("\x1b[48;5;237m"),
            "row 1 is the status bar: {:?}",
            rows[1]
        );
        assert!(rows[2].contains("> "), "row 2 is the input: {:?}", rows[2]);
        assert!(rows[3].contains('─'), "row 3 is the divider: {:?}", rows[3]);
        assert!(rows[4].contains("row"), "row 4 is the menu: {:?}", rows[4]);
    }

    /// The frame bar's dot is read from `streaming` at compose time rather than
    /// handed in as `false` by whoever composed, so the frame path and the
    /// content path cannot render the same turn differently. Asserted on the
    /// rendered bytes: a flag nobody reads would still pass a flag check.
    #[test]
    fn compose_area_reads_streaming_rather_than_being_told_it() {
        let (out, _reasoning) = capturing_output(60);
        let idle = compose_rows(&compose_area(&out, None, Vec::new()), 60);
        assert!(
            !idle[1].contains(crate::ui::bar::DOT_STREAMING),
            "no turn in flight, so the frame bar is idle: {:?}",
            idle[1]
        );

        out.begin_capture();
        out.begin_response();
        let _ = out.end_capture();
        let during = compose_rows(&compose_area(&out, None, Vec::new()), 60);
        assert!(
            during[1].contains(crate::ui::bar::DOT_STREAMING),
            "mid-turn the frame bar goes green: {:?}",
            during[1]
        );

        out.begin_capture();
        out.end_response();
        let _ = out.end_capture();
    }

    /// The bar is **always present**: no `is_off` gate, so the
    /// membership is the same shape whether or not the status has anything to
    /// say. The gate was an artifact of the old push — it protected against a
    /// stale/absent pushed row. With a live pull the bar always knows how to
    /// render itself, so an empty status reserves its row and says nothing.
    #[test]
    fn compose_area_keeps_the_bar_present_even_with_nothing_to_say() {
        let (out, _reasoning) = capturing_output(60);
        // The bar is always present: gap, then the bar row — whether or
        // not the stores have anything to say. There is no "off" state to
        // test; an unnamed, window-less session still reserves its row.
        assert_eq!(
            compose_area(&out, None, Vec::new()).len(),
            2,
            "gap, then a bar — always"
        );
    }

    /// The context figure is **pulled live** off the agent's shared usage store
    /// not pushed into the status. Bump the store mid-turn and
    /// the next compose shows the new number with no push — which is exactly
    /// what the old `set_status` snapshot could not do (it was frozen at turn
    /// start until the next push). A push-based bar would fail this: the store
    /// moves, the pushed facts do not, and the figure follows the store.
    ///
    /// The figure goes through `context_figure`, so an estimate
    /// stands in before any completion reports — set it and the bar shows it
    /// just the same, then the provider's count wins.
    #[test]
    fn the_bar_pulls_the_context_figure_live_rather_than_a_pushed_snapshot() {
        let (out, _reasoning) = capturing_output(60);
        // A fresh store, as the agent's constructor installs it.
        let usage = std::sync::Arc::new(Mutex::new(crate::agent::SessionUsage::default()));
        out.set_usage(Some(usage.clone()));

        let before = compose_rows(&compose_area(&out, None, Vec::new()), 60);
        assert!(
            !before[1].contains("100k"),
            "nothing recorded, so no context figure: {:?}",
            before[1]
        );

        // An estimate stands in before any completion reports.
        usage.lock().unwrap().estimated_input_tokens = 42_000;
        let estimated = compose_rows(&compose_area(&out, None, Vec::new()), 60);
        assert!(
            estimated[1].contains("42k"),
            "the estimate stands in: {:?}",
            estimated[1]
        );

        // A completion reports, mid-turn, writing the store in place — the one
        // thing the turn already does (agent/turn.rs, per completion). It wins
        // over the estimate.
        usage.lock().unwrap().last_input_tokens = Some(100_000);
        let after = compose_rows(&compose_area(&out, None, Vec::new()), 60);
        assert!(
            after[1].contains("100k"),
            "the provider's report wins over the estimate, no push involved: {:?}",
            after[1]
        );

        // A compaction invalidates the report (drops it) mid-menu: the
        // estimate — refreshed on demand at the next turn boundary — stands
        // in again at the next compose, with no push either way.
        usage.lock().unwrap().invalidate_context_report();
        let invalidated = compose_rows(&compose_area(&out, None, Vec::new()), 60);
        assert!(
            invalidated[1].contains("42k"),
            "the dropped report hands the figure back to the estimate: {:?}",
            invalidated[1]
        );
    }

    fn capturing_output(width: u16) -> (Content, crate::settings::ShowReasoningHandle) {
        let reasoning = crate::settings::default_show_reasoning_handle();
        let out = Content::with_size(
            reasoning.clone(),
            TermSize {
                width: width as usize,
                height: 24,
            },
        );
        (out, reasoning)
    }

    /// The bar holds **stores**, not a snapshot: a mid-turn
    /// `/reasoning` commit lands on `AppState` (`settle`), and the bar — which
    /// reads the level off `AppState` at render — shows the new level at the
    /// next paint with no push. This is the staleness the live context
    /// pull did not cover: a push-based bar would keep the old level until the
    /// turn ended (the running turn keeps its `live_params`).
    #[test]
    fn the_bar_follows_a_mid_turn_reasoning_commit_with_no_push() {
        let (out, _reasoning) = capturing_output(60);
        let app = bar_app("[bar]");
        let stack = || {
            vec![
                Element::lines(vec![String::new()]),
                crate::ui::bar::status(app.clone(), out.usage_handle(), out.streaming.clone()),
            ]
        };
        let before = compose_rows(&stack(), 60);
        assert!(
            !before[1].contains("high"),
            "no level yet, so the bar shows none: {:?}",
            before[1]
        );
        // The commit lands on the target config, exactly where `settle` writes
        // it; the running turn would still be on its old `live_params`.
        app.set_reasoning(Some(crate::params::ReasoningLevel::High));
        let after = compose_rows(&stack(), 60);
        assert!(
            after[1].contains("high"),
            "the bar reads the live level, no push involved: {:?}",
            after[1]
        );
    }

    /// The bar's label comes off `AppState`: the opening message
    /// stands in for an unnamed session, and the submit path's
    /// `set_first_message_if_unset` fills it before the branch has it.
    #[test]
    fn the_bar_reads_the_session_identity_off_app_state() {
        let (out, _reasoning) = capturing_output(60);
        let app = crate::app_state::AppState::for_test(crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_reasoning_level: None,
            show_reasoning: String::new(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            models: Default::default(),
            path: None,
        });
        app.set_model("llamacpp:test");
        let stack = || {
            vec![
                Element::lines(vec![String::new()]),
                crate::ui::bar::status(app.clone(), out.usage_handle(), out.streaming.clone()),
            ]
        };
        let empty = compose_rows(&stack(), 60);
        assert!(
            !empty[1].contains("fix the bug"),
            "no identity yet: {:?}",
            empty[1]
        );
        // The first prompt: the submit path records the text as the stand-in.
        app.set_first_message_if_unset("fix the bug");
        let shown = compose_rows(&stack(), 60);
        assert!(
            shown[1].contains("fix the bug"),
            "the message labels the session: {:?}",
            shown[1]
        );
        // A second submit must not overwrite it.
        app.set_first_message_if_unset("something else");
        let still = compose_rows(&stack(), 60);
        assert!(
            still[1].contains("fix the bug") && !still[1].contains("something else"),
            "the first message sticks: {:?}",
            still[1]
        );
    }

    /// A shared editor for a test — the handle `compose_frame`/`paint_prompt`
    /// take, seeded through a scoped lock.
    fn shared_editor(
        build: impl FnOnce(&mut crate::repl::LineEditor),
    ) -> Arc<Mutex<crate::repl::LineEditor>> {
        let e = Arc::new(Mutex::new(crate::repl::LineEditor::new()));
        build(&mut e.lock().unwrap());
        e
    }

    /// The membership a REPL transition would install, built locally so the
    /// tests can drive the pure pipeline without a live session (the test twin
    /// the plan keeps: compose a stack, then `configure` + `render(None)`).
    /// Mirrors `PromptSession::sync_stack`: gap, always-present bar, input, and
    /// the divider-then-overlay tail when an overlay is open.
    fn compose_area(out: &Content, input: Option<Element>, overlay: Vec<Element>) -> Vec<Element> {
        let mut stack = Vec::new();
        stack.push(Element::lines(vec![String::new()]));
        stack.push(crate::ui::bar::status(
            bar_app("[bar]"),
            out.usage_handle(),
            out.streaming.clone(),
        ));
        stack.extend(input);
        if !overlay.is_empty() {
            stack.push(Element::divider());
            stack.extend(overlay);
        }
        stack
    }

    /// Swap `out`'s byte edge for a recording `TermIO` and return the handle
    /// a test reads the bytes back through. The byte-edge seam for tests that
    /// need *frame* bytes: capture taps the content hand-off only
    /// (design §Content) — frame bytes can never be caught — so a test
    /// asserting on what reached the terminal reads `TermIO`'s recording
    /// backend under the manager's `Draw`.
    fn record_bytes(out: &Content) -> Recording {
        record_bytes_with(
            out,
            crate::termio::Caps {
                primary: true,
                meta: true,
            },
        )
    }

    /// [`record_bytes`] with pinned caps — the encoding double for a real
    /// backend whose primary is (or is not) a terminal, so the kind → sink
    /// routing is exercised. A non-terminal primary sends the diverting kinds
    /// to the meta bucket.
    fn record_bytes_with(out: &Content, caps: crate::termio::Caps) -> Recording {
        let hub = out.manager.clone();
        hub.lock().unwrap().draw().term_io = crate::termio::TermIO::recording_with(caps);
        Recording(hub)
    }

    /// What a [`record_bytes`] swap captured — read at assert time through
    /// the hub handle, so a test keeps no lock and no borrow into the manager.
    struct Recording(Arc<Mutex<crate::render::RenderManager>>);

    impl Recording {
        /// The primary stream — screen bytes, choreography included.
        fn bytes(&self) -> Vec<u8> {
            self.0
                .lock()
                .unwrap()
                .draw()
                .term_io
                .recorded(crate::termio::Stream::Content)
        }

        /// The meta sink — the diverted-reasoning edge (the recording double's
        /// pinned caps are a non-terminal primary, so `Reasoning` routes here).
        fn meta(&self) -> Vec<u8> {
            self.0
                .lock()
                .unwrap()
                .draw()
                .term_io
                .recorded(crate::termio::Stream::Reasoning)
        }
    }

    /// Tracked cursor row of `out` (0-based, as the screen reports it).
    fn tracked_row(out: &Content) -> usize {
        out.manager.lock().unwrap().draw().cache.tracked_row() - 1
    }

    /// Row the cursor ends on when `bytes` are replayed into a `width`x24
    /// terminal.
    ///
    /// The ONLCR shim is the one ui_draw_v2 P0 established: production emits
    /// bare `\n` and the tty line discipline turns it into CRLF, so the
    /// emulator has to be fed what the terminal would actually receive.
    /// Without it every LF leaves the column untouched and the whole model
    /// drifts sideways instead of down.
    fn screen_row(bytes: &[u8], width: u16) -> usize {
        let mut p = vt100::Parser::new(24, width, 100_000);
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        p.screen().cursor_position().0 as usize
    }

    #[test]
    fn display_text_row_is_measured_not_predicted() {
        // A message that exactly fills the width, and one that is a column
        // wider: the old `wrap(text).len()` count called both one row.
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        out.begin_response();
        out.display_text(&"x".repeat(36)); // + "⏺ " = 38 columns
        out.display_text(&"y".repeat(39)); // + "⏺ " = 41 -> wraps
        let bytes = sink.bytes();
        assert_eq!(
            tracked_row(&out),
            screen_row(&bytes, 40),
            "a message that wraps must be counted as two rows"
        );
    }

    #[test]
    fn display_text_counts_a_wide_character_as_two_columns() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        out.begin_response();
        // 19 CJK chars = 38 columns, plus the 2-column marker: wraps at 40.
        out.display_text(&"漢".repeat(19));
        let bytes = sink.bytes();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    #[test]
    fn streamed_answer_is_counted() {
        // The untracked path (§9 #10): a streamed paragraph used to leave the
        // tracked row where it was.
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        out.begin_response();
        out.stream_text("First paragraph of the answer.\n\n");
        out.stream_text("Second paragraph, streamed.\n\n");
        out.end_response();
        let bytes = sink.bytes();
        assert_eq!(
            tracked_row(&out),
            screen_row(&bytes, 40),
            "streamed content is not accounted for"
        );
    }

    #[test]
    fn streamed_thinking_is_counted() {
        let (out, reasoning) = capturing_output(40);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        let sink = record_bytes(&out);
        out.begin_response();
        out.stream_thinking("Thinking about the problem.\n\n");
        let bytes = sink.bytes();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    #[test]
    fn tool_panel_is_counted() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        out.begin_response();
        out.display_tool_call("Bash", &serde_json::json!({"command": "ls -la"}));
        out.display_tool_result("a\nb\nc");
        let bytes = sink.bytes();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    /// A tab-indented bash result must not overflow the panel row.
    ///
    /// Regression (the wrapping-indented-result artifact): `wrap` counted a
    /// leading tab as one *character* and `panel_row`'s display-width measure
    /// counted it as *zero columns*, while the terminal draws it as a run of
    /// spaces to the next tab stop (four here). The row came out
    /// `indent - 1` columns too wide, natural-wrapped in the terminal with no
    /// newline between the physical rows, and `LineClear` never armed its
    /// erase on the continuation — so the frame beneath (the status bar)
    /// bled through. `expand_tabs` now expands the tab before any measure, so
    /// every emitted row is exactly the panel width and stays one physical
    /// line. The vt100 replay is the proof: an over-wide row wraps without a
    /// `\n`, so the cursor would land past the tracked row count.
    #[test]
    fn a_tab_indented_bash_result_fills_the_panel_width_and_stays_one_row() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        out.begin_response();
        out.display_tool_call("bash", &serde_json::json!({"command": "cat types.ts"}));
        out.display_tool_result("\t\tinterface Foo {}");
        let bytes = sink.bytes();
        assert_eq!(
            tracked_row(&out),
            screen_row(&bytes, 40),
            "a tab-indented row overflowed and natural-wrapped (leaking the frame)"
        );
    }

    /// The label/result indent contract: the command label (tool name and
    /// arguments) sits flush at the panel edge, and the result below it is
    /// indented — for a fenced render by the code-fence gutter, which *is*
    /// the result indent (rows are painted flush).
    #[test]
    fn a_tool_label_is_flush_and_its_result_is_indented() {
        let (out, _reasoning) = capturing_output(80);
        let sink = record_bytes(&out);
        out.begin_response();
        out.display_tool_call("bash", &serde_json::json!({"command": "ls -la"}));
        out.display_tool_result("total 0");
        let text = strip_ansi(&String::from_utf8_lossy(&sink.bytes()));
        let label = text
            .lines()
            .find(|l| l.contains("$ ls -la"))
            .expect("label row");
        assert!(
            label.starts_with("$ ls -la"),
            "command label is indented: {label:?}"
        );
        let result = text
            .lines()
            .find(|l| l.contains("total 0"))
            .expect("result row");
        assert!(
            result.starts_with("  total 0"),
            "result not indented by the fence gutter: {result:?}"
        );
    }

    /// A multi-line bash input keeps only its first line flush: the later
    /// command lines carry the result indent pre-wrap, which aligns them
    /// under the `$ ` prompt (two columns, same as the indent).
    #[test]
    fn a_multiline_bash_label_indents_under_the_prompt() {
        let (out, _reasoning) = capturing_output(80);
        let sink = record_bytes(&out);
        out.begin_response();
        out.display_tool_call(
            "bash",
            &serde_json::json!({"command": "cat <<'EOF'\none\ntwo\nEOF"}),
        );
        let text = strip_ansi(&String::from_utf8_lossy(&sink.bytes()));
        let rows: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("$ cat") || l.contains("one") || l.contains("two"))
            .collect();
        assert!(
            rows[0].starts_with("$ cat"),
            "first line not flush: {rows:?}"
        );
        assert!(
            rows[1].starts_with("  one"),
            "second command line not indented: {rows:?}"
        );
        assert!(
            rows[2].starts_with("  two"),
            "third command line not indented: {rows:?}"
        );
    }

    #[test]
    fn markdown_and_error_and_plain_are_counted() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        out.begin_response();
        out.display_markdown("# Title\n\nA paragraph long enough to wrap at forty columns.\n");
        out.display_error("an error long enough to wrap onto a second row at forty");
        out.display_plain("plain text that is long enough to wrap at forty columns ok");
        out.display_system("a system line long enough to wrap at forty columns");
        let bytes = sink.bytes();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    // --- B3a: the separator matrix (characterization) ---
    //
    // The rule under test (row_accounting_b3.md §1): one blank line at every
    // segment boundary, and inside a segment nothing but what the markdown
    // renderer wrote. The second half is the checkable one — the renderer is
    // available directly, so the *host's* contribution is whatever the painted
    // bytes have that a fresh renderer does not produce. This suite measures
    // today's behavior; it changes nothing.

    /// One step of a response, in the order the live path delivers it.
    #[derive(Clone, Copy)]
    enum Step {
        /// A reasoning delta (one chunk of it).
        Think(&'static str),
        /// An answer delta.
        Say(&'static str),
        Tool(&'static str),
        Result(&'static str),
        /// `end_response_segment` — the segment boundary.
        SegmentEnd,
        /// `end_response`.
        ResponseEnd,
    }

    /// How the deltas are split on their way in. `stream_text` sees deltas live
    /// and whole blocks in replay, so any rule that depends on how *often* it
    /// is called is a bug waiting for a different chunk boundary.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Chunking {
        /// One call per step.
        Whole,
        /// One call per byte.
        Bytes,
        /// One call per word, whitespace dropped.
        Words,
    }

    impl Chunking {
        fn all() -> [Chunking; 3] {
            [Chunking::Whole, Chunking::Bytes, Chunking::Words]
        }
        fn name(self) -> &'static str {
            match self {
                Chunking::Whole => "whole",
                Chunking::Bytes => "bytes",
                Chunking::Words => "words",
            }
        }
    }

    /// Deliver one `Say`/`Think` delta in this chunking.
    fn feed(out: &Content, step: Step, chunk: Chunking) {
        let (text, stream): (&str, fn(&Content, &str)) = match step {
            Step::Say(t) => (t, |o, t| o.stream_text(t)),
            Step::Think(t) => (t, |o, t| o.stream_thinking(t)),
            _ => return,
        };
        match chunk {
            Chunking::Whole => stream(out, text),
            Chunking::Bytes => {
                for b in text.as_bytes() {
                    stream(out, std::str::from_utf8(&[*b]).unwrap_or(""));
                }
            }
            Chunking::Words => {
                // Split at whitespace *runs*, keeping them: a chunk boundary
                // must not eat the paragraph breaks, or the test would be
                // measuring the harness instead of the code.
                let mut rest = text;
                while !rest.is_empty() {
                    let end = rest
                        .find(char::is_whitespace)
                        .map(|i| {
                            rest[i..]
                                .find(|c: char| !c.is_whitespace())
                                .map_or(i, |j| i + j)
                        })
                        .unwrap_or(rest.len());
                    if end == 0 {
                        let ws = rest
                            .find(|c: char| !c.is_whitespace())
                            .unwrap_or(rest.len());
                        stream(out, &rest[..ws]);
                        rest = &rest[ws..];
                    } else {
                        stream(out, &rest[..end]);
                        rest = &rest[end..];
                    }
                }
            }
        }
    }

    /// Run a scripted response through the live render path; return the bytes.
    fn run_script(steps: &[Step], chunk: Chunking) -> Vec<u8> {
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        // Reasoning rides the captured stream because it is chained like any
        // other block now (`commit_reasoning`); a replay test that silently
        // stopped seeing it would look like a spacing bug, hence the explicit
        // level rather than the handle default.
        out.begin_capture();
        out.begin_response();
        for step in steps {
            match step {
                Step::Tool(name) => out.display_tool_call(name, &serde_json::json!({})),
                Step::Result(text) => out.display_tool_result(text),
                Step::SegmentEnd => out.end_response_segment(),
                Step::ResponseEnd => out.end_response(),
                other => feed(&out, *other, chunk),
            }
        }
        out.end_capture()
    }

    /// What the markdown renderer alone produces for `text`: the reference for
    /// "the host adds nothing inside a segment".
    fn renderer_alone(text: &str) -> String {
        let mut parser = StreamParser::new_gfm_stream();
        let mut events = parser.write(text.as_bytes());
        events.extend(parser.flush());
        // The same theme production builds the answer renderer with, or its
        // styling would look like host contribution.
        let mut r = Renderer::new(answer_theme(), 80);
        let mut out: Vec<u8> = Vec::new();
        let _ = r.feed(&events, &mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Visible lines of painted output, ANSI stripped, *without* the row the
    /// cursor rests on: a stream ending in `\n` leaves the cursor on a row
    /// nothing was written to, and that row reads as blank.
    fn painted_lines(bytes: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes).replace("\r", "");
        let mut lines: Vec<String> = text
            .split('\n')
            .map(|l| strip_ansi(l).trim_end().to_string())
            .collect();
        if text.ends_with('\n') {
            lines.pop();
        }
        lines
    }

    /// Indices of blank lines, and the longest run of consecutive ones.
    fn blank_runs(lines: &[String]) -> (Vec<usize>, usize) {
        let mut runs: Vec<usize> = Vec::new();
        let mut longest = 0;
        let mut cur = 0;
        for (i, l) in lines.iter().enumerate() {
            if l.trim().is_empty() {
                runs.push(i);
                cur += 1;
                longest = longest.max(cur);
            } else {
                cur = 0;
            }
        }
        (runs, longest)
    }

    /// The core of the rule: no two consecutive blank lines, ever. One blank
    /// at a boundary, and the renderer's own spacing nowhere doubled.
    #[track_caller]
    fn assert_no_doubled_blank(name: &str, steps: &[Step], chunk: Chunking) {
        let bytes = run_script(steps, chunk);
        let lines = painted_lines(&bytes);
        let (_, longest) = blank_runs(&lines);
        assert_eq!(
            longest,
            1,
            "{name} [{}]: {longest} consecutive blank lines\n{:#?}",
            chunk.name(),
            lines
        );
    }

    /// One segment, one answer block: the painted output is *exactly* what the
    /// renderer produced, ending at a line boundary. Anything else is a byte
    /// the host contributed on its own.
    #[test]
    fn host_adds_nothing_inside_a_segment() {
        for text in [
            "A single paragraph answer.\n\n",
            "Two paragraphs.\n\nThe second one.\n\n",
            "A paragraph, then a list:\n\n- one\n- two\n\n",
            "Text with `inline code` and a [link](https://x.example).\n\n",
        ] {
            for chunk in Chunking::all() {
                let bytes = run_script(&[Step::Say(text), Step::ResponseEnd], chunk);
                let painted = String::from_utf8_lossy(&bytes).to_string();
                // Neither end may add a separator. At the stream's start there
                // is nothing to separate from, so `boundary_spacer` has no
                // answer for (None, Response) and `begin_response` writes
                // nothing at all. And `end_response` strips the flushes' blank
                // rows, because the row *below* this response is the next
                // block's divider to emit — synthetic content ends at a line
                // boundary and never with a separator. What is left is the
                // renderer's bytes, with its own trailing blank normalized to
                // the one line terminator the last line needs.
                let alone = renderer_alone(text);
                let mut expected = alone.trim_end_matches('\n').to_string();
                if !expected.is_empty() {
                    expected.push('\n');
                }
                assert_eq!(
                    painted,
                    expected,
                    "host contributed extra bytes for {text:?} [{}]",
                    chunk.name()
                );
            }
        }
    }

    /// The same answer split across deltas must paint identically to the
    /// one-call version: chunking is not allowed to be visible.
    #[test]
    fn chunking_is_not_visible() {
        let text = "First paragraph of the answer.\n\nSecond paragraph here.\n\n";
        let whole = run_script(&[Step::Say(text), Step::ResponseEnd], Chunking::Whole);
        for chunk in [Chunking::Bytes, Chunking::Words] {
            assert_eq!(
                run_script(&[Step::Say(text), Step::ResponseEnd], chunk),
                whole,
                "[{}] painted differently from the one-call version",
                chunk.name()
            );
        }
    }

    /// Streamed reasoning is one continuous block: no interior boundary, any
    /// chunking. Regression for the `26d2119` fallout, where every
    /// `commit_reasoning` delta declared `closes=true` — each following delta
    /// then opened a new block and `boundary_spacer` drew a blank row between
    /// consecutive lines of one open list (live only; replay feeds the whole
    /// reasoning in one call). `assert_no_doubled_blank` cannot see this — the
    /// inserted rows are single and content-separated — so the pin is
    /// byte-equality against the one-call version, with structures whose
    /// interior the renderer must keep tight: a list and a fence.
    #[test]
    fn reasoning_chunking_is_not_visible() {
        for text in [
            "1. First item\n2. Second item\n3. Third item\n",
            "Let's think.\n\n1. First item\n2. Second item\n\nMore reasoning.\n",
            "reasoning before\n\n```python\ndef add(a, b):\n    return a + b\n```\n\nafter\n",
        ] {
            let whole = run_script(&[Step::Think(text), Step::ResponseEnd], Chunking::Whole);
            for chunk in [Chunking::Bytes, Chunking::Words] {
                assert_eq!(
                    run_script(&[Step::Think(text), Step::ResponseEnd], chunk),
                    whole,
                    "[{}] reasoning painted differently from the one-call version for {text:?}",
                    chunk.name()
                );
            }
        }
    }

    /// At a segment boundary, exactly one blank line — not zero, not two.
    #[test]
    fn one_blank_line_at_a_segment_boundary() {
        for chunk in Chunking::all() {
            assert_no_doubled_blank(
                "answer, boundary, answer",
                &[
                    Step::Say("The first answer.\n\n"),
                    Step::SegmentEnd,
                    Step::Say("After the tool.\n\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
        }
    }

    /// Reasoning is a `Stream::Reasoning` write all the way down, and the sink
    /// is the edge's answer from the backend's caps — never content's. Pinned
    /// on the recording double with *both* cap sets, so neither half can
    /// inherit the runner's terminal the way the old `reasoning_on_stderr`
    /// flag once did: a terminal primary keeps the reasoning on the primary
    /// edge (it is on screen, choreographed), a piped primary diverts it to
    /// meta (stderr through the real backends, which is what makes
    /// `2>/dev/null` work for a consumer of `prior "q" | cat`).
    #[test]
    fn reasoning_is_marked_reasoning_and_the_edge_routes_it() {
        let script = [Step::Think("Weighing the options.\n\n"), Step::ResponseEnd];

        // On the screen: a terminal primary keeps reasoning on the primary edge.
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        let recording = record_bytes_with(
            &out,
            crate::termio::Caps {
                primary: true,
                meta: true,
            },
        );
        out.begin_response();
        feed(&out, script[0], Chunking::Whole);
        feed(&out, script[1], Chunking::Whole);
        let primary = String::from_utf8_lossy(&recording.bytes()).into_owned();
        assert!(
            primary.contains("Weighing the options"),
            "a terminal primary keeps reasoning on the primary edge: {primary:?}"
        );

        // Piped: the same write diverts to meta — content marked the kind,
        // the edge picked the sink.
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        let recording = record_bytes_with(
            &out,
            crate::termio::Caps {
                primary: false,
                meta: true,
            },
        );
        out.begin_response();
        feed(&out, script[0], Chunking::Whole);
        feed(&out, script[1], Chunking::Whole);
        let meta = String::from_utf8_lossy(&recording.meta()).into_owned();
        assert!(
            meta.contains("Weighing the options"),
            "a piped primary diverts reasoning to meta: {meta:?}"
        );
    }

    /// The meta sink under the hub: a reasoning block — chained like any
    /// other, marked `Stream::Reasoning` — rides the kind token down to
    /// `TermIO`, which routes it to the meta edge under a piped primary.
    /// Recorded on the meta edge, absent from main. Main's emptiness is the
    /// choreography pin: the consume walk and reposition of a *primary* write
    /// would put bytes there, so none ran. The bytes themselves are the
    /// renderer's, fed untransformed.
    #[test]
    fn reasoning_to_meta_lands_on_the_meta_edge() {
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.begin_response();
        // A piped primary: the reasoning kind diverts to the meta bucket.
        let recording = record_bytes_with(
            &out,
            crate::termio::Caps {
                primary: false,
                meta: true,
            },
        );
        feed(
            &out,
            Step::Think("Weighing the options.\n\n"),
            Chunking::Whole,
        );

        let meta = String::from_utf8_lossy(&recording.meta()).into_owned();
        assert!(
            meta.contains("Weighing the options"),
            "the meta edge gets the rendered reasoning: {meta:?}"
        );
        let main = String::from_utf8_lossy(&recording.bytes()).into_owned();
        assert!(
            main.is_empty(),
            "no screen choreography behind a meta write — main stays empty, \
             got {main:?}"
        );
    }

    /// A tool round then the next segment's answer: one blank at the
    /// boundary, with the panel rows above it.
    #[test]
    fn one_blank_line_after_a_tool_block() {
        for chunk in Chunking::all() {
            assert_no_doubled_blank(
                "tool, boundary, answer",
                &[
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::SegmentEnd,
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
            assert_no_doubled_blank(
                "tool, tool",
                &[
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Tool("Bash"),
                    Step::Result("ok2\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
        }
    }

    /// An **empty** segment declares no boundary: a turn that ends having
    /// streamed nothing is not a segment, so the retry notices that follow it
    /// are consecutive `SystemMessage` blocks and share the space `boundary_spacer`
    /// gives two of the same kind — none.
    ///
    /// The empty `end_response` is the interesting part, and it is invisible in
    /// the output: it writes no bytes at all. What it used to leave behind was
    /// a `closed` flag set by a segment that never existed, so each retry
    /// message met `(ResponseText, closed)` rather than `(SystemMessage, open)`
    /// and was given a blank line it had not earned. `with_content_writer`
    /// decides it now — see the rollback invariants there — and the two
    /// assertions below are the two halves: the block *between* the retries,
    /// and the block *before* the first one.
    #[test]
    fn an_empty_segment_puts_no_blank_between_retries() {
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.begin_capture();
        // Three failed attempts, each an empty turn: `begin_response` then a
        // stream that errors before a single delta reaches the parser.
        out.begin_response();
        out.end_response();
        out.display_text("Error: rate limited. Retrying in 1s...");
        out.begin_response();
        out.end_response();
        out.display_text("Error: rate limited. Retrying in 2s...");
        out.begin_response();
        out.end_response();
        out.display_text("Error: rate limited. Retrying in 4s...");
        let lines = painted_lines(&out.end_capture());

        assert_eq!(
            lines.len(),
            3,
            "three retry notices and nothing else — no blank row above the \
             first one either, since no response preceded it\n{lines:#?}"
        );
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.contains("Retrying in"),
                "row {i} is a retry notice: {lines:#?}"
            );
        }
    }

    /// The mirror of the case above, and the reason the empty write cannot be
    /// made wholly inert: a segment that *did* stream text still closes
    /// itself, and that closure is what earns the blank line under the notice
    /// the REPL prints afterwards.
    ///
    /// The empty `end_response` here is genuine — the renderer already emitted
    /// the answer as it streamed, so the flush has nothing left to hand back —
    /// yet the segment closed, so the boundary stands. A rollback that dropped
    /// `closes_segment` from every empty write would collapse this to no blank
    /// line, losing the separation between a turn's answer and the status
    /// message that follows it.
    #[test]
    fn a_segment_that_streamed_still_closes_for_the_notice_after_it() {
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.begin_capture();
        // The answer streams and is emitted as it arrives; the paragraph is
        // closed by its own trailing blank, so the flush below is genuinely
        // empty and `end_response`'s write carries no content.
        out.begin_response();
        out.stream_text("The answer to the question.\n\n");
        out.end_response();
        out.display_text("Error: something happened afterwards");
        let lines = painted_lines(&out.end_capture());

        assert_eq!(
            lines,
            vec![
                // Streamed answer text carries no marker — `⏺` is
                // `display_text`'s, and this row went through the answer
                // renderer instead.
                "The answer to the question.".to_string(),
                String::new(),
                "⏺ Error: something happened afterwards".to_string(),
            ],
            "the answer closed a real segment, so the notice is separated by \
             exactly one blank row"
        );
    }

    /// Reasoning then answer: the boundary blank and the dim reset, in the
    /// order the renderer can accept (a `\n` before the reset would be
    /// swallowed as a block's closing newline).
    #[test]
    fn reasoning_then_answer_has_one_blank() {
        for chunk in Chunking::all() {
            assert_no_doubled_blank(
                "reasoning then answer",
                &[
                    Step::Think("Weighing the options.\n\n"),
                    Step::Say("The answer is 42.\n\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
        }
    }

    #[test]
    fn wrap_word_boundary() {
        assert_eq!(wrap("hello world", 5), vec!["hello", "world"]);
    }

    #[test]
    fn wrap_long_no_space() {
        assert_eq!(wrap("abcdefghij", 3), vec!["abc", "def", "ghi", "j"]);
    }

    #[test]
    fn wrap_cjk_respects_width_and_preserves_content() {
        let s = "汉字测试文本";
        let r = wrap(s, 2);
        assert!(
            r.iter().all(|l| l.chars().count() <= 2),
            "a line exceeded the wrap width: {:?}",
            r
        );
        // No spaces are inserted between CJK segments, so concatenation is lossless.
        assert_eq!(r.concat(), s);
    }

    #[test]
    fn wrap_empty_width_returns_lines() {
        assert_eq!(wrap("a\nb", 0), vec!["a", "b"]);
    }

    #[test]
    fn wrap_empty_input() {
        assert_eq!(wrap("", 10), vec![""]);
        // A pure-whitespace logical line keeps its whitespace (it has
        // visible width, unlike a truly empty line).
        assert_eq!(wrap("   ", 10), vec!["   "]);
    }

    #[test]
    fn wrap_preserves_blank_line() {
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn wrap_multiple_spaces_collapsed() {
        assert_eq!(wrap("hello   world", 5), vec!["hello", "world"]);
    }

    #[test]
    fn wrap_keeps_trailing_punctuation_attached() {
        assert_eq!(wrap("Commands:", 100), vec!["Commands:"]);
        assert_eq!(wrap("(provider:name)", 100), vec!["(provider:name)"]);
    }

    #[test]
    fn wrap_single_spaces_around_emdash() {
        assert_eq!(
            wrap("— Show available commands", 100),
            vec!["— Show available commands"]
        );
    }

    #[test]
    fn wrap_preserves_indent_while_collapsing_inter_word_runs() {
        // Leading whitespace is preserved so indentation survives a reflow,
        // but inter-word whitespace runs are still collapsed to a single
        // space (the same behavior as before for the body of the line).
        let help = "  /help      — Show available commands\n  /model      — Select a model";
        let r = wrap(help, 100);
        assert_eq!(
            r,
            vec![
                "  /help — Show available commands".to_string(),
                "  /model — Select a model".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_preserves_indent_on_continuation_lines() {
        // When a logical line is wrapped to multiple physical lines, every
        // continuation line keeps the original indent so the wrapped block
        // reads as one indented paragraph.
        let text = "    one two three four five six seven eight nine ten";
        // Width 15 - 4 (indent) = 11 chars for content per line.
        let r = wrap(text, 15);
        for line in &r {
            assert!(
                line.starts_with("    "),
                "every wrapped line should keep the 4-space indent, got {:?}",
                line
            );
        }
        assert_eq!(
            r,
            vec![
                "    one two".to_string(),
                "    three four".to_string(),
                "    five six".to_string(),
                "    seven eight".to_string(),
                "    nine ten".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_long_word_keeps_indent_on_every_chunk() {
        // A word wider than the available content width is hard-split. Every
        // chunk keeps the indent — not just the first — so no continuation
        // row lands flush-left of the block it belongs to (the panel draws
        // these rows one column in; a bare chunk read as new top-level text).
        let text = "  abcdefghij";
        // Width 5 - 2 (indent) = 3 columns of content per chunk.
        let r = wrap(text, 5);
        assert_eq!(
            r,
            vec![
                "  abc".to_string(),
                "  def".to_string(),
                "  ghi".to_string(),
                "  j".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_with_diff_markers_preserves_indent_on_minus_line() {
        // Regression: a `- ` diff line wrapping an indented source line
        // used to collapse the indent (e.g. `-     if foo {` → `- if foo {`),
        // because plain `wrap()` saw the `-` as a normal word and
        // `split_whitespace` collapsed the run of whitespace between the
        // marker and the first token. The diff-marker helper now routes
        // diff lines through a marker-aware path that preserves the body's
        // leading indent.
        let r = wrap_with_diff_markers("-     if foo {", 80);
        assert_eq!(r, vec!["-     if foo {".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_preserves_indent_on_plus_line() {
        let r = wrap_with_diff_markers("+         let x = 1;", 80);
        assert_eq!(r, vec!["+         let x = 1;".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_continuation_aligns_under_marker() {
        // Long diff line: the body wraps, the marker stays on the first
        // line, and subsequent lines indent by two columns to align under
        // the marker column.
        let text = "- foo bar baz qux quux quuz corge grault garply waldo fred";
        // Width 20 - 2 (marker column) = 18 chars for the body per line.
        let r = wrap_with_diff_markers(text, 20);
        // The first line must keep the marker; continuation lines must be
        // prefixed with two spaces to align under `- `.
        assert!(
            r[0].starts_with("- "),
            "first line must keep the marker, got {:?}",
            r
        );
        for line in &r[1..] {
            assert!(
                line.starts_with("  "),
                "continuation lines must indent by two columns, got {:?}",
                line
            );
        }
        // And the body content must survive intact (no collapsing).
        let joined: String = r.iter().flat_map(|l| l.chars()).collect();
        // Every original character must appear in the wrapped output.
        assert!(joined.contains("foo"));
        assert!(joined.contains("garply"));
    }

    #[test]
    fn wrap_with_diff_markers_passes_plain_lines_through_unchanged() {
        // Lines that aren't `- ` / `+ ` markers (e.g. a context line that
        // happens to start with `-`) are wrapped via the regular `wrap`,
        // which keeps the indent-preservation behavior for normal text.
        let r = wrap_with_diff_markers("    hello world", 80);
        assert_eq!(r, vec!["    hello world".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_empty_input() {
        assert_eq!(wrap_with_diff_markers("", 80), vec!["".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_multiple_lines_mixed() {
        // A multi-line input that mixes diff markers and a blank logical line.
        let text = "-     if foo {\n+     if bar {\n";
        let r = wrap_with_diff_markers(text, 80);
        assert_eq!(
            r,
            vec![
                "-     if foo {".to_string(),
                "+     if bar {".to_string(),
                "".to_string(),
            ]
        );
    }

    #[test]
    fn truncate_multibyte_no_panic() {
        assert_eq!(truncate("hello", 3), "hel...");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn get_preview_read_file_range() {
        let args = serde_json::json!({"path": "src/content/handle.rs", "offset": 100, "limit": 50});
        assert_eq!(
            get_preview("read_file", &args).0,
            "read src/content/handle.rs [101..150]"
        );
        // zero limit => whole file, no range
        let args = serde_json::json!({"path": "src/content/handle.rs", "offset": 0, "limit": 0});
        assert_eq!(
            get_preview("read_file", &args).0,
            "read src/content/handle.rs"
        );
        // offset-only (limit 0) => open-ended range
        let args = serde_json::json!({"path": "src/content/handle.rs", "offset": 19, "limit": 0});
        assert_eq!(
            get_preview("read_file", &args).0,
            "read src/content/handle.rs [20..]"
        );
        // byte mode replaces the line window, with the effective byte range
        // (byte_limit 0 => the default budget, resolved the same way the
        // tool resolves it).
        let args = serde_json::json!({"path": "src/content/handle.rs", "byte_offset": 5000});
        assert_eq!(
            get_preview("read_file", &args).0,
            "read src/content/handle.rs @bytes 5000..15240"
        );
        let args = serde_json::json!({"path": "f", "byte_offset": 100, "byte_limit": 50 * 1024});
        assert_eq!(
            get_preview("read_file", &args).0,
            "read f @bytes 100..51300"
        );
    }

    /// A blob that will not canonicalize — here a multi-file one, which is
    /// rejected at ingress — must render as a label and nothing else. The bug
    /// this pins: the old `edit` branch fell through to the legacy `old`/`new`
    /// arm with two empty strings, so `content.find("")` hit line 1 and the
    /// panel printed `edit b.rs [1]` above an empty diff.
    #[test]
    fn an_uncanonicalizable_blob_renders_no_range_and_no_diff() {
        let multi = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\nb.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        let (label, diff) = get_preview("edit", &serde_json::json!({ "blocks": multi }));
        assert!(!label.contains('['), "no fabricated range: {label}");
        assert!(diff.is_empty(), "no fabricated diff: {diff:?}");
    }

    /// The mirror: a blob that *does* canonicalize renders fully, and takes
    /// `path` from the blob's own header even though the args carry none.
    #[test]
    fn a_resolvable_blob_renders_its_diff_and_its_header_path() {
        let args = serde_json::json!({
            "blocks": "src/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END"
        });
        let (label, diff) = get_preview("edit", &args);
        assert_eq!(label, "edit src/x.rs");
        assert!(!diff.is_empty(), "expected a diff: {diff:?}");
    }

    #[test]
    fn get_preview_bash_full_cmdline() {
        let args = serde_json::json!({"cmd": "cargo build --release && echo done"});
        // No truncation: the full command is preserved verbatim, prefixed with
        // `$` so it reads like a shell prompt.
        assert_eq!(
            get_preview("bash", &args).0,
            "$ cargo build --release && echo done"
        );
    }

    #[test]
    fn get_preview_list_dir_shortened_to_ls() {
        // `list_dir` (and any `List_*` variant) collapses to `ls <path>`.
        for name in ["list_dir", "ListDir", "ListFiles"] {
            let (label, diff) = get_preview(name, &serde_json::json!({"path": "src"}));
            assert_eq!(label, "ls src");
            assert!(diff.is_empty());
        }
    }

    #[test]
    fn get_preview_edit_file_falls_back_without_file() {
        let args = serde_json::json!({
            "path": "/no/such/file.rs",
            "old": "a",
            "new": "b",
            "all": false
        });
        let (label, diff) = get_preview("edit_file", &args);
        assert_eq!(label, "edit /no/such/file.rs");
        // The refined diff is derived purely from oldText/newText, so it is
        // shown even when the file cannot be read (e.g. during replay).
        assert_eq!(
            diff,
            vec!["@@ -1 +1 @@", "- a", "+ b"],
            "diff should come from old/new: {diff:?}"
        );
    }

    #[test]
    fn get_preview_edit_legacy_single_edit_shape() {
        // The legacy top-level oldText/newText shape (accepted by the edit
        // tool but undocumented) must still produce a diff preview.
        let args = serde_json::json!({
            "path": "/no/such/file.rs",
            "oldText": "a",
            "newText": "b"
        });
        let (label, diff) = get_preview("edit_file", &args);
        assert_eq!(label, "edit /no/such/file.rs");
        // File doesn't exist, so no range, but the diff is still rendered from
        // oldText/newText (this is what guarantees replay shows the edits).
        assert_eq!(
            diff,
            vec!["@@ -1 +1 @@", "- a", "+ b"],
            "diff should come from oldText/newText: {diff:?}"
        );
    }

    #[test]
    fn get_preview_edit_multiple_edits_in_batch() {
        // A multi-edit batch should render a diff for each edit, concatenated
        // directly (the `@@` headers already distinguish the change regions),
        // so a single tool call previews every change it makes.
        let args = serde_json::json!({
            "path": "/no/such/file.rs",
            "edits": [
                {"oldText": "a", "newText": "b"},
                {"oldText": "x", "newText": "y"}
            ]
        });
        let (label, diff) = get_preview("edit_file", &args);
        assert_eq!(label, "edit /no/such/file.rs");
        assert_eq!(
            diff,
            vec!["@@ -1 +1 @@", "- a", "+ b", "@@ -1 +1 @@", "- x", "+ y",],
            "each batch edit should produce its own diff: {diff:?}"
        );
    }

    #[test]
    fn edit_preview_preserves_indent_on_diff_lines() {
        // Regression: edit_preview builds `- ` and `+ ` diff lines from the
        // raw source, preserving any leading indentation. Each line is then
        // routed through `wrap_with_diff_markers` (via `tool_panel`), which
        // must not collapse that indentation when reflowing.
        let _tmp = tempfile::tempdir().unwrap();
        let dir = _tmp.path().to_path_buf();
        let path = dir.join("sample.rs");
        // Use a unique content so a leftover file from a previous run
        // can't change the diff shape. Clean up before and after.
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "fn main() {\n    let x = 1;\n    println!(\"{}\", x);\n}\n",
        )
        .unwrap();
        let old_text = "    let x = 1;\n    println!(\"{}\", x);".to_string();
        let new_text = "    let x = 2;\n    println!(\"{}!\", x);".to_string();
        let edits = vec![crate::tools::EditOp {
            old_text: old_text.clone(),
            new_text: new_text.clone(),
            start_line: None,
            origin: None,
        }];
        let (_range, diff) = edit_preview(path.to_str().unwrap(), &edits);

        // First, the raw edit_preview output must keep the indent: each
        // diff line carries the source line verbatim, with the original
        // leading spaces intact.
        assert!(
            diff.iter().any(|l| l == "-     let x = 1;"),
            "old first line should keep its indent in the raw diff, got {:?}",
            diff
        );
        assert!(
            diff.iter().any(|l| l == "+     let x = 2;"),
            "new first line should keep its indent in the raw diff, got {:?}",
            diff
        );

        // Second, when each diff line is routed through the same wrap path
        // that tool_panel uses, the indent must still survive. This is the
        // visible regression: previously, the indent collapsed to a single
        // space because plain `wrap()` treated `-` as a normal word and
        // `split_whitespace` collapsed the run between `-` and the first
        // source-line token.
        for line in &diff {
            let wrapped = wrap_with_diff_markers(line, 80);
            assert_eq!(
                wrapped,
                vec![line.clone()],
                "indent should survive wrap_with_diff_markers for {:?}",
                line
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refined_diff_lines_shows_changes_without_trailing_newlines() {
        let old_text = "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x);\n}\n";
        let new_text =
            "fn main() {\n    let x = 42;\n    let y = 2;\n    println!(\"{}\", x);\n}\n";
        let lines = refined_diff_lines(old_text, new_text);

        // Hunk header present.
        assert!(
            lines.iter().any(|l| l.contains("@@")),
            "should contain hunk header: {lines:?}"
        );
        // Show the changed lines with marker + space + indented content.
        assert!(
            lines.iter().any(|l| l == "-     let x = 1;"),
            "removed line: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "+     let x = 42;"),
            "added line: {lines:?}"
        );
        // Context lines use a space marker + space then the source indent.
        assert!(
            lines.iter().any(|l| l == "      let y = 2;"),
            "context line: {lines:?}"
        );
        // No line may carry a trailing newline (would split into blank rows).
        for l in &lines {
            assert!(
                !l.ends_with('\n'),
                "line must not end with newline: {:?}",
                l
            );
        }
    }

    #[test]
    fn refined_diff_lines_handles_multiple_hunks() {
        // Changes separated by more than the context radius form distinct
        // hunks (close changes would merge into a single hunk).
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..30 {
            if i == 2 {
                old.push_str("    a = 1;\n");
                new.push_str("    a = 10;\n");
            } else if i == 25 {
                old.push_str("    b = 2;\n");
                new.push_str("    b = 20;\n");
            } else {
                old.push_str(&format!("    line {i};\n"));
                new.push_str(&format!("    line {i};\n"));
            }
        }
        let lines = refined_diff_lines(&old, &new);

        let hunk_count = lines.iter().filter(|l| l.contains("@@")).count();
        assert_eq!(hunk_count, 2, "should have two hunks, got: {lines:?}");
        assert!(lines.iter().any(|l| l == "-     a = 1;"));
        assert!(lines.iter().any(|l| l == "+     a = 10;"));
        assert!(lines.iter().any(|l| l == "-     b = 2;"));
        assert!(lines.iter().any(|l| l == "+     b = 20;"));
    }

    #[test]
    fn refined_diff_lines_no_changes_is_empty() {
        let text = "fn main() {\n    let x = 1;\n}\n";
        let lines = refined_diff_lines(text, text);
        assert!(
            lines.is_empty(),
            "identical text yields no diff lines: {lines:?}"
        );
    }

    #[test]
    fn refined_diff_lines_caps_very_large_replacements() {
        // A huge replacement must be capped and flagged as truncated rather
        // than flooding the output with the entire file.
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..500 {
            old.push_str(&format!("line {i}\n"));
            new.push_str(&format!("line {i} changed\n"));
        }
        let lines = refined_diff_lines(&old, &new);
        assert!(
            lines.len() < 200,
            "should cap huge diffs, got {}",
            lines.len()
        );
        assert!(
            lines.iter().any(|l| l.contains("truncated")),
            "should flag truncation: {lines:?}"
        );
    }

    #[test]
    fn get_preview_unknown_tool_truncates() {
        let args = serde_json::json!({"query": "a very very very very very long value that exceeds fifty characters for sure"});
        let (label, diff) = get_preview("Search", &args);
        assert!(label.starts_with("Search "));
        assert!(label.ends_with("..."));
        assert!(diff.is_empty());
    }

    #[test]
    fn get_preview_grep_full_pattern_and_path() {
        // Matches the `grep_text` tool name (and any `Grep_*` family member).
        let args = serde_json::json!({
            "pat": "fn (main|run)\\(",
            "path": "/home/gabriel/src/prior2/src"
        });
        for name in ["grep_text", "GrepText", "GrepFiles"] {
            let (label, diff) = get_preview(name, &args);
            // Label is shortened to just `grep`, and both pattern and path are
            // shown in full (no 50-col truncation).
            assert_eq!(label, "grep fn (main|run)\\( /home/gabriel/src/prior2/src");
            assert!(diff.is_empty());
        }
    }

    #[test]
    fn render_markdown_gfm_table_is_boxed() {
        let out = render_markdown("| a | b |\n|---|---|\n| 1 | 2 |\n", 80);
        assert!(out.contains('│'), "table needs column separators");
        assert!(out.contains('├'), "table needs a header rule");
        assert!(out.contains('┤'));
        assert!(strip_ansi(&out).contains('1') && strip_ansi(&out).contains('2'));
    }

    /// Strip ANSI SGR sequences so we can assert the *highlighted* output still
    /// contains the original source text (highlighting must not drop content).
    fn strip_ansi(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b {
                if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                    continue;
                }
                i += 1;
                continue;
            }
            let ch_len = utf8_len(bytes[i]);
            if i + ch_len <= bytes.len() {
                out.push_str(std::str::from_utf8(&bytes[i..i + ch_len]).unwrap_or("\u{fffd}"));
            }
            i += ch_len.max(1);
        }
        out
    }

    #[test]
    fn render_markdown_code_is_syntax_highlighted() {
        let src = "```rust\nfn main() {}\n```\n";
        let out = render_markdown(src, 80);
        // SGR codes wrap the tokens: content always renders for a terminal.
        assert!(
            out.contains('\x1b'),
            "rendering must enable syntax highlighting (ANSI present)"
        );
        // Stripping ANSI must recover the source text (no content lost) — the
        // edge's contract once `TermIO` strips for a pipe.
        assert!(
            strip_ansi(&out).contains("fn main()"),
            "highlighting must preserve the source code"
        );
    }

    #[test]
    fn render_markdown_heading_and_blockquote_keep_text() {
        let out = strip_ansi(&render_markdown("# Title\n> quoted", 80));
        assert!(out.contains("Title"));
        assert!(out.contains('│') && out.contains("quoted"));
    }

    /// The renderer is the crate's *incremental* `Renderer` + `StreamParser`:
    /// completed blocks are emitted once (so scrollback stays clean) and code
    /// lines stream as they arrive, while an open paragraph is held back until
    /// it closes (coarse-grained). These tests pin that contract.
    #[test]
    fn incremental_renderer_streams_code_lines_and_coarsens_paragraphs() {
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::no_color(), 80);
        let mut buf: Vec<u8> = Vec::new();

        // Code block streams line-by-line *before* the fence closes.
        let _ = renderer.feed(&parser.write("```python\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("def f():\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("    return 1\n".as_bytes()), &mut buf);
        let mid = String::from_utf8(buf.clone()).unwrap();
        assert!(
            mid.contains("def f():"),
            "code line should render before the fence closes"
        );
        let _ = renderer.feed(&parser.write("```\n".as_bytes()), &mut buf);

        // An open paragraph is NOT emitted until it closes (coarse-grained).
        let before = buf.len();
        let _ = renderer.feed(
            &parser.write("Intro sentence without a trailing blank line".as_bytes()),
            &mut buf,
        );
        assert_eq!(buf.len(), before, "open paragraph must not be emitted yet");

        // Finalizing the document flushes the trailing paragraph.
        let _ = renderer.feed(&parser.flush(), &mut buf);
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("Intro sentence without a trailing blank line"),
            "flush must emit the trailing paragraph"
        );
    }

    #[test]
    fn incremental_renderer_gfm_table_renders() {
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::no_color(), 80);
        let mut buf: Vec<u8> = Vec::new();
        let evs = parser.write("| a | b |\n|---|---|\n| 1 | 2 |\n\n".as_bytes());
        let _ = renderer.feed(&evs, &mut buf);
        let _ = renderer.feed(&parser.flush(), &mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains('│'), "GFM table must render with box chars");
    }

    #[test]
    fn render_markdown_emits_osc8_links() {
        // Content always renders clickable links; the edge strips them per
        // stream when the target is not a terminal (`TermIO::write`).
        let out = render_markdown("[text](https://example.com/a)", 80);
        assert!(
            out.contains("\x1b]8;;https://example.com/a\x1b\\"),
            "render must wrap link in OSC 8"
        );
        assert!(out.contains("text"));
    }

    #[test]
    fn thinking_streams_inline_and_dimmed() {
        // The renderer fades each row of thinking as it emits it: no SGR is
        // opened by the caller, so there is none to close at the end of the
        // stream and none to reason about at a boundary. Inline formatting must
        // not clear the fading *within* a row — the theme's `reset` re-applies
        // it, and the row's own close follows.
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::dimmed(), 80);
        let mut buf: Vec<u8> = Vec::new();
        let _ = renderer.feed(
            &parser.write("let me **think** step by step\n".as_bytes()),
            &mut buf,
        );
        let _ = renderer.feed(&parser.flush(), &mut buf);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\x1b[2m"), "reasoning must be dimmed");
        assert!(
            out.contains("\x1b[0m\x1b[2m"),
            "inline formatting must re-apply dim (reset re-dim), not clear it"
        );
        assert!(
            !out.ends_with("\x1b[2m"),
            "the stream must not end with the attribute still open: {out:?}"
        );
    }

    #[test]
    fn thinking_flushes_on_paragraph_boundary() {
        // Regression test: thinking content should emit on paragraph boundaries
        // (blank line) without waiting for end_response.
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::dimmed(), 80);
        let mut buf: Vec<u8> = Vec::new();

        // First paragraph - no flush yet (no blank line)
        let _ = renderer.feed(&parser.write("\n\n[test] first ".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("\nparagraph \n".as_bytes()), &mut buf);
        let before_flush = String::from_utf8(buf.clone()).unwrap();
        assert!(
            !before_flush.contains("first paragraph"),
            "content held before boundary"
        );

        // Add blank line - should flush
        let _ = renderer.feed(&parser.write("\n".as_bytes()), &mut buf);
        let after_flush = String::from_utf8(buf.clone()).unwrap();
        eprintln!("after newline: {}", after_flush);
        assert!(
            after_flush.contains("first paragraph"),
            "content emitted after boundary"
        );

        // Second paragraph with explicit blank line
        let _ = renderer.feed(&parser.write("second paragraph\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("\n".as_bytes()), &mut buf);
        // let _ = renderer.feed(&parser.write("\n".as_bytes()), &mut buf);
        let after_blank = String::from_utf8(buf.clone()).unwrap();
        assert!(
            after_blank.contains("second paragraph"),
            "content emitted on blank line"
        );
    }

    #[test]
    fn multi_turn_answer_boundary_single_blank() {
        // Reproduces the TTY orchestration with *persistent* renderers across a
        // multi-turn turn (thinking1 -> answer1 -> tool -> thinking2 -> answer2),
        // exactly as the `Content` fields behave. Before the fix, the
        // answer renderer's `pending_gap` (set when the answer paragraph was
        // committed at the tool boundary) fired at answer2's first block, on
        // top of the one blank the thinking→answer transition emits — TWO
        // blank lines. Resetting the answer renderer at the tool boundary
        // (as `end_response_segment` now does) drops that pending gap.
        let mut tparser = StreamParser::new_gfm();
        let mut trenderer = Renderer::new(Theme::dimmed(), 80);
        let mut aparser = StreamParser::new_gfm();
        let mut arenderer = Renderer::new(Theme::default(), 80);
        let mut out: Vec<u8> = Vec::new();
        let w = |out: &mut Vec<u8>, s: &[u8]| out.extend_from_slice(s);

        // thinking1 -> answer1
        let _ = trenderer.feed(&tparser.write(b"think one\n"), &mut out);
        let _ = trenderer.feed(&tparser.write(b"\n\n"), &mut out); // transition
        w(&mut out, b"\n"); // separator
        let _ = arenderer.feed(&aparser.write(b"\n\nanswer one\n"), &mut out);
        w(&mut out, b"\n");

        // tool call boundary: commit the answer paragraph, then RESET the
        // answer renderer (mirrors the fix) so no pending gap survives into
        // answer2. The model's leading blank lines stay in answer2 below.
        let _ = arenderer.feed(&aparser.write(b"\n\n"), &mut out);
        arenderer = Renderer::new(Theme::default(), 80);
        w(&mut out, b"\n");

        // thinking2 -> answer2 (model sends leading blank lines)
        let _ = trenderer.feed(&tparser.write(b"think two\n"), &mut out);
        let _ = trenderer.feed(&tparser.write(b"\n\n"), &mut out); // transition
        w(&mut out, b"\n"); // separator
        let _ = arenderer.feed(&aparser.write(b"\n\nanswer two\n"), &mut out);

        // end_response: flush both documents once.
        let _ = arenderer.feed(&aparser.flush(), &mut out);
        let _ = trenderer.feed(&tparser.flush(), &mut out);
        w(&mut out, b"\n");

        let s = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = s.split('\n').collect();
        let idx = match lines.iter().position(|l| l.contains("answer two")) {
            Some(i) => i,
            None => {
                for (i, l) in lines.iter().enumerate() {
                    eprintln!("[{i}] {:?}", l);
                }
                panic!("answer two not found");
            }
        };
        assert!(
            lines[idx - 1].trim().is_empty(),
            "line immediately before answer two must be blank"
        );
        assert!(
            !lines[idx - 2].trim().is_empty(),
            "exactly ONE blank line before answer two (no double gap)"
        );
    }

    /// Option 2 — the choke: `with_content_writer` wraps, so the
    /// renderer-direct feed path can no longer hand the terminal a line wider
    /// than itself. The break is at the cell edge and the continuation is
    /// entered by a byte — the `\x1b[K` after each `\n` in the expectation is
    /// the line-clear arming there. Pre-fix: one 120-cell run, no arm, and
    /// whatever the frame below painted survived in the continuation.
    #[test]
    fn content_choke_wraps_an_overwide_line_before_the_line_clear() {
        let (out, _reasoning) = capturing_output(60);
        let sink = record_bytes(&out);
        let y = "y".repeat(120);
        out.with_content_writer(60, Some(ContentDisplayKind::ResponseText), false, |w| {
            w.write_all(y.as_bytes()).unwrap();
            w.write_all(b"\n").unwrap();
        });
        let bytes = sink.bytes();
        let text = String::from_utf8(bytes).expect("utf-8 content");
        let expect = format!("\x1b[K{}\n\x1b[K{}\n", &y[..60], &y[60..]);
        assert_eq!(
            text, expect,
            "wrap at the cell edge, clear armed on the continuation"
        );
    }

    /// The safety net never splits an escape: the color opens before the
    /// inserted newline and closes after it — SGR state is the terminal's and
    /// carries the green across the continuation row.
    #[test]
    fn wrapped_content_keeps_its_sgr_across_the_inserted_newline() {
        let (out, _reasoning) = capturing_output(60);
        let sink = record_bytes(&out);
        let y = "y".repeat(120);
        let input = format!("\x1b[32m{y}\x1b[0m\n");
        out.with_content_writer(60, Some(ContentDisplayKind::ResponseText), false, |w| {
            w.write_all(input.as_bytes()).unwrap();
        });
        let bytes = sink.bytes();
        let text = String::from_utf8(bytes).expect("utf-8 content");
        let expect = format!("\x1b[K\x1b[32m{}\n\x1b[K{}\x1b[0m\n", &y[..60], &y[60..]);
        assert_eq!(text, expect, "escape intact, wrap inside the color");
    }

    /// A capture is an assertion against content, not a screen: the width
    /// guard steps aside and the bytes come back as they went in.
    #[test]
    fn captures_stay_raw_no_wrap_no_clear() {
        let (out, _reasoning) = capturing_output(60);
        let y = "y".repeat(120);
        out.begin_capture();
        out.with_content_writer(60, Some(ContentDisplayKind::ResponseText), false, |w| {
            w.write_all(y.as_bytes()).unwrap();
            w.write_all(b"\n").unwrap();
        });
        let cap = out.end_capture();
        let mut expect = y.into_bytes();
        expect.push(b'\n');
        assert_eq!(cap, expect, "captured bytes are the raw content");
    }

    /// Capture is Content's and a frame paint is screen-side (design §Content):
    /// an explicit frame apply reaches the screen and is never caught, while a
    /// captured content write stops at Content's output — content bytes only,
    /// never frame bytes.
    #[test]
    fn capture_catches_content_but_never_frame_bytes() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        let frame =
            crate::repl::Repl::compose_frame(&shared_editor(|_| {}), "", None, Vec::new(), 40, 10);
        out.begin_capture();
        out.manager.lock().unwrap().draw().draw(&frame, None);
        out.display_text("content only");
        let cap = out.end_capture();
        let cap = String::from_utf8_lossy(&cap).into_owned();
        assert!(
            cap.contains("content only"),
            "the content block is what a capture catches: {cap:?}"
        );
        assert!(
            !cap.contains("\x1b[?25"),
            "frame bytes can never be caught (the apply's cursor show is \
             screen-side only): {cap:?}"
        );

        let screen = String::from_utf8_lossy(&sink.bytes()).into_owned();
        assert!(
            screen.contains("\x1b[?25h"),
            "the frame apply reaches the screen: {screen:?}"
        );
        assert!(
            !screen.contains("content only"),
            "…and the content bytes do not — capture kept them off the screen: \
             {screen:?}"
        );
    }

    /// The paint path's pipeline (compose + `manager.render`) is the
    /// hand-rolled path, not a new one: it must paint **byte-identically** to
    /// the apply edge (`Draw::draw`, through the manager's test seam) fed
    /// exactly what it composes.
    ///
    /// Two paths that are meant to be the same are usually asserted against a
    /// transcript, which pins each independently and lets them drift together.
    /// Here they are compared *to each other*, because the whole point of §5.1.1
    /// was that nothing changed — only where the decision is made. Exercises all
    /// four variants so a split in any of them shows.
    ///
    /// **The pop-up is deliberately taller than the live budget.** With a
    /// composition that fits, `allocate` early-returns and both paths agree
    /// trivially — the first version of this test passed with the budget line
    /// mutated out from under it. Total `might` here is above `live_budget(20)`
    /// (⌈2/3·20⌉ = 14) and below the raw height, so the two paths only agree if
    /// they derive the *same* budget, not just the same items.
    #[test]
    fn draw_elements_paints_exactly_what_the_hand_rolled_path_paints() {
        let tall: Vec<String> = (0..16).map(|i| format!("row {i}")).collect();
        let tall: Vec<&str> = tall.iter().map(|s| s.as_str()).collect();
        // The pieces, freshly built per call: `draw_elements` moves them in, so
        // each path needs its own. The composer is the shared thing now — both
        // outputs are given the same status, so `compose_area` yields the same
        // stack for each, and the only difference under test is *who runs the
        // pipeline*.
        let pieces = || {
            let input = crate::repl::test_input(&shared_editor(|_| {}), "> ");
            let overlay = crate::repl::test_overlay(&tall, 0);
            (Some(input), overlay)
        };
        // `render()` composes at the watched size (the manager reads its own
        // private watch), so that is the geometry under test — seeded here on
        // both outputs so the two paths cannot disagree about it. `dims` below
        // is the *hand* path's apply, which still takes dims explicitly.
        let dims = (60, 20);

        let reasoning = crate::settings::default_show_reasoning_handle();
        let (tail, _tail_tx) = Content::with_size_at(
            reasoning.clone(),
            TermSize {
                width: dims.0,
                height: dims.1,
            },
        );
        let tail_sink = record_bytes(&tail);
        let (input, overlay) = pieces();
        let a = {
            let mut mgr = tail.manager.lock().unwrap();
            mgr.configure(
                crate::render::FRAME_MAX_ROW_RATIO,
                compose_area(&tail, input, overlay),
            );
            mgr.render(None);
            mgr.painted_size()
        };
        let via_tail = tail_sink.bytes();

        let (hand, _hand_tx) = Content::with_size_at(
            reasoning,
            TermSize {
                width: dims.0,
                height: dims.1,
            },
        );
        // Feed the apply edge (`Draw::draw`, through the manager's test seam)
        // exactly what the paint path composes — same composer, same status,
        // same dims — so the apply half is all that differs.
        let stack = compose_area(&hand, pieces().0, pieces().1);
        let asks: Vec<RowAsk> = stack.iter().map(|e| e.ask(dims.0)).collect();
        let heights = allocate(&asks, dims.1, live_budget(dims.1));
        let frame = render(&stack, &heights, dims.0);
        let hand_sink = record_bytes(&hand);
        let b = {
            let mut hand_manager = hand.manager.lock().unwrap();
            hand_manager.draw().draw(&frame, None);
            hand_manager.painted_size()
        };
        let via_hand = hand_sink.bytes();

        assert_eq!(a, b, "the same painted dims either way");
        assert_eq!(
            via_tail, via_hand,
            "the tail is the old path, byte for byte — not a second behaviour"
        );
    }

    /// Content written while a frame is live takes the frame's rows — and the
    /// frame comes back **below** what was written (ui_draw_v2 §4.5 P1).
    ///
    /// The other half of the reported resume artefact (§1.1): the content
    /// starts at a cursor inside a still-live frame, and the frame used to be
    /// retired outright, so the menu a commit was issued over simply vanished.
    /// Consuming means the content lands *on* the frame's rows and the
    /// reposition puts the frame back underneath, one fold lower — still on
    /// screen, still below the content, and with no gap the erase left behind.
    #[test]
    fn content_over_a_live_frame_reemits_it_below() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        // A menu frame, as `/session` has open when a commit happens: the
        // input, a divider, and overlay rows — the table header is what the
        // report saw survive.
        let overlay = crate::repl::test_overlay_parts(
            Some("MODEL            CTX    PRICE"),
            &[],
            &[
                "  gpt-5          400k   1.00",
                "  claude-opus    200k   3.00",
            ],
            Some(0),
        );
        let frame =
            crate::repl::Repl::compose_frame(&shared_editor(|_| {}), "", None, overlay, 40, 10);
        out.manager.lock().unwrap().draw().draw(&frame, None);
        out.display_text("Cannot start agent: Missing API key for provider.");
        let bytes = sink.bytes();

        let mut p = vt100::Parser::new(24, 40, 1000);
        // The capture starts at the terminal's first row, but the frame apply is
        // relative to the cursor (row_accounting §6b) and is told the anchor is
        // row 3 — which in production is where a run of content lines left the
        // cursor. Stand those two lines in before replaying.
        p.process(b"\r\n\r\n");
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        let rows: Vec<String> = (0..10)
            .map(|r| {
                (0..120)
                    .map(|c| {
                        p.screen()
                            .cell(r, c)
                            .and_then(|x| x.contents().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect()
            })
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("Missing API key")),
            "the message should be on screen: {rows:?}"
        );
        // The last row the content occupies: the message wraps at this width,
        // so it is the row carrying its tail rather than its head.
        let content_last = rows
            .iter()
            .rposition(|r| r.contains("provider"))
            .unwrap_or_else(|| panic!("the message should be on screen: {rows:?}"));
        for leftover in ["MODEL", "CTX", "gpt-5", "claude-opus", "PRICE"] {
            let at = rows
                .iter()
                .position(|r| r.contains(leftover))
                .unwrap_or_else(|| panic!("frame row {leftover:?} should be back: {rows:?}"));
            assert!(
                at > content_last,
                "frame row {leftover:?} belongs below the content, not among it: {rows:?}"
            );
        }
    }

    /// The cursor follows the frame's own caret: shown where the user can type,
    /// hidden when the input is drawn as no row at all (ui_draw_v2 R1).
    ///
    /// Showing it unconditionally was correct while every frame had an input
    /// row, and wrong the moment R1 removed one — the cursor sat blinking
    /// against a row that was never drawn. `Frame::caret` is already `None` in
    /// exactly those cases, so the decision reads the frame rather than
    /// tracking input state separately.
    #[test]
    fn the_cursor_follows_the_frames_own_caret() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        // Streaming with an empty buffer is R1's `input_hidden`: no input row,
        // and so no caret to place. The element must pull the *streaming* flag,
        // so mint it here rather than via `test_input` (which pins not-streaming).
        out.begin_response();
        {
            let hidden = crate::repl::input(
                shared_editor(|_| {}),
                "> ".to_string(),
                out.streaming.clone(),
            );
            let mut mgr = out.manager.lock().unwrap();
            mgr.configure(
                crate::render::FRAME_MAX_ROW_RATIO,
                compose_area(&out, Some(hidden), Vec::new()),
            );
        }
        crate::repl::paint_prompt(&out);
        let bytes = sink.bytes();
        assert!(
            String::from_utf8_lossy(&bytes).contains("\x1b[?25l"),
            "a frame with no caret hides the cursor"
        );

        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        let editor = shared_editor(|e| e.set("hi"));
        {
            let mut mgr = out.manager.lock().unwrap();
            mgr.configure(
                crate::render::FRAME_MAX_ROW_RATIO,
                compose_area(
                    &out,
                    Some(crate::repl::test_input(&editor, "> ")),
                    Vec::new(),
                ),
            );
        }
        crate::repl::paint_prompt(&out);
        let text = String::from_utf8_lossy(&sink.bytes()).into_owned();
        assert!(
            text.contains("\x1b[?25h"),
            "a frame with a caret shows it: {text:?}"
        );
        assert!(
            !text.contains("\x1b[?25l"),
            "…and does not also hide it: {text:?}"
        );
    }

    /// Without a live frame the streamed blocks just append — no consume, no
    /// reposition — so the tracked row stays in step with the screen.
    #[test]
    fn streamed_content_without_a_live_frame_stays_in_step() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        out.begin_response();
        out.stream_text("a streamed paragraph.\n\n");
        out.stream_text("another one.\n\n");
        out.end_response();
        let bytes = sink.bytes();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    /// A kind switch whose new content renders **only whitespace** normalizes
    /// away: text → whitespace-only reasoning → text paints exactly the two
    /// text paragraphs (one renderer gap between them, as ever), with no
    /// boundary rows around the whitespace. The `rows_due` / `StripLeading` /
    /// `restore_empty` half of the boundary rule, pinned for the shapes a
    /// provider can actually send: spaces, newlines, tab.
    #[test]
    fn whitespace_only_kind_switches_normalize_away() {
        fn run(b_delta: &str) -> String {
            let (out, reasoning) = capturing_output(60);
            *reasoning.lock().unwrap() = ShowReasoning::Content;
            out.begin_capture();
            out.begin_response();
            out.stream_text("first line.\n\n");
            out.stream_thinking(b_delta);
            out.stream_text("second line.\n\n");
            out.end_response();
            let bytes = out.end_capture();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        for b in ["   \n", "\n\n", " \n", "\t\n"] {
            assert_eq!(
                run(b),
                "first line.\n\nsecond line.\n",
                "whitespace-only reasoning delta {b:?} must leave no trace"
            );
        }
    }

    /// Live paints in provider SSE order; grouped (thinking-first) does not.
    ///
    /// Characterization of what an interleaved arrival paints: a reasoning part
    /// that arrives *mid-answer* records `(ResponseReasoning, closes=true)`
    /// between two `ResponseText` chunks, so `boundary_spacer` opens a block
    /// around it — blank row, dim reasoning line, blank row — **inside** the
    /// open code fence. Replay used to regroup thinking-first (hiding exactly
    /// these rows); it now paints stored arrival order — see
    /// `replay::tests::replay_keeps_interleaved_reasoning_in_arrival_order`.
    /// This pins the live half; whether the rows are wanted at every interleave
    /// is a design question still open.
    #[test]
    fn interleaved_reasoning_inserts_live_only_boundary_rows() {
        fn run(interleaved: bool) -> String {
            let (out, reasoning) = capturing_output(60);
            *reasoning.lock().unwrap() = ShowReasoning::Content;
            out.begin_capture();
            out.begin_response();
            if interleaved {
                out.stream_text("Consider the code:\n\n");
                out.stream_text("```python\n");
                out.stream_text("def add(a, b):\n");
                out.stream_thinking("Hmm, add or plus? Let me weigh that.\n");
                out.stream_text("    return a + b\n");
                out.stream_thinking("Also consider subtraction while here.\n");
                out.stream_text("```\n");
            } else {
                out.stream_thinking("Hmm, add or plus? Let me weigh that.\n");
                out.stream_thinking("Also consider subtraction while here.\n");
                out.stream_text(
                    "Consider the code:\n\n```python\ndef add(a, b):\n    return a + b\n```\n",
                );
            }
            out.end_response();
            let bytes = out.end_capture();
            String::from_utf8_lossy(&bytes).into_owned()
        }
        let live = run(true);
        let grouped = run(false);
        println!("=== live (interleaved SSE order) ===\n{live:?}");
        println!("=== grouped (thinking first) ===\n{grouped:?}");
        // Interleaved, a reasoning part that arrives mid-answer records
        // (Reasoning, closes=true) between two ResponseText chunks and
        // `boundary_spacer` opens a new block around it — a blank row above and
        // below the dim reasoning line, INSIDE the open code fence. The same
        // content grouped thinking-first never interleaves, so it renders tight.
        assert!(
            live.contains("add(a, b):\n\n\x1b[2mHmm, add or plus?"),
            "live: blank + reasoning above the fence line: {live:?}"
        );
        assert!(
            live.contains("weigh that.\x1b[0m\n\n      \x1b[35mreturn"),
            "live: blank between code lines after the reasoning interleave: {live:?}"
        );
        assert!(
            grouped.contains("add(a, b):\n      \x1b[35mreturn"),
            "grouped: code lines stay adjacent: {grouped:?}"
        );
        assert!(
            grouped.find("Hmm, add or plus?").unwrap() < grouped.find("Consider the code").unwrap(),
            "grouped: thinking plays before text: {grouped:?}"
        );
    }

    /// A content batch defers the frame's re-emit to the guard's drop instead
    /// of paying one per write (ui_draw_v2 R5).
    ///
    /// Both halves are asserted because either can break alone: the frame is
    /// emitted **once** for a whole burst, *and* it comes back. The invariant
    /// the per-write reposition exists to hold — an area that is consumed is
    /// always put back — is not relaxed here, it is satisfied once at the end
    /// instead of once per write.
    #[test]
    fn a_content_batch_emits_the_frame_once_at_the_end() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        let overlay = crate::repl::test_overlay_parts(
            Some("MODEL            CTX    PRICE"),
            &[],
            &["  gpt-5          400k   1.00"],
            Some(0),
        );
        let frame =
            crate::repl::Repl::compose_frame(&shared_editor(|_| {}), "", None, overlay, 40, 10);
        out.manager.lock().unwrap().draw().draw(&frame, None);
        {
            let _batch = crate::render::ContentBatchGuard::open(&out.manager);
            out.display_text("first line");
            out.display_text("second line");
            out.display_text("third line");
        } // the guard's drop owns the redraw
        let bytes = sink.bytes();

        // Once for the frame's own apply, once for the batch's single
        // re-emit. Unbatched it is four — the apply plus one per write — and
        // that multiplier is R5.
        let text = String::from_utf8_lossy(&bytes);
        let emits = text.matches("MODEL").count();
        assert_eq!(
            emits, 2,
            "one frame re-emit for the whole batch, got {emits}: {text:?}"
        );

        // …and it came back, below the content it was consumed for.
        let mut p = vt100::Parser::new(24, 40, 1000);
        p.process(b"\r\n\r\n");
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        let rows: Vec<String> = (0..10)
            .map(|r| {
                (0..120)
                    .map(|c| {
                        p.screen()
                            .cell(r, c)
                            .and_then(|x| x.contents().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect()
            })
            .collect();
        let content_last = rows
            .iter()
            .rposition(|r| r.contains("third"))
            .unwrap_or_else(|| panic!("content should be on screen: {rows:?}"));
        for leftover in ["MODEL", "gpt-5"] {
            let at = rows
                .iter()
                .position(|r| r.contains(leftover))
                .unwrap_or_else(|| panic!("frame row {leftover:?} should be back: {rows:?}"));
            assert!(
                at > content_last,
                "frame row {leftover:?} belongs below the content: {rows:?}"
            );
        }
    }

    /// A tool block followed by reasoning showed **two** blank lines: the host
    /// wrote the boundary blank, and the long-lived thinking renderer — which
    /// remembered that it had written something — opened its next block with a
    /// gap of its own. `end_tool_block` resets the renderers, so the host's
    /// blank is the only one. Reasoning *before* the tool is what leaves the
    /// renderer mid-document; without it the gap never fires and the bug hides,
    /// which is why this case exists.
    #[test]
    fn one_blank_line_between_a_tool_block_and_reasoning() {
        for (name, steps) in [
            (
                "tool, result, say, end",
                vec![
                    Step::Say("First.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
            (
                "tool, result, think, say, end",
                vec![
                    Step::Say("First.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Think("Weighing again.\n\n"),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
            (
                "tool, result, PENDING think, say, end",
                vec![
                    Step::Say("First.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    // No trailing newlines: the paragraph is still open when
                    // the answer arrives, so it is committed at the boundary.
                    Step::Think("Weighing again."),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
            (
                "think, say, tool, result, think, say, end",
                vec![
                    // Reasoning *before* the tool: the thinking renderer has
                    // written something, so it will want a leading gap for the
                    // next block it renders.
                    Step::Think("Before the tool.\n\n"),
                    Step::Say("Running it.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Think("After the tool.\n\n"),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
        ] {
            for chunk in Chunking::all() {
                assert_no_doubled_blank(name, &steps, chunk);
            }
        }
    }

    #[test]
    fn reasoning_list_before_answer_has_blank_line() {
        // Regression for two related bugs (see `doc/reasoning_order_opus.md`
        // §4 and §10):
        //  (a) reasoning whose last block is a GFM list was stranded below the
        //      answer because the thinking parser was committed with
        //      `write(b"\n\n")` (a blank line can't close a list) instead of
        //      `flush()`;
        //  (b) in the held-answer (reasoning-enabled) path the answer started
        //      immediately after the reasoning with no separating blank line.
        //
        // Using StreamParser+Renderer directly (not Content) avoids
        // fd-level stdout capture, which is fragile under parallel test
        // execution (the test harness writes "test ... ok" lines to stdout
        // from concurrent threads, polluting the capture).
        let mut thinking_parser = StreamParser::new_gfm();
        let mut thinking_renderer = Renderer::new(Theme::dimmed(), 80);
        let mut answer_parser = StreamParser::new_gfm();
        let mut answer_renderer = Renderer::new(Theme::default(), 80);
        let mut out: Vec<u8> = Vec::new();

        // Simulate reasoning that ends with a bullet list (the stranding trigger).
        // Using `flush()` (not `write(b"\n\n")`) so the list closes properly.
        let _ = thinking_renderer.feed(
            &thinking_parser.write(b"Plan:\n- step one\n- step two\n"),
            &mut out,
        );
        let _ = thinking_renderer.feed(&thinking_parser.flush(), &mut out);

        // Separator: one blank line between reasoning and answer.
        out.extend_from_slice(b"\n");

        // Simulate the answer text.
        let _ = answer_renderer.feed(&answer_parser.write(b"done.\n"), &mut out);
        let _ = answer_renderer.feed(&answer_parser.flush(), &mut out);
        out.extend_from_slice(b"\n");

        let plain = strip_ansi(&String::from_utf8_lossy(&out));
        let lines: Vec<&str> = plain.split('\n').collect();

        // Assert (a): list items appear before the answer.
        let list_idx = lines.iter().position(|l| l.contains("step one")).unwrap();
        let ans_idx = lines.iter().position(|l| l.contains("done.")).unwrap();
        assert!(
            list_idx < ans_idx,
            "reasoning (list) must render BEFORE the answer"
        );

        // Assert (b): exactly one blank line before answer.
        assert!(
            ans_idx > 0 && lines[ans_idx - 1].trim().is_empty(),
            "answer must be separated from reasoning by a blank line; got:\n{plain}"
        );
        assert!(
            ans_idx < 2 || !lines[ans_idx - 2].trim().is_empty(),
            "exactly ONE blank line before answer (no double gap)"
        );
    }

    #[test]
    fn tmp_verify_tool_rendering_helpers() {
        // line-number gutter stripping
        assert_eq!(strip_read_lineno("   1| fn main()"), "fn main()");
        assert_eq!(strip_read_lineno("  12| let x = a | b;"), "let x = a | b;");
        assert_eq!(strip_read_lineno("no gutter here"), "no gutter here");
        assert_eq!(strip_read_lineno("   5| "), "");
        // language guessing
        assert_eq!(guess_lang("src/output.rs"), "rust");
        assert_eq!(guess_lang("a/b.py"), "python");
        assert_eq!(guess_lang("x.unknown"), "text");
        // highlighting: keeps the source text and emits ANSI
        let colored = highlight_code_fg("let x = 1;", "rust", 80);
        // ANSI escapes break the literal substring, so strip them first
        let plain = strip_ansi(&colored);
        assert!(
            plain.contains("let x = 1;"),
            "highlighting must not drop content; got plain={plain:?} from colored={colored:?}"
        );
        assert!(colored.contains('\x1b'), "highlighting must emit ANSI");
    }

    #[test]
    fn edit_arg_error_pipes_align_in_the_rendered_panel() {
        // Regression. The renderer emits correct padding, but the panel pipeline
        // re-wraps lines through `wrap()`, which collapses interior runs of
        // spaces — so a padding-based table rendered fine in unit tests and came
        // out ragged on screen. Assert on the *rendered* columns, not on the
        // renderer's output, or this slips through again.
        let msg = "failed to parse tool arguments: Invalid edit tool arguments: \
Invalid block: an edit block is missing its `@@@NEW` section\n\
\n\
Expected:\nsome/path:1234\n@@@OLD\nold text\n@@@NEW\nnew text\n@@@END\n\
\n\
Actual:\nsrc/output.rs\n@@@OLD\n… (5 lines elided) …\n@@@NEW\nself.content.draw.lock().unwrap().anchor()\n";
        let out = Content::with_size(
            crate::settings::default_show_reasoning_handle(),
            TermSize {
                width: 80,
                height: 24,
            },
        );
        out.display_tool_call("edit", &serde_json::json!({"path": "src/x.rs"}));
        out.begin_capture();
        out.display_tool_result(msg);
        let bytes = out.end_capture();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));

        let columns: Vec<usize> = text
            .lines()
            .filter(|l| l.contains('|'))
            .map(|l| l.find('|').unwrap())
            .collect();
        assert!(columns.len() >= 6, "expected a table, got: {text}");
        let first = columns[0];
        assert!(
            columns.iter().all(|c| *c == first),
            "pipes at {columns:?} should all be at {first}:\n{text}"
        );
        // Still a table, and still the two markers that differ on one row.
        let row = text
            .lines()
            .find(|l| l.contains('|') && l.contains("@@@NEW"))
            .unwrap();
        assert!(
            text.contains("Expected") && text.contains("Actual"),
            "{text}"
        );
        let _ = row;
    }

    /// The rendered panel for an `edit` argument error is the `Expected`/`Actual`
    /// table — never a diff.
    ///
    /// Regression, and the reason the test above could pass while the screen was
    /// broken: the `sed`-preview renderer runs first on *every* result, and it
    /// used to sniff for `@@@OLD`. Every argument error embeds the `Expected:`
    /// example, which is a well-formed block, so the panel drew the example as a
    /// phantom diff and returned before the error was rendered. That test's
    /// fixture only escaped because its `Actual:` leaves a dangling `@@@NEW`,
    /// which breaks the parse. These fixtures are the shapes that do not.
    #[test]
    fn an_edit_arg_error_is_never_rendered_as_a_diff() {
        for raw in [
            // `Actual` is a JSON echo, and an echo elided to a single line.
            serde_json::json!({"oldText": "a", "newText": "b"}),
            serde_json::json!({"path": "src/x.rs:10", "startLine": 3, "oldText": "a", "newText": "b"}),
            serde_json::json!({"edits": [{"path": "a.rs", "oldText": "a", "newText": "b"},
                                          {"path": "b.rs", "oldText": "a", "newText": "b"}]}),
            serde_json::json!({"blocks": "@@ -10,1 +10,1 @@\n- a\n+ b"}),
        ] {
            let err = crate::tools::EditArgs::from_wire(&raw).expect_err("must fail");
            let (out, _reasoning) = capturing_output(80);
            out.begin_capture();
            out.display_tool_result(&format!("failed to parse tool arguments: {err}"));
            let bytes = out.end_capture();
            let text = strip_ansi(&String::from_utf8_lossy(&bytes));

            assert!(!text.contains("@@ -"), "phantom diff for {raw}:\n{text}");
            assert!(
                text.contains("Expected") && text.contains("Actual"),
                "no table for {raw}:\n{text}"
            );
            // The problem itself reaches the screen, not just the shape. The
            // panel word-wraps it at the terminal width, so compare with runs of
            // whitespace collapsed on both sides.
            let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let problem = err.lines().next().unwrap();
            let problem = problem.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                flat.contains(&problem),
                "problem line lost for {raw}: {problem:?}\n{text}"
            );
        }
    }

    /// A single-edit failure shows a definition list, not the block table.
    ///
    /// The `Expected:` section follows the shape the caller used (see
    /// `bsed_blockedit::wire`), so this message is JSON on both sides; `arg_error_view`
    /// returns `None` for it and the panel stacks it. The table pairs *block
    /// markers* against what the model sent, so against a JSON `Actual:` it was five
    /// `@@@OLD` rows with one row of JSON beside them.
    #[test]
    fn a_single_edit_failure_renders_as_a_definition_list() {
        let (out, _reasoning) = capturing_output(80);
        out.begin_capture();
        out.display_tool_result(
            "missing required field 'path'\n\nExpected:\n\
             {\"path\": \"some/path.rs\", \"startLine\": 1234, \"oldText\": \"old text\", \
             \"newText\": \"new text\"}\n\nActual:\n{\"newText\":\"b\",\"oldText\":\"a\"}",
        );
        let bytes = out.end_capture();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));

        assert!(!text.contains("|"), "a table was drawn:\n{text}");
        assert!(!text.contains("@@@OLD"), "block markers leaked in:\n{text}");
        assert!(text.contains("Expected:"), "{text}");
        assert!(text.contains("Actual:"), "{text}");
        // Both values survive to the screen, wrapped but intact.
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for want in ["\"oldText\": \"old text\"", "\"oldText\":\"a\""] {
            assert!(flat.contains(want), "{want:?} lost:\n{text}");
        }
    }

    /// The other half of the guard above: gating the result-side `sed` renderer
    /// must not cost the `edit` tool its own blocks-as-diff rendering. That is
    /// the *call* half — `get_preview` → `edit_preview`, fed from the arguments —
    /// and it is a different function on a different input, which is why the two
    /// renderings can be gated independently.
    #[test]
    fn an_edit_call_still_renders_its_blocks_as_a_diff() {
        let (out, _reasoning) = capturing_output(80);
        out.begin_capture();
        out.display_tool_call(
            "edit",
            &serde_json::json!({"blocks": "src/x.rs:5\n@@@OLD\n    run();\n@@@NEW\n    walk();\n@@@END"}),
        );
        let bytes = out.end_capture();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));

        assert!(text.contains("edit src/x.rs"), "no label:\n{text}");
        // The block is anchored (`src/x.rs:5`), so the header carries the real
        // file line, not a buffer-relative `1`.
        assert!(text.contains("@@ -5,1 +5,1 @@"), "no hunk header:\n{text}");
        assert!(text.contains("-     run();"), "no removed line:\n{text}");
        assert!(text.contains("+     walk();"), "no added line:\n{text}");
    }

    /// And a `sed` preview still renders as a diff *with no call context*: a
    /// replayed result arrives without a preceding `display_tool_call`, so the
    /// gate has to rest on the result's own banner rather than on panel state.
    #[test]
    fn a_sed_preview_renders_without_a_call_context() {
        let (out, _reasoning) = capturing_output(80);
        out.begin_capture();
        out.display_tool_result(
            "sed preview: 1 block for src/x.rs (1 expression). Review the blocks below, \
             then pass them to the `edit` tool as its `blocks` argument.\n\
             src/x.rs:5\n@@@OLD\n    run();\n@@@NEW\n    walk();\n@@@END\n",
        );
        let bytes = out.end_capture();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));

        assert!(
            text.contains("@@ -5,1 +5,1 @@"),
            "no anchored hunk:\n{text}"
        );
        assert!(text.contains("-     run();"), "no removed line:\n{text}");
        assert!(text.contains("+     walk();"), "no added line:\n{text}");
        assert!(text.contains("1 block in src/x.rs"), "banner lost:\n{text}");
    }

    #[test]
    fn tmp_repro_separator_blanks() {
        let out = Content::new(
            TermIO::plain(),
            crate::settings::default_show_reasoning_handle(),
        );
        out.begin_response();
        out.begin_capture();

        // Case A: turn ends with a tool result (no trailing answer text).
        out.stream_text("Let me sleep.\n");
        out.end_response_segment();
        out.display_tool_call("bash", &serde_json::json!({"command": "sleep 30"}));
        out.display_tool_result("\"2. sleep 30 — done\"");
        out.end_response();
        out.display_separator();
        let a = out.end_capture();
        let a = strip_ansi(&String::from_utf8_lossy(&a));
        eprintln!("=== Case A (tool-result end) ===\n{:?}", a);

        // Case B: turn ends with plain text answer.
        out.begin_response();
        out.begin_capture();
        out.stream_text("All done.\n");
        out.end_response();
        out.display_separator();
        let b = out.end_capture();
        let b = strip_ansi(&String::from_utf8_lossy(&b));
        eprintln!("=== Case B (text end) ===\n{:?}", b);

        // Case C: turn ends with trailing thinking after a tool result.
        out.begin_response();
        out.begin_capture();
        out.stream_text("Let me sleep.\n");
        out.end_response_segment();
        out.display_tool_call("bash", &serde_json::json!({"command": "sleep 30"}));
        out.display_tool_result("\"2. sleep 30 — done\"");
        out.stream_thinking("wrapping up\n");
        out.end_response();
        out.display_separator();
        let c = out.end_capture();
        let c = strip_ansi(&String::from_utf8_lossy(&c));
        eprintln!("=== Case C (tool + trailing thinking) ===\n{:?}", c);
    }

    /// The invariant: a multi-file batch must render exactly as N single-file
    /// batches would. The call halves come from the same split the multi-file
    /// path uses, so the only thing under test is that the view layer treats
    /// them as the same sequence of call-result pairs.
    #[test]
    fn a_multi_file_batch_renders_identically_to_n_single_file_batches() {
        let blob = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                    b.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        let multi_args = serde_json::json!({ "blocks": blob });
        let singles = multi_file_blobs(&multi_args).expect("multi-file blob");
        assert_eq!(singles.len(), 2);

        let notes = [
            "Replaced 1 block(s) in ./a.rs",
            "Replaced 1 block(s) in ./b.rs",
        ];
        let array = bsed_blockedit::render_file_results(
            &["a.rs", "b.rs"]
                .iter()
                .zip(notes)
                .map(|(path, note)| bsed_blockedit::FileResult {
                    path: (*path).into(),
                    result: Value::String((*note).into()),
                })
                .collect::<Vec<_>>(),
        );

        // A: two single-file pairs, as a sequence of real calls would render.
        let (a, _) = capturing_output(40);
        a.begin_capture();
        for (i, single) in singles.iter().enumerate() {
            a.display_tool_call("edit", single);
            a.display_tool_result(notes[i]);
        }
        let a = strip_ansi(&String::from_utf8_lossy(&a.end_capture()));

        // B: one multi-file pair.
        let (b, _) = capturing_output(40);
        b.begin_capture();
        b.display_tool_call("edit", &multi_args);
        b.display_tool_result(&array);
        let b = strip_ansi(&String::from_utf8_lossy(&b.end_capture()));

        assert_eq!(
            a, b,
            "a multi-file batch must render as N single-file batches"
        );
        // And the rendering this converges on is the real one: a label, a
        // unified diff, and the per-file note — once each, in pair order. (No
        // range here: `edit_preview` derives one by finding `old_text` in the
        // file, and these files do not exist. The diff comes from the edits
        // alone, so it renders regardless.)
        for part in [
            "edit a.rs",
            "@@ -1 +1 @@",
            "Replaced 1 block(s) in ./a.rs",
            "edit b.rs",
            "Replaced 1 block(s) in ./b.rs",
        ] {
            assert!(a.contains(part), "expected {part:?} in:\\n{a}");
        }
        assert_eq!(
            a.matches("Replaced 1 block(s) in").count(),
            2,
            "one note per file:\\n{a}"
        );
    }

    #[test]
    fn a_multi_file_result_renders_one_batch_per_file() {
        // The multi-file result is a bare array; it must render as one labelled
        // batch per file — a failure's short ui_view and a success's note — with
        // no aggregate line and no model_view diff leaking into scrollback.
        let out = Content::new(
            TermIO::plain(),
            crate::settings::default_show_reasoning_handle(),
        );
        out.begin_capture();
        out.display_tool_result(
            r#"[{"path":"src/a.rs","result":"Replaced 1 block(s) in ./a.rs"},
                {"path":"src/b.rs","result":{"ui_view":"edit failed: nope","model_view":"SECRET DIFF"}}]"#,
        );
        let d = strip_ansi(&String::from_utf8_lossy(&out.end_capture()));
        assert!(d.contains("edit src/a.rs"), "{d}");
        assert!(d.contains("Replaced 1 block(s) in ./a.rs"), "{d}");
        assert!(d.contains("edit src/b.rs"), "{d}");
        assert!(d.contains("edit failed: nope"), "{d}");
        assert!(!d.contains("SECRET DIFF"), "model_view must not leak: {d}");
        // No aggregate roll-up line.
        assert!(!d.contains("2 of"), "{d}");
    }

    #[test]
    fn an_array_of_plain_strings_is_not_a_multi_file_result() {
        // Another tool's list must keep its existing rendering; the multi-file
        // branch must not swallow it.
        let out = Content::new(
            TermIO::plain(),
            crate::settings::default_show_reasoning_handle(),
        );
        out.begin_capture();
        out.display_tool_result(r#"["alpha","beta"]"#);
        let d = strip_ansi(&String::from_utf8_lossy(&out.end_capture()));
        assert!(d.contains("alpha") && d.contains("beta"), "{d}");
    }

    #[test]
    fn tool_result_followed_by_text_has_blank_separator() {
        // Regression (replay path): render_turn must fire `end_response_segment`
        // after a tool block when more text follows, so replay output matches
        // the live path (where each CompletionCall flushes) — the trailing text
        // is separated from the tool-result line by a blank line.
        let out = Content::new(
            TermIO::plain(),
            crate::settings::default_show_reasoning_handle(),
        );
        out.begin_response();
        out.begin_capture();
        out.stream_text("Let me edit.\n");
        out.end_response_segment();
        out.display_tool_call(
            "edit_file",
            &serde_json::json!({"path": "/no/such.rs", "oldText": "a", "newText": "b"}),
        );
        out.display_tool_result("\"Replaced 1 block(s) in ./no_such.rs\"");
        // The fix: segment boundary after the tool block (mirrors live path).
        out.end_response_segment();
        out.stream_text("Done.\n");
        out.end_response();
        let d = out.end_capture();
        let d = strip_ansi(&String::from_utf8_lossy(&d));
        let after = d.find("Replaced 1 block(s) in ./no_such.rs").unwrap()
            + "Replaced 1 block(s) in ./no_such.rs".len();
        let between = &d[after..d.find("Done.").unwrap()];
        assert!(
            between.contains("\n\n"),
            "expected a blank line between the tool result and trailing text, got {:?}",
            between
        );
    }

    #[test]
    fn write_tool_renders_content_like_read() {
        // Write tool result should render with path header and syntax-highlighted
        // content preview (first 10 lines), matching read_file's rendering.
        let out = Content::new(
            TermIO::plain(),
            crate::settings::default_show_reasoning_handle(),
        );
        out.begin_response();
        out.begin_capture();

        // Simulate a write tool call with structured result (as write_file returns)
        out.display_tool_call(
            "write_file",
            &serde_json::json!({"path": "test.rs", "content": "fn main() {}"}),
        );
        let result = r#"{"path":"./test.rs","content":"fn main() {\n    let x = 42;\n    println!(\"Hello, world! {}\", x);\n}","lines":4,"bytes":72}"#;
        out.display_tool_result(result);

        out.end_response();
        let captured = out.end_capture();
        let output = strip_ansi(&String::from_utf8_lossy(&captured));

        // Should contain the path header with line count
        assert!(
            output.contains("write ./test.rs [4 lines]"),
            "missing path header with line count: {}",
            output
        );
        // Should contain syntax-highlighted content (first 10 lines)
        assert!(output.contains("fn main()"), "missing fn main: {}", output);
        assert!(
            output.contains("let x = 42"),
            "missing let x = 42: {}",
            output
        );
        assert!(output.contains("println!"), "missing println!: {}", output);
        // Should NOT contain line numbers (unlike read_file which has them in raw result but stripped for display)
        assert!(
            !output.contains("|"),
            "should not contain line number gutter: {}",
            output
        );
    }
}

#[cfg(test)]
mod ui_render_scope_tests {
    //! The model-facing rewrite must not change what the human sees.
    //!
    //! The failure envelope carries two views; only `ui_view` reaches the panel.
    //! This pins that a real failure renders its `ui_view` and never its
    //! `model_view` — whatever file content that now contains.

    use super::*;
    use crate::ansi::strip_ansi;
    use crate::settings::default_show_reasoning_handle;

    fn render(result: &str) -> String {
        let out = Content::new(TermIO::plain(), default_show_reasoning_handle());
        out.begin_capture();
        out.display_tool_result(result);
        let d = strip_ansi(&String::from_utf8_lossy(&out.end_capture()));
        d
    }

    /// The panel word-wraps at the terminal width, so a long `ui_view` arrives
    /// broken across lines. Collapse whitespace before asserting on content —
    /// otherwise the test measures the terminal width, not the render split.
    fn unwrapped(d: &str) -> String {
        d.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn a_failed_edit_renders_only_its_ui_view() {
        let envelope = serde_json::json!({
            "ui_view": "edit failed:\n  edit #1 oldText not found. Closest match at file lines 2-5.",
            "model_view": "1 of 1 edit(s) failed.\n\n--- edit #1 ---\n\
                Your block `ctx.txt:1` failed: @@@OLD lines not found. \
                Closest match at file lines 2-5:\n   2|     let alpha = 1;\n   3|     let beta = 2;"
        });
        let d = render(&envelope.to_string());

        let flat = unwrapped(&d);
        // The human gets the short summary...
        assert!(
            flat.contains("edit #1 oldText not found. Closest match at file lines 2-5."),
            "{d}"
        );
        // ...and none of the file content, the origin string, or the block
        // syntax the model is being taught.
        assert!(!flat.contains("let alpha = 1;"), "file content leaked: {d}");
        assert!(!flat.contains("ctx.txt:1"), "origin leaked: {d}");
        assert!(!flat.contains("@@@OLD"), "model wording leaked: {d}");
        assert!(!flat.contains("Your block"), "model wording leaked: {d}");
    }
}
