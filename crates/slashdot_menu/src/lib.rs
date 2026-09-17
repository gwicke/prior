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

// --- Public API (what the host sees) ---
pub mod menu;

// --- Internal modules (not exposed to host) ---
pub(crate) mod node_impl;
mod node_entry;
mod node;
mod provider;
mod interpreter;
mod cursor;
mod fields;
mod filter;
mod fuzzy;
mod grammar;
mod prompt;
mod render;
mod table;
mod value;
mod wrap;
mod accept;

// Re-export public types for convenience.
pub use menu::{SlashDotMenu, SlashDotHandle};
pub use node_impl::{SlashDotNode, HostKey, MenuResult, PathArgs, Args, DataResponse, DataError, DataResult, SubmitResult};
pub use table::{Align, Table};