//! Data providers (design §8): the two handler kinds for dynamic levels.

use crate::fields::Row;
use crate::grammar::ModifierToken;
use crate::table::Align;
use crate::value::Value;

/// Presentation metadata for one displayed column of a dynamic level
/// (design §8.1 / §6): which row field each column shows, its header label,
/// alignment, and table-builder sizing hints. Declared by the provider so
/// the generic renderer needs no per-command column layout in host code.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    /// Row field name to render (e.g. "id", "description", or any custom
    /// field). The implicit fields "id" and "description" are always
    /// available on every row.
    pub field: String,
    /// Header label shown above the column.
    pub header: String,
    pub align: Align,
    /// Minimum visible width (table shrink floor).
    pub min_width: usize,
    /// Shrink priority (higher = kept wider under pressure).
    pub priority: usize,
    /// When `true` (default), long cell content wraps across multiple
    /// visual lines. When `false`, the table uses single-line rows with
    /// ellipsis truncation instead.
    pub wrap: bool,
}

impl ColumnDef {
    /// A default left-aligned column over a row field.
    pub fn new(field: impl Into<String>, header: impl Into<String>) -> Self {
        ColumnDef {
            field: field.into(),
            header: header.into(),
            align: Align::Left,
            min_width: 3,
            priority: 1,
            wrap: true,
        }
    }

    pub fn align(mut self, a: Align) -> Self {
        self.align = a;
        self
    }

    pub fn min(mut self, w: usize) -> Self {
        self.min_width = w;
        self
    }

    pub fn prio(mut self, p: usize) -> Self {
        self.priority = p;
        self
    }

    /// Set whether this column's content wraps (default: true).
    pub fn wrap(mut self, w: bool) -> Self {
        self.wrap = w;
        self
    }
}

/// Field metadata for a dynamic level (design §8.1).
#[derive(Debug, Clone)]
pub struct FieldMeta {
    pub spec: crate::fields::FieldSpec,
    /// Completion values: pre-defined enum, or dataset-derived (None ⇒
    /// extracted from cached rows in the preprocessing pass).
    pub enum_values: Option<Vec<String>>,
}

/// Provider output for one-shot fetches.
#[derive(Debug, Clone, Default)]
pub struct DataResult {
    pub rows: Vec<Row>,
    /// Field metadata (specs + optional pre-defined enums).
    pub fields: Vec<FieldMeta>,
    /// Display columns (presentation hints for the generic renderer).
    /// Empty ⇒ the renderer falls back to id + description.
    pub columns: Vec<ColumnDef>,
    /// Default sort direction applied when the prompt has no `.sort:`.
    pub default_sort: Option<(String, crate::fields::SortDir)>,
    /// Field names not filterable front-end (back-end passthrough).
    pub backend_fields: Vec<String>,
    /// The id of the row that is *currently active* (design §5.1): the
    /// real-world current state the menu reflects before any change — the
    /// active model, the current session, the current tree leaf. The
    /// provider decides this from its own data source at `fetch` time;
    /// the engine resolves it to the `*`-marked row per frame and seeds a
    /// fresh cursor on it. `None` when no row is "currently in effect"
    /// (e.g. the root command list has none).
    pub active_row: Option<String>,
}

/// Detail/banner provider (design §5.1): supplies non-selectable
/// header lines rendered between the panel title and the candidate
/// rows — content that doesn't fit the row schema but should stay
/// in view (e.g. `/replay` command help, `/stat` session summary).
///
/// `fetch` is called per frame (the content is dynamic — a
/// `/stat`-like provider re-reads live counters; a `/replay`
/// provider reflects current limits). The returned strings are
/// painted verbatim as non-selectable header rows.
pub trait DetailProvider {
    /// Detail lines to render as non-selectable header rows between
    /// the panel title and the candidate rows. Empty ⇒ no detail
    /// section.
    fn fetch(&mut self) -> Vec<String>;
}

/// Outcome of a live-filter fetch.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveResult {
    /// Fresh candidate set (rows replace the current view).
    Rows(Vec<Row>),
    /// Candidate set unchanged — fuzzy matching keeps narrowing in place.
    NotChanged,
}

/// Active-state sourcing (design §5.1): the *active* row of a candidate
/// list comes from a single, uniform engine mechanism — the host never
/// computes or injects it per overlay.
///
/// Data providers carry both the candidate rows and the active state for a
/// level. For *static* levels (rows come from the descriptor's children /
/// enum value set — no data provider), only the **active** state varies at
/// runtime (e.g. `/persona`'s current persona, `/thinking`'s current level).
/// Those levels register an [`ActiveProvider`] instead: it supplies just the
/// active id, and the engine resolves it against the descriptor-derived rows
/// (`child_rows`) exactly as it resolves `DataResult::active_row` for dynamic
/// levels. (`DataResult::active_row` is the dynamic-level analogue; a level
/// has either a DataProvider or an ActiveProvider, never both.)
pub trait ActiveProvider {
    /// The id of the row that is currently in effect at this level (the
    /// `*` marker, design §5.1), or `None` when nothing is in effect.
    fn active(&mut self) -> Option<String>;
}

/// The two row-fetch kinds for dynamic levels (design §8, R8-6).
pub trait DataProvider {
    /// One-shot fetch: called once per menu open. Rows are cached; the
    /// interpreter applies front-end filters/sorting. The returned
    /// [`DataResult::active_row`] names the currently-active row.
    fn fetch(&mut self, modifiers: &[ModifierToken]) -> DataResult;

    /// Live-filter fetch: called per level change. `Ok(LiveResult::NotChanged)`
    /// keeps the current view narrowing in place.
    fn fetch_live(&mut self, query: &str, modifiers: &[ModifierToken]) -> LiveResult {
        let _ = (query, modifiers);
        LiveResult::NotChanged
    }
}

/// Convenience: build a one-shot provider from a static row set (tests,
/// settings-derived candidates, cached snapshots).
pub struct StaticProvider {
    pub result: DataResult,
}

impl DataProvider for StaticProvider {
    fn fetch(&mut self, _modifiers: &[ModifierToken]) -> DataResult {
        self.result.clone()
    }
}

/// Value-typed field helper.
pub fn str_field(k: &str, v: &str) -> (String, Value) {
    (k.to_string(), Value::Str(v.to_string()))
}

pub fn num_field(k: &str, v: f64) -> (String, Value) {
    (k.to_string(), Value::Num(v))
}
