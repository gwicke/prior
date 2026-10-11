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
  vocabulary going down is `Stream` (main / meta).
- Holds a cloned width-watch handle (from TermIO at setup) and formats at the current watched width
  before each block — no hub round-trip — then hands a `ContentWrite { lines, stream }` to
  `render()`. `lines` are complete, pre-wrapped, styled screen rows; boundary, hold and line-clear
  (`EL`) bookkeeping stay here. Render re-checks the width and clamps / re-wraps (`WrapLines` with
  ANSI restore) if it disagrees. A new width applies from the next block on; the width-change replay
  stays the repl's, at its next size check — content never initiates it.
- Capture (`begin_capture`/`end_capture`) taps the `ContentWrite` as handed to `render()` —
  pre width-recheck, content only, never frame bytes — so `/replay Nl` and content-path tests
  compare what Content produced. Byte-edge tests use `TermIO::recording()`.
- No direct agent → output path remains: every write enters through render().

## UI layer

Compose UI chrome into a RenderElement stack. Same level as Content.

- source: ui/; files: ui.rs (menu chrome) · bar.rs (the bar)
- Elements are long-lived and reused: the status bar and the input hold live state (the bar its
  status, the input the shared editor), joined by transient elements (menus, dialogs) as repl/ui
  state changes. Reuse is why `Element` is `Arc<Mutex<…>>`, not `Box`: the repl mutates the shared
  editor / overlay cursor in place and the next render() paints it.
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

/// A content write handed to render(): complete styled rows + the stream.
/// All content-specific classification stays above this interface; below
/// it the only vocabulary is main / meta, so Render and Draw stay generic.
pub struct ContentWrite { lines: Lines, stream: Stream }

/// The stream token shared by Content → Render → Draw → TermIO.
pub enum Stream { Main, Meta }

pub struct RenderManager {
    stack: Vec<Element>,
    draw: Draw,        // fully owned drawing + output path
    size: SizeWatch,   // = watch::Receiver<TermSize>, cloned handle
}

impl RenderManager {
    fn configure(&mut self, frame_max_row_ratio: f32, frame_elements: Vec<Element>);

    // UI calls with content = None; content paths pass one write. Composes at
    // the watched size, hands frame + content to Draw. No return: width lives
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

- source: render.rs (existing composition, moved from ui/frame.rs)
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
    /// paint-forward per ui_draw_261001), then apply `frame` below it.
    pub fn draw(&mut self, frame: &Frame, content: Option<&ContentWrite>);

    /// Raw mode off. Resume is implicit in the next draw()/render(); the
    /// RenderManager re-anchors the caret (DSR, answered by TermIO).
    pub fn suspend(&mut self);
}
```

- files: draw.rs; ansi.rs (byte / width / escape transforms and the write wrappers)
- `DrawCache` (`ui/frame.rs:415`) keeps the frame-side state only: `applied`, `consumed_rows`,
  `deferred`, `batch_active`, the tracked caret and the clear floor — not the recipe; RenderManager
  recomposes each render(). Its content-side fields
  (`last_content_display_kind`, `last_content_closes_segment`) move up to the Content layer —
  boundary bookkeeping is content's, not the screen's. Writer + cache become this layer's fields,
  so the lock order is encapsulated once.
- Caps are cached here per stream from `TermIO::caps()` at construction — collapses the 7
  `is_terminal()` sites (1950…3779 in output.rs) into two decisions (primary, meta).
- Streams: `ContentWrite.stream` picks the sink — main → primary, meta → meta. The stream decision
  (e.g. reasoning → meta when meta carries it to stderr — caps, decided once at setup) is Content's.
  Frames target TTY primary only — no UI-on-stderr case.
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
    // Per-stream caps: primary (stdout) and meta (stderr) can disagree.
    fn caps(&self, s: Stream) -> Caps;
    fn write(&mut self, s: Stream, bytes: &[u8]) -> io::Result<()>;
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
  during a turn)
- Ctrl-C / Ctrl-Z handled here — process lifecycle, never queued
- DSR cursor queries answered internally: pause dispatch, write the query, match the reply. Only for
  exceptional events (startup, resume, reflow) — never per render. This replaces `confirm_gate` /
  `reader_paused` entirely (including `$EDITOR`'s hand-off); all five `Output::cursor_row()`
  re-anchor sites (repl ×4, editor ×1) dissolve into it.
- `suspend()` stops the reader and drops raw mode ($EDITOR, Ctrl-Z); `resume()` reverses it. Upper
  layers reach it only through the RenderManager / Draw pass-throughs.
- Two variants only. Constructors over Plain, not variants: `recording()` (memory writers,
  `#[cfg(test)]` — stripped bytes, frames dropped by caps) and `null()` (sinkhole). `--quiet` is not
  a backend: it selects Plain, whose answer is printed from `run_once`'s return (main.rs:366).
- Degradation: layers above always emit ANSI + OSC-8; a `StripAnsi` writer wrapper (per stream from
  caps) strips CSI/OSC-8 at the very edge — zero-width, so geometry is untouched. Non-tty primaries
  additionally drop frames wholesale at Draw.
- **Width is exposed because it cannot degrade late**: wrapping, table columns, panel and bar fit
  are decided at render time and destructive: a wrapped line cannot be unwrapped. ANSI is
  recoverable, geometry is not; height feeds only the frame budget.

## Related

- ./261003_output_drawing_refactor_impl.md — module layout, channels, capture, testing, migration
- doc/row_accounting.md
- ./ui_draw_261001.md
- ./261002_agent_refactor.md: similar refactor for `agent.rs`
