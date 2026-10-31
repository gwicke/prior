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

use crate::agent::drive::Observer;
use crate::agent::msglog::MessageLogger;
use crate::agent::parts::CallPart;
use crate::agent::transcript::{maybe_strip_reasoning, render_model_facing};
use crate::content::Content;
use crate::session::Session;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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
    /// The session this turn writes into, if any — used by the mid-turn
    /// steering path (design §5) to commit a steer as a real user message.
    /// Persistence of tool work is the persister's job.
    #[allow(dead_code)]
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
    }

    fn dispatch(&self, name: &str, args: &serde_json::Value) -> Result<(), String> {
        if self.cancelled() {
            return Err("cancelled by user".to_string());
        }
        // `edit` paints nothing here: its panel is a *pair* (label + diff, then
        // result), and the call half is not complete until the edits resolve.
        // `outcome` paints the whole thing from the parts.
        if name == "edit" {
            return Ok(());
        }
        self.content.display_tool_call(name, args);
        Ok(())
    }

    fn outcome(&self, name: &str, parts: &[CallPart]) {
        if self.cancelled() {
            return;
        }
        // `edit` painted nothing at dispatch, so it paints the whole pair; the
        // other tools are already on screen, so only their results are left.
        self.content.display_tool_parts(name, parts, name != "edit");
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
