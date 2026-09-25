//! v2.5 `SlashDotNode` implementation for `/quit`.
//!
//! Leaf node: submit() latches the `quit` control target on `AppShared`
//! (declarative — never cleared); the agent loop observes it at the cycle
//! point and breaks.

use std::rc::Rc;

use crate::app_state::SharedApp;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult,
};

pub struct QuitNode {
    pub app: SharedApp,
}

impl SlashDotNode for QuitNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("quit")
                .desc("quit the session")],
            active_ids: Vec::new(),
        }))
    }

    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec!["Quit the session.".to_string()]
    }

    fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        self.app.request_quit();
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

    fn make_app() -> SharedApp {
        crate::app_state::AppShared::for_test(crate::settings::SettingsStore {
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

    fn build_quit_menu(app: &SharedApp) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(QuitNode { app: app.clone() }.into());
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn quit_shows_in_listing() {
        let app = make_app();
        let mut menu = build_quit_menu(&app);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("", HostKey::None, 80);
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
        let app = make_app();
        let mut menu = build_quit_menu(&app);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("quit ", HostKey::None, 80);
        let _ = handle.eval_prompt("quit ", HostKey::Enter, 80);
        assert!(app.should_quit(), "submit must latch the quit target");
        // Latching: a GC drain never clears it.
        let taken = app.take_control();
        assert!(taken.quit);
        assert!(app.should_quit());
    }

    #[test]
    fn quit_header_text() {
        let app = make_app();
        let mut menu = build_quit_menu(&app);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("quit ", HostKey::None, 80);
        match handle.eval_prompt("quit ", HostKey::None, 80) {
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
