# Tool path relativization — implementation

Companion to [`261026_tool_path_relativization.md`](261026_tool_path_relativization.md);
the design (what and why) lives there, this is the ordered how. Resume from
the first section not marked DONE.

Ordered so the tree compiles and tests pass at every step. Steps 1–3 are done;
resume at step 4.

### 1. The primitive — DONE

`crates/bsed-blockedit/src/path.rs::relativize_path` drops the `./` for a child
(`render_child` keeps it only for a leading `-`). `relativize_for_display`'s
`strip_prefix("./")` stays, now a no-op, with a comment saying why. Tests
renamed/extended in the same module. **Already committed in the working tree** —
do not redo; re-run `cargo test -p bsed-blockedit path::` to confirm.

### 2. `PathContext` — DONE

Added to `src/tool_args.rs`, with two deviations worth knowing: a
`#[cfg(test)] new(cwd, home)` for explicit contexts, and `cwd` from a new
`PathPolicy::sandbox_cwd(cwd)` accessor rather than reaching into `remap`.
`tools::current_path_policy` is now `pub(crate)`. `tools::expand_arg_tilde` also
delegates to `PathContext::sandbox().home` now, so the tools' `~` and every
egress's `~` are one value.

`cwd` = `PathPolicy::sandbox_cwd(host_cwd)` when a policy exists, else
`current_dir()`. `home` = `config::sandbox_home(cfg)` when a sandbox config
exists, else `env::var_os("HOME")`. This is identity with today's behaviour; the
point is that there is one function to change later.

### 3. `src/bash_cmd.rs` → `src/tool_args.rs` — DONE

Renamed with `git mv`; module doc rewritten to cover both rewrites. The old
`rewrite_bash_calls{,_in}` / `rewrite_in_message` / `rewrite_content` are gone,
replaced by `normalize` / `normalize_in_message` / `normalize_messages` /
`rewrite_messages`. All four egresses are wired: `tools.rs` (execution),
`transcript.rs::prepare` (persist), `session/storage.rs::build_index` (replay,
which now also applies the full `normalize`, after its field renames), and
`build.rs::on_completion_call` (history). `render_model_facing` is **not** written
yet — it lands with step 6.2, once `EditLedger` exists.

Rename the module (`git mv`), update `src/lib.rs` (`pub mod tool_args;`) and
every `crate::bash_cmd::…` caller (`src/agent/transcript.rs:363`,
`src/agent/build.rs:791`, `src/agent/turn.rs` comments). Keep
`strip_cwd_prefix` as-is; the existing tests move with the file.

Add, in this order, each with unit tests alongside:

```
pub(crate) fn normalize(name: &str, args: &mut Value, ctx: &PathContext) -> bool
pub(crate) fn normalize_in_message(msg: &mut Message, ctx: &PathContext)
pub(crate) fn normalize_messages(msgs: &mut [Message], ctx: &PathContext)
pub(crate) fn rewrite_messages(msgs: Vec<Message>, ctx: &PathContext) -> Vec<Message>
pub(crate) fn render_model_facing(msgs: Vec<Message>, ledger: &EditLedger) -> Vec<Message>
```

`normalize` is one `match` on `name.to_lowercase()`: `bash` → `strip_cwd_prefix`
on `command` (existing function, returns `Option<String>`); `read|write|sed|
grep|ls|find|mkdir|rm` → `relativize_arg` on `path`; `mv|cp` → on `src` and
`dst`; `edit` and everything else → `false`. `true` when the value changed, so
callers can skip the clone. It must be idempotent (running it twice is a
byte-level no-op) — that is what makes it safe to call at every egress.

`relativize_arg(arg, ctx) -> Option<String>`:
- `None` for an empty string, and for a path that is neither absolute nor
  `~`-prefixed (already relative — nothing to shorten, and `../x` is a
  non-goal).
- `~`/`~/x`: expand with `ctx.home` (`~` alone → `home`; `~/x` → `home/x`; no
  `home` → `None`, leave the argument alone); absolute otherwise.
- `relativize_path(resolved, &ctx.cwd)`, and if the result is still absolute
  (i.e. not under cwd) return `None` — the original spelling (`~/notes.txt`
  stays `~/notes.txt`, `/home/u/x` stays absolute).

`normalize_in_message` walks `Message::Assistant` content, and for each
`ToolCall` parses `tc.function.arguments_value()`, calls `normalize`, and only
on `true` rebuilds `tc.function = ToolFunction::new(name.clone(), args)`.
(rewrites elsewhere already mutate `tc.function` in place after moving the
`ToolCall` out; do the same so `native` survives.) Non-assistant messages pass through.

`render_model_facing` = `normalize_messages` + the `edit` array→block step;
it replaces `blob_edit_tool_calls` + `minimize_edit_tool_calls` +
`split_multi_file_edit_calls` at the history egress (step 6).

### 4. `prepare_edit_call` — the one producer

New function in `src/agent/transcript.rs` (it owns `EditResolutions`-shaped
state already), signature as in *One producing function*:

```
pub(crate) struct PreparedEdit {
    pub call_id: CallId,             // the original id
    pub files:  Vec<FileEdit>,       // minimized + relativized, host form
    pub arrays: Vec<Value>,          // `{path, edits:[…]}` per file → persist+display
    pub blocks: Vec<String>,         // render_blob per file → model history
    pub results: Option<Vec<String>>,// per file, paired by index, when split
    pub ids: Vec<CallId>,            // split_id per file
}
```

Steps, in this exact order (each depends on the previous):

1. `EditArgs::from_wire(args)` — **the only parse**. On `Err` return a
   `PreparedEdit` carrying the raw args (see step 3's fallback) or `None` for
   "leave the call untouched"; measure which the callers need and pick one.
2. Minimize per file using `resolutions: Option<&EditResolutions>`; the body is
   today's `minimized_edit_args` loop, unchanged in behaviour, but operating on
   `Vec<FileEdit>` and *keeping* `start_line` (it currently rebuilds
   `ParsedEdit`s only to render; here the `FileEdit` is the canonical value).
   A `FileEdit` whose path is not in the resolutions keeps its edits verbatim.
3. `relativize_files(&mut files, ctx)` — `files[i].path =
   relativize_arg(&files[i].path, ctx).unwrap_or(files[i].path)`.
4. `arrays` = one value per file via a new
   `bsed_blockedit::edits_to_canonical_value(path, &[EditOp])` — publish the
   existing private `edits_to_value` with a `path` and `EditOp` input, and make
   `canonicalize_edit_value`'s blob branch call it too, so there is one writer
   of the canonical array.
5. `blocks` = `render_blob(&file.path, &to_parsed(&file.edits))` — a small
   `EditOp` → `ParsedEdit` conversion (mirroring the inverse in
   `array_to_blob_value`). `to_parsed` is the only place that must know both
   types.
6. `results` = `bsed_blockedit::parse_file_results(result_text)` when
   `result_text` is `Some` **and** its length equals `files.len()`; otherwise
   `None` (the pairing guard, now inside the producer). Each result string is
   the entry verbatim (`Value::String(s) => s`, other → `to_string()`), the
   same conversion `split_edit_call_and_result` does today.

`ids` = `(0..n).map(|i| split_id(&call_id, i, n))`.

### 5. Widen the ledger; call the producer from `on_outcome`

`src/agent/transcript.rs`: replace
`pub(crate) type EditAnchorLedger = HashMap<String, EditResolutions>` with

```
pub(crate) struct EditLedger {
    /// `EditResolutions` still, for a call that is not yet prepared.
    resolutions: HashMap<String, EditResolutions>,
    prepared: HashMap<String, PreparedEdit>,
}
```

with `insert_resolutions`, `get_resolutions`, `insert_prepared`, `get_prepared`,
`is_empty`, `clone`. It stays host-only, never serialized, and is still cheap to
clone per hook call (`build.rs` clones it at two sites today — keep that, it is
a snapshot for the duration of one call).

`src/agent/build.rs::on_outcome`: keep the existing resolutions insert, then for
`edit` build the `PathContext` once and
`insert_prepared(prepare_edit_call(&args, Some(&res), Some(&result_text), &ctx))`;
then `content.display_tool_pair("edit", &prepared)` instead of
`display_tool_result`. Non-`edit` tools keep the existing
`display_tool_call`/`display_tool_result` path.

### 6. Consumers, one at a time (compile + test between each)

1. **Persist.** `TurnTranscript::prepare`: delete `minimize_one`,
   `minimize_edit_tool_calls`, `blob_edit_tool_calls`, `shape_pair`,
   `split_edit_call_and_result`, `split_note`, `split_multi_file_edit_calls`.
   For each `edit` call take `ledger.get_prepared(id)`; fall back to
   `prepare_edit_call(&args, None, None, &ctx)`. Emit N calls (`ids[i]` +
   `arrays[i]`) and, when `results.is_some()`, N paired results; otherwise one
   call carrying the joined `blocks` and the untouched original result. Stored
   args are the canonical array — `Session::append_message` then only has to
   serialize (delete `canonicalize_edit_tool_calls` and
   `split_edit_tool_results`, and their tests; `append_messages` is already
   documented as a pure serializer).
2. **Model history.** `on_completion_call` becomes
   `render_model_facing(maybe_strip_reasoning(...), &ledger)` where
   `render_model_facing` = `tool_args::normalize_messages` then, per `edit`
   call: `blocks` from the ledger when present, else `array_to_blob_value` on
   stored args (render, no parse), else `prepare_edit_call(&args, None, None)`
   for a raw legacy blob. A module-level test asserts the resulting call is a
   block whose headers are relative.
3. **Display.** `src/content/handle.rs`: delete `multi_file_blobs`,
   `multi_file_pairs`, the `edit && multi_file_blobs(args).is_some()` early
   return, and the array→blob in `get_preview`. Add
   `display_prepared_edit(&self, prepared: &PreparedEdit)` that loops
   `arrays`/`results` calling `render_call_panel`/`render_result_panel` (one
   pair per file, so N real single-file calls are rendered by the same code),
   and `display_tool_pair(&self, name: &str, args: &Value, result: &str)` for
   replay, which is the single-call case of the same renderer.
   `Content::last_tool` stays (read/write result rendering needs it).
   `get_preview`'s `edit` arm reads `path` + `edits[]` only; a raw blob with no
   resolvable array renders the label from `path` and no diff.
4. **`on_dispatch`.** `build.rs`: `if tool_name == "edit" { return
   DispatchAction::proceed(); }`, and for every other tool
   `tool_args::normalize` before `display_tool_call` — replacing the
   `canonicalize_edit_value` block.
5. **Replay.** `src/agent/replay.rs::render_turn` uses the new pair renderer,
   so live and replay share one path.

### 7. Remaining removals and fixtures

- `src/edit_blocks.rs::per_file_blocks` — keep only if `bsed`/`session` still
  call it; after step 6 nothing in-crate does, so move it to
  `crates/bsed-blockedit` if `bsed` needs it, else delete.
- `crates/bsed-blockedit/src/blob.rs`: doc comments that name
  `minimize_edit_tool_calls` / `per_file_blocks` / `Session::canonicalize…`
  point at deleted code; rewrite them to name `prepare_edit_call` and
  `edits_to_canonical_value`.
- Fixtures asserting `./`: `src/tools.rs` (`success_string_uses_relative_path`),
  `src/agent/transcript.rs` result-note fixtures, `src/content/handle.rs`'s
  `{"path":"./test.rs"}`, `src/session/session.rs` split fixtures (deleted with
  the functions).
- `src/tools.rs::expand_arg_tilde` → `PathContext`-based expansion, or leave
  with a comment pointing at it (it is the tools' own copy; unifying it is
  step 2's payoff, not a blocker).
- `FileResult::path` in `edit_files_with` relativized for the multi-file result
  array (single-file already goes through `edit_file_inner`).

### 8. Verify

```
cargo fmt && cargo clippy --all-targets
cargo test -p bsed-blockedit
cargo test --lib
cargo build --workspace   # `src/tools.rs` tests need the workspace `bsed`
```

Manual: one turn with a single-file `edit`, one multi-file, one rejected blob,
and `/replay` of each; the stored JSONL must show `{"path":"src/x.rs",…}` with
no absolute prefix, and the next request's history must show block headers
spelled the same way.

