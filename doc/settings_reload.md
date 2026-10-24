# Live reload of `settings.toml`

`settings.toml` is read once at startup (`main.rs` → `Config::parse` →
`SettingsStore::from_agent_config`) and the resulting store is the one durable
in-memory copy inside `AppState`. Everything settings-dependent — `/model`,
`/set`, `/stat`, the Ctrl-E prefill, session resume — reads that copy, so a
hand-edit of the file is invisible until restart.

This document covers the reload path that closes that gap: a `notify` watcher on
the settings file re-reads it after an external edit and installs the fresh
store, so listings and the model-settings dialog are current without a restart.

## Detection

`SettingsWatch` (`src/settings_watch.rs`) arms one `notify` watcher on the
settings file's **parent directory**, non-recursive, filtering events in the
callback to those naming the file. Watching the directory rather than the file
is deliberate: every editor save pattern (truncate+rewrite, temp-file+rename,
delete+recreate) emits an event naming the file inside a directory watch, while
a watch pinned to the file's inode dies on rename. The config dir also holds
`sessions/`, `token/`, `recent_models.txt`, hence the basename filter.

Events of kind `Access` are dropped (`is_content_change`): inotify reports an
`Open` for every `read`, and the reload itself reads the file, so counting an
access would make each reload trigger the next one — an endless reload loop that
starves the REPL (it never gets to repaint). Only `Create`/`Modify`/`Remove`
change contents.

The watcher thread sets an `AtomicBool` and pokes a `tokio::sync::Notify` per
matching event. The flag, not the notify, is the source of truth: notify pokes
may coalesce, but `take_pending` reports "an event arrived since the last check"
regardless, so no wake is lost. A burst from one save collapses into one reload,
and a reload is idempotent — a redundant one re-installs identical state and
costs one file read.

The watch is armed only for a real REPL (`use_repl`). Piped/batch input has no
prompt to wake; a one-shot run is over before an edit could land. Failure to arm
(missing parent dir, inotify limit in a container) degrades to `None`: the caller
never wakes, which is today's restart-to-see-edits behaviour.

## Wiring in the REPL

`Repl::read_input` takes `Option<&SettingsWatch>` and folds it into the key
wait:

1. **Top of the wait loop** — `take_pending()` before dispatching any key. An
   external edit that arrived while we were busy (settling, inside `$EDITOR`,
   during a turn) wins over any key queued with it: reload, then `break None`
   to re-evaluate. The key stays on the channel and lands next iteration against
   the reloaded store.
2. **The wait itself** — a `tokio::select!` with `biased` and the watch arm
   first, so when an edit and a key become ready together the reload wins.
   Without a deadline this replaces the plain `keys.recv().await`; with a prose
   deadline the `tokio::time::timeout` becomes a third `select!` arm. A watch
   wake `break None`s, exactly like the prose wake.

`break None` re-enters the outer loop, whose `settle()` re-runs every open
overlay's `data()` (which reads `with_settings`) and repaints — so an
*already-open* `/model` or `/set` refreshes live, not just the next open.

## Install semantics — `AppState::reload_settings`

The reload takes the same door as the menu writers, so a file edit and the
equivalent in-app edit are indistinguishable downstream:

- **Store replaced.** `/model`, `/set`, `/stat`, and the Ctrl-E prefill read it
  fresh at render/submit time.
- **`models_revision` bumped**, invalidating the `resolved_models` memo, so the
  listing and metadata rebuild on the next paint.
- **Active model's record propagated** to the target config — `routing`,
  `context_length`, `alias_for` — exactly as `update_settings` does, so
  `converge()` rebuilds the agent once when those moved. Params are **not**
  re-seeded: A.6 keeps runtime values for a model already visited.

Deliberately **not** done, matching today's menu semantics:

- Session-scoped live params are untouched (`model_param_map` and the current
  model's live params keep runtime values).
- `default_model` in the file does **not** switch the running model mid-session;
  it affects new sessions and what `/set default_model` shows.
- Scalars (`security_mode`, `show_reasoning`, …) live where `/set` puts them, in
  the store. They take effect on the next rebuild / new session, identically to
  a menu edit — the same parity `set_setting` has (neither touches the live
  `show_reasoning` handle or `config.yolo` post-startup).
- `recipes`, `tool_config` (sandbox / auto-allow), `agent_prompt` stay
  startup-pinned. Hot-reloading sandbox policy mid-session is a separate,
  riskier feature.

## Carried over from memory, not the file

- **CLI-pinned keys.** `main.rs` records which managed keys the CLI or a recipe
  resolved (`--yolo` → `SecurityMode`, `--tool-call-limit` → `ToolCallLimit`,
  `--provider`/`--model` → `DefaultModel`, `--persona` → `DefaultPersona`) and
  hands the list to `run_interactive`, which pins it on `AppState`. A reload
  re-applies the in-session value for each pinned key over the freshly loaded
  file, so an external edit can never clobber an explicit flag for the rest of
  the session.
- **`session_resume_replay_turns`.** Persisted nowhere (`save()` skips it,
  `Config` has no field), so a reload carries it from the pre-reload store.
- **An omitted `default_model`.** The startup store never has an empty
  `default_model`; a reload keeps the session's resolved id rather than blanking
  `/set`'s row when the file omits the key.

## Failure modes

- **Unparseable TOML** (mid-edit is common): the in-memory store is untouched and
  a dim notice is shown. The next save re-signals and reloads.
- **File deleted:** keep in-memory + notice; the next save recreates the file.
- **Self-writes:** a menu `save()` fires the watcher, we reload the file we just
  wrote, identical state re-installs, `models_revision` bumps once, the memo
  rebuilds to identical rows. No user-visible effect, no clobber.

## Latency

Event-driven — no idle `stat()` polling. Inotify delivers within milliseconds;
the reload installs before the next key is dispatched for anything edited before
the keystroke.

The one gap is an edit to the *same model's* `[models.*]` table made while that
model's Ctrl-E fragment is already open in `$EDITOR`. The reader is paused and
the REPL is inside the editor, so the reload cannot run; on accept the record is
written from the snapshot taken at open (`update_settings` →
`apply_model_settings` replaces the whole record, last writer wins) and `save()`
puts it on disk. The external edit to that one record is lost, but the state
stays coherent: the subsequent reload reads the file the accept just wrote.
Edits to other keys, or to other models, survive — the accept only rewrites the
active model's table.
