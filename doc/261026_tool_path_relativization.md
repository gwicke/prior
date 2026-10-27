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
- path not under cwd (sibling, `/tmp`, `/usr/...`, anything relative already) →
  unchanged
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

### `~` arguments — the one open decision

Two rules are wanted at once: `~` must not become a host-dependent absolute
path, and two spellings of the same file should normalize to one. Proposed:

1. expand `~`/`~/x` with the **sandbox** home (`config::sandbox_home`, what
   `expand_arg_tilde` and `resolve_sandbox` already use — *not*
   `env::var("HOME")`; if an `env` policy ever forces `HOME`, a host-HOME
   expansion would normalize to a path the tool never resolved to);
2. relativize the result;
3. if it is not under cwd, **keep the original `~` spelling**.

So `~/src/prior3/x` → `x`, `~/notes.txt` stays `~/notes.txt` (shorter than the
expansion, and home-independent), and `/home/u/notes.txt` stays absolute
(unchanged rule). Expanding with the sandbox home is what makes step 3 safe: the
spelling kept is the one the tool resolved.

### `PathContext` — one choke point for cwd and home

`normalize` takes a `PathContext { cwd, home }` built by **one** constructor that
reads the sandbox view: `remap.sandbox_cwd(host_cwd)` and
`config::sandbox_home(cfg)`. Today both collapse to `current_dir()` and `HOME`,
so behaviour is unchanged — but when cwd remapping or a forced `HOME` lands, one
function changes and every egress plus the tools follow. It also absorbs
`src/tools.rs::expand_arg_tilde`, which duplicates `resolve_with_home`'s body.

Arguments are *sandbox-namespace* spellings, so relativizing against the sandbox
cwd is the correct pairing. The current conflation is only invisible because
the mapping is the identity.

## No anchor wrapper

A path argument may carry `path:LINE`. Nothing needs a string-level wrapper for
it:

- `edit` is fully parsed (`EditArgs::from_wire` peels `path:LINE` into
  `EditOp::start_line`, and `render_blob` re-emits it), so relativization runs on
  `FileEdit::path`, which is already anchor-free;
- no other tool takes the `path:LINE` idiom: `read` pages with `offset`, and
  `write`/`grep`/`sed` have no anchor.

The round trip is already tested in `bsed_blockedit` (`from_wire` →
`render_blob`); that test is the regression guard, not a new helper.

## One producing pass, three consumers

The blocking fact: only `TurnTranscript::prepare` (`src/agent/transcript.rs`)
holds an assistant `edit` call, its results, **and** the run's placement ledger
at the same time — and it only runs once the unit is complete (`commit(..,
Ongoing)` refuses an unanswered call). So prepare does the whole `edit` job and
publishes the result; the egresses read it.

```
prepare_unit(assistant, results, anchors, ctx)        src/agent/transcript.rs
    per edit call:
        EditArgs::from_wire(args)          // the one parse
        minimize per file (ledger)         // matches raw model-spelled paths
        relativize_files(&mut files, ctx)  // the one path rule
        arrays = files → canonical arrays  // → persist
        blocks = files → render_blob       // → history + display
```

The carrier is the ledger that already exists, keyed by tool-call id. Widen
`EditAnchorLedger` from `HashMap<String, EditResolutions>` to a run-scoped
`PreparedEdit` record: resolutions (`Option`, as today filled only on success),
the canonical arrays, the per-file blocks, and the derived split ids. It stays
in-run, host-only, never serialized.

| egress | site | consumes | work left |
| --- | --- | --- | --- |
| persist | `prepare` → `append_messages` | canonical arrays | none |
| model-facing history | `on_completion_call` | blocks, by call id | substitute; render the rest |
| live display | `on_outcome` → `display_tool_result` | blocks, by call id | none |
| live call panel | `on_dispatch` → `get_preview` | arguments only | `normalize` only |

`display_tool_result` runs in `on_outcome` *after* the ledger insert in the same
function, so the pairing for a multi-file call is a lookup: the per-file blocks
and the per-file results are both in hand. `content::preview::multi_file_blobs`
and `content::handle::multi_file_pairs` therefore lose their parse of the
rendered blob.

`on_dispatch` cannot ride `prepare` — it fires before execution, so there is no
result to pair and nothing to minimize. It needs path relativization only, which
is the argument-level `normalize` registry, not a second rule. A multi-file call
still renders nothing there and waits for its result.

**What shrinks.** `split_multi_file_edit_calls`, `minimize_edit_tool_calls`,
`blob_edit_tool_calls` and `minimized_edit_args` collapse into `prepare`'s edit
arm; the `strip → blob → minimize → split` composition and its "if you change
this order, also touch …" comments go. `edit_blocks::per_file_blocks` loses its
in-crate production callers and becomes a `bsed`-facing helper. `split_id`'s
determinism stops being a correctness requirement (one site mints the ids now)
and stays only as a stable-id convention.

`Session::canonicalize_edit_tool_calls` and `split_edit_tool_results` also go:
after this change no production caller passes an `edit` call through the
single-message `append_message` path, so `append_message` becomes a serializer
like `append_messages` already documents itself to be. (`repair_tail_unanswered`
stays — it is pairing repair, not argument rewriting.)

## The registry

`normalize` must never grow per-egress copies. One table, one arm per tool:

```
src/tool_args.rs                                   (renamed from src/bash_cmd.rs)
    normalize(name, args: &mut Value, ctx: &PathContext) -> bool   // true if changed
    normalize_in_message(msg: &mut Message, ctx: &PathContext)      // assistant content walk
    normalize_messages(msgs: &mut [Message], ctx: &PathContext)
    render_model_facing(msgs) -> Vec<Message>   // edit arrays -> block form
```

- `bash` → `strip_cwd_prefix` on `command`;
- `read` | `write` | `sed` | `grep` → `relativize` on `path`;
- `ls` | `find` | `mkdir` | `rm` → `relativize` on `path` (retired tools: no new
  calls, but replay normalizes them like the rest);
- `mv` | `cp` → `relativize` on `src` and `dst`;
- `edit` → handled by `prepare` (see below); the arm here is a **no-op on the
  block form** and is not used on any persisted path;
- everything else → untouched.

Adding a tool, or changing how one argument is spelled, is then a one-line edit
in one file, and every egress follows. `tool_args` deliberately does not know
about `rig` *rendering*; `path_util` stays what it is: pure
`std::path`/string helpers shared with the `bsed` binary, with no `rig` types.

## `edit`: parse once, minimize per file, render once, emit blocks

The edit arm always **emits the block form**. A canonical array is one file by
definition (`from_edits_array` refuses two paths in one array), so an array can
never represent a multi-file call — and both display consumers read `blob_text`,
which ignores an array entirely. Rendering per file and joining is the only shape
that serves a single-file and a multi-file call alike, and it is exactly what
today's `multi_file_blobs` input looks like. There is no `to_canonical_array`
step in this arm.

`minimized_edit_args` already minimizes **per file** — it iterates
`parsed.files`, looks each up in `resolutions.files` by path, and keeps an
unresolved file's edits verbatim in a mixed call. So per-file minimization is not
a mode to add; it is the existing rule, and it reads only the ledger. What it does
badly is *render* at the end, so `split_edit_call_and_result` → `per_file_blocks`
→ `parse_blocks` parses those bytes straight back. Keeping `FileEdit`s between the
two steps removes that second parse:

- `blocks` and `arrays` are computed once, from the same `Vec<FileEdit>`;
- the split (N calls with `split_id` ids, paired with the N-entry result array)
  happens here too, guarded by `N == per_file.len()`, so no caller can skip the
  guard by reordering;
- a call whose arguments do not parse is stored and shown untouched — the tool's
  own error is the right answer, and rewriting a broken blob could contradict it
  for the only reader that wrote it. Nothing patches header lines textually.

Redundant per-entry `edits[i].path` fields collapse into the call path in the
array form, which is canonical *and* shorter. The paths on the wire to
`edit_files_with` are unaffected: normalization happens after execution.

### Fallback: a call with no ledger entry

Two cases reach the history walk without a `PreparedEdit`: a call from a *prior*
session (the ledger is per-run), and a call that failed so hard that `on_outcome`
never recorded it. Both fall back to the same function with `resolutions = None`,
so still one parse and one render:

- a stored canonical array → `array_to_blob_value`, **render only**, no parse;
  the stored paths are already relative, so this is a pure format change;
- a raw blob → `from_wire` → `relativize` → `render_blob` (minimization is
  identity without a ledger entry, as today).

Nothing here is applied to what is stored: replay reads the session as written.

## Results

The tool already relativizes what it reports (`src/tools.rs`: `write`, `edit`,
`sed`, `grep`), so the result half needs only two fixes:

- drop the `./` from `relativize_path`, so a note reads
  `Replaced 2 block(s) in src/tools.rs`;
- relativize `FileResult::path` in `edit_files_with`, so a multi-file result
  array names files relative too. The ledger keeps the raw model-spelled path for
  matching, and minimization runs before normalization, so the two never have to
  agree.

## Non-goals

- **Rewriting result text.** A `grep` hit line or a `bsed` report quotes what
  the tool saw; rewriting it would corrupt the user's search results.
- **Rewriting stored sessions.** Old entries replay exactly as written: we
  normalize on the way in, not on the way out.
- **Removing the repeated per-block header.** Bigger saving, but it changes the
  one shape the tool accepts, so it is a format change, not a path change.
- **Relative paths above cwd** (`../other/x`). They obscure where a file lives
  and collide across checkouts; `relativize_path` documents the choice.
- **Rewriting user prompt text.** Only tool calls and tool results are in scope.
- **Session-dir relativization.** `session::session::relativize_path` is a
  second, session-directory-based relativizer (already `./`-free) used by
  `touched_files`. It answers a different question and is left alone; do not
  "fix" one and not the other.

## Risks

- **Session relocation.** A session resumed from a different cwd now replays
  relative paths, which resolve somewhere else. This is the intended
  portability trade (same as the `cd` strip). Sessions are normally resumed on
  checkouts of the same code base, where relative paths continue to work even if
  the checkout location changed. A session resumed in an unrelated project /
  repository normally does not have access to the original locations due to
  sandboxing, so absolute paths would not help anyway.
- **One producer, more coupling to `prepare`.** Prepare now owns minimization,
  splitting and rendering for `edit`, so a change to the model-facing form is a
  change there. That is the intent — the coupling used to be distributed across
  four transforms with comments asking the reader to keep them in step — but it
  means `prepare` needs the `PathContext`, which is rebuilt per call.
- **Display parses at dispatch.** `on_dispatch` still renders a single-file
  `edit` from its arguments, so `get_preview` keeps its
  `canonicalize_edit_value` call. One blob parse on the display path is
  unavoidable pre-execution; the doc's "one render" claim is about the
  model-facing and stored forms, not the live panel.
- **Ledger lifetime.** The ledger now holds rendered blocks for every `edit`
  call in the run, not just line numbers. It is still host-only and dropped at
  run end, but it grows with the run's edit volume rather than its call count.

## Test plan

- `bsed_blockedit::path`: no `./` for a child, `.` for cwd, sibling/absolute
  untouched, `-prefixed` child keeps `./`.
- `bsed_blockedit::wire`: `path:LINE` round trip through `from_wire` →
  `render_blob` (moved/extended from the old anchor-wrapper test).
- `tool_args::normalize`: one call per tool name rewritten; idempotent on a
  second pass; a non-path argument left byte-identical; `~` with the sandbox
  home expanded-then-relativized, and kept as `~` when the target is outside
  cwd; a `-prefixed` child relativized with `./`.
- `prepare` edit arm: single file; multi-file with a matching result array
  (splits, ids pair, each result rides its own call); multi-file with a
  mismatched or unparsable result (one call, joined blocks, result untouched); a
  call with three files where only one resolved (the resolved file is minimized,
  the other two keep their edits verbatim); unparsable blob untouched; `blocks`
  and the canonical arrays describe the same `FileEdit`s; a call is byte-stable
  across two passes.
- history: the model-facing call is a block whose headers carry relative paths,
  for both a single- and a formerly multi-file call; a stored array renders to
  the same block without re-minimizing; a call with no ledger entry still
  relativizes.
- persist: the stored entry is the canonical array with relative paths, and its
  file list equals the history call's file list for the same turn.
- display: `get_preview` over normalized args renders `read src/x.rs`; a
  multi-file call's panel comes from the ledger, byte-identical to N single-file
  pairs.
- fixtures still asserting `./`: `src/tools.rs:4714`, `src/agent/transcript.rs`
  (result-note fixtures), `crates/bsed-blockedit/src/path.rs` tests, and
  `src/content/handle.rs`'s `{"path":"./test.rs"}` — and
  `relativize_for_display`'s `strip_prefix("./")`, which becomes a no-op.
