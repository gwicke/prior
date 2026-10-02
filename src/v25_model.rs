//! v2.5 `SlashDotNode` implementation for `/model`.
//!
//! Dynamic rows from the live catalog (cached, background-refreshed). The
//! node caches its `Rc<DataResponse>` keyed by the catalog's generation
//! counter — when the catalog hasn't refreshed, the engine's `Rc::ptr_eq`
//! check skips the full re-derive (filter → fuzzy → sort → render).
use std::sync::Arc;

use std::rc::Rc;

use crate::app_state::AppState;
use crate::slashdot_menu::fields::ColumnDef;
use crate::slashdot_menu::fields::Row;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode, StaticRow, SubmitResult,
};

// ---------------------------------------------------------------------------
// ModelNode — dynamic catalog rows with Rc-cached DataResponse.
// ---------------------------------------------------------------------------

pub struct ModelNode {
    /// Read the active model id (empty string = none).
    active_fn: Box<dyn Fn() -> String>,
    /// Write the selected model id on submit. `Ok(())` on success.
    submit_fn: crate::SubmitFn,
    /// The live `/model` node's app handle: recency ordering in `data()`
    /// AND the journal record in `submit()`. `None` for
    /// `/set default_model` — a settings write is not a model change and
    /// must not journal a `ModelChange`.
    live: Option<Arc<AppState>>,
    /// A Ctrl-E edit awaiting the confirm's accept arm, keyed by epoch and
    /// model id like the compaction draft. A stale stash means the data epoch
    /// moved under us, and writing it would store settings against a model
    /// the user is no longer looking at.
    pending_edit: std::cell::RefCell<Option<PendingEdit>>,
    /// Last generated DataResponse, reused via `Rc::ptr_eq` when nothing the rows
    /// derive from has changed. The three fields below *are* that condition —
    /// `data()` reuses only while all of them still hold.
    cached_response: Option<Rc<DataResponse>>,
    /// Catalog generation at build time: the entry set and its base order.
    cached_generation: u64,
    /// The active model id at build time. It is both hoisted to row 0 by
    /// [`order_models`] and named in `active_ids`, so a switch must rebuild.
    cached_active: String,
    /// Recently-used models at build time: they fill the rows after the
    /// active one, so a change reorders the listing and moves the active
    /// row's index.
    cached_recent: Vec<String>,
}

/// A Ctrl-E result waiting to be committed by the confirm's accept arm.
///
/// Ctrl-E is two-phase, like compaction: the editor is the *review*, the
/// dialog is the *commit point*. The engine calls `submit()` on the node that
/// armed it — for a listing, that is this node — so this stash is what tells
/// `submit()` to write settings rather than activate a model.
struct PendingEdit {
    epoch: u64,
    model: String,
    settings: crate::model_settings::ModelSettings,
}

/// The model a path is about, or `None` when none has been drilled.
///
/// `above` is how many path segments sit above the model node: 1 for the live
/// `/model` (`["model", ...]`), 2 for `/set default_model`
/// (`["set", "default_model", ...]`). Depth is the only reliable signal. The
/// engine threads ids rather than structure, and the last id at the *undrilled*
/// level is the command's own name (`"model"`, `"default_model"`) — which is
/// indistinguishable from a model id if you only look at the tail. That is
/// exactly the bug a first cut made: every catalog row grew the same two
/// actions.
///
/// With a model drilled, its id sits directly above our own action names
/// (`["model", "<id>"]` at the action level, `["model", "<id>", "save"]` one
/// level deeper).
fn drilled_model(path_args: &PathArgs, above: usize) -> Option<String> {
    if path_args.ids.len() <= above {
        return None;
    }
    path_args
        .ids
        .iter()
        .rev()
        .find(|id| !is_action_id(id))
        .cloned()
}

/// True for the action row ids this node's children contribute.
fn is_action_id(id: &str) -> bool {
    matches!(id, "activate" | "save")
}

/// A value for display, or a dash when unset. The provider's own default is
/// deliberately not shown: the live value is the question, and printing a
/// known-but-unsent default would read as "this is what will be sent".
fn fmt_opt(v: Option<f64>) -> String {
    v.map(crate::v25_number_param::fmt_value)
        .unwrap_or_else(|| "—".to_string())
}

impl ModelNode {
    pub fn new(
        active_fn: Box<dyn Fn() -> String>,
        submit_fn: crate::SubmitFn,
        live: Option<Arc<AppState>>,
    ) -> Self {
        ModelNode {
            active_fn,
            submit_fn,
            live,
            cached_response: None,
            // `u64::MAX` is unreachable as a generation, so the first build
            // always misses the cache.
            cached_generation: u64::MAX,
            cached_active: String::new(),
            cached_recent: Vec::new(),
            pending_edit: std::cell::RefCell::new(None),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for ModelNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        // Read every input *above* the early return: all three decide reuse,
        // and reading them once is cheaper than reading them on a miss and
        // again here.
        let gen = crate::model_catalog::catalog_generation();
        let active_id = (self.active_fn)();
        let recent: Vec<String> = self
            .live
            .as_ref()
            .map(|r| r.recent_models())
            .unwrap_or_default();

        // Reuse only while every input the rows derive from still holds. The
        // generation is not enough on its own: `order_models` also hoists the
        // *active* model and boosts recently-used ones, and `active_ids` is
        // what the `*` marker lands on. Keying on the generation alone served
        // a stale `active_ids` after a model switch — and because
        // `refresh_data` gates on `Rc::ptr_eq`, the stale `Rc` also stopped
        // `content.active_ids` and `slots_dirty` from refreshing, so the memo
        // in `ensure_rows` returned the stale view too. The marker then sat on
        // the *previous* active model, which `order_models` had hoisted to row
        // 0 — stale, but self-consistent enough to look like a stray marker
        // rather than a missing one.
        if let Some(ref cached) = self.cached_response {
            if self.cached_generation == gen
                && self.cached_active == active_id
                && self.cached_recent == recent
            {
                return Ok(Rc::clone(cached));
            }
        }

        // An input changed (or first call) — rebuild rows.
        //
        // The *resolved* list, so a Ctrl-E'd override of cost or an index shows
        // in the row rather than only in the `/model <id>` header. `live` is
        // `None` under `/set default_model`, which has no store to read, so
        // that node keeps the raw catalog — the same trade it already makes
        // for recency.
        let entries: Vec<crate::model_catalog::ModelEntry> = match self.live.as_ref() {
            Some(app) => app.resolved_models().as_ref().clone(),
            None => crate::model_catalog::catalog_cached(),
        };
        let active = if active_id.is_empty() {
            None
        } else {
            Some(active_id.clone())
        };

        // Build rows in the same order as the legacy ModelProvider:
        // active first, then up to 4 recent in LRU order, then server
        // popularity order for the rest.
        let ordered = order_models(&entries, &active, &recent);

        let active_ids: Vec<String> = if let Some(ref a) = active {
            vec![a.clone()]
        } else {
            Vec::new()
        };

        let rows: Vec<Row> = ordered.iter().map(model_row).collect();
        let response = Rc::new(DataResponse { rows, active_ids });
        self.cached_response = Some(Rc::clone(&response));
        self.cached_generation = gen;
        self.cached_active = active_id;
        self.cached_recent = recent;
        Ok(response)
    }

    /// What this model is, and what it is set to right now.
    ///
    /// The resolved view — catalog overlaid with the stored per-model
    /// overrides — so these are the figures the app actually uses rather than
    /// what OpenRouter last published. Absent facts read as absent.
    async fn header(&mut self, path_args: &PathArgs, render_ctx: &RenderContext) -> Vec<String> {
        let Some(model) = drilled_model(path_args, 1) else {
            return vec![
                "Pick a model to see its details, its settings, and its actions.".to_string(),
            ];
        };
        let stored = self
            .live
            .as_ref()
            .and_then(|app| app.with_settings(|s| s.model_settings(&model).cloned()));
        let entry = crate::model_catalog::resolved_entry(&model, stored.as_ref());
        let mut lines = vec![format!("model: {model}")];
        if let Some(e) = entry.as_ref() {
            if let Some(d) = e.description.as_deref() {
                let width = render_ctx.term_width.saturating_sub(4).max(20);
                lines.extend(
                    crate::output::wrap(d, width)
                        .into_iter()
                        .map(|l| format!("  {l}")),
                );
            }
            lines.push(format!(
                "  ctx {}   cost ${:.2}/${:.2} per M",
                e.context_length
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "—".into()),
                e.prompt_cost.unwrap_or(0.0),
                e.completion_cost.unwrap_or(0.0),
            ));
            let idx = |v: Option<f32>| v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "—".into());
            lines.push(format!(
                "  coding {}   intel {}   agentic {}",
                idx(e.coding_index),
                idx(e.intelligence_index),
                idx(e.agentic_index)
            ));
            lines.push(if e.reasoning_levels.is_empty() {
                "  reasoning: not published".to_string()
            } else {
                format!("  reasoning: {}", e.reasoning_levels.join(", "))
            });
        } else {
            lines.push("  (no catalog entry — details unknown)".to_string());
        }

        // Current settings: live for the active model, stored for an inactive
        // one, since "what would using this do" is the question being asked.
        if let Some(app) = self.live.as_ref() {
            if app.model() == model {
                let p = app.params();
                lines.push(format!(
                    "  now: reasoning {}   temperature {}   top_p {}",
                    p.reasoning
                        .map(|l| l.as_str().to_string())
                        .unwrap_or_else(|| "off".into()),
                    fmt_opt(p.temperature),
                    fmt_opt(p.top_p),
                ));
            } else {
                lines.push(format!(
                    "  defaults: {}",
                    stored
                        .as_ref()
                        .map(crate::settings_editor::summarize)
                        .unwrap_or_else(|| "not configured".to_string())
                ));
            }
            lines.push("  ctrl-e to edit these settings".to_string());
        }
        lines
    }

    /// Ctrl-E: edit this model's stored settings in `$EDITOR`, then arm the
    /// confirm. Two phases, as agreed — the editor is the review, the dialog is
    /// the commit point, because this writes the user's settings file and there
    /// is no undo.
    ///
    /// Works for an **inactive** model too: the store is the source either way,
    /// and a model not yet activated this session simply has no live values to
    /// be overwritten.
    async fn edit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let Some(app) = self.live.as_ref() else {
            return SubmitResult::Handled;
        };
        let Some(model) = drilled_model(path_args, 1) else {
            return SubmitResult::Handled;
        };
        let current = app
            .with_settings(|s| s.model_settings(&model).cloned())
            .unwrap_or_default();
        let hints = match model.split_once(':') {
            Some((p, m)) => crate::model_settings::hints_for(p, m),
            None => Default::default(),
        };
        match crate::settings_editor::edit_fragment(app, &model, &current, &hints) {
            // Nothing changed, so there is nothing to confirm.
            crate::settings_editor::EditOutcome::Unchanged => SubmitResult::Handled,
            crate::settings_editor::EditOutcome::Invalid(msg) => SubmitResult::Error(msg),
            crate::settings_editor::EditOutcome::Unavailable(msg) => {
                SubmitResult::Error(format!("cannot edit settings: {msg}"))
            }
            crate::settings_editor::EditOutcome::Updated(settings) => {
                *self.pending_edit.borrow_mut() = Some(PendingEdit {
                    epoch,
                    model: model.clone(),
                    settings,
                });
                SubmitResult::Prepared(Some((
                    format!("Save these settings for {model}"),
                    "Discard the edit".to_string(),
                )))
            }
        }
    }

    async fn submit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        // Two jobs, told apart by what is pending. A stashed Ctrl-E means the
        // confirm's accept arm fired: write the settings. With nothing stashed
        // this is the legacy activate path.
        if let Some(pending) = self.pending_edit.borrow_mut().take() {
            if pending.epoch != epoch {
                return SubmitResult::Error(
                    "the model list changed while editing — nothing was saved".to_string(),
                );
            }
            let Some(app) = self.live.as_ref() else {
                return SubmitResult::Handled;
            };
            return match app.update_settings(|s| {
                s.apply_model_settings(&pending.model, &pending.settings);
                Ok(())
            }) {
                Ok(()) => SubmitResult::Done(Some(format!("saved settings for {}", pending.model))),
                Err(e) => SubmitResult::Error(format!("cannot save settings: {e}")),
            };
        }

        let qualified = drilled_model(path_args, 1).unwrap_or_default();
        if qualified.is_empty() {
            return SubmitResult::Done(None);
        }
        match (self.submit_fn)(&qualified) {
            Ok(()) => {
                // Live `/model` node: the explicit commit is journaled NOW —
                // target-stamp judgment (recorded in the plan): the entry
                // lands even if the next cycle's rebuild fails; a later
                // `ModelChange` record corrects the journal. The record
                // helper skips a value the session already holds, so
                // re-commits stay idempotent.
                if let Some(app) = self.live.as_ref() {
                    if let Some(s) = app.session() {
                        crate::agent::record_model_change(&s, &qualified).await;
                    }
                }
                SubmitResult::Done(None)
            }
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }

    fn fields(&self) -> Vec<crate::slashdot_menu::fields::FieldSpec> {
        use crate::slashdot_menu::fields::{FieldSpec, SortDir};
        // Return field metadata for the engine's generic filter/sort.
        // Descriptions come from MODEL_MODIFIERS (the v2 bridge's source).
        let desc_of = |name: &str| -> &'static str {
            crate::model_catalog::MODEL_MODIFIERS
                .iter()
                .find(|m| m.token == name)
                .map(|m| m.desc)
                .unwrap_or("")
        };
        vec![
            FieldSpec::new("context", true)
                .dir(SortDir::Asc)
                .description(desc_of("context")),
            FieldSpec::new("price_input", true)
                .dir(SortDir::Asc)
                .description(desc_of("price_input")),
            FieldSpec::new("price_output", true)
                .dir(SortDir::Asc)
                .description(desc_of("price_output")),
            FieldSpec::new("price", true)
                .dir(SortDir::Asc)
                .description(desc_of("price")),
            FieldSpec::new("coding", true)
                .dir(SortDir::Desc)
                .description(desc_of("coding")),
            FieldSpec::new("intelligence", true)
                .dir(SortDir::Desc)
                .description(desc_of("intelligence")),
            FieldSpec::new("agentic", true)
                .dir(SortDir::Desc)
                .description(desc_of("agentic")),
            FieldSpec::new("reasoning", true)
                .dir(SortDir::Desc)
                .description("reasoning capability"),
            FieldSpec::new("created", false)
                .dir(SortDir::Desc)
                .description("creation date"),
            FieldSpec::new("coding_price", true)
                .dir(SortDir::Desc)
                .description("coding_price"),
            FieldSpec::new("intelligence_price", true)
                .dir(SortDir::Desc)
                .description("intelligence_price"),
            FieldSpec::new("agentic_price", true)
                .dir(SortDir::Desc)
                .description("agentic_price"),
        ]
    }

    fn columns(&self) -> Vec<ColumnDef> {
        model_columns()
    }
}

// ---------------------------------------------------------------------------
// Model row construction (mirrors slash_menu_bridge::model_row).
// ---------------------------------------------------------------------------

/// Convert a `ModelEntry` into a menu `Row` with all filterable/sortable
/// fields attached. This is the v2.5 equivalent of the legacy
/// `slash_menu_bridge::model_row`.
fn model_row(e: &crate::model_catalog::ModelEntry) -> Row {
    use crate::slashdot_menu::value::Value;
    // The OpenRouter blurb is the only human-readable text we have about a
    // model, and as the row description it also makes the fuzzy filter match
    // on something other than the id.
    let mut r =
        Row::new(e.qualified()).desc(e.description.clone().unwrap_or_else(|| e.display.clone()));
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
    r = r.field("reasoning", Value::Num(if e.reasoning { 1.0 } else { 0.0 }));
    // Combined price (max of in+out, or whichever is present).
    let combined_price = match (e.prompt_cost, e.completion_cost) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    if let Some(p) = combined_price {
        r = r.field("price", Value::Num(p));
    }
    // Index-per-price ratios (epsilon-floored for free models).
    for (idx_name, idx_val) in [
        ("coding", e.coding_index.map(|v| v as f64)),
        ("intelligence", e.intelligence_index.map(|v| v as f64)),
        ("agentic", e.agentic_index.map(|v| v as f64)),
    ] {
        if let (Some(iv), Some(p)) = (idx_val, combined_price) {
            let denom = p.max(1e-10);
            r = r.field(format!("{}_price", idx_name), Value::Num(iv / denom));
        }
    }
    // Created date as ISO string (sortable/filterable).
    if let Some(t) = e.created {
        let iso = crate::model_catalog::fmt_iso_date(t);
        r = r.field("created", Value::Str(iso));
    }
    r
}

// ---------------------------------------------------------------------------
// Model ordering: active first, then recent LRU, then server popularity.
// ---------------------------------------------------------------------------

fn order_models(
    entries: &[crate::model_catalog::ModelEntry],
    active: &Option<String>,
    recent: &[String],
) -> Vec<crate::model_catalog::ModelEntry> {
    // Server position = index in the catalog's popularity order.
    let server_rank: std::collections::HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.clone(), i))
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut ordered: Vec<crate::model_catalog::ModelEntry> = Vec::new();

    // 1. Active model first (if present in the catalog).
    let active_id = active.as_deref().unwrap_or("");
    if let Some(active_entry) = entries.iter().find(|e| e.qualified() == active_id) {
        if seen.insert(active_entry.id.clone()) {
            ordered.push(active_entry.clone());
        }
    }
    // 2. Up to 4 more recent models in LRU order (total 5 boosted).
    for recent_id in recent {
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
        .iter()
        .filter(|e| !seen.contains(&e.id))
        .cloned()
        .collect();
    rest.sort_by(|a, b| {
        let ra = server_rank.get(&a.id).copied().unwrap_or(usize::MAX);
        let rb = server_rank.get(&b.id).copied().unwrap_or(usize::MAX);
        ra.cmp(&rb)
    });
    ordered.extend(rest);
    ordered
}

// ---------------------------------------------------------------------------
// Shared column definitions.
// ---------------------------------------------------------------------------

/// Column definitions for the model catalog table, shared by
/// `/model` and `/set default_model`.
pub fn model_columns() -> Vec<ColumnDef> {
    use crate::slashdot_menu::table::Align;
    vec![
        ColumnDef::new("id", "Model").min(12).prio(3),
        ColumnDef::new("context", "Ctx").align(Align::Right).prio(1),
        ColumnDef::new("coding", "Coding")
            .align(Align::Right)
            .prio(1),
        ColumnDef::new("price_input", "$/m in")
            .align(Align::Right)
            .prio(2),
        ColumnDef::new("price_output", "$/m out")
            .align(Align::Right)
            .prio(2),
    ]
}

// ---------------------------------------------------------------------------
// Menu builder — adds model to the root menu.
// ---------------------------------------------------------------------------

/// Build the `/model` subtree as a `SlashDotMenuNode`.
///
/// The listing node carries column definitions and field metadata;
/// the `ModelNode` child provides dynamic catalog rows.
pub fn build_model_node(app_state: &Arc<AppState>) -> crate::slashdot_menu::menu::SlashDotMenuNode {
    use crate::slashdot_menu::fields::{FieldSpec, SortDir};
    use crate::slashdot_menu::menu::SlashDotMenuNode;
    use crate::v25_listing_node::ListingNode;

    let desc_of = |name: &str| -> &'static str {
        crate::model_catalog::MODEL_MODIFIERS
            .iter()
            .find(|m| m.token == name)
            .map(|m| m.desc)
            .unwrap_or("")
    };

    let mut node: SlashDotMenuNode = ListingNode::new(
        app_state,
        &[StaticRow::new("model", "select a model (provider:name)")],
    )
    .with_columns(model_columns())
    .with_fields(vec![
        FieldSpec::new("context", true)
            .dir(SortDir::Asc)
            .description(desc_of("context")),
        FieldSpec::new("price_input", true)
            .dir(SortDir::Asc)
            .description(desc_of("price_input")),
        FieldSpec::new("price_output", true)
            .dir(SortDir::Asc)
            .description(desc_of("price_output")),
        FieldSpec::new("price", true)
            .dir(SortDir::Asc)
            .description(desc_of("price")),
        FieldSpec::new("coding", true)
            .dir(SortDir::Desc)
            .description(desc_of("coding")),
        FieldSpec::new("intelligence", true)
            .dir(SortDir::Desc)
            .description(desc_of("intelligence")),
        FieldSpec::new("agentic", true)
            .dir(SortDir::Desc)
            .description(desc_of("agentic")),
        FieldSpec::new("reasoning", true)
            .dir(SortDir::Desc)
            .description("reasoning capability"),
        FieldSpec::new("created", false)
            .dir(SortDir::Desc)
            .description("creation date"),
        FieldSpec::new("coding_price", true)
            .dir(SortDir::Desc)
            .description("coding_price"),
        FieldSpec::new("intelligence_price", true)
            .dir(SortDir::Desc)
            .description("intelligence_price"),
        FieldSpec::new("agentic_price", true)
            .dir(SortDir::Desc)
            .description("agentic_price"),
    ])
    .into();
    // Live session model: reads the runtime-target config, writes it, and
    // records the explicit user commit in one place.
    let reader = app_state.clone();
    let writer = app_state.clone();
    let mut model_menu: crate::slashdot_menu::menu::SlashDotMenuNode = ModelNode::new(
        Box::new(move || reader.model()),
        Box::new(move |model| {
            writer.set_model(model);
            Ok(())
        }),
        Some(app_state.clone()),
    )
    .into();
    add_model_actions(&mut model_menu, app_state, true, 1);
    node.add_child(model_menu);
    node
}

/// Attach the action rows to a model node.
///
/// `activate` goes first so it carries the initial cursor: `/model <id>` +
/// Enter still activates in one keystroke, exactly as it did before the
/// actions existed.
///
/// `with_save` is false for `/set default_model`, which reuses this same node:
/// there "activate" already writes the setting, and a default is not a live
/// model with params to save.
pub fn add_model_actions(
    node: &mut crate::slashdot_menu::menu::SlashDotMenuNode,
    app_state: &Arc<AppState>,
    with_save: bool,
    above: usize,
) {
    let writer = app_state.clone();
    node.add_child(
        ModelActionNode::activate(
            Some(app_state.clone()),
            Box::new(move |model| {
                writer.set_model(model);
                Ok(())
            }),
            above,
        )
        .into(),
    );
    if with_save {
        node.add_child(ModelActionNode::save(app_state.clone(), above).into());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Model actions — the children of a drilled `/model <id>`
// ---------------------------------------------------------------------------

/// One action on a drilled model. Both live in one node because they differ
/// only in the row they contribute and what Enter does; a type each would be
/// two near-identical `data()`s to keep in step.
pub struct ModelActionNode {
    action: ModelAction,
    /// Journal the model switch. `None` under `/set default_model`, where
    /// "activate" means "set as the default model" — a settings write, not a
    /// session event.
    live: Option<Arc<AppState>>,
    /// Write the chosen id on activate. `None` for `save`, which writes
    /// settings rather than a model id.
    activate_fn: Option<crate::SubmitFn>,
    /// The store handle `save` needs. `None` hides the row entirely: there is
    /// no meaningful "save live settings" under `/set default_model`.
    store: Option<Arc<AppState>>,
    /// Path segments above the model node — see [`drilled_model`]. 1 for the
    /// live `/model`, 2 for `/set default_model`.
    above: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelAction {
    Activate,
    Save,
}

impl ModelAction {
    fn id(self) -> &'static str {
        match self {
            ModelAction::Activate => "activate",
            ModelAction::Save => "save",
        }
    }

    fn desc(self) -> &'static str {
        match self {
            ModelAction::Activate => "use this model",
            ModelAction::Save => "save this model's live settings as its defaults",
        }
    }
}

impl ModelActionNode {
    pub fn activate(
        live: Option<Arc<AppState>>,
        activate_fn: crate::SubmitFn,
        above: usize,
    ) -> Self {
        ModelActionNode {
            action: ModelAction::Activate,
            live,
            activate_fn: Some(activate_fn),
            store: None,
            above,
        }
    }

    pub fn save(live: Arc<AppState>, above: usize) -> Self {
        ModelActionNode {
            action: ModelAction::Save,
            live: Some(live.clone()),
            activate_fn: None,
            store: Some(live),
            above,
        }
    }

    /// Persist a model's live params as its stored defaults.
    ///
    /// The source is the session's per-model memory, not `app.params()`, so
    /// `save` works for any model *used* this session rather than only the one
    /// running now: a → b → a restores a's runtime values, and this is how the
    /// user makes them durable without re-activating a first. For the current
    /// model the two are the same value by the A.6 invariant, so this widens
    /// the set of models `save` accepts and changes nothing for the one that
    /// was already accepted.
    ///
    /// Starts from the stored record rather than a fresh one, so `save` cannot
    /// wipe a metadata override the user set through Ctrl-E — it owns the three
    /// params, not the whole table (A.5).
    fn save_live(&self, model: &str) -> SubmitResult {
        let Some(app) = self.store.as_ref() else {
            return SubmitResult::Handled;
        };
        let Some(live) = app.model_param(model) else {
            return SubmitResult::Done(Some(format!(
                "{model} has not been used in this session, so it has no live settings to save"
            )));
        };
        // Only what differs from the built-in default: reasoning is never
        // `None` (it seeds from the global) so it is always written, while
        // temperature and top_p are written only when set, leaving a model the
        // user never touched without keys for them.
        let mut record = app
            .with_settings(|s| s.model_settings(model).cloned())
            .unwrap_or_default();
        record.reasoning = live.reasoning;
        record.temperature = live.temperature;
        record.top_p = live.top_p;
        let summary = crate::settings_editor::summarize(&record);
        match app.update_settings(|s| {
            s.apply_model_settings(model, &record);
            Ok(())
        }) {
            Ok(()) => SubmitResult::Done(Some(format!("saved {summary} for {model}"))),
            Err(e) => SubmitResult::Error(format!("cannot save settings: {e}")),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for ModelActionNode {
    /// One row, and only once a model is drilled. At the `/model` level these
    /// rows must not appear: every catalog row carrying the same two actions
    /// would make the picker unusable.
    async fn data(&mut self, _epoch: u64, path_args: &PathArgs) -> DataResult {
        if drilled_model(path_args, self.above).is_none() {
            return Ok(Rc::new(DataResponse {
                rows: Vec::new(),
                active_ids: Vec::new(),
            }));
        }
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new(self.action.id()).desc(self.action.desc())],
            active_ids: Vec::new(),
        }))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let Some(model) = drilled_model(path_args, self.above) else {
            return SubmitResult::Done(None);
        };
        match self.action {
            ModelAction::Activate => {
                let Some(write) = self.activate_fn.as_mut() else {
                    return SubmitResult::Handled;
                };
                match write(&model) {
                    Ok(()) => {
                        // Load-or-seed this model's live params. Here, on the
                        // action, and not inside `set_model`: `/set
                        // default_model` shares that writer but is not an
                        // activation, and seeding there would install the
                        // default model's record as the live params of
                        // whatever is actually running. Doing it before
                        // returning also means the loop's `params_changed`
                        // comparison already sees the final values, so the
                        // switch costs one rebuild rather than two.
                        if let Some(app) = self.live.as_ref() {
                            app.activate_model_params(&model);
                        }
                        // Journaled at the commit, not on rebuild: the explicit
                        // user action is what is worth recording, and the
                        // helper skips a value the session already holds, so
                        // re-commits stay idempotent.
                        if let Some(app) = self.live.as_ref() {
                            if let Some(s) = app.session() {
                                crate::agent::record_model_change(&s, &model).await;
                            }
                        }
                        SubmitResult::Done(None)
                    }
                    Err(msg) => SubmitResult::Done(Some(msg)),
                }
            }
            ModelAction::Save => self.save_live(&model),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(
        h: &mut crate::slashdot_menu::SlashDotHandle<'_>,
        prompt: &str,
        key: crate::slashdot_menu::HostKey,
    ) -> crate::slashdot_menu::node_impl::MenuResult {
        // Resolve ContinuePrompt like the host (repl::v25_eval) does.
        futures::executor::block_on(h.eval_prompt_resolved(prompt, key, 80))
    }

    fn test_app_state() -> Arc<AppState> {
        let settings = crate::settings::SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_reasoning_level: None,
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            models: Default::default(),
            path: None,
        };
        let app_state = crate::app_state::AppState::for_test(settings);
        app_state.set_model("anthropic:claude-3-5-sonnet-20241022");
        app_state
    }

    fn test_model_node(app_state: &Arc<AppState>) -> ModelNode {
        let reader = app_state.clone();
        let writer = app_state.clone();
        ModelNode::new(
            Box::new(move || reader.model()),
            Box::new(move |model| {
                writer.set_model(model);
                Ok(())
            }),
            Some(app_state.clone()),
        )
    }

    /// The *_price ratios floor the combined price at a tiny epsilon so free
    /// models (price == 0) sort ahead of paid ones, still in rating order.
    /// (Ported from the legacy bridge test `model_row_floors_price_for_free_models`.)
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
            ..Default::default()
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

    #[test]
    fn model_node_returns_rows() {
        let app_state = test_app_state();
        let mut node = test_model_node(&app_state);
        let path_args = PathArgs {
            ids: Vec::new(),
            suffix: String::new(),
            args: Default::default(),
        };
        let result = futures::executor::block_on(node.data(1, &path_args));
        assert!(result.is_ok(), "data() should not error");
    }

    #[test]
    fn model_node_caches_rc() {
        let app_state = test_app_state();
        let mut node = test_model_node(&app_state);
        let path_args = PathArgs {
            ids: Vec::new(),
            suffix: String::new(),
            args: Default::default(),
        };
        let r1 = futures::executor::block_on(node.data(1, &path_args)).unwrap();
        let r2 = futures::executor::block_on(node.data(1, &path_args)).unwrap();
        // Same generation → same Rc.
        assert!(Rc::ptr_eq(&r1, &r2));
    }

    #[test]
    fn model_node_columns() {
        let app_state = test_app_state();
        let node = test_model_node(&app_state);
        let cols = node.columns();
        assert_eq!(cols.len(), 5);
        assert!(cols.iter().any(|c| c.header == "$/m in"));
    }

    #[test]
    fn model_node_fields() {
        let app_state = test_app_state();
        let node = test_model_node(&app_state);
        let fields = node.fields();
        assert!(fields.iter().any(|f| f.name == "context"));
        assert!(fields.iter().any(|f| f.name == "price"));
        assert!(fields.iter().any(|f| f.name == "coding"));
    }

    #[test]
    fn model_node_submit_writes_settings() {
        let app_state = test_app_state();
        let mut node = test_model_node(&app_state);
        let path_args = PathArgs {
            ids: vec!["model".into(), "openai:gpt-4o".into()],
            suffix: " ".into(),
            args: Default::default(),
        };
        match futures::executor::block_on(node.submit(1, &path_args)) {
            SubmitResult::Done(_) => {}
            _ => panic!("expected Done"),
        }
        // Runtime-target config, not persistent settings.
        assert_eq!(app_state.model(), "openai:gpt-4o");
    }

    /// The live `/model` node journals its explicit commit at submit time
    /// (target-stamp: no `model_commit` register, no loop round-trip).
    #[test]
    fn model_submit_records_model_change_inline() {
        let app_state = test_app_state();
        let (_guard, session) = crate::agent::test_session_with_entries(vec![]);
        app_state.set_session(Some(session.clone()));
        let mut node = test_model_node(&app_state);
        let path_args = PathArgs {
            ids: vec!["model".into(), "openai:gpt-4o".into()],
            suffix: " ".into(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(matches!(result, SubmitResult::Done(None)));
        let recorded = crate::agent::block_on_test(async {
            let idx = session.index().lock().await;
            idx.entries.values().any(|e| {
                matches!(
                    &e.content,
                    crate::session::SessionEntryContent::ModelChange { provider, model_id }
                        if provider == "openai" && model_id == "gpt-4o"
                )
            })
        });
        assert!(recorded, "the commit must land in the journal inline");
    }

    fn build_test_menu(app_state: &Arc<AppState>) -> crate::slashdot_menu::SlashDotMenu {
        use crate::slashdot_menu::node_impl::StaticListingNode;
        use crate::slashdot_menu::SlashDotMenuNode;
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        let mut ml: SlashDotMenuNode =
            StaticListingNode::new(&[StaticRow::new("model", "select a model")]).into();
        ml.add_child(test_model_node(app_state).into());
        root.add_child(ml);
        crate::slashdot_menu::SlashDotMenu::from_root_node(root)
    }

    /// Full menu traversal: root listing → drill "model" → catalog rows →
    /// Tab into a catalog entry → Enter submits.
    #[test]
    fn model_full_traversal() {
        let app_state = test_app_state();
        let mut menu = build_test_menu(&app_state);

        let mut h = menu.get_handle();
        // Root listing: shows the "model" row.
        match eval(&mut h, "", crate::slashdot_menu::HostKey::None) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
                assert!(
                    row_texts.iter().any(|t| t.contains("model")),
                    "root should show model row"
                );
            }
            other => panic!("expected Continue at root, got {:?}", other),
        }
        // Tab drills into the model listing.
        match eval(&mut h, "", crate::slashdot_menu::HostKey::Tab) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                assert!(
                    prompt.starts_with("model"),
                    "after Tab should drill into model, got: {}",
                    prompt
                );
            }
            other => panic!("expected Continue after Tab drill, got {:?}", other),
        }
        // If catalog has entries, Tab again drills into a model, Enter submits.
        let model_listing = eval(&mut h, "model ", crate::slashdot_menu::HostKey::None);
        if let crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) = model_listing {
            if !plan.rows.is_empty() {
                match eval(&mut h, "model ", crate::slashdot_menu::HostKey::Tab) {
                    crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                        assert!(
                            prompt.starts_with("model "),
                            "should drill into a model entry, got: {}",
                            prompt
                        );
                        match eval(&mut h, &prompt, crate::slashdot_menu::HostKey::Enter) {
                            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {
                                let model = app_state.model();
                                assert!(
                                    !model.is_empty(),
                                    "target config should have a model after submit"
                                );
                            }
                            other => panic!("expected DoneSuccess on model Enter, got {:?}", other),
                        }
                    }
                    other => panic!("expected Continue on model Tab, got {:?}", other),
                }
            }
        }
    }

    /// Model fuzzy filter: typing a partial model name narrows the listing.
    #[test]
    fn model_fuzzy_filter() {
        let app_state = test_app_state();
        let mut menu = build_test_menu(&app_state);

        let mut h = menu.get_handle();
        // Unfiltered count.
        let total = match eval(&mut h, "model ", crate::slashdot_menu::HostKey::None) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => plan.rows.len(),
            other => panic!("expected Continue, got {:?}", other),
        };
        // Filtered count must be strictly less.
        match eval(&mut h, "model deep", crate::slashdot_menu::HostKey::None) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                let filtered = plan.rows.len();
                assert!(
                    filtered < total,
                    "filter 'deep' should narrow results: {} >= {}",
                    filtered,
                    total
                );
            }
            other => panic!("expected Continue on fuzzy filter, got {:?}", other),
        }
    }

    /// Eval-level gate for the relocated dotted-modifier completion
    /// (`node_entry::default_complete`): a dot-prefixed suffix inside
    /// `/model` opens the modifier dialog with the injected `sort` key
    /// plus the model field keys.
    #[test]
    fn model_dot_modifier_opens_completion_dialog() {
        let app_state = test_app_state();
        let mut menu = build_test_menu(&app_state);
        let mut h = menu.get_handle();
        match eval(&mut h, "model .", crate::slashdot_menu::HostKey::None) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                // Dialog discriminator: candidates render with the fixed
                // Modifier/Description columns (the listing has none).
                assert!(
                    plan.table_header.is_some(),
                    "dot word must open the modifier dialog"
                );
                let rows: Vec<String> = plan.rows.iter().map(|l| l.0.clone()).collect();
                assert!(
                    rows.iter().any(|r| r.contains(".sort:")),
                    "sort key must be offered: {rows:?}"
                );
                // Keys at this level are inferred from the merged dataset
                // rows (declared-spec completion for row-less fields is the
                // arg/modifier rework track): context/price are present on
                // every catalog row.
                assert!(
                    rows.iter().any(|r| r.contains(".context:")),
                    ".context: must be offered: {rows:?}"
                );
                assert!(
                    rows.iter().any(|r| r.contains(".price:")),
                    ".price: must be offered: {rows:?}"
                );
            }
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    // ---- P5: the action listing, header, and two-phase Ctrl-E ----

    /// A model that is present in the test catalog. Only providers with no API
    /// key survive `static_fallback`, so this cannot assume a real one.
    const TEST_MODEL: &str = "ollama:llama3";

    fn model_menu(app_state: &Arc<AppState>) -> crate::slashdot_menu::SlashDotMenu {
        crate::v25_menu::build_menu(app_state)
    }

    fn rows_at(
        h: &mut crate::slashdot_menu::SlashDotHandle<'_>,
        prompt: &str,
        key: crate::slashdot_menu::HostKey,
    ) -> (String, Vec<String>) {
        match futures::executor::block_on(h.eval_prompt_resolved(prompt, key, 80)) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, p) => (
                p,
                plan.rows
                    .iter()
                    .map(|l| l.0.split_whitespace().next().unwrap_or("").to_string())
                    .collect(),
            ),
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    /// The `*` follows the active model **through a reused handle** — the case
    /// the cache key got wrong.
    ///
    /// `data()` used to reuse its `Rc<DataResponse>` whenever the *catalog
    /// generation* was unchanged, but the rows also depend on the active model
    /// (hoisted to row 0, and named in `active_ids`) and on recency. Switching
    /// the model changed neither, so the stale `Rc` came back — and because
    /// `refresh_data` gates on `Rc::ptr_eq`, that stale response also stopped
    /// `content.active_ids` and `slots_dirty` from refreshing, so `ensure_rows`
    /// returned a stale view as well. The marker stayed on the *previous*
    /// model, which row 0 still held, so it looked plausible rather than
    /// obviously broken.
    ///
    /// Reads a model off the listing rather than hardcoding one, so it holds
    /// whatever the catalog happens to contain.
    #[test]
    fn the_active_marker_follows_a_model_switch_on_a_reused_handle() {
        use crate::slashdot_menu::node_impl::MenuResult;
        use crate::slashdot_menu::HostKey;

        let app_state = test_app_state();
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        let eval = |h: &mut crate::slashdot_menu::SlashDotHandle<'_>| {
            futures::executor::block_on(h.eval_prompt_resolved("model ", HostKey::None, 80))
        };
        let plan = |out: crate::slashdot_menu::node_impl::MenuResult| match out {
            MenuResult::Continue(p, _) => p,
            other => panic!("expected Continue, got {other:?}"),
        };

        // Prime the cache with an active model that is *not* in the catalog,
        // so nothing is starred — the state the bug was stuck in.
        app_state.set_model("slashdot:nonexistent-model");
        let first = plan(eval(&mut h));
        let ids: Vec<String> = first
            .rows
            .iter()
            .map(|l| l.0.split_whitespace().next().unwrap().to_string())
            .collect();
        assert!(!ids.contains(&"slashdot:nonexistent-model".to_string()));
        assert_eq!(
            first.active_rows,
            Vec::<usize>::new(),
            "nothing is active yet"
        );

        // Switch to a model that *is* in the catalog, same handle.
        let target = ids.first().cloned().expect("catalog has a model");
        app_state.set_model(&target);
        let second = plan(eval(&mut h));
        let want = second
            .rows
            .iter()
            .position(|l| l.0.split_whitespace().next() == Some(target.as_str()))
            .expect("target is listed");
        assert_eq!(
            second.active_rows,
            vec![want],
            "the marker must move to the newly active model"
        );

        // And the `*` is actually painted on that row.
        let els = crate::ui::overlay_elements(&second, 80, second.selected);
        let extents: Vec<crate::ui::frame::Extent> =
            els.iter().map(crate::ui::frame::Element::extent).collect();
        let heights = crate::ui::frame::allocate(&extents, crate::ui::frame::live_budget(24));
        let f = crate::ui::frame::render(&els, &heights, 80);
        let starred: Vec<bool> = (0..f.height()).map(|i| f.row(i).contains('*')).collect();
        assert_eq!(
            starred.iter().filter(|s| **s).count(),
            1,
            "exactly one row is starred: {starred:?}"
        );

        // Switch away again — the marker must go with it.
        app_state.set_model("slashdot:nonexistent-model");
        let third = plan(eval(&mut h));
        assert_eq!(
            third.active_rows,
            Vec::<usize>::new(),
            "the marker must clear when nothing in the catalog is active"
        );
    }

    /// At the top level the listing is the catalog, not the actions — every
    /// model row carrying `activate`/`save` would make it unusable.
    #[test]
    fn the_top_level_listing_shows_models_not_actions() {
        let app_state = test_app_state();
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        // A passive key, not Tab: Tab *drills* into the first catalog row and
        // lands on the actions, which is the next level's job.
        let (_, rows) = rows_at(&mut h, "model ", crate::slashdot_menu::HostKey::None);
        assert!(
            !rows.is_empty(),
            "the catalog must list models; got {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r == "activate" || r == "save"),
            "the top level is the catalog, not the actions: {rows:?}"
        );
    }

    /// Drilling a model offers its actions, and typing the id reaches the same
    /// place as a Tab drill — the id is consumed as a path segment, not
    /// fuzzy-filtered against the action rows.
    #[test]
    fn a_drill_offers_activate_and_save() {
        let app_state = test_app_state();
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        let prompt = format!("model {TEST_MODEL} ");
        let (_, rows) = rows_at(&mut h, &prompt, crate::slashdot_menu::HostKey::None);
        assert_eq!(rows, vec!["activate".to_string(), "save".to_string()]);
    }

    /// The one-keystroke path is preserved: the cursor starts on `activate`, so
    /// `/model <id>` + Enter switches model exactly as it did before the
    /// actions existed.
    #[test]
    fn enter_on_the_drill_activates_the_model() {
        let app_state = test_app_state();
        app_state.set_model("anthropic:something-else");
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        let prompt = format!("model {TEST_MODEL} ");
        match futures::executor::block_on(h.eval_prompt_resolved(
            &prompt,
            crate::slashdot_menu::HostKey::Enter,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {}
            _ => panic!("expected the activate action to commit"),
        }
        assert_eq!(app_state.model(), TEST_MODEL);
    }

    fn settings_store(app_state: &Arc<AppState>) -> crate::settings::SettingsStore {
        app_state.with_settings(|s| s.clone())
    }

    /// `save` copies the live params into the stored record.
    #[test]
    fn save_writes_the_live_params() {
        let app_state = test_app_state();
        app_state.set_model(TEST_MODEL);
        app_state.set_param(|p| {
            p.temperature = Some(0.4);
            p.top_p = Some(0.9);
        });
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        // Drill to the model, move down from `activate` to `save`, commit.
        let prompt = format!("model {TEST_MODEL} ");
        let _ = futures::executor::block_on(h.eval_prompt_resolved(
            &prompt,
            crate::slashdot_menu::HostKey::None,
            80,
        ));
        match futures::executor::block_on(h.eval_prompt_resolved(
            &prompt,
            crate::slashdot_menu::HostKey::MoveCursor(1),
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, saved_prompt) => {
                match futures::executor::block_on(h.eval_prompt_resolved(
                    &saved_prompt,
                    crate::slashdot_menu::HostKey::Enter,
                    80,
                )) {
                    crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {}
                    _ => panic!("expected save to commit"),
                }
            }
            _ => panic!("expected the cursor move to stay open"),
        }
        let stored = settings_store(&app_state)
            .model_settings(TEST_MODEL)
            .cloned()
            .expect("save must write a record");
        assert_eq!(stored.temperature, Some(0.4));
        assert_eq!(stored.top_p, Some(0.9));
    }

    /// `save` owns the three params, not the whole table: a metadata override
    /// the user set through Ctrl-E must survive it.
    #[test]
    fn save_preserves_stored_metadata() {
        let app_state = test_app_state();
        app_state.set_model(TEST_MODEL);
        app_state.set_param(|p| p.temperature = Some(0.3));
        app_state
            .update_settings(|s| {
                s.apply_model_settings(
                    TEST_MODEL,
                    &crate::model_settings::ModelSettings {
                        context_length: Some(4096),
                        temperature: Some(0.1),
                        ..Default::default()
                    },
                );
                Ok(())
            })
            .unwrap();
        let mut node = ModelActionNode::save(app_state.clone(), 1);
        let pa = PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " ");
        let outcome = futures::executor::block_on(node.submit(1, &pa));
        assert!(matches!(outcome, SubmitResult::Done(..)));
        let stored = settings_store(&app_state)
            .model_settings(TEST_MODEL)
            .cloned()
            .unwrap();
        assert_eq!(stored.temperature, Some(0.3), "live wins for params");
        assert_eq!(stored.context_length, Some(4096), "metadata untouched");
    }

    /// A model never used this session has no live params, and saying so beats
    /// writing an empty record. The message names the fix, since the cause is
    /// no longer "it isn't the active model" — activating it is.
    #[test]
    fn save_on_an_unvisited_model_explains_itself() {
        let app_state = test_app_state();
        app_state.set_model("anthropic:not-this-one");
        let mut node = ModelActionNode::save(app_state.clone(), 1);
        let pa = PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " ");
        match futures::executor::block_on(node.submit(1, &pa)) {
            SubmitResult::Done(Some(msg)) => {
                assert!(msg.contains("has not been used in this session"), "{msg}");
            }
            _ => panic!("expected Done with an explanation"),
        }
        assert!(
            settings_store(&app_state)
                .model_settings(TEST_MODEL)
                .is_none(),
            "nothing must be written for a model with no live settings"
        );
    }

    /// A model that was used earlier in the session is saveable while another
    /// model is running: the source is the session's per-model memory, so
    /// a → b → a's runtime values can be made durable without re-activating a.
    #[test]
    fn save_works_for_a_model_visited_earlier_in_the_session() {
        let app_state = test_app_state();
        // Use the model, change it, then move on to another.
        app_state.set_model(TEST_MODEL);
        app_state.activate_model_params(TEST_MODEL);
        app_state.set_param(|p| p.temperature = Some(0.35));
        app_state.set_model("anthropic:not-this-one");
        assert_ne!(
            app_state.model(),
            TEST_MODEL,
            "precondition: it is not current"
        );

        let mut node = ModelActionNode::save(app_state.clone(), 1);
        let pa = PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " ");
        assert!(matches!(
            futures::executor::block_on(node.submit(1, &pa)),
            SubmitResult::Done(..)
        ));
        let stored = settings_store(&app_state)
            .model_settings(TEST_MODEL)
            .cloned()
            .expect("a model visited earlier must be saveable");
        assert_eq!(
            stored.temperature,
            Some(0.35),
            "the values it was running with, not the current model's"
        );
    }

    /// `/set default_model` reuses the node, so it offers `activate` but not
    /// `save` — there are no live params for a default.
    #[test]
    fn the_set_default_model_node_offers_activate_but_not_save() {
        let app_state = test_app_state();
        // Through the real root menu: mounting the `/set` subtree as the root
        // of a test tree would drop the leading `set` segment from the path,
        // which is exactly what the depth argument keys on.
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        let (_, rows) = rows_at(
            &mut h,
            &format!("set default_model {TEST_MODEL} "),
            crate::slashdot_menu::HostKey::None,
        );
        assert_eq!(rows, vec!["activate".to_string()], "no save under /set");
    }

    /// The header names the model, its published facts, and — the part that
    /// matters — what it is set to right now.
    #[test]
    fn the_header_shows_details_and_current_settings() {
        let app_state = test_app_state();
        app_state.set_model(TEST_MODEL);
        app_state.set_param(|p| p.temperature = Some(0.6));
        let reader = app_state.clone();
        let writer = app_state.clone();
        let mut node = ModelNode::new(
            Box::new(move || reader.model()),
            Box::new(move |m| {
                writer.set_model(m);
                Ok(())
            }),
            Some(app_state.clone()),
        );
        let lines = futures::executor::block_on(node.header(
            &PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " "),
            &RenderContext { term_width: 80 },
        ));
        let joined = lines.join("\n");
        assert!(joined.contains(TEST_MODEL), "{joined}");
        assert!(joined.contains("ctx"), "{joined}");
        assert!(joined.contains("now: reasoning"), "{joined}");
        assert!(joined.contains("0.6"), "the live value must show: {joined}");
        assert!(joined.contains("ctrl-e"), "{joined}");
    }

    /// An inactive model has no live settings, so the header reports the
    /// stored defaults — which is the question "what would using this do".
    #[test]
    fn the_header_reports_stored_defaults_for_an_inactive_model() {
        let app_state = test_app_state();
        app_state.set_model("anthropic:another");
        let reader = app_state.clone();
        let mut node = ModelNode::new(
            Box::new(move || reader.model()),
            Box::new(|_| Ok(())),
            Some(app_state.clone()),
        );
        let lines = futures::executor::block_on(node.header(
            &PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " "),
            &RenderContext { term_width: 80 },
        ));
        let joined = lines.join("\n");
        assert!(joined.contains("defaults: not configured"), "{joined}");
    }

    /// Without a tty the editor cannot run, and that must be an error on the
    /// frame — not a silent success that looks like it saved.
    #[test]
    fn ctrl_e_without_a_tty_reports_instead_of_writing() {
        let app_state = test_app_state();
        let reader = app_state.clone();
        let writer = app_state.clone();
        let mut node = ModelNode::new(
            Box::new(move || reader.model()),
            Box::new(move |m| {
                writer.set_model(m);
                Ok(())
            }),
            Some(app_state.clone()),
        );
        let outcome = futures::executor::block_on(node.edit(
            1,
            &PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " "),
        ));
        match outcome {
            SubmitResult::Error(msg) => assert!(msg.contains("cannot edit"), "{msg}"),
            _ => panic!("expected an error without a tty"),
        }
        assert!(
            settings_store(&app_state)
                .model_settings(TEST_MODEL)
                .is_none(),
            "a failed edit must not write"
        );
    }

    /// The accept arm of the confirm calls `submit()` on the node that armed
    /// it, so the stashed record is what `submit()` commits.
    #[test]
    fn submit_commits_the_stashed_ctrl_e_edit() {
        let app_state = test_app_state();
        let reader = app_state.clone();
        let writer = app_state.clone();
        let mut node = ModelNode::new(
            Box::new(move || reader.model()),
            Box::new(move |m| {
                writer.set_model(m);
                Ok(())
            }),
            Some(app_state.clone()),
        );
        *node.pending_edit.borrow_mut() = Some(PendingEdit {
            epoch: 7,
            model: TEST_MODEL.to_string(),
            settings: crate::model_settings::ModelSettings {
                temperature: Some(0.25),
                ..Default::default()
            },
        });
        let pa = PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " ");
        let outcome = futures::executor::block_on(node.submit(7, &pa));
        match outcome {
            SubmitResult::Done(Some(msg)) => assert!(msg.contains(TEST_MODEL), "{msg}"),
            _ => panic!("expected Done with a message"),
        }
        let stored = settings_store(&app_state)
            .model_settings(TEST_MODEL)
            .cloned()
            .expect("the stashed record must be written");
        assert_eq!(stored.temperature, Some(0.25));
        // Stash consumed, so a second Enter cannot write it twice.
        assert!(node.pending_edit.borrow().is_none());
    }

    /// A stash from another epoch means the data moved under us; writing it
    /// would store settings against a model the user is no longer looking at.
    #[test]
    fn submit_refuses_a_stale_stash() {
        let app_state = test_app_state();
        let reader = app_state.clone();
        let mut node = ModelNode::new(
            Box::new(move || reader.model()),
            Box::new(|_| Ok(())),
            Some(app_state.clone()),
        );
        *node.pending_edit.borrow_mut() = Some(PendingEdit {
            epoch: 3,
            model: TEST_MODEL.to_string(),
            settings: crate::model_settings::ModelSettings {
                temperature: Some(0.25),
                ..Default::default()
            },
        });
        let pa = PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " ");
        match futures::executor::block_on(node.submit(9, &pa)) {
            SubmitResult::Error(msg) => assert!(msg.contains("nothing was saved"), "{msg}"),
            _ => panic!("expected a refusal"),
        }
        assert!(settings_store(&app_state)
            .model_settings(TEST_MODEL)
            .is_none());
    }

    /// With nothing stashed, `submit()` is still the activate path — the
    /// legacy behaviour the action listing must not break.
    #[test]
    fn submit_without_a_stash_still_activates() {
        let app_state = test_app_state();
        let reader = app_state.clone();
        let writer = app_state.clone();
        let mut node = ModelNode::new(
            Box::new(move || reader.model()),
            Box::new(move |m| {
                writer.set_model(m);
                Ok(())
            }),
            Some(app_state.clone()),
        );
        let pa = PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " ");
        let outcome = futures::executor::block_on(node.submit(1, &pa));
        assert!(matches!(outcome, SubmitResult::Done(None)));
        assert_eq!(app_state.model(), TEST_MODEL);
    }

    /// The whole reported path, through the menu: `/model <id> save` writes the
    /// record, then a *new* app state activating that model starts at the
    /// stored level rather than the global's. The second half is the bug —
    /// before the fix the record was written and never read.
    #[test]
    fn saving_then_restarting_seeds_the_stored_level() {
        // Session 1: activate, set a level, save. The global is set first so
        // the assertion below is unambiguous — a record that lost to it would
        // read "low".
        let app = test_app_state();
        app.update_settings(|s| {
            s.default_reasoning_level = Some("low".to_string());
            Ok(())
        })
        .unwrap();
        app.set_model(TEST_MODEL);
        app.activate_model_params(TEST_MODEL);
        app.set_reasoning(Some(crate::params::ReasoningLevel::Max));
        let mut node = ModelActionNode::save(app.clone(), 1);
        let pa = PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " ");
        assert!(matches!(
            futures::executor::block_on(node.submit(1, &pa)),
            SubmitResult::Done(..)
        ));
        let persisted = app
            .with_settings(|s| s.model_settings(TEST_MODEL).cloned())
            .expect("save must have written the record");
        assert_eq!(
            persisted.reasoning,
            Some(crate::params::ReasoningLevel::Max)
        );

        // Session 2: a fresh app over the same store seeds from that record.
        let store2 = app.with_settings(|s| s.clone());
        let app2 = AppState::for_test(store2);
        app2.activate_model_params(TEST_MODEL);
        assert_eq!(
            app2.params().reasoning,
            Some(crate::params::ReasoningLevel::Max),
            "a new session must start at the model's stored level, not the global's"
        );
    }

    /// The reported bug: a stored override of cost showed in the `/model <id>`
    /// header but not in the listing, because the listing read the raw catalog
    /// while the header read the resolved view. Both read `resolved_models` now.
    #[test]
    fn the_listing_rows_carry_a_stored_override() {
        let app_state = test_app_state();
        // Pick a model the test catalog actually offers, then override its
        // context window and check the row reflects it.
        let (provider, model) = match app_state
            .resolved_models()
            .first()
            .map(|e| e.qualified_parts())
        {
            Some(p) => p,
            None => return, // no catalog in this environment
        };
        let qualified = format!("{provider}:{model}");
        app_state
            .update_settings(|s| {
                s.apply_model_settings(
                    &qualified,
                    &crate::model_settings::ModelSettings {
                        context_length: Some(777_777),
                        ..Default::default()
                    },
                );
                Ok(())
            })
            .unwrap();

        let mut node = test_model_node(&app_state);
        let pa = PathArgs::new(vec!["model".into()], " ");
        let response = futures::executor::block_on(node.data(1, &pa)).unwrap();
        let row = response
            .rows
            .iter()
            .find(|r| r.id == qualified)
            .unwrap_or_else(|| panic!("{qualified} must be listed"));
        assert_eq!(
            row.field_value("context").and_then(|v| v.as_num()),
            Some(777_777.0),
            "the row must carry the override, not the catalog's number"
        );
    }
}
