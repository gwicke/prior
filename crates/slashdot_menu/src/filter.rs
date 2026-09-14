//! Field filtering & sorting (design §7.1).
//!
//! Generalizes picocode's hand-written `ModelFilter`/`apply_modifier` into
//! one mechanism: every field of a dynamic row is addressable by a modifier
//! token named after the field.
//!
//! Token semantics:
//! - `.field:value` — exact match on a string field
//! - `.field:>v` / `<v` / `>=v` / `<=v` — numeric comparison
//! - `.sort:<field` — ascending (small first), `.sort:>field` — descending
//! - `.sort:field` — direction defaults to the field's `default_dir`
//!
//! Composition: filters AND together, apply before sorting; fuzzy terms
//! narrow further on top (handled by the interpreter, not here). A later
//! `.sort:` overrides an earlier one. Rows missing a filtered field are
//! dropped; rows missing a sorted field sort last.

use crate::fields::{row_field_value, FieldSpace, Row};
use crate::grammar::ModifierToken;
use crate::value::{parse_filter_value, Cmp, FilterValue, Value};

/// One filter predicate extracted from a modifier token.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldFilter {
    /// `.field:value` — exact string match (or value equality).
    Exact(Value),
    /// `.field:>v` etc. — numeric comparison; drops rows missing the field.
    Compare(Cmp, f64),
}

impl FieldFilter {
    pub fn matches(&self, v: &Value) -> bool {
        match self {
            FieldFilter::Exact(want) => match (want, v) {
                (Value::Str(w), Value::Str(s)) => w == s,
                (Value::Num(w), Value::Num(n)) => w == n,
                _ => false,
            },
            FieldFilter::Compare(cmp, t) => match v {
                Value::Num(n) => cmp.matches(*n, *t),
                _ => false,
            },
        }
    }
}

/// Compiled query over a dynamic level: field filters + optional sort.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    /// `(field_name, predicate)` pairs, ANDed together.
    pub filters: Vec<(String, FieldFilter)>,
    /// `(field_name, direction)` — a single sort (later overrides earlier).
    pub sort: Option<(String, crate::fields::SortDir)>,
}

/// Compile modifier tokens into a [`Query`] against a [`FieldSpace`].
///
/// Unknown field names are ignored by the front-end engine (they are passed
/// through to live-filter back-ends by the interpreter). The `sort` token is
/// special-cased; every other token is a field filter.
pub fn compile_query(tokens: &[ModifierToken], space: &FieldSpace) -> Query {
    let mut q = Query::default();
    for t in tokens {
        if t.key == "sort" {
            if let Some((name, dir)) = parse_sort_spec(&t.value, space) {
                q.sort = Some((name, dir)); // later sort overrides earlier
            }
            continue;
        }
        let Some(spec) = space.spec(&t.key) else {
            continue; // not a front-end field; back-end passthrough
        };
        if !spec.filterable {
            continue;
        }
        if let Some(fv) = parse_filter_value(&t.value) {
            let pred = match fv {
                FilterValue::Exact(v) => FieldFilter::Exact(v),
                FilterValue::Compare(cmp, n) => FieldFilter::Compare(cmp, n),
            };
            q.filters.push((t.key.clone(), pred));
        }
    }
    q
}

/// Parse `.sort:<field` (asc) / `.sort:>field` (desc) / `.sort:field`
/// (field's `default_dir`). Returns `None` for unknown/unsortable fields.
pub fn parse_sort_spec(
    value: &str,
    space: &FieldSpace,
) -> Option<(String, crate::fields::SortDir)> {
    let (name, explicit) = match value.chars().next() {
        Some('<') => (&value[1..], Some(crate::fields::SortDir::Asc)),
        Some('>') => (&value[1..], Some(crate::fields::SortDir::Desc)),
        _ => (value, None),
    };
    if name.is_empty() {
        return None;
    }
    let spec = space.spec(name)?;
    if !spec.sortable {
        return None;
    }
    Some((name.to_string(), explicit.unwrap_or(spec.default_dir)))
}

/// Apply a compiled [`Query`] to rows: filter (AND), then sort.
///
/// Missing filtered fields → row dropped. Missing sorted field → sorted
/// last regardless of direction (stable among themselves).
pub fn apply_query(rows: &[Row], q: &Query, space: &FieldSpace) -> Vec<Row> {
    let mut out: Vec<Row> = rows
        .iter()
        .filter(|r| {
            q.filters.iter().all(|(name, pred)| {
                row_field_value(r, name)
                    .map(|v| pred.matches(&v))
                    .unwrap_or(false)
            })
        })
        .cloned()
        .collect();

    if let Some((name, dir)) = &q.sort {
        // Pre-partition into present/missing so missing rows sort last
        // uniformly (normalizes the old f64::MAX / 0.0 sentinels).
        let key_of = |r: &Row| row_field_value(r, name);
        let mut present: Vec<(Value, Row)> = Vec::new();
        let mut missing: Vec<Row> = Vec::new();
        for r in out {
            match key_of(&r) {
                Some(v) => present.push((v, r)),
                None => missing.push(r),
            }
        }
        present.sort_by(|(va, ra), (vb, rb)| {
            let ord = va.cmp_value(vb);
            if ord == std::cmp::Ordering::Equal {
                ra.id.cmp(&rb.id) // stable tiebreak by id
            } else {
                ord
            }
        });
        if *dir == crate::fields::SortDir::Desc {
            present.reverse();
        }
        out = present.into_iter().map(|(_, r)| r).collect();
        out.extend(missing);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::{FieldSpec, SortDir};

    fn rows() -> Vec<Row> {
        vec![
            Row::new("m1")
                .desc("big flagship")
                .field("tier", Value::Str("paid".into()))
                .field("price", Value::Num(3.0))
                .field("coding", Value::Num(90.0)),
            Row::new("m2")
                .desc("small free model")
                .field("tier", Value::Str("free".into()))
                .field("price", Value::Num(0.0))
                .field("coding", Value::Num(60.0)),
            Row::new("m3")
                .desc("mid paid model")
                .field("tier", Value::Str("paid".into()))
                .field("price", Value::Num(1.0))
                .field("coding", Value::Num(75.0)),
            // missing price + tier: dropped by filters, sorted last otherwise
            Row::new("m4").desc("unknown"),
        ]
    }

    fn space() -> FieldSpace {
        FieldSpace::new(vec![
            FieldSpec::new("tier", false),
            FieldSpec::new("price", true).dir(SortDir::Asc),
            FieldSpec::new("coding", true).dir(SortDir::Desc),
        ])
    }

    fn toks(specs: &[(&str, &str)]) -> Vec<ModifierToken> {
        specs
            .iter()
            .map(|(k, v)| ModifierToken {
                key: k.to_string(),
                value: v.to_string(),
            })
            .collect()
    }

    fn ids(v: &[Row]) -> Vec<&str> {
        v.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn exact_string_filter() {
        let q = compile_query(&toks(&[("tier", "free")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m2"]);
    }

    #[test]
    fn comparison_filter_drops_missing() {
        // price < 1.5: m2 (0.0) and m3 (1.0) qualify; m4 (missing) is dropped.
        // Order follows the input order (no sort specified).
        let q = compile_query(&toks(&[("price", "<1.5")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m2", "m3"]);
    }

    #[test]
    fn filters_and_compose() {
        let q = compile_query(&toks(&[("tier", "paid"), ("price", "<2")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m3"]);
    }

    #[test]
    fn unknown_field_is_ignored_by_frontend() {
        let q = compile_query(&toks(&[("backendonly", "x")]), &space());
        // m4 lacks tier but the filter was dropped → all rows remain.
        assert_eq!(apply_query(&rows(), &q, &space()).len(), 4);
    }

    #[test]
    fn sort_ascending_with_explicit_direction() {
        let q = compile_query(&toks(&[("sort", "<price")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m2", "m3", "m1", "m4"]);
    }

    #[test]
    fn sort_descending_with_explicit_direction() {
        let q = compile_query(&toks(&[("sort", ">price")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m1", "m3", "m2", "m4"]);
    }

    #[test]
    fn sort_direction_defaults_to_field_default() {
        // price declares Asc: `.sort:price` == `.sort:<price`
        let q = compile_query(&toks(&[("sort", "price")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m2", "m3", "m1", "m4"]);
        // coding declares Desc: `.sort:coding` == `.sort:>coding`
        let q2 = compile_query(&toks(&[("sort", "coding")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q2, &space())), vec!["m1", "m3", "m2", "m4"]);
    }

    #[test]
    fn later_sort_overrides_earlier() {
        let q = compile_query(&toks(&[("sort", ">price"), ("sort", "<price")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m2", "m3", "m1", "m4"]);
    }

    #[test]
    fn filter_before_sort() {
        let q = compile_query(
            &toks(&[("tier", "paid"), ("sort", "<price")]),
            &space(),
        );
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m3", "m1"]);
    }

    #[test]
    fn sort_missing_field_goes_last_both_directions() {
        for dir in ["<price", ">price"] {
            let q = compile_query(&toks(&[("sort", dir)]), &space());
            let out = apply_query(&rows(), &q, &space());
            assert_eq!(out.last().unwrap().id, "m4", "missing must be last ({dir})");
        }
    }

    #[test]
    fn id_and_description_are_filterable_fields() {
        let q = compile_query(&toks(&[("id", "m2")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m2"]);
        let q2 = compile_query(&toks(&[("description", "big flagship")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q2, &space())), vec!["m1"]);
    }

    #[test]
    fn unsortable_field_rejects_sort() {
        let sp = FieldSpace::new(vec![FieldSpec::new("uid", false).opt_out_sort()]);
        assert!(parse_sort_spec("<uid", &sp).is_none());
        assert!(parse_sort_spec("uid", &sp).is_none());
    }

    #[test]
    fn parse_sort_spec_forms() {
        assert_eq!(
            parse_sort_spec("<price", &space()),
            Some(("price".into(), SortDir::Asc))
        );
        assert_eq!(
            parse_sort_spec(">price", &space()),
            Some(("price".into(), SortDir::Desc))
        );
        assert_eq!(
            parse_sort_spec("price", &space()),
            Some(("price".into(), SortDir::Asc))
        );
        assert!(parse_sort_spec("", &space()).is_none());
        assert!(parse_sort_spec("<", &space()).is_none());
    }
}
