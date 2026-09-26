//! v2.5 `SlashDotNode` implementations for `/session`.
//!
//! `SessionListNode`: dynamic rows (one per session from the repo). When
//! drilled into a session UUID, `header()` renders the session info table
//! and the static action children (resume/fork/rename/delete) are shown.
//!
//! Action nodes are in this same file since they are small and tightly
//! coupled to `SessionListNode`.
use std::sync::Arc;

use std::rc::Rc;

use crate::app_state::AppState;
use crate::session::repo::SessionRepo;
use crate::session::Session;
use crate::slashdot_menu::fields::{FieldSpec, Row};
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode,
    SubmitResult,
};
use crate::slashdot_menu::fields::ColumnDef;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Format an ISO 8601 timestamp as a compact relative duration: "30s", "4m", "5h", "2d".
fn relative_time(ts: &str) -> String {
    let dt = chrono::DateTime::parse_from_rfc3339(ts)
        .or_else(|_| {
            chrono::DateTime::parse_from_str(
                &format!("{}Z", ts.trim_end_matches('Z')),
                "%+",
            )
        });
    let Ok(dt) = dt else { return "?".to_string() };
    let elapsed = chrono::Utc::now() - dt.with_timezone(&chrono::Utc);
    let secs = elapsed.num_seconds().max(0);
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Format an ISO 8601 timestamp as a display string: "today 14:32",
/// "yesterday 09:15", or "Jul 14 14:32".
fn format_session_timestamp(ts: &str) -> String {
    let date_part = &ts[..10];
    let time_part = if ts.len() >= 16 { &ts[11..16] } else { "??:??" };

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    let day_label = if date_part == today {
        "today"
    } else if date_part == yesterday {
        "yesterday"
    } else {
        let month = ts.get(5..7).unwrap_or("??");
        let day = ts.get(8..10).unwrap_or("??");
        let month_name = match month {
            "01" => "Jan", "02" => "Feb", "03" => "Mar", "04" => "Apr",
            "05" => "May", "06" => "Jun", "07" => "Jul", "08" => "Aug",
            "09" => "Sep", "10" => "Oct", "11" => "Nov", "12" => "Dec",
            _ => month,
        };
        return format!("{} {} {}", month_name, day, time_part);
    };
    format!("{} {}", day_label, time_part)
}

/// Compact ISO timestamp to minute resolution: "2026-09-21 20:22Z".
fn compact_timestamp(ts: &str) -> String {
    // "2026-09-21T20:22:30.819Z" → "2026-09-21 20:22Z"
    if ts.len() >= 16 {
        let date = &ts[..10];
        let time = &ts[11..16];
        format!("{} {}Z", date, time)
    } else {
        ts.to_string()
    }
}

/// Description for a session row: `"<name> — <first_prompt>"` or just
/// `<first_prompt>` when no name is set.
fn session_description(meta: &crate::repl::SessionMeta) -> String {
    let prompt = meta
        .first_prompt_preview
        .as_deref()
        .unwrap_or("(empty)");
    match &meta.name {
        Some(name) if !name.is_empty() => format!("{}: {}", name, prompt),
        _ => prompt.to_string(),
    }
}

/// Relative path display for the session cwd, shown in the listing.
fn session_path_display(meta: &crate::repl::SessionMeta) -> String {
    crate::path_util::relativize_for_display(std::path::Path::new(&meta.cwd))
}

/// Resolve the session UUID from `path_args.ids` in the *listing* context
/// (path `["session", "<uuid>"]`): the UUID is the last element. Used by
/// `SessionListNode::header()` and by the action nodes' `data()`, which the
/// parent calls with the parent's path (per the v2.5 traversal: `refresh_data`
/// passes the parent's path down to children).
fn drilled_session_id(path_args: &PathArgs) -> Option<&str> {
    path_args.ids.last().map(|s| s.as_str())
}

/// Resolve the session UUID from `path_args.ids` in an *action node's own*
/// context (path `["session", "<uuid>", "<action>"]`): the UUID is the
/// second-to-last element. Used by action `header()` / `submit()` /
/// `prepare()`, which receive the node's own path (own ID included).
fn action_session_id(path_args: &PathArgs) -> Option<&str> {
    let ids = &path_args.ids;
    if ids.len() >= 2 {
        ids.get(ids.len() - 2).map(|s| s.as_str())
    } else {
        None
    }
}

/// Find session metadata by UUID from the repo (async overlay listing —
/// cache-first; see `list_sessions_overlay`).
async fn find_session(
    app_state: &AppState,
    session_id: &str,
) -> Option<crate::repl::SessionMeta> {
    let repo = app_state.repo()?;
    repo.list_sessions_overlay(None)
        .await
        .into_iter()
        .map(crate::repl::session_meta_from)
        .find(|m| m.id == session_id)
}

// ---------------------------------------------------------------------------
// Column definitions
// ---------------------------------------------------------------------------

/// Column definitions for the parent `/session` listing (description,
/// path, turns, age — no ID column since UUIDs are not useful to the user).
pub fn session_listing_columns() -> Vec<ColumnDef> {
    use crate::slashdot_menu::table::Align;
    vec![
        ColumnDef::new("description", "Description")
            .min(20)
            .prio(3)
            .wrap(false),
        ColumnDef::new("path", "Path")
            .min(10)
            .prio(2)
            .wrap(false),
        ColumnDef::new("msgs", "Turns")
            .align(Align::Right)
            .min(5)
            .prio(1)
            .wrap(false),
        ColumnDef::new("age", "Age")
            .align(Align::Right)
            .min(8)
            .prio(1)
            .wrap(false),
    ]
}

/// Column definitions for `SessionListNode` when drilled into a session.
/// Shows the action ID (resume/fork/rename/delete) plus description.
pub fn session_action_columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef::new("id", "Action").min(10).prio(3),
        ColumnDef::new("description", "Description").min(20).prio(2),
    ]
}

/// Field metadata for the session listing.
pub fn session_fields() -> Vec<FieldSpec> {
    vec![
        FieldSpec::new("path", false),
        FieldSpec::new("msgs", true),
        FieldSpec::new("age", true),
    ]
}

// ---------------------------------------------------------------------------
// SessionListingNode — parent listing node for /session
// ---------------------------------------------------------------------------

/// Parent listing node for `/session`. Provides the static "session" row,
/// column definitions, field metadata, and `drill_rewrite` (adds default
/// `.path:.` filter on drill). `SessionListNode` is its dynamic child.
pub struct SessionListingNode;

#[async_trait::async_trait(?Send)]
impl SlashDotNode for SessionListingNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("session")
                .desc("list, rename, resume, fork, or delete sessions")],
            active_ids: Vec::new(),
        }))
    }

    fn columns(&self) -> Vec<ColumnDef> {
        session_listing_columns()
    }

    fn fields(&self) -> Vec<FieldSpec> {
        session_fields()
    }

    async fn drill_rewrite(&mut self, _path_args: &PathArgs) -> Option<String> {
        // On drill, add a default path filter to show only sessions
        // relevant to the current working directory.
        Some("session .path:.".to_string())
    }
}

// ---------------------------------------------------------------------------
// SessionListNode
// ---------------------------------------------------------------------------

/// Dynamic listing node for `/session`. `data()` returns one row per session
/// from the repo. When drilled into (a UUID selected), `header()` renders
/// the session info table, and the static action children are shown.
pub struct SessionListNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for SessionListNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        use crate::slashdot_menu::value::Value;

        let current_id = self.app_state.current_session_id();
        let mut sessions: Vec<crate::repl::SessionMeta> = match self.app_state.repo() {
            Some(repo) => repo
                .list_sessions_overlay(None)
                .await
                .into_iter()
                .map(crate::repl::session_meta_from)
                .collect(),
            None => Vec::new(),
        };
        // Current session first, then by recency.
        sessions.sort_by(|a, b| {
            let a_cur = a.id == current_id;
            let b_cur = b.id == current_id;
            if a_cur && !b_cur {
                std::cmp::Ordering::Less
            } else if !a_cur && b_cur {
                std::cmp::Ordering::Greater
            } else {
                let a_ts = a.last_prompt_timestamp.as_ref().unwrap_or(&a.timestamp);
                let b_ts = b.last_prompt_timestamp.as_ref().unwrap_or(&b.timestamp);
                b_ts.cmp(a_ts)
            }
        });

        let rows: Vec<Row> = sessions
            .iter()
            .map(|m| {
                let desc = session_description(m);
                Row::new(m.id.clone())
                    .desc(&desc)
                    .field("path", Value::Str(session_path_display(m)))
                    .field("msgs", Value::Num(m.user_message_count as f64))
                    .field("age", Value::Num(
                            m.last_prompt_timestamp.clone()
                            .unwrap_or(m.timestamp.clone())
                            .parse().unwrap_or(0.0))
                    )
                    .display_field(
                        "age",
                        relative_time(m.last_prompt_timestamp.as_ref().unwrap_or(&m.timestamp))
                    )

            })
            .collect();

        // Only mark active if the current session is actually in the listing.
        let active_ids = if current_id.is_empty()
            || !sessions.iter().any(|s| s.id == current_id)
        {
            Vec::new()
        } else {
            vec![current_id]
        };

        Ok(Rc::new(DataResponse { rows, active_ids }))
    }

    fn columns(&self) -> Vec<ColumnDef> {
        session_action_columns()
    }

    fn fields(&self) -> Vec<FieldSpec> {
        session_fields()
    }

    async fn header(&mut self, path_args: &PathArgs, render_ctx: &RenderContext) -> Vec<String> {
        let Some(session_id) = drilled_session_id(path_args) else {
            return vec!["Select a session, then choose an action.".to_string()];
        };

        let meta = match find_session(&self.app_state, session_id).await {
            Some(m) => m,
            None => return vec![format!("Session not found: {}", session_id)],
        };

        let is_current = meta.id == self.app_state.current_session_id();
        let ts = format_session_timestamp(&meta.timestamp);
        let name = meta.name.as_deref().unwrap_or(&meta.id);
        let marker = if is_current { "◉ " } else { "  " };

        let parent_display = meta
            .parent_session
            .as_ref()
            .map(|p| format!("parent: {}", p.rsplit('/').next().unwrap_or(p)))
            .unwrap_or_else(|| {
                format!(
                    "dir: {}",
                    meta.cwd.rsplit('/').next().unwrap_or(&meta.cwd)
                )
            });

        let tw = render_ctx.term_width.max(1);

        // --- Summary as a wrapping table ---
        let summary_text = format!(
            "{}{}",
            marker, name,
        );
        use crate::slashdot_menu::table::{Align, Table as SdTable};
        let mut info_table = SdTable::new(&["Session", "Turns", "Msgs", "Age", ""])
            .with_aligns(&[Align::Left, Align::Right, Align::Right, Align::Right, Align::Left])
            .with_min_widths(&[20, 5, 5, 5, 10]);
        info_table.add_row(&[
            &summary_text,
            &meta.user_message_count.to_string(),
            &meta.message_count.to_string(),
            &ts,
            &parent_display,
        ]);
        let mut lines: Vec<String> = info_table
            .render_multiline(tw.saturating_sub(2))
            .into_iter()
            .flat_map(|row| row.split('\n').map(|s| s.to_string()).collect::<Vec<_>>())
            .collect();

        // --- First / Last as a two-column wrapping table ---
        let first = meta.first_prompt_preview.as_deref().unwrap_or("(none)").to_string();
        let last_text = meta
            .last_prompt_text
            .clone()
            .or_else(|| {
                let path = meta.path.as_ref()?;
                let detail =
                    crate::session::repo::JsonlSessionRepo::read_metadata_detail(path)?;
                detail.last_prompt_text
            })
            .unwrap_or_else(|| "(none)".to_string());
        let last_ts = meta.last_prompt_timestamp.as_deref().unwrap_or("?");

        let mut msg_table = SdTable::new(&["", ""])
            .with_aligns(&[Align::Left, Align::Left])
            .with_min_widths(&[6, 10])
            .with_priorities(&[1, 3]);
        msg_table.add_row(&["First:", &first]);
        msg_table.add_row(&["Last:", &format!("{} {}", compact_timestamp(last_ts), last_text)]);

        // Append First/Last rows, splitting wrapped lines.
        let msg_rows = msg_table.render_multiline(tw.saturating_sub(4));
        for row in &msg_rows {
            for sub in row.split('\n') {
                lines.push(format!("  {}", sub));
            }
        }

        lines
    }
}

// ---------------------------------------------------------------------------
// Action nodes
// ---------------------------------------------------------------------------

/// Resume action leaf. Hidden when viewing the current session.
pub struct SessionResumeNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for SessionResumeNode {
    async fn data(&mut self, _epoch: u64, path_args: &PathArgs) -> DataResult {
        // Guard: hidden for the current session.
        if let Some(sid) = drilled_session_id(path_args) {
            if sid == self.app_state.current_session_id() {
                return Ok(Rc::new(DataResponse {
                    rows: Vec::new(),
                    active_ids: Vec::new(),
                }));
            }
        }
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("resume").desc("continue the session")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Resume the session and replay the last N turns.".to_string(),
            "".to_string(),
            "Optional length limits:".to_string(),
            "  10      last 10 agent turns (same as 10t)".to_string(),
            "  10t     last 10 agent turns".to_string(),
            "  10l     last 10 output lines".to_string(),
        ]
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let session_id = action_session_id(path_args)
            .unwrap_or_default()
            .to_string();
        let replay_arg = path_args.suffix.trim().to_string();
        // Data-plane inline: load/migrate/replay + activate in one slot
        // write; the model decision rides the prefill register (qualified
        // id from the TARGET config — same target-stamp judgment as
        // session creation).
        let Some(repo) = self.app_state.repo() else {
            return SubmitResult::Done(Some(format!("Session not found: {}", session_id)));
        };
        let found = repo
            .list_sessions_overlay(None)
            .await
            .into_iter()
            .find(|m| m.id == session_id);
        let Some(meta) = found else {
            return SubmitResult::Done(Some(format!("Session not found: {}", session_id)));
        };
        let default = self.app_state.with_settings(|s| s.session_resume_replay_turns);
        let replay = if replay_arg.is_empty() {
            (default, false)
        } else {
            crate::agent::parse_replay_len_arg(&replay_arg, default)
        };
        let out = self.app_state.with_config(|c| c.output.clone());
        let qualified = self.app_state.with_config(|c| c.qualified_model_id());
        match crate::agent::resume_session(&repo, &meta, replay, out.as_ref(), &qualified).await {
            Ok((new_session, id, prefill)) => {
                self.app_state.activate_session(Some(new_session), id);
                if let Some(p) = prefill {
                    self.app_state.set_prefill(p);
                }
                SubmitResult::Done(None)
            }
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

/// Fork action leaf.
pub struct SessionForkNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for SessionForkNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("fork").desc("branch from this point")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Fork this session into a new one in the current directory.".to_string(),
            "The new session starts with the same history.".to_string(),
        ]
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let session_id = action_session_id(path_args)
            .unwrap_or_default()
            .to_string();
        // Data-plane inline: resolve the meta by id, fork, and activate —
        // the slot write bundles session + current id (same effect as the
        // old Switched path).
        let found = match self.app_state.repo() {
            Some(repo) => repo
                .list_sessions_overlay(None)
                .await
                .into_iter()
                .find(|m| m.id == session_id)
                .map(|m| (repo, m)),
            None => None,
        };
        let Some((repo, meta)) = found else {
            return SubmitResult::Done(Some(format!("Session not found: {}", session_id)));
        };
        match crate::agent::fork_session_at(&repo, &meta).await {
            Ok((new_session, id)) => {
                self.app_state.activate_session(Some(new_session), id);
                SubmitResult::Done(None)
            }
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }
}

/// Rename action leaf.
pub struct SessionRenameNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for SessionRenameNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("rename").desc("rename the session")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(&mut self, path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        let current_name = match action_session_id(path_args) {
            Some(sid) => find_session(&self.app_state, sid)
                .await
                .and_then(|m| m.name)
                .unwrap_or_else(|| "(unnamed)".to_string()),
            None => "(unnamed)".to_string(),
        };
        vec![
            "Rename the session. Type the new name and press Enter.".to_string(),
            "Leave empty to clear the name.".to_string(),
            format!("Current name: {}", current_name),
        ]
    }

    async fn drill_rewrite(&mut self, path_args: &PathArgs) -> Option<String> {
        // Pre-fill the current session name. ids are
        // ["session", "<uuid>", "rename"].
        let sid = path_args.ids.get(1).map(|s| s.as_str()).unwrap_or_default();
        let current_name = find_session(&self.app_state, sid)
            .await
            .and_then(|m| m.name)
            .unwrap_or_default();
        // Rebuild the full prompt: "session <uuid> rename <name>"
        // (leading space preserves the trailing-space behavior of the old
        // `format!("{} {}", prefix, name)` for an empty name).
        Some(path_args.prompt_with(&format!(" {}", current_name)))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let session_id = action_session_id(path_args)
            .unwrap_or_default()
            .to_string();
        let new_name = path_args.suffix.trim().to_string();
        if new_name.is_empty() {
            return SubmitResult::Done(Some(
                "Usage: /session <name> rename <newname>".to_string(),
            ));
        }
        // Data-plane inline: the active session renames in place; any
        // other id goes through the repo (open → set → invalidate).
        if let Some(s) = self.app_state.session() {
            if s.get_session_id() == session_id {
                let _ = s.set_session_name(&new_name).await;
                if let Some(repo) = self.app_state.repo() {
                    repo.invalidate_cache();
                }
                return SubmitResult::Done(None);
            }
        }
        let Some(repo) = self.app_state.repo() else {
            return SubmitResult::Done(Some(format!("Session not found: {}", session_id)));
        };
        let meta = repo
            .list_sessions_overlay(None)
            .await
            .into_iter()
            .find(|m| m.id == session_id);
        match meta {
            Some(meta) => match repo.open(&meta).await {
                Ok((storage, index)) => {
                    let s = Session::new_persisted(storage, index);
                    let _ = s.set_session_name(&new_name).await;
                    repo.invalidate_cache();
                    SubmitResult::Done(None)
                }
                Err(e) => SubmitResult::Done(Some(format!("Cannot open session: {}", e))),
            },
            None => SubmitResult::Done(Some(format!("Session not found: {}", session_id))),
        }
    }
}

/// Delete action leaf with two-phase confirm.
pub struct SessionDeleteNode {
    pub app_state: Arc<AppState>,
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for SessionDeleteNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("delete").desc("delete from disk")],
            active_ids: Vec::new(),
        }))
    }

    async fn header(
        &mut self,
        _path_args: &PathArgs,
        _render_ctx: &RenderContext,
    ) -> Vec<String> {
        vec![
            "Permanently delete this session and its history.".to_string(),
            "This action cannot be undone.".to_string(),
        ]
    }

    async fn prepare(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        // Guard: cannot delete the current session.
        if let Some(sid) = action_session_id(path_args) {
            if sid == self.app_state.current_session_id() {
                return SubmitResult::Done(Some(
                    "Cannot delete the current session.".to_string(),
                ));
            }
        }
        SubmitResult::Prepared
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let session_id = action_session_id(path_args)
            .unwrap_or_default()
            .to_string();
        // Data-plane inline: locate the meta by id, keep the current-
        // session guard (mirrors `prepare()`), delete via the repo
        // (which invalidates the listing cache itself).
        let Some(repo) = self.app_state.repo() else {
            return SubmitResult::Done(Some(format!("Session not found: {}", session_id)));
        };
        let meta = repo
            .list_sessions_overlay(None)
            .await
            .into_iter()
            .find(|m| m.id == session_id);
        let Some(meta) = meta else {
            return SubmitResult::Done(Some(format!("Session not found: {}", session_id)));
        };
        if self.app_state.current_session_id() == session_id {
            return SubmitResult::Done(Some("Cannot delete the current session".to_string()));
        }
        match repo.delete(&meta).await {
            Ok(()) => SubmitResult::Done(None),
            Err(e) => SubmitResult::Done(Some(format!("Cannot delete session: {}", e))),
        }
    }
}

// ---------------------------------------------------------------------------
// Menu builder
// ---------------------------------------------------------------------------

/// Build the `/session` subtree as a `SlashDotMenuNode`.
///
/// `SessionListingNode` provides the static "session" row, columns,
/// fields, and `drill_rewrite`. `SessionListNode` (its child) provides
/// dynamic session rows; action nodes are grandchildren.
pub fn build_session_node(
    app_state: &Arc<AppState>,
) -> crate::slashdot_menu::menu::SlashDotMenuNode {
    use crate::slashdot_menu::menu::SlashDotMenuNode;

    let mut listing: SlashDotMenuNode = SessionListingNode.into();

    let mut session_node: SlashDotMenuNode =
        SessionListNode { app_state: app_state.clone() }.into();
    session_node.add_child(SessionResumeNode { app_state: app_state.clone() }.into());
    session_node.add_child(SessionForkNode { app_state: app_state.clone() }.into());
    session_node.add_child(SessionRenameNode { app_state: app_state.clone() }.into());
    session_node.add_child(
        SessionDeleteNode { app_state: app_state.clone() }.into(),
    );

    listing.add_child(session_node);
    listing
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{MenuResult, StaticListingNode};
    use crate::slashdot_menu::HostKey;

    fn test_settings() -> crate::settings::SettingsStore {
        crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_thinking_level: String::new(),
            show_reasoning: String::new(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            path: None,
        }
    }

    /// Returns an `AppState` (binding kept as `app_state` in tests below —
    /// it is the injected source-of-truth handle).
    fn test_app_state() -> Arc<AppState> {
        crate::app_state::AppState::for_test(test_settings())
    }

    fn build_session_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(build_session_node(app_state));
        SlashDotMenu::from_root_node(root)
    }

    /// Write a minimal session `.jsonl` (header + one user message) into a
    /// fresh temp repo, returning `(app_state, uuid, temp_dir)`.
    fn temp_repo_with_session() -> (Arc<AppState>, String, std::path::PathBuf) {
        use crate::session::types::{encode_cwd, session_filename, SessionHeader};

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("v25-session-test-{nanos}"));
        let cwd = std::path::Path::new("/tmp/v25-cwd");
        let dir = base.join(encode_cwd(cwd));
        std::fs::create_dir_all(&dir).unwrap();

        let header = SessionHeader::new(cwd);
        let uuid = header.id.clone();
        let path = dir.join(session_filename(&chrono::Utc::now()));
        let user_entry = serde_json::json!({
            "id": "e1",
            "timestamp": "2025-01-01T00:00:00.000Z",
            "type": "message",
            "message": {"role": "user", "content": [{"type": "text", "text": "hello world"}]},
        });
        let leaf = serde_json::json!({
            "id": "leaf1",
            "timestamp": "2025-01-01T00:00:01.000Z",
            "type": "leaf",
            "targetId": "e1",
        });
        let contents = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&user_entry).unwrap(),
            serde_json::to_string(&leaf).unwrap(),
        );
        std::fs::write(&path, contents).unwrap();

        let repo = crate::session::repo::JsonlSessionRepo::new(base.clone());
        let app_state = crate::app_state::AppState::for_test(test_settings());
        app_state.set_repo(Some(repo));
        (app_state, uuid, base)
    }

    /// End-to-end traversal: `/session ` listing → drill into the session →
    /// drill into `resume` → Enter → `submit()` writes the `resume`
    /// control target carrying the drilled UUID (not the action name).
    #[test]
    fn session_resume_full_traversal_sets_op_with_uuid() {
        let (app_state, uuid, base) = temp_repo_with_session();
        // Not the current session, so resume is visible.
        app_state.set_current_session_id(String::new());

        let mut menu = build_session_menu(&app_state);
        let mut handle = menu.get_handle();

        // 1. Parent listing shows the session row.
        let listing = crate::agent::block_on_test(handle.eval_prompt("session ", HostKey::None, 80));
        match &listing {
            MenuResult::Continue(plan, _) => {
                assert!(
                    !plan.rows.is_empty(),
                    "session listing should have the temp session"
                );
            }
            other => panic!("expected Continue at listing, got {:?}", std::mem::discriminant(other)),
        }

        // 2. Drill into the session (bare-id entry with trailing space).
        let line = format!("session {} ", uuid);
        let r = crate::agent::block_on_test(handle.eval_prompt(&line, HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into session");

        // 3. Drill into `resume`.
        let line = format!("session {} resume ", uuid);
        let r = crate::agent::block_on_test(handle.eval_prompt(&line, HostKey::None, 80));
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into resume");

        // 4. Enter → submit (inline resume drives repo IO — tokio-backed
        // driver required). The fixture lives under a foreign cwd, so
        // resume takes the fork-migration path: original deleted, a
        // migrated session activated under the process cwd.
        let _ = crate::agent::block_on_test(handle.eval_prompt(&line, HostKey::Enter, 80));
        let s = app_state.session().expect("resume activates a session");
        assert!(!s.get_session_id().is_empty());
        assert_eq!(
            app_state.current_session_id(),
            s.get_session_id(),
            "slot id tracks the activated session"
        );
        assert_eq!(
            app_state.repo().unwrap().list_sessions_sync(None).len(),
            1,
            "migration: original deleted, one session remains"
        );
        // The (migrated) header records no model → no `/model` prefill.
        assert!(app_state.take_prefill().is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_listing_shows_session_row() {
        let app_state = test_app_state();
        let mut menu = build_session_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = crate::agent::block_on_test(handle.eval_prompt("session ", HostKey::None, 80));
        match result {
            MenuResult::Continue(plan, _) => {
                // With no repo, listing should be empty.
                assert!(plan.rows.is_empty(), "no repo → empty listing");
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn session_node_columns() {
        let app_state = test_app_state();
        let mut menu = build_session_menu(&app_state);
        let mut handle = menu.get_handle();
        let result = crate::agent::block_on_test(handle.eval_prompt("session ", HostKey::None, 80));
        match result {
            MenuResult::Continue(plan, _) => {
                // With no repo, the table header row should still render
                // (columns are declared on the listing node).
                // Empty listing means no table header, but columns are declared.
                // Just verify the plan renders without panic.
                let _ = &plan.table_header;
                let _ = &plan.detail;
            }
            other => panic!("expected Continue, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn session_resume_hidden_for_current() {
        let app_state = test_app_state();
        app_state.set_current_session_id("test-uuid".to_string());
        let mut node = SessionResumeNode { app_state: app_state.clone() };
        // data() is called from the parent's context: path = ["session", uuid].
        let path_args = PathArgs {
            ids: vec!["session".to_string(), "test-uuid".to_string()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.data(1, &path_args)).unwrap();
        assert!(
            result.rows.is_empty(),
            "resume should be hidden for current session"
        );
    }

    #[test]
    fn session_resume_visible_for_other() {
        let app_state = test_app_state();
        app_state.set_current_session_id("other-uuid".to_string());
        let mut node = SessionResumeNode { app_state: app_state.clone() };
        let path_args = PathArgs {
            ids: vec!["session".to_string(), "test-uuid".to_string()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.data(1, &path_args)).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].id, "resume");
    }

    #[test]
    fn session_resume_submit_switches_session() {
        let (app_state, uuid, base) = temp_repo_with_session();
        let mut node = SessionResumeNode { app_state: app_state.clone() };
        // submit() sees the node's own path: ["session", uuid, "resume"].
        let path_args = PathArgs {
            ids: vec!["session".into(), uuid.clone(), "resume".into()],
            suffix: " 10t ".to_string(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(matches!(result, SubmitResult::Done(None)), "resume must succeed");
        let s = app_state.session().expect("activated");
        // Foreign-cwd fixture → fork migration: activated id differs from
        // the original, and the slot id tracks it.
        assert_ne!(s.get_session_id(), uuid, "migrated session has its own id");
        assert_eq!(app_state.current_session_id(), s.get_session_id());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_fork_submit_switches_to_fork() {
        let (app_state, uuid, base) = temp_repo_with_session();
        let mut node = SessionForkNode { app_state: app_state.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), uuid.clone(), "fork".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(matches!(result, SubmitResult::Done(None)), "fork must succeed");
        let s = app_state.session().expect("fork activated into the slot");
        assert_ne!(s.get_session_id(), uuid, "a fork is a new session");
        assert_eq!(app_state.current_session_id(), s.get_session_id());
        // Source file (encoded fixture dir) + fork (encoded process-cwd
        // dir) both under the temp base.
        assert_eq!(
            app_state.repo().unwrap().list_sessions_sync(None).len(),
            2,
            "source + fork on disk"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_rename_submit_renames_active_session() {
        let app_state = test_app_state();
        let session = crate::agent::test_session_with_entries(vec![]);
        let sid = session.get_session_id().to_string();
        app_state.set_session(Some(session.clone()));
        let mut node = SessionRenameNode { app_state: app_state.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), sid, "rename".into()],
            suffix: " my-name ".to_string(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(matches!(result, SubmitResult::Done(None)));
        let name = crate::agent::block_on_test(async {
            session.index().lock().await.session_name.clone()
        });
        assert_eq!(
            name.as_deref(),
            Some("my-name"),
            "rename must set the session's name in place"
        );
    }

    #[test]
    fn session_rename_empty_shows_usage() {
        let app_state = test_app_state();
        let mut node = SessionRenameNode { app_state: app_state.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), "some-uuid".into(), "rename".into()],
            suffix: "  ".to_string(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        match result {
            SubmitResult::Done(Some(msg)) => {
                assert!(msg.contains("Usage: /session"), "usage message: {msg}")
            }
            _ => panic!("expected Done(usage message)"),
        }
    }

    #[test]
    fn session_delete_prepare_blocks_current() {
        let app_state = test_app_state();
        app_state.set_current_session_id("test-uuid".to_string());
        let mut node = SessionDeleteNode { app_state: app_state.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), "test-uuid".into(), "delete".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.prepare(1, &path_args));
        match result {
            SubmitResult::Done(Some(msg)) => {
                assert!(msg.contains("Cannot delete"));
            }
            _ => panic!("expected Done with error message"),
        }
    }

    #[test]
    fn session_delete_prepare_allows_other() {
        let app_state = test_app_state();
        app_state.set_current_session_id("other-uuid".to_string());
        let mut node = SessionDeleteNode { app_state: app_state.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), "some-uuid".into(), "delete".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.prepare(1, &path_args));
        assert!(
            matches!(result, SubmitResult::Prepared),
            "should allow delete of non-current session"
        );
    }

    #[test]
    fn session_delete_submit_removes_file() {
        let (app_state, uuid, base) = temp_repo_with_session();
        let repo = app_state.repo().expect("fixture repo in the slot");
        assert_eq!(repo.list_sessions_sync(None).len(), 1, "fixture present");
        let mut node = SessionDeleteNode { app_state: app_state.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), uuid, "delete".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        let result = crate::agent::block_on_test(node.submit(1, &path_args));
        assert!(matches!(result, SubmitResult::Done(None)), "delete must succeed");
        assert!(
            repo.list_sessions_sync(None).is_empty(),
            "the file must be gone from disk"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn relative_time_now() {
        let now = chrono::Utc::now().to_rfc3339();
        let age = relative_time(&now);
        assert!(age.ends_with('s'), "just now should be seconds: {}", age);
    }

    #[test]
    fn relative_time_hours() {
        let ts = (chrono::Utc::now() - chrono::Duration::hours(3)).to_rfc3339();
        let age = relative_time(&ts);
        assert_eq!(age, "3h");
    }

    #[test]
    fn relative_time_days() {
        let ts = (chrono::Utc::now() - chrono::Duration::days(5)).to_rfc3339();
        let age = relative_time(&ts);
        assert_eq!(age, "5d");
    }

    /// Path-semantics contract: `drilled_session_id` takes the LAST element
    /// (listing/parent context), `action_session_id` takes the second-to-last
    /// (an action node's own context, where the last element is the action).
    #[test]
    fn path_id_helpers_pick_the_right_element() {
        let listing = PathArgs {
            ids: vec!["session".into(), "uuid-1".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        assert_eq!(drilled_session_id(&listing), Some("uuid-1"));
        assert_eq!(action_session_id(&listing), Some("session"));

        let action = PathArgs {
            ids: vec!["session".into(), "uuid-1".into(), "resume".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        assert_eq!(drilled_session_id(&action), Some("resume"));
        assert_eq!(action_session_id(&action), Some("uuid-1"));

        // Degenerate: too short for an action path.
        let short = PathArgs {
            ids: vec!["session".into()],
            suffix: String::new(),
            args: Default::default(),
        };
        assert_eq!(action_session_id(&short), None);
        let empty = PathArgs {
            ids: Vec::new(),
            suffix: String::new(),
            args: Default::default(),
        };
        assert_eq!(action_session_id(&empty), None);
        assert_eq!(drilled_session_id(&empty), None);
    }

    #[test]
    fn session_description_with_name() {
        let meta = crate::repl::SessionMeta {
            id: "test".into(),
            name: Some("my session".into()),
            timestamp: "t".into(),
            cwd: "/test".into(),
            parent_session: None,
            message_count: 5,
            user_message_count: 3,
            first_prompt_preview: Some("fix the bug in main.rs".into()),
            last_prompt_text: None,
            last_prompt_timestamp: None,
            path: None,
        };
        let desc = session_description(&meta);
        assert_eq!(desc, "my session: fix the bug in main.rs");
    }

    #[test]
    fn session_description_without_name() {
        let meta = crate::repl::SessionMeta {
            id: "test".into(),
            name: None,
            timestamp: "t".into(),
            cwd: "/test".into(),
            parent_session: None,
            message_count: 5,
            user_message_count: 3,
            first_prompt_preview: Some("fix the bug".into()),
            last_prompt_text: None,
            last_prompt_timestamp: None,
            path: None,
        };
        let desc = session_description(&meta);
        assert_eq!(desc, "fix the bug");
    }

    #[test]
    fn session_description_empty_preview() {
        let meta = crate::repl::SessionMeta {
            id: "test".into(),
            name: None,
            timestamp: "t".into(),
            cwd: "/test".into(),
            parent_session: None,
            message_count: 0,
            user_message_count: 0,
            first_prompt_preview: None,
            last_prompt_text: None,
            last_prompt_timestamp: None,
            path: None,
        };
        let desc = session_description(&meta);
        assert_eq!(desc, "(empty)");
    }

    // --- drill_rewrite tests ---

    #[test]
    fn session_list_drill_rewrite_adds_path_filter() {
        use crate::slashdot_menu::node_impl::SlashDotNode;
        let mut node = SessionListingNode;
        let path = vec!["session".to_string()];
        let result = crate::agent::block_on_test(node.drill_rewrite(&PathArgs::new(path, " ")));
        assert_eq!(result, Some("session .path:.".to_string()));
    }

    #[test]
    fn session_rename_drill_rewrite_prefills_name() {
        use crate::slashdot_menu::node_impl::SlashDotNode;
        // No repo → find_session returns None → empty name.
        let app_state = test_app_state();
        let mut node = SessionRenameNode { app_state: app_state.clone() };
        let path = vec!["session".to_string(), "abc123".to_string(), "rename".to_string()];
        let result = crate::agent::block_on_test(node.drill_rewrite(&PathArgs::new(path, " ")));
        assert_eq!(result, Some("session abc123 rename ".to_string()));
    }
}
