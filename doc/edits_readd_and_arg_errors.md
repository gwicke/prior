# Re-accepting `edits: [...]`, and the argument errors around it

*Status: implemented. Two coupled changes: the `edit` tool's argument-error
path, and re-acceptance of pi's canonical `edits` array — accepted, never
advertised.*

Decisions taken during implementation, beyond the plan below: the multi-file pi
form is refused by the *same* rule that governs an entry path disagreeing with
the call (one rule, "the call names exactly one file"), and a stringified array
is recovered rather than refused.

## Findings first

Two things were measured before any of this was designed, because the report
that prompted it ("a malformed `edit` call answers `the tool failed`") did not
match the code at HEAD.

### 1. The `edits` array already answers helpfully

Dispatching `{"path": "x.rs", "edits": [{"oldText": "a", "newText": "b"}]}`
through the erased-tool boundary (`src/tools.rs:3039`) produces, as
`ToolErrorKind::InvalidArgs`:

```
failed to parse tool arguments: `edits` is a list; it is not a form this tool
accepts. Use `blocks` (a block string) or the single-edit fields `oldText` + `newText`

Expected:
some/path:1234
@@@OLD
old text
@@@NEW
new text
@@@END

Actual:
{"edits":[{"newText":"b","oldText":"a"}],"path":"x.rs"}
```

Every shape that reaches `EditArgs::from_wire` gets that envelope — the array,
`{}`, `{"path": …}` alone, a non-object, a `blocks` field holding an object, a
half-written single edit. The `Expected:`/`Actual:` machinery is not missing;
it is already the answer for the whole arg layer.

### 2. `the tool failed` is real, and it is the *call* layer

| input | kind | model sees |
|---|---|---|
| `{"path":"missing.rs","oldText":"a","newText":"b"}` | `Other` | **`the tool failed`** |
| `{"blocks":"missing.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END"}` | `Other` | **`the tool failed`** |

`edit` is registered raw (`.tool(EditFile)`, `src/agent.rs:2906`), so a
call-time `ToolError` reaches rig's **default** `map_error` →
`ToolExecutionError::from_error` → `redact_model_feedback()` → the `Other`
default text. Only `Guard` overrides `map_error` (`src/agent.rs:2994`), and
`edit` is not one of the guarded tools. This is already written down as an open
item: `doc/path_leak_risks.md:59-64` names `read`, `write`, `edit`, `glob`,
`grep`, `list` and `sed` as registered without `Guard`.

**The two findings are why this is one change.** Accepting the `edits` array
moves its failures from the arg layer (helpful) to the call layer
(`the tool failed`). Shipping the array without fixing the redaction would trade
a good error for a bad one.

## Part 1 — malformed requests answer with `Expected:` / `Actual:`

### 1a. Stop redacting `edit`'s own diagnostics

- `src/tools.rs` — extract `Guard::map_error`'s classification into one
  `pub(crate) fn map_tool_error(ToolError) -> ToolExecutionError`, and add
  `fn map_error` to `impl Tool for EditFile` that calls it. One table, so the
  two tools cannot classify the same error differently.
- `src/tools.rs::edit_file` — name the file in its IO errors:
  `cannot read {path}: {e}`, `cannot write {path}: {e}`. The path is the
  **model's own** (`path` argument), so this is the rule at
  `doc/path_leak_risks.md:22-29`; `io::Error`'s `Display` carries no path, so
  nothing host-shaped can ride along.
- Add the guard test that document says is missing
  (`doc/path_leak_risks.md:69-71`): under a *remapping* policy, trigger each
  constructable `edit` failure and assert the model-visible text contains no
  host-path token. This converts "happens to be clean" into "fails CI if it
  stops" for `edit` — steps 2 and 3 of that document's *when the guard comes
  off*, with step 1 (a separate operator channel) still open.

### 1b. Name the fields that arrived, in the catch-all only

The final `Err` in `from_wire` becomes a dedicated builder, so the terse paths
stay terse. Two extra lines, and only when there is something to say:

```
no recognizable edit arguments (expected `oldText`+`newText`, or `blocks`)
unrecognized: `file_path`, `old_string`, `new_string`
this tool spells them `path`, `oldText`, `newText`
```

The third line is an alias table — `file_path`, `old_string`, `new_string`,
`old_text`, `new_text`, `content`, `start_line`, `replace_all` — consulted
**only** to name the correct spelling. Aliases are never accepted: a field this
tool has not published is a field it does not read.

`Expected:` follows suit: a call carrying a translatable alias pair gets
`SINGLE_EDIT_ARGS_EXAMPLE`, because the correction is a rename inside the shape
it already used, and an example in another shape is an answer to a question
nobody asked.

### 1c. A stringified array is recovered, not misparsed

`blob_text` (`crates/blockedit/src/blob.rs:39-44`) accepts **any** string under
`edits`, so `"edits": "[{\"oldText\":…}]"` — the exact hazard the blob form was
introduced to kill (`doc/batch_editing_v2.md:64-89`) — currently reaches
`parse_blocks` and dies as `Invalid block: …`.

An `edits` string whose first non-space character is `[` (and which carries no
`@@@OLD`, so a real blob can never be mistaken for one) is parsed back into an
array and run through the array branch. Recovery is safe because the entries
are validated and applied exactly as a real array's are; the mangling costs a
`serde_json` parse, nothing else. A string that does not parse as JSON gets a
named correction instead of the block parser's opinion.

### Out of scope: truncated JSON

`"edits": "[{\"oldText\":"` never reaches this code — the provider parses the
tool-call arguments into a `Value` first, and rig's `parse_tool_args` fails
before `EditArgs` is consulted. If a case ever does reach it,
`ToolCallAction::Skip` in `on_tool_call` (`src/agent.rs:2735`) would deliver our
message verbatim. Cheap to add, with nothing to test against today.

## Part 2 — the `edits` array, accepted and unadvertised

### 2a. The wire contract

Accepted, in `from_wire`, after the blob branches and before the single-edit
fields:

| | rule |
|---|---|
| shape | `{edits: [{oldText, newText, startLine?, path?}], path?}` — pi's canonical form, single-file batching |
| file | **every** path the call names — the top-level `path` (after `:LINE` peeling) and each entry's `path` — must be **one** path |
| none named | `missing required field 'path'` |
| more than one | refused, naming `blocks` (this is the multi-file pi form; pi itself cannot express it — `doc/batch_editing_v2.md:649` gives one `path` per call and entries without one) |
| anchor, per entry | entry `path:LINE` → entry `startLine` → call `path:LINE` → call `startLine` → none |
| entry with both its own `path:LINE` and `startLine` | must agree, else the single-edit form's existing disagreement error, verbatim |
| `[]` | error — never a vacuous success |
| entry not an object / missing either text field | named error naming `edits[i]` |
| precedence | the array wins over top-level `oldText`/`newText`, and a **malformed** array must not fall through to a healthy single edit — the same rule as `blocks` over a legacy `edits` string (`wire.rs:582-601`) |
| `origin` | the entry's `path:LINE` as spelled, else the call path (+ `:LINE`), so one failed entry of ten is identifiable |

Redundant *agreeing* entry paths are accepted, and so is an array with no
top-level `path` when its entries agree on one file. One rule — "the call names
exactly one file" — covers the multi-file refusal, the disagreeing-entry
refusal, and both leniencies, and it is the policy the blob form already uses
(`path` vs a block header disagreeing is a hard error, `wire.rs:317-319`).
Nothing is ever written to a file the model did not name.

### 2b. `parameters()` does not change

`src/tools.rs:235-264` keeps advertising exactly `blocks` (a string), `path`,
`oldText`, `newText`, `startLine`. No advertised property is an array; the
schema tests already assert both of those and keep asserting them.

The parser/schema contract test (`src/tools.rs:3715`) is where the shape of the
contract changes: `from_wire`'s field list becomes **advertised** plus
**read-but-unadvertised**, and the direction that matters — *advertised ⇒
parsed* — stays, because that is the one whose failure reaches the model as a
silent dead end.

### 2c. Replay keeps normalizing arrays to blocks

`blob_edit_tool_calls` (`src/agent.rs:1744`) rewrites a canonical array back to
a block string before the model sees the history. Keep it: with two accepted
shapes, a uniformly-block history is still the one that keeps the model
emitting the shape weak harnesses handle. Its doc comment's reason ("so it is
not nudged back toward the array, the form weak harnesses mangle") is now half
defunct and gets corrected.

## Tests

`crates/blockedit/src/wire.rs` — new: array batched onto one file; per-entry
anchors and their precedence; redundant agreeing entry `path`; array with no
top-level `path`; `[]`; non-object entry; missing `oldText`/`newText`; entries
naming two files; entry `path` disagreeing with the call; array beside
top-level `oldText`/`newText`; stringified array recovered; unparseable
stringified array named; catch-all naming fields and the alias line; the alias
line choosing the JSON example.

Rewritten (each currently *specifies* the removal): `the_edits_array_is_rejected_with_a_named_correction`,
`the_two_field_names_are_not_interchangeable`, `every_wire_shape_this_tool_has_ever_accepted_is_accounted_for`,
`an_empty_edits_array_is_rejected_too`, `a_blob_field_holding_an_object_is_diagnosed_rather_than_ignored`.

`src/tools.rs` — `an_edits_array_is_rejected_and_writes_nothing` becomes the
end-to-end inverse: an array call applies N edits to one file. New: a
missing-file array call asserts the model text names the path and is **not**
`the tool failed`; the schema still advertises no array; the leak guard test
from 1a.

## Docs

- `doc/batch_editing_v2.md:64-89` — the section that removed the array gets an
  amendment: the stringify hazard is unchanged, the *named correction* is no
  longer the answer, and what replaced it.
- `doc/path_leak_risks.md:57-72` — the redaction gap closes **for `edit`
  only**; the other six tools stay redacted and the operator channel stays open.
- `src/agent.rs:1741` — the replay comment.
