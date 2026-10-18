//! The "edit settings via toml" routine (doc/model_settings_v2.md A.9).
//!
//! One reusable round-trip: render a record as a commented TOML fragment, hand
//! it to `$EDITOR`, parse the result, and hand back a validated record. It is
//! deliberately *not* a method on any node — the per-model settings table is
//! the first caller, and a `/set` bulk edit or a future per-session table is
//! meant to be the others, so the only thing this module knows about the host
//! is how to open an editor.
//!
//! What it does **not** do is write anything. The editor round-trip *is* the
//! confirmation: nothing is persisted before the user quits the editor, and
//! nothing is discarded after. That is why it can return `Done` rather than
//! `Prepared` — unlike compaction, there is no destructive half to guard.
//!
//! The fragment is self-describing: every key carries a one-line comment, the
//! user's own comments ride through from the file, and everything unset is
//! shown *commented out* with what the catalog currently reports — so
//! uncommenting a line is how you take a value, and deleting a line is how you
//! clear one.

use crate::app_state::AppState;
use crate::editor::EditorMode;
use crate::model_settings::{self, ModelSettings};

/// What an edit round-trip produced.
pub enum EditOutcome {
    /// The user accepted a new record. It has **not** been written; the caller
    /// decides where it goes (`SettingsStore::put_model_settings`).
    Updated(ModelSettings),
    /// Nothing to do: the editor was abandoned, or the text came back
    /// identical to what we offered.
    Unchanged,
    /// The text was edited but does not parse. The message is ready to show
    /// verbatim and lists *every* problem, not just the first.
    Invalid(String),
    /// The editor could not be run (no interactive terminal).
    Unavailable(String),
}

/// Offer `current` for editing and return the validated result.
///
/// `hints` supplies the commented-out suggestions (in practice, the catalog's
/// facts for the model). It never becomes a value on its own: a suggestion the
/// user did not uncomment is not a setting.
pub fn edit_fragment(
    app_state: &AppState,
    model_id: &str,
    current: &ModelSettings,
    hints: &ModelSettings,
) -> EditOutcome {
    let rendered = model_settings::render_fragment_with_generated(model_id, current, hints);
    let offered = rendered.text;
    let edited = match app_state.edit_text(&offered, EditorMode::Edit) {
        Ok(text) => text,
        // No interactive tty (quiet mode, a pipe, a test): we cannot ask, so
        // we do not guess. Reporting `Unavailable` rather than `Unchanged`
        // matters — the caller can tell the user why nothing happened.
        Err(e) => return EditOutcome::Unavailable(e.to_string()),
    };
    if edited == offered {
        return EditOutcome::Unchanged;
    }
    // `offered` is valid TOML by construction (we rendered it), so a parse
    // failure here is our bug, not the user's — but report it as invalid
    // rather than panicking, because it would be a bad time to crash.
    match model_settings::parse_fragment(&edited) {
        Err(msg) => EditOutcome::Invalid(msg),
        Ok(mut parsed) => {
            // Carry comments the user did not touch, so saving does not drop
            // a note they left in the file. Keys they *edited* keep whatever
            // they wrote, which `parse_fragment` already captured.
            for (key, comment) in &current.comments {
                parsed
                    .comments
                    .entry(key.clone())
                    .or_insert_with(|| comment.clone());
            }
            // The fragment we offered carried comments of our own (the header,
            // hints, commented suggestions). Parse them away by identity: the
            // user may leave them untouched, and they must not become part of
            // the stored record.
            model_settings::strip_generated_comments(&mut parsed, &rendered.generated_comments);
            EditOutcome::Updated(parsed)
        }
    }
}

/// A short, one-line summary of what a record holds, for a menu row or a
/// confirmation message: `reasoning high, temperature 0.8`. An empty record
/// summarizes as "not configured", which is a real state worth naming.
pub fn summarize(settings: &ModelSettings) -> String {
    let mut parts = Vec::new();
    if let Some(r) = settings.reasoning {
        parts.push(format!("reasoning {}", r.as_str()));
    }
    if let Some(t) = settings.temperature {
        parts.push(format!(
            "temperature {}",
            crate::v25_number_param::fmt_value(t)
        ));
    }
    if let Some(p) = settings.top_p {
        parts.push(format!("top_p {}", crate::v25_number_param::fmt_value(p)));
    }
    if let Some(r) = settings.routing() {
        parts.push(format!("routing {}", summarize_routing(r)));
    }
    // Metadata overrides are counted, not named — the record is settings, not
    // a catalogue, and listing nine indices would bury the three knobs the
    // user came to change.
    let overrides = [
        settings.context_length.is_some(),
        settings.prompt_cost.is_some(),
        settings.completion_cost.is_some(),
        settings.coding_index.is_some(),
        settings.intelligence_index.is_some(),
        settings.agentic_index.is_some(),
        settings.description.is_some(),
        settings.supports_reasoning.is_some(),
        settings.reasoning_levels.is_some(),
    ]
    .iter()
    .filter(|set| **set)
    .count();
    if parts.is_empty() && overrides == 0 {
        return "not configured".to_string();
    }
    if overrides > 0 {
        parts.push(format!("+{overrides} metadata"));
    }
    parts.join(", ")
}

/// The routing table's set preferences, compactly: `only=a,b allow_fallbacks=false`.
/// A request-shaping fact, so it ranks with reasoning/temperature rather than
/// with the counted metadata overrides.
pub fn summarize_routing(r: &crate::model_settings::Routing) -> String {
    let mut fields = Vec::new();
    for (name, list) in [
        ("only", &r.only),
        ("order", &r.order),
        ("ignore", &r.ignore),
    ] {
        if let Some(v) = list {
            fields.push(format!("{name}={}", v.join(",")));
        }
    }
    for (name, flag) in [
        ("allow_fallbacks", r.allow_fallbacks),
        ("require_parameters", r.require_parameters),
    ] {
        if let Some(v) = flag {
            fields.push(format!("{name}={v}"));
        }
    }
    fields.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ModelSettings {
        ModelSettings {
            reasoning: Some(crate::params::ReasoningLevel::High),
            temperature: Some(0.8),
            top_p: None,
            context_length: Some(1_048_576),
            ..Default::default()
        }
    }

    #[test]
    fn summarizes_params_and_metadata() {
        let s = summarize(&record());
        assert!(s.contains("reasoning high"), "{s}");
        assert!(s.contains("temperature 0.8"), "{s}");
        assert!(!s.contains("top_p"), "unset must not appear: {s}");
        assert!(s.contains("metadata"), "the context override counts: {s}");
    }

    #[test]
    fn an_empty_record_says_not_configured() {
        assert_eq!(summarize(&ModelSettings::default()), "not configured");
    }

    /// Routing is named, not counted: it shapes the request, so it belongs
    /// beside reasoning/temperature.
    #[test]
    fn routing_is_named_in_the_summary() {
        let s = summarize(&ModelSettings {
            routing: Some(crate::model_settings::Routing {
                only: Some(vec!["deepinfra/fp8/flex".into()]),
                allow_fallbacks: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(s, "routing only=deepinfra/fp8/flex allow_fallbacks=false");
    }

    #[test]
    fn a_params_only_record_reports_no_metadata() {
        let s = summarize(&ModelSettings {
            reasoning: Some(crate::params::ReasoningLevel::Low),
            ..Default::default()
        });
        assert_eq!(s, "reasoning low");
    }
}
