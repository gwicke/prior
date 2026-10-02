//! Per-turn overrides typed at the tail of a prompt (doc/model_settings_v2.md
//! A.10).
//!
//! ```text
//! explain this function
//! /temperature 0.2
//! /top_p 0.9
//! ```
//!
//! A pure, provider-blind parser: it knows the syntax and the value ranges, not
//! what any model supports. A level the model cannot serve is left as requested
//! and remapped downstream, in the one place that knows the model's effort
//! table — so the warning can name the model that moved it, which this layer
//! cannot.
//!
//! Nothing here mutates anything. An override is a value applied to one
//! request; because it never touches the live params it cannot leak into
//! `config.params`, the per-model memory, or the next turn.

use crate::params::{ModelParams, ReasoningLevel};
use crate::v25_number_param::{parse_value, NumberParam};

/// Per-turn overrides, last-wins per key. A `None` field means "not
/// overridden", which is distinct from an override of `None` — the latter is
/// not expressible, and does not need to be: `/temperature` with no value is
/// not an override line.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TurnOverrides {
    pub reasoning: Option<ReasoningLevel>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
}

impl TurnOverrides {
    /// True when nothing was overridden, so the caller can skip the whole
    /// per-request recomposition on an ordinary turn.
    pub fn is_empty(&self) -> bool {
        self.reasoning.is_none() && self.temperature.is_none() && self.top_p.is_none()
    }

    /// `live` with the overrides applied, per field. Never mutates `live`.
    ///
    /// A `/reasoning` override is returned as *requested*, not as remapped.
    /// The remap lives in [`crate::params::additional_params`], the one place
    /// that knows both the provider's spelling of each level and the model's
    /// own effort table; deciding it here too would be a second implementation
    /// free to disagree with the first. So the caller applies the overrides,
    /// asks `additional_params` what it will actually send, and reports a
    /// remap from the difference.
    pub fn apply(&self, live: &ModelParams) -> ModelParams {
        ModelParams {
            reasoning: self.reasoning.or(live.reasoning),
            temperature: self.temperature.or(live.temperature),
            top_p: self.top_p.or(live.top_p),
        }
    }
}

/// The outcome of [`split`].
pub struct Split {
    /// The prompt as committed, with the override lines removed.
    pub text: String,
    pub overrides: TurnOverrides,
    /// Shown once, then the turn proceeds. A stripped-but-invalid line warns
    /// rather than staying in the prompt, so no override text ever reaches the
    /// model and nothing is dropped silently.
    pub warnings: Vec<String>,
}

/// Split a submitted prompt into its text and its trailing override lines.
///
/// From the end of the input, walk lines backwards. A line is an override iff
/// it is exactly `<cmd> <value>` — two whitespace-separated tokens, `cmd` one
/// of `/reasoning`, `/temperature`, `/top_p`. Stop at the first line that is
/// not; everything from there down is prompt text. Trailing blank lines are
/// trimmed first. If nothing matched, the input comes back untouched.
///
/// Syntactically valid but semantically wrong (`/temperature 3.0`,
/// `/reasoning bogus`) is still stripped, and warns — the turn continues.
/// Duplicates: the last wins, and each earlier one warns.
///
/// Walking backwards is what makes "last wins" fall out for free: the last
/// occurrence in the text is the first one seen.
pub fn split(input: &str) -> Split {
    let mut lines: Vec<&str> = input.lines().collect();
    // Trailing blank lines are not prompt text, and an override after them is
    // still an override — so drop them before walking back.
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }

    let mut overrides = TurnOverrides::default();
    let mut warnings = Vec::new();
    let mut cut = lines.len();
    for (i, line) in lines.iter().enumerate().rev() {
        let Some((cmd, value)) = parse_override_line(line) else {
            break;
        };
        cut = i;
        apply_line(cmd, value, &mut overrides, &mut warnings);
    }

    Split {
        // Trailing whitespace goes: once the override lines are removed, any
        // blank line left at the end is a separator the user typed to make room
        // for them, and sending it (or persisting it) is just noise.
        text: lines[..cut].join("\n").trim_end().to_string(),
        overrides,
        warnings,
    }
}

/// `<cmd> <value>`, or `None` if the line is not an override.
///
/// Exactly two tokens: a third means it was prose that happened to start with
/// a command, and treating that as an override would silently delete the user's
/// text.
fn parse_override_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let cmd = parts.next()?;
    let value = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    match cmd {
        "/reasoning" | "/temperature" | "/top_p" => Some((cmd, value)),
        _ => None,
    }
}

fn apply_line(cmd: &str, value: &str, out: &mut TurnOverrides, warnings: &mut Vec<String>) {
    match cmd {
        "/temperature" => set_number(
            NumberParam::Temperature,
            value,
            &mut out.temperature,
            warnings,
        ),
        "/top_p" => set_number(NumberParam::TopP, value, &mut out.top_p, warnings),
        "/reasoning" => match value.parse::<ReasoningLevel>() {
            Ok(level) => {
                if out.reasoning.is_some() {
                    warnings.push(format!(
                        "warning: /reasoning {value} ignored (a later /reasoning wins)"
                    ));
                } else {
                    out.reasoning = Some(level);
                }
            }
            Err(_) => warnings.push(format!(
                "warning: /reasoning {value} ignored (expected off | low | med | high | max)"
            )),
        },
        _ => unreachable!("parse_override_line admits only the three commands"),
    }
}

fn set_number(param: NumberParam, value: &str, slot: &mut Option<f64>, warnings: &mut Vec<String>) {
    // The same validator the menu uses, so a value `/temperature` accepts is
    // never rejected here and vice versa.
    match parse_value(param, value) {
        Ok(Some(n)) => {
            if slot.is_some() {
                warnings.push(format!(
                    "warning: /{} {value} ignored (a later one wins)",
                    param.name()
                ));
            } else {
                *slot = Some(n);
            }
        }
        // An empty value clears rather than sets; there is no way to spell
        // "override to unset" in a single line, so treat it as not-an-override
        // that happens to be syntactically fine.
        Ok(None) => {}
        Err(msg) => warnings.push(format!("warning: /{} ignored ({msg})", param.name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> TurnOverrides {
        split(s).overrides
    }

    // ---- the grammar ----

    #[test]
    fn an_ordinary_prompt_is_untouched() {
        let s = split("explain this function");
        assert_eq!(s.text, "explain this function");
        assert!(s.overrides.is_empty());
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
    }

    #[test]
    fn trailing_overrides_are_stripped_and_the_text_kept() {
        let s = split("explain this function\n/temperature 0.2\n/top_p 0.9");
        assert_eq!(s.text, "explain this function");
        assert_eq!(s.overrides.temperature, Some(0.2));
        assert_eq!(s.overrides.top_p, Some(0.9));
        assert!(s.warnings.is_empty());
    }

    /// The menu commands are ordinary prompts when they are not at the tail.
    /// Walking back stops at the first non-override line, so anything above it
    /// is text — including something that looks exactly like an override.
    #[test]
    fn scanning_stops_at_the_first_non_override_line() {
        let s = split("/temperature 0.2\nwhat does this do?\n/top_p 0.9");
        assert_eq!(s.text, "/temperature 0.2\nwhat does this do?");
        assert_eq!(s.overrides.top_p, Some(0.9));
        assert_eq!(s.overrides.temperature, None);
    }

    /// Three tokens is prose, not an override. Treating it as one would delete
    /// the user's text without saying so.
    #[test]
    fn a_three_token_line_is_not_an_override() {
        let s = split("hi\n/temperature 0.2 and some more");
        assert_eq!(s.text, "hi\n/temperature 0.2 and some more");
        assert!(s.overrides.is_empty());
    }

    #[test]
    fn trailing_blank_lines_are_trimmed_before_scanning() {
        let s = split("hello\n\n/temperature 0.4\n\n\n");
        assert_eq!(s.text, "hello");
        assert_eq!(s.overrides.temperature, Some(0.4));
    }

    #[test]
    fn an_unknown_command_is_just_text() {
        let s = split("hello\n/bogus 1");
        assert_eq!(s.text, "hello\n/bogus 1");
        assert!(s.overrides.is_empty());
    }

    // ---- values ----

    #[test]
    fn all_three_keys_parse() {
        let o = t("go\n/reasoning high\n/temperature 1.5\n/top_p 0.25");
        assert_eq!(o.reasoning, Some(ReasoningLevel::High));
        assert_eq!(o.temperature, Some(1.5));
        assert_eq!(o.top_p, Some(0.25));
    }

    /// Out of range is stripped *and* warned — the text must not reach the
    /// model, and the user must be told it did not take.
    #[test]
    fn an_out_of_range_value_warns_and_is_stripped() {
        let s = split("hello\n/temperature 3.0");
        assert_eq!(s.text, "hello");
        assert_eq!(s.overrides.temperature, None);
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("3.0"), "{:?}", s.warnings);
        assert!(s.warnings[0].contains("0 and 2"), "{:?}", s.warnings);
    }

    #[test]
    fn a_non_numeric_value_warns() {
        let s = split("hello\n/top_p abc");
        assert_eq!(s.text, "hello");
        assert_eq!(s.warnings.len(), 1);
        assert!(
            s.warnings[0].contains("must be a number"),
            "{:?}",
            s.warnings
        );
    }

    #[test]
    fn an_unknown_reasoning_level_warns() {
        let s = split("hello\n/reasoning bogus");
        assert_eq!(s.text, "hello");
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("bogus"), "{:?}", s.warnings);
    }

    /// Last wins, and the earlier one says so rather than vanishing.
    #[test]
    fn duplicates_keep_the_last_and_warn_about_the_earlier() {
        let s = split("hello\n/temperature 0.2\n/temperature 0.8");
        assert_eq!(s.text, "hello");
        assert_eq!(s.overrides.temperature, Some(0.8));
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("0.2"), "{:?}", s.warnings);
    }

    // ---- application ----

    /// Overrides are per field: a `/temperature` line leaves reasoning alone.
    #[test]
    fn apply_is_per_field() {
        let live = ModelParams {
            reasoning: Some(ReasoningLevel::Med),
            temperature: Some(0.7),
            top_p: None,
        };
        let o = TurnOverrides {
            temperature: Some(0.1),
            ..Default::default()
        };
        let eff = o.apply(&live);
        assert_eq!(eff.temperature, Some(0.1));
        assert_eq!(eff.reasoning, Some(ReasoningLevel::Med), "inherited");
        assert_eq!(eff.top_p, None);
    }

    /// The whole point: applying an override must not disturb the live params,
    /// or it would leak into the next turn and into the per-model memory.
    #[test]
    fn apply_does_not_mutate_the_live_params() {
        let live = ModelParams {
            reasoning: Some(ReasoningLevel::Med),
            temperature: Some(0.7),
            top_p: Some(0.5),
        };
        let before = live;
        TurnOverrides {
            reasoning: Some(ReasoningLevel::Max),
            temperature: Some(0.0),
            top_p: Some(1.0),
        }
        .apply(&live);
        assert_eq!(live, before);
    }

    #[test]
    fn an_override_can_clear_a_live_value_with_zero() {
        // `Some(0.0)` is a real override, distinct from "not overridden" —
        // hence `Option<f64>` fields on the live params rather than a plain f64.
        let live = ModelParams {
            temperature: Some(0.7),
            ..Default::default()
        };
        let eff = TurnOverrides {
            temperature: Some(0.0),
            ..Default::default()
        }
        .apply(&live);
        assert_eq!(eff.temperature, Some(0.0));
    }

    #[test]
    fn is_empty_reflects_whether_anything_was_parsed() {
        assert!(TurnOverrides::default().is_empty());
        assert!(!t("go\n/top_p 0.5").is_empty());
    }

    // ---- the no-text case the REPL has to handle ----

    /// Overrides only, no prompt. The caller must notice: there is nothing to
    /// send, and running a turn would create a session for a request that
    /// never happens.
    #[test]
    fn an_overrides_only_prompt_yields_empty_text() {
        let s = split("/temperature 0.2\n/top_p 0.9");
        assert!(s.text.trim().is_empty());
        assert_eq!(s.overrides.temperature, Some(0.2));
    }
}
