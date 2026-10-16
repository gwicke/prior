//! Classification and the separator matrix — what a content write *is*, and
//! whether two writes are separated by a blank row. The formatting half of the
//! rule (`boundary_spacer`) is kept apart from the writing so an emitter can
//! draw a whole block in one `with_content_writer` and still get its
//! boundaries right.

use console::style;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// The logical kind of one transcript content write.
///
/// Kinds, not formatting: `Prompt` is a committed user line, whoever decided to
/// draw it with rules or without. Only what was *written* goes here, so
/// changing the separators around a kind cannot invalidate the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ContentDisplayKind {
    /// A user line committed with Enter — a turn's prompt, a slash command, or
    /// a `!command`. Consecutive ones share a divider rather than stacking one
    /// between each pair.
    Prompt,
    /// `stream_text`, and the segment flushes that close a segment: the answer
    /// of a turn, written through the GFM answer renderer.
    ResponseText,
    /// `stream_thinking` — the reasoning a turn shows, dimmed inline. Its
    /// dimming is internal to the block: the escape that ends it, and the
    /// newline that ends its last line, belong to this kind's content, and the
    /// row that follows it is the boundary matrix's, not this block's.
    ResponseReasoning,
    /// A tool block — `render_call_panel` / `render_result_panel`, which is
    /// every row `paint_panel` writes. One kind for call and result alike:
    /// the two halves of a pair are one block, so the second must not draw a
    /// separator above itself.
    ResponseTool,
    /// `display_text` / `display_error` — a discrete message or error block.
    SystemMessage,
    /// `display_markdown` / `display_system` — a generated block: a compaction
    /// summary, a marker, a notice.
    SystemContent,
}

/// The single line the opener draws between two blocks — the *formatting*
/// half of the rule, separated from the writing so an emitter can draw a whole
/// block in one `with_content_writer` and still get its boundaries right.
///
/// Different kinds separate; the same kind does not, which is what lets a
/// response's internals (a tool panel, a segment flush) stay inside one block,
/// and what collapses two adjacent prompts onto one rule. `None` — the stream's
/// start, or content that did not declare itself — never separates, so the
/// first thing written never gains a blank line above it.
///
/// `closed` is the second dimension of the pair: the previous write declared
/// itself the last flush of its segment, so whatever comes next opens a new
/// block even under this very same kind. Two text blocks that share a kind are
/// still two blocks when a parser and a renderer were reset between them, and
/// this is how the opener learns that — from what the writer said about itself,
/// never from the stream, which cannot see a row that has been held back.
pub(crate) fn boundary_spacer(
    old: Option<ContentDisplayKind>,
    closed: bool,
    new: Option<ContentDisplayKind>,
) -> Option<&'static [u8]> {
    match (old, new) {
        (Some(a), Some(b)) if a != b => Some(b"\n"),
        (_, Some(_)) if closed => Some(b"\n"),
        _ => None,
    }
}

/// The `──────` rule at a width — the *formatting* of a separator, kept apart
/// from writing it so an emitter can draw a whole block in a single
/// `with_content_writer` under one kind.
pub(crate) fn separator_line(width: usize) -> String {
    format!("{}\n", style("─".repeat(width)).dim())
}

/// The boundary/separator state of one content stream: the last declared kind,
/// whether that write closed its segment, and the blank rows held past its
/// line terminator.
///
/// This is the whole separator matrix gathered into one content-owned unit.
/// It used to live half in `DrawCache` (`last_content_display_kind` /
/// `last_content_closes_segment`) and half on `Content` (`held_blank`);
/// the output-drawing refactor moves it here so none of it crosses below the
/// content layer (design `doc/261003_output_drawing_refactor.md`, §Content).
pub(crate) struct ContentBoundary {
    /// `(last declared kind, last write closed its segment)`.
    ///
    /// The kind is *what was written* — not "was a rule just emitted" — so a
    /// change to how content is formatted does not have to change it. `None`
    /// means the last write was not classified. Read and written only by the
    /// content writers: `with_content_writer` records on every write and rolls
    /// an empty one back; `display_committed_prompt` reads it to decide whether
    /// it shares the previous block's rule. `draw_frame` never touches it — an
    /// area apply is not transcript content, and treating it as such would let
    /// a frame's last row decide what a later commit may collapse against.
    last: Mutex<(Option<ContentDisplayKind>, bool)>,
    /// Blank rows withheld from the end of the last content write.
    ///
    /// Trailing separators are **held, not dropped**: a following write of the
    /// same kind releases them (the internal spacing is unchanged — only *when*
    /// it is written moved), a write of a different kind discards them (that
    /// boundary's divider belongs to the next block), and the end of the
    /// stream discards them by simply never releasing. Which is what strips
    /// the response's last separator without needing to know, at the time it is
    /// written, whether anything comes after it.
    held_blank: AtomicUsize,
}

impl ContentBoundary {
    pub(crate) fn new() -> Self {
        ContentBoundary {
            last: Mutex::new((None, false)),
            held_blank: AtomicUsize::new(0),
        }
    }

    /// Record what this write declares itself to be, returning the previous
    /// `(kind, closed)` so the caller has the pair the whole boundary rule is a
    /// statement about. Recording *before* the bytes run lets an emitter
    /// drawing several things inside one block read back its own kind, so its
    /// internal separators are `(kind, kind)` and contribute nothing.
    pub(crate) fn record(
        &self,
        kind: Option<ContentDisplayKind>,
        closes: bool,
    ) -> (Option<ContentDisplayKind>, bool) {
        let mut last = self.last.lock().unwrap();
        let old = last.0;
        let closed = last.1;
        last.0 = kind;
        last.1 = closes;
        (old, closed)
    }

    /// Roll the record back after a write that produced no content: its new
    /// kind never happened, so the predecessor still stands and the next write
    /// still gets the row that boundary owed it. `closed` only propagates when
    /// this write *genuinely* closed an open segment of the same kind — so an
    /// empty turn end cannot spuriously set `closed` and inject blanks before a
    /// later retry.
    pub(crate) fn restore_empty(
        &self,
        old: Option<ContentDisplayKind>,
        closed: bool,
        kind: Option<ContentDisplayKind>,
        closes_segment: bool,
    ) {
        let mut last = self.last.lock().unwrap();
        last.0 = old;
        let genuinely_closes = closes_segment && old == kind && !closed;
        last.1 = closed || genuinely_closes;
    }

    /// The last declared kind, without recording anything.
    pub(crate) fn last_kind(&self) -> Option<ContentDisplayKind> {
        self.last.lock().unwrap().0
    }

    /// Take the held blank rows (leaving none) — they are consumed, not read.
    pub(crate) fn take_held(&self) -> usize {
        self.held_blank.swap(0, Ordering::SeqCst)
    }

    /// Store the blank rows the *next* write should find held.
    pub(crate) fn store_held(&self, rows: usize) {
        self.held_blank.store(rows, Ordering::SeqCst);
    }
}
