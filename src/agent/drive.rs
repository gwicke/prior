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
use rig_core::tool::{ToolContext, ToolResult};

/// What the drive loop needs from a `CodeAgent`: the model, the tools, the run
/// spec and the observer.
pub(crate) struct Drive {
    pub(crate) model: ModelHandle,
    pub(crate) tools: Arc<rig_agent::tool::ToolCatalog>,
    pub(crate) spec: rig_agent::run::spec::RunSpec,
    /// Owned rather than borrowed: the stream outlives the call that builds it.
    pub(crate) observer: Arc<dyn Observer>,
}

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

    /// A tool call is about to run. `args` is the model's spelling, unmodified.
    fn dispatch(&self, name: &str, args: &str) -> Result<(), String>;

    /// A tool call answered. `result` is what the run will commit; returning a
    /// replacement rewrites the model-visible content without changing the
    /// disposition (rig's `OutcomeAction::rewrite`).
    fn outcome(&self, name: &str, args: &str, result: &ToolResult) -> Option<ToolResult>;

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
    cursor: usize,
    /// Items ready to hand out before the next `await`.
    queue: VecDeque<MultiTurnStreamItem>,
    attempt: Option<Attempt>,
    /// Set once `Done` has been handed out. rig's `next_step` stays in its
    /// terminal state and keeps answering `Done`, so without this the loop
    /// would emit the final response forever.
    finished: bool,
}

impl DriveLoop {
    pub(crate) fn new(drive: Drive, run: AgentRun) -> Self {
        let cursor = run.projection_start();
        Self {
            drive,
            run,
            cursor,
            queue: VecDeque::new(),
            attempt: None,
            finished: false,
        }
    }

    /// The run's committed messages, the persistence source.
    ///
    /// Phase 3 replaces the transcript's reassembly with a cursor over this;
    /// until then `TurnTranscript` is still the writing path and this is what
    /// a caller reads to prove the two agree.
    #[allow(dead_code)]
    pub(crate) fn messages(&self) -> &[Arc<Message>] {
        self.run.messages()
    }

    /// The error to persist and report when the caller stops the loop: the
    /// same canonical history rig's own cancellation carried.
    #[allow(dead_code)]
    pub(crate) fn cancelled(&self, reason: impl Into<String>) -> PromptError {
        self.run.cancel_error(reason)
    }

    /// The run itself, once the loop has finished — what phase 2 resumes from.
    #[allow(dead_code)]
    pub(crate) fn into_run(self) -> AgentRun {
        self.run
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
                    Err(error) => return Some(Err(error)),
                }
            }
            match self.advance_step().await {
                Ok(Step::Advanced) => continue,
                Ok(Step::Done(response)) => return Some(Ok(DriveItem::Done(*response))),
                Err(error) => return Some(Err(error)),
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
                let patch = match self
                    .drive
                    .observer
                    .before_call(&prompt, &history, turn)
                    .await
                {
                    Ok(patch) => patch,
                    Err(reason) => return Err(self.run.cancel_error(reason)),
                };

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
        let response = attempt.stream.finish().await.map_err(PromptError::from)?;
        let call = self.run.record_streamed_completion_call(
            response.usage,
            response.identity(),
            response.finish_reason(),
            response.raw.clone(),
        )?;
        let assembled = attempt.assembler.finish(&response);
        let content = assembled.choice.clone();
        self.run.streamed_turn(assembled)?;
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
        let committed = self.run.messages().get(self.cursor..).unwrap_or_default();
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
        self.cursor = self.run.messages().len();
    }

    /// Execute one pending call and answer it into the run.
    ///
    /// A batch is answered call by call; the run commits the whole batch as one
    /// user message once the last answer lands. Persistence of the answers
    /// themselves is the observer's business (doc §3), which is why `outcome`
    /// fires before the answer is applied.
    async fn answer_call(&mut self, call: PendingToolCall) -> Result<(), PromptError> {
        match call {
            PendingToolCall::Execute(call) => {
                let name = call.name().to_string();
                let raw = call.tool_call().function.arguments_json();
                self.drive
                    .observer
                    .dispatch(&name, &raw)
                    .map_err(|reason| self.run.cancel_error(reason))?;
                let result = match self.drive.tools.get(&name) {
                    Some(tool) => {
                        let mut context = ToolContext::new();
                        tool.execute(raw.clone(), &mut context).await
                    }
                    // The turn's policy allowed the name, so a miss here means
                    // the registry changed under the run. Answer honestly
                    // rather than leaving the call pending forever.
                    None => ToolResult::failed(rig_core::tool::ToolExecutionError::other(format!(
                        "tool `{name}` is not registered"
                    ))),
                };
                let result = self
                    .drive
                    .observer
                    .outcome(&name, &raw, &result)
                    .unwrap_or(result);
                self.run.answer(call.answer(result))
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

/// [`DriveLoop`] as the item stream the turn pump already reads.
///
/// A generator, not a `poll_fn`: `poll_fn` would build a fresh `next` future on
/// every poll and throw away its state, which is exactly wrong for a driver that
/// owns an in-flight provider stream — the attempt would restart at every pause.
/// Inside `stream!` the future lives across the `yield`, so a pump that stops
/// selecting resumes the same attempt.
pub(crate) fn run_stream(
    mut drive: DriveLoop,
) -> impl futures::Stream<Item = Result<MultiTurnStreamItem, PromptError>> + Send {
    async_stream::stream! {
        while let Some(item) = drive.next().await {
            match item {
                Ok(DriveItem::Stream(item)) => yield Ok(item),
                Ok(DriveItem::Done(response)) => {
                    yield Ok(MultiTurnStreamItem::FinalResponse(response));
                }
                Err(error) => {
                    yield Err(error);
                    return;
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
/// Phase 2 (`doc/261030_rig_direct_drive.md` §5) is the first user.
#[allow(dead_code)]
pub(crate) fn steer_document(text: impl Into<String>) -> Document {
    Document {
        id: uuid::Uuid::now_v7().to_string(),
        text: text.into(),
        additional_props: Default::default(),
    }
}
