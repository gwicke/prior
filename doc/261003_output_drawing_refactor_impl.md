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
   the first consults the global. Separately, `is_terminal()` is consulted at **7 sites** in
   `output.rs` (1950, 2990, 3098, 3229, 3268, 3516, 3779) rather than once. The cost is in the test
   suite (see §7): **32 tests** call `capturing_output(width)`, which stores into
   `crate::repl::TERMINAL_WIDTH` — output's own tests mutating repl's global — and that is why 29 of
   them are `#[serial]`.

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

`Content` and the UI both hold `Arc<Mutex<RenderManager>>`. Dependencies point downward only: shared
vocabulary (`Lines`, `TermSize`, `Stream`) lives in the `screen.rs` leaf, imported by all four
layers — no upward or sideways edges. `draw` never imports `content` kinds; `render` depends on
neither.

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
Content behavior is now identical. The test seam is a `tap: Option<…>` field instead — when set, the
verbs record to a shared sink and short-circuit, covering the `RecordingOutput` / `ScriptOutput`
patterns without a live render stack.

Capture taps the `ContentWrite` exactly as handed to `render()` — pre width-recheck, content only.
Done since **4d** (`98bcb85`): the tap is the manager's `captured` buffer, the writer swap that
also caught frame bytes (`draw_frame` wrote through the same `writer`) is gone, and the narrowing
to content-only is pinned by `capture_catches_content_but_never_frame_bytes`.

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
  changed". The repl mutates, releases the editor lock, then triggers `render()` — the element locks
  the editor again inside `height()`/`render()` and the mutex is non-reentrant, so holding it across
  the call would deadlock.
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
all-`FixedNum` (today) stays byte-identical to the even split the `allocate_*` tests pin. No element
needs a ratio today; the variant exists so a future header/content split can express it without
reopening the allocator.

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

### Step 1: Extract the modules mechanically

done (`257e8e3`, `799cd12`, `177aa64`).

### Step 2: Introduce `TermIO`

The moves are landed (`74e987c`); the deferred remainder is below.

> **Deferred to step 4** (the moves landed in `74e987c`): the three `Output` trait-default size
> queries (`cursor_row`/`frame_size`/`painted_size`) that still fall back to a live
> `crossterm::terminal::size()` (non-console outputs only; `ConsoleOutput` already reads the
> watch) — step 4 **4g**. The loop-top resize probe is deliberate, not a leftover. (The `TermIO`
> **type** — the closed `Tty`/`Plain` enum owning the reader and folding `TermGuard`, plus
> `TermHandles` and the `recording()`/`null()` constructors — landed in **4a**, `8cc955d`.)

### Step 3: Split `content/`

Landed 3a–3d (`47122b4` … `cf7b6a8`) — the commit messages record what moved where. What it leaves
behind, for the steps below:

- ~~`with_content_writer`'s byte-production half (`WrapLines`/EL stack) lives in `Content` but is
  Draw-coupled; splitting it into `ContentWrite` handed to `render()` — **4d**.~~ Landed (`98bcb85`)
  — the split landed the other way round, deliberately: the byte-production half (boundary,
  hold, wrap, line-clear) *stays* in `Content` (design §Content: those are content's bookkeeping),
  and the Draw-coupled tail (consume → write → note → reposition) left for the hub as
  `RenderManager::put_content`.
- ~~`begin_capture`/`end_capture` tap the sink (`content.draw.saved_writer`), not the `ContentWrite`
  hand-off — **4d** re-taps them there.~~ Landed (`98bcb85`): they tap the manager's
  `captured` buffer, and `Draw::saved_writer` + `Draw::begin_capture`/`end_capture` are gone.
- The moved rendering's unit tests still live in `output.rs`'s test modules → relocate to `content/`
  when convenient (nothing claims it yet).

### Step 4: Fold in `RenderManager` and UI

`configure` / `render` / `render_batch`; convert elements to `Arc<Mutex<…>>` with the shared editor;
move element construction into `ui/`/repl.

**Commit plan (4a–4h; each independently green — suite, fmt, clippy).** Ownership moves first,
call-site repoints after; the design-led element conversion closes the step — last, where there is
most runway to expand into separate sub-commits.

- **4a — `TermIO { Tty | Plain }` as a closed type.** Landed (`8cc955d`) — the commit message
  records what moved. Leftovers, each claimed:
  - The cloneable `Arc<TermGuard>` holders (`Repl`, the prompt loop, `$EDITOR` via app_state, the
    back-off pump) still name the guard directly; `TermIO::guard()` hands the handle out — **4f**
    converts them to the manager/TermIO suspend and the handle dies.
  - Resize still travels the size watch only (`route_event` keeps it Inline, as step 2 decided);
    the design's "resize reaches the input channel too" forwards when `read_input` collapses to
    one handle — **4f**.
  - `TermIO` parks in `Interactive` (read only for its lifetime) — the `Draw` move takes it, and
    the design has `Draw { term_io, cache }` — **4c** is its natural rider.
  - `TermIO::write` has no production caller (the `Draw` writer is still a plain field), caps are
    still probed at `Content::new` rather than at `TermIO` construction, and the size watch's
    sender is still the process-wide `LazyLock`. Re-scoped by **4c** (see below): the writer +
    caps + `TermIO`-into-`Draw` move needs `TermIO` constructed *before* `Content`/the manager —
    today it is built later, in `Interactive` — and the capture writer-swap gone (**4d** landed,
    `98bcb85`), so it rides with the **4f** setup rework; the watch sender moves with its producers —
    the prompt loop's probe and the parked-resize drain — when `read_input` collapses (**4f**; 4b
    landed the manager's receiver half).
- **4b — `RenderManager` shell: composition + apply leave the `Output` trait.** Landed (`38d9513`)
  — the commit message records what moved. Leftovers, each claimed:
  - The call sites (repl, confirm, probe) keep their `&dyn Output` handles and reach the manager
    through `draw_elements`' `ConsoleOutput` forward; the handle-level switch — repl/confirm
    holding the manager — is what the 4f follow-up below already schedules (threading a manager
    through `settle`/`paint`/the turn pumps now would be re-done when those params collapse).
  - The compose head stays on `Output::compose_area` (the status row is console state) — **4h**,
    when the bar becomes an installed element and the head moves into `render()`.
  - ~~The recipe field (`stack`) did not land: the recipe must leave `DrawCache` together with
    `reposition_area` (Elements cannot be cloned into two recipes), which is **4e** — the hub owns
    reposition there, and the recipe goes with it.~~ Settled by **4e** (`bce38e7`):
    `DrawCache.element_stack` is gone, `RenderManager::recipe` is the composition `render` applied,
    `reposition_area` is the hub's (reading it at the hub's configured `budget()`, not the
    hard-coded `live_budget` — a latent mismatch with `render`'s compose a configured ratio would
    have exposed), and the direct-`draw_frame` clear is the hub-level `draw_frame` forward.
  - `render_batch`'s body landed with the `ContentWrite` hand-off (**4d**, `98bcb85`) — it
    delegates to the same `put_content` as `render_content`, the batch flag inside deferring the
    reposition; the batching guard itself moved in **4e** (`bce38e7`), which did *not* split the
    verbs — that is the final-`render()` round trip (**4f/4h**). `configure`'s `frame_elements`
    half waits for **4h** with the rest of the long-lived stack.
  - The size watch's *receiver* is the manager's; the *sender* is still the process-wide
    `LazyLock` — it moves with its producers (the prompt loop's probe, the parked-resize drain)
    when `read_input` collapses — **4f** (see the 4a note above).
- **4c — `Draw` moves into `RenderManager`; `Content` gets the manager handle.** Landed
  (`33dc92c`) — the commit message records what moved, including the bonus the stopgap note
  implied: `Draw`'s own `writer`/`saved_writer`/`cache` mutexes are gone too (plain fields,
  `&mut self` writers; the manager's lock is the one serialization). Leftovers, each claimed:
  - The writer + caps + `TermIO`-into-`Draw` move (design §Draw: `Draw { term_io, cache }`,
    caps cached from `TermIO::caps()`) is **not** in 4c: `TermIO` is constructed after
    `Content`/the manager today (a setup rework — it rides with **4f**, where the handles start
    flowing to the repl anyway), and the capture writer-swap had to be gone first — **4d**
    retired it (`98bcb85`), else `TermIO` would need a capture mode of its own.
  - ~~`Content::draw()` accessor stays until `Content` stops touching the sink — **4d** ("every
    write enters through `render()`" makes it dead).~~ Settled by **4d** (`98bcb85`): the content
    write path no longer names `Draw` at all; `RenderManager::draw()` survives as the frame verbs'
    route only (until **4f**) — its batch-flag half ended with **4e**, the mechanics being the
    hub's own now.
  - `Draw` and the manager each hold a receiver of the same size watch; the duplication dies with
    `TermIO`'s move into `Draw` (**4f**, per the first point).
- **4d — `ContentWrite` + the `render()` hand-off; capture taps early.** Landed (`98bcb85`) — the
  commit message records what moved. What it leaves behind, for the steps below:
  - The hand-off type is `ContentWrite { bytes, rows, width }`, not the sketch's
    `{ lines, stream: Stream }`: the payload stays `Vec<u8>` (a block can carry non-UTF8 tool
    output, so rows-as-`String` is not yet honest) with the measured row count and the rendered
    width alongside, and `Stream` is **absent on purpose** — there is no meta sink under the hub
    while `Draw` owns no `TermIO`, and today's reasoning-to-stderr path never reaches `Draw` at
    all; the token arrives with the meta sink — **4f**.
  - `Content` hands off through two verbs, `render_content` (streaming) and `render_batch`
    (replay, inside a batch), which are today one `put_content` choreography under two names — the
    `batch_active` flag defers the reposition either way. **4e** moved the guard to the hub but did
    *not* split them: the deferral is still the shared flag's, and the verbs diverge only when the
    end state's single `render(Some(w))` — compose and content in one call, the frame applied below
    the block — replaces the guard's `end_batch` reposition, which waits for the frame verbs
    (**4f**) and the installed stack (**4h**).
  - Capture now taps the hand-off exactly as designed: content bytes only, the deliberate
    narrowing over the old writer swap (which also caught frame bytes), pinned by
    `capture_catches_content_but_never_frame_bytes`. Captures stay *raw* — the width guard and
    line-clear still step aside while one is open, as they always did, so content assertions
    compare what Content produced. Frame-byte capture tests moved to the writer seam
    (`output.rs`'s `record_bytes`, `render.rs`'s `SharedBuf`) — the seam `TermIO::recording()`
    takes wholesale in **4f**.
  - The design's "formats … before each block — no hub round-trip" is not yet real: the manager
    lock still spans build + hand-off (deliberate — the same serialization 4c established, and the
    boundary state is Content's under it). Formatting-only, unlocked, comes with the round trip
    (**4f/4h**) once non-reentrancy is proven for the build span.
- **4e — Batching guard → the hub** (its own commit, deliberately). Landed (`bce38e7`) — the commit
  message records what moved. Settled "where batching lives": the hub owns reposition, the flag
  and the deferred frame's re-emit; the recipe moved with it (the 4b note above). What it leaves
  behind, for the steps below:
  - The `Output`-layer `BatchGuard` stays: it names the `Output` whose drop ends the batch, for
    the handle holders (repl, confirm, tests) still on `&dyn Output` — it dies with the trait in
    **step 5**. `replay_units` opens its batch through `ContentBatchGuard::open(&content.manager)`
    — the manager handle *beside* `Content` is the one Content's own layer holds down to (design
    §Content); no signature threading was needed, and the tap path is harmless there (nothing
    reaches the hub, so the flag raises and `end_batch` finds nothing deferred).
  - The two hand-off verbs remain one body under two names (see the 4d note above): the guard
    moving does not itself split the deferral — that is the final-`render()` round trip,
    **4f/4h**.
- **4f — Frame verbs re-home: agent, interactive, repl, confirm.** The biggest step; broken into
  sub-commits **4f-a … 4f-f**, each independently green (suite, fmt, clippy). Ownership moves
  before call-site repoints within each; the deep wiring (TermIO under Draw) closes it.
  - **4f-a — the bar formats itself from raw data.** Per the design decision on `set_status`: the
    bar learns to format itself — the push carries *raw data*, and the shaping is pulled into the
    bar's own render. Concretely: an owned raw type in `agent/status.rs` (session name, first
    message, and the `BarModel` fields — provider, model, thinking level, context tokens, context
    window — today's `status_text` *inputs*); the status element holds
    `Arc<Mutex<…raw…>>` + the streaming flag and runs the shaping (`status_text`: name fallback to
    the opening-message preview, the field list, the drop order) *and* the fit (`status_bar`) inside
    `render_at`. The raw-data store moves beside the streaming flag it already shares — onto
    `Content` (`set_status` becomes a content verb); `Output::set_status`'s callers re-point and
    the trait verb dies with the rest of the frame surface (or in 4f-b, whichever is last).
    Byte-identical: same shaping rules, one home later in the pipeline — and a mid-turn figure
    (context tokens) is now current at the next render without a re-shaping push.
  - **4f-b — the frame path leaves `&dyn Output`: compose head + `paint`/`settle`/`emit_commit`/
    confirm on the manager.** With the bar self-contained (4f-a), `compose_area`'s head no longer
    needs `ConsoleOutput`'s privates: the compose head moves out of `Output` into the paint path —
    a helper that builds `[status, input, …overlays]` from content's handles (status raw data,
    streaming) plus the session's elements and calls `manager.render(stack, …)` directly. The
   head's *final* home is `render()` itself, with the bar an installed element — that much is
    **4h**; this sub-commit only moves it out of `Output`. What each function actually takes,
    by what it consumes: `paint_prompt` and `Repl::paint` need content (the compose head reads the
    bar's raw-data and streaming handles) + manager (the render); `Repl::settle` the same, plus its
    error path's `display_text` (content) and `cursor_row` (manager); `emit_commit` content
    (`display_committed_prompt`) plus the redraw closure it already takes.
    `confirm::paint` needs **only the manager** — a bare overlay composition, no content anywhere;
    `confirm::resolve` needs content for the decision line (`display_plain` is a content verb,
    tap seam included) plus the redraw closure — the repaint-then-decide order is the invariant
    the function exists to protect, so the line does not go back to the callers. Pumps/turn/probe
    call sites re-point. `is_streaming` leaves the trait with them.
  - **4f-c — dims: `frame_size`/`painted_size` → the size watch.** The agent-side callers
    (`pumps.rs`, `turn.rs`, interactive) read the watched size (frame dims) and the manager's
    applied size (what a resize comparison needs) through the handles `AgentConfig` already
    carries (`content.manager`, `content.size`). The two `Output` trait verbs and their
    `ConsoleOutput` impls die.
  - **4f-d — input collapses to one handle; the `TermGuard` holders convert.** `read_input` takes
    the handle set (content + manager + the key receiver) rather than `&dyn Output` + guard;
    `get_user_input`, `run_entry_dsr` (the caller side — the DSR *answer* moving inside TermIO is
    **4g**), `turn_backoff_pump`, and `app_state`'s `run_editor` re-home to manager/TermIO. The
    four cloneable `Arc<TermGuard>` holders (Repl, the prompt loop, `$EDITOR`, the back-off pump)
    convert to the manager/TermIO suspend and the direct guard handle dies (the **4a** leftover).
    The prompt-loop size probe and the parked-resize drain keep writing the process-wide sender
    until 4f-e moves it. `AgentConfig.output` is left without a duty the agent itself needs; it
    stays as a field to step 5's trait deletion unless a holder turns up here.
  - **4f-e — TermIO into Draw: the setup rework.** TermIO is constructed first (in `Interactive`);
    its handles are handed out (`input` → repl, `size` clones → content and the manager); caps are
    probed at TermIO construction and handed to `Content::new` (the `Caps::probe` site leaves
    content); TermIO moves under Draw — `Draw { term_io, cache }`, the `writer` field dies (Draw
    writes through the main sink), and the size watch's sender comes under TermIO's roof (the
    process-wide `LazyLock` dies; 4f-d's probe and drain write through the handle; the few
    free-function subscribers with no handle are settled here). `TermIO::recording()` takes the
    byte-edge seam wholesale — the `record_bytes`/`Sink`/`SharedBuf` helpers re-point.
  - **4f-f — `Stream` + the meta sink; the formatting span unlocks.** `ContentWrite` gains
    `stream: Stream`; the reasoning-to-stderr decision becomes a *stream* decision (content picks
    Meta once at setup) and `put_content` routes Main → the screen choreography, Meta → TermIO's
    meta sink with no choreography (byte-identical to today's `ReasoningSink` stderr bypass, which
    dies). With the sinks under the hub, `with_content_writer`'s formatting span unlocks — build
    the `ContentWrite` before the lock, the design's "no hub round-trip" — pending the
    non-reentrancy proof the 4d note recorded; the last rider, and droppable on its own.
  Out of 4f: the `render(Some(w))` compose + content round trip (waits for the installed stack,
  **4h**).
- **4g — Suspend/resume + DSR through the manager; dead size fallbacks die.**
  `RenderManager::suspend()` becomes the front door (content/UI never touch TermIO), DSR is answered
  inside TermIO, and the three `Output` trait-default size queries still falling back to a live
  `crossterm::terminal::size()` go — non-console outputs read the watch. The loop-top resize probe
  stays (deliberate, step-2 note).
- **4h — Elements → `Arc<Mutex<…>>`; construction into `ui/`** (design-led, kept last for runway;
  expect it to split). `Element = Arc<Mutex<dyn RenderElement>>` with the shared editor held live
  across compositions; `Element::status` moves into `ui/`; `ui.rs` becomes a `ui/` module (menu
  chrome + bar); element construction centralizes in `ui/` + repl; `render.rs` becomes the
  RenderManager home.

Follow-ups step 3 leaves here — each claimed by a commit above unless noted:

- **Re-point the frame helpers' content calls.** `settle` (`repl.rs:1329`, `:1351`), `emit_commit`
  (`repl.rs:2266`) and `confirm::paint` (`confirm.rs:336`) take `&dyn Output` and reach content
  through `ConsoleOutput`'s forwarders. Once each holds a `Content` (or a hub that owns one) those
  forwarders go dead and `Output` shrinks to frame + input. → **4f**
- **Collapse `read_input`'s two params.** It carries `output: &dyn Output` *and*
  `content: &Content`; step 4 gives the frame/input half to RenderManager + the repl's `input`
  channel, leaving one handle. → **4f**
- **Re-home the agent's frame verbs.** `CodeAgent.output` survives only for `set_status`,
  `frame_size` and `paint`/`settle`'s `&dyn Output`; `turn.rs`/`pumps.rs` re-point those at
  RenderManager and the field goes. Same for `interactive.rs`'s `run_entry_dsr`,
  `turn_backoff_pump`, `get_user_input`, and `app_state.rs:761`'s `run_editor`. → **4f**
- ~~**Decide where batching lives.** `Content::begin_batch`/`ContentBatchGuard` suppress `Draw`'s
  reposition; if RenderManager owns reposition, the guard moves to the hub and `replay_units` takes
  that instead of `Content`.~~ Answered and landed by **4e** (`bce38e7`): the hub owns reposition;
  the guard moved there and `replay_units` opens it from the manager handle beside `Content`.
- **Revisit the interior-mutability stopgaps.** The `AtomicBool` routing flag and `Draw::set_writer`
  exist only because `Content` is shared behind an `Arc`. `set_writer` should fall in **4c** (the
  manager's `Mutex` restores `&mut` access); the flag folds with the tap re-shape (step 5) — or
  earlier if a `Mutex` reads better.

### Step 5: Delete `output.rs`

Remove the sinks, the dead taps (`LogOutput`), and the trait; prune `lib.rs` re-exports.

Follow-ups step 3 leaves here:

- **Drop `AgentConfig.output`,** reaching the §4 target `AgentConfig { content, confirm_port }`.
  `AgentConfig::content_for` and `ConsoleOutput::content()` exist only for the dual field and go
  with it, as does `ConsoleOutput` itself — ending the `Arc<Content>` sharing.
- **Re-shape the tap** (moved here from step 4 — it only pays once the trait is gone).
  `Content::tap` is `Option<Arc<dyn Output>>`; it becomes a verb recorder (`FnMut` / sink) or folds
  into capture. `content_for` then reduces to a quiet-vs-record switch, and
  `RecordingOutput`/`ScriptOutput` lose their content duties.
- **Retire the `Output`-shaped sinks.** `QuietOutput`/`NoOutput`/`RecordingOutput`/`ScriptOutput`
  remain only for `content_for` + the frame handle; with the tap re-shaped here content is covered
  and the frame is gone. `LogOutput` (§10) dies here regardless.
- **Capture stays at the `ContentWrite` hand-off** — re-tapped early in **4d** (content bytes only,
  never frames: the old writer-swap also caught `draw_frame` bytes). Nothing to migrate here; verify
  it as the sinks die.

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

`LogOutput` — structured logger with zero users (only re-exported): delete (step 5), or keep as a
content tap. An `Output` impl, not a `TermIO`. (`ScrollbackOutput` already removed — `5015829`.)
