//! v2.5 command node implementations for testing the slashdot_menu engine.
//!
//! These test nodes exercise the engine's traversal, cursor, drill,
//! and submit paths without depending on the full picocode host.

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::slashdot_menu::fields::Row;
    use crate::slashdot_menu::node_impl::{
        DataResponse, DataResult, HostKey, MenuResult, PathArgs,
        SlashDotNode, SubmitResult, StaticListingNode, StaticRow,
    };
    use crate::slashdot_menu::menu::SlashDotMenu;

    // -----------------------------------------------------------------------
    // ThinkingValue — test fixture modeling /thinking enum values.
    // Holds an Rc<RefCell<Option<String>>> — same pattern as the real
    // ThinkingValue in v25_thinking.rs (shared with MenuShared).
    // -----------------------------------------------------------------------

    struct ThinkingValue {
        level: String,
        shared: Rc<RefCell<Option<String>>>,
    }

    impl ThinkingValue {
        fn new(level: &str, shared: Rc<RefCell<Option<String>>>) -> Self {
            ThinkingValue {
                level: level.to_string(),
                shared,
            }
        }
    }

    impl SlashDotNode for ThinkingValue {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            let current = self.shared.borrow();
            let active = if current.as_deref() == Some(&self.level) {
                vec![self.level.clone()]
            } else {
                Vec::new()
            };
            Ok(Rc::new(DataResponse {
                rows: vec![Row::new(&self.level).desc("")],
                active_ids: active,
            }))
        }

        fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
            *self.shared.borrow_mut() = Some(self.level.clone());
            SubmitResult::Done(None)
        }
    }

    // -----------------------------------------------------------------------
    // Menu construction
    // -----------------------------------------------------------------------

    fn build_thinking_menu(state: &Rc<RefCell<Option<String>>>) -> SlashDotMenu {
        let mut menu = SlashDotMenu::new();

        // Root-level: StaticListingNode replaces a dedicated ThinkingNode impl.
        menu.add_child(&[], StaticListingNode::new(&[
            StaticRow::new("thinking", "set thinking level"),
        ]));

        // Enum value children under thinking (child index "0" in root's children).
        for level in &["off", "low", "med", "high"] {
            menu.add_child(
                &["0"],
                ThinkingValue::new(level, state.clone()),
            );
        }

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
        // Enter at the listing level should drill into the cursor row and
        // submit immediately, without requiring a separate Tab first.
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
