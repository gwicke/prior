//! v2.5 `SlashDotNode` implementation for `/new`.
//!
//! Leaf node: submit() sets `AsyncOp::New` on `MenuShared` so the agent
//! loop starts a fresh session.

use std::rc::Rc;

use crate::slash_menu_bridge::{AsyncOp, MenuShared};
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
};

pub struct NewNode {
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for NewNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("new")
                .desc("start a new session")],
            active_ids: Vec::new(),
        }))
    }

    fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
        vec![
            "Start a new session.".to_string(),
            "".to_string(),
            "The current session is preserved on disk and can be".to_string(),
            "resumed via /session.".to_string(),
        ]
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let name = path_args.suffix.trim().to_string();
        *self.shared.pending_op.borrow_mut() = Some(AsyncOp::New { name });
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

    fn build_new_menu(shared: &Rc<MenuShared>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(NewNode { shared: shared.clone() }.into());
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn new_shows_in_listing() {
        let shared = make_shared();
        let mut menu = build_new_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let text: String = plan.rows.iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("new"), "new should be in listing: {}", text);
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn new_submit_sets_pending_op() {
        let shared = make_shared();
        let mut menu = build_new_menu(&shared);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("new ", HostKey::None, 80);
        let _ = handle.eval_prompt("new ", HostKey::Enter, 80);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::New { name }) => assert_eq!(name, ""),
            other => panic!("expected New, got {:?}", other),
        }
    }

    #[test]
    fn new_with_name_sets_pending_op() {
        let shared = make_shared();
        let mut menu = build_new_menu(&shared);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("new my-session ", HostKey::None, 80);
        let _ = handle.eval_prompt("new my-session ", HostKey::Enter, 80);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::New { name }) => assert_eq!(name, "my-session"),
            other => panic!("expected New with name, got {:?}", other),
        }
    }
}
