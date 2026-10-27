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
   after `shape_pair` has canonicalized/split. The stored call is already
   `{path, edits:[…]}`, so this is a field write — no parsing.
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
- `edit` → `canonicalize_edit_value`, then relativize the call-level `path` and
  every `edits[i].path`;
- everything else → untouched.

Adding a tool, or changing how one argument is spelled, is then a one-line edit
in one file, and all three locations follow. This is `src/bash_cmd.rs` grown
from one argument into the table it already almost is, so it is renamed
`src/tool_args.rs` with `strip_cwd_prefix` and its tests carried over unchanged.

`ToolArgs` normalization deliberately does not know about `rig` message
*rendering* — see the next section. `path_util` stays what it is: pure
`std::path`/string helpers shared with the `bsed` binary, with no `rig` types.

## `edit`: canonical array in, block form out

The blocking constraint: `minimized_edit_args` today parses the arguments and
re-renders a blob, and `split_multi_file_edit_calls` then parses that blob
again. Path rewriting on top of that would be a third parse of the same bytes.

So the pipeline is re-cut so that **each edit call is parsed once and rendered
once**, with only canonical arrays in between:

| stage | shape |
| --- | --- |
| canonicalize | blob (if any) → canonical `{path, edits:[…]}`, split per file |
| minimize | canonical array → canonical array, anchored via the ledger |
| relativize | canonical array → canonical array |
| render | canonical array → `{blocks}` (history, display) or stored as-is (persist) |

Concretely:

- `split_multi_file_edit_calls` runs **first** and emits canonical per-file
  calls (it already derives the ids; only its output shape changes from
  `{blocks}` to the canonical array). It stays the single blob-parse site, and
  only for calls that really are multi-file.
- `minimize_edit_tool_calls` stops calling `render_blob` and returns the
  minimized canonical array instead. It runs after the split, so it sees one
  file per call; a call that is still multi-file (split could not pair a
  result) is left untouched rather than half-rewritten.
- `blob_edit_tool_calls` becomes the **only** array → block renderer and moves
  to the end of the history pipeline, so what the model reads is always a block
  formatted from the canonical array.
- Persist writes the canonical array directly, so `append_message`'s
  `canonicalize_edit_tool_calls` no longer has to re-canonicalize what
  `prepare` just built; `append_messages` stays a pure serializer.

Pipeline order per egress:

- history: `strip reasoning → normalize (array) → blob render`
- persist: `prepare → shape_pair (canonical) → minimize → normalize → store`
- display: `normalize` on dispatch, then `get_preview`

A blob that does not parse is stored and shown untouched — the tool's own
error is the right answer, and rewriting a broken blob could contradict it only
for the model that wrote it. Nothing here patches header lines textually.

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
- **Pipeline re-cut.** Moving the split ahead of the minimizer and the blob
  render to the end touches the three transforms that must stay in step
  (`shape_pair`, `minimize_edit_tool_calls`, `append_message`). The ordering
  tests below are the guard; the comment blocks that name sibling functions
  need updating in the same commit.
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
- `edit` pipeline: single file, multi-file (still splits, still pairs ids),
  unparsable blob untouched, per-entry `edits[i].path` relativized with its
  anchor; minimized anchored call stays minimized; a call is parsed once
  (assert the round trip is byte-stable across two passes).
- history: the model-facing call is a block whose headers carry relative
  paths, for both a single- and a formerly multi-file call.
- persist: stored entry is the canonical array with relative paths, and a
  reload plus replay renders the same label as the live display.
- display: `get_preview` over normalized args renders `read src/x.rs`.
