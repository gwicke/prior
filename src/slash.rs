//! Slash command registry for the interactive REPL.

use crate::fuzzy;

/// The kind of action a slash command triggers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashKind {
    Help,
    Thinking,
    Model,
}

/// A registered slash command.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: &'static str,
    pub help: &'static str,
    pub kind: SlashKind,
}

/// The static registry of built-in slash commands.
pub fn registry() -> &'static [SlashCommand] {
    static COMMANDS: &[SlashCommand] = &[
        SlashCommand {
            name: "/help",
            help: "Show available commands and key bindings",
            kind: SlashKind::Help,
        },
        SlashCommand {
            name: "/thinking",
            help: "Set thinking level: off | low | med | high",
            kind: SlashKind::Thinking,
        },
        SlashCommand {
            name: "/model",
            help: "Select a model (provider:name) with limits and cost",
            kind: SlashKind::Model,
        },
    ];
    COMMANDS
}

/// Fuzzy-filter the command registry by `query` (matches on name + help text).
pub fn filter_commands(query: &str) -> Vec<fuzzy::Ranked<'static, SlashCommand>> {
    fuzzy::fuzzy_filter(registry(), query, |c| format!("{} {}", c.name, c.help))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_three_commands() {
        assert_eq!(registry().len(), 3);
    }

    #[test]
    fn lookup_by_name() {
        let m = registry().iter().find(|c| c.name == "/model").unwrap();
        assert_eq!(m.kind, SlashKind::Model);
    }

    #[test]
    fn filter_by_query_ranks_model_first() {
        let r = filter_commands("mod");
        assert!(!r.is_empty());
        assert_eq!(r[0].item.name, "/model");
    }

    #[test]
    fn empty_query_returns_all() {
        let r = filter_commands("");
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn unknown_query_empty() {
        let r = filter_commands("zzzq");
        assert!(r.is_empty());
    }
}
