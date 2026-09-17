# Step 4 design: `NodeEntry::node_traverse()` internals

Design: `slash_menu_v2.5_design.md`. This document specifies the internal
data structures and algorithm for step 4 of the implementation plan.

---

## 1. Entry point: `SlashDotHandle::eval_prompt(prompt, key) -> MenuResult`

Thin wrapper. Nothing lives here but the epoch (design §2: "The host gets
a SlashDotHandle from SlashDotMenu::get_handle() (bumps epoch)").

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

No frame stack, no path walking. `node_traverse` does everything.

---

## 2. Per-node traversal: `NodeEntry::node_traverse(epoch, key, prompt)`

Each `NodeEntry` handles one level. The prompt arrives as the **full
prompt text**. Path walking happens inside.

### Return type (internal)

```rust
enum NodeResult {
    Continue { prompt: String, display: DisplayState },
    Done(Option<String>),
    Error(String),
}
```

### Algorithm

```
node_traverse(epoch, key, prompt):
  ┌─ Phase 0: path walk ──────────────────────────────────────────┐
  │                                                                │
  │  Scan prompt one word at a time (no full split).               │
  │  Walk children: try each word (except last) as child ID.       │
  │  Remaining words = tail for this node.                         │
  │                                                                │
  ├─ Phase 1: refresh data ───────────────────────────────────────┤
  │                                                                │
  │  // Always call data() on children (cheap Rc::ptr_eq check).   │
  │  // Design §2: "Return Rc<DataResponse> so the parent can      │
  │  // check Rc::ptr_eq for cheap 'nothing changed' signals."     │
  │  refresh_data(epoch)  // calls data() on each child            │
  │                                                                │
  ├─ Phase 2: derive local state ─────────────────────────────────┤
  │                                                                │
  │  // Only if data changed (Rc !ptr_eq) or tail changed:         │
  │  //   1. Apply §7.1 field filter (modifiers from tail)         │
  │  //   2. Rank by fuzzy terms + pin selection                   │
  │  //   3. Seed or keep cursor                                   │
  │  //   4. Resolve active markers (* rows)                       │
  │  // Otherwise: reuse cached state.                             │
  │                                                                │
  ├─ Phase 2.5: frame replay (structural check) ──────────────────┤
  │                                                                │
  │  // Design §5.1 step 1: "applied even without Esc."            │
  │  // When a child ID is valid with no trailing space, and       │
  │  // saved_filter is set, replay it.                            │
  │                                                                │
  ├─ Phase 3: handle key ─────────────────────────────────────────┤
  │                                                                │
  │  match key:                                                    │
  │    MoveCursor → update cursor (no re-derive)                   │
  │    Tab/Enter  → complete, drill or submit                      │
  │    Esc        → §5.1 ladder steps 2–4                          │
  │                                                                │
  ├─ Phase 4: build result ───────────────────────────────────────┤
  │                                                                │
  │  Reconstruct full prompt from path + new tail.                 │
  │  Build DisplayState from cache.                                │
  │  Return NodeResult::Continue { prompt, display }               │
  │                                                                │
  └────────────────────────────────────────────────────────────────┘
```

---

## 3. Internal data structures

### 3.1 `NodeCache` — per-node ephemeral state

```rust
struct NodeCache {
    // --- Data cache (design §2: "derived data cache") ---
    last_epoch: u64,
    /// Rc from the last data() merge. Checked with ptr_eq to detect
    /// "nothing changed" signals (design §2).
    data_sig: Vec<std::rc::Rc<DataResponse>>,
    /// Merged rows with child index for submit dispatch (design §2.1:
    /// "Internal to the traversal body, not in Row API").
    merged_rows: Vec<MergedRow>,
    /// Field metadata for §7.1 filter/sort (design §2.1: "Field metadata
    /// on the parent: declared once; children conform").
    field_space: Option<FieldSpace>,

    // --- Derived state (recomputed only when inputs change) ---
    /// Filtered rows after §7.1 field filter. Recomputed when modifiers
    /// change. Input: merged_rows + modifiers.
    filtered_rows: Vec<Row>,
    /// Ranked rows after fuzzy match + pin. Recomputed when terms or
    /// selection change. Input: filtered_rows + terms + selection.
    ranked_rows: Vec<Row>,
    /// Active markers: resolved from data().active_ids against ranked
    /// rows (design §5.2). Vec because multiple rows may be active.
    active_rows: Vec<usize>,
    /// The modifiers string that produced the current filtered_rows.
    /// Used to detect when filter needs re-derivation.
    cached_modifiers: Option<String>,
    /// The (terms, selection) that produced the current ranked_rows.
    /// Used to detect when ranking needs re-derivation.
    cached_rank_key: Option<(String, Option<String>)>,

    // --- Cursor (design §4: "cursor lives in the view") ---
    /// View cursor: index into ranked_rows. Moved by arrow keys.
    /// Survives re-derives until selection/terms change (design §6.5).
    cursor: usize,
    /// The (selection, terms) at the time the cursor was last set.
    /// When these change, cursor re-seeds on the derived pin/active row.
    cursor_key: Option<(Option<String>, String)>,

    // --- Esc ladder (design §5.1) ---
    /// The modifiers string before the last drill. Set on every drill
    /// (Tab/Enter into a child). Taken on frame replay (phase 2.5) or
    /// Esc step 2. Named "saved_filter" to avoid confusion with
    /// "traversal frames" in the design doc — this is per-node, not
    /// a stack.
    saved_filter: Option<String>,
}
```

**What changes when:**

| Input change | What to re-derive |
|---|---|
| `MoveCursor` | Nothing — just update cursor index, rebuild render plan from cached ranked_rows |
| Tail modifiers changed | Filter (§7.1) → recompute filtered_rows |
| Tail terms/selection changed | Rank (fuzzy + pin) → recompute ranked_rows |
| Epoch changed or data() Rc changed | Full refresh: re-fetch children, re-filter, re-rank |

### 3.2 `MergedRow` — child index tagging

```rust
struct MergedRow {
    row: Row,
    child_index: u16,  // which child produced this row (for submit dispatch)
}
```

Design §2.1: "parent tags each row with its owning child index during
merge (internal child_index: u16); Tab/Enter reads the tag and dispatches.
Internal to the traversal body, not in Row API."

For fast ID→child_index lookup, build a `HashMap<String, u16>` at refresh
time from the merged rows.

### 3.3 `DisplayState` — computed view (transient, not stored)

```rust
struct DisplayState {
    rows: Vec<Row>,            // ranked rows for display
    cursor: usize,             // view-cursor index into rows
    active_rows: Vec<usize>,   // * marker indices (design §5.2: Vec)
    title: String,             // from node's header() or child name
    detail: Vec<String>,       // from header() (design §2: "Non-selectable
                               //   header lines, replaces v2 DetailProvider")
}
```

Columns are NOT in DisplayState. The render plan builder infers columns
from field metadata (field_space) at render time — they're a presentation
concern, not a traversal concern.

is_leaf is NOT in DisplayState. Determined by `self.children.is_empty()`.

Built from NodeCache after deriving local state. Passed to render plan
builder. **Not stored** — recomputed per key event.

### 3.4 RenderPlan: Rc wrapping (design §2)

"RenderPlan wraps bulk row data in Rc so cursor movement shares unchanged
content — only marker positions (selected_row, active_rows) re-allocate."

The render plan builder should Rc-wrap the row data so MoveCursor (which
only changes cursor position) shares the same row content.

---

## 4. Phase 0: path walk

Scan one word at a time. No full split. For child lookup, use a
`HashMap<String, usize>` built at construction time (add_child updates it).

```
let trailing_space = prompt.ends_with(' ');

let mut path: Vec<String> = Vec::new();
let mut byte_pos = 0;
// Walk words, trying each (except last) as child ID
while byte_pos < prompt.len() {
    // Skip whitespace
    while byte_pos < prompt.len() && prompt[byte_pos..].starts_with(' ') {
        byte_pos += 1;
    }
    if byte_pos >= prompt.len() { break; }

    // Find end of word
    let word_start = byte_pos;
    while byte_pos < prompt.len() && prompt[byte_pos..] != " " {
        byte_pos += 1;
    }
    let word = &prompt[word_start..byte_pos];

    // Don't consume the last word (design: "never consuming the last word")
    // Check if there's more after this word
    let after_word = byte_pos;
    while after_word < prompt.len() && prompt[after_word..].starts_with(' ') {
        after_word += 1;
    }
    if after_word >= prompt.len() {
        break; // this is the last word — don't consume
    }

    if word.starts_with('.') { break; }
    if let Some(&idx) = self.child_map.get(word) {
        path.push(word.to_string());
        byte_pos = after_word; // skip past trailing space
    } else {
        break;
    }
}

// Tail = everything after the last consumed path word
let tail = &prompt[byte_pos..];
```

This scans in O(path_depth) time with no allocation for the path walk.
The tail is a slice of the original prompt (zero-copy).

### Child index map

Built at construction time, updated on add_child:

```rust
struct NodeEntry {
    inner: Box<dyn SlashDotNode>,
    children: Vec<NodeEntry>,
    child_map: HashMap<String, usize>,  // child_id → index in children
    cache: NodeCache,
}
```

---

## 5. Phase 1: refresh data

Always call data() on each child. Check Rc::ptr_eq to detect "nothing
changed" (design §2: "Return Rc<DataResponse> so the parent can check
Rc::ptr_eq for cheap 'nothing changed' signals").

```
refresh_data(epoch):
  self.cache.last_epoch = epoch;

  let mut changed = false;
  let mut all_rows: Vec<MergedRow> = Vec::new();
  let mut field_space: Option<FieldSpace> = None;
  let mut new_sigs: Vec<Rc<DataResponse>> = Vec::new();

  for (i, child) in self.children.iter_mut().enumerate() {
    let path_args = empty_path_args(); // children get empty args for data()
    let data = child.inner.data(epoch, &path_args);
    if let Ok(response) = data {
      // Check if data actually changed (design §2: Rc::ptr_eq)
      let prev_sig = self.cache.data_sig.get(i);
      if prev_sig.map(|p| !Rc::ptr_eq(p, &response)).unwrap_or(true) {
        changed = true;
      }
      new_sigs.push(response.clone());
      for row in &response.rows {
        all_rows.push(MergedRow { row: row.clone(), child_index: i as u16 });
      }
      // Merge field metadata from first child that provides it
      if field_space.is_none() && !response.fields.is_empty() {
        field_space = Some(FieldSpace::new(
          response.fields.iter().map(|m| m.spec.clone()).collect()
        ));
      }
    }
  }

  if field_space.is_none() {
    field_space = infer_field_space(&all_rows);
  }

  self.cache.data_sig = new_sigs;
  self.cache.merged_rows = all_rows;
  self.cache.field_space = field_space;

  // Build ID→child_index lookup map
  self.cache.row_index = self.cache.merged_rows.iter()
      .enumerate()
      .map(|(i, mr)| (mr.row.id.clone(), mr.child_index))
      .collect();

  changed  // caller uses this to decide whether to re-derive
```

### §7.1 field filter (only if inputs changed)

```
apply_field_filter(tail):
  let modifiers = parse_level(tail, &selection_rule).modifiers;
  let modifiers_key = serialize_modifiers(&modifiers); // for cache comparison

  if modifiers_key == self.cache.cached_modifiers {
    return; // no change, reuse cached filtered_rows
  }

  if modifiers.is_empty() {
    self.cache.filtered_rows = self.cache.merged_rows.iter()
        .map(|mr| mr.row.clone()).collect();
  } else {
    let space = self.cache.field_space.clone().unwrap_or_default();
    let query = compile_query(&modifiers, &space);
    let rows: Vec<Row> = self.cache.merged_rows.iter()
        .map(|mr| mr.row.clone()).collect();
    self.cache.filtered_rows = apply_query(&rows, &query, &space);
  }

  self.cache.cached_modifiers = Some(modifiers_key);
  // Force re-rank (terms/selection may apply differently to new filtered set)
  self.cache.cached_rank_key = None;
```

---

## 6. Phase 2: derive local state (only if inputs changed)

### 6.1 Rank (only if terms/selection changed)

```
rank(terms, selection):
  let rank_key = (terms.clone(), selection.clone());
  if self.cache.cached_rank_key == Some(rank_key.clone()) {
    return; // no change, reuse cached ranked_rows
  }

  let rank_result = rank_and_pin(
    &self.cache.filtered_rows, &terms, selection.as_deref());
  self.cache.ranked_rows = rank_result.rows;
  self.cache.cached_rank_key = Some(rank_key);
```

### 6.2 Cursor seeding (design §4, §6.5)

```
seed_or_keep_cursor(selection, terms):
  match self.cache.cursor_key {
    Some((ref prev_sel, ref prev_terms))
      if *prev_sel == selection && *prev_terms == terms =>
    {
      // Sticky: keep it, clamped
      let len = self.cache.ranked_rows.len();
      self.cache.cursor = self.cache.cursor.min(len.saturating_sub(1));
    }
    _ => {
      // Fresh: seed on selection pin, or first active row, or 0
      let active = self.cache.active_rows.first().copied();
      self.cache.cursor = selection
          .and_then(|_| self.cache.ranked_rows.iter()
              .position(|r| Some(&r.id) == selection.as_ref()))
          .or(active)
          .unwrap_or(0);
    }
  }
  self.cache.cursor_key = Some((selection, terms));
```

### 6.3 Active markers (design §5.2)

Resolves `data().active_ids: Vec<String>` against ranked rows. Multiple
rows may be active (e.g., multiple models currently in use).

```
resolve_active_rows(active_ids: &[String]):
  self.cache.active_rows = active_ids.iter()
      .filter_map(|id| self.cache.ranked_rows.iter().position(|r| &r.id == id))
      .collect();
```

---

## 7. Phase 2.5: frame replay (structural check)

Design §5.1 step 1: "Valid child ID, no trailing space → replay local
frame (restore previous prompt + filter); drop trailing ID from prompt
and match cursor. **This step is applied even without Esc**, so even when
restoring from prompt."

```
if let Some(sel) = &selection {
    if !trailing_space && self.child_map.contains_key(sel) {
        if let Some(saved) = self.cache.saved_filter.take() {
            // Resume filter state from before the drill.
            // Re-parse saved filter, re-derive display.
            let restored_display = self.derive_display(epoch, &saved);
            let new_prompt = build_prompt(&path, &saved);
            return NodeResult::Continue { prompt: new_prompt, display: restored_display };
        }
        // No saved filter: normal — fall through to show child's listing
        // (or Phase 3 Tab/Enter drill).
    }
}
```

When there IS a saved_filter and the selection matches a child without
trailing space, we restore the filter state. When there's NO saved
filter, we show the child's listing normally.

---

## 8. Phase 3: key handling

### 8.1 `MoveCursor(delta)`

```
let len = ranked_rows.len();
cursor = (cursor as isize + delta).clamp(0, len - 1) as usize;
// No re-derive. Rebuild RenderPlan from cached ranked_rows.
```

### 8.2 `Tab`

```
let active_row = ranked_rows[cursor];
let active_id = active_row.id;

// Accept semantics (design §4): fuzzy terms cleared, selection pinned.
let new_tail = format!("{} ", active_id);

// Save filter state before drill (for Esc restoration)
self.cache.saved_filter = Some(serialize_modifiers(&current_modifiers));

// If listing (has children):
//   Look up active_id in row_index → child_index.
//   Reconstruct full prompt: path + active_id + " "
//   Recurse into children[child_index].node_traverse(epoch, key, full_prompt)
//   Return child's NodeResult.

// If leaf (no children):
//   Call inner.prepare(epoch, path_args)
//   Return Done/Prepared based on SubmitResult.
```

### 8.3 `Enter`

Same as Tab, except at leaf level Enter submits directly (one key press).

### 8.4 `Esc` — §5.1 ladder steps 2–4

Step 1 (frame replay) is handled structurally in Phase 2.5. Esc handles
the remaining steps:

```
// Step 2: clear fuzzy terms + modifiers, keep selection + trailing space
if selection.is_some() && !terms.is_empty():
    new_tail = format!("{} ", selection);
    return Continue(reconstruct(path, new_tail));

// Step 3: strip trailing space (drill out)
if trailing_space:
    new_tail = tail.trim_end().to_string();
    return Continue(reconstruct(path, new_tail));

// Step 4: clear pinned selection (back to parent)
if selection.is_some():
    path.pop();
    return Continue(reconstruct(path, ""));

// Step 5: at root with nothing to clear → close
return Done(None);
```

---

## 9. Render plan construction

Clean new path, independent of old `PlanBuilder`/`MenuState`.

Columns are inferred from field_space at render time (not stored in
DisplayState). The render plan builder uses the table module for
multi-column layouts when field metadata is available.

```
fn build_render_plan(display: &DisplayState, field_space: &Option<FieldSpace>) -> RenderPlan {
  // Rc-wrap row data so MoveCursor shares unchanged content
  // (design §2: "RenderPlan wraps bulk row data in Rc")
  let rows_rc = Rc::new(display.rows.clone());

  let mut lines = Vec::new();

  // Title
  lines.push(RenderLine { text: display.title.clone(), ... });

  // Detail / header lines (from header())
  for d in &display.detail {
    lines.push(RenderLine { text: d.clone(), style: Style::HEADER, is_header: true, ... });
  }

  // Candidate rows: table layout if field metadata available, else simple
  let row_texts = if let Some(space) = field_space {
    build_table_rows(&rows_rc, space)
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

## 10. Borrow discipline

Per design §2: "Parent-state access happens only outside the child loop
(three phases: own-state → descend → post-processing). Cross-level data
flow is passed down as arguments, never read sideways."

1. **Own state** (Phase 1+2): `self.cache` accessed. No child borrows.
2. **Descend** (Phase 3): `self.children[idx].node_traverse()`. One child
   borrowed mutably at a time.
3. **Post-processing** (Phase 4): build from `self.cache`. No child borrows.

---

## 11. Implementation order

### 4a. Internal types + render plan
- `NodeCache`, `DisplayState`, `MergedRow`
- `build_render_plan()` standalone function
- Child index map (HashMap)
- Stub `node_traverse`

### 4b. Data refresh (Phase 1)
- `refresh_data()`: call data() on children, check Rc::ptr_eq
- Build merged_rows + row_index
- Field space inference from first child's metadata

### 4c. Filter + rank + cursor (Phase 2)
- §7.1 field filter (only if modifiers changed)
- rank_and_pin (only if terms/selection changed)
- Cursor seeding (sticky cursor, design §6.5)
- Active markers (Vec<usize>, design §5.2)

### 4d. MoveCursor
- Update cursor index, rebuild RenderPlan
- Test end-to-end render plan output

### 4e. Tab + Enter
- Complete, drill, saved_filter set, leaf submit
- Child dispatch via row_index HashMap

### 4f. Esc + frame replay
- §5.1 ladder steps 2–4
- Phase 2.5 frame replay (structural)
- Multi-level test scenarios

### 4g. `eval_prompt` wiring
- Thin wrapper calling `root.node_traverse`
- Convert NodeResult to MenuResult

### 4h. Tests
- Port ~50 interpreter tests to new harness

---

## 12. Migration strategy

- `build_render_plan()` takes `DisplayState` + `FieldSpace`, returns `RenderPlan`.
  No `MenuState` dependency.
- Old `render.rs` / `PlanBuilder` untouched.
- Deleted when all commands migrated (step 6).

---

## 13. Key design decisions

| Decision | Choice | Rationale |
|---|---|---|
| saved_filter (Esc) | Per-node `Option<String>` in NodeCache | One slot, set on drill, taken on replay |
| saved_filter lives in | `NodeEntry`, not `eval_prompt` | `eval_prompt` is thin — only epoch |
| Data refresh | Always call data(), Rc::ptr_eq for change detection | Design §2: "cheap nothing-changed signals" |
| Filter re-derive | Only if modifiers changed | Avoid redundant filter application |
| Rank re-derive | Only if terms/selection changed | Avoid redundant fuzzy matching |
| MoveCursor | No re-derive, just cursor + render plan | Cursor lives in the view (design §4) |
| active_rows | `Vec<usize>` | Design §5.2: multiple rows may be active |
| Columns | Not in DisplayState, inferred at render time | Presentation concern, not traversal |
| is_leaf | Not in DisplayState, use `children.is_empty()` | Structural property, not display |
| Child lookup | `HashMap<String, usize>` at construction | O(1) per word, no linear scan |
| Path walk | Scan one word at a time, no full split | Efficient, no allocation |
| Trailing space | `prompt.ends_with(' ')` check | Preserve drill signal |
| Render plan | Standalone function, no `MenuState` | Clean path for v2.5 |
| MergedRow | `Row + child_index: u16`, side vector | Design §2.1: "not in Row API" |
| Frame replay | Structural (phase 2.5), not Esc-only | Design §5.1: "applied even without Esc" |
| Tab/Enter | Clear fuzzy terms, pin selection + space | Design §4 accept semantics |
