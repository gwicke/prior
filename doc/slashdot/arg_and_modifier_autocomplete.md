# Positional arg & modifier auto-complete
Needs
- Listing nodes: pervasive auto complete on dot modifiers
  This is *generic*; currently no need for customization beyond maybe
  field config level enums or value ranges.
  Uniform behavior is valuable. 
  - Share machinery, but have one default implementation for listing nodes.
- Leaf nodes: optional parameter and possibly modifier auto complete
  Nothing generic; usually needs specific value completes
  THIS is the target.
  - Use listing node complete machinery, but provide custom completion data
- Longer term: File complete in regular prompt editing, outside of the slash
  menu context

Goals
- Reuse listing, table rendering machinery / uniform UX: parsing, UI
- Avoid complications with child data() / traverse interaction for dynamic
  data

## Design sketch
Decision: Support single word (maybe later quoted string) completion only
- want to leverage listing machinery / fuzzy filtering, and provide consistent
  UX: need shared understanding of option syntax
- only auto complete word *directly* before cursor (space-delimited, one
  word, no spaces within)
- engine receives `cursor_pos` (byte offset into the level's prompt
  suffix) on every eval. v1 only *supports* end-of-line completion:
  completion is suppressed unless `cursor_pos == suffix.len()`, so the
  dialog can never target a different word than the one being edited.
  Mid-line completion pairs with mid-prompt file expansion later.
- provide earlier parameters to completion method, so that behavior can change
  by position (e.g. sandbox from / to path args) - PathArgs
- keep handler high level, focused on providing data
- handle all prompt manipulation and display at engine level

## Behavior: Implicit open, stateless, one word, space dismisses
- feed word *directly* (no space) before cursor position to auto completer
- zero matches or exact unique match -> do not show dialog
- exactly one *non-exact* match: one-row dialog; user confirms with Tab
  (no silent auto-expand — typing never mutates the prompt)
- multiple matches: show dialog; up/down navigation, tab to expand into
  prompt.
- Tab = in-place expansion: replaces the word before cursor with the
  cursor row's id (no trailing space written directly). The engine
  re-runs completion on the expanded word within the same turn:
  - re-run non-empty → dialog continues on the new rows. This drives
    step-wise directory drilling and modifier key → value progression —
    the handler is in charge by choosing what it returns for a word.
  - re-run empty (word is now an exact/final completion) → append a
    trailing space as a convenience (space also dismisses, per below)
- adding a space implicitly dismisses complete (no word right before cursor)
- Enter and Esc do not have special meaning for completion. Both are handled
  in the surrounding context (e.g. might submit the raw in-flight word,
  clear parameters etc); they bypass the completion view entirely.
- Key routing while the dialog is up (completion rows non-empty this
  frame): `MoveCursor` (incl. page keys — the host computes the delta
  from the visible rows) and `Tab` drive the completion view's cursor;
  the listing view underneath keeps its own cursor, revealed on dismiss.
- Confirmation dialog (`confirm_pending`) has highest precedence:
  completion is suppressed while it is up.
- Display: `header()` is shown in the dialog too (detail section).
  Completion `Err` is surfaced at the top of detail (`[ERROR]: …`),
  above the header lines.

Pros
- Good fit with existing listing / fuzzy filtering by default behavior
- Fluid typing experience, does not interfere or "lock" user into dialog
- Good discoverability of e.g. dot modifiers
- Stateless implementation, prompt-is-the-state
- Can potentially handle mixed modifier / file completion

Cons
- Potential for false-positive noise, and performance challenges if firing too
  frequently
  - Quick syntactic test to rule out regular words: dot prefix for modifiers,
    dot and/or slash presence for file paths. The test lives *inside* each
    completer — there is no engine-level gate; the engine calls completion
    on every passive frame and renders whatever non-empty rows come back.

## Modifier handling
- leaf nodes usually do not support modifier syntax; dot prefix overlaps with
  file names, e.g. `.gitignore`
- Decision: expose only flat positional args to the autocompleter and to
  handlers (not pre-parsed modifiers); conversion of flat args to modifiers
  happens in a separate pass in NodeEntry view code, or in prepare/submit
  handlers of leaf nodes that want modifier-ish parsing. `Args` dissolves
  into a flat `args: Vec<Range<usize>>` member on `PathArgs`.
  - Modifier or positional is determined not just by syntax, but also by valid
    keys; if key is not in locally valid set, stays positional / fuzzy match
    in listing.
    - Valid key set = `FieldSpace::names(true)` (always includes `id`,
      `description`) + injected `sort`. Listing levels only — leaves have
      no field space, so every dot word at a leaf is positional.
    - Consequence (accepted): an invalid-key dot word (`.srot:price`)
      becomes a fuzzy term instead of today's silently-ignored modifier —
      a typo can filter the listing to zero rows instead of showing it
      unfiltered. Completer guidance makes this unlikely enough.
  - Field source stays infer-from-rows by default; explicit `fields()`
    overrides (declare fields on commands that want dot completion).

## Slashdot code changes
Data structure changes
- Replace `struct Args` with a flat `args: vec<Range<usize>>` member in
  `PathArgs`, positional only

Processing pipeline
- parse_section: stop distinguishing dot modifiers, push all non-id words into
  args (positional only)
- Feed raw args to autocomplete — after parse, for
  `key ∈ {None, MoveCursor, Tab}` while `!confirm_pending`: leaf → trait
  `complete_parameter`, listing → internal default. Empty/None falls
  through to the listing renderer; `Err` falls through with an error
  line; rows → completion renderer. Exact branches: see "Engine
  dispatch" in the Implementation plan.
- Else: Feed raw args to view code - extracts dot modifiers based on actual valid
  keys / values, leaves others as fuzzy filters
  return listingRenderer(rows, ...)

Rendering
- Extract rows -> RenderPlan pipeline into its own module with owned caches
  and cursor tracking (`view.rs` / `RowView` — see Implementation plan)
- Have separate instances for regular rendering vs. auto-complete rendering
  within a single NodeEntry
- Use dedicated instance for inline file complete (future mid-prompt work)
- Completion plans default to `Value | Description` column labels for id /
  description, but completers can override labels in response. Modifier keys
  should use "Modifier | Description". (the node's
  `columns()` describe listing fields, not completion tokens); 
  detail = optional error line + `header()`.

## SlashDotNode trait completion API sketch

The trait extension is aimed at leaf nodes only. Listings use an internal
implementation that is aligned with generic listing filtering / ranking, and
has access to merged child data.

```rust
/// Completion payload: candidate rows + optional dialog column labels
/// for (id, description).
pub struct CompletionRows {
    pub rows: Vec<Row>,
    pub labels: Option<(String, String)>,
}

fn complete_parameter(
  &mut self,          // can access instance data (fs caches etc.)
  args: &PathArgs,    // flat word ranges + suffix for this level
  cursor_pos: usize,  // byte offset into args.suffix (EOL in v1)
) -> Result<Rc<CompletionRows>, DataError>
```

Notes:
- `PathArgs` are already built by the shallow parse, so free: flat word
  ranges give the in-flight word (`word_before(cursor_pos)`), the earlier
  committed parameters (position context, e.g. sandbox src vs dst), and
  the full suffix. Normally cursor at end-of-string; non-end positions
  arrive with mid-line completion later.
- Leaf nodes only: routing is `children.is_empty() ? trait : internal
  default` — a listing's trait override is ignored (it has no access to
  the merged child rows anyway). Default impl returns an empty `Ok` —
  the common case, and the reason every node can be called every frame.
- No epoch for cache invalidation; completion caching should be based on
  logical checks like directory mtime, or time since last refresh if
  necessary.
- Return values:
  - `Err` on issue (e.g. fs permissions): no dialog; engine surfaces the
    message at the top of detail (`[ERROR]: …`) — it persists exactly as
    long as the completer keeps failing (recomputed per frame, no state).
  - Empty row vec if no match, or if *every* match is an exact match (nothing
    left to complete) — no dialog.
  - If at least one non-exact match exists: return the **full** set,
    including any exact match (no filtering or highlighting in v1).
  - Cache Rc in handler until response changes, so downstream can cache based
    on `Rc::ptr_eq` (an unchanged Rc skips the view re-derive and keeps
    the dialog cursor where it was).
  - Optional column-label override: the `labels` pair applies to
    (id, description); default `("Value", "Description")` for leaves
    (listing internal default: "Modifier | Description").
- Heavy IO is not ideal with sync, but fine short term- considering to switch
  entire engine to async later. Could also enable debouncing - don't trigger
  auto-complete on pastes or similar.

---

## Implementation plan

### Steps

1. **Flat args reshape** — `PathArgs.args`, `parse_section`, modifier
   extraction pass, Esc ladder, accessor renames. No completion yet.
   Only behavior delta: invalid-key dot words become fuzzy terms.
2. **`cursor_pos` threading** — `eval_prompt` → `node_traverse` → child
   recursion (suffix-relative), EOL guard, host char→byte conversion.
3. **View extraction** — `view.rs` `RowView` over `Rc<ViewRows>`:
   cursor + row-line cache + plan assembly move out of `NodeEntry`;
   the rows pipeline (query parse → filter/rank/sort) and its memo
   stay content-side, keyed off `&PathArgs`. Two view instances.
   Mechanical; existing tests pin behavior.
4. **Listing completion** — `NodeEntry::default_complete` + completion
   `RowView` + dialog plan + key routing (`None`/`MoveCursor`) + `Err`
   detail line.
5. **Tab expansion** — word-range replace + same-turn re-run +
   trailing-space convenience.
6. **Trait `complete_parameter`** + leaf routing.
7. **Sandbox leaf** — `AddSandboxMappingNode` fs completion (arg 1:
   host FS; arg 2: reuse the existing sandbox-aware helper —
   `sandbox_list_dir` → host fallback, as in
   `complete_path_prefix_sandbox`), DrillEnter guard/prefill, submit
   guard via `SubmitResult::Error`, two-path integration test.

Steps 1–3 are independently shippable refactors; 4–7 each add one
user-visible capability.

### Internal API changes

**`PathArgs`** (node_impl.rs):

```rust
pub struct PathArgs {
    pub ids: Vec<String>,
    pub suffix: String,
    pub args: Vec<Range<usize>>,  // was: args: Args { modifiers, positional }
}
```

- `struct Args` deleted. `modifiers()` deleted; `positional()` renamed
  `args()` (word slices). Callers today are contained: `node_entry.rs`
  (fuzzy-terms join, Esc ladder, parse tests) + the `LeafNode` test +
  the sandbox submit fallback — no `src/` caller touches `modifiers()`
  (verified), and only a handful use `positional()`.
- New helpers (engine-side, shared by completion + expansion):
  - `word_before(cursor) -> Option<Range<usize>>` — scan back from
    `cursor` to a space (same space-delimited rule `parse_section`
    uses); range = `word_start..cursor`.
  - `committed(cursor)` — all word ranges except `word_before`, for
    handlers that branch on position (sandbox arg 1 vs arg 2).

**`parse_section`** (node_entry.rs): drop the dot-modifier branch; every
non-ID word is pushed into `args`. Matched-child check stays first
(unchanged).

**Modifier extraction** — new free fn (filter.rs):

```rust
fn split_modifiers(words: &[Range<usize>], suffix: &str, space: &FieldSpace)
    -> (Vec<ModifierToken>, Vec<&str>)  // (modifiers, fuzzy terms)
```

Rule: word starts with `.` && word != `..` && key before first `:` ∈
`space.names(true)` ∪ {`sort`} → modifier (bare key → empty value, as
today); otherwise fuzzy term. `compile_query` unchanged — for *valid*
keys behavior is byte-identical to today (unknown keys were already
ignored there); the only delta is invalid-key dot words now feeding
fuzzy ranking instead of being dropped. `ensure_view` calls this before
`compile_query`; the fuzzy-terms join uses the remainder.

**Esc ladder**: `!pa.args.is_empty() || parsed.matched()` (flat args —
same reachability as today).

**`eval_prompt` / `node_traverse`**: add `cursor_pos: usize` (byte
offset, relative to that level's suffix).

- Keep the 3-arg `eval_prompt(prompt, key, term_width)` as a convenience
  (= cursor at `prompt.len()`) so the ~50 existing test call sites don't
  churn; add `eval_prompt_with_cursor(..., cursor_pos)` for the REPL.
- Host conversion: `editor.cursor` is a *char* index into the buffer;
  the engine line strips the leading `/`. Compute the byte offset into
  `line` via `line.char_indices().nth(editor.cursor - stripped_chars)`.
- Recursion: `child_cursor = cursor_pos.saturating_sub(tail.start)` —
  cursor above the child's section ⇒ 0 ⇒ no word in the child suffix ⇒
  completion inert there. (Emit only when the cursor lies in the tail —
  see dispatch below.)
- EOL guard: completion runs only when `cursor_pos == suffix.len()`
  (trivial per-node compare; equivalent at every level since child
  ranges are suffix-relative).

**`SlashDotNode`**: `complete_parameter` added as sketched above
(default `Ok(Rc::new(CompletionRows { rows: Vec::new(), labels: None }))`).
No other trait changes; `HostKey` unchanged.

**`SubmitResult`**: new variant `Error(String)` → engine returns
`Continue` (menu stays open) with NodeEntry rendering the message at
the top of detail as `[ERROR]: …` — the same surfacing slot as a
completion `Err`, but *stored* on `EpochState.error` (an engine `Err`
re-derives every frame; a submit error happens once, on the Enter key)
and cleared when the level's prompt section changes. Needed for: Enter
on a param leaf with missing/invalid args (sandbox: nonexistent path).
Drill-arrival alternative (suppresses DrillEnter-submit before
dispatch): `drill_rewrite` pre-fill of an example path. `MenuResult`
unchanged.

### Render pipeline extraction

New module `view.rs`. The cut runs along the existing
`ContentCache` / `RenderCache` line: **content vs presentation**.
RowView receives only *ordered rows* — it never parses, queries,
filters, ranks, or derives keys.

```rust
/// The view's single input: source rows + display order + markers,
/// bundled behind one Rc so "data OR query OR typed input changed"
/// collapses to one ptr_eq signal. Minted content-side only — fields
/// private: storage (slots / RowRef / id set) is an implementation
/// detail the render side never sees.
pub(crate) struct ViewRows {
    slots: Vec<Option<Rc<DataResponse>>>, // RowRef resolution source
    order: Vec<RowRef>,                   // display order (identity for completion)
    active_ids: HashSet<String>,           // union over slots
}

impl ViewRows {
    /// Content-side constructor (rows pipeline / completer wrap only).
    fn new(slots: Vec<Option<Rc<DataResponse>>>, order: Vec<RowRef>,
           active_ids: HashSet<String>) -> Self;

    /// The render side's ONLY data surface: rows in display order with
    /// the active (`*`) flag resolved — resolution and active-set
    /// lookup happen inside. Stale handles degrade to skipped rows
    /// (§3.6). `iter().enumerate()` gives render + active indices;
    /// `position`/`nth` give seeding and the cursor row.
    fn iter(&self) -> impl Iterator<Item = (&Row, bool)> + '_;
    fn len(&self) -> usize;   // + is_empty()
}

pub(crate) struct RowView {
    last_view: Option<Rc<ViewRows>>,  // last view passed to plan; ptr-diff = dirty
    // --- cursor (per view) ---
    cursor_id: Option<String>,
    cursor_idx: usize,
    cursor_user_moved: bool,
    // --- render cache ---
    last_render_ctx: RenderContext,  // rebuild rows when it differs (PartialEq)
    render: RenderCache,         // table_header, rows: Rc<Vec<LogicalLine>>, active_rows
}

/// Everything per-frame that isn't the rows themselves.
pub(crate) struct ViewRequest<'a> {
    /// Listing: the node's `columns()`. Completion: two ColumnDefs
    /// built from the response labels.
    columns: &'a [ColumnDef],
    /// Fresh this frame: optional `[ERROR]` line + `header()`.
    detail: Vec<LogicalLine>,
    /// Cursor reconcile target when the view changed (matched child id
    /// / restore target); `None` for completion.
    seed_id: Option<&'a str>,
    /// §5.1 restore only: force cursor without a view change.
    pin: Option<&'a str>,
    /// Applied after reconcile (MoveCursor / page keys).
    cursor_delta: Option<i32>,
    /// Full render context, passed through unchanged (`RenderContext`
    /// is `Copy`) — future metrics land here without touching this API.
    render_ctx: RenderContext,
}

/// What one frame needs: the rendered plan + the cursor target.
pub(crate) struct ViewPlan {
    /// Row under the cursor after reconcile/delta — the drill /
    /// expansion target. `None` when the view has no rows.
    pub cursor_row: Option<Row>,
    pub plan: RenderPlan,
}

impl RowView {
    /// One-shot: ptr-compare `view` vs last → cursor reconcile →
    /// pin → cursor_delta → rebuild row lines iff view changed or
    /// `render_ctx` changed → assemble. No key, no query: all caching
    /// is ptr/context driven and internal. `view` is taken by value
    /// (stored as `last_view`); the returned `cursor_row` covers
    /// drill/expand — callers need no second call.
    fn plan(&mut self, view: Rc<ViewRows>, req: &ViewRequest<'_>) -> ViewPlan;

    /// Epoch gate / dialog close: drop view + cursor + caches.
    fn reset(&mut self);
}
```

Drill arms (`Tab`/`Enter` at a listing) call `plan()` and discard its
`RenderPlan` — on an unchanged-view frame that costs one `header()` +
assembly (rows `Rc` reused), effectively free, in exchange for a single
uniform entry point per frame.

Why `Rc<ViewRows>` and not a literal `Rc<dyn Iterator>`: iterators are
single-use and can't outlive the slots they borrow, while rendering,
cursor reconcile, and active markers all need repeat passes.
`Vec<RowRef>` keeps the §3.3 no-clone handles (resolution stays a slot
lookup), and the outer Rc is the "possibly from cache" passthrough —
handler-cached completer responses and memoized pipeline outputs flow
through as identical pointers — and the struct hides that storage
behind `iter()`/`len()`, so the render side only ever sees
`(&Row, is_active)`.

**Content side (stays in `NodeEntry` / `ContentCache`)** — "what to
show". The whole pipeline {key, query parse, filter/rank/sort} now sits
together on one side of the cut:

```rust
struct ContentCache {
    slots: Vec<Option<Rc<DataResponse>>>, // today's data_rc
    row_index: HashMap<String, u16>,       // dispatch — unchanged
    active_ids: HashSet<String>,
    slots_dirty: bool,                     // set by refresh_data, cleared by pipeline
    // rows-pipeline memo:
    last_key: String,
    view_rows: Option<Rc<ViewRows>>,
}
```

- `refresh_data` (Phase 1, unchanged) fills slots/row_index/active and
  sets `slots_dirty`. The **rows pipeline** — today's `ensure_view`
  minus cursor/render: flat args → `split_modifiers` +
  `compile_query` → filter → fuzzy rank → sort — runs lazily exactly
  where `ensure_view` runs today (the `None`/`MoveCursor`/`Tab`
  listing arms, restore), iff `slots_dirty || key != last_key`, and
  then clears the flag. Listing arms pass the minted Rc straight to
  `plan(view, req)` — the returned `cursor_row` drives drill/expand,
  the plan drives display.
- The pipeline mints a **fresh `Rc<ViewRows>` on every run**, even if
  *input* content is equal — reconcile cadence must match today's derive
  cadence (dirty/key change ⇒ reconcile), and `ptr_eq` is now that cadence
  signal.
- **Cache key from `&PathArgs` alone: yes, sufficient** —
  `suffix.trim_end()` is exactly today's
  `suffix[..tail.start].trim_end()`, because the pipeline only runs at
  `handle_listing`-reachable frames where the tail is always empty
  (parse consumes every word when no child matches; a matched child
  with a non-empty tail recurses away; bare-id frames have empty
  tails). Restore passes `suffix = restore_to` (already trimmed); the
  drill-in `" "` frame folds into the previous key via `trim_end`, as
  today. And since the memo lives next to the parse that feeds it, no
  key crosses into the view at all — the incoming-Rc `ptr_eq` inside
  `plan` *is* the view's invalidation.
- **Completer dispatch**: on completion frames the view input is
  `wrap(completion_rc)` — `slots = [resp]`, identity order, no active —
  memoized on the completer's Rc ptr so `plan` sees a stable pointer
  while the response is unchanged (no reconcile; cursor survives).
  This is the *only* place
  completion differs: it replaces the pipeline and feeds the same
  `ViewRows` shape; RowView has no completion branch.
- Traversal memory stays in `EpochState`: `view_args` +
  `should_restore_view_args` (§5.1 restore source — same string as
  `last_key` in steady state, but memory, not cache), `confirm_pending`,
  `value_forwarding`, the per-frame completion-`Err` scratch, and
  `error: Option<String>` for `SubmitResult::Error` (set on the failing
  Enter, cleared when the level's prompt section changes). NodeEntry
  still composes the two views (`view`, `completion_view`).

`plan()` internals:

```text
1. if the incoming Rc is ptr-different from the stored last view →
   cursor reconcile (today's seed_or_keep_cursor):
     !user_moved              → re-seed (seed_id → first active → row0)
     user_moved, id in ranked → keep id, update idx to match
     user_moved, id dropped   → clear user_moved, re-seed as above
   Completion degenerates: seed_id None + no active → keep-id-else-row0.
2. if req.pin → cursor_id = pin, resolve index (§5.1 restore — applies
   even with no view change, which is precisely what keeps it cheap:
   memo hit ⇒ same Rc ⇒ no reconcile ⇒ pin alone moves the cursor).
3. if let Some(d) = req.cursor_delta → clamp-move, set user_moved
   (after reconcile — applies to the settled view, today's order).
4. rebuild row lines + markers iff (1) fired or
   `req.render_ctx != last_render_ctx` (derive `PartialEq` on
   `RenderContext`); detail always fresh — rows text depends on
   nothing else.
5. return ViewPlan { cursor_row: view.iter().nth(cursor_idx),
   plan: { table_header, detail, rows: Rc::clone,
           selected: rows.empty ? None : cursor_idx, active_rows } }.
```

**Cache management:**

- Invalidation is entirely pointer/context driven: producer memo
  (`slots_dirty` + key) decides whether to mint a new `Rc<ViewRows>`;
  the incoming-Rc ptr_eq inside `plan` decides cursor reconcile +
  row-line rebuild; a changed `render_ctx` decides row-line rebuild
  alone (today only `term_width` lives there — that's the point of
  passing the full context). The transient `ViewRequest` carries
  none of the first two.
- Handler-cached completer Rc unchanged → wrap memo passes the pointer
  through → no reconcile, cursor and rows Rc survive; cost is plan
  assembly only. (Today `build_render_plan` rebuilds all row lines on
  every non-cursor key; narrowing rebuilds to view-change/render-ctx
  keeps the rows Rc stable across more frames.)
- `reset()` on the epoch gate and on dialog close (view cleared →
  reopened dialog starts at row 0).
- Two `RowView` instances = the two cursors; `MoveCursor`/Tab routing
  by "completion active this frame" is unchanged, and dismissing the
  dialog reveals the listing cursor untouched.

Dialog rendering: a dedicated builder (shared table code; column labels
per response type — listing: `Modifier | Description`, leaf:
`Value | Description`, overridable via `labels` — not the node's
`columns()`); `detail` = `[ERROR]` line? + `header()`; `active_rows` always
empty. Host side: no `repl.rs` render changes — `V25Menu` already paints any
plan, and the loop re-evals every frame (discarded `MoveCursor`/`Tab` results
are fine because the cursors live in the engine, not in the result).

### NodeEntry default completion (listings)

Internal `fn default_complete(&mut self, parsed, cursor_pos) ->
Result<Rc<CompletionRows>, DataError>` — port of v2.0
`Interpreter::field_options` over flat args + merged child rows
(labels default to "Modifier | Description"; in the steps below, `None`
is shorthand for an empty response — gate miss or no matches):

1. `word = word_before(cursor_pos)`; none → `None` (inert).
2. Gate (completer-side, per design): `word.starts_with('.') &&
   !word.starts_with("..")`, else `None`. Non-dot words never produce
   dialog rows at listings.
3. Mode: word contains `:` → **value mode** `(key, value_prefix)`, else
   **key mode** `(key_prefix)` — local split on first `:`, not via `Args`
   (which no longer exists).
4. Key set = `field_space.names(true)` (includes `id`, `description`)
   + injected `sort`; `field_space` = declared `fields()` else inferred
   from current rows (as today).
5. Key mode: candidates = keys where `key.contains(prefix)`; row
   `id = ".{key}:"`, description = existing `field_options` format
   (type / `"<desc>; <order>, <type>"`; `sort` keeps its standing
   description). 
   Value mode:
   - `key == "sort"` → real field names as `.sort:{f}` (contains filter);
   - unknown key → `None` (word stays positional — no dialog);
   - number field → `None` (free-form);
   - string field + `filterable` → **dataset-derived (v1, design D7)**:
     distinct non-empty string values of `key` across the level's
     *full merged row set* (all slots — the dataset, not the current
     filtered view), first-seen order, extracted on demand per call
     (in-memory scan; large sets are fine — the dialog windows rows).
     Fuzzy-match the typed prefix over the values
     (`fuzzy_filter_bonus`: prefix bonus + subsequence; empty prefix →
     every value). Rows `id = ".{key}:{v}"`, `description = "string"`.
     Suggestions only — any value is valid; wherever the value is
     consumed validates it (design §7: completion constrains, accept
     validates).
6. Exactness contract: empty match set, or every match equals the word
   exactly → `None`. Otherwise return the **full** set (exact matches
   included, no highlighting).
7. Rows in field-space order (`sort` last), `Row::new(token).desc(desc)`.

`Err` never occurs for the default (in-memory data).

### Engine dispatch

In `node_traverse`, completion is evaluated in the leaf branch and in
the listing fall-through — i.e. exactly where no child recursion occurs,
so `word_before` always lies in this level's own section (words in a
matched child's tail are completed *at the child*, cursor
suffix-relative). Gated on `key ∈ {None, MoveCursor, Tab}`,
`!confirm_pending`, and `cursor_pos == suffix.len()`:

```text
result = children.is_empty() ? inner.complete_parameter(args, cursor_pos)
                             : default_complete(parsed, cursor_pos)
None / empty → normal path (listing/leaf handling unchanged)
Err(msg)     → per-frame scratch = msg; fall through (detail prepends
               "[ERROR]: msg" this frame; recomputed next frame)
Rows(rc)     → out = completion_view.plan(wrap(rc), ViewRequest {
                 cursor_delta: MoveCursor? d : None,
                 columns from labels, detail: error? + header(), … }):
   None        → Continue(out.plan, full_prompt)
   MoveCursor  → Continue(out.plan, full_prompt)   // delta applied inside
   Tab         → row = out.cursor_row; expanded = suffix
                 with word_before range replaced by row.id;
                 re-run completion against `expanded`
                 (cursor at end of expanded word):
                   empty/Err → expanded += " "   (trailing-space convenience)
                   rows      → keep               (dialog continues)
                 → Continue(plan ignored by host, new full_prompt);
                 next loop iteration re-evals and renders the outcome
```

- `Enter`, `Esc`, `DrillTab`/`DrillEnter` never reach completion —
  surrounding semantics only (Enter submits the raw in-flight word — a
  `SubmitResult::Error` keeps the menu open with `[ERROR]` in detail;
  Esc runs the §5.1 ladder; confirm dialog on `confirm_pending`).
- Completion is fully derived per frame: no session, no anchor, no
  cross-frame state beyond the two `RowView` cursors.
- PageUp/PageDown need no change: the host computes the delta from the
  plan's visible rows — the dialog rows while it is up — and
  `MoveCursor` routes accordingly.

### Testing

- **Flat parse / extraction:** dot words land in `args`; `.sort` vs
  `./x` vs `..` vs `.gitignore` classification (valid key → modifier at
  listings; everything positional at leaves); valid-key filtering/sorting
  byte-identical to today; invalid dot word narrows via fuzzy; Esc ladder
  with dot words present.
- **cursor_pos:** EOL completion works; mid-line suppressed; word in a
  matched child's tail completes at the child, not the root
  (suffix-relative adjustment); convenience 3-arg eval = EOL.
- **Default completion:** key mode (contains, `.sort:` desc, injected
  sort); value mode (string distinct + fuzzy, number inert, unknown key
  inert); exact → no dialog; single non-exact → one row; set with exact
  match → full set; `..`/non-dot gates.
- **Dialog view:** header in detail; `Err` line on top, persists only
  while the completer fails; cursor keep/reset rules (id survives
  narrowing, resets on candidate-set id change and on dialog close);
  MoveCursor routes to dialog then back to listing after dismiss; Rc
  ptr_eq skips re-derive; epoch reset clears both views.
- **Tab:** dir chain (expand → re-run → contents), key→value progression
  (`.so` → `.sort:` → values), final file → trailing space appended by engine;
  Tab with no dialog falls through to normal drill.
- **Bypass:** Enter submits raw prompt at a leaf; Esc clears parameters via
  ladder; `confirm_pending` suppresses completion.
- **Sandbox:** two-path flow (space dismisses arg 1, arg 2 completes),
  DrillEnter arrival (guard/prefill — no empty submit), nonexistent-path
  submit guard; root `/` typing ordinary words never shows a dialog.


## Deferred / later
- **Enum-set override for value completion**: intersect the
  dataset-derived value list with an explicit enum set on `FieldSpec`
  (design §7: the enum set "constrains completion only") — as part of
  the field spec / column spec cleanup. Until then v1 is purely
  dataset-derived.
- **Mid-line completion + mid-prompt file expansion** (future; see the
  sketch's cursor_pos note).
- **Sandbox doc leftovers** (separate doc, non-blocking): mapping node
  `header()` text disagrees with the tree ("Press enter to remove…" vs
  "src/dst/mode/source info"); `prepare()`/`submit()` are written on the
  mapping node but belong to `SandboxRemoveNode` — and `submit()` reads
  `path_args.ids.last()`, which is now `"remove"` (the binding id is
  `ids[1]`); root column labels are swapped vs the data (`id` = host
  path, labels say "Sandbox Path" / "Host Path" the other way around).
  - TODO: Fully rework / flesh out sandbox doc once complete has landed.
