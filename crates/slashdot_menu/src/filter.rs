//! Field filtering & sorting (design §7.1).
//!
//! Generalizes prior's hand-written `ModelFilter`/`apply_modifier` into
//! one mechanism: every field of a dynamic row is addressable by a modifier
//! token named after the field.
//!
//! Token semantics:
//! - `.field:value` — exact match on a string field
//! - `.field:>v` / `<v` / `>=v` / `<=v` — numeric comparison
//! - `.sort:field` — direction defaults to the field's `default_dir`
//! - `.sort:field>` — descending (large first), `.sort:field<` — ascending
//!   (small first); direction is a **suffix**, so type-ahead can pick the
//!   field first, then append `>`/`<`.
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

/// Classify flat word ranges into view modifiers vs fuzzy terms
/// (arg_and_modifier_autocomplete.md): a dot word is a modifier iff its
/// key — up to the first `:` — is a valid field for this level
/// (`space.names(true)`, which always includes `id`/`description`, plus
/// the injected `sort` pseudo-field). Everything else — plain words,
/// `..`, path-ish `./x`, unknown keys like `.srot:price` — stays a
/// fuzzy term. For *valid* keys the produced tokens are byte-identical
/// to the old parse-time classification.
pub fn split_modifiers<'a>(
    words: &[std::ops::Range<usize>],
    suffix: &'a str,
    space: &FieldSpace,
) -> (Vec<ModifierToken>, Vec<&'a str>) {
    let names = space.names(true);
    let valid = |key: &str| key == "sort" || names.contains(&key);
    let mut mods = Vec::new();
    let mut terms = Vec::new();
    for r in words {
        let w = &suffix[r.clone()];
        // `.` alone and `..` fall out naturally: their "keys" (`""`,
        // `"."`) are never valid field names.
        let Some(body) = w.strip_prefix('.') else {
            terms.push(w);
            continue;
        };
        let (key, value) = match body.find(':') {
            Some(c) => (&body[..c], &body[c + 1..]),
            None => (body, ""),
        };
        if valid(key) {
            mods.push(ModifierToken {
                key: key.to_string(),
                value: value.to_string(),
            });
        } else {
            terms.push(w);
        }
    }
    (mods, terms)
}

/// Parse `.sort:field` (field's `default_dir`) / `.sort:field>` (descending)
/// / `.sort:field<` (ascending). Direction is a **suffix** (more ergonomic
/// with type-ahead — the user can type the field first then append `>`/`<`)
/// rather than a prefix. Returns `None` for unknown/unsortable fields.
pub fn parse_sort_spec(
    value: &str,
    space: &FieldSpace,
) -> Option<(String, crate::fields::SortDir)> {
    let (name, explicit) = match value.chars().next_back() {
        Some('<') => (&value[..value.len() - 1], Some(crate::fields::SortDir::Asc)),
        Some('>') => (
            &value[..value.len() - 1],
            Some(crate::fields::SortDir::Desc),
        ),
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
pub fn apply_query(rows: &[Row], q: &Query, _space: &FieldSpace) -> Vec<Row> {
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
        let q = compile_query(&toks(&[("sort", "price<")]), &space());
        assert_eq!(
            ids(&apply_query(&rows(), &q, &space())),
            vec!["m2", "m3", "m1", "m4"]
        );
    }

    #[test]
    fn sort_descending_with_explicit_direction() {
        let q = compile_query(&toks(&[("sort", "price>")]), &space());
        assert_eq!(
            ids(&apply_query(&rows(), &q, &space())),
            vec!["m1", "m3", "m2", "m4"]
        );
    }

    #[test]
    fn sort_direction_defaults_to_field_default() {
        // price declares Asc: `.sort:price` == `.sort:price<`
        let q = compile_query(&toks(&[("sort", "price")]), &space());
        assert_eq!(
            ids(&apply_query(&rows(), &q, &space())),
            vec!["m2", "m3", "m1", "m4"]
        );
        // coding declares Desc: `.sort:coding` == `.sort:coding>`
        let q2 = compile_query(&toks(&[("sort", "coding")]), &space());
        assert_eq!(
            ids(&apply_query(&rows(), &q2, &space())),
            vec!["m1", "m3", "m2", "m4"]
        );
    }

    #[test]
    fn later_sort_overrides_earlier() {
        let q = compile_query(&toks(&[("sort", "price>"), ("sort", "price<")]), &space());
        assert_eq!(
            ids(&apply_query(&rows(), &q, &space())),
            vec!["m2", "m3", "m1", "m4"]
        );
    }

    #[test]
    fn filter_before_sort() {
        let q = compile_query(&toks(&[("tier", "paid"), ("sort", "price<")]), &space());
        assert_eq!(ids(&apply_query(&rows(), &q, &space())), vec!["m3", "m1"]);
    }

    #[test]
    fn sort_missing_field_goes_last_both_directions() {
        for dir in ["price<", "price>"] {
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
        assert!(parse_sort_spec("uid<", &sp).is_none());
        assert!(parse_sort_spec("uid", &sp).is_none());
    }

    #[test]
    fn parse_sort_spec_forms() {
        // Suffix direction: `.sort:price<` asc, `.sort:price>` desc.
        assert_eq!(
            parse_sort_spec("price<", &space()),
            Some(("price".into(), SortDir::Asc))
        );
        assert_eq!(
            parse_sort_spec("price>", &space()),
            Some(("price".into(), SortDir::Desc))
        );
        assert_eq!(
            parse_sort_spec("price", &space()),
            Some(("price".into(), SortDir::Asc))
        );
        assert!(parse_sort_spec("", &space()).is_none());
        assert!(parse_sort_spec("<", &space()).is_none());
    }

    // --- split_modifiers (flat args → view classification) ---

    /// Word ranges for a single-space-separated test suffix.
    fn flat(suffix: &str) -> Vec<std::ops::Range<usize>> {
        let mut out = Vec::new();
        let mut start = None;
        for (i, b) in suffix.bytes().enumerate() {
            if b == b' ' {
                if let Some(s) = start {
                    out.push(s..i);
                    start = None;
                }
            } else if start.is_none() {
                start = Some(i);
            }
        }
        if let Some(s) = start {
            out.push(s..suffix.len());
        }
        out
    }

    #[test]
    fn split_modifiers_valid_keys_become_modifiers() {
        let s = ".sort:price .tier:free fuzzy";
        let (m, t) = split_modifiers(&flat(s), s, &space());
        let got: Vec<(&str, &str)> = m
            .iter()
            .map(|t| (t.key.as_str(), t.value.as_str()))
            .collect();
        assert_eq!(got, vec![("sort", "price"), ("tier", "free")]);
        assert_eq!(t, vec!["fuzzy"]);
    }

    #[test]
    fn split_modifiers_bare_keys_yield_empty_value() {
        let s = ".sort .tier";
        let (m, t) = split_modifiers(&flat(s), s, &space());
        let got: Vec<(&str, &str)> = m
            .iter()
            .map(|t| (t.key.as_str(), t.value.as_str()))
            .collect();
        assert_eq!(got, vec![("sort", ""), ("tier", "")]);
        assert!(t.is_empty());
    }

    #[test]
    fn split_modifiers_invalid_dot_words_stay_terms() {
        // The accepted behavior delta: invalid-key dot words are fuzzy
        // terms (before: silently-ignored modifiers).
        let s = ".srot:price .. ./src .gitignore .";
        let (m, t) = split_modifiers(&flat(s), s, &space());
        assert!(m.is_empty(), "no invalid dot word may become a modifier");
        assert_eq!(t, vec![".srot:price", "..", "./src", ".gitignore", "."]);
    }

    #[test]
    fn split_modifiers_implicit_id_and_description_are_valid() {
        let s = ".id:m1 .description:x .backend:y";
        let (m, t) = split_modifiers(&flat(s), s, &space());
        let got: Vec<&str> = m.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(got, vec!["id", "description"]);
        assert_eq!(t, vec![".backend:y"]);
    }

    #[test]
    fn split_modifiers_value_splits_on_first_colon_only() {
        let s = ".tier:a:b";
        let (m, _) = split_modifiers(&flat(s), s, &space());
        assert_eq!(m[0].key, "tier");
        assert_eq!(m[0].value, "a:b");
    }
}
