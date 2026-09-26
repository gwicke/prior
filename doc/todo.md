- [x] bash tool error return: report error code
- [x] restore model on `/session <id> resume` (now via pre-fill, not silent)
- [x] Compaction unification (Phase 7)
  - [x] /compact command — same as `/tree <id> compact` but at current position.
        Resolves `target_id` to the last user message on the active branch and
        commits a canonical `/tree <last_user_id> compact <mode>` line so the
        existing /tree handler is the single entry point. Shared
        `run_compaction` backend; three modes (truncate / text-only / llm);
        same `llm_compact_prompt` setting; same prompt-customization path.
  - [ ] `/tree <id> switch --summarize` - use same menu / mechanism as others
        (backend ready: `CompactionTarget::Abandoned`; UI TBD)
        `/tree <id> switch`, `/tree <id> switch llm <prompt>`, `/tree <id> switch simple`
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
- [ ] per-turn /thinking overrides, esp. at end of turn message

- [ ] UI Draw v2 — unified frame composition (REPL + menus + dialogs)
      See: ./doc/ui_draw_v2.md
      - [ ] P0: extract real emit path so vt100 tests pin production bytes
      - [ ] P1: frame model (compose → allocate → render), input windowing
      - [ ] P2: ConsoleOutput DrawCache + apply (scroll-then-paint, partial repaint)
      - [ ] P3: REPL redraw on the frame path; $EDITOR DSR wrapper
      - [ ] P4: dialogs on the frame path; commit frame for decision line
      - [ ] P5: retire overlay_lines, fold next_input_row/clear_rows_to into apply

- [x] v2.5 Cleanup Pass 2 — repl.rs visibility reductions + confirmation dialog porting
      See: ./doc/v25_cleanup_pass_2.md
      - [x] Immediate: reduce public API surface in repl.rs (9 items) — `6c70b9f`
      - [x] Phase 1: move plan painter into crate — `927cedc`
      - [x] Phase 2: shared confirm driver in src/ui.rs — `72107ff`
      - [ ] Backlog: Phase 3 (retire overlay_lines) — folded into UI Draw v2 P5
      - [ ] Backlog: Phase 4 (async review_compaction_summary) — superseded by UI Draw v2 P3/P4
