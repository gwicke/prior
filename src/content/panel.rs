//! Tool-call panel rows: the full-width grey panel background and the
//! per-line foreground styling behind it. The *emission* of these rows stays
//! on `ConsoleOutput`
//! (`paint_panel` / `tool_panel`), which writes each through the content
//! pipeline with the `ResponseTool` kind.

use crate::ansi::strip_ansi;
use console::style;
use unicode_width::UnicodeWidthStr;

use crate::content::preview::wrap_with_diff_markers;

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
