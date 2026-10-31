//! The drive loop's host side: what `LoggingHook` used to do as a rig hook.
//!
//! rig fires hooks from inside its engine; prior's hook painted, persisted and
//! logged, and its `on_completion_call` doubled as the persistence boundary
//! because that was the only callback that saw the whole request history. With
//! the loop in hand (`crate::agent::drive`), the same work hangs off four
//! explicit call points instead, and the persistence boundary is where the
//! protocol says it is rather than where a hook happened to fire.
//!
//! Nothing here decides *policy*. A cancel is a flag read, a persist is a call,
//! and the two rewrites (model-facing history, model-facing tool text) are the
//! same functions the hook called. The point of the split is that the loop owns
//! control flow and this owns effects, so neither has to know the other's
//! structure.

use std::sync::atomic::Ordering;

use rig_agent::agent::{CompletionCallEvent, InvalidToolCallAction, InvalidToolCallContext};
use rig_agent::run::RequestPatch;
use rig_core::completion::Message;
use rig_core::message::AssistantContent;
use rig_core::tool::ToolResult;

use crate::agent::drive::Observer;
use crate::agent::msglog::MessageLogger;
use crate::agent::transcript::{
    maybe_strip_reasoning, model_facing_tool_text, render_model_facing, TurnTranscript,
};
use crate::content::Content;
use crate::session::Session;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Everything a turn's effects need.
///
/// Owns its handles (each an `Arc` the agent already shares) rather than
/// borrowing, because the drive stream has to outlive the call that builds it:
/// the turn pump holds it across `select!` arms for the whole turn.
pub(crate) struct TurnObserver {
    pub(crate) content: Arc<Content>,
    pub(crate) user_cancel: Arc<AtomicBool>,
    pub(crate) message_logger: Option<Arc<MessageLogger>>,
    pub(crate) reasoning_in_context: crate::settings::ReasoningInContextMode,
    pub(crate) provider: String,
    pub(crate) model: String,
    /// The run-scoped transcript accumulator, still the persistence queue
    /// until phase 2 replaces it with a cursor over `run.messages()`.
    pub(crate) transcript: Arc<Mutex<TurnTranscript>>,
    pub(crate) session: Option<Arc<Session>>,
}

impl TurnObserver {
    fn cancelled(&self) -> bool {
        self.user_cancel.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Observer for TurnObserver {
    async fn before_call(
        &self,
        _prompt: &Message,
        history: &[Arc<Message>],
        turn: usize,
    ) -> Result<Option<RequestPatch>, String> {
        // Persist the turn that just finished, *before* the cancel check, so a
        // cancel-stop still writes it. The turn whose response is about to be
        // requested is complete and pair-valid, which is what makes this the
        // incremental persist point.
        let _ = crate::agent::transcript::commit(
            &self.transcript,
            self.session.as_deref(),
            crate::agent::transcript::RunState::Ongoing,
        )
        .await;
        if let Ok(mut t) = self.transcript.lock() {
            t.start_live();
        }

        if self.cancelled() {
            return Err("cancelled by user".to_string());
        }

        // The history to send: reasoning stripped when configured, then every
        // tool call rendered the way the model should see it (paths short,
        // `edit` in block form). Order is `strip -> render`; nothing is
        // re-parsed or re-minimised, because the stored form already carries
        // what execution decided.
        let stripped = maybe_strip_reasoning(
            history.iter().map(|m| (**m).clone()).collect(),
            self.reasoning_in_context,
            &self.provider,
            &self.model,
        );
        let rendered = render_model_facing(stripped);

        if let Some(logger) = self.message_logger.as_deref() {
            let turn_id = logger.advance_turn();
            for message in &rendered {
                logger.log_message("sent", message, turn_id).await;
            }
        }
        let _ = turn;
        Ok(Some(RequestPatch::new().history(rendered)))
    }

    async fn after_turn(&self, _turn: usize, content: &[AssistantContent]) {
        if let Some(logger) = self.message_logger.as_deref() {
            let message = crate::agent::transcript::assistant_message(None, content.to_vec());
            let turn_id = logger.current_turn_id();
            logger.log_message("received", &message, turn_id).await;
        }
        // The accepted turn goes into the transcript so a mid-turn failure
        // persists exactly what completed. `None` for the response id: the
        // committed message carries it, and the transcript's own `accept`
        // takes the id it is given when a hook supplies one.
        if let Ok(mut t) = self.transcript.lock() {
            t.accept(None, content.to_vec());
        }
    }

    fn dispatch(&self, name: &str, args: &str) -> Result<(), String> {
        if self.cancelled() {
            return Err("cancelled by user".to_string());
        }
        // `edit` paints nothing here: its panel is a *pair* (label + diff, then
        // result), and the call half is not complete until the edits resolve.
        // `outcome` paints the whole thing from the parts.
        if name == "edit" {
            return Ok(());
        }
        let args_json =
            serde_json::from_str(args).unwrap_or_else(|_| serde_json::Value::String(args.into()));
        // The call as the tool reads it — the `bash` prefix strip and path
        // relativisation — so the label carries the same bytes the record will.
        let args_json = crate::agent::tool_shape::preprocess_or_raw(
            name,
            &args_json,
            &crate::tool_args::PathContext::sandbox(),
        );
        self.content.display_tool_call(name, &args_json);
        Ok(())
    }

    fn outcome(&self, name: &str, _args: &str, result: &ToolResult) -> Option<ToolResult> {
        if self.cancelled() {
            return None;
        }
        // A tool that answered with parts painted its own call panels at
        // dispatch, so only the results are left to paint — one per part, in
        // order, which is exactly how N genuine single-file calls render.
        if let Some(parts) = crate::agent::parts::parts_of_result(result) {
            self.content
                .display_tool_parts(name, &parts, name != "edit");
            return None;
        }
        self.content.display_tool_result(&result.output().render());
        // A malformed `edit` block's error leads with the problem: the model
        // reads a stripped form, the disposition is unchanged.
        result
            .output()
            .as_text()
            .and_then(|text| model_facing_tool_text(name, text))
            .map(|cleaned| {
                result
                    .clone()
                    .with_output(rig_core::tool::ToolOutput::text(cleaned))
            })
    }

    fn invalid_tool_call(&self, context: &InvalidToolCallContext) -> Option<InvalidToolCallAction> {
        if self.cancelled() {
            return Some(InvalidToolCallAction::stop("cancelled by user"));
        }
        // Surface the attempt before the recovery, so the panel shows what the
        // model tried. The recovery budget is the run's
        // `max_invalid_tool_call_retries`; a second hallucination of the same
        // name exhausts it and fails the run.
        self.content.display_error(&format!(
            "Model called tool `{}` which is not available. Available tools: {:?}. \
             Asking the model to retry with a valid name.",
            context.tool_name, context.available_tools
        ));
        Some(InvalidToolCallAction::retry(format!(
            "tool `{}` is not available. Available tools: {:?}. \
             Please re-emit the call with one of the available tool names.",
            context.tool_name, context.available_tools
        )))
    }
}

/// The request patch a turn sends, for the parts of prior's hook that were pure
/// history shaping. Kept as a free function so the observer and any future
/// caller (a steer, a summarizer) agree on the order.
#[allow(dead_code)]
fn shaped_history(
    history: &[Arc<Message>],
    reasoning_in_context: crate::settings::ReasoningInContextMode,
    provider: &str,
    model: &str,
) -> Vec<Message> {
    let stripped = maybe_strip_reasoning(
        history.iter().map(|m| (**m).clone()).collect(),
        reasoning_in_context,
        provider,
        model,
    );
    render_model_facing(stripped)
}

/// The event the completion-call hook used to see, rebuilt for the one consumer
/// that still wants it (the message logger's prompt half).
#[allow(dead_code)]
fn call_event<'a>(
    prompt: &'a Message,
    history: &'a [Arc<Message>],
    turn: usize,
) -> CompletionCallEvent<'a> {
    CompletionCallEvent {
        prompt,
        history,
        turn,
    }
}
