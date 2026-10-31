//! The projection: a committed run log turned into the form prior stores and
//! sends.
//!
//! `AgentRun` keeps the model's own spelling — one `edit` call covering three
//! files is one call with a `blocks` blob. Prior stores pi's canonical per-file
//! arrays instead, and the tool already stated that split on its answer, as a
//! parts list (`crate::agent::parts`). So the projection is a *structural*
//! transform and nothing more: it unpacks the parts a tool returned, and names
//! no tool.
//!
//! It runs on the run's committed log rather than on a transcript reassembled
//! from stream deltas, which is what makes it the only place the stored form is
//! decided. Steering will splice here too (`doc/261030_rig_direct_drive.md` §5).
//!
//! ## Units, and why the two halves are projected together
//!
//! A *unit* is one assistant turn plus the results answering it. The split lives
//! in the result (the parts list a tool returned) but reshapes the call, so
//! neither half can be projected alone: the assistant alone would persist the
//! model's unsplit call, the results alone would leave their calls unexpanded.
//! So a unit is projected once both halves are committed — which is also what
//! makes [`complete_prefix_len`] the only safe persist bound.

use rig_core::message::{AssistantContent, Message, UserContent};

use crate::agent::parts;

/// Project a committed slice of the run's log into the stored form.
///
/// A call whose answer is not a parts list — one that never ran — is left
/// exactly as the model wrote it.
pub(crate) fn project(messages: &[Message]) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        let assistant = matches!(&messages[index], Message::Assistant(turn) if calls_a_tool(turn));
        let results = messages.get(index + 1).and_then(tool_results_of);
        if let (true, Some(results)) = (assistant, results) {
            let (assistant, results) = parts::unpack(messages[index].clone(), results.to_vec());
            out.push(assistant);
            if !results.is_empty() {
                out.push(Message::User { content: results });
            }
            index += 2;
            continue;
        }
        out.push(messages[index].clone());
        index += 1;
    }
    out
}

/// How many of `messages` form a complete prefix of units.
///
/// Everything before the returned index is projected safely; the remainder is a
/// unit whose results have not landed yet (an assistant turn awaiting its answers)
/// and must not be written. Every other position is complete by construction: a
/// results message always terminates its unit, and a turn calling no tool needs
/// no partner.
pub(crate) fn complete_prefix_len(messages: &[Message]) -> usize {
    match messages.last() {
        Some(Message::Assistant(turn)) if calls_a_tool(turn) => messages.len() - 1,
        _ => messages.len(),
    }
}

/// The results message answering a call, if `message` is one: a `User` message
/// carrying at least one tool result.
fn tool_results_of(message: &Message) -> Option<&Vec<UserContent>> {
    let Message::User { content } = message else {
        return None;
    };
    content
        .iter()
        .any(|c| matches!(c, UserContent::ToolResult(_)))
        .then_some(content)
}

/// Whether an assistant turn calls a tool, i.e. whether it needs a results
/// message before its unit is complete.
pub(crate) fn calls_a_tool(turn: &rig_core::message::AssistantMessage) -> bool {
    turn.content
        .iter()
        .any(|c| matches!(c, AssistantContent::ToolCall(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::parts::CallPart;
    use crate::agent::transcript::{assistant_message, text_tool_result, tool_call};
    use rig_core::message::{CallId, Text, ToolResultContent};

    fn parts_result(call: &str, parts: &[CallPart]) -> rig_core::message::ToolResult {
        rig_core::message::ToolResult {
            call: CallId::from_wire(call),
            name: crate::agent::transcript::tool_name("edit"),
            content: vec![ToolResultContent::Json {
                value: serde_json::to_value(parts).expect("parts serialize"),
            }],
            is_error: false,
        }
    }

    fn edit_turn() -> Message {
        assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "t1",
                "edit",
                serde_json::json!({ "blocks": "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END" }),
            ))],
        )
    }

    /// One multi-file call and its two-part answer become two calls, each with
    /// its own result — the whole of "splitting", with no tool named here.
    #[test]
    fn a_parts_answer_is_unpacked_into_one_call_per_part() {
        let parts: Vec<CallPart> = ["a.rs", "b.rs"]
            .iter()
            .map(|path| CallPart {
                args: serde_json::json!({ "path": path, "edits": [] }),
                result: format!("edited {path}"),
                is_error: false,
            })
            .collect();
        let results = Message::User {
            content: vec![UserContent::ToolResult(parts_result("t1", &parts))],
        };

        let out = project(&[edit_turn(), results]);
        assert_eq!(out.len(), 2, "count preserving");
        let Message::Assistant(turn) = &out[0] else {
            panic!("an assistant turn")
        };
        let calls: Vec<_> = turn
            .content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2, "one call per file");
        assert_eq!(calls[0].id.to_string(), "t1", "the first keeps the id");
        assert_eq!(calls[1].id.to_string(), "t1_f1n2");
        let Message::User { content } = &out[1] else {
            panic!("a results message")
        };
        assert_eq!(content.len(), 2, "one result per file");
    }

    /// A plain-text tool answer is not a parts list, so the unit rides through
    /// byte for byte. This is every non-`edit` call.
    #[test]
    fn a_plain_answer_is_untouched() {
        let assistant = assistant_message(
            None,
            vec![AssistantContent::ToolCall(tool_call(
                "t1",
                "read",
                serde_json::json!({ "path": "a.rs" }),
            ))],
        );
        let results = Message::User {
            content: vec![UserContent::ToolResult(text_tool_result(
                CallId::from_wire("t1"),
                "read",
                "contents",
            ))],
        };
        let input = vec![assistant, results];
        assert_eq!(project(&input), input);
    }

    /// A turn that calls nothing needs no partner, so it stands alone.
    #[test]
    fn a_final_text_turn_projects_alone() {
        let turn = assistant_message(None, vec![AssistantContent::Text(Text::new("done"))]);
        assert_eq!(project(std::slice::from_ref(&turn)), vec![turn]);
    }

    /// An assistant turn whose results have not landed is not a complete unit,
    /// so the prefix stops before it and the earlier units are still writable.
    #[test]
    fn an_unanswered_call_is_outside_the_complete_prefix() {
        let first = Message::User {
            content: vec![UserContent::Text(Text::new("do it"))],
        };
        let messages = vec![first.clone(), edit_turn()];
        assert_eq!(complete_prefix_len(&messages), 1);
        assert_eq!(project(&messages[..1]), vec![first]);
    }

    /// The invariant the whole parts model rests on: a parts list exists only
    /// inside the run, between `answer` and the next projection, and a
    /// projection never emits one. Asserted on the projection's output rather
    /// than on any one caller, because the projection is the only thing that
    /// can put a message in front of a provider or a session.
    #[test]
    fn a_projection_never_emits_a_parts_list() {
        let parts: Vec<CallPart> = ["a.rs", "b.rs"]
            .iter()
            .map(|path| CallPart {
                args: serde_json::json!({ "path": path, "edits": [] }),
                result: format!("edited {path}"),
                is_error: false,
            })
            .collect();
        let results = Message::User {
            content: vec![UserContent::ToolResult(parts_result("t1", &parts))],
        };
        let out = project(&[edit_turn(), results]);
        for message in &out {
            let Message::User { content } = message else {
                continue;
            };
            for c in content {
                let UserContent::ToolResult(tr) = c else {
                    continue;
                };
                assert!(
                    crate::agent::parts::parts_of_message_result(tr).is_none(),
                    "a parts list reached the projection's output: {tr:?}"
                );
            }
        }
    }

    /// Loaded history is already projected, so a second pass is a byte-level
    /// no-op. This is what lets the loop project the *whole* history on every
    /// request rather than tracking a boundary inside it.
    #[test]
    fn projecting_projected_history_is_a_fixed_point() {
        let parts: Vec<CallPart> = ["a.rs"]
            .iter()
            .map(|path| CallPart {
                args: serde_json::json!({ "path": path, "edits": [] }),
                result: format!("edited {path}"),
                is_error: false,
            })
            .collect();
        let results = Message::User {
            content: vec![UserContent::ToolResult(parts_result("t1", &parts))],
        };
        let once = project(&[edit_turn(), results]);
        assert_eq!(project(&once), once);
    }

    #[test]
    fn a_complete_unit_is_the_whole_prefix() {
        let results = Message::User {
            content: vec![UserContent::ToolResult(parts_result("t1", &[]))],
        };
        let messages = vec![edit_turn(), results];
        assert_eq!(complete_prefix_len(&messages), 2);
    }
}
