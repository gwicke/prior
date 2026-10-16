//! Replaying a past session into the live area.
//!
//! Reading is not rendering: [`build_replay_units`] turns a branch of raw
//! [`SessionEntry`](crate::session::SessionEntry) values into a flat
//! [`ReplayUnit`] list with compaction and branch-summary markers kept in
//! place, and only then does [`replay_units`] paint it. The split matters
//! because `/replay N` needs to *count* turns without painting any of them,
//! and because the live path (`prompt()`) and the replay path must produce
//! identical output — which [`replay_paints_what_the_live_path_paints`]
//! pins, since a divergence there would be invisible until someone compared
//! two screens.
//!
//! `AssistantSegment` is the unit of grouping: the content between tool
//! executions. A turn is a user prompt followed by a chain of them.

use crate::content::Content;
// Tests only: the byte-comparison below drives `ConsoleOutput` through the
// `Output` trait's own verbs. Production content calls `Content` directly.
#[cfg(test)]
use crate::Output;
use rig_core::message::{
    AssistantContent, Message, ReasoningContent, ToolResultContent, UserContent,
};

// ---------------------------------------------------------------------------
// Session replay helpers
// ---------------------------------------------------------------------------

/// One assistant segment: the content streamed between tool executions (or the
/// whole final answer when there are no more tools to run in this turn).
pub(crate) struct AssistantSegment {
    /// Reasoning/thinking blocks in message order.
    thinking: Vec<String>,
    /// Answer text blocks in message order.
    text: Vec<String>,
    /// Tool calls from the assistant message: `(call_id, name, args)`.
    /// Populated during grouping; matched against results when the following
    /// `User { ToolResult }` message is processed.
    tool_calls: Vec<(String, String, serde_json::Value)>,
    /// Tool results keyed by call ID: `(call_id, text)`.
    /// Populated when the subsequent `User { ToolResult }` message is seen.
    tool_results: Vec<(String, String)>,
}

impl AssistantSegment {
    fn new() -> Self {
        Self {
            thinking: Vec::new(),
            text: Vec::new(),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        }
    }
}

/// One complete conversation turn: the user's text prompt followed by the
/// full chain of assistant segments (`text/reasoning → tool block → …`).
pub(crate) struct ConversationTurn {
    user_text: String,
    segments: Vec<AssistantSegment>,
}

/// One replayable item from the raw session branch.
///
/// Replay walks the FULL branch (root→leaf) so compaction entries render as
/// visible markers while the conversation replays all the way to the
/// beginning; only the agent context is prefix-limited at the compaction
/// scope (see `Session::build_context` / `apply_compaction_transforms`).
pub(crate) enum ReplayUnit {
    Turn(ConversationTurn),
    /// A compaction boundary marker. Everything on the branch after this
    /// entry is what the agent sees (it was written with `firstKeptEntryId`).
    /// `summary` is the compaction's summary text (empty for `manual`
    /// mode on a no-file span); when non-empty it is rendered as part
    /// of the marker.
    Compaction {
        tokens_before: u64,
        summary: String,
    },
    /// A branch-summary marker: the digest `/tree <id> switch summarize`
    /// writes when it moves the cursor off a spur. Not a turn — the marker
    /// renders in path order with `summary` as markdown below it, exactly
    /// like a compaction. `from_id` is the branch-from position (pi's
    /// `fromId`), surfaced in the marker so the id is copy-pasteable into
    /// `/tree <id>`.
    BranchSummary {
        from_id: String,
        summary: String,
    },
}

/// Build replay units from the raw session branch (root->leaf).
///
/// Replay walks the FULL branch so compaction entries become
/// `ReplayUnit::Compaction` markers interleaved with turns, giving the user
/// the whole history while only the agent context is prefix-limited at the
/// compaction scope. Tool-result entries are batched into the preceding
/// assistant segment exactly like `extract_messages`/`group_into_turns`, so an
/// uncompacted branch produces the same turns as the old context-based path.
pub(crate) fn build_replay_units(branch: &[crate::session::SessionEntry]) -> Vec<ReplayUnit> {
    use crate::session::SessionEntryContent;

    let mut units: Vec<ReplayUnit> = Vec::new();
    let mut open: Option<ConversationTurn> = None;
    // Pending tool-result pi messages (batched into the next assistant flush).
    let mut pending_tools: Vec<serde_json::Value> = Vec::new();

    fn flush_pending_tools(
        pending: &mut Vec<serde_json::Value>,
        open: &mut Option<ConversationTurn>,
    ) {
        if pending.is_empty() {
            return;
        }
        if let Some(turn) = open.as_mut() {
            let mut seg_vec = Vec::new();
            if let Some(seg) = turn.segments.last_mut() {
                let trs = std::mem::take(pending);
                for tr in trs {
                    // Reuse the pi -> rig tool-result conversion (pub(crate)).
                    if let Some(Message::User { content }) =
                        crate::session::pi_toolresults_to_rig_message(vec![tr])
                    {
                        seg_vec.extend(content);
                    }
                }
                for item in seg_vec {
                    if let UserContent::ToolResult(tr) = item {
                        let text = tr
                            .content
                            .iter()
                            .filter_map(|rc| match rc {
                                ToolResultContent::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        seg.tool_results.push((tr.call.clone().into_string(), text));
                    }
                }
            }
        }
        pending.clear();
    }

    for entry in branch {
        match &entry.content {
            SessionEntryContent::Message { message } => {
                let role = message.get("role").and_then(|r| r.as_str());
                match role {
                    Some("toolResult") => pending_tools.push(message.clone()),
                    Some("user") | Some("assistant") => {
                        // End of a turn boundary? A user text message starts a
                        // new turn; flush pending tool results first.
                        if role == Some("user") {
                            flush_pending_tools(&mut pending_tools, &mut open);
                            if let Some(txt) = user_text_of(message) {
                                if let Some(t) = open.take() {
                                    units.push(ReplayUnit::Turn(t));
                                }
                                open = Some(ConversationTurn {
                                    user_text: txt,
                                    segments: Vec::new(),
                                });
                            }
                        } else {
                            // Assistant: attach to current turn (flush tools first).
                            flush_pending_tools(&mut pending_tools, &mut open);
                            if let Some(msg) = crate::session::pi_message_to_rig(message) {
                                feed_turn_into(&mut open, &msg);
                            }
                        }
                    }
                    _ => {}
                }
            }
            SessionEntryContent::Compaction {
                tokens_before,
                summary,
                ..
            } => {
                // A compaction boundary comes AFTER the turn it concludes:
                // flush any open turn first, then emit the marker.
                flush_pending_tools(&mut pending_tools, &mut open);
                if let Some(t) = open.take() {
                    units.push(ReplayUnit::Turn(t));
                }
                units.push(ReplayUnit::Compaction {
                    tokens_before: *tokens_before,
                    summary: summary.clone(),
                });
            }
            // Same placement rule as a compaction: the marker sits after the
            // turn it follows, before the prompt that continues on this branch.
            // A blank digest matches no arm — pi's context mapping skips an
            // empty summary, a marker with no body would be a fiction, and
            // `apply_branch_summary` refuses a blank draft anyway.
            SessionEntryContent::BranchSummary { from_id, summary }
                if !summary.trim().is_empty() =>
            {
                flush_pending_tools(&mut pending_tools, &mut open);
                if let Some(t) = open.take() {
                    units.push(ReplayUnit::Turn(t));
                }
                units.push(ReplayUnit::BranchSummary {
                    from_id: from_id.clone(),
                    summary: summary.clone(),
                });
            }
            _ => {}
        }
    }
    flush_pending_tools(&mut pending_tools, &mut open);
    if let Some(t) = open.take() {
        units.push(ReplayUnit::Turn(t));
    }
    units
}

/// Extract the first text block of a pi user AgentMessage.
fn user_text_of(message: &serde_json::Value) -> Option<String> {
    message
        .pointer("/content/0/text")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Feed one assistant rig Message into the open turn (assistant segments
/// always belong to the current turn).
fn feed_turn_into(open: &mut Option<ConversationTurn>, msg: &Message) {
    let Some(turn) = open.as_mut() else { return };
    let Message::Assistant { content, .. } = msg else {
        return;
    };
    let mut seg = AssistantSegment::new();
    for item in content.iter() {
        match item {
            AssistantContent::Reasoning(r) => {
                let t: String = r
                    .content
                    .iter()
                    .filter_map(|rc| match rc {
                        ReasoningContent::Text { text, .. } => Some(text.as_str()),
                        ReasoningContent::Summary(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !t.is_empty() {
                    seg.thinking.push(t);
                }
            }
            AssistantContent::Text(t) if !t.text.is_empty() => {
                seg.text.push(t.text.clone());
            }
            AssistantContent::ToolCall(tc) => {
                seg.tool_calls.push((
                    tc.id.clone().into_string(),
                    tc.function.name.clone(),
                    tc.function.arguments.clone(),
                ));
            }
            _ => {}
        }
    }
    turn.segments.push(seg);
}

/// Render a single `ConversationTurn` through the Output event sequence
/// (identical to the live streaming path).
///
/// The prompt's rules are the committed prompt's own, so this no longer
/// decides whether a separator precedes it: a prompt straight after another
/// commit shares the rule it is already closed with, and one that opens a
/// turn draws its own. The rule that used to sit between the prompt and the
/// response is that closing rule too — `begin_response` then puts the blank
/// line on it, which is the spacing a turn's prompt and its output want.
fn render_turn(content: &Content, turn: &ConversationTurn) {
    content.display_committed_prompt(&turn.user_text);

    if turn.segments.is_empty() {
        return;
    }

    content.begin_response();

    let seg_count = turn.segments.len();
    for (seg_idx, segment) in turn.segments.iter().enumerate() {
        let is_last_segment = seg_idx + 1 == seg_count;

        for thinking in &segment.thinking {
            content.stream_thinking(thinking);
        }
        for text in &segment.text {
            content.stream_text(text);
        }

        if !segment.tool_calls.is_empty() {
            content.end_response_segment();
            for (call_id, name, args) in &segment.tool_calls {
                content.display_tool_call(name, args);
                if let Some((_, result_text)) =
                    segment.tool_results.iter().find(|(rid, _)| rid == call_id)
                {
                    content.display_tool_result(result_text);
                }
            }
            // Mirror the live path, where each `CompletionCall` — including one
            // that closes a tool round and is followed by more assistant text —
            // fires `end_response_segment`. Flushing after the tool block here
            // guarantees a blank line separates the tool result from any text
            // segment that follows it in the replay, exactly as in live output.
            if !is_last_segment {
                content.end_response_segment();
            }
        } else if !is_last_segment {
            content.end_response_segment();
        }
    }
    content.end_response();
}

/// Replay a slice of units starting at `start`. Compaction markers render as a
/// dim system notice between turns; `Nt`/`Nl` limits count TURNS (markers are
/// not turns and are always drawn when inside the selected window).
pub(crate) fn replay_units(content: &Content, units: &[ReplayUnit], start: usize) {
    // One frame re-emit for the whole replay, not one per line of history
    // (ui_draw_v2 R5). All three callers — `/replay`, resume, and the silent
    // Ctrl-Z restore — arrive here, so this is the one place the batching has
    // to be declared.
    //
    // The guard names the hub, not the content: batching defers
    // *reposition*, and reposition is the hub's. The manager handle comes
    // from beside `Content` — the handle its own layer holds down to
    // (design §Content: content holds `Arc<Mutex<RenderManager>>`).
    //
    // Deliberately *not* around live streaming: `stream_text` enters the same
    // writer once per delta, and there the per-write re-emit is what keeps the
    // input row visible while text flows past it.
    let _batch = crate::render::ContentBatchGuard::open(&content.manager);
    // No leading-separator bookkeeping: a turn's prompt brackets itself, and
    // the rule that used to be forced between turns *after* the first is
    // the closing rule of the turn above. Compaction markers are not turns
    // and never forced one.
    for unit in &units[start..] {
        match unit {
            ReplayUnit::Turn(turn) => {
                render_turn(content, turn);
            }
            ReplayUnit::Compaction {
                tokens_before,
                summary,
            } => {
                content.display_system(&format!("[compaction: {}k tokens]", tokens_before / 1000,));
                // A text-only compaction carries its summary — render it as
                // markdown right below the marker so the compaction point is
                // self-documenting in the replay, exactly as it was shown
                // immediately after the compaction was performed.
                if !summary.is_empty() {
                    content.display_markdown(summary);
                }
            }
            ReplayUnit::BranchSummary { from_id, summary } => {
                // The id is opaque but copy-pasteable: `/tree <id>` switches
                // there. Same shape as a compaction marker — dim system line,
                // then the digest as markdown.
                content.display_system(&format!("[branch summary from {}]", from_id));
                content.display_markdown(summary);
            }
        }
    }
}

/// Count the conversation turns in a unit list (compaction markers are not
/// turns).
pub(crate) fn count_turns(units: &[ReplayUnit]) -> usize {
    units
        .iter()
        .filter(|u| matches!(u, ReplayUnit::Turn(_)))
        .count()
}

/// Return the unit index where the last `n` turns begin (0 when `n` >= the
/// total). Markers in that window are included in the render — only the
/// leading turns before the window are dropped, matching `/replay N` semantics
/// where the limit applies to turns.
pub(crate) fn last_turns_start(units: &[ReplayUnit], n: usize) -> usize {
    if n == 0 {
        return units.len();
    }
    let mut seen = 0usize;
    let mut idx = units.len();
    while idx > 0 {
        idx -= 1;
        if matches!(units[idx], ReplayUnit::Turn(_)) {
            seen += 1;
            if seen == n {
                break;
            }
        }
    }
    if seen < n {
        0
    } else {
        idx
    }
}

/// Replay the last `n` turns (plus any markers inside that window) from a
/// unit list.
pub(crate) fn replay_last_turns(content: &Content, units: &[ReplayUnit], n: usize) {
    let start = last_turns_start(units, n);
    replay_units(content, units, start);
}

/// Parse a replay-length argument: plain number `N`, `Nt` (turns), or `Nl`
/// (lines).  Returns `(count, is_lines)`.  An empty/whitespace input returns
/// `(default, false)`.
pub(crate) fn parse_replay_len_arg(arg: &str, default: usize) -> (usize, bool) {
    let arg = arg.trim();
    if arg.is_empty() {
        return (default, false);
    }
    if arg.ends_with('l') {
        let n: usize = arg.trim_end_matches('l').trim().parse().unwrap_or(0);
        if n == 0 {
            (usize::MAX, false)
        } else {
            (n, true)
        }
    } else {
        let clean = arg.trim_end_matches('t').trim();
        let n: usize = clean.parse().unwrap_or(default);
        (n, false)
    }
}

/// Execute a `/replay` command: re-render session history.
///
/// Called from both the agent's submit handler and the v2.5 `ReplayNode`
/// (via `block_on`).
pub(crate) async fn handle_replay(arg: &str, session: &crate::session::Session, content: &Content) {
    let branch = session.get_branch().await;
    let units = build_replay_units(&branch);
    let total = count_turns(&units);
    if arg.ends_with('l') {
        let n: usize = arg.trim_end_matches('l').trim().parse().unwrap_or(0);
        if n > 0 {
            if total == 0 {
                content.display_text("Session is empty");
            } else {
                content.begin_capture();
                replay_units(content, &units, 0);
                let bytes = content.end_capture();
                let captured = String::from_utf8_lossy(&bytes);
                let lines: Vec<&str> = captured.lines().collect();
                let start = lines.len().saturating_sub(n);
                use std::io::Write as _;
                let stdout = std::io::stdout();
                let mut handle = stdout.lock();
                for line in &lines[start..] {
                    let _ = writeln!(handle, "{}", line);
                }
                let _ = handle.flush();
            }
        } else {
            replay_units(content, &units, 0);
        }
    } else {
        let (count, _is_lines) = if arg.is_empty() {
            (0, false)
        } else {
            parse_replay_len_arg(arg, 0)
        };
        if total == 0 {
            content.display_text("Session is empty");
        } else {
            let count = if count == 0 { total } else { count.min(total) };
            replay_last_turns(content, &units, count);
        }
    }
}

/// Quiet `/replay 1`: the last turn only — no status messages, no divider of
/// its own, and a no-op when the session has no turns yet. Used by the
/// key-driven resize repaint in `read_input`, which wipes the old frame and
/// replays the last turn to push reflow corruption out of view; that path must
/// stay silent (a session exists from startup, long before it has any turns to
/// replay).
pub(crate) async fn replay_last_turn_silent(session: &crate::session::Session, content: &Content) {
    let branch = session.get_branch().await;
    let units = build_replay_units(&branch);
    if count_turns(&units) == 0 {
        return;
    }
    // No rule of our own: the replayed turn's prompt brackets itself, and an
    // implicit replay after a resize has nothing above it worth separating
    // from. Its closing rule is also what the trailing `display_separator()`
    // used to paint — otherwise the repainted input sat flush against the
    // replayed turn — so the prompt's own bracket covers it.
    replay_last_turns(content, &units, 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::output::ConsoleOutput;

    // ── Phase 2: replay units ─────────────────────────────────────────

    fn pi_user_entry(id: &str, parent: Option<&str>, text: &str) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|p| p.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".to_string(),
            content: crate::session::SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    fn pi_assistant_entry(
        id: &str,
        parent: Option<&str>,
        text: &str,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|p| p.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".to_string(),
            content: crate::session::SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "assistant",
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    fn pi_branch_summary_entry(
        id: &str,
        parent: Option<&str>,
        from_id: &str,
        summary: &str,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|p| p.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".to_string(),
            content: crate::session::SessionEntryContent::BranchSummary {
                from_id: from_id.to_string(),
                summary: summary.to_string(),
            },
        }
    }

    fn pi_compaction_entry(
        id: &str,
        parent: Option<&str>,
        tokens: u64,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|p| p.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".to_string(),
            content: crate::session::SessionEntryContent::Compaction {
                summary: String::new(),
                first_kept_entry_id: "m0".to_string(),
                tokens_before: tokens,
            },
        }
    }
    #[test]
    fn build_replay_units_marks_compaction_and_keeps_all_turns() {
        let branch = vec![
            pi_user_entry("m0", None, "hello"),
            pi_assistant_entry("a0", Some("m0"), "hi there"),
            pi_compaction_entry("c1", Some("a0"), 42_000),
            pi_user_entry("m1", Some("c1"), "continue"),
        ];
        let units = build_replay_units(&branch);
        // [Turn(hello,hi), Compaction, Turn(continue)]
        assert_eq!(units.len(), 3);
        assert!(matches!(units[0], ReplayUnit::Turn(_)));
        match &units[1] {
            ReplayUnit::Compaction {
                tokens_before,
                summary,
            } => {
                assert_eq!(*tokens_before, 42_000);
                assert_eq!(summary, ""); // manual-mode compaction (no files touched)
            }
            _ => panic!("expected compaction marker"),
        }
        assert!(matches!(units[2], ReplayUnit::Turn(_)));
        // Both turns present → full history replays to the beginning.
        assert_eq!(count_turns(&units), 2);
    }

    #[test]
    fn build_replay_units_marks_a_branch_summary_in_path_order() {
        // A switch-summarize session: the abandoned turn, then the digest at
        // the switch target, then the prompt that continues on that branch.
        let branch = vec![
            pi_user_entry("m0", None, "try the side quest"),
            pi_assistant_entry("a0", Some("m0"), "working on it"),
            pi_branch_summary_entry("s1", Some("a0"), "m0", "## Goal\nship the thing"),
            pi_user_entry("m1", Some("s1"), "back to the main line"),
        ];
        let units = build_replay_units(&branch);
        assert_eq!(units.len(), 3, "[Turn, BranchSummary, Turn]");
        assert!(matches!(units[0], ReplayUnit::Turn(_)));
        match &units[1] {
            ReplayUnit::BranchSummary { from_id, summary } => {
                assert_eq!(from_id, "m0", "fromId is the branch-from position");
                assert_eq!(summary, "## Goal\nship the thing");
            }
            _ => panic!("expected a branch-summary marker"),
        }
        assert!(matches!(units[2], ReplayUnit::Turn(_)));
        // A marker is not a turn: the Nt/Nl windows count turns only.
        assert_eq!(count_turns(&units), 2);
    }

    #[test]
    fn build_replay_units_skips_an_empty_branch_summary() {
        // Nothing to say → no marker, matching pi's context mapping (which
        // skips an empty summary) rather than showing an empty notice.
        let branch = vec![
            pi_user_entry("m0", None, "hello"),
            pi_branch_summary_entry("s1", Some("m0"), "m0", "   "),
            pi_user_entry("m1", Some("s1"), "again"),
        ];
        let units = build_replay_units(&branch);
        assert_eq!(units.len(), 2, "an empty digest emits no marker");
        assert!(units.iter().all(|u| matches!(u, ReplayUnit::Turn(_))));
    }

    #[test]
    fn replay_units_renders_the_branch_summary_marker_and_digest() {
        let branch = vec![
            pi_user_entry("m0", None, "hello"),
            pi_branch_summary_entry("s1", Some("m0"), "m0", "## Goal\nship it"),
            pi_user_entry("m1", Some("s1"), "continue"),
        ];
        let units = build_replay_units(&branch);
        let (out, lines) = crate::output::RecordingOutput::new();
        let content = Content::with_tap(out);
        replay_units(&content, &units, 0);
        let captured = crate::output::RecordingOutput::joined(&lines);
        assert!(
            captured.contains("[branch summary from m0]"),
            "marker carries the copy-pasteable id: {captured}"
        );
        assert!(
            captured.contains("ship it"),
            "the digest renders below the marker: {captured}"
        );
    }

    #[test]
    fn build_replay_units_uncompacted_has_all_turns() {
        let branch = vec![
            pi_user_entry("m0", None, "q1"),
            pi_assistant_entry("a0", Some("m0"), "r1"),
            pi_user_entry("m1", Some("a0"), "q2"),
            pi_assistant_entry("a1", Some("m1"), "r2"),
        ];
        let units = build_replay_units(&branch);
        assert_eq!(units.len(), 2);
        assert!(matches!(units[0], ReplayUnit::Turn(_)));
        assert!(matches!(units[1], ReplayUnit::Turn(_)));
        assert_eq!(count_turns(&units), 2);
        // last_turns_start: the last turn begins at unit index 1.
        assert_eq!(last_turns_start(&units, 1), 1);
    }

    #[test]
    fn last_turns_start_clamps_and_honors_markers() {
        // [T0] [C] [T1] [T2]
        let branch = vec![
            pi_user_entry("m0", None, "q0"),
            pi_compaction_entry("c1", Some("m0"), 10_000),
            pi_user_entry("m1", Some("c1"), "q1"),
            pi_user_entry("m2", Some("m1"), "q2"),
        ];
        let units = build_replay_units(&branch);
        assert_eq!(count_turns(&units), 3);
        // Last 2 turns: [T1, T2] → they start at unit index 2 (after [T0][C]).
        assert_eq!(last_turns_start(&units, 2), 2);
        // More turns than exist → clamp to 0.
        assert_eq!(last_turns_start(&units, 99), 0);
        // Zero turns requested → empty range (start == len).
        assert_eq!(last_turns_start(&units, 0), units.len());
    }
    /// One turn: reasoning, an answer, a tool round, then a second answer.
    fn two_segment_turn() -> ConversationTurn {
        ConversationTurn {
            user_text: "what is the answer?".into(),
            segments: vec![
                AssistantSegment {
                    thinking: vec!["Weighing the options.".into()],
                    text: vec!["Let me check.".into()],
                    tool_calls: vec![(
                        "c1".into(),
                        "Bash".into(),
                        serde_json::json!({"command": "echo hi"}),
                    )],
                    tool_results: vec![("c1".into(), "hi\n".into())],
                },
                AssistantSegment {
                    thinking: vec![],
                    text: vec!["The answer is 42.".into()],
                    tool_calls: vec![],
                    tool_results: vec![],
                },
            ],
        }
    }

    #[test]
    fn replay_paints_what_the_live_path_paints() {
        let turn = two_segment_turn();
        let reasoning = crate::settings::default_show_reasoning_handle();
        *reasoning.lock().unwrap() = crate::settings::ShowReasoning::Content;
        // Both sides on the same private size watch: the
        // byte comparison must not depend on the runner's terminal, and the
        // width is per-instance state now, so no serialization either.
        let size = crate::termio::TermSize {
            width: 80,
            height: 24,
        };

        // The live path, exactly as the streaming loop drives it. Reasoning is
        // asked onto the captured stream so the comparison sees it (a test's
        // stdout is not a terminal, and without this it would be routed to
        // stderr by both sides and the test would prove nothing).
        let live = ConsoleOutput::with_size(reasoning.clone(), size);
        live.content.set_reasoning_on_stderr(false);
        live.begin_capture();
        live.begin_response();
        for thinking in &turn.segments[0].thinking {
            live.stream_thinking(thinking);
        }
        for text in &turn.segments[0].text {
            live.stream_text(text);
        }
        live.end_response_segment();
        live.display_tool_call("Bash", &serde_json::json!({"command": "echo hi"}));
        live.display_tool_result("hi\n");
        live.end_response_segment();
        for text in &turn.segments[1].text {
            live.stream_text(text);
        }
        live.end_response();
        let live_bytes = live.end_capture();

        // The same turn through replay. `render_turn` draws the user prompt
        // (bracketed) and then the response, so the live output should be the
        // *tail* of what replay paints — byte for byte, reasoning included.
        let replay = ConsoleOutput::with_size(reasoning, size);
        replay.content.set_reasoning_on_stderr(false);
        replay.begin_capture();
        render_turn(&replay.content, &turn);
        let replay_bytes = replay.end_capture();

        let live_text = String::from_utf8_lossy(&live_bytes).to_string();
        let replay_text = String::from_utf8_lossy(&replay_bytes).to_string();
        assert!(
            replay_text.ends_with(&live_text),
            "replay did not end with the live output.\n--- live ---\n{live_text:?}\n--- replay ---\n{replay_text:?}"
        );
        assert!(
            replay_text.contains("Weighing the options"),
            "replay lost the reasoning; the two render paths diverged again"
        );
    }
}
