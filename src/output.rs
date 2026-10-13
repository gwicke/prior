#[cfg(test)]
use crate::ansi::strip_ansi;
#[cfg(test)]
use crate::content::markdown::{answer_theme, guess_lang, highlight_code_fg, render_markdown};
#[cfg(test)]
use crate::content::preview::{
    edit_preview, get_preview, multi_file_blobs, refined_diff_lines, sed_preview_body,
    short_banner, strip_read_lineno, truncate, wrap_with_diff_markers, SED_PREVIEW_BANNER,
};
#[cfg(test)]
use crate::content::text::wrap;
use crate::content::Content;
#[cfg(test)]
use crate::content::ContentDisplayKind;
use crate::draw::ApplyPolicy;
use crate::render::{Element, Frame, RenderAt, Rendered};
#[cfg(test)]
use crate::settings::ShowReasoning;
use crate::settings::ShowReasoningHandle;
use crate::termio::TermSize;
use console::style;
#[cfg(test)]
use markdown_stream::{Parser, StreamParser};
#[cfg(test)]
use markdown_terminal::{Renderer, Theme};
use serde_json::Value;
#[cfg(test)]
use std::io::{self, Write};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Debug, PartialEq)]
pub enum Confirmation {
    Yes,
    No,
    Always,
}

/// The area below the status row: `[Input?, Divider?, Overlay?]`.
///
/// The *rule*, stated once, for two callers that must not be able to disagree
/// about it: [`Output::compose_area`] (production) and the `#[cfg(test)]` frame
/// builder in `repl.rs`. That twin exists only to let a test hand a status
/// through as a literal string — production cannot reach it — so the only thing
/// the two genuinely differ on is where the status row *came from*; how the rest
/// of the area is laid out is this function's and only this function's
/// (ui_draw_v2 §5.1.1).
///
/// `overlay` is `None` when there is no menu: a separator with nothing below it
/// is a rule nobody wanted, and the empty overlay case has no element to draw
/// either.
pub(crate) fn area_body(input: Option<Element>, overlay: Vec<Element>) -> Vec<Element> {
    let mut items = Vec::new();
    items.extend(input);
    if !overlay.is_empty() {
        items.push(Element::divider());
    }
    items.extend(overlay);
    items
}

/// Ends a content batch when dropped: one frame re-emit for the whole burst,
/// instead of one per write (ui_draw_v2 R5).
///
/// Dropped rather than called, because `?`, an early `return` and a panic all
/// release it. The failure mode of *forgetting* to end a batch would be a
/// suppressed reposition — an area that was consumed and never comes back,
/// which is exactly the invariant the per-write re-emit exists to hold. `Drop`
/// makes that unreachable rather than merely documented.
pub struct BatchGuard<'a> {
    out: Option<&'a dyn Output>,
}

impl Drop for BatchGuard<'_> {
    fn drop(&mut self) {
        if let Some(out) = self.out {
            out.end_batch();
        }
    }
}

pub trait Output: Send + Sync {
    /// Open a batch of content writes: repositioning is suppressed until the
    /// returned guard drops, which emits the frame once (ui_draw_v2 R5).
    ///
    /// For bursts — replay, resume, anything that prints history — where the
    /// per-write re-emit costs `O(writes × frame rows)` and the frame is not
    /// changing anyway. **Not** for live streaming: there the re-emit is what
    /// keeps the input row visible while content flows past it, and batching a
    /// turn would hide the prompt for the whole response.
    ///
    /// The default guard does nothing, which is correct rather than lazy: an
    /// output that does not override this already repositions on every write,
    /// and that *is* the unbatched case.
    fn begin_batch(&self) -> BatchGuard<'_> {
        BatchGuard { out: None }
    }
    /// Ends a batch, called by [`BatchGuard`]'s drop. Default no-op.
    fn end_batch(&self) {}
    fn display_text(&self, text: &str);
    /// Render a model *response* as markdown. Defaults to `display_text` so
    /// non-console impls (quiet/log/no-op) need no change; `ConsoleOutput`
    /// overrides it with full block-markdown rendering (code blocks, tables,
    /// headings, lists) via `codewandler-markdown-terminal`. Error/status text
    /// should still go through `display_text` (inline/plain), not this.
    fn display_markdown(&self, text: &str) {
        self.display_text(text);
    }
    // ---- Response event sequence ----
    // Both the live streaming path (`CodeAgent::prompt`) and the session
    // replay path (`replay_units`) drive these methods in the same
    // order, so `ConsoleOutput` only needs one rendering implementation and
    // replay output is guaranteed to match live output.
    //
    // Required call order per agent response:
    //
    //   begin_response()
    //     ( stream_thinking(block) | stream_text(block) )*    ← segment N
    //     [ end_response_segment()                            ← before each tool block
    //         ( display_tool_call(name, args)
    //           display_tool_result(result) )+
    //         ( stream_thinking(block) | stream_text(block) )*  ← segment N+1
    //     ]*
    //   end_response()
    //
    // In the live path, blocks arrive as streaming deltas; in replay they
    // arrive as complete strings. Default no-ops keep non-console impls
    // (quiet / log / no-op) compiling without change.
    /// Called once when an agent response begins; sets up per-response state.
    fn begin_response(&self) {}
    /// A delta of assistant answer text (live path) or a complete text block
    /// (replay path). Called once or more between `begin_response` and
    /// `end_response` / `end_response_segment`.
    fn stream_text(&self, _delta: &str) {}
    /// A delta of model reasoning/thinking (live) or a complete reasoning
    /// block (replay). Already gated by `show_reasoning` level upstream;
    /// implementations may render or suppress unconditionally.
    fn stream_thinking(&self, _delta: &str) {}
    /// Flush the current text/reasoning segment before a tool block. Called
    /// once per tool-using segment, immediately before `display_tool_call`.
    /// Mirrors the `CompletionCall` stream event that fires before tool
    /// execution in the live path.
    fn end_response_segment(&self) {}
    /// Called once when the whole response is complete; flush the final
    /// segment and reset per-response state.
    fn end_response(&self) {}
    fn display_tool_call(&self, name: &str, args: &Value);
    fn display_tool_result(&self, result: &str);
    /// Render a tool execution failure in a dedicated error panel.
    ///
    /// `kind` is a stable, machine-readable classification from
    /// `rig_core::tool::ToolErrorKind::as_str()` (e.g. `"invalid_args"`,
    /// `"timeout"`, `"not_found"`, `"permission_denied"`, `"rate_limited"`,
    /// `"provider"`, `"network"`, `"cancelled"`, `"other"`). `message` is
    /// the operator-facing `ToolExecutionError::message()` — redaction-safe
    /// because rig strips operator-only content into the kind-level
    /// `model_feedback()` separately.
    ///
    /// Default impl formats `{tool_name} failed [{kind}]: {message}` and
    /// forwards to `display_error`. `ConsoleOutput` overrides this with
    /// a styled error panel using the same `paint_panel` /
    /// `tool_panel` helpers as the tool-call and tool-result panels.
    fn display_tool_error(&self, tool_name: &str, kind: &str, message: &str) {
        self.display_error(&format!("{tool_name} failed [{kind}]: {message}"));
    }
    fn get_user_input(&self) -> String;
    fn display_error(&self, error: &str);
    fn display_system(&self, text: &str);
    fn confirm(&self, message: &str) -> Confirmation;
    /// The **no-pump** policy for a gated tool call: what to answer when there
    /// is no REPL to draw a dialog and no key to draw it with (piped/batch
    /// input, one-shot `run_once`, `--quiet`).
    ///
    /// Defaults to the output's own `confirm` behavior, which is the right
    /// answer for every output that *can* ask — `QuietOutput` reads a line from
    /// stdin, `NoOutput` says yes. It is `confirm_default` that an interactive
    /// session reaches only if the port failed to install, because with a port
    /// the REPL's dialog answers instead.
    fn confirm_default(&self, message: &str) -> Confirmation {
        self.confirm(message)
    }
    fn display_separator(&self);
    /// Set the transient status line the output should paint after each content
    /// block while a turn runs — the session / model / context readout.
    ///
    /// Transient and *relative* (never `CUP`, so a viewport scrolled back into
    /// the scrollback is never written into — the write lands below it,
    /// invisible). It occupies the content's last row and the next content
    /// write erases it, so the policy of when to paint it lives in the output
    /// and the caller only supplies the text. Empty disables it. A live frame
    /// (the prompt's input, a dialog) owns the last rows instead, so nothing is
    /// painted then.
    ///
    /// Default no-op: outputs with no cursor to write at have no status line.
    fn set_status(&self, _text: &StatusText) {}

    /// Is a turn running right now?
    ///
    /// The composer needs this to decide whether an empty input is a row (the
    /// resting prompt, between turns) or no row at all (mid-turn, where there is
    /// nothing to submit). `streaming` is already this output's fact — the
    /// status row's dot is resolved from the same flag — so the phase is read
    /// here rather than plumbed in from the caller.
    ///
    /// Default `false`: an output with no streaming phase has only the resting
    /// case, and so always shows its input row.
    fn is_streaming(&self) -> bool {
        false
    }
    fn display_header(
        &self,
        provider: &str,
        model: &str,
        yolo: bool,
        limit: usize,
        persona: Option<&str>,
    );
    /// Begin capturing what the content path hands to the hub, into a byte
    /// buffer. `ConsoleOutput` taps the `ContentWrite` hand-off — content
    /// bytes only, never frame bytes (step 4d, design §Content: an
    /// assertion against content, not against a screen). Must be paired
    /// with a subsequent call to [`end_capture`] which returns the bytes.
    fn begin_capture(&self) {}
    /// End a previous [`begin_capture`] and return the captured bytes.
    /// Returns an empty vec for non-console implementations.
    fn end_capture(&self) -> Vec<u8> {
        Vec::new()
    }
    /// Print plain text with no decorative prefix (used for the startup
    /// hint/status line and REPL-originated confirmations). Advances the
    /// tracked cursor by the wrapped line count.
    fn display_plain(&self, _text: &str) {}

    /// A line the user committed with Enter — a turn's prompt, a slash command,
    /// or a `!command` — bracketed by rules, the way a turn is (ui_draw_v2 R4).
    ///
    /// The default draws all three parts unconditionally, which is what every
    /// implementation that does not override it wants. `ConsoleOutput`
    /// overrides it to *share* a rule with a directly preceding commit rather
    /// than stacking one between each pair; that is a drawing decision about
    /// how a kind looks, and no other implementation draws rules to collapse.
    fn display_committed_prompt(&self, line: &str) {
        self.display_separator();
        self.display_plain(line);
        self.display_separator();
    }
    /// Current cursor row (1-based). The REPL uses it to position the input
    /// line at the natural cursor rather than forcing it to the screen
    /// bottom (which would leave a gap in the scrollback). The default
    /// returns the terminal height (correct for non-console outputs, which
    /// don't run the raw-mode REPL).
    fn cursor_row(&self) -> usize {
        crossterm::terminal::size()
            .map(|(_, h)| h as usize)
            .unwrap_or(24)
    }

    /// The size frames are composed and applied for (ui_draw_v2 §9 #3) — the
    /// one shared source for callers that don't carry their own read (e.g. a
    /// dialog's initial size). Callers that *do* hold a read (the REPL loop's
    /// per-iteration size) pass it into [`Self::draw_frame`] instead.
    /// Default: a fresh terminal query.
    fn frame_size(&self) -> (usize, usize) {
        crossterm::terminal::size()
            .map(|(w, h)| (w.max(1) as usize, h.max(1) as usize))
            .unwrap_or((80, 24))
    }

    /// The one composer for the *area* (ui_draw_v2 §5.1.1):
    /// `[Status?, Input?, Divider?, Overlay?]`.
    ///
    /// **The only place that reads state the pure phases must not see** — but
    /// what it decides is *what the area is made of*, not what each row looks
    /// like. The status row goes in as an element that resolves itself when it
    /// is rendered (`Element::status`), so its text and dot are current on
    /// every draw instead of frozen at compose time. That is not a violation of
    /// purity, it is where it was moved to: the element owns its data source
    /// rather than reaching back into `Output`, so `render_at` is still a
    /// function of `(h, term_w)` plus state the element itself holds.
    ///
    /// The distinction matters because the two events disagree about timing.
    /// Compose runs once per keystroke; `set_status` and the dot flip run at
    /// turn start and end, *after* the REPL composed. Freezing the row at
    /// compose would paint the previous turn's context figure for the whole of
    /// the next one — which is what §4.5's content path re-draw makes visible.
    ///
    /// Callers supply only what changes: the input between turns, the overlay
    /// when a menu is open. Neither the status row nor the divider that
    /// separates an overlay from what is above it is a caller's to build — that
    /// is the whole of what makes this the *one* composer rather than a
    /// convention (§5.1.1).
    ///
    /// The status row takes the **divider's row**, above the input — which is
    /// what makes it permanent and what makes it safe. Being in the frame is
    /// the whole trick: a bar written as content would need a relative erase at
    /// submit, and by then the cursor has moved into the frame region below it,
    /// so the erase would land on the wrong rows. Here the frame's own union
    /// clear takes it, at the one moment the cursor is where the frame says it
    /// is — and the partial row-diff redraws it per keystroke, so the figures
    /// are never stale.
    fn compose_area(&self, input: Option<Element>, overlay: Vec<Element>) -> Vec<Element> {
        let mut items = Vec::new();
        if let Some(row) = self.status_row() {
            // The row between the last content and the bar is the **frame's**,
            // not the content path's: synthetic content no longer emits a
            // trailing separator, so without this there would be nothing
            // between what was written and the status line. Static and part of
            // the area, so it retires with it — a content write that consumes
            // the frame takes this row too, and it can never become a
            // permanent blank in the transcript.
            items.push(Element::lines(vec![String::new()]));
            items.push(row);
        }
        items.extend(area_body(input, overlay));
        items
    }

    /// The area's status row, or `None` when there is nothing to say: one
    /// `Element`, `must == might == 1`.
    ///
    /// *Presence* is decided here and content is not, because presence is the
    /// part that does not move under it: every compose happens after
    /// `read_input`'s `set_status`, so the flag cannot flip between compose and
    /// draw. What the row says is `Element::status`'s own business, resolved
    /// when it is rendered — and at the width it is rendered at, so a width
    /// arrives from `render_at` rather than being carried through compose.
    ///
    /// Default `None` — an output with no screen has no bar to show, which is
    /// also what every non-console output wants from [`Self::compose_area`]:
    /// the input and the overlay, and nothing standing in for a readout.
    /// `ConsoleOutput` is the one that overrides it, and it is the one that
    /// holds `status_text` and `streaming`.
    fn status_row(&self) -> Option<Element> {
        None
    }

    /// Draw a composed UI frame (ui_draw_v2 §5.1) with its top row at absolute
    /// `anchor` (1-based). `dims` is the (width, height) the frame was
    /// composed for: the apply uses exactly these, so the frame's bounding
    /// box can never disagree with its content (§9 #3). Returns the dims
    /// actually applied (clamped), for the caller's repaint bookkeeping.
    /// Default: drop the frame — non-console outputs carry no interactive
    /// UI, so the raw-mode REPL never runs against them. This is the *UI*
    /// surface; content output (`display_*`) stays separate.
    fn draw_frame(
        &self,
        _frame: &Frame,
        _policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        (dims.0.max(1), dims.1.max(1))
    }

    /// Compose and hand an area to the frame hub (ui_draw_v2 §5.1.1): the
    /// pipeline — budget, render, apply, remember — is `RenderManager`'s as
    /// of step 4b; this entry keeps its signature so the REPL/dialog call
    /// sites stay put while the handles around them collapse (step 4f).
    ///
    /// Callers supply *content*: the pieces, **moved in**. Nothing comes back —
    /// no `Vec<Element>` crosses a call site, because no caller ever used one
    /// except to hand it straight to the next statement.
    ///
    /// The budget is derived from `dims`, never passed in — a caller that hands
    /// a budget is being asked to know something it shouldn't. `dims` itself
    /// stays as an argument because it is the caller's *belief* about the
    /// terminal, and resize detection compares that belief against what was
    /// painted.
    ///
    /// Default: drop the frame — non-console outputs carry no interactive UI,
    /// so the raw-mode REPL never runs against them (the same contract
    /// [`draw_frame`](Output::draw_frame)'s default states). `ConsoleOutput`
    /// is the one that forwards: it composes the area (its status row is its
    /// own fact) and hands the stack to the manager.
    fn draw_elements(
        &self,
        _input: Option<Element>,
        _overlay: Vec<Element>,
        _policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        (dims.0.max(1), dims.1.max(1))
    }

    /// The size the **last frame this output applied** took — the one number
    /// resize detection compares the terminal against (ui_draw_v2 §9 #3).
    ///
    /// Before anything has been applied, a fresh terminal query: a caller that
    /// has not painted yet should start from what is actually on screen, not
    /// from a size nobody drew at.
    ///
    /// Distinct from [`frame_size`](Output::frame_size), which is the *current*
    /// terminal size and never a memory — that is what a dialog wants when it
    /// opens, this is what a painter wants when it is deciding whether its last
    /// paint still describes the screen.
    fn painted_size(&self) -> (usize, usize) {
        crossterm::terminal::size()
            .map(|(w, h)| (w.max(1) as usize, h.max(1) as usize))
            .unwrap_or((80, 24))
    }
}

/// Output for a non-terminal consumer (`picocode "q" | cat`): no interactive UI
/// and no reasoning text, because there is no screen to show it on and the
/// answer is what the caller piped for. It therefore has no state at all — it
/// used to hold a `show_reasoning` handle for the spinner, and the spinner is
/// gone (row_accounting §3.6).
#[derive(Default)]
pub struct QuietOutput;

impl QuietOutput {
    pub fn new() -> Self {
        Self
    }
}

impl Output for QuietOutput {
    fn display_text(&self, _text: &str) {}
    fn display_tool_call(&self, _name: &str, _args: &Value) {}
    fn display_tool_result(&self, _result: &str) {}
    fn get_user_input(&self) -> String {
        String::new()
    }
    fn display_error(&self, error: &str) {
        eprintln!("Error: {}", error);
    }
    fn display_system(&self, _text: &str) {}
    fn confirm(&self, message: &str) -> Confirmation {
        eprintln!("Confirm: {} [y/n/s]", message);
        let mut input = String::new();
        let _ = std::io::stdin().read_line(&mut input);
        match input.trim().to_lowercase().as_str() {
            "y" | "yes" => Confirmation::Yes,
            "s" | "session" => Confirmation::Always,
            _ => Confirmation::No,
        }
    }
    fn display_separator(&self) {}
    fn display_header(
        &self,
        _provider: &str,
        _model: &str,
        _yolo: bool,
        _limit: usize,
        _persona: Option<&str>,
    ) {
    }
}

pub struct NoOutput;

impl Output for NoOutput {
    fn display_text(&self, _text: &str) {}
    fn display_tool_call(&self, _name: &str, _args: &Value) {}
    fn display_tool_result(&self, _result: &str) {}
    fn get_user_input(&self) -> String {
        String::new()
    }
    fn display_error(&self, _error: &str) {}
    fn display_system(&self, _text: &str) {}
    fn confirm(&self, _message: &str) -> Confirmation {
        Confirmation::Yes
    }
    fn display_separator(&self) {}
    fn display_header(
        &self,
        _provider: &str,
        _model: &str,
        _yolo: bool,
        _limit: usize,
        _persona: Option<&str>,
    ) {
    }
}

/// Test helper: an `Output` that captures every `display_text` line for
/// assertions; everything else is a no-op (mirroring `NoOutput`).
/// Construct the handle, then feed it into
/// `AppState::for_test_with_output`.
#[cfg(test)]
pub struct RecordingOutput {
    pub lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

#[cfg(test)]
impl RecordingOutput {
    pub fn new() -> (
        std::sync::Arc<Self>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            std::sync::Arc::new(RecordingOutput {
                lines: lines.clone(),
            }),
            lines,
        )
    }

    /// Joined contents of everything displayed so far.
    pub fn joined(lines: &std::sync::Mutex<Vec<String>>) -> String {
        lines.lock().unwrap().join("\n")
    }
}

#[cfg(test)]
impl Output for RecordingOutput {
    fn display_text(&self, text: &str) {
        self.lines.lock().unwrap().push(text.to_string());
    }
    fn display_markdown(&self, text: &str) {
        self.lines.lock().unwrap().push(text.to_string());
    }
    fn display_plain(&self, text: &str) {
        self.lines.lock().unwrap().push(text.to_string());
    }
    fn display_tool_call(&self, _name: &str, _args: &Value) {}
    fn display_tool_result(&self, _result: &str) {}
    fn get_user_input(&self) -> String {
        String::new()
    }
    fn display_error(&self, _error: &str) {}
    fn display_system(&self, text: &str) {
        self.lines.lock().unwrap().push(text.to_string());
    }
    fn confirm(&self, _message: &str) -> Confirmation {
        Confirmation::Yes
    }
    fn display_separator(&self) {}
    fn display_header(
        &self,
        _provider: &str,
        _model: &str,
        _yolo: bool,
        _limit: usize,
        _persona: Option<&str>,
    ) {
    }
}

pub struct LogOutput;

impl Output for LogOutput {
    fn display_text(&self, text: &str) {
        tracing::info!(target: "picocode", "{}", text);
    }

    fn display_tool_call(&self, name: &str, args: &Value) {
        tracing::info!(target: "picocode", "Tool call: {} with args: {:?}", name, args);
    }

    fn display_tool_result(&self, result: &str) {
        tracing::info!(target: "picocode", "Tool result: {}", result);
    }

    fn get_user_input(&self) -> String {
        String::new()
    }

    fn display_error(&self, error: &str) {
        tracing::error!(target: "picocode", "{}", error);
    }

    fn display_system(&self, text: &str) {
        tracing::debug!(target: "picocode", "System: {}", text);
    }

    fn confirm(&self, _message: &str) -> Confirmation {
        Confirmation::No
    }

    fn display_separator(&self) {}

    fn display_header(
        &self,
        provider: &str,
        model: &str,
        yolo: bool,
        limit: usize,
        persona: Option<&str>,
    ) {
        tracing::info!(target: "picocode", "picocode | {} | {} | persona:{} | yolo:{} limit:{}", provider, model, persona.unwrap_or("default"), yolo, limit);
    }
}

/// The merged output — the agent's `Arc<dyn Output>` — whose content half is
/// now the [`Content`] handle (step 3c): `content` owns the boundary, the
/// width watch and the streaming renderers, and `Output`'s content methods
/// forward through it. The frame methods stay here; the pipeline behind them
/// is `RenderManager`'s (4b), reached through the content handle's hub.
pub struct ConsoleOutput {
    /// The content owner (steps 3c–3d), shared by `Arc` with
    /// `AgentConfig.content`, so the config's frame handle and its content
    /// handle are one state. `Output`'s content methods forward through it;
    /// the frame methods reach the drawing edge through its `manager`
    /// (step 4c).
    pub(crate) content: Arc<Content>,
    /// The status line's content — pure frame state, never content's (see the
    /// transition contract). `Arc`'d so [`Element::status`] can hold its own
    /// handle: the row resolves at draw time, not compose time.
    status_text: Arc<Mutex<StatusText>>,
}

/// Terminal width in columns, from the size watch (`crate::termio`, step 2
/// commit A). The 20-column floor is load-bearing: `plan_apply` misbehaves
/// on a narrower geometry, and an uninitialized one must not get there.
/// `ConsoleOutput`'s methods read their own subscribed receiver instead —
/// this free function serves the callers with no output handle (the REPL
/// loop, `v25_listing_node`).
pub fn term_width() -> usize {
    crate::termio::size().width.max(20)
}

/// Terminal height in rows. Symmetric to [`term_width`] — callers use it to
/// budget vertical space for pop-up overlays (e.g. reserving room for menu
/// items below a multi-line header).
pub fn term_height() -> usize {
    crate::termio::size().height.max(1)
}

#[cfg(test)]
mod sed_preview_body_tests {
    use super::{get_preview, sed_preview_body, short_banner, SED_PREVIEW_BANNER};

    const RESULT: &str = "sed preview: 2 blocks for src/x.rs (1 expression). Review the blocks below, then pass them to the `edit` tool as its `blocks` argument.
src/x.rs:5
@@@OLD
    run();
@@@NEW
    walk();
@@@END

src/x.rs:9
@@@OLD
    drop();
    // tail
@@@NEW
    // tail
@@@END
";

    #[test]
    fn renders_blocks_as_a_diff_with_anchors() {
        let lines = sed_preview_body(RESULT).expect("body");
        let joined = lines.join("\n");
        // The counts survive as a short closing line; the instruction telling the
        // model to pass the blocks to `edit` is not something a human needs.
        assert!(
            joined.contains("2 blocks in src/x.rs, from 1 expression"),
            "{joined}"
        );
        assert!(!joined.contains("pass them to the `edit` tool"), "{joined}");
        // Each block becomes a -/+ pair under its anchor.
        assert!(joined.contains("@@ -5,1 +5,1 @@"), "{joined}");
        assert!(joined.contains("-     run();"), "{joined}");
        assert!(joined.contains("+     walk();"), "{joined}");
        assert!(joined.contains("@@ -9,2 +9,1 @@"), "{joined}");
        // A deletion keeps its removed lines and has no additions.
        assert!(joined.contains("-     drop();"), "{joined}");
        assert!(!joined.contains("+     drop();"), "{joined}");
    }

    #[test]
    fn notes_survive_because_they_say_what_is_missing() {
        // `bsed` emits NOTEs straight after the banner, before the first block
        // (see the banner builder in crates/bsed/src/lib.rs).
        let note = "NOTE: only 37 of 152 blocks are shown. The other 115 are NOT included.";
        let with_note = RESULT.replacen("\nsrc/x.rs:5", &format!("\n{note}\nsrc/x.rs:5"), 1);
        assert!(
            with_note.contains(note),
            "fixture must place the note in the banner"
        );
        let joined = sed_preview_body(&with_note).expect("body").join("\n");
        assert!(joined.contains("only 37 of 152 blocks"), "{joined}");
        // It is kept, and it is not mistaken for the one-line summary.
        assert!(
            joined.contains("2 blocks in src/x.rs, from 1 expression"),
            "{joined}"
        );
    }

    #[test]
    fn short_banner_drops_the_model_instruction() {
        assert_eq!(
            short_banner(
                "sed preview: 3 blocks for src/x.rs (2 expressions). Review the blocks below, \
                 then pass them to the `edit` tool as its `blocks` argument."
            ),
            "3 blocks in src/x.rs, from 2 expressions"
        );
        // A no-change banner has nothing to condense.
        assert_eq!(
            short_banner("no lines matched \u{2014} src/x.rs is unchanged by this 1 expression."),
            "no lines matched \u{2014} src/x.rs is unchanged by this 1 expression"
        );
    }

    #[test]
    fn the_label_carries_the_expressions_on_one_line() {
        let args = serde_json::json!({
            "path": "./test_engine.rs",
            "expressions": "s/pub fn /fn /\n    s/^    /        /",
        });
        let (label, extra) = get_preview("sed", &args);
        assert!(label.starts_with("sed ./test_engine.rs  $ "), "{label}");
        // Newlines collapse, so the whole script stays on the label's one line.
        assert!(!label.contains('\n'), "{label}");
        assert!(
            label.contains("s/pub fn /fn /; s/^    /        /"),
            "{label}"
        );
        assert!(extra.is_empty(), "{extra:?}");
    }

    #[test]
    fn a_long_script_is_truncated_rather_than_wrapping_the_panel() {
        let args = serde_json::json!({
            "path": "x.rs",
            "expressions": format!("s/{}/y/", "a".repeat(200)),
        });
        let (label, _) = get_preview("sed", &args);
        assert!(label.ends_with("..."), "{label}");
        assert!(label.lines().count() == 1, "{label}");
    }

    #[test]
    fn leaves_other_results_alone() {
        // Not a sed preview: the caller falls through to the generic renderer.
        assert!(sed_preview_body("Replaced 1 block(s) in src/x.rs").is_none());
        assert!(sed_preview_body("").is_none());
    }

    /// The banner has two shapes in `bsed` — "N blocks for PATH" (the library)
    /// and "N block(s) in M of K file(s) under DIR" (the CLI). The gate keys on
    /// their shared prefix, so both must still render.
    #[test]
    fn both_banner_shapes_are_still_recognized() {
        let multi = "sed preview: 4 block(s) in 2 of 3 file(s) under /tmp/x. \
             Check and apply them with `edit`.\n\
             a/x.rs:5\n@@@OLD\n    run();\n@@@NEW\n    walk();\n@@@END\n";
        let joined = sed_preview_body(multi).expect("body").join("\n");
        assert!(joined.contains("@@ -5,1 +5,1 @@"), "{joined}");
        assert!(
            joined.contains("4 block(s) in 2 of 3 file(s)"),
            "counts lost: {joined}"
        );
    }

    /// An `edit` argument error is not a preview, however block-shaped it looks.
    ///
    /// A blob-shaped one used to be claimed by a content sniff for `@@@OLD`: its
    /// `Expected:` example *is* a well-formed block, so the panel drew the
    /// example as a diff and hid the error behind it. The gate is the
    /// `sed preview:` banner now; no argument error carries one.
    #[test]
    fn an_edit_argument_error_is_not_mistaken_for_a_preview() {
        // Blob shapes (once drawn as a phantom diff) and single-edit shapes
        // alike: none of them is a preview.
        for raw in [
            serde_json::json!({"blocks": "@@ -10,1 +10,1 @@\n- a\n+ b"}),
            serde_json::json!({"path": "src/x.rs", "blocks": "@@@OLD\na\n@@@END"}),
            serde_json::json!({"oldText": "a", "newText": "b"}),
            serde_json::json!({"path": "src/x.rs:10", "startLine": 3, "oldText": "a", "newText": "b"}),
            serde_json::json!({"edits": [{"path": "a.rs", "oldText": "a", "newText": "b"},
                                          {"path": "b.rs", "oldText": "a", "newText": "b"}]}),
        ] {
            let msg = crate::tools::EditArgs::from_wire(&raw).expect_err("must fail");
            assert!(
                !msg.starts_with(SED_PREVIEW_BANNER),
                "claimed as a preview:\n{msg}"
            );
            assert!(
                sed_preview_body(&msg).is_none(),
                "claimed as a preview:\n{msg}"
            );
        }
    }
}

impl ConsoleOutput {
    /// The production constructor: subscribes to the process-wide size watch
    /// and probes the caps once.
    pub fn new(show_reasoning: ShowReasoningHandle) -> Self {
        Self::with_geometry(show_reasoning, crate::termio::subscribe())
    }

    /// The one constructor — everything that is per-process state (the size
    /// watch, the caps probe) arrives here, so a test can substitute the
    /// watch via [`Self::with_size`] without forking the rest of the wiring.
    fn with_geometry(show_reasoning: ShowReasoningHandle, size: watch::Receiver<TermSize>) -> Self {
        Self {
            content: Arc::new(Content::new(size, show_reasoning)),
            status_text: Arc::new(Mutex::new(StatusText::default())),
        }
    }

    /// The content handle this console owns — what `AgentConfig.content` is
    /// set to, so the config's two handles share one `Content`.
    pub fn content(&self) -> Arc<Content> {
        self.content.clone()
    }

    /// A `ConsoleOutput` on a privately seeded size watch — the test
    /// harness's answer to [`Self::new`]. The geometry becomes
    /// per-instance state, so tests that pin a width need no serialization.
    #[cfg(test)]
    pub(crate) fn with_size(show_reasoning: ShowReasoningHandle, size: TermSize) -> Self {
        Self::with_geometry(show_reasoning, crate::termio::fixed(size))
    }

    /// This instance's width: its subscribed watch, with the same 20-column
    /// floor as the [`term_width`] free function.
    fn term_width(&self) -> usize {
        self.content.width()
    }

    /// This instance's height — symmetric to [`Self::term_width`].
    fn term_height(&self) -> usize {
        self.content.height()
    }
}

impl Default for ConsoleOutput {
    fn default() -> Self {
        Self::new(crate::settings::default_show_reasoning_handle())
    }
}

impl ConsoleOutput {}

/// The status line's content, before it is fitted to a width: what names the
/// session, and the model group as separate fields.
///
/// **Data, not drawing, and deliberately not the rendered bar.** `status_bar`
/// below fits it to a width and styles it; nothing here needs to know how wide
/// the terminal is, which is what lets the two halves live on opposite sides of
/// the `agent` → `output` seam instead of the width being passed in from the
/// layer that has no business knowing it.
///
/// The group's fields are kept separate — rather than pre-joined into one
/// string — because a narrow terminal has to give up **whole** fields, from the
/// right, and a joined string can only be cut mid-word. The push order is that
/// drop order, so it is carried here rather than re-decided at the fit.
#[derive(Default, Clone)]
pub struct StatusText {
    /// The session name, or its opening message standing in for it.
    pub name: String,
    /// The model group, in the order a narrow terminal should give it up.
    pub group: Vec<String>,
}

impl StatusText {
    /// Whether there is a bar to draw at all: both fields empty means off.
    ///
    /// Tested on the **stored content**, never on the rendered bar — a rendered
    /// bar is a full-width row, so it is never empty even when there is nothing
    /// to say, and testing *it* would paint a blank bar where the status is
    /// supposed to be off.
    ///
    /// One definition for both paths that need it: [`ConsoleOutput::paint_status`]
    /// (content) and [`Output::status_row`] (frame). Two answers to "is there a
    /// status?" is how the two bars end up disagreeing about their own existence.
    pub(crate) fn is_off(&self) -> bool {
        self.name.is_empty() && self.group.is_empty()
    }
}

/// What separates the name from the model group, and the width it costs.
///
/// `pub(crate)` so the bar's own tests can assert against the separator the fit
/// actually used, rather than a copy of the string that could drift from it.
pub(crate) const BAR_SEPARATOR: &str = " | ";

/// The status line's furniture: the *table header's* background, so the bar
/// reads as the same chrome as a menu's header rather than as another line of
/// content, and a foreground dim enough to stay subordinate to it.
const STATUS_BG: &str = "\x1b[48;5;237m";
const STATUS_FG: &str = "\x1b[38;5;245m";
const STATUS_RESET: &str = "\x1b[0m";

/// The dot's two states. Streaming is green and idle takes the bar's own
/// foreground, so the bar needs no separate "idle" colour to stay subordinate.
const DOT_STREAMING: &str = "\x1b[38;5;41m";
const DOT_IDLE: &str = STATUS_FG;

/// The dot is `●` where there is room and a space where there is not — on a
/// three-column terminal a real dot would leave the name no columns at all.
/// Either way it is the first thing on the row, which is what makes it readable
/// at a glance from a scrollback.
const DOT: &str = "\u{25cf}";
const DOT_AIR: &str = " ";

/// The air that leads the bar and the air that follows it, plus the dot's total
/// width (lead + glyph + gap).
///
/// **Three** pieces, not one, and the trailing one is the reason: the text is
/// right-aligned, so the padding between the dot and the summary is whatever is
/// left over — which on a bar that is nearly full is *nothing*, and the summary
/// then reads as part of the dot's cell. A guaranteed gap column costs nothing
/// when there is padding anyway and is the whole difference when there is not.
///
/// The dot's colour goes between the lead and the glyph, which is why the cell
/// cannot be one string (see [`DOT`]). The gap comes *after* the glyph and
/// before the text's own foreground, so it is painted in the bar's colour like
/// the rest of the row — a space has no foreground to get wrong.
///
/// A terminal narrower than the cell gives way rather than wrapping it: the
/// bar's width is its contract, and a bar wider than the row wraps, and the
/// wrap is the one row movement the status path has to report.
fn dot_cell(width: usize) -> (&'static str, &'static str, usize) {
    match width {
        0 => ("", "", 0),
        1 => ("", "", 1),
        2 => ("", DOT_AIR, 2),
        _ => (DOT_AIR, DOT_AIR, 3),
    }
}

/// The status bar: [`StatusText`] fitted to `width` and rendered as one
/// full-width row, in the state the caller is in.
///
/// Everything in here needs the width, which is why it lives on this side of the
/// seam rather than with the data that feeds it. Which field gives way, the dot
/// cell's share of the budget, the truncation and the padding are *one*
/// decision, and the width is what makes it.
///
/// Deliberately **one** function, called from two places: the area's status row
/// and the transient bar are the same drawing, and two renderings of "fit the
/// group, then style it" would be two places for them to drift apart.
///
/// One pipe, and single spaces between the model fields. The labels went
/// ("ctx 34% · 78k" → "34% 78k") because on a 120-column bar they were most of
/// the line, and because the fields sit in a fixed order that does not change —
/// `model thinking pct size` reads without them once you have seen it twice.
///
/// The name is the elastic part and it *fills* what is left: it is truncated to
/// the columns between the dot and the pipe rather than to a fixed preview width,
/// so a long session name uses the whole gap and ends in an ellipsis instead of
/// stopping short and leaving a hole. The model group is measured first, because
/// it is the part with nothing to give.
/// The area's status row, as an element that resolves **when it is drawn**.
///
/// `Element::lines` would bake `status_bar(...)`'s output into the element at
/// compose time — and compose runs once per keystroke, while the two things
/// the bar shows (`set_status`'s figures and the streaming dot) move at turn
/// start and end. Holding the handles instead makes every draw current,
/// including §4.5 P1's re-emit of the area under a content write.
///
/// The element owns its data source rather than reaching back into `Output`,
/// which is what keeps `render_at` a function of `(h, term_w)` plus state this
/// element holds itself — constructible with no terminal and no output in
/// sight, so §3.1's unit-testability is intact rather than traded away.
struct StatusItem {
    text: Arc<Mutex<StatusText>>,
    streaming: Arc<AtomicBool>,
}

impl RenderAt for StatusItem {
    fn render_at(&self, _h: usize, term_w: usize) -> Rendered {
        let text = self.text.lock().unwrap();
        // Off between compose and render → say nothing rather than render an
        // empty bar's furniture. The row was reserved by the allocator either
        // way, so nothing below it moves.
        let row = if text.is_off() {
            String::new()
        } else {
            status_bar(
                &text,
                term_w,
                self.streaming.load(std::sync::atomic::Ordering::Relaxed),
            )
        };
        Rendered {
            rows: vec![row],
            caret: None,
        }
    }
}

impl Element {
    /// The status row: `must == might == 1`, text resolved at render time.
    /// Presence is `status_row`'s decision; this is only ever built when there
    /// is something to say (see [`StatusItem`]).
    pub(crate) fn status(text: Arc<Mutex<StatusText>>, streaming: Arc<AtomicBool>) -> Self {
        Element::new(1, 1, StatusItem { text, streaming })
    }
}

pub(crate) fn status_bar(text: &StatusText, width: usize, streaming: bool) -> String {
    // Owned because the fit *pops* — that is the whole drop mechanism, and a
    // borrowed slice cannot be popped from. One clone per bar, on a path that is
    // about to allocate the row anyway.
    let mut fields = text.group.clone();
    let dot_cols = dot_cell(width).2;
    let avail = width.saturating_sub(dot_cols);
    // The separator costs columns only while there *is* a group to separate from.
    // It is re-read rather than cached, because a group that drops to empty must
    // not leave the name three columns short of a pipe that is no longer there.
    let sep_cols = |g: &str| {
        if g.is_empty() {
            0
        } else {
            BAR_SEPARATOR.len()
        }
    };
    // The group is fitted first, and gives way from the right: the context figure
    // first, because it is the field that changes every turn and a missing one
    // reads as "no context yet" rather than as a wrong model; then the thinking
    // level; the model name last, because it is what identifies who is answering.
    let mut group = fields.join(" ");
    while !fields.is_empty()
        && crate::row_count::display_width_ansi(&group) + sep_cols(&group) > avail
    {
        fields.pop();
        group = fields.join(" ");
    }
    // A model name alone can still be too wide for a tiny terminal. Cut it —
    // `truncate_to_width(_, 0)` returns its input unchanged, so a zero budget has
    // to become an empty string rather than the whole name.
    let group_room = avail.saturating_sub(sep_cols(&group));
    let group = if crate::row_count::display_width_ansi(&group) > group_room {
        if group_room == 0 {
            String::new()
        } else {
            crate::row_count::truncate_to_width(&group, group_room)
        }
    } else {
        group
    };

    // What is left of the row belongs to the name, and the ellipsis is the
    // point: it takes every column the model group did not.
    let name_room = group_room.saturating_sub(crate::row_count::display_width_ansi(&group));
    // A name with no room goes rather than being cut to a single ellipsis, which
    // identifies nothing and costs the reader a column to skip over.
    let name = if text.name.is_empty() || name_room == 0 {
        String::new()
    } else {
        crate::row_count::truncate_to_width(&text.name, name_room)
    };

    let line = match (name.is_empty(), group.is_empty()) {
        (true, _) => group,
        (false, true) => name,
        (false, false) => format!("{name}{BAR_SEPARATOR}{group}"),
    };

    let dot = if streaming { DOT_STREAMING } else { DOT_IDLE };
    let (air, gap, dot_cols) = dot_cell(width);
    let pad = width
        .saturating_sub(dot_cols)
        .saturating_sub(crate::row_count::display_width_ansi(&line));
    // Air, the dot's colour, the glyph, a gap, then the text's own foreground:
    // the colour has to bracket the glyph exactly, and the text's has to follow
    // it because `38;5;245m` and the dot's colour are the same attribute slot.
    format!(
        "{STATUS_RESET}{STATUS_BG}{air}{dot}{DOT}{gap}{STATUS_FG}{}{line}{STATUS_RESET}",
        " ".repeat(pad)
    )
}

impl ConsoleOutput {
    // --- Multi-file rendering primitives ----------------------------------
    //
    // These live on an inherent impl, not in `impl Output`: they are the
    // *same* rendering a single-file call uses, invoked once per file, so the
    // view layer has exactly one code path for a tool call-result pair.
}

impl Output for ConsoleOutput {
    fn cursor_row(&self) -> usize {
        // The DSR answer is obtained through the terminal I/O layer (step 2
        // commit C): `termio::query_cursor_row` pauses the sole stdin owner
        // around the query so the reply cannot be swallowed, then this re-anchors
        // the draw cache with the answer.
        let row = crate::termio::query_cursor_row();
        self.content
            .manager
            .lock()
            .unwrap()
            .draw()
            .reanchor_to_cursor(row)
    }

    fn frame_size(&self) -> (usize, usize) {
        (self.term_width(), self.term_height())
    }

    fn painted_size(&self) -> (usize, usize) {
        self.content.manager.lock().unwrap().draw().painted_size()
    }

    fn begin_batch(&self) -> BatchGuard<'_> {
        // The mechanics (flag, debug assert, the deferred reposition) live on
        // the hub (step 4e); only the guard, which has to name an `Output`
        // for its drop, is this layer's.
        if self.content.manager.lock().unwrap().begin_batch() {
            BatchGuard { out: Some(self) }
        } else {
            BatchGuard { out: None }
        }
    }

    fn end_batch(&self) {
        self.content.manager.lock().unwrap().end_batch();
    }

    /// Bracketed, and — where the previous content was also a committed
    /// prompt — *sharing* the closing rule with it instead of stacking one
    /// (ui_draw_v2 R4).
    ///
    /// **One** `with_content_writer`, declared as `Prompt`. The rules are
    /// formatting, drawn here inside the block the prompt owns, rather than
    /// separate writes that would each have to declare a kind of their own —
    /// which is also why the share-or-open decision is taken *before* that
    /// write: read after it, the last kind could only ever be this prompt.
    ///
    /// ```text
    /// ────
    /// first
    /// ────
    /// second
    /// ────
    /// ```
    ///
    /// The prompt stops at its rules rather than carrying blank lines of its
    /// own: a blank row either side comes from `boundary_spacer`, so a prompt
    /// against model output reads as a turn while two adjacent prompts read as
    /// two commits sharing one rule.
    fn display_committed_prompt(&self, line: &str) {
        self.content.display_committed_prompt(line);
    }

    fn draw_frame(
        &self,
        frame: &Frame,
        policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        // Through the hub, not straight to `Draw` (step 4e): a frame no
        // composition produced clears the hub's recipe, so a content write
        // must not re-emit an area that is no longer on screen.
        self.content
            .manager
            .lock()
            .unwrap()
            .draw_frame(frame, policy, dims)
    }

    /// The frame path's entry (step 4b): compose the area — the status row is
    /// this output's own fact (see [`Self::status_row`]), so the compose head
    /// stays here — then hand the stack to the manager, which budgets,
    /// renders, applies and remembers it through the `Draw` under its roof
    /// (step 4c).
    fn draw_elements(
        &self,
        input: Option<Element>,
        overlay: Vec<Element>,
        policy: ApplyPolicy,
        dims: (usize, usize),
    ) -> (usize, usize) {
        let stack = self.compose_area(input, overlay);
        self.content
            .manager
            .lock()
            .unwrap()
            .render(stack, policy, dims)
    }

    fn display_plain(&self, text: &str) {
        self.content.display_plain(text);
    }

    fn display_markdown(&self, text: &str) {
        self.content.display_markdown(text);
    }

    fn display_text(&self, text: &str) {
        self.content.display_text(text);
    }

    fn begin_response(&self) {
        self.content.begin_response();
    }

    fn stream_text(&self, delta: &str) {
        self.content.stream_text(delta);
    }

    fn stream_thinking(&self, delta: &str) {
        self.content.stream_thinking(delta);
    }

    fn end_response_segment(&self) {
        self.content.end_response_segment();
    }

    fn end_response(&self) {
        self.content.end_response();
    }

    fn display_tool_call(&self, name: &str, args: &Value) {
        self.content.display_tool_call(name, args);
    }

    /// A tool block ends without a blank line: the panel rows are the last
    /// thing written, so whatever comes next — the next segment's text, another
    /// tool call, the turn's trailing blank — opens with one. Every exit below
    /// ends the block with it, which is the knowledge the `last_was_tool` flag
    /// used to carry, stated by the block that has it.
    fn display_tool_result(&self, result: &str) {
        self.content.display_tool_result(result);
    }

    fn get_user_input(&self) -> String {
        use std::io;
        // Through the counter like every other content write: it measures zero
        // rows (the prompt does not move the cursor down) but it does close any
        // blank line, so a block that follows does not inherit one.
        self.content
            .write_content(format!("{} ", style("❯").bold().blue()).as_bytes());
        let mut input = String::new();
        let _ = io::stdin().read_line(&mut input);
        input.trim().to_string()
    }

    fn display_error(&self, error: &str) {
        self.content.display_error(error);
    }

    fn display_system(&self, text: &str) {
        self.content.display_system(text);
    }

    /// The **no-pump** confirm policy: nobody is driving keys, so there is no
    /// dialog to show, and this output denies.
    ///
    /// This used to open the blocking dialog in `ui::confirm`. That path is
    /// gone — a confirmation is session state in the REPL's pump now
    /// (doc/confirm_as_menu_261002.md) — so the only callers left here are the
    /// ones with no REPL to ask: one-shot `run_once`, piped/batch input, and
    /// `--quiet`. Denying is the safe direction for all of them: it is what
    /// `Esc` meant, and it cannot run a tool the user never agreed to. An
    /// interactive session reaches this only if the port failed to install,
    /// which is a bug worth seeing rather than papering over.
    fn confirm(&self, _message: &str) -> Confirmation {
        Confirmation::No
    }

    fn display_separator(&self) {
        self.content.display_separator();
    }

    fn display_header(
        &self,
        provider: &str,
        model: &str,
        yolo: bool,
        limit: usize,
        persona: Option<&str>,
    ) {
        self.content
            .display_header(provider, model, yolo, limit, persona);
    }

    fn begin_capture(&self) {
        self.content.begin_capture();
    }

    /// The frame path's bar, from the same two facts the content path's is
    /// (see [`Self::paint_status`]).
    ///
    /// `streaming` is read here rather than passed in as `false` by whoever
    /// composes: whether a turn is running is a fact about this output, and a
    /// literal at a call site is a claim that can go stale — which is how the
    /// two bars could ever have rendered the same turn differently.
    ///
    /// `None` when the status is off, for the same reason `paint_status`
    /// returns early: there is no bar to show, and an off status must not
    /// become an empty full-width row.
    fn status_row(&self) -> Option<Element> {
        if self.status_text.lock().unwrap().is_off() {
            return None;
        }
        Some(Element::status(
            self.status_text.clone(),
            self.content.streaming.clone(),
        ))
    }

    /// Remember the status line to paint after each content block; see
    /// [`ConsoleOutput::paint_status`] for the three conditions that decide
    /// whether it actually goes out.
    fn set_status(&self, text: &StatusText) {
        *self.status_text.lock().unwrap() = text.clone();
    }

    fn is_streaming(&self) -> bool {
        self.content
            .streaming
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn end_capture(&self) -> Vec<u8> {
        self.content.end_capture()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The test-local `strip_ansi` helper below needs the lead-byte table.
    use crate::ansi::utf8_len;
    // The pure pipeline the manager's render now runs (and the composer tests
    // re-run) — imported here since the production path left `output.rs`.
    use crate::render::{allocate, live_budget, render, Extent};

    // --- Row accounting: the tracked cursor vs. the real one (phase B) ---
    //
    // A wrong row count does not show up as a wrong number in isolation: the
    // *next* frame anchors one row off, and the visible damage is a clear that
    // lands in the wrong place (row_accounting §6 B2). So the assertion is
    // against the screen: run a writer, replay its bytes through a terminal
    // emulator, and require the tracked row to be the row the cursor is on.

    /// A `ConsoleOutput` writing into a capture buffer, with the shared width
    /// pinned so the expected geometry does not depend on the test runner's
    /// terminal. Height is not pinned (the clamp in `note_lines` needs a real
    /// terminal size), so the cases stay well under one screen.
    ///
    /// The reasoning routing is pinned too, and for the same reason: a capture
    /// harness that inherits `!stdout().is_terminal()` sends reasoning to
    /// *stderr* whenever the runner's stdout is a pipe, so the test sees nothing
    /// and concludes "reasoning was dropped" — when in fact it was diverted.
    /// Under a runner that hands the binary a tty the same value is `false`, the
    /// reasoning lands in the capture, and a test that assumed the pipe case
    /// fails for a reason that has nothing to do with the code. A capturing
    /// harness should capture everything the code might write, so this pins the
    /// answer to the question every test in this file is asking.
    ///
    /// The one test whose subject *is* the routing names both values itself.
    /// A [`StatusText`] whose bar carries exactly `text`: the name, and no model
    /// group — so `status_bar` renders it verbatim between the dot and the padding.
    ///
    /// The tests below use the bar as a *marker* to find on screen; what they are
    /// for is the painting (transience, the gap, the erase, the dim). The fitting —
    /// which field gives way to what, and the ellipsis — is a different contract and
    /// has its own tests beside `status_bar`.
    fn bar_of(text: &str) -> StatusText {
        StatusText {
            name: text.to_string(),
            group: Vec::new(),
        }
    }

    /// The dot's colour is the bar's only state, and it is **derived** from the
    /// response lifecycle rather than passed in at a call site.
    ///
    /// Composed **once**, deliberately. The recipe is what `DrawCache` keeps, and
    /// the two renders below differ only in `streaming` — which is the whole of
    /// §5.1.1's split: presence is compose's, content is the element's, resolved
    /// when it is drawn. Asserting on the *rendered* row is what makes this a
    /// check that the element reads the flag; a flag set and never consulted
    /// would pass a state assertion alone.
    #[test]
    fn the_dot_follows_the_response_lifecycle() {
        let (out, _reasoning) = capturing_output(60);
        out.set_status(&bar_of("[bar]"));
        assert!(
            !out.content
                .streaming
                .load(std::sync::atomic::Ordering::Relaxed),
            "no turn in flight, so the bar is idle"
        );
        // Composed while idle — the REPL composes once per keystroke, and the
        // turn's flag flips *after* that (agent.rs `begin_response`), so a row
        // baked at compose time would show the previous turn's dot.
        let area = out.compose_area(None, Vec::new());
        out.begin_response();
        let during = compose_rows(&area, 60);
        out.end_response();
        let after = compose_rows(&area, 60);
        assert!(
            during.join("\n").contains("\x1b[38;5;41m\u{25cf}"),
            "the same recipe, mid-turn, renders a green dot: {during:?}"
        );
        assert!(
            !after.join("\n").contains("\x1b[38;5;41m\u{25cf}"),
            "and idle again once the turn is over: {after:?}"
        );
        assert!(
            !out.content
                .streaming
                .load(std::sync::atomic::Ordering::Relaxed),
            "and the flag went back down"
        );
    }

    /// The rows a composed list would paint, without touching a terminal.
    ///
    /// Extents alone cannot identify a row: the status and the divider are both
    /// `must == might == 1`, so a test about *what* the composer produced has to
    /// look at the bytes rather than the budgets.
    fn compose_rows(items: &[Element], term_w: usize) -> Vec<String> {
        let extents: Vec<Extent> = items.iter().map(Element::extent).collect();
        let heights = allocate(&extents, live_budget(24));
        let frame = render(items, &heights, term_w);
        frame
            .extents
            .iter()
            .map(|r| frame.buf[r.start..r.end].to_string())
            .collect()
    }

    /// The composer supplies only what changes: the input, and the menu when one
    /// is open. The status row and the divider are its own to resolve, because a
    /// caller that could add either would make it a second place deciding what
    /// the area is made of (ui_draw_v2 §5.1.1).
    #[test]
    fn compose_area_resolves_every_row_it_owns() {
        let (out, _reasoning) = capturing_output(60);
        out.set_status(&bar_of("[bar]"));
        let input = || {
            Element::input(
                crate::repl::LineEditor::new().layout(60, "> "),
                "> ".to_string(),
            )
        };

        // Nothing supplied: the gap and the status, and they come first.
        assert_eq!(
            compose_rows(&out.compose_area(None, Vec::new()), 60).len(),
            2
        );

        // Input only: no separator, because a separator is for a menu.
        let rows = compose_rows(&out.compose_area(Some(input()), Vec::new()), 60);
        assert_eq!(rows.len(), 3, "gap, status, input: {rows:?}");

        // Both: the divider arrives *with* the overlay, not from the caller.
        let overlay = crate::repl::test_overlay(&["row"], 0, 60);
        let rows = compose_rows(&out.compose_area(Some(input()), overlay), 60);
        assert_eq!(
            rows.len(),
            5,
            "gap, status, input, divider, overlay: {rows:?}"
        );
        assert!(
            !rows[0].contains("[bar]") && !rows[0].contains('─'),
            "row 0 is the empty gap above the bar: {:?}",
            rows[0]
        );
        assert!(
            rows[1].contains("\x1b[48;5;237m"),
            "row 1 is the status bar: {:?}",
            rows[1]
        );
        assert!(rows[2].contains("> "), "row 2 is the input: {:?}", rows[2]);
        assert!(rows[3].contains('─'), "row 3 is the divider: {:?}", rows[3]);
        assert!(rows[4].contains("row"), "row 4 is the menu: {:?}", rows[4]);
    }

    /// The frame bar's dot is read from `streaming` at compose time rather than
    /// handed in as `false` by whoever composed, so the frame path and the
    /// content path cannot render the same turn differently. Asserted on the
    /// rendered bytes: a flag nobody reads would still pass a flag check.
    #[test]
    fn compose_area_reads_streaming_rather_than_being_told_it() {
        let (out, _reasoning) = capturing_output(60);
        out.set_status(&bar_of("[bar]"));
        let idle = compose_rows(&out.compose_area(None, Vec::new()), 60);
        assert!(
            !idle[1].contains(DOT_STREAMING),
            "no turn in flight, so the frame bar is idle: {:?}",
            idle[1]
        );

        out.begin_capture();
        out.begin_response();
        let _ = out.end_capture();
        let during = compose_rows(&out.compose_area(None, Vec::new()), 60);
        assert!(
            during[1].contains(DOT_STREAMING),
            "mid-turn the frame bar goes green: {:?}",
            during[1]
        );

        out.begin_capture();
        out.end_response();
        let _ = out.end_capture();
    }

    /// A status with nothing to say draws no row, and an output with no screen
    /// never gets one. Both go through `is_off`/the default `status_row` — the
    /// same answers the content path uses, so the two bars cannot disagree
    /// about whether they exist.
    #[test]
    fn compose_area_drops_a_status_that_has_nothing_to_say() {
        let (out, _reasoning) = capturing_output(60);
        assert!(
            out.compose_area(None, Vec::new()).is_empty(),
            "never set: there is no bar to show"
        );
        out.set_status(&StatusText {
            name: String::new(),
            group: Vec::new(),
        });
        assert!(
            out.compose_area(None, Vec::new()).is_empty(),
            "explicitly off: there is no bar to show"
        );
        out.set_status(&bar_of("[bar]"));
        assert_eq!(
            out.compose_area(None, Vec::new()).len(),
            2,
            "gap, then a bar"
        );

        assert!(
            NoOutput.compose_area(None, Vec::new()).is_empty(),
            "an output with no screen composes no status"
        );
    }

    fn capturing_output(width: u16) -> (ConsoleOutput, crate::settings::ShowReasoningHandle) {
        let reasoning = crate::settings::default_show_reasoning_handle();
        let out = ConsoleOutput::with_size(
            reasoning.clone(),
            TermSize {
                width: width as usize,
                height: 24,
            },
        );
        // Pinned rather than probed: the routing is not this suite's subject,
        // and a capture harness should capture everything the code might
        // write (the doc on this helper says why in full).
        out.content.set_reasoning_on_stderr(false);
        (out, reasoning)
    }

    /// A cloneable byte sink: swapped into `ConsoleOutput::writer` so a test
    /// can read what the content path emitted without a terminal.
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Swap `out`'s byte sink for a recording one, returning the handle a
    /// test reads the bytes back through. The byte-edge seam for tests
    /// that need *frame* bytes: capture taps the content hand-off only
    /// (step 4d, design §Content) — frame bytes can never be caught — so a
    /// test asserting on what reached the terminal reads the writer, which
    /// is the seam `TermIO::recording()` takes over wholesale in 4f.
    fn record_bytes(out: &ConsoleOutput) -> Sink {
        let sink = Sink(Arc::new(Mutex::new(Vec::new())));
        out.content.manager.lock().unwrap().draw().writer = Box::new(sink.clone());
        sink
    }

    /// Tracked cursor row of `out` (0-based, as the screen reports it).
    fn tracked_row(out: &ConsoleOutput) -> usize {
        out.content
            .manager
            .lock()
            .unwrap()
            .draw()
            .cache
            .tracked_row()
            - 1
    }

    /// Row the cursor ends on when `bytes` are replayed into a `width`x24
    /// terminal.
    ///
    /// The ONLCR shim is the one ui_draw_v2 P0 established: production emits
    /// bare `\n` and the tty line discipline turns it into CRLF, so the
    /// emulator has to be fed what the terminal would actually receive.
    /// Without it every LF leaves the column untouched and the whole model
    /// drifts sideways instead of down.
    fn screen_row(bytes: &[u8], width: u16) -> usize {
        let mut p = vt100::Parser::new(24, width, 100_000);
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        p.screen().cursor_position().0 as usize
    }

    #[test]
    fn display_text_row_is_measured_not_predicted() {
        // A message that exactly fills the width, and one that is a column
        // wider: the old `wrap(text).len()` count called both one row.
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.display_text(&"x".repeat(36)); // + "⏺ " = 38 columns
        out.display_text(&"y".repeat(39)); // + "⏺ " = 41 -> wraps
        let bytes = out.end_capture();
        assert_eq!(
            tracked_row(&out),
            screen_row(&bytes, 40),
            "a message that wraps must be counted as two rows"
        );
    }

    #[test]
    fn display_text_counts_a_wide_character_as_two_columns() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        // 19 CJK chars = 38 columns, plus the 2-column marker: wraps at 40.
        out.display_text(&"漢".repeat(19));
        let bytes = out.end_capture();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    #[test]
    fn streamed_answer_is_counted() {
        // The untracked path (§9 #10): a streamed paragraph used to leave the
        // tracked row where it was.
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.stream_text("First paragraph of the answer.\n\n");
        out.stream_text("Second paragraph, streamed.\n\n");
        out.end_response();
        let bytes = out.end_capture();
        assert_eq!(
            tracked_row(&out),
            screen_row(&bytes, 40),
            "streamed content is not accounted for"
        );
    }

    #[test]
    fn streamed_thinking_is_counted() {
        let (out, reasoning) = capturing_output(40);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.begin_capture();
        out.begin_response();
        out.stream_thinking("Thinking about the problem.\n\n");
        let bytes = out.end_capture();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    #[test]
    fn tool_panel_is_counted() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.display_tool_call("Bash", &serde_json::json!({"command": "ls -la"}));
        out.display_tool_result("a\nb\nc");
        let bytes = out.end_capture();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    #[test]
    fn markdown_and_error_and_plain_are_counted() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.display_markdown("# Title\n\nA paragraph long enough to wrap at forty columns.\n");
        out.display_error("an error long enough to wrap onto a second row at forty");
        out.display_plain("plain text that is long enough to wrap at forty columns ok");
        out.display_system("a system line long enough to wrap at forty columns");
        let bytes = out.end_capture();
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    // --- B3a: the separator matrix (characterization) ---
    //
    // The rule under test (row_accounting_b3.md §1): one blank line at every
    // segment boundary, and inside a segment nothing but what the markdown
    // renderer wrote. The second half is the checkable one — the renderer is
    // available directly, so the *host's* contribution is whatever the painted
    // bytes have that a fresh renderer does not produce. This suite measures
    // today's behavior; it changes nothing.

    /// One step of a response, in the order the live path delivers it.
    #[derive(Clone, Copy)]
    enum Step {
        /// A reasoning delta (one chunk of it).
        Think(&'static str),
        /// An answer delta.
        Say(&'static str),
        Tool(&'static str),
        Result(&'static str),
        /// `end_response_segment` — the segment boundary.
        SegmentEnd,
        /// `end_response`.
        ResponseEnd,
    }

    /// How the deltas are split on their way in. `stream_text` sees deltas live
    /// and whole blocks in replay, so any rule that depends on how *often* it
    /// is called is a bug waiting for a different chunk boundary.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Chunking {
        /// One call per step.
        Whole,
        /// One call per byte.
        Bytes,
        /// One call per word, whitespace dropped.
        Words,
    }

    impl Chunking {
        fn all() -> [Chunking; 3] {
            [Chunking::Whole, Chunking::Bytes, Chunking::Words]
        }
        fn name(self) -> &'static str {
            match self {
                Chunking::Whole => "whole",
                Chunking::Bytes => "bytes",
                Chunking::Words => "words",
            }
        }
    }

    /// Deliver one `Say`/`Think` delta in this chunking.
    fn feed(out: &ConsoleOutput, step: Step, chunk: Chunking) {
        let (text, stream): (&str, fn(&ConsoleOutput, &str)) = match step {
            Step::Say(t) => (t, |o, t| o.stream_text(t)),
            Step::Think(t) => (t, |o, t| o.stream_thinking(t)),
            _ => return,
        };
        match chunk {
            Chunking::Whole => stream(out, text),
            Chunking::Bytes => {
                for b in text.as_bytes() {
                    stream(out, std::str::from_utf8(&[*b]).unwrap_or(""));
                }
            }
            Chunking::Words => {
                // Split at whitespace *runs*, keeping them: a chunk boundary
                // must not eat the paragraph breaks, or the test would be
                // measuring the harness instead of the code.
                let mut rest = text;
                while !rest.is_empty() {
                    let end = rest
                        .find(char::is_whitespace)
                        .map(|i| {
                            rest[i..]
                                .find(|c: char| !c.is_whitespace())
                                .map_or(i, |j| i + j)
                        })
                        .unwrap_or(rest.len());
                    if end == 0 {
                        let ws = rest
                            .find(|c: char| !c.is_whitespace())
                            .unwrap_or(rest.len());
                        stream(out, &rest[..ws]);
                        rest = &rest[ws..];
                    } else {
                        stream(out, &rest[..end]);
                        rest = &rest[end..];
                    }
                }
            }
        }
    }

    /// Run a scripted response through the live render path; return the bytes.
    fn run_script(steps: &[Step], chunk: Chunking) -> Vec<u8> {
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        // Reasoning on the captured stream. `capturing_output` already pins
        // this, so the line is redundant — kept because a replay test that
        // silently stopped seeing reasoning would look like a spacing bug, and
        // the pin is exactly the kind of thing a future edit drops as
        // "obviously already handled". The routing itself is the subject of
        // `reasoning_is_routed_by_the_output_layer`, which names both values.
        out.content.set_reasoning_on_stderr(false);
        out.begin_capture();
        out.begin_response();
        for step in steps {
            match step {
                Step::Tool(name) => out.display_tool_call(name, &serde_json::json!({})),
                Step::Result(text) => out.display_tool_result(text),
                Step::SegmentEnd => out.end_response_segment(),
                Step::ResponseEnd => out.end_response(),
                other => feed(&out, *other, chunk),
            }
        }
        out.end_capture()
    }

    /// What the markdown renderer alone produces for `text`: the reference for
    /// "the host adds nothing inside a segment".
    fn renderer_alone(text: &str) -> String {
        let mut parser = StreamParser::new_gfm_stream();
        let mut events = parser.write(text.as_bytes());
        events.extend(parser.flush());
        // The same theme production builds the answer renderer with, or its
        // styling would look like host contribution.
        let mut r = Renderer::new(answer_theme(false), 80);
        let mut out: Vec<u8> = Vec::new();
        let _ = r.feed(&events, &mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Visible lines of painted output, ANSI stripped, *without* the row the
    /// cursor rests on: a stream ending in `\n` leaves the cursor on a row
    /// nothing was written to, and that row reads as blank.
    fn painted_lines(bytes: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes).replace("\r", "");
        let mut lines: Vec<String> = text
            .split('\n')
            .map(|l| strip_ansi(l).trim_end().to_string())
            .collect();
        if text.ends_with('\n') {
            lines.pop();
        }
        lines
    }

    /// Indices of blank lines, and the longest run of consecutive ones.
    fn blank_runs(lines: &[String]) -> (Vec<usize>, usize) {
        let mut runs: Vec<usize> = Vec::new();
        let mut longest = 0;
        let mut cur = 0;
        for (i, l) in lines.iter().enumerate() {
            if l.trim().is_empty() {
                runs.push(i);
                cur += 1;
                longest = longest.max(cur);
            } else {
                cur = 0;
            }
        }
        (runs, longest)
    }

    /// The core of the rule: no two consecutive blank lines, ever. One blank
    /// at a boundary, and the renderer's own spacing nowhere doubled.
    #[track_caller]
    fn assert_no_doubled_blank(name: &str, steps: &[Step], chunk: Chunking) {
        let bytes = run_script(steps, chunk);
        let lines = painted_lines(&bytes);
        let (_, longest) = blank_runs(&lines);
        assert_eq!(
            longest,
            1,
            "{name} [{}]: {longest} consecutive blank lines\n{:#?}",
            chunk.name(),
            lines
        );
    }

    /// One segment, one answer block: the painted output is *exactly* what the
    /// renderer produced, ending at a line boundary. Anything else is a byte
    /// the host contributed on its own.
    #[test]
    fn host_adds_nothing_inside_a_segment() {
        for text in [
            "A single paragraph answer.\n\n",
            "Two paragraphs.\n\nThe second one.\n\n",
            "A paragraph, then a list:\n\n- one\n- two\n\n",
            "Text with `inline code` and a [link](https://x.example).\n\n",
        ] {
            for chunk in Chunking::all() {
                let bytes = run_script(&[Step::Say(text), Step::ResponseEnd], chunk);
                let painted = String::from_utf8_lossy(&bytes).to_string();
                // Neither end may add a separator. At the stream's start there
                // is nothing to separate from, so `boundary_spacer` has no
                // answer for (None, Response) and `begin_response` writes
                // nothing at all. And `end_response` strips the flushes' blank
                // rows, because the row *below* this response is the next
                // block's divider to emit — synthetic content ends at a line
                // boundary and never with a separator. What is left is the
                // renderer's bytes, with its own trailing blank normalized to
                // the one line terminator the last line needs.
                let alone = renderer_alone(text);
                let mut expected = alone.trim_end_matches('\n').to_string();
                if !expected.is_empty() {
                    expected.push('\n');
                }
                assert_eq!(
                    painted,
                    expected,
                    "host contributed extra bytes for {text:?} [{}]",
                    chunk.name()
                );
            }
        }
    }

    /// The same answer split across deltas must paint identically to the
    /// one-call version: chunking is not allowed to be visible.
    #[test]
    fn chunking_is_not_visible() {
        let text = "First paragraph of the answer.\n\nSecond paragraph here.\n\n";
        let whole = run_script(&[Step::Say(text), Step::ResponseEnd], Chunking::Whole);
        for chunk in [Chunking::Bytes, Chunking::Words] {
            assert_eq!(
                run_script(&[Step::Say(text), Step::ResponseEnd], chunk),
                whole,
                "[{}] painted differently from the one-call version",
                chunk.name()
            );
        }
    }

    /// At a segment boundary, exactly one blank line — not zero, not two.
    #[test]
    fn one_blank_line_at_a_segment_boundary() {
        for chunk in Chunking::all() {
            assert_no_doubled_blank(
                "answer, boundary, answer",
                &[
                    Step::Say("The first answer.\n\n"),
                    Step::SegmentEnd,
                    Step::Say("After the tool.\n\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
        }
    }

    /// §4.1 of the plan: reasoning is routed at the output layer, so a
    /// consumer of `picocode "q" | cat` can drop it, and the render path does
    /// not fork on whether there is a terminal.
    ///
    /// Both halves set `reasoning_on_stderr` **explicitly**, because that flag is
    /// the routing and inheriting it would make the test assert the *runner's*
    /// terminal rather than the code. It did exactly that: a test binary's stdout
    /// is usually a pipe, so "no terminal" was taken to be ambiently true, and
    /// under a runner that hands the binary a tty the first half failed with
    /// "reasoning must not reach stdout without a terminal" — the flag had been
    /// `false` because there *was* a terminal, exactly as the flag is supposed to
    /// say. The harness now pins the flag for every other test in the file
    /// (`capturing_output`); the two values are named here because they are this
    /// test's subject.
    #[test]
    fn reasoning_is_routed_by_the_output_layer() {
        let script = [Step::Think("Weighing the options.\n\n"), Step::ResponseEnd];

        // Diverted: reasoning leaves stdout — it shows up on the test runner's
        // stderr instead, which is what makes `2>/dev/null` work for a consumer.
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.content.set_reasoning_on_stderr(true);
        out.begin_capture();
        out.begin_response();
        feed(&out, script[0], Chunking::Whole);
        feed(&out, script[1], Chunking::Whole);
        let piped = out.end_capture();
        assert!(
            !String::from_utf8_lossy(&piped).contains("Weighing the options"),
            "reasoning must not reach stdout when it is routed to stderr"
        );

        // On the screen: the other side of the same decision.
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.content.set_reasoning_on_stderr(false);
        out.begin_capture();
        out.begin_response();
        feed(&out, script[0], Chunking::Whole);
        feed(&out, script[1], Chunking::Whole);
        let tty = out.end_capture();
        assert!(
            String::from_utf8_lossy(&tty).contains("Weighing the options"),
            "reasoning belongs on the screen when there is one"
        );
    }

    /// A tool round then the next segment's answer: one blank at the
    /// boundary, with the panel rows above it.
    #[test]
    fn one_blank_line_after_a_tool_block() {
        for chunk in Chunking::all() {
            assert_no_doubled_blank(
                "tool, boundary, answer",
                &[
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::SegmentEnd,
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
            assert_no_doubled_blank(
                "tool, tool",
                &[
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Tool("Bash"),
                    Step::Result("ok2\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
        }
    }

    /// An **empty** segment declares no boundary: a turn that ends having
    /// streamed nothing is not a segment, so the retry notices that follow it
    /// are consecutive `SystemMessage` blocks and share the space `boundary_spacer`
    /// gives two of the same kind — none.
    ///
    /// The empty `end_response` is the interesting part, and it is invisible in
    /// the output: it writes no bytes at all. What it used to leave behind was
    /// a `closed` flag set by a segment that never existed, so each retry
    /// message met `(ResponseText, closed)` rather than `(SystemMessage, open)`
    /// and was given a blank line it had not earned. `with_content_writer`
    /// decides it now — see the rollback invariants there — and the two
    /// assertions below are the two halves: the block *between* the retries,
    /// and the block *before* the first one.
    #[test]
    fn an_empty_segment_puts_no_blank_between_retries() {
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.content.set_reasoning_on_stderr(false);
        out.begin_capture();
        // Three failed attempts, each an empty turn: `begin_response` then a
        // stream that errors before a single delta reaches the parser.
        out.begin_response();
        out.end_response();
        out.display_text("Error: rate limited. Retrying in 1s...");
        out.begin_response();
        out.end_response();
        out.display_text("Error: rate limited. Retrying in 2s...");
        out.begin_response();
        out.end_response();
        out.display_text("Error: rate limited. Retrying in 4s...");
        let lines = painted_lines(&out.end_capture());

        assert_eq!(
            lines.len(),
            3,
            "three retry notices and nothing else — no blank row above the \
             first one either, since no response preceded it\n{lines:#?}"
        );
        for (i, line) in lines.iter().enumerate() {
            assert!(
                line.contains("Retrying in"),
                "row {i} is a retry notice: {lines:#?}"
            );
        }
    }

    /// The mirror of the case above, and the reason the empty write cannot be
    /// made wholly inert: a segment that *did* stream text still closes
    /// itself, and that closure is what earns the blank line under the notice
    /// the REPL prints afterwards.
    ///
    /// The empty `end_response` here is genuine — the renderer already emitted
    /// the answer as it streamed, so the flush has nothing left to hand back —
    /// yet the segment closed, so the boundary stands. A rollback that dropped
    /// `closes_segment` from every empty write would collapse this to no blank
    /// line, losing the separation between a turn's answer and the status
    /// message that follows it.
    #[test]
    fn a_segment_that_streamed_still_closes_for_the_notice_after_it() {
        let (out, reasoning) = capturing_output(80);
        *reasoning.lock().unwrap() = ShowReasoning::Content;
        out.content.set_reasoning_on_stderr(false);
        out.begin_capture();
        // The answer streams and is emitted as it arrives; the paragraph is
        // closed by its own trailing blank, so the flush below is genuinely
        // empty and `end_response`'s write carries no content.
        out.begin_response();
        out.stream_text("The answer to the question.\n\n");
        out.end_response();
        out.display_text("Error: something happened afterwards");
        let lines = painted_lines(&out.end_capture());

        assert_eq!(
            lines,
            vec![
                // Streamed answer text carries no marker — `⏺` is
                // `display_text`'s, and this row went through the answer
                // renderer instead.
                "The answer to the question.".to_string(),
                String::new(),
                "⏺ Error: something happened afterwards".to_string(),
            ],
            "the answer closed a real segment, so the notice is separated by \
             exactly one blank row"
        );
    }

    /// Reasoning then answer: the boundary blank and the dim reset, in the
    /// order the renderer can accept (a `\n` before the reset would be
    /// swallowed as a block's closing newline).
    #[test]
    fn reasoning_then_answer_has_one_blank() {
        for chunk in Chunking::all() {
            assert_no_doubled_blank(
                "reasoning then answer",
                &[
                    Step::Think("Weighing the options.\n\n"),
                    Step::Say("The answer is 42.\n\n"),
                    Step::ResponseEnd,
                ],
                chunk,
            );
        }
    }

    #[test]
    fn wrap_word_boundary() {
        assert_eq!(wrap("hello world", 5), vec!["hello", "world"]);
    }

    #[test]
    fn wrap_long_no_space() {
        assert_eq!(wrap("abcdefghij", 3), vec!["abc", "def", "ghi", "j"]);
    }

    #[test]
    fn wrap_cjk_respects_width_and_preserves_content() {
        let s = "汉字测试文本";
        let r = wrap(s, 2);
        assert!(
            r.iter().all(|l| l.chars().count() <= 2),
            "a line exceeded the wrap width: {:?}",
            r
        );
        // No spaces are inserted between CJK segments, so concatenation is lossless.
        assert_eq!(r.concat(), s);
    }

    #[test]
    fn wrap_empty_width_returns_lines() {
        assert_eq!(wrap("a\nb", 0), vec!["a", "b"]);
    }

    #[test]
    fn wrap_empty_input() {
        assert_eq!(wrap("", 10), vec![""]);
        // A pure-whitespace logical line keeps its whitespace (it has
        // visible width, unlike a truly empty line).
        assert_eq!(wrap("   ", 10), vec!["   "]);
    }

    #[test]
    fn wrap_preserves_blank_line() {
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn wrap_multiple_spaces_collapsed() {
        assert_eq!(wrap("hello   world", 5), vec!["hello", "world"]);
    }

    #[test]
    fn wrap_keeps_trailing_punctuation_attached() {
        assert_eq!(wrap("Commands:", 100), vec!["Commands:"]);
        assert_eq!(wrap("(provider:name)", 100), vec!["(provider:name)"]);
    }

    #[test]
    fn wrap_single_spaces_around_emdash() {
        assert_eq!(
            wrap("— Show available commands", 100),
            vec!["— Show available commands"]
        );
    }

    #[test]
    fn wrap_preserves_indent_while_collapsing_inter_word_runs() {
        // Leading whitespace is preserved so indentation survives a reflow,
        // but inter-word whitespace runs are still collapsed to a single
        // space (the same behavior as before for the body of the line).
        let help = "  /help      — Show available commands\n  /model      — Select a model";
        let r = wrap(help, 100);
        assert_eq!(
            r,
            vec![
                "  /help — Show available commands".to_string(),
                "  /model — Select a model".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_preserves_indent_on_continuation_lines() {
        // When a logical line is wrapped to multiple physical lines, every
        // continuation line keeps the original indent so the wrapped block
        // reads as one indented paragraph.
        let text = "    one two three four five six seven eight nine ten";
        // Width 15 - 4 (indent) = 11 chars for content per line.
        let r = wrap(text, 15);
        for line in &r {
            assert!(
                line.starts_with("    "),
                "every wrapped line should keep the 4-space indent, got {:?}",
                line
            );
        }
        assert_eq!(
            r,
            vec![
                "    one two".to_string(),
                "    three four".to_string(),
                "    five six".to_string(),
                "    seven eight".to_string(),
                "    nine ten".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_long_word_first_chunk_keeps_indent_subsequent_chunks_plain() {
        // A word longer than the available content width is hard-split. The
        // FIRST chunk keeps the indent (it's the first visual line of that
        // word); the renderer will prepend its own marker to subsequent
        // chunks, so emitting them plain keeps continuation lines aligned.
        let text = "  abcdefghij";
        // Width 5 - 2 (indent) = 3 chars content per chunk.
        let r = wrap(text, 5);
        assert_eq!(
            r,
            vec![
                "  abc".to_string(),
                "def".to_string(),
                "ghi".to_string(),
                "j".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_with_diff_markers_preserves_indent_on_minus_line() {
        // Regression: a `- ` diff line wrapping an indented source line
        // used to collapse the indent (e.g. `-     if foo {` → `- if foo {`),
        // because plain `wrap()` saw the `-` as a normal word and
        // `split_whitespace` collapsed the run of whitespace between the
        // marker and the first token. The diff-marker helper now routes
        // diff lines through a marker-aware path that preserves the body's
        // leading indent.
        let r = wrap_with_diff_markers("-     if foo {", 80);
        assert_eq!(r, vec!["-     if foo {".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_preserves_indent_on_plus_line() {
        let r = wrap_with_diff_markers("+         let x = 1;", 80);
        assert_eq!(r, vec!["+         let x = 1;".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_continuation_aligns_under_marker() {
        // Long diff line: the body wraps, the marker stays on the first
        // line, and subsequent lines indent by two columns to align under
        // the marker column.
        let text = "- foo bar baz qux quux quuz corge grault garply waldo fred";
        // Width 20 - 2 (marker column) = 18 chars for the body per line.
        let r = wrap_with_diff_markers(text, 20);
        // The first line must keep the marker; continuation lines must be
        // prefixed with two spaces to align under `- `.
        assert!(
            r[0].starts_with("- "),
            "first line must keep the marker, got {:?}",
            r
        );
        for line in &r[1..] {
            assert!(
                line.starts_with("  "),
                "continuation lines must indent by two columns, got {:?}",
                line
            );
        }
        // And the body content must survive intact (no collapsing).
        let joined: String = r.iter().flat_map(|l| l.chars()).collect();
        // Every original character must appear in the wrapped output.
        assert!(joined.contains("foo"));
        assert!(joined.contains("garply"));
    }

    #[test]
    fn wrap_with_diff_markers_passes_plain_lines_through_unchanged() {
        // Lines that aren't `- ` / `+ ` markers (e.g. a context line that
        // happens to start with `-`) are wrapped via the regular `wrap`,
        // which keeps the indent-preservation behavior for normal text.
        let r = wrap_with_diff_markers("    hello world", 80);
        assert_eq!(r, vec!["    hello world".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_empty_input() {
        assert_eq!(wrap_with_diff_markers("", 80), vec!["".to_string()]);
    }

    #[test]
    fn wrap_with_diff_markers_multiple_lines_mixed() {
        // A multi-line input that mixes diff markers and a blank logical line.
        let text = "-     if foo {\n+     if bar {\n";
        let r = wrap_with_diff_markers(text, 80);
        assert_eq!(
            r,
            vec![
                "-     if foo {".to_string(),
                "+     if bar {".to_string(),
                "".to_string(),
            ]
        );
    }

    #[test]
    fn truncate_multibyte_no_panic() {
        assert_eq!(truncate("hello", 3), "hel...");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[test]
    fn get_preview_read_file_range() {
        let args = serde_json::json!({"path": "src/output.rs", "offset": 100, "limit": 50});
        assert_eq!(
            get_preview("read_file", &args).0,
            "read src/output.rs [101..150]"
        );
        // zero limit => whole file, no range
        let args = serde_json::json!({"path": "src/output.rs", "offset": 0, "limit": 0});
        assert_eq!(get_preview("read_file", &args).0, "read src/output.rs");
        // offset-only (limit 0) => open-ended range
        let args = serde_json::json!({"path": "src/output.rs", "offset": 19, "limit": 0});
        assert_eq!(
            get_preview("read_file", &args).0,
            "read src/output.rs [20..]"
        );
    }

    /// A blob that will not canonicalize — here a multi-file one, which is
    /// rejected at ingress — must render as a label and nothing else. The bug
    /// this pins: the old `edit` branch fell through to the legacy `old`/`new`
    /// arm with two empty strings, so `content.find("")` hit line 1 and the
    /// panel printed `edit b.rs [1]` above an empty diff.
    #[test]
    fn an_uncanonicalizable_blob_renders_no_range_and_no_diff() {
        let multi = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\nb.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        let (label, diff) = get_preview("edit", &serde_json::json!({ "blocks": multi }));
        assert!(!label.contains('['), "no fabricated range: {label}");
        assert!(diff.is_empty(), "no fabricated diff: {diff:?}");
    }

    /// The mirror: a blob that *does* canonicalize renders fully, and takes
    /// `path` from the blob's own header even though the args carry none.
    #[test]
    fn a_resolvable_blob_renders_its_diff_and_its_header_path() {
        let args = serde_json::json!({
            "blocks": "src/x.rs\n@@@OLD\na\n@@@NEW\nb\n@@@END"
        });
        let (label, diff) = get_preview("edit", &args);
        assert_eq!(label, "edit src/x.rs");
        assert!(!diff.is_empty(), "expected a diff: {diff:?}");
    }

    #[test]
    fn get_preview_bash_full_cmdline() {
        let args = serde_json::json!({"cmd": "cargo build --release && echo done"});
        // No truncation: the full command is preserved verbatim, prefixed with
        // `$` so it reads like a shell prompt.
        assert_eq!(
            get_preview("bash", &args).0,
            "$ cargo build --release && echo done"
        );
    }

    #[test]
    fn get_preview_list_dir_shortened_to_ls() {
        // `list_dir` (and any `List_*` variant) collapses to `ls <path>`.
        for name in ["list_dir", "ListDir", "ListFiles"] {
            let (label, diff) = get_preview(name, &serde_json::json!({"path": "src"}));
            assert_eq!(label, "ls src");
            assert!(diff.is_empty());
        }
    }

    #[test]
    fn get_preview_edit_file_falls_back_without_file() {
        let args = serde_json::json!({
            "path": "/no/such/file.rs",
            "old": "a",
            "new": "b",
            "all": false
        });
        let (label, diff) = get_preview("edit_file", &args);
        assert_eq!(label, "edit /no/such/file.rs");
        // The refined diff is derived purely from oldText/newText, so it is
        // shown even when the file cannot be read (e.g. during replay).
        assert_eq!(
            diff,
            vec!["@@ -1 +1 @@", "- a", "+ b"],
            "diff should come from old/new: {diff:?}"
        );
    }

    #[test]
    fn get_preview_edit_legacy_single_edit_shape() {
        // The legacy top-level oldText/newText shape (accepted by the edit
        // tool but undocumented) must still produce a diff preview.
        let args = serde_json::json!({
            "path": "/no/such/file.rs",
            "oldText": "a",
            "newText": "b"
        });
        let (label, diff) = get_preview("edit_file", &args);
        assert_eq!(label, "edit /no/such/file.rs");
        // File doesn't exist, so no range, but the diff is still rendered from
        // oldText/newText (this is what guarantees replay shows the edits).
        assert_eq!(
            diff,
            vec!["@@ -1 +1 @@", "- a", "+ b"],
            "diff should come from oldText/newText: {diff:?}"
        );
    }

    #[test]
    fn get_preview_edit_multiple_edits_in_batch() {
        // A multi-edit batch should render a diff for each edit, concatenated
        // directly (the `@@` headers already distinguish the change regions),
        // so a single tool call previews every change it makes.
        let args = serde_json::json!({
            "path": "/no/such/file.rs",
            "edits": [
                {"oldText": "a", "newText": "b"},
                {"oldText": "x", "newText": "y"}
            ]
        });
        let (label, diff) = get_preview("edit_file", &args);
        assert_eq!(label, "edit /no/such/file.rs");
        assert_eq!(
            diff,
            vec!["@@ -1 +1 @@", "- a", "+ b", "@@ -1 +1 @@", "- x", "+ y",],
            "each batch edit should produce its own diff: {diff:?}"
        );
    }

    #[test]
    fn edit_preview_preserves_indent_on_diff_lines() {
        // Regression: edit_preview builds `- ` and `+ ` diff lines from the
        // raw source, preserving any leading indentation. Each line is then
        // routed through `wrap_with_diff_markers` (via `tool_panel`), which
        // must not collapse that indentation when reflowing.
        let _tmp = tempfile::tempdir().unwrap();
        let dir = _tmp.path().to_path_buf();
        let path = dir.join("sample.rs");
        // Use a unique content so a leftover file from a previous run
        // can't change the diff shape. Clean up before and after.
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            "fn main() {\n    let x = 1;\n    println!(\"{}\", x);\n}\n",
        )
        .unwrap();
        let old_text = "    let x = 1;\n    println!(\"{}\", x);".to_string();
        let new_text = "    let x = 2;\n    println!(\"{}!\", x);".to_string();
        let edits = vec![crate::tools::EditOp {
            old_text: old_text.clone(),
            new_text: new_text.clone(),
            start_line: None,
            origin: None,
        }];
        let (_range, diff) = edit_preview(path.to_str().unwrap(), &edits);

        // First, the raw edit_preview output must keep the indent: each
        // diff line carries the source line verbatim, with the original
        // leading spaces intact.
        assert!(
            diff.iter().any(|l| l == "-     let x = 1;"),
            "old first line should keep its indent in the raw diff, got {:?}",
            diff
        );
        assert!(
            diff.iter().any(|l| l == "+     let x = 2;"),
            "new first line should keep its indent in the raw diff, got {:?}",
            diff
        );

        // Second, when each diff line is routed through the same wrap path
        // that tool_panel uses, the indent must still survive. This is the
        // visible regression: previously, the indent collapsed to a single
        // space because plain `wrap()` treated `-` as a normal word and
        // `split_whitespace` collapsed the run between `-` and the first
        // source-line token.
        for line in &diff {
            let wrapped = wrap_with_diff_markers(line, 80);
            assert_eq!(
                wrapped,
                vec![line.clone()],
                "indent should survive wrap_with_diff_markers for {:?}",
                line
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refined_diff_lines_shows_changes_without_trailing_newlines() {
        let old_text = "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x);\n}\n";
        let new_text =
            "fn main() {\n    let x = 42;\n    let y = 2;\n    println!(\"{}\", x);\n}\n";
        let lines = refined_diff_lines(old_text, new_text);

        // Hunk header present.
        assert!(
            lines.iter().any(|l| l.contains("@@")),
            "should contain hunk header: {lines:?}"
        );
        // Show the changed lines with marker + space + indented content.
        assert!(
            lines.iter().any(|l| l == "-     let x = 1;"),
            "removed line: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "+     let x = 42;"),
            "added line: {lines:?}"
        );
        // Context lines use a space marker + space then the source indent.
        assert!(
            lines.iter().any(|l| l == "      let y = 2;"),
            "context line: {lines:?}"
        );
        // No line may carry a trailing newline (would split into blank rows).
        for l in &lines {
            assert!(
                !l.ends_with('\n'),
                "line must not end with newline: {:?}",
                l
            );
        }
    }

    #[test]
    fn refined_diff_lines_handles_multiple_hunks() {
        // Changes separated by more than the context radius form distinct
        // hunks (close changes would merge into a single hunk).
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..30 {
            if i == 2 {
                old.push_str("    a = 1;\n");
                new.push_str("    a = 10;\n");
            } else if i == 25 {
                old.push_str("    b = 2;\n");
                new.push_str("    b = 20;\n");
            } else {
                old.push_str(&format!("    line {i};\n"));
                new.push_str(&format!("    line {i};\n"));
            }
        }
        let lines = refined_diff_lines(&old, &new);

        let hunk_count = lines.iter().filter(|l| l.contains("@@")).count();
        assert_eq!(hunk_count, 2, "should have two hunks, got: {lines:?}");
        assert!(lines.iter().any(|l| l == "-     a = 1;"));
        assert!(lines.iter().any(|l| l == "+     a = 10;"));
        assert!(lines.iter().any(|l| l == "-     b = 2;"));
        assert!(lines.iter().any(|l| l == "+     b = 20;"));
    }

    #[test]
    fn refined_diff_lines_no_changes_is_empty() {
        let text = "fn main() {\n    let x = 1;\n}\n";
        let lines = refined_diff_lines(text, text);
        assert!(
            lines.is_empty(),
            "identical text yields no diff lines: {lines:?}"
        );
    }

    #[test]
    fn refined_diff_lines_caps_very_large_replacements() {
        // A huge replacement must be capped and flagged as truncated rather
        // than flooding the output with the entire file.
        let mut old = String::new();
        let mut new = String::new();
        for i in 0..500 {
            old.push_str(&format!("line {i}\n"));
            new.push_str(&format!("line {i} changed\n"));
        }
        let lines = refined_diff_lines(&old, &new);
        assert!(
            lines.len() < 200,
            "should cap huge diffs, got {}",
            lines.len()
        );
        assert!(
            lines.iter().any(|l| l.contains("truncated")),
            "should flag truncation: {lines:?}"
        );
    }

    #[test]
    fn get_preview_unknown_tool_truncates() {
        let args = serde_json::json!({"query": "a very very very very very long value that exceeds fifty characters for sure"});
        let (label, diff) = get_preview("Search", &args);
        assert!(label.starts_with("Search "));
        assert!(label.ends_with("..."));
        assert!(diff.is_empty());
    }

    #[test]
    fn get_preview_grep_full_pattern_and_path() {
        // Matches the `grep_text` tool name (and any `Grep_*` family member).
        let args = serde_json::json!({
            "pat": "fn (main|run)\\(",
            "path": "/home/gabriel/src/picocode2/src"
        });
        for name in ["grep_text", "GrepText", "GrepFiles"] {
            let (label, diff) = get_preview(name, &args);
            // Label is shortened to just `grep`, and both pattern and path are
            // shown in full (no 50-col truncation).
            assert_eq!(
                label,
                "grep fn (main|run)\\( /home/gabriel/src/picocode2/src"
            );
            assert!(diff.is_empty());
        }
    }

    #[test]
    fn render_markdown_gfm_table_is_ansi_free_and_boxed() {
        let out = render_markdown("| a | b |\n|---|---|\n| 1 | 2 |\n", 80, false);
        assert!(!out.contains('\x1b'), "no_color output must be ANSI-free");
        assert!(out.contains('│'), "table needs column separators");
        assert!(out.contains('├'), "table needs a header rule");
        assert!(out.contains('┤'));
        assert!(out.contains('1') && out.contains('2'));
    }

    /// Strip ANSI SGR sequences so we can assert the *highlighted* output still
    /// contains the original source text (highlighting must not drop content).
    fn strip_ansi(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = String::with_capacity(s.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b {
                if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                    continue;
                }
                i += 1;
                continue;
            }
            let ch_len = utf8_len(bytes[i]);
            if i + ch_len <= bytes.len() {
                out.push_str(std::str::from_utf8(&bytes[i..i + ch_len]).unwrap_or("\u{fffd}"));
            }
            i += ch_len.max(1);
        }
        out
    }

    #[test]
    fn render_markdown_code_is_syntax_highlighted_when_colored() {
        let src = "```rust\nfn main() {}\n```\n";
        let colored = render_markdown(src, 80, true);
        // Highlighting enabled on the terminal path: SGR codes wrap the tokens.
        assert!(
            colored.contains('\x1b'),
            "color path must enable syntax highlighting (ANSI present)"
        );
        // Stripping ANSI must recover the source text (no content lost).
        assert!(
            strip_ansi(&colored).contains("fn main()"),
            "highlighting must preserve the source code"
        );
    }

    #[test]
    fn render_markdown_code_has_no_ansi_when_plain() {
        let src = "```rust\nfn main() {}\n```\n";
        let plain = render_markdown(src, 80, false);
        assert!(
            !plain.contains('\x1b'),
            "non-terminal path must stay ANSI-free"
        );
        assert!(plain.contains("fn main()"));
    }

    #[test]
    fn render_markdown_heading_and_blockquote_keep_text() {
        let out = render_markdown("# Title\n> quoted", 80, false);
        assert!(!out.contains('\x1b'));
        assert!(out.contains("Title"));
        assert!(out.contains('│') && out.contains("quoted"));
    }

    /// The renderer is the crate's *incremental* `Renderer` + `StreamParser`:
    /// completed blocks are emitted once (so scrollback stays clean) and code
    /// lines stream as they arrive, while an open paragraph is held back until
    /// it closes (coarse-grained). These tests pin that contract.
    #[test]
    fn incremental_renderer_streams_code_lines_and_coarsens_paragraphs() {
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::no_color(), 80);
        let mut buf: Vec<u8> = Vec::new();

        // Code block streams line-by-line *before* the fence closes.
        let _ = renderer.feed(&parser.write("```python\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("def f():\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("    return 1\n".as_bytes()), &mut buf);
        let mid = String::from_utf8(buf.clone()).unwrap();
        assert!(
            mid.contains("def f():"),
            "code line should render before the fence closes"
        );
        let _ = renderer.feed(&parser.write("```\n".as_bytes()), &mut buf);

        // An open paragraph is NOT emitted until it closes (coarse-grained).
        let before = buf.len();
        let _ = renderer.feed(
            &parser.write("Intro sentence without a trailing blank line".as_bytes()),
            &mut buf,
        );
        assert_eq!(buf.len(), before, "open paragraph must not be emitted yet");

        // Finalizing the document flushes the trailing paragraph.
        let _ = renderer.feed(&parser.flush(), &mut buf);
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("Intro sentence without a trailing blank line"),
            "flush must emit the trailing paragraph"
        );
    }

    #[test]
    fn incremental_renderer_gfm_table_renders() {
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::no_color(), 80);
        let mut buf: Vec<u8> = Vec::new();
        let evs = parser.write("| a | b |\n|---|---|\n| 1 | 2 |\n\n".as_bytes());
        let _ = renderer.feed(&evs, &mut buf);
        let _ = renderer.feed(&parser.flush(), &mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains('│'), "GFM table must render with box chars");
    }

    #[test]
    fn render_markdown_emits_osc8_links_only_when_colored() {
        // `color` is `is_terminal()` in callers, so OSC 8 clickable links must
        // appear on a TTY (color = true) and stay absent for pipes/CI.
        let tty = render_markdown("[text](https://example.com/a)", 80, true);
        assert!(
            tty.contains("\x1b]8;;https://example.com/a\x1b\\"),
            "TTY render must wrap link in OSC 8"
        );
        assert!(tty.contains("text"));
        let piped = render_markdown("[text](https://example.com/a)", 80, false);
        assert!(
            !piped.contains("\x1b]8;;"),
            "non-TTY render must not emit OSC 8 hyperlink sequences"
        );
    }

    #[test]
    fn thinking_streams_inline_and_dimmed() {
        // The renderer fades each row of thinking as it emits it: no SGR is
        // opened by the caller, so there is none to close at the end of the
        // stream and none to reason about at a boundary. Inline formatting must
        // not clear the fading *within* a row — the theme's `reset` re-applies
        // it, and the row's own close follows.
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::dimmed(), 80);
        let mut buf: Vec<u8> = Vec::new();
        let _ = renderer.feed(
            &parser.write("let me **think** step by step\n".as_bytes()),
            &mut buf,
        );
        let _ = renderer.feed(&parser.flush(), &mut buf);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("\x1b[2m"), "reasoning must be dimmed");
        assert!(
            out.contains("\x1b[0m\x1b[2m"),
            "inline formatting must re-apply dim (reset re-dim), not clear it"
        );
        assert!(
            !out.ends_with("\x1b[2m"),
            "the stream must not end with the attribute still open: {out:?}"
        );
    }

    #[test]
    fn thinking_flushes_on_paragraph_boundary() {
        // Regression test: thinking content should emit on paragraph boundaries
        // (blank line) without waiting for end_response.
        let mut parser = StreamParser::new_gfm_stream();
        let mut renderer = Renderer::new(Theme::dimmed(), 80);
        let mut buf: Vec<u8> = Vec::new();

        // First paragraph - no flush yet (no blank line)
        let _ = renderer.feed(&parser.write("\n\n[test] first ".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("\nparagraph \n".as_bytes()), &mut buf);
        let before_flush = String::from_utf8(buf.clone()).unwrap();
        assert!(
            !before_flush.contains("first paragraph"),
            "content held before boundary"
        );

        // Add blank line - should flush
        let _ = renderer.feed(&parser.write("\n".as_bytes()), &mut buf);
        let after_flush = String::from_utf8(buf.clone()).unwrap();
        eprintln!("after newline: {}", after_flush);
        assert!(
            after_flush.contains("first paragraph"),
            "content emitted after boundary"
        );

        // Second paragraph with explicit blank line
        let _ = renderer.feed(&parser.write("second paragraph\n".as_bytes()), &mut buf);
        let _ = renderer.feed(&parser.write("\n".as_bytes()), &mut buf);
        // let _ = renderer.feed(&parser.write("\n".as_bytes()), &mut buf);
        let after_blank = String::from_utf8(buf.clone()).unwrap();
        assert!(
            after_blank.contains("second paragraph"),
            "content emitted on blank line"
        );
    }

    #[test]
    fn multi_turn_answer_boundary_single_blank() {
        // Reproduces the TTY orchestration with *persistent* renderers across a
        // multi-turn turn (thinking1 -> answer1 -> tool -> thinking2 -> answer2),
        // exactly as the `ConsoleOutput` fields behave. Before the fix, the
        // answer renderer's `pending_gap` (set when the answer paragraph was
        // committed at the tool boundary) fired at answer2's first block, on
        // top of the one blank the thinking→answer transition emits — TWO
        // blank lines. Resetting the answer renderer at the tool boundary
        // (as `end_response_segment` now does) drops that pending gap.
        let mut tparser = StreamParser::new_gfm();
        let mut trenderer = Renderer::new(Theme::dimmed(), 80);
        let mut aparser = StreamParser::new_gfm();
        let mut arenderer = Renderer::new(Theme::default(), 80);
        let mut out: Vec<u8> = Vec::new();
        let w = |out: &mut Vec<u8>, s: &[u8]| out.extend_from_slice(s);

        // thinking1 -> answer1
        let _ = trenderer.feed(&tparser.write(b"think one\n"), &mut out);
        let _ = trenderer.feed(&tparser.write(b"\n\n"), &mut out); // transition
        w(&mut out, b"\n"); // separator
        let _ = arenderer.feed(&aparser.write(b"\n\nanswer one\n"), &mut out);
        w(&mut out, b"\n");

        // tool call boundary: commit the answer paragraph, then RESET the
        // answer renderer (mirrors the fix) so no pending gap survives into
        // answer2. The model's leading blank lines stay in answer2 below.
        let _ = arenderer.feed(&aparser.write(b"\n\n"), &mut out);
        arenderer = Renderer::new(Theme::default(), 80);
        w(&mut out, b"\n");

        // thinking2 -> answer2 (model sends leading blank lines)
        let _ = trenderer.feed(&tparser.write(b"think two\n"), &mut out);
        let _ = trenderer.feed(&tparser.write(b"\n\n"), &mut out); // transition
        w(&mut out, b"\n"); // separator
        let _ = arenderer.feed(&aparser.write(b"\n\nanswer two\n"), &mut out);

        // end_response: flush both documents once.
        let _ = arenderer.feed(&aparser.flush(), &mut out);
        let _ = trenderer.feed(&tparser.flush(), &mut out);
        w(&mut out, b"\n");

        let s = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = s.split('\n').collect();
        let idx = match lines.iter().position(|l| l.contains("answer two")) {
            Some(i) => i,
            None => {
                for (i, l) in lines.iter().enumerate() {
                    eprintln!("[{i}] {:?}", l);
                }
                panic!("answer two not found");
            }
        };
        assert!(
            lines[idx - 1].trim().is_empty(),
            "line immediately before answer two must be blank"
        );
        assert!(
            !lines[idx - 2].trim().is_empty(),
            "exactly ONE blank line before answer two (no double gap)"
        );
    }

    /// Option 2 — the choke: `with_content_writer` wraps, so the
    /// renderer-direct feed path can no longer hand the terminal a line wider
    /// than itself. The break is at the cell edge and the continuation is
    /// entered by a byte — the `\x1b[K` after each `\n` in the expectation is
    /// the line-clear arming there. Pre-fix: one 120-cell run, no arm, and
    /// whatever the frame below painted survived in the continuation.
    #[test]
    fn content_choke_wraps_an_overwide_line_before_the_line_clear() {
        let (out, _reasoning) = capturing_output(60);
        let sink = record_bytes(&out);
        let y = "y".repeat(120);
        out.content
            .with_content_writer(60, Some(ContentDisplayKind::ResponseText), false, |w| {
                w.write_all(y.as_bytes()).unwrap();
                w.write_all(b"\n").unwrap();
            });
        let bytes = sink.0.lock().unwrap().clone();
        let text = String::from_utf8(bytes).expect("utf-8 content");
        let expect = format!("\x1b[K{}\n\x1b[K{}\n", &y[..60], &y[60..]);
        assert_eq!(
            text, expect,
            "wrap at the cell edge, clear armed on the continuation"
        );
    }

    /// The safety net never splits an escape: the color opens before the
    /// inserted newline and closes after it — SGR state is the terminal's and
    /// carries the green across the continuation row.
    #[test]
    fn wrapped_content_keeps_its_sgr_across_the_inserted_newline() {
        let (out, _reasoning) = capturing_output(60);
        let sink = record_bytes(&out);
        let y = "y".repeat(120);
        let input = format!("\x1b[32m{y}\x1b[0m\n");
        out.content
            .with_content_writer(60, Some(ContentDisplayKind::ResponseText), false, |w| {
                w.write_all(input.as_bytes()).unwrap();
            });
        let bytes = sink.0.lock().unwrap().clone();
        let text = String::from_utf8(bytes).expect("utf-8 content");
        let expect = format!("\x1b[K\x1b[32m{}\n\x1b[K{}\x1b[0m\n", &y[..60], &y[60..]);
        assert_eq!(text, expect, "escape intact, wrap inside the color");
    }

    /// A capture is an assertion against content, not a screen: the width
    /// guard steps aside and the bytes come back as they went in.
    #[test]
    fn captures_stay_raw_no_wrap_no_clear() {
        let (out, _reasoning) = capturing_output(60);
        let y = "y".repeat(120);
        out.begin_capture();
        out.content
            .with_content_writer(60, Some(ContentDisplayKind::ResponseText), false, |w| {
                w.write_all(y.as_bytes()).unwrap();
                w.write_all(b"\n").unwrap();
            });
        let cap = out.end_capture();
        let mut expect = y.into_bytes();
        expect.push(b'\n');
        assert_eq!(cap, expect, "captured bytes are the raw content");
    }

    /// Capture taps the content hand-off, never the sink (step 4d, design
    /// §Content): the block's bytes stop at the tap, while the frame — its
    /// apply, the consume's walk, the reposition — keeps flowing to the
    /// screen. The narrowing the writer swap retired, pinned: a capture
    /// session with a *live frame* catches content only.
    #[test]
    fn capture_catches_content_but_never_frame_bytes() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        let frame = crate::repl::Repl::compose_frame(
            &crate::repl::LineEditor::new(),
            "",
            None,
            Vec::new(),
            40,
            10,
        );
        out.begin_capture();
        out.draw_frame(&frame, ApplyPolicy::Frame, (40, 24));
        out.display_text("content only");
        let cap = out.end_capture();
        let cap = String::from_utf8_lossy(&cap).into_owned();
        assert!(
            cap.contains("content only"),
            "the content block is what a capture catches: {cap:?}"
        );
        assert!(
            !cap.contains("\x1b[?25"),
            "frame bytes can never be caught (the apply's cursor show is \
             screen-side only): {cap:?}"
        );

        let screen = String::from_utf8_lossy(&sink.0.lock().unwrap().clone()).into_owned();
        assert!(
            screen.contains("\x1b[?25h"),
            "the frame apply reaches the screen: {screen:?}"
        );
        assert!(
            !screen.contains("content only"),
            "…and the content bytes do not — they stopped at the tap: {screen:?}"
        );
    }

    /// `draw_elements` is the old hand-rolled path, not a new one: it must paint
    /// **byte-identically** to `draw_frame(&compose_frame(...))`.
    ///
    /// Two paths that are meant to be the same are usually asserted against a
    /// transcript, which pins each independently and lets them drift together.
    /// Here they are compared *to each other*, because the whole point of §5.1.1
    /// was that nothing changed — only where the decision is made. Exercises all
    /// four variants so a split in any of them shows.
    ///
    /// **The pop-up is deliberately taller than the live budget.** With a
    /// composition that fits, `allocate` early-returns and both paths agree
    /// trivially — the first version of this test passed with the budget line
    /// mutated out from under it. Total `might` here is above `live_budget(20)`
    /// (⌈2/3·20⌉ = 14) and below the raw height, so the two paths only agree if
    /// they derive the *same* budget, not just the same items.
    #[test]
    fn draw_elements_paints_exactly_what_the_hand_rolled_path_paints() {
        let tall: Vec<String> = (0..16).map(|i| format!("row {i}")).collect();
        let tall: Vec<&str> = tall.iter().map(|s| s.as_str()).collect();
        // The pieces, freshly built per call: `draw_elements` moves them in, so
        // each path needs its own. The composer is the shared thing now — both
        // outputs are given the same status, so `compose_area` yields the same
        // stack for each, and the only difference under test is *who runs the
        // pipeline*.
        let pieces = || {
            let layout = crate::repl::LineEditor::new().layout(60, "> ");
            let input = Element::input(layout, "> ".to_string());
            let overlay = crate::repl::test_overlay(&tall, 0, 60);
            (Some(input), overlay)
        };
        let dims = (60, 20);

        let (tail, _rt) = capturing_output(60);
        tail.set_status(&bar_of("[bar]"));
        let tail_sink = record_bytes(&tail);
        let (input, overlay) = pieces();
        let a = tail.draw_elements(input, overlay, ApplyPolicy::Frame, dims);
        let via_tail = tail_sink.0.lock().unwrap().clone();

        let (hand, _rh) = capturing_output(60);
        hand.set_status(&bar_of("[bar]"));
        // Feed `draw_frame` exactly what `draw_elements` composes — same
        // composer, same status, same dims — so the apply half is all that
        // differs.
        let stack = hand.compose_area(pieces().0, pieces().1);
        let extents: Vec<Extent> = stack.iter().map(Element::extent).collect();
        let heights = allocate(&extents, live_budget(dims.1));
        let frame = render(&stack, &heights, dims.0);
        let hand_sink = record_bytes(&hand);
        let b = hand.draw_frame(&frame, ApplyPolicy::Frame, dims);
        let via_hand = hand_sink.0.lock().unwrap().clone();

        assert_eq!(a, b, "the same dims either way");
        assert_eq!(
            via_tail, via_hand,
            "the tail is the old path, byte for byte — not a second behaviour"
        );
    }

    /// Content written while a frame is live takes the frame's rows — and the
    /// frame comes back **below** what was written (ui_draw_v2 §4.5 P1).
    ///
    /// The other half of the reported resume artefact (§1.1): the content
    /// starts at a cursor inside a still-live frame, and the frame used to be
    /// retired outright, so the menu a commit was issued over simply vanished.
    /// Consuming means the content lands *on* the frame's rows and the
    /// reposition puts the frame back underneath, one fold lower — still on
    /// screen, still below the content, and with no gap the erase left behind.
    #[test]
    fn content_over_a_live_frame_reemits_it_below() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        // A menu frame, as `/session` has open when a commit happens: the
        // input, a divider, and overlay rows — the table header is what the
        // report saw survive.
        let overlay = crate::repl::test_overlay_parts(
            Some("MODEL            CTX    PRICE"),
            &[],
            &[
                "  gpt-5          400k   1.00",
                "  claude-opus    200k   3.00",
            ],
            Some(0),
            40,
        );
        let frame = crate::repl::Repl::compose_frame(
            &crate::repl::LineEditor::new(),
            "",
            None,
            overlay,
            40,
            10,
        );
        out.draw_frame(&frame, ApplyPolicy::Frame, (40, 24));
        out.display_text("Cannot start agent: Missing API key for provider.");
        let bytes = sink.0.lock().unwrap().clone();

        let mut p = vt100::Parser::new(24, 40, 1000);
        // The capture starts at the terminal's first row, but the frame apply is
        // relative to the cursor (row_accounting §6b) and is told the anchor is
        // row 3 — which in production is where a run of content lines left the
        // cursor. Stand those two lines in before replaying.
        p.process(b"\r\n\r\n");
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        let rows: Vec<String> = (0..10)
            .map(|r| {
                (0..120)
                    .map(|c| {
                        p.screen()
                            .cell(r, c)
                            .and_then(|x| x.contents().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect()
            })
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("Missing API key")),
            "the message should be on screen: {rows:?}"
        );
        // The last row the content occupies: the message wraps at this width,
        // so it is the row carrying its tail rather than its head.
        let content_last = rows
            .iter()
            .rposition(|r| r.contains("provider"))
            .unwrap_or_else(|| panic!("the message should be on screen: {rows:?}"));
        for leftover in ["MODEL", "CTX", "gpt-5", "claude-opus", "PRICE"] {
            let at = rows
                .iter()
                .position(|r| r.contains(leftover))
                .unwrap_or_else(|| panic!("frame row {leftover:?} should be back: {rows:?}"));
            assert!(
                at > content_last,
                "frame row {leftover:?} belongs below the content, not among it: {rows:?}"
            );
        }
    }

    /// The cursor follows the frame's own caret: shown where the user can type,
    /// hidden when the input is drawn as no row at all (ui_draw_v2 R1).
    ///
    /// Showing it unconditionally was correct while every frame had an input
    /// row, and wrong the moment R1 removed one — the cursor sat blinking
    /// against a row that was never drawn. `Frame::caret` is already `None` in
    /// exactly those cases, so the decision reads the frame rather than
    /// tracking input state separately.
    #[test]
    fn the_cursor_follows_the_frames_own_caret() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        // Streaming with an empty buffer is R1's `input_hidden`: no input row,
        // and so no caret to place.
        out.begin_response();
        crate::repl::paint_prompt(
            &out,
            "",
            &crate::repl::LineEditor::new(),
            Vec::new(),
            (40, 24),
        );
        let bytes = sink.0.lock().unwrap().clone();
        assert!(
            String::from_utf8_lossy(&bytes).contains("\x1b[?25l"),
            "a frame with no caret hides the cursor"
        );

        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        let mut editor = crate::repl::LineEditor::new();
        editor.set("hi");
        crate::repl::paint_prompt(&out, "", &editor, Vec::new(), (40, 24));
        let text = String::from_utf8_lossy(&sink.0.lock().unwrap().clone()).into_owned();
        assert!(
            text.contains("\x1b[?25h"),
            "a frame with a caret shows it: {text:?}"
        );
        assert!(
            !text.contains("\x1b[?25l"),
            "…and does not also hide it: {text:?}"
        );
    }

    /// The clear only applies when a frame is live: streamed blocks just
    /// append, so the tracked row stays in step with the screen and a
    /// scrolled-back viewport is never written into.
    #[test]
    fn content_without_a_live_frame_is_untouched() {
        let (out, _reasoning) = capturing_output(40);
        out.begin_capture();
        out.begin_response();
        out.stream_text("a streamed paragraph.\n\n");
        out.stream_text("another one.\n\n");
        out.end_response();
        let bytes = out.end_capture();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("\x1b[K"),
            "nothing to clear while streaming: {:?}",
            String::from_utf8_lossy(&bytes).escape_debug().to_string()
        );
        assert_eq!(tracked_row(&out), screen_row(&bytes, 40));
    }

    /// A content batch defers the frame's re-emit to the guard's drop instead
    /// of paying one per write (ui_draw_v2 R5).
    ///
    /// Both halves are asserted because either can break alone: the frame is
    /// emitted **once** for a whole burst, *and* it comes back. The invariant
    /// the per-write reposition exists to hold — an area that is consumed is
    /// always put back — is not relaxed here, it is satisfied once at the end
    /// instead of once per write.
    #[test]
    fn a_content_batch_emits_the_frame_once_at_the_end() {
        let (out, _reasoning) = capturing_output(40);
        let sink = record_bytes(&out);
        let overlay = crate::repl::test_overlay_parts(
            Some("MODEL            CTX    PRICE"),
            &[],
            &["  gpt-5          400k   1.00"],
            Some(0),
            40,
        );
        let frame = crate::repl::Repl::compose_frame(
            &crate::repl::LineEditor::new(),
            "",
            None,
            overlay,
            40,
            10,
        );
        out.draw_frame(&frame, ApplyPolicy::Frame, (40, 24));
        {
            let _batch = out.begin_batch();
            out.display_text("first line");
            out.display_text("second line");
            out.display_text("third line");
        } // the guard's drop owns the redraw
        let bytes = sink.0.lock().unwrap().clone();

        // Once for the frame's own apply, once for the batch's single
        // re-emit. Unbatched it is four — the apply plus one per write — and
        // that multiplier is R5.
        let text = String::from_utf8_lossy(&bytes);
        let emits = text.matches("MODEL").count();
        assert_eq!(
            emits, 2,
            "one frame re-emit for the whole batch, got {emits}: {text:?}"
        );

        // …and it came back, below the content it was consumed for.
        let mut p = vt100::Parser::new(24, 40, 1000);
        p.process(b"\r\n\r\n");
        let onlcr: Vec<u8> = bytes
            .iter()
            .flat_map(|&b| {
                if b == b'\n' {
                    vec![b'\r', b'\n']
                } else {
                    vec![b]
                }
            })
            .collect();
        p.process(&onlcr);
        let rows: Vec<String> = (0..10)
            .map(|r| {
                (0..120)
                    .map(|c| {
                        p.screen()
                            .cell(r, c)
                            .and_then(|x| x.contents().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect()
            })
            .collect();
        let content_last = rows
            .iter()
            .rposition(|r| r.contains("third"))
            .unwrap_or_else(|| panic!("content should be on screen: {rows:?}"));
        for leftover in ["MODEL", "gpt-5"] {
            let at = rows
                .iter()
                .position(|r| r.contains(leftover))
                .unwrap_or_else(|| panic!("frame row {leftover:?} should be back: {rows:?}"));
            assert!(
                at > content_last,
                "frame row {leftover:?} belongs below the content: {rows:?}"
            );
        }
    }

    /// A tool block followed by reasoning showed **two** blank lines: the host
    /// wrote the boundary blank, and the long-lived thinking renderer — which
    /// remembered that it had written something — opened its next block with a
    /// gap of its own. `end_tool_block` resets the renderers, so the host's
    /// blank is the only one. Reasoning *before* the tool is what leaves the
    /// renderer mid-document; without it the gap never fires and the bug hides,
    /// which is why this case exists.
    #[test]
    fn one_blank_line_between_a_tool_block_and_reasoning() {
        for (name, steps) in [
            (
                "tool, result, say, end",
                vec![
                    Step::Say("First.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
            (
                "tool, result, think, say, end",
                vec![
                    Step::Say("First.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Think("Weighing again.\n\n"),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
            (
                "tool, result, PENDING think, say, end",
                vec![
                    Step::Say("First.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    // No trailing newlines: the paragraph is still open when
                    // the answer arrives, so it is committed at the boundary.
                    Step::Think("Weighing again."),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
            (
                "think, say, tool, result, think, say, end",
                vec![
                    // Reasoning *before* the tool: the thinking renderer has
                    // written something, so it will want a leading gap for the
                    // next block it renders.
                    Step::Think("Before the tool.\n\n"),
                    Step::Say("Running it.\n\n"),
                    Step::SegmentEnd,
                    Step::Tool("Bash"),
                    Step::Result("ok\n"),
                    Step::Think("After the tool.\n\n"),
                    Step::Say("Done.\n\n"),
                    Step::ResponseEnd,
                ],
            ),
        ] {
            for chunk in Chunking::all() {
                assert_no_doubled_blank(name, &steps, chunk);
            }
        }
    }

    #[test]
    fn reasoning_list_before_answer_has_blank_line() {
        // Regression for two related bugs (see `doc/reasoning_order_opus.md`
        // §4 and §10):
        //  (a) reasoning whose last block is a GFM list was stranded below the
        //      answer because the thinking parser was committed with
        //      `write(b"\n\n")` (a blank line can't close a list) instead of
        //      `flush()`;
        //  (b) in the held-answer (reasoning-enabled) path the answer started
        //      immediately after the reasoning with no separating blank line.
        //
        // Using StreamParser+Renderer directly (not ConsoleOutput) avoids
        // fd-level stdout capture, which is fragile under parallel test
        // execution (the test harness writes "test ... ok" lines to stdout
        // from concurrent threads, polluting the capture).
        let mut thinking_parser = StreamParser::new_gfm();
        let mut thinking_renderer = Renderer::new(Theme::dimmed(), 80);
        let mut answer_parser = StreamParser::new_gfm();
        let mut answer_renderer = Renderer::new(Theme::default(), 80);
        let mut out: Vec<u8> = Vec::new();

        // Simulate reasoning that ends with a bullet list (the stranding trigger).
        // Using `flush()` (not `write(b"\n\n")`) so the list closes properly.
        let _ = thinking_renderer.feed(
            &thinking_parser.write(b"Plan:\n- step one\n- step two\n"),
            &mut out,
        );
        let _ = thinking_renderer.feed(&thinking_parser.flush(), &mut out);

        // Separator: one blank line between reasoning and answer.
        out.extend_from_slice(b"\n");

        // Simulate the answer text.
        let _ = answer_renderer.feed(&answer_parser.write(b"done.\n"), &mut out);
        let _ = answer_renderer.feed(&answer_parser.flush(), &mut out);
        out.extend_from_slice(b"\n");

        let plain = strip_ansi(&String::from_utf8_lossy(&out));
        let lines: Vec<&str> = plain.split('\n').collect();

        // Assert (a): list items appear before the answer.
        let list_idx = lines.iter().position(|l| l.contains("step one")).unwrap();
        let ans_idx = lines.iter().position(|l| l.contains("done.")).unwrap();
        assert!(
            list_idx < ans_idx,
            "reasoning (list) must render BEFORE the answer"
        );

        // Assert (b): exactly one blank line before answer.
        assert!(
            ans_idx > 0 && lines[ans_idx - 1].trim().is_empty(),
            "answer must be separated from reasoning by a blank line; got:\n{plain}"
        );
        assert!(
            ans_idx < 2 || !lines[ans_idx - 2].trim().is_empty(),
            "exactly ONE blank line before answer (no double gap)"
        );
    }

    #[test]
    fn tmp_verify_tool_rendering_helpers() {
        // line-number gutter stripping
        assert_eq!(strip_read_lineno("   1| fn main()"), "fn main()");
        assert_eq!(strip_read_lineno("  12| let x = a | b;"), "let x = a | b;");
        assert_eq!(strip_read_lineno("no gutter here"), "no gutter here");
        assert_eq!(strip_read_lineno("   5| "), "");
        // language guessing
        assert_eq!(guess_lang("src/output.rs"), "rust");
        assert_eq!(guess_lang("a/b.py"), "python");
        assert_eq!(guess_lang("x.unknown"), "text");
        // highlighting: colored path keeps the source text and emits ANSI
        let colored = highlight_code_fg("let x = 1;", "rust", true, term_width());
        // ANSI escapes break the literal substring, so strip them first
        let plain = strip_ansi(&colored);
        assert!(
            plain.contains("let x = 1;"),
            "highlighting must not drop content; got plain={plain:?} from colored={colored:?}"
        );
        assert!(colored.contains('\x1b'), "color path must emit ANSI");
        // non-color path returns the line untouched
        assert_eq!(
            highlight_code_fg("let x = 1;", "rust", false, term_width()),
            "let x = 1;"
        );
    }

    #[test]
    fn edit_arg_error_pipes_align_in_the_rendered_panel() {
        // Regression. The renderer emits correct padding, but the panel pipeline
        // re-wraps lines through `wrap()`, which collapses interior runs of
        // spaces — so a padding-based table rendered fine in unit tests and came
        // out ragged on screen. Assert on the *rendered* columns, not on the
        // renderer's output, or this slips through again.
        let msg = "failed to parse tool arguments: Invalid edit tool arguments: \
Invalid block: an edit block is missing its `@@@NEW` section\n\
\n\
Expected:\nsome/path:1234\n@@@OLD\nold text\n@@@NEW\nnew text\n@@@END\n\
\n\
Actual:\nsrc/output.rs\n@@@OLD\n… (5 lines elided) …\n@@@NEW\nself.content.draw.lock().unwrap().anchor()\n";
        let out = ConsoleOutput::with_size(
            crate::settings::default_show_reasoning_handle(),
            TermSize {
                width: 80,
                height: 24,
            },
        );
        out.display_tool_call("edit", &serde_json::json!({"path": "src/x.rs"}));
        out.begin_capture();
        out.display_tool_result(msg);
        let bytes = out.end_capture();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));

        let columns: Vec<usize> = text
            .lines()
            .filter(|l| l.contains('|'))
            .map(|l| l.find('|').unwrap())
            .collect();
        assert!(columns.len() >= 6, "expected a table, got: {text}");
        let first = columns[0];
        assert!(
            columns.iter().all(|c| *c == first),
            "pipes at {columns:?} should all be at {first}:\n{text}"
        );
        // Still a table, and still the two markers that differ on one row.
        let row = text
            .lines()
            .find(|l| l.contains('|') && l.contains("@@@NEW"))
            .unwrap();
        assert!(
            text.contains("Expected") && text.contains("Actual"),
            "{text}"
        );
        let _ = row;
    }

    /// The rendered panel for an `edit` argument error is the `Expected`/`Actual`
    /// table — never a diff.
    ///
    /// Regression, and the reason the test above could pass while the screen was
    /// broken: the `sed`-preview renderer runs first on *every* result, and it
    /// used to sniff for `@@@OLD`. Every argument error embeds the `Expected:`
    /// example, which is a well-formed block, so the panel drew the example as a
    /// phantom diff and returned before the error was rendered. That test's
    /// fixture only escaped because its `Actual:` leaves a dangling `@@@NEW`,
    /// which breaks the parse. These fixtures are the shapes that do not.
    #[test]
    fn an_edit_arg_error_is_never_rendered_as_a_diff() {
        for raw in [
            // `Actual` is a JSON echo, and an echo elided to a single line.
            serde_json::json!({"oldText": "a", "newText": "b"}),
            serde_json::json!({"path": "src/x.rs:10", "startLine": 3, "oldText": "a", "newText": "b"}),
            serde_json::json!({"edits": [{"path": "a.rs", "oldText": "a", "newText": "b"},
                                          {"path": "b.rs", "oldText": "a", "newText": "b"}]}),
            serde_json::json!({"blocks": "@@ -10,1 +10,1 @@\n- a\n+ b"}),
        ] {
            let err = crate::tools::EditArgs::from_wire(&raw).expect_err("must fail");
            let (out, _reasoning) = capturing_output(80);
            out.begin_capture();
            out.display_tool_result(&format!("failed to parse tool arguments: {err}"));
            let bytes = out.end_capture();
            let text = strip_ansi(&String::from_utf8_lossy(&bytes));

            assert!(!text.contains("@@ -"), "phantom diff for {raw}:\n{text}");
            assert!(
                text.contains("Expected") && text.contains("Actual"),
                "no table for {raw}:\n{text}"
            );
            // The problem itself reaches the screen, not just the shape. The
            // panel word-wraps it at the terminal width, so compare with runs of
            // whitespace collapsed on both sides.
            let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let problem = err.lines().next().unwrap();
            let problem = problem.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                flat.contains(&problem),
                "problem line lost for {raw}: {problem:?}\n{text}"
            );
        }
    }

    /// A single-edit failure shows a definition list, not the block table.
    ///
    /// The `Expected:` section follows the shape the caller used (see
    /// `blockedit::wire`), so this message is JSON on both sides; `arg_error_view`
    /// returns `None` for it and the panel stacks it. The table pairs *block
    /// markers* against what the model sent, so against a JSON `Actual:` it was five
    /// `@@@OLD` rows with one row of JSON beside them.
    #[test]
    fn a_single_edit_failure_renders_as_a_definition_list() {
        let (out, _reasoning) = capturing_output(80);
        out.begin_capture();
        out.display_tool_result(
            "missing required field 'path'\n\nExpected:\n\
             {\"path\": \"some/path.rs\", \"startLine\": 1234, \"oldText\": \"old text\", \
             \"newText\": \"new text\"}\n\nActual:\n{\"newText\":\"b\",\"oldText\":\"a\"}",
        );
        let bytes = out.end_capture();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));

        assert!(!text.contains("|"), "a table was drawn:\n{text}");
        assert!(!text.contains("@@@OLD"), "block markers leaked in:\n{text}");
        assert!(text.contains("Expected:"), "{text}");
        assert!(text.contains("Actual:"), "{text}");
        // Both values survive to the screen, wrapped but intact.
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for want in ["\"oldText\": \"old text\"", "\"oldText\":\"a\""] {
            assert!(flat.contains(want), "{want:?} lost:\n{text}");
        }
    }

    /// The other half of the guard above: gating the result-side `sed` renderer
    /// must not cost the `edit` tool its own blocks-as-diff rendering. That is
    /// the *call* half — `get_preview` → `edit_preview`, fed from the arguments —
    /// and it is a different function on a different input, which is why the two
    /// renderings can be gated independently.
    #[test]
    fn an_edit_call_still_renders_its_blocks_as_a_diff() {
        let (out, _reasoning) = capturing_output(80);
        out.begin_capture();
        out.display_tool_call(
            "edit",
            &serde_json::json!({"blocks": "src/x.rs:5\n@@@OLD\n    run();\n@@@NEW\n    walk();\n@@@END"}),
        );
        let bytes = out.end_capture();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));

        assert!(text.contains("edit src/x.rs"), "no label:\n{text}");
        assert!(text.contains("@@ -1 +1 @@"), "no hunk header:\n{text}");
        assert!(text.contains("-     run();"), "no removed line:\n{text}");
        assert!(text.contains("+     walk();"), "no added line:\n{text}");
    }

    /// And a `sed` preview still renders as a diff *with no call context*: a
    /// replayed result arrives without a preceding `display_tool_call`, so the
    /// gate has to rest on the result's own banner rather than on panel state.
    #[test]
    fn a_sed_preview_renders_without_a_call_context() {
        let (out, _reasoning) = capturing_output(80);
        out.begin_capture();
        out.display_tool_result(
            "sed preview: 1 block for src/x.rs (1 expression). Review the blocks below, \
             then pass them to the `edit` tool as its `blocks` argument.\n\
             src/x.rs:5\n@@@OLD\n    run();\n@@@NEW\n    walk();\n@@@END\n",
        );
        let bytes = out.end_capture();
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));

        assert!(
            text.contains("@@ -5,1 +5,1 @@"),
            "no anchored hunk:\n{text}"
        );
        assert!(text.contains("-     run();"), "no removed line:\n{text}");
        assert!(text.contains("+     walk();"), "no added line:\n{text}");
        assert!(text.contains("1 block in src/x.rs"), "banner lost:\n{text}");
    }

    #[test]
    fn tmp_repro_separator_blanks() {
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response();
        out.begin_capture();

        // Case A: turn ends with a tool result (no trailing answer text).
        out.stream_text("Let me sleep.\n");
        out.end_response_segment();
        out.display_tool_call("bash", &serde_json::json!({"command": "sleep 30"}));
        out.display_tool_result("\"2. sleep 30 — done\"");
        out.end_response();
        out.display_separator();
        let a = out.end_capture();
        let a = strip_ansi(&String::from_utf8_lossy(&a));
        eprintln!("=== Case A (tool-result end) ===\n{:?}", a);

        // Case B: turn ends with plain text answer.
        out.begin_response();
        out.begin_capture();
        out.stream_text("All done.\n");
        out.end_response();
        out.display_separator();
        let b = out.end_capture();
        let b = strip_ansi(&String::from_utf8_lossy(&b));
        eprintln!("=== Case B (text end) ===\n{:?}", b);

        // Case C: turn ends with trailing thinking after a tool result.
        out.begin_response();
        out.begin_capture();
        out.stream_text("Let me sleep.\n");
        out.end_response_segment();
        out.display_tool_call("bash", &serde_json::json!({"command": "sleep 30"}));
        out.display_tool_result("\"2. sleep 30 — done\"");
        out.stream_thinking("wrapping up\n");
        out.end_response();
        out.display_separator();
        let c = out.end_capture();
        let c = strip_ansi(&String::from_utf8_lossy(&c));
        eprintln!("=== Case C (tool + trailing thinking) ===\n{:?}", c);
    }

    /// The invariant: a multi-file batch must render exactly as N single-file
    /// batches would. The call halves come from the same split the multi-file
    /// path uses, so the only thing under test is that the view layer treats
    /// them as the same sequence of call-result pairs.
    #[test]
    fn a_multi_file_batch_renders_identically_to_n_single_file_batches() {
        let blob = "a.rs\n@@@OLD\nx\n@@@NEW\nX\n@@@END\n\n\
                    b.rs\n@@@OLD\ny\n@@@NEW\nY\n@@@END";
        let multi_args = serde_json::json!({ "blocks": blob });
        let singles = multi_file_blobs(&multi_args).expect("multi-file blob");
        assert_eq!(singles.len(), 2);

        let notes = [
            "Replaced 1 block(s) in ./a.rs (matched at ./a.rs:1)",
            "Replaced 1 block(s) in ./b.rs (matched at ./b.rs:1)",
        ];
        let array = blockedit::render_file_results(
            &["a.rs", "b.rs"]
                .iter()
                .zip(notes)
                .map(|(path, note)| blockedit::FileResult {
                    path: (*path).into(),
                    result: Value::String((*note).into()),
                })
                .collect::<Vec<_>>(),
        );

        // A: two single-file pairs, as a sequence of real calls would render.
        let (a, _) = capturing_output(40);
        a.begin_capture();
        for (i, single) in singles.iter().enumerate() {
            a.display_tool_call("edit", single);
            a.display_tool_result(notes[i]);
        }
        let a = strip_ansi(&String::from_utf8_lossy(&a.end_capture()));

        // B: one multi-file pair.
        let (b, _) = capturing_output(40);
        b.begin_capture();
        b.display_tool_call("edit", &multi_args);
        b.display_tool_result(&array);
        let b = strip_ansi(&String::from_utf8_lossy(&b.end_capture()));

        assert_eq!(
            a, b,
            "a multi-file batch must render as N single-file batches"
        );
        // And the rendering this converges on is the real one: a label, a
        // unified diff, and the per-file note — once each, in pair order. (No
        // range here: `edit_preview` derives one by finding `old_text` in the
        // file, and these files do not exist. The diff comes from the edits
        // alone, so it renders regardless.)
        for part in [
            "edit a.rs",
            "@@ -1 +1 @@",
            "Replaced 1 block(s) in ./a.rs",
            "edit b.rs",
            "Replaced 1 block(s) in ./b.rs",
        ] {
            assert!(a.contains(part), "expected {part:?} in:\\n{a}");
        }
        assert_eq!(
            a.matches("Replaced 1 block(s) in").count(),
            2,
            "one note per file:\\n{a}"
        );
    }

    #[test]
    fn a_multi_file_result_renders_one_batch_per_file() {
        // The multi-file result is a bare array; it must render as one labelled
        // batch per file — a failure's short ui_view and a success's note — with
        // no aggregate line and no model_view diff leaking into scrollback.
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_capture();
        out.display_tool_result(
            r#"[{"path":"src/a.rs","result":"Replaced 1 block(s) in ./a.rs"},
                {"path":"src/b.rs","result":{"ui_view":"edit failed: nope","model_view":"SECRET DIFF"}}]"#,
        );
        let d = strip_ansi(&String::from_utf8_lossy(&out.end_capture()));
        assert!(d.contains("edit src/a.rs"), "{d}");
        assert!(d.contains("Replaced 1 block(s) in ./a.rs"), "{d}");
        assert!(d.contains("edit src/b.rs"), "{d}");
        assert!(d.contains("edit failed: nope"), "{d}");
        assert!(!d.contains("SECRET DIFF"), "model_view must not leak: {d}");
        // No aggregate roll-up line.
        assert!(!d.contains("2 of"), "{d}");
    }

    #[test]
    fn an_array_of_plain_strings_is_not_a_multi_file_result() {
        // Another tool's list must keep its existing rendering; the multi-file
        // branch must not swallow it.
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_capture();
        out.display_tool_result(r#"["alpha","beta"]"#);
        let d = strip_ansi(&String::from_utf8_lossy(&out.end_capture()));
        assert!(d.contains("alpha") && d.contains("beta"), "{d}");
    }

    #[test]
    fn tool_result_followed_by_text_has_blank_separator() {
        // Regression (replay path): render_turn must fire `end_response_segment`
        // after a tool block when more text follows, so replay output matches
        // the live path (where each CompletionCall flushes) — the trailing text
        // is separated from the tool-result line by a blank line.
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response();
        out.begin_capture();
        out.stream_text("Let me edit.\n");
        out.end_response_segment();
        out.display_tool_call(
            "edit_file",
            &serde_json::json!({"path": "/no/such.rs", "oldText": "a", "newText": "b"}),
        );
        out.display_tool_result("\"Replaced 1 block(s) in ./no_such.rs\"");
        // The fix: segment boundary after the tool block (mirrors live path).
        out.end_response_segment();
        out.stream_text("Done.\n");
        out.end_response();
        let d = out.end_capture();
        let d = strip_ansi(&String::from_utf8_lossy(&d));
        let after = d.find("Replaced 1 block(s) in ./no_such.rs").unwrap()
            + "Replaced 1 block(s) in ./no_such.rs".len();
        let between = &d[after..d.find("Done.").unwrap()];
        assert!(
            between.contains("\n\n"),
            "expected a blank line between the tool result and trailing text, got {:?}",
            between
        );
    }

    #[test]
    fn write_tool_renders_content_like_read() {
        // Write tool result should render with path header and syntax-highlighted
        // content preview (first 10 lines), matching read_file's rendering.
        let out = ConsoleOutput::new(crate::settings::default_show_reasoning_handle());
        out.begin_response();
        out.begin_capture();

        // Simulate a write tool call with structured result (as write_file returns)
        out.display_tool_call(
            "write_file",
            &serde_json::json!({"path": "test.rs", "content": "fn main() {}"}),
        );
        let result = r#"{"path":"./test.rs","content":"fn main() {\n    let x = 42;\n    println!(\"Hello, world! {}\", x);\n}","lines":4,"bytes":72}"#;
        out.display_tool_result(result);

        out.end_response();
        let captured = out.end_capture();
        let output = strip_ansi(&String::from_utf8_lossy(&captured));

        // Should contain the path header with line count
        assert!(
            output.contains("write ./test.rs [4 lines]"),
            "missing path header with line count: {}",
            output
        );
        // Should contain syntax-highlighted content (first 10 lines)
        assert!(output.contains("fn main()"), "missing fn main: {}", output);
        assert!(
            output.contains("let x = 42"),
            "missing let x = 42: {}",
            output
        );
        assert!(output.contains("println!"), "missing println!: {}", output);
        // Should NOT contain line numbers (unlike read_file which has them in raw result but stripped for display)
        assert!(
            !output.contains("|"),
            "should not contain line number gutter: {}",
            output
        );
    }
}

#[cfg(test)]
mod ui_render_scope_tests {
    //! The model-facing rewrite must not change what the human sees.
    //!
    //! The failure envelope carries two views; only `ui_view` reaches the panel.
    //! This pins that a real failure renders its `ui_view` and never its
    //! `model_view` — whatever file content that now contains.

    use super::*;
    use crate::settings::default_show_reasoning_handle;

    fn render(result: &str) -> String {
        let out = ConsoleOutput::new(default_show_reasoning_handle());
        out.begin_capture();
        out.display_tool_result(result);
        let d = strip_ansi(&String::from_utf8_lossy(&out.end_capture()));
        d
    }

    /// The panel word-wraps at the terminal width, so a long `ui_view` arrives
    /// broken across lines. Collapse whitespace before asserting on content —
    /// otherwise the test measures the terminal width, not the render split.
    fn unwrapped(d: &str) -> String {
        d.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn a_failed_edit_renders_only_its_ui_view() {
        let envelope = serde_json::json!({
            "ui_view": "edit failed:\n  edit #1 oldText not found. Closest match at file lines 2-5.",
            "model_view": "1 of 1 edit(s) failed.\n\n--- edit #1 ---\n\
                Your block `ctx.txt:1` failed: @@@OLD lines not found. \
                Closest match at file lines 2-5:\n   2|     let alpha = 1;\n   3|     let beta = 2;"
        });
        let d = render(&envelope.to_string());

        let flat = unwrapped(&d);
        // The human gets the short summary...
        assert!(
            flat.contains("edit #1 oldText not found. Closest match at file lines 2-5."),
            "{d}"
        );
        // ...and none of the file content, the origin string, or the block
        // syntax the model is being taught.
        assert!(!flat.contains("let alpha = 1;"), "file content leaked: {d}");
        assert!(!flat.contains("ctx.txt:1"), "origin leaked: {d}");
        assert!(!flat.contains("@@@OLD"), "model wording leaked: {d}");
        assert!(!flat.contains("Your block"), "model wording leaked: {d}");
    }
}
