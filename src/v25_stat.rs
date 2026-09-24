//! v2.5 `SlashDotNode` implementation for `/stat`.
//!
//! Leaf node: header() shows session stats; submit() closes the menu.

use std::rc::Rc;

use crate::slash_menu_bridge::MenuShared;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult,
};

pub struct StatNode {
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for StatNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![crate::slashdot_menu::fields::Row::new("stat")
                .desc("session stats: context window, usage, cost, cache hit")],
            active_ids: Vec::new(),
        }))
    }

    fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        let shared = &self.shared;
        let ls = shared.live_state.borrow();
        // Parse provider:model from LiveState.
        let (provider, model) = if let Some((p, m)) = ls.model.split_once(':') {
            (p.to_string(), m.to_string())
        } else {
            (String::new(), ls.model.clone())
        };
        drop(ls);

        let usage = shared.session_usage.borrow().clone();

        let session_info = {
            let session_id = shared.current_session_id.borrow();
            if session_id.is_empty() {
                None
            } else if let Some(ref repo) = shared.repo {
                let all = repo.list_sessions_sync(None);
                all.into_iter().find(|m| m.id == *session_id).map(|meta| {
                    let display_name = meta.name.unwrap_or_else(|| meta.id.clone());
                    crate::agent::SessionStatsInfo {
                        display_name,
                        created: meta.timestamp,
                        last_updated: meta.last_prompt_timestamp,
                        message_count: meta.message_count,
                        user_message_count: meta.user_message_count,
                    }
                })
            } else {
                None
            }
        };

        crate::agent::session_stats_lines(
            &provider,
            &model,
            usage,
            session_info,
            *shared.context_tokens.borrow(),
        )
    }

    fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        // Navigate to empty root path — closes the menu.
        SubmitResult::NewPath(String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash_menu_bridge::MenuShared;
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

    fn build_stat_menu(shared: &Rc<MenuShared>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(StatNode { shared: shared.clone() }.into());
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn stat_shows_model_in_header() {
        let shared = make_shared();
        shared.live_state.borrow_mut().model = "openai:gpt-4o".to_string();
        let mut menu = build_stat_menu(&shared);
        let mut handle = menu.get_handle();
        // Drill into stat leaf (trailing space).
        let result = handle.eval_prompt("stat ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let header_text: String = plan.detail.iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(header_text.contains("gpt-4o"),
                    "header should mention model: {}", header_text);
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn stat_empty_session_shows_unavailable() {
        let shared = make_shared();
        shared.live_state.borrow_mut().model = "test:model".to_string();
        let mut menu = build_stat_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("stat ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let header_text: String = plan.detail.iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(header_text.contains("(unavailable)"),
                    "header should show unavailable: {}", header_text);
            }
            other => panic!("expected Continue, got {:?}",
                std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn stat_header_shows_estimated_context_tokens() {
        let shared = make_shared();
        shared.live_state.borrow_mut().model = "openai:gpt-4o".to_string();
        *shared.context_tokens.borrow_mut() = 42_000;
        let mut menu = build_stat_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("stat ", HostKey::None, 80);
        match result {
            MenuResult::Continue(plan, _) => {
                let header_text: String = plan.detail.iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(header_text.contains("estimated context tokens: 42000"),
                    "header should show estimated context tokens: {}", header_text);
            }
            other => panic!("expected Continue, got {:?}",
                std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn stat_enter_clears() {
        let shared = make_shared();
        shared.live_state.borrow_mut().model = "test:model".to_string();
        let mut menu = build_stat_menu(&shared);
        let mut handle = menu.get_handle();
        // Drill into leaf first.
        let _ = handle.eval_prompt("stat ", HostKey::None, 80);
        // Enter → submit → NewPath("") → closes.
        let r = handle.eval_prompt("stat ", HostKey::Enter, 80);
        assert!(matches!(r, MenuResult::Continue(_, _)));
    }

    #[test]
    fn stat_esc_goes_back() {
        let shared = make_shared();
        shared.live_state.borrow_mut().model = "test:model".to_string();
        let mut menu = build_stat_menu(&shared);
        let mut handle = menu.get_handle();
        let _ = handle.eval_prompt("stat ", HostKey::None, 80);
        let result = handle.eval_prompt("stat ", HostKey::Esc, 80);
        assert!(matches!(result, MenuResult::Continue(_, _)));
    }
}
