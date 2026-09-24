//! v2.5 root menu builder — constructs the `SlashDotMenu` tree with all
//! ported commands. Called once at REPL startup.

use std::cell::RefCell;
use std::rc::Rc;

use crate::slash_menu_bridge::{LiveState, MenuShared};
use crate::settings::SettingsStore;
use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
use crate::slashdot_menu::node_impl::{StaticListingNode, StaticRow};

use crate::v25_thinking::ThinkingEnumNode;

/// Build the v2.5 `SlashDotMenu` tree with all ported commands.
pub fn build_menu(
    live_state: &Rc<RefCell<LiveState>>,
    settings: &Rc<RefCell<SettingsStore>>,
    shared: &Rc<MenuShared>,
) -> SlashDotMenu {
    let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();

    // Thinking: listing + single enum node (returns all levels via data()).
    {
        let ls = live_state.clone();
        let ls2 = live_state.clone();
        let mut tl: SlashDotMenuNode = StaticListingNode::new(&[
            StaticRow::new("thinking", "Set thinking level: off | low | med | high"),
        ]).with_columns(vec![
            crate::slashdot_menu::ColumnDef::new("id", "Thinking Level"),
            crate::slashdot_menu::ColumnDef::new("description", "Description"),
        ]).into();
        tl.add_child(ThinkingEnumNode {
            active_fn: Box::new(move || {
                let ls = ls.borrow();
                ls.thinking_level.clone().unwrap_or_else(|| "off".to_string())
            }),
            submit_fn: Box::new(move |level| {
                ls2.borrow_mut().thinking_level = Some(level.to_string());
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
        crate::v25_persona::add_persona_child(&mut pl, live_state);
        root.add_child(pl);
    }

    // Model
    {
        let ml: SlashDotMenuNode = crate::v25_model::build_model_node(shared);
        root.add_child(ml);
    }

    // Settings (/set)
    {
        let sn: SlashDotMenuNode = crate::v25_set::build_set_node(settings);
        root.add_child(sn);
    }

    // Stat (/stat) — leaf node directly under root.
    // data() returns the row for the root listing; header() shows stats.
    {
        root.add_child(crate::v25_stat::StatNode {
            shared: shared.clone(),
        }.into());
    }

    // Replay (/replay) — leaf node directly under root.
    // data() returns the row for the root listing; header() shows help.
    {
        root.add_child(crate::v25_replay::ReplayNode {
            shared: shared.clone(),
        }.into());
    }

    // Session (/session) — dynamic listing with action children.
    {
        root.add_child(crate::v25_session::build_session_node(shared));
    }

    // New (/new) — leaf node; sets AsyncOp::New.
    {
        root.add_child(crate::v25_new::NewNode {
            shared: shared.clone(),
        }.into());
    }

    // Clear (/clear) — leaf node; sets AsyncOp::Clear.
    {
        root.add_child(crate::v25_clear::ClearNode {
            shared: shared.clone(),
        }.into());
    }

    // Compact (/compact) — leaf node; redirects to /tree <id> compact.
    {
        root.add_child(crate::v25_tree::CompactNode {
            shared: shared.clone(),
        }.into());
    }

    // Tree (/tree) — dynamic listing with action children.
    {
        root.add_child(crate::v25_tree::build_tree_node(shared));
    }

    // Sandbox (/sandbox) — path mappings: static add + dynamic mapping
    // listing with remove/restore actions.
    {
        root.add_child(crate::v25_sandbox::build_sandbox_node(shared));
    }

    // Quit (/quit) — leaf node; sets AsyncOp::Quit.
    {
        root.add_child(crate::v25_quit::QuitNode {
            shared: shared.clone(),
        }.into());
    }

    SlashDotMenu::from_root_node(root)
}
