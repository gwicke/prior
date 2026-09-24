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
use std::cell::RefCell;
use std::rc::Rc;
use crossterm::event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crate::output::{term_width, wrap, Output};
use crate::model_catalog;
use crate::slash_menu_bridge::MenuShared;
use crate::settings::SettingsStore;
use crate::session::SessionTreeNode;
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
    /// A v2.5 menu committed a value (wrote to shared state / pending_op).
    /// The agent loop processes `menu_submitted` (LiveState sync) and any
    /// `pending_op` after the match; this variant is a no-op so the
    /// committed text is never treated as a prompt.
    MenuDone,
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
    /// Shared via Rc with MenuShared — no deep clone on sync.
    pub tree: std::rc::Rc<Vec<SessionTreeNode>>,
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

/// A single overlay "variant" that carries its own pre-computed data so
/// Tab/Enter/Esc handlers never need to re-query the buffer or re-filter.
///
/// Eliminates `kind: u8`, the three parallel match ladders, `help_view`,
/// `cur_word`/`cur_filter` bridge variables, and the `value_cands` hack.
pub(crate) enum ActiveOverlay {
    None,

    /// v2.5 engine overlay: carries a `RenderPlan` from the v2.5
    /// `SlashDotHandle::eval_prompt`. All key dispatch goes through the
    /// persistent handle — the overlay is purely a display snapshot.
    V25Menu {
        /// The render plan from the v2.5 engine.
        plan: crate::slashdot_menu::render::RenderPlan,
    },

    PathComplete {
        completions: Vec<String>,
        completion_pos: Option<(usize, usize)>,
    },
}

impl ActiveOverlay {
    /// Decompose an engine [`RenderPlan`] into the legacy `(opts, active)`
    /// contract for a verbatim plan-painted overlay: one logical option per
    /// candidate (visual lines joined with `\n`, marker-free text), plus the
    /// plan's `*` active-marker index. Header lines are excluded from opts
    /// (they are non-selectable); the caller's paint arm renders them.
    /// The loop uses the returned opts only for cursor/focus math.
    fn plan_rows(plan: &crate::slashdot_menu::render::RenderPlan) -> (Vec<String>, Option<usize>) {
        let opts: Vec<String> = plan.rows.iter()
            .map(|l| l.0.clone())
            .collect();
        (opts, plan.active_rows.first().copied())
    }

    /// Display parameters: `(title, logical options, active index, wrap_opts)`.
    /// The caller feeds these to `Repl::overlay_lines` for final rendering.
    fn display_rows(&self) -> (&str, Vec<String>, Option<usize>, bool) {
        match self {
            ActiveOverlay::None => ("", vec![], None, false),
            ActiveOverlay::V25Menu { plan, .. } => {
                if plan.rows.is_empty() {
                    return ("", vec!["(no results)".into()], None, false);
                }
                let (opts, active) = Self::plan_rows(plan);
                ("", opts, active, false)
            }
            ActiveOverlay::PathComplete { completions, .. } => {
                ("Path completions  (up/down, Tab/Enter select, Esc dismiss)",
                 completions.clone(), None, false)
            }
        }
    }

    /// The engine-owned cursor row for this overlay, if backed by the
    /// v2.5 engine. `None` for host-cursor overlays (PathComplete).
    pub fn cursor_row(&self) -> Option<usize> {
        match self {
            ActiveOverlay::V25Menu { plan } => plan.selected,
            _ => None,
        }
    }

    /// Handle Tab on a selected row.  Mutates the editor in-place.
    fn handle_tab(
        &self,
        sel: usize,
        editor: &mut LineEditor,
        path_completions: &mut Vec<String>,
        completion_pos: &mut Option<(usize, usize)>,
    ) {
        match self {
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
            _ => {}
        }
    }

    /// Handle Enter on a selected row: apply the path completion (the
    /// v2.5 overlay routes Enter through the engine before this).
    fn handle_enter(&self, sel: usize, editor: &mut LineEditor) {
        if let ActiveOverlay::PathComplete { completions, completion_pos: pos } = self {
            if let Some(c) = completions.get(sel) {
                let buf = editor.text();
                if let Some((start, end)) = pos {
                    editor.replace_range(*start, *end, c);
                } else {
                    editor.set(&Repl::replace_last_token(&buf, c));
                }
            }
        }
    }

    /// Handle Escape.
    fn handle_esc(
        &self,
        path_completions: &mut Vec<String>,
        completion_pos: &mut Option<(usize, usize)>,
    ) {
        match self {
            ActiveOverlay::PathComplete { .. } => {
                path_completions.clear();
                *completion_pos = None;
            }
            _ => {}
        }
    }

    /// Return a discriminant value to detect "kind changes" for sel reset.
    fn overlay_disc(&self) -> u8 {
        match self {
            ActiveOverlay::None => 0,
            ActiveOverlay::PathComplete { .. } => 7,
            ActiveOverlay::V25Menu { .. } => 20,
        }
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
        menu_submitted: &mut bool,
        pending_op: &Rc<RefCell<Option<crate::slash_menu_bridge::AsyncOp>>>,
        live_state: &Rc<RefCell<crate::slash_menu_bridge::LiveState>>,
        initial_row: usize,
        output: &dyn Output,
    ) -> Option<InputAction> {
        // Step 2 (Phase 6.7): engine providers come from shared live state.
        // Build a `MenuShared` handle bundle for this activation; it shares
        // the same SettingsStore the loop / run_interactive mutate, the
        // session repo, and the current-session/tree sync mirrors.
        let mut shared = MenuShared::new(
            settings.clone(),
            ctx.repo.clone(),
        );
        // Share the caller's LiveState Rc so menu writes are visible to
        // run_interactive after read_input returns.
        shared.live_state = live_state.clone();
        *shared.current_session_id.borrow_mut() = ctx.current_session_id.clone();
        *shared.recent_models.borrow_mut() = ctx.recent.clone();
        *shared.raw_tree.borrow_mut() = ctx.tree.clone();
        *shared.leaf_id.borrow_mut() = ctx.leaf_id.clone();
        *shared.session_usage.borrow_mut() = ctx.session_usage;
        // Share the pending_op cell with the caller so async ops are
        // visible after read_input returns.
        shared.pending_op = pending_op.clone();
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
        let mut path_completions: Vec<String> = Vec::new();
        let mut completion_pos: Option<(usize, usize)> = None;
        let mut last_disc: u8 = 0; // overlay discriminant for sel-reset tracking
        // v2.5 engine: persistent menu tree + guard-like handle.
        // The menu lives for the entire prompt session; the handle is
        // created when the prompt starts with `/` and dropped when it
        // no longer does. All `/` dispatch goes through `eval_prompt`.
        let mut v25_menu = {
            let shared_rc = std::rc::Rc::new(shared.clone_shared());
            crate::v25_menu::build_menu(&shared.live_state, &shared.settings, &shared_rc)
        };
        let mut v25_handle: Option<crate::slashdot_menu::menu::SlashDotHandle<'_>> = None;
        // Direct render: a plan returned by an Enter/Tab whose prompt did
        // not change (modulo trailing whitespace) — painted for exactly
        // the next frame instead of discarded, because such a frame (e.g.
        // `SubmitResult::Error`) is not re-derivable from the buffer.
        // Consumed by the `take()` at the top of the next iteration; no
        // clearing logic — the next key re-derives as always.
        let mut v25_enter_plan: Option<crate::slashdot_menu::render::RenderPlan> = None;
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
            let buf_text = editor.text();
            // Direct render: paint the previous dispatch's returned plan
            // once (prompt unchanged); otherwise derive from the buffer.
            let direct_plan = v25_enter_plan.take();
            let overlay = if let Some(plan) = direct_plan {
                ActiveOverlay::V25Menu { plan }
            } else if buf_text.starts_with('/') {
                // All `/` commands route through the v2.5 engine.
                // Manage handle lifecycle: create on first `/`, drop when not `/`.
                if v25_handle.is_none() {
                    v25_handle = Some(v25_menu.get_handle());
                }
                let handle = v25_handle.as_mut().unwrap();
                let result = Self::v25_eval(
                    handle,
                    &buf_text,
                    editor.cursor,
                    crate::slashdot_menu::HostKey::None,
                );
                match result {
                    crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                        ActiveOverlay::V25Menu { plan }
                    }
                    _ => ActiveOverlay::None,
                }
            } else if !path_completions.is_empty() {
                v25_handle = None;
                ActiveOverlay::PathComplete {
                    completions: path_completions.to_vec(),
                    completion_pos: None,
                }
            } else {
                // Drop the v25 handle when prompt leaves `/`.
                v25_handle = None;
                ActiveOverlay::None
            };
            // Hoisted before the sel-reset so `active` (the menu's
            // default/current-state row) can drive the cursor policy below.
            let (title, raw_opts, active, wrap) = overlay.display_rows();
            let cur_disc = overlay.overlay_disc();
            if overlay.cursor_row().is_some() {
                // Engine-owned cursor: `sel` IS the engine's cursor row
                // (arrows mutate engine state; Tab/Enter act on it). The
                // marker `active` (ctx-driven `*`) is separate. No
                // host-side cursor policy for these overlays.
                sel = overlay
                    .cursor_row()
                    .unwrap_or(active.unwrap_or(0))
                    .min(raw_opts.len().saturating_sub(1));
                if cur_disc != last_disc { last_disc = cur_disc; }
            } else {
                sel = Self::cursor_sel_policy(cur_disc != last_disc, sel);
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
            let overlay_text = match &overlay {
                ActiveOverlay::None => String::new(),
                ActiveOverlay::V25Menu { plan } => {
                    let tw = crate::output::term_width().max(1);
                    let mut result: Vec<String> = Vec::new();

                    // --- Space allocation ---
                    // `cur_h` is the full terminal height; 2/3 is the overlay budget.
                    let max_pop = ((cur_h as f64 * 2.0 / 3.0).ceil() as usize).max(1);
                    let table_header_lines = if plan.table_header.is_some() { 1 } else { 0 };
                    let min_rows = 3usize;

                    // Count visual lines each detail item occupies (LogicalLine
                    // may contain embedded \n for wrapped sub-lines).
                    let detail_visual: Vec<usize> = plan.detail.iter()
                        .map(|d| d.0.split('\n').count())
                        .collect();
                    let total_detail_vis: usize = detail_visual.iter().sum();
                    let total_rows = plan.rows.len();

                    let (detail_budget, data_budget) =
                        Self::compute_space_budget(max_pop, table_header_lines,
                            total_detail_vis, total_rows, min_rows);

                    // 1. Detail (header() output — no background, stands out).
                    let mut detail_vis_used = 0usize;
                    for (d, &vis) in plan.detail.iter().zip(&detail_visual) {
                        if detail_vis_used + vis > detail_budget { break; }
                        result.push(Self::render_logical_line(d, tw, false, false, false));
                        detail_vis_used += vis;
                    }

                    // 2. Table header (column labels, grey background).
                    if let Some(ref th) = plan.table_header {
                        result.push(Self::render_logical_line(th, tw, false, false, true));
                    }

                    // 3. Data rows, windowed around `selected` (visual-line-aware).
                    let total = plan.rows.len();
                    if total > 0 {
                        let sel_idx = plan.selected.unwrap_or(0);
                        let (win_start, win_count) =
                            Self::visual_row_window(&plan.rows, sel_idx, data_budget);
                        for i in win_start..(win_start + win_count).min(total) {
                            let is_selected = plan.selected == Some(i);
                            let is_active = plan.active_rows.contains(&i);
                            let row = &plan.rows[i];
                            let sub_lines: Vec<&str> = row.0.split('\n').collect();
                            for (j, sub) in sub_lines.iter().enumerate() {
                                let first = j == 0;
                                let marker = if first {
                                    if is_active { "* " }
                                    else if is_selected { "> " }
                                    else { "  " }
                                } else {
                                    "  "
                                };
                                let bg = if is_selected { "\x1b[48;5;239m" } else { "\x1b[48;5;235m" };
                                let fg = if is_selected { "\x1b[1;36m" } else { "" };
                                let content = format!("{}{}", marker, sub);
                                let vis = crate::slashdot_menu::table::display_width(&content);
                                let pad = tw.saturating_sub(vis);
                                result.push(format!("{}{}{}{}{}", bg, fg, content, " ".repeat(pad), "\x1b[0m"));
                            }
                        }
                    }
                    result.join("\n")
                }
                _ => Self::overlay_lines(title.as_str(), &opts, None, sel, active, wrap),
            };
            let overlay_vec: Vec<&str> = overlay_text.lines().collect();
            let focus = {
                let sc = sel.min(opts.len());
                1 + opts[..sc].iter().map(|o| o.matches('\n').count() + 1).sum::<usize>()
            };
            redraw(&editor, &overlay_vec, focus, &mut input_row);
            prev_term_h.set(cur_h);

            // --- Read and dispatch one key. ---
            match event::read() {
                Ok(Event::Key(key)) => {
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
                    // Active overlay key dispatch via enum methods.
                    if !matches!(overlay, ActiveOverlay::None) {
                        match key.code {
                            KeyCode::Down | KeyCode::Up => {
                                // v2.5: arrows route through the engine's
                                // cursor; the next frame's `sel` re-derives
                                // from it. Host-cursor overlays
                                // (PathComplete) move `sel` directly.
                                if overlay.cursor_row().is_some() {
                                    if let Some(handle) = &mut v25_handle {
                                        let delta: i32 = if key.code == KeyCode::Down { 1 } else { -1 };
                                        let _ = Self::v25_eval(
                                            handle,
                                            &editor.text(),
                                            editor.cursor,
                                            crate::slashdot_menu::HostKey::MoveCursor(delta),
                                        );
                                    }
                                    continue;
                                }
                                if key.code == KeyCode::Down {
                                    if sel + 1 < opts.len() { sel += 1; }
                                } else {
                                    if sel > 0 { sel -= 1; }
                                }
                            }
                            KeyCode::PageDown | KeyCode::PageUp => {
                                if matches!(overlay, ActiveOverlay::V25Menu { .. }) {
                                    // V25Menu: visual-line-aware page scroll through engine.
                                    if let Some(handle) = &mut v25_handle {
                                        // Get current selected from the plan.
                                        let cur_sel = match &overlay {
                                            ActiveOverlay::V25Menu { plan } => plan.selected.unwrap_or(0),
                                            _ => 0,
                                        };
                                        // Compute page budget (same formula as overlay rendering).
                                        let (cur_h, _) = crossterm::terminal::size().unwrap_or((80, 24));
                                        let cur_h = cur_h.max(1) as usize;
                                        let max_pop = ((cur_h as f64 * 2.0 / 3.0).ceil() as usize).max(1);
                                        let (plan_rows, detail_vis, th_lines) = match &overlay {
                                            ActiveOverlay::V25Menu { plan } => {
                                                let dv: usize = plan.detail.iter()
                                                    .map(|d| d.0.split('\n').count())
                                                    .sum();
                                                (&plan.rows, dv, if plan.table_header.is_some() { 1 } else { 0 })
                                            }
                                            _ => unreachable!(),
                                        };
                                        let (_, data_budget) = Self::compute_space_budget(
                                            max_pop, th_lines, detail_vis,
                                            plan_rows.len(), 3,
                                        );
                                        let direction = if matches!(key.code, KeyCode::PageDown) { 1 } else { -1 };
                                        let delta = Self::page_delta(plan_rows, cur_sel, data_budget, direction);
                                        if delta != 0 {
                                            let _ = Self::v25_eval(
                                                handle,
                                                &editor.text(),
                                                editor.cursor,
                                                crate::slashdot_menu::HostKey::MoveCursor(delta),
                                            );
                                        }
                                    }
                                    continue;
                                }
                                let page = {
                                    let (_, h) = crossterm::terminal::size().unwrap_or((80, 24));
                                    (h as usize).saturating_sub(2).max(1)
                                };
                                if matches!(key.code, KeyCode::PageDown) {
                                    sel = (sel + page).min(opts.len().saturating_sub(1));
                                } else {
                                    sel = sel.saturating_sub(page);
                                }
                            }
                            KeyCode::Tab => {
                                if matches!(overlay, ActiveOverlay::V25Menu { .. }) {
                                    if let Some(handle) = &mut v25_handle {
                                        let result = Self::v25_eval(
                                            handle,
                                            &editor.text(),
                                            editor.cursor,
                                            crate::slashdot_menu::HostKey::Tab,
                                        );
                                        if let crate::slashdot_menu::node_impl::MenuResult::Continue(plan, prompt) = result {
                                            // Same direct-render rule as the Enter arm.
                                            let new_buf = format!("/{}", prompt);
                                            let unchanged =
                                                new_buf.trim_end() == editor.text().trim_end();
                                            editor.set(&new_buf);
                                            if unchanged && Self::plan_has_content(&plan) {
                                                v25_enter_plan = Some(plan);
                                            }
                                        }
                                    }
                                } else {
                                    overlay.handle_tab(sel, &mut editor,
                                        &mut path_completions, &mut completion_pos);
                                }
                            }
                            KeyCode::Enter => {
                                if !key.modifiers.contains(KeyModifiers::SHIFT) {
                                    // v2.5 path: route through the persistent handle.
                                    if matches!(overlay, ActiveOverlay::V25Menu { .. }) {
                                        if let Some(handle) = &mut v25_handle {
                                            let result = Self::v25_eval(
                                                handle,
                                                &editor.text(),
                                                editor.cursor,
                                                crate::slashdot_menu::HostKey::Enter,
                                            );
                                            match result {
                                                crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(msg, prompt) => {
                                                    // The node's submit() wrote to shared state
                                                    // (LiveState for `/thinking`/`/model`/…, or
                                                    // `pending_op` for `/session`/`/replay`).
                                                    // Use the engine-provided self-documenting
                                                    // prompt (e.g. "/thinking low").
                                                    let committed = format!("/{}", prompt);
                                                    Repl::emit_commit(
                                                        output, &mut editor, &redraw, sel,
                                                        &mut input_row, &committed, "", false,
                                                    );
                                                    if let Some(text) = msg {
                                                        output.display_text(&text);
                                                    }
                                                    // Dismiss the overlay and hand control
                                                    // back to the agent loop, which processes
                                                    // `menu_submitted` (LiveState sync) and any
                                                    // `pending_op` (async dispatch) before the
                                                    // next prompt.
                                                    *menu_submitted = true;
                                                    return Some(InputAction::MenuDone);
                                                }
                                                crate::slashdot_menu::node_impl::MenuResult::Continue(plan, prompt) => {
                                                    // Rewrite (drill / NewPath): update the
                                                    // buffer; next frame re-evals. Prompt
                                                    // unchanged (modulo trailing whitespace —
                                                    // the drill's structural space) + a plan
                                                    // with content: render it directly for
                                                    // one frame — not re-derivable (the
                                                    // SubmitResult::Error line).
                                                    let new_buf = format!("/{}", prompt);
                                                    let unchanged =
                                                        new_buf.trim_end() == editor.text().trim_end();
                                                    editor.set(&new_buf);
                                                    if unchanged
                                                        && Self::plan_has_content(&plan)
                                                    {
                                                        v25_enter_plan = Some(plan);
                                                    }
                                                }
                                                crate::slashdot_menu::node_impl::MenuResult::DoneError(err, prompt) => {
                                                    let committed = format!("/{}", prompt);
                                                    Repl::emit_commit(
                                                        output, &mut editor, &redraw, sel,
                                                        &mut input_row, &committed, "", false,
                                                    );
                                                    output.display_text(&err);
                                                    return Some(InputAction::MenuDone);
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                    overlay.handle_enter(sel, &mut editor);
                                }
                            }
                            KeyCode::Esc => {
                                // Phase 6.6 item 3: engine-backed overlays
                                // route Esc through the engine's per-level
                                // ladder (frame replay + esc()); legacy
                                // overlays keep their bespoke rewrites until
                                // they migrate.
                                if matches!(overlay, ActiveOverlay::V25Menu { .. }) {
                                    if let Some(handle) = &mut v25_handle {
                                        let result = Self::v25_eval(
                                            handle,
                                            &editor.text(),
                                            editor.cursor,
                                            crate::slashdot_menu::HostKey::Esc,
                                        );
                                        match result {
                                            crate::slashdot_menu::node_impl::MenuResult::Continue(_plan, prompt) => {
                                                // Esc popped a level — update buffer.
                                                editor.set(&format!("/{}", prompt));
                                            }
                                            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(_, _) => {
                                                // Esc at root — close overlay.
                                                editor.set("");
                                                v25_handle = None;
                                            }
                                            _ => {
                                                editor.set("");
                                                v25_handle = None;
                                            }
                                        }
                                    } else {
                                        editor.set("");
                                    }
                                    continue;
                                }
                                overlay.handle_esc(&mut path_completions, &mut completion_pos);
                            }
                            _ => {
                                // Editing key while path-completion overlay is active
                                // dismisses it (user is typing something else).
                                if matches!(overlay, ActiveOverlay::PathComplete { .. }) {
                                    path_completions.clear();
                                    completion_pos = None;
                                }
                                if let Some(a) = Self::edit_key(
                                    key, &mut editor, history, &mut nav, &mut draft, prompt)
                                {
                                    if let InputAction::Submit(_) = &a {
                                        redraw(&LineEditor::new(), &[], sel, &mut input_row);
                                    }
                                    return Some(a);
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
    /// Render a LogicalLine as a single ANSI-styled string. Used for
    /// table_header and detail lines (not data rows, which are rendered
    /// inline with markers). Splits on `\n` for multi-line logical lines.
    pub fn render_logical_line(
        line: &crate::slashdot_menu::render::LogicalLine,
        tw: usize,
        is_selected: bool,
        is_active: bool,
        is_header: bool,
    ) -> String {
        let bg = if is_header { "\x1b[48;5;237m" }
                 else if is_selected { "\x1b[48;5;239m" }
                 else { "" };
        let fg = if is_selected { "\x1b[1;36m" } else { "" };
        let mut parts = Vec::new();
        for (j, sub) in line.0.split('\n').enumerate() {
            let marker = if j == 0 {
                if is_active { "* " }
                else if is_selected { "> " }
                else { "  " }
            } else { "  " };
            let content = format!("{}{}", marker, sub);
            let vis = crate::slashdot_menu::table::display_width(&content);
            let pad = tw.saturating_sub(vis);
            parts.push(format!("{}{}{}{}{}", bg, fg, content, " ".repeat(pad), "\x1b[0m"));
        }
        parts.join("\n")
    }

    /// Compute vertical space budgets for detail and data rows within the
    /// overlay. Returns `(detail_budget, rows_budget)` in visual lines.
    ///
    /// When both sections overflow, they split available space 50:50.
    /// If one section fits in its half, the other gets the surplus.
    /// Rows always get at least `min_rows` (capped at `total_rows`).
    pub fn compute_space_budget(
        overlay_budget: usize,   // total overlay lines (e.g. 2/3 terminal)
        table_header_lines: usize,
        total_detail_vis: usize, // visual lines all detail items consume
        total_rows: usize,
        min_rows: usize,
    ) -> (usize, usize) {
        let available = overlay_budget.saturating_sub(table_header_lines);
        let half = available / 2;
        let min_rows_guaranteed = min_rows.min(total_rows);

        // When rows overflow their half, detail is capped at half so rows
        // are not starved.  When rows fit, detail may use up to
        // `available - min_rows` (the surplus flows from rows' unused half).
        let detail_limit = if total_rows > half {
            // Both overflow (or rows do): detail capped at half.
            half
        } else {
            // Rows fit: detail may use their unused half.
            available.saturating_sub(min_rows_guaranteed)
        };
        let detail_budget = total_detail_vis.min(detail_limit);

        // Rows get the remaining space, at least `min_rows_guaranteed`.
        // When rows fit in their half, cap at total_rows (the renderer
        // shows `min(total, budget)` anyway, but a tighter budget is
        // clearer for callers).
        let rows_budget = if total_rows > half {
            available.saturating_sub(detail_budget).max(min_rows_guaranteed)
        } else {
            total_rows
        };

        (detail_budget, rows_budget)
    }

    /// Compute the window of row indices to render, accounting for multi-line
    /// `LogicalLine` rows.  Returns `(start, count)` — the first row index and
    /// how many logical rows to render.  `budget` is in visual lines.
    pub fn visual_row_window(
        rows: &[crate::slashdot_menu::render::LogicalLine],
        selected: usize,
        budget: usize,
    ) -> (usize, usize) {
        let total = rows.len();
        if total == 0 || budget == 0 {
            return (0, 0);
        }
        let heights: Vec<usize> = rows.iter()
            .map(|r| r.0.split('\n').count().max(1))
            .collect();
        let total_vis: usize = heights.iter().sum();
        if total_vis <= budget {
            return (0, total);
        }
        let sel = selected.min(total - 1);
        let half = budget / 2;

        // Walk backward from sel, using at most half the budget for rows above.
        let mut start = sel;
        let mut vis = heights[sel]; // always include the selected row
        while start > 0 && vis + heights[start - 1] <= half {
            start -= 1;
            vis += heights[start];
        }

        // Walk forward from start, filling the full budget.
        let mut count = 0;
        let mut vis_total = 0;
        for i in start..total {
            if vis_total + heights[i] > budget {
                break;
            }
            vis_total += heights[i];
            count += 1;
        }

        // If we hit the end of the list before filling the budget,
        // extend backward to use remaining space (avoids empty gap
        // below the window when selected is near the end).
        if start > 0 && vis_total < budget {
            while start > 0 {
                let h = heights[start - 1];
                if vis_total + h > budget {
                    break;
                }
                start -= 1;
                vis_total += h;
                count += 1;
            }
        }

        // At least the selected row (may exceed budget if it's very tall).
        if count == 0 {
            count = 1;
        }

        (start, count)
    }

    /// start index into the full overlay. Extracted so it can be unit-tested
    /// without a TTY.
    pub fn overlay_window(overlay_len: usize, avail: usize, sel: usize) -> usize {
        if overlay_len <= avail || avail == 0 {
            0
        } else {
            sel.saturating_sub(avail / 2).min(overlay_len - avail)
        }
    }

    /// Compute the number of logical rows to skip for a page scroll,
    /// accounting for multi-line (wrapped) rows. Returns a signed delta
    /// (positive = down, negative = up) that moves `selected` by at most
    /// `budget` visual lines, clamped to [0, total_rows).
    pub fn page_delta(
        rows: &[crate::slashdot_menu::render::LogicalLine],
        selected: usize,
        budget: usize,
        direction: i32,
    ) -> i32 {
        let total = rows.len();
        if total == 0 || budget == 0 {
            return 0;
        }
        let mut visual_used = 0usize;
        let mut delta = 0i32;
        if direction > 0 {
            // PageDown: count forward from selected+1
            let mut i = selected + 1;
            while i < total {
                let h = rows[i].0.lines().count().max(1);
                if visual_used + h > budget && delta > 0 {
                    break;
                }
                visual_used += h;
                delta += 1;
                i += 1;
            }
        } else {
            // PageUp: count backward from selected
            let mut i = selected;
            while i > 0 {
                let h = rows[i - 1].0.lines().count().max(1);
                if visual_used + h > budget && delta < 0 {
                    break;
                }
                i -= 1;
                visual_used += h;
                delta -= 1;
            }
        }
        delta
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
    /// Evaluate the v2.5 menu with the editor's real cursor: char index
    /// → byte offset into the slash-stripped line. v1 completion is
    /// end-of-line only — a mid-line cursor simply suppresses it (the
    /// engine's EOL guard), which avoids completing a different word
    /// than the user is editing.
    /// True when a `RenderPlan` carries something to paint (column
    /// header, detail lines, or rows) — i.e. it is not the empty
    /// `RenderPlan::default()` that `Prepared`/`NewPath`/no-drill
    /// outcomes return (those must keep re-deriving from the buffer).
    fn plan_has_content(plan: &crate::slashdot_menu::render::RenderPlan) -> bool {
        plan.table_header.is_some() || !plan.detail.is_empty() || !plan.rows.is_empty()
    }

    fn v25_eval(
        handle: &mut crate::slashdot_menu::SlashDotHandle<'_>,
        buf: &str,
        cursor_char: usize,
        key: crate::slashdot_menu::HostKey,
    ) -> crate::slashdot_menu::node_impl::MenuResult {
        let line = buf.trim_start_matches('/');
        let byte_in_buf = buf
            .char_indices()
            .nth(cursor_char)
            .map(|(b, _)| b)
            .unwrap_or(buf.len());
        let cursor = byte_in_buf.saturating_sub(buf.len() - line.len());
        handle.eval_prompt_with_cursor(line, key, crate::output::term_width(), cursor)
    }

    fn cursor_sel_policy(disc_changed: bool, sel: usize) -> usize {
        if disc_changed { 0 } else { sel }
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
    fn replace_last_token_replaces_tail() {
        assert_eq!(Repl::replace_last_token("/set default_model x:y", "ABC"), "/set default_model ABC");
        assert_eq!(Repl::replace_last_token("/model gpt", "gpt-4"), "/model gpt-4");
        // no space -> whole string replaced
        assert_eq!(Repl::replace_last_token("/set", "X"), "X");
    }









































    // ---- `/compact` overlay resolution ----





    // ------------------------------------------------------------------
    // /tree list rendering: turn grouping, no tools, compaction scope
    // ------------------------------------------------------------------




















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
    fn page_delta_skips_rows_by_visual_height() {
        use crate::slashdot_menu::render::LogicalLine;
        // Single-line rows: page_delta(4 budget) from row 0 down should skip 4 rows.
        let rows: Vec<LogicalLine> = (0..10).map(|i| LogicalLine::new(format!("row {}", i))).collect();
        assert_eq!(Repl::page_delta(&rows, 0, 4, 1), 4);
        // PageUp from row 9 with budget 4: skip 4 rows back to row 5.
        assert_eq!(Repl::page_delta(&rows, 9, 4, -1), -4);
        // Clamping: PageDown from row 7 with budget 100: only 2 rows left.
        assert_eq!(Repl::page_delta(&rows, 7, 100, 1), 2);
        // Clamping: PageUp from row 1 with budget 100: only 1 row above.
        assert_eq!(Repl::page_delta(&rows, 1, 100, -1), -1);
        // Multi-line rows: a row with 3 newlines = 4 visual lines.
        let mixed = vec![
            LogicalLine::new("short"),
            LogicalLine::new("line1\nline2\nline3\nline4"),
            LogicalLine::new("short"),
            LogicalLine::new("a\nb"),
        ];
        // From row 0, budget 5: row1 uses 4 lines, row2 uses 1 line = 5 total → delta 2.
        assert_eq!(Repl::page_delta(&mixed, 0, 5, 1), 2);
        // From row 3, budget 3: row2 has 1 line (fits), row1 has 4 lines (exceeds budget) → delta -1.
        assert_eq!(Repl::page_delta(&mixed, 3, 3, -1), -1);
        // Empty rows: no delta.
        assert_eq!(Repl::page_delta(&[], 0, 10, 1), 0);
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

    // --- compute_space_budget tests ---

    #[test]
    fn budget_both_fit() {
        // Both detail (5 vis lines) and rows (3) fit in overlay.
        // overlay_budget=20, table_header=1, available=19, half=9.
        let (d, r) = Repl::compute_space_budget(20, 1, 5, 3, 3);
        assert_eq!(d, 5, "detail gets what it needs");
        assert_eq!(r, 3, "rows get what they need");
    }

    #[test]
    fn budget_both_overflow_50_50() {
        // Detail has 30 vis lines, rows has 30. available=39, half=19.
        // Both overflow: detail capped at 19, rows get 20.
        let (d, r) = Repl::compute_space_budget(40, 1, 30, 30, 3);
        assert_eq!(d, 19);
        assert_eq!(r, 20);
    }

    #[test]
    fn budget_detail_overflows_rows_dont() {
        // Detail has 20 vis lines, rows has 2. available=19, half=9.
        // Rows fit (2 < 9), min_rows_guaranteed=min(3,2)=2.
        // detail gets available - 2 = 17.
        let (d, r) = Repl::compute_space_budget(20, 1, 20, 2, 3);
        assert_eq!(d, 17, "detail uses rows' surplus");
        assert_eq!(r, 2, "rows capped at total_rows");
    }

    #[test]
    fn budget_rows_overflow_detail_doesnt() {
        // Detail has 3 vis lines, rows has 30. available=19, half=9.
        // Detail fits (3 < 9): detail gets 3, rows get 16.
        let (d, r) = Repl::compute_space_budget(20, 1, 3, 30, 3);
        assert_eq!(d, 3, "detail gets what it needs");
        assert_eq!(r, 16, "rows get the rest");
    }

    #[test]
    fn budget_min_rows_enforced_even_when_detail_wants_more() {
        // Detail wants all 20 lines, rows have 2 (less than min_rows=3).
        // min_rows_guaranteed = min(3, 2) = 2.
        // available = 19. detail_limit = 19 - 2 = 17. detail = min(20, 17) = 17.
        // rows = max(19 - 17, 2) = 2.
        let (d, r) = Repl::compute_space_budget(20, 1, 20, 2, 3);
        assert_eq!(d, 17);
        assert_eq!(r, 2, "rows capped at total_rows");
    }

    #[test]
    fn budget_no_rows() {
        // No rows: all space goes to detail.
        let (d, r) = Repl::compute_space_budget(20, 1, 15, 0, 3);
        assert_eq!(d, 15);
        assert_eq!(r, 0, "no rows to allocate");
    }

    #[test]
    fn budget_no_table_header() {
        // No table header: available = overlay_budget.
        let (d, r) = Repl::compute_space_budget(18, 0, 5, 5, 3);
        assert_eq!(d, 5);
        assert_eq!(r, 5);
    }

    #[test]
    fn budget_small_terminal() {
        // Tiny terminal: overlay_budget=6, table_header=1, available=5.
        // Both overflow: half=2, detail=2, rows=3.
        let (d, r) = Repl::compute_space_budget(6, 1, 10, 10, 3);
        assert_eq!(d, 2);
        assert_eq!(r, 3, "min_rows enforced");
    }

    // --- visual_row_window tests ---

    #[test]
    fn vw_single_line_rows() {
        use crate::slashdot_menu::render::LogicalLine;
        let rows: Vec<LogicalLine> = (0..10)
            .map(|i| LogicalLine::new(format!("row {}", i)))
            .collect();
        // Budget 4: all single-line, window centered on sel=5.
        let (s, c) = Repl::visual_row_window(&rows, 5, 4);
        assert_eq!(c, 4);
        assert!(s <= 5 && s + c > 5, "selected must be in window");
    }

    #[test]
    fn vw_multi_line_row_centered() {
        use crate::slashdot_menu::render::LogicalLine;
        // row2 is 3 lines tall, budget=4.
        let rows = vec![
            LogicalLine::new("a"),
            LogicalLine::new("b"),
            LogicalLine::new("x\ny\nz"),  // 3 visual lines
            LogicalLine::new("c"),
            LogicalLine::new("d"),
        ];
        let (s, c) = Repl::visual_row_window(&rows, 2, 4);
        // Can fit: row2 (3 lines) + one 1-line row = 4 lines.
        assert!(s <= 2 && s + c > 2, "selected must be in window");
        let vis: usize = rows[s..s + c].iter()
            .map(|r| r.0.split('\n').count())
            .sum();
        assert!(vis <= 4, "window must fit budget: got {}", vis);
    }

    #[test]
    fn vw_all_fit() {
        use crate::slashdot_menu::render::LogicalLine;
        let rows = vec![
            LogicalLine::new("a"),
            LogicalLine::new("b"),
            LogicalLine::new("c"),
        ];
        // Budget is large enough for all rows.
        let (s, c) = Repl::visual_row_window(&rows, 0, 100);
        assert_eq!(s, 0);
        assert_eq!(c, 3);
    }

    #[test]
    fn vw_very_tall_row_always_shown() {
        use crate::slashdot_menu::render::LogicalLine;
        // Single row taller than budget.
        let rows = vec![LogicalLine::new("a\nb\nc\nd\ne")];
        let (s, c) = Repl::visual_row_window(&rows, 0, 2);
        assert_eq!(s, 0);
        assert_eq!(c, 1, "tall row is always included");
    }

    #[test]
    fn vw_empty_rows() {
        use crate::slashdot_menu::render::LogicalLine;
        let rows: Vec<LogicalLine> = vec![];
        let (s, c) = Repl::visual_row_window(&rows, 0, 10);
        assert_eq!(s, 0);
        assert_eq!(c, 0);
    }

    #[test]
    fn vw_selected_near_end_fills_downward() {
        use crate::slashdot_menu::render::LogicalLine;
        // 100 single-line rows, selected at end, budget=20.
        // Window should extend backward to fill all 20 lines.
        let rows: Vec<LogicalLine> = (0..100)
            .map(|i| LogicalLine::new(format!("row {}", i)))
            .collect();
        let (s, c) = Repl::visual_row_window(&rows, 99, 20);
        assert_eq!(c, 20, "must use full budget");
        assert_eq!(s, 80, "window starts 20 rows before end");
        assert!(s <= 99 && s + c > 99, "selected in window");
    }

    #[test]
    fn vw_selected_near_start_fills_forward() {
        use crate::slashdot_menu::render::LogicalLine;
        let rows: Vec<LogicalLine> = (0..100)
            .map(|i| LogicalLine::new(format!("row {}", i)))
            .collect();
        let (s, c) = Repl::visual_row_window(&rows, 0, 20);
        assert_eq!(c, 20, "must use full budget");
        assert_eq!(s, 0, "starts at 0");
    }
}

