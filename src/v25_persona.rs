//! v2.5 `SlashDotNode` implementations for `/persona`.
//!
//! A single `PersonaEnumNode` returns all persona names + "none" from
//! `data()`. Active marker from `LiveState.persona` (transient session
//! state), write on submit. `/set default_persona` will use a separate
//! node that reads/writes `SettingsStore`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
};
use crate::slashdot_menu::menu::SlashDotMenuNode;

// ---------------------------------------------------------------------------
// PersonaEnumNode — returns all persona rows from a single node.
// ---------------------------------------------------------------------------

pub struct PersonaEnumNode {
    /// Read the current persona name (None = no persona).
    pub active_fn: Box<dyn Fn() -> Option<String>>,
    /// Write the selected persona. `Ok(())` on success.
    pub submit_fn: Box<dyn FnMut(&str) -> Result<(), String>>,
}

impl SlashDotNode for PersonaEnumNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let current = (self.active_fn)().unwrap_or_default();
        let mut rows = Vec::new();
        let mut active_ids = Vec::new();
        for p in crate::persona::PERSONAS.iter() {
            rows.push(crate::slashdot_menu::fields::Row::new(p.name).desc(p.description));
            if current == p.name {
                active_ids.push(p.name.to_string());
            }
        }
        // "none" row: active when persona is empty.
        rows.push(crate::slashdot_menu::fields::Row::new("none").desc("clear persona"));
        if current.is_empty() {
            active_ids.push("none".to_string());
        }
        Ok(Rc::new(DataResponse { rows, active_ids }))
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let name = path_args.ids.last().cloned().unwrap_or_default();
        match (self.submit_fn)(&name) {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

// ---------------------------------------------------------------------------
// Menu builder — adds persona child to a listing node.
// ---------------------------------------------------------------------------

/// Add `/persona` child to a listing `SlashDotMenuNode`.
pub fn add_persona_child(listing: &mut SlashDotMenuNode, app_state: &crate::app_state::AppState) {
    let reader = app_state.clone();
    let writer = app_state.clone();
    listing.add_child(PersonaEnumNode {
        active_fn: Box::new(move || reader.persona()),
        submit_fn: Box::new(move |name| {
            let val = if name == "none" { None } else { Some(name.to_string()) };
            writer.set_persona(val);
            Ok(())
        }),
    }.into());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::slashdot_menu::menu::SlashDotMenu;
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
    /// "persona" → `PersonaEnumNode` bound to the target config.
    fn build_persona_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut pl: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("persona", "set runtime persona"),
        ])
        .into();
        add_persona_child(&mut pl, app_state);
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(pl);
        SlashDotMenu::from_root_node(root)
    }

    /// Root listing shows the `persona` command row.
    #[test]
    fn persona_root_row_present() {
        let app_state = test_app_state();
        let mut menu = build_persona_menu(&app_state);
        let mut h = menu.get_handle();
        match h.eval_prompt("", HostKey::None, 80) {
            MenuResult::Continue(plan, _) => {
                let rows: Vec<String> = plan.rows.iter().map(|l| l.0.clone()).collect();
                assert!(
                    rows.iter().any(|r| r.split_whitespace().next() == Some("persona")),
                    "root should list 'persona': {rows:?}"
                );
            }
            other => panic!("expected Continue at root, got {:?}", std::mem::discriminant(&other)),
        }
    }

    /// Tab selects a row (persona name or "none"), Enter commits it to the
    /// target config's persona (`none` → None).
    #[test]
    fn persona_submit_sets_target_config() {
        let app_state = test_app_state();
        // Pre-set a persona that is not a row, so any selection is observable.
        app_state.set_persona(Some("definitely-not-a-persona".into()));
        let mut menu = build_persona_menu(&app_state);
        let mut h = menu.get_handle();
        let prompt = match h.eval_prompt_resolved("persona ", HostKey::Tab, 80) {
            MenuResult::Continue(_, prompt) => prompt,
            other => panic!("expected Continue after Tab, got {:?}", std::mem::discriminant(&other)),
        };
        let sel = prompt
            .split_whitespace()
            .last()
            .expect("Tab should append the selected persona row")
            .to_string();
        match h.eval_prompt(&prompt, HostKey::Enter, 80) {
            MenuResult::DoneSuccess(..) => {}
            other => panic!("expected DoneSuccess on Enter, got {:?}", std::mem::discriminant(&other)),
        }
        let expect = if sel == "none" { None } else { Some(sel) };
        assert_eq!(app_state.persona(), expect);
    }
}
