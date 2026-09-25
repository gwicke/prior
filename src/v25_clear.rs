//! v2.5 `SlashDotNode` implementation for `/clear`.
//!
//! Leaf node: submit() composes the `/clear` target declaratively — start a
//! fresh session AND delete the outgoing one afterwards
//! (`new_session` + `delete_session_ids_after`); the loop's applier defines
//! the order (create → swap → delete), idempotence makes replays safe.

use std::rc::Rc;

use crate::app_state::AppState;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult,
};

pub struct ClearNode {
    pub app_state: Arc<AppState>,
}

impl SlashDotNode for ClearNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("clear")
                .desc("delete session and start fresh")],
            active_ids: Vec::new(),
        }))
    }

    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Delete the current session and start fresh.".to_string(),
            "".to_string(),
            "The old session is removed from disk.".to_string(),
        ]
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let name = path_args.suffix.trim().to_string();
        let outgoing = self.app_state.current_session_id();
        self.app_state.request_clear(name, outgoing);
        SubmitResult::Done(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{
        MenuResult, StaticListingNode,
    };
    use crate::slashdot_menu::HostKey;

    fn test_app_state() -> Arc<AppState> {
        crate::app_state::AppState::for_test(crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_thinking_level: String::new(),
            show_reasoning: String::new(),
            reasoning_in_context: false,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            path: None,
        })
    }

    fn build_clear_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(ClearNode { app_state: app_state.clone() }.into());
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn clear_shows_in_listing() {
        let app_state = test_app_state();
        let mut menu = build_clear_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let text: String = plan.rows.iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("clear"), "clear should be in listing: {}", text);
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn clear_submit_composes_new_plus_delete_targets() {
        let app_state = test_app_state();
        app_state.set_current_session_id("outgoing-1".into());
        let mut menu = build_clear_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("clear ", HostKey::None, 80);
        let _ = handle.eval_prompt("clear ", HostKey::Enter, 80);
        let taken = app_state.take_control();
        assert_eq!(taken.new_session.as_deref(), Some(""));
        assert_eq!(taken.delete_session_ids_after, vec!["outgoing-1".to_string()]);
    }

    #[test]
    fn clear_with_name_composes_targets() {
        let app_state = test_app_state();
        let mut menu = build_clear_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("clear my-fresh-start ", HostKey::None, 80);
        let _ = handle.eval_prompt("clear my-fresh-start ", HostKey::Enter, 80);
        let taken = app_state.take_control();
        assert_eq!(taken.new_session.as_deref(), Some("my-fresh-start"));
        // No current session id → nothing to delete.
        assert!(taken.delete_session_ids_after.is_empty());
    }
}
