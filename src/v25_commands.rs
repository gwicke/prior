//! v2.5 command node implementations for testing the slashdot_menu engine.
//!
//! These test nodes exercise the engine's traversal, cursor, drill,
//! and submit paths without depending on the full picocode host.

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::slashdot_menu::node_impl::{
        DataResponse, DataResult, HostKey, MenuResult, PathArgs,
        SlashDotNode, SubmitResult, StaticListingNode, StaticRow,
    };
    use crate::slashdot_menu::menu::SlashDotMenu;

    // -----------------------------------------------------------------------
    // ThinkingEnumNode — single node returning all four levels.
    // -----------------------------------------------------------------------

    struct ThinkingEnumNode {
        shared: Rc<RefCell<Option<String>>>,
    }

    impl SlashDotNode for ThinkingEnumNode {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            let current = self.shared.borrow();
            let active = current.as_deref().unwrap_or("off").to_string();
            let rows = ["off", "low", "med", "high"].iter()
                .map(|l| crate::slashdot_menu::fields::Row::new(*l).desc(""))
                .collect();
            Ok(Rc::new(DataResponse {
                rows,
                active_ids: vec![active],
            }))
        }

        fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            let level = path_args.ids.last().cloned().unwrap_or_default();
            *self.shared.borrow_mut() = Some(level);
            SubmitResult::Done(None)
        }
    }

    // -----------------------------------------------------------------------
    // Menu construction
    // -----------------------------------------------------------------------

    fn build_thinking_menu(state: &Rc<RefCell<Option<String>>>) -> SlashDotMenu {
        let mut menu = SlashDotMenu::new();
        menu.add_root_child(StaticListingNode::new(&[
            StaticRow::new("thinking", "set thinking level"),
        ]))
        .add_child_node(ThinkingEnumNode { shared: state.clone() });
        menu
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn thinking_lists_values() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("thinking ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert_eq!(row_texts.len(), 4);
                assert!(row_texts.iter().any(|t| t.contains("off")));
                assert!(row_texts.iter().any(|t| t.contains("low")));
                assert!(row_texts.iter().any(|t| t.contains("med")));
                assert!(row_texts.iter().any(|t| t.contains("high")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_active_marker() {
        let state = Rc::new(RefCell::new(Some("med".to_string())));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("thinking ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                assert!(!plan.active_rows.is_empty(), "med should have active marker");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_tab_drills_to_value() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let r = h.eval_prompt("thinking ", HostKey::None);
        match &r {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.lines.iter().filter(|l| !l.is_header).count(), 4);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        match h.eval_prompt("thinking ", HostKey::Tab) {
            MenuResult::Continue(plan, prompt) => {
                assert!(plan.lines.iter().filter(|l| !l.is_header).count() == 0
                    || plan.lines.iter().any(|l| l.text.contains("off")),
                    "should show leaf content, got {:?}", plan.lines);
                assert!(prompt.contains("off"), "prompt should contain off: {}", prompt);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_enter_submits() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        let r = h.eval_prompt("thinking ", HostKey::Tab);
        let prompt = match &r { MenuResult::Continue(_, p) => p.clone(), _ => panic!() };
        match h.eval_prompt(&prompt, HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(*state.borrow(), Some("off".to_string()));
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn thinking_enter_drills_and_submits() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        match h.eval_prompt("thinking ", HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(
                    *state.borrow(),
                    Some("off".to_string()),
                    "Enter at listing should drill cursor row (off) and submit"
                );
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn thinking_enter_drills_cursor_position() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        let _ = h.eval_prompt("thinking ", HostKey::MoveCursor(3));
        match h.eval_prompt("thinking ", HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(
                    *state.borrow(),
                    Some("high".to_string()),
                    "Enter should drill cursor row (high) and submit"
                );
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn thinking_esc_pops() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        let _ = h.eval_prompt("thinking ", HostKey::Tab);
        match h.eval_prompt("thinking off ", HostKey::Esc) {
            MenuResult::Continue(_, prompt) => {
                assert!(prompt.contains("thinking"), "should be back at thinking: {}", prompt);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_fuzzy_filter() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("thinking h", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(row_texts.iter().any(|t| t.contains("high")));
                assert!(!row_texts.iter().any(|t| t.contains("off")));
                assert!(!row_texts.iter().any(|t| t.contains("low")));
                assert!(!row_texts.iter().any(|t| t.contains("med")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_move_cursor() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        match h.eval_prompt("thinking ", HostKey::MoveCursor(2)) {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.selected, Some(2));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn root_listing_shows_thinking() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(row_texts.iter().any(|t| t.contains("thinking")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_move_cursor_then_tab() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        let _ = h.eval_prompt("thinking ", HostKey::MoveCursor(3));
        match h.eval_prompt("thinking ", HostKey::Tab) {
            MenuResult::Continue(_, prompt) => {
                assert!(prompt.contains("high"), "should drill into high: {}", prompt);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_enter_different_value() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        let _ = h.eval_prompt("thinking ", HostKey::MoveCursor(2));
        let r = h.eval_prompt("thinking ", HostKey::Tab);
        let prompt = match &r { MenuResult::Continue(_, p) => p.clone(), _ => panic!() };
        assert!(prompt.contains("med"), "should be med: {}", prompt);
        match h.eval_prompt(&prompt, HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(*state.borrow(), Some("med".to_string()));
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }
}

// ===========================================================================
// Persona command tests — same engine harness, tests the persona pattern.
// ===========================================================================

#[cfg(test)]
mod persona_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::slashdot_menu::node_impl::{
        DataResponse, DataResult, HostKey, MenuResult, PathArgs,
        SlashDotNode, SubmitResult, StaticListingNode, StaticRow,
    };
    use crate::slashdot_menu::menu::SlashDotMenu;

    /// Single node returning all persona rows + "none".
    struct PersonaEnumNode {
        shared: Rc<RefCell<Option<String>>>,
    }

    impl SlashDotNode for PersonaEnumNode {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            let current = self.shared.borrow();
            let names = ["architect", "strict", "security", "none"];
            let mut rows = Vec::new();
            let mut active_ids = Vec::new();
            for name in &names {
                rows.push(crate::slashdot_menu::fields::Row::new(*name).desc(""));
                let matched = if *name == "none" {
                    current.is_none() || current.as_deref() == Some("")
                } else {
                    current.as_deref() == Some(*name)
                };
                if matched {
                    active_ids.push(name.to_string());
                }
            }
            Ok(Rc::new(DataResponse { rows, active_ids }))
        }

        fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            let name = path_args.ids.last().cloned().unwrap_or_default();
            let val = if name == "none" { None } else { Some(name) };
            *self.shared.borrow_mut() = val;
            SubmitResult::Done(None)
        }
    }

    fn build_persona_menu(state: &Rc<RefCell<Option<String>>>) -> SlashDotMenu {
        let mut menu = SlashDotMenu::new();
        menu.add_root_child(StaticListingNode::new(&[
            StaticRow::new("persona", "set runtime persona"),
        ]))
        .add_child_node(PersonaEnumNode { shared: state.clone() });
        menu
    }

    #[test]
    fn persona_lists_values() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("persona ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert_eq!(row_texts.len(), 4);
                assert!(row_texts.iter().any(|t| t.contains("architect")));
                assert!(row_texts.iter().any(|t| t.contains("strict")));
                assert!(row_texts.iter().any(|t| t.contains("none")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn persona_active_marker() {
        let state = Rc::new(RefCell::new(Some("strict".to_string())));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("persona ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                assert!(!plan.active_rows.is_empty(), "strict should have active marker");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn persona_active_none_when_empty() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("persona ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                assert!(!plan.active_rows.is_empty(), "none should be active when persona is None");
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn persona_enter_submits() {
        let state = Rc::new(RefCell::new(Some("architect".to_string())));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("persona ", HostKey::None);
        let _ = h.eval_prompt("persona ", HostKey::Tab);
        match h.eval_prompt("persona architect ", HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(*state.borrow(), Some("architect".to_string()));
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn persona_enter_drills_and_submits() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("persona ", HostKey::None);
        match h.eval_prompt("persona ", HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(*state.borrow(), None);
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn persona_enter_drills_active_row() {
        let state = Rc::new(RefCell::new(Some("strict".to_string())));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("persona ", HostKey::None);
        match h.eval_prompt("persona ", HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(
                    *state.borrow(),
                    Some("strict".to_string()),
                    "Enter at listing should drill active row (strict) and submit"
                );
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn persona_none_clears() {
        let state = Rc::new(RefCell::new(Some("strict".to_string())));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("persona ", HostKey::None);
        let _ = h.eval_prompt("persona ", HostKey::MoveCursor(2));
        match h.eval_prompt("persona ", HostKey::Tab) {
            MenuResult::Continue(_, prompt) => {
                assert!(prompt.contains("none"), "should drill none: {}", prompt);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        match h.eval_prompt("persona none ", HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(*state.borrow(), None, "none should clear persona");
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn persona_root_listing() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(row_texts.iter().any(|t| t.contains("persona")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }
}
