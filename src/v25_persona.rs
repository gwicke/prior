//! v2.5 `SlashDotNode` implementations for `/persona`.
//!
//! Structurally identical to `/thinking`: enum values (persona names + "none"),
//! active marker from shared settings store, write on submit. Demonstrates
//! the shared-state pattern with a different backing store
//! (`SettingsStore.default_persona` vs a standalone cell).

use std::cell::RefCell;
use std::rc::Rc;

use crate::slashdot_menu::fields::Row;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
    StaticListingNode, StaticRow,
};
use crate::slashdot_menu::menu::SlashDotMenu;

// ---------------------------------------------------------------------------
// PersonaValue — one persona name (or "none").
//
// Holds a reference to the settings store — the same `Rc<RefCell<...>>`
// the REPL owns via `MenuShared`. `submit()` writes to
// `settings.default_persona`; the REPL reads it back after `DoneSuccess`.
// ---------------------------------------------------------------------------

pub struct PersonaValue {
    name: String,
    settings: Rc<RefCell<crate::settings::SettingsStore>>,
}

impl PersonaValue {
    pub fn new(name: &str, settings: Rc<RefCell<crate::settings::SettingsStore>>) -> Self {
        PersonaValue {
            name: name.to_string(),
            settings,
        }
    }
}

impl SlashDotNode for PersonaValue {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let s = self.settings.borrow();
        let current = s.default_persona.as_deref().unwrap_or("");
        // "none" row matches empty persona (explicit clear).
        let matched = if self.name == "none" {
            current.is_empty()
        } else {
            current == self.name
        };
        let active = if matched {
            vec![self.name.clone()]
        } else {
            Vec::new()
        };
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new(&self.name).desc("")],
            active_ids: active,
        }))
    }

    fn submit(&mut self, _epoch: u64, _path_args: &PathArgs) -> SubmitResult {
        let val = if self.name == "none" {
            None
        } else {
            Some(self.name.clone())
        };
        self.settings.borrow_mut().default_persona = val;
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// Menu builder — adds persona to the root menu.
// ---------------------------------------------------------------------------

/// Add `/persona` children to an existing `SlashDotMenu`.
///
/// Called from the top-level `build_menu()` after thinking children.
pub fn add_persona_children(
    menu: &mut SlashDotMenu,
    settings: &Rc<RefCell<crate::settings::SettingsStore>>,
) {
    // Listing node at root (next child index will be "1" after thinking at "0").
    menu.add_child(&[], StaticListingNode::new(&[
        StaticRow::new("persona", "set runtime persona"),
    ]));

    // Persona value children under the listing node (index "1" in root).
    // Names from the PERSONAS array + "none" for explicit clear.
    let mut names: Vec<&str> = crate::persona::PERSONAS
        .iter()
        .map(|p| p.name)
        .collect();
    names.push("none");

    for name in &names {
        menu.add_child(
            &["1"],
            PersonaValue::new(name, settings.clone()),
        );
    }
}
