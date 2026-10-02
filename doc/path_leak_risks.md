# Host-path leak risks

*Scope: which of picocode's two path spaces may be shown to the model. Pairs
with `doc/tool_error_handling.md`, which covers how tool errors are classified
and displayed.*

## The two path spaces

A path in picocode exists in two forms, and which one is correct depends on
**who performs the I/O** — the process boundary, not the tool:

| | path space | used by |
|---|---|---|
| **host** | after remapping, what this process opens | `read`/`write`/`edit`, `glob`, `list` (in-process IO) |
| **in-sandbox** | what the bwrap namespace sees | everything handed to a child: `sed`→`bsed`, `grep`→`rg`, `bash` |

With no remapping binding the two coincide, which is why these bugs survive
every test that clears the sandbox. They diverge only under a remap, e.g.
`rw = [{ from = "/home/me/tmp/agents", to = "/tmp" }]`, where the model's
`/tmp/f.txt` resolves to the host path `/home/me/tmp/agents/f.txt`.

## The rule

> **Echo the path the caller supplied. Never format a host path into a string
> a model can read.**

The path the model gave you is already an in-sandbox path, so it is correct by
construction — no rewriting, no remap-table inversion. The host path is data
for code that has an operator-facing sink; it is not prose.

## Fixed

1. **`sed` / `grep` handed the child a host path** (commit `d66bee2`,
   introduced with `sed` in `7fd05cb`). Both resolved the model's path with
   `get_path`/`get_path_with` — host — and passed it to a child running inside
   the namespace, where that path does not exist. The model's file was plainly
   there; the tool reported it as missing. Now: bounds-check on the host path,
   hand over `get_sandbox_path`'s result.

   Symptom split worth remembering: `grep` returns child stderr verbatim, so it
   said `rg: /home/me/…: No such file or directory`; `sed` maps a non-zero exit
   to `Err`, which rig redacts to `"the tool failed"`. Same bug, opposite
   visibility — which is why it surfaced on `sed` and was invisible on `grep`.

2. **`PathPolicy::check` baked `host_path.display()` into its message.** That is
   the denial a model hits most often for a mistyped path, and the most likely
   place to hand back a path the model cannot name. `check` now returns
   `PolicyDenied { kind, host_path }` — reason and path as separate data — and
   the caller renders `PolicyDenied::model_message(supplied)`.

   Nothing was reaching an operator before this change either: a `ToolError`
   becomes a `ToolExecutionError` whose `model_output` is redacted to the kind
   default, and `on_tool_result` displays `raw_result.output()`, i.e. *the same
   redacted text the model sees*. There is currently no separate operator
   channel for tool errors.

3. **`edit` stopped being one of the redaction casualties**
   (`doc/edits_readd_and_arg_errors.md`). It is still registered directly rather
   than through `Guard`, but it now overrides `map_error` and shares one
   classifier with `Guard` (`tools.rs::map_tool_error`), so its own diagnostics
   reach the model instead of the `Other` default. This is what forced the
   change: re-accepting the `edits` array moved its failures from the argument
   layer — where the `Expected:`/`Actual:` envelope applies — to the call layer,
   where they would all have arrived as `the tool failed`. Its IO errors name
   the file **as the model spelled it**, and
   `no_edit_failure_shows_the_model_a_host_path` runs the tool under a remapping
   policy and asserts no host path appears in any of them.

## Open

- **Redaction/guard gap, six tools remaining.** `sed`, `read`, `write`,
  `glob`, `grep` and `list` are still registered without `map_error`, so rig's
  default → `redact_model_feedback()` replaces the model-visible text with
  `"the tool failed"` (the real message survives only in `error.source`). `edit`
  was closed first because it was the one whose errors the model must act on;
  the rest are the same one-line change plus their own leak test, and none of
  them can be closed before step 1 below.
- **No operator channel.** Because `on_tool_result` renders `output()`, the
  operator sees exactly what the model sees. `PolicyDenied::operator_message()`
  exists but has nowhere to write: no `tracing` subscriber is installed, so
  `tracing::warn!` is a no-op.
- **No guard test, except for `edit`.** `no_edit_failure_shows_the_model_a_host_path`
  runs the tool under a remapping policy and asserts no host path reaches the
  model, so the rule above is enforced rather than coincidental for that tool.
  Nothing fails yet if someone writes `format!("cannot read {p}")` with a host
  `p` in one of the other six — and for those six the messages are still
  redacted, which is why their tests would pass vacuously. A leak test that
  asserts on a *redacted* message is not a leak test.

## When the guard comes off

Unredacting tool errors is safe **only** if the messages are host-path-free.
Order of operations:

1. Split `on_tool_result`: operator gets the full diagnostic, model gets
   `model_output`. Wire a real sink for `operator_message()`.
2. Pass through messages picocode constructs (they are clean under the rule
   above); keep redacting strings from sources we cannot vouch for.
3. Add the guard test: under a remapping policy, trigger each constructable
   error and assert the model-visible text contains no host-path token.

Steps 2 and 3 together convert "happens to be clean" into "fails CI if it
stops". Step 1 must land first, or the operator loses the host path entirely.
