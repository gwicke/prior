//! Slash command registry for the interactive REPL.

use crate::fuzzy;

/// The kind of action a slash command triggers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashKind {
    Help,
    Thinking,
    Model,
    Settings,
    Persona,
    Stat,
    Quit,
    Sandbox,
    Session,
    Tree,
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
        SlashCommand {
            name: "/set",
            help: "View and edit settings",
            kind: SlashKind::Settings,
        },
        SlashCommand {
            name: "/persona",
            help: "Set runtime persona (built-in name, file path, or 'none')",
            kind: SlashKind::Persona,
        },
        SlashCommand {
            name: "/stat",
            help: "Show session stats: context window, usage, cost, cache hit",
            kind: SlashKind::Stat,
        },
        SlashCommand {
            name: "/quit",
            help: "Quit the session",
            kind: SlashKind::Quit,
        },
        SlashCommand {
            name: "/sandbox",
            help: "Manage session path bindings: add_ro | add_rw | remove",
            kind: SlashKind::Sandbox,
        },
        SlashCommand {
            name: "/session",
            help: "List, rename, resume, fork, or delete sessions",
            kind: SlashKind::Session,
        },
        SlashCommand {
            name: "/tree",
            help: "Navigate session history — select a point to continue from",
            kind: SlashKind::Tree,
        },
    ];
    COMMANDS
}

/// Fuzzy-filter the command registry by `query` (matches on name + help text).
pub fn filter_commands(query: &str) -> Vec<fuzzy::Ranked<'static, SlashCommand>> {
    fuzzy::fuzzy_filter(registry(), query, |c| format!("{} {}", c.name, c.help))
}

/// Parse the text after a leading `/` into a dispatch intent.
///
/// Examples:
/// - `""`             -> `Help` (bare `/` lists commands)
/// - `"help"`         -> `Help`
/// - `"model"`        -> `Model`
/// - `"thinking low"` -> `Thinking("low")`
/// - `"bogus"`        -> `Unknown("bogus")`
#[derive(Debug, Clone, PartialEq)]
pub enum SlashIntent {
    /// Show help / command list.
    Help,
    /// Open the model selector.
    Model,
    /// Set thinking level (argument may be empty to mean "show current").
    Thinking(String),
    /// Unknown command name.
    Unknown(String),
}

/// Parse the text following a leading `/` into a [`SlashIntent`].
pub fn parse_slash(rest: &str) -> SlashIntent {
    let rest = rest.trim();
    if rest.is_empty() {
        return SlashIntent::Help;
    }
    let mut parts = rest.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let arg = parts.next().unwrap_or("").trim().to_string();
    match cmd.as_str() {
        "help" | "h" | "?" => SlashIntent::Help,
        "model" | "models" | "m" => SlashIntent::Model,
        "thinking" | "think" | "t" => SlashIntent::Thinking(arg),
        _ => SlashIntent::Unknown(cmd),
    }
}

/// Human-readable help text listing commands and key bindings.
pub fn render_help() -> String {
    let mut s = String::from("Commands:\n");
    for c in registry() {
        s.push_str(&format!("  {:<10} — {}\n", c.name, c.help));
    }
    s.push_str("\nShell:\n");
    s.push_str("  !cmd        run `cmd` in the sandbox; output is shown AND added to context\n");
    s.push_str("  !!cmd       run `cmd` in the sandbox; output is shown ONLY (not in context)\n");
    s.push_str("\nKey bindings:\n");
    s.push_str("  Enter        submit line\n");
    s.push_str("  Shift-Enter  newline (multi-line). On any xterm-compatible\n");
    s.push_str("                terminal (wezterm, konsole, gnome-terminal, alacritty…)\n");
    s.push_str("                without enabling the Kitty keyboard protocol it arrives as\n");
    s.push_str("                the legacy `ESC[13;2~` sequence (mapped internally to a\n");
    s.push_str("                newline); with Kitty ON (enable_kitty_keyboard=true) it\n");
    s.push_str("                arrives as a true Shift+Enter.\n");
    s.push_str("  \\\\+Enter     newline (multi-line) — works on ANY terminal, no Kitty\n");
    s.push_str("                protocol needed (type \\\\ before Enter)\n");
    s.push_str("  Ctrl-J       newline (reliable on every terminal)\n");
    s.push_str("  Esc          cancel current line\n");
    s.push_str("  Up/Down      history (or move across lines)\n");
    s.push_str("  Alt-B/Alt-F  word left / right\n");
    s.push_str("  Ctrl-U/K     delete to line start / end\n");
    s.push_str("  Ctrl-C       exit\n");
    s.push_str("  /q, exit     quit session\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_ten_commands() {
        assert_eq!(registry().len(), 10);
    }

    #[test]
    fn lookup_session_command() {
        let s = registry().iter().find(|c| c.name == "/session").unwrap();
        assert_eq!(s.kind, SlashKind::Session);
    }

    #[test]
    fn lookup_by_name() {
        let m = registry().iter().find(|c| c.name == "/model").unwrap();
        assert_eq!(m.kind, SlashKind::Model);
        let s = registry().iter().find(|c| c.name == "/set").unwrap();
        assert_eq!(s.kind, SlashKind::Settings);
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
        assert_eq!(r.len(), 10);
    }

    #[test]
    fn parse_slash_intents() {
        assert_eq!(parse_slash(""), SlashIntent::Help);
        assert_eq!(parse_slash("help"), SlashIntent::Help);
        assert_eq!(parse_slash("THINKING low"), SlashIntent::Thinking("low".into()));
        assert_eq!(parse_slash("thinking"), SlashIntent::Thinking("".into()));
        assert_eq!(parse_slash("model"), SlashIntent::Model);
        assert_eq!(parse_slash("bogus"), SlashIntent::Unknown("bogus".into()));
    }

    #[test]
    fn render_help_lists_commands() {
        let h = render_help();
        assert!(h.contains("/model"));
        assert!(h.contains("/thinking"));
        assert!(h.contains("Ctrl-C"));
    }

    #[test]
    fn render_help_is_plain_and_aligned() {
        let h = render_help();
        assert!(h.contains("Commands:"));
        // command names are column-aligned (fixed-width pad)
        assert!(h.contains("  /help"));
        assert!(h.contains("  /model"));
        assert!(h.contains("  /thinking"));
        // No termimad-style double-space artifacts between words.
        assert!(!h.contains("Show  available"));
        assert!(!h.contains("Commands :"));
    }
}
