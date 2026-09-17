//! Public API for the slash menu engine.
//!
//! Host applications use:
//! - `SlashDotNode` trait — implement for each menu command
//! - `SlashDotMenu` — owns the node tree; `add_child` to build, `get_handle` to get a handle
//! - `SlashDotHandle` — guard-like; `eval_prompt` to process a prompt

use crate::node_entry::NodeEntry;
use crate::node_impl::{HostKey, MenuResult, SlashDotNode};

/// The main menu engine. Owns the node tree and epoch counter.
/// Created empty; children added via `add_child`.
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

    /// Add a child node to the parent at `parent_path`. If `parent_path` is
    /// empty, the child is added to the root level.
    pub fn add_child(&mut self, parent_path: &[&str], child: impl SlashDotNode + 'static) {
        self.root.add_child(parent_path, NodeEntry::new_child(Box::new(child)));
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
    /// Evaluate a prompt with a key event. Returns a `MenuResult` the host
    /// acts on.
    pub fn eval_prompt(&mut self, prompt: &str, key: HostKey) -> MenuResult {
        self.root
            .node_traverse(self.epoch, key, Vec::new(), prompt)
    }
}
