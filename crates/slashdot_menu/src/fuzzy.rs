//! Fuzzy substring matching for slash-command and model-list filtering.
//!
//! Uses `fuzzy-matcher`'s Skim matcher (subsequence + score).

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use std::collections::HashMap;

/// A reference to an item plus its fuzzy match score (higher = better).
pub struct Ranked<'a, T> {
    pub item: &'a T,
    pub score: i64,
}

/// Rank `items` by fuzzy subsequence match against `query`.
///
/// `to_text` extracts the searchable text for each item. An empty/whitespace
/// query returns all items in their original order (score 0). Results are sorted
/// best-first (descending score).
///
/// When `bonus_prefix` is true, items whose search text starts with the query
/// receive a score bonus large enough to always rank them above subsequence-only
/// matches.
pub fn fuzzy_filter<'a, T>(
    items: &'a [T],
    query: &str,
    to_text: impl Fn(&T) -> String,
) -> Vec<Ranked<'a, T>> {
    fuzzy_filter_impl(items, query, to_text, false)
}

/// Like [`fuzzy_filter`] but adds a large bonus to items whose text starts
/// with the query, ensuring strict prefix matches always rank first.
pub fn fuzzy_filter_bonus<'a, T>(
    items: &'a [T],
    query: &str,
    to_text: impl Fn(&T) -> String,
) -> Vec<Ranked<'a, T>> {
    fuzzy_filter_impl(items, query, to_text, true)
}

/// Match indices for a single item across multiple fields.
/// Maps field name → list of byte indices in that field's string where the query matched.
pub type MatchIndices = HashMap<String, Vec<usize>>;

/// Rank `items` against `query` with the shared predictability rule,
/// returning both the ranked items AND match indices for specified fields.
///
/// 1. **Prefix matches first**: `prefix_of(item)` starts with `query`
///    (case-insensitive) — ordered shortest prefix first, then
///    lexicographic. `.pri` → `price`, `price_input`, `price_output`;
///    `.so` → `.sort:` ranks before `.reasoning:`.
/// 2. **Then fuzzy (subsequence) matches** on `text_of(item)` — ordered
///    by fuzzy score, then shortest prefix, then lexicographic.
/// 3. Non-matches are dropped. An empty/whitespace query returns every
///    item in input order (addChild order for listings, declaration
///    order for field completions) — nothing to rank against.
///
/// Input order breaks any remaining tie, so the output is fully
/// deterministic.
///
/// `field_extractors` specifies which fields to compute match indices for.
/// Each entry is `(field_name, extractor_fn)`. Match indices are byte
/// offsets into the extracted field string.
pub fn rank_candidates<'a, T>(
    items: &'a [T],
    query: &str,
    prefix_of: impl Fn(&T) -> String,
    text_of: impl Fn(&T) -> String,
    field_extractors: &[(String, impl Fn(&T) -> String)],
) -> (Vec<&'a T>, Vec<MatchIndices>) {
    if query.trim().is_empty() {
        // Parallel vector: one (empty) index map per item.
        return (
            items.iter().collect(),
            (0..items.len()).map(|_| HashMap::new()).collect(),
        );
    }
    let ql = query.to_lowercase();
    let matcher = SkimMatcherV2::default();
    // (tier, prefix_len, prefix, score, input_idx, input_slot) — tier 0 =
    // prefix match, tier 1 = fuzzy-only match. The input slot (index into
    // `items`) is a stable key: pointers would dangle once the caller moves
    // its item container.
    let mut classified: Vec<(u8, usize, String, i64, usize, usize)> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let prefix = prefix_of(item);
        let pl = prefix.to_lowercase();
        if pl.starts_with(&ql) {
            classified.push((0, pl.len(), pl, 0, idx, idx));
        } else if let Some(score) = matcher.fuzzy_match(&text_of(item), query) {
            // Add small positional bonus to score: earlier items get a slight edge.
            // This preserves original ordering for near-equal fuzzy scores.
            let score_with_bonus = score + (1000 - (idx as i64).min(1000));
            classified.push((1, pl.len(), pl, score_with_bonus, idx, idx));
        }
    }
    classified.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| match a.0 {
            // Prefix tier: shortest first, then lexicographic.
            0 => a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)),
            // Fuzzy tier: best score first, then ORIGINAL ORDER (input_idx),
            // then shortest prefix, then lexicographic. This gives more
            // weight to the original candidate ordering (e.g., newest-first
            // for session listings) when scores are equal or close.
            _ => {
                b.3.cmp(&a.3)
                    .then_with(|| a.4.cmp(&b.4))
                    .then_with(|| a.1.cmp(&b.1))
                    .then_with(|| a.2.cmp(&b.2))
            }
        })
    });
    let ranked: Vec<&'a T> = classified
        .iter()
        .map(|(_, _, _, _, slot, _)| &items[*slot])
        .collect();

    // Match indices for the requested fields, parallel to `ranked` (so the
    // caller never has to key by pointer). Only filtered items are computed.
    let mut match_indices: Vec<MatchIndices> = Vec::with_capacity(ranked.len());
    if field_extractors.is_empty() {
        for _ in &ranked {
            match_indices.push(HashMap::new());
        }
    } else {
        for (_, _, _, _, slot, _) in &classified {
            let item = &items[*slot];
            let mut field_map = HashMap::new();
            for (name, extractor) in field_extractors {
                let field_text = extractor(item);
                if let Some((_, indices)) = matcher.fuzzy(&field_text, query, true) {
                    if !indices.is_empty() {
                        field_map.insert(name.clone(), indices);
                    }
                }
            }
            match_indices.push(field_map);
        }
    }

    (ranked, match_indices)
}

fn fuzzy_filter_impl<'a, T>(
    items: &'a [T],
    query: &str,
    to_text: impl Fn(&T) -> String,
    bonus_prefix: bool,
) -> Vec<Ranked<'a, T>> {
    if query.trim().is_empty() {
        return items.iter().map(|item| Ranked { item, score: 0 }).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, &'a T)> = items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            let text = to_text(item);
            let base_score = matcher.fuzzy_match(&text, query)?;
            let ql = query.to_lowercase();
            let mut score = if bonus_prefix
                && text
                    .split_whitespace()
                    .next()
                    .map(|first| {
                        first
                            .trim_start_matches('/')
                            .to_lowercase()
                            .starts_with(&ql)
                    })
                    .unwrap_or(false)
            {
                // Large enough that any prefix match beats any subsequence-only match
                base_score + 10_000
            } else {
                base_score
            };
            // Small positional bonus: earlier items get a slight edge.
            // This preserves original ordering for near-equal fuzzy scores.
            // Max bonus is small (1000) so it never overrides genuine score differences.
            score += 1000 - (idx as i64).min(1000);
            Some((score, item))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
        .into_iter()
        .map(|(score, item)| Ranked { item, score })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_all_in_order() {
        let items = ["/model", "/thinking", "/help"];
        let r = fuzzy_filter(&items, "", |s| s.to_string());
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].item, &"/model");
        assert_eq!(r[1].item, &"/thinking");
        assert_eq!(r[2].item, &"/help");
    }

    #[test]
    fn subsequence_matches_and_ranks() {
        let items = ["/model", "/help", "/thinking"];
        let r = fuzzy_filter(&items, "mod", |s| s.to_string());
        assert!(!r.is_empty());
        assert_eq!(r[0].item, &"/model");
    }

    #[test]
    fn best_match_first() {
        let items = ["/help", "/model"];
        // 'm' matches /model; /help has no 'm'.
        let r = fuzzy_filter(&items, "m", |s| s.to_string());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].item, &"/model");
    }

    #[test]
    fn disjoint_query_no_matches() {
        let items = ["/model"];
        let r = fuzzy_filter(&items, "zzzq", |s| s.to_string());
        assert!(r.is_empty());
    }

    // --- rank_candidates (shared completion/drill predictability) ---

    fn rc_items() -> Vec<String> {
        vec![
            "reasoning".to_string(),
            "price_output".to_string(),
            "show_reasoning".to_string(),
            "price".to_string(),
            "price_input".to_string(),
            "sort".to_string(),
        ]
    }

    fn rc_rank(items: &[String], query: &str) -> Vec<String> {
        let field_extractors: Vec<(String, Box<dyn Fn(&String) -> String>)> = items
            .iter()
            .map(|s| {
                (
                    s.clone(),
                    Box::new(|x: &String| x.clone()) as Box<dyn Fn(&String) -> String>,
                )
            })
            .collect();
        rank_candidates(
            items,
            query,
            |s| s.clone(),
            |s| s.clone(),
            &field_extractors,
        )
        .0
        .into_iter()
        .cloned()
        .collect()
    }

    #[test]
    fn rank_candidates_prefix_tier_beats_fuzzy_tier() {
        // `.so` → `.sort:` (prefix) before fuzzy matches like `reasoning`.
        let items = rc_items();
        let got = rc_rank(&items, "so");
        assert_eq!(got[0], "sort", "{got:?}");
        assert!(got.contains(&"reasoning".to_string()), "{got:?}");
        assert!(got.contains(&"show_reasoning".to_string()), "{got:?}");
    }

    #[test]
    fn rank_candidates_shorter_prefix_first() {
        // `.pri` → price, price_input, price_output — length order,
        // regardless of input order.
        let items = rc_items();
        assert_eq!(
            rc_rank(&items, "pri"),
            vec!["price", "price_input", "price_output"]
        );
    }

    #[test]
    fn rank_candidates_empty_query_keeps_input_order() {
        let items = rc_items();
        assert_eq!(rc_rank(&items, ""), items);
        assert_eq!(rc_rank(&items, "  "), items);
    }

    #[test]
    fn rank_candidates_drops_non_matches_case_insensitively() {
        let items = rc_items();
        assert!(rc_rank(&items, "zzz").is_empty());
        // Case-insensitive prefix.
        assert_eq!(rc_rank(&items, "SO")[0], "sort");
        // No tier-2 match for "pri" outside the price family (e.g. no
        // subsequence hits in `reasoning`/`sort`) — output is exactly
        // the three prefix matches.
        assert_eq!(rc_rank(&items, "pri").len(), 3);
    }
}
