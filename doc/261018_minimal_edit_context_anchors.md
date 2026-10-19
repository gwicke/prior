# Minimal edit context and resolved line anchors

*Status: implemented.*

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
`render_blob` (`crates/bsed_blockedit/src/blob.rs:436`) currently drops anchors on render, and
`edits_to_value` (`:467`) drops `startLine` on persist. Both need to preserve it.

## Resolved placement provenance

By the time we rewrite history the file has already changed, so the apply line cannot be recomputed.
Capture it during execution instead.

`apply_to_text` (`crates/bsed_blockedit/src/apply.rs:69`) already computes `matched_lines`.
`src/tools.rs:414` (`edit_file`) currently only uses them for a note. Add a bsed_blockedit minimizer (new
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

A block trimmed this far can shrink further by splitting at long internal unchanged runs; see
"Further minimization" below.

Safe because all edits resolve against the original text, so `matched_lines[i]` is the exact
original apply line. Trimmed spans stay non-overlapping.

An edit that already carried an anchor reuses that anchor as the base rather than the ledger line.
This is what makes re-running the transform idempotent: the trimmed form's anchor is already the
first kept line. Unanchored edits — the ones this feature exists for — use the resolved line.

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

## Further minimization: split across long unchanged runs

*Status: implemented.*

Prefix/suffix trimming only helps at a block's edges. A block touching two distant regions still
carries every unchanged line between them, twice (OLD and NEW). Split it per changed region when
the gap pays for the extra block.

### Algorithm — diff first, minimize each hunk

Diff the **whole** block; prefix/suffix trimming then falls out of the diff instead of being a
separate pass:

- `similar`-diff OLD against NEW into hunks; a hunk is a run of changed lines between unchanged
  runs.
- Drop the leading and trailing unchanged runs (that *is* the prefix/suffix trim), then merge hunks
  whose separating unchanged run is `<= SPLIT_GAP` (`2`: split only above two shared lines).
- Each group is a candidate OLD/NEW, anchored at `base + group_first_old_line`. A pure-insertion
  group borrows one unchanged line from the adjacent gap as OLD context (the parser rejects only an
  empty `@@@OLD`; an empty NEW is a valid deletion).
- Run the existing `minimize_edit` on each candidate, so one function still decides what context
  survives.

### Reuse the existing diff

`bsed_blockedit` already depends on `similar`, and `udiff::unified` (`crates/bsed_blockedit/src/udiff.rs`)
already diffs with `TextDiff::from_slices` rather than `from_lines` because `from_lines` breaks on
a bare `\r`. Keep that rule. The splitter differs in one way: a hunk's ranges index line slices
that keep their terminators (`split_inclusive('\n')`, already in `minimize.rs`), so a group's slices
concatenate back to the original text exactly, CRLF and missing final newline included. Put the hunk
iterator in `udiff.rs` (`hunks(old, new)`), called from `minimize.rs`: one engine, one line rule,
two callers.

### Break-even

An extra block costs its `path:LINE` header, three markers and a separator — roughly 20–25 tokens.
A removed shared line saves twice its own tokens (OLD and NEW), so for ordinary code (~8–10
tokens/line) break-even is about one shared line and a gap of three clears it with margin, including
a split insertion's borrowed context. Hence `SPLIT_GAP = 2`, a named constant.

### Weak-locator floor

*Status: implemented.*

Trimming can leave a block whose OLD is just `}`, `end`, `);`. The line anchor resolves it exactly,
but models are biassed against trusting anchor line numbers and, seeing nothing recognisable, re-read
the edited region to check — which costs far more tokens than the trim saved. So keep adding context
lines until the trimmed OLD is no longer a *weak locator*: fewer than `MIN_LOCATOR_CHARS` (12)
**non-whitespace** characters. Whitespace is excluded because indentation is precisely the part the
model cannot distinguish; `    }` is no more informative than `}`.

Real uniqueness is deliberately not tested: that needs a file read, and the file may have changed
since apply. Length is the only signal this transform can afford, and the anchor already owns
correctness — this is purely about the model trusting the result.

The floor lives in `minimize_edit`, right after the prefix/suffix trim (and after the
keep-one-line rule for insertions):

- while the kept OLD is weak, un-trim one more common line: the preceding line when `prefix > 0`,
  the following line when `suffix > 0`, otherwise nothing;
- when both are available, take the one with more non-whitespace characters (ties go to the
  preceding line, usually the construct being closed).

Expansion is greedy and stops as soon as the combined non-whitespace content clears the threshold,
or when no trimmed lines remain. Biasing toward nearby longer lines spends the extra context where
it best reassures the model. The restored lines are common to OLD and NEW, so the edit still applies
identically and the anchor moves back with them. They are persisted with the block, so the next pass
trims them again and restores the same set: idempotency holds. `split_and_minimize` widens each weak
hunk the same way from the unchanged runs on either side before minimizing, because a candidate
built from the hunk alone has no neighbour to restore; `minimize_edit` re-trims that for a strong
hunk, so only weak hunks pay for it.

Limitation: an op already reduced to a bare `}` in an earlier turn cannot be expanded, because the
context line is gone and there is no file read. New model-authored edits and their persisted form
benefit; historical minimal ops stay minimal. That is monotone and acceptable.

### Shape and safety

`minimize.rs` has `split_and_minimize(edit, base) -> Vec<EditOp>` (plus a batch form); the only
caller that changed is `minimized_edit_args` (`transcript.rs`), mapping one input `EditOp` to `N`
`ParsedEdit`s. The hunk iterator is `udiff::hunks(old, new, split_gap)`, beside `udiff::unified` —
one diff engine, one line rule, two callers. Nothing else moves: the ledger stays indexed by *input* edit (each sub-block's anchor
is `base + offset` inside that edit), `per_file_blocks`/`split_by_file` do not care how many blocks
a file has, and `render_blob` already emits a header per block. Idempotent: sub-blocks are anchored
and `<= SPLIT_GAP` apart, so a second pass trims each to itself and splits nothing. Sub-block OLD
ranges are disjoint and exactly anchored (`d1 = 0`, always accepted), and their union is the
original edit, so a split, reloaded history still describes the same change.

### Success count (resolved)

`Replaced N block(s)` used to count the *submitted* blocks, so a split showed more blocks in the
history than `N`, which reads as a contradiction. It now counts the *post-split* blocks: the note is
built with `split_and_minimize_ops`, the same transform that renders the history, so the number
cannot drift from the visible call. Splitting depends only on the block's own text (not the anchor),
so the count is identical whether computed at execution time or during the history rewrite.

### Tests

- Split only above `SPLIT_GAP`; anchors equal `base + offset`; insertion borrows context, deletion
  has empty NEW.
- A weak OLD keeps expanding into neighbours until strong (preferring the longer line at each step,
  ties preceding), the floor is a fixed point, a strong OLD is not padded, and a weak split hunk
  borrows neighbouring unchanged lines until strong.
- Applying the pieces reproduces applying the whole edit (anchored, disjoint sub-blocks); CRLF and
  no-final-newline inputs survive; a repeat run is a fixed point; multi-file split still pairs
  calls and results.

## Edit preview: real line numbers

*Status: implemented.*

`content/preview.rs::refined_diff_lines` emits `hunk.header()`, which numbers lines *within the
buffer*, not the file. After minimization a block is already minimal, so re-diffing can only echo
it with wrong numbers.

- **Anchored block** (`start_line` present): do not diff. Emit the hunk directly in the shape
  `sed_preview_body` already uses — `@@ -{a},{old_n} +{a},{new_n} @@` with `a = start_line`, then
  `- `/`+ ` lines. Real file numbers by construction; extract one shared formatter so the two
  previews cannot drift. The `[range]` label can then come from the anchor instead of a file scan.
- **Unanchored** (legacy `old`/`new`, replay, headerless blob): keep `refined_diff_lines`; its
  per-buffer numbers are the honest ones without an anchor.

## Rewrite points to keep in sync

Anchors pass through the same representation boundaries multi-file editing already owns
(`doc/batch_editing_v2.md`). Every site must agree. At implementation time, add a comment at each
naming the others ("if you touch this, also touch ...").

Ingress / display:

- `LoggingHook::on_tool_call` canonicalizes before `display_tool_call`. Do **not** minimize there
  (no result exists yet), but the canonicalizer must retain `startLine` so replay and display render
  it.
- `get_preview` canonicalizes its input and must tolerate `startLine`; it and
  `refined_diff_lines` are the display half of "real line numbers" above.
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
