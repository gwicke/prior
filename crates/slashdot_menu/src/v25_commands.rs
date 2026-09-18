//! v2.5 command node implementations and integration tests.
//!
//! Each command is one `impl SlashDotNode`. The engine handles all
//! traversal, cursor, and child management internally.

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::fields::Row;
    use crate::node_impl::{
        DataResponse, DataResult, HostKey, MenuResult, PathArgs,
        SlashDotNode, SubmitResult,
    };

    use crate::menu::SlashDotMenu;

    // -----------------------------------------------------------------------
    // Thinking command (ported from v2.0 slash_menu_bridge)
    // -----------------------------------------------------------------------

    struct TestState {
        current_thinking_level: RefCell<Option<String>>,
    }

    impl TestState {
        fn new() -> Rc<Self> {
            Rc::new(TestState {
                current_thinking_level: RefCell::new(None),
            })
        }
    }

    /// The `/thinking` command node: returns the "thinking" row for the
    /// parent listing.
    struct ThinkingNode {
        _shared: Rc<TestState>,
    }

    impl SlashDotNode for ThinkingNode {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            Ok(Rc::new(DataResponse {
                rows: vec![Row::new("thinking").desc("set thinking level")],
                active_ids: Vec::new(),
            }))
        }
    }

    /// One of the four thinking level values (off/low/med/high).
    struct ThinkingValue {
        level: String,
        shared: Rc<TestState>,
    }

    impl ThinkingValue {
        fn new(level: &str, shared: Rc<TestState>) -> Self {
            ThinkingValue {
                level: level.to_string(),
                shared,
            }
        }
    }

    impl SlashDotNode for ThinkingValue {
        fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            let current = self.shared.current_thinking_level.borrow();
            let active = if current.as_deref() == Some(&self.level) {
                vec![self.level.clone()]
            } else {
                Vec::new()
            };
            Ok(Rc::new(DataResponse {
                rows: vec![Row::new(&self.level)],
                active_ids: active,
            }))
        }

        fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
            *self.shared.current_thinking_level.borrow_mut() =
                Some(self.level.clone());
            SubmitResult::Done(None)
        }
    }

    // -----------------------------------------------------------------------
    // Menu construction
    // -----------------------------------------------------------------------

    fn build_thinking_menu(state: &Rc<TestState>) -> SlashDotMenu {
        let mut menu = SlashDotMenu::new();

        // Thinking listing node (child of root, gets index 0).
        menu.add_child(&[], ThinkingNode { _shared: state.clone() });

        // Enum value children under thinking (index "0" in root's children).
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
        let state = TestState::new();
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("thinking ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert_eq!(row_texts.len(), 4);
                assert!(row_texts.contains(&"off"));
                assert!(row_texts.contains(&"low"));
                assert!(row_texts.contains(&"med"));
                assert!(row_texts.contains(&"high"));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_active_marker() {
        let state = TestState::new();
        *state.current_thinking_level.borrow_mut() = Some("med".to_string());
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
        // Tab from "thinking " drills into the first value (off).
        let state = TestState::new();
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        // First open the thinking listing
        let r = h.eval_prompt("thinking ", HostKey::None);
        match &r {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.lines.iter().filter(|l| !l.is_header).count(), 4);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        // Tab drills into first value
        match h.eval_prompt("thinking ", HostKey::Tab) {
            MenuResult::Continue(plan, prompt) => {
                // Leaf: no data rows (ThinkingValue has no children)
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
        // Tab into thinking listing, Tab into first value, Enter submits.
        let state = TestState::new();
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        let r = h.eval_prompt("thinking ", HostKey::Tab);
        let prompt = match &r { MenuResult::Continue(_, p) => p.clone(), _ => panic!() };
        match h.eval_prompt(&prompt, HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(*state.current_thinking_level.borrow(), Some("off".to_string()));
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn thinking_esc_pops() {
        let state = TestState::new();
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        let _ = h.eval_prompt("thinking ", HostKey::Tab);
        // At "thinking off ". Esc should pop back to thinking listing.
        match h.eval_prompt("thinking off ", HostKey::Esc) {
            MenuResult::Continue(_, prompt) => {
                assert!(prompt.contains("thinking"), "should be back at thinking: {}", prompt);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_fuzzy_filter() {
        let state = TestState::new();
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        match h.eval_prompt("thinking h", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.lines.iter()
                    .filter(|l| !l.is_header)
                    .map(|l| l.text.as_str())
                    .collect();
                assert!(row_texts.contains(&"high"));
                assert!(!row_texts.contains(&"off"));
                assert!(!row_texts.contains(&"low"));
                assert!(!row_texts.contains(&"med"));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_move_cursor() {
        let state = TestState::new();
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
        let state = TestState::new();
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
        // MoveCursor within same handle persists, Tab drills into that row.
        let state = TestState::new();
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        // Move cursor to "high" (index 3)
        let _ = h.eval_prompt("thinking ", HostKey::MoveCursor(3));
        // Tab drills into cursor row ("high")
        match h.eval_prompt("thinking ", HostKey::Tab) {
            MenuResult::Continue(_, prompt) => {
                assert!(prompt.contains("high"), "should drill into high: {}", prompt);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn thinking_enter_different_value() {
        // MoveCursor to "med", Tab to drill, Enter to submit.
        let state = TestState::new();
        let mut menu = build_thinking_menu(&state);
        let mut h = menu.get_handle();
        let _ = h.eval_prompt("thinking ", HostKey::None);
        let _ = h.eval_prompt("thinking ", HostKey::MoveCursor(2));
        let r = h.eval_prompt("thinking ", HostKey::Tab);
        let prompt = match &r { MenuResult::Continue(_, p) => p.clone(), _ => panic!() };
        assert!(prompt.contains("med"), "should be med: {}", prompt);
        match h.eval_prompt(&prompt, HostKey::Enter) {
            MenuResult::DoneSuccess(_) => {
                assert_eq!(*state.current_thinking_level.borrow(), Some("med".to_string()));
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }
}
