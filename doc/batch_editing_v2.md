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
| **2 (gated)** | Multi-file: path-per-block, path-per-edit canonical (`edits:[{path,oldText,newText}]`), per-file partial application. | Only worth it if telemetry shows the model routinely wanting cross-file batches. Bigger blast radius; see Phase 2 below. |

Rationale for gating Phase 2 (see also "Diminishing returns"): the backend is
**all-or-nothing** (`src/tools.rs` — no edits applied if any block fails), so a
call of `N` blocks succeeds with roughly `(1-p)^N`. Large multi-file batches are
heavily penalized and a single mismatch discards the whole call. Multi-file adds
the most complexity (path-per-edit canonical migration, cross-file atomicity,
partial-failure reporting) for the least reliability gain, and it does **not**
address the actual failure (JSON mangling). Single-file text edits do.

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
`oldText`/`newText` entries (multi-entry). picocode's canonical in-memory form
already matches this (`normalize_args_in_place` folds legacy `old`/`new` into the
array). So "pi compatibility" here means one concrete thing: **every model form
(the new Aider text blob, the legacy array, or a legacy `old`/`new`) must be
written to disk as that array**, or pi cannot read the file.

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

Only if telemetry shows demand (e.g. count runs of consecutive single-file
`edit` calls targeting different paths as a proxy for wanting one call).

### Changes beyond Phase 1

- **Canonical shape migrates** to path-per-edit:
  `edits:[{path, oldText, newText}]`. Output path, failure envelope, and
  storage normalizer must all learn it; the single-path top-level `path`
  becomes optional/absent when paths live per-edit.
- **Parser** allows a header per block, mapping each block to its own path;
  blocks may repeat or omit a path (decide reuse-last vs. mandatory — see
  below).
- **Backend** generalizes to `edit_files(groups: PathMap<Vec<EditOp>>)`:
  validate **everything** first (across files), then apply per file.
- **Failure/atomicity policy** must be chosen:
  - keep global all-or-nothing (simplest, but a large batch rarely lands), or
  - per-file atomic with partial success + per-file reporting (Aider-like).
  The all-or-nothing penalty is the whole reason large batches are risky, so
  Phase 2 likely *requires* partial-apply to be worth shipping.
- **Failure item labels**: extend `BatchItem` with an optional path so
  multi-file messages read `edit #2 (src/bar.rs): …`.

### Open Phase 2 questions

- Reuse-last path vs. mandatory per-block path. (Aider reuses; mandatory avoids
  a silent-retarget hazard when a prose line precedes a block.)
- Cross-file atomicity: best-effort vs. stage-all-then-flush.
- Whether to migrate persisted sessions once, or keep the legacy array shape
  parseable indefinitely.

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
  (→ blob). Unit-tested.
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
- Explore multi-file support that splits edits into single-file batches
  *before* execution. Makes failure domain per-file, and preserves pi session
  compatibility, but might be surprising for agents to see one batch split up
  into sub-edits? Would need to test.
- Refine line anchor tolerance not based on context size, but based on
  uniqueness between two closest matches, with high absolute upper bound.
  Basically, accept relatively large deviations in line position (10s of
  lines) if the ratio of distance between the two closest candidates is
  sufficiently skewed towards the strongest candidate. Avoid resolving a
  close-tie with large deviation.
- Line anchored vs. un-anchored candidate disambiguation: In several cases,
  the model omitted the line anchor at the first block, assuming that making
  other blocks un-ambiguous would only leave one match for the first block.
  Consider matching line-anchored blocks first, and accepting an un-anchored
  block if it matches a position not overlapping with any previous matches.
