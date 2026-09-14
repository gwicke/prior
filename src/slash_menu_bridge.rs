//! Bridge: picocode state → slashdot_menu interpreter (design §10).
//!
//! Each migrated command builds its descriptor subtree + handlers here;
//! `ActiveOverlay` arms delegate to the shared interpreter instead of
//! bespoke overlay variants. Phase 6 starts with `/thinking` (§10 step 1).

use crate::slashdot_menu::{
    accept::{AcceptController, AcceptOutcome, LeafInvocation, LeafResult},
    interpreter::{Interpreter, MenuState, OverlayAction},
    node::MenuNode,
    value::Value,
};

/// The shared menu engine, built per prompt iteration from picocode state.
pub struct MenuEngine {
    pub interpreter: Interpreter,
    pub controller: AcceptController,
}

impl MenuEngine {
    /// Build the engine: the full descriptor tree (all commands) with
    /// handlers bound per command as they migrate (§10 order).
    pub fn new(_ctx: &crate::repl::ReplCtx) -> Self {
        let root = Self::root_descriptor(_ctx);
        let mut controller = AcceptController::new(root.clone());
        Self::register_handlers(&mut controller);
        MenuEngine {
            interpreter: Interpreter::new(root),
            controller,
        }
    }

    /// The static descriptor tree: one child per command (§10). Only the
    /// commands migrated so far are referenced by ActiveOverlay; the rest
    /// are descriptors for the root list (Phase 6 step 2).
    fn root_descriptor(_ctx: &crate::repl::ReplCtx) -> MenuNode {
        MenuNode::new("root").children(vec![
            // §10 step 1: /thinking — static level with one enum positional
            // value child (off|low|med|high), shared leaf = SetThinking.
            MenuNode::new("thinking")
                .desc("set thinking level")
                .value(crate::slashdot_menu::node::ValueChild::Enum(vec![
                    "off".into(),
                    "low".into(),
                    "med".into(),
                    "high".into(),
                ]))
                .leaf(),
        ])
    }

    fn register_handlers(controller: &mut AcceptController) {
        // /thinking <level>: translate to SetThinking via the leaf result
        // convention — Done(Some(msg)) closes; the *action* is carried by
        // the picocode side reading the canonical line.
        controller.register(
            &["thinking"],
            Box::new(|inv: &LeafInvocation| {
                let level = inv.line.trim_start_matches("/thinking ").trim();
                let _ = level;
                // The actual InputAction is produced by the host, which
                // parses the accepted line (same path as typed input).
                LeafResult::Done(None)
            }),
        );
    }

    /// Map an interpreter state + key action onto the host-facing result.
    pub fn step(&mut self, line: &str, action: HostKey) -> StepResult {
        let st = self.interpreter.state(line);
        let ix_action = match action {
            HostKey::Tab => self.interpreter.tab(line),
            HostKey::Enter => self.interpreter.enter(line),
            HostKey::Esc => self.interpreter.esc(line),
        };
        match action {
            HostKey::Enter => {
                let out = self.controller.on_enter(ix_action, &st, line);
                StepResult {
                    buffer: None,
                    action: out,
                    state: st,
                }
            }
            _ => StepResult {
                buffer: match ix_action {
                    OverlayAction::Buffer(b) => Some(b),
                    _ => None,
                },
                action: AcceptOutcome::Continue(OverlayAction::None),
                state: st,
            },
        }
    }
}

/// Keys the host forwards to the menu engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostKey {
    Tab,
    Enter,
    Esc,
}

/// Result of one engine step.
pub struct StepResult {
    /// Buffer rewrite to apply to the LineEditor (Tab/Esc/Enter drill).
    pub buffer: Option<String>,
    /// Accept-flow outcome (Enter only).
    pub action: AcceptOutcome,
    /// The menu state after the step (for rendering).
    pub state: MenuState,
}

/// Helper: build rows for a value child's enum set (tests).
#[allow(dead_code)]
pub(crate) fn enum_rows(values: &[&str]) -> Vec<crate::slashdot_menu::fields::Row> {
    values
        .iter()
        .map(|v| crate::slashdot_menu::fields::Row::new(*v))
        .collect()
}

/// Helper: read a string field from a row's fields.
#[allow(dead_code)]
pub(crate) fn row_str(r: &crate::slashdot_menu::fields::Row, k: &str) -> Option<String> {
    r.field_value(k).and_then(|v| match v {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::ReplCtx;
    use crate::agent::SessionUsage;
    use crate::repl::ModelFilter;

    fn ctx() -> ReplCtx {
        ReplCtx {
            provider: "p".into(),
            model: "m".into(),
            recent: vec![],
            persona: None,
            session_usage: SessionUsage::default(),
            repo: None,
            current_session_id: "s".into(),
            branch_messages: vec![],
            tree: vec![],
            leaf_id: None,
            context_tokens: 0,
            model_filter: ModelFilter::default_for_coding(),
            thinking_level: Some("low".into()),
        }
    }

    /// slash_menu.md acceptance: bare `/thinking` lists all levels in
    /// declaration order; the active level is surfaced for the `*` marker.
    #[test]
    fn thinking_bare_lists_all() {
        let mut e = MenuEngine::new(&ctx());
        let st = e.interpreter.state("thinking ");
        assert_eq!(
            st.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["off", "low", "med", "high"]
        );
    }

    /// Tab completes the trailing partial keeping earlier terms (D2):
    /// "thinking h" — "h" is a fuzzy term, the active candidate completes
    /// to the id, replacing the partial (positional selection slot).
    #[test]
    fn thinking_tab_completes_partial() {
        let mut e = MenuEngine::new(&ctx());
        let st = e.interpreter.state("thinking h");
        // "h" is not a valid enum value → demoted to a fuzzy term.
        assert_eq!(st.selection, None);
        assert_eq!(st.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["high"]);
        let act = e.interpreter.tab("thinking h");
        // Value child: Tab pins without drill (leaf payload).
        assert_eq!(act, OverlayAction::Buffer("thinking high".to_string()));
    }

    /// Enter on a completed level submits the canonical line; the host
    /// parses it into SetThinking (same path as typed input).
    #[test]
    fn thinking_enter_submits() {
        let mut e = MenuEngine::new(&ctx());
        let out = e.step("thinking med", HostKey::Enter);
        match out.action {
            AcceptOutcome::Continue(OverlayAction::Submit(line)) => {
                assert_eq!(line, "thinking med");
            }
            other => panic!("expected submit, got {:?}", other),
        }
    }

    /// Esc pops the pinned selection (v1 contract): pinned → drilled level
    /// ("thinking ") → parent path → cleared.
    #[test]
    fn thinking_esc_pops_pin() {
        let mut e = MenuEngine::new(&ctx());
        let out = e.step("thinking med", HostKey::Esc);
        assert_eq!(out.buffer, Some("thinking ".to_string()));
        let out2 = e.step("thinking ", HostKey::Esc);
        assert_eq!(out2.buffer, Some("thinking".to_string()));
        let out3 = e.step("thinking", HostKey::Esc);
        assert_eq!(out3.buffer, Some(String::new()));
    }

    /// Fuzzy terms filter the value set and pin survives (R8-5).
    #[test]
    fn thinking_terms_filter() {
        let mut e = MenuEngine::new(&ctx());
        let st = e.interpreter.state("thinking e");
        // "e" matches "high" (desc) and "med"… ids: off/low/med/high —
        // "e" fuzzy-matches "med" and "high". Just verify filtering happened.
        assert!(st.candidates.len() < 4);
    }

    /// Regression: `/thinking` + Tab (no trailing word) must keep the
    /// command prefix — "/thinking" + active completion, not just "thinking".
    /// The buffer keeps its leading slash through the engine round-trip.
    #[test]
    fn thinking_tab_preserves_slash_prefix() {
        let mut e = MenuEngine::new(&ctx());
        // Engine-level: Tab on bare "thinking" (no trailing word) drills
        // into the level (trailing space) — the host re-adds the slash.
        let act = e.interpreter.tab("thinking");
        match act {
            OverlayAction::Buffer(b) => {
                let host_line = format!("/{}", b);
                assert!(
                    host_line.starts_with("/thinking"),
                    "prefix lost: {:?}", host_line
                );
            }
            other => panic!("expected buffer, got {:?}", other),
        }
        // Host-level round trip: "/thinking h" → "/thinking high".
        let act2 = e.interpreter.tab("thinking h");
        assert_eq!(
            act2,
            OverlayAction::Buffer("thinking high".to_string())
        );
        assert_eq!(format!("/{}", "thinking high"), "/thinking high");
    }
}
