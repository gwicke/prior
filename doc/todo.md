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
- [x] `/tree <id> switch summarize` — the `switch` leaf became a sub-menu:
        `cursor` (today's behavior) and `summarize` → `manual` / `text-only` /
        `prompt` (`switch summarize prompt <prompt>` for a custom focus),
        reusing the compaction nodes' generate → $EDITOR → confirm flow. Backend:
        `agent::generate_branch_summary` (pi's spur +
        `BRANCH_SUMMARY_PROMPT`) and `apply_branch_summary` (pi's
        `branchWithSummary`: move the leaf to the target, then append a
        `branch_summary` parented there, so the digest is inline on the branch
        the conversation continues on). `BranchSummary` → user message in
        `extract_messages` (pi `sessionEntryToContextMessages`; the wrapper
        text is ours — `Branch summary:` — matching the house style `78c7e7a`
        set for the compaction wrapper). Same pass added `/tree <id> edit` — pi's user-message
        re-edit (leaf = the prompt's parent + input prefill) as its own row, so
        `switch cursor` keeps its "continue after this prompt" meaning.
        Design + parity record + divergences: `doc/branch_summary.md`.
        (The old typed arm's version — parented at the spur tip with `from_id`
        = the common ancestor — was **not** pi's shape.)
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
- [ ] Address resume stack overflow in debug builds: resume_stack_overflow_audit.md
- [ ] table rendering and filtering: dynamic columns
  - Include filter / sort columns in layout (.columns)
  - [ ] Less misleading / confusing modifier value auto-complete - headers
    currently say MODIFIER | DESCRIPTION, with the description being just
    "string". Should be "Column values" or similar
  - Quoted string / space support
  - refine match types: fuzzy for string, sort into divider
    - `.sort>column`, `.sort:column` (default)
    - `.column:test` (fuzzy), `.column=test` (exact, needed for `.path=.` for
      example), `.column>2`
  - sorting on string columns: case insensitive by default?
  - fuzzy matching
    - `test data` very different result from `test_data` - surprising
    - highlighting often char-wise - not helpful. require runs or tight
      clusters of matches?

- [ ] tool errors: split the operator's view from the model's, then unredact —
  path_leak_risks.md

## Drawing / doc/ui_draw_v2.md
- [x] Input field content loss on key-down
- [ ] U4.4 — the ordering question you answered.
  content write vs. frame update synchronization
- [x] Cancelling a turn when it is being retried does not seem to work
  reliably any more - related to event pumps 
- [ ] lift overlay composition and -windowing into vertical render stack
  - [ ] restore fixed table headers
  - have one place doing all vertical scaling
