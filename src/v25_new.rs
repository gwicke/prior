//! v2.5 `SlashDotNode` implementation for `/new`.
//!
//! Leaf node: submit() creates and activates a fresh session **inline**
//! (stamped from the target config — recorded judgment in the plan).
use std::sync::Arc;

use crate::app_state::AppState;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode, SubmitResult,
};

pub struct NewNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for NewNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Arc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("new").desc("start a new session")],
            active_ids: Vec::new(),
        }))
    }

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx(&self.app_state);
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        vec![
            "Start a new session.".to_string(),
            "".to_string(),
            "The current session is preserved on disk and can be".to_string(),
            "resumed via /session.".to_string(),
        ]
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let name = path_args.suffix.trim().to_string();
        // Data-plane inline: drop + create + activate, now — no control
        // field, no loop round-trip.
        crate::agent::start_fresh_session(&self.app_state, &name).await;
        SubmitResult::Done(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{MenuResult, StaticListingNode};
    use crate::slashdot_menu::HostKey;

    fn test_app_state() -> Arc<AppState> {
        crate::app_state::AppState::for_test(crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_reasoning_level: None,
            show_reasoning: String::new(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            models: Default::default(),
            path: None,
        })
    }

    fn build_new_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(
            NewNode {
                app_state: app_state.clone(),
            }
            .into(),
        );
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn new_shows_in_listing() {
        let app_state = test_app_state();
        let mut menu = build_new_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = crate::agent::block_on_test(handle.eval_prompt("", HostKey::None, 80));
        match result {
            MenuResult::Continue(plan, _) => {
                let text: String = plan
                    .rows
                    .iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("new"), "new should be in listing: {}", text);
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn new_submit_creates_fresh_session() {
        let app_state = test_app_state();
        let (_guard, repo, outgoing, base) = crate::agent::test_repo_with_session();
        app_state.set_repo(Some(repo));
        let mut menu = build_new_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ = crate::agent::block_on_test(handle.eval_prompt("new ", HostKey::None, 80));
        let _ = crate::agent::block_on_test(handle.eval_prompt("new ", HostKey::Enter, 80));
        let s = app_state.session().expect("fresh session activated");
        assert_ne!(s.get_session_id(), outgoing, "must be a different session");
        assert_eq!(app_state.current_session_id(), s.get_session_id());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn new_with_name_names_session() {
        let app_state = test_app_state();
        let (_guard, repo, _outgoing, base) = crate::agent::test_repo_with_session();
        app_state.set_repo(Some(repo));
        let mut menu = build_new_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ =
            crate::agent::block_on_test(handle.eval_prompt("new my-session ", HostKey::None, 80));
        let _ =
            crate::agent::block_on_test(handle.eval_prompt("new my-session ", HostKey::Enter, 80));
        let s = app_state.session().expect("fresh session activated");
        let name =
            crate::agent::block_on_test(async { s.index().lock().await.session_name.clone() });
        assert_eq!(name.as_deref(), Some("my-session"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
