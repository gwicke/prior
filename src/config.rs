use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Config {
    #[serde(default)]
    pub agent_prompt: Option<String>,
    #[serde(default)]
    pub agent_prompt_file: Option<String>,
    #[serde(default)]
    pub tool_config: HashMap<String, ToolSettings>,
    #[serde(default)]
    pub recipes: HashMap<String, Recipe>,
    // --- Managed settings (persisted by the /settings menu; see doc/settings_menu.md) ---
    /// Default model as `provider:model` (used for new sessions).
    #[serde(default)]
    pub default_model: Option<String>,
    /// Tool execution policy: `yolo` (no prompts) or `confirm` (ask).
    #[serde(default)]
    pub security_mode: Option<String>,
    /// Max tool calls per prompt/turn.
    #[serde(default)]
    pub tool_call_limit: Option<usize>,
    /// Persona for new sessions (built-in name or file path); `None` = no persona.
    #[serde(default)]
    pub default_persona: Option<String>,
    /// Default thinking level: `off` | `low` | `med` | `high`.
    #[serde(default)]
    pub default_thinking_level: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct ToolSettings {
    #[serde(default)]
    pub auto_allow: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Recipe {
    pub prompt: Option<String>,
    pub prompt_file: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub persona: Option<String>,
    pub yolo: Option<bool>,
    #[serde(default)]
    pub quiet: bool,
    /// If set, response is treated as error when it matches this regex. Process exits with error.
    #[serde(default)]
    pub error_if: Option<String>,
}

impl Recipe {
    /// Returns true if the response matches the recipe's error_if regex.
    pub fn is_error(&self, response: &str) -> crate::Result<bool> {
        let Some(pattern) = &self.error_if else {
            return Ok(false);
        };
        let re = Regex::new(pattern)?;
        Ok(re.is_match(response))
    }
}

impl Config {
    /// Load config from the given path, or from `picocode.toml` in the current
    /// directory if `path` is `None`.
    pub fn load(path: Option<&str>) -> crate::Result<Self> {
        if let Some(path) = path {
            let p = Path::new(path);
            let content = std::fs::read_to_string(p).map_err(crate::PicocodeError::Io)?;
            let config = toml_edit::de::from_str::<Config>(&content)
                .map_err(|e| crate::PicocodeError::Toml(e.to_string()))?;
            return Ok(config);
        }
        let paths = ["picocode.toml"];
        for path in paths {
            let p = Path::new(path);
            if p.exists() {
                let content = std::fs::read_to_string(p).map_err(crate::PicocodeError::Io)?;
                let config = toml_edit::de::from_str::<Config>(&content)
                    .map_err(|e| crate::PicocodeError::Toml(e.to_string()))?;
                return Ok(config);
            }
        }
        Ok(Config::default())
    }

    pub fn get_bash_auto_allow(&self) -> Vec<String> {
        self.tool_config
            .get("bash")
            .map(|s| s.auto_allow.clone())
            .unwrap_or_default()
    }
}

pub fn read_prompt(prompt: Option<String>, prompt_file: Option<String>) -> crate::Result<Option<String>> {
    if let Some(file_path) = prompt_file {
        let path = Path::new(&file_path);
        let content = std::fs::read_to_string(path).map_err(crate::PicocodeError::Io)?;
        Ok(Some(content))
    } else {
        Ok(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_parses_toml_with_settings_and_recipes() {
        let toml = r#"
# comment preserved on write elsewhere
default_model = "anthropic:claude-3-5-sonnet-20241022"
security_mode = "confirm"
tool_call_limit = 50
default_persona = "architect"
default_thinking_level = "off"

[tool_config.bash]
auto_allow = ["^ls -la"]

[recipes.review-security]
prompt = "Review the codebase."
provider = "anthropic"
model = "claude-3-5-sonnet-20241022"
"#;
        let cfg: Config =
            toml_edit::de::from_str(toml).expect("TOML config with settings should parse");
        assert_eq!(
            cfg.default_model.as_deref(),
            Some("anthropic:claude-3-5-sonnet-20241022")
        );
        assert_eq!(cfg.security_mode.as_deref(), Some("confirm"));
        assert_eq!(cfg.tool_call_limit, Some(50));
        assert_eq!(cfg.default_persona.as_deref(), Some("architect"));
        assert_eq!(cfg.default_thinking_level.as_deref(), Some("off"));
        assert_eq!(cfg.recipes.len(), 1);
        assert!(cfg.tool_config.contains_key("bash"));
    }

    #[test]
    fn load_defaults_when_keys_absent() {
        let cfg: Config =
            toml_edit::de::from_str("recipes = {}").expect("minimal TOML should parse");
        assert!(cfg.default_model.is_none());
        assert!(cfg.security_mode.is_none());
        assert!(cfg.tool_call_limit.is_none());
        assert!(cfg.default_persona.is_none());
        assert!(cfg.default_thinking_level.is_none());
    }
}
