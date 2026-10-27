# TODO

NOTE: Keep things tight - normally no more than 1-2 lines per bullet point. More detail -> create
and link a doc/ and/or commit hash

- [x] sandbox R3a/b/c: done (`4576433`): `--unshare-net` unconditional; `network: true` = egress
      through the `prior-egress` shim + SOCKS5 allowlist (`src/egress.rs`). X11 unreachable by
      construction. Spec + handover: `doc/261021_sandbox_env_x11_audit.md`.
- [x] sandbox R7: done. `--new-session` alone left prior's tty inherited on fd 0 (readable/writable
      by a sandboxed command); both bwrap builders now use `.stdin_null()`. Finding 4 in the audit.
- [x] sandbox R8: done. bwrap forwarded every inherited non-`CLOEXEC` fd into the payload (only the
      egress fd was intentional). `seal_inherited_fds` now marks the whole table `CLOEXEC` pre-exec,
      keeping stdio + the egress fd. Finding 5 in the audit. (Requires Linux 5.11+, `close_range`.)
- [x] sandbox R3d: done. `src/seccomp.rs` assembles a 12-insn BPF program denying `AF_VSOCK` +
      `io_uring` (`--seccomp FD`, fresh memfd per spawn, kept through the R8 seal);
      `PR_SET_PDEATHSIG(SIGKILL)` in `shim_main`.
- [x] sandbox R3i: done. The bridge now serves SOCKS5 **and** HTTP proxy on one port (first-byte
      sniff), so `w3m`/`wget` (no SOCKS code) get egress; `HTTP_PROXY`/`HTTPS_PROXY` are `http://`,
      `ALL_PROXY` stays `socks5h://`. Accept loop is now concurrent (was one-at-a-time). R3i in the
      audit.
- [x] sandbox: `/tmp` is a per-session directory. `app_config_dir()/sandbox-tmp/prior-<uuid>`, mode
      0700, bound over `/tmp` (a `/tmp` remap is the base the session dir is created under). Id read
      live at spawn, so `/tmp` follows `/clear` and resume; `PathPolicy` resolves `/tmp` through the
      same resolver. `tmp_size_mb` retired (one-shot runs keep a fixed 256 MB tmpfs). Cleanup at
      `/clear`, `/session <id> delete`, plus a 7-day mtime sweep on startup. Design in
      `doc/sandbox.md` -> "`/tmp` is a per-session directory".
- [ ] tool path relativization: done except manual verification (one single-file turn, one
      multi-file, one rejected blob, `/replay` of each — stored JSONL and next history must both
      show `src/x.rs`). Design: `doc/261026_tool_path_relativization.md`; steps 1–7 done:
      `doc/261026_tool_path_relativization_impl.md`.
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
- [x] settings.toml live reload: `notify` watch re-reads the file on external edits so `/model`/`/set`
      listings and the Ctrl-E dialog are current without a restart. `doc/settings_reload.md`.
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
