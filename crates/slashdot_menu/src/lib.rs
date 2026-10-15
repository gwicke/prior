//! # slashdot_menu
//!
//! The v2.5 slash-menu engine: a persistent node tree whose nodes return
//! rows (static or dynamic) per level, a uniform path grammar, field
//! filtering/sorting, cursor-in-the-path semantics, and plain-data
//! [`render::RenderPlan`] output the host paints.
//!
//! The crate is independent of any host application: host state is wired
//! through [`node_impl::SlashDotNode`] implementations, the prompt buffer
//! through a narrow [`prompt::PromptBuffer`] interface.
//!
//! See `doc/slash_menu_v2.5_design.md` in the picocode repository for the
//! design this implements.

// --- Public API (what the host sees) ---
pub mod menu;

// --- Engine modules ---
pub mod cursor;
pub mod elements;
pub mod fields;
pub mod filter;
pub mod fuzzy;
pub mod grammar;
pub mod node_entry;
pub mod node_impl;
pub mod prompt;
pub mod render;
pub mod table;
pub mod value;
pub mod view;
pub mod wrap;

// Re-export public types for convenience.
pub use elements::{ElementSet, Update};
pub use fields::ColumnDef;
pub use menu::{SlashDotHandle, SlashDotMenu, SlashDotMenuNode};
pub use node_entry::{HostKey, RowRef};
pub use node_impl::{
    CompletionRows, DataError, DataResponse, DataResult, MenuResult, PathArgs, RenderContext,
    SlashDotNode, StaticListingNode, StaticRow, SubmitResult,
};
pub use render::LogicalLine;
pub use table::{Align, Table};
pub use view::{RowView, ViewPlan, ViewRequest, ViewRows};
