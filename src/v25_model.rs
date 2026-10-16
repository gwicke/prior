//! v2.5 `SlashDotNode` implementation for `/model`.
//!
//! Dynamic rows from the live catalog (cached, background-refreshed). The
//! node caches its `Arc<DataResponse>` keyed by the catalog's generation
//! counter — when the catalog hasn't refreshed, the engine's `Arc::ptr_eq`
//! check skips the full re-derive (filter → fuzzy → sort → render).
use std::sync::Arc;

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
    /// Last generated DataResponse, reused via `Arc::ptr_eq` when nothing the rows
    /// derive from has changed. The three fields below *are* that condition —
    /// `data()` reuses only while all of them still hold.
    cached_response: Option<Arc<DataResponse>>,
    /// Catalog generation at build time: the entry set and its base order.
    cached_generation: u64,
    /// The active model id at build time. It is both hoisted to row 0 by
    /// [`order_models`] and named in `active_ids`, so a switch must rebuild.
    cached_active: String,
    /// Recently-used models at build time: they fill the rows after the
    /// active one, so a change reorders the listing and moves the active
    /// row's index.
    cached_recent: Vec<String>,
    /// The resolved entry list the rows were built from.
    ///
    /// Held so the memo can compare *identity* rather than a revision counter:
    /// `AppState::resolved_models` hands back a fresh `Arc` exactly when the
    /// overlay was rebuilt (a catalog fetch or a settings write), so
    /// `Arc::ptr_eq` is the invalidation signal, and holding the `Arc` is what
    /// keeps it alive to compare against. Without this the rows outlived a
    /// Ctrl-E edit of a per-model record — the generation, the active model and
    /// the recency list are all unchanged by such an edit, so the memo missed
    /// and, because `refresh_data` gates on `Arc::ptr_eq`, the view never
    /// refreshed either.
    cached_resolved: Option<Arc<Vec<crate::model_catalog::ModelEntry>>>,
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

/// Lay out `label value` pairs onto as few lines as `width` allows.
///
/// Greedy and width-driven rather than hand-grouped: a fixed grouping has to
/// pick a width to design for, and it is wrong everywhere else — too wide on an
/// 80-column window, wasting the columns the facts could have used, and too
/// narrow on a wide one, truncating instead of fitting. Here a pair is never
/// split across lines, so each one stays readable, and the only thing that
/// changes with the terminal is how many land on a line.
fn pack_facts(facts: &[(String, String)], indent: &str, width: usize) -> Vec<String> {
    const GAP: usize = 3;
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::from(indent);
    for (label, value) in facts {
        let piece = format!("{label} {value}");
        let candidate_len = crate::slashdot_menu::table::display_width(&current)
            + GAP
            + crate::slashdot_menu::table::display_width(&piece);
        if !current.trim_start_matches(' ').is_empty() && candidate_len > width {
            lines.push(std::mem::take(&mut current));
            current = indent.to_string();
        }
        if current.len() > indent.len() {
            current.push_str(&" ".repeat(GAP));
        }
        current.push_str(&piece);
    }
    if current.trim_start_matches(' ').len() > indent.len() {
        lines.push(current);
    }
    lines
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
            cached_resolved: None,
            pending_edit: std::cell::RefCell::new(None),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for ModelNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        // Read every input *above* the early return: all four decide reuse, and
        // reading them once is cheaper than reading them on a miss and again
        // here. `resolved_models` is itself memoized on (generation, models
        // revision), so this is a cheap identity read in the common case.
        let gen = crate::model_catalog::catalog_generation();
        let active_id = (self.active_fn)();
        let recent: Vec<String> = self
            .live
            .as_ref()
            .map(|r| r.recent_models())
            .unwrap_or_default();
        let resolved = self.live.as_ref().map(|r| r.resolved_models());

        // Reuse only while every input the rows derive from still holds. The
        // generation is not enough on its own: `order_models` also hoists the
        // *active* model and boosts recently-used ones, and `active_ids` is
        // what the `*` marker lands on. Keying on the generation alone served
        // a stale `active_ids` after a model switch — and because
        // `refresh_data` gates on `Arc::ptr_eq`, the stale `Arc` also stopped
        // `content.active_ids` and `slots_dirty` from refreshing, so the memo
        // in `ensure_rows` returned the stale view too. The marker then sat on
        // the *previous* active model, which `order_models` had hoisted to row
        // 0 — stale, but self-consistent enough to look like a stray marker
        // rather than a missing one.
        if let Some(ref cached) = self.cached_response {
            let overlay_same = match (&self.cached_resolved, &resolved) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            };
            if self.cached_generation == gen
                && self.cached_active == active_id
                && self.cached_recent == recent
                && overlay_same
            {
                return Ok(Arc::clone(cached));
            }
        }

        // An input changed (or first call) — rebuild rows.
        //
        // The *resolved* list, so a Ctrl-E'd override of cost or an index shows
        // in the row rather than only in the `/model <id>` header. `live` is
        // `None` under `/set default_model`, which has no store to read, so
        // that node keeps the raw catalog — the same trade it already makes
        // for recency.
        let entries: Vec<crate::model_catalog::ModelEntry> = match &resolved {
            Some(list) => list.as_ref().clone(),
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
        let response = Arc::new(DataResponse { rows, active_ids });
        self.cached_response = Some(Arc::clone(&response));
        self.cached_generation = gen;
        self.cached_active = active_id;
        self.cached_recent = recent;
        self.cached_resolved = resolved;
        Ok(response)
    }

    /// What this model is, and what it is set to right now.
    ///
    /// The resolved view — catalog overlaid with the stored per-model
    /// overrides — so these are the figures the app actually uses rather than
    /// what OpenRouter last published. Absent facts read as absent.
    ///
    /// Laid out by *packing* rather than by fixed positions: the facts are
    /// built as `label value` pairs and wrapped to whatever the terminal has, so
    /// an 80-column window shows most of them on one line and a narrow one
    /// gives way field by field rather than truncating. Grouping them by hand
    /// meant the widest case overflowed while narrow ones wasted the width.
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
        let indent = "  ";
        let width = render_ctx.term_width.max(40);
        let mut lines = vec![format!("model: {model}")];

        // Facts, in the order they answer questions: how big, what it costs,
        // how good, what it can do.
        let mut facts: Vec<(String, String)> = Vec::new();
        if let Some(e) = entry.as_ref() {
            facts.push((
                "ctx".to_string(),
                e.context_length
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "—".into()),
            ));
            // `in`/`out` rather than a bare `$p/$c`: which is which is the part
            // a reader has to guess otherwise.
            facts.push((
                "in".to_string(),
                e.prompt_cost
                    .map(|c| format!("${c:.2}/M"))
                    .unwrap_or_else(|| "—".into()),
            ));
            facts.push((
                "out".to_string(),
                e.completion_cost
                    .map(|c| format!("${c:.2}/M"))
                    .unwrap_or_else(|| "—".into()),
            ));
            let idx = |v: Option<f32>| v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "—".into());
            facts.push(("coding".to_string(), idx(e.coding_index)));
            facts.push(("intel".to_string(), idx(e.intelligence_index)));
            facts.push(("agentic".to_string(), idx(e.agentic_index)));
            facts.push((
                "efforts".to_string(),
                if e.reasoning_levels.is_empty() {
                    "not published".to_string()
                } else {
                    e.reasoning_levels.join(" ")
                },
            ));
        }
        if facts.is_empty() {
            facts.push(("catalog".to_string(), "no entry".to_string()));
        }
        lines.extend(pack_facts(&facts, indent, width));

        // The description is OpenRouter's prose and is kept in full: it is the
        // only place the model's own summary appears anywhere in the app. It
        // comes after the facts because the facts are what you came for, and
        // the summary is what you read if one of them was unclear.
        if let Some(d) = entry.as_ref().and_then(|e| e.description.as_deref()) {
            lines.extend(
                crate::content::text::wrap(d, width.saturating_sub(indent.len()).max(20))
                    .into_iter()
                    .map(|l| format!("{indent}{l}")),
            );
        }

        // Current settings: live for the active model, stored for an inactive
        // one, since "what would using this do" is the question being asked.
        if let Some(app) = self.live.as_ref() {
            if app.model() == model {
                let p = app.params();
                let mut live: Vec<(String, String)> = vec![(
                    "reasoning".to_string(),
                    p.reasoning
                        .map(|l| l.as_str().to_string())
                        .unwrap_or_else(|| "off".into()),
                )];
                live.push(("temperature".to_string(), fmt_opt(p.temperature)));
                live.push(("top_p".to_string(), fmt_opt(p.top_p)));
                let mut packed = pack_facts(&live, indent, width);
                if let Some(first) = packed.first_mut() {
                    first.insert_str(indent.len(), "now ");
                }
                lines.extend(packed);
            } else {
                let summary = stored
                    .as_ref()
                    .map(crate::settings_editor::summarize)
                    .unwrap_or_else(|| "not configured".to_string());
                lines.extend(
                    crate::content::text::wrap(
                        &summary,
                        width.saturating_sub(indent.len() + 7).max(20),
                    )
                    .into_iter()
                    .map(|l| format!("{indent}defaults: {l}")),
                );
            }
            lines.push(format!("{indent}ctrl-e to edit these settings"));
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
    add_live_model_actions(&mut model_menu, app_state);
    node.add_child(model_menu);
    node
}

/// Attach the live `/model` action rows to a model node.
///
/// `activate` goes first so it carries the initial cursor: `/model <id>` +
/// Enter still activates in one keystroke, exactly as it did before the
/// actions existed.
///
/// Only the live mount. `/set default_model` reuses the catalog listing but
/// brings its own leaf ([`DefaultModelActivateNode`]): there "activate" writes
/// a setting rather than switching the running model.
fn add_live_model_actions(
    node: &mut crate::slashdot_menu::menu::SlashDotMenuNode,
    app_state: &Arc<AppState>,
) {
    let writer = app_state.clone();
    node.add_child(
        ModelActionNode::activate(
            Some(app_state.clone()),
            Box::new(move |model| {
                writer.set_model(model);
                Ok(())
            }),
            1,
        )
        .into(),
    );
    node.add_child(ModelActionNode::save(app_state.clone(), 1).into());
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
            return Ok(Arc::new(DataResponse {
                rows: Vec::new(),
                active_ids: Vec::new(),
            }));
        }
        Ok(Arc::new(DataResponse {
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
                // The optional level is parsed before anything is touched: a
                // level that is not one leaves the model un-switched, and
                // `Error` keeps the menu open with the bad word still in the
                // buffer to edit rather than discarding the whole line.
                let level = match path_args.suffix.trim() {
                    "" => None,
                    raw => match raw.parse() {
                        Ok(level) => Some(level),
                        Err(msg) => return SubmitResult::Error(msg),
                    },
                };
                match write(&model) {
                    Ok(()) => {
                        // Load-or-seed this model's live params. Here, on the
                        // action, and not inside `set_model`: nothing else
                        // switches models, and seeding there would install
                        // the switched model's record as the live params of
                        // whatever is actually running. Doing it before
                        // returning also means the loop's `params_changed`
                        // comparison already sees the final values, so the
                        // switch costs one rebuild rather than two.
                        if let Some(app) = self.live.as_ref() {
                            app.activate_model_params(&model);
                        }
                        // After the seed, never before it: activation installs
                        // the model's own level, and the level the user typed
                        // is the one they want *for this switch*. Setting it
                        // first would have the seed overwrite it.
                        if let (Some(app), Some(level)) = (self.live.as_ref(), level) {
                            app.set_reasoning(Some(level));
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

    /// What `activate` does, and the one thing it takes beyond the model id.
    ///
    /// Only on the leaf (`/model <id> activate `), which is the only level
    /// where the argument exists: at the action listing there is nothing typed
    /// yet, and a header advertising a parameter would be describing a prompt
    /// the user is not on yet.
    async fn header(&mut self, path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        if self.action != ModelAction::Activate {
            return Vec::new();
        }
        let Some(model) = drilled_model(path_args, self.above) else {
            return Vec::new();
        };
        // The ladder comes from `/reasoning` itself, so the two can never
        // disagree about which levels exist.
        let ladder = crate::v25_reasoning::ReasoningEnumNode::LEVELS
            .iter()
            .map(|(level, _)| *level)
            .collect::<Vec<_>>()
            .join(" | ");
        let mut lines = vec![
            format!("activate: switch to {model}"),
            format!("usage: /model {model} activate [{ladder}]"),
        ];
        // "what would activating do to my reasoning level" — the same question
        // the resume prefill asks, so both answer it with one value.
        if let Some(app) = self.live.as_ref() {
            let now = app
                .peek_model_reasoning(&model)
                .map(|l| l.as_str())
                .unwrap_or("off");
            lines.push(
                "optional: a reasoning level after `activate`, applied on Enter;".to_string(),
            );
            lines.push(format!(
                "          omitted keeps this model's own level ({now})"
            ));
        }
        lines
    }
}

/// `/set default_model <id>`'s action leaf — the setting write, and nothing
/// else.
///
/// Deliberately not [`ModelActionNode`]: there is no running model to switch,
/// no live params to load, no session event to journal, and no reasoning level
/// to set. The catalog listing above it is shared with `/model` (a default is
/// chosen from the same catalog); only this leaf differs, so only this leaf is
/// separate.
pub struct DefaultModelActivateNode {
    app_state: Arc<AppState>,
}

impl DefaultModelActivateNode {
    pub fn new(app_state: &Arc<AppState>) -> Self {
        DefaultModelActivateNode {
            app_state: app_state.clone(),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for DefaultModelActivateNode {
    /// One row, and only once a model is drilled — at the `default_model`
    /// level the listing must stay the catalog, or every entry would grow the
    /// same action.
    async fn data(&mut self, _epoch: u64, path_args: &PathArgs) -> DataResult {
        if drilled_model(path_args, 2).is_none() {
            return Ok(Arc::new(DataResponse {
                rows: Vec::new(),
                active_ids: Vec::new(),
            }));
        }
        Ok(Arc::new(DataResponse {
            rows: vec![Row::new("activate").desc("make this the default model")],
            active_ids: Vec::new(),
        }))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let Some(model) = drilled_model(path_args, 2) else {
            return SubmitResult::Done(None);
        };
        match self
            .app_state
            .set_setting(crate::settings::SettingKey::DefaultModel, &model)
        {
            Ok(()) => SubmitResult::Done(Some(format!("default model is now {model}"))),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        vec![
            "Activate: make this the model new sessions start with.".to_string(),
            "The running model is not changed.".to_string(),
        ]
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
        assert!(Arc::ptr_eq(&r1, &r2));
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
    /// `data()` used to reuse its `Arc<DataResponse>` whenever the *catalog
    /// generation* was unchanged, but the rows also depend on the active model
    /// (hoisted to row 0, and named in `active_ids`) and on recency. Switching
    /// the model changed neither, so the stale `Arc` came back — and because
    /// `refresh_data` gates on `Arc::ptr_eq`, that stale response also stopped
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
        let els = second.elements(1);
        let asks: Vec<crate::render::RowAsk> = els.iter().map(|e| e.ask(80)).collect();
        let heights = crate::render::allocate(&asks, 24, crate::render::live_budget(24));
        let f = crate::render::render(&els, &heights, 80);
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
        assert!(joined.contains("now reasoning"), "{joined}");
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

    // ---- `/model <id> activate <level>` ----

    /// The shortcut: the level lands *after* the switch, so activation's own
    /// seeding cannot overwrite it. Setting it before `activate_model_params`
    /// would pass the model the outgoing level instead of the typed one.
    #[test]
    fn activate_with_a_level_sets_it_after_the_switch() {
        let app_state = test_app_state();
        // A stored level the seed would install if the order were wrong.
        app_state
            .update_settings(|s| {
                s.apply_model_settings(
                    TEST_MODEL,
                    &crate::model_settings::ModelSettings {
                        reasoning: Some(crate::params::ReasoningLevel::Low),
                        ..Default::default()
                    },
                );
                Ok(())
            })
            .unwrap();
        app_state.set_model("anthropic:something-else");
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        let prompt = format!("model {TEST_MODEL} activate high");
        match futures::executor::block_on(h.eval_prompt_resolved(
            &prompt,
            crate::slashdot_menu::HostKey::Enter,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {}
            other => panic!("expected the activate action to commit, got {other:?}"),
        }
        assert_eq!(app_state.model(), TEST_MODEL);
        assert_eq!(app_state.reasoning().as_deref(), Some("high"));
    }

    /// Without a level the model's own level stands — the switch keeps the
    /// seed. `off` here is the built-in seed (no record, no global default),
    /// which is what "leaves it alone" looks like in the params.
    #[test]
    fn activate_without_a_level_keeps_the_models_own() {
        let app_state = test_app_state();
        app_state
            .update_settings(|s| {
                s.apply_model_settings(
                    TEST_MODEL,
                    &crate::model_settings::ModelSettings {
                        reasoning: Some(crate::params::ReasoningLevel::Med),
                        ..Default::default()
                    },
                );
                Ok(())
            })
            .unwrap();
        app_state.set_model("anthropic:something-else");
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        match futures::executor::block_on(h.eval_prompt_resolved(
            &format!("model {TEST_MODEL} activate"),
            crate::slashdot_menu::HostKey::Enter,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {}
            other => panic!("expected the activate action to commit, got {other:?}"),
        }
        assert_eq!(app_state.model(), TEST_MODEL);
        assert_eq!(
            app_state.reasoning().as_deref(),
            Some("med"),
            "the stored level, not a forced one"
        );
    }

    /// A word that is not a level is refused *before* anything is written:
    /// the model must not switch on a typo, and the frame must keep the bad
    /// word so it can be corrected rather than retyped.
    #[test]
    fn a_level_that_is_not_one_activates_nothing() {
        let app_state = test_app_state();
        app_state.set_model("anthropic:something-else");
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        let prompt = format!("model {TEST_MODEL} activate ultra");
        let plan = match futures::executor::block_on(h.eval_prompt_resolved(
            &prompt,
            crate::slashdot_menu::HostKey::Enter,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, kept) => {
                assert_eq!(kept, prompt, "the bad word must stay in the buffer");
                plan
            }
            other => panic!("a bad level must stay open, got {other:?}"),
        };
        let detail = plan
            .detail
            .iter()
            .map(|l| l.0.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(detail.contains("unknown reasoning level"), "{detail}");
        assert!(
            detail.contains("off | low | med | high | max"),
            "the error must say what is accepted: {detail}"
        );
        assert_eq!(
            app_state.model(),
            "anthropic:something-else",
            "nothing may be activated on a bad level"
        );
    }

    /// The header advertises the parameter, and only at the leaf: the actions
    /// listing has nothing typed yet, so describing a word that does not exist
    /// there would be describing a prompt the user is not on.
    #[test]
    fn the_activate_header_advertises_the_level_only_on_the_leaf() {
        let app_state = test_app_state();
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        let mut detail = |prompt: &str| match futures::executor::block_on(h.eval_prompt_resolved(
            prompt,
            crate::slashdot_menu::HostKey::None,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => plan
                .detail
                .iter()
                .map(|l| l.0.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            other => panic!("expected Continue at {prompt:?}, got {other:?}"),
        };
        let leaf = detail(&format!("model {TEST_MODEL} activate "));
        assert!(leaf.contains("activate:"), "{leaf}");
        assert!(
            leaf.contains("off | low | med | high | max"),
            "the ladder must be spelled out: {leaf}"
        );
        assert!(
            leaf.contains("omitted keeps this model's own level"),
            "the header must say what happens without one: {leaf}"
        );
        let listing = detail(&format!("model {TEST_MODEL} "));
        assert!(
            !listing.contains("usage:"),
            "the actions listing must not advertise the leaf's parameter: {listing}"
        );
    }

    /// `/set default_model <id> activate` writes the *setting*. It used to run
    /// the live action node, whose writer was `set_model` — so choosing a
    /// default switched the running model and journaled a `ModelChange` for it,
    /// the opposite of what A.8 specifies.
    #[test]
    fn set_default_model_activate_writes_the_setting_and_leaves_the_model_alone() {
        let app_state = test_app_state();
        app_state.set_model("anthropic:something-else");
        let before = app_state.model();
        let mut menu = model_menu(&app_state);
        let mut h = menu.get_handle();
        let prompt = format!("set default_model {TEST_MODEL} activate");
        match futures::executor::block_on(h.eval_prompt_resolved(
            &prompt,
            crate::slashdot_menu::HostKey::Enter,
            80,
        )) {
            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {}
            other => panic!("expected the default to be written, got {other:?}"),
        }
        assert_eq!(
            app_state.with_settings(|s| s.default_model.clone()),
            TEST_MODEL,
            "the setting is what this path writes"
        );
        assert_eq!(app_state.model(), before, "the running model must not move");
    }

    /// The `/set` leaf takes no level and takes no params at all: a level typed
    /// there is not a thing the leaf accepts, so it commits the setting and
    /// says nothing about reasoning.
    #[test]
    fn set_default_model_takes_no_reasoning_level() {
        let app_state = test_app_state();
        let mut node = DefaultModelActivateNode::new(&app_state);
        let pa = PathArgs::new(
            vec![
                "set".into(),
                "default_model".into(),
                TEST_MODEL.into(),
                "activate".into(),
            ],
            " high",
        );
        assert!(matches!(
            futures::executor::block_on(node.submit(1, &pa)),
            SubmitResult::Done(Some(_))
        ));
        assert_eq!(
            app_state.with_settings(|s| s.default_model.clone()),
            TEST_MODEL
        );
        assert_eq!(
            app_state.reasoning(),
            None,
            "a default model is not a reasoning level"
        );
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

    // ---- the /model <id> header layout ----

    fn header_for(app_state: &Arc<AppState>, width: usize) -> Vec<String> {
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
        futures::executor::block_on(node.header(
            &PathArgs::new(vec!["model".into(), TEST_MODEL.into()], " "),
            &RenderContext { term_width: width },
        ))
    }

    /// Every line has to fit. A fixed grouping cannot promise this at more than
    /// one width, which is the reason the layout is packed instead.
    #[test]
    fn no_header_line_exceeds_the_terminal_width() {
        for width in [40usize, 60, 80, 120] {
            let lines = header_for(&test_app_state(), width);
            for l in &lines {
                assert!(
                    crate::slashdot_menu::table::display_width(l) <= width,
                    "width {width}: {} columns in {l:?}",
                    crate::slashdot_menu::table::display_width(l)
                );
            }
        }
    }

    /// An 80-column window is the case that used to overflow: the facts were
    /// grouped two-per-line while the width could hold them nearly all on one.
    #[test]
    fn the_facts_pack_onto_fewer_lines_at_80() {
        let lines = header_for(&test_app_state(), 80);
        let fact_lines = lines
            .iter()
            .filter(|l| l.trim_start().starts_with("ctx "))
            .count();
        assert_eq!(
            fact_lines, 1,
            "the facts should share a line at 80: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("in ")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("out ")), "{lines:?}");
    }

    /// A narrow terminal gives way a whole fact at a time rather than
    /// truncating one mid-value.
    #[test]
    fn a_narrow_terminal_wraps_without_splitting_a_fact() {
        let lines = header_for(&test_app_state(), 40);
        for l in &lines {
            for fact in ["ctx ", "coding ", "agentic "] {
                if l.contains(fact) {
                    assert!(
                        l.trim_start().starts_with(fact) || l.contains(&format!(" {fact}")),
                        "a fact was split across lines: {l:?}"
                    );
                }
            }
        }
        assert!(
            lines.len() > header_for(&test_app_state(), 120).len(),
            "narrow must need more lines than wide"
        );
    }

    /// The stored-defaults line wraps rather than running off the edge.
    #[test]
    fn a_long_defaults_summary_wraps() {
        let app_state = test_app_state();
        app_state.set_model("anthropic:another");
        app_state
            .update_settings(|s| {
                s.apply_model_settings(
                    TEST_MODEL,
                    &crate::model_settings::ModelSettings {
                        reasoning: Some(crate::params::ReasoningLevel::Max),
                        temperature: Some(0.25),
                        top_p: Some(0.9),
                        context_length: Some(1_048_576),
                        ..Default::default()
                    },
                );
                Ok(())
            })
            .unwrap();
        for l in header_for(&app_state, 40) {
            assert!(
                crate::slashdot_menu::table::display_width(&l) <= 40,
                "overflowed: {l:?}"
            );
        }
    }

    /// A Ctrl-E edit at `/model <id>` must reach the listing behind it.
    ///
    /// `data()` memoizes on the catalog generation, the active model and the
    /// recency list. A per-model record edit changes *none* of those, so the
    /// memo used to hit and hand back the pre-edit rows - and because
    /// `refresh_data` gates on `Arc::ptr_eq`, a response that never changed never
    /// refreshed the view either. Only visible through a *reused* node, which
    /// is why the sibling test (a fresh node each time) passed throughout.
    #[test]
    fn a_ctrl_e_edit_invalidates_the_reused_listing_memo() {
        let app_state = test_app_state();
        let (provider, model) = match app_state
            .resolved_models()
            .first()
            .map(|e| e.qualified_parts())
        {
            Some(p) => p,
            None => return, // no catalog in this environment
        };
        let qualified = format!("{provider}:{model}");
        let ctx_of = |r: &DataResponse| {
            r.rows
                .iter()
                .find(|r| r.id == qualified)
                .and_then(|r| r.field_value("context").and_then(|v| v.as_num()))
        };
        let pa = PathArgs::new(vec!["model".into()], " ");

        // One node throughout: reusing it is what makes this a test of the memo
        // rather than of the overlay.
        let mut node = test_model_node(&app_state);
        let before = ctx_of(&futures::executor::block_on(node.data(1, &pa)).unwrap())
            .expect("the model is listed");
        assert_ne!(before, 4242421.0, "pick a model not already at 4242421");

        // A second read with nothing touched must still be served from the memo.
        assert_eq!(
            ctx_of(&futures::executor::block_on(node.data(1, &pa)).unwrap()),
            Some(before),
            "an untouched read must hit the memo"
        );

        // The edit, through the same door the Ctrl-E accept arm uses.
        app_state
            .update_settings(|s| {
                s.apply_model_settings(
                    &qualified,
                    &crate::model_settings::ModelSettings {
                        context_length: Some(4242421),
                        ..Default::default()
                    },
                );
                Ok(())
            })
            .unwrap();

        assert_eq!(
            ctx_of(&futures::executor::block_on(node.data(1, &pa)).unwrap()),
            Some(4242421.0),
            "the reused node must rebuild after the edit"
        );
    }
}
