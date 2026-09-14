//! Accept flows (design §9): leaf invocation, interstitial (D4), confirm
//! (D5), editor handoff, and "return new path" rewrites (R8-8).
//!
//! The framework owns the flow state; leaf handlers are stored per node
//! path and carry a mutable instance for the stateful confirm case
//! (single handler instance across prep → accept, §9.2).

use std::collections::HashMap;

/// Result a leaf handler returns.
#[derive(Debug, Clone, PartialEq)]
pub enum LeafResult {
    /// Action done; close the menu. Optional message (success or failure).
    Done(Option<String>),
    /// Rewrite the buffer to a new menu path (R8-8, `/compact` case). The
    /// trailing space (or its absence) is part of the path semantics.
    NewPath(String),
    /// Launch `$EDITOR` on the payload; the framework suspends the UI.
    /// Cancellation re-enters the prompt via the host's prefill mechanism.
    Editor,
}

/// A leaf invocation: canonical line + the selected row's fields when the
/// leaf sits below a dynamic level.
#[derive(Debug, Clone)]
pub struct LeafInvocation {
    pub line: String,
    pub node_path: Vec<String>,
    /// Fields of the selected dynamic row, when applicable.
    pub row_fields: Vec<(String, crate::value::Value)>,
    /// Mutable confirm state survives prep → accept through this instance.
    /// Handlers get `&mut` via interior mutability through the controller
    /// (the invocation holds the state by value; the controller hands the
    /// same instance back on the second step).
    pub state: std::cell::RefCell<ConfirmState>,
}

/// State carried across the two confirm steps (D5). Empty for plain leaves.
#[derive(Default, Debug, Clone)]
pub struct ConfirmState {
    pub data: Vec<(String, crate::value::Value)>,
}

impl ConfirmState {
    pub fn set(&mut self, k: &str, v: crate::value::Value) {
        self.data.retain(|(k2, _)| k2 != k);
        self.data.push((k.to_string(), v));
    }

    pub fn get(&self, k: &str) -> Option<&crate::value::Value> {
        self.data.iter().find(|(k2, _)| k2 == k).map(|(_, v)| v)
    }
}

/// Leaf handler: receives the invocation, returns the outcome. `&mut self`
/// gives confirm flows their surviving instance state.
pub trait LeafHandler {
    fn invoke(&mut self, inv: &LeafInvocation) -> LeafResult;
}

/// Convenience impl for function pointers/closures.
impl<F> LeafHandler for F
where
    F: FnMut(&LeafInvocation) -> LeafResult,
{
    fn invoke(&mut self, inv: &LeafInvocation) -> LeafResult {
        self(inv)
    }
}

/// Registry of leaf handlers keyed by node path ("tree compact llm").
#[derive(Default)]
pub struct LeafRegistry {
    handlers: HashMap<String, Box<dyn LeafHandler>>,
}

impl LeafRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, path: &[&str], h: Box<dyn LeafHandler>) {
        self.handlers.insert(path.join(" "), h);
    }

    pub fn get(&mut self, path: &[String]) -> Option<&mut Box<dyn LeafHandler>> {
        self.handlers.get_mut(&path.join(" "))
    }

    pub fn contains(&self, path: &[String]) -> bool {
        self.handlers.contains_key(&path.join(" "))
    }
}

/// Phase of a paused accept flow, tracked by the framework.
#[derive(Debug, Clone, PartialEq)]
pub enum AcceptPhase {
    /// Normal menu interaction.
    Menu,
    /// Interstitial shown (D4). `leaf_ready` = a leaf fires on next Enter;
    /// otherwise the next Enter dismisses the menu entirely (/stat).
    Interstitial { line: String, leaf_ready: bool },
    /// Confirm prep done (D5); waiting for the accept/abort Enter/Esc.
    ConfirmPrep { line: String },
}

impl Default for AcceptPhase {
    fn default() -> Self {
        AcceptPhase::Menu
    }
}

/// The accept-flow controller. Wraps the interpreter's Enter handling: when
/// a leaf fires, the controller routes it through the registry and maps the
/// `LeafResult` onto the overlay actions.
pub struct AcceptController {
    pub registry: LeafRegistry,
    pub phase: AcceptPhase,
    /// The menu tree, for node property lookups (interstitial, confirm).
    pub root: crate::node::MenuNode,
    /// Confirm state per leaf path — survives prep → accept (D5) because
    /// the controller owns it, not the transient invocation.
    confirm_states: HashMap<String, ConfirmState>,
}

impl Default for AcceptController {
    fn default() -> Self {
        AcceptController {
            registry: LeafRegistry::new(),
            phase: AcceptPhase::Menu,
            root: crate::node::MenuNode::default(),
            confirm_states: HashMap::new(),
        }
    }
}

/// What the host should do after a controller step.
#[derive(Debug, Clone, PartialEq)]
pub enum AcceptOutcome {
    /// Keep interacting; the overlay action applies as usual.
    Continue(crate::interpreter::OverlayAction),
    /// Close the menu. Optional message to print.
    Close(Option<String>),
    /// Show interstitial text; menu stays logically open.
    ShowInterstitial { text: String, line: String },
    /// Launch the editor (leaf-returned Editor signal or confirm prep that
    /// hands off). Cancellation → host re-enters with `line` as prefill.
    LaunchEditor { line: String },
    /// Buffer rewritten to a new menu path; keep the menu open.
    Rewrite(String),
}

impl AcceptController {
    pub fn new(root: crate::node::MenuNode) -> Self {
        AcceptController {
            registry: LeafRegistry::new(),
            phase: AcceptPhase::Menu,
            root,
            confirm_states: HashMap::new(),
        }
    }

    /// Register a leaf handler for a node path.
    pub fn register(&mut self, path: &[&str], h: Box<dyn LeafHandler>) {
        self.registry.register(path, h);
    }

    /// Enter pressed. `enter_action` is the interpreter's structural answer
    /// (Buffer drill / Submit); the controller intercepts Submits that hit
    /// an interstitial or confirm leaf and runs the flow.
    pub fn on_enter(
        &mut self,
        ix_action: crate::interpreter::OverlayAction,
        state: &crate::interpreter::MenuState,
        line: &str,
    ) -> AcceptOutcome {
        let phase = self.phase.clone();
        match phase {
            AcceptPhase::Menu => match ix_action {
                crate::interpreter::OverlayAction::Submit(canon) => {
                    self.submit(canon, state, line)
                }
                other => AcceptOutcome::Continue(other),
            },
            AcceptPhase::Interstitial { line: l, leaf_ready } => {
                let l = l.clone();
                let leaf_ready = leaf_ready;
                if leaf_ready {
                    self.phase = AcceptPhase::Menu;
                    self.invoke_leaf(l, state)
                } else {
                    // No leaf: dismiss the menu entirely (/stat).
                    AcceptOutcome::Close(None)
                }
            }
            AcceptPhase::ConfirmPrep { line: l } => {
                // Second Enter: accept → invoke the leaf (with surviving
                // instance state); the handler decides Done vs NewPath.
                self.phase = AcceptPhase::Menu;
                let canon = l.clone();
                self.invoke_leaf(canon, state)
            }
        }
    }

    /// Esc pressed during a paused flow: abort confirm (prep state is
    /// dropped; the handler instance is reused next time — per §9 the same
    /// instance is deliberately kept for the flow lifetime).
    pub fn on_esc(&mut self) {
        self.phase = AcceptPhase::Menu;
    }

    fn submit(
        &mut self,
        canon: String,
        state: &crate::interpreter::MenuState,
        _line: &str,
    ) -> AcceptOutcome {
        let leaf_path = self.leaf_path(state);
        let has_interstitial = self.node_prop_interstitial(&leaf_path);
        let is_confirm = self.node_prop_confirm(&leaf_path);
        let has_leaf = self.registry.contains(&leaf_path);

        if has_interstitial && self.phase == AcceptPhase::Menu {
            // First Enter shows the interstitial (D4).
            let text = self.node_interstitial_text(&leaf_path);
            self.phase = AcceptPhase::Interstitial {
                line: canon.clone(),
                leaf_ready: has_leaf,
            };
            return AcceptOutcome::ShowInterstitial { text, line: canon };
        }
        if is_confirm && has_leaf {
            // Prep step (D5): the single handler instance performs the prep
            // on the first Enter (compaction precompute) and accept on the
            // second. Invoke via invoke_leaf so confirm state persists.
            self.phase = AcceptPhase::ConfirmPrep { line: canon.clone() };
            match self.invoke_leaf(canon.clone(), state) {
                AcceptOutcome::Close(msg) => {
                    // Prep reported completion: stay paused for the confirm
                    // decision; the prep message is shown alongside the
                    // confirm prompt by the host.
                    let _ = msg;
                    AcceptOutcome::Continue(crate::interpreter::OverlayAction::Buffer(format!(
                        "{} ",
                        canon
                    )))
                }
                other => other,
            }
        } else if has_leaf {
            self.invoke_leaf(canon, state)
        } else {
            // No handler: plain structural accept.
            AcceptOutcome::Continue(crate::interpreter::OverlayAction::Submit(canon))
        }
    }

    /// Registry key for the leaf under the cursor: the static path; a
    /// dynamic level terminates it (the row id is the selection, not a
    /// path component — the handler lives on the dynamic node).
    fn leaf_path(&self, state: &crate::interpreter::MenuState) -> Vec<String> {
        if state.level_is_dynamic {
            state.node_path.clone()
        } else {
            let mut p = state.node_path.clone();
            if let Some(sel) = &state.selection {
                p.push(sel.clone());
            }
            p
        }
    }

    fn invoke_leaf(
        &mut self,
        canon: String,
        state: &crate::interpreter::MenuState,
    ) -> AcceptOutcome {
        let leaf_path = self.leaf_path(state);
        let inv = self.invocation(&canon, state);
        let key = leaf_path.join(" ");
        let outcome = match self.registry.get(&leaf_path) {
            Some(h) => match h.invoke(&inv) {
                LeafResult::Done(msg) => AcceptOutcome::Close(msg),
                LeafResult::NewPath(p) => AcceptOutcome::Rewrite(p),
                LeafResult::Editor => AcceptOutcome::LaunchEditor { line: canon },
            },
            None => {
                return AcceptOutcome::Continue(crate::interpreter::OverlayAction::Submit(
                    inv.line,
                ))
            }
        };
        // Persist the (possibly mutated) confirm state back into the
        // controller so it survives prep → accept (D5).
        let final_state = inv.state.borrow().clone();
        self.confirm_states.insert(key, final_state);
        outcome
    }

    fn invocation(
        &mut self,
        canon: &str,
        state: &crate::interpreter::MenuState,
    ) -> LeafInvocation {
        let key = self.leaf_path(state).join(" ");
        let cs = self
            .confirm_states
            .entry(key)
            .or_default()
            .clone();
        LeafInvocation {
            line: canon.to_string(),
            node_path: state.node_path.clone(),
            row_fields: state
                .selected()
                .map(|r| r.fields.clone())
                .unwrap_or_default(),
            state: std::cell::RefCell::new(cs),
        }
    }

    // Node property lookups: walk the static root through the path; fall
    // back to the last resolvable node (dynamic rows take the dynamic
    // node's properties).
    fn node_at(&self, path: &[String]) -> crate::node::MenuNode {
        let mut node = self.root.clone();
        for p in path {
            match node.child(p) {
                Some(c) => node = c.clone(),
                None => break,
            }
        }
        node
    }

    fn node_prop_interstitial(&self, path: &[String]) -> bool {
        self.node_at(path).interstitial.is_some()
    }

    fn node_interstitial_text(&self, path: &[String]) -> String {
        self.node_at(path)
            .interstitial
            .unwrap_or_default()
    }

    fn node_prop_confirm(&self, path: &[String]) -> bool {
        self.node_at(path).confirm
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::Row;
    use crate::interpreter::{Interpreter, OverlayAction};
    use crate::node::MenuNode;
    use crate::value::Value;

    // --- /stat shape: interstitial, no leaf ---

    fn stat_tree() -> MenuNode {
        MenuNode::new("root").children(vec![
            MenuNode::new("stat")
                .desc("session statistics")
                .interstitial("tokens in/out, cost, cache hits…"),
            MenuNode::new("replay")
                .desc("replay last turn")
                .interstitial("rendering transcript…")
                .leaf(),
        ])
    }

    fn controller(tree: MenuNode) -> AcceptController {
        AcceptController::new(tree)
    }

    fn submit_action() -> OverlayAction {
        OverlayAction::Submit("stat".to_string())
    }

    #[test]
    fn interstitial_no_leaf_second_enter_closes() {
        let mut c = controller(stat_tree());
        let mut ix = Interpreter::new(stat_tree());
        let st = ix.state("stat");
        // First Enter: show interstitial, menu stays.
        let out = c.on_enter(submit_action(), &st, "stat");
        assert_eq!(
            out,
            AcceptOutcome::ShowInterstitial {
                text: "tokens in/out, cost, cache hits…".into(),
                line: "stat".into()
            }
        );
        // Second Enter: dismiss the menu entirely (D4).
        let out2 = c.on_enter(OverlayAction::None, &st, "stat");
        assert_eq!(out2, AcceptOutcome::Close(None));
    }

    // --- /replay shape: interstitial + leaf ---

    #[test]
    fn interstitial_with_leaf_next_enter_invokes() {
        let mut c = controller(stat_tree());
        c.register(
            &["replay"],
            Box::new(|_inv: &LeafInvocation| LeafResult::Done(Some("replayed".into()))),
        );
        let mut ix = Interpreter::new(stat_tree());
        let st = ix.state("replay");
        let out = c.on_enter(OverlayAction::Submit("replay".into()), &st, "replay");
        assert!(matches!(out, AcceptOutcome::ShowInterstitial { .. }));
        // Next Enter: leaf fires → close with message.
        let out2 = c.on_enter(OverlayAction::None, &st, "replay");
        assert_eq!(out2, AcceptOutcome::Close(Some("replayed".into())));
    }

    // --- /compact shape: leaf returns new path (R8-8) ---

    #[test]
    fn leaf_returns_new_path() {
        let tree = MenuNode::new("root").children(vec![MenuNode::new("compact").leaf()]);
        let mut c = controller(tree.clone());
        c.register(
            &["compact"],
            Box::new(|inv: &LeafInvocation| {
                // Sugar over /tree: rewrite to the last user node's compact
                // menu (trailing space shows the menu immediately).
                LeafResult::NewPath("/tree u7 compact ".to_string())
            }),
        );
        let mut ix = Interpreter::new(tree);
        let st = ix.state("compact");
        let out = c.on_enter(OverlayAction::Submit("compact".into()), &st, "compact");
        assert_eq!(
            out,
            AcceptOutcome::Rewrite("/tree u7 compact ".to_string())
        );
    }

    // --- confirm (D5): single instance, state survives prep → accept ---

    #[test]
    fn confirm_prep_then_accept_shares_state() {
        // State key the prep writes and accept reads, proving instance
        // survival across the two Enter presses.
        const KEY: &str = "prepared";
        let tree = MenuNode::new("root").children(vec![
            MenuNode::new("session").children(vec![MenuNode::new("delete").confirm().leaf()]),
        ]);
        let mut c = controller(tree.clone());
        c.register(
            &["session", "delete"],
            Box::new(move |inv: &LeafInvocation| {
                if inv.state.borrow().get(KEY).is_none() {
                    // Prep step: stash, report done, stay paused.
                    inv.state
                        .borrow_mut()
                        .set(KEY, Value::Str("snapshotted".into()));
                    LeafResult::Done(Some("prepared".into()))
                } else {
                    // Accept step: read the surviving state.
                    match inv.state.borrow().get(KEY) {
                        Some(Value::Str(s)) if s == "snapshotted" => {
                            LeafResult::Done(Some("deleted".into()))
                        }
                        _ => LeafResult::Done(Some("state lost!".into())),
                    }
                }
            }),
        );
        let mut ix = Interpreter::new(tree);
        let st = ix.state("session s1 delete");
        // First Enter: confirm prep (handler invoked once).
        let out = c.on_enter(OverlayAction::Submit("session s1 delete".into()), &st, "x");
        assert!(matches!(
            out,
            AcceptOutcome::Continue(OverlayAction::Buffer(_))
        ));
        assert!(matches!(c.phase, AcceptPhase::ConfirmPrep { .. }));
        // Second Enter: accept — state must have survived.
        let out2 = c.on_enter(OverlayAction::None, &st, "x");
        assert_eq!(out2, AcceptOutcome::Close(Some("deleted".into())));
    }

    #[test]
    fn confirm_esc_aborts_flow() {
        let tree = MenuNode::new("root")
            .children(vec![MenuNode::new("del").confirm().leaf()]);
        let mut c = controller(tree.clone());
        let mut fired = 0usize;
        c.register(
            &["del"],
            Box::new(move |_inv: &LeafInvocation| {
                fired += 1;
                LeafResult::Done(None)
            }),
        );
        let mut ix = Interpreter::new(tree);
        let st = ix.state("del");
        let _ = c.on_enter(OverlayAction::Submit("del".into()), &st, "del");
        assert!(matches!(c.phase, AcceptPhase::ConfirmPrep { .. }));
        c.on_esc();
        assert_eq!(c.phase, AcceptPhase::Menu);
        // Next Enter starts a *fresh* flow (prep again), not a stale accept.
        let _ = c.on_enter(OverlayAction::Submit("del".into()), &st, "del");
        assert!(matches!(c.phase, AcceptPhase::ConfirmPrep { .. }));
    }

    // --- editor handoff (§9) ---

    #[test]
    fn leaf_editor_signal_launches_editor() {
        let tree = MenuNode::new("root").children(vec![MenuNode::new("edit").leaf()]);
        let mut c = controller(tree.clone());
        c.register(
            &["edit"],
            Box::new(|_inv: &LeafInvocation| LeafResult::Editor),
        );
        let mut ix = Interpreter::new(tree);
        let st = ix.state("edit");
        let out = c.on_enter(OverlayAction::Submit("edit".into()), &st, "edit");
        assert_eq!(
            out,
            AcceptOutcome::LaunchEditor { line: "edit".into() }
        );
    }

    // --- dynamic rows: leaf on the dynamic node receives row fields ---

    #[test]
    fn dynamic_leaf_receives_row_fields() {
        use crate::provider::{DataResult, StaticProvider};
        let tree = MenuNode::new("root")
            .children(vec![MenuNode::new("model").dynamic().leaf()]);
        let mut c = controller(tree.clone());
        c.register(
            &["model"],
            Box::new(|inv: &LeafInvocation| {
                match inv
                    .row_fields
                    .iter()
                    .find(|(k, _)| k == "tier")
                    .map(|(_, v)| v.clone())
                {
                    Some(Value::Str(t)) if t == "free" => {
                        LeafResult::Done(Some("free model".into()))
                    }
                    _ => LeafResult::Done(Some("paid model".into())),
                }
            }),
        );
        let mut ix = Interpreter::new(tree).with_provider(
            &["model"],
            Box::new(StaticProvider {
                result: DataResult {
                    rows: vec![
                        Row::new("m-free").field("tier", Value::Str("free".into())),
                        Row::new("m-paid").field("tier", Value::Str("paid".into())),
                    ],
                    ..Default::default()
                },
            }),
        );
        let st = ix.state("model m-free");
        let out = c.on_enter(
            OverlayAction::Submit("model m-free".into()),
            &st,
            "model m-free",
        );
        assert_eq!(out, AcceptOutcome::Close(Some("free model".into())));
    }

    // --- ConfirmState unit ---

    #[test]
    fn confirm_state_set_get() {
        let mut s = ConfirmState::default();
        assert!(s.get("k").is_none());
        s.set("k", Value::Num(1.0));
        s.set("k", Value::Num(2.0)); // overwrite, no duplicate
        assert_eq!(s.get("k"), Some(&Value::Num(2.0)));
        assert_eq!(s.data.len(), 1);
    }
}
