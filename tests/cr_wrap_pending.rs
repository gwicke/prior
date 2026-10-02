//! CR + wrap-pending replay — the primitives `ui_draw_261001.md`'s
//! paint-forward rests on, pinned against a real vt100 emulator.
//!
//! Two claims the design makes about the terminal, before any code is written
//! against them (ui_draw_v2 §5.2 prereqs 1–2, now first steps of the new doc):
//!
//! (a) `\r` cancels the wrap-pending state a full-width row leaves behind, so
//!     `\r` + gap newlines from *any* cursor position — including a row filled
//!     to its last column — land the next painted row exactly where the UI
//!     expects, without a phantom wrap eating into it.
//! (b) `\r` + N newlines from an arbitrary cursor position lands the paint
//!     exactly N rows down, leaving the rows above intact.
//!
//! Plus the stdout-switch premise (ui_draw_261001): a write past the bottom
//! row scrolls earlier content up instead of losing or overwriting it.

/// The tty's OPOST/ONLCR: production emits bare `\n`, and the line discipline
/// maps it to CR LF before the terminal sees it. Replays feed the same bytes
/// the tty would deliver.
fn onlcr(s: &str) -> Vec<u8> {
    s.replace('\n', "\r\n").into_bytes()
}

fn parser(rows: usize, cols: usize) -> vt100::Parser {
    vt100::Parser::new(rows as u16, cols as u16, 200)
}

fn feed(p: &mut vt100::Parser, chunk: &str) {
    p.process(&onlcr(chunk));
}

/// One screen row as a trimmed string.
fn row(p: &vt100::Parser, r: usize, cols: usize) -> String {
    let s = p.screen();
    (0..cols)
        .map(|c| {
            s.cell(r as u16, c as u16)
                .map(|cell| cell.contents())
                .unwrap_or(" ")
                .chars()
                .next()
                .unwrap_or(' ')
        })
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn caret(p: &vt100::Parser) -> (u16, u16) {
    p.screen().cursor_position()
}

/// A full row's worth of text — leaves the cursor past the last column, with
/// the wrap deferred until the next printable character.
fn fill(p: &mut vt100::Parser, text: &str) {
    feed(p, text);
}

/// The counter-example (a) exists to rule out: without `\r`, the next printable
/// character *performs* the deferred wrap and lands on the following row.
#[test]
fn a_full_width_row_wraps_on_the_next_printable() {
    let mut p = parser(6, 8);
    fill(&mut p, "aaaaaaaa");
    feed(&mut p, "X");

    assert_eq!(row(&p, 0, 8), "aaaaaaaa", "row 0 keeps its eight cells");
    assert_eq!(
        row(&p, 1, 8),
        "X",
        "the deferred wrap moved the paint down a row"
    );
    assert_eq!(caret(&p), (1, 1));
}

/// (a): `\r` cancels the deferred wrap — the next printable character starts at
/// column 0 *of the same row* instead of wrapping to the next one.
#[test]
fn cr_cancels_wrap_pending_on_a_full_width_row() {
    let mut p = parser(6, 8);
    fill(&mut p, "aaaaaaaa");
    feed(&mut p, "\rX");

    assert_eq!(row(&p, 0, 8), "Xaaaaaaa", "paint restarted on the same row");
    assert_eq!(row(&p, 1, 8), "", "no phantom row was created");
    assert_eq!(caret(&p), (0, 1));
}

/// (b) from a mid-row cursor: content rows exist, the cursor sits inside the
/// last one, and `\r` + N newlines must land the marker exactly N rows down
/// with everything above intact.
#[test]
fn cr_newlines_from_mid_row_land_exactly_n_rows_down() {
    let mut p = parser(8, 8);
    feed(&mut p, "line1\nlin");
    assert_eq!(
        caret(&p),
        (1, 3),
        "mid-row, mid-line: the arbitrary position"
    );

    feed(&mut p, "\r");
    feed(&mut p, "\n\n");
    feed(&mut p, "M");

    assert_eq!(caret(&p), (3, 1), "marker ended on row 1 + N, col 0");
    assert_eq!(row(&p, 1, 8), "lin", "the content row above is untouched");
    assert_eq!(row(&p, 3, 8), "M");
    assert_eq!(row(&p, 0, 8), "line1");
}

/// (b) from a *pending* full-width row — the case the design specifically
/// needs: content's last row filled to the last column, UI closes the line.
/// `\r` + gap newlines must land N rows down with the full-width row intact,
/// no wrap performed into the gap.
#[test]
fn cr_newlines_from_a_pending_full_width_row_land_exactly_n_rows_down() {
    let mut p = parser(8, 8);
    fill(&mut p, "aaaaaaaa");

    feed(&mut p, "\r");
    feed(&mut p, "\n\n");
    feed(&mut p, "M");

    assert_eq!(row(&p, 0, 8), "aaaaaaaa", "the full-width row is intact");
    assert_eq!(row(&p, 1, 8), "", "gap row 1 is empty — no phantom wrap");
    assert_eq!(row(&p, 2, 8), "M", "paint landed exactly N rows down");
    assert_eq!(caret(&p), (2, 1));
}

/// The stdout-switch premise: writing past the bottom row scrolls earlier
/// content up (into scrollback) rather than losing or overwriting it.
#[test]
fn a_write_past_the_bottom_row_scrolls_earlier_content_up() {
    let mut p = parser(4, 8);
    // Content fills the screen and ends *on* the last row: no trailing
    // newline, cursor mid-row — exactly where paint-forward starts.
    feed(&mut p, "line1\nline2\nline3\nline4");
    assert_eq!(caret(&p), (3, 5), "content ends at the bottom row");

    // The paint-forward loop discovers the bottom mid-pass: CR, then a
    // newline (which scrolls here), then the next written row.
    feed(&mut p, "\r");
    feed(&mut p, "\n");
    feed(&mut p, "after");

    assert_eq!(
        row(&p, 0, 8),
        "line2",
        "earlier content was pushed up, not lost"
    );
    assert_eq!(row(&p, 1, 8), "line3");
    assert_eq!(
        row(&p, 2, 8),
        "line4",
        "the content's last row survived intact"
    );
    assert_eq!(row(&p, 3, 8), "after", "fresh row after the scroll");
    assert_eq!(caret(&p), (3, 5));
}
