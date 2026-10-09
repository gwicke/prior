# Edit failure diagnostics: context instead of diff

*Status: implemented. See* § *Deviations from the plan* *for the two places the
code did not match the plan below.*

## Motivation

`edit` reports a failed match by returning a **Myers diff** of the model's
`oldText` against a padded window of the file. That was chosen to show "what the
model got wrong". It does not: for most inputs the diff body is the *file
region*, wearing `-`/`+`/space prefixes, and the part that matters is usually
cut off before it is rendered.

### What the diff actually renders

`crates/blockedit/src/diagnose.rs`:

1. `find_anchor` — longest common **substring** between `oldText` and the file.
2. A window of `oldText.len() + WINDOW_PAD (256)` bytes **on each side** of the
   anchor (`:178`).
3. `TextDiff::from_lines(query, window)`, then **every** change rendered —
   including `ChangeTag::Equal`, as a bare space (`:238-243`) — capped at
   `MAX_DIFF_LINES = 30` from the **front** of the change stream (`:230-236`).

So the cap is spent first on padding inserts the model never sent. Measured on
four shapes (row counts by tag):

- **case**: 4-line oldText, normal file
  - **`oldText` lines**: 4
  - **` ` (unchanged)**: 3
  - **`+` (padding)**: 21
  - **`-`**: 1
  - **truncated**: no
- **case**: 121-line oldText, typo on last line
  - **`oldText` lines**: 121
  - **` ` (unchanged)**: 30
  - **`+` (padding)**: 0
  - **`-`**: 0
  - **truncated**: **yes**
- **case**: 2-line oldText, 4-byte file lines
  - **`oldText` lines**: 2
  - **` ` (unchanged)**: **0**
  - **`+` (padding)**: 29
  - **`-`**: 1
  - **truncated**: **yes**
- **case**: 2-line oldText, normal file lines
  - **`oldText` lines**: 2
  - **` ` (unchanged)**: 1
  - **`+` (padding)**: 18
  - **`-`**: 1
  - **truncated**: no

Row 3 is the sharpest case. Real output:

```text
oldText not found. Closest match: similarity 0.50, file lines 147-202 ...
-target HERE
+7
+a148
...
... (27 more diff lines truncated)
```

The model receives `-target HERE` — the line it already sent — while
`+target here`, the only row that would let it repair `oldText`, is below the
cut. It receives strictly nothing it did not already have.

The unchanged rows are the line-LCS of query vs. window, so they are bounded by
`oldText`'s line count — but they are never what fills the budget. The padding
inserts are.

### Two pre-existing bugs in the same code

- **The anchor line is double-counted** (`:226`):
  `window_line_offset + count_lines(&search_text[..anchor_start]) + 1`. Both
  terms are measured from byte 0, so the offset is added twice. Ground truth
  line 201 is reported as 347 in a 202-line file. It goes unnoticed when the
  match is near the top of the file (`win_start == 0`), which is why the
  existing tests miss it. The message tells the model to re-anchor, so an
  out-of-range line number is actively harmful.
- **The window can start mid-line.** `win_start` is aligned with
  `floor_char_boundary`, not a line boundary, so the first diff row can be a
  torn fragment — the `+7` above (byte 722 lands inside `a145`).

Both disappear under the design below, by construction rather than by patch.

### Why the anchor is not the problem

The line number is a **disambiguator**, not a validity check. A globally unique
content match is applied regardless of the anchor, so a completely wrong line
number never fails an unambiguous edit. The failure is always about content.

This is behind the scenes today, but the current wording leaks the opposite
impression: `resolve.rs:69-71` appends `(anchored near line {s})` to the
failure, framing the anchor as a suspect. Under this design the anchor is
carried only as an **identifier** (§ *Origin*), and never as a possible cause.

## Decisions

- Q1
  - **Question**: Render format
  - **Decision**: `{:4}|` — byte-identical to `read` (`src/tools.rs:159`)
- Q2
  - **Question**: Locating the region
  - **Decision**: Myers internally, to *locate*; its output is never shown
- Q3
  - **Question**: `oldText` in the message
  - **Decision**: Identifier by origin string; `oldText` only as a fallback
- Q4
  - **Question**: `similarity`
  - **Decision**: Dropped from both views; gate re-based on the anchor alone
- Q5
  - **Question**: Elision cap
  - **Decision**: 40 file-context lines, split first/last around the divergence
- Q6
  - **Question**: Path in the body
  - **Decision**: Not threaded — the origin string already carries it
- Q7
  - **Question**: Fallback elision
  - **Decision**: At most 20 lines, elided at the end
- Q8
  - **Question**: Candidate context for `non_unique`
  - **Decision**: **Deferred** — see *Follow-ups*
- Q9
  - **Question**: Vocabulary in the model-facing prose
  - **Decision**: `@@@OLD lines`, not `oldText`; the `ui_view` keeps `oldText`

### Q1 — format

`read` emits `format!("{:4}| {}\n", i + 1, l)`, i.e. `1| fn main() {`. The
failure body uses the same format, so the model can lift lines straight into a
`@@@OLD` block and reuse the numbers as a `path:LINE` anchor without re-parsing
anything.

**Inherited wart:** `read` builds lines with `str::lines()`, which strips a
trailing `\r`. On a CRLF file the rendered context shows lines that cannot be
copied into `@@@OLD` byte-exactly, so the retry fails for a second, invisible
reason. This change puts that wart directly on the retry path. It is accepted
here because consistency with `read` is worth more than a fix in one place only
— it should be fixed in `read` and inherited.

### Q2 — locating the region

Three options were considered:

- anchor lines ± 2 — simplest, but the anchor is only the longest common
  *substring*, so if the model changed line 1 of a 10-line `oldText` the anchor
  starts at line 2 and the region **omits the line that differs**;
- a window the height of `oldText`, centred on the anchor — a heuristic with
  the same worst case;
- **run Myers to find the divergent span, then render those lines ± 2.**

The third is chosen. It costs one diff that is already being run today, and it
is the only option that guarantees the rendered region *contains the difference*
— which is the entire goal.

Implementation: walk `iter_all_changes()`, take the min and max `old_index()`
among non-`Equal` changes, and map those window-relative lines back to file
lines. If the diff yields no non-`Equal` change (defensive; cannot happen on a
true miss), fall back to the anchor's own lines.

### Q4 — the gate and the ratio

`similarity` is `max(anchor_len/query_len, equal_lines/total_lines)` computed
over the padded window, which is why it reports `0.99` for a region it renders
as 30 identical lines. The number is dropped from both views.

The *gate* stays — returning nothing when there is genuinely no related region
is right, and `not_found_no_match` is a good outcome. It is re-based on the
query-sized window (§ *Deviations*), with no padding in the denominator.

## Deviations from the plan

Two things the plan got wrong, found while implementing. Both are recorded here
because the reasoning is the useful part.

### 1. The gate keeps both signals; the *padding* was the defect

The plan said to re-base the gate on `anchor_len / query.len()` alone. That
silently drops **indentation drift** — the model wrote spaces, the file has tabs
— at 13/28 = 0.46 against a 0.5 threshold. The existing test for it calls that
"the dominant real-world failure mode", and it passed before the change. Losing
it would have been a quiet capability regression traded for tidiness.

The plan also misdiagnosed the cause. The inflation came from the **padding**,
not from combining two signals: `WINDOW_PAD` added 256 bytes of file content on
each side that matched nothing in the query, inflating `equal_lines`. So the fix
is to size the window to the query — `anchor ± oldText.len()`, which still
covers the whole query because the anchor is itself most of it — and keep
`max(anchor_ratio, line_ratio)`. Both signals earn their place: the anchor ratio
catches the one-bad-character-in-a-long-block case where no line is equal, the
line ratio catches drift where the shared run is short but most lines align.

`WINDOW_PAD` is gone entirely; there is now one window, used for both the gate
and the span.

### 2. `render_numbered` needed `i + 1`, not `first + i`

`text.lines().enumerate().skip(first - 1)` leaves `i` already at the absolute
index, so `first + i` double-counted and every rendered line number was one too
high — the header said `lines 2-6` while the body printed `3` through `7`.
Caught by the test that checks rendered numbers against the file, which is the
reason that test exists. `i + 1` is now literally `read`'s expression, so the
two renders cannot drift.

### 3. The rewrite reintroduced a panic the diff had already fixed

The old diff called `floor_char_boundary`/`ceil_char_boundary` on the window
edges. Making the window *line*-aligned looked like it made those guards
redundant, and they were dropped — but they were guarding something else.

`find_anchor` matches over **bytes**. UTF-8 is self-synchronizing, so two
different characters can share a byte or two and a byte-wise longest common
substring can start or end *inside* one. `line_start_at`'s `text[..at]` then
panicked with "byte index is not a char boundary".

This was not theoretical. A fuzz over random Unicode reproduced it in
**48,587 of 400,000 inputs** — roughly one in eight — and it was found by
asking, not by testing: the existing multibyte tests passed throughout,
because they use single-width files whose anchors happen to land on
boundaries. 1.2M inputs now run clean, and the witness plus a deterministic
sweep are pinned in `diagnose::byte_boundary_tests`. Both fail if the guards
are removed again, which was verified by removing them.

The lesson is the one the diff's own comments recorded: line alignment and
character alignment are different properties, and making one safe does not make
the other safe.

## Design

### Vocabulary

`oldText` is the **fallback** wire shape; blocks are the primary mechanism. So
the model-facing prose names `@@@OLD lines` throughout — in the not-found
header and in the prose of `non_unique`, `anchor_ambiguous`,
`anchor_out_of_tolerance`, `all_candidates_claimed` and `overlap_conflict`. A
diagnostic that taught the model `oldText` would be teaching the exception.

The split is **asymmetric**, and deliberately so:

- `model_view`
  - **speaks**: `@@@OLD lines` only — never `oldText`
  - **why**: blocks are the default; naming the exception teaches the wrong
    thing
- `ui_view`
  - **speaks**: may name `oldText`; must never contain `@@@OLD`
  - **why**: a person recognises the field, not the block syntax, and the panel
    must not teach the model-facing wording

Both directions are pinned in `diagnose::vocabulary_tests`. Note that a
`ui_view` is not required to mention `oldText` at all: `overlap_conflict` names
the two edits and their line ranges instead, which is the better summary when
the failure is about two edits colliding rather than about the text.

### The rendered region

```text
oldText not found. The closest region is file lines 138-145; the file reads:
  138| fn main() {
  139|     let x = 1;
  140|     let gamma = 3;
  141|     println!("hello");
  142| }
  143|
  144| fn other() {
  145| }
Copy the lines above into @@@OLD verbatim, or anchor the block with path:140.
```

- `CONTEXT_PAD = 2` lines each side of the divergent span (replaces
  `WINDOW_PAD`).
- `MAX_CONTEXT_LINES = 40`, split first-20/last-20 around the span, with
  `… (N lines elided) …` between — the phrasing `wire.rs:122`
  (`elision_notice`) already uses, so the model sees one elision idiom.
- **Elision is non-destructive here**: every emitted line keeps its own true
  line number, so a truncated region is still directly usable. That is the
  main advantage of the read format over a diff.

`ui_view` becomes:

```text
Closest match at file lines 138-145.
```

— shorter than today, and no longer carrying the misleading ratio. It keeps no
file content, which is the standing contract (`diagnose.rs:702`).

### Origin — the identifier

The model needs to know **which** of its blocks failed: one of ten, possibly
across several files. The tool strongly encourages `path:LINE` in every block
header, so that string is a literal the model can search its own message for.

The message leads with the origin **verbatim, exactly as the model wrote it**:

```text
--- edit #2 ---
Your block `src/foo.rs:140` failed: oldText not found. …
```

It is the block's header line unchanged. This means it must be **captured raw,
not reconstructed**: `split_line_anchor` (`blob.rs:209`) peels `:LINE` and
`:LINE:COL` off into `start_line`, so rebuilding `header:start_line` would
silently drop the column from `a/b.rs:100:5`. `ParsedEdit` therefore carries the
raw header line and `to_ops` copies it through unchanged.

For the single-edit form there is no header line, so the origin is the `path`
argument as spelled (with any `:LINE` suffix intact), and `:startLine` appended
when the line was given as a separate field. This one case is a reconstruction
rather than a verbatim capture; it is the only shape where that is true.

The multi-file path needs nothing: the result is a bare array whose entries are
`{path, result}` (`diagnose.rs:617`), so the path is already adjacent to the
failure.

### `oldText` — the fallback

|block supplied `:LINE`?|file's batch|identifier             |`oldText` echoed?|
|-----------------------|------------|-----------------------|-----------------|
|yes                    |any         |origin string (contains the line)|no     |
|no                     |`total == 1`|`edit #1`, unambiguous |no               |
|no                     |`total > 1` |origin string only     |**yes**          |

The condition is `total > 1` — hunks **in the file** — not `failed > 1`. A
ten-block file with one failure still needs the fallback, because `edit #4`
forces the model to count to identify it among its ten.

The decision belongs in `BatchEditFailure::build`, which already holds `total`
and the complete `failures` list; the per-failure constructors cannot make it.
`BatchItem` therefore gains `origin: Option<String>` and `old_text: String`
(both already in memory — no IO), and the constructors stop embedding either.

Echoed `oldText` is capped at **20 lines, elided at the end** (Q7):

```text
The text you sent for this block was:
```

fn helper() {
…
… (140 lines elided) …

```text
```

It is fenced so it cannot be confused with the `NNNN|` numbered file lines
above. This is a dedicated cap, not `wire::elide_string`: that helper is
marker-aware (`@@@OLD`/`@@@NEW`/`@@@END`), which is right for a blob being
echoed back and wrong for raw `oldText`, which never contains markers. Only the
notice phrasing is shared.

All six constructors follow the same policy — `not_found`,
`not_found_no_match`, `non_unique`, `anchor_ambiguous`,
`anchor_out_of_tolerance`, `all_candidates_claimed`, `overlap_conflict` — each
dropping its unconditional `Original oldText: '{}'` clause and gaining the
origin line.

## Changes

### Types

```rust
// blob.rs
pub struct ParsedEdit {
    // …existing…
    /// The block's header line **verbatim**, before `split_line_anchor` peeled
    /// `:LINE`/`:LINE:COL` off. `None` when the block carried no header.
    pub raw_header: Option<String>,
}

// types.rs
pub struct EditOp {
    pub old_text: String,
    pub new_text: String,
    pub start_line: Option<EditStartLine>,
    /// The block's origin string exactly as the model spelled it — a verbatim
    /// header line in the blob form, a reconstruction of the `path` argument
    /// in the single-edit form. Carried only to identify a failure; never
    /// persisted and never resolved against.
    pub origin: Option<String>,
}

// diagnose.rs
pub struct BatchItem {
    pub edit_index: usize,
    pub failure: EditFailure,
    pub origin: Option<String>,   // new
    pub old_text: String,         // new; lets `build` decide the fallback
}
```

### `diagnose()`

```rust
const MIN_ANCHOR_RATIO: f64 = 0.5;   // unchanged threshold
const CONTEXT_PAD: usize = 2;        // replaces WINDOW_PAD
const MAX_CONTEXT_LINES: usize = 40; // replaces MAX_DIFF_LINES

pub fn diagnose(query: &str, file_text: &str) -> Option<EditFailure> {
    if query.is_empty() || file_text.is_empty() { return None; }
    let (anchor_start, anchor_len) = find_anchor(file_text, query, MIN_ANCHOR_LEN)?;

    // One window, sized to the query — see Deviations 1.
    let win_start = line_start_at(file_text, anchor_start.saturating_sub(query.len()));
    let win_end   = line_end_at(file_text, (anchor_start + anchor_len + query.len()).min(file_text.len()));
    let window = &file_text[win_start..win_end];
    let offset = count_lines(&file_text[..win_start]) + 1;

    let changes: Vec<_> = TextDiff::from_lines(query, window).iter_all_changes().collect();
    if anchor_ratio.max(line_ratio).is_below_min() { return None; }

    let (lo, hi) = changed_line_range(&changes);          // window-relative
    let first = (offset + lo).saturating_sub(CONTEXT_PAD).max(1);
    let last  = ((offset + hi + 1) + CONTEXT_PAD).min(total_lines(file_text));

    let body = elide_middle(&render_numbered(file_text, first, last), MAX_CONTEXT_LINES);
    Some(EditFailure { ui_view: /* lines only */, model_view: /* header + body */ })
}

fn line_start_at(text: &str, at: usize) -> usize;  // start of the line containing `at`
fn line_end_at(text: &str, at: usize) -> usize;    // first newline at/after `at`, or len
fn render_numbered(text: &str, first: usize, last: usize) -> String;
fn elide_middle(body: &str, cap: usize) -> String;
```

`render_numbered` must select whole lines only — which is what makes the
mid-line-window bug impossible.

### `resolve.rs`

`not_found_failure` (`:67`) loses its `anchor_info` suffix, per *Why the anchor
is not the problem*. Its `not_found_no_match` fallback text is realigned with
the new wording.

### Files

- **File**: `crates/blockedit/src/diagnose.rs`
  - **Change**: `diagnose` rewrite; the two constants; six constructors drop the
    `oldText` clause; `BatchItem` fields; `BatchEditFailure::build` gains the
    origin header and the `total > 1` fallback; module doc
- **File**: `crates/blockedit/src/resolve.rs`
  - **Change**: `not_found_failure` — drop `anchor_info`, realign fallback text
- **File**: `crates/blockedit/src/blob.rs`
  - **Change**: `ParsedEdit.raw_header`
- **File**: `crates/blockedit/src/wire.rs`
  - **Change**: `to_ops` passes `raw_header` → `EditOp.origin`; the single-edit
    form reconstructs origin from the `path` argument; `elision_notice` made
    `pub(crate)` for shared phrasing
- **File**: `crates/blockedit/src/types.rs`
  - **Change**: `EditOp.origin`
- **File**: `src/tools.rs`
  - **Change**: two tests asserting `"Closest match"` in both views (`:2777`,
    `:2980`)
- **File**: `doc/batch_editing_v2.md`
  - **Change**: *Tool result: outcome, not echo* — the diff premise is inverted

**No signature change** to `apply_to_text`, and `similar` stays a dependency
(`udiff.rs`, `bsed`, and Q2's internal diff all still use it).

## Tests

- Render body is byte-identical to `read`'s format for the same lines.
- `model_view` contains no `-`, `+`, `@@`, or `@@@` marker.
- Rendered line numbers match ground truth — pins the double-count bug.
- **Named regression:** a typo on the last line of a 121-line `oldText` renders
  the changed line. This is the case the diff gets wrong today.
- A typo on the *first* line of a 10-line `oldText` renders the changed line —
  pins Q2's reason for existing.
- Elision: > 40 lines keeps true numbers and emits `… (N lines elided) …`.
- `origin` is verbatim: `a/b.rs:100:5` keeps its column; the single-edit form
  reconstructs from the `path` argument.
- `oldText` appears **only** under the `total > 1` + no-LINE fallback, capped at
  20 lines, elided at the end.
- All six constructors drop `Original oldText`.
- `ui_view` still carries no file content (`diagnose.rs:702` keeps its value).
- The three multibyte tests (`:984-1023`) rewritten around whole-line selection.
- A match whose origin anchor is **wrong** still renders the diagnosed region
  and does not blame the anchor.

## Follow-ups (not in this change)

- **Q8 — candidate context.** `non_unique` and `anchor_ambiguous` could render
  the line-numbered context *at each* candidate occurrence, since
  `occurrences_of` already returns every offset. Deferred: it multiplies output
  by candidate count, and uniqueness failures have largely disappeared now that
  the batch format is pushed to supply line numbers.
- **Mandatory line numbers.** Making `:LINE` required in the block header would
  make the identifier unconditional and remove the fallback entirely. Worth
  measuring before proposing — the anchor is only a disambiguator, so a missing
  line currently costs uniqueness failures, not wrong writes.
- **`read`'s `\r` stripping**, which this design inherits (§ Q1).
