# `src/agent.rs` — refactor

Status: **done.** `agent.rs` 11,889 → 1,685; `interactive.rs`'s
`run_interactive` decomposed into an `Interactive` struct. Done: step 1
(`run_interactive` → `src/interactive.rs`), 2a `errors`, 2b `pumps`, 2c
`status`, 2d `msglog`, 2e `replay`, 2f `compaction`, 2g `transcript`, 2h `build`,
2i `turn`, and step 3 (`Interactive` struct: `new` / `converge` / `handle_submit`
/ `run_bang_command` / `run_prompt_turn` / `run`). `CodeAgent::prompt` landed in
`agent/turn.rs` as a pure move (§4).

## 1. What the file is

11,889 lines. 93 top-level `fn`s, 330 total, 43 `pub`/`pub(crate)` items. Not one
module — **seven or eight distinct concerns concatenated**, with only two
section-divider comments in the whole file (lines 3335, 4225).

45% is tests: `mod tests` (4,814) + `mod status_bar_tests` (510) = 5,324.
Production code ≈ 6,565.

### Measured clusters (current line numbers)

| Lines | Count | Concern |
|---|---|---|
| 57–186 | 130 | `SessionUsage`, `TurnPersistence` |
| 186–947 | 761 | `CodeAgent::prompt` (**643-line fn**) + `run_once`/`summarize` |
| 941–1653 | 713 | error classification + retry policy |
| 974–1307 | 334 | cancellation pumps — **interleaved with the above** |
| 1674–2154 | 481 | transcript accumulation + message shaping |
| 2154–3080 | 927 | `AgentConfig`, `create_agent`, `LoggingHook`, `build_rig_agent`, `Guard` |
| 3263–3335 | 73 | token estimation |
| 3335–3821 | 487 | session replay |
| 3821–4678 | 858 | resume prefill + compaction + branch summary |
| 4678–5056 | 378 | session lifecycle + rebuild gate |
| 5056–5947 | 892 | `run_interactive` (**892-line fn**, the REPL's main loop) |
| 5956–6827 | 872 | status bar + `/stat` rendering (pure functions) |
| 11784–11889 | 106 | `MessageLogger` (after the tests, at EOF) |

Two findings shape everything else:

1. **Boundaries are not contiguous.** `classify_prompt_error` (941–973) is
   separated from its own continuation (1307–1653) by the entire pump block.
   A mechanical line-range split produces modules that don't compile.
2. **The two big functions are the real problem, not the file.** 1,535 lines
   between `run_interactive` and `CodeAgent::prompt`. File-splitting does not
   shorten either. 44 of the file's cross-cluster reference edges point at
   `run_interactive`.

Coupling is shallow and one-directional: of 43 public items only ~20 have
external users (`repl.rs`, `app_state.rs`, `main.rs`, `v25_*`). No module
imports *from* `agent` except for type-level uses.

## 2. Cost of a split — measured, not estimated

Extracted `errors.rs` (386 lines) and `pumps.rs` (349 lines) on a throwaway
branch, then reverted. Result: **1023 tests pass, zero warnings, `cargo fmt`
clean.**

- **~14 lines added** to `agent.rs` (`mod` decls + two `use` lines) — the
  entire ongoing maintenance cost.
- 7 `fn` → `pub(crate) fn`, 6 struct fields → `pub(crate)` (`TurnPump`; its
  test constructs the struct literally — better fix is a `TurnPump::new`).
- 2 import headers hand-written.
- **2 iterations of `cargo test`** to converge.
- **0 changes in any other file.** 107 `crate::agent::` call sites across 11
  files untouched; tests use `super::`, so a `use` re-export keeps them
  resolving.

## 3. Plan

C first — it inverts the obvious order. Doing A first would put
`run_interactive` in `agent/interactive.rs` and then immediately move it again.

### Step 1 — C: `src/interactive.rs` (~1,010 lines, 7 tests)

Move `run_interactive` + `plan_rebuild` + `RebuildFingerprint` +
`load/save_recent_models` to top level. Verified: **no back-edge** (nothing in
`agent/` references it but doc comments), one external caller (`main.rs:378`)
plus the `lib.rs:58` re-export, 21 items resolved by one `use` header.
`plan_rebuild`'s only apparent external user is a comment in `app_state.rs`.

Costing note: `resolved_context_limit` is defined in the *status* cluster
(6046) but used by `run_interactive` (5171/5303/5409) — it stays in
`agent/status.rs` and is imported. Watch this if the status split lands first.

Carries 7 tests: `bang_command_uses_sandbox_before_agent_created` (9668),
`non_repl_slash_lines_run_through_batch_engine` (9822), and 5 `plan_rebuild`
tests (11223–11331).

> **Coupling warning.** `bang_command` is `#[serial]` because it holds a
> `TOOL_CANCEL_SENDERS` registration that the Esc-pump tests broadcast to.
> Those pump tests move in step 2b. The two are joined by process-global
> state, not by code — keep them in view of each other.

### Step 2 — A: the cluster split (9 files, ~10,150 lines)

Cheapest and least-coupled first; every commit independently green.

| | File | Lines | Note |
|---|---|---|---|
| 2a | `agent/errors.rs` | 386 | verified in §2 |
| 2b | `agent/pumps.rs` | 349 | verified in §2; see coupling warning |
| 2c | `agent/status.rs` | 880 | carries its 510 `status_bar_tests` |
| 2d | `agent/msglog.rs` | 106 | self-contained, at EOF today |
| 2e | `agent/replay.rs` | 487 | |
| 2f | `agent/compaction.rs` | 858 | largest remaining |
| 2g | `agent/transcript.rs` | 481 | |
| 2h | `agent/build.rs` | 927 | |
| 2i | `agent/turn.rs` + `agent/mod.rs` | ~1,000 | most entangled — last |

Ends at `agent/` ≈ 4,000 lines, largest file under 1,100.

### Step 3 — B: `Interactive` struct (now in `src/interactive.rs`)

The loop carries ~15 cross-iteration locals (`config`, `last_attempted_target`,
`prompt_session`, `guard`, `app_state`, `repl`). Group into `struct Interactive`
with the three visually distinct regions as methods:

- `converge()` — 5367–5516, AppState→applied-config convergence + rebuild
- `handle_submit(input)` — 5524–5943
- `run()` — the loop itself, ~20 lines

Then split the `Submit` arm on its own seams: the `!cmd` shell path
(5579–5663) and the key-pump + retry loop (5664–5943).

## 4. Deferred: `CodeAgent::prompt` (643 lines)

Unchanged, and it ends up alone in `agent/turn.rs` at step 2i — which is the
right resting place for it.

The `tokio::select!` (412–806) has four arms, and the retry/de-dup logic
around it is intricate (see the comment block at 559–590). `prompt()` runs
under `doc/turn_cancellation_persistence_bugs.md`, which suggests it has
already been bitten once. The risk/benefit is unfavourable for a change framed
as cleanup. Split it deliberately, with the full suite.

## 5. Visibility widening — probe during step 2, not up front

A split widens ~25 private items to `pub(crate)`. Do this **as each cluster
lands**, not as a separate pass first: the compiler enumerates the exact set
for that cluster, so the list is free and stays accurate. Widening everything
up front would make `pub(crate)` the default and hide which widenings are
load-bearing.

Two are worth pushing back on when they come up, because crate-visibility
would be an artifact of the split rather than a real API:

- `first_user_text` — one caller (`prompt`, line 256). A parameter, or a
  method on the status-bar model that already takes a `Message` list.
- `effective_params` — one production caller (`run_interactive`, line 5555;
  the rest are tests).

Neither is forced by the module boundaries; both just happened to sit in the
wrong file. Resolving them properly costs one commit and keeps the crate
surface honest.

`TurnPump`'s six fields are the other case worth catching early: the probe
widened all six because a test constructs the struct literally. A
`TurnPump::new(...)` constructor keeps them private and is strictly better —
do that in 2b rather than shipping the widened fields.

## 6. Test placement — budget a full read

`mod tests` is 4,814 lines over 9 concerns, and the split means placing every
one of them. Treat this as a **reading pass, not a mechanical move**:

- Some tests don't obviously belong to the cluster their code sits in — the
  `ensure_session` / `new_and_clear` tests (10807, 10916) exercise interactive
  state, `message_logger_writes_jsonl_file` (9240) sits far from `MessageLogger`.
- Several clusters are already internally sectioned (`// ---- compaction: retry
  + generate/apply ----` and seven more), which gives a placement order but not
  a placement guarantee.
- Moving a test can surface a test that only passed because of shared module
  state. That is a finding, not an obstacle — but it needs someone who has
  read the test to be worth anything.

Budget a full pass over all 4,814 lines before step 2d, and treat the result as
a placement table to review, not a file-move to run.

## 7. Net

~4,100 lines out of `agent/` (34%); `agent/` 11,889 → ~4,000; largest single
function 892 → ~420; tests next to their subjects. ~12 commits, each
independently green and revertible. Only ordering constraint: **2b must not
land without awareness of step 1's `bang_command` test.**

Doing the highest-information step first is the point of the C-first order: if
C turns out to have a surprise, you learn it before making nine other moves.

---

Related: `doc/repl_refactor_options_sonnet.md` (same treatment for `repl.rs`).
