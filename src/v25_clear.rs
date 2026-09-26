//! v2.5 `SlashDotNode` implementation for `/clear`.
//!
//! Leaf node: submit() does the whole transaction **inline** in the
//! applier's original order — create → activate → delete outgoing —
//! idempotent (a missing/stale outgoing id is a no-op).
use std::sync::Arc;

use std::rc::Rc;

use crate::app_state::AppState;
use crate::session::repo::SessionRepo;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode, SubmitResult,
};

pub struct ClearNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for ClearNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("clear")
                .desc("delete session and start fresh")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        vec![
            "Delete the current session and start fresh.".to_string(),
            "".to_string(),
            "The old session is removed from disk.".to_string(),
        ]
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let name = path_args.suffix.trim().to_string();
        let outgoing = self.app_state.current_session_id();
        // Data-plane inline: create → activate first (so the fresh
        // session is the current one), then delete the outgoing file —
        // same order the applier used; deletion is idempotent.
        crate::agent::start_fresh_session(&self.app_state, &name).await;
        if !outgoing.is_empty() {
            if let Some(repo) = self.app_state.repo() {
                let meta = repo
                    .list(None)
                    .await
                    .ok()
                    .and_then(|metas| metas.into_iter().find(|m| m.id == outgoing));
                if let Some(meta) = meta {
                    // `delete` invalidates the listing cache itself.
                    let _ = repo.delete(&meta).await;
                }
            }
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

    fn test_app_state() -> Arc<AppState> {
        crate::app_state::AppState::for_test(crate::settings::SettingsStore {
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
        })
    }

    fn build_clear_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(
            ClearNode {
                app_state: app_state.clone(),
            }
            .into(),
        );
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn clear_shows_in_listing() {
        let app_state = test_app_state();
        let mut menu = build_clear_menu(&app_state);
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
                assert!(
                    text.contains("clear"),
                    "clear should be in listing: {}",
                    text
                );
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn clear_submit_switches_and_deletes_outgoing() {
        let app_state = test_app_state();
        let (repo, outgoing, base) = crate::agent::test_repo_with_session();
        app_state.set_repo(Some(repo));
        app_state.set_current_session_id(outgoing.clone());
        let mut menu = build_clear_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ = crate::agent::block_on_test(handle.eval_prompt("clear ", HostKey::None, 80));
        let _ = crate::agent::block_on_test(handle.eval_prompt("clear ", HostKey::Enter, 80));
        let s = app_state.session().expect("fresh session activated");
        assert_ne!(
            s.get_session_id(),
            outgoing,
            "fresh session replaces the outgoing one"
        );
        assert_eq!(app_state.current_session_id(), s.get_session_id());
        assert!(
            app_state
                .repo()
                .unwrap()
                .list_sessions_sync(None)
                .is_empty(),
            "the outgoing file must be gone (the fresh one is not written until its first turn)"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn clear_with_name_names_fresh_session() {
        let app_state = test_app_state();
        let base = std::env::temp_dir().join(format!("v25-clear-name-test-{}", std::process::id()));
        let repo = crate::session::repo::JsonlSessionRepo::new(base.clone());
        app_state.set_repo(Some(repo));
        let mut menu = build_clear_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ = crate::agent::block_on_test(handle.eval_prompt(
            "clear my-fresh-start ",
            HostKey::None,
            80,
        ));
        let _ = crate::agent::block_on_test(handle.eval_prompt(
            "clear my-fresh-start ",
            HostKey::Enter,
            80,
        ));
        let s = app_state.session().expect("fresh session activated");
        let name =
            crate::agent::block_on_test(async { s.index().lock().await.session_name.clone() });
        assert_eq!(name.as_deref(), Some("my-fresh-start"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
