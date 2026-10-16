//! `/sandbox` v2.5 menu: session/config path-mapping management.
//!
//! Tree (design: `doc/slashdot/sandbox_design.md`):
//!
//! ```text
//! /sandbox (StaticListingNode — "sandbox" row, columns, fields)
//! ├─ "add" (StaticListingNode)          → AddSandboxMappingNode (leaf)
//! └─ SandboxMappingNode (dynamic)       → SandboxRemoveNode
//! ```
//!
//! `SandboxMappingNode` plays the `TreeListNode` dual role: at the
//! `/sandbox` level it emits one row per merged binding (id = sandbox
//! path); drilled into it shows `header()` + the single action child.
//! Mutations go through the `sandbox` control target (`SandboxOp`) — the
//! agent loop owns the applied `config`, so it performs the mutation +
//! re-publish at the cycle point.
use std::sync::Arc;

use crate::app_state::{AppState, SandboxOp};
use crate::config::BindMode;
use crate::sandbox_state::CombinedBind;
use crate::slashdot_menu::fields::ColumnDef;
use crate::slashdot_menu::fields::{FieldSpec, Row};
use crate::slashdot_menu::node_impl::{
    CompletionRows, DataError, DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    StaticRow, SubmitResult,
};
use crate::slashdot_menu::value::Value;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn path_str(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

fn mode_str(b: &CombinedBind) -> &'static str {
    if b.mode == BindMode::Rw {
        "rw"
    } else {
        "ro"
    }
}

fn source_str(b: &CombinedBind) -> &'static str {
    if b.is_excluded {
        "cfg [DISABLED]"
    } else if b.is_session {
        "session"
    } else {
        "cfg"
    }
}

/// Look up the drilled mapping: `ids = ["sandbox", <sandbox path>, …]`.
fn find_binding(ids: &[String]) -> Option<CombinedBind> {
    let key = ids.get(1)?;
    crate::sandbox_state::combined_list()
        .into_iter()
        .find(|b| b.sandbox_path.to_string_lossy() == key.as_str())
}

// ---------------------------------------------------------------------------
// Column / field declarations (root `/sandbox` level)
// ---------------------------------------------------------------------------

/// Columns for the `/sandbox` listing: host | sandbox path (id) | mode
/// | source. Rows missing a field (e.g. the static "add" row) render
//  it blank — only the id is required.
pub fn sandbox_listing_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef::new("host", "Host path")
            .min(8)
            .prio(3)
            .wrap(false),
        ColumnDef::new("id", "Sandbox path")
            .min(8)
            .prio(3)
            .wrap(false),
        ColumnDef::new("mode", "Mode").min(4).prio(1).wrap(false),
        ColumnDef::new("source", "Source")
            .min(7)
            .prio(1)
            .wrap(false),
    ]
}

/// Filterable/sortable fields for the `/sandbox` listing (design:
/// mode + source; host/sandbox paths are reachable via `id` /
/// `description`).
pub fn sandbox_fields() -> Vec<FieldSpec> {
    vec![
        FieldSpec::new("mode", false),
        FieldSpec::new("source", false),
    ]
}

// ---------------------------------------------------------------------------
// SandboxMappingNode — dynamic rows + drilled header (TreeListNode role)
// ---------------------------------------------------------------------------

/// Dynamic listing node for `/sandbox`. `data()` returns one row per
/// merged binding (config + session); when drilled into (id = sandbox
/// path) `header()` renders the mapping info and the static action
/// child (`SandboxRemoveNode`) is shown.
pub struct SandboxMappingNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for SandboxMappingNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let rows = crate::sandbox_state::combined_list()
            .into_iter()
            .map(|b| {
                let host = path_str(&b.host_path);
                Row::new(path_str(&b.sandbox_path))
                    // description = host: fuzzy searches id + description,
                    // so typing either side of a remap finds the row.
                    .desc(&host)
                    .field("host", Value::Str(host))
                    .field("mode", Value::Str(mode_str(&b).to_string()))
                    .field("source", Value::Str(source_str(&b).to_string()))
            })
            .collect();
        Ok(Arc::new(DataResponse {
            rows,
            active_ids: Vec::new(),
        }))
    }

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx(&self.app_state);
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
    }

    async fn header(&mut self, path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        match find_binding(&path_args.ids) {
            None => vec!["Mapping not found.".to_string()],
            Some(b) => vec![
                format!("Host:    {}", path_str(&b.host_path)),
                format!("Sandbox: {}", path_str(&b.sandbox_path)),
                format!("Mode:    {}", mode_str(&b)),
                format!("Source:  {}", source_str(&b)),
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// SandboxRemoveNode — action leaf (confirm → control target)
// ---------------------------------------------------------------------------

/// Action leaf: remove a session binding, exclude a config binding, or
/// restore an excluded one. Two-phase confirm via `prepare()`.
pub struct SandboxRemoveNode {
    pub app_state: Arc<AppState>,
}

impl SandboxRemoveNode {
    /// Confirm-arm text for a binding — names the effect `submit()`
    /// will have, so the dialog never says "Accept" for an irreversible
    /// delete.
    fn arms_for(b: Option<&CombinedBind>) -> (String, String) {
        match b {
            Some(b) if b.is_excluded => (
                "Re-enable this mapping".to_string(),
                "Leave it excluded".to_string(),
            ),
            Some(b) if b.is_session => (
                "Remove this session mapping (deletes it)".to_string(),
                "Keep the mapping".to_string(),
            ),
            Some(_) => (
                "Exclude this config mapping for this session".to_string(),
                "Keep the mapping".to_string(),
            ),
            None => (
                "Apply this change".to_string(),
                "Leave it unchanged".to_string(),
            ),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for SandboxRemoveNode {
    async fn data(&mut self, _epoch: u64, path_args: &PathArgs) -> DataResult {
        let binding = find_binding(&path_args.ids);
        let row = match binding.as_ref() {
            None => Row::new("remove").desc("mapping not found"),
            Some(b) if b.is_excluded => Row::new("restore").desc("re-enable this mapping"),
            Some(b) if b.is_session => Row::new("remove").desc("remove this session mapping"),
            Some(_) => Row::new("remove").desc("exclude this config mapping"),
        };
        Ok(Arc::new(DataResponse {
            rows: vec![row],
            active_ids: Vec::new(),
        }))
    }

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx(&self.app_state);
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
    }

    async fn header(&mut self, path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        let Some(b) = find_binding(&path_args.ids) else {
            let id = path_args.ids.get(1).cloned().unwrap_or_default();
            return vec![format!("Mapping not found: {}", id)];
        };
        let mut lines = vec![
            format!("Host:    {}", path_str(&b.host_path)),
            format!("Sandbox: {}", path_str(&b.sandbox_path)),
            format!("Mode:    {}", mode_str(&b)),
            format!("Source:  {}", source_str(&b)),
        ];
        lines.push(String::new());
        if b.is_excluded {
            lines.push(
                "This config binding is currently excluded — restore re-enables it.".to_string(),
            );
        } else if !b.is_session {
            lines.push(
                "Config binding: removing only excludes it for this session \
                 (restore re-enables it)."
                    .to_string(),
            );
        } else {
            lines.push("Session binding: removing deletes it for this session.".to_string());
        }
        lines
    }

    /// The action is picked from the binding's state, so the arm text
    /// names the effect `submit()` will actually have.
    async fn prepare(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        SubmitResult::Prepared(Some(Self::arms_for(find_binding(&path_args.ids).as_ref())))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let Some(b) = find_binding(&path_args.ids) else {
            let id = path_args.ids.get(1).cloned().unwrap_or_default();
            return SubmitResult::Error(format!("mapping not found: {}", id));
        };
        let sandbox_path = path_str(&b.sandbox_path);
        let op = if b.is_excluded {
            SandboxOp::Restore { sandbox_path }
        } else if b.is_session {
            SandboxOp::Remove { sandbox_path }
        } else {
            SandboxOp::Exclude { sandbox_path }
        };
        self.app_state.request_sandbox(op);
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// AddSandboxMappingNode — parameter leaf (ro/rw rows, fs completion)
// ---------------------------------------------------------------------------

/// Parameter leaf under `/sandbox add`. `data()` returns the `ro`/`rw`
/// mode rows; the drilled mode is `ids.last()`; the paths are the flat
/// `args` (host path, optional sandbox destination).
///
/// `complete_parameter` follows the settled contract
/// (sandbox_design.md): gate → route by committed count (0 = host FS,
/// 1 = sandbox-aware) → `path_complete::complete` → exactness → rows,
/// with an L2 `Arc` cache keyed by (arg, word) that
/// `path_complete::listing_stale` invalidates.
pub struct AddSandboxMappingNode {
    pub app_state: Arc<AppState>,
    /// L2: (arg position, word) → rows. Same `Arc` on unchanged-word
    /// frames ⇒ `ptr_eq` skips the view re-derive; rebuilt when the
    /// word/arg changes or the L1 listing goes stale.
    last_rows: Option<(usize, String, Arc<CompletionRows>)>,
}

impl AddSandboxMappingNode {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self {
            app_state,
            last_rows: None,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for AddSandboxMappingNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Arc::new(DataResponse {
            rows: vec![
                Row::new("ro").desc("read only mapping"),
                Row::new("rw").desc("read-write mapping"),
            ],
            active_ids: Vec::new(),
        }))
    }

    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = crate::v25_listing_node::header_ctx(&self.app_state);
        let header = self.header(path_args, &render_ctx).await;
        crate::v25_listing_node::view_header(&self.app_state, &header)
    }

    async fn header(&mut self, path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        let mode = path_args.ids.last().map(String::as_str).unwrap_or("?");
        let mode_desc = if mode == "rw" {
            "read-write"
        } else {
            "read-only"
        };
        vec![
            "Specify host path and optionally sandbox destination path.".to_string(),
            format!("Mode: {} ({})", mode, mode_desc),
        ]
    }

    async fn complete_parameter(
        &mut self,
        args: &PathArgs,
        cursor_pos: usize,
    ) -> Result<Arc<CompletionRows>, DataError> {
        let empty = || {
            Arc::new(CompletionRows {
                rows: Vec::new(),
                labels: None,
                match_indices: None,
            })
        };
        let Some(word_range) = args.word_before(cursor_pos) else {
            return Ok(empty());
        };
        let word = args.suffix[word_range].to_string();
        if !crate::path_complete::completable(&word) {
            return Ok(empty());
        }
        let arg_idx = args.committed(cursor_pos).count();
        if arg_idx >= 2 {
            return Ok(empty());
        }
        let sandbox = arg_idx == 1;

        // L2 hit: same (arg, word) and the L1 listing still fresh.
        if let Some((a, w, rc)) = &self.last_rows {
            if *a == arg_idx && *w == word && !crate::path_complete::listing_stale(sandbox) {
                return Ok(Arc::clone(rc));
            }
        }

        let (entries, _) = crate::path_complete::complete(&word, sandbox);
        // Exactness: every candidate byte-equal to the word ⇒ nothing
        // left to complete (engine appends the trailing space on Tab;
        // a passive frame shows no dialog).
        let all_exact = !entries.is_empty() && entries.iter().all(|e| e.path == word);
        let rows: Vec<Row> = if all_exact {
            Vec::new()
        } else {
            entries
                .iter()
                .map(|e| Row::new(&e.path).desc(e.description()))
                .collect()
        };
        let labels = Some((
            if sandbox { "Sandbox path" } else { "Host path" }.to_string(),
            "Description".to_string(),
        ));
        let rc = Arc::new(CompletionRows {
            rows,
            labels,
            match_indices: None,
        });
        self.last_rows = Some((arg_idx, word, Arc::clone(&rc)));
        Ok(rc)
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let raw: Vec<&str> = path_args.args().collect();
        if raw.is_empty() {
            return SubmitResult::Error("host path required".to_string());
        }
        if raw.len() > 2 {
            return SubmitResult::Error("expected: <host path> [sandbox path]".to_string());
        }
        let mode = match path_args.ids.last().map(String::as_str) {
            Some("rw") => BindMode::Rw,
            Some("ro") => BindMode::Ro,
            other => {
                return SubmitResult::Error(format!(
                    "mode required (ro|rw), got {}",
                    other.unwrap_or("<none>")
                ))
            }
        };
        // Stay-open validation: the host path must exist (the sandbox
        // dst is NOT validated — bwrap creates it lazily).
        if let Err(e) = crate::path_util::resolve_canonical(raw[0]) {
            return SubmitResult::Error(e);
        }
        self.app_state.request_sandbox(SandboxOp::Add {
            host_path: raw[0].to_string(),
            sandbox_path: raw.get(1).map(|s| s.to_string()).unwrap_or_default(),
            mode,
        });
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// Menu builder
// ---------------------------------------------------------------------------

/// Build the `/sandbox` subtree as a `SlashDotMenuNode`.
///
/// The root `StaticListingNode` provides the "sandbox" command row,
/// the listing columns, and the field metadata; `add` (added first so
/// it tops the default listing) and `SandboxMappingNode` are the
/// children.
pub fn build_sandbox_node(
    app_state: &Arc<AppState>,
) -> crate::slashdot_menu::menu::SlashDotMenuNode {
    use crate::slashdot_menu::menu::SlashDotMenuNode;
    use crate::v25_listing_node::ListingNode;

    let mut listing: SlashDotMenuNode = ListingNode::new(
        app_state,
        &[StaticRow::new("sandbox", "Manage sandbox path mappings")],
    )
    .with_columns(sandbox_listing_columns())
    .with_fields(sandbox_fields())
    .into();

    let mut add_node: SlashDotMenuNode = ListingNode::new(
        app_state,
        &[StaticRow::new("add", "Add a path mapping to the sandbox")],
    )
    .into();
    add_node.add_child(AddSandboxMappingNode::new(app_state.clone()).into());
    listing.add_child(add_node);

    let mut map_node: SlashDotMenuNode = SandboxMappingNode {
        app_state: app_state.clone(),
    }
    .into();
    map_node.add_child(
        SandboxRemoveNode {
            app_state: app_state.clone(),
        }
        .into(),
    );
    listing.add_child(map_node);

    listing
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BindPath, SandboxConfig};
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::StaticListingNode;
    use crate::slashdot_menu::render::RenderPlan;
    use crate::slashdot_menu::{HostKey, MenuResult};

    fn test_settings() -> crate::settings::SettingsStore {
        crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_reasoning_level: None,
            show_reasoning: String::new(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            models: Default::default(),
            path: None,
        }
    }

    /// Returns an `AppState` (binding kept as `app_state` in tests below).
    fn test_app_state() -> Arc<AppState> {
        crate::app_state::AppState::for_test(test_settings())
    }

    fn sandbox_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(build_sandbox_node(app_state));
        SlashDotMenu::from_root_node(root)
    }

    /// Drain the sandbox control target (sole-consumer GC — `take_control`
    /// clears it, so no explicit reset is needed between assertions).
    fn take_sandbox_op(app_state: &Arc<AppState>) -> Option<SandboxOp> {
        app_state.take_control().sandbox
    }

    /// Reset the process-global sandbox state (session binds,
    /// exclusions, base config) and the published config (which also
    /// invalidates the completion L1 slot).
    fn setup() {
        crate::sandbox_state::clear();
        crate::tools::set_sandbox(None);
    }

    fn base_config() -> SandboxConfig {
        SandboxConfig {
            ro: vec![BindPath::Simple("/cfg-ro".to_string())],
            rw: vec![BindPath::Simple("/cfg-rw".to_string())],
            ..Default::default()
        }
    }

    fn rows_of(plan: &RenderPlan) -> Vec<String> {
        plan.rows.iter().map(|l| l.0.clone()).collect()
    }

    fn detail_of(plan: &RenderPlan) -> Vec<String> {
        plan.detail.iter().map(|l| l.0.clone()).collect()
    }

    fn header_of(plan: &RenderPlan) -> String {
        plan.table_header
            .as_ref()
            .map(|l| l.0.clone())
            .unwrap_or_default()
    }

    fn expect_continue(result: MenuResult) -> (RenderPlan, String) {
        match result {
            MenuResult::Continue(plan, prompt) => (plan, prompt),
            other => panic!("expected Continue, got {:?}", other),
        }
    }

    #[test]
    #[serial_test::serial]
    fn root_listing_shows_add_and_bindings() {
        setup();
        crate::sandbox_state::set_base(Some(base_config()));
        let td = tempfile::tempdir().unwrap();
        let host = std::fs::canonicalize(td.path()).unwrap();
        crate::sandbox_state::add(&host.to_string_lossy(), "", BindMode::Rw).unwrap();

        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            "sandbox ",
            HostKey::None,
            100,
        )));

        // Table headers render UPPERCASE (table.rs).
        let th = header_of(&plan).to_uppercase();
        for label in ["HOST PATH", "SANDBOX PATH", "MODE", "SOURCE"] {
            assert!(th.contains(label), "column {label:?} missing in {th:?}");
        }
        let rows = rows_of(&plan);
        assert!(rows.iter().any(|r| r.contains("add")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("/cfg-ro")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("/cfg-rw")), "{rows:?}");
        let host_s = host.to_string_lossy().into_owned();
        assert!(rows.iter().any(|r| r.contains(&host_s)), "{rows:?}");
        // "add" row added first ⇒ tops the default listing.
        assert!(rows[0].contains("add"), "add must sort first: {rows:?}");
    }

    #[test]
    #[serial_test::serial]
    fn excluded_binding_renders_disabled_and_offers_restore() {
        setup();
        crate::sandbox_state::set_base(Some(base_config()));
        crate::sandbox_state::exclude("/cfg-ro").unwrap();

        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();

        // Root listing: source cell shows the disabled marker.
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            "sandbox ",
            HostKey::None,
            100,
        )));
        let rows = rows_of(&plan);
        assert!(
            rows.iter().any(|r| r.contains("cfg [DISABLED]")),
            "{rows:?}"
        );

        // Drilled mapping level: the action row is `restore`.
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            "sandbox /cfg-ro ",
            HostKey::None,
            100,
        )));
        let rows = rows_of(&plan);
        assert!(
            rows.iter().any(|r| r.contains("re-enable this mapping")),
            "{rows:?}"
        );
        let detail = detail_of(&plan).join("\n");
        assert!(detail.contains("Source:  cfg [DISABLED]"), "{detail:?}");
    }

    #[test]
    #[serial_test::serial]
    fn remove_header_notes_config_and_session_bindings() {
        setup();
        crate::sandbox_state::set_base(Some(base_config()));
        let td = tempfile::tempdir().unwrap();
        let host = std::fs::canonicalize(td.path()).unwrap();
        crate::sandbox_state::add(&host.to_string_lossy(), "", BindMode::Ro).unwrap();

        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();

        // Config binding: exclude note + restore mention.
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            "sandbox /cfg-rw remove ",
            HostKey::None,
            100,
        )));
        let detail = detail_of(&plan).join("\n");
        assert!(detail.contains("excludes it"), "{detail:?}");
        assert!(detail.contains("restore"), "{detail:?}");

        // Session binding: session note, no config wording.
        let host_s = host.to_string_lossy().into_owned();
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            &format!("sandbox {} remove ", host_s),
            HostKey::None,
            100,
        )));
        let detail = detail_of(&plan).join("\n");
        assert!(detail.contains("Session binding"), "{detail:?}");
    }

    #[test]
    #[serial_test::serial]
    fn remove_submit_picks_op_by_binding_state() {
        setup();
        crate::sandbox_state::set_base(Some(base_config()));
        let td = tempfile::tempdir().unwrap();
        let host = std::fs::canonicalize(td.path()).unwrap();
        let host_s = host.to_string_lossy().into_owned();
        crate::sandbox_state::add(&host_s, "", BindMode::Ro).unwrap();

        let app_state = test_app_state();
        let mut node = SandboxRemoveNode {
            app_state: app_state.clone(),
        };
        let pa = |path: &str| PathArgs {
            ids: vec!["sandbox".into(), path.to_string(), "remove".into()],
            suffix: String::new(),
            args: Vec::new(),
        };

        // Session bind → Remove.
        assert!(matches!(
            futures::executor::block_on(node.prepare(1, &pa(&host_s))),
            SubmitResult::Prepared(_)
        ));
        assert!(matches!(
            futures::executor::block_on(node.submit(1, &pa(&host_s))),
            SubmitResult::Done(None)
        ));
        assert!(matches!(
            take_sandbox_op(&app_state),
            Some(SandboxOp::Remove { sandbox_path }) if sandbox_path == host_s
        ));

        // Active config bind → Exclude.
        assert!(matches!(
            futures::executor::block_on(node.submit(1, &pa("/cfg-ro"))),
            SubmitResult::Done(None)
        ));
        assert!(matches!(
            take_sandbox_op(&app_state),
            Some(SandboxOp::Exclude { sandbox_path }) if sandbox_path == "/cfg-ro"
        ));

        // Excluded config bind → Restore.
        crate::sandbox_state::exclude("/cfg-ro").unwrap();
        assert!(matches!(
            futures::executor::block_on(node.submit(1, &pa("/cfg-ro"))),
            SubmitResult::Done(None)
        ));
        assert!(matches!(
            take_sandbox_op(&app_state),
            Some(SandboxOp::Restore { sandbox_path }) if sandbox_path == "/cfg-ro"
        ));

        // Unknown binding → stay-open error.
        match futures::executor::block_on(node.submit(1, &pa("/no/such/binding"))) {
            SubmitResult::Error(msg) => assert!(msg.contains("mapping not found"), "{msg}"),
            _other => panic!("expected Error, got non-error result"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn remove_confirm_flow_two_enters() {
        setup();
        crate::sandbox_state::set_base(Some(base_config()));

        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();

        // Enter #1: drill into `remove` + arm the confirm (Prepared).
        // The arming step returns the dialog frame directly (no prompt
        // round trip); the passive re-derive must reproduce it.
        let (_, prompt) = expect_continue(futures::executor::block_on(h.eval_prompt_resolved(
            "sandbox /cfg-ro remove",
            HostKey::Enter,
            100,
        )));
        assert_eq!(prompt, "sandbox /cfg-ro remove ", "{prompt:?}");
        assert!(
            take_sandbox_op(&app_state).is_none(),
            "first Enter must only arm the confirm"
        );
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            &prompt,
            HostKey::None,
            100,
        )));
        assert!(
            detail_of(&plan).join("\n").contains("excludes it"),
            "confirm frame must show the header: {:?}",
            detail_of(&plan)
        );
        // The dialog itself: two arms, accept first and selected, with
        // this node's phrasing for a config exclude.
        let arms = rows_of(&plan);
        assert_eq!(arms.len(), 2, "confirm dialog arms: {arms:?}");
        assert!(arms[0].contains("Exclude this config mapping"), "{arms:?}");
        assert!(arms[1].contains("Keep the mapping"), "{arms:?}");
        assert_eq!(plan.selected, Some(0), "accept carries the cursor");

        // Enter #2: submit → Exclude op.
        let result = futures::executor::block_on(h.eval_prompt(&prompt, HostKey::Enter, 100));
        assert!(
            matches!(result, MenuResult::DoneSuccess(..)),
            "second Enter should submit: {:?}",
            result
        );
        assert!(matches!(
            take_sandbox_op(&app_state),
            Some(SandboxOp::Exclude { sandbox_path }) if sandbox_path == "/cfg-ro"
        ));
    }

    /// The `cancel` arm: a second Enter on it drops the confirm in
    /// place — no control target written, menu still open.
    #[test]
    #[serial_test::serial]
    fn remove_confirm_cancel_arm_does_not_commit() {
        setup();
        crate::sandbox_state::set_base(Some(base_config()));

        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();

        let (_, prompt) = expect_continue(futures::executor::block_on(h.eval_prompt_resolved(
            "sandbox /cfg-ro remove",
            HostKey::Enter,
            100,
        )));
        // Move onto `cancel`, then resolve.
        let (moved, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            &prompt,
            HostKey::MoveCursor(1),
            100,
        )));
        assert_eq!(moved.selected, Some(1), "cursor on the cancel arm");
        let result = futures::executor::block_on(h.eval_prompt(&prompt, HostKey::Enter, 100));
        assert!(
            matches!(result, MenuResult::Continue(..)),
            "cancel stays open: {result:?}"
        );
        assert!(
            take_sandbox_op(&app_state).is_none(),
            "cancel must not write a control target"
        );
    }

    #[test]
    #[serial_test::serial]
    fn add_level_shows_mode_rows() {
        setup();
        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();

        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            "sandbox add ",
            HostKey::None,
            100,
        )));
        let rows = rows_of(&plan);
        assert!(rows.iter().any(|r| r.contains("ro")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("rw")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("read only")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("read-write")), "{rows:?}");
    }

    #[test]
    #[serial_test::serial]
    fn add_submit_validation_and_op() {
        setup();
        let app_state = test_app_state();
        let mut node = AddSandboxMappingNode::new(app_state.clone());

        let mk = |ids_last: &str, suffix: &str| -> (PathArgs, Vec<std::ops::Range<usize>>) {
            // Word ranges over `suffix` (skip the leading space).
            let words: Vec<&str> = suffix.split_whitespace().collect();
            let mut ranges = Vec::new();
            let mut cursor = suffix.len() - suffix.trim_start().len();
            for w in &words {
                ranges.push(cursor..cursor + w.len());
                cursor += w.len() + 1;
            }
            (
                PathArgs {
                    ids: vec!["sandbox".into(), "add".into(), ids_last.into()],
                    suffix: suffix.to_string(),
                    args: ranges.clone(),
                },
                ranges,
            )
        };

        // 0 args → drill guard.
        let (pa, _) = mk("ro", "");
        match futures::executor::block_on(node.submit(1, &pa)) {
            SubmitResult::Error(msg) => assert_eq!(msg, "host path required"),
            _other => panic!("expected Error for empty args"),
        }

        // ≥3 args → rejected.
        let (pa, _) = mk("ro", " a b c");
        match futures::executor::block_on(node.submit(1, &pa)) {
            SubmitResult::Error(msg) => assert!(msg.contains("expected:"), "{msg}"),
            _other => panic!("expected Error for 3 args"),
        }

        // Nonexistent host → stay-open error.
        let (pa, _) = mk("ro", " /nonexistent-picocode-xyz-123");
        match futures::executor::block_on(node.submit(1, &pa)) {
            SubmitResult::Error(msg) => assert!(msg.contains("does not exist"), "{msg}"),
            _other => panic!("expected Error for missing host"),
        }
        assert!(
            take_sandbox_op(&app_state).is_none(),
            "no op may be queued on validation failure"
        );

        // Valid host (rw mode) → Done + Add op with the raw args.
        let td = tempfile::tempdir().unwrap();
        let host_s = td.path().to_string_lossy().into_owned();
        let suffix = format!(" {}", host_s);
        let (pa, _) = mk("rw", &suffix);
        assert!(matches!(
            futures::executor::block_on(node.submit(1, &pa)),
            SubmitResult::Done(None)
        ));
        assert!(matches!(
            take_sandbox_op(&app_state),
            Some(SandboxOp::Add { host_path, sandbox_path, mode }) if
                host_path == host_s && sandbox_path.is_empty() && mode == BindMode::Rw
        ));

        // Two args → both carried through.
        let dst = "/some/sandbox/dst".to_string();
        let suffix = format!(" {} {}", host_s, dst);
        let (pa, _) = mk("ro", &suffix);
        assert!(matches!(
            futures::executor::block_on(node.submit(1, &pa)),
            SubmitResult::Done(None)
        ));
        assert!(matches!(
            take_sandbox_op(&app_state),
            Some(SandboxOp::Add { host_path, sandbox_path, mode }) if
                host_path == host_s && sandbox_path == dst && mode == BindMode::Ro
        ));
    }

    #[test]
    #[serial_test::serial]
    fn drill_arrival_shows_error_above_header() {
        setup();
        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();

        // Enter on the bare `ro` id drills into the param leaf with an
        // empty suffix → submit guard fires → stay-open error frame.
        let (plan, prompt) = expect_continue(futures::executor::block_on(h.eval_prompt(
            "sandbox add ro",
            HostKey::Enter,
            100,
        )));
        assert_eq!(prompt, "sandbox add ro ", "{prompt:?}");
        let detail = detail_of(&plan);
        assert_eq!(detail.len(), 3, "{detail:?}"); // error + two header lines
        assert!(
            detail[0].contains("[ERROR]: host path required"),
            "{detail:?}"
        );
        assert!(detail[1].contains("Specify host path"), "{detail:?}");
        assert!(detail[2].contains("Mode: ro"), "{detail:?}");
        assert!(
            take_sandbox_op(&app_state).is_none(),
            "drill arrival must not queue an op"
        );
    }

    #[test]
    #[serial_test::serial]
    fn completion_routes_args_and_labels() {
        setup();
        let td1 = tempfile::tempdir().unwrap();
        let td2 = tempfile::tempdir().unwrap();
        std::fs::write(td1.path().join("outer.txt"), "x").unwrap();
        std::fs::write(td2.path().join("inner.txt"), "x").unwrap();
        let p1 = td1.path().to_string_lossy().into_owned();
        let p2 = td2.path().to_string_lossy().into_owned();

        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();

        // Arg 1 → host listing, "Host path" label.
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            &format!("sandbox add ro {}/", p1),
            HostKey::None,
            100,
        )));
        let th = header_of(&plan).to_uppercase();
        assert!(!th.is_empty(), "arg-1 completion must open a dialog");
        assert!(th.contains("HOST PATH"), "{th:?}");
        let rows = rows_of(&plan);
        assert!(!rows.is_empty(), "dir listing expected: {rows:?}");

        // Arg 2 → sandbox-routed completion, "Sandbox path" label
        // (no sandbox configured ⇒ host fallback, serial setup).
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            &format!("sandbox add ro {p1} {p2}/"),
            HostKey::None,
            100,
        )));
        let th = header_of(&plan).to_uppercase();
        assert!(th.contains("SANDBOX PATH"), "{th:?}");
        let rows = rows_of(&plan);
        assert!(
            rows.iter().any(|r| r.contains("inner.txt")),
            "arg-2 must list the dst dir: {rows:?}"
        );

        // ≥2 committed → inert (no dialog).
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            &format!("sandbox add ro {p1} {p2}/ extra"),
            HostKey::None,
            100,
        )));
        assert!(header_of(&plan).is_empty(), "3rd arg must be inert");
    }

    #[test]
    #[serial_test::serial]
    fn completion_exact_file_and_gate() {
        setup();
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("solo.txt"), "x").unwrap();
        let p = td.path().to_string_lossy().into_owned();

        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();

        // Non-exact stem → dialog with the file.
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            &format!("sandbox add ro {p}/so"),
            HostKey::None,
            100,
        )));
        assert!(
            header_of(&plan).to_uppercase().contains("HOST PATH"),
            "dialog expected"
        );
        assert!(
            rows_of(&plan).iter().any(|r| r.contains("solo.txt")),
            "{:?}",
            rows_of(&plan)
        );

        // Exact file → all candidates exact → no dialog (header only).
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            &format!("sandbox add ro {p}/solo.txt"),
            HostKey::None,
            100,
        )));
        assert!(
            header_of(&plan).is_empty(),
            "exact-only match must not open a dialog"
        );
        assert!(
            detail_of(&plan).join("\n").contains("Specify host path"),
            "leaf header still renders: {:?}",
            detail_of(&plan)
        );

        // Gate: ordinary bare word → inert, no dialog.
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            "sandbox add ro plainword",
            HostKey::None,
            100,
        )));
        assert!(header_of(&plan).is_empty(), "bare word must be inert");

        // Gate: dot-word is attempted (cwd listing — may be empty, but
        // must not error); a definitely-gated word never is.
        let (plan, _) = expect_continue(futures::executor::block_on(h.eval_prompt(
            "sandbox add ro ..",
            HostKey::None,
            100,
        )));
        let _ = plan; // ".." may or may not match cwd entries — just no panic.
    }

    #[test]
    #[serial_test::serial]
    fn two_path_add_end_to_end() {
        setup();
        let td = tempfile::tempdir().unwrap();
        let host_s = td.path().to_string_lossy().into_owned();
        let dst = "/mapped/dst".to_string();

        let app_state = test_app_state();
        let mut menu = sandbox_menu(&app_state);
        let mut h = menu.get_handle();

        let line = format!("sandbox add ro {host_s} {dst}");
        let result = futures::executor::block_on(h.eval_prompt(&line, HostKey::Enter, 100));
        assert!(
            matches!(result, MenuResult::DoneSuccess(..)),
            "two-path add should submit: {:?}",
            result
        );
        assert!(matches!(
            take_sandbox_op(&app_state),
            Some(SandboxOp::Add { host_path, sandbox_path, mode }) if
                host_path == host_s && sandbox_path == dst && mode == BindMode::Ro
        ));
    }
}
