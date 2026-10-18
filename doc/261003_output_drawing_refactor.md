# `output.rs` / `ui/` — a drawing layer between content and the terminal

Status: **design only.** Describes the end state. Module layout, channels, capture, testing and
migration detail: `./261003_output_drawing_refactor_impl.md`.

## Goals

- Clear layering and code organization; one obvious place for each piece
- Clear control and content flow hierarchy

## Principles

- Direct calls go **down** the layers; bottom-up facts travel as **owned** channels / handles
  (`Receiver`, `watch::Receiver`, `Notify`) acquired once at setup — never a borrow of a lower
  layer, never a callback into the hub (non-reentrancy).
- Upper layers (UI, content) always render full ANSI + OSC-8 and know nothing about the backend;
  geometry is decided once, at the layers that own the size.
- A batch is repeated `render_batch()` calls (one write / block each) plus one frame re-apply with a
  regular `render()`.

## End state layering

```text
 Repl          Agent streaming
 UI               Content
 ------------------------
         Render
          Draw
 ------------------------
         TermIO
```

Render, Draw and TermIO (plus the width utilities and the `RenderElement` trait) live in their own
crate, `slashdot_tui`. `slashdot_menu` and picocode's UI/content layers depend on it — the layering
above is enforced by the dependency graph, not by convention. Two width measures live there:
`display_width` (CSI-only) and the OSC-8-aware `display_width_ansi` (with `wrap_to_width`), so links
and styling measure the same below both layers.

## End state UI flow

1) repl — control; consumes TermIO's input-event channel (keys/paste/resize); pulls data, assembles
   UI
2) ui — chrome; compose RenderElements, configure the render stack
3) render — compose RenderElements into a frame; accept content writes
4) draw — scrollback & frame drawing, Frame caching; merges content + frame
5) TermIO — tty / stdout / stderr adapters; input events; size watch

## End state Content flow

1) agent — streaming content; also: session for replays
2) content — formatting, at the current watched width
3) render — same as UI, shared from here down
4) draw
5) TermIO

## Content layer

Render content segments / blocks to TTY bytes. Same level as UI. It is the agent's rendering seam:
the agent holds a `Content` handle (never the hub); confirmation keeps its own `ConfirmPort`.

- source: content/*; files: text.rs, markdown.rs, panel.rs, preview.rs, composition.rs
- Owns all content classification: kinds (`ContentDisplayKind`), boundary / separator bookkeeping
  (today's separator matrix, `last_content_*`, `held_blank`). None crosses below: the only
  vocabulary going down is `Stream`, the write's kind (`Content` / `Reasoning` / `Error`) — the sink
  is TermIO's.
- Holds a cloned width-watch handle (from TermIO at setup) and formats at the current watched width
  before each block — no hub round-trip — then hands a `ContentWrite { bytes, stream }` to
  `render()`. `bytes` are complete, pre-wrapped, line-cleared screen rows ending at a line boundary;
  boundary, hold and line-clear (`EL`) bookkeeping stay here. The `\n` split **is** the row structure
  — no separate count rides with the write. Render owns the final guarantee: it forces the line
  terminator a block owes and re-wraps any row over the watched width, re-arming the clear on each
  continuation; where Draw's entry `EL` cannot be trusted, it pads a short row to `term_w` as a
  fallback. A new width applies from the next block on; the width-change replay stays the repl's, at
  its next size check — content never initiates it.
- Capture is Content's: `begin_capture`/`end_capture` tap the block's bytes at the output, and a
  capturing write never reaches `render()` — content only, never frame bytes — so `/replay Nl` and
  content-path tests compare what Content produced. Byte-edge tests use `TermIO::recording()`.
- Quiet is Content's too, a flag rather than a backend: `Content::quiet` builds the same `Content`
  and drops the hub hand-off (the bytes are still built), with `Stream::Error` the one kind that
  survives — it keeps the edge's routing (meta on a pipe, the primary when it is a terminal).
  Making the error kind always-meta inside `TermIO` is a deferred refinement. The non-REPL input
  read (the piped/batch driver) is `TermIO::read_line` (Plain answers; `Tty` reports `Unsupported`);
  the handle's `InputSource` picks the source — the edge in production, nothing under quiet, a
  script in tests.
- No direct agent → output path remains: every write enters through render().

## UI layer

Compose UI chrome into a RenderElement stack. Same level as Content.

- source: ui/ (picocode); bar.rs (the bar) — menu chrome elements come from `slashdot_menu`
  (below, RenderElement stack), which implements `RenderElement` for its views
- Elements are long-lived and reused: the status bar and the input hold live state (the bar the
  stores it reads, the input the shared editor), joined by transient elements (menus, dialogs) as
  repl/ui state changes. Reuse is why `Element` is `Arc<Mutex<…>>`, not `Box`: the repl mutates the
  shared editor / overlay cursor in place and the next render() paints it.
- The bar is **always present** — no on/off gate (the old `is_off`/`set_status` gate was an
  artifact of push, guarding a stale/absent pushed row; with pull the bar always renders itself).
  The bar **holds stores, not a snapshot**, and formats them at render (name fallback, field list,
  drop order, fit). Its facts come from three live sources read at render:
  - the model fields (provider/model/thinking/context_limit) off `AppState` — the same sync
    accessors `/stat` uses, so a mid-turn `/reasoning` or `/model` commit (which lands on
    `AppState` immediately) shows at the very next paint;
  - identity (session name / first message) off `AppState`, written by the loop's cycle build;
  - the context figure off the agent's shared `usage` mutex (`Arc<Mutex<SessionUsage>>`) — the
    provider's last-reported input tokens, so the agent's per-tool-call-round update is current at
    the next render.

  **The context figure has one fallback, shared by the bar and `/stat`:** the estimate. It lives in
  the usage store as `estimated_input_tokens`, and the figure read is
  `last_input_tokens if > 0 else estimated_input_tokens` — the estimate stands only until the first
  completion reports. `/stat` calls the same read, so it is current mid-turn and needs no
  `AppState` context snapshot.
- Confirmations are repl dialogs (session state). Batch / one-shot runs have no dialog: gated tools
  there require yolo mode (the sandbox is mandatory).

## Render layer

The shared hub between UI and content; both hold `Arc<Mutex<RenderManager>>` and call in. Composes
widgets into Frame, owns frame size — resolves `configure()`'s frame max ratio (e.g. 2/3 of the
vertical) into concrete rows — and re-renders on resize by re-running the installed stack at the
watched size.
Also the front door for suspend/resume, so content / UI never touch TermIO.

```rust
type Lines = Vec<String>;

pub struct Rendered { lines: Lines, caret: Option<(usize, usize)> } // caret: element-relative

// Generalization of the current usize must / might. Frame sizing uses the
// concrete row count; the in-frame split may also consult the ratio.
pub enum RowSpec {
    FixedNum(rows: usize),
    // Fraction of the frame area, capped at `rows` (e.g. header vs content split).
    // f32: an in-frame ratio is a preference about a share, resolved to integral
    // rows either way. It is not the budget's ratio — the frame's row count comes
    // from the manager's configured ratio alone, so there is nothing here for it
    // to agree with.
    RatioAndNum(ratio: f32, rows: usize),
}

pub struct RowAsk { min: RowSpec, max: RowSpec }

pub trait RenderElement {
    /// Min/max rows of the element at the given render width.
    fn height(&self, width: usize) -> RowAsk;
    /// Rows for an allocation of exactly `height` screen rows of `width`, plus
    /// the element's caret if it has one. Must not return more than `height`
    /// rows: the frame builds row ranges from the result, and nothing
    /// downstream can clamp an over-tall impl.
    fn render(&self, height: usize, width: usize) -> Rendered;
}

// Elements are reused across compositions (UI layer) — shared and dynamic.
type Element = Arc<Mutex<dyn RenderElement + Send + Sync>>;

/// A content write handed to render(): the block's bytes + its kind. The
/// `\n` split is the row structure; Render repairs the bytes (terminator,
/// width) and reads the row count back off them. All content-specific
/// classification stays above this interface; below it the only vocabulary
/// is the kind, so Render and Draw stay sink-generic.
pub struct ContentWrite { bytes: Vec<u8>, stream: Stream }

/// The kind of an output write, shared by Content → Render → Draw → TermIO.
/// Content says *what* a write is; TermIO maps the kind to a sink from caps:
/// Content → primary; Reasoning/Error → primary when it is a terminal, else
/// meta. Frame bytes are a separate TermIO target (`write_frame`), dropped on
/// a real non-terminal primary.
pub enum Stream { Content, Reasoning, Error }

pub struct RenderManager {
    stack: Vec<Element>,
    draw: Draw,      // fully owned drawing + output path
    size: SizeWatch, // = watch::Receiver<TermSize>, cloned handle
}

impl RenderManager {
    fn configure(&mut self, frame_max_row_ratio: f32, frame_elements: Vec<Element>);

    // UI calls with content = None; content paths pass one write. Composes at
    // the watched size, repairs the write (terminator, width) and reads its
    // row count, hands frame + content + rows to Draw. No return: width lives
    // on the size watch; the repl keys off resize events + the current size,
    // never off the applied size.
    fn render(&mut self, content: Option<ContentWrite>);

    // Bulk replay: one write / block per call; frame apply deferred
    // (DrawCache::batch_active), re-drawn by a final regular render().
    // Streaming stays on render() only. A missed final render() is benign:
    // the next render() re-applies the frame.
    fn render_batch(&mut self, content: ContentWrite);

    // Pass-through to Draw/TermIO: raw mode off, reader stopped. Resume is
    // implicit in the next render(), which re-anchors the caret itself (DSR
    // answered inside TermIO — the repl never issues one).
    fn suspend(&mut self);
}
```

- source: slashdot_tui (render.rs; existing composition, moved from ui/frame.rs)
- Allocation: frame size = min(max ratio × term height, sum of element concrete max asks); floors
  first, then a **ratio-weighted** split — all-`FixedNum` (today) weights evenly, i.e. today's
  semantics, pinned by `allocate_*`.
- Resize: frame re-render is watch-driven and lazy — only at the next render(), never on the event
  (today's delayed repaint, repl.rs:1761); mid-turn render() adapts on the fly. The scrollback
  *replay* is the repl's, at its next size check (TermIO section).

## Draw layer

Owns the screen; the only place content and frame meet. Called by, and owned by, RenderManager.

```rust
pub struct TermSize { width: usize, height: usize }

pub struct Frame { size: TermSize, rows: Lines, caret: Option<(usize, usize)> } // caret: frame-absolute

struct Draw {
    term_io: TermIO, // closed enum, not a trait object
    cache: DrawCache,
}

impl Draw {
    pub fn term_size(&self) -> TermSize;

    /// Append `content` to scrollback (consuming the frame's rows,
    /// paint-forward per ui_draw_261001), then apply `frame` below it. `rows`
    /// is the count Render read off the repaired bytes; Draw writes but never
    /// measures.
    pub fn draw(&mut self, frame: &Frame, content: Option<(&ContentWrite, usize)>);

    /// Raw mode off. Resume is implicit in the next draw()/render(); the
    /// RenderManager re-anchors the caret (DSR, answered by TermIO).
    pub fn suspend(&mut self);
}
```

- files: slashdot_tui (draw.rs; ansi.rs — byte / width / escape transforms and the write wrappers)
- `DrawCache` (`ui/frame.rs:415`) keeps the frame-side state only: `applied`, `consumed_rows`,
  `deferred`, `batch_active`, the tracked caret and the clear floor — not the recipe; RenderManager
  recomposes each render(). Its content-side fields
  (`last_content_display_kind`, `last_content_closes_segment`) move up to the Content layer —
  boundary bookkeeping is content's, not the screen's. Writer + cache become this layer's fields,
  so the lock order is encapsulated once.
- Caps never reach Content (or Render/Draw): content always renders as for a terminal — colours,
  OSC 8 links, syntax highlighting — at the watched width, and fidelity is the edge's decision
  (`TermIO` strips escapes per stream at the write, drops frames on a non-terminal primary). The
  probing and the `Caps { primary, meta }` value live inside TermIO alone.
- Kinds: `ContentWrite.stream` is the write's *kind*; `TermIO` owns the kind → sink map from caps
  (Content → primary; Reasoning/Error → primary when it is a terminal, else meta). Content decides
  no routing at all: every write — reasoning included — runs the boundary chain, carrying its kind,
  and the edge picks the sink. Frame bytes are a separate TermIO target (`write_frame`) and target
  the TTY primary only — no UI-on-stderr case.
- `output.rs` dissolves: content verbs enter through render(), drawing verbs are Draw's, interactive
  verbs are repl dialogs / TermIO's. The five no-op drawing stubs and their dummy `(w, h)` returns
  disappear with the verbs.

## TermIO layer

TTY and pipe adapters, owned by Draw. **Sole stdin owner**: one reader thread for the process
lifetime. It hands out owned channel handles at setup; nothing borrows it.

```rust
pub enum InputEvent { Key(/* crossterm for now */), Paste(String), Resize(TermSize) }
pub enum CaretEvent { Invalidated } // SIGCONT / resume

/// Acquired once at setup, then distributed. `size` is `Clone` (two consumers).
pub struct TermHandles {
    pub input: Receiver<InputEvent>,     // repl: keys / paste / resize
    pub size: watch::Receiver<TermSize>, // RenderManager (frame) + content (format width)
    pub caret: Receiver<CaretEvent>,     // RenderManager only
}

// Closed enum, not a trait: exactly two runtime behaviors, and TermIO is not
// an extension point. The variant picks the input / lifecycle side; caps adjust
// output fidelity independently. A non-tty stdin selects Plain; stdout/stderr
// caps are probed separately (`picocode "q" | cat` is Tty input with a non-tty
// primary — frames dropped, primary stripped).
pub enum TermIO {
    Tty(TtyIo),     // raw mode, reader thread, DSR; caps probed at construction
    Plain(PlainIo), // line stdin, plain stdout/stderr, no raw mode, non-tty caps
}

impl TermIO {
    fn handles(&mut self) -> TermHandles;
    // `caps()` is edge-internal (strip / frame-drop / kind → sink);
    // nothing above TermIO reads it.
    // Kind → sink from caps; the strip is per *target*.
    fn write(&mut self, s: Stream, bytes: &[u8]) -> io::Result<()>;
    // Frame bytes: the primary, dropped on a real non-terminal primary.
    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn size(&self) -> TermSize;
    fn suspend(&mut self) -> io::Result<()>;
    fn resume(&mut self) -> io::Result<()>;
}
```

- resize → both the `size` watch (RenderManager frame dims, content format width) **and** the
  `input` channel — the repl consumes it without acting and runs wipe + replay + re-anchor at its
  next size check — prompt entry after a turn or the next keystroke while idle (today repl.rs:1711;
  a mid-turn key repaints at live dims and can clear the mismatch first — kept, pumps.rs:85). Acting
  on arrival would repaint mid-drag, when the scroll state is arbitrary; a keystroke scrolls the
  cursor into view first, a timer cannot (repl.rs:1761).
- caret invalidation (SIGCONT, resume) → the `caret` channel to RenderManager only
- keys / paste → the `input` channel (repl pumps: `read_input` at the prompt, the turn's select
  during a turn). One exception, still at the edge: while an LLM window is open (the compaction
  summarization) the reader *diverts* forwarded events to the window's own inbox — a registered
  sink, not a second stdin reader — and the window parks what it cannot use back into the queue
  `read_input` drains. The reader never pauses for it.
- Ctrl-C / Ctrl-Z handled here — process lifecycle, never queued
- DSR cursor queries answered internally — **by the reader itself, between polls**: the main task
  posts a request to the reader's inbox and waits on the reply channel (bounded wait; an expired
  request is dropped unasked), so no second stdin reader ever exists to swallow the answer and the
  query needs no pause. Before the reader is spawned (the entry window) the main task is the sole
  reader and scans directly; Plain answers `None` without touching stdin. Only for exceptional
  events (startup, resume, reflow) — never per render. All `RenderManager::reanchor` sites
  (repl ×4, editor ×1, caret invalidation) are front-door calls whose answer never leaves Draw.
- `suspend()` stops the reader and drops raw mode ($EDITOR, Ctrl-Z); `resume()` reverses it. Upper
  layers reach it only through the RenderManager / Draw pass-throughs. The mode guard's own escapes
  (bracketed paste, kitty keyboard, cursor) are primary-stream writes: emitted only when the primary
  is a terminal, so a redirected primary (`picocode > out.txt`) does not collect them. Raw mode
  itself is unconditional — a stdin concern.
- Two variants only. Constructors over Plain, not variants: `recording_with(caps)` (a real backend
  with pinned caps; `recording()` is the screen-caps shorthand) and `null()` (sinkhole). `--quiet`
  is not a backend: it selects Plain, whose answer is printed from `run_once`'s return (main.rs:366).
- Degradation: layers above always emit ANSI + OSC-8; a `StripAnsi` wrapper (per *target* stream from
  caps) strips CSI/OSC-8 at the very edge — zero-width, so geometry is untouched. A real non-terminal
  primary additionally drops frames wholesale, now at `TermIO::write_frame`, and the mode guard's
  escapes (bracketed paste, kitty keyboard, cursor) gate on the same primary cap. A recording double's
  **caps** decide whether it degrades: screen caps keep the byte edge composed, pipe caps exercise the
  degradation; `null` is the one non-degrading sinkhole.
- **Width is exposed because it cannot degrade late**: wrapping, table columns, panel and bar fit
  are decided at render time and destructive: a wrapped line cannot be unwrapped. ANSI is
  recoverable, geometry is not; height feeds only the frame budget.

## RenderElement stack

The manager's `stack` is configured **by the repl**, not assembled per paint and not curated by the
manager: `configure` carries the whole membership, and presence, order and separators (blank
companion row, divider above a menu) are rules the repl expresses by what it puts in. The manager
budgets, calls `height`/`render` per element and merges the rows; it has no notion of bar, input or
menu.

- Elements are built **once, from raw data** — rows plus node config, never pre-painted rows — and
  pull their state, resolve and format inside `render(h, w)` at the width being composed for. A
  menu open constructs its elements; a keystroke moves the selection through shared handles in
  place — no push, no rebuild; rows that actually change (filter, new plan) rebuild the affected
  element and reconfigure the stack.
- **One element per view section**: a listing's detail, table header and scrollable rows are three
  entries in the stack, not a pre-composed sub-stack. Nothing below the repl stacks elements.
- The **input element is always in the stack**; height carries its presence: `0` mid-turn with an
  empty buffer (a mid-turn frame is an ordinary `render()` — the input simply contributes nothing),
  the prompt row between turns even with no content, sized to the wrapped buffer whenever content is
  present. Turn presence rides `Content::streaming` for now — a pushed fact, read at pull time.
- **Dims never travel**: no frame-path verb takes dims; `render()` composes and paints at the watched
  size, so a resize applies lazily at the next render. The repl's only dims use is to *initiate* a
  content replay at its next size check.
- `RowAsk`/`RowSpec` land with the stack, not later: `height(&self, width)` is what lets each element
  answer in rows-at-this-width. All-`FixedNum` stays today's even-split semantics, pinned by
  `allocate_*`.
- `slashdot_menu` mints the elements for its views (detail, header, rows) and the repl appends them
  to the stack. The rows element holds the view handle (`Arc<ViewRows>` — the crate's handle chain is
  `Arc` throughout, so it is `Send + Sync`) and paints through its iterator inside `render(h, w)`:
  filtered and unfiltered views resolve the same way, stale slots skip, and rows are never copied
  into the element.

## Related

- ./261003_output_drawing_refactor_impl.md — module layout, channels, capture, testing, migration
- doc/row_accounting.md
- ./ui_draw_261001.md
- ./261002_agent_refactor.md: similar refactor for `agent.rs`
