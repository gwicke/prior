//! Bridge: picocode state → slashdot_menu interpreter (design §10).
//!
//! Each migrated command builds its descriptor subtree + handlers here;
//! `ActiveOverlay` arms delegate to the shared interpreter instead of
//! bespoke overlay variants. Phase 6 starts with `/thinking` (§10 step 1).

use crate::slash;
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
    /// Level identity: the node path this frame belongs to. Edit
    /// invalidation is scoped per level — typing at a deep branch does not
    /// discard the filter state of ancestor levels.
    pub level: Vec<String>,
}

/// Compute a level's path identity from a prompt line: the node path the
/// interpreter walks to (pinned level-name selections included).
fn level_of(engine: &mut MenuEngine, line: &str) -> Vec<String> {
    let st = engine.interpreter.state(line);
    let mut path = st.node_path.clone();
    // A pinned selection whose node is a level (has children/value) IS the
    // level name (e.g. "thinking" pinned at root = the thinking level).
    if let Some(sel) = &st.selection {
        if let Some(node) = resolve_node(&engine.interpreter.root, &path, sel) {
            if !node.children.is_empty() || node.value.is_some() {
                path.push(sel.clone());
            }
        }
    }
    path
}

fn resolve_node(
    root: &crate::slashdot_menu::node::MenuNode,
    path: &[String],
    sel: &str,
) -> Option<crate::slashdot_menu::node::MenuNode> {
    let mut node = root.clone();
    for p in path {
        node = node.child(p)?.clone();
    }
    node.child(sel).cloned()
}

/// The shared menu engine, built per prompt iteration from picocode state.
/// NOTE: rebuilt per key-press today (`MenuEngine::new(ctx)`), so the stack
/// lives with the *host session* via the returned snapshot (see
/// `StepResult.frames`); a persistent engine is Phase 6 cleanup.
pub struct MenuEngine {
    pub interpreter: Interpreter,
    pub controller: AcceptController,
}

/// DataProvider over the session tree entries (§10 step 6). The host
/// flattens the tree (v1 logic) and hands over the flat list; rows carry
/// id/label/indent/active-path/current so the engine ranks and pins.
pub struct TreeProvider {
    /// (indent, label, entry_id, on_active_path, is_current, branch_depth)
    /// — v1's flatten_tree_nodes tuple, passed through.
    pub entries: Vec<(usize, String, String, bool, bool, usize)>,
    pub leaf_id: Option<String>,
}

impl slashdot_menu::provider::DataProvider for TreeProvider {
    fn fetch(
        &mut self,
        _modifiers: &[slashdot_menu::grammar::ModifierToken],
    ) -> slashdot_menu::provider::DataResult {
        use slashdot_menu::value::Value;
        let rows: Vec<slashdot_menu::fields::Row> = self
            .entries
            .iter()
            .map(|(indent, label, id, on_path, is_current, branch_depth)| {
                let mut r = slashdot_menu::fields::Row::new(id.clone())
                    .desc(label.clone())
                    .field("label", Value::Str(label.clone()))
                    .field("indent", Value::Num(*indent as f64))
                    .field("branch_depth", Value::Num(*branch_depth as f64));
                if *on_path {
                    r = r.field("on_path", Value::Str("1".into()));
                }
                if *is_current {
                    r = r.field("leaf", Value::Str("1".into()));
                }
                r
            })
            .collect();
        slashdot_menu::provider::DataResult {
            rows,
            ..Default::default()
        }
    }
}

/// DataProvider over the session list (§10 step 5). Rows carry the
/// session metadata the action level renders in its per-row header.
pub struct SessionProvider {
    pub sessions: Vec<crate::repl::SessionMeta>,
    pub current_id: String,
}

impl slashdot_menu::provider::DataProvider for SessionProvider {
    fn fetch(
        &mut self,
        _modifiers: &[slashdot_menu::grammar::ModifierToken],
    ) -> slashdot_menu::provider::DataResult {
        use slashdot_menu::value::Value;
        let rows: Vec<slashdot_menu::fields::Row> = self
            .sessions
            .iter()
            .map(|m| {
                let display = m.name.clone().unwrap_or_else(|| m.id.clone());
                let mut r = slashdot_menu::fields::Row::new(m.id.clone())
                    .desc(&display)
                    .field("name", Value::Str(display))
                    .field("turns", Value::Num(m.user_message_count as f64))
                    .field("msgs", Value::Num(m.message_count as f64))
                    .field("cwd", Value::Str(m.cwd.clone()))
                    .field("timestamp", Value::Str(m.timestamp.clone()));
                if m.id == self.current_id {
                    r = r.field("current", Value::Str("1".into()));
                }
                if let Some(p) = &m.parent_session {
                    r = r.field("parent", Value::Str(p.clone()));
                }
                r
            })
            .collect();
        slashdot_menu::provider::DataResult {
            rows,
            ..Default::default()
        }
    }

    /// The current session row (`*` marker); `None` when no current id.
    fn active_selector(&self) -> Option<String> {
        if self.current_id.is_empty() {
            None
        } else {
            Some(self.current_id.clone())
        }
    }
}

/// DataProvider over the model catalog (§10 step 4). One-shot fetch:
/// applies the parsed modifier state to the repository filter and maps the
/// entries to rows with number fields the engine can sort/filter.
pub struct ModelProvider {
    pub filter: crate::repl::ModelFilter,
    /// Search terms extracted by the caller (engine fuzzy-ranks them).
    pub active_id: String,
}

impl slashdot_menu::provider::DataProvider for ModelProvider {
    /// The currently-active model row (`*` marker, design §5.1): the id
    /// handed in at build time, resolved against the candidate set.
    fn active_selector(&self) -> Option<String> {
        if self.active_id.is_empty() {
            None
        } else {
            Some(self.active_id.clone())
        }
    }

    fn fetch(
        &mut self,
        _modifiers: &[slashdot_menu::grammar::ModifierToken],
    ) -> slashdot_menu::provider::DataResult {
        let entries = crate::model_catalog::get_filtered_catalog(&self.filter.to_params());
        let rows: Vec<slashdot_menu::fields::Row> = entries
            .iter()
            .map(|e| {
                let mut r = slashdot_menu::fields::Row::new(e.qualified())
                    .desc(e.display.clone());
                if let Some(v) = e.context_length {
                    r = r.field("context", slashdot_menu::value::Value::Num(v as f64));
                }
                if let Some(v) = e.prompt_cost {
                    r = r.field("price_input", slashdot_menu::value::Value::Num(v));
                }
                if let Some(v) = e.completion_cost {
                    r = r.field("price_output", slashdot_menu::value::Value::Num(v));
                }
                // Combined price (computed): v1's get_filtered_catalog never
                // read ModelFilterParams.price, so `.price:X` filtered
                // nothing — surfaced here as a real field.
                if let (Some(p), Some(c)) = (e.prompt_cost, e.completion_cost) {
                    r = r.field("price", slashdot_menu::value::Value::Num(p + c));
                }
                if let Some(v) = e.coding_index {
                    r = r.field("coding", slashdot_menu::value::Value::Num(v as f64));
                }
                if let Some(v) = e.intelligence_index {
                    r = r.field("intelligence", slashdot_menu::value::Value::Num(v as f64));
                }
                if let Some(v) = e.agentic_index {
                    r = r.field("agentic", slashdot_menu::value::Value::Num(v as f64));
                }
                if let Some(t) = e.created {
                    r = r.field("created", slashdot_menu::value::Value::Num(t as f64));
                }
                r
            })
            .collect();
        slashdot_menu::provider::DataResult {
            rows,
            ..Default::default()
        }
    }
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

    /// Take the engine's sticky cursor map (Phase 6.5) so the host can
    /// carry it to the next frame's rebuilt engine.
    pub fn take_cursors(&mut self) -> slashdot_menu::interpreter::CursorMap {
        self.interpreter.take_cursors()
    }

    /// Restore a cursor map previously taken from a rebuilt-away engine
    /// (see [`Self::take_cursors`]).
    pub fn restore_cursors(&mut self, map: slashdot_menu::interpreter::CursorMap) {
        self.interpreter.restore_cursors(map);
    }

    /// Engine with the ModelProvider attached to the `model` dynamic level.
    /// The Tab/Enter arms must use this (not `new`) so the dynamic level
    /// resolves against the same merged-filter rows the display built.
    pub fn for_model(
        ctx: &crate::repl::ReplCtx,
        mf: crate::repl::ModelFilter,
        active_id: String,
    ) -> Self {
        let mut e = Self::new(ctx);
        e.interpreter = e.interpreter.with_provider(
            &["model"],
            Box::new(ModelProvider {
                filter: mf,
                active_id,
            }),
        );
        e
    }

    /// The static descriptor tree: one child per command (§10). Only the
    /// commands migrated so far are referenced by ActiveOverlay; the rest
    /// are descriptors for the root list (Phase 6 step 2 — done).
    fn root_descriptor(_ctx: &crate::repl::ReplCtx) -> MenuNode {
        // Root list from the static command registry (name → key without
        // the leading slash; help → description), plus the migrated
        // per-command sub-descriptors (§10 order; thinking first).
        // Leaf commands: help, quit, clear, new — no args, submit directly.
        let mut children: Vec<MenuNode> = crate::slash::registry()
            .iter()
            .map(|c| {
                let mut n = MenuNode::new(c.name.trim_start_matches('/')).desc(c.help);
                // Leaf commands: no args, direct submit.
                if matches!(c.kind, slash::SlashKind::Help
                    | slash::SlashKind::Quit
                    | slash::SlashKind::Clear
                    | slash::SlashKind::New) {
                    n = n.leaf();
                }
                n
            })
            .collect();
        // §10 step 1: /thinking — static level with one enum positional
        // value child (off|low|med|high), shared leaf = SetThinking.
        // §10 step 3: /persona — value child backed by the persona list
        // ("none" = empty value); /set — static children, each a level
        // whose value kind comes from SETTINGS metadata.
        for c in children.iter_mut() {
            match c.key.as_str() {
                "thinking" => {
                    *c = MenuNode::new("thinking")
                        .desc("set thinking level")
                        .value(crate::slashdot_menu::node::ValueChild::Enum(vec![
                            "off".into(),
                            "low".into(),
                            "med".into(),
                            "high".into(),
                        ]))
                        .leaf();
                }
                "persona" => {
                    let mut vals: Vec<String> = crate::persona::PERSONAS
                        .iter()
                        .map(|p| p.name.to_string())
                        .collect();
                    vals.push("none".into());
                    *c = MenuNode::new("persona")
                        .desc("set runtime persona")
                        .value(crate::slashdot_menu::node::ValueChild::Enum(vals))
                        .leaf();
                }
                "compact" => {
                    // §10 step 6/R8-8: leaf that returns a rewritten path —
                    // pure sugar over /tree (no parallel hierarchy).
                    *c = MenuNode::new("compact")
                        .desc("compact the session at the current position")
                        .leaf();
                }
                "stat" => {
                    // §10 step 8: interstitial, no leaf (D4) — second Enter
                    // dismisses; text lives in the node property.
                    *c = MenuNode::new("stat")
                        .desc("session stats: context window, usage, cost, cache hit")
                        .interstitial("context window, token usage, cost, cache hit rate");
                }
                "replay" => {
                    // Interstitial + leaf (D4): first Enter shows the help
                    // panel, next Enter invokes the replay.
                    *c = MenuNode::new("replay")
                        .desc("re-render session history")
                        .interstitial(
                            "replay: 10 = last 10 turns, 10t = turns, 10l = lines\n                             Examples: /replay  /replay 5  /replay 50l",
                        )
                        .leaf();
                }
                "sandbox" => {
                    // §10 step 7: static subcommand children (add_ro/add_rw
                    // with back-end fs modifier value completion; remove is
                    // a value child whose ids are the current bindings).
                    *c = MenuNode::new("sandbox")
                        .desc("manage session path bindings: add_ro | add_rw | remove")
                        .children(vec![
                            MenuNode::new("add_ro").desc("make path available read-only"),
                            MenuNode::new("add_rw").desc("make path available read-write"),
                            MenuNode::new("remove").desc("remove a path from the bindings"),
                        ]);
                }
                "tree" => {
                    // §10 step 6: dynamic level (tree entries) → shared
                    // static action level (switch/compact); compact modes
                    // are single-token enum children; llm leaf carries the
                    // `.prompt` modifier (free text) + confirm.
                    *c = MenuNode::new("tree")
                        .desc("navigate session history — select a point to continue from")
                        .dynamic()
                        .children(vec![
                            MenuNode::new("switch").desc("continue from this entry").leaf(),
                            MenuNode::new("compact").desc("compact at this entry").children(vec![
                                MenuNode::new("manual").desc("summarize the kept text yourself"),
                                MenuNode::new("text-only").desc("truncate to text, drop tool output"),
                                MenuNode::new("llm")
                                    .desc("LLM summary; .prompt:<text> sets the prompt")
                                    .confirm()
                                    .leaf(),
                            ]),
                        ]);
                }
                "session" => {
                    // §10 step 5: dynamic level (sessions) → static action
                    // children shared by all rows; delete is a confirm leaf.
                    *c = MenuNode::new("session")
                        .desc("list, rename, resume, fork, or delete sessions")
                        .dynamic()
                        .children(vec![
                            MenuNode::new("resume").desc("continue the session"),
                            MenuNode::new("fork").desc("branch from this point"),
                            MenuNode::new("rename").desc("rename the session"),
                            MenuNode::new("delete").desc("delete from disk").confirm().leaf(),
                        ]);
                }
                "model" => {
                    // §10 step 4: dynamic level over the catalog. Fields
                    // from MODEL_MODIFIERS semantics: numeric filters map
                    // to number fields; `sort` is the special-cased token
                    // in the engine's Query compiler.
                    *c = MenuNode::new("model")
                        .desc("select a model (provider:name); type . for filter options")
                        .dynamic()
                        .leaf();
                }
                "set" => {
                    let setting_children: Vec<MenuNode> = crate::settings::SETTINGS
                        .iter()
                        .map(|m| {
                            let n = MenuNode::new(m.name).desc(m.description);
                            match m.kind {
                                crate::settings::ValueKind::Enum(vals) => {
                                    n.value(crate::slashdot_menu::node::ValueChild::Enum(
                                        vals.iter().map(|v| v.to_string()).collect(),
                                    ))
                                }
                                crate::settings::ValueKind::Int => {
                                    n.value(crate::slashdot_menu::node::ValueChild::Number)
                                }
                                _ => n,
                            }
                        })
                        .collect();
                    *c = MenuNode::new("set").desc("view and edit settings").children(setting_children);
                }
                _ => {}
            }
        }
        MenuNode::new("root").children(children)
    }

    fn register_handlers(controller: &mut AcceptController) {
        // /replay: leaf → the host submits the line (agent.rs handles the
        // actual replay rendering). Signal via Done with the echo line.
        controller.register(
            &["replay"],
            Box::new(|inv: &LeafInvocation| LeafResult::Done(Some(inv.line.clone()))),
        );
        // /compact (R8-8): leaf returns the rewritten /tree path; the host
        // resolves the target id (needs tree ctx) via NewPath with the
        // sentinel "<last-user>" the host replaces.
        controller.register(
            &["compact"],
            Box::new(|_inv: &LeafInvocation| {
                LeafResult::NewPath("tree <last-user> compact ".to_string())
            }),
        );
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
            HostKey::Up | HostKey::Down => {
                // Phase 6.5: the engine owns the cursor; arrows mutate it.
                // A fresh derive after the move keeps `state` accurate.
                let delta = if action == HostKey::Up { -1 } else { 1 };
                self.interpreter.move_cursor(line, delta);
                self.interpreter.state(line);
                OverlayAction::None
            }
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
            HostKey::Esc => {
                // Esc already returned above (frame replay) or falls through
                // as the structural ladder pop. Esc never *pushes* frames
                // (it only walks up); applying the ladder's rewrite must not
                // create a new back-target.
                let buffer = match &ix_action {
                    OverlayAction::Buffer(b) => Some(b.clone()),
                    _ => None,
                };
                StepResult {
                    buffer,
                    restore_cursor: None,
                    action: AcceptOutcome::Continue(OverlayAction::None),
                    state: st,
                    frames_cleared: false,
                }
            }
            HostKey::Up | HostKey::Down => {
                // Pure cursor move: no buffer rewrite, no frame push.
                StepResult {
                    buffer: None,
                    restore_cursor: None,
                    action: AcceptOutcome::Continue(OverlayAction::None),
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
        &mut self,
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
                    level: level_of(self, line),
                });
            }
        }
    }

    /// Edit invalidation (design review, scoped): when the user edits the
    /// line, drop the traversal memory *of the current level only* — a
    /// deeply nested drill-down keeps its ancestors' filter state.
    ///
    /// Criterion: the edit is attributed to the level whose segment changed
    /// (`prev_line`'s level when the line had menu state). Removing *only*
    /// a trailing space is navigation (returning to the parent menu), not
    /// an edit — the level segment is unchanged, so nothing is cleared.
    pub fn on_edit(
        &mut self,
        prev_line: &str,
        line: &str,
        frames: &mut Vec<Frame>,
    ) -> bool {
        // Pure trailing-space removal: navigation, keep everything.
        if prev_line.trim_end() == line.trim_end() {
            return false;
        }
        // Attribute the edit to the level of the *previous* line (the one
        // the user was editing); clear that level's frames.
        let edited_level = level_of(self, prev_line);
        let before = frames.len();
        frames.retain(|f| &f.level != &edited_level);
        frames.len() != before
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
    /// Move the engine-owned view cursor up one row.
    Up,
    /// Move the engine-owned view cursor down one row.
    Down,
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
        // Frames exhausted → the structural ladder (design §5.1): clear
        // this level's terms keeping the list open, then strip the
        // trailing space, then clear the buffer at the top level.
        let r4 = e.step_at("thinking e", 0, HostKey::Esc, &mut frames);
        assert_eq!(r4.buffer, Some("thinking ".to_string()));
        let r5 = e.step_at("thinking ", 0, HostKey::Esc, &mut frames);
        assert_eq!(r5.buffer, Some("thinking".to_string()));
        let r6 = e.step_at("thinking", 0, HostKey::Esc, &mut frames);
        assert_eq!(r6.buffer, Some(String::new()));
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

    /// Root list (Phase 6 step 2): the command registry is the root
    /// descriptor; bare "/" lists all commands in registry order; a filter
    /// term ranks them; Tab/Enter drill into the pinned command.
    #[test]
    fn root_list_from_registry() {
        let mut e = MenuEngine::new(&ctx());
        let st = e.interpreter.state("");
        let ids: Vec<&str> = st.candidates.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"help"));
        assert!(ids.contains(&"thinking"));
        assert!(ids.contains(&"quit"));
        // Filter ranks by name+desc ("mo" → model).
        let st2 = e.interpreter.state("mo");
        assert!(st2.candidates.iter().any(|r| r.id == "model"));
        // Drill into a pinned command opens its level.
        let st3 = e.interpreter.state("thinking ");
        assert_eq!(st3.node_path, vec!["thinking"]);
        assert_eq!(
            st3.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["off", "low", "med", "high"]
        );
    }

    /// Persona level (Phase 6 step 3): PERSONAS + "none" as a value child.
    #[test]
    fn persona_level_from_registry() {
        let mut e = MenuEngine::new(&ctx());
        let st = e.interpreter.state("persona ");
        let ids: Vec<&str> = st.candidates.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"none"));
        assert!(!ids.is_empty());
        // Filter by prefix.
        let st2 = e.interpreter.state("persona none");
        assert_eq!(st2.selection.as_deref(), Some("none"));
        // Accept semantics: Tab pins path+id only.
        assert_eq!(
            e.interpreter.tab("persona none"),
            OverlayAction::Buffer("persona none".to_string())
        );
    }

    /// Settings levels (Phase 6 step 3): SETTINGS children exist with
    /// value children per ValueKind; pinned setting drills to its values.
    #[test]
    fn settings_levels_from_metadata() {
        let mut e = MenuEngine::new(&ctx());
        // Enum setting: security_mode.
        let st = e.interpreter.state("set security_mode ");
        assert_eq!(st.node_path, vec!["set", "security_mode"]);
        assert_eq!(
            st.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["yolo", "confirm"]
        );
        // Int setting: Number value child (no static candidates).
        let st2 = e.interpreter.state("set tool_call_limit ");
        assert_eq!(st2.node_path, vec!["set", "tool_call_limit"]);
        // FreeText setting: plain leaf (no value child).
        let st3 = e.interpreter.state("set llm_compact_prompt ");
        assert_eq!(st3.node_path, vec!["set", "llm_compact_prompt"]);
        // Filter at the settings level.
        let st4 = e.interpreter.state("set security");
        assert!(st4.candidates.iter().any(|r| r.id == "security_mode"));
    }

    /// Scoped edit invalidation: an ID edit at the current level clears
    /// that level's frames; deeper edits keep ancestor levels' state; and
    /// a pure trailing-space removal is navigation (clears nothing).
    #[test]
    fn on_edit_scoped_invalidation() {
        let mut e = MenuEngine::new(&ctx());
        let mut frames: Vec<Frame> = Vec::new();
        // Build depth: accept "thinking" (frame at root level), then drill.
        let _ = e.step_at("thinking", 0, HostKey::Tab, &mut frames);
        let _ = e.step_at("thinking ", 0, HostKey::Tab, &mut frames);
        // Two frames, both at the thinking level (value child has no deeper
        // static level): "thinking" (pre-accept) and "thinking " (pre-pin).
        assert_eq!(frames.len(), 2);
        // ID edit at the same level → both cleared (same level identity).
        assert!(e.on_edit("thinking off", "thinking low", &mut frames));
        assert!(frames.is_empty());
        // Trailing-space removal → navigation, nothing cleared.
        let _ = e.step_at("thinking", 0, HostKey::Tab, &mut frames);
        assert_eq!(frames.len(), 1);
        assert!(!e.on_edit("thinking ", "thinking", &mut frames));
        assert_eq!(frames.len(), 1);
    }

    /// Frames are pushed on drill too: Esc from a drilled child returns to
    /// the pre-drill line with the cursor.
    #[test]
    fn drill_pushes_frame_esc_returns() {
        let mut e = MenuEngine::new(&ctx());
        let mut frames: Vec<Frame> = Vec::new();
        // Host cursor decides: the buffer "/thinking" pins "thinking",
        // whose row index in the root list is 1 (help first).
        let r = e.step_at("thinking", 1, HostKey::Enter, &mut frames);
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
        assert_eq!(r2.restore_cursor, Some(1));
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
