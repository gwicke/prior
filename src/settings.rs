//! Settings registry + persistence for the `/settings` menu.
//!
//! Settings are persisted into the same `picocode.toml` that holds `recipes`
//! (see `doc/settings_menu.md`). Reads go through serde (`toml_edit::de`);
//! writes go through `toml_edit::DocumentMut` so that user comments and the
//! `[recipes.*]` blocks are preserved on every save (YAML cannot do this).
//!
//! All logic here is pure and unit-testable; the REPL only renders the rows
//! produced by [`SettingsStore::overlay_rows`] and commits edits via
//! [`SettingsStore::set_value`].

use std::path::{Path, PathBuf};

/// A managed setting, keyed for selection/editing in the `/settings` overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKey {
    /// `provider:model`.
    DefaultModel,
    /// `yolo` | `confirm`.
    SecurityMode,
    /// positive integer.
    ToolCallLimit,
    /// built-in persona name or file path; empty = none.
    DefaultPersona,
    /// `off` | `low` | `med` | `high`.
    DefaultThinking,
}

/// How a setting's *value* may be completed. Drive a value-autocomplete
/// pop-up in a later phase; not used by the Phase 1 free-text edit.
#[derive(Debug, Clone, Copy)]
pub enum ValueKind {
    Model,
    Enum(&'static [&'static str]),
    Int,
    Persona,
    FreeText,
}

/// Static metadata for one setting (the table's source of truth).
#[derive(Debug, Clone, Copy)]
pub struct SettingMeta {
    pub key: SettingKey,
    pub name: &'static str,
    pub description: &'static str,
    pub kind: ValueKind,
}

/// The centralized, extendable registry of managed settings.
pub static SETTINGS: &[SettingMeta] = &[
    SettingMeta {
        key: SettingKey::DefaultModel,
        name: "default_model",
        description: "Default model as provider:model (used for new sessions)",
        kind: ValueKind::Model,
    },
    SettingMeta {
        key: SettingKey::SecurityMode,
        name: "security_mode",
        description: "Tool execution policy: yolo (no prompts) or confirm (ask)",
        kind: ValueKind::Enum(&["yolo", "confirm"]),
    },
    SettingMeta {
        key: SettingKey::ToolCallLimit,
        name: "tool_call_limit",
        description: "Max tool calls per prompt/turn",
        kind: ValueKind::Int,
    },
    SettingMeta {
        key: SettingKey::DefaultPersona,
        name: "default_persona",
        description: "Persona for new sessions (built-in name or file)",
        kind: ValueKind::Persona,
    },
    SettingMeta {
        key: SettingKey::DefaultThinking,
        name: "default_thinking_level",
        description: "Default thinking level: off | low | med | high",
        kind: ValueKind::Enum(&["off", "low", "med", "high"]),
    },
];

/// In-memory mirror of the five managed settings, plus the file to persist to.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    pub default_model: String,
    pub security_mode: String,
    pub tool_call_limit: usize,
    pub default_persona: Option<String>,
    pub default_thinking_level: String,
    pub path: PathBuf,
}

impl SettingsStore {
    /// Build the store from the *resolved* effective session config, so the
    /// menu shows the actual current values (CLI > file > built-in precedence
    /// is already applied by the caller in `main.rs`).
    pub fn from_agent_config(cfg: &crate::AgentConfig, path: &Path) -> Self {
        SettingsStore {
            default_model: format!("{}:{}", cfg.provider, cfg.model),
            security_mode: if cfg.yolo {
                "yolo".to_string()
            } else {
                "confirm".to_string()
            },
            tool_call_limit: cfg.tool_call_limit,
            default_persona: cfg.persona_name.clone(),
            default_thinking_level: cfg
                .thinking_level
                .clone()
                .unwrap_or_else(|| "off".to_string()),
            path: path.to_path_buf(),
        }
    }

    /// Build the store from a parsed `Config` (file fields, with built-in
    /// fallbacks for anything the file omits).
    pub fn from_config(cfg: &crate::Config, path: &Path) -> Self {
        SettingsStore {
            default_model: cfg.default_model.clone().unwrap_or_default(),
            security_mode: cfg
                .security_mode
                .clone()
                .unwrap_or_else(|| "confirm".to_string()),
            tool_call_limit: cfg.tool_call_limit.unwrap_or(50),
            default_persona: cfg.default_persona.clone(),
            default_thinking_level: cfg
                .default_thinking_level
                .clone()
                .unwrap_or_else(|| "off".to_string()),
            path: path.to_path_buf(),
        }
    }

    /// Load from `path`, falling back to built-in defaults when the file is
    /// absent (the file is only written once a setting is first saved).
    pub fn load(path: &Path) -> crate::Result<Self> {
        if !path.exists() {
            return Ok(Self::from_config(&crate::Config::default(), path));
        }
        let content = std::fs::read_to_string(path).map_err(crate::PicocodeError::Io)?;
        let cfg: crate::Config = toml_edit::de::from_str(&content)
            .map_err(|e| crate::PicocodeError::Toml(e.to_string()))?;
        Ok(Self::from_config(&cfg, path))
    }

    /// Current display/edit value for a setting.
    pub fn value(&self, key: SettingKey) -> String {
        match key {
            SettingKey::DefaultModel => self.default_model.clone(),
            SettingKey::SecurityMode => self.security_mode.clone(),
            SettingKey::ToolCallLimit => self.tool_call_limit.to_string(),
            SettingKey::DefaultPersona => self.default_persona.clone().unwrap_or_default(),
            SettingKey::DefaultThinking => self.default_thinking_level.clone(),
        }
    }

    /// Validate a candidate value for `key` without persisting it.
    pub fn validate(key: SettingKey, value: &str) -> Result<(), String> {
        match key {
            SettingKey::DefaultModel => {
                let (p, m) = value
                    .split_once(':')
                    .ok_or_else(|| "expected provider:model (e.g. anthropic:claude-3-5-sonnet-20241022)".to_string())?;
                if p.trim().is_empty() || m.trim().is_empty() {
                    return Err("provider and model must be non-empty (provider:model)".into());
                }
                Ok(())
            }
            SettingKey::SecurityMode => match value.trim().to_ascii_lowercase().as_str() {
                "yolo" | "confirm" | "ask" | "safe" => Ok(()),
                other => Err(format!(
                    "security_mode must be yolo or confirm (got '{}')",
                    other
                )),
            },
            SettingKey::ToolCallLimit => {
                let n: usize = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("tool_call_limit must be a positive integer (got '{}')", value.trim()))?;
                if n == 0 {
                    return Err("tool_call_limit must be greater than 0".into());
                }
                Ok(())
            }
            SettingKey::DefaultPersona => {
                let v = value.trim();
                if v.is_empty() {
                    return Ok(()); // empty = no persona
                }
                if crate::persona::get_persona(v).is_some() {
                    Ok(())
                } else {
                    Err(format!(
                        "unknown persona '{}' (not a built-in persona and not a file)",
                        v
                    ))
                }
            }
            SettingKey::DefaultThinking => value
                .trim()
                .parse::<crate::thinking::ThinkingLevel>()
                .map(|_| ())
                .map_err(|e| e),
        }
    }

    /// Validate, apply, and persist a new value for `key`.
    pub fn set_value(&mut self, key: SettingKey, value: &str) -> Result<(), String> {
        Self::validate(key, value)?;
        self.apply(key, value.trim());
        self.save().map_err(|e| e.to_string())
    }

    /// Fuzzy-filter the registry by key + description and return each visible
    /// setting paired with its current value (for the overlay table).
    pub fn overlay_rows(&self, filter: &str) -> Vec<(SettingMeta, String)> {
        let filtered: Vec<&SettingMeta> = if filter.trim().is_empty() {
            SETTINGS.iter().collect()
        } else {
            crate::fuzzy::fuzzy_filter(SETTINGS, filter, |m| format!("{} {}", m.name, m.description))
                .into_iter()
                .map(|r| r.item)
                .collect()
        };
        filtered
            .into_iter()
            .map(|m| (*m, self.value(m.key)))
            .collect()
    }

    /// Persist the five managed keys into `self.path` via `toml_edit`, leaving
    /// everything else (comments, `[recipes.*]`, `tool_config`, …) intact.
    pub fn save(&self) -> crate::Result<()> {
        let mut doc = if self.path.exists() {
            let content = std::fs::read_to_string(&self.path).map_err(crate::PicocodeError::Io)?;
            content
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| crate::PicocodeError::Toml(e.to_string()))?
        } else {
            let mut d = toml_edit::DocumentMut::new();
            d.decor_mut().set_prefix(
                "# picocode.toml -- managed by the /settings menu; safe to edit by hand.\n\n",
            );
            d
        };
        let table = doc.as_table_mut();
        table.insert("default_model", toml_edit::value(self.default_model.clone()));
        table.insert("security_mode", toml_edit::value(self.security_mode.clone()));
        table.insert(
            "tool_call_limit",
            toml_edit::value(self.tool_call_limit as i64),
        );
        match &self.default_persona {
            Some(p) => {
                table.insert("default_persona", toml_edit::value(p.clone()));
            }
            None => {
                table.remove("default_persona");
            }
        }
        table.insert(
            "default_thinking_level",
            toml_edit::value(self.default_thinking_level.clone()),
        );
        std::fs::write(&self.path, doc.to_string()).map_err(crate::PicocodeError::Io)?;
        Ok(())
    }

    fn apply(&mut self, key: SettingKey, value: &str) {
        match key {
            SettingKey::DefaultModel => self.default_model = value.to_string(),
            SettingKey::SecurityMode => self.security_mode = normalize_security(value).to_string(),
            SettingKey::ToolCallLimit => {
                self.tool_call_limit = value.parse().unwrap_or(self.tool_call_limit)
            }
            SettingKey::DefaultPersona => {
                self.default_persona = if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                }
            }
            SettingKey::DefaultThinking => {
                self.default_thinking_level = normalize_thinking(value).to_string()
            }
        }
    }
}

fn normalize_security(v: &str) -> &'static str {
    match v.trim().to_ascii_lowercase().as_str() {
        "yolo" => "yolo",
        _ => "confirm",
    }
}

fn normalize_thinking(v: &str) -> &'static str {
    v.trim()
        .parse::<crate::thinking::ThinkingLevel>()
        .map(|l| l.as_str())
        .unwrap_or("off")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thinking::ThinkingLevel;

    fn store(path: &Path) -> SettingsStore {
        SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 50,
            default_persona: Some("architect".into()),
            default_thinking_level: "off".into(),
            path: path.to_path_buf(),
        }
    }

    #[test]
    fn validate_default_model() {
        assert!(SettingsStore::validate(SettingKey::DefaultModel, "openrouter:foo").is_ok());
        assert!(SettingsStore::validate(SettingKey::DefaultModel, "anthropic:").is_err());
        assert!(SettingsStore::validate(SettingKey::DefaultModel, ":model").is_err());
        assert!(SettingsStore::validate(SettingKey::DefaultModel, "nocolon").is_err());
    }

    #[test]
    fn validate_security_mode_normalizes_aliases() {
        assert!(SettingsStore::validate(SettingKey::SecurityMode, "yolo").is_ok());
        assert!(SettingsStore::validate(SettingKey::SecurityMode, "confirm").is_ok());
        assert!(SettingsStore::validate(SettingKey::SecurityMode, "ask").is_ok());
        assert!(SettingsStore::validate(SettingKey::SecurityMode, "bogus").is_err());
    }

    #[test]
    fn validate_tool_call_limit() {
        assert!(SettingsStore::validate(SettingKey::ToolCallLimit, "10").is_ok());
        assert!(SettingsStore::validate(SettingKey::ToolCallLimit, "0").is_err());
        assert!(SettingsStore::validate(SettingKey::ToolCallLimit, "x").is_err());
    }

    #[test]
    fn validate_default_persona() {
        assert!(SettingsStore::validate(SettingKey::DefaultPersona, "").is_ok());
        assert!(SettingsStore::validate(SettingKey::DefaultPersona, "architect").is_ok());
        assert!(SettingsStore::validate(SettingKey::DefaultPersona, "not-a-persona").is_err());
    }

    #[test]
    fn validate_default_thinking() {
        assert!(SettingsStore::validate(SettingKey::DefaultThinking, "off").is_ok());
        assert!(SettingsStore::validate(SettingKey::DefaultThinking, "high").is_ok());
        assert!(SettingsStore::validate(SettingKey::DefaultThinking, "bogus").is_err());
    }

    #[test]
    fn overlay_rows_filters_and_orders() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path().join("picocode.toml").as_path());
        let all = s.overlay_rows("");
        assert_eq!(all.len(), 5);
        let filtered = s.overlay_rows("model");
        assert!(!filtered.is_empty());
        assert_eq!(filtered[0].0.key, SettingKey::DefaultModel);
        assert_eq!(filtered[0].1, "anthropic:claude-3-5-sonnet-20241022");
    }

    #[test]
    fn set_value_persists_and_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("picocode.toml");
        let mut s = store(path.as_path());
        s.set_value(SettingKey::DefaultModel, "openrouter:foo/bar")
            .unwrap();
        assert_eq!(s.default_model, "openrouter:foo/bar");
        assert!(path.exists());
        let reloaded = SettingsStore::load(path.as_path()).unwrap();
        assert_eq!(reloaded.default_model, "openrouter:foo/bar");
        assert_eq!(reloaded.security_mode, "confirm");
        assert_eq!(reloaded.tool_call_limit, 50);
    }

    #[test]
    fn set_value_invalid_does_not_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("picocode.toml");
        let mut s = store(path.as_path());
        let err = s.set_value(SettingKey::ToolCallLimit, "0").unwrap_err();
        assert!(err.contains("greater than 0"));
        assert!(!path.exists(), "invalid value must not write the file");
    }

    #[test]
    fn set_value_none_persona_omits_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("picocode.toml");
        let mut s = store(path.as_path());
        s.set_value(SettingKey::DefaultPersona, "").unwrap();
        assert_eq!(s.default_persona, None);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("default_persona"), "empty persona must omit the key");
    }

    #[test]
    fn normalize_thinking_round_trips() {
        assert_eq!(normalize_thinking("high"), ThinkingLevel::High.as_str());
        assert_eq!(normalize_thinking("bogus"), "off");
    }

    #[test]
    fn overlay_lines_preserves_table_column_alignment() {
        // Regression: pre-formatted table rows passed through `overlay_lines`
        // must keep column alignment. Word-wrapping them collapsed the
        // inter-column padding spaces (and an empty value column vanished),
        // which is the misalignment reported for `/settings`.
        let dir = tempfile::tempdir().unwrap();
        let s = SettingsStore {
            default_model: "openrouter:tencent/hy3:free".into(), // matches the reported value
            default_persona: Some("".into()), // empty value column, like the report
            ..store(&dir.path().join("p.toml"))
        };
        let entries = s.overlay_rows("");
        let mut table = crate::table::Table::new(&["Setting", "Value", "Description"])
            .with_aligns(&[
                crate::table::Align::Left,
                crate::table::Align::Left,
                crate::table::Align::Left,
            ])
            .with_min_widths(&[16, 18, 24])
            .with_priorities(&[2, 2, 1])
            .set_gap(2);
        for (meta, value) in &entries {
            table.add_row(&[meta.name, value.as_str(), meta.description]);
        }
        // Render wide enough that nothing ellipsizes (no multi-byte '…' to
        // confuse byte-based find); wrap_opts=false keeps rows intact.
        let opts: Vec<String> = table.render(200);
        let out = crate::repl::Repl::overlay_lines("Settings", &opts, 0, None, false);
        let lines: Vec<&str> = out.lines().collect();
        // title + 5 single-line options (pre-formatted rows are NOT wrapped).
        assert_eq!(lines.len(), 6, "pre-formatted rows must not be word-wrapped");
        // Strip ANSI so visible column positions compare cleanly (the selected
        // row carries an extra color escape that must not affect alignment).
        fn strip_ansi(s: &str) -> String {
            let mut out = String::new();
            let mut chars = s.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for e in chars.by_ref() {
                        if e.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }
        let vis: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        let stripped: Vec<&str> = vis[1..]
            .iter()
            .map(|l| l.trim_start_matches("> ").trim_start_matches("  "))
            .collect();
        let v0 = stripped[0].find("openrouter").unwrap();
        let v1 = stripped[1].find("confirm").unwrap();
        assert_eq!(v0, v1, "value columns must align");
        let d0 = stripped[0].find("Default model").unwrap();
        let d1 = stripped[1].find("Tool execution").unwrap();
        let d2 = stripped[3].find("Persona for").unwrap();
        assert_eq!(d0, d1);
        assert_eq!(d0, d2, "description columns must align");
        assert!(
            stripped[3][v0..d0].chars().all(|c| c == ' '),
            "empty value column must keep its width (no collapse)"
        );
    }

    #[test]
    fn overlay_lines_keeps_wrapped_option_aligned_and_single() {
        // A wrapped option (Table::render_multiline output) must stay one
        // selectable item whose visual lines remain column-aligned.
        let dir = tempfile::tempdir().unwrap();
        let s = SettingsStore {
            default_model: "openrouter:tencent/hy3:free".into(),
            ..store(&dir.path().join("p.toml"))
        };
        let entries = s.overlay_rows("");
        let mut table = crate::table::Table::new(&["Setting", "Value", "Description"])
            .with_aligns(&[
                crate::table::Align::Left,
                crate::table::Align::Left,
                crate::table::Align::Left,
            ])
            .with_min_widths(&[16, 18, 24])
            .with_priorities(&[2, 2, 1])
            .set_gap(2);
        for (meta, value) in &entries {
            table.add_row(&[meta.name, value.as_str(), meta.description]);
        }
        let opts: Vec<String> = table.render_multiline(78);
        assert!(opts[0].contains('\n'), "first option wraps to multiple visual lines");
        let widths = table.column_widths(78);
        let first_opt_lines: Vec<&str> = opts[0].split('\n').collect();
        // The description column starts at the same column on every wrapped
        // visual line. (The value cell may also wrap when it is longer than
        // its column, which is expected.)
        let desc_start = widths[0] + 2 + widths[1] + 2; // Setting + gap + Value + gap
        assert_eq!(first_opt_lines[0].find("Default model").unwrap(), desc_start);
        assert!(first_opt_lines[1][desc_start..].starts_with("provider:model"));
        assert!(first_opt_lines[2][desc_start..].starts_with("new sessions"));
        // overlay_lines must preserve the multi-line option as visual lines
        // (title + every visual line of every option).
        let out = crate::repl::Repl::overlay_lines("Settings", &opts, 0, None, false);
        let expected = 1 + opts.iter().map(|o| o.matches('\n').count() + 1).sum::<usize>();
        assert_eq!(out.lines().count(), expected);
    }
}
