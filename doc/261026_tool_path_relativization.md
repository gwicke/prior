# Tool path relativization

## Problem

`4821776` made tool arguments resolve `~` and enforce the sandbox namespace. A
side effect is that models now spell paths absolutely
(`edit /home/gabriel/src/prior3/src/tools.rs`). Those paths land in the session,
in the display panel, and in every subsequent request, so one absolute path is
paid for on every turn of the rest of the session — and the repetition teaches
the model to keep writing them.

A path under the working directory carries no information beyond its sub-path:
the system prompt already states the cwd. So for the overwhelming majority of
paths we should store, display and send the sub-path only.

## Rules

One primitive decides, lexically, from a `cwd`:

- path == cwd → `.`
- path under cwd → the sub-path, **with no `./` prefix**: `src/tools.rs`
- path not under cwd (sibling, `/tmp`, `/usr/...`, anything relative already,
  anything `~`-prefixed) → unchanged
- the sub-path would begin with `-` → keep `./` (`./-weird.txt`); otherwise the
  value reads as a flag to `rg`/`bsed`, which take the path as a later argv
  element.

That is `bsed_blockedit::path::relativize_path` with its `./` prefix dropped, so
the change is to that one function plus its call sites and tests (`bsed`'s
own display code included); every consumer wants the short form, so a flag or a
second function would be two spellings of one rule. The lexical (not
`canonicalize`d) comparison is deliberate and already documented there: it is
the same boundary `validate_path` uses, so the two cannot disagree. A symlinked
cwd consequently yields absolute paths — the safe direction.

Only *arguments* and *results that name a file* are touched. Paths embedded in
free text (a `grep` match line, a `bsed` failure report the model wrote itself)
are left alone: rewriting inside content the user searched for would be a lie.

`~`-prefixed arguments are left as `~/x`. They are already short, and expanding
them would make the message depend on `HOME` at replay time.

## Anchor suffixes

An `edit` path argument may carry `path:LINE` (`src/tools.rs:1200`), and a block
header carries `path:LINE` too. Both must survive relativization as a pair, so
the primitive gets a thin wrapper that splits the anchor with the existing
`blob::split_line_anchor`, relativizes the path part, and re-attaches `:LINE`.
It lives next to `relativize_path` in `bsed_blockedit::path` and is re-exported
through `src/path_util.rs`.

## Where it is applied

Same policy as `doc/sandbox.md` → "A redundant `cd <cwd> && …` prefix is
stripped": one pure function, applied at every point that observes a call, so
the views cannot disagree.

1. **Persisted call** — `TurnTranscript::prepare` (`src/agent/transcript.rs`),
   after `shape_pair` has canonicalized/split: the stored call is already
   `{path, edits:[…]}`, so relativizing `path` is a field write, no parsing.
2. **Model-facing history** — `LoggingHook::on_completion_call`
   (`src/agent/build.rs`), as the **last** step of
   `strip → blob → minimize → split → relativize`. Last, because
   `minimize_edit_tool_calls` matches a call against the `EditAnchorLedger` by
   its path and the ledger holds the model-spelled path; relativizing earlier
   would silently stop minimization. At this point every `edit` call is a fresh
   `blocks` blob rendered by `render_blob`, i.e. already the parseable form.
3. **Session replay** — `normalize_args_in_place` (`src/session/storage.rs`),
   so an old session stored with absolute paths reads and redisplays the same
   way after a reload.
4. **Live display** — `LoggingHook::on_dispatch`, on `args_json` before
   `display_tool_call`. `content::preview::get_preview` then shows relative
   paths without knowing about cwd, and stays a pure formatter — which also
   covers the replay path (it renders the args the replay path already
   normalized).

Execution needs no rewrite: `get_path_with` resolves a relative argument
against the same cwd, so the call already means what it says.

## The `edit` blob: parse, never patch

An `edit` call's paths live inside the `blocks` string, once per block
(`@@@OLD` requires a header on every block — see `render_blob`). That repetition
is the single biggest win here.

Do **not** rewrite header lines textually. Instead:

```
parse_blocks(blob) → per ParsedEdit: relativize(header) + keep start_line
                   → render_blob(rel_path, &edits)
```

i.e. exactly the round trip `src/edit_blocks::per_file_blocks` already performs,
reusing `split_by_file` for the multi-file case. Consequences to accept:

- A blob that does not parse is returned untouched — the tool's own error is
  the right answer, and rewriting a broken blob could contradict it.
- A blob with a missing or rejected header is returned untouched, for the same
  reason: `render_blob` would silently drop the model's prose line.
- `render_blob` re-emits the canonical marker order, so a history call written
  new-first comes back canonical. This is a byte change in a case where the
  alternative is a path-aware string patcher; the transform is idempotent and
  the model already sees canonical order everywhere else.
- The `{path, edits:[…]}` array form (from a reloaded session, or after
  `shape_pair`) is handled directly off the `path` field.

Both shapes are one function, `relativize_edit_args(args, cwd) -> Value`, used
by points 1–3 above.

## Results

The tool already relativizes what it reports (`src/tools.rs`: `write`, `edit`,
`sed`, `grep`), so the result half needs only two fixes:

- drop the `./` from `relativize_path`, so a note reads
  `Replaced 2 block(s) in src/tools.rs`;
- relativize `FileResult::path` in `edit_files_with`, so a multi-file result
  array names files relative too. The `EditAnchorLedger` keeps the raw
  model-spelled path: it is host-only, used solely for matching, and must keep
  matching the arguments as the model wrote them.

## Code layout

`src/path_util.rs` is a re-export shim of `bsed_blockedit::path`: pure
`std::path`/string helpers with no knowledge of `rig`. That stays. The anchor
wrapper above belongs there (i.e. in `bsed_blockedit::path`), because `bsed`
needs the same stripping.

What is *not* a path util is the message walk: it needs
`rig_core::message::{Message, AssistantContent, ToolCall}` and must not move
into the agent-free crate. `src/bash_cmd.rs` already owns exactly that walk for
one argument. So:

- rename `src/bash_cmd.rs` → `src/tool_args.rs`, keeping `strip_cwd_prefix`
  and its tests;
- `tool_args::rewrite_in_message(msg, cwd)` / `rewrite_calls(messages)` become
  the single walk over assistant content, dispatching per tool name
  (`bash` → `strip_cwd_prefix`; `read|write|sed|grep` → `path`;
  `edit` → `relativize_edit_args`);
- `normalize_args_in_place(name, args)` in `storage.rs` calls the same
  name-dispatched function on a `serde_json::Value`, so persist, history and
  replay share one table instead of three.

Two walkers over the same message list is the thing to avoid; one walker with a
per-tool rule table is what `bash_cmd` already almost is.

## Non-goals

- **Rewriting result text.** A `grep` hit line or a `bsed` report quotes what
  the tool saw; rewriting it would corrupt the user's search results.
- **Removing the repeated per-block header.** Bigger saving, but it changes the
  one shape the tool accepts, so it is a format change, not a path change.
- **Relative paths above cwd** (`../other/x`). They obscure where a file lives
  and collide across checkouts; `relativize_path` documents the choice.
- **Rewriting user prompt text.** Only tool calls and tool results are in
  scope.

## Risks

- **Session relocation.** A session resumed from a different cwd now replays
  relative paths, which resolve somewhere else. This is the intended
  portability trade (same as the `cd` strip), and a path that is under the
  *old* cwd but not the new one is left absolute, because that is a genuine
  statement of location.
- **Ledger coupling.** Points 1–3 relativize display copies only; the ledger
  and `EditResolutions` keep raw paths. Anything added later that compares a
  history path against a ledger path must normalize both sides.
- **Ordering.** Point 2 must stay last in the history pipeline; moving it
  before `minimize` breaks anchoring, before `split` it would need to re-split.

## Test plan

- `bsed_blockedit::path`: no `./` for a child, `.` for cwd, sibling/absolute
  untouched, `-prefixed` child keeps `./`, anchor round trip
  (`a/b.rs:100:5` → `a/b.rs:100` under cwd).
- `tool_args`: a call per tool name rewritten; idempotent on a second pass; a
  non-path argument left byte-identical.
- `edit` blobs: single file, multi-file (still splits correctly afterwards),
  unparsable (untouched), header-less (untouched).
- history pipeline: a minimized anchored call stays minimized after
  relativization (the ledger test), and the split ids still pair.
- display: `get_preview` over relativized args renders `read src/x.rs`.
