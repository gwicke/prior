//! The edit-op types that cross the wire, and the 1-based line anchor they
//! carry.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A single targeted text replacement within a file.
#[derive(Debug, Deserialize, Serialize, JsonSchema, Default, Clone)]
pub struct EditOp {
    /// Exact text to replace. Must be unique in the file — or become unique
    /// after matches claimed by other edits in the same batch are excluded —
    /// and must not overlap any other edit's `old_text` in the same call
    /// (overlaps are rejected, not silently re-aimed).
    #[serde(rename = "oldText")]
    pub old_text: String,
    /// Replacement text.
    #[serde(rename = "newText")]
    pub new_text: String,
    /// Optional *anchor* line (1-indexed) in the original file. It does not
    /// restrict the search; it disambiguates. When `old_text` occurs more than
    /// once, the occurrence whose start line is closest to this line is chosen
    /// when within a small, context-scaled tolerance (see
    /// [`crate::resolve::anchor_tolerance`]) or — for a stale anchor — when
    /// decisively nearer than the second candidate (see
    /// [`crate::diagnose::anchor_accepts`]). The `old_text` end is implied by
    /// its own length, so there is no end line. A `path:LINE` block header
    /// supplies this per-block in the blob form.
    #[serde(rename = "startLine", default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<EditStartLine>,
}

/// A 1-indexed anchor line, validated on deserialize.
///
/// Weak harnesses sometimes stringify numbers, so a numeric string is accepted
/// like an integer. `0` and negatives are rejected: an anchor is a statement
/// about a real line, and silently accepting `0` would mean "before the file".
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
pub struct EditStartLine(pub u64);

impl<'de> Deserialize<'de> for EditStartLine {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = u64;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a positive integer for start line (1-indexed, not a string)")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value == 0 {
                    return Err(E::custom("invalid start line: must be >= 1 (1-indexed)"));
                }
                Ok(value)
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if value <= 0 {
                    return Err(E::custom("invalid start line: must be >= 1 (1-indexed)"));
                }
                Ok(value as u64)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if let Ok(parsed) = value.parse::<u64>() {
                    if parsed == 0 {
                        return Err(E::custom("invalid start line: must be >= 1 (1-indexed)"));
                    }
                    Ok(parsed)
                } else {
                    Err(E::custom(format!(
                        "invalid start line: '{}' - must be number",
                        value
                    )))
                }
            }
        }

        deserializer.deserialize_any(Visitor).map(EditStartLine)
    }
}

impl EditStartLine {
    pub fn map<U, F: FnOnce(u64) -> U>(self, f: F) -> U {
        f(self.0)
    }
}

impl From<EditStartLine> for u64 {
    fn from(w: EditStartLine) -> u64 {
        w.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_zero_and_negative() {
        assert!(serde_json::from_value::<EditStartLine>(json!(0)).is_err());
        assert!(serde_json::from_value::<EditStartLine>(json!(-3)).is_err());
    }

    #[test]
    fn accepts_integer_and_numeric_string() {
        assert_eq!(
            serde_json::from_value::<EditStartLine>(json!(7)).unwrap().0,
            7
        );
        assert_eq!(
            serde_json::from_value::<EditStartLine>(json!("7"))
                .unwrap()
                .0,
            7
        );
    }

    #[test]
    fn rejects_non_numeric_string() {
        assert!(serde_json::from_value::<EditStartLine>(json!("x")).is_err());
    }
}
