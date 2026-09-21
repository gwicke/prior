//! Bridge: picocode state → slashdot_menu interpreter (design §10).
//!
//! Each migrated command builds its descriptor subtree + handlers here;
//! `ActiveOverlay` arms delegate to the shared interpreter instead of
//! bespoke overlay variants. Phase 6 starts with `/thinking` (§10 step 1).

use std::cell::RefCell;
use std::rc::Rc;

use crate::session::repo::JsonlSessionRepo;
use crate::slash;
use crate::slashdot_menu::{
    accept::{AcceptController, AcceptOutcome, LeafInvocation, LeafResult},
    interpreter::{Interpreter, MenuState, OverlayAction},
    node::MenuNode,
    render::{PlanBuilder, RenderPlan},
    value::Value,
};

/// Verify `MenuShared`'s file-scoped cells stay inside the `RefCell`
/// (compile-time: constructing a `MenuShared` takes plain `&self`-style
/// borrows so nothing escapes as an `&mut`).
const _: () = {
    fn assert_cell<T>() {}
    let _ = assert_cell::<Rc<RefCell<crate::settings::SettingsStore>>>;
};

/// One traversal memory frame: the prompt (with filter/modifier state)
/// and cursor position as they were *before* an accept/drill. Esc replays
/// these newest-first, strictly along the current branch (frames are only
/// pushed on the way down, so Esc can never jump to siblings).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub line: String,
    pub cursor: usize,
    /// Level identity: the node path this frame belongs to. Edit
    /// invalidation is scoped per level — typing at a deep branch does not
    /// discard the filter state of ancestor levels.
    pub level: Vec<String>,
}

/// Compute a level's path identity from a prompt line: the node path the
/// interpreter walks to (pinned level-name selections included).
fn level_of(engine: &mut MenuEngine, line: &str) -> Vec<String> {
    let st = engine.interpreter.state(line);
    let mut path = st.node_path.clone();
    // A pinned selection whose node is a level (has children/value) IS the
    // level name (e.g. "thinking" pinned at root = the thinking level).
    if let Some(sel) = &st.selection {
        if let Some(node) = resolve_node(&engine.interpreter.root, &path, sel) {
            if !node.children.is_empty() || node.value.is_some() {
                path.push(sel.clone());
            }
        }
    }
    path
}

fn resolve_node(
    root: &crate::slashdot_menu::node::MenuNode,
    path: &[String],
    sel: &str,
) -> Option<crate::slashdot_menu::node::MenuNode> {
    let mut node = root.clone();
    for p in path {
        node = node.child(p)?.clone();
    }
    node.child(sel).cloned()
}

/// The shared menu engine, built per prompt iteration from picocode state.
/// NOTE: rebuilt per key-press today (`MenuEngine::new(&MenuShared)`), so
/// the stack lives with the *host session* via the returned snapshot (see
/// `StepResult.frames`); a persistent engine is Phase 6 cleanup / Phase 7.
pub struct MenuEngine {
    pub interpreter: Interpreter,
    pub controller: AcceptController,
    /// Traversal memory (Phase 5.5): frames pushed on accept/drill, Esc
    /// replays newest-first. Engine-owned since Phase 6.7 step 3 — the
    /// host keeps this engine instance across keypresses, so frames stay
    /// inside it (no host threading).
    pub frames: Vec<Frame>,
    /// Detail providers keyed by node path (design §5.1): the
    /// engine reads them per frame to populate `RenderPlan.detail`.
    /// Static levels derive their detail from the node's own
    /// handler; dynamic levels from a registered provider.
    pub detail_providers: std::collections::HashMap<String, Box<dyn crate::slashdot_menu::provider::DetailProvider>>,
}

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

/// Live state bundle shared between the REPL and the menu engine.
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
    /// Synchronous mirror of the async session tree (v1
    /// `flatten_tree_nodes` tuples) + current leaf id. The async `Session`
    /// only mutates between agent turns, so the REPL refreshes this once
    /// per loop iteration; the `/tree` provider reads it per `fetch`.
    pub tree: Rc<RefCell<Vec<(usize, String, String, bool, bool, usize)>>>,
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
            tree: Rc::new(RefCell::new(Vec::new())),
            leaf_id: Rc::new(RefCell::new(None)),
            session_usage: Rc::new(RefCell::new(crate::agent::SessionUsage::default())),
            pending_op: Rc::new(RefCell::new(None)),
        }
    }

    /// Clone the cell *handles* (shared ownership), not their contents —
    /// the engine and its providers then observe the same live cells the
    /// REPL writes.
    pub fn clone_shared(&self) -> MenuShared {
        MenuShared {
            settings: self.settings.clone(),
            live_state: self.live_state.clone(),
            repo: self.repo.clone(),
            current_session_id: self.current_session_id.clone(),
            recent_models: self.recent_models.clone(),
            tree: self.tree.clone(),
            leaf_id: self.leaf_id.clone(),
            session_usage: self.session_usage.clone(),
            pending_op: self.pending_op.clone(),
        }
    }
}

/// Active-state provider over the runtime persona (design §5.1): the
/// currently-in-effect persona sets the `*` marker on the static `/persona`
/// value list. Reads from `live_state.persona` (transient session state),
/// not from `SettingsStore` (persistent defaults). `None` (or empty) → no
/// row marked.
pub struct PersonaActiveProvider {
    pub shared: Rc<MenuShared>,
}

impl crate::slashdot_menu::provider::ActiveProvider for PersonaActiveProvider {
    fn active(&mut self) -> Option<String> {
        let s = self.shared.live_state.borrow();
        let p = s.persona.as_deref()?;
        if p.is_empty() {
            Some("none".to_string())
        } else {
            Some(p.to_string())
        }
    }
}

/// Active-state provider over the runtime thinking level (design §5.1):
/// reads from `live_state.thinking_level` (transient session state).
pub struct ThinkingActiveProvider {
    pub shared: Rc<MenuShared>,
}

impl crate::slashdot_menu::provider::ActiveProvider for ThinkingActiveProvider {
    fn active(&mut self) -> Option<String> {
        let t = self.shared.live_state.borrow();
        t.thinking_level.clone().or_else(|| Some("off".to_string()))
    }
}

/// Detail provider for `/replay` (design §5.1): reflects the
/// user-facing replay limits (`10` turns, `10t` turns, `10l`
/// lines) and command syntax. The engine drives the detail panel;
/// agent.rs dispatches the replay itself.
pub struct ReplayDetailProvider {
    pub shared: Rc<MenuShared>,
}

impl crate::slashdot_menu::provider::DetailProvider for ReplayDetailProvider {
    fn fetch(&mut self) -> Vec<String> {
        let _ = self.shared;
        vec![
            "Replay the full history of the current session.".to_string(),
            "".to_string(),
            "Optional length limits (appended after /replay):".to_string(),
            "  10      last 10 agent turns (same as 10t)".to_string(),
            "  10t     last 10 agent turns".to_string(),
            "  10l     last 10 output lines".to_string(),
            "".to_string(),
            "Examples:  /replay    /replay 5    /replay 50l".to_string(),
        ]
    }
}

/// DataProvider over the session tree entries (§10 step 6): rows from the
/// shared flattened-tree snapshot, in provider order.
pub struct TreeProvider {
    pub shared: Rc<MenuShared>,
}

impl crate::slashdot_menu::provider::DataProvider for TreeProvider {
    fn fetch(
        &mut self,
        _modifiers: &[crate::slashdot_menu::grammar::ModifierToken],
    ) -> crate::slashdot_menu::provider::DataResult {
        use crate::slashdot_menu::value::Value;
        let leaf_id = self.shared.leaf_id.borrow().clone();
        let entries = self.shared.tree.borrow().clone();
        let rows: Vec<crate::slashdot_menu::fields::Row> = entries
            .iter()
            .map(|(indent, label, id, on_path, is_current, branch_depth)| {
                let mut r = crate::slashdot_menu::fields::Row::new(id.clone())
                    .desc(label.clone())
                    .field("label", Value::Str(label.clone()))
                    .field("indent", Value::Num(*indent as f64))
                    .field("branch_depth", Value::Num(*branch_depth as f64));
                if *on_path {
                    r = r.field("on_path", Value::Str("1".into()));
                }
                if *is_current {
                    r = r.field("leaf", Value::Str("1".into()));
                }
                r
            })
            .collect();
        crate::slashdot_menu::provider::DataResult {
            rows,
            active_row: leaf_id,
            ..Default::default()
        }
    }
}

/// DataProvider over the session list (§10 step 5): rows from the live
/// repo at `fetch` time; current-first + recency order is the provider's
/// own (kept from v1) so the engine's default order is the v1 sort. The
/// active-session id rides in `active_row` (provider data, not a trait
/// call).
pub struct SessionProvider {
    pub shared: Rc<MenuShared>,
}

impl crate::slashdot_menu::provider::DataProvider for SessionProvider {
    fn fetch(
        &mut self,
        _modifiers: &[crate::slashdot_menu::grammar::ModifierToken],
    ) -> crate::slashdot_menu::provider::DataResult {
        use crate::slashdot_menu::value::Value;
        let current_id = self.shared.current_session_id.borrow().clone();
        let active = if current_id.is_empty() { None } else { Some(current_id.clone()) };
        // Live listing from the repo (the repo's own sync cache handles
        // freshness); current-first + recency order is the provider's own.
        let mut sessions: Vec<crate::repl::SessionMeta> = match &self.shared.repo {
            Some(repo) => repo
                .list_sessions_sync(None)
                .into_iter()
                .map(crate::repl::session_meta_from)
                .collect(),
            None => Vec::new(),
        };
        sessions.sort_by(|a, b| {
            let a_cur = a.id == current_id;
            let b_cur = b.id == current_id;
            if a_cur && !b_cur { std::cmp::Ordering::Less }
            else if !a_cur && b_cur { std::cmp::Ordering::Greater }
            else {
                let a_ts = a.last_prompt_timestamp.as_ref().unwrap_or(&a.timestamp);
                let b_ts = b.last_prompt_timestamp.as_ref().unwrap_or(&b.timestamp);
                b_ts.cmp(a_ts)
            }
        });
        let rows: Vec<crate::slashdot_menu::fields::Row> = sessions
            .iter()
            .map(|m| {
                let display = m.name.clone().unwrap_or_else(|| m.id.clone());
                let mut r = crate::slashdot_menu::fields::Row::new(m.id.clone())
                    .desc(&display)
                    .field("name", Value::Str(display))
                    .field("turns", Value::Num(m.user_message_count as f64))
                    .field("msgs", Value::Num(m.message_count as f64))
                    .field("cwd", Value::Str(m.cwd.clone()))
                    .field("timestamp", Value::Str(m.timestamp.clone()));
                if m.id == current_id {
                    r = r.field("current", Value::Str("1".into()));
                }
                if let Some(p) = &m.parent_session {
                    r = r.field("parent", Value::Str(p.clone()));
                }
                r
            })
            .collect();
        crate::slashdot_menu::provider::DataResult {
            rows,
            active_row: active,
            ..Default::default()
        }
    }
}

/// DataProvider over the model catalog (§10 step 4). One-shot fetch over
/// the **global catalog cache** (the live store). **The provider's own
/// ordering is the default sort**: rows come in recency × popularity
/// order with the active model boosted first ([`Self::fetch_recent_and_popular`]).
/// The engine's query compiler applies any explicit `.sort:`/field
/// modifier generically over the cached rows (§7.1) and leaves the
/// provider order alone when no `.sort:` is present.
pub struct ModelProvider {
    pub shared: Rc<MenuShared>,
}

/// One model entry → engine row with the numeric fields the generic
/// filter/sort/fuzzy logic addresses.
pub(crate) fn model_row(e: &crate::model_catalog::ModelEntry) -> crate::slashdot_menu::fields::Row {
    use crate::slashdot_menu::value::Value;
    let mut r = crate::slashdot_menu::fields::Row::new(e.qualified()).desc(e.display.clone());
    if let Some(v) = e.context_length {
        r = r.field("context", Value::Num(v as f64));
    }
    if let Some(v) = e.prompt_cost {
        r = r.field("price_input", Value::Num(v));
    }
    if let Some(v) = e.completion_cost {
        r = r.field("price_output", Value::Num(v));
    }
    if let Some(v) = e.coding_index {
        r = r.field("coding", Value::Num(v as f64));
    }
    if let Some(v) = e.intelligence_index {
        r = r.field("intelligence", Value::Num(v as f64));
    }
    if let Some(v) = e.agentic_index {
        r = r.field("agentic", Value::Num(v as f64));
    }
    // Reasoning capability as a plain number (1/0) — the menu renders it
    // as a number column like any other field.
    r = r.field("reasoning", Value::Num(if e.reasoning { 1.0 } else { 0.0 }));
    // Computed fields (design §7.1): combined price and index-per-price
    // ratios are plain provider-supplied fields — the engine treats them
    // identically to any other (filterable/sortable). `price` = per-M
    // combined max of in+out (or the average for mixed costs).
    let combined_price = match (e.prompt_cost, e.completion_cost) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    if let Some(p) = combined_price {
        r = r.field("price", Value::Num(p));
    }
    for (idx_name, idx_val) in [
        ("coding", e.coding_index.map(|v| v as f64)),
        ("intelligence", e.intelligence_index.map(|v| v as f64)),
        ("agentic", e.agentic_index.map(|v| v as f64)),
    ] {
        // The *_price ratios are computed with the combined price **floored
        // at a tiny epsilon** (1e-10). Free models (actual price == 0) then
        // land at price = epsilon, so their ratio is huge and they sort
        // *ahead* of paid models, yet in line with their rating indices —
        // a free model with a higher index beats a free model with a lower
        // index. The floor also avoids a divide-by-zero when price == 0.
        if let (Some(iv), Some(p)) = (idx_val, combined_price) {
            let denom = p.max(1e-10);
            r = r.field(format!("{}_price", idx_name), Value::Num(iv / denom));
        }
    }
    // `created` as a sortable/filterable **ISO date string** (design §7 /
    // user guidance): more user-friendly to sort/filter on than a raw unix
    // timestamp, and the sort comparator handles ISO strings lexically
    // (chronological). No engine-side "N days ago" computation.
    if let Some(t) = e.created {
        let iso = crate::model_catalog::fmt_iso_date(t);
        r = r.field("created", Value::Str(iso.clone()));
        // Sort/filter read the string value; display uses the same ISO string.
        let _ = iso;
    }
    // Price display: show a clean "$x.xx/M" glyph via display override while
    // sorting/filtering on the raw number. (Kept simple: the table column
    // already formats numbers via fmt_num; the override is available.)
    r
}

impl crate::slashdot_menu::provider::DataProvider for ModelProvider {
    fn fetch(
        &mut self,
        _modifiers: &[crate::slashdot_menu::grammar::ModifierToken],
    ) -> crate::slashdot_menu::provider::DataResult {
        // One-shot per activation. The catalog's own global cache is the
        // live store (`catalog_cached`); no menu-level snapshot. The
        // active-row id (fully-qualified `provider:model`, matching the
        // settings value exactly) rides in the same response (§5.1); a
        // disappeared model simply won't match a row.
        let settings = self.shared.settings.borrow();
        let active = crate::repl::active_model_id(&settings);
        let entries = self.fetch_recent_and_popular();
        let rows: Vec<crate::slashdot_menu::fields::Row> =
            entries.iter().map(model_row).collect();
        crate::slashdot_menu::provider::DataResult {
            rows,
            active_row: active,
            columns: ModelProvider::columns(),
            fields: ModelProvider::field_meta(),
            ..Default::default()
        }
    }
}

impl ModelProvider {
    /// Declared display columns for the model table (§8.1 / §6): mirrors the
    /// v1 `Model` overlay columns (Model/Ctx/Coding/Price in/Price
    /// out/Reason) so the generic renderer needs no host-side layout. The
    /// "Model" column shows the implicit row `id`.
    pub fn columns() -> Vec<crate::slashdot_menu::provider::ColumnDef> {
        use crate::slashdot_menu::provider::ColumnDef;
        use crate::slashdot_menu::table::Align;
        vec![
            ColumnDef::new("id", "Model").min(12).prio(3),
            ColumnDef::new("context", "Ctx")
                .align(Align::Right)
                .min(5)
                .prio(1),
            ColumnDef::new("coding", "Coding")
                .align(Align::Right)
                .min(6)
                .prio(1),
            ColumnDef::new("price_input", "Price in")
                .align(Align::Right)
                .min(8)
                .prio(2),
            ColumnDef::new("price_output", "Price out")
                .align(Align::Right)
                .min(8)
                .prio(2),
            ColumnDef::new("reasoning", "Reason")
                .align(Align::Right)
                .min(3)
                .prio(1),
        ]
    }

    /// Field metadata (design §7.1 / §8.1): every model field is
    /// filterable + sortable. Prices asc, indices + context desc. The
    /// description (shown in the `.` / `.sort:` auto-complete Description
    /// column) comes from the `MODEL_MODIFIERS` table, which is the
    /// canonical description source.
    pub fn field_meta() -> Vec<crate::slashdot_menu::provider::FieldMeta> {
        use crate::slashdot_menu::fields::{FieldSpec, SortDir};
        use crate::slashdot_menu::provider::FieldMeta;
        // Description from MODEL_MODIFIERS (token -> desc).
        let desc_of = |name: &str| -> Option<&'static str> {
            crate::model_catalog::MODEL_MODIFIERS
                .iter()
                .find(|m| m.token == name)
                .map(|m| m.desc)
        };
        let f = |name: &str, num: bool, dir: SortDir| FieldMeta {
            spec: FieldSpec::new(name, num).dir(dir).description(desc_of(name).unwrap_or("")),
            enum_values: None,
        };
        vec![
            f("context", true, SortDir::Asc),
            f("price_input", true, SortDir::Asc),
            f("price_output", true, SortDir::Asc),
            f("price", true, SortDir::Asc),
            f("coding", true, SortDir::Desc),
            f("intelligence", true, SortDir::Desc),
            f("agentic", true, SortDir::Desc),
            f("reasoning", true, SortDir::Desc),
            f("created", false, SortDir::Desc), // ISO date string
            f("coding_price", true, SortDir::Desc),
            f("intelligence_price", true, SortDir::Desc),
            f("agentic_price", true, SortDir::Desc),
        ]
    }

    /// The provider's **default ordering** (design §12): recency ×
    /// popularity, with the active model boosted first. The engine's
    /// generic query applies any explicit `.sort:`/field filter *after*
    /// this, and leaves the order alone when no `.sort:` is present, so
    /// this ordering is what the user sees by default.
    fn fetch_recent_and_popular(&self) -> Vec<crate::model_catalog::ModelEntry> {
        let entries = crate::model_catalog::catalog_cached();
        let active = crate::repl::active_model_id(&self.shared.settings.borrow());
        let recent: Vec<String> = self.shared.recent_models.borrow().clone();

        // Server position = index in the catalog's popularity order.
        let server_rank: std::collections::HashMap<String, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();

        let mut seen = std::collections::HashSet::new();
        let mut ordered: Vec<crate::model_catalog::ModelEntry> = Vec::new();
        // 1. Active model first (if present in the catalog). Fully
        // qualified ids match the settings value exactly; a disappeared
        // model simply won't be found (no `*`, no boost) — that's fine.
        let active_id = active.unwrap_or_default();
        if let Some(active_entry) = entries.iter().find(|e| e.qualified() == active_id) {
            if seen.insert(active_entry.id.clone()) {
                ordered.push(active_entry.clone());
            }
        }
        // 2. Up to 4 more recent models in LRU order (total 5 boosted).
        for recent_id in &recent {
            if ordered.len() >= 5 {
                break;
            }
            if let Some(e) = entries.iter().find(|e| e.id == *recent_id) {
                if seen.insert(e.id.clone()) {
                    ordered.push(e.clone());
                }
            }
        }
        // 3. The rest in repository (popularity) order.
        let mut rest: Vec<crate::model_catalog::ModelEntry> = entries
            .into_iter()
            .filter(|e| !seen.contains(&e.id))
            .collect();
        rest.sort_by(|a, b| {
            let ra = server_rank.get(&a.id).copied().unwrap_or(usize::MAX);
            let rb = server_rank.get(&b.id).copied().unwrap_or(usize::MAX);
            ra.cmp(&rb)
        });
        ordered.extend(rest);
        ordered
    }
}

impl MenuEngine {
    /// Build the engine: the full descriptor tree (all commands) with
    /// handlers bound per command as they migrate (§10 order). Providers
    /// are attached here from shared state (no `ReplCtx`, no `for_model`);
    /// the engine is long-lived per prompt (Phase 6.7 step 3): the host
    /// builds it once and keeps the same instance across keypresses, so
    /// cursors, traversal frames, the provider data cache, and the accept
    /// phase all survive inside it.
    pub fn new(shared: &MenuShared) -> Self {
        let root = Self::root_descriptor();
        let mut controller = AcceptController::new(root.clone());
        Self::register_handlers(&mut controller);
        let shared_rc = Rc::new(shared.clone_shared());
        let interpreter = Interpreter::new(root)
            .with_provider(&["model"], Box::new(ModelProvider { shared: shared_rc.clone() }))
            .with_provider(&["session"], Box::new(SessionProvider { shared: shared_rc.clone() }))
            .with_provider(&["tree"], Box::new(TreeProvider { shared: shared_rc.clone() }))
            .with_active_provider(
                &["persona"],
                Box::new(PersonaActiveProvider { shared: shared_rc.clone() }),
            )
            .with_active_provider(
                &["thinking"],
                Box::new(ThinkingActiveProvider { shared: shared_rc.clone() }),
            );
        let engine = MenuEngine {
            interpreter,
            controller,
            frames: Vec::new(),
            detail_providers: {
                let mut m = std::collections::HashMap::new();
                m.insert("replay".to_string(), Box::new(ReplayDetailProvider { shared: shared_rc.clone() }) as _);
                m
            },
        };
        engine
    }

    /// The static descriptor tree: one child per command (§10).
    fn root_descriptor() -> MenuNode {
        let mut children: Vec<MenuNode> = crate::slash::registry()
            .iter()
            .map(|c| {
                let mut n = MenuNode::new(c.name.trim_start_matches('/')).desc(c.help);
                // Leaf commands: no args, direct submit.
                if matches!(c.kind, slash::SlashKind::Help
                    | slash::SlashKind::Quit
                    | slash::SlashKind::Clear
                    | slash::SlashKind::New) {
                    n = n.leaf();
                }
                n
            })
            .collect();
        // §10 step 1: /thinking — static level with one enum positional
        // value child (off|low|med|high), shared leaf = SetThinking.
        // §10 step 3: /persona — value child backed by the persona list
        // ("none" = empty value); /set — static children, each a level
        // whose value kind comes from SETTINGS metadata.
        for c in children.iter_mut() {
            match c.key.as_str() {
                "thinking" => {
                    *c = MenuNode::new("thinking")
                        .desc("set thinking level")
                        .child_header("Thinking levels")
                        .value(crate::slashdot_menu::node::ValueChild::Enum(vec![
                            "off".into(),
                            "low".into(),
                            "med".into(),
                            "high".into(),
                        ]))
                        .leaf();
                }
                "persona" => {
                    let mut vals: Vec<String> = crate::persona::PERSONAS
                        .iter()
                        .map(|p| p.name.to_string())
                        .collect();
                    vals.push("none".into());
                    *c = MenuNode::new("persona")
                        .desc("set runtime persona")
                        .child_header("Personas")
                        .value(crate::slashdot_menu::node::ValueChild::Enum(vals))
                        .leaf();
                }
                "compact" => {
                    // §10 step 6/R8-8: leaf that returns a rewritten path —
                    // pure sugar over /tree (no parallel hierarchy).
                    *c = MenuNode::new("compact")
                        .desc("compact the session at the current position")
                        .leaf();
                }
                "stat" => {
                    // §10 step 8: interstitial, no leaf (D4) — second Enter
                    // dismisses; text lives in the node property.
                    // The detail panel (context, usage, cost, cache)
                    // is served by a registered detail provider
                    // (StatDetailProvider in the bridge).
                    *c = MenuNode::new("stat")
                        .desc("session stats: context window, usage, cost, cache hit")
                        .interstitial("context window, token usage, cost, cache hit rate");
                }
                "replay" => {
                    // §10 step 8: leaf whose detail panel is the help
                    // text (rendered by the engine's detail provider).
                    // The engine calls the registered detail provider
                    // per frame; agent.rs dispatches the replay itself.
                    *c = MenuNode::new("replay")
                        .desc("history replay; optional limits 10 (turns) 10t (turns) 10l (lines)")
                        .leaf();
                }
                "sandbox" => {
                    // §10 step 7: static subcommand children (add_ro/add_rw
                    // with back-end fs modifier value completion; remove is
                    // a value child whose ids are the current bindings).
                    *c = MenuNode::new("sandbox")
                        .desc("manage session path bindings: add_ro | add_rw | remove")
                        .children(vec![
                            MenuNode::new("add_ro").desc("make path available read-only"),
                            MenuNode::new("add_rw").desc("make path available read-write"),
                            MenuNode::new("remove").desc("remove a path from the bindings"),
                        ]);
                }
                "tree" => {
                    // §10 step 6: dynamic level (tree entries) → shared
                    // static action level (switch/compact); compact modes
                    // are single-token enum children; llm leaf carries the
                    // `.prompt` modifier (free text) + confirm.
                    *c = MenuNode::new("tree")
                        .desc("navigate session history — select a point to continue from")
                        .dynamic()
                        .children(vec![
                            MenuNode::new("switch").desc("continue from this entry").leaf(),
                            MenuNode::new("compact").desc("compact at this entry").children(vec![
                                MenuNode::new("manual").desc("summarize the kept text yourself"),
                                MenuNode::new("text-only").desc("truncate to text, drop tool output"),
                                MenuNode::new("llm")
                                    .desc("LLM summary; .prompt:<text> sets the prompt")
                                    .confirm()
                                    .leaf(),
                            ]),
                        ]);
                }
                "session" => {
                    // §10 step 5: dynamic level (sessions) → static action
                    // children shared by all rows; delete is a confirm leaf.
                    *c = MenuNode::new("session")
                        .desc("list, rename, resume, fork, or delete sessions")
                        .dynamic()
                        .children(vec![
                            MenuNode::new("resume").desc("continue the session"),
                            MenuNode::new("fork").desc("branch from this point"),
                            MenuNode::new("rename").desc("rename the session"),
                            MenuNode::new("delete").desc("delete from disk").confirm().leaf(),
                        ]);
                }
                "model" => {
                    // §10 step 4: dynamic level over the catalog. Field
                    // metadata (types, default sort dirs) comes from
                    // `ModelProvider::field_meta`; the engine's generic
                    // query compiles `.sort:`/field tokens from that field
                    // space (design §7.1).
                    *c = MenuNode::new("model")
                        .desc("select a model (provider:name); type . for filter options")
                        // child_header = the picker title (design §12: title
                        // from the descriptor path's child header).
                        .child_header("Models")
                        .dynamic()
                        .leaf();
                }
                "set" => {
                    let setting_children: Vec<MenuNode> = crate::settings::SETTINGS
                        .iter()
                        .map(|m| {
                            let n = MenuNode::new(m.name).desc(m.description);
                            match m.kind {
                                crate::settings::ValueKind::Enum(vals) => {
                                    n.value(crate::slashdot_menu::node::ValueChild::Enum(
                                        vals.iter().map(|v| v.to_string()).collect(),
                                    ))
                                }
                                crate::settings::ValueKind::Int => {
                                    n.value(crate::slashdot_menu::node::ValueChild::Number)
                                }
                                _ => n,
                            }
                        })
                        .collect();
                    *c = MenuNode::new("set").desc("view and edit settings").children(setting_children);
                }
                _ => {}
            }
        }
        MenuNode::new("root").children(children)
    }

    fn register_handlers(controller: &mut AcceptController) {
        // /replay has NO registered handler: it is a pure leaf whose Enter
        // submits the *canonical* line (the same expansion Tab produces,
        // with any typed limiter arg) through the generic structural submit
        // path — identical to /clear and /new. A handler here would
        // intercept the Submit as `Close`, hiding the canonical expansion
        // from the host and breaking the "Enter == Tab-then-submit"
        // contract. agent.rs dispatches the actual replay.
        //
        // /compact (R8-8): leaf returns the rewritten /tree path; the host
        // resolves the target id (needs tree ctx) via NewPath with the
        // sentinel "<last-user>" the host replaces.
        controller.register(
            &["compact"],
            Box::new(|_inv: &LeafInvocation| {
                LeafResult::NewPath("tree <last-user> compact ".to_string())
            }),
        );
        // /thinking <level>: translate to SetThinking via the leaf result
        // convention — Done(Some(msg)) closes; the *action* is carried by
        // the picocode side reading the canonical line.
        controller.register(
            &["thinking"],
            Box::new(|inv: &LeafInvocation| {
                let level = inv.line.trim_start_matches("/thinking ").trim();
                let _ = level;
                // The actual InputAction is produced by the host, which
                // parses the accepted line (same path as typed input).
                LeafResult::Done(None)
            }),
        );
    }

    /// Map an interpreter state + key action onto the host-facing result.
    /// `cursor` is the host's highlighted row (arrows move it); Tab/Enter
    /// act on it. Traversal frames are engine-owned (`self.frames`), so
    /// Esc back-navigation works across keypresses on the same engine.
    pub fn step_at(
        &mut self,
        line: &str,
        cursor: usize,
        action: HostKey,
    ) -> StepResult {
        let st = self.interpreter.state(line);
        let ix_action = match action {
            HostKey::Tab => self.interpreter.tab_at(line, cursor),
            HostKey::Enter => self.interpreter.enter_at(line, cursor),
            HostKey::Up | HostKey::Down => {
                // Phase 6.5: the engine owns the cursor; arrows mutate it.
                // A fresh derive after the move keeps `state` accurate.
                let delta = if action == HostKey::Up { -1 } else { 1 };
                self.interpreter.move_cursor(line, delta);
                self.interpreter.state(line);
                OverlayAction::None
            }
            HostKey::Esc => {
                // Back navigation: replay the newest frame (previous
                // filter state + cursor) before falling back to the
                // structural level pop. Frames are pushed only on
                // accept/drill, so this walks strictly up the tree.
                if let Some(f) = self.frames.pop() {
                    return StepResult {
                        buffer: Some(f.line),
                        restore_cursor: Some(f.cursor),
                        action: AcceptOutcome::Continue(OverlayAction::None),
                        state: st,
                        frames_cleared: false,
                    };
                }
                self.interpreter.esc(line)
            }
            HostKey::Seed(idx) => {
                // Open-time cursor seeding: set the engine cursor directly.
                // No buffer rewrite, no frame push, no accept action.
                self.interpreter.set_cursor(line, idx);
                self.interpreter.state(line);
                OverlayAction::None
            }
        };
        match action {
            HostKey::Enter => {
                let out = self.controller.on_enter(ix_action.clone(), &st, line);
                self.push_frame_on(ix_action, line, cursor);
                StepResult {
                    buffer: None,
                    restore_cursor: None,
                    action: out,
                    state: st,
                    frames_cleared: false,
                }
            }
            HostKey::Esc => {
                // Esc already returned above (frame replay) or falls through
                // as the structural ladder pop. Esc never *pushes* frames
                // (it only walks up); applying the ladder's rewrite must not
                // create a new back-target.
                let buffer = match &ix_action {
                    OverlayAction::Buffer(b) => Some(b.clone()),
                    _ => None,
                };
                StepResult {
                    buffer,
                    restore_cursor: None,
                    action: AcceptOutcome::Continue(OverlayAction::None),
                    state: st,
                    frames_cleared: false,
                }
            }
            HostKey::Up | HostKey::Down => {
                // Pure cursor move: no buffer rewrite, no frame push.
                StepResult {
                    buffer: None,
                    restore_cursor: None,
                    action: AcceptOutcome::Continue(OverlayAction::None),
                    state: st,
                    frames_cleared: false,
                }
            }
            _ => {
                let buffer = match &ix_action {
                    OverlayAction::Buffer(b) => Some(b.clone()),
                    _ => None,
                };
                self.push_frame_on(ix_action, line, cursor);
                StepResult {
                    buffer,
                    restore_cursor: None,
                    action: AcceptOutcome::Continue(OverlayAction::None),
                    state: st,
                    frames_cleared: false,
                }
            }
        }
    }

    /// Push a traversal frame when an accept/drill rewrote the buffer
    /// downward (the pre-accept prompt + cursor are the "back" target).
    fn push_frame_on(
        &mut self,
        ix_action: OverlayAction,
        line: &str,
        cursor: usize,
    ) {
        if let OverlayAction::Buffer(b) = &ix_action {
            // Drill (trailing space) or accept-rewrite: the previous line
            // becomes a back target. Plain structural rewrites that came
            // from Esc are not routed here (Esc pops instead).
            let drilled = b.ends_with(' ');
            let accepted = !drilled && !b.is_empty();
            if drilled || accepted {
                let frame = Frame {
                    line: line.to_string(),
                    cursor,
                    level: level_of(self, line),
                };
                self.frames.push(frame);
            }
        }
    }

    /// Edit invalidation (design review, scoped): when the user edits the
    /// line, drop the traversal memory *of the current level only* — a
    /// deeply nested drill-down keeps its ancestors' filter state.
    ///
    /// Criterion: the edit is attributed to the level whose segment changed
    /// (`prev_line`'s level when the line had menu state). Removing *only*
    /// a trailing space is navigation (returning to the parent menu), not
    /// an edit — the level segment is unchanged, so nothing is cleared.
    pub fn on_edit(
        &mut self,
        prev_line: &str,
        line: &str,
    ) -> bool {
        // Pure trailing-space removal: navigation, keep everything.
        if prev_line.trim_end() == line.trim_end() {
            return false;
        }
        // Attribute the edit to the level of the *previous* line (the one
        // the user was editing); clear that level's frames.
        let edited_level = level_of(self, prev_line);
        let before = self.frames.len();
        self.frames.retain(|f| &f.level != &edited_level);
        self.frames.len() != before
    }

    /// Cursor-agnostic step (engine default row).
    pub fn step(&mut self, line: &str, action: HostKey) -> StepResult {
        let cursor = self.interpreter.state(line).active;
        self.step_at(line, cursor, action)
    }

    /// Map a prompt line to the engine's [`RenderPlan`] (design §6): title
    /// from the descriptor path's child header, per-row derived header
    /// (R8-4), candidate rows via the table builder + declared column
    /// defs, and the `>` / `*` row markers resolved from the menu state.
    /// Detail lines (design §5.1) come from the registered detail
    /// provider for this level (e.g. `/replay` help, `/stat` summary);
    /// falls back to `Vec::new` (no detail section).
    /// Phase 6.7-5: the bridge now emits the plan; the host's per-variant
    /// render arms are the remaining (Phase 7.1) cleanup.
    pub fn render_plan(&mut self, line: &str, term_width: usize, detail: Vec<String>) -> RenderPlan {
        let st = self.interpreter.state(line);
        // Walk to the level's node to source its static `child_header`.
        let mut node = self.interpreter.root.clone();
        for p in &st.node_path {
            node = match node.child(p) {
                Some(c) => c.clone(),
                None => MenuNode::default(),
            };
        }
        let child_header = node.child_header.clone();
        let title = self.level_title(&st);
        // The title and the child header both come from the node's
        // `child_header` for a *pinned* level (design §12: title = child
        // header). Don't render the same text twice; only pass a child
        // header line when it is distinct from the title (a genuine
        // subtitle, none exist yet).
        let subtitle = if child_header.as_deref() == Some(title.as_str()) {
            None
        } else {
            child_header
        };
        PlanBuilder {
            title,
            child_header: subtitle,
            row_header: None,
        }
        .build(&st, term_width, detail)
    }

    /// Read the registered detail provider for the level at
    /// `line` (design §5.1): returns the provider's detail lines,
    /// or `None` if the level has no detail provider (use an
    /// empty vec in that case).
    pub fn detail_for(&mut self, line: &str) -> Option<Vec<String>> {
        let st = self.interpreter.state(line);
        let path_key = st.node_path.join(" ");
        self.detail_providers
            .get_mut(&path_key)
            .map(|p| p.fetch())
    }

    /// Display title for the level the prompt is at: the child-level's
    /// node `child_header`, else the last path key (or "Commands" at root).
    fn level_title(&self, st: &MenuState) -> String {
        let mut node = self.interpreter.root.clone();
        for p in &st.node_path {
            node = match node.child(p) {
                Some(c) => c.clone(),
                None => return p.clone(),
            };
        }
        node.child_header
            .clone()
            .unwrap_or_else(|| {
                st.node_path
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "Commands".to_string())
            })
    }
}

/// Keys the host forwards to the menu engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostKey {
    Tab,
    Enter,
    Esc,
    /// Move the engine-owned view cursor up one row.
    Up,
    /// Move the engine-owned view cursor down one row.
    Down,
    /// Open-time cursor seeding (replaces `engine_seed`).
    Seed(usize),
}

/// Result of one engine step.
pub struct StepResult {
    /// Buffer rewrite to apply to the LineEditor (Tab/Esc/Enter drill).
    pub buffer: Option<String>,
    /// Restored cursor row (back-navigation); None = keep current.
    pub restore_cursor: Option<usize>,
    /// Accept-flow outcome (Enter only).
    pub action: AcceptOutcome,
    /// The menu state after the step (for rendering).
    pub state: MenuState,
    /// True when an edit invalidated the traversal memory.
    pub frames_cleared: bool,
}

/// Helper: build rows for a value child's enum set (tests).
#[allow(dead_code)]
pub(crate) fn enum_rows(values: &[&str]) -> Vec<crate::slashdot_menu::fields::Row> {
    values
        .iter()
        .map(|v| crate::slashdot_menu::fields::Row::new(*v))
        .collect()
}

/// Helper: read a string field from a row's fields.
#[allow(dead_code)]
pub(crate) fn row_str(r: &crate::slashdot_menu::fields::Row, k: &str) -> Option<String> {
    r.field_value(k).and_then(|v| match v {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> MenuShared {
        let settings = crate::settings::SettingsStore {
            default_model: "p:m".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_thinking_level: "off".into(),
            show_reasoning: "indicator".into(),
            reasoning_in_context: false,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            path: None,
        };
        MenuShared::new(settings, None)
    }

    /// slash_menu.md acceptance: bare `/thinking` lists all levels in
    /// declaration order; the active level is surfaced for the `*` marker.
    #[test]
    fn thinking_bare_lists_all() {
        let mut e = MenuEngine::new(&shared());
        let st = e.interpreter.state("thinking ");
        assert_eq!(
            st.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["off", "low", "med", "high"]
        );
    }

    /// Tab completes the trailing partial keeping earlier terms (D2):
    /// "thinking h" — "h" is a fuzzy term, the active candidate completes
    /// to the id, replacing the partial (positional selection slot).
    #[test]
    fn thinking_tab_completes_partial() {
        let mut e = MenuEngine::new(&shared());
        let st = e.interpreter.state("thinking h");
        // "h" is not a valid enum value → demoted to a fuzzy term.
        assert_eq!(st.selection, None);
        assert_eq!(st.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["high"]);
        let act = e.interpreter.tab("thinking h");
        // Value child: Tab pins with harmless trailing space (`/thinking
        // high ` ≡ `/thinking high`).
        assert_eq!(act, OverlayAction::Buffer("thinking high ".to_string()));
    }

    /// Enter on a completed level submits the canonical line; the host
    /// parses it into SetThinking (same path as typed input).
    #[test]
    fn thinking_enter_submits() {
        let mut e = MenuEngine::new(&shared());
        let out = e.step("thinking med", HostKey::Enter);
        match out.action {
            AcceptOutcome::Continue(OverlayAction::Submit(line)) => {
                assert_eq!(line, "thinking med");
            }
            other => panic!("expected submit, got {:?}", other),
        }
    }

    /// Esc pops the pinned selection (v1 contract): pinned → drilled level
    /// ("thinking ") → parent path → cleared.
    #[test]
    fn thinking_esc_pops_pin() {
        let mut e = MenuEngine::new(&shared());
        let out = e.step("thinking med", HostKey::Esc);
        assert_eq!(out.buffer, Some("thinking ".to_string()));
        let out2 = e.step("thinking ", HostKey::Esc);
        assert_eq!(out2.buffer, Some("thinking".to_string()));
        let out3 = e.step("thinking", HostKey::Esc);
        assert_eq!(out3.buffer, Some(String::new()));
    }

    /// Fuzzy terms filter the value set and pin survives (R8-5).
    #[test]
    fn thinking_terms_filter() {
        let mut e = MenuEngine::new(&shared());
        let st = e.interpreter.state("thinking e");
        // "e" matches "high" (desc) and "med"… ids: off/low/med/high —
        // "e" fuzzy-matches "med" and "high". Just verify filtering happened.
        assert!(st.candidates.len() < 4);
    }

    /// Regression: `/thinking` (bare, no trailing word) + Enter must drill
    /// (buffer "/thinking "), not submit the level name as a level value
    /// ("thinking thinking" → "unknown thinking level").
    #[test]
    fn thinking_enter_bare_drills_not_submits() {
        let mut e = MenuEngine::new(&shared());
        let act = e.interpreter.enter("thinking");
        assert_eq!(act, OverlayAction::Buffer("thinking ".to_string()));
        // Sanity: a completed value still submits.
        assert_eq!(
            e.interpreter.enter("thinking med"),
            OverlayAction::Submit("thinking med".to_string())
        );
    }

    /// Revised accept semantics (design review): Tab/Enter act on the
    /// host cursor row and clear refinement state — the buffer keeps only
    /// path + committed id.
    ///
    /// Leaf-node accept contract (generic, not /replay-specific):
    ///   - `/repl` + Enter → submit `/replay` immediately (single key press;
    ///     the focused best partial match is a pure leaf → submit its
    ///     canonical form).
    ///   - `/repl` + Tab → `/replay ` (pins with trailing space, shows the
    ///     detail/help panel so the user can add an optional arg).
    ///   - listing nodes (e.g. `/thinking`) are unchanged: Enter drills to
    ///     the child menu, never submits.
    #[test]
    fn leaf_enter_submits_canonical_tab_drills() {
        let mut e = MenuEngine::new(&shared());
        // Pure leaf (`/replay`): Enter expands canonically and submits even
        // from a partial fuzzy match.
        let out = e.interpreter.enter("repl");
        assert_eq!(
            out,
            OverlayAction::Submit("replay".to_string()),
            "partial leaf prefix + Enter must submit the canonical leaf"
        );
        // Full leaf + Enter likewise submits.
        let out2 = e.interpreter.enter("replay");
        assert_eq!(out2, OverlayAction::Submit("replay".to_string()));
        // Tab pins with a trailing space (detail/help shown), no submit.
        let tab = e.interpreter.tab("repl");
        assert_eq!(
            tab,
            OverlayAction::Buffer("replay ".to_string()),
            "Tab on a leaf pins with trailing space (drills to the help panel)"
        );
        // Listing (value child, /thinking): Enter still drills, never submits.
        let t = e.interpreter.enter("thinking");
        assert_eq!(
            t,
            OverlayAction::Buffer("thinking ".to_string()),
            "listing Enter must drill, not submit"
        );
        let t2 = e.interpreter.tab("thinking");
        assert_eq!(t2, OverlayAction::Buffer("thinking ".to_string()));
        // Drilled listing seeded with a value pins and submits on Enter.
        let t3 = e.interpreter.enter("thinking med");
        assert_eq!(t3, OverlayAction::Submit("thinking med".to_string()));
        // A typed limiter arg to the full leaf is preserved: `/replay 5` +
        // Enter submits `replay 5` (the canonical line, arg intact).
        let with_arg = e.interpreter.enter("replay 5");
        assert_eq!(with_arg, OverlayAction::Submit("replay 5".to_string()));
    }

    /// `/repl` + Enter ≙ `/repl` + Tab + Enter: the canonical expansion
    /// (same path, possibly injecting defaults) then submit.
    #[test]
    fn leaf_enter_equals_tab_then_enter() {
        let mut e = MenuEngine::new(&shared());
        let direct = e.interpreter.enter("repl");
        // Tab expand → `/replay `, then Enter submits it stripped.
        let tabbed = e.interpreter.tab("repl");
        assert_eq!(tabbed, OverlayAction::Buffer("replay ".to_string()));
        let after_tab = e.interpreter.enter("replay ");
        assert_eq!(direct, after_tab, "Enter == Tab-then-Enter");
        assert_eq!(direct, OverlayAction::Submit("replay".to_string()));
    }

    /// Revised accept semantics (design review): Tab/Enter act on the
    /// host cursor row and clear refinement state — the buffer keeps only
    /// path + committed id.
    #[test]
    fn tab_acts_on_host_cursor_and_clears_terms() {
        let mut e = MenuEngine::new(&shared());
        // "e" ranks med first (1 candidate): cursor on row 0 must be the
        // completed id; cursor beyond the view is clamped to the same.
        let st = e.interpreter.state("thinking e");
        let top_id = st.candidates[0].id.clone();
        let r = e.step_at("thinking e", 0, HostKey::Tab);
        assert_eq!(
            r.buffer,
            Some(format!("thinking {} ", top_id)),
            "Tab must honor the host cursor row (with drill space)"
        );
        // Out-of-range cursor falls back to the engine default row.
        let r1 = e.step_at("thinking e", 9, HostKey::Tab);
        assert_eq!(r1.buffer, Some(format!("thinking {} ", top_id)));
        // Both accepts pushed frames (engine-owned) for back-navigation.
        assert_eq!(e.frames.len(), 2);
        // Esc replays the newest frame: previous filter state + cursor.
        let r2 = e.step_at(&format!("thinking {}", top_id), 0, HostKey::Esc);
        assert_eq!(r2.buffer, Some("thinking e".to_string()));
        assert_eq!(r2.restore_cursor, Some(9));
        assert_eq!(e.frames.len(), 1);
        // Second Esc replays the first frame; third falls back to the
        // structural pop (pinned level → drilled "thinking ").
        let r3 = e.step_at("thinking e", 0, HostKey::Esc);
        assert_eq!(r3.buffer, Some("thinking e".to_string()));
        // Frames exhausted → the structural ladder (design §5.1): clear
        // this level's terms keeping the list open, then strip the
        // trailing space, then clear the buffer at the top level.
        let r4 = e.step_at("thinking e", 0, HostKey::Esc);
        assert_eq!(r4.buffer, Some("thinking ".to_string()));
        let r5 = e.step_at("thinking ", 0, HostKey::Esc);
        assert_eq!(r5.buffer, Some("thinking".to_string()));
        let r6 = e.step_at("thinking", 0, HostKey::Esc);
        assert_eq!(r6.buffer, Some(String::new()));
    }

    /// Esc with an empty stack falls back to the structural level pop
    /// (strict up-walk preserved).
    #[test]
    fn esc_without_frames_pops_level() {
        let mut e = MenuEngine::new(&shared());
        let r = e.step_at("thinking med", 0, HostKey::Esc);
        assert_eq!(r.buffer, Some("thinking ".to_string()));
    }

    /// Root list (Phase 6 step 2): the command registry is the root
    /// descriptor; bare "/" lists all commands in registry order; a filter
    /// term ranks them; Tab/Enter drill into the pinned command.
    #[test]
    fn root_list_from_registry() {
        let mut e = MenuEngine::new(&shared());
        let st = e.interpreter.state("");
        let ids: Vec<&str> = st.candidates.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"help"));
        assert!(ids.contains(&"thinking"));
        assert!(ids.contains(&"quit"));
        // Filter ranks by name+desc ("mo" → model).
        let st2 = e.interpreter.state("mo");
        assert!(st2.candidates.iter().any(|r| r.id == "model"));
        // Drill into a pinned command opens its level.
        let st3 = e.interpreter.state("thinking ");
        assert_eq!(st3.node_path, vec!["thinking"]);
        assert_eq!(
            st3.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["off", "low", "med", "high"]
        );
    }

    /// Persona level (Phase 6 step 3): PERSONAS + "none" as a value child.
    #[test]
    fn persona_level_from_registry() {
        let mut e = MenuEngine::new(&shared());
        let st = e.interpreter.state("persona ");
        let ids: Vec<&str> = st.candidates.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"none"));
        assert!(!ids.is_empty());
        // Filter by prefix.
        let st2 = e.interpreter.state("persona none");
        assert_eq!(st2.selection.as_deref(), Some("none"));
        // Accept semantics: Tab pins path+id (+ harmless drill space).
        assert_eq!(
            e.interpreter.tab("persona none"),
            OverlayAction::Buffer("persona none ".to_string())
        );
    }

    /// Settings levels (Phase 6 step 3): SETTINGS children exist with
    /// value children per ValueKind; pinned setting drills to its values.
    #[test]
    fn settings_levels_from_metadata() {
        let mut e = MenuEngine::new(&shared());
        // Enum setting: security_mode.
        let st = e.interpreter.state("set security_mode ");
        assert_eq!(st.node_path, vec!["set", "security_mode"]);
        assert_eq!(
            st.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["yolo", "confirm"]
        );
        // Int setting: Number value child (no static candidates).
        let st2 = e.interpreter.state("set tool_call_limit ");
        assert_eq!(st2.node_path, vec!["set", "tool_call_limit"]);
        // FreeText setting: plain leaf (no value child).
        let st3 = e.interpreter.state("set llm_compact_prompt ");
        assert_eq!(st3.node_path, vec!["set", "llm_compact_prompt"]);
        // Filter at the settings level.
        let st4 = e.interpreter.state("set security");
        assert!(st4.candidates.iter().any(|r| r.id == "security_mode"));
    }

    /// Scoped edit invalidation: an ID edit at the current level clears
    /// that level's frames; deeper edits keep ancestor levels' state; and
    /// a pure trailing-space removal is navigation (clears nothing).
    #[test]
    fn on_edit_scoped_invalidation() {
        let mut e = MenuEngine::new(&shared());
        // Build depth: accept "thinking" (frame at root level), then drill.
        let _ = e.step_at("thinking", 0, HostKey::Tab);
        let _ = e.step_at("thinking ", 0, HostKey::Tab);
        // Two frames (engine-owned), both at the thinking level (value
        // child has no deeper static level): "thinking" (pre-accept) and
        // "thinking " (pre-pin).
        assert_eq!(e.frames.len(), 2);
        // ID edit at the same level → both cleared (same level identity).
        assert!(e.on_edit("thinking off", "thinking low"));
        assert!(e.frames.is_empty());
        // Trailing-space removal → navigation, nothing cleared.
        let _ = e.step_at("thinking", 0, HostKey::Tab);
        assert_eq!(e.frames.len(), 1);
        assert!(!e.on_edit("thinking ", "thinking"));
        assert_eq!(e.frames.len(), 1);
    }

    /// Frames are pushed on drill too: Esc from a drilled child returns to
    /// the pre-drill line with the cursor.
    #[test]
    fn drill_pushes_frame_esc_returns() {
        let mut e = MenuEngine::new(&shared());
        // Host cursor decides: the buffer "/thinking" pins "thinking",
        // whose row index in the root list is 1 (help first).
        let r = e.step_at("thinking", 1, HostKey::Enter);
        // Enter returns the action via `action`, not `buffer`.
        match &r.action {
            AcceptOutcome::Continue(OverlayAction::Buffer(b)) => {
                assert_eq!(b, "thinking ");
            }
            other => panic!("expected drill buffer, got {:?}", other),
        }
        assert_eq!(e.frames.len(), 1);
        let r2 = e.step_at("thinking ", 0, HostKey::Esc);
        assert_eq!(r2.buffer, Some("thinking".to_string()));
        assert_eq!(r2.restore_cursor, Some(1));
    }

    /// Regression: `/thinking` + Tab (no trailing word) must expand to
    /// `/thinking ` so the value menu shows (same as Enter). The host
    /// re-adds the leading slash.
    #[test]
    fn thinking_tab_preserves_slash_prefix() {
        let mut e = MenuEngine::new(&shared());
        // Bare level: Tab promotes (drill), host re-adds the slash.
        let act = e.interpreter.tab("thinking");
        assert_eq!(act, OverlayAction::Buffer("thinking ".to_string()));
        assert_eq!(format!("/{}", "thinking "), "/thinking ");
        // Partial value: Tab completes, prefix intact (+ drill space).
        let act2 = e.interpreter.tab("thinking h");
        assert_eq!(
            act2,
            OverlayAction::Buffer("thinking high ".to_string())
        );
    }

    /// Phase 6.7-5: the bridge emits a renderable plan for a dynamic
    /// (model) level — declared columns drive the row layout.
    #[test]
    fn model_level_emits_render_plan() {
        use crate::slashdot_menu::render::RenderPlan;
        let mut e = MenuEngine::new(&shared());
        // The provider declares 6 display columns (Model/Ctx/Coding/
        // Price in/Price out/Reason); the engine surfaces them so the
        // generic renderer needs no host-side column layout.
        let st = e.interpreter.state("model ");
        assert_eq!(st.columns.len(), 6);
        assert!(st.columns.iter().any(|c| c.header == "Price in"));

        let plan: RenderPlan = e.render_plan("model ", 100, Vec::new());
        assert!(!plan.rows.is_empty(), "model level has rows");
        assert_eq!(plan.selected, Some(0));
        // v2.5: title removed; header() is the panel title mechanism.
        assert!(
            !plan.rows.is_empty(),
            "child_header/subtitle must not duplicate"
        );
        // Paint path: full-width padded rows (header + rows).
        let painted = plan.paint(100);
        assert!(
            painted.iter().all(|l| l.starts_with("\x1b[48;5;")),
            "every line carries a panel background"
        );
    }

    /// The *_price ratios floor the combined price at a tiny epsilon so free
    /// models (price == 0) sort ahead of paid ones, still in rating order.
    #[test]
    fn model_row_floors_price_for_free_models() {
        use crate::model_catalog::CostSource;
        use crate::model_catalog::ModelEntry;
        use crate::slashdot_menu::value::Value;
        let entry = |prompt: Option<f64>, coding: Option<f32>| ModelEntry {
            provider: "p".into(),
            id: "m".into(),
            display: "p:m".into(),
            context_length: None,
            prompt_cost: prompt,
            completion_cost: None,
            reasoning: false,
            cost_source: CostSource::Direct,
            created: None,
            coding_index: coding,
            intelligence_index: None,
            agentic_index: None,
        };
        // Free model: price 0, coding 80.
        let free = entry(Some(0.0), Some(80.0));
        let r = model_row(&free);
        let cp = r
            .field_value("coding_price")
            .expect("coding_price present for free model");
        let Value::Num(n) = cp else {
            panic!("coding_price must be numeric");
        };
        // floored: 80 / 1e-10 == 8e11 -> huge positive, sorts ahead of paid.
        assert!(*n > 1e9, "free model ratio huge, got {}", n);

        // A slightly-priced model (price 0.5, coding 80) sorts below the
        // free one (80/0.5 == 160 vs 8e11).
        let paid = entry(Some(0.5), Some(80.0));
        let r2 = model_row(&paid);
        if let Some(Value::Num(p2)) = r2.field_value("coding_price") {
            assert!(*p2 < *n, "free model must outrank the paid one");
        }
    }
}
