# Batch Editing v2 — Aider-style text edits

*Status: Phase 1 implemented. Phase 2 gated on observed demand.*

## Motivation

The current `edit` tool advertises a nested `edits:[{oldText,newText,...}]`
array (batch variant) plus a top-level single-edit variant via an untagged
`anyOf` schema. Weak tool-calling harnesses mangle the nested array — most
commonly by **stringifying** it (`"edits": "[{...}]"`) — so the call fails
before the model ever sees an error.

Goal: replace the batch-array variant with a **flat string parameter** carrying
one or more Aider-style SEARCH/REPLACE blocks. Flat string parameters are what
these harnesses serialize *to*, so the mangling that breaks the array becomes a
no-op. **Phase 1 stays single-file**; the `path` JSON arg is authoritative and
the parsed blocks normalize into the *existing* canonical
`edits:[{oldText,newText}]` shape, so the execution backend, output path, and
session layer are reused unchanged.

## Two-phase scope

| Phase | Scope | Why |
| --- | --- | --- |
| **1 (the fix)** | Single file. `{path, edits:"<aider blob>"}` → normalize to canonical `{path, edits:[{oldText,newText}]}`. Flat schema, no `anyOf`. | Directly kills the nested-array mangling with minimal surface. Reuses `edit_file` + `render_batch` + display + storage verbatim. |
| **2 (gated)** | Multi-file blobs accepted and **split per file**. The canonical form does *not* move: it stays pi's single-`path` `{path, edits:[{oldText,newText}]}`, one stored call per file. | Only worth it if telemetry shows the model routinely wanting cross-file batches. pi cannot express a multi-file call at all, so "one call per file" *is* the pi-compatible shape, not a deviation from it. See Phase 2 below. |

Rationale for gating Phase 2: the backend is **all-or-nothing** (`src/tools.rs` —
no edits applied if any block fails), so a call of `N` blocks succeeds with
roughly `(1-p)^N`; large batches are heavily penalized and one mismatch discards
the whole call. Multi-file does **not** address the actual failure (JSON
mangling) — single-file text edits do. The gate is measurable with no new
instrumentation: the rejection is a fixed literal in `merge_path`
(`blob.rs:367`) that survives verbatim into the persisted tool result, so a grep
over existing session JSONL counts every model that wanted a cross-file batch.

---

## Phase 1 — single-file text edits

### Wire format

Keep **one** tool (`edit`) but advertise a **flat object with all fields
optional** — no `anyOf`, no nested arrays:

```jsonc
{
  "path":      "string?",   // single-edit variant OR anchor for the text variant
  "oldText":   "string?",
  "newText":   "string?",
  "startLine": "integer?",  // single-edit anchor line (near-line hint)
  "edits":     "string?"    // Aider SEARCH/REPLACE blocks (Phase 1: one file)
}
```

Variant selection stays server-side in `EditArgs::from_wire` (which already
inspects the raw `Value`), so the advertised schema is a plain bag of optional
scalars. Accepted shapes:

1. **Single (OpenAI-style):** `{path, oldText, newText, startLine?}`.
2. **Text blocks:** `{path?, edits:"<aider blob>"}` — Phase 1 single file; a
   block's anchor is a `path:LINE` header, not the top-level `startLine`.
3. **Legacy array (not advertised):** `{path, edits:[...]}` — accepted only so
   old persisted sessions / replayed calls still parse.

`startLine` (1-indexed) is an **anchor**, not a search window — see *Per-block
line anchors* below for the semantics, the per-block header form, shape (B), and
why there is **no `endLine`**. It is a matching aid against ambiguity; it is
**not persisted** (the canonical array and the blob both omit it) and **not
served to the model**.

Example:

```
src/foo.py
<<<<<<< SEARCH
def foo():
    return 1
=======
def foo():
    return 2
>>>>>>> REPLACE

<<<<<<< SEARCH
def bar():
    return 3
=======
def bar():
    return 4
>>>>>>> REPLACE
```

### Parameter name

**`edits`** (a string) — decided. Continuity (the model already associates
`edits` with this tool) and it aligns with the failure mode: the harness that
stringified the array now produces the *correct* type. Alternatives rejected:
`patch` (collides with unified `patch`), `blocks` (novel), `diff` (misleading —
this is not a unified diff).

### Path handling (Phase 1) — lenient parse, strict on conflict

The `path` JSON arg is authoritative, but LLMs that recognize "Aider format"
will inject per-block filename lines. The policy balances familiarity against
data integrity:

- The parser treats the line(s) between the previous block's `>>>>>>> REPLACE`
  (or start of text) and `<<<<<<< SEARCH` as an **optional header**. A header
  line that resolves as a path is captured; other non-marker text is ignored.
- **`path` present, header present, they match** → OK.
- **`path` present, header present, they differ** → **hard error** naming both
  paths. This is the data-integrity guard: never silently write file A when the
  text names file B.
- **`path` present, no header** → OK (use `path`).
- **`path` absent, headers present and all identical** → **infer** `path` from
  the headers (ergonomic win; removes a required arg for Aider-style calls).
- **`path` absent, headers absent** → error ("no path").
- **`path` absent, headers differ** → error: *"multi-file edits are not
  supported in this form; make one `edit` call per file."* (Bridges to Phase 2.)
- **`path` present, *some* blocks have a header and some do not** → each header
  is checked against `path` like any other; a differing one is the conflict
  error. This is the case a naive multi-file split silently gets wrong — see
  Phase 2.

Why not "reject paths entirely": models trained on Aider will emit them, and
failing every such call is hostile — it defeats the purpose of borrowing a
familiar format. Why not "silently ignore conflicting paths": it is a
silent wrong-file write. **Lenient-accept + strict-on-conflict** captures the
familiarity win with no integrity hazard.

Path comparison: normalize both sides with the **existing** path utilities —
`crate::path_util::resolve_normalized(path, &cwd)` (tilde expand → join cwd if
relative → clean `..`/`.`), applied to both the `path` arg and the header path,
then compare the results. This reuses the same normalization the rest of the
codebase relies on for session portability (relative paths within the repo)
rather than inventing a local one. If the normalized forms are not equal, error
(safe direction) rather than guess.

### Grammar (`src/edit_blocks.rs`, new)

```
<<<<<<< SEARCH
<search lines>
=======
<replace lines>
>>>>>>> REPLACE
```

- Markers must appear as their **own line**. First `=======` closes the search
  region; first following `>>>>>>> REPLACE` closes the block.
- A literal marker line inside content is unrepresentable (same limitation as
  Aider) — document it; content needing it must be split or anchored with
  surrounding context.
- Leading/trailing ```` ``` ```` fences around the whole blob are stripped
  (models habitually emit fences).
- Multiple blocks are applied in text order.

Shared parser API (one parser, shared by every site that consumes a blob):

```rust
// src/edit_blocks.rs
pub struct ParsedBlocks {
    /// Some(path) iff every block carries a header and they are all identical.
    pub path: Option<String>,
    /// (old_text, new_text) in text order.
    pub edits: Vec<(String, String)>,
}
pub fn parse_blocks(text: &str) -> Result<ParsedBlocks, ParseError>;
```

Plus a small merge helper — `(json_path: Option<&str>, parsed) -> Result<(String, Vec<EditOp>), String>`
— implementing the path policy above; called by `from_wire` (ingress) and the
canonical normalizers (persist, display) so they can never diverge.

Malformed input (missing `=======`, unterminated block, empty search) yields
the self-correcting error style of the current `from_wire`: echo the offending
fragment + a one-line format reminder.

### Tool result: outcome, not echo

The edit tool result does **not** echo the raw edit parameters, and should not.
Two different "echoes" are worth separating:

- **The tool-result body.** Today it returns `Replaced N block(s) in <path>` on
  success, and a structured `{ui_view, model_view}` failure (with a targeted
  diff of the mismatching region) on error. That is the right shape: the model
  already *has* its own parameters, so re-sending them wastes tokens and adds no
  information. The result only needs to convey the **outcome** — success, or the
  diagnostic needed to fix it. (Error diagnostics legitimately quote the *old
  text* region; that is a diff, not a parameter dump.)
- **The assistant's own tool-call arguments in the transcript.** These are
  re-sent to the model on every completion because the assistant message is part
  of history — this is not something the tool result controls. It means we are
  free to **reformat** the stored arguments (e.g. normalize the text blob to the
  array) without losing anything the model needs: providers require the
  `id`/name pairing, not byte-identical arguments. The raw emitted bytes are not
  a contract.

Net: success/error is sufficient, no echo is required, and the transcript args
may be normalized freely.

### Echo on *invalid* input: elide content, keep structure

The one place raw args *are* echoed is the self-correcting error for
**unparseable input** (`EditArgs::invalid`), which shows the received
parameters so the model can see what it sent. A naive byte-prefix truncation is
the wrong tool there: it chops a blob mid-content and routinely hides the very
marker lines that explain the failure.

So the echo is **structure-preserving** (`truncate_wire_repr` → `elide_value` →
`elide_string`): every long string field has only its *structural* lines kept —
the `<<<<<<<` / `=======` / `>>>>>>>` markers, matched by **prefix** so a
hallucinated `<<<<<<< SEARCH>` still shows — plus one line of context on each
side, with each run of elided lines collapsed to `… (N lines elided) …`. Content
is bulk, and the model already has it; the marker *shape* is what diagnoses the
error. A long string with no markers at all degrades to a plain prefix, and
short fields (a `path`, a short `oldText`) pass through verbatim. The whole
echo is still bounded by `MAX_ECHOED_ARGS`.

Example:

```
{"edits":"<<<<<<< SEARCH\nold line 0\n… (38 lines elided) …\nold line 39\n=======\n…\n>>>>>>> REPLACE","path":"src/foo.rs"}
```

### The normalization win

Because Phase 1 is single-file, the parser output maps **directly** onto the
existing canonical `edits:[{oldText,newText}]`:

- **Execution:** `EditFile::call` → `edit_file(path, edits)` — **unchanged**.
  The backend, `edit_match::diagnose`, and `render_batch` failure envelope are
  reused as-is.
- **Session layer:** the string blob is what the model emitted and what JSONL
  must persist. A shared `normalize_edit_args` gains one branch: when `edits` is
  a **string**, run `parse_blocks` + merge and rewrite to the canonical array.
  Best-effort: on parse failure, leave the raw string so replay degrades
  gracefully instead of erroring. Called on the write path (`append_message`)
  and on the read path (`build_index`, for pre-existing files).
- **Output path:** **unchanged** — `get_preview` only ever sees the canonical
  array. The parser is kept out of `output.rs` entirely; the raw string is
  canonicalized before it reaches `display_tool_call` (see *Canonical
  normalization* below).

Net effect: **one** new parser, **one** merge helper; the backend, failure
envelope, and session canonical shape are untouched.

### Canonical normalization (verified against rig rev `1c2f440`)

The goal is to keep the custom parser **out of `output.rs`** and, more broadly,
to establish a single **canonical internal form** (the pi array) that persist and
render share. Normalize the args at each boundary where they enter picocode's
internal representation, so every downstream consumer (`get_preview`,
`render_turn`, the session file) sees the same shape. (The **model-facing** form
is a separate, deliberate divergence — the blob — handled in *What the model
sees* below.)

First, a negative result that rules out the obvious hook:

- **`ToolCallAction::Rewrite` does *not* work for this.** In `runner.rs`
  (`run_single_tool`) a tool-call rewrite changes the **dispatch args** and the
  `ToolExecutionCommitted` stream event only. The persisted transcript comes
  from elsewhere: `run.rs` sets `TurnState.items = turn.choice.clone()` (the
  model's **raw** provider output) and pushes `Message::Assistant { content:
  items.clone() }`. So a rewrite never reaches `resp.messages()` /
  `final_history` and never reaches the session. Do not use it here.

The raw string reaches the internal surfaces at three points, so normalization to
canonical happens at three call sites — **one shared `normalize_edit_args` fn**,
called from each:

| Path | Where | Notes |
| --- | --- | --- |
| Live display | `LoggingHook::on_tool_call` (`agent.rs:1741`) | Fires **mid-turn**, before the assistant message is appended. Normalize the parsed `Value` *before* `display_tool_call`. This is what keeps the parser out of `output.rs`. |
| Disk JSONL + in-memory index | `Session::append_message` (`session.rs:897`) | Rewrite the assistant `edit` tool-call's args **before** `rig_message_to_pi_entries`, so both the file on disk and `index.entries` are canonical. |
| Load / reload | `build_index` → `normalize_entry_in_place` (`storage.rs:275`) | Already exists; kept for pre-existing files. |

Why normalize the **message** (before `rig_message_to_pi_entries`) rather than
only the in-memory index: **pi reads picocode's files**, so the on-disk form must
already be pi-canonical. This deliberately overrides the codebase's default
"normalize on read, keep JSONL raw" preference (`storage.rs:273`) — that
preference assumed picocode was the only reader. It is not: pi is a second
reader, so the write path must emit the canonical shape.

### What the model sees: always the blob

There are three distinct representations, with clear owners:

| Representation | Form | Owner |
| --- | --- | --- |
| **Emitted** | blob, pi array, or legacy `old`/`new` | the model; ingress only |
| **Canonical internal** | pi array `{path, edits:[{oldText,newText}]}` | persist + render |
| **Model-facing** | blob (`path` + SEARCH/REPLACE blocks) | history sent to the model |

The **model-facing** form is the blob, **consistently** — every message in the
history a completion call sends, not just cross-turn. Rationale: the model
imitates the shape of the tool calls it sees, so a history full of the canonical
array would nudge it back toward the exact form the weak harness mangles. The
blob is the form we want it to keep emitting.

**Placement: `LoggingHook::on_completion_call`, made unconditional.** This hook
is the one place that sees the *complete* request history (`CompletionCall`
carries `history: &[Message]`; a `RequestPatch::history` **replaces** the prior
messages per `prepare.rs`), so a single transform there covers prior *and*
within-turn messages — the current `on_tool_call`/`append_message` sites cannot,
because they run before the within-turn assistant message exists. Today the hook
returns `Continue` when `reasoning_in_context`; change it to **always** build the
full history and return `Patch(history(transformed))`:

```rust
// agent.rs on_completion_call
let mut history: Vec<Message> =
    maybe_strip_reasoning(event.history.iter().cloned().collect(),
                          self.reasoning_in_context, &self.provider, &self.model);
history = edit_args_to_model_form(history); // array -> blob, idempotent
if let Some(logger) = &self.message_logger { /* log `history` (what is sent) */ }
CompletionCallAction::Patch(RequestPatch::new().history(history))
```

This is semantically identical to today for every message without an `edit`
tool call (the patch replaces history with itself), so it is a no-op except for
edit calls. Note the default path **already** patches: `reasoning_in_context`
defaults to `false` (`main.rs:259`), so the `Continue` branch is only reached
when reasoning is kept in context; making the patch unconditional changes only
that case. The transform touches only the `arguments` `Value` of `edit`
tool-call parts; `id`/name/result pairing is untouched, so provider replay
contracts hold, and prompt caching is provider-side/automatic (position-based),
not driven by the patch, so it is unaffected; the blob is deterministic from the
stored array, so cache prefixes stay stable.

Why not transform at context build (`extract_messages`): that covers cross-turn
only, leaving within-turn messages as whatever the model emitted this turn —
not the "consistently" the goal requires. The completion hook is the only site
that sees the whole request.

**Disk consequence.** The stored args are the canonical array, so the JSONL is
not byte-identical to the model's emitted form. `--log-messages` captures the
sent/received messages separately if an audit trail is wanted. This is the
accepted cost of pi-compat.

### Pi compatibility and the egress points

pi's canonical `edit` call is `{path, edits:[{oldText,newText}]}` — an *array* of
`oldText`/`newText` entries (multi-entry), under a **single** `path`
(`packages/coding-agent/src/core/tools/edit.ts`; there is no per-edit `path`, and
`applyEditsToNormalizedContent` writes one file per call). picocode's canonical
in-memory form already matches this (`normalize_args_in_place` folds legacy
`old`/`new` into the array). So "pi compatibility" here means one concrete
thing: **every model form (the new Aider text blob, the legacy array, or a
legacy `old`/`new`) must be written to disk as that array**, or pi cannot read
the file. The single-`path` part is load-bearing for multi-file: pi cannot
represent a cross-file batch at all, which is why Phase 2 splits into per-file
calls instead of adopting a path-per-edit canonical form.

Two clarifications that matter:

- **Do not reduce a multi-block edit to a single `{oldText,newText}`.** pi
  *supports* an array; collapsing to one entry is lossy and *less* compatible.
  "One entry per block" (i.e. the array) is the correct target. Only if the
  intent is "one *tool call* per block" does this need splitting a single
  assistant call into N calls + N results — a lossy, invasive transcript
  rewrite; avoid it.
- **Parse validity ≠ match validity.** Normalization only needs the blob to be
  *well-formed* (markers present); it does **not** need the `oldText` to match
  the file. So a *failed* edit (old text not found) still has a parseable blob
  and **must** be normalized for storage; only a *malformed* blob (bad markers)
  cannot be, and is stored raw (pi renders raw args — graceful degradation).

**There is no single choke point.** The assistant message carrying the raw args
only materialises at `FinalResponse` (turn end); the mid-turn events that do
carry args — the `on_tool_call` event and the `ToolExecutionCommitted` effective
call — are **not** what gets persisted. So the representation must be normalized
at each *egress*, all via one shared `normalize_edit_args()` helper:

| Egress | Site | Transform |
| --- | --- | --- |
| Disk JSONL + in-memory index | `Session::append_message`, rewrite assistant tool-call args before `rig_message_to_pi_entries` | → canonical array (pi reads the file) |
| Live display | `LoggingHook::on_tool_call`, before `display_tool_call` | → canonical array (so `get_preview` stays array-only) |
| Model history | `LoggingHook::on_completion_call`, whole history, every call | → blob (see *What the model sees* above) |
| Legacy files on disk | `build_index` → `normalize_entry_in_place` (exists) | → canonical array |

The first two and the last all use the same `normalize_edit_args` (→ array);
model history uses the inverse `edit_args_to_model_form` (→ blob). Persist and
render share one direction; the model gets the other.

Cross-turn and within-turn model context are both the blob (the completion hook
transforms the whole history every call). See *What the model sees* above.

What it takes:

1. `normalize_edit_args(&Value) -> Value` (→ array), best-effort and
   **idempotent** (array form passes through; blob and legacy `old`/`new` fold
   into the array; unparseable blob returned as-is).
2. `edit_args_to_model_form(Vec<Message>) -> Vec<Message>` (→ blob), idempotent
   (non-`edit` calls and already-blob args pass through). Blob layout: the path
   as a header line above the first block, then one
   `<<<<<<< SEARCH\n<old>\n=======\n<new>\n>>>>>>> REPLACE` per block, blocks
   separated by a blank line. The anchor is not part of the blob, so the
   rendered header is a bare path (no `:LINE`). The path is used as stored (the
   model's own spelling).
3. Call `normalize_edit_args` at the persist + display + `build_index` sites;
   call `edit_args_to_model_form` in `on_completion_call`. Execution keeps using
   `EditArgs::from_wire` (ingress).
4. Tests: file on disk is pi-canonical after a blob-form edit; live display
   canonical; re-load canonical; model history is blob for both a within-turn
   and a cross-turn call; unparseable blob degrades without error.

Risks / trade-offs:

- **Disk is no longer byte-identical to the model output.** This is the
  necessary cost of pi-compat (pi reads the file). The raw emitted form is not
  retained; `--log-messages` already captures the sent/received messages
  separately if an audit trail is wanted.
- **Drift risk**: the array↔blob transforms must be used at every egress; a
  divergent site makes live display, replay, and model context disagree.
  Mitigated by two shared functions.
- **Provider safety**: only the `arguments` `Value` changes; tool-call `id` and
  name and result pairing are untouched, so Anthropic/OpenAI replay contracts
  hold.

### Files to change (Phase 1)

- `src/edit_blocks.rs` (new): parser (`parse_blocks`/`merge_path`) +
  `canonicalize_edit_value` (→ array) + `array_to_blob_value`/`render_blob`
  (→ blob) + unit tests.
- `src/tools.rs`: replace `EditArgs`/`EditArgsVariant` with a flat struct;
  variant selection in `from_wire`; call `parse_blocks` for the string form;
  hand-written flat `parameters()`; update `EditFile::description` and
  `EDIT_ARGS_EXAMPLE`.
- `src/output.rs`: **unchanged** (parser stays out).
- `src/session/storage.rs`: the `edit` branch of `normalize_args_in_place` now
  delegates to `canonicalize_edit_value` (best-effort).
- `src/session/session.rs`: in `append_message`, canonicalize each assistant
  `edit` tool-call's args **before** `rig_message_to_pi_entries`, so both the
  JSONL on disk and the in-memory index are pi-canonical.
- `src/agent.rs`: in `LoggingHook::on_tool_call`, canonicalize edit args before
  `display_tool_call`; in `LoggingHook::on_completion_call`, make the history
  patch unconditional and apply `blob_edit_tool_calls` (array→blob) to the whole
  history; update the edit guidance line in the default system prompt.
- Docs: this file; note the format in `README` / tool docs.

### Tests (Phase 1)

- Parser: single block; multi-block same file; fences; no header; matching
  header; conflicting header (error names both); missing `=======` /
  `>>>>>>> REPLACE`; empty search.
- Path policy: `path`+header agree; `path`+header differ → error; `path` absent
  + one distinct header → inferred; `path` absent + differing headers → error.
- Wire: flat schema has **no** `anyOf`; `edits` string applies via
  `ErasedTool::execute`; malformed text errors with guidance; legacy array +
  single still parse.
- Normalization: string-form persisted entry re-loads as canonical array;
  unparseable string is left as-is (no replay error); a freshly appended entry
  is written pi-canonical (array) to the JSONL and to `index.entries` without a
  reload.
- Display: live string-form call renders label + diff (via `on_tool_call`
  normalization); a canonical entry renders identically.
- Model form: `blob_edit_tool_calls` on a canonical-array history yields the
  blob, and is idempotent (blob input passes through unchanged).
- Regression: existing `edit_file` / `edit_args_wire_tests` / output-preview
  tests pass unchanged.

---

## Per-block line anchors (`path:LINE`)

*Implemented.*

### Why

Batching makes it cheap to fix the same pattern in many places in one file —
but the same pattern is, by definition, **ambiguous**: the existing tool only
had a single top-level `startLine`/`endLine` window that constrained *every*
block equally, so you could not aim two structurally-identical replacements at
two different regions.

### Grammar

A block's header line may carry a trailing line anchor:

```
some/path.rs:120
<<<<<<< SEARCH
The original text
=======
The new text
>>>>>>> REPLACE
```

### Semantics

- **Anchor, not window.** A lone line number does not restrict the search; the
  whole file is always searched. It **disambiguates**: when `oldText` occurs
  more than once, the occurrence whose start line is nearest the anchor wins.
- **Tolerance.** Nearest-match is accepted only within
  `(context_lines / 3).min(4)` lines — a bare single-line SEARCH stays exact and
  three context lines allow ±1. A globally unique match ignores the anchor.
- **One anchor source per shape (B).** Single edit: the `path` argument
  (`path:LINE` suffix or `startLine`). Blob: **only** `path:LINE` block headers;
  a top-level `startLine`/`path:LINE` is stripped and ignored, so a stray
  call-wide anchor cannot silently aim an unrelated block.
- **Transient.** Anchors are not persisted (the canonical array has no line) and
  not rendered in the model-facing blob.
- **No end line.** The `oldText` end is implied by its length; see *No
  `endLine`*.

### Teaching the syntax

When the model did *not* anchor, the success string echoes where each block
matched — `Replaced 1 block(s) in ./src/foo.rs (matched at ./src/foo.rs:132)` —
so the `path:LINE` idiom appears in the very transcript it imitates. It is a
picocode-authored string (deterministic, stored), never a rewrite of the
model's own arguments. The ambiguity error teaches the same syntax more
pointedly.

### No `endLine`

`endLine` was **dropped** rather than kept alongside the anchor. With it, the
field had two meanings — "first line of an exact byte window" when `endLine` was
set, "nearest-match anchor" when it was not — a comprehension tax selected by a
sibling field's presence. The window's only unique power was targeting a *far*
occurrence that was nonetheless the sole match in a broad range; in practice you
anchor *on* the target or add context instead. Crucially it did **not** remove the
anchor's one real hazard — a silent wrong-pick, which is inherent to nearest-match
and survives either way. Dropping it is a strict simplification (one anchor
concept, no window code), and its cost is *loud*: a legacy call relying on a wide
window now fails as out-of-tolerance and can simply be retried. Since anchors are
transient, there is no session or pi-format migration.

---

## Phase 2 — multi-file (gated)

Gated on the grep described under *Two-phase scope* — count the persisted
`multi-file edits are not supported` rejections before building any of this.

### Audit against HEAD

The original plan does not survive contact with the code. Four findings:

1. **The canonical-shape migration must be dropped — it would break the pi
   compatibility this whole document is built on.** pi's `edit` schema is
   `{path: string, edits: Array<{oldText, newText}>}`: **one `path` per call**,
   no per-edit path (`packages/coding-agent/src/core/tools/edit.ts`, checked
   against `main`). `edits:[{path,oldText,newText}]` is not a shape pi can read
   or render, so migrating to it would trade a pi-compatible single-file form
   for an incompatible multi-file one. pi's own prompt guidance agrees — *"When
   changing multiple separate locations in one file, use one edit call with
   multiple entries in `edits[]` instead of multiple edit calls"* — so
   **path-per-call is the pi-compatible multi-file shape**, not a workaround for
   one. The canonical form does not move; the largest Phase 2 item is gone.
2. **The parser work is already done.** `ParsedEdit` carries its own `header`,
   `parse_blocks` sets `multi_file`, and `blockedit::group_by_file(parsed,
   fallback) -> Vec<FileBatch>` is exactly the split this section describes, in
   first-seen order, with a tested consumer (`bsed apply -i`). "Parser allows a
   header per block; blocks may repeat or omit a path" is implemented — and its
   answer to the open question below is already coded.
3. **`group_by_file` is not a drop-in for `merge_path`; the difference is a
   wrong-file-write hazard.** `merge_path` is strict-on-conflict;
   `group_by_file` is header-authoritative — a header wins and `path` only fills
   gaps. So a blob whose first block says `a.rs`, whose second block has no
   header, sent with `path: "b.rs"` is **today a loud `path mismatch` error**, but
   under a naive `group_by_file` split becomes **two batches, `a.rs` and
   `b.rs`** (verified). That is precisely the silent wrong-file write
   `merge_path` exists to prevent, so the split must run the conflict check
   first — cheapest as a `merge_path` variant that keeps the policy and relaxes
   only its `multi_file` arm.
4. **`group_by_file` keys on the raw header string, not the normalized path.**
   `path: "./a.rs"` with a header `a.rs` yields *two* batches for one file,
   where `merge_path`'s `same_path` (`resolve_normalized`) collapses them
   (verified). Two all-or-nothing applies and two stored calls for one file.
   The split key must be the normalized path.

### Three pipelines, not two

Multi-file touches three separate paths, and only the first is the tool:

| | assistant `edit` call | tool result | source |
| --- | --- | --- | --- |
| **execution** | 1 call | 1 aggregated envelope | `EditFile::call` |
| **in-turn model context** | 1 × multi-file blob | 1 aggregated | rig's live history → `on_completion_call` |
| **stored + post-reload** | N × single-path | N per-file | `append_message` → JSONL → `extract_messages` |

`append_message` writes the session file and the index; it does not touch rig's
live `history`. `on_completion_call` (`agent.rs:1794`) reads that live history
(`AgentRunStep::CallModel { history }`, `streaming.rs:610`), which holds the
model's **raw** assistant message and the tool results. `blob_edit_tool_calls`
(`agent.rs:1197`) only rewrites `AssistantContent::ToolCall`, and
`array_to_blob_value` returns a blob string unchanged because `edits` is not an
array. So **the model sees exactly the one multi-file blob it emitted, in-turn
and forever within that turn** — verified.

**This is the trap in the obvious plan.** Splitting only at persist is the single
option where the two views of one session disagree: the panel, the model and
`--log-messages` all say one call, while the file says three, and a reload
silently rewrites the model's own history underneath it. A model that trusts its
context less after a reload is a worse outcome than either alternative. So the
model-facing split is not optional garnish on the storage split — it is what
makes the storage split safe.

The corollary is cheap. `on_completion_call` receives the **whole** history, so
the assistant call and its matching `Message::User` tool-result batch are in one
pass and ids pair directly. The persist site gets the same two sides in two
separate `append_message` calls with no shared state, which is why determinism
is a correctness requirement *there* and merely good hygiene in the model hook.
One transform, two call sites, and the per-file content is the same envelope
either way.

### Two independent axes (not the retired A/B/C strategies)

pi compatibility is a *storage* property, not an execution one, so "accept the
blob" and "let pi read the session" are separate changes that can be gated on
separate evidence:

| | Today | Change |
| --- | --- | --- |
| **execution** | `merge_path` rejects; the model retries, or had already emitted parallel per-file calls (rig dispatches those; costs nothing) | accept the blob, split per file in `EditFile::call` |
| **storage** | one raw multi-file call; pi cannot read the file | split at persist into N canonical single-path calls |

Either can ship alone, and each is small. The storage axis presupposes the
execution one — a call that is rejected never produces the per-file results the
splitter consumes — but the execution axis alone is already a strict improvement
over the status quo.

### Execution axis

- **Atomicity — resolve all files, then write the ones that resolved.** The two
  original bullets ("validate everything first across files" vs. "per-file
  atomic with partial success") contradict; this is the reconciliation.
  `apply_to_text` is pure over text, so every file can be resolved in memory
  before anything is written. That reports every problem in one round trip, and
  — because sandbox checks (`get_path_with`) and reads also happen in the first
  pass — a path outside the sandbox or a missing file is found *before* any
  write, not after. That is `bsed`'s per-file policy minus its one weakness (it
  writes as it goes). Global all-or-nothing stays available as a one-line change
  to the write step, but is the wrong default for a many-file refactor, where
  one stale file blocking the rest is exactly what `doc/bsed.md` §7.2 exists to
  reject.
- **Per-file IO errors fold into the report, never `Err`.** `edit_file` returns
  `Err` for a missing file, a non-UTF-8 read and a sandbox denial; one of those
  must not abort the remaining files.
- **Result shape**: `BatchItem` gains `path: Option<String>` so failures read
  `edit #2 (src/bar.rs): …`. `output.rs` needs **no change** —
  `display_tool_result` matches on the presence of `ui_view` + `model_view`
  (`output.rs:2169`), so an aggregate envelope with those same keys renders
  through the existing branch. The original "output path must learn it" is
  wrong.
- The "all blocks need to be within the same file" sentence exists twice,
  byte-identical, in the tool `description()` and the `edits` parameter
  description (`tools.rs:203` and `tools.rs:240`). Both change; lift it to one
  `const` while there or the next edit drifts.

### Storage axis

- **Per-file structured result** — one result must become N paired
  `toolResult` messages (providers reject unpaired tool-call ids). The aggregate
  envelope carries a `files` array alongside `ui_view`/`model_view`; that array
  is what the splitter reads.
- **Split at persist**, in `append_message`: the assistant `edit` tool call
  becomes N calls, and the matching tool-result message becomes N results. These
  are two independent `append_message` calls on the turn transcript
  (`agent.rs:696`) with no shared state, so they agree only because
  **determinism is a correctness requirement, not a cache nicety** — both derive
  ids from the original id plus the file index, never from a counter or a clock.
- **No new API for the per-file canonical value**: `render_blob` +
  `canonicalize_edit_value` already produce it, and `array_to_blob_value`
  inverts it idempotently (verified). `render_blob` drops the `:LINE` anchors,
  which is correct here — anchors are transient and the canonical array has never
  carried them.
- **This repairs compaction for free.** `collect_touched_files`
  (`session.rs:194`) reads `arguments.path` per `edit` call, so a multi-file blob
  stored raw contributes at most its top-level `path` and every header-only file
  is missing from the compaction summary. N stored calls fix it with no code.
- **`on_completion_call` is the second call site, not a bystander.** An earlier
  draft of this note claimed it "needs nothing", on the grounds that
  `blob_edit_tool_calls` derives the model-facing form from the stored canonical
  args. That is true only of the *reload* direction, and it misses the in-turn
  direction entirely — see *Three pipelines* above. The hook must apply the same
  split, in the same pass, over the assistant call and the matching tool-result
  batch, or the live view and the stored view of one session disagree. It is the
  easier of the two sites: one function call, both sides in scope, ids paired by
  inspection rather than by convention. Composition is
  `split(blob(strip(history)))` — split after blobing, so the transform sees the
  shape it is documented to accept, and idempotent either way (a history of
  single-file calls has `multi_file == false` and passes through).
- **Rejoining is the alternative worth weighing.** If N calls mid-turn is judged
  too much of a lie to the model's own emission, the deterministic inverse
  exists: group the synthetic calls by their id suffix in the same transform and
  re-render one multi-file blob, applied at *both* sites so the model sees one
  call and the file holds N. Do not apply the split at persist and skip the
  model hook — that combination is the inconsistency, not a policy.

### Display: unchanged by the above, and wrong today

Splitting inside the tool does not change the live panel, because
`on_tool_call` canonicalizes before `display_tool_call` and a multi-file blob
*cannot* canonicalize. `get_preview` therefore receives `{path,
edits:"<blob>"}`; its `edit` branch reads `edits` as an array (miss), then the
legacy `old`/`new` fields (miss, both empty), and calls `edit_preview` with one
empty op — an empty diff plus a bogus `[1]` range, since `content.find("")` is
`Some(0)`. The panel reads `edit b.rs [1]` with nothing under it, for a call
that failed. So "output path only ever sees the canonical array" holds for every
shape *except* the one this section is about. Cheapest correct fix: when `edits`
is a string that does not resolve to one file, print the label with no range and
no diff.

### Files

`crates/blockedit/src/blob.rs` (conflict-checking split, normalized key),
`diagnose.rs` (`BatchItem.path`, aggregate envelope), `wire.rs` (`EditArgs`
gains multiple files), `src/tools.rs` (loop, folded IO errors, deduplicated
description), `src/session/session.rs` (persist-time split), `src/output.rs`
(get_preview fix only). `src/edit_blocks.rs` is a re-export shim over the crate —
edit the crate, not the shim.

### Open questions (reduced)

- ~~Reuse-last vs. mandatory per-block path~~ → **reuse-last**, already
  implemented in `group_by_file` (a header beats the fallback; header-less
  blocks fall back; neither is an error). Aider reuses, and the prose-line hazard
  is already covered: `header_candidate` requires a whitespace-free token.
- ~~Cross-file atomicity~~ → resolve-all, write-what-resolved.
- ~~Migrate persisted sessions~~ → **no migration**: the canonical form does not
  change, and every form the parser produces already has a legacy reader
  (`canonicalize_edit_value`, `EditArgs::from_wire`).
- **Open**: N blobs or one rejoined blob in model history?
- **Open**: cap on files per call? The single-file form caps blocks not at all,
  but a header-per-file blob makes an unbounded fan-out reachable from one
  string argument.

---

## Decisions

- **Field name**: `edits` (a string).
- **`startLine` semantics**: an **anchor** (near-line hint), not a search
  window; never persisted, never served to the model.
- **`endLine`**: **dropped** (see *No `endLine`*).
- **Anchor source is per shape (B)**: single edit → the `path` argument
  (`path:LINE` suffix or `startLine`, which must agree if both are given); blob
  → `path:LINE` headers only (a top-level `startLine`/`path:LINE` is stripped
  and ignored).
- **Path normalization** for the conflict check: reuse
  `path_util::resolve_normalized` (resolve both sides against cwd, then
  compare).
- **Blob rendering**: the path appears once, as a header line above the first
  block (matching the Aider example), with no `:LINE` suffix. The text path may
  also be omitted entirely by the model (the `path` argument then supplies it);
  it is only *required* when `path` is absent.
- **Per-block anchor**: a header may be `path:LINE`, supplying that block's
  nearest-match hint (see *Per-block line anchors*); transient (not persisted,
  not rendered).
- **`path:LINE` on the `path` argument**: accepted in every shape; the suffix is
  always peeled off before path resolution. It is an anchor only for the single
  edit (alias of `startLine`, must agree); in the blob form it is stripped and
  ignored.
- **Tolerance**: `(context_lines / 3).min(4)`; unique matches ignore it.
- **Success-string hint**: when no edit anchored, the result echoes each match
  location (`Replaced N block(s) in ./f (matched at ./f:LINE)`) to teach the
  syntax by example — a picocode-authored, deterministic string, never a rewrite
  of the model's own args.

## Implementation status (Phase 1 — done)

- `src/edit_blocks.rs` (new): `parse_blocks`, `merge_path`, `render_blob`,
  `canonicalize_edit_value` (→ array, best-effort), `array_to_blob_value`
  (→ blob). Unit-tested. *Since moved: the file is now a re-export shim over
  `crates/blockedit/src/{blob,wire,resolve,apply,diagnose,path}.rs`, so the
  `src/edit_blocks::*` paths named throughout this document still resolve but
  the code lives in the crate.*
- `src/tools.rs`: `EditArgs` reduced to `{path, edits: Vec<EditOp>}` with a
  manual `Deserialize` (`from_wire`) selecting the variant from the raw JSON;
  hand-written flat `parameters()` (no `anyOf`).
- `src/session/storage.rs`: the `edit` normalizer branch now delegates to
  `edit_blocks::canonicalize_edit_value` (covers block string, `old`/`new`,
  `oldText`/`newText`, array).
- `src/session/session.rs`: `append_message` canonicalizes `edit` tool-call
  arguments before `rig_message_to_pi_entries`, so the JSONL and in-memory index
  are pi-canonical.
- `src/agent.rs`: `on_tool_call` canonicalizes `edit` args before display;
  `on_completion_call` is now unconditional and applies `blob_edit_tool_calls`
  (array→block) to the whole history; the default system prompt's edit guidance
  was updated.
- `src/output.rs`: **unchanged** (the parser stays out).

New tests: block parse/merge round-trips (`edit_blocks`), flat-schema shape,
block-form apply/infer/conflict (`tools`), persisted-canonical and
legacy-normalize (`session`), history blobification (`agent`). Full suite green.

## Implementation status (anchors — done)

- `src/edit_blocks.rs`: `ParsedEdit` carries a per-block `start_line`;
  `split_line_anchor` peels a trailing `:LINE`/`:LINE:COL`; `merge_path` and
  `render_blob` speak `ParsedEdit`.
- `src/tools.rs`: `anchor_tolerance`; `edit_file` enumerates occurrences and
  resolves nearest-within-tolerance (no more byte windows); `from_wire` takes
  the anchor per shape (B); `parameters()` documents the anchor and drops
  `endLine`; `EditOp.end_line` removed.
- `src/edit_match.rs`: `diagnose` simplified to whole-file; `non_unique`
  suggests the `path:LINE` header; `anchor_out_of_tolerance` /
  `anchor_ambiguous` added.
- `src/config.rs`: `EditEndLine` removed.
- `src/output.rs`: `endLine` read dropped.
- `src/agent.rs`: system prompt documents the `path:LINE` anchor.
- `src/tools.rs`: `path:LINE` on the `path` argument (honored for a single edit;
  stripped-and-ignored in the blob form); the success string echoes match
  locations when unanchored.

New tests: nearest-duplicate pick, within/out-of-tolerance, unique-ignores
anchor, tie-is-ambiguous, tolerance scaling, header anchor capture,
`split_line_anchor` forms, top-level `startLine` ignored in blob form,
`path:LINE` single-edit anchor + conflict, suffix stripped in blob form,
success-string location hint. Green.

## Notes / follow ups

Each idea below is evaluated against the current code (findings verified against
rig rev `1c2f440`, a live probe of `edit_file`, and probe tests against the
`blockedit` crate). Status is per item, as marked.

### 1. Multi-file batches split into single-file batches — investigated

**Idea**: accept a multi-file blob, split into per-file batches before
execution → per-file failure domain, preserved pi session compatibility.

**Status: investigated; the design is written up under *Phase 2* above.** This
note is kept for the findings that do not belong there.

**Status quo, re-verified.** `merge_path` rejects a multi-file blob
(`blob.rs:367`), `EditArgs::from_wire` wraps it as a self-correcting argument
error, and the model retries — one extra round trip unless it had
*pre-emptively* emitted several parallel `edit` calls in the same response (rig
dispatches parallel tool calls; that path already works and costs nothing). On
the persistence side the call is stored as the raw blob string, because
`canonicalize_edit_value` returns a multi-file blob unchanged (verified), so
**pi compatibility for multi-file is already degraded today**. Any
implementation is a strict improvement.

**The three strategies are not rivals.** A (split inside the tool) changes the
*execution* axis; B (split into N stored calls) changes the *storage* axis; C is
both. Treating them as alternatives — as this note originally did — hid two
things: that pi compatibility is a storage property, and that accepting the blob
and letting pi read the session can ship independently and be gated on different
evidence. The A/B/C table is retired; see *Two independent axes* and *Three
pipelines* in Phase 2.

**The hook-surface constraint stands.** No rig `1c2f440` hook can expand one
assistant message into N before dispatch: `ToolCallAction` is `{Run, Rewrite,
Skip, Stop}` (it rewrites *one* call's arguments, never the call list),
`ToolResultAction` is `{Keep, Rewrite, Stop}` (presentation, not structure), and
`on_completion_response` / `on_model_turn_finished` observe, retry or stop only.
Verified. So a multi-file call is necessarily *one* call to the dispatcher and
one result on the wire — the split is a boundary transform, applied either when
the turn's messages are persisted (`append_message`) or when the request is built
(`on_completion_call`), and the two are genuinely different moments.

**They have to be both.** An earlier draft of this note reasoned that
`append_message` alone sufficed, because the model-facing history is derived
from the stored canonical args. That is true only after a reload, and it misses
the in-turn direction: rig's live `history` is not what `append_message`
writes, so a persist-only split leaves the model looking at one multi-file blob
while the file says three. See Phase 2, *Three pipelines*.

**Determinism is a correctness requirement at persist, and only hygiene in the
model hook.** The assistant-call split and the tool-result split are two
independent `append_message` calls with no shared state, so at that boundary
they agree only because both derive synthetic ids from the original id plus the
file index. `on_completion_call` holds both sides in one pass and pairs by
inspection — but it runs on *every* completion call, so the transform is
deterministic there too, which is what keeps the prompt-cache prefix stable.

**The "surprising to agents" question answers itself against the wrong baseline.**
pi cannot represent a cross-file batch at all and its own prompt guidance steers
per-file, so a model following pi's habits emits per-file calls. Once the split
reaches the model hook, the model would then *see* N per-file calls where it
emitted one — which is either the nudge we want or an avoidable divergence, and
the deterministic rejoin decides it (Phase 2, *Storage axis*). A tool call
becoming several is not surprising where the alternative is a hard error, and it
is not surprising at all if the same session then reloads as N calls, which is
the consistency that actually costs something to get wrong.

**One thing both A and B leave unfixed**, and worth not forgetting: live display.
`on_tool_call` canonicalizes, and a multi-file blob cannot canonicalize, so the
panel renders an empty diff and a bogus `[1]` range today. Splitting the
execution does not fix it. See Phase 2, *Display*.

### 2. Ratio-based anchor tolerance ✅ *implemented*

**Idea**: tolerance based on the distance *ratio* between the two closest
candidates rather than context size, with a high absolute cap; reject
close-ties at large deviation.

**Evaluation — the direction is right because context size is the wrong
axis.** The anchor's only job is choosing among ≥2 exact candidates. Once
≥2 candidates exist, how much context the block carries is irrelevant to
*which* candidate is intended — what matters is how decisively the anchor
favors one over the others. (Context scaling isn't wasted, though: with a
*single* candidate the anchor is ignored entirely, so context only ever
influenced the multi-candidate case, which is exactly where it's a proxy.)

**Proposed rule** (additive to today's, so every currently-accepted call stays
accepted):

```
accept nearest candidate iff
    d1 ≤ tol_ctx(old_text)                       # current rule, kept as fast path
    OR (d1 ≤ CAP AND d2 ≥ RATIO × d1)            # NEW: decisive-skew escape hatch
and the nearest candidate is strictly unique      # d1 tie → ambiguous, as today
```

- `d1`/`d2` = distances from the anchor to the nearest / second-nearest
  candidate. `RATIO ≈ 2–3`, `CAP ≈ 50–100` lines ("10s of lines" per the
  note) — constants to tune with tests.
- **Why additive**: the current `tol_ctx` fast path preserves today's
  behavior exactly; the ratio path only *adds* accepts where the anchor is
  stale (the model read the file earlier, subsequent edits shifted things)
  but the near candidate is unambiguous. Nothing that passes today starts
  failing, except close-ties at large deviation — which already fail today
  (out of tolerance), so that's not a change either.
- **Tie handling stays as-is**: equal distances → `anchor_ambiguous`. The
  ratio rule naturally refuses ties (`RATIO > 1`), so no special case.
- **Edge cases to pin in tests**: `d1 = 0` with a second candidate on the
  *same line* (two matches on one line → tie at 0 → must stay ambiguous);
  >2 candidates (only `d1` vs `d2` matter — a far third candidate is
  irrelevant to the choice); `d1` measured to occurrence *start line* today —
  consider min-distance to the occurrence's line *span* instead, so a model
  anchoring mid-block isn't penalized by block length.
- **Error message impact**: `anchor_out_of_tolerance` currently says "none
  within N lines" — with the skew rule it should report `d1`, `d2`, and the
  candidate lines so the model can see *why* the pick was refused ("nearest
  at line 140 (40 away), next at 145 (45 away) — too close to call").
- Existing tests (`anchor_out_of_tolerance`, tolerance scaling) need updating;
  the current `anchor_tolerance` doc comment claims context size is the
  rationale and would need a rewrite.

**Verdict**: straightforward, orthogonal to 1 and 3, strictly better
diagnosis. Implement alongside an update to 3's failure messages.

**Decisions (implemented)**:

- Rule shipped as `edit_match::anchor_accepts(d1, d2, tolerance)`:
  `d1 <= tolerance || (d1 <= ANCHOR_SKEW_CAP && d2 >= ANCHOR_SKEW_RATIO * d1)`
  with `RATIO = 3`, `CAP = 60` — additive to the old tolerance, so every
  previously accepted call still passes. Ties (`d1 == d2`) can only pass via
  the tolerance clause and are separately rejected, exactly as before.
- `d1` remains distance-to-occurrence-start-line (the span refinement was
  skipped for now — it complicates the ranking for a case the ratio rule
  already tolerates; revisit if models are seen anchoring mid-block).
- `anchor_out_of_tolerance` now reports `d1`, `d2`, both candidate lines, and
  the rule's thresholds; the test asserting the old "none within" wording was
  updated.

### 3. Anchored-first, claim-based resolution ✅ *implemented* (with refinement)

**Idea**: resolve line-anchored blocks first; accept an un-anchored block if
its candidate set shrinks to positions not overlapping previous matches.

**Evaluation — correct, and the investigation surfaced a missing invariant.**
A probe against the current `edit_file` (since removed; not committed):

```
file: "hello world\n"
edits: [{old: "hello", new: "bye"}, {old: "hello", new: "hi"}]   # same unique old_text
→ result: "Replaced 2 block(s) … (matched lines 1, 1)"
→ file:   "hiorld\n"          # CORRUPTED — both edits resolved to byte range 0..5
```

`EditOp` documents "must not overlap with any other edit's old_text in the
same call", but **nothing enforces it**: each edit resolves independently,
and the apply loop (sort by start desc, `replace_range` on the original
offsets) is only sound for *disjoint* ranges — overlapping ranges silently
corrupt (and depending on lengths can panic on a non-char-boundary offset).
The failure mode needs no repeated text at all: two *different* `old_text`s
that overlap in the file corrupt the same way.

So this item is not only a disambiguation improvement — the claim-tracking it
requires is the same machinery that enforces disjointness. It should ship
**with** an overlap check, closing a correctness hole, not just a UX gap.

**Implemented algorithm** — two passes, split by anchor (not by uniqueness),
both in block order, all-or-nothing preserved:

1. **Pass 1 — line-anchored blocks**, in block order. Resolution: unique match
   short-circuits; otherwise nearest + item-2 rule (tolerance OR skew), ties
   ambiguous. The chosen match's byte range is then checked against all
   existing claims: **overlap → conflict, reject** — never silently re-aimed
   to the next-nearest candidate (an anchor is a positive statement of
   location; two anchors on one region is a model error worth surfacing).
   On success the range is *claimed*.
2. **Pass 2 — content-only blocks**, in block order. Partition candidates
   against the accumulated claims (pass 1 + earlier pass 2):
   - exactly one free → accept and claim (so later blocks see the new
     constraint — narrowing chains transitively through pass 2 as well);
   - zero free → `all_candidates_claimed` (names the claiming edits);
   - ≥2 free → `non_unique` listing the *free* lines and the excluded count.
   Never order-based guessing between 2+ free candidates — that would be a
   coin flip the anchor refused to make.
3. **Failures claim nothing.** A failed block (not-found, out-of-tolerance,
   tie, conflict) adds no claim, so later un-anchored blocks still see its
   region and report plain ambiguity — the diagnostic the model can fix by
   anchoring. Both passes run to completion regardless of earlier failures
   (full diagnostics in one response); failure list is sorted by block index
   for stable, block-ordered output.
4. Failures anywhere → nothing applied (unchanged).

**Notes**:

- The two-pass order (anchored first, *regardless of batch position*) is what
  fixes the observed scenario — the model anchors blocks 2..n and leaves
  block 1 un-anchored expecting elimination; pass 1 claims line-anchored
  matches, pass 2's block 1 then sees only the remaining candidates.
  Un-anchored *unique* matches live in pass 2: anchors are positive
  statements of location and get first claim on contested regions;
  uniqueness is a passive property.
- A wrong anchor in pass 1 produces a wrong claim, which can cascade into a
  wrong pass-2 acceptance. Accepted trade-off: batch editing with this risk
  still vastly beats `sed` and friends, which is what models reach for when
  batches get too finicky. Bounded by item 2's tolerance + disjointness +
  all-or-nothing (any failure discards the whole batch → one retry), and
  failure messages name the claiming edit ("already matched by edit(s) #1")
  so the model can untangle it.
- Overlap for *identical* `old_text` is exact-range identity (candidates from
  one `match_indices` pass are non-overlapping by construction); for
  *different* texts it's genuine range intersection. One predicate
  (`start < other_end && other_start < end`) covers both.
- **Failure-claims-nothing** (refinement): a failed block contributes no
  claim, so later un-anchored blocks see its region and surface plain
  ambiguity — the more productive diagnostic (anchor it) rather than
  "everything is claimed". Both passes always run to completion; the
  failure list is sorted by block index so diagnostics arrive in block order.
- Interplay: this item's pass 2 makes item 2's error messages more load-bearing
  (the model must understand anchors to fix pass-2 failures) — implemented
  together, as recommended.

**Verdict**: implemented with the refinement above; higher priority than it
looked because it carries the disjointness bug fix (the probe's `"hiorld"`
corruption is now a loud `overlapping`/`all-claimed` failure).

**Decisions (implemented)**:

- `resolve_edits()` in `src/tools.rs` owns both passes; claims are
  `(edit_idx, byte_start, byte_end)` and only successful resolutions ever
  push one.
- Conflict on anchored overlap → `overlap_conflict` naming both edits and
  both line ranges (`render_line_span`: `line N` / `lines A-B`).
- Pass-2 zero-free → `all_candidates_claimed` (names claimers; single-
  occurrence phrasing for the probe case); ≥2-free → `non_unique` with an
  `excluded` count (message lists only *free* lines).
- Diagnostics: `failures.sort_by_key(edit_index)` before batching; full
  completion of both passes before the all-or-nothing gate.

### Suggested order — completed

1. ✅ **3 + 2** (implemented in [`f05d078`](https://github.com/gwicke/picocode/commit/f05d078)) — the skew rule and claim-based resolver are live; 653 tests pass.
2. **1** — multi-file: still gated, but the gate is now free to measure (grep the
   persisted `multi-file edits are not supported` rejections) and the design is
   done. Two independent changes behind that gate, in dependency order:
   1. **execution** — accept the blob and split per file in `EditFile::call`,
      with a conflict-checking `group_by_file` (raw `group_by_file` would turn
      today's loud `path mismatch` into a silent wrong-file write) and
      resolve-all-then-write-what-resolved.
   2. **storage + model-facing** — one split transform applied at *both* egresses
      that see the call: `append_message` (N canonical single-path calls on disk,
      so pi can read the file, which also repairs compaction's touched-files
      list) and `on_completion_call` (N calls in-turn, so the live view and the
      reloaded view of one session agree). Persist-only is the one option to
      avoid: it makes the same session read as one call mid-turn and three after
      a reload.

   No canonical-form migration and no session migration. See *Phase 2*.
