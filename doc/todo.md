# TODO

NOTE: Keep things tight - normally no more than 1-2 lines per bullet point. More detail -> create
and link a doc/ and/or commit hash

- [ ] `--quiet` with no positional prompt spins: the non-REPL input arm (`repl == None`) pairs a
      `QuietOutput::get_user_input` that always returns `""` with `if input.is_empty() { continue }`
      — a pre-existing hot loop (reproduces at `50e4e69`; piped stdin was never a command source —
      one-shots go through `Commands::Input`/the positional prompt). Either make `QuietOutput` read
      lines (batch-friendly: pairs with `Repl::batch_slash`) or stop the loop on empty input.
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

- [ ] padded full-width diff lines from edit tool render wrap & cause rendering artifacts

## Token efficiency

- [ ] Minimize tool context
  - Strip failed edits / replace with placeholder
  - Minimize successful edits: Minimal context (only changed lines), all line anchored to actual
    resolved lines.
  - More aggressive truncation for long `read` / `bash` outputs
- [ ] Strip reasoning content for older aborted / failed turns once a newer turn completes
      successfully with a final answer.
- [ ] Multi-file edits - Error: Provider rejected the request (HTTP 400); An assistant message with
  'tool_calls' must be followed by tool messages responding to each 'tool_call_id'. (insufficient
  tool messages following tool_calls message)
      - 0ef960188195b9cf02c38ba011fc5662a82c2391 was not successful. Drop it, and instead implement
      doc/261013_multi_file_edit_id_continuity.md.
- [x] More robust anchoring in crates/blockedit/src/diagnose.rs diagnose(). Currently uses longest
      common sub-string.

## Cleanup

- [x] `src/agent.rs` refactor — 261002_agent_refactor.md
  - [x] Step 1: move `run_interactive` out to `src/interactive.rs` (§3)
  - [x] Read all of `mod tests`, produce a placement table to review (§6)
  - [x] Step 2: split `agent.rs` into `agent/` — 9 files, one commit each (§3)
  - [x] Step 3: `Interactive` struct, in the new `interactive.rs` (§3)
  - [x] Deferred: `CodeAgent::prompt` — deliberately, not cleanup (§4)
- [ ] audit output, repl
- [ ] output.rs drawing refactor — 261003_output_drawing_refactor.md (+ impl companion)
  - [x] Step 1: extract `render.rs` / `draw.rs` / `ansi.rs` mechanically; Output facade kept (§8.1)
  - [ ] Step 2: `TermIO` — reader, raw mode, DSR, size watch; un-`#[serial]` width tests (§8.2) —
    moves landed (`74e987c`); the type fold landed in 4a (`8cc955d`); the size-query
    fallbacks landed in 4g (`c21be0e`)
  - [x] Step 3: split `content/`; `AgentConfig.content` + `Content` tap seam; content consumers on
    `&Content` (§8.3) — landed 3a–3d (`47122b4`…`cf7b6a8`); frame/input rides `AgentConfig.output`
    until step 4 (follow-ups recorded in the impl doc)
  - [ ] Step 4: fold in `RenderManager` + UI; elements → `Arc<Mutex<…>>`, shared editor (§8.4) —
    sub-commit plan 4a–4h recorded in the impl doc — 4a (`8cc955d`), 4b (`38d9513`), 4c
    (`33dc92c`), 4d (`98bcb85`), 4e (`bce38e7`) landed; 4f landed whole — 4f-a
    (`529f278`, bar formats itself from raw data), 4f-b (`cb01977`, frame path leaves
    `&dyn Output`), 4f-c (`fad8f76`, dims → the watch/manager, the size verbs leave the trait),
    4f-d (`348f7ed`, input collapses to one handle; `TermGuard` holders convert),
    4f-e (`8733d09`+`c7ef4fe`+`786009e`+`2906fb1`, TermIO into Draw: setup rework, writer dies,
    reader-stop suspend, resize both edges),
    4f-f (`cbeceed`+`044c7f9`, Stream + the meta sink; the formatting span unlocks),
    4g (`c21be0e`+`6961d0b`+`e7b17ad`, cursor_row verb dies, entry-DSR on the front-door
    guard, the caret channel reaches the manager);
    4h-a (`ba8d6be`, `slashdot_tui` crate extraction), 4h-b (`ddf5522`+`8f80640`,
    `height(width)` on the trait, one place computes the frame row count), 4h-c
    (`280f654`+`2651b19`, the bar moves into `ui/`), 4h-d
    (`a2ebeff`+`063d993`+`8daa008`, menu views mint their own elements: `Arc` handle
    chain, `RenderElement` impls in `slashdot_menu`, the repl appends the element set),
    4h-e (`14542025`+`744a8dfe`+`25730624`, selection figures live behind a shared
    handle; a cursor move updates in place, changed rows re-mint), 4h-f
    (`08ed747`+`be0e7b4`, the editor goes behind `Arc<Mutex<LineEditor>>` and the input
    element pulls it; `Layout` comes home to repl), 4h-g (`ac92095`+`72a3ac7`+`fb3a793`,
    the flip: `render` composes from the configured stack, membership reconfigured on
    transitions; the bar is always present and pulls usage live); 4h-h next (confirm dialog
    selection state + its place in the configured stack, out-of-crate callers, cleanup)
  - [ ] Step 5: delete `output.rs` — sinks, dead taps, trait; prune `lib.rs` re-exports (§8.5)
