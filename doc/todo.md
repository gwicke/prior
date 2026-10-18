# TODO

NOTE: Keep things tight - normally no more than 1-2 lines per bullet point. More detail -> create
and link a doc/ and/or commit hash

- [ ] `--quiet` with no positional prompt spins: the non-REPL input arm (`repl == None`) pairs
      `Content::quiet`'s empty input source (`InputSource::Empty`, always `""`) with
      `if input.is_empty() { continue }` — a pre-existing hot loop (reproduces at `50e4e69`; piped
      stdin was never a command source — one-shots go through `Commands::Input`/the positional
      prompt). Either make the empty input source read lines (batch-friendly: pairs with
      `Repl::batch_slash`) or stop the loop on empty input.
- [ ] table rendering and filtering: dynamic columns
  - Include filter / sort columns in layout (.columns)
  - [ ] Less misleading / confusing modifier value auto-complete - headers currently say MODIFIER |
        DESCRIPTION, with the description being just "string". Should be "Column values" or similar
  - Quoted string / space support
  - refine match types: fuzzy for string, sort into divider
    - `.sort>column`, `.sort:column` (default)
    - `.column:test` (fuzzy), `.column=test` (exact, needed for `.path=.` for example), `.column>2`
  - sorting on string columns: case insensitive by default?
  - fuzzy matching
    - `test data` very different result from `test_data` - surprising
    - highlighting often char-wise - not helpful. require runs or tight clusters of matches?
- [ ] tool errors: split the operator's view from the model's, then unredact — path_leak_risks.md
      (`edit` done; `read`/`write`/`glob`/`grep`/`list`/`sed` open)
- [x] Status bar context usage stale after compaction — `apply_compaction` drops the last
      completion's report (`last_input_tokens: Option<u64>`); the figure falls back to the on-demand
      estimate (turn-boundary write) until the next completion reports. `/stat` shows the same
      merged figure as the bar.

## Token efficiency

- [ ] Minimize tool context
  - [ ] More aggressive truncation for long `read` / `bash` outputs
  - [ ] Strip failed edits from previous (before last) completion turns
- [ ] Strip reasoning content for older aborted / failed turns once a newer turn completes
      successfully with a final answer.
