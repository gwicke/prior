//! The request-shaping half of a turn, recomposed at every call boundary.
//!
//! Sampling used to be baked once, when the agent was built, and the turn's
//! per-prompt overrides were layered on at dispatch. Both are too early now that
//! a turn can last several model calls and the user can change the knobs while
//! it runs (`doc/261030_rig_direct_drive.md` §5):
//!
//! - a `/reasoning`, `/temperature` or `/top_p` **menu** commit mid-turn writes
//!   the live params, so it must reach the *next* request of the same turn;
//! - the same lines **trailed on a steering message** apply to the rest of that
//!   user turn only, and are exactly [`TurnOverrides`].
//!
//! So the composition is a function of (live params, this turn's overrides), read
//! fresh at each `CallModel`. That is why it lives on the drive loop rather than
//! in a `RequestPatch`: `prepare_request` merges an `additional_params` patch
//! key-wise, so a cleared knob could not be removed (an anthropic `thinking`
//! block would survive a switch to `off`), and the patch has no vocabulary for
//! "this value, exactly".
//!
//! Writing the spec instead is exact and still one path: `prepare_request` reads
//! the spec per call, and nothing else writes these two fields. With no app
//! state — piped, batch, a summarizer — the live params are the agent's own
//! baked ones, so the recomposition is the identity and those paths are
//! byte-for-byte what they were.

use std::sync::{Arc, Mutex};

use rig_agent::run::spec::RunSpec;

use crate::agent::build::request_additional_params;
use crate::params::ModelParams;
use crate::turn_overrides::TurnOverrides;

pub(crate) struct Sampling {
    /// The live params' source. `None` where nothing can change them mid-turn,
    /// which is every non-interactive path: the baked params below are then the
    /// answer.
    app_state: Option<Arc<crate::app_state::AppState>>,
    /// What this agent was built with, for the `None` case above.
    baked: ModelParams,
    /// The wire pair and routing, as [`request_additional_params`] spells them.
    provider: String,
    model: String,
    routing: Option<crate::model_settings::Routing>,
    /// This turn's overrides: the lines typed on the prompt, plus any trailed on
    /// a steer, later winning. `Mutex` because a steer is scheduled from the key
    /// pump and consumed by the loop, both on the same thread but at different
    /// times — the lock is never held across an await.
    overrides: Mutex<TurnOverrides>,
}

impl Sampling {
    pub(crate) fn new(
        app_state: Option<Arc<crate::app_state::AppState>>,
        baked: ModelParams,
        provider: String,
        model: String,
        routing: Option<crate::model_settings::Routing>,
        overrides: TurnOverrides,
    ) -> Self {
        Self {
            app_state,
            baked,
            provider,
            model,
            routing,
            overrides: Mutex::new(overrides),
        }
    }

    /// This turn's params, as of right now: the live ones with every override
    /// this turn has accumulated applied.
    pub(crate) fn params(&self) -> ModelParams {
        let live = match &self.app_state {
            Some(app) => app.params(),
            None => self.baked,
        };
        self.overrides().apply(&live)
    }

    /// Layer a steer's override lines on top of what the turn already carries.
    /// Last wins, per key.
    pub(crate) fn note_overrides(&self, later: TurnOverrides) {
        let mut slot = self
            .overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = slot.overlay(later);
    }

    /// Write this turn's params into `spec`, replacing whatever was there.
    ///
    /// Replacing rather than `or`-ing is the point: a knob the user turned *off*
    /// has to disappear, and a fallback to the previously composed value would
    /// quietly keep it. The one composition is
    /// [`request_additional_params`], so an override turn cannot produce a
    /// different request shape than a plain one.
    pub(crate) fn apply(&self, spec: &mut RunSpec) {
        let params = self.params();
        spec.temperature = params.temperature;
        spec.additional_params =
            request_additional_params(&self.provider, &self.model, &params, self.routing.as_ref());
    }

    fn overrides(&self) -> TurnOverrides {
        *self
            .overrides
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampling(overrides: TurnOverrides) -> (Sampling, Arc<crate::app_state::AppState>) {
        let app = crate::app_state::AppState::for_test(test_settings());
        let sampling = Sampling::new(
            Some(app.clone()),
            ModelParams::default(),
            "anthropic".to_string(),
            "claude-sonnet-4-5".to_string(),
            None,
            overrides,
        );
        (sampling, app)
    }

    fn test_settings() -> crate::settings::SettingsStore {
        crate::settings::SettingsStore {
            default_model: "anthropic:claude-sonnet-4-5".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 10,
            default_persona: None,
            default_reasoning_level: None,
            models: Default::default(),
            show_reasoning: "content".into(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::from("default prompt"),
            path: None,
        }
    }

    /// The requirement the module exists for: a menu commit during a turn
    /// reaches the *next* request of that same turn. Reading the live params at
    /// each boundary is what makes that fall out with no further wiring.
    #[test]
    fn a_mid_turn_menu_commit_reaches_the_next_composition() {
        let (sampling, app) = sampling(TurnOverrides::default());
        app.set_param(|p| p.temperature = Some(0.9));
        assert_eq!(sampling.params().temperature, Some(0.9));
    }

    /// A steer's lines apply for the rest of the turn, and the later write wins
    /// over the prompt's.
    #[test]
    fn a_steers_overrides_layer_over_the_turns() {
        let prompt = TurnOverrides {
            temperature: Some(0.2),
            ..Default::default()
        };
        let (sampling, _app) = sampling(prompt);
        sampling.note_overrides(TurnOverrides {
            reasoning: Some(crate::params::ReasoningLevel::Low),
            ..Default::default()
        });
        assert_eq!(sampling.params().temperature, Some(0.2));
        assert_eq!(
            sampling.params().reasoning,
            Some(crate::params::ReasoningLevel::Low)
        );
    }

    /// Composing writes the *current* value, not a fallback to whatever was
    /// there before: a knob the user cleared must disappear from the request.
    #[test]
    fn composition_replaces_rather_than_falls_back() {
        let (sampling, app) = sampling(TurnOverrides::default());
        app.set_param(|p| p.temperature = Some(0.4));
        let mut spec = RunSpec::new();
        spec.temperature = Some(0.4);
        spec.additional_params = Some(serde_json::json!({"thinking": {"type": "enabled"}}));

        app.set_param(|p| {
            p.temperature = None;
            p.reasoning = Some(crate::params::ReasoningLevel::Off);
        });
        sampling.apply(&mut spec);
        assert_eq!(spec.temperature, None, "a cleared temperature must clear");
        assert_eq!(
            spec.additional_params, None,
            "reasoning off carries no additional params"
        );
    }

    /// Without an app state there is nothing to read, and the baked params are
    /// the answer — the piped/batch and summarizer paths.
    #[test]
    fn no_app_state_falls_back_to_the_baked_params() {
        let sampling = Sampling::new(
            None,
            ModelParams {
                temperature: Some(0.15),
                ..Default::default()
            },
            "openai".to_string(),
            "gpt-4o".to_string(),
            None,
            TurnOverrides::default(),
        );
        let mut spec = RunSpec::new();
        sampling.apply(&mut spec);
        assert_eq!(spec.temperature, Some(0.15));
    }
}
