# v2.5 Cleanup Pass 2 — repl.rs & Confirmation Dialogs

**Status:** Phase 1 & 2 complete (`6c70b9f`, `927cedc`, `72107ff`); backlog updated below.

---

## ✅ Immediate Cleanup (done)

### Reduce public API surface in `repl.rs`
| Item | Before | After |
|------|--------|-------|
| `overlay_lines_with_bgs` | `pub` | `pub(crate)` |
| `overlay_window` | `pub` | `pub(crate)` |
| `page_delta` | `pub` | `pub(crate)` |
| `paint_plan` | `pub` | private |
| `compute_space_budget` | `pub` | private |
| `visual_row_window` | `pub` | private |
| `render_logical_line` | `pub` | private |
| `ActiveOverlay::cursor_row` | `pub` | private |
| `ModelFilter::title_suffix` | `pub` | **deleted** (unused) |

### Keep as-is (production dependencies)
- `overlay_lines` — used by `ConsoleOutput::confirm()` and `confirm_compaction_summary()` (will be retired after Phase 3)

---

## ✅ Phase 1 — Move plan painter into crate (done, `927cedc`)

**Moved to `crates/slashdot_menu/src/render.rs`:**
- `paint_plan`, `compute_space_budget`, `visual_row_window`, `render_logical_line`, `page_delta`

**Signature change:**
```rust
// repl.rs (today)
fn paint_plan(plan: &RenderPlan, term_h: usize) -> String

// render.rs (done)
pub fn paint_plan(plan: &RenderPlan, term_w: usize, term_h: usize) -> String
```
`RenderPlan` is already built at a specific width (via `ViewRequest.render_ctx.term_width` → `rebuild_rows` → pre-formatted `LogicalLine` strings). The host must pass the **same** `term_w` used by the engine when building the plan. `term_h` is host-side (overlay budget: 2/3 of screen height).

**Stays in repl.rs** (host input-region concerns):
- `overlay_window`, `page_delta` — windowing/scroll math tied to the REPL's `prev_term_h` anchoring

**Gate:** all tests moved; `cargo test` green; `cargo fmt`.

---

## ✅ Phase 2 — Shared sync confirm driver in `src/ui.rs` (done, `72107ff`)

**New module** `src/ui.rs` (not on `Output` trait).

```rust
use crate::output::Output;
use crate::slashdot_menu::{render::paint_plan, LogicalLine, RenderPlan};
use crate::slashdot_menu::node_impl::StaticRow;

pub struct ConfirmSpec<'a> {
    pub header: &'a str,
    pub options: &'a [StaticRow],           // id + desc
    pub quick_keys: Option<&'a [(char, usize)]>,
    pub default_sel: usize,
}

pub enum ConfirmResult {
    Selected(usize),
    Dismissed,
}

/// Sync (blocking) driver — compatible with `Output::confirm` trait.
pub fn confirm(out: &dyn Output, spec: &ConfirmSpec<'_>) -> ConfirmResult
```

**Trait addition** (trivial, on existing trait):
```rust
// output.rs - one new default method
fn confirm_default(&self, message: &str) -> Confirmation {
    self.confirm(message)  // existing behavior for non-interactive
}
```

**Why not on `Output`?**
- `confirm_gate()` already discriminates interactivity (`None` = non-interactive)
- `cursor_row()`, `stop_thinking()`, `display_plain()` already on trait
- `Output` stays 100% sync (no dyn-compat issues, no `#[async_trait]`)
- The 5 non-interactive impls keep their current sync behavior (`Yes`/`No` policies)

**Dialogs rewired:**

| Dialog | `spec.options` (id, desc) | default | quick_keys | Esc maps to |
|---|---|---|---|---|
| `confirm()` (tool) | `("yes","Yes")`, `("no","No")`, `("always","Always (session)")` | 1 (`no`) | `[('y',0), ('s',2)]` | `no` |
| `confirm_compaction_summary()` | `("accept","Accept")`, `("cancel","Cancel")` | 0 (`accept`) | `[('a',0), ('c',1)]` | `cancel` |

⚠️ **Semantic note:** Driver returns `Dismissed` on Esc; callers **must** map to the safe option for THAT dialog (not the default). Documented on the driver.

---

## 📋 Backlog — Remaining

### Phase 3 — Retire `overlay_lines` / `overlay_lines_with_bgs`

- Delete `Repl::overlay_lines` and `overlay_lines_with_bgs` (and their tests)
- The two `settings.rs` alignment tests port to `paint_plan` or drop
- `overlay_lines` is the last user of the old overlay rendering path

### Phase 4 — (Optional) `review_compaction_summary` to async

```rust
// repl.rs
pub async fn review_compaction_summary(guard: &TermGuard, out: &dyn Output, draft: &str) -> Option<String>
```
Callers: `agent.rs:4579` (already async) and `agent.rs:3387` (sync closure → becomes async closure). Small ripple.

---

## Prerequisites / Risks (addressed in Phase 2)

1. **`crossterm` `event-stream` feature** — Added. Driver uses blocking `event::read()` for sync trait compatibility. `EventStream` (non-blocking) would require async trait or a separate runtime; current approach works with sync `Output::confirm`.

2. **Visual change** — Dialogs now use `paint_plan` (panel BG `48;5;235`) instead of `overlay_lines` (plain). Side-by-side eyeball against `/` menus recommended.

3. **Never-scroll clamp** — Dialogs clamp `top = min(cursor, h - n)` via `paint_plan` budget logic. Long messages won't push dialog off-screen.

---

## Gates
- `src/bin/confirm_probe.rs` + `confirm_raw_mode` pty test — green
- Full `cargo test` (671 passed)
- `cargo fmt`

---

## Notes
- `StaticRow` re-exported from `slashdot_menu` (`pub use node_impl::StaticRow` in lib.rs)
- In-menu `prepare() → submit()` confirm ladder already exists (`node_entry.rs:425`) — this driver only covers the **standalone** case
- `note_lines` not needed: `display_plain` already does "print + advance cursor"