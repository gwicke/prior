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
use crate::output::{term_width, wrap};
use crate::model_catalog;
use crate::slash;
use crate::thinking;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, Write};
use std::path::PathBuf;

/// Owns terminal mode transitions. [`TermGuard::start`] enters raw mode and
/// negotiates capabilities; [`TermGuard::stop`] (idempotent) restores cooked
/// mode. Safe to call from `Drop`, a panic hook, or a signal handler.
pub struct TermGuard {
    active: bool,
    saved_paste: bool,
}

impl TermGuard {
    pub fn new() -> Self {
        Self {
            active: false,
            saved_paste: false,
        }
    }

    /// Enter raw mode and negotiate capabilities. Idempotent.
    pub fn start(&mut self) -> io::Result<()> {
        if self.active {
            return Ok(());
        }
        self.saved_paste = false;
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
        self.active = true;
        Ok(())
    }

    /// Restore cooked mode. Idempotent; safe to call multiple times / from `Drop`.
    pub fn stop(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        // Pop one level of Kitty keyboard enhancement (matches the single push).
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
        if self.saved_paste {
            Self::csi("?2004h")?;
        } else {
            Self::csi("?2004l")?;
        }
        disable_raw_mode()?;
        self.active = false;
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
    guard: TermGuard,
}

/// Result of one `read_input` interaction.
pub enum InputAction {
    /// Submit a normal prompt (text may be empty when the line was cancelled).
    Submit(String),
    /// Switch model: (provider, model).
    SetModel(String, String),
    /// Set thinking level (string form: off | low | med | high).
    SetThinking(String),
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
}

impl Repl {
    pub fn new() -> Self {
        Self {
            guard: TermGuard::new(),
        }
    }

    /// Enter raw mode (idempotent). Best-effort; `Drop` still restores.
    pub fn start(&mut self) -> io::Result<()> {
        self.guard.start()
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
        initial_row: usize,
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
        // The input line starts at `initial_row` — the natural cursor row tracked
        // by the console `Output` — and is *not* forced to the bottom of the
        // screen, so it never leaves a gap in the session scrollback. `input_row`
        // is the *current* visible row of the input and is updated every redraw to
        // follow the terminal's native scroll: when a tall pop-up pushes the input
        // up, `input_row` decreases and stays there as the pop-up later shrinks
        // (the terminal never scrolls back down on its own).
        let mut input_row = initial_row.max(1);

        // Render the input line at `input_row` and the live overlay directly
        // *below* it. We clear from `input_row` to the end of the screen
        // (`\x1b[0J`) and rewrite input + overlay; content *above* the input
        // (greeter / status / divider / session) is untouched. When the overlay
        // is taller than the space left, the terminal scrolls the overflow into
        // its native scrollback (session preserved); `input_row` is adjusted by
        // the scroll delta so the next redraw clears/writes at the right place
        // (no duplication). See plan §3.7.
        let redraw = |editor: &LineEditor, overlay: &[&str], sel: usize,
                      current_row: &mut usize| {
            let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
            let w = w as usize;
            let term_h = h.max(1) as usize;
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
            // Position at the input block's top row and clear to end of screen.
            s.push_str(&format!("\x1b[{};1H", row));
            s.push_str("\x1b[0J");
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
                if let Some((word, rest)) = after.split_once(' ') {
                    match word {
                        "model" | "models" | "m" => {
                            kind = 2;
                            title = "Models  (up/down or PgUp/PgDn, Enter select, Esc back)".into();
                            cur_word = "model".into();
                            cur_filter = rest.trim().to_string();
                            let (entries, act) = Self::model_overlay_entries(ctx, &cur_filter);
                            active = act;
                            // Dim models whose provider has no API key configured.
                            opts = entries
                                .iter()
                                .map(|e| {
                                    let s = model_catalog::format_option(e);
                                    if model_catalog::provider_available(&e.provider) {
                                        s
                                    } else {
                                        format!("\x1b[2m{}\x1b[0m", s)
                                    }
                                })
                                .collect();
                            if opts.is_empty() {
                                opts.push("(loading models... or no matches)".into());
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
                        _ => {
                            kind = 0;
                        }
                    }
                } else {
                    kind = 1;
                    title = "Commands  (type to filter, up/down, Tab/Enter select)".into();
                    cur_word = after.to_string();
                    let ranked = slash::filter_commands(after);
                    for r in &ranked {
                        opts.push(format!("{:<10} {}", r.item.name, r.item.help));
                    }
                    if opts.is_empty() {
                        opts.push("(no matching command)".into());
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
            let overlay_text = match kind {
                0 => String::new(),
                4 => Self::render_help_overlay(),
                _ => Self::overlay_lines(&title, &opts, sel, active),
            };
            let overlay_vec: Vec<&str> = overlay_text.lines().collect();
            redraw(&editor, &overlay_vec, sel, &mut input_row);

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
                                if kind == 1 {
                                    let ranked = slash::filter_commands(&cur_word);
                                    if let Some(cmd) = ranked.get(sel) {
                                        editor.set(match cmd.item.kind {
                                            slash::SlashKind::Model => "/model ",
                                            slash::SlashKind::Thinking => "/thinking ",
                                            slash::SlashKind::Help => "/help",
                                            slash::SlashKind::Quit => "/quit",
                                        });
                                    }
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
                                                        redraw(&editor, &[], sel, &mut input_row);
                                                        return Some(InputAction::Quit);
                                                    }
                                                    slash::SlashKind::Model => {
                                                        editor.set("/model ");
                                                    }
                                                    slash::SlashKind::Thinking => {
                                                        editor.set("/thinking ");
                                                    }
                                                }
                                            }
                                        }
                                        2 => {
                                            let (entries, _) =
                                                Self::model_overlay_entries(ctx, &cur_filter);
                                            if let Some(e) = entries.get(sel) {
                                                let model = e
                                                    .id
                                                    .splitn(2, '/')
                                                    .nth(1)
                                                    .unwrap_or(&e.id)
                                                    .to_string();
                                                redraw(&editor, &[], sel, &mut input_row);
                                                return Some(InputAction::SetModel(
                                                    e.provider.clone(),
                                                    model,
                                                ));
                                            }
                                        }
                                        3 => {
                                            let levels = Self::thinking_overlay_entries(&cur_filter);
                                            if let Some(l) = levels.get(sel) {
                                                redraw(&editor, &[], sel, &mut input_row);
                                                return Some(InputAction::SetThinking(
                                                    l.as_str().to_string(),
                                                ));
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
                                    editor.set("/model");
                                }
                                3 => {
                                    editor.set("/thinking");
                                }
                                _ => {}
                            },
                            _ => {
                                if let Some(a) =
                                    Self::edit_key(key, &mut editor, history, &mut nav, &mut draft)
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
                        Self::edit_key(key, &mut editor, history, &mut nav, &mut draft)
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
                Ok(_) => {} // resize/mouse: ignored (Stage 6 redraw)
                Err(_) => return None,
            }
        }
    }

    /// Render the `/model` / `/thinking` overlay: a panel title line plus
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

    /// Render the `/model` / `/thinking` overlay: a panel title line plus
    /// wrapped, colored option rows with a `> ` selection marker and
    /// (for `/model`) a green `*` on the currently-active model. Returns
    /// plain text; the caller emits it directly above the input line.
    pub fn overlay_lines(
        title: &str,
        options: &[String],
        sel: usize,
        active: Option<usize>,
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
            let wrapped = wrap(o, opt_w);
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
            let marker = if *first && *i == sel {
                "> "
            } else if *first && active == Some(*i) {
                "* "
            } else {
                "  "
            };
            let body = if *first {
                format!("{}{}", marker, line)
            } else {
                line.clone()
            };
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
        Self::overlay_lines("Help  (any key closes)", &lines, usize::MAX, None)
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
            crate::fuzzy::fuzzy_filter(&entries, filter, |e| format!("{} {}", e.display, e.id))
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
    ) -> Option<InputAction> {
        match key.code {
            KeyCode::Enter => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    editor.insert_newline();
                    None
                } else {
                    Some(InputAction::Submit(editor.text()))
                }
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
                if !editor.move_up_line() {
                    if let Some(s) = history.prev(&editor.text(), nav, draft) {
                        editor.set(&s);
                    }
                }
                None
            }
            KeyCode::Down => {
                if !editor.move_down_line() {
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

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.buf.remove(self.cursor);
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
                for (ri, lr) in lrows.iter().enumerate() {
                    rows.push(lr.clone());
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

/// Word-boundary wrap of one logical line (no `'\n'`). Returns the visual rows
/// and, if `cursor` is `Some`, the (row, col) where that char index lands.
/// Long words and CJK (no spaces) are char-wrapped; spaces are break
/// opportunities and dropped at row starts.
fn wrap_line(
    line: &[char],
    first_width: usize,
    width: usize,
    cursor: Option<usize>,
) -> (Vec<String>, Option<(usize, usize)>) {
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
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
                i += 1;
                continue;
            }
            if col + 1 > avail(row) {
                // The trailing space is dropped and the next row starts here.
                if cursor == Some(i) {
                    cursor_pos = Some((row, col));
                }
                rows.push(std::mem::take(&mut cur));
                row += 1;
                col = 0;
                i += 1;
                continue;
            }
            cur.push(' ');
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
                // Cursor may sit anywhere inside the word.
                if let Some(cu) = cursor {
                    if i <= cu && cu < j {
                        cursor_pos = Some((row, col + (cu - i)));
                    }
                }
                for k in i..j {
                    cur.push(line[k]);
                }
                col += wlen;
                i = j;
            } else {
                let mut k = i;
                while k < j {
                    if col >= avail(row) {
                        rows.push(std::mem::take(&mut cur));
                        row += 1;
                        col = 0;
                    }
                    if cursor == Some(k) {
                        cursor_pos = Some((row, col));
                    }
                    cur.push(line[k]);
                    col += 1;
                    k += 1;
                }
                i = j;
            }
        } else if col + wlen <= avail(row) {
            // Cursor may sit anywhere inside the word.
            if let Some(cu) = cursor {
                if i <= cu && cu < j {
                    cursor_pos = Some((row, col + (cu - i)));
                }
            }
            for k in i..j {
                cur.push(line[k]);
            }
            col += wlen;
            i = j;
        } else {
            rows.push(std::mem::take(&mut cur));
            row += 1;
            col = 0;
            continue; // re-process the word at the start of the new row
        }
    }
    if cursor == Some(n) {
        cursor_pos = Some((row, col));
    }
    rows.push(cur);
    (rows, cursor_pos)
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
    if let Some(home) = std::env::var_os("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("picocode")
                .join("history"),
        );
    }
    if let Some(app) = std::env::var_os("APPDATA") {
        return Some(PathBuf::from(app).join("picocode").join("history"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn backspace_at_boundary_noop() {
        let mut e = LineEditor::new();
        e.insert('a');
        e.insert('b');
        e.home();
        assert_eq!(e.cursor, 0);
        e.backspace(); // at start -> no change
        assert_eq!(e.cursor, 0);
        assert_eq!(e.text(), "ab");
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
    fn backspace_middle() {
        let mut e = LineEditor::new();
        e.insert_str("abc");
        e.left();
        e.backspace(); // deletes 'b', the char before the cursor
        assert_eq!(e.text(), "ac");
        assert_eq!(e.cursor, 1);
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
        let out = Repl::overlay_lines("Select a model:", &opts, 0, None);
        assert_eq!(out.matches("Select a model:").count(), 1);
        assert!(out.contains("> a: alpha"));
        assert!(out.contains("  b: beta"));
    }

    #[test]
    fn render_select_wraps_long_option_with_aligned_continuation() {
        let long = "deepseek:deepseek-v4-flash ctx 1048576 $0.000000435/0.00000087 per M (est) and some extra trailing text to force a wrap";
        let opts = vec![long.to_string()];
        let out = Repl::overlay_lines("Select a model:", &opts, 0, None);
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
