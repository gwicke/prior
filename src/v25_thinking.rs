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
// Shared state: mutable reference shared between all nodes in the tree.
// ---------------------------------------------------------------------------

/// Shared state for the thinking command tree. All ThinkingNode and
/// ThinkingValue instances hold an `Rc` to this; mutations in `submit`
/// are visible to all subsequent `data()` calls.
#[derive(Debug)]
pub struct ThinkingState {
    pub level: RefCell<Option<String>>,
}

impl ThinkingState {
    pub fn new(initial: Option<String>) -> Rc<Self> {
        Rc::new(ThinkingState {
            level: RefCell::new(initial),
        })
    }
}

// ---------------------------------------------------------------------------
// ThinkingValue — one of the four enum values (off/low/med/high).
// ---------------------------------------------------------------------------

pub struct ThinkingValue {
    level: String,
    shared: Rc<ThinkingState>,
}

impl ThinkingValue {
    pub fn new(level: &str, shared: Rc<ThinkingState>) -> Self {
        ThinkingValue {
            level: level.to_string(),
            shared,
        }
    }
}

impl SlashDotNode for ThinkingValue {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let current = self.shared.level.borrow();
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
        *self.shared.level.borrow_mut() = Some(self.level.clone());
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// Menu builder — called once at REPL startup.
// ---------------------------------------------------------------------------

/// Build the v2.5 `SlashDotMenu` tree. Currently only contains `/thinking`.
/// Other commands will be added as they are ported.
pub fn build_menu(thinking_state: &Rc<ThinkingState>) -> SlashDotMenu {
    let mut menu = SlashDotMenu::new();

    // Root-level commands: each `add_child(&[], ...)` adds a child at the
    // root. The root's listing merges all children's `data()` rows.
    // Uses StaticListingNode — no dedicated impl needed.
    menu.add_child(&[], StaticListingNode::new(&[
        StaticRow::new("thinking", "set thinking level"),
    ]));

    // ThinkingValue children under ThinkingNode (child index "0").
    for level in &["off", "low", "med", "high"] {
        menu.add_child(
            &["0"],
            ThinkingValue::new(level, thinking_state.clone()),
        );
    }

    menu
}
