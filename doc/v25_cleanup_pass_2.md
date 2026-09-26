# v2.5 Cleanup Pass 2 — repl.rs & Confirmation Dialogs

**Status:** Immediate items committed (`6c70b9f`); backlog refined below.

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
- `overlay_lines` — used by `ConsoleOutput::confirm()` and `confirm_compaction_summary()`

---

## 📋 Backlog — Refined Plan

### Phase 1 — Move plan painter into crate (no behavior change)

**Move to `crates/slashdot_menu/src/render.rs`:**
- `paint_plan`, `compute_space_budget`, `visual_row_window`, `render_logical_line`

**Signature change:**
```rust
// repl.rs (today)
fn paint_plan(plan: &RenderPlan, term_h: usize) -> String

// render.rs (proposed)
pub fn paint_plan(plan: &RenderPlan, term_w: usize, term_h: usize) -> String
```
`RenderPlan` is already built at a specific width (via `ViewRequest.render_ctx.term_width` → `rebuild_rows` → pre-formatted `LogicalLine` strings). The host must pass the **same** `term_w` used by the engine when building the plan. `term_h` is host-side (overlay budget: 2/3 of screen height).

**Stays in repl.rs** (host input-region concerns):
- `overlay_window`, `page_delta` — windowing/scroll math tied to the REPL's `prev_term_h` anchoring

**Gate:** the 4 fns' tests move with them; `cargo test` green; `cargo fmt`.

---

### Phase 2 — Shared async confirm driver in `src/ui.rs`

**New module** `src/ui.rs` (or `src/confirm.rs`), **not** on `Output` trait.

```rust
use crate::output::Output;
use crate::slashdot_menu::{StaticRow, render::RenderPlan, HostKey};

pub struct ConfirmSpec<'a> {
    pub header: &'a str,
    pub options: &'a [StaticRow],           // id + desc; default = options[0]
    pub quick_keys: Option<&'a [(char, usize)]>, // optional override
}

/// Returns `Some(chosen_index)`, `None` if dismissed with Esc.
pub async fn confirm(out: &dyn Output, spec: &ConfirmSpec<'_>) -> Option<usize> {
    // 1. Early-return for non-interactive: delegate to trait's sync policy
    if out.confirm_gate().is_none() {
        return Some(out.confirm_default(spec.header)); // see below
    }

    // 2. Build RenderPlan
    let plan = RenderPlan {
        detail: vec![LogicalLine::new(spec.header.to_string())],
        rows: Rc::new(spec.options.iter()
            .map(|r| LogicalLine::new(format!("{}  {}", r.id, r.desc.as_deref().unwrap_or(""))))
            .collect()),
        selected: Some(0),
        active_rows: vec![],
        table_header: None,
    };

    // 3. Position anchor (existing cursor_row + clamp math from output.rs)
    // 4. Loop: paint via slashdot_menu::render::paint_plan(), read HostKey::Up/Down/Enter/Esc
    // 5. Clear overlay, echo choice via out.display_plain()
}
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

---

### Phase 3 — Rewire both dialogs + retire `overlay_lines`

| Dialog | `spec.options` (id, desc) | default | quick_keys | Esc maps to |
|---|---|---|---|---|
| `confirm()` (tool) | `("yes","Yes")`, `("no","No")`, `("always","Always (session)")` | 1 (`no`) | `[('y',0), ('s',2)]` | `no` |
| `confirm_compaction_summary()` | `("accept","Accept")`, `("cancel","Cancel")` | 0 (`accept`) | `[('a',0), ('c',1)]` | `cancel` |

⚠️ **Semantic note:** Driver returns `None` on Esc (dismiss); callers **must** decide what that means:
```rust
match ui::confirm(out, &spec).await {
    Some(idx) => spec.options[idx].id,
    None => {
        // Esc means "cancel / go back" — choose the safe option for THIS dialog.
        // Tool confirm: "no"  (index 1).  Compaction review: "cancel" (index 1).
        spec.options[1].id
    }
}
```
The "default" (first option) may be the *preferred* choice (Accept/Yes), but Esc universally means *dismiss* (Cancel/No). Document this on the driver.

Then delete `overlay_lines` / `overlay_lines_with_bgs` and their tests. The two `settings.rs` alignment tests port to `paint_plan` or drop.

---

### Phase 4 — (Optional) `review_compaction_summary` to async

```rust
// repl.rs
pub async fn review_compaction_summary(guard: &TermGuard, out: &dyn Output, draft: &str) -> Option<String>
```
Callers: `agent.rs:4579` (already async) and `agent.rs:3387` (sync closure → becomes async closure). Small ripple.

---

## Prerequisites / Risks to Verify

1. **`crossterm` `event-stream` feature** — `SlashDotNode` already async; confirm driver should use `EventStream` (non-blocking) instead of `event::read()` (blocks runtime). Current runtime is `current_thread` in tests, `multi_thread` in prod. Verify `EventStream` cooperates before committing shape.

2. **Visual change** — Dialogs today use `overlay_lines` (no per-row BG); `paint_plan` gives rows `48;5;235` panel BG. They'll look different. Needs side-by-side eyeball against `/` menus.

3. **Never-scroll clamp** — Dialogs clamp `top = min(cursor, h - n)` with `n=4`/`3`. `paint_plan` budgets 2/3 of `term_h`. Pass a clamped `term_h` or cap detail budget so long messages don't push dialog off-screen.

---

## Gates
- `src/bin/confirm_probe.rs` + `confirm_raw_mode` pty test — must stay green
- Full `cargo test`
- `cargo fmt` (my last commit left `paint_plan` multi-line; later `cargo fmt` fixed, run it this time)

---

## Notes
- `StaticRow` already re-exported from `slashdot_menu` (`pub use node_impl::StaticRow` in lib.rs)
- In-menu `prepare() → submit()` confirm ladder already exists (`node_entry.rs:425`) — this driver only covers the **standalone** case
- `note_lines` not needed: `display_plain` already does "print + advance cursor"