//! v2.5 `SlashDotNode` implementation for `/model`.
//!
//! Dynamic rows from the live catalog (cached, background-refreshed). The
//! node caches its `Rc<DataResponse>` keyed by the catalog's generation
//! counter — when the catalog hasn't refreshed, the engine's `Rc::ptr_eq`
//! check skips the full re-derive (filter → fuzzy → sort → render).

use std::cell::RefCell;
use std::rc::Rc;

use crate::slashdot_menu::fields::Row;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
    StaticListingNode, StaticRow,
};
use crate::slashdot_menu::fields::ColumnDef;

// ---------------------------------------------------------------------------
// ModelNode — dynamic catalog rows with Rc-cached DataResponse.
// ---------------------------------------------------------------------------

pub struct ModelNode {
    /// Read the active model id (empty string = none).
    active_fn: Box<dyn Fn() -> String>,
    /// Write the selected model id on submit. `Ok(())` on success.
    submit_fn: Box<dyn FnMut(&str) -> Result<(), String>>,
    /// Recent model ids for ordering (only for `/model`, None for `/set`).
    recent: Option<Rc<RefCell<Vec<String>>>>,
    /// Last generated DataResponse, reused via Rc::ptr_eq when the catalog
    /// generation hasn't changed.
    cached_response: Option<Rc<DataResponse>>,
    /// The catalog generation at the time cached_response was built.
    cached_generation: u64,
}

impl ModelNode {
    pub fn new(
        active_fn: Box<dyn Fn() -> String>,
        submit_fn: Box<dyn FnMut(&str) -> Result<(), String>>,
        recent: Option<Rc<RefCell<Vec<String>>>>,
    ) -> Self {
        ModelNode {
            active_fn,
            submit_fn,
            recent,
            cached_response: None,
            cached_generation: u64::MAX, // force first rebuild
        }
    }
}

impl SlashDotNode for ModelNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let gen = crate::model_catalog::catalog_generation();
        if let Some(ref cached) = self.cached_response {
            if self.cached_generation == gen {
                return Ok(Rc::clone(cached));
            }
        }

        // Catalog changed (or first call) — rebuild rows.
        let entries = crate::model_catalog::catalog_cached();
        let active_id = (self.active_fn)();
        let active = if active_id.is_empty() { None } else { Some(active_id) };
        let recent: Vec<String> = self.recent.as_ref()
            .map(|r| r.borrow().clone())
            .unwrap_or_default();

        // Build rows in the same order as the legacy ModelProvider:
        // active first, then up to 4 recent in LRU order, then server
        // popularity order for the rest.
        let ordered = order_models(&entries, &active, &recent);

        let active_ids: Vec<String> = if let Some(ref a) = active {
            vec![a.clone()]
        } else {
            Vec::new()
        };

        let rows: Vec<Row> = ordered.iter().map(|e| model_row(e)).collect();
        let response = Rc::new(DataResponse {
            rows,
            active_ids,
        });
        self.cached_response = Some(Rc::clone(&response));
        self.cached_generation = gen;
        Ok(response)
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let qualified = path_args.ids.last().cloned().unwrap_or_default();
        if qualified.is_empty() {
            return SubmitResult::Done(None);
        }
        match (self.submit_fn)(&qualified) {
            Ok(()) => SubmitResult::Done(None),
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
            FieldSpec::new("context", true).dir(SortDir::Asc).description(desc_of("context")),
            FieldSpec::new("price_input", true).dir(SortDir::Asc).description(desc_of("price_input")),
            FieldSpec::new("price_output", true).dir(SortDir::Asc).description(desc_of("price_output")),
            FieldSpec::new("price", true).dir(SortDir::Asc).description(desc_of("price")),
            FieldSpec::new("coding", true).dir(SortDir::Desc).description(desc_of("coding")),
            FieldSpec::new("intelligence", true).dir(SortDir::Desc).description(desc_of("intelligence")),
            FieldSpec::new("agentic", true).dir(SortDir::Desc).description(desc_of("agentic")),
            FieldSpec::new("reasoning", true).dir(SortDir::Desc).description("reasoning capability"),
            FieldSpec::new("created", false).dir(SortDir::Desc).description("creation date"),
            FieldSpec::new("coding_price", true).dir(SortDir::Desc).description("coding_price"),
            FieldSpec::new("intelligence_price", true).dir(SortDir::Desc).description("intelligence_price"),
            FieldSpec::new("agentic_price", true).dir(SortDir::Desc).description("agentic_price"),
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
    let mut r = Row::new(e.qualified()).desc(e.display.clone());
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

// ---------------------------------------------------------------------------
// Menu builder — adds model to the root menu.
// ---------------------------------------------------------------------------

/// Build the `/model` subtree as a `SlashDotMenuNode`.
///
/// The listing node carries column definitions and field metadata;
/// the `ModelNode` child provides dynamic catalog rows.
pub fn build_model_node(
    shared: &Rc<crate::slash_menu_bridge::MenuShared>,
) -> crate::slashdot_menu::menu::SlashDotMenuNode {
    use crate::slashdot_menu::fields::{FieldSpec, SortDir};
    use crate::slashdot_menu::menu::SlashDotMenuNode;

    let desc_of = |name: &str| -> &'static str {
        crate::model_catalog::MODEL_MODIFIERS
            .iter()
            .find(|m| m.token == name)
            .map(|m| m.desc)
            .unwrap_or("")
    };

    let mut node: SlashDotMenuNode = StaticListingNode::new(&[
        StaticRow::new("model", "select a model (provider:name)"),
    ])
    .with_columns(model_columns())
    .with_fields(vec![
        FieldSpec::new("context", true).dir(SortDir::Asc).description(desc_of("context")),
        FieldSpec::new("price_input", true).dir(SortDir::Asc).description(desc_of("price_input")),
        FieldSpec::new("price_output", true).dir(SortDir::Asc).description(desc_of("price_output")),
        FieldSpec::new("price", true).dir(SortDir::Asc).description(desc_of("price")),
        FieldSpec::new("coding", true).dir(SortDir::Desc).description(desc_of("coding")),
        FieldSpec::new("intelligence", true).dir(SortDir::Desc).description(desc_of("intelligence")),
        FieldSpec::new("agentic", true).dir(SortDir::Desc).description(desc_of("agentic")),
        FieldSpec::new("reasoning", true).dir(SortDir::Desc).description("reasoning capability"),
        FieldSpec::new("created", false).dir(SortDir::Desc).description("creation date"),
        FieldSpec::new("coding_price", true).dir(SortDir::Desc).description("coding_price"),
        FieldSpec::new("intelligence_price", true).dir(SortDir::Desc).description("intelligence_price"),
        FieldSpec::new("agentic_price", true).dir(SortDir::Desc).description("agentic_price"),
    ]).into();
    // Live session model: reads from LiveState, writes to LiveState.
    let ls = shared.live_state.clone();
    let ls2 = shared.live_state.clone();
    let recent = Some(shared.recent_models.clone());
    node.add_child(ModelNode::new(
        Box::new(move || ls.borrow().model.clone()),
        Box::new(move |model| { ls2.borrow_mut().model = model.to_string(); Ok(()) }),
        recent,
    ).into());
    node
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(h: &mut crate::slashdot_menu::SlashDotHandle<'_>, prompt: &str, key: crate::slashdot_menu::HostKey) -> crate::slashdot_menu::node_impl::MenuResult {
        h.eval_prompt(prompt, key, 80)
    }

    fn test_shared() -> Rc<crate::slash_menu_bridge::MenuShared> {
        let settings = crate::settings::SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
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
        let shared = Rc::new(crate::slash_menu_bridge::MenuShared::new(settings, None));
        shared.live_state.borrow_mut().model = "anthropic:claude-3-5-sonnet-20241022".into();
        shared
    }

    fn test_model_node(shared: &Rc<crate::slash_menu_bridge::MenuShared>) -> ModelNode {
        let ls = shared.live_state.clone();
        let ls2 = shared.live_state.clone();
        let recent = Some(shared.recent_models.clone());
        ModelNode::new(
            Box::new(move || ls.borrow().model.clone()),
            Box::new(move |model| { ls2.borrow_mut().model = model.to_string(); Ok(()) }),
            recent,
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
        let shared = test_shared();
        let mut node = test_model_node(&shared);
        let path_args = PathArgs {
            ids: Vec::new(),
            suffix: String::new(),
            args: Default::default(),
        };
        let result = node.data(1, &path_args);
        assert!(result.is_ok(), "data() should not error");
    }

    #[test]
    fn model_node_caches_rc() {
        let shared = test_shared();
        let mut node = test_model_node(&shared);
        let path_args = PathArgs {
            ids: Vec::new(),
            suffix: String::new(),
            args: Default::default(),
        };
        let r1 = node.data(1, &path_args).unwrap();
        let r2 = node.data(1, &path_args).unwrap();
        // Same generation → same Rc.
        assert!(Rc::ptr_eq(&r1, &r2));
    }

    #[test]
    fn model_node_columns() {
        let shared = test_shared();
        let node = test_model_node(&shared);
        let cols = node.columns();
        assert_eq!(cols.len(), 6);
        assert!(cols.iter().any(|c| c.header == "Price in"));
    }

    #[test]
    fn model_node_fields() {
        let shared = test_shared();
        let node = test_model_node(&shared);
        let fields = node.fields();
        assert!(fields.iter().any(|f| f.name == "context"));
        assert!(fields.iter().any(|f| f.name == "price"));
        assert!(fields.iter().any(|f| f.name == "coding"));
    }

    #[test]
    fn model_node_submit_writes_settings() {
        let shared = test_shared();
        let mut node = test_model_node(&shared);
        let path_args = PathArgs {
            ids: vec!["model".into(), "openai:gpt-4o".into()],
            suffix: " ".into(),
            args: Default::default(),
        };
        match node.submit(1, &path_args) {
            SubmitResult::Done(_) => {}
            _ => panic!("expected Done"),
        }
        // Live state, not persistent settings.
        assert_eq!(shared.live_state.borrow().model, "openai:gpt-4o");
    }

    fn build_test_menu(shared: &Rc<crate::slash_menu_bridge::MenuShared>) -> crate::slashdot_menu::SlashDotMenu {
        use crate::slashdot_menu::SlashDotMenuNode;
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        let mut ml: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("model", "select a model"),
        ]).into();
        ml.add_child(test_model_node(shared).into());
        root.add_child(ml);
        crate::slashdot_menu::SlashDotMenu::from_root_node(root)
    }

    /// Full menu traversal: root listing → drill "model" → catalog rows →
    /// Tab into a catalog entry → Enter submits.
    #[test]
    fn model_full_traversal() {
        let shared = test_shared();
        let mut menu = build_test_menu(&shared);

        let mut h = menu.get_handle();
        // Root listing: shows the "model" row.
        match eval(&mut h, "", crate::slashdot_menu::HostKey::None) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                let row_texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str())
                    .collect();
                assert!(row_texts.iter().any(|t| t.contains("model")),
                    "root should show model row");
            }
            other => panic!("expected Continue at root, got {:?}", other),
        }
        // Tab drills into the model listing.
        match eval(&mut h, "", crate::slashdot_menu::HostKey::Tab) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                assert!(prompt.starts_with("model"),
                    "after Tab should drill into model, got: {}", prompt);
            }
            other => panic!("expected Continue after Tab drill, got {:?}", other),
        }
        // If catalog has entries, Tab again drills into a model, Enter submits.
        let model_listing = eval(&mut h, "model ", crate::slashdot_menu::HostKey::None);
        if let crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) = model_listing {
            if !plan.rows.is_empty() {
                match eval(&mut h, "model ", crate::slashdot_menu::HostKey::Tab) {
                    crate::slashdot_menu::node_impl::MenuResult::Continue(_, prompt) => {
                        assert!(prompt.starts_with("model "),
                            "should drill into a model entry, got: {}", prompt);
                        match eval(&mut h, &prompt, crate::slashdot_menu::HostKey::Enter) {
                            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(..) => {
                                let model = shared.live_state.borrow().model.clone();
                                assert!(!model.is_empty(),
                                    "live_state should have a model after submit");
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
        let shared = test_shared();
        let mut menu = build_test_menu(&shared);

        let mut h = menu.get_handle();
        // Unfiltered count.
        let total = match eval(&mut h, "model ", crate::slashdot_menu::HostKey::None) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                plan.rows.len()
            }
            other => panic!("expected Continue, got {:?}", other),
        };
        // Filtered count must be strictly less.
        match eval(&mut h, "model deep", crate::slashdot_menu::HostKey::None) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                let filtered = plan.rows.len();
                assert!(filtered < total,
                    "filter 'deep' should narrow results: {} >= {}", filtered, total);
            }
            other => panic!("expected Continue on fuzzy filter, got {:?}", other),
        }
    }
}
