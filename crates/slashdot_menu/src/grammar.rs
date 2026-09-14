//! Path grammar for slash-menu prompts.
//!
//! Phase 0 placeholder: Phase 1 implements the full per-level tokenizer
//! (`[fuzzy terms][dot modifiers][selection]`, structural terminators).

/// A dot-modifier token parsed from a prompt: `.key:value`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModifierToken {
    pub key: String,
    pub value: String,
}

/// One grammar level parsed from the prompt's tail.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Level {
    /// Free-text fuzzy search terms (may contain spaces).
    pub terms: String,
    /// Dot modifiers in order of appearance.
    pub modifiers: Vec<ModifierToken>,
    /// The selection token (child key or row id), if pinned.
    pub selection: Option<String>,
    /// `true` when a trailing space follows the selection (drill-in signal).
    pub trailing_space: bool,
}
