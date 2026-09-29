# B3 — one separator rule, one render path

**Status:** plan for approval — nothing implemented yet
**Part of:** `doc/row_accounting.md` §6, phase B (the row-counting phase). A1 (the
composition) waits on this: the blank line a block gets above it is a row, and
the frame's placement is decided against the block boundary, so a separator
rule that moves under the composition makes the composition wrong in a way
that is hard to see.

Four sub-commits, each independently revertible, in this order. The first is a
characterization suite: it changes no behavior and its job is to *tell us what
the current behavior is* before anything moves it.

---

## 1. What is there now

**The separator state** is three booleans in three places:

| State | Set by | Read by |
|---|---|---|
| `last_blank` (`set_blank` / `ended_blank`) | ~12 call sites | every block's leading-blank decision |
| `last_was_tool` (`note_tool` / `note_text`) | `paint_panel`, the streaming boundary | `stream_text` (blank after a tool), `display_tool_call` (blank between tool calls) |
| `dim_on` (`ensure_dim`) | the thinking boundary | the thinking boundary |

So "does this block need a blank line above it?" is answered from partial
state, differently at each of ~12 sites. The subtle part is the *ordering*:
the thinking→answer blank must be written **after** the dim reset SGR or a GFM
list swallows it as the block's closing newline (`flush_held_answer`'s comment).
That is the class of bug that only shows in one render path — the live one.

**The commit dividers** are, as of the editor cleanups (E5), dead: `emit_commit`
takes `sep: bool` and both call sites pass `false`. So half of "the commit
dividers" is a deleted parameter waiting to happen, plus a stale comment about
`/set` value commits that no longer take that path.

**The hold-back branch** is disabled but present. `agent.rs:311` is
`// self.output.set_answer_hold(reasoning_expected);` — so the agent never
enables it, and the machinery is unreachable: `Output::set_answer_hold`,
`answer_held`, `answer_buffer`, `flush_held_answer` and its three call sites,
the dead `_reasoning_expected` computation, and the
`late_reasoning_answer_is_withheld_until_turn_end` test (which tests a path
nothing takes any more).

**The live/replay branch** is `ConsoleOutput::live`, set from
`stdout().is_terminal()`. When false: `stream_text` appends to `text_buffer`,
`stream_thinking` writes to stderr, and `end_response` renders the whole
buffer as one markdown block (`flush_text`). When true: the incremental
parser + renderer. Replay in a test runs with a non-tty stdout, so **every
replay test exercises the buffered path and none of them exercise the
incremental renderer** — which is where this morning's spacing bug lived. The
new screen-truth tests had to force `begin_response_inner(true)` to get around
it.

---

## 2. The commits

### B3a — characterize, change nothing

A matrix of block sequences driven through the **live** render path, capturing
bytes and asserting structure (blank-line runs, rule placement, panel
boundaries) rather than exact bytes. Axes:

- **Sequence**: reasoning→answer · answer→tool→answer · tool→tool ·
  text→text · text→tool · error-after-content · reasoning→answer→tool→answer
  (two segments) · message after a committed command · empty final segment.
- **Chunking**: each text sequence run three ways — one call per block, byte at
  a time, and split mid-word. The chunking axis is the point: `stream_text` is
  called with deltas live and with whole blocks in replay, and any rule that
  depends on *how often* it is called is a bug waiting for a chunk boundary to
  land differently.
- **Live vs replay**: for the same turn, `render_turn` and the live event
  sequence must produce identical bytes. Today they go through different
  branches, so this test is expected to fail at first and is the measurement
  that says how much is at stake.

Whatever fails here is *today's* behavior. It gets reported before it gets
changed, and the failing cases decide how much of B3c is a refactor versus a
bug fix.

### B3b — delete the hold-back branch

Pure deletion, no behavior change (it is already disabled): the trait method and
its impls, the two fields, `flush_held_answer` and its call sites, the dead
computation in `agent.rs`, the obsolete test. The `flushed` flag that threads
through `end_response_segment` / `end_response` disappears with it, which
simplifies those two bodies.

### B3c — one render path

`stream_text` and `end_response` lose the `live` branch: the incremental
parser + renderer always. `text_buffer` and `flush_text` go. What this buys
beyond the test surface: replay and live become the same code, so the matrix's
live-vs-replay equality is achievable, and the `begin_response_inner(true)`
workaround in the row-accounting tests disappears.

Consequences to accept, stated plainly:

- **Piped stdout changes shape.** `picocode "q" | cat` today emits one
  markdown block at the end; after this it streams block by block. The text is
  the same; the framing is not. That is the intent (one path, no buffered
  special case), and it belongs in the release notes.
- **The stderr branch for reasoning is a separate question** (see §4).

### B3d — one separator rule

```rust
/// What kind of block is being written. The separator rule is a function of
/// this and of what came before — nothing else.
pub enum BlockKind { Reasoning, Text, Message, ToolCall, ToolResult, Error, System, Commit }

/// What the last write left behind: its kind, and whether it ended with a
/// blank line. One mutex, replacing `last_blank` + `last_was_tool` + `dim_on`'s
/// layout half.
struct BlockState { last: Option<BlockKind>, ended_blank: bool }

impl ConsoleOutput {
    /// Emit whatever separates `next` from what came before, and record it.
    /// Every block writer calls this first; none of them decides anything.
    fn begin_block(&self, next: BlockKind);
}
```

The rules, each one a line of today's behavior that the matrix pins:

| After | Before a `Reasoning` | before a `Text`/`Message` | before `ToolCall`/`ToolResult` |
|---|---|---|---|
| anything | none | blank, unless the last write already ended blank | blank |
| a tool block | — | blank | blank |
| a block that ended blank | none | none | none |

`dim_on` keeps its own field — it is the *style* state, and the dim reset has
to be written after the boundary blank. The ordering moves into `begin_block`,
where it is visible and testable, rather than being a comment in one call site.

`set_blank` / `ended_blank` / `note_text` / `note_tool` / `last_was_tool` /
`last_blank` all go. The ─ rules (`display_separator`) are *not* governed by
the rule: they are explicit structural separators (the prompt/response
boundary in `render_turn`, the per-prompt framing in `agent.rs`), and keeping
them explicit is what stops "does this need a blank" and "does this need a
rule" from blurring into each other the way they have.

### B3e — the commit dividers

Delete `emit_commit`'s `sep` parameter and its stale `/set` comment; the two
call sites agree on `false` today, and the frame's own input divider is what
separates a committed line from the next prompt. A `Commit` block kind in the
rule (so a commit is a first-class block) but no automatic separator: the
frame cleared the input region, and the rule below the input belongs to the
frame, not to the content.

---

## 3. What each commit is gated on

| Commit | Gate |
|---|---|
| B3a | the matrix exists and its failures are reported (not fixed) |
| B3b | full suite green; no behavior change to assert |
| B3c | full suite green; the row-accounting tests no longer need `begin_response_inner(true)`; live-vs-replay equality is the new gate |
| B3d | the matrix, unchanged, still green — the rule has to reproduce today's structure exactly, including the cases that were already odd |
| B3e | full suite green; the two `emit_commit` sites unchanged in behavior |

## 4. Decisions I need

1. **Reasoning off a tty.** `stream_thinking` writes to stderr when stdout is
   not a terminal, so a consumer of `picocode "q" | cat` can drop it. Removing
   the `live` branch would put reasoning on stdout through the renderer. My
   recommendation: keep that one branch — it is a *stream-selection* decision,
   not a render path, and it protects piped stdout — and remove only the
   text-buffering branch. The alternative (reasoning joins stdout everywhere)
   is more consistent and costs a consumer-visible change.
2. **Is B3c's piped-output framing change acceptable?** It follows from one
   render path, but it is user-visible for anyone parsing the output.
3. **B3d's blast radius.** If the matrix shows the current behavior is already
   inconsistent for some combination (a plausible outcome, given the bug found
   this morning), do I (a) encode today's inconsistency in the rule and fix it
   later, or (b) fix it in B3d and say so in the diff? I lean (b) — a rule that
   reproduces a bug is a rule that will be read as intentional — but it makes
   B3d a behavior change and not a refactor, which affects how it reviews.

## 5. Risks

- The matrix itself is the main cost: this area is subtle and under-tested, and
  a bad matrix is worse than none. Mitigation: assert *structure* (blank runs,
  rule placement), not byte equality, except for the one live-vs-replay case
  where byte equality is the point.
- B3c removes a path that some consumer may depend on (the buffered shape for
  non-tty stdout). If that turns out to matter, the buffering comes back as an
  `Output` wrapper rather than as a branch inside the renderer.
- Ordering within the phase matters: B3d before B3e, and B3a before both. If
  the matrix shows the current behavior is more tangled than expected, stopping
  after B3c is a coherent place to stop — B3a–B3c are self-contained and A1 can
  proceed on top of them.
