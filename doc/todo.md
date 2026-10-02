# TODO
NOTE: Keep things tight - normally no more than 1-2 lines per bullet point.
More detail -> create and link a doc/ and/or commit hash

- [ ] `--quiet` with no positional prompt spins: the non-REPL input arm
      (`repl == None`) pairs a `QuietOutput::get_user_input` that always
      returns `""` with `if input.is_empty() { continue }` — a pre-existing
      hot loop (reproduces at `50e4e69`; piped stdin was never a command
      source — one-shots go through `Commands::Input`/the positional prompt).
      Either make `QuietOutput` read lines (batch-friendly: pairs with
      `Repl::batch_slash`) or stop the loop on empty input.
- [x] Reasoning level restore on `/session <id> resume`
      `/model <id> activate [off|low|med|high|max]` — the optional level is set
      after the activation seeds the model's own; advertised in the activate
      leaf's `header()`. The resume prefill carries the session's level only
      when activating that model would not reproduce it (`AppState::
      peek_model_reasoning`), else the short `/model <id>` stands.
      Deferred: a level auto-completion dialog on the activate leaf.
- [x] Address resume stack overflow in debug builds: resume_stack_overflow_audit.md
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
  path_leak_risks.md (`edit` done; `read`/`write`/`glob`/`grep`/`list`/`sed` open)
