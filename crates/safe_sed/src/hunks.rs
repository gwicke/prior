//! Turning "old text, new text" into anchored blocks.
//!
//! The diff is the source of truth. `similar` gives us hunks with line spans;
//! each hunk becomes one `@@@OLD`/`@@@NEW` block whose header anchors to the
//! first line it replaces. Nothing here knows about sed.
//!
//! After the raw hunks exist, four passes adjust them, in this order (each can
//! expose the next):
//!
//! 1. **Leading context** — `context` lines of unchanged text before the change.
//! 2. **Trailing floor** — at least `trailing` lines after, *clamped* so a block
//!    can never reach into the next one (overlapping blocks are rejected by the
//!    `edit` tool, and a rejected batch helps nobody).
//! 3. **Balance growth** — if the block still ends inside an unclosed
//!    `(`/`[`/`{`/string, extend it until the construct closes, capped.
//! 4. **Borrow** — a pure insertion has an empty `OLD`, which the blob parser
//!    rejects outright; take one unchanged neighbour so the block is appliable.
//!
//! Growth and the tail only ever extend a block's *end*, and never into the next
//! hunk, so they can never swallow a line sed actually changed. The anchor — the
//! block's first line — therefore always stays exactly where the diff put it.

use similar::{DiffTag, TextDiff};

use crate::balance::{self, Balance};

/// One appliable block: a line range of the file, and what it becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// 1-based line in the original file where `old_text` starts.
    pub anchor: usize,
    pub old_text: String,
    pub new_text: String,
}

/// How much context to attach, and how far growth may reach.
#[derive(Debug, Clone, Copy)]
pub struct Context {
    /// Unchanged lines to include before the change.
    pub leading: usize,
    /// Minimum unchanged lines to include after the change.
    pub trailing: usize,
    /// Whether to grow a block until its brackets balance.
    pub grow: bool,
    /// Hard cap on lines added by growth.
    pub growth_cap: usize,
}

impl Default for Context {
    fn default() -> Self {
        Context {
            leading: 0,
            trailing: 2,
            grow: true,
            growth_cap: 20,
        }
    }
}

/// A hunk before context is attached: 0-based, half-open line spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Raw {
    /// First line of the block, which may include leading context.
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
    /// First and last lines sed actually changed, ignoring context. Growth is
    /// measured over exactly this span, so a *context* line that happens to
    /// open a brace (`fn main() {`, `if count > 0 {`) can never trigger it and
    /// drag the block across a whole function.
    change_start: usize,
    change_end: usize,
}

/// Blocks plus what the caller needs to explain the result.
#[derive(Debug, Default)]
pub struct Hunks {
    pub blocks: Vec<Block>,
    /// Blocks whose text still ends inside an unclosed construct after growth
    /// (unbalanced source, heredocs, macros). Worth mentioning in the banner.
    pub still_unbalanced: usize,
    /// True when growth added lines to at least one block.
    pub grew_any: bool,
}

/// Build every block for the change from `old` to `new`.
pub fn build(old: &str, new: &str, ctx: Context) -> Hunks {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    // 1. Raw hunks, in file order, with **no** context baked in.
    //
    //    `grouped_ops(n)` pads both sides of every hunk, which is not what we
    //    want: the padding is asymmetric (a floor below, a request above), so
    //    letting `similar` do it would apply the request twice. Asking for 0 and
    //    padding ourselves also means every bound is clamped in one place.
    let raws: Vec<Raw> = grouped(old, new, 0);
    if raws.is_empty() {
        return Hunks::default();
    }

    let mut out = Hunks::default();
    // End of the previous block: both padding directions are clamped against
    // it, so two blocks can never claim the same line.
    let mut prev_end = 0usize;
    for r in raws.iter() {
        // The next hunk's first line is this block's hard ceiling: we may show
        // up to it, never past it.
        let ceiling = raws
            .iter()
            .find(|n| n.change_start >= r.change_end)
            .map(|n| n.change_start)
            .unwrap_or(old_lines.len());

        let mut old_start = r.old_start;
        let mut old_end = r.old_end;
        let mut new_start = r.new_start;
        let mut new_end = r.new_end;

        // 2. Leading context, clamped to the previous block.
        let want_start = r.old_start.saturating_sub(ctx.leading).max(prev_end);
        if want_start < old_start {
            new_start -= old_start - want_start;
            old_start = want_start;
        }

        // 3. Trailing floor: extend, never shrink, and never past the next
        //    block.
        let want = (r.old_end + ctx.trailing).min(ceiling);
        if want > old_end {
            new_end += want - old_end;
            old_end = want;
        }

        // 4. Balance growth, same ceiling, plus its own cap.
        //
        //    Measured over the *change* only — from the first changed line to
        //    the hunk's own end, before any padding was added. Measuring over
        //    the padded block would make our own trailing floor a growth
        //    trigger: a change on `let x = 1;` followed by `if count > 0 {`
        //    would pull the block out to the end of the function, which is the
        //    opposite of what the caller asked for.
        if ctx.grow {
            let grown = grow_to_balanced(
                &old_lines,
                r.change_start,
                r.change_end,
                ceiling,
                ctx.growth_cap,
            );
            if grown > old_end {
                new_end += grown - old_end;
                old_end = grown;
                out.grew_any = true;
            }
        }

        // 5. Borrow for a pure insertion, which otherwise has an empty OLD.
        if old_end <= old_start {
            if old_start < old_lines.len() {
                old_end = old_start + 1;
                new_end = new_end.max(r.new_end) + 1;
            } else if old_start > 0 {
                // Append at EOF: borrow the line *before* the insertion point.
                let shift = 1;
                out.blocks.push(Block {
                    anchor: old_start,
                    old_text: old_lines[old_start - shift].to_string(),
                    new_text: join(&new_lines, new_start.saturating_sub(shift), new_end + shift),
                });
                prev_end = old_start;
                continue;
            } else {
                // An insertion into an empty file has no neighbour to borrow;
                // the caller reports it rather than emitting an empty OLD.
                continue;
            }
        }

        let old_text = join(&old_lines, old_start, old_end);
        let new_text = join(&new_lines, new_start, new_end);
        // Only report a mid-construct block when *the change itself* is
        // unbalanced — i.e. growth ran and could not reach the closer (capped,
        // or blocked by the next hunk). A block that merely ends inside a brace
        // the caller's own trailing context pulled in is not news: they asked
        // for that context.
        if balance::scan(&join(&old_lines, r.change_start, old_end)) == Balance::Unbalanced {
            out.still_unbalanced += 1;
        }
        prev_end = old_end;
        out.blocks.push(Block {
            // 1-based. The anchor is the first line of the block, which is the
            // diff's line unless leading context moved it.
            anchor: old_start + 1,
            old_text,
            new_text,
        });
    }
    out
}

/// The ops in a group that represent an actual change, not context.
fn changed(g: &[similar::DiffOp]) -> impl Iterator<Item = &similar::DiffOp> {
    g.iter().filter(|o| o.tag() != DiffTag::Equal)
}

/// `similar` groups, reduced to the line spans we need.
fn grouped(old: &str, new: &str, leading: usize) -> Vec<Raw> {
    let diff = TextDiff::from_lines(old, new);
    diff.grouped_ops(leading)
        .iter()
        .filter_map(|g| {
            let first = g.first()?;
            let last = g.last()?;
            Some(Raw {
                old_start: first.old_range().start,
                old_end: last.old_range().end,
                new_start: first.new_range().start,
                new_end: last.new_range().end,
                // Skip the Equal ops `grouped_ops` prepends as context: the
                // change starts at the first line sed actually touched.
                change_start: changed(g)
                    .map(|o| o.old_range().start)
                    .min()
                    .unwrap_or(first.old_range().start),
                change_end: changed(g)
                    .map(|o| o.old_range().end)
                    .max()
                    .unwrap_or(last.old_range().end),
            })
        })
        .collect()
}

/// Extend `old_end` until the text from `start` balances, or we hit `ceiling`
/// or the cap. Returns the new end (unchanged when growth does not apply).
///
/// Growing only reads lines the diff already called *unchanged*, which is what
/// makes this safe: the extra text appears in both OLD and NEW verbatim.
fn grow_to_balanced(
    old_lines: &[&str],
    start: usize,
    mut end: usize,
    ceiling: usize,
    cap: usize,
) -> usize {
    let limit = ceiling.min(start + cap);
    loop {
        if end >= limit {
            return end;
        }
        let candidate = join(old_lines, start, end + 1);
        match balance::scan(&candidate) {
            Balance::Balanced => return end + 1,
            // More closers than openers: the construct started before this
            // block, so adding lines cannot fix it.
            Balance::OverClosed => return end,
            Balance::Unbalanced => {
                end += 1;
            }
        }
    }
}

fn join(lines: &[&str], start: usize, end: usize) -> String {
    lines[start.min(lines.len())..end.min(lines.len())].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "fn main() {\n    let x = 1;\n    let y = 2;\n    if count > 0 {\n        run();\n    }\n    // trailing\n    let z = 3;\n}\n";

    fn ctx0() -> Context {
        Context {
            leading: 0,
            trailing: 0,
            grow: false,
            growth_cap: 20,
        }
    }

    #[test]
    fn single_line_change_is_one_anchored_block() {
        let new = FILE.replace("let y = 2;", "let y = 3;");
        let h = build(FILE, &new, ctx0());
        assert_eq!(h.blocks.len(), 1);
        assert_eq!(h.blocks[0].anchor, 3);
        assert_eq!(h.blocks[0].old_text, "    let y = 2;");
        assert_eq!(h.blocks[0].new_text, "    let y = 3;");
    }

    #[test]
    fn deletion_yields_an_empty_new() {
        let new = FILE.replace("    // trailing\n", "");
        let h = build(FILE, &new, ctx0());
        assert_eq!(h.blocks.len(), 1);
        assert_eq!(h.blocks[0].old_text, "    // trailing");
        assert_eq!(h.blocks[0].new_text, "");
    }

    #[test]
    fn insertion_borrows_the_following_line() {
        let new = FILE.replace("    let y = 2;\n", "    let y = 2;\n    let w = 0;\n");
        let h = build(FILE, &new, ctx0());
        assert_eq!(h.blocks.len(), 1);
        // OLD must be non-empty for the blob parser to accept the block.
        assert!(!h.blocks[0].old_text.is_empty());
        // The *following* line is borrowed, so the inserted text lands above it
        // — i.e. exactly where sed put it.
        assert_eq!(h.blocks[0].old_text, "    if count > 0 {");
        assert_eq!(h.blocks[0].new_text, "    let w = 0;\n    if count > 0 {");
        assert_eq!(h.blocks[0].anchor, 4);
    }

    #[test]
    fn insertion_at_bof_borrows_the_first_line() {
        let new = format!("// header\n{FILE}");
        let h = build(FILE, &new, ctx0());
        assert_eq!(h.blocks.len(), 1);
        assert_eq!(h.blocks[0].anchor, 1);
        assert_eq!(h.blocks[0].old_text, "fn main() {");
        assert!(h.blocks[0].new_text.starts_with("// header"));
    }

    #[test]
    fn trailing_floor_is_clamped_against_the_next_block() {
        // Two changes three lines apart: `similar` merges hunks whose gap is
        // within 2*context, so adjacent changes would be one hunk and would not
        // exercise the clamp at all.
        let new = FILE
            .replace("let x = 1;", "let x = 11;")
            .replace("run();", "walk();");
        let h = build(
            FILE,
            &new,
            Context {
                leading: 0,
                trailing: 5,
                grow: false,
                growth_cap: 20,
            },
        );
        assert_eq!(h.blocks.len(), 2);
        let a = &h.blocks[0];
        let b = &h.blocks[1];
        assert!(!a.old_text.contains("walk();"), "{a:?}");
        assert!(!b.old_text.contains("let x = 11;"), "{b:?}");
        // Anchors are still exact, and the ranges stay disjoint.
        assert_eq!(a.anchor, 2);
        assert_eq!(b.anchor, 5);
    }

    #[test]
    fn scattered_duplicates_produce_one_block_each() {
        // Every third line, so each change is its own hunk.
        let old: String = (0..40)
            .map(|i| if i % 3 == 0 { "run();\n" } else { "other();\n" })
            .collect();
        let new: String = (0..40)
            .map(|i| {
                if i % 3 == 0 {
                    "walk();\n"
                } else {
                    "other();\n"
                }
            })
            .collect();
        let h = build(&old, &new, ctx0());
        assert_eq!(h.blocks.len(), 14); // lines 1, 4, 7, ...
        for b in &h.blocks {
            assert_eq!(b.old_text, "run();");
            assert_eq!(b.new_text, "walk();");
        }
        // Anchors land on the changed lines, in order.
        let anchors: Vec<usize> = h.blocks.iter().map(|b| b.anchor).collect();
        assert_eq!(anchors[0], 1);
        assert_eq!(anchors[1], 4);
        assert_eq!(anchors[2], 7);
    }

    #[test]
    fn adjacent_changes_merge_into_one_block() {
        // A contiguous rewrite is one hunk, and one block — which is what the
        // model wants to review anyway.
        let old: String = (0..10).map(|_| "run();\n").collect();
        let new: String = (0..10).map(|_| "walk();\n").collect();
        let h = build(&old, &new, ctx0());
        assert_eq!(h.blocks.len(), 1);
        assert_eq!(h.blocks[0].anchor, 1);
        assert_eq!(h.blocks[0].old_text.lines().count(), 10);
        assert_eq!(h.blocks[0].new_text.lines().count(), 10);
    }

    #[test]
    fn growth_extends_a_block_that_ends_inside_a_call() {
        let file = "let a = 1;\nlet v = compute(\n    x,\n    y,\n);\nlet b = 2;\n";
        // Change only the opening line of the call.
        let new = file.replace("compute(", "compute_all(");
        let h = build(
            file,
            &new,
            Context {
                leading: 0,
                trailing: 0,
                grow: true,
                growth_cap: 20,
            },
        );
        assert_eq!(h.blocks.len(), 1);
        // The block now spans the whole call, so the model can see what it
        // changed and re-indent if it needs to.
        assert!(h.blocks[0].old_text.contains(");"), "{:?}", h.blocks[0]);
        assert_eq!(h.blocks[0].anchor, 2);
        assert!(h.grew_any);
    }

    #[test]
    fn growth_never_crosses_into_the_next_hunk() {
        // An unclosed construct immediately followed by another change: growth
        // must stop at the next hunk rather than swallow it.
        let file = "let v = compute(\n    x,\n    y = 1;\n    z = 2;\n);\n";
        let new = file.replace("y = 1;", "y = 99;");
        let h = build(
            file,
            &new,
            Context {
                leading: 0,
                trailing: 0,
                grow: true,
                growth_cap: 20,
            },
        );
        assert_eq!(h.blocks.len(), 1);
        // The `z = 2` line was not changed, so including it is fine; what must
        // never happen is including it *and* losing the change.
        assert!(
            h.blocks[0].new_text.contains("y = 99;"),
            "{:?}",
            h.blocks[0]
        );
    }

    #[test]
    fn leading_context_does_not_trigger_growth() {
        // `fn main() {` in the *context* is unbalanced, but it is not what sed
        // changed — growing from it would swallow the whole function.
        let new = FILE.replace("let x = 1;", "let x = 11;");
        let h = build(
            FILE,
            &new,
            Context {
                leading: 1,
                trailing: 1,
                grow: true,
                growth_cap: 20,
            },
        );
        assert_eq!(h.blocks.len(), 1);
        assert!(
            !h.grew_any,
            "grew past the requested context: {:?}",
            h.blocks[0]
        );
        // The requested context, and only that: one line before, one after.
        assert_eq!(
            h.blocks[0].old_text,
            "fn main() {\n    let x = 1;\n    let y = 2;"
        );
        assert_eq!(h.blocks[0].anchor, 1);
    }

    #[test]
    fn oversized_single_block_is_still_emitted() {
        // One huge contiguous rewrite is one block; dropping it would leave the
        // model with nothing at all, so the budget defers to having an answer.
        let old: String = (0..500).map(|_| "a line of source here\n").collect();
        let new: String = (0..500).map(|_| "a different line here\n").collect();
        let blocks = build(&old, &new, ctx0()).blocks;
        let r = crate::blocks::render("f.rs", &blocks, 400);
        assert_eq!(r.shown, 1);
        assert_eq!(r.total, 1);
        assert!(r.blob.len() > 400);
    }

    #[test]
    fn identical_text_yields_no_blocks() {
        let h = build(FILE, FILE, ctx0());
        assert!(h.blocks.is_empty());
    }

    #[test]
    fn text_without_trailing_newline_is_still_a_no_op() {
        let file = "a\nb";
        let h = build(file, file, ctx0());
        assert!(h.blocks.is_empty(), "{:?}", h.blocks);
    }
}
