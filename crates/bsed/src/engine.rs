//! Running the screened script, then putting the output back into a shape the
//! diff can trust.
//!
//! Three steps, in this order:
//!
//! 1. **Evaluate.** `sed-rs` gives us a pure `text -> text` transform. No file
//!    handles, no in-place mode, no shell: the deny-list in [`crate::script`]
//!    has already removed everything that could reach outside.
//! 2. **Collapse case sentinels.** Expands the markers
//!    [`crate::script`] planted around `\U`-style spans. A missing sentinel when
//!    one was expected is a hard error, not a silent wrong preview — that is the
//!    only way this can go wrong, and it means some other command in the script
//!    consumed the marker bytes.
//! 3. **Restore the trailing newline.** `sed-rs` always terminates the last
//!    line; GNU preserves the absence of one. Without this step, a no-op
//!    expression on a file that lacks a final newline would look like an edit of
//!    the last line.

use crate::script::ChangeRange;

const CLOSE_C: char = '\u{e003}';

/// Why evaluation failed. Distinct from a rejected script: the script passed
/// screening but the engine could not run it.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineError(pub String);

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Apply `script` to `input`, returning text ready to diff.
/// Also applies any extracted change-range commands with GNU semantics.
pub fn run(
    script: &str,
    input: &str,
    expect_sentinels: bool,
    change_ranges: &[ChangeRange],
) -> Result<String, EngineError> {
    let mut out = sed_rs::eval(script, input).map_err(|e| EngineError(e.to_string()))?;
    if expect_sentinels {
        out = collapse_case(&out)?;
    }
    // Apply change ranges with GNU semantics (replace entire range with one copy)
    if !change_ranges.is_empty() {
        out = apply_change_ranges(&out, change_ranges)?;
    }
    // `sed-rs` terminates the last line unconditionally; GNU preserves the
    // absence of one when the input's last line is still the output's last line.
    // So put back what the file had, and only when that line really is still
    // there.
    if !input.ends_with('\n') && out.ends_with('\n') && last_line_survives(input, &out) {
        out.pop();
    }
    Ok(out)
}

/// Whether the input's last line is still the output's last line.
///
/// GNU's rule for the final newline, measured against sed rather than assumed:
///
/// ```text
///   s/zzz/yyy/   in "a\nb\nc"   out "a\nb\nc"    no terminator   (a no-op)
///   s/c/z/       in "a\nb\nc"   out "a\nb\nz"    no terminator   (rewritten in place)
///   1d           in "a\nb\nc"   out "b\nc"       no terminator   (still the last line)
///   2i\ X        in "a\nb\nc"   out "a\nX\nb\nc" no terminator   (pushed up)
///   $d           in "a\nb\nc"   out "a\nb\n"     terminator      (the last line is gone)
///   $a app       in "a\nb\nc"   out "a\nb\nc\napp\n" terminator (something follows it)
/// ```
///
/// Equal line counts cover the in-place rewrite, which is the one case where the
/// last line's *text* changes but the line itself is the same one. Equal last
/// lines cover everything that only moved. What is left -- fewer lines *and* a
/// different last line -- is the last line having been removed or displaced, and
/// that is where GNU re-terminates.
fn last_line_survives(input: &str, out: &str) -> bool {
    let in_lines: Vec<&str> = input.lines().collect();
    let out_lines: Vec<&str> = out.lines().collect();
    in_lines.len() == out_lines.len() || in_lines.last() == out_lines.last()
}

/// Apply change-range commands with GNU sed semantics.
/// GNU sed: `2,4c\new text` replaces lines 2-4 with ONE copy of "new text".
fn apply_change_ranges(input: &str, change_ranges: &[ChangeRange]) -> Result<String, EngineError> {
    let lines: Vec<&str> = input.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line_num = i + 1; // 1-based line number
        let mut applied = false;
        for cr in change_ranges {
            // Handle end_line = usize::MAX (means "to end of file")
            let end = if cr.end_line == usize::MAX {
                lines.len()
            } else {
                cr.end_line
            };
            if line_num >= cr.start_line && line_num <= end {
                // This line is in the range. Add the replacement once and skip the range.
                if line_num == cr.start_line {
                    // Split replacement by newlines and add each as a line
                    for repl_line in cr.replacement.lines() {
                        result.push(repl_line);
                    }
                }
                // Skip all lines in this range
                i = end;
                applied = true;
                break;
            }
        }
        if !applied {
            result.push(lines[i]);
            i += 1;
        } else {
            // i was set to `end` (1-based), which is the 0-based index of the
            // line AFTER the range. Do not increment further.
        }
    }
    // Join with newlines, preserving trailing newline behavior
    let mut out = result.join("\n");
    if input.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Expand the case sentinels planted by [`crate::script::rewrite_case_escapes`].
///
/// An unpaired or missing sentinel means the markers were damaged in flight, so
/// this fails rather than emitting a preview that is quietly wrong.
fn collapse_case(input: &str) -> Result<String, EngineError> {
    // Note what is *not* checked: that a marker is present. A substitution that
    // never matched emits nothing, so absence is normal and must stay a no-op.
    // What is checked is that no marker is left *unbalanced* — that is the real
    // hazard, and it is what happens if a later command in the same script
    // rewrites one half of a pair: the result would carry a stray private-use
    // character into the file, or lose a transform the model asked for.
    let mut out = String::with_capacity(input.len());
    let mut mode: Option<bool> = None; // Some(true) = upper, Some(false) = lower
    let chars = input.chars().peekable();
    for ch in chars {
        match ch {
            '\u{e001}' => {
                if mode.is_some() {
                    return Err(EngineError("internal: nested case markers".into()));
                }
                mode = Some(true);
            }
            '\u{e002}' => {
                if mode.is_some() {
                    return Err(EngineError("internal: nested case markers".into()));
                }
                mode = Some(false);
            }
            CLOSE_C => {
                if mode.take().is_none() {
                    return Err(EngineError("internal: unbalanced case marker".into()));
                }
            }
            _ => match mode {
                Some(true) => out.extend(ch.to_uppercase()),
                Some(false) => out.extend(ch.to_lowercase()),
                None => out.push(ch),
            },
        }
    }
    if mode.is_some() {
        return Err(EngineError("internal: unterminated case marker".into()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_newline_is_preserved_both_ways() {
        // File without a final newline: output must not gain one, or a no-op
        // would look like an edit of the last line.
        let out = run("s/a/b/", "x\na", false, &[]).unwrap();
        assert_eq!(out, "x\nb");
        // And a file with one keeps exactly one.
        let out = run("s/a/b/", "x\na\n", false, &[]).unwrap();
        assert_eq!(out, "x\nb\n");
    }

    #[test]
    fn no_op_expression_is_byte_identical() {
        let text = "a\nb\nc\n";
        assert_eq!(run("s/zzz/yyy/", text, false, &[]).unwrap(), text);
    }

    #[test]
    fn sentinels_expand_to_case_transforms() {
        // Built through `script::build`, because that is what plants them: a
        // raw `\U` reaching `run` directly is exactly the gap the sentinel
        // method exists to close.
        // The backreference must land *inside* the sentinel span, which is the
        // whole reason the rewrite emits segments rather than characters.
        let scr = crate::script::build("s/\\(hi there\\)/\\U\\1/").unwrap();
        assert!(scr.used_case_sentinels);
        let out = run(&scr.text, "hi there\n", scr.used_case_sentinels, &[]).unwrap();
        assert_eq!(out, "HI THERE\n");
    }

    #[test]
    fn sticky_case_escapes_convert_one_character() {
        // \u upper-cases the next character only, then the mode is off.
        let scr = crate::script::build("s/hi/\\uHI there/").unwrap();
        let out = run(&scr.text, "hi\n", scr.used_case_sentinels, &[]).unwrap();
        assert_eq!(out, "HI there\n");
    }

    #[test]
    fn a_substitution_that_never_matched_is_a_no_op() {
        // No marker in the output is normal: the command simply did not fire.
        let out = run(r"s/zzz/\U&/", "abc\n", true, &[]).unwrap();
        assert_eq!(out, "abc\n");
    }

    #[test]
    fn a_half_eaten_marker_is_an_error_not_a_stray_character() {
        // A stray closer with no opener — the shape a half-consumed marker
        // leaves behind. Refuse rather than write a private-use codepoint into
        // the user's file.
        let e = run("a\n\u{e003}stray", "text\n", true, &[]).unwrap_err();
        assert!(e.0.contains("unbalanced"), "{e}");
    }

    #[test]
    fn unbalanced_close_marker_is_an_error() {
        let e = collapse_case("a\u{e003}b").unwrap_err();
        assert!(e.0.contains("unbalanced"), "{e}");
    }
}
