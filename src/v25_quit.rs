//! v2.5 `SlashDotNode` implementation for `/quit`.
//!
//! Leaf node: submit() latches the `quit` control target on `AppState`
//! (declarative — never cleared); the agent loop observes it at the cycle
//! point and breaks.
use std::sync::Arc;

use std::rc::Rc;

use crate::app_state::AppState;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult,
};

pub struct QuitNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for QuitNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("quit")
                .desc("quit the session")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec!["Quit the session.".to_string()]
    }

    async fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        self.app_state.request_quit();
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

    fn build_quit_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(QuitNode { app_state: app_state.clone() }.into());
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn quit_shows_in_listing() {
        let app_state = test_app_state();
        let mut menu = build_quit_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("", HostKey::None, 80));
        match result {
            MenuResult::Continue(plan, _) => {
                let text: String = plan.rows.iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("quit"), "quit should be in listing: {}", text);
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn quit_submit_latches_quit_target() {
        let app_state = test_app_state();
        let mut menu = build_quit_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ = futures::executor::block_on(handle.eval_prompt("quit ", HostKey::None, 80));
        let _ = futures::executor::block_on(handle.eval_prompt("quit ", HostKey::Enter, 80));
        assert!(app_state.should_quit(), "submit must latch the quit target");
        // Latching: a GC drain never clears it.
        let taken = app_state.take_control();
        assert!(taken.quit);
        assert!(app_state.should_quit());
    }

    #[test]
    fn quit_header_text() {
        let app_state = test_app_state();
        let mut menu = build_quit_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ = futures::executor::block_on(handle.eval_prompt("quit ", HostKey::None, 80));
        match futures::executor::block_on(handle.eval_prompt("quit ", HostKey::None, 80)) {
            MenuResult::Continue(plan, _) => {
                let header: String = plan.detail.iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(header.contains("Quit the session"), "{}", header);
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }
}
