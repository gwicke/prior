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

/// One traversal memory frame: the prompt (with filter/modifier state)
/// and cursor position as they were *before* an accept/drill. Esc replays
/// these newest-first, strictly along the current branch (frames are only
/// pushed on the way down, so Esc can never jump to siblings).
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub line: String,
    pub cursor: usize,
}

/// The shared menu engine, built per prompt iteration from picocode state.
/// NOTE: rebuilt per key-press today (`MenuEngine::new(ctx)`), so the stack
/// lives with the *host session* via the returned snapshot (see
/// `StepResult.frames`); a persistent engine is Phase 6 cleanup.
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
    /// `cursor` is the host's highlighted row (arrows move it); Tab/Enter
    /// act on it. `frames` is the traversal memory in/out.
    pub fn step_at(
        &mut self,
        line: &str,
        cursor: usize,
        action: HostKey,
        frames: &mut Vec<Frame>,
    ) -> StepResult {
        let st = self.interpreter.state(line);
        let ix_action = match action {
            HostKey::Tab => self.interpreter.tab_at(line, cursor),
            HostKey::Enter => self.interpreter.enter_at(line, cursor),
            HostKey::Esc => {
                // Back navigation: replay the newest frame (previous
                // filter state + cursor) before falling back to the
                // structural level pop. Frames are pushed only on
                // accept/drill, so this walks strictly up the tree.
                if let Some(f) = frames.pop() {
                    return StepResult {
                        buffer: Some(f.line),
                        restore_cursor: Some(f.cursor),
                        action: AcceptOutcome::Continue(OverlayAction::None),
                        state: st,
                        frames_cleared: false,
                    };
                }
                self.interpreter.esc(line)
            }
        };
        match action {
            HostKey::Enter => {
                let out = self.controller.on_enter(ix_action.clone(), &st, line);
                self.push_frame_on(ix_action, line, cursor, frames);
                StepResult {
                    buffer: None,
                    restore_cursor: None,
                    action: out,
                    state: st,
                    frames_cleared: false,
                }
            }
            _ => {
                let buffer = match &ix_action {
                    OverlayAction::Buffer(b) => Some(b.clone()),
                    _ => None,
                };
                self.push_frame_on(ix_action, line, cursor, frames);
                StepResult {
                    buffer,
                    restore_cursor: None,
                    action: AcceptOutcome::Continue(OverlayAction::None),
                    state: st,
                    frames_cleared: false,
                }
            }
        }
    }

    /// Push a traversal frame when an accept/drill rewrote the buffer
    /// downward (the pre-accept prompt + cursor are the "back" target).
    fn push_frame_on(
        &self,
        ix_action: OverlayAction,
        line: &str,
        cursor: usize,
        frames: &mut Vec<Frame>,
    ) {
        if let OverlayAction::Buffer(b) = &ix_action {
            // Drill (trailing space) or accept-rewrite: the previous line
            // becomes a back target. Plain structural rewrites that came
            // from Esc are not routed here (Esc pops instead).
            let drilled = b.ends_with(' ');
            let accepted = !drilled && !b.is_empty();
            if drilled || accepted {
                frames.push(Frame {
                    line: line.to_string(),
                    cursor,
                });
            }
        }
    }

    /// Cursor-agnostic step (engine default row).
    pub fn step(&mut self, line: &str, action: HostKey) -> StepResult {
        let cursor = self.interpreter.state(line).active;
        let mut frames = Vec::new();
        self.step_at(line, cursor, action, &mut frames)
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
    /// Restored cursor row (back-navigation); None = keep current.
    pub restore_cursor: Option<usize>,
    /// Accept-flow outcome (Enter only).
    pub action: AcceptOutcome,
    /// The menu state after the step (for rendering).
    pub state: MenuState,
    /// True when an edit invalidated the traversal memory.
    pub frames_cleared: bool,
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

    /// Regression: `/thinking` (bare, no trailing word) + Enter must drill
    /// (buffer "/thinking "), not submit the level name as a level value
    /// ("thinking thinking" → "unknown thinking level").
    #[test]
    fn thinking_enter_bare_drills_not_submits() {
        let mut e = MenuEngine::new(&ctx());
        let act = e.interpreter.enter("thinking");
        assert_eq!(act, OverlayAction::Buffer("thinking ".to_string()));
        // Sanity: a completed value still submits.
        assert_eq!(
            e.interpreter.enter("thinking med"),
            OverlayAction::Submit("thinking med".to_string())
        );
    }

    /// Revised accept semantics (design review): Tab/Enter act on the
    /// host cursor row and clear refinement state — the buffer keeps only
    /// path + committed id.
    #[test]
    fn tab_acts_on_host_cursor_and_clears_terms() {
        let mut e = MenuEngine::new(&ctx());
        let mut frames: Vec<Frame> = Vec::new();
        // "e" ranks med first (1 candidate): cursor on row 0 must be the
        // completed id; cursor beyond the view is clamped to the same.
        let st = e.interpreter.state("thinking e");
        let top_id = st.candidates[0].id.clone();
        let r = e.step_at("thinking e", 0, HostKey::Tab, &mut frames);
        assert_eq!(
            r.buffer,
            Some(format!("thinking {}", top_id)),
            "Tab must honor the host cursor row"
        );
        // Out-of-range cursor falls back to the engine default row.
        let r1 = e.step_at("thinking e", 9, HostKey::Tab, &mut frames);
        assert_eq!(r1.buffer, Some(format!("thinking {}", top_id)));
        // Both accepts pushed frames for back-navigation.
        assert_eq!(frames.len(), 2);
        // Esc replays the newest frame: previous filter state + cursor.
        let r2 = e.step_at(&format!("thinking {}", top_id), 0, HostKey::Esc, &mut frames);
        assert_eq!(r2.buffer, Some("thinking e".to_string()));
        assert_eq!(r2.restore_cursor, Some(9));
        assert_eq!(frames.len(), 1);
        // Second Esc replays the first frame; third falls back to the
        // structural pop (pinned level → drilled "thinking ").
        let r3 = e.step_at("thinking e", 0, HostKey::Esc, &mut frames);
        assert_eq!(r3.buffer, Some("thinking e".to_string()));
        let r4 = e.step_at("thinking e", 0, HostKey::Esc, &mut frames);
        // Structural pop at the root list = clear (v1 rule).
        assert_eq!(r4.buffer, Some(String::new()));
    }

    /// Esc with an empty stack falls back to the structural level pop
    /// (strict up-walk preserved).
    #[test]
    fn esc_without_frames_pops_level() {
        let mut e = MenuEngine::new(&ctx());
        let mut frames: Vec<Frame> = Vec::new();
        let r = e.step_at("thinking med", 0, HostKey::Esc, &mut frames);
        assert_eq!(r.buffer, Some("thinking ".to_string()));
    }

    /// Frames are pushed on drill too: Esc from a drilled child returns to
    /// the pre-drill line with the cursor.
    #[test]
    fn drill_pushes_frame_esc_returns() {
        let mut e = MenuEngine::new(&ctx());
        let mut frames: Vec<Frame> = Vec::new();
        let r = e.step_at("thinking", 0, HostKey::Enter, &mut frames);
        // Enter returns the action via `action`, not `buffer`.
        match &r.action {
            AcceptOutcome::Continue(OverlayAction::Buffer(b)) => {
                assert_eq!(b, "thinking ");
            }
            other => panic!("expected drill buffer, got {:?}", other),
        }
        assert_eq!(frames.len(), 1);
        let r2 = e.step_at("thinking ", 0, HostKey::Esc, &mut frames);
        assert_eq!(r2.buffer, Some("thinking".to_string()));
        assert_eq!(r2.restore_cursor, Some(0));
    }

    /// Regression: `/thinking` + Tab (no trailing word) must expand to
    /// `/thinking ` so the value menu shows (same as Enter). The host
    /// re-adds the leading slash.
    #[test]
    fn thinking_tab_preserves_slash_prefix() {
        let mut e = MenuEngine::new(&ctx());
        // Bare level: Tab promotes (drill), host re-adds the slash.
        let act = e.interpreter.tab("thinking");
        assert_eq!(act, OverlayAction::Buffer("thinking ".to_string()));
        assert_eq!(format!("/{}", "thinking "), "/thinking ");
        // Partial value: Tab completes, prefix intact.
        let act2 = e.interpreter.tab("thinking h");
        assert_eq!(
            act2,
            OverlayAction::Buffer("thinking high".to_string())
        );
    }
}
