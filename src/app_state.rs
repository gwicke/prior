//! `AppState` / `State` — the single source of truth for menu-visible
//! application state (design: `doc/slashdot/state_and_async.md`, "Context
//! shape (decided): A″").
//!
//! This replaces `MenuShared` + `LiveState` + the pre-eval snapshot copies:
//! one coarse `parking_lot::Mutex<State>` behind one `Arc`, injected into
//! node instances. Nodes hold `Arc<AppState>` (background tasks clone the
//! `Arc`); the engine never sees it.
//!
//! # Lock rules (review-enforced — see the design doc)
//!
//! 1. **Encapsulated handle only** — no public cells; all access goes through
//!    the methods below. Otherwise this is `MenuShared` again.
//! 2. **Lock once per public method** — private helpers take `&mut State`.
//!    The mutex is *not* re-entrant: an inner re-lock deadlocks.
//! 3. **Never hold a guard across `.await`** — clone the handle/value out,
//!    drop the guard, await, relock to write. (This module is sync-only for
//!    that reason; async facades layer on top later.)
//! 4. **Whole read-modify-write in one acquisition** — a lock fixes data
//!    races, not lost updates split across critical sections.
//! 5. **Single source of truth** — no mirrors, no snapshots. `control` target
//!    fields are written by eval, read + GC'd only by their sole consumer
//!    (the loop's convergence step).
//! 6. **Only prepare/submit mutate** — `data()`/`header()` are render-path
//!    reads (convention).
//!
//! # Target vs applied
//!
//! `State.config` is the **runtime target** (directives: what should be in
//! effect); the loop's local `AgentConfig` is the **applied** layer, converged
//! to the target at the cycle point. The durable target is `SettingsStore`
//! (written through [`AppState::set_setting`], persisted by the store).
//! The active `Session` object itself is the loop's applied layer; the menu
//! sees [`SessionSlot`] (`repo` handle + current id) only.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::agent::{PicoAgent, SessionUsage};
use crate::session::repo::JsonlSessionRepo;
use crate::session::{Session, SessionTreeNode};
use crate::settings::{SettingKey, SettingsStore};
use crate::AgentConfig;

// ---------------------------------------------------------------------------
// Control register types (moved from slash_menu_bridge::AsyncOp — intent
// fields, not a queue; see design doc "Decisions")
// ---------------------------------------------------------------------------

/// Resume a session by id, optionally replaying the last N turns.
#[derive(Debug, Clone)]
pub struct ResumeIntent {
    pub session_id: String,
    pub replay_arg: String,
}

/// Rename a session (empty `new_name` clears the name).
#[derive(Debug, Clone)]
pub struct RenameIntent {
    pub session_id: String,
    pub new_name: String,
}

/// Compact mode for `tree_compact`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactMode {
    /// User-editable touched-files list (already edited in `$EDITOR`).
    Manual,
    /// Deterministic text extraction (already edited in `$EDITOR`).
    TextOnly,
    /// LLM-generated summary; `payload` is the custom prompt.
    Prompt,
}

/// Compact the session at a given entry (LLM mode needs the agent — loop or
/// agent-side consumption).
#[derive(Debug, Clone)]
pub struct TreeCompactIntent {
    pub target_id: String,
    pub mode: CompactMode,
    pub payload: String,
}

/// Sandbox path-mapping mutation requested by the v2.5 `/sandbox` nodes.
/// The applier owns the merge + re-publish (it owns the applied config).
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

/// Declarative control targets: handlers write; each field's sole consumer
/// (the loop's convergence step) applies at its application point. One-shot
/// fields are taken (GC'd) by [`AppState::take_control`]; `quit` latches and
/// is never cleared. Idempotent re-application is safe by design.
#[derive(Debug, Clone, Default)]
pub struct ControlConfig {
    /// Latching: the loop exits when it observes `true` (never cleared).
    pub quit: bool,
    /// `/new` (and `/clear`): create a fresh session, optionally named.
    pub new_session: Option<String>,
    /// `/session <id> resume [replay]` — load + swap + prefill.
    pub resume: Option<ResumeIntent>,
    /// `/session <id> fork`.
    pub fork: Option<String>,
    /// `/session <id> rename <name>`.
    pub rename: Option<RenameIntent>,
    /// Sessions that should not exist (idempotent deletes; `/clear` pushes
    /// the outgoing current id alongside `new_session`).
    pub delete_session_ids_after: Vec<String>,

    /// `/tree <id> compact …`.
    pub tree_compact: Option<TreeCompactIntent>,
    /// `/sandbox` mutations (applier merges + re-publishes).
    pub sandbox: Option<SandboxOp>,
    /// Explicit `/model` row commit — the loop's convergence step records a
    /// session `ModelChange` only for this id, and only when the *applied*
    /// config ends up running it (failed rebuild records nothing).
    pub model_commit: Option<String>,
}

impl ControlConfig {
    /// Clone out every one-shot field and clear it (sole-consumer GC).
    /// `quit` is latching and untouched.
    fn take_one_shots(&mut self) -> ControlConfig {
        ControlConfig {
            quit: self.quit,
            new_session: self.new_session.take(),
            resume: self.resume.take(),
            fork: self.fork.take(),
            rename: self.rename.take(),
            delete_session_ids_after: std::mem::take(&mut self.delete_session_ids_after),

            tree_compact: self.tree_compact.take(),
            sandbox: self.sandbox.take(),
            model_commit: self.model_commit.take(),
        }
    }
}

// ---------------------------------------------------------------------------
// SessionSlot — the menu-facing view of session state. The active `Session`
// object itself is the loop's applied layer (loop-local); `repo` is a
// Clone-shared handle (Arc cache) so both views see the same listing state.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct SessionSlot {
    pub(crate) repo: Option<JsonlSessionRepo>,
    pub(crate) current_id: String,
    /// The active session — shared handle (all methods `&self`; internals
    /// are `Arc<AsyncMutex>` by design). Swapped atomically with
    /// `current_id` via `activate_session` so the menu never sees a
    /// half-applied activation. The loop clones it out for turns; menu
    /// handlers clone it out for direct data-plane work.
    pub(crate) session: Option<Arc<Session>>,
    /// The running agent — `Send + Sync`, all methods `&self`. Created
    /// and rebuilt **loop-side from the applied config** (convergence),
    /// published via `activate_agent`; handlers only consume the `Arc`.
    pub(crate) agent: Option<Arc<dyn PicoAgent>>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Coarse-grained app state: a few reasonably-sized pieces, one source of
/// truth, one lock. Fields are `pub(crate)` — external access goes through
/// [`AppState`] methods (lock rule 1).
pub struct State {
    /// Runtime **target** config: model, persona, thinking, sandbox …
    /// (`LiveState` folded in here). The loop converges its applied config
    /// to this at the cycle point. Durable target stays `SettingsStore`.
    pub(crate) config: AgentConfig,
    /// The durable-target settings store — the one copy (was `MenuShared`'s
    /// cloned cell).
    pub(crate) settings: SettingsStore,
    /// Menu-facing session view (repo handle + current id).
    pub(crate) session: SessionSlot,
    /// Recently-used model ids (LRU, most recent first).
    pub(crate) recent_models: Vec<String>,
    /// Session entry tree snapshot (source of truth for `/tree`).
    pub(crate) raw_tree: Arc<Vec<SessionTreeNode>>,
    pub(crate) leaf_id: Option<String>,
    /// Session token usage snapshot (`/stat`).
    pub(crate) session_usage: SessionUsage,
    /// Estimated tokens of the next-turn context (`/stat`).
    pub(crate) context_tokens: u64,
    /// Declarative control targets (see [`ControlConfig`]).
    pub(crate) control: ControlConfig,
}

// ---------------------------------------------------------------------------
// AppState — the injected handle
// ---------------------------------------------------------------------------

/// The single injected handle — `MenuShared`'s replacement. Nodes hold
/// `Arc<AppState>`; the REPL loop clones the same `Arc`.
pub struct AppState {
    state: Mutex<State>,
}


impl AppState {
    /// Wrap the startup-resolved state. `config` is both the initial target
    /// and (cloned by the loop) the initial applied layer.
    pub fn new(
        config: AgentConfig,
        settings: SettingsStore,
        repo: Option<JsonlSessionRepo>,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            state: Mutex::new(State {
                config,
                settings,
                session: SessionSlot {
                    repo,
                    current_id: String::new(),
                    session: None,
                    agent: None,
                },
                recent_models: Vec::new(),
                raw_tree: Arc::new(Vec::new()),
                leaf_id: None,
                session_usage: SessionUsage::default(),
                context_tokens: 0,
                control: ControlConfig::default(),
            }),
        })
    }

    // --- config (runtime target) ---

    /// Canonical `provider:model` id of the target model.
    pub fn model(&self) -> String {
        self.state.lock().config.qualified_model_id()
    }

    /// Set the target model from a canonical `provider:model` id. Model ids
    /// may themselves contain colons — split only at the first.
    pub fn set_model(&self, qualified_id: &str) {
        let mut st = self.state.lock();
        if let Some((p, m)) = qualified_id.split_once(':') {
            st.config.provider = p.to_string();
            st.config.model = m.to_string();
        }
    }

    pub fn thinking(&self) -> Option<String> {
        self.state.lock().config.thinking_level.clone()
    }

    pub fn set_thinking(&self, level: Option<String>) {
        self.state.lock().config.thinking_level = level;
    }

    pub fn persona(&self) -> Option<String> {
        self.state.lock().config.persona_name.clone()
    }

    pub fn set_persona(&self, name: Option<String>) {
        self.state.lock().config.persona_name = name;
    }

    /// Read access to the target config (one acquisition; return owned data).
    pub fn with_config<R>(&self, f: impl FnOnce(&AgentConfig) -> R) -> R {
        f(&self.state.lock().config)
    }

    /// Mutate the target config (whole RMW inside one acquisition).
    pub fn update_config(&self, f: impl FnOnce(&mut AgentConfig)) {
        f(&mut self.state.lock().config);
    }

    /// Clone of the target config (the loop's convergence step compares it
    /// against its applied layer; `AgentConfig` is cheap-ish: strings + Arcs).
    pub fn config_snapshot(&self) -> AgentConfig {
        self.state.lock().config.clone()
    }

    // --- settings (durable target; the one copy) ---

    /// Read access to the settings store.
    pub fn with_settings<R>(&self, f: impl FnOnce(&SettingsStore) -> R) -> R {
        f(&self.state.lock().settings)
    }

    /// Mutate + persist a setting (validates, applies, saves — same behavior
    /// as the old menu cell, but on the canonical copy).
    pub fn set_setting(&self, key: SettingKey, value: &str) -> Result<(), String> {
        let mut st = self.state.lock();
        st.settings.set_value(key, value)
    }

    /// Snapshot of the settings store for read-only use across `.await`s
    /// (helpers like `handle_session_command` take `&SettingsStore`): clone
    /// out under the lock, then await against the private copy (rule 3).
    pub fn settings(&self) -> SettingsStore {
        self.state.lock().settings.clone()
    }

    // --- session slot (menu-facing view) ---

    pub fn repo(&self) -> Option<JsonlSessionRepo> {
        self.state.lock().session.repo.clone()
    }

    /// Refresh the repo handle (only needed if the loop replaced its local
    /// `Option<JsonlSessionRepo>`; clone-sharing means the cache stays live).
    pub fn set_repo(&self, repo: Option<JsonlSessionRepo>) {
        self.state.lock().session.repo = repo;
    }

    pub fn current_session_id(&self) -> String {
        self.state.lock().session.current_id.clone()
    }

    pub fn set_current_session_id(&self, id: String) {
        self.state.lock().session.current_id = id;
    }

    // --- shared session/agent handles (clone out → drop guard → await) ---

    /// Clone-out of the active session handle (rule 3: the guard is never
    /// held across an await; the returned `Arc` keeps the session alive).
    pub fn session(&self) -> Option<Arc<Session>> {
        self.state.lock().session.session.clone()
    }

    /// Write the session slot (id untouched) — callers write back
    /// unconditionally right after a helper that may have swapped it.
    pub fn set_session(&self, session: Option<Arc<Session>>) {
        self.state.lock().session.session = session;
    }

    /// Bundle activation: session + current id in ONE acquisition —
    /// the menu can never observe a half-applied switch.
    pub fn activate_session(&self, session: Option<Arc<Session>>, current_id: String) {
        let mut st = self.state.lock();
        st.session.session = session;
        st.session.current_id = current_id;
    }

    /// Clone-out of the running agent (created/rebuilt loop-side from the
    /// applied config; handlers only consume).
    pub fn agent(&self) -> Option<Arc<dyn PicoAgent>> {
        self.state.lock().session.agent.clone()
    }

    /// Publish a freshly created/rebuilt agent (convergence, lazy first
    /// prompt, compact's Llm fallback — all loop-side).
    pub fn activate_agent(&self, agent: Arc<dyn PicoAgent>) {
        self.state.lock().session.agent = Some(agent);
    }

    // --- display snapshots (loop writes at cycle build; menu reads) ---

    /// One acquisition for the whole per-cycle view so the menu can never see
    /// a torn pair of fields (rule: one acquisition per logical read/write).
    pub fn update_cycle_view(
        &self,
        current_id: String,
        usage: SessionUsage,
        context_tokens: u64,
        tree: Arc<Vec<SessionTreeNode>>,
        leaf_id: Option<String>,
    ) {
        let mut st = self.state.lock();
        st.session.current_id = current_id;
        st.session_usage = usage;
        st.context_tokens = context_tokens;
        st.raw_tree = tree;
        st.leaf_id = leaf_id;
    }

    pub fn raw_tree(&self) -> Arc<Vec<SessionTreeNode>> {
        self.state.lock().raw_tree.clone()
    }

    pub fn leaf_id(&self) -> Option<String> {
        self.state.lock().leaf_id.clone()
    }

    /// Single-field tree write (incremental updates; the loop's per-cycle
    /// write goes through [`update_cycle_view`] for atomicity).
    pub fn set_raw_tree(&self, tree: Arc<Vec<SessionTreeNode>>) {
        self.state.lock().raw_tree = tree;
    }

    /// Single-field leaf write (same caveat as [`set_raw_tree`]).
    pub fn set_leaf_id(&self, leaf_id: Option<String>) {
        self.state.lock().leaf_id = leaf_id;
    }

    pub fn session_usage(&self) -> SessionUsage {
        self.state.lock().session_usage
    }

    pub fn context_tokens(&self) -> u64 {
        self.state.lock().context_tokens
    }

    // --- recent models (loop is the sole writer) ---

    pub fn recent_models(&self) -> Vec<String> {
        self.state.lock().recent_models.clone()
    }

    /// Seed the recent-models list at startup (no-op when already present).
    pub fn seed_recent_model(&self, id: &str) {
        let mut st = self.state.lock();
        if !st.recent_models.iter().any(|m| m == id) {
            st.recent_models.push(id.to_string());
        }
    }

    /// LRU touch: move `id` to the front. Returns the updated list so the
    /// caller can persist it *outside* the lock (no file IO under the guard).
    pub fn note_recent_model(&self, id: &str) -> Vec<String> {
        let mut st = self.state.lock();
        st.recent_models.retain(|m| m != id);
        st.recent_models.insert(0, id.to_string());
        st.recent_models.clone()
    }

    /// Replace the list wholesale (initial load).
    pub fn set_recent_models(&self, models: Vec<String>) {
        self.state.lock().recent_models = models;
    }

    // --- control targets (handlers write) ---

    pub fn request_quit(&self) {
        self.state.lock().control.quit = true;
    }

    /// Latching quit flag — observed by the loop, never cleared.
    pub fn should_quit(&self) -> bool {
        self.state.lock().control.quit
    }

    pub fn request_new(&self, name: String) {
        self.state.lock().control.new_session = Some(name);
    }

    /// `/clear`: start fresh *and* delete the outgoing session afterwards —
    /// declarative transaction intent (`applier`: create → swap → delete).
    pub fn request_clear(&self, name: String, outgoing_session_id: String) {
        let mut st = self.state.lock();
        st.control.new_session = Some(name);
        if !outgoing_session_id.is_empty()
            && !st.control.delete_session_ids_after.contains(&outgoing_session_id)
        {
            st.control.delete_session_ids_after.push(outgoing_session_id);
        }
    }

    pub fn request_resume(&self, session_id: String, replay_arg: String) {
        self.state.lock().control.resume = Some(ResumeIntent {
            session_id,
            replay_arg,
        });
    }

    pub fn request_fork(&self, session_id: String) {
        self.state.lock().control.fork = Some(session_id);
    }

    pub fn request_rename(&self, session_id: String, new_name: String) {
        self.state.lock().control.rename = Some(RenameIntent {
            session_id,
            new_name,
        });
    }

    pub fn request_delete(&self, session_id: String) {
        let mut st = self.state.lock();
        if !session_id.is_empty()
            && !st.control.delete_session_ids_after.contains(&session_id)
        {
            st.control.delete_session_ids_after.push(session_id);
        }
    }


    pub fn request_tree_compact(&self, target_id: String, mode: CompactMode, payload: String) {
        self.state.lock().control.tree_compact = Some(TreeCompactIntent {
            target_id,
            mode,
            payload,
        });
    }

    pub fn request_sandbox(&self, op: SandboxOp) {
        self.state.lock().control.sandbox = Some(op);
    }

    pub fn set_model_commit(&self, qualified_id: String) {
        self.state.lock().control.model_commit = Some(qualified_id);
    }

    /// Sole-consumer drain: clone out + GC every one-shot control field in a
    /// single acquisition (`quit` latches). Run by the loop's convergence
    /// step every cycle — a missed cycle only delays convergence.
    pub fn take_control(&self) -> ControlConfig {
        self.state.lock().control.take_one_shots()
    }

    // --- test constructor ---

    /// Minimal state bundle for in-crate tests (no repo).
    #[cfg(test)]
    pub fn for_test(settings: SettingsStore) -> Arc<AppState> {
        Self::for_test_with_output(settings, Arc::new(crate::output::NoOutput))
    }

    /// Like [`for_test`], with a caller-chosen output — tests assert on
    /// captured display lines via `RecordingOutput`.
    #[cfg(test)]
    pub fn for_test_with_output(
        settings: SettingsStore,
        output: Arc<dyn crate::output::Output>,
    ) -> Arc<AppState> {
        let config = AgentConfig {
            provider: "test".to_string(),
            model: "model".to_string(),
            output,
            yolo: false,
            quiet: true,
            thinking_level: None,
            tool_call_limit: 10,
            system_message_extension: None,
            persona_prompt: None,
            persona_name: None,
            bash_auto_allow: None,
            agent_prompt: None,
            user_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            sandbox: None,
            show_reasoning_handle: crate::settings::default_show_reasoning_handle(),
            reasoning_in_context: false,
            message_log_path: None,
        };
        Self::new(config, settings, None)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> SettingsStore {
        SettingsStore {
            default_model: String::new(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_thinking_level: "off".into(),
            show_reasoning: "indicator".into(),
            reasoning_in_context: false,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            path: None,
        }
    }

    /// The `Arc` only "covers all bases" if the state is `Send + Sync`
    /// (design doc: Send check, plan phase 1).
    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn app_shared_is_send_sync() {
        assert_send_sync::<AppState>();
        assert_send_sync::<State>();
    }

    #[test]
    fn model_target_roundtrip_splits_only_first_colon() {
        let app_state = AppState::for_test(test_settings());
        app_state.set_model("openrouter:tencent/hy3:free");
        assert_eq!(app_state.model(), "openrouter:tencent/hy3:free");
        assert_eq!(app_state.with_config(|c| c.provider.clone()), "openrouter");
        assert_eq!(app_state.with_config(|c| c.model.clone()), "tencent/hy3:free");
    }

    #[test]
    fn control_take_gc_one_shots_but_quit_latches() {
        let app_state = AppState::for_test(test_settings());
        app_state.request_resume("abc".into(), "5t".into());
        app_state.request_clear("fresh".into(), "outgoing-id".into());
        app_state.set_model_commit("test:model".into());
        app_state.request_quit();

        let taken = app_state.take_control();
        assert_eq!(taken.resume.as_ref().unwrap().session_id, "abc");
        assert_eq!(taken.new_session.as_deref(), Some("fresh"));
        assert_eq!(
            taken.delete_session_ids_after,
            vec!["outgoing-id".to_string()]
        );
        assert_eq!(taken.model_commit.as_deref(), Some("test:model"));
        assert!(taken.quit);

        // GC: one-shots gone, quit still latching.
        let taken2 = app_state.take_control();
        assert!(taken2.resume.is_none());
        assert!(taken2.new_session.is_none());
        assert!(taken2.delete_session_ids_after.is_empty());
        assert!(taken2.model_commit.is_none());
        assert!(app_state.should_quit(), "quit is latching — never cleared");
    }

    #[test]
    fn recent_models_lru_touch_is_single_acquisition() {
        let app_state = AppState::for_test(test_settings());
        app_state.seed_recent_model("a:m1");
        app_state.seed_recent_model("a:m2");
        assert_eq!(app_state.recent_models(), vec!["a:m1", "a:m2"]);
        let updated = app_state.note_recent_model("a:m1");
        assert_eq!(updated, vec!["a:m1", "a:m2"]);
        // Duplicate ids are deduped (no second entry).
        app_state.seed_recent_model("a:m1");
        assert_eq!(app_state.recent_models(), vec!["a:m1", "a:m2"]);
    }

    #[test]
    fn cycle_view_updates_together() {
        let app_state = AppState::for_test(test_settings());
        app_state.update_cycle_view(
            "sess-1".into(),
            SessionUsage::default(),
            42_000,
            Arc::new(vec![]),
            Some("leaf-1".into()),
        );
        assert_eq!(app_state.current_session_id(), "sess-1");
        assert_eq!(app_state.context_tokens(), 42_000);
        assert_eq!(app_state.leaf_id().as_deref(), Some("leaf-1"));
    }

    #[test]
    fn clear_composes_new_plus_delete() {
        let app_state = AppState::for_test(test_settings());
        app_state.request_clear("".into(), "old".into());
        let taken = app_state.take_control();
        assert_eq!(taken.new_session.as_deref(), Some(""));
        assert_eq!(taken.delete_session_ids_after, vec!["old".to_string()]);
    }
}
