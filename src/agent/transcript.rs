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
use std::collections::HashMap;

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
    pub(crate) fn prepare(
        &mut self,
        ledger: &EditLedger,
        state: RunState,
    ) -> Option<PreparedBatch> {
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
        // The `bash` prefix strip and path relativization, so what gets
        // persisted is the compact form. `crate::tool_args` is the single
        // implementation the execution path and the model-facing history use,
        // so every view of one call agrees.
        let ctx = crate::tool_args::PathContext::sandbox();
        let mut assistant = assistant;
        crate::tool_args::normalize_in_message(&mut assistant, &ctx);

        // `edit` is the one tool whose stored shape differs from its argument
        // shape: one call per file, each a canonical array. The ledger holds
        // what `on_outcome` already prepared; a call that never got that far
        // (denied, interrupted, or synthesized from a result) is prepared
        // here instead, without resolutions or a result to split.
        let (assistant, results) = shape_edits(assistant, results, ledger, &ctx);

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
    ledger: &EditLedger,
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
            t.prepare(ledger, state)
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

/// Reshape `edit` calls (and their results) into the stored form: one call per
/// file, each a canonical `{path, edits:[…]}` array — and, when the call split,
/// one result per call.
///
/// A lookup, not a transform: `on_outcome` has already run the one producer
/// ([`prepare_edit_call`]) for every call that executed, so this reads the
/// [`EditLedger`]. A call with no entry — denied, interrupted, or synthesized
/// from a result — is prepared here without resolutions or a result, which
/// still relativizes and serializes it, so the stored form stays canonical
/// (`doc/261026_tool_path_relativization.md`, *One producing function*).
///
/// A multi-file call whose result is not a per-file array splits into nothing:
/// it stays one call carrying the joined block form, with its original result.
/// One call per file cannot be stored as arrays under one result, and a
/// partially-saved history must never show a split with a dangling result.
///
/// Non-`edit` calls pass through untouched (path arguments were already
/// rewritten by [`crate::tool_args::normalize_in_message`]).
fn shape_edits(
    assistant: Message,
    results: Vec<UserContent>,
    ledger: &EditLedger,
    ctx: &crate::tool_args::PathContext,
) -> (Message, Vec<UserContent>) {
    let Message::Assistant(turn) = assistant else {
        return (assistant, results);
    };
    if !assistant_content(&Message::Assistant(turn.clone()))
        .iter()
        .any(|c| matches!(c, AssistantContent::ToolCall(tc) if tc.function.name == "edit"))
    {
        return (Message::Assistant(turn), results);
    }
    let response = response_id(&turn).map(str::to_string);

    let mut result_by_id: HashMap<CallId, UserContent> = HashMap::new();
    let mut others: Vec<UserContent> = Vec::new();
    for c in results {
        match &c {
            UserContent::ToolResult(tr) => {
                result_by_id.insert(tr.call.clone(), c);
            }
            _ => others.push(c),
        }
    }

    let mut out_calls: Vec<AssistantContent> = Vec::new();
    let mut out_results: Vec<UserContent> = Vec::new();
    for c in turn.content {
        let AssistantContent::ToolCall(tc) = c else {
            out_calls.push(c);
            continue;
        };
        if tc.function.name != "edit" {
            if let Some(r) = result_by_id.remove(&tc.id) {
                out_results.push(r);
            }
            out_calls.push(AssistantContent::ToolCall(tc));
            continue;
        }

        let args = tc.function.arguments_value();
        let prepared = match ledger.get_prepared(tc.id.wire().as_ref()) {
            Some(p) => p.clone(),
            None => match prepare_edit_call(&tc.id, &args, None, None, ctx) {
                Some(p) => p,
                // Unparsable arguments: the call is not ours to reshape. Keep
                // the model's own bytes and its result.
                None => {
                    if let Some(r) = result_by_id.remove(&tc.id) {
                        out_results.push(r);
                    }
                    out_calls.push(AssistantContent::ToolCall(tc));
                    continue;
                }
            },
        };

        if prepared.splits() {
            let paired = result_by_id.remove(&tc.id);
            let results = prepared.results.clone().unwrap_or_default();
            for (i, id) in prepared.ids.iter().enumerate() {
                out_calls.push(AssistantContent::ToolCall(rig_core::message::ToolCall {
                    id: id.clone(),
                    function: ToolFunction::new(
                        tc.function.name.clone(),
                        prepared.arrays[i].clone(),
                    ),
                    // A synthesized call has no provider id or signature: the
                    // provider issued one of each, and N copies of one
                    // provider id would be unmatchable on a wire that keys
                    // results on it.
                    native: None,
                }));
                if let Some(result) = results.get(i) {
                    out_results.push(UserContent::ToolResult(ToolResult {
                        call: id.clone(),
                        name: tc.function.name.clone(),
                        content: vec![ToolResultContent::text(result.clone())],
                        is_error: false,
                    }));
                }
            }
            // Anything else the original result entry carried is dropped with
            // it: the array *is* the result, and its entries are now the
            // per-file results.
            let _ = paired;
            continue;
        }

        // One call, one result. A single-file call stores its canonical array;
        // a multi-file call that could not split stores the joined block form,
        // because the canonical array names exactly one path.
        let stored = if prepared.arrays.len() == 1 {
            prepared.arrays[0].clone()
        } else {
            serde_json::json!({ "blocks": prepared.blocks.join("\n\n") })
        };
        out_calls.push(AssistantContent::ToolCall(rig_core::message::ToolCall {
            id: tc.id.clone(),
            function: ToolFunction::new(tc.function.name.clone(), stored),
            native: tc.native.clone(),
        }));
        if let Some(r) = result_by_id.remove(&tc.id) {
            out_results.push(r);
        }
    }

    // Orphan results and non-tool content ride after in their original spirit;
    // `assemble` then checks for unanswered calls.
    out_results.extend(result_by_id.into_values());
    out_results.extend(others);

    (assistant_message(response, out_calls), out_results)
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

/// Render the request history the model is about to see: every tool call's
/// arguments relativized, and every `edit` call in the block form.
///
/// The transcript is stored canonically (a pi-compatible `edits` array) and the
/// wire *accepts* that array too, but the schema does not advertise it — so the
/// history renders it back to blocks and the model sees **one** shape. That is
/// still the right call with two shapes accepted: the block form is the one
/// weak harnesses can encode, and a history that mixes both teaches the model to
/// alternate between them.
///
/// Two passes, in this order:
///
/// 1. [`crate::tool_args::normalize_messages`] — the `bash` prefix strip and
///    path relativization, shared with every other egress.
/// 2. `edit` → block form: the ledger's prepared `blocks` for this run's calls,
///    else [`prepare_edit_call`] for a call the ledger never saw (a prior
///    session, or a rejected call stored untouched). The fallback is still the
///    one producer, so a call from an earlier session relativizes exactly as a
///    live one does, and because its paths are already relative on disk the
///    rewrite is a no-op.
///
/// Idempotent: a call already in block form is returned unchanged (this
/// function's own output is block form, so a second pass takes that arm), and
/// the ledger lookup means a second pass cannot minimize a block twice.
pub(crate) fn render_model_facing(
    mut messages: Vec<Message>,
    ledger: &EditLedger,
    ctx: &crate::tool_args::PathContext,
) -> Vec<Message> {
    crate::tool_args::normalize_messages(&mut messages, ctx);
    messages
        .into_iter()
        .map(|m| match m {
            Message::Assistant(mut turn) => {
                let content = std::mem::take(&mut turn.content)
                    .into_iter()
                    .map(|c| match c {
                        AssistantContent::ToolCall(tc) if tc.function.name == "edit" => {
                            let mut tc = tc;
                            let args = tc.function.arguments_value();
                            let value = match ledger
                                .get_prepared(tc.id.wire().as_ref())
                                .filter(|p| !p.blocks.is_empty())
                            {
                                Some(prepared) => Some(serde_json::json!({
                                    "blocks": prepared.blocks.join("\n\n"),
                                })),
                                // Already in block form: a prior session's
                                // call, or this function's own output. Nothing
                                // to do — and saying so is what makes a second
                                // pass a byte-level no-op.
                                // No ledger entry (a prior session's call, a
                                // rejected one stored untouched): the one parse
                                // renders it, and relativizes it. Idempotent —
                                // the output is block form, and parsing a
                                // relativized block reproduces it byte for
                                // byte — so a session rendered twice is stable.
                                None => prepare_edit_call(&tc.id, &args, None, None, ctx)
                                    .filter(|p| !p.blocks.is_empty())
                                    .map(|p| {
                                        serde_json::json!({
                                            "blocks": p.blocks.join("\n\n"),
                                        })
                                    }),
                            };
                            if let Some(value) = value {
                                tc.function = ToolFunction::new(tc.function.name.clone(), value);
                            }
                            AssistantContent::ToolCall(tc)
                        }
                        other => other,
                    })
                    .collect();
                Message::Assistant(turn.with_content(content))
            }
            other => other,
        })
        .collect()
}

/// The per-run state the `edit` tool's one producer writes and every egress
/// reads.
///
/// Two maps, deliberately not one: a *resolutions* entry is what execution
/// reported (where each edit landed), and a *prepared* entry is the finished
/// product of [`prepare_edit_call`]. `on_outcome` inserts the first, then
/// prepares and inserts the second from it, so the only consumer of
/// `resolutions` is the producer itself; everything downstream looks up
/// `prepared`.
///
/// In-run and host-only: it holds file paths, line numbers and rendered
/// arguments, never file contents, and is never serialized. It is cloned per
/// hook call (a snapshot for the duration of one call), which is why both maps
/// hold owned values and nothing borrows.
#[derive(Default, Clone)]
pub(crate) struct EditLedger {
    /// Resolved placements for a successful `edit` call, keyed by the durable
    /// tool-call id. Written by `on_outcome` from the tool's host-only
    /// [`crate::tools::EditResolutions`] before the call is prepared.
    resolutions: HashMap<String, crate::tools::EditResolutions>,
    /// The prepared form of an `edit` call: canonical arrays for persist and
    /// display, block form for the model's history, and the split ids/results
    /// when the call names several files.
    prepared: HashMap<String, PreparedEdit>,
}

impl EditLedger {
    /// Record where a successful call's edits landed.
    pub(crate) fn insert_resolutions(
        &mut self,
        id: impl Into<String>,
        resolutions: crate::tools::EditResolutions,
    ) {
        self.resolutions.insert(id.into(), resolutions);
    }

    /// The placements recorded for `id`, if execution reported any.
    pub(crate) fn get_resolutions(&self, id: &str) -> Option<&crate::tools::EditResolutions> {
        self.resolutions.get(id)
    }

    /// Store a call's prepared form.
    pub(crate) fn insert_prepared(&mut self, id: impl Into<String>, prepared: PreparedEdit) {
        self.prepared.insert(id.into(), prepared);
    }

    /// The prepared form of `id`, if this run produced one.
    pub(crate) fn get_prepared(&self, id: &str) -> Option<&PreparedEdit> {
        self.prepared.get(id)
    }
}

/// One `edit` call, parsed once and rendered into every form its consumers
/// need.
///
/// The single owner of the call's shape: [`prepare_edit_call`] builds it, and
/// each egress picks a field rather than re-deriving one. Because all of them
/// come off the same `files`, the persisted array, the displayed panel and the
/// model's block cannot disagree about a path or an anchor.
#[derive(Clone)]
pub(crate) struct PreparedEdit {
    /// `{path, edits:[…]}` per file → persisted and displayed.
    pub(crate) arrays: Vec<serde_json::Value>,
    /// `render_blob` per file → the model-facing block form.
    pub(crate) blocks: Vec<String>,
    /// One result string per file, paired to `arrays` by index. `Some` only
    /// when the call split, i.e. it names several files *and* its result is a
    /// per-file array of the same length.
    pub(crate) results: Option<Vec<String>>,
    /// `split_id` per file: the original id for the first file, derived for the
    /// rest. So `ids[0]` is the id the provider issued, and a non-split call
    /// keeps it by construction.
    pub(crate) ids: Vec<CallId>,
}

impl PreparedEdit {
    /// Whether this call becomes N per-file calls (it has more than one file
    /// *and* a result to pair each one with).
    pub(crate) fn splits(&self) -> bool {
        self.results.is_some() && self.ids.len() > 1
    }
}

/// The one producer for an `edit` call: parse once, then render every form its
/// consumers need.
///
/// Called by `LoggingHook::on_outcome`, which is the only place holding all
/// three inputs — the call id, the raw arguments plus the host-only
/// `EditResolutions`, and the result text — and which runs before both the
/// persist and the history egresses. (The id is a parameter rather than a
/// record inside `args`: a tool-call id is rig's, not the argument JSON's.) Those then look the result up in the [`EditLedger`] instead of
/// re-parsing (see `doc/261026_tool_path_relativization.md`).
///
/// The steps are ordered because each depends on the previous one:
///
/// 1. [`bsed_blockedit::EditArgs::from_wire`] — **the** only parse of an
///    `edit` call's arguments. Unparsable arguments yield `None`: the call is
///    not ours to reshape, and every egress falls back to the model's own
///    spelling.
/// 2. Minimize per file against `resolutions` (a no-op when absent). A file
///    with no recorded placement keeps its edits verbatim — no resolved line
///    means no honest anchor.
/// 3. Relativize each file's path. This is where the one path rule enters an
///    `edit` call; `tool_args::normalize` deliberately skips `edit` for
///    exactly this reason.
/// 4. Arrays, 5. blocks, 6. results: three renderings of the same `files`.
///
/// `results` is `Some` only when the result text parses as a per-file array
/// *and* its length matches the file count — the pairing guard, so a
/// partially-written history never shows a split with a dangling result.
/// `None` means "one call, one result": the original result rides the joined
/// block form untouched.
pub(crate) fn prepare_edit_call(
    call_id: &CallId,
    args: &serde_json::Value,
    resolutions: Option<&crate::tools::EditResolutions>,
    result_text: Option<&str>,
    ctx: &crate::tool_args::PathContext,
) -> Option<PreparedEdit> {
    let mut files = bsed_blockedit::EditArgs::from_wire(args).ok()?.files;
    minimize_files(&mut files, resolutions);
    relativize_files(&mut files, ctx);

    let arrays: Vec<serde_json::Value> = files
        .iter()
        .map(|f| bsed_blockedit::edits_to_canonical_value(&f.path, &f.edits))
        .collect();
    let blocks: Vec<String> = files
        .iter()
        .map(|f| bsed_blockedit::render_blob(&f.path, &bsed_blockedit::to_parsed(&f.edits)))
        .collect();
    let results = result_text.and_then(|text| paired_results(text, files.len()));
    // Derived, never minted: the persist and history egresses each split their
    // own half of the conversation and must arrive at the same id for the same
    // file. The producer is now the only caller, which is why `split_id`'s
    // determinism is a stable-id convention rather than a correctness
    // requirement.
    let ids = (0..files.len())
        .map(|i| crate::edit_blocks::split_id(call_id, i, files.len()))
        .collect();

    Some(PreparedEdit {
        arrays,
        blocks,
        results,
        ids,
    })
}

/// Minimize every file that has a recorded placement, in place.
///
/// Today's `minimized_edit_args` loop, operating on the canonical `FileEdit`
/// rather than rebuilding `ParsedEdit`s only to render them. A file whose path
/// is not in `resolutions` — a failed file in a mixed batch, or any file when
/// execution reported nothing — keeps its edits verbatim: without a resolved
/// line there is no honest anchor.
///
/// Idempotent, which is why the result may be prepared more than once: an
/// already-anchored edit's own anchor is its base, so re-minimizing it is a
/// no-op.
fn minimize_files(
    files: &mut [bsed_blockedit::FileEdit],
    resolutions: Option<&crate::tools::EditResolutions>,
) {
    let Some(resolutions) = resolutions else {
        return;
    };
    for file in files.iter_mut() {
        let Some(resolved) = resolutions.files.iter().find(|r| r.path == file.path) else {
            continue;
        };
        if resolved.matched_lines.len() != file.edits.len() {
            continue;
        }
        // The ledger's resolved line is for edits that arrived unanchored,
        // where only execution knows the placement; an anchored edit is
        // already the trimmed form, so its own anchor is the base.
        file.edits = file
            .edits
            .iter()
            .zip(&resolved.matched_lines)
            .flat_map(|(edit, line)| {
                let base = edit.start_line.map(|s| s.0 as usize).unwrap_or(*line);
                // One input edit may split into several anchored sub-edits at
                // long internal unchanged runs.
                bsed_blockedit::split_and_minimize(edit, base)
            })
            .collect();
    }
}

/// Relativize each file's path with the one path rule, in place.
fn relativize_files(files: &mut [bsed_blockedit::FileEdit], ctx: &crate::tool_args::PathContext) {
    for file in files.iter_mut() {
        if let Some(rel) = crate::tool_args::relativize_arg(&file.path, ctx) {
            file.path = rel;
        }
    }
}

/// The result text split into one string per file, or `None` when it is not a
/// per-file array of exactly `n` entries.
///
/// The length check is the pairing guard: the array is built in the same order
/// the files were applied, so equal lengths mean index `i` is file `i`'s
/// outcome. A mismatch means a corrupt or partially-written array, and leaving
/// the call whole is the safe reading — better one call with one result than a
/// split with a dangling one.
fn paired_results(result_text: &str, n: usize) -> Option<Vec<String>> {
    let parsed = bsed_blockedit::parse_file_results(result_text)?;
    (parsed.len() == n).then(|| {
        parsed
            .iter()
            .map(|f| match &f.result {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect()
    })
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

    /// The path context the `edit` tests relativize against: cwd `/proj`, home
    /// `/home/u`, so `/proj/src/x.rs` → `src/x.rs` and `~/notes.txt` stays.
    fn ctx() -> crate::tool_args::PathContext {
        crate::tool_args::PathContext::new("/proj", Some("/home/u".into()))
    }

    /// A three-file call plus its array result, as the stored history holds it.
    fn multi_file_history() -> Vec<Message> {
        let blocks = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                      b.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END\n\n\
                      c.rs\n@@@OLD\nq\n@@@NEW\nQ\n@@@END";
        let result = bsed_blockedit::render_file_results(&[
            bsed_blockedit::FileResult {
                path: "a.rs".into(),
                result: serde_json::json!("Replaced 1 block(s) in a.rs"),
            },
            bsed_blockedit::FileResult {
                path: "b.rs".into(),
                result: serde_json::json!({
                    "ui_view": "edit failed: nope", "model_view": "long diff"
                }),
            },
            bsed_blockedit::FileResult {
                path: "c.rs".into(),
                result: serde_json::json!("Replaced 1 block(s) in c.rs"),
            },
        ]);
        vec![
            assistant_message(
                None,
                vec![AssistantContent::ToolCall(tool_call(
                    "t1",
                    "edit",
                    serde_json::json!({ "blocks": blocks }),
                ))],
            ),
            Message::User {
                content: vec![UserContent::ToolResult(text_tool_result(
                    CallId::from_wire("t1"),
                    "edit",
                    result,
                ))],
            },
        ]
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

    // ---- prepare_edit_call: the one producer ----

    #[test]
    fn a_single_file_call_produces_one_array_and_one_block() {
        let args = serde_json::json!({"blocks": "/proj/src/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END"});
        let p = prepare_edit_call(&CallId::from_wire("e1"), &args, None, None, &ctx())
            .expect("a parseable call");
        assert_eq!(
            p.arrays,
            vec![serde_json::json!({
                "path": "src/x.rs",
                "edits": [{"oldText": "a", "newText": "b"}],
            })]
        );
        assert_eq!(
            p.blocks,
            vec!["src/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END".to_string()]
        );
        // No result text: nothing to pair, so the call cannot split.
        assert!(p.results.is_none());
        assert!(!p.splits());
        assert_eq!(p.ids.len(), 1);
        assert_eq!(&*p.ids[0].wire(), "e1");
    }

    #[test]
    fn a_multi_file_call_splits_when_its_result_array_matches() {
        let history = multi_file_history();
        let args = tool_calls_of(&history[0])[0].function.arguments_value();
        let result = tool_result_text(&match &history[1] {
            Message::User { content } => match &content[0] {
                UserContent::ToolResult(tr) => tr.clone(),
                _ => panic!("a result"),
            },
            _ => panic!("a user message"),
        })
        .unwrap();
        let p = prepare_edit_call(&CallId::from_wire("t1"), &args, None, Some(&result), &ctx())
            .expect("a parseable call");
        assert_eq!(p.arrays.len(), 3);
        assert!(p.splits());
        assert_eq!(
            p.ids.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
            vec!["t1", "t1_f1n3", "t1_f2n3"]
        );
        // Each result rides its own call by index.
        let results = p.results.clone().expect("paired results");
        assert_eq!(results.len(), 3);
        assert!(results[0].contains("Replaced 1 block(s) in a.rs"));
        assert!(results[1].contains("long diff"), "{}", results[1]);
        assert!(results[2].contains("c.rs"));
    }

    #[test]
    fn a_multi_file_call_without_a_matching_result_does_not_split() {
        let args = serde_json::json!({"blocks": "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                                               b.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END"});
        let id = CallId::from_wire("t1");
        // A plain-text result (an argument error, or a call that errored before
        // producing the array) and a mismatched array both mean "one call".
        for result in [
            "the tool failed",
            r#"[{"path":"a.rs","result":"Replaced 1 block(s) in a.rs"}]"#,
        ] {
            let p = prepare_edit_call(&id, &args, None, Some(result), &ctx()).expect("a call");
            assert_eq!(p.arrays.len(), 2);
            assert!(p.results.is_none(), "{result}");
            assert!(!p.splits(), "{result}");
        }
    }

    #[test]
    fn only_the_resolved_file_of_a_mixed_batch_is_minimized() {
        use crate::tools::{EditResolutions, FileResolution};
        let args = serde_json::json!({"blocks": "a.rs\n@@@OLD\nkeep\nold\nalso_keep\n\
                                               @@@NEW\nkeep\nnew\nalso_keep\n@@@END\n\n\
                                               b.rs\n@@@OLD\nsame\n@@@NEW\nSAME\n@@@END"});
        let resolutions = EditResolutions {
            files: vec![FileResolution {
                path: "a.rs".into(),
                matched_lines: vec![40],
            }],
        };
        let p = prepare_edit_call(
            &CallId::from_wire("e1"),
            &args,
            Some(&resolutions),
            None,
            &ctx(),
        )
        .expect("a parseable call");
        // The resolved file is anchored where it applied, and trimmed to the
        // changed line plus enough context to stay a strong locator (`old` alone
        // is under the weak-locator floor, so `also_keep` is restored); the
        // unresolved one keeps its edits verbatim, with no anchor.
        assert_eq!(
            p.blocks[0],
            "a.rs:41\n@@@OLD\nold\nalso_keep\n@@@NEW\nnew\nalso_keep\n@@@END"
        );
        assert_eq!(p.blocks[1], "b.rs\n@@@OLD\nsame\n@@@NEW\nSAME\n@@@END");
    }

    #[test]
    fn a_long_internal_gap_splits_into_anchored_sub_blocks() {
        use crate::tools::{EditResolutions, FileResolution};
        let args = serde_json::json!({
            "blocks": "a.rs\n@@@OLD\nalpha_context_line\nold_x_context_line\n\
                       bravo_context_line\ncharlie_line\ndelta_context_line\n\
                       old_y_context_line\necho_context_line\n@@@NEW\nalpha_context_line\n\
                       new_x_context_line\nbravo_context_line\ncharlie_line\n\
                       delta_context_line\nnew_y_context_line\necho_context_line\n@@@END"
        });
        let resolutions = EditResolutions {
            files: vec![FileResolution {
                path: "a.rs".into(),
                matched_lines: vec![10],
            }],
        };
        let p = prepare_edit_call(
            &CallId::from_wire("e1"),
            &args,
            Some(&resolutions),
            None,
            &ctx(),
        )
        .expect("a call");
        assert_eq!(
            p.blocks[0],
            "a.rs:11\n@@@OLD\nold_x_context_line\n@@@NEW\nnew_x_context_line\n@@@END\n\n\
             a.rs:15\n@@@OLD\nold_y_context_line\n@@@NEW\nnew_y_context_line\n@@@END"
        );
    }

    #[test]
    fn preparing_twice_is_byte_stable() {
        use crate::tools::{EditResolutions, FileResolution};
        let args = serde_json::json!({"blocks": "a.rs\n@@@OLD\nkeep\nold\nkeep2\n\
                                               @@@NEW\nkeep\nnew\nkeep2\n@@@END"});
        let resolutions = EditResolutions {
            files: vec![FileResolution {
                path: "a.rs".into(),
                matched_lines: vec![40],
            }],
        };
        let id = CallId::from_wire("e1");
        let once = prepare_edit_call(&id, &args, Some(&resolutions), None, &ctx()).expect("a call");
        // The second pass starts from the first's own block form: an anchored
        // edit's anchor is its base, so minimizing again is a no-op.
        let twice = prepare_edit_call(
            &id,
            &serde_json::json!({ "blocks": once.blocks[0] }),
            Some(&resolutions),
            None,
            &ctx(),
        )
        .expect("a call");
        assert_eq!(once.arrays, twice.arrays);
        assert_eq!(once.blocks, twice.blocks);
    }

    #[test]
    fn unparsable_arguments_produce_nothing() {
        let args = serde_json::json!({"blocks": "@@@OLD\na\n@@@END"});
        assert!(prepare_edit_call(&CallId::from_wire("e1"), &args, None, None, &ctx()).is_none());
    }

    #[test]
    fn arrays_and_blocks_describe_the_same_files() {
        let args = serde_json::json!({"blocks": "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                                               b.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END"});
        let p =
            prepare_edit_call(&CallId::from_wire("e1"), &args, None, None, &ctx()).expect("a call");
        assert_eq!(p.arrays.len(), p.blocks.len());
        for (array, block) in p.arrays.iter().zip(&p.blocks) {
            let path = array["path"].as_str().unwrap();
            assert!(block.starts_with(path), "{block}");
            // The block re-parses to the array's edits, anchors included.
            let reparsed =
                bsed_blockedit::canonicalize_edit_value(&serde_json::json!({ "blocks": block }));
            assert_eq!(&reparsed, array);
        }
    }

    // ---- persist: the stored shape ----

    #[test]
    fn persist_splits_a_prepared_multi_file_call_into_canonical_arrays() {
        let history = multi_file_history();
        let mut ledger = EditLedger::default();
        let args = tool_calls_of(&history[0])[0].function.arguments_value();
        let result = results_of(&history[1])[0].1.clone();
        let prepared =
            prepare_edit_call(&CallId::from_wire("t1"), &args, None, Some(&result), &ctx())
                .expect("a call");
        ledger.insert_prepared("t1", prepared);

        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(None, assistant_content(&history[0]).to_vec());
        t.push_tool_result(text_tool_result(CallId::from_wire("t1"), "edit", result));
        let batch = t.prepare(&ledger, RunState::Ongoing).expect("a batch");

        let calls = tool_calls_of(&batch.messages[0]);
        assert_eq!(calls.len(), 3);
        // Stored as canonical arrays, one path per call, in call order.
        for (call, path) in calls.iter().zip(["a.rs", "b.rs", "c.rs"]) {
            assert_eq!(call.function.arguments["path"], path, "{:?}", call.function);
            assert!(call.function.arguments["edits"].is_array());
            assert!(call.function.arguments.get("blocks").is_none());
        }
        // And the results pair by the derived ids.
        let results = results_of(&batch.messages[1]);
        assert_eq!(
            results
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["t1", "t1_f1n3", "t1_f2n3"]
        );
        assert!(results[1].1.contains("long diff"));
    }

    #[test]
    fn persist_prepares_a_call_the_ledger_never_saw() {
        // Denied or interrupted: `on_outcome` never ran, so there is no ledger
        // entry. The stored form must still be the canonical array, relativized
        // against the sandbox context `prepare` builds (hence a real cwd path).
        let cwd = std::env::current_dir().unwrap();
        let call = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "s1",
                "edit",
                serde_json::json!({
                    "blocks": format!(
                        "{}/solo.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END",
                        cwd.display()
                    ),
                }),
            ))],
        );
        let result = Message::User {
            content: vec![UserContent::ToolResult(text_tool_result(
                CallId::from_wire("s1"),
                "edit",
                "Replaced 1 block(s) in solo.rs",
            ))],
        };
        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(None, assistant_content(&call).to_vec());
        for c in assistant_content(&result) {
            let _ = c;
        }
        t.push_tool_result(text_tool_result(
            CallId::from_wire("s1"),
            "edit",
            "Replaced 1 block(s) in solo.rs",
        ));
        let batch = t
            .prepare(&EditLedger::default(), RunState::Ongoing)
            .expect("a batch");
        let calls = tool_calls_of(&batch.messages[0]);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].function.arguments_value(),
            serde_json::json!({
                "path": "solo.rs",
                "edits": [{"oldText": "x", "newText": "X"}],
            })
        );
        // Its original result rides along untouched.
        let results = results_of(&batch.messages[1]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "Replaced 1 block(s) in solo.rs");
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
        let out = render_model_facing(vec![stored], &EditLedger::default(), &ctx());
        assert_eq!(
            blocks_of(&out[0]),
            "src/x.rs:12\n@@@OLD\na\n@@@NEW\nb\n@@@END"
        );
        // The canonical array must not survive into the model-facing form.
        let calls = tool_calls_of(&out[0]);
        assert!(calls[0].function.arguments.get("edits").is_none());
        // Idempotent: rendering the block form again changes nothing.
        let twice = render_model_facing(out.clone(), &EditLedger::default(), &ctx());
        assert_eq!(twice, out);
    }

    #[test]
    fn history_relativizes_a_call_with_no_ledger_entry() {
        // A prior session's raw blob: stored untouched by a rejected call, so
        // the ledger has nothing and the one parse produces the block.
        let blob = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "r1",
                "edit",
                serde_json::json!({"blocks": "/proj/src/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END"}),
            ))],
        );
        let out = render_model_facing(vec![blob], &EditLedger::default(), &ctx());
        assert_eq!(blocks_of(&out[0]), "src/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END");
    }

    #[test]
    fn history_uses_the_ledgers_prepared_blocks() {
        // The ledger's block is what the model sees, so a minimized call cannot
        // be re-derived (or re-minimized) from its stored array.
        let stored = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "e1",
                "edit",
                serde_json::json!({
                    "path": "src/x.rs",
                    "edits": [{"oldText": "old", "newText": "new"}],
                }),
            ))],
        );
        let mut ledger = EditLedger::default();
        let prepared = prepare_edit_call(
            &CallId::from_wire("e1"),
            &serde_json::json!({"blocks": "src/x.rs\n@@@OLD\nkeep\nold\nkeep2\n\
                                           @@@NEW\nkeep\nnew\nkeep2\n@@@END"}),
            Some(&crate::tools::EditResolutions {
                files: vec![crate::tools::FileResolution {
                    path: "src/x.rs".into(),
                    matched_lines: vec![40],
                }],
            }),
            None,
            &ctx(),
        )
        .expect("a call");
        ledger.insert_prepared("e1", prepared);
        // The `old` locator is under the weak-locator floor, so the minimizer
        // keeps the common context lines, and the anchor moves back with them.
        // The point of the test is *which* block this came from: the ledger's,
        // not one re-derived from the stored `{path, edits}` array.
        let out = render_model_facing(vec![stored], &ledger, &ctx());
        assert_eq!(
            blocks_of(&out[0]),
            "src/x.rs:40\n@@@OLD\nkeep\nold\nkeep2\n@@@NEW\nkeep\nnew\nkeep2\n@@@END"
        );
    }

    #[test]
    fn history_relativizes_every_other_tool_too() {
        let msg = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "r1",
                "read",
                serde_json::json!({"path": "/proj/src/x.rs"}),
            ))],
        );
        let out = render_model_facing(vec![msg], &EditLedger::default(), &ctx());
        assert_eq!(
            tool_calls_of(&out[0])[0].function.arguments_value(),
            serde_json::json!({"path": "src/x.rs"})
        );
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
        let anchors = EditLedger::default();
        let batch = t.prepare(&anchors, RunState::Ongoing).expect("batch");
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
        let batch = t
            .prepare(&EditLedger::default(), RunState::Ongoing)
            .expect("batch");
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
        let anchors = EditLedger::default();
        let batch = t.prepare(&anchors, RunState::Ended).expect("batch");
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
        let anchors = EditLedger::default();
        assert!(t.prepare(&anchors, RunState::Ongoing).is_none());
    }

    #[test]
    fn transcript_skips_turns_without_visible_content() {
        use super::{RunState, TurnTranscript};

        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(None, vec![AssistantContent::text("   ")]);
        let anchors = EditLedger::default();
        assert!(!t.has_content(), "blank-text turn is not content");
        assert!(t.prepare(&anchors, RunState::Ended).is_none());
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
