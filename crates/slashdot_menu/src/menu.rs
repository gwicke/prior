//! Public API for the slash menu engine.
//!
//! Host applications use:
//! - `SlashDotNode` trait — implement for each menu command
//! - `SlashDotMenuNode` — wraps a node; `add_child` to build the tree
//! - `SlashDotMenu` — owns the built tree; `get_handle` to run
//! - `SlashDotHandle` — guard-like; `eval_prompt` to process a prompt
//!
//! # Building a menu tree
//!
//! ```ignore
//! use slashdot_menu::{SlashDotMenu, SlashDotMenuNode, StaticListingNode, StaticRow};
//!
//! let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
//! {
//!     let mut tl: SlashDotMenuNode = StaticListingNode::new(&[
//!         StaticRow::new("thinking", "set thinking level"),
//!     ]).with_columns(vec![ColumnDef::new("id", "Level")]).into();
//!     tl.add_child(ThinkingEnumNode::new().into());
//!     root.add_child(tl);
//! }
//! {
//!     let mut ml: SlashDotMenuNode = StaticListingNode::new(&[
//!         StaticRow::new("model", "select a model"),
//!     ]).into();
//!     ml.add_child(ModelNode::new().into());
//!     root.add_child(ml);
//! }
//!
//! let menu = SlashDotMenu::from_root_node(root);
//! ```

use crate::node_entry::NodeEntry;
use crate::node_impl::{HostKey, MenuResult, SlashDotNode};

/// A node in the menu tree. Wraps the internal `NodeEntry` with a
/// minimal public API: `new()` to create a node, `add_child()` to
/// attach children. Use `From<T: SlashDotNode>` (`.into()`) for
/// convenience.
pub struct SlashDotMenuNode(pub(crate) NodeEntry);

impl SlashDotMenuNode {
    /// Create a node from a host `SlashDotNode` impl.
    pub fn new(inner: impl SlashDotNode + 'static) -> Self {
        SlashDotMenuNode(NodeEntry::new_child(Box::new(inner)))
    }

    /// Append a child node. Returns `&mut Self` for chaining:
    /// ```ignore
    /// node.add_child(a).add_child(b);
    /// ```
    pub fn add_child(&mut self, child: SlashDotMenuNode) -> &mut Self {
        self.0.add_child_entry(child.0);
        self
    }

    /// Enable value forwarding on this node. When enabled, `node_data()`
    /// enriches this node's rows with active values from children's
    /// `node_data()` responses (injected as the `"value"` field).
    ///
    /// This is the public API for the `ActiveValueForwardingListingNode`
    /// pattern: a listing node whose value column is populated from its
    /// children's active IDs.
    pub fn with_value_forwarding(mut self) -> Self {
        self.0.value_forwarding = true;
        self
    }
}

impl<T: SlashDotNode + 'static> From<T> for SlashDotMenuNode {
    fn from(node: T) -> Self {
        SlashDotMenuNode::new(node)
    }
}

/// The main menu engine. Owns the node tree and epoch counter.
/// Created from a root `SlashDotMenuNode`; `get_handle` to run.
pub struct SlashDotMenu {
    root: NodeEntry,
    epoch: u64,
}

impl SlashDotMenu {
    /// Create a menu from a pre-built root node.
    pub fn from_root_node(root: SlashDotMenuNode) -> Self {
        SlashDotMenu {
            root: root.0,
            epoch: 0,
        }
    }

    /// Get a handle for one menu invocation. This bumps the epoch.
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
