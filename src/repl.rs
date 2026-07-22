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
use crate::output::{term_width, wrap, Output};
use crate::model_catalog;
use crate::slash;
use crate::thinking;
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
    /// Navigate the session tree to a target entry.
    NavigateTree(String),
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
    /// A short preview of the first user message in the session.
    pub first_prompt_preview: Option<String>,
}

/// A candidate value shown in a setting's value-pick overlay.
#[derive(Clone)]
pub(crate) struct ValueCandidate {
    display: String,
    value: String,
    desc: String,
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
        cmds: Vec<crate::fuzzy::Ranked<'static, slash::SlashCommand>>,
    },

    Model {
        entries: Vec<model_catalog::ModelEntry>,
        _filter: String,
        /// Index of the currently-active model (for the `*` marker).
        active: Option<usize>,
    },

    Thinking {
        levels: Vec<thinking::ThinkingLevel>,
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
        /// Entry IDs, 1:1 with `rows`, for Enter resolution.
        targets: Vec<String>,
        active: Option<usize>,
        _filter: String,
    },
}

impl ActiveOverlay {
    /// Construct the overlay from the current buffer and external state.
    fn from_buffer(
        buf: &str,
        ctx: &ReplCtx,
        settings: &SettingsStore,
        help_view: bool,
        path_completions: &[String],
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
        ) {
            (after, "")
        } else {
            ("", after)
        };
        match word {
            "model" | "models" | "m" => {
                let filter = rest.trim().to_string();
                let (entries, active) = Repl::model_overlay_entries(ctx, &filter);
                ActiveOverlay::Model { entries, _filter: filter, active }
            }
            "thinking" | "think" | "t" => {
                let filter = rest.trim().to_string();
                let levels = Repl::thinking_overlay_entries(&filter);
                ActiveOverlay::Thinking { levels }
            }
            "persona" | "personas" | "p" => {
                let filter = rest.trim().to_string();
                let cands = Repl::setting_value_candidates(
                    crate::settings::SettingKey::DefaultPersona,
                    ctx,
                );
                let filtered: Vec<ValueCandidate> = if filter.is_empty() {
                    cands
                } else {
                    crate::fuzzy::fuzzy_filter_bonus(&cands, &filter, |c| c.display.clone())
                        .into_iter()
                        .map(|r| r.item.clone())
                        .collect()
                };
                let active = filtered.iter().position(|c| {
                    !c.value.is_empty() && ctx.persona.as_deref() == Some(c.value.as_str())
                });
                ActiveOverlay::Persona { cands: filtered, _filter: filter, active }
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
                let lines = crate::agent::session_stats_lines(
                    &ctx.provider, &ctx.model, ctx.session_usage,
                );
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
            "sandbox" | "sb" => {
                let filter = rest.trim().to_string();
                let mut items = Vec::new();
                let mut active = None;
                let sub = Repl::sandbox_overlay_entries(&filter, &mut items, &mut active);
                ActiveOverlay::Sandbox { sub, items, _filter: filter }
            }
            "session" | "sessions" => {
                let filter = rest.trim().to_string();

                // Check for delete confirmation: buffer is `/session <name> delete`
                // or `/session <name> delete current` (for current session → /clear).
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
                    ActiveOverlay::SessionAction { session_name, is_current, header, opts, active, _filter: filter }
                } else {
                    let mut opts = Vec::new();
                    let mut active = None;
                    Repl::session_overlay_entries(ctx, &filter, &mut opts, &mut active);
                    let names: Vec<String> = filtered
                        .iter()
                        .map(|m| m.id.clone())
                        .collect();
                    ActiveOverlay::Session { opts, names, active, _filter: filter }
                }
            }
            "tree" => {
                let filter = rest.trim().to_string();
                let mut rows = Vec::new();
                let mut targets = Vec::new();
                let mut active = None;
                Repl::tree_overlay_entries(
                    &ctx.tree,
                    ctx.leaf_id.as_deref(),
                    &filter,
                    &mut rows,
                    &mut targets,
                    &mut active,
                );
                ActiveOverlay::Tree {
                    rows,
                    targets,
                    active,
                    _filter: filter,
                }
            }
            _ => {
                let cmds = slash::filter_commands(rest);
                ActiveOverlay::CommandList { cmds }
            }
        }
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
            ActiveOverlay::CommandList { cmds } => {
                let opts: Vec<String> = cmds.iter().map(|r| {
                    format!("{:<10} {}", r.item.name, r.item.help)
                }).collect();
                if opts.is_empty() {
                    ("Commands  (type to filter, up/down, Tab/Enter select)",
                     vec!["(no matching command)".into()], None, false)
                } else {
                    ("Commands  (type to filter, up/down, Tab/Enter select)",
                     opts, None, true)
                }
            }
            ActiveOverlay::Model { entries, active, .. } => {
                if entries.is_empty() {
                    return ("Models  (up/down or PgUp/PgDn, Enter select, Esc back)",
                            vec!["(loading models... or no matches)".into()],
                            None, false);
                }
                let avail = crate::output::term_width().saturating_sub(2);
                let mut table = Table::new(&["Model", "Ctx", "Cost", "?"])
                    .with_aligns(&[Align::Left, Align::Right, Align::Right, Align::Left])
                    .with_min_widths(&[12, 5, 14, 3])
                    .with_priorities(&[3, 1, 2, 1])
                    .set_gap(2);
                for e in entries {
                    table.add_row(&[
                        e.qualified(),
                        model_catalog::ctx_cell(e),
                        model_catalog::cost_cell(e),
                        model_catalog::reason_cell(e),
                    ]);
                }
                let rows = table.render_multiline(avail);
                let opts: Vec<String> = rows.into_iter().enumerate().map(|(i, row)| {
                    if model_catalog::provider_available(&entries[i].provider) {
                        row
                    } else {
                        row.lines()
                            .map(|l| format!("\x1b[2m{}\x1b[0m", l))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                }).collect();
                ("Models  (up/down or PgUp/PgDn, Enter select, Esc back)",
                 opts, *active, false)
            }
            ActiveOverlay::Thinking { levels } => {
                let opts: Vec<String> = levels.iter().map(|l| l.as_str().to_string()).collect();
                if opts.is_empty() {
                    ("Thinking levels  (up/down, Enter select, Esc back)",
                     vec!["(no matches)".into()], None, false)
                } else {
                    ("Thinking levels  (up/down, Enter select, Esc back)",
                     opts, None, false)
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
        }
    }

    fn title(&self) -> String {
        match self {
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

    /// Handle Tab on a selected row.  Mutates the editor in-place.
    fn handle_tab(
        &self,
        sel: usize,
        editor: &mut LineEditor,
        path_completions: &mut Vec<String>,
        completion_pos: &mut Option<(usize, usize)>,
    ) {
        match self {
            ActiveOverlay::CommandList { cmds } => {
                if let Some(c) = cmds.get(sel) {
                    editor.set(&Repl::complete_command(&c.item));
                }
            }
            ActiveOverlay::Model { entries, .. } => {
                if let Some(e) = entries.get(sel) {
                    let buf = editor.text();
                    editor.set(&Repl::replace_last_token(&buf, &e.display));
                }
            }
            ActiveOverlay::Thinking { levels } => {
                if let Some(l) = levels.get(sel) {
                    let buf = editor.text();
                    editor.set(&Repl::replace_last_token(&buf, l.as_str()));
                }
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
            ActiveOverlay::SessionAction { session_name, opts, .. } => {
                // Fill the chosen action into the buffer (stay in input).
                if let Some(label) = opts.get(sel) {
                    let action_str = match label.as_str() {
                        _ if label.starts_with("resume") => "resume",
                        "fork" => "fork",
                        "rename" => "rename ",
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
            ActiveOverlay::Tree { .. } => {
                // Tab on tree: no-op for now (future: copy entry text).
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
    ) -> OverlayAction {
        match self {
            ActiveOverlay::CommandList { cmds } => {
                if let Some(cmd) = cmds.get(sel) {
                    match cmd.item.kind {
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
                        slash::SlashKind::Clear => {
                            redraw(&LineEditor::new(), &[], sel, input_row);
                            output.display_plain("");
                            return OverlayAction::Action(InputAction::Submit("/clear".into()));
                        }
                        slash::SlashKind::Replay => editor.set("/replay "),
                    }
                }
                OverlayAction::Continue
            }
            ActiveOverlay::Model { entries, .. } => {
                if let Some(e) = entries.get(sel) {
                    let (provider, model) = e.qualified_parts();
                    let line = format!("/model {}", e.qualified());
                    Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                    OverlayAction::Action(InputAction::SetModel(provider, model))
                } else {
                    OverlayAction::Continue
                }
            }
            ActiveOverlay::Thinking { levels } => {
                if let Some(l) = levels.get(sel) {
                    let line = format!("/thinking {}", l.as_str());
                    Repl::emit_commit(output, editor, redraw, sel, input_row, &line, "", false);
                    OverlayAction::Action(InputAction::SetThinking(l.as_str().to_string()))
                } else {
                    OverlayAction::Continue
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
            ActiveOverlay::SessionAction { session_name, is_current, opts, .. } => {
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
                            // Keep the user in input to type the new name.
                            let line = format!("/session {} rename ", session_name);
                            editor.set(&line);
                            OverlayAction::Continue
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
            ActiveOverlay::Tree { targets, .. } => {
                if let Some(target_id) = targets.get(sel) {
                    editor.set("");
                    return OverlayAction::Action(InputAction::NavigateTree(target_id.clone()));
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
        history: &mut History,
        ctx: &ReplCtx,
        settings: &mut SettingsStore,
        dirty: &mut bool,
        initial_row: usize,
        output: &dyn Output,
    ) -> Option<InputAction> {
        let mut editor = LineEditor::new();
        let mut nav: Option<usize> = None;
        let mut draft = String::new();
        let mut sel: usize = 0;
        let mut help_view = false;
        let mut value_error: Option<String> = None;
        let mut path_completions: Vec<String> = Vec::new();
        let mut completion_pos: Option<(usize, usize)> = None;
        let mut last_disc: u8 = 0; // overlay discriminant for sel-reset tracking
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
            let overlay = ActiveOverlay::from_buffer(
                &editor.text(), ctx, settings, help_view, &path_completions,
            );
            // Reset sel when the overlay kind changes (discriminant comparison).
            let cur_disc = overlay.overlay_disc();
            if cur_disc != last_disc {
                sel = 0;
                last_disc = cur_disc;
            }
            let (title, raw_opts, active, wrap) = overlay.display_rows();
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
                ActiveOverlay::Confirm { header, .. } => {
                    Self::overlay_lines(effective_title, &opts, Some(header.as_slice()), sel, active, wrap)
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
                            KeyCode::Down => {
                                if sel + 1 < opts.len() { sel += 1; }
                            }
                            KeyCode::Up => {
                                if sel > 0 { sel -= 1; }
                            }
                            KeyCode::PageDown => {
                                let page = {
                                    let (_, h) = crossterm::terminal::size().unwrap_or((80, 24));
                                    (h as usize).saturating_sub(2).max(1)
                                };
                                sel = (sel + page).min(opts.len().saturating_sub(1));
                            }
                            KeyCode::PageUp => {
                                let page = {
                                    let (_, h) = crossterm::terminal::size().unwrap_or((80, 24));
                                    (h as usize).saturating_sub(2).max(1)
                                };
                                sel = sel.saturating_sub(page);
                            }
                            KeyCode::Tab => {
                                overlay.handle_tab(sel, &mut editor,
                                    &mut path_completions, &mut completion_pos);
                            }
                            KeyCode::Enter => {
                                if !key.modifiers.contains(KeyModifiers::SHIFT) {
                                    match overlay.handle_enter(
                                        sel, &mut editor, output, &redraw, &mut input_row,
                                        settings, dirty, &mut value_error, &mut help_view,
                                    ) {
                                        OverlayAction::Continue => {}
                                        OverlayAction::Action(action) => return Some(action),
                                    }
                                }
                            }
                            KeyCode::Esc => {
                                overlay.handle_esc(&mut editor,
                                    &mut path_completions, &mut completion_pos);
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
    /// start index into the full overlay. Extracted so it can be unit-tested
    /// without a TTY.
    pub fn overlay_window(overlay_len: usize, avail: usize, sel: usize) -> usize {
        if overlay_len <= avail || avail == 0 {
            0
        } else {
            sel.saturating_sub(avail / 2).min(overlay_len - avail)
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
        const BG: &str = "\x1b[48;5;235m"; // panel background (dark)
        const BG_SEL: &str = "\x1b[48;5;239m"; // selected row background
        const C_SEL: &str = "\x1b[1;36m"; // bold cyan: cursor selection
        const C_ACT: &str = "\x1b[32m"; // green: currently-active model
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
        out.push_str(&bg_line(title, BG));
        // Render header rows (non-selectable info) if present.
        if let Some(hdr) = header {
            for hline in hdr {
                for subline in hline.split('\n') {
                    out.push('\n');
                    out.push_str(&format!("{}{:<width$}{}", BG_HEADER, subline, C_RESET, width = tw));
                }
            }
        }
        for (i, line, first) in &rows {
            // The 2-char marker area prefixes every line (including wrapped
            // continuation lines) so all visual lines of one option stay
            // column-aligned; only the first line of the selected/active
            // option shows `> `/`* `.
            let marker = if *first && *i == sel {
                "> "
            } else if *first && active == Some(*i) {
                "* "
            } else {
                "  "
            };
            let body = format!("{}{}", marker, line);
            let (bg, fg) = if *i == sel {
                (BG_SEL, C_SEL)
            } else if active == Some(*i) {
                (BG, C_ACT)
            } else {
                (BG, "")
            };
            let content = if fg.is_empty() {
                body
            } else {
                format!("{}{}", fg, body)
            };
            out.push('\n');
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
    fn setting_value_candidates(key: SettingKey, ctx: &ReplCtx) -> Vec<ValueCandidate> {
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
            SettingKey::DefaultModel => {
                let (entries, _) = Self::model_overlay_entries(ctx, "");
                entries.iter().map(Self::model_candidate).collect()
            }
            SettingKey::ToolCallLimit => Vec::new(),
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
        }
    }

    /// Candidate values for `key`, fuzzy-filtered by `vq` (empty `vq` => all).
    fn filtered_value_candidates(
        key: SettingKey,
        ctx: &ReplCtx,
        vq: &str,
    ) -> Vec<ValueCandidate> {
        let cands = Self::setting_value_candidates(key, ctx);
        if vq.trim().is_empty() {
            cands
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
            desc: format!("ctx {} | {}", model_catalog::ctx_cell(e), model_catalog::cost_cell(e)),
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
    fn complete_command(c: &slash::SlashCommand) -> String {
        if c.name == "/session" {
            format!("/session path:. ")
        } else {
            format!("{} ", c.name)
        }
    }

    fn model_overlay_entries(
        ctx: &ReplCtx,
        filter: &str,
    ) -> (Vec<model_catalog::ModelEntry>, Option<usize>) {
        let entries = model_catalog::catalog_cached();
        let active_id = Self::active_entry_id(&entries, &ctx.provider, &ctx.model);
        let filtered: Vec<model_catalog::ModelEntry> = if filter.trim().is_empty() {
            // No filter: show the top 10 most recently used (available) models
            // first, then the rest of the catalog alphabetically.
            let mut seen = std::collections::HashSet::new();
            let mut recent_shown = Vec::new();
            for recent_id in &ctx.recent {
                if recent_shown.len() >= 10 {
                    break;
                }
                // Find the catalog entry that best matches this recent id.
                // Prefer a Direct entry over an OpenRouter entry when both exist.
                if let Some(e) = entries.iter().find(|e| e.id == *recent_id && model_catalog::provider_available(&e.provider)) {
                    if seen.insert(e.id.clone()) {
                        recent_shown.push(e.clone());
                    }
                }
            }
            // Sort remaining entries alphabetically.
            let mut rest: Vec<model_catalog::ModelEntry> = entries
                .into_iter()
                .filter(|e| !seen.contains(&e.id))
                .collect();
            rest.sort_by(|a, b| a.display.cmp(&b.display));
            recent_shown.extend(rest);
            recent_shown
        } else {
            // With filter: fuzzy-match across all catalog entries (current behaviour).
            let mut sorted: Vec<model_catalog::ModelEntry> = entries;
            sorted.sort_by(|a, b| {
                Self::model_rank(&a.id, &active_id, &ctx.recent)
                    .cmp(&Self::model_rank(&b.id, &active_id, &ctx.recent))
                    .then_with(|| a.display.cmp(&b.display))
            });
            crate::fuzzy::fuzzy_filter(
                &sorted,
                filter,
                // Searchable text leads with the `provider:model` display (so a
                // `provider:model` query matches directly) and also includes the
                // bare provider, the slash-id form, and the qualified form.
                |e| format!("{} {} {} {}", e.provider, e.display, e.id, e.qualified()),
            )
            .into_iter()
            .map(|r| r.item.clone())
            .collect()
        };
        let active = filtered.iter().position(|e| e.id == active_id);
        (filtered, active)
    }

    /// Build the `/thinking` overlay rows for `filter`.
    fn thinking_overlay_entries(filter: &str) -> Vec<thinking::ThinkingLevel> {
        let all = [
            thinking::ThinkingLevel::Off,
            thinking::ThinkingLevel::Low,
            thinking::ThinkingLevel::Med,
            thinking::ThinkingLevel::High,
        ];
        if filter.trim().is_empty() {
            all.to_vec()
        } else {
            crate::fuzzy::fuzzy_filter_bonus(&all, filter, |l| l.as_str().to_string())
                .into_iter()
                .map(|r| *r.item)
                .collect()
        }
    }

    /// Build the `/session` overlay rows.
    ///
    /// Shows sessions filtered by `filter` (understands `path:/dir` prefix).
    /// Current session is marked with ◉ and sorted first.
    /// Each row shows: marker · session name (with first-prompt preview) · dir · turns + relative age.
    fn session_overlay_entries(
        ctx: &ReplCtx,
        filter: &str,
        opts: &mut Vec<String>,
        active: &mut Option<usize>,
    ) {
        let (dir_filter, fuzzy_filter) = Self::parse_session_path_filter(filter);

        let avail = term_width().saturating_sub(2);
        let mut table = Table::new(&["", "Session", "Dir", ""])
            .with_aligns(&[Align::Left, Align::Left, Align::Left, Align::Right])
            .with_min_widths(&[2, 20, 10, 10])
            .with_priorities(&[0, 2, 2, 1])
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
                first_prompt_preview: m.first_prompt_preview,
            })
            .collect();
        metas.sort_by(|a, b| {
            let a_cur = a.id == ctx.current_session_id;
            let b_cur = b.id == ctx.current_session_id;
            if a_cur && !b_cur { std::cmp::Ordering::Less }
            else if !a_cur && b_cur { std::cmp::Ordering::Greater }
            else { b.timestamp.cmp(&a.timestamp) }
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
            let marker = if meta.id == ctx.current_session_id { "◉" } else { " " };
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
            let rel = Self::relative_time(&meta.timestamp);
            let info = format!("{}  {}", meta.message_count, rel);
            table.add_row(&[marker, &display_name, &dir_display, &info]);
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
        let mut table = crate::table::Table::new(&["Session", "Turns", "Age", "Parent/Dir"])
            .with_aligns(&[
                crate::table::Align::Left,
                crate::table::Align::Right,
                crate::table::Align::Right,
                crate::table::Align::Left,
            ])
            .with_min_widths(&[20, 5, 11, 12])
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
            &meta.message_count.to_string(),
            &ts,
            &parent_display,
        ]);

        for row in table.render(avail) {
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
                first_prompt_preview: m.first_prompt_preview,
            })
            .collect();

        // Sort: current session first, then by timestamp descending.
        metas.sort_by(|a, b| {
            let a_cur = a.id == ctx.current_session_id;
            let b_cur = b.id == ctx.current_session_id;
            if a_cur && !b_cur { std::cmp::Ordering::Less }
            else if !a_cur && b_cur { std::cmp::Ordering::Greater }
            else { b.timestamp.cmp(&a.timestamp) }
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

    // ── /tree overlay rendering ──────────────────────────────────────────

    /// Build `/tree` overlay rows from a pre-built session tree.
    /// Uses the same `Table` infrastructure as `/session` for consistent rendering.
    fn tree_overlay_entries(
        tree: &[SessionTreeNode],
        leaf_id: Option<&str>,
        filter: &str,
        rows: &mut Vec<String>,
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

        // Precompute flat entries: (indent, label, entry_id, is_on_active_path, is_current)
        let mut flat: Vec<(usize, String, String, bool, bool)> = Vec::new();
        Self::flatten_tree_nodes(tree, 0, leaf_id, &active_path, &mut flat);

        // Apply text filter (fuzzy match on display labels).
        if !filter.is_empty() {
            let f = filter.to_lowercase();
            flat.retain(|(_, label, _, _, _)| label.to_lowercase().contains(&f));
        }

        if flat.is_empty() {
            rows.push("(no matches)".into());
            return;
        }

        // Render via Table for column alignment like /session.
        let avail = term_width().saturating_sub(2);
        let mut table = Table::new(&["", "Entry"])
            .with_aligns(&[Align::Left, Align::Left])
            .with_min_widths(&[2, 20])
            .with_priorities(&[0, 1])
            .set_gap(1);

        for (indent, label, entry_id, is_active, is_curr) in &flat {
            let marker = if *is_curr { "◉" } else { " " };
            let display = if *is_active {
                format!("{:indent$}{}", "", label, indent = indent)
            } else {
                format!("\x1b[2m{:indent$}{}\x1b[0m", "", label, indent = indent)
            };
            table.add_row(&[marker, &display]);
            targets.push(entry_id.clone());
        }

        *rows = table.render(avail);

        // Highlight current leaf.
        if let Some(lid) = leaf_id {
            *active = flat.iter().position(|(_, _, id, _, _)| id == lid);
        }
    }

    /// Recursively flatten tree nodes into entries for table rendering.
    /// `indent` is the number of leading spaces (0 = root, 2 = branch child, …).
    /// Indent only increases at branching points (multiple children).
    fn flatten_tree_nodes(
        nodes: &[SessionTreeNode],
        indent: usize,
        leaf_id: Option<&str>,
        active_path: &std::collections::HashSet<String>,
        out: &mut Vec<(usize, String, String, bool, bool)>,
    ) {
        for node in nodes {
            let label = Self::entry_display_label(&node.entry, node.label.as_deref());

            if label.is_empty() {
                // Meta entry: skip visual row but still descend.
                let child_indent = if node.children.len() > 1 {
                    indent + 2
                } else {
                    indent
                };
                Self::flatten_tree_nodes(
                    &node.children, child_indent, leaf_id, active_path, out,
                );
                continue;
            }

            let is_active = active_path.contains(&node.entry.id);
            let is_current = leaf_id == Some(node.entry.id.as_str());
            out.push((indent, label.clone(), node.entry.id.clone(), is_active, is_current));

            let child_indent = if node.children.len() > 1 {
                indent + 2
            } else {
                indent
            };
            Self::flatten_tree_nodes(
                &node.children, child_indent, leaf_id, active_path, out,
            );
        }
    }

    /// Build a display label for one tree entry.
    /// Returns an empty string for meta entries that should be skipped.
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
                        // Parse content blocks: skip reasoning, show tool calls
                        // or text. Matches the live render's get_preview format.
                        let content_arr = message.get("content").and_then(|c| c.as_array());
                        let mut text = String::new();
                        let mut tool_call_label: Option<String> = None;
                        if let Some(arr) = content_arr {
                            for item in arr {
                                match item.get("type").and_then(|t| t.as_str()) {
                                    Some("text") => {
                                        if text.is_empty() {
                                            text = item.get("text")
                                                .and_then(|t| t.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                        }
                                    }
                                    Some("toolCall") => {
                                        if tool_call_label.is_none() {
                                            let name = item.get("name")
                                                .and_then(|n| n.as_str())
                                                .unwrap_or("tool");
                                            let args = item.get("arguments");
                                            tool_call_label = Some(
                                                Self::tool_preview_label(name, args)
                                            );
                                        }
                                    }
                                    // Skip thinking/reasoning blocks
                                    _ => {}
                                }
                            }
                        }
                        // Prefer tool call label; fall back to text.
                        if let Some(label) = tool_call_label {
                            label
                        } else if !text.is_empty() {
                            Self::sanitise_single_line(&text)
                        } else {
                            "(assistant)".into()
                        }
                    }
                    Some("toolResult") => "(tool result)".into(),
                    _ => "(unknown)".into(),
                }
            }
            SessionEntryContent::ThinkingLevelChange { thinking_level } => {
                format!("thinking: {}", thinking_level)
            }
            SessionEntryContent::ModelChange { provider, model_id } => {
                format!("model: {}/{}", provider, model_id)
            }
            SessionEntryContent::ActiveToolsChange { .. } => String::new(),
            SessionEntryContent::BranchSummary { summary, .. } => {
                let s = Self::sanitise_single_line(summary);
                if s.is_empty() { "[S] (branch summary)".into() } else { format!("[S] {}", s) }
            }
            SessionEntryContent::Compaction { summary, .. } => {
                let s = Self::sanitise_single_line(summary);
                if s.is_empty() { "[C] (compacted)".into() } else { format!("[C] {}", s) }
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

    /// Format a tool call preview for the tree overlay, matching the live
    /// render's `get_preview` style from `output.rs`.
    fn tool_preview_label(name: &str, args: Option<&serde_json::Value>) -> String {
        use serde_json::Value;
        let get_str = |key: &str| {
            args.and_then(|a| a.as_object())
                .and_then(|o| o.get(key))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let get_u64 = |key: &str| {
            args.and_then(|a| a.as_object())
                .and_then(|o| o.get(key))
                .and_then(|v| v.as_u64())
        };
        match name {
            "read_file" => {
                let path = get_str("path").unwrap_or_default();
                let offset = get_u64("offset").unwrap_or(0);
                let limit = get_u64("limit").unwrap_or(0);
                let mut s = format!("read {}", path);
                if limit > 0 {
                    s.push_str(&format!(" [{}..{}]", offset + 1, offset + limit));
                } else if offset > 0 {
                    s.push_str(&format!(" [{}..]", offset + 1));
                }
                s
            }
            "bash" => {
                let cmd = get_str("cmd").unwrap_or_default();
                format!("$ {}", cmd.trim())
            }
            s if s.starts_with("grep") => {
                let pat = get_str("pat").or_else(|| get_str("pattern")).unwrap_or_default();
                let path = get_str("path").unwrap_or_default();
                format!("grep {} {}", pat, path)
            }
            s if s.starts_with("list") => {
                let path = get_str("path").unwrap_or_default();
                format!("ls {}", path)
            }
            "edit_file" => {
                let path = get_str("path").unwrap_or_default();
                format!("edit {}", path)
            }
            _ => {
                let first = args
                    .and_then(|a| a.as_object())
                    .and_then(|o| o.values().next())
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        _ => v.to_string(),
                    })
                    .unwrap_or_default();
                if first.len() > 50 {
                    format!("{} {}...", name, &first[..47])
                } else if first.is_empty() {
                    name.to_string()
                } else {
                    format!("{} {}", name, first.replace('\n', " "))
                }
            }
        }
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
            KeyCode::Esc => Some(InputAction::Submit(String::new())),
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
                    if let Some(s) = history.prev(&editor.text(), nav, draft) {
                        editor.set(&s);
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
fn edit_in_editor(current: &str) -> io::Result<String> {
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
