//! The host's editor-capable listing node — a decorator around the
//! library's `StaticListingNode` (doc/editor_cleanup.md §4).
//!
//! Eligibility: exactly the menu levels that use `StaticListingNode` today
//! (static rows, no `submit`). Dynamic listing nodes keep their own impls
//! and the trait's no-op `edit()`.
//!
//! Every trait method *forwards* to the inner node — no logic duplication —
//! and the inner type is concrete, so forwarding costs nothing beyond the
//! menu's one existing `dyn` layer. Only `edit()` is host behavior: a
//! read-only (`EditorMode::View`) look at this level's `header()`. Rows in
//! the view are a later addition, once specced.

use std::rc::Rc;
use std::sync::Arc;

use crate::app_state::AppState;
use crate::editor::EditorMode;
use crate::output;
use crate::slashdot_menu::fields::{ColumnDef, FieldSpec};
use crate::slashdot_menu::node_impl::{
    CompletionRows, DataError, DataResult, PathArgs, RenderContext, SlashDotNode,
    StaticListingNode, StaticRow, SubmitResult,
};

/// Render context for driving a node's own `header()` from `edit()` — the
/// engine's `RenderContext` is not passed through `edit()`.
pub(crate) fn header_ctx() -> RenderContext {
    RenderContext {
        term_width: output::term_width(),
    }
}

/// The shared read-only `edit()` body for header-bearing nodes: open
/// `header` in `$EDITOR` (`View`, nothing persisted). An empty header is a
/// no-op (no editor launched); an editor failure surfaces as `Error` (the
/// engine shows it and the menu stays open).
pub(crate) fn view_header(app_state: &AppState, header: &[String]) -> SubmitResult {
    let text = header.join("\n");
    if text.is_empty() {
        return SubmitResult::Handled;
    }
    match app_state.edit_text(&text, EditorMode::View) {
        Ok(_) => SubmitResult::Handled,
        Err(e) => SubmitResult::Error(format!("editor error: {}", e)),
    }
}

/// `StaticListingNode` + host `edit()` (read-only header view).
pub struct ListingNode {
    inner: StaticListingNode,
    app_state: Arc<AppState>,
}

impl ListingNode {
    pub fn new(app_state: &Arc<AppState>, rows: &[StaticRow]) -> Self {
        Self {
            inner: StaticListingNode::new(rows),
            app_state: app_state.clone(),
        }
    }

    // Builder parity with `StaticListingNode` — delegating.
    pub fn with_header(mut self, header: Vec<String>) -> Self {
        self.inner = self.inner.with_header(header);
        self
    }

    pub fn with_fields(mut self, specs: Vec<FieldSpec>) -> Self {
        self.inner = self.inner.with_fields(specs);
        self
    }

    pub fn with_columns(mut self, cols: Vec<ColumnDef>) -> Self {
        self.inner = self.inner.with_columns(cols);
        self
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for ListingNode {
    // --- Forwards: inner is concrete, so these are static calls. ---

    async fn data(&mut self, epoch: u64, path_args: &PathArgs) -> DataResult {
        self.inner.data(epoch, path_args).await
    }

    async fn submit(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        self.inner.submit(epoch, path_args).await
    }

    async fn prepare(&mut self, epoch: u64, path_args: &PathArgs) -> SubmitResult {
        self.inner.prepare(epoch, path_args).await
    }

    async fn header(&mut self, path_args: &PathArgs, render_ctx: &RenderContext) -> Vec<String> {
        self.inner.header(path_args, render_ctx).await
    }

    async fn complete_parameter(
        &mut self,
        args: &PathArgs,
        cursor_pos: usize,
    ) -> Result<Rc<CompletionRows>, DataError> {
        self.inner.complete_parameter(args, cursor_pos).await
    }

    fn fields(&self) -> Vec<FieldSpec> {
        self.inner.fields()
    }

    fn columns(&self) -> Vec<ColumnDef> {
        self.inner.columns()
    }

    async fn drill_rewrite(&mut self, path_args: &PathArgs) -> Option<String> {
        self.inner.drill_rewrite(path_args).await
    }

    /// Read-only view of this level's header in `$EDITOR` (`View`: 0400
    /// tempfile, nothing read back). An empty header is already a no-op —
    /// no editor is launched. (`edit()` is intentionally *not* forwarded:
    /// `StaticListingNode` has no override, and this host behavior is the
    /// point of the decorator.)
    async fn edit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let render_ctx = header_ctx();
        let header = self.inner.header(path_args, &render_ctx).await;
        view_header(&self.app_state, &header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::{HostKey, MenuResult};
    use futures::executor::block_on;

    // Per-module test settings — the repo convention (see v25_replay.rs).
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

    fn app_state() -> Arc<AppState> {
        AppState::for_test(test_settings())
    }

    fn ctx() -> RenderContext {
        RenderContext { term_width: 80 }
    }

    #[test]
    fn forwards_listing_behavior_to_the_inner_node() {
        let state = app_state();
        let mut node = ListingNode::new(&state, &[StaticRow::new("x", "equis")])
            .with_header(vec!["hdr".to_string()]);
        let rows = block_on(node.data(1, &PathArgs::new(vec![], ""))).unwrap();
        assert_eq!(rows.rows[0].id, "x");
        assert_eq!(
            block_on(node.header(&PathArgs::new(vec![], ""), &ctx())),
            vec!["hdr".to_string()]
        );
    }

    #[test]
    fn edit_on_an_empty_header_is_a_noop() {
        // No header → no editor, no terminal needed: `Handled`.
        let state = app_state();
        let mut node = ListingNode::new(&state, &[]);
        let result = block_on(node.edit(1, &PathArgs::new(vec![], "")));
        assert!(matches!(result, SubmitResult::Handled));
    }

    #[test]
    fn edit_with_a_header_needs_an_interactive_guard() {
        // `AppState::for_test`'s manager carries no mode guard → `edit_text`
        // reports `Unsupported`, surfaced as `Error` (the engine shows it and
        // the menu stays open).
        let state = app_state();
        let mut node = ListingNode::new(&state, &[]).with_header(vec!["hdr".into()]);
        match block_on(node.edit(1, &PathArgs::new(vec![], ""))) {
            SubmitResult::Error(msg) => assert!(msg.contains("editor error"), "got: {msg}"),
            _ => panic!("expected Error without an interactive guard"),
        }
    }

    #[test]
    fn menu_wrapping_is_a_drop_in_for_static_listing() {
        // The decorator must be usable anywhere `StaticListingNode` was:
        // `.into()` + `add_child`, and eval through the engine like before.
        let state = app_state();
        let mut root: SlashDotMenuNode = ListingNode::new(&state, &[]).into();
        let mut child: SlashDotMenuNode =
            ListingNode::new(&state, &[StaticRow::new("a", "ay")]).into();
        child.add_child(ListingNode::new(&state, &[StaticRow::new("b", "bee")]).into());
        root.add_child(child);

        let mut menu = SlashDotMenu::from_root_node(root);
        let mut h = menu.get_handle();
        match block_on(h.eval_prompt("a ", HostKey::None, 80)) {
            MenuResult::Continue(..) => {}
            other => panic!("expected a Continue frame, got {:?}", other),
        }
    }
}
