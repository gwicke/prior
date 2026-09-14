//! # slashdot_menu
//!
//! A data-driven slash-menu engine: menu descriptors, a uniform per-level
//! path grammar, field filtering/sorting, cursor-in-the-path semantics, and
//! a generic overlay interpreter.
//!
//! The crate is independent of any host application: host data (catalogs,
//! sessions, filesystem) is supplied through handler traits, the prompt
//! buffer through a narrow [`prompt::PromptBuffer`] interface, and rendering
//! is emitted as a plain-data [`render::RenderPlan`] the host paints.
//!
//! See `doc/slash_menu_v2_design.md` in the picocode repository for the
//! design this implements.

pub mod accept;
pub mod cursor;
pub mod fields;
pub mod filter;
pub mod fuzzy;
pub mod grammar;
pub mod interpreter;
pub mod node;
pub mod prompt;
pub mod render;
pub mod provider;
pub mod table;
pub mod value;
pub mod wrap;

pub use table::{Align, Table};
