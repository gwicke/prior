//! Raw-mode REPL input layer (Stages 2–4).
//!
//! - [`TermGuard`] owns terminal raw-mode lifecycle + capability negotiation
//!   (plan §3.5.2) and is safe to call from `Drop`/panic hook/signal handler.
//! - [`Repl`] is the raw-mode driver. [`Repl::read_input`] is a multi-line
//!   line editor; [`Repl::select`] is a single-column list picker.
//!
//! The line editor keeps a single flat `Vec<char>` buffer (with `'\n'` as a
//! hard line break) plus a char-index cursor. Rendering wraps the buffer to
//! visual rows via [`layout`] (word-boundary wrap, CJK char-wrap) and positions
//! the cursor. All editor *logic* is pure and unit-tested; only the final
//! escape-sequence emission touches the terminal.

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crate::output::{term_height, term_width, wrap, Output};
use crate::model_catalog;
use crate::slash;
use crate::slash_menu_bridge;
use crate::slash_menu_bridge::MenuShared;
use crate::settings::{SettingsStore, SettingKey, SETTINGS};
use crate::table::{Align, Table};
use crate::session::{SessionTreeNode, SessionEntry, SessionEntryContent};
use crate::agent::SessionUsage;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Set by the `SIGCONT` handler on resume from `Ctrl-Z`. Read by the
/// in-flight reader thread, which re-enters raw mode once it sees the
/// flag cleared.
pub(crate) static RESUMED: AtomicBool = AtomicBool::new(false);

/// `SIGCONT` handler: we just record that the process resumed.
/// It must stay async-signal-safe (no I/O, no allocation), so the
/// actual terminal re-entry is performed by the reader thread (a normal
/// thread context) or by `read_input`'s loop, which poll this flag.
extern "C" fn on_sigcont(_sig: i32) {
    RESUMED.store(true, Ordering::SeqCst);
}

/// Register the `SIGCONT` handler exactly once. Safe to call multiple
/// times; `libc::signal` just replaces the handler. Required so that
/// `Ctrl-Z` suspend/resume restores raw mode.
pub(crate) fn register_sigcont() {
    unsafe {
        libc::signal(libc::SIGCONT, on_sigcont as *const () as usize);
    }
}

/// Spawn a background stdin reader that lives *only* while an LLM request
/// is in flight (`prompt().await` blocks the main task). In raw mode
/// `event::read` is blocking and the main task cannot read keys during
/// the await, so this thread catches `Esc`/`Ctrl-C`/`Ctrl-Z` that
/// arrive mid-request. Normal editing keys are read by `read_input`
/// (main task) and are simply dropped here.
///
/// - `Esc`        → `cancel.store(true)` (the rig `PromptHook` bridge
///                  aborts the in-flight request).
/// - `Ctrl-C`     → restore terminal, hard exit (process-wide).
/// - `Ctrl-Z`     → restore terminal, `killpg(getpgrp(), SIGTSTP)`
///                  (suspend the whole process group to the shell).
/// - other keys  → ignored (cannot be typed into the prompt mid-request).
///
/// `done` is set by the caller once the await returns, so the loop can
/// exit and be joined. `guard` and `cancel` are `Arc` and `stop()`/
/// `store()` are process-wide, so calling them from this thread is safe.
/// `paused` is set by a confirmation dialog on the main task while it owns
/// stdin; while it is `true` this reader must not read, or it would swallow
/// the dialog's keystrokes.
pub(crate) fn spawn_in_flight_reader(
    guard: Arc<TermGuard>,
    cancel: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    done: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    thread::spawn(move || loop {
        if done.load(Ordering::SeqCst) {
            break;
        }
        if RESUMED.swap(false, Ordering::SeqCst) {
            let _ = guard.start();
        }
        // A confirmation dialog on the main task owns stdin right now; back
        // off so we don't consume the keystrokes it is waiting for.
        if paused.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        match event::poll(Duration::from_millis(50)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => {
                    if let KeyCode::Char('c') = key.code {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            let _ = guard.stop();
                            process::exit(1);
                        }
                    } else if let KeyCode::Char('z') = key.code {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            let _ = guard.stop();
                            unsafe {
                                libc::killpg(libc::getpgrp(), libc::SIGTSTP);
                            }
                        }
                    } else if let KeyCode::Esc = key.code {
                        cancel.store(true, Ordering::SeqCst);
                        cancel_notify.notify_one();
                        // Wake any in-flight tool watchdog threads so the
                        // shell command is killed immediately (not after the
                        // timeout).  Sending on an unbounded mpsc channel is
                        // non-blocking, so this is safe from the reader thread.
                        for tx in crate::tools::TOOL_CANCEL_SENDERS.lock().unwrap().iter() {
                            let _ = tx.send(());
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    })
}

/// Owns terminal mode transitions. [`TermGuard::start`] enters raw mode and
/// negotiates capabilities; [`TermGuard::stop`] (idempotent) restores cooked
/// mode. Safe to call from `Drop`, a panic hook, or a signal handler.
pub struct TermGuard {
    active: AtomicBool,
    saved_paste: AtomicBool,
}

impl TermGuard {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            saved_paste: AtomicBool::new(false),
        }
    }

    /// Enter raw mode and negotiate capabilities. Idempotent.
    pub fn start(&self) -> io::Result<()> {
        if self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.saved_paste.store(false, Ordering::SeqCst);
        enable_raw_mode()?;
        // crossterm's `enable_raw_mode` calls `cfmakeraw`, which clears `OPOST`.
        // With `OPOST` off the terminal does NOT translate `\n` -> `\r\n`, so
        // every bare `\n` (help text, separators, agent responses) only moves
        // down a row and leaves the cursor at the same column — producing the
        // staggered "indentation" seen on a full screen. Re-enable output
        // post-processing (specifically `ONLCR`) so line breaks return to
        // column 0, while keeping raw *input* semantics (no canonical line
        // editing, no echo). `disable_raw_mode` restores the original mode.
        enable_output_processing();
        Self::csi("?2004h")?; // bracketed paste on
        // Kitty keyboard protocol: disambiguate keys + report all keys as CSI u,
        // so Shift+Enter arrives as Enter+SHIFT (newline) and Alt+Letter as a
        // distinct Alt-modified char. Honored by wezterm/alacritty/kitty.
        crossterm::execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            )
        )?;
        self.active.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Restore cooked mode. Idempotent; safe to call multiple times / from `Drop`.
    pub fn stop(&self) -> io::Result<()> {
        if !self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Pop one level of Kitty keyboard enhancement (matches the single push).
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        if self.saved_paste.load(Ordering::SeqCst) {
            Self::csi("?2004h")?;
        } else {
            Self::csi("?2004l")?;
        }
        disable_raw_mode()?;
        self.active.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn csi(seq: &str) -> io::Result<()> {
        let mut s = String::from("\x1b[");
        s.push_str(seq);
        io::stdout().write_all(s.as_bytes())?;
        io::stdout().flush()
    }
}

/// Re-enable terminal output post-processing (specifically `OPOST`/`ONLCR`)
/// on the stdout terminal. `crossterm::enable_raw_mode` runs `cfmakeraw`, which
/// disables `OPOST`; without it a bare `\n` does not return the cursor to
/// column 0, so agent responses and separators stagger. We operate on fd 1
/// (stdout), which is the pty slave when interactive and a pipe otherwise
/// (tcsetattr on a pipe harmlessly fails). Best-effort: errors are ignored.
fn enable_output_processing() {
    let fd = libc::STDOUT_FILENO;
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) != 0 {
            return;
        }
        termios.c_oflag |= libc::OPOST | libc::ONLCR;
        let _ = libc::tcsetattr(fd, libc::TCSANOW, &termios);
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// The interactive REPL driver.
pub struct Repl {
    guard: Arc<TermGuard>,
}

/// The currently-active model id, from the settings store (the authority).
/// `provider:model` — the same canonical form rows carry (`qualified()`),
/// so the model provider's `DataResult::active_row` matches exactly and
/// the engine marks the `*` row per frame.
pub fn active_model_id(settings: &crate::settings::SettingsStore) -> Option<String> {
    if settings.default_model.is_empty() {
        None
    } else {
        Some(settings.default_model.clone())
    }
}

/// Result of one `read_input` interaction.
pub enum InputAction {
    /// Submit a normal prompt (text may be empty when the line was cancelled).
    Submit(String),
    /// Switch model: (provider, model).
    SetModel(String, String),
    /// Set thinking level (string form: off | low | med | high).
    SetThinking(String),
    /// Set runtime persona (empty string => none).
    SetPersona(String),
    /// Quit the session.
    Quit,
    /// Sandbox operation: (subcommand, path, sandbox_path).
    Sandbox(String, String, String),
}

/// Context the REPL needs to render slash overlays and to re-emit recent
/// session output after an inline overlay is dismissed. Rebuilt each loop
/// iteration from the live `AgentConfig`.
pub struct ReplCtx {
    pub provider: String,
    pub model: String,
    pub recent: Vec<String>,
    /// Currently active runtime persona name (None => none), for marking the
    /// active entry in the `/persona` overlay.
    pub persona: Option<String>,
    /// Accumulated agent token usage (per-turn + session totals), snapshotted
    /// by `run_interactive` each loop iteration so the `/stat` overlay can
    /// render without direct access to the agent.
    pub session_usage: SessionUsage,
    /// Session repository (if available), for on-demand loading in `/session` overlay.
    pub repo: Option<crate::session::repo::JsonlSessionRepo>,
    /// The ID of the currently active session (empty if no session).
    pub current_session_id: String,
    /// Branch messages for the fork picker: (entry_id, preview_text) pairs
    /// for user messages in the current session's branch.
    pub branch_messages: Vec<(String, String)>,
    /// Pre-built session entry tree (empty if no session), used by `/tree`.
    pub tree: Vec<SessionTreeNode>,
    /// Current leaf entry ID (for highlighting in the tree).
    pub leaf_id: Option<String>,
    /// Estimated tokens of the current (compaction-scoped) context. Shown in
    /// the `/tree compact` sub-menu header; the compact handler writes the
    /// same estimate as `tokensBefore` (same `estimate_context_tokens` call).
    pub context_tokens: u64,
    /// Current model filter/sort state for the `/model` overlay.
    pub model_filter: ModelFilter,
    /// Currently-active thinking level (for the `*` marker in `/thinking`).
    pub thinking_level: Option<String>,
}

/// Model filter state for the `/model` overlay.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelFilter {
    /// Sort order (e.g., "most-popular", "created", "coding", "intelligence", "agentic", "price_input", "price_output", "price", "coding_price", "intelligence_price", "agentic_price")
    pub sort: Option<String>,
    /// Maximum prompt (input) price per million tokens
    pub price_input: Option<f64>,
    /// Maximum output (completion) price per million tokens
    pub price_output: Option<f64>,
    /// Maximum combined price (input + output) per million tokens
    pub price: Option<f64>,
    /// Minimum coding index (0-100)
    pub min_coding_index: Option<f32>,
    /// Minimum intelligence index (0-100)
    pub min_intelligence_index: Option<f32>,
    /// Minimum agentic index (0-100)
    pub min_agentic_index: Option<f32>,
    /// Maximum model age in days
    pub max_age_days: Option<u32>,
    /// Minimum context length
    pub min_context: Option<u32>,
    /// Search terms (after removing .sort: etc. modifiers)
    pub search_terms: String,
}

/// Model filter state for the `/model` overlay.

impl ModelFilter {
    /// Convert to ModelFilterParams for API calls.
    pub fn to_params(&self) -> model_catalog::ModelFilterParams {
        model_catalog::ModelFilterParams {
            sort: self.sort.clone(),
            price_input: self.price_input,
            price_output: self.price_output,
            price: self.price,
            min_coding_index: self.min_coding_index,
            min_intelligence_index: self.min_intelligence_index,
            min_agentic_index: self.min_agentic_index,
            max_age_days: self.max_age_days,
            min_context: self.min_context,
        }
    }

    /// Default filter: no category/sort restrictions (show all models)
    pub fn default_for_coding() -> Self {
        Self {
            ..Default::default()
        }
    }

    /// Generate a display string for the overlay title.
    pub fn title_suffix(&self) -> String {
        let mut parts = Vec::new();
        if let Some(s) = &self.sort {
            parts.push(format!("sort:{}", s));
        }
        if let Some(p) = self.price_input {
            if p == 0.0 {
                parts.push("free".to_string());
            } else {
                parts.push(format!("in:${:.2}/M", p));
            }
        }
        if let Some(p) = self.price_output {
            parts.push(format!("out:${:.2}/M", p));
        }
        if let Some(p) = self.price {
            if p == 0.0 {
                parts.push("free".to_string());
            } else {
                parts.push(format!("${:.2}/M", p));
            }
        }
        if let Some(v) = self.min_coding_index {
            parts.push(format!("coding>={}", v));
        }
        if let Some(v) = self.min_intelligence_index {
            parts.push(format!("intel>={}", v));
        }
        if let Some(v) = self.min_agentic_index {
            parts.push(format!("agentic>={}", v));
        }
        if let Some(v) = self.max_age_days {
            parts.push(format!("age<={}d", v));
        }
        if let Some(v) = self.min_context {
            parts.push(format!("ctx>={}", v));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", parts.join(" "))
        }
    }

}
/// Lightweight session metadata for the `/session` overlay.
#[derive(Clone)]
pub struct SessionMeta {
    pub id: String,
    pub name: Option<String>,
    pub timestamp: String,
    pub cwd: String,
    pub parent_session: Option<String>,
    pub message_count: usize,
    /// Number of user-role messages (the canonical "turn count"; excludes
    /// assistant and `toolResult` entries).
    pub user_message_count: usize,
    /// A short preview of the first user message in the session.
    pub first_prompt_preview: Option<String>,
    /// Full text of the last user message (for detail display).
    /// Only populated in the detail view via `read_metadata_detail`.
    pub last_prompt_text: Option<String>,
    /// Timestamp of the last user message.
    pub last_prompt_timestamp: Option<String>,
    /// Path to the `.jsonl` file on disk.
    pub path: Option<std::path::PathBuf>,
}

/// A candidate value shown in a setting's value-pick overlay.
#[derive(Debug, Clone)]
pub(crate) struct ValueCandidate {
    display: String,
    value: String,
    desc: String,
}

/// Map a repo `SessionMetadata` listing entry to the lightweight
/// [`SessionMeta`] used by the menu providers and the `/session` overlay.
pub fn session_meta_from(m: crate::session::types::SessionMetadata) -> SessionMeta {
    SessionMeta {
        id: m.id,
        name: m.name,
        timestamp: m.timestamp,
        cwd: m.cwd,
        parent_session: m.parent_session.map(|p| p.display().to_string()),
        message_count: m.message_count,
        user_message_count: m.user_message_count,
        first_prompt_preview: m.first_prompt_preview,
        last_prompt_text: m.last_prompt_text,
        last_prompt_timestamp: m.last_prompt_timestamp,
        path: Some(m.path),
    }
}

// ── Option A: ActiveOverlay enum ───────────────────────────────────────

/// What the caller should do after an overlay handles Enter.
enum OverlayAction {
    /// Overlay consumed the key; editor/state already updated, continue loop.
    Continue,
    /// Return this InputAction from `read_input`.
    Action(InputAction),
}

/// A single overlay "variant" that carries its own pre-computed data so
/// Tab/Enter/Esc handlers never need to re-query the buffer or re-filter.
///
/// Eliminates `kind: u8`, the three parallel match ladders, `help_view`,
/// `cur_word`/`cur_filter` bridge variables, and the `value_cands` hack.
pub(crate) enum ActiveOverlay {
    None,
    Help,

    CommandList {
        /// Legacy ranked list; kept until per-command handlers migrate.
        #[allow(dead_code)]
        cmds: Vec<crate::fuzzy::Ranked<'static, slash::SlashCommand>>,
        /// v2 engine rows (root descriptor, ranked). When non-empty these
        /// drive display/Tab/Enter; the legacy `cmds` list stays until the
        /// per-command handlers migrate (Phase 6 step 2).
        engine_rows: Vec<crate::slashdot_menu::fields::Row>,
        engine_active: usize,
    },

    Model {
        filter: ModelFilter,
        /// Index of the currently-active model (for the `*` marker) —
        /// the engine's `active_row_marker` (provider-declared).
        active: Option<usize>,
        /// Step 2: the engine's ranked/queried rows (provider order +
        /// generic query) — the single source of truth for display +
        /// Tab/Enter.
        engine_rows: Vec<crate::slashdot_menu::fields::Row>,
        /// Engine-owned view cursor (Phase 6.5): the row Tab/Enter act on.
        cursor: usize,
    },

    /// Dotted-modifier completion for a filter menu (`/model .sort:…`).
    /// Promoted from the list overlay when the trailing token starts with `.`;
    /// lists modifier names (and, once a `.name:` is typed, the enumerated
    /// `.name` values). Selecting Tab/Enter rewrites the dotted token into the
    /// buffer and the next `from_buffer` pass re-derives the state.
    Mod {
        rows: Vec<ValueCandidate>,
        /// `true` => user is selecting among a modifier's enumerated values
        /// (a `.name:<prefix>` is in the buffer); `false` => among modifier names.
        want_value: bool,
    },

    Thinking {
        /// Candidates after the engine's rank (fuzzy terms applied, active
        /// pinned). Empty ⇒ "(no matches)".
        rows: Vec<crate::slashdot_menu::fields::Row>,
        /// Index of the active (current) level, for the `*` marker.
        active: Option<usize>,
        /// Engine-owned view cursor (Phase 6.5): the row Tab/Enter act on.
        cursor: usize,
    },

    Settings {
        rows: Vec<(crate::settings::SettingMeta, String)>,
        _filter: String,
    },

    SettingsVal {
        key: crate::settings::SettingKey,
        cands: Vec<ValueCandidate>,
        _filter: String,
    },

    Persona {
        cands: Vec<ValueCandidate>,
        _filter: String,
        /// Index of the currently-active persona (for the `*` marker).
        active: Option<usize>,
        /// Engine-owned view cursor (Phase 6.5): the row Tab/Enter act on.
        cursor: usize,
    },

    Stat {
        lines: Vec<String>,
    },

    /// Help text for the `/replay` command syntax.
    ReplayHelp {
        lines: Vec<String>,
    },

    Sandbox {
        sub: Option<&'static str>,
        items: Vec<String>,
        _filter: String,
    },

    PathComplete {
        completions: Vec<String>,
        completion_pos: Option<(usize, usize)>,
    },

    /// `/session` overlay: list of sessions for the current CWD, filtered by
    /// `_filter`. `names` mirrors `opts` (1:1 before any row wrapping) so Tab/
    /// Enter can resolve the selected session without re-querying `ctx`.
    Session {
        opts: Vec<String>,
        names: Vec<String>,
        active: Option<usize>,
        _filter: String,
    },

    /// `/session <name> <action>` action menu for a single session.
    SessionAction {
        /// Display/completion name of the session (`name` or `id`).
        session_name: String,
        /// The session's current name (if any), for pre-filling rename.
        current_name: Option<String>,
        /// Whether this is the currently active session.
        is_current: bool,
        /// Pre-rendered info header rows (name, turns, age, parent, dir).
        header: Vec<String>,
        /// Action labels (resume/fork/rename/delete).
        opts: Vec<String>,
        active: Option<usize>,
        _filter: String,
    },

    /// Confirmation dialog (e.g. before deleting a session).
    Confirm {
        /// Pre-rendered header lines (the prompt, word-wrapped).
        header: Vec<String>,
        /// The buffer text to submit if confirmed.
        confirm_action: String,
        /// Previous `session_name` to go back to on cancel/esc.
        session_name: String,
        opts: Vec<String>,
        active: Option<usize>,
    },

    /// `/tree` overlay: visual session entry tree.
    Tree {
        rows: Vec<String>,
        /// Per-row background override. `Some(esc)` paints the row with
        /// that ANSI background (used by `/tree` to tint by indent
        /// depth); `None` falls back to the default panel BG. Length must
        /// be the same as `rows`.
        row_bgs: Vec<Option<&'static str>>,
        /// Entry IDs, 1:1 with `rows`, for Enter resolution.
        targets: Vec<String>,
        active: Option<usize>,
        _filter: String,
        /// v2 engine rows (Phase 6 step 6): flattened tree as provider
        /// rows; drives Tab/Enter cursor semantics when non-empty.
        engine_rows: Vec<crate::slashdot_menu::fields::Row>,
    },

    /// `/tree <id>` action menu (switch / compact). `target_id` is the
    /// resolved entry ID; `prev_id` / `next_id` are the adjacent IDs in
    /// tree order (used by PageUp/PageDown to navigate entries).
    TreeAction {
        target_id: String,
        prev_id: Option<String>,
        next_id: Option<String>,
        /// Pre-rendered info header rows (selected entry body — multi-line
        /// for messages, single-line otherwise; the entry ID is intentionally
        /// omitted since it is already visible in the prompt buffer).
        header: Vec<String>,
        /// Fuzzy-filtered action labels (switch / compact).
        opts: Vec<String>,
        active: Option<usize>,
        _filter: String,
    },

    /// `/tree <id> switch` sub-menu (just switch / summarize branch).
    SwitchAction {
        target_id: String,
        prev_id: Option<String>,
        next_id: Option<String>,
        /// Pre-rendered info header rows (selected entry body — multi-line
        /// for messages, single-line otherwise; the entry ID is intentionally
        /// omitted since it is already visible in the prompt buffer).
        header: Vec<String>,
        /// Fuzzy-filtered option labels (just switch / summarize).
        opts: Vec<String>,
        active: Option<usize>,
        _filter: String,
    },

    /// `/tree <id> compact` summary-mode sub-menu. The `header` carries the
    /// informational rows (selected entry, dropped/kept counts, token
    /// estimate) — the in-menu equivalent of `SessionAction`'s per-session
    /// info header. No such info is ever printed inline after the commit.
    CompactSummary {
        target_id: String,
        prev_id: Option<String>,
        next_id: Option<String>,
        header: Vec<String>,
        /// Fuzzy-filtered mode labels (truncate; more modes later).
        opts: Vec<String>,
        active: Option<usize>,
        _filter: String,
    },
}

impl ActiveOverlay {
    /// Construct the overlay from the current buffer and external state.
    /// (Legacy signature: test convenience; the live loop uses
    /// [`Self::from_buffer_cursors`].)
    #[cfg(test)]
    fn from_buffer(
        buf: &str,
        ctx: &ReplCtx,
        settings: &SettingsStore,
        help_view: bool,
        path_completions: &[String],
    ) -> Self {
        let shared = MenuShared::new(settings.clone(), ctx.repo.clone());
        *shared.current_session_id.borrow_mut() = ctx.current_session_id.clone();
        *shared.recent_models.borrow_mut() = ctx.recent.clone();
        *shared.tree.borrow_mut() = Repl::flatten_all_tree(ctx);
        *shared.leaf_id.borrow_mut() = ctx.leaf_id.clone();
        let mut engine = slash_menu_bridge::MenuEngine::new(&shared);
        Self::from_buffer_cursors(buf, ctx, settings, help_view, path_completions, &mut engine)
    }

    /// Like [`Self::from_buffer`], but seeds the engine-derived overlays
    /// with the sticky cursor map carried across frames (Phase 6.5): the
    /// derived `engine_active` then reflects arrow movement instead of
    /// always resetting to the pinned row.
    fn from_buffer_cursors(
        buf: &str,
        ctx: &ReplCtx,
        settings: &SettingsStore,
        help_view: bool,
        path_completions: &[String],
        engine: &mut slash_menu_bridge::MenuEngine,
    ) -> Self {
        if help_view {
            return ActiveOverlay::Help;
        }
        if buf.is_empty() || !buf.starts_with('/') {
            // Path-completions overlay when Tab was pressed on a bare prompt.
            if !path_completions.is_empty() {
                return ActiveOverlay::PathComplete {
                    completions: path_completions.to_vec(),
                    completion_pos: None,
                };
            }
            return ActiveOverlay::None;
        }
        let after = &buf[1..];
        let (word, rest) = if let Some((w, r)) = after.split_once(' ') {
            (w, r)
        } else if matches!(
            // Open without trailing space
            after,
            "" | "model" | "thinking" | "set"
                | "persona" | "stat" | "replay"
                | "sandbox" | "session" | "tree"
                | "compact"
        ) {
            (after, "")
        } else {
            ("", after)
        };
        match word {
            _ if Self::bare_listing_word(buf) => {
                // Phase 6.6 uniform listing rule (design §3): a bare
                // listing command stays in the root list (pinned).
                return Self::root_list_overlay(engine, buf, rest);
            }
            "model" | "models" | "m" => {
                let filter = rest.trim().to_string();
                // If the trailing token is a dotted-modifier prefix (starting
                // with `.`), show the modifier auto-complete overlay instead of
                // the model list. A bare `.` lists all modifiers.
                let last_tok = filter.split_whitespace().last().unwrap_or("");
                if last_tok.starts_with('.') && !last_tok.starts_with("..") {
                    let (mod_rows, want_value) = Repl::modifier_overlay_entries(&filter);
                    if !mod_rows.is_empty() {
                        return ActiveOverlay::Mod { rows: mod_rows, want_value };
                    }
                    // No completions → the dotted token is complete (a full
                    // `.name:value` or a free-form `.name:` waiting for the
                    // number): fall through to the model list.
                }
                // Phase 6.7 step 3: derive from the persistent engine. The
                // ModelProvider serves the catalog in recency×popularity
                // order (active model first); the engine's generic query
                // applies fuzzy terms + any `.sort:`/field modifiers.
                let line = buf.trim_start_matches('/').to_string();
                let st = engine.interpreter.state(&line);
                ActiveOverlay::Model {
                    engine_rows: st.candidates.clone(),
                    active: st.active_row_marker,
                    cursor: st.active,
                    filter: ModelFilter::default_for_coding(),
                }
            }
            "thinking" | "think" | "t" => {
                // v2 engine: descriptor tree + rank_and_pin (Phase 6 step 1).
                // The buffer is "/thinking <terms...>"; the engine's
                // canonical line strips the leading slash.
                let line = buf.trim_start_matches('/').to_string();
                let st = engine.interpreter.state(&line);
                // `*` = the CURRENT level (pre-change state), from ctx —
                // never from the typed/pinned selection.
                let active = ctx
                    .thinking_level
                    .as_deref()
                    .and_then(|lvl| st.candidates.iter().position(|r| r.id == lvl));
                ActiveOverlay::Thinking { rows: st.candidates.clone(), active, cursor: st.active }
            }
            "persona" | "personas" | "p" => {
                // v2 engine (Phase 6 step 3): persona is a value child of
                // the persona node; the engine ranks and pins. `*` marker
                // from ctx (current persona).
                let line = buf.trim_start_matches('/').to_string();
                let st = engine.interpreter.state(&line);
                let active = ctx.persona.as_deref().and_then(|p| {
                    // "none" row carries the empty value.
                    let name = if p.is_empty() { "none" } else { p };
                    st.candidates.iter().position(|r| r.id == name)
                });
                ActiveOverlay::Persona { cands: st.candidates.iter().map(|r| ValueCandidate {
                    display: r.id.clone(),
                    value: if r.id == "none" { String::new() } else { r.id.clone() },
                    desc: r.description.clone().unwrap_or_default(),
                }).collect(), _filter: rest.trim().to_string(), active, cursor: st.active }
            }
            "set" | "s" => {
                let filter = rest.to_string();
                let (skey, vq, is_value) = Repl::parse_settings(rest);
                if is_value {
                    if let Some(k) = skey {
                        let cands = Repl::filtered_value_candidates(k, ctx, &vq);
                        ActiveOverlay::SettingsVal { key: k, cands, _filter: filter }
                    } else {
                        ActiveOverlay::Settings { rows: vec![], _filter: filter }
                    }
                } else {
                    let rows = settings.overlay_rows(&filter);
                    ActiveOverlay::Settings { rows, _filter: filter }
                }
            }
            "stat" | "stats" => {
                // v2 engine (Phase 6 step 8): the interstitial phase lives
                // in AcceptController (D4); the panel lines still render
                // from the v1 stats path.
                let session_info = Repl::current_session_stats_info(ctx);
                let lines = crate::agent::session_stats_lines(
                    &ctx.provider, &ctx.model, ctx.session_usage, session_info,
                );
                // D4 phase: first Enter shows (this arm); the phase is
                // tracked in `stat_phase` and flipped by the Enter handler.
                ActiveOverlay::Stat { lines }
            }
            "replay" => {
                if rest.trim().is_empty() {
                    let lines = vec![
                        "Replay the full history of the current session.".into(),
                        "".into(),
                        "Optional length filters (appended after /replay):".into(),
                        "  10      last 10 agent turns (same as 10t)".into(),
                        "  10t     last 10 agent turns".into(),
                        "  10l     last 10 output lines".into(),
                        "".into(),
                        "Examples:  /replay    /replay 5    /replay 50l".into(),
                    ];
                    ActiveOverlay::ReplayHelp { lines }
                } else {
                    // Non-empty arg → no overlay, submit directly.
                    ActiveOverlay::None
                }
            }
            "compact" => {
                // v2 (R8-8): /compact is a leaf whose accept rewrites the
                // path to /tree <last-user-id> compact . The resolved
                // CompactSummary view below is the *rewritten* menu state.
                // `/compact` auto-resolves `target_id` to the last
                // user message on the active branch, so the committed
                // line is `/tree <last_user_id> compact <mode>` and
                // the dropped span = everything before the user's last
                // prompt. The kept range starts with the user's prompt
                // and continues to the current leaf, so the next LLM
                // turn sees [summary] + [last user prompt] + [last
                // assistant response (if any)] + [new user prompt].
                let filter = rest.trim().to_string();
                let Some(leaf_id) = ctx.leaf_id.as_deref() else {
                    return ActiveOverlay::None;
                };
                let Some(target_id) =
                    Repl::last_user_message_id_on_active_branch(&ctx.tree, leaf_id)
                else {
                    // No user message on the branch (empty session or
                    // non-message-only branch). Match the no-leaf case:
                    // silent `None` so the user just sees no overlay.
                    return ActiveOverlay::None;
                };
                // Resolve the target's display label from the tree so
                // the header shows what is being compacted at.
                let label = {
                    let mut flat = Vec::new();
                    let active_path: std::collections::HashSet<String> =
                        std::collections::HashSet::new();
                    Repl::flatten_tree_nodes(
                        &ctx.tree,
                        0,
                        Some(leaf_id),
                        &active_path,
                        &mut flat,
                    );
                    flat.iter()
                        .find(|(_, _, id, _, _, _)| id == &target_id)
                        .map(|(_, l, _, _, _, _)| l.clone())
                        .unwrap_or_else(|| target_id.clone())
                };
                let mut header = Vec::new();
                Repl::compact_summary_header(&target_id, &label, ctx, &mut header);
                let (opts, _fuzzy_active) =
                    Repl::filter_tree_options(&["manual", "text only", "llm"], &filter);
                let first = filter.split_whitespace().next().unwrap_or("");
                let first_normalized = first.replace(' ', "-");
                let active = opts
                    .iter()
                    .position(|l| {
                        l == first || l.replace(' ', "-") == first_normalized
                    })
                    .or(_fuzzy_active);
                let (prev_id, next_id) =
                    Repl::prev_next_entry_id(&ctx.tree, Some(leaf_id), &target_id);
                ActiveOverlay::CompactSummary {
                    target_id,
                    prev_id,
                    next_id,
                    header,
                    opts,
                    active,
                    _filter: filter,
                }
            }
            "sandbox" | "sb" => {
                let filter = rest.trim().to_string();
                // v2 engine subcommands (Phase 6 step 7): the sandbox node's
                // static children rank/filter via the engine; path-value
                // completion (add_ro/add_rw) and remove stay on the v1
                // overlay for now (fs live-filter lands with the modifier
                // value-completion migration).
                let line = buf.trim_start_matches('/').to_string();
                let _last_tok = filter.split_whitespace().next().unwrap_or("");
                let is_value_phase = {
                    let words: Vec<&str> = filter.split_whitespace().collect();
                    words.len() >= 2
                        || words.first().map(|w| {
                            matches!(*w, "add_ro" | "add_rw" | "remove")
                        }).unwrap_or(false) && !filter.ends_with(' ') && words.len() >= 1
                };
                if !is_value_phase {
                    let st = engine.interpreter.state(&line);
                    if st.candidates.iter().any(|r| {
                        matches!(r.id.as_str(), "add_ro" | "add_rw" | "remove")
                    }) {
                        let items: Vec<String> = st
                            .candidates
                            .iter()
                            .map(|r| {
                                let name = format!("{:<10}", r.id);
                                match &r.description {
                                    Some(d) => format!("{} {}", name, d),
                                    None => name,
                                }
                            })
                            .collect();
                        return ActiveOverlay::Sandbox { sub: None, items, _filter: filter };
                    }
                }
                let mut items = Vec::new();
                let mut active = None;
                let sub = Repl::sandbox_overlay_entries(&filter, &mut items, &mut active);
                ActiveOverlay::Sandbox { sub, items, _filter: filter }
            }
            "session" | "sessions" => {
                let filter = rest.trim().to_string();

                // Delete confirmation (unchanged v1 path): the Confirm
                // overlay handles the two-step; the v2 confirm flow for
                // this command lands with the accept-flow migration.
                if !filter.is_empty() {
                    let (name_part, after_name) = crate::agent::parse_session_name_after(&filter);
                    if !name_part.is_empty() {
                        let action = after_name.split_once(char::is_whitespace)
                            .map(|(a, _)| a)
                            .unwrap_or(&after_name);
                        if action == "delete" || action == "delete_current" {
                            let single = Repl::filtered_sessions(ctx, &name_part);
                            if single.len() == 1 {
                                let meta = &single[0];
                                let display_name = meta.name.as_deref().unwrap_or(&meta.id);
                                let is_current = meta.id == ctx.current_session_id;
                                let (prompt, confirm_action) = if action == "delete_current" || is_current {
                                    (format!("Delete the current session and start a new one? '{}' will be permanently deleted from disk.", display_name),
                                     "/clear".to_string())
                                } else {
                                    (format!("Delete session '{}'? This will permanently delete it from disk.", display_name),
                                     format!("/session {} delete", meta.id))
                                };
                                return ActiveOverlay::Confirm {
                                    header: crate::output::wrap(&prompt, crate::output::term_width().saturating_sub(2)),
                                    confirm_action,
                                    session_name: meta.id.clone(),
                                    opts: vec!["Cancel".into(), "Confirm".into()],
                                    active: Some(0),
                                };
                            }
                        }
                    }
                }

                // When the filter is `<name> <action>` with a known action, strip
                // the action and show the action menu for that session.
                let lookup_filter = if !filter.is_empty() {
                    let (name_part, after) = crate::agent::parse_session_name_after(&filter);
                    let known_action = after.split_once(char::is_whitespace)
                        .map(|(a, _)| a)
                        .unwrap_or(&after);
                    if !name_part.is_empty() && matches!(known_action, "resume" | "fork" | "rename" | "delete") {
                        name_part
                    } else {
                        filter.clone()
                    }
                } else {
                    filter.clone()
                };

                let filtered = Repl::filtered_sessions(ctx, &lookup_filter);
                // When a filter is typed and resolves to exactly one session,
                // jump straight to that session's action menu.
                if !filter.is_empty() && filtered.len() == 1 {
                    let meta = &filtered[0];
                    let session_name = meta.id.clone();
                    let is_current = meta.id == ctx.current_session_id;
                    let mut header = Vec::new();
                    let mut opts = Vec::new();
                    let mut active = None;
                    Repl::session_action_overlay_entries(meta, ctx, &filter, &mut header, &mut opts, &mut active);
                    let current_name = meta.name.clone();
                    ActiveOverlay::SessionAction { session_name, current_name, is_current, header, opts, active, _filter: filter }
                } else {
                    // v2 engine list (Phase 6 step 5): SessionProvider rows
                    // from shared state; `*` on the current session. Step 2:
                    // providers attach in `MenuEngine::new(&shared)` — no
                    // per-arm wiring here.
                    let (_dir_f, _fuzzy_f) = Repl::parse_session_path_filter(&filter);
                    let line = buf.trim_start_matches('/').to_string();
                    let st = engine.interpreter.state(&line);
                    let opts: Vec<String> = st.candidates.iter().map(|r| {
                        let get_str = |k: &str| r.field_value(k).and_then(|v| {
                            if let crate::slashdot_menu::value::Value::Str(s) = v { Some(s.clone()) } else { None }
                        });
                        let get_num = |k: &str| r.field_value(k).and_then(|v| {
                            if let crate::slashdot_menu::value::Value::Num(n) = v { Some(*n as usize) } else { None }
                        });
                        let name = get_str("name").unwrap_or_else(|| r.id.clone());
                        let msgs = get_num("msgs").unwrap_or(0);
                        let cwd = get_str("cwd").unwrap_or_default();
                        let dir = cwd.rsplit('/').next().unwrap_or(&cwd).to_string();
                        let ts = get_str("timestamp")
                            .map(|t| Repl::format_session_timestamp(&t))
                            .unwrap_or_default();
                        format!("{} · {} {} · {}", name, msgs, dir, ts)
                    }).collect();
                    let names: Vec<String> = st.candidates.iter().map(|r| r.id.clone()).collect();
                    let active = st.active_row_marker;
                    ActiveOverlay::Session { opts, names, active, _filter: filter }
                }
            }
            "tree" => {
                let filter = rest.trim().to_string();
                // Only an id enclosed in angle brackets selects a tree point
                // (`/tree <deadbeef>`); anything else is a fuzzy search term
                // for the entry list (`/tree refactor`).
                if let Some(target_id) = Repl::parse_tree_id(&filter) {
                    let mut flat = Vec::new();
                    Repl::flatten_tree_nodes(
                        &ctx.tree,
                        0,
                        ctx.leaf_id.as_deref(),
                        &std::collections::HashSet::new(),
                        &mut flat,
                    );
                    if let Some((_, label, _, _, _, _)) =
                        flat.iter().find(|(_, _, id, _, _, _)| id == &target_id)
                    {
                        let label = label.clone();
                        // Tokens after the id: `<id>`, `<id> switch`,
                        // `<id> switch <filter>`, `<id> compact [filter]`, or
                        // `<id> <partial-action>` (fuzzy filter on the action
                        // menu, e.g. `<id> sw` → `switch`).
                        let after = filter
                            .strip_prefix(&format!("<{}>", target_id))
                            .unwrap_or("")
                            .trim();
                        let (action_tok, sub_filter) =
                            match after.split_once(' ') {
                                Some((tok, rest)) => (tok, rest.trim()),
                                None => (after, ""),
                            };
                        // An unknown single token is a fuzzy filter for the
                        // action menu, not an action.
                        let (action_tok, sub_filter) = if action_tok.is_empty()
                            || matches!(action_tok, "switch" | "compact")
                        {
                            (action_tok, sub_filter)
                        } else {
                            ("", after)
                        };
                        let sub_filter = sub_filter.to_string();
                        match action_tok {
                            "switch" => {
                                // Compute opts first so the message-preview
                                // cap can reserve room for the menu rows
                                // (see `tree_entry_header`).
                                let (opts, active) =
                                    Repl::filter_tree_options(&["just switch", "summarize"], &sub_filter);
                                let max_body_lines =
                                    Repl::tree_preview_line_budget(opts.len());
                                let mut header = Vec::new();
                                let entry = Repl::find_entry_by_id(&ctx.tree, &target_id);
                                Repl::tree_entry_header(
                                    entry,
                                    &label,
                                    max_body_lines,
                                    &mut header,
                                );
                                let (prev_id, next_id) =
                                    Repl::prev_next_entry_id(&ctx.tree, ctx.leaf_id.as_deref(), &target_id);
                                return ActiveOverlay::SwitchAction {
                                    target_id,
                                    prev_id,
                                    next_id,
                                    header,
                                    opts,
                                    active,
                                    _filter: sub_filter,
                                };
                            }
                            "compact" => {
                                let mut header = Vec::new();
                                Repl::compact_summary_header(
                                    &target_id,
                                    &label,
                                    ctx,
                                    &mut header,
                                );
                                let (opts, _fuzzy_active) =
                                    Repl::filter_tree_options(&["manual", "text only", "llm"], &sub_filter);
                                // If the filter begins with a known mode
                                // label followed by whitespace (e.g. `llm
                                // <payload>`), prefer that mode even when
                                // the trailing payload makes SkimMatcher's
                                // subsequence filter return no scores or
                                // pre-select a different mode. The
                                // `filter_tree_options` exact-token check
                                // is full-string only, so we promote here
                                // for first-word matches.
                                let first = sub_filter
                                    .split_whitespace()
                                    .next()
                                    .unwrap_or("");
                                let first_normalized = first.replace(' ', "-");
                                let active = opts
                                    .iter()
                                    .position(|l| {
                                        l == first
                                            || l.replace(' ', "-") == first_normalized
                                    })
                                    .or(_fuzzy_active);
                                let (prev_id, next_id) =
                                    Repl::prev_next_entry_id(&ctx.tree, ctx.leaf_id.as_deref(), &target_id);
                                return ActiveOverlay::CompactSummary {
                                    target_id,
                                    prev_id,
                                    next_id,
                                    header,
                                    opts,
                                    active,
                                    _filter: sub_filter,
                                };
                            }
                            _ => {}
                        }
                        // No (or unknown) action token → action menu.
                        let (opts, active) =
                            Repl::filter_tree_options(&["switch", "compact"], &sub_filter);
                        let max_body_lines = Repl::tree_preview_line_budget(opts.len());
                        let mut header = Vec::new();
                        let entry = Repl::find_entry_by_id(&ctx.tree, &target_id);
                        Repl::tree_entry_header(
                            entry,
                            &label,
                            max_body_lines,
                            &mut header,
                        );
                        let (prev_id, next_id) =
                            Repl::prev_next_entry_id(&ctx.tree, ctx.leaf_id.as_deref(), &target_id);
                        return ActiveOverlay::TreeAction {
                            target_id,
                            prev_id,
                            next_id,
                            header,
                            opts,
                            active,
                            _filter: sub_filter,
                        };
                    }
                }
                let mut rows = Vec::new();
                let mut row_bgs: Vec<Option<&'static str>> = Vec::new();
                let mut targets = Vec::new();
                let mut active = None;
                Repl::tree_overlay_entries(
                    &ctx.tree,
                    ctx.leaf_id.as_deref(),
                    &filter,
                    &mut rows,
                    &mut row_bgs,
                    &mut targets,
                    &mut active,
                );
                // v2 engine view (Phase 6 step 6): the TreeProvider reads the shared
                // flattened-tree mirror; ordering/labels/indents are the
                // provider's own (v1 flatten logic) and the engine ranks
                // by terms and pins the target ids. Phase 6.7 step 3:
                // derive from the persistent engine (no construction).
                let line = buf.trim_start_matches('/').to_string();
                let st = engine.interpreter.state(&line);
                let _ = (&rows, &row_bgs, &targets, &active);
                ActiveOverlay::Tree {
                    rows,
                    row_bgs,
                    targets,
                    active,
                    _filter: filter,
                    engine_rows: st.candidates.clone(),
                }
            }
            _ => {
                Self::root_list_overlay(engine, buf, rest)
            }
        }
    }

    /// v2 engine root list (Phase 6 step 2): the command registry IS the
    /// root descriptor; the engine ranks and pins. The sticky cursor map
    /// (Phase 6.5) seeds `active` so arrow moves survive the per-frame
    /// rebuild. Phase 6.6: a *bare* listing command (e.g. `/model`, no
    /// trailing space) is routed here too (uniform listing rule, design
    /// §3) — the interpreter's `state(line)` pins that command at the root,
    /// so the root list shows with it highlighted; only a trailing space or
    /// terms drills into the command's own dialog.
    fn root_list_overlay(
        engine: &mut slash_menu_bridge::MenuEngine,
        buf: &str,
        rest: &str,
    ) -> Self {
        let line = buf.trim_start_matches('/').to_string();
        let st = engine.interpreter.state(&line);
        ActiveOverlay::CommandList {
            cmds: slash::filter_commands(rest),
            engine_rows: st.candidates.clone(),
            engine_active: st.active,
        }
    }

    /// Phase 6.6 uniform listing rule (design §3): a *bare* listing command
    /// ("/model", no space in the buffer) stays in the root command list,
    /// already pinned/highlighted by the interpreter. A trailing space or
    /// any following token drills into that command's dialog. Returns
    /// `true` if the caller should render the root list instead.
    fn bare_listing_word(buf: &str) -> bool {
        // A listing command is bare when the buffer holds exactly the
        // command token (no space → no drill-in, no terms).
        !buf.contains(' ')
            && Self::is_listing_command(&buf[1..])
    }

    /// Is `word` a listing command (has its own dialog)? Phase 6.6: the
    /// uniform trailing-space rule applies only to these; leaves still
    /// submit directly.
    fn is_listing_command(word: &str) -> bool {
        matches!(word, "model" | "models" | "m"
            | "thinking" | "think" | "t"
            | "set" | "s"
            | "persona" | "personas" | "p"
            | "session" | "sessions"
            | "tree"
            | "sandbox" | "sb")
    }

    /// Display parameters: `(title, logical options, active index, wrap_opts)`.
    /// The caller feeds these to `Repl::overlay_lines` for final rendering.
    fn display_rows(&self) -> (&str, Vec<String>, Option<usize>, bool) {
        match self {
            ActiveOverlay::None => ("", vec![], None, false),
            ActiveOverlay::Help => {
                let lines: Vec<String> = slash::render_help().lines().map(String::from).collect();
                ("Help  (any key closes)", lines, None, true)
            }
            ActiveOverlay::CommandList { engine_rows, engine_active, .. } => {
                let opts: Vec<String> = engine_rows.iter().map(|r| {
                    let name = format!("/{}", r.id);
                    match &r.description {
                        Some(d) => format!("{:<10} {}", name, d),
                        None => name,
                    }
                }).collect();
                // v2: the engine's active row seeds the *cursor* (the typed
                // pin follows the prompt) but is NOT rendered as a `*` —
                // the root command list has no persistent "current command"
                // state, so no row is ever active-marked. The cursor policy
                // below uses `engine_active` via this value; the marker
                // path sees `None`.
                let active: Option<usize> = Some(*engine_active);
                if opts.is_empty() {
                    ("Commands  (type to filter, up/down, Tab/Enter select)",
                     vec!["(no matching command)".into()], active, false)
                } else {
                    ("Commands  (type to filter, up/down, Tab/Enter select)",
                     opts, active, true)
                }
            }
            ActiveOverlay::Model { engine_rows, filter: _, active, .. } => {
                if engine_rows.is_empty() {
                    return ("Models  (up/down or PgUp/PgDn, Enter select, Esc back)",
                            vec!["(loading models... or no matches)".into()],
                            None, false);
                }
                // Plain generic table: id + provider fields as cells, with
                // numbers rendered as-is (no currency prefix, no custom
                // glyphs). The engine's `render.rs` does the same for
                // dynamic levels; this only names the visible columns.
                let cols = &[
                    ("Model", "id"),
                    ("Ctx", "context"),
                    ("Coding", "coding"),
                    ("Price in", "price_input"),
                    ("Price out", "price_output"),
                    ("Reason", "reasoning"),
                ];
                let avail = crate::output::term_width().saturating_sub(2);
                let mut table = Table::new(&cols.iter().map(|(h, _)| *h).collect::<Vec<_>>())
                    .with_aligns(&[Align::Left, Align::Right, Align::Right, Align::Right, Align::Right, Align::Right])
                    .with_min_widths(&[12, 5, 6, 8, 8, 3])
                    .with_priorities(&[3, 1, 1, 2, 2, 1])
                    .set_gap(2);
                for r in engine_rows {
                    let text = |k: &str| -> String {
                        match r.field_value(k) {
                            Some(crate::slashdot_menu::value::Value::Num(n)) => n.to_string(),
                            Some(crate::slashdot_menu::value::Value::Str(s)) => s.clone(),
                            None => String::new(),
                        }
                    };
                    let mut row: Vec<String> = cols.iter().map(|(_, k)| {
                        if *k == "id" { r.id.clone() } else { text(k) }
                    }).collect();
                    // The id column keeps the full qualified id; the rest
                    // come straight from the row fields.
                    let _ = &mut row;
                    table.add_row(&row);
                }
                let opts = table.render_multiline(avail);
                ("", opts, *active, false)
            }
            ActiveOverlay::Mod { rows, want_value } => {
                if rows.is_empty() {
                    return ("", vec!["(no completions)".into()], None, false);
                }
                let title = if *want_value {
                    "Modifier values  (type to filter, up/down, Enter select, Esc back)"
                } else {
                    "Modifier options  (type to filter, up/down, Enter select, Esc back)"
                };
                let avail = crate::output::term_width().saturating_sub(2);
                let mut table = Table::new(&["Modifier", "Description"])
                    .with_aligns(&[Align::Left, Align::Left])
                    .with_min_widths(&[20, 40])
                    .with_priorities(&[2, 1])
                    .set_gap(2);
                for c in rows {
                    table.add_row(&[c.display.clone(), c.desc.clone()]);
                }
                let opts = table.render_multiline(avail);
                (title, opts, None, false)
            }
            ActiveOverlay::Thinking { rows, active, .. } => {
                let opts: Vec<String> = rows
                    .iter()
                    .map(|r| match &r.description {
                        Some(d) => format!("{} — {}", r.id, d),
                        None => r.id.clone(),
                    })
                    .collect();
                if opts.is_empty() {
                    ("Thinking levels  (up/down, Enter select, Esc back)",
                     vec!["(no matches)".into()], *active, false)
                } else {
                    ("Thinking levels  (up/down, Enter select, Esc back)",
                     opts, *active, false)
                }
            }
            ActiveOverlay::Settings { rows, .. } => {
                if rows.is_empty() {
                    return ("Settings  (type to filter, up/down, Tab/Enter select, Esc back)",
                            vec!["(no matching setting)".into()], None, false);
                }
                let avail = crate::output::term_width().saturating_sub(2);
                let mut table = Table::new(&["Setting", "Value", "Description"])
                    .with_aligns(&[Align::Left, Align::Left, Align::Left])
                    .with_min_widths(&[22, 18, 26])
                    .with_priorities(&[2, 2, 1])
                    .set_gap(2);
                for (meta, value) in rows {
                    table.add_row(&[meta.name, value.as_str(), meta.description]);
                }
                let opts = table.render_multiline(avail);
                ("Settings  (type to filter, up/down, Tab/Enter select, Esc back)",
                 opts, None, false)
            }
            ActiveOverlay::SettingsVal { cands, .. } => {
                if cands.is_empty() {
                    return ("", vec!["(no candidates)".into()], None, false);
                }
                let avail = crate::output::term_width().saturating_sub(2);
                let mut table = Table::new(&["Value", "Description"])
                    .with_aligns(&[Align::Left, Align::Left])
                    .with_min_widths(&[28, 30])
                    .with_priorities(&[2, 1])
                    .set_gap(2);
                for c in cands {
                    table.add_row(&[c.display.clone(), c.desc.clone()]);
                }
                let opts = table.render_multiline(avail);
                // Title is set dynamically from the setting name
                ("", opts, None, false)
            }
            ActiveOverlay::Persona { cands, active, .. } => {
                if cands.is_empty() {
                    return ("Personas  (up/down, Enter select, Esc back)",
                            vec!["(no matching persona)".into()], None, false);
                }
                let avail = crate::output::term_width().saturating_sub(2);
                let mut table = Table::new(&["Persona", "Description"])
                    .with_aligns(&[Align::Left, Align::Left])
                    .with_min_widths(&[20, 30])
                    .with_priorities(&[2, 1])
                    .set_gap(2);
                for c in cands {
                    table.add_row(&[c.display.clone(), c.desc.clone()]);
                }
                let opts = table.render_multiline(avail);
                ("Personas  (up/down, Enter select, Esc back)",
                 opts, *active, false)
            }
            ActiveOverlay::Stat { lines } => {
                ("Session stats  (Esc clears input to close)",
                 lines.clone(), None, true)
            }
            ActiveOverlay::ReplayHelp { lines } => {
                ("History replay  (type a length filter or Esc to cancel)",
                 lines.clone(), None, true)
            }
            ActiveOverlay::Sandbox { items, sub, .. } => {
                let title = match sub {
                    Some("remove") => "Sandbox bindings  (type to filter, up/down, Tab/Enter select, Esc back)",
                    _ => "Sandbox bindings  (type to filter, up/down, Tab/Enter select, Esc back)",
                };
                (title,
                 items.clone(), None, false)
            }
            ActiveOverlay::PathComplete { completions, .. } => {
                ("Path completions  (up/down, Tab/Enter select, Esc dismiss)",
                 completions.clone(), None, false)
            }
            ActiveOverlay::Session { opts, active, .. } => {
                if opts.is_empty() {
                    ("Sessions  (up/down, Tab complete, Enter select, Esc back)",
                     vec!["(no sessions)".into()], None, false)
                } else {
                    ("Sessions  (up/down, Tab complete, Enter select, Esc back)",
                     opts.clone(), *active, false)
                }
            }
            ActiveOverlay::SessionAction { opts, active, .. } => {
                ("Session actions  (up/down, Tab/Enter select, Esc back)",
                 opts.clone(), *active, false)
            }
            ActiveOverlay::Confirm { opts, active, .. } => {
                ("Session actions  (up/down, Tab/Enter select, Esc back)",
                 opts.clone(), *active, false)
            }
            ActiveOverlay::Tree { rows, active, .. } => {
                ("Tree  (up/down, Enter navigate, Esc back)",
                 rows.clone(), *active, false)
            }
            ActiveOverlay::TreeAction { opts, active, .. } => {
                ("", opts.clone(), *active, false)
            }
            ActiveOverlay::SwitchAction { opts, active, .. } => {
                ("", opts.clone(), *active, false)
            }
            ActiveOverlay::CompactSummary { opts, active, .. } => {
                ("", opts.clone(), *active, false)
            }
        }
    }

    fn title(&self) -> String {
        match self {
            ActiveOverlay::Model { filter, .. } => {
                // Step 2: the engine owns filtering; the title carries the
                // raw typed modifiers (the old ModelFilter title_suffix
                // summarization is gone with the v1 filter plumbing).
                let suffix = filter.title_suffix();
                if suffix.is_empty() {
                    "Models".to_string()
                } else {
                    format!("Models [{}]", suffix)
                }
            }
            ActiveOverlay::SettingsVal { key, .. } => {
                let name = crate::settings::SETTINGS
                    .iter()
                    .find(|m| m.key == *key)
                    .map(|m| m.name)
                    .unwrap_or("?");
                format!("{}  (type to filter, up/down, Enter select, Esc back)", name)
            }
            _ => String::new(),
        }
    }

    /// The engine-owned cursor row for this overlay (Phase 6.5), if backed
    /// by the engine. None for legacy overlays (host cursor).
    pub fn cursor_row(&self) -> Option<usize> {
        match self {
            ActiveOverlay::CommandList { engine_active, .. } => Some(*engine_active),
            ActiveOverlay::Model { cursor, .. } => Some(*cursor),
            ActiveOverlay::Thinking { cursor, .. } => Some(*cursor),
            ActiveOverlay::Persona { cursor, .. } => Some(*cursor),
            _ => None,
        }
    }

    /// Phase 6.5/6.7: force the engine cursor to an absolute row (host paging
    /// sync; open-time seeding). The engine is persistent, so the next
    /// frame re-derives `sel` from the same instance.
    fn engine_seed(
        &self,
        editor: &LineEditor,
        engine: &mut slash_menu_bridge::MenuEngine,
        idx: usize,
    ) {
        let line = editor.text().trim_start_matches('/').to_string();
        engine.interpreter.set_cursor(&line, idx);
    }

    /// Phase 6.5: route an Up/Down key through the engine's cursor
    /// (`move_cursor`). The engine is persistent; the next frame re-derives
    /// `sel` from its `active`.
    fn engine_arrow(
        &self,
        editor: &LineEditor,
        engine: &mut slash_menu_bridge::MenuEngine,
        delta: isize,
    ) {
        let line = editor.text().trim_start_matches('/').to_string();
        engine.interpreter.move_cursor(&line, delta);
    }

    /// Phase 6.6 item 3 (design §5.1): route Esc through the engine's
    /// back-navigation ladder for engine-backed overlays. Traversal frames
    /// are replayed (previous filter/cursor) before the structural
    /// per-level pop; the engine produces the new buffer. Returns `true`
    /// if the engine handled it (buffer written).
    #[must_use]
    fn engine_esc(
        &self,
        editor: &mut LineEditor,
        engine: &mut slash_menu_bridge::MenuEngine,
    ) -> bool {
        let line = editor.text().trim_start_matches('/').to_string();
        let r = engine.step_at(
            &line,
            0,
            slash_menu_bridge::HostKey::Esc,
        );
        if let Some(b) = r.buffer {
            if b.is_empty() {
                editor.set("");
            } else {
                editor.set(&format!("/{}", b));
            }
        }
        true
    }

    /// Handle Tab on a selected row.  Mutates the editor in-place.
    fn handle_tab(
        &self,
        sel: usize,
        editor: &mut LineEditor,
        path_completions: &mut Vec<String>,
        completion_pos: &mut Option<(usize, usize)>,
        settings: &SettingsStore,
        engine: &mut slash_menu_bridge::MenuEngine,
    ) {
        match self {
            ActiveOverlay::CommandList { engine_rows, .. } => {
                // v2 engine Tab (Phase 6 step 2). Phase 6.7: the engine owns
                // the cursor — the sticky map carries the arrow-moved row;
                // `sel` is only a fallback for the first frame.
                let buf = editor.text();
                let line = buf.trim_start_matches('/').to_string();
                let r = engine.step_at(
                    &line,
                    sel,
                    slash_menu_bridge::HostKey::Tab,
                );
                if let Some(b) = r.buffer {
                    editor.set(&format!("/{}", b));
                }
                let _ = engine_rows;
            }
            ActiveOverlay::Model { engine_rows, .. } => {
                // v2 engine Tab (Phase 6 step 4): host cursor decides the
                // row; clear-filter accept semantics (path+id only). The
                // engine sees the ModelProvider via `shared` (dynamic rows).
                if !engine_rows.is_empty() {
                    let buf = editor.text();
                    let line = buf.trim_start_matches('/').to_string();
                    let r = engine.step_at(
                        &line,
                        sel,
                        slash_menu_bridge::HostKey::Tab,
                    );
                    if let Some(b) = r.buffer {
                        editor.set(&format!("/{}", b));
                    }
                }
            }
            ActiveOverlay::Mod { rows, .. } => {
                if let Some(c) = rows.get(sel) {
                    let buf = editor.text();
                    // Finish the dotted token. If we're completing a full
                    // `.name:value`, replace the whole token; otherwise replace
                    // just the trailing `.xxx` fragment with `.name:`.
                    let val = &c.value;
                    editor.set(&Repl::replace_last_token(&buf, val));
                }
            }
            ActiveOverlay::Thinking { .. } => {
                // v2 engine owns the Tab rewrite. Phase 6.7: engine is
                // persistent — cursor + frames live inside; `sel` is a
                // fallback for the first frame.
                let buf = editor.text();
                let line = buf.trim_start_matches('/').to_string();
                let r = engine.step_at(
                    &line,
                    sel,
                    slash_menu_bridge::HostKey::Tab,
                );
                if let Some(b) = r.buffer {
                    editor.set(&format!("/{}", b));
                }
                let _ = r.restore_cursor;
            }
            ActiveOverlay::Settings { rows, .. } => {
                if let Some((meta, _)) = rows.get(sel) {
                    editor.set(&format!("/set {} ", meta.name));
                }
            }
            ActiveOverlay::SettingsVal { cands, key, .. } => {
                if let Some(c) = cands.get(sel) {
                    let buf = editor.text();
                    let setting_name = crate::settings::SETTINGS
                        .iter()
                        .find(|m| m.key == *key)
                        .map(|m| m.name)
                        .unwrap_or("?");
                    let trimmed = buf.trim_end();
                    if trimmed.ends_with(setting_name) {
                        editor.set(&format!("/set {} {}", setting_name, c.value));
                    } else {
                        editor.set(&Repl::replace_last_token(&buf, &c.value));
                    }
                }
            }
            ActiveOverlay::Persona { cands, .. } => {
                if let Some(c) = cands.get(sel) {
                    let buf = editor.text();
                    let tok = if c.value.is_empty() {
                        c.display.clone()
                    } else {
                        c.value.clone()
                    };
                    editor.set(&Repl::replace_last_token(&buf, &tok));
                }
            }
            ActiveOverlay::Sandbox { sub, .. } => {
                let buf = editor.text();
                match sub {
                    Some("add_ro") | Some("add_rw") => {
                        let after = &buf["/sandbox".len()..].trim();
                        let rest = after.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                        if !rest.is_empty() {
                            let last_token = rest.split_whitespace().last().unwrap_or("");
                            let uses_tilde = last_token.starts_with('~');
                            let expanded = if uses_tilde {
                                crate::path_util::expand_tilde(last_token).to_string_lossy().into_owned()
                            } else {
                                last_token.to_string()
                            };
                            let completions = Repl::complete_path_prefix(&expanded);
                            // Round-trip `~`-prefixed results back to `~`.
                            let home = if uses_tilde {
                                std::env::var("HOME").map(|h| {
                                    if h.ends_with('/') { h } else { format!("{}/", h) }
                                }).ok()
                            } else {
                                None
                            };
                            let completions: Vec<String> = completions.into_iter().map(|c| {
                                if let Some(ref home) = home {
                                    if let Some(rest) = c.strip_prefix(home) {
                                        return format!("~/{}", rest);
                                    }
                                }
                                c
                            }).collect();
                            if let Some(c) = completions.get(sel) {
                                editor.set(&Repl::replace_last_token(&buf, c));
                            }
                        }
                    }
                    Some("remove") => {
                        let buf = editor.text();
                        let filter = buf["/sandbox".len()..].trim()
                            .split_once(' ')
                            .map(|(_, r)| r.trim())
                            .unwrap_or("");
                        let combined = crate::sandbox_bindings::combined_list();
                        let filtered: Vec<_> = if filter.is_empty() {
                            combined
                        } else {
                            crate::fuzzy::fuzzy_filter(&combined, filter, |b| {
                                let mode_str = if b.mode == crate::config::BindMode::Ro { "ro" } else { "rw" };
                                let src = if b.is_excluded { "excluded" } else if b.is_session { "session" } else { "cfg" };
                                format!("{} {} {} {}",
                                    b.host_path.to_string_lossy(),
                                    b.sandbox_path.to_string_lossy(),
                                    mode_str,
                                    src)
                            })
                            .into_iter()
                            .map(|r| r.item.clone())
                            .collect()
                        };
                        if let Some(r) = filtered.get(sel) {
                            let path = r.sandbox_path.to_string_lossy().to_string();
                            // If the buffer doesn't have a space after "remove" yet,
                            // insert one so the path is properly tokenized.
                            let trimmed = buf.trim_end();
                            let new_buf = if trimmed.ends_with("remove") && !buf.ends_with(' ') {
                                format!("{} {}", trimmed, path)
                            } else {
                                Repl::replace_last_token(&buf, &path)
                            };
                            editor.set(&new_buf);
                        }
                    }
                    None => {
                        let buf = editor.text();
                        let filter = buf["/sandbox".len()..].trim();
                        let subcommands = ["add_ro", "add_rw", "remove"];
                        let helptext = ["Make path available read-only",
                                        "Make path available read-write",
                                        "Remove a path from the bindings"];
                        // Replicate the same prefix/contains filtering as
                        // `sandbox_overlay_entries` to map sel → subcommand name.
                        let filtered: Vec<&str> = subcommands.iter().enumerate()
                            .filter(|(i, name)| {
                                name.starts_with(filter) || helptext[*i].contains(filter)
                            })
                            .map(|(_, name)| *name)
                            .collect();
                        if let Some(c) = filtered.get(sel) {
                            editor.set(&format!("/sandbox {} ", c));
                        }
                    }
                    _ => {}
                }
            }
            ActiveOverlay::PathComplete { completions, completion_pos: pos } => {
                if let Some(c) = completions.get(sel) {
                    let buf = editor.text();
                    if let Some((start, end)) = pos {
                        editor.replace_range(*start, *end, c);
                    } else {
                        editor.set(&Repl::replace_last_token(&buf, c));
                    }
                    path_completions.clear();
                    *completion_pos = None;
                }
            }
            ActiveOverlay::Session { names, .. } => {
                if let Some(name) = names.get(sel) {
                    editor.set(&format!("/session {}", name));
                }
            }
            ActiveOverlay::SessionAction { session_name, current_name, opts, .. } => {
                // Fill the chosen action into the buffer (stay in input).
                if let Some(label) = opts.get(sel) {
                    let action_str = match label.as_str() {
                        _ if label.starts_with("resume") => "resume",
                        "fork" => "fork",
                        "rename" => {
                            // Pre-fill the current name for editing if nothing typed yet.
                            let buf = editor.text();
                            let rest = &buf["/session".len()..].trim();
                            let (_name, after_name) = crate::agent::parse_session_name_after(rest);
                            let trailing = after_name.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim();
                            if trailing.is_empty() {
                                let default = current_name.clone().unwrap_or_default();
                                if default.is_empty() {
                                    "rename "
                                } else {
                                    editor.set(&format!("/session {} rename {}", session_name, default));
                                    ""
                                }
                            } else {
                                let line = format!("/session {} rename {}", session_name, trailing);
                                editor.set(&line);
                                ""
                            }
                        }
                        "delete" => "delete",
                        _ => "",
                    };
                    if !action_str.is_empty() {
                        // Preserve any argument already typed after the action
                        // (e.g. "10l" in "/session <id> resume 10l").
                        let buf = editor.text();
                        let rest = &buf["/session".len()..].trim();
                        let (_name, after_name) = crate::agent::parse_session_name_after(rest);
                        let trailing = after_name.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim();
                        if trailing.is_empty() {
                            editor.set(&format!("/session {} {}", session_name, action_str));
                        } else {
                            editor.set(&format!("/session {} {} {}", session_name, action_str.trim(), trailing));
                        }
                    }
                }
            }
            ActiveOverlay::Confirm { session_name, opts, .. } => {
                // Tab fills "confirm" action into buffer.
                if let Some(label) = opts.get(sel) {
                    let confirm_str = match label.as_str() {
                        "Confirm" => "confirm",
                        _ => "",
                    };
                    if !confirm_str.is_empty() {
                        editor.set(&format!("/session {} confirm", session_name));
                    }
                }
            }
            ActiveOverlay::Tree { targets, engine_rows, .. } => {
                // Complete the id into the buffer; the next from_buffer pass
                // promotes to the action menu (mirrors the Session overlay).
                // v2 engine path when rows present: host cursor decides.
                if !engine_rows.is_empty() {
                    let buf = editor.text();
                    let line = buf.trim_start_matches('/').to_string();
                    let r = engine.step_at(
                        &line,
                        sel,
                        slash_menu_bridge::HostKey::Tab,
                    );
                    if let Some(b) = r.buffer {
                        // Canonical v2 line "tree <id>" → the host's
                        // angle-bracket id form (the v1 action-menu parser).
                        let id = b.split_whitespace().nth(1).unwrap_or("");
                        editor.set(&format!("/tree <{}>", id));
                    }
                } else if let Some(target_id) = targets.get(sel) {
                    editor.set(&format!("/tree <{}>", target_id));
                }
            }
            ActiveOverlay::TreeAction { target_id, opts, .. } => {
                // Tab fills the chosen action; sub-menus open via promotion.
                if let Some(label) = opts.get(sel) {
                    let action = match label.as_str() {
                        "switch" => "switch",
                        "compact" => "compact",
                        _ => "",
                    };
                    if !action.is_empty() {
                        editor.set(&format!("/tree <{}> {}", target_id, action));
                    }
                }
            }
            ActiveOverlay::SwitchAction { target_id, opts, .. } => {
                if let Some(label) = opts.get(sel) {
                    let mode = match label.as_str() {
                        "just switch" => "",
                        "summarize" => " --summarize",
                        _ => "",
                    };
                    editor.set(&format!("/tree <{}> switch{}", target_id, mode));
                }
            }
            ActiveOverlay::CompactSummary { target_id, opts, .. } => {
                if let Some(label) = opts.get(sel) {
                    let mode = match label.as_str() {
                        "manual" => " manual",
                        "text only" => " text-only",
                        // LLM mode stages an editable prompt for the user
                        // to tweak before submitting; no emit_commit here
                        // (the user hasn't committed yet — see
                        // doc/tree_compaction.md §6.7).
                        "llm" => " llm ",
                        _ => "",
                    };
                    // Both `/tree <id> compact` and `/compact` resolve
                    // to the same canonical line:
                    // `/tree <{target_id}> compact <mode>`.
                    let line = format!("/tree <{}> compact{}", target_id, mode);
                    if label.as_str() == "llm" {
                        // Stage the prompt: <line><settings.llm_compact_prompt()>
                        let staged = format!("{}{}", line, settings.llm_compact_prompt());
                        editor.set(&staged);
                    } else {
                        editor.set(&line);
                    }
                }
            }
            _ => {}
        }
    }

    /// Handle Enter on a selected row.  Returns an action for the caller.
    fn handle_enter<F: Fn(&LineEditor, &[&str], usize, &mut usize)>(
        &self,
        sel: usize,
        editor: &mut LineEditor,
        output: &dyn Output,
        redraw: &F,
        input_row: &mut usize,
        settings: &mut SettingsStore,
        dirty: &mut bool,
        value_error: &mut Option<String>,
        help_view: &mut bool,
        ctx: &ReplCtx,
        engine: &mut slash_menu_bridge::MenuEngine,
    ) -> OverlayAction {
        match self {
            ActiveOverlay::CommandList { engine_rows, .. } => {
                // v2 engine Enter (Phase 6 step 2). Phase 6.7: engine is
                // persistent — cursor/frames live inside; `sel` is only a
                // fallback for the first frame. A pinned root child drills
                // (buffer "<cmd> ", arg menu opens); leaf dispatch still
                // goes through SlashKind below once the id is committed.
                let buf = editor.text();
                let line = buf.trim_start_matches('/').to_string();
                let r = engine.step_at(
                    &line,
                    sel,
                    slash_menu_bridge::HostKey::Enter,
                );
                match r.action {
                    crate::slashdot_menu::accept::AcceptOutcome::Continue(
                        crate::slashdot_menu::interpreter::OverlayAction::Buffer(b),
                    ) => {
                        editor.set(&format!("/{}", b));
                        // Cursor restore is not propagated: the caller
                        // re-derives `sel` when the buffer re-renders.
                        let _ = r.restore_cursor;
                        return OverlayAction::Continue;
                    }
                    crate::slashdot_menu::accept::AcceptOutcome::Continue(
                        crate::slashdot_menu::interpreter::OverlayAction::Submit(canon),
                    ) => {
                        // Leaf command accepted: translate the canonical
                        // line to the SlashKind dispatch (same path as
                        // typed submission).
                        let key = canon.split(' ').next().unwrap_or("");
                        let item = slash::registry()
                            .iter()
                            .find(|c| c.name.trim_start_matches('/') == key);
                        if let Some(cmd) = item {
                            match cmd.kind {
                                slash::SlashKind::Help => {
                                    *help_view = true;
                                    editor.set("");
                                }
                                slash::SlashKind::Quit => {
                                    redraw(&LineEditor::new(), &[], sel, input_row);
                                    output.display_plain("");
                                    return OverlayAction::Action(InputAction::Quit);
                                }
                                slash::SlashKind::Model => editor.set("/model "),
                                slash::SlashKind::Thinking => editor.set("/thinking "),
                                slash::SlashKind::Settings => editor.set("/set "),
                                slash::SlashKind::Persona => editor.set("/persona "),
                                slash::SlashKind::Stat => editor.set("/stat "),
                                slash::SlashKind::Sandbox => editor.set("/sandbox "),
                                slash::SlashKind::Session => editor.set("/session path:. "),
                                slash::SlashKind::Tree => editor.set("/tree "),
                                slash::SlashKind::Compact => editor.set("/compact "),
                                slash::SlashKind::Clear => {
                                    redraw(&LineEditor::new(), &[], sel, input_row);
                                    output.display_plain("/clear");
                                    return OverlayAction::Action(
                                        InputAction::Submit("/clear".into()),
                                    );
                                }
                                slash::SlashKind::New => {
                                    redraw(&LineEditor::new(), &[], sel, input_row);
                                    output.display_plain("/new");
                                    return OverlayAction::Action(
                                        InputAction::Submit("/new".into()),
                                    );
                                }
                                slash::SlashKind::Replay => editor.set("/replay "),
                            }
                        }
                        return OverlayAction::Continue;
                    }
                    _ => {}
                }
                let _ = engine_rows;
                OverlayAction::Continue
            }
ActiveOverlay::Model { engine_rows, .. } => {
                // v2 engine Enter (Phase 6 step 4): dynamic row + leaf →
                // Submit with the canonical line; the host translates to
                // SetModel. The engine sees the ModelProvider via `shared`.
                if !engine_rows.is_empty() {
                    let buf = editor.text();
                    let line = buf.trim_start_matches('/').to_string();
                    let r = engine.step_at(
                        &line,
                        sel,
                        slash_menu_bridge::HostKey::Enter,
                    );
                    if let crate::slashdot_menu::accept::AcceptOutcome::Continue(
                        crate::slashdot_menu::interpreter::OverlayAction::Submit(canon),
                    ) = r.action
                    {
                        let qualified = canon.split_whitespace().nth(1).unwrap_or("");
                        let (provider, model) = qualified
                            .split_once(':')
                            .map(|(p, m)| (p.to_string(), m.to_string()))
                            .unwrap_or((ctx.provider.clone(), qualified.to_string()));
                        Repl::emit_commit(output, editor, redraw, sel, input_row, &format!("/model {}", qualified), "", false);
                        return OverlayAction::Action(InputAction::SetModel(provider, model));
                    }
                    return OverlayAction::Continue;
                }
                OverlayAction::Continue
            }
            ActiveOverlay::Mod { rows, .. } => {
                if let Some(c) = rows.get(sel) {
                    let buf = editor.text();
                    editor.set(&Repl::replace_last_token(&buf, &c.value));
                }
                OverlayAction::Continue
            }
            ActiveOverlay::Thinking { .. } => {
                // v2 engine owns the Enter flow; the host cursor decides
                // the row. Bare level → drill (value menu shows); valid
                // value → SetThinking.
                let buf = editor.text();
                let line = buf.trim_start_matches('/').to_string();
                let r = engine.step_at(
                    &line,
                    sel,
                    slash_menu_bridge::HostKey::Enter,
                );
                match r.action {
                    crate::slashdot_menu::accept::AcceptOutcome::Continue(
                        crate::slashdot_menu::interpreter::OverlayAction::Buffer(b),
                    ) => {
                        editor.set(&format!("/{}", b));
                        // Cursor restore is not propagated: the caller
                        // re-derives `sel` when the buffer re-renders.
                        let _ = r.restore_cursor;
                        OverlayAction::Continue
                    }
                    crate::slashdot_menu::accept::AcceptOutcome::Continue(
                        crate::slashdot_menu::interpreter::OverlayAction::Submit(canon),
                    ) => {
                        let level = canon
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("")
                            .to_string();
                        let echo = format!("/thinking {}", level);
                        Repl::emit_commit(
                            output, editor, redraw, sel, input_row, &echo, "", false,
                        );
                        OverlayAction::Action(InputAction::SetThinking(level))
                    }
                    _ => OverlayAction::Continue,
                }
            }
            ActiveOverlay::Persona { cands, .. } => {
                if let Some(c) = cands.get(sel) {
                    let name = if c.value.is_empty() {
                        c.display.clone()
                    } else {
                        c.value.clone()
                    };
                    let line = format!("/persona {}", name);
                    Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                    OverlayAction::Action(InputAction::SetPersona(name))
                } else {
                    OverlayAction::Continue
                }
            }
            ActiveOverlay::Settings { rows, .. } => {
                if let Some((meta, _)) = rows.get(sel) {
                    editor.set(&format!("/set {} ", meta.name));
                }
                OverlayAction::Continue
            }
            ActiveOverlay::SettingsVal { cands, key, .. } => {
                let vq = &self.filter_for(editor);
                let vq_eff = if vq.trim().is_empty() {
                    cands.get(sel).map(|c| c.value.clone()).unwrap_or_default()
                } else {
                    vq.clone()
                };
                match settings.set_value(*key, &vq_eff) {
                    Ok(()) => {
                        *value_error = None;
                        *dirty = true;
                        if vq.trim().is_empty() {
                            if let Some(name) =
                                crate::settings::SETTINGS.iter().find(|m| m.key == *key).map(|m| m.name)
                            {
                                editor.set(&format!("/set {} {}", name, vq_eff));
                            }
                        }
                        let line = editor.text();
                        Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "/set", true);
                    }
                    Err(msg) => {
                        *value_error = Some(msg);
                    }
                }
                OverlayAction::Continue
            }
            ActiveOverlay::Sandbox { sub, .. } => {
                let buf = editor.text();
                match sub {
                    Some("add_ro") | Some("add_rw") => {
                        let after = &buf["/sandbox".len()..].trim();
                        let path = after.split_once(' ').map(|(_, r)| r.trim()).unwrap_or("");
                        if !path.is_empty() {
                            let parts: Vec<&str> = path.split_whitespace().collect();
                            let host = parts[0];
                            let sandbox_path = parts.get(1).unwrap_or(&"");
                            let line = if sandbox_path.is_empty() {
                                format!("/sandbox {} {}", sub.as_ref().unwrap_or(&""), host)
                            } else {
                                format!("/sandbox {} {} {}", sub.as_ref().unwrap_or(&""), host, sandbox_path)
                            };
                            Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                            return OverlayAction::Action(
                                InputAction::Sandbox(sub.as_ref().unwrap_or(&"").to_string(), host.to_string(), sandbox_path.to_string()),
                            );
                        }
                        editor.set(&format!("/sandbox {} ", sub.as_ref().unwrap_or(&"")));
                    }
                    Some("remove") => {
                        let buf = editor.text();
                        let filter = buf["/sandbox".len()..].trim()
                            .split_once(' ')
                            .map(|(_, r)| r.trim())
                            .unwrap_or("");
                        let (sub_cmd, sandbox_path) = {
                            let combined = crate::sandbox_bindings::combined_list();
                            let filtered: Vec<_> = if filter.is_empty() {
                                combined
                            } else {
                                crate::fuzzy::fuzzy_filter(&combined, filter, |b| {
                                    let mode_str = if b.mode == crate::config::BindMode::Ro { "ro" } else { "rw" };
                                    let src = if b.is_excluded { "excluded" } else if b.is_session { "session" } else { "cfg" };
                                    format!("{} {} {} {}",
                                        b.host_path.to_string_lossy(),
                                        b.sandbox_path.to_string_lossy(),
                                        mode_str,
                                        src)
                                })
                                .into_iter()
                                .map(|r| r.item.clone())
                                .collect()
                            };
                            let sub = filtered.get(sel)
                                .map(|b| if b.is_session { "remove" } else { "exclude" })
                                .unwrap_or("remove");
                            let path = filtered.get(sel)
                                .map(|b| b.sandbox_path.to_string_lossy().to_string())
                                .unwrap_or_default();
                            (sub, path)
                        };
                        if !sandbox_path.is_empty() {
                            let line = format!("/sandbox {} {}", sub_cmd, sandbox_path);
                            Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                            return OverlayAction::Action(
                                InputAction::Sandbox(sub_cmd.to_string(), sandbox_path, String::new()),
                            );
                        }
                    }
                    None => {
                        let filter = buf["/sandbox".len()..].trim();
                        let subcommands = ["add_ro", "add_rw", "remove"];
                        let helptext = ["Make path available read-only",
                                        "Make path available read-write",
                                        "Remove a path from the bindings"];
                        let filtered: Vec<&str> = subcommands.iter().enumerate()
                            .filter(|(i, name)| {
                                name.starts_with(filter) || helptext[*i].contains(filter)
                            })
                            .map(|(_, name)| *name)
                            .collect();
                        if let Some(c) = filtered.get(sel) {
                            editor.set(&format!("/sandbox {} ", c));
                        }
                    }
                    _ => {}
                }
                OverlayAction::Continue
            }
            ActiveOverlay::PathComplete { completions, completion_pos: pos } => {
                if let Some(c) = completions.get(sel) {
                    let buf = editor.text();
                    if let Some((start, end)) = pos {
                        editor.replace_range(*start, *end, c);
                    } else {
                        editor.set(&Repl::replace_last_token(&buf, c));
                    }
                }
                OverlayAction::Continue
            }
            ActiveOverlay::Session { names, .. } => {
                if names.is_empty() {
                    // No sessions match the filter (e.g. `/session <name> rename newname`
                    // after the action menu selected "rename"). Submit the buffer so the
                    // command reaches `handle_session_command` instead of doing nothing.
                    let line = editor.text();
                    Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                    return OverlayAction::Action(InputAction::Submit(line));
                }
                // Selecting a session completes its name; the next loop
                // iteration promotes the buffer to the action menu (kind 11).
                if let Some(name) = names.get(sel) {
                    editor.set(&format!("/session {}", name));
                }
                OverlayAction::Continue
            }
            ActiveOverlay::SessionAction { session_name, current_name, is_current, opts, .. } => {
                if let Some(label) = opts.get(sel) {
                    match label.as_str() {
                        _ if label.starts_with("resume") => {
                            // Preserve any argument already typed after the action (e.g. "10l").
                            let buf = editor.text();
                            let rest = &buf["/session".len()..].trim();
                            let (_name, after_name) = crate::agent::parse_session_name_after(rest);
                            let after_action = after_name.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim().to_string();
                            let line = if after_action.is_empty() {
                                format!("/session {} resume", session_name)
                            } else {
                                format!("/session {} resume {}", session_name, after_action)
                            };
                            Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                            OverlayAction::Action(InputAction::Submit(line))
                        }
                        "fork" => {
                            let line = format!("/session {} fork", session_name);
                            Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                            OverlayAction::Action(InputAction::Submit(line))
                        }
                        "rename" => {
                            // If the user already typed text after "rename", submit it.
                            // Otherwise pre-fill the current name (if any) for editing.
                            let buf = editor.text();
                            let rest = &buf["/session".len()..].trim();
                            let (_name, after_name) = crate::agent::parse_session_name_after(rest);
                            let trailing = after_name.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim();
                            if trailing.is_empty() {
                                let default = current_name.clone().unwrap_or_default();
                                let line = format!("/session {} rename {}", session_name, default);
                                editor.set(&line);
                                OverlayAction::Continue
                            } else {
                                let line = format!("/session {} rename {}", session_name, trailing);
                                Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                                OverlayAction::Action(InputAction::Submit(line))
                            }
                        }
                        "delete" => {
                            // Set buffer to trigger confirmation overlay.
                            // `from_buffer` will detect `<name> delete` and show Confirm.
                            let line = if *is_current {
                                format!("/session {} delete_current", session_name)
                            } else {
                                format!("/session {} delete", session_name)
                            };
                            editor.set(&line);
                            OverlayAction::Continue
                        }
                        _ => OverlayAction::Continue,
                    }
                } else {
                    OverlayAction::Continue
                }
            }
            ActiveOverlay::Confirm { confirm_action, session_name, opts, .. } => {
                if let Some(label) = opts.get(sel) {
                    match label.as_str() {
                        "Confirm" => {
                            let line = confirm_action.clone();
                            Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                            OverlayAction::Action(InputAction::Submit(line))
                        }
                        _ => {
                            // Cancel — go back to the session action menu.
                            editor.set(&format!("/session {}", session_name));
                            OverlayAction::Continue
                        }
                    }
                } else {
                    OverlayAction::Continue
                }
            }
            ActiveOverlay::Tree { targets, engine_rows, .. } => {
                // v2 engine Enter when rows present (host cursor decides);
                // legacy targets path unchanged.
                if !engine_rows.is_empty() {
                    let buf = editor.text();
                    let line = buf.trim_start_matches('/').to_string();
                    let r = engine.step_at(
                        &line,
                        sel,
                        slash_menu_bridge::HostKey::Enter,
                    );
                    if let Some(b) = r.buffer {
                        let id = b.split_whitespace().nth(1).unwrap_or("");
                        editor.set(&format!("/tree <{}>", id));
                    }
                    return OverlayAction::Continue;
                }
                if let Some(target_id) = targets.get(sel) {
                    editor.set(&format!("/tree <{}>", target_id));
                }
                OverlayAction::Continue
            }
            ActiveOverlay::TreeAction { target_id, opts, .. } => {
                if let Some(label) = opts.get(sel) {
                    match label.as_str() {
                        "switch" => {
                            editor.set(&format!("/tree <{}> switch", target_id));
                        }
                        "compact" => {
                            editor.set(&format!("/tree <{}> compact", target_id));
                        }
                        _ => {}
                    }
                }
                OverlayAction::Continue
            }
            ActiveOverlay::SwitchAction { target_id, opts, .. } => {
                if let Some(label) = opts.get(sel) {
                    let line = match label.as_str() {
                        "just switch" => format!("/tree <{}> switch", target_id),
                        "summarize" => format!("/tree <{}> switch --summarize", target_id),
                        _ => String::new(),
                    };
                    if !line.is_empty() {
                        Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                        return OverlayAction::Action(InputAction::Submit(line));
                    }
                }
                OverlayAction::Continue
            }
            ActiveOverlay::CompactSummary { target_id, opts, .. } => {
                if let Some(label) = opts.get(sel) {
                    // If the buffer already carries a fully-formed
                    // `compact llm <payload>` line, the user has
                    // already committed a canonical prompt. Honor it
                    // directly — commit and submit, regardless of
                    // which option is currently highlighted (the fuzzy
                    // filter treats the long payload as a query and
                    // may not pre-select `llm`). This is the only
                    // point where the slash-menu convention (Enter
                    // commits a canonical line) holds across label
                    // selection.
                    if label.as_str() == "llm" {
                        let buf = editor.text().to_string();
                        if Repl::llm_compact_buffer_is_canonical(
                            &buf,
                            target_id,
                        ) {
                            Repl::emit_commit(
                                output,
                                editor,
                                redraw,
                                sel,
                                input_row,
                                &buf,
                                "",
                                false,
                            );
                            return OverlayAction::Action(InputAction::Submit(buf));
                        }
                        // Otherwise stage the default prompt for editing.
                        let prefix_str =
                            format!("/tree <{}> compact llm", target_id);
                        let staged = format!("{} {}", prefix_str, settings.llm_compact_prompt());
                        editor.set(&staged);
                        return OverlayAction::Continue;
                    }
                    let head = format!("/tree <{}> compact", target_id);
                    let line = match label.as_str() {
                        "manual" => format!("{} manual", head),
                        "text only" => format!("{} text-only", head),
                        _ => String::new(),
                    };
                    if !line.is_empty() {
                        Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                        return OverlayAction::Action(InputAction::Submit(line));
                    }
                }
                OverlayAction::Continue
            }
            ActiveOverlay::ReplayHelp { .. } => {
                let line = editor.text();
                if !line.trim().is_empty() {
                    Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                    OverlayAction::Action(InputAction::Submit(line))
                } else {
                    OverlayAction::Continue
                }
            }
            _ => OverlayAction::Continue,
        }
    }

    /// Handle Escape.
    fn handle_esc(
        &self,
        editor: &mut LineEditor,
        path_completions: &mut Vec<String>,
        completion_pos: &mut Option<(usize, usize)>,
    ) {
        match self {
            ActiveOverlay::CommandList { .. }
            | ActiveOverlay::Model { .. }
            | ActiveOverlay::Thinking { .. }
            | ActiveOverlay::Persona { .. }
            | ActiveOverlay::Stat { .. }
            | ActiveOverlay::ReplayHelp { .. }
            | ActiveOverlay::Sandbox { .. } => {
                editor.set("");
            }
            ActiveOverlay::Settings { .. } | ActiveOverlay::SettingsVal { .. } => {
                let buf = editor.text();
                if let Some(idx) = buf.rfind(' ') {
                    editor.set(&buf[..idx]);
                } else {
                    editor.set("");
                }
            }
            ActiveOverlay::PathComplete { .. } | ActiveOverlay::Help { .. } => {
                path_completions.clear();
                *completion_pos = None;
            }
            ActiveOverlay::Session { .. } => {
                // Clear the session filter (go back to bare `/session`).
                let buf = editor.text();
                if let Some(idx) = buf.rfind(' ') {
                    editor.set(&buf[..idx]);
                } else {
                    editor.set("");
                }
            }
            ActiveOverlay::SessionAction { .. } => {
                // Go back to the session list.
                editor.set("/session ");
            }
            ActiveOverlay::Confirm { session_name, .. } => {
                // Go back to the session action menu.
                editor.set(&format!("/session {}", session_name));
            }
            ActiveOverlay::Tree { .. } => {
                editor.set("");
            }
            ActiveOverlay::Mod { .. } => {
                // Back up from the modifier auto-complete to the model list,
                // dropping the trailing dotted token (the parent level).
                editor.set("/model ");
            }
            ActiveOverlay::TreeAction { target_id, .. } => {
                // Back to the tree list (keep nothing: the id may have come
                // from Enter-complete; the list is the parent level).
                let _ = target_id;
                editor.set("/tree ");
            }
            ActiveOverlay::SwitchAction { target_id, .. }
            | ActiveOverlay::CompactSummary { target_id, .. } => {
                // Back to the action menu for the same entry.
                editor.set(&format!("/tree <{}>", target_id));
            }
            _ => {}
        }
    }

    /// Return a discriminant value to detect "kind changes" for sel reset.
    fn overlay_disc(&self) -> u8 {
        match self {
            ActiveOverlay::None => 0,
            ActiveOverlay::Help => 1,
            ActiveOverlay::CommandList { .. } => 2,
            ActiveOverlay::Model { .. } => 3,
            ActiveOverlay::Mod { .. } => 19,
            ActiveOverlay::Thinking { .. } => 4,
            ActiveOverlay::Settings { .. } => 5,
            ActiveOverlay::SettingsVal { .. } => 6,
            ActiveOverlay::Persona { .. } => 7,
            ActiveOverlay::Stat { .. } => 8,
            ActiveOverlay::ReplayHelp { .. } => 15,
            ActiveOverlay::Sandbox { .. } => 9,
            ActiveOverlay::PathComplete { .. } => 10,
            ActiveOverlay::Session { .. } => 11,
            ActiveOverlay::SessionAction { .. } => 12,
            ActiveOverlay::Confirm { .. } => 13,
            ActiveOverlay::Tree { .. } => 14,
            ActiveOverlay::TreeAction { .. } => 16,
            ActiveOverlay::SwitchAction { .. } => 17,
            ActiveOverlay::CompactSummary { .. } => 18,
        }
    }

    /// Helper: extract the filter string from SettingsVal for Enter/re-entry.
    fn filter_for(&self, editor: &LineEditor) -> String {
        // For SettingsVal the filter is embedded in the buffer after `/set <key> `.
        let buf = editor.text();
        if let Some((_prefix, rest)) = buf.split_once(' ') {
            if let Some((_key, val)) = rest.split_once(' ') {
                return val.to_string();
            }
        }
        String::new()
    }
}

impl Repl {
    pub fn new() -> Self {
        Self {
            guard: Arc::new(TermGuard::new()),
        }
    }

    /// Enter raw mode (idempotent). Best-effort; `Drop` still restores.
    pub fn start(&self) -> io::Result<()> {
        self.guard.start()
    }

    /// Shared handle to the terminal-mode guard. Used by `run_interactive` to
    /// drive suspend/resume (Ctrl-Z) and in-flight cancel from a background
    /// reader thread while an LLM request is in flight.
    pub(crate) fn guard(&self) -> Arc<TermGuard> {
        self.guard.clone()
    }

    /// Read a multi-line input in raw mode, with inline slash-command overlays.
    ///
    /// Returns `Some(InputAction::Submit(text))` with the submitted prompt,
    /// `Some(InputAction::SetModel(p, m))` / `SetThinking(level)` / `Quit` when
    /// the user accepts an overlay selection, or `None` only on a read error.
    /// `Ctrl-C` restores the terminal and exits the process.
    ///
    /// `history` backs `Up`/`Down` navigation and is persisted to disk.
    /// `Ctrl-C` restores the terminal and exits the process.
    ///
    /// As the user types a command (`/model`, `/thinking`, `/help`, `/quit`),
    /// an inline overlay opens in the input region and live-filters as they
    /// type. The prompt stays in the same position; overlays are dismissed with
    /// Esc (or any key for `/help`).
    /// The multi-line input loop. Slash commands (`/model`, `/thinking`,
    /// `/help`, `/quit`) open an **inline overlay** directly *above* the
    /// input line. The prompt stays in the same position; overlays are
    /// dismissed with `Esc` (or any key for `/help`). Any key typed into
    /// `/help` is passed through to the editor.
    ///
    /// The input line sits at `initial_row` — the natural cursor row tracked
    /// by the console `Output` — and is *not* forced to the screen
    /// bottom, so closing an overlay never leaves a gap in the scrollback.
    /// Overlays are drawn by **overwriting the rows just below the input line**
    /// (each cleared with `CSI K`); they never scroll, so once a dialog is
    /// dismissed there are no picker traces in the scrollback. All clearing is
    /// confined to the input row and below, so the greeter / status / divider /
    /// session content above the input is never touched (plan §3.7). The pop-up
    /// uses at most 2/3 of the terminal height (≥ 1 line).
    pub fn read_input(
        &mut self,
        prompt: &str,
        initial_buffer: Option<&str>,
        history: &mut History,
        ctx: &ReplCtx,
        settings: &mut SettingsStore,
        dirty: &mut bool,
        initial_row: usize,
        output: &dyn Output,
    ) -> Option<InputAction> {
        // Step 2 (Phase 6.7): engine providers come from shared live state.
        // Build a `MenuShared` handle bundle for this activation; it shares
        // the same SettingsStore the loop / run_interactive mutate, the
        // session repo, and the current-session/tree sync mirrors.
        let shared = MenuShared::new(
            settings.clone(),
            ctx.repo.clone(),
        );
        *shared.current_session_id.borrow_mut() = ctx.current_session_id.clone();
        *shared.recent_models.borrow_mut() = ctx.recent.clone();
        *shared.tree.borrow_mut() = Repl::flatten_all_tree(ctx);
        *shared.leaf_id.borrow_mut() = ctx.leaf_id.clone();
        let mut editor = LineEditor::new();
        // Seed the editor with a prefill string if one was provided. Used by
        // `/session <id> resume` to surface the resumed session's stored model
        // (`/model <provider>:<model_id>`) so the user can press Enter to
        // accept, Esc to dismiss, or type to choose a different model.
        // Pre-filling a slash command also opens its overlay immediately,
        // so the model picker is visible without any extra keystroke.
        if let Some(s) = initial_buffer {
            editor.set(s);
        }
        let mut nav: Option<usize> = None;
        let mut draft = String::new();
        let mut sel: usize = 0;
        let mut help_view = false;
        let mut value_error: Option<String> = None;
        let mut path_completions: Vec<String> = Vec::new();
        let mut completion_pos: Option<(usize, usize)> = None;
        let mut last_disc: u8 = 0; // overlay discriminant for sel-reset tracking
        let mut last_active: Option<usize> = None; // last frame's `active` row (see `cursor_sel_policy`)
        // Phase 6.7 step 3: ONE persistent engine per prompt. Cursors,
        // traversal frames, provider data cache, and accept phase all live
        // inside it; the host keeps the same instance across keypresses
        // (no rebuild, no take/restore, no host frames/cursors threading).
        let mut engine = slash_menu_bridge::MenuEngine::new(&shared);
        // D4 interstitial phase for /stat: first Enter shows the panel,
        // second Enter dismisses the menu entirely.
        let mut stat_phase = false;
        // The input line starts at `initial_row` — the natural cursor row tracked
        // by the console `Output` — and is *not* forced to the bottom of the
        // screen, so it never leaves a gap in the session scrollback. `input_row`
        // is the *current* visible row of the input and is updated every redraw to
        // follow the terminal's native scroll: when a tall pop-up pushes the input
        // up, `input_row` decreases and stays there as the pop-up later shrinks
        // (the terminal never scrolls back down on its own).
        let mut input_row = initial_row.max(1);

        // Height of the screen as of the last redraw. `redraw` uses it as the
        // lower bound for its clear, so a *grow* (maximize) that reveals
        // scrollback below the old bottom does not get erased. It is kept one
        // step behind the live size: the resize handling at the top of the loop
        // re-anchors `input_row` via a DSR query *before* redrawing, and only
        // advances `prev_term_h` to the new size *after* the redraw (see below).
        // Interior mutability (`Cell`) lets the `redraw` closure read it without
        // holding a mutable borrow across the loop.
        let prev_term_h = Cell::new(
            crossterm::terminal::size()
                .map(|(_, h)| h.max(1) as usize)
                .unwrap_or(24),
        );

        // Render the input line at `input_row` and the live overlay directly
        // *below* it. We clear the input + overlay rows (bounded at the previous
        // screen bottom on a grow, so a maximize's revealed scrollback survives)
        // and rewrite input + overlay; content *above* the input (greeter /
        // status / divider / session) is untouched. When the overlay is taller
        // than the space left, the terminal scrolls the overflow into its native
        // scrollback (session preserved); `input_row` is adjusted by the scroll
        // delta so the next redraw clears/writes at the right place (no
        // duplication). See plan §3.7.
        let redraw = |editor: &LineEditor, overlay: &[&str], sel: usize,
                      current_row: &mut usize| {
            let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
            let w = w as usize;
            let term_h = h.max(1) as usize;
            // Lower bound for the clear below: the *previous* screen bottom when
            // the terminal just grew, so a maximize that reveals scrollback
            // below the old bottom is not erased. `prev_term_h` is advanced only
            // after the redraw (see the loop), and `input_row` is re-anchored to
            // the real cursor before the redraw, so this is safe on both the
            // top- and bottom-anchored reflow models.
            let clear_to = prev_term_h.get().min(term_h);
            let row = (*current_row).max(1).min(term_h);
            let layout = editor.layout(w, prompt);
            let er = layout.row_count.max(1);
            // Overlay rows live BELOW the input line. Pop-up uses at most 2/3 of
            // the terminal height (≥ 1 line); the terminal scrolls on overflow.
            let max_pop = ((term_h as f64 * 2.0 / 3.0).ceil() as usize).max(1);
            let n = overlay.len().min(max_pop);
            // Window the (possibly long) overlay around the selection.
            let win = Self::overlay_window(overlay.len(), n, sel);
            let mut s = String::new();
            // Position at the input block's top row, then clear the rows the
            // input + overlay will occupy — bounded at `clear_to` (the previous
            // screen bottom on a grow). A plain `\x1b[0J` would clear to the new
            // bottom and wipe the scrollback a maximized window just revealed.
            s.push_str(&format!("\x1b[{};1H", row));
            s.push_str(&Self::clear_rows_to(row, clear_to));
            s.push_str(&format!("\x1b[{};1H", row));
            s.push_str(&render_content(&layout, prompt));
            s.push_str(&cursor_positioning(&layout, prompt));
            if n > 0 {
                // Move *below* the input block using a relative down-move. An absolute
                // address would be out of range when the input sits on the bottom
                // row (and get silently ignored, overwriting the input); a relative
                // move scrolls the screen instead, which is the desired behavior.
                // `below` is the number of rows from the (caret) cursor row to just
                // past the last input row.
                let below = (er as isize - layout.cursor_row as isize).max(0) as usize;
                let divider = format!("\x1b[2m{}\x1b[0m", "─".repeat(w));
                // Step below the input block with line feeds rather than a
                // relative cursor-down (`CSI <below> B`). `CSI B` only *clamps*
                // the cursor to the bottom margin and never scrolls, so when the
                // input sits on the bottom row the divider was emitted on the
                // input's own row, overwriting it (see bug report: two stacked
                // dividers, no input line). A bare `\n` is translated to `\r\n`
                // by the OPOST/ONLCR re-enabled in `TermGuard::start`, so it both
                // returns to column 0 and *scrolls* when the cursor is on the
                // bottom row — exactly the move we need. Emitting `below` of
                // them lands just past the last input row and makes the real
                // scroll count match `next_input_row`'s `er + n` assumption.
                for _ in 0..below {
                    s.push('\n');
                }
                s.push('\r');
                s.push_str(&divider);
                s.push('\n');
                for i in 0..n {
                    s.push_str(overlay[win + i]);
                    if i + 1 < n {
                        s.push('\n');
                    }
                }
                // Reposition the cursor back on the input line after drawing
                // the overlay below it.  The cursor was placed at
                // `(row + cursor_row, cursor_col)` by `cursor_positioning`
                // above; the overlay moved it down as it drew.  Rather than
                // a relative move that breaks when the terminal scrolls,
                // compute the *visual* row after the scroll and use an
                // absolute position.
                //
                // The cursor after the overlay is at viewport row
                // `row + er + n` (er = total input rows, n = overlay count).
                // Anything past `term_h` causes the terminal to scroll,
                // shifting the input up by `delta = max(0, (row + er + n) - term_h)`.
                let delta = (row + er + n).saturating_sub(term_h);
                let visual_row = (row + layout.cursor_row).saturating_sub(delta).max(1);
                s.push_str(&format!("\x1b[{};{}H", visual_row, layout.cursor_col + 1));
            }
            let _ = io::stdout().write_all(s.as_bytes());
            let _ = io::stdout().flush();
            // Follow the terminal's native scroll: if this write pushed the input
            // up, remember the new visible row so the next redraw clears/writes
            // at the correct place (prevents the duplication seen when a tall
            // pop-up shrinks but the screen has not scrolled back down).
            *current_row = Self::next_input_row(row, term_h, er, n);
        };
        loop {
            // On resume from Ctrl-Z (editing phase; the in-flight reader thread
            // is not running then), re-enter raw mode so the next keystroke is
            // read correctly. Idempotent; a no-op on every normal iteration.
            if RESUMED.swap(false, Ordering::SeqCst) {
                let _ = self.guard.start();
            }
            // Re-anchor `input_row` to the real cursor *before* we redraw, so no
            // redraw ever runs with a stale `input_row` against a changed size.
            // `crossterm::terminal::size()` reports the new size the instant the
            // kernel resizes — typically *before* the `Resize` event is consumed
            // below — so a redraw driven off that event would otherwise clear
            // from the stale row down to the new bottom and wipe the scrollback a
            // maximize just revealed. `prev_term_h` stays one step behind (it is
            // advanced only after the redraw), so `redraw` uses it as the clear
            // boundary and the revealed rows survive.
            let cur_h = crossterm::terminal::size()
                .map(|(_, h)| h.max(1) as usize)
                .unwrap_or(24);
            if cur_h != prev_term_h.get() {
                input_row = output.cursor_row();
            }
            // --- Compute the overlay from the current buffer. ---
            let overlay = ActiveOverlay::from_buffer_cursors(
                &editor.text(), ctx, settings, help_view, &path_completions,
                &mut engine,
            );
            // Hoisted before the sel-reset so `active` (the menu's
            // default/current-state row) can drive the cursor policy below.
            let (title, raw_opts, active, wrap) = overlay.display_rows();
            let cur_disc = overlay.overlay_disc();
            // Buffer-derived option sub-menus expose `active` as the row the
            // typed prompt spells out (e.g. `/tree <id> compact llm <payload>`
            // pre-selects `llm`; `/tree <id> switch --summarize` pre-selects
            // `summarize`). When such a menu opens, or the buffer re-derives a
            // different `active`, the cursor follows it so Enter acts on the
            // canonical line the prompt names — otherwise restoring such a
            // prompt from history and pressing Enter would commit the default
            // first row (e.g. `truncate`) instead. Arrow navigation is
            // preserved: arrows change only `sel`, never `active`, so a stable
            // `active` leaves the cursor where the user put it.
            let follows_active = matches!(
                overlay,
                ActiveOverlay::TreeAction { .. }
                    | ActiveOverlay::SwitchAction { .. }
                    | ActiveOverlay::CompactSummary { .. }
            );
            // The /tree list seeds its cursor on the current position (the
            // leaf row) when it opens, but does NOT follow later filter
            // re-derivations (that would yank the cursor as the user types a
            // search term). The action sub-menus seed AND follow `active`.
            let is_tree_list = matches!(overlay, ActiveOverlay::Tree { .. });
            if overlay.cursor_row().is_some() {
                // Engine-owned cursor: `sel` IS the engine's cursor row
                // (arrows mutate engine state; Tab/Enter act on it). The
                // marker `active` (ctx-driven `*`) is separate. No
                // host-side cursor policy for these overlays.
                sel = overlay
                    .cursor_row()
                    .unwrap_or(active.unwrap_or(0))
                    .min(raw_opts.len().saturating_sub(1));
                last_active = active;
                if cur_disc != last_disc { last_disc = cur_disc; }
            } else {
                let (new_sel, new_last_active) = Self::cursor_sel_policy(
                    active,
                    cur_disc != last_disc,
                    is_tree_list || follows_active,
                    follows_active,
                    last_active,
                    sel,
                );
                sel = new_sel;
                last_active = new_last_active;
            }
            if cur_disc != last_disc {
                last_disc = cur_disc;
            }
            let title = title.to_string();
            let opts = raw_opts;
            let max_sel = opts.len().saturating_sub(1);
            if sel > max_sel {
                sel = max_sel;
            }
            // Build the dynamic title for overlays that need it.
            let dyn_title = overlay.title();
            let effective_title = if dyn_title.is_empty() { title.as_str() } else { &dyn_title };
            let mut overlay_text = match &overlay {
                ActiveOverlay::None => String::new(),
                ActiveOverlay::Help => Self::render_help_overlay(),
                ActiveOverlay::Stat { .. } | ActiveOverlay::ReplayHelp { .. } => Self::overlay_lines(effective_title, &opts, None, usize::MAX, None, true),
                ActiveOverlay::SessionAction { header, .. } => {
                    Self::overlay_lines(effective_title, &opts, Some(header.as_slice()), sel, active, wrap)
                }
                ActiveOverlay::TreeAction { header, .. }
                | ActiveOverlay::SwitchAction { header, .. }
                | ActiveOverlay::CompactSummary { header, .. } => {
                    Self::overlay_lines(effective_title, &opts, Some(header.as_slice()), sel, active, wrap)
                }
                ActiveOverlay::Confirm { header, .. } => {
                    Self::overlay_lines(effective_title, &opts, Some(header.as_slice()), sel, active, wrap)
                }
                ActiveOverlay::Tree { row_bgs, .. } => {
                    // Tree carries per-row background tints (see
                    // `tree_overlay_entries`); build a parallel vec of
                    // Option<&str> for `overlay_lines_with_bgs`.
                    let row_bg_view: Vec<Option<&str>> = row_bgs
                        .iter()
                        .map(|b| b.map(|s| -> &str { s }))
                        .collect();
                    Self::overlay_lines_with_bgs(
                        effective_title,
                        &opts,
                        None,
                        sel,
                        active,
                        wrap,
                        Some(&row_bg_view),
                    )
                }
                ActiveOverlay::CommandList { .. } => {
                    // No `*` marker on the root command list (no current
                    // state there) — the engine active only seeds `sel`.
                    Self::overlay_lines(effective_title, &opts, None, sel, None, wrap)
                }
                _ => Self::overlay_lines(effective_title, &opts, None, sel, active, wrap),
            };
            // Value-mode settings error: red header line.
            if matches!(overlay, ActiveOverlay::SettingsVal { .. }) && value_error.is_some() {
                let err = format!(
                    "\x1b[48;5;235m\x1b[31merror: {}\x1b[0m",
                    value_error.as_ref().unwrap()
                );
                overlay_text = format!("{}\n{}", err, overlay_text);
            }
            let overlay_vec: Vec<&str> = overlay_text.lines().collect();
            let focus = {
                let sc = sel.min(opts.len());
                let base = if matches!(overlay, ActiveOverlay::SettingsVal { .. }) && value_error.is_some() {
                    2
                } else {
                    1
                };
                base + opts[..sc].iter().map(|o| o.matches('\n').count() + 1).sum::<usize>()
            };
            redraw(&editor, &overlay_vec, focus, &mut input_row);
            prev_term_h.set(cur_h);

            // --- Read and dispatch one key. ---
            match event::read() {
                Ok(Event::Key(key)) => {
                    // Any key event starts a fresh interaction: drop a stale
                    // value-mode validation error. The Enter handler re-sets it
                    // if the new value is still invalid, so the red header only
                    // persists while the user is re-submitting the same bad input.
                    value_error = None;
                    // Ctrl-C: restore terminal, hard exit.
                    if let KeyCode::Char('c') = key.code {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            let _ = self.guard.stop();
                            std::process::exit(1);
                        }
                    }
                    // Ctrl-Z: suspend the process group to the shell (SIGTSTP).
                    // On resume (fg), the `RESUMED` check at the top of the loop
                    // re-enters raw mode. Raw mode clears `ISIG`, so this must be
                    // handled as a key event rather than relying on the kernel.
                    if let KeyCode::Char('z') = key.code {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            let _ = self.guard.stop();
                            unsafe {
                                libc::killpg(libc::getpgrp(), libc::SIGTSTP);
                            }
                            // `killpg(SIGTSTP)` stops the process group; execution
                            // resumes here (in the main thread, a safe context)
                            // once the shell sends `SIGCONT`. Re-enter raw mode so
                            // the next keystroke is read correctly.
                            let _ = self.guard.start();
                        }
                    }
                    // Ctrl-E: external editor (only when no overlay is active).
                    if let KeyCode::Char('e') = key.code {
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(overlay, ActiveOverlay::None)
                        {
                            let _ = self.guard.stop();
                            match edit_in_editor(&editor.text()) {
                                Ok(new_text) => {
                                    editor.set(&new_text);
                                }
                                Err(e) => {
                                    let _ = io::stdout().write_all(
                                        format!("\r\neditor error: {}\r\n", e).as_bytes()
                                    );
                                }
                            }
                            let _ = self.guard.start();
                            input_row = output.cursor_row();
                            if let Ok((_, h)) = crossterm::terminal::size() {
                                prev_term_h.set(h.max(1) as usize);
                            }
                            continue;
                        }
                    }
                    if matches!(overlay, ActiveOverlay::Help) {
                        // Any key dismisses /help and is passed through.
                        if let KeyCode::Char(c) = key.code {
                            if !key.modifiers.contains(KeyModifiers::CONTROL)
                                && !key.modifiers.contains(KeyModifiers::ALT)
                            {
                                editor.insert(c);
                            }
                        }
                        help_view = false;
                        continue;
                    }
                    // Active overlay key dispatch via enum methods.
                    if !matches!(overlay, ActiveOverlay::None) {
                        match key.code {
                            KeyCode::Down | KeyCode::Up => {
                                // Phase 6.5: engine-backed overlays route
                                // arrows through the engine's cursor; the
                                // next frame's `sel` re-derives from it.
                                if overlay.cursor_row().is_some() {
                                    let delta = if key.code == KeyCode::Down { 1 } else { -1 };
                                    overlay.engine_arrow(&editor, &mut engine, delta);
                                    continue;
                                }
                                if key.code == KeyCode::Down {
                                    if sel + 1 < opts.len() { sel += 1; }
                                } else {
                                    if sel > 0 { sel -= 1; }
                                }
                            }
                            KeyCode::PageDown | KeyCode::PageUp => {
                                // PageUp/PageDown in the tree action sub-menus
                                // navigate to the previous/next entry in the tree
                                // view rather than scrolling the action menu.
                                let (prev_id, next_id) = match &overlay {
                                    ActiveOverlay::TreeAction { prev_id, next_id, .. } =>
                                        (prev_id.clone(), next_id.clone()),
                                    ActiveOverlay::SwitchAction { prev_id, next_id, .. } =>
                                        (prev_id.clone(), next_id.clone()),
                                    ActiveOverlay::CompactSummary { prev_id, next_id, .. } =>
                                        (prev_id.clone(), next_id.clone()),
                                    _ => (None, None),
                                };
                                let new_id = match key.code {
                                    KeyCode::PageUp => prev_id,
                                    _ => next_id, // PageDown
                                };
                                if let Some(id) = new_id {
                                    // Strip any typed action/filter suffix so we
                                    // land on the new entry's action menu.
                                    let base = format!("/tree <{}>", id);
                                    let buf = editor.text();
                                    // `after` holds the trailing token(s) from the
                                    // original prompt (`switch`, `compact llm <text>`
                                    // etc.).  Preserve them on the new prompt so the
                                    // user can PageUp/Down through entries while
                                    // keeping the same mode active.
                                    let after = buf
                                        .strip_prefix("/tree <")
                                        .and_then(|s| s.find('>').map(|i| &s[i + 1..]))
                                        .unwrap_or("")
                                        .trim();
                                    let new_line = if after.is_empty() {
                                        base.clone()
                                    } else {
                                        format!("{} {}", base, after)
                                    };
                                    editor.set(&new_line);
                                }
                                // else: no prev/next entry; bell is fine (or fall
                                // through to the generic page-scroll as a fallback).
                                let page = {
                                    let (_, h) = crossterm::terminal::size().unwrap_or((80, 24));
                                    (h as usize).saturating_sub(2).max(1)
                                };
                                if matches!(key.code, KeyCode::PageDown) {
                                    sel = (sel + page).min(opts.len().saturating_sub(1));
                                } else {
                                    sel = sel.saturating_sub(page);
                                }
                                // Phase 6.5: keep the engine cursor in sync
                                // with host paging on engine-backed overlays
                                // (they have no tree prev/next to navigate).
                                if overlay.cursor_row().is_some() {
                                    overlay.engine_seed(&editor, &mut engine, sel);
                                }
                            }
                            KeyCode::Tab => {
                                overlay.handle_tab(sel, &mut editor,
                                    &mut path_completions, &mut completion_pos,
                                    settings, &mut engine);
                            }
                            KeyCode::Enter => {
                                if !key.modifiers.contains(KeyModifiers::SHIFT) {
                                    // D4: /stat second Enter dismisses the
                                    // menu (interstitial with no leaf).
                                    if matches!(overlay, ActiveOverlay::Stat { .. }) {
                                        if stat_phase {
                                            editor.set("");
                                            stat_phase = false;
                                            continue;
                                        }
                                        stat_phase = true;
                                    } else {
                                        stat_phase = false;
                                    }
                                    match overlay.handle_enter(
                                        sel, &mut editor, output, &redraw, &mut input_row,
                                        settings, dirty, &mut value_error, &mut help_view,
                                        ctx, &mut engine,
                                    ) {
                                        OverlayAction::Continue => {}
                                        OverlayAction::Action(action) => return Some(action),
                                    }
                                }
                            }
                            KeyCode::Esc => {
                                // Phase 6.6 item 3: engine-backed overlays
                                // route Esc through the engine's per-level
                                // ladder (frame replay + esc()); legacy
                                // overlays keep their bespoke rewrites until
                                // they migrate.
                                if !overlay.engine_esc(&mut editor, &mut engine) {
                                    overlay.handle_esc(&mut editor,
                                        &mut path_completions, &mut completion_pos);
                                }
                            }
                            _ => {
                                // Editing key while path-completion overlay is active
                                // dismisses it (user is typing something else).
                                if matches!(overlay, ActiveOverlay::PathComplete { .. }) {
                                    path_completions.clear();
                                    completion_pos = None;
                                }
                                let prev_line = editor.text();
                                if let Some(a) = Self::edit_key(
                                    key, &mut editor, history, &mut nav, &mut draft, prompt)
                                {
                                    if let InputAction::Submit(_) = &a {
                                        redraw(&LineEditor::new(), &[], sel, &mut input_row);
                                    }
                                    return Some(a);
                                }
                                // Content edit on a v2 menu line: scoped
                                // traversal-memory invalidation. The engine
                                // compares the edited line's level segment;
                                // removing only a trailing space is
                                // navigation (parent menu), not an edit.
                                let now = editor.text();
                                if now.starts_with('/') || prev_line.starts_with('/') {
                                    let _ = engine.on_edit(&prev_line, &now);
                                }
                            }
                        }
                        continue;
                    }
                    // No overlay: normal editing.
                    // Tab: file-path completion on the token at the cursor.
                    if let KeyCode::Tab = key.code {
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT)
                        {
                            let buf = editor.text();
                            let cursor = editor.cursor;
                            let buf_chars: Vec<char> = buf.chars().collect();
                            let (tok_start, tok_end, token) =
                                Self::token_at_cursor(&buf_chars, cursor);
                            if !token.is_empty() {
                                // Complete using sandbox-aware listing when
                                // the sandbox is active; falls back to the
                                // host filesystem otherwise.
                                let completions = Self::complete_path_prefix_sandbox(&token);
                                if completions.len() == 1 {
                                    editor.replace_range(tok_start, tok_end, &completions[0]);
                                } else if !completions.is_empty() {
                                    path_completions = completions;
                                    completion_pos = Some((tok_start, tok_end));
                                    sel = 0;
                                }
                            }
                            continue;
                        }
                    }
                    if let Some(a) =
                        Self::edit_key(key, &mut editor, history, &mut nav, &mut draft, prompt)
                    {
                        if let InputAction::Submit(_) = &a {
                            redraw(&LineEditor::new(), &[], sel, &mut input_row);
                        }
                        return Some(a);
                    }
                }
                Ok(Event::Paste(text)) => {
                    editor.insert_str(&text);
                }
                Ok(Event::Resize(_, _)) => {
                    // Belt-and-suspenders re-anchor: the terminal changed size,
                    // so `input_row` may now sit above the real input line. The
                    // primary re-anchor already happens *before* the redraw at
                    // the top of the loop (so a redraw never runs with a stale
                    // `input_row` against the new size), but a `Resize` that
                    // arrives after the top-of-loop check still benefits from a
                    // DSR-driven refresh here. `redraw` clears only down to the
                    // previous screen bottom (`prev_term_h`), so the scrollback
                    // a maximized window just revealed is never clobbered.
                    input_row = output.cursor_row();
                }
                Ok(_) => {} // other events (mouse): ignored
                Err(_) => return None,
            }
        }
    }


    /// Compute the input row for the *next* redraw after emitting `er` input rows
    /// plus an `n`-line pop-up, given the current input row and terminal height.
    /// When the pop-up overflows the bottom, the terminal scrolls up by the
    /// overflow (`delta`), shifting the input up; it never scrolls back down on
    /// its own, so a shrinking pop-up leaves the input where the tallest pop-up
    /// put it. Pure + unit-tested (see `tests`).
    fn next_input_row(start: usize, term_h: usize, er: usize, n: usize) -> usize {
        let end_row = if n > 0 { start + er + n } else { start + er - 1 };
        let delta = end_row.saturating_sub(term_h);
        if delta > 0 {
            start.saturating_sub(delta).max(1)
        } else {
            start
        }
    }
    /// Build the escape sequence that clears from `row` down to `clear_to`
    /// (inclusive), erasing each line to its end with `\x1b[K`, without
    /// touching anything above `row` or below `clear_to`. Returns an empty
    /// string when `clear_to` is above `row` (nothing to clear).
    ///
    /// Used by `read_input`'s redraw so a terminal *grow* (maximize) clears
    /// only up to the previous screen bottom and never wipes the scrollback a
    /// maximized window reveals below it — a plain `\x1b[0J` would clear to
    /// the new bottom and destroy that history. Extracted as a free function
    /// (no TTY needed) so the boundary logic is unit-testable.
    fn clear_rows_to(row: usize, clear_to: usize) -> String {
        if clear_to < row {
            return String::new();
        }
        let mut s = String::new();
        for i in 0..(clear_to - row + 1) {
            s.push_str("\x1b[K");
            if i + 1 < clear_to - row + 1 {
                s.push_str("\x1b[1E");
            }
        }
        s
    }
    /// `avail` rows (≤ `overlay_len`), centered on `sel`. Returns the
    /// start index into the full overlay. Extracted so it can be unit-tested
    /// without a TTY.
    pub fn overlay_window(overlay_len: usize, avail: usize, sel: usize) -> usize {
        if overlay_len <= avail || avail == 0 {
            0
        } else {
            sel.saturating_sub(avail / 2).min(overlay_len - avail)
        }
    }

    /// Cursor policy for buffer-derived option menus (`read_input`).
    ///
    /// `active` is the overlay's current-state / default row as re-derived
    /// from the typed buffer on every keystroke. Two cases seed the cursor
    /// from `active`:
    ///
    /// - **On open** (`disc_changed`): a freshly-opened menu whose default
    ///   is meaningful (the /tree list seeding on the current leaf row, or a
    ///   tree action sub-menu whose `active` names the mode the prompt
    ///   already spells out, e.g. `/tree <id> compact llm <payload>` →
    ///   `llm`) starts the cursor on that row. Without this, restoring such a
    ///   prompt from history leaves the cursor on row 0 and Enter commits
    ///   the wrong option.
    /// - **On re-derivation** (`follow_changes`): when the buffer changes
    ///   within the same menu and `active` moves to a different row (the
    ///   user typed `switch --summarize` or `compact llm …`), the cursor
    ///   follows it.
    ///
    /// Arrow navigation stays intact: arrows modify only `sel`, never the
    /// buffer, so `active` is unchanged on the next frame and the cursor is
    /// left where the user put it.
    ///
    /// `seed_on_open` and `follow_changes` are separate because the /tree
    /// *list* seeds on the leaf row when it opens but must not yank the
    /// cursor as the user types a fuzzy search term, whereas the tree action
    /// sub-menus seed and follow.
    fn cursor_sel_policy(
        active: Option<usize>,
        disc_changed: bool,
        seed_on_open: bool,
        follow_changes: bool,
        last_active: Option<usize>,
        sel: usize,
    ) -> (usize, Option<usize>) {
        let new_sel = if disc_changed {
            if seed_on_open {
                active.unwrap_or(0)
            } else {
                0
            }
        } else if follow_changes && active != last_active {
            // The typed buffer re-derived a different default row; follow it.
            active.unwrap_or(sel)
        } else {
            sel
        };
        // Track the frame's `active` regardless (only consulted when
        // `follow_changes` is true on a later frame).
        (new_sel, active)
    }

    /// Echo a committed-invocation `line` to the transcript (a record of the
    /// change), draw a separator, then begin the next input prompt pre-filled
    /// with `next_input` (empty string for a leaf command). This unifies the
    /// `/model`, `/thinking`, and `/set <key> <value>` commits so each
    /// leaves exactly one separator directly below the emitted line — the next
    /// prompt's own divider (drawn below its input) no longer has to do that
    /// job, which previously left `/set` value commits looking
    /// separator-less because their next prompt (`/set` list) is visibly
    /// different from the emitted line. `redraw` is the live overlay closure
    /// from `read_input`; `input_row` is kept in sync so the next loop
    /// iteration redraws at the correct row.
    ///
    /// `sep` controls whether this helper draws the separator itself:
    /// `/set` value commits *stay in the `read_input` loop* (they revert
    /// to the list rather than returning an action), so no other separator
    /// will follow the emitted line — pass `true`. `/model` and `/thinking`
    /// *return* an `InputAction`, so `agent.rs` draws a separator before the
    /// next `read_input` (its per-prompt framing); passing `false` here avoids
    /// a doubled separator.
    fn emit_commit<F: Fn(&LineEditor, &[&str], usize, &mut usize)>(
        output: &dyn Output,
        editor: &mut LineEditor,
        redraw: &F,
        sel: usize,
        input_row: &mut usize,
        line: &str,
        next_input: &str,
        sep: bool,
    ) {
        redraw(&LineEditor::new(), &[], sel, input_row);
        output.display_plain(line);
        if sep {
            output.display_separator();
        }
        *input_row = output.cursor_row();
        editor.set(next_input);
    }

    /// Render the `/model` / `/thinking` / `/set` overlay: a panel title
    /// line plus option rows with a `> ` selection marker and (for `/model`)
    /// a green `*` on the currently-active model. Returns plain text; the
    /// caller emits it directly above the input line.
    ///
    /// `wrap_opts` controls layout of the option rows: pre-formatted rows
    /// (model/settings/thinking tables) must pass `false` so the inter-column
    /// padding spaces are preserved (word-wrapping would collapse them and
    /// destroy column alignment); prose rows (help) pass `true`.
    pub fn overlay_lines(
        title: &str,
        options: &[String],
        header: Option<&[String]>,
        sel: usize,
        active: Option<usize>,
        wrap_opts: bool,
    ) -> String {
        Self::overlay_lines_with_bgs(title, options, header, sel, active, wrap_opts, None)
    }

    /// Like [`overlay_lines`], but accepts a `row_bgs` array that overrides
    /// the default panel BG on a per-option basis. Length must match
    /// `options` (entries past the end fall back to the default BG; a
    /// `None` entry also falls back). The cursor-selected row always uses
    /// BG_SEL regardless of `row_bgs` so the selection stays distinct.
    pub fn overlay_lines_with_bgs(
        title: &str,
        options: &[String],
        header: Option<&[String]>,
        sel: usize,
        active: Option<usize>,
        wrap_opts: bool,
        row_bgs: Option<&[Option<&str>]>,
    ) -> String {
        const BG: &str = "\x1b[48;5;235m"; // panel background (dark)
        const BG_SEL: &str = "\x1b[48;5;239m"; // selected row background
        const C_SEL: &str = "\x1b[1;36m"; // bold cyan: cursor selection
        const C_RESET: &str = "\x1b[0m";
        const BG_HEADER: &str = "\x1b[48;5;237m"; // slightly lighter for header
        let tw = term_width().max(1);
        let opt_w = tw.saturating_sub(2); // "> " / "* " / "  " marker
        let mut rows: Vec<(usize, String, bool)> = Vec::new();
        for (i, o) in options.iter().enumerate() {
            // Prose overlays (help, command list) word-wrap each option.
            // Pre-formatted overlays (model/settings/thinking tables) are already
            // width-bounded and may carry their own `\n` (multi-line rows from
            // `Table::render_multiline`); preserve newlines instead of
            // re-wrapping, so column alignment survives across the wrapped
            // lines of one logical option.
            let wrapped: Vec<String> = if wrap_opts {
                wrap(o, opt_w)
            } else {
                o.split('\n').map(String::from).collect()
            };
            for (j, line) in wrapped.into_iter().enumerate() {
                rows.push((i, line, j == 0));
            }
        }
        let bg_line = |s: &str, bg: &str| -> String {
            format!("{}{:<width$}{}", bg, s, C_RESET, width = tw)
        };
        let mut out = String::new();
        if !title.is_empty() {
            out.push_str(&bg_line(title, BG));
        }
        // Render header rows (non-selectable info) if present.
        if let Some(hdr) = header {
            for hline in hdr {
                for subline in hline.split('\n') {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&format!("{}{:<width$}{}", BG_HEADER, subline, C_RESET, width = tw));
                }
            }
        }
        for (i, line, first) in &rows {
            // The 2-char marker area prefixes every line (including wrapped
            // continuation lines) so all visual lines of one option stay
            // column-aligned; only the first line shows a marker. The
            // marker carries two pieces of information, both piggy-backing
            // on the same 2-char slot:
            //   - cursor selection (`>`)
            //   - current-state row (active model, current session, leaf
            //     in `/tree`, ...) (`*`)
            // If the cursor is on the current-state row, `*` wins so the
            // user sees "this is where you already are" instead of a
            // generic selection arrow.
            let is_selected = *first && *i == sel;
            let is_active = *first && active == Some(*i);
            let marker = if is_selected && is_active {
                "* "
            } else if is_selected {
                "> "
            } else if is_active {
                "* "
            } else {
                "  "
            };
            let body = format!("{}{}", marker, line);
            // BG selection:
            //   - cursor row → BG_SEL (always wins so the cursor is
            //     distinct from any per-row tint).
            //   - row has an explicit `row_bgs[i]` → use that.
            //   - otherwise → default BG.
            // Active rows no longer get their own foreground colour — the
            // `*` marker is the indicator — so the table stays consistent
            // (overlays that need an inline active marker can prefix it
            // into the row string themselves).
            let bg = if *i == sel {
                BG_SEL
            } else {
                row_bgs
                    .and_then(|bgs| bgs.get(*i).and_then(|b| *b))
                    .unwrap_or(BG)
            };
            let fg = if *i == sel { C_SEL } else { "" };
            let content = if fg.is_empty() {
                body
            } else {
                format!("{}{}", fg, body)
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&bg_line(&content, bg));
        }
        out
    }

    /// Render the `/help` overlay (panel background, no selection). Any key
    /// dismisses it (see `read_input`).
    fn render_help_overlay() -> String {
        let lines: Vec<String> = slash::render_help().lines().map(String::from).collect();
        Self::overlay_lines("Help  (any key closes)", &lines, None, usize::MAX, None, true)
    }

    /// Resolve the catalog id of the currently-active model. OpenRouter routes
    /// models under their *original* provider namespace (`tencent/hy3:free`),
    /// while `ctx.model` is usually the short name (`hy3:free`) and `ctx.provider`
    /// is `openrouter` — so a naïve `"{provider}/{model}"` id never matches.
    fn active_entry_id(entries: &[model_catalog::ModelEntry], provider: &str, model: &str) -> String {
        let p = provider.to_lowercase();
        let m = model.to_lowercase();
        for e in entries {
            let id = e.id.to_lowercase();
            if id == format!("{}:{}", p, m) {
                return e.id.clone();
            }
            if id == m {
                return e.id.clone();
            }
            // OpenRouter passthrough (`tencent/hy3:free`) and native
            // (`openrouter:tencent/hy3:free`) both end in the short model name.
            if p == "openrouter" && id.ends_with(&format!("/{}", m)) {
                return e.id.clone();
            }
        }
        format!("{}:{}", p, m)
    }


    /// Rank a model id for `/model` sorting: active first (0), then recently
    /// used (1, 2, ...), then everything else (u32::MAX).
    fn model_rank(id: &str, active_id: &str, recent: &[String]) -> u32 {
        if id == active_id {
            0
        } else if let Some(p) = recent.iter().position(|r| r == id) {
            1 + p as u32
        } else {
            u32::MAX
        }
    }

    /// Build the `/model` overlay rows for `filter`, sorted active-first then
    /// recently-used, fuzzy-filtered by `filter`. Returns the (sorted+filtered)
    /// entries and the position of the active model within them.

    /// Parse the model filter string for `.*` modifiers and search terms.
    /// Returns (search_terms, ModelFilter).
    ///
    /// Modifier tokens (`.sort:coding`, `.price:0.5`, …) are consumed via the
    /// data-driven [`MODEL_MODIFIERS`](crate::model_catalog::MODEL_MODIFIERS)
    /// table (`apply_modifier`), so adding a modifier there keeps the parser in
    /// sync automatically. An empty value after the colon (e.g. `.sort:`) clears
    /// that filter. Anything that isn't a known modifier token falls through as a
    /// fuzzy search term.
    fn parse_model_filter(input: &str) -> (String, ModelFilter) {
        let mut filter = ModelFilter::default();
        let mut search_parts = Vec::new();

        for token in input.split_whitespace() {
            if token.starts_with('.') {
                // `.name` (no colon → intro) or `.name:value` / `.name:`
                if let Some((name, val)) = token[1..].split_once(':') {
                    if crate::model_catalog::MODEL_MODIFIERS
                        .iter()
                        .any(|m| m.token == name)
                    {
                        Self::apply_modifier(&mut filter, name, val);
                        continue;
                    }
                }
            }
            search_parts.push(token);
        }

        (search_parts.join(" "), filter)
    }

    /// Apply one `.name:value` modifier to `filter`, using the data-driven
    /// `MODEL_MODIFIERS` table for the token's value kind. An empty `value`
    /// clears the field. Unknown modifier names are ignored (the exact-token
    /// match in `parse_model_filter` prevents them reaching here).
    fn apply_modifier(filter: &mut ModelFilter, name: &str, value: &str) {
        use std::str::FromStr;
        // Helper to apply a numeric modifier, clearing on empty value.
        fn num<T: FromStr>(filter: &mut ModelFilter, value: &str, set: impl Fn(&mut ModelFilter, Option<T>)) {
            if value.is_empty() {
                set(filter, None);
            } else if let Ok(v) = value.parse::<T>() {
                set(filter, Some(v));
            }
        }
        match name {
            "sort" => {
                filter.sort = if value.is_empty() { None } else { Some(value.to_string()) };
            }
            "price_input" => {
                if value.is_empty() {
                    filter.price_input = None;
                } else if value == "0" || value == "free" {
                    filter.price_input = Some(0.0);
                } else if let Ok(v) = value.parse::<f64>() {
                    filter.price_input = Some(v);
                }
            }
            "price_output" => num(filter, value, |f, v| f.price_output = v),
            "price" => num(filter, value, |f, v| f.price = v),
            "coding" => num(filter, value, |f, v| f.min_coding_index = v),
            "intelligence" => num(filter, value, |f, v| f.min_intelligence_index = v),
            "agentic" => num(filter, value, |f, v| f.min_agentic_index = v),
            "max_age_days" => num(filter, value, |f, v| f.max_age_days = v),
            "context" => num(filter, value, |f, v| f.min_context = v),
            _ => {}
        }
    }

    /// Parse the `/settings` argument string into (setting, value query, is_value_mode).
    ///
    /// Value mode opens only when the first token EXACTLY matches a setting
    /// name (case-insensitive). A partial prefix (e.g. `d`, `def`, `sec`) stays
    /// in list mode — the user types more or Tab-completes to the full name,
    /// which is what actually opens the sub-menu.
    fn parse_settings(rest: &str) -> (Option<SettingKey>, String, bool) {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.is_empty() {
            return (None, String::new(), false);
        }
        let exact = SETTINGS.iter().find(|m| m.name.eq_ignore_ascii_case(parts[0]));
        if let Some(m) = exact {
            let vq = if parts.len() >= 2 {
                parts[1..].join(" ")
            } else {
                String::new()
            };
            (Some(m.key), vq, true)
        } else {
            (None, rest.trim().to_string(), false)
        }
    }

    /// Candidate values for a setting's value-pick overlay.
    fn setting_value_candidates(key: SettingKey, _ctx: &ReplCtx, vq: Option<&str>) -> Vec<ValueCandidate> {
        match key {
            SettingKey::SecurityMode => vec![
                ValueCandidate { display: "confirm".into(), value: "confirm".into(), desc: "ask before each tool call".into() },
                ValueCandidate { display: "yolo".into(), value: "yolo".into(), desc: "run tools without prompts".into() },
            ],
            SettingKey::DefaultThinking => vec![
                ValueCandidate { display: "off".into(), value: "off".into(), desc: "no extra reasoning".into() },
                ValueCandidate { display: "low".into(), value: "low".into(), desc: "minimal reasoning".into() },
                ValueCandidate { display: "med".into(), value: "med".into(), desc: "balanced reasoning".into() },
                ValueCandidate { display: "high".into(), value: "high".into(), desc: "maximum reasoning".into() },
            ],
            SettingKey::DefaultPersona => {
                let mut v: Vec<ValueCandidate> = crate::persona::PERSONAS
                    .iter()
                    .map(|p| ValueCandidate {
                        display: p.name.into(),
                        value: p.name.into(),
                        desc: p.description.into(),
                    })
                    .collect();
                v.push(ValueCandidate { display: "none".into(), value: String::new(), desc: "no persona".into() });
                v
            }
            SettingKey::ShowReasoning => vec![
                ValueCandidate { display: "disabled".into(), value: "disabled".into(), desc: "show neither indicator nor reasoning text".into() },
                ValueCandidate { display: "indicator".into(), value: "indicator".into(), desc: "show only a Thinking… spinner".into() },
                ValueCandidate { display: "content".into(), value: "content".into(), desc: "show the reasoning text (default)".into() },
            ],
            SettingKey::ReasoningInContext => vec![
                ValueCandidate { display: "true".into(), value: "true".into(), desc: "keep reasoning blocks in the session context".into() },
                ValueCandidate { display: "false".into(), value: "false".into(), desc: "drop reasoning from context to save tokens (default)".into() },
            ],
            SettingKey::SessionResumeReplayTurns => Vec::new(),
            SettingKey::LlmCompactPrompt => Vec::new(),
            SettingKey::ToolCallLimit => Vec::new(),
            SettingKey::DefaultModel => {
                let filter = vq.unwrap_or("").trim().to_string();
                let (entries, _active) = Self::model_selection_overlay_entries(_ctx, &filter, false);
                entries.iter().map(Self::model_candidate).collect()
            }
        }
    }

    /// Candidate values for `key`, fuzzy-filtered by `vq` (empty `vq` => all).
    fn filtered_value_candidates(
        key: SettingKey,
        ctx: &ReplCtx,
        vq: &str,
    ) -> Vec<ValueCandidate> {
        let cands = Self::setting_value_candidates(key, ctx, Some(vq));
        if vq.trim().is_empty() {
            cands
        } else if key == SettingKey::DefaultModel {
            // For default_model, parse filter/sort modifiers from vq
            let filter = vq.trim().to_string();
            let (entries, _) = Self::model_selection_overlay_entries(ctx, &filter, false);
            entries.iter().map(Self::model_candidate).collect()
        } else {
            crate::fuzzy::fuzzy_filter_bonus(&cands, vq, |c| c.display.clone())
                .into_iter()
                .map(|r| r.item.clone())
                .collect()
        }
    }

    /// Map a catalog `ModelEntry` to a `default_model` value candidate.
    /// Both `value` and `display` carry the `provider:model` form so the panel
    /// always shows — and stores — a fully-qualified model id.
    fn model_candidate(e: &model_catalog::ModelEntry) -> ValueCandidate {
        let q = e.qualified();
        ValueCandidate {
            display: q.clone(),
            value: q,
            desc: format!("ctx {} | coding {} | {}", model_catalog::ctx_cell(e), model_catalog::coding_index_cell(e), model_catalog::cost_cell(e)),
        }
    }

    /// Build the dotted-modifier completion rows for `filter`.
    ///
    /// `filter` is the buffer text after `/model `. The trailing token decides the
    /// phase:
    /// - a bare `.` (or partial `.x`) lists the modifier *names*;
    /// - `.name:` (or `.name:part`) for an enumerated modifier lists that
    ///   modifier's fixed *values* (filtered by `part`);
    /// - `.name:` for a free-form numeric modifier yields no overlay (returns
    ///   `(row, want_values=false)`) so the list overlay takes over for typing the
    ///   number.
    /// Returns `(rows, want_value)`: `want_value` tells the caller the user is
    /// picking among enumerated values for a single modifier.
    fn modifier_overlay_entries(filter: &str) -> (Vec<ValueCandidate>, bool) {
        use crate::model_catalog::MODEL_MODIFIERS;

        let last = filter.split_whitespace().last().unwrap_or("");
        let Some(inner) = last.strip_prefix('.') else {
            // No dotted token in the buffer — nothing to complete.
            return (Vec::new(), false);
        };

        let (name, has_colon) = match inner.split_once(':') {
            Some((n, _)) => (n, true),
            None => (inner, false),
        };

        if let Some(m) = MODEL_MODIFIERS.iter().find(|m| m.token == name && !name.is_empty()) {
            if let Some(_choices) = m.choices {
                // Enumerated modifier: if the user has typed a *complete* value
                // (`.sort:coding`), nothing more to complete — hand back to the
                // model list. A bare `.name:` or partial value shows the filtered
                // value set.
                let value_query = last.strip_prefix(&format!(".{}:", m.token)).unwrap_or("");
                if has_colon && !value_query.is_empty() {
                    let exact = m.choices.unwrap_or_default().iter()
                        .any(|c| *c == value_query);
                    if exact {
                        return (Vec::new(), false);
                    }
                }
                return (Self::modifier_value_rows(m, filter), true);
            }
            // Free-form numeric modifier. If the user has typed past the colon
            // (`.coding:` or `.coding:80`), hand control back to the model list
            // so they can type the number — nothing more to complete. Only a
            // bare `.coding` (no colon) shows the single name row as a hint.
            if !has_colon {
                let q = format!(".{}:", name);
                return (vec![ValueCandidate { display: q.clone(), value: q, desc: m.desc.into() }], false);
            }
            return (Vec::new(), false);
        }

        // Unknown/partial name or bare `.`: list modifier names, fuzzy-filtered.
        let base: Vec<ValueCandidate> = MODEL_MODIFIERS
            .iter()
            .map(|m| {
                let q = format!(".{}:", m.token);
                ValueCandidate { display: q.clone(), value: q, desc: m.desc.into() }
            })
            .collect();
        if name.is_empty() {
            (base, false)
        } else {
            let rows = crate::fuzzy::fuzzy_filter_bonus(&base, name, |c| c.display.clone())
                .into_iter()
                .map(|r| r.item.clone())
                .collect();
            (rows, false)
        }
    }

    /// Rows for the enumerated values of modifier `m`, filtered by the value
    /// text the user has typed after `.m:`.
    fn modifier_value_rows(m: &model_catalog::ModifierSpec, filter: &str) -> Vec<ValueCandidate> {
        let last = filter.split_whitespace().last().unwrap_or("");
        let prefix = format!(".{}:", m.token);
        let value_query = last.strip_prefix(&prefix).unwrap_or("").to_string();
        let base: Vec<ValueCandidate> = m
            .choices
            .unwrap_or_default()
            .iter()
            .map(|c| {
                let v = format!(".{}:{}", m.token, c);
                ValueCandidate { display: v.clone(), value: v, desc: m.desc.into() }
            })
            .collect();
        if value_query.trim().is_empty() {
            base
        } else {
            crate::fuzzy::fuzzy_filter_bonus(&base, &value_query, |c| c.display.clone())
                .into_iter()
                .map(|r| r.item.clone())
                .collect()
        }
    }

    /// Replace the last whitespace-delimited token of `s` with `repl`.
    fn replace_last_token(s: &str, repl: &str) -> String {
        if let Some(idx) = s.rfind(' ') {
            format!("{} {}", &s[..idx], repl)
        } else {
            repl.to_string()
        }
    }

    /// Find the whitespace-delimited token at or near `cursor` (char index
    /// into `buf`). Returns `(start_byte, end_byte, token_text)`.
    ///
    /// Strategy, in priority order:
    /// 1. Cursor at/beyond end-of-buffer → last whitespace-delimited token.
    /// 2. Cursor *on* a non-WS character → the word containing that position.
    /// 3. Cursor *on* whitespace → word *before* cursor (the token the user
    ///    just typed and wants to Tab-complete).  If none, empty.
    ///
    /// Returns `(0, 0, "")` when there is no word.
    fn token_at_cursor(buf: &[char], cursor: usize) -> (usize, usize, String) {
        let len = buf.len();
        if len == 0 || cursor == 0 {
            return (0, 0, String::new());
        }
        let pos = cursor.min(len);

        // 1. Past end or on trailing WS → last word.
        if pos >= len || (pos == len - 1 && buf[pos].is_whitespace()) {
            let mut end = len;
            while end > 0 && buf[end - 1].is_whitespace() {
                end -= 1;
            }
            if end == 0 {
                return (0, 0, String::new());
            }
            let mut start = end;
            while start > 0 && !buf[start - 1].is_whitespace() {
                start -= 1;
            }
            return (start, end, buf[start..end].iter().collect());
        }

        // 2. Cursor on a non-WS char → chars of that word *before* the cursor.
        if !buf[pos].is_whitespace() {
            let mut start = pos;
            while start > 0 && !buf[start - 1].is_whitespace() {
                start -= 1;
            }
            if start < cursor {
                return (start, cursor, buf[start..cursor].iter().collect());
            }
            // Cursor at word start with nothing typed → no completion.
            return (0, 0, String::new());
        }

        // 3. Cursor on whitespace → word *before* cursor (the token the user
        //    just typed and wants to complete).
        let mut start_before = pos;
        while start_before > 0 && !buf[start_before - 1].is_whitespace() {
            start_before -= 1;
        }
        if start_before < pos {
            return (start_before, pos, buf[start_before..pos].iter().collect());
        }
        // Nothing before cursor on this line → no completion.
        (0, 0, String::new())
    }

    /// Complete a file path prefix, preferring sandbox-aware listing when the
    /// sandbox is active.
    ///
    /// When the sandbox has explicit bindings, runs `ls -1p` *inside* the
    /// sandbox (via bwrap) so completions reflect exactly what the agent would
    /// see.  Falls back to the host filesystem only when no sandbox is
    /// configured at all; when the sandbox IS active but a directory doesn't
    /// exist inside it, returns empty so non-sandbox paths are never suggested.
    fn complete_path_prefix_sandbox(prefix: &str) -> Vec<String> {
        if prefix.is_empty() {
            return Vec::new();
        }

        // Detect `~` prefix so we can round-trip results back to `~`.
        let uses_tilde = prefix.starts_with('~');
        let expanded = expand_tilde(prefix);
        let path = std::path::Path::new(&expanded);

        let (dir, filter) = if expanded.ends_with('/') || path.is_dir() {
            let d = expanded.trim_end_matches('/');
            if d.is_empty() {
                ("/".to_string(), String::new())
            } else {
                (d.to_string(), String::new())
            }
        } else {
            let parent = path.parent().unwrap_or(std::path::Path::new("."));
            let stem = path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            (parent.to_string_lossy().to_string(), stem)
        };

        let sandbox_active = crate::tools::sandbox_config()
            .as_ref()
            .as_ref()
            .is_some();

        if let Some(mut entries) = crate::tools::sandbox_list_dir(&dir) {
            entries.retain(|name| name != "./" && name != "../");
            let home = if uses_tilde {
                let h = std::env::var("HOME").unwrap_or_default();
                if h.ends_with('/') { h } else { format!("{}/", h) }
            } else {
                String::new()
            };
            let mut results: Vec<String> = entries
                .into_iter()
                .filter(|name| name.starts_with(&filter))
                .map(|name| {
                    let full = format!("{}/{}", dir, name);
                    if uses_tilde {
                        if let Some(rest) = full.strip_prefix(&home) {
                            return format!("~/{}", rest);
                        }
                    }
                    full
                })
                .collect();
            results.sort();
            return results;
        }

        if sandbox_active {
            return Vec::new();
        }
        complete_path_prefix_impl(&expanded)
    }

    /// Build the line that replaces the current input when Tab-completing a
    /// slash command. Command `name`s already include the leading `/`
    /// (e.g. `"/help"`), so we must NOT prepend another — that would yield
    /// `//help`. A trailing space opens the argument slot (matching the
    /// Enter handler's `editor.set("/model ")`, etc.).
    #[allow(dead_code)] // legacy v1 path; kept until full v2 migration
    fn complete_command(c: &slash::SlashCommand) -> String {
        if c.name == "/session" {
            format!("/session path:. ")
        } else {
            format!("{} ", c.name)
        }
    }

    fn model_selection_overlay_entries(
        ctx: &ReplCtx,
        filter: &str,
        allow_recent_boost: bool,
    ) -> (Vec<model_catalog::ModelEntry>, Option<usize>) {
        // Parse filter for .category: .sort: etc. modifiers
        let (search_terms, mut model_filter) = Self::parse_model_filter(filter);
        
        // Merge with existing filter state (persist non-overridden values)
        if model_filter.sort.is_none() {
            model_filter.sort = ctx.model_filter.sort.clone();
        }
        if model_filter.price_input.is_none() {
            model_filter.price_input = ctx.model_filter.price_input;
        }
        if model_filter.price_output.is_none() {
            model_filter.price_output = ctx.model_filter.price_output;
        }
        if model_filter.min_coding_index.is_none() {
            model_filter.min_coding_index = ctx.model_filter.min_coding_index;
        }
        if model_filter.min_intelligence_index.is_none() {
            model_filter.min_intelligence_index = ctx.model_filter.min_intelligence_index;
        }
        if model_filter.min_agentic_index.is_none() {
            model_filter.min_agentic_index = ctx.model_filter.min_agentic_index;
        }
        if model_filter.max_age_days.is_none() {
            model_filter.max_age_days = ctx.model_filter.max_age_days;
        }
        if model_filter.min_context.is_none() {
            model_filter.min_context = ctx.model_filter.min_context;
        }
        
        // Update the search terms
        model_filter.search_terms = search_terms;
        
        // Check if filter params changed (excluding search_terms)
        let params_changed = model_filter.sort != ctx.model_filter.sort
            || model_filter.price_input != ctx.model_filter.price_input
            || model_filter.price_output != ctx.model_filter.price_output
            || model_filter.min_coding_index != ctx.model_filter.min_coding_index
            || model_filter.min_intelligence_index != ctx.model_filter.min_intelligence_index
            || model_filter.min_agentic_index != ctx.model_filter.min_agentic_index
            || model_filter.max_age_days != ctx.model_filter.max_age_days
            || model_filter.min_context != ctx.model_filter.min_context;
        
        // Fetch catalog with the filter params applied at the repository level
        let entries = model_catalog::get_filtered_catalog(&model_filter.to_params());
        
        // If filter params changed, trigger a background refresh
        if params_changed {
            tokio::spawn(async move {
                let _ = model_catalog::refresh_catalog().await;
            });
        }
        
        let active_id = Self::active_entry_id(&entries, &ctx.provider, &ctx.model);
        
        // Build server rank map (position in the filtered+sorted catalog)
        let server_rank: std::collections::HashMap<String, usize> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        
        let filtered: Vec<model_catalog::ModelEntry> = if model_filter.search_terms.trim().is_empty() && model_filter.sort.is_none() && allow_recent_boost {
            // No search terms AND no explicit sort AND allow recent boost: boost the active model + 5 most recently used models to the top
            // (in LRU order), then the rest in repository sort order.
            let mut seen = std::collections::HashSet::new();
            let mut recent_shown = Vec::new();
            
            // First, ensure the active model is at the very top if it exists in entries
            if let Some(active_entry) = entries.iter().find(|e| e.id == active_id) {
                if seen.insert(active_entry.id.clone()) {
                    recent_shown.push(active_entry.clone());
                }
            }
            
            // Then add up to 4 more recent models (for a total of 5)
            for recent_id in &ctx.recent {
                if recent_shown.len() >= 5 {
                    break;
                }
                if let Some(e) = entries.iter().find(|e| e.id == *recent_id) {
                    if seen.insert(e.id.clone()) {
                        recent_shown.push(e.clone());
                    }
                }
            }
            // Remaining entries in repository order (already sorted by repo)
            let mut rest: Vec<model_catalog::ModelEntry> = entries
                .into_iter()
                .filter(|e| !seen.contains(&e.id))
                .collect();
            rest.sort_by(|a, b| {
                let ra = server_rank.get(&a.id).copied().unwrap_or(usize::MAX);
                let rb = server_rank.get(&b.id).copied().unwrap_or(usize::MAX);
                ra.cmp(&rb)
            });
            recent_shown.extend(rest);
            recent_shown
        } else if model_filter.search_terms.trim().is_empty() {
            // No search terms, but explicit sort requested: use the catalog sort order directly
            entries
        } else {
            // With search terms: blend fuzzy match score with server rank
            let mut sorted: Vec<model_catalog::ModelEntry> = entries;
            sorted.sort_by(|a, b| {
                Self::model_rank(&a.id, &active_id, &ctx.recent)
                    .cmp(&Self::model_rank(&b.id, &active_id, &ctx.recent))
                    .then_with(|| a.display.cmp(&b.display))
            });
            let fuzzy_results = crate::fuzzy::fuzzy_filter(
                &sorted,
                &model_filter.search_terms,
                |e| format!("{} {} {} {}", e.provider, e.display, e.id, e.qualified()),
            );
            
            // Blend: combined_score = fuzzy_score * 100 - server_position
            // This prioritizes fuzzy matches but respects server popularity
            let mut ranked: Vec<(model_catalog::ModelEntry, i64)> = fuzzy_results
                .into_iter()
                .map(|r| {
                    let server_pos = server_rank.get(&r.item.id).copied().unwrap_or(usize::MAX) as i64;
                    let combined_score = r.score * 100 - server_pos;
                    (r.item.clone(), combined_score)
                })
                .collect();
            
            ranked.sort_by(|a, b| b.1.cmp(&a.1));
            ranked.into_iter().map(|(e, _)| e).collect()
        };
        
        let active = filtered.iter().position(|e| e.id == active_id);
        (filtered, active)
    }

    /// Build the `/session` overlay rows.
    ///
    /// Shows sessions filtered by `filter` (understands `path:/dir` prefix).
    /// Current session is marked via the `*` marker in the 2-char prefix
    /// area (see `overlay_lines`), so no separate ◉ column is needed.
    /// Each row shows: session name (with first-prompt preview) · dir · turns + relative age.
    #[allow(dead_code)] // legacy v1 path; kept until /session migrates
    fn session_overlay_entries(
        ctx: &ReplCtx,
        filter: &str,
        opts: &mut Vec<String>,
        active: &mut Option<usize>,
    ) {
        let (dir_filter, fuzzy_filter) = Self::parse_session_path_filter(filter);

        let avail = term_width().saturating_sub(2);
        let mut table = Table::new(&["Session", "Dir", ""])
            .with_aligns(&[Align::Left, Align::Left, Align::Right])
            .with_min_widths(&[20, 10, 10])
            .with_priorities(&[2, 2, 1])
            .set_gap(2);

        // Load sessions synchronously from disk.
        let cwd_filter = dir_filter.as_ref()
            .and_then(|d| if d.is_empty() { None } else { Some(std::path::Path::new(d)) });
        let all = if let Some(ref repo) = ctx.repo {
            repo.list_sessions_sync(cwd_filter)
        } else {
            Vec::new()
        };

        // Map to SessionMeta and sort.
        let mut metas: Vec<SessionMeta> = all.into_iter()
            .map(|m| SessionMeta {
                id: m.id,
                name: m.name,
                timestamp: m.timestamp,
                cwd: m.cwd,
                parent_session: m.parent_session.map(|p| p.display().to_string()),
                message_count: m.message_count,
                user_message_count: m.user_message_count,
                first_prompt_preview: m.first_prompt_preview,
                last_prompt_text: m.last_prompt_text,
                last_prompt_timestamp: m.last_prompt_timestamp,
                path: Some(m.path),
            })
            .collect();
        metas.sort_by(|a, b| {
            let a_cur = a.id == ctx.current_session_id;
            let b_cur = b.id == ctx.current_session_id;
            if a_cur && !b_cur { std::cmp::Ordering::Less }
            else if !a_cur && b_cur { std::cmp::Ordering::Greater }
            else {
                let a_ts = a.last_prompt_timestamp.as_ref().unwrap_or(&a.timestamp);
                let b_ts = b.last_prompt_timestamp.as_ref().unwrap_or(&b.timestamp);
                b_ts.cmp(a_ts)
            }
        });

        // Fuzzy-filter by name, prompt preview, id, and cwd.
        let filtered: Vec<&SessionMeta> = if fuzzy_filter.is_empty() {
            metas.iter().collect()
        } else {
            crate::fuzzy::fuzzy_filter_bonus(&metas, &fuzzy_filter, |m| {
                let name = m.name.as_deref().unwrap_or("");
                let preview = m.first_prompt_preview.as_deref().unwrap_or("");
                format!("{} {} {} {}", name, preview, m.id, m.cwd)
            })
            .iter()
            .map(|r| r.item)
            .collect()
        };

        if filtered.is_empty() {
            opts.push("(no sessions)".into());
            *active = None;
            return;
        }

        for meta in &filtered {
            let name_part = meta.name.clone().unwrap_or_default();
            let raw_preview = meta.first_prompt_preview.as_deref().unwrap_or("");
            let preview = Self::sanitise_single_line(raw_preview);
            let display_name = if !name_part.is_empty() && !preview.is_empty() {
                format!("{}: {}", name_part, preview)
            } else if !name_part.is_empty() {
                name_part
            } else if !preview.is_empty() {
                preview
            } else {
                "(no messages)".to_string()
            };
            let dir_display = meta.cwd.rsplit('/')
                .take(2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("/");
            let rel_ts = meta.last_prompt_timestamp.as_ref().unwrap_or(&meta.timestamp);
            let rel = Self::relative_time(rel_ts);
            let info = format!("{}  {}", meta.message_count, rel);
            table.add_row(&[&display_name, &dir_display, &info]);
        }

        for row in table.render(avail) {
            opts.push(row);
        }

        *active = filtered.iter().position(|m| m.id == ctx.current_session_id);
    }

    /// Format an ISO 8601 timestamp for display: "today 14:32", "yesterday 09:15",
    /// or "Jul 14 11:00".
    fn format_session_timestamp(ts: &str) -> String {
        // Parse "2025-01-01T14:32:00.000Z" → extract date components
        let date_part = &ts[..10]; // "2025-01-01"
        let time_part = if ts.len() >= 16 { &ts[11..16] } else { "??:??" }; // "14:32"

        // Compute relative day
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let yesterday = (chrono::Utc::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d").to_string();

        let day_label = if date_part == today {
            "today"
        } else if date_part == yesterday {
            "yesterday"
        } else {
            // Show "Jul 14" format
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

    /// Format a timestamp as a compact relative duration: "30s", "4m", "5h", "2d".
    #[allow(dead_code)] // legacy v1 path; kept until /session migrates
    fn relative_time(ts: &str) -> String {
        let dt = chrono::DateTime::parse_from_rfc3339(ts)
            .or_else(|_| {
                // Try UTC-only format: "2025-01-01T14:32:00.000Z" without offset
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

    /// Collapse a string to a single visual line: replace any whitespace-run
    /// (including newlines, tabs, etc.) with a single space, so it fits cleanly
    /// in a one-line table cell.
    fn sanitise_single_line(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut prev_space = false;
        for c in s.chars() {
            if c.is_whitespace() {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            } else {
                out.push(c);
                prev_space = false;
            }
        }
        // Trim leading/trailing whitespace that was collapsed.
        let trimmed = out.trim().to_string();
        if trimmed.is_empty() { String::new() } else { trimmed }
    }

    /// Build the per-session action menu overlay.
    ///
    /// Renders a header (using Table for width management) with session identity
    /// info, and populates `opts` with the available actions.
    /// Actions: resume (if not current), fork, rename, delete (always shown).
    /// Cursor starts on resume (or fork if current) for zero-key common case.
    fn session_action_overlay_entries(
        meta: &SessionMeta,
        ctx: &ReplCtx,
        _filter: &str,
        header: &mut Vec<String>,
        opts: &mut Vec<String>,
        active: &mut Option<usize>,
    ) {
        let is_current = meta.id == ctx.current_session_id;
        let ts = Self::format_session_timestamp(&meta.timestamp);
        let name = meta.name.as_deref().unwrap_or(&meta.id);

        // Build a header table with session identity and metadata.
        let avail = crate::output::term_width().saturating_sub(2);
        let mut table = crate::table::Table::new(&["Session", "Turns/Msgs", "Age", "Parent/Dir"])
            .with_aligns(&[
                crate::table::Align::Left,
                crate::table::Align::Right,
                crate::table::Align::Right,
                crate::table::Align::Left,
            ])
            .with_min_widths(&[20, 10, 11, 12])
            .with_priorities(&[3, 1, 1, 2])
            .set_gap(2);

        let marker = if is_current { "◉ " } else { "  " };
        let parent_display = meta.parent_session.as_ref().map(|p| {
            format!("parent: {}", p.rsplit('/').next().unwrap_or(p))
        }).unwrap_or_else(|| {
            format!("dir: {}", meta.cwd.rsplit('/').next().unwrap_or(&meta.cwd))
        });

        table.add_row(&[
            &format!("{}{}", marker, name),
            &format!("{} / {}", meta.user_message_count, meta.message_count),
            &ts,
            &parent_display,
        ]);

        for row in table.render(avail) {
            header.push(row);
        }

        // ── Extra detail rows (non-selectable, full-width) ─────────────
        // Use a separate single-column table so the text spans the full avail width.
        let mut detail_table = crate::table::Table::new(&[""])
            .with_min_widths(&[10])
            .with_priorities(&[1]);
        {
            let first_text = meta.first_prompt_preview.as_deref().unwrap_or("(none)");
            detail_table.add_row(&[&format!("{}: {}", &meta.timestamp, first_text)]);
        }
        {
            // Lazily load the full last prompt text only in the detail view.
            let last_text = meta.last_prompt_text.clone()
                .or_else(|| {
                    let path = meta.path.as_ref()?;
                    let detail = crate::session::repo::JsonlSessionRepo::read_metadata_detail(path)?;
                    detail.last_prompt_text
                })
                .unwrap_or_else(|| "(none)".to_string());
            let last_ts = meta.last_prompt_timestamp.as_deref().unwrap_or("?");
            detail_table.add_row(&[&format!("{}: {}", last_ts, last_text)]);
        }
        for row in detail_table.render(avail) {
            header.push(row);
        }

        // Actions (always shown; resume conditionally absent).
        // Labels carry a hint about the optional [N[t|l]] syntax for resume.
        if !is_current {
            opts.push("resume  [N[t|l] replay length]".into());
        }
        opts.push("fork".into());
        opts.push("rename".into());
        opts.push("delete".into());

        // Cursor starts on resume (if available) or fork.
        *active = if is_current { Some(1) } else { Some(0) };
    }

    /// Parse a session filter, extracting an optional `path:/some/dir` prefix.
    /// `path:.` is expanded to the current working directory.
    /// Returns `(Some(directory) | None, remaining_filter_text)`.
    fn parse_session_path_filter(filter: &str) -> (Option<String>, String) {
        let trimmed = filter.trim();
        if let Some(rest) = trimmed.strip_prefix("path:") {
            let rest = rest.trim_start();
            let (raw_path, after) = if let Some((path_val, after)) = rest.split_once(char::is_whitespace) {
                (path_val, after.trim())
            } else {
                (rest, "")
            };
            let dir = if raw_path.is_empty() || raw_path == "." {
                // Expand `.` to the current working directory.
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|s| !s.is_empty())
            } else {
                Some(raw_path.to_string())
            };
            (dir, after.to_string())
        } else {
            (None, trimmed.to_string())
        }
    }

    /// Load sessions from the repo (if available) and filter by `path:` + fuzzy text.
    /// Returns owned `Vec<SessionMeta>` — loaded fresh each call, discarded after use.
    fn filtered_sessions(ctx: &ReplCtx, filter: &str) -> Vec<SessionMeta> {
        let (dir_filter, fuzzy_filter) = Self::parse_session_path_filter(filter);

        // Determine which CWD to pass to the sync loader:
        // - `path:.` or `path:<explicit>` → load only that directory
        // - bare filter (no path:) → load all
        let cwd_filter = dir_filter.as_ref()
            .and_then(|d| {
                if d.is_empty() { None }
                else { Some(std::path::Path::new(d)) }
            });

        // Load sessions synchronously from disk.
        let all = if let Some(ref repo) = ctx.repo {
            repo.list_sessions_sync(cwd_filter)
        } else {
            Vec::new()
        };

        // Map to SessionMeta.
        let mut metas: Vec<SessionMeta> = all.into_iter()
            .map(|m| SessionMeta {
                id: m.id,
                name: m.name,
                timestamp: m.timestamp,
                cwd: m.cwd,
                parent_session: m.parent_session.map(|p| p.display().to_string()),
                message_count: m.message_count,
                user_message_count: m.user_message_count,
                first_prompt_preview: m.first_prompt_preview,
                last_prompt_text: m.last_prompt_text,
                last_prompt_timestamp: m.last_prompt_timestamp,
                path: Some(m.path),
            })
            .collect();

        // Sort: current session first, then by last modification timestamp descending.
        metas.sort_by(|a, b| {
            let a_cur = a.id == ctx.current_session_id;
            let b_cur = b.id == ctx.current_session_id;
            if a_cur && !b_cur { std::cmp::Ordering::Less }
            else if !a_cur && b_cur { std::cmp::Ordering::Greater }
            else {
                let a_ts = a.last_prompt_timestamp.as_ref().unwrap_or(&a.timestamp);
                let b_ts = b.last_prompt_timestamp.as_ref().unwrap_or(&b.timestamp);
                b_ts.cmp(a_ts)
            }
        });

        if fuzzy_filter.is_empty() {
            return metas;
        }

        // Fuzzy-filter by name, prompt preview, id, and cwd.
        crate::fuzzy::fuzzy_filter_bonus(&metas, &fuzzy_filter, |m| {
            let name = m.name.as_deref().unwrap_or("");
            let preview = m.first_prompt_preview.as_deref().unwrap_or("");
            format!("{} {} {} {}", name, preview, m.id, m.cwd)
        })
        .into_iter()
        .map(|r| r.item.clone())
        .collect()
    }

    /// Build a [`crate::agent::SessionStatsInfo`] for the session currently
    /// active in the REPL. Returns `None` when the repo is unavailable, the
    /// session id is empty (no session), or the id can't be located on disk
    /// (e.g. an unsaved / in-memory session that hasn't been flushed yet).
    ///
    /// Looks up by `ctx.current_session_id` across all CWDs to avoid relying
    /// on the CWD filter; `/stat` should always reflect the actual current
    /// session regardless of where it lives.
    fn current_session_stats_info(ctx: &ReplCtx) -> Option<crate::agent::SessionStatsInfo> {
        if ctx.current_session_id.is_empty() {
            return None;
        }
        let repo = ctx.repo.as_ref()?;
        let all = repo.list_sessions_sync(None);
        let meta = all.into_iter().find(|m| m.id == ctx.current_session_id)?;
        let display_name = meta.name.unwrap_or_else(|| meta.id.clone());
        Some(crate::agent::SessionStatsInfo {
            display_name,
            created: meta.timestamp,
            last_updated: meta.last_prompt_timestamp,
            message_count: meta.message_count,
            user_message_count: meta.user_message_count,
        })
    }

    // ── /tree overlay rendering ──────────────────────────────────────────

    /// Parse the `/tree <…>` argument. Returns the entry id when the argument
    /// **starts** with `<id>` (angle-bracketed id, optionally followed by an
    /// action token / filter text); `None` for a pure search term. The id is
    /// required to have a closing `>`; trailing text after it is not part of
    /// the id.
    fn parse_tree_id(filter: &str) -> Option<String> {
        let f = filter.trim();
        if !f.starts_with('<') {
            return None;
        }
        let end = f.find('>')?;
        if end < 2 {
            return None; // `<>` — empty id
        }
        Some(f[1..end].to_string())
    }

    /// True iff the editor buffer is a fully-formed `compact llm <payload>`
    /// True iff `buffer` is a fully-formed `/tree <id> compact llm
    /// <payload>` line (i.e. the user has already typed a canonical
    /// LLM-compaction line for the given target id). Used by
    /// `handle_enter` to decide between re-staging the default prompt
    /// and submitting verbatim. A payload is required so a bare head or
    /// an empty payload re-stages the default prompt instead of
    /// submitting an empty summary request.
    fn llm_compact_buffer_is_canonical(buffer: &str, target_id: &str) -> bool {
        let head = format!("/tree <{}> compact llm", target_id);
        buffer
            .strip_prefix(&head)
            .map(|rest| !rest.trim().is_empty())
            .unwrap_or(false)
    }

    /// Fuzzy-filter option labels for a `/tree` sub-menu. Returns
    /// `(filtered_opts, active)`. An empty filter returns all options with
    /// the first active; an exact (case-insensitive) match is pre-selected;
    /// otherwise fuzzy matching narrows the list with the best match active.
    /// An unmatched filter keeps the full list with the first option active.
    fn filter_tree_options(all: &[&str], filter: &str) -> (Vec<String>, Option<usize>) {
        let labels: Vec<String> = all.iter().map(|s| s.to_string()).collect();
        if filter.is_empty() {
            return (labels, Some(0));
        }
        // Exact-token pre-selection: the typed prompt *is* the selection.
        // A leading `--` (e.g. `--summarize`) is stripped before comparing.
        let fl = filter.to_lowercase();
        let fl = fl.strip_prefix("--").unwrap_or(&fl);
        if let Some(i) = labels.iter().position(|l| {
            l.to_lowercase().replace(' ', "-") == fl
                || l.to_lowercase() == fl
        }) {
            return (labels, Some(i));
        }
        let ranked = crate::fuzzy::fuzzy_filter(&labels, filter, |l| l.clone());
        if ranked.is_empty() {
            return (labels, Some(0));
        }
        let opts: Vec<String> = ranked.iter().map(|r| r.item.clone()).collect();
        (opts, Some(0))
    }

    /// Find the entry with `id` in the tree (depth-first). Returns `None`
    /// if the id is not present (corrupt index or out-of-tree id).
    fn find_entry_by_id<'a>(
        nodes: &'a [SessionTreeNode],
        id: &str,
    ) -> Option<&'a SessionEntry> {
        fn walk<'a>(
            nodes: &'a [SessionTreeNode],
            id: &str,
        ) -> Option<&'a SessionEntry> {
            for n in nodes {
                if n.entry.id == id {
                    return Some(&n.entry);
                }
                if let Some(e) = walk(&n.children, id) {
                    return Some(e);
                }
            }
            None
        }
        walk(nodes, id)
    }

    /// Return `(prev_id, next_id)` for the entry at `target_id` in tree order.
    /// The flat list is built exactly as `tree_overlay_entries` does — same
    /// filtering, same ordering — so PageUp/PageDown in the action sub-menus
    /// land on the same entries the user would navigate with ↑/↓ in the
    /// `/tree` list overlay.
    fn prev_next_entry_id(
        tree: &[SessionTreeNode],
        leaf_id: Option<&str>,
        target_id: &str,
    ) -> (Option<String>, Option<String>) {
        let active_path: std::collections::HashSet<String> = leaf_id
            .map(|lid| {
                let mut path = std::collections::HashSet::new();
                collect_active_path(tree, lid, &mut path);
                path
            })
            .unwrap_or_default();
        let mut flat: Vec<(usize, String, String, bool, bool, usize)> = Vec::new();
        Self::flatten_tree_nodes(tree, 0, leaf_id, &active_path, &mut flat);
        let pos = flat.iter().position(|(_, _, id, _, _, _)| id == target_id);
        match pos {
            Some(p) => {
                let prev = p.checked_sub(1).and_then(|i| flat.get(i)).map(|(_, _, id, _, _, _)| id.clone());
                let next = flat.get(p + 1).map(|(_, _, id, _, _, _)| id.clone());
                (prev, next)
            }
            None => (None, None),
        }
    }

    /// Walk the tree looking for a node whose `id` matches, and return
    /// that node's `parent_id`. Returns `None` if `id` is not in the
    /// tree or if the matching node has no parent (a root).
    ///
    /// Used by `tree_overlay_entries` to walk the parent_id chain when
    /// the leaf itself is invisible in the list (a tool-result or a
    /// tool-call-only assistant segment that doesn't produce a row) —
    /// we walk up until we find an ancestor that IS visible and mark
    /// that as the active row.
    fn find_parent_id_in_tree<'a>(
        nodes: &'a [SessionTreeNode],
        id: &str,
    ) -> Option<&'a str> {
        fn walk<'a>(
            nodes: &'a [SessionTreeNode],
            id: &str,
        ) -> Option<&'a str> {
            for n in nodes {
                if n.entry.id == id {
                    return n.entry.parent_id.as_deref();
                }
                if let Some(p) = walk(&n.children, id) {
                    return Some(p);
                }
            }
            None
        }
        walk(nodes, id)
    }

    /// Extract the full message body of a `Message` entry (user or
    /// assistant), preserving newlines and surrounding whitespace.
    /// Returns `None` for any other entry variant.
    fn entry_message_body(entry: &SessionEntry) -> Option<String> {
        let message = match &entry.content {
            SessionEntryContent::Message { message } => message,
            _ => return None,
        };
        match message.get("role").and_then(|r| r.as_str()) {
            Some("user") => message
                .pointer("/content/0/text")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            Some("assistant") => message
                .get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    // Same multi-text concatenation as `entry_display_label`,
                    // but joined with newlines (not spaces) so the wrap-aware
                    // header can show paragraphs.
                    arr.iter()
                        .filter_map(|item| {
                            match item.get("type").and_then(|t| t.as_str()) {
                                Some("text") => item.get("text").and_then(|t| t.as_str()),
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }),
            _ => None,
        }
    }

    /// Compute the line budget for the `/tree <id>` message-preview
    /// header. Subtracts the title row and the menu rows from the overlay
    /// budget (`2/3` of the terminal height, matching `redraw`'s
    /// `max_pop`). Anything leftover is the maximum number of header rows
    /// the message preview is allowed to consume. Returns at least 1 so
    /// even a tiny terminal still gets a single truncated preview line.
    fn tree_preview_line_budget(opts_count: usize) -> usize {
        let term_h = term_height().max(1);
        let max_pop = ((term_h as f64 * 2.0 / 3.0).ceil() as usize).max(1);
        // Title (1) + opts rows + header rows ≤ max_pop. The header can
        // also legitimately be empty (a one-line label still fits), so the
        // budget has a floor of 1.
        max_pop.saturating_sub(1).saturating_sub(opts_count).max(1)
    }

    /// Build the info header for `TreeAction` / `SwitchAction`: the selected
    /// entry's body (multi-line, wrapped to the terminal width) when it's a
    /// message, otherwise the precomputed single-line label. The ID is
    /// intentionally NOT shown — it is already visible in the prompt buffer
    /// (`/tree <id>`).
    ///
    /// `max_body_lines` caps the number of header rows consumed by the
    /// message preview. The caller computes it from the terminal height
    /// and the count of menu items so the menu is never pushed off-screen
    /// by a long message: the overlay budget (`2/3` of `term_h`) minus the
    /// title row and the menu rows must cover the header. When the body
    /// exceeds the cap, the last visible line gets a trailing `…` so the
    /// user knows the preview is incomplete — same convention as the
    /// single-line rendering in `Table::render`.
    fn tree_entry_header(
        entry: Option<&SessionEntry>,
        label: &str,
        max_body_lines: usize,
        header: &mut Vec<String>,
    ) {
        let avail = term_width().saturating_sub(2);
        // Try the multi-line message body first; fall back to the
        // single-line label for non-message entries (compaction, model
        // change, branch summary, ...). The label branch never exceeds
        // one row, so it is unaffected by `max_body_lines`.
        let body_lines: Vec<String> = if let Some(text) =
            entry.and_then(Self::entry_message_body)
        {
            let mut lines = wrap(&text, avail);
            // Sanity floor: a caller that hands us `max_body_lines == 0`
            // (terminal too short to fit anything) still gets a single
            // truncated row so the menu remains self-describing.
            let cap = max_body_lines.max(1);
            if lines.len() > cap {
                lines.truncate(cap);
                // Marker sits at the end of the last visible row — same
                // ellipsis convention as the single-line path. Never
                // appends a dedicated truncation row (that would steal
                // one of the menu's reserved lines).
                if let Some(last) = lines.last_mut() {
                    *last = format!("{last}…");
                }
            }
            lines
        } else {
            let single = Self::sanitise_single_line(label);
            if single.is_empty() {
                Vec::new()
            } else {
                vec![single]
            }
        };
        for line in body_lines {
            header.push(line);
        }
    }

    /// Build the info header for the `CompactSummary` sub-menu: what this
    /// compaction will drop / keep and the token estimate that becomes
    /// `tokensBefore`. Derived from the flattened tree + active path already
    /// used by `tree_overlay_entries` — no extra I/O. This is the in-menu
    /// equivalent of `SessionAction`'s per-session info header.
    fn compact_summary_header(
        target_id: &str,
        label: &str,
        ctx: &ReplCtx,
        header: &mut Vec<String>,
    ) {
        // Count dropped (before target on the active path) vs kept (target
        // through leaf) entries from the flattened tree.
        let mut flat = Vec::new();
        let active_path: std::collections::HashSet<String> = ctx
            .leaf_id
            .as_deref()
            .map(|lid| {
                let mut p = std::collections::HashSet::new();
                collect_active_path(&ctx.tree, lid, &mut p);
                p
            })
            .unwrap_or_default();
        Self::flatten_tree_nodes(
            &ctx.tree,
            0,
            ctx.leaf_id.as_deref(),
            &active_path,
            &mut flat,
        );
        let idx = flat.iter().position(|(_, _, id, _, _, _)| id == target_id);
        let (dropped, kept) = match idx {
            Some(i) => (i, flat.len() - i),
            None => (0, flat.len()),
        };

        let avail = term_width().saturating_sub(2);
        let mut table = Table::new(&["Entry", "ID"])
            .with_aligns(&[Align::Left, Align::Left])
            .with_min_widths(&[24, 12])
            .with_priorities(&[3, 1])
            .set_gap(2);
        table.add_row(&[label, &target_id[..target_id.len().min(12)]]);
        for row in table.render(avail) {
            header.push(row);
        }
        // Detail rows (non-selectable, full-width): the SessionAction style.
        let mut detail = Table::new(&[""])
            .with_min_widths(&[10])
            .with_priorities(&[1]);
        detail.add_row(&[format!(
            "drops {} entr{} before this point (~{}k tokens)",
            dropped,
            if dropped == 1 { "y" } else { "ies" },
            ctx.context_tokens / 1000,
        )]);
        detail.add_row(&[format!("keeps {} entries to the current leaf", kept)]);
        detail.add_row(&["written into this session (in-place compaction)"]);
        for row in detail.render(avail) {
            header.push(row);
        }
    }

    /// Per-branch-depth background colours for the `/tree` overlay.
/// Unlike turn indent (which advances for every model response),
/// `branch_depth` only increments when descending from a node with
/// multiple children — so un-branched conversations stay on the
/// default panel background (`None`) and only forks get tinted.
///
/// The palette uses 256-colour codes (`\x1b[48;5;Nm`) rather than
/// true-colour (`\x1b[48;2;R;G;Bm`) so the tints render correctly
/// on every terminal that supports 256-colour SGR — a much wider
/// compatibility surface than true-colour, and every value here is
/// also valid in 24-bit-capable terminals.
///
/// All tints sit a clear step below the default panel background
/// (`48;5;235` ≈ RGB(38,38,38)) so the eye reads them as "deeper,
/// separate from the conversation above" rather than as a brighter
/// accent. The differences are large enough to be obvious at a
/// glance — not so subtle that they vanish against the panel. The
/// 256-colour greyscale is coarse, so the warm/cool tints are
/// encoded by alternating the *brightness* step (one row sits one
/// step below the panel, the next sits two steps below, alternating)
/// rather than by hue — the alternation reads as a depth alternation
/// even without chromatic variation.
///
/// Indexing: `(branch_depth - 1) % 4`, so depth 1 picks the first
/// entry, depth 2 the second, depth 3 the third, depth 4 the
/// fourth, depth 5 cycles back to the first, and so on.
    const TREE_BG_BY_LEVEL: &[&str] = &[
        // Depth 1 — one step below the panel, 235 -> 234.
        "\x1b[48;5;234m",
        // Depth 2 — two steps below, 235 -> 233.
        "\x1b[48;5;233m",
        // Depth 3 — back to one step below (235 -> 234) for the
        // depth alternation: depth 1 / 3 share a tint, depth 2 / 4
        // share a deeper tint, making two consecutive forks easy
        // to tell apart.
        "\x1b[48;5;234m",
        // Depth 4 — two steps below.
        "\x1b[48;5;233m",
    ];
    fn tree_row_bg(branch_depth: usize) -> Option<&'static str> {
        if branch_depth == 0 {
            None
        } else {
            let idx = (branch_depth - 1) % Self::TREE_BG_BY_LEVEL.len();
            Some(Self::TREE_BG_BY_LEVEL[idx])
        }
    }

    /// Build `/tree` overlay rows from a pre-built session tree.
    /// Uses the same `Table` infrastructure as `/session` for consistent
    /// rendering. Each row carries its own per-branch-depth background
    /// tint so a forked tree is readable at a glance — un-branched
    /// conversations stay on the default panel background, and only
    /// branches are tinted.
    fn tree_overlay_entries(
        tree: &[SessionTreeNode],
        leaf_id: Option<&str>,
        filter: &str,
        rows: &mut Vec<String>,
        row_bgs: &mut Vec<Option<&'static str>>,
        targets: &mut Vec<String>,
        active: &mut Option<usize>,
    ) {
        if tree.is_empty() {
            rows.push("(no session)".into());
            return;
        }

        // Build active-path set by walking from leaf_id up through the tree.
        let active_path: std::collections::HashSet<String> = leaf_id
            .map(|lid| {
                let mut path = std::collections::HashSet::new();
                collect_active_path(tree, lid, &mut path);
                path
            })
            .unwrap_or_default();

        // Precompute flat entries: (indent, label, entry_id, is_on_active_path,
        // is_current, branch_depth)
        let mut flat: Vec<(usize, String, String, bool, bool, usize)> = Vec::new();
        Self::flatten_tree_nodes(tree, 0, leaf_id, &active_path, &mut flat);

        // Apply text filter (fuzzy match on display labels).
        if !filter.is_empty() {
            let f = filter.to_lowercase();
            flat.retain(|(_, label, _, _, _, _)| label.to_lowercase().contains(&f));
        }

        if flat.is_empty() {
            rows.push("(no matches)".into());
            return;
        }

        // Render via Table for column alignment like /session. There is
        // no separate indicator column — the 2-char marker area in
        // `overlay_lines` carries the `>` / `*` cursor and active-state
        // symbols, so the row string itself just needs the label, prefixed
        // by the indent space run.
        let avail = term_width().saturating_sub(2);
        let mut table = Table::new(&["Entry"])
            .with_aligns(&[Align::Left])
            .with_min_widths(&[20])
            .with_priorities(&[1])
            .set_gap(0);

        for (indent, label, entry_id, is_active, _is_curr, branch_depth) in &flat {
            // Active-path rows get a brighter foreground (`\x1b[22m`
            // — normal intensity) versus `\x1b[2m` (dim) for abandoned
            // branches. The branch-depth background tint is applied
            // separately by `overlay_lines` so the cursor-selected row
            // (which uses BG_SEL) can override it without clashing with
            // an inner `\x1b[48;2;R;G;Bm` escape. The marker `*` then
            // conveys the leaf / current position, so we don't need a
            // dedicated indicator column.
            let fg = if *is_active { "\x1b[22m" } else { "\x1b[2m" };
            let display = format!(
                "{fg}{:indent$}{label}",
                "",
                indent = indent,
                fg = fg,
            );
            table.add_row(&[&display]);
            targets.push(entry_id.clone());
            row_bgs.push(Self::tree_row_bg(*branch_depth));
        }

        *rows = table.render(avail);

        // Highlight current leaf. The leaf may be invisible in the list
        // (a tool-result or a tool-call-only assistant segment), so
        // walk up the parent_id chain to the nearest visible ancestor
        // and mark THAT as the active row — otherwise the user
        // opens /tree and the cursor lands at the top of a long
        // list, with no `*` indicator anywhere.
        if let Some(lid) = leaf_id {
            let mut cur: Option<&str> = Some(lid);
            while let Some(id) = cur {
                if let Some(pos) = flat.iter().position(|(_, _, eid, _, _, _)| eid == id) {
                    *active = Some(pos);
                    break;
                }
                // Walk up the parent_id chain. We need the SessionEntry
                // map for that, which lives in `labels` only by id;
                // look it up via a tree walk.
                cur = Self::find_parent_id_in_tree(tree, id);
            }
        }
    }

    /// Recursively flatten tree nodes into entries for table rendering.
    ///
    /// `indent` is the number of leading spaces (increases +2 at branching
    /// points). Entries are additionally **grouped by turn**: a user prompt
    /// starts a turn, and everything belonging to that turn until the next
    /// user prompt (assistant response text) is indented one extra char so
    /// the response visually belongs to its prompt. Entries without a
    /// visible label (tool results, tool-call-only assistant segments, meta
    /// entries) are skipped but still descended and keep the turn state
    /// flowing through them. Compaction rows read
    /// `compaction <id> before "…"` with the kept entry's text resolved from
    /// the subtree.
    fn flatten_tree_nodes(
        nodes: &[SessionTreeNode],
        indent: usize,
        leaf_id: Option<&str>,
        active_path: &std::collections::HashSet<String>,
        out: &mut Vec<(usize, String, String, bool, bool, usize)>,
    ) {
        // Precompute id -> display label over the WHOLE tree so compaction
        // rows can resolve their kept entry: `firstKeptEntryId` points to an
        // entry BEFORE the compaction (an ancestor/sibling elsewhere in the
        // tree), never a descendant, so a subtree search never finds it.
        let mut labels: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        fn collect_labels(
            nodes: &[SessionTreeNode],
            labels: &mut std::collections::HashMap<String, String>,
        ) {
            for n in nodes {
                let t = Repl::entry_display_label(&n.entry, n.label.as_deref());
                if !t.is_empty() {
                    labels.insert(n.entry.id.clone(), t);
                }
                collect_labels(&n.children, labels);
            }
        }
        collect_labels(nodes, &mut labels);

        // branch_depth starts at 0 — the root conversation lives on the
        // default panel background. The depth only increments when we
        // descend from a node with multiple children.
        Self::flatten_turns(
            nodes,
            indent,
            0,
            leaf_id,
            active_path,
            &labels,
            &mut false, // not inside a user turn yet
            out,
        );
    }

    /// Flatten the whole session tree for a menu provider (Phase 6.7):
    /// computes the active path from the leaf and returns the
    /// `(indent, label, id, on_path, is_current, branch_depth)` tuples
    /// `TreeProvider` turns into rows.
    fn flatten_all_tree(ctx: &ReplCtx) -> Vec<(usize, String, String, bool, bool, usize)> {
        let active_path: std::collections::HashSet<String> = ctx
            .leaf_id
            .as_deref()
            .map(|lid| {
                let mut p = std::collections::HashSet::new();
                collect_active_path(&ctx.tree, lid, &mut p);
                p
            })
            .unwrap_or_default();
        let mut flat: Vec<(usize, String, String, bool, bool, usize)> = Vec::new();
        Repl::flatten_tree_nodes(&ctx.tree, 0, ctx.leaf_id.as_deref(), &active_path, &mut flat);
        flat
    }

    /// Find the id of the most recent user-role message on the active
    /// branch (the branch ending at `leaf_id`). Returns `None` if the
    /// active branch has no user message (e.g. an empty session, or a
    /// branch containing only model / thinking / compaction entries).
    ///
    /// Used by the `/compact` overlay to auto-resolve `target_id` to
    /// "the start of the current turn": the dropped span becomes
    /// everything before the user's last prompt, the kept range starts
    /// with that prompt, and the next LLM turn sees
    /// `[summary] + [last user prompt] + [last assistant response] +
    /// [new user prompt]`. This matches the natural "compact the
    /// conversation so far" intent.
    ///
    /// The tree contains only the active branch (replay flattens
    /// descendants of the leaf); a leaf-root walk is enough.
    pub(crate) fn last_user_message_id_on_active_branch(
        tree: &[SessionTreeNode],
        leaf_id: &str,
    ) -> Option<String> {
        fn walk(
            nodes: &[SessionTreeNode],
            leaf_id: &str,
            out: &mut Option<String>,
        ) -> bool {
            for node in nodes {
                // Found the leaf: emit the nearest user-role ancestor
                // up the chain (the just-before-leaf user prompt) and
                // stop the walk. We track `out` so a user message
                // sitting ON the leaf is still picked up — the leaf
                // itself is the most recent user message in that case.
                if node.entry.id == leaf_id {
                    if Repl::is_user_message(&node.entry) {
                        *out = Some(node.entry.id.clone());
                    }
                    return true;
                }
                if walk(&node.children, leaf_id, out) {
                    if out.is_none() && Repl::is_user_message(&node.entry) {
                        *out = Some(node.entry.id.clone());
                    }
                    return true;
                }
            }
            false
        }
        let mut found = None;
        walk(tree, leaf_id, &mut found);
        found
    }

    /// True iff `entry` is a `Message` with `role == "user"`. Mirrors
    /// the `entry_display_label` role check.
    pub(crate) fn is_user_message(entry: &SessionEntry) -> bool {
        matches!(
            &entry.content,
            SessionEntryContent::Message { message }
                if message.get("role").and_then(|r| r.as_str()) == Some("user")
        )
    }
    /// Turn-tracking worker for `flatten_tree_nodes`. `in_turn` flips on at
    /// a user prompt and off at the next user prompt / compaction entry;
    /// visible non-user rows while `in_turn` are indented two chars deeper
    /// (the model response sits visibly under its prompt, not flush with
    /// it). `branch_depth` tracks how many branching ancestors sit above
    /// the current node — it stays the same through a linear chain and
    /// increments by one when descending from a node with multiple
    /// children. Used by `tree_overlay_entries` to colour alternating
    /// branches differently so a forked tree is readable at a glance;
    /// un-branched conversations stay on the default panel background.
    /// `labels` is the whole-tree id → display-label map used by
    /// compaction rows to resolve the kept entry.
    #[allow(clippy::too_many_arguments)]
    fn flatten_turns(
        nodes: &[SessionTreeNode],
        indent: usize,
        branch_depth: usize,
        leaf_id: Option<&str>,
        active_path: &std::collections::HashSet<String>,
        labels: &std::collections::HashMap<String, String>,
        in_turn: &mut bool,
        out: &mut Vec<(usize, String, String, bool, bool, usize)>,
    ) {
        for node in nodes {
            let kind = turn_kind(&node.entry);
            let label =
                Self::entry_display_label_ext(&node.entry, node.label.as_deref(), labels);

            // Turn-state updates happen even for skipped entries so long
            // tool-result chains don't break turn grouping. Invisible rows
            // (tool results, tool-call-only assistant segments) pass the
            // turn indent through unchanged.
            let row_indent = match kind {
                TurnKind::User => {
                    *in_turn = true;
                    indent
                }
                TurnKind::Assistant => {
                    if *in_turn {
                        indent + 2
                    } else {
                        indent
                    }
                }
                TurnKind::Boundary => {
                    *in_turn = false;
                    indent
                }
            };

            if label.is_empty() {
                // Meta entry: skip visual row but still descend, keeping the
                // SAME row indent (an invisible tool chain stays inside the
                // turn, and the next visible response stays at turn depth).
                // Branching status is preserved: descendants of an invisible
                // branching point are still on a branch.
                let child_branch_depth = if node.children.len() > 1 {
                    branch_depth + 1
                } else {
                    branch_depth
                };
                Self::flatten_turns(
                    &node.children,
                    indent,
                    child_branch_depth,
                    leaf_id,
                    active_path,
                    labels,
                    in_turn,
                    out,
                );
                continue;
            }

            let is_active = active_path.contains(&node.entry.id);
            let is_current = leaf_id == Some(node.entry.id.as_str());
            out.push((
                row_indent,
                label.clone(),
                node.entry.id.clone(),
                is_active,
                is_current,
                branch_depth,
            ));

            // Children continue the turn flow at the same base indent: the
            // chain user→assistant→tool→assistant→user is linear, and the
            // turn depth is derived per node from `in_turn`, never stacked.
            // Branching points indent their children +4. Mathematically:
            //   - parent is a user prompt at parameter indent N → branch
            //     row at N+4 ("four over user prompts one level up")
            //   - parent is a model response at row_indent N+2 →
            //     branch row at (N+2)+2 = N+4 ("two over model
            //     responses one level up")
            // Both rules land on +4, so a single constant covers both.
            //
            // A node with multiple children also increments branch_depth:
            // its descendants belong to a sub-branch and should take on
            // the next colour in the alternation.
            let child_indent = if node.children.len() > 1 {
                indent + 4
            } else {
                indent
            };
            let child_branch_depth = if node.children.len() > 1 {
                branch_depth + 1
            } else {
                branch_depth
            };
            Self::flatten_turns(
                &node.children,
                child_indent,
                child_branch_depth,
                leaf_id,
                active_path,
                labels,
                in_turn,
                out,
            );
        }
    }

    /// `entry_display_label` with the whole-tree label map, so compaction
    /// rows can resolve the kept entry's text (`firstKeptEntryId` lives
    /// before the compaction, elsewhere in the tree).
    fn entry_display_label_ext(
        entry: &SessionEntry,
        label: Option<&str>,
        labels: &std::collections::HashMap<String, String>,
    ) -> String {
        if let SessionEntryContent::Compaction { first_kept_entry_id, .. } = &entry.content {
            let kept = labels
                .get(first_kept_entry_id)
                .map(|t| Self::sanitise_single_line(t))
                .or_else(|| label.map(|l| Self::sanitise_single_line(l)))
                .unwrap_or_else(|| "(kept entry)".into());
            // Yellow FG marks compactions as a "special action" — they
            // summarise / restructure history rather than carry content.
            //
            // `\x1b[39m` (default foreground) closes the yellow span
            // WITHOUT touching the background, so the row's BG (set by
            // `bg_line` in the overlay pipeline) carries through the
            // trailing padding. Using `\x1b[0m` here would reset the BG
            // mid-row, leaving the padding against the terminal's
            // default background and visually breaking the row.
            return format!(
                "\x1b[33mcompaction {} before \"{}\"\x1b[39m",
                &first_kept_entry_id[..first_kept_entry_id.len().min(12)],
                kept
            );
        }
        Self::entry_display_label(entry, label)
    }

    /// Build a display label for one tree entry.
    ///
    /// The /tree list is turn-oriented: only **user prompts** and **model
    /// response text** are shown (no tool calls, no tool results, no
    /// reasoning). Assistant responses whose text is empty (tool-call-only
    /// segments) are suppressed entirely. Compaction entries are rendered by
    /// `entry_display_label_ext` (needs subtree lookup for the kept text).
    fn entry_display_label(entry: &SessionEntry, _label: Option<&str>) -> String {
        let body = match &entry.content {
            SessionEntryContent::Message { message } => {
                match message.get("role").and_then(|r| r.as_str()) {
                    Some("user") => {
                        let text = message.pointer("/content/0/text")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(user)");
                        Self::sanitise_single_line(text)
                    }
                    Some("assistant") => {
                        // Text only: reasoning and tool calls are skipped;
                        // a response with no text (tool-call-only segment)
                        // is suppressed entirely and its entries fold into
                        // the previous visible row.
                        let text = message.get("content")
                            .and_then(|c| c.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|item| match item.get("type").and_then(|t| t.as_str()) {
                                        Some("text") => item.get("text").and_then(|t| t.as_str()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default();
                        if text.is_empty() {
                            String::new() // no visible row for this entry
                        } else {
                            Self::sanitise_single_line(&text)
                        }
                    }
                    // Tool results are never shown.
                    Some("toolResult") => String::new(),
                    _ => String::new(),
                }
            }
            SessionEntryContent::Compaction { .. } => {
                // Rendered by entry_display_label_ext (kept text resolved
                // from the subtree); unreachable here but kept exhaustive.
                String::new()
            }
            SessionEntryContent::ThinkingLevelChange { thinking_level } => {
                format!("thinking: {}", thinking_level)
            }
            SessionEntryContent::ModelChange { provider, model_id } => {
                // Yellow FG for model switches — another "special action"
                // marker, mirroring the compaction colour. `\x1b[39m` (not
                // `\x1b[0m`) so the row's BG carries through the trailing
                // padding — see `entry_display_label_ext` for rationale.
                format!("\x1b[33mmodel: {}/{}\x1b[39m", provider, model_id)
            }
            SessionEntryContent::ActiveToolsChange { .. } => String::new(),
            SessionEntryContent::BranchSummary { summary, .. } => {
                let s = Self::sanitise_single_line(summary);
                if s.is_empty() { "[S] (branch summary)".into() } else { format!("[S] {}", s) }
            }
            SessionEntryContent::Label { target_id, label } => {
                let l = label.as_deref().unwrap_or("(label)");
                format!("[L] {} -> {}", l, target_id)
            }
            // Meta entries: skip.
            SessionEntryContent::SessionInfo { .. } => String::new(),
            SessionEntryContent::Leaf { .. } => String::new(),
            SessionEntryContent::Custom { custom_type, .. } => {
                format!("custom: {}", custom_type)
            }
            SessionEntryContent::CustomMessage { custom_type, .. } => {
                format!("custom msg: {}", custom_type)
            }
        };
        // Normalise whitespace (newlines → spaces, collapse runs) like the
        // /session first-prompt preview does.
        let body = Self::sanitise_single_line(&body);
        // Prepend resolved label if this node has one from a Label entry.
        if let Some(l) = _label {
            if !body.is_empty() {
                return format!("[{}] {}", l, body);
            }
        }
        body
    }

    /// Build the `/sandbox` overlay rows for `filter`.
    ///
    /// When `filter` is empty, shows the three sub-commands (add_ro, add_rw, remove).
    /// When `filter` starts with a sub-command + space, shows file-path completions
    /// (for add_ro/add_rw) or the current session bindings (for remove).
    fn sandbox_overlay_entries(
        filter: &str,
        opts: &mut Vec<String>,
        active: &mut Option<usize>,
    ) -> Option<&'static str> {
        let subcommands = [
            ("add_ro", "Make path available read-only"),
            ("add_rw", "Make path available read-write"),
            ("remove", "Remove a path from the bindings"),
        ];

        if filter.trim().is_empty() {
            // Show the three sub-commands.
            for (name, help) in &subcommands {
                opts.push(format!("{:<10} {}", name, help));
            }
            *active = None;
            return None;
        }

        let (sub, path) = if let Some((w, r)) = filter.split_once(' ') {
            (w, r)
        } else if subcommands.iter().any(|(n, _)| *n == filter) {
            // Exact subcommand match: enter its mode with an empty path.
            (filter, "")
        } else {
            // Fuzzy-filter on sub-commands.
            for (name, help) in &subcommands {
                if name.starts_with(filter) || help.contains(filter) {
                    opts.push(format!("{:<10} {}", name, help));
                }
            }
            if opts.is_empty() {
                opts.push("(no matching command)".into());
            }
            *active = None;
            return None;
        };

        match sub {
            "add_ro" | "add_rw" => {
                let path = path.trim();
                if path.is_empty() {
                    opts.push("(type a path and press Tab to complete)".into());
                    *active = None;
                    return Some("add_ro");
                }
                // The last token is the one being typed; show file completions.
                let last_token = path.split_whitespace().last().unwrap_or("");
                // Expand ~/ prefix before listing files, then round-trip back.
                let uses_tilde = last_token.starts_with('~');
                let expanded = if uses_tilde {
                    crate::path_util::expand_tilde(last_token).to_string_lossy().into_owned()
                } else {
                    last_token.to_string()
                };
                let completions = Self::complete_path_prefix(&expanded);
                // Round-trip ~-prefixed results back to ~ for display.
                let home = if uses_tilde {
                    std::env::var("HOME")
                        .map(|h| if h.ends_with('/') { h } else { format!("{}/", h) })
                        .ok()
                } else {
                    None
                };
                let completions: Vec<String> = completions.into_iter().map(|c| {
                    if let Some(ref home) = home {
                        if let Some(rest) = c.strip_prefix(home) {
                            return format!("~/{}", rest);
                        }
                    }
                    c
                }).collect();
                if completions.is_empty() {
                    opts.push("(no matching path)".into());
                } else {
                    for c in &completions {
                        opts.push(c.clone());
                    }
                }
                *active = None;
                return Some("add_ro");
            }
            "remove" => {
                let path = path.trim();
                let combined = crate::sandbox_bindings::combined_list();
                if combined.is_empty() {
                    opts.push("(no bindings configured)".into());
                    *active = None;
                    return Some("remove");
                }
                let filtered: Vec<_> = if path.is_empty() {
                    combined
                } else {
                    crate::fuzzy::fuzzy_filter(&combined, path, |b| {
                        let mode_str = if b.mode == crate::config::BindMode::Ro { "ro" } else { "rw" };
                        let src = if b.is_excluded { "excluded" } else if b.is_session { "session" } else { "cfg" };
                        format!("{} {} {} {}",
                            b.host_path.to_string_lossy(),
                            b.sandbox_path.to_string_lossy(),
                            mode_str,
                            src)
                    })
                    .into_iter()
                    .map(|r| r.item.clone())
                    .collect()
                };
                if filtered.is_empty() {
                    opts.push("(no matching bindings)".into());
                } else {
                    let avail = term_width().saturating_sub(2);
                    let mut table = Table::new(&["From", "To", "Mode", "Src"])
                        .with_aligns(&[Align::Left, Align::Left, Align::Center, Align::Left])
                        .with_min_widths(&[10, 10, 4, 5])
                        .with_priorities(&[2, 2, 1, 1])
                        .set_gap(2);
                    for b in &filtered {
                        let from = b.host_path.to_string_lossy().to_string();
                        let to = b.sandbox_path.to_string_lossy().to_string();
                        let mode = if b.mode == crate::config::BindMode::Ro { "ro" } else { "rw" };
                        let src = if b.is_excluded { "excluded" } else if b.is_session { "session" } else { "cfg" };
                        table.add_row(&[from.as_str(), to.as_str(), mode, src]);
                    }
                    let rows = table.render_multiline(avail);
                    opts.extend(rows);
                }
                *active = None;
                return Some("remove");
            }
            _ => {
                opts.push("(unknown subcommand: use add_ro, add_rw, or remove)".into());
                *active = None;
                return None;
            }
        }
    }

    /// Complete a file path prefix by listing the parent directory.
    /// Returns matching paths (absolute) that start with `prefix`.
    /// If `prefix` is empty, returns nothing (the user must type at least a `/`).
    fn complete_path_prefix(prefix: &str) -> Vec<String> {
        complete_path_prefix_impl(prefix)
    }

    /// Apply a normal editing key to the line editor. Returns `Some(action)`
    /// only for terminal actions (Enter submits, Esc cancels the line,
    /// Ctrl-C exits). All other keys mutate `editor` and return `None`. History
    /// navigation is handled here too. `Ctrl-C` is *not* handled here (the
    /// caller checks it before delegating, so it can stop the terminal).
    fn edit_key(
        key: event::KeyEvent,
        editor: &mut LineEditor,
        history: &mut History,
        nav: &mut Option<usize>,
        draft: &mut String,
        prompt: &str,
    ) -> Option<InputAction> {
        match key.code {
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    editor.insert_newline();
                    None
                } else if editor.char_before_cursor() == Some('\\') {
                    // Kitty-independent multi-line entry (mirrors pi): a '\\'
                    // before Enter inserts a newline instead of submitting,
                    // so users on terminals that don't disambiguate Shift+Enter
                    // still get a newline without changing terminal settings.
                    editor.backspace();
                    editor.insert_newline();
                    None
                } else {
                    Some(InputAction::Submit(editor.text()))
                }
            }
            KeyCode::F(3) => {
                // wezterm (and xterm-style terminals) send Shift+Enter as the
                // legacy CSI sequence `\x1b[13;2~` even when the Kitty keyboard
                // protocol is *disabled*. crossterm 0.29 parses that as
                // `KeyCode::F(3)` (+SHIFT), not Enter — so it never reaches the
                // Enter handler above. Mirror pi: treat it as a newline.
                // (picocode has no F-key bindings, so this is safe.)
                editor.insert_newline();
                None
            }
            KeyCode::Esc => None,
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match c {
                        'u' => editor.delete_line_start(),
                        'k' => editor.delete_line_end(),
                        // Ctrl-J (LF) inserts a newline reliably even where
                        // Shift+Enter isn't disambiguated (Kitty off).
                        'j' => editor.insert_newline(),
                        _ => {}
                    }
                    None
                } else if key.modifiers.contains(KeyModifiers::ALT) {
                    match c {
                        'b' => editor.word_left(),
                        'f' => editor.word_right(),
                        _ => {}
                    }
                    None
                } else {
                    editor.insert(c);
                    None
                }
            }
            KeyCode::Backspace => {
                editor.backspace();
                None
            }
            KeyCode::Delete => {
                editor.delete();
                None
            }
            KeyCode::Left => {
                editor.left();
                None
            }
            KeyCode::Right => {
                editor.right();
                None
            }
            KeyCode::Home => {
                editor.home();
                None
            }
            KeyCode::End => {
                editor.end();
                None
            }
            KeyCode::Up => {
                let (w, _) = crossterm::terminal::size().unwrap_or((80, 24));
                if !editor.visual_up(w as usize, prompt) {
                    // Only fall through to history when there is no unsubmitted
                    // draft in the buffer. Otherwise, a stray Up at the top of the
                    // prompt would silently stash the in-progress text and
                    // replace it with the previous history entry — a destructive
                    // action that's easy to trigger when reaching for Home.
                    // When a draft *does* exist (the user already navigated
                    // back), keep the existing behavior so they can still step
                    // through history.
                    if editor.text().is_empty() || nav.is_some() {
                        if let Some(s) = history.prev(&editor.text(), nav, draft) {
                            editor.set(&s);
                        }
                    }
                }
                None
            }
            KeyCode::Down => {
                let (w, _) = crossterm::terminal::size().unwrap_or((80, 24));
                if !editor.visual_down(w as usize, prompt) {
                    if let Some(s) = history.next(nav, draft) {
                        editor.set(&s);
                    }
                }
                None
            }
            _ => None,
        }
    }
}

/// Resolve the external editor command from environment variables.
/// Priority: `$VISUAL` > `$EDITOR` > `"vim"`.
/// Returns `(command, args)` where args may include flags e.g. `("emacs", ["-nw"])`.
fn resolve_editor() -> (String, Vec<String>) {
    let raw = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vim".into());
    let parts: Vec<String> = raw.split_whitespace().map(|s| s.to_string()).collect();
    let cmd = parts.first().cloned().unwrap_or_else(|| "vim".into());
    let args: Vec<String> = parts.into_iter().skip(1).collect();
    (cmd, args)
}

/// Open `current` in `$EDITOR` (foreground child process) and return the
/// modified text.  The editor inherits the terminal directly (stdin/stdout/
/// stderr), so the caller must drop raw mode beforehand and re‑enter it
/// afterwards.
///
/// A single trailing `\n` is stripped on read‑back because most editors add
/// one.  Internal newlines and intentional trailing blank lines are preserved.
/// The tempfile is cleaned up on every return path.
pub(crate) fn edit_in_editor(current: &str) -> io::Result<String> {
    use std::os::unix::fs::PermissionsExt;

    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let mut path = PathBuf::from(tmpdir);
    path.push(format!("picocode_prompt_{}", std::process::id()));

    // Write current content, mode 0600 so other users cannot read the
    // prompt mid‑edit.
    {
        let mut f = std::fs::File::create(&path)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(current.as_bytes())?;
        f.flush()?;
    }

    // Launch editor.
    let (cmd, mut args) = resolve_editor();
    args.push(path.to_string_lossy().to_string());

    let status = std::process::Command::new(&cmd)
        .args(&args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| {
            let _ = std::fs::remove_file(&path);
            e
        })?;

    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("editor exited with status {:?}", status.code()),
        ));
    }

    // Read back.
    let mut result = std::fs::read_to_string(&path).map_err(|e| {
        let _ = std::fs::remove_file(&path);
        e
    })?;

    // Strip exactly one trailing newline (most editors add one).
    if result.ends_with('\n') {
        result.pop();
    }

    let _ = std::fs::remove_file(&path);
    Ok(result)
}

/// Let the user review — and edit — a generated compaction summary before it
/// is written to the session (doc/nested_compactions.md §review UX).
///
/// 1. Drop raw mode, open `draft` in the external `$EDITOR` (reuses the
///    Ctrl-E prompt-editing machinery), re-enter raw mode.
/// 2. Ask the user to accept (default) or cancel via `Output`'s
///    accept/cancel dialog.
///
/// Returns `Some(final_text)` to store (the possibly-edited summary) on
/// accept, or `None` on cancel / editor failure. The caller must not append a
/// compaction entry when this returns `None`.
///
/// `guard` must be the REPL's term guard (only present on an interactive
/// TTY); the editor cannot be launched without it. Callers running outside
/// the interactive REPL (quiet / pipe / tests) should pass a review closure
/// that simply accepts the draft — see `run_compaction`'s callers.
pub(crate) fn review_compaction_summary(
    guard: &TermGuard,
    output: &dyn Output,
    draft: &str,
) -> Option<String> {
    // Drop raw mode for the foreground editor; re-enter on return (mirrors
    // the Ctrl-E path in read_input).
    let _ = guard.stop();
    let edited = edit_in_editor(draft);
    let _ = guard.start();

    let text = match edited {
        Ok(t) => t,
        Err(e) => {
            output.display_error(&format!("editor error: {}", e));
            return None;
        }
    };
    if output.confirm_compaction_summary(
        "Write this compaction summary into the session? (edited text is stored)",
    ) {
        Some(text)
    } else {
        output.display_text("Compaction cancelled.");
        None
    }
}


/// Expand `~` and `~/` prefix to the home directory.  Leaves other paths
/// unchanged.  Used by path completion to resolve `~` before calling `ls`
/// inside the sandbox (which doesn't do shell expansion).
fn expand_tilde(path: &str) -> String {
    crate::path_util::expand_tilde(path).to_string_lossy().into_owned()
}

/// Generic file-path prefix completion. Lists the parent directory of `prefix`
/// and returns entries whose file name starts with the stem of `prefix`.
/// Directories get a trailing `/`.
///
/// When `prefix` is an existing directory (or ends with `/`), lists the full
/// contents of that directory (no stem filtering).  This makes `/tmp<tab>`
/// show everything under `/tmp`, not just entries starting with `"tmp"`.
///
/// This is a public free function so it can be used by the regular prompt input
/// (e.g. when the user types `/tmp/foo<tab>` in the prompt text) as well as
/// slash-command overlays.
pub fn complete_path_prefix_impl(prefix: &str) -> Vec<String> {
    if prefix.is_empty() {
        return Vec::new();
    }

    let path = std::path::Path::new(prefix);

    // If the prefix is an existing directory (or ends with /), list its
    // full contents rather than filtering by the directory name as a stem.
    // This makes `/tmp<tab>` show everything under /tmp, not just entries
    // starting with "tmp".
    if prefix.ends_with('/') || path.is_dir() {
        return list_dir_entries(path, "");
    }

    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let stem = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // When the stem is empty (e.g. prefix is just "/"), list all.
    if stem.is_empty() {
        list_dir_entries(parent, "")
    } else {
        list_dir_entries(parent, &stem)
    }
}

/// List entries in `dir`, filtering by `filter` (empty = all).
/// Directories get a trailing `/`.
fn list_dir_entries(dir: &std::path::Path, filter: &str) -> Vec<String> {
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(filter) {
                let full = dir.join(&name);
                let display = full.to_string_lossy().to_string();
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    results.push(format!("{}/", display));
                } else {
                    results.push(display);
                }
            }
        }
    }
    results.sort();
    results
}

/// A flat character buffer + cursor for multi-line editing. All methods are pure
/// (no terminal access) and unit-tested in [`tests`].
#[derive(Default)]
pub struct LineEditor {
    buf: Vec<char>,
    cursor: usize, // char index into `buf`
}

impl LineEditor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole buffer (used by history navigation). Cursor → end.
    pub fn set(&mut self, s: &str) {
        self.buf = s.chars().collect();
        self.cursor = self.buf.len();
    }

    /// Replace the char range `[start, end)` with `repl` and position the
    /// cursor right after the replacement text.
    pub fn replace_range(&mut self, start: usize, end: usize, repl: &str) {
        let start = start.min(self.buf.len());
        let end = end.min(self.buf.len());
        if start > end {
            return;
        }
        let new_chars: Vec<char> = repl.chars().collect();
        // Remove old range, insert new chars at `start`.
        self.buf.splice(start..end, new_chars.iter().copied());
        // Position cursor after the replacement.
        self.cursor = start + new_chars.len();
    }

    /// Current buffer as a `String` (newlines preserved).
    pub fn text(&self) -> String {
        self.buf.iter().collect()
    }

    pub fn insert(&mut self, c: char) {
        self.buf.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.buf.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Character immediately before the cursor (`None` at buffer start).
    pub fn char_before_cursor(&self) -> Option<char> {
        if self.cursor == 0 {
            return None;
        }
        self.buf.get(self.cursor - 1).copied()
    }

    pub fn backspace(&mut self) {
        // Delete the character immediately before the cursor (standard editor
        // behavior), so backspace works correctly when the cursor is in the
        // middle of the line. No-op at the buffer start.
        if self.cursor > 0 {
            self.buf.remove(self.cursor - 1);
            self.cursor -= 1;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    pub fn right(&mut self) {
        if self.cursor < self.buf.len() {
            self.cursor += 1;
        }
    }

    /// Move to the start of the current logical line.
    pub fn home(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.buf[i - 1] != '\n' {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Move to the end of the current logical line.
    pub fn end(&mut self) {
        let n = self.buf.len();
        let mut i = self.cursor;
        while i < n && self.buf[i] != '\n' {
            i += 1;
        }
        self.cursor = i;
    }

    /// Move back one word (stopping at whitespace/line boundaries).
    pub fn word_left(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.buf[i - 1].is_whitespace() {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Move forward one word.
    pub fn word_right(&mut self) {
        let n = self.buf.len();
        let mut i = self.cursor;
        while i < n && self.buf[i].is_whitespace() {
            i += 1;
        }
        while i < n && !self.buf[i].is_whitespace() {
            i += 1;
        }
        self.cursor = i;
    }

    pub fn insert_newline(&mut self) {
        self.buf.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Delete from the cursor to the start of the logical line (`Ctrl-U`).
    /// When the cursor is already at the start of a line (right after a `'\n'`),
    /// this clears the preceding newline and the whole previous line, so
    /// successive `Ctrl-U` presses walk backward one logical line at a time.
    pub fn delete_line_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Start of the current logical line (after the preceding '\n', or buffer start).
        let line_start = {
            let mut i = self.cursor;
            while i > 0 && self.buf[i - 1] != '\n' {
                i -= 1;
            }
            i
        };
        if line_start < self.cursor {
            // There is content before the cursor on this line: delete it.
            self.buf.drain(line_start..self.cursor);
            self.cursor = line_start;
            return;
        }
        // Cursor is at the start of a line that follows a '\n': clear the
        // preceding newline and the whole previous line so the next press can
        // continue backward.
        let prev_start = {
            let mut i = self.cursor - 1; // step past the '\n'
            while i > 0 && self.buf[i - 1] != '\n' {
                i -= 1;
            }
            i
        };
        self.buf.drain(prev_start..self.cursor);
        self.cursor = prev_start;
    }

    /// Delete from the cursor to the end of the logical line (`Ctrl-K`).
    pub fn delete_line_end(&mut self) {
        let n = self.buf.len();
        let mut end = self.cursor;
        while end < n && self.buf[end] != '\n' {
            end += 1;
        }
        if end > self.cursor {
            self.buf.drain(self.cursor..end);
        }
    }

    /// Move up one logical line (to the end of the previous line). Returns
    /// `false` when already on the first line (caller may navigate history).
    pub fn move_up_line(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let mut i = self.cursor;
        while i > 0 && self.buf[i - 1] != '\n' {
            i -= 1;
        }
        if i == 0 {
            return false;
        }
        self.cursor = i - 1; // just before the preceding '\n'
        true
    }

    /// Move down one logical line (to the end of the next line). Returns
    /// `false` when already on the last line (caller may navigate history).
    pub fn move_down_line(&mut self) -> bool {
        let n = self.buf.len();
        if self.cursor >= n {
            return false;
        }
        let mut i = self.cursor;
        while i < n && self.buf[i] != '\n' {
            i += 1;
        }
        if i == n {
            return false;
        }
        // jump to the end of the next logical line
        let mut j = i + 1;
        while j < n && self.buf[j] != '\n' {
            j += 1;
        }
        self.cursor = j;
        true
    }

    /// Move up one *visual* row (wrapped line), keeping the column when
    /// possible. Returns `false` when already on the first row (caller may
    /// navigate history). Unlike [`move_up_line`], this moves by soft-wrapped
    /// row, so the cursor lands on the character directly above.
    pub fn visual_up(&mut self, width: usize, prompt: &str) -> bool {
        let (ranges, crow, ccol) = self.visual_rows(width, prompt);
        if ranges.is_empty() || crow == 0 {
            return false;
        }
        let (s, e) = ranges[crow - 1];
        let len = e.saturating_sub(s);
        let new = s + ccol.min(len);
        if new == self.cursor {
            return false;
        }
        self.cursor = new;
        true
    }

    /// Move down one *visual* row (wrapped line), keeping the column when
    /// possible. Returns `false` when already on the last row (caller may
    /// navigate history).
    pub fn visual_down(&mut self, width: usize, prompt: &str) -> bool {
        let (ranges, crow, ccol) = self.visual_rows(width, prompt);
        if ranges.is_empty() || crow + 1 >= ranges.len() {
            return false;
        }
        let (s, e) = ranges[crow + 1];
        let len = e.saturating_sub(s);
        let new = s + ccol.min(len);
        if new == self.cursor {
            return false;
        }
        self.cursor = new;
        true
    }

    /// Buffer index ranges `(start, end_exclusive)` for each visual row across
    /// the whole buffer (logical lines concatenated), plus the cursor's
    /// `(row, col_within_row)` position. Pure.
    fn visual_rows(&self, width: usize, prompt: &str) -> (Vec<(usize, usize)>, usize, usize) {
        let width = width.max(1);
        let pcols = prompt.chars().count();
        let n = self.buf.len();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut cursor_row = 0usize;
        let mut cursor_col = 0usize; // col within the row's content
        let mut cursor_set = false;
        let mut line_start = 0usize;
        let mut i = 0usize;
        let mut row_offset = 0usize;
        while i <= n {
            if i == n || self.buf[i] == '\n' {
                let line: Vec<char> = self.buf[line_start..i].to_vec();
                let first_avail = if line_start == 0 {
                    width.saturating_sub(pcols)
                } else {
                    width
                };
                let coff = if !cursor_set && self.cursor >= line_start && self.cursor <= i {
                    Some(self.cursor - line_start)
                } else {
                    None
                };
                let (ranges_partial, cpos) = wrap_line(&line, first_avail, width, coff);
                for r in &ranges_partial {
                    ranges.push((line_start + r.0, line_start + r.1));
                }
                if let Some((cr, cc)) = cpos {
                    cursor_row = row_offset + cr;
                    cursor_col = cc;
                    cursor_set = true;
                }
                row_offset += ranges_partial.len();
                if i == n {
                    break;
                }
                i += 1; // consume '\n'
                line_start = i;
            } else {
                i += 1;
            }
        }
        (ranges, cursor_row, cursor_col)
    }

    /// Compute the visual layout of the buffer at `width` columns, with `prompt`
    /// occupying the first visual row's prefix. Pure + unit-tested.
    pub fn layout(&self, width: usize, prompt: &str) -> Layout {
        let width = width.max(1);
        let pcols = prompt.chars().count();
        let mut rows: Vec<String> = Vec::new();
        let mut cursor_row = 0usize;
        let mut cursor_col = 0usize;
        let mut cursor_set = false;
        let n = self.buf.len();
        let mut line_start = 0usize;
        let mut i = 0usize;
        let mut row_offset = 0usize;
        while i <= n {
            if i == n || self.buf[i] == '\n' {
                let line: Vec<char> = self.buf[line_start..i].to_vec();
                let first_avail = if line_start == 0 {
                    width.saturating_sub(pcols)
                } else {
                    width
                };
                let coff = if !cursor_set && self.cursor >= line_start && self.cursor <= i {
                    Some(self.cursor - line_start)
                } else {
                    None
                };
                let (lrows, cpos) = wrap_line(&line, first_avail, width, coff);
                for (ri, range) in lrows.iter().enumerate() {
                    rows.push(line[range.0..range.1].iter().collect());
                    if let Some((cr, cc)) = cpos {
                        if cr == ri {
                            cursor_row = row_offset + cr;
                            cursor_col = if row_offset + cr == 0 { pcols + cc } else { cc };
                            cursor_set = true;
                        }
                    }
                }
                row_offset += lrows.len();
                if i == n {
                    break;
                }
                i += 1; // consume '\n'
                line_start = i;
            } else {
                i += 1;
            }
        }
        let row_count = rows.len();
        Layout {
            rows,
            cursor_row,
            cursor_col,
            row_count,
        }
    }
}

/// Visual layout of a [`LineEditor`] buffer.
pub struct Layout {
    /// Content-only visual rows (the prompt is rendered separately on row 0).
    pub rows: Vec<String>,
    /// Visual row (0-based) the cursor is on.
    pub cursor_row: usize,
    /// Absolute column (0-based) the cursor is on, counting the prompt on row 0.
    pub cursor_col: usize,
    /// Total visual rows.
    pub row_count: usize,
}

/// Word-boundary wrap of one logical line (no `'\n'`). Returns, for each
/// visual row, the `(start, end_exclusive)` char range **within `line`**, and if
/// `cursor` is `Some`, the `(row, col)` where that char lands (`col` is the
/// column within the row's *content*, not counting any prompt prefix).
/// Long words and CJK (no spaces) are char-wrapped; inter-word spaces are
/// break opportunities. Leading whitespace (indentation) at the start of a
/// line is preserved and counted toward the column/wrap width, so the cursor
/// offset matches the on-screen text. Only the *trailing* break space at a
/// wrap point is dropped (not carried to the next row's start).
fn wrap_line(
    line: &[char],
    first_width: usize,
    width: usize,
    cursor: Option<usize>,
) -> (Vec<(usize, usize)>, Option<(usize, usize)>) {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut cur_start = 0usize; // start char index (within `line`) of current visual row
    let mut col = 0usize;
    let mut row = 0usize;
    let mut cursor_pos: Option<(usize, usize)> = None;
    let avail = |r: usize| if r == 0 { first_width } else { width };
    let n = line.len();
    let mut i = 0usize;
    while i < n {
        let c = line[i];
        if c == ' ' {
            if cursor == Some(i) {
                cursor_pos = Some((row, col));
            }
            if col == 0 {
                // Leading whitespace (indentation). Keep it in the rendered row
                // (cur_start is unchanged) but count it toward the column so the
                // cursor offset and wrap width match what's actually on screen.
                col += 1;
                i += 1;
                continue;
            }
            if col + 1 > avail(row) {
                // The trailing space is dropped and the next row starts here.
                ranges.push((cur_start, i));
                row += 1;
                col = 0;
                cur_start = i + 1;
                i += 1;
                continue;
            }
            col += 1;
            i += 1;
            continue;
        }
        // Measure the next word (run of non-space).
        let mut j = i;
        while j < n && line[j] != ' ' {
            j += 1;
        }
        let wlen = j - i;
        if col == 0 {
            if wlen <= avail(row) {
                if let Some(cu) = cursor {
                    if i <= cu && cu < j {
                        cursor_pos = Some((row, col + (cu - i)));
                    }
                }
                col += wlen;
                i = j;
            } else {
                let mut k = i;
                while k < j {
                    if col >= avail(row) {
                        ranges.push((cur_start, k));
                        row += 1;
                        col = 0;
                        cur_start = k;
                    }
                    if cursor == Some(k) {
                        cursor_pos = Some((row, col));
                    }
                    col += 1;
                    k += 1;
                }
                i = j;
            }
        } else if col + wlen <= avail(row) {
            if let Some(cu) = cursor {
                if i <= cu && cu < j {
                    cursor_pos = Some((row, col + (cu - i)));
                }
            }
            col += wlen;
            i = j;
        } else {
            ranges.push((cur_start, i));
            row += 1;
            col = 0;
            cur_start = i;
            continue; // re-process the word at the start of the new row
        }
    }
    if cursor == Some(n) {
        cursor_pos = Some((row, col));
    }
    ranges.push((cur_start, n));
    (ranges, cursor_pos)
}

/// Build the escape sequence that restores the saved input-top, clears down,
/// redraws `prompt` + rows, and positions the cursor. Pure string builder.
/// Content of the input region (prompt + visual rows), CRLF-separated, with
/// no cursor positioning or other terminal escapes. Composed by the editor
/// redraw and by [`render_input`].
fn render_content(layout: &Layout, prompt: &str) -> String {
    let mut s = String::new();
    let rc = layout.row_count.max(1);
    for r in 0..rc {
        if r > 0 {
            // Bare `\n`: `OPOST`/`ONLCR` (re-enabled in `TermGuard::start`) turns it
            // into `\r\n`, so writing `\r\n` here would become `\r\r\n`.
            s.push('\n');
        }
        if r == 0 {
            s.push_str(prompt);
        }
        if r < layout.rows.len() {
            s.push_str(&layout.rows[r]);
        }
    }
    s
}

/// Cursor-positioning suffix that places the caret at `layout.cursor_row` /
/// `layout.cursor_col` after [`render_content`] has been written.
/// `prompt` is needed to account for the prompt prefix when the cursor is on
/// the only row (rc == 1), which always includes the prompt.  Without it the
/// left‑arrow adjustment would be off by the prompt width, placing the cursor
/// too far right on the first character.
fn cursor_positioning(layout: &Layout, prompt: &str) -> String {
    let rc = layout.row_count.max(1);
    let pcols = prompt.chars().count();
    let last_row_len = if rc == 1 {
        layout
            .rows
            .first()
            .map(|x| x.chars().count())
            .unwrap_or(0)
    } else {
        layout.rows.last().map(|x| x.chars().count()).unwrap_or(0)
    };
    let mut s = String::new();
    let move_up = (rc - 1).saturating_sub(layout.cursor_row);
    if move_up > 0 {
        s.push_str(&format!("\x1b[{}A", move_up));
        s.push('\r');
        // Only emit CUF when cursor_col > 0; some terminals treat
        // CSI 0 C (or missing param) as 1, which would shift the cursor
        // one column to the right when we want column 0.
        if layout.cursor_col > 0 {
            s.push_str(&format!("\x1b[{}C", layout.cursor_col));
        }
    } else {
        // When the cursor is on the last (or only) row and the prompt prefix
        // is present on row 0 (rc == 1), the distance from the rendered end
        // to the cursor column must include the prompt prefix.
        let effective_end_col = if rc == 1 {
            pcols + last_row_len
        } else {
            last_row_len
        };
        let d = effective_end_col.saturating_sub(layout.cursor_col);
        if d > 0 {
            s.push_str(&format!("\x1b[{}D", d));
        }
    }
    s
}

/// Turn-grouping classification for a session entry in the /tree list.
enum TurnKind {
    /// A user prompt starts (or re-opens) a turn.
    User,
    /// Assistant content belongs to the open turn (indented +1).
    Assistant,
    /// Compaction entries close the turn scope.
    Boundary,
}

fn turn_kind(entry: &SessionEntry) -> TurnKind {
    match &entry.content {
        SessionEntryContent::Message { message } => {
            match message.get("role").and_then(|r| r.as_str()) {
                Some("user") => TurnKind::User,
                Some("assistant") => TurnKind::Assistant,
                _ => TurnKind::Assistant, // toolResult: invisible, keeps state
            }
        }
        SessionEntryContent::Compaction { .. } => TurnKind::Boundary,
        SessionEntryContent::ModelChange { .. } => TurnKind::Boundary,
        _ => TurnKind::Assistant, // meta entries: invisible, keeps state
    }
}

/// Recursively search the tree for `leaf_id` and collect all ancestor IDs
/// (including the leaf itself) into `out`. Returns `true` if found in this
/// subtree (short-circuits further searching).
fn collect_active_path(
    nodes: &[SessionTreeNode],
    leaf_id: &str,
    out: &mut std::collections::HashSet<String>,
) -> bool {
    for node in nodes {
        if node.entry.id == leaf_id
            || collect_active_path(&node.children, leaf_id, out)
        {
            out.insert(node.entry.id.clone());
            return true;
        }
    }
    false
}

/// `render_input` is only used by the unit tests below; gate it so the
/// non-test build doesn't warn about an unused function.
#[cfg(test)]
fn render_input(layout: &Layout, prompt: &str) -> String {
    let mut s = String::from("\x1b[u\r\x1b[0J");
    s.push_str(&render_content(layout, prompt));
    s.push_str(&cursor_positioning(layout, prompt));
    s
}

/// Persistent input-line history (in-memory + appended to
/// `~/.config/picocode/history`). `Up`/`Down` navigate; the in-progress line is
/// restored when navigating back past the newest entry.
pub struct History {
    entries: Vec<String>,
    file: Option<PathBuf>,
}

impl History {
    pub fn load() -> Self {
        let file = history_path();
        let mut entries = Vec::new();
        if let Some(f) = &file {
            if let Ok(s) = std::fs::read_to_string(f) {
                for line in s.lines() {
                    if !line.trim().is_empty() {
                        entries.push(line.to_string());
                    }
                }
            }
        }
        Self { entries, file }
    }

    /// Append a submitted line (skips empty) to memory + disk.
    pub fn add(&mut self, line: &str) {
        let line = line.trim_end_matches('\n').to_string();
        if line.trim().is_empty() {
            return;
        }
        self.entries.push(line.clone());
        if let Some(f) = &self.file {
            if let Some(parent) = f.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            use std::io::Write;
            if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
                let _ = fh.write_all(line.as_bytes());
                let _ = fh.write_all(b"\n");
            }
        }
    }

    /// Step backward through history; first call stashes `current` as the draft.
    pub fn prev(&self, current: &str, nav: &mut Option<usize>, draft: &mut String) -> Option<String> {
        let n = self.entries.len();
        if n == 0 {
            return None;
        }
        if nav.is_none() {
            *draft = current.to_string();
            *nav = Some(n); // point just past the newest entry
        }
        let i = nav.unwrap();
        if i == 0 {
            return None;
        }
        let new_i = i - 1;
        *nav = Some(new_i);
        Some(self.entries[new_i].clone())
    }

    /// Step forward; past the newest entry restores the stashed `draft`.
    pub fn next(&self, nav: &mut Option<usize>, draft: &mut String) -> Option<String> {
        let n = self.entries.len();
        match nav {
            None => None,
            Some(i) => {
                if *i + 1 >= n {
                    let d = std::mem::take(draft);
                    *nav = None;
                    return Some(d);
                }
                let new_i = *i + 1;
                *nav = Some(new_i);
                Some(self.entries[new_i].clone())
            }
        }
    }
}

fn history_path() -> Option<PathBuf> {
    crate::platform::history_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Phase 2: string-hierarchy parsing (pure) ---
    #[test]
    fn parse_settings_modes() {
        // empty -> list, no filter
        assert_eq!(Repl::parse_settings(""), (None, String::new(), false));
        // Partial prefixes do NOT open the sub-menu: they stay in list mode
        // (the user must type the full name or Tab-complete it).
        assert_eq!(Repl::parse_settings("d"), (None, "d".to_string(), false));
        assert_eq!(Repl::parse_settings("def"), (None, "def".to_string(), false));
        assert_eq!(Repl::parse_settings("sec"), (None, "sec".to_string(), false));
        assert_eq!(Repl::parse_settings("default"), (None, "default".to_string(), false));
        assert_eq!(Repl::parse_settings("default_"), (None, "default_".to_string(), false));
        // single token that resolves -> value mode, empty value
        assert_eq!(
            Repl::parse_settings("default_model"),
            (Some(crate::settings::SettingKey::DefaultModel), String::new(), true)
        );
        // two tokens -> value mode with value query
        assert_eq!(
            Repl::parse_settings("default_model openrouter:foo"),
            (
                Some(crate::settings::SettingKey::DefaultModel),
                "openrouter:foo".to_string(),
                true
            )
        );
        // unresolved single token -> list mode (filtered)
        assert_eq!(
            Repl::parse_settings("bogus"),
            (None, "bogus".to_string(), false)
        );
        // multi-word value query preserved with spaces
        assert_eq!(
            Repl::parse_settings("default_persona my models/x y"),
            (
                Some(crate::settings::SettingKey::DefaultPersona),
                "my models/x y".to_string(),
                true
            )
        );
        // `show_reasoning` resolves to its own key in value mode.
        assert_eq!(
            Repl::parse_settings("show_reasoning indicator"),
            (
                Some(crate::settings::SettingKey::ShowReasoning),
                "indicator".to_string(),
                true
            )
        );
    }

    #[test]
    fn model_candidate_has_provider_prefix() {
        // Flat id (no nested path): provider:model, value == display.
        let e = model_catalog::ModelEntry {
            provider: "anthropic".into(),
            id: "anthropic:claude-3-5-sonnet".into(),
            display: "anthropic:claude-3-5-sonnet".into(),
            context_length: None,
            prompt_cost: None,
            completion_cost: None,
            reasoning: false,
            cost_source: model_catalog::CostSource::OpenRouterEstimate,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        let c = Repl::model_candidate(&e);
        assert_eq!(c.value, "anthropic:claude-3-5-sonnet");
        assert_eq!(c.display, c.value);
        // Nested OpenRouter id keeps the full model path after the colon.
        // `parse_openrouter` stores the raw id *without* the `openrouter/`
        // prefix (the provider is carried separately), so the qualified form
        // is `openrouter:deepseek/deepseek-v4-flash`.
        let e2 = model_catalog::ModelEntry {
            provider: "openrouter".into(),
            id: "openrouter:deepseek/deepseek-v4-flash".into(),
            display: "openrouter:deepseek/deepseek-v4-flash".into(),
            context_length: None,
            prompt_cost: None,
            completion_cost: None,
            reasoning: false,
            cost_source: model_catalog::CostSource::OpenRouterEstimate,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        let c2 = Repl::model_candidate(&e2);
        assert_eq!(c2.value, "openrouter:deepseek/deepseek-v4-flash");
        assert!(c2.value.starts_with("openrouter:"), "value must carry the provider: prefix");
    }

    #[test]
    fn replace_last_token_replaces_tail() {
        assert_eq!(Repl::replace_last_token("/set default_model x:y", "ABC"), "/set default_model ABC");
        assert_eq!(Repl::replace_last_token("/model gpt", "gpt-4"), "/model gpt-4");
        // no space -> whole string replaced
        assert_eq!(Repl::replace_last_token("/set", "X"), "X");
    }

    #[test]
    fn modifier_overlay_bare_dot_lists_all_names() {
        let (rows, want_value) = Repl::modifier_overlay_entries(".");
        assert!(!want_value, "bare dot lists modifier names, not values");
        let tokens: Vec<&str> = rows.iter().map(|c| c.value.as_str()).collect();
        assert!(tokens.contains(&".sort:"), "sort present: {:?}", tokens);
        assert!(tokens.contains(&".price:"), "price present: {:?}", tokens);
        assert!(tokens.contains(&".context:"), "context present: {:?}", tokens);
        // Every row is a `.name:` token.
        assert!(rows.iter().all(|c| c.value.starts_with('.') && c.value.ends_with(':')));
    }

    #[test]
    fn modifier_overlay_partial_name_filters() {
        let (rows, want_value) = Repl::modifier_overlay_entries(".so");
        assert!(!want_value);
        // fuzzy matches `.sort:` (and possibly no others) — `so` matches sort.
        assert!(rows.iter().any(|c| c.value == ".sort:"), "rows: {:?}", rows.iter().map(|c| &c.value).collect::<Vec<_>>());
    }

    #[test]
    fn modifier_overlay_enum_values_after_colon() {
        let (rows, want_value) = Repl::modifier_overlay_entries(".sort:");
        assert!(want_value, "enumerated modifier with colon → value phase");
        let values: Vec<String> = rows.iter().map(|c| c.value.clone()).collect();
        assert!(values.contains(&".sort:most-popular".to_string()));
        assert!(values.contains(&".sort:coding".to_string()));
        assert!(values.contains(&".sort:price".to_string()));
    }

    #[test]
    fn modifier_overlay_enum_values_filtered() {
        let (rows, want_value) = Repl::modifier_overlay_entries(".sort:pri");
        assert!(want_value);
        assert!(!rows.is_empty(), "rows not empty");
        // Query narrows the enumerated values to the price-related ones.
        assert!(rows.iter().any(|c| c.value == ".sort:price"));
        assert!(rows.iter().all(|c| c.value.contains("price")), "values filtered: {:?}", rows.iter().map(|c| &c.value).collect::<Vec<_>>());
        // Unrelated options (e.g. created) are filtered out.
        assert!(!rows.iter().any(|c| c.value == ".sort:created"));
    }

    #[test]
    fn modifier_overlay_freeform_after_colon_hands_back() {
        // `.coding:` is a free-form numeric modifier → no value phase (empty rows).
        let (rows, want_value) = Repl::modifier_overlay_entries(".coding:");
        assert_eq!(want_value, false);
        assert!(rows.is_empty(), "free-form modifier past colon offers nothing: {:?}", rows);
        // But a bare `.coding` shows the single name as a hint.
        let (rows, want_value) = Repl::modifier_overlay_entries(".coding");
        assert!(!want_value);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, ".coding:");
    }

    #[test]
    fn modifier_overlay_prefix_searches_across_names() {
        // `.p` fuzzy-matches price_input, price_output, price, and more.
        let (rows, _) = Repl::modifier_overlay_entries(".p");
        assert!(!rows.is_empty());
        assert!(rows.iter().any(|c| c.value == ".price:"));
    }

    #[test]
    fn parse_model_filter_remains_data_driven() {
        // `.sort:coding` consumed into the filter, not the search terms.
        let (search, f) = Repl::parse_model_filter(".sort:coding");
        assert_eq!(search.trim(), "");
        assert_eq!(f.sort.as_deref(), Some("coding"));
        // `.price:0.5` parsed as number.
        let (_, f) = Repl::parse_model_filter(".price:0.5");
        assert_eq!(f.price, Some(0.5));
        // Empty value clears.
        let (_, f) = Repl::parse_model_filter(".sort:");
        assert_eq!(f.sort, None);
        // Free-form number captured.
        let (_, f) = Repl::parse_model_filter(".context:100");
        assert_eq!(f.min_context, Some(100));
        // Search terms still fall through.
        let (search, _) = Repl::parse_model_filter("deepseek .price:0.1");
        assert_eq!(search.trim(), "deepseek");
    }

    #[test]
    fn parse_tree_id_angle_brackets_only() {
        // Angle-bracketed → entry id.
        assert_eq!(
            Repl::parse_tree_id("<0194abc-def>"),
            Some("0194abc-def".to_string())
        );
        assert_eq!(Repl::parse_tree_id(" <x> "), Some("x".to_string()));
        // Trailing text after the id still selects the id (the trailing text
        // is the action token / sub-menu filter, not part of the id).
        assert_eq!(Repl::parse_tree_id("<x> switch"), Some("x".to_string()));
        assert_eq!(
            Repl::parse_tree_id("<0194abc> sw"),
            Some("0194abc".to_string())
        );
        assert_eq!(Repl::parse_tree_id("<x> compact truncate"), Some("x".to_string()));
        // Anything else → fuzzy search term for the entry list.
        assert_eq!(Repl::parse_tree_id("someterm"), None);
        assert_eq!(Repl::parse_tree_id(""), None);
        assert_eq!(Repl::parse_tree_id("<"), None);
        assert_eq!(Repl::parse_tree_id("<>"), None);
        assert_eq!(Repl::parse_tree_id("<unbalanced"), None);
        assert_eq!(Repl::parse_tree_id("unbalanced>"), None);
    }

    #[test]
    fn filter_tree_options_empty_and_exact() {
        let all = ["switch", "compact"];
        // Empty filter → full list, first active.
        let (opts, active) = Repl::filter_tree_options(&all, "");
        assert_eq!(opts, vec!["switch".to_string(), "compact".to_string()]);
        assert_eq!(active, Some(0));
        // Exact match pre-selects regardless of position.
        let (_, active) = Repl::filter_tree_options(&all, "compact");
        assert_eq!(active, Some(1));
        // Hyphenated label matches its spaced form.
        let all2 = ["just switch", "summarize"];
        let (_, active) = Repl::filter_tree_options(&all2, "just-switch");
        assert_eq!(active, Some(0));
    }

    #[test]
    fn filter_tree_options_fuzzy_narrows_and_orders() {
        let all2 = ["just switch", "summarize"];
        // `su` fuzzy-matches `summarize` best → first (active).
        let (opts, active) = Repl::filter_tree_options(&all2, "su");
        assert_eq!(opts[0], "summarize");
        assert_eq!(active, Some(0));
        // `ju` matches `just switch` first.
        let (opts, _) = Repl::filter_tree_options(&all2, "ju");
        assert_eq!(opts[0], "just switch");
        // No match → full list unchanged, first active.
        let (opts, active) = Repl::filter_tree_options(&all2, "zzzz");
        assert_eq!(opts.len(), 2);
        assert_eq!(active, Some(0));
    }

    /// End-to-end `from_buffer` promotion for the `/tree` menus, including
    /// the exact reported cases: `/tree <full-uuid>` (bare), `/tree <uuid>
    /// sw` (partial action token), and `/tree <uuid> switch` (exact action).
    fn tree_ctx() -> ReplCtx {
        use crate::session::{SessionEntry, SessionEntryContent};
        let mk = |id: &str, role: &str| SessionEntry {
            id: id.to_string(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": role,
                    "content": [{"type": "text", "text": format!("{} msg", role)}]
                }),
            },
        };
        let root = mk("root-entry", "user");
        let leaf = mk("leaf-entry", "assistant");
        let tree = vec![crate::session::SessionTreeNode {
            entry: root,
            children: vec![crate::session::SessionTreeNode {
                entry: leaf,
                children: vec![],
                label: None,
            }],
            label: None,
        }];
        ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree,
            leaf_id: Some("leaf-entry".into()),
            context_tokens: 1234,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        }
    }

    fn tree_overlay(buf: &str) -> ActiveOverlay {
        let ctx = tree_ctx();
        // `from_buffer` only reads settings for the /set overlays; a default
        // store is enough here.
        let settings = SettingsStore {
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
        ActiveOverlay::from_buffer(buf, &ctx, &settings, false, &[])
    }

    fn model_overlay(buf: &str) -> ActiveOverlay {
        let ctx = tree_ctx();
        let settings = SettingsStore {
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
        ActiveOverlay::from_buffer(buf, &ctx, &settings, false, &[])
    }

    #[test]
    fn from_buffer_model_bare_dot_promotes_to_modifier_overlay() {
        match model_overlay("/model .") {
            ActiveOverlay::Mod { rows, want_value } => {
                assert!(!want_value, "bare dot => modifier names");
                assert!(!rows.is_empty());
                assert!(rows.iter().any(|c| c.value == ".sort:"));
            }
            other => panic!("expected Mod for '/model .', got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_model_sort_colon_promotes_to_value_overlay() {
        match model_overlay("/model .sort:") {
            ActiveOverlay::Mod { rows, want_value } => {
                assert!(want_value, "`.sort:` => enumerated value phase");
                assert!(rows.iter().any(|c| c.value == ".sort:coding"));
            }
            other => panic!("expected Mod for '/model .sort:', got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_model_plain_filter_stays_in_list() {
        // A non-dotted filter must NOT promote to the modifier overlay.
        match model_overlay("/model deep "){
            ActiveOverlay::Model { .. } => {}
            other => panic!("expected Model for '/model deep ', got disc {:?}", other.overlay_disc()),
        }
    }

    #[tokio::test]
    async fn from_buffer_model_complete_modifier_returns_to_list() {
        // A fully-specified `.name:value` has nothing left to complete → the
        // modifier overlay hands back to the list (which the user Enters).
        match model_overlay("/model .sort:coding"){
            ActiveOverlay::Model { .. } => {}
            other => panic!("expected Model for complete '.sort:coding', got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_bare_listing_command_shows_root_list() {
        // Phase 6.6 uniform listing rule (design §3): a bare listing
        // command (no trailing space) stays in the root command list,
        // pinned/highlighted — it must NOT open the command's dialog.
        for (buf, word) in [
            ("/model", "model"),
            ("/thinking", "thinking"),
            ("/set", "set"),
            ("/persona", "persona"),
            ("/session", "session"),
            ("/tree", "tree"),
            ("/sandbox", "sandbox"),
        ] {
            match model_overlay(buf) {
                ActiveOverlay::CommandList { engine_rows, engine_active, .. } => {
                    // The pinned command is the cursor row: the row whose
                    // id equals the command must be active.
                    assert!(
                        engine_rows.get(engine_active).map(|r| r.id.as_str())
                            == Some(word),
                        "expected cursor on pinned {word:?}, got active={engine_active} rows={:?}",
                        engine_rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
                    );
                }
                other => panic!(
                    "expected root CommandList for bare {buf:?} (listing rule), \
                     got disc {:?}",
                    other.overlay_disc()
                ),
            }
            let _ = word;
        }
    }

    #[test]
    fn from_buffer_trailing_space_opens_listing_dialog() {
        // The same command WITH a trailing space drills into its dialog.
        match model_overlay("/model ") {
            ActiveOverlay::Model { .. } => {}
            other => panic!(
                "expected Model for '/model ' (drill in), got disc {:?}",
                other.overlay_disc()
            ),
        }
        match model_overlay("/set ") {
            ActiveOverlay::Settings { .. } => {}
            other => panic!(
                "expected Settings for '/set ' (drill in), got disc {:?}",
                other.overlay_disc()
            ),
        }
        match model_overlay("/thinking ") {
            ActiveOverlay::Thinking { .. } => {}
            other => panic!(
                "expected Thinking for '/thinking ' (drill in), got disc {:?}",
                other.overlay_disc()
            ),
        }
    }

    #[test]
    fn from_buffer_tree_id_promotes_to_action_menu() {
        let ov = tree_overlay("/tree <root-entry>");
        match ov {
            ActiveOverlay::TreeAction { target_id, opts, active, .. } => {
                assert_eq!(target_id, "root-entry");
                assert_eq!(opts, vec!["switch".to_string(), "compact".to_string()]);
                assert_eq!(active, Some(0));
            }
            other => panic!("expected TreeAction, got {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_tree_id_partial_action_token_fuzzy_filters() {
        // `/tree <uuid> sw` — reported bug: must show the action menu
        // narrowed to `switch`, NOT the entry list with "no matches".
        let ov = tree_overlay("/tree <root-entry> sw");
        match ov {
            ActiveOverlay::TreeAction { target_id, opts, .. } => {
                assert_eq!(target_id, "root-entry");
                assert_eq!(opts, vec!["switch".to_string()]);
            }
            other => panic!("expected TreeAction (fuzzy-filtered), got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn tree_entry_header_user_message_preserves_newlines_and_drops_id() {
        // The header for a user-message entry shows the FULL body (with
        // newlines preserved), wrapped to the terminal width. The entry
        // id is intentionally absent — it is already in the prompt.
        let body = "line one\nline two\nline three";
        let entry = SessionEntry {
            id: "u1".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": body}]
                }),
            },
        };
        let mut header = Vec::new();
        Repl::tree_entry_header(Some(&entry), "stale label (ignored for messages)", usize::MAX, &mut header);
        // Newlines preserved → one header row per line.
        assert_eq!(header.len(), 3, "got: {:?}", header);
        assert_eq!(header[0], "line one");
        assert_eq!(header[1], "line two");
        assert_eq!(header[2], "line three");
        // Id never appears in the header.
        assert!(!header.iter().any(|l| l.contains("u1")), "got: {:?}", header);
    }

    #[test]
    fn tree_entry_header_long_message_truncates_with_inline_ellipsis() {
        // 25 lines, capped at 10: header has exactly 10 rows, the last
        // ends with `…` so the user knows the preview is incomplete. No
        // separate truncation row (that would steal one of the menu's
        // reserved lines).
        let body: String = (1..=25)
            .map(|i| format!("line{}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let entry = SessionEntry {
            id: "u1".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": body}]
                }),
            },
        };
        let mut header = Vec::new();
        Repl::tree_entry_header(Some(&entry), "label", 10, &mut header);
        assert_eq!(header.len(), 10, "got: {:?}", header);
        // First 9 rows are unmodified message lines (line1..line9).
        for (i, row) in header.iter().take(9).enumerate() {
            assert_eq!(*row, format!("line{}", i + 1), "row {i}");
        }
        // Last row ends with the ellipsis marker.
        assert!(
            header[9].ends_with('…'),
            "expected trailing ellipsis, got: {:?}",
            header[9]
        );
        assert!(
            header[9].starts_with("line10"),
            "expected last visible line to start with the truncated content, got: {:?}",
            header[9]
        );
    }

    #[test]
    fn tree_entry_header_under_cap_no_ellipsis() {
        // 3-line message under the cap: no ellipsis, all rows intact.
        let body = "a\nb\nc";
        let entry = SessionEntry {
            id: "u1".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": body}]
                }),
            },
        };
        let mut header = Vec::new();
        Repl::tree_entry_header(Some(&entry), "label", 10, &mut header);
        assert_eq!(header, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        // No ellipsis anywhere — message fit under the cap.
        assert!(header.iter().all(|l| !l.ends_with('…')));
    }

    #[test]
    fn tree_preview_line_budget_reserves_room_for_menu_items() {
        // The budget is `2/3 * term_h` minus the title (1) and the menu
        // rows. On a 24-row terminal that's 16 - 1 - opts_count. With
        // 2 opts (the bare /tree <id> menu) that's 13 header rows — enough
        // for typical prompts.
        let budget = Repl::tree_preview_line_budget(2);
        let term_h = term_height();
        let max_pop = ((term_h as f64 * 2.0 / 3.0).ceil() as usize).max(1);
        assert_eq!(budget, max_pop.saturating_sub(1).saturating_sub(2).max(1));
        // Floor: at least 1 even on a tiny / bizarre terminal.
        assert!(budget >= 1);
        // More menu rows ⇒ smaller preview budget.
        assert!(Repl::tree_preview_line_budget(2) >= Repl::tree_preview_line_budget(5));
    }

    #[test]
    fn tree_entry_header_assistant_message_concatenates_text_segments() {
        // Multi-segment assistant content (common with tool-call-only
        // turns split into multiple text blocks) joins with newlines so
        // paragraphs stay separated in the preview.
        let entry = SessionEntry {
            id: "a1".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "first paragraph"},
                        {"type": "text", "text": "second paragraph"}
                    ]
                }),
            },
        };
        let mut header = Vec::new();
        Repl::tree_entry_header(Some(&entry), "label", usize::MAX, &mut header);
        assert_eq!(
            header,
            vec!["first paragraph".to_string(), "second paragraph".to_string()]
        );
    }

    #[test]
    fn tree_entry_header_non_message_falls_back_to_single_line_label() {
        // Non-message entries (compaction, branch summary, ...) keep the
        // existing single-line label behaviour.
        let entry = SessionEntry {
            id: "c1".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Compaction {
                summary: String::new(),
                first_kept_entry_id: "m0".into(),
                tokens_before: 42_000,
            },
        };
        let mut header = Vec::new();
        Repl::tree_entry_header(Some(&entry), "compaction ... before \"...\"", usize::MAX, &mut header);
        assert_eq!(header.len(), 1, "got: {:?}", header);
        assert!(header[0].contains("compaction"));
        assert!(!header[0].contains("c1"), "id should not appear: {:?}", header[0]);
    }

    #[test]
    fn tree_entry_header_unknown_id_renders_only_label() {
        // Defensive: if the caller can't locate the entry, fall back to
        // the precomputed single-line label.
        let mut header = Vec::new();
        Repl::tree_entry_header(None, "compaction ...", usize::MAX, &mut header);
        assert_eq!(header, vec!["compaction ...".to_string()]);
    }

    #[test]
    fn from_buffer_tree_id_action_menu_shows_full_message_body() {
        // The TreeAction overlay for a user message: the header now
        // contains the multi-line body, not the previous single-line
        // summary.
        let body = "first line\nsecond line";
        let entry = SessionEntry {
            id: "root-entry".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": body}]
                }),
            },
        };
        // Build a fresh ctx whose root entry carries the multi-line body.
        let tree = vec![crate::session::SessionTreeNode {
            entry,
            children: vec![],
            label: None,
        }];
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree,
            leaf_id: Some("root-entry".into()),
            context_tokens: 1234,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        };
        let settings = SettingsStore {
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
        let ov = ActiveOverlay::from_buffer("/tree <root-entry>", &ctx, &settings, false, &[]);
        match ov {
            ActiveOverlay::TreeAction { header, opts, .. } => {
                // Body preserved: two header lines, one per source line.
                assert_eq!(header.len(), 2, "got: {:?}", header);
                assert_eq!(header[0], "first line");
                assert_eq!(header[1], "second line");
                // No id, no stale single-line summary.
                assert!(!header.iter().any(|l| l.contains("root-entry")));
                // Menu still wired up.
                assert_eq!(opts, vec!["switch".to_string(), "compact".to_string()]);
            }
            other => panic!("expected TreeAction, got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_tree_id_switch_menu_shows_full_message_body() {
        // Same expectation for the SwitchAction sub-menu: it shares the
        // header builder.
        let body = "alpha\nbeta\ngamma";
        let entry = SessionEntry {
            id: "root-entry".into(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": "user",
                    "content": [{"type": "text", "text": body}]
                }),
            },
        };
        let tree = vec![crate::session::SessionTreeNode {
            entry,
            children: vec![],
            label: None,
        }];
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree,
            leaf_id: Some("root-entry".into()),
            context_tokens: 1234,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        };
        let settings = SettingsStore {
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
        let ov = ActiveOverlay::from_buffer("/tree <root-entry> switch", &ctx, &settings, false, &[]);
        match ov {
            ActiveOverlay::SwitchAction { header, opts, .. } => {
                assert_eq!(header.len(), 3, "got: {:?}", header);
                assert_eq!(header, vec!["alpha", "beta", "gamma"]);
                assert_eq!(
                    opts,
                    vec!["just switch".to_string(), "summarize".to_string()]
                );
            }
            other => panic!("expected SwitchAction, got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_tree_id_exact_action_promotes_to_submenu() {
        // `/tree <uuid> switch` — reported bug: must open the SwitchAction
        // sub-menu, NOT show "no matches".
        let ov = tree_overlay("/tree <root-entry> switch");
        match ov {
            ActiveOverlay::SwitchAction { target_id, opts, active, .. } => {
                assert_eq!(target_id, "root-entry");
                assert_eq!(opts, vec!["just switch".to_string(), "summarize".to_string()]);
                assert_eq!(active, Some(0)); // `just switch` pre-selected
            }
            other => panic!("expected SwitchAction, got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_tree_id_compact_promotes_to_compact_summary() {
        let ov = tree_overlay("/tree <root-entry> compact");
        match ov {
            ActiveOverlay::CompactSummary { target_id, opts, active, header, .. } => {
                assert_eq!(target_id, "root-entry");
                assert_eq!(opts, vec!["manual".to_string(), "text only".to_string(), "llm".to_string()]);
                assert_eq!(active, Some(0));
                // Info header present: entry row + dropped/kept/token rows.
                assert!(!header.is_empty());
                let joined = header.join("\n");
                assert!(joined.contains("root-entry"));
                assert!(joined.contains("tokens"));
                assert!(joined.contains("drops"));
            }
            other => panic!("expected CompactSummary, got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_tree_id_compact_llm_with_payload_promotes_to_compact_summary() {
        // When the buffer already carries a fully-formed `compact llm
        // <payload>` line, the overlay re-opens as CompactSummary with
        // `llm` pre-selected (the first-word promotion in `from_buffer`
        // overrides the empty SkimMatcher score list for the long
        // payload). The handler then detects trailing text and commits
        // + submits.
        let ov = tree_overlay("/tree <root-entry> compact llm Summarize this");
        match ov {
            ActiveOverlay::CompactSummary { target_id, opts, active, _filter, .. } => {
                assert_eq!(target_id, "root-entry");
                // First-word promotion picks `llm` regardless of the
                // fuzzy score (which is empty for a long payload).
                assert_eq!(active, Some(2));
                assert_eq!(opts[active.unwrap()], "llm");
            }
            other => panic!("expected CompactSummary, got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn llm_compact_buffer_is_canonical_detects_payload() {
        // Buffer with trailing payload is canonical.
        assert!(Repl::llm_compact_buffer_is_canonical(
            "/tree <e1> compact llm Summarize this",
            "e1",
        ));
        // Whitespace-only payload (single space) is not canonical.
        assert!(!Repl::llm_compact_buffer_is_canonical(
            "/tree <e1> compact llm ",
            "e1",
        ));
        // Just `compact llm` (no trailing text) is not canonical.
        assert!(!Repl::llm_compact_buffer_is_canonical(
            "/tree <e1> compact llm",
            "e1",
        ));
        // Different target id — not canonical for this target.
        assert!(!Repl::llm_compact_buffer_is_canonical(
            "/tree <e2> compact llm Summarize this",
            "e1",
        ));
        // `/compact` head does not match a `/tree` buffer.
        // (After Phase 7, `/compact` is rewritten to
        // `/tree <last_user_id> compact <mode>` at the overlay layer,
        // so a bare `/compact` line never reaches this check.)
        assert!(!Repl::llm_compact_buffer_is_canonical(
            "/compact llm Summarize this",
            "e1",
        ));
        // Unrelated buffer.
        assert!(!Repl::llm_compact_buffer_is_canonical(
            "hello world",
            "e1",
        ));
    }

    #[test]
    fn from_buffer_tree_trailing_mode_preselects_option() {
        let ov = tree_overlay("/tree <root-entry> switch --summarize");
        match ov {
            ActiveOverlay::SwitchAction { active, .. } => {
                assert_eq!(active, Some(1)); // `summarize` pre-selected
            }
            other => panic!("expected SwitchAction, got disc {:?}", other.overlay_disc()),
        }
    }

    // ---- last_user_message_id_on_active_branch ----

            use crate::session::{SessionEntryContent, SessionHeader, SessionIndex, SessionTreeNode};

    fn mk_message(id: &str, role: &str, parent: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Message {
                message: serde_json::json!({
                    "role": role,
                    "content": [{"type": "text", "text": format!("{} msg", id)}]
                }),
            },
        }
    }

    fn mk_compaction(id: &str, parent: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Compaction {
                summary: String::new(),
                first_kept_entry_id: String::new(),
                tokens_before: 0,
            },
        }
    }

    fn node(entry: SessionEntry, children: Vec<SessionTreeNode>) -> SessionTreeNode {
        SessionTreeNode { entry, children, label: None }
    }

    #[test]
    fn last_user_message_id_picks_just_before_leaf_assistant() {
        // Tree: user(u1) -> assistant(a1) -> user(u2) -> assistant(a2) = leaf
        // last_user_message_id should be u2 (the just-before-leaf user).
        let a2 = mk_message("a2", "assistant", Some("u2"));
        let u2 = mk_message("u2", "user", Some("a1"));
        let a1 = mk_message("a1", "assistant", Some("u1"));
        let u1 = mk_message("u1", "user", None);
        let tree = vec![node(
            u1,
            vec![node(a1, vec![node(u2, vec![node(a2, vec![])])])],
        )];
        assert_eq!(
            Repl::last_user_message_id_on_active_branch(&tree, "a2"),
            Some("u2".to_string())
        );
    }

    #[test]
    fn last_user_message_id_returns_leaf_when_leaf_is_user() {
        // Mid-turn: leaf is a user prompt (no response yet). The leaf
        // itself is the most recent user message.
        let u2 = mk_message("u2", "user", Some("a1"));
        let a1 = mk_message("a1", "assistant", Some("u1"));
        let u1 = mk_message("u1", "user", None);
        let tree = vec![node(
            u1,
            vec![node(a1, vec![node(u2, vec![])])],
        )];
        assert_eq!(
            Repl::last_user_message_id_on_active_branch(&tree, "u2"),
            Some("u2".to_string())
        );
    }

    #[test]
    fn last_user_message_id_returns_root_when_only_user() {
        // Single user message, no assistant response yet.
        let u1 = mk_message("u1", "user", None);
        let tree = vec![node(u1, vec![])];
        assert_eq!(
            Repl::last_user_message_id_on_active_branch(&tree, "u1"),
            Some("u1".to_string())
        );
    }

    #[test]
    fn last_user_message_id_none_when_no_user_message() {
        // Empty / non-message-only branch: thinking-level change or
        // compaction entry as the only nodes.
        let t = SessionEntry {
            id: "t1".to_string(),
            parent_id: None,
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::ThinkingLevelChange {
                thinking_level: "low".to_string(),
            },
        };
        let tree = vec![node(t, vec![])];
        assert_eq!(
            Repl::last_user_message_id_on_active_branch(&tree, "t1"),
            None
        );
    }

    #[test]
    fn last_user_message_id_walks_past_compaction_entries() {
        // Compaction entries are not user messages; walk past them.
        // Tree: user(u1) -> compaction(c1) -> user(u2) = leaf.
        let u2 = mk_message("u2", "user", Some("c1"));
        let c1 = mk_compaction("c1", Some("u1"));
        let u1 = mk_message("u1", "user", None);
        let tree = vec![node(
            u1,
            vec![node(c1, vec![node(u2, vec![])])],
        )];
        assert_eq!(
            Repl::last_user_message_id_on_active_branch(&tree, "u2"),
            Some("u2".to_string())
        );
    }

    // ---- `/compact` overlay resolution ----

    #[test]
    fn from_buffer_compact_resolves_target_to_last_user_message() {
        // The /compact overlay must auto-resolve target_id to the
        // last user-role message, NOT the current leaf. The header
        // also re-anchors to the resolved target's label.
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree: vec![node(
                mk_message("u1", "user", None),
                vec![node(
                    mk_message("a1", "assistant", Some("u1")),
                    vec![node(mk_message("u2", "user", Some("a1")), vec![])],
                )],
            )],
            leaf_id: Some("u2".into()), // mid-turn: leaf IS a user message
            context_tokens: 1234,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        };
        let settings = SettingsStore {
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
        let ov = ActiveOverlay::from_buffer("/compact", &ctx, &settings, false, &[]);
        match ov {
            ActiveOverlay::CompactSummary { target_id, .. } => {
                assert_eq!(
                    target_id, "u2",
                    "/compact should resolve to the most recent user message (the leaf when leaf is user)"
                );
            }
            other => panic!("expected CompactSummary, got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_compact_resolves_to_user_before_assistant_leaf() {
        // Most common case: leaf is the assistant's last response.
        // /compact should resolve to the user prompt that came
        // immediately before it.
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree: vec![node(
                mk_message("u1", "user", None),
                vec![node(
                    mk_message("a1", "assistant", Some("u1")),
                    vec![node(mk_message("u2", "user", Some("a1")), vec![
                        node(mk_message("a2", "assistant", Some("u2")), vec![]),
                    ])],
                )],
            )],
            leaf_id: Some("a2".into()),
            context_tokens: 1234,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        };
        let settings = SettingsStore {
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
        let ov = ActiveOverlay::from_buffer("/compact", &ctx, &settings, false, &[]);
        match ov {
            ActiveOverlay::CompactSummary { target_id, .. } => {
                assert_eq!(
                    target_id, "u2",
                    "/compact should resolve to the user message just before the assistant leaf"
                );
            }
            other => panic!("expected CompactSummary, got disc {:?}", other.overlay_disc()),
        }
    }

    #[test]
    fn from_buffer_compact_no_overlay_when_no_user_message() {
        // Empty session (no user message): the overlay does not open.
        // Mirrors the existing no-leaf behavior.
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree: vec![],
            leaf_id: None,
            context_tokens: 0,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        };
        let settings = SettingsStore {
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
        let ov = ActiveOverlay::from_buffer("/compact", &ctx, &settings, false, &[]);
        assert!(
            matches!(ov, ActiveOverlay::None),
            "expected no overlay when there is no user message"
        );
    }

    #[test]
    fn from_buffer_tree_searchterm_stays_in_list() {
        // Bare search terms (no angle brackets) must NOT promote — they stay
        // in the fuzzy entry list (existing `/tree <searchterm>` behavior).
        let ov = tree_overlay("/tree user");
        match ov {
            ActiveOverlay::Tree { targets, .. } => {
                assert!(targets.contains(&"root-entry".to_string()));
            }
            other => panic!("expected Tree list, got disc {:?}", other.overlay_disc()),
        }
        // Unknown angle-bracketed id → falls through to the list too.
        let ov = tree_overlay("/tree <does-not-exist>");
        assert!(matches!(ov, ActiveOverlay::Tree { .. }));
    }

    // ------------------------------------------------------------------
    // /tree list rendering: turn grouping, no tools, compaction scope
    // ------------------------------------------------------------------

    fn tree_entry(id: &str, parent: Option<&str>, role: &str, text: &str) -> SessionTreeNode {
        SessionTreeNode {
            entry: SessionEntry {
                id: id.into(),
                parent_id: parent.map(|p| p.into()),
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": role,
                        "content": [{"type": "text", "text": text}]
                    }),
                },
            },
            children: vec![],
            label: None,
        }
    }

    fn assistant_with_tools(
        id: &str,
        parent: Option<&str>,
        text: Option<&str>,
        tool_name: Option<&str>,
    ) -> SessionTreeNode {
        let mut content = vec![];
        if let Some(t) = text {
            content.push(serde_json::json!({"type": "text", "text": t}));
        }
        if let Some(name) = tool_name {
            content.push(serde_json::json!({
                "type": "toolCall", "name": name,
                "arguments": {"path": "x.rs"}
            }));
        }
        SessionTreeNode {
            entry: SessionEntry {
                id: id.into(),
                parent_id: parent.map(|p| p.into()),
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "assistant",
                        "content": content
                    }),
                },
            },
            children: vec![],
            label: None,
        }
    }

    fn tool_result(id: &str, parent: Option<&str>) -> SessionTreeNode {
        SessionTreeNode {
            entry: SessionEntry {
                id: id.into(),
                parent_id: parent.map(|p| p.into()),
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "toolResult",
                        "content": [{"type": "text", "text": "tool output"}]
                    }),
                },
            },
            children: vec![],
            label: None,
        }
    }

    fn compaction(id: &str, parent: Option<&str>, kept: &str) -> SessionTreeNode {
        SessionTreeNode {
            entry: SessionEntry {
                id: id.into(),
                parent_id: parent.map(|p| p.into()),
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::Compaction {
                    summary: String::new(),
                    first_kept_entry_id: kept.into(),
                    tokens_before: 42_000,
                },
            },
            children: vec![],
            label: None,
        }
    }

    fn attach(mut parent: SessionTreeNode, child: SessionTreeNode) -> SessionTreeNode {
        parent.children.push(child);
        parent
    }

    #[test]
    fn tree_list_skips_tools_and_groups_turns() {
        // Linear chain: m0 -> a0(text+toolCall) -> t0 -> a1(text) -> m1.
        // (Real sessions are linear parent chains, not nested forks.)
        let chain = attach(
            tree_entry("m0", None, "user", "first prompt"),
            attach(
                assistant_with_tools("a0", Some("m0"), Some("partial answer"), Some("read")),
                attach(
                    tool_result("t0", Some("a0")),
                    attach(
                        assistant_with_tools("a1", Some("t0"), Some("final answer"), None),
                        tree_entry("m1", Some("a1"), "user", "second prompt"),
                    ),
                ),
            ),
        );
        let tree = vec![chain];

        let mut out = Vec::new();
        Repl::flatten_tree_nodes(&tree, 0, Some("m1"), &std::collections::HashSet::new(), &mut out);

        // Visible rows: user prompt, the assistant's text before AND after
        // the tool call (both are response text), then the second user
        // prompt. NO tool-call/tool-result rows.
        let labels: Vec<String> = out.iter().map(|(_, l, _, _, _, _)| l.clone()).collect();
        assert_eq!(labels.len(), 4, "got: {:?}", labels);
        assert!(labels[0].contains("first prompt"));
        assert!(labels[1].contains("partial answer"));
        assert!(labels[2].contains("final answer"));
        assert!(labels[3].contains("second prompt"));
        let all = labels.join("\n");
        assert!(!all.contains("read"));
        assert!(!all.contains("tool output"));
        // Indent: assistant rows are TWO chars deeper than their user
        // prompt (the model response sits visibly under its prompt, not
        // flush with it); the indent persists across the invisible
        // tool-call/tool-result chain between the two text segments.
        let indents: Vec<usize> = out.iter().map(|(i, _, _, _, _, _)| *i).collect();
        assert_eq!(indents, vec![0, 2, 2, 0], "got: {:?}", out);
    }

    #[test]
    fn tree_list_tool_only_assistant_segment_is_suppressed() {
        // Assistant message with a tool call but NO text → no visible row.
        let with_call = attach(
            assistant_with_tools("a0", Some("m0"), None, Some("read")),
            tool_result("t0", Some("a0")),
        );
        let tree = vec![attach(
            tree_entry("m0", None, "user", "prompt"),
            attach(with_call, tree_entry("m1", Some("t0"), "assistant", "done")),
        )];
        let mut out = Vec::new();
        Repl::flatten_tree_nodes(&tree, 0, Some("m1"), &std::collections::HashSet::new(), &mut out);
        let labels: Vec<String> = out.iter().map(|(_, l, _, _, _, _)| l.clone()).collect();
        assert_eq!(labels.len(), 2, "got: {:?}", labels);
        assert!(labels[0].contains("prompt"));
        assert!(labels[1].contains("done"));
        assert_eq!(out[1].0, 2); // still part of the same turn (indent+2)
    }

    #[test]
    fn tree_list_compaction_shows_kept_text() {
        // Kept entry (m1) is a SIBLING elsewhere in the tree — the real
        // case: firstKeptEntryId points BEFORE the compaction, so the label
        // map must resolve it across the whole tree (regression: a subtree-
        // only search always missed it and showed "(kept entry not on
        // visible path)").
        let tree = vec![attach(
            tree_entry("m0", None, "user", "kept prompt"),
            attach(
                tree_entry("a0", Some("m0"), "assistant", "old answer"),
                compaction("c1", Some("a0"), "m0"), // keeps m0 (the root)
            ),
        )];
        let mut out = Vec::new();
        Repl::flatten_tree_nodes(&tree, 0, Some("c1"), &std::collections::HashSet::new(), &mut out);
        let labels: Vec<String> = out.iter().map(|(_, l, _, _, _, _)| l.clone()).collect();
        let compaction_row = labels
            .iter()
            .find(|l| l.contains("\x1b[33m") && l.contains("compaction"))
            .expect("compaction row must be visible (yellow-coloured)");
        assert!(compaction_row.contains("m0"));
        assert!(compaction_row.contains("before \"kept prompt\""));
    }

    #[test]
    fn tree_list_branch_indent_is_four() {
        // a0 forks: two children -> each child indented +4 (over the
        // assistant response at indent+2, that's two more; over a user
        // prompt at the same indent, that's four — the two rules land
        // on the same value because the response now sits 2 chars in).
        let b1 = tree_entry("m1", Some("a0"), "user", "branch one");
        let b2 = tree_entry("m2", Some("a0"), "user", "branch two");
        let fork = SessionTreeNode {
            entry: SessionEntry {
                id: "a0".into(),
                parent_id: Some("m0".into()),
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "text", "text": "fork point"}]
                    }),
                },
            },
            children: vec![b1, b2],
            label: None,
        };
        let tree = vec![attach(tree_entry("m0", None, "user", "prompt"), fork)];
        let mut out = Vec::new();
        Repl::flatten_tree_nodes(&tree, 0, Some("m2"), &std::collections::HashSet::new(), &mut out);
        let branch_rows: Vec<usize> = out
            .iter()
            .filter(|(_, l, _, _, _, _)| l.contains("branch"))
            .map(|(i, _, _, _, _, _)| *i)
            .collect();
        assert_eq!(branch_rows, vec![4, 4]); // base 2 (assistant in turn) + branch indent 4
        // The fork point itself stays at turn depth (+2: assistant in turn).
        let fork_indent = out
            .iter()
            .find(|(_, l, _, _, _, _)| l.contains("fork point"))
            .map(|(i, _, _, _, _, _)| *i)
            .unwrap();
        assert_eq!(fork_indent, 2);
    }

    #[test]
    fn tree_list_compaction_and_model_change_are_yellow() {
        // Special-action entries (compactions, model switches) use a
        // yellow FG (`\x1b[33m`) so they stand out from the regular
        // user / assistant rows in the tree list.
        let comp = SessionEntry {
            id: "c1".into(),
            parent_id: Some("a0".into()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::Compaction {
                summary: String::new(),
                first_kept_entry_id: "m0".into(),
                tokens_before: 42_000,
            },
        };
        let comp_label = Repl::entry_display_label_ext(
            &comp,
            None,
            &std::collections::HashMap::new(),
        );
        assert!(comp_label.starts_with("\x1b[33m"), "got: {comp_label:?}");
        // Ends with `\x1b[39m` (default FG) so the row's BG carries
        // through the trailing padding, not `\x1b[0m` (full reset).
        assert!(comp_label.ends_with("\x1b[39m"), "got: {comp_label:?}");
        assert!(comp_label.contains("compaction"));

        let model_change = SessionEntry {
            id: "mc1".into(),
            parent_id: Some("a0".into()),
            timestamp: "2025-01-01T00:00:00.000Z".into(),
            content: SessionEntryContent::ModelChange {
                provider: "openrouter".into(),
                model_id: "anthropic/claude-3.7".into(),
            },
        };
        let mc_label = Repl::entry_display_label(&model_change, None);
        assert!(mc_label.starts_with("\x1b[33m"), "got: {mc_label:?}");
        assert!(mc_label.ends_with("\x1b[39m"), "got: {mc_label:?}");
        assert!(mc_label.contains("model: openrouter/anthropic/claude-3.7"));
    }

    #[test]
    fn tree_list_model_change_is_unindented() {
        // Model changes are user-initiated (the user picked a
        // different model for the next turn) so they should sit at
        // the same indent as the user prompt that just closed — the
        // same way compactions do, and the same way any other turn
        // boundary sits. Previously a ModelChange was classified as
        // TurnKind::Assistant, so the row inherited the model-
        // response +2 indent and looked like a continuation of the
        // just-finished turn.
        let mc = SessionTreeNode {
            entry: SessionEntry {
                id: "mc".into(),
                parent_id: Some("a0".into()),
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::ModelChange {
                    provider: "openrouter".into(),
                    model_id: "anthropic/claude-3.7".into(),
                },
            },
            children: vec![tree_entry("m1", Some("mc"), "user", "after switch")],
            label: None,
        };
        let tree = vec![attach(
            tree_entry("m0", None, "user", "first prompt"),
            attach(
                tree_entry("a0", Some("m0"), "assistant", "first answer"),
                mc,
            ),
        )];
        let mut out = Vec::new();
        Repl::flatten_tree_nodes(&tree, 0, Some("m1"), &std::collections::HashSet::new(), &mut out);
        let indents: Vec<usize> = out.iter().map(|(i, _, _, _, _, _)| *i).collect();
        // m0=0, a0=2, mc=0 (Boundary, un-indented), m1=0 (User, un-indented).
        assert_eq!(indents, vec![0, 2, 0, 0], "got: {out:?}");
    }

    #[test]
    fn tree_row_bg_alternates_by_indent_level() {
        // branch_depth == 0 (the root conversation) gets None — the
        // row stays on the default panel background, no tint.
        assert_eq!(Repl::tree_row_bg(0), None);
        // Depth 1: one step below the panel (235 -> 234).
        assert_eq!(Repl::tree_row_bg(1), Some("\x1b[48;5;234m"));
        // Depth 2: two steps below the panel (235 -> 233).
        assert_eq!(Repl::tree_row_bg(2), Some("\x1b[48;5;233m"));
        // Depth 3: back to one step below for the depth alternation.
        assert_eq!(Repl::tree_row_bg(3), Some("\x1b[48;5;234m"));
        // Depth 4: two steps below again.
        assert_eq!(Repl::tree_row_bg(4), Some("\x1b[48;5;233m"));
        // Depth 5 wraps back to depth 1.
        assert_eq!(Repl::tree_row_bg(5), Some("\x1b[48;5;234m"));
    }

    #[test]
    fn tree_overlay_keeps_descendants_past_session_info() {
        // Regression test for the user-reported bug: a `SessionInfo`
        // entry in the middle of the active path used to strand every
        // descendant below it, because `build_subtree` returned
        // `None` for `SessionInfo` (a meta entry) and dropped its
        // children along with it. The user saw a /tree list that
        // stopped at the SessionInfo, with the rest of the session
        // (compaction entries, post-compaction content) gone.
        //
        // This test builds a synthetic SessionIndex shaped like the
        // failing one: a linear chain m0 -> a0 -> SI (SessionInfo)
        // -> MC (model_change) -> CMP (compaction) -> m1 (user) -> a1
        // (assistant) -> tr (toolResult, the leaf). The leaf points
        // at a toolResult, so the visible active row is the previous
        // user prompt.
        //
        // After the fix, build_tree produces a tree where the
        // SessionInfo is present but its children are spliced
        // directly into the parent, so the full chain — including
        // the compaction and the post-compaction content — reaches
        // `tree_overlay_entries` and the active row is set to a
        // visible ancestor of the leaf.
        use std::collections::HashMap;

        // Build a SessionIndex with the entries.
        let mut entries: HashMap<String, SessionEntry> = HashMap::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();

        let chain: Vec<(&str, &str, SessionEntryContent)> = vec![
            (
                "m0",
                "",
                SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "user",
                        "content": [{"type": "text", "text": "first prompt"}]
                    }),
                },
            ),
            (
                "a0",
                "m0",
                SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "text", "text": "first answer"}]
                    }),
                },
            ),
            (
                "si",
                "a0",
                SessionEntryContent::SessionInfo {
                    name: Some("synthetic".into()),
                },
            ),
            (
                "mc",
                "si",
                SessionEntryContent::ModelChange {
                    provider: "test".into(),
                    model_id: "m".into(),
                },
            ),
            (
                "cmp",
                "mc",
                SessionEntryContent::Compaction {
                    summary: String::new(),
                    first_kept_entry_id: "m0".into(),
                    tokens_before: 42_000,
                },
            ),
            (
                "m1",
                "cmp",
                SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "user",
                        "content": [{"type": "text", "text": "after compaction"}]
                    }),
                },
            ),
            (
                "a1",
                "m1",
                SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "text", "text": "after compaction answer"}]
                    }),
                },
            ),
            (
                "tr",
                "a1",
                SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "toolResult",
                        "content": [{"type": "text", "text": "ok"}]
                    }),
                },
            ),
        ];
        for (id, parent, content) in &chain {
            let entry = SessionEntry {
                id: (*id).into(),
                parent_id: if parent.is_empty() {
                    None
                } else {
                    Some((*parent).into())
                },
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: content.clone(),
            };
            entries.insert((*id).into(), entry);
            if !parent.is_empty() {
                children
                    .entry((*parent).into())
                    .or_default()
                    .push((*id).into());
            }
        }
        let index = SessionIndex {
            header: SessionHeader::new(std::path::Path::new("/test")),
            entries,
            leaf_id: Some("tr".into()),
            children,
            session_name: None,
        };
        let tree = index.build_tree(Some("tr"));

        // Walk the tree and confirm every entry is reachable from
        // the root. Before the fix, descendants of `si` were missing
        // because build_subtree returned None for the SessionInfo
        // and dropped its children along with it.
        fn collect_ids<'a>(
            nodes: &'a [SessionTreeNode],
            out: &mut std::collections::HashSet<&'a str>,
        ) {
            for n in nodes {
                out.insert(&n.entry.id);
                collect_ids(&n.children, out);
            }
        }
        let mut tree_ids = std::collections::HashSet::new();
        collect_ids(&tree, &mut tree_ids);
        for expected in &["m0", "a0", "si", "mc", "cmp", "m1", "a1", "tr"] {
            assert!(
                tree_ids.contains(*expected),
                "{expected} missing from tree after SessionInfo; got: {:?}",
                tree_ids
            );
        }

        // Run tree_overlay_entries. The compaction must be present
        // (yellow label) and the active row must be set.
        let mut rows = Vec::new();
        let mut row_bgs: Vec<Option<&'static str>> = Vec::new();
        let mut targets = Vec::new();
        let mut active = None;
        Repl::tree_overlay_entries(
            &tree,
            Some("tr"),
            "",
            &mut rows,
            &mut row_bgs,
            &mut targets,
            &mut active,
        );
        let has_compaction = rows.iter().any(|r| r.contains("\x1b[33mcompaction"));
        assert!(
            has_compaction,
            "compaction row missing from /tree; rows: {rows:?}"
        );
        let has_post_compaction = rows.iter().any(|r| r.contains("after compaction"));
        assert!(
            has_post_compaction,
            "post-compaction content missing from /tree; rows: {rows:?}"
        );
        // The active row must be set even though the leaf itself is
        // a toolResult (invisible). The cursor needs to land on a
        // visible row so the user can find where they are.
        assert!(
            active.is_some(),
            "active row not set when leaf is invisible; rows: {rows:?}"
        );
    }

    #[test]
    fn tree_overlay_branches_render_with_visible_indent_and_tint() {
        // End-to-end visual smoke test: build a tree with a branching
        // assistant, push it through tree_overlay_entries AND the
        // overlay_lines rendering, and confirm:
        //  - the depth-1 warm tint escape appears for branch rows;
        //  - the row content does not contain \x1b[0m (which would
        //    break the BG and make the row's padding render against
        //    the terminal's default background);
        //  - the rendered output includes the indented branch labels.
        let b1 = tree_entry("m1", Some("a0"), "user", "branch one");
        let b2 = tree_entry("m2", Some("a0"), "user", "branch two");
        let fork = SessionTreeNode {
            entry: SessionEntry {
                id: "a0".into(),
                parent_id: Some("m0".into()),
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "text", "text": "fork point"}]
                    }),
                },
            },
            children: vec![b1, b2],
            label: None,
        };
        let tree = vec![attach(
            tree_entry("m0", None, "user", "first prompt"),
            fork,
        )];
        let mut rows = Vec::new();
        let mut row_bgs: Vec<Option<&'static str>> = Vec::new();
        let mut targets = Vec::new();
        let mut active = None;
        Repl::tree_overlay_entries(
            &tree,
            Some("m2"),
            "",
            &mut rows,
            &mut row_bgs,
            &mut targets,
            &mut active,
        );
        let row_bg_view: Vec<Option<&str>> =
            row_bgs.iter().map(|b| b.map(|s| -> &str { s })).collect();
        let rendered = Repl::overlay_lines_with_bgs(
            "Tree",
            &rows,
            None,
            active.unwrap_or(0),
            active,
            false,
            Some(&row_bg_view),
        );
        eprintln!("--- rendered ---\n{rendered}\n--- end ---");
        // The depth-1 warm tint escape must appear for the branch rows.
        assert!(
            rendered.contains("\x1b[48;5;234m"),
            "branch tint missing from rendered output: {rendered:?}"
        );
        // Both branch labels must reach the rendered output.
        assert!(
            rendered.contains("branch one"),
            "branch one row missing: {rendered:?}"
        );
        assert!(
            rendered.contains("branch two"),
            "branch two row missing: {rendered:?}"
        );
    }

    #[test]
    fn tree_overlay_entries_renders_row_bgs_per_indent() {
        // Two-level tree: root user prompt → assistant response → fork.
        // Row BGs should mirror the indents: root None, response ochre,
        // branch children bluish-grey.
        let b1 = tree_entry("m1", Some("a0"), "user", "branch one");
        let b2 = tree_entry("m2", Some("a0"), "user", "branch two");
        let fork = SessionTreeNode {
            entry: SessionEntry {
                id: "a0".into(),
                parent_id: Some("m0".into()),
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "text", "text": "fork point"}]
                    }),
                },
            },
            children: vec![b1, b2],
            label: None,
        };
        let tree = vec![attach(
            tree_entry("m0", None, "user", "prompt"),
            fork,
        )];
        let mut rows = Vec::new();
        let mut row_bgs: Vec<Option<&'static str>> = Vec::new();
        let mut targets = Vec::new();
        let mut active = None;
        Repl::tree_overlay_entries(
            &tree,
            Some("m2"),
            "",
            &mut rows,
            &mut row_bgs,
            &mut targets,
            &mut active,
        );
        assert_eq!(rows.len(), row_bgs.len());
        // Root user prompt (depth 0) — default panel background.
        assert_eq!(row_bgs[0], None);
        // Assistant response is still in the SAME conversation as the
        // root user prompt (no branching happened yet), so it stays on
        // the default background even though it's indented.
        assert_eq!(row_bgs[1], None);
        // Two branch children of the fork point — depth 1.
        assert_eq!(row_bgs[2], Some("\x1b[48;5;234m"));
        assert_eq!(row_bgs[3], Some("\x1b[48;5;234m"));
        // No more leading whitespace in the rendered top-level label:
        // previously `display_indent = indent + 1` prepended an extra
        // space at the top level.
        assert!(rows[0].starts_with("\x1b[22mprompt") || rows[0].starts_with("\x1b[2mprompt"));
        // Indented sub-tree is actually indented in the rendered output:
        // branch children sit 4 spaces in, the assistant response sits
        // 2 spaces in, and the top-level label has no leading space.
        assert!(
            rows[1].starts_with("\x1b[22m  ") || rows[1].starts_with("\x1b[2m  "),
            "assistant row must be 2-space indented; got: {rows:?}"
        );
        assert!(
            rows[2].starts_with("\x1b[22m    ") || rows[2].starts_with("\x1b[2m    "),
            "branch child must be 4-space indented; got: {rows:?}"
        );
        assert!(
            rows[3].starts_with("\x1b[22m    ") || rows[3].starts_with("\x1b[2m    "),
            "branch child must be 4-space indented; got: {rows:?}"
        );
    }

    #[test]
    fn tree_list_branch_depth_tracks_fork_only() {
        // branch_depth only increments at branching nodes (a node with
        // multiple children). Within an un-branched turn — even with a
        // nested model response — every row sits at the same depth, so
        // a linear conversation gets NO background tint (all rows on
        // the default panel background).
        let chain = attach(
            tree_entry("m0", None, "user", "first prompt"),
            attach(
                tree_entry("a0", Some("m0"), "assistant", "first answer"),
                attach(
                    tree_entry("m1", Some("a0"), "user", "second prompt"),
                    tree_entry("a1", Some("m1"), "assistant", "second answer"),
                ),
            ),
        );
        let tree = vec![chain];
        let mut out = Vec::new();
        Repl::flatten_tree_nodes(&tree, 0, None, &std::collections::HashSet::new(), &mut out);
        let depths: Vec<usize> = out.iter().map(|(_, _, _, _, _, d)| *d).collect();
        assert_eq!(depths, vec![0, 0, 0, 0], "got: {out:?}");

        // Forks, by contrast, push their children one depth deeper:
        // m0 → a0 (depth0) → [m1 (depth1) | m2 (depth1)]
        let b1 = tree_entry("m1", Some("a0"), "user", "branch one");
        let b2 = tree_entry("m2", Some("a0"), "user", "branch two");
        let fork = SessionTreeNode {
            entry: SessionEntry {
                id: "a0".into(),
                parent_id: Some("m0".into()),
                timestamp: "2025-01-01T00:00:00.000Z".into(),
                content: SessionEntryContent::Message {
                    message: serde_json::json!({
                        "role": "assistant",
                        "content": [{"type": "text", "text": "fork point"}]
                    }),
                },
            },
            children: vec![b1, b2],
            label: None,
        };
        let tree = vec![attach(
            tree_entry("m0", None, "user", "prompt"),
            fork,
        )];
        let mut out = Vec::new();
        Repl::flatten_tree_nodes(&tree, 0, Some("m2"), &std::collections::HashSet::new(), &mut out);
        let depths: Vec<usize> = out.iter().map(|(_, _, _, _, _, d)| *d).collect();
        // m0, a0 stay at depth 0 (the conversation so far).
        // m1 and m2 (the branches) sit at depth 1.
        assert_eq!(depths, vec![0, 0, 1, 1], "got: {out:?}");
    }

    #[test]
    fn from_buffer_tree_list_with_compaction_keeps_row_visible() {
        // Regression: a real session with a compaction entry must
        // surface the compaction as a visible row in the `/tree` LIST
        // overlay (not just the per-id action menu).
        let tree = vec![attach(
            tree_entry("m0", None, "user", "kept prompt"),
            attach(
                tree_entry("a0", Some("m0"), "assistant", "old answer"),
                attach(
                    compaction("c1", Some("a0"), "m0"),
                    tree_entry("m1", Some("c1"), "user", "after compaction"),
                ),
            ),
        )];
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree,
            leaf_id: Some("m1".into()),
            context_tokens: 1234,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        };
        let settings = SettingsStore {
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
        let ov = ActiveOverlay::from_buffer("/tree ", &ctx, &settings, false, &[]);
        let (title, opts, active, _) = ov.display_rows();
        assert!(
            opts.iter().any(|r| r.contains("compaction")),
            "compaction row missing from /tree list; got: {opts:#?}"
        );
        // The compaction id is what users will type to target it via
        // /tree <c1>, so it must be wired up as a target.
        if let ActiveOverlay::Tree { ref targets, .. } = ov {
            assert!(targets.iter().any(|t| t == "c1"), "targets: {targets:?}");
        } else {
            panic!("expected Tree overlay");
        }
        // Render the overlay (sel = active = leaf row) and confirm the
        // compaction row reaches the rendered output with its yellow
        // FG escape.
        let sel = active.expect("leaf must be set as active");
        let row_bg_view: Vec<Option<&str>> = if let ActiveOverlay::Tree { ref row_bgs, .. } = ov {
            row_bgs.iter().map(|b| b.map(|s| -> &str { s })).collect()
        } else {
            vec![]
        };
        let rendered = Repl::overlay_lines_with_bgs(
            title,
            &opts,
            None,
            sel,
            active,
            false,
            Some(&row_bg_view),
        );
        assert!(
            rendered.contains("compaction"),
            "rendered output missing the compaction row: {rendered:?}"
        );
        assert!(
            rendered.contains("\x1b[33m"),
            "compaction row not styled yellow in the rendered output: {rendered:?}"
        );
    }

    #[test]
    fn from_buffer_tree_list_compaction_as_leaf_keeps_active_marker() {
        // Variant: the LEAF itself is the compaction entry. The user
        // just ran /compact and the leaf is the resulting Compaction
        // node, with no post-compaction children. The compaction must
        // still show up and be marked as the active row.
        let tree = vec![attach(
            tree_entry("m0", None, "user", "kept prompt"),
            attach(
                tree_entry("a0", Some("m0"), "assistant", "old answer"),
                compaction("c1", Some("a0"), "m0"),
            ),
        )];
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree,
            leaf_id: Some("c1".into()),
            context_tokens: 1234,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        };
        let settings = SettingsStore {
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
        let ov = ActiveOverlay::from_buffer("/tree ", &ctx, &settings, false, &[]);
        let (_title, opts, active, _) = ov.display_rows();
        // m0, a0, c1 all reachable; c1 is the active row.
        assert!(opts.iter().any(|r| r.contains("kept prompt")), "got: {opts:#?}");
        assert!(opts.iter().any(|r| r.contains("old answer")), "got: {opts:#?}");
        assert!(opts.iter().any(|r| r.contains("compaction")), "got: {opts:#?}");
        // c1 must be the active index (the leaf).
        if let ActiveOverlay::Tree { ref targets, .. } = ov {
            let pos = targets.iter().position(|t| t == "c1").expect("c1 missing");
            assert_eq!(active, Some(pos));
        } else {
            panic!("expected Tree overlay");
        }
    }

    #[test]
    fn from_buffer_tree_list_post_compaction_chain_visible() {
        // Variant: chain with multiple post-compaction entries (the
        // real-life shape after a /compact + a few follow-up turns).
        // The user reports this shape drops the compaction AND the
        // post-compaction content; verify all of it shows.
        let tree = vec![attach(
            tree_entry("m0", None, "user", "first prompt"),
            attach(
                tree_entry("a0", Some("m0"), "assistant", "first answer"),
                attach(
                    compaction("c1", Some("a0"), "m0"),
                    attach(
                        tree_entry("m1", Some("c1"), "user", "after compact 1"),
                        attach(
                            tree_entry("a1", Some("m1"), "assistant", "after compact answer"),
                            tree_entry("m2", Some("a1"), "user", "after compact 2"),
                        ),
                    ),
                ),
            ),
        )];
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree,
            leaf_id: Some("m2".into()),
            context_tokens: 1234,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        };
        let settings = SettingsStore {
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
        let ov = ActiveOverlay::from_buffer("/tree ", &ctx, &settings, false, &[]);
        let (_title, opts, active, _) = ov.display_rows();
        // Every visible label must show.
        for needle in &["first prompt", "first answer", "compaction", "after compact 1", "after compact answer", "after compact 2"] {
            assert!(
                opts.iter().any(|r| r.contains(needle)),
                "row missing for {:?}; got: {opts:#?}",
                needle
            );
        }
        // m2 is the leaf, so it must be the active index.
        if let ActiveOverlay::Tree { ref targets, .. } = ov {
            let pos = targets.iter().position(|t| t == "m2").expect("m2 missing");
            assert_eq!(active, Some(pos));
        } else {
            panic!("expected Tree overlay");
        }
    }

    #[test]
    fn overlay_lines_selected_active_uses_star_marker() {
        // Selected AND active row shows `* `, not `> ` (so the user
        // sees "you're already here" rather than a generic cursor; the
        // cursor itself is still visible via the row highlight).
        let opts = vec!["one".to_string(), "two".to_string()];
        let out = Repl::overlay_lines("title", &opts, None, 1, Some(1), true);
        let line_with_two = out
            .lines()
            .find(|l| l.contains("two"))
            .unwrap_or("");
        assert!(
            line_with_two.contains("* two"),
            "selected active row must use `* ` not `> `: {line_with_two:?}"
        );
        assert!(!line_with_two.contains("> two"));
    }

    #[test]
    fn overlay_lines_active_not_selected_uses_star_marker() {
        // Active row that's NOT selected still gets `* ` (consistent
        // with the previous behaviour: active state lives in the
        // 2-char prefix, no separate indicator column).
        let opts = vec!["one".to_string(), "two".to_string()];
        let out = Repl::overlay_lines("title", &opts, None, 0, Some(1), true);
        let line_with_two = out
            .lines()
            .find(|l| l.contains("two"))
            .unwrap_or("");
        assert!(line_with_two.contains("* two"), "got: {line_with_two:?}");
    }

    #[test]
    fn overlay_lines_with_bgs_applies_per_row_background() {
        // Per-row BG override: row 0 default, row 1 ochre, row 2
        // bluish-grey. Selected row keeps BG_SEL regardless of the
        // override.
        let opts = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let bgs: Vec<Option<&str>> = vec![
            None,
            Some("\x1b[48;5;180m"),
            Some("\x1b[48;5;240m"),
        ];
        // sel = 0 → row 0 uses BG_SEL, others use their override.
        let out = Repl::overlay_lines_with_bgs(
            "title",
            &opts,
            None,
            0,
            None,
            true,
            Some(&bgs),
        );
        // Selected row uses BG_SEL (\x1b[48;5;239m) and *not* the row's
        // BG override (which is None here, so BG_SEL is also the
        // fallback).
        let line0 = out.lines().nth(1).unwrap_or("");
        assert!(line0.contains("\x1b[48;5;239m"), "got: {line0:?}");
        // Row 1 (non-selected) uses its ochre override.
        let line1 = out.lines().nth(2).unwrap_or("");
        assert!(line1.contains("\x1b[48;5;180m"), "got: {line1:?}");
        // Row 2 uses bluish-grey.
        let line2 = out.lines().nth(3).unwrap_or("");
        assert!(line2.contains("\x1b[48;5;240m"), "got: {line2:?}");
    }

    #[test]
    fn cursor_sel_policy_seeds_on_open_when_seed_enabled() {
        // Menu opens (disc change) with a buffer-derived default row (the
        // `/tree <id> compact llm <payload>` case → `active` = llm index 2).
        // The cursor must land there, not on row 0.
        let (sel, _last) = Repl::cursor_sel_policy(
            Some(2), true, true, true, None, 0,
        );
        assert_eq!(sel, 2);
    }

    #[test]
    fn cursor_sel_policy_open_without_seed_keeps_zero() {
        // Value pickers (model etc.) don't seed from `active` on open.
        let (sel, _last) = Repl::cursor_sel_policy(
            Some(5), true, false, false, None, 0,
        );
        assert_eq!(sel, 0);
    }

    #[test]
    fn cursor_sel_policy_follows_active_change_within_menu() {
        // Same menu, but the user typed a mode token so `active` moved from
        // 0 (truncate default) to 2 (llm). The cursor follows.
        let (sel, last) = Repl::cursor_sel_policy(
            Some(2), false, true, true, Some(0), 0,
        );
        assert_eq!(sel, 2);
        assert_eq!(last, Some(2));
    }

    #[test]
    fn cursor_sel_policy_arrow_navigation_is_preserved() {
        // `active` unchanged (the buffer did not change) → the cursor stays
        // where the user arrowed it (e.g. sel=0 while active is still 2).
        let (sel, last) = Repl::cursor_sel_policy(
            Some(2), false, true, true, Some(2), 0,
        );
        assert_eq!(sel, 0);
        assert_eq!(last, Some(2));
    }

    #[test]
    fn cursor_sel_policy_tree_list_seeds_on_open_but_does_not_follow_filters() {
        // /tree list: seed the cursor on the current leaf when the list
        // opens (seed_on_open = true)…
        let (sel, last) = Repl::cursor_sel_policy(
            Some(4), true, true, false, None, 0,
        );
        assert_eq!(sel, 4);
        assert_eq!(last, Some(4));
        // …but typing a filter that moves `active` must NOT yank the cursor
        // (follow_changes = false).
        let (sel, _last) = Repl::cursor_sel_policy(
            Some(1), false, true, false, Some(4), 4,
        );
        assert_eq!(sel, 4);
    }

    #[test]
    fn compact_llm_enter_submits_canonical_buffer_when_llm_selected() {
        // Simulates restoring `/tree <id> compact llm <payload>` from history
        // and pressing Enter. from_buffer pre-selects llm (active index 2);
        // cursor_sel_policy seeds the cursor onto it; handle_enter must then
        // commit + submit the canonical buffer verbatim (NOT truncate).
        let buf = "/tree <root-entry> compact llm Summarize this";
        let ov = tree_overlay(buf);
        let (_title, opts, active, _) = ov.display_rows();
        let sel = Repl::cursor_sel_policy(active, true, true, true, None, 0).0;
        assert_eq!(active, Some(2));
        assert_eq!(sel, 2);
        let label = opts.get(sel).expect("opts row").as_str();
        assert_eq!(label, "llm");

        let mut editor = LineEditor::from_text(buf);
        let settings = SettingsStore {
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
        let mut settings = settings;
        let output = crate::output::NoOutput;
        let redraw = |_: &LineEditor, _: &[&str], _: usize, _: &mut usize| {};
        let mut input_row = 0usize;
        let mut dirty = false;
        let mut value_error: Option<String> = None;
        let mut help_view = false;
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree: vec![],
            leaf_id: None,
            context_tokens: 0,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: None,
        };
        let shared = MenuShared::new(settings.clone(), ctx.repo.clone());
        let mut engine = slash_menu_bridge::MenuEngine::new(&shared);
        let action = ov.handle_enter(
            sel,
            &mut editor,
            &output,
            &redraw,
            &mut input_row,
            &mut settings,
            &mut dirty,
            &mut value_error,
            &mut help_view,
            &ctx,
            &mut engine,
        );
        match action {
            OverlayAction::Action(InputAction::Submit(line)) => {
                assert_eq!(line, buf);
            }
            _ => panic!("expected Submit(buf)"),
        }
    }

    #[test]
    fn compact_llm_tab_from_active_row_stages_default_prompt() {
        // Typing `/tree <id> compact llm ` (no payload): the mode is
        // pre-selected (active=llm) but the buffer is not yet canonical, so
        // handle_enter stages the default prompt for editing (per
        // doc/tree_compaction.md §6.7) instead of submitting.
        let buf = "/tree <root-entry> compact llm";
        let ov = tree_overlay(buf);
        let (_title, opts, active, _) = ov.display_rows();
        let sel = Repl::cursor_sel_policy(active, true, true, true, None, 0).0;
        assert_eq!(active, Some(2));
        assert_eq!(opts[sel], "llm");

        let mut editor = LineEditor::from_text(buf);
        let settings = SettingsStore {
            default_model: "p:m".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_thinking_level: "off".into(),
            show_reasoning: "indicator".into(),
            reasoning_in_context: false,
            session_resume_replay_turns: 3,
            llm_compact_prompt: "default summarize".into(),
            path: None,
        };
        let mut settings = settings;
        let output = crate::output::NoOutput;
        let redraw = |_: &LineEditor, _: &[&str], _: usize, _: &mut usize| {};
        let mut input_row = 0usize;
        let mut dirty = false;
        let mut value_error: Option<String> = None;
        let mut help_view = false;
        let ctx = ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree: vec![],
            leaf_id: None,
            context_tokens: 0,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: None,
        };
        let shared = MenuShared::new(settings.clone(), ctx.repo.clone());
        let mut engine = slash_menu_bridge::MenuEngine::new(&shared);
        let action = ov.handle_enter(
            sel,
            &mut editor,
            &output,
            &redraw,
            &mut input_row,
            &mut settings,
            &mut dirty,
            &mut value_error,
            &mut help_view,
            &ctx,
            &mut engine,
        );
        match action {
            OverlayAction::Continue => {
                assert_eq!(editor.text(), "/tree <root-entry> compact llm default summarize");
            }
            _ => panic!("expected Continue (stage default prompt)"),
        }
    }

    #[test]
    fn complete_command_single_slash() {
        for c in slash::registry() {
            let s = Repl::complete_command(c);
            assert!(s.starts_with('/'), "command completion must keep its leading slash: {:?}", s);
            assert!(!s.starts_with("//"), "command completion must not double the slash: {:?}", s);
            assert!(s.ends_with(' '), "command completion should open the arg slot: {:?}", s);
        }
        // the reported bug case: `/` + Tab must yield `/help`, not `//help`.
        let help = slash::registry().iter().find(|c| c.name == "/help").unwrap();
        assert_eq!(Repl::complete_command(help), "/help ");
    }

    #[test]
    fn insert_and_text() {
        let mut e = LineEditor::new();
        for c in "ab".chars() {
            e.insert(c);
        }
        assert_eq!(e.text(), "ab");
        assert_eq!(e.cursor, 2);
    }

    #[test]
    fn backspace_deletes_char_before_cursor() {
        let mut e = LineEditor::new();
        e.insert('a');
        e.insert('b');
        e.home(); // cursor at start; backspace is a no-op
        assert_eq!(e.cursor, 0);
        e.backspace();
        assert_eq!(e.text(), "ab");
        assert_eq!(e.cursor, 0);
        // non-empty buffer, cursor at start -> still no-op
        let mut e2 = LineEditor::new();
        e2.backspace();
        assert_eq!(e2.text(), "");
    }

    #[test]
    fn delete_at_boundary_noop() {
        let mut e = LineEditor::new();
        e.insert('a');
        e.delete(); // at end -> no change
        assert_eq!(e.text(), "a");
        assert_eq!(e.cursor, 1);
    }

    #[test]
    fn char_before_cursor_returns_preceding_char() {
        let mut e = LineEditor::new();
        assert_eq!(e.char_before_cursor(), None);
        e.insert_str("ab");
        assert_eq!(e.char_before_cursor(), Some('b'));
        e.left();
        assert_eq!(e.char_before_cursor(), Some('a'));
    }

    #[test]
    fn backslash_then_enter_inserts_newline() {
        // Mirrors pi: a '\\' immediately before Enter inserts a newline
        // (Kitty-independent multi-line entry) instead of submitting.
        let mut e = LineEditor::new();
        e.insert_str("line one\\");
        assert_eq!(e.char_before_cursor(), Some('\\'));
        e.backspace(); // drop the backslash
        e.insert_newline();
        assert_eq!(e.text(), "line one\n");
        assert_eq!(e.cursor, 9);
    }

    #[test]
    fn backspace_deletes_char_before_cursor_in_middle() {
        let mut e = LineEditor::new();
        e.insert_str("abc");
        e.left(); // cursor in the middle (after 'b')
        e.backspace(); // deletes 'b', not the final 'c'
        assert_eq!(e.text(), "ac");
        assert_eq!(e.cursor, 1);
    }

    #[test]
    fn visual_up_down_moves_by_wrapped_row() {
        // A long line that wraps at width 7: "aaa bbb ccc" -> rows
        // ["aaa bbb", "ccc"]. Cursor at end (col 3 of row 1, past "ccc").
        // Up -> character directly above at col 3 on row 0 = the space (index 3).
        let mut e = LineEditor::new();
        e.insert_str("aaa bbb ccc");
        assert_eq!(e.cursor, 11);
        assert!(e.visual_up(7, ""));
        assert_eq!(e.cursor, 3);
        // Down -> back to end of buffer.
        assert!(e.visual_down(7, ""));
        assert_eq!(e.cursor, 11);
        // stays on last row -> false
        assert!(!e.visual_down(7, ""));
    }

    #[test]
    fn visual_up_down_clamps_column_on_shorter_row() {
        // "aaaaaaa bbb" at width 7 wraps to ["aaaaaaa", " bbb"] (row 0 is
        // 7 chars, cols 0-6). Cursor at end (row1 col3). Up -> col 3 on row 0
        // (still within its content) = index 3 (the 4th 'a').
        let mut e = LineEditor::new();
        e.insert_str("aaaaaaa bbb");
        assert_eq!(e.cursor, 11);
        assert!(e.visual_up(7, ""));
        assert_eq!(e.cursor, 3);
    }

    #[test]
    fn visual_up_down_crosses_hard_newlines() {
        // Two short lines, no wrap: "ab\ncd". cursor at end of line 2 (index 5).
        // Up -> line 1 end (index 2).
        let mut e = LineEditor::new();
        e.insert_str("ab\ncd");
        assert_eq!(e.cursor, 5);
        assert!(e.visual_up(80, ""));
        assert_eq!(e.cursor, 2);
        assert!(e.visual_down(80, ""));
        assert_eq!(e.cursor, 5);
    }

    #[test]
    fn cursor_move_across_lines() {
        let mut e = LineEditor::new();
        e.insert_str("a\nb");
        assert_eq!(e.cursor, 3);
        assert!(e.move_up_line());
        assert_eq!(e.cursor, 1); // end of "a"
        assert!(e.move_down_line());
        assert_eq!(e.cursor, 3);
        // already on last line -> false
        assert!(!e.move_down_line());
    }

    #[test]
    fn word_boundaries() {
        let mut e = LineEditor::new();
        e.insert_str("hello world");
        assert_eq!(e.cursor, 11);
        e.word_left();
        assert_eq!(e.cursor, 6); // start of "world"
        e.word_left();
        assert_eq!(e.cursor, 0);
        e.word_right();
        assert_eq!(e.cursor, 5); // end of "hello"
    }

    #[test]
    fn newline_and_line_delete() {
        let mut e = LineEditor::new();
        e.insert_str("foo\nbar"); // cursor 7 (end of "bar")
        e.home(); // start of "bar" line -> 4
        e.right(); // 5 (between b|a)
        e.delete_line_start(); // "foo\nar", cursor 4
        assert_eq!(e.text(), "foo\nar");
        assert_eq!(e.cursor, 4);
        e.right(); // 5 (between a|r)
        e.delete_line_end(); // "foo\na", cursor at end (5)
        assert_eq!(e.text(), "foo\na");
        assert_eq!(e.cursor, 5);
    }

    #[test]
    fn delete_line_start_walks_back_across_newlines() {
        // "line1\nline2" with the cursor at the end of line 2.
        let mut e = LineEditor::new();
        e.insert_str("line1\nline2"); // cursor 11
        e.delete_line_start(); // clears line2 -> "line1\n", cursor 6
        assert_eq!(e.text(), "line1\n");
        assert_eq!(e.cursor, 6);
        e.delete_line_start(); // at boundary: clears preceding '\n' + line1 -> "", cursor 0
        assert_eq!(e.text(), "");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn delete_line_start_keeps_partial_line() {
        // "a\nbc|d" -> first Ctrl-U clears "bc", second clears "a\n".
        let mut e = LineEditor::new();
        e.insert_str("a\nbcd"); // 0-4, cursor 5
        e.left(); // cursor 4 (a\nbc|d)
        e.delete_line_start(); // "a\nd"
        assert_eq!(e.text(), "a\nd");
        assert_eq!(e.cursor, 2);
        e.delete_line_start(); // at boundary -> clears "a\n" -> "d"
        assert_eq!(e.text(), "d");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn paste_insertion_preserves_newlines() {
        let mut e = LineEditor::new();
        e.insert_str("foo\nbar");
        assert_eq!(e.cursor, 7);
        assert_eq!(e.text(), "foo\nbar");
    }

    #[test]
    fn layout_cursor_tracks_mid_word() {
        let mut e = LineEditor::new();
        e.insert_str("abcdef");
        e.left();
        e.left();
        e.left(); // cursor at position 3 ('d')
        let layout = e.layout(100, "");
        assert_eq!(layout.rows, vec!["abcdef"]);
        // Cursor must follow the actual position, not snap to column 0.
        assert_eq!((layout.cursor_row, layout.cursor_col), (0, 3));
    }

    #[test]
    fn layout_word_wrap_breaks_long_line() {
        let e = LineEditor::from_text("aaa bbb ccc");
        // width 7: "aaa bbb" (7) wraps, "ccc" on row 2
        let layout = e.layout(7, "");
        assert_eq!(layout.rows, vec!["aaa bbb", "ccc"]);
        assert_eq!(layout.row_count, 2);
        // cursor at end -> row 1, col 3
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));
    }

    #[test]
    fn layout_prompt_offset_on_row_zero() {
        let e = LineEditor::from_text("hi");
        let layout = e.layout(80, "> ");
        assert_eq!(layout.rows, vec!["hi"]);
        // cursor at end; col includes the 2-char prompt prefix
        assert_eq!((layout.cursor_row, layout.cursor_col), (0, 4));
    }

    #[test]
    fn layout_preserves_indent_and_cursor_offset() {
        // Leading spaces (indentation) must be counted toward the cursor column
        // and not silently dropped — otherwise the caret lands inside the indent.
        let e = LineEditor::from_text("  ab"); // 2-space indent
        // cursor at end of buffer (after "ab")
        let layout = e.layout(80, "");
        assert_eq!(layout.rows, vec!["  ab"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (0, 4));

        // cursor in the middle of the indentation (position 1, after 1 space)
        let mut e2 = LineEditor::from_text("  ab");
        e2.home();
        e2.right();
        let layout2 = e2.layout(80, "");
        assert_eq!((layout2.cursor_row, layout2.cursor_col), (0, 1));
    }

    #[test]
    fn layout_indented_long_line_wraps_within_width() {
        // width 6: an indented long line must wrap so no visual row overflows
        // the terminal (previously leading spaces were free, causing overflow
        // and a desynced cursor).
        let e = LineEditor::from_text("  a b c");
        let layout = e.layout(6, "");
        for r in &layout.rows {
            assert!(r.chars().count() <= 6, "row overflowed width: {:?}", r);
        }
        // Sanity: a non-indented line of the same shape still wraps identically.
        assert!(layout.row_count >= 2);
    }

    #[test]
    fn layout_hard_newline_splits_rows() {
        let e = LineEditor::from_text("ab\ncd");
        let layout = e.layout(80, "");
        assert_eq!(layout.rows, vec!["ab", "cd"]);
        // cursor at end of "cd" (row 1, col 2)
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 2));
    }

    #[test]
    fn render_input_roundtrips_empty() {
        let e = LineEditor::new();
        let layout = e.layout(80, "> ");
        let s = render_input(&layout, "> ");
        // restores input-top, clears down, writes prompt, no trailing newline,
        // and positions the cursor at the prompt end (col 2, no move needed).
        assert!(s.starts_with("\x1b[u\r\x1b[0J> "));
        assert!(!s.ends_with('\n'));
    }

    #[test]
    fn render_input_multiline_positions_cursor() {
        // cursor on the last row -> no vertical move needed
        let e = LineEditor::from_text("aaa\nbbb");
        let layout = e.layout(80, "");
        assert_eq!(layout.rows, vec!["aaa", "bbb"]);
        let s = render_input(&layout, "");
        assert!(s.contains("aaa\nbbb"), "{}", s);

        // cursor on row 0 -> render must move up one row to reposition
        let mut e2 = LineEditor::from_text("aaa\nbbb");
        e2.left(); // cursor 5 -> 4 (on row 1)
        e2.left();
        e2.left();
        e2.left(); // cursor 1 (row 0)
        let layout2 = e2.layout(80, "");
        assert_eq!(layout2.cursor_row, 0);
        let s2 = render_input(&layout2, "");
        assert!(s2.contains("\x1b[1A"), "expected move-up, got {}", s2);
    }

    #[test]
    fn history_navigation_with_draft_restore() {
        let mut h = History {
            entries: Vec::new(),
            file: None,
        };
        h.add("first");
        h.add("second");
        let mut nav: Option<usize> = None;
        let mut draft = String::new();
        assert_eq!(h.prev("draft", &mut nav, &mut draft), Some("second".to_string()));
        assert_eq!(h.prev("draft", &mut nav, &mut draft), Some("first".to_string()));
        assert_eq!(h.prev("draft", &mut nav, &mut draft), None);
        // stepping forward restores the stashed draft past the newest entry
        assert_eq!(h.next(&mut nav, &mut draft), Some("second".to_string()));
        assert_eq!(h.next(&mut nav, &mut draft), Some("draft".to_string()));
        assert!(nav.is_none());
    }

    #[test]
    fn up_at_top_preserves_unsubmitted_prompt() {
        // Regression: hitting Up at the top of an unsubmitted prompt used to
        // silently stash the in-progress text as the history draft and replace
        // it with the previous entry — easy to trigger when reaching for Home
        // and destructive enough to lose the whole prompt. Up should now be a
        // no-op while the buffer is non-empty and the user hasn't started
        // navigating history yet.
        let mut h = History {
            entries: Vec::new(),
            file: None,
        };
        h.add("first");
        h.add("second");
        let mut nav: Option<usize> = None;
        let mut draft = String::new();
        let mut editor = LineEditor::from_text("in progress");
        editor.end();
        let key = event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let _ = Repl::edit_key(key, &mut editor, &mut h, &mut nav, &mut draft, "");
        assert_eq!(editor.text(), "in progress", "Up must not clobber the unsubmitted prompt");
        assert!(nav.is_none(), "history navigation must not have started");
        assert!(draft.is_empty(), "no draft should have been stashed");
    }

    #[test]
    fn up_at_top_with_empty_buffer_still_loads_history() {
        // Counterpart to the regression test: an empty buffer should still
        // navigate history on Up so the previous-entry recall behavior
        // (the original reason Up-on-top exists) is preserved.
        let mut h = History {
            entries: Vec::new(),
            file: None,
        };
        h.add("first");
        h.add("second");
        let mut nav: Option<usize> = None;
        let mut draft = String::new();
        let mut editor = LineEditor::new();
        let key = event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let _ = Repl::edit_key(key, &mut editor, &mut h, &mut nav, &mut draft, "");
        assert_eq!(editor.text(), "second", "empty buffer should load latest history entry");
        assert!(nav.is_some());
        assert!(draft.is_empty());
    }

    #[test]
    fn up_continues_history_navigation_when_already_inside() {
        // Once the user has already navigated back (nav is Some), they are
        // intentionally walking the history; Up must keep advancing even if
        // the buffer is non-empty (it now holds a history entry, not a fresh
        // draft), and Down must keep restoring.
        let mut h = History {
            entries: Vec::new(),
            file: None,
        };
        h.add("first");
        h.add("second");
        let mut nav: Option<usize> = None;
        let mut draft = String::new();
        let mut editor = LineEditor::new();
        let up = event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        let _ = Repl::edit_key(up, &mut editor, &mut h, &mut nav, &mut draft, "");
        assert_eq!(editor.text(), "second");
        // second Up while buffer is non-empty should still navigate (nav != None)
        let _ = Repl::edit_key(up, &mut editor, &mut h, &mut nav, &mut draft, "");
        assert_eq!(editor.text(), "first");
        // Down restores the previous entry
        let down = event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let _ = Repl::edit_key(down, &mut editor, &mut h, &mut nav, &mut draft, "");
        assert_eq!(editor.text(), "second");
    }

    #[test]
    fn render_select_title_appears_once() {
        let opts = vec![
            "a: alpha model ctx 128000 $0.10/0.20 per M".to_string(),
            "b: beta model ctx 200000 $0.30/0.40 per M".to_string(),
        ];
        let out = Repl::overlay_lines("Select a model:", &opts, None, 0, None, true);
        assert_eq!(out.matches("Select a model:").count(), 1);
        assert!(out.contains("> a: alpha"));
        assert!(out.contains("  b: beta"));
    }

    #[test]
    fn render_select_wraps_long_option_with_aligned_continuation() {
        let long = "deepseek:deepseek-v4-flash ctx 1048576 $0.000000435/0.00000087 per M (est) and some extra trailing text to force a wrap";
        let opts = vec![long.to_string()];
        let out = Repl::overlay_lines("Select a model:", &opts, None, 0, None, true);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains("> "), "first row is the marker row");
        let cont: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains("  ") && !l.contains("> "))
            .collect();
        assert!(
            !cont.is_empty(),
            "expected a wrapped continuation line indented under the option text"
        );
    }

    #[test]
    fn overlay_window_centers_on_selection_and_clamps() {
        // 50 options in a 10-row window, selection at the very end.
        assert_eq!(Repl::overlay_window(50, 10, 49), 40);
        assert_eq!(Repl::overlay_window(50, 10, 0), 0);
        assert_eq!(Repl::overlay_window(50, 10, 5), 0);
        // avail >= len: show everything from the top.
        assert_eq!(Repl::overlay_window(5, 10, 4), 0);
        // avail == 0: degenerate, clamp to 0.
        assert_eq!(Repl::overlay_window(50, 0, 49), 0);
    }

    #[test]
    fn next_input_row_tracks_scroll_on_open_and_stays_on_shrink() {
        // Input at the bottom row (40). A tall pop-up (n=4) overflows by 5, so
        // the input scrolls up to row 35.
        assert_eq!(Repl::next_input_row(40, 40, 1, 4), 35);
        // The pop-up now shrinks (n=1) but the screen does NOT scroll back down,
        // so the input stays at row 35 (no duplication on the next redraw).
        assert_eq!(Repl::next_input_row(35, 40, 1, 1), 35);
        // No overflow: input row is unchanged.
        assert_eq!(Repl::next_input_row(5, 40, 1, 4), 5);
        // Overflow never pushes the input off the top (clamped to 1).
        assert_eq!(Repl::next_input_row(2, 40, 1, 40), 1);
        // No pop-up: only the input block is rewritten in place.
        assert_eq!(Repl::next_input_row(35, 40, 1, 0), 35);
    }

    #[test]
    fn clear_rows_to_bounds_clear_at_previous_bottom_on_grow() {
        // Grew from 24 to 40 rows. Input at row 10 (top-anchored reflow: the
        // cursor stays put). The revealed scrollback is rows 25..=40, *below*
        // the old bottom. The clear must stop at the old bottom (24) and must
        // NOT contain a clear-to-end-of-screen (`\x1b[0J`), which would wipe
        // that reveal.
        let seq = Repl::clear_rows_to(10, 24);
        assert!(!seq.contains("\x1b[0J"), "must not clear to end of screen");
        assert_eq!(seq.matches("\x1b[K").count(), 15, "one erase per row 10..=24");
        // Each line is stepped to the next (col 1) except after the last row.
        assert_eq!(seq.matches("\x1b[1E").count(), 14);
        // Nothing below the old bottom (row 25) is referenced.
        assert!(!seq.contains("\x1b[25;1H"));
    }

    #[test]
    fn clear_rows_to_full_screen_on_no_resize() {
        // No resize: clear_to == term_h, so the clear must cover every row
        // from the input down to the bottom — equivalent to the old `\x1b[0J`.
        let seq = Repl::clear_rows_to(3, 40);
        assert_eq!(seq.matches("\x1b[K").count(), 38);
        assert_eq!(seq.matches("\x1b[1E").count(), 37);
    }

    #[test]
    fn clear_rows_to_no_clear_when_input_below_old_bottom() {
        // Bottom-anchored grow: the cursor moved to the new bottom (40) but the
        // previous bottom was 24. Clearing "below" the input would be upwards
        // and must not happen; nothing is cleared so the revealed scrollback
        // (above the new input) is preserved.
        assert_eq!(Repl::clear_rows_to(40, 24), "");
    }

    #[test]
    fn clear_rows_to_single_row() {
        assert_eq!(Repl::clear_rows_to(5, 5), "\x1b[K");
    }

    #[test]
    fn model_rank_sorts_active_then_recent_then_rest() {
        let active = "openai/gpt-4";
        let recent = vec!["anthropic/claude".to_string(), "gemini/gem".to_string()];
        // active first
        assert_eq!(Repl::model_rank("openai/gpt-4", active, &recent), 0);
        // recent second (1-based)
        assert_eq!(Repl::model_rank("anthropic/claude", active, &recent), 1);
        assert_eq!(Repl::model_rank("gemini/gem", active, &recent), 2);
        // everything else last
        assert_eq!(
            Repl::model_rank("deepseek/flash", active, &recent),
            u32::MAX
        );
        // ordering: active < recent1 < recent2 < other
        let mut ids = vec![
            "deepseek/flash".to_string(),
            "gemini/gem".to_string(),
            "anthropic/claude".to_string(),
            "openai/gpt-4".to_string(),
        ];
        ids.sort_by(|a, b| {
            Repl::model_rank(a, active, &recent).cmp(&Repl::model_rank(b, active, &recent))
        });
        assert_eq!(
            ids,
            vec![
                "openai/gpt-4".to_string(),
                "anthropic/claude".to_string(),
                "gemini/gem".to_string(),
                "deepseek/flash".to_string(),
            ]
        );
    }

    #[test]
    fn token_at_cursor_handles_various_positions() {
        // Empty buffer
        assert_eq!(Repl::token_at_cursor(&[], 0), (0, 0, String::new()));

        // "hello world foo"
        //  h=0 e=1 l=2 l=3 o=4  =5 w=6 o=7 r=8 l=9 d=10  =11 f=12 o=13 o=14
        let buf: Vec<char> = "hello world foo".chars().collect();
        // Cursor at start (0) → empty
        assert_eq!(Repl::token_at_cursor(&buf, 0), (0, 0, String::new()));
        // Cursor at position 1 (after 'h') → chars before: "h"
        let (s, e, w) = Repl::token_at_cursor(&buf, 1);
        assert_eq!(&w, "h", "cursor after 'h': got {:?}", &w);
        assert_eq!(s, 0);
        assert_eq!(e, 1);
        // Cursor at position 5 (on the space after "hello") → word *before*: "hello"
        let (_s, _e, w) = Repl::token_at_cursor(&buf, 5);
        assert_eq!(&w, "hello", "cursor on space after hello: {:?}", &w);
        // Cursor at position 8 (after "wo" in "world") → chars before: "wo"
        let (s, e, w) = Repl::token_at_cursor(&buf, 8);
        assert_eq!(&w, "wo", "cursor after 'wo' in 'world': got {:?}", &w);
        assert_eq!(s, 6);
        assert_eq!(e, 8);
        // Cursor at position 11 (the space after "world") → word *before*: "world"
        let (_s, _e, w) = Repl::token_at_cursor(&buf, 11);
        assert_eq!(&w, "world", "cursor on space after world: {:?}", &w);
        // Cursor after last word (at end) → last word "foo"
        let (_s, _e, w) = Repl::token_at_cursor(&buf, buf.len());
        assert_eq!(&w, "foo", "cursor at end: last word");
    }

    #[test]
    fn token_at_cursor_empty_at_whitespace_start() {
        let buf: Vec<char> = "   abc".chars().collect();
        // Cursor at position 0 (start, before any word) → empty
        assert_eq!(Repl::token_at_cursor(&buf, 0), (0, 0, String::new()));
        // Cursor at position 2 (on leading whitespace) → no word before → empty
        assert_eq!(Repl::token_at_cursor(&buf, 2), (0, 0, String::new()), "cursor on leading whitespace");
        // Cursor at position 4 (on 'b' in "abc") → chars before: "a"
        let (s, e, w) = Repl::token_at_cursor(&buf, 4);
        assert_eq!(&w, "a", "cursor on 'b' in 'abc': chars before is {:?}", &w);
        assert_eq!(s, 3);
        assert_eq!(e, 4);
    }

    #[test]
    fn token_at_cursor_middle_of_sentence() {
        // Cursor in the middle of a sentence
        let buf: Vec<char> = "write_file src/foo.rs src/bar.rs".chars().collect();
        // Cursor at position 21 (the space between the two paths)
        // → word BEFORE cursor: "src/foo.rs"
        let (_s, _e, w) = Repl::token_at_cursor(&buf, 21);
        assert_eq!(&w, "src/foo.rs", "cursor on space: word before: {:?}", &w);
        // Cursor at position 20 (on the 's' at end of "src/foo.rs")
        // → chars before cursor: "src/foo.r"
        let (s, e, w) = Repl::token_at_cursor(&buf, 20);
        assert_eq!(&w, "src/foo.r", "cursor on 's' in src/foo.rs: chars before: {:?}", &w);
        assert_eq!(s, 11);
        assert_eq!(e, 20);
        // Cursor at position 22 (on 's' at start of "src/bar.rs")
        // → cursor at word start, no chars before → empty (no completion)
        assert_eq!(Repl::token_at_cursor(&buf, 22), (0, 0, String::new()), "cursor at start of src/bar.rs");
    }

    #[test]
    fn token_at_cursor_trailing_whitespace() {
        // Cursor past trailing whitespace → last word
        let buf: Vec<char> = "write_file src/ ".chars().collect();
        let (_s, _e, w) = Repl::token_at_cursor(&buf, buf.len());
        assert_eq!(&w, "src/", "trailing ws: last word");
    }

    #[test]
    fn token_at_cursor_single_word() {
        let buf: Vec<char> = "hello".chars().collect();
        // Cursor at end of buffer
        let (_s, _e, w) = Repl::token_at_cursor(&buf, 5);
        assert_eq!(&w, "hello");
        // Cursor at 1 (after 'h') → chars before: "h"
        let (_s, _e, w) = Repl::token_at_cursor(&buf, 1);
        assert_eq!(&w, "h");
    }

    #[test]
    fn replace_range_tests() {
        let mut e = LineEditor::from_text("hello world foo");
        // Replace "world" with "there" in the middle
        e.replace_range(6, 11, "there");
        assert_eq!(e.text(), "hello there foo");
        assert_eq!(e.cursor, 11);
        // Replace at start
        let mut e = LineEditor::from_text("hello world foo");
        e.replace_range(0, 5, "hi");
        assert_eq!(e.text(), "hi world foo");
        assert_eq!(e.cursor, 2);
        // Replace at end
        let mut e = LineEditor::from_text("hello world foo");
        e.replace_range(12, 15, "bar");
        assert_eq!(e.text(), "hello world bar");
        assert_eq!(e.cursor, 15);
    }

    impl LineEditor {
        fn from_text(s: &str) -> Self {
            let mut e = LineEditor::new();
            e.insert_str(s);
            e
        }
    }

    // ===== Fuzz-like deterministic tests (pure model) =====

    /// Verify that the layout invariants hold for a single editor state.
    fn check_layout(e: &LineEditor, width: usize, prompt: &str) {
        let layout = e.layout(width, prompt);
        let rc = layout.row_count.max(1);
        let pcols = prompt.chars().count();
        assert!(
            layout.cursor_row < rc,
            "cursor_row {} >= row_count {} (text={:?})",
            layout.cursor_row, rc, e.text()
        );
        if layout.cursor_row < layout.rows.len() {
            let content_col = if layout.cursor_row == 0 {
                layout.cursor_col.saturating_sub(pcols)
            } else {
                layout.cursor_col
            };
            let row_len = layout.rows[layout.cursor_row].chars().count();
            assert!(
                content_col <= row_len,
                "content_col {} > row_len {} in rows={:?}, text={:?}, cursor={}",
                content_col, row_len, layout.rows, e.text(), e.cursor
            );
        }
    }

    const STEP_KINDS: usize = 10;

    fn apply_step(e: &mut LineEditor, step: usize, width: usize, prompt: &str) {
        match step % STEP_KINDS {
            0 => { e.insert('a'); }
            1 => { e.insert(' '); }
            2 => { e.insert('\n'); }
            3 => { e.backspace(); }
            4 => { e.left(); }
            5 => { e.right(); }
            6 => { e.home(); }
            7 => { e.end(); }
            8 => { e.insert('-'); }
            9 => { e.insert_newline(); }
            _ => unreachable!()
        }
        check_layout(e, width, prompt);
    }

    #[test]
    fn fuzz_deterministic_sweep() {
        let params: Vec<(usize, &str)> = vec![
            (5, ""), (8, ""), (10, ""), (12, ""), (15, ""), (20, ""),
            (40, ""), (80, ""),
            (5, "> "), (8, "> "), (10, "> "), (12, "> "), (15, "> "),
            (20, "> "), (40, "> "), (80, "> "),
            (5, "$ "), (10, "$ "), (15, "$ "), (20, "$ "),
            (5, ">>>"), (10, ">>>"), (20, ">>>"),
        ];
        let seeds = [1usize, 3, 7, 11, 13, 17, 19, 23, 29, 31];
        let steps = 250;

        for (pi, (width, prompt)) in params.iter().enumerate() {
            for seed in seeds.iter() {
                let mut e = LineEditor::new();
                for s in 0..steps {
                    let step = (seed * (s + 1) * (pi + 1)) % STEP_KINDS;
                    apply_step(&mut e, step, *width, prompt);
                }
            }
        }
    }

    fn exhaustive(text: &str, width: usize, prompt: &str) {
        let mut e = LineEditor::new();
        e.insert_str(text);
        for pos in 0..=e.text().len() {
            e.cursor = pos;
            check_layout(&e, width, prompt);
        }
    }

    #[test]
    fn fuzz_exhaustive_realistic_texts() {
        let cases = [
            ("", 80, ""),
            ("a", 80, ""),
            ("hello world", 80, "> "),
            ("abc\ndef", 5, ""),
            ("abc\ndef", 10, "> "),
            ("abc def ghi", 5, ""),
            ("abc def ghi", 8, "> "),
            ("abc def ghi jkl", 10, "> "),
            ("- item 1\n- item 2", 80, ""),
            ("Multi-line input: There are still bugs that affect cursor", 20, "> "),
            ("  indented\n  too", 10, ""),
            ("abc \ndef", 80, ""),
            ("abc\n", 80, ""),
            ("\n", 80, ""),
            ("\n\n", 80, ""),
            ("abc def ghi\njk lm", 80, "> "),
            ("abc def ghi jkl mno\npqr stu", 10, "> "),
            ("abc def ghi jkl mno pqr stu vwx yz", 12, ""),
            ("- [ ] task\n- [x] done", 80, "> "),
            ("a\nb\nc\nd\ne\nf\ng", 80, ""),
            ("    fn main() {\n        println!(\"hello\");\n    }", 30, ""),
            ("   ", 80, ""),
            ("   \n   ", 80, ""),
            ("abc def\n\nghi", 15, ""),
            ("x", 1, ""),
            ("x y", 1, ""),
        ];
        for (text, width, prompt) in &cases {
            exhaustive(text, *width, prompt);
        }
    }

    #[test]
    fn fuzz_user_scenario_back_to_first_char() {
        let w = 80usize;
        let prompt = "> ";
        let mut e = LineEditor::new();
        for c in "abc def ghi".chars() { e.insert(c); }
        e.insert_newline();
        for c in "jk lm".chars() { e.insert(c); }
        assert_eq!(e.text(), "abc def ghi\njk lm");
        assert!(e.visual_up(w, prompt), "visual_up");
        while e.cursor > 0 { e.left(); check_layout(&e, w, prompt); }
        assert_eq!(e.cursor, 0, "cursor must reach position 0");
        let before = e.text();
        e.backspace();
        assert_eq!(e.text(), before, "backspace at pos 0 no-op");
    }

    #[test]
    fn fuzz_reproduce_user_report_exact_text() {
        let w = 80usize;
        let prompt = "> ";
        let mut e = LineEditor::new();
        for c in "some text and more".chars() { e.insert(c); }
        e.insert_newline();
        for c in "and even more text in this line".chars() { e.insert(c); }
        assert_eq!(e.cursor, 50, "cursor at buffer end");
        assert!(e.visual_up(w, prompt), "visual_up");
        while e.cursor > 0 {
            check_layout(&e, w, prompt);
            e.left();
        }
        assert_eq!(e.cursor, 0, "cursor must reach position 0");
    }

    #[test]
    fn fuzz_wrapped_first_line_nav_to_pos0() {
        let w = 15usize;
        let prompt = "> ";
        let mut e = LineEditor::new();
        for c in "abc def ghi jkl mno".chars() { e.insert(c); }
        e.insert_newline();
        for c in "stu vwx".chars() { e.insert(c); }
        assert!(e.visual_up(w, prompt), "visual_up");
        while e.cursor > 0 { e.left(); check_layout(&e, w, prompt); }
        assert_eq!(e.cursor, 0, "cursor at position 0");
    }

    #[test]
    fn fuzz_home_end_every_position() {
        let text = "abc def ghi\njk lm\nnop qrs";
        let mut e = LineEditor::new();
        e.insert_str(text);
        for pos in 0..=text.len() {
            e.cursor = pos;
            let home_pos = {
                let mut i = e.cursor;
                while i > 0 && e.buf[i - 1] != '\n' { i -= 1; }
                i
            };
            let end_pos = {
                let n = text.len();
                let mut i = e.cursor;
                while i < n && e.buf[i] != '\n' { i += 1; }
                i
            };
            e.home();
            assert_eq!(e.cursor, home_pos, "home from pos {}", pos);
            check_layout(&e, 40, "> ");
            e.end();
            assert_eq!(e.cursor, end_pos, "end from pos {}", pos);
            check_layout(&e, 40, "> ");
            e.cursor = pos;
        }
    }

    #[test]
    fn fuzz_visual_up_down_roundtrip() {
        let texts = [
            "abc def ghi\njk lm",
            "abc def ghi jkl\npqr stu",
            "a\nb\nc\nd\ne",
            "abc",
            "a long line that will wrap at narrow widths",
        ];
        let widths = [5usize, 10, 20, 40];
        for text in &texts {
            for w in &widths {
                let mut e = LineEditor::new();
                e.insert_str(text);
                for pos in 0..=text.len() {
                    e.cursor = pos;
                    if e.visual_up(*w, "> ") {
                        assert!(e.visual_down(*w, "> "));
                        check_layout(&e, *w, "> ");
                    }
                    e.cursor = pos;
                    if e.visual_down(*w, "> ") {
                        assert!(e.visual_up(*w, "> "));
                        check_layout(&e, *w, "> ");
                    }
                    e.cursor = pos;
                }
            }
        }
    }

    #[test]
    fn fuzz_insert_delete_patterns() {
        let mut e = LineEditor::new();
        for c in "hello world".chars() { e.insert(c); check_layout(&e, 80, "> "); }
        for _ in 0.."hello world".len() { e.backspace(); check_layout(&e, 80, "> "); }
        assert_eq!(e.text(), "");
        e.insert_str("abc");
        e.home();
        e.insert('X');
        check_layout(&e, 80, "> ");
        assert_eq!(e.text(), "Xabc");
    }

    #[test]
    fn fuzz_line_delete_keeps_invariants() {
        let tests = ["abc def ghi", "abc def ghi\njk lm", "\nhello", "hello\n", "a\nb\nc"];
        for text in &tests {
            let mut e = LineEditor::new();
            e.insert_str(text);
            for pos in 0..=text.len() {
                e.cursor = pos;
                let mut e2 = LineEditor::new();
                e2.insert_str(text);
                e2.cursor = pos;
                e2.delete_line_start();
                check_layout(&e2, 80, "> ");
                let mut e3 = LineEditor::new();
                e3.insert_str(text);
                e3.cursor = pos;
                e3.delete_line_end();
                check_layout(&e3, 80, "> ");
            }
        }
    }

    #[test]
    fn fuzz_single_row_narrow_widths() {
        let texts = ["a", "ab", "abc", "abc def", "abc def ghi", "  indented"];
        let widths = [1usize, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        for text in &texts {
            for w in &widths {
                exhaustive(text, *w, "");
                exhaustive(text, *w, "> ");
                exhaustive(text, *w, "$ ");
            }
        }
    }

    #[test]
    fn fuzz_path_completion_preserves_layout() {
        let tests = [("/home/user", "/home/user/project"), ("/tmp", "/tmp/data"), ("foo", "foobar")];
        for (buf, replacement) in &tests {
            let result = Repl::replace_last_token(buf, replacement);
            let mut e = LineEditor::new();
            e.insert_str(&result);
            check_layout(&e, 80, "> ");
        }
    }

    // ===== vt100 terminal-emulator tests =====
    //
    // Build the actual escape sequences the terminal would see (as emitted by
    // `redraw`) and feed them through a real vt100 terminal emulator.  This
    // checks both visible cell content and the final cursor position — the
    // strongest test possible without a real TTY.
    #[cfg(test)]
    mod vt100_tests {
        use vt100::Parser;
        use crate::repl::LineEditor;

        // Build the escape sequence the production `redraw` emits for the
        // input region, minus the clear-down (which uses `\x1b[1E` that the
        // vt100 parser doesn't implement).  The core positioning is:
        //
        // 1. `\x1b[{input_row};1H` — jump to input top row
        // 2. `render_content(prompt)` — write prompt + rows with `\n` separators
        // 3. `cursor_positioning(prompt)` — fine-tune cursor to the right cell
        fn build_render(editor: &LineEditor, width: usize, prompt: &str,
                        term_rows: usize, input_row: usize) -> Vec<u8> {
            let row = input_row.max(1).min(term_rows);
            let layout = editor.layout(width, prompt);
            let mut s = String::new();
            let pcols = prompt.chars().count();
            let rc = layout.row_count.max(1);

            // 1. Position at input top row
            s.push_str(&format!("\x1b[{};1H", row));

            // 2. Render content (use \r\n because the real terminal has
            // OPOST/ONLCR which converts bare \n to \r\n)
            for r in 0..rc {
                if r > 0 { s.push_str("\r\n"); }
                if r == 0 { s.push_str(prompt); }
                if r < layout.rows.len() { s.push_str(&layout.rows[r]); }
            }

            // 3. cursor_positioning (same logic as the real function)
            let last_row_len = if rc == 1 {
                layout.rows.first().map(|x| x.chars().count()).unwrap_or(0)
            } else {
                layout.rows.last().map(|x| x.chars().count()).unwrap_or(0)
            };
            let move_up = (rc - 1).saturating_sub(layout.cursor_row);
            if move_up > 0 {
                s.push_str(&format!("\x1b[{}A", move_up));
                s.push('\r');
                if layout.cursor_col > 0 {
                    s.push_str(&format!("\x1b[{}C", layout.cursor_col));
                }
            } else {
                let effective_end_col = if rc == 1 { pcols + last_row_len } else { last_row_len };
                let d = effective_end_col.saturating_sub(layout.cursor_col);
                if d > 0 { s.push_str(&format!("\x1b[{}D", d)); }
            }

            s.into_bytes()
        }

        fn render_through_vt100(editor: &LineEditor, width: usize, prompt: &str,
                                term_rows: usize, term_cols: usize,
                                input_row: usize) -> (usize, usize, Vec<String>) {
            let bytes = build_render(editor, width, prompt, term_rows, input_row);
            let mut p = Parser::new(term_rows as u16, term_cols as u16, 1000);
            p.process(&bytes);
            let screen = p.screen();
            let (cr, cc) = screen.cursor_position();
            let rows: Vec<String> = (0..term_rows)
                .map(|r| {
                    let mut s = String::new();
                    for c in 0..term_cols {
                        s.push(screen.cell(r as u16, c as u16).map(|cell| cell.contents()).unwrap_or(" ").chars().next().unwrap_or(' '));
                    }
                    s.trim_end().to_string()
                })
                .collect();
            (cr as usize, cc as usize, rows)
        }

        #[test]
        fn vt100_cursor_at_start_of_single_line() {
            let mut e = LineEditor::new();
            e.insert_str("Hello");
            e.cursor = 0;
            let (cr, cc, rows) = render_through_vt100(&e, 80, "> ", 24, 80, 10);
            // input_row=10 is 1-indexed; vt100 uses 0-indexed rows, so row = 9
            assert_eq!((cr, cc), (9, 2), "cursor at start should be row 9 col 2");
            assert!(rows[9].starts_with("> Hello"), "row 9: {:?}", rows[9]);
        }

        #[test]
        fn vt100_cursor_at_start_of_multi_line() {
            let mut e = LineEditor::new();
            e.insert_str("abc def ghi\njk lm");
            e.cursor = 0;
            let (cr, cc, rows) = render_through_vt100(&e, 80, "> ", 24, 80, 10);
            assert_eq!((cr, cc), (9, 2), "cursor at start of multi-line should be row 9 col 2");
            assert!(rows[9].starts_with("> abc"), "row 9: {:?}", rows[9]);
        }

        #[test]
        fn vt100_cursor_at_mid_of_first_line() {
            let mut e = LineEditor::new();
            e.insert_str("abc def ghi\njk lm");
            e.cursor = 4;
            let (cr, cc, _) = render_through_vt100(&e, 80, "> ", 24, 80, 10);
            // cursor_col = pcols(2) + index(4) = 6
            assert_eq!((cr, cc), (9, 6), "cursor at pos 4 should be row 9 col 6");
        }

        #[test]
        fn vt100_cursor_at_start_of_second_line() {
            let mut e = LineEditor::new();
            e.insert_str("abc def ghi\njk lm");
            e.cursor = 12;
            let (cr, cc, rows) = render_through_vt100(&e, 80, "> ", 24, 80, 10);
            // cursor on second visual row (row 10 0-indexed), col 0
            assert_eq!((cr, cc), (10, 0), "cursor at start of second line should be row 10 col 0");
            assert!(rows[10].starts_with("jk lm"), "row 10: {:?}", rows[10]);
        }

        #[test]
        fn vt100_user_exact_text_cursor_at_start() {
            let mut e = LineEditor::new();
            for c in "some text and more".chars() { e.insert(c); }
            e.insert_newline();
            for c in "and even more text in this line".chars() { e.insert(c); }
            e.cursor = 0;
            let (cr, cc, rows) = render_through_vt100(&e, 80, "> ", 24, 80, 10);
            // cursor_row=0 in layout → 0-indexed vt100 row = 9 + 0 = 9
            assert_eq!((cr, cc), (9, 2),
                "cursor at start should be (9,2), got ({},{})", cr, cc);
            assert!(rows[9].starts_with("> some text and more"), "row 9: {:?}", rows[9]);
            assert!(rows[10].starts_with("and even more text"), "row 10: {:?}", rows[10]);
        }

        #[test]
        fn vt100_every_cursor_position_roundtrips() {
            let text = "some text and more\nand even more text in this line";
            let w = 80usize;
            let prompt = "> ";
            let mut e = LineEditor::new();
            e.insert_str(text);
            for pos in 0..=text.len() {
                e.cursor = pos;
                let layout = e.layout(w, prompt);
                let (cr, cc, _) = render_through_vt100(&e, w, prompt, 24, 80, 10);
                // vt100 rows are 0-indexed; input_row=10 1-indexed = 9 0-indexed
                let expected_row = 9 + layout.cursor_row;
                assert_eq!(
                    (cr, cc), (expected_row, layout.cursor_col),
                    "pos {}: expected ({},{}), got ({},{}); layout=({},{}), rows={:?}",
                    pos, expected_row, layout.cursor_col, cr, cc,
                    layout.cursor_row, layout.cursor_col, layout.rows
                );
            }
        }

        #[test]
        fn vt100_narrow_width_exhaustive() {
            let texts = ["a", "ab", "abc", "abc def", "  indented"];
            let widths = [1usize, 2, 3, 4, 5, 6, 7, 8, 9, 10, 15];
            for text in &texts {
                for w in &widths {
                    let mut e = LineEditor::new();
                    e.insert_str(text);
                    for pos in 0..=text.len() {
                        e.cursor = pos;
                        let layout = e.layout(*w, "> ");
                        let (cr, cc, _) = render_through_vt100(&e, *w, "> ", 24, 80, 5);
                        // input_row=5 1-indexed = 4 0-indexed
                        assert_eq!(
                            (cr, cc), (4 + layout.cursor_row, layout.cursor_col),
                            "text={:?}, w={}, pos={}: rows={:?}",
                            text, w, pos, layout.rows
                        );
                    }
                }
            }
        }

        #[test]
        fn vt100_single_row_no_prompt() {
            let mut e = LineEditor::new();
            e.insert_str("Hello");
            e.cursor = 0;
            let (cr, cc, rows) = render_through_vt100(&e, 80, "", 24, 80, 5);
            // input_row=5 1-indexed = 4 0-indexed. No prompt, so col=0.
            assert_eq!((cr, cc), (4, 0), "cursor at start without prompt should be row 4 col 0");
            assert!(rows[4].starts_with("Hello"), "row 4: {:?}", rows[4]);
        }
    }

    // ===== resolve_editor / edit_in_editor tests =====
    //
    // These tests modify environment variables (VISUAL / EDITOR) and must
    // run serially to avoid races.  `ENV_LOCK` serialises them.

    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Global mutex that serialises all env-var-modifying tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Save and restore environment variables around a test closure.
    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved: Vec<(String, Option<String>)> = vars.iter().map(|(k, _)| {
            (k.to_string(), std::env::var(k).ok())
        }).collect();
        for (k, v) in vars {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val); }
                None => unsafe { std::env::remove_var(k); }
            }
        }
        f();
        for (k, v) in saved {
            match v {
                Some(val) => unsafe { std::env::set_var(&k, &val); }
                None => unsafe { std::env::remove_var(&k); }
            }
        }
    }

    #[test]
    fn resolve_editor_prefers_visual() {
        with_env(&[
            ("VISUAL", Some("myeditor")),
            ("EDITOR", Some("other")),
        ], || {
            let (cmd, args) = super::resolve_editor();
            assert_eq!(cmd, "myeditor", "VISUAL must take priority");
            assert!(args.is_empty(), "no flags: {:?}", args);
        });
    }

    #[test]
    fn resolve_editor_falls_back_to_editor() {
        with_env(&[
            ("VISUAL", None),
            ("EDITOR", Some("nano")),
        ], || {
            let (cmd, args) = super::resolve_editor();
            assert_eq!(cmd, "nano", "EDITOR fallback");
            assert!(args.is_empty());
        });
    }

    #[test]
    fn resolve_editor_defaults_to_vim() {
        with_env(&[
            ("VISUAL", None),
            ("EDITOR", None),
        ], || {
            let (cmd, args) = super::resolve_editor();
            assert_eq!(cmd, "vim", "default to vim");
            assert!(args.is_empty());
        });
    }

    #[test]
    fn resolve_editor_splits_flags() {
        with_env(&[
            ("VISUAL", Some("emacs -nw --no-site-file")),
            ("EDITOR", None),
        ], || {
            let (cmd, args) = super::resolve_editor();
            assert_eq!(cmd, "emacs");
            assert_eq!(args, vec!["-nw", "--no-site-file"]);
        });
    }

    /// Helper: run `edit_in_editor` with `current` content by setting
    /// `EDITOR` to a shell script that writes `expected_output` to the file.
    fn mock_edit(current: &str, editor_body: &str, expected_output: &str) {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("mock_editor.sh");
        {
            let mut f = std::fs::File::create(&script).expect("create script");
            f.write_all(b"#!/bin/sh\n").unwrap();
            f.write_all(editor_body.as_bytes()).unwrap();
            f.flush().unwrap();
        }
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");

        with_env(&[
            ("VISUAL", Some(script.to_string_lossy().as_ref())),
            ("EDITOR", None),
        ], || {
            let result = super::edit_in_editor(current);
            assert!(result.is_ok(), "edit_in_editor failed: {:?}", result.err());
            assert_eq!(result.unwrap(), expected_output);
        });
    }

    #[test]
    fn edit_in_editor_roundtrip() {
        mock_edit(
            "hello world",
            "printf '%s' 'edited content' > \"$1\"\n",
            "edited content",
        );
    }

    #[test]
    fn edit_in_editor_empty_buffer() {
        mock_edit(
            "",
            "printf '%s' 'new content' > \"$1\"\n",
            "new content",
        );
    }

    #[test]
    fn edit_in_editor_trailing_newline_stripped() {
        mock_edit(
            "hello",
            "printf '%s\\n' 'hello world' > \"$1\"\n",
            "hello world",
        );
    }

    #[test]
    fn edit_in_editor_preserves_internal_newlines() {
        mock_edit(
            "line1\nline2\nline3",
            "printf '%s\\n%s\\n%s' 'aaa' 'bbb' 'ccc' > \"$1\"\n",
            "aaa\nbbb\nccc",
        );
    }

    #[test]
    fn edit_in_editor_keeps_trailing_blank_lines() {
        // If the file ends with "a\n\n" (a, plus one blank line), stripping
        // one trailing \n must leave "a\n" — the blank line is preserved.
        mock_edit(
            "a\n\n",
            "printf '%s\\n\\n' 'a' > \"$1\"\n",
            "a\n",
        );
    }

    #[test]
    fn edit_in_editor_nonzero_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("failing_editor.sh");
        {
            let mut f = std::fs::File::create(&script).expect("create script");
            f.write_all(b"#!/bin/sh\nexit 1\n").unwrap();
            f.flush().unwrap();
        }
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");

        with_env(&[
            ("VISUAL", Some(script.to_string_lossy().as_ref())),
            ("EDITOR", None),
        ], || {
            let result = super::edit_in_editor("original");
            assert!(result.is_err(), "non-zero exit must return error");
        });
    }

    #[test]
    fn edit_in_editor_nonexistent_command() {
        with_env(&[
            ("VISUAL", Some("/nonexistent/editor_binary")),
            ("EDITOR", None),
        ], || {
            let result = super::edit_in_editor("hello");
            assert!(result.is_err(), "nonexistent command must return error");
        });
    }
}

#[cfg(test)]
mod model_v2_tests {
    use super::*;
    use crate::output::NoOutput;

    fn editor_with(s: &str) -> LineEditor {
        let mut e = LineEditor::new();
        e.insert_str(s);
        e
    }

    fn model_ctx() -> ReplCtx {
        ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: Default::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree: vec![],
            leaf_id: None,
            context_tokens: 0,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: None,
        }
    }

    fn model_ov(buf: &str) -> ActiveOverlay {
        let ctx = model_ctx();
        let settings = SettingsStore {
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
        ActiveOverlay::from_buffer(buf, &ctx, &settings, false, &[])
    }

    /// `/model ` opens the dynamic list; the engine rows must be non-empty
    /// (the ModelProvider resolves the catalog rows).
    #[test]
    fn model_menu_opens_with_dynamic_rows() {
        match model_ov("/model ") {
            ActiveOverlay::Model { engine_rows, .. } => {
                assert!(!engine_rows.is_empty(), "engine rows empty");
            }
            other => panic!("expected Model, got disc {:?}", other.overlay_disc()),
        }
    }

    /// Tab on a cursor row completes that row's model id (host cursor wins).
    #[test]
    fn model_tab_completes_cursor_row() {
        let ov = model_ov("/model ");
        let mut editor = editor_with("/model ");
        let ctx = model_ctx();
        let settings = SettingsStore {
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
        let shared = MenuShared::new(settings.clone(), ctx.repo.clone());
        let mut engine = slash_menu_bridge::MenuEngine::new(&shared);
        ov.handle_tab(1, &mut editor, &mut Vec::new(), &mut None,
            &settings, &mut engine);
        let text = editor.text();
        // Row 1 of the engine rows (the list is provider-filtered) must be
        // what Tab commits: `/model <qualified-id>`.
        assert!(text.starts_with("/model ") && text != "/model ",
            "Tab must append a model id, got {text:?}");
        assert!(!text.contains("help"), "Tab must not fall back to the root menu");
    }

    /// Enter on a cursor row selects that model (SetModel with its id).
    #[test]
    fn model_enter_selects_cursor_row() {
        let ov = model_ov("/model ");
        let (_t, _opts, _active, _) = ov.display_rows();
        let mut editor = editor_with("/model ");
        let ctx = model_ctx();
        let settings = SettingsStore {
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
        let mut settings = settings;
        let output = NoOutput;
        let redraw = |_: &LineEditor, _: &[&str], _: usize, _: &mut usize| {};
        let mut input_row = 0usize;
        let mut dirty = false;
        let mut value_error: Option<String> = None;
        let mut help_view = false;
        let shared = MenuShared::new(settings.clone(), ctx.repo.clone());
        let mut engine = slash_menu_bridge::MenuEngine::new(&shared);
        let action = ov.handle_enter(
            1, &mut editor, &output, &redraw, &mut input_row,
            &mut settings, &mut dirty, &mut value_error, &mut help_view,
            &ctx, &mut engine,
        );
        match action {
            OverlayAction::Action(InputAction::SetModel(p, m)) => {
                assert!(p == "p" || !m.is_empty(),
                    "SetModel must carry a concrete model, got {p}:{m}");
            }
            other => {
                let disc = match other { OverlayAction::Continue => 0, OverlayAction::Action(_) => 1 };
                panic!("expected SetModel action, got disc {disc}");
            }
        }
    }
}
