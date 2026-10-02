//! v2.5 command node implementations for testing the slashdot_menu engine.
//!
//! These test nodes exercise the engine's traversal, cursor, drill,
//! and submit paths without depending on the full picocode host.

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::slashdot_menu::menu::SlashDotMenu;
    use crate::slashdot_menu::node_impl::{
        DataResponse, DataResult, MenuResult, PathArgs, SlashDotNode, StaticListingNode, StaticRow,
        SubmitResult,
    };
    use crate::slashdot_menu::HostKey;

    fn eval(
        h: &mut crate::slashdot_menu::SlashDotHandle<'_>,
        prompt: &str,
        key: HostKey,
    ) -> MenuResult {
        // Resolve ContinuePrompt like the host (repl::v25_eval) does:
        // tests observe frames, never half-completed rewrites.
        futures::executor::block_on(h.eval_prompt_resolved(prompt, key, 80))
    }

    /// Rendered row texts with ANSI SGR escapes stripped (fuzzy highlighting).
    fn row_texts(plan: &crate::slashdot_menu::render::RenderPlan) -> Vec<String> {
        plan.rows
            .iter()
            .map(|l| {
                let mut s = String::new();
                let mut chars = l.0.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '\x1b' {
                        for e in chars.by_ref() {
                            if e.is_ascii_alphabetic() {
                                break;
                            }
                        }
                        continue;
                    }
                    s.push(c);
                }
                s
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // ReasoningEnumNode — single node returning all four levels.
    // -----------------------------------------------------------------------

    struct ReasoningEnumNode {
        app_state: Rc<RefCell<Option<String>>>,
    }

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for ReasoningEnumNode {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            let current = self.app_state.borrow();
            let active = current.as_deref().unwrap_or("off").to_string();
            let rows = ["off", "low", "med", "high", "max"]
                .iter()
                .map(|l| crate::slashdot_menu::fields::Row::new(*l).desc(""))
                .collect();
            Ok(Rc::new(DataResponse {
                rows,
                active_ids: vec![active],
            }))
        }

        async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            let level = path_args.ids.last().cloned().unwrap_or_default();
            *self.app_state.borrow_mut() = Some(level);
            SubmitResult::Done(None)
        }
    }

    // -----------------------------------------------------------------------
    // Menu construction
    // -----------------------------------------------------------------------

    fn build_reasoning_menu(state: &Rc<RefCell<Option<String>>>) -> SlashDotMenu {
        use crate::slashdot_menu::SlashDotMenuNode;
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        let mut tl: SlashDotMenuNode =
            StaticListingNode::new(&[StaticRow::new("reasoning", "set thinking level")]).into();
        tl.add_child(
            ReasoningEnumNode {
                app_state: state.clone(),
            }
            .into(),
        );
        root.add_child(tl);
        SlashDotMenu::from_root_node(root)
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[test]
    fn reasoning_lists_values() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        match eval(&mut h, "reasoning ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
                assert_eq!(row_texts.len(), 5);
                assert!(row_texts.iter().any(|t| t.contains("off")));
                assert!(row_texts.iter().any(|t| t.contains("low")));
                assert!(row_texts.iter().any(|t| t.contains("med")));
                assert!(row_texts.iter().any(|t| t.contains("high")));
                assert!(row_texts.iter().any(|t| t.contains("max")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn reasoning_active_marker() {
        let state = Rc::new(RefCell::new(Some("med".to_string())));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        match eval(&mut h, "reasoning ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                assert!(
                    !plan.active_rows.is_empty(),
                    "med should have active marker"
                );
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn reasoning_tab_drills_to_value() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        let r = eval(&mut h, "reasoning ", HostKey::None);
        match &r {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.rows.len(), 5);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        match eval(&mut h, "reasoning ", HostKey::Tab) {
            MenuResult::Continue(plan, prompt) => {
                assert!(
                    plan.rows.is_empty() || plan.rows.iter().any(|l| l.0.contains("off")),
                    "should show leaf content, got {:?}",
                    plan.rows
                );
                assert!(
                    prompt.contains("off"),
                    "prompt should contain off: {}",
                    prompt
                );
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn reasoning_enter_submits() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        let _ = eval(&mut h, "reasoning ", HostKey::None);
        let r = eval(&mut h, "reasoning ", HostKey::Tab);
        let prompt = match &r {
            MenuResult::Continue(_, p) => p.clone(),
            _ => panic!(),
        };
        match eval(&mut h, &prompt, HostKey::Enter) {
            MenuResult::DoneSuccess(..) => {
                assert_eq!(*state.borrow(), Some("off".to_string()));
            }
            other => panic!("expected DoneSuccess, got {:?}", other),
        }
    }

    #[test]
    fn reasoning_enter_drills_and_submits() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        let _ = eval(&mut h, "reasoning ", HostKey::None);
        match eval(&mut h, "reasoning ", HostKey::Enter) {
            MenuResult::DoneSuccess(..) => {
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
    fn reasoning_enter_drills_cursor_position() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        let _ = eval(&mut h, "reasoning ", HostKey::None);
        let _ = eval(&mut h, "reasoning ", HostKey::MoveCursor(3));
        match eval(&mut h, "reasoning ", HostKey::Enter) {
            MenuResult::DoneSuccess(..) => {
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
    fn reasoning_esc_pops() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        let _ = eval(&mut h, "reasoning ", HostKey::None);
        let _ = eval(&mut h, "reasoning ", HostKey::Tab);
        match eval(&mut h, "reasoning off ", HostKey::Esc) {
            MenuResult::Continue(_, prompt) => {
                assert!(
                    prompt.contains("reasoning"),
                    "should be back at thinking: {}",
                    prompt
                );
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn reasoning_fuzzy_filter() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        match eval(&mut h, "reasoning h", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let rows = row_texts(&plan);
                assert!(rows.iter().any(|t| t.contains("high")));
                assert!(!rows.iter().any(|t| t.contains("off")));
                assert!(!rows.iter().any(|t| t.contains("low")));
                assert!(!rows.iter().any(|t| t.contains("med")));
                assert!(!rows.iter().any(|t| t.contains("max")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn reasoning_move_cursor() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        let _ = eval(&mut h, "reasoning ", HostKey::None);
        match eval(&mut h, "reasoning ", HostKey::MoveCursor(2)) {
            MenuResult::Continue(plan, _) => {
                assert_eq!(plan.selected, Some(2));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn root_listing_shows_reasoning() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        match eval(&mut h, "", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
                assert!(row_texts.iter().any(|t| t.contains("reasoning")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn reasoning_move_cursor_then_tab() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        let _ = eval(&mut h, "reasoning ", HostKey::None);
        let _ = eval(&mut h, "reasoning ", HostKey::MoveCursor(3));
        match eval(&mut h, "reasoning ", HostKey::Tab) {
            MenuResult::Continue(_, prompt) => {
                assert!(
                    prompt.contains("high"),
                    "should drill into high: {}",
                    prompt
                );
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn reasoning_enter_different_value() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_reasoning_menu(&state);
        let mut h = menu.get_handle();
        let _ = eval(&mut h, "reasoning ", HostKey::None);
        let _ = eval(&mut h, "reasoning ", HostKey::MoveCursor(2));
        let r = eval(&mut h, "reasoning ", HostKey::Tab);
        let prompt = match &r {
            MenuResult::Continue(_, p) => p.clone(),
            _ => panic!(),
        };
        assert!(prompt.contains("med"), "should be med: {}", prompt);
        match eval(&mut h, &prompt, HostKey::Enter) {
            MenuResult::DoneSuccess(..) => {
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

    use crate::slashdot_menu::menu::SlashDotMenu;
    use crate::slashdot_menu::node_impl::{
        DataResponse, DataResult, MenuResult, PathArgs, SlashDotNode, StaticListingNode, StaticRow,
        SubmitResult,
    };
    use crate::slashdot_menu::HostKey;

    fn eval(
        h: &mut crate::slashdot_menu::SlashDotHandle<'_>,
        prompt: &str,
        key: HostKey,
    ) -> MenuResult {
        // Resolve ContinuePrompt like the host (repl::v25_eval) does.
        futures::executor::block_on(h.eval_prompt_resolved(prompt, key, 80))
    }

    /// Single node returning all persona rows + "none".
    struct PersonaEnumNode {
        app_state: Rc<RefCell<Option<String>>>,
    }

    #[async_trait::async_trait(?Send)]
    impl SlashDotNode for PersonaEnumNode {
        async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
            let current = self.app_state.borrow();
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

        async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
            let name = path_args.ids.last().cloned().unwrap_or_default();
            let val = if name == "none" { None } else { Some(name) };
            *self.app_state.borrow_mut() = val;
            SubmitResult::Done(None)
        }
    }

    fn build_persona_menu(state: &Rc<RefCell<Option<String>>>) -> SlashDotMenu {
        use crate::slashdot_menu::SlashDotMenuNode;
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        let mut pl: SlashDotMenuNode =
            StaticListingNode::new(&[StaticRow::new("persona", "set runtime persona")]).into();
        pl.add_child(
            PersonaEnumNode {
                app_state: state.clone(),
            }
            .into(),
        );
        root.add_child(pl);
        SlashDotMenu::from_root_node(root)
    }

    #[test]
    fn persona_lists_values() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        match eval(&mut h, "persona ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
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
        match eval(&mut h, "persona ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                assert!(
                    !plan.active_rows.is_empty(),
                    "strict should have active marker"
                );
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn persona_active_none_when_empty() {
        let state = Rc::new(RefCell::new(None));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        match eval(&mut h, "persona ", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                assert!(
                    !plan.active_rows.is_empty(),
                    "none should be active when persona is None"
                );
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    fn persona_enter_submits() {
        let state = Rc::new(RefCell::new(Some("architect".to_string())));
        let mut menu = build_persona_menu(&state);
        let mut h = menu.get_handle();
        let _ = eval(&mut h, "persona ", HostKey::None);
        let _ = eval(&mut h, "persona ", HostKey::Tab);
        match eval(&mut h, "persona architect ", HostKey::Enter) {
            MenuResult::DoneSuccess(..) => {
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
        let _ = eval(&mut h, "persona ", HostKey::None);
        match eval(&mut h, "persona ", HostKey::Enter) {
            MenuResult::DoneSuccess(..) => {
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
        let _ = eval(&mut h, "persona ", HostKey::None);
        match eval(&mut h, "persona ", HostKey::Enter) {
            MenuResult::DoneSuccess(..) => {
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
        let _ = eval(&mut h, "persona ", HostKey::None);
        let _ = eval(&mut h, "persona ", HostKey::MoveCursor(2));
        match eval(&mut h, "persona ", HostKey::Tab) {
            MenuResult::Continue(_, prompt) => {
                assert!(prompt.contains("none"), "should drill none: {}", prompt);
            }
            other => panic!("expected Continue, got {:?}", other),
        }
        match eval(&mut h, "persona none ", HostKey::Enter) {
            MenuResult::DoneSuccess(..) => {
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
        match eval(&mut h, "", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
                assert!(row_texts.iter().any(|t| t.contains("persona")));
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }
}
