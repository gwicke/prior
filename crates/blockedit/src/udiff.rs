//! A real unified diff, for showing a change instead of handing over blocks.
//!
//! `bsed -u` is the one mode whose output is read by a person rather than
//! pasted into `edit`, so it is a unified diff proper: `---`/`+++` headers and
//! `@@` hunk markers, which is what a pager, `patch` and every diff tool expect.
//!
//! This is deliberately *not* the same shape as picocode's in-panel edit preview
//! (`refined_diff_lines` in `src/output.rs`), which is a display format: no file
//! headers at all, a space after each `-`/`+` marker so the terminal panel can
//! align columns, and a 60-line cap because it is about to be drawn in a
//! scrollback. None of that survives a redirect to a file, and a multi-file `-u`
//! run has no other way to say which file a hunk belongs to. The two share the
//! diff itself -- Myers via `similar`, and a line split that keeps carriage
//! returns where they are content -- and nothing else.

use similar::TextDiff;

/// Unchanged lines shown on each side of a change.
pub const DEFAULT_CONTEXT: usize = 2;

/// Render `old -> new` as a unified diff under `label`, or `""` if they are equal.
///
/// The label is used for both `---` and `+++`: this is a preview of one file
/// becoming another, not a patch that applies across two trees, so naming the
/// same path twice says "these are two versions of this file" rather than
/// implying a rename or a source/destination pair.
///
/// Returns `""` for an unchanged input so a caller can skip the file entirely
/// rather than print a header with no hunks under it.
pub fn unified(old: &str, new: &str, label: &str, context: usize) -> String {
    if old == new {
        return String::new();
    }
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    // Diff the line slices rather than the text, so the hunk headers' line
    // numbers count the same lines the caller does. `from_lines` tokenises on
    // its own rule and breaks at a bare `\r`, which `str::lines()` keeps as
    // content -- the two disagree, and the hunk ranges then describe a file
    // that does not exist. `bsed`'s own hunk builder works the same way.
    let diff = TextDiff::from_slices(&old_lines, &new_lines);
    let mut u = diff.unified_diff();
    u.context_radius(context).header(label, label);
    // `to_string` leaves a trailing newline, and the caller decides the
    // separator between files, so it is trimmed here.
    u.to_string().trim_end_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_is_a_unified_diff_with_headers_and_hunks() {
        let d = unified(
            "fn main() {\n    let x = 1;\n}\n",
            "fn main() {\n    let x = 2;\n}\n",
            "src/x.rs",
            DEFAULT_CONTEXT,
        );
        assert!(d.starts_with("--- src/x.rs\n+++ src/x.rs\n"), "{d}");
        assert!(d.contains("@@ -1,3 +1,3 @@"), "{d}");
        assert!(d.contains("-    let x = 1;"), "{d}");
        assert!(d.contains("+    let x = 2;"), "{d}");
        // Context is a leading space, no marker-space, so it is a real diff and
        // not the panel's display form.
        assert!(d.contains("\n fn main() {"), "{d}");
        assert!(!d.contains("- let x"), "panel form leaked in: {d}");
    }

    #[test]
    fn no_change_renders_nothing_at_all() {
        let t = "a\nb\n";
        assert_eq!(unified(t, t, "f.rs", DEFAULT_CONTEXT), "");
    }

    #[test]
    fn context_is_configurable() {
        let old: String = (1..=8).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line5", "CHANGED");
        assert!(!unified(&old, &new, "f", 0).contains("line1"), "0 context");
        assert!(unified(&old, &new, "f", 3).contains("line3"), "3 context");
    }

    /// A carriage return inside a line is content, not a line break. Counting it
    /// as one desynchronises the hunk line numbers from the file.
    #[test]
    fn a_carriage_return_does_not_desynchronise_hunk_numbers() {
        let d = unified("a\rb\nc\n", "a\rb\nC\n", "f", DEFAULT_CONTEXT);
        assert!(d.contains("@@ -1,2 +1,2 @@"), "{d}");
    }

    /// Hunk headers are what a reader uses to find the line in their editor, so
    /// a hunk far from the top has to report its real position.
    #[test]
    fn hunk_headers_carry_real_line_numbers() {
        let old: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let new = old.replace("line15", "CHANGED");
        let d = unified(&old, &new, "f", 1);
        assert!(d.contains("@@ -14,3 +14,3 @@"), "{d}");
    }
}
