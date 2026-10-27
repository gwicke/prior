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

### `~` arguments

`~`/`~/x` is expanded with the **sandbox** home (`config::sandbox_home`, what
`expand_arg_tilde` and `resolve_sandbox` already use — *not* `env::var("HOME")`;
if an `env` policy ever forces `HOME`, a host-HOME expansion would normalize to a
path the tool never resolved to), then normalized by the rule above. If the
result is not under cwd, the **original `~` spelling is kept**.

- `~/src/prior3/x` → `x` (under cwd: the sub-path wins)
- `~/notes.txt` → `~/notes.txt` (outside cwd: shortest, and already
  HOME-independent)
- `/home/u/notes.txt` → unchanged (absolute, outside cwd)

The spelling kept is the one the tool resolved, because the expansion used the
same sandbox home the tool uses.

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

## The `edit` tool: one array form, one block form

Two representations, two owners, no crossing:

- **canonical array** (`{path, edits:[…]}`, one file per call) → persist, and
  display;
- **block form** (`{path, blocks:"…"}`) → the model's history, only.

The array form is sufficient for display, which is the change that makes this
plan small. `content::preview::get_preview` already renders an `edit` label and
diff from `edits[]`; the only reason a multi-file panel needed blocks was that
`multi_file_pairs` recovered the per-file halves by re-parsing a rendered blob.
Persist's split already produces one array per file, so `get_preview` over those
arrays yields N single-file panels — byte-identical to what the blob round trip
fakes today. `multi_file_blobs`, the blob parse in `multi_file_pairs`, and the
in-crate use of `edit_blocks::per_file_blocks` all go; `per_file_blocks` stays
only for `bsed`.

## One producing function, three consumers

Editing is fast, so the panel can wait for the result. That is the enabler:
`LoggingHook::on_outcome` is the one place that holds *all* of an `edit` call's
inputs — `tool_args`, the host-only `EditResolutions`, and the result text — and
it already runs (and today already records into the ledger) immediately before
rendering the result.

```
prepare_edit_call(args, resolutions: Option<&EditResolutions>, result: Option<&str>, ctx)
    EditArgs::from_wire(args)              // the one parse
    minimize per file (resolutions)        // no-op when absent
    relativize_files(&mut files, ctx)      // the one path rule
    arrays   = files → canonical arrays    // → persist + display
    blocks   = files → render_blob         // → model history
    results  = N-entry result array, paired to arrays by index
```

Called once per `edit` call from `on_outcome`; the returned `PreparedEdit` is
stored in the ledger keyed by tool-call id. Everything downstream is a lookup:

| egress | site | consumes | work left |
| --- | --- | --- | --- |
| live display | `on_outcome` → `display_tool_pair` | prepared arrays + results | render |
| persist | `TurnTranscript::prepare` → `append_messages` | prepared arrays | none |
| model-facing history | `on_completion_call` | prepared blocks, by call id | substitute |

The ledger is the existing `EditAnchorLedger`, widened from
`HashMap<String, EditResolutions>` to `HashMap<String, PreparedEdit>`: the
canonical arrays, the blocks, the paired results, and the derived split ids. It
stays in-run, host-only, and is never serialized.

`prepare_edit_call` with `resolutions = None, result = None` is the fallback for
a call that never reached `on_outcome` (a skipped or denied call, or a turn
rebuilt by `assistant_from_results`, whose arguments are `{}`). It still parses,
relativizes and serializes, so the stored form stays canonical; it just cannot
minimize or split. `prepare` uses it directly for such a call rather than
touching the ledger.

### Display defers, live and replay identically

`display_tool_call` returns early for `edit`, exactly as it does today for a
multi-file call. `on_outcome` then paints the whole call in one go — label,
diff, then result — from `display_tool_pair(name, args, result)`.

Replay can call the same function: `AssistantSegment` already carries each
call's arguments and its result together, so the live and replayed panels come
from one renderer instead of two sequences that have to be kept in step. (The
`replay.rs` module doc explicitly warns that a divergence there is invisible
until two screens are compared; this removes the divergence rather than testing
for it.)

A single-file `edit` therefore no longer paints a label the instant it is
dispatched. The one case that loses a panel is a call that never produces a
result at all — denied, `<Esc>`, or a killed process — which now prints nothing
where today it prints a label. Multi-file already behaves this way, replay still
shows such a call from the stored session, and editing is fast enough that the
delay is not perceptible.

`on_dispatch` keeps its `normalize` call for every other tool (`bash`, `read`,
`write`, `sed`, `grep`, and the retired `ls`/`find`/`mkdir`/`rm`/`mv`/`cp`); for
`edit` it does nothing at all, so `get_preview`'s `canonicalize_edit_value` call
can go. A raw blob the tool rejected is stored untouched; its panel falls back
to the `path` argument (see *Fallbacks*).

## The registry

`normalize` must never grow per-egress copies. One table, one arm per tool:

```
src/tool_args.rs                                   (renamed from src/bash_cmd.rs)
    normalize(name, args: &mut Value, ctx: &PathContext) -> bool   // true if changed
    normalize_in_message(msg: &mut Message, ctx: &PathContext)      // assistant content walk
    normalize_messages(msgs: &mut [Message], ctx: &PathContext)
    render_model_facing(msgs, ledger) -> Vec<Message>   // edit arrays -> block form
```

- `bash` → `strip_cwd_prefix` on `command`;
- `read` | `write` | `sed` | `grep` → `relativize` on `path`;
- `ls` | `find` | `mkdir` | `rm` → `relativize` on `path` (retired tools: no new
  calls, but old sessions replay through the same walk);
- `mv` | `cp` → `relativize` on `src` and `dst`;
- `edit` → not in this table: `prepare_edit_call` owns it, and the only egress
  that sees raw `edit` arguments before it is `on_outcome`;
- everything else → untouched.

Adding a tool, or changing how one argument is spelled, is then a one-line edit
in one file, and every egress follows. `tool_args` deliberately does not know
about `rig` *rendering*; `path_util` stays what it is: pure
`std::path`/string helpers shared with the `bsed` binary, with no `rig` types.

## Fallbacks

**Model-facing history.** The ledger is per-run, so a call from a *prior* session
has no entry. `render_model_facing` handles it without a second parse:

- a stored canonical array → `array_to_blob_value`, **render only**, no parse;
  the stored paths are already relative, so this is a pure format change;
- a raw blob (a call the tool rejected, stored untouched) → `from_wire` →
  `relativize` → `render_blob`. Minimization is identity without resolutions, so
  this is the same function with the optional inputs absent.

**Display.** A stored single-file call renders from its array as today. A stored
raw blob renders its label from the `path` argument and shows no diff — the
existing "malformed, no edits" behaviour, which is the honest rendering.

Nothing here is applied to what is stored: replay reads the session as written.

## What shrinks

`split_multi_file_edit_calls`, `minimize_edit_tool_calls`,
`blob_edit_tool_calls`, `minimized_edit_args`, `split_edit_call_and_result` and
the `strip → blob → minimize → split` composition collapse into
`prepare_edit_call` plus a ledger lookup. The "if you change this order, also
touch …" comments go with them, and `split_id`'s determinism downgrades from a
correctness requirement to a stable-id convention, because one site mints the
ids now.

`Session::canonicalize_edit_tool_calls` and `split_edit_tool_results` also go:
after this change no production caller passes an `edit` call through the
single-message `append_message` path, so `append_message` becomes a serializer,
as `append_messages` already documents itself to be. (`repair_tail_unanswered`
stays — it is pairing repair, not argument rewriting.)

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
- **A deferred panel hides a non-executing call.** A denied or interrupted
  `edit` paints nothing (see *Display defers*). Accepted: multi-file already
  behaves so, editing is fast, and the stored session still shows the call.
- **One producer, later in the run.** `on_outcome` now owns parse, minimize,
  split, relativize and render for `edit`, so a change to the model-facing form
  is a change there — which is the point, but it means the ledger must be
  populated before `prepare` and before `on_completion_call` read it. That holds
  today: `on_outcome` precedes both.
- **Ledger lifetime.** It now holds arrays, blocks and results per `edit` call,
  not just line numbers. Still host-only and dropped at run end, but it grows
  with the run's edit volume.

## Test plan

- `bsed_blockedit::path`: no `./` for a child, `.` for cwd, sibling/absolute
  untouched, `-prefixed` child keeps `./`.
- `bsed_blockedit::wire`: `path:LINE` round trip through `from_wire` →
  `render_blob`.
- `tool_args::normalize`: one call per tool name rewritten; idempotent on a
  second pass; a non-path argument left byte-identical; `~` under cwd
  relativized and `~` outside cwd left as `~` (both via the sandbox home); a
  `-prefixed` child relativized with `./`.
- `prepare_edit_call`: single file; multi-file with a matching result array
  (splits, ids pair, each result rides its own call); multi-file with a
  mismatched or unparsable result (one call, joined, result untouched); a call
  with three files where only one resolved (that file minimized, the others
  verbatim); unparsable blob untouched; arrays and blocks describe the same
  `FileEdit`s; byte-stable across two passes.
- history: the model-facing call is a block whose headers carry relative paths,
  for both a single- and a formerly multi-file call; a stored array renders to
  the same block without re-minimizing; a call with no ledger entry still
  relativizes.
- persist: the stored entry is the canonical array with relative paths, and its
  file list equals the display pair's for the same turn.
- display: a multi-file call renders N single-file panels from arrays,
  byte-identical to N genuine single-file pairs; `get_preview` over normalized
  args renders `read src/x.rs`; a rejected blob renders its label from `path`
  with no diff.
- fixtures still asserting `./`: `src/tools.rs:4714`, `src/agent/transcript.rs`
  result-note fixtures, `crates/bsed-blockedit/src/path.rs` tests, and
  `src/content/handle.rs`'s `{"path":"./test.rs"}` — plus
  `relativize_for_display`'s `strip_prefix("./")`, which becomes a no-op.
