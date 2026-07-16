//! Fuzzy substring matching for slash-command and model-list filtering.
//!
//! Uses `fuzzy-matcher`'s Skim matcher (subsequence + score).

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

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
pub fn fuzzy_filter<'a, T>(
    items: &'a [T],
    query: &str,
    to_text: impl Fn(&T) -> String,
) -> Vec<Ranked<'a, T>> {
    if query.trim().is_empty() {
        return items
            .iter()
            .map(|item| Ranked { item, score: 0 })
            .collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, &'a T)> = items
        .iter()
        .filter_map(|item| {
            matcher
                .fuzzy_match(&to_text(item), query)
                .map(|score| (score, item))
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
}
