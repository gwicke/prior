//! v2.5 `SlashDotNode` implementations for `/persona`.
//!
//! A single `PersonaEnumNode` returns all persona names + "none" from
//! `data()`. Active marker from `SettingsStore.default_persona`, write on
//! submit.

use std::cell::RefCell;
use std::rc::Rc;

use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
    StaticListingNode, StaticRow,
};
use crate::slashdot_menu::menu::SlashDotMenu;

// ---------------------------------------------------------------------------
// PersonaEnumNode — returns all persona rows from a single node.
// ---------------------------------------------------------------------------

struct PersonaEnumNode {
    settings: Rc<RefCell<crate::settings::SettingsStore>>,
}

impl SlashDotNode for PersonaEnumNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let s = self.settings.borrow();
        let current = s.default_persona.as_deref().unwrap_or("");
        let mut rows = Vec::new();
        let mut active_ids = Vec::new();
        for p in crate::persona::PERSONAS.iter() {
            rows.push(crate::slashdot_menu::fields::Row::new(p.name).desc(p.description));
            if current == p.name {
                active_ids.push(p.name.to_string());
            }
        }
        // "none" row: active when persona is empty.
        rows.push(crate::slashdot_menu::fields::Row::new("none").desc("clear persona"));
        if current.is_empty() {
            active_ids.push("none".to_string());
        }
        Ok(Rc::new(DataResponse { rows, active_ids }))
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let name = path_args.ids.last().cloned().unwrap_or_default();
        let val = if name == "none" { None } else { Some(name) };
        self.settings.borrow_mut().default_persona = val;
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// Menu builder — adds persona to the root menu.
// ---------------------------------------------------------------------------

/// Add `/persona` children to an existing `SlashDotMenu`.
pub fn add_persona_children(
    menu: &mut SlashDotMenu,
    settings: &Rc<RefCell<crate::settings::SettingsStore>>,
) {
    menu.add_root_child(StaticListingNode::new(&[
        StaticRow::new("persona", "set runtime persona"),
    ]).with_columns(vec![
        crate::slashdot_menu::ColumnDef::new("id", "Persona"),
        crate::slashdot_menu::ColumnDef::new("description", "Description"),
    ]))
    .add_child_node(PersonaEnumNode { settings: settings.clone() });
}
