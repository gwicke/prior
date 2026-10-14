//! v2.5 `SlashDotNode` implementation for `/stat`.
//!
//! Leaf node: header() shows session stats; submit() closes the menu.

use std::rc::Rc;
use std::sync::Arc;

use crate::app_state::AppState;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode, SubmitResult,
};

pub struct StatNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for StatNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("stat")
                .desc("session stats: context window, usage, cost, cache hit")],
            active_ids: Vec::new(),
        }))
    }

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx(&self.app_state);
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        // Read achieved state directly — direct accessor calls (no timing
        // constraint; these fields are written by the loop between evals).
        let model = self.app_state.model();
        let (provider, model) = if let Some((p, m)) = model.split_once(':') {
            (p.to_string(), m.to_string())
        } else {
            (String::new(), model)
        };

        let usage = self.app_state.session_usage();

        let session_info = {
            let session_id = self.app_state.current_session_id();
            if session_id.is_empty() {
                None
            } else if let Some(repo) = self.app_state.repo() {
                let all = repo.list_sessions_overlay(None).await;
                all.into_iter().find(|m| m.id == session_id).map(|meta| {
                    let detail = meta.detail.as_ref();
                    crate::agent::status::SessionStatsInfo {
                        id: meta.id,
                        name: meta.name.filter(|n| !n.is_empty()),
                        cwd: Some(meta.cwd).filter(|c| !c.is_empty()),
                        created: meta.timestamp,
                        last_updated: detail.and_then(|d| d.last_prompt_timestamp.clone()),
                        message_count: detail.map(|d| d.message_count).unwrap_or(0),
                        user_message_count: detail.map(|d| d.user_message_count).unwrap_or(0),
                    }
                })
            } else {
                None
            }
        };

        crate::agent::status::session_stats_lines(
            &provider,
            &model,
            usage,
            session_info,
            self.app_state.context_tokens(),
            self.app_state
                .with_settings(|s| s.model_settings(&format!("{provider}:{model}")).cloned())
                .as_ref(),
            &self.app_state.params(),
        )
    }

    async fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        // Navigate to empty root path — closes the menu.
        SubmitResult::NewPath(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{MenuResult, StaticListingNode};
    use crate::slashdot_menu::HostKey;
    use std::sync::Arc;

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

    fn build_stat_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(
            StatNode {
                app_state: app_state.clone(),
            }
            .into(),
        );
        SlashDotMenu::from_root_node(root)
    }

    /// A repo holding one session under a synthetic cwd. `name` goes on the
    /// *header*, which is where `set_name` puts it — not in a trailing
    /// `session_info` entry, which the listing read only consults for
    /// pre-migration files whose header lacks the field. An empty `name` is
    /// the "never named" case `SessionHeader::new` itself writes.
    ///
    /// Returns the state, the session uuid, and the session's cwd.
    ///
    /// The repo lives in a [`tempfile::TempDir`]: unique per call (so
    /// concurrent runs sharing a `TMPDIR` cannot collide) and removed on drop,
    /// including on unwind. A hand-named `temp_dir()/v25-stat-test-<nanos>`
    /// directory is only ever removed on the happy path, so a failing run
    /// leaves one behind.
    fn temp_repo_with_session(name: &str) -> (tempfile::TempDir, Arc<AppState>, String, String) {
        use crate::session::types::{encode_cwd, session_filename, SessionHeader};

        let base_guard = tempfile::tempdir().unwrap();
        let base = base_guard.path().to_path_buf();
        // A synthetic cwd nothing creates; only `encode_cwd` and the session
        // header ever see it.
        let cwd = base.join("synthetic-cwd");
        let dir = base.join(encode_cwd(&cwd));
        std::fs::create_dir_all(&dir).unwrap();

        let mut header = SessionHeader::new(&cwd);
        header.name = Some(name.to_string());
        let uuid = header.id.clone();
        let path = dir.join(session_filename(&chrono::Utc::now()));
        let user_entry = serde_json::json!({
            "id": "e1",
            "timestamp": "2025-01-01T00:00:00.000Z",
            "type": "message",
            "message": {"role": "user", "content": [{"type": "text", "text": "hello world"}]},
        });
        let leaf = serde_json::json!({
            "id": "leaf1",
            "timestamp": "2025-01-01T00:00:01.000Z",
            "type": "leaf",
            "targetId": "e1",
        });
        let contents = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&user_entry).unwrap(),
            serde_json::to_string(&leaf).unwrap(),
        );
        std::fs::write(&path, contents).unwrap();

        let repo = crate::session::repo::JsonlSessionRepo::new(base.clone());
        let app_state = test_app_state();
        app_state.set_repo(Some(repo));
        (base_guard, app_state, uuid, header.cwd)
    }

    fn stat_header_text(app_state: &Arc<AppState>) -> String {
        let mut menu = build_stat_menu(app_state);
        let mut handle = menu.get_handle();
        match crate::agent::block_on_test(handle.eval_prompt("stat ", HostKey::None, 80)) {
            MenuResult::Continue(plan, _) => plan
                .detail
                .iter()
                .map(|l| l.0.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// The name and cwd the repo holds must reach the overlay: both are read
    /// out of session metadata at the call site, and a pure-function test on
    /// `session_stats_lines` would pass even if the call site dropped them.
    #[test]
    fn stat_header_shows_the_session_name_and_cwd() {
        let (_repo, app_state, uuid, cwd) = temp_repo_with_session("deploy work");
        app_state.set_current_session_id(uuid.clone());
        let header_text = stat_header_text(&app_state);

        assert!(
            header_text.contains(&format!("session: {}", uuid)),
            "the id should head the section: {header_text}"
        );
        assert!(
            header_text.contains("  name: deploy work"),
            "the user-assigned name should be listed: {header_text}"
        );
        // The cwd is quoted whole, not relativized: the overlay lists
        // sessions from every cwd, so a short form would be ambiguous.
        assert!(
            header_text.contains(&format!("  cwd: {}", cwd)),
            "the session cwd should be listed: {header_text}"
        );
    }

    /// A session the user never named shows the id and no `name:` line.
    /// `SessionHeader::new` seeds `name: Some("")` and the listing read
    /// turns that into `None`; a bare `name:` would read as a name that
    /// exists.
    #[test]
    fn stat_header_omits_an_unset_name() {
        let (_repo, app_state, uuid, _cwd) = temp_repo_with_session("");
        app_state.set_current_session_id(uuid.clone());
        let header_text = stat_header_text(&app_state);

        assert!(
            !header_text.contains("name:"),
            "an unnamed session must not grow a name line: {header_text}"
        );
        // The id is still there: dropping the name must not cost the reader
        // the one identifier every session has.
        assert!(
            header_text.contains(&format!("session: {}", uuid)),
            "the id should still head the section: {header_text}"
        );
    }

    #[test]
    fn stat_shows_model_in_header() {
        let app_state = test_app_state();
        app_state.set_model("openai:gpt-4o");
        let mut menu = build_stat_menu(&app_state);
        let mut handle = menu.get_handle();
        // Drill into stat leaf (trailing space).
        let result = futures::executor::block_on(handle.eval_prompt("stat ", HostKey::None, 80));
        match result {
            MenuResult::Continue(plan, _) => {
                let header_text: String = plan
                    .detail
                    .iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    header_text.contains("gpt-4o"),
                    "header should mention model: {}",
                    header_text
                );
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn stat_empty_session_shows_unavailable() {
        let app_state = test_app_state();
        app_state.set_model("test:model");
        let mut menu = build_stat_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("stat ", HostKey::None, 80));
        match result {
            MenuResult::Continue(plan, _) => {
                let header_text: String = plan
                    .detail
                    .iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    header_text.contains("(unavailable)"),
                    "header should show unavailable: {}",
                    header_text
                );
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn stat_header_shows_estimated_context_tokens() {
        let app_state = test_app_state();
        app_state.set_model("openai:gpt-4o");
        app_state.update_cycle_view(
            String::new(),
            Default::default(),
            42_000,
            Arc::new(Vec::new()),
            None,
        );
        let mut menu = build_stat_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = futures::executor::block_on(handle.eval_prompt("stat ", HostKey::None, 80));
        match result {
            MenuResult::Continue(plan, _) => {
                let header_text: String = plan
                    .detail
                    .iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    header_text.contains("estimated context tokens: 42000"),
                    "header should show estimated context tokens: {}",
                    header_text
                );
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn stat_enter_clears() {
        let app_state = test_app_state();
        app_state.set_model("test:model");
        let mut menu = build_stat_menu(&app_state);
        let mut handle = menu.get_handle();
        // Drill into leaf first.
        let _ = futures::executor::block_on(handle.eval_prompt("stat ", HostKey::None, 80));
        // Enter → submit → NewPath("") → host re-derives from the
        // rewritten prompt (resolved here like repl::v25_eval does).
        let r =
            futures::executor::block_on(handle.eval_prompt_resolved("stat ", HostKey::Enter, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)));
    }

    #[test]
    fn stat_esc_goes_back() {
        let app_state = test_app_state();
        app_state.set_model("test:model");
        let mut menu = build_stat_menu(&app_state);
        let mut handle = menu.get_handle();
        let _ = futures::executor::block_on(handle.eval_prompt("stat ", HostKey::None, 80));
        let result =
            futures::executor::block_on(handle.eval_prompt_resolved("stat ", HostKey::Esc, 80));
        assert!(matches!(result, MenuResult::Continue(_, _)));
    }
}
