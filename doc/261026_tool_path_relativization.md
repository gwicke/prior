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
Relative paths also make it easier to move sessions between different checkouts
of the same codebase.

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

`~`-prefixed arguments are first expanded to absolute paths, then relativized
to the cwd. This ensures independence from HOME.

## Anchor suffixes

A path argument may carry `path:LINE`. For `edit` this happens **twice**: the
call-level `path`, and — in the canonical array form — each `edits[i].path`,
which `canonicalize_edit_value` deliberately leaves in place. Both must survive
as a pair, so the primitive gets a thin wrapper that splits the anchor with the
existing `blob::split_line_anchor`, relativizes the path part, and re-attaches
`:LINE`. It lives next to `relativize_path` in `bsed_blockedit::path` and is
re-exported through `src/path_util.rs`.

## Where it is applied

Same policy as `doc/sandbox.md` → "A redundant `cd <cwd> && …` prefix is
stripped": one pure function, applied at every point that observes a call, so
the views cannot disagree. Three points:

1. **Persisted call** — `TurnTranscript::prepare` (`src/agent/transcript.rs`),
   through the same `edit` arm as the other two points (it already parses to
   `EditArgs` before `shape_pair` writes the canonical array).
2. **Model-facing history** — `LoggingHook::on_completion_call`
   (`src/agent/build.rs`).
3. **Live display** — `LoggingHook::on_dispatch`, on `args_json` before
   `display_tool_call`, so `content::preview::get_preview` sees clean arguments
   and can drop its own canonicalize step.

**Session replay is not touched.** We normalize on the way into storage, not on
the way out: an old session keeps whatever it was written with, and that is the
honest record.

Execution needs no rewrite either: `get_path_with` resolves a relative argument
against the same cwd, so the call already means what it says.

## One registry, not three hooks

The three points must never grow their own copies of the rewrite. A single
normalizer owns the whole table:

```
src/tool_args.rs
    normalize(name, args: &mut Value, cwd, home) -> bool    // true if changed
    normalize_in_message(msg: &mut Message, cwd)            // assistant content walk
    normalize_messages(msgs: &mut [Message], cwd)
    render_model_facing(msgs: Vec<Message>)  // edit array -> block form
```

`normalize` is the registry: one `match` on the canonical tool name, one arm per
tool, each arm a call to a pure value-level function.

- `bash` → `strip_cwd_prefix` on `command`;
- `read` | `write` | `sed` | `grep` → `relativize_anchored` on `path`;
- `edit` → `from_wire`, `relativize_files`, `to_canonical_array`;
- everything else → untouched.

The edit rule itself is `relativize_files(&mut [FileEdit], cwd)`: relativize
each `FileEdit.path` through the anchor wrapper. Everything else about `edit` is
parse and serialize:

```
edit arm:  from_wire(args) → relativize_files → to_canonical_array
```

`from_wire` already normalizes every accepted shape (blob, array, single-edit)
into `Vec<FileEdit>`, so the arm is the same parse `prepare_edit_call` does and
the same canonical form persist stores. Redundant per-entry `path:LINE` fields
collapse into the call path there, which is canonical and shorter. A call that
does not parse is returned untouched (unchanged from today's behaviour, where
`canonicalize_edit_value` bails out the same way).

`to_canonical_array` is new: `edits_to_value` exists but is private and takes
`&[ParsedEdit]`, so publish a `FileEdit`-shaped serializer next to it and let
`canonicalize_edit_value` use it for its blob branch too — that branch currently
builds the same JSON inline.

Adding a tool, or changing how one argument is spelled, is then a one-line edit
in one file, and all three locations follow. This is `src/bash_cmd.rs` grown
from one argument into the table it already almost is, so it is renamed
`src/tool_args.rs` with `strip_cwd_prefix` and its tests carried over unchanged.

`tool_args` normalization deliberately does not know about `rig` message
*rendering* — see the next section. `path_util` stays what it is: pure
`std::path`/string helpers shared with the `bsed` binary, with no `rig` types.

## `edit`: parse once, minimize per file, render once

The work unit is a **file**, not a call. `EditArgs::from_wire` already returns
`Vec<FileEdit>` (`from_blob` → `split_by_file`), and `minimized_edit_args`
already minimizes *per file*, looking each one up in `resolutions.files` by
path and keeping an unresolved file's edits verbatim in a mixed call. So
per-file minimization is not a new mode to add; it is what the code does, and
it needs only `FileEdit`s. It does not need the tool result, and therefore has
no degraded case: a multi-file call minimizes exactly like a single-file one.

What today's code does badly is that it *renders* at the end of minimization
(`render_blob` per file, joined into one `blocks` string), so the later
`split_edit_call_and_result` → `per_file_blocks` parses those bytes straight
back with `split_by_file`. That is the re-parse to remove. The split stage does
need the tool result — it pairs one call with N results and refuses unless the
result is an N-entry array — but that guard is orthogonal to minimization.

So: one function, one parse, one render, with the per-file blocks handed to both
consumers.

```
prepare_edit_call(tc, result_text, ledger) -> PreparedEditCall
    args = EditArgs::from_wire(&tc.function.arguments_value())   // the one parse
    files = args.files.map(|f| minimize_file(f, ledger.get(tc.id)))   // per file
    files = files.map(|f| { f.path = relativize_anchored(f.path); f }) // normalize
    blocks = files.map(|f| render_blob(f.path, f.edits))               // the one render
```

- If `result_text` parses as an N-entry `FileResult` array with `N ==
  blocks.len()`, emit N calls with derived ids (`split_id`) and each file's
  result — the existing view transform, now fed the per-file blocks instead of
  re-deriving them.
- Otherwise emit one call carrying `blocks.join("\n\n")` and the result as-is.
- A call whose arguments do not parse returns the original `ToolCall`
  untouched: the tool's own error is the right answer, and rewriting a broken
  blob could contradict it only for the model that wrote it. Nothing here
  patches header lines textually.

Note this replaces `split_multi_file_edit_calls`, `blob_edit_tool_calls` and
`minimized_edit_args` on the history path with one pass; the three-way
`strip → blob → minimize → split` composition and the "order matters" coupling
between them go away. `shape_pair` (persist) uses the same function, so the
stored record and the model view cannot disagree about which files a call
touched.

Rendering on persist is unnecessary: the canonical `{path, edits:[…]}` array is
the stored form, so persist normalizes the parsed files' paths and stores the
array. `append_message`'s `canonicalize_edit_tool_calls` therefore has nothing
left to canonicalize on a call `prepare` just built, and stays a pure
serializer.

Pipeline order per egress:

- history: `strip reasoning → prepare_edit_call (parse → minimize → normalize → render)`
- persist: `prepare → shape_pair (canonical array, normalized paths) → store`
- display: `normalize` on dispatch, then `get_preview`

A multi-file blob that `split_by_file` rejects (a block with no header) fails
`from_wire` and is stored and shown untouched, in whatever shape it arrived.

## Results

The tool already relativizes what it reports (`src/tools.rs`: `write`, `edit`,
`sed`, `grep`), so the result half needs only two fixes:

- drop the `./` from `relativize_path`, so a note reads
  `Replaced 2 block(s) in src/tools.rs`;
- relativize `FileResult::path` in `edit_files_with`, so a multi-file result
  array names files relative too. The `EditAnchorLedger` keeps the raw
  model-spelled path: it is host-only, used solely for matching, and
  minimization runs before normalization, so the two never have to agree.

## Non-goals

- **Rewriting result text.** A `grep` hit line or a `bsed` report quotes what
  the tool saw; rewriting it would corrupt the user's search results.
- **Rewriting stored sessions on read.** Old entries replay exactly as written.
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
- **Pipeline re-cut.** `prepare_edit_call` replaces three transforms
  (`minimize_edit_tool_calls`, `blob_edit_tool_calls`,
  `split_multi_file_edit_calls`) that had to stay in step, and the long "if you
  change this order, also touch …" comments become one function's doc. The
  pairing guard (N results for N files, else no split) moves inside it, so it
  can no longer be skipped by a caller applying the transforms in a different
  order. The tests below are the guard for the rewrite itself.
- **Ledger coupling.** Anything added later that compares a history path
  against a ledger path must compare both sides normalized, since the history
  copy may be relative while the ledger entry is as the model spelled it.

## Test plan

- `bsed_blockedit::path`: no `./` for a child, `.` for cwd, sibling/absolute
  untouched, `-prefixed` child keeps `./`, anchor round trip
  (`a/b.rs:100:5` → `a/b.rs:100` under cwd).
- `tool_args::normalize`: one call per tool name rewritten; idempotent on a
  second pass; a non-path argument left byte-identical; `~` expanded then
  relativized.
- `edit` pipeline (`prepare_edit_call`): single file; multi-file with a
  matching result array (splits, ids pair, each result rides its own call);
  multi-file with a mismatched or unparsable result (one call, joined blocks,
  result untouched); a call with three files where only one resolved
  (the resolved file is minimized, the other two keep their edits verbatim —
  the existing mixed-call rule, now explicitly a per-file rule);
  unparsable blob untouched; per-entry `edits[i].path` relativized with its
  anchor; a call is byte-stable across two passes.
- history: the model-facing call is a block whose headers carry relative
  paths, for both a single- and a formerly multi-file call.
- persist: stored entry is the canonical array with relative paths, and the
  stored call's file list equals the history call's file list for the same
  turn.
- display: `get_preview` over normalized args renders `read src/x.rs`.
