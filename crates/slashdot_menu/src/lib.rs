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

// --- Internal modules (exposed to host for v2.0 bridge) ---
pub mod node_impl;
pub mod node_entry;
pub mod node;
pub mod provider;
pub mod interpreter;
pub mod cursor;
pub mod fields;
pub mod filter;
pub mod fuzzy;
pub mod grammar;
pub mod prompt;
pub mod render;
pub mod table;
pub mod value;
pub mod wrap;
pub mod accept;

// Re-export public types for convenience.
pub use menu::{SlashDotMenu, SlashDotHandle};
pub use node_impl::{SlashDotNode, HostKey, MenuResult, PathArgs, Args, DataResponse, DataError, DataResult, SubmitResult, StaticListingNode, StaticRow};
pub use provider::ColumnDef;
pub use table::{Align, Table};