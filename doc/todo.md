# TODO

NOTE: Keep things tight - normally no more than 1-2 lines per bullet point. More detail -> create
and link a doc/ and/or commit hash

- [ ] sandbox R3g: macOS (three Seatbelt rules + a profile generator, reusing `egress::host_allowed`;
      `curl`/`git` honour `ALL_PROXY=socks5h://` for http and https; HTTP-only clients now come
      through R3i's front end instead).
      R4 (Landlock, per-path unix sockets) is independent.
      UDP is deferred; if needed the shape is `pasta`/`slirp4netns` plus in-sandbox nftables,
      not Landlock or host iptables — reasoning in R3h.
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
- [x] tool set: default `security_mode = yolo`; retire `ls`/`find`/`remove`/`make_dir`/`move_file`/
      `copy_file`/`agent_browser` (tree traversal + mutation belong to `bash` in the namespace).
      `auto_allow` is now a whole-command match (`\A(?:…)\z`) — it used to approve
      `^ls -la; rm -rf ~`. Display of retired names kept for old sessions.
- [ ] tool errors: split the operator's view from the model's, then unredact — path_leak_risks.md
      (`edit` done; `read`/`write`/`grep`/`sed` open)

## Width / wrapping

- [ ] `DetailItem::height` paints the detail section just to count rows, then `render` paints it
      again — split once and share (cheap since `split_to_width` is borrowed now).

## Token efficiency

- [ ] Minimize tool context
  - [ ] More aggressive truncation for long `read` / `bash` outputs
  - [ ] Strip failed edits from previous (before last) completion turns

## Tool robustness

- [ ] Read tool does not seem to understand ~/* paths (home dir).
