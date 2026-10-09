# node

Structure

- EITHER: childNodes
  - need to return rows that are uniform in fields - single handler or uniform
    handlers minimal common fields: id, description
- OR: leaf handler
- optional
  - description
  - field metadata etc
- own and validate their ids

## Parent / child interface

- traverse(keys, parsed_args, path_prefix, id, path_remainder):
 `[Continue(Option<RenderPlan>, Option<newprompt>) | DoneSuccess | DoneError]`
  - called by parent
  - validates own ids, extracts / consume their own parameters based on
    generic code + spec - positional, dot modifiers - generic traid default
    code
  - path fully consumed?
    - yes:
      - if children: show listing; return newpath (normally unchanged full
        path)
        - if keys
          - arrow up/down; page up / down: navigate listing, return new render
            plan
          - tab / enter: call child with best matching ID
      - no children:
        - no special keys (tab / enter, arrows): maybe show help, return full
          path
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

## Review (picocode, Phase 7 in flight)

Read against the current engine: `MenuNode` (data descriptor, 117 lines) +
centralized `Interpreter` (~1500 lines: state derivation, Tab/Enter/Esc,
sticky cursors, modifier query, active-row resolution) + side-table
provider/handler registration + `AcceptController`.

### What the proposal actually changes

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

### Why A is the real win (and directly serves the remaining plan)

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

### Questions on B (distributed traversal)

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
   - I was leaning towards having each node's default implementation own the
     local traversal and cursor state, and clear on epoch (menu invocation
     counter) increment (needs to be passed down the traverse chain, omitted
     in the example). This avoids the need for a magic ctx datatype.
     All this caching would be in the single, private node implementation - so
     easy to test. Privacy / non-overridability of e.g. the traverse() method
     can be ensured e.g. using the extension trait / blanket impl pattern.
     Multi-stage processes like the compaction node are fairly easy to support
     in this setting, with a localized implementation and principled cache
     management.
   - **Epoch-invalidated per-node state: agree this is tractable**, and it
     mirrors the persistent engine's reopen semantics from 6.7-3 (new
     engine per menu open ⇒ fresh caches), pushed down per node. Two notes:
     (1) the Esc ladder is not "ask the current node": frame replay means
     re-entering the root traverse with a *shorter* path (the pre-pin
     buffer). Fine with recursion, but worth writing down: Esc always
     re-enters at the root, it never recurses up from the current node.
     - Yes, Esc traversing the node bundary back to the parent would involve
       several steps. Lets say we start at `/parent child` (child listing),
       with earlier filter modifiers at `/parent <filters>`:
       1) first Esc: child strips trailing slash, returns newpath
       2) (assuming no fancy optimizations) traversal restarts at the root,
       parent menu now active with `child` selected (`/parent child`). Child
       node is no longer traversed.
       3) second Esc: parent pops its own frame cache, returns newpath
       `/parent <filters`; new root traversal, old filter state resumed
       4) third Esc: parent clears filters, returns just `/parent`
       5) continue at 1)
       So pretty much all path shortening, with the occasional internal frame
       pop.
     (2) Multi-stage nodes like compaction (stage prompt → Esc → re-stage →
     accept) survive *within* one epoch, so their cache key needs
     (epoch, stage-line), not epoch alone — per-node policy, same as the
     engine's sticky map keying on terms today. Manageable, but a policy
     to specify rather than one uniform rule.
     - Yes, they need to key on the compaction point / ID as well. They could
       even decide to key on *only* that, since sessions are append-only trees.
2. **State derivation vs. key stepping.** Today `state(line)` is called per
   frame (host re-derives everything from the buffer; the engine is
   stateless w.r.t. frames) and `step_at(line, key)` is the mutation point.
   The `traverse(keys, args, prefix, id, remainder)` signature fuses them:
   nodes must then be *stateful across frames* (per-node cursor?) or the
   caller keeps re-deriving and passes it in. Which is intended? (The
   persistent engine from 6.7-3 makes stateful long-lived nodes viable —
   but then per-node state needs reset-on-reopen semantics, which the
   engine currently owns centrally.)
   - Yes, per-node cursor. Keyed on the filter state, remember the filtered
     list + active ID. Reset all cache state on epoch increment.
   - Acknowledged — per-node cursor keyed on filter state mirrors the
     engine's `CursorMap` semantics ((index, pin, terms) with reset-on-
     change). One request: keep the (index, pin, terms) reset rule as one
     tiny shared `CursorCache` struct used by the default body, so
     open-seeding (tree on the current leaf, model on the active row)
     behaves identically at every level rather than per-node policy.
     list + active ID. Reset all cache state on epoch increment.
3. **Selection validity (D2).** The centralized `state()` demotes a trailing
   token that matches no candidate id into a fuzzy term, checked against the
   *cached row set*. In the default traverse this is replicable, but a
   per-node override (the sandbox fs-complete node, say) can silently skip
   it — the failure mode is a pin on a nonexistent id instead of a filter
   term. Centralized keeps the invariant in one audited place.
   - Wouldn't this all be handled generically in the private node code? Both
     are uniform / private implementations, just split up slightly
     differently.
   - Yes — with traverse() private and the D2 check in the default body,
     this objection dissolves. The residual is only: a node whose data()
     returns ids that changed between fetches can still create stale-pin
     drift — but that is equally possible today (provider returning new
     rows). Retracted; it is a "keep it in the default body" note, not a
     structural objection.
     are uniform / private implementations, just split up slightly
     differently.
4. **Borrow/mutability ergonomics.** `traverse(&mut self)` recursively over
   a tree of `Box<dyn Node>` means `&mut` dispatch into a subtree per level
   while the parent holds siblings — index-based descent or arena. The
   current interpreter sidesteps this by cloning the (cheap, `Clone`-derived)
   descriptor and holding providers in a flat map. Not a blocker, but the
   trait tree makes the engine's data structures noticeably hairier.
   - > `&mut` dispatch into a subtree per level while the parent holds siblings
     Where does the "while the parent holds siblings" come from? There is only
     one traverse of the tree active at a time, correct?
   - **You are right — only one traversal is active at a time.** The
     concrete friction is subtler, and I verified it by compiling the
     shapes: a *naive* default body (`for c in self.children() { ... }`)
     compiles fine. It breaks only when the default body wants to
     interleave access to the **parent's own state** with the child
     recursion — e.g. read the parent cursor *inside* the loop to seed a
     child's cursor, or record a frame for the parent level while
     descending. Because `children()` returns `&mut [Box<dyn Node>]` whose
     lifetime is tied to the whole `&mut self` borrow, `self` is locked
     for the entire loop (E0499). The discipline that compiles: **parent-
     state access only outside the child loop, or via a second pass** —
     a three-phase body (own-state decisions → descend → own-state
     post-processing). Fine for traversal and arguably good structure; the
     one thing it forbids is a custom node's data() reaching into parent
     state *during* the parent's loop. Not a blocker — a shape rule worth
     one sentence in the design doc.
5. **What is left of the interpreter, concretely?** Grammar parse
   (`parse_level`), rank-and-pin, query compilation (§7.1), render plan
   build, cursor map, frames, accept phase. If the default `traverse` body
   contains all of that, the engine's *concept count* did not shrink — its
   code relocated into a trait. The audit criterion ("host has no
   per-command logic") is unaffected either way. So B's value proposition
   needs to be stated as something other than "less machinery": is it
   extensibility (new commands = implement one trait), testability
   (per-node unit tests), or eliminating the descriptor/handler split?
   - Agreed, the basic logic is still there - just structured differently,
     mostly behind the scenes. I think the main strength of the trait
     implementation side of the idea is more on the "node as one
     understandable implementation unit" side. I think this will make the code
     easier to follow and extend. And the trait approach can start out with a
     very small set of extension points (data and submit being the main ones),
     but it is not hard to later expose targeted additional extension points
     where necessary, while keeping it all in a small and local interface.
6. **RenderPlan in the return type.** `Continue(Option<RenderPlan>,
   Option<newprompt>)` fuses derivation and rendering into each call. The
   host still needs `MenuState` details the plan does not carry (cursor row
   for Tab/Enter focus math, `pin_lost`, level identity for cursor keys) —
   either the plan grows fields (it becomes MenuState) or there is a second
   call. Clarify.
   - What is the tab/enter focus math about? If that is the initial focused
     row on first enter of a menu, then I thought that would already be
     indicated in the rows sent in the render plan?
     My conceptual thinking of RenderPlan is more like a text version of
     server-rendered HTML: Send the content, indicate which rows should be
     marked / focused, send all mutations (key presses) to the server, which
     returns a new render. The level identity part would dissolve into hittng
     the same node. "pin lost" is also something the node itself knows best
     about, right? Tab/enter would be node-level logic and state (for esc). So
     the RenderPlan would be not much more than content - maybe making the
     host layer even thinner?
7. **Migration timing.** Steps 1–7 of the revised 7.2 (uniform key loop,
   CommandList/Model/Thinking/Persona/Mod/Session/Tree/Stat) are
   model-agnostic — they collapse host overlays onto the engine as it
   exists. Steps 8–9 (Settings, Sandbox) *are* the mixed-hierarchy cases.
   So there is a clean seam: adopt A when reaching step 8, or adopt it
   first. Adopting B (distributed traversal) mid-flight invalidates the
   interpreter's test suite (~40 table tests) — that cost needs to be
   weighed against finishing 7.2 first.
   - How much of the test suite is prompt / key level (reusable?), and how
     much is low level unit tests?

### Pros / Cons summary

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
  - Do you see points that would really benefit from a cental coordinator
    here? To me, it feels like a structural tree that follows the traversal
    makes many things easier - there are not several specialized and
    partly-aligned tree structures mirroring the node structure.
- Per-node overrides are unbounded escape hatches: each one can diverge
  from a centralized invariant silently (validity demotion being the
  clearest). Today's model confines per-command code to data + handlers.
  - This depends on what is opened up, right? As mentioned before, the main
    entry points like traverse() would be private, for sure. I would start
    with a minimal set of customization points.
- Recursive `&mut` dispatch over a node tree complicates the engine's data
  structures (arena/index descent) for no user-visible gain.
  - Can you elaborate on this?
- Fusing state derivation + key stepping + rendering into one signature
  either grows RenderPlan into MenuState or forces a second call.
- Full-trait migration invalidates the engine's table-driven test suite
  mid-Phase-7; A alone preserves it (row-source plumbing changes, behavior
  contract does not).

### Synthesis (recommended)

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

---

## Round 2 responses (picocode)

### On the `&mut` traversal question (§4) — the direct answer

You asked where "the parent holds siblings" comes from. **You are right:
only one traversal is active at a time.** My original phrasing was wrong
about the mechanism. I verified the actual friction by compiling the
shapes; here is what really happens:

1. A naive recursive default body compiles cleanly:

   ```rust
   fn traverse(&mut self, epoch: u64) {
       for c in self.children_mut() { c.traverse(epoch); }
   }
   ```

   No arena needed. That part of my earlier claim was overstated.

2. The genuine borrow constraint appears only when the default body wants
   to **interleave access to the parent's own state with the child
   recursion** — e.g. reading the parent's cursor *inside* the loop to
   seed each child's cursor:

   ```rust
   for c in self.children_mut() {          // &mut self borrow #1
       let seed = *self.cursor();          // &mut self borrow #2 → E0499
       *c.cursor() = seed;
       c.traverse(epoch);
   }
   ```

   `children_mut(&mut self) -> &mut [Box<dyn Node>]` ties the returned
   slice's lifetime to the whole `&mut self` borrow, so `self` is locked
   for the entire loop. For a *concrete* struct the loop can be rewritten
   with disjoint field borrows (`let (kids, cursor) =
   (&mut self.kids, &mut self.cursor);` — the compiler sees the fields are
   disjoint and allows both), but **not from a default trait body**, which
   sees an opaque `&mut Self` and can only reach state through accessor
   methods — the compiler cannot prove those accessors touch different
   fields.

3. The discipline that compiles — and that the default body must follow:

   ```rust
   // phase 1: own-state decisions
   let seed = *self.cursor();
   // phase 2: descend (children borrow active)
   for c in self.children_mut() { c.traverse(epoch); }
   // phase 3: own-state post-processing
   let _ = self.cursor();
   ```

   i.e. **parent-state access only outside the child loop, or in a second
   pass**. For traversal this is fine (arguably better structure); the one
   thing it forbids is a child's `data()` reaching into parent state
   *during* the parent's loop — cross-level data flow must be passed down
   as arguments (the `seed`), not read sideways. That is a shape rule, not
   a structural problem, and it is worth one sentence in the design doc.

So: retracted as a structural claim ("needs arena/index descent"); the
honest cost is the three-phase discipline above plus moving tree ownership
into the nodes (`Vec<Box<dyn Node>>` per parent) instead of today's flat
`Clone` descriptor. The reopen-reset then becomes an epoch walk (or
reconstruction) rather than a cheap clone — both fine, both policies to
write down.

### On epoch-cached per-node state (§1)

Agreed this is tractable — it mirrors the persistent engine's reopen
semantics (new engine per menu open ⇒ fresh caches), pushed down per node.
Two policy notes worth writing down:

- **Esc is a root re-entry, not an up-recursion.** Frame replay means
  re-deriving the state the *parent* had before the pin, i.e. re-running
  the traverse with a shorter path. With per-node recursion this works,
  but Esc should be specified as "re-enter at the root with the previous
  buffer", never "bubble up from the current node" (the latter would need
  every ancestor to reconstruct its pre-pin state on the way out).
- **Multi-stage nodes need richer cache keys.** Compaction (stage prompt →
  Esc → re-stage → accept) survives *within* one epoch — no reopen between
  stages. Its cache key is (epoch, stage-line), not epoch alone; same as
  the engine's sticky map keying on terms today. Per-node policy, fine,
  but it is a policy rather than one uniform epoch rule.
  - Agreed.

Also a Rust note on making `traverse()` non-overridable via the
extension-trait/blanket-impl pattern: Rust has no `final` methods — a
default method in the *same* trait can still be shadowed by an impl that
defines it. The pattern that actually closes it: mutable cache + traversal
logic live in a private non-generic struct (or in the blanket impl of a
separate extension trait over `NodeImpl`), `data()`/`submit()` are the
only overridable members of the base trait, and the public entry is the
extension trait's `traverse()` implemented once via blanket impl. Your
"single, private node implementation" is then enforceable by construction,
not by convention.

- Yup, blanket impl is what I had in mind.

### On the coordinator question (Cons §1)

Partial retraction. With `traverse()` private and epoch-cached per-node
state, the coordinator shrinks to three small pieces: (a) the root entry
point, (b) the epoch counter + reopen reset, (c) the accept/confirm phase
(prep → pause → accept spans *levels* — a confirm leaf's prep state is
per-leaf, but the pause/cancel/accept control flow is menu-level). I
agree the "several mirrored tree structures" concern was illusory — the
side tables are flat maps keyed by path, not trees. My lean: keep (c)
engine-owned (control flow, not per-node data); (a) and (b) are trivial.

- I think the "accept / cancel" dialog could be rendered by the same node that
  also owns the final submit handler. Actually, she custom submit handler itself
  could keep track of the state machine:
  1) if no cache match: run the prep, show editor (picocode shared code)
  2) call a pre-defined (not overridable) menu method that renders the accept /
     cancel dialog and handles keystrokes / cursoring, and returns yay or nay on
     enter
  3) on yay commit the compaction (or whatever it is)
- Convinced — and this is a genuinely better decomposition than my (c):
  the confirm flow becomes a *two-phase submit* protocol, where the
  not-overridable engine service is the yea/nay dialog (render + keys +
  cursor, zero per-node logic) and the *state machine lives with the
  handler*, which is where the payload and the staging policy already
  live. What the engine must still provide is the *pause* semantics —
  after prep, the menu must stay open with the dialog as the render
  target until yea/nay resolves, and Esc/rewrite must route to the
  handler's cancel path (clearing its cached payload, per your (1)).
  Concretely the `submit()` contract grows one return value
  (`Submit::Prepared` = "pause with dialog, resume me on resolve") and
  one callback entry (`submit_resolved(yea)`); everything else — what the
  payload is, whether the editor is shown first, what cancel means — is
  handler-local. That removes the last menu-level piece of (c): the
  coordinator is now just entry + epoch, and the accept-controller's
  ConfirmPrep state machine folds into handler-local state.

### On test-suite reuse (§7)

Counted: the engine crate has ~99 tests. The prompt/key-level scenario
tests (state/tab/enter/esc/cursor semantics) are ~32 in `interpreter.rs`,
8 in `accept.rs`, plus 18 bridge-level ones — they go through
`state`/`tab_at`/`enter_at`/`esc`, not through data plumbing, so they are
precisely the ones a distributed traversal *relocates* (same scenario
tables, new harness entry point). The other ~40 (grammar/filter/fuzzy/
table/render) are pure unit tests, reused unchanged either way. So the
migration cost is a mechanical port of ~50 harness call sites, not a
redesign of the semantics.

- Okay, that is promising. I guess mainly the lower level interpreter specific
  tests would be lost.
- Right, with one refinement: the "lost" interpreter tests split into two
  kinds. (a) *Derivation tests* — `state(line)` → node_path/terms/
  selection/active — port as traversal tests (same assertions, driven
  through the root traverse with a rendered-plan assertion instead of a
  MenuState). (b) *Pure helpers* — grammar tokenizer, rank_and_pin,
  filter/sort engine — survive unchanged because they are free functions
  the default bodies call. The genuinely deleted ones are only the
  cursor-map internals tests (they become per-node cursor tests instead).
  So the port is ~50 call-site adaptations plus writing new per-node
  cursor tests, while grammar/filter/table/render (~45 tests) are
  untouched.

### Net position after round 3

The stateful-node design is viable and the two open coordination questions
resolved in your favor: the confirm flow is a two-phase submit protocol
(dialog as a not-overridable engine service, state machine handler-local),
and the coordinator shrinks to **root entry + epoch counter** only. The
remaining costs are the three-phase borrow discipline and the ~50-call-site
test port.

Sequencing recommendation (unchanged in substance): land 7.2 steps 1–7 on
the current engine first — they are model-agnostic and none of that work is
throwaway, since the §6/§12 host contract (plan-verbatim rendering, key
routing, leaf-result translation) is identical under both node models. Then
do the node reshape as one step spanning idea A (merged child sets) + the
trait node-impl, landing Settings (step 8) and Sandbox (step 9) on it, with
the ~50 test port inside that step. Rationale for bundling A+B into one
reshape rather than A first: A's merged child sets change
`child_rows`/`is_selection_at` anyway — doing A alone means re-touching the
same interpreter seams twice (once for merged rows, once to move them into
node-owned default bodies). Bundled, each seam is touched once and the
intermediate state (A without B) never has to be shippable on its own.

One design item to pin down before the reshape (needed by both models):
the `/set default_model` reuse of the model rows requires the model child
node to be *shareable* — two parents (`/model`, `/set default_model`)
referencing one node impl with different leaf handlers. In the
`Box<dyn Node>` ownership model that means either an `Rc<RefCell<…>>`
shared node (single-threaded app, matches MenuShared's handles) or a
flavor/copy mechanism (instantiate the model node template with
parent-specific submit). Decide `Rc`-share vs. template-instantiate when
specing the trait — it determines whether per-node cursor state can be
shared or must be keyed by *full path* inside the node (shared node ⇒
path-keyed cursor, template ⇒ plain local cursor).

---

> Esc must be specified as a root re-entry with the shorter buffer

Yup, I sketched this in a bit more detail in a response slightly higher up, see
"Yes, Esc traversing the node bundary back to the parent would involve". Unless
I am missing something, I think no special return protocol is needed beyong
newpath and node-local key handling.

> Borrow/traversal question

For the normal traversal pattern, the need to structure this in phases seems
fine to me. I would think that most local mutation happens when the local node
is at the leaf, and is listing children - so the pattern you modeled. And in
that case, having the well-defined fetch phase seems fine. For pure traversals,
the pattern is pretty linear.

> The submit() contract grows one return value (Prepared = pause with dialog)
and one callback (submit_resolved(yea));

Another way could be to offer an overridable (default-noop) "prepare" method in
the trait. Default impl would be just `return submit()`, but an override can add
its prep phase. Would need a return protocol, too, as you said. That is slightly
less flexible than a directly callable "confirm" method, but maybe easier to
understand - more of the scaffolding can be generic / hidden.

> One design item flagged for the reshape spec: node sharing. /set default_model
> reusing the model

I think it would be fine to use two instances. The default model is rarely
touched, and the model repo can cache independently, so any wins from having
the same instance are pretty minimal.
