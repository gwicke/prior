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
    ("moonshot", "moonshot-v1-8k", 8_192, 0.0, 0.0, false),
    ("together", "meta-llama/Llama-3-70b-chat-hf", 8_192, 0.0, 0.0, false),
    ("perplexity", "llama-3-sonar-large-32k-online", 32_000, 1.0, 1.0, false),
    ("hyperbolic", "meta-llama/Llama-3-70b-instruct", 8_192, 0.0, 0.0, false),
    ("huggingface", "meta-llama/Llama-3-70b-chat-hf", 8_192, 0.0, 0.0, false),
    ("galadriel", "llama3-70b", 8_192, 0.0, 0.0, false),
    ("mira", "mira-v1", 8_192, 0.0, 0.0, false),
];

fn required_env_vars(provider: &str) -> &'static [&'static str] {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "azure" => &["AZURE_OPENAI_API_KEY", "AZURE_OPENAI_ENDPOINT"],
        "cohere" => &["COHERE_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "galadriel" => &["GALADRIEL_API_KEY"],
        "gemini" | "google" => &["GOOGLE_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "huggingface" => &["HF_TOKEN"],
        "hyperbolic" => &["HYPERBOLIC_API_KEY"],
        "mira" => &["MIRA_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "moonshot" => &["MOONSHOT_API_KEY"],
        "ollama" => &[],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "perplexity" => &["PERPLEXITY_API_KEY"],
        "together" => &["TOGETHER_API_KEY"],
        "xai" => &["XAI_API_KEY"],
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
        let id = id.to_string();
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
        out.push(ModelEntry {
            provider: "openrouter".to_string(),
            id: id.clone(),
            display: id.clone(),
            context_length,
            prompt_cost,
            completion_cost,
            reasoning,
            cost_source: CostSource::OpenRouterEstimate,
        });
    }
    out
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
    let url = "https://openrouter.ai/api/v1/models";
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
    for e in or_entries {
        out.push(e.clone());
        if let Some((ns, model)) = e.id.split_once('/') {
            if provider_available(ns) {
                let (pc, cc, source) = direct_price(ns, model, e.prompt_cost, e.completion_cost);
                out.push(ModelEntry {
                    provider: ns.to_string(),
                    id: e.id.clone(),
                    display: format!("{}:{}", ns, model),
                    context_length: e.context_length,
                    prompt_cost: pc,
                    completion_cost: cc,
                    reasoning: e.reasoning,
                    cost_source: source,
                });
            }
        }
    }
    out
}

/// Look up a curated direct price; fall back to OpenRouter's price (estimate).
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

/// Offline / fetch-failure fallback: curated table gated by availability.
fn static_fallback() -> Vec<ModelEntry> {
    DIRECT_PRICING
        .iter()
        .filter(|(p, _, _, _, _, _)| provider_available(p))
        .map(|(p, m, ctx, pc, cc, r)| ModelEntry {
            provider: p.to_string(),
            id: format!("{}/{}", p, m),
            display: format!("{}:{}", p, m),
            context_length: Some(*ctx),
            prompt_cost: Some(*pc),
            completion_cost: Some(*cc),
            reasoning: *r,
            cost_source: CostSource::Direct,
        })
        .collect()
}

/// Live ids from a provider's native `/models` endpoint (best-effort).
async fn native_models(provider: &str) -> Vec<String> {
    let (url, var) = match provider {
        "deepseek" => ("https://api.deepseek.com/models", "DEEPSEEK_API_KEY"),
        "openai" => ("https://api.openai.com/v1/models", "OPENAI_API_KEY"),
        _ => return Vec::new(),
    };
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
    for provider in ["deepseek", "openai"] {
        if !provider_available(provider) {
            continue;
        }
        let live = native_models(provider).await;
        if live.is_empty() {
            continue;
        }
        let live_set: HashSet<String> = live.iter().cloned().collect();
        entries.retain(|e| {
            if e.provider == provider {
                let model = e.id.splitn(2, '/').nth(1).unwrap_or(&e.id);
                live_set.contains(model)
            } else {
                true
            }
        });
        for m in live {
            let id = format!("{}/{}", provider, m);
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
                });
            }
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
    if let Some(c) = CACHE
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|g| g.as_ref().cloned())
    {
        if c.at.elapsed() < CACHE_TTL {
            return c.entries;
        }
    }
    let entries = match fetch_openrouter().await {
        Ok(or) => extrapolate(or),
        Err(_) => static_fallback(),
    };
    let entries = liveness_prune(entries).await;
    if let Some(mut m) = CACHE.get_or_init(|| Mutex::new(None)).lock().ok() {
        *m = Some(CacheEntry {
            at: Instant::now(),
            entries: entries.clone(),
        });
    }
    entries
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
        assert_eq!(first.id, "deepseek/deepseek-v4-flash");
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
            .any(|e| e.provider == "openrouter" && e.id == "deepseek/deepseek-chat"));
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
            id: "deepseek/deepseek-chat".into(),
            display: "deepseek:deepseek-chat".into(),
            context_length: Some(128_000),
            prompt_cost: Some(0.0),
            completion_cost: Some(0.0),
            reasoning: false,
            cost_source: CostSource::Direct,
        };
        assert_eq!(
            format_option(&direct),
            "deepseek:deepseek-chat  ctx 128000  $0.00/0.00 per M"
        );
        // No local price -> OpenRouter price shown, tagged (est).
        let est = ModelEntry {
            provider: "deepseek".into(),
            id: "deepseek/deepseek-v4-flash".into(),
            display: "deepseek:deepseek-v4-flash".into(),
            context_length: Some(128_000),
            prompt_cost: Some(0.435),
            completion_cost: Some(0.87),
            reasoning: false,
            cost_source: CostSource::OpenRouterEstimate,
        };
        let s = format_option(&est);
        assert!(s.contains("(est)"), "{}", s);
        assert!(s.contains("$0.43/0.87 per M"), "{}", s);
        // Missing cost entirely -> no per-M line, no (est) tag.
        let missing = ModelEntry {
            provider: "deepseek".into(),
            id: "deepseek/unknown".into(),
            display: "deepseek:unknown".into(),
            context_length: None,
            prompt_cost: None,
            completion_cost: None,
            reasoning: false,
            cost_source: CostSource::Direct,
        };
        let s = format_option(&missing);
        assert!(!s.contains("per M"), "{}", s);
        assert!(!s.contains("(est)"), "{}", s);
    }
    #[tokio::test]
    #[ignore]
    async fn live_catalog_fetches_and_extrapolates() {
        let entries = catalog().await;
        assert!(!entries.is_empty());
        assert!(entries.iter().any(|e| e.provider == "deepseek"));
    }
}
