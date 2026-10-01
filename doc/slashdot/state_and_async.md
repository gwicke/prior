# Slashdot state and async review

## Issues with the current architecture

- Sync impl methods: Cannot directly execute async; AsyncOp indirection
  creates complexity.
- Complex state access: MenuShared with Rc's, lots of clones, sync complexity.
  Also source of bugs, e.g. model changes.
- (Root cause of the second bullet: the host snapshots state before eval and
  handlers reach into cloned `Rc<RefCell<..>>` cells — copies instead of
  live access, so writes land where the next sync may or may not look.
  `LiveState` mirrors `AgentConfig` and needs bidirectional sync +
  `model_commit` flags to stay coherent.)

## Idea

- Make `eval_prompt*` and the paths down through the tree fully async.
  SlashDotNode handlers that might perform IO or access / update async
  settings all become async too: data(), prepare(), submit(), header(),
  complete_parameter().
- Pass all state down the chain: Make the menu polymorphic / generic over a
  context type, and pass that context type by `&mut` down eval_menu*. No more
  MenuShared. All trait handlers receive the `&mut app_ctx`, and can access
  app state that way. Instances are free to also have additional state in
  their structs - that is up to the implementation. But the app_ctx mechanism
  exists so direct by-ref access is easy.

> **Resolution (see Decisions):** the async half is adopted as stated. The
> `&mut app_ctx` *threading* is superseded — state reaches nodes as an
> injected source-of-truth handle (A″, below) rather than a per-call
> parameter. No loose `MenuShared`-style cells remain; the handle replaces
> the parameter.

## Decisions

- **Context shape (decided): A″ — one coarse shared state.** A single
  `AppState` behind one `Arc<Mutex<..>>` (`parking_lot`), injected into
  node instances as the `MenuShared` replacement. The engine gains **no
  context type parameter** — its only change is going async. Rationale,
  rules, and rejected alternatives: "Context shape" below.
- **Run-to-completion.** Keys queue while an eval is in flight; no
  cancellation (handlers mutate state behind the lock).
- **Uniform async** for state-touching methods, *including* `data()` /
  `header()` — both do real disk IO (session loads), today just blocking
  the input thread. Perf lever = caching (memo across frames, generation
  counter invalidation), not boxing.
- **`#[async_trait(?Send)]`**; tree stays `Box<dyn SlashDotNode>`.
- **Control = declarative target config, not a queue.** A
  `ControlConfig` group on `AppState` ("config", not "pending"):
  handlers write target fields; each field has exactly one consumer that
  converges at its application point. No clear-on-execution — GC a field
  only when the target is achieved (or abandoned after a reported
  error), relying on idempotence (re-applying is a no-op). Latching
  fields (`quit`) are never cleared. Eval is the only writer, so each
 field is last-wins; missing an application point only delays
  convergence, never loses the request. Config convergence attempts one
  rebuild per distinct target (fingerprint of model/thinking/persona;
  an explicit `/model` re-commit re-arms) — a failed attempt errors once,
  not every cycle (`create_agent` errors are final for a given target).
- **Membership rule:** direct call if the handler can finish the
  operation itself (rename/delete/fork/replay/tree-switch); a target
  field only when application needs loop/agent-owned objects (control
  flow, `agent.initialize_base`, LLM calls). `AsyncOp` and its central
  match are deleted outright.
- **Reads are always direct calls.** Reading current state has no timing
  constraint — there is nothing to converge. Three-way rule for every
  handler operation: read achieved state → direct accessor (one
  acquisition for a consistent multi-field view); write
  handler-completable state → direct accessor (RMW in one acquisition);
  write state applied elsewhere → target field. Read *achieved* fields
  only — never another consumer's target fields (may not be applied
  yet); cached reads need invalidation (generation counter).
- **Control fields are runtime target state.** `AgentConfig` writes are
  directives ("what should be in effect"); the record of what *applied*
  lives in the session journal and loop-locals — the target is consumed
  at defined application points (model at turn build, sandbox per tool
  exec, `pending_prefill` at `read_input`). The loop applying into its
  own locals keeps half-applied states invisible (no second reader).
  Never persisted (`SettingsStore` is the durable target).
- **Single copy:** `LiveState` folds into `AgentConfig` (inside
  `AppState`); no mirrors, no sync, no `model_commit`.
- **Lock discipline:** one coarse lock, encapsulated accessors — see
  "Context shape" for the six rules (this is the price of A″).

## Key signatures

### Engine (`crates/slashdot_menu`) — async, no context parameter

The crate stays app-agnostic and gains **no** type parameter: nodes carry
their own `Arc<AppState>` field (where they used to carry
`Rc<MenuShared>`); the engine never sees it.

```rust
// node_impl.rs
#[async_trait(?Send)]   // eval path holds Rc tree caches: not Send
pub trait SlashDotNode {
    // Pure metadata — stay sync:
    fn fields(&self) -> Vec<FieldSpec> { Vec::new() }
    fn columns(&self) -> Vec<ColumnDef> { Vec::new() }

    // State-touching — async; state comes from the node's own
    // `Arc<AppState>` field, not from parameters:
    async fn data(&mut self, epoch: u64, pa: &PathArgs) -> DataResult;
    async fn prepare(&mut self, epoch: u64, pa: &PathArgs) -> SubmitResult {
        self.submit(epoch, pa).await        // default still delegates
    }
    async fn submit(&mut self, epoch: u64, pa: &PathArgs) -> SubmitResult;
    async fn header(&mut self, pa: &PathArgs, rc: &RenderContext) -> Vec<String>;
    async fn complete_parameter(&mut self, pa: &PathArgs, cursor_pos: usize)
        -> Result<Rc<CompletionRows>, DataError>;
    async fn drill_rewrite(&mut self, pa: &PathArgs) -> Option<String> { None }
}
```

```rust
// menu.rs — signatures unchanged except async; no `C` anywhere
pub struct SlashDotMenu { root: NodeEntry, epoch: u64 }
pub struct SlashDotHandle<'a> { root: &'a mut NodeEntry, epoch: u64 }

impl SlashDotHandle<'_> {
    pub async fn eval_prompt(&mut self, prompt: &str, key: HostKey,
                             term_width: usize) -> MenuResult;
    pub async fn eval_prompt_with_cursor(...) -> MenuResult;
    pub async fn eval_prompt_resolved(...) -> MenuResult;  // rewrite loop = `.await`s
}
```

Threading: every engine fn on the eval path becomes async —
`node_traverse_at`, `refresh_data`, `node_data`, `handle_leaf`,
`handle_listing`, `completion_pass`, `view_plan` — with no other
signature change. Pure fns (`parse_section`, `ensure_rows`,
filter/rank/sort) stay sync. `NodeEntry`, `MenuResult`, `RenderPlan`,
`HostKey`, `SubmitResult` are unchanged data.

### Host (`src/`) — `AppState` (handle) / `State` (payload)

The single injected handle — `MenuShared`'s replacement:

```rust
pub struct AppState {
    state: parking_lot::Mutex<State>,       // one coarse lock, no poisoning
}

/// Coarse-grained app state: a few reasonably-sized pieces, one source
/// of truth, one lock. Accessor rules: see "Context shape".
pub struct State {
    /// Runtime target config: model, persona, thinking, sandbox —
    /// `LiveState` folded in here. (Durable target stays `SettingsStore`.)
    pub config: AgentConfig,
    /// Durable-target store. Async load/save via `AppState` methods:
    /// clone out → drop guard → await → relock to write.
    pub settings: SettingsStore,
    /// Menu-facing session view. The active `Session` object itself is
    /// the loop's applied layer (loop-local): activation builds it
    /// across awaits, then writes `current_id` — the menu never sees a
    /// half-applied state. `repo` is a clone-shared handle (Arc cache).
    pub session: SessionSlot,
    /// Former loose MenuShared cells, each with a single-writer owner:
    pub recent_models: Vec<String>,        // RMW inside one acquisition
    pub raw_tree: Arc<Vec<SessionTreeNode>>,
    pub leaf_id: Option<String>,
    pub session_usage: SessionUsage,
    pub context_tokens: u64,
    /// Declarative control targets: handlers write; each field's sole
    /// consumer converges at its application point; GC when achieved
    /// (idempotent re-apply is a no-op).
    pub control: ControlConfig,
}

pub struct SessionSlot {
    pub repo: Option<JsonlSessionRepo>,
    pub current_id: String,
}
// `ControlConfig` membership = "direct call if the handler can finish
// it, target field otherwise" (see Decisions).

impl AppState {
    // Accessors: lock ONCE, complete the whole read/write inside, and
    // return owned values — never return a guard.
    pub fn model(&self) -> String { /* lock → read → owned */ }
    pub fn set_model(&self, id: &str) { /* one acquisition, whole RMW */ }
    pub fn take_control(&self) -> ControlConfig { /* sole-consumer GC */ }
}

// Nodes hold `Arc<AppState>` (background tasks clone the Arc).
// Service handles that never change after startup (output, cancel
// flags) stay outside the lock. `State` members must be `Send` for the
// Arc to cover background tasks — Send check passed (phase 1).
```

What the shared async state kills directly:

- **Data-plane AsyncOp variants** (rename/delete/fork, settings save,
  replay load): `submit().await` executes them inline; the loop never
  sees them.
- **`LiveState` mirror + `model_commit`**: `/model` submit writes
  `config` directly *and* records the session `ModelChange` inline
  (repo is async now). No loop-side sync, no flag, no spurious entries.
- **Snapshot building** in the `v25_eval` / `menu_submitted` bridge.

What survives (control-plane) — target fields, one consumer each:

| Target field | Written by | Consumed at |
|---|---|---|
| `config.model/sandbox/…` | menu handlers | agent: turn build / tool exec |
| `control.quit` | `/quit` | loop: cycle check → exit |
| `control.new_session` | `/new` | loop: create + swap slot |
| `control.resume` | `/session … resume` | loop: load + swap slot + prefill |
| `control.delete_session_ids_after` | `/clear` | loop: after activation (idempotent) |
| `control.compact` | `/tree … compact` | agent (Prompt mode needs the LLM) |

`/clear`'s pair composes transaction-like intent declaratively — "after
the new session is live, these should be gone"; the applier defines the
order, idempotence makes replays safe. `rename/delete/fork/replay/
tree-switch` need nothing the handler doesn't have → direct calls.

## Async dyn traits: trade-offs

The tree must stay `Box<dyn …>`: node impls are host-defined and the set is
open, built dynamically at startup. Native `async fn` in a trait is **not
dyn-compatible on stable Rust**, so the options are:

1. **`async-trait` crate (recommended).** Rewrites each method to
   `fn(...) -> Pin<Box<dyn Future<Output=T> + 'async_trait>>` with a
   `Box::pin(async move { ... })` body.
   - Cost model: **one heap allocation per method invocation** — the entire
     future/state machine lives in that box, so there are *no* further
     allocations per `.await`; plus one vtable indirection per poll (already
     the norm in async code). The allocation is a plain malloc/free on the
     call's fast path, freed when the call completes — not a per-await or
     growing/streaming allocation.
   - At menu rates (per keystroke/frame, ~100 rows → hundreds of trait calls
     per frame) this is noise next to rendering and IO. `data()`/`header()`
     are the hot methods; even they are fine unless a handler does real IO
     per call (mitigated by the caching discipline in Decisions).
   - Must be written `#[async_trait(?Send)]`: the default requires `Send`
     futures, and the eval path holds `Rc` tree caches (`Rc<DataResponse>`,
     view memos). Consequence: eval futures never move across threads
     mid-poll — fine for a single-task REPL (the *state* itself can still
     be shared via the `Arc`).
   - Supports default methods (`prepare` delegating to `submit` relies on
     this) and generic traits unchanged.
   - Costs: one proc-macro dependency; error messages point into desugared
     code (manageable, document it); verify MSRV.
2. **Hand-rolled `BoxFuture`, no dependency.** Same generated shape,
   desugared manually (`type BoxFuture<'a, T> = Pin<Box<dyn Future<Output=T> + 'a>>`).
   Identical one-alloc cost, zero dep, but every signature spells out
   lifetimes/`'async_trait`-style bounds by hand across ~7 methods × N nodes
   — easy to get subtly wrong. async-trait is exactly this with less noise;
   only worth it if the crate must stay dependency-free.
3. **Native `async fn` (RPITIT).** Zero boxing — but not object-safe on
   stable, so the engine loses `Box<dyn SlashDotNode>`: requires enum
   dispatch or generics over concrete node shapes. Rejected: the node set is
   open and host-defined; either leaks all node types into the crate or
   turns `NodeEntry` into a type-level combinatorial.
4. **Enum dispatch.** Same as (3) with a closed node set: native async, no
   alloc, exhaustive matches. Rejected for the same reason — the crate is
   host-agnostic.

Conclusion: pay the one-box-per-call; it is a fixed, bounded cost with no
per-await allocation, and it is the price of keeping `dyn` + an open node
set. Revisit only if a profile ever says otherwise (it will not).

## Context shape (decided): A″ — one coarse shared state

**Decision:** a single coarse-grained `AppState` — the injected handle,
`pub struct AppState { state: parking_lot::Mutex<State> }` (no poisoning)
— replacing `MenuShared`. Nodes hold `Arc<AppState>`; background tasks
clone the `Arc`; the locked payload is `State` (target config, settings,
slot, display snapshots, `ControlConfig`). The engine gets **no context
type parameter**; its only change is going async.

**Why it wins:** every historical bug was a *mirror* (`LiveState`), a
*snapshot* (`ReplCtx`, pre-eval builds), a *data channel* (`pending_op`),
or loose unowned cells (10 pub `Rc` fields on `MenuShared`) — all fixed
by rules that do not depend on how nodes reach state: single source of
truth, live handles, encapsulated methods, no pub cells, intent-only
fields. Meanwhile `&mut C` threading buys little: anything
background-touched must sit outside the exclusive borrow anyway, and
`Session`/`JsonlSessionRepo` are already `&self`/`Clone` shared by
nature. Bonus: background tasks just lock — no per-field Send
classification, no watch/atomics needed (the coarse lock covers
loop→menu scalars too). At keystroke-rate contention, the lock is free;
there is only ever one lock, so no lock-ordering problem.

### Rules (the price — review-enforced, not compiler-enforced)

1. **Encapsulated handle only:** `AppState` exposes methods; no pub
   cells — otherwise this is MenuShared again.
2. **Lock once per public method** — private helpers take `&mut State`.
   The Mutex is *not* re-entrant: an inner re-lock deadlocks
   (unspecified behavior in std; RefCell's equivalent at least panicked).
3. **Never hold a guard across `.await`** — clone handle/value out →
   drop guard → await → relock to write. A guard across await is the
   sharper failure mode than RefCell's borrow panic: silent deadlock
   instead of a loud one.
4. **Whole read-modify-write in one acquisition** — a lock fixes data
   races, not lost updates split across critical sections.
5. **Single source of truth:** no mirrors, no snapshots; `control`
   target fields are written by eval, read + GC'd only by their sole
   consumer. The active `Session` is the loop's applied layer: activation
   builds it across awaits, then writes `SessionSlot::current_id` in one
   acquisition — the menu sees only the slot, never a half-applied state.
6. **Only prepare/submit mutate** — `data()`/`header()` are render-path
   reads. Convention only; the rejected threaded design could have
   encoded this as `&C` vs `&mut C` — accepted loss.

**Send check:** the `Arc` only "covers all bases" if `AppState: Send`
(`Session`, repo, config services). If a member is `!Send`, either
convert/wrap it or accept that tasks stay single-thread
(`spawn_local`) — decide in plan phase 1.

### Rejected alternatives (record)

- **A: threaded `&mut C`.** Buys exclusivity over an all-handle struct
  shell (the interesting data must be shared anyway), infects every
  engine signature with a `C` parameter, and forces the per-field Send
  classification the coarse lock makes unnecessary.
- **B: `C = AppCtx<'a>`.** `&mut C` args make `dyn SlashDotNode<C>`
  invariant in `C`: the stored tree pins one `'a` for its whole life —
  tree and state can't be siblings; lifetime params infect
  signatures/tests; the coarse borrow compile-locks background writers
  out, forcing the same state split A″ requires by rule, plus
  contagion, plus watch/atomics anyway.
- **Per-field watch/atomics classification.** Unnecessary: the coarse
  lock covers loop→menu scalars. The *single-writer* rules survive, but
  as logical-coherence discipline, not race-freedom plumbing.

## Implementation plan

The main work is **source-of-truth migration** (phases 0–2, doable while
still sync); the async conversion rides on top.

0. **Audit** **[done].** Inventory every state location — MenuShared's
   cells, `LiveState`, `ReplCtx` snapshots, `pending_op`, `model_commit`,
   `recent_models` sync, loop-locals the menu reads — and assign each a
   home in `AppState` or a deletion.
1. **`AppState` module** (still sync) **[done].** Struct, accessor
   methods, lock rules in the module doc; `ControlConfig` register types
   moved in from `AsyncOp`. **Send check passed** (test asserts
   `Send + Sync`).
2. **Migrate to source of truth** (still sync) **[done].** Swap
   `Rc<MenuShared>` → `Arc<AppState>` in all `v25_*` nodes; delete
   `LiveState` + its loop sync (handlers write `config` directly);
   `pending_op` → `ControlConfig` target fields; menu-side ReplCtx
   snapshots → `update_cycle_view` writes. Also landed: `repl`/`agent`
   wiring (convergence step + per-field control appliers),
   `slash_menu_bridge.rs` deleted. Tests green (639 + 2 ignored).
3. **Async conversion** **[done].** `#[async_trait(?Send)]` on
   `SlashDotNode` + all 38 impls (no signature change beyond `async`);
   the whole engine eval path is async (`node_data`, `node_traverse*`,
   `handle_leaf/listing`, `refresh_data`, `view_plan`, `completion_pass`,
   `eval_prompt*`) with its two recursive cycles boxed (`Box::pin`, one
   manual future each — E0733); `Repl::read_input` → `v25_eval` are
   async (the only production chain). Sync tests stay sync: call sites
   in `#[cfg(test)]` regions are wrapped in `futures::executor::block_on`
   (scripted, paren-balanced, one descending insertion pass) — 242
   wraps; test bodies and helpers unchanged.
4. **Slot exposure** **[done].** `SessionSlot` gains the two shared handles —
  `session: Option<Arc<Session>>` (all methods `&self`; internals are
  `Arc<AsyncMutex>` by design) and `agent: Option<Arc<CodeAgent>>`
  (`Send + Sync`, all methods `&self`) — plus `activate_session(id)` /
   `activate_agent(a)` one-acquisition swaps (stronger bundle guarantee:
   one atomic write). Loop call-sites become *lock → clone `Arc` out →
   drop guard → await*; creation/rebuild stays loop-side, from the
   **applied** config (handlers only ever consume the `Arc`s).
5. **Data-plane inline — appliers collapse** **[done].** Inline
   in `submit().await` (slot session/agent/output/repo reachable):
   tree_switch, replay, rename, delete, new, clear (full transaction —
   `agent::start_fresh_session` + outgoing delete, preserving the
   applier's create→activate→delete order), fork
   (`agent::fork_session_at`, shared with the typed path — also fixed
   a latent bug: `repo.fork` never created the target cwd's dir, so the
   first fork into a fresh directory failed with ENOENT), and `/model`'s
   inline `ModelChange` journal (live-node role; `model_commit`
   deleted; the rebuild-gate re-arm is fingerprint-change-only now).
   `ControlConfig` = **quit, resume, tree_compact, sandbox** (from 12).
   Tests assert side effects (leaf moved, rendered output captured via
   `output::RecordingOutput`, index session_name, files gone/present,
   journal entry), driven through `agent::block_on_test` (submits hit
   `tokio::fs`) with the `test_session_with_entries` /
   `test_repo_with_session` fixtures.
   Also done: **resume** (`agent::resume_session` extracted from
   `handle_session_command`, shared with the typed path — foreign-cwd
   fork migration included; the model decision rides the new **prefill
   register** in `AppState`, sole consumer the loop before
   `read_input`, which replaced the `pending_prefill` local entirely)
   and **compact** (`agent::compact_inline`: entry/tokens/early-out
   guards, Llm agent from the TARGET config, review closure from the
   slot's published `TermGuard`). Mid-eval `$EDITOR` is safe via
   **`emit_commit`'s live-DSR re-anchor** of `input_row` before redraw
   (the Ctrl-E post-`guard.start()` pattern, applied unconditionally).
   Async-IO follow-ups **[done]**: `JsonlSessionRepo::
  list_sessions_overlay` — cache-first (the same 2s mtime debounce;
  warm = zero IO), `tokio::fs` cold scan, cache lock never held across
  an await. Production callers migrated (session listing, `find_session`,
  `/stat` header, the inline rename/delete/fork/resume meta lookups);
  `list_sessions_sync` is now `#[cfg(test)]` (same cache, same
  results). Settings save: async facade **dropped as low value** — a
  tiny user-paced write the frame waits on regardless; the real defect
  was holding the coarse lock across the TOML write, fixed as
  apply-under-lock → clone → write-outside (`SettingsStore::
  apply_value`), with the rule recorded in the module doc.
   Judgment recorded and applied: session creation, the model journal,
   AND compact's Llm-agent creation stamp from the **target** config
   handler-side (divergence only in the failed-rebuild state; a later
   `ModelChange` corrects). Final `ControlConfig`: **quit, sandbox**
   (was 12 fields) — the convergence appliers are just the sandbox
   merge/re-publish.
6. **Verify.** Full suite green **[done]**; stale-name grep
   `MenuShared|LiveState|pending_op|model_commit|AsyncOp` → zero
   **[done]**. E2E pty tests **[done]** — `tests/e2e_session_state.rs`
   (isolated XDG + temp cwd + seeded sessions, real binary, no network):
   `menu_traffic_never_creates_empty_session_files` = THE empty-session
   repro (menu delete + thinking switch → convergence → journal record
   against an unpersisted session must not mint files) plus
   `/session`-listing/delete/quit smokes; `resume_replays_history_and_
   seeds_model_prefill` = resume replay + `/model` prefill smoke. Both
   green. `/model`-overlay switch itself stays unit-covered (the static
   catalog has no llamacpp rows without network — E2E uses `/thinking`
   for the same convergence/record surface). Harness notes: reply to
   DSR scans before every keystroke (`cursor_row`'s 60ms window eats
   bytes) and clear the input line before typing over a prefill.

## Open questions / potential issues

1. **Abandon policy for unachievable targets.** Pure convergence retries
   every cycle — a resume to a deleted id, a delete hitting a permanent
   error. Report + GC (user re-decides) or retry with backoff? Per-field
   policy.
2. **Testing cost.** ~100 sync tests become async: tiny executor +
   mechanical `.await`; watch tests that build menus with no runtime.
3. **Error surface.** Async IO errors funnel into `DataError{message}` /
   `SubmitResult::Error(String)`; revisit if handlers need structured
   errors (not-found vs permission).
