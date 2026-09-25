//! Selection & cursor semantics (design §4).
//!
//! Ranking + pinning: fuzzy terms rank candidates; an exact trailing token
//! pins the selection (pinned id wins while it stays in the filtered view —
//! R8-5). Exact-token preselection and fuzzy-unique auto-jumps are one
//! generic rule.

use std::collections::HashMap;

use fuzzy_matcher::FuzzyMatcher;

use crate::fields::Row;

/// Rank rows against fuzzy terms and resolve the active index.
///
/// - No terms: rows in original order; active = pinned row's index if the
///   selection names one, else 0.
/// - With terms: rows ranked best-first (id + description searched). A
///   pinned selection still present in the filtered view keeps the cursor;
///   when the pin fell out of the view, it is replaced by the new top match
///   (R8-5) — `pinned_survived == false` tells the interpreter to rewrite
///   the prompt's id token.
///
/// Returns `(ranked_rows, active_index, pinned_survived)`.
pub fn rank_and_pin(rows: &[Row], terms: &str, selection: Option<&str>) -> RankResult {
    let ranked: Vec<Row> = if terms.trim().is_empty() {
        rows.to_vec()
    } else {
        let with_texts: Vec<(&Row, String)> = rows
            .iter()
            .map(|r| {
                let text = match &r.description {
                    Some(d) => format!("{} {}", r.id, d),
                    None => r.id.clone(),
                };
                (r, text)
            })
            .collect();
        let mut scored: Vec<(i64, &Row)> = Vec::new();
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
        for (r, text) in &with_texts {
            if let Some(score) = matcher.fuzzy_match(text, terms) {
                scored.push((score, r));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, r)| r.clone()).collect()
    };

    let pinned_idx = selection.and_then(|sel| ranked.iter().position(|r| r.id == sel));
    let (active, survived) = match pinned_idx {
        Some(i) => (i, true),
        None => (0, false),
    };
    RankResult {
        rows: ranked,
        active,
        pinned_survived: survived,
    }
}

/// Ranking outcome: display rows, active index, and whether a pinned
/// selection survived the filter (false ⇒ interpreter rewrites the id).
#[derive(Debug, Clone, PartialEq)]
pub struct RankResult {
    pub rows: Vec<Row>,
    pub active: usize,
    pub pinned_survived: bool,
}

impl RankResult {
    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.active)
    }
}

// --- RowRef-based ranking for v2.5 ---

use crate::node_entry::RowRef;

/// Ranking outcome for RowRef handles.
pub(crate) struct RankRefResult {
    pub(crate) rows: Vec<RowRef>,
    pub(crate) _active: usize,
    pub(crate) _pinned_survived: bool,
    /// Per-row match indices: RowRef → (field_name → match byte indices).
    /// Only populated for string columns that were requested.
    pub(crate) match_indices: HashMap<RowRef, HashMap<String, Vec<usize>>>,
}

/// Rank `candidates` (RowRef handles with pre-resolved Row references)
/// against fuzzy `terms`, with an optional pinned `selection`.
///
/// Filter: rows that don't match fuzzy terms are dropped.
/// Rank: via [`crate::fuzzy::rank_candidates`] — id-prefix matches first
/// (shortest id, then lexicographic) for predictable drill order, then
/// remaining fuzzy matches by score (§4: "exact token pre-selection").
/// Pin: if `selection` is present and still in the filtered view, it
/// becomes the active index; otherwise active defaults to 0.
///
/// `display_fields` specifies which row fields to compute match indices for.
/// These should be the fields that appear as string columns in the table.
pub(crate) fn rank_row_refs(
    candidates: &[(RowRef, &Row)],
    terms: &str,
    selection: Option<&str>,
    display_fields: &[String],
) -> RankRefResult {
    // Build field extractors for match index computation.
    let field_extractors: Vec<(String, Box<dyn for<'a> Fn(&'a (RowRef, &Row)) -> String>)> = display_fields
        .iter()
        .filter_map(|f| {
            match f.as_str() {
                "id" => Some(("id".to_string(), Box::new(|(_, row): &(RowRef, &Row)| row.id.clone()) as Box<dyn for<'a> Fn(&'a (RowRef, &Row)) -> String>)),
                "description" => Some(("description".to_string(), Box::new(|(_, row): &(RowRef, &Row)| row.description.clone().unwrap_or_default()) as Box<dyn for<'a> Fn(&'a (RowRef, &Row)) -> String>)),
                _ => {
                    // Custom field - extract from row.fields
                    let field_name = f.clone();
                    Some((f.clone(), Box::new(move |(_, row): &(RowRef, &Row)| {
                        row.fields.iter()
                            .find(|(k, _)| k == &field_name)
                            .map(|(_, v)| match v {
                                crate::value::Value::Str(s) => s.clone(),
                                crate::value::Value::Num(n) => crate::value::fmt_num(*n),
                            })
                            .unwrap_or_default()
                    }) as Box<dyn for<'a> Fn(&'a (RowRef, &Row)) -> String>))
                }
            }
        })
        .collect();

    let (ranked_items, match_indices_map) = crate::fuzzy::rank_candidates(
        candidates,
        terms,
        |(_, row)| row.id.clone(),
        |(_, row)| match &row.description {
            Some(d) => format!("{} {}", row.id, d),
            None => row.id.clone(),
        },
        &field_extractors,
    );

    let ranked: Vec<RowRef> = ranked_items.into_iter().map(|(rref, _)| *rref).collect();

    // Convert match_indices_map (keyed by *const T where T = &(RowRef, &Row))
    // to HashMap<RowRef, ...>. The key in match_indices_map is *const &(RowRef, &Row).
    // We need to get the pointer to the original slice element.
    let mut match_indices = HashMap::new();
    for item in candidates {
        let ptr = item as *const (RowRef, &Row);
        if let Some(indices) = match_indices_map.get(&ptr) {
            match_indices.insert(item.0, indices.clone());
        }
    }

    let pinned_idx = selection.and_then(|sel| {
        candidates
            .iter()
            .find(|(_, row)| row.id == sel)
            .and_then(|(target, _)| {
                ranked.iter().position(|r| {
                    r.child_idx == target.child_idx && r.row_offset == target.row_offset
                })
            })
    });
    let (active, survived) = match pinned_idx {
        Some(i) => (i, true),
        None => (0, false),
    };
    RankRefResult {
        rows: ranked,
        _active: active,
        _pinned_survived: survived,
        match_indices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Row> {
        vec![
            Row::new("alpha").desc("first option"),
            Row::new("beta").desc("second option"),
            Row::new("gamma").desc("third option"),
        ]
    }

    #[test]
    fn no_terms_keeps_order() {
        let r = rank_and_pin(&rows(), "", None);
        assert_eq!(
            r.rows.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
        assert_eq!(r.active, 0);
    }

    #[test]
    fn terms_rank_best_first() {
        let r = rank_and_pin(&rows(), "gam", None);
        assert_eq!(r.rows[0].id, "gamma");
        assert_eq!(r.active, 0);
    }

    #[test]
    fn pinned_selection_is_active_without_terms() {
        let r = rank_and_pin(&rows(), "", Some("beta"));
        assert_eq!(r.active, 1);
        assert!(r.pinned_survived);
    }

    #[test]
    fn pinned_survives_terms_when_still_in_view() {
        let r = rank_and_pin(&rows(), "opt", Some("beta"));
        // All three match "opt" (description text); beta stays visible.
        assert!(r.rows.iter().any(|x| x.id == "beta"));
        let beta_idx = r.rows.iter().position(|x| x.id == "beta").unwrap();
        assert_eq!(r.active, beta_idx);
        assert!(r.pinned_survived);
    }

    #[test]
    fn pin_out_of_view_is_replaced() {
        // "gam" filters beta out; the pinned beta no longer survives.
        let r = rank_and_pin(&rows(), "gam", Some("beta"));
        assert_eq!(r.rows[0].id, "gamma");
        assert_eq!(r.active, 0);
        assert!(!r.pinned_survived);
    }

    #[test]
    fn unknown_pin_defaults_to_top() {
        let r = rank_and_pin(&rows(), "", Some("nope"));
        assert_eq!(r.active, 0);
        assert!(!r.pinned_survived);
    }

    #[test]
    fn non_matching_terms_filter_all() {
        let r = rank_and_pin(&rows(), "zzz", None);
        assert!(r.rows.is_empty());
        assert_eq!(r.active, 0);
    }

    #[test]
    fn rank_row_refs_prefix_ids_shortest_first() {
        // Predictable drill order: id-prefix matches first, shortest id
        // first — regardless of input order or fuzzy score noise.
        let long = Row::new("model_xtra_long").desc("long model");
        let fuzzy_only = Row::new("alphabet").desc("modular parts");
        let short = Row::new("model").desc("short model");
        let mid = Row::new("models").desc("plural model");
        let mk = |off: u32| RowRef {
            child_idx: 0,
            row_offset: off,
        };
        let candidates = vec![
            (mk(0), &long),
            (mk(1), &fuzzy_only),
            (mk(2), &short),
            (mk(3), &mid),
        ];
        let ranked = rank_row_refs(&candidates, "mod", None, &["id".to_string(), "description".to_string()]).rows;
        let order: Vec<u32> = ranked.iter().map(|r| r.row_offset).collect();
        assert_eq!(
            order,
            vec![2, 3, 0, 1],
            "model, models, model_xtra_long, then the fuzzy-only match"
        );
    }
}
