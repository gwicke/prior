# TODO

NOTE: Keep things tight - normally no more than 1-2 lines per bullet point. More detail -> create
and link a doc/ and/or commit hash

- [x] Drive rig's `AgentRun` directly: own the loop, retry by re-issuing `next_step`, steers as
      rebuilt user messages. Plan: doc/261030_rig_direct_drive.md. Phases **1–5 done** (`edd10e5`,
      `d10aa8c`, `7b23590`, `e4014f2`, phase 4): the loop is the only path, persistence is a cursor
      over `run.messages()`, tools state their own call shape, `"continue"` is gone — a retry
      re-issues the attempt — and a mid-turn submit is a real user message delivered at the next
      call boundary. Remaining: **6** (cleanup, below).
- [x] Status dot spans the whole turn: `TurnPhase` (`Idle`/`Streaming`/`WaitingRetry`) on
      `AppState`, so a retry back-off no longer greys the bar. Three distinct colours — green
      streaming, amber `38;5;214` waiting, grey idle — chosen in one match in `ui::bar::dot_colour`.
      See doc/261030_rig_direct_drive.md §5c.
- [ ] direct-drive phase 6 cleanup: stale comments naming deleted machinery (`LoggingHook`,
      `TurnTranscript`, `prepare_edit_call`, `TurnPersistence`) in agent/build.rs, agent/turn.rs,
      agent/transcript.rs, session/*, tools.rs, content/preview.rs, tool_args.rs; six broken rustdoc
      links (`cargo doc --document-private-items`); rewrite doc/261026_tool_path_relativization.md +
      `_impl.md`, which still describe `normalize` and `PreparedEdit` as current; decide
      doc/261026_rig_steering.md's fate (superseded except as a rig-facts reference); record the rig
      version-pin decision (§7.1).
- [x] Retain a partial turn across a retry: the text is written to the session, marked as a pending
      turn, and the retried request continues from it. Also resumes on an empty submit.
      doc/261102_partial_turn_retention.md; rig facts pinned in tests/rig_replay_rules.rs.
- [ ] reasoning metadata: a stored `reasoning_levels` override does not reach the pre-calculated
      prior→backend map. `ModelSettings::apply_to_entry` overlays costs/context/indices/description
      and skips `reasoning_levels` / `supports_reasoning`, so a Ctrl-E of either is silently inert
      (`model_catalog.rs` `overlay`). Fixing it properly needs the setting to define a *complete*
      mapping — validation of arbitrary backend spellings, precedence against the catalog, and what
      "unsupported" means for a hand-written list — so it is deferred rather than patched. Until
      then the catalog is the map's only source, and `reasoning_map`'s docs say so.
- [ ] sandbox R3g: macOS (three Seatbelt rules + a profile generator, reusing
      `egress::host_allowed`; `curl`/`git` honour `ALL_PROXY=socks5h://` for http and https;
      HTTP-only clients now come through R3i's front end instead). R4 (Landlock, per-path unix
      sockets) is independent. UDP is deferred; if needed the shape is `pasta`/`slirp4netns` plus
      in-sandbox nftables, not Landlock or host iptables — reasoning in R3h.
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

- [x] `DetailItem::height` paints the detail section just to count rows, then `render` paints it
      again — split once and share (cheap since `split_to_width` is borrowed now).

## Token efficiency

- [ ] Minimize tool context
  - [ ] More aggressive truncation for long `read` / `bash` outputs
  - [ ] Strip failed edits from previous (before last) completion turns

## Tool robustness

- [ ] Read tool does not seem to understand ~/* paths (home dir).
