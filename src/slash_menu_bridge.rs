//! Host-side menu state shared between the REPL/agent loop and the v2.5
//! slashdot_menu node tree (design §9/§12).
//!
//! The v2.5 command nodes (`v25_*`, built by `v25_menu`) read/write the
//! live cells below directly; async work requested from a node's sync
//! `submit()` rides out via `MenuShared::pending_op` and is executed by
//! the agent loop in its own async context.

use std::cell::RefCell;
use std::rc::Rc;

use crate::session::repo::JsonlSessionRepo;

/// Verify `MenuShared`'s file-scoped cells stay inside the `RefCell`
/// (compile-time: constructing a `MenuShared` takes plain `&self`-style
/// borrows so nothing escapes as an `&mut`).
const _: () = {
    fn assert_cell<T>() {}
    let _ = assert_cell::<Rc<RefCell<crate::settings::SettingsStore>>>;
};

/// Shared application state the menu handlers read/write at call time
/// Transient session state — model, persona, thinking level. These are
/// the *live* values for the current session, distinct from the persistent
/// defaults in `SettingsStore`. `/model`, `/persona`, and `/thinking`
/// read/write here; `/set` reads/writes `SettingsStore`. The REPL keeps
/// `LiveState` in sync from `AgentConfig` on each loop iteration.
pub struct LiveState {
    /// Session model as `provider:model`. Empty = default.
    pub model: String,
    /// Session persona name. `None` = no persona.
    pub persona: Option<String>,
    /// Session thinking level. `None` ≈ "off".
    pub thinking_level: Option<String>,
}

impl Default for LiveState {
    fn default() -> Self {
        LiveState {
            model: String::new(),
            persona: None,
            thinking_level: None,
        }
    }
}

/// Live state bundle shared between the REPL and the v2.5 menu nodes.
///
/// (design §9/§12). Handles are **live** stores, not snapshots: handlers
/// read and write the same underlying data the rest of the app uses. All
/// `Rc<RefCell<…>>` (single-threaded app); `Arc` only where a background
/// thread participates (the catalog's global cache). The REPL keeps
/// building `ReplCtx` snapshots from the same storage for its own overlay
/// rendering.
pub struct MenuShared {
    /// The settings store — canonical persistent state the `/set` handler
    /// and `run_interactive` both mutate. `/set` nodes read/write directly;
    /// live commands (`/model`, `/persona`, `/thinking`) use `live_state`
    /// instead and never touch this (except via the deferred rebuild path).
    pub settings: Rc<RefCell<crate::settings::SettingsStore>>,
    /// Transient session state (model, persona, thinking level). Live
    /// commands read/write here; the REPL syncs from `AgentConfig` each
    /// loop iteration.
    pub live_state: Rc<RefCell<LiveState>>,
    /// Session repository (if available). The `/session` listing provider
    /// calls `list_sessions_sync` on it; session mutation handlers write
    /// through it. `None` when no session store exists.
    pub repo: Option<JsonlSessionRepo>,
    /// The id of the currently-active session (empty = none). Written by
    /// session actions; read per `fetch`.
    pub current_session_id: Rc<RefCell<String>>,
    /// Recently-used model ids (LRU, most recent first) — the provider's
    /// recency boost source. `run_interactive` keeps this in sync with its
    /// `recent_models` local on every SetModel.
    pub recent_models: Rc<RefCell<Vec<String>>>,
    /// The session entry tree (source of truth for /tree listing).
    /// Immutable between rebuilds; shared via Rc with ReplCtx.
    /// REPL writes once per prompt cycle; `data()` reads many times.
    /// Inner Rc enables `ptr_eq` caching in TreeListNode.
    pub raw_tree: Rc<RefCell<Rc<Vec<crate::session::SessionTreeNode>>>>,
    pub leaf_id: Rc<RefCell<Option<String>>>,
    /// Session token usage snapshot (set per `read_input` call from the
    /// agent's `session_usage()`). Used by `/stat` to display context
    /// window and cost info.
    pub session_usage: Rc<RefCell<crate::agent::SessionUsage>>,
    /// Async operation requested by a submit handler.  The agent loop
    /// checks this after each `DoneSuccess` and executes it in its own
    /// async context (the node cannot call `block_on` from inside a
    /// tokio runtime).
    pub pending_op: Rc<RefCell<Option<AsyncOp>>>,
}

/// Operations that a node can request but cannot execute itself (because
/// `submit()` is sync and may run inside a tokio context).
#[derive(Debug, Clone)]
pub enum AsyncOp {
    /// Run `/replay` with the given argument (e.g. `""`, `"5t"`, `"10l"`).
    Replay(String),
    /// Resume a session by UUID, optionally replay the last N turns.
    Resume { session_id: String, replay_arg: String },
    /// Fork a session by UUID into a new session in the current directory.
    Fork { session_id: String },
    /// Rename a session (empty new_name clears the name).
    Rename { session_id: String, new_name: String },
    /// Delete a session by UUID.
    Delete { session_id: String },
    /// Quit the session (break the main loop).
    Quit,
    /// Start a new session, optionally named.
    New { name: String },
    /// Delete the current session and start fresh.
    Clear { name: String },
    /// Move the session leaf pointer to the given entry.
    TreeSwitch { target_id: String },
    /// Compact the session at a given entry.
    TreeCompact {
        target_id: String,
        mode: CompactMode,
        payload: String,
    },
        /// Mutate the sandbox path mappings (v2.5 `/sandbox`). The agent
    /// loop owns `config`, so it performs the mutation + re-publish.
    Sandbox(SandboxOp),
}

/// Compact mode for `AsyncOp::TreeCompact`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactMode {
    /// User-editable touched-files list (already edited in $EDITOR).
    Manual,
    /// Deterministic text extraction (already edited in $EDITOR).
    TextOnly,
    /// LLM-generated summary; `payload` is the custom prompt.
    Prompt,
}

/// Sandbox path-mapping mutations requested by the v2.5 `/sandbox`
/// nodes. Executed by the agent loop (it owns `config`, which must be
/// re-published with the merged bindings — see the `AsyncOp::Sandbox`
/// arm).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxOp {
    /// Add/upgrade a session binding.
    Add {
        host_path: String,
        sandbox_path: String,
        mode: crate::config::BindMode,
    },
    /// Remove a session binding.
    Remove { sandbox_path: String },
    /// Temporarily exclude a config binding.
    Exclude { sandbox_path: String },
    /// Re-enable an excluded config binding.
    Restore { sandbox_path: String },
}

impl MenuShared {
    pub fn new(
        settings: crate::settings::SettingsStore,
        repo: Option<JsonlSessionRepo>,
    ) -> Self {
        MenuShared {
            settings: Rc::new(RefCell::new(settings)),
            live_state: Rc::new(RefCell::new(LiveState::default())),
            repo,
            current_session_id: Rc::new(RefCell::new(String::new())),
            recent_models: Rc::new(RefCell::new(Vec::new())),
            raw_tree: Rc::new(RefCell::new(Rc::new(Vec::new()))),
            leaf_id: Rc::new(RefCell::new(None)),
            session_usage: Rc::new(RefCell::new(crate::agent::SessionUsage::default())),
            pending_op: Rc::new(RefCell::new(None)),
        }
    }

    /// Clone the cell *handles* (shared ownership), not their contents —
    /// the v2.5 nodes then observe the same live cells the REPL writes.
    pub fn clone_shared(&self) -> MenuShared {
        MenuShared {
            settings: self.settings.clone(),
            live_state: self.live_state.clone(),
            repo: self.repo.clone(),
            current_session_id: self.current_session_id.clone(),
            recent_models: self.recent_models.clone(),
            raw_tree: self.raw_tree.clone(),
            leaf_id: self.leaf_id.clone(),
            session_usage: self.session_usage.clone(),
            pending_op: self.pending_op.clone(),
        }
    }
}
