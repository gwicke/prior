//! v2.5 `SlashDotNode` implementation for `/replay`.
//!
//! Leaf node: header() shows help text; submit() writes the `replay`
//! control target on `AppState` — the agent loop executes it (needs the
//! active session + output, both loop-owned) at the cycle point.
use std::sync::Arc;

use std::rc::Rc;

use crate::app_state::AppState;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode, SubmitResult,
};

pub struct ReplayNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for ReplayNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("replay")
                .desc("history replay; optional limits 10 (turns) 10t (turns) 10l (lines)")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        vec![
            "Replay the full history of the current session.".to_string(),
            "".to_string(),
            "Optional length limits (appended after /replay):".to_string(),
            "  10      last 10 agent turns (same as 10t)".to_string(),
            "  10t     last 10 agent turns".to_string(),
            "  10l     last 10 output lines".to_string(),
            "".to_string(),
            "Examples:  /replay    /replay 5    /replay 50l".to_string(),
        ]
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let arg = path_args.suffix.trim().to_string();
        // Data-plane inline: replay renders through the shared output
        // handle; no active session → the same message the old applier
        // showed. Session and output are both reachable via the slot.
        let out = self.app_state.with_config(|c| c.output.clone());
        match self.app_state.session() {
            Some(s) => crate::agent::handle_replay(&arg, &s, out.as_ref()).await,
            None => out.display_text("No active session to replay"),
        }
        SubmitResult::Done(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{MenuResult, StaticListingNode};
    use crate::slashdot_menu::HostKey;

    fn test_settings() -> crate::settings::SettingsStore {
        crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_thinking_level: String::new(),
            show_reasoning: String::new(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            path: None,
        }
    }

    fn test_app_state() -> Arc<AppState> {
        crate::app_state::AppState::for_test(test_settings())
    }

    fn test_app_state_with(output: Arc<dyn crate::output::Output>) -> Arc<AppState> {
        crate::app_state::AppState::for_test_with_output(test_settings(), output)
    }

    /// One user message the replay assertions look for.
    fn replay_entry() -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: "e1".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: crate::session::SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "hello-replay"}]
                }),
            },
        }
    }

    fn build_replay_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(
            ReplayNode {
                app_state: app_state.clone(),
            }
            .into(),
        );
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn replay_shows_help_in_header() {
        let app_state = test_app_state();
        let mut menu = build_replay_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("replay ", HostKey::None, 80));
        match result {
            MenuResult::Continue(plan, _) => {
                let header: String = plan
                    .detail
                    .iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(header.contains("Replay the full history"), "{}", header);
                assert!(header.contains("10t"), "{}", header);
                assert!(header.contains("10l"), "{}", header);
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn replay_submit_without_session_displays_message() {
        let (out, lines) = crate::output::RecordingOutput::new();
        let app_state = test_app_state_with(out);
        let mut menu = build_replay_menu(&app_state);
        let mut handle = menu.get_handle();
        // Drill in then Enter.
        let _ = futures::executor::block_on(handle.eval_prompt("replay ", HostKey::None, 80));
        let _ = futures::executor::block_on(handle.eval_prompt("replay ", HostKey::Enter, 80));
        let captured = crate::output::RecordingOutput::joined(&lines);
        assert!(
            captured.contains("No active session to replay"),
            "expected the no-session message, got: {captured}"
        );
    }

    #[test]
    fn replay_with_suffix_renders_history() {
        let (out, lines) = crate::output::RecordingOutput::new();
        let app_state = test_app_state_with(out);
        let session = crate::agent::test_session_with_entries(vec![replay_entry()]);
        app_state.set_session(Some(session));
        let mut menu = build_replay_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ = futures::executor::block_on(handle.eval_prompt("replay ", HostKey::None, 80));
        let _ = futures::executor::block_on(handle.eval_prompt("replay 10t ", HostKey::Enter, 80));
        let captured = crate::output::RecordingOutput::joined(&lines);
        assert!(
            captured.contains("hello-replay"),
            "replay must render the session history, got: {captured}"
        );
    }
}
