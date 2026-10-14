//! Sign-in and the connections' auth (`extensions/mcp/oauth.ts`): `/mcp login` runs the authorization code flow
//! against a loopback callback (or a redirect URL the user pastes); a connection's [`OAuthAuth`] sends the stored
//! access token, refreshes it when it is about to expire or after a 401, and otherwise reports that sign-in is needed.

use super::callback::{Callback, CallbackServer, PageRenderer};
use super::flow::{Fetcher, FlowOptions, FlowResult, Provider, StateStore, authorize, step_up_scope};
use super::store::{ServerStore, now_ms};
use super::{Challenge, OAuthError, parse_www_authenticate};
use crate::CancelToken;
use crate::http::{AuthProvider, Unauthorized, http_failure};
use crate::jsonrpc::Error;
use serde_json::Value;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use url::Url;

const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PATH: &str = "/callback";
/// Redirect URI for refreshes when none is stored; refreshing never redirects the user.
const FALLBACK_REDIRECT_URL: &str = "http://127.0.0.1/callback";
/// Access tokens this close to expiry are refreshed before they are sent.
const REFRESH_SKEW_MS: i64 = 30_000;
/// Bounds each request of a refresh.
const REFRESH_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// `APP_NAME`: the client name of dynamic client registrations.
pub const APP_NAME: &str = "ultron";

/// `McpOAuthSettings` (the server's `oauth` config, the client secret already resolved).
#[derive(Clone, Debug, Default)]
pub struct OAuthSettings {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub callback_port: Option<u16>,
    pub callback_url: Option<String>,
    pub scope: Option<String>,
    pub client_name: Option<String>,
    pub auth_server_metadata_url: Option<String>,
}

/// Where the loopback callback listens and the redirect URI it serves.
struct CallbackSettings {
    host: String,
    redirect_host: String,
    port: Option<u16>,
    path: String,
    /// The exact redirect URI, when the port is fixed.
    fixed_redirect_url: Option<String>,
}

fn callback_settings(settings: &OAuthSettings) -> CallbackSettings {
    let configured = settings.callback_url.clone();
    let mut url = configured
        .as_deref()
        .and_then(|u| Url::parse(u).ok())
        .unwrap_or_else(|| Url::parse(&format!("http://{CALLBACK_HOST}{CALLBACK_PATH}")).expect("valid URL"));
    let address = url
        .host_str()
        .unwrap_or(CALLBACK_HOST)
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let explicit_port = url.port();
    let port = explicit_port.or(settings.callback_port);
    // A configured URI with a port is sent exactly as written, since servers compare it as a string.
    let fixed_redirect_url = match (explicit_port, port) {
        (Some(_), _) => configured,
        (None, Some(port)) => {
            let _ = url.set_port(Some(port));
            Some(url.to_string())
        }
        _ => None,
    };
    CallbackSettings {
        host: if address == "localhost" || address == CALLBACK_HOST {
            ai::oauth_flow::oauth_callback_host()
        } else {
            address.clone()
        },
        redirect_host: address,
        port,
        path: url.path().to_string(),
        fixed_redirect_url,
    }
}

/// Scopes of all lists, each once.
fn merge_scopes(scopes: &[Option<&str>]) -> Option<String> {
    let mut merged: Vec<&str> = Vec::new();
    for scope in scopes.iter().flatten().flat_map(|s| s.split_whitespace()) {
        if !merged.contains(&scope) {
            merged.push(scope);
        }
    }
    (!merged.is_empty()).then(|| merged.join(" "))
}

fn registered_redirect_urls(client: Option<&Value>) -> Vec<String> {
    client
        .and_then(|c| c["redirect_uris"].as_array())
        .map(|uris| uris.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

fn provider<'a>(
    server_url: &str,
    store: &'a dyn StateStore,
    settings: &OAuthSettings,
    redirect_url: &str,
    on_redirect: Box<dyn Fn(&str) + 'a>,
) -> Provider<'a> {
    Provider::new(
        server_url,
        redirect_url,
        settings.client_name.as_deref().unwrap_or(APP_NAME),
        settings.client_id.as_deref(),
        settings.client_secret.as_deref(),
        store,
        on_redirect,
    )
}

/// How a sign-in reaches the user (`McpSignInPrompt`).
pub trait SignInPrompt: Sync {
    /// Show the authorization URL to the user and open it in a browser.
    fn show_authorization_url(&self, url: &str);
    /// Ask for the redirect URL from the browser's address bar, for when the browser cannot reach the loopback
    /// callback. Cancelled once the callback arrives; `None` or blank when the user cancels.
    fn prompt_for_redirect_url(&self, cancel: &CancelToken) -> Option<String>;
}

/// `responseFromRedirectUrl`.
fn response_from_redirect_url(input: &str, state: &str) -> Result<(String, Option<String>), OAuthError> {
    let url = Url::parse(input.trim())
        .map_err(|_| OAuthError::Other("Expected the full redirect URL from the browser address bar".into()))?;
    let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    if let Some(error) = query.get("error") {
        return Err(OAuthError::Other(
            query.get("error_description").unwrap_or(error).clone(),
        ));
    }
    if query.get("state").map(String::as_str) != Some(state) {
        return Err(OAuthError::Other(
            "The redirect URL belongs to a different sign-in".into(),
        ));
    }
    let code = query
        .get("code")
        .filter(|c| !c.is_empty())
        .ok_or_else(|| OAuthError::Other("The redirect URL does not contain an authorization code".into()))?;
    Ok((code.clone(), query.get("iss").cloned()))
}

/// The browser's callback or a pasted redirect URL, whichever comes first.
fn wait_for_response(
    callback: &CallbackServer,
    state: &str,
    prompt: &dyn SignInPrompt,
) -> Result<(String, Option<String>), OAuthError> {
    let from_browser = callback.wait_for_callback(state)?;
    let cancel = CancelToken::new();
    let (tx, rx) = mpsc::channel::<Result<(String, Option<String>), OAuthError>>();
    std::thread::scope(|scope| {
        let browser_tx = tx.clone();
        let browser = scope.spawn(move || {
            let outcome = from_browser
                .recv()
                .unwrap_or_else(|_| Err(OAuthError::Other("OAuth callback server closed".into())));
            let _ = browser_tx.send(outcome.map(|Callback { code, iss, .. }| (code, iss)));
        });
        let user_cancel = cancel.clone();
        let user = scope.spawn(move || {
            let answer = prompt.prompt_for_redirect_url(&user_cancel);
            if user_cancel.is_cancelled() {
                return;
            }
            let outcome = match answer.filter(|a| !a.trim().is_empty()) {
                None => Err(OAuthError::Cancelled),
                Some(answer) => response_from_redirect_url(&answer, state),
            };
            let _ = tx.send(outcome);
        });
        let first = rx
            .recv()
            .unwrap_or_else(|_| Err(OAuthError::Other("OAuth callback server closed".into())));
        // The losing side ends: the prompt is cancelled, the callback server closes.
        cancel.cancel(None);
        callback.close();
        let _ = user.join();
        let _ = browser.join();
        first
    })
}

/// Listens on `port`, or on a free port when it is taken and not `required`.
fn listen_for_callback(
    settings: &CallbackSettings,
    port: Option<u16>,
    required: bool,
) -> Result<CallbackServer, OAuthError> {
    let render: PageRenderer = Arc::new(|page| match page {
        None => ai::oauth_flow::oauth_success_html("Signed in to the MCP server. You may now close this page."),
        Some((message, details)) => ai::oauth_flow::oauth_error_html(message, details),
    });
    let listen = |port: u16| {
        CallbackServer::listen(
            &settings.host,
            &settings.redirect_host,
            port,
            &settings.path,
            render.clone(),
        )
    };
    match listen(port.unwrap_or(0)) {
        Ok(server) => Ok(server),
        Err(error) if required || port.is_none() => Err(error),
        Err(_) => listen(0),
    }
}

/// `signInMcpServer`: the stored refresh token when it still works, else the browser flow. Tokens go to `store`.
pub fn sign_in(
    server_url: &str,
    store: &ServerStore,
    settings: &OAuthSettings,
    challenge: Option<&Challenge>,
    prompt: &dyn SignInPrompt,
) -> Result<(), OAuthError> {
    let stored = store.load()?;
    let step_up = challenge.and_then(|c| c.error.as_deref()) == Some("insufficient_scope");
    let callback_options = callback_settings(settings);
    // Reuse the port of the registered redirect URI so the registered client stays valid.
    let registered = registered_redirect_urls(stored.as_ref().and_then(|s| s.get("clientInformation")));
    let preferred_port = callback_options.port.or_else(|| {
        registered
            .first()
            .and_then(|u| Url::parse(u).ok())
            .and_then(|u| u.port())
    });
    let callback = listen_for_callback(&callback_options, preferred_port, callback_options.port.is_some())?;
    let redirect_url = callback_options
        .fixed_redirect_url
        .clone()
        .unwrap_or_else(|| callback.redirect_url.clone());
    let result = (|| {
        if let Some(Value::Object(stored)) = &stored {
            let mut next = stored.clone();
            // Every sign-in gets a fresh `state` parameter.
            next.shift_remove("oauthState");
            // A registered client cannot use another redirect URI, and its tokens belong to it.
            if settings.client_id.is_none() && !registered.contains(&redirect_url) {
                next.shift_remove("clientInformation");
                next.shift_remove("tokens");
                next.shift_remove("tokensExpireAt");
            }
            store.save(&Value::Object(next))?;
        }
        let authorization_url: Mutex<Option<String>> = Mutex::new(None);
        let provider = provider(
            server_url,
            store,
            settings,
            &redirect_url,
            Box::new(|url: &str| {
                *authorization_url
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(url.to_string());
            }),
        );
        let granted = stored.as_ref().and_then(|s| s["tokens"]["scope"].as_str());
        let challenged = challenge.and_then(|c| c.scope.as_deref());
        // A server asking for more scope gets it on top of the configured scope and of the scope granted so far.
        let scope = if step_up {
            merge_scopes(&[settings.scope.as_deref(), step_up_scope(granted, challenged).as_deref()])
        } else {
            merge_scopes(&[settings.scope.as_deref(), challenged])
        };
        let flow = FlowOptions {
            server_url: server_url.to_string(),
            resource_metadata_url: challenge.and_then(|c| c.resource_metadata_url.clone()),
            authorization_server_metadata_url: settings.auth_server_metadata_url.clone(),
            scope,
            skip_refresh: step_up,
            ..FlowOptions::default()
        };
        if authorize(&provider, &flow)? == FlowResult::Authorized {
            return Ok(());
        }
        let url = authorization_url
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| OAuthError::Other("OAuth flow did not produce an authorization URL".into()))?;
        let state = provider.state()?;
        prompt.show_authorization_url(&url);
        let (code, iss) = wait_for_response(&callback, &state, prompt)?;
        authorize(
            &provider,
            &FlowOptions {
                authorization_code: Some(code),
                iss,
                skip_refresh: false,
                ..flow
            },
        )
        .map(drop)
    })();
    callback.close();
    result
}

/// An OAuth failure as the transport's error: sign-in needed is `McpOAuthAuthorizationRequiredError`.
fn transport_error(error: OAuthError) -> Error {
    match error {
        OAuthError::AuthorizationRequired => http_failure(
            "McpOAuthAuthorizationRequiredError",
            401,
            OAuthError::AuthorizationRequired.to_string(),
            "",
        ),
        OAuthError::Network(message) => http_failure("TypeError", 0, message, ""),
        other => Error::Other(other.to_string()),
    }
}

/// The auth of a connection to an OAuth server (`createMcpAuthProvider`): the stored access token, refreshed when it
/// is about to expire or after a 401 (once per stale token across this process and, through the store's refresh
/// lock, across processes); a 403 asking for more scope, or a refresh that is not possible, needs sign-in.
pub struct OAuthAuth {
    server_url: String,
    store: ServerStore,
    /// Read only when a refresh is needed, so a secret that fails to resolve fails the refresh, not the connection.
    settings: Box<dyn Fn() -> Result<OAuthSettings, String> + Send + Sync>,
    challenge: Arc<Mutex<Option<Challenge>>>,
}

impl OAuthAuth {
    pub fn new(
        server_url: &str,
        store: ServerStore,
        settings: Box<dyn Fn() -> Result<OAuthSettings, String> + Send + Sync>,
        challenge: Arc<Mutex<Option<Challenge>>>,
    ) -> Self {
        Self {
            server_url: server_url.to_string(),
            store,
            settings,
            challenge,
        }
    }

    /// Replace `stale`, the access token that expired or was rejected.
    fn refresh(&self, stale: Option<&str>, challenge: Option<&Challenge>) -> Result<(), OAuthError> {
        self.store.with_refresh_lock(|| {
            let state = self.store.load()?;
            let current = state.as_ref().and_then(|s| s["tokens"]["access_token"].as_str());
            if current != stale {
                return Ok(());
            }
            if state
                .as_ref()
                .and_then(|s| s["tokens"]["refresh_token"].as_str())
                .is_none()
            {
                return Err(OAuthError::AuthorizationRequired);
            }
            let settings = (self.settings)().map_err(OAuthError::Other)?;
            let redirect_url = callback_settings(&settings)
                .fixed_redirect_url
                .or_else(|| {
                    registered_redirect_urls(state.as_ref().and_then(|s| s.get("clientInformation")))
                        .into_iter()
                        .next()
                })
                .unwrap_or_else(|| FALLBACK_REDIRECT_URL.into());
            let provider = provider(
                &self.server_url,
                &self.store,
                &settings,
                &redirect_url,
                Box::new(|_| {}),
            );
            let result = authorize(
                &provider,
                &FlowOptions {
                    server_url: self.server_url.clone(),
                    resource_metadata_url: challenge.and_then(|c| c.resource_metadata_url.clone()),
                    authorization_server_metadata_url: settings.auth_server_metadata_url.clone(),
                    scope: challenge.and_then(|c| c.scope.clone()),
                    fetch: Fetcher {
                        timeout: Some(REFRESH_REQUEST_TIMEOUT),
                    },
                    ..FlowOptions::default()
                },
            )?;
            if result == FlowResult::Redirect {
                return Err(OAuthError::AuthorizationRequired);
            }
            Ok(())
        })
    }
}

impl AuthProvider for OAuthAuth {
    fn token(&self) -> Result<Option<String>, Error> {
        let token_of = |state: &Option<Value>| {
            state
                .as_ref()
                .and_then(|s| s["tokens"]["access_token"].as_str())
                .map(String::from)
        };
        let state = self.store.load().map_err(transport_error)?;
        let token = token_of(&state);
        let expired = state
            .as_ref()
            .and_then(|s| s["tokensExpireAt"].as_i64())
            .is_some_and(|at| at - REFRESH_SKEW_MS <= now_ms());
        let refreshable = state
            .as_ref()
            .and_then(|s| s["tokens"]["refresh_token"].as_str())
            .is_some();
        if !expired || !refreshable {
            return Ok(token);
        }
        // Failures fall through: the request goes out with the old token and a 401 decides what happens.
        let _ = self.refresh(token.as_deref(), None);
        Ok(token_of(&self.store.load().map_err(transport_error)?))
    }

    fn on_unauthorized(&self, context: &Unauthorized) -> Result<(), Error> {
        let challenge = parse_www_authenticate(context.challenge.as_deref());
        *self.challenge.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(challenge.clone());
        // A refresh keeps the granted scope, so more scope needs a new sign-in.
        if challenge.error.as_deref() == Some("insufficient_scope") {
            return Err(transport_error(OAuthError::AuthorizationRequired));
        }
        self.refresh(context.token.as_deref(), Some(&challenge))
            .map_err(transport_error)
    }

    fn refreshes(&self) -> bool {
        true
    }
}

/// The auth of a server with `auth.provider`: the provider's current key, read for every request.
pub struct ProviderAuth {
    pub provider: String,
    pub token: crate::connection::ProviderToken,
}

impl AuthProvider for ProviderAuth {
    fn token(&self) -> Result<Option<String>, Error> {
        Ok((self.token)(&self.provider))
    }
}

/// Where this process runs relative to the user's browser (`browserReach`).
pub fn browser_reach(var: &dyn Fn(&str) -> Option<String>, exists: &dyn Fn(&str) -> bool) -> &'static str {
    let set = |key: &str| var(key).is_some_and(|v| !v.trim().is_empty());
    if set("ULTRON_OAUTH_CALLBACK_HOST") || set("PI_OAUTH_CALLBACK_HOST") {
        return "local";
    }
    let container = set("container")
        || set("KUBERNETES_SERVICE_HOST")
        || exists("/.dockerenv")
        || exists("/run/.containerenv")
        || std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|cgroup| {
            ["docker", "containerd", "kubepods", "libpod", "lxc"]
                .iter()
                .any(|marker| cgroup.contains(marker))
        });
    if container {
        return "container";
    }
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        "ssh"
    } else {
        "local"
    }
}

const UNREACHABLE_CALLBACK_STEP: &str = "copy that page's full address from the address bar and paste it here.";

/// `unreachableCallbackHint`.
pub fn unreachable_callback_hint(reach: &str) -> String {
    if reach == "local" {
        return format!(
            "If the browser ends on a page that can't be reached (localhost refused to connect), {UNREACHABLE_CALLBACK_STEP}"
        );
    }
    let place = if reach == "container" {
        "in a container"
    } else {
        "over SSH"
    };
    format!(
        "Ultron is running {place}, so the browser on your computer may not reach this login's callback. If it ends on a page that can't be reached (localhost refused to connect) after you sign in, that is expected: {UNREACHABLE_CALLBACK_STEP}"
    )
}
