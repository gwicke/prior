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
//!    races, not lost updates split across critical sections. No sync
//!    file IO under the lock either: `set_setting` applies in-lock,
//!    clones, and writes outside.
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

use std::collections::HashMap;
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

/// Compaction mode for the `/tree … compact` menu: names the summary
/// flavor `agent::generate_compaction` produces; the node's `prepare()`
/// opens the generated draft in `$EDITOR` before the engine confirm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactMode {
    /// User-editable touched-files list.
    Manual,
    /// Deterministic text extraction.
    TextOnly,
    /// LLM-generated summary; `payload` is the custom prompt.
    Prompt,
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

    /// `/sandbox` mutations (applier merges + re-publishes).
    pub sandbox: Option<SandboxOp>,
}

impl ControlConfig {
    /// Clone out every one-shot field and clear it (sole-consumer GC).
    /// `quit` is latching and untouched.
    fn take_one_shots(&mut self) -> ControlConfig {
        ControlConfig {
            quit: self.quit,

            sandbox: self.sandbox.take(),
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
    /// Per-model live params, session-scoped: id -> the params that model was
    /// last running with (doc/model_settings_v2.md A.6).
    ///
    /// This is what makes a -> b -> a restore a's *runtime* values rather than
    /// its stored ones. It is the session memory; `config.params` is the live
    /// value for the current model, and the invariant
    /// `model_param_map[current_id] == config.params` holds at all times
    /// because every write goes through [`Self::write_live_params`].
    ///
    /// Session-scoped on purpose: a fresh process re-seeds from the stored
    /// record, so a value changed at runtime is forgotten by a restart, while
    /// one written by `save` survives.
    pub(crate) model_param_map: HashMap<String, crate::params::ModelParams>,
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
    /// One-shot prompt prefill (resume's `/model …` model decision, the
    /// auto-resume seed, the typed compact-cancel restore): written by
    /// handlers/appliers/the loop, **taken by the loop** right before
    /// the next `read_input` (sole consumer).
    pub(crate) prefill: Option<String>,
    /// REPL raw-mode guard — a service handle published once at startup;
    /// the compaction review closure clones it out to suspend raw mode
    /// around `$EDITOR` (same pattern as the Ctrl-E path).
    pub(crate) term_guard: Option<Arc<crate::repl::TermGuard>>,
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
                // Empty on purpose. `config.params` arrives seeded from the
                // global alone, so the caller's first
                // `activate_model_params` is what consults the per-model
                // record — pre-seeding here would make it find its own entry
                // and never reach the store. The invariant
                // (`model_param_map[current] == config.params`) is established
                // by that first call, or by the first `set_param`; nothing
                // reads the map before one of them.
                model_param_map: HashMap::new(),
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
                prefill: None,
                term_guard: None,
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

    /// Canonical reasoning level of the target model.
    /// The live request params of the target model.
    pub fn params(&self) -> crate::params::ModelParams {
        self.state.lock().config.params
    }

    /// The target model's reasoning level as a string, for the menu's `*`
    /// marker and the session layer.
    pub fn reasoning(&self) -> Option<String> {
        self.state.lock().config.params.reasoning_str()
    }

    /// Write one live param. A `None` clears it back to "leave the provider
    /// alone" (doc/model_settings_v2.md A.5).
    pub fn set_param(&self, set: impl FnOnce(&mut crate::params::ModelParams)) {
        let mut st = self.state.lock();
        set(&mut st.config.params);
        let id = st.config.qualified_model_id();
        let params = st.config.params;
        st.model_param_map.insert(id, params);
    }

    pub fn set_reasoning(&self, level: Option<crate::params::ReasoningLevel>) {
        self.set_param(|p| p.reasoning = level);
    }

    /// Make `id` the current model and give it its live params.
    ///
    /// Load-or-seed, per field, never per record (doc/model_settings_v2.md A.6):
    ///
    /// ```text
    /// model_param_map[id] exists?  -> use it (a -> b -> a)
    /// else per field f:
    ///     stored[id].f              -> use it
    ///     else global default       -> reasoning only: default_reasoning_level
    ///     else built-in             -> reasoning: off; temperature/top_p: None
    /// ```
    ///
    /// Per field, so a record that sets only `reasoning` still gets the
    /// provider's own temperature rather than inheriting the outgoing model's.
    /// The built-in is applied to *every* newly loaded model on purpose:
    /// switching from a model the user set `temperature = 0.2` on to one with
    /// nothing on file gives the second `None`, not an inherited `0.2`.
    ///
    /// Call this on activation, **not** from [`Self::set_model`]:
    /// `/set default_model` shares that writer but is not an activation, and
    /// seeding there would install the default model's record as the live
    /// params of whatever is actually running.
    pub fn activate_model_params(&self, id: &str) {
        if id.is_empty() {
            return;
        }
        let mut st = self.state.lock();
        // First visit this session: the runtime values win, per A.6. A record
        // edited while the model sits in the map is deliberately NOT re-read.
        if let Some(memory) = st.model_param_map.get(id).copied() {
            Self::write_live_params(&mut st, id, memory);
            return;
        }
        let stored = st.settings.model_settings(id).cloned().unwrap_or_default();
        // `value()` returns the stored string, defaulting to "off" when the
        // key was never written.
        let global_reasoning: Option<crate::params::ReasoningLevel> = st
            .settings
            .value(crate::settings::SettingKey::DefaultReasoning)
            .parse()
            .ok();
        // Resolved verbatim, not normalized: `/reasoning off` itself writes
        // `Some(Off)`, so folding that to `None` here would make the seeded
        // value differ from the one the menu produces, and `params_changed`
        // would fire a rebuild for a value the user never touched.
        let seeded = crate::params::ModelParams {
            reasoning: stored.reasoning.or(global_reasoning),
            temperature: stored.temperature,
            top_p: stored.top_p,
        };
        Self::write_live_params(&mut st, id, seeded);
    }

    /// The single writer of the live params: sets `config.params` and records
    /// the same value under the current model, so
    /// `model_param_map[current_id] == config.params` cannot drift.
    ///
    /// `id` must be the model the params belong to — normally the current one.
    /// Activation passes the incoming model, which is not yet the current one
    /// until the caller has set it; see [`Self::activate_model_params`].
    fn write_live_params(st: &mut State, id: &str, params: crate::params::ModelParams) {
        st.config.params = params;
        st.model_param_map.insert(id.to_string(), params);
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

    /// Mutate the settings store and persist the result.
    ///
    /// The mutation runs under the lock; the TOML write runs **outside** it —
    /// the store is cloned out first, exactly as `set_setting` does, so no file
    /// IO ever happens under the coarse lock. This is the path for any
    /// settings change that needs `&mut SettingsStore` — a per-model record,
    /// rather than one scalar.
    pub fn update_settings(
        &self,
        f: impl FnOnce(&mut SettingsStore) -> Result<(), String>,
    ) -> Result<(), String> {
        let snapshot = {
            let mut st = self.state.lock();
            f(&mut st.settings)?;
            st.settings.clone()
        };
        snapshot.save().map_err(|e| e.to_string())
    }

    /// Mutate + persist a setting. Validate + in-memory apply under the
    /// lock; the TOML write runs **outside** it — the coarse lock is
    /// never held across file IO (an async facade was considered and
    /// dropped as low value: this is a tiny, user-paced write and the
    /// frame waits for it either way).
    pub fn set_setting(&self, key: SettingKey, value: &str) -> Result<(), String> {
        let snapshot = {
            let mut st = self.state.lock();
            st.settings.apply_value(key, value)?;
            st.settings.clone()
        };
        snapshot.save().map_err(|e| e.to_string())
    }

    /// Snapshot of the settings store for read-only use across `.await`s
    /// (helpers taking `&SettingsStore`): clone out under the lock, then
    /// await against the private copy (rule 3).
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

    pub fn request_sandbox(&self, op: SandboxOp) {
        self.state.lock().control.sandbox = Some(op);
    }

    /// Sole-consumer drain: clone out + GC every one-shot control field in a
    /// single acquisition (`quit` latches). Run by the loop's convergence
    /// step every cycle — a missed cycle only delays convergence.
    pub fn take_control(&self) -> ControlConfig {
        self.state.lock().control.take_one_shots()
    }

    /// Write the one-shot prompt prefill (handlers/appliers/the loop).
    pub fn set_prefill(&self, prefill: String) {
        self.state.lock().prefill = Some(prefill);
    }

    /// Sole-consumer take: the loop pops the prefill right before the
    /// next `read_input`.
    pub fn take_prefill(&self) -> Option<String> {
        self.state.lock().prefill.take()
    }

    /// Publish the REPL's term guard (set once at startup).
    pub fn set_term_guard(&self, guard: Arc<crate::repl::TermGuard>) {
        self.state.lock().term_guard = Some(guard);
    }

    /// Clone-out of the term guard (rule 3 — callers hold it across the
    /// editor outside any lock; `edit_text` does exactly that).
    pub fn term_guard(&self) -> Option<Arc<crate::repl::TermGuard>> {
        self.state.lock().term_guard.clone()
    }

    /// Run the external editor over `current` through the single wrapper
    /// (`crate::editor::run_editor`): reader gate, raw-mode turnover, DSR
    /// re-anchor (doc/editor_cleanup.md §2). Requires an interactive term
    /// guard — without one (quiet/pipe) there is no tty to hand over, so
    /// this reports `Unsupported`.
    pub fn edit_text(
        &self,
        current: &str,
        mode: crate::editor::EditorMode,
    ) -> std::io::Result<String> {
        let guard = self.term_guard().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "editor requires an interactive terminal",
            )
        })?;
        let output = self.with_config(|c| c.output.clone());
        crate::editor::run_editor(&guard, output.as_ref(), current, mode)
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
            params: Default::default(),
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
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            message_log_path: None,
            sessions_dir: None,
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
            default_reasoning_level: None,
            models: Default::default(),
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
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
        assert_eq!(
            app_state.with_config(|c| c.model.clone()),
            "tencent/hy3:free"
        );
    }

    #[test]
    fn edit_text_requires_an_interactive_guard() {
        let app_state = AppState::for_test(test_settings());
        // No term guard (quiet/pipe): there is no tty to hand to $EDITOR.
        let err = app_state
            .edit_text("draft", crate::editor::EditorMode::Edit)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    fn control_take_gc_one_shots_but_quit_latches() {
        let app_state = AppState::for_test(test_settings());
        app_state.request_sandbox(SandboxOp::Remove {
            sandbox_path: "/x".into(),
        });
        app_state.request_quit();

        let taken = app_state.take_control();
        assert!(matches!(
            taken.sandbox,
            Some(SandboxOp::Remove { ref sandbox_path }) if sandbox_path == "/x"
        ));
        assert!(taken.quit);

        // GC: one-shots gone, quit still latching.
        let taken2 = app_state.take_control();
        assert!(taken2.sandbox.is_none());
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

    // ---- per-model live-param seeding (doc/model_settings_v2.md A.6) ----

    use crate::params::ReasoningLevel;

    /// A store with a per-model record, and a global to prove the record wins.
    fn store_with_model_record(record: crate::model_settings::ModelSettings) -> SettingsStore {
        let mut s = test_settings();
        s.default_reasoning_level = Some("low".to_string());
        s.models.insert("test:model".to_string(), record);
        s
    }

    fn seeded_app(record: crate::model_settings::ModelSettings) -> Arc<AppState> {
        AppState::for_test(store_with_model_record(record))
    }

    /// The reported bug: `save` wrote `reasoning = "max"`, but a new session
    /// started at the global's level because nothing ever read the record.
    #[test]
    fn a_stored_reasoning_level_seeds_a_new_session() {
        let app = seeded_app(crate::model_settings::ModelSettings {
            reasoning: Some(ReasoningLevel::Max),
            ..Default::default()
        });
        app.activate_model_params("test:model");
        assert_eq!(app.params().reasoning, Some(ReasoningLevel::Max));
    }

    /// The global is the fallback, not the answer: a model with no record
    /// takes the global.
    #[test]
    fn a_model_with_no_record_falls_back_to_the_global() {
        let app = seeded_app(crate::model_settings::ModelSettings::default());
        app.activate_model_params("test:model");
        assert_eq!(app.params().reasoning, Some(ReasoningLevel::Low));
    }

    /// Per field, never per record: a record that sets only `reasoning` must
    /// still get the provider's own temperature, not the outgoing model's.
    #[test]
    fn seeding_is_per_field_not_per_record() {
        // The record sets reasoning and nothing else.
        let app = seeded_app(crate::model_settings::ModelSettings {
            reasoning: Some(ReasoningLevel::High),
            ..Default::default()
        });
        app.activate_model_params("test:model");
        let p = app.params();
        assert_eq!(p.reasoning, Some(ReasoningLevel::High));
        assert_eq!(p.temperature, None);
        assert_eq!(p.top_p, None);
    }

    /// a -> b -> a restores a's *runtime* values, not its stored ones. This is
    /// the case the session map exists for: seed from the record, change at
    /// runtime, switch away, switch back.
    #[test]
    fn a_b_a_restores_runtime_values() {
        let app = seeded_app(crate::model_settings::ModelSettings {
            reasoning: Some(ReasoningLevel::Med),
            ..Default::default()
        });
        app.activate_model_params("test:model");
        assert_eq!(app.params().reasoning, Some(ReasoningLevel::Med));
        // The user changes it at runtime.
        app.set_reasoning(Some(ReasoningLevel::High));
        // b: another model, seeded from the global.
        app.set_model("test:other");
        app.activate_model_params("test:other");
        assert_eq!(app.params().reasoning, Some(ReasoningLevel::Low));
        // Back to a: the runtime `high` wins over the record's `med`.
        app.set_model("test:model");
        app.activate_model_params("test:model");
        assert_eq!(app.params().reasoning, Some(ReasoningLevel::High));
    }

    /// The built-in applies to *every* newly loaded model, so temperature does
    /// not follow the user from one model to the next (A.6).
    #[test]
    fn temperature_does_not_leak_across_a_switch() {
        let app = seeded_app(crate::model_settings::ModelSettings::default());
        app.activate_model_params("test:model");
        app.set_param(|p| p.temperature = Some(0.2));
        assert_eq!(app.params().temperature, Some(0.2));
        app.set_model("test:other");
        app.activate_model_params("test:other");
        assert_eq!(
            app.params().temperature,
            None,
            "the outgoing model's 0.2 must not carry over"
        );
    }

    /// After a→b→a, the live values win over a record edited in between —
    /// by design: the record is only consulted on a first visit.
    #[test]
    fn a_record_edited_after_a_visit_does_not_displace_the_live_values() {
        let mut store = store_with_model_record(crate::model_settings::ModelSettings {
            reasoning: Some(ReasoningLevel::Med),
            ..Default::default()
        });
        let app = AppState::for_test(store.clone());
        app.activate_model_params("test:model");
        app.set_reasoning(Some(ReasoningLevel::High));
        // Edit the record underneath the visited model.
        store.models.insert(
            "test:model".to_string(),
            crate::model_settings::ModelSettings {
                reasoning: Some(ReasoningLevel::Low),
                ..Default::default()
            },
        );
        app.set_model("test:other");
        app.activate_model_params("test:other");
        app.set_model("test:model");
        app.activate_model_params("test:model");
        assert_eq!(app.params().reasoning, Some(ReasoningLevel::High));
    }

    /// The invariant the whole design rests on: every write records the same
    /// value under the current model. A drift here is what made the first cut
    /// lose a->b->a.
    #[test]
    fn the_map_and_the_live_params_never_diverge() {
        let app = seeded_app(crate::model_settings::ModelSettings::default());
        app.activate_model_params("test:model");
        app.set_param(|p| {
            p.temperature = Some(0.7);
            p.top_p = Some(0.8);
        });
        app.set_reasoning(Some(ReasoningLevel::Max));
        let (live, remembered) = {
            let st = app.state.lock();
            (
                st.config.params,
                st.model_param_map.get("test:model").copied(),
            )
        };
        assert_eq!(Some(live), remembered);
        // And a menu write to reasoning goes through the same path.
        let remembered = app.state.lock().model_param_map["test:model"].reasoning;
        assert_eq!(Some(ReasoningLevel::Max), remembered);
    }

    /// A model id with no provider prefix is not activatable; seeding must not
    /// scribble an entry for it.
    #[test]
    fn seeding_ignores_an_empty_model_id() {
        let app = seeded_app(crate::model_settings::ModelSettings::default());
        app.activate_model_params("");
        assert!(app.state.lock().model_param_map.is_empty());
    }
}
