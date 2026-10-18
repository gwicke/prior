# Minimal edit context and resolved line anchors

*Status: proposed.*

## Goal

Successful `edit` calls are replayed to the model on every later completion. Today we replay the
exact blocks the model wrote: often many unchanged context lines, and no anchor even though we
resolved the patch ourselves. Rewrite successful calls in history so they carry only changed lines,
anchored at the line the edit actually applied.

This is purely a *history* transform. The model-facing tool schema, execution, and visible tool
result are unchanged.

## Where it hooks in

`src/agent/build.rs` already rebuilds history before each completion:

```text
stripped -> blob_edit_tool_calls -> split_multi_file_edit_calls
```

Add a minimization pass here. `src/agent/transcript.rs:183` converts canonical
`{path, edits:[{oldText,newText}]}` back to block form; `:277` splits multi-file calls.
`render_blob` (`crates/blockedit/src/blob.rs:436`) currently drops anchors on render, and
`edits_to_value` (`:467`) drops `startLine` on persist. Both need to preserve it.

## Resolved placement provenance

By the time we rewrite history the file has already changed, so the apply line cannot be recomputed.
Capture it during execution instead.

`apply_to_text` (`crates/blockedit/src/apply.rs:69`) already computes `matched_lines`.
`src/tools.rs:414` (`edit_file`) currently only uses them for a note. Add a blockedit minimizer (new
`minimize.rs`):

```rust
pub fn minimize_edit(edit: &EditOp, resolved_start_line: usize) -> EditOp
pub fn minimize_ops(edits: &[EditOp], matched_lines: &[usize]) -> Vec<EditOp>
```

Algorithm, per edit:

- split OLD and NEW into lines, retaining terminators;
- remove the longest common whole-line prefix `p` and suffix `s`;
- new anchor = `resolved_start_line + p`;
- keep one OLD line if trimming would empty it (pure insertion); an empty `@@@OLD` is rejected today
  (`blob.rs:362`);
- never trim inside a line; leave partial-line edits intact;
- set `EditOp.start_line = Some(anchor)`.

Safe because all edits resolve against the original text, so `matched_lines[i]` is the exact
original apply line. Trimmed spans stay non-overlapping.

## Host-only metadata

`edit_files` gets a metadata-returning variant; on `ApplyOutcome::Applied` blocks it inserts, via
`ToolContext::insert_result`:

```rust
#[derive(Clone)]
pub struct EditResolutions {
    pub files: Vec<FileResolution>, // path + Vec<resolved_start_line>
}
```

`insert_result` is host-only: never sent to the model, never persisted. The visible tool result is
byte-identical.

`LoggingHook::on_tool_result` (`src/agent/build.rs:728`) copies it into the run-scoped scratchpad:

```rust
EditAnchorLedger = HashMap<ToolCallId, EditResolutions>
```

Absent metadata means no rewrite: failed, skipped and non-edit calls pass through.

## History transform

In `on_completion_call`, after blob conversion and alongside the existing split:

```rust
let history = blob_edit_tool_calls(stripped_history);
let history = minimize_edit_tool_calls(history, ledger);
let history = split_multi_file_edit_calls(history);
```

`minimize_edit_tool_calls` parses each assistant `edit` call into blocks
(`per_file_blocks`/`split_by_file`) and, per block, picks the anchor from the existing `startLine`
or the ledger. No known anchor -> leave that block untouched. Otherwise minimize and re-render as
`path:LINE` headers.

The existing split then runs unchanged, but `per_file_blocks` must preserve anchors when rendering
per-file blobs. `render_blob` emits `some/path.rs:142` when `start_line` is set;
`array_to_blob_value` (`:553`) reads `startLine` back into `ParsedEdit.start_line`.

## Rewrite points to keep in sync

Anchors pass through the same representation boundaries multi-file editing already owns
(`doc/batch_editing_v2.md`). Every site must agree. At implementation time, add a comment at each
naming the others ("if you touch this, also touch ...").

Ingress / display:

- `LoggingHook::on_tool_call` canonicalizes before `display_tool_call`. Do **not** minimize there
  (no result exists yet), but the canonicalizer must retain `startLine` so replay and display render
  it.
- `get_preview` canonicalizes its input and must tolerate `startLine`.
- `EditArgs::from_wire` and `normalize_args_in_place` (`src/session/storage.rs`) must accept and
  preserve `startLine` in every input form: blob header, canonical array, legacy `old`/`new`.

Canonicalization and rendering:

- `canonicalize_edit_value` (blob -> canonical) must parse `path:LINE` into `startLine`;
  `edits_to_value` and `array_to_blob_value` must round-trip it.
- `parse_blocks` already produces `ParsedEdit.start_line`; `render_blob` emits it. These are the
  pair `per_file_blocks`/`split_by_file` use, so the split must preserve anchors too.

Egress:

- Model history: `on_completion_call` composes `strip -> blob -> minimize -> split`. Minimize reads
  the ledger; split then renders per-file anchored blobs.
- Disk: `append_message` composes `minimize -> split -> canonicalize`, before
  `rig_message_to_pi_entries`. It uses the same ledger and the shared `split_id`; minimization must
  never change ids.
- Result side: `split_edit_tool_results` / `parse_file_results` are untouched. Results are never
  minimized or anchored.

Execution source:

- `edit_file`/`edit_files` insert `EditResolutions` only for `Applied` blocks;
  `LoggingHook::on_tool_result` moves them into the ledger. These are a pair: a new apply outcome
  must decide whether it has resolved lines.

Do not use `ToolCallAction::Rewrite` or `ToolResultAction::Rewrite` for any of this: both alter
dispatch or permanent history rather than a transient view.

## Persistence

Without persisted anchors, the saving disappears at the next user turn, because `prior` comes back
unanchored and cannot be re-minimized against a changed file. So preserve `startLine` in the
canonical edit array (`edits_to_value`).

Verified against the pi checkout at `~/src/pi`: pi's `replaceEditSchema` is a plain
`Type.Object({oldText, newText})` without `additionalProperties:false`, so TypeBox `Compile.Check`
accepts the extra field; edit application maps only `oldText`/`newText`; session load stores
arguments verbatim. `startLine` is therefore accepted, ignored, and survives session load. Cost: a
few JSON bytes if a session is replayed through pi.

Before `append_message` in `src/agent/turn.rs`, apply the same minimization to `to_commit` using a
ledger shared with `LoggingHook` (same sharing pattern as `TurnTranscript`).

## Properties

- No change to the model-facing call or execution path; only history is rewritten.
- No extra file read, so no read/execute race.
- Failed edits are excluded, because metadata is inserted only on success.
- Idempotent: an already-minimal anchored block re-minimizes to itself.
- Multi-file calls minimize before the existing split; the split renders per-file anchored blobs.

## Tests

- `minimize_edit`: prefix/suffix removal, anchor adjustment, insertion keeps one line, partial-line
  unchanged, CRLF-ish input, no-op.
- Duplicate lines still resolve after minimization (one-line OLD -> zero anchor tolerance).
- Tool metadata: success inserts resolutions, failure inserts none.
- History: successful unanchored call becomes minimal `path:LINE`; stale anchor updates; no metadata
  leaves call untouched; transform is idempotent; multi-file split preserves anchors.
- Persistence: canonical `startLine` reloads and renders `path:LINE`.

## See also

- batch_editing_v2.md: Multi-file edit design doc. Covers all places the edit is transformed
  currently.
