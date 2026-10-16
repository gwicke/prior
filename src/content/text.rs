//! Text formatting: word wrap, the discrete-message block, and the start-up
//! greeter header.

use console::{style, StyledObject};

/// Wrap `text` to fit `width` characters per line.
///
/// Each logical line (split on `\n`) is wrapped independently; blank lines are
/// preserved. The leading whitespace of each logical line is captured and
/// re-emitted at the start of the first wrapped line *and* every continuation
/// line, so indentation survives a reflow.
///
/// Tokenization inside the body uses `split_whitespace`, collapsing any run of
/// whitespace between words to a single space. A word longer than the available
/// width (after subtracting the indent) is hard-split into `width`-sized
/// character chunks (CJK-safe). The first chunk keeps the indent; subsequent
/// chunks are emitted plain so the overlay renderer can prepend its own marker.
///
/// Unlike a grapheme-boundary splitter, this keeps trailing punctuation
/// attached to its word (e.g. `"Commands:"` stays `"Commands:"`, not
/// `"Commands :"`) and never inserts a space adjacent to existing whitespace
/// between words.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return text.lines().map(|l| l.to_string()).collect();
    }
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        // Capture leading whitespace so the indent survives wrap.
        let indent: String = logical.chars().take_while(|c| c.is_whitespace()).collect();
        let body = &logical[indent.len()..];
        if body.is_empty() {
            // All-whitespace line: preserve the indent itself.
            out.push(indent);
            continue;
        }
        let indent_chars = indent.chars().count();
        let content_width = width.saturating_sub(indent_chars).max(1);
        let words: Vec<&str> = body.split_whitespace().collect();
        let mut line = String::from(&indent);
        for word in words {
            if word.chars().count() > content_width {
                // Flush any pending line, then hard-split the long word.
                if line.chars().count() > indent_chars {
                    out.push(std::mem::take(&mut line));
                }
                let chunks: Vec<String> = word
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(content_width)
                    .map(|c| c.iter().collect())
                    .collect();
                let last_idx = chunks.len().saturating_sub(1);
                for (i, c) in chunks.iter().enumerate() {
                    if i == 0 {
                        out.push(format!("{}{}", indent, c));
                    } else if i < last_idx {
                        out.push(c.clone());
                    }
                }
                line = chunks.last().cloned().unwrap_or_default();
                continue;
            }
            let content_len = line.chars().count() - indent_chars;
            if content_len == 0 {
                line.push_str(word);
            } else if content_len + 1 + word.chars().count() > content_width {
                out.push(std::mem::take(&mut line));
                line.push_str(&indent);
                line.push_str(word);
            } else {
                line.push(' ');
                line.push_str(word);
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// The `⏺ `-prefixed block for a discrete message (`display_text`), with a
/// trailing newline. Inline markdown is rewritten; measuring the bytes (not
/// the source) is what decides the height downstream.
pub(crate) fn system_message(text: &str) -> String {
    format!("{} {}\n", style("⏺").cyan(), termimad::inline(text))
}

/// The start-up greeter block — avatar over a status line — for
/// `display_header`, with a trailing newline. `width` centers the avatar.
pub(crate) fn header_block(
    provider: &str,
    model: &str,
    yolo: bool,
    limit: usize,
    persona: Option<&str>,
    width: usize,
) -> String {
    let avatar = [
        "    ▄     ▄    ",
        "   ███   ███   ",
        "  ███████████  ",
        " ███ █   █ ███ ",
        " █████████████ ",
        "   ███   ███   ",
        "  ██       ██  ",
    ];

    // One counted block: the greeter's height used to be a hardcoded 9
    // (blank + 3 avatar lines + blank + status), which is a guess like the
    // ones this phase exists to remove.
    let mut block = String::from("\n");
    for line in avatar {
        let padding = width.saturating_sub(15) / 2;
        block.push_str(&format!("{}{}\n", " ".repeat(padding), style(line).cyan()));
    }
    block.push('\n');

    let status = |active, label, color: fn(StyledObject<String>) -> StyledObject<String>| {
        let s = style(format!("[{}] {}", if active { "x" } else { " " }, label));
        if active {
            color(s)
        } else {
            s.dim()
        }
    };

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".into());

    block.push_str(&format!(
        "{} | {} ({})",
        style("picocode").bold(),
        style(provider).cyan(),
        style(model).blue(),
    ));

    if let Some(p) = persona {
        block.push_str(&format!(" | {}", style(p).magenta()));
    }

    block.push_str(&format!(
        " | {} | {} | {}\n",
        status(yolo, "yolo", |s| s.red()),
        style(format!("limit:{}", limit)).yellow(),
        style(cwd).dim()
    ));
    block
}
