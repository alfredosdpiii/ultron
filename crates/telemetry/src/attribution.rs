//! Provider attribution headers ported from `packages/coding-agent/src/core/provider-attribution.ts`: the only
//! request-visible effect of install telemetry. With telemetry off no attribution header is sent.

use indexmap::IndexMap;
use url::Url;

/// `ProviderHeaders`: a `None` value removes the header downstream.
pub type ProviderHeaders = IndexMap<String, Option<String>>;

fn matches_host(base_url: &str, host: &str) -> bool {
    Url::parse(base_url).is_ok_and(|url| url.host_str() == Some(host))
}

fn headers(pairs: &[(&str, &str)]) -> ProviderHeaders {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), Some((*v).to_owned())))
        .collect()
}

fn default_attribution_headers(provider: &str, base_url: &str) -> Option<ProviderHeaders> {
    if provider == "openrouter" || base_url.contains("openrouter.ai") {
        return Some(headers(&[
            ("HTTP-Referer", "https://pi.dev"),
            ("X-OpenRouter-Title", "pi"),
            ("X-OpenRouter-Categories", "cli-agent"),
        ]));
    }
    if provider == "nvidia" || matches_host(base_url, "integrate.api.nvidia.com") {
        return Some(headers(&[("X-BILLING-INVOKE-ORIGIN", "Pi")]));
    }
    if provider == "cloudflare-workers-ai"
        || provider == "cloudflare-ai-gateway"
        || matches_host(base_url, "api.cloudflare.com")
        || matches_host(base_url, "gateway.ai.cloudflare.com")
    {
        return Some(headers(&[("User-Agent", "pi-coding-agent")]));
    }
    None
}

fn session_headers(provider: &str, base_url: &str, session_id: Option<&str>) -> Option<ProviderHeaders> {
    let session_id = session_id.filter(|id| !id.is_empty())?;
    let opencode = provider == "opencode" || provider == "opencode-go" || matches_host(base_url, "opencode.ai");
    opencode.then(|| headers(&[("x-opencode-session", session_id), ("x-opencode-client", "pi")]))
}

/// `mergeProviderAttributionHeaders`: session headers, then attribution headers (only when `telemetry_enabled`, see
/// [`crate::is_install_telemetry_enabled`]), then each source in order; later values win. `None` when empty.
pub fn merge_provider_attribution_headers(
    provider: &str,
    base_url: &str,
    telemetry_enabled: bool,
    session_id: Option<&str>,
    sources: &[Option<&ProviderHeaders>],
) -> Option<ProviderHeaders> {
    let mut merged = session_headers(provider, base_url, session_id).unwrap_or_default();
    if telemetry_enabled && let Some(attribution) = default_attribution_headers(provider, base_url) {
        merged.extend(attribution);
    }
    for source in sources.iter().flatten() {
        merged.extend(source.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    (!merged.is_empty()).then_some(merged)
}
