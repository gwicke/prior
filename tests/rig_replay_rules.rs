//! What rig will and will not replay, which decides how a partial turn is
//! retained across a retry (`doc/261102_partial_turn_retention.md` §2).
//!
//! Both facts below are rig behaviours, not prior logic. They are pinned
//! because the retention design rests on them and both were arrived at by
//! experiment rather than from documentation: the difference between a partial
//! that survives and one that is silently deleted is a single `StopReason`.

use rig_core::completion::history::{adapt, Accepts, ReplayTarget};
use rig_core::completion::options::{Mapping, OptionFields, OptionMap};
use rig_core::completion::CompletionRequest;
use rig_core::message::{
    Api, AssistantContent, AssistantMessage, Message, StopReason, Text, UserContent,
};

/// A minimal wire, only to run the same history adaptor a provider request
/// goes through.
#[derive(Debug)]
struct Target;

impl ReplayTarget for Target {
    fn api(&self) -> Api {
        Api::from_static("openai.chat")
    }
    fn provider(&self) -> &str {
        "openai"
    }
    fn model(&self) -> &str {
        "gpt-4o"
    }
    fn accepts(&self, _model: &str) -> Accepts {
        Accepts {
            user_images: true,
            assistant_images: true,
            tool_result_images: true,
            tools: true,
            foreign_reasoning: false,
        }
    }
    fn map_options(&self, _request: &CompletionRequest, _fields: OptionFields<'_>) -> OptionMap {
        OptionMap {
            reasoning: Mapping::Nothing,
            cache: Mapping::Nothing,
            service_tier: Mapping::Nothing,
            verbosity: Mapping::Nothing,
            parallel_tool_calls: Mapping::Nothing,
            top_p: Mapping::Nothing,
            seed: Mapping::Nothing,
            stop: Mapping::Nothing,
            cache_key: Mapping::Nothing,
        }
    }
}

fn partial(text: &str, stop: Option<StopReason>) -> Message {
    let mut turn = AssistantMessage::new(vec![AssistantContent::Text(Text::new(text))]);
    turn.stop = stop;
    Message::Assistant(turn)
}

fn wire(history: Vec<Message>) -> Vec<Message> {
    adapt(&history, &Target)
}

/// Whether `history` reaches the wire still holding `text`.
fn survives(history: Vec<Message>, text: &str) -> bool {
    format!("{:?}", wire(history)).contains(text)
}

/// The partial cannot be taken from `stream.partial()`: a turn marked as a
/// failure is dropped when the request is encoded, below `prepare_request` and
/// out of any driver's reach. `AssistantMessage::new` leaves `stop: None`, and
/// that is what keeps the text.
#[test]
fn only_an_unmarked_partial_turn_survives_encoding() {
    assert!(
        survives(
            vec![
                Message::user("fix the bug"),
                partial("halfway through", None),
            ],
            "halfway through"
        ),
        "a hand-built partial turn is replayed"
    );
    for stop in [
        StopReason::Aborted("cut".to_owned()),
        StopReason::Error("boom".to_owned()),
    ] {
        assert!(
            !survives(
                vec![
                    Message::user("fix the bug"),
                    partial("halfway through", Some(stop.clone())),
                ],
                "halfway through"
            ),
            "a turn stopped with {stop:?} is dropped from the wire"
        );
    }
    assert!(
        survives(
            vec![
                Message::user("fix the bug"),
                partial("halfway through", Some(StopReason::Stop)),
            ],
            "halfway through"
        ),
        "a completed turn is replayed"
    );
}

/// A rebuilt run takes its last message as the prompt, so the continuation is
/// the prompt and the partial must sit in the history ahead of it. Putting the
/// partial in both places is a pairing error, not a duplication: two assistant
/// turns in a row are invalid.
#[test]
fn a_rebuilt_run_reads_prompt_history_partial_continue() {
    use rig_agent::run::{spec::RunSpec, AgentRun, AgentRunStep};
    use std::sync::Arc;

    let core: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("fix the bug")),
        Arc::new(partial("halfway through", None)),
    ];
    let mut run = AgentRun::from_spec(&RunSpec::default(), Message::user("continue"), Some(core));
    let Ok(AgentRunStep::CallModel {
        prompt, history, ..
    }) = run.next_step()
    else {
        panic!("a rebuilt run emits a model call");
    };
    let mut sent: Vec<Message> = history.iter().map(|m| (**m).clone()).collect();
    sent.push((*prompt).clone());

    let shaped = wire(sent);
    assert_eq!(shaped.len(), 3, "prompt, partial, continuation");
    assert_eq!(
        shaped[1],
        partial("halfway through", None),
        "the partial is kept"
    );
    let Message::User { content } = &shaped[2] else {
        panic!("the continuation is the prompt, last");
    };
    assert!(
        matches!(content.first(), Some(UserContent::Text(t)) if t.text == "continue"),
        "the continuation follows the partial"
    );
}

/// A consecutive retry reuses the trailing continuation as the prompt instead
/// of appending a second one, so the wire is unchanged however many times the
/// attempt fails.
#[test]
fn a_consecutive_retry_does_not_add_a_second_continuation() {
    use rig_agent::run::{spec::RunSpec, AgentRun, AgentRunStep};
    use std::sync::Arc;

    // The session after a first failed attempt and its rebuild.
    let session: Vec<Arc<Message>> = vec![
        Arc::new(Message::user("fix the bug")),
        Arc::new(partial("halfway through", None)),
        Arc::new(Message::user("continue")),
    ];
    let (prompt, core) = session.split_last().expect("history is non-empty");
    let mut run = AgentRun::from_spec(&RunSpec::default(), (**prompt).clone(), Some(core.to_vec()));
    let Ok(AgentRunStep::CallModel {
        prompt, history, ..
    }) = run.next_step()
    else {
        panic!("a rebuilt run emits a model call");
    };
    let mut sent: Vec<Message> = history.iter().map(|m| (**m).clone()).collect();
    sent.push((*prompt).clone());

    // Assert on the pre-wire history. rig merges consecutive user messages
    // when it encodes a request, so a duplicated continuation would be
    // invisible to a provider while still littering the log the session
    // replays - the one place this can be caught.
    let continuations = sent
        .iter()
        .filter(|m| {
            matches!(m,
                Message::User { content }
                    if matches!(content.first(), Some(UserContent::Text(t)) if t.text == "continue"))
        })
        .count();
    assert_eq!(
        continuations, 1,
        "exactly one continuation, not one per retry"
    );
    assert_eq!(sent.len(), 3, "the retry appends nothing");
    assert_eq!(wire(sent).len(), 3, "and the wire is unchanged");
}

/// The same partial twice in one history is rejected by rig's own pairing
/// rule, which is why the rebuild puts it in exactly one place.
#[test]
fn a_partial_in_history_and_as_prompt_is_rejected() {
    use rig_core::conversation::pairing::validate;

    let history = vec![
        Message::user("fix the bug"),
        partial("halfway through", None),
        partial("halfway through", None),
    ];
    let error = validate(&history).expect_err("consecutive assistant turns are invalid");
    assert!(
        error.to_string().contains("consecutive assistant"),
        "unexpected repair: {error}"
    );
}
