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

- [ ] v2.5 Cleanup Pass 2 — repl.rs visibility reductions + confirmation dialog porting
      See: ./doc/v25_cleanup_pass_2.md
      - [ ] Immediate: reduce public API surface in repl.rs (9 items)
      - [ ] Backlog: port tool confirmation dialog to v2.5 menu
      - [ ] Backlog: port compaction review dialog to v2.5 menu
      - [ ] Backlog: retire overlay_lines entirely
