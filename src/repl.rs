//! Raw-mode REPL input layer (Stages 2–4).
//!
//! - [`TermGuard`] owns terminal raw-mode lifecycle + capability negotiation
//!   (plan §3.5.2) and is safe to call from `Drop`/panic hook/signal handler.
//! - [`Repl`] is the raw-mode driver. [`Repl::read_input`] is a multi-line
//!   line editor with the v2.5 slash-menu frame.
//!
//! The line editor keeps a single flat `Vec<char>` buffer (with `'\n'` as a
//! hard line break) plus a char-index cursor. Rendering wraps the buffer to
//! visual rows via [`layout`] (word-boundary wrap, CJK char-wrap) and positions
//! the cursor. All editor *logic* is pure and unit-tested; the on-screen frame
//! is composed and applied by the shared draw pipeline (`crate::ui::frame`,
//! plan `doc/ui_draw_v2.md`).

use crate::agent::SessionUsage;
use crate::app_state::AppState;
use crate::model_catalog;
use crate::output::Output;
use crate::session::SessionTreeNode;
use crate::ui::frame::{allocate, live_budget, render, ApplyPolicy, Element, Extent, Frame};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::cell::Cell;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Current terminal width in columns, updated by the in-flight reader
/// on `Event::Resize`. Defaults to 80; renderers read this to pick up
/// size changes mid-stream.
pub(crate) static TERMINAL_WIDTH: AtomicUsize = AtomicUsize::new(80);

/// Get the current terminal width (updated by the in-flight reader).
/// Falls back to 80 if not yet initialized.
pub(crate) fn current_term_width() -> usize {
    TERMINAL_WIDTH.load(Ordering::SeqCst).max(20)
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
                Ok(Event::Resize(w, _h)) => {
                    // Update the shared terminal width so renderers pick up
                    // the change on their next streaming call.
                    TERMINAL_WIDTH.store(w as usize, Ordering::SeqCst);
                }
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
    /// A v2.5 menu committed a value (node wrote to `AppState` — the
    /// runtime-target config and/or control fields). The agent loop's
    /// convergence step applies targets at the cycle point; this variant
    /// is a no-op so the committed text is never treated as a prompt.
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
    /// Copied into `AppState` (`update_cycle_view`) — the one source the
    /// menu reads; no snapshot copies per activation.
    pub tree: std::sync::Arc<Vec<SessionTreeNode>>,
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

/// The input-region overlay currently shown: none, the v2.5 menu frame,
/// or the prompt-line (prose) completion dialog. Both plan-carrying
/// variants are pure display snapshots — key dispatch routes through the
/// engine that produced the plan (slash handle / prose completer).
pub(crate) enum ActiveOverlay {
    None,

    /// v2.5 engine overlay: carries a `RenderPlan` from the v2.5
    /// `SlashDotHandle::eval_prompt`. All key dispatch goes through the
    /// persistent handle — the overlay is purely a display snapshot.
    V25Menu {
        /// The render plan from the v2.5 engine.
        plan: crate::slashdot_menu::render::RenderPlan,
    },

    /// Prose completion dialog: a `RenderPlan` from
    /// `prompt_complete::PromptCompletion::eval`. Tab splices the
    /// cursor row's id through the prose layer; Enter/Esc bypass
    /// (submit the raw line / no-op — stateless dismissal).
    PathComplete {
        plan: crate::slashdot_menu::render::RenderPlan,
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
        let opts: Vec<String> = plan.rows.iter().map(|l| l.0.clone()).collect();
        (opts, plan.active_rows.first().copied())
    }

    /// Display parameters: `(logical options, active index)` for
    /// cursor/focus math. Both plan-carrying variants decompose the
    /// same way; painting goes through `paint_plan` directly.
    fn display_rows(&self) -> (Vec<String>, Option<usize>) {
        match self {
            ActiveOverlay::None => (vec![], None),
            ActiveOverlay::V25Menu { plan } | ActiveOverlay::PathComplete { plan } => {
                if plan.rows.is_empty() {
                    return (vec!["(no results)".into()], None);
                }
                Self::plan_rows(plan)
            }
        }
    }

    /// The engine-owned cursor row for this overlay (both variants are
    /// plan-backed). `None` for no overlay / a plan without rows.
    fn cursor_row(&self) -> Option<usize> {
        match self {
            ActiveOverlay::None => None,
            ActiveOverlay::V25Menu { plan } | ActiveOverlay::PathComplete { plan } => plan.selected,
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

/// Result of [`Repl::v25_eval`]: a `MenuResult` with the prompt-rewrite
/// case (`ContinuePrompt`) already resolved. The helper re-evals rewrite
/// chains to completion, so callers only ever see a frame to render
/// (`Continue`) or a terminal outcome — this type encodes that.
enum V25Frame {
    Continue(crate::slashdot_menu::render::RenderPlan, String),
    DoneSuccess(Option<String>, String),
    DoneError(String, String),
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

    /// Read a multi-line input in raw mode, with the v2.5 slash-menu frame.
    ///
    /// Returns `Some(InputAction::Submit(text))` with the submitted prompt,
    /// `Some(InputAction::MenuDone)` when a v2.5 menu commit wrote shared
    /// state (no text is treated as a prompt), or `None` only on a read
    /// error. `Ctrl-C` restores the terminal and exits the process.
    ///
    /// `history` backs `Up`/`Down` navigation and is persisted to disk.
    ///
    /// Typing `/` opens the v2.5 menu frame in the input region; keys
    /// dispatch through the persistent `SlashDotHandle` and `Esc` dismisses
    /// the frame. Prompt-line Tab completion uses its own `PathComplete`
    /// overlay.
    ///
    /// The input line sits at the natural cursor row (re-anchored via DSR on
    /// entry) and is *not* forced to the screen bottom, so closing an overlay
    /// never leaves a gap in the scrollback. Drawing goes through the unified
    /// frame pipeline (plan `doc/ui_draw_v2.md` §5.2): `redraw` composes
    /// `[Input, Divider?, Overlay?]`, and `Output::draw_frame` anchors
    /// (`frame_anchor()`), scrolls and clears within the live region — all
    /// clearing is confined to the frame region and below, so the greeter /
    /// status / session content above the input is never touched. The whole
    /// frame uses at most 2/3 of the terminal height (≥ 1 row).
    pub async fn read_input(
        &mut self,
        prompt: &str,
        initial_buffer: Option<&str>,
        history: &mut History,
        ctx: &ReplCtx,
        app_state: &Arc<AppState>,
        output: &dyn Output,
    ) -> Option<InputAction> {
        // Re-anchor the draw cache to the real cursor (DSR) before the first
        // paint: content streamed since the last prompt is not tracked row by
        // row, and the input must start at the natural cursor row (plan §4.2).
        // From here the REPL anchors through the cache (`frame_anchor()`).
        let _ = output.cursor_row();
        // Refresh the menu-facing view from this cycle's context. The loop
        // is the sole writer of these display fields (one acquisition);
        // menu nodes read them live through the same handle — no snapshot
        // copies, no sync flags.
        app_state.update_cycle_view(
            ctx.current_session_id.clone(),
            ctx.session_usage,
            ctx.context_tokens,
            ctx.tree.clone(),
            ctx.leaf_id.clone(),
        );
        // Repo handle: refresh in case the loop replaced its local Option
        // (clone-shared Arc cache — same listing state either way).
        app_state.set_repo(ctx.repo.clone());
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
        let mut last_disc: u8 = 0; // overlay discriminant for sel-reset tracking
                                   // v2.5 engine: persistent menu tree + guard-like handle.
                                   // The menu lives for the entire prompt session; the handle is
                                   // created when the prompt starts with `/` and dropped when it
                                   // no longer does. All `/` dispatch goes through `eval_prompt`.
        let mut v25_menu = crate::v25_menu::build_menu(app_state);
        let mut v25_handle: Option<crate::slashdot_menu::menu::SlashDotHandle<'_>> = None;
        // Prose (non-`/`) completion: one instance for the prompt
        // session, parallel to the slash handle. Spawns its FS worker;
        // the worker exits when this (and its `Sender`) drops.
        let mut prose = crate::prompt_complete::PromptCompletion::new();
        // Engine key queued by the dispatch below for the *next*
        // iteration's eval (eval → display → wait loop shape). `None`
        // = passive frame (buffer edits, first iteration). Consumed
        // unconditionally at the top, so a stale key can never outlive
        // one iteration.
        let mut pending_key: Option<crate::slashdot_menu::HostKey> = None;
        // Terminal size at the last paint, `(w, h)`. Resizes are consumed
        // without repainting (see the wait below), so a mismatch at the top of
        // the loop is the "repaint is due, and re-anchor first" signal (plan
        // §4.2); a *width* change additionally wipes + replays first. The
        // clear bound this pair used to provide is `plan_apply`'s
        // `screen_bottom` now (plan §4.3). Interior mutability (`Cell`) lets
        // the loop read it without holding a mutable borrow.
        let painted_size = Cell::new(
            crossterm::terminal::size()
                .map(|(w, h)| (w.max(1) as usize, h.max(1) as usize))
                .unwrap_or((80, 24)),
        );

        // Compose → allocate → render → apply the input frame (plan §5.2):
        // `[Input]`, or `[Input, Divider, Overlay]` while a pop-up shows.
        // The apply phase owns the rest — `frame_anchor()` tracks the live
        // frame's top (so a tall pop-up's scroll is followed without a row
        // counter), the scroll `delta` and the bounded clear live in
        // `plan_apply`. Content *above* the frame (greeter / status / session)
        // is never touched. The closure carries no state.
        let redraw = |editor: &LineEditor, overlay: &[&str], focus: usize| {
            let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
            let frame = Self::compose_frame(
                editor,
                prompt,
                overlay,
                focus,
                w.max(1) as usize,
                h.max(1) as usize,
            );
            output.draw_frame(&frame, output.frame_anchor(), ApplyPolicy::Frame);
        };
        loop {
            // On resume from Ctrl-Z (editing phase; the in-flight reader thread
            // is not running then), re-enter raw mode so the next keystroke is
            // read correctly. Idempotent; a no-op on every normal iteration.
            // The shell may have repainted while we were stopped, so re-anchor
            // the draw cache (DSR) before the next paint (plan §4.2).
            if RESUMED.swap(false, Ordering::SeqCst) {
                let _ = self.guard.start();
                let _ = output.cursor_row();
            }
            // Resize detection against `painted_size`. Resize events are
            // consumed without repainting (see the wait below), so after a drag
            // this check first fires on the keystroke that triggers the
            // repaint: a size mismatch means "repaint is due, and re-anchor
            // first" — reflow may have moved the cursor, so the cached frame
            // rows can no longer be trusted (plan §4.2). The clear boundary a
            // stale `prev_term_h` used to provide lives in `plan_apply`
            // (`screen_bottom`) now, so a grow's revealed scrollback survives
            // without any state here.
            let (cur_w_u16, cur_h_u16) = crossterm::terminal::size().unwrap_or((80, 24));
            let cur_h = cur_h_u16 as usize;
            let cur_w = cur_w_u16 as usize;
            let (painted_w, painted_h) = painted_size.get();
            if cur_w != painted_w || cur_h != painted_h {
                // Key-triggered repaint after a size change. A *width*
                // change makes a reflowing terminal (wezterm) re-wrap the
                // painted frame, stacking wrapped copies of the input and
                // overlay that no clear can locate precisely. Instead:
                // wipe everything from the cursor down (the reflow leaves
                // it at the end of the content — the best possible
                // starting point), replay the last turn through the normal
                // scrollback (pushing any corruption above the fold), then
                // re-anchor; the usual redraw below repaints the input.
                // Skipped without a session — nothing to replay and usually
                // nothing to clear up. Height-only changes don't re-wrap,
                // so they keep the cheap re-anchor path.
                if cur_w != painted_w {
                    if let Some(s) = app_state.session() {
                        let _ = io::stdout().write_all(b"\x1b[0J");
                        let _ = io::stdout().flush();
                        crate::agent::replay_last_turn_silent(&s, output).await;
                    }
                }
                let _ = output.cursor_row();
            }
            // Keep the shared width in sync (covers width changes even without
            // an explicit Resize event, e.g. on some terminals).
            TERMINAL_WIDTH.store(cur_w, Ordering::SeqCst);
            // --- Eval: resolve the pending key (or a passive frame)
            // into an overlay. Single place where an engine prompt is
            // applied to the buffer AND its plan rendered, so a §5.1
            // keyless restore lands both in the same frame — no window
            // for a racing edit between them. ---
            let key = pending_key
                .take()
                .unwrap_or(crate::slashdot_menu::HostKey::None);
            let buf_text = editor.text();
            let overlay = if buf_text.starts_with('/') {
                // All `/` commands route through the v2.5 engine.
                // Manage handle lifecycle: create on first `/`, drop when not `/.
                if v25_handle.is_none() {
                    v25_handle = Some(v25_menu.get_handle());
                }
                let result = {
                    let handle = v25_handle.as_mut().unwrap();
                    Self::v25_eval(handle, &buf_text, editor.cursor, key).await
                };
                match result {
                    V25Frame::Continue(plan, prompt) => {
                        // Apply the engine's prompt — but only when it
                        // actually changed: an unchanged passive eval
                        // must not reset a mid-line text cursor
                        // (`LineEditor::set` jumps to EOL).
                        let new_buf = format!("/{}", prompt);
                        if new_buf != editor.text() {
                            editor.set(&new_buf);
                        }
                        ActiveOverlay::V25Menu { plan }
                    }
                    V25Frame::DoneSuccess(msg, prompt)
                        if matches!(key, crate::slashdot_menu::HostKey::Enter) =>
                    {
                        // The node's submit() wrote to `AppState` (target
                        // config for `/thinking`/`/model`/…, control fields
                        // for `/session`/`/replay`/…). Use the
                        // engine-provided self-documenting prompt
                        // (e.g. "/thinking low").
                        let committed = format!("/{}", prompt);
                        Repl::emit_commit(
                            output,
                            &mut editor,
                            &redraw,
                            sel,
                            &committed,
                            "",
                            false,
                            history,
                        );
                        if let Some(text) = msg {
                            output.display_text(&text);
                        }
                        // Dismiss the overlay and hand control back to the
                        // agent loop, whose convergence step applies the
                        // targets written during this eval.
                        return Some(InputAction::MenuDone);
                    }
                    V25Frame::DoneError(err, prompt)
                        if matches!(key, crate::slashdot_menu::HostKey::Enter) =>
                    {
                        let committed = format!("/{}", prompt);
                        Repl::emit_commit(
                            output,
                            &mut editor,
                            &redraw,
                            sel,
                            &committed,
                            "",
                            false,
                            history,
                        );
                        output.display_text(&err);
                        return Some(InputAction::MenuDone);
                    }
                    // Rewrite-cycle bail-out on a non-Enter key (chain
                    // already on stderr from v25_eval): clear the input
                    // area, surface it in the transcript, and drop the
                    // prompt — back to normal operation.
                    V25Frame::DoneError(err, _) => {
                        // Clear the input region through the frame path, then
                        // surface the error. The exact line `display_text`
                        // consumed depends on inline-markdown wrapping, so
                        // re-anchor (DSR) rather than trusting the wrap count
                        // ("any uncertainty → clear it", plan §4.2): the next
                        // iteration anchors below the error.
                        redraw(&LineEditor::new(), &[], sel);
                        output.display_text(&err);
                        let _ = output.cursor_row();
                        editor.set("");
                        v25_handle = None;
                        ActiveOverlay::None
                    }
                    // Esc at root — close the overlay.
                    V25Frame::DoneSuccess(_, _) => {
                        editor.set("");
                        v25_handle = None;
                        ActiveOverlay::None
                    }
                }
            } else {
                // Drop the v25 handle when prompt leaves `/`.
                v25_handle = None;
                // Prose frames: one eval drives the word gate, the
                // debounce, and the dialog. Idle ⇒ no overlay (stateless
                // dismissal: the next frame re-derives from the buffer).
                let ctx = crate::slashdot_menu::RenderContext {
                    term_width: crate::output::term_width().max(1),
                };
                match prose.eval(&mut editor, key, ctx) {
                    crate::prompt_complete::Frame::Idle => ActiveOverlay::None,
                    crate::prompt_complete::Frame::Dialog(plan) => {
                        ActiveOverlay::PathComplete { plan }
                    }
                }
            };
            // Hoisted before the sel-reset so `active` (the menu's
            // default/current-state row) can drive the cursor policy below.
            let (raw_opts, active) = overlay.display_rows();
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
                if cur_disc != last_disc {
                    last_disc = cur_disc;
                }
            } else {
                sel = Self::cursor_sel_policy(cur_disc != last_disc, sel);
            }
            if cur_disc != last_disc {
                last_disc = cur_disc;
            }
            let opts = raw_opts;
            let max_sel = opts.len().saturating_sub(1);
            if sel > max_sel {
                sel = max_sel;
            }
            let overlay_text = match &overlay {
                ActiveOverlay::None => String::new(),
                ActiveOverlay::V25Menu { plan } | ActiveOverlay::PathComplete { plan } => {
                    crate::slashdot_menu::render::paint_plan(plan, cur_w, cur_h)
                }
            };
            let overlay_vec: Vec<&str> = overlay_text.lines().collect();
            let focus = {
                let sc = sel.min(opts.len());
                1 + opts[..sc]
                    .iter()
                    .map(|o| o.matches('\n').count() + 1)
                    .sum::<usize>()
            };
            redraw(&editor, &overlay_vec, focus);
            painted_size.set((cur_w, cur_h));

            // --- Wait for a key (or a prose-completion wake). While a
            // prose request is armed/in flight, poll with a deadline so
            // the dialog can open (or the debounce fire) without a
            // keystroke; otherwise block as before. Slash frames block —
            // prose state is not consulted while `/` owns the buffer.
            //
            // Resize events are consumed *here, without repainting*. A drag
            // emits a stream of them, and repainting per event writes
            // absolute-positioned frames while a reflowing terminal is still
            // moving rows around (wezterm) — interleaving that strands stale
            // frames; xterm doesn't reflow, which is why it never corrupted.
            // The painted frame is left for the terminal to reflow on its own;
            // the next keystroke re-anchors via DSR at the top of the loop and
            // repaints once, against the settled size. ---
            let prose_wait = if editor.chars().first() == Some(&'/') {
                None
            } else {
                prose.poll_timeout()
            };
            let ev: Option<Event> = loop {
                let e = match prose_wait {
                    Some(d) => match event::poll(d) {
                        Ok(true) => event::read(),
                        Ok(false) => break None, // prose wake: re-eval
                        Err(_) => return None,
                    },
                    None => event::read(),
                };
                match e {
                    Ok(Event::Resize(w, _)) => {
                        // Track the width for renderers; the repaint is
                        // deferred to the next key / content wake.
                        TERMINAL_WIDTH.store(w as usize, Ordering::SeqCst);
                        continue;
                    }
                    Ok(e) => break Some(e),
                    Err(_) => return None,
                }
            };
            // --- Dispatch one key. ---
            match ev {
                Some(Event::Key(key)) => {
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
                                        format!("\r\neditor error: {}\r\n", e).as_bytes(),
                                    );
                                }
                            }
                            let _ = self.guard.start();
                            // The editor owned the screen while we were out:
                            // re-anchor the draw cache (DSR) before the next
                            // paint (plan §4.2). A *height* change the editor
                            // caused is folded into `painted_size` here (the
                            // re-anchor already covered it), while a width
                            // change is left for the wipe + replay at the
                            // loop top.
                            let _ = output.cursor_row();
                            if let Ok((_, h)) = crossterm::terminal::size() {
                                let (w, _) = painted_size.get();
                                painted_size.set((w, h.max(1) as usize));
                            }
                            continue;
                        }
                    }
                    // Active overlay key dispatch via enum methods.
                    if !matches!(overlay, ActiveOverlay::None) {
                        match key.code {
                            KeyCode::Down | KeyCode::Up => {
                                // Engine-owned cursor (slash handle or prose
                                // completer): queue the delta for the next
                                // eval — it applies the result in one place.
                                // The host-`sel` fallback below is for the
                                // empty-plan V25Menu case only.
                                if overlay.cursor_row().is_some() {
                                    let delta: i32 = if key.code == KeyCode::Down { 1 } else { -1 };
                                    pending_key =
                                        Some(crate::slashdot_menu::HostKey::MoveCursor(delta));
                                    continue;
                                }
                                if key.code == KeyCode::Down {
                                    if sel + 1 < opts.len() {
                                        sel += 1;
                                    }
                                } else {
                                    if sel > 0 {
                                        sel -= 1;
                                    }
                                }
                            }
                            KeyCode::PageDown | KeyCode::PageUp => {
                                // Plan-carrying overlays: visual-line-aware
                                // page scroll through the engine that owns
                                // the cursor (slash handle / prose).
                                let plan = match &overlay {
                                    ActiveOverlay::V25Menu { plan }
                                    | ActiveOverlay::PathComplete { plan } => Some(plan),
                                    ActiveOverlay::None => None,
                                };
                                if let Some(plan) = plan {
                                    if !matches!(overlay, ActiveOverlay::PathComplete { .. })
                                        && v25_handle.is_none()
                                    {
                                        continue; // V25 overlay without its handle
                                    }
                                    let cur_sel = plan.selected.unwrap_or(0);
                                    // Compute page budget (same formula as overlay rendering).
                                    let (cur_h, _) =
                                        crossterm::terminal::size().unwrap_or((80, 24));
                                    let cur_h = cur_h.max(1) as usize;
                                    let max_pop =
                                        ((cur_h as f64 * 2.0 / 3.0).ceil() as usize).max(1);
                                    let dv: usize =
                                        plan.detail.iter().map(|d| d.0.split('\n').count()).sum();
                                    let th_lines = if plan.table_header.is_some() { 1 } else { 0 };
                                    let (_, data_budget) =
                                        crate::slashdot_menu::render::compute_space_budget(
                                            max_pop,
                                            th_lines,
                                            dv,
                                            plan.rows.len(),
                                            3,
                                        );
                                    let direction = if matches!(key.code, KeyCode::PageDown) {
                                        1
                                    } else {
                                        -1
                                    };
                                    let delta = crate::slashdot_menu::render::page_delta(
                                        &plan.rows,
                                        cur_sel,
                                        data_budget,
                                        direction,
                                    );
                                    if delta != 0 {
                                        pending_key =
                                            Some(crate::slashdot_menu::HostKey::MoveCursor(delta));
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
                                // Both plan-carrying overlays route Tab
                                // through the engine that produced the plan:
                                // slash drills/expands, prose splices the
                                // selected id over the word.
                                if matches!(
                                    overlay,
                                    ActiveOverlay::V25Menu { .. }
                                        | ActiveOverlay::PathComplete { .. }
                                ) {
                                    pending_key = Some(crate::slashdot_menu::HostKey::Tab);
                                }
                            }
                            KeyCode::Enter => {
                                if matches!(overlay, ActiveOverlay::V25Menu { .. }) {
                                    if !key.modifiers.contains(KeyModifiers::SHIFT) {
                                        // Slash: queue the key; the next
                                        // iteration's eval handles submit
                                        // (commit + return) or rewrite inline.
                                        pending_key = Some(crate::slashdot_menu::HostKey::Enter);
                                    }
                                    continue;
                                }
                                // Prose dialog: Enter bypasses completion —
                                // submit the raw line (Shift+Enter inserts a
                                // newline). Tab is the only expansion key.
                                if let Some(a) = Self::edit_key(
                                    key,
                                    &mut editor,
                                    history,
                                    &mut nav,
                                    &mut draft,
                                    prompt,
                                ) {
                                    if let InputAction::Submit(_) = &a {
                                        redraw(&LineEditor::new(), &[], sel);
                                    }
                                    return Some(a);
                                }
                                continue;
                            }
                            KeyCode::Esc => {
                                // Slash engine-backed overlays route Esc
                                // through the engine's per-level ladder
                                // (frame replay + esc()). Prose: Esc is a
                                // no-op — dismissal is buffer-driven
                                // (stateless); in plain editing Esc is a
                                // no-op too, so this is uniform.
                                if matches!(overlay, ActiveOverlay::V25Menu { .. }) {
                                    pending_key = Some(crate::slashdot_menu::HostKey::Esc);
                                    continue;
                                }
                            }
                            _ => {
                                // Editing keys pass through in both cases;
                                // dismissal is stateless — the next eval
                                // re-derives the dialog from the buffer.
                                if let Some(a) = Self::edit_key(
                                    key,
                                    &mut editor,
                                    history,
                                    &mut nav,
                                    &mut draft,
                                    prompt,
                                ) {
                                    if let InputAction::Submit(_) = &a {
                                        redraw(&LineEditor::new(), &[], sel);
                                    }
                                    return Some(a);
                                }
                            }
                        }
                        continue;
                    }
                    // No overlay: normal editing (Tab with no dialog is
                    // inert — implicit open means the dialog is already up
                    // whenever there is anything to complete).
                    if let Some(a) =
                        Self::edit_key(key, &mut editor, history, &mut nav, &mut draft, prompt)
                    {
                        if let InputAction::Submit(_) = &a {
                            redraw(&LineEditor::new(), &[], sel);
                        }
                        return Some(a);
                    }
                }
                Some(Event::Paste(text)) => {
                    editor.insert_str(&text);
                }
                Some(_) => {} // other events (mouse): ignored
                None => {}    // prose wake with no key: re-eval below
            }
        }
    }

    /// Compose → allocate → render the REPL's input frame (plan §3, pure):
    /// `[Input]`, or `[Input, Divider, Overlay]` while `overlay` shows.
    ///
    /// `focus` is the overlay's windowing index (the pre-computed visual row
    /// of the selection); `term_w`/`term_h` are the size the frame is composed
    /// for — the same values `Output::draw_frame` applies it with. The frame
    /// (input + divider + pop-up together) never exceeds the live-region
    /// budget of ⌈2/3·term_h⌉; overflow is windowed (`render`) and, if the
    /// frame does not fit below its anchor, scrolled in by `plan_apply`.
    ///
    /// Shared by `read_input`'s `redraw` closure and the vt100 tests, so both
    /// drive the exact bytes production emits (ui_draw_v2 P0/P3).
    pub(crate) fn compose_frame(
        editor: &LineEditor,
        prompt: &str,
        overlay: &[&str],
        focus: usize,
        term_w: usize,
        term_h: usize,
    ) -> Frame {
        let term_w = term_w.max(1);
        let layout = editor.layout(term_w, prompt);
        let mut elements = vec![Element::Input {
            layout,
            prompt: prompt.to_string(),
        }];
        if !overlay.is_empty() {
            elements.push(Element::Divider);
            elements.push(Element::Overlay {
                rows: overlay.iter().map(|s| (*s).to_string()).collect(),
                focus,
            });
        }
        let extents: Vec<Extent> = elements.iter().map(Element::extent).collect();
        let heights = allocate(&extents, live_budget(term_h.max(1)));
        render(&elements, &heights, term_w)
    }

    /// Evaluate one menu key event (editor's real cursor: char index →
    /// byte offset into the slash-stripped line; v1 completion is
    /// end-of-line only, so a mid-line cursor just suppresses it —
    /// the engine's EOL guard), resolving `ContinuePrompt` rewrites.
    ///
    /// The engine answers a pure prompt rewrite (Esc ladder step, drill
    /// rewrite, completion expansion, `SubmitResult::NewPath`) with
    /// `ContinuePrompt(p)`: the prompt is authoritative and the frame
    /// must be re-derived from it. This helper applies the rewrite
    /// locally and re-evals with `HostKey::None` until a plan-carrying
    /// (`Continue`) or terminal (`Done*`) result arrives — so callers
    /// only ever see frames, never half-rewrites.
    ///
    /// The loop is bounded: an engine rewrite cycle is a bug, so we
    /// report the chain on stderr (debugging detail) and bail out with
    /// `DoneError` carrying the same message — never spin, never
    /// synthesize a plan.
    async fn v25_eval(
        handle: &mut crate::slashdot_menu::SlashDotHandle<'_>,
        buf: &str,
        cursor_char: usize,
        key: crate::slashdot_menu::HostKey,
    ) -> V25Frame {
        use crate::slashdot_menu::node_impl::MenuResult;
        // Max rewrites before we call it a cycle. Normal chains are
        // 1–2 steps (rewrite → passive re-derive).
        const MAX_REWRITES: usize = 16;
        let mut buf = buf.to_string();
        let mut cursor_char = cursor_char;
        let mut key = key;
        let mut chain: Vec<String> = Vec::new();
        for _ in 0..MAX_REWRITES {
            let line = buf.trim_start_matches('/').to_string();
            let byte_in_buf = buf
                .char_indices()
                .nth(cursor_char)
                .map(|(b, _)| b)
                .unwrap_or(buf.len());
            let cursor = byte_in_buf.saturating_sub(buf.len() - line.len());
            match handle
                .eval_prompt_with_cursor(&line, key, crate::output::term_width(), cursor)
                .await
            {
                MenuResult::ContinuePrompt(new_prompt) => {
                    chain.push(format!("/{}", new_prompt));
                    // Structural rewrite: the text cursor jumps to the
                    // end (same as `LineEditor::set` at the call sites).
                    // `cursor_char` indexes *buf* chars, which include
                    // the leading '/', hence `buf.chars().count()`.
                    buf = format!("/{}", new_prompt);
                    cursor_char = buf.chars().count();
                    key = crate::slashdot_menu::HostKey::None;
                }
                MenuResult::Continue(plan, prompt) => return V25Frame::Continue(plan, prompt),
                MenuResult::DoneSuccess(msg, prompt) => return V25Frame::DoneSuccess(msg, prompt),
                MenuResult::DoneError(err, prompt) => return V25Frame::DoneError(err, prompt),
            }
        }
        let msg = format!(
            "menu engine bug: prompt-rewrite cycle after {} steps: {}",
            MAX_REWRITES,
            chain.join(" -> ")
        );
        eprintln!("picocode: {}", msg);
        V25Frame::DoneError(msg, buf.trim_start_matches('/').to_string())
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
    fn cursor_sel_policy(disc_changed: bool, sel: usize) -> usize {
        if disc_changed {
            0
        } else {
            sel
        }
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
    /// from `read_input`; it clears the input region through the frame
    /// pipeline, after which `display_plain` writes the line at the frame's
    /// caret (plan §5.2).
    ///
    /// `sep` controls whether this helper draws the separator itself:
    /// `/set` value commits *stay in the `read_input` loop* (they revert
    /// to the list rather than returning an action), so no other separator
    /// will follow the emitted line — pass `true`. `/model` and `/thinking`
    /// *return* an `InputAction`, so `agent.rs` draws a separator before the
    /// next `read_input` (its per-prompt framing); passing `false` here avoids
    /// a doubled separator.
    fn emit_commit<F: Fn(&LineEditor, &[&str], usize)>(
        output: &dyn Output,
        editor: &mut LineEditor,
        redraw: &F,
        sel: usize,
        line: &str,
        next_input: &str,
        sep: bool,
        history: &mut History,
    ) {
        // v2.5 menu commits (`/replay 1`, `/thinking low`, …) return
        // `MenuDone` and never pass through the `Submit` arm in agent.rs
        // where typed input is recorded — record the committed line here
        // so recall (Up/Down) sees every command the user actually ran.
        history.add(line);
        // No DSR here: the draw cache is the steady-state anchor
        // (`frame_anchor()`), content writes advance it, and a foreign
        // screen writer ($EDITOR via `review_compaction_summary`) re-anchors
        // on exit (plan §4.2). Clear the input region, then echo the line
        // at the caret the frame apply left.
        redraw(&LineEditor::new(), &[], sel);
        output.display_plain(line);
        if sep {
            output.display_separator();
        }
        editor.set(next_input);
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
    // The editor is a foreign screen writer: re-anchor the draw cache to the
    // real cursor (DSR) and invalidate the frame region before anything else
    // draws (plan §4.2).
    let _ = output.cursor_row();

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

    /// The buffer as a char slice (no copy) — word extraction for the
    /// prose completion layer, which works entirely in char space.
    pub fn chars(&self) -> &[char] {
        &self.buf
    }

    /// Current cursor position (char index into the buffer).
    pub fn cursor(&self) -> usize {
        self.cursor
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
            if let Ok(mut fh) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(f)
            {
                let _ = fh.write_all(line.as_bytes());
                let _ = fh.write_all(b"\n");
            }
        }
    }

    /// Step backward through history; first call stashes `current` as the draft.
    pub fn prev(
        &self,
        current: &str,
        nav: &mut Option<usize>,
        draft: &mut String,
    ) -> Option<String> {
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

    // ---- `/compact` overlay resolution ----

    // ------------------------------------------------------------------
    // /tree list rendering: turn grouping, no tools, compaction scope
    // ------------------------------------------------------------------

    #[test]
    fn paint_plan_active_row_keeps_star_when_not_selected() {
        // Port of the old `overlay_lines` marker tests (ui_draw_v2 P5):
        // the `*` current-state marker owns the 2-char prefix and does not
        // depend on the cursor — an active row that is NOT selected keeps
        // `* `, and the cursor row keeps `> `. (active == selected, where
        // `* ` wins over `> `, is pinned by
        // `paint_plan_active_marker_beats_cursor`.)
        //
        // The old per-row BG override test went away with
        // `overlay_lines_with_bgs`: paint_plan has fixed
        // selected/default backgrounds and no override feature (nothing
        // used it).
        use crate::slashdot_menu::render::{paint_plan, LogicalLine, RenderPlan};
        use std::rc::Rc;
        let plan = RenderPlan {
            rows: Rc::new(vec![LogicalLine::new("one"), LogicalLine::new("two")]),
            selected: Some(0),
            active_rows: vec![1],
            ..Default::default()
        };
        let painted = paint_plan(&plan, 40, 20);
        assert!(
            painted.lines().any(|l| l.contains("* two")),
            "got: {painted:?}"
        );
        assert!(
            painted.lines().any(|l| l.contains("> one")),
            "got: {painted:?}"
        );
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
    fn history_navigation_with_draft_restore() {
        let mut h = History {
            entries: Vec::new(),
            file: None,
        };
        h.add("first");
        h.add("second");
        let mut nav: Option<usize> = None;
        let mut draft = String::new();
        assert_eq!(
            h.prev("draft", &mut nav, &mut draft),
            Some("second".to_string())
        );
        assert_eq!(
            h.prev("draft", &mut nav, &mut draft),
            Some("first".to_string())
        );
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
        assert_eq!(
            editor.text(),
            "in progress",
            "Up must not clobber the unsubmitted prompt"
        );
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
        assert_eq!(
            editor.text(),
            "second",
            "empty buffer should load latest history entry"
        );
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
        // Ported to paint_plan (ui_draw_v2 P5): the title lives in the
        // plan's detail block, rows keep their `> `/`  ` markers.
        use crate::slashdot_menu::render::{paint_plan, LogicalLine, RenderPlan};
        use std::rc::Rc;
        let plan = RenderPlan {
            detail: vec![LogicalLine::new("Select a model:")],
            rows: Rc::new(vec![
                LogicalLine::new("a: alpha model ctx 128000 $0.10/0.20 per M"),
                LogicalLine::new("b: beta model ctx 200000 $0.30/0.40 per M"),
            ]),
            selected: Some(0),
            ..Default::default()
        };
        let out = paint_plan(&plan, 80, 40);
        assert_eq!(out.matches("Select a model:").count(), 1);
        assert!(out.contains("> a: alpha"));
        assert!(out.contains("  b: beta"));
    }

    #[test]
    fn render_select_wraps_long_option_with_aligned_continuation() {
        // Ported to paint_plan (ui_draw_v2 P5): the option arrives
        // pre-wrapped (word wrap now happens where the plan is built, as
        // the v2.5 engine does); paint_plan must keep the first line
        // marked and every continuation indented under the option text.
        use crate::slashdot_menu::render::{paint_plan, LogicalLine, RenderPlan};
        use std::rc::Rc;
        let long = "deepseek:deepseek-v4-flash ctx 1048576 $0.000000435/0.00000087 per M (est) and some extra trailing text to force a wrap";
        let wrapped = crate::output::wrap(long, 60);
        assert!(wrapped.len() > 1, "long option must wrap at 60 cols");
        let plan = RenderPlan {
            rows: Rc::new(vec![LogicalLine::new(wrapped.join("\n"))]),
            selected: Some(0),
            ..Default::default()
        };
        let out = paint_plan(&plan, 60, 20);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines[0].contains("> "),
            "first row is the marker row: {lines:?}"
        );
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
    fn page_delta_skips_rows_by_visual_height() {
        use crate::slashdot_menu::render::LogicalLine;
        // Single-line rows: page_delta(4 budget) from row 0 down should skip 4 rows.
        let rows: Vec<LogicalLine> = (0..10)
            .map(|i| LogicalLine::new(format!("row {}", i)))
            .collect();
        assert_eq!(crate::slashdot_menu::render::page_delta(&rows, 0, 4, 1), 4);
        // PageUp from row 9 with budget 4: skip 4 rows back to row 5.
        assert_eq!(
            crate::slashdot_menu::render::page_delta(&rows, 9, 4, -1),
            -4
        );
        // Clamping: PageDown from row 7 with budget 100: only 2 rows left.
        assert_eq!(
            crate::slashdot_menu::render::page_delta(&rows, 7, 100, 1),
            2
        );
        // Clamping: PageUp from row 1 with budget 100: only 1 row above.
        assert_eq!(
            crate::slashdot_menu::render::page_delta(&rows, 1, 100, -1),
            -1
        );
        // Multi-line rows: a row with 3 newlines = 4 visual lines.
        let mixed = vec![
            LogicalLine::new("short"),
            LogicalLine::new("line1\nline2\nline3\nline4"),
            LogicalLine::new("short"),
            LogicalLine::new("a\nb"),
        ];
        // From row 0, budget 5: row1 uses 4 lines, row2 uses 1 line = 5 total → delta 2.
        assert_eq!(crate::slashdot_menu::render::page_delta(&mixed, 0, 5, 1), 2);
        // From row 3, budget 3: row2 has 1 line (fits), row1 has 4 lines (exceeds budget) → delta -1.
        assert_eq!(
            crate::slashdot_menu::render::page_delta(&mixed, 3, 3, -1),
            -1
        );
        // Empty rows: no delta.
        assert_eq!(crate::slashdot_menu::render::page_delta(&[], 0, 10, 1), 0);
    }

    // `next_input_row` / `clear_rows_to` were folded into `plan_apply` by
    // ui_draw_v2 P3; their cases live on in `ui::frame::apply_tests`
    // (`frame_tracks_scroll_then_stays_on_shrink`,
    // `full_clears_nothing_when_frame_sits_below_the_old_bottom`,
    // `grow_preserves_revealed_rows_below_old_bottom`).

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
            layout.cursor_row,
            rc,
            e.text()
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
                content_col,
                row_len,
                layout.rows,
                e.text(),
                e.cursor
            );
        }
    }

    const STEP_KINDS: usize = 10;

    fn apply_step(e: &mut LineEditor, step: usize, width: usize, prompt: &str) {
        match step % STEP_KINDS {
            0 => {
                e.insert('a');
            }
            1 => {
                e.insert(' ');
            }
            2 => {
                e.insert('\n');
            }
            3 => {
                e.backspace();
            }
            4 => {
                e.left();
            }
            5 => {
                e.right();
            }
            6 => {
                e.home();
            }
            7 => {
                e.end();
            }
            8 => {
                e.insert('-');
            }
            9 => {
                e.insert_newline();
            }
            _ => unreachable!(),
        }
        check_layout(e, width, prompt);
    }

    #[test]
    fn fuzz_deterministic_sweep() {
        let params: Vec<(usize, &str)> = vec![
            (5, ""),
            (8, ""),
            (10, ""),
            (12, ""),
            (15, ""),
            (20, ""),
            (40, ""),
            (80, ""),
            (5, "> "),
            (8, "> "),
            (10, "> "),
            (12, "> "),
            (15, "> "),
            (20, "> "),
            (40, "> "),
            (80, "> "),
            (5, "$ "),
            (10, "$ "),
            (15, "$ "),
            (20, "$ "),
            (5, ">>>"),
            (10, ">>>"),
            (20, ">>>"),
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
            (
                "Multi-line input: There are still bugs that affect cursor",
                20,
                "> ",
            ),
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
            (
                "    fn main() {\n        println!(\"hello\");\n    }",
                30,
                "",
            ),
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
        for c in "abc def ghi".chars() {
            e.insert(c);
        }
        e.insert_newline();
        for c in "jk lm".chars() {
            e.insert(c);
        }
        assert_eq!(e.text(), "abc def ghi\njk lm");
        assert!(e.visual_up(w, prompt), "visual_up");
        while e.cursor > 0 {
            e.left();
            check_layout(&e, w, prompt);
        }
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
        for c in "some text and more".chars() {
            e.insert(c);
        }
        e.insert_newline();
        for c in "and even more text in this line".chars() {
            e.insert(c);
        }
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
        for c in "abc def ghi jkl mno".chars() {
            e.insert(c);
        }
        e.insert_newline();
        for c in "stu vwx".chars() {
            e.insert(c);
        }
        assert!(e.visual_up(w, prompt), "visual_up");
        while e.cursor > 0 {
            e.left();
            check_layout(&e, w, prompt);
        }
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
                while i > 0 && e.buf[i - 1] != '\n' {
                    i -= 1;
                }
                i
            };
            let end_pos = {
                let n = text.len();
                let mut i = e.cursor;
                while i < n && e.buf[i] != '\n' {
                    i += 1;
                }
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
        for c in "hello world".chars() {
            e.insert(c);
            check_layout(&e, 80, "> ");
        }
        for _ in 0.."hello world".len() {
            e.backspace();
            check_layout(&e, 80, "> ");
        }
        assert_eq!(e.text(), "");
        e.insert_str("abc");
        e.home();
        e.insert('X');
        check_layout(&e, 80, "> ");
        assert_eq!(e.text(), "Xabc");
    }

    #[test]
    fn fuzz_line_delete_keeps_invariants() {
        let tests = [
            "abc def ghi",
            "abc def ghi\njk lm",
            "\nhello",
            "hello\n",
            "a\nb\nc",
        ];
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
        // Ported to the new edit path: `word_at_cursor` finds the range,
        // `LineEditor::replace_range` splices the completion (the prose
        // layer does exactly this on Tab).
        use crate::prompt_complete::word_at_cursor;
        let tests = [
            ("/home/user", "/home/user/project"),
            ("/tmp", "/tmp/data"),
            ("foo", "foobar"),
        ];
        for (buf, replacement) in &tests {
            let chars: Vec<char> = buf.chars().collect();
            let range = word_at_cursor(&chars, chars.len()).unwrap_or(0..0);
            let mut e = LineEditor::new();
            e.insert_str(buf);
            e.replace_range(range.start, range.end, replacement);
            check_layout(&e, 80, "> ");
        }
    }

    /// Paint precedence: an active row shows `*` even when it is also the
    /// cursor row (`>` loses to `*`, Marker contract).
    #[test]
    fn paint_plan_active_marker_beats_cursor() {
        use crate::slashdot_menu::render::{paint_plan, LogicalLine, RenderPlan};
        use std::rc::Rc;
        let plan = RenderPlan {
            rows: Rc::new(vec![LogicalLine::new("x")]),
            selected: Some(0),
            active_rows: vec![0],
            ..Default::default()
        };
        let painted = paint_plan(&plan, 80, 40);
        assert!(
            painted.contains("* x"),
            "shared row shows `*`, not `>`: {painted:?}"
        );
    }

    // ===== vt100 terminal-emulator tests =====
    //
    // Build the frame production draws — `Repl::compose_frame` →
    // `plan_apply`, i.e. the exact bytes `Output::draw_frame` writes
    // (ONLCR-translated by [`onlcr`]) — and feed them through a real vt100
    // terminal emulator. This checks both visible cell content and the final
    // cursor position — the strongest test possible without a real TTY.
    #[cfg(test)]
    mod vt100_tests {
        use crate::repl::{LineEditor, Repl};
        use crate::ui::frame::{plan_apply, ApplyPolicy, DrawCache};
        use vt100::Parser;

        /// Simulate the tty's OPOST/ONLCR: `plan_apply` emits bare `\n` (the
        /// real terminal's line discipline maps it to `\r\n` before the
        /// emulator sees it). This harness feeds bytes straight to the vt100
        /// parser, so apply the same mapping here — and *only* here: nothing
        /// else in this module may re-encode newlines.
        fn onlcr(s: &str) -> Vec<u8> {
            s.replace('\n', "\r\n").into_bytes()
        }

        /// Render one full redraw — input *plus* an optional overlay — through
        /// the production pipeline ([`Repl::compose_frame`] + [`plan_apply`],
        /// ONLCR-translated by [`onlcr`]) into the emulator. The cache starts
        /// fresh (`live = None`, as after a re-anchor), so the apply takes the
        /// full path with the frame's top at `anchor`. Returns (cursor row,
        /// cursor col, screen rows, post-apply anchor).
        fn render_overlay_through_vt100(
            editor: &LineEditor,
            width: usize,
            prompt: &str,
            term_rows: usize,
            term_cols: usize,
            anchor: usize,
            overlay: &[&str],
            focus: usize,
        ) -> (usize, usize, Vec<String>, usize) {
            let frame = Repl::compose_frame(editor, prompt, overlay, focus, width, term_rows);
            let mut cache = DrawCache::new(term_rows);
            let s = plan_apply(
                &mut cache,
                &frame,
                anchor,
                ApplyPolicy::Frame,
                width,
                term_rows,
            );
            let mut p = Parser::new(term_rows as u16, term_cols as u16, 1000);
            p.process(&onlcr(&s));
            let screen = p.screen();
            let (cr, cc) = screen.cursor_position();
            let rows: Vec<String> = (0..term_rows)
                .map(|r| {
                    let mut s = String::new();
                    for c in 0..term_cols {
                        s.push(
                            screen
                                .cell(r as u16, c as u16)
                                .map(|cell| cell.contents())
                                .unwrap_or(" ")
                                .chars()
                                .next()
                                .unwrap_or(' '),
                        );
                    }
                    s.trim_end().to_string()
                })
                .collect();
            let anchor = cache.anchor();
            (cr as usize, cc as usize, rows, anchor)
        }

        /// Input-only variant (no overlay): the pre-existing entry point the
        /// cursor-positioning tests use.
        fn render_through_vt100(
            editor: &LineEditor,
            width: usize,
            prompt: &str,
            term_rows: usize,
            term_cols: usize,
            anchor: usize,
        ) -> (usize, usize, Vec<String>) {
            let (cr, cc, rows, _) = render_overlay_through_vt100(
                editor,
                width,
                prompt,
                term_rows,
                term_cols,
                anchor,
                &[],
                0,
            );
            (cr, cc, rows)
        }

        #[test]
        fn vt100_cursor_at_start_of_single_line() {
            let mut e = LineEditor::new();
            e.insert_str("Hello");
            e.cursor = 0;
            let (cr, cc, rows) = render_through_vt100(&e, 80, "> ", 24, 80, 10);
            // anchor=10 is 1-indexed; vt100 uses 0-indexed rows, so row = 9
            assert_eq!((cr, cc), (9, 2), "cursor at start should be row 9 col 2");
            assert!(rows[9].starts_with("> Hello"), "row 9: {:?}", rows[9]);
        }

        #[test]
        fn vt100_cursor_at_start_of_multi_line() {
            let mut e = LineEditor::new();
            e.insert_str("abc def ghi\njk lm");
            e.cursor = 0;
            let (cr, cc, rows) = render_through_vt100(&e, 80, "> ", 24, 80, 10);
            assert_eq!(
                (cr, cc),
                (9, 2),
                "cursor at start of multi-line should be row 9 col 2"
            );
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
            assert_eq!(
                (cr, cc),
                (10, 0),
                "cursor at start of second line should be row 10 col 0"
            );
            assert!(rows[10].starts_with("jk lm"), "row 10: {:?}", rows[10]);
        }

        #[test]
        fn vt100_user_exact_text_cursor_at_start() {
            let mut e = LineEditor::new();
            for c in "some text and more".chars() {
                e.insert(c);
            }
            e.insert_newline();
            for c in "and even more text in this line".chars() {
                e.insert(c);
            }
            e.cursor = 0;
            let (cr, cc, rows) = render_through_vt100(&e, 80, "> ", 24, 80, 10);
            // cursor_row=0 in layout → 0-indexed vt100 row = 9 + 0 = 9
            assert_eq!(
                (cr, cc),
                (9, 2),
                "cursor at start should be (9,2), got ({},{})",
                cr,
                cc
            );
            assert!(
                rows[9].starts_with("> some text and more"),
                "row 9: {:?}",
                rows[9]
            );
            assert!(
                rows[10].starts_with("and even more text"),
                "row 10: {:?}",
                rows[10]
            );
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
                // vt100 rows are 0-indexed; anchor=10 1-indexed = 9 0-indexed
                let expected_row = 9 + layout.cursor_row;
                assert_eq!(
                    (cr, cc),
                    (expected_row, layout.cursor_col),
                    "pos {}: expected ({},{}), got ({},{}); layout=({},{}), rows={:?}",
                    pos,
                    expected_row,
                    layout.cursor_col,
                    cr,
                    cc,
                    layout.cursor_row,
                    layout.cursor_col,
                    layout.rows
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
                        // anchor=5 1-indexed = 4 0-indexed
                        assert_eq!(
                            (cr, cc),
                            (4 + layout.cursor_row, layout.cursor_col),
                            "text={:?}, w={}, pos={}: rows={:?}",
                            text,
                            w,
                            pos,
                            layout.rows
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
            // anchor=5 1-indexed = 4 0-indexed. No prompt, so col=0.
            assert_eq!(
                (cr, cc),
                (4, 0),
                "cursor at start without prompt should be row 4 col 0"
            );
            assert!(rows[4].starts_with("Hello"), "row 4: {:?}", rows[4]);
        }

        // ---- overlay path (input + pop-up + scroll-delta caret) ----

        #[test]
        fn vt100_overlay_fits_no_scroll() {
            // Input at row 10 of 24, 3-row overlay: the frame (1 + 1 + 3)
            // fits both the screen and the budget, so nothing scrolls. The
            // overlay must appear directly below the input, the caret must
            // return to the input, and the live frame's anchor must not move.
            let mut e = LineEditor::new();
            e.insert_str("hi");
            let overlay = ["one", "two", "three"];
            let (cr, cc, rows, anchor) =
                render_overlay_through_vt100(&e, 80, "> ", 24, 80, 10, &overlay, 0);
            assert!(rows[9].starts_with("> hi"), "input row: {:?}", rows[9]);
            assert!(
                rows[10].chars().all(|c| c == '─'),
                "divider: {:?}",
                rows[10]
            );
            assert_eq!(rows[11], "one", "ov0: {:?}", rows[11]);
            assert_eq!(rows[12], "two");
            assert_eq!(rows[13], "three");
            // Caret back on the input line, at prompt(2) + "hi"(2) = col 4.
            assert_eq!((cr, cc), (9, 4), "caret should be on the input row");
            // No overflow: the frame stays anchored where it was drawn.
            assert_eq!(anchor, 10);
        }

        #[test]
        fn vt100_overlay_overflow_scrolls_and_tracks_input() {
            // Input at row 20 of a 24-row terminal with a 6-row overlay:
            // 20 + 1 (input) + 1 (divider) + 6 (overlay) = 28 rows, so
            // `plan_apply` scrolls by delta = 20 + 8 - 1 - 24 = 3 before
            // painting (scroll-then-paint, plan §4.3). The emulator's
            // *actual* input row must match the anchor the apply records in
            // the cache, and the caret must land on the input row — this is
            // exactly the bug class the old hand-rolled test copy could not
            // catch.
            let mut e = LineEditor::new();
            e.insert_str("hi");
            let overlay = ["one", "two", "three", "four", "five", "six"];
            let (cr, cc, rows, anchor) =
                render_overlay_through_vt100(&e, 80, "> ", 24, 80, 20, &overlay, 0);
            // delta = (20 + 8) - 1 - 24 = 3 → frame lands at row 17 (1-indexed).
            assert_eq!(anchor, 17, "the cache must track the scrolled row");
            // Find the input row in the emulator's screen: it must be at the
            // anchor (1-indexed) = index 16 (0-indexed).
            assert!(
                rows[16].starts_with("> hi"),
                "input must be at the reported row: {:?}",
                rows
            );
            assert!(rows[17].chars().all(|c| c == '─'), "divider below input");
            assert_eq!(rows[18], "one");
            assert_eq!(rows[23], "six", "last overlay row at screen bottom");
            // Caret tracks the input: row 16 (0-indexed), col 4.
            assert_eq!(
                (cr, cc),
                (16, 4),
                "caret must be repositioned to the scrolled input row"
            );
        }

        #[test]
        fn vt100_overlay_clamped_to_two_thirds_budget() {
            // The live region — input + divider + pop-up *together* — may use
            // at most ⌈2/3·term_h⌉ = 16 rows of a 24-row terminal (plan §2).
            // A 100-row overlay is flexed down to 16 - 2 = 14 rows and
            // windowed around the selection, so the frame fits at its anchor
            // (row 8 + 16 - 1 = 23 ≤ 24): no scroll, top 1/3 of the screen
            // stays scrollback.
            let e = LineEditor::new();
            let overlay: Vec<String> = (0..100).map(|i| format!("item {i}")).collect();
            let refs: Vec<&str> = overlay.iter().map(|s| s.as_str()).collect();
            let (cr, cc, rows, anchor) =
                render_overlay_through_vt100(&e, 80, "> ", 24, 80, 8, &refs, 50);
            // allocate: [input 1, divider 1, overlay 14] of budget 16.
            // The overlay window (14 rows, prefer 7 of context) centers on
            // focus=50: start = 50 - 7 = 43, so rows 43..=56 are shown.
            assert_eq!(anchor, 8, "frame fits: no scroll");
            assert_eq!(rows[7].trim_end(), ">", "empty input: {:?}", rows[7]);
            assert!(rows[8].chars().all(|c| c == '─'), "divider: {:?}", rows[8]);
            assert_eq!(rows[9], "item 43", "window start: {:?}", rows[9]);
            assert_eq!(rows[22], "item 56", "window end: {:?}", rows[22]);
            // The frame starts at row 8 (1-indexed): everything above is
            // untouched scrollback.
            assert!(
                rows[..7].iter().all(|r| r.is_empty()),
                "clean top: {:?}",
                &rows[..7]
            );
            // Caret on the (single-row) input: 0-idx row 7, col 2 (prompt end).
            assert_eq!((cr, cc), (7, 2));
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
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => unsafe {
                    std::env::set_var(k, val);
                },
                None => unsafe {
                    std::env::remove_var(k);
                },
            }
        }
        f();
        for (k, v) in saved {
            match v {
                Some(val) => unsafe {
                    std::env::set_var(&k, &val);
                },
                None => unsafe {
                    std::env::remove_var(&k);
                },
            }
        }
    }

    #[test]
    fn resolve_editor_prefers_visual() {
        with_env(
            &[("VISUAL", Some("myeditor")), ("EDITOR", Some("other"))],
            || {
                let (cmd, args) = super::resolve_editor();
                assert_eq!(cmd, "myeditor", "VISUAL must take priority");
                assert!(args.is_empty(), "no flags: {:?}", args);
            },
        );
    }

    #[test]
    fn resolve_editor_falls_back_to_editor() {
        with_env(&[("VISUAL", None), ("EDITOR", Some("nano"))], || {
            let (cmd, args) = super::resolve_editor();
            assert_eq!(cmd, "nano", "EDITOR fallback");
            assert!(args.is_empty());
        });
    }

    #[test]
    fn resolve_editor_defaults_to_vim() {
        with_env(&[("VISUAL", None), ("EDITOR", None)], || {
            let (cmd, args) = super::resolve_editor();
            assert_eq!(cmd, "vim", "default to vim");
            assert!(args.is_empty());
        });
    }

    #[test]
    fn resolve_editor_splits_flags() {
        with_env(
            &[
                ("VISUAL", Some("emacs -nw --no-site-file")),
                ("EDITOR", None),
            ],
            || {
                let (cmd, args) = super::resolve_editor();
                assert_eq!(cmd, "emacs");
                assert_eq!(args, vec!["-nw", "--no-site-file"]);
            },
        );
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

        with_env(
            &[
                ("VISUAL", Some(script.to_string_lossy().as_ref())),
                ("EDITOR", None),
            ],
            || {
                let result = super::edit_in_editor(current);
                assert!(result.is_ok(), "edit_in_editor failed: {:?}", result.err());
                assert_eq!(result.unwrap(), expected_output);
            },
        );
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
        mock_edit("", "printf '%s' 'new content' > \"$1\"\n", "new content");
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
        mock_edit("a\n\n", "printf '%s\\n\\n' 'a' > \"$1\"\n", "a\n");
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

        with_env(
            &[
                ("VISUAL", Some(script.to_string_lossy().as_ref())),
                ("EDITOR", None),
            ],
            || {
                let result = super::edit_in_editor("original");
                assert!(result.is_err(), "non-zero exit must return error");
            },
        );
    }

    #[test]
    fn edit_in_editor_nonexistent_command() {
        with_env(
            &[
                ("VISUAL", Some("/nonexistent/editor_binary")),
                ("EDITOR", None),
            ],
            || {
                let result = super::edit_in_editor("hello");
                assert!(result.is_err(), "nonexistent command must return error");
            },
        );
    }

    // --- compute_space_budget tests ---

    #[test]
    fn budget_both_fit() {
        // Both detail (5 vis lines) and rows (3) fit in overlay.
        // overlay_budget=20, table_header=1, available=19, half=9.
        let (d, r) = crate::slashdot_menu::render::compute_space_budget(20, 1, 5, 3, 3);
        assert_eq!(d, 5, "detail gets what it needs");
        assert_eq!(r, 3, "rows get what they need");
    }

    #[test]
    fn budget_both_overflow_50_50() {
        // Detail has 30 vis lines, rows has 30. available=39, half=19.
        // Both overflow: detail capped at 19, rows get 20.
        let (d, r) = crate::slashdot_menu::render::compute_space_budget(40, 1, 30, 30, 3);
        assert_eq!(d, 19);
        assert_eq!(r, 20);
    }

    #[test]
    fn budget_detail_overflows_rows_dont() {
        // Detail has 20 vis lines, rows has 2. available=19, half=9.
        // Rows fit (2 < 9), min_rows_guaranteed=min(3,2)=2.
        // detail gets available - 2 = 17.
        let (d, r) = crate::slashdot_menu::render::compute_space_budget(20, 1, 20, 2, 3);
        assert_eq!(d, 17, "detail uses rows' surplus");
        assert_eq!(r, 2, "rows capped at total_rows");
    }

    #[test]
    fn budget_rows_overflow_detail_doesnt() {
        // Detail has 3 vis lines, rows has 30. available=19, half=9.
        // Detail fits (3 < 9): detail gets 3, rows get 16.
        let (d, r) = crate::slashdot_menu::render::compute_space_budget(20, 1, 3, 30, 3);
        assert_eq!(d, 3, "detail gets what it needs");
        assert_eq!(r, 16, "rows get the rest");
    }

    #[test]
    fn budget_min_rows_enforced_even_when_detail_wants_more() {
        // Detail wants all 20 lines, rows have 2 (less than min_rows=3).
        // min_rows_guaranteed = min(3, 2) = 2.
        // available = 19. detail_limit = 19 - 2 = 17. detail = min(20, 17) = 17.
        // rows = max(19 - 17, 2) = 2.
        let (d, r) = crate::slashdot_menu::render::compute_space_budget(20, 1, 20, 2, 3);
        assert_eq!(d, 17);
        assert_eq!(r, 2, "rows capped at total_rows");
    }

    #[test]
    fn budget_no_rows() {
        // No rows: all space goes to detail.
        let (d, r) = crate::slashdot_menu::render::compute_space_budget(20, 1, 15, 0, 3);
        assert_eq!(d, 15);
        assert_eq!(r, 0, "no rows to allocate");
    }

    #[test]
    fn budget_no_table_header() {
        // No table header: available = overlay_budget.
        let (d, r) = crate::slashdot_menu::render::compute_space_budget(18, 0, 5, 5, 3);
        assert_eq!(d, 5);
        assert_eq!(r, 5);
    }

    #[test]
    fn budget_small_terminal() {
        // Tiny terminal: overlay_budget=6, table_header=1, available=5.
        // Both overflow: half=2, detail=2, rows=3.
        let (d, r) = crate::slashdot_menu::render::compute_space_budget(6, 1, 10, 10, 3);
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
        let (s, c) = crate::slashdot_menu::render::visual_row_window(&rows, 5, 4);
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
            LogicalLine::new("x\ny\nz"), // 3 visual lines
            LogicalLine::new("c"),
            LogicalLine::new("d"),
        ];
        let (s, c) = crate::slashdot_menu::render::visual_row_window(&rows, 2, 4);
        // Can fit: row2 (3 lines) + one 1-line row = 4 lines.
        assert!(s <= 2 && s + c > 2, "selected must be in window");
        let vis: usize = rows[s..s + c].iter().map(|r| r.0.split('\n').count()).sum();
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
        let (s, c) = crate::slashdot_menu::render::visual_row_window(&rows, 0, 100);
        assert_eq!(s, 0);
        assert_eq!(c, 3);
    }

    #[test]
    fn vw_very_tall_row_always_shown() {
        use crate::slashdot_menu::render::LogicalLine;
        // Single row taller than budget.
        let rows = vec![LogicalLine::new("a\nb\nc\nd\ne")];
        let (s, c) = crate::slashdot_menu::render::visual_row_window(&rows, 0, 2);
        assert_eq!(s, 0);
        assert_eq!(c, 1, "tall row is always included");
    }

    #[test]
    fn vw_empty_rows() {
        use crate::slashdot_menu::render::LogicalLine;
        let rows: Vec<LogicalLine> = vec![];
        let (s, c) = crate::slashdot_menu::render::visual_row_window(&rows, 0, 10);
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
        let (s, c) = crate::slashdot_menu::render::visual_row_window(&rows, 99, 20);
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
        let (s, c) = crate::slashdot_menu::render::visual_row_window(&rows, 0, 20);
        assert_eq!(c, 20, "must use full budget");
        assert_eq!(s, 0, "starts at 0");
    }
}
