//! Persistence: a cursor over the run's committed log.
//!
//! `AgentRun`'s messages are the canonical history, append-only and
//! provider-exact, so persisting a turn is "project what was committed past the
//! cursor, append it, advance the cursor". No transcript is reassembled from
//! stream deltas, and the question "what survived the failure" does not arise:
//! the run is in hand, and the cursor simply does not advance over what it did
//! not write.
//!
//! ## The one thing the log cannot tell us
//!
//! rig commits a whole tool batch as **one** user message, only once every call
//! is answered. So a call that ran while its batch peers were pending is not in
//! the log yet — and if the turn is aborted first, its result would be lost.
//!
//! Losing it matters: a file that was really edited belongs in the record as
//! edited, not as "did not execute". So the loop reports each answer as it lands
//! ([`Persister::note_answer`]), and [`Persister::close`] writes the unfinished
//! batch from those answers plus rig's own "did not execute" text for the calls
//! that never ran.
//!
//! That makes the persist *boundary* the completed unit rather than the
//! individual answer: an early write would put a results message in the store
//! that the run has not produced. What is required is only that nothing is lost
//! when a turn ends early, and the held answers are what guarantee it.

use std::collections::HashMap;
use std::sync::Arc;

use rig_agent::run::AgentRun;
use rig_core::message::{AssistantContent, CallId, Message, UserContent};

use crate::agent::project;
use crate::session::Session;

/// Where persistence has got to, and what it is holding for the unit in flight.
pub(crate) struct Persister {
    session: Option<Arc<Session>>,
    /// Where a persist failure is reported. The error channel, not stderr: a
    /// raw-mode turn owns the screen through the draw pipeline, and a stray
    /// stderr line would corrupt the live frame and break the row accounting.
    content: Arc<crate::content::Content>,
    /// Index into the run's log: everything below it has been written.
    cursor: usize,
    /// The accepted assistant turn of the unit in flight, while its answers are
    /// still arriving. Held because `close` must be callable without the run: an
    /// abort drops the loop, and a `Drop` impl cannot await.
    tail: Option<Message>,
    /// Where that turn sits in the run's log. Once the cursor passes it the unit
    /// has been written and the held copy is redundant — clearing it there is
    /// what stops `close` re-writing a batch that already reached disk, which is
    /// the ordinary shape of a retry (batch completes, continuation fails, turn
    /// exits).
    tail_at: Option<usize>,
    /// The answers that landed for the batch in flight, by call id.
    answered: HashMap<CallId, UserContent>,
    /// A batch whose append failed, so a retry writes the same ids instead of
    /// duplicating the turn.
    unsaved: Option<(Vec<Message>, uuid::Uuid)>,

    /// Whether the model itself produced output on the attempt that just
    /// ended — text, reasoning or a tool call.
    ///
    /// This, not [`Self::wrote`], is what the back-off resets on. `wrote` says
    /// an append succeeded, which a held tool answer satisfies without the model
    /// having spoken and a reasoning-only stream does not; and progress is a
    /// property of the attempt, not of the store. Set by the loop when an
    /// attempt ends, because only the loop holds the streamed fold.
    produced: bool,
}

impl Persister {
    /// `run` seeds the cursor from
    /// [`projection_start`](AgentRun::projection_start): everything below it is
    /// the prompt (persisted at dispatch) or loaded history (already stored).
    pub(crate) fn new(
        run: &AgentRun,
        session: Option<Arc<Session>>,
        content: Arc<crate::content::Content>,
    ) -> Self {
        Self {
            session,
            content,
            cursor: run.projection_start(),
            tail: None,
            tail_at: None,
            answered: HashMap::new(),
            unsaved: None,
            produced: false,
        }
    }

    /// The run accepted an assistant turn. A tool-bearing one is held until its
    /// answers land, because the run commits a whole batch as one message and an
    /// abort in between would drop what already ran.
    /// `at` is the turn's index in the run's log, so the persister can tell when
    /// the cursor has moved past it.
    pub(crate) fn set_tail(&mut self, turn: Message, at: usize) {
        let calls_tools = matches!(&turn, Message::Assistant(t) if project::calls_a_tool(t));
        self.tail = calls_tools.then_some(turn);
        self.tail_at = calls_tools.then_some(at);
        self.answered.clear();
    }

    /// One tool answer of the batch in flight landed.
    ///
    /// Held rather than written: the run commits the batch as one message, and
    /// this is the copy that survives an abort before it does.
    pub(crate) fn note_answer(&mut self, result: &rig_core::message::ToolResult) {
        self.answered
            .insert(result.call.clone(), UserContent::ToolResult(result.clone()));
    }

    /// Write every complete unit committed since the last write.
    ///
    /// Called at a completion-call boundary and at the end of a turn. A failure
    /// leaves the cursor where it was, so the next call retries the same bytes
    /// under the same ids.
    pub(crate) async fn write_committed(&mut self, run: &AgentRun) {
        if !self.retry_unsaved().await {
            return;
        }
        let pending: Vec<Message> = run
            .messages()
            .get(self.cursor..)
            .unwrap_or_default()
            .iter()
            .map(|m| (**m).clone())
            .collect();
        let complete = project::complete_prefix_len(&pending);
        if complete == 0 {
            return;
        }
        let messages = project::project(&pending[..complete]);
        if self.append(messages).await {
            self.cursor += complete;
            // The run committed the held unit, so its held copy is now redundant
            // and `close` must not write it again. That matters because a turn
            // often ends *after* a written boundary (a tool batch completes, the
            // continuation fails, the turn exits) — the normal shape of a retry.
            if self.tail_at.is_some_and(|at| self.cursor > at) {
                self.tail = None;
                self.tail_at = None;
                self.answered.clear();
            }
        }
    }

    /// Close the turn: write the batch whose results never all landed, from the
    /// answers that did plus "did not execute" for the rest.
    ///
    /// This is the write-path half of the partial-batch story. The read path
    /// (`repair_pairing`) stays as the net for a process that died before it
    /// could run.
    pub(crate) async fn close(&mut self) {
        if !self.retry_unsaved().await {
            return;
        }
        self.tail_at = None;
        let Some(Message::Assistant(turn)) = self.tail.take() else {
            return;
        };
        let answers = std::mem::take(&mut self.answered);
        let messages = close_unit(turn, &answers);
        if messages.is_empty() {
            return;
        }
        let _ = self.append(messages).await;
    }

    /// Append one batch, keeping it for retry if the append fails.
    ///
    /// The seed is fixed per batch, so a retry writes identical entry ids and
    /// the session's index skips them rather than duplicating the turn.
    async fn append(&mut self, messages: Vec<Message>) -> bool {
        if messages.is_empty() {
            return true;
        }
        let seed = match &self.unsaved {
            Some((_, seed)) => *seed,
            None => uuid::Uuid::now_v7(),
        };
        let Some(session) = self.session.as_ref() else {
            // Sessionless: nothing to write, but nothing pending either.
            self.unsaved = None;
            return true;
        };
        match session.append_messages(&messages, &seed).await {
            Ok(()) => {
                self.unsaved = None;
                true
            }
            Err(error) => {
                self.content
                    .display_error(&format!("failed to persist the session: {error}"));
                self.unsaved = Some((messages, seed));
                false
            }
        }
    }

    /// Retry a batch whose append failed, reporting whether the way is clear.
    async fn retry_unsaved(&mut self) -> bool {
        match self.unsaved.clone() {
            Some((messages, _)) => self.append(messages).await,
            None => true,
        }
    }

    /// Record that the attempt which just ended emitted output of its own.
    ///
    /// Called from every path that ends an attempt — failure, abort, and the
    /// invalid-call rollback — so the answer covers the whole attempt rather
    /// than whichever path happened to notice.
    pub(crate) fn note_produced(&mut self, produced: bool) {
        self.produced |= produced;
    }

    /// Clear the progress record, once it has been read.
    ///
    /// Called *after* the back-off has consulted [`Self::made_progress`], never
    /// when a request goes out. The flag has to survive from one attempt to the
    /// next, because the attempt that makes progress is often not the one that
    /// fails: a tool-calling attempt succeeds, its continuation 429s instantly,
    /// and the reset is owed to the first. Clearing on dispatch would lose it.
    pub(crate) fn reset_progress(&mut self) {
        self.produced = false;
    }

    /// Whether the model produced anything since the last reset. Drives the
    /// back-off: a 429 before the first delta is a failure with nothing to show
    /// for it, while a stream that reasoned for thirty seconds and then
    /// truncated is not.
    ///
    /// Deliberately *not* `wrote || !answered.is_empty()`: a tool answer that
    /// never reached the model is not model output, and an append that
    /// succeeded says nothing about whether the model spoke.
    pub(crate) fn made_progress(&self) -> bool {
        self.produced
    }
}

/// The messages closing a unit whose batch never finished: the answers that
/// landed, and "did not execute" for the calls that did not.
///
/// **Call order, not answer order.** The record then reads like a fully answered
/// batch, which is what the model sees next turn — and it is what makes a
/// partially executed batch indistinguishable in shape from a complete one.
///
/// Empty when the turn calls no tool: a text-only turn has no results to close.
fn close_unit(
    turn: rig_core::message::AssistantMessage,
    answers: &HashMap<CallId, UserContent>,
) -> Vec<Message> {
    let content: Vec<UserContent> = turn
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::ToolCall(tc) => {
                Some(UserContent::ToolResult(match answers.get(&tc.id) {
                    Some(UserContent::ToolResult(tr)) => tr.clone(),
                    _ => crate::agent::transcript::synthesized_result(tc),
                }))
            }
            _ => None,
        })
        .collect();
    if content.is_empty() {
        return Vec::new();
    }
    project::project(&[Message::Assistant(turn), Message::User { content }])
}

#[cfg(test)]
mod tests {
    //! The requirement this module exists for: a turn that ends early — aborted,
    //! or failed mid-batch — still records the tool work that actually ran.
    //!
    //! Without the held answers, the close could only write "did not execute" for
    //! every call of the unfinished batch, telling the next turn that a file it
    //! edited was untouched.

    use super::*;
    use crate::agent::transcript::{assistant_message, tool_call};
    use rig_core::message::{CallId, Text, ToolResult, ToolResultContent};

    fn answer(call: &str, text: &str, is_error: bool) -> UserContent {
        UserContent::ToolResult(ToolResult {
            call: CallId::from_wire(call),
            name: crate::agent::transcript::tool_name("edit"),
            content: vec![ToolResultContent::Text(Text::new(text))],
            is_error,
        })
    }

    fn turn_of(ids: &[&str]) -> rig_core::message::AssistantMessage {
        let Message::Assistant(turn) = assistant_message(
            None,
            ids.iter()
                .map(|id| {
                    rig_core::message::AssistantContent::ToolCall(tool_call(
                        id,
                        "edit",
                        serde_json::json!({ "path": "a.rs" }),
                    ))
                })
                .collect(),
        ) else {
            unreachable!("assistant_message builds an assistant turn")
        };
        turn
    }

    fn results_of(messages: &[Message]) -> Vec<(String, String, bool)> {
        let Message::User { content } = &messages[1] else {
            panic!("a results message")
        };
        content
            .iter()
            .map(|c| {
                let UserContent::ToolResult(tr) = c else {
                    panic!("a tool result")
                };
                let ToolResultContent::Text(t) = &tr.content[0] else {
                    panic!("text content")
                };
                (tr.call.to_string(), t.text.clone(), tr.is_error)
            })
            .collect()
    }

    /// The whole point: one of three calls answered, the other two never ran.
    /// The record keeps the real result in call order and closes the rest
    /// honestly — so the next turn knows `a.rs` really was edited.
    #[test]
    fn a_partially_answered_batch_keeps_the_answers_that_landed() {
        let answers: HashMap<CallId, UserContent> = [(
            CallId::from_wire("c1"),
            answer("c1", "Replaced 1 block(s) in a.rs", false),
        )]
        .into_iter()
        .collect();

        let closed = close_unit(turn_of(&["c1", "c2", "c3"]), &answers);
        let results = results_of(&closed);
        assert_eq!(results.len(), 3, "one result per call, in call order");
        assert_eq!(results[0].0, "c1");
        assert_eq!(results[0].1, "Replaced 1 block(s) in a.rs");
        assert!(!results[0].2);
        assert_eq!(results[1].0, "c2");
        assert!(
            results[1].1.contains("interrupted"),
            "the call that never ran says so: {}",
            results[1].1
        );
        assert_eq!(results[2].0, "c3");
    }

    /// Answer order is not call order: a batch whose second call finished first
    /// still records in the order the model wrote the calls.
    #[test]
    fn results_follow_call_order_not_answer_order() {
        let answers: HashMap<CallId, UserContent> = [
            (CallId::from_wire("c2"), answer("c2", "second", false)),
            (CallId::from_wire("c1"), answer("c1", "first", false)),
        ]
        .into_iter()
        .collect();
        let results = results_of(&close_unit(turn_of(&["c1", "c2"]), &answers));
        assert_eq!(results[0], ("c1".to_string(), "first".to_string(), false));
        assert_eq!(results[1], ("c2".to_string(), "second".to_string(), false));
    }

    /// A per-file failure rides a successful call, and closing preserves it
    /// rather than flattening everything to the call's own disposition.
    #[test]
    fn a_failed_part_keeps_its_disposition() {
        let answers: HashMap<CallId, UserContent> = [(
            CallId::from_wire("c1"),
            answer("c1", "could not edit a.rs", true),
        )]
        .into_iter()
        .collect();
        let results = results_of(&close_unit(turn_of(&["c1"]), &answers));
        assert!(results[0].2, "the part's own is_error survives the close");
    }

    /// A unit the run committed and the cursor wrote past must not be closed a
    /// second time.
    ///
    /// This is the ordinary shape of a retried turn: a tool batch completes (so
    /// `write_committed` writes it at the next boundary) and the *continuation*
    /// then fails, so the turn exits and `close` runs. Without the clearing rule
    /// the close would append the same batch again — a duplicate turn in the
    /// session, doubling every tool result the model then reads back.
    #[test]
    fn a_written_unit_is_not_closed_again() {
        let mut p = Persister::new(
            &AgentRun::new(Message::user("go")),
            None,
            Arc::new(crate::content::Content::quiet()),
        );
        // The turn sits at index 3 of the log, and the cursor has written past
        // it (indices 0..=3 are on disk).
        p.set_tail(turn_of(&["c1"]).into(), 3);
        p.cursor = 4;
        // The rule `write_committed` applies once the cursor moves.
        if p.tail_at.is_some_and(|at| p.cursor > at) {
            p.tail = None;
            p.tail_at = None;
        }
        assert!(
            p.tail.is_none(),
            "the held copy must be dropped once written"
        );
    }

    /// The converse: a unit the cursor has *not* reached is still held, which is
    /// what lets an abort mid-batch lose nothing.
    #[test]
    fn an_unwritten_unit_is_still_held() {
        let mut p = Persister::new(
            &AgentRun::new(Message::user("go")),
            None,
            Arc::new(crate::content::Content::quiet()),
        );
        p.set_tail(turn_of(&["c1"]).into(), 3);
        p.cursor = 2;
        if p.tail_at.is_some_and(|at| p.cursor > at) {
            p.tail = None;
            p.tail_at = None;
        }
        assert!(p.tail.is_some(), "an unwritten unit stays held for `close`");
    }

    /// A text-only turn has nothing to close: no synthesized results, no message.
    #[test]
    fn a_text_only_turn_closes_to_nothing() {
        let Message::Assistant(turn) = assistant_message(
            None,
            vec![rig_core::message::AssistantContent::Text(Text::new("done"))],
        ) else {
            unreachable!()
        };
        assert!(close_unit(turn, &HashMap::new()).is_empty());
    }
}
