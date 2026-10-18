//! The canonical transcript of an in-flight run, and the message shaping that
//! turns raw rig stream events into the messages that get persisted and sent.
//!
//! `TurnTranscript` accumulates `[assistant, user(tool-results), …]` exactly
//! the way rig's `new_messages` does, so a mid-turn failure can persist what
//! actually completed. `finalize` closes out an interrupted trailing batch with
//! synthesized "did not execute" tool results — the same shape rig answers
//! rejected calls with.
//!
//! The rest is per-message surgery on the canonical list: stripping reasoning
//! that shouldn't reach context, splitting multi-file `edit` calls into the
//! per-file sub-calls a weak model can self-correct from, normalizing tool-call
//! ids, and shortening the model-facing text of a malformed `edit` result. All
//! of it is pure on a `Vec<Message>` (or a `(provider, model)` pair) — no
//! session, no output.

use rig_core::message::{
    AssistantContent, Message, ReasoningContent, Text, ToolCallId, ToolFunction, ToolResult,
    ToolResultContent, UserContent,
};
use std::collections::HashMap;

use blockedit::EditOp;

/// Canonical transcript of the in-flight run, mirroring rig's `new_messages`
/// shape: `[assistant, user(tool-results), assistant, …]` (the user prompt is
/// added at persist time by `prompt()`, which owns `input`). picocode
/// accumulates this from the stream events so a mid-turn failure can persist
/// exactly what completed — and nothing more — before the REPL retries.
///
/// * [`LoggingHook::on_model_turn_finished`] appends each *accepted* assistant
///   turn (`event.content` is the canonical, provider-normalized
///   `AssistantContent` rig parked for the turn — the same content a
///   successful run persists via `FinalResponse.messages()`).
/// * Tool-result stream items (`StreamUserItem(ToolResult)`) are batched into
///   one `Message::User` and flushed on the next assistant turn or at
///   [`finalize`](Self::finalize) — matching rig, which pushes each completed
///   batch as a single `Message::User`.
///
/// [`finalize`](Self::finalize) returns a canonical transcript for
/// persistence: a trailing assistant turn whose tool calls were never answered
/// (the run was interrupted mid-batch) is *completed* with synthesized text
/// tool results — one per unanswered call, marked "did not execute" — so
/// nothing is dropped or truncated and rig's `validate_canonical` passes.
/// This mirrors rig's own `invalid_tool_retry_user_message`
/// (`rig-run/src/transcript.rs`), which answers rejected calls with text
/// results rather than removing them.
#[derive(Default)]
pub(crate) struct TurnTranscript {
    /// Completed messages in canonical order.
    committed: Vec<Message>,
    /// Accepted assistant turns. `> 0` means the run got past the first model
    /// call — i.e. there is progress worth persisting on a mid-turn failure.
    assistant_turns: usize,
    /// Tool results of the in-flight batch (following an assistant tool
    /// turn); flushed as one `Message::User` on the next assistant turn or at
    /// finalize.
    pending_results: Vec<UserContent>,
}

impl TurnTranscript {
    /// Append an accepted assistant turn. Any pending tool-result batch is
    /// flushed first so the transcript stays canonical (assistant → user
    /// results → …). Turns with no visible content (rig curates empty turns
    /// out of history) are skipped.
    pub(crate) fn push_assistant_turn(
        &mut self,
        id: Option<String>,
        content: Vec<AssistantContent>,
    ) {
        self.flush_results();
        if !assistant_content_is_visible(&content) {
            return;
        }
        self.committed.push(Message::Assistant { id, content });
        self.assistant_turns += 1;
    }

    /// Accumulate one tool result of the in-flight batch.
    pub(crate) fn push_tool_result(&mut self, result: rig_core::message::ToolResult) {
        self.pending_results.push(UserContent::ToolResult(result));
    }

    /// Whether any model turn completed (progress worth persisting).
    pub(crate) fn has_progress(&self) -> bool {
        self.assistant_turns > 0
    }

    /// The canonical transcript to persist on interruption. Flushes any
    /// pending tool-result batch, then — if the transcript still ends on an
    /// assistant turn with tool calls that never ran — answers each call with
    /// a synthesized text result. Never truncates or drops progress.
    pub(crate) fn finalize(&mut self) -> Vec<Message> {
        self.flush_results();
        let needs_synthesis = matches!(
            self.committed.last(),
            Some(Message::Assistant { content, .. })
                if content.iter().any(|c| matches!(c, AssistantContent::ToolCall(_)))
        );
        if needs_synthesis {
            let calls: Vec<&rig_core::message::ToolCall> = match self.committed.last() {
                Some(Message::Assistant { content, .. }) => content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::ToolCall(tc) => Some(tc),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let synthesized: Vec<UserContent> = calls
                .iter()
                .map(|tc| {
                    UserContent::ToolResult(rig_core::message::ToolResult {
                        call: tc.id.clone(),
                        provider: tc.provider.clone(),
                        name: tc.function.name.clone(),
                        content: vec![ToolResultContent::text(format!(
                            "[tool call `{}` was interrupted before execution; \
                             it did not run. Continue from the conversation above.]",
                            tc.function.name
                        ))],
                    })
                })
                .collect();
            if !synthesized.is_empty() {
                self.committed.push(Message::User {
                    content: synthesized,
                });
            }
        }
        self.committed.clone()
    }

    /// Flush a pending tool-result batch as one `Message::User`.
    fn flush_results(&mut self) {
        if self.pending_results.is_empty() {
            return;
        }
        let results = std::mem::take(&mut self.pending_results);
        self.committed.push(Message::User { content: results });
    }
}

/// Whether an accepted assistant turn carries anything the user/model should
/// see (non-empty text, reasoning, a tool call, or any other content part).
/// Mirrors rig's curation of empty turns out of history.
fn assistant_content_is_visible(content: &[AssistantContent]) -> bool {
    content.iter().any(|c| match c {
        AssistantContent::Text(t) => !t.text.trim().is_empty(),
        AssistantContent::Reasoning(r) => !reasoning_text(r).is_empty(),
        AssistantContent::ToolCall(_) => true,
        _ => true,
    })
}

/// Flatten a rig `Reasoning` payload into plain text. Provider-specific
/// encrypted/redacted blocks are kept verbatim; other shapes are ignored.
pub(crate) fn reasoning_text(r: &rig_core::message::Reasoning) -> String {
    let mut result = String::new();
    for c in &r.content {
        let s = match c {
            ReasoningContent::Text { text, .. } => text.as_str(),
            ReasoningContent::Summary(s) => s.as_str(),
            ReasoningContent::Encrypted(s) => s.as_str(),
            ReasoningContent::Redacted { data } => data.as_str(),
        };
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(s);
    }
    result
}

/// Render `edit` tool-call arguments in the model-facing block form.
///
/// The transcript is stored canonically (a pi-compatible `edits` array) and the
/// wire *accepts* that array too, but the schema does not advertise it — so the
/// history normalizes it back to blocks and the model sees **one** shape. That
/// is still the right call with two shapes accepted: the block form is the one
/// weak harnesses can encode, and a history that mixes both teaches the model to
/// alternate between them. Idempotent and allocation-light: non-`edit` calls and
/// already-block arguments pass through unchanged.
pub(crate) fn blob_edit_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|m| match m {
            Message::Assistant { id, content } => {
                let content = content
                    .into_iter()
                    .map(|c| match c {
                        AssistantContent::ToolCall(tc) if tc.function.name == "edit" => {
                            let mut tc = tc;
                            tc.function.arguments =
                                crate::edit_blocks::array_to_blob_value(&tc.function.arguments);
                            AssistantContent::ToolCall(tc)
                        }
                        other => other,
                    })
                    .collect();
                Message::Assistant { id, content }
            }
            other => other,
        })
        .collect()
}

/// Resolved placements for successful `edit` calls in this run, keyed by the
/// durable tool-call id.
///
/// Shared between the hook (which records placements from
/// [`crate::tools::EditResolutions`]) and the persist path (which applies the
/// same minimization before `append_message`). In-run only: it holds a file's
/// line numbers, not its contents, and is never serialized.
pub(crate) type EditAnchorLedger = HashMap<String, crate::tools::EditResolutions>;

/// Shrink successful `edit` calls in history to their changed lines and anchor
/// each block at the line it actually applied to.
///
/// Only calls present in `ledger` are touched, so failed, skipped and
/// non-`edit` calls pass through byte-identically. Idempotent: an already
/// minimal anchored call re-minimizes to itself.
///
/// This is the single transform both history egresses use — model-facing
/// (`on_completion_call`, before the multi-file split) and persisted
/// (`append_message`, before canonicalization). If you touch the argument
/// shape here, also check `blob_edit_tool_calls`, `split_multi_file_edit_calls`,
/// `per_file_blocks`, and `Session::append_message`'s canonicalize/split.
pub(crate) fn minimize_edit_tool_calls(
    messages: Vec<Message>,
    ledger: &EditAnchorLedger,
) -> Vec<Message> {
    if ledger.is_empty() {
        return messages;
    }
    messages
        .into_iter()
        .map(|m| match m {
            Message::Assistant { id, content } => {
                let content = content
                    .into_iter()
                    .map(|c| match c {
                        AssistantContent::ToolCall(tc)
                            if tc.function.name == "edit"
                                && ledger.contains_key(tc.id.as_ref()) =>
                        {
                            let Some(resolutions) = ledger.get(tc.id.as_ref()) else {
                                return AssistantContent::ToolCall(tc);
                            };
                            let Some(args) =
                                minimized_edit_args(&tc.function.arguments, resolutions)
                            else {
                                return AssistantContent::ToolCall(tc);
                            };
                            let mut tc = tc;
                            tc.function.arguments = args;
                            AssistantContent::ToolCall(tc)
                        }
                        other => other,
                    })
                    .collect();
                Message::Assistant { id, content }
            }
            other => other,
        })
        .collect()
}

/// Rewrite one successful call's arguments into a compact anchored block blob,
/// or `None` when the call cannot be parsed or carries no resolved file.
fn minimized_edit_args(
    args: &serde_json::Value,
    resolutions: &crate::tools::EditResolutions,
) -> Option<serde_json::Value> {
    let parsed = blockedit::EditArgs::from_wire(args).ok()?;
    let mut any_resolved = false;
    let mut blobs = Vec::with_capacity(parsed.files.len());
    for file in parsed.files {
        let resolved = resolutions.files.iter().find(|r| r.path == file.path);
        let minimized: Vec<EditOp> = match resolved {
            Some(r) if r.matched_lines.len() == file.edits.len() => {
                any_resolved = true;
                // An already-anchored edit is already the trimmed form: its
                // own anchor is the base, so re-minimizing is a no-op. This
                // matters for history transformed more than once in a turn.
                // The ledger's resolved line is for edits that arrived
                // unanchored, where only execution knows the placement.
                file.edits
                    .iter()
                    .zip(&r.matched_lines)
                    .flat_map(|(edit, line)| {
                        let base = edit.start_line.map(|s| s.0 as usize).unwrap_or(*line);
                        // One input edit may split into several anchored
                        // sub-edits at long internal unchanged runs.
                        blockedit::split_and_minimize(edit, base)
                    })
                    .collect()
            }
            // A mixed multi-file call keeps the unresolved file exactly as it
            // was: no resolved line means no honest anchor.
            _ => file.edits,
        };
        let edits: Vec<blockedit::ParsedEdit> = minimized
            .into_iter()
            .map(|e| blockedit::ParsedEdit {
                old_text: e.old_text,
                new_text: e.new_text,
                start_line: e.start_line.map(|s| s.0),
                header: None,
                rejected_header: None,
                raw_header: None,
            })
            .collect();
        blobs.push(blockedit::render_blob(&file.path, &edits));
    }
    any_resolved.then(|| serde_json::json!({ "blocks": blobs.join("\n\n") }))
}

/// The text of a tool result, as the model sees it.
pub(crate) fn tool_result_text(tr: &ToolResult) -> Option<String> {
    let joined = tr
        .content
        .iter()
        .filter_map(|rc| match rc {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

/// The per-file halves of one multi-file `edit` call and its result.
///
/// `per_file` is one blob per file (the synthetic call arguments);
/// `results` is one result string per file. Both are in the same first-seen
/// order, because the result array is built in the same order the files were
/// applied — so a synthetic call and its result pair up by index.
struct SplitEditCall {
    per_file: Vec<String>,
    results: Vec<String>,
}

/// Split a multi-file `edit` call and its result into per-file halves.
///
/// Returns `None` unless *both* halves are present, multi-file, and the same
/// length: a call with no array result (mid-turn, or a call that errored before
/// producing one) and a single-file call are left exactly as they are, so a
/// partially-saved history never renders a split with a dangling result. The
/// length check is a belt-and-braces guard — the two are built from the same
/// file list, so a mismatch would mean a corrupt array, and dropping the whole
/// call to the original single form is the safe reading of that.
fn split_edit_call_and_result(args: &serde_json::Value, result: &str) -> Option<SplitEditCall> {
    let per_file = crate::edit_blocks::per_file_blocks(args)?;
    let results: Vec<String> = blockedit::parse_file_results(result)?
        .iter()
        .map(|f| match &f.result {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    (per_file.len() == results.len()).then_some(SplitEditCall { per_file, results })
}

#[allow(dead_code)]
/// The note shown in place of one multi-file call, so the model is not left
/// wondering why it is looking at N calls it did not make.
fn split_note(n: usize) -> String {
    format!(
        "Note: the edit call above edited {n} files. It is shown here as {n} \
         per-file calls with one result each, so each file's outcome is \
         attributed to a file."
    )
}

/// Rewrite a multi-file `edit` call and its result into per-file calls and
/// per-file results, so the model sees one file per call — the shape it would
/// have produced by calling the tool N times, and the shape a reloaded session
/// replays (the persist half of this, W8, writes the same per-file form).
///
/// This is a **view** transform on the request history. It must not alter what
/// is stored: the per-file result array is the durable record, and rewriting it
/// here would leave the session unsplittable. See doc/batch_editing_v2.md,
/// "The result must survive to history" — the reason `on_tool_result` is left
/// alone.
///
/// Idempotent by construction: re-applying finds only single-file calls (the
/// synthetic ones) and non-array results, and leaves the history alone.
pub(crate) fn split_multi_file_edit_calls(messages: Vec<Message>) -> Vec<Message> {
    // The call (assistant message) and its result (a user message) are
    // different messages, and the result follows the call, so build the whole
    // plan before rebuilding any message.
    let mut results: HashMap<ToolCallId, String> = HashMap::new();
    for m in &messages {
        let Message::User { content } = m else {
            continue;
        };
        for c in content {
            if let UserContent::ToolResult(tr) = c {
                if let Some(text) = tool_result_text(tr) {
                    results.insert(tr.call.clone(), text);
                }
            }
        }
    }

    let mut plan: HashMap<ToolCallId, SplitEditCall> = HashMap::new();
    for m in &messages {
        let Message::Assistant { content, .. } = m else {
            continue;
        };
        for c in content {
            let AssistantContent::ToolCall(tc) = c else {
                continue;
            };
            if tc.function.name != "edit" {
                continue;
            }
            if let Some(result) = results.get(&tc.id) {
                if let Some(split) = split_edit_call_and_result(&tc.function.arguments, result) {
                    plan.insert(tc.id.clone(), split);
                }
            }
        }
    }
    if plan.is_empty() {
        return messages;
    }

    // Rebuild. Every synthetic message clears `provider` and `signature`: the
    // provider issued one id and one signature, and Anthropic's wire requires
    // `tool_result.tool_use_id` to equal a `tool_use.id` it issued — N
    // messages claiming the original provider id would be unmatchable.
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            Message::Assistant { id, content } => {
                let mut new_content = Vec::with_capacity(content.len());
                for c in content {
                    let AssistantContent::ToolCall(tc) = &c else {
                        new_content.push(c);
                        continue;
                    };
                    let Some(split) = plan.get(&tc.id) else {
                        new_content.push(c);
                        continue;
                    };
                    let n = split.per_file.len();
                    for (i, blocks) in split.per_file.iter().enumerate() {
                        new_content.push(AssistantContent::ToolCall(rig_core::message::ToolCall {
                            id: crate::edit_blocks::split_id(&tc.id, i, n),
                            provider: None,
                            function: ToolFunction::new(
                                "edit".into(),
                                serde_json::json!({ "blocks": blocks }),
                            ),
                            signature: None,
                            additional_params: None,
                        }));
                    }
                }
                out.push(Message::Assistant {
                    id,
                    content: new_content,
                });
            }
            Message::User { content } => {
                let mut new_content = Vec::with_capacity(content.len());
                for c in content {
                    let UserContent::ToolResult(tr) = &c else {
                        new_content.push(c);
                        continue;
                    };
                    let Some(split) = plan.get(&tr.call) else {
                        new_content.push(c);
                        continue;
                    };
                    let n = split.per_file.len();
                    // The note rides with the results it explains, as leading
                    // text in the same user message — not a message of its own,
                    // which would perturb the turn structure.
                    // gwicke: disabled to see if that avoids deepseek issues
                    // new_content.push(UserContent::Text(Text::new(split_note(n))));
                    for (i, result) in split.results.iter().enumerate() {
                        new_content.push(UserContent::ToolResult(ToolResult {
                            call: crate::edit_blocks::split_id(&tr.call, i, n),
                            provider: None,
                            name: tr.name.clone(),
                            content: vec![ToolResultContent::text(result.clone())],
                        }));
                    }
                }
                out.push(Message::User {
                    content: new_content,
                });
            }
            other => out.push(other),
        }
    }
    out
}

/// No-op in rig ≥1c2f440: `ToolResult::call` (rig's correlation handle) is
/// always present — minted at the provider boundary when the provider
/// issued no id — so the old cross-provider `call_id` normalisation is
/// inherently satisfied. Kept as a named passthrough so the persist path
/// reads the same. See doc/pi_steering_rig.md §6.5.4.
pub(crate) fn normalize_call_ids(messages: Vec<Message>) -> Vec<Message> {
    messages
}

/// Merge a finished (or aborted) turn into the session `history`.
///
/// * `final_history` — messages for a *successfully completed* turn (`[user,
///   assistant]`). Appended, because `history` already holds the prior turns;
///   this mirrors rig's blocking `chat()`, which does `chat_history.extend(
///   messages)`. This is the root-cause-#1 fix: the previous code *replaced*
///   the whole history with just this turn, so earlier context was lost every
///   turn.
/// * `cancelled_history` — the in-flight *full* history rig attaches to a
///   `PromptCancelled` error (prior turns + the aborted user prompt + any
///   partial assistant text). Replaces `history` rather than appending, since
///   it already contains the prior turns; appending would duplicate them. This
///   is the root-cause-#2 fix.
/// * `reasoning_in_context` — when `false`, `Reasoning` blocks are stripped
///   from assistant messages before they enter the stored context (a cheap,
///   deterministic "Level 0" compaction that saves tokens). Ignored for
///   Anthropic-family providers (`provider`/`model` satisfy
///   [`is_anthropic_family`]) **on turns that contain tool calls**, which
///   require the leading reasoning to be replayed verbatim on the next turn
///   (thinking+tool_use contract); reasoning is force-kept there. On
///   Anthropic turns *without* tool calls, and on all other providers when
///   the flag is `false`, reasoning is stripped normally.
#[allow(dead_code)]
pub(crate) fn persist_turn(
    history: &mut Option<&mut Vec<Message>>,
    final_history: Option<Vec<Message>>,
    cancelled_history: Option<Vec<Message>>,
    reasoning_in_context: crate::settings::ReasoningInContextMode,
    provider: &str,
    model: &str,
) {
    if let Some(h) = history {
        if let Some(fh) = final_history {
            let to_commit = maybe_strip_reasoning(fh, reasoning_in_context, provider, model);
            let to_commit = normalize_call_ids(to_commit);
            h.extend(to_commit);
        } else if let Some(ch) = cancelled_history {
            let to_commit = maybe_strip_reasoning(ch, reasoning_in_context, provider, model);
            let to_commit = normalize_call_ids(to_commit);
            **h = to_commit;
        }
    }
}

/// Strip `AssistantContent::Reasoning` blocks based on the
/// `ReasoningInContextMode`. Returns messages unchanged when:
/// - Mode is `Always`
/// - Provider is Anthropic-family **and** the turn contains tool calls
///   (the thinking+tool_use replay contract)
///
/// When mode is `AbortedTurnsOnly`, reasoning is preserved for assistant
/// messages with `id: None` (aborted/errored turns created locally without
/// a provider responseId), but stripped from successful turns.
///
/// Assistant messages left empty by the filter get a placeholder `Text` so
/// the message stays well-formed and user/assistant alternation holds.
pub(crate) fn maybe_strip_reasoning(
    messages: Vec<Message>,
    reasoning_in_context: crate::settings::ReasoningInContextMode,
    provider: &str,
    model: &str,
) -> Vec<Message> {
    let keep_all = matches!(
        reasoning_in_context,
        crate::settings::ReasoningInContextMode::Always
    ) || (is_anthropic_family(provider, model) && turn_has_tool_calls(&messages));
    if keep_all {
        return messages;
    }
    let keep_aborted = matches!(
        reasoning_in_context,
        crate::settings::ReasoningInContextMode::AbortedTurnsOnly
    );
    messages
        .into_iter()
        .map(|m| match m {
            Message::Assistant { id, content } => {
                let is_aborted = id.is_none(); // No provider responseId = aborted/errored turn
                let kept: Vec<AssistantContent> = content
                    .into_iter()
                    .filter(|c| {
                        if keep_aborted && is_aborted && matches!(c, AssistantContent::Reasoning(_))
                        {
                            true // Preserve reasoning for aborted turns
                        } else {
                            !matches!(c, AssistantContent::Reasoning(_))
                        }
                    })
                    .collect();
                let content = if kept.is_empty() {
                    vec![AssistantContent::Text(Text::new("[reasoning stripped]"))]
                } else {
                    kept
                };
                Message::Assistant { id, content }
            }
            other => other,
        })
        .collect()
}

/// Whether the turn's persisted messages include at least one assistant
/// `ToolCall` block. Used to decide whether Anthropic-family reasoning must
/// be force-kept: the thinking+tool_use replay contract only applies on
/// turns that actually involve tool calls.
fn turn_has_tool_calls(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m {
        Message::Assistant { content, .. } => content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_))),
        _ => false,
    })
}

/// Whether `provider`/`model` refers to an Anthropic-family model, for which
/// reasoning must be kept in context (the thinking+tool_use replay contract).
/// Covers direct `anthropic` and OpenRouter routing to `anthropic/*` models.
pub(crate) fn is_anthropic_family(provider: &str, model: &str) -> bool {
    provider.eq_ignore_ascii_case("anthropic")
        || (provider.eq_ignore_ascii_case("openrouter") && model.starts_with("anthropic/"))
}

pub(crate) fn is_tool_available(tool: &str) -> bool {
    std::process::Command::new("which")
        .arg(tool)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
/// The note to show when a `/reasoning` override asks for a level `support`
/// cannot serve, or `None` when it can.
///
/// Split out of [`effective_params`] so it is testable against a support table
/// directly, with no catalog cache involved — the same reason
/// `model_catalog::support_for_entry` is split out.
///
/// The comparison is on **rank**, not on the string. `resolve` returns the
/// model's own spelling, so a model publishing `medium` for our `med` would
/// otherwise be reported as a downgrade that never happened.
pub(crate) fn reasoning_remap_note(
    requested: crate::params::ReasoningLevel,
    support: &crate::params::ReasoningSupport,
) -> Option<String> {
    let resolved = support.resolve(requested)?;
    let moved = resolved
        .parse::<crate::params::ReasoningLevel>()
        .map(|l| l.rank())
        != Ok(requested.rank());
    moved.then(|| {
        format!(
            "note: reasoning '{}' → '{resolved}' (this model does not support it)",
            requested.as_str()
        )
    })
}
/// The text the model should see for a tool result, or `None` for "leave it
/// alone".
///
/// The `edit` tool's argument errors are a self-correcting message meant to be
/// read by the model, but they arrive wrapped in two redundant prefixes (rig's
/// `"failed to parse tool arguments: "` and the tool's own `"Invalid edit tool
/// arguments: "`). Stripping them leaves the model only the problem, the
/// expected block shape, and the offending structure — a weak model is
/// measurably likelier to self-correct from a terse message than from one buried
/// in ceremony, and the "something went wrong, let me try python" fallback starts
/// with exactly such a wall.
///
/// Only the *text* changes. The result's kind, retryability, and raw structured
/// form are untouched, and a non-`edit` tool (or a message without the known
/// prefixes) is returned unchanged, so this can never swallow content.
pub(crate) fn model_facing_tool_text<'a>(tool_name: &str, text: &'a str) -> Option<&'a str> {
    if tool_name != "edit" {
        return None;
    }
    let cleaned = blockedit::strip_arg_error_prefixes(text);
    (cleaned != text).then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent::TurnPersistence;
    use rig_core::message::Reasoning;
    /// The model-facing text for a malformed `edit` block must lead with the
    /// problem and drop the two redundant wrappers. This is the single highest-
    /// value behavior in the change: it is what a weak model actually reads when
    /// it gets a marker wrong.
    #[test]
    fn model_text_strips_redundant_arg_error_wrappers() {
        let raw = "failed to parse tool arguments: Invalid edit tool arguments: \
Invalid block: malformed `@@@NEW` marker `@@@NEWEND`: a marker must be alone on its line";
        let cleaned = model_facing_tool_text("edit", raw).expect("edit errors are rewritten");
        assert_eq!(
            cleaned,
            "Invalid block: malformed `@@@NEW` marker `@@@NEWEND`: a marker must be alone on its line"
        );
        assert!(!cleaned.contains("failed to parse tool arguments"));
        assert!(!cleaned.contains("Invalid edit tool arguments"));
    }

    #[test]
    fn model_text_rewrites_nothing_else() {
        // A non-edit tool is never rewritten.
        let args_err = "failed to parse tool arguments: bad integer";
        assert_eq!(model_facing_tool_text("bash", args_err), None);
        // An edit success (or any message without the wrappers) is untouched.
        let ok = "Replaced 1 block(s) in src/x.rs";
        assert_eq!(model_facing_tool_text("edit", ok), None);
        // A call-time error the model sees is passed through as-is.
        let call_err = "the tool failed";
        assert_eq!(model_facing_tool_text("edit", call_err), None);
    }

    /// A three-file call plus its array result, as the stored history holds it.
    fn multi_file_history() -> Vec<Message> {
        let blocks = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                      b.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END\n\n\
                      c.rs\n@@@OLD\nq\n@@@NEW\nQ\n@@@END";
        let result = blockedit::render_file_results(&[
            blockedit::FileResult {
                path: "a.rs".into(),
                result: serde_json::json!("Replaced 1 block(s) in ./a.rs"),
            },
            blockedit::FileResult {
                path: "b.rs".into(),
                result: serde_json::json!({
                    "ui_view": "edit failed: nope", "model_view": "long diff"
                }),
            },
            blockedit::FileResult {
                path: "c.rs".into(),
                result: serde_json::json!("Replaced 1 block(s) in ./c.rs"),
            },
        ]);
        vec![
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                    id: ToolCallId::new("t1").unwrap(),
                    provider: None,
                    function: ToolFunction::new(
                        "edit".into(),
                        serde_json::json!({ "blocks": blocks }),
                    ),
                    signature: None,
                    additional_params: None,
                })],
            },
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult {
                    call: ToolCallId::new("t1").unwrap(),
                    provider: None,
                    name: "edit".into(),
                    content: vec![ToolResultContent::text(result)],
                })],
            },
        ]
    }

    fn tool_calls_of(m: &Message) -> Vec<rig_core::message::ToolCall> {
        let Message::Assistant { content, .. } = m else {
            panic!("expected an assistant message")
        };
        content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect()
    }

    fn results_of(m: &Message) -> Vec<(String, String)> {
        let Message::User { content } = m else {
            panic!("expected a user message")
        };
        content
            .iter()
            .filter_map(|c| match c {
                UserContent::ToolResult(tr) => Some((
                    tr.call.to_string(),
                    tool_result_text(tr).unwrap_or_default(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_multi_file_call_becomes_n_per_file_calls_and_results() {
        let out = split_multi_file_edit_calls(multi_file_history());

        // Three calls, each naming exactly one file.
        let calls = tool_calls_of(&out[0]);
        assert_eq!(calls.len(), 3, "{:?}", calls);
        for (i, name) in ["a.rs", "b.rs", "c.rs"].iter().enumerate() {
            let args = &calls[i].function.arguments;
            assert_eq!(calls[i].function.name, "edit");
            let blocks = args.get("blocks").and_then(|v| v.as_str()).unwrap();
            assert!(blocks.contains(name), "call {i} must name {name}: {blocks}");
            assert_eq!(
                blocks.matches("@@@OLD").count(),
                1,
                "call {i} must carry one file only: {blocks}"
            );
            // The id is derived, not minted, so both split sites agree.
            if i == 0 {
                assert_eq!(calls[i].id.to_string(), format!("t1"));
            } else {
                assert_eq!(calls[i].id.to_string(), format!("t1_f{i}n3"));
            }
            // A synthesized call has no provider id or signature: Anthropic
            // keys tool_result on the id it issued, and N copies of one would
            // be unmatchable.
            assert!(calls[i].provider.is_none());
            assert!(calls[i].signature.is_none());
        }

        // Three results, one per call, paired by that same derived id.
        let results = results_of(&out[1]);
        assert_eq!(results.len(), 3, "{:?}", results);
        assert_eq!(results[0].0, "t1");
        assert_eq!(results[1].0, "t1_f1n3");
        assert_eq!(results[2].0, "t1_f2n3");
        assert!(results[0].1.contains("Replaced 1 block(s) in ./a.rs"));
        // A failing file keeps its full envelope: the model reads model_view.
        assert!(results[1].1.contains("long diff"), "{}", results[1].1);
        assert!(results[2].1.contains("./c.rs"));
    }

    // #[test]
    // fn a_split_explains_itself_in_one_note_carrying_no_aggregate() {
    //     let out = split_multi_file_edit_calls(multi_file_history());
    //     let Message::User { content } = &out[1] else {
    //         panic!("expected a user message")
    //     };
    //     let notes: Vec<&Text> = content
    //         .iter()
    //         .filter_map(|c| match c {
    //             UserContent::Text(t) => Some(t),
    //             _ => None,
    //         })
    //         .collect();
    //     assert_eq!(notes.len(), 1, "exactly one note per split: {content:?}");
    //     let n = notes[0].text.as_str();
    //     assert!(n.contains('3'), "the note must name the file count: {n}");
    //     // The note rides inside the results' message, not as its own.
    //     assert_eq!(content[0], UserContent::Text(Text::new(n.to_string())));
    // }

    #[test]
    fn successful_calls_are_minimized_and_line_anchored() {
        use crate::tools::{EditResolutions, FileResolution};
        let call = Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                id: ToolCallId::new("e1").unwrap(),
                provider: None,
                function: ToolFunction::new(
                    "edit".into(),
                    serde_json::json!({
                        "blocks": "a.rs\n@@@OLD\nkeep_line_alpha\nold_value_line\nkeep_line_bravo\n\
                                   @@@NEW\nkeep_line_alpha\nnew_value_line\nkeep_line_bravo\n@@@END"
                    }),
                ),
                signature: None,
                additional_params: None,
            })],
        };
        let mut ledger = EditAnchorLedger::default();
        ledger.insert(
            "e1".into(),
            EditResolutions {
                files: vec![FileResolution {
                    path: "a.rs".into(),
                    matched_lines: vec![40],
                }],
            },
        );
        let out = minimize_edit_tool_calls(vec![call], &ledger);
        let blocks = tool_calls_of(&out[0])[0].function.arguments["blocks"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            blocks,
            "a.rs:41\n@@@OLD\nold_value_line\n@@@NEW\nnew_value_line\n@@@END"
        );
    }

    #[test]
    fn a_long_internal_gap_splits_into_anchored_sub_blocks() {
        use crate::tools::{EditResolutions, FileResolution};
        let call = Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                id: ToolCallId::new("e1").unwrap(),
                provider: None,
                function: ToolFunction::new(
                    "edit".into(),
                    serde_json::json!({
                        "blocks": "a.rs\n@@@OLD\nalpha_line\nold_x_line\nbravo_line\ncharlie_line\n\
                                   delta_line\nold_y_line\necho_line\n@@@NEW\nalpha_line\nnew_x_line\n\
                                   bravo_line\ncharlie_line\ndelta_line\nnew_y_line\necho_line\n@@@END"
                    }),
                ),
                signature: None,
                additional_params: None,
            })],
        };
        let mut ledger = EditAnchorLedger::default();
        ledger.insert(
            "e1".into(),
            EditResolutions {
                files: vec![FileResolution {
                    path: "a.rs".into(),
                    matched_lines: vec![10],
                }],
            },
        );
        let out = minimize_edit_tool_calls(vec![call], &ledger);
        let blocks = tool_calls_of(&out[0])[0].function.arguments["blocks"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            blocks,
            "a.rs:11\n@@@OLD\nold_x_line\n@@@NEW\nnew_x_line\n@@@END\n\n\
             a.rs:15\n@@@OLD\nold_y_line\n@@@NEW\nnew_y_line\n@@@END"
        );
    }

    #[test]
    fn minimization_is_idempotent() {
        use crate::tools::{EditResolutions, FileResolution};
        let call = Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                id: ToolCallId::new("e1").unwrap(),
                provider: None,
                function: ToolFunction::new(
                    "edit".into(),
                    serde_json::json!({
                        "blocks": "a.rs\n@@@OLD\nkeep\nold\nkeep2\n@@@NEW\nkeep\nnew\nkeep2\n@@@END"
                    }),
                ),
                signature: None,
                additional_params: None,
            })],
        };
        let mut ledger = EditAnchorLedger::default();
        ledger.insert(
            "e1".into(),
            EditResolutions {
                files: vec![FileResolution {
                    path: "a.rs".into(),
                    matched_lines: vec![40],
                }],
            },
        );
        let once = minimize_edit_tool_calls(vec![call], &ledger);
        let twice = minimize_edit_tool_calls(once.clone(), &ledger);
        assert_eq!(once, twice);
    }

    #[test]
    fn a_failed_call_without_resolutions_is_untouched() {
        let call = Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                id: ToolCallId::new("e1").unwrap(),
                provider: None,
                function: ToolFunction::new(
                    "edit".into(),
                    serde_json::json!({"blocks": "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END"}),
                ),
                signature: None,
                additional_params: None,
            })],
        };
        let out = minimize_edit_tool_calls(vec![call.clone()], &EditAnchorLedger::default());
        assert_eq!(out, vec![call]);
    }

    #[test]
    fn splitting_twice_changes_nothing() {
        let once = split_multi_file_edit_calls(multi_file_history());
        let twice = split_multi_file_edit_calls(once.clone());
        assert_eq!(once, twice, "the transform must be idempotent");
    }

    #[test]
    fn a_single_file_call_is_left_byte_identical() {
        let call = Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                id: ToolCallId::new("s1").unwrap(),
                provider: None,
                function: ToolFunction::new(
                    "edit".into(),
                    serde_json::json!({"blocks": "solo.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END"}),
                ),
                signature: None,
                additional_params: None,
            })],
        };
        let result = Message::User {
            content: vec![UserContent::ToolResult(ToolResult {
                call: ToolCallId::new("s1").unwrap(),
                provider: None,
                name: "edit".into(),
                content: vec![ToolResultContent::text("Replaced 1 block(s) in ./solo.rs")],
            })],
        };
        let hist = vec![call, result];
        let out = split_multi_file_edit_calls(hist.clone());
        assert_eq!(out, hist, "a single-file call must be untouched");
    }

    #[test]
    fn a_call_with_no_result_yet_is_not_split() {
        // Mid-turn: the model made the call, the result has not come back.
        // Splitting now would show a call with a dangling result.
        let pending = vec![multi_file_history().remove(0)];
        let out = split_multi_file_edit_calls(pending.clone());
        assert_eq!(out, pending, "a call without its result stays whole");
    }

    #[test]
    fn blob_edit_tool_calls_renders_block_form_and_is_idempotent() {
        let args = serde_json::json!({
            "path": "src/x.rs",
            "edits": [{"oldText": "a", "newText": "b"}]
        });
        let mk = |args: serde_json::Value| Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(rig_core::message::ToolCall {
                id: rig_core::message::ToolCallId::new("t1").unwrap(),
                provider: None,
                function: rig_core::message::ToolFunction::new("edit".into(), args),
                signature: None,
                additional_params: None,
            })],
        };

        // Canonical array -> blob, under the model-facing field name.
        let out = blob_edit_tool_calls(vec![mk(args)]);
        let Message::Assistant { content, .. } = &out[0] else {
            panic!("expected assistant")
        };
        let AssistantContent::ToolCall(tc) = &content[0] else {
            panic!("expected tool call")
        };
        let batches = tc
            .function
            .arguments
            .get("blocks")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(batches.contains("@@@OLD"), "got: {batches}");
        assert!(batches.contains("src/x.rs"), "got: {batches}");
        // The canonical array must not survive into the model-facing form.
        assert!(
            tc.function.arguments.get("edits").is_none(),
            "model form must not carry the array: {:?}",
            tc.function.arguments
        );

        // Idempotent: an already-block value is unchanged.
        let already =
            serde_json::json!({"path": "src/x.rs", "blocks": "@@@OLD\na\n@@@NEW\nb\n@@@END"});
        let out2 = blob_edit_tool_calls(vec![mk(already.clone())]);
        let Message::Assistant { content, .. } = &out2[0] else {
            panic!("expected assistant")
        };
        let AssistantContent::ToolCall(tc) = &content[0] else {
            panic!("expected tool call")
        };
        assert_eq!(tc.function.arguments, already);
    }

    #[test]
    fn turn_persistence_counts_dispatch_not_text() {
        // The per-turn record counts NEW dispatches (the user's original
        // prompt plus each steering/continuation directive), never the
        // prompt TEXT. This is what lets two identical "continue" messages
        // in one turn be treated as distinct dispatches.
        let turn = TurnPersistence::new();
        assert_eq!(turn.sent(), 0);

        // The user's first prompt is dispatch 1.
        turn.mark_new_dispatch();
        assert_eq!(turn.sent(), 1);

        // A retry that re-sends the SAME prompt does NOT bump the count:
        // callers only call mark_new_dispatch when next_input actually
        // changes to a new message.
        // (No-op here; the no-resend case is just "don't call it".)

        // A steering "continue" (same text as a possible original) is a NEW
        // dispatch and must bump.
        turn.mark_new_dispatch();
        assert_eq!(turn.sent(), 2);

        // A second "continue" steering is yet another dispatch.
        turn.mark_new_dispatch();
        assert_eq!(turn.sent(), 3);
    }

    #[test]
    fn turn_persistence_is_fresh_per_new_turn() {
        // Two successive user turns must have independent dispatch counts
        // (e.g. the user types "continue" twice). The REPL mints a fresh
        // TurnPersistence per user prompt, so turn 2 starts at 0 even
        // though turn 1 dispatched and persisted a "continue".
        let turn1 = TurnPersistence::new();
        turn1.mark_new_dispatch();
        assert_eq!(turn1.sent(), 1);

        let turn2 = TurnPersistence::new();
        assert_eq!(turn2.sent(), 0);
    }
    // ---- TurnTranscript accumulation & finalization ----
    //
    // The transcript mirrors rig's `new_messages` shape so a mid-turn failure
    // can persist exactly what completed (doc/tool_error_handling.md
    // §Mid-turn failure). These tests pin the accumulation order and the
    // no-truncation finalization: unanswered trailing tool calls are answered
    // with synthesized text results, never dropped.

    #[test]
    fn transcript_batches_tool_results_between_assistant_turns() {
        use super::TurnTranscript;
        use rig_core::message::{ToolCall, ToolFunction, ToolResultContent};

        let tc = ToolCall::from_wire(
            "call_1",
            ToolFunction::new("read".to_string(), serde_json::json!({})),
        );
        let call_id = tc.id.clone();
        let mut t = TurnTranscript::default();
        // Assistant turn with a tool call.
        t.push_assistant_turn(None, vec![AssistantContent::ToolCall(tc)]);
        assert!(t.has_progress());
        // Tool result streams back (batch accumulates).
        t.push_tool_result(rig_core::message::ToolResult {
            call: call_id,
            provider: None,
            name: "read".to_string(),
            content: vec![ToolResultContent::text("ok")],
        });
        // Next assistant turn flushes the batch as one Message::User.
        t.push_assistant_turn(None, vec![AssistantContent::text("done")]);
        let f = t.finalize();
        assert_eq!(f.len(), 3, "assistant + user(results) + assistant: {f:?}");
        assert!(
            matches!(&f[1], Message::User { .. }),
            "batch must be one user msg"
        );
    }

    #[test]
    fn transcript_finalize_answers_interrupted_calls_with_text_results() {
        use super::TurnTranscript;
        use rig_core::message::{ToolCall, ToolFunction, ToolResultContent, UserContent};

        let tc = ToolCall::from_wire(
            "call_1",
            ToolFunction::new("read".to_string(), serde_json::json!({})),
        );
        let mut t = TurnTranscript::default();
        t.push_assistant_turn(None, vec![AssistantContent::ToolCall(tc)]);
        // Run interrupted before any tool result: finalize must synthesize a
        // text result per unanswered call — never truncate.
        let f = t.finalize();
        assert_eq!(f.len(), 2, "assistant + synthesized user: {f:?}");
        let Message::User { content } = &f[1] else {
            panic!("expected a user message");
        };
        let UserContent::ToolResult(tr) = &content[0] else {
            panic!("expected a tool result");
        };
        assert_eq!(tr.name, "read");
        let ToolResultContent::Text(txt) = &tr.content[0] else {
            panic!("expected text content");
        };
        assert!(txt.text.contains("interrupted"), "got: {}", txt.text);
    }

    #[test]
    fn transcript_skips_turns_without_visible_content() {
        use super::TurnTranscript;

        let mut t = TurnTranscript::default();
        t.push_assistant_turn(None, vec![AssistantContent::text("   ")]);
        assert!(
            !t.has_progress(),
            "blank-text turn must not count as progress"
        );
        assert!(t.finalize().is_empty());
    }
    #[test]
    fn reasoning_text_extracts_text_block() {
        let r = Reasoning::new("think step by step");
        assert_eq!(reasoning_text(&r), "think step by step");
    }

    #[test]
    fn reasoning_text_extracts_text_with_signature() {
        let r = Reasoning::new_with_signature("with sig", Some("sig".to_string()));
        assert_eq!(reasoning_text(&r), "with sig");
    }
    // Regression test for root cause #1: a successful turn must *append* to the
    // existing history, preserving prior turns. The old code replaced the whole
    // history with just the latest turn, so earlier exchanges were lost every
    // turn.
    #[test]
    fn successful_turn_appends_to_history() {
        let mut hist: Vec<Message> = vec![Message::user("prior-1")];
        persist_turn(
            &mut Some(&mut hist),
            Some(vec![
                Message::user("turn-2-prompt"),
                Message::assistant("turn-2-reply"),
            ]),
            None,
            crate::settings::ReasoningInContextMode::Always,
            "openai",
            "",
        );

        // Prior turn preserved, new turn appended (not wiped/replaced).
        assert_eq!(hist.len(), 3);
        assert!(hist.contains(&Message::user("prior-1")));
        assert!(hist.contains(&Message::user("turn-2-prompt")));
        assert!(hist.contains(&Message::assistant("turn-2-reply")));
        // Prior must come before the new turn.
        let prior_idx = hist
            .iter()
            .position(|m| *m == Message::user("prior-1"))
            .unwrap();
        let new_idx = hist
            .iter()
            .position(|m| *m == Message::user("turn-2-prompt"))
            .unwrap();
        assert!(prior_idx < new_idx);
    }

    // Regression test for root cause #2: an aborted turn must keep the prior
    // turns and commit the in-flight full history *without* duplicating the
    // priors (rig's cancellation error already includes them).
    #[test]
    fn aborted_turn_keeps_prior_and_no_duplication() {
        let mut hist: Vec<Message> = vec![Message::user("prior-1")];
        // rig's `PromptCancelled.chat_history` == full_history() == prior + this turn.
        let cancelled = vec![
            Message::user("prior-1"),
            Message::user("aborted-prompt"),
            Message::assistant("aborted-partial"),
        ];
        use crate::settings::ReasoningInContextMode;
        persist_turn(
            &mut Some(&mut hist),
            None,
            Some(cancelled),
            ReasoningInContextMode::Always,
            "openai",
            "",
        );

        // Replaced (not appended): exactly the in-flight full history, with no
        // duplicate of `prior-1`.
        assert_eq!(hist.len(), 3);
        assert_eq!(
            hist.iter()
                .filter(|m| **m == Message::user("prior-1"))
                .count(),
            1
        );
        assert!(hist.contains(&Message::user("aborted-prompt")));
        assert!(hist.contains(&Message::assistant("aborted-partial")));
    }

    // `reasoning_in_context=Never` must drop reasoning blocks from assistant
    // messages for non-Anthropic providers, while keeping text (and keeping
    // reasoning entirely for Anthropic-family turns that involve tool calls, or
    // when the mode is Always). On an Anthropic turn *without* tool calls, the
    // mode is honored and reasoning is stripped.
    #[test]
    fn strips_reasoning_for_non_anthropic_when_never() {
        use crate::settings::ReasoningInContextMode;
        let assistant = Message::Assistant {
            id: None,
            content: vec![
                AssistantContent::Reasoning(Reasoning::new("hidden thought")),
                AssistantContent::Text(Text::new("answer")),
            ],
        };
        let msgs = vec![Message::user("q"), assistant];

        // Never + non-Anthropic: reasoning dropped, text preserved.
        let out = maybe_strip_reasoning(msgs.clone(), ReasoningInContextMode::Never, "openai", "");
        if let Message::Assistant { content, .. } = &out[1] {
            assert!(!content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))));
            assert!(content
                .iter()
                .any(|c| matches!(c, AssistantContent::Text(_))));
        } else {
            panic!("expected assistant message");
        }

        // Always: reasoning kept.
        let kept =
            maybe_strip_reasoning(msgs.clone(), ReasoningInContextMode::Always, "openai", "");
        if let Message::Assistant { content, .. } = &kept[1] {
            assert!(content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))));
        } else {
            panic!("expected assistant message");
        }

        // Anthropic-family WITHOUT tool calls: mode honored, reasoning stripped.
        let stripped = maybe_strip_reasoning(
            msgs.clone(),
            ReasoningInContextMode::Never,
            "anthropic",
            "claude-3-5-sonnet",
        );
        if let Message::Assistant { content, .. } = &stripped[1] {
            assert!(!content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))));
            assert!(content
                .iter()
                .any(|c| matches!(c, AssistantContent::Text(_))));
        } else {
            panic!("expected assistant message");
        }
    }

    /// Test AbortedTurnsOnly mode: preserves reasoning for aborted turns (id: None)
    /// but strips from successful turns (id: Some).
    #[test]
    fn aborted_turns_only_preserves_reasoning_for_aborted() {
        use crate::settings::ReasoningInContextMode;
        // Successful turn: has responseId
        let successful_assistant = Message::Assistant {
            id: Some("resp-123".into()),
            content: vec![
                AssistantContent::Reasoning(Reasoning::new("successful thought")),
                AssistantContent::Text(Text::new("successful answer")),
            ],
        };
        // Aborted turn: no responseId
        let aborted_assistant = Message::Assistant {
            id: None,
            content: vec![
                AssistantContent::Reasoning(Reasoning::new("aborted thought")),
                AssistantContent::Text(Text::new("aborted answer")),
            ],
        };
        let msgs = vec![
            Message::user("q1"),
            successful_assistant,
            Message::user("q2"),
            aborted_assistant,
        ];

        // AbortedTurnsOnly: successful reasoning stripped, aborted reasoning kept
        let out = maybe_strip_reasoning(
            msgs.clone(),
            ReasoningInContextMode::AbortedTurnsOnly,
            "openai",
            "",
        );
        let contents: Vec<&Vec<AssistantContent>> = out
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(contents.len(), 2);
        // First (successful) should have reasoning stripped
        assert!(!contents[0]
            .iter()
            .any(|c| matches!(c, AssistantContent::Reasoning(_))));
        assert!(contents[0]
            .iter()
            .any(|c| matches!(c, AssistantContent::Text(t) if t.text == "successful answer")));
        // Second (aborted) should have reasoning preserved
        assert!(contents[1]
            .iter()
            .any(|c| matches!(c, AssistantContent::Reasoning(_))));
        assert!(contents[1]
            .iter()
            .any(|c| matches!(c, AssistantContent::Text(t) if t.text == "aborted answer")));
    }

    // Anthropic-family turn WITH a tool call: the leading reasoning must be
    // force-kept (thinking+tool_use replay contract), even with the flag off.
    #[test]
    fn anthropic_tool_call_turn_forces_reasoning_kept() {
        let assistant = Message::Assistant {
            id: None,
            content: vec![
                AssistantContent::Reasoning(Reasoning::new("hidden thought")),
                AssistantContent::Text(Text::new("answer")),
                AssistantContent::ToolCall(rig_core::message::ToolCall::new(
                    rig_core::message::ToolCallId::new("tool-1").unwrap(),
                    rig_core::message::ToolFunction::new(
                        "Bash".to_string(),
                        serde_json::json!({"cmd": "ls"}),
                    ),
                )),
            ],
        };
        let msgs = vec![Message::user("q"), assistant];

        let forced = maybe_strip_reasoning(
            msgs,
            crate::settings::ReasoningInContextMode::Never,
            "anthropic",
            "claude-3-5-sonnet",
        );
        if let Message::Assistant { content, .. } = &forced[1] {
            assert!(content
                .iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))));
        } else {
            panic!("expected assistant message");
        }
    }

    #[test]
    fn is_anthropic_family_classifies_providers() {
        assert!(is_anthropic_family("anthropic", "claude-x"));
        assert!(is_anthropic_family(
            "openrouter",
            "anthropic/claude-3.5-sonnet"
        ));
        assert!(!is_anthropic_family("openrouter", "openai/gpt-4o"));
        assert!(!is_anthropic_family("openai", "gpt-4o"));
    }
}
