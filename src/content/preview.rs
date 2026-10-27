//! Tool-call previews: command labels, edit diffs, sed previews, and the
//! banner/diff helpers behind them.

use crate::config::ByteLimit;
use crate::content::text::wrap;
use crate::tools::EditOp;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

pub(crate) fn truncate(s: &str, max_len: usize) -> String {
    if s.chars().count() > max_len {
        format!("{}...", s.chars().take(max_len).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Like [`wrap`], but recognizes a leading `- ` / `+ ` as a diff marker: the
/// marker is stripped, the body is wrapped (which preserves the body's
/// leading indent), and the marker is re-prepended to the first wrapped
/// line. Continuation lines are prefixed with two spaces so they visually
/// align under the marker column.
///
/// `wrap()` alone would collapse the run of whitespace between the `-` and
/// the original line's first non-whitespace character — for an indented
/// source line such as `    if foo {`, `format!("- {l}")` produces
/// `-     if foo {`, which `split_whitespace` then reduces to `- if foo {`,
/// losing the indentation. Routing diff lines through this helper instead
/// keeps the original indentation visible.
pub(crate) fn wrap_with_diff_markers(text: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for logical in text.split('\n') {
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        // A diff line is exactly `<marker><space><original line>` with the
        // marker being `-` or `+`. Anything else (e.g. `---` from a header
        // divider or `--foo`) is treated as plain text.
        let is_diff = matches!(logical.as_bytes(), [b'-' | b'+', b' ', ..]);
        if is_diff {
            let marker = &logical[..2];
            let rest = &logical[2..];
            // Reserve two columns for the marker + the space that follows it
            // on the first line. Continuation lines also need two leading
            // spaces to keep their content aligned under the marker column.
            let content_width = width.saturating_sub(2).max(1);
            if rest.is_empty() {
                // Marker with no body (degenerate but possible): emit the
                // marker on its own.
                out.push(marker.to_string());
                continue;
            }
            let wrapped = wrap(rest, content_width);
            for (i, line) in wrapped.into_iter().enumerate() {
                if i == 0 {
                    out.push(format!("{}{}", marker, line));
                } else {
                    out.push(format!("  {}", line));
                }
            }
        } else {
            out.extend(wrap(logical, width));
        }
    }
    out
}

/// Drop the `NNNN| ` line-number gutter that `read_file` prepends to each line,
/// so the displayed file content is clean (the model still receives the numbers
/// in the raw tool result). Lines without a numeric gutter are returned as-is.
pub(crate) fn strip_read_lineno(line: &str) -> &str {
    if let Some(pos) = line.find('|') {
        let (gutter, rest) = line.split_at(pos);
        let g = gutter.trim();
        if !g.is_empty() && g.chars().all(|c| c.is_ascii_digit()) {
            return rest
                .strip_prefix("| ")
                .unwrap_or_else(|| rest.strip_prefix('|').unwrap_or(rest));
        }
    }
    line
}

pub(crate) fn get_preview(name: &str, args: &Value) -> (String, Vec<String>) {
    let obj = args.as_object();
    let get_str = |key: &str| obj.and_then(|o| o.get(key)).and_then(|v| v.as_str());
    let get_u64 = |key: &str| obj.and_then(|o| o.get(key)).and_then(|v| v.as_u64());

    match name.to_lowercase().as_str() {
        "read" | "read_file" => {
            let path = get_str("path").unwrap_or("");
            let offset = get_u64("offset").unwrap_or(0);
            let limit = get_u64("limit").unwrap_or(0);
            let byte_offset = get_u64("byte_offset").unwrap_or(0);
            let byte_limit = get_u64("byte_limit").unwrap_or(0);
            let mut s = format!("read {}", path);
            // `offset` is 0-based; `limit` is a count. Display is 1-based, so the
            // affected window is [offset+1, offset+limit]. A zero `limit` means
            // the default window — the label names the file, not a promise of
            // its exact extent. Byte mode replaces the line window entirely, so
            // it takes the label, with the effective byte range.
            if byte_offset > 0 {
                let budget = ByteLimit(byte_limit).resolved();
                s.push_str(&format!(
                    " @bytes {}..{}",
                    byte_offset,
                    byte_offset + budget
                ));
            } else if limit > 0 {
                s.push_str(&format!(" [{}..{}]", offset + 1, offset + limit));
            } else if offset > 0 {
                s.push_str(&format!(" [{}..]", offset + 1));
            }
            (s, Vec::new())
        }
        "bash" => {
            // Show the *entire* command line; never shorten it. Prefix with `$`
            // so it reads like a shell prompt. The canonical field is "command";
            // old sessions may have "cmd".
            let cmd = get_str("command")
                .or_else(|| get_str("cmd"))
                .unwrap_or("")
                .trim();
            (format!("$ {}", cmd), Vec::new())
        }
        "sed" | "sed_preview" => {
            // Like `bash`, show what was asked for: the path, then the script on
            // one line, `$`-prefixed so it reads as a command. The long "review
            // these and pass them to `edit`" sentence stays in the tool result,
            // where the model reads it — putting it in the panel spent a human's
            // screen on instructions meant for the model.
            let path = get_str("path").unwrap_or("");
            let one_line = get_str("expressions")
                .unwrap_or("")
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join("; ");
            let mut label = format!("sed {path}");
            if !one_line.is_empty() {
                label.push_str(&format!("  $ {}", truncate(&one_line, 56)));
            }
            // The body needs the *result* (the blocks), which this function does
            // not have; `display_tool_result` renders those instead.
            (label, Vec::new())
        }
        s if s.starts_with("grep") => {
            // Grep tool: show pattern (from "pattern" or "pat") and path.
            let pat = get_str("pattern").or_else(|| get_str("pat")).unwrap_or("");
            let path = get_str("path").unwrap_or("");
            (format!("grep {} {}", pat, path), Vec::new())
        }
        // Retired tools (doc/sandbox.md -> "Tree traversal belongs to the
        // shell"): no session records new calls, but old ones must still render
        // with the shape they had, so `/tree` on a pre-retirement session reads
        // the same as it did.
        s if s == "ls" || s.starts_with("list") => {
            let path = get_str("path").unwrap_or("");
            (format!("ls {}", path), Vec::new())
        }
        "find" | "glob_files" => {
            let pattern = get_str("pattern").unwrap_or("");
            let path = get_str("path").unwrap_or("");
            (format!("find {pattern} {path}"), Vec::new())
        }
        "make_dir" | "mkdir" => {
            let path = get_str("path").unwrap_or("");
            (format!("mkdir -p {}", path), Vec::new())
        }
        "remove" | "rm" => {
            let path = get_str("path").unwrap_or("");
            let recursive = args
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            (
                format!("rm {}{}", if recursive { "-r " } else { "" }, path),
                Vec::new(),
            )
        }
        "move_file" | "mv" => {
            let src = get_str("src").unwrap_or("");
            let dst = get_str("dst").unwrap_or("");
            (format!("mv {} {}", src, dst), Vec::new())
        }
        "copy_file" | "cp" => {
            let src = get_str("src").unwrap_or("");
            let dst = get_str("dst").unwrap_or("");
            (format!("cp {} {}", src, dst), Vec::new())
        }
        "agent_browser" | "agent-browser" => {
            let args_str = get_str("args").unwrap_or("");
            (format!("agent-browser {}", args_str), Vec::new())
        }
        "edit" | "edit_file" => {
            // Two shapes reach here, and both come from
            // `bsed_blockedit::edits_to_canonical_value`: the persisted and
            // displayed form is `{path, edits:[…]}`, one file per call. Reading
            // `path` and `edits[]` directly is deliberate — a blob is *not*
            // parsed here, because parsing is the producer's job
            // (`prepare_edit_call`) and doing it twice is how the panel and the
            // stored call drift apart.
            //
            // A raw blob (a call the tool rejected, stored untouched) has no
            // `path` and no array: the label then renders empty and there is no
            // diff, which is the honest rendering of a call nothing could
            // resolve. The legacy single-edit scalars are still read, since the
            // preview is also reached for arguments that never went through the
            // producer.
            let path = get_str("path").unwrap_or("");
            let edits: Vec<EditOp> = if let Some(edits_array) =
                obj.and_then(|o| o.get("edits")).and_then(|v| v.as_array())
            {
                edits_array
                    .iter()
                    .filter_map(|e| {
                        Some(EditOp {
                            old_text: e.get("oldText")?.as_str()?.to_string(),
                            new_text: e.get("newText")?.as_str()?.to_string(),
                            start_line: e
                                .get("startLine")
                                .and_then(|v| v.as_u64())
                                .map(crate::config::EditStartLine),
                            origin: None,
                        })
                    })
                    .collect()
            } else if let (Some(old), Some(new)) = (get_str("oldText"), get_str("newText")) {
                // Legacy single-edit shape (accepted but undocumented).
                vec![EditOp {
                    old_text: old.to_string(),
                    new_text: new.to_string(),
                    start_line: None,
                    origin: None,
                }]
            } else {
                vec![]
            };
            let (range, diff) = if !edits.is_empty() {
                edit_preview(path, &edits)
            } else {
                // Old format: old/new fields.
                let old = get_str("old").unwrap_or("");
                let new_str = get_str("new").unwrap_or("");
                edit_preview(
                    path,
                    &[EditOp {
                        old_text: old.to_string(),
                        new_text: new_str.to_string(),
                        start_line: None,
                        origin: None,
                    }],
                )
            };
            let mut s = if path.is_empty() {
                // A raw blob: no path is known without parsing, which display
                // does not do.
                "edit".to_string()
            } else {
                format!("edit {path}")
            };
            if let Some(r) = range {
                s.push_str(&format!(" [{}]", r));
            }
            (s, diff)
        }
        _ => {
            let first = obj
                .and_then(|o| o.values().next())
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    _ => v.to_string(),
                })
                .unwrap_or_else(|| args.to_string());
            let cap = name
                .chars()
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + &name[1..])
                .unwrap_or_else(|| name.to_string());
            (
                format!("{} {}", cap, truncate(&first.replace('\n', " "), 50)),
                Vec::new(),
            )
        }
    }
}

/// Best-effort line range + diff preview for an edit tool call.
///
/// An **anchored** edit (`startLine` set, e.g. a minimized/replayed block) is
/// rendered straight from its own lines by [`hunk_lines`], numbered from the
/// anchor — no diff, because it is already minimal and re-diffing would only
/// number it from 1. An **unanchored** edit (the model's own live call) is
/// diffed via [`refined_diff_lines`], which derives the change from `old_text`
/// vs `new_text` and so renders even when the file can't be read or the text has
/// drifted (replay after the edit already applied). Each `@@` hunk header
/// conveys the change regions, so small batches concatenate their diffs with no
/// separators; very large batches are truncated. The label's line *range* comes
/// from the anchor when present, otherwise from reading the file and finding
/// `old_text`; absent both, it is omitted but the diff still shows. Empty edits
/// yield `(None, [])`.
pub(crate) fn edit_preview(path: &str, edits: &[EditOp]) -> (Option<String>, Vec<String>) {
    let first = match edits.first() {
        Some(e) => e,
        None => return (None, Vec::new()),
    };

    // The refined diffs are derived purely from each edit's oldText vs newText,
    // so they are always available — even during replay or after the target text
    // has drifted in the on-disk file. Only the 1-based line *range* requires
    // reading the file and finding `old_text` in its current contents; a
    // missing/read-error file or a no-longer-present oldText degrades to a
    // `None` range while still yielding the diff previews.
    const MAX_OUT: usize = 80;
    let mut diff: Vec<String> = Vec::new();
    let mut truncated = false;

    for edit in edits {
        if diff.len() >= MAX_OUT {
            truncated = true;
            break;
        }
        // An anchored block is already minimal: re-diffing it could only echo
        // it with wrong (buffer-relative) numbers. Render it directly, with
        // real file line numbers. Unanchored edits keep the honest diff.
        let lines = match edit.start_line {
            Some(anchor) => hunk_lines(anchor.0, &edit.old_text, &edit.new_text),
            None => refined_diff_lines(&edit.old_text, &edit.new_text),
        };
        for line in lines {
            if diff.len() >= MAX_OUT {
                truncated = true;
                break;
            }
            diff.push(line);
        }
        if truncated {
            break;
        }
    }

    if truncated {
        diff.push("    … (diff truncated)".to_string());
    }

    let range = (|| {
        // An anchored edit states its own location: no file read, and the
        // label agrees with the hunk header.
        if let Some(anchor) = first.start_line {
            let start = anchor.0;
            let end = start + first.old_text.matches('\n').count() as u64;
            return Some(if start == end {
                format!("{start}")
            } else {
                format!("{start}..{end}")
            });
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return None;
        };
        let idx = content.find(&first.old_text)?;
        // 1-based line number of the first line of `old_text`.
        let start = content[..idx].matches('\n').count() + 1;
        let end = start + first.old_text.matches('\n').count();
        Some(if start == end {
            format!("{start}")
        } else {
            format!("{start}..{end}")
        })
    })();

    (range, diff)
}

/// The opening words of every `bsed` preview result (`crates/bsed` emits two
/// banner shapes — "N blocks for PATH" and "N block(s) in M of K file(s) under
/// DIR" — and both start with this).
pub(crate) const SED_PREVIEW_BANNER: &str = "sed preview: ";

/// Render a `sed` preview result as diff lines, or `None` if it is not one.
///
/// Identified by that banner, **not** by "the text happens to contain
/// `@@@OLD`". This renderer is applied to *every* tool result before the
/// result-specific ones, and the `edit` tool's own argument errors embed the
/// fixed `Expected:` example (`bsed_blockedit::wire::EDIT_ARGS_EXAMPLE`) — which is a
/// well-formed block. So a content sniff parsed the example out of the error,
/// drew it as a phantom `-`/`+` diff, and returned before the error itself was
/// ever rendered; its trailing "summary" was the error's first line cut at the
/// first `@@@OLD`, which is the one *inside backticks* mid-sentence. The banner
/// is a property of the result rather than of panel state (a replayed result
/// arrives with no `last_tool`), so it is what this keys on.
///
/// The banner is kept verbatim (it is the part that says how many blocks are
/// shown, whether growth ran, and what was *not* included) and each block's
/// contents become `-`/`+` pairs under a hunk header carrying its anchor. An
/// empty `NEW` — a deletion — simply has no `+` lines.
pub(crate) fn sed_preview_body(result: &str) -> Option<Vec<String>> {
    if !result.starts_with(SED_PREVIEW_BANNER) {
        return None;
    }
    let parsed = crate::edit_blocks::parse_blocks(result).ok()?;

    let mut out: Vec<String> = Vec::new();
    let mut summary = String::new();
    // The banner is model-facing; only the parts a human needs are lifted out —
    // the counts as a closing line, and any NOTE, which is a warning about what
    // is *missing* from the preview and must not be scrolled past.
    if let Some(first) = result.find("@@@OLD") {
        for line in result[..first].lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if t.starts_with("NOTE:") {
                out.push(t.to_string());
            } else if summary.is_empty() {
                // "sed preview: 3 blocks for src/x.rs (1 expression). Review ..."
                // → "3 blocks in src/x.rs from 1 expression"
                summary = short_banner(t);
            }
        }
    }

    for e in &parsed.edits {
        out.extend(hunk_lines(
            e.start_line.unwrap_or(0),
            &e.old_text,
            &e.new_text,
        ));
    }

    if !summary.is_empty() {
        out.push(String::new());
        out.push(summary);
    }
    Some(out)
}

/// One anchored edit as unified-diff hunk lines, numbering from `start_line`.
///
/// Shared by the `sed` preview body and [`edit_preview`] so the two display
/// paths cannot drift: both emit `@@ -a,n +a,m @@` with `a = start_line`, then
/// `- `/`+ ` lines (`tool_panel` keys its diff rendering off those markers).
/// An empty NEW (a deletion) simply has no `+` lines.
pub(crate) fn hunk_lines(start_line: u64, old_text: &str, new_text: &str) -> Vec<String> {
    let old_n = old_text.lines().count().max(1) as u64;
    let new_n = new_text.lines().count().max(1) as u64;
    let mut out = Vec::with_capacity(1 + old_n as usize + new_n as usize);
    out.push(format!("@@ -{start_line},{old_n} +{start_line},{new_n} @@"));
    for line in old_text.lines() {
        out.push(format!("- {line}"));
    }
    for line in new_text.lines() {
        out.push(format!("+ {line}"));
    }
    out
}

/// Condense the model-facing banner into one line for a human.
///
/// The banner is written for the model — it ends with an instruction about
/// passing the blocks to `edit`, which is noise to someone reading a panel —
/// so keep the counts and the file, drop the instruction.
pub(crate) fn short_banner(banner: &str) -> String {
    // "sed preview: 3 blocks for src/x.rs (1 expression). Review the blocks ..."
    let head = banner
        .split(". ")
        .next()
        .unwrap_or(banner)
        .trim_end_matches('.');
    let head = head.strip_prefix("sed preview: ").unwrap_or(head);
    let (counts, file) = match head.find(" for ") {
        Some(i) => (&head[..i], head[i + 5..].trim()),
        None => (head, ""),
    };
    // The parenthetical is moved out of the path rather than left to be printed
    // twice.
    let (file, exprs) = match file.split_once(" (") {
        Some((f, rest)) => (f, rest.strip_suffix(')').unwrap_or(rest)),
        None => (file, ""),
    };
    let mut out = String::new();
    if !counts.is_empty() {
        out.push_str(counts);
        // "3 blocks for x" → "3 blocks in x"; keep the counts wording otherwise.
        if !file.is_empty() {
            out.push_str(" in");
        }
    }
    if !file.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(file);
    }
    if !exprs.is_empty() {
        out.push_str(&format!(", from {exprs}"));
    }
    out
}

/// Build a refined unified-diff preview of `old -> new`.
///
/// Uses Myers diff (via `similar`) so only genuinely changed regions show up,
/// each with two lines of context. Changed lines are emitted as `<marker>
/// <content>` (`-` removed, `+` added) and unchanged context as `<marker>
/// <content>` with `<marker>` a single space — the space after the marker keeps
/// all content columns aligned and matches `tool_panel`'s `- ` / `+ ` diff
/// detection. Lines are free of trailing newlines, so `tool_panel` can reflow
/// them without inserting spurious blank rows. A cap keeps pathological
/// replacements (e.g. replacing the whole file) from flooding the scrollback.
pub(crate) fn refined_diff_lines(old: &str, new: &str) -> Vec<String> {
    const MAX: usize = 60;
    const CONTEXT: usize = 2;

    let diff = TextDiff::from_lines(old, new);
    let mut unified = diff.unified_diff();
    unified.context_radius(CONTEXT);

    let mut out: Vec<String> = Vec::new();
    let mut truncated = false;

    for hunk in unified.iter_hunks() {
        for (idx, change) in hunk.iter_changes().enumerate() {
            if idx == 0 {
                // Emit a hunk header as its own neutral context line.
                out.push(format!("{}", hunk.header()));
                if out.len() >= MAX {
                    truncated = true;
                    break;
                }
            }
            if out.len() >= MAX {
                truncated = true;
                break;
            }
            let marker = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            // `from_lines` includes the trailing newline on each value; strip it
            // so `tool_panel` wraps the line as one physical row. The space
            // after the marker keeps `- ` / `+ ` recognizable to `tool_panel`
            // and aligns context lines with changed lines.
            let value = change.value().trim_end_matches('\n');
            out.push(format!("{marker} {value}"));
        }
        if truncated {
            break;
        }
    }

    if truncated {
        out.push("    … (diff truncated)".to_string());
    }
    out
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
    fn an_anchored_edit_previews_real_line_numbers_without_a_diff() {
        // A minimized/replayed block carries `startLine`; the preview must show
        // the real file line directly instead of re-diffing the (already
        // minimal) block and numbering it from 1.
        let args = serde_json::json!({
            "path": "src/x.rs",
            "edits": [{"oldText": "run();", "newText": "walk();", "startLine": 142}]
        });
        let (label, diff) = get_preview("edit", &args);
        assert_eq!(label, "edit src/x.rs [142]");
        assert_eq!(diff, vec!["@@ -142,1 +142,1 @@", "- run();", "+ walk();"]);
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
