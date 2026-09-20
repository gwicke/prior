//! Public API for the slash menu engine.
//!
//! Host applications use:
//! - `SlashDotNode` trait — implement for each menu command
//! - `SlashDotMenu` — owns the node tree; `add_root_child` to build, `get_handle` to run
//! - `SlashDotHandle` — guard-like; `eval_prompt` to process a prompt
//!
//! # Building a menu tree
//!
//! ```ignore
//! use slashdot_menu::{SlashDotMenu, StaticListingNode, StaticRow};
//!
//! let mut menu = SlashDotMenu::new();
//! menu.add_root_child(StaticListingNode::new(&[
//!     StaticRow::new("thinking", "set thinking level"),
//! ]))
//! .add_child_node(ThinkingEnumNode::new())
//! .add_child_node(SomeOtherChild::new());
//! ```

use crate::node_entry::NodeEntry;
use crate::node_impl::{HostKey, MenuResult, SlashDotNode};

/// Opaque handle to a freshly-added node. Exposes `add_child_node` for
/// building subtrees without leaking `NodeEntry` internals.
///
/// Returned by [`SlashDotMenu::add_root_child`] and
/// [`ChildBuilder::add_child_node`]. The lifetime is tied to the
/// underlying mutable borrow of the menu's tree.
pub struct ChildBuilder<'a>(&'a mut NodeEntry);

impl<'a> ChildBuilder<'a> {
    /// Append a child node under this node. Returns `&mut Self` for
    /// chaining:
    /// ```ignore
    /// builder.add_child_node(a).add_child_node(b);
    /// ```
    pub fn add_child_node(&mut self, child: impl SlashDotNode + 'static) -> &mut Self {
        self.0.add_child_node(child);
        self
    }
}

/// The main menu engine. Owns the node tree and epoch counter.
/// Created empty; children added via `add_root_child`.
/// Lifetime: ~app lifetime (or as long as the menu is needed).
pub struct SlashDotMenu {
    root: NodeEntry,
    epoch: u64,
}

impl SlashDotMenu {
    /// Create a new empty menu.
    pub fn new() -> Self {
        SlashDotMenu {
            root: NodeEntry::new_root(),
            epoch: 0,
        }
    }

    /// Add a child node to the root level. Returns a [`ChildBuilder`]
    /// so the caller can add grandchildren without knowing internal types:
    /// ```ignore
    /// menu.add_root_child(listing)
    ///     .add_child_node(value_a)
    ///     .add_child_node(value_b);
    /// ```
    pub fn add_root_child(&mut self, child: impl SlashDotNode + 'static) -> ChildBuilder<'_> {
        self.root.add_child_node(child);
        ChildBuilder(self.root.last_child_mut())
    }

    /// Get a handle for one menu invocation. This bumps the epoch.
    /// Returns a guard-like handle that borrows the menu.
    pub fn get_handle(&mut self) -> SlashDotHandle<'_> {
        self.epoch += 1;
        SlashDotHandle {
            root: &mut self.root,
            epoch: self.epoch,
        }
    }
}

/// Guard-like handle for one menu invocation. Borrows the menu's root
/// node. Exposes `eval_prompt` — the only way for the host to interact
/// with the menu.
pub struct SlashDotHandle<'a> {
    root: &'a mut NodeEntry,
    epoch: u64,
}

impl<'a> SlashDotHandle<'a> {
    /// Evaluate a prompt with a key event. `term_width` is the current
    /// terminal width in columns, used for table column layout.
    pub fn eval_prompt(&mut self, prompt: &str, key: HostKey, term_width: usize) -> MenuResult {
        self.root
            .node_traverse(self.epoch, key, Vec::new(), prompt, term_width)
    }
}
