# Slash Menu Conventions & Infrastructure

A compact primer for working on picocode's interactive slash-command menus.
Read this before adding, extending, or refactoring any slash menu UI. It
describes **current usage** (verified against `src/repl.rs`, `src/agent.rs`,
`src/slash.rs` at `9e4a481`) and the principles new menus must follow.

Related docs: `doc/tree.md` (the `/tree` overlay), `doc/settings_menu.md`
(the `/set` overlay). This document is the *general* reference they build on.

---

## 1. The core principle — the prompt is the state

**The prompt structure mirrors (and drives) the menu structure.** There is no
hidden "which menu is open" state: the current buffer text *is* the current
menu. Every keystroke re-derives the overlay from the buffer
(`ActiveOverlay::from_buffer(buf, ctx, settings, help_view, path_completions)`,
called at the top of every `read_input` loop pass, `repl.rs:1658`).

Consequences:

- **Promotion is structural.** Typing `/model ` (or Tab-selecting a command)
  opens the model list; typing `/session <name>` *promotes* to that session's
  action menu; `/session <name> delete` promotes to the confirm dialog; a bare
  `/tree <id>` becomes the tree-action menu; `/tree <id> compact` becomes the
  compaction sub-menu. Each level is a token appended to the buffer.
- **The buffer survives Esc.** Esc is a *back up one level* — it rewrites the
  buffer to the parent state (e.g. `SessionAction` Esc → `/session `; `Confirm`
  Esc → `/session <name>`). Only Esc on a *root* menu clears the buffer.
- **Nested menus are visible in the prompt.** Because the menu is spelled out
  in the buffer, the user always sees where they are: `/tree <id> switch`,
  `/session <id> resume 10l`, `/set <key> <value>`.

### 1.1 Canonical vs. alias forms

The committed prompt is always the **canonical** command for the action taken.
Sub-menu option labels may be friendlier than the parsed tokens ("just switch",
"fork truncated"), and the filter text a user types while in a menu is *not*
the committed form (see §4). The `handle_enter` for a leaf option builds the
canonical line and submits it; hand-typed aliases (e.g. `/tree <id>
--summarize` ≡ `/tree <id> switch --summarize`) are accepted by the parser for
backward compatibility but the menus never emit them.

---

## 2. Architecture

| Piece | Location | Role |
| --- | --- | --- |
| `InputAction` enum | `src/repl.rs` (~`236`) | What `read_input` returns: `Submit(String)`, `SetModel`, `SetThinking`, `Quit`, `NavigateTree` (deprecated, see `doc/tree_compaction.md`), … |
| `ReplCtx` | `src/repl.rs` `ReplCtx` | Everything `from_buffer` needs to render overlays without touching the agent directly: provider/model, `recent`, `persona`, `session_usage`, `repo`, `current_session_id`, `branch_messages`, `tree`, `leaf_id`, `context_tokens`. Rebuilt each loop by `build_session_ctx` (`src/agent.rs`). |
| `ActiveOverlay` enum | `src/repl.rs` `ActiveOverlay` | One variant per menu (`Model`, `Session`, `SessionAction`, `Confirm`, `Thinking`, `Settings`, `SettingsVal`, `Persona`, `Sandbox`, `Tree`, …). Carries **pre-computed** data (option rows, targets, header) so Tab/Enter/Esc never re-query the buffer. |
| `from_buffer` | `ActiveOverlay::from_buffer` | Pure-ish: buffer + `ReplCtx` → overlay. Classifies the leading command word, then builds the appropriate variant (with filtering, `active` index, headers). |
| `display_rows` / `title` | `ActiveOverlay` | `(title, options, active_index, wrap_opts)` → the caller runs them through `Repl::overlay_lines` for uniform panel rendering. |
| `handle_tab` | `ActiveOverlay` | Fill a completion into the buffer (stays in input). |
| `handle_enter` | `ActiveOverlay` | Leaf options: `Repl::emit_commit` + `InputAction::Submit(canonical_line)`; non-leaf options: `editor.set(...)` to promote to the next level. Returns `OverlayAction::{Continue, Action}`. |
| `handle_esc` | `ActiveOverlay` | Rewrite the buffer to the parent menu (`editor.set`). |
| `overlay_disc` | `ActiveOverlay` | Discriminant number per variant; the loop resets `sel` to 0 when it changes (so moving between menus doesn't carry the cursor over). |
| `Repl::overlay_lines` | `src/repl.rs` `overlay_lines` | Renders one panel: title row (`BG`), optional non-selectable **header** rows (`BG_HEADER`), then selectable option rows with `> ` marker (selected) / `* ` marker (active-default), `BG_SEL` highlight + cyan on the selected row. |
| `Repl::emit_commit` | `src/repl.rs` `emit_commit` | Redraws an empty editor, `output.display_plain(committed_line)`, optional separator, re-anchors `input_row`, sets the next editor text. **This is how the prompt gets into the session scrollback.** |
| fuzzy module | `src/fuzzy.rs` | `fuzzy_filter` / `fuzzy_filter_bonus` over a candidate list with a scorer/ranker. |

The dispatch loop (`read_input`, `repl.rs:1657-1710`) is uniform for every
menu: compute overlay → reset `sel` if kind changed → `display_rows` →
`overlay_lines` → `redraw` → read one key → route to
`handle_tab`/`handle_enter`/`handle_esc`/`edit_key`. Adding a menu is mostly:
new `ActiveOverlay` variant + arms in `from_buffer`, `display_rows`,
`handle_tab`, `handle_enter`, `handle_esc`, `overlay_disc`, and (if it has a
header) the header-forwarding arm in the render `match`.

---

## 3. Menu structure conventions

### 3.1 Levels & promotion rules

Two menu-promotion archetypes are used today:

- **Flat pickers** (`/model`, `/thinking`, `/persona`, `/sandbox <sub>`):
  typing the command (and a filter) shows one list; Enter on a row commits the
  value. `/model`/`/persona` mark the currently-active item with a green `*`.
- **Two-level accept** (`/session`, `/tree`): Enter on the *list* only
  completes the buffer (`editor.set("/session <name>")`), and the **next**
  `from_buffer` pass promotes to the detail/action menu; Enter on an action
  there commits. This two-step keeps the list keyboard-drivable without forcing
  an accept on the first Enter. (`/session <name> delete` further promotes to a
  `Confirm` dialog — a third level.)

Promotion conditions use **exact** matches on the leading tokens where the
structure matters (`/session <name>` resolves the *session id/name*; `/tree
<id>` resolves the *entry id*), and **fuzzy** matches only for choosing among
*options within a level* (§4).

### 3.2 Option rows

- Option labels are terse imperative nouns/verbs (`resume`, `fork`, `rename`,
  `delete`, `switch`, `compact`, `just switch`, `summarize`, `truncate`).
- A small hint may ride the label where syntax matters, e.g.
  `resume  [N[t|l] replay length]` — never a wall of prose in the option row.
- Contextual info belongs in the **header**, not in options: `SessionAction`
  shows the session's identity/turns/age/parent rows; the `/tree compact`
  sub-menu shows "drops N entries ~Xk tokens / keeps M / written into this
  session" rows (§6).

### 3.3 Titles (`display_rows`)

Title strings follow one shape:

```
<Menu name>  (up/down, Tab/Enter select, Esc back)
```

(`"Models  (up/down or PgUp/PgDn, Enter select, Esc back)"`,
`"Session actions  (up/down, Tab/Enter select, Esc back)"`, …). The title
states the navigation keys exactly; keep it consistent.

---

## 4. Pervasive fuzzy search + completion

- **Fuzzy filtering.** Every option list is filtered by the trailing buffer
  text on every keystroke, using `crate::fuzzy::fuzzy_filter_bonus`. Examples:
  `/model an` narrows the model list; `/persona ex` narrows personas;
  `/sandbox rem` narrows remove-bindings; `/tree <id> co` narrows
  `switch`/`compact`.
- **`active` follows the filter.** The first filtered match is the
  `active`/cursor position; the loop's `sel` continues to navigate the
  *filtered* list.
- **Tab completes.** `handle_tab` fills the selected option's token into the
  buffer *without committing* (so the user can keep typing or down-arrow). For
  two-level commands it just finishes the token that promotes the next menu
  (`/session <name>` Tab; `/session <name> resume` Tab). For value pickers it
  replaces the last token (`replace_last_token`).
- **"Direct correspondence with the slash prompt."** If the user has typed a
  token that *exactly* names an option (`--summarize`, `truncate`, `switch`,
  `resume`, …), that option is **pre-selected** regardless of fuzzy rank — the
  typed prompt *is* the selection, and Enter commits it verbatim (see
  `doc/tree_compaction.md` §5.2 for the canonical-line normalization around
  this).
- **Enter accepts** — and, at the leaf, **Tab also stands in for
  Enter's structural step**: Tab on a list row completes the token that
  promotes (same editor state as Enter on that row, minus the commit).

### 4.1 Dotted-modifier auto-complete (`.sort:`, `.price:`, …)

Some lists accept **`.*` filter modifiers** (e.g. `/model .sort:coding`,
`/model .price:0.5`). These are made discoverable with a data-driven
auto-complete overlay (`ActiveOverlay::Mod`), so the option set stays in one
place:

- **A bare `.` (e.g. `/model .`) lists every modifier** with a one-line
  description, so the option set is always discoverable.
- **Typing `.s` narrows by fuzzy match** to `.sort:`; **Tab/Enter** fills the
  token `.sort:` (staying in the menu for the value step).
- For **enumerated** modifiers (`.sort:`), the overlay then lists the **fixed
  value set** (fuzzy-filtered as you type) — `.sort:most-popular`, `.sort:coding`,
  etc. Choosing one rewrites the token and returns to the list.
- For **free-form numeric** modifiers (`.coding:`, `.price:`, …), a bare `.coding`
  shows the single name as a hint; once `.coding:` is typed the overlay hands
  back to the list so you can type the number.
- A fully-formed `.name:value` returns to the list (nothing left to complete).

The parser (`parse_model_filter`) and the overlay both read the same
`MODEL_MODIFIERS` table, so **adding a modifier is one entry in that table**
(plus, if it targets a new field, a field on `ModelFilter` and a match arm in
`apply_modifier`). Escape from the modifier overlay returns to `/model `.

---

## 5. On final accept: the prompt self-documents

The most important output rule:

> **On final accept, the completed prompt is printed and self-documents what
> was done. Avoid printing extra information in the general case.**

Mechanics:

- The leaf `handle_enter` commits the canonical line through
  `Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false)`
  and returns `InputAction::Submit(line)`. `emit_commit` prints the line into
  the session scrollback, so **the committed prompt is the record of the
  action** — exactly like `/model <model>`, `/session <id> resume`, or (new)
  `/tree <id> switch --summarize`.
- The main loop's command handlers therefore **must not** print their own
  confirmation. Any current `display_plain("/<cmd> … done (…)")` is a
  pre-convention artifact and should be removed when touched (e.g. the old
  `/tree navigated (<id>)` line; see `doc/tree_compaction.md`).
- **Error cases are the exception**: failures still print via
  `display_text` / `display_error` (`Cannot resume session: …`, `Session not
  found: …`). "Done" status is never printed; "failed" is.
- **Informational detail lives in the menu, not the output.** Token counts,
  dropped/kept counts, per-session identity — anything the user might want
  *before* committing — is rendered as non-selectable **header rows** in the
  menu (`BG_HEADER`), the same place `SessionAction` shows its session info.
  After the committed line is printed, nothing informational follows it.
- **Cancel during review → stay in place.** Some commands have a
  **review step**: the node's `prepare()` opens a generated artifact
  (e.g. a compaction summary draft) in `$EDITOR` and returns
  `SubmitResult::Prepared`; the engine's native confirm (`NodeEntry`)
  then gates `submit()` behind Enter / Esc. When the user cancels
  (`Esc`), do **not** print a cancel message or re-seed the prompt —
  the confirm disarms and the menu stays open on the node, so the user
  can re-edit (Ctrl-E re-opens the stashed draft), switch modes, tune a
  parameter, or navigate away, all without extra output. A stale epoch
  or consumed stash rejects `submit()` with a regenerate hint, and the
  next Enter re-runs `prepare()`. This applies to any slash command
  that grows a review/edit step before writing (the pre-E3
  `pending_prefill` re-entry + `CompactionResult` enum were deleted;
  doc/editor_cleanup.md §6).

Why: the scrollback becomes a clean, greppable transcript of *what the user
did* (`/model anthropic:claude-…`, `/session <id> fork`, `/tree <id> switch
--summarize`), with no duplicate chatter, and the detail the user actually
wanted to see was already visible in the menu before they hit Enter.

---

## 6. Headers — the "per-session info" pattern

`overlay_lines` supports a `header: Option<&[String]>` rendered between the
title and the selectable options, in `BG_HEADER` (slightly lighter background,
non-selectable, full-width). Use it for contextual information:

- `SessionAction` builds a `Table` header (session name, turns, age,
  parent/dir) plus detail rows (first prompt, last prompt timestamps) via
  `session_action_overlay_entries` (`repl.rs:2632`).
- `Confirm` uses it for the prompt text (word-wrapped).
- The `/tree compact` sub-menu uses it for what-will-be-dropped / token
  estimates (`doc/tree_compaction.md` §5.2).

Anything informational that would otherwise have to be printed *after* the
action should instead be in this header *before* the action.

---

## 7. Data plumbing for overlays

- Overlays are **read-only consumers** of `ReplCtx`-style snapshots: they never
  touch the `Session`/agent directly. `build_session_ctx` (`src/agent.rs`)
  prebuilds what the overlays need each loop iteration (session tree, leaf,
  branch messages, `context_tokens`…).
- `from_buffer` must be **cheap**: it runs on every keystroke. Precompute
  anything expensive in the loop, not in the overlay; keep per-keystroke work to
  filtering already-in-memory lists.
- Tab/Enter/Esc resolve selections from the **pre-computed** per-variant data
  (`targets`, `names`, `opts`) — never by re-parsing the buffer or
  re-querying the repo.

---

## 8. Checklist for a new slash menu

1. Register the command (`src/slash.rs`: `SlashKind` + registry entry + help
   string) and route it in `from_buffer` (`matches!` list + arm).
2. Implement the overlay as `ActiveOverlay` variants; commit everything the
   menu needs into the variant (no re-queries).
3. Decide the promotion shape: flat picker vs two-level accept vs deeper;
   encode it as buffer tokens (the prompt is the state).
4. Add fuzzy filtering over the option rows; pre-select exact-token matches.
5. `handle_tab` completes the current level; `handle_enter` at the leaf
   commits the canonical line via `emit_commit` + `Submit`; `handle_esc`
   rewrites the buffer to the parent level.
6. Put contextual info in the header; print nothing informational after the
   committed line.
7. Wire `display_rows`, `overlay_disc`, the header rendering arm (`overlay_lines`
   forward with `Some(header)`), and the `title`.
8. Tests: `from_buffer` promotion, filtering/active, pre-selection,
   Tab/Enter/Esc transitions, canonical committed line, no extra inline
   output. If the command has a review step, verify that cancel sets
   `pending_prefill` and the next prompt re-enters the editor with the
   same line.

---

## 9. Quick reference (concrete examples, current code)

| Command | Levels | Commit example | Key helpers |
| --- | --- | --- | --- |
| `/model` | list → filter modifiers → Enter | `/model anthropic:claude-…` → `InputAction::SetModel` | `model_overlay_entries`, `active_entry_id`, `modifier_overlay_entries` (`.` prefix) |
| `/thinking` off/low/med/high | list → Enter | `/thinking high` → `SetThinking` | `thinking_overlay_entries` |
| `/set <key>` | list → value eval | `editor.set("/set <key> ")` promotion; `SettingsVal` value-errors rendered red above title | `parse_settings`, `filtered_value_candidates` |
| `/persona` | list → Enter | `/persona none` → `SetPersona` | `setting_value_candidates` |
| `/session <name>` | list → detail → Enter/Tab | `/session <id> resume [N[t|l]]` → `Submit` | `parse_session_name_after`, `session_action_overlay_entries` |
| `/tree <id>` | list → action → sub-menus → Enter | `/tree <id> switch [--summarize]`, `/tree <id> compact truncate` → `Submit` | `tree_overlay_entries`, trailing-token pre-selection (`doc/tree_compaction.md`) |
| `/sandbox` | subcommand → path completion | `/sandbox add_ro <path>` → `InputAction::Sandbox` | `sandbox_overlay_entries`, `complete_path_prefix_sandbox` |

---

## 10. Anti-behaviors (things that have regressed before)

- Printing a confirmation line after the committed prompt (duplicated
  self-documentation).
- Holding menu state in a `kind: u8` + parallel match ladders instead of the
  enum variants (the old overlay design; `ActiveOverlay` replaced it —
  `doc/repl_refactor_options_sonnet.md`).
- `from_buffer` doing I/O or re-loading sessions per keystroke (the
  `/session` cache exists precisely to avoid this).
- Making Enter on a list level commit when the menu is two-level (breaks the
  drivability / accidental commits).
- Forgetting `overlay_disc` update → stale `sel` across menus.
- Header rows rendered as selectable options (they are `BG_HEADER`, not
  option rows).

---

*Keep this primer honest: when the implementation drifts, update this page and
the concrete examples — new menus are expected to copy the current patterns
verbatim.*