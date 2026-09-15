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
    /// Row field name to render. Empty/absent means the row `id`.
    pub field: String,
    /// Header label shown above the column.
    pub header: String,
    pub align: Align,
    /// Minimum visible width (table shrink floor).
    pub min_width: usize,
    /// Shrink priority (higher = kept wider under pressure).
    pub priority: usize,
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

/// Outcome of a live-filter fetch.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveResult {
    /// Fresh candidate set (rows replace the current view).
    Rows(Vec<Row>),
    /// Candidate set unchanged — fuzzy matching keeps narrowing in place.
    NotChanged,
}

/// The two data-fetch kinds (design §8, R8-6).
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
