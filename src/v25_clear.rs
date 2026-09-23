//! v2.5 `SlashDotNode` implementation for `/clear`.
//!
//! Leaf node: submit() sets `AsyncOp::Clear` on `MenuShared` so the agent
//! loop deletes the current session and starts fresh.

use std::rc::Rc;

use crate::slash_menu_bridge::{AsyncOp, MenuShared};
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult,
};

pub struct ClearNode {
    pub shared: Rc<MenuShared>,
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
        *self.shared.pending_op.borrow_mut() = Some(AsyncOp::Clear { name });
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

    fn build_clear_menu(shared: &Rc<MenuShared>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(ClearNode { shared: shared.clone() }.into());
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn clear_shows_in_listing() {
        let shared = make_shared();
        let mut menu = build_clear_menu(&shared);
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
    fn clear_submit_sets_pending_op() {
        let shared = make_shared();
        let mut menu = build_clear_menu(&shared);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("clear ", HostKey::None, 80);
        let _ = handle.eval_prompt("clear ", HostKey::Enter, 80);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::Clear { name }) => assert_eq!(name, ""),
            other => panic!("expected Clear, got {:?}", other),
        }
    }

    #[test]
    fn clear_with_name_sets_pending_op() {
        let shared = make_shared();
        let mut menu = build_clear_menu(&shared);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("clear my-fresh-start ", HostKey::None, 80);
        let _ = handle.eval_prompt("clear my-fresh-start ", HostKey::Enter, 80);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::Clear { name }) => assert_eq!(name, "my-fresh-start"),
            other => panic!("expected Clear with name, got {:?}", other),
        }
    }
}
