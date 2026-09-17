//! Internal node entry: wraps a host `SlashDotNode` with children,
//! cursor cache, and traversal logic. Not exposed to the host.

use std::collections::HashMap;

use crate::node_impl::{
    DataResult, HostKey, MenuResult, PathArgs,
    SlashDotNode,
};

/// Internal node entry: wraps a host `SlashDotNode` impl with children,
/// cursor cache, and traversal logic. Not exposed to the host.
pub(crate) struct NodeEntry {
    inner: Box<dyn SlashDotNode>,
    /// Static children (built via `add_child`). The child's `data()` rows
    /// are merged into the parent's listing. Routing is done by child
    /// index, not by key.
    children: Vec<NodeEntry>,
    /// Cursor cache: maps a path key to (active index, selection, terms).
    cursor_cache: HashMap<String, (usize, Option<String>, String)>,
    /// Data cache: maps a path key to the last data response (for
    /// pointer-equality "nothing changed" signals).
    data_cache: HashMap<String, DataResult>,
}

/// A minimal `SlashDotNode` that does nothing (for the root container).
struct RootSlashDotNode;

impl SlashDotNode for RootSlashDotNode {}

impl NodeEntry {
    /// Create a root node (metadata-only container, no data/submit).
    pub fn new_root() -> Self {
        NodeEntry {
            inner: Box::new(RootSlashDotNode),
            children: Vec::new(),
            cursor_cache: HashMap::new(),
            data_cache: HashMap::new(),
        }
    }

    /// Create a child node from a host `SlashDotNode` impl.
    pub fn new_child(inner: Box<dyn SlashDotNode>) -> Self {
        NodeEntry {
            inner,
            children: Vec::new(),
            cursor_cache: HashMap::new(),
            data_cache: HashMap::new(),
        }
    }

    /// Add a child node at the root level (path `[]` = root).
    pub fn add_child(&mut self, parent: &[&str], child: NodeEntry) {
        if parent.is_empty() {
            self.children.push(child);
        } else {
            let mut node = self;
            for id in parent {
                let idx: usize = id.parse().expect("parent path must be child index");
                node = &mut node.children[idx];
            }
            node.children.push(child);
        }
    }

    /// Get a child by index.
    fn child(&self, idx: usize) -> Option<&NodeEntry> {
        self.children.get(idx)
    }

    /// Get a child by index (mutable).
    fn child_mut(&mut self, idx: usize) -> Option<&mut NodeEntry> {
        self.children.get_mut(idx)
    }

    /// Drive one key event through this node's subtree.
    pub fn node_traverse(
        &mut self,
        epoch: u64,
        key: HostKey,
        path_args: &PathArgs,
    ) -> MenuResult {
        // Stub — step 4 will implement the full traversal.
        let _ = (epoch, key, path_args);
        MenuResult::DoneError("not yet implemented".into())
    }

    /// Get the data rows for this node.
    pub fn node_data(&mut self, epoch: u64, path_args: &PathArgs) -> DataResult {
        self.inner.data(epoch, path_args)
    }
}