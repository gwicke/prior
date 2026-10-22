# TODO

NOTE: Keep things tight - normally no more than 1-2 lines per bullet point. More detail -> create
and link a doc/ and/or commit hash

- [ ] sandbox R3/R4: `network: true` becomes an egress bridge (SOCKS5, host:port allowlist) and
      `--unshare-net` stays on always, so X11 is unreachable by construction instead of via a
      Landlock/seccomp ladder. R4 absorbs the Landlock work (per-path unix sockets).
      Spec + handover: `doc/261021_sandbox_env_x11_audit.md` ("Where to start next"). R1/R2 are done.
- [ ] `--quiet` with no positional prompt spins: the non-REPL input arm (`repl == None`) pairs
      `Content::quiet`'s empty input source (`InputSource::Empty`, always `""`) with
      `if input.is_empty() { continue }` — a pre-existing hot loop (reproduces at `50e4e69`; piped
      stdin was never a command source — one-shots go through `Commands::Input`/the positional
      prompt). Either make the empty input source read lines (batch-friendly: pairs with
      `Repl::batch_slash`) or stop the loop on empty input.
- [ ] table rendering and filtering: dynamic columns
  - Include filter / sort columns in layout (.columns) — sort column done
        (doc/slashdot/extra_columns.md); filter columns and a `.columns:` modifier to go
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

## Width / wrapping

- [x] `table::style_units` (fit/truncation) used the old "escape runs to first alpha" parser —
      OSC 8 URLs counted as visible columns there. Replaced: `style_units` and `extract_snippet`'s
      third parser are gone, both rebuilt on `wrap::char_widths`; `fit`/`pad`/`layout`/`view`
      measure with `wrap::width_of` (classify-led ASCII fast path).
- [ ] `DetailItem::height` paints the detail section just to count rows, then `render` paints it
      again — split once and share (cheap since `split_to_width` is borrowed now).
- [x] Bumped `unicode-width` 0.1 -> 0.2 across our crates (0.1 remains in the tree only via
      `termimad`).

## Token efficiency

- [ ] Minimize tool context
  - [ ] More aggressive truncation for long `read` / `bash` outputs
  - [ ] Strip failed edits from previous (before last) completion turns

## Tool robustness

- [ ] Read tool does not seem to understand ~/* paths (home dir).
