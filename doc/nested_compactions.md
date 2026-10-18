# Nested compactions

## Status

Phases implemented:

- [x] **A. Step over nested compaction entries in runtime LLM context.**
      `apply_compaction_transforms` (`src/session/session.rs`) now drops
      every `SessionEntryContent::Compaction` entry encountered in the kept
      range. Only the *latest* compaction's `summary` is emitted at the
      front. Older nested compactions are invisible to the runtime LLM.
- [x] **B. Preserve prior context in LLM compaction via the leaf dance.**
      The `compact llm` summarization runs with the session moved to the
      entry just before the target, and the request prefix is built by the
      SAME `session_context_messages` helper a normal prompt uses — so a
      prior compaction on that prefix re-activates its `<summary>` wrapper
      from the tree structure (no textual `previousSummary` injection;
      see below). This also restores the provider prefix-cache property.
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
2. **LLM compaction mode should be lossless across iterations and cache-
   friendly.** Re-compacting forward must preserve the prior summary's
   content and, ideally, share the provider prefix-cache with the live
   turns. The leaf dance achieves both: the summarizer runs at the
   predecessor with the same context a prompt there would send, which
   re-activates the prior compaction's `<summary>` wrapper (lossless) and
   sends identical leading bytes (cache hit).
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

### B. `previousSummary` injection in LLM compaction mode (superseded by the leaf dance)

**The original textual injection is gone.** The leaf dance (move to the
predecessor, summarize with the live prefix, restore) now makes the
`previousSummary` semantics **emergent from the tree structure**:

- `generate_compaction` (editor cleanup E3; was `run_compaction_at_point`)
  moves the session leaf to the entry strictly
  before `target_id`, then calls `summarize_with_session_context`, which
  builds the request prefix via `session_context_messages` — the SAME
  helper (`Session::build_context()` + `maybe_strip_reasoning`) a normal
  user prompt uses. `apply_compaction_transforms` then re-activates the
  latest compaction on that root→predecessor prefix as the leading
  `<summary>` wrapper.
- A prior compaction whose `first_kept_entry_id` is strictly before the
  target is an **ancestor** of the predecessor, so its summary lands in the
  context as the loader wrapper — preserving it across the new compaction
  (the phase-B merge behaviour, now structural).
- A prior compaction AT the target's own boundary is a **descendant** of the
  predecessor, NOT an ancestor, so it is excluded — the new summary
  re-derives its content from the raw kept messages, exactly what the
  original compaction at that point saw (the strict-before supersede rule).

This also fixes the provider **prefix-cache property**: the summarization
request sends the same leading context bytes as a prompt at that position,
so the cached prefix (warmed by live turns) hits instead of re-tokenising a
flattened transcript (`doc/provider_caching.md`).

The two phase-B helpers (`find_previous_summary_on_active_branch`,
`compose_compaction_llm_prompt`) and the `--- PREVIOUS SUMMARY ---` prose in
`DEFAULT_LLM_COMPACT_PROMPT` were removed; the context-cache prefix makes
them redundant.

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
- **TextOnly**: `build_text_only_summary` scopes its input to
  `entries_since_last_compaction` (the same kept range the pivoted runtime
  context uses), so it starts from the previous summary and only renders
  `Message` entries after that boundary. Nested compaction entries are
  skipped, so a superseded summary never leaks into the new text-only summary.
- **Llm**: unaffected by the guard removal; still errors on an empty dropped
  span and (Phase B) injects `previousSummary` into the summarizer prompt.

`previousSummary` injection (Phase B) applies only to the **LLM** mode;
text-only instead seeds its deterministic draft with the previous summary and
scopes the raw `Message` entries to the same compaction boundary. Truncate
still writes no summary.

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
