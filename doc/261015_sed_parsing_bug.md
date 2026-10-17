# SED tool syntax issues

*Status 261016: the bracket-expression bug below is **fixed** — option A. `sed-rs` commit `93309fa`
(`gwicke/sed-rs` main), `cargo update -p sed-rs` done; regression cases live in
`crates/bsed/src/lib.rs` (`preview_applies_back_to_exactly_what_sed_produced`), which rounds the
scripts out through GNU sed as the reference. The workarounds below are no longer needed. The tool
wording (option D) and the `sed -f` docs TODO stay open for a manual pass.*

## Document `sed -f` syntax - no escapes

Add this in the tool documentation

## The one genuine bug: raw `/` inside a bracket expression

The parser scanning for the pattern's closing delimiter doesn't skip `[...]`, so a literal `/`
inside a char class terminates the regex early:

```text
s|a[/]b|X|g          → parse error: unknown command: 'X'
s/[^/ ]*/X/g         → parse error: unknown command: 'X'
s/, impl doc [§0-9][0-9A-Za-z./§-]*//g
                    → parse error: unterminated regex (expected closing '/')
```

GNU sed accepts all three (`sed -e 's|a[/]b|X|g'` → `aaXbb ccc`). It also bites with the `/`
delimiter even though the class "contains" the delimiter legitimately — that's the error I hit in my
first batch attempts (`[0-9A-Za-z./§-]`), which is why I spent so long fighting "missing
command"/"unknown command: '('" errors that had nothing to do with parens. Note it hits the
**replacement** side too, presumably for the same reason — though I didn't construct a failing
replacement-side case here.

  Workarounds that work through the tool, confirmed:

- `\/` inside the class: `s/[a\/b]*a/X/g` works (GNU sed accepts the escape inside `[]` too).
- Split the rule: drop that expression from the batch and match the content differently (e.g. `.*`
  instead of a class containing `/`).

## Investigation findings (261016)

### Reproduced

All three expressions above fail through `target/debug/bsed` with exactly the reported errors, while
GNU sed accepts them:

```text
bsed 's|a[/]b|X|g' f                  → parse error: unknown command: 'X', exit 2
bsed 's/[^/ ]*/X/g' f                 → expression 0: parse error: unknown command: 'X', exit 2
bsed 's/, impl doc [§0-9][0-9A-Za-z./§-]*//g' f
                    → expression 0: parse error: unterminated regex (expected closing '/'), exit 2
```

Baseline green before any change: `cargo test -p bsed` (33 passed), `cargo test -p sed-rs`.

### Root cause: upstream `sed-rs`, not bsed

The error strings live in `sed-rs`'s `src/command.rs`, not in this repo. `parse_regex_delimited`
(command.rs:403) scans character-by-character for the closing delimiter with no notion of `[...]`,
so a raw delimiter inside a bracket expression ends the regex early; the tail is then re-read as
commands (`unknown command: 'X'`). `bsed` gets it via `[patch.crates-io]` → `gwicke/sed-rs` branch
main (Cargo.toml:147; upstream is `quinnjr/sed-rs`, already carrying our open `a`/`i`/`c` whitespace
PR). `sed_rs::eval` re-parses the same script text (sed-rs lib.rs:144), so one fix in the scanner
covers both the error and execution.

Scope, verified against GNU sed:

- Hits every regex context: `s///` pattern **and addresses** — `bsed '/[a/]/d'` →
  `unknown command: ']'`. The address side was not in the original report.
- The **replacement side is not a bug** (open question above, now resolved): GNU sed does not treat
  `[` as special in replacement text either — `sed 's/hello/[a/b]/'` →
  ``unknown option to `s'``. `parse_replacement_delimited` matches that behavior; only the message
  differs. Weird errors like `unknown command: ']'` on the replacement side come from the *pattern*
  having been cut short earlier in the script.
- `y///` also feeds `parse_regex_delimited`, so a fix must not make the `y` scanner
  bracket-aware: GNU treats `[` as an ordinary char there (`sed 'y/[/x/'` translates a `[`).

GNU semantics a fix must match (all verified empirically):

- A raw delimiter inside `[...]` does not terminate the pattern: `s|a[/]b|X|g`, `s/[a/]/Q/`.
- `]` first in a class (after optional `^`) is literal; POSIX-class and collating interiors do not
  close the class early: `s/[[:digit:]/]/Q/g` works in GNU.
- Backslash inside a class is **literal** in GNU (POSIX rule): `s/[\]]/Q/` does not match `]`, and
  `s/[\.]/X/` matches a literal backslash too. The current scanner's backslash branch (consume the
  next char unconditionally) is what makes the `\/` workaround pass; keeping it costs nothing for
  delimiter-skipping even though it is not byte-exact POSIX.

## Fix options

- **A — Fix it in `sed-rs` (recommended).** Add a bracket-aware scan for regex contexts
  (`s///` pattern, `/re/` addresses) and keep the plain scanner for `y///`; rules: raw delimiter
  inside `[...]` is skipped, `]` first (after `^`) is literal, `[:...:]` / `[[....]]` interiors are
  consumed as units, backslash consumes the next char (as today). Regression tests in
  `command.rs`'s test module plus a GNU-comparison case in `crates/bsed/tests` (the GNU-reference
  helper in lib.rs:617 already exists). Push to `gwicke/sed-rs` main — already the pinned patch, so
  `cargo update -p sed-rs` is the whole integration — and fold into/upstream alongside the open
  `quinnjr/sed-rs` PR. Fixes every consumer (bsed, the sed-rs CLI, addresses, future callers) with
  one scanner and no duplicated parser knowledge in this repo.

- **B — Pre-escape rewrite inside bsed.** bsed already rewrites scripts before handing them to
  sed-rs (`rewrite_case_escapes`, `remove_change_ranges`): add a pass that escapes raw delimiters
  inside bracket expressions (`/` → `\/`) and pass the rewritten text to both
  `sed_rs::command::parse` and `sed_rs::eval` (consistent, since eval re-parses). No upstream
  wait, but bsed must re-derive *where* regexes start (any `s` delimiter, addresses, `y` regions
  and `a`/`i`/`c` text left untouched, multiline `c\`) — parser knowledge duplicated across
  crates with drift risk, echoed error text would show the rewritten script unless carefully
  mapped back, and the sed-rs CLI stays buggy.

- **C — Vendor a path-patched `sed-rs` into the workspace.** Replace the git patch with a vendored
  copy (`[patch.crates-io] sed-rs = { path = ... }`) and fix it here. Full control and immediate,
  but a permanent fork to carry — Cargo.toml:141 already wants to *delete* the temporary git patch
  once the whitespace PR lands, so this cuts against that direction.

- **D — Document only.** Keep the workaround section; add a line to the `sed` tool description
  (src/tools.rs:587) warning that a delimiter inside `[...]` must be written `\/`, and cover it in
  doc/bsed.md. Zero risk, but `[0-9A-Za-z./§-]`-style classes are common and the failure mode
  (`unknown command: 'X'`, or a paren glyph from a half-consumed script) is exactly the kind of
  misleading error that costs the model a long debugging spiral.

Path taken: **A**, landed 261016 (`sed-rs` `93309fa`, lock bumped, GNU round-trip cases added).
D's tool-description wording is left for a manual pass.

## Open item

The first section above (`Document sed -f syntax - no escapes`) is still a docs TODO, separate from
the parser bug: it belongs in the `sed` tool description (src/tools.rs:587) and/or doc/bsed.md —
the exact intended wording needs clarification.
