//! Static-level interpreter (design §3–§5, Phase 3 subset).
//!
//! Derives the current menu state from a prompt buffer and a static node
//! tree, and produces the Tab/Enter/Esc results. Cursor lives in the path:
//! the trailing selection token IS the selection; a trailing space is the
//! drill-in signal.

use crate::grammar::{parse_level, SelectionRule};
use crate::node::MenuNode;
use crate::fields::Row;
use crate::provider::{DataResult, DataProvider};

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
    /// The displayed level is a dynamic (data-provider) level.
    pub level_is_dynamic: bool,
    /// The row that is *currently in effect* (`*` marker, design §5.1),
    /// from the provider's `active_selector` (or enum/static analogue),
    /// resolved against `candidates`. `None` when no row is active (root
    /// command list, or the selector didn't resolve). Distinct from
    /// `active` (the cursor row), though a fresh cursor seeds on it.
    pub active_row_marker: Option<usize>,
}

impl MenuState {
    pub fn selected(&self) -> Option<&Row> {
        self.candidates.get(self.active)
    }
}

/// Sticky per-level view-cursor state (Phase 6.5): path → (index, pin at
/// set time, terms at set time). Typed input that changes the pin or terms
/// resets the cursor to the derived pin.
pub type CursorMap = std::collections::HashMap<String, (usize, Option<String>, String)>;

/// Opaque per-activation state bundle (Phase 6.7 step 1). Across menu
/// frames the *engine* is rebuilt while this bundle survives with the host;
/// it carries everything the engine knows that is *not* derived from the
/// prompt line: the sticky per-level cursor map and the one-shot provider
/// data cache (design §8 — fetched once per menu open; persisting it here
/// stops re-fetching dynamic rows on every keypress). Traversal frames live
/// with the bridge/host today and join this bundle as part of the same
/// consolidation.
///
/// The host must treat this as opaque: it takes it before dropping the
/// engine and restores it into the next engine via
/// [`Interpreter::take_activation`]/[`Interpreter::restore_activation`] and
/// never inspects its fields.
pub struct Activation {
    pub(crate) cursors: CursorMap,
    pub(crate) data_cache: std::collections::HashMap<String, DataResult>,
}

impl Clone for Activation {
    fn clone(&self) -> Self {
        Activation {
            cursors: self.cursors.clone(),
            data_cache: self.data_cache.clone(),
        }
    }
}

impl Activation {
    /// Cursor-map read for the host's display path, which still builds the
    /// overlay from a cursor map until Phase 7 collapses those arms. The
    /// bundle itself is opaque; this is the lone deliberate escape for the
    /// not-yet-migrated display readers (which only need the cursor map).
    pub fn cursors(&self) -> &CursorMap {
        &self.cursors
    }
}

impl Default for Activation {
    fn default() -> Self {
        Activation {
            cursors: Default::default(),
            data_cache: Default::default(),
        }
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
    /// Interstitial (D4): show plain text; the framework keeps the menu
    /// state. Next Enter dismisses (no leaf) or invokes the leaf.
    Interstitial {
        text: String,
        /// Canonical line at the moment of the interstitial.
        line: String,
    },
    /// Editor handoff (§9): the leaf handler asked for an `$EDITOR` pass.
    /// Cancellation re-enters the prompt via `pending_prefill`.
    Editor {
        line: String,
    },
    /// Leaf returned a new path (R8-8): rewrite the buffer to another menu
    /// state (e.g. `/compact` → `/tree <id> compact `).
    Rewrite(String),
}

/// Internal Tab outcome: plain buffer rewrite or buffer + drill (promote).
enum TabResult {
    Buffer(String),
    BufferDrill(String),
    None,
}

/// Interpreter over a static node tree.
pub struct Interpreter {
    pub root: MenuNode,
    /// Cached one-shot data per dynamic node path (design §8: fetched once
    /// per menu open). Key = node path joined with spaces.
    data_cache: std::collections::HashMap<String, DataResult>,
    /// Node paths whose dynamic node needs a provider to resolve rows.
    dynamic_paths: std::collections::HashMap<String, Box<dyn DataProvider>>,
    /// Engine-owned view cursor (Phase 6.5): sticky per displayed level,
    /// keyed by node path → (active index, pin at the time it was set).
    /// The stored index survives buffer re-derives (arrow keys move it;
    /// typing rewrites it) until the pin or terms change — then it resets
    /// to the derived pin.
    cursors: CursorMap,
}

impl Interpreter {
    pub fn new(root: MenuNode) -> Self {
        Interpreter {
            root,
            data_cache: Default::default(),
            dynamic_paths: Default::default(),
            cursors: Default::default(),
        }
    }

    /// Register a data provider for the node at `path` (e.g. `["model"]`).
    /// The node must be marked dynamic via [`MenuNode::dynamic`].
    pub fn with_provider(mut self, path: &[&str], provider: Box<dyn DataProvider>) -> Self {
        self.dynamic_paths
            .insert(path.join(" "), provider);
        self
    }

    /// Field space for a dynamic level, derived from cached provider
    /// metadata (FieldMeta) or inferred from row types (§8.1).
    fn field_space_for(&mut self, path_key: &str) -> Option<crate::fields::FieldSpace> {
        let result = self.data_cache.get(path_key)?;
        if result.fields.is_empty() {
            return None; // no explicit metadata → fall through to inference
        }
        let fields = result
            .fields
            .iter()
            .map(|m| m.spec.clone())
            .collect();
        Some(crate::fields::FieldSpace::new(fields))
    }

    /// Field space for a level: explicit metadata when provided, else
    /// inferred from the cached rows (types by first non-missing value).
    fn field_space(&mut self, path_key: &str, rows: &[Row]) -> crate::fields::FieldSpace {
        if let Some(space) = self.field_space_for(path_key) {
            return space;
        }
        // Infer: a field is a number when its first present value is Num.
        let mut names: Vec<String> = Vec::new();
        for r in rows {
            for (k, _) in &r.fields {
                if !names.contains(k) {
                    names.push(k.clone());
                }
            }
        }
        let specs = names
            .iter()
            .map(|k| {
                let is_num = rows
                    .iter()
                    .find_map(|r| r.field_value(k))
                    .map(|v| matches!(v, crate::value::Value::Num(_)))
                    .unwrap_or(false);
                crate::fields::FieldSpec::new(k.clone(), is_num)
            })
            .collect();
        crate::fields::FieldSpace::new(specs)
    }

    /// Resolve rows for a dynamic node, using the cache when populated.
    fn dynamic_rows(
        &mut self,
        path_key: &str,
        modifiers: &[crate::grammar::ModifierToken],
    ) -> Option<DataResult> {
        if let Some(cached) = self.data_cache.get(path_key) {
            return Some(cached.clone());
        }
        let provider = self.dynamic_paths.get_mut(path_key)?;
        let result = provider.fetch(modifiers);
        self.data_cache.insert(path_key.to_string(), result.clone());
        Some(result)
    }

    /// Candidate rows for a node's child menu: static children become rows
    /// (id = key, description); a value child becomes its value set rows;
    /// a dynamic node resolves via its registered provider (§8).
    fn level_rows(
        &mut self,
        path: &[String],
        node: &MenuNode,
        modifiers: &[crate::grammar::ModifierToken],
    ) -> Vec<Row> {
        if node.dynamic {
            let path_key = path.join(" ");
            if let Some(result) = self.dynamic_rows(&path_key, modifiers) {
                return result.rows;
            }
            return Vec::new();
        }
        Self::child_rows(node)
    }

    /// Static/value child rows (no provider involved).
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

/// Resolve the provider's `active_selector` against the candidate `rows`
/// (design §5.1). Uses an immutable borrow of `dynamic_paths` so it can be
/// called while `dynamic_rows` caching holds a mutable borrow elsewhere.
fn resolve_active_selector(
    path_key: &str,
    providers: &std::collections::HashMap<String, Box<dyn DataProvider>>,
    rows: &[Row],
) -> Option<usize> {
    let id = providers.get(path_key)?.active_selector()?;
    rows.iter().position(|r| r.id == id)
}

/// Does `word` name a selection at `node`'s level? Exact key match for
    /// static children; permissive for value-typed children (any token).
    fn is_selection_at(node: &MenuNode) -> SelectionRule {
        if node.dynamic {
            // Dynamic level: a trailing word is a selection candidate if it
            // is space-free and non-dotted; *validity* (cheap id lookup, D2)
            // happens in the cursor against the cached rows — the grammar
            // accepts it permissively so parse-level is row-independent.
            return SelectionRule {
                is_selection: Some(std::sync::Arc::new(|word: &str| {
                    !word.starts_with('.')
                })),
            };
        }
        // static: unchanged
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
    pub fn state(&mut self, line: &str) -> MenuState {
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

        // Dynamic level demotion (D2): the grammar accepts any trailing
        // word permissively; a token that matches no candidate id (cached
        // dynamic rows or an enum value set) is a fuzzy term, not a pin.
        // Validity is always checked against the candidate set (R8-5).
        let all_rows_probe = self.level_rows(&path, &node, &lvl.modifiers);
        let lvl = match &lvl.selection {
            Some(sel) if !all_rows_probe.iter().any(|r| &r.id == sel) => {
                let mut terms = lvl.terms.clone();
                if !terms.is_empty() {
                    terms.push(' ');
                }
                terms.push_str(sel);
                crate::grammar::LevelTokens {
                    terms,
                    selection: None,
                    ..lvl
                }
            }
            _ => lvl,
        };

        // Trailing space after a selection = drill in (design §3): a static
        // child becomes the next node level; a dynamic row id drills into
        // the row's shared static action level.
        if let (Some(sel), true) = (&lvl.selection, trailing_space) {
            if let Some(child) = node.child(sel).cloned() {
                node = child;
                path.push(sel.clone());
                let rows = self.level_rows(&path, &node, &[]);
                return self.drilled_state(path, rows, node.dynamic);
            }
            if node.dynamic {
                // Dynamic row drilled: the shared action level applies to
                // all rows (design §2). Show the action children (if any)
                // as a static level below the row id.
                path.push(sel.clone());
                let rows = Self::child_rows(&node);
                return self.drilled_state(path, rows, false);
            }
        }

        let selection = lvl.selection.clone();
        // Front-end filter/sort over cached rows (§7.1) on dynamic levels.
        let all_rows_final: Vec<Row> = if node.dynamic && !lvl.modifiers.is_empty() {
            let path_key = path.join(" ");
            let field_space = self.field_space(&path_key, &all_rows_probe);
            let q = crate::filter::compile_query(&lvl.modifiers, &field_space);
            crate::filter::apply_query(&all_rows_probe, &q, &field_space)
        } else {
            all_rows_probe
        };
        let rank = crate::cursor::rank_and_pin(&all_rows_final, &lvl.terms, selection.as_deref());

        // Engine-owned cursor (Phase 6.5): a sticky per-level index set by
        // Up/Down survives buffer re-derives. It resets to the derived pin
        // when the pin or terms change (typed input re-targets the cursor).
        let path_key = path.join(" ");
        let stored = self
            .cursors
            .get(&path_key)
            .cloned();
        // `*` row (design §5.1): resolve the provider's active selector
        // against the ranked candidates. Dynamic levels use the provider;
        // enum/static value levels resolve a pre-pinned selector below.
        let active_row_marker = if node.dynamic {
            Self::resolve_active_selector(
                &path_key,
                &self.dynamic_paths,
                &rank.rows,
            )
        } else {
            // Static/level: a pinned selection naming the active value (or
            // an enum set carrying an active selector) — see §5.1. For
            // listing levels with no explicit selector, None.
            selection
                .as_ref()
                .and_then(|sel| rank.rows.iter().position(|r| &r.id == sel))
                .filter(|_| node.value.is_some())
        };

        let active = match stored {
            // Sticky cursor present → keep it (clamped).
            Some((idx, prev_sel, prev_terms))
                if prev_sel == selection && prev_terms == lvl.terms =>
            {
                idx.min(rank.rows.len().saturating_sub(1))
            }
            // Fresh level → seed the cursor on the active row if any,
            // else the derived pin.
            _ => active_row_marker.unwrap_or(rank.active),
        };
        self.cursors.insert(
            path_key,
            (active, selection.clone(), lvl.terms.clone()),
        );

        MenuState {
            node_path: path,
            terms: lvl.terms,
            selection,
            drill_in: trailing_space && lvl.selection.is_some(),
            candidates: rank.rows,
            active,
            pin_lost: !rank.pinned_survived && lvl.selection.is_some(),
            level_is_dynamic: node.dynamic,
            active_row_marker,
        }
    }

    /// Build the MenuState for a drilled level (pinned selection +
    /// trailing space), resolving the engine-owned sticky cursor (Phase
    /// 6.5) instead of a hard-coded row 0.
    fn drilled_state(
        &mut self,
        path: Vec<String>,
        rows: Vec<Row>,
        dynamic: bool,
    ) -> MenuState {
        let path_key = path.join(" ");
        // `*` marker (design §5.1) on the drilled level, from the provider
        // (dynamic) — the current-state row the cursor seeds on when fresh.
        let active_row_marker = if dynamic {
            Self::resolve_active_selector(&path_key, &self.dynamic_paths, &rows)
        } else {
            None
        };
        let stored = self.cursors.get(&path_key).cloned();
        let active = match stored {
            Some((idx, _, _)) => idx.min(rows.len().saturating_sub(1)),
            None => active_row_marker.unwrap_or(0),
        };
        self.cursors
            .insert(path_key, (active, None, String::new()));
        MenuState {
            node_path: path,
            terms: String::new(),
            selection: None,
            drill_in: true,
            candidates: rows,
            active,
            pin_lost: false,
            level_is_dynamic: dynamic,
            active_row_marker,
        }
    }

    /// Move the engine-owned view cursor by `delta` (clamped to the
    /// candidate list). Returns the new active index.
    pub fn move_cursor(&mut self, line: &str, delta: isize) -> usize {
        let st = self.state(line);
        let len = st.candidates.len();
        if len == 0 {
            return 0;
        }
        let next = (st.active as isize + delta).clamp(0, len as isize - 1) as usize;
        self.cursors.insert(
            st.node_path.join(" "),
            (next, st.selection.clone(), st.terms.clone()),
        );
        next
    }

    /// Open-time cursor seeding (e.g. `/tree` starts on the current leaf):
    /// force the sticky cursor for the displayed level to `idx`.
    /// Take the engine's non-prompt-derived per-activation state (Phase
    /// 6.7 step 1): cursor map + provider data cache. The host carries this
    /// between rebuilt engines and restores it on the next frame.
    pub fn take_activation(&mut self) -> Activation {
        Activation {
            cursors: std::mem::take(&mut self.cursors),
            data_cache: std::mem::take(&mut self.data_cache),
        }
    }

    /// Restore previously taken per-activation state (see
    /// [`Self::take_activation`]).
    pub fn restore_activation(&mut self, act: Activation) {
        self.cursors = act.cursors;
        self.data_cache = act.data_cache;
    }

    /// Take the sticky cursor map only (legacy Phase 6.5 bridge API; Phase
    /// 6.7 consolidates this into [`Self::take_activation`]).
    pub fn take_cursors(&mut self) -> CursorMap {
        std::mem::take(&mut self.cursors)
    }

    /// Restore a previously taken cursor map (legacy Phase 6.5 bridge API).
    pub fn restore_cursors(&mut self, map: CursorMap) {
        self.cursors = map;
    }

    pub fn set_cursor(&mut self, line: &str, idx: usize) -> usize {
        let st = self.state(line);
        let len = st.candidates.len();
        let next = idx.min(len.saturating_sub(1));
        self.cursors.insert(
            st.node_path.join(" "),
            (next, st.selection.clone(), st.terms.clone()),
        );
        next
    }

    /// Tab: complete the active candidate into the buffer.
    ///
    /// Completing a selection adds or replaces the id token, leaving fuzzy
    /// terms in place (D2). If the completed node has a child menu, Tab
    /// leaves the buffer pinned (no trailing space; drill is explicit via
    /// Enter or space).
    /// Tab: complete the active candidate into the buffer.
    ///
    /// Completing a selection adds or replaces the trailing word, keeping
    /// earlier fuzzy terms (D2). When the completed node is a *listing*
    /// (has children or a value child), Tab promotes exactly like Enter:
    /// a trailing space is appended so the child menu shows.
    pub fn tab(&mut self, line: &str) -> OverlayAction {
        let active = self.state(line).active;
        self.tab_at(line, active)
    }

    /// Tab acting on the host cursor row (`cursor` clamped to the view);
    /// the no-cursor variant uses the engine's default (pinned/active).
    pub fn tab_at(&mut self, line: &str, cursor: usize) -> OverlayAction {
        match self.tab_inner(line, Some(cursor)) {
            TabResult::Buffer(b) => OverlayAction::Buffer(b),
            TabResult::BufferDrill(b) => OverlayAction::Buffer(format!("{} ", b)),
            TabResult::None => OverlayAction::None,
        }
    }

    /// Core of Tab: returns the rewritten buffer, optionally flagged as a
    /// promotion (drill with trailing space).
    /// Does the node the current selection points at have a child menu
    /// (static children or a value child)? Such nodes promote on Tab when
    /// the level is otherwise empty.
    fn tab_promotes(root: &MenuNode, st: &MenuState) -> bool {
        // Resolve the node the selection lands on: walk node_path, then
        // the pinned selection if present.
        let mut node = root.clone();
        for p in &st.node_path {
            if let Some(c) = node.child(p) {
                node = c.clone();
            }
        }
        if let Some(sel) = &st.selection {
            if let Some(c) = node.child(sel) {
                node = c.clone();
            } else {
                return false; // pinned id is a dynamic row or value: no child menu
            }
        }
        !node.children.is_empty() || node.value.is_some() || node.dynamic
    }

    fn tab_inner(&mut self, line: &str, cursor: Option<usize>) -> TabResult {
        let mut st = self.state(line);
        // Host cursor wins: Tab/Enter act on the row the user highlighted,
        // not the engine's internal active default.
        if let Some(c) = cursor {
            if st.candidates.get(c).is_some() {
                st.active = c;
            }
        }
        let Some(sel) = st.selected() else {
            return TabResult::None;
        };
        let token = sel.id.clone();
        let trailing_space = line.ends_with(' ');

        let mut parts: Vec<String> = st.node_path.clone();
        if trailing_space {
            // Pinned/drilled level: complete the active candidate of the
            // displayed level as a pinned selection.
            parts.push(token.clone());
        } else if st.terms.is_empty() && Self::tab_promotes(&self.root, &st) {
            // Bare level with nothing typed at the child level (the pinned
            // word is the level name itself, e.g. "/thinking" on a root
            // child): Tab promotes exactly like Enter — trailing space so
            // the child/value menu shows. The full line is node_path +
            // the selected id (host cursor row when nothing is pinned).
            let mut full = parts.clone();
            full.push(token);
            return TabResult::BufferDrill(full.join(" "));
        } else {
            // Trailing word handling:
            // - Static level: the last word is the selection-in-progress
            //   slot — replace it (e.g. "think" → "thinking").
            // - Dynamic level (D2): Tab *adds* the id, leaving fuzzy terms
            //   in place ("model flag" → "model flag model-a"); an already
            //   valid pinned id is replaced (cursoring).
            // Accept semantics (v2 revised): the prompt holds the canonical
            // path + the committed id only — fuzzy terms/modifiers are
            // *refinement* state, cleared on accept. Back-navigation
            // restores them via the traversal stack (MenuEngine frames).
            parts.push(token);
        }
        let buf = parts.join(" ");

        // Promotion rule: the completed node is a listing (children, a
        // value child, or a dynamic provider level) ⇒ drill (trailing
        // space). Otherwise plain pin. Note: parts currently ends with the
        // completed token; resolve its node by walking the root through
        // `parts`.
        let mut node = self.root.clone();
        for p in &parts {
            if let Some(c) = node.child(p) {
                node = c.clone();
            }
        }
        // If the walk resolved the completed token as a static child, the
        // landed node is that child (its key == token). For a dynamic row
        // id the walk stops at the dynamic level itself — its key is the
        // level name, not the row id. Only a static-child completion drills.
        let completed = parts.last().map(String::as_str).unwrap_or("");
        let landed_on_child = node.key == completed
            && parts.len() == st.node_path.len() + 1;
        if !node.children.is_empty()
            || node.value.is_some()
            || (node.dynamic && landed_on_child)
        {
            // Value children: Tab pins the value, but the *listing* itself
            // was already the level; completing to a value just pins.
            if node.value.is_some() {
                TabResult::Buffer(buf)
            } else {
                TabResult::BufferDrill(buf)
            }
        } else {
            TabResult::Buffer(buf)
        }
    }

    /// Enter: accept.
    ///
    /// 1. If a selection is pinned with a trailing space and the node has a
    ///    child menu → already drilled; Enter on the drilled level commits
    ///    the active leaf or drills further.
    /// 2. If the pinned node is a leaf → submit the canonical line.
    /// 3. Otherwise Enter behaves like Tab + trailing space (drill).
    pub fn enter(&mut self, line: &str) -> OverlayAction {
        let active = self.state(line).active;
        self.enter_at(line, active)
    }

    /// Enter acting on the host cursor row (see [`Interpreter::tab_at`]).
    pub fn enter_at(&mut self, line: &str, cursor: usize) -> OverlayAction {
        // Enter = Tab + trailing space for structural promotion; Tab already
        // promotes listings (same as Enter), so delegate and reuse logic.
        match self.tab_at(line, cursor) {
            OverlayAction::Buffer(b) if !line.ends_with(' ') && !b.ends_with(' ') => {
                // Tab produced a pin (leaf or value child) — Enter adds the
                // drill space to show the next level (per contract, Enter
                // on a non-leaf always drills).
                let st = self.state(line);
                // If the completed node is a leaf → submit canonical line.
                let mut node = self.root.clone();
                let parts: Vec<&str> = b.trim_end().split(' ').collect();
                for p in &parts {
                    if let Some(c) = node.child(p) {
                        node = c.clone();
                    }
                }
                // A dynamic row's leaf-ness is the dynamic node's leaf flag.
                if (node.is_leaf || node.dynamic) && !parts.is_empty() {
                    return OverlayAction::Submit(b.to_string());
                }
                // Non-leaf pinned (e.g. value child) → Enter pins; a second
                // Enter (drilled) submits. Match prior behavior: pin only.
                if st.drill_in {
                    return OverlayAction::Buffer(b);
                }
                OverlayAction::Buffer(format!("{} ", b))
            }
            OverlayAction::Buffer(b) if line.ends_with(' ') && !b.ends_with(' ') => {
                // Drilled level pinned a row (Tab completed the id). If the
                // drilled level is dynamic and leaf-marked, the pin IS the
                // commit: submit the canonical line (design §5 — Enter on a
                // drilled leaf submits).
                let mut node = self.root.clone();
                for p in b.split(' ').filter(|p| !p.is_empty()) {
                    if let Some(c) = node.child(p).cloned() {
                        node = c;
                    }
                }
                if node.dynamic && node.is_leaf {
                    return OverlayAction::Submit(b);
                }
                OverlayAction::Buffer(b)
            }
            other => other,
        }
    }

    /// Esc: pop one level — rewrite the buffer to the parent state.
    /// Esc at the root clears the buffer (v1 rule).
    /// Esc: per-level back-navigation ladder (design §5.1). Each Esc press
    /// advances one step; traversal frames are replayed by the caller
    /// (`step_at` HostKey::Esc) *before* this is reached, so frames are not
    /// consulted here:
    ///   1. strip a trailing space (drill-out one level);
    ///   2. (— frame replay — handled by the caller);
    ///   3. clear this level's fuzzy terms/modifiers, keeping the level's
    ///      list visible (path + trailing space) — e.g. `/model gpt4` →
    ///      `/model `;
    ///   4. clear the level's pinned selection, leaving a trailing space so
    ///      the parent list stays visible;
    ///   5. at the top level, clear the whole buffer.
    pub fn esc(&mut self, line: &str) -> OverlayAction {
        let st = self.state(line);
        // Step 1: strip a trailing space (drill-out).
        if line.ends_with(' ') {
            return OverlayAction::Buffer(line.trim_end().to_string());
        }
        // Step 3: a listing level with refinement (terms/modifiers) but no
        // pinned selection → clear the refinement, keep the list open.
        if !st.node_path.is_empty() && st.selection.is_none() {
            return OverlayAction::Buffer(format!("{} ", st.node_path.join(" ")));
        }
        // Step 4: clear a pinned selection; leave a trailing space when a
        // parent level exists so that parent's list stays shown.
        if st.selection.is_some() {
            let mut parent = st.node_path.join(" ");
            if !parent.is_empty() {
                parent.push(' ');
            }
            return OverlayAction::Buffer(parent);
        }
        // Step 5: top level → clear.
        OverlayAction::Buffer(String::new())
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
    fn enter_on_value_child_submits_directly() {
        // The thinking node is a leaf; its value child is the argument.
        // Enter with a pinned value submits the canonical line directly
        // (no two-step) — the leaf takes the pinned value as its payload.
        let act = ix().enter("thinking med");
        assert_eq!(act, OverlayAction::Submit("thinking med".to_string()));
        // Typing a partial value does not submit: the permissive rule pins
        // any token, but here "h" has no exact candidate — Enter completes
        // to the active candidate. That IS the completion flow; test that
        // it lands on a full value token.
        let act2 = ix().enter("thinking h");
        match act2 {
            OverlayAction::Submit(s) => assert!(s.starts_with("thinking ")),
            OverlayAction::Buffer(b) => assert!(b.starts_with("thinking ")),
            other => panic!("expected completion, got {:?}", other),
        }
    }

    #[test]
    fn esc_pops_one_level() {
        // Pinned selection popped → back to the drilled level (trailing
        // space shows the menu).
        assert_eq!(
            ix().esc("tree compact"),
            OverlayAction::Buffer("tree ".to_string())
        );
        // Drilled level popped → back to the drilled-from path.
        assert_eq!(
            ix().esc("tree compact "),
            OverlayAction::Buffer("tree compact".to_string())
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

    // --- dynamic levels (Phase 4) ---

    fn dyn_provider_rows() -> Vec<crate::fields::Row> {
        vec![
            crate::fields::Row::new("model-a")
                .desc("flagship")
                .field("tier", crate::value::Value::Str("paid".into()))
                .field("price", crate::value::Value::Num(3.0)),
            crate::fields::Row::new("model-b")
                .desc("small")
                .field("tier", crate::value::Value::Str("free".into()))
                .field("price", crate::value::Value::Num(0.0)),
            crate::fields::Row::new("model-c")
                .desc("mid")
                .field("tier", crate::value::Value::Str("paid".into()))
                .field("price", crate::value::Value::Num(1.0)),
        ]
    }

    fn dyn_tree() -> MenuNode {
        MenuNode::new("root").children(vec![
            // Leaf dynamic level: Enter commits the selected row (/model).
            MenuNode::new("model").dynamic().leaf(),
            // Non-leaf dynamic level: Enter drills into the shared action
            // level (/tree, /session).
            MenuNode::new("session").dynamic().children(vec![
                MenuNode::new("resume").leaf(),
                MenuNode::new("fork").leaf(),
                MenuNode::new("delete").leaf(),
            ]),
        ])
    }

    fn dyn_session_ix() -> Interpreter {
        Interpreter::new(dyn_tree()).with_provider(
            &["session"],
            Box::new(crate::provider::StaticProvider {
                result: crate::provider::DataResult {
                    rows: vec![crate::fields::Row::new("s1").desc("main session")],
                    ..Default::default()
                },
            }),
        )
    }

    fn dyn_ix() -> Interpreter {
        Interpreter::new(dyn_tree()).with_provider(
            &["model"],
            Box::new(crate::provider::StaticProvider {
                result: crate::provider::DataResult {
                    rows: dyn_provider_rows(),
                    ..Default::default()
                },
            }),
        )
    }

    #[test]
    fn dynamic_level_lists_rows() {
        let mut i = dyn_ix();
        let st = i.state("model ");
        assert_eq!(st.node_path, vec!["model"]);
        assert_eq!(st.drill_in, true);
        assert_eq!(st.candidates.len(), 3);
        assert_eq!(st.candidates[0].id, "model-a");
    }

    #[test]
    fn dynamic_row_pins_by_id() {
        let mut i = dyn_ix();
        let st = i.state("model model-b");
        assert_eq!(st.selection.as_deref(), Some("model-b"));
        assert_eq!(st.active, 1);
    }

    #[test]
    fn active_selector_seeds_cursor_and_marker() {
        // design §5.1: a dynamic provider's active_selector() becomes the
        // `*` row, and a fresh drilled level seeds its cursor on it.
        struct Sel {
            rows: Vec<crate::fields::Row>,
            active: String,
        }
        impl crate::provider::DataProvider for Sel {
            fn fetch(&mut self, _m: &[crate::grammar::ModifierToken]) -> crate::provider::DataResult {
                crate::provider::DataResult {
                    rows: self.rows.clone(),
                    ..Default::default()
                }
            }
            fn active_selector(&self) -> Option<String> {
                Some(self.active.clone())
            }
        }
        let rows = dyn_provider_rows(); // model-a, model-b, model-c
        let mut i = Interpreter::new(dyn_tree()).with_provider(
            &["model"],
            Box::new(Sel { rows: rows.clone(), active: "model-b".into() }),
        );
        let st = i.state("model ");
        // Active model is model-b (index 1): fresh cursor AND marker land there.
        assert_eq!(st.active_row_marker, Some(1));
        assert_eq!(st.active, 1);

        // A sticky cursor (arrow move) overrides the marker for the cursor,
        // while the marker stays pinned to the active row.
        let mut i = Interpreter::new(dyn_tree()).with_provider(
            &["model"],
            Box::new(Sel { rows, active: "model-b".into() }),
        );
        i.move_cursor("model ", 2);
        let st = i.state("model ");
        assert_eq!(st.active, 2);
        assert_eq!(st.active_row_marker, Some(1));
    }

    #[test]
    fn dynamic_terms_filter_rows() {
        let mut i = dyn_ix();
        let st = i.state("model flag");
        assert_eq!(st.candidates.len(), 1);
        assert_eq!(st.candidates[0].id, "model-a");
    }

    #[test]
    fn dynamic_pin_wins_over_terms() {
        let mut i = dyn_ix();
        // Terms "mid" filter; pinned model-b survives (still in view).
        let st = i.state("model mid model-b");
        // "mid" matches model-c's desc; model-b "small" does not match "mid"
        // → pin falls out of view.
        assert!(!st.candidates.iter().any(|r| r.id == "model-b"));
        assert_eq!(st.pin_lost, true);
        assert_eq!(st.active, 0);
    }

    #[test]
    fn dynamic_tab_pins_id_clearing_terms() {
        let mut i = dyn_ix();
        let act = i.tab("model flag");
        // Accept semantics: the id is committed, refinement terms are
        // cleared (back-navigation restores them via the traversal stack).
        assert_eq!(act, OverlayAction::Buffer("model model-a".to_string()));
    }

    #[test]
    fn dynamic_enter_pins_then_drills() {
        let mut i = dyn_session_ix();
        // Non-leaf dynamic row: Enter = Tab + space (drill into the row's
        // shared action level).
        let act = i.enter("session s1");
        match act {
            OverlayAction::Buffer(b) => assert_eq!(b, "session s1 ".to_string()),
            other => panic!("expected buffer, got {:?}", other),
        }
        // Drilled: the action level shows.
        let st = i.state("session s1 ");
        assert_eq!(st.node_path, vec!["session", "s1"]);
        assert_eq!(st.drill_in, true);
        assert_eq!(
            st.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["resume", "fork", "delete"]
        );
        // Leaf dynamic level (/model shape): Enter submits directly.
        let mut m = dyn_ix();
        assert_eq!(
            m.enter("model model-b"),
            OverlayAction::Submit("model model-b".to_string())
        );
    }

    #[test]
    fn dynamic_field_filters_and_sort() {
        use crate::fields::{FieldSpec, SortDir};
        use crate::grammar::ModifierToken;
        let mut i = Interpreter::new(dyn_tree()).with_provider(
            &["model"],
            Box::new(crate::provider::StaticProvider {
                result: crate::provider::DataResult {
                    rows: dyn_provider_rows(),
                    fields: vec![
                        crate::provider::FieldMeta {
                            spec: FieldSpec::new("tier", false),
                            enum_values: None,
                        },
                        crate::provider::FieldMeta {
                            spec: FieldSpec::new("price", true).dir(SortDir::Asc),
                            enum_values: None,
                        },
                    ],
                    ..Default::default()
                },
            }),
        );
        // Filter: tier=free → only model-b.
        let st = i.state("model .tier:free ");
        assert_eq!(st.candidates.len(), 1);
        assert_eq!(st.candidates[0].id, "model-b");
        // Sort: price ascending via default_dir (paid 3.0, mid 1.0, free 0.0).
        let st2 = i.state("model .sort:price ");
        assert_eq!(st2.candidates[0].id, "model-b");
        // Combined: paid + price ascending → model-c (1.0) then model-a (3.0).
        let st3 = i.state("model .tier:paid .sort:price ");
        assert_eq!(
            st3.candidates.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["model-c", "model-a"]
        );
    }

    #[test]
    fn dynamic_tab_uses_arrowed_cursor_after_rank() {
        // Phase 6.6 item 5: Tab from a filtered view must complete the
        // *cursor* row, not the pre-filter default. Simulate: filter that
        // ranks, arrow to a non-default row, re-derive, Tab.
        let root = MenuNode::new("root").children(vec![
            MenuNode::new("model").dynamic().leaf(),
        ]);
        struct P;
        impl crate::provider::DataProvider for P {
            fn fetch(&mut self, _m: &[crate::grammar::ModifierToken]) -> crate::provider::DataResult {
                crate::provider::DataResult {
                    rows: vec![
                        crate::fields::Row::new("p:gpt4o").desc("GPT-4o flagship"),
                        crate::fields::Row::new("p:gpt4-mini").desc("GPT-4 mini fast"),
                        crate::fields::Row::new("c:opus").desc("Claude opus"),
                    ],
                    ..Default::default()
                }
            }
        }
        let mut i = Interpreter::new(root.clone()).with_provider(&["model"], Box::new(P));
        // Filter narrows to [gpt4o, gpt4-mini]; arrow to index 1 (gpt4-mini).
        let st = i.state("model gpt4");
        assert_eq!(st.candidates.len(), 2);
        i.move_cursor("model gpt4", 1);
        // The cursor survives the re-derive; Tab on it completes gpt4-mini,
        // not the pre-filter pin/default (gpt4o).
        let st = i.state("model gpt4");
        let act = i.tab_at("model gpt4", st.active);
        assert_eq!(act, OverlayAction::Buffer("model p:gpt4-mini".to_string()));
    }

    #[test]
    fn dynamic_fetch_cached_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        struct Counting {
            rows: Vec<Row>,
            calls: Arc<AtomicUsize>,
        }
        impl crate::provider::DataProvider for Counting {
            fn fetch(&mut self, _m: &[crate::grammar::ModifierToken]) -> crate::provider::DataResult {
                self.calls.fetch_add(1, Ordering::SeqCst);
                crate::provider::DataResult {
                    rows: self.rows.clone(),
                    ..Default::default()
                }
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let mut i = Interpreter::new(dyn_tree()).with_provider(
            &["model"],
            Box::new(Counting {
                rows: dyn_provider_rows(),
                calls: calls.clone(),
            }),
        );
        let _ = i.state("model ");
        let _ = i.state("model a");
        let _ = i.tab("model ");
        let _ = i.enter("model ");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one-shot fetch is cached");
    }

    #[test]
    fn activation_bundle_persists_cache_across_engines() {
        // Phase 6.7 step 1: the opaque per-activation bundle carries the
        // one-shot data cache across engine rebuilds (menu re-opens / frame
        // re-derives), so a dynamic provider is fetched once per open, not
        // once per keypress.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        struct Counting {
            rows: Vec<Row>,
            calls: Arc<AtomicUsize>,
        }
        impl crate::provider::DataProvider for Counting {
            fn fetch(&mut self, _m: &[crate::grammar::ModifierToken]) -> crate::provider::DataResult {
                self.calls.fetch_add(1, Ordering::SeqCst);
                crate::provider::DataResult {
                    rows: self.rows.clone(),
                    ..Default::default()
                }
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let mut i = Interpreter::new(dyn_tree()).with_provider(
            &["model"],
            Box::new(Counting {
                rows: dyn_provider_rows(),
                calls: calls.clone(),
            }),
        );
        let _ = i.state("model ");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Rebuild (as the host does per keypress): take the bundle, drop,
        // build a fresh engine, restore.
        let act = i.take_activation();
        let mut i2 = Interpreter::new(dyn_tree()).with_provider(
            &["model"],
            Box::new(Counting {
                rows: dyn_provider_rows(),
                calls: calls.clone(),
            }),
        );
        // Without the restored cache the fresh engine would re-fetch.
        assert_eq!(calls.load(Ordering::SeqCst), 1, "no re-fetch before restore");
        let _ = i2.state("model ");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "fresh engine re-fetches");
        i2.restore_activation(act);
        let _ = i2.state("model ");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "restored cache avoids re-fetch");
    }
}
