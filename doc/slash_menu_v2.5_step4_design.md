# Step 4 design: `NodeEntry::node_traverse()` internals

Design: `slash_menu_v2.5_design.md`. This document specifies the internal
data structures and algorithm for step 4 of the implementation plan.

---

## 1. Entry point: `SlashDotHandle::eval_prompt(prompt, key) -> MenuResult`

Thin wrapper. Nothing lives here but the epoch (design §2).

```
eval_prompt(prompt, key):
  let result = self.root.node_traverse(self.epoch, key, prompt);
  match result {
    NodeResult::Continue { prompt, display } =>
      MenuResult::Continue(Rc::new(build_render_plan(&display)), prompt),
    NodeResult::Done(msg) => MenuResult::DoneSuccess(msg),
    NodeResult::Error(msg) => MenuResult::DoneError(msg),
  }
```

---

## 2. Traversal algorithm (the core)

This is the structural logic of how keys route through the tree. The
view state (filter, rank, cursor) is a separate concern used only when
displaying a listing or completing a row.

```
node_traverse(epoch, key, prompt):
  ┌─ Phase 0: path walk ──────────────────────────────────────────┐
  │                                                                │
  │  Scan prompt one word at a time.                               │
  │  Try each word (except last) as child ID via child_map.        │
  │  Collect matched IDs into path Vec.                            │
  │  Remaining words = tail for this node.                         │
  │                                                                │
  ├─ Phase 1: refresh data ───────────────────────────────────────┤
  │                                                                │
  │  Always call data() on each child. Check Rc::ptr_eq to see     │
  │  if anything changed. Rebuild merged_rows + row_child if so.   │
  │                                                                │
  │  // At this point: path walk done, data refreshed.                │
  │  // Now follow the design doc's listing node algorithm:          │
  │  //   - Child ID match + non-empty suffix → recurse              │
  │  //   - Child ID match + empty suffix → Esc ladder (phase 2)     │
  │  //   - No child ID match → handle key at this level (phase 3)   │
  │                                                                │
  ├─ Phase 2: child dispatch ──────────────────────────────────────┤
  │                                                                │
  │  // Design doc §5.1: "Empty suffix: handle per §5.1 Esc ladder              │
  │  // (frame restore, filter drop, or parent-level re-entry)."              │
  │                                                                            │
  │  if selection matches a child && no trailing_space:                        │
  │    // Child ID match + empty suffix → Esc ladder                          │
  │    if saved_filter exists:                                                 │
  │      → frame replay: drop the ID from prompt, resume saved filter         │
  │    else:                                                                   │
  │      → filter drop: show child's listing (drill in, display child)        │
  │                                                                            │
  │  if selection matches a child && (non-empty suffix or trailing_space):     │
  │    // Non-empty suffix → recurse. All keys handled deeper.                │
  │    → child.node_traverse(epoch, key, remaining_prompt)                    │
  │                                                                            │
  │  // saved_filter has nothing to do with recursion. It is only about       │
  │  the Esc ladder (frame replay / filter state restoration).
  │                                                                │
  ├─ Phase 3: handle key at this level ────────────────────────────┤
  │                                                                │
  │  // Design doc: "Path exhausted, no child ID match"            │
  │  // Listing nodes:                                             │
  │  //   MoveCursor → return listing Continue                     │
  │  //   Tab → expand focused ID, append space, push frame,       │
  │  //         call child's node_traverse()                       │
  │  //   Enter → as Tab, but forward key; child may submit()      │
  │  //           immediately if it is a leaf                      │
  │                                                                │
  │  match key:                                                    │
  │    MoveCursor → update cursor, rebuild DisplayState            │
  │    Tab → complete active row, drill into child                 │
  │    Enter → same as Tab but forward key to child                │
  │    other → just display (fall through to phase 4)              │
  │                                                                │
  ├─ Phase 4: build result ────────────────────────────────────────┤
  │                                                                │
  │  Reconstruct full prompt from path + new tail.                 │
  │  Build DisplayState from view state.                           │
  │  Return NodeResult::Continue { prompt, display }               │
  │                                                                │
  └────────────────────────────────────────────────────────────────┘
```

### 2.1 Path walk (phase 0)

Scan one word at a time. Interleave child ID lookup with modifier
parsing. Exit as soon as a child ID is found — the child handles the
rest (recursive model, design §2: "The traversal is recursive").

For child lookup, use a `HashMap<String, usize>` built at construction
time.

```
let trailing_space = prompt.ends_with(' ');
let mut path: Vec<String> = Vec::new();
let mut args = Args::default();
let mut pos = 0;

loop {
    // Skip whitespace
    while pos < prompt.len() && prompt.as_bytes()[pos] == b' ' { pos += 1; }
    if pos >= prompt.len() { break; }

    // Find end of word
    let start = pos;
    while pos < prompt.len() && prompt.as_bytes()[pos] != b' ' { pos += 1; }
    let word = &prompt[start..pos];

    // Check if this word is a child ID (design: terminate on first match)
    if let Some(&child_idx) = self.child_map.get(word) {
        // Found child — single match, not a vector.
        // In the recursive model, each node handles one level, so path
        // is a single (id, child_index) pair at most.
        child = Some((word.to_string(), child_idx));
        break;
    }

    // Not a child ID — parse as dot modifier or fuzzy term
    if let Some(dot) = parse_dot_token(word) {
        args.push_modifier(dot);  // accumulates multi-word values
    } else {
        args.push_positional(word);  // fuzzy term or selection
    }
}

// Tail = everything after the last consumed path word (zero-copy slice)
let tail = &prompt[pos..];
```

The loop terminates on the first child ID match. The remaining prompt
(after the matched ID) is the tail for the child. The child's
`node_traverse` will parse its own level from the tail.

If no child matches, the entire remaining prompt is the tail for THIS
node (listing view with fuzzy filter).

### 2.2 Esc ladder (phase 2)

Per design §5.1. Each step applies only when the next does not. No view
state needed — just manipulate the prompt string.

```
// Step 1 (structural, design §5.1): valid child ID, no trailing space,
// and saved_filter set → replay. This applies even without Esc.
// Checked BEFORE the Esc-specific steps.

// Context: the selection was parsed from this node's tail. The cursor
// position is resolved from the selection against the ranked rows
// (see §3.4 cursor seeding). If the selection names a valid child,
// the prompt was: `... parent_id child_id` (no trailing space).
// build_prompt reconstructs: `... parent_id saved_filter_state`.

if let Some(sel) = &selection {
    if !trailing_space && self.child_map.contains_key(sel) {
        if let Some(saved) = self.cache.saved_filter.take() {
            // Frame replay: drop the child ID from the prompt, resume
            // the saved filter state.
            let new_prompt = build_prompt(&path, &saved);
            let display = self.derive_view(epoch, &saved);
            return NodeResult::Continue { prompt: new_prompt, display };
        }
        // No saved filter: show the child's listing without filter
        // state (the child was opened without a prior drill).
    }
}

// Steps 2-4: Esc-specific
if key == Esc {
    // Step 2: clear fuzzy terms + modifiers, keep selection + trailing space
    if selection.is_some() && !terms.is_empty() {
        new_tail = format!("{} ", selection);
        return Continue(reconstruct(path, new_tail));
    }

    // Step 3: strip trailing space (drill out one level)
    if trailing_space {
        new_tail = tail.trim_end().to_string();
        return Continue(reconstruct(path, new_tail));
    }

    // Step 4: clear pinned selection (back to parent)
    if selection.is_some() {
        path.pop();
        return Continue(reconstruct(path, ""));
    }

    // Step 5: at root with nothing to clear → close
    return Done(None);
}
```

### 2.3 Leaf handling (phase 3)

Leaf nodes (no children) handle Tab/Enter directly — no view state
needed. Call `prepare()` which defaults to `submit()`.

```
if self.children.is_empty() && matches!(key, Tab | Enter) {
    let path_args = build_path_args(&path, tail);
    match inner.prepare(epoch, &path_args) {
        SubmitResult::Done(msg) => return NodeResult::Done(msg),
        SubmitResult::Prepared => {
            // Confirm flow: show dialog, wait for next Enter
            self.cache.confirm_pending = true;
            // Fall through to display (show confirm dialog)
        }
        SubmitResult::NewPath(p) => return NodeResult::Continue {
            prompt: p, display: empty_display()
        },
    }
}
```

### 2.4 Key handling at this level (phase 3)

When no child ID matches, handle the key at this level. For listing
nodes, this means display + cursor + Tab/Enter drill.

```
// Ensure view state is fresh (filter + rank from merged_rows)
ensure_view(epoch, tail);

match key {
    HostKey::MoveCursor(delta) => {
        self.view.move_cursor(delta);
    }

    HostKey::Tab => {
        // Design §5: "Tab: bare level → promote (trailing space,
        // same as Enter); otherwise complete cursor row's id."
        // Tab ALWAYS drills — never submits.
        let active_id = self.view.cursor_row_id();
        let child_idx = self.view.row_index[active_id];

        // Save filter state for Esc restoration
        self.cache.saved_filter = Some(self.view.current_filter_state());

        // Drill into child — Tab does NOT forward the key
        let full_prompt = build_prompt(&path, &format!("{} ", active_id));
        return self.children[child_idx]
            .node_traverse(epoch, key, &full_prompt);
    }

    HostKey::Enter => {
        // Design §5: "Enter: as Tab, but forward key; child may
        // submit() immediately if it is a leaf."
        let active_id = self.view.cursor_row_id();
        let child_idx = self.view.row_index[active_id];

        self.cache.saved_filter = Some(self.view.current_filter_state());

        // Drill into child — Enter FORWARDS the key
        let full_prompt = build_prompt(&path, &format!("{} ", active_id));
        return self.children[child_idx]
            .node_traverse(epoch, key, &full_prompt);
        // The child (if leaf) receives Enter and can call submit().
    }

    _ => {} // fall through to build result
}
```

Tab never submits (design §5: "Tab drills"). Enter forwards the key
so a leaf child can receive it and call submit() (design §5: "Enter:
as Tab, but forward key; child may submit() immediately if it is a leaf").

---

## 3. View state (separate from traversal)

The view state handles filter, rank, cursor, and active markers. It is
only relevant for listing nodes (has children) and only when displaying
or completing rows.

### 3.1 `ViewCache` — per-node view state

```rust
struct ViewCache {
    // --- Data (rebuilt when data() returns new Rc) ---
    // Store the raw Rc<DataResponse> per child, not a flat Vec<Row>.
    // This avoids cloning rows until needed for filtering/ranking.
    // The Rc::ptr_eq check detects "nothing changed" cheaply.
    data_rc: Vec<Rc<DataResponse>>,
    row_index: HashMap<String, u16>,  // row_id → child_index (for D2, submit dispatch)

    // --- Derived (rebuilt when inputs change) ---
    // ranked_rows is filtered + ranked. Built lazily from data_rc via
    // iterators when the view is needed (not stored as a flat Vec).
    ranked_rows: Vec<Row>,
    // Active IDs from data().active_ids. Resolved to indices at display
    // time by looking up in ranked_rows.
    active_ids: Vec<String>,     // design §5.2: Vec, may have multiple

    // --- Cursor (design §4: "cursor lives in the view") ---
    cursor_id: Option<String>,   // row ID — survives re-rank/filter
    // Cache the cursor offset for performance. Lists can be long
    // (e.g. /tree 1000+ lines); cursoring through those is O(n)
    // per MoveCursor without caching. Updated on cursor_id change
    // or re-rank.
    cursor_idx: usize,
}
```

**field_space**: In v2.5, field metadata is declared on the parent node
(design §2.1: "Field metadata on the parent: declared once; children
conform"). It is NOT stored in ViewCache — it is a property of the
`NodeEntry` itself (or derived from children's `DataResponse.fields`).
Filter/sort queries use it via the node's own method.

**active_rows in RenderPlan**: The design doc says
`RenderPlan.active_rows: Option<Vec<usize>>` (offsets). So we resolve
active_ids → indices at display time. RenderPlan also needs the cursor
offset, which is `cursor_idx` (cached).

**Why row_id (String) for cursor instead of index?** The cursor should
travel with the row across re-rank/filter. An index becomes stale when
rows are reordered. A row ID is stable. On display, look up the ID in
ranked_rows to get the index (O(n) but n is small). On MoveCursor, move
the index and update the ID.

**Why also cache cursor_idx?** Lists can be long (e.g. /tree 1000+
lines). Cursoring through those is O(n) per MoveCursor if we always
look up cursor_id in ranked_rows. Caching the offset makes MoveCursor
O(1). Updated when cursor_id changes or ranked_rows is rebuilt.

**Why Vec<String> for active_rows?** IDs don't change across filtering
or sorting. Indices do. The `*` marker resolves from data().active_ids
(Which are strings) against ranked rows at display time.

**Why no row_child Vec?** The HashMap `row_index` already maps
row_id → child_index in O(1). No need for a parallel Vec. On Tab/Enter,
look up `row_index[cursor_row_id]`.

**Why no field_space in ViewCache?** In v2.5, field metadata is declared
on the parent node (design §2.1: "Field metadata on the parent: declared
once; children conform"). It is a property of the `NodeEntry` itself
(or inferred from children's `DataResponse.fields` at refresh time).
Filter/sort queries use it via the node's own method — not stored
redundantly in ViewCache.

**Why no data_version?** Redundant. Change detection uses
`data_sig: Vec<Rc<DataResponse>>` — compare Rc::ptr_eq per child to
detect "nothing changed" (design §2).

**Why view_args?** The full parsed arguments (modifiers + terms +
selection) that produced the current ranked_rows. When the view_args
change, re-derive. When they don't, reuse cached ranked_rows. This is
the single cache key for the entire filter+rank pipeline. Named
`view_args` to avoid confusion with "tail" in `PathArgs`, which means
the *unconsumed* part of the prompt handled by child nodes.

**Why not store filtered_rows separately?** Filtering (§7.1) and ranking
(fuzzy match) are both O(n) for typical menu sizes (10-100 rows). The
filter loop is negligible. Storing an intermediate filtered_rows adds a
cache field and invalidation logic for no measurable benefit. Re-filter
from merged_rows when any input changes.



### 3.2 Data refresh

Always call data() on each child. Check Rc::ptr_eq to detect change.

```
refresh_data(epoch):
  let mut changed = false;
  let mut new_data: Vec<Rc<DataResponse>> = Vec::new();

  for (i, child) in self.children.iter_mut().enumerate() {
    let data = child.inner.data(epoch, &empty_path_args());
    if let Ok(response) = data {
      // Rc::ptr_eq check — no cloning of rows
      if prev_sig_not_eq(i, &response) { changed = true; }
      new_data.push(response);
    }
  }

  if changed {
    self.view.data_rc = new_data;
    // Build row_index lazily: iterate over data_rc, not a flat cloned Vec
    self.view.row_index = self.view.data_rc.iter().enumerate()
        .flat_map(|(child_idx, resp)| {
            resp.rows.iter().map(move |r| (r.id.clone(), child_idx as u16))
        })
        .collect();
    // field_space comes from the parent node (design §2.1), not from
    // children's data. See §3.1 "field_space" note.
    self.view.view_args = None;  // force re-derive
  }
```

### 3.3 View derivation (filter + rank + cursor)

Only called when displaying a listing. Re-derives from merged_rows when
inputs change.

```
ensure_view(epoch, view_args):
  // Skip if nothing changed
  if self.view.view_args == Some(view_args.to_string()) {
    return;  // reuse cached ranked_rows
  }

  // Step 1: filter (§7.1) — iterate over raw Rc's, no flat clone
  let modifiers = parse_level(view_args, &selection_rule).modifiers;
  let filtered: Vec<Row> = if modifiers.is_empty() {
    self.view.data_rc.iter().flat_map(|rc| rc.rows.iter().cloned()).collect()
  } else {
    let space = self.parent_field_space();
    let query = compile_query(&modifiers, &space);
    let all_rows: Vec<Row> = self.view.data_rc.iter().flat_map(|rc| rc.rows.iter().cloned()).collect();
    apply_query(&all_rows, &query, &space)
  };

  // Step 2: rank (fuzzy + pin)
  let selection = parse_level(view_args, &selection_rule).selection;
  let terms = parse_level(view_args, &selection_rule).terms;
  let rank_result = rank_and_pin(&filtered, &terms, selection.as_deref());

  self.view.ranked_rows = rank_result.rows;
  self.view.view_args = Some(view_args.to_string());

  // Step 3: active markers (design §5.2)
  // active_ids already stored from data() responses
  // (resolved to indices at display time)

  // Step 4: cursor seeding (design §4, §6.5)
  seed_or_keep_cursor(&mut self.view, &selection, &terms);
```

### 3.4 Cursor (design §4: "cursor lives in the view")

The cursor is a row ID, not an index. It survives re-rank and re-filter
because the row ID stays with the row regardless of order. On display,
look up the ID in ranked_rows to get the index.

```
seed_or_keep_cursor(view, selection, terms):
  // If cursor_id is still valid in the current ranked_rows, keep it.
  // Otherwise, re-seed.
  if let Some(ref id) = view.cursor_id {
      if ranked_rows.iter().any(|r| &r.id == id) {
          return;  // cursor still valid, keep it
      }
  }

  // Re-seed: selection pin, or first active row, or row 0
  view.cursor_id = selection
      .or_else(|| view.active_rows.first().cloned())
      .or_else(|| ranked_rows.first().map(|r| r.id.clone()));
```

MoveCursor updates both the index (for display) and the ID (for
persistence across re-derives).

### 3.5 Active markers (design §5.2)

`data().active_ids: Vec<String>` resolved against ranked rows. Multiple
rows may be active (e.g., multiple models currently in use). The `*`
marker shows on active rows; `>` on the cursor row; `*` wins when both
are on the same row.

---

## 4. Internal structures

### 4.1 `NodeEntry`

```rust
struct NodeEntry {
    inner: Box<dyn SlashDotNode>,
    children: Vec<NodeEntry>,
    child_map: HashMap<String, usize>,  // child_id → index in children
    cache: NodeCache,
}

// child_map is derived from children at construction time. NOT
// invalidated with the rest of the cache — children don't change
// after construction (design §2.1: "Ordering: addChild() order").
// It is a structural index, not a derived cache.

struct NodeCache {
    view: ViewCache,
    saved_filter: Option<String>,  // Esc ladder: filter state before last drill
    confirm_pending: bool,         // confirm flow: waiting for yea/nay
}
```

### 4.2 `DisplayState` — transient, not stored

DisplayState is the interface between node_traverse and the render plan
builder. It bundles what the render plan needs from the traversal:
rows, cursor position, active markers, title, and header text.

```rust
struct DisplayState {
    rows: Vec<Row>,
    cursor_idx: usize,            // looked up from cursor_id in ranked_rows
    active_rows: Vec<usize>,      // resolved active indices (design §5.2)
    title: String,
    header_lines: Vec<String>,    // header() output (design §2: "Non-selectable
                                  //   header lines, replaces v2 DetailProvider")
}
```

Built from ViewCache after derivation. Passed to render plan builder.
Columns are NOT here — inferred from field_space at render time.
is_leaf is NOT here — use `children.is_empty()`.

### 4.3 `NodeResult` (internal)

```rust
enum NodeResult {
    Continue { prompt: String, display: DisplayState },
    Done(Option<String>),
    Error(String),
}
```

---

## 5. Render plan construction

Clean new path, independent of old `PlanBuilder`/`MenuState`.

```
fn build_render_plan(display: &DisplayState, field_space: &Option<FieldSpace>, term_width: usize) -> RenderPlan {
  // Rc-wrap row data for MoveCursor sharing (design §2)
  let rows_rc = Rc::new(display.rows.clone());

  let mut lines = Vec::new();
  // Design §6 panel structure order:
  //   1. header() output (help text, stats, previews)
  //   2. table header line (if child rows have columns)
  //   3. selectable rows with > (cursor) / * (active) markers
  for h in &display.header_lines {
    lines.push(RenderLine { text: h.clone(), style: Style::HEADER, is_header: true, ... });
  }

  // Table width: handled in the render plan builder. The host passes
  // term_width (from its terminal dimensions). The engine truncates
  // headers and wraps table columns to fit. Design §6: "Headers
  // truncated to fit terminal."
  let row_texts = if let Some(space) = field_space {
    build_table_rows(&rows_rc, space, term_width)
  } else {
    rows_rc.iter().map(|r| format_row(r)).collect()
  };

  for (i, text) in row_texts.iter().enumerate() {
    for (j, sub) in text.split('\n').enumerate() {
      lines.push(RenderLine { text: sub.clone(), ... option: i, is_marker_row: j == 0 });
    }
  }

  RenderPlan {
    lines,
    selected: Some(display.cursor_idx),
    active_rows: display.active_rows.clone(),
  }
}
```

---

## 6. Borrow discipline

Per design §2: "Parent-state access happens only outside the child loop."

1. **Own state** (phases 0-2): `self.cache` and `self.view` accessed. No child borrows.
2. **Descend** (phase 4-5 Tab/Enter): `self.children[idx].node_traverse()`. One child at a time.
3. **Post-processing** (phase 6): build from cache. No child borrows.

---

## 7. Implementation order

### 4a. Traversal skeleton
- `NodeEntry`, `NodeCache`, `child_map`
- Path walk (phase 0)
- Esc ladder (phase 2) — prompt manipulation only, no view
- Leaf handling (phase 3) — stub prepare/submit
- Tab drill (phase 4-5) — stub view, just dispatch to child
- `NodeResult`, `eval_prompt` wiring

### 4b. View state
- `ViewCache` with merged_rows, row_child, row_index
- Data refresh (Rc::ptr_eq change detection)
- Filter + rank (§7.1 + cursor::rank_and_pin)
- Cursor seeding (design §6.5)
- Active markers (design §5.2)

### 4c. Render plan
- `DisplayState`
- `build_render_plan()` standalone function
- Table layout from field_space

### 4d. MoveCursor
- Update cursor, rebuild DisplayState
- Test end-to-end

### 4e. Tab completion + drill
- Complete active row's ID, save saved_filter, recurse into child

### 4f. Esc + frame replay
- §5.1 ladder steps 2-4
- Phase 2.5 frame replay (structural, applies without Esc)
- Multi-level test scenarios

### 4g. Tests
- Port interpreter tests to new harness

---

## 8. Key design decisions

| Decision | Choice | Rationale |
|---|---|---|
| Traversal vs view | Separate concerns | Traversal is structural (path walk, key dispatch, Esc). View is data (filter, rank, cursor). View only used for listing display. |
| filtered_rows cache | None | Re-filter from merged_rows when inputs change. O(n) loop is negligible for typical menu sizes. |
| Cursor identifier | Row ID (String), not index | Survives re-rank/re-filter. Index looked up at display time. |
| active_rows | `Vec<String>` (IDs) | Design §5.2: IDs don't change across filter/sort. Resolved to indices at display time. |
| Child index lookup | `HashMap<String, u16>` (row_index) | O(1) by row ID. Independent of row reordering. No parallel Vec needed. |
| saved_filter (Esc) | Per-node `Option<String>` in NodeCache | One slot, set on drill, taken on replay. Not a stack. |
| saved_filter lives in | `NodeEntry`, not `eval_prompt` | `eval_prompt` is thin — only epoch. |
| Data refresh | Always call data(), Rc::ptr_eq | Design §2: "cheap nothing-changed signals." |
| View re-derive | Only if tail changed | Skip filter+rank if cached_tail matches. |
| MoveCursor | No re-derive, just cursor | Cursor lives in the view (design §4). |
| Columns | Not in DisplayState | Inferred from field_space at render time. |
| is_leaf | Not in DisplayState | Use `children.is_empty()`. |
| Child lookup | `HashMap<String, usize>` at construction | O(1) per word in path walk. |
| Path walk | Scan one word, exit on first child ID | Recursive model: each node handles one level. Interleaves modifier parsing. |
| Render plan | Standalone function | No `MenuState` dependency. Clean v2.5 path. |
| Frame replay | Structural (phase 2), not Esc-only | Design §5.1: "applied even without Esc." |
| Tab | Drills, never submits | Design §5: "Tab: bare level → promote; otherwise complete." |
| Enter | Drills + forwards key | Design §5: "Enter: as Tab, but forward key; child may submit()." |
