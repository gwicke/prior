//! The two readouts the user sees without asking the model anything: the
//! transient status line under each content block, and the `/stat` overlay.
//!
//! Both are pure. [`status_text`] shapes a [`BarStatus`] push (raw facts) into
//! the pair `ui::bar::status_bar` fits — both halves run inside the bar's own
//! render (step 4f-a); [`session_stats_lines`] takes the session's stored
//! facts and returns lines. Neither reads a store or holds state — the
//! caller resolves the context window ([`resolved_context_limit`]) and the
//! session metadata and hands them over — which is what makes them testable
//! without a terminal, and why `status_bar_tests` can assert on the rendered
//! bar rather than on paint.

use super::SessionUsage;
use crate::model_catalog;

/// Session-level metadata shown in the `/stat` overlay header.
///
/// Captures the same fields that the `/session <id>` action-menu header
/// displays (name, message_count, age/created/last update). Built from
/// [`crate::session::repo::SessionMetadata`] at the call site so this
/// function stays pure / easily testable.
#[derive(Debug, Clone)]
pub struct SessionStatsInfo {
    /// The session UUID. Always shown: it is the one identifier every session
    /// has, and it is what `/session <id>` takes.
    pub id: String,
    /// User-assigned name, if any. `None` (or empty) drops the line — a name
    /// the user never set is not the same fact as a name that happens to be
    /// blank, and printing `name:` with nothing after it says neither.
    pub name: Option<String>,
    /// Working directory recorded in the session header. `None` (or empty)
    /// drops the line rather than printing a blank path: the header's `cwd`
    /// is a required field, but a hand-written or foreign session file can
    /// still leave it empty, and a bare `cwd:` reads as a bug rather than
    /// as "unknown".
    pub cwd: Option<String>,
    /// ISO 8601 creation timestamp (from the session header).
    pub created: String,
    /// ISO 8601 timestamp of the most recent user message, if any.
    /// When `None`, falls back to `created` for display.
    pub last_updated: Option<String>,
    /// Total number of message entries (user + assistant + toolResult).
    /// Mirrors the "messages" count shown in `/session <id>`.
    pub message_count: usize,
    /// Number of user-role message entries (= turn count).
    /// Mirrors the "turns" count shown in `/session <id>`.
    pub user_message_count: usize,
}

/// What the status bar shows, as **raw data**, assembled at render (4h-g4
/// part 1; design §UI: "the bar's held state is raw data, formatted by the
/// bar").
///
/// The bar's line is `● session | provider:model thinking 34% 78k` — the
/// mid-turn readout (doc/row_accounting.md, zone 2) — carried here as *facts*,
/// never as shaped text. [`bar_status`] builds this value at render from the
/// stores that hold each fact (the model/thinking off `AppState`, the identity
/// off the per-cycle view); nothing is pushed, so a mid-turn config commit
/// shows at the next paint. The context figure is deliberately absent: it is
/// read off the agent's shared usage store at render and [`status_text`] takes
/// it as an argument. The shaping (name fallback to the opening-message
/// preview, the field list, the drop order) is `status_text`'s; the fit is
/// `ui::bar::status_bar`'s.
///
/// A struct rather than four more positional arguments because those four are
/// two strings and two `Option`s in a row: nothing at the call site says which
/// `Option<String>` is the thinking level and which the first message, and
/// getting that wrong compiles. The field names are the documentation. Fields
/// that are unknown are dropped rather than shown empty.
#[derive(Default, Clone)]
pub struct BarStatus {
    /// The session name, when it has one. Wins over [`Self::first_message`]:
    /// a name is a thing the user chose.
    pub session_name: Option<String>,
    /// The session's opening message — stands in for an unnamed session, so
    /// the line still says *which* session this is, not just which model.
    pub first_message: Option<String>,
    pub provider: String,
    pub model: String,
    /// The active thinking level, if any.
    pub thinking: Option<String>,
    /// The context window in force for the current model, resolved: the
    /// catalog overlaid with any stored per-model override.
    ///
    /// Supplied by the caller rather than looked up here: `status_text` is a
    /// pure function with no store to read, and the raw-catalog lookup it used
    /// to do divided the percentage by OpenRouter's number even when the user
    /// had overridden it. `None` renders the bare token count, which is what
    /// an unknown window always did.
    pub context_limit: Option<u64>,
}
/// The context window in force for the current model, resolved: the catalog
/// overlaid with any stored per-model override.
///
/// Read from `AppState::resolved_models` rather than looked up per status-bar
/// paint, so the percentage uses the number the app actually enforces and the
/// prompt-side and mid-turn bars cannot disagree. The mid-turn bar gets it
/// through `plan_rebuild`, which compares this value to decide whether to
/// rebuild — hence the window's place in that gate.
pub fn resolved_context_limit(app_state: &crate::app_state::AppState) -> Option<u64> {
    let qualified = app_state.model();
    let (provider, model) = qualified.split_once(':')?;
    app_state
        .resolved_models()
        .iter()
        .find(|e| {
            let (p, m) = e.qualified_parts();
            p.eq_ignore_ascii_case(provider) && m.eq_ignore_ascii_case(model)
        })
        .and_then(|e| e.context_length)
        .filter(|max| *max > 0)
}

/// Assemble the bar's raw data from the **stores** (4h-g4 part 1): the model
/// in force, the thinking level, the resolved context window, and the session
/// identity — all read live off `AppState`'s sync accessors at render. The
/// bar element calls this each paint, so a mid-turn `/reasoning` or `/model`
/// commit (which lands on `AppState`) shows at the next paint with no push.
pub fn bar_status(app_state: &crate::app_state::AppState) -> BarStatus {
    let qualified = app_state.model();
    let (provider, model) = match qualified.split_once(':') {
        Some((p, m)) => (p.to_string(), m.to_string()),
        None => (String::new(), qualified),
    };
    let (session_name, first_message) = app_state.bar_identity();
    BarStatus {
        session_name,
        first_message,
        provider,
        model,
        thinking: app_state.reasoning(),
        context_limit: resolved_context_limit(app_state),
    }
}

/// The status line's content, after shaping and before the fit: what names the
/// session, and the model group as separate fields. [`status_text`] below
/// builds this from the raw [`BarStatus`]; the fit (`ui::bar::status_bar`)
/// turns it into a row at a width. Both halves run inside the bar's own
/// render (step 4f-a).
///
/// **Data, not drawing, and deliberately not the rendered bar.** Nothing here
/// needs to know how wide the terminal is, which is what lets the two halves
/// live on opposite sides of the `agent` → `ui` seam instead of the width
/// being passed in from the layer that has no business knowing it.
///
/// The group's fields are kept separate — rather than pre-joined into one
/// string — because a narrow terminal has to give up **whole** fields, from the
/// right, and a joined string can only be cut mid-word. The shaping order is
/// that drop order, so it is carried here rather than re-decided at the fit.
#[derive(Default, Clone)]
pub struct StatusText {
    /// The session name, or its opening message standing in for it.
    pub name: String,
    /// The model group, in the order a narrow terminal should give it up.
    pub group: Vec<String>,
}

/// The shaping half of the bar's drawing: raw [`BarStatus`] to the shaped pair
/// `status_bar` fits — the name (session name, else the opening
/// message's preview) and the model group in drop order.
///
/// Run by the bar's own render (step 4f-a), which is why it takes the raw
/// data by reference: nothing here is width-dependent, and the fit that is
/// lives on the other side. Built from the same pieces `session_stats_lines`
/// uses — the caller for the resolved context window, its own token count for
/// the usage — so the two cannot disagree about what the context figure
/// means. That holds because a per-model `context_length` edit moves both: the
/// prompt-side bar re-reads it every cycle, and `plan_rebuild` compares it so
/// the mid-turn agent is rebuilt with the new one.
///
/// `context_tokens` is a parameter, not a field (4h-g part 2): it is the one
/// figure that moves mid-turn, so it comes off the agent's live usage store
/// rather than the pushed `status`, which no one re-pushes per completion.
/// `0` means "not known yet" and drops the figure rather than showing a zero —
/// the window before the first completion has reported.
pub fn status_text(status: &BarStatus, context_tokens: u64) -> StatusText {
    let mut fields: Vec<String> = Vec::new();
    if !status.provider.is_empty() || !status.model.is_empty() {
        fields.push(format!("{}:{}", status.provider, status.model));
    }
    if let Some(level) = status
        .thinking
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
    {
        fields.push(level.to_string());
    }
    if context_tokens > 0 {
        // The percentage needs the catalog's context window, and a model
        // missing from it is normal (a local endpoint, a new release). So the
        // count stands alone rather than the whole figure disappearing: no
        // window known means no percentage, not no context figure.
        let window = status.context_limit.filter(|max| *max > 0);
        fields.push(match window {
            Some(max) => {
                let pct = (context_tokens as f64 / max as f64) * 100.0;
                format!("{pct:.0}% {}", compact_tokens(context_tokens))
            }
            None => compact_tokens(context_tokens),
        });
    }

    // With no name to show, the session's opening message stands in: the line
    // should still say *which* session this is, not just which model.
    let name = status
        .session_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| {
            status
                .first_message
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(message_preview)
        })
        .unwrap_or_default();
    StatusText {
        name,
        group: fields,
    }
}

/// A first message as a single line, flattened, for the status line to truncate
/// to whatever the bar has left.
///
/// Whitespace runs collapse, because a message that opens with a blank line, an
/// indent, or a code fence would otherwise show as nothing at all. Escapes are
/// stripped: this is untrusted text from a session file, and a pasted escape
/// sequence should not be able to colour or reposition the bar.
///
/// No width cap of its own — `status_line` truncates to the columns the model
/// group left, and a cap here would be a second opinion that is wrong exactly
/// when the bar is widest. What used to be a fixed 25 columns showed a third of
/// a long message on a 120-column bar and left a hole before the pipe.
fn message_preview(msg: &str) -> String {
    let flat = msg.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::ansi::strip_ansi(&flat)
}

#[cfg(test)]
mod status_bar_tests {
    use super::{message_preview, status_text, BarStatus};
    // The separator the fit actually used, not a copy of the string: these tests
    // are about where the fit put it, so the constant has to be the real one.
    use crate::ui::bar::BAR_SEPARATOR as SEPARATOR;

    /// The whole bar: the raw push shaped and fitted — exactly the two calls
    /// the bar's own render makes (step 4f-a), spelled out here so the tests
    /// can pass borrowed literals where the store carries owned data.
    ///
    /// Every test here is really about the *pair* — `status_text` builds the
    /// fields and `ui::bar::status_bar` fits them to a width — so they go through
    /// one seam-crossing helper rather than each spelling out the two calls. They
    /// live here rather than beside `status_bar` because most of them are as much
    /// about `BarStatus`'s data (which field is dropped first, what an unknown
    /// model does, what an unnamed session shows) as about the fitting.
    fn bar(
        session_name: Option<&str>,
        first_message: Option<&str>,
        (m, context_tokens): (BarStatus, u64),
        width: usize,
        streaming: bool,
    ) -> String {
        let status = BarStatus {
            session_name: session_name.map(str::to_string),
            first_message: first_message.map(str::to_string),
            ..m
        };
        // `context_tokens` is the one live fact (4h-g part 2), so it arrives
        // alongside the pushed facts — destructured here exactly as the bar's
        // own render pulls it from the live store beside `Content::bar`.
        crate::ui::bar::status_bar(&status_text(&status, context_tokens), width, streaming)
    }

    /// A [`BarStatus`] for the tests, identity fields unset — the `bar` helper
    /// takes those. Positional on purpose: every test wants the same five
    /// things, and the field names at each of the twenty-odd call sites would
    /// be noise. The names are still checked, because the layout test is about
    /// the order they render in.
    ///
    /// `context_tokens` rides *beside* the struct rather than inside it (4h-g
    /// part 2): it is the one fact the bar no longer pushes, so the tests pass
    /// it the same way production supplies it — alongside the pushed facts, not
    /// as one of them. Returning the pair keeps the twenty-odd `model(...)`
    /// call sites unchanged.
    fn model(
        provider: &str,
        model: &str,
        thinking: Option<&str>,
        context_tokens: u64,
        context_limit: Option<u64>,
    ) -> (BarStatus, u64) {
        (
            BarStatus {
                provider: provider.to_string(),
                model: model.to_string(),
                thinking: thinking.map(str::to_string),
                context_limit,
                ..Default::default()
            },
            context_tokens,
        )
    }

    /// The dot never touches the text: there is always a column of air after it.
    ///
    /// The text is right-aligned, so the gap is whatever padding is left over —
    /// which on a nearly-full bar is nothing, and the summary then reads as part
    /// of the dot's cell. This is the case that needs the guaranteed column, and
    /// it is invisible at any width where there happens to be padding.
    #[test]
    fn the_dot_always_has_a_column_of_air_after_it() {
        // Widths chosen to leave the text exactly filling the row: at each of
        // these the padding computes to zero, so only the gap column separates
        // the dot from the text.
        let text = "run the command | llamacpp:test";
        for extra in 0..6_usize {
            let w = text.chars().count() + 2 + extra;
            let s = bar(
                None,
                None,
                model("llamacpp", "test", None, 0, None),
                w,
                true,
            );
            let visible = crate::ansi::strip_ansi(&s);
            assert_eq!(display(&s), w, "width {w}: one row: {visible:?}");
            assert!(
                visible.contains("\u{25cf} "),
                "width {w}: a space must follow the dot: {visible:?}"
            );
            assert!(
                !visible.contains("\u{25cf}r"),
                "width {w}: the dot must not abut the text: {visible:?}"
            );
        }
    }

    /// A planned bar's visible width — its contract with the terminal.
    fn display(s: &str) -> usize {
        crate::row_count::display_width_ansi(s)
    }

    /// The bar is a full row: the background is set before the padding, the text
    /// sits against the right margin, and the visible width is exactly the
    /// terminal's. The width is what makes it a bar rather than a highlighted
    /// fragment, and the right margin is what makes it read as a figure rather
    /// than the start of a line.
    #[test]
    fn the_bar_fills_the_row_and_right_aligns_the_text() {
        let s = bar(
            Some("main"),
            None,
            model("llamacpp", "test", None, 0, None),
            40,
            true,
        );
        assert_eq!(
            crate::row_count::display_width_ansi(&s),
            40,
            "the bar should be exactly one row wide: {s:?}"
        );
        // The dot cell leads, then padding, then the text against the right
        // margin — the right margin is the point, the dot is the price of it.
        let visible = crate::ansi::strip_ansi(&s);
        assert!(
            visible.starts_with(" \u{25cf}  "),
            "the bar should lead with the dot cell: {visible:?}"
        );
        assert!(
            visible.ends_with("main | llamacpp:test"),
            "the text should sit against the right margin: {visible:?}"
        );
        // The background has to be set *before* the padding, or the bar is a
        // highlighted fragment with default-coloured cells around it.
        let padding = visible.find("main |").expect("the text");
        let before = &s[..s.find("main |").expect("the text")];
        assert!(
            before.contains("\x1b[48;5;237m"),
            "the background must precede the padding: {s:?}"
        );
        assert_eq!(padding, visible.len() - "main | llamacpp:test".len());
        assert!(s.ends_with("\x1b[0m"), "the bar must be reset: {s:?}");
    }

    /// The bar is truncated to the terminal, so it never moves the cursor down
    /// without a newline — a wrapping bar would be a row movement the frame
    /// never accounted for. Zero rows is the assertion, not one: writing a bar
    /// that fits leaves the cursor *on* the row.
    #[test]
    fn the_bar_never_moves_the_cursor_down() {
        let long = "x".repeat(200);
        for w in [1_usize, 10, 40, 200] {
            let s = bar(Some(&long), None, model("p", "m", None, 0, None), w, true);
            assert_eq!(
                crate::row_count::rows_of(&s, w),
                0,
                "width {w}: the bar should not wrap: {s:?}"
            );
        }
    }

    /// A long line is cut to the terminal, not spilled past it.
    #[test]
    fn a_long_bar_is_truncated_to_the_terminal() {
        let s = bar(
            Some(&"n".repeat(100)),
            None,
            model("p", "m", None, 0, None),
            20,
            true,
        );
        assert_eq!(crate::row_count::display_width_ansi(&s), 20, "{s:?}");
        assert!(crate::ansi::strip_ansi(&s).contains('…'), "{s:?}");
    }

    /// With no session name, the opening message stands in — so the line still
    /// says which session this is. A name always wins: the user chose it.
    #[test]
    fn an_unnamed_session_shows_its_first_message() {
        let s = bar(
            None,
            Some("fix the flaky resize test"),
            model("llamacpp", "test", None, 0, None),
            60,
            true,
        );
        let visible = crate::ansi::strip_ansi(&s);
        assert!(visible.contains("fix the flaky resize test"), "{visible:?}");
        let named = bar(
            Some("main"),
            Some("fix the flaky resize test"),
            model("llamacpp", "test", None, 0, None),
            60,
            true,
        );
        let named_visible = crate::ansi::strip_ansi(&named);
        assert!(named_visible.contains("main"), "{named_visible:?}");
        assert!(
            !named_visible.contains("fix the flaky"),
            "a named session must not also show the message: {named_visible:?}"
        );
    }

    /// The whole shape in one assertion: one pipe, the model fields in order
    /// separated by single spaces, no labels.
    ///
    /// The labels are the substance of this, not the spacing. `ctx 34% · 78k`
    /// spends eight columns saying what three digits and a suffix already say,
    /// and on a narrow bar those columns are the difference between a session
    /// name that fits and one that is ellipsised.
    #[test]
    fn the_model_group_is_unlabelled_and_space_separated() {
        // `ollama:llama3` is in the curated static fallback with an 8k window,
        // and ollama needs no API key, so the percentage is real here. The
        // `None` window arm — a local endpoint or a new release — is a different
        // shape and has its own test below.
        let s = bar(
            Some("main"),
            None,
            model("ollama", "llama3", Some("high"), 5_000, Some(8_192)),
            60,
            true,
        );
        let visible = crate::ansi::strip_ansi(&s);
        assert!(
            visible.ends_with("main | ollama:llama3 high 61% 5k"),
            "one pipe, model then level then the figure, single spaces: {visible:?}"
        );
        assert_eq!(
            visible.matches('|').count(),
            1,
            "exactly one separator: {visible:?}"
        );
        assert!(
            !visible.contains("ctx") && !visible.contains('·'),
            "no labels left: {visible:?}"
        );
    }

    /// A model the catalog does not know: the count stands alone rather than the
    /// whole figure disappearing, because no window known means no percentage,
    /// not no context figure.
    #[test]
    fn an_unknown_model_still_shows_its_token_count() {
        let s = bar(
            Some("main"),
            None,
            model("llamacpp", "test", Some("high"), 78_000, None),
            60,
            true,
        );
        let visible = crate::ansi::strip_ansi(&s);
        assert!(
            visible.ends_with("main | llamacpp:test high 78k"),
            "the count alone when there is no window to take a percentage of: {visible:?}"
        );
    }

    /// The name fills the gap between the dot and the pipe, and is ellipsised
    /// when it does not fit.
    ///
    /// "Fills" is the part that is easy to get wrong: a fixed preview width
    /// truncates early and leaves a hole before the pipe, so the bar reads as
    /// though the name were short. The assertion is that the name reaches the
    /// pipe — the row is full to the last text column on both sides.
    #[test]
    fn the_name_fills_the_space_the_model_group_leaves() {
        let group = "ollama:llama3 high 61% 5k";
        for w in [40_usize, 60, 100] {
            let long = "a".repeat(400);
            let s = bar(
                Some(&long),
                None,
                model("ollama", "llama3", Some("high"), 5_000, Some(8_192)),
                w,
                true,
            );
            let visible = crate::ansi::strip_ansi(&s);
            assert_eq!(display(&s), w, "width {w}: the bar is one row: {visible:?}");
            assert!(
                visible.ends_with(group),
                "width {w}: the model group keeps the right margin: {visible:?}"
            );
            // The bar is `\u{25a0} name | group` with the dot cell in front, so
            // the name is what is left once both ends are taken off.
            let name = visible
                .trim_start()
                .strip_prefix('\u{25cf}')
                .and_then(|r| r.strip_suffix(group))
                .and_then(|r| r.strip_suffix(SEPARATOR))
                .unwrap_or_else(|| panic!("width {w}: cannot find the name in {visible:?}"));
            assert_eq!(
                display(name),
                w - 2 - group.len() - SEPARATOR.len(),
                "width {w}: the name should take the whole gap: {visible:?}"
            );
            assert!(name.ends_with('…'), "width {w}: {name:?}");
        }
    }

    /// On a narrow terminal the model group gives way **as whole fields**, from
    /// the right: the context figure first, then the thinking level, and the
    /// model name last.
    ///
    /// Field-at-a-time rather than a character cut, because the alternative is a
    /// bar ending in `78` or `llamacpp:te` — figures and identifiers cut in half
    /// read as data, not as truncation. The order is the priority order: the
    /// figure changes every turn, so losing it is unremarkable, while losing the
    /// model name leaves a bar that cannot say who is answering.
    #[test]
    fn a_narrow_bar_drops_whole_model_fields_from_the_right() {
        // The only shapes the group may take, widest first. Anything else is a
        // character cut or a field in the wrong order.
        const ALLOWED: [&str; 4] = [
            "ollama:llama3 high 61% 5k",
            "ollama:llama3 high",
            "ollama:llama3",
            "",
        ];
        // Owned, because the borrow is out of the `visible` it came from by the
        // time the next iteration rebinds it.
        let mut seen: Vec<String> = Vec::new();
        // Narrow enough that the group itself has to give. Above ~30 columns the
        // *name* is the only thing shrinking, so a wider sweep would see one
        // shape and pass without ever exercising the drop order.
        // Downward, so `seen` reads in the order the fields drop.
        for w in (4_usize..=40).rev() {
            // No name and no message on purpose: with a name present the bar is
            // `name | group` and the *name* is what shrinks first, so a sweep
            // wide enough to matter would read a mixture of the two and could not
            // say which one it was looking at. The name's own give-way has its
            // own test.
            let s = bar(
                None,
                None,
                model("ollama", "llama3", Some("high"), 5_000, Some(8_192)),
                w,
                true,
            );
            let visible = crate::ansi::strip_ansi(&s);
            assert_eq!(display(&s), w, "width {w}: one row: {visible:?}");
            // The dot cell leads and the padding is all that is left over, so
            // what remains in the middle is the group and nothing else.
            let group = visible
                .trim()
                .strip_prefix('\u{25cf}')
                .unwrap_or(visible.trim())
                .trim();
            assert!(
                ALLOWED.contains(&group),
                "width {w}: the group must be whole fields, got {group:?} in {visible:?}"
            );
            if seen.last().map(String::as_str) != Some(group) {
                seen.push(group.to_string());
            }
        }
        // The transitions are the point: every field was exercised on the way
        // down, so a regression that drops the wrong one is visible here.
        assert_eq!(
            seen,
            ALLOWED.map(str::to_string),
            "each field should drop in turn as the terminal narrows"
        );
    }

    /// Neither present: no leading separator, and the model still identifies the
    /// line.
    #[test]
    fn no_name_and_no_message_still_shows_the_model() {
        let s = bar(
            None,
            None,
            model("llamacpp", "test", None, 0, None),
            40,
            true,
        );
        let visible = crate::ansi::strip_ansi(&s);
        assert!(visible.ends_with("llamacpp:test"), "{visible:?}");
        assert_eq!(display(&s), 40, "{visible:?}");
    }

    /// The dot is the spinner's replacement: green while a response streams,
    /// the bar's own grey when it is not. Two states because the prompt-side
    /// carrier will show the bar between turns too, and the dot is what will
    /// tell "working" from "idle" then.
    ///
    /// The order of the escape and the glyph is the substance of this test, not
    /// a detail: an `SGR` colours the cells written *after* it, so a colour
    /// emitted after the dot paints the dot in the default foreground and applies
    /// the colour to the padding. That is not a subtle version of the bug — the
    /// green lands on nothing, and the dot reads as default bright white.
    #[test]
    fn the_dot_is_green_while_streaming_and_grey_when_not() {
        let streaming = bar(Some("main"), None, model("p", "m", None, 0, None), 40, true);
        let idle = bar(
            Some("main"),
            None,
            model("p", "m", None, 0, None),
            40,
            false,
        );
        assert!(
            streaming.contains("\x1b[38;5;41m"),
            "a streaming bar should carry a green dot: {streaming:?}"
        );
        assert!(streaming.contains('\u{25cf}'), "and a dot: {streaming:?}");
        assert!(
            !idle.contains("\x1b[38;5;41m"),
            "an idle bar should not: {idle:?}"
        );
        assert!(
            idle.contains("\u{25cf}"),
            "but it still has a dot: {idle:?}"
        );
        // Same width either way: the dot is a cell, not a different layout.
        assert_eq!(display(&streaming), display(&idle));
        // And the colour has to land *on* the glyph. The escape used to be
        // emitted after it, so the dot was painted in the default foreground and
        // the green was applied to nothing at all — and every assertion above
        // passed straight over that, because they only asked whether the escape
        // was present.
        let glyph_at = streaming.find('\u{25cf}').expect("the dot");
        let green_at = streaming.find("\x1b[38;5;41m").expect("the green");
        assert_eq!(
            glyph_at,
            green_at + "\x1b[38;5;41m".len(),
            "the green must be emitted immediately before the dot, not after it: {streaming:?}"
        );
    }

    /// The bar's appearance must not depend on what was written above it.
    ///
    /// `dim` is an SGR *attribute* and `38;5;245m` sets a colour, not an
    /// attribute — so a bar written while streamed reasoning has the terminal in
    /// dim mode rendered dimmed, and one written after a plain block did not.
    /// A leading full reset is what makes the bar self-contained.
    #[test]
    fn the_bar_resets_first_so_ambient_dim_cannot_reach_it() {
        let s = bar(Some("main"), None, model("p", "m", None, 0, None), 40, true);
        let body = s.trim_start_matches("\x1b[0m");
        assert!(
            s.starts_with("\x1b[0m"),
            "the bar must open with a reset: {s:?}"
        );
        assert!(
            body.starts_with("\x1b[48;5;237m"),
            "and only then set its own colours: {body:?}"
        );
    }

    /// The preview is one line and collapsed. It is *not* capped here — the bar
    /// truncates it to the columns the model group left, so a preview that came
    /// back already cut would be a second opinion, and a wrong one on exactly the
    /// widest bars.
    #[test]
    fn the_preview_is_one_flattened_line_and_caps_itself_nowhere() {
        assert_eq!(message_preview("fix the bug"), "fix the bug");
        // A leading blank line and indentation must not make it show nothing,
        // and a multi-line message shows as its first line only — a preview is
        // one row, and the bar is one row.
        assert_eq!(
            message_preview("\n\n    fn main() {\n        todo!()\n    }"),
            "fn main() { todo!() }",
            "the first non-empty line, with its indentation collapsed away"
        );
        assert_eq!(
            message_preview(&"a".repeat(40)),
            "a".repeat(40),
            "the whole line comes through; `status_line` is what cuts it"
        );
    }

    /// …and the bar does cut it, at whatever width is left, ellipsised and never
    /// mid-glyph.
    #[test]
    fn a_long_message_is_cut_to_the_bar_with_an_ellipsis() {
        // No name and no model group at all: the preview has the whole row, so
        // the cut is the bar's doing and nothing else.
        let s = bar(
            None,
            Some(&"a".repeat(80)),
            model("", "", None, 0, None),
            20,
            true,
        );
        let visible = crate::ansi::strip_ansi(&s);
        let text = visible.trim().strip_prefix('\u{25cf}').unwrap().trim_end();
        assert_eq!(display(&s), 20, "{visible:?}");
        assert!(
            text.ends_with('…'),
            "a cut preview should be ellipsised: {text:?}"
        );
        // 20 columns less the dot cell: with no model group there is no separator
        // to pay for, so the name gets the whole remainder.
        assert_eq!(display(text), 18, "{visible:?}");
        // A wide glyph is counted as two columns, not cut mid-character.
        let wide = bar(
            None,
            Some(&"\u{4f60}\u{597d}".repeat(20)),
            model("", "", None, 0, None),
            20,
            true,
        );
        let text = crate::ansi::strip_ansi(&wide);
        // `trim` first: the dot cell is a space and a glyph, and the space would
        // otherwise be the first character the assertion looks at.
        // `trim` again after the strip: the text is right-aligned, so the
        // padding the dot cell leaves is between the glyph and the name.
        let text = text.trim().strip_prefix('\u{25cf}').unwrap().trim();
        assert!(
            text.chars()
                .all(|c| matches!(c, '\u{4f60}' | '\u{597d}' | '\u{2026}')),
            "no half glyphs: {text:?}"
        );
    }

    /// The preview comes out of a session file, so it is untrusted: an escape
    /// sequence in it must not colour or reposition the bar.
    #[test]
    fn the_preview_strips_escapes() {
        let p = message_preview("\x1b[2J\x1b[31mred\x1b[0m");
        assert_eq!(p, "red");
        assert!(!p.contains('\u{1b}'), "{p:?}");
    }
}

/// `78k` / `1.2M` — the same abbreviations `/stat` uses for token counts.
fn compact_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// The `/stat` lines for a session.
///
/// `stored` is the model's per-model record and `live` its current params;
/// both are `None`/empty when unavailable. Together they make this the place
/// you look when a request is rejected: the provider's error names the limit it
/// enforced, and these say what was actually sent and what the resolved
/// context window is — the two facts needed to see why.
///
/// Both token figures are read **live** off the agent's shared usage store
/// (4h-g part 2): `usage` for the session/cost/cache totals, `context_tokens`
/// for the estimated next-turn context — so `/stat` opened mid-turn is current,
/// not a loop-cycle snapshot.
pub fn session_stats_lines(
    provider: &str,
    model: &str,
    usage: SessionUsage,
    session: Option<SessionStatsInfo>,
    context_tokens: u64,
    stored: Option<&crate::model_settings::ModelSettings>,
    live: &crate::params::ModelParams,
) -> Vec<String> {
    // The *resolved* view, not the raw catalog entry: a Ctrl-E'd
    // `context_length` override is what the app actually enforces, so quoting
    // OpenRouter's published number here would contradict the `/model` header
    // on the same fact with nothing to explain the difference.
    let entry = model_catalog::resolved_entry(&format!("{provider}:{model}"), stored);
    let ctx_len = entry.as_ref().and_then(|e| e.context_length);

    let mut lines: Vec<String> = Vec::new();

    // ── session section ─────────────────────────────────────────────────
    match session {
        Some(s) => {
            // The id, not the name-or-id fallback the `/session` header uses:
            // here the name gets its own line below, so folding it into the
            // headline would print the same string twice and leave the reader
            // unable to tell a name from a UUID.
            lines.push(format!("session: {}", s.id));
            if let Some(name) = s.name.as_deref().filter(|n| !n.is_empty()) {
                lines.push(format!("  name: {}", name));
            }
            // The full path, not the session listing's `relativize_for_display`
            // form: the overlay lists sessions from *every* cwd, so a
            // cwd-relative rendering would not say which tree this one is in.
            if let Some(cwd) = s.cwd.as_deref().filter(|c| !c.is_empty()) {
                lines.push(format!("  cwd: {}", cwd));
            }
            let created = format_stat_ts(&s.created);
            let last = s
                .last_updated
                .as_deref()
                .map(format_stat_ts)
                .unwrap_or_else(|| created.clone());
            lines.push(format!("  created: {}, last update {}", created, last));
            lines.push(format!(
                "  size: {} turns ({} messages)",
                s.user_message_count, s.message_count
            ));
        }
        None => {
            // No repo / no metadata available (e.g. in tests without a session).
            lines.push("session: (unavailable)".to_string());
        }
    }
    // Estimated compacted context tokens (what would be sent on next turn).
    if context_tokens > 0 {
        lines.push(format!("  estimated context tokens: {}", context_tokens));
    }

    // ── model section ───────────────────────────────────────────────────
    lines.push(format!("model: {}:{}", provider, model));

    // What this turn is actually sending. A rejected `temperature` is only
    // diagnosable with both halves: the provider names the limit, this says
    // what was sent against it. Absent fields read "provider default" rather
    // than being omitted, so their absence is itself the information.
    let num = |v: Option<f64>| match v {
        Some(n) => crate::v25_number_param::fmt_value(n),
        None => "provider default".to_string(),
    };
    lines.push(format!(
        "  reasoning: {}   temperature: {}   top_p: {}",
        live.reasoning
            .map(|l| l.as_str().to_string())
            .unwrap_or_else(|| "off".to_string()),
        num(live.temperature),
        num(live.top_p),
    ));

    // Context window limit.
    match ctx_len {
        Some(max) => lines.push(format!("  context window limit: {} tokens", max)),
        None => lines.push("  context window limit: unknown (model not in catalog)".to_string()),
    }

    // Current context usage (absolute + % of max).
    let cur = usage.last_input_tokens;
    if cur > 0 {
        if let Some(max) = ctx_len {
            let pct = (cur as f64 / max as f64) * 100.0;
            lines.push(format!(
                "  current usage: {} tokens ({:.1}% of limit)",
                cur, pct
            ));
        } else {
            lines.push(format!("  current usage: {} tokens", cur));
        }
    } else {
        lines.push("  current usage: n/a (no completion call recorded yet)".to_string());
    }

    // Approximate session cost.
    let (pc, cc) = entry
        .as_ref()
        .map(|e| (e.prompt_cost, e.completion_cost))
        .unwrap_or((None, None));
    match (pc, cc) {
        (Some(p), Some(c)) if p > 0.0 || c > 0.0 => {
            let cost = (usage.session_input_tokens as f64 / 1_000_000.0) * p
                + (usage.session_output_tokens as f64 / 1_000_000.0) * c;
            lines.push(format!(
                "  approx session cost: ${:.4} (in {} / out {} tokens)",
                cost, usage.session_input_tokens, usage.session_output_tokens
            ));
        }
        _ => lines.push("  approx session cost: n/a (no pricing in catalog)".to_string()),
    }

    // Cache hit rate (session-level: reads as a fraction of all input tokens).
    let cached = usage.session_cached_input_tokens;
    let creation = usage.session_cache_creation_input_tokens;
    let total_in = usage.session_input_tokens;
    if total_in > 0 && (cached > 0 || creation > 0) {
        let hit = (cached as f64 / total_in as f64) * 100.0;
        lines.push(format!(
            "  cache hit rate: {:.1}% (read {} / written {} / fresh {} tokens)",
            hit,
            cached,
            creation,
            total_in.saturating_sub(cached + creation)
        ));
    } else {
        lines.push("  cache hit rate: n/a (provider reported no cache metrics)".to_string());
    }

    lines
}

/// Format an ISO-8601 timestamp (RFC3339 or "YYYY-MM-DDTHH:MM:SS.sssZ") as
/// "YYYY-MM-DD HH:MM" for the `/stat` header. Falls back to the raw string
/// when parsing fails so a malformed timestamp never blanks the panel.
fn format_stat_ts(ts: &str) -> String {
    // Try RFC3339 first ("2025-01-01T14:32:00.123456789+00:00").
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return dt.format("%Y-%m-%d %H:%M").to_string();
    }
    // Fall back to the truncated UTC format used by session headers
    // ("2025-01-01T14:32:00.000Z") via chrono::DateTime::parse_from_str.
    let normalized = format!("{}Z", ts.trim_end_matches('Z'));
    if let Ok(dt) = chrono::DateTime::parse_from_str(&normalized, "%+") {
        return dt.format("%Y-%m-%d %H:%M").to_string();
    }
    ts.to_string()
}

#[cfg(test)]
mod stat_tests {
    use super::*;

    #[test]
    fn session_stats_lines_renders_session_and_model_sections() {
        use super::super::SessionUsage;
        use super::{session_stats_lines, SessionStatsInfo};

        let info = SessionStatsInfo {
            id: "0199f3c1-7a2e-7000-8000-000000000000".into(),
            name: Some("my-session".into()),
            cwd: Some("/home/gabriel/src/picocode2".into()),
            created: "2025-01-02T03:04:05.000Z".into(),
            last_updated: Some("2025-01-02T10:11:12.000Z".into()),
            message_count: 14,
            user_message_count: 7,
        };
        let usage = SessionUsage::default();
        let lines = session_stats_lines(
            "openrouter",
            "minimax/minimax-m3:free",
            usage,
            Some(info),
            42_000,
            None,
            &Default::default(),
        );

        // The first five lines are the "session:" section.
        assert_eq!(lines[0], "session: 0199f3c1-7a2e-7000-8000-000000000000");
        // The name is its own line, not folded into the headline — otherwise
        // a reader cannot tell a name from a UUID.
        assert_eq!(lines[1], "  name: my-session");
        // The cwd is quoted whole: the overlay lists sessions from every
        // cwd, so a shortened form would not say which tree this is in.
        assert_eq!(lines[2], "  cwd: /home/gabriel/src/picocode2");
        assert!(
            lines[3].starts_with("  created:") && lines[3].contains("last update"),
            "expected created/last update line, got {:?}",
            lines[3]
        );
        // Both turn and message counts must be shown — the former is the
        // number of user prompts (= turns), the latter the total entries
        // (user + assistant + toolResult).
        assert_eq!(lines[4], "  size: 7 turns (14 messages)");
        // New line: estimated context tokens
        assert_eq!(lines[5], "  estimated context tokens: 42000");

        // Then the "model:" section starts on the next line, with the rest
        // of the existing context-window / usage / cost / cache rows.
        let model_idx = lines
            .iter()
            .position(|l| l.starts_with("model:"))
            .expect("model section must be present");
        assert!(model_idx >= 6, "model section must follow session section");
        assert!(lines[model_idx].contains("openrouter:minimax/minimax-m3:free"));
        assert!(lines.iter().any(|l| l.contains("context window limit")));
    }

    /// A session with no recorded cwd drops the line. `cwd` is a required
    /// header field, so an empty one means a malformed or hand-written
    /// session file — printing `cwd: ` would read as a rendering bug, and
    /// inventing a value would be a lie about where the session was started.
    #[test]
    fn session_stats_lines_omits_an_empty_cwd() {
        use super::super::SessionUsage;
        use super::{session_stats_lines, SessionStatsInfo};

        for cwd in [None, Some(String::new())] {
            let info = SessionStatsInfo {
                id: "0199f3c1-7a2e-7000-8000-000000000000".into(),
                name: Some("my-session".into()),
                cwd,
                created: "2025-01-02T03:04:05.000Z".into(),
                last_updated: None,
                message_count: 1,
                user_message_count: 1,
            };
            let lines = session_stats_lines(
                "openrouter",
                "some/model",
                SessionUsage::default(),
                Some(info),
                0,
                None,
                &Default::default(),
            );
            assert!(
                lines.iter().all(|l| !l.contains("cwd")),
                "no cwd line expected, got {lines:?}"
            );
            // The rest of the section still renders.
            assert!(
                lines[2].starts_with("  created:"),
                "expected the section to continue, got {:?}",
                lines[2]
            );
        }
    }

    /// An unnamed session shows the id and nothing where the name would go.
    /// A blank name line would read as a name the user chose, and a name
    /// substituted for the id would be indistinguishable from a real one.
    #[test]
    fn session_stats_lines_omits_an_unset_name_but_keeps_the_id() {
        use super::super::SessionUsage;
        use super::{session_stats_lines, SessionStatsInfo};

        for name in [None, Some(String::new())] {
            let info = SessionStatsInfo {
                id: "0199f3c1-7a2e-7000-8000-000000000000".into(),
                name,
                cwd: Some("/tmp".into()),
                created: "2025-01-02T03:04:05.000Z".into(),
                last_updated: None,
                message_count: 1,
                user_message_count: 1,
            };
            let lines = session_stats_lines(
                "openrouter",
                "some/model",
                SessionUsage::default(),
                Some(info),
                0,
                None,
                &Default::default(),
            );
            assert_eq!(lines[0], "session: 0199f3c1-7a2e-7000-8000-000000000000");
            assert!(
                lines.iter().all(|l| !l.contains("name")),
                "no name line expected, got {lines:?}"
            );
            // The next rendered row is the cwd, not a hole where name was.
            assert_eq!(lines[1], "  cwd: /tmp");
        }
    }

    #[test]
    fn session_stats_lines_falls_back_when_no_session_info() {
        use super::super::SessionUsage;
        use super::session_stats_lines;

        let lines = session_stats_lines(
            "openrouter",
            "minimax/minimax-m3:free",
            SessionUsage::default(),
            None,
            0,
            None,
            &Default::default(),
        );

        // Without a repo / session metadata the panel still renders both
        // sections — session just shows "(unavailable)" instead of details.
        assert!(
            lines[0].starts_with("session:") && lines[0].contains("unavailable"),
            "expected unavailable session header, got {:?}",
            lines[0]
        );
        let model_idx = lines
            .iter()
            .position(|l| l.starts_with("model:"))
            .expect("model section must be present");
        assert!(model_idx >= 1);
    }

    // ---- the rebuild gate (doc/model_settings_v2.md A.7) ----

    // ---- /stat: the resolved view, and what this turn sends ----

    /// A Ctrl-E'd `context_length` is what the app enforces, so `/stat` must
    /// quote it. Reading the raw catalog here made this contradict the
    /// `/model` header on the same fact with nothing to explain the difference.
    #[test]
    fn stat_quotes_a_stored_context_length_not_the_catalogs() {
        let override_len = 4_096u64;
        let stored = crate::model_settings::ModelSettings {
            context_length: Some(override_len),
            ..Default::default()
        };
        let lines = session_stats_lines(
            "openrouter",
            "some/model",
            SessionUsage::default(),
            None,
            0,
            Some(&stored),
            &Default::default(),
        );
        let joined = lines.join("\n");
        assert!(
            joined.contains(&format!("{override_len} tokens")),
            "the override must win: {joined}"
        );
    }

    /// A model the catalog does not cover still has a record, and the record is
    /// the user's own statement about it. Quoting "unknown" there would make
    /// every Ctrl-E override on an uncatalogued model invisible.
    #[test]
    fn stat_uses_a_stored_record_for_a_model_the_catalog_lacks() {
        let stored = crate::model_settings::ModelSettings {
            context_length: Some(8_192),
            ..Default::default()
        };
        let lines = session_stats_lines(
            "openrouter",
            "vendor/uncatalogued",
            SessionUsage::default(),
            None,
            0,
            Some(&stored),
            &Default::default(),
        );
        let joined = lines.join("\n");
        assert!(joined.contains("8192 tokens"), "{joined}");
    }

    /// The diagnostic pairing: when a 400 says `temperature must be <= 1`, the
    /// only way to act is to see what was sent. Absent fields say "provider
    /// default" — their absence is the information, so they are not omitted.
    #[test]
    fn stat_reports_the_params_this_turn_sends() {
        let live = crate::params::ModelParams {
            reasoning: Some(crate::params::ReasoningLevel::High),
            temperature: Some(1.5),
            top_p: None,
        };
        let lines = session_stats_lines(
            "anthropic",
            "claude-sonnet-4",
            SessionUsage::default(),
            None,
            0,
            None,
            &live,
        );
        let joined = lines.join("\n");
        assert!(joined.contains("reasoning: high"), "{joined}");
        assert!(joined.contains("temperature: 1.5"), "{joined}");
        assert!(joined.contains("top_p: provider default"), "{joined}");
    }
}
