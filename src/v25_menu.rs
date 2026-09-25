//! v2.5 root menu builder — constructs the `SlashDotMenu` tree with all
//! ported commands. Called once at REPL startup.

use crate::app_state::SharedApp;
use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
use crate::slashdot_menu::node_impl::{StaticListingNode, StaticRow};

use crate::v25_thinking::ThinkingEnumNode;

/// Build the v2.5 `SlashDotMenu` tree with all ported commands.
pub fn build_menu(app: &SharedApp) -> SlashDotMenu {
    let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();

    // Thinking: listing + single enum node (returns all levels via data()).
    {
        let reader = app.clone();
        let writer = app.clone();
        let mut tl: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("thinking", "Set thinking level: off | low | med | high"),
        ]).with_columns(vec![
            crate::slashdot_menu::ColumnDef::new("id", "Thinking Level"),
            crate::slashdot_menu::ColumnDef::new("description", "Description"),
        ]).into();
        tl.add_child(ThinkingEnumNode {
            active_fn: Box::new(move || {
                reader.thinking().unwrap_or_else(|| "off".to_string())
            }),
            submit_fn: Box::new(move |level| {
                writer.set_thinking(Some(level.to_string()));
                Ok(())
            }),
        }.into());
        root.add_child(tl);
    }

    // Persona
    {
        let mut pl: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("persona", "set runtime persona"),
        ]).with_columns(vec![
            crate::slashdot_menu::ColumnDef::new("id", "Persona"),
            crate::slashdot_menu::ColumnDef::new("description", "Description"),
        ]).into();
        crate::v25_persona::add_persona_child(&mut pl, app);
        root.add_child(pl);
    }

    // Model
    {
        let ml: SlashDotMenuNode = crate::v25_model::build_model_node(app);
        root.add_child(ml);
    }

    // Settings (/set)
    {
        let sn: SlashDotMenuNode = crate::v25_set::build_set_node(app);
        root.add_child(sn);
    }

    // Stat (/stat) — leaf node directly under root.
    // data() returns the row for the root listing; header() shows stats.
    {
        root.add_child(crate::v25_stat::StatNode {
            app: app.clone(),
        }.into());
    }

    // Replay (/replay) — leaf node directly under root.
    // data() returns the row for the root listing; header() shows help.
    {
        root.add_child(crate::v25_replay::ReplayNode {
            app: app.clone(),
        }.into());
    }

    // Session (/session) — dynamic listing with action children.
    {
        root.add_child(crate::v25_session::build_session_node(app));
    }

    // New (/new) — leaf node; sets the `new_session` control target.
    {
        root.add_child(crate::v25_new::NewNode {
            app: app.clone(),
        }.into());
    }

    // Clear (/clear) — leaf node; composes new + delete-ids targets.
    {
        root.add_child(crate::v25_clear::ClearNode {
            app: app.clone(),
        }.into());
    }

    // Compact (/compact) — leaf node; redirects to /tree <id> compact.
    {
        root.add_child(crate::v25_tree::CompactNode {
            app: app.clone(),
        }.into());
    }

    // Tree (/tree) — dynamic listing with action children.
    {
        root.add_child(crate::v25_tree::build_tree_node(app));
    }

    // Sandbox (/sandbox) — path mappings: static add + dynamic mapping
    // listing with remove/restore actions.
    {
        root.add_child(crate::v25_sandbox::build_sandbox_node(app));
    }

    // Quit (/quit) — leaf node; latches the `quit` control target.
    {
        root.add_child(crate::v25_quit::QuitNode {
            app: app.clone(),
        }.into());
    }

    SlashDotMenu::from_root_node(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slashdot_menu::node_impl::MenuResult;
    use crate::slashdot_menu::HostKey;

    fn test_app() -> SharedApp {
        let settings = crate::settings::SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_thinking_level: "off".into(),
            show_reasoning: "indicator".into(),
            reasoning_in_context: false,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            path: None,
        };
        crate::app_state::AppShared::for_test(settings)
    }

    /// The root listing lists every command, and `help` is **absent**
    /// (codifies the deliberate removal of the help overlay).
    #[test]
    fn root_listing_lists_all_commands_without_help() {
        let app = test_app();
        let mut menu = build_menu(&app);
        let mut h = menu.get_handle();
        match h.eval_prompt("", HostKey::None, 80) {
            MenuResult::Continue(plan, _) => {
                let ids: Vec<&str> = plan
                    .rows
                    .iter()
                    .filter_map(|l| l.0.split_whitespace().next())
                    .collect();
                for cmd in [
                    "thinking", "persona", "model", "set", "stat", "replay", "session",
                    "new", "clear", "compact", "tree", "sandbox", "quit",
                ] {
                    assert!(ids.contains(&cmd), "missing '{cmd}' in root listing: {ids:?}");
                }
                assert!(
                    !ids.contains(&"help"),
                    "'help' must stay removed from the root listing: {ids:?}"
                );
            }
            other => panic!("expected Continue at root, got {:?}", std::mem::discriminant(&other)),
        }
    }
}
