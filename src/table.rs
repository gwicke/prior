//! A small, dependency-light table builder for terminal UIs.
//!
//! Designed for *select overlays* and other fixed-column displays: given a
//! set of columns (with headers + alignment) and a max render width, it
//! computes balanced column widths and renders each row as a single aligned,
//! width-fitted line. Overflow is handled with an ellipsis (positioned per
//! column alignment) rather than wrapping, so every row stays one visual line
//! — which is exactly what the REPL's overlay machinery expects (one option =
//! one line).
//!
//! It deliberately does *not* draw its own borders or move the cursor; callers
//! (e.g. `Repl::overlay_lines`) own the row backgrounds, selection markers,
//! and scrolling. Use [`Table::render`] for selectable lists and
//! [`Table::render_with_header`] for static summaries.

use unicode_width::UnicodeWidthChar;

/// Horizontal alignment of a column's cells (and its header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// A build-once, render-many table of plain-text cells.
///
/// Columns are configured up front (headers, alignment, min width, shrink
/// priority); rows are appended with [`Table::add_row`]; [`Table::render`]
/// produces the aligned lines for a given total width.
#[derive(Debug, Clone)]
pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    /// Minimum visible width per column (never shrink below this).
    min_widths: Vec<usize>,
    /// Shrink priority per column: higher = kept wider when space is tight.
    priorities: Vec<usize>,
    rows: Vec<Vec<String>>,
    /// Spaces between adjacent columns.
    gap: usize,
    /// Character signalling truncated content (default `…`).
    ellipsis: char,
}

impl Table {
    /// Create a table with the given header labels. All columns default to
    /// left-aligned, a min width of 3, and equal shrink priority.
    pub fn new(headers: &[&str]) -> Self {
        let n = headers.len();
        Table {
            headers: headers.iter().map(|s| s.to_string()).collect(),
            aligns: vec![Align::Left; n],
            min_widths: vec![3; n],
            priorities: vec![1; n],
            rows: Vec::new(),
            gap: 2,
            ellipsis: '…',
        }
    }

    /// Set column alignments (extra/short entries are ignored/padded).
    pub fn with_aligns(mut self, aligns: &[Align]) -> Self {
        self.aligns = aligns.to_vec();
        self
    }

    /// Set per-column minimum widths (extra/short entries are ignored/padded).
    pub fn with_min_widths(mut self, mins: &[usize]) -> Self {
        self.min_widths = mins.to_vec();
        self
    }

    /// Set per-column shrink priority (higher = kept wider under pressure).
    pub fn with_priorities(mut self, prio: &[usize]) -> Self {
        self.priorities = prio.to_vec();
        self
    }

    /// Set the number of spaces between columns (default 2).
    pub fn set_gap(mut self, gap: usize) -> Self {
        self.gap = gap;
        self
    }

    /// Set the ellipsis character (default `…`).
    pub fn set_ellipsis(mut self, ellipsis: char) -> Self {
        self.ellipsis = ellipsis;
        self
    }

    /// Append a row. `cells` may be shorter/longer than the column count;
    /// missing cells render as empty, extra cells are ignored.
    pub fn add_row<C: AsRef<str>>(&mut self, cells: &[C]) -> &mut Self {
        let row: Vec<String> = cells.iter().map(|c| c.as_ref().to_string()).collect();
        self.rows.push(row);
        self
    }

    /// Number of data rows added so far.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Render data rows to `max_width` total columns. Each returned string is
    /// exactly one visual line, column-aligned and width-fitted.
    pub fn render(&self, max_width: usize) -> Vec<String> {
        let widths = self.layout(max_width);
        self.rows
            .iter()
            .map(|r| self.render_row(r, &widths))
            .collect()
    }

    /// Render a header (uppercased) + separator + data rows. For static,
    /// non-selectable summaries; do not feed the header/separator lines into a
    /// selectable overlay (they would become selectable items).
    pub fn render_with_header(&self, max_width: usize) -> Vec<String> {
        let widths = self.layout(max_width);
        let header_cells: Vec<String> = self.headers.iter().map(|h| h.to_uppercase()).collect();
        let header = self.render_row(&header_cells, &widths);
        let sep = self.render_sep(&widths);
        let mut out = vec![header, sep];
        out.extend(self.rows.iter().map(|r| self.render_row(r, &widths)));
        out
    }

    // --- internals -------------------------------------------------------

    /// Compute final column widths that sum (with gaps) to <= `max_width`.
    fn layout(&self, max_width: usize) -> Vec<usize> {
        let n = self.headers.len();
        if n == 0 {
            return Vec::new();
        }
        let max_width = max_width.max(n); // at least one char per column
        let gaps = if n > 1 { (n - 1) * self.gap } else { 0 };

        // Effective min per column: caller min, but at least the header width
        // (so headers always fit) and at least 1.
        let min: Vec<usize> = (0..n)
            .map(|i| {
                let h = display_width(&self.headers[i]);
                self.min_widths.get(i).copied().unwrap_or(3).max(h).max(1)
            })
            .collect();

        // Natural content max per column (header included).
        let mut nat = min.clone();
        for col in 0..n {
            let mut w = display_width(&self.headers[col]);
            for r in &self.rows {
                if let Some(c) = r.get(col) {
                    w = w.max(display_width(c));
                }
            }
            nat[col] = w;
        }

        let total_min: usize = min.iter().sum::<usize>() + gaps;
        if total_min >= max_width {
            // Can't even fit the minimums; return them (content ellipsizes).
            return min;
        }
        let total_nat: usize = nat.iter().sum::<usize>() + gaps;
        if total_nat <= max_width {
            return nat; // everything fits naturally, no truncation needed
        }

        // Shrink from natural down toward min until it fits. Greedily reduce
        // the column with the most "shrinkable" space per priority unit.
        let mut cur = nat;
        let deficit = total_nat - max_width;
        for _ in 0..deficit {
            let mut best: Option<usize> = None;
            let mut best_score = 0f64;
            for i in 0..n {
                let shrinkable = cur[i].saturating_sub(min[i]);
                if shrinkable == 0 {
                    continue;
                }
                let prio = self.priorities.get(i).copied().unwrap_or(1).max(1) as f64;
                let score = shrinkable as f64 / prio;
                if score > best_score {
                    best_score = score;
                    best = Some(i);
                }
            }
            match best {
                Some(i) => cur[i] -= 1,
                None => break, // nothing left to shrink
            }
        }
        cur
    }

    /// Render one row (cells or header) into an aligned line of `widths`.
    fn render_row(&self, cells: &[String], widths: &[usize]) -> String {
        let n = self.headers.len();
        let mut s = String::new();
        for i in 0..n {
            let cell = cells.get(i).map(String::as_str).unwrap_or("");
            let align = self.aligns.get(i).copied().unwrap_or(Align::Left);
            let w = widths.get(i).copied().unwrap_or(0);
            if i > 0 {
                s.push_str(&" ".repeat(self.gap));
            }
            s.push_str(&fit(cell, w, align, self.ellipsis));
        }
        s
    }

    /// A separator line of `-` under each column.
    fn render_sep(&self, widths: &[usize]) -> String {
        let n = self.headers.len();
        let mut s = String::new();
        for i in 0..n {
            let w = widths.get(i).copied().unwrap_or(0);
            if i > 0 {
                s.push_str(&" ".repeat(self.gap));
            }
            s.push_str(&"-".repeat(w));
        }
        s
    }
}

/// Visible width of `s`, ignoring ANSI SGR escapes (so styled cells size
/// correctly). Falls back to width 1 for control/ambiguous chars.
fn display_width(s: &str) -> usize {
    let mut w = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip an escape sequence up to and including the final letter.
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        w += char_width(c);
    }
    w
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1)
}

/// Pad `s` (already <= `width` visible) to `width` per `align`.
fn pad(s: &str, width: usize, align: Align) -> String {
    let w = display_width(s);
    if w >= width {
        return s.to_string();
    }
    let pad = width - w;
    match align {
        Align::Left => format!("{}{}", s, " ".repeat(pad)),
        Align::Right => format!("{}{}", " ".repeat(pad), s),
        Align::Center => {
            let l = pad / 2;
            let r = pad - l;
            format!("{}{}{}", " ".repeat(l), s, " ".repeat(r))
        }
    }
}

/// Keep the first `visible` display columns of `s` (char-safe).
fn keep_first(s: &str, visible: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > visible {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// Keep the last `visible` display columns of `s` (char-safe).
fn keep_last(s: &str, visible: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars().rev() {
        let cw = char_width(c);
        if w + cw > visible {
            break;
        }
        out.insert(0, c);
        w += cw;
    }
    out
}

/// Fit `cell` into `width` visible columns, ellipsizing on overflow. The
/// ellipsis is placed at the end (left), start (right), or middle (center).
fn fit(cell: &str, width: usize, align: Align, ellipsis: char) -> String {
    let w = display_width(cell);
    if w <= width {
        return pad(cell, width, align);
    }
    let content_w = width.saturating_sub(1).max(1);
    let with_ell = match align {
        Align::Left => format!("{}{}", keep_first(cell, content_w), ellipsis),
        Align::Right => format!("{}{}", ellipsis, keep_last(cell, content_w)),
        Align::Center => {
            let l = content_w / 2;
            let r = content_w - l;
            format!("{}{}{}", keep_first(cell, l), ellipsis, keep_last(cell, r))
        }
    };
    // with_ell is exactly `width` visible columns wide; pad is a no-op.
    pad(&with_ell, width, align)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_width_fits_without_ellipsis() {
        let mut t = Table::new(&["A", "B"]);
        t.add_row(&["x", "y"]);
        t.add_row(&["longer", "zz"]);
        let rows = t.render(80);
        assert_eq!(rows.len(), 2);
        // Natural widths: A=6 ("longer"), B=2 ("zz"), gap=2 => total 10. Both
        // rows share that width and the 2nd column starts at the same offset.
        assert_eq!(display_width(&rows[0]), 10);
        assert_eq!(display_width(&rows[1]), 10);
        let col1 = 6 + 2;
        assert!(rows[0][col1..].starts_with('y'));
        assert!(rows[1][col1..].starts_with("zz"));
    }

    #[test]
    fn narrow_width_balances_and_stays_within_budget() {
        let mut t = Table::new(&["Model", "Ctx", "Cost"])
            .with_aligns(&[Align::Left, Align::Right, Align::Right])
            .with_min_widths(&[8, 4, 8])
            .with_priorities(&[3, 1, 2]);
        // Natural widths ~ Model=24, Ctx=7, Cost=16 (+gaps 4) = 51.
        for _ in 0..3 {
            t.add_row(&[
                "openrouter/tencent/hy3:free-long-name",
                "1048576",
                "$0.123/$0.456",
            ]);
        }
        let max = 40;
        let rows = t.render(max);
        assert_eq!(rows.len(), 3);
        for r in &rows {
            assert!(
                display_width(r) <= max,
                "row exceeds budget: {:?} (width {})",
                r,
                display_width(r)
            );
        }
        // The Model column (highest priority) should keep the most width.
        let widths: Vec<usize> = rows[0].split(' ').map(display_width).collect();
        assert!(widths[0] >= widths[2], "model col kept wider: {:?}", rows[0]);
    }

    #[test]
    fn left_right_center_ellipsis_positions() {
        let mut t = Table::new(&["L", "R", "C"])
            .with_aligns(&[Align::Left, Align::Right, Align::Center])
            .with_min_widths(&[3, 3, 3]);
        t.add_row(&["abcdef", "123456", "ghijkl"]);
        // Width 5 -> content 4 + ellipsis 1.
        let rows = t.render(5 + 2 + 5 + 2 + 5);
        let r = &rows[0];
        // Split on the 2-space gap.
        let parts: Vec<&str> = r.split("  ").collect();
        assert_eq!(parts.len(), 3);
        assert!(parts[0].ends_with('…'), "left ellipsis at end: {:?}", parts[0]);
        assert!(parts[1].starts_with('…'), "right ellipsis at start: {:?}", parts[1]);
        assert!(parts[2].contains('…'), "center ellipsis inside: {:?}", parts[2]);
        for p in &parts {
            assert_eq!(display_width(p), 5, "part width: {:?}", p);
        }
    }

    #[test]
    fn ansi_in_cells_ignored_for_sizing() {
        let styled = "\x1b[2mdim\x1b[0m";
        let mut t = Table::new(&["H"]);
        t.add_row(&[styled]);
        let rows = t.render(80);
        // "dim" is 3 visible; padded to the natural header width 1 -> "dim".
        assert_eq!(display_width(&rows[0]), 3);
        assert!(rows[0].contains("dim"));
        assert!(!rows[0].contains("  "));
    }

    #[test]
    fn render_with_header_includes_separator() {
        let mut t = Table::new(&["Name", "N"]).with_aligns(&[Align::Left, Align::Right]);
        t.add_row(&["alpha", "1"]);
        t.add_row(&["beta", "22"]);
        let out = t.render_with_header(80);
        assert_eq!(out.len(), 4); // header + sep + 2 rows
        // Column widths: Name=5, N=2, gap=2 => every line is width 9.
        for line in &out {
            assert_eq!(display_width(line), 9);
        }
        // Columns align: col1 ("N"/"1"/"22") starts at offset 5+2=7 (right-
        // aligned, so check the trimmed cell content).
        let c1 = 5 + 2;
        assert_eq!(out[0][c1..].trim(), "N");
        assert_eq!(out[2][c1..].trim(), "1");
        assert_eq!(out[3][c1..].trim(), "22");
        // Separator is dashes + the inter-column gap.
        assert!(out[1].chars().all(|ch| ch == '-' || ch == ' '));
    }
}
