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
//! (e.g. `render::paint_rows`, which paints backgrounds and selection markers,
//! and `ui::Element::data_rows`, which windows them) own the presentation. Use
//! [`Table::render`] for selectable lists and [`Table::render_with_header`]
//! for static summaries.

use unicode_width::UnicodeWidthChar;

use crate::wrap::{normalize, wrap};

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
            gap: 1,
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

    /// Set the number of spaces between columns (default 1).
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
    /// missing cells render as empty, extra cells are ignored. Cells are
    /// normalized (`wrap`'s control-character contract) on the way in, so the
    /// widths [`Table::layout`] measures match what wrapping and painting
    /// later render for the same bytes.
    pub fn add_row<C: AsRef<str>>(&mut self, cells: &[C]) -> &mut Self {
        let row: Vec<String> = cells
            .iter()
            .map(|c| normalize(c.as_ref()).into_owned())
            .collect();
        self.rows.push(row);
        self
    }

    /// Number of data rows added so far.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Column widths that [`Table::render`] / [`Table::render_multiline`] will
    /// use for `max_width`. Exposed so callers can compute where a column
    /// starts (e.g. to wrap a description and keep continuations aligned).
    pub fn column_widths(&self, max_width: usize) -> Vec<usize> {
        self.layout(max_width)
    }

    /// Render data rows to `max_width` total columns, **wrapping** long cells
    /// (e.g. a description) across multiple visual lines instead of
    /// truncating them. Each returned `String` is one *logical* row and may
    /// itself contain `\n` (one visual line per wrapped cell line). All visual
    /// lines stay column-aligned: continuation lines leave the other columns
    /// blank. Pair this with an overlay renderer that splits on `\n` so a
    /// wrapped logical row is treated as one selectable item (arrow keys move
    /// by logical row, not by visual line).
    ///
    /// ```
    /// # use slashdot_menu::table::{Align, Table};
    /// let mut t = Table::new(&["Setting", "Description"])
    ///     .with_aligns(&[Align::Left, Align::Left]);
    /// t.add_row(&["default_model", "a fairly long description that must wrap"]);
    /// let rows = t.render_multiline(40);
    /// assert!(rows[0].contains('\n'), "long cell wraps to multiple visual lines");
    /// # assert_eq!(rows[0].split('\n').count(), 2);
    /// ```
    pub fn render_multiline(&self, max_width: usize) -> Vec<String> {
        let widths = self.layout(max_width);
        let mut out = Vec::new();
        for row in &self.rows {
            // Wrap each cell independently to its column width.
            let wrapped: Vec<Vec<String>> = (0..widths.len())
                .map(|col| match row.get(col).map(String::as_str) {
                    Some("") | None => vec![String::new()],
                    Some(cell) => wrap(cell, widths[col]),
                })
                .collect();
            let max_lines = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let mut lines: Vec<String> = Vec::with_capacity(max_lines);
            for k in 0..max_lines {
                let mut s = String::new();
                for (col, cell_lines) in wrapped.iter().enumerate() {
                    if col > 0 {
                        s.push_str(&" ".repeat(self.gap));
                    }
                    let chunk = cell_lines.get(k).map(String::as_str).unwrap_or("");
                    let align = self.aligns.get(col).copied().unwrap_or(Align::Left);
                    s.push_str(&fit(chunk, widths[col], align, self.ellipsis));
                }
                lines.push(s);
            }
            out.push(lines.join("\n"));
        }
        out
    }

    /// Render data rows to `max_width` total columns. Each returned string is
    /// exactly one visual line, column-aligned and width-fitted.
    /// Embedded `\n` in cells are replaced with spaces (single-line mode).
    pub fn render(&self, max_width: usize) -> Vec<String> {
        let widths = self.layout(max_width);
        self.rows
            .iter()
            .map(|r| {
                let flat: Vec<String> = r.iter().map(|c| c.replace('\n', " ")).collect();
                self.render_row(&flat, &widths)
            })
            .collect()
    }

    /// Render just the header row with proper column alignment.
    pub fn render_header(&self, max_width: usize) -> String {
        let widths = self.layout(max_width);
        let header_cells: Vec<String> = self.headers.iter().map(|h| h.to_uppercase()).collect();
        self.render_row(&header_cells, &widths)
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
        for (col, (slot, header)) in nat.iter_mut().zip(&self.headers).enumerate() {
            let w = self
                .rows
                .iter()
                .fold(display_width(header), |w, r| match r.get(col) {
                    Some(c) => w.max(display_width(c)),
                    None => w,
                });
            *slot = w;
        }

        let total_min: usize = min.iter().sum::<usize>() + gaps;
        if total_min >= max_width {
            // Can't even fit the minimums; return them (content ellipsizes).
            return min;
        }
        let total_nat: usize = nat.iter().sum::<usize>() + gaps;
        if total_nat <= max_width {
            // Everything fits naturally. Grow the last (typically descriptive)
            // column to absorb the slack so the row fills `max_width` and stays
            // column-aligned — no empty gutter on the right of wide terminals,
            // and the explanation column gets the extra room instead of being
            // ellipsized early.
            let mut cur = nat;
            let slack = max_width - total_nat;
            if let Some(last) = cur.last_mut() {
                *last += slack;
            }
            return cur;
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

pub use slashdot_tui::ansi::display_width;

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1)
}

/// Split `s` into renderable units: `(byte length, visible width,
/// is_escape)`. An SGR sequence is one atomic unit of width 0, so
/// truncation can never cut a styled run in half.
fn style_units(s: &str) -> Vec<(usize, usize, bool)> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (off, c) = chars[i];
        if c == '\x1b' {
            let mut j = i + 1;
            while j < chars.len() && !chars[j].1.is_ascii_alphabetic() {
                j += 1;
            }
            let end = if j < chars.len() {
                chars[j].0 + chars[j].1.len_utf8()
            } else {
                s.len()
            };
            out.push((end - off, 0, true));
            i = j + 1;
        } else {
            out.push((c.len_utf8(), char_width(c), false));
            i += 1;
        }
    }
    out
}

/// Keep the first (`from_end == false`) or last `visible` display columns
/// of `s`, ANSI-aware: escape sequences ride along without consuming
/// width, and a sequence is never cut in half. `fit` re-emits the reset
/// once the ellipsis is composed — that is where the style would
/// otherwise bleed out of the row.
fn keep_visible(s: &str, visible: usize, from_end: bool) -> String {
    let units = style_units(s);
    let mut w = 0;
    if from_end {
        let mut start = units.len();
        while start > 0 {
            let (_, cw, _) = units[start - 1];
            if w + cw > visible {
                break;
            }
            w += cw;
            start -= 1;
        }
        let mut off = 0;
        for (len, _, _) in units.iter().take(start) {
            off += len;
        }
        s[off..].to_string()
    } else {
        let mut end = 0;
        while end < units.len() {
            let (_, cw, _) = units[end];
            if w + cw > visible {
                break;
            }
            w += cw;
            end += 1;
        }
        let mut off = 0;
        for (len, _, _) in units.iter().take(end) {
            off += len;
        }
        s[..off].to_string()
    }
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

/// Keep the first `visible` display columns of `s` (char-safe,
/// ANSI-aware).
fn keep_first(s: &str, visible: usize) -> String {
    keep_visible(s, visible, false)
}

/// Keep the last `visible` display columns of `s` (char-safe,
/// ANSI-aware).
fn keep_last(s: &str, visible: usize) -> String {
    keep_visible(s, visible, true)
}

/// Fit `cell` into `width` visible columns, ellipsizing on overflow. The
/// ellipsis is placed at the end (left), start (right), or middle (center).
fn fit(cell: &str, width: usize, align: Align, ellipsis: char) -> String {
    let w = display_width(cell);
    if w <= width {
        return pad(cell, width, align);
    }
    let content_w = width.saturating_sub(1).max(1);
    let mut with_ell = match align {
        Align::Left => format!("{}{}", keep_first(cell, content_w), ellipsis),
        Align::Right => format!("{}{}", ellipsis, keep_last(cell, content_w)),
        Align::Center => {
            let l = content_w / 2;
            let r = content_w - l;
            format!("{}{}{}", keep_first(cell, l), ellipsis, keep_last(cell, r))
        }
    };
    // A styled cell truncated mid-run would bleed its SGR past the row
    // (and past the terminal line). Close the style after the ellipsis.
    if cell.contains('\x1b') && !with_ell.ends_with("\x1b[0m") {
        with_ell.push_str("\x1b[0m");
    }
    // with_ell is exactly `width` visible columns wide; pad is a no-op.
    pad(&with_ell, width, align)
}

/// ANSI escape codes for match highlighting.
/// Match highlight: bold + magenta foreground only. The end sequence resets
/// *only* bold and foreground (SGR 22;39) so any row background / selection
/// styling applied by the host survives the highlight.
const MATCH_HIGHLIGHT_START: &str = "\x1b[1;35m";
const MATCH_HIGHLIGHT_END: &str = "\x1b[22;39m";

/// Max number of visible columns between two match runs that still counts as
/// "one cluster" (shown as a single snippet span). Longer gaps become separate
/// spans joined by `…`.
const SNIPPET_CLUSTER_GAP: usize = 4;

/// Extract a snippet from `cell` centered on `match_indices` (byte offsets),
/// at most `max_width` visible columns wide (ANSI codes excluded), with the
/// matched characters highlighted.
///
/// The window is chosen to use the *full* available width even when the match
/// sits at the very start or end of the cell (asymmetric context): the slack
/// the match cannot use on one side is re-allocated to the other. A `…` is
/// emitted for each side that was trimmed, reserving a column for it.
pub fn extract_snippet(cell: &str, match_indices: &[usize], max_width: usize) -> String {
    if match_indices.is_empty() {
        return cell.to_string();
    }
    let cell_width = display_width(cell);
    if cell_width <= max_width {
        // Fits: highlight in place.
        return highlight_matches(cell, match_indices);
    }

    // Per-character tables: byte → char index, char → byte index / visible col.
    // ANSI-aware: an SGR sequence occupies characters but no columns, so
    // the window arithmetic below is in *visible* columns while the slice
    // still re-emits the escapes verbatim. (A styled cell reaching this
    // path is the norm: the caller highlights before rendering.)
    let chars: Vec<char> = cell.chars().collect();
    let n_chars = chars.len();
    let mut byte_to_char = vec![0usize; cell.len() + 1];
    let mut char_to_visible = Vec::with_capacity(n_chars + 1);
    let mut visible = 0;
    char_to_visible.push(0);
    // Mark the characters that belong to SGR sequences (ESC .. final
    // letter) so they occupy characters but no visible columns.
    let mut in_escape = vec![false; n_chars];
    let mut k = 0;
    while k < n_chars {
        if chars[k] == '\x1b' {
            let mut j = k + 1;
            while j < n_chars && !chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            let end = (j + 1).min(n_chars);
            for slot in in_escape.iter_mut().take(end).skip(k) {
                *slot = true;
            }
            k = end;
        } else {
            k += 1;
        }
    }

    let mut byte_off = 0usize;
    for (i, c) in chars.iter().enumerate() {
        byte_to_char[byte_off] = i;
        byte_off += c.len_utf8();
        if !in_escape[i] {
            visible += char_width(*c);
        }
        char_to_visible.push(visible);
    }
    let vis = |ci: usize| char_to_visible[ci];

    // Matched char set.
    let mut matched = std::collections::HashSet::new();
    for &b in match_indices {
        if b < byte_to_char.len() {
            matched.insert(byte_to_char[b]);
        }
    }

    // Contiguous match runs → clusters (runs separated by more than
    // SNIPPET_CLUSTER_GAP visible columns become distinct spans).
    let mut clusters: Vec<(usize, usize)> = Vec::new(); // char ranges [start,end)
    {
        let mut i = 0;
        while i < n_chars {
            if matched.contains(&i) {
                let start = i;
                let mut end = i + 1;
                while end < n_chars && matched.contains(&end) {
                    end += 1;
                }
                // Merge with previous cluster if the visible gap is small.
                if let Some(last) = clusters.last_mut() {
                    if vis(start).saturating_sub(vis(last.1)) <= SNIPPET_CLUSTER_GAP {
                        last.1 = end;
                        i = end;
                        continue;
                    }
                }
                clusters.push((start, end));
                i = end;
            } else {
                i += 1;
            }
        }
    }
    if clusters.is_empty() {
        return cell.to_string();
    }

    // Single cluster: one contiguous window with maximal context (asymmetric
    // slack re-flow so near-edge matches still fill the width).
    if clusters.len() == 1 {
        let (cs, ce) = clusters[0];
        let (ms, me) = (vis(cs), vis(ce));
        let match_w = me - ms;
        let (mut w_start, mut w_end) = if match_w >= max_width {
            (ms, (ms + max_width).min(cell_width))
        } else {
            let slack = max_width - match_w;
            let s = ms.saturating_sub(slack / 2);
            let e = (me + (slack - (ms - s))).min(cell_width);
            let s = s.saturating_sub(max_width.saturating_sub(e - s));
            let e = if e < cell_width {
                (s + max_width).min(cell_width)
            } else {
                e
            };
            (s, e)
        };
        // Reserve a column per ellipsis.
        loop {
            let le = (w_start > 0) as usize;
            let re = (w_end < cell_width) as usize;
            let total = (w_end - w_start) + le + re;
            if total <= max_width {
                break;
            }
            let over = total - max_width;
            if w_end == cell_width {
                w_start += over;
            } else {
                w_end = w_end.saturating_sub(over);
            }
        }
        let start_char = char_to_visible
            .iter()
            .position(|&v| v >= w_start)
            .unwrap_or(0);
        let end_char = char_to_visible
            .iter()
            .position(|&v| v >= w_end)
            .unwrap_or(n_chars);
        let (s, e) = tighten(
            &chars,
            &matched,
            start_char,
            end_char,
            start_char > 0,
            end_char < n_chars,
        );
        let mut out = String::new();
        if s > 0 {
            out.push('…');
        }
        out.push_str(&render_highlighted(&chars, &matched, s, e));
        if e < n_chars {
            out.push('…');
        }
        return out;
    }

    // Multiple clusters: show each as a context-padded span joined by `…`,
    // sharing the budget. If they cannot all fit, fall back to a single
    // window covering everything (current behaviour).
    let interior = clusters.len() - 1;
    let total_match: usize = clusters.iter().map(|(s, e)| vis(*e) - vis(*s)).sum();
    if total_match + interior >= max_width {
        // Cannot show several spans — degrade to the single-window behaviour
        // over the whole match extent by treating all clusters as one.
        let ms = vis(clusters[0].0);
        let me = vis(clusters.last().unwrap().1);
        return single_extent_snippet(
            &chars,
            &matched,
            &char_to_visible,
            ms,
            me,
            cell_width,
            max_width,
        );
    }

    // Equal context share per cluster (remaining budget after matches and the
    // interior ellipses).
    let ctx_budget = max_width - total_match - interior;
    let per = ctx_budget / clusters.len();
    let mut segs: Vec<(usize, usize)> = Vec::with_capacity(clusters.len());
    for (s, e) in &clusters {
        let left = per / 2;
        let right = per - left;
        // Approximate context in characters (matches are width-1 chars in the
        // common ASCII path; wide chars just yield slightly tighter spans).
        let ls = s.saturating_sub(left);
        let le = (e + right).min(n_chars);
        segs.push((ls, le));
    }
    // Shrink while over budget, preferring the widest segment's larger side.
    let seg_w = |(s, e): (usize, usize)| vis(e) - vis(s);
    loop {
        let leading = (segs[0].0 > 0) as usize;
        let trailing = (segs[segs.len() - 1].1 < n_chars) as usize;
        let total: usize =
            segs.iter().map(|&s| seg_w(s)).sum::<usize>() + interior + leading + trailing;
        if total <= max_width {
            break;
        }
        // Find the segment with the most shrinkable context.
        let mut idx = None;
        let mut best = 0usize;
        for (i, &(s, e)) in segs.iter().enumerate() {
            let w = seg_w((s, e));
            if w > best && w > vis(clusters[i].1) - vis(clusters[i].0) {
                best = w;
                idx = Some(i);
            }
        }
        let Some(i) = idx else { break };
        let (s, e) = segs[i];
        let lctx = vis(clusters[i].0) - vis(s);
        let rctx = vis(e) - vis(clusters[i].1);
        if lctx >= rctx {
            segs[i].0 = s + 1;
        } else {
            segs[i].1 = e - 1;
        }
    }

    let n_segs = segs.len();
    let mut out = String::new();
    for (i, &(s, e)) in segs.iter().enumerate() {
        let left_ell = i > 0 || s > 0;
        let right_ell = i < n_segs - 1 || e < n_chars;
        let (ts, te) = tighten(&chars, &matched, s, e, left_ell, right_ell);
        if i == 0 {
            if ts > 0 {
                out.push('…');
            }
        } else {
            out.push('…');
        }
        out.push_str(&render_highlighted(&chars, &matched, ts, te));
    }
    if segs[n_segs - 1].1 < n_chars {
        out.push('…');
    }
    out
}

/// Tighten a segment's bounds by trimming whitespace that sits against an
/// ellipsis boundary (a space next to `…` adds no information). Never trims a
/// matched character or crosses the match content.
fn tighten(
    chars: &[char],
    matched: &std::collections::HashSet<usize>,
    start: usize,
    end: usize,
    left_ellipsis: bool,
    right_ellipsis: bool,
) -> (usize, usize) {
    let mut s = start;
    let mut e = end;
    if left_ellipsis {
        while s < e && chars[s].is_whitespace() && !matched.contains(&s) {
            s += 1;
        }
    }
    if right_ellipsis {
        while e > s && chars[e - 1].is_whitespace() && !matched.contains(&(e - 1)) {
            e -= 1;
        }
    }
    (s, e)
}

/// Render `chars[start,end)` with ANSI highlight runs (no ellipses).
fn render_highlighted(
    chars: &[char],
    matched: &std::collections::HashSet<usize>,
    start: usize,
    end: usize,
) -> String {
    let mut out = String::new();
    let mut in_match = false;
    for (i, c) in chars.iter().enumerate().take(end).skip(start) {
        let is_matched = matched.contains(&i);
        if is_matched && !in_match {
            out.push_str(MATCH_HIGHLIGHT_START);
            in_match = true;
        } else if !is_matched && in_match {
            out.push_str(MATCH_HIGHLIGHT_END);
            in_match = false;
        }
        out.push(*c);
    }
    if in_match {
        out.push_str(MATCH_HIGHLIGHT_END);
    }
    out
}

/// Single contiguous window covering visible extents `[ms, me)` with maximal
/// context and ellipsis reservation (the pre-clustering behaviour).
fn single_extent_snippet(
    chars: &[char],
    matched: &std::collections::HashSet<usize>,
    char_to_visible: &[usize],
    ms: usize,
    me: usize,
    cell_width: usize,
    max_width: usize,
) -> String {
    let n_chars = chars.len();
    let match_w = me.saturating_sub(ms);
    let (mut w_start, mut w_end) = if match_w >= max_width {
        (ms, (ms + max_width).min(cell_width))
    } else {
        let slack = max_width - match_w;
        let s = ms.saturating_sub(slack / 2);
        let e = (me + (slack - (ms - s))).min(cell_width);
        let s = s.saturating_sub(max_width.saturating_sub(e - s));
        let e = if e < cell_width {
            (s + max_width).min(cell_width)
        } else {
            e
        };
        (s, e)
    };
    loop {
        let le = (w_start > 0) as usize;
        let re = (w_end < cell_width) as usize;
        let total = (w_end - w_start) + le + re;
        if total <= max_width {
            break;
        }
        let over = total - max_width;
        if w_end == cell_width {
            w_start += over;
        } else {
            w_end = w_end.saturating_sub(over);
        }
    }
    let start_char = char_to_visible
        .iter()
        .position(|&v| v >= w_start)
        .unwrap_or(0);
    let end_char = char_to_visible
        .iter()
        .position(|&v| v >= w_end)
        .unwrap_or(n_chars);
    let (s, e) = tighten(
        chars,
        matched,
        start_char,
        end_char,
        start_char > 0,
        end_char < n_chars,
    );
    let mut out = String::new();
    if s > 0 {
        out.push('…');
    }
    out.push_str(&render_highlighted(chars, matched, s, e));
    if e < n_chars {
        out.push('…');
    }
    out
}

/// Apply ANSI highlighting to matched byte indices in `text`.
/// Public so view.rs can use it for columns that don't need snippet extraction.
pub fn highlight_matches(text: &str, match_indices: &[usize]) -> String {
    if match_indices.is_empty() {
        return text.to_string();
    }
    // Convert byte indices to char indices for easier processing
    let mut byte_to_char = Vec::with_capacity(text.len() + 1);
    let mut char_idx = 0;
    for (byte_idx, _) in text.char_indices() {
        while byte_to_char.len() <= byte_idx {
            byte_to_char.push(char_idx);
        }
        char_idx += 1;
    }
    while byte_to_char.len() <= text.len() {
        byte_to_char.push(char_idx);
    }

    // Build a set of matched char indices for O(1) lookup
    let mut matched_chars = std::collections::HashSet::new();
    for &byte_idx in match_indices {
        if byte_idx < byte_to_char.len() {
            matched_chars.insert(byte_to_char[byte_idx]);
        }
    }

    // Build result with ANSI codes around matched character runs
    let mut result = String::new();
    let mut in_match = false;
    for (c_idx, c) in text.chars().enumerate() {
        let is_matched = matched_chars.contains(&c_idx);
        if is_matched && !in_match {
            result.push_str(MATCH_HIGHLIGHT_START);
            in_match = true;
        } else if !is_matched && in_match {
            result.push_str(MATCH_HIGHLIGHT_END);
            in_match = false;
        }
        result.push(c);
    }
    if in_match {
        result.push_str(MATCH_HIGHLIGHT_END);
    }
    result
}

#[cfg(test)]
mod snippet_tests {
    use super::*;

    #[test]
    fn snippet_no_indices_returns_original() {
        let cell = "hello world";
        assert_eq!(extract_snippet(cell, &[], 20), "hello world");
    }

    #[test]
    fn snippet_fits_in_width_highlights_matches() {
        let cell = "hello world";
        // "w" at byte 6 is matched - should be highlighted.
        let snippet = extract_snippet(cell, &[6], 20);
        assert!(
            snippet.contains("\x1b[1;35mw\x1b[22;39m"),
            "snippet: {:?}",
            snippet
        );
        assert!(snippet.contains("hello "));
        assert!(snippet.contains("orld"));
    }

    #[test]
    fn snippet_extracts_around_match_within_budget() {
        let cell = "this is a very long path with many directories";
        let path_start = cell.find("path").unwrap();
        let indices = (path_start..path_start + 4).collect::<Vec<_>>();
        let snippet = extract_snippet(cell, &indices, 35);
        assert!(snippet.contains("path"), "snippet: {}", snippet);
        assert!(
            display_width(&snippet) <= 35,
            "visual width: {}",
            display_width(&snippet)
        );
    }

    #[test]
    fn snippet_match_at_start_fills_full_width() {
        // A match at the very beginning must not leave the right half empty:
        // the slack the match cannot use on the left is re-allocated right.
        let cell = "model_with_a_very_long_suffix_indeed";
        let indices = vec![0, 1, 2, 3, 4];
        let snippet = extract_snippet(cell, &indices, 20);
        assert!(
            snippet.starts_with("\x1b[1;35mmodel\x1b[22;39m"),
            "snippet: {:?}",
            snippet
        );
        // No leading ellipsis (nothing trimmed at the start) and full width used.
        assert!(!snippet.starts_with('…'));
        assert_eq!(display_width(&snippet), 20, "snippet: {:?}", snippet);
    }

    #[test]
    fn snippet_match_at_end_fills_full_width() {
        let cell = "a_very_long_prefix_then_model";
        let m = cell.find("model").unwrap();
        let indices: Vec<usize> = (m..m + 5).collect();
        let snippet = extract_snippet(cell, &indices, 20);
        assert!(
            snippet.ends_with("model\x1b[22;39m"),
            "snippet: {:?}",
            snippet
        );
        assert!(snippet.starts_with('…'));
        assert_eq!(display_width(&snippet), 20, "snippet: {:?}", snippet);
    }

    #[test]
    fn snippet_ellipsis_is_tight() {
        // Whitespace adjacent to a trimming ellipsis is dropped.
        let cell = "one two three TARGET four five six seven";
        let t = cell.find("TARGET").unwrap();
        let indices: Vec<usize> = (t..t + 6).collect();
        let snippet = extract_snippet(cell, &indices, 16);
        assert!(
            !snippet.contains("… "),
            "leading space after ellipsis: {:?}",
            snippet
        );
        assert!(
            !snippet.contains(" …"),
            "space before ellipsis: {:?}",
            snippet
        );
        assert!(snippet.contains("TARGET"), "{:?}", snippet);
    }

    #[test]
    fn snippet_multiple_spans_when_matches_far_apart() {
        // Two matches far apart: both must be visible, joined by an ellipsis.
        let cell = "alpha_one_two_three_four_five_six_seven_eight_nine_omega";
        let a = cell.find("alpha").unwrap();
        let o = cell.find("omega").unwrap();
        let mut indices: Vec<usize> = (a..a + 5).collect();
        indices.extend(o..o + 5);
        let snippet = extract_snippet(cell, &indices, 30);
        assert!(snippet.contains("alpha"), "snippet: {:?}", snippet);
        assert!(snippet.contains("omega"), "snippet: {:?}", snippet);
        // The interior gap is elided (an interior ellipsis is present).
        assert!(snippet.matches('…').count() >= 1, "snippet: {:?}", snippet);
        assert!(
            display_width(&snippet) <= 30,
            "w={}",
            display_width(&snippet)
        );
    }

    #[test]
    fn snippet_handles_unicode() {
        let cell = "path/to/日本語/dir";
        let match_start = cell.find("日本語").unwrap();
        let match_end = match_start + "日本語".len();
        let indices: Vec<usize> = (match_start..match_end).collect();
        let snippet = extract_snippet(cell, &indices, 15);
        assert!(snippet.contains("日本語"), "snippet: {}", snippet);
        assert!(
            snippet.contains("\x1b[1;35m"),
            "should highlight unicode match"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_width_fills_slack_into_last_column() {
        let mut t = Table::new(&["A", "B"]);
        t.add_row(&["x", "y"]);
        t.add_row(&["longer", "zz"]);
        let rows = t.render(80);
        assert_eq!(rows.len(), 2);
        // Natural widths: A=6 ("longer"), B=2 ("zz"), gap=1 => total 9, which
        // fits in 80. The 71 cols of slack are absorbed by the last column, so
        // every row spans the full width and columns stay aligned.
        assert_eq!(display_width(&rows[0]), 80);
        assert_eq!(display_width(&rows[1]), 80);
        let col1 = 6 + 1;
        assert!(rows[0][col1..].starts_with('y'));
        assert!(rows[1][col1..].starts_with("zz"));
        // B is left-aligned within its grown column, so trailing padding only.
        // (Col A is padded to its natural width 6, then a 1-space gap, then B.)
        assert_eq!(rows[0].trim_end(), "x      y");
        assert_eq!(rows[1].trim_end(), "longer zz");
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
        assert!(
            widths[0] >= widths[2],
            "model col kept wider: {:?}",
            rows[0]
        );
    }

    #[test]
    fn left_right_center_ellipsis_positions() {
        let mut t = Table::new(&["L", "R", "C"])
            .with_aligns(&[Align::Left, Align::Right, Align::Center])
            .with_min_widths(&[3, 3, 3]);
        t.add_row(&["abcdef", "123456", "ghijkl"]);
        // Width 5 -> content 4 + ellipsis 1.
        let rows = t.render(5 + 1 + 5 + 1 + 5);
        let r = &rows[0];
        // Split on the 1-space gap.
        let parts: Vec<&str> = r.split(' ').collect();
        assert_eq!(parts.len(), 3);
        assert!(
            parts[0].ends_with('…'),
            "left ellipsis at end: {:?}",
            parts[0]
        );
        assert!(
            parts[1].starts_with('…'),
            "right ellipsis at start: {:?}",
            parts[1]
        );
        assert!(
            parts[2].contains('…'),
            "center ellipsis inside: {:?}",
            parts[2]
        );
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
        // "dim" is 3 visible; the single column grows to fill the width, so
        // the row is 80 wide but the visible content is just "dim".
        assert_eq!(display_width(&rows[0]), 80);
        assert!(rows[0].contains("dim"));
        // The styled content is preserved exactly, then padded to the right.
        assert_eq!(rows[0].trim_end(), "\x1b[2mdim\x1b[0m");
    }

    #[test]
    fn render_with_header_includes_separator() {
        let mut t = Table::new(&["Name", "N"]).with_aligns(&[Align::Left, Align::Right]);
        t.add_row(&["alpha", "1"]);
        t.add_row(&["beta", "22"]);
        let out = t.render_with_header(80);
        assert_eq!(out.len(), 4); // header + sep + 2 rows
                                  // Column widths: Name=5, N grows to fill the slack => every line is
                                  // width 80 (Name=5, gap=2, N=73).
        for line in &out {
            assert_eq!(display_width(line), 80);
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

    #[test]
    fn render_multiline_wraps_long_cell_and_aligns_continuations() {
        let mut t = Table::new(&["A", "B"])
            .with_aligns(&[Align::Left, Align::Left])
            .with_min_widths(&[4, 20]);
        t.add_row(&[
            "x",
            "a fairly long description that must wrap across multiple visual lines",
        ]);
        let rows = t.render_multiline(30);
        assert_eq!(rows.len(), 1, "one logical row");
        let lines: Vec<&str> = rows[0].split('\n').collect();
        assert!(lines.len() >= 2, "long cell should wrap");
        // All visual lines share one total width (columns stay aligned).
        let w = lines[0].len();
        assert!(
            lines.iter().all(|l| l.len() == w),
            "visual lines must be equal width"
        );
        // Continuation lines leave column A blank (aligned under the first).
        let a_w = t.column_widths(30)[0];
        for line in &lines[1..] {
            assert!(
                line[..a_w].chars().all(|c| c == ' '),
                "column A must be blank on continuation lines"
            );
        }
    }

    /// A styled cell that overflows the column must keep its reset: the
    /// old char-based truncation cut the sequence in half, so the
    /// highlight bled past the row (and lost ~7 columns of text to the
    /// escape bytes being counted as visible).
    #[test]
    fn styled_cell_truncation_keeps_reset_and_full_budget() {
        let styled = "\x1b[1;35mhello world this is a long message\x1b[0m";
        let mut t = Table::new(&[""])
            .with_aligns(&[Align::Left])
            .with_min_widths(&[20])
            .with_priorities(&[3])
            .set_gap(1);
        t.add_row(&[styled.to_string()]);
        let out = t.render(20);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].trim_end().ends_with("\x1b[0m"),
            "truncated styled cell must be reset: {:?}",
            out[0]
        );
        // The escape bytes must not eat the text budget: 19 columns of
        // text + the ellipsis.
        assert_eq!(
            display_width(&out[0]),
            20,
            "styled cell must still fill its column: {:?}",
            out[0]
        );
        assert!(
            out[0].contains("hello world this "),
            "truncation must keep the full column budget: {:?}",
            out[0]
        );
    }

    /// Unstyled cells keep byte-identical truncation behaviour.
    #[test]
    fn unstyled_truncation_is_unchanged() {
        assert_eq!(keep_first("hello world", 5), "hello");
        assert_eq!(keep_last("hello world", 5), "world");
        // Escape sequences occupy no width.
        let styled = "\x1b[1;35mhello\x1b[0m";
        assert_eq!(display_width(&keep_first(styled, 5)), 5);
        assert_eq!(display_width(&keep_last(styled, 5)), 5);
    }

    /// Right-aligned styled cells truncate from the left without cutting
    /// an escape sequence.
    #[test]
    fn styled_cell_right_align_truncation() {
        let styled = "\x1b[1;35mhello world this is a long message\x1b[0m";
        let mut t = Table::new(&[""])
            .with_aligns(&[Align::Right])
            .with_min_widths(&[20])
            .with_priorities(&[3])
            .set_gap(1);
        t.add_row(&[styled.to_string()]);
        let out = t.render(20);
        assert_eq!(display_width(&out[0]), 20, "{:?}", out[0]);
        assert!(out[0].ends_with("age\x1b[0m"), "{:?}", out[0]);
    }

    /// A styled cell that has to be snippeted: the window is measured in
    /// visible columns (the SGR bytes must not eat the budget) and the
    /// escapes ride through the slice verbatim.
    #[test]
    fn styled_cell_snippet_is_escape_aware() {
        let styled = "\x1b[1;35mrefactor the tree indentation\x1b[0m and more text here";
        // Match "indentation": byte offset of 'i' inside the styled string.
        // Match indices are byte offsets into the cell.
        let byte_off = styled.find("indentation").expect("needle present");
        let out = extract_snippet(styled, &[byte_off], 24);
        assert!(
            out.contains("\x1b[1;35m"),
            "the leading escape must survive the slice: {out:?}"
        );
        assert!(
            display_width(&out) <= 24,
            "snippet must fit its window: {:?} ({})",
            out,
            display_width(&out)
        );
        // The match is present, though the highlight escapes split the
        // word, so compare against the unstyled form.
        let plain = strip_sgr(&out);
        assert!(plain.contains("indentation"), "match lost: {out:?}");
    }

    /// Drop SGR sequences — for asserting on the text of a styled cell.
    /// Fragments with no escape in them (the text before the first one)
    /// must be kept.
    fn strip_sgr(s: &str) -> String {
        s.split('\x1b')
            .map(|part| match part.split_once('m') {
                Some((_, rest)) => rest.to_string(),
                None => part.to_string(),
            })
            .collect()
    }
}
