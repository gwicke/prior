//! v2.5 `SlashDotNode` implementation for `/replay`.
//!
//! Leaf node: header() shows help text; submit() writes the `replay`
//! control target on `AppShared` — the agent loop executes it (needs the
//! active session + output, both loop-owned) at the cycle point.

use std::rc::Rc;

use crate::app_state::SharedApp;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult,
};

pub struct ReplayNode {
    pub app: SharedApp,
}

impl SlashDotNode for ReplayNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("replay")
                .desc("history replay; optional limits 10 (turns) 10t (turns) 10l (lines)")],
            active_ids: Vec::new(),
        }))
    }

    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
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

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let arg = path_args.suffix.trim().to_string();
        self.app.request_replay(arg);
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

    fn build_replay_menu(app: &SharedApp) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(ReplayNode { app: app.clone() }.into());
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn replay_shows_help_in_header() {
        let app = make_app();
        let mut menu = build_replay_menu(&app);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("replay ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let header: String = plan.detail.iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(header.contains("Replay the full history"), "{}", header);
                assert!(header.contains("10t"), "{}", header);
                assert!(header.contains("10l"), "{}", header);
            }
            other => panic!("expected Continue, got {:?}",
                std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn replay_submit_sets_replay_target() {
        let app = make_app();
        let mut menu = build_replay_menu(&app);
        let mut handle = menu.get_handle();
        // Drill in then Enter.
        let _ = handle.eval_prompt("replay ", HostKey::None, 80);
        let _ = handle.eval_prompt("replay ", HostKey::Enter, 80);
        let taken = app.take_control();
        assert_eq!(taken.replay.as_ref().unwrap().arg, "");
    }

    #[test]
    fn replay_with_suffix_sets_replay_target() {
        let app = make_app();
        let mut menu = build_replay_menu(&app);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("replay ", HostKey::None, 80);
        let _ = handle.eval_prompt("replay 10t ", HostKey::Enter, 80);
        let taken = app.take_control();
        assert_eq!(taken.replay.as_ref().unwrap().arg, "10t");
    }
}
