# Batch Editing v2 — Aider-style text edits

*Status: Phase 1 implemented. W1–W4, W7 and W8 done — multi-file editing is accepted, split per
file, applied and reported with per-file failure granularity, an unreadable file no longer aborts
the call, the model sees N per-file calls and results, and a saved session stores N pi-canonical
single-path calls with matching results. W5 wording remains, deliberately deferred.*

## Motivation

The current `edit` tool advertises a nested `edits:[{oldText,newText,...}]` array (batch variant)
plus a top-level single-edit variant via an untagged `anyOf` schema. Weak tool-calling harnesses
mangle the nested array — most commonly by **stringifying** it (`"edits": "[{...}]"`) — so the call
fails before the model ever sees an error.

Goal: replace the batch-array variant with a **flat string parameter** carrying one or more
Aider-style SEARCH/REPLACE blocks. Flat string parameters are what these harnesses serialize *to*,
so the mangling that breaks the array becomes a no-op. **Phase 1 stays single-file**; the `path`
JSON arg is authoritative and the parsed blocks normalize into the *existing* canonical
`edits:[{oldText,newText}]` shape, so the execution backend, output path, and session layer are
reused unchanged.

## Two-phase scope

- **Phase**: **1 (the fix)**
  - **Scope**: Single file. `{path, blocks:"<block blob>"}` → normalize to canonical
    `{path, edits:[{oldText,newText}]}`. Flat schema, no `anyOf`, no array on the wire.
  - **Why**: Directly kills the nested-array mangling with minimal surface. Reuses `edit_file` +
    `render_batch` + display + storage verbatim.
- **Phase**: **2 (gated)**
  - **Scope**: Multi-file blobs accepted and **split per file**. The canonical form does *not* move:
    it stays pi's single-`path` `{path, edits:[{oldText,newText}]}`, one stored call per file.
  - **Why**: Only worth it if telemetry shows the model routinely wanting cross-file batches. pi
    cannot express a multi-file call at all, so "one call per file" *is* the pi-compatible shape,
    not a deviation from it. See Phase 2 below.

Rationale for gating Phase 2: the backend is **all-or-nothing** (`src/tools.rs` — no edits applied
if any block fails), so a call of `N` blocks succeeds with roughly `(1-p)^N`; large batches are
heavily penalized and one mismatch discards the whole call. Multi-file does **not** address the
actual failure (JSON mangling) — single-file text edits do. The gate is measurable with no new
instrumentation: the rejection is a fixed literal in `merge_path` (`blob.rs:367`) that survives
verbatim into the persisted tool result, so a grep over existing session JSONL counts every model
that wanted a cross-file batch.

---

## Phase 1 — single-file text edits

### Wire format

Keep **one** tool (`edit`) but advertise a **flat object with all fields optional** — no `anyOf`, no
nested arrays:

```jsonc
{
  "path":      "string?",   // single-edit variant OR anchor for the block variant
  "oldText":   "string?",
  "newText":   "string?",
  "startLine": "integer?",  // single-edit anchor line (near-line hint)
  "blocks":   "string?"    // the @@@OLD/@@@NEW blocks (Phase 1: one file)
}
```

Variant selection stays server-side in `EditArgs::from_wire` (which already inspects the raw
`Value`), so the advertised schema is a plain bag of optional scalars. Accepted shapes — and
**only** these two:

1. **Single (OpenAI-style):** `{path, oldText, newText, startLine?}`.
2. **Block string:** `{path?, blocks:"<blob>"}` — Phase 1 single file; a block's anchor is a
   `path:LINE` header, not the top-level `startLine`.

### `blocks`, not `edits` — and no array on the wire

`edits` used to name **two different things**: this string blob, and the canonical array. Splitting
them removes the overloading, and each name is now unambiguous:

- **Field**: `blocks`
  - **Type**: string (blob)
  - **Who writes it**: the model; `array_to_blob_value`
  - **Who reads it**: `EditArgs::from_wire` (wire), `blob_text`
- **Field**: `edits`
  - **Type**: array
  - **Who writes it**: `canonicalize_edit_value` (canonical, on disk); a model by reflex
  - **Who reads it**: `canonicalize_edit_value`, `get_preview`, replay, and `EditArgs::from_wire` —
    accepted, never advertised

**The `edits` array was removed as a wire shape, and has since been put back.** It was accepted for
pi parity and then withdrawn, because it is the exact thing weak harnesses mangle — most often by
*stringifying* it, which turns a batch into a JSON string and silently loses the edit.

Two of the three reasons that decision rested on turned out not to hold, so the array is accepted
again (`crates/blockedit/src/wire.rs`, and `doc/edits_readd_and_arg_errors.md` for the full
argument):

- **The stringify hazard is handled rather than avoided.** An `edits` *string* whose first character
  is `[` is parsed back into an array and run through the same validation, so the mangled call now
  works instead of dying in the block parser. The hazard was never "the array is hard to read" — it
  was "the array is hard to *arrive* with" — and the arrival problem is now detected by name.
- **The refusal cost more than it bought.** A pi-trained model sends it by reflex, so every such
  call was paying a round trip to be told what the schema had already told it. A correction cannot
  beat the model not knowing the answer; it can only be slower.

What did *not* change is the advertising, and that is the part that was always load-bearing: `edits`
remains absent from `parameters()`, `blocks` remains a string, and no advertised property is an
array. Accepting a shape and encouraging it are separate decisions, and only the first one was
reversed — which is exactly the distinction the original design drew when it picked `blocks` over
`edits` as the *name*. `src/tools.rs` pins both halves.

The multi-file pi form (`edits: [{path: "a"}, {path: "b"}]`) stays refused: pi itself cannot express
it, and `blocks` is the shape that can. One rule covers it and the lenient cases — every path the
call names must be the same one.

`startLine` (1-indexed) is an **anchor**, not a search window — see *Per-block line anchors* below
for the semantics, the per-block header form, shape (B), and why there is **no `endLine`**. It is a
matching aid against ambiguity; it is **not persisted** (the canonical array and the blob both omit
it) and **not served to the model**.

Example:

```text
src/foo.py
@@@OLD
def foo():
    return 1
@@@NEW
def foo():
    return 2
@@@END

@@@OLD
def bar():
    return 3
@@@NEW
def bar():
    return 4
@@@END
```

### Parameter name

**`blocks`** (a string) — settled, and the third name this parameter has had. The trail is the
argument:

- **`edits`** was right for continuity. The model already associated the name with this tool, and a
  harness that stringified the array then produced the *correct* type. It stopped being right once
  `edits` was pinned as pi's canonical array: one name for a string blob *and* an array is a trap,
  and it is the array that must not be encouraged.
- **`batches`** fixed the overloading and was wrong about the content. A `batches` string is not a
  set of batches — it is a sequence of `@@@OLD` / `@@@NEW` blocks, and (from Phase 2) they are the
  *parts* of a per-file batch, not batches themselves. Calling the parts the batch is the same
  category error in the other direction, and it is the kind of imprecision that compounds: "one
  batch" then means two different things in the same codebase.
- **`blocks`** names what is actually in the string, matches the `blocks` the `sed` preview tool
  already reports, and leaves `edits` meaning exactly one thing everywhere else. The
  `@@@OLD`/`@@@NEW`/`@@@END` markers make it unambiguous to a model, and the plural is honest about
  the multi-block case.

Alternatives rejected: `patch` (collides with unified `patch`), `diff` (misleading — this is not a
unified diff), `blob` (names the format rather than the parameter's role), `replacements` (accurate
but long, and `oldText` / `newText` already carry the word).

### Path handling (Phase 1) — lenient parse, strict on conflict

The `path` JSON arg is authoritative, but LLMs that recognize "Aider format" will inject per-block
filename lines. The policy balances familiarity against data integrity:

- The parser treats the line(s) between the previous block's `@@@END` (or start of text) and
  `@@@OLD` as an **optional header**. A header line that resolves as a path is captured; other
  non-marker text is ignored.
- **`path` present, header present, they match** → OK.
- **`path` present, header present, they differ** → **hard error** naming both paths. This is the
  data-integrity guard: never silently write file A when the text names file B.
- **`path` present, no header** → OK (use `path`).
- **`path` absent, headers present and all identical** → **infer** the file from the headers.
  (Aider-style calls need no `path` argument at all.)
- **`path` absent, headers absent** → error: block 1 names no file.
- **`path` present, headers absent** → **error too** (this used to be the ergonomic "supply the path
  once" case). `path` is the OpenAI form's argument; the block form takes its file from each header.
  See *Decisions*.
- **`path` present, headers present and differing from it** → the headers win; `path` is ignored. A
  redundant argument is not a second source of truth, so it cannot re-target the call and is not
  reconciled against N headers.
- **`path` present, *some* blocks have a header and some do not** → error naming the block that
  names no file. This is the case a naive multi-file split silently gets wrong — see Phase 2.

Why not "reject paths entirely": models trained on Aider will emit per-block filenames, and failing
every such call is hostile — it defeats the purpose of borrowing a familiar format. The block form
therefore *accepts* headers everywhere, and the `path` argument it tolerates is ignored rather than
reconciled, so tolerance can never become a silent wrong-file write.

### Path handling (multi-file, Phase 2) — every block names its own file

The rules above are the **single-file** policy and stay exactly as they are. A blob that spans files
is governed differently, and strictly:

- **`path` must not be supplied.** It is one file; a multi-file blob carries N paths, and a
  top-level `path` cannot be right for all of them. It is also no longer needed — a single value
  cannot answer an N-file question, so requiring it would be asking for something that cannot be
  correct. The block form should not carry `path` at all (see *Decisions*); what a single-edit call
  needs it for is the OpenAI form, which is where it now lives.
- **Every block must carry its own header.** No defaulting — not to the `path` argument, not to the
  previous block. A block without a header is an error naming it.
- A `path` argument alongside a multi-file blob is **ignored, not reconciled.** It is not a second
  source of truth to be checked against N headers.

Why no defaulting, which is stricter than the single-file rule: the single-file fallback is safe
because there is only one candidate — `path` can only be the answer. Once a blob spans files, a
header-less block has two or more plausible homes and a rule that picks one is guessing at a write.
Guessing here is how a block meant for `a.rs` lands in `b.rs`; an error is one round trip.
Defaulting also makes the blob *unparseable-in-advance*: you cannot tell "this block belongs to the
file named above" from "this block belongs to the `path` argument" without a precedence rule, and
every precedence rule has a wrong answer for some blob.

The ergonomic cost is one filename line above each block, in a format whose whole point is that the
filename line is a natural thing for a model to write.

Path comparison: normalize both sides with the **existing** path utilities —
`crate::path_util::resolve_normalized(path, &cwd)` (tilde expand → join cwd if relative → clean
`..`/`.`), applied to both the `path` arg and the header path, then compare the results. This reuses
the same normalization the rest of the codebase relies on for session portability (relative paths
within the repo) rather than inventing a local one. If the normalized forms are not equal, error
(safe direction) rather than guess.

### Grammar (`src/edit_blocks.rs`, new)

```text
@@@OLD
<original lines>
@@@NEW
<replacement lines>
@@@END
```

- Markers must appear as their **own line**, with nothing trailing. First `@@@NEW` closes the
  original region; first following `@@@END` closes the block. A marker that *almost* matches
  (`@@@OLD>`, `@@@END `) is rejected when it is the thing that lost the structure, and left alone
  inside a body — where it is far more likely to be literal text the caller means to write.
- A literal marker line inside content is unrepresentable (same limitation as Aider) — document it;
  content needing it must be split or anchored with surrounding context.
- Leading/trailing ```` ``` ```` fences around the whole blob are stripped (models habitually emit
  fences).
- Multiple blocks are applied in text order.

Shared parser API (one parser, shared by every site that consumes a blob):

```rust
// crates/blockedit/src/blob.rs
pub struct ParsedBlocks {
    /// Some(path) iff every header present agrees and at least one is.
    pub path: Option<String>,
    /// True when two blocks carry *differing* headers (a multi-file attempt).
    pub multi_file: bool,
    /// Blocks in text order, each keeping its own header and anchor.
    pub edits: Vec<ParsedEdit>,
}
pub struct ParsedEdit {
    pub old_text: String,
    pub new_text: String,
    pub start_line: Option<u64>,  // from a `path:LINE` header
    pub header: Option<String>,   // this block's own file, if it named one
    /// The line above `@@@OLD` that was *not* taken as a header, when there
    /// was one. Lets the multi-file error quote back what the model wrote.
    pub rejected_header: Option<String>,
}
pub fn parse_blocks(text: &str) -> Result<ParsedBlocks, ParseError>;
```

`ParsedEdit.header` is what makes multi-file reachable at all: the parser validates that headers
agree without keeping them, and keeping them is what turns a validated blob into a groupable one.

Plus a small merge helper —
`(json_path: Option<&str>, parsed) -> Result<(String, Vec<EditOp>), String>` — implementing the path
policy above; called by `from_wire` (ingress) and the canonical normalizers (persist, display) so
they can never diverge.

Malformed input (missing `@@@NEW`, unterminated block, empty original text) yields the
self-correcting error style of `from_wire`: name the one problem, show the one accepted shape, echo
the offending structure.

### Tool result: outcome, not echo

The edit tool result does **not** echo the raw edit parameters, and should not. Two different
"echoes" are worth separating:

- **The tool-result body.** Today it returns `Replaced N block(s) in <path>` on success, and a
  structured `{ui_view, model_view}` failure on error. That is the right shape: the model already
  *has* its own parameters, so re-sending them wastes tokens and adds no information. The result
  only needs to convey the **outcome** — success, or the diagnostic needed to fix it.

  The failure diagnostic **used to** be a targeted diff of the mismatching region, and the
  parenthetical below used to argue for it: *"Error diagnostics legitimately quote the old text
  region; that is a diff, not a parameter dump."* **That argument no longer holds.** The diff was
  over a window padded by a fixed 256 bytes on each side, capped at 30 rows from the front, so for a
  small `oldText` in a short-line file the rows that actually differed were usually the ones cut —
  the model received its own rejected line and nothing else. It now quotes the **file**, in `read`'s
  exact `NNNN|` format, at the region that differs; it never quotes the model's own text except when
  the block cannot be identified any other way. See `doc/edit_error_improvements_260930.md`.
- **The assistant's own tool-call arguments in the transcript.** These are re-sent to the model on
  every completion because the assistant message is part of history — this is not something the tool
  result controls. It means we are free to **reformat** the stored arguments (e.g. normalize the
  text blob to the array) without losing anything the model needs: providers require the `id`/name
  pairing, not byte-identical arguments. The raw emitted bytes are not a contract.

Net: success/error is sufficient, no echo is required, and the transcript args may be normalized
freely.

### Echo on *invalid* input: elide content, keep structure

The one place raw args *are* echoed is the self-correcting error for **unparseable input**
(`EditArgs::invalid`), which shows the received parameters so the model can see what it sent. A
naive byte-prefix truncation is the wrong tool there: it chops a blob mid-content and routinely
hides the very marker lines that explain the failure.

So the echo is **structure-preserving** (`truncate_wire_repr` → `elide_value` → `elide_string`):
every long string field has only its *structural* lines kept — the `@@@OLD` / `@@@NEW` / `@@@END`
markers, matched by **prefix** so a hallucinated `@@@OLD>` still shows — plus one line of context on
each side, with each run of elided lines collapsed to `… (N lines elided) …`. Content is bulk, and
the model already has it; the marker *shape* is what diagnoses the error. A long string with no
markers at all degrades to a plain prefix, and short fields (a `path`, a short `oldText`) pass
through verbatim. The whole echo is still bounded by `MAX_ECHOED_ARGS`.

Example:

```text
{"blocks":"@@@OLD\nold line 0\n… (38 lines elided) …\nold line 39\n@@@NEW\n…\n@@@END","path":"src/foo.rs"}
```

### The normalization win

Because Phase 1 is single-file, the parser output maps **directly** onto the existing canonical
`edits:[{oldText,newText}]`:

- **Execution:** `EditFile::call` → `edit_file(path, edits)` — **unchanged**. The backend,
  `edit_match::diagnose`, and `render_batch` failure envelope are reused as-is.
- **Session layer:** the string blob is what the model emitted and what JSONL must persist. A shared
  `normalize_edit_args` gains one branch: when `edits` is a **string**, run `parse_blocks` + merge
  and rewrite to the canonical array. Best-effort: on parse failure, leave the raw string so replay
  degrades gracefully instead of erroring. Called on the write path (`append_message`) and on the
  read path (`build_index`, for pre-existing files).
- **Output path:** **unchanged** — `get_preview` only ever sees the canonical array. The parser is
  kept out of `output.rs` entirely; the raw string is canonicalized before it reaches
  `display_tool_call` (see *Canonical normalization* below).

Net effect: **one** new parser, **one** merge helper; the backend, failure envelope, and session
canonical shape are untouched.

### Canonical normalization (verified against rig rev `1c2f440`)

The goal is to keep the custom parser **out of `output.rs`** and, more broadly, to establish a
single **canonical internal form** (the pi array) that persist and render share. Normalize the args
at each boundary where they enter picocode's internal representation, so every downstream consumer
(`get_preview`, `render_turn`, the session file) sees the same shape. (The **model-facing** form is
a separate, deliberate divergence — the blob — handled in *What the model sees* below.)

First, a negative result that rules out the obvious hook:

- **`ToolCallAction::Rewrite` does *not* work for this.** In `runner.rs` (`run_single_tool`) a
  tool-call rewrite changes the **dispatch args** and the `ToolExecutionCommitted` stream event
  only. The persisted transcript comes from elsewhere: `run.rs` sets
  `TurnState.items = turn.choice.clone()` (the model's **raw** provider output) and pushes
  `Message::Assistant { content: items.clone() }`. So a rewrite never reaches `resp.messages()` /
  `final_history` and never reaches the session. Do not use it here.

The raw string reaches the internal surfaces at three points, so normalization to canonical happens
at three call sites — **one shared `normalize_edit_args` fn**, called from each:

- **Path**: Live display
  - **Where**: `LoggingHook::on_tool_call` (`agent.rs:1741`)
  - **Notes**: Fires **mid-turn**, before the assistant message is appended. Normalize the parsed
    `Value` *before* `display_tool_call`. This is what keeps the parser out of `output.rs`.
- **Path**: Disk JSONL + in-memory index
  - **Where**: `Session::append_message` (`session.rs:897`)
  - **Notes**: Rewrite the assistant `edit` tool-call's args **before** `rig_message_to_pi_entries`,
    so both the file on disk and `index.entries` are canonical.
- **Path**: Load / reload
  - **Where**: `build_index` → `normalize_entry_in_place` (`storage.rs:275`)
  - **Notes**: Already exists; kept for pre-existing files.

Why normalize the **message** (before `rig_message_to_pi_entries`) rather than only the in-memory
index: **pi reads picocode's files**, so the on-disk form must already be pi-canonical. This
deliberately overrides the codebase's default "normalize on read, keep JSONL raw" preference
(`storage.rs:273`) — that preference assumed picocode was the only reader. It is not: pi is a second
reader, so the write path must emit the canonical shape.

### What the model sees: always the blob

There are three distinct representations, with clear owners:

- **Representation**: **Emitted**
  - **Form**: blob, pi array, or legacy `old`/`new`
  - **Owner**: the model; ingress only
- **Representation**: **Canonical internal**
  - **Form**: pi array `{path, edits:[{oldText,newText}]}`
  - **Owner**: persist + render
- **Representation**: **Model-facing**
  - **Form**: blob (`path` + SEARCH/REPLACE blocks)
  - **Owner**: history sent to the model

The **model-facing** form is the blob, **consistently** — every message in the history a completion
call sends, not just cross-turn. Rationale: the model imitates the shape of the tool calls it sees,
so a history full of the canonical array would nudge it back toward the exact form the weak harness
mangles. The blob is the form we want it to keep emitting.

**Placement: `LoggingHook::on_completion_call`, made unconditional.** This hook is the one place
that sees the *complete* request history (`CompletionCall` carries `history: &[Message]`; a
`RequestPatch::history` **replaces** the prior messages per `prepare.rs`), so a single transform
there covers prior *and* within-turn messages — the current `on_tool_call`/`append_message` sites
cannot, because they run before the within-turn assistant message exists. Today the hook returns
`Continue` when `reasoning_in_context`; change it to **always** build the full history and return
`Patch(history(transformed))`:

```rust
// agent.rs on_completion_call
let mut history: Vec<Message> =
    maybe_strip_reasoning(event.history.iter().cloned().collect(),
                          self.reasoning_in_context, &self.provider, &self.model);
history = edit_args_to_model_form(history); // array -> blob, idempotent
if let Some(logger) = &self.message_logger { /* log `history` (what is sent) */ }
CompletionCallAction::Patch(RequestPatch::new().history(history))
```

This is semantically identical to today for every message without an `edit` tool call (the patch
replaces history with itself), so it is a no-op except for edit calls. Note the default path
**already** patches: `reasoning_in_context` defaults to `false` (`main.rs:259`), so the `Continue`
branch is only reached when reasoning is kept in context; making the patch unconditional changes
only that case. The transform touches only the `arguments` `Value` of `edit` tool-call parts;
`id`/name/result pairing is untouched, so provider replay contracts hold, and prompt caching is
provider-side/automatic (position-based), not driven by the patch, so it is unaffected; the blob is
deterministic from the stored array, so cache prefixes stay stable.

Why not transform at context build (`extract_messages`): that covers cross-turn only, leaving
within-turn messages as whatever the model emitted this turn — not the "consistently" the goal
requires. The completion hook is the only site that sees the whole request.

**Disk consequence.** The stored args are the canonical array, so the JSONL is not byte-identical to
the model's emitted form. `--log-messages` captures the sent/received messages separately if an
audit trail is wanted. This is the accepted cost of pi-compat.

### Pi compatibility and the egress points

pi's canonical `edit` call is `{path, edits:[{oldText,newText}]}` — an *array* of
`oldText`/`newText` entries (multi-entry), under a **single** `path`
(`packages/coding-agent/src/core/tools/edit.ts`; there is no per-edit `path`, and
`applyEditsToNormalizedContent` writes one file per call). picocode's canonical in-memory form
already matches this (`normalize_args_in_place` folds legacy `old`/`new` into the array). So "pi
compatibility" here means one concrete thing: **every model form (the new Aider text blob, the
legacy array, or a legacy `old`/`new`) must be written to disk as that array**, or pi cannot read
the file. The single-`path` part is load-bearing for multi-file: pi cannot represent a cross-file
batch at all, which is why Phase 2 splits into per-file calls instead of adopting a path-per-edit
canonical form.

Two clarifications that matter:

- **Do not reduce a multi-block edit to a single `{oldText,newText}`.** pi *supports* an array;
  collapsing to one entry is lossy and *less* compatible. "One entry per block" (i.e. the array) is
  the correct target. Only if the intent is "one *tool call* per block" does this need splitting a
  single assistant call into N calls + N results — a lossy, invasive transcript rewrite; avoid it.
- **Parse validity ≠ match validity.** Normalization only needs the blob to be *well-formed*
  (markers present); it does **not** need the `oldText` to match the file. So a *failed* edit (old
  text not found) still has a parseable blob and **must** be normalized for storage; only a
  *malformed* blob (bad markers) cannot be, and is stored raw (pi renders raw args — graceful
  degradation).

**There is no single choke point.** The assistant message carrying the raw args only materialises at
`FinalResponse` (turn end); the mid-turn events that do carry args — the `on_tool_call` event and
the `ToolExecutionCommitted` effective call — are **not** what gets persisted. So the representation
must be normalized at each *egress*, all via one shared `normalize_edit_args()` helper:

- **Egress**: Disk JSONL + in-memory index
  - **Site**: `Session::append_message`, rewrite assistant tool-call args before
    `rig_message_to_pi_entries`
  - **Transform**: → canonical array (pi reads the file)
- **Egress**: Live display
  - **Site**: `LoggingHook::on_tool_call`, before `display_tool_call`
  - **Transform**: → canonical array (so `get_preview` stays array-only)
- **Egress**: Model history
  - **Site**: `LoggingHook::on_completion_call`, whole history, every call
  - **Transform**: → blob (see *What the model sees* above)
- **Egress**: Legacy files on disk
  - **Site**: `build_index` → `normalize_entry_in_place` (exists)
  - **Transform**: → canonical array

The first two and the last all use the same `normalize_edit_args` (→ array); model history uses the
inverse `edit_args_to_model_form` (→ blob). Persist and render share one direction; the model gets
the other.

Cross-turn and within-turn model context are both the blob (the completion hook transforms the whole
history every call). See *What the model sees* above.

What it takes:

1. `normalize_edit_args(&Value) -> Value` (→ array), best-effort and **idempotent** (array form
   passes through; blob and legacy `old`/`new` fold into the array; unparseable blob returned
   as-is).
2. `edit_args_to_model_form(Vec<Message>) -> Vec<Message>` (→ blob), idempotent (non-`edit` calls
   and already-blob args pass through). Blob layout: the path as a header line above the first
   block, then one `@@@OLD\n<old>\n@@@NEW\n<new>\n@@@END` per block, blocks separated by a blank
   line. The anchor is not part of the blob, so the rendered header is a bare path (no `:LINE`). The
   path is used as stored (the model's own spelling).
3. Call `normalize_edit_args` at the persist + display + `build_index` sites; call
   `edit_args_to_model_form` in `on_completion_call`. Execution keeps using `EditArgs::from_wire`
   (ingress).
4. Tests: file on disk is pi-canonical after a blob-form edit; live display canonical; re-load
   canonical; model history is blob for both a within-turn and a cross-turn call; unparseable blob
   degrades without error.

Risks / trade-offs:

- **Disk is no longer byte-identical to the model output.** This is the necessary cost of pi-compat
  (pi reads the file). The raw emitted form is not retained; `--log-messages` already captures the
  sent/received messages separately if an audit trail is wanted.
- **Drift risk**: the array↔blob transforms must be used at every egress; a divergent site makes
  live display, replay, and model context disagree. Mitigated by two shared functions.
- **Provider safety**: only the `arguments` `Value` changes; tool-call `id` and name and result
  pairing are untouched, so Anthropic/OpenAI replay contracts hold.

### Files to change (Phase 1)

- `src/edit_blocks.rs` (new): parser (`parse_blocks`/`merge_path`) + `canonicalize_edit_value` (→
  array) + `array_to_blob_value`/`render_blob` (→ blob) + unit tests.
  *(Now `crates/blockedit/src/{blob,wire}.rs`, reached through a re-export shim.)*
- `src/tools.rs`: replace `EditArgs`/`EditArgsVariant` with a flat struct; variant selection in
  `from_wire`; call `parse_blocks` for the string form; hand-written flat `parameters()`; update
  `EditFile::description` and `EDIT_ARGS_EXAMPLE`.
- `src/output.rs`: unchanged at the time — the parser stayed out, which held until the rename made
  `get_preview` canonicalize its own input.
- `src/session/storage.rs`: the `edit` branch of `normalize_args_in_place` now delegates to
  `canonicalize_edit_value` (best-effort).
- `src/session/session.rs`: in `append_message`, canonicalize each assistant `edit` tool-call's args
  **before** `rig_message_to_pi_entries`, so both the JSONL on disk and the in-memory index are
  pi-canonical.
- `src/agent.rs`: in `LoggingHook::on_tool_call`, canonicalize edit args before `display_tool_call`;
  in `LoggingHook::on_completion_call`, make the history patch unconditional and apply
  `blob_edit_tool_calls` (array→blob) to the whole history; update the edit guidance line in the
  default system prompt.
- Docs: this file; note the format in `README` / tool docs.

### Tests (Phase 1)

- Parser: single block; multi-block same file; fences; no header; matching header; conflicting
  header (error names both); missing `@@@NEW` / `@@@END`; empty original text.
- Path policy: `path`+header agree; `path`+header differ → error; `path` absent
  - one distinct header → inferred; `path` absent + differing headers → error.
- Wire: flat schema has **no** `anyOf` and advertises **no array** property; the `blocks` string
  applies via `ErasedTool::execute`; malformed text errors with guidance; the single-edit shape
  still parses; the `edits` array is **rejected** with a named correction and leaves the file
  untouched.
- Normalization: string-form persisted entry re-loads as canonical array; unparseable string is left
  as-is (no replay error); a freshly appended entry is written pi-canonical (array) to the JSONL and
  to `index.entries` without a reload.
- Display: live string-form call renders label + diff (via `on_tool_call` normalization); a
  canonical entry renders identically; a blob that will not canonicalize renders a label with **no**
  range and **no** diff, never a fabricated one.
- Model form: `blob_edit_tool_calls` on a canonical-array history yields the blob, and is idempotent
  (blob input passes through unchanged).
- Regression: existing `edit_file` / `edit_args_wire_tests` / output-preview tests pass unchanged.

---

## Per-block line anchors (`path:LINE`)

*Implemented.*

### Why

Batching makes it cheap to fix the same pattern in many places in one file — but the same pattern
is, by definition, **ambiguous**: the existing tool only had a single top-level
`startLine`/`endLine` window that constrained *every* block equally, so you could not aim two
structurally-identical replacements at two different regions.

### Grammar

A block's header line may carry a trailing line anchor:

```text
some/path.rs:120
@@@OLD
The original text
@@@NEW
The new text
@@@END
```

### Semantics

- **Anchor, not window.** A lone line number does not restrict the search; the whole file is always
  searched. It **disambiguates**: when `oldText` occurs more than once, the occurrence whose start
  line is nearest the anchor wins.
- **Tolerance.** Nearest-match is accepted only within `(context_lines / 3).min(4)` lines — a bare
  single-line SEARCH stays exact and three context lines allow ±1. A globally unique match ignores
  the anchor.
- **One anchor source per shape (B).** Single edit: the `path` argument (`path:LINE` suffix or
  `startLine`). Blob: **only** `path:LINE` block headers; a top-level `startLine`/`path:LINE` is
  stripped and ignored, so a stray call-wide anchor cannot silently aim an unrelated block.
- **Transient.** Anchors are not persisted (the canonical array has no line) and not rendered in the
  model-facing blob.
- **No end line.** The `oldText` end is implied by its length; see *No `endLine`*.

### Teaching the syntax

> Superseded: the success string no longer echoes a match location. Successful history now carries
> the resolved anchor in the block header (`path:LINE`), which is the single source of truth; a
> second line in the message only disagreed with it — see
> `261018_minimal_edit_context_anchors.md`. The ambiguity error still teaches the same syntax.

### No `endLine`

`endLine` was **dropped** rather than kept alongside the anchor. With it, the field had two meanings
— "first line of an exact byte window" when `endLine` was set, "nearest-match anchor" when it was
not — a comprehension tax selected by a sibling field's presence. The window's only unique power was
targeting a *far* occurrence that was nonetheless the sole match in a broad range; in practice you
anchor *on* the target or add context instead. Crucially it did **not** remove the anchor's one real
hazard — a silent wrong-pick, which is inherent to nearest-match and survives either way. Dropping
it is a strict simplification (one anchor concept, no window code), and its cost is *loud*: a legacy
call relying on a wide window now fails as out-of-tolerance and can simply be retried. Since anchors
are transient, there is no session or pi-format migration.

---

## Phase 2 — multi-file (gated)

Gated on the grep described under *Two-phase scope* — count the persisted
`multi-file edits are not supported` rejections before building any of this.

### Audit against HEAD

The original plan does not survive contact with the code. Five findings:

1. **The canonical-shape migration must be dropped — it would break the pi compatibility this whole
   document is built on.** pi's `edit` schema is `{path: string, edits: Array<{oldText, newText}>}`:
   **one `path` per call**, no per-edit path (`packages/coding-agent/src/core/tools/edit.ts`,
   checked against `main`). `edits:[{path,oldText,newText}]` is not a shape pi can read or render,
   so migrating to it would trade a pi-compatible single-file form for an incompatible multi-file
   one. pi's own prompt guidance agrees — *"When changing multiple separate locations in one file,
   use one edit call with multiple entries in `edits[]` instead of multiple edit calls"* — so
   **path-per-call is the pi-compatible multi-file shape**, not a workaround for one. The canonical
   form does not move; the largest Phase 2 item is gone.
2. **The parser work is already done.** `ParsedEdit` carries its own `header`, `parse_blocks` sets
   `multi_file`, and `blockedit::group_by_file(parsed, fallback) -> Vec<FileBatch>` is exactly the
   split this section describes, in first-seen order, with a tested consumer (`bsed apply -i`).
   "Parser allows a header per block; blocks may repeat or omit a path" is implemented — and its
   answer to the open question below is already coded.
3. **`group_by_file` is not a drop-in; the difference is a wrong-file-write hazard.** (Written when
   the block path went through `merge_path`, which is strict-on-conflict. `merge_path` has since
   been removed from the block path entirely — `split_by_file` serves every blob — so the hazard
   below stands on its own:) `merge_path` is strict-on-conflict; `group_by_file` is
   header-authoritative — a header wins and `path` only fills gaps. So a blob whose first block says
   `a.rs`, whose second block has no header, sent with `path: "b.rs"` is
   **today a loud `path mismatch` error**, but under a naive `group_by_file` split becomes
   **two batches, `a.rs` and `b.rs`** (verified). That is precisely the silent wrong-file write
   `merge_path` exists to prevent, so the split must run the conflict check first — cheapest as a
   `merge_path` variant that keeps the policy and relaxes only its `multi_file` arm.
4. **`group_by_file` keys on the raw header string, not the normalized path.** `path: "./a.rs"` with
   a header `a.rs` yields *two* batches for one file, where `merge_path`'s `same_path`
   (`resolve_normalized`) collapses them (verified). Two all-or-nothing applies and two stored calls
   for one file. The split key must be the normalized path.
5. **`group_by_file`'s own doc comment is wrong**, which is a trap for whoever implements the split.
   `crates/bsed/src/cli.rs:500` claims a file named on the command line wins with headers as
   fallback, and calls that "the same precedence as the `edit` tool's `path` argument" — but
   `group_by_file` does `edit.header.or(fallback)` (header wins), and the `edit` tool has no
   precedence at all, it has `merge_path`'s strict-on-conflict. Two false claims in three lines. Fix
   it as a drive-by; do not design around it.

### Three pipelines, not two

Multi-file touches three separate paths, and only the first is the tool:

- **execution**
  - **assistant `edit` call**: 1 call
  - **tool result**: 1 aggregated envelope
  - **source**: `EditFile::call`
- **in-turn model context**
  - **assistant `edit` call**: 1 × multi-file blob
  - **tool result**: 1 aggregated
  - **source**: rig's live history → `on_completion_call`
- **stored + post-reload**
  - **assistant `edit` call**: N × single-path
  - **tool result**: N per-file
  - **source**: `append_message` → JSONL → `extract_messages`

`append_message` writes the session file and the index; it does not touch rig's live `history`.
`on_completion_call` (`agent.rs:1794`) reads that live history
(`AgentRunStep::CallModel { history }`, `streaming.rs:610`), which holds the model's **raw**
assistant message and the tool results. `blob_edit_tool_calls` (`agent.rs:1197`) only rewrites
`AssistantContent::ToolCall`, and `array_to_blob_value` returns a blob string unchanged because
`edits` is not an array. So
**the model sees exactly the one multi-file blob it emitted, in-turn and forever within that turn**
— verified.

**This is the trap in the obvious plan.** Splitting only at persist is the single option where the
two views of one session disagree: the panel, the model and `--log-messages` all say one call, while
the file says three, and a reload silently rewrites the model's own history underneath it. A model
that trusts its context less after a reload is a worse outcome than either alternative. So the
model-facing split is not optional garnish on the storage split — it is what makes the storage split
safe.

The corollary is cheap. `on_completion_call` receives the **whole** history, so the assistant call
and its matching `Message::User` tool-result batch are in one pass and ids pair directly. The
persist site gets the same two sides in two separate `append_message` calls with no shared state,
which is why determinism is a correctness requirement *there* and merely good hygiene in the model
hook. One transform, two call sites, and the per-file content is the same envelope either way.

### Two independent axes (not the retired A/B/C strategies)

pi compatibility is a *storage* property, not an execution one, so "accept the blob" and "let pi
read the session" are separate changes that can be gated on separate evidence:

- **execution**
  - **Today**: a multi-file blob is rejected; the model retries, or had already emitted parallel
    per-file calls (rig dispatches those; costs nothing)
  - **Change**: accept the blob, split per file in `EditFile::call`
- **storage**
  - **Today**: one raw multi-file call; pi cannot read the file
  - **Change**: split at persist into N canonical single-path calls

Either can ship alone, and each is small. The storage axis presupposes the execution one — a call
that is rejected never produces the per-file results the splitter consumes — but the execution axis
alone is already a strict improvement over the status quo.

### Execution axis

- **Atomicity — resolve all files, then write the ones that resolved.** The two original bullets
  ("validate everything first across files" vs. "per-file atomic with partial success") contradict;
  this is the reconciliation. `apply_to_text` is pure over text, so every file can be resolved in
  memory before anything is written. That reports every problem in one round trip, and — because
  sandbox checks (`get_path_with`) and reads also happen in the first pass — a path outside the
  sandbox or a missing file is found *before* any write, not after. That is `bsed`'s per-file policy
  minus its one weakness (it writes as it goes). Global all-or-nothing stays available as a one-line
  change to the write step, but is the wrong default for a many-file refactor, where one stale file
  blocking the rest is exactly what `doc/bsed.md` §7.2 exists to reject.
- **Per-file IO errors fold into the report, never `Err`.** `edit_file` returns `Err` for a missing
  file, a non-UTF-8 read and a sandbox denial; one of those must not abort the remaining files.
- **Result shape**: `BatchItem` gains `path: Option<String>` so failures read
  `edit #2 (src/bar.rs): …`. `output.rs` needs **no change** — `display_tool_result` matches on the
  presence of `ui_view` + `model_view` (`output.rs:2169`), so an aggregate envelope with those same
  keys renders through the existing branch. The original "output path must learn it" is wrong.
- **Done** (in the rename): the "all blocks need to be within the same file" sentence existed twice,
  byte-identical, in the tool `description()` and the blob-parameter description. Both are now
  interpolated from one `BLOCK_SYNTAX` and one `BLOCK_POLICY` const, so the copies that were
  drifting when this was written cannot drift again. What is left for Phase 2 is only the *wording*
  — see W5.

### Storage axis

- **Per-file structured result** — one result must become N paired `toolResult` messages (providers
  reject unpaired tool-call ids). The aggregate envelope carries a `files` array alongside
  `ui_view`/`model_view`; that array is what the splitter reads.
- **Split at persist**, in `append_message`: the assistant `edit` tool call becomes N calls, and the
  matching tool-result message becomes N results. These are two independent `append_message` calls
  on the turn transcript (`agent.rs:696`) with no shared state, so they agree only because
  **determinism is a correctness requirement, not a cache nicety** — both derive ids from the
  original id plus the file index, never from a counter or a clock.
- **No new API for the per-file canonical value**: `render_blob` + `canonicalize_edit_value` already
  produce it, and `array_to_blob_value` inverts it idempotently (verified). `render_blob` drops the
  `:LINE` anchors, which is correct here — anchors are transient and the canonical array has never
  carried them.
- **This repairs compaction for free.** `collect_touched_files` (`session.rs:194`) reads
  `arguments.path` per `edit` call, so a multi-file blob stored raw contributes at most its
  top-level `path` and every header-only file is missing from the compaction summary. N stored calls
  fix it with no code.
- **`on_completion_call` is the second call site, not a bystander.** An earlier draft of this note
  claimed it "needs nothing", on the grounds that `blob_edit_tool_calls` derives the model-facing
  form from the stored canonical args. That is true only of the *reload* direction, and it misses
  the in-turn direction entirely — see *Three pipelines* above. The hook must apply the same split,
  in the same pass, over the assistant call and the matching tool-result batch, or the live view and
  the stored view of one session disagree. It is the easier of the two sites: one function call,
  both sides in scope, ids paired by inspection rather than by convention. Composition is
  `split(blob(strip(history)))` — split after blobing, so the transform sees the shape it is
  documented to accept, and idempotent either way (a history of single-file calls has
  `multi_file == false` and passes through).
- **Rejoining is the alternative worth weighing.** If N calls mid-turn is judged too much of a lie
  to the model's own emission, the deterministic inverse exists: group the synthetic calls by their
  id suffix in the same transform and re-render one multi-file blob, applied at *both* sites so the
  model sees one call and the file holds N. Do not apply the split at persist and skip the model
  hook — that combination is the inconsistency, not a policy.

### Display: the bogus `[1]`, and what the rename fixed

**Fixed in the rename.** Splitting inside the tool does not change the live panel, because
`on_tool_call` canonicalizes before `display_tool_call` and a blob that cannot canonicalize reaches
`get_preview` as-is. The `edit` branch used to read `edits` as an array (miss), then the legacy
`old`/`new` fields (miss, both empty), and called `edit_preview` with one empty op — an empty diff
plus a bogus `[1]` range, since `content.find("")` is `Some(0)`. The panel read `edit b.rs [1]` with
nothing under it, for a call that failed.

`get_preview` now canonicalizes its own input first, so a resolvable blob renders normally and an
unresolvable one (malformed, or spanning files) yields no edits at all — the label, with no range
and no diff. It also takes `path` from the canonical value, since resolving a blob can *supply* a
path from a header line that the raw args never carried.

This is the general lesson, and it is why the parser is allowed at this one display site:
**`get_preview` must never invent a range.** A line number it cannot justify is worse than no line
number.

### Files

`src/edit_blocks.rs` is a re-export shim — edit `crates/blockedit`, not the shim.

- **#**: W1–W3
  - **File**: `blockedit/src/{blob,wire,diagnose}.rs`
  - **Change**: split policy + cap, `EditArgs.files`, the envelope
- **#**: W4–W5
  - **File**: `src/tools.rs`
  - **Change**: plan/commit split, multi-file driver, deduplicated description
- **#**: W6
  - **File**: `src/output.rs`
  - **Change**: `get_preview` label-only for an unsplittable blob
- **#**: W7
  - **File**: `src/agent.rs`
  - **Change**: the shared split transform + `on_completion_call` site
- **#**: W8
  - **File**: `src/session/session.rs`
  - **Change**: the persist site

### Types

Everything new lives in `blockedit` (pure, testable, shared with `bsed`); the `Message`-level split
lives in picocode because `blockedit` has no rig types.

```rust
// blob.rs — the path policy for splitting
/// Split a parsed blob into per-file batches under the *multi-file* path
/// policy: **every** block must carry its own header — there is no
/// defaulting, and no `path` argument is consulted at all. A single-file blob
/// is not a special case: this one policy serves every block blob.
pub fn split_by_file(parsed: ParsedBlocks) -> Result<Vec<FileBatch>, String>;

/// `{"path": p, "edits": [{oldText, newText}, …]}` for one file — the exact
/// value that must land on disk. Anchors are dropped (transient).
pub fn file_to_canonical_value(path: &str, edits: &[ParsedEdit]) -> Value;

/// Multi-file args -> N canonical values in batch order. Empty (nothing to
/// split) when the blob is single-file, malformed, or already split.
pub fn split_canonical_values(args: &Value) -> Vec<Value>;

/// Upper bound on files in one call. Beyond it the blob is rejected whole,
/// before any file is touched.
pub const MAX_FILES_PER_CALL: usize = 32;

// wire.rs — the tool's Args, uniform across both accepted shapes
pub struct EditArgs { pub files: Vec<FileEdit> }
pub struct FileEdit  { pub path: String, pub edits: Vec<EditOp> }

// diagnose.rs — the multi-file result: a bare array, no wrapper
/// One file's entry. `result` is that file's single-file result verbatim:
/// a success-note string, or a `{ui_view, model_view}` failure envelope.
/// Serializes to a bare `[{path, result}, …]` — see *W3* for why there is no
/// aggregate wrapper and no top-level `ui_view`.
pub struct FileResult { pub path: String, pub result: serde_json::Value }
pub fn render_file_results(files: &[FileResult]) -> String;  // bare JSON array
pub fn parse_file_results(result: &str) -> Option<Vec<FileResult>>;  // display split
```

The array is what `display_tool_result` splits into per-file batches, and what W7/W8 read to produce
the N per-file calls and results. `BatchItem.path` from the original sketch is unnecessary: each
element's `path` carries the file, so `FileResult` supersedes it.

`EditArgs.files` is length 1 for the single-edit shape (inherently single-file), so the tool has one
loop and no shape special-case beyond construction. The legacy array shape is gone from the wire
entirely, so there are two shapes to construct, not three.

### The split transform

One function, two call sites. It is the only new piece of logic in the plan.

```rust
// agent.rs
/// Deterministic id for the i-th of n files of one split call. The count is
/// encoded so a reader can assert a group is complete.
fn split_id(orig: &ToolCallId, i: usize, n: usize) -> ToolCallId;   // "{orig}~f{i}n{n}"
fn split_multi_file_edit_calls(messages: Vec<Message>) -> Vec<Message>;
```

It rewrites two things, and must find them independently:

- **the call** — an `edit` tool call whose arguments are a blob that splits into more than one file
  becomes N calls, arguments `split_canonical_values(&args)[i]`, ids `split_id(&orig, i, n)`;
- **its result** — a `ToolResult` whose text parses as the per-file array becomes N results, ids
  taken by index, each carrying that entry's own `result` verbatim (its success note, or its
  `{ui_view, model_view}` envelope).

The result side pairs by *index*, not by re-deriving the grouping: the envelope already carries the
files in the order the call was split. That is what lets the two call sites agree without shared
state.

**Identity fields must be cleared on synthesized messages.** Set `provider: None` and
`signature: None` on synthesized `ToolCall`s, and `provider: None` on synthesized `ToolResult`s.
This matters at the `on_completion_call` site specifically: `ToolCall::wire_call_id()`
(`message.rs:650`) returns `provider.call_id` when present and *only* falls back to `id` otherwise,
and the Anthropic serializer writes `wire_call_id()` for both `tool_use.id` and
`tool_result.tool_use_id` — so inheriting the provider's id would put N identical `tool_use_id`s on
the wire and the results would be unmatchable. The persist site is safe either way
(`rig_message_to_pi_entries` writes only `id`; `pi_message_to_rig` rebuilds `provider: None`), but
clear it uniformly so the two sites cannot drift.

**Idempotence** is the safety property for both sites. An already-split history holds single-file
calls, so `multi_file` is `false` and nothing splits; a single-file result is a bare
`Replaced N block(s) …` string, not an envelope, so nothing splits. Both sites may therefore run
unconditionally, in either order, on repeated turns.

### Work items

Each is independently shippable and independently revertible.

**W1 — the path policy (`blockedit/src/blob.rs`). — DONE.** `split_by_file` is implemented alongside
`group_by_file`, which is left alone: `bsed apply -i` already ships against it, and re-basing it on
a different policy would change a released binary's behaviour for no gain. `split_by_file`, in
order:

1. **every** block must have a header — no defaulting to the `path` argument and no reuse-last. A
   block without one is an error naming its index, not a block silently attributed to whatever came
   before;
2. **the `path` argument is not consulted at all** — see *Decisions* below. The headers are
   authoritative;
3. group by the **normalized** path in first-seen order, returning `FileBatch.path` as first-spelled
   so the model still sees its own spelling;
4. reject the whole call if the group count exceeds `MAX_FILES_PER_CALL` — after grouping, since
   only then is the count known, and before any file is touched.

Note what is *not* here: a fallback. `group_by_file` has one, and reusing it would import exactly
the silent-retarget hazard the strict policy exists to close. The single-file blob still goes
through `merge_path` and still defaults freely — there is only one candidate, so a default cannot be
wrong.

*Do not cite `group_by_file`'s own comment as precedent.* The comment at
`crates/bsed/src/cli.rs:500` said a file named on the command line wins and headers are the
fallback, and called that "the same precedence as the `edit` tool's `path` argument" — both halves
were wrong: `group_by_file` does `edit.header.or(fallback)`, so the **header** wins, and the `edit`
tool has no precedence at all. **That comment is fixed** as the W1 drive-by, and now names
`split_by_file`'s no-fallback rule so the two cannot be conflated again.

*Invariants:*

- a header-less block is rejected, and never becomes a second file — the exact blob `group_by_file`
  turns into two batches;
- `./a.rs` and `a.rs` are **one** batch, not two;
- the batch path is the **first spelling**, not the normalized one;
- a header-less blob that is not multi-file is untouched — `merge_path` owns it;
- the cap counts **files**, not blocks: many blocks for one file is not a fan-out and is not capped.

*Tests:* the four rules; the `group_by_file` hazard as a named regression; first-seen ordering;
exactly 32 files ok / 33 rejected; the cap counting files not blocks; `merge_path`'s own behaviour
untouched.

*Two fixes found while implementing, both pre-existing:*

- **`group_by_file` named the wrong block.** Its error used `order.len() + 1` — the *group* count,
  not the block index — so three blocks naming `a.rs` followed by a header-less block reported
  "block 2" for block 4, pointing the model at a block that was fine. Now enumerated; pinned by
  `the_error_names_the_offending_block_not_the_group_count`.
- **A header-less block could not say *why*.** `header_candidate` requires a whitespace-free token,
  so a real filename containing a space (`my notes.rs`) silently produced a header-less block — and
  with `path` no longer consulted there was no way out but a message telling the model to add the
  line it had already added. `ParsedEdit` now carries `rejected_header` (the line above `@@@OLD`
  that was not taken as a header, when there was one) and the error quotes it back, splitting the
  two causes: a multi-word line is reported as possibly-a-spaced-path and routed to the single-edit
  form, a single token as a wrong path. This is the P1 lesson applied one level down — name the
  malformation rather than report an absence.

**W2 — the tool's Args (`blockedit/src/wire.rs`). — DONE.** `EditArgs` is now
`{ files: Vec<FileEdit> }`, with `FileEdit { path, edits }`. `from_wire` routes a **multi-file**
block blob through `split_by_file` and wraps the single-edit and single-file block shapes as a
one-element `files`, so the tool has one loop and no shape special-case beyond construction.

**Multi-file is now reachable by a model** — this is the commit that ships the feature. Routing is
on `parsed.multi_file`, not on "did it look multi-file": a single-file blob still goes through
`merge_path` and still defaults freely, which is the case W1's no-defaulting rule must *not* leak
into (there is only one candidate, so a default cannot be wrong).

*`to_ops` is now shared.* The `ParsedEdit → EditOp` map moved out of `from_wire` into
`blockedit::blob::to_ops`, and `bsed`'s private copy (`crates/bsed/src/cli.rs:677`) is deleted in
favour of it. It was going to be the third copy, and a copy is a place a new field silently fails to
reach.

*Tests:* all wire shapes still parse; a multi-file blob yields N files in blob order with per-block
anchors intact; a `path` argument neither supplies nor re-targets a multi-file blob; a header-less
block in a multi-file blob is rejected; a single-file blob is still one entry via `merge_path`. End
to end through the erased tool: one call edits three files, and a file that fails to match leaves
the others written.

*Two scope notes, both deliberate:*

- `FileEdit` has no `PartialEq`. `EditOp` deliberately has none, and widening a core type for a
  convenience nothing uses would be the wrong trade.
- **`EditFile::call` applies each file as it resolves.** That is the per-file failure granularity
  the tool description already advertises, and the single-file path is `edit_file` verbatim. What is
  *not* here is W4's resolve-everything-then-write-what-resolved pass, which would report all of a
  call's problems in one round trip instead of one file per trip. Until W4, a multi-file call is
  applied file-by-file and each file's own report is returned.

**W3 — the result shape (`blockedit/src/diagnose.rs` + `src/output.rs`) — DONE.** The multi-file
result is a **bare JSON array of per-file results**, one entry per file, each `result` being that
file's single-file result *verbatim* (a success note string, or a `{ui_view, model_view}` failure
envelope):

```json
[ {"path": "src/a.rs", "result": "Replaced 1 block(s) in ./a.rs"},
  {"path": "src/b.rs", "result": {"ui_view": "edit failed: …", "model_view": "…"}} ]
```

There is **no aggregate wrapper and no top-level `ui_view`**. The earlier
`MultiFileOutcome { ui_view, model_view, files }` sketch is withdrawn: the summary line had nowhere
to go, because every durable representation (model history via W7, disk via W8) is per-file, and a
"2 of 3 applied" roll-up exists only in front of two readers that both want per-file detail. `path`
is the one field the array must add, because a result inside an array is no longer sitting beside
the assistant call's `path` argument — that association existed only on the wire. Everything else is
the existing single-file result reused.

*Display (`src/output.rs`).* `display_tool_result` gains an array branch: when the result is a JSON
array of `{path, result}`, it renders each file as its own batch under a path label, reusing the
existing per-result rendering (the `ui_view` branch for a failure, the text branch for a success
note). This makes the live panel and a reloaded session — which stores per-file results — agree that
the report is per-file. The array branch must be checked **before** the existing
`Value::Array`-of-strings fall-through, which would otherwise stringify each object.

*Not changed:* `on_tool_result`. See *The result must survive to history*.

*Tests:* the array shape round-trips; `display_tool_result` on the array renders one labelled batch
per file and no aggregate line; a failure entry renders its `ui_view` and a success entry its note;
a single-file call is byte-identical to today (bare string / bare envelope, never a one-element
array); the array branch does not swallow an array-of-strings result from another tool.

**W4 — plan/commit — DROPPED, and its goal met by the `edit_files` loop.**

The original plan split `edit_file` into `plan_file` (never `Err`; every IO, sandbox and non-UTF-8
failure folded into the outcome as a string) and `commit_file`, so a driver could plan every file
and commit only the resolved ones. **That split is withdrawn**: with a by-file failure policy it
buys nothing observable. Per-file all-or-nothing already lives inside `edit_file`; reads and sandbox
checks already precede that file's write; and "resolve all, then write" only differs from "write as
you go" for *global* all-or-nothing, which is rejected above as the wrong default. It would have
cost a `PlannedFile` type, a function boundary, and the invariant that the single-file path stays
unchanged — to reproduce, in more code, the per-file outcome the W3 array already is.

What it was actually for — a per-file failure being **data, not control flow** — is now a fold in
the existing loop: a file that cannot be resolved, read, or written becomes that file's
`{path, result}` entry (naming the path) and the loop continues, so the model gets every file's
outcome in one round trip. Before this, the `?` in the loop aborted the whole call and discarded the
other files' reports, reducing a 3-file call to a bare `the tool failed`.

*Tests:* two files both apply; one fails to match and the other still applies, and the report names
both; **a missing (or unreadable, or sandbox-denied) file folds into its own entry naming the path,
the other files still apply, and the array always has exactly one entry per file**; success notes
name each file's own matched lines.

**W5 — the tool contract (`src/tools.rs`). — the de-dup is DONE; the copy is not.** The rename
already lifted the syntax and the policy sentence into `BLOCK_SYNTAX` / `BLOCK_POLICY` and
interpolated both into the description and the `blocks` parameter, so only the *wording* of
`BLOCK_POLICY` remains: state that a blob may span files, that each file is all-or-nothing, and that
a file which fails is reported without blocking the others. Do **not** advertise the
split-to-N-calls behaviour — that is a storage detail, not a model contract. Also state that a
multi-file blob needs a filename line above **every** block, so the stricter policy is something the
model is told rather than discovered.

*Test:* already in place — the schema test asserts `@@@OLD` appears exactly once in the description
and that the description interpolates `BLOCK_SYNTAX`, so the two copies cannot diverge again.

**W6 — display (`src/output.rs`) — DONE, ahead of the gate.** `get_preview`'s `edit` branch now
canonicalizes its own input and takes `path` from the canonical value. A blob that resolves renders
normally; one that does not (malformed, or spanning files) yields no edits, so the label carries
**no** range and **no** diff. This retires the pre-existing `edit b.rs [1]` label — which the rename
had to address anyway, since the model-facing field moved and `get_preview` is the one place that
reads it.

*Tests:* a multi-file blob renders a label with no `[1]`; a resolvable blob and a single-file blob
are unchanged.

### The view layer has one unit: the call-result pair

**A multi-file batch must render byte-for-byte as N single-file batches would.** This is not a
preference: the session file already stores N per-file calls and N per-file results (W8), so a view
that showed anything else would be showing a shape nothing else in the system holds.

*What the view layer actually receives.* It does not receive "calls" and "results" as separate
streams that could be grouped however it likes — rig fires them as **pairs**. `run_single_tool` (rig
`runner.rs:605`) calls `resolve_tool_call` at ~635 and `on_tool_result` at 720: the call and its
result are raised inside one function, once per tool. So the render unit is the pair, and between
pairs the order does not matter — each pair is a self-delimiting block (`boundary()` above, label,
diff, note, `end_tool_block()` below), so concatenating them in any order yields the same bytes.

*What that rules out.* A multi-file call is one hook event and its array result a second, so the
tempting shape — render all N labels and diffs at the call event, then all N notes at the result
event — produces **two contiguous runs** instead of N blocks. That is the shape that would have
looked wrong. The pair has to be kept whole, which means the call half must wait for the result
half.

*The mechanism* (`src/output.rs`):

- **where**: `display_tool_call`
  - **what**: stashes `last_tool`, and for a multi-file `edit` **returns without rendering** — the
    pair is incomplete
- **where**: `multi_file_blobs` / `per_file_blocks`
  - **what**: per-file call arguments, in first-seen order, each a complete single-file argument
    value
- **where**: `display_tool_result`
  - **what**: reads the stashed args back, pairs them with the array's entries by index, and for
    each pair calls `render_call_panel` then `render_result_panel`
- **where**: `render_call_panel`
  - **what**: the call half: `boundary()`, label, trailing diff lines
- **where**: `render_result_panel`
  - **what**: the result half: envelope/`sed`/note, ending in `end_tool_block()`
- **where**: `multi_file_pairs`
  - **what**: the pairing; a length mismatch degrades to rendering the result alone rather than
    guessing

`render_call_panel` and `render_result_panel` are the *same* functions a single-file call goes
through — there is no second code path, which is what makes the invariant structural rather than a
property someone must remember to maintain. The only multi-file-specific logic is deciding to defer.

Pairing is by index, from the same file list in first-seen order, which is the same derivation the
W7 and W8 splits use. A result replayed with no call in context falls back to per-file result panels
with a path label — the diff is gone because the call half is gone, but the report stays per file.

*Tests:* `a_multi_file_batch_renders_identically_to_n_single_file_batches` renders both shapes into
a capture buffer and asserts the strings are **equal**, then asserts the shared rendering really is
the single-file one (label, `@@` hunk, per-file note, once each).

**W7 — the model-facing site (`src/agent.rs`) — DONE.** `split_multi_file_ edit_calls`, called in
`on_completion_call` as `split(blob(strip(history)))`: N per-file calls and N per-file results, with
ids derived as `{orig}~f{i}n{n}`.

Three details that are load-bearing, not incidental:

1. **Ids are derived, never minted.** The persist path (W8) splits in a *different* pass over
   *different* messages, and both must independently arrive at the same id for the same file;
   re-applying must also be a no-op rather than re-derive a fresh random id and break correlation
   with an already-stored result. `~` is not a character any provider issues, so a derived id cannot
   collide with a real one — which is what licenses the synthetic calls carrying no `provider`.
2. **`provider` and `signature` are cleared.** Anthropic's wire requires `tool_result.tool_use_id`
   to equal a `tool_use.id` the provider issued, and `wire_call_id()` returns `provider` when
   present. N synthetic messages claiming one original provider id would be **unmatchable** — a 400,
   not a cosmetic issue. `None` means rig mints rig's own handle, and that is what travels.
3. **Both halves must be present.** A call whose result has not arrived yet (mid-turn) is left whole
   rather than split, so a partially-saved history never shows a call with a dangling result. This
   is why the transform runs over the whole history in one pass and matches call-to-result by id,
   not by position.

The split is **idempotent by construction**: re-running finds only single-file calls (the synthetic
ones) and non-array results, and leaves the history alone.

**`on_tool_result` needs no change, and that is load-bearing — see *The result must survive to
history* below.** The tempting shortcut is to rewrite the multi-file array into one aggregate
message at `on_tool_result` (the model emitted one call, so give it one result) and let the per-file
form appear only in storage. That **cannot** work: a `ToolResultAction::Rewrite` is not a transient
in-turn view, it *replaces the tool result permanently in history*, so the array is gone before the
split transform — or `append_message` — ever sees it. The comment this replaces ("the envelope
carries no arg-error prefix, so the raw envelope survives to the history") was not incidental; it
recorded the exact property the storage axis depends on. The aggregate therefore lives *inside* W7's
split (a one-line note that the group is N per-file results), not in a competing `on_tool_result`
rewrite. See *The result must survive to history* for the rig code pointers.

*Tests:* an in-turn multi-file call becomes N calls + N results with matching ids;
`provider`/`signature` are `None` on every synthesized message; a second application is a no-op; a
history with no multi-file call is byte-identical (identity).

**W8 — the persist site (`src/session/session.rs`) — DONE.** `canonicalize_edit_tool_calls` gains
the split, and `split_edit_tool_results` is its result-side sibling. Both stay
**within one message**, so `append_message`'s signature and return type do not change.

**Split *before* canonicalize, and that order is the whole mechanism.** `canonicalize_edit_value`
only rewrites a *single*-file blob — it deliberately stores a multi-file one raw, "until the storage
axis splits it". Splitting first means each per-file call canonicalizes to the ordinary
`{path, edits}` shape, so the on-disk JSONL is N pi-canonical single-path tool calls and
**nothing downstream needs to know multi-file editing exists** — including `collect_touched_files`,
which keeps reading `arguments.path` and now yields every file simply because there is one call per
file. (The original sketch called for teaching it to read `files[]`; that work evaporates.)

The two halves are separate messages appended in separate calls and never see each other, so their
agreement comes only from `split_id` being **one shared function** — now
`crate::edit_blocks::split_id`, used by both this file and the model-facing transform in `agent.rs`.
That is the reason it was moved out of `agent.rs` rather than duplicated.

*Tests:* the on-disk JSONL holds N pi-canonical single-path calls and N results whose `toolCallId`s
match them exactly (read back from the raw file, since the persisted shape is the contract); a
reloaded session's context agrees with disk; a single-file call is canonicalized but never split and
keeps its id; a plain-text (argument-error) result and an array result from another tool are both
left whole.

### The result must survive to history

The single most load-bearing constraint in Phase 2, and the easiest to break by accident.
**The tool's result string must reach `on_completion_call` (W7) and `append_message` (W8) intact.**
Both split the multi-file array; if anything replaces it first, the split has nothing to read and pi
compatibility is lost.

The trap is that rig offers an obvious, tempting place to put a friendlier message —
`on_tool_result` → `ToolResultAction::Rewrite` — and it looks like a transient "what the model sees
this round." **It is not.** A rewrite replaces the result *permanently in history*:

- **what**: `ToolResultAction::Rewrite`
  - **where (rig rev `1c2f440`, `crates/rig-agent/src/agent/`)**: `hook.rs` — "Replace the effective
    presentation sent to the model"
  - **what it does**: the replacement becomes the `ToolCallOutcome.content`
- **what**: how it lands
  - **where (rig rev `1c2f440`, `crates/rig-agent/src/agent/`)**: `runner.rs:743` —
    `Rewrite(replacement) => … tool_result_output(id, …, replacement)`
  - **what it does**: the history entry is built from `replacement`, **not** `exec.output()`
- **what**: proof
  - **where (rig rev `1c2f440`, `crates/rig-agent/src/agent/`)**: `runner.rs` test
    `rewrite_result_is_delivered_verbatim_not_reparsed` — asserts the tool's own output
    `!tool_result_text_in_history(..)` and the replacement *is* in history
  - **what it does**: the original never reaches the model
- **what**: also
  - **where (rig rev `1c2f440`, `crates/rig-agent/src/agent/`)**: `runner.rs` test
    `rewrite_result_does_not_mask_the_structured_outcome`
  - **what it does**: a rewrite changes what the *model* sees, not classification — so the bytes in
    history are the rewrite

Consequence: rewriting the array to an aggregate at `on_tool_result` would erase it before W7/W8
read it. The model would be happy for exactly one round, and the session would be permanently
un-splittable — the opposite of the goal. This is why `on_tool_result` "needs no change" is a
*requirement*, not an oversight, and why the anti-confusion affordance belongs **inside** W7's split
(a leading note that the single call became N per-file results) rather than in a rewrite that
competes with the data.

*Corollary for the two rewrites in `model_facing_tool_text`*, which now decides the model-facing
text for two unrelated reasons: the arg-error one **strips ceremony** (`tool_errors.rs:1-23` — a
weak model's self-correction measurably degrades behind the wrappers), while a future multi-file one
would **add structure**. They are opposite jobs; do not "unify" them. (Today only the strip applies
— the array is left alone so it survives.)

### Test matrix

- **Area**: split policy
  - **Where**: `blockedit` unit
  - **Cases**: 4 rules; header-less block rejected even with `path` set; `./a.rs`+`a.rs` → 1 batch;
    `path` conflict → error; ordering; 32/33 cap
- **Area**: wire
  - **Where**: `blockedit` unit
  - **Cases**: 2 shapes; multi-file → N; single-file unchanged; `edits` array rejected; `blocks` as
    array unrecognised
- **Area**: result shape
  - **Where**: `blockedit` unit
  - **Cases**: array round-trips; per-file `result` verbatim; single-file unchanged
- **Area**: tool e2e
  - **Where**: `tools::edit_args_wire_tests` (reuse `CwdGuard`, `#[serial]`)
  - **Cases**: 2 files apply; partial; sandbox denial; missing file; notes per file
- **Area**: display
  - **Where**: `output` unit
  - **Cases**: no `[1]`, no diff; canonicalizable unchanged
- **Area**: model hook
  - **Where**: `agent` unit
  - **Cases**: N calls + N results, ids paired, identity fields cleared, idempotent, identity
    without
- **Area**: persist
  - **Where**: `session` unit
  - **Cases**: JSONL pi-canonical; N results; reload no-op; touched-files complete
- **Area**: regression
  - **Where**: whole suite
  - **Cases**: existing `edit_file` / wire / output-preview tests pass unchanged

### Rollout

Two commits, not one. **W1–W4** (execution) is self-contained: multi-file blobs are accepted,
applied per file, and reported per file. It is reverted by reinstating one guard in `from_wire`.
**W7–W8** (storage + model-facing) is then added on top; if the model-facing split turns out to
confuse models, the deterministic rejoin is a small change *inside the same function* rather than an
undo.

**W1, W5 and W6 have already landed.** W5 and W6 came in with the `blocks` rename, which touched the
same two call sites the split needs. W1 landed next, as a pure path policy with no caller:
`split_by_file` is implemented and tested, but nothing routes to it yet — `from_wire` still goes
through `merge_path` and still rejects a multi-file blob. So multi-file editing is **not** available
to a model at this commit; W2 is what makes it so, and the remaining work is W2–W4 and W7–W8. The
only item still open in W5 is the *wording* of `BLOCK_POLICY` once multi-file ships. The rename
itself is already deployed and ungated, because it removes a wire shape rather than adding one.

W1 landing first is deliberate: it is pure, it touches no wire shape, and it puts the strict policy
— plus the two error messages — under test before anything can call them. It is also revertible on
its own, which matters more than usual here because it is the piece that could silently re-target a
file if it were wrong.

### Risks

- **Risk**: `group_by_file` used for the agent by mistake, silently re-targeting a file
  - **Mitigation**: `split_by_file` is the only entry point `from_wire` may call; the conflict case
    is a named regression test
- **Risk**: N identical provider ids on the wire
  - **Mitigation**: clear `provider`/`signature`; asserted in the W7 tests
- **Risk**: Prompt-cache churn from a non-deterministic rewrite
  - **Mitigation**: ids derive from `(orig, i, n)` only; the idempotence test plus a fixed-golden
    transform test
- **Risk**: The model reads a call it did not emit as N calls
  - **Mitigation**: the rejoin is the escape hatch, and it is deterministic
- **Risk**: Partial write surprises a cross-file refactor
  - **Mitigation**: the model-facing envelope enumerates every file's outcome; global all-or-nothing
    is a one-line change to the write step
- **Risk**: Unbounded fan-out from one string argument
  - **Mitigation**: `MAX_FILES_PER_CALL`, checked before any file is touched
- **Risk**: `bsed` drift
  - **Mitigation**: both binaries share `blockedit`; W1–W3 add to it rather than fork it, and
    `blockedit`'s tests (plus `bsed`'s own) gate the change
- **Risk**: A stricter multi-file rule than the single-file one costs a round trip on a sloppy blob
  - **Mitigation**: accepted: the cost is one filename line per block in a format whose header line
    is already idiomatic, and the alternative is a silent wrong-file write

### Open questions (reduced)

- ~~Reuse-last vs. mandatory per-block path~~ → **mandatory** (decided, not defaulted). Aider
  reuses, and `group_by_file` implements reuse-last — but reuse-last is only safe while there is one
  candidate file, and a multi-file blob is exactly the case where there are not. Every block carries
  its own header, and `path` is **not consulted at all** (see *Decisions*). See
  *Path handling (multi-file, Phase 2)*. The prose-line hazard is separately covered, because
  `header_candidate` only accepts a whitespace-free token — and is now *diagnosed* rather than
  merely avoided, via `ParsedEdit.rejected_header`.
- ~~Cross-file atomicity~~ → resolve-all, write-what-resolved.
- ~~Migrate persisted sessions~~ → **no migration**: the canonical form does not change, and every
  form the parser produces already has a legacy reader (`canonicalize_edit_value`,
  `EditArgs::from_wire`).
- ~~`MAX_FILES_PER_CALL`~~ → **32, shipped in W1**. A cross-file refactor fits comfortably under it
  and the work is bounded; the single-file form caps blocks not at all, so there was no existing
  precedent to copy. The cap counts *files*, not blocks.
- **Open, decide at W7**: N blobs or one rejoined blob in model history. The split ships N; the
  rejoin is the follow-up if measurement says otherwise.

---

## Decisions

- **Field names**: the model-facing blob is `blocks` (a string); the canonical array is `edits`
  (pi's, and it never crosses the model boundary). One name per thing.
- **The `edits` array is not a wire shape.** Accepted only on the storage side; a model that sends
  it gets a named correction.
- **`startLine` semantics**: an **anchor** (near-line hint), not a search window; never persisted,
  never served to the model.
- **`endLine`**: **dropped** (see *No `endLine`*).
- **Anchor source is per shape (B)**: single edit → the `path` argument (`path:LINE` suffix or
  `startLine`, which must agree if both are given); blob → `path:LINE` headers only (a top-level
  `startLine`/`path:LINE` is stripped and ignored).
- **Path normalization** for the conflict check: reuse `path_util::resolve_normalized` (resolve both
  sides against cwd, then compare).
- **Every block names its own file, in every blob.** Not only multi-file ones. There is no
  defaulting to the `path` argument and no reuse-last, and there is no longer a "single file is safe
  to default" exemption: one rule, one shape, and a model that has learned it never has to ask which
  case it is in. The exemption was dropped because it bought nothing — a block already carries its
  `path:LINE` — while costing a rule to state, a rule to test, and a second code path (`merge_path`)
  whose only remaining job was the lax case. With it gone, the `path` argument is read for exactly
  one shape: the OpenAI single-edit form, which has no per-edit file of its own.
- **`path` in the block form**: MUST NOT be supplied when the blob spans files, and SHOULD NOT be
  supplied for block editing at all — **block paths are authoritative**, so `split_by_file` never
  reads the argument. `path` is the OpenAI single-edit form's file argument and nothing else. This
  *strikes* the earlier plan to require a `path` argument to `same_path`-agree with every header: a
  blob that already names its files has answered the question, and a redundant argument reconciled
  against N headers is a second source of truth that can only ever conflict. There is no defaulting
  *and* no reconciliation, so nothing in this path can be silently re-targeted.
- **Line numbers**: *encouraged*, not required. A block needs its file path; the `:LINE` suffix is a
  matching aid for repeated text and is free to supply when the model has just read or grepped the
  file. Consistency across blocks costs nothing either way, which is why the strict rule below does
  not depend on a line number being present.
- **Blob rendering**: the path appears once, as a header line above the first block, with no `:LINE`
  suffix. For a single-file blob the text path may be omitted entirely (the `path` argument then
  supplies it); for a **multi-file** blob every block carries its own header, and `path` is optional
  but must agree with all of them.
- **Per-block anchor**: a header may be `path:LINE`, supplying that block's nearest-match hint (see
  *Per-block line anchors*); transient (not persisted, not rendered).
- **`path:LINE` on the `path` argument**: accepted in every shape; the suffix is always peeled off
  before path resolution. It is an anchor only for the single edit (alias of `startLine`, must
  agree); in the blob form it is stripped and ignored.
- **Tolerance**: `(context_lines / 3).min(4)`; unique matches ignore it.
- **Success string**: just `Replaced N block(s) in ./f`; it carries no match location. The resolved
  anchor lives in the rewritten block header (`path:LINE`), which is the single source of truth.
  Superseded by `261018_minimal_edit_context_anchors.md`; `N` is now the post-split block count.

## Implementation status (W1 — the multi-file path policy — done)

- `blockedit::blob::split_by_file`: the four rules below, no caller yet. `MAX_FILES_PER_CALL = 32`;
  grouping is keyed on the normalized path while `FileBatch.path` keeps the first spelling.
  `group_by_file` is untouched apart from the block-index fix, and `bsed` is unaffected.
- **Every block names its own file — in every blob, not just multi-file.** No defaulting to the
  `path` argument and no reuse-last. `split_by_file` takes no `path` at all, so there is no code
  path by which a redundant argument could re-target a file. The agreement-with-every-header rule
  from the original plan is **struck**: block paths are authoritative and `path` is not reconciled
  against them. The old single-file exemption is **also gone** — one rule rather than two (see
  *Decisions*).
- **The missing-file error names the block, quotes back what was written, and never offers `path`.**
  A bare "names no file" would tell the model to add a line it may already have added, so
  `ParsedEdit.rejected_header` carries the line that was *not* taken as a header. The two causes are
  distinguished: a multi-word line is reported as possibly-a-spaced-path (unaddressable by a header)
  and routed to the single-edit form; a single token is reported as a wrong path. This is the P1
  lesson — name the malformation, do not report an absence — applied one level down.
- **Two pre-existing bugs fixed on the way through**, both now pinned by tests: `group_by_file`
  reported the *group* count where it meant the block index, and a filename containing a space was
  silently unrepresentable with no way out.
- The `bsed` comment drive-by is done: `crates/bsed/src/cli.rs:500` no longer claims its precedence
  matches the `edit` tool's, and now names `split_by_file`'s no-fallback rule.

**Now wired (W2).** `EditArgs::from_wire` routes a multi-file blob through this function, so a model
can reach it. A single-file blob still goes through `merge_path` and still defaults freely.

- `src/edit_blocks.rs` (new): `parse_blocks`, `merge_path`, `render_blob`, `canonicalize_edit_value`
  (→ array, best-effort), `array_to_blob_value` (→ blob). Unit-tested. *Since moved: the file is now
  a re-export shim over `crates/blockedit/src/{blob,wire,resolve,apply,diagnose,path}.rs`, so the
  `src/edit_blocks::*` paths named throughout this document still resolve but the code lives in the
  crate.*
- `src/tools.rs`: `EditArgs` reduced to `{path, edits: Vec<EditOp>}` with a manual `Deserialize`
  (`from_wire`) selecting the variant from the raw JSON; hand-written flat `parameters()` (no
  `anyOf`).
- `src/session/storage.rs`: the `edit` normalizer branch now delegates to
  `edit_blocks::canonicalize_edit_value` (covers the `blocks` blob string, `old`/`new`,
  `oldText`/`newText`, and the already-canonical array).
- `src/session/session.rs`: `append_message` canonicalizes `edit` tool-call arguments before
  `rig_message_to_pi_entries`, so the JSONL and in-memory index are pi-canonical.
- `src/agent.rs`: `on_tool_call` canonicalizes `edit` args before display; `on_completion_call` is
  now unconditional and applies `blob_edit_tool_calls` (array→blob) to the whole history; the
  default system prompt's edit guidance was updated.
- `src/output.rs`: the parser stays out of `get_preview`'s hot path, but the function now
  canonicalizes its input so a blob it can resolve renders normally and one it cannot renders no
  diff and no range.

New tests: block parse/merge round-trips (`blockedit`), flat-schema shape, block-form
apply/infer/conflict (`tools`), persisted-canonical and legacy-normalize (`session`), history
blobification (`agent`). Full suite green.

## Implementation status (`blocks`, no array on the wire — done)

- `blockedit::blob`: `BLOCKS_FIELD` + `blob_text`, the one accessor every site uses to find the
  blob, so the field name is stated once. A blob string under a pre-rename `edits` as a **string**
  is still read (`LEGACY_BLOCKS_FIELD`), so the rename does not break callers that never saw it.
- `array_to_blob_value` emits `blocks`; `canonicalize_edit_value` reads it. The canonical array
  keeps the name `edits` — that is pi's, and it stays.
- `wire::EditArgs::from_wire` accepts **two** shapes: the `blocks` blob and the single-edit scalars.
  The array branch is gone, replaced by a correction that names `blocks`; `op_from_value` and the
  legacy-array tests went with it.
- `tools.rs`: the schema advertises `blocks` and no `edits`; the block syntax and the policy
  sentence moved into `BLOCK_SYNTAX` / `BLOCK_POLICY` consts, interpolated into both the description
  and the parameter, so the two copies that were drifting can no longer. A test asserts no
  advertised property is an array. The `sed` prompt's "pass them as its `edits` argument" now says
  `blocks`, in picocode and in `bsed`'s banner.
- `output.rs`: `get_preview` canonicalizes, and takes `path` from the canonical value. This retires
  the bogus `edit b.rs [1]` label.
- Tests: the array-on-wire test became `an_edits_array_is_rejected_and_writes_nothing` (it now
  proves the file is untouched); `canonical_multi_edit_applies_all` was deleted as a duplicate of
  `block_form_applies_all_edits`; new coverage for field non-interchangeability, and for the legacy
  `edits` string winning only when `blocks` is absent.
- **Second rename**, `batches` → `blocks`, carried by the `sed` tool: the field, the constant, and
  the two description consts all moved together (`BLOCKS_FIELD`, `BLOCK_SYNTAX`, `BLOCK_POLICY`).
  See *Parameter name* for why the middle name was wrong. The rename was mechanical, so it went
  through `sed` + `bsed apply` rather than by hand — with one exception worth recording, below.

**A tool-behaviour note worth keeping.** `bsed` refused to express `src/tools.rs` and
`src/output.rs` as blocks, and was right to: both contain literal `@@@OLD` lines (the block-syntax
example and the preview banner), which a block file cannot represent — the exact hazard
`doc/bsed.md` §7.3 documents. It named the way through (`oldText`/`newText`) and the two files were
finished that way. The refusal is the feature, not an obstacle: a rename that also rewrote a
block-syntax example is exactly the change a block-based editor cannot represent safely.

**Deliberate consequences.** A model that reaches for pi's array now spends a round trip on the
correction — the intended trade, since the alternative is a batch that a harness may silently
stringify into nothing.

**Correction, after shipping.** The rename first shipped *requiring* the new name on ingress:
`blob_text` read `blocks` only, and a pre-rename `{"edits": "<blob>"}` — the shape this tool had
advertised for its whole life — fell through to the generic "no recognizable edit arguments". That
was wrong, and it was the more likely caller of the two: a harness, a model, or a session recorded
before the rename. `blocks` is the name we *ask* for, not one we *require*.

`blob_text` now reads `blocks`, then falls back to a **string** `edits` via `LEGACY_BLOCKS_FIELD`,
with the array deliberately excluded (that is pi's canonical form, and `from_wire` rejects it by
name). Two rules, because both halves matter: `blocks` wins when both are present, and a malformed
`blocks` is an error rather than a silent fall-through to a healthy `edits` — otherwise a typo in
the new name would apply an edit the model did not ask for.

The earlier justification ("a pre-rename blob was stored raw precisely because it did not
canonicalize, so it is display-only either way") was a *storage* argument applied to the *wire*,
where it does not hold. Keeping the alias recovers a small thing too: a session recorded before the
rename now normalizes on load.

## Implementation status (anchors — done)

- `src/edit_blocks.rs`: `ParsedEdit` carries a per-block `start_line`; `split_line_anchor` peels a
  trailing `:LINE`/`:LINE:COL`; `merge_path` and `render_blob` speak `ParsedEdit`.
- `src/tools.rs`: `anchor_tolerance`; `edit_file` enumerates occurrences and resolves
  nearest-within-tolerance (no more byte windows); `from_wire` takes the anchor per shape (B);
  `parameters()` documents the anchor and drops `endLine`; `EditOp.end_line` removed.
- `src/edit_match.rs`: `diagnose` simplified to whole-file; `non_unique` suggests the `path:LINE`
  header; `anchor_out_of_tolerance` / `anchor_ambiguous` added.
- `src/config.rs`: `EditEndLine` removed.
- `src/output.rs`: `endLine` read dropped.
- `src/agent.rs`: system prompt documents the `path:LINE` anchor.
- `src/tools.rs`: `path:LINE` on the `path` argument (honored for a single edit;
  stripped-and-ignored in the blob form); the success string echoes match locations when unanchored.

New tests: nearest-duplicate pick, within/out-of-tolerance, unique-ignores anchor, tie-is-ambiguous,
tolerance scaling, header anchor capture, `split_line_anchor` forms, top-level `startLine` ignored
in blob form, `path:LINE` single-edit anchor + conflict, suffix stripped in blob form,
success-string location hint. Green.

## Notes / follow ups

Each idea below is evaluated against the current code (findings verified against rig rev `1c2f440`,
a live probe of `edit_file`, and probe tests against the `blockedit` crate). Status is per item, as
marked.

### 1. Multi-file batches split into single-file batches — investigated

**Idea**: accept a multi-file blob, split into per-file batches before execution → per-file failure
domain, preserved pi session compatibility.

**Status: investigated; the design is written up under *Phase 2* above.** This note is kept for the
findings that do not belong there.

**Status quo, re-verified.** `merge_path` rejects a multi-file blob (`blob.rs:367`),
`EditArgs::from_wire` wraps it as a self-correcting argument error, and the model retries — one
extra round trip unless it had *pre-emptively* emitted several parallel `edit` calls in the same
response (rig dispatches parallel tool calls; that path already works and costs nothing). On the
persistence side the call is stored as the raw `blocks` string, because `canonicalize_edit_value`
returns a multi-file blob unchanged (verified), so
**pi compatibility for multi-file is already degraded today**. Any implementation is a strict
improvement.

**The three strategies are not rivals.** A (split inside the tool) changes the *execution* axis; B
(split into N stored calls) changes the *storage* axis; C is both. Treating them as alternatives —
as this note originally did — hid two things: that pi compatibility is a storage property, and that
accepting the blob and letting pi read the session can ship independently and be gated on different
evidence. The A/B/C table is retired; see *Two independent axes* and *Three pipelines* in Phase 2.

**The hook-surface constraint stands.** No rig `1c2f440` hook can expand one assistant message into
N before dispatch: `ToolCallAction` is `{Run, Rewrite, Skip, Stop}` (it rewrites *one* call's
arguments, never the call list), `ToolResultAction` is `{Keep, Rewrite, Stop}` (presentation, not
structure), and `on_completion_response` / `on_model_turn_finished` observe, retry or stop only.
Verified. So a multi-file call is necessarily *one* call to the dispatcher and one result on the
wire — the split is a boundary transform, applied either when the turn's messages are persisted
(`append_message`) or when the request is built (`on_completion_call`), and the two are genuinely
different moments.

**They have to be both.** An earlier draft of this note reasoned that `append_message` alone
sufficed, because the model-facing history is derived from the stored canonical args. That is true
only after a reload, and it misses the in-turn direction: rig's live `history` is not what
`append_message` writes, so a persist-only split leaves the model looking at one multi-file blob
while the file says three. See Phase 2, *Three pipelines*.

**Determinism is a correctness requirement at persist, and only hygiene in the model hook.** The
assistant-call split and the tool-result split are two independent `append_message` calls with no
shared state, so at that boundary they agree only because both derive synthetic ids from the
original id plus the file index. `on_completion_call` holds both sides in one pass and pairs by
inspection — but it runs on *every* completion call, so the transform is deterministic there too,
which is what keeps the prompt-cache prefix stable.

**The "surprising to agents" question answers itself against the wrong baseline.** pi cannot
represent a cross-file batch at all and its own prompt guidance steers per-file, so a model
following pi's habits emits per-file calls. Once the split reaches the model hook, the model would
then *see* N per-file calls where it emitted one — which is either the nudge we want or an avoidable
divergence, and the deterministic rejoin decides it (Phase 2, *Storage axis*). A tool call becoming
several is not surprising where the alternative is a hard error, and it is not surprising at all if
the same session then reloads as N calls, which is the consistency that actually costs something to
get wrong.

The stricter path policy interacts well with this: because a multi-file blob must name **every**
file, the split has no ambiguity to resolve and no rule to get wrong. A model that under-specifies
gets a named error instead of a block landing somewhere plausible.

**One thing both A and B leave unfixed**, and worth not forgetting: live display. `on_tool_call`
canonicalizes, and a multi-file blob cannot canonicalize, so the panel renders an empty diff and a
bogus `[1]` range today. Splitting the execution does not fix it. See Phase 2, *Display*.

### 2. Ratio-based anchor tolerance ✅ *implemented*

**Idea**: tolerance based on the distance *ratio* between the two closest candidates rather than
context size, with a high absolute cap; reject close-ties at large deviation.

**Evaluation — the direction is right because context size is the wrong axis.** The anchor's only
job is choosing among ≥2 exact candidates. Once ≥2 candidates exist, how much context the block
carries is irrelevant to *which* candidate is intended — what matters is how decisively the anchor
favors one over the others. (Context scaling isn't wasted, though: with a *single* candidate the
anchor is ignored entirely, so context only ever influenced the multi-candidate case, which is
exactly where it's a proxy.)

**Proposed rule** (additive to today's, so every currently-accepted call stays accepted):

```text
accept nearest candidate iff
    d1 ≤ tol_ctx(old_text)                       # current rule, kept as fast path
    OR (d1 ≤ CAP AND d2 ≥ RATIO × d1)            # NEW: decisive-skew escape hatch
and the nearest candidate is strictly unique      # d1 tie → ambiguous, as today
```

- `d1`/`d2` = distances from the anchor to the nearest / second-nearest candidate. `RATIO ≈ 2–3`,
  `CAP ≈ 50–100` lines ("10s of lines" per the note) — constants to tune with tests.
- **Why additive**: the current `tol_ctx` fast path preserves today's behavior exactly; the ratio
  path only *adds* accepts where the anchor is stale (the model read the file earlier, subsequent
  edits shifted things) but the near candidate is unambiguous. Nothing that passes today starts
  failing, except close-ties at large deviation — which already fail today (out of tolerance), so
  that's not a change either.
- **Tie handling stays as-is**: equal distances → `anchor_ambiguous`. The ratio rule naturally
  refuses ties (`RATIO > 1`), so no special case.
- **Edge cases to pin in tests**: `d1 = 0` with a second candidate on the *same line* (two matches
  on one line → tie at 0 → must stay ambiguous);
  >2 candidates (only `d1` vs `d2` matter — a far third candidate is irrelevant to the choice); `d1`
measured to occurrence *start line* today — consider min-distance to the occurrence's line *span*
instead, so a model anchoring mid-block isn't penalized by block length.
- **Error message impact**: `anchor_out_of_tolerance` currently says "none within N lines" — with
  the skew rule it should report `d1`, `d2`, and the candidate lines so the model can see *why* the
  pick was refused ("nearest at line 140 (40 away), next at 145 (45 away) — too close to call").
- Existing tests (`anchor_out_of_tolerance`, tolerance scaling) need updating; the current
  `anchor_tolerance` doc comment claims context size is the rationale and would need a rewrite.

**Verdict**: straightforward, orthogonal to 1 and 3, strictly better diagnosis. Implement alongside
an update to 3's failure messages.

**Decisions (implemented)**:

- Rule shipped as `edit_match::anchor_accepts(d1, d2, tolerance)`:
  `d1 <= tolerance || (d1 <= ANCHOR_SKEW_CAP && d2 >= ANCHOR_SKEW_RATIO * d1)` with `RATIO = 3`,
  `CAP = 60` — additive to the old tolerance, so every previously accepted call still passes. Ties
  (`d1 == d2`) can only pass via the tolerance clause and are separately rejected, exactly as
  before.
- `d1` remains distance-to-occurrence-start-line (the span refinement was skipped for now — it
  complicates the ranking for a case the ratio rule already tolerates; revisit if models are seen
  anchoring mid-block).
- `anchor_out_of_tolerance` now reports `d1`, `d2`, both candidate lines, and the rule's thresholds;
  the test asserting the old "none within" wording was updated.

### 3. Anchored-first, claim-based resolution ✅ *implemented* (with refinement)

**Idea**: resolve line-anchored blocks first; accept an un-anchored block if its candidate set
shrinks to positions not overlapping previous matches.

**Evaluation — correct, and the investigation surfaced a missing invariant.** A probe against the
current `edit_file` (since removed; not committed):

```text
file: "hello world\n"
edits: [{old: "hello", new: "bye"}, {old: "hello", new: "hi"}]   # same unique old_text
→ result: "Replaced 2 block(s) … (matched lines 1, 1)"
→ file:   "hiorld\n"          # CORRUPTED — both edits resolved to byte range 0..5
```

`EditOp` documents "must not overlap with any other edit's old_text in the same call", but
**nothing enforces it**: each edit resolves independently, and the apply loop (sort by start desc,
`replace_range` on the original offsets) is only sound for *disjoint* ranges — overlapping ranges
silently corrupt (and depending on lengths can panic on a non-char-boundary offset). The failure
mode needs no repeated text at all: two *different* `old_text`s that overlap in the file corrupt the
same way.

So this item is not only a disambiguation improvement — the claim-tracking it requires is the same
machinery that enforces disjointness. It should ship **with** an overlap check, closing a
correctness hole, not just a UX gap.

**Implemented algorithm** — two passes, split by anchor (not by uniqueness), both in block order,
all-or-nothing preserved:

1. **Pass 1 — line-anchored blocks**, in block order. Resolution: unique match short-circuits;
   otherwise nearest + item-2 rule (tolerance OR skew), ties ambiguous. The chosen match's byte
   range is then checked against all existing claims: **overlap → conflict, reject** — never
   silently re-aimed to the next-nearest candidate (an anchor is a positive statement of location;
   two anchors on one region is a model error worth surfacing). On success the range is *claimed*.
2. **Pass 2 — content-only blocks**, in block order. Partition candidates against the accumulated
   claims (pass 1 + earlier pass 2):
   - exactly one free → accept and claim (so later blocks see the new constraint — narrowing chains
     transitively through pass 2 as well);
   - zero free → `all_candidates_claimed` (names the claiming edits);
   - ≥2 free → `non_unique` listing the *free* lines and the excluded count. Never order-based
     guessing between 2+ free candidates — that would be a coin flip the anchor refused to make.
3. **Failures claim nothing.** A failed block (not-found, out-of-tolerance, tie, conflict) adds no
   claim, so later un-anchored blocks still see its region and report plain ambiguity — the
   diagnostic the model can fix by anchoring. Both passes run to completion regardless of earlier
   failures (full diagnostics in one response); failure list is sorted by block index for stable,
   block-ordered output.
4. Failures anywhere → nothing applied (unchanged).

**Notes**:

- The two-pass order (anchored first, *regardless of batch position*) is what fixes the observed
  scenario — the model anchors blocks 2..n and leaves block 1 un-anchored expecting elimination;
  pass 1 claims line-anchored matches, pass 2's block 1 then sees only the remaining candidates.
  Un-anchored *unique* matches live in pass 2: anchors are positive statements of location and get
  first claim on contested regions; uniqueness is a passive property.
- A wrong anchor in pass 1 produces a wrong claim, which can cascade into a wrong pass-2 acceptance.
  Accepted trade-off: batch editing with this risk still vastly beats `sed` and friends, which is
  what models reach for when batches get too finicky. Bounded by item 2's tolerance + disjointness +
  all-or-nothing (any failure discards the whole batch → one retry), and failure messages name the
  claiming edit ("already matched by edit(s) #1") so the model can untangle it.
- Overlap for *identical* `old_text` is exact-range identity (candidates from one `match_indices`
  pass are non-overlapping by construction); for *different* texts it's genuine range intersection.
  One predicate (`start < other_end && other_start < end`) covers both.
- **Failure-claims-nothing** (refinement): a failed block contributes no claim, so later un-anchored
  blocks see its region and surface plain ambiguity — the more productive diagnostic (anchor it)
  rather than "everything is claimed". Both passes always run to completion; the failure list is
  sorted by block index so diagnostics arrive in block order.
- Interplay: this item's pass 2 makes item 2's error messages more load-bearing (the model must
  understand anchors to fix pass-2 failures) — implemented together, as recommended.

**Verdict**: implemented with the refinement above; higher priority than it looked because it
carries the disjointness bug fix (the probe's `"hiorld"` corruption is now a loud
`overlapping`/`all-claimed` failure).

**Decisions (implemented)**:

- `resolve_edits()` in `src/tools.rs` owns both passes; claims are
  `(edit_idx, byte_start, byte_end)` and only successful resolutions ever push one.
- Conflict on anchored overlap → `overlap_conflict` naming both edits and both line ranges
  (`render_line_span`: `line N` / `lines A-B`).
- Pass-2 zero-free → `all_candidates_claimed` (names claimers; single- occurrence phrasing for the
  probe case); ≥2-free → `non_unique` with an `excluded` count (message lists only *free* lines).
- Diagnostics: `failures.sort_by_key(edit_index)` before batching; full completion of both passes
  before the all-or-nothing gate.

### Suggested order — completed

1. ✅ **3 + 2** (implemented in [`f05d078`](https://github.com/gwicke/picocode/commit/f05d078)) —
   the skew rule and claim-based resolver are live; 653 tests pass.
2. **1** — multi-file: the gate is still unmeasured (no session corpus was reachable to run the
   grep), with the design and the work breakdown done (W1–W8, *Phase 2*). **W1 has landed** — the
   strict `split_by_file` path policy, plus the `group_by_file` block-index fix and the
   `rejected_header` diagnosis, all with no caller yet. What remains ships as two commits:
   1. **W1–W2 (execution) — DONE.** Multi-file blobs are accepted, split per file, and applied with
      per-file failure granularity. What remains is **W3–W4 are also now in**: the per-file result
      array, its display split, and the per-file fold for an unreadable file, so one round trip
      reports every file's outcome. Reverted by one guard in `from_wire`.
      **Extended past the original W2 scope**: strictness is now uniform rather than
      multi-file-only, which also removed `merge_path` from the block path entirely and forced
      `render_blob` to head every block. Both were consequences, not extra features — see
      *Decisions*.
   2. **W7–W8 (storage + model-facing)** — one split transform at *both* egresses that see the call:
      `append_message` (N canonical single-path calls on disk, so pi can read the file, which also
      repairs compaction's touched-files list) and `on_completion_call` (N calls in-turn, so the
      live view and the reloaded view of one session agree). Persist-only is the one option to
      avoid: it makes the same session read as one call mid-turn and three after a reload.

   No canonical-form migration and no session migration.

---

## Audit — `e35717d` → HEAD, and what is actually broken

Requested after a session in which block editing could not be driven at all. The premise going in
was that the refined error reporting had gone missing. It has not. What *is* wrong is real, and none
of it is that.

### Nothing was lost from `e35717d`

`e35717d` ("make the edit tool's argument errors terse and line-based for the model") is an
**ancestor of both sides** of the `7d7fd3b` merge, so nothing about it could have been dropped by
the merge resolution. Verified by content, not by ancestry alone:

- **Added by `e35717d`**: `render_blob_structure` (the line-based `Actual:`)
  - **`e35717d`**: present
  - **`57e069d`**: present
  - **HEAD**: present
- **Added by `e35717d`**: `actual_summary`
  - **`e35717d`**: present
  - **`57e069d`**: present
  - **HEAD**: present
- **Added by `e35717d`**: `is_path_header`
  - **`e35717d`**: present
  - **`57e069d`**: present
  - **HEAD**: present
- **Added by `e35717d`**: `strip_arg_error_prefixes`
  - **`e35717d`**: present
  - **`57e069d`**: present
  - **HEAD**: present
- **Added by `e35717d`**: `arg_error_view` (the two-column table)
  - **`e35717d`**: **absent**
  - **`57e069d`**: present
  - **HEAD**: present

The only deletions in the whole error path between `e35717d` and HEAD are two lines of mine, both
deliberate: `op_from_value` and its test `legacy_array_shape_still_parses`, which went when the
canonical array left the wire (`81f6fcf`), replaced by
`the_edits_array_is_rejected_with_a_named_correction`.

The error path at HEAD is byte-equivalent to `57e069d` and **strictly better than `e35717d`**, which
predates `arg_error_view` entirely. Reverting does not restore error reporting, because none of it
was lost.

### What is actually broken

**F1 — the client cannot express a block call, and the tool's error does not say so.** Every
block-shaped call in the session arrived as

```text
sent:    {"path": "probe.py", "edits": "<blob>"}
arrived: {"edits": {"path": "<blob>"}, "path": "probe.py"}
```

The blob arrives as an **object**, mislabelled under a `path` key, and `newText` was dropped from
single-edit calls. No tool build can accept that: the pre-rename parser wanted a *string* under
`edits`, the post-rename parser wants a *string* under `blocks`, and neither ever sees one.
Rebuilding on `57e069d` changed the error text from ``or `blocks` `` to ``or `edits` `` and the
behaviour not at all.

The tool's reply — *"no recognizable edit arguments"* — is true and useless. The malformation is
specific and nameable: a property that should hold a blob string is holding an object.

**F2 — the advertised schema and the parser are not cross-checked, so a client can be handed a stale
one and nothing notices.** In the same session the schema offered advertised `edits` (string) while
the tool behind it parsed `blocks`. The repo was self-consistent and green throughout, because every
test reads the live `parameters()` and the *served* copy is not observable from inside. That is the
durable class behind F1's sibling failure: the tool was fine, the contract was stale, and no test
could see it.

**F3 — a pre-existing cwd race in the test suite, surfaced by the rebuild.** On `57e069d` three lib
tests fail deterministically — `sed_preview_is_read_only…`,
`sed_preview_reports_a_rejected_expression…`, `grep_text_uses_ripgrep…` — all with
`./x: No such file or directory`. Root cause, localized: `src/tools.rs` carries **two** cwd guards,
`TempCwd` (line 1535) and `CwdGuard` (line 2222). Every `CwdGuard` test is `#[serial]`, but the
single `TempCwd` user, `glob_files_results_are_relative_to_cwd`, is **not** — so it runs outside the
mutex and `set_current_dir`s the process underneath the serialized group.

The important consequence: these three pass at HEAD. That is **ordering luck**, not a fix. Adding or
removing tests shifts the interleaving, and at HEAD the narrow interleaving happens to be benign.
Nothing about the race was fixed, and the next test added anywhere in the crate can re-arm it.

**F4 — multi-file is still gated.** W1–W8 above are a plan, not shipped work. Correctly so: the gate
is still unmeasured.

### Proposal

**P1 — name the malformation instead of reporting an absence.** F1 is not tool-fixable (the payload
is not there to accept), but it is *diagnosable*, and that is the same class as the array correction
that already exists. In `EditArgs::from_wire`, before the fallthrough error: if `edits`/`blocks` is
present but is neither a string nor an array, say so —

> `` `blocks` is an object; it must be a string of `@@@OLD`/`@@@NEW`/`@@@END` blocks ``

This refuses exactly as now, so it widens acceptance by nothing, and it turns the one error a client
author needs into an error that names the fault. It also covers the plausible neighbouring case: a
harness that JSON-*stringifies* the blob into a nested field, which is the same bug wearing a
different hat.

**P2 — make the advertised schema derive from the parser, and say so when they diverge.** Already
staged, uncommitted, in `src/tools.rs`: the advertised key is now `(BLOCKS_FIELD)` rather than a
hand-typed `"blocks"`, so the schema and `from_wire` cannot name two different fields. Alongside it,
`every_advertised_property_is_one_the_parser_reads` asserts both directions — every advertised
property is read by `from_wire`, every property `from_wire` reads is advertised, and the description
names the field it advertises and not a retired one. That closes F2 from inside; it cannot detect a
*served* stale copy, which is a client-side concern and should be, since only the client knows what
it was handed.

### Status

**P1 and P2 are done.** `from_wire` now diagnoses a blob field that is neither a string nor an
array, naming the fault
(`` `blocks` is an object; it must be a string of `@@@OLD`/`@@@NEW`/`@@@END` blocks ``) instead of
reporting an absence the caller cannot act on — refusal is unchanged, so acceptance widens by
nothing. The advertised schema key is now `(BLOCKS_FIELD)`, and
`every_advertised_property_is_one_the_parser_reads` closes the schema⇄parser gap in both directions.
P1's test replays the payload this session actually produced, byte for byte, through
`ErasedTool::execute`, and asserts the file is untouched.

P3, P4 and F5 are open, in that order.

### Suggested order

1. ~~**P1, P2**~~ — done. Both were about one thing: a client and the tool disagreeing about a field
   name, with neither able to say so.
2. **P3** — before adding any further tests, or the next one added re-arms the race and the suite
   stops being usable as a regression signal.
3. **P4** — with the frame's owners.
4. **F5** — still behind the telemetry gate, unchanged.
