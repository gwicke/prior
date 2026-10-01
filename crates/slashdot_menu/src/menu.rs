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

use crate::node_entry::HostKey;
use crate::node_entry::NodeEntry;
use crate::node_impl::{MenuResult, RenderContext, SlashDotNode};

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

    /// Get a handle for a *continuing* invocation — the epoch
    /// [`SlashDotMenu::get_handle`] already bumped for it.
    ///
    /// The split exists because the epoch means two opposite things to the
    /// two callers. `get_handle` bumps so that a **new** invocation cannot
    /// inherit the previous one's intent. This one must not *lose* the
    /// current invocation's: a node's `EpochState` (node_entry §epoch
    /// state — the §5.1 view restore, `should_restore_view_args`, the armed
    /// confirm dialog) is reset whenever `cache_epoch != epoch`, so handing
    /// out a freshly-bumped epoch on every key would wipe the user's
    /// position in the menu after each keystroke.
    ///
    /// The host tracks which of the two it wants with its own
    /// "am I still inside one `/`-session" flag.
    ///
    /// TODO(api): this pair asks the host to track an open invocation and to
    /// pick the right constructor for it — `PromptSession::epoch_active` and
    /// `PromptSession::slash_handle` exist only to do that. A better shape
    /// would make the invocation the unit: something the host *holds* for the
    /// duration of one `/`-session, constructed with the bump and offering
    /// only continuation afterwards, so there is no flag and no choice. The
    /// host already does exactly that where it can — `Repl::batch_slash` keeps
    /// `menu` and `handle` as two locals for a whole rewrite chain.
    ///
    /// What blocks it is not taste but the borrow rule: the invocation would
    /// borrow the menu, and a struct that owns its menu cannot also hold a
    /// borrow of it. Fixing that properly means either an unsafe/self-
    /// referential handle or moving the menu out of the session, which would
    /// stop `PromptSession` being one value the turn can carry (ui_draw_v2
    /// §4.5 U4). Worth revisiting if that constraint ever lifts.
    pub fn continuation_handle(&mut self) -> SlashDotHandle<'_> {
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
    /// Evaluate a prompt with a key event, cursor at the end of the
    /// prompt (the common case: tests and end-of-line interaction).
    /// `term_width` is the current terminal width in columns; it is
    /// packaged into a [`RenderContext`] and threaded through the
    /// traversal by reference.
    pub async fn eval_prompt(
        &mut self,
        prompt: &str,
        key: HostKey,
        term_width: usize,
    ) -> MenuResult {
        self.eval_prompt_with_cursor(prompt, key, term_width, prompt.len())
            .await
    }

    /// Like [`eval_prompt`](Self::eval_prompt) with an explicit cursor
    /// (byte offset into `prompt`). v1 only supports end-of-line
    /// completion — the resolved level suppresses completion when the
    /// cursor is not at the end of its section, so a mid-line cursor
    /// can never complete a different word than the user edits.
    pub async fn eval_prompt_with_cursor(
        &mut self,
        prompt: &str,
        key: HostKey,
        term_width: usize,
        cursor_pos: usize,
    ) -> MenuResult {
        let render_ctx = RenderContext { term_width };
        self.root
            .node_traverse_at(self.epoch, key, Vec::new(), prompt, cursor_pos, &render_ctx)
            .await
    }

    /// Like [`eval_prompt`](Self::eval_prompt), but resolves
    /// [`MenuResult::ContinuePrompt`] rewrite chains: apply the new
    /// prompt and re-eval with [`HostKey::None`] until a plan-carrying
    /// or terminal result arrives. This is the standard host contract —
    /// a host that has no cursor state of its own can call this and
    /// only ever see frames (`Continue`) and terminal outcomes.
    ///
    /// Bounded: a rewrite cycle is an engine bug, reported as a
    /// `DoneError` whose message carries the full chain (no plan is
    /// ever synthesized). Hosts with their own cursor handling (the
    /// REPL's `v25_eval`) implement the same loop themselves so they
    /// can trace it.
    pub async fn eval_prompt_resolved(
        &mut self,
        prompt: &str,
        key: HostKey,
        term_width: usize,
    ) -> MenuResult {
        // Normal chains are 1–2 steps (rewrite → passive re-derive).
        const MAX_REWRITES: usize = 16;
        let mut prompt = prompt.to_string();
        let mut key = key;
        let mut chain: Vec<String> = Vec::new();
        for _ in 0..MAX_REWRITES {
            match self.eval_prompt(&prompt, key, term_width).await {
                MenuResult::ContinuePrompt(new_prompt) => {
                    chain.push(new_prompt.clone());
                    prompt = new_prompt;
                    key = HostKey::None;
                }
                other => return other,
            }
        }
        MenuResult::DoneError(
            format!(
                "menu engine bug: prompt-rewrite cycle after {} steps: {}",
                MAX_REWRITES,
                chain.join(" -> ")
            ),
            prompt,
        )
    }
}
