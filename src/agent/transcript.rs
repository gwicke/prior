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
    Api, AssistantContent, AssistantMessage, CallId, Message, Origin, Text, ToolFunction, ToolName,
    ToolResult, ToolResultContent, UserContent,
};
use rig_core::streaming::{Part, PartKind, StreamEvent};

/// The wire triple prior stamps on the turns it builds itself.
///
/// rig 0.45 keeps no message id: an assistant turn's provider identity lives
/// in [`AssistantMessage::origin`], whose `response_id` is what prior stores
/// as pi's `responseId`. The remaining fields name the wire/model that
/// produced the turn, and "prior" says exactly that — none of these turns
/// carries provider items, so rig always replays them canonically rather than
/// from a provider's own encoding.
const PRIOR_ORIGIN: &str = "prior";

/// An assistant turn carrying prior's own content and the provider's response
/// id, if any. `None` marks a turn prior built itself — an aborted or errored
/// partial turn, which is what pi's missing `responseId` means.
pub(crate) fn assistant_message(
    response_id: Option<String>,
    content: Vec<AssistantContent>,
) -> Message {
    let turn = AssistantMessage::new(content);
    Message::Assistant(match response_id {
        Some(id) => turn.with_origin(Origin {
            api: Api::from_static(PRIOR_ORIGIN),
            provider: PRIOR_ORIGIN.to_string(),
            model: PRIOR_ORIGIN.to_string(),
            response_model: None,
            response_id: Some(id),
            context: None,
        }),
        None => turn,
    })
}

/// The provider response id prior keeps for an assistant turn, i.e. pi's
/// `responseId`: `None` for a turn prior built itself (aborted/errored).
pub(crate) fn response_id(turn: &AssistantMessage) -> Option<&str> {
    turn.origin
        .as_ref()
        .and_then(|origin| origin.response_id.as_deref())
}

/// A [`ToolName`] from `name`. Every name prior holds is a literal or one a
/// model emitted in a call that was already decoded, so the empty case cannot
/// occur; an empty one would still be representable, so it is mapped to a
/// stand-in rather than panicking mid-turn.
pub(crate) fn tool_name(name: &str) -> ToolName {
    ToolName::new(name).unwrap_or_else(|_| ToolName::new("tool").expect("literal is not empty"))
}

/// A tool call identified by its wire id: the shape prior holds in storage,
/// from pi's JSONL, and in its tests.
pub(crate) fn tool_call(
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> rig_core::message::ToolCall {
    rig_core::message::ToolCall::new(
        CallId::from_wire(id),
        ToolFunction::new(tool_name(name), arguments),
    )
}

/// A tool result answering `call` with one literal text part.
#[cfg(test)]
pub(crate) fn text_tool_result(call: CallId, name: &str, text: impl Into<String>) -> ToolResult {
    ToolResult {
        call,
        name: tool_name(name),
        content: vec![ToolResultContent::Text(Text::new(text))],
        is_error: false,
    }
}

/// The content blocks of an assistant message, or an empty slice for any other
/// role.
pub(crate) fn assistant_content(msg: &Message) -> &[AssistantContent] {
    match msg {
        Message::Assistant(turn) => &turn.content,
        _ => &[],
    }
}

/// Whether the run is still going (a completion-call boundary) or has ended
/// (a turn exit). It is the one thing `prepare` reads to decide whether an
/// unanswered call may be persisted.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunState {
    /// A completion call is about to be dispatched; the run continues. A
    /// call without a result is a violation, never a call "about to run".
    Ongoing,
    /// The turn is ending — normal, aborted, or errored. Every unanswered call
    /// is answered with a "did not execute" result so the tail stays valid.
    Ended,
}

/// One completion call's live unit: the assistant half as it streams, the
/// accepted turn once rig commits it, and the results that answer it.
///
/// Separate fields keep **order observed rather than assumed**: a provider may
/// interleave text and tool calls (Gemini, Ollama), so a flat `String` per kind
/// would silently reorder a turn. A part's `End` supersedes its fragments, and
/// `accepted` supersedes the whole `streaming` list wholesale.
#[derive(Default)]
pub(crate) struct LiveUnit {
    /// Parts in provider order, keyed by their stream `Part`. A part's `End`
    /// supersedes its fragments; fragments survive only for a part that never
    /// ended (a cut stream).
    parts: Vec<LivePart>,
    /// `event.content` from `on_model_turn_finished`: canonical, provider
    /// order, supersedes `streaming` wholesale.
    accepted: Option<Message>,
    /// One `ToolResult` per answered call of the batch.
    results: Vec<UserContent>,
}

struct LivePart {
    part: Part,
    kind: Option<PartKind>,
    /// Fragments for a part that never ended (text/reasoning/argument JSON).
    fragments: String,
    /// The finalized content this part ended with.
    ended: Option<AssistantContent>,
}

impl LiveUnit {
    /// The part's slot, created on first mention so start order is preserved.
    fn slot(&mut self, part: Part) -> &mut LivePart {
        let idx = match self.parts.iter().position(|p| p.part == part) {
            Some(i) => i,
            None => {
                self.parts.push(LivePart {
                    part,
                    kind: None,
                    fragments: String::new(),
                    ended: None,
                });
                self.parts.len() - 1
            }
        };
        &mut self.parts[idx]
    }

    /// Whether the unit holds anything at all (its own content, not the
    /// transcript's already-persisted messages).
    fn is_empty(&self) -> bool {
        self.accepted.is_none()
            && self.results.is_empty()
            && self
                .parts
                .iter()
                .all(|p| p.ended.is_none() && p.fragments.is_empty())
    }

    /// Whether the unit is a complete pair: every tool call the assistant half
    /// carries has a result. A unit with no accepted turn but with results
    /// (`TurnAbandoned`) counts as complete — direction B rebuilds the
    /// assistant half from exactly those results, so it is answered by
    /// construction.
    fn answered_all(&self) -> bool {
        let calls: Vec<CallId> = match &self.accepted {
            Some(Message::Assistant(turn)) => turn
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(tc) => Some(tc.id.clone()),
                    _ => None,
                })
                .collect(),
            // No accepted turn: only the results are known. If there are any,
            // direction B makes a matching assistant; if there are none there
            // is nothing to answer.
            _ => return !self.results.is_empty(),
        };
        let answered: std::collections::HashSet<&CallId> = self
            .results
            .iter()
            .filter_map(|c| match c {
                UserContent::ToolResult(tr) => Some(&tr.call),
                _ => None,
            })
            .collect();
        calls.iter().all(|id| answered.contains(id))
    }

    /// The assistant half: `accepted` when present, else the streamed parts
    /// flattened in provider order. A tool-call part that never ended is
    /// dropped (its argument JSON is incomplete — a fabricated call would
    /// enter the session as a real, answered request for a tool that never
    /// ran). `None` when nothing visible streamed.
    fn finish(&self) -> Option<Message> {
        if let Some(turn) = &self.accepted {
            return Some(turn.clone());
        }
        let mut content: Vec<AssistantContent> = Vec::new();
        for part in &self.parts {
            if let Some(ended) = &part.ended {
                content.push(ended.clone());
                continue;
            }
            if part.fragments.is_empty() {
                continue;
            }
            match part.kind {
                Some(PartKind::Text) => {
                    content.push(AssistantContent::Text(Text::new(&part.fragments)));
                }
                Some(PartKind::Reasoning) => {
                    content.push(AssistantContent::Reasoning(
                        rig_core::message::Reasoning::new(&part.fragments),
                    ));
                }
                // An unfinished call, image or opaque part: nothing to write.
                _ => {}
            }
        }
        assistant_content_is_visible(&content).then(|| assistant_message(None, content))
    }
}

/// The canonical transcript of the in-flight run, and the incremental
/// persistence that turns each completion call into one durable batch.
///
/// `messages` holds only *prepared*, durable messages: every element is on
/// disk. The one mutable thing is the current [`LiveUnit`], which is not a
/// `Message` yet and never a half-unit — it is shaped by
/// [`prepare`](Self::prepare) before it enters `messages`.
#[derive(Default)]
pub(crate) struct TurnTranscript {
    /// Prepared messages: every element is on disk. Append-only, never
    /// rewritten.
    messages: Vec<Message>,
    /// The prepared batch awaiting its append; retried verbatim on failure.
    prepared: Option<PreparedBatch>,
    /// The current completion call: the only accumulating state.
    live: LiveUnit,
}

/// A whole completion turn ready to persist: one to three messages plus the
/// batch id seed, so entry ids are `{seed}-{i}` and a retried append is
/// idempotent instead of a duplicated turn.
#[derive(Clone)]
pub(crate) struct PreparedBatch {
    pub(crate) messages: Vec<Message>,
    pub(crate) seed: uuid::Uuid,
}

impl TurnTranscript {
    /// Begin a new completion call: the live unit is empty for it.
    pub(crate) fn start_live(&mut self) {
        self.live = LiveUnit::default();
    }

    /// Feed one streamed part event into the live unit. Each fragment exists
    /// once, in the current poll's `StreamAssistantItem`, so the loop is the
    /// single drain site — the hook only ever sees the accepted turn.
    pub(crate) fn push_event(&mut self, event: &StreamEvent) {
        let part = event.part();
        let lp = self.live.slot(part);
        match event {
            StreamEvent::Start { kind, name, .. } => {
                lp.kind = Some(*kind);
                let _ = name;
            }
            StreamEvent::Text { text, .. } | StreamEvent::Reasoning { text, .. } => {
                lp.fragments.push_str(text);
            }
            StreamEvent::Arguments { json, .. } => lp.fragments.push_str(json),
            StreamEvent::End { content, .. } => lp.ended = Some(content.clone()),
        }
    }

    /// Record the accepted assistant turn from `on_model_turn_finished`.
    /// Empty turns (curated out by rig) are dropped; either way the streamed
    /// fragments are superseded.
    pub(crate) fn accept(&mut self, response_id: Option<String>, content: Vec<AssistantContent>) {
        self.live.parts.clear();
        self.live.accepted =
            assistant_content_is_visible(&content).then(|| assistant_message(response_id, content));
    }

    /// Accumulate one tool result of the batch.
    pub(crate) fn push_tool_result(&mut self, result: rig_core::message::ToolResult) {
        self.live.results.push(UserContent::ToolResult(result));
    }

    /// Whether there is anything to write (drives the back-off reset): any
    /// persisted message, a pending batch, or live content.
    pub(crate) fn has_content(&self) -> bool {
        !self.messages.is_empty() || self.prepared.is_some() || !self.live.is_empty()
    }

    /// Shape the live unit into a batch, in memory only. Retry-first: while a
    /// prepared batch is pending (a failed append), it is returned unchanged,
    /// so a later turn cannot overtake it and ordering on disk is preserved.
    /// `None` when there is nothing to write.
    ///
    /// `state` decides whether an unanswered call is filled with a "did not
    /// execute" result. At a completion-call boundary the run is ongoing: a
    /// dangling call there is a violation to log, never a call "about to run",
    /// so the unit is left in place and not written (it is written at the next
    /// boundary or the exit, once its results have arrived). At an exit it is
    /// `Ended` and every unanswered call is answered — the tail must stay a
    /// valid prefix.
    pub(crate) fn prepare(&mut self, state: RunState) -> Option<PreparedBatch> {
        if let Some(batch) = &self.prepared {
            return Some(batch.clone());
        }
        if self.live.is_empty() {
            return None;
        }
        if state == RunState::Ongoing && !self.live.answered_all() {
            // Mid-run: a call without a result is not yet a complete unit.
            return None;
        }
        let live = std::mem::take(&mut self.live);
        let assistant = live.finish();
        let results = live.results;

        // Direction B: results with no accepted turn (`TurnAbandoned`, a
        // repaired-name turn) — rebuild the assistant half from what each
        // result carries (call id + name, args `{}`).
        let assistant = match assistant {
            Some(a) => a,
            None if !results.is_empty() => assistant_from_results(&results),
            // Nothing visible streamed and no results: a whitespace-only cut.
            // There is nothing to write (an empty assistant message would be
            // noise); drop the unit.
            None => return None,
        };
        // Two steps, and they answer different questions.
        //
        // First the call *as the tool read it*: `preprocess_args` for a tool the
        // table governs, which is the same function the dispatch label used, so
        // the panel and the record cannot disagree. A call that never ran gets
        // nothing — there is no tool to ask, and the model's spelling stands.
        let assistant = preprocess_calls(assistant);
        // Then the parts: a tool that returned one part per file becomes one
        // call per file, each with its own result. A call nothing answered, or
        // answered with plain text, is untouched.
        let (assistant, results) = crate::agent::parts::unpack(assistant, results);

        let messages = assemble(assistant, results, state);
        let batch = PreparedBatch {
            messages,
            seed: uuid::Uuid::now_v7(),
        };
        self.prepared = Some(batch.clone());
        Some(batch)
    }

    /// Fold the prepared batch into `messages` after a successful append.
    fn mark_committed(&mut self) {
        if let Some(batch) = self.prepared.take() {
            self.messages.extend(batch.messages);
        }
    }
}

/// Persist the transcript's pending batch to `session`, if any.
///
/// A free async fn because a `std::sync::Mutex` guard cannot cross the
/// `await`: prepare under the lock, append with the lock dropped, re-lock to
/// fold. Failure leaves the transcript untouched (the batch stays `prepared`)
/// so a retry re-emits a byte-identical batch under the same ids — idempotent,
/// not a duplicated turn — and is logged rather than swallowed.
pub(crate) async fn commit(
    transcript: &std::sync::Mutex<TurnTranscript>,
    session: Option<&crate::session::Session>,
    state: RunState,
) -> crate::Result<()> {
    // Loop so a pending batch (a failed append) is written first, then any
    // live content that accumulated meanwhile — preserving order. In the
    // common case live is empty at the boundary and this runs once.
    loop {
        let batch = {
            let mut t = transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            t.prepare(state)
        };
        let Some(batch) = batch else {
            return Ok(());
        };
        // The append is the only session-gated step. Without a session the
        // batch is still folded into `messages`, so `has_content` reflects the
        // progress a sessionless run made (it drives the back-off reset).
        if let Some(session) = session {
            if let Err(e) = session.append_messages(&batch.messages, &batch.seed).await {
                eprintln!("Warning: failed to persist session batch: {e}");
                return Err(e);
            }
        }
        let mut t = transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        t.mark_committed();
    }
}

// ---------------------------------------------------------------------------
// Load-path repair
// ---------------------------------------------------------------------------

/// Repair call/result pairing on a *loaded* `Vec<Message>` (pure, idempotent).
///
/// The write path never needs this — `prepare()` shapes both halves of the
/// live unit together, so a half-unit never reaches `messages`. This is for
/// what a file already holds: a crash residue or a legacy session.
///
/// - **Direction A** — an assistant turn with unanswered calls (a torn write
///   between the call entry and its result entries): answers each with the
///   synthesized "did not execute" result, matching rig's `NO_RESULT_PROVIDED`.
/// - **Direction B** — `ToolResult` items whose `CallId` appears in no
///   assistant part: inserts a synthesized assistant turn, rebuilt from each
///   result's `call` + `name` (arguments `{}`), *before* the results.
pub(crate) fn repair_pairing(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg {
            Message::User { content } => {
                // Direction B: answer orphan results (their `CallId` is in no
                // preceding assistant turn) with a synthesized assistant turn
                // placed *before* them, rebuilt from the results themselves.
                let known: std::collections::HashSet<CallId> = out
                    .last()
                    .map(|m| {
                        assistant_content(m)
                            .iter()
                            .filter_map(|c| match c {
                                AssistantContent::ToolCall(tc) => Some(tc.id.clone()),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let orphans: Vec<UserContent> = content
                    .iter()
                    .filter(|c| match c {
                        UserContent::ToolResult(tr) => !known.contains(&tr.call),
                        _ => false,
                    })
                    .cloned()
                    .collect();
                if !orphans.is_empty() {
                    out.push(assistant_from_results(&orphans));
                }
                out.push(Message::User { content });
            }
            other => out.push(other),
        }
    }
    // Direction A: answer a trailing assistant turn's unanswered calls. The
    // tail may be the assistant alone (torn before any result) or the
    // assistant followed by a *partial* result batch; either way, any call id
    // no result answers gets a synthesized "did not execute" result.
    let asst_idx = out.iter().rposition(|m| matches!(m, Message::Assistant(_)));
    if let Some(idx) = asst_idx {
        let calls: Vec<rig_core::message::ToolCall> = assistant_content(&out[idx])
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect();
        // Results only count if nothing intervenes between the assistant and
        // them (a later assistant turn starts a new unit).
        let results_start = idx + 1;
        let answered: std::collections::HashSet<CallId> = out[results_start..]
            .iter()
            .filter_map(|m| match m {
                Message::User { content } => Some(content.as_slice()),
                _ => None,
            })
            .flatten()
            .filter_map(|c| match c {
                UserContent::ToolResult(tr) => Some(tr.call.clone()),
                _ => None,
            })
            .collect();
        let missing: Vec<UserContent> = calls
            .into_iter()
            .filter(|tc| !answered.contains(&tc.id))
            .map(|tc| UserContent::ToolResult(synthesized_result(&tc)))
            .collect();
        if !missing.is_empty() {
            // Extend a trailing results message, or append a new one.
            match out.last_mut() {
                Some(Message::User { content }) => content.extend(missing),
                _ => out.push(Message::User { content: missing }),
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Batch shaping
// ---------------------------------------------------------------------------

/// Rebuild the assistant half from the results that answered it, for a turn
/// rig committed without firing `on_model_turn_finished` (`TurnAbandoned`,
/// a repaired-name turn). A `ToolResult` carries both `call` and `name`, so
/// the ids and tools are exact; the arguments are lost and left `{}`.
fn assistant_from_results(results: &[UserContent]) -> Message {
    let content = results
        .iter()
        .filter_map(|c| match c {
            UserContent::ToolResult(tr) => Some(AssistantContent::ToolCall(
                rig_core::message::ToolCall::new(
                    tr.call.clone(),
                    ToolFunction::new(tr.name.clone(), serde_json::json!({})),
                ),
            )),
            _ => None,
        })
        .collect();
    assistant_message(None, content)
}

/// The "did not execute" result rig answers an unanswered call with
/// (`NO_RESULT_PROVIDED` semantics).
fn synthesized_result(tc: &rig_core::message::ToolCall) -> ToolResult {
    ToolResult {
        call: tc.id.clone(),
        name: tc.function.name.clone(),
        content: vec![ToolResultContent::text(format!(
            "[tool call `{}` was interrupted before execution; \
             it did not run. Continue from the conversation above.]",
            tc.function.name
        ))],
        is_error: false,
    }
}

/// Assemble `[assistant]` and, when there are results, `[assistant, user]`.
/// Every unanswered call is answered with a synthesized "did not execute"
/// result — safe at a boundary (there are none) and required at a run exit —
/// so the on-disk tail is always a valid prefix.
fn assemble(assistant: Message, mut results: Vec<UserContent>, state: RunState) -> Vec<Message> {
    if state == RunState::Ended {
        let answered: std::collections::HashSet<CallId> = results
            .iter()
            .filter_map(|c| match c {
                UserContent::ToolResult(tr) => Some(tr.call.clone()),
                _ => None,
            })
            .collect();
        let mut derived: Vec<UserContent> = Vec::new();
        for c in assistant_content(&assistant) {
            if let AssistantContent::ToolCall(tc) = c {
                if !answered.contains(&tc.id) {
                    derived.push(UserContent::ToolResult(synthesized_result(tc)));
                }
            }
        }
        results.extend(derived);
    }

    let mut out = vec![assistant];
    if !results.is_empty() {
        out.push(Message::User { content: results });
    }
    out
}

/// Replace each call's arguments with the form `preprocess_args` gives them.
///
/// The same function the dispatch label used, so the panel and the record read
/// one value rather than two rewrites that had to be proven to agree. A call the
/// table does not govern — an unknown tool, `edit`, or any call that never ran —
/// keeps the model's own bytes.
fn preprocess_calls(assistant: Message) -> Message {
    let ctx = crate::tool_args::PathContext::sandbox();
    let Message::Assistant(mut turn) = assistant else {
        return assistant;
    };
    let content: Vec<AssistantContent> = std::mem::take(&mut turn.content)
        .into_iter()
        .map(|c| match c {
            AssistantContent::ToolCall(tc) => {
                let args = tc.function.arguments_value();
                match crate::agent::tool_shape::preprocess(tc.function.name.as_str(), &args, &ctx) {
                    Some(rewritten) => {
                        // Rebuild around the new arguments, keeping the provider
                        // item: only the args changed, and a `native` that did
                        // not would make rig replay the old spelling.
                        let mut rebuilt = rig_core::message::ToolCall::new(
                            tc.id.clone(),
                            ToolFunction::new(tc.function.name.clone(), rewritten),
                        );
                        rebuilt.native = tc.native;
                        AssistantContent::ToolCall(rebuilt)
                    }
                    None => AssistantContent::ToolCall(tc),
                }
            }
            other => other,
        })
        .collect();
    Message::Assistant(turn.with_content(content))
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
    r.text.clone()
}

/// Render the request history the model is about to see.
///
/// The transcript is stored canonically — a pi-compatible `{path, edits:[…]}`
/// array per file — and the wire *accepts* that array too, but the schema does
/// not advertise it. So the history renders it back to the block form the model
/// is asked for, and the model sees **one** shape. That is still the right call
/// with two shapes accepted: the block form is the one weak harnesses can
/// encode, and a history that mixes both teaches the model to alternate.
///
/// Wire-only, which is why it lives here rather than in `prepare`: persistence
/// takes the array form (pi's canonical) and the request takes blocks, both from
/// the same messages, so neither has to know that the other shaped anything.
///
/// Two steps:
///
/// 1. [`crate::agent::tool_shape::preprocess`] — the `bash` prefix strip and
///    path relativization, the same function the dispatch label and the record
///    use. A loaded session's calls are already in this form, so it is a no-op
///    there.
/// 2. `edit` → block form, from the stored per-file arrays. A call already in
///    block form is returned unchanged, which is what makes a second pass a
///    byte-level no-op.
pub(crate) fn render_model_facing(mut messages: Vec<Message>) -> Vec<Message> {
    render_model_facing_with(&mut messages, &crate::tool_args::PathContext::sandbox());
    messages
}

/// [`render_model_facing`] against an explicit path context, so a test can pin
/// the rule without depending on where the binary happens to be running.
pub(crate) fn render_model_facing_with(
    messages: &mut [Message],
    ctx: &crate::tool_args::PathContext,
) {
    for message in messages.iter_mut() {
        let Message::Assistant(turn) = message else {
            continue;
        };
        for content in &mut turn.content {
            let AssistantContent::ToolCall(tc) = content else {
                continue;
            };
            let args = tc.function.arguments_value();
            let rendered = match tc.function.name.as_str() {
                "edit" => edit_blocks_of(&args)
                    .or_else(|| crate::agent::tool_shape::preprocess("edit", &args, ctx)),
                name => crate::agent::tool_shape::preprocess(name, &args, ctx),
            };
            if let Some(rendered) = rendered {
                tc.function = ToolFunction::new(tc.function.name.clone(), rendered);
            }
        }
    }
}

/// The block form of a stored `edit` call, or `None` when it is already blocks
/// (or is not a parsed array at all, in which case the model's own spelling is
/// what it wrote and the honest thing to send back).
///
/// `None` on the already-blocks arm is what makes this idempotent: the output of
/// this function is block form, so a second pass takes that arm and changes
/// nothing.
fn edit_blocks_of(args: &serde_json::Value) -> Option<serde_json::Value> {
    let files = bsed_blockedit::EditArgs::from_wire(args).ok()?.files;
    if files.is_empty() {
        return None;
    }
    let blocks: Vec<String> = files
        .iter()
        .map(|f| bsed_blockedit::render_blob(&f.path, &bsed_blockedit::to_parsed(&f.edits)))
        .collect();
    Some(serde_json::json!({ "blocks": blocks.join("\n\n") }))
}

/// The text of a tool result, as the model sees it.
#[cfg(test)]
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

/// Strip `AssistantContent::Reasoning` blocks based on the
/// `ReasoningInContextMode`. Returns messages unchanged when:
/// - Mode is `Always`
/// - Provider is Anthropic-family **and** the turn contains tool calls
///   (the thinking+tool_use replay contract)
///
/// When mode is `AbortedTurnsOnly`, reasoning is preserved only for the
/// *trailing run* of aborted assistant messages — those with `id: None`
/// (aborted/errored turns created locally without a provider responseId)
/// that follow the **last** completed turn. That window is the one the
/// reasoning exists for: the model continuing the thread it was aborted
/// mid-way through. Once a later user turn completes with a final response,
/// the abort it superseded is closed and its reasoning is stale context —
/// it is stripped like a normal turn's, saving the tokens on every request
/// for the rest of the session. Successive aborts with no completion
/// between them all keep theirs: several aborts in a row are still one
/// unfinished thread.
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
    // `AbortedTurnsOnly` keeps reasoning only for the *trailing* run of
    // aborted turns: those positioned after the last completed assistant
    // message (one carrying a provider responseId). An abort older than
    // that completion is closed — a later user turn ran to a final answer —
    // so its reasoning no longer helps and is stripped. `None` (no completed
    // turn anywhere yet) leaves every abort in the trailing run.
    let last_completed = messages.iter().rposition(|m| match m {
        Message::Assistant(turn) => response_id(turn).is_some(),
        _ => false,
    });
    messages
        .into_iter()
        .enumerate()
        .map(|(i, m)| match m {
            Message::Assistant(mut turn) => {
                // No provider responseId = aborted/errored turn.
                let is_aborted = response_id(&turn).is_none();
                let in_trailing_run = last_completed.is_none_or(|last| i > last);
                let kept: Vec<AssistantContent> = std::mem::take(&mut turn.content)
                    .into_iter()
                    .filter(|c| {
                        if keep_aborted
                            && is_aborted
                            && in_trailing_run
                            && matches!(c, AssistantContent::Reasoning(_))
                        {
                            true // Preserve reasoning for the trailing aborted run
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
                Message::Assistant(turn.with_content(content))
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
    messages.iter().any(|m| {
        assistant_content(m)
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)))
    })
}

/// Whether `provider`/`model` refers to an Anthropic-family model, for which
/// reasoning must be kept in context (the thinking+tool_use replay contract).
/// Covers direct `anthropic` and OpenRouter routing to `anthropic/*` models.
pub(crate) fn is_anthropic_family(provider: &str, model: &str) -> bool {
    provider.eq_ignore_ascii_case("anthropic")
        || (provider.eq_ignore_ascii_case("openrouter") && model.starts_with("anthropic/"))
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
    let cleaned = bsed_blockedit::strip_arg_error_prefixes(text);
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

    fn tool_calls_of(m: &Message) -> Vec<rig_core::message::ToolCall> {
        if !matches!(m, Message::Assistant(_)) {
            panic!("expected an assistant message")
        }
        assistant_content(m)
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

    /// The `blocks` string of the first call in `m`.
    fn blocks_of(m: &Message) -> String {
        tool_calls_of(m)[0].function.arguments["blocks"]
            .as_str()
            .expect("a block-form call")
            .to_string()
    }

    // ---- the parts model: unpack, preprocess, render ----
    //
    // The shape of a call is stated by the tool that ran it, so what is left to
    // test here is the framework's half: how a parts list becomes N pairs, how
    // a call's args are normalized, and how an `edit` array renders back to the
    // block form the model is asked for.

    /// A two-file `edit` result, as the tool answers it: one part per file, each
    /// with its own canonical args and its own result.
    fn edit_parts() -> Vec<crate::agent::parts::CallPart> {
        vec![
            crate::agent::parts::CallPart {
                args: serde_json::json!({
                    "path": "a.rs",
                    "edits": [{"oldText": "x", "newText": "X", "startLine": 1}],
                }),
                result: "Replaced 1 block(s) in a.rs".into(),
                is_error: false,
            },
            crate::agent::parts::CallPart {
                args: serde_json::json!({
                    "path": "b.rs",
                    "edits": [{"oldText": "y", "newText": "Y", "startLine": 3}],
                }),
                result: r#"{"ui_view":"edit failed: nope","model_view":"long diff"}"#.into(),
                is_error: true,
            },
        ]
    }

    /// A committed result carrying a parts list, which is what the loop writes
    /// into the run for a tool that answers more than one call.
    fn parts_result(
        call: &str,
        parts: &[crate::agent::parts::CallPart],
    ) -> rig_core::message::ToolResult {
        rig_core::message::ToolResult {
            call: CallId::from_wire(call),
            name: tool_name("edit"),
            content: vec![ToolResultContent::Json {
                value: serde_json::to_value(parts).expect("parts serialize"),
            }],
            is_error: false,
        }
    }

    #[test]
    fn unpack_splits_a_parts_list_into_one_pair_per_part() {
        let assistant = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "t1",
                "edit",
                serde_json::json!({ "blocks": "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END" }),
            ))],
        );
        let parts = edit_parts();
        let (assistant, results) = crate::agent::parts::unpack(
            assistant,
            vec![UserContent::ToolResult(parts_result("t1", &parts))],
        );

        // One call per part, each carrying that part's own args.
        let calls = tool_calls_of(&assistant);
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls.iter().map(|c| c.id.to_string()).collect::<Vec<_>>(),
            vec!["t1", "t1_f1n2"]
        );
        assert_eq!(calls[0].function.arguments["path"], "a.rs");
        assert_eq!(calls[1].function.arguments["path"], "b.rs");

        // One result per call, paired structurally — no length check.
        let results = results_of(&Message::User { content: results });
        assert_eq!(results.len(), 2);
        assert!(results[0].1.contains("Replaced 1 block(s) in a.rs"));
        assert!(results[1].1.contains("long diff"));
    }

    /// A part's `is_error` wins over the batch's: one file failing inside a
    /// successful call is that file's outcome, and the stored pair says so.
    #[test]
    fn unpack_carries_each_parts_disposition() {
        let assistant = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "t1",
                "edit",
                serde_json::json!({ "blocks": "a.rs" }),
            ))],
        );
        let (_, results) = crate::agent::parts::unpack(
            assistant,
            vec![UserContent::ToolResult(parts_result("t1", &edit_parts()))],
        );
        let flags: Vec<bool> = results
            .iter()
            .filter_map(|c| match c {
                UserContent::ToolResult(tr) => Some(tr.is_error),
                _ => None,
            })
            .collect();
        assert_eq!(flags, vec![false, true]);
    }

    /// A single part is not a split: the model's own call and the one result
    /// stand, so nothing is rewritten for the common case.
    #[test]
    fn a_single_part_call_is_left_as_it_is() {
        let assistant = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "t1",
                "edit",
                serde_json::json!({ "blocks": "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END" }),
            ))],
        );
        let one = &edit_parts()[..1];
        let (assistant, results) = crate::agent::parts::unpack(
            assistant,
            vec![UserContent::ToolResult(parts_result("t1", one))],
        );
        assert_eq!(tool_calls_of(&assistant).len(), 1);
        // The call keeps the arguments the model sent: nothing was re-derived.
        assert!(tool_calls_of(&assistant)[0]
            .function
            .arguments
            .get("blocks")
            .is_some());
        assert_eq!(results.len(), 1, "{results:?}");
        // And the committed result rides along untouched — the parts list
        // renders as its one result, which is byte-identical to today's text.
        let rendered = crate::agent::parts::parts_text(
            &crate::agent::parts::parts_of_message_result(match &results[0] {
                UserContent::ToolResult(tr) => tr,
                _ => panic!("a result"),
            })
            .expect("still a parts list"),
        );
        assert_eq!(rendered, "Replaced 1 block(s) in a.rs");
    }

    // ---- persist: the stored shape ----

    /// What reaches disk is one canonical array per file — pi's format — because
    /// unpacking happened before the batch was assembled.
    #[test]
    fn persist_stores_one_canonical_array_per_file() {
        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "t1",
                "edit",
                serde_json::json!({ "blocks": "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END" }),
            ))],
        );
        t.push_tool_result(parts_result("t1", &edit_parts()));
        let batch = t.prepare(RunState::Ongoing).expect("a batch");

        let calls = tool_calls_of(&batch.messages[0]);
        assert_eq!(calls.len(), 2);
        for (call, path) in calls.iter().zip(["a.rs", "b.rs"]) {
            assert_eq!(call.function.arguments["path"], path, "{:?}", call.function);
            assert!(call.function.arguments["edits"].is_array());
            assert!(call.function.arguments.get("blocks").is_none());
        }
        // Plain text per call: a parts list never reaches disk.
        let results = results_of(&batch.messages[1]);
        assert_eq!(results.len(), 2);
        assert!(!results[0].1.contains("prior_call"), "no envelope on disk");
        assert!(serde_json::from_str::<serde_json::Value>(&results[0].1).is_err());
    }

    /// A call the tool never ran keeps the args the model sent — there is no
    /// tool to ask, and inventing a shape would be worse than the model's own.
    #[test]
    fn persist_leaves_a_call_that_never_ran_alone() {
        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "d1",
                "bash",
                serde_json::json!({ "command": "rm -rf /" }),
            ))],
        );
        let batch = t.prepare(RunState::Ended).expect("a batch");
        let calls = tool_calls_of(&batch.messages[0]);
        assert_eq!(
            calls[0].function.arguments_value(),
            serde_json::json!({ "command": "rm -rf /" })
        );
        // `Ended` answers it with the "did not execute" result so the stored
        // tail stays a valid prefix.
        assert_eq!(results_of(&batch.messages[1]).len(), 1);
    }

    /// `preprocess_args` is what the record carries: the `bash` prefix goes and
    /// a path shortens, because the panel was labelled with exactly this.
    #[test]
    fn persist_records_the_call_as_preprocessed() {
        let cwd = std::env::current_dir().unwrap();
        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(
            None,
            vec![
                AssistantContent::ToolCall(tool_call(
                    "b1",
                    "bash",
                    serde_json::json!({ "command": format!("cd {} && ls", cwd.display()) }),
                )),
                AssistantContent::ToolCall(tool_call(
                    "r1",
                    "read",
                    serde_json::json!({ "path": format!("{}/src/x.rs", cwd.display()) }),
                )),
            ],
        );
        let batch = t.prepare(RunState::Ended).expect("a batch");
        let calls = tool_calls_of(&batch.messages[0]);
        assert_eq!(
            calls[0].function.arguments_value(),
            serde_json::json!({ "command": "ls" })
        );
        assert_eq!(
            calls[1].function.arguments_value(),
            serde_json::json!({ "path": "src/x.rs" })
        );
    }

    // ---- history: the model-facing shape ----

    #[test]
    fn history_renders_a_stored_array_as_an_anchored_block() {
        let stored = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "x1",
                "edit",
                serde_json::json!({
                    "path": "src/x.rs",
                    "edits": [{"oldText": "a", "newText": "b", "startLine": 12}],
                }),
            ))],
        );
        let out = render_model_facing(vec![stored]);
        assert_eq!(
            blocks_of(&out[0]),
            "src/x.rs:12\n@@@OLD\na\n@@@NEW\nb\n@@@END"
        );
        // The canonical array must not survive into the model-facing form.
        let calls = tool_calls_of(&out[0]);
        assert!(calls[0].function.arguments.get("edits").is_none());
        // Idempotent: rendering the block form again changes nothing.
        let twice = render_model_facing(out.clone());
        assert_eq!(twice, out);
    }

    #[test]
    fn history_relativizes_every_other_tool_too() {
        let msg = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "r1",
                "read",
                serde_json::json!({"path": "src/x.rs"}),
            ))],
        );
        // `src/x.rs` is already the stored form of a call in this run, so the
        // render is a no-op — which is what makes a loaded history safe to push
        // through the same function on every request.
        let out = render_model_facing(vec![msg]);
        assert_eq!(
            tool_calls_of(&out[0])[0].function.arguments_value(),
            serde_json::json!({"path": "src/x.rs"})
        );
    }

    /// A stored array that is already the projected form is not re-minimized or
    /// re-split: the two renderings are stable under a second request.
    #[test]
    fn history_is_stable_over_many_requests() {
        let stored = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "e1",
                "edit",
                serde_json::json!({
                    "path": "src/x.rs",
                    "edits": [{"oldText": "old", "newText": "new", "startLine": 40}],
                }),
            ))],
        );
        let once = render_model_facing(vec![stored]);
        let twice = render_model_facing(once.clone());
        let thrice = render_model_facing(twice.clone());
        assert_eq!(once, twice);
        assert_eq!(twice, thrice);
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
    // ---- TurnTranscript: live unit, prepare, commit batches ----
    //
    // The transcript is the persistence source of truth: one `LiveUnit` per
    // completion call is shaped by `prepare()` into a batch of one to three
    // messages, and `commit()` writes each completion turn rather than the
    // whole run. These tests pin the accumulation order, the batch shape, and
    // the no-truncation `Ended` synthesis.

    #[test]
    fn transcript_iteration_one_batch_per_completion_call() {
        use super::{RunState, TurnTranscript};
        use rig_core::message::{ToolCall, ToolFunction};

        let tc = ToolCall::from_wire(
            "call_1",
            ToolFunction::new(tool_name("read"), serde_json::json!({})),
        );
        let call_id = tc.id.clone();
        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(None, vec![AssistantContent::ToolCall(tc)]);
        t.push_tool_result(text_tool_result(call_id, "read", "ok"));
        // An `Ongoing` boundary (call start) writes the pair as one batch.
        let batch = t.prepare(RunState::Ongoing).expect("batch");
        assert_eq!(
            batch.messages.len(),
            2,
            "assistant + one user: {:?}",
            batch.messages
        );
        assert!(
            matches!(&batch.messages[1], Message::User { .. }),
            "batch must be one user message"
        );
    }

    #[test]
    fn transcript_strips_a_redundant_cd_prefix_from_bash_calls() {
        use super::{RunState, TurnTranscript};
        use rig_core::message::{ToolCall, ToolFunction};

        let cwd = std::env::current_dir().unwrap();
        let tc = ToolCall::from_wire(
            "call_1",
            ToolFunction::new(
                tool_name("bash"),
                serde_json::json!({ "command": format!("cd {} && ls", cwd.display()) }),
            ),
        );
        let call_id = tc.id.clone();
        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(None, vec![AssistantContent::ToolCall(tc)]);
        t.push_tool_result(text_tool_result(call_id, "bash", "ok"));
        let batch = t.prepare(RunState::Ongoing).expect("batch");
        // The persisted call is the compact form, matching what the shell ran
        // and what the next turn is shown.
        let calls = tool_calls_of(&batch.messages[0]);
        assert_eq!(
            calls[0].function.arguments_value()["command"],
            serde_json::json!("ls")
        );
    }

    #[test]
    fn transcript_ended_synthesizes_interrupted_calls() {
        use super::{RunState, TurnTranscript};
        use rig_core::message::{ToolCall, ToolFunction, ToolResultContent, UserContent};

        let tc = ToolCall::from_wire(
            "call_1",
            ToolFunction::new(tool_name("read"), serde_json::json!({})),
        );
        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(None, vec![AssistantContent::ToolCall(tc)]);
        // Interrupted before any result: `Ended` synthesizes one per call.
        let batch = t.prepare(RunState::Ended).expect("batch");
        assert_eq!(batch.messages.len(), 2, "assistant + synthesized user");
        let Message::User { content } = &batch.messages[1] else {
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
    fn transcript_ongoing_leaves_dangling_call_unwritten() {
        use super::{RunState, TurnTranscript};
        use rig_core::message::{ToolCall, ToolFunction};

        let tc = ToolCall::from_wire(
            "call_1",
            ToolFunction::new(tool_name("read"), serde_json::json!({})),
        );
        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(None, vec![AssistantContent::ToolCall(tc)]);
        // No result yet: at an ongoing boundary the unit is incomplete, so
        // there is nothing to write (the tail never gets a dangling call).
        assert!(t.prepare(RunState::Ongoing).is_none());
    }

    #[test]
    fn transcript_skips_turns_without_visible_content() {
        use super::{RunState, TurnTranscript};

        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(None, vec![AssistantContent::text("   ")]);
        assert!(!t.has_content(), "blank-text turn is not content");
        assert!(t.prepare(RunState::Ended).is_none());
    }

    #[test]
    fn repair_pairing_answers_a_dangling_call() {
        // Direction A: an assistant turn with an unanswered call gets a
        // synthesized "did not execute" result, so the tail is a valid prefix.
        use super::repair_pairing;
        use rig_core::message::{ToolCall, ToolFunction};
        let tc = ToolCall::from_wire(
            "c1",
            ToolFunction::new(tool_name("read"), serde_json::json!({})),
        );
        let msgs = vec![assistant_message(
            None,
            vec![AssistantContent::ToolCall(tc)],
        )];
        let repaired = repair_pairing(msgs);
        assert_eq!(repaired.len(), 2, "assistant + synthesized results");
        let Message::User { content } = &repaired[1] else {
            panic!("expected user results");
        };
        assert!(matches!(content[0], UserContent::ToolResult(_)));
        // Idempotent: the repaired tail ends on results, so a second pass is a
        // no-op.
        assert_eq!(repair_pairing(repaired.clone()).len(), 2);
    }

    #[test]
    fn repair_pairing_rebuilds_an_orphan_results_assistant() {
        // Direction B: orphan results (no preceding assistant carried their id)
        // get a synthesized assistant turn placed before them.
        use super::repair_pairing;
        let orphan = Message::User {
            content: vec![UserContent::ToolResult(text_tool_result(
                rig_core::message::CallId::from_wire("c9"),
                "read",
                "ok",
            ))],
        };
        let repaired = repair_pairing(vec![orphan]);
        assert_eq!(repaired.len(), 2, "synthesized assistant + results");
        assert!(matches!(repaired[0], Message::Assistant(_)));
        let content = assistant_content(&repaired[0]);
        let AssistantContent::ToolCall(tc) = &content[0] else {
            panic!("expected a synthesized call");
        };
        assert_eq!(&*tc.id.wire(), "c9");
    }

    #[test]
    fn reasoning_text_extracts_text_block() {
        let r = Reasoning::new("think step by step");
        assert_eq!(reasoning_text(&r), "think step by step");
    }

    #[test]
    fn reasoning_text_extracts_text_with_signature() {
        // rig 0.45 keeps the provider's signature in the block's `native`
        // item; the text is the whole canonical payload.
        let r = Reasoning::new("with sig");
        assert_eq!(reasoning_text(&r), "with sig");
    }
    // `reasoning_in_context=Never` must drop reasoning blocks from assistant
    // messages for non-Anthropic providers, while keeping text (and keeping
    // reasoning entirely for Anthropic-family turns that involve tool calls, or
    // when the mode is Always). On an Anthropic turn *without* tool calls, the
    // mode is honored and reasoning is stripped.
    #[test]
    fn strips_reasoning_for_non_anthropic_when_never() {
        use crate::settings::ReasoningInContextMode;
        let assistant = assistant_message(
            None,
            vec![
                AssistantContent::Reasoning(Reasoning::new("hidden thought")),
                AssistantContent::Text(Text::new("answer")),
            ],
        );
        let msgs = vec![Message::user("q"), assistant];

        // Never + non-Anthropic: reasoning dropped, text preserved.
        let out = maybe_strip_reasoning(msgs.clone(), ReasoningInContextMode::Never, "openai", "");
        if let Message::Assistant(turn) = &out[1] {
            let content = &turn.content;
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
        if let Message::Assistant(turn) = &kept[1] {
            let content = &turn.content;
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
        if let Message::Assistant(turn) = &stripped[1] {
            let content = &turn.content;
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
    /// but strips from successful turns (id: Some). Here the abort trails the
    /// completion, so it is still the open thread and keeps its reasoning.
    #[test]
    fn aborted_turns_only_preserves_reasoning_for_aborted() {
        use crate::settings::ReasoningInContextMode;
        // Successful turn: has responseId
        let successful_assistant = assistant_message(
            Some("resp-123".into()),
            vec![
                AssistantContent::Reasoning(Reasoning::new("successful thought")),
                AssistantContent::Text(Text::new("successful answer")),
            ],
        );
        // Aborted turn: no responseId
        let aborted_assistant = assistant_message(
            None,
            vec![
                AssistantContent::Reasoning(Reasoning::new("aborted thought")),
                AssistantContent::Text(Text::new("aborted answer")),
            ],
        );
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
                Message::Assistant(turn) => Some(&turn.content),
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

    /// Once a later user turn completes, an abort it superseded is closed:
    /// its reasoning is stale context and must be stripped, while an abort
    /// trailing the completion (the still-open thread) keeps its reasoning.
    #[test]
    fn aborted_reasoning_stripped_once_a_later_turn_completes() {
        use crate::settings::ReasoningInContextMode;
        let aborted = |label: &str, thought: &str| {
            assistant_message(
                None,
                vec![
                    AssistantContent::Reasoning(Reasoning::new(thought)),
                    AssistantContent::Text(Text::new(label)),
                ],
            )
        };
        let completed = assistant_message(
            Some("resp-1".into()),
            vec![AssistantContent::Text(Text::new("final answer"))],
        );
        let msgs = vec![
            Message::user("q1"),
            aborted("abort-1", "stale thought"),
            Message::user("q2"),
            completed,
            Message::user("q3"),
            aborted("abort-2", "open thought"),
        ];

        let out =
            maybe_strip_reasoning(msgs, ReasoningInContextMode::AbortedTurnsOnly, "openai", "");
        let thought = |m: &Message| match m {
            Message::Assistant(turn) => turn.content.iter().find_map(|c| match c {
                AssistantContent::Reasoning(r) => Some(reasoning_text(r)),
                _ => None,
            }),
            _ => None,
        };
        let assistants: Vec<&Message> = out
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .collect();
        // [abort-1, completed, abort-2]
        assert_eq!(assistants.len(), 3);
        assert_eq!(
            thought(assistants[0]),
            None,
            "the abort before the completion is stale and must be stripped"
        );
        assert_eq!(
            thought(assistants[2]),
            Some("open thought".to_string()),
            "the abort trailing the completion is the still-open thread"
        );
        // The stale abort keeps its text, so the message stays well-formed.
        assert!(assistant_content(assistants[0])
            .iter()
            .any(|c| matches!(c, AssistantContent::Text(t) if t.text == "abort-1")));
    }

    /// Two successive aborts with no completion between them are still one
    /// unfinished thread: both keep their reasoning.
    #[test]
    fn successive_aborts_all_keep_their_reasoning() {
        use crate::settings::ReasoningInContextMode;
        let abort = |thought: &str| {
            assistant_message(
                None,
                vec![
                    AssistantContent::Reasoning(Reasoning::new(thought)),
                    AssistantContent::Text(Text::new("partial")),
                ],
            )
        };
        let msgs = vec![
            Message::user("q1"),
            abort("first abort"),
            Message::user("steer"),
            abort("second abort"),
            Message::user("steer again"),
        ];

        let out =
            maybe_strip_reasoning(msgs, ReasoningInContextMode::AbortedTurnsOnly, "openai", "");
        let kept: Vec<String> = out
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(turn) => Some(
                    turn.content
                        .iter()
                        .filter_map(|c| match c {
                            AssistantContent::Reasoning(r) => Some(reasoning_text(r)),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(kept, vec!["first abort", "second abort"]);
    }

    // Anthropic-family turn WITH a tool call: the leading reasoning must be
    // force-kept (thinking+tool_use replay contract), even with the flag off.
    #[test]
    fn anthropic_tool_call_turn_forces_reasoning_kept() {
        let assistant = assistant_message(
            None,
            vec![
                AssistantContent::Reasoning(Reasoning::new("hidden thought")),
                AssistantContent::Text(Text::new("answer")),
                AssistantContent::ToolCall(rig_core::message::ToolCall::new(
                    rig_core::message::CallId::from_wire("tool-1"),
                    rig_core::message::ToolFunction::new(
                        tool_name("Bash"),
                        serde_json::json!({"cmd": "ls"}),
                    ),
                )),
            ],
        );
        let msgs = vec![Message::user("q"), assistant];

        let forced = maybe_strip_reasoning(
            msgs,
            crate::settings::ReasoningInContextMode::Never,
            "anthropic",
            "claude-3-5-sonnet",
        );
        if let Message::Assistant(turn) = &forced[1] {
            let content = &turn.content;
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
