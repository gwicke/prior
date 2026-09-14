//! Path grammar (design §3): per-level tokenization of the prompt.
//!
//! At every level the buffer tail has the shape:
//!
//! ```text
//! [fuzzy search terms] [dot modifiers] [selection token][trailing space?]
//! ```
//!
//! - **Fuzzy search terms**: free words, may contain spaces. Filter candidates.
//! - **Dot modifiers**: `.key:value` tokens; a value consumes following words
//!   until the next dot-modifier, a valid selection token, or end of prompt
//!   (R8-7). Multi-word values are legal; selection tokens are space-free.
//! - **Selection token**: space-free, last in the level. For static levels it
//!   must exactly match a child key; for dynamic levels a row id; for
//!   value-typed children any token is *permissively* valid (§2).
//!
//! Parsing is structural, not a whitespace peek: the terminator of a modifier
//! value or a term run is the next dot-token, an exact selection match, or
//! end-of-prompt. The trailing space is the *drill-in* signal only.

/// A parsed dot-modifier token: `.key:value`. Multi-word values are joined
/// with spaces.
#[derive(Debug, Clone, PartialEq)]
pub struct ModifierToken {
    pub key: String,
    pub value: String,
}

/// A dot-token seen in the raw prompt: `sort` + `coding` from `.sort:coding`.
///
/// Also covers the bare `.foo` (no colon) and `.foo:` (empty value) forms so
/// the completion UI can complete the key or the value respectively.
#[derive(Debug, Clone, PartialEq)]
pub enum DotToken {
    /// `.foo` — modifier name being typed.
    Key(String),
    /// `.foo:` or `.foo:bar` — name (settled) with an optional value prefix.
    NameValue { key: String, value: String },
}

/// Split a modifier-ish word into a [`DotToken`], or `None` if not dotted.
pub fn parse_dot_token(word: &str) -> Option<DotToken> {
    let rest = word.strip_prefix('.')?;
    match rest.split_once(':') {
        Some((key, value)) => Some(DotToken::NameValue {
            key: key.to_string(),
            value: value.to_string(),
        }),
        None => Some(DotToken::Key(rest.to_string())),
    }
}

/// Parse one level of the prompt (everything after the parent tokens).
///
/// `selection_candidates` decides which trailing word is the selection: the
/// *last* whitespace-separated word is the selection candidate if it is
/// space-free and (for static levels) exactly matches a candidate; for
/// dynamic/value-typed levels the candidate check is done by the caller
/// against the row set (cheap lookup, D2), so `exact_selection` is passed in
/// as a closure over "does this word name a selection here?".
///
/// Returns the parsed [`LevelTokens`]: terms, modifiers, selection, and
/// whether the prompt ends with a trailing space after the selection
/// (the drill-in signal).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LevelTokens {
    /// Fuzzy search terms (original text, may contain spaces).
    pub terms: String,
    /// Modifiers in order, with values (multi-word values joined with spaces).
    pub modifiers: Vec<ModifierToken>,
    /// The selection token (child key / row id / value-typed value), if any.
    pub selection: Option<String>,
    /// Trailing space directly after the selection → drill-in.
    pub trailing_space: bool,
}

/// Selection predicate: owns its closure so it can be built freely and
/// passed to `parse_level` by reference.
#[derive(Clone, Default)]
pub struct SelectionRule {
    /// Is `word` a valid selection at this level? (exact key/id match, or
    /// permissive for value-typed children).
    pub is_selection: Option<std::sync::Arc<dyn Fn(&str) -> bool>>,
}

impl std::fmt::Debug for SelectionRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SelectionRule { .. }")
    }
}

/// Tokenize the prompt tail `s` into terms / modifiers / selection.
///
/// `is_selection` decides whether a trailing word pins as the selection.
/// The split is: walk words right-to-left; the last word is the selection
/// candidate when `is_selection` accepts it **and** the prompt either ends
/// there or continues after a space (drill). Everything before it is
/// re-parsed left-to-right: dotted words start/continue modifiers, plain
/// words are either fuzzy terms or modifier-value continuations.
pub fn parse_level(s: &str, rule: &SelectionRule) -> LevelTokens {
    let trailing_space = s.ends_with(' ');
    let trimmed = s.trim_end();
    let words: Vec<&str> = if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed.split(' ').collect()
    };

    // Candidate selection: last word, if space-free and accepted. The word
    // must not be a dot-modifier and the selection must not be followed by
    // more words on the same level (it is last-in-level by construction).
    let mut selection: Option<String> = None;
    let mut body_end = words.len();
    if let Some(last) = words.last() {
        let sel_ok = !last.starts_with('.')
            && rule
                .is_selection
                .as_ref()
                .map(|f| f(last))
                .unwrap_or(false);
        if sel_ok {
            // Only treat it as a selection if the words before it do not
            // *require* it as a modifier value (i.e. a pending multi-word
            // value after the last dot-token). Structural rule (R8-7): a
            // modifier value consumes following words until the next
            // dot-token / valid id / end. A valid id *after* a value starts
            // a new position, so selection wins.
            selection = Some((*last).to_string());
            body_end = words.len() - 1;
        }
    }

    // Walk the remaining words left-to-right: dotted words open modifiers,
    // plain words before any modifier are terms, plain words after are
    // modifier-value continuations.
    let mut terms: Vec<&str> = Vec::new();
    let mut modifiers: Vec<ModifierToken> = Vec::new();
    let mut in_value: Option<usize> = None; // index into modifiers collecting value words
    for word in &words[..body_end] {
        if let Some(tok) = parse_dot_token(word) {
            in_value = None;
            match tok {
                DotToken::Key(key) => {
                    // `.foo` with no colon: key settled, value still empty.
                    modifiers.push(ModifierToken {
                        key,
                        value: String::new(),
                    });
                    in_value = Some(modifiers.len() - 1);
                }
                DotToken::NameValue { key, value } => {
                    // Value consumes following words until the next
                    // dot-token / valid selection / end (R8-7) — even when
                    // the token itself already carries a value prefix.
                    modifiers.push(ModifierToken { key, value });
                    in_value = Some(modifiers.len() - 1);
                }
            }
        } else if let Some(idx) = in_value {
            let m = &mut modifiers[idx];
            if m.value.is_empty() {
                m.value = (*word).to_string();
            } else {
                m.value.push(' ');
                m.value.push_str(word);
            }
        } else {
            terms.push(word);
        }
    }

    LevelTokens {
        terms: terms.join(" "),
        modifiers,
        selection,
        trailing_space,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_sel_static(word: &str) -> bool {
        // Stand-in static children for tests.
        matches!(word, "switch" | "compact" | "resume" | "delete")
    }

    fn rule_static() -> SelectionRule {
        SelectionRule {
            is_selection: Some(std::sync::Arc::new(is_sel_static)),
        }
    }

    fn always_sel(word: &str) -> bool {
        !word.starts_with('.')
    }

    #[test]
    fn empty_level() {
        let t = parse_level("", &rule_static());
        assert_eq!(t, LevelTokens::default());
    }

    #[test]
    fn plain_terms_only() {
        let t = parse_level("some fuzzy words", &rule_static());
        assert_eq!(t.terms, "some fuzzy words");
        assert!(t.modifiers.is_empty());
        assert!(t.selection.is_none());
        assert!(!t.trailing_space);
    }

    #[test]
    fn static_selection_pinned() {
        let t = parse_level("tree switch", &rule_static());
        assert_eq!(t.terms, "tree");
        assert_eq!(t.selection.as_deref(), Some("switch"));
        assert!(!t.trailing_space);
    }

    #[test]
    fn static_selection_with_drill() {
        let t = parse_level("tree switch ", &rule_static());
        assert_eq!(t.terms, "tree");
        assert_eq!(t.selection.as_deref(), Some("switch"));
        assert!(t.trailing_space);
    }

    #[test]
    fn non_matching_last_word_is_a_term() {
        let t = parse_level("tree swi", &rule_static());
        assert_eq!(t.terms, "tree swi");
        assert!(t.selection.is_none());
    }

    #[test]
    fn modifier_value_consumes_until_selection() {
        // A value consumes words until the next dot-token / valid selection /
        // end (R8-7). "big-model" is not a static child → part of the value.
        let t = parse_level(".sort:coding big-model", &rule_static());
        assert_eq!(t.modifiers.len(), 1);
        assert_eq!(t.modifiers[0].key, "sort");
        assert_eq!(t.modifiers[0].value, "coding big-model");
        assert_eq!(t.terms, "");
    }

    #[test]
    fn multi_word_modifier_value() {
        let t = parse_level(".prompt:some long prompt", &rule_static());
        assert_eq!(t.modifiers.len(), 1);
        assert_eq!(t.modifiers[0].key, "prompt");
        assert_eq!(t.modifiers[0].value, "some long prompt");
        assert!(t.selection.is_none());
    }

    #[test]
    fn modifier_value_stops_at_next_modifier() {
        let t = parse_level(".prompt:hello world .sort:new", &rule_static());
        assert_eq!(
            t.modifiers,
            vec![
                ModifierToken {
                    key: "prompt".into(),
                    value: "hello world".into()
                },
                ModifierToken {
                    key: "sort".into(),
                    value: "new".into()
                },
            ]
        );
    }

    #[test]
    fn modifier_value_stops_at_valid_selection() {
        let t = parse_level(".prompt:hello compact", &rule_static());
        // "compact" is a valid selection → value keeps only "hello".
        assert_eq!(t.selection.as_deref(), Some("compact"));
        assert_eq!(t.modifiers[0].value, "hello");
    }

    #[test]
    fn selection_wins_over_value_even_multiword() {
        let t = parse_level(".prompt:some long prompt compact", &rule_static());
        assert_eq!(t.selection.as_deref(), Some("compact"));
        assert_eq!(t.modifiers[0].value, "some long prompt");
    }

    #[test]
    fn dynamic_level_accepts_any_id() {
        // Dynamic levels: any space-free word (except dotted) can be an id.
        let rule = SelectionRule {
            is_selection: Some(std::sync::Arc::new(always_sel)),
        };
        let t = parse_level("model abc-123 ", &rule);
        assert_eq!(t.terms, "model");
        assert_eq!(t.selection.as_deref(), Some("abc-123"));
        assert!(t.trailing_space);
    }

    #[test]
    fn bare_dot_token_is_key_form() {
        let t = parse_level(".s", &rule_static());
        assert_eq!(t.modifiers.len(), 1);
        assert_eq!(t.modifiers[0].key, "s");
        assert_eq!(t.modifiers[0].value, "");
    }

    #[test]
    fn empty_value_colon_form_collects_following() {
        let t = parse_level(".prompt: world", &rule_static());
        assert_eq!(t.modifiers[0].value, "world");
    }

    #[test]
    fn parse_dot_token_forms() {
        assert_eq!(parse_dot_token("foo"), None);
        assert_eq!(parse_dot_token(".sort"), Some(DotToken::Key("sort".into())));
        assert_eq!(
            parse_dot_token(".sort:"),
            Some(DotToken::NameValue {
                key: "sort".into(),
                value: "".into()
            })
        );
        assert_eq!(
            parse_dot_token(".sort:new"),
            Some(DotToken::NameValue {
                key: "sort".into(),
                value: "new".into()
            })
        );
    }
}
