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
  ├─ Phase 2: Esc ladder ─────────────────────────────────────────┤
  │                                                                │
  │  if key == Esc:                                                │
  │    // Ladder steps are checked top-down (design §5.1).         │
  │    // No view state needed — just manipulate prompt.           │
  │    Step 1: frame replay (structural, not Esc-specific)         │
  │    Step 2: clear terms + modifiers                             │
  │    Step 3: strip trailing space                                │
  │    Step 4: clear selection, pop path                           │
  │    Step 5: at root → close                                     │
  │    return NodeResult with new prompt                           │
  │                                                                │
  ├─ Phase 3: leaf check ─────────────────────────────────────────┤
  │                                                                │
  │  if self.children.is_empty() && (key == Tab || key == Enter):  │
  │    // Leaf node: call prepare/submit directly.                 │
  │    // No view state needed.                                    │
  │    result = inner.prepare(epoch, path_args)                    │
  │    return Done/Continue based on SubmitResult                  │
  │                                                                │
  ├─ Phase 4: listing view ───────────────────────────────────────┤
  │                                                                │
  │  // Only needed for listing nodes (has children).              │
  │  // Handles: display, MoveCursor, Tab/Enter completion.        │
  │  // See §3 (view state) for details.                           │
  │  ensure_view(epoch, tail)  // refresh + filter + rank if needed│
  │                                                                │
  ├─ Phase 5: handle key ─────────────────────────────────────────┤
  │                                                                │
  │  match key:                                                    │
  │    MoveCursor(delta) → update cursor, rebuild DisplayState     │
  │    Tab → complete active row's ID, drill into child            │
  │    Enter → same as Tab (drill), or submit if leaf already      │
  │               handled in phase 3                               │
  │    NoKey / other → just display (fall through to phase 6)      │
  │                                                                │
  ├─ Phase 6: build result ───────────────────────────────────────┤
  │                                                                │
  │  Reconstruct full prompt from path + new tail.                 │
  │  Build DisplayState from view state.                           │
  │  Return NodeResult::Continue { prompt, display }               │
  │                                                                │
  └────────────────────────────────────────────────────────────────┘
```

### 2.1 Path walk (phase 0)

Scan one word at a time. No full split. For child lookup, use a
`HashMap<String, usize>` built at construction time.

```
let trailing_space = prompt.ends_with(' ');

let mut path: Vec<String> = Vec::new();
let mut pos = 0;
while pos < prompt.len() {
    // Skip whitespace
    while pos < prompt.len() && prompt.as_bytes()[pos] == b' ' { pos += 1; }
    if pos >= prompt.len() { break; }

    // Find end of word
    let start = pos;
    while pos < prompt.len() && prompt.as_bytes()[pos] != b' ' { pos += 1; }
    let word = &prompt[start..pos];

    // Don't consume the last word
    let mut ahead = pos;
    while ahead < prompt.len() && prompt.as_bytes()[ahead] == b' ' { ahead += 1; }
    if ahead >= prompt.len() { break; }  // last word — stop

    if word.starts_with('.') { break; }
    if let Some(&idx) = self.child_map.get(word) {
        path.push(word.to_string());
        pos = ahead;  // advance past trailing spaces
    } else {
        break;
    }
}

let tail = &prompt[pos..];
```

O(path_depth) time. Tail is a slice of the original prompt (zero-copy).

### 2.2 Esc ladder (phase 2)

Per design §5.1. Each step applies only when the next does not. No view
state needed — just manipulate the prompt string.

```
// Step 1 (structural, design §5.1): valid child ID, no trailing space,
// and saved_filter set → replay. This applies even without Esc.
// Checked BEFORE the Esc-specific steps.
if let Some(sel) = &selection {
    if !trailing_space && self.child_map.contains_key(sel) {
        if let Some(saved) = self.cache.saved_filter.take() {
            // Resume filter state from before the drill.
            let new_prompt = build_prompt(&path, &saved);
            // Re-derive view with the restored filter state.
            let display = self.derive_view(epoch, &saved);
            return NodeResult::Continue { prompt: new_prompt, display };
        }
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

### 2.4 Listing view (phase 4-5)

Only for listing nodes (has children). Handles display, MoveCursor,
Tab/Enter completion. See §3 for view state details.

```
// Tab: complete active row's ID, drill into child
if key == Tab {
    let (active_id, child_idx) = self.view.active_row_with_child();
    let new_tail = format!("{} ", active_id);

    // Save filter state before drill (for Esc restoration)
    self.cache.saved_filter = Some(self.view.current_modifiers());

    // Drill into child
    let full_prompt = build_prompt(&path, &new_tail);
    return self.children[child_idx].node_traverse(epoch, key, full_prompt);
}

// Enter on a listing: same as Tab (drill in)
if key == Enter {
    // Same as Tab — drill into the active row's child
    // (leaf case already handled in phase 3)
}

// MoveCursor: update cursor, rebuild display
if let HostKey::MoveCursor(delta) = key {
    self.view.move_cursor(delta);
}
```

---

## 3. View state (separate from traversal)

The view state handles filter, rank, cursor, and active markers. It is
only relevant for listing nodes (has children) and only when displaying
or completing rows.

### 3.1 `ViewCache` — per-node view state

```rust
struct ViewCache {
    // --- Data (rebuilt when data() returns new Rc) ---
    merged_rows: Vec<Row>,        // flat merged from all children
    row_child: Vec<u16>,          // parallel: row_child[i] = child index for merged_rows[i]
    row_index: HashMap<String, u16>,  // row_id → child_index (for D2, submit)
    field_space: Option<FieldSpace>,

    // --- Derived (rebuilt when inputs change) ---
    ranked_rows: Vec<Row>,        // filtered + ranked for display
    ranked_child: Vec<u16>,       // parallel: child index for each ranked row
    active_rows: Vec<usize>,      // * marker indices into ranked_rows

    // --- Cursor (design §4: "cursor lives in the view") ---
    cursor: usize,                // index into ranked_rows
    cursor_key: Option<(Option<String>, String)>,  // (selection, terms) at cursor set time

    // --- Input tracking ---
    cached_tail: Option<String>,  // tail that produced current ranked_rows
    data_version: u64,            // incremented when data changes
}
```

**Why not store filtered_rows separately?** Filtering (§7.1) and ranking
(fuzzy match) are both O(n) for typical menu sizes (10-100 rows). The
filter loop is negligible. Storing an intermediate filtered_rows adds a
cache field and invalidation logic for no measurable benefit. Re-filter
from merged_rows when any input changes.

**Why not store MergedRow wrapper?** The child_index is carried through
to ranked_child (parallel Vec). No per-row wrapper needed. The HashMap
row_index is for O(1) ID lookups (D2 demotion, submit dispatch fallback).

**Why Vec<u16> for row_child/ranked_child instead of HashMap?** The
cursor position is an index. On Tab/Enter, we need `row_child[cursor]`
— O(1) Vec access, no hash lookup. The HashMap row_index is supplementary
for ID-based lookups (D2 check, frame replay).

### 3.2 Data refresh

Always call data() on each child. Check Rc::ptr_eq to detect change.

```
refresh_data(epoch):
  let mut changed = false;
  let mut rows: Vec<Row> = Vec::new();
  let mut child_idx: Vec<u16> = Vec::new();
  let mut field_space = None;

  for (i, child) in self.children.iter_mut().enumerate() {
    let data = child.inner.data(epoch, &empty_path_args());
    if let Ok(response) = data {
      if prev_sig_not_eq(i, &response) { changed = true; }
      for row in &response.rows {
        rows.push(row.clone());
        child_idx.push(i as u16);
      }
      if field_space.is_none() && !response.fields.is_empty() {
        field_space = Some(FieldSpace::new(...));
      }
    }
  }

  if changed {
    self.view.merged_rows = rows;
    self.view.row_child = child_idx;
    self.view.row_index = rows.iter().zip(child_idx.iter())
        .map(|(r, ci)| (r.id.clone(), *ci))
        .collect();
    self.view.field_space = field_space;
    self.view.data_version += 1;
    // Force full re-derive (clear cached_tail)
    self.view.cached_tail = None;
  }
```

### 3.3 View derivation (filter + rank + cursor)

Only called when displaying a listing. Re-derives from merged_rows when
inputs change.

```
ensure_view(epoch, tail):
  // Skip if nothing changed
  if self.view.cached_tail == Some(tail.to_string()) {
    return;  // reuse cached ranked_rows
  }

  // Step 1: filter (§7.1)
  let modifiers = parse_level(tail, &selection_rule).modifiers;
  let filtered = if modifiers.is_empty() {
    self.view.merged_rows.clone()
  } else {
    let space = self.view.field_space.clone().unwrap_or_default();
    let query = compile_query(&modifiers, &space);
    apply_query(&self.view.merged_rows, &query, &space)
  };

  // Step 2: rank (fuzzy + pin)
  let selection = parse_level(tail, &selection_rule).selection;
  let terms = parse_level(tail, &selection_rule).terms;
  let rank_result = rank_and_pin(&filtered, &terms, selection.as_deref());

  // Carry child_index through ranking
  // (rank_and_pin reorders filtered; we need to track which child each row came from)
  // Build ranked_child by matching ranked rows back to merged_rows
  let ranked_child = rank_result.rows.iter()
      .map(|r| self.view.row_index[&r.id])
      .collect();

  self.view.ranked_rows = rank_result.rows;
  self.view.ranked_child = ranked_child;
  self.view.cached_tail = Some(tail.to_string());

  // Step 3: active markers (design §5.2)
  // Resolve data().active_ids against ranked rows
  self.view.active_rows = resolve_active(&ranked_rows, &active_ids);

  // Step 4: cursor seeding (design §4, §6.5)
  seed_or_keep_cursor(&mut self.view, &selection, &terms);
```

### 3.4 Cursor (design §4: "cursor lives in the view")

The cursor is an index into ranked_rows. It survives re-derives until
selection or terms change (design §6.5: "sticky per-level index set by
Up/Down survives buffer re-derives").

```
seed_or_keep_cursor(view, selection, terms):
  match view.cursor_key {
    Some((ref prev_sel, ref prev_terms))
      if *prev_sel == selection && *prev_terms == terms =>
    {
      // Sticky: keep it, clamped
      view.cursor = view.cursor.min(view.ranked_rows.len().saturating_sub(1));
    }
    _ => {
      // Fresh: seed on selection pin, or first active row, or 0
      view.cursor = selection
          .and_then(|sel| view.ranked_rows.iter().position(|r| r.id == sel))
          .or(view.active_rows.first().copied())
          .unwrap_or(0);
    }
  }
  view.cursor_key = Some((selection, terms));
```

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

struct NodeCache {
    view: ViewCache,
    saved_filter: Option<String>,  // Esc ladder: filter state before last drill
    confirm_pending: bool,         // confirm flow: waiting for yea/nay
}
```

### 4.2 `DisplayState` — transient, not stored

```rust
struct DisplayState {
    rows: Vec<Row>,
    cursor: usize,
    active_rows: Vec<usize>,
    title: String,
    detail: Vec<String>,
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
fn build_render_plan(display: &DisplayState, field_space: &Option<FieldSpace>) -> RenderPlan {
  // Rc-wrap row data for MoveCursor sharing (design §2)
  let rows_rc = Rc::new(display.rows.clone());

  let mut lines = Vec::new();
  lines.push(RenderLine { text: display.title.clone(), ... });

  for d in &display.detail {
    lines.push(RenderLine { text: d.clone(), style: Style::HEADER, is_header: true, ... });
  }

  let row_texts = if let Some(space) = field_space {
    build_table_rows(&rows_rc, space)  // table layout from field metadata
  } else {
    rows_rc.iter().map(|r| format_row(r)).collect()
  };

  for (i, text) in row_texts.iter().enumerate() {
    for (j, sub) in text.split('\n').enumerate() {
      lines.push(RenderLine { text: sub.clone(), ... option: i, is_marker_row: j == 0 });
    }
  }

  RenderPlan {
    title: Some(display.title.clone()),
    lines,
    selected: Some(display.cursor),
    active_rows: display.active_rows.clone(),
    detail: display.detail.clone(),
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
| MergedRow wrapper | None | Parallel Vec<u16> (row_child, ranked_child) carries child_index. No per-row wrapper. |
| Child index lookup | Vec<u16> parallel to rows | Cursor is an index → O(1) Vec access. HashMap row_index supplementary for D2, frame replay. |
| saved_filter (Esc) | Per-node `Option<String>` in NodeCache | One slot, set on drill, taken on replay. Not a stack. |
| saved_filter lives in | `NodeEntry`, not `eval_prompt` | `eval_prompt` is thin — only epoch. |
| Data refresh | Always call data(), Rc::ptr_eq | Design §2: "cheap nothing-changed signals." |
| View re-derive | Only if tail changed | Skip filter+rank if cached_tail matches. |
| MoveCursor | No re-derive, just cursor | Cursor lives in the view (design §4). |
| active_rows | `Vec<usize>` | Design §5.2: multiple rows may be active. |
| Columns | Not in DisplayState | Inferred from field_space at render time. |
| is_leaf | Not in DisplayState | Use `children.is_empty()`. |
| Child lookup | `HashMap<String, usize>` at construction | O(1) per word in path walk. |
| Path walk | Scan one word at a time | No full split, zero-copy tail. |
| Render plan | Standalone function | No `MenuState` dependency. Clean v2.5 path. |
| Frame replay | Structural (phase 2.5), not Esc-only | Design §5.1: "applied even without Esc." |
| Tab/Enter | Clear fuzzy terms, pin selection + space | Design §4 accept semantics. |
