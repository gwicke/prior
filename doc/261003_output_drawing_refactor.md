# `output.rs` / `ui/` — a drawing layer between content and the terminal

Status: **design only.** Describes the end state.

## Goals

- Clear layering and code organization; one obvious place for each piece
- Clear control and content flow hierarchy

## End state layering

```text
Repl
UI               Agent streaming
Render           Content
------------------------
         Draw
------------------------
        Output
```

## End state UI flow

1) repl - control, pulls data from data layers assembles UI
2) ui - chrome; compose Elements; render to Frame, cursor ownership
3) draw - scrollback & frame drawing; Frame caching; request render from UI
4) output - tty and stdout adapters

## End state Content flow

1) agent - streaming content; also: session for replays
2) content - formatting
3) draw - same as UI, shared from here down
4) output

## Content layer

Render content stream segments / blocks to TTY bytes. Same level as UI.

- source: content/*; files: preview.rs · panel.rs · markdown.rs

Cleanups

- Remove: `console::set_colors_enabled()` is a process global that `style()` honours and that
  already respects `CLICOLOR`/`CLICOLOR_FORCE` (`console-0.15.11/src/utils.rs:44`).
  - `render_markdown(src, w, color)` takes the flag directly.

## UI layer

Compose and render UI chrome into Frames. Same level as Content.

- source: ui/
- files: ui.rs (menu chrome) · bar.rs (the bar)

## Render layer

Directly underneath UI. Composes and renders widgets into Frame. Owns frame size, e.g. the 2/3 of
vertical term size.

```rust
// New structure: Generalization of current usize must / might
enum RowAsk {
    FixedNum(rows: usize),
    // Fraction of *frame* vertical size, e.g. for a reasonable division of space
    // between header and content rows.
    VerticalRatio(ratio: f32),
}

struct Element {
    // Row space requested by the element 
    // Static so vertical stacking is only re-computed at configure() time,
    // not per draw
    // min/max replace must/might
    min: RowAsk,
    max: RowAsk,
    // Each element knows how to render itself
    /// `Box` rather than `Rc`: an element is owned by exactly one composition
    /// and is only ever moved between them, so the shared ownership would buy
    /// nothing. `Send + Sync` is not optional — `Output: Send + Sync` and
    /// `draw: Mutex<DrawCache>` put the composition behind both.
    render: Box<dyn RenderAt + Send + Sync>,
}

/// Stateful renderer
struct Renderer {
    stack: vec<Element>,
    // Both content and UI Renderer use the Drawer to paint to the shared screen.
    drawer: Arc<Mutex<Drawer>>,
}

impl Renderer {
    fn configure(&mut self, vec<Element>);
    fn render(&mut self, width: usize, height: usize) {
        // Takes into account screen height / width. Budgets
        // vertical space between elements.
        let frame: Frame = self.render_internal();
        // Submit frame for drawing. Consumes / moves
        self.drawer.lock().unwrap().draw(frame);
    }
}
```

- source: frame.rs (existing, composition)

## Draw layer

Owns the screen, do all drawing to the output. Merges content and ui.

```rust
pub struct Frame {
    width: usize,
    height: usize,
    frame: vec<Line>,
}

struct Draw {
    sink: Box<dyn Sink>,
    cache: DrawCache
}
impl Draw {
    
    /// Re-render the installed configuration
    /// force_refresh for ctrl-l / startup / external cursor writers
    ///   triggers DSR internally, forces full re-draw
    pub fn draw(&mut self, force_refresh: bool)
}
```

- files: draw.rs; maybe split out content / frame rendering specifics later output.rs Sink +
  Confirmation + the `Output` verb interface
- `DrawCache` (`ui/frame.rs:415`) is already the shared state needed `element_stack`, `applied`,
  `consumed_rows`, `deferred`, `batch_active` for the frame side; `last_content_display_kind`,
  `last_content_closes_segment` for the content side.

Utilities:

- ansi.rs: pure byte / width / escape transforms

## Output layer

TTY and pipe output variant adapter layer

```rust
pub trait Sink: Send {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
    // Pushing the TTY / non-TTY distinction up to Draw might be easier -
    // changes rendering model
    fn caps(&mut self) -> isTTY;
    // Output owns low level TTY interfacing
    fn screen_size(&self) -> Result({width, height});
    fn absolute_cursor_position(&self) -> Result({top, left}); // DSR
    // TODO: What is this about?
    fn event(&self, e: Event);     // LogOutput's structured path
}
```

- `ConsoleOutput`: full-featured sink for ttys
- `QuietOutput`: null sink + an interactive `confirm` on stderr/stdin
- `NoOutput`: null sink, auto-approve
- `RecordingOutput`: test double (`#[cfg(test)]`)

Non byte sinks

- `ScrollbackOutput`: **decorator** over `Arc<dyn Output>`, tapping verb *arguments*
- `LogOutput`: **not a byte sink** — a structured logger; never renders

`LineClass` exists because five of six impls carry no-op stubs for all 11 drawing verbs, returning
dummies like `(dims.0.max(1), dims.1.max(1))`. A non-tty sink drops `Frame` wholesale.

`Draw` holds **two** sinks: `reasoning_on_stderr` (1950) means content goes to stdout while
reasoning goes to stderr in the same run, so one `Box<dyn Sink>` is insufficient.
`begin_capture`/`end_capture` stop being verbs and become the writer swap they already are
(`saved_writer`).

`Caps` is never on `Draw`'s public surface. It is cached there from `Sink::caps()` at construction,
which collapses the **7 independent `is_terminal()` sites** (1950, 2990, 3098, 3229, 3268, 3516,
3779) into one decision.

- Render layers always target TTY with ANSI, OSC-8 - do not know about output
- Lower fidelity outputs strip ANSI / OSC-8 in the sink

**Width is exposed because it cannot degrade late.** Wrapping, table column widths, panel fit and
bar fitting are decided at render time and are destructive — a wrapped line cannot be unwrapped. So
the split is asymmetric on purpose: ANSI is recoverable, geometry is not. Raw height needs no
accessor: all five sites only feed `live_budget`, `DrawCache::new`, `reanchor` or `frame_size`.

### Output cleanups

- `ScrollbackOutput` is **dead code**: zero references outside its own `#[allow(dead_code)]` impl.
  `LogOutput` emits four semantic events at four `tracing` levels, one carrying the tool `name` and
  `args` structurally, which bytes cannot express.
- `confirm` alone has **three answers** across the impls (`Console`/`Log` → `No`, `No`/`Recording` →
  `Yes`) plus `Quiet`'s interactive prompt. That is interaction policy, not transport, so it stays
  on the verb layer.

## 3. The sink, the degradation rule, and `Draw`'s surface

### 3.1 Two paths, by decision

There is a **content path** and a **frame path**, and they are separate on purpose. An earlier shape
had `draw_frame` *return* the applied geometry, so a content write could read the frame's grant.
That was rejected: a return value ties content to whoever drew the frame, which routes content
writes through the repl layers.

Instead the drawing layer **holds** the frame configuration. `remember_compose` (3939) installs
`element_stack` + `applied` under one lock —
*"the recipe and the geometry it was rendered at describe one paint and must not be read apart"* —
and `reposition_area` re-renders from that install. So:

```rust
impl Draw {
    /// Configure the live frame and paint it. The installed configuration is
    /// what every later content write re-renders from, until the next call.
    fn configure_and_draw_frame(&mut self, elements: Vec<Element>, policy: ApplyPolicy);

    /// Re-render the installed configuration at its shifted rows, after
    /// `written` rows of content moved the seam. Elements that resolve at
    /// render time (the bar) pick up their new values; no caller involved.
    fn refresh(&mut self, written: usize);

    fn width(&self) -> usize;                 // every renderer
    fn dims(&self) -> (usize, usize);         // resize detection
    fn set_dims(&self, d: (usize, usize));    // the reader's resize branch
    fn painted_size(&self) -> (usize, usize); // already in the cache
}
```

`refresh` is `reposition_area` under its real name. Making it an entry point turns "a content write
refreshes the UI" from an accident into a contract.

### 3.2 `live_budget` goes private; `rows_grant` goes away

Vertical composition belongs to `configure_and_draw_frame`, so the `2/3` rule stops being a public
function. Of 12 `allocate` + `live_budget` call sites, 4 are production and
**8 are tests hardcoding `live_budget(24)`** (`ui.rs` ×5, `v25_model.rs`, `output.rs` ×2,
`repl.rs`). Under `Draw` those eight read a real budget instead of a copied literal — they change,
not just move.

`rows_grant` (701) is a **redundant query on a mechanism that already works**: its own doc says it
re-runs *"the same composition and the same `allocate` the frame will, so the two cannot differ"*,
and it exists only because the grant isn't otherwise obtainable. But widgets already own their
windows — `slashdot_menu` sends `RenderPlan`s of `LogicalLine`s for generic windowing and
cursor/active-marker rendering, driven by the row grant
(`window_heights(heights, selected, budget)`), and `StatusItem` likewise. The rows element is handed
`h` in `render_at(h, term_w)`. So `rows_grant` is deleted rather than replaced: no `measure`, and no
`Applied` return type.

### 3.3 `Sink`: where bytes land

## 4. What stays

`Content` implements the `Output` verb interface, so all ~20 `&dyn Output` call sites in
`interactive.rs`, `agent.rs`, `agent/turn.rs`, `repl.rs` and the tests are untouched. The 34-verb
trait keeps the 11 drawing methods for now (§5); the content half is `Content`'s.

`repl::read_input` is **drawing-only** — it calls exactly `cursor_row`, `painted_size`,
`set_status`. Splitting its signature is the one phase-2 change worth doing, and the table for it is
`agent/turn.rs:248,596,649` (all three `frame_size` uses feed the in-turn REPL input) plus
`set_status` at 187.

## 5. Deferred

- **Phase 2 — `Draw` out of the `Output` verb trait.** The 11 drawing verbs move to `Draw`'s own
  surface; `read_input` takes it instead of `&dyn Output`. This makes visible a fact the single
  trait currently hides: the turn loop is *both* a content producer and a frame driver, because
  in-turn input is a drawing operation.
- **`draw.rs` stays one module** (~2,900 lines after the move). Split `content`-side from
  `frame`-side branches only if it lands unwieldy.
- **Making content an `Element` citizen.** Not a refactor. `Element` rows are in-place-repaintable
  (`ui/frame.rs:17–20`) and content is append-only; that is a redesign of the streaming path, and it
  belongs in `doc/ui_draw_v2.md`.

## 6. Phase 0 — preliminary cleanup

`reset_renderer_gaps` (`output.rs:2590`) is called from exactly one place, `end_tool_block` (2652).
Both its effects are covered systematically elsewhere:

- **effect**: width refresh
  - **covered by**: `stream_text` (3588) — `set_width` per chunk, both renderers
- **effect**: drop `pending_gap`
  - **covered by**: `end_response_segment` (3665) — fresh answer renderer per segment

So the tool boundary is the last ad-hoc holderover. Remove it: `end_tool_block` becomes a pure
boundary declaration, and `Boundary::closes` then means *only* the content rule, with no renderer
mechanic fused in.

**This is the plan's one behaviour change.** It is also the one thing the suite does not cover — the
only `pending_gap` hits are comments (3679, 5564). Add a tool-boundary pin *first*, then delete the
reset. If the pin fails, we have found something.

## 7. Net

`output.rs` 6,624 → ~500 (`Sink`, `Confirmation`, the verb interface); `ui/frame.rs` 2,308 folds
into `draw.rs`; `ui.rs` + a new `bar.rs` hold chrome; five new leaf modules (`ansi`,
`content/{preview,panel,markdown}`, `ui/bar`). No cycles; the `ui → content` edge disappears
entirely (the bar was the only thing that crossed).

Three payoffs beyond line counts:

- **`ScrollbackOutput` deleted** — 78 lines of dead decorator.
- **29 `#[serial]` attributes** in `output.rs` exist only because tests pin width through
  `repl::TERMINAL_WIDTH`. `Draw::set_dims` retires the global, and with it the serialization.
- **7 `is_terminal()` sites → one `Caps`**, decided at construction.

Related: `doc/261002_agent_refactor.md` (the same treatment for `agent.rs`), `doc/ui_draw_261001.md`
(the design this consolidates), `doc/row_accounting.md`.
