# `bsed` — a preview-only GNU sed that emits `edit`-tool blocks

*Status: implemented. Decisions in §12, deferred work in §13. §14 records where
the implementation corrected the design, which is the part worth reading if you
are comparing this document to the code.*

## 1. The problem this solves

The agent prompt currently tells the model:

> Be precise: When editing files, use `edit`. … **Avoid editing with sed or python
> scripts - you are likely to make mistakes. If you MUST use them, always save all
> outstanding work with a snapshot commit first, and double check the results right
> after.** — `src/agent.rs:1923`

That is a workaround, not a fix. The failure mode is real and observed: `sed -i`
with a line range computed against a *stale* read of the file, on a working tree
with uncommitted work, silently eats the uncommitted work. Real expressions seen
in logs:

```
1498,1499s/if count > 0 {/if count > 0 {/
177s/edits: EditInput,/edits: serde_json::Value,/
s/const MAG_VAR_THRESH_LOW: f32 = 2.0;/const MAG_VAR_THRESH_LOW: f32 = 5.0;/
870,874d
326d;327d
279r /tmp/model_filter.rs
/^            $/{ N; /^\n$/d; }
```

The idea: **sed becomes advisory.** The model formulates GNU sed expressions, gets
back a reviewable, line-anchored `@@@OLD`/`@@@NEW`/`@@@END` preview, and applies
(all or part of) it through the existing `edit` tool — already anchor-aware,
uniqueness-checked, overlap-checked, all-or-nothing. Previewing never writes.
If the preview is wrong, nothing was lost.

This is not "sed that is safer at writing". It is a **sed-to-block-edit
compiler**: a preview, in the one format the agent already knows how to apply.

## 2. Architecture (agreed shape)

```
 picocode (agent)                          crates/bsed
 ┌────────────────────────┐                ┌──────────────────────────────┐
 │ tool `sed`             │                │ lib: script → engine → diff  │
 │  path / expressions    │   argv         │      → hunks → blocks        │
 │  context               │ ─────────────► │ bin `bsed`                   │
 │ get_path() policy      │   read-only    │   preview | apply | (later)   │
 │ run_sandboxed_ro()     │ ◄───────────── │        replace -r            │
 │ (bwrap, no shell)      │   stdout+exit  └──────────────────────────────┘
 └────────────────────────┘
        │                                        ▲
        │  paste blocks                          │ deny-list (AST)
        ▼                                        │ enforced inside the
 ┌────────────────────────┐                       │ binary, whoever calls it
 │ tool `edit` (existing) │                       └──────────────┐
 └────────────────────────┘                                      │
        ▲                                                         │
        └──── crates/blockedit  (parse / match / apply, shared) ──┘
             extracted from src/edit_blocks.rs + src/edit_match.rs + edit_file
```

Three independent layers, each sufficient on its own to prevent "sed ate my
uncommitted work":

1. **AST deny-list** inside `bsed` — `w`/`W`/`r`/`R`/`e`/`q` are rejected before
   the engine runs, by `match`ing the closed `Command` enum with **no wildcard
   arm** (§5.1).
2. **Process boundary** — a separate binary on `$PATH`. Model-supplied text is
   only ever file *content*/argv *data*; it is never argv structure, and it is
   never interpreted by a shell. The engine's `q`/`Q` handler calls
   `std::process::exit` (§5.2) — a subprocess is the only place that is harmless.
3. **Read-only bwrap namespace** — the same machinery `rg`/grep uses
   (`run_sandboxed_ro`, `src/tools.rs:1256`): cwd ro-bound, `/tmp` a scratch
   tmpfs, `--unshare-net`, `--cap-drop ALL`, `--die-with-parent`, memory cap,
   120 s default timeout. The agent only ever previews there.

`bsed` is genuinely useful standalone (`bsed apply` from a shell, later
`replace -r` recursive search/replace), which is what makes extracting the block
edit core worth doing.

## 3. Tool contract

Tool name `sed`, registered **only when `bsed` is found on `$PATH`** (§9).

```jsonc
{
  "path":        "string",           // required; same semantics as `edit` (accepts file:LINE)
  "expressions": "string",           // required; ONE GNU sed expression per line
  "context":     "integer|string"    // optional; see §4.1
}
```

* `expressions` — one expression per line, nothing else. No multi-line
  continuations (`a\`+newline is rejected with a pointed message; it never appears
  in observed usage), and deliberately **no JSON array form** — weak harnesses
  stringify array parameters wholesale, and the string form is what survives.
  Lines are joined with `\n` into one script, which is byte-for-byte what
  `sed -e A -e B` does (GNU concatenates `-e` scripts with newlines), so
  `326d;327d` on one line and two separate lines are equivalent. Blank lines are
  ignored. NUL bytes are rejected up front (they cannot cross an argv boundary).
* `context` — integer or numeric string, same shape as `ReadLimit`
  (`src/config.rs:100`), because harnesses stringify numbers. Default **0**;
  clamped to `[0, 20]`. Effective padding is `leading = context`,
  `trailing = 0`. A `bsed` caller asks for trailing context with `-t N`; the
  agent tool does not, and the default is 0 on both sides. See §4.1.
* Returns a string: a one-line banner, then the block blob (§7). Never writes.
  Not wrapped in `guard()` — it is a read-only preview.

## 4. Producing the preview

### 4.1 What `context` is, and why both defaults are 0

**`context` is not "print N lines around the match"** (that is `grep -C`). It is
the number of **unchanged** lines of the original file carried *into each emitted
block*, inside `OLD` and `NEW`. Only regions that actually changed are ever
emitted; `context` pads each one.

**Both defaults are 0.** `leading = context`, `trailing = 0`; a caller wanting a
tail asks for it with `-t N`. An unadorned call therefore gets only the changed
lines, plus whatever the balance-growth pass (§4.6) reaches.

This was not always so, and the argument for the 2-line tail it replaced is worth
keeping, because it is the reason `context` exists at all. The tail turned a
1-line `OLD` into a 3-line one, which lifted `anchor_tolerance` from 0 to 1 (see
below) — insurance against a file that shifted between preview and apply. It also
covered the smallest multi-line call shape (`compute(` / `a,` / `b,`), signalling
that a multi-line statement was in play.

It was dropped because every block already carries a `path:LINE` anchor computed
from the file `bsed` just read, so an unshifted file needs no tolerance at all;
the slack only ever mattered for a *stale* anchor, which is a workflow problem
(apply the whole blob in one `edit` call) rather than a preview one. Meanwhile the
tail cost a third of the 50 KB budget (§5.5) on every single-line change, and the
growth pass already reaches the closing bracket when one is nearby. Dropping it
does make a 1-line block have exactly zero slack, so a batch applied after an
unrelated edit can still be refused — correctly, with "anchor out of tolerance" —
and re-previewing is the answer. That trade is deliberate: fewer blocks fit in the
budget, and the same information is available on request.

**Line anchoring is what makes small blocks viable — that is the entire reason
the blocks are anchored.** Reading `edit`'s anchored pass (Pass 1,
`src/tools.rs:666-738`) against a 40-occurrence duplicate such as `    run();`:

* Candidates are ranked by `|line − anchor|`; the nearest wins.
* The window is `anchor_tolerance = min(4, old_lines/3)`, so a 1-line `OLD` gets
  tolerance **0** — and tolerance 0 still accepts an *exact* anchor, which is
  exactly what `bsed` emits (derived from the file it just read).
* Beyond the window the skew rule rescues a stale anchor: accept only if
  `d1 ≤ 60` and the runner-up is `≥ 3×` farther (`edit_match::anchor_accepts`,
  `src/edit_match.rs:53`).
* Ties are rejected — two copies at equal distance is an error, not a coin flip.
* If `old_text` occurs exactly once, the anchor is not consulted at all
  (`src/tools.rs:676`).

So a mass rename applies correctly with **zero** context and no uniqueness
requirement: every block carries its own `path:LINE`, and the uniqueness rules
(`src/tools.rs:767`, `:781`) bind only *unanchored* blocks, a shape `bsed` never
emits. How *many* blocks a mass rename produces depends on spacing, and both
answers are good — see §4.4.

What `context` then buys is three things, none of which is "makes the edit apply":

1. **Stale-anchor tolerance.** Tolerance scales with `old_text` length (1 line → 0,
   3 lines → 1, 6 lines → 2, 12 lines → 4). A bare 1-line block has *zero* slack:
   if the file shifted between preview and apply — the model applied an earlier
   batch first, or something else touched the file — the 40-duplicate case fails
   the skew rule outright (`d2 = 1 < 3 × d1` for any `d1 ≥ 1`) and `edit` reports
   "anchor out of tolerance". Correct behaviour, but a wasted round-trip. With
   both defaults 0 this is the case to expect: a 1-line block has no slack at all.
   The rule that avoids it is the same either way — apply the whole blob in **one**
   `edit` call, and re-preview before any follow-up batch — and a caller who wants
   the insurance can ask for a tail with `-t 2`, which restores ±1.
2. **Legibility of the preview** — `run(); → walk();` alone does not show the
   enclosing `if count > 0 {`. Largely free via §4.6 (the TUI panel renders a
   context-rich diff; only the *model* payload is terse), so there is no reason to
   pay for it in tokens.
3. **Guarding the model against reasoning about a truncated construct** — the one
   correctness-adjacent case, and the motivation for the v2 growth heuristic in
   §4.6. The edit is byte-exact either way; the risk is the model *rewriting* `NEW`
   on the assumption that it is looking at a whole statement.

Cost when used: blocks grow, so the 50 KB preview budget (§5.5) is consumed
faster, and hunks within `2*context` merge into one larger block (harmless —
`similar` guarantees they cannot overlap). With both defaults 0 the only growth
is the balance pass, clamped so it can never reach the next block (§4.5, rule 8).

### 4.2 Pipeline (inside `bsed`)

```
file ──read──> original text
                   │
 expressions ──────┼──> sed-rs eval(script, text) ──> transformed text
                   │            (deny-list already passed)
                   └──> similar::TextDiff::from_lines(orig, new)
                             └── .grouped_ops(context) ──> hunks ──> blocks ──> blob
```

`sed-rs` only: single engine, no fallback to a system `sed` (agreed).

### 4.3 Options considered for "which lines are affected?"

| Option | Verdict |
| --- | --- |
| **A. Diff old vs new** | Works for *every* command (`s`, `d`, `a`, `i`, `c`, hold space, `{}`). One code path, no sed internals parsed, `similar` already in the tree. **Chosen.** |
| B. `sed -n '/re/='` probing | Only expresses address *selection*; useless for `a`/`i`/`c` and anything that mutates the pattern space; needs per-expression address rewriting; reports sed's line counter, not file line numbers (which is what anchors must be). |
| C. `sed --debug` | GNU-only, unbounded, version-fragile, logs pattern space not final state. |
| D. External `diff -u` + temp files | Extra binary, tmpfs writes, cleanup — for a result `similar` computes in-process. |
| E. Engine-side per-line hooks | Unavailable (sed-rs has none) and unnecessary once we diff. |

Option A also has a property that matters: **the preview is derived from the same
bytes `edit` will match against.** Line numbers in the file, not in sed.

### 4.4 Verified hunk mechanics (`similar` 2.7)

`TextDiff::from_lines(old,new).grouped_ops(ctx)` → groups of ops;
`first().old_range().start` … `last().old_range().end` gives the OLD span (0-based),
likewise for NEW. Measured:

| change | ctx=0 | ctx=1 | ctx=2 |
| --- | --- | --- | --- |
| `C`→`X` at line 3 and `J`→`Y` at line 10 | OLD=`["C"]` / OLD=`["J"]` | OLD=`["b","C","d"]` | OLD=`["a","b","C","d","e"]` |
| prepend 2 lines | `old_range (0,0)` — **empty OLD** | `(0,1)` | `(0,2)` |
| truncate last line | `old_range (2,3)`, `new_range (2,2)` — **empty NEW** | `(1,3)/(1,2)` | — |

Two consequences of how `similar` groups, both verified against it and both
better than this document originally claimed:

* **Adjacent changes merge into one block.** `grouped_ops` folds ops whose gap
  is `<= 2*ctx` into a single group, so a *contiguous* rewrite — 40 consecutive
  identical lines — is **one** block of 40 lines, not 40 blocks. That is the
  better outcome: a mass rename arrives as one reviewable hunk and is applied
  atomically. The 40-separate-blocks case needs the duplicates *scattered*
  (every third line), which is what the tests use.
* **We ask `similar` for no context at all** (`grouped_ops(0)`) and pad both
  directions ourselves. `grouped_ops(n)` pads symmetrically, which would apply
  the caller's `context` twice — once as the floor, once as `similar`'s — so an
  asymmetric contract cannot be routed through a symmetric knob.

Because both padding directions are ours now, every bound is clamped in one
place: the leading pad stops at the previous block's end, the trailing pad at the
next block's start, and growth at that same ceiling plus its own cap. That is
what guarantees blocks never overlap — the `edit` tool rejects overlapping
claims (`src/tools.rs:719`), and a rejected batch helps nobody.

### 4.5 Hunk → block rules

1. **Anchor** = `path:<first OLD line, 1-based>` — the same `path:LINE` idiom the
   blob parser already accepts (`split_line_anchor`, `src/edit_blocks.rs:123`).
   Anchors are exact (derived from the file just read), so `edit`'s anchored pass
   resolves them at distance 0.
2. **Empty `NEW` is fine** — `d` produces `@@@NEW` immediately followed by
   `@@@END`; only empty *OLD* is rejected by the parser.
3. **Empty `OLD` is not fine** — pure insertions yield `old_range (n,n)`. The
   trailing context (rule 8) normally supplies the missing line for free, since it
   extends the *end* of the hunk; only if `OLD` is *still* empty (insertion at EOF)
   do we borrow the *preceding* line. Verified: `2a\ let w = 0;` → `src/foo.rs:3`,
   `OLD=["    let y = 2;"]`, `NEW=["    let w = 0;","    let y = 2;"]`. An
   insertion into an *empty* file has no neighbour → "file is empty; use `write`".
4. **Whole-file rewrite** (e.g. `1,$d`) → one giant block; the §5.5 budget stops it.
5. **Marker-line hazard** — a line that is exactly `@@@OLD`/`@@@NEW`/`@@@END`
   inside `OLD`/`NEW` would be read as structure. Detect and return a specific
   error pointing at `edit`'s `oldText`/`newText` JSON form, which bypasses the
   blob parser.
6. **Trailing newline** must be preserved exactly (§6, gap 5), so a no-op
   expression on a file without a final newline reports "unchanged".
7. **Anchors never move.** The tail (rule 8) and any growth (§4.6) extend a
   hunk's end only, so a block's first `OLD` line — and its `path:LINE` header —
   is always the first line the diff reported.
8. **The trailing context is clamped against the next block.** With `context: 0`,
   `grouped_ops(0)` splits hunks maximally, so a naive 2-line tail can reach into
   the next hunk's first line and produce two blocks with overlapping `OLD`
   ranges — which `edit` rejects outright. Rule: extend the tail only up to (not
   including) the next block's first line. Adjacent mass edits therefore get
   1-line blocks even with the floor, which is correct: there is no unchanged room
   to show.
9. **Order of operations**, each step able to expose the next:
   `grouped_ops(context)` → tail floor → balance growth (§4.6) →
   borrow-if-still-empty-`OLD` → budget truncation (§5.5) → marker-hazard re-check
   (rule 5) → render. The marker check runs *last* because the tail and growth can
   pull a marker line into a block; truncation runs before rendering so it always
   lands on a whole block.

### 4.6 Two audiences: terse payload for the model, rich view for the human

`context = 0` is cheap for the model but thin for the person watching the TUI.
Those are separate surfaces, and only one of them is a token cost:

* **Model-facing payload** — the blob, exactly as specified, at `context = 0`.
  Reused verbatim as the `edit` tool's `edits` argument.
* **UI panel** — `display_tool_result` already renders a context-rich unified diff
  for `edit` results (`refined_diff_lines`, `src/output.rs:966`, 2 lines of
  context, `-`/`+`/space markers). Extend the same renderer to the `sed` result:
  the panel shows the blocks *with* surrounding context, so the human review that
  motivates `context` never consumes model tokens. This is the main reason
  `context = 0` is safe to default to.

**Balance-based growth — "do as I mean, not do as I say".** When a hunk's `OLD`
ends inside an unbalanced construct — an open `(`/`[`/`{`/`<`, or an unterminated
string — the block is still *applied* correctly (it is a byte-exact replacement of
exactly those lines), but the model cannot see what it is editing, and may "fix"
the `NEW` side against an imagined wider statement. That is the one case where
enlarging the block serves what the model meant, so it is worth doing.

**Complexity: ~300 lines, and the scanner is not the expensive part.** A balance
scanner (~150 lines: string states for `"` and `'`, escapes, `//` and `/* */`
comments, a depth stack over four bracket kinds — the work is *state*, not
counting, and it is a pure `&str -> bool`, trivially unit-tested), a growth walk
(~60: extend until balanced, clamp at the next hunk, cap the span — the clamp is
what makes it safe), wiring into rule 9's ordering (~40), tests (~120). The same
scanner also serves `bsed replace -r` later.

The safety argument for **trailing-only** growth: it appends only lines the diff
already reported as unchanged, and stops before the next hunk's first line. So a
grown block can never contain a line `sed` actually modified — that is precisely
the failure mode that would silently revert another edit. `NEW` needs no separate
handling: for unchanged lines `old[i] == new[i]`, so the same text goes into both.

Angle brackets need one guard or the heuristic misfires constantly: count `<`
only when it is not part of `->`/`<-` **and** a matching `>` appears within the
growth cap. Then `a < b` never triggers growth while `Vec<String>` does. Same care
for `'`: in Rust that is a lifetime, not a char literal, so track char-literal
state that requires a closing `'` within a few characters.

**Recommendation: ship growth in v1, not only the hint.** The residual risk is
cosmetic and bounded (a mis-scan can only add unchanged, capped lines), the
scanner is needed by `replace -r` anyway, and "a one-line replacement inside a
five-line call" is exactly the case this tool exists to serve. Keep the hint too,
for what growth cannot fix (unbalanced source, heredocs, macros):
`2 block(s) end mid-expression; re-run with context: 4 if you need more.`

## 5. Safety model

### 5.1 Layer 1 — AST deny-list (verified against the sed-rs source)

`command::parse` returns `Vec<SedCommand>`, `SedCommand { address, command }`,
`command: Command` — a **closed** enum. I read `sed-rs`'s `engine.rs` and located
every filesystem/exec touch point in it; all six are inside the cycle executor and
each maps 1:1 to an AST node:

| engine.rs site | operation | AST node | decision |
| --- | --- | --- | --- |
| `:575` `fs::read_to_string(path)` | read a file into the stream | `ReadFile` (`r`) | **allow** — see §5.4 |
| `:604` `fs::File::open(path)` | one line per cycle | `ReadLine` (`R`) | **allow** — see §5.4 |
| `:580` `OpenOptions::create(true).append(true)` | append to a file | `WriteFile` (`w`) | **reject** |
| `:592` same | append first line | `WriteFirstLine` (`W`) | **reject** |
| `:435` same, inside `Substitute` | `s///w FILE` | `SubstituteCmd.write_file` | **reject** |
| `:625` `Command::new("sh").arg("-c")` | shell execution | `Execute` (`e`, incl. the `e` flag) | **reject** |
| `:304`/`:310` `std::process::exit(code)` | `q N` / `Q N` with N≠0 | `Quit` / `QuitNoprint` | **reject** — see §5.4 |
| `:222`/`:245`/`:1014` `File::open` / `fs::copy` / `tempfile::NamedTempFile` | in-place editing, backups | *entry points* `Engine::run` / `run_in_place` | never called — `bsed` uses only `eval(script, text) -> String` |

Allowed (everything else, including the pure text transforms `s`, `y`, `d`, `D`,
`p`, `P`, `=`, `l`, `n`, `N`, `h H g G x`, `a`, `i`, `c`, `:label`, `b`, `t`,
`T`, `{ }`, `z`, `F`, `v`, and the file *readers* `r`/`R`): the deny-list is now
write, exec, and early-exit only. §5.4 argues each.

The walk is written as a `match` **without a wildcard arm**, so if `sed-rs` ever
adds a variant the crate fails to compile rather than silently permitting it.
Rejections name the offending expression and line: `expression 3: 'r FILE' reads a
file from disk, which the sed tool does not allow — use the 'edit' tool to splice
the file in manually.`

### 5.2 Layer 2 — process boundary (the question: "is the tall order avoidable?")

Short answer: **no, it is not a tall order — and we should still take it.**

*Provable side.* Everything above is enumerable, and the "no I/O in the hot path"
claim is checkable: `Engine::new` only compiles regexes (`engine.rs:172-180`), the
only I/O in the cycle executor is the six sites tabulated, and the two I/O entry
points (`run`, `run_in_place`) are never reachable from `eval`. So an in-process
design can be made structurally I/O-free with a compile-time-exhaustive deny-list.

*But there is a class the AST does not cover.* `q N` / `Q N` (N≠0) call
`std::process::exit(code)` — that is not an I/O side effect, it is a **process
exit**, and in-process it would kill the agent mid-turn. It is deny-listable, but
it is a good illustration that "I audited the enum" and "the library cannot hurt
the process" are different claims. A future release could add something similar
that is *not* a new enum variant (a panic in a pathological regex, an
`unwrap`, a stack overflow on a pathological script).

*Why the boundary anyway, now that it is cheap:*

* The blast radius of being wrong is precisely the thing we are building to
  protect: the user's uncommitted work. "We reasoned correctly about a
  third-party crate's internals" is a weaker guarantee than "even if we were
  wrong, that process could not write to your workspace".
* With `bsed` as a binary, the sandbox costs one `run_sandboxed_ro` call we were
  going to make anyway. There is no extra process, no extra dependency, no
  plumbing.
* Defence in depth has *different* failure modes per layer, which is the point:
  a deny-list hole degrades to "can write inside an ephemeral tmpfs, with no
  network, no caps, no host filesystem, and a 120 s timeout"; a sandbox
  misconfiguration degrades to "deny-list still holds".

So: keep all three layers. The deny-list is not redundant with the sandbox — it
is what makes a preview safe when a *human* runs it outside any sandbox, and
it produces a good error message instead of an opaque `sed: can't read` from
inside a namespace.

### 5.3 Layer 3 — read-only sandbox

A preview runs via `run_sandboxed_ro(argv, cfg, timeout)` (`src/tools.rs:1256`),
identical to the grep/`rg` path:

* argv only, never a shell; the model never supplies a subcommand, a flag name, or
  a path — picocode templates the whole argv and the expressions ride as one argv
  element (data, not structure).
* read-only namespace: cwd ro-bound, `/tmp` an ephemeral tmpfs, `--unshare-net`,
  `--cap-drop ALL`, `--die-with-parent`, `prlimit --as` memory cap, watchdog
  timeout with `<Esc>` cancellation (all already implemented).
* Consequence to document: the file must be visible inside the namespace. With
  the default config (cwd ro-bound) it is. With a custom `bindings` list that
  excludes it, `bsed` reports a clear error: *file not visible inside the sandbox
  — add it to `[tool_config.bash.sandbox]` `ro` bindings*. (`grep` has the same
  constraint today.)
* The *binary* has the same visibility problem as the file, and for a sharper
  reason: the namespace starts from a cleared environment, so there is no `$PATH`
  to resolve `bsed` by even if the host has one, and the default bindings cover
  only the workspace plus `/usr`, `/bin`, `/lib`, `/lib64` — not
  `~/.cargo/bin`, where `cargo install --path .` puts it. Passing the bare name
  therefore works on a developer's machine (where a `bsed` may be on `/usr`) and
  fails for everyone who installed it the documented way. Resolve the absolute
  path once via `$PATH` on the host (`bsed_path`), pass that in argv, and add it
  to the namespace as an extra `--ro-bind` at its own path — read-only even in a
  writable namespace, since a tool call has no business rewriting the binary it
  is running. `build_bwrap_args` takes `extra_ro: &[PathBuf]` for this; the bash
  and grep paths pass `&[]`.
* Output cap: `capture_bounded` truncates at `max_output_bytes` (default 20 KiB),
  which is *below* our 50 KB preview budget. Pass an explicit cap of
  `budget + 8 KiB` slack and treat a short read as an error rather than parsing a
  truncated blob (§5.5).
* `run_sandboxed_ro` currently discards the exit code. `bsed` needs a
  three-valued contract: `0` = blob on stdout, `2` = model-fixable error (message
  on stderr, forwarded as a normal tool result so the UI doesn't paint it as a
  crash), `1` = internal error. Add a status-returning RO variant next to
  `run_sandboxed_with_status`.

### 5.4 Resolved: what the deny-list rejects, and why

**`q` / `Q` — reject.** The question was whether allowing them has any value, and
it does not, in this tool. Every useful use of `q` is about *not* processing the
rest of a stream: bounded reads in a pipeline (`sed -n '1,10p;10q'`), early exit
from a huge input, `-n 'q'` used as a grep-like scanner. None of that applies —
`bsed` already holds the whole file, diffs it, and the preview is the *result*, not
a filtered stream. The only effect `q` can have here is to stop the output early,
which the diff then renders as "every line after N was deleted". That is a
maximally misleading preview, and it is misleading in exactly the direction that
loses work. Reject with the reason spelled out, so the model can route around it:

```
expression 1: 'q' stops processing early, which this tool would report as
"delete the rest of the file". There is no useful form of it here — the whole
file is already loaded. Use an address or a range instead (e.g. '1,400d').
```

`Q` is the same thing with a variant spelling, and `q N` (N≠0) additionally
reaches `std::process::exit` (§5.2).

**`r` / `R` — allow.** The deny-list adds nothing here: everything readable inside
the namespace is already reachable through `read`, `grep` or `bash`, so refusing
`r` is not a security boundary, only a functional restriction. And `279r
/tmp/model_filter.rs` is a real refactor idiom ("splice this file in here") that
the model reaches for. Two consequences to handle rather than block:

* **Budget.** Spliced content flows through the same diff and the same 50 KB cap,
  so `r` of a large file truncates like anything else — and the banner says so.
* **Marker hazard, now reachable via a file the tool never read.** The §4.5 rule 5
  check runs last, over the *final* block text, precisely so a marker line arriving
  through `r` is caught like one from the source file. It returns the
  `oldText`/`newText` error rather than emitting an unappliable blob.

**Truncation — banner + `exit 0`.** Agreed: report and guide, don't fail. The
model gets a usable prefix plus an explicit "these 37 blocks are complete, the
other 115 are not" line and the instruction to apply this batch first and
re-preview for the rest. A hard error would throw away work the model can already
put to use, and the failure mode it guards against (a model believing it saw
everything) is covered by stating the count in the first line.

### 5.5 Size budget

Per your call: **no input-size cap** (the file being previewed is unbounded), and
a **~50 KB preview cap**. Concretely:

* `bsed` builds blocks in order, appending while the rendered blob stays under
  `[tool_config.sed] max_preview_bytes` (default 50 KiB), and stops **at a block
  boundary** — never mid-block.
* The banner then states exactly what happened:
  `preview: 37 of 152 block(s) for src/foo.rs (preview budget 50 KiB reached — the remaining 115 blocks are NOT shown). Apply these first, then re-run with narrower expressions (e.g. add line ranges) for the rest.`
* The trailer repeats it, so a partially-read result is visible from either end.
* Truncation is a normal result (`exit 0`) with the banner, **not** an error: an
  error would hide a perfectly usable prefix, and the intended workflow is
  preview → apply → preview → apply (§5.4).
* Block count is reported either way, so the model always knows whether it is
  looking at everything.

## 6. Engine fidelity — `sed-rs` 2.0.0 vs GNU sed 4.9

Identical output across ~60 scripts: `s///`, `s///g`, `s///I`, `s///N`, `s///2g`,
`s///I2`, delimiters `| # ,`, addresses `N` `$` `/re/` `N~M` ranges `a,b` negation
`a!b`, `d`, `p`, `=`, `y///`, `a`/`i`/`c` (single line), `{ }` with labels and
`b`/`t`, hold space, `n`/`N`/`D`/`z`, BRE `\( \)` `\{ \}` `\+` `\?` `\|`, `\<`/`\>`
in patterns, POSIX classes, replacement backrefs, `\n`/`\t` in replacements, empty
input, no-match scripts.

Nine divergences found. The split that decides the work is not
popular/unpopular but **silent-wrong vs loud-error**:

| # | Construct | GNU | sed-rs | Class | Decision |
| --- | --- | --- | --- | --- | --- |
| 5 | input has no trailing newline | preserved | a `\n` is added | **silent-wrong** | **normalize before diffing** (mandatory — this is about ordinary files, not exotic syntax) |
| 1 | `\u \l \U \L \E` in the replacement | applies case transform | literal passthrough | **silent-wrong** | **shim**, ~80 lines by the sentinel method in §6.1 |
| 2 | `c` over a line **range** | whole range → one copy | one copy per line | **silent-wrong** | **reject**: "range `c` is a rewrite; use the `edit` tool". Emulating delete-range+insert is where a diff-based preview stops being trustworthy. |
| 3 | `/re/,+1` relative address | ok | parse error | loud | none — improve the error hint |
| 4 | `s///M` (GNU 4.8 modifier) | ok | parse error | loud | none |
| 6 | `s/x/9/ 2` (space before flags) | ok | parse error | loud | none (hint: "no space between the closing delimiter and the flags") |
| 7 | `s/a/b/gg` (invalid flag) | error | applied as `g` | loud-ish | none — it errs toward *more* change, visible in the preview |
| 8 | `\<`/`\>` in the *replacement* | backslashes consumed | kept literally | loud | none — the literal `\<` is visible in the block |
| 9 | `s/a/b/e` | executes RHS via `sh -c` | parses as `Substitute` + `Execute` | **safety-positive** | already rejected by the deny-list |

Net: one normalization, one shim, one rejection. Everything else already fails
loudly, and loud is fine — the model gets a clear error and retries.

### 6.1 The case-escape shim, and why it is ~80 lines and not ~40

`\u \l \U \L \E` are the only *silent-wrong* gap a model is likely to hit by
accident, so it gets fixed. The naive approach — re-implement `s///` for these
commands — is the wrong shape: `sed-rs` exposes no hook into how it builds a
replacement, so that means a second substitution engine (BRE translation, `&`,
backrefs, `g`/`I`) and two implementations to keep agreeing. Instead, note what the
divergence actually is: GNU *case-transforms a span that is otherwise copied
verbatim from the old text*. `sed-rs` copies it verbatim and leaves the `\U`
characters sitting there. So the transform can be recovered afterwards.

**Sentinel method** — three steps, all inside `script.rs` / `engine.rs`:

1. **Rewrite the replacement before it reaches the engine.** For each `s` command
   whose replacement contains a case escape, replace the escapes with a pair of
   sentinel strings around the affected span, carrying the intended mode:
   `s/pat/\U&/g` becomes `s/pat/<OPEN-U>&<CLOSE>/g`. `\E` closes the current span
   and resets the mode, so `s/a/\Ufoo\Ebar\lbaz/` becomes
   `<OPEN-U>foo<CLOSE>bar<OPEN-L>baz<CLOSE>`. Everything else in the replacement
   (`&`, `\1`, literals) is left as written, because the engine still expands it —
   and it expands it *before* we ever look at the output.
2. **Run the engine normally.** The sentinels are ordinary characters to it (we
   verified `sed-rs` passes unknown bytes through literally — which is exactly why
   it currently leaves `\U` in place). Each match emits its own sentinel pair, so
   the `g` flag needs no special handling.
3. **Collapse the sentinels after evaluation, before the diff.** Scan the output;
   between an opener and its closer apply the carried mode (`to_uppercase`,
   `to_lowercase`, `to_titlecase` for `\u`/`\l`); then drop the sentinels. The
   result is byte-identical to what GNU sed prints for this flag set.

Three details make it safe rather than merely clever:

* **Nonce the sentinels.** Generate a per-run random token (private-use
  codepoints plus a nonce) and regenerate it if it occurs in the input, so a file
  cannot forge one.
* **Verify none survived unbalanced.** A *lone leftover* marker is a hard error:
  it means a later command in the script rewrote one half of a pair, and writing
  that private-use codepoint into the user's file — or dropping the transform
  they asked for — is the one way this can go wrong. Absence of markers is
  **not** an error: a substitution that never matched simply emits nothing, and
  refusing that would break the commonest case there is.
* **Order.** Collapse *after* the full script has run and *before* the trailing
  newline normalization and the diff, so nothing downstream ever sees a sentinel.

Scope: this covers `\u \l \U \L \E` together with `&`, backrefs, `g` and `I` — the
whole GNU surface for these escapes. Each `s` command carries its own sentinels,
which is also GNU's per-command scoping.

## 7. `bsed` — the standalone tool

```
bsed [-c N] [-p PATH] <expressions>   # print the block blob (the default)
bsed -r -p DIR <expressions>          # walk a tree, one blob for all of it
bsed apply  [-p PATH] [-i] [FILE]             # read a blob from stdin, apply
bsed --version                                # "bsed 0.1.0" (probe + handshake)
```

* `preview` never writes. It is what the agent calls, always inside the sandbox.
* `apply` is for humans. Without `-i` a **single-file** blob writes its result to
  stdout, so `bsed apply f.rs < blocks.txt | diff f.rs -` is a review loop that
  cannot touch the file. A **multi-file** blob requires `-i`, since there is no
  single result to print.
* `preview -p -` reads the file from stdin and writes the blob to stdout (for
  pipelines). Not used by the agent.
* Exit codes: `0` ok, `2` the caller's problem (a refused expression, an
  unresolvable block, a partial apply), `1` ours.
* No dependency on an external `sed` binary, no shell, no config file.

### 7.1 The recursive flow

```
bsed -r -p src 's/foo/bar/g' > blocks.txt   # review
bsed apply -i < blocks.txt                          # commit it
```

One preview format and one write path, for a tree as well as a file — which is
the whole reason this is a *flag* and not a separate `replace` subcommand. A
second mode would either duplicate the diff→block machinery or become a second
way to write files, and a second writer is exactly what this design removes.

* **No budget in `-r` mode.** A human is reading the output, not a model paying
  per token, and a silently truncated tree refactor is the worst outcome
  available. The 50 KiB budget is for the agent's single-file preview; `-b` does
  not apply here.
* **The walk** uses `ignore` (already in the tree via picocode): `.gitignore` is
  honoured *without* requiring a git repository — skipping `node_modules` should
  not depend on whether the tree is under version control — hidden files are
  skipped, and files are visited in **sorted order** so two runs over one tree are
  diffable. Files that are not UTF-8 text are counted and named in the banner
  rather than failing the run; a tree is allowed to contain binaries.
* **`-g`/`--glob`** narrows the walk: repeatable, any match wins, matched against
  the path relative to the walk root. It uses the same matcher and the same
  semantics as the `find` tool, so one mental model covers both — which means a
  recursive match is written `**/*.rs`, not `*.rs`.
* **The banner** reports files, not blocks: `2 of 4 file(s) under src changed,
  3 block(s) total`, plus a note when files were skipped or when a file matched
  but could not be expressed as blocks (§7.4).

### 7.2 Apply is atomic *per file*

This is the one place the design changed under contact with a real use case. The
original plan was all-or-nothing across the whole blob, on the grounds that a
half-applied refactor is the failure mode this tool exists to prevent. For a
single file that is exactly right. For a tree it is unusable: one stale file
would block two hundred others, and "apply the rest, tell me what did not work" is
the only workflow that scales. So:

* each file is applied on its own — **all-or-nothing per file**;
* a file whose blocks no longer resolve is reported and **left untouched**, while
  every other file still goes through;
* the **failed blocks are echoed back**, and the report is *itself a valid blocks
  file*: the diagnosis goes in prose, the blocks come out bare at the end. So
  after fixing one you re-pipe the report with no detangling —
  `bsed apply -i < report.txt` — and nothing is lost but the failures themselves.
  This is what `ApplyOutcome::Failed { failed }` carries: the rendered report
  says *what* went wrong, the indices say *which* blocks, and a caller can hand
  back exactly those. (The `edit` tool does not need the indices — it shows the
  model everything — but it is the same type either way.)

  That trick leans on one property of the blob parser: prose between blocks is
  ignored. It holds only while no diagnostic line is a *bare token*, which the
  parser would read as a `path:LINE` header, so `prose()` enforces it rather than
  trusting the wording. A unit test pins the invariant.
* the run's report goes to **stdout**, because it is the result: which files
  applied, and the blocks to fix for the ones that did not. stderr carries the
  one-line failure count, and the exit code is 2, so a script that reads neither
  stream still cannot miss a partial run.

**Note the anchor in the echo.** It is re-rendered with the anchored headers
`preview` emits, not with `render_blob` — which writes one bare path line above
the first block. Re-piping a bare-path block for a duplicated line would resolve
by content alone on the retry, and fail.

### 7.2a Losing the output to a scrollback

When stdout is a **terminal**, any output carrying blocks is also written to
`$TMPDIR/bsed-<pid>-<n>.blocks`, and the path is named:

```
bsed: wrote a copy to /tmp/bsed-5-1790649955849874731.blocks
      edit it, then apply with: bsed apply -i < /tmp/bsed-5-1790649955849874731.blocks
```

The reasoning is narrow and practical: scrolling back through a terminal to copy
a block verbatim is exactly how you produce a wrong edit, and the report is meant
to be *re-piped* rather than retyped. A **piped** run is already capturing the
bytes, so it spools nothing and leaves the filesystem alone. Output with no
blocks in it is not spooled either — there would be nothing to re-pipe, and a
pointer would be noise. `BSED_SPOOL_DIR` overrides the location.

### 7.3 A file that cannot be expressed as blocks

Previewing a file that is *itself* a blocks file (`bsed -r > blocks.txt`
is a normal thing to do, and then previewing `blocks.txt`) puts literal
`@@@NEW` lines inside a block, because that is what the file contains. The blob
format cannot express it: those lines are structure, not content.

`blocks::verify` existed for this, but `preview_text` built its own banner and
dropped the result — so **both** paths emitted a block set they described as
appliable, and `apply` then rejected it. Now:

* the check runs over the in-memory blocks *first*, so the message names the
  block and the offending line instead of the parser's generic "unexpected
  delimiter";
* it then re-parses as confirmation, which also catches a silent round-trip
  failure — a block that does not survive `parse_blocks` unchanged is an internal
  error, not something to hand to the user;
* **single file**: refused, exit 2, with the `edit` tool's `oldText`/`newText`
  form suggested as the way through;
* **recursive**: the file is *named* and left out, and the rest of the tree still
  previews. One unrepresentable file must not sink a tree — but silence would
  read as "that file needed no change", which is the failure mode this tool
  exists to prevent.

### 7.4 The multi-file seam

`blockedit::group_by_file(parsed, fallback_path)` is where "a blob may span
files" meets "a write is atomic per file": it splits a parsed blob into one batch
per file, in first-seen order, with header-less blocks falling back to the
caller's `path` argument. It is the single place that knowledge lives, and it is
what the recursive apply uses.

It is also the seam the agent's multi-file `edit` will need. Splitting a
multi-file batch into per-file, pi-compatible tool calls for session storage is
the same operation, and it now has one tested implementation to call rather than
a second one to write.

**Distribution: one `cargo install --path .`, two binaries.** Cargo installs every
`[[bin]]` target of the package you point it at, so the only question is whether
`bsed` is a target of the `picocode` package or of a different one. It should be a
target of `picocode`, as a two-line shim that delegates to the crate:

```rust
// picocode/src/bin/bsed.rs
fn main() -> std::process::ExitCode { bsed::cli::main() }
```

with `bsed = { path = "crates/bsed" }` as a dependency.

Then `cargo install --path .` installs **both** binaries, while `crates/bsed`
stays self-contained and separately installable later (`cargo install --path
crates/bsed`, or published and installed by name). The release build already
produces both, so `release.yml` needs only the archive step updated and
`install.sh` only two names. The cost is honest and small: `picocode` gains a
compile-time dependency on `sed-rs`, and the tarball grows by one binary — worth
paying for "the tool is simply there after a normal install", which is what turns
the conditional advertisement in §9 into graceful degradation rather than a
feature most users never get.

## 8. Crate layout

```
crates/bsed/            # package: lib + bin bsed. No dependency from picocode.
  Cargo.toml                # sed-rs = "2", similar = "2", thiserror = "2", clap? (hand-rolled argv is fine)
  src/lib.rs                # pub fn preview(req) -> Result<Preview, Error>
  src/script.rs             # split lines, parse each, walk the AST (deny-list), join
  src/engine.rs             # eval + trailing-newline normalization + \u\l\U\L\E shim
  src/hunks.rs              # TextDiff -> Vec<Hunk>  (non-empty OLD, borrow rule)
  src/blocks.rs             # Hunk -> blob, budget enforcement, banner
  src/bin/bsed.rs           # argv parsing, exit codes
  tests/                    # golden + contract tests

crates/blockedit/           # NEW: the pure block-edit core, extracted from picocode
  src/parse.rs              # from src/edit_blocks.rs   (parse/render/canonicalize)
  src/match.rs              # from src/edit_match.rs     (resolve/diagnose; `similar`)
  src/apply.rs              # from src/tools.rs         (resolve_edits + apply-on-text)
  src/types.rs              # EditOp / EditStartLine, serde names unchanged
```

`blockedit` is what makes §7's `apply` and the round-trip contract possible, and
it is the risky part of this proposal: `src/edit_blocks.rs` (759 lines),
`src/edit_match.rs` (928 lines) and `edit_file`/`resolve_edits` in `src/tools.rs`
must move with their tests, keeping the persisted JSON shape
(`{"oldText","newText","startLine"}`) byte-identical for session replay.
`picocode` re-exports the types; `bsed` depends on `blockedit` for the markers
and to self-validate the blob it emits.

The extraction should be its own PR, before any `sed` work: it is a refactor with
no behaviour change, and it de-risks the crate work.

## 9. Integration in picocode

| Where | Change |
| --- | --- |
| `Cargo.toml` | `members = [".", "crates/slashdot_menu", "crates/blockedit", "crates/bsed"]`; `blockedit = { path = … }`. add `bsed` as a dep, purely for the `bsed` shim binary (§7). |
| `src/tools.rs` | `#[rig_tool(name = "sed", required(path, expressions))] async fn sed_preview(path, expressions, context: ContextLines)`: `get_path` → relativize → argv `["bsed",expressions,"-c",n,"-b",budget,"-p",rel]` → `run_sandboxed_ro_with_status` (new, §5.3) with an explicit cap → map exit 2 to `Ok(message)`, 1 to `Err`. New `ContextLines` in `src/config.rs` cloned from `ReadLimit`. |
| `src/tools.rs` | `bsed_available()` probe (`LazyLock<bool>` = `Command::new("bsed").arg("--version").status().is_ok()`), mirroring `is_executable` in `src/main.rs:97`. |
| `src/agent.rs:1967` | register conditionally: `if bsed_available() { builder = builder.tool(SedPreview) }` — same conditional-advertisement shape as `guard(..., yolo, …)` for the mutating tools. |
| `src/agent.rs:1923` | prompt: replace the "avoid sed … snapshot commit first" sentence with "use `sed` to preview bulk line changes — it never writes; review the returned blocks and apply them with `edit`." Only append the `sed` bullet when the tool is registered. |
| `src/output.rs:804` | panel title: `sed <path> (N expressions)`, body rendered with the existing context-rich diff renderer (§4.6) — the human sees context the model never pays for. |
| `src/session/storage.rs:23` | `CANONICAL_NAMES`: `("sed_preview", "sed")`; no arg renames needed. |
| `src/config.rs` | `[tool_config.sed] { max_preview_bytes = 51200 }` (optional, default 50 KiB). |
| `install.sh` | extract and install both names from the one archive; warn when the second is missing. |
| `.github/workflows/release.yml` | `cargo build --release --target …` already builds every workspace member, so the archive step just needs both binaries: `tar … picocode bsed`. |

## 10. Worked example (real prototype output)

`src/foo.rs`:

```
1  fn main() {
2      let x = 1;
3      let y = 2;
4      if count > 0 {
5          run();
6      }
7      // trailing
8      let z = 3;
9  }
```

| `expressions` (ctx) | result |
| --- | --- |
| `5s/run/walk/` (0, **default**) | one block at `src/foo.rs:5`, OLD = `        run();`, NEW = `        walk();` |
| `5s/run/walk/` (1) | one block at `src/foo.rs:4`, OLD = 3 lines (context + change + context), NEW = same with `walk()` |
| `6,7d` (0) | one block at `src/foo.rs:6`, OLD = `}` + `// trailing`, NEW = *(empty)* |
| `2a\⏎    let w = 0;` (0) | one block at `src/foo.rs:3`, OLD = `let y = 2;`, NEW = `let w = 0;` + `let y = 2;` (borrow rule) |
| `1i\⏎// header` (0) | one block at `src/foo.rs:1`, OLD = `fn main() {`, NEW = `// header` + `fn main() {` |
| `999s/zzz/yyy/` (0) | `no lines matched — src/foo.rs is unchanged` |
| `s/run/walk/g` over a file with 40 `run();` lines (0) | 40 one-line blocks, each anchored to its own line; all apply in one `edit` call (§4.1, test 6a) |
| `1,$d` | exceeds the budget → truncated at a block boundary; the banner states the remaining blocks are NOT shown |

All rows are real output from a prototype built on `sed-rs` + `similar`, not
hand-written.

## 11. Test plan

1. `script.rs` — deny-list unit tests per rejected node, **including nested inside
   `{}`**, and `s///e` (which parses as `Execute`); a test asserting the `match`
   has no wildcard arm is enforced by the compiler, not by a test.
2. `engine.rs` — golden `expression -> text` table for every construct in §6, plus
   a test pinning the *known* gaps so a `sed-rs` upgrade that changes them is
   noticed rather than silently altering previews.
3. `hunks.rs` — modify / insert-at-BOF / insert-at-EOF / delete-tail / delete-all /
   two-close-hunks (assert no overlap) / file without trailing newline.
4. `blocks.rs` — golden blobs + budget truncation at a block boundary.
5. **Round-trip contract** (the most valuable test in the project):
   `blocks.rs` output → `blockedit::parse_blob` → assert parsed
   `(old_text, new_text, anchor)` equals the hunk slices and anchors are exact.
   Run it as an integration test in *picocode* too, so the two sides cannot drift.
6. **Anchored-resolution tests against the real `edit` backend** — the assumption
   §4.1 rests on, pinned as an executable claim rather than a comment:
   (a) a file with 40 identical lines, previewed at `context = 0` as 40 one-line
   anchored blocks, applied in one `edit` call → all 40 land, none rejected;
   (b) the same blob with one block's anchor deliberately shifted by 3 lines →
   must fail cleanly ("anchor out of tolerance"), not apply to the wrong line;
   (c) a blob with its headers stripped → falls to the unanchored pass and is
   rejected as non-unique, which is exactly the argument for anchoring every block.
7. `blockedit` extraction — existing tests move with the code; add a session-replay
   test over a recorded `edit` call to prove the JSON shape is unchanged.
8. Tool-level — `sed` absent from `$PATH` ⇒ tool not advertised; exit 2 ⇒ message;
   file outside the sandbox's visible bindings ⇒ clear error. Panel rendering
   (§4.6) shows context even though the model payload has none.

## 12. Decisions taken, and what is left

All of the review questions are settled; the reasoning lives in the section each
row points to.

| # | Question | Decision | Where |
| --- | --- | --- | --- |
| 1 | Truncation signalling | banner + `exit 0`, with shown/total counts and "apply this batch first, then re-preview" | §5.5 |
| 2 | `q` / `Q` | **reject** — no useful form exists once the whole file is loaded, and its only effect is a maximally misleading preview | §5.4 |
| 3 | `r` / `R` | **allow** — the sandbox already exposes those files to the model; the deny-list is for write/exec/early-exit only | §5.4 |
| 4 | `\u \l \U \L \E` | **implement** via the sentinel method (~80 lines, not a second `s///` engine) | §6.1 |
| 5 | Distribution | one `cargo install --path .` installs both, via a `[[bin]]` shim; `crates/bsed` stays independently installable | §7 |
| 6 | `crates/blockedit` | **separate crate** — keeps the binary separable later | §8 |
| 7 | Per-expression detail | **deferred**, low priority | §13 |
| 8 | Balance growth | **ship growth in v1** (~300 lines), keeping the hint for what growth cannot fix | §4.6 |
| 9 | Default `context` | `0` with a free 2-line trailing floor; symmetric on request | §4.1 |

Resolved since (reasoning in §7):

| # | Question | Decision | Where |
| --- | --- | --- | --- |
| 10 | Separate `replace -r` mode, or a flag on `preview`? | a **flag** — one output format, one write path | §7.1 |
| 11 | Scope of atomicity for a tree | **per file**: a stale file is reported and skipped, the rest apply | §7.2 |
| 12 | Budget in `-r` mode | **none** — a human is reading it, and silent truncation is the worst outcome | §7.1 |
| 13 | How do you see which replacements did not apply? | the **failed blocks are echoed back**, with paths and anchors, ready to re-pipe | §7.2 |
| 14 | Trailing context by default? | **none** — `trailing: 0`, on both the CLI and the agent tool. Every block is anchored from the file just read, so the tail only bought slack against a stale anchor, at a third of the budget on every change | §4.1 |

Still open — small tuning decisions, not architecture:

1. **Growth cap.** How many lines may a grown block add before we give up and emit
   the banner hint instead? 40 was the sketch value; 20 is probably the honest one
   for code review.
2. **Angle brackets.** Keep the "count `<` only if a `>` appears within the cap"
   guard, or drop `<`/`>` from balancing entirely? Dropping is safer but stops
   `Vec<T>` and `HashMap<K, V>` in a changed line from triggering growth.
3. **`bsed apply` in v1** or with `replace -r` later? `apply` is the piece a shell
   user reaches for first, but it is not available until `blockedit` is extracted.

## 13. Deferred

* Per-expression match report (`expression 2 's/a/b/g' changed 12 lines`), behind
  `--explain` or a config flag. The banner's aggregate covers the common case.
* A `--glob` filter for `preview -r`. `.gitignore` handles the common noise
  (`target`, `node_modules`), but "only `*.rs`" is a one-line addition on top of
  the same `ignore` walk and would make a tree refactor much less noisy.
* Multi-file `edit` in the agent. The plumbing is in place (§7.3); what remains is
  splitting a batch into per-file tool calls and re-joining the results, which is
  a session-storage question rather than a matching one.
* Publishing `bsed` to crates.io, so `cargo install bsed` works without a
  git checkout.
* A `--json` output mode, for tooling that wants the blocks
  without parsing the blob text.

## 14. Where the implementation corrected this document

Four things the design got wrong or left open, found while building it, and
settled in the code. Each is covered by a test that fails if it regresses.

**1. A contiguous rewrite is one block, not many.** §4.1's "40 identical lines
→ 40 one-line blocks" only holds for *scattered* duplicates. `similar` folds ops
whose gap is `<= 2*ctx` into one group, so 40 *consecutive* identical lines become
a single 40-line block. This is better than what was claimed: a mass rename shows
up as one reviewable hunk, applied atomically. (See also §4.4.)

**2. Growth is measured from the change, not from the block.** As first written,
growth scanned the block's whole `OLD`. That is wrong in both directions, and both
directions bit during development:

* with leading context, a context line that opens a brace (`fn main() {`) dragged
  the block out to the end of the function;
* with the trailing floor, *our own padding* triggered growth — a change on
  `let x = 1;` followed by `if count > 0 {` pulled in the closing brace four
  lines later, which is the opposite of what the caller asked for.

Growth now scans exactly `[first changed line, block end)`, so only an imbalance
sed itself introduced can trigger it. The mid-construct note in the banner was
narrowed to match: it fires when growth ran and could not reach the closer, not
when the caller's own context ends inside a brace.

**3. `grouped_ops(0)`, then pad both directions ourselves.** Passing `context` to
`grouped_ops` applies it symmetrically, which double-counts against the
asymmetric trailing floor: `-c 1` came out as 1 before and 2 after. Since the
contract is asymmetric, the padding has to be too — and doing both sides in one
place means every bound is clamped in one place (§4.4).

**4. The case-escape guard watches for imbalance, not absence.** §6.1 specified
failing when "no markers survived", on the theory that a later command ate them.
That fails on every substitution that never matched, which is the commonest case
in the corpus. A lone *leftover* marker is the real signal and is still a hard
error; absence is a no-op. Verified byte-identical to GNU sed 4.9 across seven
scripts, including backreferences, `\u`/`\l` and `\E`.

### Corrections from the recursive round

**`ParsedEdit` was throwing away the one field a multi-file blob needs.** The
header path was used for the single-file agreement check and then dropped — the
anchor survived, the file did not. Multi-file was unreachable only because
`merge_path` rejects it, which is a policy decision wearing a data-structure
costume. `ParsedEdit` now carries `header: Option<String>`, and
`group_by_file` is the one place that knows how to split a blob by file.

**A file that cannot be expressed as blocks was not being caught at all.**
`blocks::verify` was correct, but `preview_text` built its own banner and dropped
the result, so both preview paths described a block set as appliable when it was
not. The check now runs first over the in-memory blocks — so the message names
the block, not the parser's generic complaint — and re-parses as confirmation
(§7.3).

**Per-file atomicity, not all-or-nothing.** The original reasoning was that a
half-applied refactor is the failure mode to prevent, and that is right for one
file. It does not survive contact with a tree: one stale file blocking two
hundred is not a safety property, it is a dead end. Splitting the difference by
file keeps both halves of the claim — no file is ever half-written, and the ones
that failed come back as blocks to fix.

Two smaller notes on what shipped:

* **`bsed apply` is in v1.** The question was whether to defer it, but it fell out
  of the `blockedit` extraction for free — it is `apply_to_text` plus a file
  write — and without `-i` it writes to stdout, so `bsed apply f.rs < blocks.txt |
  diff f.rs -` is a review loop that cannot touch the file. It is how the
  round-trip property is tested.
* **The trailing floor is a tool-side constant.** The `sed` tool passes
  `-t 2` explicitly rather than relying on the CLI default, so the value the
  agent depends on (and the ±1 anchor tolerance it buys) is stated at the call
  site in `src/tools.rs` rather than implied by a default three layers down.
