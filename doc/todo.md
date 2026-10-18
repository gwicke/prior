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
- [ ] Status bar context usage stale after compaction - check if fixed after drawing refactor.

## Rendering / drawing

- [x] `/tree` multiple roots / chronological rows / root hues —
      doc/261017_tree_robustness.md (write-side anchor still deferred)
- [x] padded full-width diff lines from edit tool render wrap & cause rendering artifacts —
      fixed by expand_tabs (tabstop 4) before panel width math; see content/text.rs

## Token efficiency

- [ ] Minimize tool context
  - Minimize successful edits: Minimal context (only changed lines), all line anchored to actual
    resolved lines.
  - More aggressive truncation for long `read` / `bash` outputs
  - Strip failed edits from previous (before last) completion turns
- [ ] Strip reasoning content for older aborted / failed turns once a newer turn completes
      successfully with a final answer.
- [ ] Multi-file edits - Error: Provider rejected the request (HTTP 400); An assistant message with
  'tool_calls' must be followed by tool messages responding to each 'tool_call_id'. (insufficient
  tool messages following tool_calls message)
      - 0ef960188195b9cf02c38ba011fc5662a82c2391 was not successful. Drop it, and instead implement
      doc/261013_multi_file_edit_id_continuity.md.
- [x] Fix "text only" compaction strategy to a) include last summary, and b) only add text context
  added since.

## Cleanup

- `src/agent.rs` refactor — 261002_agent_refactor.md
  - [ ] deferred: pad a short row to `term_w` only where draw's per-row entry `EL` proves unreliable
    (design §Content; impl doc)
  - [ ] follow-up: track the line-terminator / width repairs back into content — Render's
        enforcement is a net, not the plan (impl doc)
