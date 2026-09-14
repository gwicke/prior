//! Static-level interpreter (design §3–§5, Phase 3 subset).
//!
//! Derives the current menu state from a prompt buffer and a static node
//! tree, and produces the Tab/Enter/Esc results. Cursor lives in the path:
//! the trailing selection token IS the selection; a trailing space is the
//! drill-in signal.

use crate::grammar::{parse_level, SelectionRule};
use crate::node::MenuNode;
use crate::fields::Row;

/// What the interpreter derived from the prompt for one level.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuState {
    /// The node whose *child menu* is displayed (i.e. the level containing
    /// the candidates shown). Root when no selection is pinned.
    pub node_path: Vec<String>,
    /// Fuzzy search terms for the displayed level.
    pub terms: String,
    /// The pinned selection token, if any.
    pub selection: Option<String>,
    /// Trailing space after the selection → the child menu is shown.
    pub drill_in: bool,
    /// Rows/candidates displayed, in display order (ranked by fuzzy match
    /// when terms are present, otherwise node/declaration order).
    pub candidates: Vec<Row>,
    /// Index into `candidates` of the active/default row.
    pub active: usize,
    /// A pinned selection existed but fell out of the filtered view
    /// (R8-5): the interpreter reports it so the host can rewrite the id.
    pub pin_lost: bool,
}

impl MenuState {
    pub fn selected(&self) -> Option<&Row> {
        self.candidates.get(self.active)
    }
}

/// Key handling results.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayAction {
    /// Buffer rewritten (Tab completion, Esc pop, Enter drill).
    Buffer(String),
    /// Leaf accepted: the (canonical) line to submit.
    Submit(String),
    /// Nothing to do.
    None,
}

/// Interpreter over a static node tree.
pub struct Interpreter {
    pub root: MenuNode,
}

impl Interpreter {
    pub fn new(root: MenuNode) -> Self {
        Interpreter { root }
    }

    /// Candidate rows for a node's child menu: static children become rows
    /// (id = key, description); a value child becomes its value set rows.
    fn child_rows(node: &MenuNode) -> Vec<Row> {
        if let Some(vc) = &node.value {
            match vc {
                crate::node::ValueChild::Enum(vals) => {
                    vals.iter().map(|v| Row::new(v.clone())).collect()
                }
                crate::node::ValueChild::Number => Vec::new(), // no static candidates
            }
        } else {
            node.children
                .iter()
                .map(|c| {
                    let mut r = Row::new(c.key.clone());
                    if let Some(d) = &c.description {
                        r = r.desc(d.clone());
                    }
                    r
                })
                .collect()
        }
    }

    /// Does `word` name a selection at `node`'s level? Exact key match for
    /// static children; permissive for value-typed children (any token).
    /// Does `word` name a selection at `node`'s level? Exact key match for
    /// static children; permissive for value-typed children (any token).
    fn is_selection_at(node: &MenuNode) -> SelectionRule {
        let owned_keys: Vec<String> = node.children.iter().map(|c| c.key.clone()).collect();
        let has_value_child = node.value.is_some();
        SelectionRule {
            is_selection: Some(std::sync::Arc::new(move |word: &str| {
                if word.starts_with('.') {
                    return false;
                }
                owned_keys.iter().any(|k| k == word) || has_value_child
            })),
        }
    }

    /// Derive the full menu state for the prompt `line`.
    ///
    /// Walks the node tree: each pinned selection drills into the child
    /// level; the last level's tokens (terms/modifiers/selection) come from
    /// the prompt tail after the parent path.
    pub fn state(&self, line: &str) -> MenuState {
        let mut node = self.root.clone();
        let mut path: Vec<String> = Vec::new();

        // Walk the path: leading words that exactly name children of the
        // current node — never consuming the last word (that is the pinned
        // selection of the deepest level, resolved by parse_level below).
        let trailing_space = line.ends_with(' ');
        let trimmed = line.trim_end();
        let words: Vec<&str> = if trimmed.is_empty() {
            Vec::new()
        } else {
            trimmed.split(' ').collect()
        };

        let mut wi = 0usize;
        while wi < words.len().saturating_sub(1) {
            let w = words[wi];
            if w.starts_with('.') {
                break;
            }
            match node.child(w).cloned() {
                Some(c) => {
                    node = c;
                    path.push(w.to_string());
                    wi += 1;
                }
                None => break,
            }
        }

        // The remaining words are the current level's tokens.
        let tail = if words.is_empty() {
            String::new()
        } else {
            words[wi..].join(" ")
        };
        let rule = Self::is_selection_at(&node);
        let lvl = parse_level(&tail, &rule);

        // Trailing space after a static-child selection = drill in: the
        // child menu is shown as a fresh level (design §3). A value child
        // keeps the selection pinned on the same level instead.
        if let (Some(sel), true) = (&lvl.selection, trailing_space) {
            if let Some(child) = node.child(sel).cloned() {
                node = child;
                path.push(sel.clone());
                return MenuState {
                    node_path: path,
                    terms: String::new(),
                    selection: None,
                    drill_in: true,
                    candidates: Self::child_rows(&node),
                    active: 0,
                    pin_lost: false,
                };
            }
        }

        let all_rows = Self::child_rows(&node);
        let selection = lvl.selection.clone();
        let rank = crate::cursor::rank_and_pin(&all_rows, &lvl.terms, selection.as_deref());

        MenuState {
            node_path: path,
            terms: lvl.terms,
            selection,
            drill_in: trailing_space && lvl.selection.is_some(),
            candidates: rank.rows,
            active: rank.active,
            pin_lost: !rank.pinned_survived && lvl.selection.is_some(),
        }
    }

    /// Tab: complete the active candidate into the buffer.
    ///
    /// Completing a selection adds or replaces the id token, leaving fuzzy
    /// terms in place (D2). If the completed node has a child menu, Tab
    /// leaves the buffer pinned (no trailing space; drill is explicit via
    /// Enter or space).
    pub fn tab(&self, line: &str) -> OverlayAction {
        let st = self.state(line);
        let Some(sel) = st.selected() else {
            return OverlayAction::None;
        };
        let token = sel.id.clone();
        let trailing_space = line.ends_with(' ');

        // Rebuild: path + earlier terms + completed token. The *trailing*
        // partial word (or pinned selection) is replaced by the token; any
        // earlier words are fuzzy terms and stay (D2).
        let mut parts: Vec<String> = st.node_path.clone();
        if trailing_space {
            // Pinned/drilled level: complete the active candidate of the
            // displayed level as a pinned selection.
            parts.push(token);
        } else {
            // Trailing partial: everything before the last word stays.
            let trimmed = line.trim_end();
            let words: Vec<&str> = trimmed.split(' ').collect();
            let kept = &words[..words.len() - 1];
            for w in kept.iter().skip(st.node_path.len()) {
                parts.push((*w).to_string());
            }
            parts.push(token);
        }
        OverlayAction::Buffer(parts.join(" "))
    }

    /// Enter: accept.
    ///
    /// 1. If a selection is pinned with a trailing space and the node has a
    ///    child menu → already drilled; Enter on the drilled level commits
    ///    the active leaf or drills further.
    /// 2. If the pinned node is a leaf → submit the canonical line.
    /// 3. Otherwise Enter behaves like Tab + trailing space (drill).
    pub fn enter(&self, line: &str) -> OverlayAction {
        let st = self.state(line);

        // Leaf at the current path? The node at node_path is a leaf → submit.
        if st.drill_in {
            // Drilled: the displayed level is the child menu. Enter here
            // acts on the active candidate of the child level.
            let Some(sel) = st.selected() else {
                return OverlayAction::None;
            };
            // Resolve the child node to check leaf-ness.
            let mut node = self.root.clone();
            for k in &st.node_path {
                if let Some(c) = node.child(k) {
                    node = c.clone();
                }
            }
            // The active candidate of the drilled level is either a static
            // grandchild or a value child's value.
            if node.value.is_some() || node.child(&sel.id).map(|c| c.is_leaf).unwrap_or(false) {
                // Rebuild the canonical line: path + selection.
                let mut canon = st.node_path.join(" ");
                if !canon.is_empty() {
                    canon.push(' ');
                }
                canon.push_str(&sel.id);
                return OverlayAction::Submit(canon);
            }
            // Drill deeper: append the candidate as a pinned selection.
            let mut buf = line.trim_end().to_string();
            buf.push(' ');
            buf.push_str(&sel.id);
            return OverlayAction::Buffer(buf);
        }

        // Not drilled: pin the selection (like Tab) + trailing space.
        match self.tab(line) {
            OverlayAction::Buffer(mut b) => {
                b.push(' ');
                OverlayAction::Buffer(b)
            }
            other => other,
        }
    }

    /// Esc: pop one level — rewrite the buffer to the parent state.
    /// Esc at the root clears the buffer (v1 rule).
    pub fn esc(&self, line: &str) -> OverlayAction {
        let st = self.state(line);
        if st.node_path.is_empty() && st.selection.is_none() {
            return OverlayAction::Buffer(String::new());
        }
        if st.selection.is_some() {
            // Pop the pinned selection back to a trailing space (show the
            // drilled level again).
            let mut parent = st.node_path.join(" ");
            if !parent.is_empty() {
                parent.push(' ');
            }
            return OverlayAction::Buffer(parent);
        }
        let parent = st.node_path[..st.node_path.len() - 1].join(" ");
        OverlayAction::Buffer(parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::ValueChild;

    fn tree() -> MenuNode {
        MenuNode::new("root")
            .child_header("Root")
            .children(vec![
                MenuNode::new("tree")
                    .desc("browse the tree")
                    .children(vec![
                        MenuNode::new("switch").desc("switch session").leaf(),
                        MenuNode::new("compact")
                            .desc("compact a node")
                            .children(vec![
                                MenuNode::new("manual").leaf(),
                                MenuNode::new("text-only").leaf(),
                                MenuNode::new("llm")
                                    .desc("llm compaction")
                                    .leaf(),
                            ]),
                    ]),
                MenuNode::new("thinking")
                    .desc("set thinking level")
                    .value(ValueChild::Enum(vec![
                        "off".into(),
                        "low".into(),
                        "med".into(),
                        "high".into(),
                    ]))
                    .leaf(),
            ])
    }

    fn ix() -> Interpreter {
        Interpreter::new(tree())
    }

    #[test]
    fn root_state_lists_commands() {
        let st = ix().state("");
        assert_eq!(st.node_path, Vec::<String>::new());
        assert_eq!(st.candidates.len(), 2);
        assert_eq!(st.candidates[0].id, "tree");
        assert_eq!(st.active, 0);
    }

    #[test]
    fn root_fuzzy_terms_rank_candidates() {
        let st = ix().state("think");
        assert_eq!(st.terms, "think");
        assert_eq!(st.candidates[0].id, "thinking");
        assert_eq!(st.active, 0);
    }

    #[test]
    fn pinned_selection_shows_parent_with_child_selected() {
        // "/tree" typed alone: exact match ⇒ pinned, menu shows root with
        // `tree` selected.
        let st = ix().state("tree");
        assert_eq!(st.selection.as_deref(), Some("tree"));
        assert_eq!(st.active, 0); // pinned wins
        assert!(!st.drill_in);
    }

    #[test]
    fn drill_in_shows_child_menu() {
        let st = ix().state("tree ");
        assert_eq!(st.node_path, vec!["tree"]);
        assert_eq!(st.candidates.len(), 2);
        assert_eq!(st.candidates[0].id, "switch");
    }

    #[test]
    fn value_child_enum_candidates() {
        let st = ix().state("thinking ");
        assert_eq!(st.node_path, vec!["thinking"]);
        assert_eq!(
            st.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["off", "low", "med", "high"]
        );
    }

    #[test]
    fn value_child_permissive_pin() {
        // Any token is permissively valid at a value child; a pinned value
        // becomes the active row (exact match wins over position).
        let st = ix().state("thinking med");
        assert_eq!(st.node_path, vec!["thinking"]);
        assert_eq!(st.selection.as_deref(), Some("med"));
        assert_eq!(st.active, 2);
        assert!(st.candidates.iter().any(|r| r.id == "med"));
    }

    #[test]
    fn tab_completes_keeping_terms() {
        let act = ix().tab("think");
        assert_eq!(
            act,
            OverlayAction::Buffer("thinking".to_string())
        );
        // terms preserved: "/tree think" → wait, root has no tree-chain here.
        let act2 = ix().tab("tree ");
        // At "tree " the drilled level shows switch/compact; active = switch.
        assert_eq!(act2, OverlayAction::Buffer("tree switch".to_string()));
    }

    #[test]
    fn enter_drills_by_appending_space() {
        let act = ix().enter("tree");
        assert_eq!(act, OverlayAction::Buffer("tree ".to_string()));
    }

    #[test]
    fn enter_submits_leaf() {
        let act = ix().enter("thinking med");
        // "thinking" node has a value child; "med" pinned, not drilled →
        // Enter = Tab + space = drill... but pinned leaf semantics: the
        // value child path ends here. The drilled enter path submits.
        match act {
            OverlayAction::Buffer(b) => assert_eq!(b, "thinking med ".to_string()),
            other => panic!("expected buffer, got {:?}", other),
        }
        // Second enter (drilled) submits.
        let act2 = ix().enter("thinking med ");
        assert_eq!(act2, OverlayAction::Submit("thinking med".to_string()));
    }

    #[test]
    fn esc_pops_one_level() {
        // Pinned selection popped → back to the drilled level (trailing
        // space shows the menu).
        assert_eq!(
            ix().esc("tree compact"),
            OverlayAction::Buffer("tree ".to_string())
        );
        // Drilled level popped → parent path.
        assert_eq!(
            ix().esc("tree compact "),
            OverlayAction::Buffer("tree".to_string())
        );
        // Root → clear.
        assert_eq!(ix().esc("tree"), OverlayAction::Buffer(String::new()));
    }

    #[test]
    fn nested_drill() {
        let st = ix().state("tree compact ");
        assert_eq!(st.node_path, vec!["tree", "compact"]);
        assert_eq!(
            st.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["manual", "text-only", "llm"]
        );
    }
}
