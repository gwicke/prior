//! v2.5 `SlashDotNode` implementations for `/session`.
//!
//! `SessionListNode`: dynamic rows (one per session from the repo). When
//! drilled into a session UUID, `header()` renders the session info table
//! and the static action children (resume/fork/rename/delete) are shown.
//!
//! Action nodes are in this same file since they are small and tightly
//! coupled to `SessionListNode`.

use std::rc::Rc;

use crate::slash_menu_bridge::{AsyncOp, MenuShared};
use crate::slashdot_menu::fields::{FieldSpec, Row};
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, SlashDotNode, SubmitResult,
};
use crate::slashdot_menu::provider::ColumnDef;

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
        Some(name) if !name.is_empty() => format!("{} — {}", name, prompt),
        _ => prompt.to_string(),
    }
}

/// Relative path display for the session cwd, shown in the listing.
/// Returns the session's working directory relative to the current
/// working directory, or the basename if no relative path can be computed.
fn session_path_display(meta: &crate::repl::SessionMeta) -> String {
    let session_cwd = std::path::Path::new(&meta.cwd);
    match std::env::current_dir() {
        Ok(now) => crate::path_util::relativize_path(session_cwd, &now),
        Err(_) => session_cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| meta.cwd.clone()),
    }
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

/// Find session metadata by UUID from the repo.
fn find_session(
    shared: &MenuShared,
    session_id: &str,
) -> Option<crate::repl::SessionMeta> {
    let repo = shared.repo.as_ref()?;
    repo.list_sessions_sync(None)
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
// SessionListNode
// ---------------------------------------------------------------------------

/// Dynamic listing node for `/session`. `data()` returns one row per session
/// from the repo. When drilled into (a UUID selected), `header()` renders
/// the session info table, and the static action children are shown.
pub struct SessionListNode {
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for SessionListNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        use crate::slashdot_menu::value::Value;

        let current_id = self.shared.current_session_id.borrow().clone();
        let mut sessions: Vec<crate::repl::SessionMeta> = match &self.shared.repo {
            Some(repo) => repo
                .list_sessions_sync(None)
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
                    .field("age", Value::Str(relative_time(
                        m.last_prompt_timestamp.as_ref().unwrap_or(&m.timestamp),
                    )))
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

    fn header(&mut self, path_args: &PathArgs) -> Vec<String> {
        let Some(session_id) = drilled_session_id(path_args) else {
            return vec!["Select a session, then choose an action.".to_string()];
        };

        let meta = match find_session(&self.shared, session_id) {
            Some(m) => m,
            None => return vec![format!("Session not found: {}", session_id)],
        };

        let is_current = meta.id == *self.shared.current_session_id.borrow();
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

        let tw = path_args.term_width.max(1);

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
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for SessionResumeNode {
    fn data(&mut self, _epoch: u64, path_args: &PathArgs) -> DataResult {
        // Guard: hidden for the current session.
        if let Some(sid) = drilled_session_id(path_args) {
            if sid == self.shared.current_session_id.borrow().as_str() {
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

    fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
        vec![
            "Resume the session and replay the last N turns.".to_string(),
            "".to_string(),
            "Optional length limits:".to_string(),
            "  10      last 10 agent turns (same as 10t)".to_string(),
            "  10t     last 10 agent turns".to_string(),
            "  10l     last 10 output lines".to_string(),
        ]
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let session_id = action_session_id(path_args)
            .unwrap_or_default()
            .to_string();
        let replay_arg = path_args.suffix.trim().to_string();
        *self.shared.pending_op.borrow_mut() =
            Some(AsyncOp::Resume { session_id, replay_arg });
        SubmitResult::Done(None)
    }
}

/// Fork action leaf.
pub struct SessionForkNode {
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for SessionForkNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("fork").desc("branch from this point")],
            active_ids: Vec::new(),
        }))
    }

    fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
        vec![
            "Fork this session into a new one in the current directory.".to_string(),
            "The new session starts with the same history.".to_string(),
        ]
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let session_id = action_session_id(path_args)
            .unwrap_or_default()
            .to_string();
        *self.shared.pending_op.borrow_mut() = Some(AsyncOp::Fork { session_id });
        SubmitResult::Done(None)
    }
}

/// Rename action leaf.
pub struct SessionRenameNode {
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for SessionRenameNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("rename").desc("rename the session")],
            active_ids: Vec::new(),
        }))
    }

    fn header(&mut self, path_args: &PathArgs) -> Vec<String> {
        let current_name = action_session_id(path_args)
            .and_then(|sid| find_session(&self.shared, sid))
            .and_then(|m| m.name)
            .unwrap_or_else(|| "(unnamed)".to_string());
        vec![
            "Rename the session. Type the new name and press Enter.".to_string(),
            "Leave empty to clear the name.".to_string(),
            format!("Current name: {}", current_name),
        ]
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let session_id = action_session_id(path_args)
            .unwrap_or_default()
            .to_string();
        let new_name = path_args.suffix.trim().to_string();
        *self.shared.pending_op.borrow_mut() =
            Some(AsyncOp::Rename { session_id, new_name });
        SubmitResult::Done(None)
    }
}

/// Delete action leaf with two-phase confirm.
pub struct SessionDeleteNode {
    pub shared: Rc<MenuShared>,
}

impl SlashDotNode for SessionDeleteNode {
    fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        Ok(Rc::new(DataResponse {
            rows: vec![Row::new("delete").desc("delete from disk")],
            active_ids: Vec::new(),
        }))
    }

    fn header(&mut self, _path_args: &PathArgs) -> Vec<String> {
        vec![
            "Permanently delete this session and its history.".to_string(),
            "This action cannot be undone.".to_string(),
        ]
    }

    fn prepare(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        // Guard: cannot delete the current session.
        if let Some(sid) = action_session_id(path_args) {
            if sid == self.shared.current_session_id.borrow().as_str() {
                return SubmitResult::Done(Some(
                    "Cannot delete the current session.".to_string(),
                ));
            }
        }
        SubmitResult::Prepared
    }

    fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let session_id = action_session_id(path_args)
            .unwrap_or_default()
            .to_string();
        *self.shared.pending_op.borrow_mut() = Some(AsyncOp::Delete { session_id });
        SubmitResult::Done(None)
    }
}

// ---------------------------------------------------------------------------
// Menu builder
// ---------------------------------------------------------------------------

/// Build the `/session` subtree as a `SlashDotMenuNode`.
///
/// The listing node carries column definitions and field metadata;
/// `SessionListNode` provides dynamic session rows; its static children
/// are the action nodes.
pub fn build_session_node(
    shared: &Rc<MenuShared>,
) -> crate::slashdot_menu::menu::SlashDotMenuNode {
    use crate::slashdot_menu::menu::SlashDotMenuNode;
    use crate::slashdot_menu::node_impl::{StaticListingNode, StaticRow};

    let mut listing: SlashDotMenuNode = StaticListingNode::new(&[StaticRow::new(
        "session",
        "list, rename, resume, fork, or delete sessions",
    )])
    .with_columns(session_listing_columns())
    .with_fields(session_fields())
    .into();

    let mut session_node: SlashDotMenuNode =
        SessionListNode { shared: shared.clone() }.into();
    session_node.add_child(SessionResumeNode { shared: shared.clone() }.into());
    session_node.add_child(SessionForkNode { shared: shared.clone() }.into());
    session_node.add_child(SessionRenameNode { shared: shared.clone() }.into());
    session_node.add_child(
        SessionDeleteNode { shared: shared.clone() }.into(),
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
    use crate::slashdot_menu::node_impl::{HostKey, MenuResult, StaticListingNode};

    fn test_settings() -> crate::settings::SettingsStore {
        crate::settings::SettingsStore {
            default_model: String::new(),
            security_mode: String::new(),
            tool_call_limit: 0,
            default_persona: None,
            default_thinking_level: String::new(),
            show_reasoning: String::new(),
            reasoning_in_context: false,
            session_resume_replay_turns: 0,
            llm_compact_prompt: String::new(),
            path: None,
        }
    }

    fn make_shared() -> Rc<MenuShared> {
        Rc::new(MenuShared::new(test_settings(), None))
    }

    fn build_session_menu(shared: &Rc<MenuShared>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(build_session_node(shared));
        SlashDotMenu::from_root_node(root)
    }

    /// Write a minimal session `.jsonl` (header + one user message) into a
    /// fresh temp repo, returning `(shared, uuid, temp_dir)`.
    fn temp_repo_with_session() -> (Rc<MenuShared>, String, std::path::PathBuf) {
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
        let shared = Rc::new(MenuShared::new(test_settings(), Some(repo)));
        (shared, uuid, base)
    }

    /// End-to-end traversal: `/session ` listing → drill into the session →
    /// drill into `resume` → Enter → `submit()` sets an `AsyncOp::Resume`
    /// carrying the drilled UUID (not the action name).
    #[test]
    fn session_resume_full_traversal_sets_op_with_uuid() {
        let (shared, uuid, base) = temp_repo_with_session();
        // Not the current session, so resume is visible.
        *shared.current_session_id.borrow_mut() = String::new();

        let mut menu = build_session_menu(&shared);
        let mut handle = menu.get_handle();

        // 1. Parent listing shows the session row.
        let listing = handle.eval_prompt("session ", HostKey::None, 80);
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
        let r = handle.eval_prompt(&line, HostKey::None, 80);
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into session");

        // 3. Drill into `resume`.
        let line = format!("session {} resume ", uuid);
        let r = handle.eval_prompt(&line, HostKey::None, 80);
        assert!(matches!(r, MenuResult::Continue(_, _)), "drill into resume");

        // 4. Enter → submit.
        let _ = handle.eval_prompt(&line, HostKey::Enter, 80);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::Resume { session_id, .. }) => {
                assert_eq!(session_id, &uuid, "resume op must carry the session UUID");
            }
            other => panic!("expected AsyncOp::Resume, got {:?}", other),
        }
        drop(op);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn session_listing_shows_session_row() {
        let shared = make_shared();
        let mut menu = build_session_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("session ", HostKey::None, 80);
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
        let shared = make_shared();
        let mut menu = build_session_menu(&shared);
        let mut handle = menu.get_handle();
        let result = handle.eval_prompt("session ", HostKey::None, 80);
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
        let shared = make_shared();
        *shared.current_session_id.borrow_mut() = "test-uuid".to_string();
        let mut node = SessionResumeNode { shared: shared.clone() };
        // data() is called from the parent's context: path = ["session", uuid].
        let path_args = PathArgs {
            ids: vec!["session".to_string(), "test-uuid".to_string()],
            suffix: String::new(),
            args: Default::default(),
            term_width: 80,
        };
        let result = node.data(1, &path_args).unwrap();
        assert!(
            result.rows.is_empty(),
            "resume should be hidden for current session"
        );
    }

    #[test]
    fn session_resume_visible_for_other() {
        let shared = make_shared();
        *shared.current_session_id.borrow_mut() = "other-uuid".to_string();
        let mut node = SessionResumeNode { shared: shared.clone() };
        let path_args = PathArgs {
            ids: vec!["session".to_string(), "test-uuid".to_string()],
            suffix: String::new(),
            args: Default::default(),
            term_width: 80,
        };
        let result = node.data(1, &path_args).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].id, "resume");
    }

    #[test]
    fn session_resume_submit_sets_async_op() {
        let shared = make_shared();
        let mut node = SessionResumeNode { shared: shared.clone() };
        // submit() sees the node's own path: ["session", uuid, "resume"].
        let path_args = PathArgs {
            ids: vec!["session".into(), "some-uuid".into(), "resume".into()],
            suffix: " 10t ".to_string(),
            args: Default::default(),
            term_width: 80,
        };
        let _ = node.submit(1, &path_args);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::Resume { session_id, replay_arg }) => {
                assert_eq!(session_id, "some-uuid");
                assert_eq!(replay_arg, "10t");
            }
            other => panic!("expected Resume, got {:?}", other),
        }
    }

    #[test]
    fn session_fork_submit_sets_async_op() {
        let shared = make_shared();
        let mut node = SessionForkNode { shared: shared.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), "some-uuid".into(), "fork".into()],
            suffix: String::new(),
            args: Default::default(),
            term_width: 80,
        };
        let _ = node.submit(1, &path_args);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::Fork { session_id }) => {
                assert_eq!(session_id, "some-uuid");
            }
            other => panic!("expected Fork, got {:?}", other),
        }
    }

    #[test]
    fn session_rename_submit_sets_async_op() {
        let shared = make_shared();
        let mut node = SessionRenameNode { shared: shared.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), "some-uuid".into(), "rename".into()],
            suffix: " my-name ".to_string(),
            args: Default::default(),
            term_width: 80,
        };
        let _ = node.submit(1, &path_args);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::Rename { session_id, new_name }) => {
                assert_eq!(session_id, "some-uuid");
                assert_eq!(new_name, "my-name");
            }
            other => panic!("expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn session_rename_empty_clears() {
        let shared = make_shared();
        let mut node = SessionRenameNode { shared: shared.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), "some-uuid".into(), "rename".into()],
            suffix: "  ".to_string(),
            args: Default::default(),
            term_width: 80,
        };
        let _ = node.submit(1, &path_args);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::Rename { new_name, .. }) => {
                assert!(new_name.is_empty(), "empty suffix should clear name");
            }
            other => panic!("expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn session_delete_prepare_blocks_current() {
        let shared = make_shared();
        *shared.current_session_id.borrow_mut() = "test-uuid".to_string();
        let mut node = SessionDeleteNode { shared: shared.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), "test-uuid".into(), "delete".into()],
            suffix: String::new(),
            args: Default::default(),
            term_width: 80,
        };
        let result = node.prepare(1, &path_args);
        match result {
            SubmitResult::Done(Some(msg)) => {
                assert!(msg.contains("Cannot delete"));
            }
            _ => panic!("expected Done with error message"),
        }
    }

    #[test]
    fn session_delete_prepare_allows_other() {
        let shared = make_shared();
        *shared.current_session_id.borrow_mut() = "other-uuid".to_string();
        let mut node = SessionDeleteNode { shared: shared.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), "some-uuid".into(), "delete".into()],
            suffix: String::new(),
            args: Default::default(),
            term_width: 80,
        };
        let result = node.prepare(1, &path_args);
        assert!(
            matches!(result, SubmitResult::Prepared),
            "should allow delete of non-current session"
        );
    }

    #[test]
    fn session_delete_submit_sets_async_op() {
        let shared = make_shared();
        let mut node = SessionDeleteNode { shared: shared.clone() };
        let path_args = PathArgs {
            ids: vec!["session".into(), "some-uuid".into(), "delete".into()],
            suffix: String::new(),
            args: Default::default(),
            term_width: 80,
        };
        let _ = node.submit(1, &path_args);
        let op = shared.pending_op.borrow();
        match op.as_ref() {
            Some(AsyncOp::Delete { session_id }) => {
                assert_eq!(session_id, "some-uuid");
            }
            other => panic!("expected Delete, got {:?}", other),
        }
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
            term_width: 80,
        };
        assert_eq!(drilled_session_id(&listing), Some("uuid-1"));
        assert_eq!(action_session_id(&listing), Some("session"));

        let action = PathArgs {
            ids: vec!["session".into(), "uuid-1".into(), "resume".into()],
            suffix: String::new(),
            args: Default::default(),
            term_width: 80,
        };
        assert_eq!(drilled_session_id(&action), Some("resume"));
        assert_eq!(action_session_id(&action), Some("uuid-1"));

        // Degenerate: too short for an action path.
        let short = PathArgs {
            ids: vec!["session".into()],
            suffix: String::new(),
            args: Default::default(),
            term_width: 80,
        };
        assert_eq!(action_session_id(&short), None);
        let empty = PathArgs {
            ids: Vec::new(),
            suffix: String::new(),
            args: Default::default(),
            term_width: 80,
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
        assert_eq!(desc, "my session — fix the bug in main.rs");
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
}
