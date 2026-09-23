# Positional arg & modifier auto-complete
Needs
- Listing nodes: pervasive auto complete on dot modifiers
  This is *generic*; currently no need for customization beyond maybe
  field config level enums or value ranges.
  Uniform behavior is valuable. 
  - Share machinery, but have one default implementation for listing nodes.
- Leaf nodes: optional parameter and possibly modifier auto complete
  Nothing generic; usually needs specific value completes
  THIS is the target.
  - Use listing node complete machinery, but provide custom completion data

Goals
- Reuse listing, table rendering machinery / uniform UX: parsing, UI
- Avoid complications with child data() / traverse interaction for dynamic
  data

## Design sketch
Decision: Support single word (maybe later quoted string) completion only
- want to leverage listing machinery / fuzzy filtering, and provide consistent
  UX: need shared understanding of option syntax
- only auto complete last parameter in prompt: earlier parameters are treated
  as "committed". Completion machinery owns suffix including incomplete
  parameter until it is explicitly committed
  - How to distinguish completion / filter args from positional arg?
    - stateful / hidden: remember start of auto-complete parameter; only clear
      if "committed" prefix changes, or completion is exhausted / committed
      with enter
    - avoid spaces / fuzzy search `committed complet/some/wordsalad`
    - use separate input field, only update prompt once done
- provide earlier parameters to completion method, so that behavior can change
  by position (e.g. sandbox from / to path args) - PathArgs
- Need fuzzy filtering as separate words: 
  - `param`+tab -> list opens
  - user fuzzy searches by adding space + terms and/or dot modifiers (e.g. `param term`)
  - tab to select / apply -> `param_terminator`; if completion returns only an
    exact match (or no match), close auto-complete
  - if complete returned multiple rows: continue to show listing, repeat
- keep handler high level, focused on providing data
- handle all prompt manipulation at engine level

```rust
enum CompletionRequest {
  PositionalCompletion(prefix: Range<usize>),
  DotModifierKey(prefix: Range<usize>),
  DotModifierValue(key: Range<usize>, prefix: Range<usize>),
}

fn parameter_completion(
  &mut self,        // can access instance data
  args: &PathArgs,  // commited set of options, and full string
  completion: CompletionRequest, // Concrete thing to complete
) -> Option<Rc<Vec<String>>>
```

---

## Review — critical notes (post-Q&A)

### Verdict

Direction is right. The central insight — **completion candidates are
values, not nodes** — is the correct split and the main reason the
data()-based PathPicker approach fails: any word matching a row id
hijacks `parse_section` (a file named `ro` or `add` in the completed
directory would divert traversal into the leaf), fs churn dirties the
merged listing every keystroke, and candidates would be subject to
row-uniformity / id-uniqueness / D2-demotion rules that don't apply to
parameter values. Keeping candidates out of the id space and rendering
them through the existing `RenderPlan` stack fits the thin-client role
split (design §12) and needs essentially no new host code — `V25Menu`
already paints whatever plan the engine returns.

The Q&A resolved the sketch's open questions (anchor-based committed /
in-flight split; engine-owned rewrites; NodeEntry default for listings +
trait data for leaves; row-ish candidates). What follows is what still
needs deciding or fixing before implementation.

### Issues

**1. The key matrix is underspecified.** "Tab drills, Enter applies,
Esc invalidates" is a start, but each key has multiple meanings across
(session-active × listing/leaf × candidate-kind). Needs an explicit
decision table covering at least:

- **Tab on a non-terminal candidate** (directory, `.sort:` key token):
  apply without trailing space, clear fuzzy terms, keep session — this
  is the "drill" reading, and matches how the session must continue
  listing after `dir/`. Define "unique match": exactly one candidate
  matching the current filter? And decide Tab on a multi-match
  non-terminal prefix — shell users expect longest-common-prefix
  extension (`/home/ga` → `/home/gab`); currently undefined.
- **Enter**: apply + trailing space + close session (per Q&A). But
  Enter at a listing during a *modifier* session must route to
  completion-apply, not child-drill — and Enter at a leaf with no
  session is still submit. The routing rule falls out naturally if the
  completion view is **exclusive** (candidates fully replace rows, so
  the cursor indexes candidates, never `row_index` children) — state
  that invariant; it makes cursor/dispatch unambiguous.
- **Enter vs submit at a param-taking leaf**: arriving via parent
  DrillEnter with no params yet must neither submit nor apply row 0 —
  it should open the session and `Continue`. This is the same problem
  as the existing deferred item "Avoid drill-submit where args are
  expected" (`/session <id> rename`, slash_menu_v2.5_impl.md); this
  design should settle both, e.g. DrillEnter at a leaf with an openable
  completion shows the listing, and `submit()` guards missing params.
- **Esc**: close session first, ladder only on the next Esc? Or both in
  one press? (Recommend: session first — mirrors the §5.1 ladder
  layering.)

**2. "Enter applies the selection (with trailing space)" has a bug.**
A blanket trailing space breaks key completion: applying `.sort:` +
space yields `.sort: ` and the next typed char lands *after* the space
(`.sort: p`), splitting the token. v2.0 handled this
(`repl.rs`: `if val.ends_with(':') { "" } else { " " }`). Same issue
for path directories (`dir/` must continue, not commit). Fix: candidate
declares its terminator — either the v2.0 suffix convention
(`:`/`/` ⇒ no space) or an explicit continuation flag on the candidate
row. This also defines the sketch's undefined `param_terminator`.

**3. Dot-token syntax collides with path positionals.**
`parse_section` classifies *any* word starting with `.` as a modifier.
At a leaf, `./src`, `../foo`, and `.gitignore` are exactly the values
sandbox completion must produce: they'd be parsed as bare modifiers,
`positional()` would miss them (breaking `submit()`), and the
auto-trigger would fire on `./src` as if it were a key. Two candidate
resolutions:

- **(a) Node-kind-dependent classification**: modifiers (and dot
  auto-trigger) only at listing levels; at leaf levels a `.`-word is a
  positional unless it contains `:`. Colon-carrying words stay
  modifier-like so in-session candidate filters (`param .dir:true`)
  still work, and bare dotfiles complete as path prefixes. Costs a
  deviation from design goal #3 ("uniform per-level grammar") — but
  leaves have no rows to filter outside a session, so the deviation is
  mostly theoretical. Record it as a conscious grammar amendment.
- **(b) Tighten the modifier shape globally** (`.` + ident, no `/`) —
  insufficient alone: `.gitignore` is still ident-shaped.

Recommend (a). Whichever is chosen, add `./`, `../`, `.gitignore` to
the parse tests — this will otherwise slip through.

**4. Session-open timing vs key dispatch order.** Sessions are
auto-opened on passive frames (syntax trigger) but also opened by Tab;
Enter/DrillEnter must know whether a session existed *before* the
keypress (apply) or is being seen for the first frame (arrival → show
listing, don't apply). Specify: derive/open sessions only on
`None`/`MoveCursor` frames and on explicit Tab; keys act on an
already-open session. Also decide the default cursor: today
`seed_or_keep_cursor` seeds row 0, so Enter-without-arrows applies
row 0 — acceptable (v2.0 parity, and the `param`+Tab+Enter fast path
needs it) but then the arrival rule above is mandatory.

**5. Session kinds and the full invalidation list.** Two kinds with
different lifetimes:
- **syntax-opened** (dot auto-trigger): closes when the syntax stops
  matching (trailing token backspaced into an ordinary word) — the
  committed-prefix rule alone does *not* cover this;
- **user-opened** (Tab positionals): closes on committed-prefix
  change, Esc, final Enter (per Q&A); handle drop (buffer leaves `/`)
  kills EpochState anyway.
Store the kind + anchor (committed prefix, in-flight start) in
`EpochState`. For the anchor on syntax sessions, anchor = prompt up to
*before* the dot-token — the token itself changes every keystroke and
must not invalidate.

**6. Main mechanical cost: pluggable row source in `NodeEntry`.**
`ensure_view` / `build_render_plan` / cursor seeding all read
`content.data_rc` + `row_index`, which are populated from *children*.
Candidates must flow through the same filter → fuzzy-rank → sort
pipeline (the sketch promises "fuzzy + dot modifiers" *inside* a
session — that's free if candidates are `Row`s and the source set is
swappable), but `RowRef::resolve` indexes `data_rc` by child slot.
Expect a "current view rows" abstraction (children rows vs completion
candidates) as the core internal refactor; budget for it — this is more
than a bolt-on method.

**7. Trait / API shape.**
- Add `epoch: u64` to `parameter_completion` (parity with `data()`/
  `submit()`; lets fs completers invalidate TTL caches across menu
  reopens).
- Decide ranges vs slices: handlers almost certainly want `&str`
  slices (prefix, committed args); ranges are engine-internal for the
  rewrite. `CompletionRequest` as written forces every impl to index
  `suffix` itself.
- Return type: rows — `Option<Rc<Vec<Row>>>` (or a thin
  `CompletionRow { insert, description }`), consistent with Q&A #5.
  `Rc` enables the same ptr-eq skip so an unchanged prefix doesn't
  rebuild the view per frame; cache the `Rc` in the entry until
  suffix-since-anchor changes.
- `Option` can't express a failed completer (fs permission error).
  Silent "no candidates" is probably fine for v1; note it.
- Heavy work (fs listing) belongs in the node's `&mut self` — cache
  keyed by deepest dir + TTL as in sandbox_design.md §9.4; engine stays
  free of I/O.

**8. Doc / cross-doc cleanup.**
- The three-alternative bullet under "How to distinguish completion /
  filter args from positional arg?" is now decided (stateful anchor) —
  mark it, drop or archive the other two (`committed
  complet/some/wordsalad` and the separate-input-field option) or the
  next reader re-litigates it.
- Define `param_terminator` (see issue 2).
- sandbox_design.md §9.4–9.5 still describes the data()-rows
  PathPicker; per Q&A #6 it's superseded by leaf + this trait method
  (only §9.2's tree structure is updated). Add a pointer from this doc
  (or a staleness note there) so the two don't diverge during the
  sandbox port.

### Suggestions (beyond fixing the above)

- **Candidates-as-`Row`s pays off twice**: the in-session filter/sort
  machinery reuse (issue 6), plus free capabilities like a `dir`
  field on path candidates so `.dir:false` filters to files-only.
- **Frame the state model as "mostly derived"**: display = f(prompt,
  field space, candidates) computed fresh each frame; the *only* sticky
  state is (anchor, session kind, candidate cursor). That's a much
  easier invariant to test than v2.0's stateful overlay, and it makes
  "no candidates ⇒ no session" the natural exit rule — absorbing the
  sketch's exact-match/no-match close conditions.
- Tests worth writing first (they pin the design): parse classification
  for `./`, `../`, `.gitignore`, `.dir:`; invalidation on
  committed-prefix change vs in-flight edits vs mid-buffer edits;
  terminator rules; the key matrix; DrillEnter arrival at a param leaf;
  two-path sandbox flow end-to-end.

### Bare-prompt completion (Q8 — feasibility thoughts)

Not critical, but worth designing *for* without designing *in*:

- What's genuinely menu-specific is small: slash-tree node resolution
  (which leaf supplies data) and the dot-syntax auto-trigger. Everything
  else — anchor/committed-prefix state, candidates-as-rows, filtering,
  cursor, engine-owned rewrites, `RenderPlan` rendering — is
  completer-agnostic if kept in types that don't assume a `/`-prefixed
  prompt or a resolved node path.
- The current inline completer's worst problems (stale
  `completion_pos` ranges, completing not at buffer end) are exactly
  what a **modal** version avoids: restrict to the token at the cursor,
  pin the anchor there on open, render candidates via the normal plan,
  and treat any edit outside the committed prefix as invalidation.
  That is effectively your "separate menu modal with only a leaf that
  auto-completes" — a special root leaf whose `parameter_completion` is
  the sandbox-aware fs completer, driven by a non-`/` prompt. Feasibility
  is high.
- Two things to keep flexible while building v1: (i) don't weld the
  candidate view into `NodeEntry`'s child-data plumbing so hard that
  extracting it later costs a rewrite (issue 6's abstraction is the
  extraction point — "factor rendering out of NodeEntry" and "separate
  modal leaf" converge on it); (ii) express rewrites as
  (anchor-range → replacement) rather than "replace last token", so a
  future non-end-of-buffer variant isn't blocked. Design §1's
  out-of-scope statement for bare-prompt completion can stay untouched
  for now — this is about keeping the door open, not scope creep.

