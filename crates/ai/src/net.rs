//! How the HTTP clients this process builds reach the network, as the TypeScript product's HTTP stack does
//! (`configureHttpDispatcher`: undici's `EnvHttpProxyAgent`): `http_proxy`/`HTTP_PROXY` for `http:` URLs,
//! `https_proxy`/`HTTPS_PROXY` (else the HTTP proxy) for `https:` URLs, and `no_proxy`/`NO_PROXY` exceptions.
//!
//! The variables come from an injected lookup (the run's environment, with the `httpProxy` setting applied), never
//! from this process's own environment: a client built from a [`ClientConfig`] does no proxy lookup of its own.

use reqwest::Url;

/// The proxy variables of one environment (`EnvHttpProxyAgent`'s constructor and `#parseNoProxy`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProxyEnv {
    /// `http_proxy ?? HTTP_PROXY`, when set to a non-empty value.
    pub http: Option<String>,
    /// `https_proxy ?? HTTPS_PROXY`, when set to a non-empty value (unset: `http:`'s proxy serves `https:` too).
    pub https: Option<String>,
    /// `no_proxy ?? NO_PROXY ?? ""`.
    pub no_proxy: String,
}

/// One `NO_PROXY` entry: a host name (lower case, no leading `*.`/`.`, no trailing `.`) and a port (0: any).
#[derive(Clone, Debug, PartialEq, Eq)]
struct NoProxyEntry {
    hostname: String,
    port: u16,
}

impl ProxyEnv {
    /// The proxy variables `var` holds. JavaScript's `??` keeps an empty lowercase variable over its uppercase
    /// form, and an empty value means no proxy.
    pub fn from_lookup(var: &dyn Fn(&str) -> Option<String>) -> ProxyEnv {
        let pick = |lower: &str, upper: &str| var(lower).or_else(|| var(upper)).filter(|v| !v.is_empty());
        ProxyEnv {
            http: pick("http_proxy", "HTTP_PROXY"),
            https: pick("https_proxy", "HTTPS_PROXY"),
            no_proxy: var("no_proxy").or_else(|| var("NO_PROXY")).unwrap_or_default(),
        }
    }

    /// Whether any proxy is configured.
    pub fn is_set(&self) -> bool {
        self.http.is_some() || self.https.is_some()
    }

    fn entries(&self) -> Vec<NoProxyEntry> {
        self.no_proxy
            .split([',', ' ', '\t', '\n', '\r', '\u{b}', '\u{c}'])
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                // `[::1]:443`: a bracketed IPv6 address with a port.
                let bracketed_with_port = entry.strip_prefix('[').and_then(|rest| {
                    let (host, port) = rest.split_once("]:")?;
                    let port = digits(port)?;
                    (!host.is_empty()).then(|| (host.to_string(), port))
                });
                let (hostname, port) = bracketed_with_port.unwrap_or_else(|| {
                    let unbracketed = entry
                        .strip_prefix('[')
                        .and_then(|e| e.strip_suffix(']'))
                        .filter(|e| !e.is_empty())
                        .unwrap_or(entry);
                    // Only `host:port` with exactly one colon splits (a bare IPv6 address has several).
                    let split = (unbracketed.matches(':').count() == 1)
                        .then(|| unbracketed.rsplit_once(':'))
                        .flatten()
                        .filter(|(host, _)| !host.is_empty())
                        .and_then(|(host, port)| digits(port).map(|port| (host.to_string(), port)));
                    split.unwrap_or_else(|| (unbracketed.to_string(), 0))
                });
                NoProxyEntry {
                    hostname: normalize_entry_host(&hostname),
                    port,
                }
            })
            .collect()
    }

    /// `#shouldProxy`.
    fn should_proxy(&self, hostname: &str, port: u16) -> bool {
        let entries = self.entries();
        if entries.is_empty() {
            return true;
        }
        if self.no_proxy == "*" {
            return false;
        }
        !entries.iter().any(|entry| {
            (entry.port == 0 || entry.port == port)
                && (hostname == entry.hostname || hostname.ends_with(&format!(".{}", entry.hostname)))
        })
    }

    /// The proxy a request to `url` goes through (`#getProxyAgentForUrl`), or `None` for a direct connection.
    pub fn proxy_for(&self, url: &Url) -> Option<&str> {
        let https = url.scheme() == "https";
        let host = url.host_str().unwrap_or_default();
        let mut hostname = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host)
            .to_lowercase();
        if hostname.len() > 1 && hostname.ends_with('.') {
            hostname.pop();
        }
        // `DEFAULT_PORTS`: only http and https have one.
        let default = match url.scheme() {
            "http" => 80,
            "https" => 443,
            _ => 0,
        };
        let port = url.port().unwrap_or(default);
        if !self.should_proxy(&hostname, port) {
            return None;
        }
        let http = self.http.as_deref();
        if https { self.https.as_deref().or(http) } else { http }
    }
}

/// `Number.parseInt` of a port that the entry's regex (`\d+`) matched.
fn digits(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // A port past 65535 can never match a URL's port.
    Some(
        text.parse::<u32>()
            .ok()
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(u16::MAX),
    )
}

/// `hostname.replace(/^\*?\./, '').replace(/^(.+)\.$/, '$1').toLowerCase()`.
fn normalize_entry_host(hostname: &str) -> String {
    let stripped = hostname
        .strip_prefix("*.")
        .or_else(|| hostname.strip_prefix('.'))
        .unwrap_or(hostname);
    let stripped = match stripped.strip_suffix('.') {
        Some(head) if !head.is_empty() => head,
        _ => stripped,
    };
    stripped.to_lowercase()
}

/// How an HTTP client is built: its proxies, and extra trusted roots.
#[derive(Clone, Debug, Default)]
pub struct ClientConfig {
    /// `Some`: exactly these proxy variables (and nothing from this process's environment). `None`: reqwest's own
    /// lookup, for callers that never had an injected environment.
    pub proxy: Option<ProxyEnv>,
    /// Certificates (PEM) trusted besides the system's: a test's TLS fake.
    pub extra_roots: Vec<Vec<u8>>,
}

impl ClientConfig {
    /// The configuration of a run whose environment is `var`.
    pub fn from_lookup(var: &dyn Fn(&str) -> Option<String>) -> ClientConfig {
        ClientConfig {
            proxy: Some(ProxyEnv::from_lookup(var)),
            extra_roots: Vec::new(),
        }
    }

    /// A reqwest builder with this configuration applied.
    pub fn builder(&self) -> Result<reqwest::ClientBuilder, String> {
        let mut builder = reqwest::Client::builder();
        if let Some(proxy) = &self.proxy {
            builder = builder.no_proxy();
            if proxy.is_set() {
                let proxy = proxy.clone();
                builder = builder.proxy(reqwest::Proxy::custom(move |url| proxy.proxy_for(url).map(proxy_url)));
            }
        }
        for pem in &self.extra_roots {
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(pem).map_err(|e| e.to_string())?);
        }
        Ok(builder)
    }

    /// A built client.
    pub fn client(&self) -> Result<reqwest::Client, String> {
        self.builder()?.build().map_err(|e| e.to_string())
    }
}

/// A proxy URI as reqwest takes it: a value without a scheme is an `http://` proxy.
fn proxy_url(uri: &str) -> String {
    if uri.contains("://") {
        uri.to_string()
    } else {
        format!("http://{uri}")
    }
}

// ---------------------------------------------------------------------------------------------------------------
// utils/node-http-proxy.ts: the proxy a Node SDK client (Bedrock's) is given for its target

/// `UNSUPPORTED_PROXY_PROTOCOL_MESSAGE`.
pub const UNSUPPORTED_PROXY_PROTOCOL_MESSAGE: &str =
    "Unsupported proxy protocol. SOCKS and PAC proxy URLs are not supported; use an HTTP or HTTPS proxy URL.";

/// `getProxyEnv`: the lowercase then uppercase variable, from `env` first, then the process environment.
fn node_proxy_env(key: &str, env: Option<&crate::env::ProviderEnv>) -> String {
    let (lower, upper) = (key.to_lowercase(), key.to_uppercase());
    let from = |name: &str| env.and_then(|e| e.get(name)).filter(|v| !v.is_empty()).cloned();
    from(&lower)
        .or_else(|| from(&upper))
        .or_else(|| crate::env::provider_env_value(&lower, None))
        .or_else(|| crate::env::provider_env_value(&upper, None))
        .unwrap_or_default()
}

fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

/// `Number.parseInt(text, 10)`: the leading digits, if any.
fn parse_int(text: &str) -> Option<u32> {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `parseNoProxyEntry`.
fn node_no_proxy_entry(entry: &str) -> Option<(String, u32)> {
    let trimmed = entry.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with('[')
        && let Some(close) = trimmed.find(']')
    {
        let host = trimmed[1..close].to_string();
        let rest = &trimmed[close + 1..];
        return Some(match rest.strip_prefix(':') {
            Some(port) => (host, parse_int(port).unwrap_or(0)),
            None => (host, 0),
        });
    }
    if trimmed.split(':').count() > 2 {
        return Some((trimmed, 0));
    }
    if let Some(colon) = trimmed.rfind(':')
        && Some(colon) == trimmed.find(':')
        && let Some(port) = parse_int(&trimmed[colon + 1..])
    {
        return Some((trimmed[..colon].to_string(), port));
    }
    Some((trimmed, 0))
}

/// `shouldProxyHostname`.
fn node_should_proxy(hostname: &str, port: u32, env: Option<&crate::env::ProviderEnv>) -> bool {
    let no_proxy = node_proxy_env("no_proxy", env).to_lowercase();
    if no_proxy.is_empty() {
        return true;
    }
    if no_proxy == "*" {
        return false;
    }
    let target = hostname.to_lowercase();
    let target = strip_brackets(&target);
    no_proxy
        .split([',', ' ', '\t', '\n', '\r', '\u{b}', '\u{c}'])
        .all(|entry| {
            let Some((host, entry_port)) = node_no_proxy_entry(entry) else {
                return true;
            };
            if entry_port != 0 && entry_port != port {
                return true;
            }
            let mut domain = strip_brackets(&host);
            if let Some(rest) = domain.strip_prefix("*.") {
                domain = rest;
            } else if let Some(rest) = domain.strip_prefix('.').or_else(|| domain.strip_prefix('*')) {
                domain = rest;
            }
            domain.is_empty() || (target != domain && !target.ends_with(&format!(".{domain}")))
        })
}

/// `resolveHttpProxyUrlForTarget`: the HTTP(S) proxy URL `target` goes through under `env` (then the process
/// environment), `{protocol}_proxy` before `all_proxy`; `None` for a direct connection; an error for a proxy URL
/// that does not parse or is not HTTP(S).
pub fn node_proxy_for_target(target: &str, env: Option<&crate::env::ProviderEnv>) -> Result<Option<String>, String> {
    let Ok(url) = Url::parse(target) else {
        return Ok(None);
    };
    let Some(host) = url.host_str().filter(|h| !h.is_empty()) else {
        return Ok(None);
    };
    let protocol = url.scheme().to_string();
    let default = match protocol.as_str() {
        "ftp" => 21,
        "gopher" => 70,
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        _ => 0,
    };
    let port = url.port().map_or(default, u32::from);
    if !node_should_proxy(strip_brackets(host), port, env) {
        return Ok(None);
    }
    let mut proxy = node_proxy_env(&format!("{protocol}_proxy"), env);
    if proxy.is_empty() {
        proxy = node_proxy_env("all_proxy", env);
    }
    if proxy.is_empty() {
        return Ok(None);
    }
    if !proxy.contains("://") {
        proxy = format!("{protocol}://{proxy}");
    }
    // `new URL()`'s TypeError message.
    let parsed = Url::parse(&proxy).map_err(|_| {
        format!(
            "Invalid proxy URL {}: Invalid URL",
            serde_json::to_string(&proxy).unwrap_or_default()
        )
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(format!("{UNSUPPORTED_PROXY_PROTOCOL_MESSAGE} Got {}:", parsed.scheme()));
    }
    Ok(Some(proxy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> ProxyEnv {
        let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        ProxyEnv::from_lookup(&move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
    }

    #[test]
    fn ai_net_lowercase_variables_win_even_when_empty() {
        let proxy = env(&[("http_proxy", ""), ("HTTP_PROXY", "http://upper:1")]);
        assert_eq!(proxy.http, None);
        let proxy = env(&[("https_proxy", "http://lower:2"), ("HTTPS_PROXY", "http://upper:3")]);
        assert_eq!(proxy.https.as_deref(), Some("http://lower:2"));
    }
}
