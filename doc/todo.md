- [x] `sed` preview tool (`bsed` binary): a preview-only GNU sed that emits
  line-anchored `@@@OLD`/`@@@NEW`/`@@@END` blocks for the `edit` tool, so the
  model never runs `sed -i` on uncommitted work. Design, decisions and the
  corrections the build forced in ./doc/bsed.md. Block-edit core extracted to
  `crates/blockedit`; the tool runs in `crates/bsed` + `src/bin/bsed.rs`.
  - c\ over range support: c\ - text - Replace (change) lines with text.
- [x] bash tool error return: report error code
- [x] restore model on `/session <id> resume` (now via pre-fill, not silent)
- [x] Compaction unification (Phase 7)
  - [x] /compact command — same as `/tree <id> compact` but at current position.
        Resolves `target_id` to the last user message on the active branch and
        commits a canonical `/tree <last_user_id> compact <mode>` line so the
        existing /tree handler is the single entry point. Three modes
        (manual / text-only / llm; `truncate` aliases `manual`); same
        `llm_compact_prompt` setting; same prompt-customization path.
        Backend: the Phase-7 `run_compaction` was replaced by the
        editor-cleanup E3 `generate_compaction` / `apply_compaction` split
        (doc/editor_cleanup.md §6).
  - [ ] `/tree <id> switch --summarize` - use same menu / mechanism as others
        (UI TBD; the old `CompactionTarget::Abandoned` backend hook was
        deleted in editor cleanup E3 — a menu version would add a target
        kind to `generate_compaction`)
        `/tree <id> switch`, `/tree <id> switch llm <prompt>`, `/tree <id> switch simple`
- [x] Non-REPL slash input (in-process / non-tty hosts) — routed through the
      v2.5 menu eval (`Repl::batch_slash`: passive derive + virtual Enter,
      drawing ignored); the four typed arms are deleted
      (doc/editor_cleanup.md §9 E5).
- [ ] `--quiet` with no positional prompt spins: the non-REPL input arm
      (`repl == None`) pairs a `QuietOutput::get_user_input` that always
      returns `""` with `if input.is_empty() { continue }` — a pre-existing
      hot loop (reproduces at `50e4e69`; piped stdin was never a command
      source — one-shots go through `Commands::Input`/the positional prompt).
      Either make `QuietOutput` read lines (batch-friendly: pairs with
      `Repl::batch_slash`) or stop the loop on empty input.
- [ ] Thinking-level restore on `/session <id> resume` (single input field
      currently can only carry one prefill — model or thinking. Bundle both
      into a slug syntax like `/thinking <level> & /model <slug>` or a new
      `/resume-config <slug>` once the need arises.)
- [ ] Address resume stack overflow in debug builds:
  ./doc/resume_stack_overflow_audit.md.
- [ ] table rendering and filtering: dynamic columns
  - Include filter / sort columns in layout (.columns)
  - Less misleading / confusing modifier value auto-complete - headers
    currently say MODIFIER | DESCRIPTION, with the description being just
    "string". Should be "Column values" or similar
  - Quoted string / space support
  - refine match types: fuzzy for string, sort into divider
    - `.sort>column`, `.sort:column` (default)
    - `.column:test` (fuzzy), `.column=test` (exact, needed for `.path=.` for
      example), `.column>2`
  - sorting on string columns: case insensitive by default?
  - fuzzy matching: `test data` very different result from `test_data` -
    surprising

- [x] UI Draw v2 — unified frame composition (REPL + menus + dialogs)
      See: ./doc/ui_draw_v2.md
      - [x] P0: extract real emit path so vt100 tests pin production bytes
      - [x] P1: frame model (compose → allocate → render), input windowing
      - [x] P2: ConsoleOutput DrawCache + apply (scroll-then-paint, partial repaint)
      - [x] P3: REPL redraw on the frame path; $EDITOR DSR wrapper
      - [x] P4: dialogs on the frame path; commit frame for decision line
      - [x] P5: retire overlay_lines, fold next_input_row/clear_rows_to into apply
      - [ ] Cleanups / follow-ups
- [ ] doc/editor_cleanup.md - generalize editor integration
- [ ] tools: safe sed wrapper producing edit input - generic / separate crate?
  - [ ] research: identify patterns to support, existing similar tooling
- [ ] tree view: lowest indent for longest "master" branch
- [ ] model parameter revisit / expansion
  - [ ] temperature support: model level only? move thinking level to model too?
  - [ ] per-turn /thinking overrides, esp. at end of turn message
- [ ] tool errors: split the operator's view from the model's, then unredact — ./doc/path_leak_risks.md
- [ ] edit tool: return best matched text with line numbers (read format) to
  the *model*, so it can construct a follow-up edit; do not show the diff to the model.
