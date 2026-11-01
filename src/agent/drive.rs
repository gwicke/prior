//! The drive loop: prior owns rig's agent step protocol.
//!
//! `agent.prompt(..).stream()` builds an `AgentRun`, drives it to completion and
//! drops it, so a failed attempt is indistinguishable from a failed turn: the
//! run — its committed history, its turn budget, its pending call — is gone, and
//! the only way to retry is to re-dispatch the prompt. That is where the
//! synthetic `"continue"` messages came from, and it is why persistence had to
//! reassemble a turn from stream deltas and hook callbacks: prior could not see
//! what rig had committed.
//!
//! This module owns the loop instead. `AgentRun` stays the state machine; this
//! is the I/O around it, and the run stays in hand at every boundary:
//!
//! - a failed attempt is re-issued in place — `next_step` from `AwaitingModel`
//!   re-emits the identical `CallModel` — so a retry is the same turn, with no
//!   new user message and no budget consumed;
//! - its committed log is the persistence source, so nothing is reassembled
//!   from deltas;
//! - the request the model sees is a projection of that log, not the same
//!   bytes, which is what lets steers and settings ride a call boundary.
//!
//! See `doc/261030_rig_direct_drive.md`.
//!
//! ## Why a step API rather than one future
//!
//! [`DriveLoop::next`] yields one item at a time, the way rig's own `stream()`
//! does, because the caller must stay free between items: the turn pump selects
//! on the key queue, the confirm port and the cancel signal *while* a completion
//! streams. A driver that owned the whole turn would have to re-implement that
//! select, and it would lose the ability to drop a cancelled turn at an await
//! point rather than after it.
//!
//! Cancelling is therefore dropping the future returned by `next`, then asking
//! the loop for the history to persist: [`DriveLoop::cancelled`] builds the
//! same `PromptError` rig's hooks produced, from the run that is still here.
//!
//! ## The four callbacks
//!
//! [`Observer::before_call`] (cancel check, steering, request patch),
//! [`Observer::after_turn`] (paint and record the accepted turn),
//! [`Observer::dispatch`] / [`Observer::outcome`] (paint a tool call and its
//! result), and [`Observer::invalid_tool_call`] (the recovery decision).

// rig's `PromptError` carries the whole conversation in its error variants
// (a cancellation or budget error reports the history it stopped with), so
// every `Result<_, PromptError>` here is large. Boxing it would only move the
// cost to every `?` site and hide the fact that the value is big; the errors
// are cold paths, and the type is rig's, not ours to shrink.
#![allow(clippy::result_large_err)]

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::Mutex as AsyncMutex;

use futures::StreamExt;
use rig_agent::agent::{ModelHandle, MultiTurnStreamItem};
use rig_agent::completion::PromptError;
use rig_agent::run::{
    prepare_request, project, AgentRun, AgentRunStep, CommittedItem, PendingToolCall,
    PromptResponse, RequestPatch, StreamedResolution, StreamedTurnAssembler, StreamedTurnEvent,
    UnhandledInvalidToolCall,
};
use rig_agent::streaming::{CompletionStream, Item, StreamEvent};
use rig_core::completion::{CompletionRequest, Document, Message};
use rig_core::message::AssistantContent;
use rig_core::tool::ToolContext;

use crate::agent::parts::CallPart;
use crate::agent::persist::Persister;
use crate::agent::transcript::assistant_message;
use crate::content::Content;
use crate::session::Session;

/// What the drive loop needs from a `CodeAgent`: the model, the tools, the run
/// spec and the observer.
pub(crate) struct Drive {
    pub(crate) model: ModelHandle,
    pub(crate) tools: Arc<rig_agent::tool::ToolCatalog>,
    pub(crate) spec: rig_agent::run::spec::RunSpec,
    /// Owned rather than borrowed: the stream outlives the call that builds it.
    pub(crate) observer: Arc<dyn Observer>,
    /// Where committed units are written. `None` for a sessionless run, which
    /// still needs to know it made progress but has nowhere to put it.
    pub(crate) session: Option<Arc<Session>>,
    /// Where a persist failure is reported (`doc/261102_partial_turn_retention.md`
    /// §6a): the error channel, so a raw-mode turn's live frame is not corrupted
    /// by a stray stderr line.
    pub(crate) content: Arc<Content>,
    /// A one-slot mailbox from the retry loop in `turn.rs::prompt` to
    /// [`Observer::before_call`]: the corrective hint the *next* request should
    /// carry.
    ///
    /// It is a document ([`crate::agent::drive::context_document`]) rather than a
    /// message because it must steer the model without entering the transcript:
    /// a synthetic "your last tool call was malformed" line that became part of
    /// history would be re-read as a user instruction on every later turn.
    pub(crate) retry_hint: crate::agent::drive::RetryHint,
    /// The steer waiting to be delivered, and this turn's request-shaping
    /// params. Both are here rather than on [`DriveLoop`] because a rebuild
    /// replaces the loop's run and must not lose them: the slot is the turn's,
    /// and the sampling state carries the overrides a steer just contributed.
    pub(crate) steer: crate::agent::steer::SteerSlot,
    /// The REPL's input row, for the one thing a steer needs from it: clearing
    /// the text once the steer is delivered, and only when the row still holds
    /// what was scheduled.
    ///
    /// The editor rather than the whole [`crate::repl::PromptSession`]: this is
    /// already the shared handle the input *element* paints from (`Arc<Mutex<_>>`
    /// exists so the frame shows the live buffer), so it needs nothing else from
    /// the session and does not outlive the turn. `None` without a REPL — piped
    /// or batch input has no row to clear.
    pub(crate) input: Option<std::sync::Arc<std::sync::Mutex<crate::repl::LineEditor>>>,
    pub(crate) sampling: crate::agent::sampling::Sampling,
    /// How session history is built when a steer rebuilds the run: the same
    /// three values `before_call` renders with, so the rebuilt history and the
    /// request that follows it agree about what the model may see.
    pub(crate) reasoning_in_context: crate::settings::ReasoningInContextMode,
    pub(crate) provider: String,
    pub(crate) model_name: String,
}

/// Where a retry hint waits for the next request.
pub(crate) type RetryHint = Arc<std::sync::Mutex<Option<Document>>>;

/// The host's side of the loop: paint, persist, cancel, and the recovery
/// policies that used to be rig hooks.
///
/// Async because the persist boundary inside a batch is a session append; the
/// painting calls themselves are synchronous and await nothing.
#[async_trait::async_trait]
pub(crate) trait Observer: Send + Sync {
    /// Before a model call is prepared. `Err` stops the run with that reason;
    /// the [`RequestPatch`] replaces the run's history for this attempt, which
    /// is where the model-facing projection and mid-turn steering land.
    ///
    /// Also the incremental persist point: the turn that just finished is
    /// complete and pair-valid by now, so this is where it reaches disk.
    async fn before_call(
        &self,
        prompt: &Message,
        history: &[Arc<Message>],
        turn: usize,
    ) -> Result<Option<RequestPatch>, String>;

    /// The run accepted `content` as this turn's assistant message.
    async fn after_turn(&self, turn: usize, content: &[AssistantContent]);

    /// A tool call is about to run. `args` is the call as the tool reads it
    /// ([`crate::agent::tool_shape::preprocess`]), already computed by the loop,
    /// so the label and the record are one value rather than two rewrites that
    /// had to be proven to agree.
    fn dispatch(&self, name: &str, args: &serde_json::Value) -> Result<(), String>;

    /// A tool call answered: `parts` is the call as the tool understood it, with
    /// its result (one part per file for `edit`). The loop has already wrapped
    /// them for the run, so this is display only.
    fn outcome(&self, name: &str, parts: &[crate::agent::parts::CallPart]);

    /// The invalid-call decision for a call rig refused to dispatch as written.
    /// `None` leaves it to the run's `UnhandledInvalidToolCall` policy.
    fn invalid_tool_call(
        &self,
        context: &rig_agent::agent::InvalidToolCallContext,
    ) -> Option<rig_agent::agent::InvalidToolCallAction>;
}

/// One thing the loop produced.
pub(crate) enum DriveItem {
    /// A streamed item, as rig's own stream produced it.
    Stream(MultiTurnStreamItem),
    /// The run finished.
    Done(PromptResponse),
}

/// The in-flight model attempt: its stream, the assembler that owns the turn
/// being built, and the turn number for the callbacks.
///
/// The stream is held by value for the whole attempt, which is what makes
/// [`DriveLoop::next`] cancel-safe: dropping the future it returns mid-poll
/// drops only a borrow of `self`, so the next call resumes from the same
/// stream. The one narrow window is `finish`, which consumes the stream; it is
/// entered only after the stream returned `None`, so it completes on its first
/// poll without awaiting, and the loop sets [`Attempt::drained`] first so a
/// resumed call knows the fold is what remains.
struct Attempt {
    turn: usize,
    stream: CompletionStream,
    assembler: StreamedTurnAssembler,
    /// The stream returned `None`: only the terminal fold is left.
    drained: bool,
}

impl Attempt {
    /// Whether the fold so far holds anything the model itself emitted.
    ///
    /// Reasoning counts: it is output the user paid for and watched, and a
    /// provider that truncates after it is not a provider that refused the
    /// request.
    fn produced_output(&self) -> bool {
        self.stream.partial().choice.iter().any(|c| match c {
            AssistantContent::ToolCall(_) => true,
            AssistantContent::Text(t) => !t.text.is_empty(),
            // `redacted` reasoning carries no text by design, and is still
            // output the provider emitted.
            AssistantContent::Reasoning(r) => !r.text.is_empty() || r.redacted,
            _ => false,
        })
    }
}

/// What of an abandoned attempt's content is worth persisting, or `None`.
///
/// **Reasoning is kept.** Whether it reaches the *model* is
/// `reasoning_in_context`'s decision, applied at request time by
/// [`crate::agent::transcript::maybe_strip_reasoning`] — and its
/// `AbortedTurnsOnly` mode exists precisely for this case: the trailing run of
/// turns with no provider `responseId`, so the model can continue the thread it
/// was cut off in. Dropping it here would pre-empt that setting and silently
/// lose tokens the user paid for.
///
/// Tool calls are dropped, for a different reason: a call with no result is a
/// genuine pairing violation on the wire (`OrphanToolCall`), unlike the
/// reasoning case, which is only about what the model is shown. The retried
/// attempt re-issues whatever it still wants to call.
///
/// `None` when nothing of the model's own output survives — an attempt that
/// failed before its first delta produced nothing to keep, so the caller
/// appends no message.
fn keep_abandoned(content: Option<Vec<AssistantContent>>) -> Option<Vec<AssistantContent>> {
    let kept: Vec<AssistantContent> = content?
        .into_iter()
        .filter(|c| match c {
            AssistantContent::ToolCall(_) => false,
            AssistantContent::Text(t) => !t.text.is_empty(),
            AssistantContent::Reasoning(_) => true,
            _ => false,
        })
        .collect();
    (!kept.is_empty()).then_some(kept)
}

/// What one state-machine step did.
///
/// Boxed because `PromptResponse` carries the whole conversation while
/// `Advanced` carries nothing; without it every step moves the response's size
/// around the loop.
enum Step {
    /// The run advanced; another step (or the in-flight attempt) is next.
    Advanced,
    /// The run finished.
    Done(Box<PromptResponse>),
}

/// The step-wise driver. See the module docs for why it is not one future.
pub(crate) struct DriveLoop {
    drive: Drive,
    run: AgentRun,
    /// Where this run's own appends begin. Everything below is loaded history,
    /// already projected when it was written (doc §4).
    announce_cursor: usize,
    /// The turn budget the run was built with, and how many turns rebuilds have
    /// already spent. A rebuilt run starts its own `current_turn` at zero, so
    /// the budget has to be carried by hand for it to keep meaning anything
    /// (`doc/261102_partial_turn_retention.md` §6).
    max_turns: usize,
    turns_spent: usize,
    /// The writing half of the same idea: a cursor over the run's log, plus the
    /// answers of a batch whose commit has not landed yet.
    ///
    /// Shared, because the abort path drops this loop and the persister still
    /// has to be closed: an `Esc` in the middle of a tool batch must not lose
    /// the files that batch already edited.
    persist: Arc<AsyncMutex<Persister>>,
    /// Items ready to hand out before the next `await`.
    queue: VecDeque<MultiTurnStreamItem>,
    attempt: Option<Attempt>,
    /// Set once `Done` has been handed out. rig's `next_step` stays in its
    /// terminal state and keeps answering `Done`, so without this the loop
    /// would emit the final response forever.
    finished: bool,
    /// What a failed attempt had already produced when it died, kept because
    /// the run never sees it: `streamed_turn` runs only on a finished stream.
    ///
    /// The text is what the user watched arrive, so it has to reach the session
    /// (`doc/261102_partial_turn_retention.md`) — and the retried request has
    /// to carry it, or the model restarts from scratch instead of continuing.
    /// Tool calls are dropped: a call with no result is a pairing violation,
    /// unlike the retention case, which is only about a stop reason.
    abandoned: Option<Vec<AssistantContent>>,
}

impl DriveLoop {
    pub(crate) fn new(drive: Drive, run: AgentRun, max_turns: usize) -> (Self, PersistHandle) {
        let announce_cursor = run.projection_start();
        let persist = Arc::new(AsyncMutex::new(Persister::new(
            &run,
            drive.session.clone(),
            drive.content.clone(),
        )));
        let handle = PersistHandle(persist.clone());
        let this = Self {
            drive,
            run,
            announce_cursor,
            max_turns,
            turns_spent: 0,
            persist,
            queue: VecDeque::new(),
            attempt: None,
            finished: false,
            abandoned: None,
        };
        (this, handle)
    }

    /// Snapshot what the in-flight attempt has streamed so far.
    ///
    /// Used on the paths that stop the loop *without* an error — an `<Esc>`
    /// abort drops the stream mid-flight, so nothing has recorded the text the
    /// user watched arrive. Safe to call after the item stream is dropped: the
    /// attempt, and therefore the provider stream's fold, is still here. A no-op
    /// when the failure path already captured it, so the two cannot disagree.
    pub(crate) fn capture_abandoned(&mut self) {
        if self.abandoned.is_some() {
            return;
        }
        if let Some(attempt) = &self.attempt {
            let partial = attempt.stream.partial();
            self.abandoned = (!partial.choice.is_empty()).then_some(partial.choice);
        }
    }

    /// What the last failed attempt produced before it died, if anything.
    ///
    /// Taken, not read: the content becomes a message in the session exactly
    /// once, and a second read would append it twice.
    ///
    /// **Reasoning is kept.** Whether it reaches the model is
    /// `reasoning_in_context`'s decision, applied at request time by
    /// [`crate::agent::transcript::maybe_strip_reasoning`] — and its
    /// `AbortedTurnsOnly` mode exists precisely for this case: the trailing run
    /// of turns with no provider `responseId`, so the model can continue the
    /// thread it was cut off in. Dropping it here would pre-empt that setting
    /// and silently lose tokens the user paid for.
    ///
    /// Tool calls are dropped, and for a different reason: a call with no
    /// result is a genuine pairing violation on the wire (`OrphanToolCall`),
    /// unlike the reasoning case which is only about what the model is shown.
    /// The retried attempt re-issues whatever it still wants to call.
    ///
    /// `None` when nothing of the model's own output survives: an attempt that
    /// failed before its first delta produced nothing to keep.
    pub(crate) fn take_abandoned(&mut self) -> Option<Vec<AssistantContent>> {
        keep_abandoned(self.abandoned.take())
    }

    /// Rebuild the run around prior-controlled history, keeping everything else
    /// (spec, tools, observer, hint mailbox).
    ///
    /// A retry after a mid-stream failure cannot reuse the run: the abandoned
    /// text is not a turn rig will accept (`streamed_turn` requires a finished
    /// stream), and rig strips a failed turn from the wire below
    /// `prepare_request`, so the text cannot be replayed through the run's own
    /// history either. Rebuilding is the one shape that puts it back in front of
    /// the model (retention doc §2, §3).
    ///
    /// `history` is the session's exact history and **its last message is the
    /// prompt**: `call_model_step` splits the log's last message off as the
    /// prompt, so `[.. asst: partial] [user: continue]` is what makes the wire
    /// `[user: prompt] [asst: partial] [user: continue]`. Passing the partial as
    /// the prompt *and* in the history is rejected as consecutive assistant
    /// messages, which is why the split is done here rather than by the caller.
    ///
    /// This is the retry half; a steer rebuilds through the same function with
    /// one turn less charged ([`Self::rebuild_with`]), because its interrupted
    /// step never sent a request.
    ///
    /// The persister is **rebased, not replaced** (design §5a): the turn, the
    /// session and the progress flag are the same turn, so the `Arc` the pump
    /// holds stays the same one and a rebuild can happen inside
    /// [`Self::next`] with nobody told. `Persister::rebase` carries the one
    /// piece of genuinely run-relative state — where writing had got to — plus
    /// the held unit, which is closed by definition.
    ///
    /// Returns whether the history had a prompt to rebuild around; `false`
    /// leaves the run untouched.
    pub(crate) async fn rebuild_from(&mut self, history: Vec<Message>) -> bool {
        let spent = self.run.turn();
        self.rebuild_with(history, spent).await
    }

    /// [`Self::rebuild_from`], with the run's spent-turn count stated by the
    /// caller.
    ///
    /// A steer rebuilds at a `CallModel` whose request has **not** gone out yet:
    /// `next_step` already counted that turn, so charging it would make every
    /// steer cost a turn of the budget for work that never happened. The retry
    /// path has no such discount — its attempt was dispatched and billed.
    async fn rebuild_with(&mut self, mut history: Vec<Message>, spent: usize) -> bool {
        let Some(prompt) = history.pop() else {
            return false;
        };
        let mut spec = self.drive.spec.clone();
        // The turn budget is per *run*, and a rebuild makes a new one. Carrying
        // the spent turns across keeps a repeatedly failing provider from being
        // handed a fresh `tool_call_limit` on every attempt — the two budgets
        // would otherwise stop composing (retention doc §6).
        self.turns_spent += spent;
        spec.max_turns = Some(self.max_turns.saturating_sub(self.turns_spent).max(1));
        let run = AgentRun::from_spec(
            &spec,
            prompt,
            Some(history.into_iter().map(Arc::new).collect()),
        );
        self.run = run;
        self.announce_cursor = self.run.projection_start();
        self.attempt = None;
        self.finished = false;
        self.abandoned = None;
        self.queue.clear();
        self.persist.lock().await.rebase(&self.run);
        true
    }

    /// Deliver a scheduled steer, if there is one: write it to the session as an
    /// ordinary user message and rebuild the run around it.
    ///
    /// Returns whether the run was replaced. `false` covers both "nothing
    /// scheduled" and "no session to write into" — a sessionless run has no
    /// durable place for the message, so delivering it would put words in the
    /// transcript the record does not hold.
    ///
    /// The write comes first so the rebuild reads a session that already
    /// contains the steer; that is also what makes the steer survive a reload
    /// and a compaction, since it is a `role: user` message like any other.
    async fn deliver_steer(&mut self) -> bool {
        // Checked before taking: a slot left full is a steer the turn's exit
        // hands back to the input row. Taking it here without a session would
        // drop words the user typed and nothing else would notice.
        let Some(session) = self.drive.session.as_deref() else {
            return false;
        };
        let Some(scheduled) = self.drive.steer.take() else {
            return false;
        };
        let steer = scheduled.steer;
        // **The row is consumed before the paint, and the order is the point.**
        //
        // `display_committed_prompt` recomposes the frame, and the input element
        // decides its own caret from the buffer it holds at that moment: mid-turn
        // with an empty buffer it asks for no rows and renders no caret, so the
        // frame draws `HIDE_CURSOR` and the row is correctly bare. Clearing
        // *after* the paint instead would compose the frame while the buffer
        // still held the steer — one input row, with a caret sitting at its end —
        // and leave that caret on screen over a row the steer no longer occupies.
        //
        // The text stays in the editor until now so it is editable, and delivery
        // is what consumes it — **only if the row still holds exactly what was
        // scheduled**. Anything else there is newer work: the user edited this
        // steer after submitting it, or began typing the next message while the
        // boundary was pending. Clearing regardless would discard keystrokes
        // nothing else has a copy of, and the steer itself is already in
        // `scheduled`.
        commit_steer_to_screen(
            self.drive.input.as_deref(),
            &self.drive.content,
            &scheduled.typed,
            &steer.raw,
        );
        if let Err(error) = session.append_message(&Message::user(&steer.text)).await {
            self.drive
                .content
                .display_error(&format!("failed to persist the steering message: {error}"));
            return false;
        }
        // The steer's own override lines apply for the rest of the turn, and are
        // later than the prompt's — hence `note_overrides`, not a replacement.
        self.drive.sampling.note_overrides(steer.overrides);
        // Read back rather than splicing: a splice would be a one-shot the next
        // `CallModel` has to re-place, and the suffix would reorder as the turn
        // progresses (retention doc §3). The rebuild's last message is the
        // prompt, which for a steer is the steer itself.
        let history = match crate::agent::compaction::session_context_messages(
            session,
            self.drive.reasoning_in_context,
            &self.drive.provider,
            &self.drive.model_name,
        )
        .await
        {
            Ok(history) => history,
            Err(error) => {
                self.drive
                    .content
                    .display_error(&format!("failed to rebuild after steering: {error}"));
                return false;
            }
        };
        // `turn() - 1`: the step this steer interrupted counted its own turn and
        // never sent a request, so it must not be charged.
        let spent = self.run.turn().saturating_sub(1);
        self.rebuild_with(history, spent).await
    }

    /// The mailbox the next request's corrective hint is written into.
    pub(crate) fn retry_hint(&self) -> &RetryHint {
        &self.drive.retry_hint
    }

    /// Advance the loop by one item.
    ///
    /// Returns `None` once the run is done or has failed; the caller reads the
    /// outcome from [`Self::response`] or from the returned error.
    pub(crate) async fn next(&mut self) -> Option<Result<DriveItem, PromptError>> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                return Some(Ok(DriveItem::Stream(item)));
            }
            if self.finished {
                return None;
            }
            if self.attempt.is_some() {
                match self.advance_attempt().await {
                    Ok(()) => continue,
                    Err(error) => {
                        // Keep what the attempt had already produced: the run
                        // will never see it (a failed stream never reaches
                        // `streamed_turn`), and the user already watched it
                        // arrive. §2 of the retention doc is why this is taken
                        // here rather than from the run.
                        if let Some(attempt) = self.attempt.take() {
                            let partial = attempt.stream.partial();
                            let produced = attempt.produced_output();
                            self.abandoned = (!partial.choice.is_empty()).then_some(partial.choice);
                            self.persist.lock().await.note_produced(produced);
                        }
                        // The attempt is gone (taken above). A failed stream is
                        // dead and must not be polled again, but the *run* is
                        // untouched: `streamed_turn` never saw the turn, so it is
                        // still `AwaitingModel` and the next step re-issues the
                        // same `CallModel` — same turn, no budget, no new message.
                        // That is the whole of "a retry is the same attempt".
                        return Some(Err(error));
                    }
                }
            }
            match self.advance_step().await {
                Ok(Step::Advanced) => continue,
                Ok(Step::Done(response)) => return Some(Ok(DriveItem::Done(*response))),
                Err(error) => {
                    // A step-machine error is terminal: the run's own state went
                    // to `Failed` (or was never resumable), so re-issuing would
                    // only reproduce it. Stop rather than spin.
                    self.finished = true;
                    return Some(Err(error));
                }
            }
        }
    }

    /// Take one step of the run's state machine, queueing what it commits.
    async fn advance_step(&mut self) -> Result<Step, PromptError> {
        let step = self.run.next_step()?;
        self.project_committed();

        match step {
            AgentRunStep::CallModel {
                prompt,
                history,
                turn,
            } => {
                // The unit that just finished is complete and pair-valid, which
                // is what makes this the persist boundary: mid-turn progress
                // reaches disk before the next request goes out.
                self.persist.lock().await.write_committed(&self.run).await;
                // A steer enters here and nowhere else. This is the only point
                // in a turn where history is a valid provider prefix — the
                // assistant turn is committed and its results answered — so a
                // steer typed mid-batch is delivered *with* the results rather
                // than between them (`doc/261030_rig_direct_drive.md` §5).
                //
                // Delivering it means rebuilding the run around the session
                // ([`Self::deliver_steer`]), because rig's committed log cannot
                // be appended to. The rebuild is transparent to the caller: the
                // persister is rebased in place (§5a), so the pump keeps its one
                // handle, and `false` (no session) leaves the run untouched.
                if self.deliver_steer().await {
                    // The run was replaced; re-take its step so the request is
                    // prepared against the history that now includes the steer.
                    return Ok(Step::Advanced);
                }
                // Project what the model will see. The run's log holds the parts
                // lists this loop wrote into its answers, and a parts list must
                // never reach a provider. Projecting the *whole* history is safe:
                // a stored result is plain text (persistence wrote the parts
                // unpacked), so `unpack` finds nothing in loaded history and is a
                // byte-level no-op there.
                let history: Vec<Arc<Message>> = {
                    let owned: Vec<Message> = history.iter().map(|m| (**m).clone()).collect();
                    crate::agent::project::project(&owned)
                        .into_iter()
                        .map(Arc::new)
                        .collect()
                };
                let mut patch = match self
                    .drive
                    .observer
                    .before_call(&prompt, &history, turn)
                    .await
                {
                    Ok(patch) => patch,
                    Err(reason) => return Err(self.run.cancel_error(reason)),
                };
                // A retry's corrective hint rides this one request and is gone:
                // the observer shapes history, the loop owns what a retry says.
                // `take`, not `clone`, so a hint cannot outlive the attempt it
                // was written for.
                if let Some(hint) = self
                    .drive
                    .retry_hint
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    patch
                        .get_or_insert_with(RequestPatch::new)
                        .extra_context
                        .push(hint);
                }

                // This turn's sampling, recomposed now: a menu commit made while
                // the turn runs, and a steer's override lines, both reach the
                // *next* request through here and nowhere else
                // (`src/agent/sampling.rs`).
                self.drive.sampling.apply(&mut self.drive.spec);
                let prepared = prepare_request(
                    &self.drive.spec,
                    &self.drive.model.capabilities(),
                    &history,
                    self.drive.tools.definitions().to_vec(),
                    self.run.output_tool_name(),
                    patch.as_ref(),
                )
                .map_err(|error| PromptError::Provider(error.into()))?;
                let policy = prepared.policy.clone();
                // What the model was offered becomes run data, so a resumed run
                // can re-pair calls with the tools that answered them.
                self.run.advertise_tools(turn, prepared.tools.clone());

                let request =
                    prepared.apply(CompletionRequest::new(Arc::unwrap_or_clone(prompt.clone())));
                self.attempt = Some(Attempt {
                    turn,
                    stream: self.drive.model.stream(request),
                    assembler: StreamedTurnAssembler::new(policy),
                    drained: false,
                });
                Ok(Step::Advanced)
            }
            AgentRunStep::CallTools { calls } => {
                for call in calls {
                    self.answer_call(call).await?;
                }
                self.project_committed();
                Ok(Step::Advanced)
            }
            AgentRunStep::Done(response) => {
                // The last assistant turn is committed and complete, and it has
                // no following completion call to flush it. This is its write.
                self.persist.lock().await.write_committed(&self.run).await;
                self.persist.lock().await.close().await;
                // Hand the response straight out and mark the loop finished:
                // rig's terminal state keeps answering `Done`, so a second pass
                // would emit it forever.
                self.finished = true;
                self.queue.clear();
                Ok(Step::Done(Box::new(response)))
            }
        }
    }

    /// Take one item of the in-flight attempt.
    async fn advance_attempt(&mut self) -> Result<(), PromptError> {
        let Some(attempt) = &mut self.attempt else {
            return Ok(());
        };
        if attempt.drained {
            return self.finish_attempt().await;
        }
        let Some(item) = attempt.stream.next().await else {
            attempt.drained = true;
            return Ok(());
        };
        let item = item.map_err(PromptError::from)?;
        let event = attempt.assembler.ingest(&item).map_err(PromptError::from)?;

        // The fields the handlers need, borrowed disjointly so the assembler
        // and the run can be mutated together.
        let Self {
            drive,
            run,
            queue,
            attempt: Some(attempt),
            ..
        } = self
        else {
            unreachable!("checked above")
        };

        match event {
            StreamedTurnEvent::EmitIngested | StreamedTurnEvent::EmitToolCallDelta => {
                queue.push_back(MultiTurnStreamItem::StreamAssistantItem(item));
            }
            // Held fragments belong to a call the turn does not allow; they are
            // replayed by `EmitToolCall` under the repaired name, or dropped.
            StreamedTurnEvent::HoldToolCall => {}
            StreamedTurnEvent::EmitToolCall { call } => {
                // The item that just arrived is the call's end, but its
                // `content` may name the tool as the provider spelled it (a
                // repaired call is renamed by the assembler). Forward the end
                // carrying the validated call, which is what a consumer keys
                // the following `ToolCall` item on.
                let surfaced = match item {
                    Item::Event(StreamEvent::End { part, .. }) => Item::Event(StreamEvent::End {
                        part,
                        content: AssistantContent::ToolCall(call),
                    }),
                    other => other,
                };
                queue.push_back(MultiTurnStreamItem::StreamAssistantItem(surfaced));
            }
            StreamedTurnEvent::InvalidToolCall(invalid) => {
                let partial = attempt.assembler.partial_turn(&attempt.stream.partial());
                let context = run.streamed_invalid_tool_call_context(&partial, &invalid);
                let resolution = match drive.observer.invalid_tool_call(&context) {
                    Some(action) => {
                        run.resolve_streamed_invalid_tool_call(&partial, &invalid, action)
                    }
                    None => match drive.spec.unhandled_invalid_tool_call {
                        UnhandledInvalidToolCall::Fail => run.resolve_streamed_invalid_tool_call(
                            &partial,
                            &invalid,
                            rig_agent::agent::InvalidToolCallAction::fail(),
                        ),
                        UnhandledInvalidToolCall::Ignore => run.ignore_streamed_invalid_tool_call(),
                    },
                }?;
                let abandoned = matches!(resolution, StreamedResolution::TurnAbandoned);
                // A repaired call is returned as an event to forward; the
                // assembler has already renamed it, so the next poll's
                // `EmitToolCall` carries the validated call.
                let _ = attempt.assembler.resolve_pending_invalid(&resolution);
                if abandoned {
                    // The rollback is in the run; drop the attempt and let the
                    // next `next_step` re-issue it. That is the retry.
                    self.attempt = None;
                }
            }
            // `StreamedTurnEvent` is non-exhaustive. Dropping an unknown event
            // is the safe direction: a consumer sees less, never something
            // wrong.
            _ => {}
        }
        Ok(())
    }

    /// End the in-flight attempt: fold its response, record the completion call
    /// and let the run accept or reject the turn.
    async fn finish_attempt(&mut self) -> Result<(), PromptError> {
        let Some(attempt) = self.attempt.take() else {
            return Ok(());
        };
        // An attempt that got this far produced output by definition: `finish`
        // succeeds only on a complete stream. Recorded here as well as on the
        // failure path so a successful attempt counts as progress for the
        // *next* attempt's back-off — which is the common shape, since the
        // attempt that fails instantly is usually the continuation after a
        // successful tool-calling turn.
        let produced = attempt.produced_output();
        let response = attempt.stream.finish().await.map_err(PromptError::from)?;
        if produced {
            self.persist.lock().await.note_produced(true);
        }
        let call = self.run.record_streamed_completion_call(
            response.usage,
            response.identity(),
            response.finish_reason(),
            response.raw.clone(),
        )?;
        let assembled = attempt.assembler.finish(&response);
        let content = assembled.choice.clone();
        let recorded = assistant_message(None, content.clone());
        self.run.streamed_turn(assembled)?;
        // `at` is where the run put it: the persister clears its held copy once
        // the cursor has written past it.
        let at = self.run.messages().len().saturating_sub(1);
        self.persist.lock().await.set_tail(recorded, at);
        // Per-call usage reaches the caller the same way rig's stream carried
        // it, so the status line keeps reading the provider's own count.
        self.queue
            .push_back(MultiTurnStreamItem::CompletionCall(call));
        self.drive.observer.after_turn(attempt.turn, &content).await;
        Ok(())
    }

    /// Feed the run's committed tail past the cursor to the queue, then advance
    /// it.
    fn project_committed(&mut self) {
        let committed = self
            .run
            .messages()
            .get(self.announce_cursor..)
            .unwrap_or_default();
        for item in project(committed) {
            match item {
                CommittedItem::ToolCall(tool_call) => {
                    self.queue.push_back(MultiTurnStreamItem::ToolCall {
                        tool_call: tool_call.clone(),
                    });
                }
                CommittedItem::ToolResult(tool_result) => {
                    self.queue.push_back(MultiTurnStreamItem::ToolResult {
                        tool_result: tool_result.clone(),
                    });
                }
                // `CommittedItem` is non-exhaustive; a variant this loop does
                // not know is not a tool exchange, so it contributes nothing.
                _ => {}
            }
        }
        self.announce_cursor = self.run.messages().len();
    }

    /// Execute one pending call and answer it into the run.
    ///
    /// A batch is answered call by call; the run commits the whole batch as one
    /// user message once the last answer lands. Persistence of the answers
    /// themselves is the observer's business (doc §3), which is why the *answers
    /// written so far* are the persist boundary inside a batch.
    ///
    /// Every executed answer is wrapped in a parts list before it enters the
    /// run, so the run's own history is where the projection finds the call as
    /// the tool understood it. That is what makes `preprocess_args` a *once per
    /// live call* computation: it runs here, and nothing downstream re-derives
    /// it.
    async fn answer_call(&mut self, call: PendingToolCall) -> Result<(), PromptError> {
        match call {
            PendingToolCall::Execute(call) => {
                let name = call.name().to_string();
                // The call as the tool reads it: paths relativized, a `bash`
                // `cd <cwd> &&` prefix stripped. Computed once, used twice — for
                // the panel label now and for the part the run records.
                let raw = call.tool_call().function.arguments_value();
                let understood = crate::agent::tool_shape::preprocess_or_raw(
                    &name,
                    &raw,
                    &crate::tool_args::PathContext::sandbox(),
                );
                self.drive
                    .observer
                    .dispatch(&name, &understood)
                    .map_err(|reason| self.run.cancel_error(reason))?;
                let result = match self.drive.tools.get(&name) {
                    Some(tool) => {
                        let mut context = ToolContext::new();
                        tool.execute(raw.to_string(), &mut context).await
                    }
                    // The turn's policy allowed the name, so a miss here means
                    // the registry changed under the run. Answer honestly
                    // rather than leaving the call pending forever.
                    None => rig_core::tool::ToolResult::failed(
                        rig_core::tool::ToolExecutionError::other(format!(
                            "tool `{name}` is not registered"
                        )),
                    ),
                };
                // A tool that returns parts (`edit`) said so itself; anything
                // else is one call with one result, built from the value
                // computed above. `is_error` is read before the wrap and travels
                // in the part: it is rig's own rule for the message it would
                // have written, so a refused or failed call lands in the part
                // exactly as it landed in the message before.
                let parts = match crate::agent::parts::parts_of_result(&result) {
                    Some(parts) => parts,
                    None => {
                        // A malformed `edit` block's error leads with the
                        // problem once the redundant wrappers are gone: a weak
                        // model is far likelier to self-correct from a terse
                        // message. Only the model-visible text changes; the
                        // disposition and telemetry keep the raw failure.
                        let rendered = result.output().render();
                        let text =
                            crate::agent::transcript::model_facing_tool_text(&name, &rendered)
                                .map(str::to_string)
                                .unwrap_or(rendered);
                        vec![CallPart {
                            args: understood.clone(),
                            result: text,
                            is_error: !result.is_success(),
                        }]
                    }
                };
                self.drive.observer.outcome(&name, &parts);
                // `with_output` keeps the disposition and swaps only the
                // model-visible content, so a failure injects exactly as a
                // success does. `answer_all` commits this block verbatim.
                let wrapped = result.with_output(rig_core::tool::ToolOutput::json(
                    serde_json::to_value(&parts).map_err(|error| {
                        self.run.cancel_error(format!(
                            "tool `{name}` produced parts that do not serialize: {error}"
                        ))
                    })?,
                ));
                // Held, not written: the run commits a whole batch as one
                // message, and this is the copy that survives an abort first.
                // The shape is the one rig would commit, so a repaired batch is
                // indistinguishable from a completed one.
                self.persist
                    .lock()
                    .await
                    .note_answer(&rig_core::message::ToolResult {
                        call: call.tool_call().id.clone(),
                        name: rig_core::message::ToolName::new(name.clone())
                            .unwrap_or_else(|_| crate::agent::transcript::tool_name(&name)),
                        content: wrapped.output().clone().into_content(),
                        is_error: !wrapped.is_success(),
                    });
                self.run.answer(call.answer(wrapped))?;
                Ok(())
            }
            PendingToolCall::Malformed(call) => {
                let context = self.run.malformed_context(&call, true);
                let action = self.drive.observer.invalid_tool_call(&context);
                self.run.answer(call.answer(action))
            }
            // `PendingToolCall` is non-exhaustive. A kind this loop does not
            // know cannot be answered, and leaving it pending hangs the run, so
            // fail with the run's own error rather than continuing.
            _ => Err(self
                .run
                .cancel_error("the run emitted a tool-call kind this driver does not handle")),
        }
    }
}

/// Consume the input row, then paint the steer as a committed prompt — in that
/// order, because the paint is what composes the frame.
///
/// **The two steps are one function so the order cannot be inverted.** The input
/// element decides its own caret from the buffer at compose time: mid-turn with
/// an empty buffer it asks for no rows and renders no caret, so the frame draws
/// `HIDE_CURSOR` over a row that is genuinely gone. Painting first would compose
/// with the steer still in the buffer — one input row with a caret at its end —
/// and that caret would stay on screen over a row nothing occupies.
fn commit_steer_to_screen(
    row: Option<&std::sync::Mutex<crate::repl::LineEditor>>,
    content: &crate::content::Content,
    scheduled: &str,
    raw: &str,
) {
    consume_input_row(row, scheduled);
    content.display_committed_prompt(raw);
}

/// Empty the input row if — and only if — it still holds the text a steer was
/// scheduled from.
///
/// A steer is editable until it is delivered (`src/agent/steer.rs`), so the row
/// is untouched at schedule time and this is where the text is finally consumed.
/// The equality is the whole guard: anything else in the row is newer work —
/// either an edit to this steer or the beginning of the next message — and
/// clearing it would discard keystrokes nothing else has a copy of. The steer
/// itself is already captured in the slot, so a mismatch loses nothing.
fn consume_input_row(row: Option<&std::sync::Mutex<crate::repl::LineEditor>>, scheduled: &str) {
    let Some(row) = row else {
        // No REPL: piped or batch input has no row to clear.
        return;
    };
    let mut editor = row
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if editor.text() == scheduled {
        editor.set("");
    }
}

/// The loop's persistence, reachable after the loop is dropped.
///
/// The turn pump stops by dropping the stream, so the exit path cannot ask the
/// loop anything — but a half-finished tool batch still has to be written. The
/// handle is the loop's own `Arc`, so closing through it is closing the same
/// persister, and it is idempotent: a normal exit closes, and then the pump's
/// own close finds nothing left to do.
#[derive(Clone)]
pub(crate) struct PersistHandle(Arc<AsyncMutex<Persister>>);

impl PersistHandle {
    /// Close the turn. See [`DriveLoop::close`].
    pub(crate) async fn close(&self) {
        self.0.lock().await.close().await;
    }

    /// Whether the model produced output since the last reset. Drives the
    /// back-off reset; see [`Persister::made_progress`].
    pub(crate) async fn made_progress(&self) -> bool {
        self.0.lock().await.made_progress()
    }

    /// Clear the progress record once the back-off has read it.
    pub(crate) async fn reset_progress(&self) {
        self.0.lock().await.reset_progress();
    }
}

/// [`DriveLoop`] as the item stream the turn pump already reads.
///
/// A generator, not a `poll_fn`: `poll_fn` would build a fresh `next` future on
/// every poll and throw away its state, which is exactly wrong for a driver that
/// owns an in-flight provider stream — the attempt would restart at every pause.
/// Inside `stream!` the future lives across the `yield`, so a pump that stops
/// selecting resumes the same attempt.
/// Borrows the loop rather than owning it: a failed attempt leaves the run
/// resumable, and resuming means polling this same loop again.
pub(crate) fn run_stream(
    drive: &mut DriveLoop,
) -> impl futures::Stream<Item = Result<MultiTurnStreamItem, PromptError>> + '_ {
    async_stream::stream! {
        while let Some(item) = drive.next().await {
            match item {
                Ok(DriveItem::Stream(item)) => yield Ok(item),
                Ok(DriveItem::Done(response)) => {
                    yield Ok(MultiTurnStreamItem::FinalResponse(response));
                }
                Err(error) => {
                    // Yield, do not return. The caller owns the retry policy —
                    // back-off, budget and the cancellable wait all live in the
                    // turn pump, where `Esc` and the key queue already are — and
                    // if it polls again the loop resumes from wherever the run
                    // really is. Dropping the stream is the caller's way of
                    // giving up, which is why there is nothing to unwind here.
                    yield Err(error);
                }
            }
        }
    }
}

/// A context document carrying a steer or a retry hint.
///
/// Documents are sent with the request and never persisted, which is exactly the
/// property a steer needs: it changes what the model sees without becoming part
/// of the transcript the next turn reads.
///
/// Both users exist: a retry hint (`turn.rs::prompt`), and — from phase 5 —
/// a queued mid-turn steer.
pub(crate) fn context_document(text: impl Into<String>) -> Document {
    Document {
        id: uuid::Uuid::now_v7().to_string(),
        text: text.into(),
        additional_props: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_with(text: &str) -> std::sync::Mutex<crate::repl::LineEditor> {
        let mut editor = crate::repl::LineEditor::new();
        editor.set(text);
        std::sync::Mutex::new(editor)
    }

    /// A steer is editable until it is delivered, so the row still holds the
    /// text when the boundary arrives — and delivery is what empties it.
    #[test]
    fn delivery_clears_the_row_it_was_scheduled_from() {
        let row = row_with("use sed instead");
        consume_input_row(Some(&row), "use sed instead");
        assert_eq!(row.lock().unwrap().text(), "");
    }

    /// Anything else in the row is newer work. A steer edited after submitting
    /// it, or the beginning of the next message, must survive: the steer itself
    /// is already captured in the slot, so a mismatch costs nothing, while
    /// clearing regardless would discard keystrokes nothing else holds.
    #[test]
    fn delivery_leaves_a_row_the_user_has_since_changed() {
        let edited = row_with("use sed instead, please");
        consume_input_row(Some(&edited), "use sed instead");
        assert_eq!(
            edited.lock().unwrap().text(),
            "use sed instead, please",
            "an edit to the steer must not be thrown away"
        );

        let next_message = row_with("now do something else");
        consume_input_row(Some(&next_message), "use sed instead");
        assert_eq!(
            next_message.lock().unwrap().text(),
            "now do something else",
            "the next message must not be swallowed by a pending steer"
        );
    }

    /// The two steps are one call, so the order cannot be written the wrong way
    /// round. Asserted through the observable end state: after committing, the
    /// row is empty and the steer is in the transcript.
    ///
    /// The *visual* half — that the frame then composites no caret — is
    /// `Content`'s own rule (`a_mid_turn_paint_takes_its_caret_from_the_buffer_at_compose_time`)
    /// and `tests/mid_turn_steer.rs`, which drives the real binary.
    #[test]
    fn committing_a_steer_empties_the_row_it_came_from() {
        let row = row_with("steer text");
        let content = crate::content::Content::with_size(
            crate::settings::default_show_reasoning_handle(),
            crate::termio::TermSize {
                width: 40,
                height: 24,
            },
        );
        commit_steer_to_screen(Some(&row), &content, "steer text", "steer text");
        assert_eq!(
            row.lock().unwrap().text(),
            "",
            "the row is consumed, so the frame that follows has no caret to place"
        );
    }

    /// An edit made between scheduling and delivery survives the commit: the row
    /// is not consumed, so the frame keeps its caret over text the user still
    /// owns.
    #[test]
    fn committing_a_steer_leaves_a_row_that_has_since_changed() {
        let row = row_with("steer text, edited");
        let content = crate::content::Content::with_size(
            crate::settings::default_show_reasoning_handle(),
            crate::termio::TermSize {
                width: 40,
                height: 24,
            },
        );
        commit_steer_to_screen(Some(&row), &content, "steer text", "steer text");
        assert_eq!(row.lock().unwrap().text(), "steer text, edited");
    }

    /// No REPL, no row: piped and batch input have nothing to clear, and this
    /// must not panic on the way there.
    #[test]
    fn delivery_without_a_row_is_a_no_op() {
        consume_input_row(None, "anything");
    }

    /// The hint is taken, not read: one corrective message for one attempt.
    ///
    /// A cloned or non-consuming read would replay "your last tool call was
    /// malformed" on every later request of the turn — and, because the hint is
    /// a context document, without ever appearing in the transcript to explain
    /// where it came from.
    #[test]
    fn a_retry_hint_is_delivered_once() {
        let hint: RetryHint = RetryHint::default();
        *hint.lock().unwrap() = Some(context_document("re-emit the call"));

        let first = hint.lock().unwrap().take();
        assert_eq!(first.map(|d| d.text), Some("re-emit the call".to_string()));

        let second = hint.lock().unwrap().take();
        assert!(second.is_none(), "the hint must not survive its attempt");
    }

    /// Each hint carries a fresh id, so two steers are two documents rather
    /// than one document rendered twice.
    #[test]
    fn context_documents_are_distinct() {
        let a = context_document("one");
        let b = context_document("one");
        assert_ne!(a.id, b.id);
    }

    /// A truncated attempt's reasoning must survive into the session.
    ///
    /// The intent is `ReasoningInContextMode::AbortedTurnsOnly`, whose whole
    /// point is the trailing run of turns with no provider `responseId`: keep
    /// their reasoning so the model can continue the thread it was cut off in.
    /// Filtering it out here pre-empts that setting, and the user paid for the
    /// tokens either way.
    #[test]
    fn an_abandoned_attempt_keeps_its_reasoning_and_drops_its_calls() {
        let content = vec![
            AssistantContent::Reasoning(rig_core::message::Reasoning::new("thought")),
            AssistantContent::Text(rig_core::message::Text::new("half an ans")),
            AssistantContent::ToolCall(rig_core::message::ToolCall::new(
                rig_core::message::CallId::from_wire("call_1"),
                rig_core::message::ToolFunction::new(
                    rig_core::message::ToolName::new("read").expect("valid name"),
                    serde_json::json!({}),
                ),
            )),
        ];
        let kept = keep_abandoned(Some(content)).expect("content worth keeping");
        assert!(
            kept.iter()
                .any(|c| matches!(c, AssistantContent::Reasoning(_))),
            "reasoning must be kept: {kept:?}"
        );
        assert!(
            kept.iter()
                .any(|c| matches!(c, AssistantContent::Text(t) if t.text == "half an ans")),
            "text must be kept: {kept:?}"
        );
        assert!(
            !kept
                .iter()
                .any(|c| matches!(c, AssistantContent::ToolCall(_))),
            "an unanswered call is an orphan on the wire: {kept:?}"
        );
    }

    /// Nothing of the model's own output means nothing to append — which is
    /// also the case where the retry re-issues in place instead of rebuilding.
    #[test]
    fn an_attempt_with_nothing_of_its_own_is_not_kept() {
        assert!(keep_abandoned(None).is_none());
        assert!(keep_abandoned(Some(Vec::new())).is_none());
        assert!(
            keep_abandoned(Some(vec![AssistantContent::Text(
                rig_core::message::Text::new("")
            )]))
            .is_none(),
            "an empty text block is not output"
        );
    }
}
