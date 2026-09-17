# Step 4 design: `NodeEntry::node_traverse()` internals

Design: `slash_menu_v2.5_design.md`. This document specifies the internal
data structures and algorithm for step 4 of the implementation plan.

**Revision note (step4 v2).** This revision fixes correctness bugs found in
the first draft and tightens the caching story. The substantive changes:

1. **Recursion contract clarified.** A child receives the *tail* — the
   unconsumed part of the prompt after its row ID — not the full prompt.
   Ancestors' sections are *not* re-parsed at deeper levels. The first
   draft passed the full prompt down, which caused deep levels to
   re-parse ancestor IDs as fuzzy terms.
2. **`data_rc` indexed parallel to `children`.** On `DataError`, the failed
   child's slot is replaced with `None` instead of dropping the entry —
   the first draft compacted the vec, corrupting `child_index` and
   `row_index` for every child after the failure.
3. **No frame stack.** Restoration is per-node state, not a traversal
   stack: each node stores `view_args` (its last rendered section, default
   `""`, doubles as the view cache key) plus a
   `should_restore_view_args` boolean (set on drill, consumed by
   restore, cleared on any render that is not exactly one valid ID).
4. **Tab and Enter drill identically.** Both replace the level's section
   with `<cursor_id> `, set the restore flag, and recurse into the child;
   only the forwarded key differs (Tab sends `None`). The first draft's
   Tab path contradicted its own comment ("Tab does NOT forward the key"
   next to code that forwarded it).
5. **Copy discipline.** `ranked_rows` holds `RowRef` handles —
   `(child_idx, row_offset)` pairs indexing into the child's original
   `Rc<DataResponse>.rows` vec, deref encapsulated in
   `RowRef::resolve` — not owned row clones and not per-row `Rc`s;
   `RenderCache.lines` is rebuilt only on `dirty` or `term_width`
   change, and `MoveCursor` reuses lines wholesale (only `selected` and
   `active_rows` are recomputed).

---

## 0. Scope and preconditions

- `NodeEntry` is the engine-internal wrapper (`design §2`); all names below
  live in the engine crate, not the host.
- The host calls `eval_prompt(prompt, key)` on the handle; the epoch was
  bumped at menu open. Nothing in this doc opens, closes, or re-enters the
  menu itself.
- One activation = one epoch. Caches are keyed implicitly on epoch (see
  §3.1); the traversal never re-enters the same node twice per descent.

## 1. Entry point: `SlashDotHandle::eval_prompt(prompt, key) -> MenuResult`

Thin wrapper. Nothing lives here but the epoch (design §2).

```
eval_prompt(prompt, key):
  let result = self.root.node_traverse(self.epoch, key, prompt, /*path*/ vec![]);
  match result {
    NodeResult::Continue { prompt, plan } =>
      MenuResult::Continue(plan, prompt),
    NodeResult::Done(msg) => MenuResult::DoneSuccess(msg),
    NodeResult::Error(msg) => MenuResult::DoneError(msg),
  }
```

`path` accumulates drilled IDs for leaf APIs (§2.7). Neither carries
view state.

---

## 2. Traversal algorithm (the core)

This is the structural logic of how keys route through the tree. The view
state (filter, rank, cursor) is a separate concern used only when displaying
a listing or completing a row.

```
node_traverse(epoch, key, prompt_section, path):

  1. Epoch gate: if epoch != cache_epoch, reset EpochState (view_args,
     cursor, restore flag) and set cache_epoch = epoch.

  2. Parse section: scan one word at a time. Build Args (dot modifiers
     + fuzzy terms). First word that is a row ID in the merged child
     set = matched child; the section ends there. Local prompt before
     the ID = this node's view args; everything after = the child's
     tail.

  3. Refresh data: always call node_data() on each child. Rc::ptr_eq
     detects no-change. Rebuild row_index if any child's Rc changed.

  4. Dispatch:
     - Matched child + non-empty tail → recurse with the tail.
     - Matched child, empty tail:
       - flag set → restore (§2.2): cursor seed + view_args rewrite,
         return listing at this level
       - flag clear → cursor seed, return listing at this level
     - No matched child → show filtered listing, handle keys (§2.4)

  5. Handle key at this level:
     - Leaf nodes: Enter → prepare()/submit(). Tab: no-op.
     - Listing nodes:
       - MoveCursor → move cursor, reuse cached lines
       - Tab → drill: append cursor row ID + space to prompt, set
         should_restore flag, recurse into child
       - Enter → same, but forward key; leaf child may submit
       - Esc → Esc restore (§2.2)
       - other → fall through to display

  6. Build result: update view_args + flag-clear check (§2.6). Build
     RenderPlan from RenderCache + cursor/active markers. Return
     NodeResult::Continue { prompt, plan }.
```

### 2.1 Section parse (phase 2)

Parse this node's section one word at a time. Exit as soon as a row ID in
the merged child set is found — the section terminates there (a valid ID
terminates the level's prompt section; design §3). The matched child gets
everything after the ID as its `tail`; everything before the ID is this
node's view args.

The `row_index` HashMap (row_id → child_index) is the lookup. It is
rebuilt only when a child's `node_data()` returns a new `Rc` (§3.2).

```
let trailing_space = prompt_section.ends_with(' ');
let mut matched_child: Option<(String, usize)> = None;
let mut args = Args::default();
let mut pos = 0;

while pos < prompt_section.len() {
    // Skip whitespace runs
    while pos < prompt_section.len() && prompt_section.as_bytes()[pos] == b' ' { pos += 1; }
    if pos >= prompt_section.len() { break; }

    // Find end of word
    let start = pos;
    while pos < prompt_section.len() && prompt_section.as_bytes()[pos] != b' ' { pos += 1; }
    let word = &prompt_section[start..pos];

    // Check if this word is a row ID in the merged child set —
    // a valid ID terminates this level's section
    if let Some(&child_idx) = self.content.row_index.get(word) {
        matched_child = Some((word.to_string(), child_idx));
        break;
    }

    // Not a row ID — parse as dot modifier or fuzzy term
    if let Some(dot) = parse_dot_token(word) {
        args.push_modifier(dot);
    } else {
        args.push_positional(word.to_string());
    }
}

// Tail = everything after the matched ID (zero-copy slice of `prompt_section`)
let tail: &str = matched_child.map(|(_, idx)| &prompt_section[pos..]).unwrap_or("");
// View args = everything before the matched ID (or the whole section if
// no ID matched). Rendered from `prompt_section[..args_end]` where
// args_end = matched_child.map(|_| start_of_id).unwrap_or(prompt_section.len()).
```

Terminology (binding for the rest of this document):

- **tail** — exclusively the unconsumed part handed to the child
  (everything after the matched ID). Never used for the current node's
  own prompt content.
- **view args** — the words *before* the matched ID (fuzzy terms +
  modifiers). They are this node's filter/sort input and are stored in
  `EpochState.view_args` whenever this node renders a listing.
- **prompt section** — this node's slice of the full prompt buffer: its
  view args plus (when drilled) the trailing child ID.

Notes:

- **The matched child's tail is the unconsumed suffix**, not the full
  prompt. Phase 4 recurses with `tail`, never with `prompt_section`.
  This is the recursion contract: each level parses exactly its own
  slice.
- `trailing_space` is computed once from this node's section. When the
  child is drilled into (phase 5 Tab/Enter), the child parses its own section —
  the parent never forwards its own flag.
- `matched_child` stores the ID's `String` clone because the original
  slice is borrowed from `prompt_section` and outlives the loop body;
  the ID is only a `String` clone, not a row clone.
- If a later word would also match a row ID (back-to-back IDs), the walk
  still exits at the first: the first ID terminates the section, and the
  remainder is the child's tail — parsed recursively by the child (its
  own words + its own ID candidates).

### 2.2 Restore logic (phase 4)

High level intuition: Tab / enter drill replaces local view args with just ID
+ trailing space, which then hits the next child down. The restore logic
restores the local view args on the way up, triggered by Esc. Basically,
restore to a filtered view in a level above, at the cursor position you were
in.

Per design §5.1. There is **no frame stack** — that language is a
hold-over from the previous non-recursive design. The prompt is the
state. Each node carries exactly two pieces of restoration state:

- `view_args: String` — this node's last rendered view args section
  (fuzzy terms + modifiers). Default `""`. Doubles as the view cache
  key (§3.3).
- `should_restore_view_args: bool` — set when this node drills into a
  child (phase 5 Tab/Enter); consumed once by the restore below.

```
// Restore trigger (design §5.1 step 1): this node's section is exactly
// one valid child ID, no trailing space, and this node drilled earlier.
// This applies even without Esc — walking back into a bare drilled ID
// always triggers it.

if matched_child.is_some() && tail.is_empty()
    && !trailing_space
    && self.epoch.should_restore_view_args
{
    // Undo the drill:
    // 1. Seed the cursor on the row named by the matched ID — the ID
    //    becomes the focused row, so the user can immediately add
    //    filter terms or re-drill.
    seed_cursor_on(&matched_child.0);  // = seed_or_keep_cursor with cursor_id := the ID (§3.4)
    // 2. Replace the bare ID in the prompt with this node's stored
    //    view args (e.g. `/model claude-4 ` → Esc → `/model sonnet`).
    let new_section = self.epoch.view_args.clone();  // often ""
    self.epoch.should_restore_view_args = false;
    let display = self.derive_view(epoch, &new_section);  // ensure_view + plan (§3.3, §5)
    return NodeResult::Continue { prompt: new_section, display };
}

// Flag clear (design §5.1): whenever this node renders and its section
// is NOT exactly one valid ID, the restore flag is stale — e.g. the
// user typed `/` or added filter terms. Clear it so restore never
// fires on the way down. Performed in phase 6 (§2.6).
```

Without the flag (the ID was typed by hand, e.g. `/model` letter by
letter), nothing is rewritten: this node renders its own listing with
the cursor seeded on the ID's row (§3.4 selection seeding); the deeper
level's listing is hidden (§2.5).

Esc handling when there is no restore to perform (no matched ID, no
trailing space):

```
if key == Esc {
    // Step 2: clear refinement — terms, modifiers, and selection all
    // dropped (e.g. `/model sonnet` → Esc → `/model `). The cursor
    // position is kept (design §5.1: "Keep cursor position").
    if !args.terms.is_empty() || !args.modifiers.is_empty() || args.selection.is_some() {
        return Continue(rewrite_section(""));
    }

    // Step 3: strip trailing space → parent becomes responsible for
    // the prompt again; the PARENT's restore (§2.2 above) fires on the
    // next traversal if the parent drilled.
    if trailing_space {
        return Continue(rewrite_section(tail.trim_end()));
    }

    // Step 4: nothing left at this level → close
    return Done(None);
}
```

Steps 2–4 reference `args` and `trailing_space` from phase 2 —
available because the parse completed even when no child ID matched.

Why the boolean instead of checking "was there ever a drill": the same
ID can be reached by typing. `/model` fully typed must not turn into
`/mo` on Esc — the flag is cleared as soon as the node renders anything
that is not exactly one valid ID (typing `/` already clears it), so it
can only be true immediately after a drill of this node.

### 2.3 Leaf handling (phase 5)

Leaf nodes (no children) handle Enter directly — no view state needed.
Call `prepare()` which defaults to `submit()`.

```
if self.children.is_empty() && matches!(key, Enter) {
    // path_args carries PathArgs.ids (= `path`, the drilled route),
    // the parsed args, and the leaf's suffix.
    let path_args = build_path_args(&path, &args, tail);
    match inner.prepare(epoch, &path_args) {
        SubmitResult::Done(msg) => return NodeResult::Done(msg),
        SubmitResult::Prepared => {
            // Confirm flow: show dialog, wait for next Enter
            self.epoch.confirm_pending = true;
            // Fall through to display (show confirm dialog)
        }
        SubmitResult::NewPath(p) => return NodeResult::Continue {
            prompt: p, display: empty_display()
        },
    }
}
```

`Prepared` falls through to phase 6 with `confirm_pending` set — the render
plan gains the confirm dialog lines (§5) and the *next* Enter resolves the
dialog (`confirm_pending` → `submit()`; Esc → clear flag + restore).

Leaf `SubmitResult` handling: `NewPath(p)` returns the rewritten buffer
directly (pass-through, §2.7); the next `eval_prompt` starts at root
with the new prompt.

### 2.4 Key handling at this level (phase 5)

When no row ID matches, handle the key at this level. For listing nodes,
this means display + cursor + Tab/Enter drill.

```
// Ensure view state is fresh (filter + rank from this node's view args).
// After this call, every handle in ranked_rows was minted from the
// current data_rc (§3.3), so handle resolution below is infallible —
// call sites still go through cursor_id_or_fallback (§3.6) rather than
// unwrap, so a future invariant violation degrades instead of panics.
ensure_view(epoch, view_args);

match key {
    HostKey::MoveCursor(delta) => {
        let n = self.content.ranked_rows.len();
        if n > 0 {
            self.epoch.cursor_idx = (self.epoch.cursor_idx as isize + delta as isize)
                .rem_euclid(n as isize) as usize;
            // Keep the ID in sync (§3.4) — ID is the cross-re-derive truth.
            let rref = self.content.ranked_rows[self.epoch.cursor_idx];
            self.epoch.cursor_id = rref.resolve(&self.content.data_rc).map(|row| row.id.clone());
        }
    }

    HostKey::Tab => {
        // Design §5: "Tab: bare level → promote (trailing space,
        // same as Enter); otherwise complete cursor row's id."
        // Tab ALWAYS drills — never submits. The key is NOT forwarded.
        let Some(active_id) = cursor_id_or_fallback(self) else {
            return NodeResult::Error("cursor row vanished".into());
        };
        let child_idx = self.content.row_index[&active_id];

        // Record restoration state on THIS node: the pre-drill section
        // is replaced by `<active_id> `, so remember what was here.
        self.epoch.view_args = render_view_args(&args);
        self.epoch.should_restore_view_args = true;

        // Drill into the child with its section only (§2.1): exactly
        // `<active_id> `. The parent's view args are NOT forwarded.
        // `path` gains the drilled ID (§2.7). Tab forwards HostKey::None.
        path.push(active_id.clone());
        return self.children[child_idx]
            .node_traverse(epoch, HostKey::None, "<active_id> ", path);
    }

    HostKey::Enter => {
        // Design §5: "Enter: as Tab, but forward key; child may
        // submit() immediately if it is a leaf."
        let Some(active_id) = cursor_id_or_fallback(self) else {
            return NodeResult::Error("cursor row vanished".into());
        };
        let child_idx = self.content.row_index[&active_id];

        self.epoch.view_args = render_view_args(&args);
        self.epoch.should_restore_view_args = true;

        // Drill into child. Enter forwards the key, but the child's
        // section is still exactly `<active_id> ` (§2.1). path gains ID.
        path.push(active_id.clone());
        return self.children[child_idx]
            .node_traverse(epoch, key /* forwarded */, "<active_id> ", path);
    }

    _ => {} // fall through to build result
}

// §3.6: resolve the cursor row's ID, or None if the handle is stale
// (defensive — ensure_view above should have made this impossible).
fn cursor_id_or_fallback(node: &NodeEntry) -> Option<String> {
    node.content.ranked_rows.get(node.epoch.cursor_idx)?
        .resolve(&node.content.data_rc)
        .map(|row| row.id.clone())
}
```

Two corrections vs the first draft:

- **Tab forwards `HostKey::None`, not `key`.** The first draft's Tab arm
  wrote `node_traverse(epoch, key, ...)` under a comment saying Tab does
  not forward the key. The child would then see Tab again and could drill
  a second level in one keypress. Tab's drill is a *prompt mutation*
  (section becomes `id + ' '`), then the child renders; no key action at
  the child.
- **Enter forwards the key but the child's section is the appended ID +
  space**, so the child's parse sees exactly `<id> ` — its own level —
  not the parent's view args. The *recursion* argument is only the
  section; parents `return` the child's result unchanged (§2.7).
- **Restoration state lives on the drilling node** (`self.epoch`, not the
  child's): `view_args` records the pre-drill section, the flag marks
  "this node drilled". The restore (§2.2) runs when this node's section
  is a bare drilled ID, which happens exactly when the child returns
  control.

`MoveCursor` touches only the cursor pair (`cursor_idx` + `cursor_id`,
kept in sync per §3.4) — the ranked rows are untouched and the render
cache is reused wholesale (§5, `plan_from_cache`). `rem_euclid` keeps the
cursor inside `[0, n)`; n is re-read every keypress so filter changes
mid-listing are handled.

### 2.5 Bare ID (deeper level hidden)

When the parse matches a child with an empty tail and no restore to
perform (flag clear — the ID was typed by hand), this node renders its
own listing — header, cursor seeded on the ID's row (§3.4) — but does
**not** recurse: the child level's listing is hidden, this node's list is
rendered (design §3: "No
trailing space = sub-menu hidden"). The ID row renders without a
trailing space, so no child section exists to display.

This is the counterpart of §2.2's restore trigger: same structural
situation, different flag value, different behavior.

### 2.6 Flag lifecycle

`should_restore_view_args` per node:

- **Set** on drill (phase 5 Tab/Enter).
- **Consumed** by restore (§2.2): bare drilled ID re-enters the node.
- **Cleared** in phase 6 whenever the node renders and its section is not
  exactly one valid ID (e.g. `/`, `/model sonnet`, `/model `) — ensures
  restore can never fire on the way down or on a hand-typed ID.

`view_args` per node:

- Default `""`.
- **Updated in phase 6 whenever this node renders a listing** — drilled
  or not (drill replaces the section with the ID, so the pre-drill
  snapshot is taken at drill time).
- Doubles as the ensure_view cache key (§3.3), so view_args and the
  cached ranked_rows never disagree about what was last rendered.

### 2.7 Return flow and `path`

Recursion is pass-through on the way up: a parent that descended simply
`return child.node_traverse(...)` — the child's `NodeResult` **is** the
parent's result, unmodified. There is no bottom-up prompt reassembly.

Consequences:

- `NodeResult::Continue` carries the **newprompt** the child decided on.
  Leaves have full control over it (`SubmitResult::NewPath` is just
  its most visible case — e.g. `/compact` returning `/tree <id> compact
  `). After such a rewrite, traversal restarts at root on the next
  `eval_prompt`.
- **`path` (`PathArgs.ids`) stays**: it is the list of IDs drilled so
  far, one entry per level, passed down as a parameter (not stored in
  `EpochState`). Its consumer is the leaf API — `prepare()`/`submit()`
  use it to know the full route (e.g. `/tree <id> compact` needs the
  tree ID). Listing levels accumulate onto it when drilling; it is
  irrelevant to the prompt, which the current level owns.
- **Optional optimization, deliberately skipped**: a parent could peek at
  a returned `NewPath`/newprompt and short-circuit its own remaining
  traversal. Not worth the complexity — traversals restart at root and
  the tree is shallow.

`path` being a parameter (not shared state) keeps the recursion
re-entrant: two concurrent activations each carry their own `path`.

---

## 3. View state (separate from traversal)

The view state handles filter, rank, cursor, and active markers. It is
only relevant for listing nodes (has children) and only when displaying
or completing rows.

### 3.1 State split

Per-node state is split by *invalidation trigger*, not by data kind:

- **`ContentCache`** — everything derived from `node_data()` output.
  Invalidated when any child's `node_data()` returns a new `Rc` (checked
  per-child, §3.2) or on epoch change. Contains the derived view
  (ranked_rows) and the render cache; the view is re-derived *within*
  this struct on view_args change (the `dirty` flag tracks it) rather
  than being a sibling cache with duplicate inputs.

The first draft had `ViewCache` and `ContentCache` as parallel top-level
structs with overlapping fields (`row_index` typed twice as
`HashMap<String, u16>` and `HashMap<String, usize>`; `cursor_idx` in both
`ViewCache` and `EpochState`). This revision collapses to two structs:
one `ContentCache` (content-derived, includes the derived view + render
cache) and one `EpochState` (user-intent; `view_args` is the view cache
key and lives here).

```rust
struct ContentCache {
    // Raw per-child Rc handles, parallel to self.children.
    // Index i corresponds to children[i]. On DataError, slot i is None.
    data_rc: Vec<Option<Rc<DataResponse>>>,
    active_ids: HashSet<String>, // design §5.2: may have multiple

    // row_id → child index, built from data_rc when any child changes.
    row_index: HashMap<String, u16>,

    // --- Derived view (rebuilt on dirty or view_args change) ---
    // Handles into data_rc: (child_idx, row_offset within that child's
    // rows vec). Resolving a Row is row_ref.resolve(&data_rc) (§3.1).
    // Two ints, Copy — no per-row Rc allocation.
    ranked_rows: Vec<RowRef>,
    dirty: bool,                 // true when data changed since last ensure_view

    // --- Render cache (§4.2) ---
    render: RenderCache,
}

/// Handle to a row in one child's DataResponse. Copy, 6 bytes.
/// Only ensure_view creates handles; they are valid against the
/// data_rc snapshot they were derived from (§3.6 for the policy).
#[derive(Clone, Copy)]
struct RowRef {
    child_idx: u16,
    row_offset: u32,
}

impl RowRef {
    /// The deref chain — data_rc → Rc → rows → offset — lives here and
    /// nowhere else. Returns None on a stale/foreign handle; callers
    /// must handle that per §3.6 (degrade/skip — never unwrap).
    fn resolve<'a>(&self, data: &'a [Option<Rc<DataResponse>>]) -> Option<&'a Row> {
        let resp = data.get(self.child_idx as usize)?.as_ref()?;
        resp.rows.get(self.row_offset as usize)
    }
}
```

`ranked_rows` is a `Vec<RowRef>`: each entry names the *original* row in
its child's `Rc<DataResponse>.rows` vec — the single allocation
`node_data()` made. Filtering and ranking build a new vec of two-int
handles; the deref chain is encapsulated in
`RowRef::resolve(&self.content.data_rc)` (§3.1) — the child's `Rc` keeps
the response alive for the whole cache lifetime. This is the single
most important copy discipline in the file — the first draft cloned
every row twice per re-derive (`iter().cloned()` twice), and an
intermediate `Vec<Rc<Row>>` design would have re-wrapped every row into
its own `Rc` (each `Rc<Row>` must *own* the row, i.e. move it out of the
child's vec). Handles also make `u16` child indices and `u32` offsets
(feasible for any realistic row count) cheaper than pointer-width
`Rc`s: 6 bytes vs 8+ per entry, and no refcount churn on re-derive.

```rust
struct EpochState {
    // Cursor is a row ID (stable across re-rank), resolved to an index
    // lazily against ranked_rows.
    cursor_id: Option<String>,
    cursor_idx: usize,               // cached resolution of cursor_id
    view_args: String,               // last rendered section; view cache key
    should_restore_view_args: bool,  // set on drill, consumed by restore
    confirm_pending: bool,
}
```

**Why `cursor_id` AND `cursor_idx`?** The ID survives re-rank; the index is
the O(1) display lookup. Both are updated together on `MoveCursor`; on
re-derive, `cursor_id` is looked up once in the new `ranked_rows` to
re-derive `cursor_idx` (O(n), once per re-derive, not per keypress). The
first draft kept only `cursor_idx` in `EpochState` and a `cursor_id` in a
separate struct — two sources of truth. Here: ID is authoritative, index
is a memoized resolution.

**Why `row_index: HashMap<String, u16>`?** `u16` suffices — child count is
small (hand-authored menus); it halves the map's entry size vs `usize`.
No parallel `row_child` Vec is needed: on Tab/Enter the lookup is
`row_index[cursor_id]`, and on submit dispatch the same map is read
(D2 demotion and dispatch share one map).

**Why no `data_version` counter?** `Rc::ptr_eq` per child (§3.2) already
answers "did anything change" in O(children) with no hashing. A counter
would be redundant bookkeeping.

**Why no separate `filtered_rows` cache?** Filter and rank are one O(n)
pass over the merged rows (§3.3). An intermediate vec doubles peak memory
for rows that are never read again. Filter + rank runs over
`data_rc` handles directly.

**Why is `field_space` not cached here?** Field metadata is declared on
the parent node (design §2.1: "Field metadata on the parent: declared
once; children conform"). It is a property of the `NodeEntry` itself (or
derived from children's `DataResponse.fields`); it does not change with
node_data() calls. Filter/sort queries use it via the node's own method.

### 3.2 Data refresh

Always call `node_data()` on each child. `Rc::ptr_eq` against the cached
Rc detects no-change without touching rows.

```
refresh_data(epoch) -> Result<(), NodeError>:
  // Always call node_data() — data may be dynamic within a single
  // activation (e.g. fs autocomplete). Rc::ptr_eq detects no-change.
  let mut changed = false;
  let mut new_data: Vec<Option<Rc<DataResponse>>> =
      Vec::with_capacity(self.children.len());

  for (i, child) in self.children.iter_mut().enumerate() {
    match child.inner.node_data(epoch, &empty_path_args()) {
      Ok(response) => {
        // Rc::ptr_eq against the previous slot — no row clones
        let same = match self.content.data_rc.get(i) {
          Some(Some(prev)) => Rc::ptr_eq(prev, &response),
          _ => false,
        };
        if !same { changed = true; }
        new_data.push(Some(response));
      }
      Err(e) => {
        // Keep the previous Rc if one exists (stale rows beat no rows);
        // mark changed so the view re-derives and the error surfaces
        // in the header.
        changed = true;
        new_data.push(self.content.data_rc.get(i).cloned().flatten());
        last_error = Some(e);
      }
    }
  }

  if changed {
    self.content.data_rc = new_data;
    self.content.row_index = self.content.data_rc.iter()
        .enumerate()
        .filter_map(|(child_idx, slot)| slot.as_ref().map(|rc| (child_idx, rc)))
        .flat_map(|(child_idx, resp)| {
            resp.rows.iter().map(move |r| (r.id.clone(), child_idx as u16))
        })
        .collect();
    // Collect active IDs into a HashSet for O(1) membership test at
    // render time (§3.5: "is this row active?").
    self.content.active_ids = self.content.data_rc.iter()
        .filter_map(|slot| slot.as_ref())
        .flat_map(|resp| resp.active_ids.iter().cloned())
        .collect();
    self.content.dirty = true;   // force re-derive on next ensure_view
  }
```

Points the first draft got wrong:

- **`data_rc` must stay index-aligned with `children`.** The first draft
  pushed only successful responses into `new_data`, compacting the vec on
  error and shifting every subsequent child's index — `row_index` then
  points at the wrong child and Tab dispatches the wrong submit handler.
  The fix keeps one slot per child (`Option<Rc<_>>`).
- **On error, prefer stale rows.** A transient handler failure should not
  blank the menu. The stale Rc stays; the view re-derives against it; the
  error is surfaced via the header lines (§5), not by dropping data.
- **Rebuild `row_index` from the raw `Rc`s, not from a merged row vec.**
  The first draft built the index from `data_rc` directly — keep that, but
  note it depends on the index-aligned slot fix above.
- **Duplicate IDs across children.** Design §2.1 requires unique IDs per
  level; `row_index` construction silently lets the last child win. The
  implementation should `debug_assert` on duplicate insertion (dev build
  catches node bugs; release keeps last-wins, which matches "later
  child overrides" used elsewhere in the codebase).
- **Handles are only valid while `dirty` is consumed.** `RowRef`
  (child_idx, row_offset) points into the child's *current* rows vec; a
  child that returns a new `Rc` may have different row counts. The
  `dirty` flag guarantees `ensure_view` rebuilds `ranked_rows` from the
  new data before anything reads the handles — no handle survives a
  data change. Render cache lines are likewise rebuilt on the same
  dirty pass. Violations are handled per §3.6 (degrade/skip, never
  panic).

### 3.3 View derivation (filter + rank + cursor)

Only called when displaying a listing. Re-derives from the child `Rc`s
when inputs change.

```
ensure_view(epoch, view_args):
  // Skip if nothing changed: view_args the same AND content not dirty
  if !self.content.dirty && self.epoch.view_args == *view_args {
    return;  // reuse cached ranked_rows
  }
  self.content.dirty = false;

  // Step 1: parse args once, share the parse result
  let parsed = parse_level(view_args, &selection_rule);

  // Step 2: filter — emit RowRef handles (child_idx, row_offset) into
  // each child's rows vec. No row clones, no per-row Rcs.
  //
  // Resolution is infallible here by construction: we hold the row
  // reference at the same moment we mint its handle. Handles are only
  // resolved later (§3.4, §5), where staleness is handled (§3.6).
  let filtered: Vec<(RowRef, &Row)> = if parsed.modifiers.is_empty() {
    self.content.data_rc.iter().enumerate()
        .filter_map(|(ci, s)| s.as_ref().map(|rc| (ci as u16, rc)))
        .flat_map(|(ci, rc)| {
            rc.rows.iter().enumerate()
                .map(move |(off, row)| (RowRef { child_idx: ci, row_offset: off as u32 }, row))
        })
        .collect()
  } else {
    let space = self.parent_field_space();
    let query = compile_query(&parsed.modifiers, &space);
    self.content.data_rc.iter().enumerate()
        .filter_map(|(ci, s)| s.as_ref().map(|rc| (ci as u16, rc)))
        .flat_map(|(ci, rc)| {
            rc.rows.iter().enumerate()
                .map(move |(off, row)| (RowRef { child_idx: ci, row_offset: off as u32 }, row))
        })
        .filter(|(_, row)| query.matches(row, &space))
        .collect()
  };

  // Step 3: rank (fuzzy + pin) — consumes `filtered` (move, not copy),
  // drops the &Row borrows, returns Vec<RowRef> in ranked order.
  let rank_result = rank_and_pin(filtered, &parsed.terms, parsed.selection.as_deref());

  self.content.ranked_rows = rank_result.rows;
  self.epoch.view_args = view_args.to_owned();

  // Step 4: active markers already stored (active_ids collected in
  // refresh_data); resolved to indices at display time.

  // Step 5: cursor re-seed / keep (§3.4)
  seed_or_keep_cursor(self, &parsed);
  // plus: re-derive cursor_idx from cursor_id against the new ranked_rows
```

Efficiency notes:

- **One `Row`-parse per call** (`parse_level` once, not three times — the
  first draft called it three times: modifiers, selection, terms).
- **No row clones, no per-row `Rc`s.** A `RowRef` is two ints (Copy).
  Resolution goes through the one encapsulated deref chain
  (`RowRef::resolve`, §3.1) — a bounds-checked index into the child's
  original vec, the one allocation `node_data()` made. The `Rc`
  refcount never changes during filter/rank. Filter/match reads the row
  *directly* (the `(RowRef, &Row)` pair) — resolving through the handle
  inside the filter loop would be circular; handles are minted and
  used-in-place in the same pass.
- **Filter is fused into the collect** when modifiers exist — no
  intermediate `all_rows` vec (the first draft materialized
  `all_rows: Vec<Row>` then filtered into a second vec).
- **`rank_and_pin` takes `filtered` by value** (it consumes and reorders).
  No second vec. Fuzzy ranking needs row fields (id/description) —
  resolved through the handle inside the comparator; resolution is two
  array indexes, cheaper than any clone.
- The cache check is `view_args` string equality + one bool — no hashing
  of row data.

### 3.4 Cursor (design §4: "cursor lives in the view")

The cursor is a row ID, not an index. It survives re-rank and re-filter
because the row ID stays with the row regardless of order.

```
seed_or_keep_cursor(view, parsed):
  // If cursor_id is still present in the current ranked_rows, keep it
  // and refresh the cached index. Row IDs resolve through the handle:
  // r.resolve(&data_rc).id.
  if let Some(ref id) = view.epoch.cursor_id {
      if let Some(idx) = view.content.ranked_rows.iter().position(|r| {
          r.resolve(&view.content.data_rc).map_or(false, |row| row.id == *id)
      }) {
          view.epoch.cursor_idx = idx;
          return;
      }
  }

  // Re-seed: selection (if any), then first active row, then row 0
  view.epoch.cursor_id = parsed.selection
      .cloned()
      .or_else(|| view.content.active_ids.first().cloned())
      .or_else(|| view.content.ranked_rows.first()
          .and_then(|r| r.resolve(&view.content.data_rc))
          .map(|row| row.id.clone()));
  view.epoch.cursor_idx = 0;
```

`MoveCursor` (§2.4) updates both `cursor_idx` (for display) and
`cursor_id` (for persistence across re-derives):

```
on_move_cursor(delta):
  cursor_idx = (cursor_idx + delta).rem_euclid(n);
  cursor_id  = ranked_rows[cursor_idx].resolve(&data_rc).id.clone();
```

The index is authoritative for display; the ID is authoritative across
re-derives. Keeping both in sync is two lines of code; the first draft's
single-`cursor_idx`-in-`EpochState` design forced an O(n) `position()`
lookup on every `MoveCursor` for long lists (`/tree` 1000+ rows), which
contradicted its own "cache the offset" rationale.

### 3.5 Active markers (design §5.2)

`node_data().active_ids` collected into a `HashSet<String>` during
`refresh_data`. At render time, checking whether a row is active is an
O(1) `HashSet::contains` call — important for cursor-move reuse where
every row's marker is re-evaluated against the set. Multiple rows may be
active (e.g. multiple models currently in use). The `*` marker shows on
active rows; `>` on the cursor row; `>` wins when both are on the same
row (the cursor must always be visible).

`active_ids` is collected during `refresh_data` (from each child's
`DataResponse`), not re-derived in `ensure_view` — one place reads it.

### 3.6 Stale handles — error-handling policy

`RowRef::resolve` returns `Option`, and the `None` case is a **broken
invariant**, not an expected input: handles are minted and consumed
within one `ContentCache` generation, and the §3.2 dirty discipline
guarantees `ensure_view` rebuilds `ranked_rows` whenever any child's
`Rc` changes. A failing `resolve` means a bug in that discipline (or
corrupted state), never user input. Policy:

1. **Filter/rank (§3.3): infallible by construction.** Handles are
   minted next to the row they name — the `(RowRef, &Row)` pair — so
   there is no resolution and nothing to handle. This is why §3.3 has
   no `resolve` calls at all.
2. **Key handling (§2.4): degrade, don't panic.** Tab/Enter go through
   `cursor_id_or_fallback` (returns `Option<String>`); a `None` yields
   `NodeResult::Error("cursor row vanished")` — the menu closes with an
   error message rather than a panic in the host's render loop. MoveCursor
   propagates the `Option` into `cursor_id` (a stale handle simply
   doesn't update the ID; the next re-derive re-seeds).
3. **Render (§5): skip, don't panic.** A handle that fails to resolve
   renders as an empty placeholder line — indices (`option: i`) stay
   aligned with `ranked_rows`, so cursor/active markers remain correct.
   The skip is visible (blank row) instead of silently shifting the
   listing.
4. **Never `unwrap` on resolve.** If a call site can prove the invariant
   locally (as §3.3 does), it doesn't call `resolve` at all. Otherwise
   it must handle the `Option`. A `debug_assert` inside `resolve` is
   fine for diagnostics; an `unwrap` at a call site is not.

---

## 4. Internal structures

### 4.1 `NodeEntry`

```rust
struct NodeEntry {
    inner: Box<dyn SlashDotNode>,
    children: Vec<NodeEntry>,
    content: ContentCache,   // content-derived (§3.1)
    epoch: EpochState,       // user-intent (§3.1)
    cache_epoch: u64,        // last epoch; triggers EpochState reset
}
```

Note on naming: `epoch` here is the *epoch-scoped* state struct, not the
epoch counter itself. The counter is passed as a parameter through the
traversal. `cache_epoch` (on `NodeEntry`, not in either sub-struct) tracks
the last-seen epoch; when it differs from the current epoch (new menu
activation), `EpochState` is reset.

### 4.2 `RenderCache` — cached rendered lines

```rust
struct RenderCache {
    lines: Rc<Vec<RenderLine>>,   // rebuilt when inputs change, Rc for zero copy
    columns: Vec<ColumnDef>,      // rebuilt when field metadata change
    term_width: usize,            // cache key: invalidate on resize
}
```

Invalidation is caller-driven, not signature-based. The caller (the
traversal loop) already knows whether it just re-ranked (`content.dirty`
was consumed by `ensure_view`). It passes a `dirty: bool` flag to
`build_render_plan()`. If dirty, the cache is rebuilt. If clean
(cursor move only), the existing lines are reused.

This avoids hashing ranked_rows (expensive, fragile) — the caller already
has the answer. Invalidation triggers:

- **Cursor move** (`dirty=false`): only cursor index changes — lines
  reused from cache; the plan's `selected` + `active_rows` are recomputed
  (cheap: `Option<usize>` + resolved IDs, §5).
- **Filter/rank change** (`dirty=true`): lines rebuilt from new
  ranked_rows.
- **Terminal resize** (`dirty=true`): lines rebuilt — table column
  widths, header truncation, and word wrapping are all width-dependent.
  `term_width` is a cache key; compare against current width to detect
  resize.

Cursor and view_args live in `EpochState`, NOT in RenderCache. RenderCache
is purely content-dependent.

The cache does NOT need to track height — the host already windows
via overlay_window(). The engine sends all ranked rows; the host
decides which to display.

### 4.3 What is stored where (single source of truth)

| State | Struct | Survives | First draft's bug |
|---|---|---|---|
| `data_rc`, `row_index` | `ContentCache` | until a child returns a new Rc | `data_rc` compacted on `DataError` |
| `ranked_rows`, `active_ids`, `dirty` | `ContentCache` | until next ensure_view | split across two structs |
| `cache_epoch` | `NodeEntry` | until next epoch change | triggers EpochState reset on new activation; belongs at node level since it bridges both caches |
| `render.lines`, `columns`, `term_width` | `RenderCache` (inside ContentCache) | until dirty/resize | — |
| `cursor_id` + `cursor_idx` | `EpochState` | epoch | ID and index in two structs, out of sync |
| `view_args` + `should_restore_view_args` | `EpochState` | epoch | first draft had no per-node restore state (single `saved_filter` slot / frame-stack detour) |
| `confirm_pending` | `EpochState` | epoch | — |

### 4.4 `NodeResult` (internal)

```rust
enum NodeResult {
    Continue { prompt: String, plan: RenderPlan },
    Done(Option<String>),
    Error(String),
}
```

The RenderPlan is built from RenderCache + cursor/active markers.
RenderPlan.lines is `Rc<Vec<RenderLine>>`, shared directly from
RenderCache — zero-copy on cursor moves.

---

## 5. Render plan construction

Clean new path, independent of old `PlanBuilder`/`MenuState`.

```
fn build_render_plan(cache: &ContentCache, header_lines: &[String],
                     field_space: &Option<FieldSpace>, term_width: usize) -> RenderPlan {
  // No Rc::new(...) of row data here — ranked_rows are RowRef handles
  // into the children's Rc<DataResponse>.rows vecs (§3.1); rendering
  // resolves each handle through data_rc. The first draft wrapped a fresh clone in a
  // new Rc (`Rc::new(display.rows.clone())`), defeating the entire
  // zero-copy design. The only fresh allocation on this path is the
  // rendered `lines` vec itself.
  let mut lines = Vec::new();
  // Design §6 panel structure order:
  //   1. header() output (help text, stats, previews)
  //   2. table header line (if child rows have columns)
  //   3. selectable rows with > (cursor) / * (active) markers
  for h in header_lines {
    lines.push(RenderLine { text: h.clone(), style: Style::HEADER, is_header: true, ... });
  }

  // Table width: handled in the render plan builder. The host passes
  // term_width (from its terminal dimensions). The engine truncates
  // headers and wraps table columns to fit. Design §6: "Headers
  // truncated to fit terminal."
  //
  // Vertical pagination: the host does a FULL REWRITE on every keypress
  // (crossterm, not ratatui — no double-buffering). The host already
  // windows via overlay_window() (at most 2/3 terminal height). The
  // engine could send only visible rows to reduce engine-side work
  // (table rendering for 1000+ rows), but the host redraws everything
  // anyway. Defer this optimization — send all ranked rows for now.
  //
  // If we do paginate later: send Vec<RenderLine> for visible rows +
  // scroll_offset + total_count. The host renders what it receives.
  // Stale-handle rule (§3.6): resolve eagerly per row; a handle that
  // fails resolves to a skipped-line marker rather than panicking or
  // silently shifting indices (markers index by rank position i).
  let row_texts = if let Some(space) = field_space {
    build_table_rows(&cache.ranked_rows, &cache.data_rc, space, term_width)
  } else {
    cache.ranked_rows.iter()
        .map(|r| match r.resolve(&cache.data_rc) {
            Some(row) => format_row(row),
            None => format_row_skipped(),  // renders as an empty row line
        })
        .collect()
  };

  for (i, text) in row_texts.iter().enumerate() {
    for (j, sub) in text.split('\n').enumerate() {
      lines.push(RenderLine { text: sub.to_string(), ... option: i, is_marker_row: j == 0 });
    }
  }

  RenderPlan {
    lines: Rc::new(lines),   // this Rc is fresh — built once per dirty
    selected: Some(cursor_idx),
    active_rows: resolved_active_rows,
  }
}
```

**The Rc/handle split is the point.** Row data lives in the children's
`Rc<DataResponse>.rows` vecs (kept alive by `data_rc`); `ranked_rows`
holds only two-int handles into them, and `lines` is the *rendered
text*, rebuilt only when `dirty || term_width changed`.
`MoveCursor` skips this function's row loop entirely: the plan is
assembled from the cached `lines` Rc with only `selected` and
`active_rows` changed:

```
fn plan_from_cache(cache: &RenderCache, cursor_idx: usize,
                   active_rows: &[usize]) -> RenderPlan {
    RenderPlan {
        lines: cache.lines.clone(),      // Rc share, no rebuild
        selected: Some(cursor_idx),
        active_rows: Some(active_rows.to_vec()),
    }
}
```

`active_rows: Vec<usize>` is recomputed per plan — it is a handful of
ints; caching it buys nothing.

`field_space` on the render path: columns come from the parent's field
metadata (design §2.1). If the node has no field metadata, rows render as
plain text (`format_row`); the first draft's `columns` field in
`RenderCache` is only populated when a field space exists.

---

## 6. Borrow discipline

Per design §2: "Parent-state access happens only outside the child loop."

1. **Own state** (phases 0–2): `self.content` and `self.epoch` accessed.
   No child borrows. All parent-level state needed for the descent is
   prepared here (args parsed, filter state snapshotted).
2. **Descend** (phase 5 Tab/Enter): snapshot `view_args` on `self.epoch`,
   then `self.children[idx].node_traverse(...)`.
   One child at a time; the recursive call takes `&mut self.children[idx]`
   while no other child or `self.content`/`self.epoch` borrow is live.
   The child's result is returned unmodified (§2.7 pass-through).
3. **Post-processing** (phase 6): build from cache. No child borrows.

The recursion is `&mut self` on one child at a time; the prompt section,
`path`, and key are passed by value/borrow as arguments — nothing is read
sideways from parent state during the child call, and the child's result
is returned unmodified (§2.7).

---

## 7. Implementation order

### 4a. Traversal skeleton
- `NodeEntry`, `ContentCache`, `EpochState` (view_args + restore flag)
- Section parse (phase 2)
- Restore logic (phase 4) — prompt manipulation + flag lifecycle
- Bare-ID render (§2.5)
- Leaf handling (phase 5) — stub prepare/submit, path in PathArgs
- Tab drill (phase 5) — stub view, dispatch to child with section only,
  pass result through
- `NodeResult`, `eval_prompt` wiring

### 4b. View state
- Data refresh with index-aligned `data_rc` (Rc::ptr_eq change detection)
- Filter + rank over `RowRef` handles (§3.3)
- Cursor seeding + `cursor_id`/`cursor_idx` sync (§3.4)
- Active marker collection (§3.5)

### 4c. Render plan
- `RenderCache` with cached lines + `term_width` key
- `build_render_plan()` (dirty path) + `plan_from_cache()` (cursor path)
- Table layout from field_space

### 4d. MoveCursor
- Update cursor_id + cursor_idx, reuse cached lines via `plan_from_cache`
- Test end-to-end

### 4e. Tab completion + drill
- Complete cursor row's ID, snapshot view_args, set restore flag, drill
  with section only

### 4f. Esc + restore
- §5.1 restore trigger (applies without Esc) + Esc steps 2–4
- Flag lifecycle tests: typed `/model` does NOT restore; drilled
  `/model claude-4` → Esc → `/model sonnet` does
- Multi-level test scenarios (`/tree <id> compact` two-deep)

### 4g. Tests
- Port interpreter tests to new harness

---

## 8. Key design decisions

| Decision | Choice | Rationale |
|---|---|---|
| Traversal vs view | Separate concerns | Traversal is structural (section parse, key dispatch, Esc). View is data (filter, rank, cursor). View only used for listing display. |
| State split | `ContentCache` + `EpochState` | Invalidation triggers differ: content vs user intent. First draft's `ViewCache` overlapped both. |
| Rows in caches | `RowRef` handles + encapsulated `resolve`, never owned clones or per-row Rcs | Rows live in the child's `Rc<DataResponse>.rows`; handles are two Copy ints and the deref chain lives in one method (`RowRef::resolve`). An intermediate `Rc<Row>` design would re-wrap every row (each `Rc<Row>` must own its row). |
| filtered_rows cache | None | Filter is fused into the rank pass; an intermediate vec is dead weight. |
| Cursor identifier | Row ID (String), authoritative | Survives re-rank/re-filter. |
| cursor_idx | Cached, kept in sync on MoveCursor | O(1) display lookup; re-resolved from ID on re-derive (once per re-derive, not per keypress). |
| active_rows | `Vec<String>` IDs in cache, `Vec<usize>` in plan | IDs stable across filter/sort; indices resolved at display time. |
| Child index lookup | `HashMap<String, u16>` (row_index) | O(1) by row ID; `u16` shrinks entries; no parallel Vec. |
| data_rc shape | `Vec<Option<Rc<DataResponse>>>` parallel to children | Errors keep slot alignment; dispatch never shifts. |
| Esc restoration | removed — per-node `view_args` + `should_restore_view_args` | The prompt is the state; restore is per-node undo of a drill, not a traversal stack. |
| `path` | Parameter of node_traverse, consumed by leaves | `prepare()`/`submit()` need the full route; passed down, never shared state. |
| Data refresh | Always call node_data(), Rc::ptr_eq | Design §2: "cheap nothing-changed signals." |
| View re-derive | Only if view_args changed or dirty | String compare + bool; no hashing. |
| MoveCursor | No re-derive, `plan_from_cache` | Cursor lives in the view (design §4); lines Rc-shared. |
| Recursion payload | Section only, plus forwarded key on Enter | Each level parses exactly its own slice; no ancestor re-parse. |
| Tab forwarding | `HostKey::None` at child | Tab is a prompt mutation + render, not a key action. |
| Restoration | Per-node `view_args` + flag, no stack | Prompt is the state; drill sets flag, bare-ID re-entry consumes it; any non-bare-ID render clears it (§2.6). |
| Return flow | Pass-through on the way up | Child's NodeResult is returned unmodified; leaf owns newprompt; NewPath rewrites restart at root (§2.7). |
| Columns | Derived from field_space at render time | Not per-row state; `RenderCache.columns` populated only when a field space exists. |
| is_leaf | Not cached | `children.is_empty()` is O(1). |
| RenderCache | Caller-driven invalidation, Rc share | No hash: caller passes dirty flag. Lives inside ContentCache. |
| Render marker precedence | `>` cursor wins over `*` active | Cursor must stay visible when it rests on the active row. |
| Vertical pagination | Deferred | Host already windows via overlay_window(). Send all ranked rows. |
