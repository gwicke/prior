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

/// Resolve an optional level string (from `AgentConfig::reasoning_level`) to
/// provider-specific `additional_params`. Returns `None` when off/unsupported
/// or the string fails to parse.
pub fn mapping_opt(provider: &str, level: Option<&str>) -> Option<Value> {
    let parsed = level.and_then(|s| s.parse::<ReasoningLevel>().ok())?;
    mapping(provider, parsed)
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

    #[test]
    fn mapping_opt_handles_none_and_bogus() {
        assert!(mapping_opt("anthropic", None).is_none());
        assert!(mapping_opt("anthropic", Some("bogus")).is_none());
        assert_eq!(
            mapping_opt("openrouter", Some("high")),
            Some(json!({"reasoning": {"effort": "high"}}))
        );
    }
}
