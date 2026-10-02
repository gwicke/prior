//! Settings registry + persistence for the `/settings` menu.
//!
//! Settings are persisted into the same `settings.toml` that holds `recipes`
//! (see `doc/settings_menu.md`). Reads go through serde (`toml_edit::de`);
//! writes go through `toml_edit::DocumentMut` so that user comments and the
//! `[recipes.*]` blocks are preserved on every save (YAML cannot do this).
//!
//! All logic here is pure and unit-testable; the REPL only renders the rows
//! produced by [`SettingsStore::overlay_rows`] and commits edits via
//! [`SettingsStore::set_value`].

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// How model reasoning/thinking is surfaced to the user.
///
/// * `Content`   (default) — stream the thinking/reasoning text inline, dimmed.
/// * `Disabled`           — show no reasoning text.
///
/// There is no third mode: "something is happening" is the status line's job
/// (its dot), not a separate spinner, so the only question left is whether the
/// reasoning text itself is shown.
///
/// Driven by the `show_reasoning` setting (editable via `/set`). The live value
/// lives in a [`ShowReasoningHandle`] shared by the `Output` impl and the running
/// `AgentConfig`, so a `/set show_reasoning …` change applies to the very next
/// response without rebuilding the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShowReasoning {
    /// Stream the thinking/reasoning text inline (dimmed).
    #[default]
    Content,
    /// Show no reasoning text.
    Disabled,
}

impl ShowReasoning {
    /// Canonical lowercase name used for persistence and the `/set` menu.
    pub fn as_str(self) -> &'static str {
        match self {
            ShowReasoning::Content => "content",
            ShowReasoning::Disabled => "disabled",
        }
    }

    /// Map a stored/parsed string to the mode. Tolerant of case and a few
    /// common synonyms; returns `None` for anything unrecognized so callers
    /// can fall back to the default and reject invalid `/set` input.
    ///
    /// The removed `indicator`/`spinner`/`status` values map to `Disabled`, not
    /// to the default: someone who chose the spinner chose it *to not see the
    /// reasoning text*, and that is the half of the behaviour that still
    /// exists. Mapping them to `Content` would start showing them text they had
    /// asked not to see. The activity signal they also got — the spinner — is
    /// now the status line's dot.
    pub fn parse_opt(s: &str) -> Option<ShowReasoning> {
        match s.trim().to_ascii_lowercase().as_str() {
            "content" | "on" | "true" | "reasoning" => Some(ShowReasoning::Content),
            "disabled" | "off" | "none" | "false" => Some(ShowReasoning::Disabled),
            "indicator" | "spinner" | "status" => Some(ShowReasoning::Disabled),
            _ => None,
        }
    }
}

impl FromStr for ShowReasoning {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_opt(s).ok_or_else(|| {
            format!(
                "unknown show_reasoning value: '{}' (use disabled | content)",
                s.trim()
            )
        })
    }
}

/// Shared, live-editable handle for the [`ShowReasoning`] display mode. The
/// `Output` impl holds a clone and reads it on every thinking event; the
/// running `AgentConfig` also holds a clone so `/set` can update it and the
/// change is effective immediately.
pub type ShowReasoningHandle = Arc<Mutex<ShowReasoning>>;

/// Build an owned default handle (`Content`).
pub fn default_show_reasoning_handle() -> ShowReasoningHandle {
    Arc::new(Mutex::new(ShowReasoning::default()))
}

/// User-tweakable prompt prepended to the dropped-span transcript when
/// `/tree <id> compact llm` is run. When the committed `/tree` line has no
/// trailing text after `llm`, the LLM-compaction handler substitutes this
/// value (or the built-in `DEFAULT_LLM_COMPACT_PROMPT` if empty).
///
/// The LLM-compaction handler sends the live conversation prefix (same
/// construction as a normal prompt) plus this instruction. A prior
/// compaction active at the prefix re-activates as a `<summary>` wrapper, so
/// the default prompt tells the model to treat that as the existing context
/// checkpoint and merge rather than replace — iterative re-compaction is
/// lossless (see doc/nested_compactions.md phase B).
pub const DEFAULT_LLM_COMPACT_PROMPT: &str = "Summarize the conversation in less than 2000 words. \
     Preserve the key documents, technical decisions, file paths, current \
     task state, and any unresolved questions.";

/// How reasoning blocks are handled in the LLM context (conversation history).
///
/// * `Always` — keep all reasoning blocks in context (default for Anthropic-family).
/// * `Never` — strip all reasoning blocks from context to save tokens.
/// * `AbortedTurnsOnly` — strip reasoning from successful turns, but preserve
///   reasoning from aborted/errored turns so the model can resume its train
///   of thought when the user provides a steering message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningInContextMode {
    /// Keep all reasoning in context.
    Always,
    /// Strip all reasoning from context.
    #[default]
    Never,
    /// Preserve reasoning only from aborted/errored turns.
    AbortedTurnsOnly,
}

impl ReasoningInContextMode {
    /// Canonical lowercase name used for persistence and the `/set` menu.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningInContextMode::Always => "always",
            ReasoningInContextMode::Never => "never",
            ReasoningInContextMode::AbortedTurnsOnly => "aborted_turns_only",
        }
    }

    /// Parse from string, case-insensitive. Returns `None` for unknown values.
    pub fn parse_opt(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" | "true" | "on" | "yes" => Some(ReasoningInContextMode::Always),
            "never" | "false" | "off" | "no" => Some(ReasoningInContextMode::Never),
            "aborted_turns_only" | "aborted" | "aborted_only" => {
                Some(ReasoningInContextMode::AbortedTurnsOnly)
            }
            _ => None,
        }
    }

    /// All enum variants with their canonical values and descriptions.
    /// Used by `/set` menu for autocomplete and value selection.
    pub fn variants() -> &'static [(&'static str, &'static str)] {
        &[
            ("always", "keep all reasoning blocks in context"),
            (
                "never",
                "drop reasoning from context to save tokens (default)",
            ),
            (
                "aborted_turns_only",
                "preserve reasoning only from aborted/errored turns",
            ),
        ]
    }
}

impl std::str::FromStr for ReasoningInContextMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_opt(s).ok_or_else(|| {
            format!(
                "unknown reasoning_in_context value: '{}' (use always | never | aborted_turns_only)",
                s.trim()
            )
        })
    }
}

impl std::fmt::Display for ReasoningInContextMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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
    DefaultReasoning,
    /// `disabled` | `indicator` | `content`.
    ShowReasoning,
    /// `always` | `never` | `aborted_turns_only` — how reasoning blocks are kept in context.
    ReasoningInContext,
    /// positive integer — number of turns to replay on session resume.
    SessionResumeReplayTurns,
    /// Free text — instruction prepended to the dropped-span transcript
    /// when `/tree <id> compact llm` is run with no trailing prompt.
    LlmCompactPrompt,
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
        key: SettingKey::DefaultReasoning,
        name: "default_reasoning_level",
        description: "Default thinking level: off | low | med | high",
        kind: ValueKind::Enum(&["off", "low", "med", "high"]),
    },
    SettingMeta {
        key: SettingKey::ShowReasoning,
        name: "show_reasoning",
        description: "How reasoning is shown: disabled | content",
        kind: ValueKind::Enum(&["disabled", "content"]),
    },
    SettingMeta {
        key: SettingKey::ReasoningInContext,
        name: "reasoning_in_context",
        description: "Keep reasoning in session context: always | never | aborted_turns_only",
        kind: ValueKind::Enum(&["always", "never", "aborted_turns_only"]),
    },
    SettingMeta {
        key: SettingKey::SessionResumeReplayTurns,
        name: "session_resume_replay_turns",
        description: "Turns to replay on session resume (0 = none)",
        kind: ValueKind::Int,
    },
    SettingMeta {
        key: SettingKey::LlmCompactPrompt,
        name: "llm_compact_prompt",
        description: "Prompt prepended to /tree <id> compact llm when no trailing text is given",
        kind: ValueKind::FreeText,
    },
];

/// In-memory mirror of the five managed settings, plus the optional file to
/// persist to (`None` when no platform location exists, e.g. no `$HOME`).
#[derive(Debug, Clone)]
pub struct SettingsStore {
    pub default_model: String,
    pub security_mode: String,
    pub tool_call_limit: usize,
    pub default_persona: Option<String>,
    /// `off` | `low` | `med` | `high` | `max`, or `None` for "no default
    /// recorded" — which is the same effective state as `off`, and is what an
    /// untouched file carries. Optional on purpose (doc/model_settings_v2.md
    /// A.5): the key is written only when the user actually picks a level, so
    /// "off" is represented by the key's *absence* rather than by a literal
    /// that would sit in every settings file forever.
    pub default_reasoning_level: Option<String>,
    /// `disabled` | `indicator` | `content` (how reasoning is shown).
    pub show_reasoning: String,
    /// How reasoning blocks are kept in the LLM context. Default `Never`.
    /// Forced `Always` for Anthropic-family providers.
    pub reasoning_in_context: ReasoningInContextMode,
    /// Number of most recent turns to replay on session resume.
    /// 0 = no replay. Default: 3.
    pub session_resume_replay_turns: usize,
    /// Free-text instruction used by `/tree <id> compact llm` when the
    /// committed line has no trailing prompt after `llm`. Empty falls back
    /// to [`DEFAULT_LLM_COMPACT_PROMPT`] at the call site.
    pub llm_compact_prompt: String,
    /// Per-model defaults and metadata overrides, keyed `provider:model`
    /// (doc/model_settings_v2.md A.5). The durable side of the live/live-vs-
    /// stored split: this is what `/model <id> save` writes and what the
    /// seeding rule (A.6) reads on a model's first activation.
    pub models: std::collections::HashMap<String, crate::model_settings::ModelSettings>,
    pub path: Option<PathBuf>,
}

impl SettingsStore {
    /// Build the store from the *resolved* effective session config, so the
    /// menu shows the actual current values (CLI > file > built-in precedence
    /// is already applied by the caller in `main.rs`).
    pub fn from_agent_config(
        cfg: &crate::AgentConfig,
        models: &std::collections::HashMap<String, crate::model_settings::ModelSettings>,
        path: Option<&Path>,
    ) -> Self {
        SettingsStore {
            default_model: cfg.qualified_model_id(),
            security_mode: if cfg.yolo {
                "yolo".to_string()
            } else {
                "confirm".to_string()
            },
            tool_call_limit: cfg.tool_call_limit,
            default_persona: cfg.persona_name.clone(),
            // A resolved "off" (i.e. `params.reasoning: None`) is stored as
            // "no key" rather than a literal "off" — the two are the same
            // effective state, and the absent form is what a file should carry.
            default_reasoning_level: cfg
                .params
                .reasoning_str()
                .filter(|l| !l.eq_ignore_ascii_case("off")),
            show_reasoning: cfg
                .show_reasoning_handle
                .lock()
                .unwrap()
                .as_str()
                .to_string(),
            reasoning_in_context: cfg.reasoning_in_context,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            // The per-model records, threaded in from the parsed `Config`.
            //
            // They have to be a parameter: they exist only on `Config`, and
            // `AgentConfig` has no field for them, so this constructor could
            // not recover them on its own. Defaulting them to empty here — with
            // a comment claiming `main` would fill them in — is what made the
            // whole per-model feature dead at startup: the file's records were
            // written correctly and never read, so `activate_model_params`
            // fell back to the global and the Ctrl-E prefill rendered a
            // fully-commented template. A Ctrl-E commit was worse still,
            // since `apply_model_settings` replaces the table, so committing
            // an edit against an empty record dropped the user's other keys.
            models: models.clone(),
            path: path.map(|p| p.to_path_buf()),
        }
    }

    /// Build the store from a parsed `Config` (file fields, with built-in
    /// fallbacks for anything the file omits).
    pub fn from_config(cfg: &crate::Config, path: Option<&Path>) -> Self {
        SettingsStore {
            default_model: cfg.default_model.clone().unwrap_or_default(),
            security_mode: cfg
                .security_mode
                .clone()
                .unwrap_or_else(|| "confirm".to_string()),
            tool_call_limit: cfg.tool_call_limit.unwrap_or(50),
            default_persona: cfg.default_persona.clone(),
            default_reasoning_level: cfg.default_reasoning_level.clone(),
            show_reasoning: cfg
                .show_reasoning
                .clone()
                .and_then(|s| ShowReasoning::parse_opt(&s))
                .map(|t| t.as_str().to_string())
                .unwrap_or_else(|| "content".to_string()),
            reasoning_in_context: cfg.reasoning_in_context.unwrap_or_default(),
            session_resume_replay_turns: 3,
            llm_compact_prompt: cfg.llm_compact_prompt.clone().unwrap_or_default(),
            models: cfg.models.clone(),
            path: path.map(|p| p.to_path_buf()),
        }
    }

    /// Apply the managed settings onto an `AgentConfig`, overwriting only
    /// the fields `/settings` controls. Used by `run_interactive` to live-apply
    /// pending `/settings` changes; the non-settings fields (output, agent/
    /// system prompt, bash auto-allow, cancel flag, …) are preserved. This is
    /// the inverse of [`SettingsStore::from_agent_config`].
    pub fn apply_to_config(&self, cfg: &mut crate::AgentConfig) {
        if let Some((p, m)) = self.default_model.split_once(':') {
            cfg.provider = p.to_string();
            cfg.model = m.to_string();
        }
        cfg.yolo = self.security_mode.eq_ignore_ascii_case("yolo");
        cfg.tool_call_limit = self.tool_call_limit;
        cfg.persona_name = self.default_persona.clone();
        cfg.persona_prompt = self
            .default_persona
            .as_ref()
            .and_then(|p| crate::persona::get_persona(p));
        // `None` (key absent) and an explicit "off" are the same effective
        // state; both mean "no reasoning requested".
        //
        // NOT a live writer: only tests call `apply_to_config`, and the live
        // path seeds via `AppState::activate_model_params` instead. If this
        // were ever wired into the running config it would be a bug — it
        // takes reasoning from the *global*, stomping whatever the model's own
        // record (or a runtime `/reasoning`) resolved to.
        cfg.params.reasoning = self
            .default_reasoning_level
            .clone()
            .filter(|l| !l.eq_ignore_ascii_case("off"))
            .and_then(|l| l.parse().ok());
        // Live-update the shared `show_reasoning` handle so the `Output` impl
        // (which holds a clone) honors the new mode on the very next response.
        if let Ok(mode) = self.show_reasoning.parse::<ShowReasoning>() {
            *cfg.show_reasoning_handle.lock().unwrap() = mode;
        }
        cfg.reasoning_in_context = self.reasoning_in_context;
    }

    /// Load from `path`, falling back to built-in defaults when the file is
    /// absent (the file is only written once a setting is first saved).
    pub fn load(path: &Path) -> crate::Result<Self> {
        if !path.exists() {
            return Ok(Self::from_config(&crate::Config::default(), Some(path)));
        }
        let content = std::fs::read_to_string(path).map_err(crate::PicocodeError::Io)?;
        // `Config::parse` (not a bare `toml_edit::de::from_str`) so this path
        // gets the same forward migrations as `Config::load`.
        let cfg = crate::Config::parse(&content)?;
        Ok(Self::from_config(&cfg, Some(path)))
    }

    /// Current display/edit value for a setting.
    pub fn value(&self, key: SettingKey) -> String {
        match key {
            SettingKey::DefaultModel => self.default_model.clone(),
            SettingKey::SecurityMode => self.security_mode.clone(),
            SettingKey::ToolCallLimit => self.tool_call_limit.to_string(),
            SettingKey::DefaultPersona => self.default_persona.clone().unwrap_or_default(),
            SettingKey::DefaultReasoning => self
                .default_reasoning_level
                .clone()
                .unwrap_or_else(|| "off".to_string()),
            SettingKey::ShowReasoning => self.show_reasoning.clone(),
            SettingKey::ReasoningInContext => self.reasoning_in_context.to_string(),
            SettingKey::SessionResumeReplayTurns => self.session_resume_replay_turns.to_string(),
            SettingKey::LlmCompactPrompt => self.llm_compact_prompt.clone(),
        }
    }

    /// Resolve the prompt used by `/tree <id> compact llm` when the
    /// committed line has no trailing text. Falls back to
    /// [`DEFAULT_LLM_COMPACT_PROMPT`] when the setting is empty.
    pub fn llm_compact_prompt(&self) -> String {
        if self.llm_compact_prompt.trim().is_empty() {
            DEFAULT_LLM_COMPACT_PROMPT.to_string()
        } else {
            self.llm_compact_prompt.clone()
        }
    }

    /// Validate a candidate value for `key` without persisting it.
    pub fn validate(key: SettingKey, value: &str) -> Result<(), String> {
        match key {
            SettingKey::DefaultModel => {
                let (p, m) = value.split_once(':').ok_or_else(|| {
                    "expected provider:model (e.g. anthropic:claude-3-5-sonnet-20241022)"
                        .to_string()
                })?;
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
                let n: usize = value.trim().parse().map_err(|_| {
                    format!(
                        "tool_call_limit must be a positive integer (got '{}')",
                        value.trim()
                    )
                })?;
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
            SettingKey::DefaultReasoning => value
                .trim()
                .parse::<crate::params::ReasoningLevel>()
                .map(|_| ())
                .map_err(|e| e),
            SettingKey::ShowReasoning => value
                .trim()
                .parse::<ShowReasoning>()
                .map(|_| ())
                .map_err(|e| e),
            SettingKey::ReasoningInContext => value
                .trim()
                .parse::<ReasoningInContextMode>()
                .map(|_| ())
                .map_err(|e| e),
            SettingKey::SessionResumeReplayTurns => {
                value.trim().parse::<usize>().map_err(|_| {
                    format!(
                        "session_resume_replay_turns must be a non-negative integer (got '{}')",
                        value.trim()
                    )
                })?;
                Ok(())
            }
            // FreeText: any value is acceptable. Empty is allowed and falls
            // back to DEFAULT_LLM_COMPACT_PROMPT at the call site, so the
            // user can clear it to restore the default.
            SettingKey::LlmCompactPrompt => Ok(()),
        }
    }

    /// Validate and apply a new value for `key` **without persisting** —
    /// for callers that must not hold a lock across file IO (the
    /// `AppState` coarse mutex): apply under the lock, clone the store,
    /// save outside it.
    pub fn apply_value(&mut self, key: SettingKey, value: &str) -> Result<(), String> {
        Self::validate(key, value)?;
        self.apply(key, value.trim());
        Ok(())
    }

    /// Validate, apply, and persist a new value for `key`.
    pub fn set_value(&mut self, key: SettingKey, value: &str) -> Result<(), String> {
        self.apply_value(key, value)?;
        self.save().map_err(|e| e.to_string())
    }

    /// Fuzzy-filter the registry by key + description and return each visible
    /// setting paired with its current value (for the overlay table).
    pub fn overlay_rows(&self, filter: &str) -> Vec<(SettingMeta, String)> {
        let filtered: Vec<&SettingMeta> = if filter.trim().is_empty() {
            SETTINGS.iter().collect()
        } else {
            crate::fuzzy::fuzzy_filter_bonus(SETTINGS, filter, |m| {
                format!("{} {}", m.name, m.description)
            })
            .into_iter()
            .map(|r| r.item)
            .collect()
        };
        filtered
            .into_iter()
            .map(|m| (*m, self.value(m.key)))
            .collect()
    }

    /// Persist the five managed keys into the configured file via `toml_edit`,
    /// leaving everything else (comments, `[recipes.*]`, `tool_config`, …)
    /// intact. When no persistent location is configured (`self.path` is
    /// `None` — e.g. no `$HOME`/`$XDG_CONFIG_HOME`/`%APPDATA%`), this is a
    /// no-op: in-session edits still apply but are not written to disk.
    pub fn save(&self) -> crate::Result<()> {
        let path = match &self.path {
            Some(p) => p,
            None => return Ok(()),
        };
        let mut doc = if path.exists() {
            let content = std::fs::read_to_string(path).map_err(crate::PicocodeError::Io)?;
            content
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| crate::PicocodeError::Toml(e.to_string()))?
        } else {
            let mut d = toml_edit::DocumentMut::new();
            d.decor_mut().set_prefix(
                "# settings.toml -- managed by the /settings menu; safe to edit by hand.\n\n",
            );
            d
        };
        let table = doc.as_table_mut();
        // `Table::insert` replaces the key, and a key's leading comment lives
        // on the key — so a naive replace silently drops a comment the user
        // wrote above a managed key. This has always happened here; the
        // module's claim that comments survive only held for keys we did not
        // touch. Carry the decor across on every managed write.
        let put = |table: &mut toml_edit::Table, key: &str, item: toml_edit::Item| {
            // A key's leading comment lives on the KEY's leaf decor, not on
            // the value's — so the decor must be carried onto the new key
            // after the insert, or the note lands inside the value.
            let decor = table.key(key).map(|k| k.leaf_decor().clone());
            table.insert(key, item);
            if let (Some(decor), Some(mut new_key)) = (decor, table.key_mut(key)) {
                *new_key.leaf_decor_mut() = decor;
            }
        };
        put(
            table,
            "default_model",
            toml_edit::value(self.default_model.clone()),
        );
        put(
            table,
            "security_mode",
            toml_edit::value(self.security_mode.clone()),
        );
        put(
            table,
            "tool_call_limit",
            toml_edit::value(self.tool_call_limit as i64),
        );
        match &self.default_persona {
            Some(p) => {
                put(table, "default_persona", toml_edit::value(p.clone()));
            }
            None => {
                table.remove("default_persona");
            }
        }
        // default_reasoning_level: written only when a level is recorded, and
        // removed when there is none — same shape as `default_persona` above.
        // "off" is stored as absence, so an untouched file never carries it.
        match &self.default_reasoning_level {
            Some(level) => {
                put(
                    table,
                    "default_reasoning_level",
                    toml_edit::value(level.clone()),
                );
            }
            None => {
                table.remove("default_reasoning_level");
            }
        }
        // The pre-rename key is never written back: `Config` folds it into
        // `default_reasoning_level` on load, and the next save drops it.
        table.remove("default_thinking_level");
        put(
            table,
            "show_reasoning",
            toml_edit::value(self.show_reasoning.clone()),
        );
        put(
            table,
            "reasoning_in_context",
            toml_edit::value(self.reasoning_in_context.as_str()),
        );
        // llm_compact_prompt: only written when set (non-empty); an absent
        // key falls back to DEFAULT_LLM_COMPACT_PROMPT on load.
        if !self.llm_compact_prompt.trim().is_empty() {
            put(
                table,
                "llm_compact_prompt",
                toml_edit::value(self.llm_compact_prompt.clone()),
            );
        } else {
            table.remove("llm_compact_prompt");
        }
        // Per-model tables are written back wholesale, each through the
        // shared writer so comments survive and an empty record leaves no
        // trace.
        for (id, settings) in &self.models {
            crate::model_settings::write_to_doc(&mut doc, id, settings)
                .map_err(|e| crate::PicocodeError::Other(format!("{id}: {e}")))?;
        }
        // Ensure the parent directory (e.g. ~/.config/picocode) exists before
        // writing, mirroring how the history file is created.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(crate::PicocodeError::Io)?;
        }
        std::fs::write(path, doc.to_string()).map_err(crate::PicocodeError::Io)?;
        Ok(())
    }

    /// Record one model's settings **in memory**, without persisting — the
    /// `apply_value` half of the `apply_value`/`set_value` pair. The write
    /// belongs to
    /// [`AppState::update_settings`](crate::app_state::AppState::update_settings),
    /// which runs this under the store's lock and writes the file outside it.
    ///
    /// A record with nothing set **removes** the table: "not configured" and
    /// "configured to nothing" are the same state, and keeping an empty table
    /// would only invite the reader to wonder what it was for.
    pub fn apply_model_settings(
        &mut self,
        id: &str,
        settings: &crate::model_settings::ModelSettings,
    ) {
        self.models.insert(id.to_string(), settings.clone());
    }

    /// `apply_model_settings` + `save`, for callers that already hold the
    /// store by value (tests, and the startup path with no lock involved).
    /// Anything reached through `AppState` must use `update_settings` instead,
    /// so the file write never happens under the coarse lock.
    pub fn put_model_settings(
        &mut self,
        id: &str,
        settings: &crate::model_settings::ModelSettings,
    ) -> Result<(), String> {
        self.apply_model_settings(id, settings);
        self.save().map_err(|e| e.to_string())
    }

    /// The stored record for a model, if the file has one.
    pub fn model_settings(&self, id: &str) -> Option<&crate::model_settings::ModelSettings> {
        self.models.get(id)
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
            SettingKey::DefaultReasoning => {
                // "off" is stored as absence, so `/set default_reasoning_level
                // off` removes the key rather than pinning a literal that
                // means the same thing.
                let level = normalize_reasoning(value);
                self.default_reasoning_level = if level == "off" {
                    None
                } else {
                    Some(level.to_string())
                };
            }
            SettingKey::ShowReasoning => {
                self.show_reasoning = normalize_show_reasoning(value).to_string()
            }
            SettingKey::ReasoningInContext => {
                self.reasoning_in_context = value
                    .trim()
                    .parse::<ReasoningInContextMode>()
                    .unwrap_or_default()
            }
            SettingKey::SessionResumeReplayTurns => {
                self.session_resume_replay_turns = value
                    .trim()
                    .parse()
                    .unwrap_or(self.session_resume_replay_turns)
            }
            SettingKey::LlmCompactPrompt => {
                self.llm_compact_prompt = value.trim().to_string();
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

fn normalize_reasoning(v: &str) -> &'static str {
    v.trim()
        .parse::<crate::params::ReasoningLevel>()
        .map(|l| l.as_str())
        .unwrap_or("off")
}

fn normalize_show_reasoning(v: &str) -> &'static str {
    v.trim()
        .parse::<ShowReasoning>()
        .map(|l| l.as_str())
        .unwrap_or("content")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::ReasoningLevel;

    fn store(path: &Path) -> SettingsStore {
        SettingsStore {
            default_model: "anthropic:claude-3-5-sonnet-20241022".into(),
            security_mode: "confirm".into(),
            tool_call_limit: 50,
            default_persona: Some("architect".into()),
            default_reasoning_level: None,
            show_reasoning: "content".into(),
            reasoning_in_context: ReasoningInContextMode::Never,
            session_resume_replay_turns: 3,
            llm_compact_prompt: String::new(),
            models: std::collections::HashMap::new(),
            path: Some(path.to_path_buf()),
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
    fn validate_default_reasoning() {
        assert!(SettingsStore::validate(SettingKey::DefaultReasoning, "off").is_ok());
        assert!(SettingsStore::validate(SettingKey::DefaultReasoning, "high").is_ok());
        assert!(SettingsStore::validate(SettingKey::DefaultReasoning, "bogus").is_err());
    }

    #[test]
    fn validate_show_reasoning() {
        assert!(SettingsStore::validate(SettingKey::ShowReasoning, "disabled").is_ok());
        assert!(SettingsStore::validate(SettingKey::ShowReasoning, "INDICATOR").is_ok());
        assert!(SettingsStore::validate(SettingKey::ShowReasoning, "content").is_ok());
        assert!(SettingsStore::validate(SettingKey::ShowReasoning, "bogus").is_err());
    }

    #[test]
    fn validate_reasoning_in_context() {
        assert!(SettingsStore::validate(SettingKey::ReasoningInContext, "always").is_ok());
        assert!(SettingsStore::validate(SettingKey::ReasoningInContext, "never").is_ok());
        assert!(
            SettingsStore::validate(SettingKey::ReasoningInContext, "aborted_turns_only").is_ok()
        );
        // Legacy aliases
        assert!(SettingsStore::validate(SettingKey::ReasoningInContext, "true").is_ok());
        assert!(SettingsStore::validate(SettingKey::ReasoningInContext, "FALSE").is_ok());
        assert!(SettingsStore::validate(SettingKey::ReasoningInContext, "bogus").is_err());
    }

    #[test]
    fn show_reasoning_parse_and_default() {
        assert_eq!(ShowReasoning::default(), ShowReasoning::Content);
        assert_eq!(
            "disabled".parse::<ShowReasoning>().unwrap(),
            ShowReasoning::Disabled
        );
        assert_eq!(
            "content".parse::<ShowReasoning>().unwrap(),
            ShowReasoning::Content
        );
        assert!("bogus".parse::<ShowReasoning>().is_err());
        // The removed spinner values survive as a parse, and mean "no text".
        assert_eq!(
            ShowReasoning::parse_opt("spinner"),
            Some(ShowReasoning::Disabled)
        );
        assert_eq!(
            ShowReasoning::parse_opt("indicator"),
            Some(ShowReasoning::Disabled)
        );
        assert_eq!(ShowReasoning::as_str(ShowReasoning::Disabled), "disabled");
    }

    #[test]
    fn apply_to_config_maps_the_five_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = store(tmp.path().join("picocode.toml").as_path());
        // Baseline `AgentConfig` built the way `main.rs` does.
        let mut cfg = crate::AgentConfig {
            provider: "anthropic".into(),
            model: "claude-3-5-sonnet-20241022".into(),
            output: std::sync::Arc::new(crate::QuietOutput::new()),
            yolo: false,
            quiet: true,
            params: Default::default(),
            tool_call_limit: 50,
            system_message_extension: None,
            persona_prompt: None,
            persona_name: None,
            bash_auto_allow: None,
            agent_prompt: None,
            user_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cancel_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            sandbox: None,
            show_reasoning_handle: crate::settings::default_show_reasoning_handle(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            message_log_path: None,
            sessions_dir: None,
        };
        // Mutate every setting, then apply.
        s.set_value(SettingKey::DefaultModel, "openrouter:tencent/hy3:free")
            .unwrap();
        s.set_value(SettingKey::SecurityMode, "yolo").unwrap();
        s.set_value(SettingKey::ToolCallLimit, "12").unwrap();
        s.set_value(SettingKey::DefaultPersona, "architect")
            .unwrap();
        // My renamed key, upstream's value: `ShowReasoning::Indicator` no
        // longer exists upstream, so the "disabled" arm is the live one.
        s.set_value(SettingKey::DefaultReasoning, "high").unwrap();
        s.set_value(SettingKey::ShowReasoning, "disabled").unwrap();
        s.set_value(SettingKey::ReasoningInContext, "always")
            .unwrap();
        s.apply_to_config(&mut cfg);
        assert_eq!(cfg.provider, "openrouter");
        assert_eq!(cfg.model, "tencent/hy3:free");
        assert!(cfg.yolo);
        assert_eq!(cfg.tool_call_limit, 12);
        assert_eq!(cfg.persona_name.as_deref(), Some("architect"));
        assert!(
            cfg.persona_prompt.is_some(),
            "persona prompt should resolve"
        );
        assert_eq!(cfg.params.reasoning_str().as_deref(), Some("high"));
        // `show_reasoning` is applied live via the shared handle.
        assert_eq!(
            *cfg.show_reasoning_handle.lock().unwrap(),
            crate::settings::ShowReasoning::Disabled
        );

        // reasoning_in_context is applied from the store value.
        assert!(
            matches!(
                cfg.reasoning_in_context,
                crate::settings::ReasoningInContextMode::Always
            ),
            "reasoning_in_context should be Always after /set"
        );

        // "off" thinking level maps back to `None`.
        s.set_value(SettingKey::DefaultReasoning, "off").unwrap();
        s.apply_to_config(&mut cfg);
        assert_eq!(cfg.params.reasoning, None);

        // Non-settings fields are preserved.
        assert!(cfg.quiet);
        assert!(cfg.agent_prompt.is_none());
    }

    #[test]
    fn overlay_rows_filters_and_orders() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path().join("picocode.toml").as_path());
        let all = s.overlay_rows("");
        assert_eq!(all.len(), 9);
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

    // ---- default_reasoning_level is optional (doc/model_settings_v2.md A.5) ----

    /// An unrecorded level is the ABSENCE of the key, not a literal "off" —
    /// so a settings file that the user never touched reasoning in does not
    /// carry one. `value()` still reports "off" so the menu shows the right
    /// current state.
    #[test]
    fn unset_reasoning_level_is_omitted_from_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("picocode.toml");
        let mut s = store(path.as_path());
        assert_eq!(s.value(SettingKey::DefaultReasoning), "off");
        s.set_value(SettingKey::DefaultModel, "openrouter:foo")
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("default_reasoning_level"),
            "an unset level must not be written: {content}"
        );
        assert!(s.default_reasoning_level.is_none());
        // Still reported as "off" on read-back.
        assert_eq!(
            SettingsStore::load(&path)
                .unwrap()
                .value(SettingKey::DefaultReasoning),
            "off"
        );
    }

    /// A chosen level round-trips; choosing "off" again removes the key rather
    /// than pinning a literal that means the same thing.
    #[test]
    fn reasoning_level_round_trips_and_off_removes_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("picocode.toml");
        let mut s = store(path.as_path());
        s.set_value(SettingKey::DefaultReasoning, "max").unwrap();
        assert_eq!(s.default_reasoning_level.as_deref(), Some("max"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains(r#"default_reasoning_level = "max""#),
            "{content}"
        );
        assert_eq!(
            SettingsStore::load(&path)
                .unwrap()
                .default_reasoning_level
                .as_deref(),
            Some("max")
        );

        s.set_value(SettingKey::DefaultReasoning, "off").unwrap();
        assert!(s.default_reasoning_level.is_none());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("default_reasoning_level"),
            "'off' must be stored as absence: {content}"
        );
    }

    /// Saving never writes the pre-rename key back, and drops it from a file
    /// that still carries it (the read side folded it into the new key).
    #[test]
    fn save_drops_the_pre_rename_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("picocode.toml");
        std::fs::write(&path, "default_thinking_level = \"high\"\n").unwrap();
        let mut s = SettingsStore::load(&path).unwrap();
        assert_eq!(s.default_reasoning_level.as_deref(), Some("high"));
        s.set_value(SettingKey::DefaultModel, "openrouter:foo")
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("default_thinking_level"),
            "the old spelling must not survive a save: {content}"
        );
        assert!(
            content.contains(r#"default_reasoning_level = "high""#),
            "the migrated level must be written under the new key: {content}"
        );
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
        assert!(
            !content.contains("default_persona"),
            "empty persona must omit the key"
        );
    }

    #[test]
    fn normalize_reasoning_round_trips() {
        assert_eq!(normalize_reasoning("high"), ReasoningLevel::High.as_str());
        assert_eq!(normalize_reasoning("bogus"), "off");
    }

    #[test]
    fn paint_plan_preserves_table_column_alignment() {
        // Regression: pre-formatted table rows passed through the overlay
        // paint path must keep column alignment (ported from
        // `overlay_lines` to `paint_plan`, ui_draw_v2 P5). Word-wrapping
        // them collapsed the inter-column padding spaces (and an empty
        // value column vanished), which is the misalignment reported for
        // `/settings`.
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
            .set_gap(1);
        for (meta, value) in &entries {
            table.add_row(&[meta.name, value.as_str(), meta.description]);
        }
        // Render wide enough that nothing ellipsizes (no multi-byte '…' to
        // confuse byte-based find); `render` never wraps. The layout width
        // is the terminal width *minus the marker column*: the painter adds
        // `> ` back to reach the full width, and splits anything wider than
        // that (the same `tw - 2` the v25 nodes lay their tables out at).
        let opts: Vec<String> = table.render(198);
        let plan = crate::slashdot_menu::render::RenderPlan {
            detail: vec![crate::slashdot_menu::render::LogicalLine::new("Settings")],
            rows: std::sync::Arc::new(
                opts.iter()
                    .map(|o| crate::slashdot_menu::render::LogicalLine::new(o.clone()))
                    .collect(),
            ),
            selected: Some(0),
            ..Default::default()
        };
        let out = crate::slashdot_menu::render::paint_plan(&plan, 200, 40);
        let lines: Vec<&str> = out.lines().collect();
        // title + 9 single-line options (pre-formatted rows are NOT wrapped).
        assert_eq!(
            lines.len(),
            10,
            "pre-formatted rows must not be word-wrapped"
        );
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
    fn paint_plan_keeps_wrapped_option_aligned_and_single() {
        // A wrapped option (Table::render_multiline output) must stay one
        // selectable plan row whose visual lines remain column-aligned
        // and come out of the paint path un-collapsed (ported from
        // `overlay_lines` to `paint_plan`, ui_draw_v2 P5).
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
            .set_gap(1);
        for (meta, value) in &entries {
            table.add_row(&[meta.name, value.as_str(), meta.description]);
        }
        let opts: Vec<String> = table.render_multiline(78);
        assert!(
            opts[0].contains('\n'),
            "first option wraps to multiple visual lines"
        );
        let widths = table.column_widths(78);
        let first_opt_lines: Vec<&str> = opts[0].split('\n').collect();
        // The description column starts at the same column on every wrapped
        // visual line. (The value cell may also wrap when it is longer than
        // its column, which is expected.)
        let desc_start = widths[0] + 1 + widths[1] + 1; // Setting + gap + Value + gap
        assert_eq!(
            first_opt_lines[0].find("Default model").unwrap(),
            desc_start
        );
        assert!(first_opt_lines[1][desc_start..].starts_with("provider:model"));
        // Third wrapped line continues the description text
        assert!(first_opt_lines[2][desc_start..].starts_with("sessions"));
        // paint_plan must preserve the multi-line option as visual lines:
        // every line of the *selected* option appears (marked first line,
        // `  `-indented continuations), none of them re-wrapped. Unlike
        // overlay_lines, paint_plan windows long plans around the
        // selection — the old exact total-line count belonged to a
        // renderer that never windowed, so pin what the plan guarantees:
        // sel = 0 keeps option 0 fully in view.
        let plan = crate::slashdot_menu::render::RenderPlan {
            detail: vec![crate::slashdot_menu::render::LogicalLine::new("Settings")],
            rows: std::sync::Arc::new(
                opts.iter()
                    .map(|o| crate::slashdot_menu::render::LogicalLine::new(o.clone()))
                    .collect(),
            ),
            selected: Some(0),
            ..Default::default()
        };
        // Painted at 80 = the 78-column layout plus the 2-column marker, so
        // every laid-out line fits the painter's content width untouched.
        let out = crate::slashdot_menu::render::paint_plan(&plan, 80, 40);
        let lines: Vec<&str> = out.lines().collect();
        assert!(
            lines.iter().any(|l| l.contains(first_opt_lines[0].trim())),
            "first option's marked line missing: {lines:?}"
        );
        for sub in first_opt_lines.iter().skip(1) {
            assert!(
                lines.iter().any(|l| l.contains(sub)),
                "first option's continuation line missing: {sub:?}\n{lines:?}"
            );
        }
    }
    // ---- per-model tables (doc/model_settings_v2.md A.5 / P4) ----

    fn model_settings(reasoning: &str, temperature: f64) -> crate::model_settings::ModelSettings {
        crate::model_settings::ModelSettings {
            reasoning: reasoning.parse().ok(),
            temperature: Some(temperature),
            ..Default::default()
        }
    }

    /// The whole point of writing through `toml_edit`: a hand-written file
    /// with comments, `[recipes.*]` and `[tool_config]` must come back
    /// byte-for-byte on the parts we did not touch, with the per-model table
    /// added.
    #[test]
    fn per_model_table_lands_without_disturbing_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(
            &path,
            "# my settings\ndefault_model = \"openrouter:tencent/hy3:free\"\n\n\
             [recipes.review]\nprompt = \"Review this.\"\n",
        )
        .unwrap();
        let mut s = SettingsStore::load(&path).unwrap();
        s.put_model_settings("openrouter:tencent/hy3:free", &model_settings("high", 0.8))
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("# my settings"),
            "comment kept:\n{content}"
        );
        assert!(
            content.contains("default_model = \"openrouter:tencent/hy3:free\""),
            "root key kept:\n{content}"
        );
        assert!(
            content.contains("prompt = \"Review this.\""),
            "recipes kept:\n{content}"
        );
        // The model id needs quoting: it contains `:` and `/`.
        assert!(
            content.contains("[models.\"openrouter:tencent/hy3:free\"]"),
            "quoted table header:\n{content}"
        );
        assert!(content.contains("reasoning = \"high\""), "{content}");
        assert!(content.contains("temperature = 0.8"), "{content}");
    }

    /// A file that already has per-model tables loads them, and saving keeps
    /// them while changing something else.
    #[test]
    fn per_model_tables_load_and_survive_an_unrelated_save() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(
            &path,
            "# nightly\n[models.\"p:m\"]\n# only for the batch job\nreasoning = \"max\"\ntop_p = 0.9\n",
        )
        .unwrap();
        let mut s = SettingsStore::load(&path).unwrap();
        let record = s.model_settings("p:m").expect("table must load");
        assert_eq!(record.reasoning, Some(crate::params::ReasoningLevel::Max));
        assert_eq!(record.top_p, Some(0.9));
        assert!(
            record
                .comments
                .get("reasoning")
                .is_some_and(|c| c.contains("batch job")),
            "comment must load: {:?}",
            record.comments
        );

        s.set_value(SettingKey::ToolCallLimit, "42").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("tool_call_limit = 42"), "{content}");
        assert!(content.contains("reasoning = \"max\""), "kept: {content}");
        assert!(content.contains("batch job"), "comment kept: {content}");
    }

    /// A typo in a per-model table fails the load loudly, naming the key.
    #[test]
    fn a_typo_in_a_per_model_table_is_a_load_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "[models.\"p:m\"]\ntemprature = 0.8\n").unwrap();
        let err = SettingsStore::load(&path).unwrap_err().to_string();
        assert!(err.contains("temprature"), "{err}");
        assert!(err.contains("did you mean 'temperature'"), "{err}");
    }

    /// Storing an empty record removes the table, so a model the user cleared
    /// leaves no trace and reads as unconfigured again.
    #[test]
    fn storing_an_empty_record_removes_the_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        let mut s = store(path.as_path());
        s.put_model_settings("p:m", &model_settings("high", 0.8))
            .unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("models"));
        s.put_model_settings("p:m", &crate::model_settings::ModelSettings::default())
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("models"),
            "table should be gone: {content}"
        );
        assert!(s.model_settings("p:m").is_some_and(|r| r.is_empty()));
    }

    /// `params()` is the seam the seeding rule (A.6) reads, so a record must
    /// hand over exactly its three knobs and nothing else.
    #[test]
    fn a_records_params_are_its_three_knobs() {
        let record = crate::model_settings::ModelSettings {
            reasoning: Some(crate::params::ReasoningLevel::Med),
            temperature: Some(0.2),
            top_p: Some(0.9),
            context_length: Some(1_000),
            ..Default::default()
        };
        let p = record.params();
        assert_eq!(p.reasoning, Some(crate::params::ReasoningLevel::Med));
        assert_eq!(p.temperature, Some(0.2));
        assert_eq!(p.top_p, Some(0.9));
    }

    // ---- the startup store must carry the file's records (P5.6) ----

    /// The whole per-model feature was dead at startup because
    /// `from_agent_config` had nowhere to get `[models.*]` and defaulted them
    /// to empty. These go through the *startup* constructor deliberately: a
    /// test that hand-builds a `SettingsStore` with `models` populated passes
    /// happily and proves nothing, which is exactly what hid the bug.
    fn startup_store(records: &[(&str, ReasoningLevel)]) -> (SettingsStore, crate::AgentConfig) {
        let mut cfg = SettingsStore::from_config(&crate::Config::default(), None);
        cfg.default_reasoning_level = Some("low".to_string());
        cfg.models = records
            .iter()
            .map(|(id, level)| {
                (
                    id.to_string(),
                    crate::model_settings::ModelSettings {
                        reasoning: Some(*level),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let agent_config = crate::AgentConfig {
            provider: "openrouter".to_string(),
            model: "stealth/space-bunny-alpha".to_string(),
            params: crate::params::ModelParams {
                reasoning: Some(ReasoningLevel::Low),
                ..Default::default()
            },
            ..test_agent_config()
        };
        let store = SettingsStore::from_agent_config(&agent_config, &cfg.models, None);
        (store, agent_config)
    }

    fn test_agent_config() -> crate::AgentConfig {
        crate::AgentConfig {
            provider: "test".to_string(),
            model: "model".to_string(),
            output: Arc::new(crate::output::NoOutput),
            yolo: false,
            quiet: true,
            params: Default::default(),
            tool_call_limit: 10,
            system_message_extension: None,
            persona_prompt: None,
            persona_name: None,
            bash_auto_allow: None,
            agent_prompt: None,
            user_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            sandbox: None,
            show_reasoning_handle: crate::settings::ShowReasoningHandle::default(),
            reasoning_in_context: crate::settings::ReasoningInContextMode::Never,
            message_log_path: None,
            sessions_dir: None,
        }
    }

    /// Symptom 1: the model's record was ignored and the global won, so
    /// `/reasoning` reported the global's level.
    #[test]
    fn the_startup_store_keeps_the_per_model_records() {
        let (store, _) =
            startup_store(&[("openrouter:stealth/space-bunny-alpha", ReasoningLevel::Max)]);
        let record = store
            .model_settings("openrouter:stealth/space-bunny-alpha")
            .expect("the record must survive the startup constructor");
        assert_eq!(record.reasoning, Some(ReasoningLevel::Max));
    }

    /// ...which is only interesting because it fixes the seeding that reads it.
    #[test]
    fn seeding_reads_the_record_through_the_startup_store() {
        let (store, agent_config) =
            startup_store(&[("openrouter:stealth/space-bunny-alpha", ReasoningLevel::Max)]);
        let app = crate::app_state::AppState::new(agent_config, store, None);
        app.activate_model_params("openrouter:stealth/space-bunny-alpha");
        assert_eq!(
            app.params().reasoning,
            Some(ReasoningLevel::Max),
            "the model's own level, not the global's 'low'"
        );
    }

    /// Symptom 2: the Ctrl-E prefill rendered every key commented out, because
    /// it was handed a default record instead of the stored one.
    #[test]
    fn the_editor_prefill_shows_the_stored_values() {
        let (store, _) =
            startup_store(&[("openrouter:stealth/space-bunny-alpha", ReasoningLevel::Max)]);
        let current = store
            .model_settings("openrouter:stealth/space-bunny-alpha")
            .cloned()
            .unwrap();
        let offered = crate::model_settings::render_fragment(
            "openrouter:stealth/space-bunny-alpha",
            &current,
            &Default::default(),
        );
        assert!(
            offered.contains("\nreasoning = \"max\"\n"),
            "the stored level must render uncommented; got:\n{offered}"
        );
        assert!(
            !offered.contains("# reasoning = "),
            "and not as a commented suggestion:\n{offered}"
        );
    }

    /// Symptom 3, the damaging one: `apply_model_settings` replaces the table,
    /// so a Ctrl-E commit against an empty startup record would have dropped
    /// the keys the file already had.
    #[test]
    fn a_ctrl_e_commit_keeps_the_keys_it_did_not_touch() {
        let mut store =
            startup_store(&[("openrouter:stealth/space-bunny-alpha", ReasoningLevel::Max)]).0;
        let mut edited = store
            .model_settings("openrouter:stealth/space-bunny-alpha")
            .cloned()
            .unwrap();
        edited.context_length = Some(4096);
        store.apply_model_settings("openrouter:stealth/space-bunny-alpha", &edited);
        let after = store
            .model_settings("openrouter:stealth/space-bunny-alpha")
            .cloned()
            .unwrap();
        assert_eq!(
            after.reasoning,
            Some(ReasoningLevel::Max),
            "untouched key kept"
        );
        assert_eq!(after.context_length, Some(4096), "edited key applied");
    }

    /// The full startup chain, real file included: parse the settings file,
    /// build the store the way `main` does, and confirm the record reaches the
    /// live params. This is the step the previous tests stubbed out — the file
    /// *is* read correctly (`Config::parse` populates `models`), so nothing
    /// above this line would have caught the bug.
    #[test]
    fn a_real_settings_file_survives_the_whole_startup_chain() {
        let dir = std::env::temp_dir().join("picocode-startup-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(
            &path,
            r#"default_model = "ollama:llama3"
default_reasoning_level = "low"

[models."ollama:llama3"]
reasoning = "max"
"#,
        )
        .unwrap();

        // 1. the file parses, and the record is on the Config
        let cfg = crate::Config::load(Some(path.to_str().unwrap())).expect("load");
        let record = cfg
            .models
            .get("ollama:llama3")
            .expect("Config::parse must populate models");
        assert_eq!(record.reasoning, Some(ReasoningLevel::Max));

        // 2. the startup store keeps it
        let store =
            SettingsStore::from_agent_config(&test_agent_config(), &cfg.models, Some(&path));
        assert!(store.model_settings("ollama:llama3").is_some());

        // 3. seeding honours it over the global
        let app = crate::app_state::AppState::new(test_agent_config(), store, None);
        app.activate_model_params("ollama:llama3");
        assert_eq!(
            app.params().reasoning,
            Some(ReasoningLevel::Max),
            "the file's record must beat the global's 'low'"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
