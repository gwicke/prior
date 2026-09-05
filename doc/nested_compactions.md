# Nested compactions: design

## Status

**Proposed.** Aimed at picocode's `/tree <id> compact` and `/compact` paths. The
on-disk format remains unchanged — `SessionEntryContent::Compaction` keeps its
pi-compatible `{ summary, first_kept_entry_id, tokens_before }` shape
(`src/session/types.rs:204`). All changes are interpretation-level
(`apply_compaction_transforms`) and prompt-assembly-level
(`run_compaction_at_point` LLM arm), with one guard removed and one new
`previousSummary` selection rule.

## Motivation

Pi's compaction protocol is one‑way: the latest compaction on the path defines
the LLM context floor, and that floor can only move *backwards* (i.e. summarise
more, not less). Once a compaction is written, there is no protocol for revising
its summary — except to write a *new* compaction that supersedes it.

Today picocode mirrors pi's protocol mechanically (`apply_compaction_transforms`
in `src/session/session.rs:759‑813`, dispatch in `src/agent.rs:3649‑3735`).
This is fine for the *initial* compaction of a session, but two gaps become
obvious once the user actually iterates:

1. **Re-compaction is not lossless.** The LLM compaction mode (`/tree <id>
   compact llm`) summarises the raw dropped span from the active branch but
   never threads the prior compaction's `summary` into the prompt. Re‑compacting
   forward in picocode today therefore *discards* the prior summary's content
   from the new summary, even though the prior summary sits in the file. Pi
   avoids this via `previousSummary` injection + the `UPDATE_SUMMARIZATION_PROMPT`
   (`pi/.../compaction.ts:546‑579`, `474‑511`); picocode has no equivalent.

2. **The "leaf is a compaction" guard prevents legitimate re-compaction.** The
   dispatch-layer check at `src/agent.rs:3658‑3668` refuses any compaction when
   the current leaf is itself a compaction entry. This blocks the natural
   iteration flow ("I just compacted; let me re-compact with a better summary")
   unless the user first appends at least one extra turn. There is no analogous
   guard in pi's source for this case in the form that would block the
   non‑no-op variants: pi's `prepareCompaction` only refuses when the dropped
   span is empty (`compaction.ts:688`); the no-op leaf-stacking case is handled
   at the *content* level, not by a flat refusal.

Additionally, pi's `buildContextEntries` emits every compaction entry on the
path as its own `compactionSummary` user message
(`session-manager.ts:400‑402`, `messages.ts:176‑183`). For *iterative*
compaction this means the LLM sees contradictory mid-turn summaries: the latest
compaction's summary at the front, plus every older compaction's summary at its
path position. The older summaries are redundant noise and, in a session where
later iterations superseded earlier ones, actively misleading — the LLM cannot
tell which summary is authoritative.

## Goal

Make compaction iteratively editable in place, with no information loss, no
mid-turn summary noise, and no schema drift vs. pi.

Concretely:

1. **Runtime LLM context never contains nested compaction summaries mid-turn.**
   Only the *latest* compaction on the active branch surfaces as a
   `compactionSummary` user message. Older compactions that sit in the kept
   range are stepped over silently.

2. **LLM compaction mode is lossless across iterations.** When summarising a
   new compaction, the deepest prior compaction on the active branch whose
   `first_kept_entry_id` is at-or-before the new `first_kept_entry_id` has its
   `summary` injected into the summarizer prompt as `previousSummary`. The
   summarizer is then expected to produce a new summary that subsumes the
   previous one (under the user's configured summarization prompt).

3. **The "leaf is a compaction" guard is dropped.** The natural no-op case
   (target_id = predecessor of leaf) is still blocked for LLM mode by the
   existing "nothing before the current position" check at
   `src/agent.rs:3697‑3708`; truncate and text-only modes on an empty dropped
   span produce a legal (if useless) compaction entry, which is acceptable per
   pi's behaviour.

4. **No on-disk schema change.** `SessionEntryContent::Compaction` remains
   byte-compatible with pi. Round-tripping a picocode session through pi
   preserves all compaction entries; pi's `buildContextEntries` will still emit
   the older compaction entries as redundant `compactionSummary` messages at
   their path positions (the same noise pi produces for its own nested
   compactions). The latest compaction is always read correctly because it has
   standard pi shape.

## Final design

### A. `apply_compaction_transforms`: step over nested compaction entries

In `src/session/session.rs:759‑813`, change the kept-range walk so that
**every** `Compaction` entry encountered in `[first_kept_entry_id .. leaf]` is
skipped silently — neither the latest's summary message nor any older nested
compaction's summary message is emitted from the kept range. The latest
compaction's summary still goes at the front of the result (unchanged). The
resulting context shape becomes:

```
[summary message from the latest compaction (if non-empty)]
[entries from first_kept_entry_id (inclusive) up to the leaf]
   — with all SessionEntryContent::Compaction entries removed silently —
```

This **diverges from pi** (which emits each older compaction's summary as its
own `compactionSummary` user message), but loses none of the context: every
older compaction is either superseded by a newer one on the path (in which
case its summary was meant for a boundary that no longer exists) or is itself
the latest (in which case its summary is the one at the front).

#### Edge cases

- **Empty summary on the latest compaction.** The front summary message is
  omitted (existing behaviour at `session.rs:789‑791`); the kept range still
  has all compaction entries stepped over. The result is the raw kept range
  with no summary message at all.
- **No compaction on the path.** Function returns the branch unchanged
  (existing behaviour).
- **`first_kept_entry_id` does not resolve on the branch.** Defensive fallback
  returns the branch unchanged (existing behaviour).

#### Tests

`test_apply_compaction_transforms_repeated_compaction_keeps_older_summary`
(`src/session/session.rs:2091‑2118`) currently asserts the *current* behaviour
("older compaction projects its own summary message"). Rewrite to assert the
new behaviour: older compaction entries in the kept range are silently
dropped; only the latest compaction's summary message is emitted. Add a new
test that covers a triple-nested case (three compactions on the path,
verifying only the deepest's summary reaches the result).

### B. `run_compaction_at_point` LLM arm: inject `previousSummary`

In `src/agent.rs:2959‑3055`, after computing the dropped span (already filtered
to `SessionEntryContent::Message` by `build_summary_prompt`), compute
`previous_summary` from the active branch and inject it into the composed
prompt as a separate section.

#### Selection rule (strict-before)

Walk the active branch root→leaf. For each `SessionEntryContent::Compaction`
entry on the branch:

1. Let `compaction_floor = compaction.first_kept_entry_id`.
2. Find the path-position of `compaction_floor` on the active branch
   (`branch.iter().position(...)`). If not found, this compaction does not
   qualify (defensive fallback).
3. Find the path-position of the new target (`target_id`). If not found,
   `previous_summary = None` and we abort (the caller has already validated
   that `target_id` is on the branch, so this is unreachable in practice).
4. If `compaction_floor_position < target_position` (strictly before), this
   compaction qualifies. Assign `previous_summary = compaction.summary.clone()`.
   Last assignment wins (the *deepest* — closest to the leaf — qualifying
   compaction is kept).

The strict-before rule means:

- A prior compaction whose `first_kept_entry_id` equals the new target is
  **not** merged in — it is being superseded by the new compaction, and its
  summary represents the very boundary the new compaction is rewriting.
  Treating that case as nested (step past it) makes the new compaction's
  summarizer see exactly the prior context that the original compaction at
  that boundary saw.
- A prior compaction whose `first_kept_entry_id` is strictly before the new
  target represents older context that the new compaction is *not* covering
  itself; injecting that summary is the only way to preserve that context
  across the new compaction.

#### Why strict-before, not at-or-before

Strict-before (rather than `<=`) is the principled choice for two reasons:

1. **The at-or-before case is a re-compaction at the same boundary.** The new
   compaction's job is to *replace* the prior summary at that boundary, not
   to merge with it. Injecting the about-to-be-superseded summary tells the
   summarizer "preserve this verbatim" — which produces a new summary that
   is structurally identical to the old one, defeating the point of
   re-compacting. By stepping past it, the summarizer sees the same prior
   context the original compaction saw (which may be empty, or may be an
   earlier compaction's summary).
2. **It matches pi's semantics on the supported case.** Pi only ever
   re-compacts forward (`compaction.ts:642‑655` constrains the cut point to
   `≥ latest.first_keptEntryId` via `boundaryStart`), so the strict-before
   rule is the relevant one whenever both tools agree on what counts as
   "previous." Picocode's `/tree <id> compact` can target an earlier point,
   and the strict-before rule degrades gracefully in that case (step past
   the about-to-be-superseded compaction, look for the next-deepest
   qualifying one).

The strict-before rule is mildly *less* conservative than `<=` for the
exact-point case: when the prior compaction at that boundary had no summary
content worth merging, stepping past it is identical to `<=`. When it had
non-trivial content, stepping past it means the new summarizer is asked to
re-derive that content from whatever strictly-previous context exists. That
re-derivation may produce a different summary text — which is the *intended*
outcome of re-compacting at the same boundary.

#### "Same as the first compaction at that point"

Under strict-before, when the user re-compacts at the exact `first_kept_entry_id`
of a prior compaction, the new summarizer's `previousSummary` is whatever the
*first* compaction at that point saw:

- If there is a strictly-earlier compaction on the branch, its summary is
  injected — matching what the first compaction's summarizer was given as
  `previousSummary` (pi's `previousSummary` is always the latest-on-path
  compaction; under strict-before, "latest" with the at-the-boundary one
  excluded is exactly the next-deepest qualifying one).
- If there is no strictly-earlier compaction, `previous_summary = None` —
  matching the first compaction at that point (which also had no prior
  context to merge).

This is the rule the design targets.

#### Prompt assembly

The composed prompt becomes:

```
{user_prompt}

--- BEGIN DROPPED SPAN ---
{serialised dropped span (Message entries only)}
--- END DROPPED SPAN ---

--- PREVIOUS SUMMARY ---        # only when previous_summary is non-empty
{prior compaction's summary text}
--- END PREVIOUS SUMMARY ---
```

The user-controlled `user_prompt` remains the primary instruction. If the user
wants strict pi parity (the `UPDATE_SUMMARIZATION_PROMPT` template's
"PRESERVE all existing information from the previous summary" rules), they
can include that text in the trailing payload (`llm_payload`) or in the
`llm_compact_prompt` setting. The default `llm_compact_prompt` should be
updated to mention the `--- PREVIOUS SUMMARY ---` section so users get the
merge behaviour by default.

#### Tests

Add tests covering:

- A branch with one prior compaction and a new compaction at a strictly later
  target: `previous_summary` = the prior compaction's `summary`.
- A branch with one prior compaction and a new compaction at the prior's
  exact `first_kept_entry_id`: `previous_summary` is **not** the prior's
  summary (the prior is being superseded); it falls back to whatever
  strictly-earlier context exists on the branch, or `None` if none.
- A branch with no prior compaction: `previous_summary` is `None`; the prompt
  has no `--- PREVIOUS SUMMARY ---` section.
- A branch with two nested prior compactions and a new compaction strictly
  after both: `previous_summary` = the deeper (later, closest to leaf)
  qualifying compaction's `summary`.
- A branch with two prior compactions, the deeper one's `first_kept_entry_id`
  being *at-or-after* the new target: `previous_summary` = the shallower
  qualifying one (whose `first_kept_entry_id` < new target, per the
  strict-before rule).

### C. Drop the leaf-is-compaction guard

Remove the block at `src/agent.rs:3658‑3668`:

```rust
if let Some(id) = &old_leaf_id {
    if let Some(entry) = s.get_entry(id).await {
        if matches!(
            entry.content,
            crate::session::SessionEntryContent::Compaction { .. }
        ) {
            config.output.display_text("Cannot compact: the session was already compacted at this point");
            continue;
        }
    }
}
```

The existing LLM-mode guard at `src/agent.rs:3697‑3708`
("`predecessor(target_id) == leaf`" → "nothing before the current position")
covers the only case where the guard's removal introduces user-visible
incorrectness: a stacked compaction with no intervening turns, which would
otherwise write a compaction entry with an empty dropped span. The LLM mode
correctly refuses; truncate and text-only still write a (harmless) compaction
entry whose `first_kept_entry_id` equals the leaf's structure. This matches
pi's behaviour (`compaction.ts:688` returns undefined only when the dropped
span is empty; otherwise the write proceeds).

#### Tests

- Existing "nothing before the current position" test continues to pass.
- Add a new test: branch ending in a `Compaction` entry with `first_kept_entry_id`
  set to a real message elsewhere on the branch; `/tree <that-message> compact
  truncate` succeeds and writes a new compaction entry.

### D. Documentation updates

- **`doc/tree_compaction.md`** §3 (Context scoping): rewrite the paragraph
  describing how older nested compactions are handled. New wording: "older
  nested compaction entries are stepped over silently; only the latest
  compaction's summary appears as a `compactionSummary` user message at the
  front of the context." Note the deliberate divergence from pi.
- **`doc/tree_compaction.md`** §4 (Repeated compactions): rewrite to describe
  the `previousSummary` selection rule. Note that picocode uses the
  user-configured summarization prompt as the primary instruction; the
  `--- PREVIOUS SUMMARY ---` section is injected unconditionally when a
  qualifying prior compaction exists. Note this matches pi's `previousSummary`
  semantics with a different prompt-assembly strategy (pi switches to
  `UPDATE_SUMMARIZATION_PROMPT`; picocode uses a generic user prompt).
- **`doc/tree_compaction.md`** §4.2 (Leaf guard): replace the "leaf-is-compaction
  guard" paragraph with a note explaining why it's been dropped. Reference the
  remaining LLM-mode "nothing before" guard for the no-op stacked case.
- **Settings doc** for `llm_compact_prompt`: update the default to mention
  the `--- PREVIOUS SUMMARY ---` section so users get merge behaviour by
  default. Suggested text:

  > "Summarise the conversation so far in less than 2000 words. Preserve the
  > key technical decisions, file paths touched, current task state, and any
  > unresolved questions. When a `--- PREVIOUS SUMMARY ---` section is
  > present, treat it as the existing context checkpoint and merge new
  > information into it rather than replacing it."

## Cross-tool fidelity

### What a pi user sees when loading a picocode session

- The latest compaction entry has standard pi shape (no schema change).
  Pi's `buildContextEntries` (`session-manager.ts:414‑449`) will pick it up
  as the active boundary and emit its `summary` as the first
  `compactionSummary` user message via `sessionEntryToContextMessages`
  (`session-manager.ts:400‑402`) → `convertToLlm` (`messages.ts:176‑183`).
  **Identical to pi-written compactions.**
- The kept-range messages are emitted in order by `sessionEntryToContextMessages`.
  **Identical to pi-written compactions.**
- Any older compaction entries that sit in the kept range (because the user
  iterated in picocode) are emitted by pi as redundant `compactionSummary`
  user messages at their path positions — exactly the noise pi itself
  produces for its own nested compactions, since pi never writes nested
  compactions in its auto-compaction loop but does not refuse them when
  loaded. **Identical cross-tool behaviour to pure-pi nested-compaction
  sessions.**
- If pi then auto-compacts further, pi's `prepareCompaction`
  (`compaction.ts:633‑712`) picks the latest compaction as `prevCompaction`
  and uses its `first_kept_entry_id` as `boundaryStart`. Older nested
  compactions contribute nothing to `messagesToSummarize`
  (`getMessageFromEntryForCompaction` returns `undefined` for any compaction
  entry, `compaction.ts:79‑84`). Pi then injects `previousSummary = latest
  compaction's summary` into the new summarization prompt and produces a
  merged summary. **Identical to pi-written nested compaction auto-compaction
  behaviour.**

The cross-tool fidelity story is therefore: latest compaction's content
reaches pi users intact; the cross-tool noise level (older compaction
summaries emitted as redundant mid-turn messages) is the same as it would
be for any nested-compaction session, regardless of which tool wrote it.

## What this does NOT do

- **No new schema fields.** `SessionEntryContent::Compaction` keeps
  `summary / first_kept_entry_id / tokens_before`.
- **No storage-layer extensions.** All changes are in
  `apply_compaction_transforms`, `run_compaction_at_point`, and the
  removed guard.
- **No behaviour change for sessions without nested compactions.** A single
  compaction produces the same `apply_compaction_transforms` output as before
  (latest summary at front, raw messages after, no compaction entries in
  between because there are no compaction entries to skip).
- **No behaviour change for the on-disk format.** Round-trip with pi is
  unchanged.
- **No re-summarisation tool beyond compaction.** A dedicated `/tree
  <compaction-id> edit-summary` command is a natural follow-up (Option A in
  the design discussion) but is out of scope for this change. The user's
  primary tool for fixing a bad summary is "re-compact with the LLM mode and
  a customised `llm_payload`."

## Open questions

- The strict-before rule means re-compacting at an exact prior boundary does
  *not* inject the about-to-be-superseded summary, but instead falls through
  to whatever strictly-earlier context exists. That is the principled default.
  An opt-in "merge with the prior summary at this boundary" flag would be a
  one-line addition to the rule (`≤` instead of `<`) and could be exposed as
  a `--merge-with-prior` flag on `/tree <id> compact llm` if users want it.
  Default strict-before; opt-in merge is low-value until someone asks.
- Should we surface a "compaction iteration" UI affordance (e.g. show the
  current `previousSummary` in the `compact llm` sub-menu header so the user
  knows what will be merged)? Out of scope here; the header already shows
  dropped/kept counts and `tokensBefore`.
