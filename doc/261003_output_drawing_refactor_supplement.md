# Output drawing refactor: detail information

## 1. The problem

`output.rs` (6,624 lines) is not one concern. Measured clusters:

|Lines    |Concern                                                             |
|---------|--------------------------------------------------------------------|
|63–128   |transcript block kinds + the separator matrix                       |
|130–495|byte-level write pipeline (`WrapLines`, `LineClear`, `HoldTrailing`, `BatchGuard`)|
|496–862  |`Output` trait — 34 methods                                         |
|863–1039|four sinks (`Quiet`/`No`/`Recording`/`Log`), each a 23–34 method impl|
|1122–2051|text wrap + tool-call preview (pure)                                |
|2052–2767|DSR + the content write path + frame reposition                     |
|2768–2988|the status bar (a **frame row**, not content)                       |
|3041–3328|tool panel rendering (string-building)                              |
|3329–3966|`impl Output for ConsoleOutput` — 30 methods                        |

Two structural faults, both measurable:

1. **Drawing is split across two modules.** `ui/frame.rs` owns compose →
   allocate → render → apply and documents itself as *"nothing here touches a
   terminal"*; `ConsoleOutput::draw_frame` (3461), `reposition_area` (2515),
   `cursor_row`/DSR (2070–2116), `frame_size`/`painted_size`, `begin_batch`,
   `remember_compose` own the half that does. A content write is a drawing
   operation on the chrome: it consumes the frame's rows, moves the seam, and
   repaints forward (`ui_draw_261001.md`: *"Content append consumes the frame
   rows; everything below is rewritten in full at the shifted position"*).
   `reposition_area` recomposes the frame (`allocate` + `render`) to do it.

2. **The ui/content edge is inverted.** `area_body` — the chrome composition
   rule (ui_draw_v2 §5.1.1) — lives in `output.rs:47`, and `ui/frame.rs:24`
   imports `ContentDisplayKind` *from* `output.rs` while `output.rs:4` imports
   12 items *from* `ui::frame`. Those are the only two edges, and one is
   backwards. The frame's own doc is already drawing the distinction the layer
   structure denies it: `last_content_closes_segment` is *"not transcript
   content and `draw_frame` leaves it alone"* (`ui/frame.rs:463`).

3. **Terminal geometry has three sources and a second back-edge.** `output.rs:1`
   is `use crate::repl::current_term_width` — the output layer reads the *input*
   layer for its own width. Width is then read from `repl::TERMINAL_WIDTH` (a
   process global the in-flight reader owns), from `crossterm::terminal::size()`
   (**14 independent call sites**, 8 in `output.rs` and 6 in `repl.rs`, every
   one `.unwrap_or((80, 24))`), or from `term_width()`/`term_height()` — and
   those last two disagree about their own source, since only the first consults
   the global. Separately, `is_terminal()` is consulted at **7 sites** in
   `output.rs` (1950, 2990, 3098, 3229, 3268, 3516, 3779) rather than once.

   The cost is in the test suite: **32 tests** call `capturing_output(width)`,
   which stores into `crate::repl::TERMINAL_WIDTH` — output's own tests mutating
   repl's global — and that is why 29 of them are `#[serial]`.

### Vocabularies

`Draw` owns **both**, because both are screen facts and both are produced
elsewhere — exactly as `Element` already is:

|vocabulary                                     |owned by   |produced by      |
|-----------------------------------------------|-----------|-----------------|
|`Element` (chrome rows)                        |`draw.rs`  |`ui/`            |
|`Boundary { kind: ContentDisplayKind, closes }`|`draw.rs`  |`content/`       |
|`BarText` (was `StatusText`)                   |`ui/bar.rs`|`agent/status.rs`|

`Boundary` living in `draw` is what keeps the graph acyclic: content calls
`write_content`, and draw needs the kind for both the signature and the cache
field. Put it in `content/` and you get a module cycle.

### Where the 14 `ConsoleOutput` fields go

|→ `draw.rs`                   |→ `content/`       |→ `ui/bar.rs`|→ interaction|
|------------------------------|-------------------|-------------|-------------|
|`draw: Mutex<DrawCache>`      |`parser`         |`status_text`|`reader_paused`|
|`writer` → `sink`             |`renderer`         |    |(in-flight stdin gate)|
|`saved_writer` (capture)      |`thinking_parser`  |             |             |
|`held_blank` (joins the cache)|`thinking_renderer`|             |             |
|`streaming` (the bar's dot)   |`last_tool`        |             |             |
|`reasoning_on_stderr`         |`show_reasoning`   |             |             |

The lock order — writer, then `draw` — is restated across ~30 methods in
`output.rs` today. In `draw.rs` it is encapsulated once, because `Draw` owns
both fields.

## UI/Content <-> Draw API

- UI owns frame rendering, vertical split policy
  - render.rs module
- Draw only does screen drawing, content / frame composition
- Top-down ownership
  - Draw -> Render via Channel<RenderRequest> drained by Repl; *by value*
  - RenderRequest is sending `term_size`
- Repl owns render stack / mints IDs; drains Channel & calls renderers in order
  Epoch for render stack generation, ids can be direct offsets into vec (u16)

Frame

- width, height, Arc<lines>, cursor

Content write

- Pre-render at last known width
- call append_scrollback()
  - immediate re-draw with cached frame - minimize latency (async)
  - send RenderRequest on channel to parent
  - on frame receive w/ matching width + height, re-render (usually no-op with
    diffing)

UI update

- send new Frame, using last known size
- if size mis-match: Draw will send new RenderRequest

Resize

- Draw -> RenderRequest to parent with updated dimensions

Special events / cursor recovery

- Whenever absolute position is unknown / possibly invalidated: startup,
  ctrl-z, $EDITOR
- `absolute_cursor() -> position` - Direct call from render() through draw to
  output. Only called when render / UI knows cursor has been invalidated.

## Open questions

- resize signal handling: bottom up, or top-down in single event loop?
  - bottom-up: better encapsulation
