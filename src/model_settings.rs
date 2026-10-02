//! Per-model settings: the `[models."provider:model"]` tables in
//! `settings.toml` (doc/model_settings_v2.md A.2 / A.5).
//!
//! One record per model, holding two kinds of fact:
//!
//! * **params** — the reasoning level, temperature and top_p to apply when
//!   the model is first activated in a session. A field that is *absent* is
//!   not "off" and not "0": it means "no opinion here", and the seeding rule
//!   (A.6) falls through to the global default or the built-in.
//! * **metadata overrides** — context length, prices, indices, the
//!   description, the supported effort list. Used only where set; the catalog
//!   fills whatever is left, so a partial table is the normal case. These
//!   exist for models OpenRouter does not cover, and for correcting it.
//!
//! Everything here is pure text-in / struct-out: no file IO, no terminal, no
//! `AppState`. The `$EDITOR` round-trip lives in `settings_editor`, which is
//! the only part that needs a tty.

use std::collections::BTreeMap;
use std::collections::HashMap;

use crate::params::ReasoningLevel;

/// The table name in `settings.toml`. Keys are quoted because model ids
/// contain `/` and `:` (`[models."openrouter:tencent/hy3:free"]`).
pub const MODELS_TABLE: &str = "models";

/// One model's stored defaults and metadata overrides.
///
/// `deny_unknown_fields` is deliberate: a typo in a hand-edited settings file
/// is a **loud** error naming the key, never a silently ignored line. It is
/// scoped to this struct — the top-level `Config` stays permissive, as it has
/// always been.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    // --- params: the same spelling as ModelParams ---
    /// `off` | `low` | `med` | `high` | `max`. Absent = no opinion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    // --- metadata overrides: used only where set ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coding_index: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intelligence_index: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agentic_index: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// True when the model exposes any reasoning effort. Kept separate from
    /// `reasoning_levels` so "yes, but here is what" is expressible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_reasoning: Option<bool>,
    /// The efforts this model accepts, e.g. `["low","high","max"]`. Drives
    /// backend effort remapping (A.4) without per-model code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_levels: Option<Vec<String>>,

    /// Leading comment block per key, carried across saves and through the
    /// editor round-trip. Never serialized: it is rebuilt from the file
    /// whenever the file is read, so it cannot go stale.
    #[serde(skip)]
    pub comments: BTreeMap<String, String>,
}

impl ModelSettings {
    /// The params half — what the agent actually sends. The split keeps the
    /// seeding rule (A.6) from ever having to know about metadata.
    pub fn params(&self) -> crate::params::ModelParams {
        crate::params::ModelParams {
            reasoning: self.reasoning,
            temperature: self.temperature,
            top_p: self.top_p,
        }
    }

    /// Overlay the metadata half onto a catalog entry. `None` fields keep the
    /// catalog's value, so a partial table is normal and expected.
    pub fn apply_to_entry(&self, e: &mut crate::model_catalog::ModelEntry) {
        // `where set` semantics: an absent field keeps the catalog's value, so
        // a partial table is the normal case and a complete one the exception.
        macro_rules! over {
            ($field:ident) => {
                if let Some(v) = self.$field {
                    e.$field = Some(v);
                }
            };
        }
        over!(context_length);
        over!(prompt_cost);
        over!(completion_cost);
        over!(coding_index);
        over!(intelligence_index);
        over!(agentic_index);
        // `description` is the one non-Copy field, so it is copied rather
        // than moved out of the shared reference.
        if let Some(d) = self.description.clone() {
            e.description = Some(d);
        }
    }

    /// The full set of keys this struct accepts, in the order the editor
    /// fragment lists them. Also the vocabulary the validator checks against,
    /// so a typo suggestion can only ever point at a real key.
    pub const KEYS: &'static [&'static str] = &[
        "reasoning",
        "temperature",
        "top_p",
        "context_length",
        "prompt_cost",
        "completion_cost",
        "coding_index",
        "intelligence_index",
        "agentic_index",
        "description",
        "supports_reasoning",
        "reasoning_levels",
    ];

    /// A one-line hint per key, shown as a comment above it in the editor
    /// fragment so `$EDITOR` alone explains the file.
    pub fn key_hint(key: &str) -> &'static str {
        match key {
            "reasoning" => "off | low | med | high | max",
            "temperature" => "0.0 - 2.0 (omit to use the provider's own default)",
            "top_p" => "0.0 - 1.0 (omit to use the provider's own default)",
            "context_length" => "context window in tokens",
            "prompt_cost" => "USD per million input tokens",
            "completion_cost" => "USD per million output tokens",
            "coding_index" => "Artificial Analysis coding index (0-100)",
            "intelligence_index" => "Artificial Analysis intelligence index (0-100)",
            "agentic_index" => "Artificial Analysis agentic index (0-100)",
            "description" => "what the model is",
            "supports_reasoning" => "true | false",
            "reasoning_levels" => "the efforts this model accepts",
            _ => "",
        }
    }

    /// True when the record holds nothing at all — the shape of a model the
    /// user has never configured. Such a table is not worth writing.
    pub fn is_empty(&self) -> bool {
        self.reasoning.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.context_length.is_none()
            && self.prompt_cost.is_none()
            && self.completion_cost.is_none()
            && self.coding_index.is_none()
            && self.intelligence_index.is_none()
            && self.agentic_index.is_none()
            && self.description.is_none()
            && self.supports_reasoning.is_none()
            && self.reasoning_levels.is_none()
    }
}

/// Every known key, for the validator.
const KNOWN_KEYS: &[&str] = ModelSettings::KEYS;

/// Levenshtein distance, capped at `max` — only used to decide whether a
/// "did you mean" is close enough to be worth showing.
fn edit_distance(a: &str, b: &str, max: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    if d <= max {
        Some(d)
    } else {
        None
    }
}

/// The closest known key to `typo`, if one is close enough to be a likely
/// intent. Returns the *name*; the caller words the message.
pub fn suggest_key(typo: &str) -> Option<&'static str> {
    // A distance of 3 covers every plausible transposition ("tomperature") and
    // abbreviation without waving at unrelated keys.
    let mut best: Option<(usize, &'static str)> = None;
    for key in KNOWN_KEYS {
        if let Some(d) = edit_distance(typo, key, 3) {
            if best.map(|(bd, _)| d < bd).unwrap_or(true) {
                best = Some((d, key));
            }
        }
    }
    best.map(|(_, key)| key)
}

// ---------------------------------------------------------------------------
// Parsing a fragment: every error, not just the first
// ---------------------------------------------------------------------------

/// Parse one model's TOML fragment into a validated record.
///
/// Walks the table key by key rather than handing the text to serde, for two
/// reasons: serde stops at the first bad key, and the user who just typed
/// three mistakes should see all three. Each error names the key, what was
/// found, and what was expected; an unknown key gets a "did you mean".
pub fn parse_fragment(text: &str) -> Result<ModelSettings, String> {
    let doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("could not parse as TOML: {e}"))?;

    let mut out = ModelSettings::default();
    let mut errors: Vec<String> = Vec::new();

    for (key, item) in doc.as_table().iter() {
        // A comment above the key is data we want back, not a setting. It
        // lives on the key's decor, so it needs the table to reach.
        if let Some(prefix) = comment_prefix(doc.as_table(), key) {
            // Verbatim, trailing newline included: that newline is the
            // separator between the comment and its key, and trimming it
            // welds them together on the way back out.
            if !prefix.trim().is_empty() {
                out.comments.insert(key.to_string(), prefix);
            }
        }
        let line = line_of_key(text, key);
        let at = |msg: String| match line {
            Some(l) => format!("line {l}: {msg}"),
            None => msg,
        };
        if !KNOWN_KEYS.contains(&key) {
            let hint = match suggest_key(key) {
                Some(k) => format!(" (did you mean '{k}'?)"),
                None => String::new(),
            };
            errors.push(at(format!(
                "unknown key '{key}'{hint}; known keys: {}",
                KNOWN_KEYS.join(", ")
            )));
            continue;
        }
        match coerce(key, item) {
            Ok(()) => {}
            Err(e) => errors.push(at(e)),
        }
        apply_one(&mut out, key, item);
    }

    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(out)
}

/// 1-based line number of `key = …` in the fragment.
///
/// Computed by scanning rather than from `Item::span()`: spans need
/// toml_edit's `parse` feature, and a model table is flat — one `key = value`
/// per line, no nesting — so a scan is exact here and costs no dependency. A
/// comment line mentioning the same word cannot be mistaken for the key,
/// because the text before the `=` must match the key exactly.
fn line_of_key(text: &str, key: &str) -> Option<usize> {
    text.lines().enumerate().find_map(|(i, line)| {
        let before_eq = line.split_once('=')?.0.trim();
        (before_eq == key).then_some(i + 1)
    })
}

/// Normalize a comment block into a decor prefix.
///
/// `toml_edit` keeps a key's leading comments in its prefix *including* the
/// newline that separates them from the key. Writing that back verbatim is
/// right; writing a hand-built `" {comment}"` loses the newline and welds the
/// comment to the key. The only fix needed is dropping a trailing blank line,
/// and adding the leading newline if the block had none (a comment at the very
/// top of a fragment).
pub fn comment_prefix_text(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }
    // Nothing is trimmed here. The trailing newline is the separator between
    // the comment and its key, and trimming it welds them together
    // (`# notreasoning = "max"`). Only the leading newline is ours to add.
    if raw.starts_with('\n') {
        raw.to_string()
    } else {
        format!("\n{raw}")
    }
}

/// The comment block directly above a key.
///
/// The decor lives on the **key**, not the value — `toml_edit` attaches a
/// key's leading comment to the key. That is why this takes the table and the
/// key name rather than the item.
fn comment_prefix(table: &dyn toml_edit::TableLike, key: &str) -> Option<String> {
    // `Key::leaf_decor` is where a key's own leading comment lives; the
    // value's decor holds only the whitespace after the `=`.
    let decor = table.key(key)?.leaf_decor();
    Some(decor.prefix()?.as_str()?.to_string())
}

/// Validate one key's value. Type errors are caught here rather than by serde
/// so the message can say what was found and what was wanted.
fn coerce(key: &str, item: &toml_edit::Item) -> Result<(), String> {
    let Some(value) = item.as_value() else {
        return Err(format!("'{key}' must be a single value, not a table"));
    };
    match key {
        "reasoning" => {
            let s = value.as_str().ok_or_else(|| {
                format!(
                    "'reasoning' must be a quoted string, found {}",
                    type_of(value)
                )
            })?;
            s.parse::<ReasoningLevel>().map(|_| ())
        }
        "supports_reasoning" => value.as_bool().map(|_| ()).ok_or_else(|| {
            format!(
                "'supports_reasoning' must be true or false, found {}",
                type_of(value)
            )
        }),
        "temperature" => in_range(key, value, 0.0, 2.0),
        "top_p" => in_range(key, value, 0.0, 1.0),
        "context_length" => u64_field(key, value).map(|_| ()),
        "coding_index" | "intelligence_index" | "agentic_index" => {
            f64_field(key, value).and_then(|v| in_0_100(key, v))
        }
        "prompt_cost" | "completion_cost" => f64_field(key, value).map(|_| ()),
        "description" => value.as_str().map(|_| ()).ok_or_else(|| {
            format!(
                "'description' must be a quoted string, found {}",
                type_of(value)
            )
        }),
        "reasoning_levels" => {
            let arr = value.as_array().ok_or_else(|| {
                format!(
                    "'reasoning_levels' must be an array of strings, found {}",
                    type_of(value)
                )
            })?;
            for (i, v) in arr.iter().enumerate() {
                if v.as_str().is_none() {
                    return Err(format!(
                        "'reasoning_levels[{i}]' must be a quoted string, found {}",
                        type_of(v)
                    ));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn in_range(key: &str, value: &toml_edit::Value, lo: f64, hi: f64) -> Result<(), String> {
    let n = f64_field(key, value)?;
    if !n.is_finite() || n < lo || n > hi {
        return Err(format!("'{key}' must be between {lo} and {hi} (found {n})"));
    }
    Ok(())
}

fn in_0_100(key: &str, n: f64) -> Result<(), String> {
    if !(0.0..=100.0).contains(&n) {
        return Err(format!("'{key}' must be 0-100 (found {n})"));
    }
    Ok(())
}

fn f64_field(key: &str, value: &toml_edit::Value) -> Result<f64, String> {
    value
        .as_float()
        .or_else(|| value.as_integer().map(|i| i as f64))
        .ok_or_else(|| format!("'{key}' must be a number, found {}", type_of(value)))
}

fn u64_field(key: &str, value: &toml_edit::Value) -> Result<u64, String> {
    value
        .as_integer()
        .and_then(|i| u64::try_from(i).ok())
        .ok_or_else(|| format!("'{key}' must be a whole number, found {}", type_of(value)))
}

/// How to describe a value in an error, in the user's own words.
fn type_of(v: &toml_edit::Value) -> &'static str {
    if v.is_str() {
        "a string"
    } else if v.is_integer() {
        "an integer"
    } else if v.is_float() {
        "a float"
    } else if v.is_bool() {
        "a boolean"
    } else if v.is_array() {
        "an array"
    } else if v.is_inline_table() {
        "a table"
    } else {
        "a datetime"
    }
}

/// Copy one validated key onto the record. Only reached for keys that passed
/// `coerce`, so the conversions here cannot fail.
fn apply_one(out: &mut ModelSettings, key: &str, item: &toml_edit::Item) {
    let Some(v) = item.as_value() else { return };
    let num = |v: &toml_edit::Value| v.as_float().or_else(|| v.as_integer().map(|i| i as f64));
    match key {
        "reasoning" => out.reasoning = v.as_str().and_then(|s| s.parse().ok()),
        "temperature" => out.temperature = num(v),
        "top_p" => out.top_p = num(v),
        "context_length" => out.context_length = v.as_integer().and_then(|i| u64::try_from(i).ok()),
        "prompt_cost" => out.prompt_cost = num(v),
        "completion_cost" => out.completion_cost = num(v),
        "coding_index" => out.coding_index = num(v).map(|n| n as f32),
        "intelligence_index" => out.intelligence_index = num(v).map(|n| n as f32),
        "agentic_index" => out.agentic_index = num(v).map(|n| n as f32),
        "description" => out.description = v.as_str().map(str::to_string),
        "supports_reasoning" => out.supports_reasoning = v.as_bool(),
        "reasoning_levels" => {
            out.reasoning_levels = v.as_array().map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::to_string)
                    .collect()
            })
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Rendering the fragment the editor shows
// ---------------------------------------------------------------------------

/// Render a record as a commented TOML fragment for `$EDITOR`.
///
/// Three jobs, in one pass over the text:
///   * the current values, as real lines;
///   * the user's own comments, carried through from the file;
///   * everything unset, as *commented* suggestions, so the file itself
///     explains what could be set without implying anything is.
///
/// The suggestions come from the catalog where we have it, so the fragment
/// shows real values (`# context_length = 1048576`) rather than placeholders.
pub fn render_fragment(model_id: &str, current: &ModelSettings, hints: &ModelSettings) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Settings for {model_id}\n\
         #\n\
         # Delete a line to clear it. Lines that are not set are shown commented\n\
         # out with what the catalog currently reports, so you can uncomment one\n\
         # to take it.\n\
         #\n\
         # reasoning / temperature / top_p are defaults applied when this model is\n\
         # first activated in a session. A `/model <id> save` copies your live\n\
         # values here.\n\n"
    ));

    for key in ModelSettings::KEYS {
        let present = current.value_of(key);
        if let Some(comment) = current.comments.get(*key) {
            for line in comment.lines() {
                out.push_str(&format!("#{line}\n"));
            }
        }
        let hint = ModelSettings::key_hint(key);
        if !hint.is_empty() {
            out.push_str(&format!("# {hint}\n"));
        }
        match present {
            Some(v) => out.push_str(&format!("{key} = {v}\n")),
            None => {
                if let Some(suggested) = hints.value_of(key) {
                    out.push_str(&format!("# {key} = {suggested}\n"));
                } else {
                    out.push_str(&format!("# {key} =\n"));
                }
            }
        }
        out.push('\n');
    }
    out
}

impl ModelSettings {
    /// A key's current value rendered as TOML source, or `None` when unset.
    ///
    /// Rendered directly rather than through a serialiser: every field here is
    /// a scalar or a list of scalars, so the shapes are known, and the output
    /// is what the fragment shows *and* what the file stores — one renderer,
    /// so they cannot disagree.
    fn value_of(&self, key: &str) -> Option<String> {
        Some(match key {
            "reasoning" => quote(self.reasoning?.as_str()),
            "temperature" => num(self.temperature?),
            "top_p" => num(self.top_p?),
            "context_length" => self.context_length?.to_string(),
            "prompt_cost" => num(self.prompt_cost?),
            "completion_cost" => num(self.completion_cost?),
            "coding_index" => num(self.coding_index? as f64),
            "intelligence_index" => num(self.intelligence_index? as f64),
            "agentic_index" => num(self.agentic_index? as f64),
            "description" => quote(self.description.as_ref()?),
            "supports_reasoning" => self.supports_reasoning?.to_string(),
            "reasoning_levels" => {
                let items: Vec<String> = self
                    .reasoning_levels
                    .as_ref()?
                    .iter()
                    .map(|s| quote(s))
                    .collect();
                format!("[{}]", items.join(", "))
            }
            _ => return None,
        })
    }
}

/// A TOML float that reads back as the same value: no trailing-zero noise,
/// and never a bare `1` for a float the user typed as `1.0` — the value is
/// what matters, and `1` and `1.0` parse to the same number.
fn num(v: f64) -> String {
    let s = format!("{v}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("nan") {
        s
    } else {
        format!("{s}.0")
    }
}

/// A TOML basic string, with the escapes that actually matter for prose.
fn quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

// ---------------------------------------------------------------------------
// Reading and writing the tables inside settings.toml
// ---------------------------------------------------------------------------

/// Read every `[models.*]` table out of a parsed document, capturing each
/// key's leading comment so it can be written back.
///
/// Unknown keys inside a model table are reported, not skipped: a typo should
/// be loud at load time, the same way the editor refuses one at save time.
pub fn read_from_doc(
    doc: &toml_edit::DocumentMut,
) -> Result<HashMap<String, ModelSettings>, String> {
    let mut out = HashMap::new();
    let Some(models) = doc.get(MODELS_TABLE).and_then(|m| m.as_table_like()) else {
        return Ok(out);
    };
    let mut errors = Vec::new();
    for (id, item) in models.iter() {
        let Some(table) = item.as_table_like() else {
            errors.push(format!(
                "[{MODELS_TABLE}.{id:?}] must be a table of settings"
            ));
            continue;
        };
        // Rebuild the table as a standalone fragment and reuse the one
        // validator, so the file and the editor cannot disagree about what is
        // acceptable.
        let mut fragment = toml_edit::DocumentMut::new();
        for (key, value) in table.iter() {
            fragment.insert(key, value.clone());
        }
        match parse_fragment(&fragment.to_string()) {
            Ok(mut settings) => {
                // The rebuild above drops each key's decor, so the comments are
                // read from the source table here rather than through the
                // fragment. They are the user's own words about their own
                // file; losing them on a plain load would be its own bug.
                for (key, _) in table.iter() {
                    if let Some(prefix) = comment_prefix(table, key) {
                        if !prefix.trim().is_empty() {
                            settings.comments.insert(key.to_string(), prefix);
                        }
                    }
                }
                out.insert(id.to_string(), settings);
            }
            Err(msg) => errors.push(format!("[{MODELS_TABLE}.{id:?}]: {msg}")),
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n"));
    }
    Ok(out)
}

/// Replace one model's table in the document, preserving comments and leaving
/// every other table alone.
///
/// An empty record removes the table: a model with nothing set should leave no
/// trace, so "not configured" stays distinguishable from "configured to
/// nothing".
pub fn write_to_doc(
    doc: &mut toml_edit::DocumentMut,
    model_id: &str,
    settings: &ModelSettings,
) -> Result<(), String> {
    let models = doc
        .entry(MODELS_TABLE)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    if let Some(parent) = models.as_table_like() {
        // `[models]` holds only sub-tables, so it must not render a header of
        // its own — otherwise every save leaves a bare, empty `[models]` above
        // the real ones. Implicit means "exists, but print nothing".
        let _ = parent;
    }
    if !models.is_table_like() {
        return Err(format!(
            "'{MODELS_TABLE}' exists but is not a table — cannot store per-model settings"
        ));
    }
    if let Some(t) = models.as_table_mut() {
        t.set_implicit(true);
    }
    let models = models.as_table_like_mut().expect("checked above");

    if settings.is_empty() {
        models.remove(model_id);
        // Drop the now-empty parent so the file does not keep a bare
        // `[models]` header around.
        if models.is_empty() {
            doc.remove(MODELS_TABLE);
        }
        return Ok(());
    }

    let existing = models
        .get(model_id)
        .and_then(|i| i.as_table_like())
        .map(|t| {
            let mut copy = toml_edit::Table::new();
            for (k, v) in t.iter() {
                copy.insert(k, v.clone());
            }
            copy
        });

    let mut table = toml_edit::Table::new();
    for key in ModelSettings::KEYS {
        if let Some(v) = settings.value_of(key) {
            // Parse the rendered *value* on its own, so a value we cannot
            // parse is a loud error rather than a key that silently vanishes
            // from the file — which is what a bare `if let Ok(..)` here would
            // do, quietly.
            let parsed: toml_edit::Value = v.parse().map_err(|e| {
                format!("internal: {key} rendered as '{v}' but is not valid TOML: {e}")
            })?;
            let item = toml_edit::Item::Value(parsed);
            // Carry the user's comment through to the new value. A comment
            // already on the key in the file wins: it is the one the user last
            // wrote, and this record's copy may be older.
            let carried = existing
                .as_ref()
                .and_then(|t| t.key(key).and_then(|k| k.leaf_decor().prefix().cloned()))
                .or_else(|| settings.comments.get(*key).map(|c| c.clone().into()));
            table.insert(key, item);
            // The comment goes on the KEY. Putting it on the value instead
            // writes `reasoning = # note"max"` — the note lands inside the
            // value and the line stops being valid TOML.
            if let (Some(prefix), Some(mut new_key)) = (carried, table.key_mut(key)) {
                let mut decor = new_key.leaf_decor().clone();
                decor.set_prefix(comment_prefix_text(prefix.as_str().unwrap_or_default()));
                *new_key.leaf_decor_mut() = decor;
            }
        }
    }
    if table.is_empty() {
        models.remove(model_id);
    } else {
        models.insert(model_id, toml_edit::Item::Table(table));
    }
    if models.is_empty() {
        doc.remove(MODELS_TABLE);
    }
    Ok(())
}

/// The catalog facts for a model, in the shape the fragment renderer wants:
/// what we know today, offered as commented suggestions.
pub fn hints_for(provider: &str, model: &str) -> ModelSettings {
    let Some(e) = crate::model_catalog::lookup(provider, model) else {
        return ModelSettings::default();
    };
    ModelSettings {
        context_length: e.context_length,
        prompt_cost: e.prompt_cost,
        completion_cost: e.completion_cost,
        coding_index: e.coding_index,
        intelligence_index: e.intelligence_index,
        agentic_index: e.agentic_index,
        description: e.description.clone(),
        supports_reasoning: Some(e.reasoning || !e.reasoning_levels.is_empty()),
        reasoning_levels: if e.reasoning_levels.is_empty() {
            None
        } else {
            Some(e.reasoning_levels.clone())
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> toml_edit::DocumentMut {
        text.parse().expect("valid TOML")
    }

    // ---- round-trip ----

    #[test]
    fn a_record_round_trips_through_the_file() {
        let id = "openrouter:tencent/hy3:free";
        let mut d = doc("default_model = \"openrouter:tencent/hy3:free\"\n");
        let original = ModelSettings {
            reasoning: Some(ReasoningLevel::High),
            temperature: Some(0.8),
            top_p: Some(0.95),
            context_length: Some(1_048_576),
            coding_index: Some(61.5),
            description: Some("Tencent HY 3, \"fast\"".into()),
            reasoning_levels: Some(vec!["low".into(), "high".into(), "max".into()]),
            supports_reasoning: Some(true),
            ..Default::default()
        };
        write_to_doc(&mut d, id, &original).unwrap();

        let read = read_from_doc(&d).unwrap();
        let back = read.get(id).expect("record must come back");
        // `comments` is file-derived, not part of the record's identity.
        assert_eq!(back.params(), original.params());
        assert_eq!(back.context_length, original.context_length);
        assert_eq!(back.coding_index, original.coding_index);
        assert_eq!(back.description, original.description);
        assert_eq!(back.reasoning_levels, original.reasoning_levels);
        assert_eq!(back.supports_reasoning, original.supports_reasoning);
    }

    /// An empty record leaves no table: "not configured" and "configured to
    /// nothing" are the same state.
    #[test]
    fn an_empty_record_removes_its_table() {
        let id = "p:m";
        let mut d = doc("[recipes.x]\nprompt = \"hi\"\n");
        write_to_doc(&mut d, id, &ModelSettings::default()).unwrap();
        assert!(!d.to_string().contains("models"), "{}", d);
        assert!(d.to_string().contains("recipes.x"), "{}", d);
    }

    /// Writing one model's table must not disturb another, or anything else
    /// in the file.
    #[test]
    fn writing_one_model_leaves_the_others_alone() {
        let mut d = doc(
            "[models.\"a:one\"]\nreasoning = \"low\"\n\n[models.\"b:two\"]\ntemperature = 0.5\n\n[recipes.x]\nprompt = \"hi\"\n",
        );
        write_to_doc(
            &mut d,
            "a:one",
            &ModelSettings {
                reasoning: Some(ReasoningLevel::Max),
                ..Default::default()
            },
        )
        .unwrap();
        let out = d.to_string();
        assert!(out.contains("reasoning = \"max\""), "{out}");
        assert!(out.contains("temperature = 0.5"), "b:two kept: {out}");
        assert!(out.contains("prompt = \"hi\""), "recipes kept: {out}");
    }

    // ---- comments ----

    /// A comment the user wrote survives a round-trip. This is the whole
    /// reason `comments` exists rather than regenerating the table.
    #[test]
    fn a_users_comment_survives_a_round_trip() {
        let id = "p:m";
        let source = format!(
            "[models.\"{id}\"]\n# only for the nightly batch, do not lower\nreasoning = \"low\"\n"
        );
        let d = doc(&source);
        let read = read_from_doc(&d).unwrap();
        let record = read.get(id).unwrap();
        assert_eq!(record.reasoning, Some(ReasoningLevel::Low));
        assert!(
            record
                .comments
                .get("reasoning")
                .is_some_and(|c| c.contains("nightly batch")),
            "comment not captured: {:?}",
            record.comments
        );

        // Writing it back must not lose it.
        let mut out = doc(&source);
        write_to_doc(&mut out, id, record).unwrap();
        assert!(
            out.to_string().contains("nightly batch"),
            "comment dropped on write: {}",
            out
        );
    }

    /// A comment on a key the record no longer sets is dropped with the key —
    /// keeping it would attach a note to nothing.
    #[test]
    fn a_comment_dies_with_its_key() {
        let id = "p:m";
        let mut d = doc(&format!(
            "[models.\"{id}\"]\n# was needed for X\ntemperature = 0.5\n"
        ));
        write_to_doc(&mut d, id, &ModelSettings::default()).unwrap();
        assert!(!d.to_string().contains("was needed"), "{}", d);
    }

    // ---- validation: every error, not just the first ----

    #[test]
    fn a_typo_is_reported_with_a_suggestion() {
        let err = parse_fragment("temprature = 0.8\n").unwrap_err();
        assert!(err.contains("unknown key 'temprature'"), "{err}");
        assert!(err.contains("did you mean 'temperature'"), "{err}");
    }

    #[test]
    fn three_bad_keys_produce_three_errors() {
        let err = parse_fragment("temprature = 0.8\ntopp = 0.5\ntemperature = 9.0\n").unwrap_err();
        assert!(err.contains("temprature"), "{err}");
        assert!(err.contains("topp"), "{err}");
        assert!(err.contains("between 0 and 2"), "{err}");
        // Three problems, three lines — not just the first.
        assert_eq!(err.lines().count(), 3, "{err}");
    }

    #[test]
    fn errors_carry_line_numbers() {
        let err = parse_fragment("reasoning = \"high\"\ntemperature = 9.0\n").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
    }

    #[test]
    fn type_errors_say_what_was_found() {
        let err = parse_fragment("temperature = \"hot\"\n").unwrap_err();
        assert!(err.contains("must be a number"), "{err}");
        assert!(err.contains("a string"), "what was found: {err}");

        let err = parse_fragment("supports_reasoning = \"yes\"\n").unwrap_err();
        assert!(err.contains("true or false"), "{err}");

        let err = parse_fragment("reasoning_levels = \"low\"\n").unwrap_err();
        assert!(err.contains("array"), "{err}");

        let err = parse_fragment("reasoning = 3\n").unwrap_err();
        assert!(err.contains("quoted string"), "{err}");
    }

    #[test]
    fn ranges_are_checked_per_key() {
        assert!(parse_fragment("temperature = 2.0\n").is_ok());
        assert!(parse_fragment("temperature = 2.1\n").is_err());
        assert!(parse_fragment("top_p = 1.0\n").is_ok());
        assert!(parse_fragment("top_p = 1.1\n").is_err());
        assert!(parse_fragment("coding_index = 101\n").is_err());
        assert!(parse_fragment("coding_index = 100\n").is_ok());
        assert!(parse_fragment("context_length = -1\n").is_err());
    }

    #[test]
    fn an_unknown_level_names_the_valid_ones() {
        let err = parse_fragment("reasoning = \"extreme\"\n").unwrap_err();
        assert!(err.contains("unknown reasoning level"), "{err}");
        assert!(err.contains("off | low | med | high | max"), "{err}");
    }

    #[test]
    fn unparsable_toml_is_reported_as_such() {
        let err = parse_fragment("temperature = = 1\n").unwrap_err();
        assert!(err.contains("could not parse as TOML"), "{err}");
    }

    // ---- the fragment ----

    #[test]
    fn the_fragment_shows_values_and_offers_the_rest() {
        let current = ModelSettings {
            reasoning: Some(ReasoningLevel::High),
            ..Default::default()
        };
        let hints = ModelSettings {
            context_length: Some(1_048_576),
            prompt_cost: Some(0.43),
            ..Default::default()
        };
        let text = render_fragment("p:m", &current, &hints);
        // The set value is a real line.
        assert!(text.contains("\nreasoning = \"high\"\n"), "{text}");
        // The unset ones are commented suggestions carrying real values.
        assert!(text.contains("# context_length = 1048576"), "{text}");
        assert!(text.contains("# prompt_cost = 0.43"), "{text}");
        assert!(text.contains("# top_p ="), "{text}");
        // Every key is mentioned, so the fragment documents the whole schema.
        for key in ModelSettings::KEYS {
            assert!(text.contains(key), "missing {key}: {text}");
        }
    }

    /// The fragment must itself be valid, or the user's first save would fail
    /// on something we generated.
    #[test]
    fn the_fragment_is_itself_a_valid_record() {
        let current = ModelSettings {
            reasoning: Some(ReasoningLevel::Med),
            temperature: Some(0.25),
            context_length: Some(200_000),
            description: Some("a \"quoted\" description\nwith a newline".into()),
            reasoning_levels: Some(vec!["low".into(), "max".into()]),
            supports_reasoning: Some(false),
            ..Default::default()
        };
        let hints = ModelSettings {
            prompt_cost: Some(3.0),
            ..Default::default()
        };
        let text = render_fragment("p:m", &current, &hints);
        let parsed = parse_fragment(&text).unwrap_or_else(|e| panic!("{e}\n---\n{text}"));
        assert_eq!(parsed.params(), current.params());
        assert_eq!(parsed.context_length, current.context_length);
        assert_eq!(parsed.description, current.description);
        assert_eq!(parsed.reasoning_levels, current.reasoning_levels);
        assert_eq!(parsed.supports_reasoning, Some(false));
    }

    /// Deleting a line in the editor is how a value is cleared.
    #[test]
    fn deleting_a_line_clears_the_value() {
        let parsed = parse_fragment("reasoning = \"high\"\n# top_p =\n").unwrap();
        assert_eq!(parsed.reasoning, Some(ReasoningLevel::High));
        assert_eq!(parsed.top_p, None);
    }

    // ---- metadata overlay ----

    #[test]
    fn metadata_overrides_apply_where_set_and_only_there() {
        let mut entry = crate::model_catalog::ModelEntry {
            context_length: Some(128_000),
            prompt_cost: Some(3.0),
            ..Default::default()
        };
        let settings = ModelSettings {
            context_length: Some(1_048_576),
            coding_index: Some(55.0),
            ..Default::default()
        };
        settings.apply_to_entry(&mut entry);
        assert_eq!(entry.context_length, Some(1_048_576), "override wins");
        assert_eq!(entry.prompt_cost, Some(3.0), "unset keeps the catalog");
        assert_eq!(entry.coding_index, Some(55.0), "set but unknown before");
    }

    // ---- suggestions ----

    #[test]
    fn suggestions_are_only_offered_when_close() {
        assert_eq!(suggest_key("temprature"), Some("temperature"));
        assert_eq!(suggest_key("topp"), Some("top_p"));
        assert_eq!(suggest_key("reasonn"), Some("reasoning"));
        assert_eq!(suggest_key("descripion"), Some("description"));
        // Nothing close enough: better to say nothing than to guess wrong.
        assert_eq!(suggest_key("banana"), None);
    }
}
