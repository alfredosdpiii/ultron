//! Stored model catalogs (`models-store.json`) and their refresh:
//! - `withRemoteCatalog` (coding-agent/src/core/remote-catalog-provider.ts): a built-in provider's pi.dev catalog
//!   overlay, restored only when it is newer than the bundled data, and refreshed from
//!   `<catalog>/api/models/providers/<id>` (etag revalidation, 304, 404/501, transient failures, retries).
//! - `radiusProvider` (ai/src/providers/radius.ts, radius-config.ts): a Radius provider's stored catalog, the legacy
//!   OAuth credential's `gatewayConfig`, and `<gateway>/v1/config`.
//!
//! The registry restores catalogs offline; `ultron update --models` (session-core `model_refresh`) refreshes them.
//!
//! Evidence: `fixtures/generate-model-refresh.mjs` runs the TypeScript runtime against loopback stand-ins for pi.dev
//! and a Radius gateway; `crates/session-core/tests/model_refresh.rs` replays it.

use crate::http::{HeaderMap, HttpRequest, Transport};
use crate::jsutil::Utf8Stream;
use crate::types::Model;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;
use std::time::Duration;

/// `getBuiltinModelDataGeneratedAt`: when the bundled catalog was generated (ms), from `data/catalog-meta.json`.
pub fn builtin_generated_at() -> Option<i64> {
    static AT: LazyLock<Option<i64>> = LazyLock::new(|| {
        let meta: Value = serde_json::from_str(include_str!("../data/catalog-meta.json")).ok()?;
        parse_iso_ms(meta.get("generatedAt")?.as_str()?)
    });
    *AT
}

/// `REMOTE_CATALOG_REFRESH_INTERVAL_MS`.
pub const REMOTE_CATALOG_REFRESH_INTERVAL_MS: i64 = 4 * 60 * 60 * 1000;
/// `REMOTE_CATALOG_ATTEMPT_TIMEOUT_MS`.
const REMOTE_CATALOG_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(4);
/// The pi.dev catalog service (`DEFAULT_CATALOG_BASE_URL`).
pub const DEFAULT_CATALOG_BASE_URL: &str = "https://pi.dev";

/// `getPiUserAgent(VERSION)`, the user agent of management requests: `pi/<version> (<platform>; <runtime>; <arch>)`
/// with Node's platform and arch names. The runtime is `rust` (TypeScript reports `node/<version>`).
pub fn management_user_agent(version: &str) -> String {
    let platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    format!("pi/{version} ({platform}; rust; {arch})")
}

/// `mergeModels`: `dynamic` replaces baseline models with the same id in place and adds the rest.
pub fn merge_models(mut models: Vec<Model>, dynamic: Vec<Model>) -> Vec<Model> {
    for model in dynamic {
        match models.iter().position(|m| m.id == model.id) {
            Some(index) => models[index] = model,
            None => models.push(model),
        }
    }
    models
}

fn models_of(values: &[Value], provider: &str) -> Vec<Model> {
    values
        .iter()
        .filter(|m| m.get("provider").and_then(Value::as_str) == Some(provider))
        .filter_map(|m| serde_json::from_value(m.clone()).ok())
        .collect()
}

fn entry_models(entry: &Value) -> &[Value] {
    entry.get("models").and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// `remoteModels` filtered to the provider: a stored pi.dev overlay counts only when its `lastModified` is newer
/// than the bundled data.
pub fn remote_overlay(provider: &str, entry: Option<&Value>, generated_at: Option<i64>) -> Vec<Model> {
    let Some(entry) = entry else {
        return Vec::new();
    };
    if let Some(generated) = generated_at {
        match entry.get("lastModified").and_then(Value::as_f64) {
            Some(modified) if modified > generated as f64 => {}
            _ => return Vec::new(),
        }
    }
    models_of(entry_models(entry), provider)
}

/// A Radius provider's stored catalog (applied as stored).
pub fn radius_stored(provider: &str, entry: Option<&Value>) -> Vec<Model> {
    entry.map_or_else(Vec::new, |entry| models_of(entry_models(entry), provider))
}

/// `isRadiusGatewayModel`.
fn is_gateway_model(value: &Value) -> bool {
    let Some(model) = value.as_object() else {
        return false;
    };
    model.get("id").is_some_and(Value::is_string)
        && model.get("name").is_some_and(Value::is_string)
        && model.get("reasoning").is_some_and(Value::is_boolean)
        && model.get("input").is_some_and(Value::is_array)
        && model.get("cost").is_some_and(Value::is_object)
        && model.get("contextWindow").is_some_and(Value::is_number)
        && model.get("maxTokens").is_some_and(Value::is_number)
}

/// `getRadiusModelsFromConfig` over `sanitizeRadiusGatewayConfig`: the config's valid models as `pi-messages`
/// models of `provider` on the config's base URL; `None` when the config is not one.
pub fn radius_models_from_config(provider: &str, config: &Value) -> Option<Vec<Value>> {
    let base_url = config.get("baseUrl")?.as_str()?;
    let models = config.get("models")?.as_array()?;
    Some(
        models
            .iter()
            .filter(|m| is_gateway_model(m))
            .map(|m| {
                let mut model = m.as_object().cloned().unwrap_or_default();
                model.insert("api".into(), json!("pi-messages"));
                model.insert("provider".into(), json!(provider));
                model.insert("baseUrl".into(), json!(base_url));
                Value::Object(model)
            })
            .collect(),
    )
}

/// A response read whole.
pub struct Fetched {
    pub status: u16,
    pub headers: HeaderMap,
    pub text: String,
}

impl Fetched {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn fetch(transport: &dyn Transport, request: HttpRequest, timeout: Option<Duration>) -> Result<Fetched, String> {
    let work = async {
        let mut response = transport.send(request).await?;
        let mut decoder = Utf8Stream::default();
        let mut text = String::new();
        while let Some(chunk) = response.body.next_chunk().await? {
            text.push_str(&decoder.push(&chunk));
        }
        text.push_str(&decoder.finish());
        Ok(Fetched {
            status: response.status,
            headers: response.headers,
            text,
        })
    };
    match timeout {
        Some(limit) => tokio::time::timeout(limit, work)
            .await
            .map_err(|_| "The operation was aborted due to timeout".to_string())?,
        None => work.await,
    }
}

/// `fetchWithRetry` with its defaults: up to two retries after transport failures and transient statuses (408,
/// 425, 429, 500, 502, 503, 504), each attempt bounded by `attempt_timeout`.
async fn fetch_with_retry(
    transport: &dyn Transport,
    request: HttpRequest,
    attempt_timeout: Option<Duration>,
) -> Result<Fetched, String> {
    const RETRYABLE: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];
    const MAX_RETRIES: usize = 2;
    let mut attempt = 0;
    loop {
        match fetch(transport, request.clone(), attempt_timeout).await {
            Ok(response) if RETRYABLE.contains(&response.status) && attempt < MAX_RETRIES => {}
            Ok(response) => return Ok(response),
            Err(error) if attempt >= MAX_RETRIES => return Err(error),
            Err(_) => {}
        }
        attempt += 1;
    }
}

/// `truncateHttpBody`.
fn truncate_body(body: &str) -> String {
    let trimmed = body.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() > 512 {
        format!("{}…", chars[..512].iter().collect::<String>())
    } else {
        trimmed.to_string()
    }
}

/// `loadRadiusGatewayConfig`: `<gateway>/v1/config` with the key as a bearer token, sanitized.
pub async fn load_radius_gateway_config(
    gateway: &str,
    api_key: Option<&str>,
    transport: &dyn Transport,
) -> Result<Value, String> {
    let mut headers = HeaderMap::new();
    headers.insert("accept".into(), "application/json".into());
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        headers.insert("authorization".into(), format!("Bearer {key}"));
    }
    let url = reqwest::Url::parse(gateway)
        .and_then(|base| base.join("/v1/config"))
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("{gateway}/v1/config"));
    let response = fetch(
        transport,
        HttpRequest {
            method: "GET".into(),
            url,
            headers,
            body: String::new(),
        },
        None,
    )
    .await?;
    if !(200..300).contains(&response.status) {
        return Err(format!(
            "Could not load Radius config from {gateway}: {}: {}",
            response.status,
            truncate_body(&response.text)
        ));
    }
    let config: Value = serde_json::from_str(&response.text).map_err(|e| e.to_string())?;
    let sanitized = radius_models_from_config("", &config).is_some();
    if !sanitized {
        return Err(format!("Invalid Radius config from {gateway}"));
    }
    Ok(config)
}

/// The outcome of one pi.dev catalog refresh: the entry to persist (if any) and the error to report (if any).
#[derive(Debug, Default, PartialEq)]
pub struct CatalogRefresh {
    pub persist: Option<Value>,
    pub error: Option<String>,
}

/// `withRemoteCatalog.refreshModels` with the network allowed: revalidate or fetch `provider`'s catalog.
pub async fn refresh_remote_catalog(
    provider: &str,
    stored: Option<&Value>,
    catalog_base_url: &str,
    force: bool,
    user_agent: &str,
    now: &(dyn Fn() -> i64 + Sync),
    transport: &dyn Transport,
) -> CatalogRefresh {
    let number = |key: &str| stored.and_then(|s| s.get(key)).and_then(Value::as_f64);
    if !force
        && let (Some(checked), Some(_)) = (number("checkedAt"), number("lastModified"))
        && (now() as f64) - checked < REMOTE_CATALOG_REFRESH_INTERVAL_MS as f64
    {
        return CatalogRefresh::default();
    }
    let validator = stored
        .filter(|s| !entry_models(s).is_empty())
        .and_then(|s| s.get("etag"))
        .and_then(Value::as_str);
    let mut headers = HeaderMap::new();
    headers.insert("accept".into(), "application/json".into());
    headers.insert("user-agent".into(), user_agent.to_string());
    if let Some(etag) = validator {
        headers.insert("if-none-match".into(), etag.to_string());
    }
    let encoded: String = provider
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect();
    let url = reqwest::Url::parse(catalog_base_url)
        .and_then(|base| base.join(&format!("/api/models/providers/{encoded}")))
        .map(|u| u.to_string())
        .unwrap_or_default();
    let request = HttpRequest {
        method: "GET".into(),
        url,
        headers,
        body: String::new(),
    };
    let response = match fetch_with_retry(transport, request, Some(REMOTE_CATALOG_ATTEMPT_TIMEOUT)).await {
        Ok(response) => response,
        Err(error) => {
            return CatalogRefresh {
                persist: None,
                error: Some(error),
            };
        }
    };
    let checked_at = json!(now());
    let base = || -> Map<String, Value> {
        stored
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| Map::from_iter([("models".to_string(), json!([]))]))
    };
    if response.status == 304
        && let Some(stored) = stored.and_then(Value::as_object)
    {
        let mut entry = stored.clone();
        entry.insert("checkedAt".into(), checked_at);
        return CatalogRefresh {
            persist: Some(Value::Object(entry)),
            error: None,
        };
    }
    if response.status == 404 || response.status == 501 {
        let mut entry = base();
        entry.insert("checkedAt".into(), checked_at);
        entry.insert("lastModified".into(), json!(0));
        entry.shift_remove("etag");
        return CatalogRefresh {
            persist: Some(Value::Object(entry)),
            error: None,
        };
    }
    if !(200..300).contains(&response.status) {
        let mut entry = base();
        entry.insert("checkedAt".into(), checked_at);
        return CatalogRefresh {
            persist: Some(Value::Object(entry)),
            error: Some(format!(
                "Model catalog request failed for {provider}: {}",
                response.status
            )),
        };
    }
    let models = match serde_json::from_str::<Value>(&response.text)
        .map_err(|e| e.to_string())
        .and_then(|body| parse_catalog(provider, &body))
    {
        Ok(models) => models,
        Err(error) => {
            return CatalogRefresh {
                persist: None,
                error: Some(error),
            };
        }
    };
    let last_modified = response
        .header("last-modified")
        .and_then(parse_http_date_ms)
        .unwrap_or(0);
    let mut entry = Map::new();
    entry.insert("models".into(), Value::Array(models));
    entry.insert("checkedAt".into(), checked_at);
    entry.insert("lastModified".into(), json!(last_modified));
    if let Some(etag) = response.header("etag") {
        entry.insert("etag".into(), json!(etag));
    }
    CatalogRefresh {
        persist: Some(Value::Object(entry)),
        error: None,
    }
}

/// `parseCatalog`: an array, `{ models: [...] }`, or an id-keyed object; entries with an `id`, as `provider`'s.
fn parse_catalog(provider: &str, value: &Value) -> Result<Vec<Value>, String> {
    let entries: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(map) => match map.get("models") {
            Some(Value::Array(items)) => items.iter().collect(),
            _ => map.values().collect(),
        },
        _ => return Err(format!("Invalid model catalog for provider \"{provider}\"")),
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| {
            let mut model = entry.as_object()?.clone();
            if !model.contains_key("id") {
                return None;
            }
            model.insert("provider".into(), json!(provider));
            Some(Value::Object(model))
        })
        .collect())
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `Date.parse` of an ISO timestamp (`2026-10-07T19:43:49.517Z`).
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    let (date, time) = text.trim_end_matches('Z').split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, millis) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (t.next()??, t.next()??, t.next()??);
    let ms: i64 = format!("{millis:0<3}").get(..3)?.parse().ok()?;
    Some(((days_from_civil(year, month, day) * 24 + h) * 60 + m) * 60_000 + s * 1000 + ms)
}

/// `Date.parse` of an HTTP date (`Thu, 08 Oct 2026 19:43:49 GMT`).
pub fn parse_http_date_ms(text: &str) -> Option<i64> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let [_, day, month, year, time, _] = parts.as_slice() else {
        return None;
    };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(month))? as i64 + 1;
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (t.next()??, t.next()??, t.next()??);
    let days = days_from_civil(year.parse().ok()?, month, day.parse().ok()?);
    Some(((days * 24 + h) * 60 + m) * 60_000 + s * 1000)
}
