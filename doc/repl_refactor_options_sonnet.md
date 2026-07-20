# REPL overlay/menu system — structural critique and refactor options

*Analysis of `src/repl.rs` as of 2025-07 (~4 100 lines).*

---

## 1. Current structure at a glance

`read_input` is the heart of the REPL.  Its inner event loop (~750 lines) has three
distinct phases on every iteration:

1. **Overlay computation** — inspect the buffer, set `kind: u8` (0–9),
   produce `opts: Vec<String>` (display rows), `title`, `cur_word`, `cur_filter`,
   `active`.
2. **Redraw** — call the `redraw` closure with the computed rows.
3. **Key dispatch** — match on `key.code` and then, inside each arm, match on
   `kind` again.

The nine overlay "kinds" are:

| kind | Meaning          | Trigger                          |
|------|------------------|----------------------------------|
| 0    | No overlay       | bare prompt                      |
| 1    | Command list     | `/` + unknown word               |
| 2    | Model picker     | `/model …`                       |
| 3    | Thinking picker  | `/thinking …`                    |
| 4    | Help             | `help_view` flag (separate bool) |
| 5    | Settings         | `/set …`                         |
| 6    | Persona picker   | `/persona …`                     |
| 7    | Session stats    | `/stat`                          |
| 8    | Sandbox bindings | `/sandbox …`                     |
| 9    | Path completions | Tab on bare prompt               |

---

## 2. Structural pain points

### 2.1 `kind: u8` is a type-erased coupling

The integer `kind` is assigned during the overlay-computation phase and then
consulted again in three separate match ladders inside the key-dispatch phase:

```rust
// --- Tab handler ---
KeyCode::Tab => {
    match kind {
        1 => { /* re-run filter_commands */ }
        2 => { /* re-run model_overlay_entries */ }
        3 => { /* re-run thinking_overlay_entries */ }
        5 => { /* re-run parse_settings + overlay_rows / value_cands */ }
        6 => { /* re-run setting_value_candidates + fuzzy_filter */ }
        8 => { /* re-derive subcommand, re-run completions */ }
        9 => { /* index path_completions */ }
        _ => {}
    }
}
// --- Enter handler ---
KeyCode::Enter => {
    match kind {
        1 => { /* re-run filter_commands */ }
        2 => { /* re-run model_overlay_entries */ }
        3 => { /* re-run thinking_overlay_entries */ }
        5 => { /* re-run parse_settings + overlay_rows / value_cands */ }
        6 => { /* re-run setting_value_candidates + fuzzy_filter */ }
        8 => { /* re-derive subcommand */ }
        9 => { /* index path_completions */ }
        _ => {}
    }
}
// --- Esc handler ---
KeyCode::Esc => match kind {
    1..=4 | 6..=8 => editor.set(""),
    5 => { /* pop one level: find last space */ }
    9 => { path_completions.clear(); }
    _ => {}
}
```

Three parallel ladders over the same space.  Adding a new overlay requires
edits in all three places, plus the overlay-computation block above.  There is
no type system help ensuring consistency — a new overlay kind 10 that handles
Tab but not Esc silently does nothing on Esc.

### 2.2 Double (or triple) computation of overlay entries

The display rows and the "value" needed by Tab/Enter come from the same
underlying data, but are computed separately.  For example, the `/model`
overlay calls `model_overlay_entries(ctx, &cur_filter)` once during
overlay-computation and again in both the Tab and Enter handlers.  Same for
`/thinking`, `/persona`, `/sandbox`.

The `/set` value-pick mode has a `value_cands: Vec<ValueCandidate>` variable
that lives *outside* the loop precisely to work around this: it is populated
during overlay-computation and read by the Enter handler.  This is an ad-hoc
cache for one overlay; there is no general mechanism.

### 2.3 Bridge variables pollute the loop scope

`cur_word`, `cur_filter`, `value_cands`, `path_completions`, and
`completion_pos` are all declared before the loop and mutated by the
overlay-computation phase so the key-dispatch phase can read them.  They are
effectively a manual, partial re-encoding of the overlay's computed state as
loose local variables.  The loop carries ~8 such bookkeeping variables.

### 2.4 `help_view` escapes the `kind` system

`help_view` is a separate `bool` that gate-sets `kind = 4` early in the
overlay-computation block.  Rather than a ninth kind in the table, help gets
special-cased with its own `if kind == 4 { ... continue; }` guard before the
general overlay dispatch.  It also routes *all* non-modifier key events to the
editor (a unique behavior not shared by any other overlay).  This makes "help"
harder to reason about as a regular overlay.

### 2.5 Verbosity from repeated table construction

The two-column and four-column `Table` construction pattern repeats four times
with minor variation (different column names, widths, alignment, content):

```rust
// Model overlay
let mut table = Table::new(&["Model", "Ctx", "Cost", "?"])
    .with_aligns(&[...]).with_min_widths(&[12, 5, 14, 3])
    .with_priorities(&[3, 1, 2, 1]).set_gap(2);
for e in &entries { table.add_row(&[...]); }
let rows = table.render_multiline(avail);

// Persona overlay
let mut table = Table::new(&["Persona", "Description"])
    .with_aligns(&[...]).with_min_widths(&[20, 30])
    .with_priorities(&[2, 1]).set_gap(2);
for c in &filtered { table.add_row(&[...]); }
for row in table.render_multiline(avail) { opts.push(row); }

// Settings overlay — two variants (list mode and value-pick mode)
// ... same pattern twice more
```

Each arm computes `avail = term_width().saturating_sub(2)` independently.  A
shared helper would collapse these to one or two lines each.

### 2.6 The Esc ladder is structurally wrong for `/set`

`/set` value-mode's Esc "pops one level" (strips the last token, reverting to
the setting-list view) rather than clearing the buffer entirely.  This is a
real design choice — `/set` has two nested levels — but it is expressed as a
special case inside a flat `match kind { ... }` rather than as a property of
the overlay type.  If a future overlay ever wants a third level, this structure
does not compose.

### 2.7 Sel-clamping and active-marker logic is separated from the overlay

```rust
if kind != last_kind {
    sel = 0;
    last_kind = kind;
}
let max_sel = opts.len().saturating_sub(1);
if sel > max_sel {
    sel = max_sel;
}
```

These post-computation fixups belong to the overlay's identity but live in the
loop as free-floating mutations.

---

## 3. Refactoring options

### Option A — `ActiveOverlay` enum carrying computed state  *(recommended)*

Replace `kind: u8` and the bridge variables with a single enum whose variants
carry the computed data:

```rust
enum ActiveOverlay {
    None,
    Help,
    CommandList(Vec<Ranked<'static, SlashCommand>>),
    Model      { entries: Vec<ModelEntry>,      filter: String },
    Thinking   (Vec<ThinkingLevel>),
    Settings   { rows: Vec<(&'static SettingMeta, String)>, filter: String },
    SettingsVal{ key: SettingKey, cands: Vec<ValueCandidate>, filter: String },
    Persona    { cands: Vec<ValueCandidate>,    filter: String },
    Stat       (Vec<String>),
    Sandbox    { items: Vec<SandboxItem>,       filter: String },
    PathComplete{ completions: Vec<String>, tok_pos: Option<(usize, usize)> },
}
```

The overlay-computation block becomes `ActiveOverlay::from_buffer(buf, ctx,
settings)` — one function that returns the enum.

Three methods handle all keybinding dispatch:

```rust
impl ActiveOverlay {
    fn display_rows(&self) -> (/*title*/ &str, Vec<String>, /*active*/ Option<usize>);

    fn handle_tab(&self, sel: usize, editor: &mut LineEditor);

    fn handle_enter(
        &self, sel: usize,
        editor: &mut LineEditor,
        output: &dyn Output, input_row: &mut usize,
        settings: &mut SettingsStore, dirty: &mut bool,
    ) -> OverlayAction;   // enum: Nothing | Close(InputAction) | RevertTo(String)

    fn handle_esc(&self, editor: &mut LineEditor);

    fn resets_sel(&self, prev: &Self) -> bool;  // true when kind changed
}
```

**What this buys:**

- Each variant's Enter handler indexes `self.entries[sel]` or `self.cands[sel]`
  — no recomputation, no `value_cands` hack.
- Three parallel match ladders collapse to three one-liner calls in the event loop.
- Adding a new overlay = add one variant + implement three methods.  The
  compiler enforces completeness.
- `help_view` disappears; Help is just `ActiveOverlay::Help` with a
  passthrough-key impl.
- Esc "pop one level" for `/set` lives in
  `ActiveOverlay::SettingsVal::handle_esc`, not in a shared ladder.
- Sel-clamping lives in `resets_sel` + overlay length query, inside
  `ActiveOverlay`, not in free-floating loop code.

The main event loop shrinks from ~750 lines to roughly:

```rust
let overlay = ActiveOverlay::from_buffer(&editor.text(), ctx, settings);
if overlay.resets_sel(&last_overlay) { sel = 0; }
let (title, rows, active) = overlay.display_rows();
redraw(&editor, &rows, sel, active, &mut input_row);
last_overlay = overlay;

match event::read() { ... key ... =>
    match key.code {
        Up        => { if sel > 0 { sel -= 1; } }
        Down      => { sel = (sel + 1).min(overlay.len().saturating_sub(1)); }
        Tab       => overlay.handle_tab(sel, &mut editor),
        Enter     => { match overlay.handle_enter(...) { ... } }
        Esc       => overlay.handle_esc(&mut editor),
        _         => { /* editing */ }
    }
}
```

The loop is now structurally identical for all overlays.

**Trade-off:** `from_buffer` is still a long function (all the match arms move
there), but it is now a pure computation with no side effects and no key
handling — easier to test and read.

---

### Option B — `OverlayItem` typed value union

Extend Option A so that display and value travel together:

```rust
struct OverlayItem {
    display: String,       // pre-rendered (may contain \n for multiline table rows)
    value:   OverlayValue,
}

enum OverlayValue {
    Command(SlashCommand),
    Model(ModelEntry),
    ThinkingLevel(ThinkingLevel),
    SettingRow(&'static SettingMeta),
    SettingValue(ValueCandidate),
    PersonaValue(ValueCandidate),
    SandboxItem(SandboxItem),
    Path(String),
    Info,    // stat / help rows: display only, no action
}
```

The `ActiveOverlay` then holds `items: Vec<OverlayItem>` instead of a
variant-specific `Vec<ModelEntry>`, `Vec<ValueCandidate>`, etc.  Tab and Enter
pattern-match on `items[sel].value` — a single match replaces the three
parallel ladders.

This is more uniform but slightly less type-safe per-variant (a Model overlay
variant *could* accidentally hold a `Path` item).  The `ActiveOverlay` enum
from Option A can be kept as the outer type while `OverlayItem` carries the
individual row values.

---

### Option C — Data-driven `OverlayDef` table  *(evolutionary, lower impact)*

Rather than a trait or rich enum, register each overlay as a `&'static`
descriptor:

```rust
struct OverlayDef {
    /// Trigger words after `/` (case-insensitive).
    triggers:    &'static [&'static str],
    title:       &'static str,
    /// Esc behaviour: clear buffer, or pop to parent prefix.
    esc:         EscBehavior,
    /// Whether the overlay is selectable (false = stat/help info panels).
    selectable:  bool,
}

enum EscBehavior { Clear, PopLastToken, DismissLocal }
```

The variable parts (item generation, Tab/Enter actions) still live as functions
or closures, but the static structural properties move into one table that is
trivially audited.  Adding a new command is a two-step: add a `OverlayDef`
entry and implement the dynamic callbacks.

This is the least invasive option — it can be adopted incrementally alongside
the current code, and the Esc ladder (the most error-prone) is the first thing
it eliminates.

---

### Option D — Split `read_input` into sub-functions by concern

Without changing the data model at all, the current 750-line event loop can be
split into:

```rust
fn compute_overlay(buf: &str, ctx: &ReplCtx, settings: &SettingsStore, ...) -> OverlayState;
fn handle_overlay_key(state: &OverlayState, key: KeyEvent, editor: &mut LineEditor, ...) -> OverlayKeyResult;
```

This does not reduce the number of match arms (they just move to separate
functions) but it isolates the two concerns so they can be read and tested
independently.  A worthwhile first step even if Options A–C are adopted later.

---

## 4. Recommended path

1. **Short term — Option D first:** Extract `compute_overlay` and
   `handle_overlay_key` without changing the data model.  Each function is
   still long but the event loop itself shrinks to ~80 lines and the two
   concerns stop interleaving.  Adds no new abstractions; low risk.

2. **Medium term — Option A:** Introduce `ActiveOverlay` enum.  This is the
   change with the highest payoff: it eliminates double computation, the
   `value_cands` hack, the `help_view` bool, and the three parallel match
   ladders.  The overlay-computation phase becomes testable in pure unit tests
   (no TTY, no event loop).

3. **Opportunistic — shared table builder:** Add a
   `render_two_col_table(rows: &[(&str, &str)], avail: usize) -> Vec<String>`
   free function (and a four-col variant for models) called from
   `display_rows()` in each variant.  Eliminates the repeated Table setup.

4. **Future — Option C / B:** If the command set grows significantly, a
   `OverlayDef` table makes adding slash commands nearly mechanical.  Option B
   (`OverlayItem` typed values) is worth considering if the per-item match arms
   in Tab/Enter remain noisy after Option A.

---

## 5. Consistency guarantees the refactor provides

| Behaviour                                   | Current                            | After Option A                        |
|---------------------------------------------|------------------------------------|---------------------------------------|
| Overlay entries computed once               | No (double or triple)              | Yes — carried in enum variant         |
| Esc defined once per overlay                | No — match ladder, /set special    | Yes — `handle_esc` per variant        |
| Tab/Enter defined once per overlay          | No — two parallel match ladders    | Yes — two methods per variant         |
| New overlay requires edits in N places      | 4+ (compute, Tab, Enter, Esc)      | 1 (add variant + impl methods)        |
| Help overlay treated uniformly              | No — `help_view` bool special-case | Yes — `ActiveOverlay::Help` variant   |
| Compiler checks completeness of key dispatch| No                                 | Yes (match on enum)                   |
| Overlay computation is unit-testable        | Hard (entangled with event loop)   | Yes (`from_buffer` is pure)           |

---

## 6. What should *not* change

- `LineEditor` — already well-factored, pure, and exhaustively tested.  Leave it alone.
- `History` — simple and self-contained.
- `TermGuard` — lifecycle and signal-safety concerns are correctly isolated.
- `overlay_lines` / `overlay_window` / `next_input_row` / `clear_rows_to` —
  pure rendering helpers, well-named, unit-tested.  They remain free functions
  called from `display_rows()`.
- The `redraw` closure API — the way it takes `&[&str]` rows is fine; it just
  gets called with the output of `display_rows()`.
- `emit_commit` — already correctly extracted.
- The vt100 and layout unit tests — they test the right things at the right
  level; keep them.
