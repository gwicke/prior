//! The `Content` handle (step 3c of the output-drawing refactor): the agent-
//! facing owner of the transcript's content. It holds the content boundary
//! state, the width watch and the streaming renderers, formats content at the
//! watched width, and writes through the byte sink — which lives inside the
//! frame hub it holds (steps 4b/4c).
//!
//! It also holds the frame hub (steps 4b/4c): `RenderManager` runs the frame
//! pipeline and has owned the `Draw` sink since 4c. Since 4d every write
//! this layer produces leaves as a `ContentWrite` handed to that hub
//! (`with_content_writer` → `render_content`/`render_batch`) — the sink is
//! never named here. Of the frame *verbs*, `frame_size` lives here since 4f-c
//! (the watch this handle carries *is* the dims source), `painted_size` is the
//! manager's (`RenderManager::painted_size`), `cursor_row` is the manager's
//! with `Output` forwarding, and `draw_frame` — the test drivers' edge — stays
//! on `Output` until step 5.

use std::io::{self, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use console::style;
use serde_json::Value;
use tokio::sync::watch;

use crate::ansi::{at_line_start, EscState, HoldTrailing, LineClear, StripLeading, WrapLines};
use crate::content;
use crate::content::markdown::{
    answer_theme, command_lang, guess_lang, highlight_code_fg, render_markdown,
};
use crate::content::preview::{get_preview, multi_file_blobs, sed_preview_body, strip_read_lineno};
use crate::content::text::wrap;
use crate::output::Output;
use crate::render::ContentWrite;
use crate::row_count::RowCounter;
use crate::settings::{ShowReasoning, ShowReasoningHandle};
use crate::termio::{TermIO, TermSize};
use markdown_stream::{Parser, StreamParser};
use markdown_terminal::{Renderer, Theme};

use super::composition::ContentBoundary;
use super::{boundary_spacer, separator_line, ContentDisplayKind};

/// The agent-facing content owner (step 3c).
///
/// The fields are `pub(crate)` while the content methods still live on
/// `ConsoleOutput` and reach them through `self.content.…`; they tighten to
/// private once those methods move onto `Content` (3c-b).
pub struct Content {
    /// The frame hub (steps 4b/4c): where the frame pipeline *and* the
    /// drawing edge live — allocate → render → apply → remember, with the
    /// `Draw` sink under the manager's roof since 4c. `Arc<Mutex<…>>` is the
    /// design's handle shape (§Render: UI and content both hold one); today
    /// the only holder is this `Content`, reached by `ConsoleOutput` through
    /// it. Since 4d every content write reaches the screen as a
    /// `ContentWrite` handed to this hub (`render_content`/`render_batch`)
    /// — nothing here touches the sink. Everything that writes locks this
    /// first.
    pub(crate) manager: Arc<Mutex<crate::render::RenderManager>>,
    /// This instance's view of the size watch — the same channel `draw`
    /// holds (it came from the same `TermIO`, seeded at construction), so
    /// the content path's width reads and the frame path's geometry cannot
    /// disagree. In tests, a private seeded one.
    pub(crate) size: watch::Receiver<TermSize>,
    /// Per-stream capabilities, probed once at `TermIO` construction
    /// (4f-e-a): colour and the reasoning routing read this instead of
    /// calling `is_terminal()` per site — the probe site left content with
    /// this step.
    pub(crate) caps: crate::termio::Caps,
    /// Where reasoning goes: `true` = stderr, so a consumer of
    /// `picocode "q" | cat` can drop it (`2>/dev/null`) or merge it (`2>&1`)
    /// without touching stdout. Decided once, in [`Self::new`]. `AtomicBool`
    /// rather than `bool` only because the handle is now shared behind an
    /// `Arc` (step 3d) and a test pins it to the screen for a byte comparison —
    /// see [`Self::set_reasoning_on_stderr`].
    pub(crate) reasoning_on_stderr: AtomicBool,
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
    /// The content boundary/separator state (step 3a): the last declared kind,
    /// its segment-closed flag, and the held blank rows.
    pub(crate) boundary: ContentBoundary,
    /// Whether a turn is in flight — the bar dot's only state, flipped in
    /// `begin_response`/`end_response`. Owned by content (the path that knows
    /// when a response starts/ends); the frame reads it via a shared handle.
    pub(crate) streaming: Arc<AtomicBool>,
    /// The status bar's raw data (step 4f-a, design §UI): pushes store *facts*
    /// ([`crate::agent::status::BarStatus`]), never formatted text — the bar
    /// element pulls this and runs the shaping and the fit in its own render.
    /// Beside `streaming`, which it is shared the same way (the element
    /// clones the `Arc`).
    pub(crate) bar: Arc<Mutex<crate::agent::status::BarStatus>>,
    /// The content seam (impl doc §4): when set, every content verb hands the
    /// call to this handle and returns without touching the sink — the
    /// `RecordingOutput` / `QuietOutput` patterns, covered without a live
    /// render stack behind them. `None` in production, where `ConsoleOutput`
    /// holds this `Content` by `Arc` and its own content methods forward here.
    pub(crate) tap: Option<Arc<dyn Output>>,
}

impl Content {
    /// Build the content half from its per-process inputs: the terminal
    /// backend (4f-e — constructed first at setup, so *it* seeds both facts
    /// content used to gather itself: the caps it probed, and the size watch
    /// it owns). The frame hub is built from the same watch and takes the
    /// `TermIO` under `Draw` — one wiring point, one geometry source; the
    /// renderers start blank at a placeholder width.
    pub(crate) fn new(term_io: TermIO, show_reasoning: ShowReasoningHandle) -> Self {
        let size = term_io.subscribe_size();
        Self::new_from(term_io, size, show_reasoning)
    }

    /// [`Self::new`] with the geometry channel chosen by the caller — the
    /// test seam behind `ConsoleOutput::with_size`, where a privately seeded
    /// watch replaces the instance's own (the manager and `Draw` read the
    /// seeded one; publishes to `term_io`'s watch go where they always did
    /// relative to a pinned test: nowhere the test reads).
    pub(crate) fn new_from(
        term_io: TermIO,
        size: watch::Receiver<TermSize>,
        show_reasoning: ShowReasoningHandle,
    ) -> Self {
        let caps = term_io.caps();
        // The hub is born with its geometry and its budget ratio (step 4b):
        // `configure` is the ratio's home, and this is the one wiring point.
        let manager = Arc::new(Mutex::new(crate::render::RenderManager::new(
            size.clone(),
            term_io,
        )));
        manager
            .lock()
            .unwrap()
            .configure(crate::render::FRAME_MAX_ROW_RATIO);
        Self {
            manager,
            size,
            caps,
            reasoning_on_stderr: AtomicBool::new(!caps.primary),
            parser: Mutex::new(StreamParser::new_gfm_stream()),
            renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            thinking_parser: Mutex::new(StreamParser::new_gfm_stream()),
            thinking_renderer: Mutex::new(Renderer::new(Theme::no_color(), 80)),
            show_reasoning,
            last_tool: Mutex::new(None),
            boundary: ContentBoundary::new(),
            streaming: Arc::new(AtomicBool::new(false)),
            bar: Arc::new(Mutex::new(crate::agent::status::BarStatus::default())),
            tap: None,
        }
    }

    /// A `Content` whose content verbs delegate to `tap` instead of rendering
    /// (impl doc §4's test seam). `RecordingOutput` records the source strings
    /// an assertion compares; `QuietOutput` discards them; neither needs a
    /// render stack or a terminal behind it.
    ///
    /// The geometry is this instance's own Plain backend, not a private
    /// watch: with the verbs short-circuited there is no rendering to get
    /// wrong, and a test that needs a pinned width renders through
    /// `ConsoleOutput::with_size` as it always has. Plain is the backend a
    /// tapped content deserves — no reader, no raw mode — and the `Draw`
    /// underneath it (which no tapped verb reaches) still has a sink to own.
    pub fn with_tap(tap: Arc<dyn Output>) -> Self {
        let mut content = Self::new(
            TermIO::plain(),
            crate::settings::default_show_reasoning_handle(),
        );
        content.tap = Some(tap);
        content
    }

    /// Test knob: pin where reasoning goes, so a capture harness sees it on
    /// the captured stream (a test's stdout is not a terminal, and the default
    /// would route it to stderr). Production decides once in [`Self::new`].
    #[cfg(test)]
    pub(crate) fn set_reasoning_on_stderr(&self, on: bool) {
        self.reasoning_on_stderr
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// The size frames are composed and applied for (ui_draw_v2 §9 #3) — the
    /// watched geometry, floored at 20 columns / 1 row (`plan_apply`
    /// misbehaves under 20 columns; an uninitialized geometry must not get
    /// there). `frame_size` left `Output` in 4f-c; since 4f-e-a it is also
    /// the out-of-crate driver's answer (the confirm probe's `dims`, which
    /// the retired `term_width`/`term_height` free functions used to
    /// serve), hence `pub`. This handle's receiver is the source, the same
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
        // No wrapping here: `with_content_writer`'s `WrapLines` is the one
        // choke every writer passes — batch strings, streamed renderer
        // output, everything — so a line can no longer reach the terminal
        // wider than the terminal.
        let width = self.width();
        self.with_content_writer(width, kind, closes, |out| {
            let _ = out.write_all(bytes);
        });
    }

    /// Run `f` with this write's bytes collected through the boundary/hold/
    /// wrap/line-clear chain, then hand the result to the hub as a
    /// [`ContentWrite`] (step 4d, design §Content) — the one way content
    /// reaches the screen. For the writers that produce their own bytes —
    /// the streaming markdown renderers, which want a `Write` to feed.
    ///
    /// `f` gets a `&mut dyn io::Write` (the chain's head), which keeps the
    /// counter's type out of the signature. A caller handing it to
    /// something generic over `W: Write` — `Renderer::feed`,
    /// `commit_thinking` — passes `&mut out`, which is itself a `Write` and
    /// satisfies the `Sized` bound that `&mut dyn Write` cannot.
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
    #[allow(clippy::drop_non_drop)]
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
        // The hub's lock is the sink's lock now (step 4c): one guard across
        // the whole write, exactly the span the old writer lock covered —
        // same non-reentrancy (a closure that called back into a content
        // write would deadlock, as it always would have), one level up.
        let mut mgr = self.manager.lock().unwrap();
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
        // not against a screen — tapped at the hand-off below, which is why
        // the transforms step aside while one is open.
        let capturing = mgr.is_capturing();
        let mut buf: Vec<u8> = Vec::new();
        let mut clear = LineClear {
            inner: &mut buf,
            pending: true,
            enabled: !capturing,
        };
        // The counter sits *below* the line-clear (the codes are added to the
        // stream after it counts) and *above* the buffer, so it counts the
        // bytes as the terminal will receive them. The consume's walk, when
        // there is one, still goes straight to the writer, ahead of all of
        // this: it is cursor movement, not content, and never passes through
        // the counter or the clear.
        let mut counter = RowCounter::new(&mut clear, width);
        // The width guard goes between the hold and the counter: held
        // newlines reset its column, its inserted newline reaches the
        // counter (counted as the row move it replaces) and the line-clear
        // (armed for the continuation row). See `WrapLines`.
        let mut wrap = WrapLines {
            inner: &mut counter,
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
        let rows = counter.rows();
        let _ = counter.flush();
        drop(counter);
        drop(clear);
        // Nothing to hand over: no rows moved, nothing consumed, no
        // reposition owed — and the frame stays *live*, so the next compose
        // can still diff it. The kind/hold bookkeeping above already ran: an
        // empty write is still a statement about the boundary.
        if buf.is_empty() {
            return r;
        }
        // The hand-off (step 4d, design §Content/§Render): the block's
        // bytes, its measured rows and the width it was rendered at —
        // everything the screen choreography needs, and nothing about
        // *what* it is. Content never touches the sink from here on;
        // `RenderManager` consumes the live frame, writes the block,
        // advances the cursor and brings the frame back below.
        let write = ContentWrite {
            bytes: buf,
            rows,
            width,
        };
        // One choreography, two design verbs (§Render): the replay path
        // writes inside a batch and hands off as `render_batch`, the
        // streaming path as `render_content` — today one body under both
        // names, the batch flag inside deferring the reposition (ui_draw_v2
        // R5) either way. The guard moved to the hub in 4e; the deferral
        // stays the shared flag's until the final `render()` round trip
        // (4f/4h) gives `render_batch` its own end.
        if mgr.batching() {
            mgr.render_batch(write);
        } else {
            mgr.render_content(write);
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
        *renderer = Renderer::new(answer_theme(self.caps.primary), width);
        let mut trenderer = self.thinking_renderer.lock().unwrap();
        *trenderer = Renderer::new(Theme::dimmed(), width);
    }

    /// Store the status bar's raw data (step 4f-a). The push carries facts,
    /// never shaped text: the bar element formats whatever stands here at its
    /// next render — shaping and fit both — so a figure refreshed mid-turn is
    /// current at the next paint without a re-shaping push.
    ///
    /// A store, not a write: nothing reaches the sink, so the tap seam has
    /// nothing to intercept (the verbs that do reach the sink are the ones
    /// that forward to it).
    pub(crate) fn set_status(&self, status: crate::agent::status::BarStatus) {
        *self.bar.lock().unwrap() = status;
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
                self.with_content_writer(width, Some(ResponseReasoning), true, |out| {
                    let _ = out.write_all(&thinking);
                });
            }
            (false, false) => {
                self.with_content_writer(width, Some(ResponseReasoning), false, |out| {
                    let _ = out.write_all(&thinking);
                });
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
        let files = blockedit::parse_file_results(result)?;
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
        // Dark-grey panel box (no `⏺`, no tree line-art). The command label is
        // the first line and is left flush; its output — the trailing
        // diff/preview lines — is indented one column so the command's output
        // reads as a distinct block. The command itself is syntax-highlighted
        // through the same formatter the answer/result path uses (flushed per
        // block via a fresh fenced code render).
        let color = self.caps.primary;
        let label_styled = highlight_code_fg(&label, command_lang(name), color, self.width());
        self.paint_panel(&label_styled, false);
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
        cleaned = blockedit::strip_arg_error_prefixes(cleaned);

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
            let avail = width.saturating_sub(1).max(1);
            if let Some(table) = blockedit::arg_error_view::render_arg_error_table(cleaned, avail) {
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
                    // Show the path as a header with line count
                    let color = self.caps.primary;
                    let lang = guess_lang(path);
                    let header = format!("write {} [{} lines]", path, total_lines);
                    let header_styled = highlight_code_fg(&header, "bash", color, self.width());
                    self.paint_panel(&header_styled, true);

                    // Render the content with syntax highlighting (like read_file)
                    let show_max = 10; // Limit preview to 10 lines (match read_file)
                    let width = self.width();
                    let inner = width.saturating_sub(1).max(1);
                    let content_lines: Vec<_> = content.lines().collect();
                    for (printed, line) in content_lines.iter().enumerate() {
                        if printed >= show_max {
                            break;
                        }
                        for wline in wrap(line, inner) {
                            let styled = highlight_code_fg(&wline, lang, color, self.width());
                            self.paint_panel(&styled, true);
                        }
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

        let color = self.caps.primary;
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
        let inner = width.saturating_sub(1).max(1);
        for (printed, line) in lines.iter().enumerate() {
            if printed >= show_max {
                break;
            }
            // `read_file` renders `NNNN| content`; drop the gutter for display
            // (the model still gets the numbers in the raw tool result).
            let visible = if is_read {
                strip_read_lineno(line)
            } else {
                line
            };
            for wline in wrap(visible, inner) {
                if is_error {
                    self.tool_panel(&wline, true, true);
                } else {
                    // Syntax-highlight through the result-path formatter, then
                    // layer the grey tool panel on top (flushed per line).
                    let styled = highlight_code_fg(&wline, lang, color, self.width());
                    self.paint_panel(&styled, true);
                }
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
        if let Some(tap) = &self.tap {
            tap.display_committed_prompt(line);
            return;
        }
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
        if let Some(tap) = &self.tap {
            tap.display_plain(text);
            return;
        }
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
        if let Some(tap) = &self.tap {
            tap.display_markdown(text);
            return;
        }
        let color = self.caps.primary;
        let rendered = render_markdown(text, self.width(), color);
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
        if let Some(tap) = &self.tap {
            tap.display_text(text);
            return;
        }
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

    /// One line of non-REPL input — the piped/batch `/`-driver's read (4f-d,
    /// where the duty left `Output::get_user_input`'s call site). Like every
    /// content verb it short-circuits through the tap when one is installed
    /// (quiet's `QuietOutput` answers empty, a test's script answers its
    /// script); without one it writes the `❯ ` prompt through the counter —
    /// zero rows, but it closes any blank line — and reads the line.
    pub(crate) fn get_user_input(&self) -> String {
        if let Some(tap) = &self.tap {
            return tap.get_user_input();
        }
        self.write_content(format!("{} ", style("❯").bold().blue()).as_bytes());
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        input.trim().to_string()
    }

    pub(crate) fn begin_response(&self) {
        if let Some(tap) = &self.tap {
            tap.begin_response();
            return;
        }
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
        let theme = answer_theme(self.caps.primary);
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
        if let Some(tap) = &self.tap {
            tap.stream_text(delta);
            return;
        }
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
            self.write_content_kinded(
                &at_line_start(&thinking),
                Some(ContentDisplayKind::ResponseReasoning),
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
        if let Some(tap) = &self.tap {
            tap.stream_thinking(delta);
            return;
        }
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
        // the boundary after the reasoning has nothing to close.
        let mut sink = self.reasoning_sink();
        let mut parser = self.thinking_parser.lock().unwrap();
        let events = parser.write(delta.as_bytes());
        let mut renderer = self.thinking_renderer.lock().unwrap();
        let _ = renderer.feed(&events, &mut sink);
        sink.commit(self);
    }

    pub(crate) fn end_response_segment(&self) {
        if let Some(tap) = &self.tap {
            tap.end_response_segment();
            return;
        }
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
        if let Some(tap) = &self.tap {
            tap.end_response();
            return;
        }
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
        if let Some(tap) = &self.tap {
            tap.display_tool_call(name, args);
            return;
        }
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
        if let Some(tap) = &self.tap {
            tap.display_tool_result(result);
            return;
        }
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
        if let Some(files) = blockedit::parse_file_results(result) {
            let color = self.caps.primary;
            for fr in files {
                let label =
                    highlight_code_fg(&format!("edit {}", fr.path), "bash", color, self.width());
                self.paint_panel(&label, true);
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
        if let Some(tap) = &self.tap {
            tap.display_error(error);
            return;
        }
        // Measured: a long error wraps, and the message carries a 2-column
        // prefix the row count has to include.
        let block = format!("{} Error: {}\n", style("⏺").red(), error);
        self.write_content_kinded(
            &at_line_start(block.as_bytes()),
            Some(ContentDisplayKind::SystemMessage),
        );
    }

    pub(crate) fn display_system(&self, text: &str) {
        if let Some(tap) = &self.tap {
            tap.display_system(text);
            return;
        }
        let block = format!("{}\n", style(text).bold().dim());
        self.write_content_kinded(
            &at_line_start(block.as_bytes()),
            Some(ContentDisplayKind::SystemContent),
        );
    }

    pub(crate) fn display_separator(&self) {
        if let Some(tap) = &self.tap {
            tap.display_separator();
            return;
        }
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
        if let Some(tap) = &self.tap {
            tap.display_header(provider, model, yolo, limit, persona);
            return;
        }
        let block =
            content::text::header_block(provider, model, yolo, limit, persona, self.width());
        self.write_content(block.as_bytes());
    }

    /// Begin capturing everything content hands to the hub (§4: capture is
    /// content's, tapped at the hand-off — step 4d reads each
    /// `ContentWrite` exactly as `render()` receives it: content bytes
    /// only, never frame bytes). With a tap set there is nothing reaching
    /// the hub to capture, so the tap's own capture policy answers.
    pub(crate) fn begin_capture(&self) {
        if let Some(tap) = &self.tap {
            tap.begin_capture();
            return;
        }
        self.manager.lock().unwrap().begin_capture();
    }

    /// End a capture session and return what content handed over. See
    /// [`Self::begin_capture`].
    pub(crate) fn end_capture(&self) -> Vec<u8> {
        if let Some(tap) = &self.tap {
            return tap.end_capture();
        }
        self.manager.lock().unwrap().end_capture()
    }

    pub(crate) fn reasoning_sink(&self) -> ReasoningSink {
        ReasoningSink {
            buf: Vec::new(),
            to_stderr: self
                .reasoning_on_stderr
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// Buffered reasoning bytes awaiting routing. See
/// [`ConsoleOutput::reasoning_sink`].
pub(crate) struct ReasoningSink {
    buf: Vec<u8>,
    to_stderr: bool,
}

impl ReasoningSink {
    /// Route what has been written: stderr when there is no terminal, else the
    /// screen through the same counter every other content write uses (so the
    /// draw cache learns the height). Idempotent — the buffer is taken.
    fn commit(&mut self, out: &Content) {
        if self.buf.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.buf);
        if self.to_stderr {
            let mut err = std::io::stderr();
            let _ = err.write_all(&bytes);
            let _ = err.flush();
        } else {
            // Reasoning sits inside `begin_response`…`end_response`, and is a
            // kind of its own: it is the one kind whose blocks are dimmed, so
            // the blank row above a following text block is the boundary's and
            // not part of what the reasoning emitted.
            out.write_content_kinded(&bytes, Some(ContentDisplayKind::ResponseReasoning));
        }
    }
}

impl io::Write for ReasoningSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The content seam (impl doc §4): a tapped `Content` hands its verbs to
    /// the seam and never reaches its own sink — the `RecordingOutput` pattern,
    /// covered without a render stack behind it.
    #[test]
    fn tap_short_circuits_content_verbs_to_the_seam() {
        let (out, lines) = crate::output::RecordingOutput::new();
        let content = Content::with_tap(out);
        content.display_text("hello");
        content.display_separator();
        content.begin_response();
        content.stream_text("streamed");
        content.end_response();
        assert_eq!(
            crate::output::RecordingOutput::joined(&lines),
            "hello",
            "the seam sees the source strings, unrendered"
        );
        assert!(
            content
                .manager
                .lock()
                .unwrap()
                .draw()
                .cache
                .applied
                .is_none(),
            "a tapped verb must not move the real screen"
        );
    }

    /// Capture is content's (§4) and obeys the same seam: with a tap set there
    /// is nothing reaching the sink to capture, so the tap's own policy
    /// answers instead.
    #[test]
    fn tap_captures_through_the_seam() {
        let (out, _lines) = crate::output::RecordingOutput::new();
        let content = Content::with_tap(out);
        content.begin_capture();
        content.display_text("capped");
        assert!(
            content.end_capture().is_empty(),
            "RecordingOutput captures nothing"
        );
    }
}
