//! v2.5 `SlashDotNode` implementations for `/set`.
//!
//! Each setting is a child of the `/set` listing node. Listing-type
//! settings (model, persona, thinking) use `value_forwarding` on the
//! parent wrapper node — the wrapper's `data()` returns a static row
//! (id + description), and the child's active_ids become the "value"
//! column. Enum settings use the same pattern with `EnumSettingNode`.
//! Leaf settings (int, freetext) return their value directly in `data()`.
use std::sync::Arc;

use std::rc::Rc;

use crate::app_state::AppState;
use crate::settings::{SettingKey, ValueKind, SETTINGS};
use crate::slashdot_menu::fields::Row;
use crate::slashdot_menu::menu::SlashDotMenuNode;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode, StaticRow, SubmitResult,
};

// ---------------------------------------------------------------------------
// EnumSettingNode — a leaf child for enum settings (yolo/confirm, etc.)
// ---------------------------------------------------------------------------

struct EnumSettingNode {
    key: SettingKey,
    values: &'static [(&'static str, &'static str)],
    app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for EnumSettingNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let current = self.app_state.with_settings(|s| s.value(self.key));
        let rows = self
            .values
            .iter()
            .map(|(val, desc)| Row::new(*val).desc(*desc))
            .collect();
        Ok(Rc::new(DataResponse {
            rows,
            active_ids: vec![current],
        }))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let val = path_args.ids.last().cloned().unwrap_or_default();
        match self.app_state.set_setting(self.key, &val) {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

// ---------------------------------------------------------------------------
// SettingLeafNode — for free-text / int settings (tool_call_limit, etc.)
// ---------------------------------------------------------------------------

pub struct SettingLeafNode {
    key: SettingKey,
    app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for SettingLeafNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let meta = SETTINGS.iter().find(|m| m.key == self.key).unwrap();
        let val = self.app_state.with_settings(|s| s.value(self.key));
        let rows = vec![Row::new(meta.name)
            .desc(meta.description)
            .field("value", crate::slashdot_menu::value::Value::Str(val))];
        Ok(Rc::new(DataResponse {
            rows,
            active_ids: Vec::new(),
        }))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let val = path_args.suffix.trim();
        if val.is_empty() {
            // Return NewPath to re-enter the leaf and show header().
            let path = path_args.ids.join(" ");
            return SubmitResult::NewPath(format!("{} ", path));
        }
        match self.app_state.set_setting(self.key, val) {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx();
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        let meta = SETTINGS.iter().find(|m| m.key == self.key).unwrap();
        let val = self.app_state.with_settings(|s| s.value(self.key));
        vec![format!(
            "{} = {}  (type a new value and press Enter)",
            meta.name, val
        )]
    }

    async fn drill_rewrite(&mut self, path_args: &PathArgs) -> Option<String> {
        // Pre-fill the current value. ids are ["set", "<setting-name>"].
        // The leading space keeps the trailing-space behavior of the old
        // `format!("{} {}", prefix, val)` (empty value → trailing space).
        let val = self.app_state.with_settings(|s| s.value(self.key));
        Some(path_args.prompt_with(&format!(" {}", val)))
    }
}

// ---------------------------------------------------------------------------
// Value descriptions — ported from v1.0 setting_value_candidates().
// ---------------------------------------------------------------------------

/// Return `(value, description)` pairs for enum setting values.
fn enum_value_descs(key: SettingKey) -> &'static [(&'static str, &'static str)] {
    match key {
        SettingKey::SecurityMode => &[
            ("confirm", "ask before each tool call"),
            ("yolo", "run tools without prompts"),
        ],
        SettingKey::ShowReasoning => &[
            ("disabled", "show no reasoning text"),
            ("content", "show the reasoning text (default)"),
        ],
        SettingKey::ReasoningInContext => crate::settings::ReasoningInContextMode::variants(),
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// Menu builder — adds /set subtree to the root menu.
// ---------------------------------------------------------------------------

/// Build the `/set` subtree as a `SlashDotMenuNode`.
pub fn build_set_node(app_state: &Arc<AppState>) -> SlashDotMenuNode {
    use crate::slashdot_menu::fields::ColumnDef;
    use crate::v25_listing_node::ListingNode;

    let mut set_node: SlashDotMenuNode = ListingNode::new(
        app_state,
        &[StaticRow::new("set", "view and edit settings")],
    )
    .with_columns(vec![
        ColumnDef::new("id", "Setting").min(22).prio(2),
        ColumnDef::new("value", "Value").min(18).prio(2),
        ColumnDef::new("description", "Description").min(26).prio(1),
    ])
    .into();

    // --- default_model ---
    {
        let reader = app_state.clone();
        let writer = app_state.clone();
        let child = crate::v25_model::ModelNode::new(
            Box::new(move || reader.with_settings(|s| s.default_model.clone())),
            Box::new(move |model| writer.set_setting(SettingKey::DefaultModel, model)),
            None, // no recent models for /set
        );
        let mut wrapper: SlashDotMenuNode = ListingNode::new(
            app_state,
            &[StaticRow::new(
                "default_model",
                "Default model as provider:model (used for new sessions)",
            )],
        )
        .with_columns(crate::v25_model::model_columns())
        .into();
        wrapper = wrapper.with_value_forwarding();
        wrapper.add_child(crate::slashdot_menu::SlashDotMenuNode::new(child));
        set_node.add_child(wrapper);
    }

    // --- default_persona ---
    {
        let reader = app_state.clone();
        let writer = app_state.clone();
        let child = crate::v25_persona::PersonaEnumNode {
            active_fn: Box::new(move || reader.with_settings(|s| s.default_persona.clone())),
            submit_fn: Box::new(move |name| {
                let val = if name == "none" { "" } else { name };
                writer.set_setting(SettingKey::DefaultPersona, val)
            }),
        };
        let mut wrapper: SlashDotMenuNode = ListingNode::new(
            app_state,
            &[StaticRow::new(
                "default_persona",
                "Persona for new sessions (built-in name or file)",
            )],
        )
        .into();
        wrapper = wrapper.with_value_forwarding();
        wrapper.add_child(crate::slashdot_menu::SlashDotMenuNode::new(child));
        set_node.add_child(wrapper);
    }

    // --- default_reasoning_level ---
    {
        let reader = app_state.clone();
        let writer = app_state.clone();
        let child = crate::v25_reasoning::ReasoningEnumNode {
            active_fn: Box::new(move || {
                reader.with_settings(|s| s.value(SettingKey::DefaultReasoning))
            }),
            submit_fn: Box::new(move |level| {
                writer.set_setting(SettingKey::DefaultReasoning, level)
            }),
            // A *default* is not the running model, so there is nothing to
            // report effort mappings for.
            active_model_fn: None,
        };
        let mut wrapper: SlashDotMenuNode = ListingNode::new(
            app_state,
            &[StaticRow::new(
                "default_reasoning_level",
                "Default reasoning level: off | low | med | high | max",
            )],
        )
        .into();
        wrapper = wrapper.with_value_forwarding();
        wrapper.add_child(crate::slashdot_menu::SlashDotMenuNode::new(child));
        set_node.add_child(wrapper);
    }

    // --- Remaining settings (enum, int, freetext) ---
    // Loop once; build wrapper with value_forwarding for enum children,
    // direct leaf for int/freetext.
    for meta in SETTINGS {
        if matches!(
            meta.key,
            SettingKey::DefaultModel | SettingKey::DefaultPersona | SettingKey::DefaultReasoning
        ) {
            continue;
        }
        match meta.kind {
            ValueKind::Enum(_) => {
                let descs = enum_value_descs(meta.key);
                let child = EnumSettingNode {
                    key: meta.key,
                    values: descs,
                    app_state: app_state.clone(),
                };
                let mut wrapper: SlashDotMenuNode =
                    ListingNode::new(app_state, &[StaticRow::new(meta.name, meta.description)])
                        .into();
                wrapper = wrapper.with_value_forwarding();
                wrapper.add_child(crate::slashdot_menu::SlashDotMenuNode::new(child));
                set_node.add_child(wrapper);
            }
            ValueKind::Int | ValueKind::FreeText => {
                let leaf = SettingLeafNode {
                    key: meta.key,
                    app_state: app_state.clone(),
                };
                set_node.add_child(crate::slashdot_menu::SlashDotMenuNode::new(leaf));
            }
            ValueKind::Model | ValueKind::Persona => {
                // Already handled above.
            }
        }
    }

    set_node
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slashdot_menu::HostKey;

    /// Returns an `AppState` whose settings store starts at these values
    /// (binding kept as `settings` in tests below — it *is* the settings view).
    fn test_app_state() -> Arc<AppState> {
        crate::app_state::AppState::for_test(crate::settings::SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 50,
            default_persona: Some("architect".into()),
            default_reasoning_level: None,
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            path: None,
        })
    }

    fn build_test_menu(app_state: &Arc<AppState>) -> crate::slashdot_menu::SlashDotMenu {
        use crate::slashdot_menu::node_impl::StaticListingNode;
        use crate::slashdot_menu::SlashDotMenuNode;
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(build_set_node(app_state));
        crate::slashdot_menu::SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn set_listing_shows_settings() {
        let app_state = test_app_state();
        let mut menu = build_test_menu(&app_state);
        let mut h = menu.get_handle();
        // Root listing shows the "set" row.
        match futures::executor::block_on(h.eval_prompt("", HostKey::None, 80)) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                let texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
                assert!(texts.iter().any(|t| t.contains("set")));
            }
            other => panic!("expected Continue at root, got {:?}", other),
        }
    }

    #[test]
    fn set_drill_shows_settings_table() {
        let app_state = test_app_state();
        let mut menu = build_test_menu(&app_state);
        let mut h = menu.get_handle();
        // Tab into /set → should show settings with value column.
        match futures::executor::block_on(h.eval_prompt("", HostKey::Tab, 80)) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                assert!(
                    prompt.starts_with("set"),
                    "should drill into set, got: {}",
                    prompt
                );
            }
            other => panic!("expected Continue after Tab, got {:?}", other),
        }
        // Now at /set — should show settings rows with values.
        match futures::executor::block_on(h.eval_prompt("set ", HostKey::None, 80)) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                assert!(!plan.rows.is_empty(), "/set should show settings rows");
                // Check that value forwarding populated the value column.
                let first = &plan.rows[0].0;
                assert!(
                    first.contains("default_model") || first.contains("security_mode"),
                    "should contain a setting name, got: {}",
                    first
                );
            }
            other => panic!("expected Continue at /set, got {:?}", other),
        }
    }

    #[test]
    fn set_enum_submit_persists() {
        let app_state = test_app_state();
        let mut menu = build_test_menu(&app_state);
        let mut h = menu.get_handle();
        // Drill into security_mode.
        let result =
            futures::executor::block_on(h.eval_prompt("set security_mode ", HostKey::None, 80));
        if let crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) = result {
            // Should show yolo/confirm rows.
            let texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
            assert!(texts.iter().any(|t| t.contains("yolo")), "should show yolo");
            assert!(
                texts.iter().any(|t| t.contains("confirm")),
                "should show confirm"
            );
        }
        // The cursor follows the active row ("confirm" is current).
        // Tab drills into the highlighted row; Enter on it persists.
        match futures::executor::block_on(h.eval_prompt_resolved(
            "set security_mode ",
            HostKey::Tab,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                assert!(
                    prompt.contains("security_mode"),
                    "should drill into a value, got: {}",
                    prompt
                );
                match futures::executor::block_on(h.eval_prompt(&prompt, HostKey::Enter, 80)) {
                    crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {
                        // The active value was resubmitted (confirm).
                        assert_eq!(
                            app_state.with_settings(|s| s.security_mode.clone()),
                            "confirm"
                        );
                    }
                    other => panic!("expected DoneSuccess, got {:?}", other),
                }
            }
            other => panic!("expected Continue on Tab, got {:?}", other),
        }
    }

    #[test]
    fn set_enum_change_persists() {
        let app_state = test_app_state();
        let mut menu = build_test_menu(&app_state);
        let mut h = menu.get_handle();
        // Move cursor to "yolo" (Down from "confirm" position).
        let _ = futures::executor::block_on(h.eval_prompt("set security_mode ", HostKey::None, 80));
        let _ = futures::executor::block_on(h.eval_prompt(
            "set security_mode ",
            HostKey::MoveCursor(1),
            80,
        ));
        match futures::executor::block_on(h.eval_prompt_resolved(
            "set security_mode ",
            HostKey::Tab,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                assert!(
                    prompt.contains("yolo"),
                    "should drill to yolo, got: {}",
                    prompt
                );
                match futures::executor::block_on(h.eval_prompt(&prompt, HostKey::Enter, 80)) {
                    crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {
                        assert_eq!(app_state.with_settings(|s| s.security_mode.clone()), "yolo");
                    }
                    other => panic!("expected DoneSuccess, got {:?}", other),
                }
            }
            other => panic!("expected Continue on Tab, got {:?}", other),
        }
    }

    #[test]
    fn set_leaf_submit_persists() {
        let app_state = test_app_state();
        let mut menu = build_test_menu(&app_state);
        let mut h = menu.get_handle();
        // Type a value for tool_call_limit directly.
        match futures::executor::block_on(h.eval_prompt(
            "set tool_call_limit 42",
            HostKey::Enter,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {
                assert_eq!(app_state.with_settings(|s| s.tool_call_limit), 42);
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn set_leaf_invalid_shows_error() {
        let app_state = test_app_state();
        let mut menu = build_test_menu(&app_state);
        let mut h = menu.get_handle();
        // Invalid value for tool_call_limit.
        match futures::executor::block_on(h.eval_prompt(
            "set tool_call_limit 0",
            HostKey::Enter,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(msg, _) => {
                assert!(
                    msg.unwrap_or_default().contains("greater than 0"),
                    "should show validation error"
                );
                assert_eq!(app_state.with_settings(|s| s.tool_call_limit), 50);
            }
            other => panic!("expected DoneSuccess with error, got {:?}", other),
        }
    }

    #[test]
    fn setting_leaf_drill_rewrite_prefills_value() {
        use crate::slashdot_menu::node_impl::SlashDotNode;
        let app_state = test_app_state();
        let mut node = SettingLeafNode {
            key: SettingKey::ToolCallLimit,
            app_state: app_state.clone(),
        };
        let path = vec!["set".to_string(), "tool_call_limit".to_string()];
        let result = futures::executor::block_on(node.drill_rewrite(&PathArgs::new(path, " ")));
        assert_eq!(result, Some("set tool_call_limit 50".to_string()));
    }
}
