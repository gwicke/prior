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

> ~~**Deferred to step 4**: the three `Output` trait-default size queries (`cursor_row`/
> `frame_size`/`painted_size`) that still fall back to a live `crossterm::terminal::size()`.~~
> Landed: `frame_size`/`painted_size` with 4f-c, `cursor_row` in **4g** (`c21be0e`). The
> loop-top resize probe is deliberate, not a leftover. (The `TermIO`
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

`configure` / `render` / `render_batch`; move Render/Draw/TermIO into the `slashdot_tui` crate,
which exposes `RenderElement`; convert elements to `Arc<Mutex<…>>`; `slashdot_menu` mints its views'
elements, `ui/`/repl the rest.

**Commit plan (4a–4h; each independently green — suite, fmt, clippy).** Ownership moves first,
call-site repoints after; the design-led element conversion closes the step — last, where there is
most runway to expand into separate sub-commits.

- **4a — `TermIO { Tty | Plain }` as a closed type.** Landed (`8cc955d`) — the commit message
  records what moved. Leftovers, each claimed:
  - ~~The cloneable `Arc<TermGuard>` holders (`Repl`, the prompt loop, `$EDITOR` via app_state, the
    back-off pump) still name the guard directly; `TermIO::guard()` hands the handle out — **4f**
    converts them to the manager/TermIO suspend and the handle dies.~~ Settled by **4f-d**
    (`348f7ed`): all four holders converted to the manager's `suspend`/`resume`, the direct Arc
    clones died, and `TermIO::guard()`'s one remaining caller is setup's publish into the manager
    (retired with the `Draw` move — **4f-e** landed, `8733d09`).
  - ~~Resize still travels the size watch only (`route_event` keeps it Inline, as step 2 decided);
    the design's "resize reaches the input channel too" forwards when `read_input` collapses to
    one handle — **4f**. The collapse landed in 4f-d *without* the forward (see 4f-d's leftover);
    the forward rides **4f-e**, with the sender moves it must not race.~~ Landed (`2906fb1`):
    both edges; the sender had already moved with the probe/drain (`8733d09`), so no window had
    two publishers.
  - ~~`TermIO` parks in `Interactive` (read only for its lifetime) — the `Draw` move takes it, and
    the design has `Draw { term_io, cache }` — **4c** is its natural rider.~~ Landed (`8733d09`):
    constructed first in `main`, moved into `Draw` at `Content::new`; `Interactive`'s field died.
  - ~~`TermIO::write` has no production caller (the `Draw` writer is still a plain field), caps are
    still probed at `Content::new` rather than at `TermIO` construction, and the size watch's
    sender is still the process-wide `LazyLock`. Re-scoped by **4c** (see below): the writer +
    caps + `TermIO`-into-`Draw` move needs `TermIO` constructed *before* `Content`/the manager —
    today it is built later, in `Interactive` — and the capture writer-swap gone (**4d** landed,
    `98bcb85`), so it rides with the **4f** setup rework; the watch sender moves with its
    producers — the prompt loop's probe and the parked-resize drain — when `read_input`
    collapses (**4f**; 4b landed the manager's receiver half).~~ Landed (`8733d09`,
    `c7ef4fe`): `TermIO` constructs before `Content` (in `main` — see 4f-e's recorded deviation),
    production bytes flow through `TermIO::write`, caps probe at construction, the sender is
    per-instance with probe + drain on the manager's handle.
- **4b — `RenderManager` shell: composition + apply leave the `Output` trait.** Landed (`38d9513`)
  — the commit message records what moved. Leftovers, each claimed:
  - ~~The call sites (repl, confirm, probe) keep their `&dyn Output` handles and reach the manager
    through `draw_elements`' `ConsoleOutput` forward; the handle-level switch — repl/confirm
    holding the manager — is what the 4f follow-up below already schedules.~~ Settled by **4f-b**
    (`cb01977`): `paint_prompt`/`settle`/`paint`/`emit_commit`/`confirm::paint`/`confirm::resolve`
    take content and/or the manager; `Output::draw_elements` is gone.
  - ~~The compose head stays on `Output::compose_area` (the status row is console state) — **4h**,
    when the bar becomes an installed element and the head moves into `render()`.~~ The head left
    `Output` in **4f-b** (`repl::compose_area`, interim home); its final home inside `render()`
    with the bar installed is still **4h**.
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
    all; the token arrives with the meta sink — ~~**4f**~~ Landed (`cbeceed`).
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
  - ~~The design's "formats … before each block — no hub round-trip" is not yet real: the manager
    lock still spans build + hand-off (deliberate — the same serialization 4c established, and the
    boundary state is Content's under it). Formatting-only, unlocked, comes with the round trip
    (**4f/4h**) once non-reentrancy is proven for the build span.~~ Landed (`044c7f9`): the
    proof — one writer per content (the turn is inline in `prompt()`'s `select!`; prompt-time
    verbs and capture toggles are the same task) and `f` non-reentrancy — is recorded at the
    span, with a `Content.building` debug tripwire keeping a violation loud; the manager lock
    now covers the hand-off alone.
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
  - **4f-a — the bar formats itself from raw data.** Landed (`529f278`) — the commit message
    records what moved: `BarStatus` (the owned raw push) in `agent/status.rs`, `status_text`
    taking `&BarStatus`, the store on `Content` beside the streaming flag (`set_status` is a
    content verb — a store, not a write, so the tap has nothing to intercept), `StatusItem`
    running shaping *and* fit in `render_at`, and `Output::set_status` dead with its callers
    re-pointed. Byte-identical; `BarModel` retired with the push-time shaping. Leftovers, each
    claimed:
    - The compose-side presence check (`status_row`) now shapes once at compose and once at
      render — two cheap calls, deliberate: off is a shaped answer, and a second `is_off` on raw
      emptiness would be a second definition of "nothing to say" to drift. A later cleanup could
      shape once per compose and hand the shaped pair down; not worth it while the store is this
      small.
    - `StatusText` stays `pub` (it crosses the pub `agent::status::status_text` return); the
      visibility pruning is step 5's.
  - **4f-b — the frame path leaves `&dyn Output`: compose head + `paint`/`settle`/`emit_commit`/
    confirm on the manager.** Landed (`cb01977`) — the commit message records what moved. Per
    function, by what it consumes: `paint_prompt`/`Repl::paint` content (compose head + streaming;
    the manager comes through `content.manager` — one handle, the design's graph); `Repl::settle`
    the same plus `display_text` (content) and `cursor_row` (now a `RenderManager` method — the
    DSR query + re-anchor, with `Output::cursor_row` a trait forward); `emit_commit` content plus
    its redraw closure; `confirm::paint` the manager only (a bare overlay through the shared
    `area_body`); `confirm::resolve` content (the decision line, tap seam included) plus the
    redraw closure. `Output` loses `compose_area`, `status_row`, `draw_elements` and `is_streaming`
    — defaults and `ConsoleOutput` impls both. Leftovers, each claimed:
    - The compose head's interim home is `repl::compose_area` (a free fn, content handles in); its
      final home is `render()` with the bar an installed element — **4h**.
    - `ConsoleOutput::hub()` (pub, beside the existing `content()`) and the now-pub
      `RenderManager::render` exist for out-of-crate drivers (the confirm probe). Both are
      transitional surface for **step 5**'s trait deletion to prune.
    - ~~The pumps thread content *beside* output (dims and `$EDITOR` still reach for it) — the
      overlap ends with **4f-c** (dims) and **4f-d** (`$EDITOR`, guards).~~ The dims half closed
      with **4f-c** (`fad8f76`), the `$EDITOR`/guard half with **4f-d** (`348f7ed`) — what `output`
      carries now is only what step 5 deletes (the quiet tap, the tools' `confirm_default`).
    - ~~`Output::draw_frame` keeps only test drivers (the 4d-era byte-edge tests); it and the
      remaining frame verbs (`frame_size`, `painted_size`, `cursor_row`'s forward) go with
      **4f-c**/**4f-d** and **step 5**.~~ `frame_size`/`painted_size` went with **4f-c**;
      `draw_frame` (test drivers) and `cursor_row`'s forward remain for **step 5**.
  - **4f-c — dims: `frame_size`/`painted_size` → the size watch.** Landed (`fad8f76`) — the
    commit message records what moved. `frame_size` → `Content::frame_size` (the receiver this
    handle carries, same 20/1 floors, the same channel `RenderManager::new` clones — dims and the
    manager's geometry view cannot disagree); `painted_size` → `RenderManager::painted_size` (the
    applied dims `render`/`draw_frame` record). The two trait verbs, the two `ConsoleOutput`
    impls, and `ConsoleOutput`'s private `term_width`/`term_height` (which only served its
    `frame_size`) all die. Callers: turn pumps + turn read `content.frame_size()`; the REPL
    loop-top resize check reads `content.manager.lock().painted_size()`; the confirm probe (an
    out-of-crate driver) reads the `term_width`/`term_height` free fns — the same watch. No
    `interactive.rs` site existed to move (the plan named it; grep agrees it never called the
    verbs). Leftovers, each claimed:
    - ~~The REPL loop's live per-keystroke terminal query and its `set_size` publish stay: they
      are the probe the watch refreshes from — **4f-e** moves them under `TermIO`'s roof (already
      listed under 4f-d's "prompt-loop size probe" note).~~ Landed (`8733d09`):
      `RenderManager::probe_size()` is the loop-top refresh — one call, query + publish.
    - `RenderManager::draw()` went `#[cfg(test)]` — its last production caller was the
      `Output::painted_size` forward this commit took; only the writer/cache test seams reach
      past the manager now. Un-gate only if a later sub-commit finds a production seam that
      genuinely reads *past* the manager's verbs (none should).
    - ~~The probe's `dims` closure is the out-of-crate driver's pattern for now; 4f-e's handle
      hand-out may give drivers a subscribed receiver instead of the process-wide free fns.~~
      Landed (`8733d09`): the probe's `dims` reads `content.frame_size()` — a subscribed
      receiver through a handle, which is why `Content::frame_size`/`width`/`height` went `pub`.
    - `Output::draw_frame` (test drivers) and `Output::cursor_row`'s default (non-console
      outputs) stay for **step 5** — 4f-d took neither; cursor_row re-anchors go via the manager.
  - **4f-d — input collapses to one handle; the `TermGuard` holders convert.** Landed (`348f7ed`)
    — the commit message records what moved. The manager gained the design's front door
    (`set_mode_guard`, `suspend`, `resume`, `is_tty` — §Render: content/UI never touch TermIO),
    and setup's one publish (`Interactive` → `set_mode_guard`) replaced the four direct Arc
    holders (`Repl` → a unit; `Interactive`'s field; `AppState`'s `term_guard` slot and its two
    getters; `TurnPump`/`InTurnInput`/dispatch/back-off params). Per duty: `read_input` takes
    `(content, keys)` — its DSR re-anchors go through `content.manager.cursor_row()`, its
    Ctrl-C/Ctrl-Z through `suspend`/`resume`, raw entry is the manager's `resume` where
    `Repl::start` was; `run_entry_dsr` takes content (caller side only — the DSR *answer* inside
    TermIO is **4g**); `run_editor` takes content (order byte-identical: check → pause → stop →
    edit → start → DSR; its locks are per-flip, never across the human edit) and now owns the
    Unsupported check off `is_tty()`, so `edit_text` lost its caller-side guard check;
    `turn_backoff_pump`/`dispatch`/`TurnPump`/`InTurnInput` drop guard *and* output — a prompt
    session exists only where TermIO is Tty, so the old guard-presence zip changes nothing.
    (The DSR answer was inside TermIO all along — step 2 commit C; 4g put the entry window on
    the same guard, `6961d0b`.)
    `CodeAgent`'s private `output` copy dies with dispatch's last use; `AgentConfig.output` stays
    (the quiet tap, the tools' `confirm_default` copies out of `build_rig_agent`, step 5's
    deletion). ~~The prompt-loop size probe and the parked-resize drain keep writing the
    process-wide sender until 4f-e moves it.~~ Moved with the sender (`8733d09`).
    Leftovers, each claimed:
    - **`get_user_input` went to `Content`, not TermIO** (the plan said manager/TermIO): the
      prompt is a content write and the answer goes through the §4 tap — which is what keeps
      quiet's empty answer and the scripted `ScriptOutput` interactive test byte-identical; a
      `TermIO` line read would bypass the tap and block on test stdin. `ConsoleOutput`'s impl is
      a forward; the trait verb stays as the tap seam to step 5.
    - ~~The *reader-stop* half of `suspend()` is still each caller's `ReaderPauseGuard`; the
      manager's pass-through stops raw mode only. Design's `TermIO::suspend()` (pause + raw off)
      arrives when TermIO moves under `Draw` — **4f-e** — and retires `set_mode_guard` with
      it.~~ Landed (`786009e`, `set_mode_guard` with `8733d09`): `TermIO::suspend()` raises the
      pause gate (ownership-latched — an outer holder survives a nested pair) before dropping
      raw mode, and `run_editor`'s `ReaderPauseGuard` folded in.
    - ~~**4a's resize note** rides 4f-e: `route_event` still handles Resize inline (`set_size`,
      no forward to the input channel); moving the sender and the forward together with the
      probe/drain relocation avoids a window where both paths publish. `read_input`'s defensive
      `Resize` arms are already in place for when the forward starts.~~ Landed (`2906fb1`):
      the reader publishes the watch first, then forwards; the prompt arms republish
      idempotently and wait, the turn's dispatch ignores — no window ever had two senders.
    - `run_editor` takes `&Content`, not the bare manager — content is the handle its callers
      (`edit_text`, `open_editor`) already hold; widen only for a manager-only caller.
  - **4f-e — TermIO into Draw: the setup rework.** Landed (`8733d09` wiring, `c7ef4fe` writer,
    `786009e` reader-stop suspend, `2906fb1` resize forward) — the commit messages record what
    moved. Everything the plan named is in: caps probed at construction and fed to `Content::new`
    (the `Caps::probe` site left content — the four `TermIO::*` constructors are the only probes
    now); `Draw { term_io, cache }`, `writer` field dead, every byte through
    `TermIO::write(Stream::Main, …)` with `recording()` on the same edge (`record_bytes` re-pointed,
    `Sink`/`SharedBuf` gone); the size sender per-instance under TermIO's roof (the `LazyLock` and
    `set_size`/`size`/`subscribe` free fns dead), probe + drains through the manager's front door;
    the free-function subscribers settled (see `8733d09`); `TermIO::suspend()` the design's whole
    pause+raw-off; resize both edges. Two recorded deviations from the plan's wording:
    - **Constructed in `main`, not `Interactive`** — forced: `Content::new` runs where
      `AgentConfig` is built, so the backend must pre-exist it. `main` picks the variant (Tty iff
      stdin is a terminal and `!quiet` — the condition `Interactive` derives `repl` from) and hands
      it to `ConsoleOutput::new`; the *hand-out* (pause gate → `handles()` → input to the repl)
      stays inside `Interactive::new`'s ordered entry-DSR window.
    - `Content::new(term_io, …)` takes the backend by value: it derives the size receiver and caps
      before moving `TermIO` down into `RenderManager` → `Draw`, so the plan's step-3 "move TermIO
      into Draw" needed no install step — and `Interactive` reaches the hand-out through
      `manager.handles()`, never naming `TermIO`.
    Leftovers:
    - ~~`TermHandles.caret` still parks in `Interactive.handles`, unconsumed.~~ Landed 4g
      (`e7b17ad`): the manager takes the receiver at the hand-out and drains it in
      `poll_caret`.
    - Design §Degradation's Draw-side degradation — non-tty primaries dropping frames wholesale,
      the stripped-by-caps byte edge — is implemented nowhere and claimed by no phasing step.
      Flagged for a slot (closest home: **step 5**, beside the sink dissolution) rather than
      slipped into this step.
    - The loop-top probe now publishes *inside* the query (one call instead of query … replay …
      `set_size`), so the never-applied `painted_size` shortcut reads the just-published watch
      and a replay formats at fresh width — the watch being current at read time, which is what
      publishing there means (`8733d09` records it).
  - **4f-f — `Stream` + the meta sink; the formatting span unlocks.** Landed (`cbeceed` stream +
    meta sink, `044c7f9` the span) — the commit messages record what moved. `ContentWrite` gained
    `stream: Stream`; the reasoning decision is the hand-off's token now (content picks `Meta`
    once at setup from the caps — `commit_reasoning`; `ReasoningSink` and `reasoning_sink()`
    died). `put_content` routes Main → today's choreography and Meta → `Draw::write_meta` →
    `TermIO::write(Stream::Meta)`: no consume, count, reposition, or capture interception —
    byte-identical to the stderr bypass it replaced, recorded on the meta edge by
    `reasoning_to_meta_lands_on_the_meta_edge` (main's emptiness is the choreography pin). The
    formatting span then unlocked with the proof the 4d note was waiting for: one writer per
    content (the turn runs inline in `prompt()`'s `select!` with no spawn on the path —
    confirm.rs' module doc — and prompt-time verbs run between turns on that same task, capture
    toggles included), plus enumerable `f` closures; a `Content.building` debug tripwire keeps a
    future violation loud where the held lock used to self-deadlock, and the capture read is its
    own short lock. Leftovers: none — the `render(Some(w))` compose + content round trip stays
    out of 4f (**4h**, below).
  Out of 4f: the `render(Some(w))` compose + content round trip (waits for the installed stack,
  **4h**).
- **4g — Suspend/resume + DSR through the manager; dead size fallbacks die.** Landed
  (`c21be0e` verb, `6961d0b` guard, `e7b17ad` caret, `d4113ff` doc).
  ~~`RenderManager::suspend()` becomes the front door~~ — landed 4f-d + 4f-e-c (`786009e`).
  The remainder, all landed here:
  - *Dead size fallbacks*: only `Output::cursor_row`'s trait default still reached for a live
    `crossterm::terminal::size()` (the `frame_size`/`painted_size` halves of the old note died
    with 4f-c). The verb had no trait-object caller left — every re-anchor names
    `manager.cursor_row()` — so verb and forward died together; non-console outputs needed
    nothing (the REPL never runs against them). The two survivors in `termio.rs` are the
    deliberate watch seed (`TermSize::probe`) and the loop-top probe (`probe_size`).
  - *DSR through the front door*: `query_cursor_row` has carried its own `ReaderPauseGuard`
    since step 2 commit C (answered inside the terminal layer, design §TermIO). What was left
    was the entry-DSR window in `Interactive::new` hand-raising the gate with raw `store()`s —
    now `ReaderPauseGuard::raise()` (nesting-safe, lowers on drop; the reader starts inside
    `handles()`, so the raise is instant and the reader starts parked, same ordering).
  - *Caret handle to the manager* (design §3, "RenderManager only"): `Interactive::new` splits
    the `TermHandles` hand-out — input stays with the repl, caret moves into the manager
    (`set_caret_receiver`) — and the prompt loop's old `RESUMED_REANCHOR` flag check becomes
    `RenderManager::poll_caret()`: drains the channel, re-anchors via DSR on a pending
    `Invalidated`. The flag and its belt-and-suspenders `resume()` are gone; the reader's own
    ordering makes them unnecessary (it emits the event only after `guard.start()` and before
    forwarding any later key, same thread). Leftovers: none.
  The loop-top resize probe stays (deliberate, step-2 note).
- **4h — The element stack: `slashdot_tui` crate; `Arc<Mutex<…>>` elements; pull-based rendering.**
  Design-led (design §RenderElement stack + the crate note under §End state layering), kept last for
  runway; split into eight commits, each independently green:
  - **4h-a — crate extraction.** Landed (`ba8d6be`). Create `crates/slashdot_tui` and move `ansi`,
    `termio`, `draw`, `render` wholesale, plus the width utility: `slashdot_menu::table::display_width`
    moves into the crate beside `ansi` (menu then imports it from `slashdot_tui`); `row_count.rs` stays
    picocode-side (content's). The two outward edges of the {ansi, termio, draw, render} SCC are exactly
    these, so the crate knows nothing of repl/content — the layering becomes compiler-enforced. The two
    `crate::repl::LineEditor` test imports rehome: 6 render.rs + 3 draw.rs input-element pipeline tests
    move to repl.rs `input_frame_tests` (the ones that pin manager+input behavior); `Element::status`/
    `input`/`input_hidden` leave the inherent impl and become free fns; `Layout` (the input element's
    data contract) moves beside `Element` in the tui crate. `lib.rs` re-exports
    (`pub use slashdot_tui::{…}`) keep `confirm_probe` and examples source-compatible. The test-only
    seams (`TermIO::recording`, `RenderManager::draw`, `tracked_row`, `fixed`) gate on a
    `test-helpers` feature. Behavior-free.
  - **4h-b — `RowAsk`/`RowSpec` on the trait.** Landed (`ddf5522`). `RenderElement { height(&self,
    width) -> RowAsk; render(&self, height, width) -> Rendered }`; `RowAsk { min, max }`,
    `RowSpec::FixedNum(n)` / `RatioAndNum(r, n)`. The allocator resolves specs against the frame
    height (`FixedNum(n)` → n, `RatioAndNum(r, n)` → min(ceil(r × term_h), n)); budget = min(ratio ×
    term_h, Σ resolved max — the fit branch is that min); floors first, then the split.
    Construction-time `must`/`might` fields die with `Element::extent()` — every element answers
    `height(width)` itself. All-`FixedNum` reproduces today's even-split semantics; `allocate_*`
    stay as parity pins (rewritten to ask in `RowAsk::fixed`; `term_h` is inert for a fixed ask).
    `RatioAndNum` keeps the sketch's `f32` — no deviation: an in-frame ratio is a *preference about
    a share*, resolved into integral rows either way, so precision is not the point. What the point
    is came out of the review that started as an `f32`-vs-`f64` question: **one place computes each
    number.** The frame's row count is `RenderManager::budget`'s alone, so `live_budget_ratio` went
    private and `live_budget` is test-only — a production caller that needs the number the next
    `render` budgets with asks the manager for it (`budget` went `pub` for that). The one such
    caller today is the page grant: `rows_grant`/`handle_key` take the budget as a parameter
    (alongside `opts_len`, which is passed for the same reason) instead of re-deriving the ratio, so
    the grant cannot disagree with the paint. No test now pins a budget-vs-element ratio agreement
    either — by construction rather than by agreement. Leftovers, each claimed:
    - ~~`InputItem`/`SectionItem`/`RowsItem` answer `height(width)` from a compose-width snapshot
      (the `Layout` / `PaintedSection` / engine heights), so `width` is presently inert for them.~~
      The menu sections landed with **4h-d**: `SectionItem`/`RowsItem` are gone, the crate's
      detail/header/rows elements lay out at the render width inside `render(h, w)`, and their ask
      reads `width`. `InputItem`'s snapshot closed with **4h-f** — the element holds the editor
      handle and lays out inside `height`/`render` at the width it is asked at.
    - ~~`InputItem` carries the composer's `hidden` flag (a push fact, set where the mid-turn rule
      decides).~~ Design §RenderElement stack: turn presence rides `Content::streaming`, read at
      pull time — landed with **4h-f**: the element holds the flag handle next to the editor and
      the R1 rule lives inside its `height`.
    - `Extent` survives as the *resolved* pair (plan §3.2's `must`/`might`) one step below the
      element's `RowAsk` — the allocator's own vocabulary; no caller outside `allocate` reaches it
      now.
  - **4h-c — the bar moves into `ui/`.** Landed (`280f654` + `2651b19`). `ui.rs` → `ui/mod.rs` +
    `ui/bar.rs`: the bar element (`StatusItem`), its constructor, `status_bar` and `dot_cell` come
    out of `output.rs` (`Element::status` and its `StatusItem` die there; the bar element holds the
    raw store handles as today — `Arc<Mutex<BarStatus>>` + the streaming flag, both halves of the
    drawing still resolved in its own render). `output.rs`'s render imports shrink to `Frame`.
    One addition beyond the sketch: `StatusText` (the shaped pair `status_text` builds and
    `status_bar` fits) moved to `agent/status.rs`, beside the shaping — with the fit in `ui/`,
    leaving it in `output.rs` would have both `agent` and `ui` depending on `output` for a pure
    data type; now `ui → agent::status` matches the existing `content → agent::status` direction.
    `agent::status`'s tests assert against `ui::bar::{status_bar, BAR_SEPARATOR, DOT_STREAMING}`.
   No `ui/menu.rs`: everything else `ui/mod.rs` holds went to the menu crate with the element impls
   in **4h-d** (`window_keeps` rides along beside `window_heights` — pure windowing, its only user
   is the rows element). `pad_row`/`el_if_short` were already `slashdot_tui`'s (`pub`) — the
    sketch's precondition, no change needed. With **4h-d** landed, `ui/mod.rs` is only `bar`.
  - **4h-d — menu views mint their own elements** (`slashdot_menu`). Landed
    (`a2ebeff` + `063d993` + `8daa008`), in three commits:
    1. `s/Rc/Arc/` on the handle chain (`a2ebeff`) — `ViewRows`, `DataResponse`, `CompletionRows`,
       the `RowView::plan` signature; `Rc::ptr_eq` → `Arc::ptr_eq` (dirty-check semantics
       unchanged). Pure widening: the async-trait `DataResult` returns become `Send`, never less.
       `RefCell` sites stay `Rc` — node-internal (`node_impl.rs:426` active_ids, node_entry test
       spies), never leave the thread.
    2. `RenderElement` impls for a view's sections (`063d993`) — detail, table header, rows, one
       element each, minted by `RenderPlan::elements(row_min)` (the plan grows `view`/`columns`
       fields; a hand-built plan leaves `view: None` and gets the pre-laid-out-lines elements the
       confirm dialog and tests still want). The rows element holds the view handle
       (`Arc<ViewRows>`, auto `Send + Sync`) and paints through `iter()` inside `render(h, w)` at
       the render width — the iterator logic (order-driven resolution, stale-slot skips,
       active-flag lookup) runs *in* the element; rows are never copied into it, and
       `RowView::plan`'s ptr_eq rebuild caching is untouched. `RowView::rebuild_rows`' body is
       extracted verbatim as `assemble_table`, shared by the plan cache and the elements so the
       paint cannot drift from the rows the page keys page over. Detail/header hold raw
       config/data and format at render width. The layout is `TableLines`, shared by the header
       and rows elements and keyed by width, so a cursor move repaints the window (O(window))
       without re-laying-out the table (O(table)).
    3. The menu layering returns the element set for a view; the repl appends it to the configured
       stack (`8daa008`). `repl::overlay_elements` drops its `term_w` and dispatches to
       `plan.elements(1)`; the confirm dialog passes its `OPTIONS.len()` floor through
       `plan.elements` (whose `selected` is the dialog cursor, kept in sync with `self.sel`). The
       compose-width `PaintedSection` and `ui::overlay_elements*` die; `ui/mod.rs` is now only
       `bar`.
    `window_keeps` moved to `elements.rs` beside `window_heights`, and the overlay element tests
    moved to the crate beside the elements that back them.
  - **4h-e — selection behind shared state.** *(landed — `14542025`, `744a8dfe`, `25730624`)*
    Menu open constructs the element set once; a keystroke moves the selection through a shared
    handle in place (no push, no rebuild — the engine's plan call still runs, but only the
    selection figures flow to the handle); rows that actually change (filter, new plan) rebuild
    the affected element and reconfigure the stack. The set's plan is refreshed by the same
    `update`, so host cursor math reads the freshest engine answer.
   - "Reconfigure the stack" is per-paint today (`compose_area` re-runs per frame); it collapses
     with **4h-g**. `Element::ptr_eq` exists under the test-helpers seam only (production has no
     need to name element identity).
   - Panel-shape rules, pinned by tests in the crate and in the repl: a detail section appearing
     or vanishing (an error line arriving on an unchanged view) is a rebuild, not a figure move.
  - **4h-f — the input element.** *(landed — `08ed747` + `be0e7b4`)* `PromptSession::editor` is
    `Arc<Mutex<LineEditor>>`, shared with the input element (label + streaming flag alongside);
    layout/wrap computes inside `height`/`render` at render width. Height rules: `0` mid-turn with
    an empty buffer; the prompt row between turns even with no content; sized to the wrapped buffer
    whenever content is present, in either case. Turn presence rides `Content::streaming` — a
    pushed fact, read at pull time inside the element.
   - The mid-turn pixel check (the open question from design discussion): verified preserved. The
     turn pumps' repaints all go through `paint_prompt` → the manager's one synchronous
     compose→allocate→render, so the flag read at ask time is the same fact the old compose-time
     push read — no await, no interleaving. The one place the pixels *improve* is the manager's
     recipe re-render (resize reposition): it now re-lays-out the input at the new width instead
     of replaying a stale compose-width snapshot; the recipe dies with **4h-g** anyway.
   - `Layout` moved home from `slashdot_tui` — with the element pulling the editor, both sides of
     the contract (`LineEditor::layout` produces, `InputItem` consumes) are repl's.
  - `emit_commit` resets the buffer *before* the redraw rather than after (the closure paints
    through the shared handle now; every caller passes `""`, so same pixels).
  - **4h-g — the flip.** *(landed — `ac92095` + `72a3ac7` + `fb3a793`, gate `799665f`, probe
   `243890e`)* The manager's `render` takes only content and composes from the **configured**
   stack; membership lives on the manager, changed by the repl on *state transitions*, never
   recomputed at paint. Bar is always present (no `is_off` gate) and pulls **live** agent state
   (shared `usage` mutex), which killed the mid-turn context staleness. Git summaries are the
   record of what landed; what follows is only what the step left behind.

   **The model (settled here, reflected in the design doc §UI):** *membership is reconfigured only
   on user input or streamed events, by the repl; paint just renders whatever is configured.* The
   transition points are the callers of `PromptSession::sync_stack` — overlay open/close/change,
   confirm open/close, submit clear, `emit_commit`'s redraw — each a `sync_stack` immediately
   followed by a `paint_prompt`. The input element is 4h-f's stable shared handle and never changes
   membership; `ActiveOverlay` variants carry `ElementSet` by value (elements moved in, not rebuilt
   per paint), so membership genuinely changes only at those transitions. Paint composes nothing.

  **Follow-ups this step leaves:**
  - `sync_stack` takes a `label: &str` (the prompt text) the design's signature does not carry —
   the input element's label is not constant across a session's life, so it is threaded at the one
   moment membership is built rather than stored. The design doc should either add it or say why
   the label is not part of the membership it describes.
  - The `estimated_ctx` → `usage.last_input_tokens` seeding (turn start, and the prompt cycle) is
    superseded by **4h-g4**: the estimate gets a home of its own and the fallback moves into the
    read (below).
  - `AppState.session_usage` and `update_cycle_view`'s usage copy are gone (part 2's cleanup ran).
    The earlier note that `context_tokens` stays for `/tree` was wrong — `/stat` is its only reader;
    **4h-g4** removes it.

  - **4h-g4 — the bar pulls every fact; the estimate fallback is shared.** *(planned)* **4h-g** left
    two kinds of fact on the push. The model fields (provider/model/thinking/context_limit) and the
    identity (session name / first message) still arrive via `set_status`, stamped at turn start and
    once per cycle — so a mid-turn `/reasoning` or `/model` commit (which lands on `AppState` in
    `settle`, pumps.rs:147) never reaches the bar; the running turn keeps its `live_params` and the
    bar shows the old level until the turn ends. And the context figure has no fallback of its own:
    the estimate is *seeded into* `usage.last_input_tokens` at two sites (turn.rs, repl.rs), a write
    that impersonates a provider report. Both are one defect — the bar holds a snapshot where it
    should hold stores.

    **The model (settled here):** the bar holds three stores and assembles its `BarStatus` **value**
    at render. (1) `AppState` — provider/model/thinking/context_limit through the existing sync
    accessors, and identity through two new cycle-written fields. (2) the shared `usage` mutex —
    the context figure, plus a new `estimated_input_tokens` fallback. (3) `Content.streaming`,
    unchanged. `Content.bar`, `set_status`, and both pushes die; `BarStatus` survives only as a
    render-time value, so the shaping tests keep working.

    - **Part 1 — the model fields go live.** The bar element takes the `AppState` handle
      (`PromptSession` already receives `&Arc<AppState>` in `new`; store it, and hand it to
      `ui::bar::status` in `sync_stack`). Render reads provider/model (`AppState::model`/`params`),
      thinking (`AppState::reasoning`), and `context_limit` (`resolved_context_limit`). Kills the
      model fields in the turn-start push and in the cycle push. A mid-turn `/reasoning` or `/model`
      commit now shows at the next paint — the staleness the **4h-g** live context pull did *not*
      cover.
    - **Part 2 — identity moves to `AppState`.** `update_cycle_view` carries `session_name` +
      `first_message` (the loop computes both at cycle build, interactive.rs:1046). The turn-start
      push's turn-1 gap (branch empty at cycle time, input not yet persisted) is closed by the loop's
      submit path setting `first_message` to the submitted input when it is still unset — the
      `first_user_text(prior).or(input)` fallback, relocated from turn.rs. The bar reads both from
      `AppState`.
    - **Part 3 — one estimate home, one fallback read.** `SessionUsage` gains
      `estimated_input_tokens`. The two seed sites become honest estimate *writes* (an estimate is
      recorded as an estimate; nothing impersonates a provider report). Both readers call one
      helper — `context_figure(usage) = last_input_tokens if > 0 else estimated_input_tokens`.
      `/stat` calls it too, so its `context_tokens` parameter (today `AppState::context_tokens`)
      goes, and with it `AppState.context_tokens` and `update_cycle_view`'s copy.

    **Gates:** the mid-turn context test (`799665f`) stays green; a mid-turn `/reasoning <level>`
    commit changes the bar bytes at the next paint with no push; the bar shows the estimate before
    the first completion and the provider figure after, with no seed write.

    - Follow-up: a mid-turn `/session rename` still lags one phase — `session_name` is a
      cycle-written snapshot, not a name handle. Closing it means the rename node writing `AppState`,
      which the design's "reconfigure on transitions" model does not yet require.
  - **4h-h — remaining callers + cleanup.** *(landed)* The confirm dialog owns its selection state
   (`ConfirmPrompt::sel`) and takes its place in the repl-configured stack (`sync_stack` extends the
   overlay with `confirm.elements()`); the out-of-crate probe reached the flipped `render` in
   `243890e` and no longer touches `draw_frame`. No production `render(Some(stack))` and no dims
   param remain. `overlay_elements*` went in **4h-d**.
  - Resolved during design (recorded here so they don't get re-litigated): RowAsk/RowSpec lands with
   the stack (**4h-b**), not later; dims flow TermIO → Render exclusively (landed earlier in **4h-g**
   — the `render` no-dims sub-split, `10224a1`; the *flip* sub-split that follows keeps it); the input
   element is always in the stack with height carrying presence (**4h-f**); menu elements hold
   `Arc<ViewRows>` directly after the crate's `Arc` swap (**4h-d** — landed), preserving the
   no-copy row iterator for filtered and unfiltered views alike.

  Deferred beyond 4h — surfaced by **4h-f**, owned by no remaining sub-step:

  - **Engine re-mines its view handle per cursor eval.** `StaticListingNode::data()` mints a fresh
    `DataResponse` on every call, so `refresh_data` always reports change, `slots_dirty` stays
    set, and `ensure_rows` hands back a fresh `ViewRows` each eval. `ElementSet::update`'s
    `same_panel` then sees a new handle and rebuilds instead of moving figures in place — so
    **4h-e**'s no-rebuild ideal holds in the crate's tests (where a plan carries a stable
    `Arc<ViewRows>`) but not on the production path. Pixels are right either way; what is lost is
    the flicker-free carry. Fixing it means memoizing the child `data()` response (return the same
    `Arc` when unchanged), which is engine-side and cuts across nodes — not a rendering-step
    concern. A repl-level test asserting the carry was dropped in **4h-f** because it encoded
    this memoization as fact before it is one; it should return once the engine memoizes.

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
  (As landed in 4g, `e7b17ad`: the lazy drain is `RenderManager::poll_caret()`, called at the
  prompt loop's top — the old flag-check site — rather than inside `render()` itself; the repl
  still neither touches the channel nor issues a DSR, it just pokes the manager's verb.)
- **Resize propagation** — bottom-up via `TermIO`'s `size` watch (RenderManager + content) and the
  repl `input` channel. Ported to the *hook* side first: `RenderElement::height`/`render` are
  width-aware, so elements adapt to a width change without a repaint-per-event.
- **Re-draw triggers** — push from the repl (status/streaming/keystroke state); the bar element
  pulls its own shared state at `render()` time.

## 10. Dead taps

`LogOutput` — structured logger with zero users (only re-exported): delete (step 5), or keep as a
content tap. An `Output` impl, not a `TermIO`. (`ScrollbackOutput` already removed — `5015829`.)
