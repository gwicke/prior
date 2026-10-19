//! v2.5 `SlashDotNode` for the numeric request params: `/temperature` and
//! `/top_p` (doc/model_settings_v2.md A.8).
//!
//! One node serves both. They differ only in their name, their accepted range,
//! and the words that explain what moving the number does — so the difference
//! is data on the node, not two near-identical implementations.
//!
//! **No `complete_parameter`.** The numbers are typed, not chosen: the prefill
//! carries the current value and `header()` explains the default, the range,
//! and the effect. Discovery comes from the header, not a dialog.
//!
//! **Empty means unset.** The built-in default for both is `None` — "leave the
//! provider's own default alone" — so a prefill of `0` or `1` would misreport
//! the state *and* set a value on Enter that the user never chose. An empty
//! submit therefore clears the setting back to `None`, which is the inverse of
//! setting it and the only undo that does not need `$EDITOR`.

use std::sync::Arc;

use crate::app_state::AppState;
use crate::params::ModelParams;
use crate::slashdot_menu::fields::Row;
use crate::slashdot_menu::node_impl::{
    DataResponse, DataResult, PathArgs, RenderContext, SlashDotNode, SubmitResult,
};

/// Which numeric knob a node edits. The metadata each carries is everything
/// that differs between `/temperature` and `/top_p`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberParam {
    Temperature,
    TopP,
}

impl NumberParam {
    /// The command name — also the row id in the root listing.
    pub fn name(self) -> &'static str {
        match self {
            NumberParam::Temperature => "temperature",
            NumberParam::TopP => "top_p",
        }
    }

    /// One-line description for the root listing.
    pub fn blurb(self) -> &'static str {
        match self {
            NumberParam::Temperature => "Sampling temperature (empty = provider default)",
            NumberParam::TopP => "Nucleus sampling cutoff (empty = provider default)",
        }
    }

    /// The accepted range, inclusive. Chosen as the widest any major provider
    /// takes, so a value inside it is not rejected on a technicality; a value
    /// outside the *provider's* narrower range is accepted here and left to
    /// surface as the provider's own complaint (see Risks in the design doc).
    pub fn range(self) -> (f64, f64) {
        match self {
            // OpenAI-style 0–2; Anthropic caps at 1.0 and Gemini at 2.0.
            NumberParam::Temperature => (0.0, 2.0),
            NumberParam::TopP => (0.0, 1.0),
        }
    }

    /// The current value from the target config, or `None` when unset.
    pub fn current(self, params: &ModelParams) -> Option<f64> {
        match self {
            NumberParam::Temperature => params.temperature,
            NumberParam::TopP => params.top_p,
        }
    }

    /// Write (or clear, with `None`) the value on the target config.
    fn apply(self, app_state: &AppState, value: Option<f64>) {
        app_state.set_param(move |p| match self {
            NumberParam::Temperature => p.temperature = value,
            NumberParam::TopP => p.top_p = value,
        });
    }

    /// What moving this number does, for `header()`. Deliberately in terms of
    /// behavior rather than numbers — "more random" is the part a user can act
    /// on, and the exact curve is the provider's business.
    fn effect(self) -> &'static str {
        match self {
            NumberParam::Temperature => "lower = more focused and repeatable; higher = more varied",
            NumberParam::TopP => {
                "lower = narrower candidate set, more predictable; higher = broader, more varied"
            }
        }
    }
}

/// Format a value for display without trailing-zero noise: `0.8`, not
/// `0.8000000000000001`, and `1` rather than `1.0`.
pub fn fmt_value(v: f64) -> String {
    let s = format!("{v:.3}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Parse + range-check one value for `param`.
///
/// Empty clears the setting (the inverse of setting it). Anything else must
/// parse as a number inside the range; the error names the key, the value and
/// the range, because a bare "invalid" sends the user hunting.
///
/// The single validator, shared by the `/temperature` and `/top_p` menu nodes
/// and by the per-turn override parser — so a value one accepts cannot be
/// rejected by the other, and a range change lands in both at once.
pub fn parse_value(param: NumberParam, raw: &str) -> Result<Option<f64>, String> {
    let v = raw.trim();
    if v.is_empty() {
        return Ok(None);
    }
    let n: f64 = v
        .parse()
        .map_err(|_| format!("{} must be a number (got '{v}')", param.name()))?;
    let (lo, hi) = param.range();
    if !n.is_finite() || n < lo || n > hi {
        return Err(format!(
            "{} must be between {} and {} (got '{v}')",
            param.name(),
            fmt_value(lo),
            fmt_value(hi),
        ));
    }
    Ok(Some(n))
}

/// Display value for the row: the live value, or the fact that there is none.
/// A literal placeholder number would be a lie — the default is the provider's.
fn display_value(p: NumberParam, params: &ModelParams) -> String {
    match p.current(params) {
        Some(v) => fmt_value(v),
        None => "provider default".to_string(),
    }
}

/// What the catalog publishes about this knob for the active model.
///
/// Two facts, both worth having and neither worth guessing: whether the model
/// accepts the knob at all (a fair number do not — the most-popular
/// `openai/gpt-*` entries list neither `temperature` nor `top_p`), and the
/// provider's own default when it publishes one. The default is shown and
/// never sent: `None` means "leave it alone", and sending a default we merely
/// know about would put an explicit value on the wire for every model —
/// changing sampling on models that were fine, and breaking the reasoning
/// models that reject a non-default temperature.
fn catalog_notes(p: NumberParam, app_state: &AppState) -> Vec<String> {
    let active = app_state.model();
    let Some((provider, model)) = active.split_once(':') else {
        return Vec::new();
    };
    let resolved = app_state.resolved_models();
    let Some(entry) = resolved.iter().find(|e| {
        let (p, m) = e.qualified_parts();
        p.eq_ignore_ascii_case(provider) && m.eq_ignore_ascii_case(model)
    }) else {
        return Vec::new();
    };
    let (supported, published) = match p {
        NumberParam::Temperature => (entry.supports_temperature, entry.default_temperature),
        NumberParam::TopP => (entry.supports_top_p, entry.default_top_p),
    };
    vec![
        if supported {
            format!("this model accepts {}", p.name())
        } else {
            format!(
                "this model does not list {} in supported_parameters",
                p.name()
            )
        },
        // An explicit `null` in `default_parameters` and an absent key mean the
        // same thing here: the provider publishes no default.
        match published {
            Some(v) => format!("provider default: {}", fmt_value(v)),
            None => "provider default: not published".to_string(),
        },
    ]
}

pub struct NumberParamNode {
    param: NumberParam,
    app_state: Arc<AppState>,
}

impl NumberParamNode {
    pub fn new(param: NumberParam, app_state: &Arc<AppState>) -> Self {
        NumberParamNode {
            param,
            app_state: app_state.clone(),
        }
    }

    /// Parse + range-check one committed value. See [`parse_value`].
    fn parse(&self, raw: &str) -> Result<Option<f64>, String> {
        parse_value(self.param, raw)
    }
}

#[async_trait::async_trait(?Send)]
impl SlashDotNode for NumberParamNode {
    async fn data(&mut self, _epoch: u64, _path_args: &PathArgs) -> DataResult {
        let params = self.app_state.params();
        let row = Row::new(self.param.name()).desc(self.param.blurb()).field(
            "value",
            crate::slashdot_menu::value::Value::Str(display_value(self.param, &params)),
        );
        Ok(Arc::new(DataResponse {
            rows: vec![row],
            active_ids: Vec::new(),
        }))
    }

    /// Prefill the current value; empty when unset, so Enter on an untouched
    /// line is a no-op rather than a silent set (see the module doc).
    async fn drill_rewrite(&mut self, path_args: &PathArgs) -> Option<String> {
        let current = self.param.current(&self.app_state.params());
        let suffix = match current {
            Some(v) => format!(" {}", fmt_value(v)),
            None => " ".to_string(),
        };
        Some(path_args.prompt_with(&suffix))
    }

    async fn submit(&mut self, _epoch: u64, path_args: &PathArgs) -> SubmitResult {
        match self.parse(&path_args.suffix) {
            // `Error` (not `Done`): the engine renders it above `header()` and
            // keeps the menu open with the bad text still in the buffer, so
            // the user edits rather than retypes.
            Err(msg) => SubmitResult::Error(msg),
            Ok(value) => {
                self.param.apply(&self.app_state, value);
                SubmitResult::Done(None)
            }
        }
    }

    async fn header(&mut self, _path_args: &PathArgs, _render_ctx: &RenderContext) -> Vec<String> {
        let params = self.app_state.params();
        let (lo, hi) = self.param.range();
        let mut lines = vec![
            format!(
                "{} = {}",
                self.param.name(),
                display_value(self.param, &params)
            ),
            format!("range: {} – {}", fmt_value(lo), fmt_value(hi)),
            "default: provider (empty clears it back to that)".to_string(),
            format!("effect: {}", self.param.effect()),
        ];
        // The catalog-derived facts (does the model accept this knob, and
        // what is the provider's own default) come from `ModelEntry`. They
        // are absent for a model the catalog does not cover — and absent is
        // the right answer there, where a guess would be worse than silence.
        lines.extend(catalog_notes(self.param, &self.app_state));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::ReasoningLevel;
    use crate::settings::SettingsStore;
    use crate::slashdot_menu::menu::{SlashDotMenu, SlashDotMenuNode};
    use crate::slashdot_menu::node_impl::MenuResult;
    use crate::slashdot_menu::node_impl::StaticListingNode;
    use crate::slashdot_menu::HostKey;

    fn test_app_state() -> Arc<AppState> {
        let settings = SettingsStore {
            default_model: "openrouter:z-ai/glm-5.3".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 50,
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

    fn node(p: NumberParam, app_state: &Arc<AppState>) -> NumberParamNode {
        NumberParamNode::new(p, app_state)
    }

    /// Root listing → `/temperature` → typed value → Enter, the full path the
    /// user actually walks.
    fn build_menu(app_state: &Arc<AppState>) -> SlashDotMenu {
        let mut root: SlashDotMenuNode = StaticListingNode::new(&[]).into();
        root.add_child(node(NumberParam::Temperature, app_state).into());
        root.add_child(node(NumberParam::TopP, app_state).into());
        SlashDotMenu::from_root_node(root)
    }

    fn eval(
        h: &mut crate::slashdot_menu::SlashDotHandle<'_>,
        prompt: &str,
        key: HostKey,
    ) -> MenuResult {
        futures::executor::block_on(h.eval_prompt_resolved(prompt, key, 80))
    }

    // ---- parse + range ----

    #[test]
    fn accepts_values_inside_the_range_and_rejects_outside() {
        let app_state = test_app_state();
        let temp = node(NumberParam::Temperature, &app_state);
        assert_eq!(temp.parse("0.8"), Ok(Some(0.8)));
        assert_eq!(temp.parse("0"), Ok(Some(0.0)));
        assert_eq!(temp.parse(" 1.5 "), Ok(Some(1.5)));
        assert!(temp.parse("2.1").is_err());
        assert!(temp.parse("-0.1").is_err());
        assert!(temp.parse("abc").is_err());
        // The error must name the key and the range, not just "invalid".
        let err = temp.parse("9").unwrap_err();
        assert!(err.contains("temperature"), "{err}");
        assert!(err.contains("0"), "{err}");

        let top_p = node(NumberParam::TopP, &app_state);
        assert_eq!(top_p.parse("0.95"), Ok(Some(0.95)));
        assert_eq!(top_p.parse("1"), Ok(Some(1.0)));
        assert!(top_p.parse("1.5").is_err(), "top_p tops out at 1.0");
    }

    #[test]
    fn empty_clears_rather_than_errors() {
        let app_state = test_app_state();
        let n = node(NumberParam::Temperature, &app_state);
        assert_eq!(n.parse(""), Ok(None));
        assert_eq!(n.parse("   "), Ok(None));
    }

    // ---- values reach the target config ----

    #[test]
    fn submit_writes_and_clears_the_target_config() {
        let app_state = test_app_state();
        let mut n = node(NumberParam::Temperature, &app_state);
        let pa = PathArgs::new(vec!["temperature".into()], " 0.25");
        assert!(matches!(
            futures::executor::block_on(n.submit(1, &pa)),
            SubmitResult::Done(None)
        ));
        assert_eq!(app_state.params().temperature, Some(0.25));

        // Empty submit is the undo.
        let pa = PathArgs::new(vec!["temperature".into()], " ");
        assert!(matches!(
            futures::executor::block_on(n.submit(1, &pa)),
            SubmitResult::Done(None)
        ));
        assert_eq!(app_state.params().temperature, None);
    }

    #[test]
    fn top_p_writes_its_own_field_and_leaves_temperature_alone() {
        let app_state = test_app_state();
        let mut n = node(NumberParam::TopP, &app_state);
        futures::executor::block_on(n.submit(1, &PathArgs::new(vec!["top_p".into()], "0.9")));
        let p = app_state.params();
        assert_eq!(p.top_p, Some(0.9));
        assert_eq!(p.temperature, None, "top_p must not touch temperature");
    }

    /// A rejected value must not land in the config.
    #[test]
    fn an_out_of_range_value_stays_open_and_writes_nothing() {
        let app_state = test_app_state();
        app_state.set_param(|p| p.temperature = Some(0.5));
        let mut n = node(NumberParam::Temperature, &app_state);
        match futures::executor::block_on(
            n.submit(1, &PathArgs::new(vec!["temperature".into()], "7")),
        ) {
            SubmitResult::Error(msg) => assert!(msg.contains("between"), "{msg}"),
            _ => panic!("expected an Error that keeps the menu open"),
        }
        assert_eq!(
            app_state.params().temperature,
            Some(0.5),
            "a rejected value must not be applied"
        );
    }

    // ---- prefill ----

    #[test]
    fn drill_prefills_the_current_value() {
        let app_state = test_app_state();
        app_state.set_param(|p| p.temperature = Some(0.7));
        let mut n = node(NumberParam::Temperature, &app_state);
        let out = futures::executor::block_on(
            n.drill_rewrite(&PathArgs::new(vec!["temperature".into()], " ")),
        );
        assert_eq!(out, Some("temperature 0.7".to_string()));
    }

    /// Unset prefills EMPTY — a placeholder like `1` would both misreport the
    /// state and set a value the user never chose on Enter.
    #[test]
    fn drill_prefills_empty_when_unset() {
        let app_state = test_app_state();
        for p in [NumberParam::Temperature, NumberParam::TopP] {
            let mut n = node(p, &app_state);
            let out = futures::executor::block_on(
                n.drill_rewrite(&PathArgs::new(vec![p.name().into()], " ")),
            );
            assert_eq!(out, Some(format!("{} ", p.name())));
            assert!(
                !out.unwrap().contains(char::is_numeric),
                "{} must not prefill a number it does not have",
                p.name()
            );
        }
    }

    // ---- header + row ----

    #[test]
    fn header_reports_value_range_default_and_effect() {
        let app_state = test_app_state();
        let mut n = node(NumberParam::TopP, &app_state);
        let lines = futures::executor::block_on(n.header(
            &PathArgs::new(vec![], ""),
            &RenderContext { term_width: 80 },
        ));
        let joined = lines.join("\n");
        assert!(joined.contains("top_p = provider default"), "{joined}");
        assert!(joined.contains("0 – 1"), "{joined}");
        assert!(joined.contains("default: provider"), "{joined}");
        assert!(joined.contains("effect:"), "{joined}");

        app_state.set_param(|p| p.top_p = Some(0.95));
        let lines = futures::executor::block_on(n.header(
            &PathArgs::new(vec![], ""),
            &RenderContext { term_width: 80 },
        ));
        assert!(lines[0].contains("0.95"), "{:?}", lines[0]);
    }

    #[test]
    fn data_row_shows_the_live_value() {
        let app_state = test_app_state();
        app_state.set_param(|p| p.temperature = Some(0.35));
        let mut n = node(NumberParam::Temperature, &app_state);
        let resp = futures::executor::block_on(n.data(1, &PathArgs::new(vec![], ""))).unwrap();
        assert_eq!(resp.rows.len(), 1);
        assert_eq!(resp.rows[0].id, "temperature");
        assert_eq!(
            resp.rows[0].field_value("value"),
            Some(&crate::slashdot_menu::value::Value::Str("0.35".into()))
        );
    }

    // ---- full traversal ----

    #[test]
    fn full_traversal_sets_the_value() {
        let app_state = test_app_state();
        let mut menu = build_menu(&app_state);
        let mut h = menu.get_handle();

        // Root lists both commands.
        match eval(&mut h, "", HostKey::None) {
            MenuResult::Continue(plan, _) => {
                let texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
                assert!(texts.iter().any(|t| t.contains("temperature")), "{texts:?}");
                assert!(texts.iter().any(|t| t.contains("top_p")), "{texts:?}");
            }
            other => panic!("expected Continue at root, got {other:?}"),
        }

        // Drill in (Enter on the root row) and commit a typed value.
        match eval(&mut h, "temperature 0.15", HostKey::Enter) {
            MenuResult::DoneSuccess(..) => {}
            other => panic!("expected DoneSuccess, got {other:?}"),
        }
        assert_eq!(app_state.params().temperature, Some(0.15));
    }

    /// Setting one knob must leave the others — and the reasoning level —
    /// exactly as they were, since they share one `ModelParams` now.
    #[test]
    fn setting_one_knob_leaves_the_others_alone() {
        let app_state = test_app_state();
        app_state.set_param(|p| p.reasoning = Some(ReasoningLevel::High));
        app_state.set_param(|p| p.top_p = Some(0.8));
        let mut n = node(NumberParam::Temperature, &app_state);
        futures::executor::block_on(n.submit(1, &PathArgs::new(vec!["temperature".into()], "0.4")));
        let p = app_state.params();
        assert_eq!(p.temperature, Some(0.4));
        assert_eq!(p.top_p, Some(0.8), "top_p untouched");
        assert_eq!(p.reasoning, Some(ReasoningLevel::High), "level untouched");
    }

    #[test]
    fn value_formatting_drops_float_noise() {
        assert_eq!(fmt_value(0.8), "0.8");
        assert_eq!(fmt_value(1.0), "1");
        assert_eq!(fmt_value(0.0), "0");
        assert_eq!(fmt_value(0.125), "0.125");
    }
    /// P2 smoke: the real root menu (v25_menu::build_menu) offers both numeric
    /// commands, and a full drill sets the value on the target config.
    #[test]
    fn real_root_menu_exposes_and_sets_the_numeric_params() {
        use crate::slashdot_menu::HostKey;
        let app_state = test_app_state();
        let mut menu = crate::v25_menu::build_menu(&app_state);
        let mut h = menu.get_handle();

        match futures::executor::block_on(h.eval_prompt_resolved("", HostKey::None, 80)) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                let texts: Vec<&str> = plan.rows.iter().map(|l| l.0.as_str()).collect();
                assert!(
                    texts.iter().any(|t| t.starts_with("temperature")),
                    "{texts:?}"
                );
                assert!(texts.iter().any(|t| t.starts_with("top_p")), "{texts:?}");
            }
            other => panic!("expected Continue, got {other:?}"),
        }

        // Drill via Enter on the bare id, then type the value and commit.
        let drilled =
            futures::executor::block_on(h.eval_prompt_resolved("temperature", HostKey::Enter, 80));
        let prompt = match drilled {
            crate::slashdot_menu::node_impl::MenuResult::Continue(_, p) => p,
            other => panic!("expected Continue after drill, got {other:?}"),
        };
        assert!(
            prompt.trim().is_empty() || prompt.trim_start_matches("temperature").trim().is_empty(),
            "unset temperature must drill to an empty value, got {prompt:?}"
        );

        let committed = format!("{}0.6", prompt);
        match futures::executor::block_on(h.eval_prompt_resolved(&committed, HostKey::Enter, 80)) {
            crate::slashdot_menu::node_impl::MenuResult::DoneSuccess(msg, _) => {
                // A rejected value would surface its message here; a good one
                // commits silently.
                assert!(msg.is_none(), "unexpected message: {msg:?}");
            }
            other => panic!("expected DoneSuccess, got {other:?}"),
        }
        assert_eq!(app_state.params().temperature, Some(0.6));
    }

    /// The same walk for `/top_p`, and a bad value must NOT close the menu.
    #[test]
    fn bad_value_keeps_the_menu_open() {
        use crate::slashdot_menu::HostKey;
        let app_state = test_app_state();
        let mut menu = crate::v25_menu::build_menu(&app_state);
        let mut h = menu.get_handle();
        match futures::executor::block_on(h.eval_prompt_resolved("top_p 1.5", HostKey::Enter, 80)) {
            crate::slashdot_menu::node_impl::MenuResult::Continue(plan, _) => {
                let frame = plan
                    .detail
                    .iter()
                    .map(|l| l.0.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    frame.contains("ERROR") || frame.contains("between"),
                    "{frame}"
                );
            }
            other => panic!("a bad value must keep the menu open, got {other:?}"),
        }
        assert_eq!(app_state.params().top_p, None);
    }
}
