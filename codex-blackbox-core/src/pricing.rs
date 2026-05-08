use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use serde::Deserialize;
use tracing::{info, warn};

pub const BUILTIN_COST_SOURCE: &str = "builtin_model_family_pricing";
pub const BUILTIN_OPENAI_API_COST_SOURCE: &str = "builtin_openai_api_pricing_standard_under_270k";
pub const MIXED_COST_SOURCE: &str = "mixed_pricing_sources";
const PRICING_FILE_ENV: &str = "CODEX_BLACKBOX_PRICING_FILE";
const OPENAI_STANDARD_PRICING_CONTEXT_LIMIT_TOKENS: u64 = 270_000;
const ZERO_PRICING: ModelPricing = ModelPricing {
    input: 0.0,
    output: 0.0,
    cache_read: 0.0,
    cache_create: 0.0,
};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_create: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EstimatedCostBreakdown {
    pub total_cost_dollars: f64,
    pub cost_source: String,
    pub trusted_for_budget_enforcement: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPricing {
    pub pricing: ModelPricing,
    pub cost_source: String,
    pub trusted_for_budget_enforcement: bool,
}

#[derive(Clone, Debug)]
pub struct PricingCatalog {
    trusted_for_budget_enforcement: bool,
    catalog_source: String,
    family: HashMap<String, ModelPricing>,
    model: HashMap<String, ModelPricing>,
    builtin_only: bool,
}

#[derive(Default, Deserialize)]
struct PricingFile {
    #[serde(default)]
    trusted_for_budget_enforcement: bool,
    #[serde(default)]
    source_label: Option<String>,
    #[serde(default)]
    family: HashMap<String, ModelPricing>,
    #[serde(default)]
    model: HashMap<String, ModelPricing>,
}

pub static PRICING_CATALOG: LazyLock<PricingCatalog> = LazyLock::new(PricingCatalog::load_from_env);

impl PricingCatalog {
    pub fn builtin() -> Self {
        Self {
            trusted_for_budget_enforcement: false,
            catalog_source: BUILTIN_COST_SOURCE.to_string(),
            family: HashMap::new(),
            model: HashMap::new(),
            builtin_only: true,
        }
    }

    pub fn load_from_env() -> Self {
        let Some(path) = std::env::var(PRICING_FILE_ENV).ok() else {
            info!(
                cost_source = BUILTIN_COST_SOURCE,
                trusted_for_budget_enforcement = false,
                "using built-in model-family pricing"
            );
            return Self::builtin();
        };

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                warn!(
                    path = %path,
                    error = %err,
                    "failed to read pricing file, falling back to built-in pricing"
                );
                return Self::builtin();
            }
        };

        match Self::from_toml_str(&text, &path) {
            Ok(catalog) => {
                info!(
                    path = %path,
                    cost_source = %catalog.catalog_source,
                    trusted_for_budget_enforcement = catalog.trusted_for_budget_enforcement,
                    exact_models = catalog.model.len(),
                    families = catalog.family.len(),
                    "loaded pricing catalog from file"
                );
                catalog
            }
            Err(err) => {
                warn!(
                    path = %path,
                    error = %err,
                    "failed to parse pricing file, falling back to built-in pricing"
                );
                Self::builtin()
            }
        }
    }

    pub fn from_toml_str(text: &str, source_path: &str) -> Result<Self, String> {
        let parsed: PricingFile =
            toml::from_str(text).map_err(|err| format!("parse pricing toml: {err}"))?;

        let family = parsed
            .family
            .into_iter()
            .map(|(key, pricing)| (key.trim().to_ascii_lowercase(), pricing))
            .collect::<HashMap<_, _>>();
        let model = parsed
            .model
            .into_iter()
            .map(|(key, pricing)| (key.trim().to_string(), pricing))
            .collect::<HashMap<_, _>>();

        let source_label = parsed
            .source_label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                Path::new(source_path)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(|stem| stem.to_string())
            })
            .unwrap_or_else(|| "custom".to_string());

        Ok(Self {
            trusted_for_budget_enforcement: parsed.trusted_for_budget_enforcement,
            catalog_source: format!("pricing_file:{source_label}"),
            family,
            model,
            builtin_only: false,
        })
    }

    pub fn active_catalog_source(&self) -> &str {
        &self.catalog_source
    }

    pub fn trusted_for_budget_enforcement(&self) -> bool {
        self.trusted_for_budget_enforcement
    }

    pub fn resolve(&self, model: &str) -> ResolvedPricing {
        if let Some(pricing) = self.model.get(model) {
            return ResolvedPricing {
                pricing: *pricing,
                cost_source: self.catalog_source.clone(),
                trusted_for_budget_enforcement: !self.builtin_only
                    && self.trusted_for_budget_enforcement,
            };
        }

        if let Some(family) = family_for_model(model) {
            if let Some(pricing) = self.family.get(family) {
                return ResolvedPricing {
                    pricing: *pricing,
                    cost_source: self.catalog_source.clone(),
                    trusted_for_budget_enforcement: !self.builtin_only
                        && self.trusted_for_budget_enforcement,
                };
            }
        }

        if let Some((pricing, cost_source)) = builtin_pricing(model) {
            return ResolvedPricing {
                pricing,
                cost_source: cost_source.to_string(),
                trusted_for_budget_enforcement: false,
            };
        }

        ResolvedPricing {
            pricing: ZERO_PRICING,
            cost_source: unpriced_unknown_model_cost_source(model),
            trusted_for_budget_enforcement: false,
        }
    }
}

pub fn active_catalog_source() -> String {
    PRICING_CATALOG.active_catalog_source().to_string()
}

pub fn trusted_for_budget_enforcement() -> bool {
    PRICING_CATALOG.trusted_for_budget_enforcement()
}

pub fn summarize_cost_sources(sources: &std::collections::HashSet<String>) -> String {
    match sources.len() {
        0 => active_catalog_source(),
        1 => sources
            .iter()
            .next()
            .cloned()
            .unwrap_or_else(active_catalog_source),
        _ => MIXED_COST_SOURCE.to_string(),
    }
}

pub fn resolve_pricing(model: &str) -> ResolvedPricing {
    PRICING_CATALOG.resolve(model)
}

pub fn unpriced_unknown_model_cost_source(model: &str) -> String {
    format!("codex_unpriced:unknown_model:{model}")
}

pub fn unpriced_long_context_cost_source(model: &str) -> String {
    format!("codex_unpriced:long_context_api_pricing:{model}")
}

pub fn is_unpriced_cost_source(cost_source: &str) -> bool {
    cost_source.starts_with("codex_unpriced:")
}

pub fn token_cost(tokens: u64, price_per_mtok: f64) -> f64 {
    (tokens as f64) * price_per_mtok / 1_000_000.0
}

pub fn estimate_cost_dollars(
    model: &str,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_create: u64,
) -> EstimatedCostBreakdown {
    let resolved = resolve_pricing(model);
    let total_cost_dollars = token_cost(input, resolved.pricing.input)
        + token_cost(output, resolved.pricing.output)
        + token_cost(cache_read, resolved.pricing.cache_read)
        + token_cost(cache_create, resolved.pricing.cache_create);

    EstimatedCostBreakdown {
        total_cost_dollars,
        cost_source: resolved.cost_source,
        trusted_for_budget_enforcement: resolved.trusted_for_budget_enforcement,
    }
}

pub fn estimate_codex_api_cost_dollars(
    model: &str,
    input: u64,
    cached_input: u64,
    output: u64,
) -> EstimatedCostBreakdown {
    let resolved = resolve_pricing(model);
    if input > OPENAI_STANDARD_PRICING_CONTEXT_LIMIT_TOKENS
        && resolved.cost_source == BUILTIN_OPENAI_API_COST_SOURCE
    {
        return EstimatedCostBreakdown {
            total_cost_dollars: 0.0,
            cost_source: unpriced_long_context_cost_source(model),
            trusted_for_budget_enforcement: false,
        };
    }

    let uncached_input = input.saturating_sub(cached_input);
    let total_cost_dollars = token_cost(uncached_input, resolved.pricing.input)
        + token_cost(cached_input, resolved.pricing.cache_read)
        + token_cost(output, resolved.pricing.output);

    EstimatedCostBreakdown {
        total_cost_dollars,
        cost_source: resolved.cost_source,
        trusted_for_budget_enforcement: resolved.trusted_for_budget_enforcement,
    }
}

pub fn estimate_cache_rebuild_waste_dollars(
    model: &str,
    cache_create: u64,
) -> EstimatedCostBreakdown {
    let resolved = resolve_pricing(model);
    let rebuild_delta = (resolved.pricing.cache_create - resolved.pricing.cache_read).max(0.0);

    EstimatedCostBreakdown {
        total_cost_dollars: token_cost(cache_create, rebuild_delta),
        cost_source: resolved.cost_source,
        trusted_for_budget_enforcement: resolved.trusted_for_budget_enforcement,
    }
}

fn family_for_model(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("gpt-5.5") {
        Some("gpt-5.5")
    } else if lower.starts_with("gpt-5.4-mini") {
        Some("gpt-5.4-mini")
    } else if lower.starts_with("gpt-5.4") {
        Some("gpt-5.4")
    } else {
        None
    }
}

fn builtin_pricing(model: &str) -> Option<(ModelPricing, &'static str)> {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("gpt-5.5") {
        Some((
            ModelPricing {
                input: 5.0,
                output: 30.0,
                cache_read: 0.50,
                cache_create: 5.0,
            },
            BUILTIN_OPENAI_API_COST_SOURCE,
        ))
    } else if lower.starts_with("gpt-5.4-mini") {
        Some((
            ModelPricing {
                input: 0.75,
                output: 4.50,
                cache_read: 0.075,
                cache_create: 0.75,
            },
            BUILTIN_OPENAI_API_COST_SOURCE,
        ))
    } else if lower.starts_with("gpt-5.4") {
        Some((
            ModelPricing {
                input: 2.50,
                output: 15.0,
                cache_read: 0.25,
                cache_create: 2.50,
            },
            BUILTIN_OPENAI_API_COST_SOURCE,
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        estimate_cache_rebuild_waste_dollars, estimate_codex_api_cost_dollars,
        estimate_cost_dollars, PricingCatalog, BUILTIN_OPENAI_API_COST_SOURCE,
    };

    #[test]
    fn pricing_catalog_resolves_exact_model_family_and_builtin_fallback() {
        let catalog = PricingCatalog::from_toml_str(
            r#"
trusted_for_budget_enforcement = true

[family."gpt-5.4"]
input = 2.10
output = 10.50
cache_read = 0.21
cache_create = 2.63

[model."gpt-5.4-catalog-exact"]
input = 1.95
output = 9.75
cache_read = 0.20
cache_create = 2.45
"#,
            "/tmp/contract-2026q2.toml",
        )
        .expect("parse catalog");

        let exact = catalog.resolve("gpt-5.4-catalog-exact");
        assert_eq!(exact.cost_source, "pricing_file:contract-2026q2");
        assert!(exact.trusted_for_budget_enforcement);
        assert_eq!(exact.pricing.input, 1.95);

        let family = catalog.resolve("gpt-5.4-preview");
        assert_eq!(family.cost_source, "pricing_file:contract-2026q2");
        assert!(family.trusted_for_budget_enforcement);
        assert_eq!(family.pricing.output, 10.50);

        let builtin = catalog.resolve("gpt-5.5");
        assert_eq!(builtin.cost_source, BUILTIN_OPENAI_API_COST_SOURCE);
        assert!(!builtin.trusted_for_budget_enforcement);
        assert_eq!(builtin.pricing.input, 5.0);

        let gpt = catalog.resolve("gpt-5.5");
        assert_eq!(gpt.cost_source, BUILTIN_OPENAI_API_COST_SOURCE);
        assert!(!gpt.trusted_for_budget_enforcement);
        assert_eq!(gpt.pricing.input, 5.0);
        assert_eq!(gpt.pricing.cache_read, 0.50);
        assert_eq!(gpt.pricing.output, 30.0);
    }

    #[test]
    fn estimate_cost_helpers_share_the_same_catalog() {
        let builtin = estimate_cost_dollars("gpt-5.5", 1_000_000, 0, 0, 0);
        assert_eq!(builtin.cost_source, BUILTIN_OPENAI_API_COST_SOURCE);
        assert!((builtin.total_cost_dollars - 5.0).abs() < f64::EPSILON);

        let waste = estimate_cache_rebuild_waste_dollars("gpt-5.5", 1_000_000);
        assert_eq!(waste.cost_source, BUILTIN_OPENAI_API_COST_SOURCE);
        assert!((waste.total_cost_dollars - 4.5).abs() < 1e-9);
    }

    #[test]
    fn codex_api_estimate_prices_cached_input_as_subset() {
        let estimate = estimate_codex_api_cost_dollars("gpt-5.5", 1_280, 512, 96);

        assert_eq!(estimate.cost_source, BUILTIN_OPENAI_API_COST_SOURCE);
        assert!(!estimate.trusted_for_budget_enforcement);
        assert!((estimate.total_cost_dollars - 0.006976).abs() < 1e-12);
    }

    #[test]
    fn codex_api_estimate_stays_unpriced_for_unknown_or_long_context_models() {
        let unknown = estimate_codex_api_cost_dollars("gpt-codex-fixture", 1_280, 512, 96);
        assert_eq!(unknown.total_cost_dollars, 0.0);
        assert!(unknown
            .cost_source
            .starts_with("codex_unpriced:unknown_model:"));

        let long = estimate_codex_api_cost_dollars("gpt-5.5", 270_001, 0, 96);
        assert_eq!(long.total_cost_dollars, 0.0);
        assert!(long
            .cost_source
            .starts_with("codex_unpriced:long_context_api_pricing:"));
    }
}
