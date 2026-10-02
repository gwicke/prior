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

/// Floor for the density gate's term length: a 1-char query highlights its one
/// matched char precisely (no scattering possible), but must not weaken a
/// multi-term query whose shortest term is a single char.
const MIN_TERM_CHARS: usize = 2;

/// `ceil(len * 3 / 5)`, integer-only.
fn required_hits(len: usize) -> usize {
    (len * 3).div_ceil(5)
}

/// Split a query into its independent terms. Terms are matched separately and
/// AND-ed, so their order in the query carries no meaning.
fn split_terms(query: &str) -> Vec<&str> {
    query.split_whitespace().collect()
}

/// `(min_term, required_hits)` for a single term: the density-gate window and
/// the minimum matches within it, scaled to that term's own length (floored at
/// [`MIN_TERM_CHARS`]) so a short term cannot weaken a long one.
fn term_gate(term: &str) -> (usize, usize) {
    let min_term = term.chars().count().max(MIN_TERM_CHARS);
    (min_term, required_hits(min_term))
}

/// Keep only matched positions that lie in a window of `min_term` consecutive
/// chars containing at least `required` matches. This is the density gate that
/// suppresses scattered single-char subsequence noise; a fully dense cell
/// (every char matched) always survives.
fn qualify_positions(n_chars: usize, matched: &[usize], min_term: usize, required: usize) -> Vec<usize> {
    if matched.is_empty() || n_chars == 0 {
        return Vec::new();
    }
    if matched.len() >= n_chars {
        return matched.to_vec();
    }
    let required = required.min(n_chars);
    // pre[k] = number of matched positions with char index < k
    let mut pre = vec![0usize; n_chars + 1];
    for &p in matched {
        pre[p + 1] = 1;
    }
    for k in 0..n_chars {
        pre[k + 1] += pre[k];
    }
    let mut kept = Vec::with_capacity(matched.len());
    for &p in matched {
        // Window start s ranges over [p - min_term + 1, p]; keep p if any such
        // window (clamped to the text) holds `required` matches.
        let lo = (p + 1).saturating_sub(min_term);
        for s in lo..=p {
            let end = (s + min_term).min(n_chars);
            if pre[end] - pre[s] >= required {
                kept.push(p);
                break;
            }
        }
    }
    kept
}

/// Byte-offset view of [`qualify_positions`]: qualify `raw` byte offsets within
/// `text` and return the surviving byte offsets (the downstream contract).
fn qualify_indices(text: &str, raw: &[usize], min_term: usize, required: usize) -> Vec<usize> {
    if raw.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut byte_to_char = vec![0usize; text.len() + 1];
    let mut char_to_byte = Vec::with_capacity(chars.len());
    let mut off = 0usize;
    for (i, c) in chars.iter().enumerate() {
        char_to_byte.push(off);
        byte_to_char[off] = i;
        off += c.len_utf8();
    }
    byte_to_char[text.len()] = chars.len();
    let mut pos: Vec<usize> = raw
        .iter()
        .filter(|&&b| b < byte_to_char.len())
        .map(|&b| byte_to_char[b])
        .collect();
    pos.sort_unstable();
    pos.dedup();
    qualify_positions(chars.len(), &pos, min_term, required)
        .into_iter()
        .map(|p| char_to_byte[p])
        .collect()
}

/// Rank `items` against `query` with the shared predictability rule,
/// returning both the ranked items AND match indices for specified fields.
///
/// The query is split on whitespace into **independent terms** that are
/// matched separately and AND-ed: every term must match for the item to
/// survive, and the score is the **sum** of per-term scores. Term order in
/// the query therefore carries no meaning — `"foo bar"` and `"bar foo"`
/// score identically (see [`split_terms`]).
///
/// 1. **Prefix matches first** (single-term queries only): `prefix_of(item)`
///    starts with that term (case-insensitive) — ordered shortest prefix
///    first, then lexicographic. `.pri` → `price`, `price_input`,
///    `price_output`; `.so` → `.sort:` ranks before `.reasoning:`.
/// 2. **Then fuzzy (subsequence) matches** on `text_of(item)` — ordered
///    by summed score (with a small positional bonus favouring earlier
///    input, so recency-sorted sources stay ranked), then shortest prefix,
///    then lexicographic.
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
    let terms = split_terms(query);
    let single_term = terms.len() == 1;
    let term_lower: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    let matcher = SkimMatcherV2::default();
    // (tier, prefix_len, prefix, score, input_idx, input_slot) — tier 0 =
    // prefix match (single-term only), tier 1 = fuzzy-only match. The input
    // slot (index into `items`) is a stable key: pointers would dangle once
    // the caller moves its item container.
    let mut classified: Vec<(u8, usize, String, i64, usize, usize)> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let prefix = prefix_of(item);
        let pl = prefix.to_lowercase();
        // Prefix tier only for a single term, where "id starts with the term"
        // is meaningful. For multi-term queries the id-prefix check against
        // the whole joined string would essentially never fire.
        if single_term && pl.starts_with(&term_lower[0]) {
            classified.push((0, pl.len(), pl, 0, idx, idx));
            continue;
        }
        // Fuzzy tier: every term must match (AND); score is the sum.
        let text = text_of(item);
        let mut total: i64 = 0;
        let mut all_match = true;
        for term in terms.iter() {
            match matcher.fuzzy_match(&text, term) {
                Some(s) => total += s,
                None => {
                    all_match = false;
                    break;
                }
            }
        }
        if all_match {
            // Add small positional bonus to score: earlier items get a slight
            // edge, so recency-sorted sources rank newer matches first. This
            // preserves original ordering for near-equal fuzzy scores.
            let score_with_bonus = total + (1000 - (idx as i64).min(1000));
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
    // Each term is matched and density-gated independently against its own
    // gate (see `term_gate`), then the surviving indices are unioned, so
    // scattered noise never reaches the display and a short term cannot
    // weaken a long one.
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
                let mut all_indices: Vec<usize> = Vec::new();
                for term in terms.iter() {
                    if let Some((_, raw)) = matcher.fuzzy(&field_text, term, true) {
                        let (min_term, required) = term_gate(term);
                        all_indices.extend(qualify_indices(&field_text, &raw, min_term, required));
                    }
                }
                if !all_indices.is_empty() {
                    all_indices.sort_unstable();
                    all_indices.dedup();
                    field_map.insert(name.clone(), all_indices);
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
    scored.sort_by_key(|b| std::cmp::Reverse(b.0));
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

    // --- multi-term: order-independent AND matching ---

    fn multi_items() -> Vec<String> {
        vec![
            "alpha beta".to_string(),
            "beta alpha".to_string(),
            "alpha only".to_string(),
            "gamma".to_string(),
        ]
    }

    #[test]
    fn multi_term_order_does_not_change_ranking() {
        // Sum-of-scores is commutative, so swapping terms must yield the
        // exact same ranking (same rows, same relative order).
        let items = multi_items();
        let ab = rc_rank(&items, "alpha beta");
        let ba = rc_rank(&items, "beta alpha");
        assert_eq!(ab, ba, "term order must not affect ranking");
        // Both rows containing both terms survive.
        assert!(ab.contains(&"alpha beta".to_string()));
        assert!(ab.contains(&"beta alpha".to_string()));
    }

    #[test]
    fn multi_term_and_semantics_drops_partial_matches() {
        let items = multi_items();
        let got = rc_rank(&items, "alpha beta");
        // "alpha only" is missing "beta" and "gamma" is missing both.
        assert!(!got.contains(&"alpha only".to_string()), "{got:?}");
        assert!(!got.contains(&"gamma".to_string()), "{got:?}");
    }

    #[test]
    fn multi_term_no_literal_space_required_between_terms() {
        // Before per-term matching, "alpha beta" was the literal pattern
        // `a,l,p,h,a,SPACE,b,e,t,a`; now terms match independently.
        let items = vec!["alpha_and_beta".to_string()];
        let got = rc_rank(&items, "alpha beta");
        assert_eq!(got.len(), 1, "terms must match independently: {got:?}");
    }

    #[test]
    fn multi_term_skips_prefix_tier() {
        // Prefix tier is single-term only; "abc def" must not be treated as
        // a prefix of the id. Both rows go to the fuzzy tier.
        let items = vec!["abc def".to_string(), "abc  xyz def".to_string()];
        let got = rc_rank(&items, "abc def");
        assert_eq!(got.len(), 2, "both match as terms: {got:?}");
    }

    // --- density gate ---

    #[test]
    fn gate_suppresses_scattered_single_chars() {
        // Truly scattered noise for term "abc" (min_term 3, required 2):
        // a@1, b@20, c@40 — no 3-char window holds 2 matches.
        let text: String = "a".to_string() + &"x".repeat(18) + "b" + &"x".repeat(19) + "c";
        let raw: Vec<usize> = vec![0, 19, 39];
        let (min_term, required) = term_gate("abc"); // (3, 2)
        let kept = qualify_indices(&text, &raw, min_term, required);
        assert!(kept.is_empty(), "sparse noise must be suppressed: {kept:?}");
    }

    #[test]
    fn gate_keeps_adjacent_match_for_short_term() {
        // Term "ab" (min_term 2, required 2): two *adjacent* hits meet the bar.
        let text = "abXc";
        let raw: Vec<usize> = vec![0, 1];
        let (min_term, required) = term_gate("ab"); // (2, 2)
        assert_eq!(qualify_indices(text, &raw, min_term, required), vec![0, 1]);
    }

    #[test]
    fn gate_keeps_dense_match() {
        // Dense match: "abc" contiguous within a 3-char window hits the bar.
        let text = "xxabcxx";
        let raw: Vec<usize> = [2usize, 3, 4].to_vec(); // a, b, c
        let (min_term, required) = term_gate("abc");
        let kept = qualify_indices(text, &raw, min_term, required);
        assert_eq!(kept, vec![2, 3, 4]);
    }

    #[test]
    fn gate_drops_single_matched_char() {
        // A lone matched char never meets a required>=2 bar → dropped.
        let text = "XaXbXc";
        let raw: Vec<usize> = vec![1];
        let (min_term, required) = term_gate("z"); // (2, 2)
        assert!(qualify_indices(text, &raw, min_term, required).is_empty());
    }

    #[test]
    fn gate_scales_with_term_length() {
        // Longer terms demand proportionally more matches in a wider window.
        // term "abcde" → min_term 5, required ceil(15/5)=3.
        let (min_term, required) = term_gate("abcde");
        assert_eq!((min_term, required), (5, 3));
        // term "ab" → min_term 2, required ceil(6/5)=2.
        let (min_term, required) = term_gate("ab");
        assert_eq!((min_term, required), (2, 2));
        // term "abcd" → min_term 4, required ceil(12/5)=3.
        let (min_term, required) = term_gate("abcd");
        assert_eq!((min_term, required), (4, 3));
    }

    #[test]
    fn gate_keeps_fully_dense_short_match() {
        // A perfect short match (all chars matched) always survives.
        let text = "abc";
        let raw: Vec<usize> = [0usize, 1, 2].to_vec();
        let (mt, req) = term_gate("abc");
        assert_eq!(qualify_indices(text, &raw, mt, req), vec![0, 1, 2]);
    }
}
