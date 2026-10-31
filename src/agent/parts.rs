//! One tool call as the tool understood it, plus that call's result.
//!
//! A tool holds every fact about how its call should be spelled: it parsed the
//! arguments, it knows how many files it was given, and it knows what each
//! file's result was. Before this module existed those facts were thrown away
//! with the tool's return value and reconstructed later by re-parsing the call
//! and the joined result — three readers of one derived shape, each of which
//! could drift from the others.
//!
//! So a tool's answer is a list of [`CallPart`]s: the call it understood, with
//! the result of exactly that call. One part is the common case; `edit` returns
//! one per file. Everything downstream — persistence, the model-facing history
//! and the display — unpacks the same list, so there is nothing left to
//! reconstruct and nothing to disagree about.
//!
//! See `doc/261030_rig_direct_drive.md` §4.

use rig_core::message::CallId;

/// One call as the tool understood it, with its result.
///
/// Deliberately carries **no id**: the framework mints ids when it unpacks
/// ([`crate::edit_blocks::split_id`]), because the provider issued exactly one
/// id and which of the parts keeps it is a protocol rule, not a tool's
/// business. A tool that minted ids would be a second place that rule could go
/// wrong.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CallPart {
    /// The call's arguments, as the tool read them: paths relativized, a
    /// `cd <cwd> &&` prefix stripped, an `edit` split to one file per part.
    pub args: serde_json::Value,
    /// The result for exactly this call.
    pub result: String,
    /// Whether this part failed. A part may fail while the call succeeded:
    /// one of five files in an `edit` batch can fail to apply while the batch
    /// as a whole reports success. That is the honest model of a per-file
    /// outcome, and it is why this is not simply the call's disposition.
    #[serde(default)]
    pub is_error: bool,
}

impl CallPart {
    /// The single part of a call that is one call: the args as understood, and
    /// the call's one result.
    pub fn single(args: serde_json::Value, result: impl Into<String>, is_error: bool) -> Vec<Self> {
        vec![Self {
            args,
            result: result.into(),
            is_error,
        }]
    }
}

/// The parts a tool result's content represents, if it is a parts list.
///
/// A parts list is a JSON array whose every element is a [`CallPart`]. There is
/// no marker key and no wrapper object — the array *is* the answer — which is
/// safe only because of where this runs, and that is worth stating plainly
/// because the safety is not in the shape:
///
/// - only the drive loop puts parts in a message, and it does so for the calls
///   it just executed;
/// - a tool that returns plain text yields a *text* content block, never JSON,
///   so a `bash` result that happens to print this exact array stays text;
/// - what a session holds was written already unpacked, so a loaded message
///   cannot be a parts list — the reader waves nothing through that passed
///   through persistence.
///
/// The one caller that must be careful is the projection, which sees both: it
/// unpacks only at or above the boundary of this run's own messages. See
/// [`parts_of_result`].
pub(crate) fn as_parts(value: &serde_json::Value) -> Option<Vec<CallPart>> {
    let array = value.as_array()?;
    if array.is_empty() {
        return None;
    }
    array
        .iter()
        .map(|v| serde_json::from_value::<CallPart>(v.clone()).ok())
        .collect()
}

/// The [`CallPart`]s carried by a tool result's content blocks, if any.
///
/// Only a `Json` block can carry them: [`CallPart`]s serialize to an array, and
/// every prior tool that answers one call returns a `String` (a text block).
/// So a text block — however JSON-shaped its contents — is never read as a
/// parts list.
pub(crate) fn parts_of_result(result: &rig_core::tool::ToolResult) -> Option<Vec<CallPart>> {
    result.output().as_json().and_then(as_parts)
}

/// The [`CallPart`]s carried by a committed message-level tool result, if any.
///
/// The mirror of [`parts_of_result`] for the other side of the boundary: a
/// result that has entered history (the projection's input) rather than one the
/// tool just answered.
pub(crate) fn parts_of_message_result(
    result: &rig_core::message::ToolResult,
) -> Option<Vec<CallPart>> {
    result.content.iter().find_map(|c| match c {
        rig_core::message::ToolResultContent::Json { value } => as_parts(value),
        _ => None,
    })
}

/// The model-visible text of a [`CallPart`] list.
///
/// One part renders as itself; several render as a JSON array of their results.
/// Persistence never sees a list (unpacking gives each part its own message), so
/// this exists for tests and for any caller that wants the whole answer as text.
#[cfg(test)]
pub(crate) fn parts_text(parts: &[CallPart]) -> String {
    match parts {
        [one] => one.result.clone(),
        _ => serde_json::Value::Array(
            parts
                .iter()
                .map(|p| serde_json::Value::String(p.result.clone()))
                .collect(),
        )
        .to_string(),
    }
}

/// The call ids of a parts list, in order: the original id for the first part
/// and derived ids for the rest.
pub(crate) fn part_ids(orig: &CallId, n: usize) -> Vec<CallId> {
    (0..n)
        .map(|i| crate::edit_blocks::split_id(orig, i, n))
        .collect()
}

/// Replace each call that answered with a parts list by `parts.len()` calls,
/// each with its own result.
///
/// This is the whole of "splitting": the tool stated the split, so there is
/// nothing to parse, pair or guess here. A call whose result is not a parts list
/// — one that never ran (denied, interrupted), or a plain-text tool — is left
/// exactly as the model wrote it.
///
/// Ids are minted here rather than by the tool: the provider issued one id, and
/// which part keeps it is a protocol rule (see [`part_ids`]).
///
/// `is_error` becomes per-part. rig committed the batch result with the *call's*
/// disposition, and projection replaces it with the part's — which is why a
/// successful `edit` whose third file failed to apply still shows one failed
/// result. Two audiences, two records: the call's disposition still drove
/// execution, telemetry and the fail-closed checks.
pub(crate) fn unpack(
    assistant: rig_core::message::Message,
    results: Vec<rig_core::message::UserContent>,
) -> (
    rig_core::message::Message,
    Vec<rig_core::message::UserContent>,
) {
    use rig_core::message::{AssistantContent, ToolFunction, ToolResult, ToolResultContent};
    use rig_core::message::{Message, UserContent};

    let Message::Assistant(turn) = assistant else {
        return (assistant, results);
    };
    // Fast path: nothing to unpack, so nothing is rebuilt. This is every turn
    // that ran no multi-result tool, i.e. nearly all of them.
    if !results.iter().any(|c| match c {
        UserContent::ToolResult(tr) => parts_of_message_result(tr).is_some(),
        _ => false,
    }) {
        return (Message::Assistant(turn), results);
    }

    let mut by_call: std::collections::HashMap<rig_core::message::CallId, UserContent> =
        std::collections::HashMap::new();
    let mut others = Vec::new();
    for c in results {
        match &c {
            UserContent::ToolResult(tr) => {
                by_call.insert(tr.call.clone(), c);
            }
            _ => others.push(c),
        }
    }

    let response = crate::agent::transcript::response_id(&turn).map(str::to_string);
    let mut calls = Vec::new();
    let mut out_results = Vec::new();
    for content in turn.content {
        let AssistantContent::ToolCall(tc) = content else {
            calls.push(content);
            continue;
        };
        // Take the answer out of the map exactly once: a call nothing answered
        // has none, and a call that was answered must not lose its result to a
        // second lookup.
        let answered = by_call.remove(&tc.id);
        let parts = answered
            .as_ref()
            .and_then(|c| match c {
                UserContent::ToolResult(tr) => parts_of_message_result(tr),
                _ => None,
            })
            .filter(|p| p.len() > 1);
        let Some(parts) = parts else {
            // One call, one result — or a call nothing answered. Either way the
            // model's own spelling stands, and the committed result rides along
            // untouched.
            if let Some(r) = answered {
                out_results.push(r);
            }
            calls.push(AssistantContent::ToolCall(tc));
            continue;
        };
        let ids = part_ids(&tc.id, parts.len());
        for (i, part) in parts.into_iter().enumerate() {
            calls.push(AssistantContent::ToolCall(rig_core::message::ToolCall {
                id: ids[i].clone(),
                function: ToolFunction::new(tc.function.name.clone(), part.args),
                // A synthesized call has no provider id or signature: the
                // provider issued one of each, and N copies of one provider id
                // would be unmatchable on a wire that keys results on it.
                native: None,
            }));
            out_results.push(UserContent::ToolResult(ToolResult {
                call: ids[i].clone(),
                name: tc.function.name.clone(),
                content: vec![ToolResultContent::text(part.result)],
                is_error: part.is_error,
            }));
        }
        // The committed batch result is dropped with the call it answered: its
        // content *was* the parts, and each part now has its own.
    }
    out_results.extend(by_call.into_values());
    out_results.extend(others);

    // Rebuilding keeps the response id (pi's `responseId`) so a provider that
    // keys on it still matches this turn.
    (
        crate::agent::transcript::assistant_message(response, calls),
        out_results,
    )
}

#[cfg(test)]
mod integration_tests {
    //! End to end through the production path: a tool answers parts, the
    //! transcript persists them unpacked, and the same messages render back to
    //! the model.
    //!
    //! This is the property the whole design rests on — one value read by the
    //! panel, the record and the wire — so it is asserted across the seams
    //! rather than inside any one of them.

    use crate::agent::parts::{parts_of_message_result, CallPart};
    use crate::agent::transcript::{assistant_content, RunState, TurnTranscript};
    use rig_core::message::{Message, UserContent};

    fn two_file_parts() -> Vec<CallPart> {
        [
            ("a.rs", "Replaced 1 block(s) in a.rs"),
            ("b.rs", "could not edit b.rs: denied"),
        ]
        .iter()
        .map(|(path, result)| CallPart {
            args: serde_json::json!({
                "path": path,
                "edits": [{"oldText": "x", "newText": "X", "startLine": 1}],
            }),
            result: (*result).to_string(),
            is_error: path.contains('b'),
        })
        .collect()
    }

    fn parts_result(call: &str, parts: &[CallPart]) -> rig_core::message::ToolResult {
        rig_core::message::ToolResult {
            call: rig_core::message::CallId::from_wire(call),
            name: crate::agent::transcript::tool_name("edit"),
            content: vec![rig_core::message::ToolResultContent::Json {
                value: serde_json::to_value(parts).expect("parts serialize"),
            }],
            is_error: false,
        }
    }

    /// Store → render → store again. The stored form is already the projected
    /// form, so the second pass must be a byte-level no-op; if it were not, a
    /// session would drift further from its model on every request.
    #[test]
    fn a_stored_parts_answer_round_trips_through_render() {
        let mut t = TurnTranscript::default();
        t.start_live();
        t.accept(
            None,
            vec![rig_core::message::AssistantContent::ToolCall(
                crate::agent::transcript::tool_call(
                    "t1",
                    "edit",
                    serde_json::json!({ "blocks": "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END" }),
                ),
            )],
        );
        t.push_tool_result(parts_result("t1", &two_file_parts()));
        let batch = t.prepare(RunState::Ongoing).expect("a batch");

        // On disk: N calls, each one file, and N plain-text results.
        let calls: Vec<_> = assistant_content(&batch.messages[0])
            .iter()
            .filter_map(|c| match c {
                rig_core::message::AssistantContent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        for (call, path) in calls.iter().zip(["a.rs", "b.rs"]) {
            assert_eq!(call.function.arguments["path"], path);
            assert!(
                call.function.arguments["edits"].is_array(),
                "stored as pi's canonical array"
            );
        }
        let Message::User { content } = &batch.messages[1] else {
            panic!("a results message")
        };
        assert_eq!(content.len(), 2);
        for c in content {
            let UserContent::ToolResult(tr) = c else {
                panic!("a tool result")
            };
            // Plain text, never a parts list: nothing on disk needs recognising.
            assert!(parts_of_message_result(tr).is_none());
            assert_eq!(tr.content.len(), 1);
        }

        // Render for the model: block form, one call and one result per file,
        // and stable when the rendered history is rendered again.
        let once = crate::agent::transcript::render_model_facing(batch.messages.clone());
        let twice = crate::agent::transcript::render_model_facing(once.clone());
        assert_eq!(once, twice, "rendering is idempotent");
        let blocks = assistant_content(&once[0])
            .iter()
            .filter_map(|c| match c {
                rig_core::message::AssistantContent::ToolCall(tc) => Some(
                    tc.function.arguments["blocks"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(blocks.len(), 2, "one block-form call per file");
        assert!(blocks[0].starts_with("a.rs:1"), "{}", blocks[0]);
        assert!(blocks[1].starts_with("b.rs:1"), "{}", blocks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(result: &str) -> CallPart {
        CallPart {
            args: serde_json::json!({ "path": "a.rs" }),
            result: result.to_string(),
            is_error: false,
        }
    }

    #[test]
    fn a_parts_list_round_trips_through_json() {
        let parts = CallPart::single(serde_json::json!({"path": "src/x.rs"}), "done", false);
        let value = serde_json::to_value(&parts).expect("serializes");
        assert_eq!(as_parts(&value), Some(parts));
    }

    /// The array is the answer, so anything else must not be read as one. A
    /// `bash` result that prints a parts-shaped array is text, and text is
    /// never examined.
    #[test]
    fn only_a_complete_array_of_parts_is_a_parts_list() {
        assert_eq!(as_parts(&serde_json::json!("just text")), None);
        assert_eq!(as_parts(&serde_json::json!([])), None);
        assert_eq!(as_parts(&serde_json::json!([{"path": "a"}])), None);
        assert_eq!(
            as_parts(&serde_json::json!([{"args": {}, "result": "x", "is_error": false}])),
            Some(vec![CallPart {
                args: serde_json::json!({}),
                result: "x".into(),
                is_error: false,
            }])
        );
    }

    /// `is_error` defaults to false when a producer omits it, so a part list
    /// written by an older shape still reads.
    #[test]
    fn is_error_is_optional() {
        assert_eq!(
            as_parts(&serde_json::json!([{"args": {}, "result": "x"}])),
            Some(vec![CallPart {
                args: serde_json::json!({}),
                result: "x".into(),
                is_error: false,
            }])
        );
    }

    #[test]
    fn one_part_renders_as_its_own_text() {
        assert_eq!(
            parts_text(&[part("Replaced 1 block(s) in a.rs")]),
            "Replaced 1 block(s) in a.rs"
        );
    }

    #[test]
    fn several_parts_render_as_their_result_array() {
        let text = parts_text(&[part("one"), part("two")]);
        let parsed = bsed_blockedit::parse_file_results(&text);
        assert!(
            parsed.is_none(),
            "results are bare strings, not file entries"
        );
        assert_eq!(text, r#"["one","two"]"#);
    }

    /// The first part keeps the provider's id: some providers require the issued
    /// id to be answered within its own turn.
    #[test]
    fn the_first_part_keeps_the_provider_id() {
        let orig = CallId::from_wire("call_1");
        let ids = part_ids(&orig, 3);
        assert_eq!(ids[0], orig);
        assert_eq!(ids[1].to_string(), "call_1_f1n3");
    }
}
