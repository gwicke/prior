//! v2.5 `SlashDotNode` implementations for `/persona`.
//!
//! A single `PersonaEnumNode` returns all persona names + "none" from
//! `data()`. Active marker from `LiveState.persona` (transient session
//! state), write on submit. `/set default_persona` will use a separate
//! node that reads/writes `SettingsStore`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
};
use crate::slashdot_menu::menu::SlashDotMenuNode;

// ---------------------------------------------------------------------------
// PersonaEnumNode — returns all persona rows from a single node.
// ---------------------------------------------------------------------------

pub struct PersonaEnumNode {
    /// Read the current persona name (None = no persona).
    active_fn: Box<dyn Fn() -> Option<String>>,
    /// Write the selected persona. `Ok(())` on success.
    submit_fn: Box<dyn FnMut(&str) -> Result<(), String>>,
}

impl SlashDotNode for PersonaEnumNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let current = (self.active_fn)().unwrap_or_default();
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
        match (self.submit_fn)(&name) {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

// ---------------------------------------------------------------------------
// Menu builder — adds persona child to a listing node.
// ---------------------------------------------------------------------------

/// Add `/persona` child to a listing `SlashDotMenuNode`.
pub fn add_persona_child(
    listing: &mut SlashDotMenuNode,
    live_state: &Rc<RefCell<crate::slash_menu_bridge::LiveState>>,
) {
    let ls = live_state.clone();
    let ls2 = live_state.clone();
    listing.add_child(PersonaEnumNode {
        active_fn: Box::new(move || ls.borrow().persona.clone()),
        submit_fn: Box::new(move |name| {
            let val = if name == "none" { None } else { Some(name.to_string()) };
            ls2.borrow_mut().persona = val;
            Ok(())
        }),
    }.into());
}
