//! Tool-call panel rows: the full-width grey panel background and the
//! per-line foreground styling behind it. The *emission* of these rows stays
//! on `Content`
//! (`paint_panel` / `tool_panel`), which writes each through the content
//! pipeline with the `ResponseTool` kind.

use crate::ansi::strip_ansi;
use console::style;
use unicode_width::UnicodeWidthStr;

use crate::content::preview::wrap_with_diff_markers;
use crate::content::text::expand_tabs;

/// One full-width dark-grey panel row around pre-styled `content`, without a
/// trailing newline.
///
/// `content` carries any *foreground* ANSI it needs — dim text, red/green diff
/// markers, or syntax-highlight tokens from `highlight_code_fg`. The
/// background is re-applied after every internal `\x1b[0m` the highlighter
/// emitted, so the grey stays solid across a whole line even when the content
/// changes foreground color several times. `indent` leaves one leading column
/// so the line reads as a command's *output*; padding uses display-column
/// width so wide/CJK characters keep the panel edge aligned.
pub(crate) fn panel_row(content: &str, indent: bool, width: usize) -> String {
    const BG: &str = "\x1b[48;2;22;22;26m";
    const RESET: &str = "\x1b[0m";
    // Prepend the background and re-apply it after each reset, so a token's
    // own `\x1b[0m` cannot drop the grey behind the rest of the line.
    let painted = format!("{BG}{}", content.replace(RESET, &format!("{RESET}{BG}")));
    let vis = UnicodeWidthStr::width(strip_ansi(&painted).as_str());
    let pad = if width == 0 {
        0
    } else if indent {
        width.saturating_sub(vis + 1)
    } else {
        width.saturating_sub(vis)
    };
    let lead = if indent {
        format!("{BG} ")
    } else {
        String::new()
    };
    format!("{lead}{painted}{BG}{}{RESET}", " ".repeat(pad))
}

/// The styled contents for each wrapped line of a plain-text tool panel —
/// foreground only; [`panel_row`] adds the grey background.
///
/// `text` is wrapped to the available width and painted dim (or red for
/// errors); `-`/`+` diff markers keep their red/green. For pre-styled /
/// syntax-highlighted content, [`panel_row`] is called directly instead.
pub(crate) fn tool_row_contents(
    text: &str,
    is_error: bool,
    indent: bool,
    width: usize,
) -> Vec<String> {
    // When indented, leave one column for the indent; keep the background
    // full-width in both cases.
    let inner = if indent {
        width.saturating_sub(1).max(1)
    } else {
        width.max(1)
    };
    // Expand tabs before any width math: this path carries raw tool output
    // (sed previews, error envelopes) whose program-indented lines must be
    // measured in the columns the terminal will draw them, or the row pads
    // over-full and wraps. Both `wrap` and `panel_row` measure with
    // unicode-width, where a tab is zero columns.
    let text = expand_tabs(text);
    let text = text.as_ref();
    let lines = if text.is_empty() {
        vec![String::new()]
    } else {
        wrap_with_diff_markers(text, inner)
    };
    lines
        .into_iter()
        .map(|line| {
            // Foreground-only styling here; `panel_row` adds the grey
            // background (and re-applies it across any internal resets).
            if is_error {
                style(line.clone()).red().to_string()
            } else if let Some(rest) = line.strip_prefix("- ") {
                format!("{}{}", style("-").red(), style(format!(" {rest}")).dim())
            } else if let Some(rest) = line.strip_prefix("+ ") {
                format!("{}{}", style("+").green(), style(format!(" {rest}")).dim())
            } else {
                style(line.clone()).dim().to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::text::wrap;

    /// Display width of a panel row as the terminal draws it: ANSI stripped,
    /// and any tab that somehow survived expanded to its stop. This mirrors
    /// what a real terminal computes before deciding the row wrapped.
    fn drawn_width(row: &str) -> usize {
        UnicodeWidthStr::width(expand_tabs(strip_ansi(row).as_ref()).as_ref())
    }

    /// The exact bug: a tab-indented result line used to be padded on the
    /// assumption its leading tab was zero columns, so the row came out wider
    /// than the panel and natural-wrapped in the terminal (bleeding the frame
    /// through, since `LineClear` never armed on the continuation). Every
    /// production path hands `panel_row` tab-expanded text now, so the drawn
    /// row is exactly `width` — no wrap.
    #[test]
    fn a_tab_indented_line_pads_to_exactly_the_panel_width() {
        let width = 60;
        let expanded = expand_tabs("\t\t\tconst x = 1;");
        for line in wrap(expanded.as_ref(), width - 1) {
            let styled = style(line.clone()).dim().to_string();
            let row = panel_row(&styled, true, width);
            assert_eq!(drawn_width(&row), width, "row wrapped: {row:?}");
        }
    }

    /// The plain-tool path (`tool_row_contents`) is the choke the sed-preview
    /// body and error envelopes flow through, and it carries **raw** program
    /// output — tabs intact, never pre-expanded by the caller. It must expand
    /// them itself or the row pads over-full and wraps. Here the input is a
    /// literal tab-bearing blob; the drawn row must land exactly at `width`.
    #[test]
    fn tool_row_contents_expands_raw_tab_bearing_blobs() {
        let width = 60;
        let raw = "\t\tinterface Foo {\n\t\t\tbar(): void;\n\t\tbaz: string;\n\t}";
        for content in tool_row_contents(raw, false, true, width) {
            let row = panel_row(&content, true, width);
            assert!(
                !strip_ansi(&row).contains('\t'),
                "tab reached the panel: {row:?}"
            );
            assert_eq!(drawn_width(&row), width, "row wrapped: {row:?}");
        }
    }
}
