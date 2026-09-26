# v2.5 Cleanup Pass 2 — repl.rs & Confirmation Dialogs

**Status:** Proposed — audit complete, ready for execution

---

## ✅ Immediate Cleanup (safe, no behavior change)

### Reduce public API surface in `repl.rs`
| Item | Current | Change | Reason |
|------|---------|--------|--------|
| `overlay_lines_with_bgs` | `pub` | → `pub(crate)` | Only used in repl.rs tests |
| `overlay_window` | `pub` | → `pub(crate)` | Only used in repl.rs tests |
| `page_delta` | `pub` | → `pub(crate)` | Only used in repl.rs tests |
| `paint_plan` | `pub` | → private | Internal only |
| `compute_space_budget` | `pub` | → private | Internal only |
| `visual_row_window` | `pub` | → private | Internal only |
| `render_logical_line` | `pub` | → private | Internal only |
| `ActiveOverlay::cursor_row` | `pub` | → private | Internal only |
| `ModelFilter::title_suffix` | `pub` | → **delete** | Unused anywhere |

### Keep as-is (production dependencies)
- `overlay_lines` — used by `ConsoleOutput::confirm()` and `confirm_compaction_summary()`
- `LineEditor`, `History`, `SessionMeta` — used by agent.rs, prompt_complete.rs
- `TermGuard`, `Repl`, `InputAction`, `ReplCtx`, `ModelFilter` — used by agent.rs

---

## 📋 Backlog — Future Work (requires v2.5 ports)

### 1. Tool Confirmation Dialog → v2.5 Menu
**Location:** `src/output.rs:1915` — `ConsoleOutput::confirm()`
- Triggered when agent tools need permission (file write, shell, etc.)
- Shows: **Yes / No / Always (session)**
- Currently: Custom overlay loop with `overlay_lines` + raw `event::read()`
- **Port to:** v2.5 `SlashDotNode` with `data()` + `submit()`, or a dedicated confirm node type
- Blocker: Runs in raw mode during in-flight request; must pause background reader (same pattern as `/replay`)

### 2. Compaction Summary Review Dialog → v2.5 Menu
**Location:** `src/output.rs:2013` — `ConsoleOutput::confirm_compaction_summary()`
- Triggered after `$EDITOR` review of LLM-generated compaction summary
- Shows: **Accept / Cancel**
- Currently: Custom overlay loop with `overlay_lines` + raw `event::read()`
- **Port to:** v2.5 leaf node (similar to `/replay` / `/quit` pattern)
- Blocker: Same raw-mode constraints as above

### 3. Settings Overlay Tests → Remove or Relocate
**Location:** `src/settings.rs:879, 955` — test-only uses of `overlay_lines`
- These are regression tests for table alignment (not a `/settings` command)
- Once `overlay_lines` is no longer used in production, these tests can be deleted or converted to test `paint_plan` directly

---

## Execution Order

1. **Phase A** — Reduce visibility / delete `title_suffix` (green build + tests)
2. **Phase B** — Port confirmation dialogs to v2.5 (separate PR, depends on async v2.5 menu support for blocking dialogs)
3. **Phase C** — Remove `overlay_lines` entirely once (1) and (2) are done

---

## Notes
- The v2.5 menu engine already supports the patterns needed (leaf nodes with header + submit)
- `AsyncOp`/`ControlConfig` in `AppState` can carry confirmation results back to the loop
- Background reader pause/resume pattern is already established (`reader_paused` in `ConsoleOutput`)