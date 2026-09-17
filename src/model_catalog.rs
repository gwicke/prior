//! Model catalog: live metadata + provider availability + extrapolation.
//!
//! **Source of truth:** OpenRouter's public, unauthenticated
//! `GET /api/v1/models`, which aggregates ~300 providers with
//! `context_length` + per-token `pricing` + `supported_parameters` (incl.
//! `reasoning`). `rig` 0.28 exposes no provider model-listing API (only
//! `mira`), so we fetch at runtime.
//!
//! **Direct-provider extrapolation:** each OpenRouter id is namespaced
//! `{provider}/{model}`. For every provider that is *available* (env key
//! present), we derive a direct entry `{provider}/{model}` whose price comes
//! from the curated `DIRECT_PRICING` table (direct access differs from
//! OpenRouter's markup); otherwise we fall back to OpenRouter's price tagged
//! `OpenRouterEstimate`. The OpenRouter-routed entry is kept too, so an
//! unconfigured model is still selectable via OpenRouter.
//!
//! **Liveness:** for providers exposing `/models` (DeepSeek, OpenAI), we
//! prune/extend the direct list against the live id set to kill staleness
//! (e.g. surface `deepseek-v4-flash`/`v4-pro`). **Cache:** 5-minute TTL.

use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostSource {
    /// Price taken from the curated `DIRECT_PRICING` table (direct access).
    Direct,
    /// Price taken from OpenRouter's markup (estimate for direct access).
    OpenRouterEstimate,
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// Provider namespace, e.g. `deepseek` or `openrouter`.
    pub provider: String,
    /// Full id, e.g. `deepseek/deepseek-v4-flash` or `openrouter/deepseek/deepseek-v4-flash`.
    pub id: String,
    /// Human display string, e.g. `deepseek:deepseek-v4-flash`.
    pub display: String,
    pub context_length: Option<u64>,
    /// Cost per million prompt tokens (USD).
    pub prompt_cost: Option<f64>,
    /// Cost per million completion tokens (USD).
    pub completion_cost: Option<f64>,
    /// Whether the model exposes thinking/reasoning.
    pub reasoning: bool,
    pub cost_source: CostSource,
    /// Model creation timestamp (unix seconds)
    pub created: Option<u64>,
    /// Artificial Analysis coding index (0-100)
    pub coding_index: Option<f32>,
    /// Artificial Analysis intelligence index (0-100)
    pub intelligence_index: Option<f32>,
    /// Artificial Analysis agentic index (0-100)
    pub agentic_index: Option<f32>,
}

impl ModelEntry {
    /// OpenRouter-routed entries carry `provider == "openrouter"` and an `id`
    /// that is the full (possibly namespaced) model path, e.g.
    /// `tencent/hy3:free`; the qualified form is `openrouter:tencent/hy3:free`.
    /// Direct entries carry the provider both in `provider` and as the `id`
    /// prefix, e.g. `tencent/hy3:free` → `tencent:hy3:free` (the namespace is
    /// stripped).
    pub fn qualified_parts(&self) -> (String, String) {
        self.id
            .split_once(':')
            .map(|(p, m)| (p.to_string(), m.to_string()))
            .unwrap_or_else(|| (self.provider.clone(), self.id.clone()))
    }

    /// The `provider:model` string (see [`ModelEntry::qualified_parts`]).
    pub fn qualified(&self) -> String {
        let (p, m) = self.qualified_parts();
        format!("{}:{}", p, m)
    }
}

/// Curated fallback: **direct-access** pricing per million tokens.
/// Used when offline or for providers OpenRouter doesn't surface.
const DIRECT_PRICING: &[(&str, &str, u64, f64, f64, bool)] = &[
    ("deepseek", "deepseek-chat", 128_000, 0.0, 0.0, false),
    ("deepseek", "deepseek-reasoner", 64_000, 0.55, 2.19, true),
    ("openai", "gpt-4o", 128_000, 2.5, 10.0, false),
    ("openai", "gpt-4o-mini", 128_000, 0.15, 0.60, false),
    ("anthropic", "claude-3-5-sonnet-20241022", 200_000, 3.0, 15.0, false),
    ("anthropic", "claude-3-7-sonnet", 200_000, 3.0, 15.0, true),
    ("gemini", "gemini-1.5-pro", 1_000_000, 1.25, 5.0, false),
    ("groq", "llama3-70b-8192", 8_192, 0.59, 0.79, false),
    ("mistral", "mistral-large-latest", 128_000, 2.0, 6.0, false),
    ("ollama", "llama3", 8_192, 0.0, 0.0, false),
    ("openrouter", "meta-llama/llama-3-70b-instruct", 8_192, 0.0, 0.0, false),
    ("xai", "grok-1", 8_192, 5.0, 15.0, false),
    ("cohere", "command-r-plus", 128_000, 2.5, 10.0, false),
    ("chatgpt", "gpt-5", 128_000, 0.0, 0.0, false),
    ("copilot", "gpt-5-codex", 128_000, 0.0, 0.0, false),
    ("moonshot", "moonshot-v1-8k", 8_192, 0.0, 0.0, false),
    ("together", "meta-llama/Llama-3-70b-chat-hf", 8_192, 0.0, 0.0, false),
    ("perplexity", "llama-3-sonar-large-32k-online", 32_000, 1.0, 1.0, false),
    ("hyperbolic", "meta-llama/Llama-3-70b-instruct", 8_192, 0.0, 0.0, false),
    ("huggingface", "meta-llama/Llama-3-70b-chat-hf", 8_192, 0.0, 0.0, false),
    ("mira", "mira-v1", 8_192, 0.0, 0.0, false),
    ("doubleword", "Qwen/Qwen3.5-4B", 128_000, 0.0, 0.0, false),
    ("llamafile", "LLaMA_CPP", 128_000, 0.0, 0.0, false),
    ("minimax", "MiniMax-M2.7", 200_000, 0.0, 0.0, false),
    ("xiaomimimo", "mimo-v2.5-pro", 200_000, 0.0, 0.0, false),
    ("zai", "glm-4.6", 128_000, 0.0, 0.0, false),
];

/// All known provider names, in one place.  Kept in sync with
/// [`required_env_vars`] match arms (every arm name must appear here).
const KNOWN_PROVIDERS: &[&str] = &[
    "anthropic", "openai", "azure", "cohere", "chatgpt", "copilot",
    "deepseek", "doubleword", "gemini", "google", "groq", "huggingface",
    "hyperbolic", "llamafile", "mira", "minimax", "mistral", "moonshot",
    "ollama", "openrouter", "perplexity", "together", "xai", "xiaomimimo", "zai",
];

/// Does picocode know about this provider name?  Unknown providers cannot
/// have their API key verified, so OpenRouter-extrapolated entries must not
/// be created for them (they would appear as "live" in the overlay even
/// though no API key can be configured).
pub fn is_known_provider(provider: &str) -> bool {
    KNOWN_PROVIDERS
        .iter()
        .any(|k| k.eq_ignore_ascii_case(provider))
}

fn required_env_vars(provider: &str) -> &'static [&'static str] {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "azure" => &["AZURE_OPENAI_API_KEY", "AZURE_OPENAI_ENDPOINT"],
        "cohere" => &["COHERE_API_KEY"],
        "chatgpt" => &["CHATGPT_ACCESS_TOKEN"],
        "copilot" => &["GITHUB_COPILOT_API_KEY", "COPILOT_GITHUB_ACCESS_TOKEN"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "doubleword" => &["DOUBLEWORD_API_KEY"],
        "gemini" | "google" => &["GOOGLE_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "hyperbolic" => &["HYPERBOLIC_API_KEY"],
        "llamafile" => &["LLAMAFILE_API_BASE_URL"],
        "mira" => &["MIRA_API_KEY"],
        "minimax" => &["MINIMAX_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "moonshot" => &["MOONSHOT_API_KEY"],
        "ollama" => &[],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "perplexity" => &["PERPLEXITY_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "xiaomimimo" => &["XIAOMI_MIMO_API_KEY"],
        "zai" => &["ZAI_API_KEY"],
        _ => &[],
    }
}

/// Cheap env-var scan: are the provider's required key(s) present?
/// This is *availability* (the key exists), not a liveness check.
pub fn provider_available(provider: &str) -> bool {
    let vars = required_env_vars(provider);
    if vars.is_empty() {
        // ollama (localhost default) / unknown providers: treat as available.
        return true;
    }
    vars.iter().all(|v| std::env::var(v).is_ok())
}

/// Map an OpenRouter `/api/v1/models` payload to catalog entries.
/// Prices are converted from per-token (OpenRouter) to per-million (USD).
pub fn parse_openrouter(value: &Value) -> Vec<ModelEntry> {
    let mut out = Vec::new();
    let Some(arr) = value.get("data").and_then(|v| v.as_array()) else {
        return out;
    };
    for m in arr {
        let Some(id) = m.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let raw_id = id.to_string();
        let id = format!("openrouter:{}", raw_id);
        let context_length = m.get("context_length").and_then(|v| v.as_u64());
        let (prompt_cost, completion_cost) = m
            .get("pricing")
            .map(|p| {
                let pp = p
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|x| x * 1_000_000.0);
                let cc = p
                    .get("completion")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|x| x * 1_000_000.0);
                (pp, cc)
            })
            .unwrap_or((None, None));
        let reasoning = m
            .get("supported_parameters")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().any(|x| x.as_str() == Some("reasoning")))
            .unwrap_or(false);
        let created = m.get("created").and_then(|v| v.as_u64());
        let coding_index = m
            .get("benchmarks")
            .and_then(|b| b.get("artificial_analysis"))
            .and_then(|aa| aa.get("coding_index"))
            .and_then(|v| v.as_f64())
            .map(|v| v as f32);
        let intelligence_index = m
            .get("benchmarks")
            .and_then(|b| b.get("artificial_analysis"))
            .and_then(|aa| aa.get("intelligence_index"))
            .and_then(|v| v.as_f64())
            .map(|v| v as f32);
        let agentic_index = m
            .get("benchmarks")
            .and_then(|b| b.get("artificial_analysis"))
            .and_then(|aa| aa.get("agentic_index"))
            .and_then(|v| v.as_f64())
            .map(|v| v as f32);
        out.push(ModelEntry {
            provider: "openrouter".to_string(),
            id: id.clone(),
            display: id.clone(),
            context_length,
            prompt_cost,
            completion_cost,
            reasoning,
            cost_source: CostSource::OpenRouterEstimate,
            created,
            coding_index,
            intelligence_index,
            agentic_index,
        });
    }
    out
}

/// Cell text for the `Ctx` column of an aligned model table: the context
/// length, or `—` when unknown.
pub fn ctx_cell(e: &ModelEntry) -> String {
    match e.context_length {
        Some(c) => c.to_string(),
        None => "—".to_string(),
    }
}

/// Cell text for the `Cost` column of an aligned model table: `$prompt/$completion`
/// per million tokens. When the price is an OpenRouter estimate it is prefixed
/// with `~` (the number is approximate). Returns `—` when cost is unknown.
/// Reasoning is reported by [`reason_cell`].
pub fn cost_cell(e: &ModelEntry) -> String {
    match (e.prompt_cost, e.completion_cost) {
        (Some(p), Some(c)) => {
            let inner = format!("${:.2}/${:.2}", p, c);
            if e.cost_source == CostSource::OpenRouterEstimate {
                format!("~{}", inner)
            } else {
                inner
            }
        }
        _ => "—".to_string(),
    }
}

/// Cell text for the `Reason` column of an aligned model table: `✓` when the
/// model exposes reasoning/thinking, `✗` otherwise.
pub fn reason_cell(e: &ModelEntry) -> String {
    if e.reasoning { "✓".to_string() } else { "✗".to_string() }
}

/// Cell text for the `Coding` column showing the Artificial Analysis coding index.
pub fn coding_index_cell(e: &ModelEntry) -> String {
    e.coding_index.map(|v| format!("{:.1}", v)).unwrap_or_else(|| "—".to_string())
}

/// Human-readable one-line description for the `/model` picker.
/// When the local `DIRECT_PRICING` table has no explicit cost, the OpenRouter
/// (per-token) price is shown instead, tagged `(est)` — it's in the ballpark.
pub fn format_option(e: &ModelEntry) -> String {
    let cost = match (e.prompt_cost, e.completion_cost) {
        (Some(p), Some(c)) => format!("  ${:.2}/{:.2} per M", p, c),
        _ => String::new(),
    };
    let ctx = e
        .context_length
        .map(|c| format!("  ctx {}", c))
        .unwrap_or_default();
    let reason = if e.reasoning { "  [reasoning]" } else { "" };
    let tag = match e.cost_source {
        CostSource::OpenRouterEstimate => "  (est)",
        CostSource::Direct => "",
    };
    format!("{}{}{}{}{}", e.display, ctx, cost, tag, reason)
}

/// Fetch OpenRouter's live model list (unauthenticated). Network calls are
/// time-boxed so a slow/unreachable endpoint can't hang the REPL.
pub async fn fetch_openrouter() -> Result<Vec<ModelEntry>, String> {
    let url = "https://openrouter.ai/api/v1/models?sort=most-popular";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("OpenRouter returned {}", resp.status()));
    }
    let value: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(parse_openrouter(&value))
}

/// Derive direct-provider entries from OpenRouter records. Keeps the
/// OpenRouter-routed entry and, for available providers, adds a direct entry
/// priced from `DIRECT_PRICING` (or OpenRouter's value, tagged as estimate).
fn extrapolate(or_entries: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let mut out = Vec::new();
    let mut debug_count = 0usize;
    for e in or_entries {
        out.push(e.clone());
        // e.id is "openrouter:deepseek/deepseek-v4-flash" — strip the
        // "openrouter:" prefix to get the original OpenRouter API id.
        if let Some(or_id) = e.id.strip_prefix("openrouter:") {
            if let Some((ns, model)) = or_id.split_once('/') {
                if is_known_provider(ns) && provider_available(ns) {
                    let (pc, cc, source) = direct_price(ns, model, e.prompt_cost, e.completion_cost);
                    out.push(ModelEntry {
                        provider: ns.to_string(),
                        id: format!("{}:{}", ns, model),
                        display: format!("{}:{}", ns, model),
                        context_length: e.context_length,
                        prompt_cost: pc,
                        completion_cost: cc,
                        reasoning: e.reasoning,
                        cost_source: source,
                        created: e.created,
                        coding_index: e.coding_index,
                        intelligence_index: e.intelligence_index,
                        agentic_index: e.agentic_index,
                    });
                    debug_count += 1;
                }
            }
        }
    }
    if DEBUG_PROVIDERS.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("[debug-providers] extrapolate: {} OpenRouter entries → {} direct entries added", out.len() - debug_count, debug_count);
        for e in &out {
            if e.provider != "openrouter" {
                eprintln!("[debug-providers]   direct: {} (cost_source={:?})", e.display, e.cost_source);
            }
        }
    }
    out
}


/// Parameters for filtering/sorting the model list (client-side only).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelFilterParams {
    /// Sort order: "most-popular", "created", "coding", "intelligence", "agentic", "price_input", "price_output", "price", "coding_price", "intelligence_price", "agentic_price"
    pub sort: Option<String>,
    /// Maximum prompt (input) price per million tokens
    pub price_input: Option<f64>,
    /// Maximum output (completion) price per million tokens
    pub price_output: Option<f64>,
    /// Maximum combined price (input + output) per million tokens
    pub price: Option<f64>,
    /// Minimum coding index (0-100)
    pub min_coding_index: Option<f32>,
    /// Minimum intelligence index (0-100)
    pub min_intelligence_index: Option<f32>,
    /// Minimum agentic index (0-100)
    pub min_agentic_index: Option<f32>,
    /// Maximum model age in days
    pub max_age_days: Option<u32>,
    /// Minimum context length
    pub min_context: Option<u32>,
}

impl ModelFilterParams {
    /// Default parameters: no filter (show all models)
    pub fn default_for_coding() -> Self {
        Self::default()
    }
}


/// One dotted modifier token usable in a filter string (e.g. `.sort:coding`,
/// `.price:0.5`).  This is the **single source of truth** for both the parser
/// (`apply_modifier`) and the in-prompt auto-complete overlay: adding a new
/// `.*` option means adding one row here (plus a field and an apply arm when it
/// targets a new `ModelFilter` field).
#[derive(Debug, Clone, Copy)]
pub struct ModifierSpec {
    /// The token without the leading dot or colon — e.g. `"sort"`.
    pub token: &'static str,
    /// One-line description shown as the row's detail column.
    pub desc: &'static str,
    /// The enumerated value set, when the value is one of a fixed list
    /// (`None` means the value is a free-form number the user types).
    pub choices: Option<&'static [&'static str]>,
}

/// The dotted modifiers understood by the `/model` filter.  Shared between the
/// parser and the auto-complete overlay so they can never drift apart.
pub const MODEL_MODIFIERS: &[ModifierSpec] = &[
    ModifierSpec {
        token: "sort",
        desc: "sort order",
        choices: Some(&[
            "most-popular", "created", "coding", "intelligence", "agentic",
            "price_input", "price_output", "price", "coding_price",
            "intelligence_price", "agentic_price",
        ]),
    },
    ModifierSpec { token: "price_input", desc: "max prompt $/M", choices: None },
    ModifierSpec { token: "price_output", desc: "max output $/M", choices: None },
    ModifierSpec { token: "price", desc: "max combined $/M", choices: None },
    ModifierSpec { token: "coding", desc: "min coding index", choices: None },
    ModifierSpec { token: "intelligence", desc: "min intel index", choices: None },
    ModifierSpec { token: "agentic", desc: "min agentic index", choices: None },
    ModifierSpec { token: "max_age_days", desc: "max model age (days)", choices: None },
    ModifierSpec { token: "context", desc: "min context length", choices: None },
];


/// Apply client-side filters and sorting to the cached catalog.
/// This is the main entry point for the `/model` overlay.
pub fn get_filtered_catalog(params: &ModelFilterParams) -> Vec<ModelEntry> {
    let mut entries = catalog_cached();
    
    // Apply filters
    if let Some(max_price) = params.price_input {
        entries.retain(|e| e.prompt_cost.map_or(false, |p| p <= max_price));
    }
    if let Some(max_output_price) = params.price_output {
        entries.retain(|e| e.completion_cost.map_or(false, |c| c <= max_output_price));
    }
    if let Some(min_coding) = params.min_coding_index {
        entries.retain(|e| e.coding_index.map_or(false, |v| v >= min_coding));
    }
    if let Some(min_intel) = params.min_intelligence_index {
        entries.retain(|e| e.intelligence_index.map_or(false, |v| v >= min_intel));
    }
    if let Some(min_agentic) = params.min_agentic_index {
        entries.retain(|e| e.agentic_index.map_or(false, |v| v >= min_agentic));
    }
    if let Some(max_age) = params.max_age_days {
        let cutoff = chrono::Utc::now().timestamp() - (max_age as i64 * 86400);
        entries.retain(|e| e.created.map_or(false, |c| c as i64 >= cutoff));
    }
    if let Some(min_ctx) = params.min_context {
        entries.retain(|e| e.context_length.map_or(false, |c| c >= min_ctx as u64));
    }
    
    // Apply sort
    if let Some(sort) = &params.sort {
        match sort.as_str() {
            "most-popular" => {
                // Already sorted by popularity from API fetch
            }
            "created" => {
                entries.sort_by(|a, b| b.created.unwrap_or(0).cmp(&a.created.unwrap_or(0)));
            }
            "coding" => {
                entries.sort_by(|a, b| b.coding_index.unwrap_or(0.0).partial_cmp(&a.coding_index.unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
            }
            "intelligence" => {
                entries.sort_by(|a, b| b.intelligence_index.unwrap_or(0.0).partial_cmp(&a.intelligence_index.unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
            }
            "agentic" => {
                entries.sort_by(|a, b| b.agentic_index.unwrap_or(0.0).partial_cmp(&a.agentic_index.unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
            }
            "price_input" => {
                entries.sort_by(|a, b| a.prompt_cost.unwrap_or(f64::MAX).partial_cmp(&b.prompt_cost.unwrap_or(f64::MAX)).unwrap_or(std::cmp::Ordering::Equal));
            }
            "price_output" => {
                entries.sort_by(|a, b| a.completion_cost.unwrap_or(f64::MAX).partial_cmp(&b.completion_cost.unwrap_or(f64::MAX)).unwrap_or(std::cmp::Ordering::Equal));
            }
            "price" => {
                entries.sort_by(|a, b| combined_price(a).unwrap_or(f64::MAX).partial_cmp(&combined_price(b).unwrap_or(f64::MAX)).unwrap_or(std::cmp::Ordering::Equal));
            }
            "coding_price" => {
                entries.sort_by(|a, b| price_to_index_ratio(b, b.coding_index).unwrap_or(0.0).partial_cmp(&price_to_index_ratio(a, a.coding_index).unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
            }
            "intelligence_price" => {
                entries.sort_by(|a, b| price_to_index_ratio(b, b.intelligence_index).unwrap_or(0.0).partial_cmp(&price_to_index_ratio(a, a.intelligence_index).unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
            }
            "agentic_price" => {
                entries.sort_by(|a, b| price_to_index_ratio(b, b.agentic_index).unwrap_or(0.0).partial_cmp(&price_to_index_ratio(a, a.agentic_index).unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));
            }
            _ => {}
        }
    }
    
    entries
}

/// Compute combined price (input + output) for a model entry.
fn combined_price(e: &ModelEntry) -> Option<f64> {
    match (e.prompt_cost, e.completion_cost) {
        (Some(p), Some(c)) => Some(p + c),
        _ => None,
    }
}

/// Compute price-to-index ratio for a model entry.
/// Uses a nominal minimal fee (0.0000001) for free models so they sort before paid models
/// but still have relative ordering based on their indices.
fn price_to_index_ratio(e: &ModelEntry, index: Option<f32>) -> Option<f64> {
    let total_price = combined_price(e);
    match (total_price, index) {
        (Some(p), Some(i)) if p <= 0.0 && i > 0.0 => {
            // Free model with index: use nominal minimal fee for sorting
            Some(i as f64 / 0.0000001)
        }
        (Some(p), Some(i)) if p > 0.0 => Some(i as f64 / p),
        _ => None,
    }
}

/// Offline / fetch-failure fallback: curated table gated by availability.
fn static_fallback() -> Vec<ModelEntry> {
    let entries: Vec<ModelEntry> = DIRECT_PRICING
        .iter()
        .filter(|(p, _, _, _, _, _)| provider_available(p))
        .map(|(p, m, ctx, pc, cc, r)| ModelEntry {
            provider: p.to_string(),
            id: format!("{}:{}", p, m),
            display: format!("{}:{}", p, m),
            context_length: Some(*ctx),
            prompt_cost: Some(*pc),
            completion_cost: Some(*cc),
            reasoning: *r,
            cost_source: CostSource::Direct,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        })
        .collect();
    if DEBUG_PROVIDERS.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("[debug-providers] static_fallback: {} entries from DIRECT_PRICING", entries.len());
        for e in &entries {
            eprintln!("[debug-providers]   {}", e.display);
        }
    }
    entries
}

/// Refresh the catalog cache in the background (fetches from OpenRouter with default params).
pub async fn refresh_catalog() -> Vec<ModelEntry> {
    catalog().await
}

/// Look up a curated direct price; fall back to OpenRouter's price (estimate)./// Look up a curated direct price; fall back to OpenRouter's price (estimate).
fn direct_price(
    ns: &str,
    model: &str,
    or_pc: Option<f64>,
    or_cc: Option<f64>,
) -> (Option<f64>, Option<f64>, CostSource) {
    if let Some(row) = DIRECT_PRICING
        .iter()
        .find(|(p, m, _, _, _, _)| *p == ns && *m == model)
    {
        (Some(row.3), Some(row.4), CostSource::Direct)
    } else {
        (or_pc, or_cc, CostSource::OpenRouterEstimate)
    }
}


/// Live ids from a provider's native `/models` endpoint (best-effort).
async fn native_models(provider: &str) -> Vec<String> {
    // TODO: support env var API base + hard-coded fall-back consistently?
    let (maybeurlbase, var) = match provider {
        "deepseek" => ("https://api.deepseek.com", "DEEPSEEK_API_KEY"),
        "xiaomi" => ("XIAOMI_MIMO_API_BASE", "XIAOMI_MIMO_API_KEY"),
        "openai" => ("https://api.openai.com/v1", "OPENAI_API_KEY"),
        _ => return Vec::new(),
    };
    let urlbase = std::env::var(maybeurlbase).unwrap_or(maybeurlbase.to_string());
    let url = urlbase + "/models";

    let Ok(key) = std::env::var(var) else {
        return Vec::new();
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let req = client.get(url).header("Authorization", format!("Bearer {}", key));
    let Ok(resp) = req.send().await else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(value) = resp.json::<Value>().await else {
        return Vec::new();
    };
    value
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Prune/extend direct entries for providers exposing `/models`, using the
/// live id set to drop stale models and surface new ones. Best-effort.
async fn liveness_prune(mut entries: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let debug = DEBUG_PROVIDERS.load(std::sync::atomic::Ordering::Relaxed);
    for provider in ["deepseek", "openai", "xiaomi"] {
        let before = entries.len();
        if !provider_available(provider) {
            if debug {
                eprintln!("[debug-providers] liveness_prune: {} not available (skipped)", provider);
            }
            continue;
        }
        let live = native_models(provider).await;
        if live.is_empty() {
            if debug {
                eprintln!("[debug-providers] liveness_prune: {} native /models returned empty (skipped)", provider);
            }
            continue;
        }
        if debug {
            eprintln!("[debug-providers] liveness_prune: {} returned {} live models", provider, live.len());
            for m in &live {
                eprintln!("[debug-providers]   live: {}", m);
            }
        }
        let live_set: HashSet<String> = live.iter().cloned().collect();
        let retained = entries.len();
        entries.retain(|e| {
            if e.provider == provider {
                // Extract model name from "provider:model" format.
                let model = e.id.splitn(2, ':').nth(1).unwrap_or(&e.id);
                live_set.contains(model)
            } else {
                true
            }
        });
        let pruned = retained - entries.len();
        if debug && pruned > 0 {
            eprintln!("[debug-providers] liveness_prune: pruned {} stale entries for {}", pruned, provider);
        }
        // Translate cases where openrouter provider does not match rig provider ID
        let rigprovider = match provider {
            "xiaomi" => "xiaomimimo",
            _ => provider
        };
        for m in live {
            let id = format!("{}:{}", rigprovider, m);
            if !entries.iter().any(|e| e.id == id) {
                entries.push(ModelEntry {
                    provider: provider.to_string(),
                    id: id.clone(),
                    display: format!("{}:{}", provider, m),
                    context_length: None,
                    prompt_cost: None,
                    completion_cost: None,
                    reasoning: false,
                    cost_source: CostSource::Direct,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
                });
                if debug {
                    eprintln!("[debug-providers] liveness_prune: added new live model {}", id);
                }
            }
        }
        if debug {
            eprintln!("[debug-providers] liveness_prune: {} entries before, {} after", before, entries.len());
        }
    }
    entries
}

#[derive(Clone)]
struct CacheEntry {
    at: Instant,
    entries: Vec<ModelEntry>,
}

static CACHE: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();
const CACHE_TTL: Duration = Duration::from_secs(300);

/// Return the model catalog: live OpenRouter fetch → extrapolate → liveness
/// prune, cached for 5 minutes. Falls back to the curated table on fetch error.
pub async fn catalog() -> Vec<ModelEntry> {
    let debug = DEBUG_PROVIDERS.load(std::sync::atomic::Ordering::Relaxed);
    if let Some(c) = CACHE
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|g| g.as_ref().cloned())
    {
        if c.at.elapsed() < CACHE_TTL {
            if debug {
                eprintln!("[debug-providers] catalog: returning {} cached entries (age={}s)", c.entries.len(), c.at.elapsed().as_secs());
            }
            return c.entries;
        }
        if debug {
            eprintln!("[debug-providers] catalog: cache stale (age={}s), re-fetching", c.at.elapsed().as_secs());
        }
    }
    let entries = match fetch_openrouter().await {
        Ok(or) => {
            if debug {
                eprintln!("[debug-providers] catalog: OpenRouter returned {} models", or.len());
            }
            extrapolate(or)
        }
        Err(e) => {
            if debug {
                eprintln!("[debug-providers] catalog: OpenRouter fetch failed: {}, using static fallback", e);
            }
            static_fallback()
        }
    };
    let entries = liveness_prune(entries).await;
    let entries = dedupe(entries);
    if debug {
        eprintln!("[debug-providers] catalog: final count = {} entries", entries.len());
    }
    if let Some(mut m) = CACHE.get_or_init(|| Mutex::new(None)).lock().ok() {
        *m = Some(CacheEntry {
            at: Instant::now(),
            entries: entries.clone(),
        });
    }
    entries
}

/// Synchronous, non-blocking catalog: returns the last cached entries if any
/// (even if stale), otherwise the curated static fallback. Used by the live
/// `/model` overlay so it can render instantly without a network round-trip;
/// the network-backed [`catalog`] refreshes the cache in the background.
pub fn catalog_cached() -> Vec<ModelEntry> {
    if let Some(c) = CACHE
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|g| g.as_ref().cloned())
    {
        return c.entries;
    }
    static_fallback()
}

/// Find a catalog entry matching `provider:model` (case-insensitive), using the
/// cached catalog (or the curated static fallback) so the call is non-blocking.
/// Prefers the `Direct`-priced copy when both a direct and an OpenRouter-routed
/// entry exist. Returns `None` when no entry matches (e.g. an unknown model or a
/// provider whose catalog wasn't fetched). Used by `/stat` to read the context
/// window size and per-token pricing for the active model.
pub fn lookup(provider: &str, model: &str) -> Option<ModelEntry> {
    catalog_cached().into_iter().find(|e| {
        let (p, m) = e.qualified_parts();
        p.eq_ignore_ascii_case(provider) && m.eq_ignore_ascii_case(model)
    })
}

/// Remove entries that share the same `id`. `extrapolate` keeps the
/// OpenRouter-routed entry and adds a "direct" entry for every available
/// provider namespace, so OpenRouter-native models (namespace `openrouter`)
/// get a true duplicate and passthrough models (`anthropic/...`) appear twice
/// (once routed via OpenRouter, once direct). Collapsing by `id` keeps one row
/// per model and prefers the `Direct`-priced copy when both exist.
fn dedupe(entries: Vec<ModelEntry>) -> Vec<ModelEntry> {
    use std::collections::HashMap;
    let mut keep: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<ModelEntry> = Vec::with_capacity(entries.len());
    for e in entries {
        match keep.get(&e.id) {
            None => {
                let idx = out.len();
                keep.insert(e.id.clone(), idx);
                out.push(e);
            }
            Some(&idx) => {
                // Prefer Direct-priced entries over estimates.
                if e.cost_source == CostSource::Direct
                    && out[idx].cost_source != CostSource::Direct
                {
                    out[idx] = e;
                }
                // When both have the same cost source, prefer the direct
                // (non-OpenRouter) entry so the model appears as
                // `provider:model` rather than `openrouter:provider/model`.
                else if e.cost_source == out[idx].cost_source
                    && e.provider != "openrouter"
                    && out[idx].provider == "openrouter"
                {
                    out[idx] = e;
                }
            }
        }
    }
    out
}

/// Global debug flag for model catalog operations. Set via
/// [`set_debug_providers`] or the `--debug-providers` CLI flag.
static DEBUG_PROVIDERS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Enable or disable debug logging for provider discovery and catalog
/// extrapolation/filtering.
pub fn set_debug_providers(enabled: bool) {
    DEBUG_PROVIDERS.store(enabled, std::sync::atomic::Ordering::SeqCst);
}

/// Return a human-readable list of all known providers and whether they are
/// configured (API key env var present). Used by `--debug-providers` startup
/// logging.
pub fn list_available_providers() -> Vec<(String, bool, Vec<String>)> {
    let mut out: Vec<(String, bool, Vec<String>)> = Vec::new();
    for p in KNOWN_PROVIDERS {
        let vars = required_env_vars(p);
        let available = provider_available(p);
        let env_list: Vec<String> = vars.iter().map(|v| {
            let val = std::env::var(v).ok().unwrap_or_default();
            if val.is_empty() {
                format!("{}=", v)
            } else {
                format!("{}={}{}", v, &val[..val.len().min(8)], if val.len() > 8 { "…" } else { "" })
            }
        }).collect();
        out.push((p.to_string(), available, env_list));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ollama_treated_available_without_keys() {
        assert!(provider_available("ollama"));
    }

    #[test]
    fn unknown_provider_treated_available() {
        assert!(provider_available("does-not-exist"));
    }

    #[test]
    fn static_fallback_only_returns_available_providers() {
        for e in static_fallback() {
            assert!(
                provider_available(&e.provider),
                "static fallback returned unavailable provider {}",
                e.provider
            );
        }
    }

    #[test]
    fn parse_openrouter_maps_fields_and_scales_pricing() {
        let payload = json!({
            "data": [
                {
                    "id": "deepseek/deepseek-v4-flash",
                    "context_length": 1048576,
                    "pricing": {"prompt": "0.000000435", "completion": "0.00000087"},
                    "supported_parameters": ["tools", "reasoning"]
                },
                {
                    "id": "openai/gpt-4o",
                    "context_length": 128000,
                    "pricing": {"prompt": "0.0000025", "completion": "0.00001"},
                    "supported_parameters": ["tools"]
                }
            ]
        });
        let entries = parse_openrouter(&payload);
        assert_eq!(entries.len(), 2);
        let first = &entries[0];
        assert_eq!(first.id, "openrouter:deepseek/deepseek-v4-flash");
        assert_eq!(first.context_length, Some(1048576));
        assert_eq!(first.prompt_cost, Some(0.435));
        assert_eq!(first.completion_cost, Some(0.87));
        assert!(first.reasoning);
        assert_eq!(first.cost_source, CostSource::OpenRouterEstimate);

        let second = &entries[1];
        assert!(!second.reasoning);
        assert_eq!(second.prompt_cost, Some(2.5));
    }

    #[test]
    fn parse_openrouter_handles_missing_data() {
        assert!(parse_openrouter(&json!({"data": []})).is_empty());
        assert!(parse_openrouter(&json!({"nope": 1})).is_empty());
    }

    // OpenRouter entries carry `provider == "openrouter"` and an `id` that is
    // the full (namespaced) model path; the qualified `provider:model` form
    // must keep the namespace (e.g. `openrouter:tencent/hy3:free`), not drop
    // it. Direct entries keep `provider:model` with the namespace stripped.
    #[test]
    fn qualified_parts_keeps_namespace_for_openrouter() {
        let or = ModelEntry {
            provider: "openrouter".into(),
            id: "openrouter:tencent/hy3:free".into(),
            display: "openrouter:tencent/hy3:free".into(),
            context_length: None,
            prompt_cost: None,
            completion_cost: None,
            reasoning: false,
            cost_source: CostSource::OpenRouterEstimate,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        assert_eq!(or.qualified_parts(), ("openrouter".to_string(), "tencent/hy3:free".to_string()));
        assert_eq!(or.qualified(), "openrouter:tencent/hy3:free");

        let direct = ModelEntry {
            provider: "tencent".into(),
            id: "tencent:hy3:free".into(),
            display: "tencent:hy3:free".into(),
            context_length: None,
            prompt_cost: None,
            completion_cost: None,
            reasoning: false,
            cost_source: CostSource::Direct,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        assert_eq!(direct.qualified_parts(), ("tencent".to_string(), "hy3:free".to_string()));
        assert_eq!(direct.qualified(), "tencent:hy3:free");
    }

    // `extrapolate` keeps the OpenRouter entry and adds a direct one (when the
    // provider is available) priced from DIRECT_PRICING or OR-estimate.
    #[test]
    fn extrapolate_adds_direct_entries_for_available_providers() {
        // Make the providers we assert on deterministically available.
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        std::env::set_var("DEEPSEEK_API_KEY", "test-key");
        // deepseek-chat IS in DIRECT_PRICING (Direct); deepseek-v4-flash is not
        // (so it stays an OpenRouterEstimate); anthropic entry exercises the
        // available-but-no-direct-price path with reasoning preserved.
        let payload = json!({
            "data": [
                {"id": "deepseek/deepseek-chat", "context_length": 128000,
                 "pricing": {"prompt": "0.00000027", "completion": "0.0000011"},
                 "supported_parameters": ["tools"]},
                {"id": "deepseek/deepseek-v4-flash", "context_length": 128000,
                 "pricing": {"prompt": "0.00000027", "completion": "0.0000011"},
                 "supported_parameters": ["tools"]},
                {"id": "anthropic/claude-3-7-sonnet", "context_length": 200000,
                 "pricing": {"prompt": "0.000003", "completion": "0.000015"},
                 "supported_parameters": ["tools", "reasoning"]}
            ]
        });
        let entries = extrapolate(parse_openrouter(&payload));
        // deepseek-chat available -> direct entry priced from DIRECT_PRICING.
        let chat = entries
            .iter()
            .find(|e| e.provider == "deepseek" && e.display == "deepseek:deepseek-chat")
            .expect("direct deepseek-chat entry");
        assert_eq!(chat.cost_source, CostSource::Direct);
        assert_eq!(chat.prompt_cost, Some(0.0));
        // deepseek-v4-flash available but not in DIRECT_PRICING -> estimate.
        let flash = entries
            .iter()
            .find(|e| e.provider == "deepseek" && e.display == "deepseek:deepseek-v4-flash")
            .expect("direct deepseek-v4-flash entry");
        assert_eq!(flash.cost_source, CostSource::OpenRouterEstimate);
        // OR-routed entries kept too.
        assert!(entries
            .iter()
            .any(|e| e.provider == "openrouter" && e.id == "openrouter:deepseek/deepseek-chat"));
        // anthropic available -> direct entry; priced from DIRECT_PRICING (Direct),
        // reasoning preserved.
        let anthropic_direct = entries
            .iter()
            .find(|e| e.provider == "anthropic" && e.display == "anthropic:claude-3-7-sonnet")
            .expect("direct anthropic entry");
        assert_eq!(anthropic_direct.cost_source, CostSource::Direct);
        assert!(anthropic_direct.reasoning);
    }

    #[test]
    fn format_option_shows_est_for_openrouter_fallback() {
        let direct = ModelEntry {
            provider: "deepseek".into(),
            id: "deepseek:deepseek-chat".into(),
            display: "deepseek:deepseek-chat".into(),
            context_length: Some(128_000),
            prompt_cost: Some(0.0),
            completion_cost: Some(0.0),
            reasoning: false,
            cost_source: CostSource::Direct,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        assert_eq!(
            format_option(&direct),
            "deepseek:deepseek-chat  ctx 128000  $0.00/0.00 per M"
        );
        // No local price -> OpenRouter price shown, tagged (est).
        let est = ModelEntry {
            provider: "deepseek".into(),
            id: "deepseek:deepseek-v4-flash".into(),
            display: "deepseek:deepseek-v4-flash".into(),
            context_length: Some(128_000),
            prompt_cost: Some(0.435),
            completion_cost: Some(0.87),
            reasoning: false,
            cost_source: CostSource::OpenRouterEstimate,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        let s = format_option(&est);
        assert!(s.contains("(est)"), "{}", s);
        assert!(s.contains("$0.43/0.87 per M"), "{}", s);
        // Missing cost entirely -> no per-M line, no (est) tag.
        let missing = ModelEntry {
            provider: "deepseek".into(),
            id: "deepseek:unknown".into(),
            display: "deepseek:unknown".into(),
            context_length: None,
            prompt_cost: None,
            completion_cost: None,
            reasoning: false,
            cost_source: CostSource::Direct,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        let s = format_option(&missing);
        assert!(!s.contains("per M"), "{}", s);
        assert!(!s.contains("(est)"), "{}", s);
    }

    #[test]
    fn ctx_cell_and_cost_cell_feed_the_table_columns() {
        // Direct price -> plain `$p/$c`, no `est`/reasoning tags.
        let direct = ModelEntry {
            provider: "deepseek".into(),
            id: "deepseek:deepseek-chat".into(),
            display: "deepseek:deepseek-chat".into(),
            context_length: Some(128_000),
            prompt_cost: Some(0.0),
            completion_cost: Some(0.0),
            reasoning: false,
            cost_source: CostSource::Direct,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        assert_eq!(ctx_cell(&direct), "128000");
        assert_eq!(cost_cell(&direct), "$0.00/$0.00");
        assert_eq!(reason_cell(&direct), "✗");
        // OpenRouter estimate + reasoning -> `~` prefix on cost, `✓` on reason.
        let est = ModelEntry {
            provider: "deepseek".into(),
            id: "deepseek:deepseek-v4-flash".into(),
            display: "deepseek:deepseek-v4-flash".into(),
            context_length: Some(1048576),
            prompt_cost: Some(0.435),
            completion_cost: Some(0.87),
            reasoning: true,
            cost_source: CostSource::OpenRouterEstimate,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        assert_eq!(ctx_cell(&est), "1048576");
        let c = cost_cell(&est);
        assert!(c.contains("$0.43/$0.87"), "{}", c);
        assert!(c.starts_with('~'), "estimate marked with ~ inline: {}", c);
        assert!(!c.contains("est"), "no separate est tag: {}", c);
        assert_eq!(reason_cell(&est), "✓");
        // Missing fields -> em dash placeholders.
        let missing = ModelEntry {
            provider: "deepseek".into(),
            id: "deepseek:unknown".into(),
            display: "deepseek:unknown".into(),
            context_length: None,
            prompt_cost: None,
            completion_cost: None,
            reasoning: false,
            cost_source: CostSource::Direct,
            created: None,
            coding_index: None,
            intelligence_index: None,
            agentic_index: None,
        };
        assert_eq!(ctx_cell(&missing), "—");
        assert_eq!(cost_cell(&missing), "—");
    }
    // Non-OAuth providers added from rig 0.40 (chat-capable). Verifies the
    // env-var + curated catalog wiring so they are selectable and listed.
    #[test]
    fn new_providers_wired_with_env_vars_and_catalog_entries() {
        assert_eq!(required_env_vars("doubleword"), &["DOUBLEWORD_API_KEY"]);
        assert_eq!(required_env_vars("llamafile"), &["LLAMAFILE_API_BASE_URL"]);
        assert_eq!(required_env_vars("minimax"), &["MINIMAX_API_KEY"]);
        assert_eq!(required_env_vars("xiaomimimo"), &["XIAOMI_MIMO_API_KEY"]);
        assert_eq!(required_env_vars("zai"), &["ZAI_API_KEY"]);
        assert_eq!(required_env_vars("chatgpt"), &["CHATGPT_ACCESS_TOKEN"]);
        assert_eq!(
            required_env_vars("copilot"),
            &["GITHUB_COPILOT_API_KEY", "COPILOT_GITHUB_ACCESS_TOKEN"]
        );
        for (p, m) in [
            ("doubleword", "Qwen/Qwen3.5-4B"),
            ("llamafile", "LLaMA_CPP"),
            ("minimax", "MiniMax-M2.7"),
            ("xiaomimimo", "mimo-v2.5-pro"),
            ("zai", "glm-4.6"),
            ("chatgpt", "gpt-5"),
            ("copilot", "gpt-5-codex"),
        ] {
            assert!(
                DIRECT_PRICING
                    .iter()
                    .any(|(pp, mm, _, _, _, _)| *pp == p && *mm == m),
                "missing catalog entry for {}/{}", p, m
            );
        }
    }
    #[tokio::test]
    #[ignore]
    async fn live_catalog_fetches_and_extrapolates() {
        let entries = catalog().await;
        assert!(!entries.is_empty());
        assert!(entries.iter().any(|e| e.provider == "deepseek"));
    }
}
