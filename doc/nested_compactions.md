# Nested compactions

## Status

Phases implemented:

- [x] **A. Step over nested compaction entries in runtime LLM context.**
      `apply_compaction_transforms` (`src/session/session.rs`) now drops
      every `SessionEntryContent::Compaction` entry encountered in the kept
      range. Only the *latest* compaction's `summary` is emitted at the
      front. Older nested compactions are invisible to the runtime LLM.
- [x] **B. Inject `previousSummary` into the LLM compaction summarizer.**
      `run_compaction_at_point` LLM arm. Selection rule: strict-before
      (deepest prior compaction on the active branch whose
      `first_kept_entry_id` is strictly before the new target). Done via
      `find_previous_summary_on_active_branch`;
      `compose_compaction_llm_prompt` appends the `--- PREVIOUS SUMMARY ---`
      section (omitted when none/empty). Default `llm_compact_prompt` now
      instructs the model to merge it.
- [x] **C. Drop the leaf-is-compaction guard.** `src/agent.rs`. The dispatch
      block is removed; the LLM-mode "nothing before" guard still covers
      the empty-dropped-span no-op case. Multi-mode interop verified:
      truncate / text-only / llm all re-compact correctly over a session
      whose leaf is already a compaction.
- [x] **D. Update `doc/tree_compaction.md`** to describe the new semantics
      and update the default `llm_compact_prompt`.

No schema change: `SessionEntryContent::Compaction` keeps its
pi-compatible `{ summary, first_kept_entry_id, tokens_before }` shape
(`src/session/types.rs:204`). All changes are interpretation- and
prompt-assembly-level.

## Why

Pi's compaction protocol is one‑way: the latest compaction on the path
defines the LLM context floor, and that floor only moves *backwards*. Once
a compaction is written, there is no protocol for revising its summary —
except to write a *new* compaction that supersedes it. Iterative
compaction needs three things today:

1. **Runtime context should not contain nested compaction summaries
   mid-turn.** Older compactions represent superseded boundaries; their
   summary text mid-turn is redundant noise and, when later iterations
   supersede earlier ones, actively misleading. The LLM cannot tell which
   summary is authoritative.
2. **LLM compaction mode should be lossless across iterations.** Today
   picocode's `run_compaction_at_point` LLM arm summarises the raw dropped
   span but never threads the prior compaction's `summary` into the
   prompt, so re-compacting forward discards the prior summary's content.
   Pi avoids this via `previousSummary` injection (`compaction.ts:546‑579`).
3. **The "leaf is a compaction" guard blocks legitimate re-compaction.**
   Pi refuses only when the dropped span is empty (`compaction.ts:688`),
   not when the leaf is a compaction. Picocode should match.

## Design summary

### A. Step over nested compactions in runtime context

Done. `apply_compaction_transforms` walks `[first_kept_entry_id .. leaf]`
and drops every `SessionEntryContent::Compaction` entry silently. The
latest compaction's `summary` still appears as the first
`compactionSummary` user message at the front (when non-empty).

This **diverges from pi**, which would emit each older compaction's
summary as its own `compactionSummary` user message. Pi users loading a
picocode-iterated session will see the older compaction summaries re-emit
at their path positions — the same noise pi itself produces for its own
nested-compaction sessions.

### B. `previousSummary` injection in LLM compaction mode (done)

Implemented as two functions in `src/agent.rs`:

- `find_previous_summary_on_active_branch(index, target_id) -> Option<String>`:
  walks the active branch root→leaf and returns the deepest
  `SessionEntryContent::Compaction`'s `summary` whose `first_kept_entry_id`
  position is **strictly before** the new target's position. Last
  assignment wins while scanning root→leaf, so the deepest (closest to
  target) qualifying compaction is kept. Returns the empty string when a
  qualifying compaction has an empty summary (the caller filters it out).
- `compose_compaction_llm_prompt(user_prompt, dropped_span, previous_summary)`:
  appends the `--- PREVIOUS SUMMARY ---` section after the dropped span
  when `previous_summary` is non-empty; omits it otherwise.

Selection semantics (strict-before):

- A prior compaction whose `first_kept_entry_id` equals the new target is
  being superseded by the new compaction; it is not merged in. The new
  summarizer sees whatever strictly-previous context exists on the
  branch — i.e. exactly what the original compaction at that boundary saw.
- A prior compaction whose `first_kept_entry_id` is strictly before the
  target represents older context the new compaction is not covering
  itself; injecting it preserves that context across the new compaction.

Composed prompt (in LLM mode only):

```
{user_prompt}

--- BEGIN DROPPED SPAN ---
{serialised dropped span (Message entries only)}
--- END DROPPED SPAN ---

--- PREVIOUS SUMMARY ---        # only when previous_summary is non-empty
{prior compaction's summary text}
--- END PREVIOUS SUMMARY ---
```

The user-controlled `user_prompt` is the primary instruction. The default
`llm_compact_prompt` now mentions the `--- PREVIOUS SUMMARY ---` section so
users get merge behaviour by default.

### C. Drop the leaf-is-compaction guard (done)

The dispatch block at the old `src/agent.rs:3658‑3668` is removed: compacting a
session whose leaf is already a `Compaction` entry is now allowed. The existing
LLM-mode guard at `src/agent.rs:3697‑3708` ("nothing before the current
position") still covers the no-op stacked case (empty dropped span). Truncate
and text-only on an empty dropped span write a (harmless) compaction entry
whose `first_kept_entry_id` equals the leaf's structure — matching pi
(`compaction.ts:688` refuses only when the dropped span is empty).

**Multi-mode interop** (verified by tests):

- **Truncate**: appends an empty-summary compaction; never examines the
  dropped span, so nested compaction entries are irrelevant. Works when the
  leaf is already a compaction.
- **TextOnly**: `build_text_only_summary` renders only `Message` entries and
  skips `Compaction` entries in the dropped span (session.rs), so a nested
  compaction's summary text never leaks into a new text-only summary.
- **Llm**: unaffected by the guard removal; still errors on an empty dropped
  span and (Phase B) injects `previousSummary` into the summarizer prompt.

`previousSummary` injection (Phase B) applies only to the **LLM** mode.
Truncate and text-only are deterministic / non-generative: truncate writes no
summary, and text-only re-derives its summary from the raw `Message` entries
in the dropped span (which still physically exist in the session file), so
neither needs to thread a prior compaction's summary forward.

### D. Documentation updates (done)

- **`doc/tree_compaction.md`** §4 (Repeated compactions) rewritten to describe
  the new semantics (step over nested compactions; `previousSummary`
  injection; guard removal). The Phase 1 code sketch and edge-case tables
  carry `NOTE (superseded)` markers pointing here.
- **`settings.toml.example`** and the built-in default `llm_compact_prompt`
  now mention the `--- PREVIOUS SUMMARY ---` section.

## Cross-tool fidelity

When a picocode-iterated session is loaded by pi:

- The latest compaction entry has standard pi shape (no schema change).
  Pi's `buildContextEntries` (`session-manager.ts:414‑449`) emits its
  `summary` as the first `compactionSummary` user message — identical to
  pi-written compactions.
- Kept-range messages reach pi's LLM context in order — identical.
- Older nested compaction entries are emitted by pi as redundant
  `compactionSummary` user messages at their path positions, because pi
  does not know they were superseded. This is the same noise pi produces
  for its own nested compactions on load. **Not a regression.**
- When pi next auto-compacts, its `prepareCompaction`
  (`compaction.ts:633‑712`) treats the latest compaction as
  `prevCompaction`, uses its `first_kept_entry_id` as `boundaryStart`,
  and threads its `summary` into the new summarization prompt as
  `previousSummary`. Older nested compactions contribute nothing to
  `messagesToSummarize` (`getMessageFromEntryForCompaction` returns
  `undefined` for compaction entries, `compaction.ts:79‑84`). **Identical
  to pi-written nested compaction auto-compaction behaviour.**

The cross-tool fidelity story is therefore: latest compaction's content
reaches pi users intact; cross-tool noise (older compaction summaries
re-emitted mid-turn) matches what pi itself produces for nested
compactions regardless of authoring tool.
