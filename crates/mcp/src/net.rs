//! How this crate's HTTP clients (the streamable HTTP transport and its GET stream, OAuth discovery, registration,
//! token exchange and refresh) reach the network: through the proxies of an injected environment
//! ([`ai::net::ProxyEnv`]: undici's `EnvHttpProxyAgent` semantics, as the rest of the product), never through
//! reqwest's own lookup of this process's environment.
//!
//! `None` keeps reqwest's own lookup, for callers that never had an injected environment.

use ai::net::ProxyEnv;
use reqwest::blocking::{Client, ClientBuilder};
use std::time::Duration;

/// A proxy URI as reqwest takes it: a value without a scheme is an `http://` proxy (undici's `new URL` of it fails,
/// and Pi documents `host:port` values as HTTP proxies).
fn proxy_url(uri: &str) -> String {
    if uri.contains("://") {
        uri.to_string()
    } else {
        format!("http://{uri}")
    }
}

/// A blocking client builder whose proxies are exactly `proxy`'s (`None`: reqwest's own lookup).
pub fn builder(proxy: Option<&ProxyEnv>) -> ClientBuilder {
    let mut builder = Client::builder();
    if let Some(proxy) = proxy {
        builder = builder.no_proxy();
        if proxy.is_set() {
            let proxy = proxy.clone();
            builder = builder.proxy(reqwest::Proxy::custom(move |url| proxy.proxy_for(url).map(proxy_url)));
        }
    }
    builder
}

/// [`builder`] with an optional request timeout, built.
pub fn client(proxy: Option<&ProxyEnv>, timeout: Option<Duration>) -> reqwest::Result<Client> {
    let mut builder = builder(proxy);
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder.build()
}
