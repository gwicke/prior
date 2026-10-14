//! The status bar: the element, and the fit that turns the shaped data into a
//! row (design `doc/261003_output_drawing_refactor.md` §UI — "the bar's held
//! state is raw data, formatted by the bar").
//!
//! Moved here from `output.rs` in step 4h-c: the bar is UI chrome, so its
//! element lives with the rest of the UI layer's elements. The *data* half of
//! the seam stays on the agent side — the store carries
//! [`crate::agent::status::BarStatus`] facts, and
//! [`crate::agent::status::status_text`] shapes them into a
//! [`StatusText`]; both halves of the *drawing* (the shaping's fit and the
//! dot) run in the element's own render, below.

use crate::agent::status::{status_text, BarStatus, StatusText};
use crate::render::{Element, RenderElement, Rendered, RowAsk};
use crate::row_count::{display_width_ansi, truncate_to_width};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
pub(crate) const DOT_STREAMING: &str = "\x1b[38;5;41m";
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

/// [`StatusText`] fitted to `width` and rendered as one full-width row, in the
/// state the caller is in.
///
/// Everything in here needs the width, which is why it lives on this side of the
/// seam rather than with the data that feeds it. Which field gives way, the dot
/// cell's share of the budget, the truncation and the padding are *one*
/// decision, and the width is what makes it.
///
/// Deliberately **one** function: "fit the group, then style it" is one
/// decision, and a second rendering of it would be a place for the two to
/// drift apart. Its only production caller is the bar's own render (step
/// 4f-a); the tests beside `agent::status` are the other readers.
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
    while !fields.is_empty() && display_width_ansi(&group) + sep_cols(&group) > avail {
        fields.pop();
        group = fields.join(" ");
    }
    // A model name alone can still be too wide for a tiny terminal. Cut it —
    // `truncate_to_width(_, 0)` returns its input unchanged, so a zero budget has
    // to become an empty string rather than the whole name.
    let group_room = avail.saturating_sub(sep_cols(&group));
    let group = if display_width_ansi(&group) > group_room {
        if group_room == 0 {
            String::new()
        } else {
            truncate_to_width(&group, group_room)
        }
    } else {
        group
    };

    // What is left of the row belongs to the name, and the ellipsis is the
    // point: it takes every column the model group did not.
    let name_room = group_room.saturating_sub(display_width_ansi(&group));
    // A name with no room goes rather than being cut to a single ellipsis, which
    // identifies nothing and costs the reader a column to skip over.
    let name = if text.name.is_empty() || name_room == 0 {
        String::new()
    } else {
        truncate_to_width(&text.name, name_room)
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
        .saturating_sub(display_width_ansi(&line));
    // Air, the dot's colour, the glyph, a gap, then the text's own foreground:
    // the colour has to bracket the glyph exactly, and the text's has to follow
    // it because `38;5;245m` and the dot's colour are the same attribute slot.
    format!(
        "{STATUS_RESET}{STATUS_BG}{air}{dot}{DOT}{gap}{STATUS_FG}{}{line}{STATUS_RESET}",
        " ".repeat(pad)
    )
}

/// The area's status row, as an element that resolves **when it is drawn** —
/// both halves of the drawing (step 4f-a, design §UI: the bar formats itself
/// from raw data).
///
/// `Element::lines` would bake `status_bar(...)`'s output into the element at
/// compose time — and compose runs once per keystroke, while the two things
/// the bar shows (the raw store's figures and the streaming dot) move at turn
/// start and end. Holding the handles instead makes every draw current,
/// including §4.5 P1's re-emit of the area under a content write. Since 4f-a
/// the *shaping* runs here too, not at the push: the store carries facts
/// (`agent::status::BarStatus`), and `status_text` + `status_bar` turn them
/// into a row at the width being rendered for — neither half can be applied
/// without the other, so neither left with the data.
///
/// The element owns its data source rather than reaching back into the output
/// that pushed it, which is what keeps `render` a function of `(h, term_w)`
/// plus state this element holds itself — constructible with no terminal and
/// no output in sight, so §3.1's unit-testability is intact rather than traded
/// away.
struct StatusItem {
    /// The bar's **raw data** (step 4f-a, design §UI): the store is content's
    /// (`Content::bar`), shared the same way the streaming flag is.
    status: Arc<Mutex<BarStatus>>,
    streaming: Arc<AtomicBool>,
}

impl RenderElement for StatusItem {
    fn height(&self, _width: usize) -> RowAsk {
        RowAsk::fixed(1, 1)
    }

    fn render(&self, _h: usize, term_w: usize) -> Rendered {
        // The bar formats itself (step 4f-a): the held state is facts, and
        // both halves of the drawing run here — the shaping (name fallback,
        // field list, drop order) and the fit at the width being rendered
        // for. Neither can be applied without the other, so neither left with
        // the data.
        let status = self.status.lock().unwrap();
        let text = status_text(&status);
        // Off between compose and render → say nothing rather than render an
        // empty bar's furniture. The row was reserved by the allocator either
        // way, so nothing below it moves.
        let row = if text.is_off() {
            String::new()
        } else {
            status_bar(&text, term_w, self.streaming.load(Ordering::Relaxed))
        };
        Rendered {
            rows: vec![row],
            caret: None,
        }
    }
}

/// The status row: `min == max == 1`, data resolved at render time.
/// Presence is the compose's decision (`repl::compose_area`); this is
/// only ever built when there is something to say (see [`StatusItem`]).
///
/// A free function rather than an inherent method on `Element`: those would
/// live in `slashdot_tui`, and this element is the UI layer's (it holds
/// content's bar store). Same shape as `Element::new`, whose `RenderElement`
/// impl lives there.
pub(crate) fn status(status: Arc<Mutex<BarStatus>>, streaming: Arc<AtomicBool>) -> Element {
    Element::new(StatusItem { status, streaming })
}
