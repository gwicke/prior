//! `/reasoning` level → provider-specific `additional_params` mapping.
//!
//! `rig` 0.28 has no typed reasoning-level request API; the universal path is
//! core `CompletionRequest::additional_params` (a `serde_json::Value` merged
//! into the request body). `AgentBuilder` exposes `additional_params`, so the
//! REPL rebuilds the rig `Agent` via `AgentBuilder::additional_params` whenever
//! the level changes (see `agent.rs`). Mapping is best-effort: providers that
//! don't support reasoning return `None` and we fall back to store + display.
//!
//! Note the two vocabularies that meet here and must not be merged: our levels
//! are always `off | low | med | high | max`, while the *wire* spelling is
//! whatever the provider calls them (`thinking: {type: enabled}` for Anthropic,
//! `reasoning_effort` for OpenAI, `reasoning: {effort}` for OpenRouter). The
//! `json!` payloads below are provider contracts and keep their own key names
//! regardless of what the command is called.

use serde_json::{json, Value};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningLevel {
    Off,
    Low,
    Med,
    High,
    Max,
}

impl FromStr for ReasoningLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" | "false" => Ok(ReasoningLevel::Off),
            "low" | "min" | "1" => Ok(ReasoningLevel::Low),
            "med" | "medium" | "2" => Ok(ReasoningLevel::Med),
            "high" | "3" => Ok(ReasoningLevel::High),
            "max" | "xhigh" | "x-high" | "4" => Ok(ReasoningLevel::Max),
            other => Err(format!(
                "unknown reasoning level: '{}' (use off | low | med | high | max)",
                other
            )),
        }
    }
}

impl ReasoningLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningLevel::Off => "off",
            ReasoningLevel::Low => "low",
            ReasoningLevel::Med => "med",
            ReasoningLevel::High => "high",
            ReasoningLevel::Max => "max",
        }
    }

    /// Position on the internal ladder. Used to pick the closest supported
    /// effort without ever exceeding what the user asked for.
    fn rank(self) -> u8 {
        match self {
            ReasoningLevel::Off => 0,
            ReasoningLevel::Low => 1,
            ReasoningLevel::Med => 2,
            ReasoningLevel::High => 3,
            ReasoningLevel::Max => 4,
        }
    }
}

/// The set of reasoning efforts a model accepts, as published by the catalog
/// (`data[].reasoning.supported_efforts`). `Unknown` (the empty list) means we
/// have no data for this model, so the static per-provider spelling is sent
/// unchanged — today's behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReasoningSupport {
    levels: Vec<String>,
}

impl ReasoningSupport {
    /// Build from the catalog's level list. Any spelling we don't recognize is
    /// dropped, so a provider inventing a new effort degrades to "unknown"
    /// rather than to a wrong choice.
    pub fn new(levels: Vec<String>) -> Self {
        ReasoningSupport {
            levels: levels
                .into_iter()
                .filter(|l| rank_of(l).is_some())
                .collect(),
        }
    }

    /// No data for this model: never remap.
    pub fn unknown() -> Self {
        ReasoningSupport::default()
    }

    /// True when the catalog told us what this model supports.
    pub fn is_known(&self) -> bool {
        !self.levels.is_empty()
    }

    /// The raw published levels, for display in the model header.
    pub fn levels(&self) -> &[String] {
        &self.levels
    }

    /// Map an internal level onto an effort this model actually accepts.
    ///
    /// The rule is "never more than asked for": keep the effort if the list has
    /// it, else take the supported level with the highest rank at or below the
    /// target, else the lowest supported. Under-reasoning is visible in the
    /// header and costs the user a keystroke to fix; silently escalating would
    /// cost latency and tokens they did not ask to spend.
    ///
    /// Returns `None` when the model supports nothing (or we have no data),
    /// which leaves the caller's static spelling in place.
    pub fn resolve(&self, level: ReasoningLevel) -> Option<String> {
        if level == ReasoningLevel::Off || !self.is_known() {
            return None;
        }
        // `off`-like entries rank 0 and are filtered out of non-off requests.
        let candidates: Vec<(&str, u8)> = self
            .levels
            .iter()
            .filter_map(|l| rank_of(l).filter(|r| *r > 0).map(|r| (l.as_str(), r)))
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let target = level.rank();
        // An exact rank match keeps the model's own spelling. A model that
        // publishes two spellings for one rank (`["max","xhigh"]`) accepts
        // either, so the first published wins — deterministic, and never a
        // spelling the model did not list.
        if let Some((exact, _)) = candidates.iter().find(|(_, r)| *r == target) {
            return Some((*exact).to_string());
        }
        // Highest supported that does not exceed the target…
        if let Some((best, _)) = candidates
            .iter()
            .filter(|(_, r)| *r <= target)
            .max_by_key(|(_, r)| *r)
        {
            return Some((*best).to_string());
        }
        // …else the lowest supported, since everything outranks the target.
        candidates
            .iter()
            .min_by_key(|(_, r)| *r)
            .map(|(l, _)| (*l).to_string())
    }
}

/// Rank a provider's effort spelling against the internal ladder. `None` for
/// anything unrecognized; the off-family (`none`/`disabled`) ranks 0 and is
/// filtered out of non-off requests.
fn rank_of(spelling: &str) -> Option<u8> {
    match spelling.trim().to_ascii_lowercase().as_str() {
        "none" | "off" | "disabled" => Some(0),
        "low" | "minimal" | "min" => Some(1),
        "med" | "medium" => Some(2),
        "high" => Some(3),
        "max" | "xhigh" | "x-high" => Some(4),
        _ => None,
    }
}

/// The request-shaping knobs picocode sends per turn. Every field is optional
/// and `None` means "unspecified — leave the provider's own default alone",
/// which is the shape of a model the user never configured.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelParams {
    /// `None` = off. Never reaches the provider.
    pub reasoning: Option<ReasoningLevel>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
}

impl ModelParams {
    /// True when nothing is set — the shape of a model with no per-model
    /// config and no live edits.
    pub fn is_empty(&self) -> bool {
        self.reasoning.is_none() && self.temperature.is_none() && self.top_p.is_none()
    }

    /// The level as a plain string, for the session layer and the `ReplCtx`
    /// marker. The session journal keeps its own `thinking_level: String`
    /// vocabulary (an on-disk compatibility surface), so this is the one
    /// place the two types meet.
    pub fn reasoning_str(&self) -> Option<String> {
        self.reasoning.map(|l| l.as_str().to_string())
    }
}

/// Map a reasoning level to provider-specific `additional_params`.
/// Returns `None` when reasoning is `Off` or the provider is unsupported.
pub fn mapping(provider: &str, level: ReasoningLevel) -> Option<Value> {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => {
            if level == ReasoningLevel::Off {
                None
            } else {
                Some(json!({"thinking": {"type": "enabled", "budget_tokens": 1024}}))
            }
        }
        "openai" => {
            if level == ReasoningLevel::Off {
                None
            } else {
                let effort = match level {
                    ReasoningLevel::Low => "low",
                    ReasoningLevel::Med => "medium",
                    ReasoningLevel::High => "high",
                    ReasoningLevel::Max => "max",
                    ReasoningLevel::Off => unreachable!(),
                };
                Some(json!({"reasoning_effort": effort}))
            }
        }
        "openrouter" => {
            let effort = match level {
                ReasoningLevel::Off => "none",
                ReasoningLevel::Low => "low",
                ReasoningLevel::Med => "medium",
                ReasoningLevel::High => "high",
                ReasoningLevel::Max => "max",
            };
            Some(json!({"reasoning": {"effort": effort}}))
        }
        "deepseek" | "xiaomimimo" => {
            // Like openrouter, but with extra "thinking: {type: "enabled|disabled" }" object
            let thinking_type = if level == ReasoningLevel::Off {
                "disabled"
            } else {
                "enabled"
            };
            // Xiaomimimo does not officially support reasoning_effort, but ignores it
            let effort = match level {
                ReasoningLevel::Off => "none", // Should technically be omitted, but probably fine
                ReasoningLevel::Low => "low",
                ReasoningLevel::Med => "medium", // For DS 4.1, service maps "medium" to "high"
                ReasoningLevel::High => "high",
                ReasoningLevel::Max => "max",
            };
            Some(json!({"thinking": {"type": thinking_type}, "reasoning_effort": effort}))
        }
        _ => None,
    }
}

// `mapping_opt` (a string -> `additional_params` helper) used to live here as
// `create_agent`'s entry point. It is gone: `additional_params` takes the typed
// `ModelParams` instead, and a string-typed entry point would be a way to skip
// the composed producer and lose the top_p / remap handling.

/// Rewrite a provider payload's reasoning effort through the model's published
/// level list, when the provider spells its effort the way we can rank it.
///
/// Only the reasoning keys are touched, so each provider keeps its own shape
/// (`reasoning: {effort}` for OpenRouter, a bare `reasoning_effort` for
/// OpenAI, `reasoning_effort` beside `thinking` for DeepSeek). An effort we
/// cannot rank leaves the payload untouched rather than guessing.
fn remap_effort(payload: &mut Value, support: &ReasoningSupport) {
    if !support.is_known() {
        return;
    }
    // `reasoning: {effort}` — OpenRouter and friends.
    if let Some(effort) = payload
        .get_mut("reasoning")
        .and_then(|r| r.get_mut("effort"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
    {
        if let Some(mapped) = resolve_spelling(&effort, support) {
            if let Some(slot) = payload
                .get_mut("reasoning")
                .and_then(|r| r.get_mut("effort"))
            {
                *slot = Value::String(mapped);
            }
        }
        return;
    }
    // A bare `reasoning_effort` — OpenAI, DeepSeek, xiaomimimo.
    if let Some(effort) = payload
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    {
        if let Some(mapped) = resolve_spelling(&effort, support) {
            payload["reasoning_effort"] = Value::String(mapped);
        }
    }
}

/// Rank a backend's own effort spelling and resolve it through `support`.
/// Returns `None` when the spelling is unrecognized (leave it alone) or when
/// the model's list does not contain it.
fn resolve_spelling(effort: &str, support: &ReasoningSupport) -> Option<String> {
    let level = match rank_of(effort)? {
        0 => ReasoningLevel::Off,
        1 => ReasoningLevel::Low,
        2 => ReasoningLevel::Med,
        3 => ReasoningLevel::High,
        _ => ReasoningLevel::Max,
    };
    // `Off` is not remapped: the provider's own off spelling is the whole
    // payload's job (e.g. `thinking: {type: disabled}`), not a level choice.
    if level == ReasoningLevel::Off {
        return None;
    }
    support.resolve(level)
}

/// The provider-specific `additional_params` for one model.
///
/// Composes the two JSON-carried knobs. `temperature` is deliberately absent:
/// it rides the typed `CompletionRequest.temperature` field (rig maps that per
/// provider for us), so putting it here too would risk two spellings for one
/// knob. `top_p` has no typed field anywhere, and Gemini needs it nested under
/// `generationConfig` — a flat key there is forwarded verbatim to the wire and
/// rejected.
pub fn additional_params(
    provider: &str,
    params: &ModelParams,
    support: &ReasoningSupport,
) -> Option<Value> {
    let mut out = serde_json::Map::new();

    if let Some(level) = params.reasoning {
        if let Some(mut payload) = mapping(provider, level) {
            remap_effort(&mut payload, support);
            if let Some(obj) = payload.as_object() {
                out.extend(obj.clone());
            }
        }
    }

    if let Some(v) = params.top_p {
        if is_gemini(provider) {
            out.insert("generationConfig".to_string(), json!({ "topP": v }));
        } else {
            out.insert("top_p".to_string(), json!(v));
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

/// Gemini and Google are the one family whose `additional_params` is parsed
/// into a typed struct rather than flattened into the body, so their sampling
/// knobs live under `generationConfig` with camelCase keys.
fn is_gemini(provider: &str) -> bool {
    matches!(provider.to_ascii_lowercase().as_str(), "gemini" | "google")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_level_variants() {
        assert_eq!(
            "off".parse::<ReasoningLevel>().unwrap(),
            ReasoningLevel::Off
        );
        assert_eq!(
            "LOW".parse::<ReasoningLevel>().unwrap(),
            ReasoningLevel::Low
        );
        assert_eq!(
            "medium".parse::<ReasoningLevel>().unwrap(),
            ReasoningLevel::Med
        );
        assert_eq!("3".parse::<ReasoningLevel>().unwrap(), ReasoningLevel::High);
        assert_eq!(
            "max".parse::<ReasoningLevel>().unwrap(),
            ReasoningLevel::Max
        );
        assert!("bogus".parse::<ReasoningLevel>().is_err());
    }

    #[test]
    fn off_maps_to_none_almost_everywhere() {
        assert!(mapping("anthropic", ReasoningLevel::Off).is_none());
        assert!(mapping("openai", ReasoningLevel::Off).is_none());
        assert!(mapping("groq", ReasoningLevel::Off).is_none());
    }

    #[test]
    fn anthropic_uses_thinking_block() {
        assert_eq!(
            mapping("anthropic", ReasoningLevel::Low),
            Some(json!({"thinking": {"type": "enabled", "budget_tokens": 1024}}))
        );
    }

    #[test]
    fn openai_reasoning_effort() {
        assert_eq!(
            mapping("openai", ReasoningLevel::High),
            Some(json!({"reasoning_effort": "high"}))
        );
    }

    #[test]
    fn openrouter_reasoning_effort() {
        assert_eq!(
            mapping("openrouter", ReasoningLevel::Med),
            Some(json!({"reasoning": {"effort": "medium"}}))
        );
    }

    #[test]
    fn unsupported_provider_falls_back_to_none() {
        assert!(mapping("groq", ReasoningLevel::High).is_none());
        assert!(mapping("ollama", ReasoningLevel::Low).is_none());
    }

    #[test]
    fn xiaomimimo_thinking_enabled_disabled() {
        // Off maps to disabled
        assert_eq!(
            mapping("xiaomimimo", ReasoningLevel::Off),
            Some(json!({"thinking": {"type": "disabled"}, "reasoning_effort": "none"}))
        );
        // Any "on" level maps to enabled
        assert_eq!(
            mapping("xiaomimimo", ReasoningLevel::Low),
            Some(json!({"thinking": {"type": "enabled"}, "reasoning_effort": "low"}))
        );
        assert_eq!(
            mapping("xiaomimimo", ReasoningLevel::Med),
            Some(json!({"thinking": {"type": "enabled"}, "reasoning_effort": "medium"}))
        );
        assert_eq!(
            mapping("xiaomimimo", ReasoningLevel::High),
            Some(json!({"thinking": {"type": "enabled"}, "reasoning_effort": "high"}))
        );
        assert_eq!(
            mapping("xiaomimimo", ReasoningLevel::Max),
            Some(json!({"thinking": {"type": "enabled"}, "reasoning_effort": "max"}))
        );
    }

    /// The string-parsing behavior `mapping_opt` used to own now lives in
    /// `FromStr` plus the `.parse().ok()` call sites, so pin both halves:
    /// a bogus level is rejected by the parser, and a level that never
    /// reaches the parser produces no payload.
    #[test]
    fn an_unparsable_level_is_rejected_before_it_becomes_params() {
        assert!("bogus".parse::<ReasoningLevel>().is_err());
        let parsed: Option<ReasoningLevel> = Some("bogus").and_then(|s| s.parse().ok());
        assert!(parsed.is_none());
        assert!(additional_params(
            "openrouter",
            &ModelParams {
                reasoning: parsed,
                ..Default::default()
            },
            &ReasoningSupport::unknown()
        )
        .is_none());
    }

    // ---- ReasoningSupport: the published effort list drives remapping ----

    /// The two real payloads that motivated the remapper: a model with no
    /// `medium`, and one whose top level is only `high`.
    fn support(levels: &[&str]) -> ReasoningSupport {
        ReasoningSupport::new(levels.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn resolve_keeps_an_exactly_supported_effort() {
        let s = support(&["low", "medium", "high", "max"]);
        assert_eq!(s.resolve(ReasoningLevel::Med).as_deref(), Some("medium"));
        assert_eq!(s.resolve(ReasoningLevel::Max).as_deref(), Some("max"));
    }

    /// Never more than asked for: `med` on a model without `medium` steps
    /// *down* to `high`, never up.
    #[test]
    fn resolve_never_exceeds_the_requested_level() {
        // Real payload: deepseek/deepseek-v4.1-flash.
        let s = support(&["max", "high", "low"]);
        assert_eq!(s.resolve(ReasoningLevel::Med).as_deref(), Some("low"));
        assert_eq!(s.resolve(ReasoningLevel::High).as_deref(), Some("high"));
        // Real payload: tencent/hy4-preview tops out at high.
        let t = support(&["high", "low", "none"]);
        assert_eq!(t.resolve(ReasoningLevel::Max).as_deref(), Some("high"));
    }

    /// When everything supported outranks the target, take the lowest rather
    /// than escalating — `low` asked for on a `high`-only model yields `high`,
    /// which is the one case where the user gets more than they asked for,
    /// and it is better than sending an effort the model rejects.
    #[test]
    fn resolve_falls_back_to_the_lowest_supported() {
        let s = support(&["high", "max"]);
        assert_eq!(s.resolve(ReasoningLevel::Low).as_deref(), Some("high"));
        assert_eq!(s.resolve(ReasoningLevel::Med).as_deref(), Some("high"));
    }

    /// `none` in the list means "off"; it is never chosen for a non-off
    /// request, and `Off` is left to the provider's own spelling.
    #[test]
    fn resolve_ignores_the_off_family_and_leaves_off_to_the_provider() {
        let s = support(&["high", "low", "none"]);
        assert_eq!(s.resolve(ReasoningLevel::Off), None);
        assert_eq!(s.resolve(ReasoningLevel::High).as_deref(), Some("high"));
        // A list of nothing but off-spellings is not usable for a real level.
        let only_none = support(&["none"]);
        assert_eq!(only_none.resolve(ReasoningLevel::High), None);
    }

    /// `xhigh` is how some models spell our top level; the remapper sends the
    /// model's own spelling back, not ours. A model that publishes only
    /// `xhigh` must never receive our `max`.
    #[test]
    fn resolve_returns_the_models_own_spelling() {
        let s = support(&["xhigh", "high", "medium", "low"]);
        assert_eq!(s.resolve(ReasoningLevel::Max).as_deref(), Some("xhigh"));
        assert_eq!(s.resolve(ReasoningLevel::Med).as_deref(), Some("medium"));
    }

    /// When a model publishes two spellings for the same rank (`["max",
    /// "xhigh", …]`), either is acceptable to it and the first published wins —
    /// deterministic, and never a spelling the model did not list.
    #[test]
    fn resolve_ties_break_to_the_first_published_spelling() {
        let s = support(&["max", "xhigh", "high", "medium", "low"]);
        assert_eq!(s.resolve(ReasoningLevel::Max).as_deref(), Some("max"));
        let reversed = support(&["xhigh", "max", "high", "low"]);
        assert_eq!(
            reversed.resolve(ReasoningLevel::Max).as_deref(),
            Some("xhigh")
        );
    }

    /// Unknown support means no remapping at all — the static per-provider
    /// spelling goes out untouched.
    #[test]
    fn unknown_support_never_remaps() {
        let s = ReasoningSupport::unknown();
        assert!(!s.is_known());
        assert_eq!(s.resolve(ReasoningLevel::Med), None);
        // A list of only unrecognized spellings is filtered to unknown.
        let junk = support(&["ultra", "extreme"]);
        assert!(!junk.is_known());
        assert_eq!(junk.resolve(ReasoningLevel::High), None);
    }

    // ---- additional_params: the composed producer ----

    fn params(
        reasoning: Option<ReasoningLevel>,
        temperature: Option<f64>,
        top_p: Option<f64>,
    ) -> ModelParams {
        ModelParams {
            reasoning,
            temperature,
            top_p,
        }
    }

    /// An untouched model sends nothing at all: no key, no `null`, no
    /// provider default we merely know about.
    #[test]
    fn additional_params_is_none_when_nothing_is_set() {
        let none = ReasoningSupport::unknown();
        assert!(additional_params("openrouter", &ModelParams::default(), &none).is_none());
        assert!(additional_params("openai", &params(None, None, None), &none).is_none());
    }

    /// `temperature` is deliberately absent from this payload — it rides the
    /// typed rig field, so it must not also appear here.
    #[test]
    fn additional_params_never_carries_temperature() {
        let s = ReasoningSupport::unknown();
        let out = additional_params("openrouter", &params(None, Some(0.7), Some(0.9)), &s)
            .expect("top_p should produce a payload");
        assert_eq!(out, json!({"top_p": 0.9}));
    }

    /// Flat for the OpenAI-compatible family, nested + camelCase for Gemini,
    /// which parses `additional_params` into a typed struct and would forward
    /// a flat `top_p` straight to the wire.
    #[test]
    fn additional_params_nests_top_p_for_gemini_only() {
        let s = ReasoningSupport::unknown();
        let p = params(None, None, Some(0.95));
        assert_eq!(
            additional_params("openrouter", &p, &s),
            Some(json!({"top_p": 0.95}))
        );
        assert_eq!(
            additional_params("anthropic", &p, &s),
            Some(json!({"top_p": 0.95}))
        );
        assert_eq!(
            additional_params("deepseek", &p, &s),
            Some(json!({"top_p": 0.95}))
        );
        assert_eq!(
            additional_params("gemini", &p, &s),
            Some(json!({"generationConfig": {"topP": 0.95}}))
        );
        assert_eq!(
            additional_params("google", &p, &s),
            Some(json!({"generationConfig": {"topP": 0.95}}))
        );
    }

    /// Reasoning and top_p compose into one object, each keeping its
    /// provider's own shape.
    #[test]
    fn additional_params_composes_reasoning_with_top_p() {
        let s = ReasoningSupport::unknown();
        assert_eq!(
            additional_params(
                "openrouter",
                &params(Some(ReasoningLevel::High), None, Some(0.8)),
                &s
            ),
            Some(json!({"reasoning": {"effort": "high"}, "top_p": 0.8}))
        );
    }

    /// The end-to-end case the remapper exists for: `/reasoning med` on a
    /// model that publishes no `medium` sends an effort it accepts, keeping
    /// the provider's own payload shape.
    #[test]
    fn additional_params_remaps_a_level_the_model_lacks() {
        // deepseek/deepseek-v4.1-flash: ["max","high","low"].
        let s = support(&["max", "high", "low"]);
        assert_eq!(
            additional_params(
                "openrouter",
                &params(Some(ReasoningLevel::Med), None, None),
                &s
            ),
            Some(json!({"reasoning": {"effort": "low"}}))
        );
        // Same list, OpenAI shape (a bare `reasoning_effort`).
        assert_eq!(
            additional_params("openai", &params(Some(ReasoningLevel::Med), None, None), &s),
            Some(json!({"reasoning_effort": "low"}))
        );
        // DeepSeek shape: the effort is remapped, the `thinking` block is not.
        assert_eq!(
            additional_params(
                "deepseek",
                &params(Some(ReasoningLevel::Med), None, None),
                &s
            ),
            Some(json!({
                "thinking": {"type": "enabled"},
                "reasoning_effort": "low"
            }))
        );
    }

    /// With no support data the level goes out verbatim — the pre-P1
    /// behavior, and the one non-OpenRouter models keep.
    #[test]
    fn additional_params_leaves_the_level_alone_without_support_data() {
        let s = ReasoningSupport::unknown();
        assert_eq!(
            additional_params(
                "openrouter",
                &params(Some(ReasoningLevel::Med), None, None),
                &s
            ),
            Some(json!({"reasoning": {"effort": "medium"}}))
        );
    }

    /// Anthropic has no effort vocabulary to remap: its payload is a
    /// `thinking` block with a token budget, so a support list must not
    /// disturb it.
    #[test]
    fn additional_params_leaves_anthropics_thinking_block_alone() {
        let s = support(&["low", "medium", "high"]);
        assert_eq!(
            additional_params(
                "anthropic",
                &params(Some(ReasoningLevel::Med), None, None),
                &s
            ),
            Some(json!({"thinking": {"type": "enabled", "budget_tokens": 1024}}))
        );
    }

    #[test]
    fn model_params_reports_emptiness() {
        assert!(ModelParams::default().is_empty());
        assert!(!params(None, Some(0.5), None).is_empty());
        assert!(!params(Some(ReasoningLevel::Off), None, None).is_empty());
    }
}
