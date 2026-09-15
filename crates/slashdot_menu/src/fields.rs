//! Row model and field specs (design §8.1).
//!
//! A dynamic level's rows carry typed key-value fields; each field declares
//! small metadata (type, filterable/sortable opt-outs, default sort
//! direction) used by the filter/sort engine and completion.

use crate::value::Value;

/// A dynamic row: id + description + typed fields (design §8).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub id: String,
    pub description: Option<String>,
    /// Explicit fields (string or number values). Sorting/filtering reads
    /// these.
    pub fields: Vec<(String, Value)>,
    /// Optional per-field display-string overrides (design §8.1 "display
    /// string"): used verbatim for table display, but **ignored** in sorting
    /// and filtering (which read `fields`). Lets a provider refine display
    /// (e.g. an ISO date, a price glyph) without complicating the engine.
    /// Keys parallel `fields`; absent key ⇒ render the field value itself.
    pub display: Vec<(String, String)>,
}

impl Row {
    pub fn new(id: impl Into<String>) -> Self {
        Row {
            id: id.into(),
            ..Default::default()
        }
    }

    pub fn desc(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    pub fn field(mut self, k: impl Into<String>, v: Value) -> Self {
        self.fields.push((k.into(), v));
        self
    }

    /// Attach a display-string override for a field (design §8.1): rendered
    /// verbatim in tables, ignored by sort/filter. Produces an empty string
    /// (falls back to the value) if the override is empty/absent.
    pub fn display_field(mut self, k: impl Into<String>, d: impl Into<String>) -> Self {
        self.display.push((k.into(), d.into()));
        self
    }

    /// Explicit field value by name (does not consult implicit fields).
    pub fn field_value(&self, name: &str) -> Option<&Value> {
        self.fields
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    /// The display string for a field: the provider's `display` override if
    /// present (non-empty), else the rendered field value.
    pub fn field_display(&self, name: &str) -> Option<String> {
        self.display
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, d)| d.clone())
            .filter(|d| !d.is_empty())
    }
}

/// Field metadata (design §8.1).
#[derive(Debug, Clone, Default)]
pub struct FieldSpec {
    pub name: String,
    /// Field type: only `string` / `number` exist (design §7).
    pub is_number: bool,
    /// Participate in `.field:` filters (default on).
    pub filterable: bool,
    /// Participate in `.sort:` (default on; raw uids may opt out).
    pub sortable: bool,
    /// Sort direction when `.sort:field` has no explicit direction
    /// (design §7.1: indices `desc`, prices `asc`).
    pub default_dir: SortDir,
    /// Optional human description of the field, shown in the modifier
    /// auto-complete (and `.sort:`) Description column. When absent, the
    /// component falls back to the type ("string"/"number"); the field
    /// name itself is never repeated as a description.
    pub description: Option<String>,
}

impl Default for SortDir {
    fn default() -> Self {
        SortDir::Asc
    }
}

/// Sort direction. `>` = descending (large first), `<` = ascending
/// (design §7.1 as user-adjusted: `.sort:<field` asc, `.sort:>field` desc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl FieldSpec {
    /// An implicit spec (any typed value participates fully).
    pub fn new(name: impl Into<String>, is_number: bool) -> Self {
        FieldSpec {
            name: name.into(),
            is_number,
            filterable: true,
            sortable: true,
            default_dir: if is_number {
                SortDir::Desc
            } else {
                SortDir::Asc
            },
            description: None,
        }
    }

    pub fn opt_out_filter(mut self) -> Self {
        self.filterable = false;
        self
    }

    pub fn opt_out_sort(mut self) -> Self {
        self.sortable = false;
        self
    }

    pub fn dir(mut self, d: SortDir) -> Self {
        self.default_dir = d;
        self
    }

    /// Attach a human description for the field's modifier auto-complete
    /// and `.sort:` Description column.
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }
}

/// The field space of a dynamic level: implicit id/description fields plus
/// declared field specs. Field names resolve first to declared specs, then
/// to the implicit fields.
#[derive(Debug, Clone, Default)]
pub struct FieldSpace {
    pub id_field: FieldSpec,
    pub description_field: FieldSpec,
    pub fields: Vec<FieldSpec>,
}

impl FieldSpace {
    pub fn new(fields: Vec<FieldSpec>) -> Self {
        FieldSpace {
            id_field: FieldSpec::new("id", false),
            description_field: FieldSpec::new("description", false),
            fields,
        }
    }

    /// Resolve a field name to its spec.
    pub fn spec(&self, name: &str) -> Option<&FieldSpec> {
        if name == "id" {
            return Some(&self.id_field);
        }
        if name == "description" {
            return Some(&self.description_field);
        }
        self.fields.iter().find(|f| f.name == name)
    }

    /// Field names available for completion (per usage).
    pub fn names(&self, want_sortable: bool) -> Vec<&str> {
        let mut out = Vec::new();
        for f in [&self.id_field, &self.description_field]
            .into_iter()
            .chain(self.fields.iter())
        {
            if want_sortable && !f.sortable {
                continue;
            }
            out.push(f.name.as_str());
        }
        out
    }
}

/// Extract a row's value for a field name (implicit fields included),
/// cloning for the implicit string fields.
pub fn row_field_value(row: &Row, name: &str) -> Option<Value> {
    match name {
        "id" => Some(Value::Str(row.id.clone())),
        "description" => row.description.clone().map(Value::Str),
        _ => row.field_value(name).cloned(),
    }
}

/// Distinct string values of a field across rows (design D7 / §8.1
/// preprocessing pass: dataset-derived completion source).
pub fn distinct_values(rows: &[Row], name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        if let Some(v) = row_field_value(r, name) {
            if let Value::Str(s) = v {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_rows() -> Vec<Row> {
        vec![
            Row::new("a").desc("first").field("tier", Value::Str("free".into())),
            Row::new("b").desc("second").field("tier", Value::Str("paid".into())),
            Row::new("c").field("tier", Value::Str("free".into())),
        ]
    }

    #[test]
    fn implicit_fields_resolve() {
        let space = FieldSpace::new(vec![]);
        assert!(space.spec("id").is_some());
        assert!(space.spec("description").is_some());
        assert!(space.spec("nope").is_none());
    }

    #[test]
    fn row_field_value_reads_implicit_and_explicit() {
        let rows = sample_rows();
        assert_eq!(row_field_value(&rows[0], "id"), Some(Value::Str("a".into())));
        assert_eq!(
            row_field_value(&rows[0], "description"),
            Some(Value::Str("first".into()))
        );
        assert_eq!(
            row_field_value(&rows[0], "tier"),
            Some(Value::Str("free".into()))
        );
        assert_eq!(row_field_value(&rows[2], "description"), None);
    }

    #[test]
    fn distinct_values_dedups_and_skips_numbers() {
        assert_eq!(
            distinct_values(&sample_rows(), "tier"),
            vec!["free".to_string(), "paid".to_string()]
        );
        assert!(distinct_values(&sample_rows(), "price").is_empty());
    }

    #[test]
    fn sortable_opt_out() {
        let space = FieldSpace::new(vec![
            FieldSpec::new("uid", false).opt_out_sort(),
            FieldSpec::new("price", true),
        ]);
        assert_eq!(space.names(true), vec!["id", "description", "price"]);
        assert_eq!(space.names(false).len(), 4);
    }

    /// display string override (design §8.1): used verbatim for rendering
    /// but ignored by value access (`row_field_value` / `distinct_values`),
    /// so sorting/filtering/completion read the raw value.
    #[test]
    fn display_field_override_does_not_leak_into_value_access() {
        let r = Row::new("a")
            .field("created", crate::value::Value::Str("2024-01-01".into()))
            .display_field("created", "Jan 1 2024");
        // Display override present...
        assert_eq!(r.field_display("created").as_deref(), Some("Jan 1 2024"));
        // ...but value access (filter/sort/completion) reads the raw value.
        assert_eq!(
            row_field_value(&r, "created"),
            Some(crate::value::Value::Str("2024-01-01".into()))
        );
        // Absent override → display falls back to the value.
        let r2 = Row::new("b").field("created", crate::value::Value::Str("x".into()));
        assert_eq!(r2.field_display("created"), None);
    }
}
