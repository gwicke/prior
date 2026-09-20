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
// ThinkingEnumNode — returns all thinking levels from a single node.
//
// The parent listing shows off/low/med/high as rows. On submit, the
// selected level is read from path_args (the drilled row ID) and written
// to the shared cell.
// ---------------------------------------------------------------------------

struct ThinkingEnumNode {
    shared: Rc<RefCell<Option<String>>>,
}

impl SlashDotNode for ThinkingEnumNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let current = self.shared.borrow();
        let levels = ["off", "low", "med", "high"];
        let active = current.as_deref().unwrap_or("off").to_string();
        let rows = levels.iter()
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

// ---------------------------------------------------------------------------
// Menu builder — called once at REPL startup.
// ---------------------------------------------------------------------------

/// Build the v2.5 `SlashDotMenu` tree with all ported commands.
pub fn build_menu(
    thinking_level: &Rc<RefCell<Option<String>>>,
    settings: &Rc<RefCell<crate::settings::SettingsStore>>,
    shared: &Rc<crate::slash_menu_bridge::MenuShared>,
) -> SlashDotMenu {
    let mut menu = SlashDotMenu::new();

    // Thinking: listing + single enum node (returns all levels via data()).
    menu.add_root_child(StaticListingNode::new(&[
        StaticRow::new("thinking", "set thinking level"),
    ]).with_columns(vec![
        crate::slashdot_menu::ColumnDef::new("id", "Thinking Level"),
    ]))
    .add_child_node(ThinkingEnumNode { shared: thinking_level.clone() });

    // Persona
    crate::v25_persona::add_persona_children(&mut menu, settings);

    // Model
    crate::v25_model::add_model_children(&mut menu, shared);

    menu
}
