use crate::path_util;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// Wrapper types that can deserialize from both JSON numbers and strings.
/// These handle the common case where LLMs send numeric parameters as strings.

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    schemars::JsonSchema,
    Default,
    serde::Serialize,
)]
pub struct ReadOffset(pub u64);

impl<'de> serde::Deserialize<'de> for ReadOffset {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = u64;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a positive integer for read offset (not a string)")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(value)
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(E::custom("invalid read offset: negative value not allowed"));
                }
                Ok(value as u64)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if let Ok(parsed) = value.parse::<u64>() {
                    Ok(parsed)
                } else {
                    Err(E::custom(format!(
                        "invalid read offset: '{}' - must be number",
                        value
                    )))
                }
            }
        }

        deserializer.deserialize_any(Visitor).map(ReadOffset)
    }
}

impl ReadOffset {
    pub fn map<U, F: FnOnce(u64) -> U>(self, f: F) -> U {
        f(self.0)
    }
}

impl From<ReadOffset> for u64 {
    fn from(w: ReadOffset) -> u64 {
        w.0
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    schemars::JsonSchema,
    Default,
    serde::Serialize,
)]
pub struct ReadLimit(pub u64);

impl<'de> serde::Deserialize<'de> for ReadLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = u64;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a positive integer for read limit (not a string)")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(value)
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(E::custom("invalid read limit: negative value not allowed"));
                }
                Ok(value as u64)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if let Ok(parsed) = value.parse::<u64>() {
                    Ok(parsed)
                } else {
                    Err(E::custom(format!(
                        "invalid read limit: '{}' - must be number",
                        value
                    )))
                }
            }
        }

        deserializer.deserialize_any(Visitor).map(ReadLimit)
    }
}

impl ReadLimit {
    pub fn map<U, F: FnOnce(u64) -> U>(self, f: F) -> U {
        f(self.0)
    }
}

impl From<ReadLimit> for u64 {
    fn from(w: ReadLimit) -> u64 {
        w.0
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    schemars::JsonSchema,
    Default,
    serde::Serialize,
)]
pub struct BashTimeout(pub Option<u64>);

impl<'de> serde::Deserialize<'de> for BashTimeout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Option<u64>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a positive integer for bash timeout (not a string), or null")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Some(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(E::custom(
                        "invalid bash timeout: negative value not allowed",
                    ));
                }
                Ok(Some(value as u64))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.is_empty() {
                    return Ok(None);
                }
                if let Ok(parsed) = value.parse::<u64>() {
                    Ok(Some(parsed))
                } else {
                    Err(E::custom(format!(
                        "invalid bash timeout: '{}' - must be number",
                        value
                    )))
                }
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }
        }

        deserializer.deserialize_any(Visitor).map(BashTimeout)
    }
}

impl BashTimeout {
    pub fn map<U, F: FnOnce(u64) -> U>(self, f: F) -> Option<U> {
        self.0.map(f)
    }

    pub fn unwrap_or(self, default: u64) -> u64 {
        self.0.unwrap_or(default)
    }

    pub fn or(self, other: BashTimeout) -> BashTimeout {
        if self.0.is_some() {
            self
        } else {
            other
        }
    }
}

impl From<BashTimeout> for Option<u64> {
    fn from(w: BashTimeout) -> Option<u64> {
        w.0
    }
}

/// Custom deserializer for `ReasoningInContextMode` that accepts both legacy
/// boolean values (`true`/`false`) and new string enum values
/// (`"always"`, `"never"`, `"aborted_turns_only"`).
fn deserialize_reasoning_in_context<'de, D>(
    deserializer: D,
) -> Result<Option<crate::settings::ReasoningInContextMode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum BoolOrString {
        Bool(bool),
        String(String),
    }

    let opt = Option::<BoolOrString>::deserialize(deserializer)?;
    Ok(opt.map(|v| match v {
        BoolOrString::Bool(b) => {
            if b {
                crate::settings::ReasoningInContextMode::Always
            } else {
                crate::settings::ReasoningInContextMode::Never
            }
        }
        BoolOrString::String(s) => {
            crate::settings::ReasoningInContextMode::parse_opt(&s).unwrap_or_default()
        }
    }))
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    schemars::JsonSchema,
    Default,
    serde::Serialize,
)]
pub struct SandboxTimeoutSecs(pub Option<u64>);

impl<'de> serde::Deserialize<'de> for SandboxTimeoutSecs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Option<u64>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a positive integer for timeout_secs (not a string), or null")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Some(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(E::custom(
                        "invalid timeout_secs: negative value not allowed",
                    ));
                }
                Ok(Some(value as u64))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.is_empty() {
                    return Ok(None);
                }
                if let Ok(parsed) = value.parse::<u64>() {
                    Ok(Some(parsed))
                } else {
                    Err(E::custom(format!(
                        "invalid timeout_secs: '{}' - must be number",
                        value
                    )))
                }
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }
        }

        deserializer
            .deserialize_any(Visitor)
            .map(SandboxTimeoutSecs)
    }
}

impl SandboxTimeoutSecs {
    pub fn map<U, F: FnOnce(u64) -> U>(self, f: F) -> Option<U> {
        self.0.map(f)
    }

    pub fn unwrap_or(self, default: u64) -> u64 {
        self.0.unwrap_or(default)
    }

    pub fn or(self, other: SandboxTimeoutSecs) -> SandboxTimeoutSecs {
        if self.0.is_some() {
            self
        } else {
            other
        }
    }
}

impl From<SandboxTimeoutSecs> for Option<u64> {
    fn from(w: SandboxTimeoutSecs) -> Option<u64> {
        w.0
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    schemars::JsonSchema,
    Default,
    serde::Serialize,
)]
pub struct SandboxMemoryMb(pub Option<u64>);

impl<'de> serde::Deserialize<'de> for SandboxMemoryMb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Option<u64>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a positive integer for memory_mb (not a string), or null")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Some(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(E::custom("invalid memory_mb: negative value not allowed"));
                }
                Ok(Some(value as u64))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.is_empty() {
                    return Ok(None);
                }
                if let Ok(parsed) = value.parse::<u64>() {
                    Ok(Some(parsed))
                } else {
                    Err(E::custom(format!(
                        "invalid memory_mb: '{}' - must be number",
                        value
                    )))
                }
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }
        }

        deserializer.deserialize_any(Visitor).map(SandboxMemoryMb)
    }
}

impl SandboxMemoryMb {
    pub fn map<U, F: FnOnce(u64) -> U>(self, f: F) -> Option<U> {
        self.0.map(f)
    }

    pub fn unwrap_or(self, default: u64) -> u64 {
        self.0.unwrap_or(default)
    }

    pub fn or(self, other: SandboxMemoryMb) -> SandboxMemoryMb {
        if self.0.is_some() {
            self
        } else {
            other
        }
    }
}

impl From<SandboxMemoryMb> for Option<u64> {
    fn from(w: SandboxMemoryMb) -> Option<u64> {
        w.0
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    schemars::JsonSchema,
    Default,
    serde::Serialize,
)]
pub struct SandboxTmpSizeMb(pub Option<u64>);

impl<'de> serde::Deserialize<'de> for SandboxTmpSizeMb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Option<u64>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a positive integer for tmp_size_mb (not a string), or null")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Some(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(E::custom("invalid tmp_size_mb: negative value not allowed"));
                }
                Ok(Some(value as u64))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.is_empty() {
                    return Ok(None);
                }
                if let Ok(parsed) = value.parse::<u64>() {
                    Ok(Some(parsed))
                } else {
                    Err(E::custom(format!(
                        "invalid tmp_size_mb: '{}' - must be number",
                        value
                    )))
                }
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }
        }

        deserializer.deserialize_any(Visitor).map(SandboxTmpSizeMb)
    }
}

impl SandboxTmpSizeMb {
    pub fn map<U, F: FnOnce(u64) -> U>(self, f: F) -> Option<U> {
        self.0.map(f)
    }

    pub fn unwrap_or(self, default: u64) -> u64 {
        self.0.unwrap_or(default)
    }

    pub fn or(self, other: SandboxTmpSizeMb) -> SandboxTmpSizeMb {
        if self.0.is_some() {
            self
        } else {
            other
        }
    }
}

impl From<SandboxTmpSizeMb> for Option<u64> {
    fn from(w: SandboxTmpSizeMb) -> Option<u64> {
        w.0
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    schemars::JsonSchema,
    Default,
    serde::Serialize,
)]
pub struct SandboxMaxOutputBytes(pub Option<usize>);

impl<'de> serde::Deserialize<'de> for SandboxMaxOutputBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Option<usize>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter
                    .write_str("a positive integer for max_output_bytes (not a string), or null")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(Some(value as usize))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(E::custom(
                        "invalid max_output_bytes: negative value not allowed",
                    ));
                }
                Ok(Some(value as usize))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value.is_empty() {
                    return Ok(None);
                }
                if let Ok(parsed) = value.parse::<usize>() {
                    Ok(Some(parsed))
                } else {
                    Err(E::custom(format!(
                        "invalid max_output_bytes: '{}' - must be number",
                        value
                    )))
                }
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(None)
            }
        }

        deserializer
            .deserialize_any(Visitor)
            .map(SandboxMaxOutputBytes)
    }
}

impl SandboxMaxOutputBytes {
    pub fn map<U, F: FnOnce(usize) -> U>(self, f: F) -> Option<U> {
        self.0.map(f)
    }

    pub fn unwrap_or(self, default: usize) -> usize {
        self.0.unwrap_or(default)
    }

    pub fn or(self, other: SandboxMaxOutputBytes) -> SandboxMaxOutputBytes {
        if self.0.is_some() {
            self
        } else {
            other
        }
    }
}

impl From<SandboxMaxOutputBytes> for Option<usize> {
    fn from(w: SandboxMaxOutputBytes) -> Option<usize> {
        w.0
    }
}

/// The `edit` tool's 1-indexed `startLine` anchor (and the same numbers a
/// `path:LINE` header parses to). Defined in the `blockedit` crate so the
/// `bsed` binary shares one type; re-exported here for the existing
/// `crate::config::EditStartLine` call sites.
pub use blockedit::EditStartLine;

/// How many unchanged lines the `sed` preview attaches around each change.
///
/// Symmetric, and the same reading `grep -C` has: N lines before and N after.
/// Defaults to 1, so a change arrives with one line of context on each side.
/// Symmetry is the point — an asymmetric default gives the *following* line a
/// special role, which is one more thing to hold in one's head about what a
/// block will contain, and it made the trailing line the only place a
/// continuation was visible. One line each way covers the common shapes, the
/// line a call opens on and the line a block closes on, and the balance-growth
/// pass still extends a block to its closing bracket when the construct runs
/// further.
///
/// Set 0 for the bare change, or higher for a wider look; the two sides always
/// move together. Asymmetric context is available on the `bsed` binary as
/// `-t N`, which adds N *extra* lines after — the `sed` tool exposes only the
/// symmetric form.
///
/// Accepts an integer or a numeric string, like [`ReadOffset`], because weak
/// tool-call harnesses stringify numbers often enough that rejecting `"2"` would
/// cost more than tolerating it. Clamped to `MAX_CONTEXT` — past that the block
/// stops being reviewable and starts being the file.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, schemars::JsonSchema, serde::Serialize,
)]
pub struct ContextLines(pub u16);

impl Default for ContextLines {
    /// One line each side — see the type's doc.
    fn default() -> Self {
        ContextLines(0)
    }
}

/// Upper bound on [`ContextLines`].
pub const MAX_CONTEXT: u16 = 20;

impl ContextLines {
    pub fn map<U, F: FnOnce(u16) -> U>(self, f: F) -> U {
        f(self.0)
    }

    /// The value clamped to [`MAX_CONTEXT`].
    pub fn clamped(self) -> u16 {
        self.0.min(MAX_CONTEXT)
    }
}

impl From<ContextLines> for u16 {
    fn from(w: ContextLines) -> u16 {
        w.0
    }
}

impl From<ContextLines> for usize {
    fn from(w: ContextLines) -> usize {
        w.0 as usize
    }
}

impl<'de> Deserialize<'de> for ContextLines {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = u16;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a positive integer for context lines (not a string)")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                u16::try_from(value)
                    .map_err(|_| E::custom(format!("invalid context: {value} - too large")))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value < 0 {
                    return Err(E::custom("invalid context: negative value not allowed"));
                }
                u16::try_from(value as u64)
                    .map_err(|_| E::custom(format!("invalid context: {value} - too large")))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value.trim().parse::<u16>() {
                    Ok(n) => Ok(n),
                    Err(_) => Err(E::custom(format!(
                        "invalid context: '{value}' - must be number"
                    ))),
                }
            }
        }

        deserializer.deserialize_any(Visitor).map(ContextLines)
    }
}

#[cfg(test)]
mod context_lines_tests {
    use super::*;

    fn parse(v: serde_json::Value) -> Result<ContextLines, serde_json::Error> {
        serde_json::from_value(v)
    }

    #[test]
    fn accepts_integer_and_numeric_string() {
        assert_eq!(parse(serde_json::json!(0)).unwrap(), ContextLines(0));
        assert_eq!(parse(serde_json::json!(4)).unwrap(), ContextLines(4));
        // Harnesses stringify numbers often enough that rejecting "4" would
        // cost more than tolerating it.
        assert_eq!(parse(serde_json::json!("4")).unwrap(), ContextLines(4));
        assert_eq!(parse(serde_json::json!(" 4 ")).unwrap(), ContextLines(4));
    }

    #[test]
    fn rejects_negative_and_non_numeric() {
        assert!(parse(serde_json::json!(-1)).is_err());
        assert!(parse(serde_json::json!("two")).is_err());
    }

    #[test]
    fn clamps_to_max() {
        // Silently accepted but bounded: past 20 the "block" is the file.
        let big = parse(serde_json::json!(9999)).unwrap();
        assert_eq!(big.0, 9999);
        assert_eq!(big.clamped(), MAX_CONTEXT);
    }
}

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
    /// Default reasoning level: `off` | `low` | `med` | `high` | `max`.
    /// `None` = no level recorded, which is the same effective state as `off`
    /// (doc/model_settings_v2.md A.5) — the key is written only once the user
    /// actually picks one.
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
    /// Pre-rename spelling of [`Self::default_reasoning_level`]. Read-only:
    /// [`Config::parse`] folds it into the new key when the new one is absent,
    /// and [`SettingsStore::save`](crate::settings::SettingsStore::save) never
    /// writes it back, so the old name disappears on the first save. Kept as a
    /// field only because `Config` is permissive about unknown keys — without
    /// it the old value would be dropped silently instead of migrated.
    #[serde(default, skip_serializing)]
    pub default_thinking_level: Option<String>,
    /// How reasoning is shown: `disabled` | `indicator` | `content`.
    #[serde(default)]
    pub show_reasoning: Option<String>,
    /// Whether reasoning content is included in the session context (the
    /// conversation history sent to the model). `false` (default) drops
    /// How reasoning blocks are kept in the LLM context.
    /// `always` = keep all, `never` = strip all, `aborted_turns_only` = keep only
    /// from aborted/errored turns. Default: `never`. Forced `always` for
    /// Anthropic-family providers (thinking+tool_use replay contract).
    /// See `doc/compaction.md`.
    #[serde(default, deserialize_with = "deserialize_reasoning_in_context")]
    pub reasoning_in_context: Option<crate::settings::ReasoningInContextMode>,
    /// Free-text instruction prepended to the dropped-span transcript when
    /// `/tree <id> compact llm` is run with no trailing text. When unset,
    /// the built-in `DEFAULT_LLM_COMPACT_PROMPT` is used.
    #[serde(default)]
    pub llm_compact_prompt: Option<String>,
    /// Per-model defaults and metadata overrides, keyed by the canonical
    /// `provider:model`. One `[models."provider:model"]` table each; see
    /// `model_settings`. Read through the shared validator, so a typo in a
    /// hand-edited file is a load error naming the key.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub models: std::collections::HashMap<String, crate::model_settings::ModelSettings>,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct ToolSettings {
    #[serde(default)]
    pub auto_allow: Vec<String>,
    #[serde(default)]
    pub sandbox: Option<SandboxConfig>,
}

/// Sandbox configuration for the `bash` (and `agent-browser`) tools.
///
/// See `doc/sandbox.md` for the full design. All fields are optional and
/// merged with built-in defaults by [`SandboxConfig::with_defaults`].
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct SandboxConfig {
    /// Wall-clock timeout (seconds) for the whole command. `None`/0 = no timeout.
    #[serde(default)]
    pub timeout_secs: SandboxTimeoutSecs,
    /// Give the sandbox network access. Default false (network is unshared).
    #[serde(default)]
    pub network: Option<bool>,
    /// RLIMIT_AS in MB (virtual-memory cap). Default none (unlimited).
    #[serde(default)]
    pub memory_mb: SandboxMemoryMb,
    /// Run in a fresh PID namespace. Default true.
    #[serde(default)]
    pub pid_isolation: Option<bool>,
    /// Drop all capabilities. Default true.
    #[serde(default)]
    pub drop_caps: Option<bool>,
    /// Size of the tmpfs mounted at /tmp inside the sandbox (MB). Default 256.
    #[serde(default)]
    pub tmp_size_mb: SandboxTmpSizeMb,
    /// Read-only filesystem bindings. Each entry is either a bare path (mounted
    /// at the same in-sandbox location) or an explicit `{ from, to }` remap.
    /// Paths are resolved relative to the agent CWD (`~` expands to the home
    /// directory). Only the bindings listed here (plus the read-only system
    /// dirs `/usr`, `/bin`, `/lib`, `/lib64`, `/etc`) are visible inside the
    /// sandbox; everything else on the host is hidden (deny-by-default).
    #[serde(default)]
    pub ro: Vec<BindPath>,
    /// Read-write filesystem bindings (same shape as [`SandboxConfig::ro`]).
    /// Writes require an `rw` binding; the non-shell file tools enforce the
    /// same policy as the shell.
    #[serde(default)]
    pub rw: Vec<BindPath>,
    /// Environment-filter policy. See [`EnvPolicy`]. When unset, a built-in
    /// deny-secrets default (see [`DEFAULT_ENV_DISALLOW`]) is applied.
    #[serde(default)]
    pub env: Option<EnvPolicy>,
    /// Maximum combined stdout+stderr (bytes) to retain from a single sandboxed
    /// command. Output beyond this is truncated with a notice. Default:
    /// [`DEFAULT_MAX_OUTPUT_BYTES`].
    #[serde(default)]
    pub max_output_bytes: SandboxMaxOutputBytes,
}

/// Default cap on combined stdout+stderr (bytes) retained per sandboxed
/// command. Output beyond this is truncated with a notice. Overridable via the
/// `[tool_config.bash.sandbox]` `max_output_bytes` setting.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 20 * 1024; // 20 KiB

impl SandboxConfig {
    /// Merge this (user-provided) config with built-in defaults.
    ///
    /// The sandbox is always active — `bwrap` is required at startup and
    /// `run_sandboxed` fails closed if it is missing, so there is no enable
    /// switch to resolve here. This only fills in the remaining defaults.
    pub fn with_defaults(&self) -> SandboxConfig {
        SandboxConfig {
            timeout_secs: self.timeout_secs.or(SandboxTimeoutSecs(Some(120))),
            network: self.network.or(Some(false)),
            memory_mb: self.memory_mb,
            pid_isolation: self.pid_isolation.or(Some(true)),
            drop_caps: self.drop_caps.or(Some(true)),
            tmp_size_mb: self.tmp_size_mb.or(SandboxTmpSizeMb(Some(256))),
            ro: self.ro.clone(),
            rw: self.rw.clone(),
            env: self.env.clone(),
            max_output_bytes: self
                .max_output_bytes
                .or(SandboxMaxOutputBytes(Some(DEFAULT_MAX_OUTPUT_BYTES))),
        }
    }

    /// Effective output size cap in bytes for a sandboxed command.
    ///
    /// Returns the user-configured `max_output_bytes`, falling back to
    /// [`DEFAULT_MAX_OUTPUT_BYTES`]. Used by the bash and grep/rg reader paths
    /// to bound how much child output is kept in memory.
    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES)
    }
}

/// Environment-filter policy for the sandbox.
///
/// Controls which of the parent process's environment variables are visible
/// to the sandboxed `bash`/`agent_browser` command. Modeled as a single
/// mutually-exclusive mode: you cannot combine a blacklist and a whitelist in
/// one policy.
///
/// - `disallow`: start from the full inherited environment and drop names
///   matching any pattern (this is also the built-in default when `env` is
///   unset — see [`DEFAULT_ENV_DISALLOW`]).
/// - `only_allow`: keep only names matching a pattern; everything else is
///   dropped (deny-by-default).
///
/// Patterns are matched against variable *names* (not values) and support a
/// minimal glob: `*` matches any run of characters, `?` matches a single
/// character, and anything else is a literal. Matching is case-sensitive and
/// anchored to the whole name.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct EnvPolicy {
    /// Which filtering mode to apply.
    #[serde(rename = "mode")]
    pub mode: EnvMode,
    /// Name patterns to match, interpreted per `mode`. Empty means "drop
    /// nothing" for `disallow` and "keep nothing" for `only_allow`.
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// The two environment-filtering modes.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvMode {
    /// Drop matching names; keep the rest.
    Disallow,
    /// Keep only matching names; drop the rest.
    OnlyAllow,
}

/// Built-in default `env` policy, applied when the user configures no `env`
/// block. Drops common secret shapes (`*_KEY`, `*_SECRET`, `*_TOKEN`, `AWS_*`,
/// `SSH_AUTH_SOCK`) while leaving the rest of the environment intact, so
/// ordinary commands (which rely on `PATH`, `HOME`, `TERM`, ...) still work.
pub const DEFAULT_ENV_DISALLOW: &[&str] =
    &["*_KEY", "*_SECRET", "*_TOKEN", "AWS_*", "SSH_AUTH_SOCK"];

impl SandboxConfig {
    /// Effective environment policy.
    ///
    /// Returns the user's `env` policy if configured, otherwise the built-in
    /// deny-secrets default ([`DEFAULT_ENV_DISALLOW`]). The default does not
    /// depend on `bwrap` presence; it applies whenever the sandbox runs.
    pub fn env_policy(&self) -> EnvPolicy {
        match &self.env {
            Some(p) => p.clone(),
            None => EnvPolicy {
                mode: EnvMode::Disallow,
                patterns: DEFAULT_ENV_DISALLOW.iter().map(|s| s.to_string()).collect(),
            },
        }
    }
}

/// Compute the `(name, value)` pairs from the parent environment that should be
/// visible inside the sandbox under `policy`.
///
/// `env` is the parent's full environment (e.g. `std::env::vars().collect()`).
/// The result is what `build_bwrap_expr` passes via `--clearenv` + `--setenv`.
pub fn env_vars_to_keep(policy: &EnvPolicy, env: &[(String, String)]) -> Vec<(String, String)> {
    let keep = |name: &str| patterns_match_any(&policy.patterns, name);
    match policy.mode {
        EnvMode::Disallow => env.iter().filter(|(n, _)| !keep(n)).cloned().collect(),
        EnvMode::OnlyAllow => env.iter().filter(|(n, _)| keep(n)).cloned().collect(),
    }
}

/// True if `name` matches any of `patterns` (literal or minimal glob).
fn patterns_match_any(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

/// Minimal glob match: `*` = any run of chars, `?` = single char, else literal.
/// Case-sensitive, anchored to the whole `name`.
fn glob_match(pattern: &str, name: &str) -> bool {
    glob_match_impl(pattern.as_bytes(), name.as_bytes())
}

fn glob_match_impl(pat: &[u8], s: &[u8]) -> bool {
    match (pat.first(), s.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(b'*'), _) => {
            // `*` matches zero or more chars: try consuming 0..=len of `s`.
            (0..=s.len()).any(|i| glob_match_impl(&pat[1..], &s[i..]))
        }
        (Some(b'?'), Some(_)) => glob_match_impl(&pat[1..], &s[1..]),
        (Some(a), Some(b)) if a == b => glob_match_impl(&pat[1..], &s[1..]),
        _ => false,
    }
}

/// A single filesystem binding for the sandbox.
///
/// Expressed either as a bare path (`from == to`, mounted at the same
/// in-sandbox location) or as an explicit remap `{ from, to }`. The read/write
/// mode is *not* carried here — it is implied by which list
/// ([`SandboxConfig::ro`] vs [`SandboxConfig::rw`]) the binding is placed in.
/// TOML accepts either a plain string or a `{ from, to }` table for each entry.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum BindPath {
    /// Bare path, mounted read-only or read-write depending on the list it is
    /// in (`ro` vs `rw`), at the same in-sandbox location.
    Simple(String),
    /// Explicit host→sandbox remap.
    Remap { from: String, to: String },
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BindMode {
    Ro,
    Rw,
}

impl BindMode {
    pub fn is_writable(self) -> bool {
        matches!(self, BindMode::Rw)
    }
}

/// A binding with absolute, fully-resolved host/sandbox paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBind {
    pub mode: BindMode,
    pub from: PathBuf,
    pub to: PathBuf,
}

/// Ordered `to -> from` map used to translate agent-visible (in-sandbox) paths
/// back to host paths, so non-shell tools and the shell see the same files.
///
/// Built once from the resolved bindings; longest `to` prefix wins on lookup.
#[derive(Debug, Clone, Default)]
pub struct RemapTable {
    /// (to, from) pairs, sorted by `to` length descending for prefix matching.
    entries: Vec<(PathBuf, PathBuf)>,
}

impl RemapTable {
    pub fn build(binds: &[ResolvedBind]) -> RemapTable {
        let mut entries: Vec<(PathBuf, PathBuf)> = binds
            .iter()
            .map(|b| (b.to.clone(), b.from.clone()))
            .collect();
        // Longest `to` first so the most specific remap wins on overlap.
        entries.sort_by_key(|b| std::cmp::Reverse(b.0.as_os_str().len()));
        RemapTable { entries }
    }

    /// Rewrite `path` through the table: if it (or an ancestor) matches a `to`
    /// prefix, replace that prefix with the corresponding host `from`.
    pub fn rewrite(&self, path: &Path) -> PathBuf {
        for (to, from) in &self.entries {
            if let Ok(suffix) = path.strip_prefix(to) {
                let mut rewritten = from.clone();
                rewritten.push(suffix);
                return rewritten;
            }
        }
        path.to_path_buf()
    }
}

/// Resolved path policy for the non-shell file tools.
///
/// Built once from the sandbox config (see [`resolve_path_policy`]). When the
/// policy is active (sandbox active with explicit bindings), every filesystem
/// access must fall under a binding, and writes require an `rw` binding. The
/// `remap` table translates agent-visible (in-sandbox) paths like `/tmp` back
/// to host paths so non-shell tools and the shell see the same files.
#[derive(Debug, Clone, Default)]
pub struct PathPolicy {
    binds: Vec<ResolvedBind>,
    remap: RemapTable,
    root: PathBuf,
}

/// Why a [`PathPolicy::check`] call was denied.
///
/// Path-free on purpose: the *reason* is safe to show anyone, and only the
/// caller knows which path form the audience wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDeniedKind {
    /// Not covered by any binding.
    NotCovered,
    /// Covered, but by a read-only binding and a write was requested.
    ReadOnly,
}

impl PolicyDeniedKind {
    fn reason(self) -> &'static str {
        match self {
            Self::NotCovered => "path is not within a permitted binding",
            Self::ReadOnly => "path is read-only (binding is ro)",
        }
    }
}

/// A denial from [`PathPolicy::check`].
///
/// Carries the host path **separately** from the reason, because the two
/// audiences want different forms of it and collapsing them into one string is
/// what leaked. The model must be shown the path it supplied — an in-sandbox
/// path it can act on — while the operator diagnosing a misconfigured binding
/// wants the absolute host path. See `doc/path_leak_risks.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDenied {
    pub kind: PolicyDeniedKind,
    /// The resolved **host** path. Never put this in a message a model can
    /// read; it is data for callers that have an operator-facing sink.
    pub host_path: PathBuf,
}

impl PolicyDenied {
    /// Model-facing: `supplied` is the path exactly as the caller asked for it.
    /// Contains no host path, so it is safe to expose once tool errors stop
    /// being redacted.
    pub fn model_message(&self, supplied: &str) -> String {
        format!("{}: {supplied}", self.kind.reason())
    }

    /// Operator-facing: keeps the absolute host path, which is what makes a
    /// misconfigured binding diagnosable — that path is the one that *would*
    /// have matched a binding.
    pub fn operator_message(&self) -> String {
        format!("{}: {}", self.kind.reason(), self.host_path.display())
    }
}

impl PathPolicy {
    /// Rewrite an agent-visible path to its host location (via the remap
    /// table), then resolve it against the root and normalize components.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let rewritten = self.remap.rewrite(Path::new(path));
        let joined = if rewritten.is_absolute() {
            rewritten
        } else {
            self.root.join(rewritten)
        };
        normalize(&joined)
    }

    /// Resolve a path to an absolute sandbox path **without remapping** to host.
    /// This is used for display purposes so the agent sees sandbox-relative paths.
    pub fn resolve_sandbox(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        normalize(&joined)
    }

    /// Check that `host_path` (already resolved against the root) is covered by
    /// a binding and, if `write`, that the covering binding is `rw`.
    ///
    /// Returns `Ok(())` or a [`PolicyDenied`] carrying the host path as *data*
    /// rather than baked into a sentence. This used to return
    /// `format!("…: {host_path}")`, which put a host path into a string that
    /// one day reaches the model: under a remapping binding the model asks for
    /// `/tmp/f.txt` and would be told `/home/me/tmp/agents/f.txt` — a path it
    /// cannot name back to us, so it would keep retrying with it. Splitting
    /// reason from path lets the caller render the form each audience needs
    /// (`PolicyDenied::model_message` / `operator_message`).
    ///
    /// The operator loses nothing by this: `check`'s result becomes a
    /// `ToolError`, which rig redacts to the kind's default text before it is
    /// displayed, so the host path was never reaching anyone — not the model,
    /// not the scrollback.
    pub fn check(&self, host_path: &Path, write: bool) -> std::result::Result<(), PolicyDenied> {
        // Find the most specific (longest-prefix) covering binding.
        let mut best: Option<&ResolvedBind> = None;
        for b in &self.binds {
            if host_path.starts_with(&b.from) {
                match best {
                    Some(prev) if prev.from.as_os_str().len() >= b.from.as_os_str().len() => {}
                    _ => best = Some(b),
                }
            }
        }
        let denied = |kind| PolicyDenied {
            kind,
            host_path: host_path.to_path_buf(),
        };
        let bind = best.ok_or_else(|| denied(PolicyDeniedKind::NotCovered))?;
        if write && !bind.mode.is_writable() {
            return Err(denied(PolicyDeniedKind::ReadOnly));
        }
        Ok(())
    }
}

/// Normalize `..` / `.` components without touching the filesystem.
fn normalize(p: &Path) -> PathBuf {
    path_util::normalize(p)
}

/// Build a [`PathPolicy`] from a sandbox config. Returns `None` when there are
/// no explicit bindings (permissive default).
pub fn resolve_path_policy(
    cfg: &SandboxConfig,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<PathPolicy> {
    if cfg.ro.is_empty() && cfg.rw.is_empty() {
        return None;
    }
    let binds = resolve_paths(cwd, home, &cfg.ro, &cfg.rw).ok()?;
    Some(PathPolicy {
        remap: RemapTable::build(&binds),
        binds,
        root: cwd.to_path_buf(),
    })
}

/// Resolve a possibly-relative or `~`-prefixed path against `cwd`/`home`.
fn resolve_path(base: &Path, home: Option<&Path>, p: &str) -> PathBuf {
    path_util::resolve_with_home(p, base, home)
}

/// Resolve the `ro`/`rw` binding lists into absolute [`ResolvedBind`]s.
///
/// The mode of each binding is implied by which list it came from (`ro` →
/// read-only, `rw` → read-write). `cwd` is the agent working directory; `home`
/// is used to expand `~`. Relative `from`/`to` are resolved against `cwd`. The
/// operator explicitly lists bindings, so any absolute host path is permitted
/// (the deny-by-default root and the non-shell `validate_path` guard provide
/// the real safety).
pub fn resolve_paths(
    cwd: &Path,
    home: Option<&Path>,
    ro: &[BindPath],
    rw: &[BindPath],
) -> Result<Vec<ResolvedBind>, String> {
    let mut out = Vec::with_capacity(ro.len() + rw.len());
    for spec in ro {
        out.push(resolve_one(cwd, home, spec, BindMode::Ro));
    }
    for spec in rw {
        out.push(resolve_one(cwd, home, spec, BindMode::Rw));
    }
    Ok(out)
}

/// Resolve a single [`BindPath`] (simple or remap) into a [`ResolvedBind`] with
/// the given `mode` (which list the entry lives in).
fn resolve_one(cwd: &Path, home: Option<&Path>, spec: &BindPath, mode: BindMode) -> ResolvedBind {
    let (from, to) = match spec {
        // Simple form: mounted at the same in-sandbox location as the host.
        BindPath::Simple(p) => {
            let from = resolve_path(cwd, home, p);
            (from.clone(), from)
        }
        // Remap form: explicit host (`from`) and in-sandbox (`to`) paths.
        BindPath::Remap { from, to } => {
            (resolve_path(cwd, home, from), resolve_path(cwd, home, to))
        }
    };
    ResolvedBind { mode, from, to }
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
    /// Load config from the given `--config` path, or from the platform config
    /// file (`~/.config/picocode/settings.toml` on Linux,
    /// `%APPDATA%/picocode/settings.toml` on Windows) when `path` is `None`.
    ///
    /// If no config location is available (no `--config` and no home /
    /// application-data directory) or the resolved file does not exist, the
    /// built-in defaults are used (so a fresh install runs without a config
    /// file present).
    pub fn load(path: Option<&str>) -> crate::Result<Self> {
        if let Some(path) = crate::platform::resolve_config_path(path) {
            if path.exists() {
                let content = std::fs::read_to_string(&path).map_err(crate::PicocodeError::Io)?;
                return Self::parse(&content);
            }
        }
        Ok(Config::default())
    }

    /// Deserialize a `settings.toml` body and apply forward migrations.
    ///
    /// The single read path for the config (both `Config::load` and
    /// `SettingsStore::load` go through here) so a migration can never be
    /// applied on one path and forgotten on the other.
    pub fn parse(content: &str) -> crate::Result<Self> {
        // Parse twice: serde for the typed scalar fields, and `toml_edit` for
        // the per-model tables, which go through the shared validator so the
        // file and the $EDITOR route cannot disagree about what is acceptable.
        // The per-model tables are read *first*, deliberately.
        // `ModelSettings` has `deny_unknown_fields`, so serde would also catch
        // a typo — but with a bare "unknown field", no suggestion, and it stops
        // at the first one. Our validator says "did you mean
        // 'temperature'?", names the range when a value is merely out of
        // bounds, and lists every problem at once. Serde remains the backstop
        // for the scalar fields.
        let doc: toml_edit::DocumentMut = content
            .parse()
            .map_err(|e: toml_edit::TomlError| crate::PicocodeError::Toml(e.to_string()))?;
        let models = crate::model_settings::read_from_doc(&doc)
            .map_err(|m| crate::PicocodeError::Toml(format!("invalid per-model settings: {m}")))?;
        let mut config: Config = toml_edit::de::from_str(content)
            .map_err(|e| crate::PicocodeError::Toml(e.to_string()))?;
        config.models = models;
        config.migrate();
        Ok(config)
    }

    /// Fold deprecated keys into their replacements. Idempotent, and a no-op
    /// once the file carries the new spelling — which is what makes it safe to
    /// run on every load rather than once at upgrade time.
    ///
    /// * `default_thinking_level` → `default_reasoning_level` (the `/thinking`
    ///   → `/reasoning` rename, doc/model_settings_v2.md A.11). The new key
    ///   wins when both are present: a file that has been edited since the
    ///   rename must not have its explicit value clobbered by a stale one.
    fn migrate(&mut self) {
        if self.default_reasoning_level.is_none() {
            if let Some(old) = self.default_thinking_level.take() {
                self.default_reasoning_level = Some(old);
            }
        }
        // The old spelling is never carried forward.
        self.default_thinking_level = None;
    }

    pub fn get_bash_auto_allow(&self) -> Vec<String> {
        self.tool_config
            .get("bash")
            .map(|s| s.auto_allow.clone())
            .unwrap_or_default()
    }

    /// Returns the raw (user-provided) sandbox config for the `bash` tool, if any.
    pub fn bash_sandbox_raw(&self) -> Option<&SandboxConfig> {
        self.tool_config
            .get("bash")
            .and_then(|s| s.sandbox.as_ref())
    }
}

pub fn read_prompt(
    prompt: Option<String>,
    prompt_file: Option<String>,
) -> crate::Result<Option<String>> {
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

    fn parse_bind(toml: &str) -> SandboxConfig {
        toml_edit::de::from_str(toml).expect("sandbox config should parse")
    }

    #[test]
    fn bind_simple_ro() {
        // The `ro` list takes bare paths (and `BindPath::Remap`) as read-only.
        let cfg = parse_bind("ro = [\"./src\"]");
        assert_eq!(cfg.ro, vec![BindPath::Simple("./src".to_string())]);
        assert!(cfg.rw.is_empty());
    }

    #[test]
    fn bind_simple_rw() {
        let cfg = parse_bind("rw = [\"./target\"]");
        assert_eq!(cfg.rw, vec![BindPath::Simple("./target".to_string())]);
        assert!(cfg.ro.is_empty());
    }

    #[test]
    fn bind_remap_rw() {
        let cfg = parse_bind("rw = [{ from = \"./cache\", to = \"/var/cache/app\" }]");
        assert_eq!(
            cfg.rw,
            vec![BindPath::Remap {
                from: "./cache".to_string(),
                to: "/var/cache/app".to_string(),
            }]
        );
    }

    #[test]
    fn bind_remap_ro() {
        let cfg = parse_bind("ro = [{ from = \"~/.cargo\", to = \"/opt/cargo\" }]");
        assert_eq!(
            cfg.ro,
            vec![BindPath::Remap {
                from: "~/.cargo".to_string(),
                to: "/opt/cargo".to_string(),
            }]
        );
    }

    /// A denial must carry the host path as *data* and keep it out of the
    /// model-facing sentence. With a remapping binding the two forms differ,
    /// so this is the case that would leak. See `doc/path_leak_risks.md`.
    #[test]
    fn a_denial_keeps_the_host_path_out_of_the_model_message() {
        let cfg = SandboxConfig {
            ro: vec![BindPath::Remap {
                from: "/home/me/tmp/agents".to_string(),
                to: "/tmp".to_string(),
            }],
            ..Default::default()
        };
        let policy = resolve_path_policy(&cfg, Path::new("/work"), None).expect("policy is active");

        // Writing under a read-only binding: denied, and the remap applies, so
        // the host path (`/home/me/...`) and the supplied path (`/tmp/...`)
        // are genuinely different strings.
        let supplied = "/tmp/f.txt";
        let host = policy.resolve(supplied);
        assert_ne!(host, Path::new(supplied), "test needs a real remap");
        let denied = policy.check(&host, true).unwrap_err();
        assert_eq!(denied.host_path, host);

        let model = denied.model_message(supplied);
        assert!(
            model.contains(supplied),
            "model should be shown the path it supplied: {model}"
        );
        assert!(
            !model.contains("/home/me"),
            "model message leaked a host path: {model}"
        );

        let operator = denied.operator_message();
        assert!(
            operator.contains("/home/me"),
            "operator should still get the host path: {operator}"
        );
        // The reason survives in both, so the message stays diagnosable.
        assert!(model.contains("read-only"), "{model}");
        assert!(operator.contains("read-only"), "{operator}");

        // Not covered by any binding at all: same split, no path fallback to
        // a string built from `host_path`.
        let uncovered = "/var/data/f.txt";
        let denied = policy.check(&policy.resolve(uncovered), false).unwrap_err();
        assert_eq!(
            denied.model_message(uncovered),
            format!("path is not within a permitted binding: {uncovered}")
        );
    }

    #[test]
    fn with_defaults_merges() {
        let raw = SandboxConfig {
            timeout_secs: SandboxTimeoutSecs(Some(5)),
            network: None,
            memory_mb: SandboxMemoryMb(Some(512)),
            pid_isolation: None,
            drop_caps: None,
            tmp_size_mb: SandboxTmpSizeMb(None),
            ro: vec![],
            rw: vec![],
            env: None,
            max_output_bytes: SandboxMaxOutputBytes(None),
        };
        let d = raw.with_defaults();
        assert_eq!(d.timeout_secs, SandboxTimeoutSecs(Some(5)));
        assert_eq!(d.network, Some(false));
        assert_eq!(d.memory_mb, SandboxMemoryMb(Some(512)));
        assert_eq!(d.pid_isolation, Some(true));
        assert_eq!(d.drop_caps, Some(true));
        assert_eq!(d.tmp_size_mb, SandboxTmpSizeMb(Some(256)));
        // Output cap defaults when unset.
        assert_eq!(
            d.max_output_bytes,
            SandboxMaxOutputBytes(Some(DEFAULT_MAX_OUTPUT_BYTES))
        );
        // ...and a user value is preserved.
        let raw2 = SandboxConfig {
            max_output_bytes: SandboxMaxOutputBytes(Some(4096)),
            ..Default::default()
        };
        assert_eq!(
            raw2.with_defaults().max_output_bytes,
            SandboxMaxOutputBytes(Some(4096))
        );
    }

    #[test]
    fn default_config_is_always_active() {
        // There is no enable switch: a bare (empty) user config resolves to an
        // active sandbox with the built-in defaults applied. `run()` guarantees
        // `bwrap` is present at startup, so the sandbox can never be silently
        // disabled just because the `[tool_config.bash.sandbox]` block was
        // omitted.
        let raw = SandboxConfig::default();
        let d = raw.with_defaults();
        assert_eq!(d.timeout_secs, SandboxTimeoutSecs(Some(120)));
        assert_eq!(d.network, Some(false));
        assert_eq!(d.pid_isolation, Some(true));
        assert_eq!(d.drop_caps, Some(true));
        assert_eq!(d.tmp_size_mb, SandboxTmpSizeMb(Some(256)));
        assert_eq!(
            d.max_output_bytes,
            SandboxMaxOutputBytes(Some(DEFAULT_MAX_OUTPUT_BYTES))
        );
    }

    #[test]
    fn resolve_paths_resolves_relative_and_home() {
        // `ro` entries resolve read-only; `rw` entries read-write. Mode is
        // implied by which list the binding lives in, not stored per-binding.
        let binds = resolve_paths(
            Path::new("/work"),
            Some(Path::new("/home/u")),
            &[BindPath::Simple(".".to_string())],
            &[
                BindPath::Remap {
                    from: "./cache".to_string(),
                    to: "/var/cache/app".to_string(),
                },
                BindPath::Remap {
                    from: "~/.cargo".to_string(),
                    to: "/opt/cargo".to_string(),
                },
            ],
        )
        .unwrap();
        assert_eq!(binds[0].mode, BindMode::Ro);
        assert_eq!(binds[0].from, Path::new("/work"));
        assert_eq!(binds[0].to, Path::new("/work"));
        assert_eq!(binds[1].mode, BindMode::Rw);
        assert_eq!(binds[1].from, Path::new("/work/cache"));
        assert_eq!(binds[1].to, Path::new("/var/cache/app"));
        assert_eq!(binds[2].mode, BindMode::Rw);
        assert_eq!(binds[2].from, Path::new("/home/u/.cargo"));
        assert_eq!(binds[2].to, Path::new("/opt/cargo"));
    }

    #[test]
    fn remap_table_rewrites_to_host() {
        let binds = resolve_paths(
            Path::new("/work"),
            None,
            &[],
            &[BindPath::Remap {
                from: "./scratch".to_string(),
                to: "/tmp".to_string(),
            }],
        )
        .unwrap();
        assert_eq!(binds[0].mode, BindMode::Rw);
        let table = RemapTable::build(&binds);
        assert_eq!(
            table.rewrite(Path::new("/tmp/x")),
            Path::new("/work/scratch/x")
        );
        // Non-remapped path is unchanged.
        assert_eq!(table.rewrite(Path::new("/work/y")), Path::new("/work/y"));
    }

    // ---- Environment policy ----

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn disallow_drops_matching_keeps_rest() {
        let policy = EnvPolicy {
            mode: EnvMode::Disallow,
            patterns: vec!["*_KEY".into()],
        };
        let kept = env_vars_to_keep(
            &policy,
            &env(&[("ANTHROPIC_API_KEY", "x"), ("PATH", "/b"), ("HOME", "/h")]),
        );
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"ANTHROPIC_API_KEY"));
        assert!(names.contains(&"PATH"));
        assert!(names.contains(&"HOME"));
    }

    #[test]
    fn disallow_empty_keeps_all() {
        let policy = EnvPolicy {
            mode: EnvMode::Disallow,
            patterns: vec![],
        };
        let kept = env_vars_to_keep(&policy, &env(&[("ANTHROPIC_API_KEY", "x"), ("PATH", "/b")]));
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn only_allow_keeps_matching_only() {
        let policy = EnvPolicy {
            mode: EnvMode::OnlyAllow,
            patterns: vec!["PATH".into()],
        };
        let kept = env_vars_to_keep(
            &policy,
            &env(&[("ANTHROPIC_API_KEY", "x"), ("PATH", "/b"), ("HOME", "/h")]),
        );
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["PATH"]);
    }

    #[test]
    fn only_allow_empty_keeps_nothing() {
        let policy = EnvPolicy {
            mode: EnvMode::OnlyAllow,
            patterns: vec![],
        };
        let kept = env_vars_to_keep(&policy, &env(&[("ANTHROPIC_API_KEY", "x"), ("PATH", "/b")]));
        assert!(kept.is_empty());
    }

    #[test]
    fn default_policy_drops_secrets_but_keeps_path() {
        // No `env` configured => built-in deny-secrets default applies.
        let cfg = SandboxConfig::default();
        let policy = cfg.env_policy();
        assert_eq!(policy.mode, EnvMode::Disallow);
        assert_eq!(
            policy.patterns,
            DEFAULT_ENV_DISALLOW
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );

        let kept = env_vars_to_keep(
            &policy,
            &env(&[
                ("ANTHROPIC_API_KEY", "x"),
                ("HF_TOKEN", "y"),
                ("AWS_SECRET_ACCESS_KEY", "z"),
                ("SSH_AUTH_SOCK", "/tmp/a"),
                ("MY_SECRET", "w"),
                ("PATH", "/b"),
                ("HOME", "/h"),
            ]),
        );
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert!(!names.contains(&"ANTHROPIC_API_KEY"));
        assert!(!names.contains(&"HF_TOKEN"));
        assert!(!names.contains(&"AWS_SECRET_ACCESS_KEY"));
        assert!(!names.contains(&"SSH_AUTH_SOCK"));
        // `*_SECRET` glob catches MY_SECRET too.
        assert!(!names.contains(&"MY_SECRET"));
        // Non-secret vars survive.
        assert!(names.contains(&"PATH"));
        assert!(names.contains(&"HOME"));
    }

    #[test]
    fn env_policy_deserializes() {
        let cfg: SandboxConfig = toml_edit::de::from_str(
            "[env]\nmode = \"disallow\"\npatterns = [\"*_KEY\", \"AWS_*\"]\n",
        )
        .expect("env policy should parse");
        let p = cfg.env.expect("env present");
        assert_eq!(p.mode, EnvMode::Disallow);
        assert_eq!(p.patterns, vec!["*_KEY".to_string(), "AWS_*".to_string()]);

        let cfg: SandboxConfig =
            toml_edit::de::from_str("[env]\nmode = \"only_allow\"\npatterns = [\"PATH\"]\n")
                .expect("only_allow should parse");
        let p = cfg.env.expect("env present");
        assert_eq!(p.mode, EnvMode::OnlyAllow);
        assert_eq!(p.patterns, vec!["PATH".to_string()]);
    }

    // ---- p2: TOML settings load ----

    #[test]
    fn load_parses_toml_with_settings_and_recipes() {
        let toml = r#"
# comment preserved on write elsewhere
default_model = "anthropic:claude-3-5-sonnet-20241022"
security_mode = "confirm"
tool_call_limit = 50
default_persona = "architect"
default_reasoning_level = "off"

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
        assert_eq!(cfg.default_reasoning_level.as_deref(), Some("off"));
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
        assert!(cfg.default_reasoning_level.is_none());
    }

    // ---- the /thinking -> /reasoning rename (doc/model_settings_v2.md A.11) ----

    /// A pre-rename file keeps its level: the old key is folded into the new
    /// one, so upgrading never silently resets the user's reasoning level.
    #[test]
    fn migrate_folds_default_thinking_level_into_the_new_key() {
        let cfg = Config::parse("default_thinking_level = \"high\"\n")
            .expect("pre-rename config should parse");
        assert_eq!(cfg.default_reasoning_level.as_deref(), Some("high"));
        assert!(cfg.default_thinking_level.is_none(), "old key is consumed");
    }

    /// Migration is idempotent and never clobbers an explicit new value: a
    /// file edited since the rename keeps what the user last chose.
    #[test]
    fn migrate_lets_the_new_key_win_over_the_old_one() {
        let cfg =
            Config::parse("default_thinking_level = \"high\"\ndefault_reasoning_level = \"low\"\n")
                .expect("both spellings should parse");
        assert_eq!(cfg.default_reasoning_level.as_deref(), Some("low"));
    }

    /// A file with neither key is untouched — the absent key is a valid state
    /// (it means "off"), not a missing one.
    #[test]
    fn migrate_is_a_noop_without_either_key() {
        let cfg = Config::parse("default_model = \"anthropic:x\"\n").expect("should parse");
        assert!(cfg.default_reasoning_level.is_none());
        assert!(cfg.default_thinking_level.is_none());
    }

    /// The migration runs on the `SettingsStore` read path too, not just
    /// `Config::load` — both go through `Config::parse`.
    #[test]
    fn settings_store_load_applies_the_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.toml");
        std::fs::write(&path, "default_thinking_level = \"max\"\n").unwrap();
        let store = crate::settings::SettingsStore::load(&path).expect("load should succeed");
        assert_eq!(
            store.default_reasoning_level.as_deref(),
            Some("max"),
            "the store must see the migrated level"
        );
    }

    #[test]
    fn example_toml_file_parses() {
        // Regression check: the shipped `settings.toml.example` (ported from the
        // old YAML example) must parse under the TOML config loader.
        let content =
            std::fs::read_to_string("settings.toml.example").expect("example file present");
        let cfg: Config = toml_edit::de::from_str(&content).expect("example TOML should parse");
        let bash = cfg
            .tool_config
            .get("bash")
            .expect("bash tool config present");
        assert!(
            bash.sandbox.is_some(),
            "example should include a sandbox block"
        );
        assert_eq!(cfg.tool_call_limit, Some(50));
        assert_eq!(cfg.security_mode.as_deref(), Some("confirm"));
    }

    /// The example's `[models.*]` tables are commented out, so
    /// `example_toml_file_parses` never sees them — and `deny_unknown_fields`
    /// only fires on real content. Uncomment them here and parse, so a typo in
    /// the documented example is a failing test rather than a startup error the
    /// first user hits.
    #[test]
    fn the_examples_per_model_tables_are_real_and_valid() {
        let content =
            std::fs::read_to_string("settings.toml.example").expect("example file present");
        // Take just the per-model section and strip its comment markers. A
        // blanket uncomment would be wrong: the prose in the example is not
        // TOML, and `deny_unknown_fields` would rightly reject it.
        let lines: Vec<&str> = content.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.trim_start().starts_with("# [models."))
            .expect("the example should document a [models.*] table");
        let uncommented: String = lines[start..]
            .iter()
            .take_while(|l| l.trim_start().starts_with('#'))
            .map(|l| {
                // Keep only what is actually TOML — a table header or a
                // `key = value`. The prose between them documents the
                // section and would be a parse error uncommented.
                let body = l.trim_start().strip_prefix('#').unwrap_or("").trim();
                let is_toml = body.starts_with('[')
                    || body.split_once('=').is_some_and(|(k, _)| {
                        let k = k.trim();
                        !k.is_empty()
                            && k.chars().next().is_some_and(char::is_alphabetic)
                            && k.chars().all(|c| c.is_alphanumeric() || c == '_')
                    });
                if is_toml {
                    format!("{body}\n")
                } else {
                    "\n".to_string()
                }
            })
            .collect();
        let cfg: Config = toml_edit::de::from_str(&uncommented)
            .unwrap_or_else(|e| panic!("the example's uncommented lines must parse: {e}"));
        assert!(
            !cfg.models.is_empty(),
            "the example should document at least one model table"
        );
        for id in cfg.models.keys() {
            assert!(id.contains(':'), "a model key must be provider:model: {id}");
        }
        // Spot-check that the two kinds of setting are both documented, since
        // they behave differently and that distinction is the whole point of
        // the section.
        assert!(
            cfg.models.values().any(|r| r.reasoning.is_some()),
            "a default should be shown"
        );
        assert!(
            cfg.models.values().any(|r| r.context_length.is_some()),
            "a display override should be shown"
        );
    }
}
