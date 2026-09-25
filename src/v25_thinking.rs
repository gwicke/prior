//! v2.5 `SlashDotNode` implementations for picocode commands.
//!
//! Each command is one `impl SlashDotNode`. The engine handles traversal,
//! cursor, and child management internally.

use std::rc::Rc;

use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
};

// ---------------------------------------------------------------------------
// ThinkingEnumNode — returns all thinking levels from a single node.
//
// The parent listing shows off/low/med/high as rows. On submit, the
// selected level is read from path_args (the drilled row ID) and written
// to the shared cell.
// ---------------------------------------------------------------------------

pub struct ThinkingEnumNode {
    /// Read the current thinking level.
    pub active_fn: Box<dyn Fn() -> String>,
    /// Write the selected thinking level. `Ok(())` on success.
    pub submit_fn: Box<dyn FnMut(&str) -> Result<(), String>>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for ThinkingEnumNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let active = (self.active_fn)();
        let levels: &[(&str, &str)] = &[
            ("off", "no extra reasoning"),
            ("low", "minimal reasoning"),
            ("med", "balanced reasoning"),
            ("high", "maximum reasoning"),
        ];
        let rows = levels.iter()
            .map(|(l, d)| crate::slashdot_menu::fields::Row::new(*l).desc(*d))
            .collect();
        Ok(Rc::new(DataResponse {
            rows,
            active_ids: vec![active],
        }))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let level = path_args.ids.last().cloned().unwrap_or_default();
        match (self.submit_fn)(&level) {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::app_state::AppState;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{MenuResult, StaticListingNode, StaticRow};
    use crate::slashdot_menu::HostKey;

    fn test_app_state() -> Arc<AppState> {
        let settings = crate::settings::SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_thinking_level: "off".into(),
            show_reasoning: "indicator".into(),
            reasoning_in_context: false,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            path: None,
        };
        crate::app_state::AppState::for_test(settings)
    }

    /// Mini menu mirroring the `v25_menu` wiring: root → listing row
    /// "thinking" → `ThinkingEnumNode` bound to the target config.
    fn build_thinking_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let reader = app_state.clone();
        let writer = app_state.clone();
        let mut tl: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("thinking", "Set thinking level: off | low | med | high"),
        ])
        .into();
        tl.add_child(
            ThinkingEnumNode {
                active_fn: Box::new(move || {
                    reader
                        .thinking()
                        .unwrap_or_else(|| "off".to_string())
                }),
                submit_fn: Box::new(move |level| {
                    writer.set_thinking(Some(level.to_string()));
                    Ok(())
                }),
            }
            .into(),
        );
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(tl);
        SlashDotMenu::from_root_node(root)
    }

    fn row_texts(plan: &crate::slashdot_menu::render::RenderPlan) -> Vec<String> {
        plan.rows.iter().map(|l| l.0.clone()).collect()
    }

    /// Root listing shows the `thinking` command row.
    #[test]
    fn thinking_root_row_present() {
        let app_state = test_app_state();
        let mut menu = build_thinking_menu(&app_state);
        let mut h = menu.get_handle();
        match h.eval_prompt("", HostKey::None, 80) {
            MenuResult::Continue(plan, _) => {
                let rows = row_texts(&plan);
                assert!(
                    rows.iter().any(|r| r.split_whitespace().next() == Some("thinking")),
                    "root should list 'thinking': {rows:?}"
                );
            }
            other => panic!("expected Continue at root, got {:?}", std::mem::discriminant(&other)),
        }
    }

    /// The current level gets the `*` marker: `plan.active_rows` points at
    /// the row whose id is the active level.
    #[test]
    fn thinking_active_marker_on_current_level() {
        let app_state = test_app_state();
        app_state.set_thinking(Some("med".to_string()));
        let mut menu = build_thinking_menu(&app_state);
        let mut h = menu.get_handle();
        match h.eval_prompt("thinking ", HostKey::None, 80) {
            MenuResult::Continue(plan, _) => {
                let rows = row_texts(&plan);
                let idx = rows
                    .iter()
                    .position(|r| r.split_whitespace().next() == Some("med"))
                    .expect("'med' row must be listed");
                assert_eq!(
                    plan.active_rows,
                    vec![idx],
                    "active marker must point at 'med': rows={rows:?}"
                );
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    /// Tab selects a level, Enter submits it → the target config's thinking.
    #[test]
    fn thinking_submit_sets_target_config() {
        let app_state = test_app_state();
        let mut menu = build_thinking_menu(&app_state);
        let mut h = menu.get_handle();
        let prompt = match h.eval_prompt_resolved("thinking ", HostKey::Tab, 80) {
            MenuResult::Continue(_, prompt) => prompt,
            other => panic!("expected Continue after Tab, got {:?}", std::mem::discriminant(&other)),
        };
        let sel = prompt
            .split_whitespace()
            .last()
            .expect("Tab should append the selected level")
            .to_string();
        match h.eval_prompt(&prompt, HostKey::Enter, 80) {
            MenuResult::DoneSuccess(..) => {}
            other => panic!("expected DoneSuccess on Enter, got {:?}", std::mem::discriminant(&other)),
        }
        assert_eq!(app_state.thinking().as_deref(), Some(sel.as_str()));
    }
}
