# The confirm dialog is a menu in the area

Extracted from `doc/ui_draw_v2.md` §4.5 **U3** (the delivery item — *not* the
P3 composition item, which landed and is why the audit table reads "✅" for a
unit that is not done). ui_draw_v2 is overloaded; this is the working design.

Status: **implemented.** The amendments below are the places the tree diverged
from the design as first written, each with the reason.

## 1. The problem

`ui::confirm` (ui.rs:64) is a blocking, self-driving dialog: raise the reader
gate → entry DSR → compose `[Divider?, Overlay]` → `event::read()` in a loop
until Enter/quick-key/`Esc` → `Frame::empty()` erase → decision line.

It has to be all of that because of where it is called from. `Guard::call`
(agent.rs:3012) is reached as
`stream.next()` → rig `drive_tool_calls` → `run_single_tool` →
`tool_snapshot.dispatch(...).await` (rig runner.rs:709), all inside the
`async_stream` that **is** the select's stream arm (agent.rs:404). There is no
`tokio::spawn` on that path. So the tool call is awaited **inline on the same
task as the `select!`**, and while a confirm sits on the stack the loop cannot
poll its other arms.

Two consequences, both pure overhead:

- The main task becomes a **second stdin consumer**, so the reader thread must
  stand down — `confirm_gate` (output.rs:836), and with it the whole
  "a gate left raised freezes the session for the rest of the run" hazard class
  (plan §9 latent issue 1) plus its release-on-every-exit discipline.
- The dialog needs its **own anchor** (the entry DSR, ui.rs:99) and its own
  erase (`Frame::empty()`, ui.rs:428), because it is not the area's composer
  and cannot be one.

Neither is a drawing problem. Both are consequences of the dialog being a
*loop* instead of a *state*.

## 2. The move

**Convert the blocking confirm into an `await`.** The stream arm then returns
`Pending`, the loop is free, and the confirm becomes ordinary session state that
the existing pump already knows how to drive.

```text
Guard::call                       prompt()'s select!              PromptSession
────────────                       ───────────────────             ────────────
!yolo && !approved(tool):
  port.request(msg) ──mpsc──▶   arm: req = rx.recv()
  reply.await()                    pending = Some(req)
      ▲                           (stream arm now Pending,      overlay element
      │                            keys keep arriving)           appended to the
      └──── oneshot send ────── resolve                         default stack
```

`ConfirmPort` = `mpsc::UnboundedSender<ConfirmRequest>`;
`ConfirmRequest { header, subject, reply: oneshot::Sender<Accept|Deny> }`.

- **Sender** is cloned into each `Guard` at `build_rig_agent` (the 7
  `guard(...)` sites, agent.rs:2932-2956). Absent ⇒ piped/batch, and `Guard`
  keeps today's `Output::confirm` path unchanged.
- **Receiver** is `&mut`-borrowed per turn, exactly like `InTurnInput::keys`
  (repl.rs:908/915) — it dies with the turn, so a request can never outlive its
  pump.
- **Reply channel** is a per-request oneshot, not a shared one: there is nothing
  to correlate, and a dropped sender (turn cancelled with a confirm pending) is
  simply `Deny` — the safe default, and the same answer `Esc` gives.

Unbounded, for the reason `route_event` already records (repl.rs:148): a bounded
channel that filled would block the sender at the worst moment.

### 2.1 Only one select arm

A confirm blocks its own tool, so **one can never be pending during a retry
back-off** — by the time `cancellable_sleep_pumped` runs, the confirm resolved
and the tool returned. The new arm therefore goes in `prompt()`'s select only,
not in the back-off pump (U4.5) or the compaction window pump (U4.6). Both keep
working unchanged, for the same reason: no tool runs in either.

## 3. Composition

The confirm contributes `overlay_elements(&plan, tw, Some(sel))`
(ui.rs:364) — the **same** builder the slash menu uses — appended after the
menu's own elements. Its `RenderPlan` is today's `build_confirm_plan`
(ui.rs:457). Default stack: `[Status?, Input?, Divider?, Overlay…, Confirm…]`.

Append, not replace (#3): the menu drives the prompt, so clearing the prompt to
make room for the confirm would fight the menu it is layered over. The menu's
epoch/handle survive a round-trip through the confirm untouched.

### 3.1 `must: 3` on the option rows

The options must always be visible — a confirm you cannot read is not a
consent prompt. So the confirm's rows element carries `must = 3` (or 4 counting
the header) rather than `data_rows`' current `must = 1` (ui.rs:229).
`allocate` (frame.rs:211) runs `fill(&mut heights, &musts, budget)` **before**
the `might` pass, so this is the same protection the existing pin
`header_element_extent_yields_to_a_row_under_starvation` documents for the
header — just pointed the other way.

Caveat to state in the test: `fill` degenerates to *one row each in element
order* when the budget is below the sum of the `must`s, and the rows element is
last, so "always visible" means "whenever the terminal has ≥3 spare rows", not
"at any height". The pin should assert the realistic case and say so.

## 4. The option list moves to the repl (#2)

`ConfirmSpec` has exactly **one** production caller today
(`ConsoleOutput::confirm`, output.rs:3798), so its generality is unearned. The
request carries `{header, subject}` and nothing else; the repl owns the option
list, the quick keys and the default selection:

```text
? Confirm tool BASH call?
  yes              Yes
  ❯ no             No                  ← default (deny)
    always         Always (session)
```

Order and default are **unchanged** from the old `ConfirmSpec` (which listed
yes/no/always with `default_sel: 1`) — moving the dialog should not move what
the user's hands expect.

Quick keys are the table `[('y', 0), ('n', 1), ('s', 2)]`, *not* derived from
each id's first letter. The obvious derivation is wrong for one of them:
`always` starts with `a`, and the key that has always meant it is `s`
(session). **Amendment:** the first draft of this doc claimed the ids
generated the keys; the pty test caught it immediately.

### 4.1 `Always` is the repl's business too

Today `Confirmation::Always` → `self.always.store(true)` in the `Guard`
(agent.rs:3028), and the agent is the only thing that reads it. Under this
design **the agent never learns which option was chosen**: the repl persists the
approval and answers `Accept`.

Because the guard must then skip the dialog on later calls, the skip needs a
store the guard can read. That store *is* the port:

- `ConfirmPort::is_approved(tool) -> bool` — a session-scoped set the repl
  writes when `Always` is picked and the guards consult before asking.
- The guard's `always: Arc<AtomicBool>` is gone **from the interactive path**,
  and so is the `Confirmation::Always` arm there: the port's reply type is
  `Accept | Deny`, not `Confirmation`, so "accept" is genuinely all the agent
  learns.
- **Amendment — the field survives on the no-port path.** With no REPL there is
  no pump to record an approval, so `Guard::always` is what makes
  `QuietOutput`'s `Always` ("s" = session) still mean *for the session* rather
  than *just this once*. It is now reachable only when `confirm` is `None`
  (`Guard::already_approved`), and an interactive session never reads it.

**Open question — lifetime.** Today's `always` is an in-memory
`AtomicBool`: per tool, per process, gone on exit. Moving it to the repl is not
a lifetime change, but the user asked for "settings persistence", and there *is*
a settings surface that already means "don't ask me about these"
(`auto_allow` in settings.toml, `Config::bash_auto_allow`, config.rs:955/1509).
Wiring `always` into it would make an approval **outlive the session**, which is
a product decision, not a refactor. Default for this work: **session-scoped**,
matching today, in a repl-side `Approvals` set keyed by tool name (per-tool, so
`always` on `Bash` still does not approve `Edit`). Say the word if it should
persist.

Note this also makes `yolo` a candidate for the same treatment — the port could
short-circuit it and `build_rig_agent` could drop its `yolo` parameter. Out of
scope here: it is threaded through 7 `guard(...)` sites *and* every test
`Output`, and `security_mode` is already a live settings node.

## 5. Key ladder (#4)

```text
Esc → 1. resolve the confirm      (Deny)
     → 2. step the v2.5 menu ladder     (already the case)
     → 3. cancel the turn           (root only, already the case)
```

The new rule is rule 1, and it has to come *ahead* of the existing root-`Esc`
cancel in `dispatch_in_turn_event` (agent.rs:1010) — today that check is
unconditional, so without the precedence a single `Esc` would dismiss the
dialog *and* kill the turn.

`Ctrl-C` / `Ctrl-Z` / `Resize` are unaffected: `route_event` (repl.rs:126) takes
them inline in the reader, before anything reaches the channel.

## 6. Resolve

Drop the element, repaint, write the decision line as content — the shape
`emit_commit` already has (repl.rs:2097):

1. `settle` with the confirm cleared → the area is `[Status?, Input?, Divider?,
   Overlay…]` again, and the frame apply's union clear takes the confirm's rows.
   **That is the erase**; there is no `Frame::empty()`.
2. `display_plain("accepted tool BASH" | "denied tool BASH")` at the caret the
   apply left.

`subject` still comes from `confirm_subject` (ui.rs:451) — same convention
stripping, now computed on the request side.

## 7. First in-turn submit

`dispatch_in_turn_event` drops `KeyOutcome::Action(_)` mid-turn, silently
(agent.rs:1031) — v0 blocks mid-turn submit. This design introduces the **first
in-turn `Enter` that acts**: it resolves the dialog. Two consequences worth
carrying into the doc rather than rediscovering later:

- The blocked-submit rule needs its exception stated, not worked around: while
  a confirm is open, `Enter` is the dialog's, and only the dialog's.
- It is the first worked example of the shape mid-turn submit will need — a
  request for the pump to service, answered back into the awaited future —
  which is why it belongs here and not in a later unit.

## 8. Deletes / retires

- **Gone**: `ui::confirm`'s blocking body: gate raise/release, entry DSR,
  `event::read()` loop, `Frame::empty()` resolve
  - **Where**: ui.rs:64-166, 414-445
- **Gone**: `confirm_gate` **for confirms** (the `$EDITOR` hold in editor.rs:35
  stays — a foreign writer really does own the tty)
  - **Where**: output.rs:836, 1031
- **Gone**: `ConfirmSpec`, `ConfirmResult`, `quick_key_index`,
  `build_confirm_plan`, `draw_dialog`, `resolve` — the option list is the repl's
  now
  - **Where**: ui.rs (all)
- **Gone**: `Guard::always` + the `Confirmation::Always` arm,
  **on the interactive path** (kept for no-port; §4.1)
  - **Where**: agent.rs
- **Gone**: `Frame::empty()` — the erase was always "apply a frame with no
  rows", and the area now simply stops containing the dialog
  - **Where**: ui/frame.rs
- **Gone**: `dispatch_in_turn_event`'s unconditional root-`Esc` cancel (becomes
  the third rung)
  - **Where**: agent.rs:1010
- **Gone**: **U5's dialog half** — the `ui.rs:92` entry DSR — and with it the
  last open item in `doc/ui_draw_261001.md`
  - **Where**: ui.rs:99

Also: the "`Confirm tool … call?` gate left raised freezes every later in-flight
reader" hazard (plan §9 latent issue 1) stops existing, because there is no
longer a second stdin consumer to coordinate with.

## 9. Test plan

`tests/confirm_raw_mode.rs` + `src/bin/confirm_probe.rs` exist to pin *"the
dialog does not hang in raw mode"* by driving `ConsoleOutput::confirm` in a pty
(confirm_raw_mode.rs:1-13). There is no longer a standalone dialog to drive — so
this is the real cost of the change and it is not optional:

- **Rewrite the probe** to drive the *repl pump*: request a confirm through the
  port, feed the same pty key, resolve. It must call the production dispatcher,
  not a copy — a mini-repl in the probe would pin the probe.
- **Keep the anti-hang property** (the original regression: `ICRNL` off means
  `read_line` waits forever). That is the test's actual value and it survives.
- **New positional pin.** U3's own verify line demands one and the current suite
  cannot supply it: "the confirm suite asserts *behaviour* (which option was
  chosen) and not the dialog's row, so green is not evidence for anything
  here". A vt100 assertion that a mid-turn approval shows the input row above it
  and that the area returns to `[Status, Input]` on resolve.
- **`must: 3`**: options visible under a starved budget (§3.1).
- **`Esc` precedence** (§5): one `Esc` dismisses, does not cancel.
- **Dropped reply** ⇒ `Deny`, no hang.
- Existing: every test `Output` implementing `Output::confirm` stays valid,
  because the fallback path is untouched.

## 10. Non-goals / open

- Mid-turn submit stays blocked apart from the dialog (§7).
- `/tree … compact`'s compaction-review confirm is mentioned throughout
  ui_draw_v2 but has **no caller** in the tree today; if it comes back it
  composes the same way, and its `subject` is the request's business.
- **Behaviour change on the no-pump paths.** With the blocking driver gone,
  `ConsoleOutput::confirm` is now the *policy* and denies. So a one-shot
  `prior "q"` / `Recipe` run with a console output that reaches a gated tool
  without an `auto_allow` pattern and without yolo is denied where it used to
  prompt. Denying is the right direction (it cannot run a tool the user never
  agreed to, and the dialog was never reliable without a REPL pump), but it is a
  change and not a silent one.
- `yolo` through the port: available, not taken (§4.1).
- `Always` lifetime: session by default, persistence on request (§4.1).
