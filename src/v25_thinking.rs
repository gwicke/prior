//! v2.5 `SlashDotNode` implementations for picocode commands.
//!
//! Each command is one `impl SlashDotNode`. The engine handles traversal,
//! cursor, and child management internally.

use std::cell::RefCell;
use std::rc::Rc;

use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
    StaticListingNode, StaticRow,
};
use crate::slashdot_menu::menu::SlashDotMenu;

// ---------------------------------------------------------------------------
// ThinkingValue — one of the four enum values (off/low/med/high).
//
// Holds an `Rc<RefCell<Option<String>>>` that is the *same cell* the REPL
// owns (via `MenuShared.current_thinking_level`). `submit()` writes to it;
// the REPL reads it back after `DoneSuccess`. No parallel state.
// ---------------------------------------------------------------------------

pub struct ThinkingValue {
    level: String,
    shared: Rc<RefCell<Option<String>>>,
}

impl ThinkingValue {
    pub fn new(level: &str, shared: Rc<RefCell<Option<String>>>) -> Self {
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
            rows: vec![crate::slashdot_menu::fields::Row::new(&self.level).desc("")],
            active_ids: active,
        }))
    }

    fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        *self.shared.borrow_mut() = Some(self.level.clone());
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// Menu builder — called once at REPL startup.
// ---------------------------------------------------------------------------

/// Build the v2.5 `SlashDotMenu` tree with all ported commands.
pub fn build_menu(
    thinking_level: &Rc<RefCell<Option<String>>>,
    settings: &Rc<RefCell<crate::settings::SettingsStore>>,
) -> SlashDotMenu {
    let mut menu = SlashDotMenu::new();

    // Thinking: StaticListingNode parent + ThinkingValue children.
    menu.add_child(&[], StaticListingNode::new(&[
        StaticRow::new("thinking", "set thinking level"),
    ]));
    for level in &["off", "low", "med", "high"] {
        menu.add_child(
            &["0"],
            ThinkingValue::new(level, thinking_level.clone()),
        );
    }

    // Persona: StaticListingNode parent + PersonaValue children.
    crate::v25_persona::add_persona_children(&mut menu, settings);

    menu
}
