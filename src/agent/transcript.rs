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
                    new_content.push(UserContent::Text(Text::new(split_note(n))));
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
