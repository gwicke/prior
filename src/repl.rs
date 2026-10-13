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
//! is composed and applied by the shared draw pipeline (`crate::render`,
//! `crate::draw`, plan `doc/ui_draw_v2.md`).

use crate::agent::SessionUsage;
use crate::app_state::AppState;
use crate::content::Content;
use crate::draw::{el_if_short, ApplyPolicy};
use crate::editor::{run_editor, EditorMode};
use crate::model_catalog;
use crate::output::Output;
use crate::render::{allocate, live_budget, window, Element, Extent, RenderAt, Rendered};
#[cfg(test)]
use crate::render::{render, Frame};
use crate::session::SessionTreeNode;
use crate::termio::{InputEvent, TermGuard, RESUMED_REANCHOR};
use crossterm::event::{self, KeyCode, KeyModifiers};
use tokio::sync::mpsc::UnboundedReceiver;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The entry DSR at [`read_input`] has run once this process.
///
/// The query's whole job is the *first* paint of a session (the cache starts
/// at row 1 while the shell that launched us may have left the prompt rows
/// down — see the comment at the call site); from the second prompt on, the
/// tracked row is measured from the emitted bytes and the query is pure
/// overhead — a 60 ms timeout against any terminal that does not answer, and a
/// re-anchor that could disturb a perfectly live frame between turns. DSR
/// belongs to exceptional events (startup, resume, resize, `$EDITOR` return,
/// error/dialog entries); this flag is what keeps it out of regular drawing
/// (ui_draw_261001).
static ENTRY_DSR_DONE: AtomicBool = AtomicBool::new(false);

/// The entry DSR: anchor the draw cache to the real cursor once, at startup,
/// **before the persistent reader starts consuming stdin** (step 2 commit B).
/// Running it here — with the reader still paused — is what guarantees the
/// terminal's reply (or a test harness's pre-written one) is not swallowed by
/// the reader's first poll. Idempotent via [`ENTRY_DSR_DONE`], so
/// `read_input`'s own entry check becomes a no-op.
pub(crate) fn run_entry_dsr(output: &dyn Output) {
    if !ENTRY_DSR_DONE.swap(true, Ordering::SeqCst) {
        let _ = output.cursor_row();
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
    /// The resolved context window for the prompt-side bar's percentage.
    pub context_limit: Option<u64>,
    /// Current model filter/sort state for the `/model` overlay.
    pub model_filter: ModelFilter,
    /// Currently-active reasoning level (for the `*` marker in `/reasoning`).
    pub reasoning_level: Option<String>,
    /// The session's name, for the prompt-side status bar. `None` when unnamed,
    /// in which case the bar falls back to the session's opening message — the
    /// mid-turn bar reads the same two fields off the session index, and the two
    /// bars must not be able to disagree about which session they are naming.
    pub session_name: Option<String>,
    /// The session's opening user message, for the same fallback.
    pub session_first_message: Option<String>,
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
    /// File size in bytes (for display and sorting in listing).
    pub file_size: u64,
    /// File modification time (for age display).
    pub file_mtime: Option<std::time::SystemTime>,
}

/// Map a repo `SessionMetadata` listing entry to the lightweight
/// [`SessionMeta`] used by the menu providers and the `/session` overlay.
pub fn session_meta_from(m: crate::session::types::SessionMetadata) -> SessionMeta {
    // Extract detail fields if present
    let (message_count, user_message_count, last_prompt_text, last_prompt_timestamp) =
        if let Some(d) = m.detail {
            (
                d.message_count,
                d.user_message_count,
                d.last_prompt_text,
                d.last_prompt_timestamp,
            )
        } else {
            (0, 0, None, None)
        };

    SessionMeta {
        id: m.id,
        name: m.name,
        timestamp: m.timestamp,
        cwd: m.cwd,
        parent_session: m.parent_session.map(|p| p.display().to_string()),
        message_count,
        user_message_count,
        first_prompt_preview: m.first_prompt_preview,
        last_prompt_text,
        last_prompt_timestamp,
        path: Some(m.path),
        file_size: m.file_size,
        file_mtime: m.file_mtime,
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
    /// same way; painting goes through `overlay_elements`.
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

// --- the input element ---------------------------------------------------
//
// The editor's own element, carrying the editor's own squeeze policy: how
// many rows of context to keep above a caret is an opinion about *editing*
// (ui_draw_v2 §3.2.2). It used to sit in the frame layer as `render`'s `Input`
// arm, where the frame layer could not help but accumulate opinions like it.

/// Rows of context preferred above the caret when an input window must clip
/// its wrapped height (plan §3.4).
const INPUT_CONTEXT_ROWS: usize = 3;

impl Element {
    /// Prompt + editor content. `layout` is the editor's visual layout
    /// (prompt rendered on row 0); the caret lives at `layout.cursor_row` /
    /// `layout.cursor_col` — derived, not stored.
    ///
    /// `must` is the caret's own row, which is why the input can never be
    /// squeezed away entirely; wrapping grows `might`.
    pub(crate) fn input(layout: Layout, prompt: String) -> Self {
        let might = layout.row_count.max(1);
        Element::new(1, might, InputItem { layout, prompt })
    }

    /// An input that occupies **no rows at all** — the mid-turn case.
    ///
    /// An input row with nothing in it reads as "type here", and while a turn is
    /// running there is nothing to submit, so the row is a claim the screen
    /// cannot back up. Between turns it is the opposite: it is what tells the
    /// reader where input goes, so [`Self::input`] keeps it. The REPL is live
    /// either way (OQ2) — the frame is composed and applied on every keystroke;
    /// it simply has no row to draw until something is typed.
    ///
    /// `must == might == 0`, so the allocator serves it nothing and
    /// `InputItem::render_at(0, _)` returns the empty `Rendered`. The choice is
    /// made here rather than inferred inside the element, because "is a turn
    /// running" is the output's fact (`streaming`), not the frame layer's.
    pub(crate) fn input_hidden(layout: Layout, prompt: String) -> Self {
        Element::new(0, 0, InputItem { layout, prompt })
    }
}

/// The input element for one compose, and the R1 rule in one place.
///
/// Mid-turn an empty input is **no row at all**; between turns it is the resting
/// row that says where to type; anything typed is an ordinary input either way.
/// [`Element::input_hidden`] has the reasoning.
///
/// Shared by the REPL's redraw and the test harness rather than written twice —
/// a copy would drift, and then the tests would pin a rule that production does
/// not follow.
pub(crate) fn input_element(
    editor: &LineEditor,
    prompt: &str,
    streaming: bool,
    term_w: usize,
) -> Element {
    let layout = editor.layout(term_w, prompt);
    if streaming && editor.text().is_empty() {
        Element::input_hidden(layout, prompt.to_string())
    } else {
        Element::input(layout, prompt.to_string())
    }
}

/// Overlay elements for a plain list of rows — for tests that care about the
/// frame, not about a menu's sections. See [`test_overlay_parts`].
#[cfg(test)]
pub(crate) fn test_overlay(lines: &[&str], sel: usize, term_w: usize) -> Vec<Element> {
    test_overlay_parts(None, &[], lines, Some(sel), term_w)
}

/// Overlay elements from a plan's three sections, for tests that want a header
/// or detail as well as rows.
///
/// Goes through a real [`RenderPlan`] and the real [`overlay_elements`], so a
/// test that pastes in arbitrary strings is still exercising production's
/// composition rather than a shortcut that can drift from it.
#[cfg(test)]
pub(crate) fn test_overlay_parts(
    header: Option<&str>,
    detail: &[&str],
    rows: &[&str],
    sel: Option<usize>,
    term_w: usize,
) -> Vec<Element> {
    use crate::slashdot_menu::render::{LogicalLine, RenderPlan};
    let plan = RenderPlan {
        table_header: header.map(LogicalLine::new),
        detail: detail.iter().map(|l| LogicalLine::new(*l)).collect(),
        rows: std::sync::Arc::new(rows.iter().map(|l| LogicalLine::new(*l)).collect()),
        selected: sel,
        active_rows: vec![],
    };
    crate::ui::overlay_elements(&plan, term_w, sel)
}

/// The rows element's allocation in a prompt frame — the page size a PageUp or
/// PageDown moves by.
///
/// The page path needs the answer *before* the frame is rendered, which is why
/// it used to call the painter a second time and read `data_budget` off the
/// result: the painter's own guess at the budget, which is a *different number*
/// from the one `allocate` hands the rows element. That disagreement is what
/// let a page move by more than one screen.
///
/// This runs the same composition and the same `allocate` the frame will, so
/// the two cannot differ. The status row is left out — it is one row, and a
/// menu is only open between turns, when there is nothing streaming to report.
pub(crate) fn rows_grant(
    editor: &LineEditor,
    prompt: &str,
    overlay: Vec<Element>,
    term_w: usize,
    term_h: usize,
) -> usize {
    let elements = Repl::compose_elements(editor, prompt, None, overlay, term_w);
    let extents: Vec<Extent> = elements.iter().map(Element::extent).collect();
    let heights = allocate(&extents, live_budget(term_h.max(1)));
    // The rows element is the last of the overlay's sections by construction
    // (`overlay_elements` appends detail, header, then rows).
    heights.last().copied().unwrap_or(0)
}

/// The overlay's frame elements: no overlay ⇒ none.
///
/// The overlay is *not* painted here. Its sections become elements and the
/// frame allocates them, so there is no painted row list to hand on and no
/// focus index to translate — the cursor's logical row index goes straight to
/// the rows element, which is the only thing that scrolls.
pub(crate) fn overlay_elements(overlay: &ActiveOverlay, term_w: usize) -> Vec<Element> {
    match overlay {
        ActiveOverlay::None => Vec::new(),
        ActiveOverlay::V25Menu { plan } | ActiveOverlay::PathComplete { plan } => {
            crate::ui::overlay_elements(plan, term_w, plan.selected)
        }
    }
}

/// The area below the status row: `[Input?, Divider?, Overlay?]`.
///
/// The *rule*, stated once, for the two callers that must not be able to
/// disagree about it: [`compose_area`] (production) and
/// [`Repl::compose_elements`] (the test/grant twin, whose status arrives as a
/// literal string). The only thing the two genuinely differ on is where the
/// status row *came from*; how the rest of the area is laid out is this
/// function's and only this function's (ui_draw_v2 §5.1.1).
///
/// `overlay` is empty when there is no menu: a separator with nothing below it
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

/// The area's composition: `[Status?, Blank, Input, Divider?, Overlay…]` —
/// the compose head, from content's own handles (step 4f-b). The head moved
/// out of `Output` once the bar became self-formatting (4f-a): presence is
/// shaped here (an off bar reserves no row) and the row's *content* is
/// formatted at render from the same raw store, so neither the shape nor the
/// bytes are anyone's to carry through compose.
///
/// The status row takes the **divider's row**, above the input — which is
/// what makes it permanent and what makes it safe. Being in the frame is
/// the whole trick: a bar written as content would need a relative erase at
/// submit, and by then the cursor has moved into the frame region below it,
/// so the erase would land on the wrong rows. Here the frame's own union
/// clear takes it, at the one moment the cursor is where the frame says it
/// is — and the partial row-diff redraws it per keystroke, so the figures
/// are never stale.
///
/// Presence is decided here and the bar's render does not, because presence
/// is the part that does not move under it: every compose happens after the
/// cycle's `set_status`, so the flag cannot flip between compose and draw.
/// What the row says is `Element::status`'s own business, resolved when it
/// is rendered — and at the width it is rendered at. The head's *final* home
/// is `render()` itself, with the bar an installed element (**4h**).
pub(crate) fn compose_area(
    content: &Content,
    input: Option<Element>,
    overlay: Vec<Element>,
) -> Vec<Element> {
    let mut items = Vec::new();
    if !crate::agent::status::status_text(&content.bar.lock().unwrap()).is_off() {
        // The row between the last content and the bar is the **frame's**,
        // not the content path's: synthetic content no longer emits a
        // trailing separator, so without this there would be nothing
        // between what was written and the status line. Static and part of
        // the area, so it retires with it — a content write that consumes
        // the frame takes this row too, and it can never become a
        // permanent blank in the transcript.
        items.push(Element::lines(vec![String::new()]));
        items.push(Element::status(
            content.bar.clone(),
            content.streaming.clone(),
        ));
    }
    items.extend(area_body(input, overlay));
    items
}

/// Compose and apply the prompt frame: `[Input]`, or
/// `[Input, Divider, Overlay]` while a pop-up shows (plan §5.2).
///
/// **The one way the prompt writes to the terminal.** `read_input`'s blocking
/// loop and the turn's `select!` key branch both come here (ui_draw_v2 §4.5
/// U4), so neither pump can grow its own idea of what the input row looks
/// like — the split that made `handle_key` and `eval` draw nothing only holds
/// if the pixels they are silent about have exactly one owner.
///
/// The apply phase owns the rest: an intact live frame means a partial row-diff
/// (so a tall pop-up's scroll is followed without a row counter), and the
/// scroll `delta` and bounded clear live in `plan_apply`. Content *above* the
/// frame — greeter, status, session — is never touched.
///
/// Takes content, not `&dyn Output` (step 4f-b): the compose head reads the
/// bar's raw store and the streaming flag from it, and the render goes
/// through the manager content holds down to (design §Content).
pub(crate) fn paint_prompt(
    content: &Content,
    label: &str,
    editor: &LineEditor,
    overlay: Vec<Element>,
    dims: (usize, usize),
) {
    let dims = (dims.0.max(1), dims.1.max(1));
    // The REPL supplies only what changes: the input, and the menu when one is
    // open. The status row is `compose_area`'s to resolve — it is made of
    // content's own facts and not something this module may reason about —
    // and so is the divider that separates the menu from the input above it
    // (ui_draw_v2 §5.1.1).
    let input = input_element(
        editor,
        label,
        content.streaming.load(std::sync::atomic::Ordering::Relaxed),
        dims.0,
    );
    // Composition *and* division are the tail's now: pieces move in and nothing
    // comes back, so no `Vec<Element>` crosses a call site (ui_draw_v2 §5.1.1).
    let stack = compose_area(content, Some(input), overlay);
    content
        .manager
        .lock()
        .unwrap()
        .render(stack, ApplyPolicy::Frame, dims);
}

/// Editor content: the prompt on row 0, the wrapped buffer below it.
struct InputItem {
    layout: Layout,
    prompt: String,
}

impl RenderAt for InputItem {
    fn render_at(&self, h: usize, tw: usize) -> Rendered {
        if h == 0 {
            return Rendered::default();
        }
        let layout = &self.layout;
        let n = layout.row_count.max(1);
        // Three rows above the caret: enough to see the argument you just
        // edited without losing the caret off the top. This constant is
        // *why* the impl lives here rather than in the frame layer.
        let start = window(n, layout.cursor_row, h, INPUT_CONTEXT_ROWS);
        let count = h.min(n.saturating_sub(start));
        let mut rows = Vec::with_capacity(count);
        for r in start..start + count {
            // The prompt renders on row 0; later rows are content-only.
            let body: &str = if r == 0 { &self.prompt } else { "" };
            let text = layout.rows.get(r).map(String::as_str).unwrap_or("");
            let mut content = String::with_capacity(body.len() + text.len() + 3);
            content.push_str(body);
            content.push_str(text);
            el_if_short(&mut content, tw);
            rows.push(content);
        }
        let caret = (layout.cursor_row >= start && layout.cursor_row < start + count)
            .then_some((layout.cursor_row - start, layout.cursor_col));
        Rendered { rows, caret }
    }
}

/// What one key did to the prompt session.
///
/// [`PromptSession::handle_key`] mutates the session and answers with one of
/// these; **it never draws.** Keeping the drawing out of it is the whole point
/// of the split: the same dispatcher has to serve two pumps — `read_input`'s
/// blocking loop today, and the turn's `tokio::select!` branch once in-turn
/// input lands (ui_draw_v2 §4.5 U4) — and a dispatcher that painted would give
/// them two ways to write the terminal.
pub(crate) enum KeyOutcome {
    /// Nothing that leaves the wait loop: repaint and read the next key.
    Continue,
    /// Hand the tty to `$EDITOR` over the input buffer. The takeover (reader
    /// gate, raw mode, DSR re-anchor) belongs to the caller — see
    /// `editor::run_editor`, which owns that whole sequence.
    RunEditor,
    /// Leave `read_input` with this action. The caller draws first for a
    /// `Submit` (the input region is cleared before the transcript gets it).
    Action(InputAction),
}

/// What [`PromptSession::eval`] asks the caller to do once the state is settled.
///
/// `eval` resolves the pending key into an overlay; it does **not** write to
/// the terminal. Two branches used to write from inside the engine call —
/// they now hand back the bytes and the shape of the write instead, so the
/// same eval can drive `read_input`'s blocking loop *and* the turn's
/// `select!` branch (ui_draw_v2 §4.5 U4) without either growing a second
/// copy of the sequence.
///
/// It is the same split `KeyOutcome` makes for keys: state here, pixels at
/// the call site. One dispatcher, two pumps — and now one evaluator too.
#[derive(Debug)]
pub(crate) enum EvalOutcome {
    /// Nothing terminal. `overlay` holds the frame: sync the selection and
    /// paint.
    Frame,
    /// A menu commit landed on `AppState`. The caller writes it —
    /// [`Repl::emit_commit`], then the engine's optional message — and
    /// returns [`InputAction::MenuDone`], whose convergence step applies the
    /// targets this eval wrote. `emit_commit` is what clears the buffer.
    Commit {
        /// The self-documenting prompt as committed, e.g. `/reasoning low`.
        line: String,
        /// The node's message, when it had one.
        message: Option<String>,
    },
    /// A non-Enter key ended a rewrite chain. The caller clears the input
    /// region, surfaces `message` in the transcript and re-anchors (DSR).
    /// `eval` has already emptied the buffer and left the `/`-session.
    Error { message: String },
}

/// Everything that survives between one keystroke and the next.
///
/// This used to be six `let mut`s at the top of `Repl::read_input`, which
/// meant the prompt session could not outlive that function's frame — the
/// blocking loop, the drawing and the state were one thing. Splitting it out
/// is what lets the state reach the turn's `select!` (ui_draw_v2 §4.5 U4).
///
/// The `SlashDotMenu` lives here too, and is *not* a borrow: the menu handle
/// borrows it, so storing both would be self-referential. Instead the session
/// tracks `epoch_active` and mints a handle for the duration of each eval —
/// [`SlashDotMenu::get_handle`] when a `/`-session begins (bumping the epoch so
/// a new invocation starts clean), [`SlashDotMenu::continuation_handle`]
/// afterwards (so the in-progress one keeps its view restore and armed
/// confirm).
pub struct PromptSession {
    /// The input buffer, cursor, wrap layout, and the `Up`/`Down` history-recall
    /// position (which lives on the editor — see [`LineEditor::nav`]).
    editor: LineEditor,
    /// Selected row of the current overlay.
    sel: usize,
    /// Overlay discriminant, for resetting `sel` when the overlay *kind*
    /// changes rather than its contents.
    last_disc: u8,
    /// Prose (non-`/`) completion. Spawns its FS worker; the worker exits when
    /// this drops.
    prose: crate::prompt_complete::PromptCompletion,
    /// Engine key queued for the *next* eval (eval → display → wait shape).
    /// Consumed unconditionally at the top of the loop, so a stale key cannot
    /// outlive one iteration.
    pending_key: Option<crate::slashdot_menu::HostKey>,
    /// The v2.5 menu tree. Persistent for the session.
    menu: crate::slashdot_menu::menu::SlashDotMenu,
    /// Whether a `/`-session is in progress — i.e. whether the epoch has been
    /// bumped for the invocation currently on screen.
    ///
    /// Named for the mechanism, not the buffer: this is *not* "the prompt
    /// starts with `/`" (the buffer says that) but "an epoch has been bumped
    /// for the invocation on screen". It is what used to be read off
    /// `v25_handle.is_none()`, kept as a `bool` because the handle itself
    /// cannot be stored alongside the menu it borrows.
    epoch_active: bool,
    /// The overlay the last eval produced, which the next key dispatches
    /// against. Recomputed every iteration; held here so `handle_key` sees the
    /// same frame that was just painted.
    overlay: ActiveOverlay,
    /// A gated tool call's confirmation, if one is on screen.
    ///
    /// Not an `ActiveOverlay` variant and not `None`-when-absent by design: a
    /// confirm is **modal over** whatever menu is open (it appends below it), so
    /// it is a separate layer rather than a replacement — clearing the overlay
    /// to make room would destroy the prompt the menu is driven from
    /// (doc/confirm_as_menu_261002.md §3).
    ///
    /// Only ever `Some` during a turn: the port is installed by
    /// `run_interactive` and its receiver is borrowed per turn, so nothing can
    /// open a confirm at the prompt. A dropped `ConfirmPrompt` denies — see
    /// its reply channel.
    confirm: Option<crate::confirm::ConfirmPrompt>,
}

/// Everything the turn needs to put a key back into the prompt
/// (ui_draw_v2 §4.5 U4).
///
/// Bundled so `prompt()` gains one parameter rather than five, and because the
/// pieces are coupled: they exist only where there *is* an interactive prompt.
/// Piped/batch input builds no session and spawns no in-flight reader, so it
/// has nothing to hand over — hence the caller wraps this in an `Option`.
///
/// It is the same set `read_input` receives, arrived at differently: there the
/// pieces are arguments, here they are a value carried alongside the turn. The
/// turn is the second pump, and the second pump gets the same inputs.
pub struct InTurnInput<'a> {
    /// The session keys land in. Its `handle_key` → [`settle`](PromptSession::settle)
    /// ordering is `read_input`'s loop ordering, minus the wait between them.
    pub session: &'a mut PromptSession,
    /// The in-flight reader's channel. Unbounded because a bounded one that
    /// filled would block the reader at the moment it most needs to deliver
    /// `Ctrl-C` (see `route_event`).
    pub keys: &'a mut tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    /// Confirmation requests from gated tool calls, answered by the session's
    /// own dialog (doc/confirm_as_menu_261002.md). Borrowed per turn for the
    /// same reason `keys` is: a request must not outlive the pump that would
    /// answer it, and the reply sender failing is a `Deny`.
    pub confirms: &'a mut tokio::sync::mpsc::UnboundedReceiver<crate::confirm::ConfirmRequest>,
    /// What the input row says — `read_input`'s `prompt` argument.
    pub label: &'a str,
    /// History the Up/Down keys page through.
    pub history: &'a mut History,
    /// For [`open_editor`](PromptSession::open_editor) when `Ctrl-E` lands
    /// mid-turn. There is no reader to stop in that case — the turn's own is
    /// what handed us the key — but the raw-mode turnover and the DSR
    /// re-anchor are still the wrapper's to do.
    pub guard: &'a TermGuard,
}

impl PromptSession {
    /// Fresh session, seeded with `initial_buffer` if the caller has one
    /// (the `/session <id> resume` prefill, and any other pop that wants the
    /// editor to start with text already in it).
    pub(crate) fn new(initial_buffer: Option<&str>, app_state: &Arc<AppState>) -> Self {
        let mut editor = LineEditor::new();
        if let Some(s) = initial_buffer {
            editor.set(s);
        }
        Self {
            editor,
            sel: 0,
            last_disc: 0,
            prose: crate::prompt_complete::PromptCompletion::new(),
            pending_key: None,
            menu: crate::v25_menu::build_menu(app_state),
            epoch_active: false,
            overlay: ActiveOverlay::None,
            confirm: None,
        }
    }

    /// Open a gated tool call's confirmation.
    ///
    /// Replaces any confirm already on screen rather than queueing: rig
    /// dispatches tool calls through `buffer_unordered(concurrency)` and the
    /// default concurrency is 1, so two at once needs `tool_concurrency` set
    /// explicitly. When it is, the second request waits in the channel and is
    /// picked up as soon as the first resolves — still one dialog, still one
    /// modal layer.
    pub(crate) fn open_confirm(
        &mut self,
        req: crate::confirm::ConfirmRequest,
        approvals: crate::confirm::Approvals,
    ) {
        self.confirm = Some(crate::confirm::ConfirmPrompt::new(req, approvals));
    }

    /// Is a confirmation on screen? The dispatcher asks *before* the `Esc`
    /// ladder, so `Esc` dismisses the dialog rather than stepping the menu or
    /// cancelling the turn.
    pub(crate) fn confirm_open(&self) -> bool {
        self.confirm.is_some()
    }

    /// Feed one key to an open confirmation, answering with the decision when
    /// the dialog resolved.
    ///
    /// The caller then paints the area back and writes the decision line — the
    /// order `emit_commit` uses, and the reason there is no second erase:
    /// dropping the element *is* the erase.
    pub(crate) fn confirm_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Option<crate::confirm::ConfirmDecision> {
        self.confirm.as_mut()?.handle_key(key)
    }

    /// The subject of the open confirmation, for its decision line.
    pub(crate) fn confirm_subject(&self) -> Option<String> {
        self.confirm.as_ref().map(|c| c.subject().to_string())
    }

    /// Drop a resolved confirmation. The caller paints afterwards, which is
    /// what erases its rows.
    pub(crate) fn clear_confirm(&mut self) {
        self.confirm = None;
    }

    /// Seed the editor with a prefill.
    ///
    /// *Replaces* the buffer, because a prefill (`/session <id> resume` and
    /// anything else that calls `set_prefill`) is an instruction for this
    /// particular prompt — it is the one thing allowed to override what the
    /// carried buffer holds. When there is no prefill, the session keeps what
    /// it had: the `/` prompt *is* the menu state, so the buffer that ends a
    /// turn is the buffer the next prompt opens with.
    pub(crate) fn apply_prefill(&mut self, text: &str) {
        self.editor.set(text);
    }

    /// The handle to eval with, for the `/`-session the buffer is currently in.
    ///
    /// Bumps the epoch only on the way *into* `/`, so one invocation's
    /// traversal memory survives every key inside it (see the struct doc).
    ///
    /// Takes the two fields rather than `&mut self` because [`eval`](Self::eval)
    /// has the session destructured by the time it needs the handle, and a
    /// borrow of the whole session would not survive alongside them. Passing
    /// the fields keeps those borrows disjoint.
    ///
    /// TODO(api): see the note on [`SlashDotMenu::continuation_handle`] — this
    /// helper, and `epoch_active` below, exist only because the host has to
    /// choose between two constructors. A held invocation would delete both.
    fn slash_handle<'m>(
        menu: &'m mut crate::slashdot_menu::menu::SlashDotMenu,
        epoch_active: &mut bool,
    ) -> crate::slashdot_menu::menu::SlashDotHandle<'m> {
        let fresh = !*epoch_active;
        *epoch_active = true;
        if fresh {
            menu.get_handle()
        } else {
            menu.continuation_handle()
        }
    }

    /// Resolve the pending key (or a passive frame) into an overlay.
    ///
    /// The one place an engine prompt is applied to the buffer *and* its plan
    /// rendered, so a §5.1 keyless restore lands both in the same frame — no
    /// window for a racing edit between them. Takes `pending_key` unconditionally,
    /// so a queued key cannot outlive one eval.
    ///
    /// Writes nothing itself: what must reach the terminal comes back as an
    /// [`EvalOutcome`], and the frame it produced is in `overlay`.
    ///
    /// Takes `&mut self` for the reason [`handle_key`](Self::handle_key) does:
    /// both pumps drive one session by reference, and neither may see it split
    /// apart.
    pub(crate) async fn eval(&mut self) -> EvalOutcome {
        let Self {
            editor,
            pending_key,
            menu,
            epoch_active,
            prose,
            overlay,
            ..
        } = self;
        let key = pending_key
            .take()
            .unwrap_or(crate::slashdot_menu::HostKey::None);
        let buf_text = editor.text();
        *overlay = if buf_text.starts_with('/') {
            // All `/` commands route through the v2.5 engine. The handle is
            // minted for the eval and dropped with it — it carries no state
            // beyond the borrowed root and the epoch, both of which live in
            // the session's menu (`slash_handle` decides which epoch).
            let cursor = editor.cursor;
            let result = {
                let mut handle = Self::slash_handle(menu, epoch_active);
                Repl::v25_eval(&mut handle, &buf_text, cursor, key).await
            };
            match result {
                V25Frame::Continue(plan, prompt) => {
                    // Apply the engine's prompt — but only when it actually
                    // changed: an unchanged passive eval must not reset a
                    // mid-line text cursor (`LineEditor::set` jumps to EOL).
                    //
                    // `set_derived`, not `set`: the text here is re-derived from
                    // the buffer already on screen, not typed by the user, so it
                    // must not look like an edit. That matters most for recall —
                    // history holds `/`-commands, so recalling `/model` opens the
                    // menu and expands the line to `/model `, and a plain `set`
                    // would end the walk on that rewrite, leaving the user stuck
                    // in the recalled prompt with `Up`/`Down` refusing to
                    // browse. Derived writes keep them walking.
                    let new_buf = format!("/{}", prompt);
                    if new_buf != editor.text() {
                        editor.set_derived(&new_buf);
                    }
                    ActiveOverlay::V25Menu { plan }
                }
                V25Frame::DoneSuccess(msg, prompt)
                    if matches!(key, crate::slashdot_menu::HostKey::Enter) =>
                {
                    // The node's submit() wrote to `AppState` (target config
                    // for `/reasoning`/`/model`/…, control fields for
                    // `/session`/`/replay`/…). Use the engine-provided
                    // self-documenting prompt (e.g. "/reasoning low").
                    return EvalOutcome::Commit {
                        line: format!("/{}", prompt),
                        message: msg,
                    };
                }
                V25Frame::DoneError(err, prompt)
                    if matches!(key, crate::slashdot_menu::HostKey::Enter) =>
                {
                    return EvalOutcome::Commit {
                        line: format!("/{}", prompt),
                        message: Some(err),
                    };
                }
                // Rewrite-cycle bail-out on a non-Enter key (chain already on
                // stderr from `v25_eval`): the caller clears the input area,
                // surfaces it in the transcript and re-anchors; we drop the
                // prompt — back to normal operation. The buffer and the
                // `/`-session are cleared here rather than by the caller,
                // because they are state, not pixels.
                V25Frame::DoneError(err, _) => {
                    editor.set("");
                    *epoch_active = false;
                    *overlay = ActiveOverlay::None;
                    return EvalOutcome::Error { message: err };
                }
                // Esc at root — close the overlay.
                V25Frame::DoneSuccess(_, _) => {
                    editor.set("");
                    *epoch_active = false;
                    ActiveOverlay::None
                }
            }
        } else {
            // Leave the `/`-session: the next `/` starts a fresh invocation
            // (a fresh epoch), so no traversal memory carries across.
            *epoch_active = false;
            // Prose frames: one eval drives the word gate, the debounce, and
            // the dialog. Idle ⇒ no overlay (stateless dismissal: the next
            // frame re-derives from the buffer).
            let ctx = crate::slashdot_menu::RenderContext {
                term_width: crate::output::term_width().max(1),
            };
            match prose.eval(editor, key, ctx) {
                crate::prompt_complete::Frame::Idle => ActiveOverlay::None,
                crate::prompt_complete::Frame::Dialog(plan) => ActiveOverlay::PathComplete { plan },
            }
        };
        EvalOutcome::Frame
    }

    /// Align the selection to the overlay [`eval`](Self::eval) just produced.
    ///
    /// Hoisted for the same reason `eval` was: the turn's key branch must do
    /// this too, and two copies would let the cursor and the paint disagree
    /// about which row is selected. It no longer hands its rows back —
    /// [`settle`](Self::settle) reads them off the overlay for the painter, and
    /// a returned copy would be a second thing to keep in step with the first.
    pub(crate) fn sync_selection(&mut self) {
        let Self {
            overlay,
            sel,
            last_disc,
            ..
        } = self;
        let (raw_opts, active) = overlay.display_rows();
        let cur_disc = overlay.overlay_disc();
        if overlay.cursor_row().is_some() {
            // Engine-owned cursor: `sel` IS the engine's cursor row (arrows
            // mutate engine state; Tab/Enter act on it). The marker `active`
            // (ctx-driven `*`) is separate. No host-side cursor policy for
            // these overlays.
            *sel = overlay
                .cursor_row()
                .unwrap_or(active.unwrap_or(0))
                .min(raw_opts.len().saturating_sub(1));
        } else {
            // Host-side policy: a *changed* overlay starts back at the top.
            *sel = Repl::cursor_sel_policy(cur_disc != *last_disc, *sel);
        }
        if cur_disc != *last_disc {
            *last_disc = cur_disc;
        }
        let max_sel = raw_opts.len().saturating_sub(1);
        if *sel > max_sel {
            *sel = max_sel;
        }
    }

    /// The overlay's row count at this instant — the bounds `handle_key`'s
    /// host-side cursor needs.
    ///
    /// `settle` renders those rows for the painter instead; this is for a
    /// dispatcher that needs the number and nothing else. Both read `overlay`,
    /// and `overlay` is only ever written by [`eval`](Self::eval), so neither
    /// can be out of step with the other.
    pub(crate) fn opts_len(&self) -> usize {
        self.overlay.display_rows().0.len()
    }

    /// Is a v2.5 menu open — the case where `Esc` steps the engine back a
    /// level instead of ending the turn (settled U4: menu-first, abort only at
    /// root).
    ///
    /// Deliberately `V25Menu` and not "any overlay": prose's dialog is
    /// dismissed by the buffer, not by `Esc` — [`handle_key`](Self::handle_key)
    /// says so in its `Esc` arm — so there is nothing there for `Esc` to step
    /// out of. A bare prompt is the same situation. Both count as root, and
    /// root is where the turn stops.
    pub(crate) fn menu_open(&self) -> bool {
        matches!(self.overlay, ActiveOverlay::V25Menu { .. })
    }

    /// How long the turn's `select!` should wait before re-evaluating prose —
    /// `None` when nothing is pending, and there is then nothing to wake for.
    ///
    /// Without this arm the turn's key branch would be the only one with a
    /// deadline, and prose would go inert mid-turn: its debounce and in-flight
    /// slices need a timeout of their own to fire while the stream runs.
    ///
    /// See [`crate::prompt_complete::PromptCompletion::poll_timeout`].
    pub(crate) fn prose_timeout(&self) -> Option<Duration> {
        self.prose.poll_timeout()
    }

    /// Hand the tty to `$EDITOR` over the buffer and write the result back.
    ///
    /// Shared for the reason [`settle`](Self::settle) is: `read_input`'s
    /// [`KeyOutcome::RunEditor`] arm and the turn's would otherwise each carry
    /// the raw-mode turnover and the DSR re-anchor — and a failure that
    /// surfaces differently depending on which pump took the key is a failure
    /// the user sees one way out of two.
    pub(crate) fn open_editor(&mut self, guard: &TermGuard, output: &dyn Output) {
        match run_editor(guard, output, &self.editor.text(), EditorMode::Edit) {
            Ok(new_text) => self.editor.set(&new_text),
            Err(e) => {
                let _ = io::stdout().write_all(format!("\r\neditor error: {}\r\n", e).as_bytes());
            }
        }
    }

    /// Insert pasted text at the caret. Shared so "paste" is one operation
    /// rather than one each pump reaches for differently.
    pub(crate) fn paste(&mut self, text: &str) {
        self.editor.insert_str(text);
    }

    /// eval → outcome → sync → paint: the whole "make the session say
    /// something true, and say it out loud" half of one pump iteration.
    ///
    /// **Both pumps call this.** `read_input`'s loop and the turn's `select!`
    /// branch (ui_draw_v2 §4.5 U4) share it rather than each carrying a copy,
    /// and the reason is sharpest in the arms: a `/`-command committed
    /// mid-turn has to land exactly where it lands at the prompt, and an eval
    /// that bails out of the rewrite chain has to look the same there. Two
    /// versions would be two ways to see a different result from one keystroke.
    ///
    /// Returns `Some(action)` when the read ends — only `MenuDone`, from a
    /// committed `/`-command. `None` means "keep looping": by then the frame
    /// is already painted.
    pub(crate) async fn settle(
        &mut self,
        content: &Content,
        prompt: &str,
        history: &mut History,
        dims: (usize, usize),
    ) -> Option<InputAction> {
        // `emit_commit` wants a painter as a closure; the painter it wants is
        // the same one everything else uses, so this is an adapter over
        // `paint_prompt` and not a second composition.
        let redraw = |editor: &LineEditor, overlay: Vec<Element>, dims: (usize, usize)| {
            paint_prompt(content, prompt, editor, overlay, dims);
        };
        // Bound before the `match` on purpose: a future in a match scrutinee
        // lives to the end of the `match`, and it would still hold the borrow
        // of `self` the arms below want back.
        let eval = self.eval().await;
        match eval {
            EvalOutcome::Frame => {}
            EvalOutcome::Commit { line, message } => {
                Repl::emit_commit(content, &mut self.editor, &redraw, dims, &line, "", history);
                if let Some(text) = message {
                    content.display_text(&text);
                }
                // Dismiss the overlay and hand control back to the agent loop,
                // whose convergence step applies the targets this eval wrote.
                return Some(InputAction::MenuDone);
            }
            EvalOutcome::Error { message } => {
                // Clear the input region through the frame path — the menu
                // that produced the error is not what should keep showing
                // under its message — surface it, then re-anchor (DSR) so the
                // next iteration anchors below it.
                //
                // The DSR used to be justified by "the exact line
                // `display_text` consumed depends on inline-markdown wrapping,
                // so re-anchor rather than trusting the wrap count". That is
                // no longer true: the count is *measured* from the emitted
                // bytes, and `display_text_row_is_measured_not_predicted` pins
                // it against a terminal emulator. The DSR stays because this is
                // an error path — rare, and the cost of being wrong is a
                // stranded input region — not because the accounting needs
                // rescuing.
                redraw(&LineEditor::new(), Vec::new(), dims);
                content.display_text(&message);
                let _ = content.manager.lock().unwrap().cursor_row();
            }
        }
        self.sync_selection();
        self.paint(content, prompt, dims);
        None
    }

    /// The session's frame, as one composition: the open overlay and then — if a
    /// gated tool is waiting — the confirmation below it.
    ///
    /// The single place the area is composed from session state, so `settle`
    /// and the turn's confirm arm cannot disagree about what the screen shows.
    /// A confirm *appends* rather than replaces (§3 of
    /// doc/confirm_as_menu_261002.md): it is modal over the menu, and the
    /// prompt that drives that menu stays exactly where it was.
    pub(crate) fn paint(&self, content: &Content, prompt: &str, dims: (usize, usize)) {
        let mut overlay = overlay_elements(&self.overlay, dims.0);
        if let Some(confirm) = &self.confirm {
            overlay.extend(confirm.elements(dims.0));
        }
        paint_prompt(content, prompt, &self.editor, overlay, dims);
    }

    /// Apply one key to the session.
    ///
    /// **Draws nothing.** Every branch either mutates state and asks for
    /// another turn ([`KeyOutcome::Continue`]), hands the tty to `$EDITOR`
    /// ([`KeyOutcome::RunEditor`]), or ends the read
    /// ([`KeyOutcome::Action`]) — what that looks like on screen is the
    /// caller's decision. Two pumps drive this: `read_input`'s blocking loop
    /// today, the turn's `tokio::select!` branch once in-turn input lands
    /// (ui_draw_v2 §4.5 U4). A dispatcher that painted would hand them two
    /// ways to write the terminal.
    ///
    /// Takes `&mut self` because both pumps hold the session by reference:
    /// `read_input`'s loop, and the turn's `select!` branch (ui_draw_v2 §4.5
    /// U4). Everything that is plain text editing still goes to
    /// [`Repl::edit_key`].
    ///
    /// `Ctrl-C` and `Ctrl-Z` are *not* here: they are process and terminal
    /// operations that must not queue behind edit keys, so the caller handles
    /// them before this runs.
    pub(crate) fn handle_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        // Row count of the overlay as the caller measured it — the bounds the
        // host-side cursor needs. Passed rather than recomputed so the
        // dispatch and the paint cannot disagree about how many rows there are.
        opts_len: usize,
        prompt: &str,
        history: &mut History,
    ) -> KeyOutcome {
        let Self {
            editor,
            sel,
            pending_key,
            ..
        } = self;
        // Read, never written: bound immutably so the arms see the
        // `&ActiveOverlay` they were written against rather than a `&mut` that
        // would re-bind every `plan` in them.
        let overlay: &ActiveOverlay = &self.overlay;
        let epoch_active: bool = self.epoch_active;
        // Ctrl-E: external editor for input buffer (no overlay) or header (V25Menu).
        if let KeyCode::Char('e') = key.code {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match overlay {
                    ActiveOverlay::None => {
                        // Edit the input buffer. The wrapper owns the whole
                        // turnover — reader gate, raw mode, DSR re-anchor; no
                        // local logic here (doc/editor_cleanup.md §2).
                        return KeyOutcome::RunEditor;
                    }
                    ActiveOverlay::V25Menu { .. } => {
                        // Ctrl-E on a menu endpoint dispatches to the node's
                        // edit() through the engine (doc/editor_cleanup.md
                        // §3/§5): the node owns what (if anything) to edit.
                        *pending_key = Some(crate::slashdot_menu::HostKey::Edit);
                        return KeyOutcome::Continue;
                    }
                    ActiveOverlay::PathComplete { .. } => {
                        // No-op in prose completion dialog.
                    }
                }
            }
        }
        // Active overlay key dispatch via enum methods.
        if !matches!(overlay, ActiveOverlay::None) {
            match key.code {
                KeyCode::Down | KeyCode::Up => {
                    // Engine-owned cursor (slash handle or prose completer):
                    // queue the delta for the next eval — it applies the
                    // result in one place. The host-`sel` fallback below is
                    // for the empty-plan V25Menu case only.
                    if overlay.cursor_row().is_some() {
                        let delta: i32 = if key.code == KeyCode::Down { 1 } else { -1 };
                        *pending_key = Some(crate::slashdot_menu::HostKey::MoveCursor(delta));
                        return KeyOutcome::Continue;
                    }
                    if key.code == KeyCode::Down {
                        if *sel + 1 < opts_len {
                            *sel += 1;
                        }
                    } else if *sel > 0 {
                        *sel -= 1;
                    }
                }
                KeyCode::PageDown | KeyCode::PageUp => {
                    // Plan-carrying overlays: visual-line-aware page scroll
                    // through the engine that owns the cursor (slash handle /
                    // prose).
                    let plan = match overlay {
                        ActiveOverlay::V25Menu { plan } | ActiveOverlay::PathComplete { plan } => {
                            Some(plan)
                        }
                        ActiveOverlay::None => None,
                    };
                    if let Some(plan) = plan {
                        if !matches!(overlay, ActiveOverlay::PathComplete { .. }) && !epoch_active {
                            return KeyOutcome::Continue; // V25 overlay without its handle
                        }
                        let cur_sel = plan.selected.unwrap_or(0);
                        // Page by what actually fits on screen: the rows
                        // element's own allocation, from the same
                        // composition and the same `allocate` the frame will
                        // run. Measured on the rows as the engine wrote them,
                        // which is the unit the rows element windows in.
                        let cur_h = crate::termio::size().height;
                        let term_w = crate::output::term_width().max(1);
                        let page = rows_grant(
                            &self.editor,
                            prompt,
                            overlay_elements(overlay, term_w),
                            term_w,
                            cur_h.max(1),
                        );
                        let direction = if matches!(key.code, KeyCode::PageDown) {
                            1
                        } else {
                            -1
                        };
                        let delta = crate::slashdot_menu::render::page_delta(
                            &plan.rows, cur_sel, page, direction,
                        );
                        if delta != 0 {
                            *pending_key = Some(crate::slashdot_menu::HostKey::MoveCursor(delta));
                        }
                        return KeyOutcome::Continue;
                    }
                    let page = crate::termio::size().height.saturating_sub(2).max(1);
                    let max = opts_len.saturating_sub(1);
                    if matches!(key.code, KeyCode::PageDown) {
                        *sel = (*sel + page).min(max);
                    } else {
                        *sel = (*sel).saturating_sub(page);
                    }
                }
                KeyCode::Tab => {
                    // Both plan-carrying overlays route Tab through the engine
                    // that produced the plan: slash drills/expands, prose
                    // splices the selected id over the word.
                    if matches!(
                        overlay,
                        ActiveOverlay::V25Menu { .. } | ActiveOverlay::PathComplete { .. }
                    ) {
                        *pending_key = Some(crate::slashdot_menu::HostKey::Tab);
                    }
                }
                KeyCode::Enter => {
                    if matches!(overlay, ActiveOverlay::V25Menu { .. }) {
                        if !key.modifiers.contains(KeyModifiers::SHIFT) {
                            // Slash: queue the key; the next iteration's eval
                            // handles submit (commit + return) or rewrite
                            // inline.
                            *pending_key = Some(crate::slashdot_menu::HostKey::Enter);
                        }
                        return KeyOutcome::Continue;
                    }
                    // Prose dialog: Enter bypasses completion — submit the raw
                    // line (Shift+Enter inserts a newline). Tab is the only
                    // expansion key.
                    if let Some(a) = Repl::edit_key(key, editor, history, prompt) {
                        return KeyOutcome::Action(a);
                    }
                    return KeyOutcome::Continue;
                }
                KeyCode::Esc => {
                    // Slash engine-backed overlays route Esc through the
                    // engine's per-level ladder (frame replay + esc()). Prose:
                    // Esc is a no-op — dismissal is buffer-driven (stateless);
                    // in plain editing Esc is a no-op too, so this is uniform.
                    if matches!(overlay, ActiveOverlay::V25Menu { .. }) {
                        *pending_key = Some(crate::slashdot_menu::HostKey::Esc);
                        return KeyOutcome::Continue;
                    }
                }
                _ => {
                    // Editing keys pass through in both cases; dismissal is
                    // stateless — the next eval re-derives the dialog from
                    // the buffer.
                    if let Some(a) = Repl::edit_key(key, editor, history, prompt) {
                        return KeyOutcome::Action(a);
                    }
                }
            }
            return KeyOutcome::Continue;
        }
        // No overlay: normal editing (Tab with no dialog is inert — implicit
        // open means the dialog is already up whenever there is anything to
        // complete).
        if let Some(a) = Repl::edit_key(key, editor, history, prompt) {
            return KeyOutcome::Action(a);
        }
        KeyOutcome::Continue
    }
}

impl Repl {
    /// The interactive input-layer driver. The mode guard is *not* created
    /// here: `TermIO::tty` owns the canonical instance and hands a clone to
    /// this constructor (step 4a) — raw-mode entry stays [`start`](Self::start),
    /// while the reader, the signal handler and the guard itself live with
    /// `TermIO`.
    pub fn new(guard: Arc<TermGuard>) -> Self {
        Self { guard }
    }

    /// Enter raw mode (idempotent). Best-effort; `Drop` still restores.
    pub fn start(&self) -> io::Result<()> {
        self.guard.start()
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
    /// `[Input, Divider?, Overlay?]`, and `Output::draw_frame` draws from the
    /// cursor, scrolls and clears within the live region — all
    /// clearing is confined to the frame region and below, so the greeter /
    /// status / session content above the input is never touched. The whole
    /// frame uses at most 2/3 of the terminal height (≥ 1 row).
    pub async fn read_input(
        &mut self,
        prompt: &str,
        // The prompt session. Borrowed, not owned: this is the state the turn
        // needs once in-turn input lands (ui_draw_v2 §4.5 U4), so it has to
        // outlive this call.
        session: &mut PromptSession,
        history: &mut History,
        ctx: &ReplCtx,
        app_state: &Arc<AppState>,
        output: &dyn Output,
        // Content (step 3d): the resize repaint below replays a turn through
        // `Content`, while `output` stays this loop's frame + input seam.
        content: &Content,
        // The persistent reader's event queue (step 2 commit B). The reader is
        // the sole stdin owner; this loop consumes what it forwards rather
        // than calling `event::poll`/`event::read` itself.
        keys: &mut UnboundedReceiver<InputEvent>,
    ) -> Option<InputAction> {
        // Re-anchor the draw cache to the real cursor (DSR) before the first
        // paint (plan §4.2).
        //
        // **This is the starting row, not a correction.** Every content write is
        // counted from the bytes on their way out (`RowCounter` →
        // `note_lines`, checked against a vt100 replay by
        // `streamed_answer_is_counted`), so from the second prompt onward the
        // tracked row is already right and this DSR confirms it. What it is
        // actually for is the *first* paint of a session: the cache starts at row
        // 1 because nothing has been written yet, while the shell that launched
        // picocode may have left its prompt three rows down. Every byte of the
        // greeter would then be counted from the wrong place and the drift
        // would be baked in before the first prompt is drawn.
        // First prompt of the process only — every later prompt's tracked row
        // is measured from the bytes it wrote (see `ENTRY_DSR_DONE`).
        if !ENTRY_DSR_DONE.swap(true, Ordering::SeqCst) {
            let _ = output.cursor_row();
        }
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
        // The prompt-side bar's **raw data**, pushed rather than rendered:
        // this module builds no row — the bar element formats whatever is
        // stored here at its own render (step 4f-a, design §UI).
        //
        // Pushed once per cycle from the same `ctx` the overlays read, so the
        // prompt-side bar and the mid-turn one cannot name different sessions —
        // and pushed before the first paint, because the very first prompt runs
        // before any turn has set a status, and an unset one draws no bar at all.
        content.set_status(crate::agent::status::BarStatus {
            session_name: ctx.session_name.clone(),
            first_message: ctx.session_first_message.clone(),
            provider: ctx.provider.clone(),
            model: ctx.model.clone(),
            thinking: ctx.reasoning_level.clone(),
            context_tokens: ctx.context_tokens,
            context_limit: ctx.context_limit,
        });
        // The session's state, as the loop's own bindings. Destructuring a
        // `&mut PromptSession` reborrows each field *disjointly*, so the body
        // keeps addressing them as locals while they are, in fact, one movable
        // value — and `PromptSession::handle_key` can be handed exactly the
        // fields it touches instead of a `&mut self` that would conflict with
        // these bindings.
        // Terminal size at the last paint is `output.painted_size()`'s answer,
        // read fresh each iteration rather than kept in a `Cell` here. Resizes
        // are consumed without repainting (see the wait below), so a mismatch at
        // the top of the loop is the "repaint is due, and re-anchor first"
        // signal (plan §4.2); a *width* change additionally wipes + replays
        // first. The clear bound this pair used to provide is `plan_apply`'s
        // `screen_bottom` now (plan §4.3).
        //
        // Owning it in the output rather than here also means a resize that
        // happens *during a turn* is caught when the next prompt opens, instead
        // of being absorbed by a fresh read at entry.

        // Compose → allocate → render → apply the input frame (plan §5.2):
        // `[Input]`, or `[Input, Divider, Overlay]` while a pop-up shows.
        // The apply phase owns the rest — an intact live frame means a partial
        // row-diff (so a tall pop-up's scroll is followed without a row
        // counter), and the scroll `delta` and bounded clear live in
        // `plan_apply`. Content *above* the frame (greeter / status / session)
        // is never touched. The closure keeps no state of its own: compose and
        // apply both receive the same `dims`, so they cannot disagree on the
        // frame's size (ui_draw_v2 §9 #3), and the applied pair `draw_frame`
        // reports is recorded by the output itself — the repaint bookkeeping is
        // the engine's fact, not a secondhand read and not a number this loop
        // has to carry between calls.
        // The prompt-side status bar: the same `status_line` the mid-turn bar
        // uses, with the dot **grey** — the two states are the whole reason the
        // bar is permanent. Built once per `read_input`, not per keystroke: the
        // pieces it needs (session name, model, reasoning level, context tokens)
        // cannot change while the input is being typed, and the frame's partial
        // row-diff keeps the row itself current.
        //
        // `term_width` is read at paint time rather than here, so a resize that
        // lands mid-edit re-flows the bar with the frame instead of leaving a
        // bar sized for the old terminal.
        loop {
            // On resume from Ctrl-Z, re-anchor the draw cache (DSR): the shell
            // may have repainted while we were stopped, so the cached frame
            // rows can no longer be trusted (plan §4.2). Raw-mode re-entry is
            // the persistent reader's job (`RESUMED`); this flag is this
            // loop's own resume signal, so the two never race. `guard.start()`
            // here is a harmless idempotent belt-and-suspenders — the DSR below
            // needs raw mode, and this keeps it correct regardless of whether
            // the reader has re-entered yet.
            if RESUMED_REANCHOR.swap(false, Ordering::SeqCst) {
                let _ = self.guard.start();
                let _ = output.cursor_row();
            }
            // Resize detection against the output's `painted_size`. Resize events are
            // consumed without repainting (see the wait below), so after a drag
            // this check first fires on the keystroke that triggers the
            // repaint: a size mismatch means "repaint is due, and re-anchor
            // first" — reflow may have moved the cursor, so the cached frame
            // rows can no longer be trusted (plan §4.2). The clear boundary a
            // stale `prev_term_h` used to provide lives in `plan_apply`
            // (`screen_bottom`) now, so a grow's revealed scrollback survives
            // without any state here.
            // The one place that still queries the terminal live: a resize
            // that produced no event has to be caught somewhere, and this is
            // the loop that runs per keystroke. The answer is *published*
            // (`set_size`) so every reader below and around consumes the
            // watch rather than a private query.
            let (cur_w_u16, cur_h_u16) = crossterm::terminal::size().unwrap_or((80, 24));
            let cur_h = cur_h_u16 as usize;
            let cur_w = cur_w_u16 as usize;
            let (painted_w, painted_h) = output.painted_size();
            if cur_w != painted_w || cur_h != painted_h {
                // Key-triggered repaint after a size change. A *width*
                // change makes a reflowing terminal (wezterm) re-wrap the
                // painted frame, stacking wrapped copies of the input and
                // overlay that no clear can locate precisely. Instead:
                // wipe everything from the cursor down (the reflow leaves
                // it at the end of the content — the best possible
                // starting point), replay the last turn through the normal
                // scrollback (pushing any corruption above the fold), then
                // re-anchor; `settle`'s paint below repaints the input.
                // Skipped without a session — nothing to replay and usually
                // nothing to clear up. Height-only changes don't re-wrap,
                // so they keep the cheap re-anchor path.
                if cur_w != painted_w {
                    if let Some(s) = app_state.session() {
                        let _ = io::stdout().write_all(b"\x1b[0J");
                        let _ = io::stdout().flush();
                        crate::agent::replay::replay_last_turn_silent(&s, content).await;
                    }
                }
                let _ = output.cursor_row();
            }
            // Keep the watch in sync (covers size changes even without an
            // explicit Resize event, e.g. on some terminals).
            crate::termio::set_size(crate::termio::TermSize {
                width: cur_w,
                height: cur_h,
            });
            // --- Eval → outcome → sync → paint, and end the read if that
            // produced an action. The whole sequence lives in `settle`
            // because the turn's `select!` branch runs it too (ui_draw_v2
            // §4.5 U4): one place to get a `/`-command commit and a
            // rewrite-chain bail-out right, rather than two that drift.
            // Everything after it here is the *other* half of the iteration —
            // waiting for a key — which is exactly what the turn has instead
            // of a wait.
            if let Some(action) = session
                .settle(content, prompt, history, (cur_w, cur_h))
                .await
            {
                return Some(action);
            }

            // --- Wait for a key (or a prose-completion wake). While a
            // prose request is armed/in flight, wait with a deadline so
            // the dialog can open (or the debounce fire) without a
            // keystroke; otherwise block as before. Slash frames block —
            // prose state is not consulted while `/` owns the buffer.
            //
            // Events come from the **persistent reader** (`keys`), the sole
            // stdin owner — this loop no longer calls `event::poll`/`read`.
            // Resize is handled *by the reader*, inline: it publishes the new
            // geometry (`set_size`) and does not forward the event, so a drag's
            // stream of resizes never reaches here to force a per-event repaint
            // (which would strand frames while a reflowing terminal is still
            // moving rows). The loop-top probe above notices the settled size
            // and repaints once, re-anchoring via DSR. ---
            let prose_wait = if session.editor.chars().first() == Some(&'/') {
                None
            } else {
                session.prose.poll_timeout()
            };
            let ev: Option<InputEvent> = loop {
                // Keys a window pump parked while nobody could read stdin
                // (the compaction LLM window — `WindowKeySource`) dispatch
                // exactly as if they arrived just now: same Resize
                // handling, same Ctrl-C/Z below, one per iteration so the
                // settle cadence is the typing cadence. Drain before
                // blocking — parked events are older than anything still
                // waiting in the tty.
                if let Some(parked) = app_state.take_parked_key() {
                    match parked {
                        InputEvent::Resize(size) => {
                            crate::termio::set_size(size);
                            continue;
                        }
                        e => break Some(e),
                    }
                }
                // One event off the reader's queue. A deadline when a prose
                // request may wake us; a block otherwise. The reader is the
                // sole sender and lives for the process, so a `None` here means
                // it is gone — treated as a read error, as `event::read`'s `Err`
                // was.
                let e: Option<InputEvent> = match prose_wait {
                    Some(d) => match tokio::time::timeout(d, keys.recv()).await {
                        Ok(Some(ev)) => Some(ev),
                        Ok(None) => return None,
                        Err(_) => break None, // prose wake: re-eval
                    },
                    None => keys.recv().await,
                };
                match e {
                    // Defensive: the reader handles Resize inline and does not
                    // forward it, so this should not fire; if it does, publish
                    // and keep waiting rather than treating it as a key. (The
                    // parked path above is where Resize actually arrives.)
                    Some(InputEvent::Resize(size)) => {
                        crate::termio::set_size(size);
                        continue;
                    }
                    Some(e) => break Some(e),
                    None => return None,
                }
            };
            // --- Dispatch one key. ---
            match ev {
                Some(InputEvent::Key(key)) => {
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
                    // The dispatcher. It answers with what *should* happen on
                    // screen; this loop is the one that actually does it, so
                    // the same function can serve the turn's `select!` branch
                    // later (ui_draw_v2 §4.5 U4) without learning to draw.
                    // Bound before the call: the receiver's borrow of `session`
                    // would otherwise still be live while the argument reads
                    // the same session for its row count.
                    let opts_len = session.opts_len();
                    match session.handle_key(key, opts_len, prompt, history) {
                        KeyOutcome::Continue => {}
                        KeyOutcome::RunEditor => {
                            // The wrapper owns the whole turnover — reader
                            // gate, raw mode, DSR re-anchor; no local logic
                            // here (doc/editor_cleanup.md §2). It is shared
                            // because the turn's arm calls it too.
                            session.open_editor(&self.guard, output);
                        }
                        KeyOutcome::Action(a) => {
                            if let InputAction::Submit(_) = &a {
                                // U4: one `PromptSession` outlives this call,
                                // so the *painted* editor being empty is not the
                                // same as the editor being empty. Painting a
                                // throwaway `LineEditor::new()` cleared the screen
                                // and left the submitted text in `session.editor`,
                                // which is what the *next* compose paints — so
                                // the first keystroke of the turn, or the next
                                // prompt, put the old text back on screen. Clear
                                // the real one and paint that.
                                session.editor.set("");
                                paint_prompt(
                                    content,
                                    prompt,
                                    &session.editor,
                                    Vec::new(),
                                    (cur_w, cur_h),
                                );
                            }
                            return Some(a);
                        }
                    }
                }
                Some(InputEvent::Paste(text)) => {
                    session.paste(&text);
                }
                Some(_) => {} // no other input variants today: ignored
                None => {}    // prose wake with no key: re-eval below
            }
        }
    }

    /// The prompt area's composition: `[Status?, Input, Divider?, Overlay…]`.
    ///
    /// The twin of [`compose_area`] — same body ([`area_body`]), with the
    /// status row arriving as a literal `&str` rather than a rendered bar.
    /// Two callers, and both need it to be *this* composition: the tests,
    /// which assert against bytes they wrote, and [`rows_grant`], which has to
    /// know the rows element's allocation before the frame exists. Sharing it
    /// is what makes a page one screen of rows by construction — a second
    /// composer here is how the page size and the painted height drifted apart
    /// in the first place.
    pub(crate) fn compose_elements(
        editor: &LineEditor,
        prompt: &str,
        status: Option<&str>,
        overlay: Vec<Element>,
        term_w: usize,
    ) -> Vec<Element> {
        let term_w = term_w.max(1);
        let layout = editor.layout(term_w, prompt);
        let input = Element::input(layout, prompt.to_string());
        let mut items: Vec<Element> = Vec::new();
        if let Some(bar) = status {
            items.push(Element::lines(vec![bar.to_string()]));
        }
        items.extend(area_body(Some(input), overlay));
        items
    }

    /// Compose *and* render, for tests that drive `plan_apply` directly and have
    /// no output handle to go through.
    ///
    /// `#[cfg(test)]` on purpose: it is the only remaining way to reach the pure
    /// pipeline outside `Output::draw_elements`, so production *cannot* build a
    /// frame by hand. Dropping the `cfg` would silently reintroduce the second
    /// pipeline that `ui.rs` used to have (ui_draw_v2 §5.1.1).
    #[cfg(test)]
    pub(crate) fn compose_frame(
        editor: &LineEditor,
        prompt: &str,
        status: Option<&str>,
        overlay: Vec<Element>,
        term_w: usize,
        term_h: usize,
    ) -> Frame {
        let term_w = term_w.max(1);
        let elements = Self::compose_elements(editor, prompt, status, overlay, term_w);
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

    /// Non-REPL slash driver (quiet / piped stdin): run one complete `/…`
    /// line through the v2.5 menu engine as if it had been typed and
    /// submitted, with **no overlay drawing** — `Continue` plans are
    /// dropped, commits echo through the same `Output` methods an
    /// interactive `emit_commit` uses (messages, confirm, and the
    /// no-tty `$EDITOR` fallback all stay `Output`/`AppState` concerns;
    /// menu painting is host-side and never happens here).
    ///
    /// Returns `None` for input this driver does not own (empty, non-slash,
    /// `/q`) so the caller falls back to raw `Submit`.
    ///
    /// Key mapping — a passive derive (`HostKey::None`, exactly what typing
    /// does), then virtual `Enter` presses:
    /// - ordinary endpoints finish on the first Enter (`prepare` defaults to
    ///   `submit`);
    /// - two-phase nodes (`Prepared`, e.g. compaction) take a second one —
    ///   an Enter whose returned prompt still matches the stage re-presses;
    /// - an Enter that *navigates* (returned prompt moved: a listing drill)
    ///   stops the sequence instead — blind further Enters could act on a
    ///   row the user never chose;
    /// - a pure rewrite (`ContinuePrompt`, e.g. `/compact` →
    ///   `/tree <id> compact`) re-derives passively and presses again —
    ///   from the user's viewpoint that pair is still one Enter.
    pub(crate) async fn batch_slash(
        app_state: &Arc<AppState>,
        input: &str,
        content: &Content,
        history: &mut History,
    ) -> Option<InputAction> {
        use crate::slashdot_menu::node_impl::MenuResult;
        use crate::slashdot_menu::HostKey;
        // Total evals (passive derives + Enters) for one line: rewrite
        // chains plus prepare/confirm pairs; anything beyond a handful is
        // a cycle or an endlessly re-rendering node — stop.
        const MAX_EVALS: usize = 8;
        if input.is_empty() || !input.starts_with('/') || input == "/q" {
            return None;
        }
        let mut menu = crate::v25_menu::build_menu(app_state);
        let mut handle = menu.get_handle();
        let width = crate::output::term_width();
        let mut buf = input.to_string();
        // Prompt of the last Continue — the node an Enter acts on. `None`
        // means "derive passively before pressing".
        let mut stage: Option<String> = None;
        let mut press_enter = false;
        let mut evals = 0;
        loop {
            evals += 1;
            if evals > MAX_EVALS {
                break;
            }
            let key = if press_enter {
                HostKey::Enter
            } else {
                HostKey::None
            };
            let line = buf.trim_start_matches('/').to_string();
            let cursor = line.len();
            match handle
                .eval_prompt_with_cursor(&line, key, width, cursor)
                .await
            {
                MenuResult::ContinuePrompt(new_prompt) => {
                    // Pure rewrite: adopt it, re-derive passively, then
                    // press again (the rewrite consumed one Enter).
                    buf = format!("/{}", new_prompt);
                    stage = None;
                    press_enter = false;
                }
                MenuResult::Continue(_plan, prompt) => {
                    if press_enter {
                        match &stage {
                            // Still on the same node → the Enter armed or
                            // re-armed a confirm → press again.
                            Some(s) if *s == prompt => {}
                            // Navigated (listing drill): stop uncommitted.
                            _ => break,
                        }
                    } else {
                        stage = Some(prompt);
                    }
                    press_enter = true;
                }
                MenuResult::DoneSuccess(msg, prompt) => {
                    Self::batch_commit(content, history, &prompt, msg.as_deref());
                    return Some(InputAction::MenuDone);
                }
                MenuResult::DoneError(err, prompt) => {
                    Self::batch_commit(content, history, &prompt, Some(&err));
                    return Some(InputAction::MenuDone);
                }
            }
        }
        // Cap exhausted or navigation without a commit: nothing written and
        // nothing echoed (an overlay would have been showing in a REPL).
        Some(InputAction::MenuDone)
    }

    /// Echo a batch-committed prompt to the transcript and surface the
    /// node's message — the drawing-free counterpart of `emit_commit`
    /// (no input region to clear, no prefill to seed).
    fn batch_commit(content: &Content, history: &mut History, prompt: &str, msg: Option<&str>) {
        let committed = format!("/{}", prompt);
        history.add(&committed);
        content.display_plain(&committed);
        if let Some(m) = msg {
            content.display_text(m);
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
    /// `/model`, `/reasoning`, and `/set <key> <value>` commits so each
    /// leaves exactly one separator directly below the emitted line — the next
    /// prompt's own divider (drawn below its input) no longer has to do that
    /// job, which previously left `/set` value commits looking
    /// separator-less because their next prompt (`/set` list) is visibly
    /// different from the emitted line. `redraw` is the live overlay closure
    /// from `read_input`; it clears the input region through the frame
    /// pipeline, after which `display_plain` writes the line at the frame's
    /// caret (plan §5.2).
    ///
    /// **The committed line is bracketed by rules** — one above, one below.
    ///
    /// This comment used to say the opposite, and the reason it gave has since
    /// stopped being true: "the next prompt's frame draws its own divider, so a
    /// rule above the committed line would double it." That divider is gone — the
    /// input frame is `[Status, Input]` and draws a rule only when an overlay is
    /// open — so there was nothing left to double, and a committed line sat in
    /// the transcript as bare text between two answers, easy to read as
    /// something the model said. A commit is a real event (it changed a setting
    /// the reader will want to find again after scrolling) and gets the same
    /// `─────` treatment a turn does.
    ///
    /// The status bar follows underneath without being asked for: the commit is
    /// written at the caret, which pushes the tracked row down, so the next
    /// frame apply draws the area below what was just written.
    fn emit_commit<F: Fn(&LineEditor, Vec<Element>, (usize, usize))>(
        content: &Content,
        editor: &mut LineEditor,
        redraw: &F,
        dims: (usize, usize),
        line: &str,
        next_input: &str,
        history: &mut History,
    ) {
        // v2.5 menu commits (`/replay 1`, `/reasoning low`, …) return
        // `MenuDone` and never pass through the `Submit` arm in agent.rs
        // where typed input is recorded — record the committed line here
        // so recall (Up/Down) sees every command the user actually ran.
        history.add(line);
        // No DSR here: the draw cache's tracked row is the steady-state
        // anchor, content writes advance it, and a foreign
        // screen writer ($EDITOR through `run_editor`) re-anchors on exit
        // (plan §4.2). Clear the input region, then echo the line
        // at the caret the frame apply left.
        redraw(&LineEditor::new(), Vec::new(), dims);
        // Bracketed by rules, the way a turn is — and, when a prompt follows
        // directly, sharing one with it rather than stacking two (ui_draw_v2
        // R4). A committed line is a real event in the transcript — `/thinking
        // high` changed a setting the reader will want to find again after
        // scrolling — and a bare `/thinking high` between two answers is easy
        // to read as something the model said. The status bar follows
        // underneath, because the next `redraw` repaints the frame below
        // whatever was just written: content written at the caret pushes the
        // tracked row down, and the next apply clears from *there*.
        content.display_committed_prompt(line);
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
                let w = crate::termio::size().width;
                // Not on the first visual row: this is cursor movement inside a
                // multi-line line, not recall.
                if !editor.visual_up(w, prompt) {
                    // History is browsed only from an *empty* line, plus while a
                    // walk is in progress — the recalled entry on screen is not
                    // a draft, so `Up` keeps stepping back through it. Any text
                    // the user typed ends the walk (every content write does), so
                    // `nav.is_some()` and "the buffer is the user's" can never
                    // both be true, and a draft is never replaced.
                    let browsable = editor.text().is_empty() || editor.nav.is_some();
                    if browsable {
                        if let Some(s) = history.prev(editor) {
                            editor.set_recalled(&s);
                        }
                    }
                }
                None
            }
            KeyCode::Down => {
                let w = crate::termio::size().width;
                if !editor.visual_down(w, prompt) {
                    // Forward-only: `Down` moves through a walk that `Up` began,
                    // and does nothing at all otherwise. With no walk in progress
                    // `nav` is `None` — the line is either empty or the user's own
                    // draft, and neither may be overwritten by an entry here.
                    if editor.nav.is_some() {
                        if let Some(s) = history.next(editor) {
                            editor.set_recalled(&s);
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }
}

/// A flat character buffer + cursor for multi-line editing, plus the `Up`/`Down`
/// history-recall position that describes what the buffer currently holds. All
/// methods are pure (no terminal access) and unit-tested in [`tests`].
#[derive(Default)]
pub struct LineEditor {
    buf: Vec<char>,
    cursor: usize, // char index into `buf`
    /// Where the line sits in the `Up`/`Down` recall walk: `Some(index)` while
    /// the buffer holds an untouched history entry, `None` while it holds the
    /// user's own text.
    ///
    /// Lives here rather than on the session because the rule that maintains it
    /// is about *this buffer*: any content write ends the walk (see
    /// [`LineEditor::wrote`]), and every write goes through a method below.
    /// Session-level writers — prefill, `Ctrl-E`, submit, a `/`-commit — end it
    /// for free, where a flag cleared at each call site would rot.
    nav: Option<usize>,
    /// The line as it was before the walk started, restored when `Down` steps
    /// past the newest entry.
    ///
    /// Always empty today: recall only starts on an empty line, so there is
    /// nothing to stash. Kept because it is the restore path the moment recall
    /// is ever allowed to start over a draft.
    draft: String,
}

impl LineEditor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Any content write ends the recall walk: the line is the user's again, so
    /// `Up`/`Down` must not go on replacing it. One line, called at the end of
    /// every mutator below, is the whole rule — no key arm can forget it.
    ///
    /// The two writes that are *not* the user's own text say so by not coming
    /// through here: [`set_recalled`](Self::set_recalled) for history, and
    /// [`set_derived`](Self::set_derived) for the `/`-engine's rewrite of a
    /// prompt that recall brought back.
    fn wrote(&mut self) {
        self.nav = None;
        self.draft.clear();
    }

    /// Replace the whole buffer. Cursor → end.
    ///
    /// Ends recall: the buffer is no longer the entry the walk landed on, so
    /// `Up`/`Down` stop browsing until the line is cleared again.
    pub fn set(&mut self, s: &str) {
        self.set_derived(s);
        self.wrote();
    }

    /// Replace the whole buffer without ending recall — for writes whose text
    /// comes *from* the line already on screen rather than from the user.
    ///
    /// Two callers, and both need it: history navigation ([`set_recalled`]),
    /// and the `/`-menu's rewrite of a recalled `/`-command, which re-derives
    /// the prompt from the buffer the walk put there. Without this second one,
    /// recalling `/model` would expand to `/model `, the rewrite would look like
    /// an edit, and the walk would end with the menu open — locking the user
    /// into the prompt they just recalled, unable to reach `Up`/`Down` again
    /// short of clearing the line.
    fn set_derived(&mut self, s: &str) {
        self.buf = s.chars().collect();
        self.cursor = self.buf.len();
    }

    /// Replace the whole buffer with a history entry (cursor → end), staying in
    /// the recall walk. The counterpart to [`wrote`](Self::wrote): this is the
    /// write that *moves* the walk rather than ending it.
    pub fn set_recalled(&mut self, s: &str) {
        self.set_derived(s);
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
        self.wrote();
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
        self.wrote();
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.buf.insert(self.cursor, c);
            self.cursor += 1;
        }
        self.wrote();
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
        self.wrote();
    }

    pub fn delete(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.remove(self.cursor);
        }
        self.wrote();
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
        self.wrote();
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
            self.wrote();
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
        self.wrote();
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
        self.wrote();
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

    /// Step backward through history; the first call stashes the current line as
    /// the draft and the last step leaves the walk entirely.
    ///
    /// Takes the editor rather than a `nav`/`draft` pair because the walk *is*
    /// the editor's [`nav`](LineEditor::nav) plus its stashed
    /// [`draft`](LineEditor::draft) — splitting them out let a caller write to
    /// the buffer and leave them behind, which is how a recalled prompt ended
    /// up with a stale position and a `Down` that overwrote the user's text.
    ///
    /// Returns the text to show; the caller installs it with
    /// [`set_recalled`](LineEditor::set_recalled), the one write that keeps the
    /// walk alive. Writing it with plain `set` would end the walk on the very
    /// keystroke that started it.
    pub fn prev(&self, editor: &mut LineEditor) -> Option<String> {
        let n = self.entries.len();
        if n == 0 {
            return None;
        }
        if editor.nav.is_none() {
            editor.draft = editor.text();
            editor.nav = Some(n); // point just past the newest entry
        }
        let i = editor.nav.unwrap();
        if i == 0 {
            return None;
        }
        let new_i = i - 1;
        editor.nav = Some(new_i);
        Some(self.entries[new_i].clone())
    }

    /// Step forward through a walk [`prev`](Self::prev) started; past the newest
    /// entry it restores the stashed draft and leaves the walk.
    ///
    /// `None` when no walk is in progress — the caller gates on
    /// `editor.nav.is_some()` too, so that "Down only ever moves forward
    /// through a walk Up began" is stated at the key as well as here.
    pub fn next(&self, editor: &mut LineEditor) -> Option<String> {
        let n = self.entries.len();
        match editor.nav {
            None => None,
            Some(i) => {
                if i + 1 >= n {
                    let d = std::mem::take(&mut editor.draft);
                    editor.nav = None;
                    return Some(d);
                }
                let new_i = i + 1;
                editor.nav = Some(new_i);
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

    /// A committed line is bracketed by rules: one above, one below, and the
    /// line between them.
    ///
    /// Pinned here rather than through a pty, because a pty cannot reach this
    /// path: a v2.5 menu commit needs key routing the engine owns, and the
    /// level of a `/thinking` commit comes from a *drilled row* rather than from
    /// the typed buffer — so `/thinking high` plus Enter commits nothing and the
    /// test would be asserting the engine's menu behaviour while claiming to
    /// cover this. (An earlier pty version of this test "passed" about half the
    /// time, and its failures were read as build races.)
    ///
    /// What is asserted is the *order and kind* of the three writes, which is
    /// the whole contract: `display_separator`, `display_plain`, `display_separator`.
    #[test]
    fn a_committed_line_is_written_between_two_rules() {
        let reasoning = crate::settings::default_show_reasoning_handle();
        // The width is per-instance state since step 2 commit A (`with_size`
        // seeds a private size watch), so no serialization is needed.
        let out = crate::output::ConsoleOutput::with_size(
            reasoning,
            crate::termio::TermSize {
                width: 80,
                height: 24,
            },
        );
        let mut editor = LineEditor::new();
        // Built by hand rather than `History::load()`, which reads the real
        // history file — a test that depends on the machine's `~/.picocode`, and
        // `add` would then write to it.
        let mut history = History {
            entries: Vec::new(),
            file: None,
        };
        let line = "/thinking high";

        out.begin_capture();
        // The redraw closure is a no-op: this test is about what `emit_commit`
        // writes, and the real closure's frame apply is covered by the REPL's
        // own tests. Passing a real one here would only add a bar row to search.
        Repl::emit_commit(
            &out.content,
            &mut editor,
            &|_, _, _| {},
            (80, 24),
            line,
            "",
            &mut history,
        );
        let bytes = out.end_capture();
        let text = String::from_utf8_lossy(&bytes);

        let rule = '\u{2500}';
        let first_rule = text
            .find(rule)
            .unwrap_or_else(|| panic!("a rule above the committed line: {text:?}"));
        let the_line = text
            .find(line)
            .unwrap_or_else(|| panic!("the committed line itself: {text:?}"));
        let second_rule = text[the_line + line.len()..]
            .find(rule)
            .map(|i| the_line + line.len() + i)
            .unwrap_or_else(|| panic!("a rule below the committed line: {text:?}"));
        assert!(
            first_rule < the_line && the_line < second_rule,
            "rule, line, rule — in that order: {text:?}"
        );
        // And the editor is re-seeded for the next prompt, so the committed text
        // is not left in the input row as if it were still being typed.
        assert_eq!(
            editor.text(),
            "",
            "the input row is cleared for the next prompt"
        );
    }

    /// A 40-column replay of `bytes`, as trimmed screen rows.
    fn screen_rows(bytes: &[u8]) -> Vec<String> {
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
        let mut p = vt100::Parser::new(24, 40, 1000);
        p.process(&onlcr);
        (0..24)
            .map(|r| {
                (0..40)
                    .map(|c| {
                        p.screen()
                            .cell(r as u16, c as u16)
                            .and_then(|x| x.contents().chars().next())
                            .unwrap_or(' ')
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// A `ConsoleOutput` carrying a status, plus the redraw `emit_commit`
    /// expects: compose the area and draw it, the way `read_input` does.
    fn commit_harness() -> (crate::output::ConsoleOutput, LineEditor) {
        let reasoning = crate::settings::default_show_reasoning_handle();
        let out = crate::output::ConsoleOutput::with_size(
            reasoning,
            crate::termio::TermSize {
                width: 40,
                height: 24,
            },
        );
        out.content.set_status(crate::agent::status::BarStatus {
            session_name: Some("sess".to_string()),
            provider: "llamacpp".to_string(),
            model: "test".to_string(),
            ..Default::default()
        });
        (out, LineEditor::new())
    }

    fn redraw_fn(
        out: &crate::output::ConsoleOutput,
    ) -> impl Fn(&LineEditor, Vec<Element>, (usize, usize)) + '_ {
        move |editor: &LineEditor, overlay: Vec<Element>, dims: (usize, usize)| {
            paint_prompt(&out.content, "> ", editor, overlay, dims);
        }
    }

    /// Two committed prompts render as two rule-pairs with **no blank row
    /// between them**.
    ///
    /// The transcript is `rule, line, rule` for each commit, and the two groups
    /// butt together — a blank there reads as a third, empty event. The row
    /// count is what makes this fragile: the area is re-drawn under each commit
    /// (its status and input rows), and if the re-draw's position leaves a row
    /// of air, the next commit's opening rule lands below it.
    #[test]
    fn two_committed_prompts_render_without_a_blank_between_their_rules() {
        let (out, mut editor) = commit_harness();
        let mut history = History {
            entries: Vec::new(),
            file: None,
        };
        let redraw = redraw_fn(&out);

        out.begin_capture();
        for line in [
            "/model xiaomimimo:mimo-v2.6-flash activate",
            "/reasoning high",
        ] {
            Repl::emit_commit(
                &out.content,
                &mut editor,
                &redraw,
                (40, 24),
                line,
                "",
                &mut history,
            );
        }
        let rows = screen_rows(&out.end_capture());

        // Two commits, and the rule *between* them is shared: one `──────`,
        // not the first's closing rule stacked against the second's opening
        // one (which is what this test's arithmetic used to assume — four
        // rules, adjacent, and a `third == second + 1` that only held because
        // both rules were drawn). Three rules for two commits is the outer
        // pair of each plus that shared middle.
        let rules: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.starts_with('\u{2500}'))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            rules.len(),
            3,
            "two consecutive commits share the rule between them: {rows:?}"
        );
        // …and nothing spaced out: the point of the shared rule is that a
        // prompt commits directly beneath the one above it (ui_draw_v2 R4).
        for pair in rules.windows(2) {
            assert!(
                rows[pair[0] + 1..pair[1]]
                    .iter()
                    .all(|r| !r.trim().is_empty()),
                "blank row between the rules of two consecutive commits: {rows:?}"
            );
        }
    }

    /// A committed prompt following turn content keeps **both** its rules.
    ///
    /// The upper one is the one that goes missing: the turn's content ends on
    /// its own trailing blank, and the commit's opening rule has to land on the
    /// row directly after it rather than be absorbed into the gap.
    #[test]
    fn a_committed_prompt_after_turn_content_keeps_its_upper_rule() {
        let (out, mut editor) = commit_harness();
        let mut history = History {
            entries: Vec::new(),
            file: None,
        };
        let redraw = redraw_fn(&out);

        out.begin_capture();
        // Turn content the way a real turn leaves it: output ending in a blank.
        out.display_plain("git status output line\nmore output\n");
        Repl::emit_commit(
            &out.content,
            &mut editor,
            &redraw,
            (40, 24),
            "Great, nice work! Please commit your changes.",
            "",
            &mut history,
        );
        let rows = screen_rows(&out.end_capture());

        let line = rows
            .iter()
            .position(|r| r.contains("Great, nice work"))
            .expect("the committed prompt is on screen");
        assert!(
            rows[line - 1].starts_with('\u{2500}'),
            "a rule directly above the prompt: {:?}",
            &rows[line - 3..=line + 3]
        );
        // …and the blank above *that* is the turn's own trailing blank, not a
        // second one the commit opened.
        assert!(
            rows[line - 2].trim().is_empty() && !rows[line - 3].trim().is_empty(),
            "the turn's content, one blank, then the rule: {:?}",
            &rows[line - 3..=line + 5]
        );
        // The prompt wraps at this width, so the closing rule is "somewhere
        // below", not "the next row" — find it rather than assume.
        let closing = rows
            .iter()
            .enumerate()
            .skip(line + 1)
            .find(|(_, r)| r.starts_with('\u{2500}'))
            .map(|(i, _)| i)
            .expect("the rule below the committed prompt");
        assert!(
            rows[closing - 1].contains("nges.") || rows[closing - 1].contains("commit"),
            "the rule follows the prompt's last row: {:?}",
            &rows[line..=closing]
        );
    }

    // ------------------------------------------------------------------
    // Batch slash driver (`Repl::batch_slash`) — the non-REPL input path
    // (quiet / piped stdin): a complete line commits through the v2.5
    // engine with a virtual Enter and no overlay drawing.
    // ------------------------------------------------------------------

    fn batch_state() -> Arc<AppState> {
        let settings = crate::settings::SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_reasoning_level: None,
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            models: Default::default(),
            path: None,
        };
        AppState::for_test(settings)
    }

    /// In-memory history (no disk file) — `batch_commit` only appends.
    fn batch_hist() -> History {
        History {
            entries: Vec::new(),
            file: None,
        }
    }

    #[test]
    fn batch_slash_reasoning_high_commits_in_one_enter() {
        let state = batch_state();
        let (out, lines) = crate::output::RecordingOutput::new();
        let content = Content::with_tap(out);
        let action = crate::agent::block_on_test(Repl::batch_slash(
            &state,
            "/reasoning high",
            &content,
            &mut batch_hist(),
        ));
        assert!(matches!(action, Some(InputAction::MenuDone)));
        // The node wrote the runtime target; the loop's convergence step
        // would apply it on the same cycle.
        assert_eq!(state.reasoning().as_deref(), Some("high"));
        let captured = crate::output::RecordingOutput::joined(&lines);
        assert!(
            captured.contains("reasoning high"),
            "committed line not echoed: {:?}",
            captured
        );
    }

    #[test]
    fn batch_slash_quit_latches_control_target() {
        let state = batch_state();
        let (out, _lines) = crate::output::RecordingOutput::new();
        let content = Content::with_tap(out);
        let action = crate::agent::block_on_test(Repl::batch_slash(
            &state,
            "/quit",
            &content,
            &mut batch_hist(),
        ));
        assert!(matches!(action, Some(InputAction::MenuDone)));
        assert!(state.take_control().quit);
    }

    #[test]
    fn batch_slash_defers_what_it_does_not_own() {
        // Empty, prose, and the `/q` alias fall back to raw `Submit`
        // (the agent loop's `/q`/`exit` arm keeps owning them).
        let state = batch_state();
        let (out, _lines) = crate::output::RecordingOutput::new();
        let content = Content::with_tap(out);
        for line in ["", "hello", "/q"] {
            let action = crate::agent::block_on_test(Repl::batch_slash(
                &state,
                line,
                &content,
                &mut batch_hist(),
            ));
            assert!(action.is_none(), "{:?} should fall through", line);
        }
    }

    #[test]
    fn batch_slash_unknown_command_returns_instead_of_hanging() {
        let state = batch_state();
        let (out, _lines) = crate::output::RecordingOutput::new();
        let content = Content::with_tap(out);
        let action = crate::agent::block_on_test(Repl::batch_slash(
            &state,
            "/definitely-not-a-command",
            &content,
            &mut batch_hist(),
        ));
        // Whatever the engine reports for garbage input, the driver must
        // terminate (the eval cap) and hand back control.
        assert!(matches!(action, Some(InputAction::MenuDone)));
    }

    // --- Phase 2: string-hierarchy parsing (pure) ---

    // ---- `/compact` overlay resolution ----

    // ------------------------------------------------------------------
    // /tree list rendering: turn grouping, no tools, compaction scope
    // ------------------------------------------------------------------

    #[test]
    fn an_active_row_keeps_star_when_it_is_not_selected() {
        // The `*` current-state marker owns the 2-char prefix and does not
        // depend on the cursor: an active row that is NOT selected keeps `*`,
        // and the cursor row keeps `>`. (active == selected, where `*` wins
        // over `>`, is pinned by `an_active_marker_beats_the_cursor`.)
        use crate::slashdot_menu::render::{paint_rows, LogicalLine};
        let rows = vec![LogicalLine::new("one"), LogicalLine::new("two")];
        let s = paint_rows(&rows, Some(0), &[1], 40);
        assert!(s.rows[0].contains("> one"), "{:?}", s.rows[0]);
        assert!(s.rows[1].contains("* two"), "{:?}", s.rows[1]);
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

    /// Three entries, no disk file — the walk needs something to walk.
    fn recall_hist() -> History {
        let mut h = History {
            entries: Vec::new(),
            file: None,
        };
        h.add("first");
        h.add("second");
        h.add("third");
        h
    }

    fn up_key() -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)
    }

    fn down_key() -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
    }

    /// A recall that lands on the newest entry, with the editor holding it —
    /// the state every `Up`/`Down` question below starts from.
    fn recalled_newest() -> (History, LineEditor) {
        let mut h = recall_hist();
        let mut ed = LineEditor::new();
        Repl::edit_key(up_key(), &mut ed, &mut h, "");
        assert_eq!(ed.text(), "third");
        (h, ed)
    }

    #[test]
    fn history_navigation_with_draft_restore() {
        // The walk itself, with no key handler around it: `prev`/`next` move the
        // editor's position and hand back the text for the caller to install
        // with `set_recalled`. Written against a buffer that already holds a
        // draft so the stash is exercised; recall from the key handler only
        // ever starts on an empty line (see `draft_is_always_empty`).
        let mut h = History {
            entries: Vec::new(),
            file: None,
        };
        h.add("first");
        h.add("second");
        let mut ed = LineEditor::new();
        ed.insert_str("draft");
        ed.nav = Some(2); // as if `prev` had already stashed "draft"
        ed.draft = "draft".to_string();
        assert_eq!(h.prev(&mut ed), Some("second".to_string()));
        ed.set_recalled("second");
        assert_eq!(h.prev(&mut ed), Some("first".to_string()));
        ed.set_recalled("first");
        assert_eq!(h.prev(&mut ed), None);
        // stepping forward restores the stashed draft past the newest entry
        assert_eq!(h.next(&mut ed), Some("second".to_string()));
        assert_eq!(h.next(&mut ed), Some("draft".to_string()));
        assert!(ed.nav.is_none());
    }

    #[test]
    fn draft_is_always_empty() {
        // `draft` looks vestigial because it is, under the current policy:
        // recall only starts on an empty line, so there is nothing to stash.
        // Pinned so that stays true — the moment recall is allowed to start
        // over a draft, this is the test that has to be re-read.
        let mut h = recall_hist();
        let mut ed = LineEditor::new();
        Repl::edit_key(up_key(), &mut ed, &mut h, "");
        assert_eq!(
            ed.draft, "",
            "recall starts on an empty line, so nothing to stash"
        );
        ed.set_recalled("second");
        Repl::edit_key(up_key(), &mut ed, &mut h, "");
        assert_eq!(ed.draft, "");
    }

    #[test]
    fn up_at_top_preserves_unsubmitted_prompt() {
        // Regression: hitting Up at the top of an unsubmitted prompt used to
        // silently stash the in-progress text as the history draft and replace
        // it with the previous entry — easy to trigger when reaching for Home
        // and destructive enough to lose the whole prompt. Up should now be a
        // no-op while the buffer is non-empty and the user hasn't started
        // navigating history yet.
        let mut h = recall_hist();
        let mut editor = LineEditor::from_text("in progress");
        editor.end();
        let _ = Repl::edit_key(up_key(), &mut editor, &mut h, "");
        assert_eq!(
            editor.text(),
            "in progress",
            "Up must not clobber the unsubmitted prompt"
        );
        assert!(
            editor.nav.is_none(),
            "history navigation must not have started"
        );
        assert_eq!(editor.draft, "", "no draft should have been stashed");
    }

    #[test]
    fn up_at_top_with_empty_buffer_still_loads_history() {
        // Counterpart to the regression test: an empty buffer should still
        // navigate history on Up so the previous-entry recall behavior
        // (the original reason Up-on-top exists) is preserved.
        let mut h = recall_hist();
        let mut editor = LineEditor::new();
        let _ = Repl::edit_key(up_key(), &mut editor, &mut h, "");
        assert_eq!(
            editor.text(),
            "third",
            "empty buffer should load latest history entry"
        );
        assert!(editor.nav.is_some());
        assert_eq!(editor.draft, "");
    }

    #[test]
    fn up_continues_history_navigation_when_already_inside() {
        // Once the user has already navigated back, they are intentionally
        // walking the history; Up must keep advancing even though the buffer is
        // non-empty (it holds a history entry, not a fresh draft), and Down
        // must keep restoring. This is the whole content of
        // `browsable = text.is_empty() || nav.is_some()`.
        let mut h = recall_hist();
        let mut editor = LineEditor::new();
        let _ = Repl::edit_key(up_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "third");
        let _ = Repl::edit_key(up_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "second");
        let _ = Repl::edit_key(down_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "third");
    }

    #[test]
    fn down_does_not_clobber_a_draft_typed_after_recall() {
        // Regression: Down consulted the recall position alone, so a line
        // edited after a recall was still "inside history" as far as it was
        // concerned — Down replaced the edit with the next entry, and at the
        // newest entry with the (empty) stash, so the prompt went blank. The
        // edit has to survive.
        let (mut h, mut editor) = recalled_newest();
        editor.insert_str(" EDITED");
        assert_eq!(editor.text(), "third EDITED");
        let _ = Repl::edit_key(down_key(), &mut editor, &mut h, "");
        assert_eq!(
            editor.text(),
            "third EDITED",
            "Down must not replace a draft with a history entry"
        );
        assert!(editor.nav.is_none(), "the edit ended the walk");
    }

    #[test]
    fn down_does_not_clobber_a_draft_on_a_fresh_line() {
        // The same guarantee with no recall in progress at all — Down is
        // forward-only, so an untouched draft is unreachable by definition.
        let mut h = recall_hist();
        let mut editor = LineEditor::from_text("half-written thought");
        editor.end();
        let _ = Repl::edit_key(down_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "half-written thought");
    }

    #[test]
    fn down_on_a_cleared_line_does_not_pull_history() {
        // Regression: clearing the buffer (here `Ctrl-U`) used to leave the
        // recall position behind, so Down on the now-empty line pulled an entry
        // in — history browsing the user had not asked for. Clearing the line
        // ends the walk, so the next Down does nothing.
        let mut h = recall_hist();
        let mut editor = LineEditor::new();
        let _ = Repl::edit_key(up_key(), &mut editor, &mut h, "");
        let _ = Repl::edit_key(up_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "second");
        Repl::edit_key(
            event::KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut editor,
            &mut h,
            "",
        );
        assert_eq!(editor.text(), "");
        assert!(editor.nav.is_none(), "clearing the line ends the walk");
        let _ = Repl::edit_key(down_key(), &mut editor, &mut h, "");
        assert_eq!(
            editor.text(),
            "",
            "Down on an empty line must not pull an entry in"
        );
    }

    #[test]
    fn up_after_editing_a_recalled_line_keeps_the_edit() {
        // Regression (the Up direction): once inside history, an edit used to
        // be overwritten by the next `Up` with nothing stashed to recover it.
        let (mut h, mut editor) = recalled_newest();
        editor.insert_str(" EDITED");
        let _ = Repl::edit_key(up_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "third EDITED");
    }

    #[test]
    fn every_content_write_ends_the_walk() {
        // The invariant the key handler leans on: with `nav` cleared, `Up`
        // cannot browse over a line the user owns. One case per writer that can
        // change the text without the user typing a printable char, plus the
        // two session-level ones (`set` for prefill/`Ctrl-E`/submit,
        // `replace_range` for the prose completion splice).
        let mut h = recall_hist();
        let mut editor = LineEditor::new();
        Repl::edit_key(up_key(), &mut editor, &mut h, "");
        assert!(editor.nav.is_some());
        // Cursor moves are not content writes and must keep the walk alive —
        // this is what lets `Up`/`Down` move the caret inside a recalled
        // multi-line entry without dropping out of history.
        editor.left();
        editor.end();
        editor.move_down_line();
        assert!(editor.nav.is_some(), "cursor movement keeps the walk");
        editor.insert_newline();
        assert!(editor.nav.is_none(), "insert_newline ends the walk");

        for (what, f) in [
            ("insert_str", 1usize),
            ("backspace", 2),
            ("delete", 3),
            ("delete_line_start", 4),
            ("delete_line_end", 5),
            ("replace_range", 6),
            ("set", 7),
        ] {
            let mut ed = LineEditor::new();
            Repl::edit_key(up_key(), &mut ed, &mut h, "");
            assert!(ed.nav.is_some(), "{what}: recall to start from");
            match f {
                1 => ed.insert_str("x"),
                2 => ed.backspace(),
                3 => {
                    ed.left();
                    ed.delete();
                }
                4 => ed.delete_line_start(),
                5 => {
                    ed.home();
                    ed.delete_line_end();
                }
                6 => ed.replace_range(0, 1, "y"),
                _ => ed.set("z"),
            }
            assert!(ed.nav.is_none(), "{what} must end the walk");
        }
    }

    #[test]
    fn recall_starts_at_the_newest_entry_after_a_submit() {
        // Regression: the recall position lived on the session and outlived the
        // prompt, so after submitting a recalled line the next prompt's first
        // `Up` resumed from the stale index and offered an *older* entry. The
        // editor that `read_input` clears on submit now carries the reset.
        let mut h = recall_hist();
        let mut editor = LineEditor::new();
        Repl::edit_key(up_key(), &mut editor, &mut h, "");
        // Submit: the buffer is cleared and the line recorded (repl.rs:1707).
        editor.set("");
        h.add("third");
        assert!(editor.nav.is_none());
        let _ = Repl::edit_key(up_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "third", "recall starts at the newest entry");
    }

    #[test]
    fn a_derived_rewrite_keeps_the_walk() {
        // History holds `/`-commands, so recalling one re-opens its menu and
        // the `/`-engine rewrites the line from what recall put there
        // (`/model` → `/model `). That rewrite is not the user typing, so it
        // must not look like an edit: if it ended the walk, `Up`/`Down` would
        // refuse to browse with the menu open and the user would be locked into
        // the prompt they had just recalled until they cleared it.
        let (mut h, mut editor) = recalled_newest();
        editor.set_recalled("/model");
        editor.set_derived("/model ");
        assert_eq!(editor.text(), "/model ");
        assert!(
            editor.nav.is_some(),
            "the engine's rewrite of a recalled prompt keeps the walk"
        );
        // …and the walk still works: `Up` steps one entry further back.
        h.add("/model");
        let _ = Repl::edit_key(up_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "second", "Up still walks the history");
        // `Down` likewise, and back to the newest entry rather than locking.
        let _ = Repl::edit_key(down_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "third");
        let _ = Repl::edit_key(down_key(), &mut editor, &mut h, "");
        assert_eq!(editor.text(), "/model", "Down reaches the newest entry");
    }

    #[test]
    fn a_recalled_slash_command_still_browses_when_its_menu_reopens() {
        // The same lock-in, end to end through the real `/`-engine. History
        // holds the `/`-commands the menu commits, so recalling one re-opens its
        // menu, and opening it rewrites the prompt it was recalled from
        // (`/model` → `/model `). That rewrite is the engine talking about the
        // line already on screen, not the user typing, so the walk survives it
        // — otherwise `Up`/`Down` would go dead with the menu open and the user
        // would have to clear the prompt to get history back.
        //
        // `Tab` is what opens the option list, so it is the key that makes the
        // engine rewrite; a passive eval of `/model` alone derives no options
        // and leaves the text alone.
        let mut h = History {
            entries: Vec::new(),
            file: None,
        };
        h.add("older line");
        h.add("/model");
        let mut sess = PromptSession::new(None, &batch_state());
        Repl::edit_key(up_key(), &mut sess.editor, &mut h, "");
        assert_eq!(sess.editor.text(), "/model");
        sess.pending_key = Some(crate::slashdot_menu::HostKey::Tab);
        let outcome = crate::agent::block_on_test(sess.eval());
        assert!(
            matches!(outcome, EvalOutcome::Frame),
            "opening the menu is a frame, not a commit"
        );
        assert!(
            matches!(sess.overlay, ActiveOverlay::V25Menu { .. }),
            "recalling a `/`-command re-opens its menu"
        );
        assert_eq!(sess.editor.text(), "/model ", "the menu rewrote the prompt");
        assert!(
            sess.editor.nav.is_some(),
            "the rewrite must not end the walk"
        );
        // …and history still answers.
        let _ = Repl::edit_key(up_key(), &mut sess.editor, &mut h, "");
        assert_eq!(sess.editor.text(), "older line", "Up still walks history");
    }

    #[test]
    fn a_title_lives_in_the_detail_section_above_the_rows() {
        // The title is the node's `header()` output, so it is the detail
        // section; rows keep their `> `/`  ` markers beside it.
        use crate::slashdot_menu::render::{paint_detail, paint_rows, LogicalLine};
        let detail = paint_detail(&[LogicalLine::new("Select a model:")], 80);
        assert_eq!(detail.height(), 1, "the title appears once");
        let rows = paint_rows(
            &[
                LogicalLine::new("a: alpha model ctx 128000 $0.10/0.20 per M"),
                LogicalLine::new("b: beta model ctx 200000 $0.30/0.40 per M"),
            ],
            Some(0),
            &[],
            80,
        );
        assert!(rows.rows[0].contains("> a: alpha"), "{:?}", rows.rows[0]);
        assert!(rows.rows[1].contains("  b: beta"), "{:?}", rows.rows[1]);
    }

    #[test]
    fn a_wrapped_option_keeps_its_first_line_marked() {
        // The option arrives pre-wrapped (word wrap happens where the plan is
        // built, as the v2.5 engine does): the painter must keep the first line
        // marked and every continuation indented under the option text.
        use crate::slashdot_menu::render::{paint_rows, LogicalLine};
        let long = "deepseek:deepseek-v4-flash ctx 1048576 $0.000000435/0.00000087 per M (est) and some extra trailing text to force a wrap";
        let wrapped = crate::content::text::wrap(long, 60);
        assert!(wrapped.len() > 1, "long option must wrap at 60 cols");
        let s = paint_rows(&[LogicalLine::new(wrapped.join("\n"))], Some(0), &[], 60);
        assert!(s.heights[0] > 1, "the option wraps: {:?}", s.heights);
        assert!(
            s.rows[0].contains("> "),
            "first row is the marker row: {:?}",
            s.rows[0]
        );
        assert!(
            s.rows[1..].iter().all(|r| !r.contains('>')),
            "continuations carry no cursor marker: {:?}",
            &s.rows[1..]
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
    // ui_draw_v2 P3; their cases live on in `draw::apply_tests`
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
    fn an_active_marker_beats_the_cursor() {
        use crate::slashdot_menu::render::{paint_rows, LogicalLine};
        let s = paint_rows(&[LogicalLine::new("x")], Some(0), &[0], 80);
        assert!(s.rows[0].contains("* x"), "{:?}", s.rows[0]);
        assert!(!s.rows[0].contains('>'), "{:?}", s.rows[0]);
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
        use crate::draw::{plan_apply, ApplyPolicy, DrawCache};
        use crate::repl::{LineEditor, Repl};
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
            let overlay = crate::repl::test_overlay(overlay, focus, width);
            let frame = Repl::compose_frame(editor, prompt, None, overlay, width, term_rows);
            let mut cache = DrawCache::new(term_rows);
            // The tracked row is the block's position (A3.2): the apply reads it
            // from the cache, so the test sets it the way content would.
            cache.note_lines(anchor.saturating_sub(1), term_rows);
            let s = plan_apply(&mut cache, &frame, ApplyPolicy::Frame, width, term_rows);
            let mut p = Parser::new(term_rows as u16, term_cols as u16, 1000);
            // A frame apply is relative to the cursor (row_accounting §6b), so
            // the cursor has to be where the tracked anchor says the block
            // goes. In production it gets there by content writing lines, which
            // is what these newlines stand in for.
            p.process(&onlcr(&"\n".repeat(anchor.saturating_sub(1))));
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
            // Rows now go through the painter, so the cursor's marker is on
            // the selected row rather than added by the test's raw strings.
            assert!(rows[11].trim_end() == "> one", "ov0: {:?}", rows[11]);
            assert!(rows[12].trim_end() == "  two", "ov1: {:?}", rows[12]);
            assert!(rows[13].trim_end() == "  three", "ov2: {:?}", rows[13]);
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
            assert_eq!(
                rows[18].trim_end(),
                "> one",
                "cursor marker: {:?}",
                rows[18]
            );
            assert!(
                rows[23].trim_end().ends_with("six"),
                "last overlay row at screen bottom: {:?}",
                rows[23]
            );
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
            let overlay: Vec<&str> = overlay.iter().map(|s| s.as_str()).collect();
            let (cr, cc, rows, anchor) =
                render_overlay_through_vt100(&e, 80, "> ", 24, 80, 8, &overlay, 50);
            // allocate: [input 1, divider 1, rows 14] of budget 16. The rows
            // element windows with `window_heights`: walk back over at most
            // half the budget (14/2 = 7), then fill forward. Cursor 50 →
            // start 44, so rows 44..=57 are shown.
            //
            // The window start moved from 43 to 44 with the split: the flat
            // overlay used the frame's `window` (prefer_context = h/2), the
            // rows element uses `window_heights` — the same primitive the
            // painter always used for rows. What it no longer does is window
            // twice: this is one window, over one allocation.
            assert_eq!(anchor, 8, "frame fits: no scroll");
            assert_eq!(rows[7].trim_end(), ">", "empty input: {:?}", rows[7]);
            assert!(rows[8].chars().all(|c| c == '─'), "divider: {:?}", rows[8]);
            assert!(
                rows[9].trim_end().ends_with("item 44"),
                "start: {:?}",
                rows[9]
            );
            assert!(
                rows[22].trim_end().ends_with("item 57"),
                "end: {:?}",
                rows[22]
            );
            assert!(
                rows[15].contains("> item 50"),
                "the cursor row is painted: {:?}",
                rows[15]
            );
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

    /// **R1: the input is a row between turns and no row at all mid-turn.**
    ///
    /// The contract has three cases, and the third is the one that keeps it from
    /// being "never draw the input": between turns an empty buffer still shows
    /// its row, because that row is what tells the reader where to type.
    ///
    /// Asserted through [`input_element`] rather than through a composed frame,
    /// because this is a property of the element's `(must, might)` and a test
    /// that replayed bytes to find it would pass just as well with the frame
    /// layer accidentally padding the gap back in.
    #[test]
    fn an_empty_input_is_a_row_between_turns_and_none_mid_turn() {
        let empty = LineEditor::new();

        // Between turns: the resting row, `must == 1`.
        let e = input_element(&empty, "> ", false, 60);
        assert_eq!(
            (e.must, e.might),
            (1, 1),
            "an idle REPL keeps its prompt row — it is where input goes"
        );

        // Mid-turn, still empty: nothing to submit, so no row and no claim.
        let e = input_element(&empty, "> ", true, 60);
        assert_eq!(
            (e.must, e.might),
            (0, 0),
            "an empty input mid-turn is not a row"
        );

        // Mid-turn with something typed: an ordinary input again, and `must` is
        // the caret's row so it can never be squeezed away entirely.
        let mut typed = LineEditor::new();
        typed.set("hello");
        let e = input_element(&typed, "> ", true, 60);
        assert_eq!(
            (e.must, e.might),
            (1, 1),
            "typed input is drawn mid-turn: the row appears on the first keystroke"
        );

        // And through the real pipeline, a zero-row element really yields a zero-row
        // frame — otherwise `must == might == 0` would only be a claim.
        use crate::render::{allocate, live_budget, render};
        let height = |streaming: bool| {
            let e = input_element(&empty, "> ", streaming, 60);
            let heights = allocate(&[e.extent()], live_budget(24));
            render(&[e], &heights, 60).height()
        };
        assert_eq!(height(true), 0, "a hidden input paints no row at all");
        assert_eq!(height(false), 1, "a resting input paints its row");
    }
}

#[cfg(test)]
mod confirm_tests {
    //! The three things the pty probe cannot reach, because each needs a real
    //! `PromptSession` (and therefore an `AppState`): the confirm **appending**
    //! below an open menu, the `Esc` ladder's new top rung, and the option
    //! rows' starvation floor. See doc/confirm_as_menu_261002.md §9.
    use super::*;
    use crate::confirm::{ConfirmBridge, ConfirmDecision, ConfirmRequest};
    use crate::slashdot_menu::render::LogicalLine;

    fn batch_state() -> Arc<AppState> {
        // Same helper the prompt tests use; duplicated rather than shared
        // because that one is private to the `tests` module above.
        let settings = crate::settings::SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_reasoning_level: None,
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            models: Default::default(),
            path: None,
        };
        AppState::for_test(settings)
    }

    fn session() -> PromptSession {
        PromptSession::new(None, &batch_state())
    }

    /// A confirm arriving while a menu is open must land **below** it, with the
    /// prompt and the menu untouched — the menu is driven from that prompt, so
    /// replacing the overlay would strand the very thing the user is looking at.
    #[test]
    fn a_confirm_appends_below_an_open_menu() {
        let mut sess = session();
        sess.overlay = ActiveOverlay::V25Menu {
            plan: crate::slashdot_menu::render::RenderPlan {
                table_header: None,
                detail: vec![LogicalLine::new("pick a model")],
                rows: std::sync::Arc::new(vec![LogicalLine::new("menu-row")]),
                selected: Some(0),
                active_rows: vec![],
            },
        };
        let bridge = ConfirmBridge::new();
        let (reply, _rx) = tokio::sync::oneshot::channel();
        sess.open_confirm(
            ConfirmRequest {
                tool: "bash".into(),
                header: "Confirm tool BASH call?".into(),
                subject: "tool BASH".into(),
                reply,
            },
            bridge.port.approvals().clone(),
        );

        // Compose the whole stack the way `paint` does, then read the frame:
        // element order is the row order, so "where in the stack" is exactly
        // "where on screen".
        let mut els = overlay_elements(&sess.overlay, 40);
        els.extend(sess.confirm.as_ref().unwrap().elements(40));
        let extents: Vec<Extent> = els.iter().map(Element::extent).collect();
        let heights = crate::render::allocate(&extents, 40);
        let frame = crate::render::render(&els, &heights, 40);
        let joined = (0..frame.height())
            .map(|i| frame.row(i).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let menu_at = joined.find("menu-row").expect("the menu row survives");
        let confirm_at = joined
            .find("Confirm tool BASH call?")
            .expect("the dialog is composed at all");
        assert!(
            menu_at < confirm_at,
            "the dialog goes below the menu, not over it:\\n{joined}"
        );
    }

    /// `Esc` is the dialog's first: it must resolve to a denial rather than
    /// falling through to the menu ladder or the turn cancel.
    #[test]
    fn esc_resolves_the_confirm_before_anything_else() {
        let mut sess = session();
        let bridge = ConfirmBridge::new();
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        sess.open_confirm(
            ConfirmRequest {
                tool: "bash".into(),
                header: "Confirm tool BASH call?".into(),
                subject: "tool BASH".into(),
                reply,
            },
            bridge.port.approvals().clone(),
        );
        let decision = sess.confirm_key(key(KeyCode::Esc));
        assert_eq!(decision, Some(ConfirmDecision::Deny));
        sess.clear_confirm();
        assert_eq!(
            rx.try_recv().ok(),
            Some(ConfirmDecision::Deny),
            "the waiting tool is told Deny, not left hanging"
        );
    }

    /// An unmapped key is a denial too. A consent prompt that treats a stray
    /// keystroke as consent is not one.
    #[test]
    fn an_unmapped_key_denies_rather_than_accepting() {
        let mut sess = session();
        let bridge = ConfirmBridge::new();
        let (reply, _rx) = tokio::sync::oneshot::channel();
        sess.open_confirm(
            ConfirmRequest {
                tool: "bash".into(),
                header: "Confirm tool BASH call?".into(),
                subject: "tool BASH".into(),
                reply,
            },
            bridge.port.approvals().clone(),
        );
        assert_eq!(
            sess.confirm_key(key(KeyCode::Char('z'))),
            Some(ConfirmDecision::Deny)
        );
    }

    /// A dialog that opens on "run it" gets muscle-memory'd into running
    /// things, so the default row is *no*.
    #[test]
    fn the_dialog_opens_on_no() {
        let mut sess = session();
        let bridge = ConfirmBridge::new();
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        sess.open_confirm(
            ConfirmRequest {
                tool: "bash".into(),
                header: "Confirm tool BASH call?".into(),
                subject: "tool BASH".into(),
                reply,
            },
            bridge.port.approvals().clone(),
        );
        assert_eq!(
            sess.confirm_key(key(KeyCode::Enter)),
            Some(ConfirmDecision::Deny),
            "Enter on the default row must deny"
        );
        assert_eq!(rx.try_recv().ok(), Some(ConfirmDecision::Deny));
    }

    /// The option rows carry `must = 3`, so a starved budget cannot squeeze the
    /// choices away — the whole point of the floor. Verified through
    /// `allocate`, the same way the header/rows priority test does.
    #[test]
    fn the_option_rows_survive_a_starved_budget() {
        let bridge = ConfirmBridge::new();
        let (reply, _rx) = tokio::sync::oneshot::channel();
        let prompt = crate::confirm::ConfirmPrompt::new(
            ConfirmRequest {
                tool: "bash".into(),
                header: "Confirm tool BASH call?".into(),
                subject: "tool BASH".into(),
                reply,
            },
            bridge.port.approvals().clone(),
        );
        let els = prompt.elements(40);
        let rows_el = els.last().expect("the rows element");
        assert_eq!(
            rows_el.extent().must,
            3,
            "all three options are pinned, not just one row"
        );
        // Under a budget that cannot fit the header, the rows still get their
        // three: `allocate` serves every `must` before any `might`.
        let all: Vec<_> = els.iter().map(Element::extent).collect();
        let heights = crate::render::allocate(&all, 3);
        assert_eq!(
            heights[heights.len() - 1],
            3,
            "a 3-row budget still shows every option"
        );
    }

    fn key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// The session owns the dialog now, so a turn ending with one open drops it
    /// — and with it the reply sender, without ever having answered. The waiting
    /// tool must read that as a denial instead of parking forever.
    ///
    /// Asserted through the port, not the raw channel: a dropped sender
    /// surfaces as *disconnected*, and it is `ConfirmPort::request` that turns
    /// that into `Deny`. Testing the channel would pin the wrong layer.
    #[tokio::test]
    async fn a_dropped_confirm_denies_rather_than_hanging() {
        let mut bridge = ConfirmBridge::new();
        let port = bridge.port.clone();
        let ask = tokio::spawn({
            let port = port.clone();
            async move { port.request("bash", "Confirm tool BASH call?".into()).await }
        });
        let req = bridge.requests.recv().await.expect("a request arrives");
        let mut sess = session();
        sess.open_confirm(req, port.approvals().clone());
        assert!(sess.confirm_open());
        // The turn is cancelled: the session goes, dialog and all.
        drop(sess);
        assert_eq!(
            ask.await.unwrap(),
            ConfirmDecision::Deny,
            "a dialog torn down unanswered must fail closed"
        );
    }
}
