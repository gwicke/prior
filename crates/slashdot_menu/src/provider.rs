//! Data providers (design §8): the two handler kinds for dynamic levels.

use crate::fields::Row;
use crate::grammar::ModifierToken;
use crate::value::Value;

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
