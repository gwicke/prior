//! `/thinking` level → provider-specific `additional_params` mapping.
//!
//! `rig` 0.28 has no typed thinking-level request API; the universal path is
//! core `CompletionRequest::additional_params` (a `serde_json::Value` merged
//! into the request body). `AgentBuilder` exposes `additional_params`, so the
//! REPL rebuilds the rig `Agent` via `AgentBuilder::additional_params` whenever
//! the level changes (see `agent.rs`). Mapping is best-effort: providers that
//! don't support thinking return `None` and we fall back to store + display.

use serde_json::{json, Value};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingLevel {
    Off,
    Low,
    Med,
    High,
    Max,
}

impl FromStr for ThinkingLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" | "false" => Ok(ThinkingLevel::Off),
            "low" | "min" | "1" => Ok(ThinkingLevel::Low),
            "med" | "medium" | "2" => Ok(ThinkingLevel::Med),
            "high" | "3" => Ok(ThinkingLevel::High),
            "max" | "xhigh" | "x-high" | "4" => Ok(ThinkingLevel::Max),
            other => Err(format!(
                "unknown thinking level: '{}' (use off | low | med | high)",
                other
            )),
        }
    }
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Med => "med",
            ThinkingLevel::High => "high",
            ThinkingLevel::Max => "max",
        }
    }
}

/// Map a thinking level to provider-specific `additional_params`.
/// Returns `None` when thinking is `Off` or the provider is unsupported.
pub fn mapping(provider: &str, level: ThinkingLevel) -> Option<Value> {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => {
            if level == ThinkingLevel::Off {
                None
            } else {
                Some(json!({"thinking": {"type": "enabled", "budget_tokens": 1024}}))
            }
        }
        "openai" => {
            if level == ThinkingLevel::Off {
                None
            } else {
                let effort = match level {
                    ThinkingLevel::Low => "low",
                    ThinkingLevel::Med => "medium",
                    ThinkingLevel::High => "high",
                    ThinkingLevel::Max => "max",
                    ThinkingLevel::Off => unreachable!(),
                };
                Some(json!({"reasoning_effort": effort}))
            }
        }
        "openrouter" => {
            let effort = match level {
                ThinkingLevel::Off => "none",
                ThinkingLevel::Low => "low",
                ThinkingLevel::Med => "medium",
                ThinkingLevel::High => "high",
                ThinkingLevel::Max => "max",
            };
            Some(json!({"reasoning": {"effort": effort}}))
        }
        "deepseek" | "xiaomimimo" => {
            // Like openrouter, but with extra "thinking: {type: "enabled|disabled" }" object
            let thinking_type = if level == ThinkingLevel::Off {
                "disabled"
            } else {
                "enabled"
            };
            // Xiaomimimo does not officially support reasoning_effort, but ignores it
            let effort = match level {
                ThinkingLevel::Off => "none", // Should technically be omitted, but probably fine
                ThinkingLevel::Low => "low",
                ThinkingLevel::Med => "medium", // For DS 4.1, service maps "medium" to "high"
                ThinkingLevel::High => "high",
                ThinkingLevel::Max => "max",
            };
            Some(json!({"thinking": {"type": thinking_type}, "reasoning_effort": effort}))
        }
        _ => None,
    }
}

/// Resolve an optional level string (from `AgentConfig::thinking_level`) to
/// provider-specific `additional_params`. Returns `None` when off/unsupported
/// or the string fails to parse.
pub fn mapping_opt(provider: &str, level: Option<&str>) -> Option<Value> {
    let parsed = level.and_then(|s| s.parse::<ThinkingLevel>().ok())?;
    mapping(provider, parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_level_variants() {
        assert_eq!("off".parse::<ThinkingLevel>().unwrap(), ThinkingLevel::Off);
        assert_eq!("LOW".parse::<ThinkingLevel>().unwrap(), ThinkingLevel::Low);
        assert_eq!(
            "medium".parse::<ThinkingLevel>().unwrap(),
            ThinkingLevel::Med
        );
        assert_eq!("3".parse::<ThinkingLevel>().unwrap(), ThinkingLevel::High);
        assert_eq!("max".parse::<ThinkingLevel>().unwrap(), ThinkingLevel::Max);
        assert!("bogus".parse::<ThinkingLevel>().is_err());
    }

    #[test]
    fn off_maps_to_none_almost_everywhere() {
        assert!(mapping("anthropic", ThinkingLevel::Off).is_none());
        assert!(mapping("openai", ThinkingLevel::Off).is_none());
        assert!(mapping("groq", ThinkingLevel::Off).is_none());
    }

    #[test]
    fn anthropic_uses_thinking_block() {
        assert_eq!(
            mapping("anthropic", ThinkingLevel::Low),
            Some(json!({"thinking": {"type": "enabled", "budget_tokens": 1024}}))
        );
    }

    #[test]
    fn openai_reasoning_effort() {
        assert_eq!(
            mapping("openai", ThinkingLevel::High),
            Some(json!({"reasoning_effort": "high"}))
        );
    }

    #[test]
    fn openrouter_reasoning_effort() {
        assert_eq!(
            mapping("openrouter", ThinkingLevel::Med),
            Some(json!({"reasoning": {"effort": "medium"}}))
        );
    }

    #[test]
    fn unsupported_provider_falls_back_to_none() {
        assert!(mapping("groq", ThinkingLevel::High).is_none());
        assert!(mapping("ollama", ThinkingLevel::Low).is_none());
    }

    #[test]
    fn xiaomimimo_thinking_enabled_disabled() {
        // Off maps to disabled
        assert_eq!(
            mapping("xiaomimimo", ThinkingLevel::Off),
            Some(json!({"thinking": {"type": "disabled"}, "reasoning_effort": "none"}))
        );
        // Any "on" level maps to enabled
        assert_eq!(
            mapping("xiaomimimo", ThinkingLevel::Low),
            Some(json!({"thinking": {"type": "enabled"}, "reasoning_effort": "low"}))
        );
        assert_eq!(
            mapping("xiaomimimo", ThinkingLevel::Med),
            Some(json!({"thinking": {"type": "enabled"}, "reasoning_effort": "medium"}))
        );
        assert_eq!(
            mapping("xiaomimimo", ThinkingLevel::High),
            Some(json!({"thinking": {"type": "enabled"}, "reasoning_effort": "high"}))
        );
        assert_eq!(
            mapping("xiaomimimo", ThinkingLevel::Max),
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
