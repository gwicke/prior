# Output drawing refactor: implementation notes

Companion to [`261003_output_drawing_refactor.md`](./261003_output_drawing_refactor.md). The design
doc holds the end state; this file holds the current-state measurements, the concrete module /
channel shapes, capture, the agent seam, and the migration plan.

## 1. Current state (motivation)

`output.rs` (6,624 lines) is not one concern. Measured clusters:

| Lines     | Concern                                                                            |
| --------- | ---------------------------------------------------------------------------------- |
| 63–128    | transcript block kinds + the separator matrix                                      |
| 130–495   | byte-level write pipeline (`WrapLines`, `LineClear`, `HoldTrailing`, `BatchGuard`) |
| 496–862   | `Output` trait — 34 methods                                                        |
| 863–1039  | four sinks (`Quiet`/`No`/`Recording`/`Log`), each a 23–34 method impl              |
| 1122–2051 | text wrap + tool-call preview (pure)                                               |
| 2052–2767 | DSR + the content write path + frame reposition                                    |
| 2768–2988 | the status bar (a **frame row**, not content)                                      |
| 3041–3328 | tool panel rendering (string-building)                                             |
| 3329–3966 | `impl Output for ConsoleOutput` — 30 methods                                       |

Three structural faults:

1. **Drawing is split across two modules.** `ui/frame.rs` owns compose → allocate → render → apply
   and documents itself as *"nothing here touches a terminal"*; `ConsoleOutput::draw_frame` (3461),
   `reposition_area` (2515), `cursor_row`/DSR (2070–2116), `frame_size`/`painted_size`,
   `begin_batch`, `remember_compose` own the half that does. A content write is a drawing operation
   on the chrome: it consumes the frame's rows, moves the seam, and repaints forward
   (`ui_draw_261001.md`: *"Content append consumes the frame rows; everything below is rewritten in
   full at the shifted position"*). `reposition_area` recomposes the frame (`allocate` + `render`)
   to do it.

2. **The ui/content edge is inverted.** `area_body` — the chrome composition rule (ui_draw_v2
   §5.1.1) — lives in `output.rs:47`, and `ui/frame.rs:24` imports `ContentDisplayKind` *from*
   `output.rs` while `output.rs:4` imports 13 items *from* `ui::frame`. Those are the only two
   edges, and one is backwards.

3. **Terminal geometry has three sources and a second back-edge.** `output.rs:1` is
   `use crate::repl::current_term_width` — the output layer reads the *input* layer for its own
   width. Width is then read from `repl::TERMINAL_WIDTH` (a process global the in-flight reader
   owns), from `crossterm::terminal::size()` (**12 independent call sites**, 7 in `output.rs` and 5
   in `repl.rs`, each with its own `.unwrap_or(...)` fallback — 80, 24, or `(80, 24)`), or from
   `term_width()`/`term_height()` — and those last two disagree about their own source, since only
   the first consults the global.
   Separately, `is_terminal()` is consulted at **7 sites** in `output.rs` (1950, 2990, 3098, 3229,
   3268, 3516, 3779) rather than once. The cost is in the test suite (see §7): **32 tests** call
   `capturing_output(width)`, which stores into `crate::repl::TERMINAL_WIDTH` — output's own tests
   mutating repl's global — and that is why 29 of them are `#[serial]`.

The end state removes all three: geometry has one owner (the TermIO watch), the hub is one module,
and the ui/content edge points the one way (content kinds stay above Draw).

## 2. Module layout

```text
src/screen.rs    Lines, TermSize, Stream — leaf: imports nothing
src/render.rs    RenderManager, Element, RenderElement, RowSpec/RowAsk, Rendered, ContentWrite, Frame
src/draw.rs      Draw, DrawCache, plan_apply/consume/reposition, paint_forward
src/ansi.rs      width/escape transforms + the io::Write wrappers (LineClear, WrapLines, StripAnsi)
src/termio.rs    TermIO, TtyIo, PlainIo, Caps, TermHandles
src/content/     mod.rs, text.rs, markdown.rs, panel.rs, preview.rs, composition.rs
src/ui/          ui.rs (menu chrome), bar.rs (the bar)
```

Dependency graph:

```text
Agent → Content ─┐
Repl  → UI ──────┴→ RenderManager → Draw → TermIO
```

`Content` and the UI both hold `Arc<Mutex<RenderManager>>`. Dependencies point downward only:
shared vocabulary (`Lines`, `TermSize`, `Stream`) lives in the `screen.rs` leaf, imported by all
four layers — no upward or sideways edges. `draw` never imports `content` kinds; `render` depends
on neither.

## 3. Channels and ownership

The self-reference the design doc avoids at the type level (a Hub cannot own a watcher that points
into a value it also owns through `Draw`) is resolved by **handing out cloneable handles at setup**:

```rust
enum InputEvent { Key(Key), Paste(String), Resize(TermSize) }
enum CaretEvent { Invalidated }              // SIGCONT / resume

pub struct TermHandles {
    pub input: Receiver<InputEvent>,         // one consumer: repl
    pub size:  watch::Receiver<TermSize>,    // Clone: RenderManager + content
    pub caret: Receiver<CaretEvent>,         // one consumer: RenderManager
}
```

Wiring, once, before `TermIO` moves into `Draw`:

1. build `TermIO`, call `handles()`;
2. hand `input` to the repl, clone `size` for content (format width) and keep one for
   `RenderManager` (frame dims), hand `caret` to `RenderManager`;
3. move `TermIO` into `Draw`; move `Draw` into `RenderManager`.

No layer holds a reference into another. `size` is a `watch::Receiver` (latest value, `Clone`);
`input`/`caret` are `mpsc` receivers (take-once). If a second consumer of `input` is ever needed,
promote it to `watch`/broadcast then; today resize reaches the repl on `input` and everyone else on
`size`, which is the split the design wants.

`RenderManager::suspend` / `Draw::suspend` are pass-throughs so content / UI never name `TermIO`;
`TermIO::suspend` is the only real implementation.

## 4. Content: the agent's seam

The agent needs exactly two things: a rendering sink and a consent port. The consent port already
exists and is the template — `ConfirmPort` (`confirm.rs:81`) is a cloneable, by-value `mpsc` +
`oneshot` handle held for the agent's life. Split the same way:

```rust
// agent/build.rs
pub struct AgentConfig {
    pub content: Arc<Content>,              // was output: Arc<dyn Output>
    pub confirm_port: Option<ConfirmPort>,  // unchanged
}
```

`Content` owns the rendering verbs the agent calls (`display_text/plain/markdown/system`,
`display_tool_call/result/error`, `begin_response` / `stream_text` / `stream_thinking` /
`end_response[_segment]`, `committed_prompt`, `separator`, `display_header`, capture). Internally it
holds `Arc<Mutex<RenderManager>>`, formats to `ContentWrite`, calls `render()`. The verbs the repl
drives stay with the repl/setup (`get_user_input`, `set_status`, `is_streaming`,
`draw_elements`/`paint_prompt`, `frame_size`). `confirm`/`confirm_default` go to `ConfirmPort`.

`display_header` is called by the agent (`agent/turn.rs:703`) → `Content`. `get_user_input` is
called by the repl (`interactive.rs:1089`) → not on `Content` at all.

`Content` is a struct, not a trait: backend variance moved down into `TermIO`, and every run's
Content behavior is now identical. The test seam is a `tap: Option<…>` field instead — when set,
the verbs record to a shared sink and short-circuit, covering the `RecordingOutput` /
`ScriptOutput` patterns without a live render stack.

Capture taps the `ContentWrite` exactly as handed to `render()` — pre width-recheck, content only.
Today's writer swap also caught frame bytes (`draw_frame` writes through the same `writer`,
`output.rs:3474`); the narrowing to content-only is deliberate.

## 5. Element reuse and the shared editor

Today `input_element(editor, prompt, streaming, term_w)` (`repl.rs:643`) builds a fresh
`InputItem { layout, prompt }` per compose, where `layout = editor.layout(term_w, prompt)` is a
snapshot (`repl.rs:649`, `:2725`). Each keystroke makes a new element, so `Box` would suffice.

For the input to be long-lived, it must recompute rows at render time instead of snapshotting:

```rust
struct InputItem {
    editor: Arc<Mutex<LineEditor>>,   // the live editor, not a Layout
    prompt: Arc<str>,
    streaming: Arc<AtomicBool>,
}
```

Consequences:

- `LineEditor` moves out of the per-iteration locals into a shared `Arc<Mutex<…>>` that both the
  element and the repl's key loop lock (`PromptSession::handle_key` mutates `&mut LineEditor`
  today). The same pattern covers overlay cursor moves in `/session`, `/tree`, `/model`: the rows
  element holds the selection, and a cursor key mutates it in place.
- `height()` / `render()` now read live session state; allowed under non-reentrancy (the editor is
  not the hub), but it is a shift from "the repl supplies what changed" to "the element reads what
  changed". The repl mutates, releases the editor lock, then triggers `render()` — the element
  locks the editor again inside `height()`/`render()` and the mutex is non-reentrant, so holding it
  across the call would deadlock.
- `configure()` runs when the element *set* changes (menu open/close), not per keystroke; the frame
  is re-rendered from the same stack each keystroke.
- Two locks per compose (allocate then render) on the editor; order is always
  `RenderManager → Element → editor`, and the repl takes the editor lock on its own.

The status bar is already this shape (`Arc<Mutex<StatusText>>`, resolved at render). Keep `Box` only
if a decision is later made to rebuild the input element per compose — then only the bar is truly
shared and the `Arc<Mutex<…>>` justification narrows to it.

## 6. RowSpec resolution

`RatioAndNum(ratio, rows)` removes the frame-size circularity: the **frame** budget uses the
concrete `rows` of every element's max ask (so `frame = min(max_ratio·term_h, Σ max.rows)`), and
only the **in-frame split** consults `ratio` against the already-resolved frame height — the split
is **ratio-weighted**. `FixedNum(n)` is the common case (status/divider) and weights evenly, so
all-`FixedNum` (today) stays byte-identical to the even split the `allocate_*` tests pin. No
element needs a ratio today; the variant exists so a future header/content split can express it
without reopening the allocator.

## 7. Testing strategy

The split moves ~2,500 lines of tests out of `output.rs`. Treat placement as a reading pass (as
`261002_agent_refactor.md` §6 does), not a mechanical move — some tests sit far from their subject.

- **Pure phases** (`allocate`, `render`, `plan_apply`/`plan_consume`/`plan_reposition`) keep their
  unit tests, moving with `frame.rs` → `render.rs`/`draw.rs`.
- **Byte-level** assertions move to a `TermIO::recording()` (memory writers, `#[cfg(test)]`) or a
  `Draw` over it: they test the *edge*, and see stripped bytes with frames dropped by caps.
- **Line-level** assertions (`RecordingOutput::joined`, `repl.rs:3392`, `replay.rs:650`) become
  `Content`'s tap — recorded verb entries, short-circuiting the pipeline — since `RecordingOutput`
  captures display lines, not bytes.
- **`capturing_output(width)`** and its 29 `#[serial]` tests go away: width comes from the `size`
  watch, seeded by the test harness, and the tests stop writing `repl::TERMINAL_WIDTH`. This is the
  single biggest test-win of the refactor.
- **pty / vt100** tests (`row_accounting`, `cr_wrap_pending`) are unchanged: they replay bytes.
- **New coverage** for the seams: watch-handle cloning (no borrow), capture capturing content but
  not frames, the shared-editor input element, `TermHandles` distribution.

## 8. Migration plan

Ordered so the geometry change lands first — it kills the test coupling that makes every later stage
expensive. Each step is independently green and revertible; the `Output` trait survives as a facade
until the last step.

1. **Extract the modules mechanically.** `render.rs` (from `ui/frame.rs`) and `draw.rs` (apply +
   `DrawCache` + `ConsoleOutput`'s drawing/writer half) and `ansi.rs`. `ConsoleOutput` keeps the
   `Output` trait, delegating drawing to `Draw`. No behavior change.
2. **Introduce `TermIO`.** Move the reader thread, raw mode, caps, DSR, suspend/resume, and the size
   watch behind it (Tty/Plain). Replace `TERMINAL_WIDTH` and the 12 `crossterm::terminal::size()`
   sites with the watch; collapse the 7 `is_terminal()` sites into per-stream caps. Un-`#[serial]`
   the width tests. Repoint `editor.rs` (`run_editor`'s `confirm_gate` + `cursor_row`) at the
   suspend/DSR pass-throughs; delete `GatedOutput`/`NoGateOutput`.

   > **Status (2026-10-04).** The *moves* are landed as module-level primitives in `termio.rs`: the
   > size watch + per-stream caps, the sole persistent stdin reader, the DSR query, and the
   > process-wide pause handshake (the gate is process-wide — one reader, so it is a mutual-exclusion
   > flag, not per-instance config like the size watch). That handshake is what let `confirm_gate`,
   > `reader_paused`, `editor::ReaderHold`, `GatedOutput`, and `NoGateOutput` be deleted and
   > `run_editor` be repointed.
   >
   > What is **not** done is step 2's headline: the `TermIO` **type** itself — the closed `Tty`/`Plain`
   > enum that owns the reader thread and folds `TermGuard` (raw mode) into it, plus `TermHandles`
   > and the `recording()`/`null()` constructors. That unification is **deferred to step 4**, where
   > `TermIO` moves into `Draw`/`RenderManager` and the ownership actually restructures; the
   > pause/DSR/deletions above do not need the type now, and folding `TermGuard` across its call
   > sites is safest done alongside that restructure.
   >
   > Minor remainder, also riding with step 4: the three `Output` trait-default size queries
   > (`cursor_row`, `frame_size`, `painted_size`) still fall back to a live `crossterm::terminal::size()`.
   > They serve non-console outputs only — `ConsoleOutput` overrides all three to read the watch — and
   > the loop-top resize probe (`repl.rs`, one per keystroke) is a deliberate live query that publishes
   > to the watch, not a leftover.
3. **Split `content/`.** Move classification, the separator matrix, the write pipeline, and the
   content rendering there; add the `Content` handle and switch `AgentConfig.output` →
   `AgentConfig.content`, along with `AppState.output` and the settings test helpers.
   `ConfirmPort` unchanged.
4. **Fold in `RenderManager` and UI.** `configure` / `render` / `render_batch`; convert elements to
   `Arc<Mutex<…>>` with the shared editor; move element construction into `ui/`/repl.
5. **Delete `output.rs`.** Remove the four sinks, the dead taps (`ScrollbackOutput`, `LogOutput`),
   and the trait; prune `lib.rs` re-exports.

## 9. Resolved open questions

Resolved from the earlier design discussion, with the end-state answers:

- **Cursor re-anchor** — bottom-up: TermIO pushes `CaretEvent::Invalidated` (SIGCONT, resume) on the
  `caret` channel to `RenderManager`, which re-anchors lazily at its next `render()` (the DSR is
  answered inside TermIO). The repl neither asks for it nor issues one.
- **Resize propagation** — bottom-up via `TermIO`'s `size` watch (RenderManager + content) and the
  repl `input` channel. Ported to the *hook* side first: `RenderElement::height`/`render` are
  width-aware, so elements adapt to a width change without a repaint-per-event.
- **Re-draw triggers** — push from the repl (status/streaming/keystroke state); the bar element
  pulls its own shared state at `render()` time.

## 10. Dead taps

`ScrollbackOutput` — dead code today; a content-path decorator if ever needed. `LogOutput` —
structured logger with zero users (only re-exported): delete, or keep as a content tap. Neither is a
`TermIO`.
