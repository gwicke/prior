# node
Structure
  - EITHER: childNodes
    - need to return rows that are uniform in fields - single handler or uniform handlers
      minimal common fields: id, description
  - OR: leaf handler
  - optional
    - description
    - field metadata etc

- own and validate their ids

## Parent / child interface
- traverse(keys, parsed_args, path_prefix, id, path_remainder): 
 [Continue(Option<RenderPlan>, Option<newprompt>) | DoneSuccess | DoneError]
    - called by parent
    - validates own ids, extracts / consume their own parameters based on
      generic code + spec - positional, dot modifiers - generic traid default
      code
    - path fully consumed?
      - yes:
        - if children: show listing; return newpath (normally unchanged full
          path)
          - if keys
            - arrow up/down; page up / down: navigate listing, return new render plan
            - tab / enter: call child with best matching ID
        - no children:
          - no special keys (tab / enter, arrows): maybe show help, return full path
          - on submit: call handler (leaf)
      - no: 
        - if children: call their own children with path remainder
        - no children: error? extra params that were not consumed

## Tree assembly
- addChild() - order determines merge / default sort order of rows / ids
               returned - unless re-sorted in parent

## Internal actions / customization points
- metadata
  - UI config: fields to display (table), title / description / help
- headerText(): default to static description (also shown in parent);
  customizable for /stat or similar
- getData
- onSubmit


## Tree example
/ -> children
  /model -> single dynamic child returning uniform data
    /model <id> -> leaf handler

## Dynamic parameter complete
FS complete for sandbox, with space-in-path support
  `/sandbox add_ro /some/long/path`
  - add_ro: node
    - child fs complete: no children, commit handler
      `traverse(keys, args, prefix, remainder): [rows|newpath|submit|error]`
          if directory, return new set of children - parent displays / caches
          else: submit handler

---

# Review (picocode, Phase 7 in flight)

Read against the current engine: `MenuNode` (data descriptor, 117 lines) +
centralized `Interpreter` (~1500 lines: state derivation, Tab/Enter/Esc,
sticky cursors, modifier query, active-row resolution) + side-table
provider/handler registration + `AcceptController`.

## What the proposal actually changes

Two separable ideas are bundled in the sketch:

**A. Merged child model.** One level has a *set of child nodes*; each child
contributes rows (static keys, enum values, or dynamic data) and carries its
own data source + submit handler; the parent declares the field metadata all
children roughly conform to.

**B. Distributed traversal.** The data structure + centralized interpreter is
replaced by a node trait whose *default implementation* implements the whole
key/traverse/render pipeline; per-node impls only override `getData()` and
`onSubmit()`. Parent calls child's `traverse(...)` recursively with the path
remainder.

These have very different costs. **A is a data-model change that slots into
the existing interpreter. B moves the interpreter's logic into trait default
bodies and distributes control flow across the tree.**

## Why A is the real win (and directly serves the remaining plan)

The merged model solves exactly the friction the remaining 7.2 steps hit:

- `/set` is the motivating case, and the current model genuinely cannot
  express it cleanly: `is_listing()` unions children/value/dynamic, but
  `child_rows()` and `is_selection_at()` assume a level has *one* row source.
  A mixed `/set` level (static enum children + a dynamic default_model
  picker reusing the model rows) would need ad-hoc merge logic in the
  interpreter — precisely the per-command special-casing §12 forbids. In A,
  `/set` = static setting children + one dynamic child reusing the model
  rows with its own leaf handler. The `MODEL_MODIFIERS` retention and the
  `parse_model_filter`/`model_selection_overlay_entries` v1 ladder (kept
  alive only for this path today) both die naturally.
- `/sandbox` "add entry mixed into the delete list" is the same capability:
  static `add` child + dynamic bindings rows in one level, ordered by
  `addChild()`.
- Handler/provider binding *on the node* removes the stringly-keyed side
  tables (`with_provider(&["model"], …)`, `active_paths`, `detail_providers`,
  `register_handlers(&["compact"], …)`). The row-producing node owning its
  submit handler is the honest shape — today the leaf that accepts a model
  row is registered by path, not by the node that produced the row.
- Field metadata on the parent is one declaration instead of
  provider-carried `DataResult::fields` + inference fallback (`field_space`
  currently guesses types from the first present row value).

Crucially, **A does not touch the host side at all** (design §12: host
renders plan verbatim, translates leaf results — unchanged), and it does not
touch the grammar, the fuzzy rank/pin, the sticky cursor map, the §5.1
active-row mechanism, or the Esc ladder. It changes where rows come from,
not how keys behave.

## Questions on B (distributed traversal)

1. **Who owns cross-level state?** The interpreter's subtlest invariants are
   all cross-level: the Esc ladder + frame replay walks strictly *up* the
   path; scoped edit invalidation clears only the edited level's frames; the
   sticky cursor map is keyed by full path and survives buffer re-derives.
   A per-node `traverse` sees only its own level + remainder. Esc from a
   deep level must climb through every ancestor, and each node would need to
   re-derive "what did this level look like before the pin" to replay a
   frame. That is a coordinator, wherever it lives — either the parent chain
   threads return values, or a supervisor above the nodes keeps the cursor
   map and frames anyway. If a supervisor survives, the "no interpreter"
   framing dissolves: the interpreter became the default `traverse` body +
   the coordinator.
2. **State derivation vs. key stepping.** Today `state(line)` is called per
   frame (host re-derives everything from the buffer; the engine is
   stateless w.r.t. frames) and `step_at(line, key)` is the mutation point.
   The `traverse(keys, args, prefix, id, remainder)` signature fuses them:
   nodes must then be *stateful across frames* (per-node cursor?) or the
   caller keeps re-deriving and passes it in. Which is intended? (The
   persistent engine from 6.7-3 makes stateful long-lived nodes viable —
   but then per-node state needs reset-on-reopen semantics, which the
   engine currently owns centrally.)
3. **Selection validity (D2).** The centralized `state()` demotes a trailing
   token that matches no candidate id into a fuzzy term, checked against the
   *cached row set*. In the default traverse this is replicable, but a
   per-node override (the sandbox fs-complete node, say) can silently skip
   it — the failure mode is a pin on a nonexistent id instead of a filter
   term. Centralized keeps the invariant in one audited place.
4. **Borrow/mutability ergonomics.** `traverse(&mut self)` recursively over
   a tree of `Box<dyn Node>` means `&mut` dispatch into a subtree per level
   while the parent holds siblings — index-based descent or arena. The
   current interpreter sidesteps this by cloning the (cheap, `Clone`-derived)
   descriptor and holding providers in a flat map. Not a blocker, but the
   trait tree makes the engine's data structures noticeably hairier.
5. **What is left of the interpreter, concretely?** Grammar parse
   (`parse_level`), rank-and-pin, query compilation (§7.1), render plan
   build, cursor map, frames, accept phase. If the default `traverse` body
   contains all of that, the engine's *concept count* did not shrink — its
   code relocated into a trait. The audit criterion ("host has no
   per-command logic") is unaffected either way. So B's value proposition
   needs to be stated as something other than "less machinery": is it
   extensibility (new commands = implement one trait), testability
   (per-node unit tests), or eliminating the descriptor/handler split?
6. **RenderPlan in the return type.** `Continue(Option<RenderPlan>,
   Option<newprompt>)` fuses derivation and rendering into each call. The
   host still needs `MenuState` details the plan does not carry (cursor row
   for Tab/Enter focus math, `pin_lost`, level identity for cursor keys) —
   either the plan grows fields (it becomes MenuState) or there is a second
   call. Clarify.
7. **Migration timing.** Steps 1–7 of the revised 7.2 (uniform key loop,
   CommandList/Model/Thinking/Persona/Mod/Session/Tree/Stat) are
   model-agnostic — they collapse host overlays onto the engine as it
   exists. Steps 8–9 (Settings, Sandbox) *are* the mixed-hierarchy cases.
   So there is a clean seam: adopt A when reaching step 8, or adopt it
   first. Adopting B (distributed traversal) mid-flight invalidates the
   interpreter's test suite (~40 table tests) — that cost needs to be
   weighed against finishing 7.2 first.

## Pros / Cons summary

**Pros**
- Merged child set + per-child handlers makes `/set`, `/sandbox`, and
  `/set default_model` modelable without per-command interpreter logic —
  the strongest argument, and it directly unblocks 7.2 steps 8–9.
- Reusing the model hierarchy under `default_model` kills the last v1
  filter helpers (`MODEL_MODIFIERS`, `parse_model_filter`,
  `model_selection_overlay_entries`) as a *consequence of modeling*, not
  cleanup.
- Node-owned handlers/providers replace path-keyed string registration;
  the accept-controller's path lookup collapses into node identity.
- Field metadata declared once on the parent; child conformance ("at least
  id + description") is a simple, enforceable contract.
- "EITHER childNodes OR leaf handler" formalizes `is_listing()` cleanly.
- Trait-per-node is idiomatic Rust and makes the customization surface
  explicit (`getData`, `onSubmit`) instead of enum-arm dispatch.

**Cons**
- Distributed `traverse` recursion relocates ~1500 lines of centralized
  logic into default trait bodies without reducing it; the cross-cutting
  invariants (Esc up-walk + frame replay, scoped edit invalidation,
  sticky-cursor-by-path, D2 demotion, §7.1 query compile) still need a
  coordinator, so the "no interpreter" goal is likely illusory.
- Per-node overrides are unbounded escape hatches: each one can diverge
  from a centralized invariant silently (validity demotion being the
  clearest). Today's model confines per-command code to data + handlers.
- Recursive `&mut` dispatch over a node tree complicates the engine's data
  structures (arena/index descent) for no user-visible gain.
- Fusing state derivation + key stepping + rendering into one signature
  either grows RenderPlan into MenuState or forces a second call.
- Full-trait migration invalidates the engine's table-driven test suite
  mid-Phase-7; A alone preserves it (row-source plumbing changes, behavior
  contract does not).

## Synthesis (recommended)

Adopt **A** as node-model v2; skip **B** as stated, or fold it into A:

- Keep the centralized interpreter as the single owner of grammar, cursor,
  frames, Esc ladder, query compile, and render-plan emission — the
  invariants stay auditable in one place.
- Reshape the node to the trait: `trait MenuNodeImpl { data(…), submit(…),
  metadata }` with default impls for static/enum children — i.e. the *trait
  is the node*, not a distributed interpreter. The interpreter consults
  nodes' `data()` to build level rows (merged child sets, ordered by
  declaration) and nodes' `submit()` on leaf accept. Per-node overrides
  then only touch data and submit, exactly the sketch's customization
  points, while traversal stays central.
- Sequence: land 7.2 steps 1–7 on the current model (they are
  model-agnostic), introduce merged child sets for steps 8–9 (Settings,
  Sandbox), then revisit whether any remaining per-node `traverse` override
  is worth distributing control flow for. This keeps the direct path: no
  step's work is throwaway under A, and B is deferred until there is a
  concrete command that needs it.


