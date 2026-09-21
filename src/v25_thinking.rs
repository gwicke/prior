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
use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};

// ---------------------------------------------------------------------------
// ThinkingEnumNode — returns all thinking levels from a single node.
//
// The parent listing shows off/low/med/high as rows. On submit, the
// selected level is read from path_args (the drilled row ID) and written
// to the shared cell.
// ---------------------------------------------------------------------------

pub struct ThinkingEnumNode {
    /// Read the current thinking level.
    pub active_fn: Box<dyn Fn() -> String>,
    /// Write the selected thinking level. `Ok(())` on success.
    pub submit_fn: Box<dyn FnMut(&str) -> Result<(), String>>,
}

impl SlashDotNode for ThinkingEnumNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let active = (self.active_fn)();
        let levels: &[(&str, &str)] = &[
            ("off", "no extra reasoning"),
            ("low", "minimal reasoning"),
            ("med", "balanced reasoning"),
            ("high", "maximum reasoning"),
        ];
        let rows = levels.iter()
            .map(|(l, d)| crate::slashdot_menu::fields::Row::new(*l).desc(*d))
            .collect();
        Ok(Rc::new(DataResponse {
            rows,
            active_ids: vec![active],
        }))
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let level = path_args.ids.last().cloned().unwrap_or_default();
        match (self.submit_fn)(&level) {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

// ---------------------------------------------------------------------------
// Menu builder — called once at REPL startup.
// ---------------------------------------------------------------------------

/// Build the v2.5 `SlashDotMenu` tree with all ported commands.
pub fn build_menu(
    live_state: &Rc<RefCell<crate::slash_menu_bridge::LiveState>>,
    settings: &Rc<RefCell<crate::settings::SettingsStore>>,
    shared: &Rc<crate::slash_menu_bridge::MenuShared>,
) -> SlashDotMenu {
    let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();

    // Thinking: listing + single enum node (returns all levels via data()).
    {
        let ls = live_state.clone();
        let ls2 = live_state.clone();
        let mut tl: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("thinking", "Set thinking level: off | low | med | high"),
        ]).with_columns(vec![
            crate::slashdot_menu::ColumnDef::new("id", "Thinking Level"),
        ]).into();
        tl.add_child(ThinkingEnumNode {
            active_fn: Box::new(move || {
                let ls = ls.borrow();
                ls.thinking_level.clone().unwrap_or_else(|| "off".to_string())
            }),
            submit_fn: Box::new(move |level| {
                ls2.borrow_mut().thinking_level = Some(level.to_string());
                Ok(())
            }),
        }.into());
        root.add_child(tl);
    }

    // Persona
    {
        let mut pl: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("persona", "set runtime persona"),
        ]).with_columns(vec![
            crate::slashdot_menu::ColumnDef::new("id", "Persona"),
            crate::slashdot_menu::ColumnDef::new("description", "Description"),
        ]).into();
        crate::v25_persona::add_persona_child(&mut pl, live_state);
        root.add_child(pl);
    }

    // Model
    {
        let ml: SlashDotMenuNode = crate::v25_model::build_model_node(shared);
        root.add_child(ml);
    }

    // Settings (/set)
    {
        let sn: SlashDotMenuNode = crate::v25_set::build_set_node(settings);
        root.add_child(sn);
    }

    SlashDotMenu::from_root_node(root)
}
