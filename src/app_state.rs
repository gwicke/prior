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

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::agent::CodeAgent;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub(crate) agent: Option<Arc<CodeAgent>>,
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
    /// Bumped on every settings write, so `resolved_models` can tell a stale
    /// overlay from a current one. Over-invalidates deliberately: any setting
    /// change rebuilds the list, which is cheaper than proving the change was
    /// not a per-model record.
    pub(crate) models_revision: u64,
    /// Memo for `AppState::resolved_models`, keyed by
    /// `(catalog generation, models_revision)`.
    pub(crate) resolved_cache: Option<(u64, u64, Arc<Vec<crate::model_catalog::ModelEntry>>)>,
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
    /// The session's user-assigned name, or `None` when unnamed — the status
    /// bar's leading label (4h-g4 part 2). Written once per cycle by the loop
    /// off the session index; the bar reads it at render rather than a
    /// per-turn push.
    pub(crate) session_name: Option<String>,
    /// The session's opening user message — the bar's label when the session
    /// has no name. Written per cycle off the branch, and filled in by the
    /// submit path on the first prompt (before the message is persisted).
    pub(crate) session_first_message: Option<String>,
    /// Declarative control targets (see [`ControlConfig`]).
    pub(crate) control: ControlConfig,
    /// One-shot prompt prefill (resume's `/model …` model decision, the
    /// auto-resume seed, the typed compact-cancel restore): written by
    /// handlers/appliers/the loop, **taken by the loop** right before
    /// the next `read_input` (sole consumer).
    pub(crate) prefill: Option<String>,
    /// Key events captured by a window pump while nobody could read stdin
    /// (the compaction LLM window), parked for `read_input` to dispatch on
    /// its next iteration — never dropped, never leaked into `$EDITOR`.
    /// Converted to [`InputEvent`] at the park edge (step 4a): the drain
    /// speaks the input channel's vocabulary; events the prompt could never
    /// show (mouse, focus) are dropped here, as the drain used to ignore them.
    ///
    /// [`InputEvent`]: crate::termio::InputEvent
    pub(crate) parked_keys: VecDeque<crate::termio::InputEvent>,
}

// ---------------------------------------------------------------------------
// AppState — the injected handle
// ---------------------------------------------------------------------------

/// The single injected handle — `MenuShared`'s replacement. Nodes hold
/// `Arc<AppState>`; the REPL loop clones the same `Arc`.
pub struct AppState {
    state: Mutex<State>,
}

// ---------------------------------------------------------------------------
// Window key pump — the input side of an LLM window nobody else can read
// ---------------------------------------------------------------------------

/// Who feeds a key pump while an LLM operation runs *outside* a turn — the
/// compaction summarization, whose await spans requests and back-offs with
/// the REPL loop suspended inside `settle` (no `read_input`, no in-flight
/// reader). Implemented by [`AppState`] (polls stdin — nobody else can while
/// the window lasts — and parks what it cannot act on) and by tests' fakes.
///
/// A root `Esc` is *not* dispatched through the session here: the engine is
/// mid-`prepare()` and cannot evaluate a key, and the node's own two-phase
/// contract makes `Esc` mean "abort" the moment `prepare()` returns — so the
/// pump turns it straight into the cancel signal (the same flag + notify
/// `prompt()`'s dispatcher raises). Everything else is parked for
/// `read_input`, which drains the queue through the normal dispatch.
pub trait WindowKeySource: Sync {
    /// May this window read keys at all? `false` for piped/batch stdin (the
    /// script owns it) and for states with no REPL — those never touch
    /// stdin.
    fn window_active(&self) -> bool;
    /// The next event ready right now — must not block.
    fn poll_key(&self) -> Option<crossterm::event::Event>;
    /// Park an event the window cannot act on for later dispatch.
    fn defer_event(&self, ev: crossterm::event::Event);
}

impl WindowKeySource for AppState {
    fn window_active(&self) -> bool {
        self.window_pump_active()
    }
    fn poll_key(&self) -> Option<crossterm::event::Event> {
        // Belt and braces: `window_active` is the caller's gate; this keeps
        // a stray live reference in piped mode off stdin entirely.
        if !self.window_pump_active() {
            return None;
        }
        match crossterm::event::poll(std::time::Duration::ZERO) {
            Ok(true) => crossterm::event::read().ok(),
            _ => None,
        }
    }
    fn defer_event(&self, ev: crossterm::event::Event) {
        self.park_key(ev);
    }
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
                models_revision: 0,
                resolved_cache: None,
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
                session_name: None,
                session_first_message: None,
                control: ControlConfig::default(),
                prefill: None,
                parked_keys: VecDeque::new(),
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

    /// The live params of `id` as of its last visit this session, or `None` if
    /// it has never been activated. What `save` writes from — see
    /// [`Self::activate_model_params`].
    pub fn model_param(&self, id: &str) -> Option<crate::params::ModelParams> {
        self.state.lock().model_param_map.get(id).copied()
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
        let seeded = Self::seeded_params(&st, id);
        Self::write_live_params(&mut st, id, seeded);
    }

    /// The reasoning level `id` would run at if it were activated right now,
    /// without writing anything.
    ///
    /// The read-only half of [`Self::activate_model_params`]: both sides
    /// resolve through [`Self::seeded_params`], so a preview can never drift
    /// from the activation it is previewing. Asked wherever the answer must be
    /// known *before* committing — the `activate` header ("omitted keeps this
    /// level") and the resume prefill, which only carries a level when
    /// activating would not produce that level by itself.
    pub fn peek_model_reasoning(&self, id: &str) -> Option<crate::params::ReasoningLevel> {
        let st = self.state.lock();
        match st.model_param_map.get(id) {
            Some(memory) => memory.reasoning,
            None => Self::seeded_params(&st, id).reasoning,
        }
    }

    /// Load-or-seed params for a model this session has not run yet, per A.6:
    /// each field from its stored record, then the global default, then the
    /// built-in. Per field, so a record that sets only `reasoning` still gets
    /// the provider's own temperature rather than inheriting the outgoing
    /// model's.
    fn seeded_params(st: &State, id: &str) -> crate::params::ModelParams {
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
        crate::params::ModelParams {
            reasoning: stored.reasoning.or(global_reasoning),
            temperature: stored.temperature,
            top_p: stored.top_p,
        }
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

    /// The content handle this state's config carries — the same `Arc` the
    /// loop's applied config holds (clones share it). Setup paths that need
    /// a geometry read but hold no content — the prompt session's cloned
    /// size watch, the header context's width (4f-e-a) — go through here
    /// instead of cloning the whole config.
    pub(crate) fn content(&self) -> Arc<crate::content::Content> {
        self.state.lock().config.content.clone()
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
            st.models_revision += 1;
            // Keep the *target* config's window in step with the record just
            // written. `plan_rebuild` compares the target against the running
            // agent, and the mid-turn status bar reads the window off the agent,
            // so a Ctrl-E of `context_length` only reaches it if the target
            // moves. Resolved for the one model rather than via
            // `resolved_models`, which would deadlock on the lock already held.
            let qualified = st.config.qualified_model_id();
            st.config.context_limit = crate::model_catalog::resolved_entry(
                &qualified,
                st.settings.models.get(&qualified),
            )
            .and_then(|e| e.context_length)
            .filter(|max| *max > 0);
            st.settings.clone()
        };
        snapshot.save().map_err(|e| e.to_string())
    }

    /// The catalog with every stored per-model record applied — the list every
    /// model path should read.
    ///
    /// Memoized on `(catalog generation, models_revision)`, so a fetch or a
    /// settings write invalidates it and nothing else does. The catalog module
    /// stays pure: it cannot reach the store, which is why the composition
    /// lives here, where both sources are already in hand.
    ///
    /// Reading this instead of `catalog_cached()` is what keeps the `/model`
    /// listing, the status bar's context percentage and the `/temperature`
    /// header from quoting OpenRouter's numbers where the user has overrides.
    pub fn resolved_models(&self) -> Arc<Vec<crate::model_catalog::ModelEntry>> {
        let gen = crate::model_catalog::catalog_generation();
        let mut st = self.state.lock();
        if let Some((g, r, list)) = &st.resolved_cache {
            if *g == gen && *r == st.models_revision {
                return Arc::clone(list);
            }
        }
        let entries = crate::model_catalog::overlay(
            crate::model_catalog::catalog_cached(),
            &st.settings.models,
        );
        let list = Arc::new(entries);
        st.resolved_cache = Some((gen, st.models_revision, Arc::clone(&list)));
        list
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
    pub fn agent(&self) -> Option<Arc<CodeAgent>> {
        self.state.lock().session.agent.clone()
    }

    /// Publish a freshly created/rebuilt agent (convergence, lazy first
    /// prompt, compact's Llm fallback — all loop-side).
    pub fn activate_agent(&self, agent: Arc<CodeAgent>) {
        self.state.lock().session.agent = Some(agent);
    }

    // --- display snapshots (loop writes at cycle build; menu reads) ---

    /// One acquisition for the whole per-cycle view so the menu can never see
    /// a torn pair of fields (rule: one acquisition per logical read/write).
    pub fn update_cycle_view(
        &self,
        current_id: String,
        session_name: Option<String>,
        session_first_message: Option<String>,
        tree: Arc<Vec<SessionTreeNode>>,
        leaf_id: Option<String>,
    ) {
        let mut st = self.state.lock();
        st.session.current_id = current_id;
        st.session_name = session_name;
        st.session_first_message = session_first_message;
        st.raw_tree = tree;
        st.leaf_id = leaf_id;
    }

    /// The status bar's leading label (4h-g4 part 2): the session name, else
    /// its opening message. Both come off the per-cycle write
    /// ([`Self::update_cycle_view`]) except on the first prompt, where the
    /// branch is still empty and [`Self::set_first_message_if_unset`] fills the
    /// message in at submit.
    pub fn bar_identity(&self) -> (Option<String>, Option<String>) {
        let st = self.state.lock();
        (st.session_name.clone(), st.session_first_message.clone())
    }

    /// Fill the bar's message label when the cycle view has none (4h-g4 part
    /// 2): on the very first prompt the branch is not yet persisted, so the
    /// loop's cycle write leaves it empty. The submit path knows the text it
    /// is about to send, so it stands in — the `first_user_text(prior).or(input)`
    /// fallback the turn used to carry, relocated to where the input is.
    pub fn set_first_message_if_unset(&self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let mut st = self.state.lock();
        if st.session_first_message.is_none() {
            st.session_first_message = Some(text.to_string());
        }
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

    /// Park a key event captured while nobody could read stdin (a window
    /// pump — see [`WindowKeySource`]). `read_input` drains the queue on
    /// its next iteration through the normal dispatch, so the event lands
    /// exactly as if it had arrived a moment later: same Ctrl-C/Z, same
    /// Resize handling, same dispatcher — and *after* any `$EDITOR` the
    /// window opened, instead of leaking into its stdin.
    pub fn park_key(&self, ev: crossterm::event::Event) {
        if let Some(ev) = crate::termio::InputEvent::from_event(ev) {
            self.state.lock().parked_keys.push_back(ev);
        }
    }

    /// Oldest parked key, if any — one per `read_input` iteration.
    pub fn take_parked_key(&self) -> Option<crate::termio::InputEvent> {
        self.state.lock().parked_keys.pop_front()
    }

    /// Is stdin owned by the interactive REPL? True iff the config's manager
    /// carries the mode guard (interactive tty, non-quiet — the same
    /// published-at-setup fact the term-guard slot used to answer) — a
    /// window pump may read keys only then: piped/batch stdin belongs to the
    /// script, and test states have no guard at all.
    pub fn window_pump_active(&self) -> bool {
        let content = self.with_config(|c| c.content.clone());
        // Bound: the lock guard must drop before `content` (a tail-expression
        // temporary would outlive the borrow).
        let is_tty = content.manager.lock().unwrap().is_tty();
        is_tty
    }

    /// Run the external editor over `current` through the single wrapper
    /// (`crate::editor::run_editor`): reader gate, raw-mode turnover, DSR
    /// re-anchor (doc/editor_cleanup.md §2). The turnover goes through the
    /// config's manager — the front door for suspend/resume (4f-d) — which
    /// reports `Unsupported` when no mode guard was installed (quiet/pipe:
    /// no tty to hand over).
    pub fn edit_text(
        &self,
        current: &str,
        mode: crate::editor::EditorMode,
    ) -> std::io::Result<String> {
        let content = self.with_config(|c| c.content.clone());
        crate::editor::run_editor(&content, current, mode)
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
        let content = AgentConfig::content_for(&output);
        let config = AgentConfig {
            provider: "test".to_string(),
            model: "model".to_string(),
            output,
            content,
            yolo: false,
            confirm_port: None,
            quiet: true,
            params: Default::default(),
            context_limit: None,
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
        // No mode guard on the manager (quiet/pipe): no tty to hand to $EDITOR.
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
            Some("work".into()),
            None,
            Arc::new(vec![]),
            Some("leaf-1".into()),
        );
        assert_eq!(app_state.current_session_id(), "sess-1");
        assert_eq!(app_state.bar_identity(), (Some("work".to_string()), None));
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

    /// `peek_model_reasoning` is a *preview* of `activate_model_params`, so the
    /// two must agree in every case the preview is asked about: a first visit
    /// (the stored record), a re-visit (the session memory), and a model with
    /// no record at all (the global). The resume prefill decides whether to
    /// spell out a level by asking exactly this, and a preview that disagreed
    /// with the activation would prefill a level the activation then overwrites.
    #[test]
    fn peeking_a_level_matches_what_activation_installs() {
        let app = seeded_app(crate::model_settings::ModelSettings {
            reasoning: Some(ReasoningLevel::Med),
            ..Default::default()
        });
        // First visit: the record.
        assert_eq!(
            app.peek_model_reasoning("test:model"),
            Some(ReasoningLevel::Med)
        );
        app.activate_model_params("test:model");
        assert_eq!(
            app.peek_model_reasoning("test:model"),
            app.params().reasoning
        );
        // Re-visit: the runtime value, which the record no longer describes.
        app.set_reasoning(Some(ReasoningLevel::Max));
        assert_eq!(
            app.peek_model_reasoning("test:model"),
            Some(ReasoningLevel::Max),
            "a visited model previews its runtime level, not its record"
        );
        // A model with no record previews the global default.
        assert_eq!(
            app.peek_model_reasoning("test:other"),
            Some(ReasoningLevel::Low)
        );
        // Peeking writes nothing.
        assert!(!app.state.lock().model_param_map.contains_key("test:other"));
    }
}
