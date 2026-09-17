# Step 4 design: `NodeEntry::node_traverse()` internals

Design: `slash_menu_v2.5_design.md`. This document specifies the internal
data structures and algorithm for step 4 of the implementation plan.

---

## 1. Entry point: `SlashDotHandle::eval_prompt(prompt, key) -> MenuResult`

Thin wrapper. Nothing lives here but the epoch.

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
prompt text** (not a pre-parsed tail). Path walking happens inside.

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
  │  Split prompt preserving trailing space.                       │
  │  Walk children: try each word (except last) as child ID.       │
  │  Remaining words = tail for this node.                         │
  │                                                                │
  ├─ Phase 1: data refresh ───────────────────────────────────────┤
  │                                                                │
  │  if key != MoveCursor:                                         │
  │    if epoch changed or tail changed: refresh_data(epoch, tail) │
  │    apply_field_filter(tail)                                    │
  │                                                                │
  ├─ Phase 2: rank + cursor ──────────────────────────────────────┤
  │                                                                │
  │  if key != MoveCursor: rank(terms, selection)                  │
  │  seed_or_keep_cursor(selection, terms)                         │
  │  resolve_active_marker()                                       │
  │                                                                │
  ├─ Phase 2.5: frame replay (structural, not Esc-specific) ──────┤
  │                                                                │
  │  // Design §5.1 step 1: applies even without Esc.              │
  │  // When a child ID is valid with no trailing space, replay    │
  │  // the local frame (restore previous filter state).           │
  │  if sel matches a child && !trailing_space:                    │
  │    if let Some(frame_tail) = self.cache.frame.take():          │
  │      // Resume the filter state from before the drill.         │
  │      // The child ID is on the path; frame has the tail.       │
  │      new_prompt = reconstruct(path, frame_tail)                │
  │      return Continue(new_prompt, display)                      │
  │    // No frame: show the child's listing (normal Tab drill)    │
  │                                                                │
  ├─ Phase 3: handle key ─────────────────────────────────────────┤
  │                                                                │
  │  match key:                                                    │
  │    MoveCursor → update cursor                                  │
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

### 3.1 `NodeCursorCache` — per-node ephemeral state

```rust
struct NodeCursorCache {
    // Invalidation key
    last_epoch: u64,
    last_tail: String,

    // Raw data (from children's data())
    raw_rows: Vec<TaggedRow>,
    field_space: Option<FieldSpace>,

    // Filtered data (after §7.1 field filter)
    filtered_rows: Vec<Row>,

    // Ranked view (from filtered_rows + terms + selection)
    ranked_rows: Vec<Row>,
    active_marker: Option<usize>,

    // Sticky cursor: survives re-derives until selection/terms change.
    // (cursor, selection_at_set_time, terms_at_set_time)
    sticky_cursor: Option<(usize, Option<String>, String)>,

    // Esc ladder frame: the tail before the last drill.
    // Set on every drill (Tab/Enter into a child).
    // Cleared (taken) on frame replay (phase 2.5 or Esc step 2).
    frame: Option<String>,
}
```

**Keyed on `(epoch, tail)`**: the tail IS this node's input — the part
of the prompt after path walking consumed child IDs. It captures terms,
modifiers, selection, and the trailing-space drill signal. When either
epoch or tail changes, we re-fetch and re-rank. For `MoveCursor`, we
skip Phases 1 and 2 entirely.

### 3.2 Frame — per-node `Option<String>`, NOT a stack

Each `NodeEntry` stores `frame: Option<String>` in its cursor cache.
This is the tail (filter/sort state) at this node *before the last
drill*. The selection ID is already on the path (consumed by the
child's path walk), so we don't need to remember it.

- **Set** on every drill (Tab/Enter into a child): `frame = Some(current_tail)`
- **Cleared** (taken) on frame replay: `frame.take()` → returns the
  stored tail, sets frame to None

Lives entirely inside `NodeEntry`. `eval_prompt` knows nothing about it.

### 3.3 `DisplayState` — computed view (transient)

```rust
struct DisplayState {
    rows: Vec<Row>,
    cursor: usize,
    active: Option<usize>,
    title: String,
    detail: Vec<String>,
    columns: Vec<ColumnDef>,
    is_leaf: bool,
}
```

Built from `NodeCursorCache` after ranking. Not stored.

### 3.4 Row augmentation — child index tagging

Each row from a child is tagged with `child_index: u16` for submit
dispatch. Side vector, not on `Row`:

```rust
struct TaggedRow { row: Row, child_index: u16 }
```

`NodeCursorCache` stores `Vec<TaggedRow>` internally; `DisplayState`
exposes only `Vec<Row>`.

---

## 4. Phase 0: path walk

Split the prompt preserving trailing space, then walk children.

```
let trailing_space = prompt.ends_with(' ');
let words: Vec<&str> = prompt.split(' ')
    .filter(|w| !w.is_empty())
    .collect();

let mut path: Vec<String> = Vec::new();
let mut wi = 0;
while wi < words.len().saturating_sub(1) {
    if words[wi].starts_with('.') { break; }
    if let Some(idx) = self.child_index(words[wi]) {
        path.push(words[wi].to_string());
        wi += 1;
    } else {
        break;
    }
}

let mut tail = words[wi..].join(" ");
if trailing_space && !tail.is_empty() {
    tail.push(' ');
}
```

---

## 5. Phase 1: data refresh

```
refresh_data(epoch, tail):
  self.cache.last_epoch = epoch;
  self.cache.last_tail = tail.clone();

  let mut all_rows: Vec<TaggedRow> = Vec::new();
  let mut field_space: Option<FieldSpace> = None;

  for (i, child) in self.children.iter_mut().enumerate() {
    let data = child.inner.data(epoch, &empty_path_args());
    if let Ok(response) = data {
      for row in response.rows {
        all_rows.push(TaggedRow { row, child_index: i as u16 });
      }
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

  self.cache.raw_rows = all_rows;
  self.cache.field_space = field_space;
```

### §7.1 field filter

```
apply_field_filter(tail):
  let lvl = parse_level(tail, &selection_rule);
  if lvl.modifiers.is_empty() {
    self.cache.filtered_rows = self.cache.raw_rows.iter()
        .map(|tr| tr.row.clone()).collect();
    return;
  }
  let space = self.cache.field_space.clone().unwrap_or_default();
  let query = compile_query(&lvl.modifiers, &space);
  self.cache.filtered_rows = apply_query(
      &self.cache.raw_rows.iter().map(|tr| tr.row.clone()).collect(),
      &query, &space
  );
```

---

## 6. Phase 2: rank + cursor

### 6.1 Rank

```
rank(terms, selection):
  let rank_result = rank_and_pin(
    &self.cache.filtered_rows, &terms, selection.as_deref());
  self.cache.ranked_rows = rank_result.rows;
```

### 6.2 Cursor seeding

Same logic as the current interpreter (§4, §6.5):

```
seed_or_keep_cursor(selection, terms):
  match self.cache.sticky_cursor {
    Some((idx, prev_sel, prev_terms))
      if prev_sel == selection && prev_terms == terms =>
    {
      // Sticky: keep it, clamped
      self.cache.cursor = idx.min(ranked_rows.len() - 1)
    }
    _ => {
      // Fresh: seed on selection pin, or active marker, or 0
      self.cache.cursor =
        selection.map(|_| rank_active)
          .unwrap_or(active_marker.unwrap_or(0))
    }
  }
  self.cache.sticky_cursor =
    Some((self.cache.cursor, selection.clone(), terms.clone()));
```

### 6.3 Active marker

Resolves `data().active_ids` against ranked rows. Same mechanism as
current interpreter's `resolve_active_selector`.

---

## 7. Phase 2.5: frame replay (structural check)

Design §5.1 step 1: "applied even without Esc, so even when restoring
from prompt." When a child ID is valid with no trailing space, and the
local frame holds a previous tail, replay it.

```
// After rank+cursor, before key handling:
if let Some(sel) = &selection {
    if !trailing_space && self.child_index(sel).is_some() {
        if let Some(frame_tail) = self.cache.frame.take() {
            // Resume filter state from before the drill.
            // Reconstruct prompt: path + frame_tail.
            let new_prompt = build_prompt(&path, &frame_tail);
            // Rebuild display from the restored state.
            // (Need to re-rank with frame_tail's terms/selection.)
            let restored = self.compute_display(epoch, &frame_tail);
            return NodeResult::Continue { prompt: new_prompt, display: restored };
        }
        // No frame: normal — show child's listing.
        // (Fall through to phase 3 Tab/Enter drill, or display the listing.)
    }
}
```

When there IS a frame and the selection matches a child without trailing
space, we restore the filter state. When there's NO frame, we show the
child's listing normally (the user Tab'd to it).

---

## 8. Phase 3: key handling

### 8.1 `MoveCursor(delta)`

```
let len = ranked_rows.len();
cursor = (cursor as isize + delta).clamp(0, len - 1) as usize;
// Rebuild RenderPlan from cached rows.
```

### 8.2 `Tab`

```
let active_row = ranked_rows[cursor];
let active_id = active_row.id;

// Accept semantics (design §4): fuzzy terms cleared, selection pinned.
let new_tail = format!("{} ", active_id);

// Save filter state before drill
self.cache.frame = Some(tail.clone());

// If listing (has children):
//   Find child matching active_id, recurse into it.
//   Return child's NodeResult with full prompt.

// If leaf:
//   Call inner.prepare(epoch, path_args)
//   Return Done/Prepared.
```

### 8.3 `Enter`

Same as Tab, except at leaf level Enter submits directly.

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
    // Pop the ID from the path. The parent's node_traverse will
    // pick up from there.
    path.pop();
    return Continue(reconstruct(path, ""));

// Step 5: at root with nothing to clear → close
return Done(None);
```

**Esc walk-through:**

| Prompt | frame | tail parsed | Action | Result |
|--------|-------|-------------|--------|--------|
| `model model-a ` | - | sel="model-a", ts=true | step 3: strip space | `model model-a` |
| `model model-a` | None | sel="model-a" | step 4: pop selection | root close |
| `model flag model-a ` | - | terms="flag", sel="model-a", ts=true | step 3: strip space | `model flag model-a` |
| `model flag model-a` | None | terms="flag", sel="model-a" | step 2: clear terms | `model model-a ` |
| `model model-a ` | - | sel="model-a", ts=true | step 3: strip space | `model model-a` |
| `model model-a` | None | sel="model-a" | step 4: pop selection | root close |
| `model ` | Some("flag") | ts=true, no sel | step 3: strip space | `model` |
| `model` | Some("flag") | sel="model" (if valid child) | **phase 2.5**: frame replay | `flag` (restored) |

The frame replay in phase 2.5 catches the case where the user drilled
into "model " with frame=Some("flag"), then Esc strips the space to
"model", and phase 2.5 detects: "model" is a valid child ID, no trailing
space, frame=Some("flag") → replay, restoring the "flag" filter state.

---

## 9. Render plan construction

Clean new path, independent of old `PlanBuilder`/`MenuState`.

```
fn build_render_plan(display: &DisplayState) -> RenderPlan {
  let mut lines = Vec::new();

  lines.push(RenderLine { text: display.title.clone(), ... });

  for d in &display.detail {
    lines.push(RenderLine { text: d.clone(), style: Style::HEADER, is_header: true, ... });
  }

  let row_texts = if !display.columns.is_empty() || has_fields(&display.rows) {
    build_table_rows(display)
  } else {
    display.rows.iter().map(|r| format_row(r)).collect()
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
    active: display.active_marker,
    detail: display.detail.clone(),
  }
}
```

No dependency on `MenuState`. Uses `DisplayState` directly.

---

## 10. Borrow discipline

1. **Own state** (Phase 1+2): `self.cache` accessed. No child borrows.
2. **Descend** (Phase 3): `self.children[idx].node_traverse()`. One child
   borrowed mutably at a time.
3. **Post-processing** (Phase 4): build from `self.cache`. No child borrows.

---

## 11. Implementation order

### 4a. Internal types + render plan
- `NodeCursorCache`, `DisplayState`, `TaggedRow`
- `build_render_plan()` standalone function
- Stub `node_traverse`

### 4b. Data refresh (Phase 1)
- `refresh_data()`, field filter, field space inference

### 4c. Rank + cursor (Phase 2)
- `rank_and_pin`, cursor seeding, active marker

### 4d. MoveCursor
- Update cursor, rebuild RenderPlan

### 4e. Tab + Enter
- Complete, drill, frame set, leaf submit

### 4f. Esc + frame replay
- §5.1 ladder steps 2–4
- Phase 2.5 frame replay (structural)
- Multi-level test scenarios

### 4g. `eval_prompt` wiring
- Thin wrapper calling `root.node_traverse`

### 4h. Tests
- Port ~50 interpreter tests to new harness

---

## 12. Migration strategy

- `build_render_plan()` takes `DisplayState`, returns `RenderPlan`.
  No `MenuState` dependency.
- Old `render.rs` / `PlanBuilder` untouched.
- Deleted when all commands migrated (step 6).

---

## 13. Key design decisions

| Decision | Choice | Rationale |
|---|---|---|
| Frame storage | Per-node `Option<String>` in cache | One slot, set on drill, taken on replay |
| Frame lives in | `NodeEntry`, not `eval_prompt` | `eval_prompt` is thin — only epoch |
| Cursor cache key | `(epoch, tail)` | Tail IS this node's input after path walk |
| Frame content | Previous tail (filter/sort state) | ID on path, not needed in frame |
| Frame replay | Structural check (phase 2.5), not Esc-only | Design §5.1: "applied even without Esc" |
| Path walk | Inside `node_traverse` | Each node handles its own level |
| Trailing space | `split(' ')` not `split_whitespace()` | Preserve trailing space |
| Render plan | Standalone function, not `PlanBuilder` | Clean path, no `MenuState` |
| Tagged rows | Side `Vec<TaggedRow>` | Don't change `Row` API |
| `MoveCursor` | Skip refresh + re-rank | Only cursor changes |
| Tab/Enter | Clear fuzzy terms, pin selection + space | Design §4 accept semantics |
