//! Built-in model catalog (generated from `models.generated.ts` by `data/generate-models.mjs`), cost calculation and
//! thinking-level clamping (`models.ts`).

use crate::types::{Model, ModelCostTier, Usage};
use indexmap::IndexMap;
use std::sync::LazyLock;

type Catalog = IndexMap<String, IndexMap<String, Model>>;

static CATALOG: LazyLock<Catalog> =
    LazyLock::new(|| serde_json::from_str(include_str!("../data/models.json")).expect("built-in model catalog parses"));

/// The models of a built-in provider: the generated catalog's, or the `claude-code` provider's own
/// ([`crate::claude_code::MODELS`], not in the generated catalog).
fn provider_models(provider: &str) -> Option<&'static IndexMap<String, Model>> {
    if provider == crate::claude_code::PROVIDER {
        return Some(&crate::claude_code::MODELS);
    }
    CATALOG.get(provider)
}

/// Built-in providers in catalog order, then `claude-code` (`builtinProviders`: last, so an automatic model
/// fallback never lands on a provider without tool calling).
pub fn builtin_providers() -> impl Iterator<Item = &'static str> {
    CATALOG
        .keys()
        .map(String::as_str)
        .chain(std::iter::once(crate::claude_code::PROVIDER))
}

/// Built-in models of `provider` in catalog order; empty for an unknown provider.
pub fn builtin_models(provider: &str) -> impl Iterator<Item = &'static Model> {
    provider_models(provider).into_iter().flat_map(IndexMap::values)
}

/// A built-in model; `claude-subscription/<id>` resolves through that provider's pinned route table (any valid
/// id: unpinned ids pass through at the 200K window).
pub fn builtin_model(provider: &str, id: &str) -> Option<&'static Model> {
    if provider == crate::claude_subscription::PROVIDER {
        return crate::claude_subscription::catalog::interned(id);
    }
    provider_models(provider)?.get(id)
}

/// Fill `usage.cost` from the model's rates: the highest tier whose threshold the total input exceeds applies to the
/// whole request; 1h cache writes cost twice the base input rate.
pub fn calculate_cost(model: &Model, usage: &mut Usage) {
    let input_tokens = (usage.input + usage.cache_read + usage.cache_write) as f64;
    let base = ModelCostTier {
        input_tokens_above: -1.0,
        input: model.cost.input,
        output: model.cost.output,
        cache_read: model.cost.cache_read,
        cache_write: model.cost.cache_write,
    };
    let mut rates = &base;
    let mut matched = -1.0;
    for tier in model.cost.tiers.iter().flatten() {
        if input_tokens > tier.input_tokens_above && tier.input_tokens_above > matched {
            rates = tier;
            matched = tier.input_tokens_above;
        }
    }
    let long_write = usage.cache_write_1h.unwrap_or(0) as f64;
    let short_write = usage.cache_write as f64 - long_write;
    let cost = &mut usage.cost;
    cost.input = (rates.input / 1_000_000.0) * usage.input as f64;
    cost.output = (rates.output / 1_000_000.0) * usage.output as f64;
    cost.cache_read = (rates.cache_read / 1_000_000.0) * usage.cache_read as f64;
    cost.cache_write = (rates.cache_write * short_write + rates.input * 2.0 * long_write) / 1_000_000.0;
    cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
}

const EXTENDED_THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub fn supported_thinking_levels(model: &Model) -> Vec<&'static str> {
    if !model.reasoning {
        return vec!["off"];
    }
    EXTENDED_THINKING_LEVELS
        .into_iter()
        .filter(|level| match model.thinking_level(level) {
            crate::types::Mapped::Unsupported => false,
            crate::types::Mapped::Missing => !matches!(*level, "xhigh" | "max"),
            crate::types::Mapped::Value(_) => true,
        })
        .collect()
}

/// The requested level if supported, else the nearest supported level above it, else below it.
pub fn clamp_thinking_level(model: &Model, level: &str) -> &'static str {
    let available = supported_thinking_levels(model);
    let first = available.first().copied().unwrap_or("off");
    if let Some(found) = available.iter().find(|l| **l == level) {
        return found;
    }
    let Some(requested) = EXTENDED_THINKING_LEVELS.iter().position(|l| *l == level) else {
        return first;
    };
    let above = EXTENDED_THINKING_LEVELS[requested..]
        .iter()
        .find(|l| available.contains(l));
    let below = EXTENDED_THINKING_LEVELS[..requested]
        .iter()
        .rev()
        .find(|l| available.contains(l));
    above.or(below).copied().unwrap_or(first)
}
