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
}

/// A candidate value shown in a setting's value-pick overlay.
#[derive(Clone)]
struct ValueCandidate {
    display: String,
    value: String,
    desc: String,
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
        let mut sel: usize = 0; // selection within the active overlay
        let mut last_kind: u8 = 0; // 0=None,1=Cmd,2=Model,3=Thinking,4=Help
        let mut help_view = false;
        // Command word / filter text captured when building the overlay, so the
        // Enter handler can re-resolve the highlighted item without re-parsing.
        let mut cur_word = String::new();
        let mut cur_filter = String::new();
        let mut value_error: Option<String> = None;
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
            s.push_str(&cursor_positioning(&layout));
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
            let buffer = editor.text();
            let mut kind: u8 = 0;
            let mut opts: Vec<String> = Vec::new();
            let mut active: Option<usize> = None;
            let mut title = String::new();
            cur_word.clear();
            cur_filter.clear();
            if help_view {
                kind = 4;
            } else if buffer.starts_with('/') {
                let after = &buffer[1..];
                // A bare token that is exactly a known command word opens that
                // menu (no trailing space required); a non-command bare token
                // (e.g. `/hel`) falls through to the command list.
                let (word, rest) = if let Some((w, r)) = after.split_once(' ') {
                    (w, r)
                } else if matches!(
                    after,
                    "" | "model" | "models" | "m" | "thinking" | "think" | "t" | "set"
                        | "s" | "persona" | "personas" | "p" | "stat" | "stats"
                ) {
                    (after, "")
                } else {
                    ("", after)
                };
                match word {
                        "model" | "models" | "m" => {
                            kind = 2;
                            title = "Models  (up/down or PgUp/PgDn, Enter select, Esc back)".into();
                            cur_word = "model".into();
                            cur_filter = rest.trim().to_string();
                            let (entries, act) = Self::model_overlay_entries(ctx, &cur_filter);
                            active = act;
                            if entries.is_empty() {
                                opts.push("(loading models... or no matches)".into());
                            } else {
                                // Aligned table: Model | Ctx | Cost | Reason. `avail` mirrors
                                // `overlay_lines`' `opt_w` (terminal − marker width) so each
                                // row fits on one line and the selection marker still fits.
                                let avail = term_width().saturating_sub(2);
                                let mut table = Table::new(&["Model", "Ctx", "Cost", "?"])
                                    .with_aligns(&[Align::Left, Align::Right, Align::Right, Align::Left])
                                    .with_min_widths(&[12, 5, 14, 3])
                                    .with_priorities(&[3, 1, 2, 1])
                                    .set_gap(2);
                                for e in &entries {
                                    table.add_row(&[
                                        e.qualified(),
                                        model_catalog::ctx_cell(e),
                                        model_catalog::cost_cell(e),
                                        model_catalog::reason_cell(e),
                                    ]);
                                }
                                let rows = table.render_multiline(avail);
                                // Dim models whose provider has no API key configured.
                                // Dim each visual line (a wrapped row carries `\n`).
                                for (i, row) in rows.into_iter().enumerate() {
                                    let styled = if model_catalog::provider_available(&entries[i].provider) {
                                        row
                                    } else {
                                        row.lines()
                                            .map(|l| format!("\x1b[2m{}\x1b[0m", l))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    };
                                    opts.push(styled);
                                }
                            }
                        }
                        "thinking" | "think" | "t" => {
                            kind = 3;
                            title = "Thinking levels  (up/down, Enter select, Esc back)".into();
                            cur_word = "thinking".into();
                            cur_filter = rest.trim().to_string();
                            let levels = Self::thinking_overlay_entries(&cur_filter);
                            opts = levels.iter().map(|l| l.as_str().to_string()).collect();
                            if opts.is_empty() {
                                opts.push("(no matches)".into());
                            }
                        }
                        "persona" | "personas" | "p" => {
                            kind = 6;
                            title = "Personas  (up/down, Enter select, Esc back)".into();
                            cur_word = "persona".into();
                            cur_filter = rest.trim().to_string();
                            // Reuse the value candidates already defined for the
                            // `default_persona` setting (built-in names + "none").
                            let cands = Self::setting_value_candidates(
                                crate::settings::SettingKey::DefaultPersona,
                                ctx,
                            );
                            let filtered: Vec<ValueCandidate> = if cur_filter.is_empty() {
                                cands
                            } else {
                                crate::fuzzy::fuzzy_filter(&cands, &cur_filter, |c| {
                                    c.display.clone()
                                })
                                .into_iter()
                                .map(|r| r.item.clone())
                                .collect()
                            };
                            if filtered.is_empty() {
                                opts.push("(no matching persona)".into());
                                active = None;
                            } else {
                                let avail = term_width().saturating_sub(2);
                                let mut table = Table::new(&["Persona", "Description"])
                                    .with_aligns(&[Align::Left, Align::Left])
                                    .with_min_widths(&[20, 30])
                                    .with_priorities(&[2, 1])
                                    .set_gap(2);
                                for c in &filtered {
                                    table.add_row(&[c.display.clone(), c.desc.clone()]);
                                }
                                for row in table.render_multiline(avail) {
                                    opts.push(row);
                                }
                                // Mark the currently active persona (skip the
                                // "none" entry, whose value is empty).
                                active = filtered
                                    .iter()
                                    .position(|c| {
                                        !c.value.is_empty()
                                            && ctx.persona.as_deref() == Some(c.value.as_str())
                                    });
                            }
                        }
                        "set" | "s" => {
                            kind = 5;
                            title = "Settings  (type to filter, up/down, Tab/Enter select, Esc back)".into();
                            cur_word = "set".into();
                            cur_filter = rest.to_string();
                            let entries = settings.overlay_rows(&cur_filter);
                            if entries.is_empty() {
                                opts.push("(no matching setting)".into());
                                active = None;
                            } else {
                                let avail = term_width().saturating_sub(2);
                                let mut table = Table::new(&["Setting", "Value", "Description"])
                                    .with_aligns(&[Align::Left, Align::Left, Align::Left])
                                    .with_min_widths(&[22, 18, 26])
                                    .with_priorities(&[2, 2, 1])
                                    .set_gap(2);
                                for (meta, value) in &entries {
                                    table.add_row(&[meta.name, value.as_str(), meta.description]);
                                }
                                for row in table.render_multiline(avail) {
                                    opts.push(row);
                                }
                                active = None;
                            }
                        }
                        "stat" | "stats" => {
                            kind = 7;
                            title = "Session stats  (Esc clears input to close)".into();
                            cur_word = "stat".into();
                            // Render straight from the agent's usage snapshot; no
                            // filtering, no selection. Intermediate backspacing of
                            // the buffer re-derives the overlay live (and falls
                            // back to the command list once the word is gone).
                            opts = crate::agent::session_stats_lines(
                                &ctx.provider,
                                &ctx.model,
                                ctx.session_usage,
                            );
                            active = None;
                        }
                        _ => {
                            kind = 1;
                            title = "Commands  (type to filter, up/down, Tab/Enter select)".into();
                            cur_word = rest.to_string();
                            let ranked = slash::filter_commands(rest);
                            for r in &ranked {
                                opts.push(format!("{:<10} {}", r.item.name, r.item.help));
                            }
                            if opts.is_empty() {
                                opts.push("(no matching command)".into());
                            }
                        }
                    }
            }
            if kind != last_kind {
                sel = 0;
                last_kind = kind;
            }
            let max_sel = opts.len().saturating_sub(1);
            if sel > max_sel {
                sel = max_sel;
            }
            let mut overlay_text = match kind {
                0 => String::new(),
                4 => Self::render_help_overlay(),
                7 => Self::overlay_lines(&title, &opts, usize::MAX, None, true),
                _ => Self::overlay_lines(&title, &opts, sel, active, kind == 1),
            };
            // Value-mode settings error: red header line above the regular overlay.
            if kind == 5 && value_error.is_some() {
                let err = format!(
                    "\x1b[48;5;235m\x1b[31merror: {}\x1b[0m",
                    value_error.as_ref().unwrap()
                );
                overlay_text = format!("{}\n{}", err, overlay_text);
            }
            let overlay_vec: Vec<&str> = overlay_text.lines().collect();
            // `sel` indexes logical options; a wrapped option spans several
            // visual lines, so center the window on the selected option's
            // first visual line (the title accounts for the +1 offset).
            let focus = {
                let sc = sel.min(opts.len());
                let base = if kind == 5 && value_error.is_some() {
                    2 // red header line + title
                } else {
                    1 // title
                };
                base + opts[..sc].iter().map(|o| o.matches('\n').count() + 1).sum::<usize>()
            };
            redraw(&editor, &overlay_vec, focus, &mut input_row);
            // The redraw used `prev_term_h` (the height *before* this resize) as
            // its clear boundary; advance it now so the next resize is detected
            // relative to the current size.
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
                    if kind == 4 {
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
                    if kind != 0 {
                        match key.code {
                            KeyCode::Down => {
                                if sel + 1 < opts.len() {
                                    sel += 1;
                                }
                            }
                            KeyCode::Up => {
                                if sel > 0 {
                                    sel -= 1;
                                }
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
                                let buf = editor.text();
                                match kind {
                                    1 => {
                                        let ranked = slash::filter_commands(&cur_word);
                                        if let Some(c) = ranked.get(sel) {
                                            editor.set(&Self::complete_command(&c.item));
                                        }
                                    }
                                    2 => {
                                        let (entries, _) = Self::model_overlay_entries(ctx, &cur_filter);
                                        if let Some(e) = entries.get(sel) {
                                            editor.set(&Self::replace_last_token(&buf, &e.display));
                                        }
                                    }
                                    3 => {
                                        let levels = Self::thinking_overlay_entries(&cur_filter);
                                        if let Some(l) = levels.get(sel) {
                                            editor.set(&Self::replace_last_token(&buf, l.as_str()));
                                        }
                                    }
                                    5 => {
                                        let (skey, vq, is_value) = Self::parse_settings(&cur_filter);
                                        if is_value {
                                            if let Some(k) = skey {
                                                let filtered =
                                                    Self::filtered_value_candidates(k, ctx, &vq);
                                                if let Some(c) = filtered.get(sel) {
                                                    editor.set(&Self::replace_last_token(&buf, &c.value));
                                                }
                                            }
                                        } else {
                                            let entries = settings.overlay_rows(&cur_filter);
                                            if let Some((meta, _)) = entries.get(sel) {
                                                editor.set(&format!("/set {} ", meta.name));
                                            }
                                        }
                                    }
                                    6 => {
                                        let cands = Self::setting_value_candidates(
                                            crate::settings::SettingKey::DefaultPersona,
                                            ctx,
                                        );
                                        let filtered = if cur_filter.is_empty() {
                                            cands
                                        } else {
                                            crate::fuzzy::fuzzy_filter(&cands, &cur_filter, |c| {
                                                c.display.clone()
                                            })
                                            .into_iter()
                                            .map(|r| r.item.clone())
                                            .collect()
                                        };
                                        // `value` is empty for the "none" entry; complete with the
                                        // display name in that case so the buffer reads `/persona none`.
                                        if let Some(c) = filtered.get(sel) {
                                            let tok = if c.value.is_empty() {
                                                c.display.clone()
                                            } else {
                                                c.value.clone()
                                            };
                                            editor.set(&Self::replace_last_token(&buf, &tok));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            KeyCode::Enter => {
                                if !key.modifiers.contains(KeyModifiers::SHIFT) {
                                    match kind {
                                        1 => {
                                            let ranked = slash::filter_commands(&cur_word);
                                            if let Some(cmd) = ranked.get(sel) {
                                                match cmd.item.kind {
                                                    slash::SlashKind::Help => {
                                                        help_view = true;
                                                        editor.set("");
                                                    }
                                                    slash::SlashKind::Quit => {
                                                        // Clear the input line and emit a trailing newline so
                                                        // the shell prompt after quit begins on a fresh line
                                                        // (otherwise it is left indented under the input).
                                                        redraw(&editor, &[], sel, &mut input_row);
                                                        output.display_plain("");
                                                        return Some(InputAction::Quit);
                                                    }
                                                    slash::SlashKind::Model => {
                                                        editor.set("/model ");
                                                    }
                                                    slash::SlashKind::Thinking => {
                                                        editor.set("/thinking ");
                                                    }
                                                    slash::SlashKind::Settings => {
                                                        editor.set("/set ");
                                                    }
                                                    slash::SlashKind::Persona => {
                                                        editor.set("/persona ");
                                                    }
                                                    slash::SlashKind::Stat => {
                                                        // Open the `/stat` overlay (kind 7) on the
                                                        // next loop iteration, mirroring `/set`. No
                                                        // commit/submission — Esc or clearing the
                                                        // input closes it.
                                                        editor.set("/stat ");
                                                    }
                                                }
                                            }
                                        }
                                        2 => {
                                            let (entries, _) =
                                                Self::model_overlay_entries(ctx, &cur_filter);
                                            if let Some(e) = entries.get(sel) {
                                                let (provider, model) = e.qualified_parts();
                                                // Echo the *selected* model as a committed-invocation
                                                // record (not the raw editor text), so a bare `/model`
                                                // + Enter still records the choice. Next prompt is an
                                                // empty (leaf) command.
                                                let line = format!("/model {}", e.qualified());
                                                Self::emit_commit(output, &mut editor, &redraw, sel, &mut input_row, &line, "", false);
                                                return Some(InputAction::SetModel(provider, model));
                                            }
                                        }
                                        3 => {
                                            let levels = Self::thinking_overlay_entries(&cur_filter);
                                            if let Some(l) = levels.get(sel) {
                                                // Echo the *selected* level as a committed-invocation
                                                // record; next prompt is an empty (leaf) command.
                                                let line = format!("/thinking {}", l.as_str());
                                                Self::emit_commit(output, &mut editor, &redraw, sel, &mut input_row, &line, "", false);
                                                return Some(InputAction::SetThinking(
                                                    l.as_str().to_string(),
                                                ));
                                            }
                                        }
                                        6 => {
                                            let cands = Self::setting_value_candidates(
                                                crate::settings::SettingKey::DefaultPersona,
                                                ctx,
                                            );
                                            let filtered = if cur_filter.is_empty() {
                                                cands
                                            } else {
                                                crate::fuzzy::fuzzy_filter(&cands, &cur_filter, |c| {
                                                    c.display.clone()
                                                })
                                                .into_iter()
                                                .map(|r| r.item.clone())
                                                .collect()
                                            };
                                            // `value` is empty for the "none" entry; emit the display
                                            // name so the transcript reads `/persona none` (the agent
                                            // treats empty as no persona).
                                            if let Some(c) = filtered.get(sel) {
                                                let name = if c.value.is_empty() {
                                                    c.display.clone()
                                                } else {
                                                    c.value.clone()
                                                };
                                                let line = format!("/persona {}", name);
                                                Self::emit_commit(output, &mut editor, &redraw, sel, &mut input_row, &line, "", false);
                                                return Some(InputAction::SetPersona(name));
                                            }
                                        }
                                        5 => {
                                            let (skey, vq, is_value) = Self::parse_settings(&cur_filter);
                                            if is_value {
                                                if let Some(k) = skey {
                                                    // Enter with an empty value saves the highlighted
                                                    // candidate (mirrors Tab); a typed value is saved as-is.
                                                    let vq_eff = if vq.trim().is_empty() {
                                                        Self::filtered_value_candidates(k, ctx, &vq)
                                                            .get(sel)
                                                            .map(|c| c.value.clone())
                                                            .unwrap_or_default()
                                                    } else {
                                                        vq.clone()
                                                    };
                                                    match settings.set_value(k, &vq_eff) {
                                                        Ok(()) => {
                                                            value_error = None;
                                                            // A successful save means the running session should
                                                            // pick up the change; `run_interactive` performs the
                                                            // (async) agent rebuild before the next prompt.
                                                            *dirty = true;
                                                            // When Enter was pressed on an empty value (the
                                                            // highlighted candidate), reflect the saved value in
                                                            // the emitted line so the record is accurate.
                                                            if vq.trim().is_empty() {
                                                                if let Some(name) =
                                                                    SETTINGS.iter().find(|m| m.key == k).map(|m| m.name)
                                                                {
                                                                    editor.set(&format!(
                                                                        "/set {} {}",
                                                                        name, vq_eff
                                                                    ));
                                                                }
                                                            }
                                                            // Echo the committed `/set <key> <value>` line,
                                                            // then revert one level up to the `/set` list
                                                            // (handled by `editor.set` below). `emit_commit` draws
                                                            // the separator, so the emitted line is framed even
                                                            // though the list's own divider sits below its input.
                                                            let line = editor.text();
                                                            Self::emit_commit(output, &mut editor, &redraw, sel, &mut input_row, &line, "/set", true);
                                                        }
                                                        Err(msg) => {
                                                            value_error = Some(msg);
                                                        }
                                                    }
                                                }
                                            } else {
                                                let entries = settings.overlay_rows(&cur_filter);
                                                if let Some((meta, _)) = entries.get(sel) {
                                                    editor.set(&format!("/set {} ", meta.name));
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            KeyCode::Esc => match kind {
                                1 => {
                                    editor.set("");
                                }
                                2 => {
                                    editor.set("");
                                }
                                3 => {
                                    editor.set("");
                                }
                                5 => {
                                    let buf = editor.text();
                                    if let Some(idx) = buf.rfind(' ') {
                                        editor.set(&buf[..idx]);
                                    } else {
                                        editor.set("");
                                    }
                                }
                                6 => {
                                    editor.set("");
                                }
                                7 => {
                                    editor.set("");
                                }
                                _ => {}
                            },
                            _ => {
                                if let Some(a) =
                                    Self::edit_key(key, &mut editor, history, &mut nav, &mut draft, prompt)
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
        sel: usize,
        active: Option<usize>,
        wrap_opts: bool,
    ) -> String {
        const BG: &str = "\x1b[48;5;235m"; // panel background (dark)
        const BG_SEL: &str = "\x1b[48;5;239m"; // selected row background
        const C_SEL: &str = "\x1b[1;36m"; // bold cyan: cursor selection
        const C_ACT: &str = "\x1b[32m"; // green: currently-active model
        const C_RESET: &str = "\x1b[0m";
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
        Self::overlay_lines("Help  (any key closes)", &lines, usize::MAX, None, true)
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
            if id == format!("{}/{}", p, m) {
                return e.id.clone();
            }
            if id == m {
                return e.id.clone();
            }
            // OpenRouter passthrough (`tencent/hy3:free`) and native
            // (`openrouter/hy3:free`) both end in the short model name.
            if p == "openrouter" && id.ends_with(&format!("/{}", m)) {
                return e.id.clone();
            }
        }
        format!("{}/{}", p, m)
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
            crate::fuzzy::fuzzy_filter(&cands, vq, |c| c.display.clone())
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

    /// Build the line that replaces the current input when Tab-completing a
    /// slash command. Command `name`s already include the leading `/`
    /// (e.g. `"/help"`), so we must NOT prepend another — that would yield
    /// `//help`. A trailing space opens the argument slot (matching the
    /// Enter handler's `editor.set("/model ")`, etc.).
    fn complete_command(c: &slash::SlashCommand) -> String {
        format!("{} ", c.name)
    }

    fn model_overlay_entries(
        ctx: &ReplCtx,
        filter: &str,
    ) -> (Vec<model_catalog::ModelEntry>, Option<usize>) {
        let mut entries = model_catalog::catalog_cached();
        let active_id = Self::active_entry_id(&entries, &ctx.provider, &ctx.model);
        entries.sort_by(|a, b| {
            Self::model_rank(&a.id, &active_id, &ctx.recent)
                .cmp(&Self::model_rank(&b.id, &active_id, &ctx.recent))
                .then_with(|| a.display.cmp(&b.display))
        });
        let filtered: Vec<model_catalog::ModelEntry> = if filter.trim().is_empty() {
            entries
        } else {
            crate::fuzzy::fuzzy_filter(
                &entries,
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
            crate::fuzzy::fuzzy_filter(&all, filter, |l| l.as_str().to_string())
                .into_iter()
                .map(|r| *r.item)
                .collect()
        }
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
fn cursor_positioning(layout: &Layout) -> String {
    let rc = layout.row_count.max(1);
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
        s.push_str(&format!("\x1b[{}C", layout.cursor_col));
    } else {
        let d = last_row_len.saturating_sub(layout.cursor_col);
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
    s.push_str(&cursor_positioning(layout));
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
            id: "anthropic/claude-3-5-sonnet".into(),
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
            id: "deepseek/deepseek-v4-flash".into(),
            display: "deepseek:deepseek-v4-flash".into(),
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
        let out = Repl::overlay_lines("Select a model:", &opts, 0, None, true);
        assert_eq!(out.matches("Select a model:").count(), 1);
        assert!(out.contains("> a: alpha"));
        assert!(out.contains("  b: beta"));
    }

    #[test]
    fn render_select_wraps_long_option_with_aligned_continuation() {
        let long = "deepseek:deepseek-v4-flash ctx 1048576 $0.000000435/0.00000087 per M (est) and some extra trailing text to force a wrap";
        let opts = vec![long.to_string()];
        let out = Repl::overlay_lines("Select a model:", &opts, 0, None, true);
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

    impl LineEditor {
        fn from_text(s: &str) -> Self {
            let mut e = LineEditor::new();
            e.insert_str(s);
            e
        }
    }
}
