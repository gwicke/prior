//! Two ways a conversation gets shorter than the transcript behind it:
//! compaction (collapse a span into a summary) and the branch summary (record
//! what an explored-and-returned-from branch was about).
//!
//! Both are generate-then-apply, and both are two-phase on purpose. Generation
//! never moves the leaf — it writes a draft the user can edit, and only
//! `apply_*` commits it. The caller does the leaf dance
//! (`/tree <id> compact llm` moves the leaf to the pivot, generates, then
//! restores) so the summarization request sees exactly the conversation prefix
//! a normal prompt at that position would have seen, and so the provider's
//! prefix cache stays warm across the span.
//!
//! `retry_summarization` is the shared driver: both generators call it, and it
//! is the one place the auto-retry policy for a *summarization* request lives.
//! A summarization that 429s should back off and try again like any other
//! turn, and should stop promptly on `Esc` — the same two pumps, the same
//! cancel signal.
//!
//! Both write pi-format summary messages, so what lands in the session is
//! what a pi session would have written.

use super::pumps::{cancel_observed, cancellable_sleep_windowed, pump_window_step};
use crate::app_state::{AppState, WindowPump};
use crate::content::Content;
use crate::session::Session;
use std::borrow::Borrow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::build::create_agent;
use super::errors::{retry_budget, retry_delay_ms};
use super::{estimate_context_tokens, maybe_strip_reasoning, CodeAgent};

/// Summary mode for `/tree compact`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactSummaryMode {
    /// User-editable summary: the dropped span is scanned for touched files
    /// (split into `Documents` = `*.md` and `Files` = everything else) and
    /// that list is opened in the user's `$EDITOR` for them to edit before
    /// being stored. The leaf-dance / pre-fill machinery that used to be
    /// called "truncate" (because it just wrote `summary = ""`) now produces
    /// a meaningful pre-filled summary. When the dropped span touches no
    /// files the template is empty and we fall back to `summary = ""` for
    /// back-compat with the no-op compaction.
    Manual,
    /// Deterministic text-only summary: user prompts + assistant text
    /// responses per turn, plus a touched-files list (read/write/edit
    /// paths, also split into Documents / Files). No reasoning, no tool
    /// calls, no tool output.
    TextOnly,
    /// LLM-generated summary: the dropped span is serialized into a prompt,
    /// sent through `agent.summarize()` (after temporarily moving the leaf
    /// to the entry *before* the compaction point and remembering the
    /// original leaf), and the response is written as `summary`.
    /// Accepts an optional trailing free-text payload (the user-editable
    /// instruction); when empty, `settings.llm_compact_prompt()` is used.
    Llm,
}

/// Build the LLM-bound conversation prefix for the session's current leaf.
///
/// Shared by the normal agent turn (`prompt`) and the session-context
/// summarizer (`summarize_with_session_context`) so the two requests build
/// their leading history identically — this is what preserves the provider
/// prefix-cache property: a summarization at a given position sends the same
/// bytes as a user prompt at that position, so the cached span (warmed by live
/// turns) hits. `Session::build_context()` applies the compaction transforms,
/// which re-activates a prior compaction active at the leaf as the leading
/// `<summary>` wrapper (see `doc/nested_compactions.md`).
///
/// Unlike `prompt` (which falls back to an empty vec on a stale index), the
/// summarizer treats an index failure as fatal — summarizing an empty prefix
/// would silently drop the whole conversation.
pub(crate) async fn session_context_messages(
    session: &Session,
    reasoning_in_context: crate::settings::ReasoningInContextMode,
    provider: &str,
    model: &str,
) -> crate::Result<Vec<rig_core::message::Message>> {
    let ctx = session.build_context().await?;
    // A resumed session's tail may end on a dangling call (a crash residue) or
    // carry orphan results — repair it in memory so the first request is valid
    // before anything is written. `Session::append_messages` repairs direction A
    // persistently on the next append; this is prior's own guard, independent of
    // rig's `pairing::repair` on load.
    let repaired = crate::agent::transcript::repair_pairing(ctx.messages);
    Ok(maybe_strip_reasoning(
        repaired,
        reasoning_in_context,
        provider,
        model,
    ))
}

/// Walk the active branch (root → leaf) and return the entry id that
/// immediately precedes `target_id`. Returns `None` if `target_id` is not
/// on the active branch or is the root. Used by the `compact llm` handler
/// to pick the position from which to run the LLM (so the LLM's prior
/// context equals the span the compaction entry is dropping).
/// Find the predecessor of `target_id` on an arbitrary branch (root→leaf order).
/// Returns the ID of the entry immediately before `target_id`, or `None` if
/// `target_id` is not on the branch or is the first entry.
fn find_predecessor<T: Borrow<crate::session::SessionEntry>>(
    branch: &[T],
    target_id: &str,
) -> Option<String> {
    let pos = branch.iter().position(|e| e.borrow().id == target_id)?;
    if pos == 0 {
        None
    } else {
        Some(branch[pos - 1].borrow().id.clone())
    }
}

/// The generated compaction summary plus the context snapshot a later
/// [`apply_compaction`] needs (doc/editor_cleanup.md §6).
///
/// Produced by [`generate_compaction`] with **no** session writes and no
/// rendering: the caller — a menu node's `prepare()` or the typed
/// `/tree … compact` entry — owns the review step (typically `$EDITOR`)
/// and hands the possibly-edited draft back to `apply_compaction`.
#[derive(Debug, Clone)]
pub(crate) struct CompactionDraft {
    /// Summary text as generated. Callers replace it with the reviewed
    /// (possibly edited) text before applying; empty is legal — manual
    /// mode on a span that touched no files.
    pub summary: String,
    /// Estimate of the context the compaction replaces, taken at the
    /// original leaf before the pivot — the same
    /// `estimate_context_tokens(build_context())` call the `/tree
    /// compact` sub-menu header shows, so header and stored value agree
    /// by construction.
    pub tokens_before: u64,
}

/// Generate a compaction summary draft for `mode` at `target_id` — the
/// ONE code path every compaction mode shares (doc/editor_cleanup.md §6):
///
/// 1. validate: active session, known target, non-empty span for `Llm`;
/// 2. snapshot `tokens_before` at the original leaf;
/// 3. **pivot**: move the leaf to the predecessor of the target so
///    `build_context()` scopes to `[root .. target)` — a previous
///    compaction at or before that point re-enters as its `<summary>`
///    wrapper, which is what makes iterative compaction lossless;
/// 4. build the mode's draft against that context (`Llm` summarizes
///    *while pivoted*, so retries re-send the identical scoped prefix);
/// 5. **restore** the original leaf on every return path.
///
/// No special cases: when the target sits at the current leaf (or the
/// branch root) the pivot is just a move. Pure data — no session writes,
/// no display; errors come back as strings and all UX (editor, confirm)
/// belongs to the caller.
pub(crate) async fn generate_compaction(
    app_state: &AppState,
    target_id: &str,
    mode: CompactSummaryMode,
    llm_payload: &str,
) -> std::result::Result<CompactionDraft, String> {
    // 1. Validation — one place; the typed arm and the menu nodes
    //    previously kept near-verbatim copies of this preamble.
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot compact: no active session".to_string())?;
    if session.get_entry(target_id).await.is_none() {
        return Err(format!(
            "Cannot compact: unknown tree entry {}",
            &target_id[..target_id.len().min(12)]
        ));
    }

    // 2. tokens_before at the original leaf (pre-pivot).
    let tokens_before = match session.build_context().await {
        Ok(ctx) => estimate_context_tokens(&ctx.messages),
        Err(_) => 0,
    };

    // Branch facts, pre-pivot: the predecessor scopes the pivot; the
    // dropped span (everything strictly before the target) feeds the
    // deterministic modes.
    let branch = session.get_branch().await;
    let predecessor_id = find_predecessor(&branch, target_id);
    let dropped: Vec<_> = branch
        .into_iter()
        .take_while(|e| e.id != target_id)
        .collect();

    // Llm has nothing to summarize without a predecessor (root-level
    // target). Manual/TextOnly tolerate it: the draft comes out empty
    // and the user can write it in the editor.
    if predecessor_id.is_none() && matches!(mode, CompactSummaryMode::Llm) {
        return Err("Cannot compact: nothing before this point".to_string());
    }

    // Llm needs a live agent, created lazily from the target config
    // (same target-stamp judgment as session creation). Manual/TextOnly
    // never touch the agent, so they stay network-free.
    let mut agent = app_state.agent();
    if matches!(mode, CompactSummaryMode::Llm) && agent.is_none() {
        let cfg = app_state.config_snapshot();
        match create_agent(cfg).await {
            Ok(a) => {
                let a: Arc<CodeAgent> = Arc::from(a);
                app_state.activate_agent(a.clone());
                agent = Some(a);
            }
            Err(e) => return Err(format!("Cannot start agent: {}", e)),
        }
    }

    // Session directory for relativizing file paths in the drafts.
    let session_dir = std::path::PathBuf::from(session.index().lock().await.header.cwd.clone());

    // 3./5. The pivot dance — one path for every mode: move → draft →
    // restore, with the restore running on every return path.
    let old_leaf = session.get_leaf_id().await;
    session
        .move_leaf(predecessor_id)
        .await
        .map_err(|e| e.to_string())?;
    let generated = draft_at_pivot(
        &session,
        &dropped,
        &session_dir,
        mode,
        llm_payload,
        &agent,
        app_state,
    )
    .await;
    // Restore unconditionally (the await below runs before anything is
    // returned); a restore failure is fatal even when the draft
    // succeeded — the leaf must never be left on the pivot.
    let restored = session
        .move_leaf(old_leaf)
        .await
        .map_err(|e| format!("compaction pivot restore failed: {}", e));
    match generated {
        Ok(summary) => restored.map(|_| CompactionDraft {
            summary,
            tokens_before,
        }),
        Err(draft_err) => Err(match restored {
            Err(restore_err) => format!("{} ({})", draft_err, restore_err),
            Ok(()) => draft_err,
        }),
    }
}

/// Build the mode's draft against the **pivoted** leaf (step 4 of
/// [`generate_compaction`]): the seed comes from the same scoped
/// `build_context()` the Llm summarization reads, so previous
/// compactions are accounted for exactly once, on one path.
async fn draft_at_pivot(
    session: &Session,
    dropped: &[Arc<crate::session::SessionEntry>],
    session_dir: &std::path::Path,
    mode: CompactSummaryMode,
    llm_payload: &str,
    agent: &Option<Arc<CodeAgent>>,
    app_state: &AppState,
) -> std::result::Result<String, String> {
    let seed_text = session
        .build_context()
        .await
        .ok()
        .as_ref()
        .and_then(crate::session::Session::extract_seed_from_context);
    // The deterministic modes only cover the span since the active compaction,
    // exactly like the pivoted runtime context. This keeps their drafts
    // incremental: the previous summary is the seed, and only messages/files
    // after its boundary are re-rendered.
    let since = crate::session::entries_since_last_compaction(dropped);
    match mode {
        CompactSummaryMode::Manual => Ok(crate::session::build_manual_template(
            &since,
            seed_text.as_deref(),
            session_dir,
        )),
        CompactSummaryMode::TextOnly => Ok(crate::session::build_text_only_summary(
            &since,
            seed_text.as_deref(),
            session_dir,
        )),
        CompactSummaryMode::Llm => {
            // Compose the user-tweakable instruction: trailing text on
            // the committed line wins; otherwise the setting (with
            // built-in fallback).
            let settings = app_state.settings();
            let mut instruction = if llm_payload.trim().is_empty() {
                settings.llm_compact_prompt()
            } else {
                llm_payload.to_string()
            };

            // Append cumulative file list (same as manual/text-only: previous
            // summary files first with custom labels preserved, then newly
            // touched files in last-access order). This gives the LLM a
            // structured reference and the user an editable list in the editor.
            let (files, _) = crate::session::merge_files_with_previous(
                seed_text.as_deref(),
                &since,
                session_dir,
            );
            if !files.is_empty() {
                let mut file_section = String::new();
                crate::session::render_files_section(&mut file_section, &files, "Documents");
                crate::session::render_files_section(&mut file_section, &files, "Files");
                if !file_section.is_empty() {
                    instruction.push_str("\n\n");
                    instruction.push_str(file_section.trim_start());
                }
            }

            // The leaf sits at the predecessor, so this reads the scoped
            // prefix — a previous compaction at or before the point
            // enters as its leading <summary> wrapper, and retries
            // re-send the identical prefix.
            let Some(a) = agent.as_ref() else {
                return Err("no agent".to_string());
            };
            let content = app_state.with_config(|c| c.content.clone());
            let (user_cancel, cancel_notify) =
                app_state.with_config(|c| (c.user_cancel.clone(), c.cancel_notify.clone()));
            let instruction = instruction.clone();
            let reply = retry_summarization(
                content.as_ref(),
                &user_cancel,
                &cancel_notify,
                Some(WindowPump::open(app_state)),
                &mut || a.summarize_with_session_context(session, &instruction),
            )
            .await
            .map_err(|e| format!("Summary failed: {}", e))?;
            let summary = reply.trim().to_string();
            if summary.is_empty() {
                return Err("Summary was empty; nothing written.".to_string());
            }
            Ok(summary)
        }
    }
}

/// Write the compaction entry for a reviewed [`CompactionDraft`] — the
/// apply half of doc/editor_cleanup.md §6. Data layer only: no display
/// (the user has just seen the summary; the committed input line
/// documents the write) and no confirm (the caller's UI owns that).
///
/// The one side effect beyond the session write is on the agent's usage
/// store: the compaction rewrote the history the last completion's input
/// count measured, so that report is invalidated. Nothing here computes
/// an estimate — the turn boundary's per-cycle write (`read_input`) and
/// the next turn's start (`prompt`) refresh it on demand, the same
/// on-demand path session resume uses.
pub(crate) async fn apply_compaction(
    app_state: &AppState,
    target_id: &str,
    draft: &CompactionDraft,
) -> std::result::Result<(), String> {
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot compact: no active session".to_string())?;
    session
        .append_compaction(&draft.summary, target_id, draft.tokens_before)
        .await
        .map_err(|e| e.to_string())?;
    // Flag only: the estimate the figure falls back to is refreshed on
    // demand at the next turn boundary, not here — no context build on
    // the compaction path.
    if let Some(usage) = app_state.content().usage_handle() {
        usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .invalidate_context_report();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Branch summarization — `/tree <id> switch summarize`
//
// pi parity, verified against ~/src/pi @ this change:
//   - `collectEntriesForBranchSummary` (core/compaction/branch-summarization.ts)
//     collects the spur; `navigateTree` (core/agent-session.ts) summarizes it
//     and calls `branchWithSummary(newLeafId, …)` (core/session-manager.ts),
//     which sets `leafId = target` FIRST and then appends the
//     `branch_summary` entry with `parentId = target`, `fromId = target`.
//     The entry is therefore *inline on the branch the conversation continues
//     on* — not parked on the abandoned spur — and `buildSessionContext`'s
//     per-entry mapping turns it into a user message (see
//     `session::branch_summary_message_text`). pi's context build walks only
//     the leaf path, so nothing has to go looking for summaries in side spurs.
// ---------------------------------------------------------------------------

/// Prepended to the **stored** summary text by pi's `generateBranchSummary`,
/// so the framing ("…before returning here") travels with the entry and is
/// visible in `/tree` too, not only inside the LLM wrapper.
const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

/// pi's `BRANCH_SUMMARY_PROMPT` (core/compaction/branch-summarization.ts),
/// byte-exact. The `## Goal / ## Constraints / ## Progress / ## Key Decisions
/// / ## Next Steps` shape is what makes the stored summary useful as *context*
/// later — a prose recap does not tell the next turn what is unfinished.
/// pi sends it with `maxTokens: 2048` alongside `SUMMARIZATION_SYSTEM_PROMPT`;
/// our summarizer (`CodeAgent::summarize`) is a single user-message request with
/// no system prompt, and the text above is self-contained, so it all lives in
/// the prompt instead.
pub(crate) const DEFAULT_BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// The entries a switch from `old_leaf` to `target` leaves behind: everything
/// after the deepest common ancestor on the old leaf's path, in chronological
/// order (pi `collectEntriesForBranchSummary`). Empty when the target is
/// already on the current path (i.e. navigating forward/down) — pi skips
/// summarization in that case and just branches.
///
/// Iterative (two `get_path_to_root` walks); no recursion, so a 1000-entry
/// linear session cannot overflow the stack. The caller holds the index lock.
pub(crate) fn abandoned_spur(
    index: &crate::session::SessionIndex,
    old_leaf: &str,
    target: &str,
) -> Vec<Arc<crate::session::SessionEntry>> {
    let old_path = index.get_path_to_root(old_leaf);
    let target_path = index.get_path_to_root(target);
    let target_ids: std::collections::HashSet<&str> =
        target_path.iter().map(|e| e.id.as_str()).collect();
    // The **deepest** shared entry is the branch point (pi walks
    // `oldLeafId → …` *until* it, so the ancestor itself is not in the span).
    // `rposition`, not `position`: when the target is a *descendant* of the
    // old leaf, the deepest shared entry IS the old leaf, which makes the spur
    // empty — forward navigation abandons nothing. And the ancestor is
    // excluded by `skip(i + 1)`: skipping `i` would fold the shared prefix into
    // the spur and have the summarizer re-summarize what the new branch keeps.
    match old_path
        .iter()
        .rposition(|e| target_ids.contains(e.id.as_str()))
    {
        Some(i) => old_path.into_iter().skip(i + 1).collect(),
        // No shared ancestor (an id that is not in this tree): the whole old
        // path is the spur. Callers validate the target first.
        None => old_path,
    }
}

/// Serialize spur entries to the text body of pi's `<conversation>` block
/// (`serializeConversation`, core/compaction/utils.ts): one line per message,
/// role-tagged, with tool calls rendered as `name(arg=value, …)`.
///
/// Deliberate divergences, both consistent with `build_text_only_summary`:
/// tool *results* are skipped (their content is already in the assistant's
/// tool call) and reasoning blocks are omitted (pi includes them; our
/// `reasoning_in_context` setting says reasoning does not belong in context).
/// `Compaction` / `BranchSummary` entries inside the spur are serialized as
/// the user message pi's conversion would produce, so a nested summary
/// survives into the outer summary.
fn serialize_spur<T: Borrow<crate::session::SessionEntry>>(entries: &[T]) -> String {
    use crate::session::SessionEntryContent as C;
    let mut parts: Vec<String> = Vec::new();
    for entry in entries {
        match &entry.borrow().content {
            C::Message { message } => {
                let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("");
                let content = message.get("content").and_then(|c| c.as_array());
                let Some(items) = content else { continue };
                match role {
                    "user" => {
                        let text = items
                            .iter()
                            .filter_map(|i| {
                                (i.get("type").and_then(|t| t.as_str()) == Some("text"))
                                    .then(|| i.get("text").and_then(|t| t.as_str()))
                                    .flatten()
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        if !text.is_empty() {
                            parts.push(format!("[User]: {}", text));
                        }
                    }
                    "assistant" => {
                        let text = items
                            .iter()
                            .filter_map(|i| {
                                (i.get("type").and_then(|t| t.as_str()) == Some("text"))
                                    .then(|| i.get("text").and_then(|t| t.as_str()))
                                    .flatten()
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            parts.push(format!("[Assistant]: {}", text));
                        }
                        let calls = items
                            .iter()
                            .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("toolCall"))
                            .map(|call| {
                                let name =
                                    call.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                                let args = match call.get("arguments").and_then(|a| a.as_object()) {
                                    Some(map) if !map.is_empty() => map
                                        .iter()
                                        .map(|(k, v)| {
                                            format!(
                                                "{}={}",
                                                k,
                                                serde_json::to_string(v).unwrap_or_default()
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    _ => String::new(),
                                };
                                format!("{}({})", name, args)
                            })
                            .collect::<Vec<_>>();
                        if !calls.is_empty() {
                            parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                        }
                    }
                    // toolResult: context is in the assistant's tool call.
                    _ => {}
                }
            }
            C::Compaction { summary, .. } if !summary.is_empty() => {
                parts.push(format!(
                    "[User]: {}{}{}",
                    crate::session::COMPACTION_SUMMARY_PREFIX,
                    summary,
                    crate::session::COMPACTION_SUMMARY_SUFFIX
                ));
            }
            C::BranchSummary { summary, .. } if !summary.is_empty() => {
                parts.push(format!(
                    "[User]: {}",
                    crate::session::branch_summary_message_text(summary)
                ));
            }
            // Meta entries (model/thinking/labels/custom): no conversation content.
            _ => {}
        }
    }
    parts.join("\n\n")
}

/// The `Documents` / `Files` section pi appends to every generated branch
/// summary (its `formatFileOperations`), built from the same collector the
/// text-only summary uses. `None` when the spur touched no files — nothing to
/// append.
fn spur_files_section<T: Borrow<crate::session::SessionEntry>>(
    entries: &[T],
    session_dir: &Path,
) -> Option<String> {
    let mut files: Vec<crate::session::TouchedFile> = Vec::new();
    for entry in entries {
        if let crate::session::SessionEntryContent::Message { message } = &entry.borrow().content {
            crate::session::collect_touched_files(message, &mut files, session_dir);
        }
    }
    if files.is_empty() {
        return None;
    }
    let mut out = String::new();
    crate::session::render_files_section(&mut out, &files, "Documents");
    crate::session::render_files_section(&mut out, &files, "Files");
    (!out.is_empty()).then_some(out)
}

/// Generate a branch summary draft for the spur left behind by switching from
/// the current leaf to `target_id` — the `/tree <id> switch summarize` twin of
/// [`generate_compaction`], with the same three modes and the same "no writes,
/// errors as strings" contract (doc/editor_cleanup.md §6).
///
/// 1. validate: active session, known target, target ≠ leaf, non-empty spur;
/// 2. build the mode's draft from the spur entries (never from the live
///    conversation prefix — a spur is off-path, so there is no prefix to
///    summarize; pi sends it as a standalone request too);
/// 3. `Llm` only: prepend pi's preamble and append the touched-files section,
///    so the stored summary carries its own framing.
///
/// `tokens_before` is left at 0: a `BranchSummary` entry has no such field
/// (pi's is `fromId` + `summary`), so nothing consumes it.
pub(crate) async fn generate_branch_summary(
    app_state: &AppState,
    target_id: &str,
    mode: CompactSummaryMode,
    llm_payload: &str,
) -> std::result::Result<CompactionDraft, String> {
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot summarize: no active session".to_string())?;
    if session.get_entry(target_id).await.is_none() {
        return Err(format!(
            "Cannot summarize: unknown tree entry {}",
            &target_id[..target_id.len().min(12)]
        ));
    }
    let old_leaf = session
        .get_leaf_id()
        .await
        .ok_or_else(|| "Cannot summarize: no current position".to_string())?;
    if old_leaf == target_id {
        return Err("Already at this entry — nothing to summarize.".to_string());
    }
    // One index lock, one collection — the spur is a snapshot; nothing below
    // mutates the index (the LLM request is the only await).
    let spur: Vec<Arc<crate::session::SessionEntry>> = {
        let index = session.index().lock().await;
        abandoned_spur(&index, &old_leaf, target_id)
    };
    if spur.is_empty() {
        return Err("Nothing to summarize: this entry is ahead on the current branch.".to_string());
    }
    let session_dir = std::path::PathBuf::from(session.index().lock().await.header.cwd.clone());

    let mut agent = app_state.agent();
    if matches!(mode, CompactSummaryMode::Llm) && agent.is_none() {
        let cfg = app_state.config_snapshot();
        match create_agent(cfg).await {
            Ok(a) => {
                let a: Arc<CodeAgent> = Arc::from(a);
                app_state.activate_agent(a.clone());
                agent = Some(a);
            }
            Err(e) => return Err(format!("Cannot start agent: {}", e)),
        }
    }

    let summary = match mode {
        // The deterministic modes are the compaction ones verbatim — they
        // take the dropped entries and touch no LLM. The manual template's
        // seed (a previous compaction summary) has no analogue here: a spur
        // carries no compaction boundary, so the file list stands alone.
        CompactSummaryMode::Manual => {
            crate::session::build_manual_template(&spur, None, &session_dir)
        }
        CompactSummaryMode::TextOnly => {
            crate::session::build_text_only_summary(&spur, None, &session_dir)
        }
        CompactSummaryMode::Llm => {
            let conversation = serialize_spur(&spur);
            // pi appends custom instructions rather than replacing the
            // structured-format prompt (`Additional focus:`), so a user
            // instruction can never break the required `##` sections.
            let instructions = if llm_payload.trim().is_empty() {
                DEFAULT_BRANCH_SUMMARY_PROMPT.to_string()
            } else {
                format!(
                    "{}\n\nAdditional focus: {}",
                    DEFAULT_BRANCH_SUMMARY_PROMPT,
                    llm_payload.trim()
                )
            };
            let prompt = format!(
                "<conversation>\n{}\n</conversation>\n\n{}",
                conversation, instructions
            );
            let Some(a) = agent.as_ref() else {
                return Err("no agent".to_string());
            };
            let content = app_state.with_config(|c| c.content.clone());
            let (user_cancel, cancel_notify) =
                app_state.with_config(|c| (c.user_cancel.clone(), c.cancel_notify.clone()));
            let reply = retry_summarization(
                content.as_ref(),
                &user_cancel,
                &cancel_notify,
                Some(WindowPump::open(app_state)),
                &mut || a.summarize(&prompt),
            )
            .await
            .map_err(|e| format!("Summary failed: {}", e))?;
            let reply = reply.trim();
            if reply.is_empty() {
                return Err("Summary was empty; nothing written.".to_string());
            }
            let mut summary = format!("{}{}", BRANCH_SUMMARY_PREAMBLE, reply);
            if let Some(files) = spur_files_section(&spur, &session_dir) {
                summary.push_str(&files);
            }
            summary
        }
    };

    Ok(CompactionDraft {
        summary,
        tokens_before: 0,
    })
}

/// Whether `target_id` names an entry `/tree <id> edit` can re-ask: a user
/// `Message` or a `CustomMessage` — pi's two cases
/// (`agent-session.ts:2950`/`:2954`). Read from the **session**, not the tree
/// listing, so the node's row cannot go stale against it.
pub(crate) async fn is_reaskable_entry(session: &Session, target_id: &str) -> bool {
    let Some(entry) = session.get_entry(target_id).await else {
        return false;
    };
    match &entry.content {
        crate::session::SessionEntryContent::Message { message } => {
            message.get("role").and_then(|r| r.as_str()) == Some("user")
        }
        crate::session::SessionEntryContent::CustomMessage { .. } => true,
        _ => false,
    }
}

/// `/tree <id> edit` — pi's user-message re-edit: move the leaf to the entry's
/// **parent** (so the prompt leaves the active branch) and put its text in the
/// input buffer for re-asking (`agent-session.ts:2950-2953`).
///
/// Distinct from `/tree <id> switch cursor`, which moves the leaf *to* the
/// entry — that keeps the prompt in context and continues after it. Both are
/// useful, so they are separate menu rows rather than one overloaded action.
///
/// The prefill goes through the one-shot register the loop pops before the next
/// `read_input`; it replaces the buffer outright (unlike pi, which only fills an
/// empty one) because that is the register's contract for handler writes.
pub(crate) async fn reask_from_entry(
    app_state: &AppState,
    target_id: &str,
) -> std::result::Result<(), String> {
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot edit: no active session".to_string())?;
    let entry = session.get_entry(target_id).await.ok_or_else(|| {
        format!(
            "Cannot edit: unknown tree entry {}",
            &target_id[..target_id.len().min(12)]
        )
    })?;
    let text = match &entry.content {
        crate::session::SessionEntryContent::Message { message } => {
            if message.get("role").and_then(|r| r.as_str()) != Some("user") {
                return Err("Only a user prompt can be edited — use `switch` to move here.".into());
            }
            crate::session::user_prompt_text(message)
        }
        crate::session::SessionEntryContent::CustomMessage { content, .. } => content
            .as_str()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        _ => return Err("Only a user prompt can be edited — use `switch` to move here.".into()),
    };
    let Some(text) = text else {
        return Err("That prompt has no text to edit.".into());
    };
    // Parent, not the entry: re-asking forks a new turn in place of the old
    // one, so the original prompt leaves the active branch (pi does the same,
    // `parentId: null` for a root prompt).
    session
        .move_leaf(entry.parent_id.clone())
        .await
        .map_err(|e| e.to_string())?;
    app_state.set_prefill(text);
    Ok(())
}

/// Write a reviewed branch summary and move the leaf — the submit half of
/// `/tree <id> switch summarize`, and the one place the two summary flows
/// differ in their effect.
///
/// Order matters and follows pi's `navigateTree`: **move first, then append**.
/// `append_branch_summary` parents to the current leaf and advances it, so
/// moving to `target_id` first yields pi's exact shape — a `branch_summary`
/// entry with `parentId = fromId = target_id`, sitting inline on the branch the
/// conversation continues on, where `build_session_context` projects it into
/// the LLM context.
///
/// The leaf-moved check is the guard against a draft prepared for a different
/// spur: the summary only describes what was behind the leaf at generation
/// time, so a leaf that moved since (the user switched elsewhere and came
/// back mid-review) makes the draft stale.
pub(crate) async fn apply_branch_summary(
    app_state: &AppState,
    target_id: &str,
    expected_old_leaf: &str,
    draft: &CompactionDraft,
) -> std::result::Result<(), String> {
    let session = app_state
        .session()
        .ok_or_else(|| "Cannot summarize: no active session".to_string())?;
    let current = session
        .get_leaf_id()
        .await
        .ok_or_else(|| "Cannot summarize: no current position".to_string())?;
    if current != expected_old_leaf {
        return Err(
            "The session moved since this summary was generated — press Enter to regenerate."
                .to_string(),
        );
    }
    if draft.summary.trim().is_empty() {
        return Err("Summary is empty; nothing written.".to_string());
    }
    // pi: leaf = branchFromId, then append parented to it.
    session
        .move_leaf(Some(target_id.to_string()))
        .await
        .map_err(|e| e.to_string())?;
    session
        .append_branch_summary(target_id, &draft.summary)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Run a *summarization* request with the same auto-retry / back-off / Esc-
/// cancel behaviour as a regular user prompt (`prompt()`'s retry loop in
/// `run_interactive`).
///
/// `window` is the window pump (see [`WindowPump`]): while the caller awaits
/// this future, the REPL loop is suspended inside `settle`, and the pump
/// consumes the persistent reader's *divert* — the reader keeps sole
/// ownership of stdin and forwards events to the window instead of the REPL
/// channel for the window's lifetime. The pump runs beside both phases: a
/// root `Esc` cancels the in-flight attempt (the select arm fires the
/// signal; the rig hook and this loop's `cancel_observed` both watch it)
/// *and* any back-off (`cancellable_sleep_windowed`); other keys are parked
/// for `read_input`'s next iteration. `None` (tests, piped/batch — no
/// reader to divert) leaves the arms pending — the signal-only behaviour,
/// stdin untouched.
///
/// `build` produces the summarization request and returns the model's reply
/// (or a classified `PriorError`). On a retryable failure it is re-invoked
/// (previous `build` again) after a `retry_delay_ms` back-off in
/// [`cancellable_sleep_windowed`]; non-retryable errors and cancellation
/// propagate out.
///
/// The leaf dance (if any) is the caller's responsibility: `build` reads the
/// session's *current* leaf to assemble context but must NOT move it, so the
/// caller can restore the original leaf on every return path. This keeps the
/// helper agnostic between the compaction path (leaf moved to the predecessor
/// for a conversation prefix) and the abandoned-branch path (no move).
pub(crate) async fn retry_summarization<F, Fut>(
    content: &Content,
    user_cancel: &Arc<AtomicBool>,
    cancel_notify: &Arc<tokio::sync::Notify>,
    mut window: Option<WindowPump<'_>>,
    build: &mut F,
) -> std::result::Result<String, crate::PriorError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<String, crate::PriorError>>,
{
    // Fresh operation: a stale cancel flag from a previous abort must not
    // cancel this one before the pump has read anything. `prompt()` applies
    // the same fresh-request rule; nothing else clears the flag on this
    // path (compaction runs between turns).
    user_cancel.store(false, Ordering::SeqCst);
    // No reader pause: the window does not read stdin — it consumes the
    // reader's *divert* ([`WindowPump`]), so the two can never race. `window:
    // None` (tests, piped/batch) is the plain signal-only wait.
    let mut consecutive_failures: u8 = 0;
    loop {
        // One attempt with the pump live beside it. The attempt future is
        // pinned: a key press polls *it* again rather than dropping the
        // in-flight request on the floor.
        let attempt = build();
        tokio::pin!(attempt);
        let outcome: Option<std::result::Result<String, crate::PriorError>> = loop {
            tokio::select! {
                r = &mut attempt => break Some(r),
                _ = cancel_observed(user_cancel, cancel_notify) => break None,
                fired = pump_window_step(&mut window, user_cancel, cancel_notify) => {
                    if fired {
                        break None;
                    }
                }
            }
        };
        match outcome {
            // Cancelled — a window `Esc` (signalled above) or the flag from
            // elsewhere. Nothing in flight to persist: a summarization has
            // no partial turn.
            None => return Err(crate::PriorError::Cancelled("Aborted.".into())),
            Some(Ok(text)) => return Ok(text),
            Some(Err(crate::PriorError::Cancelled(_))) => {
                return Err(crate::PriorError::Cancelled("Aborted.".into()));
            }
            Some(Err(e)) if e.is_auto_retryable() && consecutive_failures < retry_budget(&e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let ms = retry_delay_ms(&e, consecutive_failures);
                content.display_text(&format!("Error: {}. Retrying in {}s...", e, ms / 1000));
                let slept =
                    cancellable_sleep_windowed(user_cancel, cancel_notify, ms, &mut window).await;
                if !slept {
                    return Err(crate::PriorError::Cancelled("Aborted.".into()));
                }
                // `build` is re-invoked verbatim on the retry (re-sends the
                // same conversation prefix from the session leaf; cheap, and
                // identical to a normal prompt retry).
                if user_cancel.load(Ordering::SeqCst) {
                    return Err(crate::PriorError::Cancelled("Aborted.".into()));
                }
                continue;
            }
            Some(Err(e)) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::storage::SessionStorage;
    use crate::session::Session;
    use crate::settings::SettingsStore;
    use rig_core::message::{Message, UserContent};

    // ---- compaction: retry + generate/apply ----
    //
    // The compaction data path is `generate_compaction` (validate →
    // pivot to the predecessor → per-mode draft at the scoped context
    // → restore the leaf) and `apply_compaction` (append the reviewed
    // draft — data only, no display). `retry_summarization` is the
    // Llm arm's retrier (Esc-cancellable back-off). The tests below
    // verify the per-mode mechanics in isolation: Manual writes a
    // `Compaction` entry with an empty summary when the dropped span
    // touched no files; TextOnly writes a deterministic summary; Llm
    // fails cleanly without a live model. Mode plumbing itself is
    // covered by the node two-phase tests in `v25_tree.rs` — here we
    // focus on the side effects of the two halves, including that
    // generation restores the leaf it pivoted.

    #[tokio::test]
    async fn retry_summarization_succeeds_on_first_attempt() {
        let mut calls = 0;
        let content = null_content();
        let cancel = &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = &std::sync::Arc::new(tokio::sync::Notify::new());
        let r = retry_summarization(content.as_ref(), cancel, notify, None, &mut || {
            calls += 1;
            async { Ok::<_, crate::PriorError>("summary".to_string()) }
        })
        .await;
        assert_eq!(r.unwrap(), "summary");
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn retry_summarization_retries_on_rate_limit_then_succeeds() {
        // First call fails with a retryable 429; the back-off is cancellable
        // and the second attempt succeeds. Verify the retrier runs build
        // twice. (The sleep is 1s; acceptable for a focused test.)
        let mut calls = 0;
        let content = null_content();
        let cancel = &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = &std::sync::Arc::new(tokio::sync::Notify::new());
        let r = retry_summarization(content.as_ref(), cancel, notify, None, &mut || {
            calls += 1;
            async move {
                if calls == 1 {
                    Err(crate::PriorError::LlmRateLimited {
                        retry_after_secs: Some(1),
                    })
                } else {
                    Ok("second".to_string())
                }
            }
        })
        .await;
        assert_eq!(r.unwrap(), "second");
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn retry_summarization_propagates_non_retryable() {
        let mut calls = 0;
        let content = null_content();
        let cancel = &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = &std::sync::Arc::new(tokio::sync::Notify::new());
        let r = retry_summarization(content.as_ref(), cancel, notify, None, &mut || {
            calls += 1;
            async { Err(crate::PriorError::Llm("permanent".to_string())) }
        })
        .await;
        assert!(matches!(r, Err(crate::PriorError::Llm(_))));
        assert_eq!(calls, 1, "non-retryable must not retry");
    }

    #[tokio::test]
    async fn retry_summarization_cancels_after_esc_sleep() {
        // A provider hint of 5s: set the cancel flag so the windowed
        // sleep's signal arm ends the wait and the retrier surfaces
        // `Cancelled` without a second attempt.
        let mut calls = 0;
        let content = null_content();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let cancel_out = cancel.clone();
        // Cancel fires right after the first (failed) call, via a spawned
        // task that races the sleep.
        let cancel2 = cancel.clone();
        let notify2 = notify.clone();
        let started = std::time::Instant::now();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel2.store(true, std::sync::atomic::Ordering::SeqCst);
            notify2.notify_one();
        });
        let r = retry_summarization(content.as_ref(), &cancel_out, &notify, None, &mut || {
            calls += 1;
            async {
                Err(crate::PriorError::LlmRateLimited {
                    retry_after_secs: Some(5),
                })
            }
        })
        .await;
        let _ = handle.await;
        assert!(matches!(r, Err(crate::PriorError::Cancelled(_))));
        assert_eq!(calls, 1, "cancelled during back-off must not retry");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the 5s back-off ran its course instead of yielding to the signal"
        );
    }

    /// Build a writable `Session` from `entries` for the compaction
    /// generate/apply tests. The agent's `build_session_with_entries`
    /// opens the storage as read-only because it only tests reading
    /// (resume prefill); `apply_compaction` calls `append_compaction`,
    /// which needs a writable handle, so we use `open_or_create` here.
    async fn build_writable_session_with(
        dir: &Path,
        entries: Vec<crate::session::SessionEntry>,
    ) -> Session {
        use crate::session::storage::JsonlSessionStorage;
        use crate::session::types::session_filename;
        // The caller owns the directory (and keeps it alive for the whole test):
        // the returned `Session` re-opens by path, so the dir must outlive it.
        let filename = session_filename(&chrono::Utc::now());
        let path = dir.join(&filename);
        let storage = JsonlSessionStorage::open_or_create(path.clone())
            .await
            .expect("create storage");
        let header = crate::session::SessionHeader::new(Path::new("/test"));
        storage.write_header(&header).await.expect("write header");
        for entry in &entries {
            storage.append_entry(entry).await.expect("append");
        }
        // Set the leaf to the last entry so `get_branch()` returns the full path.
        if let Some(last) = entries.last() {
            let leaf = crate::session::SessionEntry::new_leaf(Some(last.id.clone()), None);
            storage.append_entry(&leaf).await.expect("append leaf");
        }
        let index = storage.build_index().await.expect("build index");
        Session::new_persisted(storage, index)
    }

    fn message_entry(
        id: &str,
        parent: Option<&str>,
        role: &str,
        text: &str,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: crate::session::SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": role,
                    "content": [{"type": "text", "text": text}]
                }),
            },
        }
    }

    /// Helper: a Compaction entry with the given summary / first-kept / tokens.
    fn compaction_entry(
        id: &str,
        parent: Option<&str>,
        summary: &str,
        first_kept: &str,
        tokens: u64,
    ) -> crate::session::SessionEntry {
        crate::session::SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: crate::session::SessionEntryContent::Compaction {
                summary: summary.to_string(),
                first_kept_entry_id: first_kept.to_string(),
                tokens_before: tokens,
            },
        }
    }

    fn test_settings() -> SettingsStore {
        SettingsStore {
            default_model: "p:m".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_reasoning_level: None,
            models: Default::default(),
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::from("default prompt"),
            path: None,
        }
    }

    fn null_content() -> std::sync::Arc<Content> {
        // `retry_summarization` surfaces retry notices through content; a
        // quiet `Content` discards them, which is what this suite wants — no
        // rendering, no terminal behind it.
        std::sync::Arc::new(Content::quiet())
    }

    /// Test app state for the compaction data path: writable session and
    /// provider `test` (so `create_agent` fails deterministically — an
    /// Llm generate without an injected agent errors offline).
    fn compaction_app_state(
        s: std::sync::Arc<Session>,
    ) -> std::sync::Arc<crate::app_state::AppState> {
        let app_state = crate::app_state::AppState::for_test(test_settings());
        app_state.set_session(Some(s));
        app_state
    }

    /// True when the session has any `BranchSummary` entry.
    async fn has_branch_summary_entry(s: &Session) -> bool {
        let index = s.index().lock().await;
        index.entries.values().any(|e| {
            matches!(
                e.content,
                crate::session::SessionEntryContent::BranchSummary { .. }
            )
        })
    }

    /// True when the session has any `Compaction` entry.
    async fn has_compaction_entry(s: &Session) -> bool {
        let index = s.index().lock().await;
        index.entries.values().any(|e| {
            matches!(
                e.content,
                crate::session::SessionEntryContent::Compaction { .. }
            )
        })
    }

    /// Pre-pivot estimate of the session's current context — the value
    /// `generate_compaction` snapshots into `tokens_before`.
    async fn expected_tokens(s: &Session) -> u64 {
        s.build_context()
            .await
            .map(|c| estimate_context_tokens(&c.messages))
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn generate_apply_manual_writes_empty_summary() {
        // Manual on a span that touched no files: generate → empty draft;
        // apply → Compaction entry with empty summary, firstKept=u1, and
        // the tokens_before snapshot generate took pre-pivot.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_compaction(&app_state, "u1", CompactSummaryMode::Manual, "")
            .await
            .expect("manual generate");
        assert_eq!(draft.summary, "", "no touched files → empty template");
        let tokens = expected_tokens(&s).await;
        assert_eq!(
            draft.tokens_before, tokens,
            "tokens_before is the pre-pivot estimate of the same context"
        );
        apply_compaction(&app_state, "u1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        let new_entry = s.get_entry(&new_leaf).await.expect("entry");
        match new_entry.content.clone() {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
            } => {
                assert_eq!(summary, "");
                assert_eq!(first_kept_entry_id, "u1");
                assert_eq!(tokens_before, draft.tokens_before);
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn apply_compaction_drops_the_context_report_and_computes_no_estimate() {
        // The compaction rewrote the history the last completion's input
        // count measured: apply drops that report from the shared usage
        // store, so `context_figure` (bar, /stat) falls back to the estimate
        // — refreshed on demand at the next turn boundary. Apply computes no
        // estimate of its own: the flag-only half of that contract.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "the question"),
                    message_entry("a1", Some("u1"), "assistant", "the answer"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        // A live store as the agent's wiring installs it, carrying a
        // standing provider report and a pre-compaction estimate.
        let usage = std::sync::Arc::new(std::sync::Mutex::new(crate::agent::SessionUsage {
            last_input_tokens: Some(100_000),
            estimated_input_tokens: 90_000,
            ..Default::default()
        }));
        app_state.content().set_usage(Some(usage.clone()));

        let draft = generate_compaction(&app_state, "a1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("generate");
        apply_compaction(&app_state, "a1", &draft)
            .await
            .expect("apply");

        let g = usage.lock().unwrap();
        assert!(
            g.last_input_tokens.is_none(),
            "the report the compaction invalidated must be dropped"
        );
        assert_eq!(
            g.estimated_input_tokens, 90_000,
            "apply computes no estimate — the turn boundary's write owns that"
        );
        assert_eq!(
            g.context_figure(),
            90_000,
            "the figure falls back to the estimate"
        );
    }

    #[tokio::test]
    async fn generate_apply_text_only_writes_deterministic_summary() {
        // TextOnly over an empty dropped span (target = first entry)
        // yields the empty deterministic summary; apply still writes a
        // legal entry.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_compaction(&app_state, "u1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("text-only generate");
        assert_eq!(
            draft.summary, "",
            "empty dropped span → empty deterministic summary"
        );
        apply_compaction(&app_state, "u1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        match s.get_entry(&new_leaf).await.expect("entry").content.clone() {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "u1");
                assert_eq!(summary, "");
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_compaction_llm_root_target_refuses() {
        // Llm at the branch root has nothing before the target: the one
        // "nothing before" guard refuses before any agent work.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let err = generate_compaction(&app_state, "u1", CompactSummaryMode::Llm, "payload")
            .await
            .expect_err("root-level Llm must refuse");
        assert!(err.contains("nothing before this point"), "err: {err}");
        assert_eq!(
            s.get_leaf_id().await.as_deref(),
            Some("a1"),
            "leaf untouched"
        );
        assert!(!has_compaction_entry(&s).await, "no write");
    }

    // ── Branch summarization (`/tree <id> switch summarize`) ─────────────────

    /// A spur session with the leaf at `a2`:
    ///
    /// ```text
    /// u1 ─ a1 ─ a2 ─ a3
    ///     └─ u2
    /// ```
    ///
    /// Switching to `u1` abandons `a1` + `a2` (a spur); `a3` is *ahead* of the
    /// leaf, so switching to it abandons nothing. The builder parks the leaf on
    /// the last entry, so it is moved to `a2` explicitly.
    async fn branch_summarize_session(dir: &std::path::Path) -> std::sync::Arc<Session> {
        let s = std::sync::Arc::new(
            build_writable_session_with(
                dir,
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                    message_entry("u2", Some("u1"), "user", "sibling path"),
                    message_entry("a2", Some("a1"), "assistant", "abandoned work"),
                    message_entry("a3", Some("a2"), "assistant", "the next turn"),
                ],
            )
            .await,
        );
        s.move_leaf(Some("a2".to_string())).await.expect("leaf");
        s
    }

    fn spur_ids<T: Borrow<crate::session::SessionEntry>>(entries: &[T]) -> Vec<String> {
        entries.iter().map(|e| e.borrow().id.clone()).collect()
    }

    #[tokio::test]
    async fn abandoned_spur_is_what_the_switch_leaves_behind() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let index = s.index().lock().await;
        // Switching to a sibling: everything after the common ancestor (u1).
        assert_eq!(
            spur_ids(&abandoned_spur(&index, "a2", "u2")),
            vec!["a1", "a2"],
            "chronological, common ancestor excluded"
        );
        // Switching to the branch point abandons the whole spur.
        assert_eq!(spur_ids(&abandoned_spur(&index, "a2", "u1")), ["a1", "a2"]);
        // Switching forward/down abandons nothing — pi skips the summary here.
        assert!(abandoned_spur(&index, "a2", "a3").is_empty());
        assert!(abandoned_spur(&index, "a2", "a2").is_empty(), "no move");
        // An id outside the tree has no shared ancestor: the whole old path is the
        // spur (no panic) — callers reject the unknown target first.
        assert_eq!(
            spur_ids(&abandoned_spur(&index, "a2", "nope")),
            ["u1", "a1", "a2"]
        );
    }

    #[tokio::test]
    async fn abandoned_spur_handles_a_long_linear_session_without_recursing() {
        // Large sessions are a linked list of depth N; the collection must be
        // iterative (explicit walks, no recursion) to survive the stack.
        let n = 20_000;
        let mut entries = Vec::with_capacity(n);
        for i in 0..n {
            let parent = if i == 0 {
                None
            } else {
                Some(format!("e{}", i - 1))
            };
            entries.push(message_entry(
                &format!("e{}", i),
                parent.as_deref(),
                "user",
                "x",
            ));
        }
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(build_writable_session_with(_g.path(), entries).await);
        let index = s.index().lock().await;
        let spur = abandoned_spur(&index, &format!("e{}", n - 1), "e0");
        assert_eq!(spur.len(), n - 1, "the whole tail past e0");
    }

    #[tokio::test]
    async fn generate_branch_summary_text_only_and_apply_moves_the_leaf() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        let draft = generate_branch_summary(&app_state, "u1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("generate");
        assert!(
            draft.summary.contains("abandoned work"),
            "the spur is what got summarized: {:?}",
            draft.summary
        );
        assert!(
            !draft.summary.contains("first prompt"),
            "the kept prefix must not be summarized: {:?}",
            draft.summary
        );
        assert_eq!(
            draft.tokens_before, 0,
            "BranchSummary entries carry no tokensBefore"
        );
        // Nothing written yet, and the leaf has not moved.
        assert!(!has_branch_summary_entry(&s).await);
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("a2"));

        apply_branch_summary(&app_state, "u1", "a2", &draft)
            .await
            .expect("apply");

        // pi's `branchWithSummary`: parentId = fromId = the switch target, and
        // the summary entry is the new leaf — inline on the branch the
        // conversation continues on.
        let leaf = s.get_leaf_id().await.expect("leaf");
        let entry = s.get_entry(&leaf).await.expect("entry");
        match entry.content.clone() {
            crate::session::SessionEntryContent::BranchSummary { from_id, summary } => {
                assert_eq!(from_id, "u1", "fromId is the branch-from position");
                assert_eq!(summary, draft.summary);
            }
            other => panic!("expected BranchSummary, got {other:?}"),
        }
        assert_eq!(entry.parent_id.as_deref(), Some("u1"));
    }

    #[tokio::test]
    async fn branch_summary_reaches_the_context_of_the_branch_it_was_written_on() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        let draft = generate_branch_summary(&app_state, "u1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("generate");
        apply_branch_summary(&app_state, "u1", "a2", &draft)
            .await
            .expect("apply");

        let ctx = s.build_context().await.expect("context");
        let joined: String = ctx
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User { content } => match content.first() {
                    Some(UserContent::Text(t)) => Some(t.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("Branch summary:"),
            "the wrapper must be the LLM-visible form: {joined}"
        );
        assert!(joined.contains("abandoned work"), "{joined}");
        // The abandoned spur itself is NOT in the new branch's context.
        assert!(
            !joined.contains("abandoned work\n"),
            "spur dropped: {joined}"
        );
    }

    #[tokio::test]
    async fn generate_branch_summary_refuses_without_a_spur_or_a_move() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        for (target, needle) in [
            ("a2", "Already at this entry"), // the leaf itself
            ("a3", "ahead"),                 // forward navigation: nothing abandoned
        ] {
            let err = generate_branch_summary(&app_state, target, CompactSummaryMode::TextOnly, "")
                .await
                .expect_err("nothing to summarize");
            assert!(err.contains(needle), "target {target}: {err}");
        }
        let err = generate_branch_summary(&app_state, "nope", CompactSummaryMode::TextOnly, "")
            .await
            .expect_err("unknown entry");
        assert!(err.contains("unknown tree entry"), "{err}");
        assert!(!has_branch_summary_entry(&s).await, "no write");
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("a2"), "leaf kept");
    }

    #[tokio::test]
    async fn apply_branch_summary_refuses_a_moved_leaf_and_an_empty_summary() {
        let _g = tempfile::tempdir().unwrap();
        let s = branch_summarize_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        let draft = CompactionDraft {
            summary: "digest".into(),
            tokens_before: 0,
        };
        let err = apply_branch_summary(&app_state, "u1", "a1", &draft)
            .await
            .expect_err("leaf moved since generation");
        assert!(err.contains("moved"), "{err}");
        assert!(!has_branch_summary_entry(&s).await, "no write");
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("a2"), "leaf kept");

        // An empty digest writes nothing either — the user discarded it.
        let empty = CompactionDraft {
            summary: "   ".into(),
            tokens_before: 0,
        };
        let err = apply_branch_summary(&app_state, "u1", "a2", &empty)
            .await
            .expect_err("empty summary");
        assert!(err.contains("empty"), "{err}");
        assert!(!has_branch_summary_entry(&s).await, "no write");
    }

    #[tokio::test]
    async fn generate_branch_summary_manual_mode_lists_the_spur_files() {
        // A `write` tool call in the spur is the file provenance the manual
        // template surfaces.
        let mut assistant = message_entry("a1", Some("u1"), "assistant", "editing");
        if let crate::session::SessionEntryContent::Message { message } = &mut assistant.content {
            message["content"] = serde_json::json!([
                {"type": "text", "text": "editing"},
                {"type": "toolCall", "name": "write",
                 "arguments": {"path": "notes.md", "content": "hi"}},
            ]);
        }
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    assistant,
                    message_entry("a2", Some("a1"), "assistant", "abandoned work"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_branch_summary(&app_state, "u1", CompactSummaryMode::Manual, "")
            .await
            .expect("generate");
        assert!(draft.summary.contains("notes.md"), "{:?}", draft.summary);
        assert!(
            draft.summary.contains("modified"),
            "a write is a modification: {:?}",
            draft.summary
        );
    }

    // ── `/tree <id> edit` (pi's user-message re-edit) ─────────────────────

    /// A session whose leaf is at `a1`: `u1` (user) ─ `a1` (assistant).
    async fn reask_session(dir: &std::path::Path) -> std::sync::Arc<Session> {
        let s = std::sync::Arc::new(
            build_writable_session_with(
                dir,
                vec![
                    message_entry("u1", None, "user", "the original prompt"),
                    message_entry("a1", Some("u1"), "assistant", "the reply"),
                ],
            )
            .await,
        );
        s.move_leaf(Some("a1".to_string())).await.expect("leaf");
        s
    }

    #[tokio::test]
    async fn reask_moves_to_the_parent_and_prefills_the_prompt() {
        let _g = tempfile::tempdir().unwrap();
        let s = reask_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        assert!(is_reaskable_entry(&s, "u1").await, "a user prompt");
        assert!(
            !is_reaskable_entry(&s, "a1").await,
            "not an assistant reply"
        );
        assert!(!is_reaskable_entry(&s, "nope").await, "unknown entry");

        reask_from_entry(&app_state, "u1").await.expect("re-ask");

        // The prompt leaves the active branch — re-asking replaces it.
        assert_eq!(
            s.get_leaf_id().await,
            None,
            "a root prompt's parent is the root, not the prompt"
        );
        assert_eq!(
            app_state.take_prefill().as_deref(),
            Some("the original prompt"),
            "the prompt rides the one-shot prefill register"
        );
    }

    #[tokio::test]
    async fn reask_refuses_a_non_prompt_entry_without_moving_the_leaf() {
        let _g = tempfile::tempdir().unwrap();
        let s = reask_session(_g.path()).await;
        let app_state = compaction_app_state(s.clone());
        let err = reask_from_entry(&app_state, "a1")
            .await
            .expect_err("assistant replies cannot be re-asked");
        assert!(err.contains("user prompt"), "{err}");
        assert_eq!(
            s.get_leaf_id().await.as_deref(),
            Some("a1"),
            "leaf untouched"
        );
        assert!(app_state.take_prefill().is_none(), "no prefill");
        let err = reask_from_entry(&app_state, "nope")
            .await
            .expect_err("unknown entry");
        assert!(err.contains("unknown tree entry"), "{err}");
    }

    #[tokio::test]
    async fn serialize_spur_uses_pi_role_tags_and_skips_tool_results() {
        let mut assistant = message_entry("a1", Some("u1"), "assistant", "reply text");
        if let crate::session::SessionEntryContent::Message { message } = &mut assistant.content {
            message["content"] = serde_json::json!([
                {"type": "text", "text": "reply text"},
                {"type": "toolCall", "name": "read", "arguments": {"path": "src/lib.rs"}},
            ]);
        }
        let mut tool_result = message_entry("t1", Some("a1"), "user", "");
        if let crate::session::SessionEntryContent::Message { message } = &mut tool_result.content {
            *message = serde_json::json!({
                "role": "toolResult",
                "toolCallId": "c1",
                "toolName": "read",
                "content": [{"type": "text", "text": "file contents"}],
            });
        }
        let spur = vec![
            message_entry("u1", None, "user", "do the thing"),
            assistant,
            tool_result,
            crate::session::SessionEntry::new_branch_summary(
                "x".into(),
                "earlier digest".into(),
                Some("a1".into()),
            ),
        ];
        let text = serialize_spur(&spur);
        assert!(text.contains("[User]: do the thing"), "{text}");
        assert!(text.contains("[Assistant]: reply text"), "{text}");
        assert!(
            text.contains("[Assistant tool calls]: read(path=\"src/lib.rs\")"),
            "{text}"
        );
        assert!(
            !text.contains("file contents"),
            "tool results are skipped (their content is in the call): {text}"
        );
        assert!(
            text.contains("[User]: Branch summary:"),
            "a nested digest survives as the wrapped user message: {text}"
        );
    }

    #[tokio::test]
    async fn generate_compaction_llm_without_agent_errors() {
        // A non-root target passes the guard; agent creation then fails
        // (provider `test` is unsupported) → structured error, no write.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "first prompt"),
                    message_entry("a1", Some("u1"), "assistant", "first reply"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let err = generate_compaction(&app_state, "a1", CompactSummaryMode::Llm, "user payload")
            .await
            .expect_err("no agent can be created in tests");
        assert!(err.contains("Cannot start agent"), "err: {err}");
        assert!(!has_compaction_entry(&s).await, "no write on failure");
    }

    #[tokio::test]
    async fn generate_apply_text_only_nonzero_dropped_span() {
        // target a1, dropped = [u1]: the summary carries the dropped
        // user prompt; apply stores it with firstKept=a1.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "the question"),
                    message_entry("a1", Some("u1"), "assistant", "the answer"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_compaction(&app_state, "a1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("text-only generate");
        assert!(
            draft.summary.contains("the question"),
            "draft should contain the dropped user prompt, got: {:?}",
            draft.summary
        );
        apply_compaction(&app_state, "a1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        match s.get_entry(&new_leaf).await.expect("entry").content.clone() {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "a1");
                assert!(
                    summary.contains("the question"),
                    "stored summary should contain the dropped user prompt, got: {:?}",
                    summary
                );
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_restores_leaf_manual_over_leaf_compaction() {
        // Branch [u0, a0, C1{firstKept:u0}] with leaf C1: manual generate
        // at u0 (no predecessor — root-level pivot) must restore the leaf
        // it pivoted before returning; apply then appends a fresh entry.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u0", None, "user", "prompt one"),
                    message_entry("a0", Some("u0"), "assistant", "reply one"),
                    compaction_entry("c1", Some("a0"), "earlier summary", "u0", 50),
                ],
            )
            .await,
        );
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("c1"));
        let tokens = expected_tokens(&s).await;
        let app_state = compaction_app_state(s.clone());

        let draft = generate_compaction(&app_state, "u0", CompactSummaryMode::Manual, "")
            .await
            .expect("manual re-compaction over a leaf-compaction");
        assert_eq!(
            s.get_leaf_id().await.as_deref(),
            Some("c1"),
            "generate must restore the leaf it pivoted"
        );
        assert_eq!(draft.tokens_before, tokens);

        apply_compaction(&app_state, "u0", &draft)
            .await
            .expect("apply");
        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        assert_ne!(new_leaf, "c1", "a new compaction entry must be appended");
        match s.get_entry(&new_leaf).await.expect("entry").content.clone() {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                tokens_before,
            } => {
                assert_eq!(summary, "");
                assert_eq!(first_kept_entry_id, "u0");
                assert_eq!(tokens_before, tokens);
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_apply_text_only_skips_nested_summaries() {
        // [u0, a0, u1, a1, C1{firstKept:u0, "EARLY SECRET SUMMARY"}],
        // leaf C1; target u1 → dropped [u0, a0]: the new summary includes
        // the raw dropped text but never the earlier compaction's text.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u0", None, "user", "first question"),
                    message_entry("a0", Some("u0"), "assistant", "first answer"),
                    message_entry("u1", Some("a0"), "user", "second question"),
                    message_entry("a1", Some("u1"), "assistant", "second answer"),
                    compaction_entry("c1", Some("a1"), "EARLY SECRET SUMMARY", "u0", 80),
                ],
            )
            .await,
        );
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("c1"));
        let app_state = compaction_app_state(s.clone());

        let draft = generate_compaction(&app_state, "u1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("text-only re-compaction");
        assert!(
            draft.summary.contains("first question"),
            "draft should include dropped user prompt, got: {:?}",
            draft.summary
        );
        assert!(
            !draft.summary.contains("EARLY SECRET SUMMARY"),
            "draft must NOT leak the nested compaction's summary, got: {:?}",
            draft.summary
        );
        apply_compaction(&app_state, "u1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        match s.get_entry(&new_leaf).await.expect("entry").content.clone() {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "u1");
                assert!(
                    summary.contains("first question"),
                    "stored summary should include dropped user prompt, got: {:?}",
                    summary
                );
                assert!(
                    !summary.contains("EARLY SECRET SUMMARY"),
                    "stored summary must NOT leak the nested compaction's text, got: {:?}",
                    summary
                );
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_text_only_is_incremental_since_last_compaction() {
        // Branch: [u0, a0(read thinned.rs), C1{firstKept:u2}, u2, a2(read
        // new.rs), u3]. Compacting at u3 must start from C1's summary and
        // only cover messages/files since C1's firstKept boundary; a file
        // the user thinned from C1's summary must not reappear from the
        // pre-compaction span.
        let _g = tempfile::tempdir().unwrap();
        let mut a0 = message_entry("a0", Some("u0"), "assistant", "old answer");
        if let crate::session::SessionEntryContent::Message { message } = &mut a0.content {
            message["content"] = serde_json::json!([
                {"type": "text", "text": "old answer"},
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "thinned.rs"}},
            ]);
        }
        let mut a2 = message_entry("a2", Some("u2"), "assistant", "new answer");
        if let crate::session::SessionEntryContent::Message { message } = &mut a2.content {
            message["content"] = serde_json::json!([
                {"type": "text", "text": "new answer"},
                {"type": "toolCall", "name": "read",
                 "arguments": {"path": "new.rs"}},
            ]);
        }
        let previous = "# Prior summary\n\nold notes\n\n---\n# [Files]\n- read: kept.rs\n";
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u0", None, "user", "old prompt"),
                    a0,
                    compaction_entry("c1", Some("a0"), previous, "u2", 20),
                    message_entry("u2", Some("c1"), "user", "new prompt"),
                    a2,
                    message_entry("u3", Some("a2"), "user", "next prompt"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_compaction(&app_state, "u3", CompactSummaryMode::TextOnly, "")
            .await
            .expect("text-only generate");

        assert!(
            draft.summary.starts_with("# Prior summary\n\nold notes"),
            "previous summary leads: {:?}",
            draft.summary
        );
        assert!(
            draft.summary.contains("# Messages since last compaction"),
            "{:?}",
            draft.summary
        );
        assert!(draft.summary.contains("new prompt"), "{:?}", draft.summary);
        assert!(draft.summary.contains("new answer"), "{:?}", draft.summary);
        assert!(
            !draft.summary.contains("old prompt") && !draft.summary.contains("old answer"),
            "pre-compaction messages must not be re-rendered: {:?}",
            draft.summary
        );
        assert!(
            draft.summary.contains("- read: kept.rs"),
            "previous files carried forward: {:?}",
            draft.summary
        );
        assert!(
            draft.summary.contains("- read: new.rs"),
            "new file appended: {:?}",
            draft.summary
        );
        assert!(
            !draft.summary.contains("thinned.rs"),
            "a thinned pre-compaction file must not reappear: {:?}",
            draft.summary
        );
    }

    #[tokio::test]
    async fn apply_stores_edited_draft() {
        // The UI review step hands an edited draft to apply: generate →
        // (the node's $EDITOR swaps the text) → apply stores the EDITED
        // summary, not the generated one.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "the question"),
                    message_entry("a1", Some("u1"), "assistant", "the answer"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let mut draft = generate_compaction(&app_state, "a1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("generate");
        assert!(
            draft.summary.contains("the question"),
            "generate must receive the dropped span, got: {:?}",
            draft.summary
        );
        draft.summary = format!("{}\n\nEDITED BY USER", draft.summary);
        apply_compaction(&app_state, "a1", &draft)
            .await
            .expect("apply");

        let new_leaf = s.get_leaf_id().await.expect("leaf must exist");
        match s.get_entry(&new_leaf).await.expect("entry").content.clone() {
            crate::session::SessionEntryContent::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id, "a1");
                assert!(
                    summary.contains("EDITED BY USER"),
                    "stored summary must be the edited text, got: {:?}",
                    summary
                );
            }
            other => panic!("expected Compaction entry, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn generate_without_apply_writes_nothing() {
        // Cancel (Esc at the engine's confirm, i.e. never reaching
        // submit) = simply not applying: generate leaves the session
        // byte-identical — leaf unchanged, no compaction entry, branch
        // length untouched.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u1", None, "user", "the question"),
                    message_entry("a1", Some("u1"), "assistant", "the answer"),
                ],
            )
            .await,
        );
        let app_state = compaction_app_state(s.clone());
        let draft = generate_compaction(&app_state, "a1", CompactSummaryMode::TextOnly, "")
            .await
            .expect("generate");
        assert!(!draft.summary.is_empty(), "draft was generated");

        assert_eq!(s.get_leaf_id().await.as_deref(), Some("a1"));
        assert!(!has_compaction_entry(&s).await, "no entry without apply");
        assert_eq!(s.get_branch().await.len(), 2, "branch untouched");
    }

    #[tokio::test]
    async fn generate_compaction_llm_leaf_compaction_errors_cleanly() {
        // [u0, a0, C1] with leaf C1, target a0 (predecessor u0 — passes
        // the guard): generation must return a structured error (no
        // agent can be built in tests) and never panic, even when the
        // leaf is already a compaction.
        let _g = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(
            build_writable_session_with(
                _g.path(),
                vec![
                    message_entry("u0", None, "user", "prompt one"),
                    message_entry("a0", Some("u0"), "assistant", "reply one"),
                    compaction_entry("c1", Some("a0"), "earlier summary", "u0", 50),
                ],
            )
            .await,
        );
        assert_eq!(s.get_leaf_id().await.as_deref(), Some("c1"));
        let app_state = compaction_app_state(s.clone());
        let err = generate_compaction(&app_state, "a0", CompactSummaryMode::Llm, "user payload")
            .await
            .expect_err("no agent can be created in tests");
        assert!(err.contains("Cannot start agent"), "err: {err}");
        assert_eq!(
            s.get_leaf_id().await.as_deref(),
            Some("c1"),
            "failed generation must leave the leaf where it was"
        );
        // The fixture already contains c1 — failure must not add another.
        let count = s
            .index()
            .lock()
            .await
            .entries
            .values()
            .filter(|e| {
                matches!(
                    e.content,
                    crate::session::SessionEntryContent::Compaction { .. }
                )
            })
            .count();
        assert_eq!(count, 1, "only the fixture's c1 — no new entry");
    }
}
