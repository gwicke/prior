//! v2.5 `SlashDotNode` for `/reasoning`.
//!
//! The command keeps a static five-level list (off | low | med | high | max)
//! regardless of what the provider behind it accepts — the frontend does not
//! learn provider vocabularies. What a level *turns into* is a backend
//! question, and `header()` is where the answer is disclosed.

use std::sync::Arc;

use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode, SubmitResult,
};

// ---------------------------------------------------------------------------
// ReasoningEnumNode — returns all reasoning levels from a single node.
//
// The parent listing shows off/low/med/high/max as rows. On submit, the
// selected level is read from path_args (the drilled row ID) and written
// to the shared cell.
//
// `header()` discloses what each preset will actually run on the active
// provider + model. Two independent things can move a preset off its
// nominal value — the provider's own request table (DeepSeek accepts
// `medium` and serves `high`) and the model's advertised level list — and a
// substitution the user cannot see is worse than one we show them.
// ---------------------------------------------------------------------------

pub struct ReasoningEnumNode {
    /// Read the current thinking level.
    pub active_fn: Box<dyn Fn() -> String>,
    /// Write the selected reasoning level. `Ok(())` on success.
    pub submit_fn: crate::SubmitFn,
    /// The active `provider:model`, when this node is the live `/reasoning`
    /// one. `None` under `/set default_reasoning_level`.
    pub active_model_fn: Option<Box<dyn Fn() -> String>>,
}

impl ReasoningEnumNode {
    /// The five presets, in ladder order. Shared with the header so the
    /// listing and the explanation can never disagree about what exists.
    pub const LEVELS: [(&'static str, &'static str); 5] = [
        ("off", "no extra reasoning"),
        ("low", "minimal reasoning"),
        ("med", "balanced reasoning"),
        ("high", "high reasoning"),
        ("max", "maximum reasoning"),
    ];

    /// Provider + support list for the active model, or `None` when there is
    /// no active model to report on (`/set default_reasoning_level`).
    fn level_context(&self) -> Option<LevelContext> {
        let model_fn = self.active_model_fn.as_ref()?;
        let active = model_fn();
        let (provider, model) = active.split_once(':')?;
        let (provider, model) = (provider.to_string(), model.to_string());
        let support = crate::model_catalog::reasoning_support(&provider, &model);
        Some(LevelContext { provider, support })
    }

    /// Explain what each preset will actually run, so the user can choose
    /// working backwards from the effort they want.
    ///
    /// This is the honest answer to a remapper that cannot always send what
    /// was asked for: rather than substituting quietly, show the substitution.
    /// Emits nothing for a provider with no documented table and a model with
    /// no published list — there is nothing to disclose.
    fn mapping_lines(&self) -> Vec<String> {
        let Some(c) = self.level_context() else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        if let Some(summary) = crate::params::provider_effort_mapping_summary(&c.provider) {
            lines.push(format!("{} maps: {summary}", c.provider));
        }
        if c.support.is_known() {
            lines.push(format!(
                "this model supports: {}",
                c.support.levels().join(", ")
            ));
        }
        if lines.is_empty() {
            return lines;
        }
        lines.push("your presets land on:".to_string());
        for (name, _) in Self::LEVELS {
            if let Some(landed) = landing(name, &c) {
                lines.push(format!("  {name} → {landed}"));
            }
        }
        lines
    }
}

/// The active provider plus the model's published effort list — everything
/// needed to say where a preset lands.
struct LevelContext {
    provider: String,
    support: crate::params::ReasoningSupport,
}

/// Where a preset actually lands on this provider+model, as the user should
/// read it: what the service *serves*, which may differ from what we send
/// (`medium` in, `high` out on DeepSeek). `None` when the level is `off` or
/// the provider has no effort vocabulary to speak of.
fn landing(name: &str, c: &LevelContext) -> Option<String> {
    let level: crate::params::ReasoningLevel = name.parse().ok()?;
    let sent = crate::params::resolve_effort_for(&c.provider, level, &c.support)?;
    Some(
        crate::params::provider_served_effort(&c.provider, &sent)
            .unwrap_or(sent.as_str())
            .to_string(),
    )
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for ReasoningEnumNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let active = (self.active_fn)();
        let rows = Self::LEVELS
            .iter()
            .map(|(l, d)| {
                // Descriptions carry where the level actually lands, so the
                // answer shows while scanning rows, not only in `header()`.
                let desc = match self.level_context() {
                    Some(c) => match landing(l, &c) {
                        Some(landed) if landed != *l => format!("{d} → {landed}"),
                        _ => (*d).to_string(),
                    },
                    None => (*d).to_string(),
                };
                crate::slashdot_menu::fields::Row::new(*l).desc(desc)
            })
            .collect();
        Ok(Arc::new(DataResponse {
            rows,
            active_ids: vec![active],
        }))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        let level = path_args.ids.last().cloned().unwrap_or_default();
        match (self.submit_fn)(&level) {
            Ok(()) => SubmitResult::Done(None),
            Err(msg) => SubmitResult::Done(Some(msg)),
        }
    }

    /// Current level plus, when this is the live node, what each preset will
    /// actually run on this provider and model.
    async fn header(&mut self, _path_args: &PathArgs, _ctx: &RenderContext) -> Vec<String> {
        let mut lines = vec![format!("reasoning: {}", (self.active_fn)())];
        lines.extend(self.mapping_lines());
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_state::AppState;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::{MenuResult, StaticListingNode, StaticRow};
    use crate::slashdot_menu::HostKey;
    use std::sync::Arc;

    fn test_app_state() -> Arc<AppState> {
        let settings = crate::settings::SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_reasoning_level: None,
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            models: Default::default(),
            path: None,
        };
        crate::app_state::AppState::for_test(settings)
    }

    /// Mini menu mirroring the `v25_menu` wiring: root → listing row
    /// "reasoning" → `ReasoningEnumNode` bound to the target config.
    fn build_reasoning_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let reader = app_state.clone();
        let reader2 = app_state.clone();
        let writer = app_state.clone();
        let mut tl: SlashDotMenuNode = StaticListingNode::new(&[StaticRow::new(
            "reasoning",
            "Set reasoning level: off | low | med | high | max",
        )])
        .into();
        tl.add_child(
            ReasoningEnumNode {
                active_fn: Box::new(move || {
                    reader.reasoning().unwrap_or_else(|| "off".to_string())
                }),
                submit_fn: Box::new(move |level| {
                    let parsed = level.parse::<crate::params::ReasoningLevel>()?;
                    writer.set_reasoning(Some(parsed));
                    Ok(())
                }),
                active_model_fn: Some(Box::new(move || reader2.model())),
            }
            .into(),
        );
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(tl);
        SlashDotMenu::from_root_node(root)
    }

    fn row_texts(plan: &crate::slashdot_menu::render::RenderPlan) -> Vec<String> {
        plan.rows.iter().map(|l| l.0.clone()).collect()
    }

    /// Root listing shows the `thinking` command row.
    #[test]
    fn reasoning_root_row_present() {
        let app_state = test_app_state();
        let mut menu = build_reasoning_menu(&app_state);
        let mut h = menu.get_handle();
        match futures::executor::block_on(h.eval_prompt("", HostKey::None, 80)) {
            MenuResult::Continue(plan, _) => {
                let rows = row_texts(&plan);
                assert!(
                    rows.iter()
                        .any(|r| r.split_whitespace().next() == Some("reasoning")),
                    "root should list 'thinking': {rows:?}"
                );
            }
            other => panic!(
                "expected Continue at root, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// The current level gets the `*` marker: `plan.active_rows` points at
    /// the row whose id is the active level.
    #[test]
    fn reasoning_active_marker_on_current_level() {
        let app_state = test_app_state();
        app_state.set_reasoning(Some(crate::params::ReasoningLevel::Med));
        let mut menu = build_reasoning_menu(&app_state);
        let mut h = menu.get_handle();
        match futures::executor::block_on(h.eval_prompt("reasoning ", HostKey::None, 80)) {
            MenuResult::Continue(plan, _) => {
                let rows = row_texts(&plan);
                let idx = rows
                    .iter()
                    .position(|r| r.split_whitespace().next() == Some("med"))
                    .expect("'med' row must be listed");
                assert_eq!(
                    plan.active_rows,
                    vec![idx],
                    "active marker must point at 'med': rows={rows:?}"
                );
            }
            other => panic!(
                "expected Continue, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// Tab selects a level, Enter submits it → the target config's thinking.
    #[test]
    fn reasoning_submit_sets_target_config() {
        let app_state = test_app_state();
        let mut menu = build_reasoning_menu(&app_state);
        let mut h = menu.get_handle();
        let prompt = match futures::executor::block_on(h.eval_prompt_resolved(
            "reasoning ",
            HostKey::Tab,
            80,
        )) {
            MenuResult::Continue(_, prompt) => prompt,
            other => panic!(
                "expected Continue after Tab, got {:?}",
                std::mem::discriminant(&other)
            ),
        };
        let sel = prompt
            .split_whitespace()
            .last()
            .expect("Tab should append the selected level")
            .to_string();
        match futures::executor::block_on(h.eval_prompt(&prompt, HostKey::Enter, 80)) {
            MenuResult::DoneSuccess(..) => {}
            other => panic!(
                "expected DoneSuccess on Enter, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert_eq!(app_state.reasoning().as_deref(), Some(sel.as_str()));
    }
    /// The header discloses what each preset will actually run, so the user
    /// can pick working backwards from the effort they want.
    #[test]
    fn header_discloses_the_provider_mapping_and_where_presets_land() {
        use crate::slashdot_menu::node_impl::RenderContext;
        let app_state = test_app_state();
        app_state.set_model("deepseek:deepseek-v4.1-flash");
        app_state.set_reasoning(Some(crate::params::ReasoningLevel::Med));
        let reader = app_state.clone();
        let writer = app_state.clone();
        let reader2 = app_state.clone();
        let mut node = ReasoningEnumNode {
            active_fn: Box::new(move || reader.reasoning().unwrap_or_else(|| "off".to_string())),
            submit_fn: Box::new(move |level| {
                writer.set_reasoning(Some(level.parse().unwrap()));
                Ok(())
            }),
            active_model_fn: Some(Box::new(move || reader2.model())),
        };
        let lines = futures::executor::block_on(node.header(
            &PathArgs::new(vec![], ""),
            &RenderContext { term_width: 80 },
        ));
        let joined = lines.join("\n");
        assert!(joined.contains("reasoning: med"), "{joined}");
        assert!(joined.contains("deepseek maps:"), "{joined}");
        assert!(joined.contains("medium"), "{joined}");
        // `med` must be shown landing on what DeepSeek actually serves.
        assert!(
            joined.contains("med \u{2192} high"),
            "med must disclose it lands on high: {joined}"
        );
        // Every preset that is an actual effort is listed, so the whole ladder
        // is readable. `off` is deliberately absent: it is the absence of
        // reasoning, not an effort, and there is nothing to disclose about it.
        for preset in ["low", "med", "high", "max"] {
            assert!(joined.contains(preset), "missing preset {preset}: {joined}");
        }
        assert!(
            !joined.contains("off →"),
            "off must not be given a landing: {joined}"
        );
    }

    /// The picker rows carry the landing inline, so the mapping is visible
    /// while scanning rather than only after opening the header.
    #[test]
    fn row_descriptions_carry_where_each_level_lands() {
        let app_state = test_app_state();
        app_state.set_model("deepseek:deepseek-v4.1-flash");
        let reader = app_state.clone();
        let reader2 = app_state.clone();
        let mut node = ReasoningEnumNode {
            active_fn: Box::new(move || reader.reasoning().unwrap_or_else(|| "off".to_string())),
            submit_fn: Box::new(|_| Ok(())),
            active_model_fn: Some(Box::new(move || reader2.model())),
        };
        let resp = futures::executor::block_on(node.data(1, &PathArgs::new(vec![], ""))).unwrap();
        let by_id = |id: &str| {
            resp.rows
                .iter()
                .find(|r| r.id == id)
                .map(|r| r.description.clone().unwrap_or_default())
                .unwrap_or_default()
        };
        // DeepSeek serves `high` for a `medium` request: the row must say so.
        let med = by_id("med");
        assert!(med.contains("→ high"), "med row must disclose: {med}");
        // An effort that survives unchanged keeps its plain blurb.
        let high = by_id("high");
        assert!(!high.contains('→'), "high lands as requested: {high}");
        // `off` is not an effort and gets no landing.
        assert!(!by_id("off").contains('→'), "{}", by_id("off"));
    }

    /// With nothing to disclose the rows keep their plain blurbs, so the
    /// picker never shows an empty or noisy arrow.
    #[test]
    fn row_descriptions_stay_plain_when_there_is_nothing_to_disclose() {
        let app_state = test_app_state();
        app_state.set_model("anthropic:claude-3-5-sonnet-20241022");
        let reader = app_state.clone();
        let reader2 = app_state.clone();
        let mut node = ReasoningEnumNode {
            active_fn: Box::new(move || reader.reasoning().unwrap_or_else(|| "off".to_string())),
            submit_fn: Box::new(|_| Ok(())),
            active_model_fn: Some(Box::new(move || reader2.model())),
        };
        let resp = futures::executor::block_on(node.data(1, &PathArgs::new(vec![], ""))).unwrap();
        for row in &resp.rows {
            let desc = row.description.clone().unwrap_or_default();
            assert!(!desc.contains('→'), "{}: {desc}", row.id);
        }
    }

    /// A provider with no published table and a model with no published list
    /// has nothing to disclose, so the header stays one line.
    #[test]
    fn header_is_just_the_level_when_there_is_nothing_to_disclose() {
        use crate::slashdot_menu::node_impl::RenderContext;
        let app_state = test_app_state();
        app_state.set_model("anthropic:claude-3-5-sonnet-20241022");
        let reader = app_state.clone();
        let reader2 = app_state.clone();
        let mut node = ReasoningEnumNode {
            active_fn: Box::new(move || reader.reasoning().unwrap_or_else(|| "off".to_string())),
            submit_fn: Box::new(|_| Ok(())),
            active_model_fn: Some(Box::new(move || reader2.model())),
        };
        let lines = futures::executor::block_on(node.header(
            &PathArgs::new(vec![], ""),
            &RenderContext { term_width: 80 },
        ));
        assert_eq!(lines, vec!["reasoning: off".to_string()]);
    }

    /// `/set default_reasoning_level` reuses this node with no active model,
    /// and must not claim anything about the running model.
    #[test]
    fn header_stays_quiet_without_an_active_model() {
        use crate::slashdot_menu::node_impl::RenderContext;
        let mut node = ReasoningEnumNode {
            active_fn: Box::new(|| "off".to_string()),
            submit_fn: Box::new(|_| Ok(())),
            active_model_fn: None,
        };
        let lines = futures::executor::block_on(node.header(
            &PathArgs::new(vec![], ""),
            &RenderContext { term_width: 80 },
        ));
        assert_eq!(lines, vec!["reasoning: off".to_string()]);
    }
}
