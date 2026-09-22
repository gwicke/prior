//! v2.5 `SlashDotNode` implementation for `/quit`.
//!
//! Leaf node: submit() sets `AsyncOp::Quit` on `MenuShared` so the agent
//! loop breaks on the next iteration.

use std::rc::Rc;

use crate::slash_menu_bridge::{AsyncOp, MenuShared};
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
};

pub struct QuitNode {
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for QuitNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("quit")
                .desc("quit the session")],
            active_ids: Vec::new(),
        }))
    }

    fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
        vec!["Quit the session.".to_string()]
    }

    fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        *self.shared.pending_op.borrow_mut() = Some(AsyncOp::Quit);
        SubmitResult::Done(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{
        HostKey, MenuResult, StaticListingNode,
    };

    fn make_shared() -> Rc<MenuShared> {
        Rc::new(MenuShared::new(
            crate::settings::SettingsStore {
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
            },
            None,
        ))
    }

    fn build_quit_menu(shared: &Rc<MenuShared>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(QuitNode { shared: shared.clone() }.into());
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn quit_shows_in_listing() {
        let shared = make_shared();
        let mut menu = build_quit_menu(&shared);
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
    fn quit_submit_sets_pending_op() {
        let shared = make_shared();
        let mut menu = build_quit_menu(&shared);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("quit ", HostKey::None, 80);
        let _ = handle.eval_prompt("quit ", HostKey::Enter, 80);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::Quit) => {}
            other => panic!("expected Quit, got {:?}", other),
        }
    }

    #[test]
    fn quit_header_text() {
        let shared = make_shared();
        let mut menu = build_quit_menu(&shared);
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
