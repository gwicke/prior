//! v2.5 `SlashDotNode` implementations for `/set`.
//!
//! Each setting is a child of the `/set` listing node. Listing-type
//! settings (model, persona, thinking) use `value_forwarding` on the
//! parent wrapper node — the wrapper's `data()` returns a static row
//! (id + description), and the child's active_ids become the "value"
//! column. Enum settings use the same pattern with `EnumSettingNode`.
//! Leaf settings (int, freetext) return their value directly in `data()`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::settings::{SettingKey, SettingsStore, SETTINGS, ValueKind};
use crate::slashdot_menu::fields::Row;
use crate::slashdot_menu::menu::SlashDotMenuNode;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
    StaticListingNode, StaticRow,
};

// ---------------------------------------------------------------------------
// EnumSettingNode — a leaf child for enum settings (yolo/confirm, etc.)
// ---------------------------------------------------------------------------

struct EnumSettingNode {
    key: SettingKey,
    values: &'static [(&'static str, &'static str)],
    settings: Rc<RefCell<SettingsStore>>,
}

impl SlashDotNode for EnumSettingNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let s = self.settings.borrow();
        let current = s.value(self.key);
        let rows = self.values.iter()
            .map(|(val, desc)| Row::new(*val).desc(*desc))
            .collect();
        Ok(Rc::new(DataResponse {
            rows,
            active_ids: vec![current],
        }))
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let val = path_args.ids.last().cloned().unwrap_or_default();
        match self.settings.borrow_mut().set_value(self.key, &val) {
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
    settings: Rc<RefCell<SettingsStore>>,
}

impl SlashDotNode for SettingLeafNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let meta = SETTINGS.iter().find(|m| m.key == self.key).unwrap();
        let val = self.settings.borrow().value(self.key);
        let rows = vec![Row::new(meta.name)
            .desc(meta.description)
            .field("value", crate::slashdot_menu::value::Value::Str(val))];
        Ok(Rc::new(DataResponse {
            rows,
            active_ids: Vec::new(),
        }))
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let val = path_args.suffix.trim();
        if val.is_empty() {
            // Return NewPath to re-enter the leaf and show header().
            let path = path_args.ids.join(" ");
            return SubmitResult::NewPath(format!("{} ", path));
        }
        match self.settings.borrow_mut().set_value(self.key, val) {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }

    fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
        let meta = SETTINGS.iter().find(|m| m.key == self.key).unwrap();
        let val = self.settings.borrow().value(self.key);
        vec![format!("{} = {}  (type a new value and press Enter)", meta.name, val)]
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
            ("disabled", "show neither indicator nor reasoning text"),
            ("indicator", "show only a Thinking… spinner"),
            ("content", "show the reasoning text (default)"),
        ],
        SettingKey::ReasoningInContext => &[
            ("true", "keep reasoning blocks in the session context"),
            ("false", "drop reasoning from context to save tokens (default)"),
        ],
        _ => &[],
    }
}

// ---------------------------------------------------------------------------
// Menu builder — adds /set subtree to the root menu.
// ---------------------------------------------------------------------------

/// Build the `/set` subtree as a `SlashDotMenuNode`.
pub fn build_set_node(
    settings: &Rc<RefCell<SettingsStore>>,
) -> SlashDotMenuNode {
    use crate::slashdot_menu::provider::ColumnDef;

    let mut set_node: SlashDotMenuNode = StaticListingNode::new(&[
        StaticRow::new("set", "view and edit settings"),
    ])
    .with_columns(vec![
        ColumnDef::new("id", "Setting").min(22).prio(2),
        ColumnDef::new("value", "Value").min(18).prio(2),
        ColumnDef::new("description", "Description").min(26).prio(1),
    ])
    .into();

    // --- default_model ---
    {
        let settings_clone = settings.clone();
        let settings_clone2 = settings.clone();
        let child = crate::v25_model::ModelNode::new(
            Box::new(move || settings_clone.borrow().default_model.clone()),
            Box::new(move |model| {
                settings_clone2.borrow_mut().set_value(SettingKey::DefaultModel, model)
            }),
            None, // no recent models for /set
        );
        let mut wrapper: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("default_model", "Default model as provider:model (used for new sessions)"),
        ]).with_columns(crate::v25_model::model_columns()).into();
        wrapper = wrapper.with_value_forwarding();
        wrapper.add_child(crate::slashdot_menu::SlashDotMenuNode::new(child));
        set_node.add_child(wrapper);
    }

    // --- default_persona ---
    {
        let settings_clone = settings.clone();
        let settings_clone2 = settings.clone();
        let child = crate::v25_persona::PersonaEnumNode {
            active_fn: Box::new(move || settings_clone.borrow().default_persona.clone()),
            submit_fn: Box::new(move |name| {
                let val = if name == "none" { "" } else { name };
                settings_clone2.borrow_mut().set_value(SettingKey::DefaultPersona, val)
            }),
        };
        let mut wrapper: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("default_persona", "Persona for new sessions (built-in name or file)"),
        ]).into();
        wrapper = wrapper.with_value_forwarding();
        wrapper.add_child(crate::slashdot_menu::SlashDotMenuNode::new(child));
        set_node.add_child(wrapper);
    }

    // --- default_thinking_level ---
    {
        let settings_clone = settings.clone();
        let settings_clone2 = settings.clone();
        let child = crate::v25_thinking::ThinkingEnumNode {
            active_fn: Box::new(move || {
                settings_clone.borrow().default_thinking_level.clone()
            }),
            submit_fn: Box::new(move |level| {
                settings_clone2.borrow_mut().set_value(SettingKey::DefaultThinking, level)
            }),
        };
        let mut wrapper: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("default_thinking_level", "Default thinking level: off | low | med | high"),
        ]).into();
        wrapper = wrapper.with_value_forwarding();
        wrapper.add_child(crate::slashdot_menu::SlashDotMenuNode::new(child));
        set_node.add_child(wrapper);
    }

    // --- Remaining settings (enum, int, freetext) ---
    // Loop once; build wrapper with value_forwarding for enum children,
    // direct leaf for int/freetext.
    for meta in SETTINGS {
        if matches!(meta.key,
            SettingKey::DefaultModel
            | SettingKey::DefaultPersona
            | SettingKey::DefaultThinking
        ) {
            continue;
        }
        match meta.kind {
            ValueKind::Enum(_) => {
                let descs = enum_value_descs(meta.key);
                let settings_clone = settings.clone();
                let child = EnumSettingNode {
                    key: meta.key,
                    values: descs,
                    settings: settings_clone,
                };
                let mut wrapper: SlashDotMenuNode = StaticListingNode::new(&[
                    StaticRow::new(meta.name, meta.description),
                ]).into();
                wrapper = wrapper.with_value_forwarding();
                wrapper.add_child(crate::slashdot_menu::SlashDotMenuNode::new(child));
                set_node.add_child(wrapper);
            }
            ValueKind::Int | ValueKind::FreeText => {
                let settings_clone = settings.clone();
                let leaf = SettingLeafNode {
                    key: meta.key,
                    settings: settings_clone,
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
    use crate::slashdot_menu::node_impl::HostKey;

    fn test_settings() -> Rc<RefCell<SettingsStore>> {
        Rc::new(RefCell::new(SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 50,
            default_persona: Some("architect".into()),
            default_thinking_level: "off".into(),
            show_reasoning: "content".into(),
            reasoning_in_context: false,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            path: None,
        }))
    }

    fn build_test_menu(settings: &Rc<RefCell<SettingsStore>>) -> crate::slashdot_menu::SlashDotMenu {
        use crate::slashdot_menu::SlashDotMenuNode;
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(build_set_node(settings));
        crate::slashdot_menu::SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn set_listing_shows_settings() {
        let settings = test_settings();
        let mut menu = build_test_menu(&settings);
        let mut h = menu.get_handle();
        // Root listing shows the "set" row.
        match h.eval_prompt("", HostKey::None, 80) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                let texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
                assert!(texts.iter().any(|t| t.contains("set")));
            }
            other => panic!("expected Continue at root, got {:?}", other),
        }
    }

    #[test]
    fn set_drill_shows_settings_table() {
        let settings = test_settings();
        let mut menu = build_test_menu(&settings);
        let mut h = menu.get_handle();
        // Tab into /set → should show settings with value column.
        match h.eval_prompt("", HostKey::Tab, 80) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                assert!(prompt.starts_with("set"), "should drill into set, got: {}", prompt);
            }
            other => panic!("expected Continue after Tab, got {:?}", other),
        }
        // Now at /set — should show settings rows with values.
        match h.eval_prompt("set ", HostKey::None, 80) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                assert!(!plan.rows.is_empty(), "/set should show settings rows");
                // Check that value forwarding populated the value column.
                let first = &plan.rows[0].0;
                assert!(first.contains("default_model") || first.contains("security_mode"),
                    "should contain a setting name, got: {}", first);
            }
            other => panic!("expected Continue at /set, got {:?}", other),
        }
    }

    #[test]
    fn set_enum_submit_persists() {
        let settings = test_settings();
        let mut menu = build_test_menu(&settings);
        let mut h = menu.get_handle();
        // Drill into security_mode.
        let result = h.eval_prompt("set security_mode ", HostKey::None, 80);
        if let crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) = result {
            // Should show yolo/confirm rows.
            let texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
            assert!(texts.iter().any(|t| t.contains("yolo")), "should show yolo");
            assert!(texts.iter().any(|t| t.contains("confirm")), "should show confirm");
        }
        // The cursor follows the active row ("confirm" is current).
        // Tab drills into the highlighted row; Enter on it persists.
        match h.eval_prompt("set security_mode ", HostKey::Tab, 80) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                assert!(prompt.contains("security_mode"),
                    "should drill into a value, got: {}", prompt);
                match h.eval_prompt(&prompt, HostKey::Enter, 80) {
                    crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(_) => {
                        // The active value was resubmitted (confirm).
                        assert_eq!(settings.borrow().security_mode, "confirm");
                    }
                    other => panic!("expected DoneSuccess, got {:?}", other),
                }
            }
            other => panic!("expected Continue on Tab, got {:?}", other),
        }
    }

    #[test]
    fn set_enum_change_persists() {
        let settings = test_settings();
        let mut menu = build_test_menu(&settings);
        let mut h = menu.get_handle();
        // Move cursor to "yolo" (Down from "confirm" position).
        let _ = h.eval_prompt("set security_mode ", HostKey::None, 80);
        let _ = h.eval_prompt("set security_mode ", HostKey::MoveCursor(1), 80);
        match h.eval_prompt("set security_mode ", HostKey::Tab, 80) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                assert!(prompt.contains("yolo"), "should drill to yolo, got: {}", prompt);
                match h.eval_prompt(&prompt, HostKey::Enter, 80) {
                    crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(_) => {
                        assert_eq!(settings.borrow().security_mode, "yolo");
                    }
                    other => panic!("expected DoneSuccess, got {:?}", other),
                }
            }
            other => panic!("expected Continue on Tab, got {:?}", other),
        }
    }

    #[test]
    fn set_leaf_submit_persists() {
        let settings = test_settings();
        let mut menu = build_test_menu(&settings);
        let mut h = menu.get_handle();
        // Type a value for tool_call_limit directly.
        match h.eval_prompt("set tool_call_limit 42", HostKey::Enter, 80) {
            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(_) => {
                assert_eq!(settings.borrow().tool_call_limit, 42);
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn set_leaf_invalid_shows_error() {
        let settings = test_settings();
        let mut menu = build_test_menu(&settings);
        let mut h = menu.get_handle();
        // Invalid value for tool_call_limit.
        match h.eval_prompt("set tool_call_limit 0", HostKey::Enter, 80) {
            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(msg) => {
                assert!(msg.unwrap_or_default().contains("greater than 0"),
                    "should show validation error");
                assert_eq!(settings.borrow().tool_call_limit, 50);
            }
            other => panic!("expected DoneSuccess with error, got {:?}", other),
        }
    }
}
