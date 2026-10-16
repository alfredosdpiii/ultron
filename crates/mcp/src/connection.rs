//! One configured server's connection (`extensions/mcp/runtime.ts` `McpServerConnection`): connected on first use,
//! shared by every call, reconnected lazily when a call finds it gone (a stdio server that exited), and closed with
//! the session. Concurrent callers share one connection attempt. The tools, resources and instructions are what
//! the server listed at the last connect or change notification.
//!
//! An OAuth server sends the stored access token (`mcp-auth.json`, crate::oauth) and refreshes it after a 401; when
//! no token works it reads as `needs-auth` until `/mcp login` or `ultron mcp login` signs in (crate::oauth::signin).
//! A server with `auth.provider` sends that `/login` provider's current key.

use crate::client::{Client, ClientOptions, ConnState, RequestOptions};
use crate::config::ServerEntry;
use crate::http::{AuthProvider, HttpTransport, http_error};
use crate::jsonrpc::{Error, METHOD_NOT_FOUND};
use crate::log::ServerLog;
use crate::oauth::Challenge;
use crate::oauth::signin::{OAuthAuth, OAuthSettings, ProviderAuth};
use crate::oauth::store::CredentialStore;
use crate::results::is_mcp_app_resource;
use crate::runtime::{CONNECT_RETRY_DELAYS_MS, TransportSpec, is_transient_error, transport_spec};
use crate::stdio::StdioTransport;
use crate::{CancelToken, Transport};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Duration;

const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;
const STDERR_TAIL_CHARS: usize = 2_000;

/// Receives values that must be masked in cell output (resolved env values, header credentials).
pub type SecretSink = Arc<dyn Fn(&[String]) + Send + Sync>;
/// The current token of a `/login` provider, for servers with `auth.provider`.
pub type ProviderToken = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Builds the transport of a server in place of the default stdio and HTTP ones (`createTransport`).
pub type TransportFactory = Arc<dyn Fn(&ServerEntry) -> Result<Arc<dyn Transport>, Error> + Send + Sync>;

/// What every connection of a session shares.
#[derive(Clone, Default)]
pub struct ConnectionEnv {
    /// The session's working directory: relative stdio `cwd`s resolve against it; it is the client's root.
    pub cwd: PathBuf,
    pub home: PathBuf,
    /// Overlays the process environment when `$VAR` config values resolve (tests).
    pub env_overlay: HashMap<String, String>,
    /// `clientInfo.version` of the initialize request.
    pub client_version: String,
    pub log: Option<Arc<ServerLog>>,
    pub secrets: Option<SecretSink>,
    pub provider_token: Option<ProviderToken>,
    /// Default: stdio and streamable HTTP transports.
    pub transport: Option<TransportFactory>,
    /// OAuth credentials (`mcp-auth.json`); without them OAuth servers send no token.
    pub credentials: Option<Arc<CredentialStore>>,
}

/// HTTP servers authenticate with OAuth unless the config supplies an `Authorization` header or `auth`.
fn oauth_url_of(entry: &ServerEntry) -> Option<String> {
    let config = &entry.config;
    let url = config.get("url").and_then(Value::as_str)?;
    if config.get("auth").is_some_and(|a| !a.is_null()) {
        return None;
    }
    let keyed = config
        .get("headers")
        .and_then(Value::as_object)
        .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("authorization")));
    (!keyed).then(|| url.to_string())
}

fn oauth_settings_of(entry: &ServerEntry, env: &HashMap<String, String>) -> Result<OAuthSettings, String> {
    let Some(oauth) = entry
        .config
        .get("url")
        .and(entry.config.get("oauth"))
        .and_then(Value::as_object)
    else {
        return Ok(OAuthSettings::default());
    };
    let text = |key: &str| oauth.get(key).and_then(Value::as_str).map(String::from);
    let client_secret = match oauth.get("clientSecret").and_then(Value::as_str) {
        Some(secret) => Some(crate::resolve::resolve_config_value_or_err(
            secret,
            &format!("MCP server \"{}\" oauth.clientSecret", entry.name),
            env,
        )?),
        None => None,
    };
    Ok(OAuthSettings {
        client_id: text("clientId"),
        client_secret,
        callback_port: oauth
            .get("callbackPort")
            .and_then(Value::as_u64)
            .and_then(|p| u16::try_from(p).ok()),
        callback_url: text("callbackUrl"),
        scope: text("scope"),
        client_name: text("clientName"),
        auth_server_metadata_url: text("authServerMetadataUrl"),
    })
}

/// `disconnected`: the connection dropped (for example the stdio server exited); the next call reconnects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    Connecting,
    Connected,
    Disconnected,
    NeedsAuth,
    Failed,
    Closed,
}

impl ServerState {
    pub fn as_str(self) -> &'static str {
        match self {
            ServerState::Connecting => "connecting",
            ServerState::Connected => "connected",
            ServerState::Disconnected => "disconnected",
            ServerState::NeedsAuth => "needs-auth",
            ServerState::Failed => "failed",
            ServerState::Closed => "closed",
        }
    }
}

/// The transport to connect, and the same transport as stdio when it is one (for its stderr).
type Transports = (Arc<dyn Transport>, Option<Arc<StdioTransport>>);

/// Why a connect attempt failed, and the stdio server's stderr tail.
type ConnectFailure = (Error, Option<String>);

struct Data {
    state: ServerState,
    error: Option<String>,
    tools: Vec<Value>,
    /// Whether the server declared the resources capability at the last connect (`hasResources`).
    has_resources: bool,
    resources: Vec<Value>,
    resource_templates: Vec<Value>,
    instructions: Option<String>,
    /// The live client and its generation (callbacks of older clients compare generations).
    client: Option<(u64, Client)>,
    next_generation: u64,
    opening: bool,
    /// Bumped when an attempt ends; waiters of that attempt read its outcome.
    attempts: u64,
    outcome: Result<(), String>,
    closed: bool,
}

pub struct Connection {
    pub entry: ServerEntry,
    env: ConnectionEnv,
    data: Mutex<Data>,
    settled: Condvar,
    /// OAuth (HTTP servers without an Authorization header or `auth`), or a `/login` provider's key.
    auth: Option<Arc<dyn AuthProvider>>,
    /// The server's last OAuth challenge; sign-in uses its resource metadata URL and scope.
    challenge: Arc<Mutex<Option<Challenge>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The last `max` characters of `text`, trimmed; `None` when empty.
fn tail(text: &str, max: usize) -> Option<String> {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    let tail: String = trimmed.chars().skip(count.saturating_sub(max)).collect();
    (!tail.is_empty()).then_some(tail)
}

/// `pathToFileURL(cwd).href`.
fn file_url(path: &std::path::Path) -> String {
    url::Url::from_file_path(path).map_or_else(|()| format!("file://{}", path.display()), |u| u.to_string())
}

impl Connection {
    pub fn new(entry: ServerEntry, env: ConnectionEnv) -> Arc<Self> {
        let challenge: Arc<Mutex<Option<Challenge>>> = Arc::default();
        let auth = Self::auth_for(&entry, &env, &challenge);
        Arc::new(Self {
            auth,
            challenge,
            entry,
            env,
            data: Mutex::new(Data {
                state: ServerState::Connecting,
                error: None,
                tools: Vec::new(),
                has_resources: false,
                resources: Vec::new(),
                resource_templates: Vec::new(),
                instructions: None,
                client: None,
                next_generation: 0,
                opening: false,
                attempts: 0,
                outcome: Ok(()),
                closed: false,
            }),
            settled: Condvar::new(),
        })
    }

    pub fn name(&self) -> &str {
        &self.entry.name
    }

    fn auth_for(
        entry: &ServerEntry,
        env: &ConnectionEnv,
        challenge: &Arc<Mutex<Option<Challenge>>>,
    ) -> Option<Arc<dyn AuthProvider>> {
        let url = entry.config.get("url").and_then(Value::as_str)?;
        if let Some(url) = oauth_url_of(entry)
            && let Some(credentials) = &env.credentials
        {
            let (settings_entry, settings_env) = (entry.clone(), env.env_overlay.clone());
            return Some(Arc::new(OAuthAuth::new(
                &url,
                credentials.for_server(&entry.name, &url),
                Box::new(move || oauth_settings_of(&settings_entry, &settings_env)),
                challenge.clone(),
            )));
        }
        let _ = url;
        let provider = entry.config.get("auth")?.get("provider")?.as_str()?;
        let token = env.provider_token.clone().unwrap_or_else(|| Arc::new(|_: &str| None));
        Some(Arc::new(ProviderAuth {
            provider: provider.to_string(),
            token,
        }))
    }

    /// The server URL when the server authenticates with OAuth (`oauthUrl`).
    pub fn oauth_url(&self) -> Option<String> {
        oauth_url_of(&self.entry)
    }

    /// `oauthSettings`: the `oauth` config, the client secret resolved.
    pub fn oauth_settings(&self) -> Result<OAuthSettings, String> {
        oauth_settings_of(&self.entry, &self.env.env_overlay)
    }

    /// The server's last OAuth challenge.
    pub fn challenge(&self) -> Option<Challenge> {
        self.challenge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn state(&self) -> ServerState {
        lock(&self.data).state
    }

    pub fn error(&self) -> Option<String> {
        lock(&self.data).error.clone()
    }

    pub fn tools(&self) -> Vec<Value> {
        lock(&self.data).tools.clone()
    }

    pub fn instructions(&self) -> Option<String> {
        lock(&self.data).instructions.clone()
    }

    /// Whether the server listed resources or templates at the last connect (`mcp.read` can then be named).
    pub fn has_listed_resources(&self) -> bool {
        let data = lock(&self.data);
        !data.resources.is_empty() || !data.resource_templates.is_empty()
    }

    /// `hasResources` with the lists of the last connect or change, without MCP App resources: how many resources
    /// and URI templates a server offering resources listed (`None` without the capability).
    pub fn listed_resource_counts(&self) -> Option<(usize, usize)> {
        let data = lock(&self.data);
        data.has_resources
            .then(|| (data.resources.len(), data.resource_templates.len()))
    }

    /// The per-request timeout: `timeout` seconds of the config, default 60.
    pub fn timeout(&self) -> Duration {
        let seconds = self
            .entry
            .config
            .get("timeout")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        Duration::from_millis((seconds * 1000.0) as u64)
    }

    fn is_http(&self) -> bool {
        self.entry.config.contains_key("url")
    }

    fn auth_provider_name(&self) -> Option<&str> {
        if !self.is_http() {
            return None;
        }
        self.entry.config.get("auth")?.get("provider")?.as_str()
    }

    /// A refresh that needs the user, or a rejection of a server with OAuth or a `/login` provider, needs sign-in.
    fn needs_sign_in(&self, error: &Error) -> bool {
        match http_error(error) {
            Some(("McpOAuthAuthorizationRequiredError", _)) => true,
            Some(("McpAuthRequiredError", _)) => self.auth.is_some(),
            _ => false,
        }
    }

    fn sign_in_required_message(&self) -> String {
        let how = self
            .auth_provider_name()
            .map_or_else(|| "/mcp".to_string(), |p| format!("/login {p}"));
        format!(
            "MCP server \"{}\" requires sign-in. Run {how} to sign in.",
            self.entry.name
        )
    }

    /// The connected client: the live one, the attempt in progress, or a new attempt.
    pub fn get_client(self: &Arc<Self>) -> Result<(u64, Client), String> {
        let mut data = lock(&self.data);
        loop {
            if data.closed {
                return Err(format!("MCP server \"{}\" is shut down", self.entry.name));
            }
            if let Some((generation, client)) = &data.client
                && client.connection_state() == ConnState::Connected
            {
                return Ok((*generation, client.clone()));
            }
            if !data.opening {
                break;
            }
            let attempt = data.attempts;
            while data.opening && data.attempts == attempt {
                data = self
                    .settled
                    .wait(data)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if let Err(error) = &data.outcome {
                return Err(error.clone());
            }
        }
        data.opening = true;
        drop(data);
        let result = self.open();
        let mut data = lock(&self.data);
        data.opening = false;
        data.attempts += 1;
        data.outcome = result.as_ref().map(drop).map_err(Clone::clone);
        self.settled.notify_all();
        result
    }

    fn open(self: &Arc<Self>) -> Result<(u64, Client), String> {
        lock(&self.data).state = ServerState::Connecting;
        let retries: &[u64] = if self.is_http() { &CONNECT_RETRY_DELAYS_MS } else { &[] };
        let mut attempt = 0;
        loop {
            match self.connect_once() {
                Ok(client) => return Ok(client),
                Err(failure) => {
                    let (error, stderr) = *failure;
                    let delay = retries.get(attempt);
                    let closed = lock(&self.data).closed;
                    let Some(delay) = delay.filter(|_| !closed && is_transient_error(&error)) else {
                        return Err(self.connect_failed(&error, stderr));
                    };
                    std::thread::sleep(Duration::from_millis(*delay));
                    if lock(&self.data).closed {
                        return Err(self.connect_failed(&error, stderr));
                    }
                }
            }
            attempt += 1;
        }
    }

    fn transport(&self) -> Result<Transports, Error> {
        if let Some(factory) = &self.env.transport {
            return Ok((factory(&self.entry)?, None));
        }
        let (spec, secrets) = transport_spec(
            &self.entry.name,
            &self.entry.config,
            &self.env.cwd,
            &self.env.home,
            &self.env.env_overlay,
        )
        .map_err(Error::Other)?;
        if let Some(sink) = &self.env.secrets {
            sink(&secrets);
        }
        Ok(match spec {
            TransportSpec::Http(mut options) => {
                options.auth = self.auth.clone();
                (Arc::new(HttpTransport::new(options)), None)
            }
            TransportSpec::Stdio(options) => {
                let stdio = Arc::new(StdioTransport::new(options));
                (stdio.clone(), Some(stdio))
            }
        })
    }

    fn connect_once(self: &Arc<Self>) -> Result<(u64, Client), Box<ConnectFailure>> {
        let cwd = &self.env.cwd;
        let client = Client::new(ClientOptions {
            name: "ultron".into(),
            version: self.env.client_version.clone(),
            request_timeout: Some(self.timeout()),
            roots: Some(vec![json!({
                "uri": file_url(cwd),
                "name": cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            })]),
            ..ClientOptions::default()
        });
        if let Some(log) = self.env.log.clone() {
            let name = self.entry.name.clone();
            client.on_notification("notifications/message", move |params| log.write(&name, params));
        }
        let mut stdio = None;
        let result = (|| {
            let (transport, stdio_transport) = self.transport()?;
            stdio = stdio_transport;
            client.connect(transport)?;
            let generation = {
                let mut data = lock(&self.data);
                data.next_generation += 1;
                data.next_generation
            };
            let weak = Arc::downgrade(self);
            let refreshed = client.clone();
            client.on_notification("notifications/tools/list_changed", move |_| {
                // A request cannot wait on the reader thread that delivers its answer.
                let (weak, client) = (weak.clone(), refreshed.clone());
                std::thread::spawn(move || refresh_tools(&weak, generation, &client));
            });
            let weak = Arc::downgrade(self);
            let refreshed = client.clone();
            client.on_notification("notifications/resources/list_changed", move |_| {
                let (weak, client) = (weak.clone(), refreshed.clone());
                std::thread::spawn(move || refresh_resources(&weak, generation, &client));
            });
            let weak = Arc::downgrade(self);
            let closed_stdio = stdio.clone();
            client.on_close(move || {
                if let Some(connection) = weak.upgrade() {
                    connection.client_closed(generation, closed_stdio.as_deref());
                }
            });
            // Servers without the tools capability (prompts or resources only) do not answer tools/list.
            let capabilities = client.server_capabilities().unwrap_or(Value::Null);
            let has_resources = capabilities.get("resources").is_some();
            let tools = if capabilities.get("tools").is_some_and(truthy) {
                client.list_tools(RequestOptions::default())?
            } else {
                Vec::new()
            };
            let (resources, templates) = if has_resources {
                fetch_resources(&client)
            } else {
                (Vec::new(), Vec::new())
            };
            let mut data = lock(&self.data);
            if data.closed {
                return Err(Error::Other("shut down while connecting".into()));
            }
            if client.connection_state() != ConnState::Connected {
                return Err(Error::Other("connection closed during setup".into()));
            }
            data.client = Some((generation, client.clone()));
            data.tools = tools;
            data.has_resources = has_resources;
            data.resources = resources;
            data.resource_templates = templates;
            data.instructions = client
                .instructions()
                .map(|i| i.trim().to_string())
                .filter(|i| !i.is_empty());
            data.state = ServerState::Connected;
            data.error = None;
            Ok((generation, client.clone()))
        })();
        result.map_err(|error| {
            client.close();
            let stderr = stdio.and_then(|s| tail(&s.stderr(), STDERR_TAIL_CHARS));
            Box::new((error, stderr))
        })
    }

    /// Node's message for a stdio server that could not be spawned (`spawn <command> ENOENT`), which the gateway
    /// shows; other errors keep their text.
    fn node_message(&self, error: &Error) -> String {
        let text = error.to_string();
        let command = self
            .entry
            .config
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let command = crate::resolve::expand_home(command, &self.env.home)
            .display()
            .to_string();
        let Some(reason) = text.strip_prefix(&format!("spawn {command} failed: ")) else {
            return text;
        };
        let code = match reason {
            r if r.ends_with("(os error 2)") => "ENOENT",
            r if r.ends_with("(os error 13)") => "EACCES",
            _ => return text,
        };
        format!("spawn {command} {code}")
    }

    fn connect_failed(&self, error: &Error, stderr: Option<String>) -> String {
        let mut data = lock(&self.data);
        if self.needs_sign_in(error) && !data.closed {
            data.state = ServerState::NeedsAuth;
            data.error = None;
            return self.sign_in_required_message();
        }
        data.state = if data.closed {
            ServerState::Closed
        } else {
            ServerState::Failed
        };
        let message = match stderr {
            Some(stderr) => format!("{}\n{stderr}", self.node_message(error)),
            None => self.node_message(error),
        };
        data.error = Some(message.clone());
        format!("MCP server \"{}\" failed to connect: {message}", self.entry.name)
    }

    /// The transport dropped. The next call reconnects; until then the status shows why.
    fn client_closed(&self, generation: u64, stdio: Option<&StdioTransport>) {
        let mut data = lock(&self.data);
        if data.closed || data.client.as_ref().map(|(g, _)| *g) != Some(generation) {
            return;
        }
        data.client = None;
        data.state = ServerState::Disconnected;
        data.error = Some(match stdio.and_then(|s| tail(&s.stderr(), STDERR_TAIL_CHARS)) {
            Some(stderr) => format!("Connection closed\n{stderr}"),
            None => "Connection closed".into(),
        });
    }

    fn drop_client(&self, generation: u64, client: &Client) {
        {
            let mut data = lock(&self.data);
            if data.client.as_ref().map(|(g, _)| *g) == Some(generation) {
                data.client = None;
            }
        }
        client.close();
    }

    /// Runs a request, reconnecting when needed. `read_only` requests are retried once after a transient HTTP
    /// error; tool calls are not, since they may have run.
    fn with_client<T>(
        self: &Arc<Self>,
        read_only: bool,
        run: impl Fn(&Client) -> Result<T, Error>,
    ) -> Result<T, String> {
        let mut attempt = 1;
        loop {
            let (generation, client) = self.get_client()?;
            match run(&client) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let kind = http_error(&error).map(|(kind, _)| kind);
                    if read_only && attempt == 1 && kind.is_some_and(|k| k != "TypeError") && is_transient_error(&error)
                    {
                        std::thread::sleep(Duration::from_millis(CONNECT_RETRY_DELAYS_MS[0]));
                        attempt += 1;
                        continue;
                    }
                    if kind == Some("McpSessionExpiredError") && attempt == 1 {
                        // The server no longer knows the session, so it did not run the request. The old client is
                        // detached but not closed: its other in-flight calls get the same 404 and retry the same way.
                        let mut data = lock(&self.data);
                        if data.client.as_ref().map(|(g, _)| *g) == Some(generation) {
                            data.client = None;
                        }
                        attempt += 1;
                        continue;
                    }
                    if !self.needs_sign_in(&error) {
                        return Err(error.to_string());
                    }
                    self.drop_client(generation, &client);
                    let mut data = lock(&self.data);
                    data.state = ServerState::NeedsAuth;
                    data.error = None;
                    return Err(self.sign_in_required_message());
                }
            }
        }
    }

    fn request_options(&self, cancel: Option<&CancelToken>) -> RequestOptions {
        RequestOptions {
            timeout: Some(self.timeout()),
            cancel: cancel.cloned(),
            on_progress: None,
        }
    }

    pub fn call_tool(
        self: &Arc<Self>,
        name: &str,
        args: &Map<String, Value>,
        cancel: Option<&CancelToken>,
    ) -> Result<Value, String> {
        let options = self.request_options(cancel);
        self.with_client(false, |client| {
            client.call_tool(name, Some(Value::Object(args.clone())), options.clone())
        })
    }

    pub fn read_resource(self: &Arc<Self>, uri: &str, cancel: Option<&CancelToken>) -> Result<Value, String> {
        let options = self.request_options(cancel);
        self.with_client(true, |client| client.read_resource(uri, options.clone()))
    }

    pub fn all_resources(self: &Arc<Self>, cancel: Option<&CancelToken>) -> Result<Vec<Value>, String> {
        let options = self.request_options(cancel);
        self.with_client(true, |client| client.list_resources(options.clone()))
    }

    pub fn all_resource_templates(self: &Arc<Self>, cancel: Option<&CancelToken>) -> Result<Vec<Value>, String> {
        let options = self.request_options(cancel);
        self.with_client(true, |client| list_templates(client, options.clone()))
    }

    /// Connect again with fresh credentials.
    pub fn reconnect(self: &Arc<Self>) -> Result<(), String> {
        self.wait_opening();
        let current = lock(&self.data).client.clone();
        if let Some((generation, client)) = current {
            self.drop_client(generation, &client);
        }
        self.get_client().map(drop)
    }

    fn wait_opening(&self) {
        let mut data = lock(&self.data);
        while data.opening {
            data = self
                .settled
                .wait(data)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Disconnect after the stored credentials were removed: the server needs sign-in again.
    pub fn sign_out(&self) {
        self.wait_opening();
        let current = lock(&self.data).client.clone();
        if let Some((generation, client)) = current {
            self.drop_client(generation, &client);
        }
        let mut data = lock(&self.data);
        if !data.closed {
            data.state = ServerState::NeedsAuth;
            data.error = None;
        }
    }

    pub fn close(&self) {
        let client = {
            let mut data = lock(&self.data);
            data.closed = true;
            data.state = ServerState::Closed;
            data.client.take()
        };
        if let Some((_, client)) = client {
            client.close();
        }
    }
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

/// Servers that do not implement `resources/templates/list` have no templates.
fn list_templates(client: &Client, options: RequestOptions) -> Result<Vec<Value>, Error> {
    match client.list_resource_templates(options) {
        Err(Error::Mcp {
            code: METHOD_NOT_FOUND, ..
        }) => Ok(Vec::new()),
        other => other,
    }
}

/// Resources and templates at connect time. A server whose lists fail still connects.
fn fetch_resources(client: &Client) -> (Vec<Value>, Vec<Value>) {
    let resources = client.list_resources(RequestOptions::default()).unwrap_or_default();
    let templates = list_templates(client, RequestOptions::default()).unwrap_or_default();
    (
        resources.into_iter().filter(|r| !is_mcp_app_resource(r)).collect(),
        templates.into_iter().filter(|t| !is_mcp_app_resource(t)).collect(),
    )
}

fn refresh_tools(weak: &Weak<Connection>, generation: u64, client: &Client) {
    let result = client.list_tools(RequestOptions::default());
    let Some(connection) = weak.upgrade() else { return };
    let mut data = lock(&connection.data);
    match result {
        Ok(tools) => {
            if data.closed || data.client.as_ref().map(|(g, _)| *g) != Some(generation) {
                return;
            }
            data.tools = tools;
        }
        Err(error) => data.error = Some(format!("Failed to refresh tools: {error}")),
    }
}

fn refresh_resources(weak: &Weak<Connection>, generation: u64, client: &Client) {
    let (resources, templates) = fetch_resources(client);
    let Some(connection) = weak.upgrade() else { return };
    let mut data = lock(&connection.data);
    if data.closed || data.client.as_ref().map(|(g, _)| *g) != Some(generation) {
        return;
    }
    data.resources = resources;
    data.resource_templates = templates;
}
