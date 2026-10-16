//! Google application default credentials as `@google/genai`'s Vertex client resolves them through
//! `google-auth-library` (10.6.2, `GoogleAuth` with the `cloud-platform` scope): the key file named by
//! `GOOGLE_APPLICATION_CREDENTIALS`, else the ADC chain (the lower-case variable, the gcloud well-known file, the GCE
//! metadata server), then an access token for the credential (`authorized_user` refresh, `service_account` JWT bearer
//! exchange, `impersonated_service_account` through IAM credentials, Compute Engine metadata, and workload identity
//! federation in `external.rs`), sent as `authorization: Bearer <token>` plus `x-goog-user-project` for a quota project.
//! A service account outside the default universe signs its own JWT instead of exchanging one.
//!
//! Upstream builds a fresh `GoogleAuth` for every stream call, so nothing is cached here either. Error messages are
//! upstream's (gaxios' API error extraction, each client's rewording, Node's file errors), recorded by
//! `fixtures/generate-vertex-adc.mjs`. Environment lookups that upstream makes on `process.env` (HOME, the metadata
//! server settings, the quota project override) go through the provider env first, so a credential can scope them.

mod external;

use crate::env::{ProviderEnv, provider_env_value};
use crate::http::{HttpRequest, HttpResponse, Transport};
use crate::jsutil::json_parse_error;
use crate::oauth_flow::{base64url, form_urlencode};
use serde_json::{Map, Value, json};
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use external::{AwsCredentials, aws_subject_token, self_signed_jwt};

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const AUTH_USER_AGENT: &str = "google-api-nodejs-client/10.6.2";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const DEFAULT_UNIVERSE: &str = "googleapis.com";
const METADATA_HOST: &str = "http://169.254.169.254";
const METADATA_SECONDARY_HOST: &str = "http://metadata.google.internal.";
const NO_ADC_FOUND: &str = "Could not load the default credentials. Browse to https://cloud.google.com/docs/authentication/getting-started for more information.";
const KEY_TYPE_ERROR: &str = "key must be a string, a buffer or an object";
const BAD_KEY: &str = "error:1E08010C:DECODER routines::unsupported";

/// The headers `GoogleAuth.getRequestHeaders` adds to a Vertex request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthHeaders {
    pub authorization: String,
    pub user_project: Option<String>,
}

/// A failure with upstream's message; `status` is the HTTP status of the request that failed, when one did.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fail {
    /// The JavaScript error class, for messages that embed `String(error)`.
    name: &'static str,
    message: String,
    status: Option<u16>,
    /// The parsed error response body.
    data: Option<Box<Value>>,
}

impl Fail {
    fn new(message: impl Into<String>) -> Self {
        Fail {
            name: "Error",
            message: message.into(),
            status: None,
            data: None,
        }
    }

    fn typed(name: &'static str, message: impl Into<String>) -> Self {
        Fail {
            name,
            ..Fail::new(message)
        }
    }
}

#[derive(Clone, Debug)]
enum Credential {
    User {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        universe: String,
        quota: Option<String>,
    },
    ServiceAccount {
        email: String,
        key: String,
        key_id: Option<String>,
        universe: String,
        quota: Option<String>,
    },
    Impersonated {
        source: Box<Credential>,
        target: String,
        delegates: Value,
        universe: String,
        quota: Option<String>,
    },
    Compute {
        quota: Option<String>,
    },
    /// `external_account`: a subject token exchanged at STS (`external.rs`).
    External(Box<external::External>),
    /// `external_account_authorized_user`: a refresh token redeemed at STS.
    ExternalUser(Box<external::ExternalUser>),
    /// `fromStreamAsync`'s fallback for a key file that is not a usable credential JSON: a `JWT` with only `keyFile`.
    /// `scoped` when a failed service-account `fromJSON` had already put the scopes on the client options.
    KeyFile {
        path: PathBuf,
        scoped: bool,
    },
}

impl Credential {
    fn set_quota(&mut self, value: String) {
        match self {
            Credential::User { quota, .. }
            | Credential::ServiceAccount { quota, .. }
            | Credential::Impersonated { quota, .. }
            | Credential::Compute { quota } => *quota = Some(value),
            Credential::External(external) => external.quota = Some(value),
            Credential::ExternalUser(user) => user.quota = Some(value),
            Credential::KeyFile { .. } => {}
        }
    }

    fn quota(&self) -> Option<String> {
        match self {
            Credential::User { quota, .. }
            | Credential::ServiceAccount { quota, .. }
            | Credential::Impersonated { quota, .. }
            | Credential::Compute { quota } => quota.clone(),
            Credential::External(external) => external.quota.clone(),
            Credential::ExternalUser(user) => user.quota.clone(),
            Credential::KeyFile { .. } => None,
        }
    }

    /// The client's `universeDomain`.
    fn universe(&self) -> String {
        match self {
            Credential::User { universe, .. }
            | Credential::ServiceAccount { universe, .. }
            | Credential::Impersonated { universe, .. } => universe.clone(),
            Credential::External(external) => external.universe.clone(),
            Credential::ExternalUser(user) => user.universe.clone(),
            Credential::Compute { .. } | Credential::KeyFile { .. } => DEFAULT_UNIVERSE.into(),
        }
    }

    /// `OAuth2Client.getRequestMetadataAsync` rewords a 403/404 refresh failure; the external account clients are
    /// plain `AuthClient`s and do not.
    fn rewords_refresh_failures(&self) -> bool {
        !matches!(self, Credential::External(_) | Credential::ExternalUser(_))
    }
}

/// `GoogleAuth({ keyFilename: GOOGLE_APPLICATION_CREDENTIALS, scopes: [cloud-platform] }).getRequestHeaders()`.
pub async fn request_headers(env: Option<&ProviderEnv>, transport: &dyn Transport) -> Result<AuthHeaders, String> {
    let credential = match provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env) {
        Some(path) => key_file_credential(&path),
        None => application_default(env, transport).await,
    }
    .map_err(|f| f.message)?;
    let token = match &credential {
        // `JWT.getRequestMetadataAsync`: outside the default universe, a self-signed JWT with the scopes.
        Credential::ServiceAccount {
            email,
            key,
            key_id,
            universe,
            ..
        } if universe != DEFAULT_UNIVERSE => {
            let iat = crate::options::now_ms().div_euclid(1000);
            self_signed_jwt(email, key, key_id.as_deref(), iat).map_err(Fail::new)
        }
        _ => access_token(&credential, env, transport).await,
    };
    let token = token.map_err(|mut fail| {
        if credential.rewords_refresh_failures() && matches!(fail.status, Some(403 | 404)) {
            fail.message = format!("Could not refresh access token: {}", fail.message);
        }
        fail.message
    })?;
    Ok(AuthHeaders {
        authorization: format!("Bearer {token}"),
        user_project: credential.quota(),
    })
}

// ------------------------------------------------------------------------------------------ loading the credential

fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(_) => true,
    }
}

/// `String(value)` for the scalar fields a credential file holds.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".into(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
        Some(other) => other.to_string(),
    }
}

fn optional_string(json: &Value, key: &str) -> Option<String> {
    js_truthy(json.get(key)).then(|| js_string(json.get(key)))
}

/// Which `fromJSON` branch a credential JSON takes; the service-account branch mutates the shared client options.
enum Branch {
    Known,
    Jwt,
}

/// A JSON truthy string field, else the client default (`json.universe_domain || this.universeDomain`).
fn universe_of(json: &Value) -> String {
    optional_string(json, "universe_domain").unwrap_or_else(|| DEFAULT_UNIVERSE.into())
}

/// `GoogleAuth.fromJSON`.
fn from_json(json: &Value) -> Result<Credential, (Fail, Branch)> {
    let kind = match json {
        Value::Null => {
            return Err((
                Fail::typed("TypeError", "Cannot read properties of null (reading 'type')"),
                Branch::Known,
            ));
        }
        Value::Object(map) => map.get("type").and_then(Value::as_str).unwrap_or_default(),
        _ => "",
    };
    let missing = |field: &str| Fail::new(format!("The incoming JSON object does not contain a {field} field"));
    match kind {
        "authorized_user" => {
            for field in ["client_id", "client_secret", "refresh_token"] {
                if !js_truthy(json.get(field)) {
                    return Err((missing(field), Branch::Known));
                }
            }
            Ok(Credential::User {
                client_id: js_string(json.get("client_id")),
                client_secret: js_string(json.get("client_secret")),
                refresh_token: js_string(json.get("refresh_token")),
                universe: universe_of(json),
                quota: optional_string(json, "quota_project_id"),
            })
        }
        "impersonated_service_account" => impersonated(json).map_err(|fail| (fail, Branch::Known)),
        "external_account" => external::External::from_json(json)
            .map(|client| Credential::External(Box::new(client)))
            .map_err(|fail| (fail, Branch::Known)),
        "external_account_authorized_user" => Ok(Credential::ExternalUser(Box::new(
            external::ExternalUser::from_json(json),
        ))),
        _ => {
            if !js_truthy(Some(json)) {
                return Err((
                    Fail::new("Must pass in a JSON object containing the service account auth settings."),
                    Branch::Jwt,
                ));
            }
            for field in ["client_email", "private_key"] {
                if !js_truthy(json.get(field)) {
                    return Err((missing(field), Branch::Jwt));
                }
            }
            Ok(Credential::ServiceAccount {
                email: js_string(json.get("client_email")),
                key: js_string(json.get("private_key")),
                key_id: optional_string(json, "private_key_id"),
                universe: universe_of(json),
                quota: optional_string(json, "quota_project_id"),
            })
        }
    }
}

/// `GoogleAuth.fromImpersonatedJSON`.
fn impersonated(json: &Value) -> Result<Credential, Fail> {
    let Some(source_json) = json.get("source_credentials").filter(|v| js_truthy(Some(v))) else {
        return Err(Fail::new(
            "The incoming JSON object does not contain a source_credentials field",
        ));
    };
    if !js_truthy(json.get("service_account_impersonation_url")) {
        return Err(Fail::new(
            "The incoming JSON object does not contain a service_account_impersonation_url field",
        ));
    }
    let source = from_json(source_json).map_err(|(fail, _)| fail)?;
    let url = js_string(json.get("service_account_impersonation_url"));
    if url.encode_utf16().count() > 256 {
        return Err(Fail::typed(
            "RangeError",
            format!("Target principal is too long: {url}"),
        ));
    }
    // `/(?<target>[^/]+):(generateAccessToken|generateIdToken)$/`
    let target = ["generateAccessToken", "generateIdToken"].iter().find_map(|method| {
        let head = url.strip_suffix(method)?.strip_suffix(':')?;
        let name = head.rsplit('/').next().unwrap_or(head);
        (!name.is_empty()).then(|| name.to_string())
    });
    let Some(target) = target else {
        return Err(Fail::typed(
            "RangeError",
            format!("Cannot extract target principal from {url}"),
        ));
    };
    // `new Impersonated`: the source's universe, unless one is set explicitly, which must then match it.
    let universe = match external::camel_option(json, "universe_domain").filter(|v| js_truthy(Some(v))) {
        None => source.universe(),
        Some(explicit) => {
            let explicit = js_string(Some(explicit));
            if explicit != source.universe() {
                return Err(Fail::typed(
                    "RangeError",
                    format!(
                        "Universe domain {} in source credentials does not match {explicit} universe domain set for impersonated credentials.",
                        source.universe()
                    ),
                ));
            }
            explicit
        }
    };
    Ok(Credential::Impersonated {
        source: Box::new(source),
        target,
        delegates: json
            .get("delegates")
            .filter(|v| !v.is_null())
            .cloned()
            .unwrap_or_else(|| json!([])),
        universe,
        quota: optional_string(json, "quota_project_id"),
    })
}

fn io_message(error: &std::io::Error, syscall: &str, path: &Path) -> String {
    let path = path.display();
    match error.kind() {
        std::io::ErrorKind::NotFound => format!("ENOENT: no such file or directory, {syscall} '{path}'"),
        std::io::ErrorKind::PermissionDenied => format!("EACCES: permission denied, {syscall} '{path}'"),
        std::io::ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".into(),
        _ => format!("{error}, {syscall} '{path}'"),
    }
}

fn absolute(path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

/// The `keyFilename` path: `fromStreamAsync` over `fs.createReadStream(path.resolve(keyFilename))`, falling back to a
/// key-file `JWT` when the JSON is not a usable credential.
fn key_file_credential(key_file: &str) -> Result<Credential, Fail> {
    let path = absolute(key_file);
    let text = std::fs::read(&path).map_err(|e| Fail::new(io_message(&e, "open", &path)))?;
    let text = String::from_utf8_lossy(&text);
    let fallback = |scoped| Credential::KeyFile {
        path: path.clone(),
        scoped,
    };
    if json_parse_error(&text).is_some() {
        return Ok(fallback(false));
    }
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match from_json(&json) {
        Ok(credential) => Ok(credential),
        Err((_, Branch::Jwt)) => Ok(fallback(true)),
        Err((_, Branch::Known)) => Ok(fallback(false)),
    }
}

/// Node's `fs.realpathSync` (the JavaScript walk): the first component that cannot be `lstat`ed names the error.
fn realpath(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        if let Err(error) = std::fs::symlink_metadata(&prefix) {
            return Err(std::io::Error::new(error.kind(), io_message(&error, "lstat", &prefix)));
        }
    }
    std::fs::canonicalize(path)
}

/// `_getApplicationCredentialsFromFilePath`: the ADC chain's file reader, which throws instead of falling back.
fn file_path_credential(file_path: &str) -> Result<Credential, Fail> {
    if file_path.is_empty() {
        return Err(Fail::new("The file path is invalid."));
    }
    let not_a_file = |shown: &str, inner: &str| {
        Fail::new(format!(
            "The file at {shown} does not exist, or it is not a file. {inner}"
        ))
    };
    let real = realpath(&absolute(file_path)).map_err(|e| not_a_file(file_path, &e.to_string()))?;
    let shown = real.display().to_string();
    match std::fs::symlink_metadata(&real) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(not_a_file(&shown, "")),
        Err(e) => return Err(not_a_file(&shown, &io_message(&e, "lstat", &real))),
    }
    let text = std::fs::read(&real).map_err(|e| Fail::new(io_message(&e, "open", &real)))?;
    let text = String::from_utf8_lossy(&text);
    if let Some(message) = json_parse_error(&text) {
        return Err(Fail::typed("SyntaxError", message));
    }
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    from_json(&json).map_err(|(fail, _)| fail)
}

/// `getApplicationDefaultAsync`.
async fn application_default(env: Option<&ProviderEnv>, transport: &dyn Transport) -> Result<Credential, Fail> {
    let mut credential = if let Some(path) = provider_env_value("google_application_credentials", env) {
        file_path_credential(&path)?
    } else if let Some(path) = provider_env_value("HOME", env)
        .map(|home| Path::new(&home).join(".config/gcloud/application_default_credentials.json"))
        .filter(|path| path.exists())
    {
        file_path_credential(&path.display().to_string())?
    } else if gcp_residency(env) || metadata_available(env, transport).await? {
        Credential::Compute { quota: None }
    } else {
        return Err(Fail::new(NO_ADC_FOUND));
    };
    if let Some(quota) = provider_env_value("GOOGLE_CLOUD_QUOTA_PROJECT", env) {
        credential.set_quota(quota);
    }
    Ok(credential)
}

// ------------------------------------------------------------------------------------------- the metadata server

/// gcp-metadata's `detectGCPResidency`: a serverless runtime, a Google BIOS, or a GCE MAC address.
fn gcp_residency(env: Option<&ProviderEnv>) -> bool {
    if ["CLOUD_RUN_JOB", "FUNCTION_NAME", "K_SERVICE"]
        .iter()
        .any(|name| provider_env_value(name, env).is_some())
    {
        return true;
    }
    if cfg!(target_os = "linux")
        && Path::new("/sys/class/dmi/id/bios_date").exists()
        && std::fs::read_to_string("/sys/class/dmi/id/bios_vendor").is_ok_and(|v| v.contains("Google"))
    {
        return true;
    }
    std::fs::read_dir("/sys/class/net").is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            std::fs::read_to_string(entry.path().join("address")).is_ok_and(|mac| mac.trim().starts_with("42:01"))
        })
    })
}

fn metadata_base(env: Option<&ProviderEnv>, host: Option<&str>) -> String {
    let host = host
        .map(str::to_string)
        .or_else(|| provider_env_value("GCE_METADATA_IP", env))
        .or_else(|| provider_env_value("GCE_METADATA_HOST", env))
        .unwrap_or_else(|| METADATA_HOST.to_string());
    let host = if host.starts_with("http://") || host.starts_with("https://") {
        host
    } else {
        format!("http://{host}")
    };
    reqwest::Url::parse(&host)
        .and_then(|url| url.join("/computeMetadata/v1"))
        .map_or_else(|_| format!("{host}/computeMetadata/v1"), |url| url.to_string())
}

fn metadata_timeout(env: Option<&ProviderEnv>) -> Option<Duration> {
    (!gcp_residency(env)).then_some(Duration::from_millis(3000))
}

fn metadata_request(url: String) -> HttpRequest {
    HttpRequest {
        method: "GET".into(),
        url,
        headers: [("metadata-flavor".to_string(), "Google".to_string())].into(),
        body: String::new(),
    }
}

/// `metadataAccessor`: the response must carry `Metadata-Flavor: Google`.
async fn metadata_get(
    transport: &dyn Transport,
    url: String,
    no_response_retries: u32,
    timeout: Option<Duration>,
) -> Result<Value, Fail> {
    let policy = Retry {
        no_response_retries,
        status_retries: STATUS_RETRIES,
        timeout,
    };
    let response = gaxios(transport, metadata_request(url), &policy, ResponseType::Text).await?;
    match response.headers.get("metadata-flavor").map(String::as_str) {
        Some("Google") => {}
        other => {
            return Err(Fail::typed(
                "RangeError",
                format!(
                    "Invalid response from metadata service: incorrect Metadata-Flavor header. Expected 'Google', got {}",
                    other.map_or_else(|| "no header".to_string(), |v| format!("'{v}'"))
                ),
            ));
        }
    }
    Ok(match &response.data {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| response.data.clone()),
        other => other.clone(),
    })
}

/// gcp-metadata's `isAvailable`.
async fn metadata_available(env: Option<&ProviderEnv>, transport: &dyn Transport) -> Result<bool, Fail> {
    if let Some(value) = provider_env_value("METADATA_SERVER_DETECTION", env) {
        match value.trim().to_lowercase().as_str() {
            "assume-present" => return Ok(true),
            "none" => return Ok(false),
            "bios-only" => return Ok(gcp_residency(env)),
            "ping-only" => {}
            other => {
                return Err(Fail::typed(
                    "RangeError",
                    format!(
                        "Unknown `METADATA_SERVER_DETECTION` env variable. Got `{other}`, but it should be `assume-present`, `none`, `bios-only`, `ping-only`, or unset"
                    ),
                ));
            }
        }
    }
    let retries = provider_env_value("DETECT_GCP_RETRIES", env)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let timeout = metadata_timeout(env);
    let primary = metadata_get(
        transport,
        format!("{}/instance", metadata_base(env, None)),
        retries,
        timeout,
    );
    let explicit_host =
        provider_env_value("GCE_METADATA_IP", env).is_some() || provider_env_value("GCE_METADATA_HOST", env).is_some();
    if explicit_host {
        return Ok(primary.await.is_ok());
    }
    // `fastFailMetadataRequest`: the first of the two hosts to answer.
    let secondary = metadata_get(
        transport,
        format!("{}/instance", metadata_base(env, Some(METADATA_SECONDARY_HOST))),
        retries,
        timeout,
    );
    let (primary, secondary) = (Box::pin(primary), Box::pin(secondary));
    Ok(match futures_select(primary, secondary).await {
        (Ok(_), _) => true,
        (Err(_), other) => other.await.is_ok(),
    })
}

/// The first of two futures to finish, and the other one.
async fn futures_select<'a, T>(
    a: std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>,
    b: std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>,
) -> (T, std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>) {
    let (mut a, mut b) = (a, b);
    tokio::select! {
        first = &mut a => (first, b),
        first = &mut b => (first, a),
    }
}

// ------------------------------------------------------------------------------------------------------- gaxios

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResponseType {
    /// `responseType: 'json'`: parsed when it parses, else the text.
    Json,
    Text,
    /// Upstream's default: by content type.
    Unknown,
}

struct Retry {
    no_response_retries: u32,
    /// Retries of 1xx/408/429/5xx statuses: three under a retry config, none without one.
    status_retries: u32,
    timeout: Option<Duration>,
}

/// The retry policy every request here runs under: three retries of 1xx/408/429/5xx statuses for any method.
const STATUS_RETRIES: u32 = 3;

struct Response {
    headers: crate::http::HeaderMap,
    data: Value,
}

async fn read_text(response: &mut HttpResponse) -> Result<String, String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.body.next_chunk().await? {
        bytes.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn response_data(kind: ResponseType, content_type: Option<&str>, text: String) -> Value {
    let parsed = |text: String| serde_json::from_str(&text).unwrap_or(Value::String(text));
    match kind {
        ResponseType::Json => parsed(text),
        ResponseType::Text => Value::String(text),
        ResponseType::Unknown => match content_type.map(str::to_lowercase) {
            Some(ct) if ct.contains("application/json") => parsed(text),
            _ => Value::String(text),
        },
    }
}

/// `GaxiosError.extractAPIErrorFromResponse`.
fn api_error_message(status: u16, data: &Value) -> String {
    let mut message = format!("Request failed with status code {status}");
    if let Value::String(text) = data {
        message = text.clone();
    }
    if let Some(error) = data.get("error").filter(|e| js_truthy(Some(e))) {
        match error {
            Value::String(text) => return text.clone(),
            Value::Object(object) => {
                if let Some(text) = object.get("message").and_then(Value::as_str) {
                    message = text.to_string();
                }
                if let Some(Value::Array(errors)) = object.get("errors") {
                    let joined = errors
                        .iter()
                        .filter_map(|e| e.get("message").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !joined.is_empty() {
                        return joined;
                    }
                }
            }
            _ => {}
        }
    }
    message
}

/// `getNextRetryDelay` with gaxios' defaults: 100 ms, then `(2^n - 1) / 2` seconds.
fn retry_delay(attempt: u32) -> Duration {
    let base = if attempt == 0 { 100.0 } else { 0.0 };
    Duration::from_millis((base + (2f64.powi(attempt as i32) - 1.0) / 2.0 * 1000.0) as u64)
}

/// `Gaxios.request` with a retry config: status failures and missing responses are retried, then thrown as a
/// `GaxiosError` whose message comes from the response body.
async fn gaxios(
    transport: &dyn Transport,
    request: HttpRequest,
    policy: &Retry,
    kind: ResponseType,
) -> Result<Response, Fail> {
    let mut attempt = 0;
    loop {
        let sent = match policy.timeout {
            Some(limit) => match tokio::time::timeout(limit, transport.send(request.clone())).await {
                Ok(sent) => sent,
                Err(_) => Err("The operation was aborted due to timeout".into()),
            },
            None => transport.send(request.clone()).await,
        };
        let fail = match sent {
            Err(message) => {
                if attempt < policy.no_response_retries {
                    tokio::time::sleep(retry_delay(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Fail::new(if message.contains("timeout") {
                    message
                } else {
                    "fetch failed".to_string()
                })
            }
            Ok(mut response) => {
                let content_type = response.headers.get("content-type").cloned();
                let text = read_text(&mut response).await.map_err(Fail::new)?;
                let data = response_data(kind, content_type.as_deref(), text);
                if (200..300).contains(&response.status) {
                    return Ok(Response {
                        headers: response.headers,
                        data,
                    });
                }
                let status = response.status;
                let retryable = matches!(status, 100..=199 | 408 | 429 | 500..=599);
                if retryable && attempt < policy.status_retries {
                    tokio::time::sleep(retry_delay(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Fail {
                    name: "Error",
                    message: api_error_message(status, &data),
                    status: Some(status),
                    data: Some(Box::new(data)),
                }
            }
        };
        return Err(fail);
    }
}

fn form_request(body: &[(&str, &str)], json_response: bool) -> HttpRequest {
    let mut headers: crate::http::HeaderMap = [
        ("user-agent".to_string(), AUTH_USER_AGENT.to_string()),
        (
            "content-type".to_string(),
            "application/x-www-form-urlencoded;charset=UTF-8".to_string(),
        ),
    ]
    .into();
    if json_response {
        headers.insert("accept".into(), "application/json".into());
    }
    HttpRequest {
        method: "POST".into(),
        url: TOKEN_URL.into(),
        headers,
        body: form_urlencode(body),
    }
}

const AUTH_RETRY: Retry = Retry {
    no_response_retries: 2,
    status_retries: STATUS_RETRIES,
    timeout: None,
};

/// A request without a retry config: gaxios tries it once.
const NO_RETRY: Retry = Retry {
    no_response_retries: 0,
    status_retries: 0,
    timeout: None,
};

// ------------------------------------------------------------------------------------------------- access tokens

fn token_field(data: &Value, key: &str) -> String {
    js_string(data.get(key))
}

/// `UserRefreshClient.refreshTokenNoCache`.
async fn refresh_user(
    transport: &dyn Transport,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<String, Fail> {
    let request = form_request(
        &[
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "refresh_token"),
        ],
        false,
    );
    match gaxios(transport, request, &AUTH_RETRY, ResponseType::Unknown).await {
        Ok(response) => Ok(token_field(&response.data, "access_token")),
        Err(mut fail) => {
            let reauth = fail.data.as_ref().is_some_and(|data| {
                data.get("error_description")
                    .map(|d| js_string(Some(d)).to_lowercase().contains("reauth"))
                    .unwrap_or(false)
            });
            if fail.message == "invalid_grant" && reauth {
                fail.message = serde_json::to_string(fail.data.as_deref().unwrap_or(&Value::Null)).unwrap_or_default();
            }
            Err(fail)
        }
    }
}

/// The RS256 JWT `gtoken` signs for the JWT bearer grant.
pub fn service_account_assertion(email: &str, pem: &str, iat_seconds: i64) -> Result<String, String> {
    let header = base64url(br#"{"alg":"RS256"}"#);
    let payload = json!({
        "iss": email,
        "scope": SCOPE,
        "aud": TOKEN_URL,
        "exp": iat_seconds + 3600,
        "iat": iat_seconds,
    });
    let payload = base64url(serde_json::to_string(&payload).unwrap_or_default().as_bytes());
    let input = format!("{header}.{payload}");
    let signature = sign_rs256(pem, input.as_bytes())?;
    Ok(format!("{input}.{}", base64url(&signature)))
}

fn sign_rs256(pem: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let der = crate::jsutil::atob(&body).ok_or_else(|| BAD_KEY.to_string())?;
    let pkcs1 = pem.contains("BEGIN RSA PRIVATE KEY");
    let key = if pkcs1 {
        ring::signature::RsaKeyPair::from_der(&der)
    } else {
        ring::signature::RsaKeyPair::from_pkcs8(&der)
    }
    .map_err(|_| BAD_KEY.to_string())?;
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        message,
        &mut signature,
    )
    .map_err(|_| BAD_KEY.to_string())?;
    Ok(signature)
}

/// `JWT.refreshTokenNoCache` through `gtoken`.
async fn service_account_token(transport: &dyn Transport, email: &str, key: &str) -> Result<String, Fail> {
    let iat = crate::options::now_ms().div_euclid(1000);
    let assertion = service_account_assertion(email, key, iat).map_err(Fail::new)?;
    let request = form_request(&[("grant_type", JWT_BEARER), ("assertion", &assertion)], true);
    match gaxios(transport, request, &AUTH_RETRY, ResponseType::Json).await {
        Ok(response) => Ok(token_field(&response.data, "access_token")),
        Err(mut fail) => {
            if let Some(data) = &fail.data
                && js_truthy(data.get("error"))
            {
                fail.message = format!(
                    "{}: {}",
                    js_string(data.get("error")),
                    js_string(data.get("error_description"))
                );
            }
            Err(fail)
        }
    }
}

/// `Compute.refreshTokenNoCache`.
async fn compute_token(env: Option<&ProviderEnv>, transport: &dyn Transport) -> Result<String, Fail> {
    let url = format!(
        "{}/instance/service-accounts/default/token?{}",
        metadata_base(env, None),
        form_urlencode(&[("scopes", SCOPE)])
    );
    match metadata_get(transport, url, 3, metadata_timeout(env)).await {
        Ok(data) => Ok(token_field(&data, "access_token")),
        Err(mut fail) if fail.name == "Error" => {
            fail.message = format!("Could not refresh access token: {}", fail.message);
            match fail.status {
                Some(403) => {
                    fail.message = format!(
                        "A Forbidden error was returned while attempting to retrieve an access token for the Compute Engine built-in service account. This may be because the Compute Engine instance does not have the correct permission scopes specified: {}",
                        fail.message
                    );
                }
                Some(404) => {
                    fail.message = format!(
                        "A Not Found error was returned while attempting to retrieve an accesstoken for the Compute Engine built-in service account. This may be because the Compute Engine instance does not have any permission scopes specified: {}",
                        fail.message
                    );
                }
                _ => {}
            }
            Err(fail)
        }
        Err(fail) => Err(fail),
    }
}

/// `Impersonated.refreshToken`.
async fn impersonated_token(
    env: Option<&ProviderEnv>,
    transport: &dyn Transport,
    source: &Credential,
    target: &str,
    delegates: &Value,
    universe: &str,
) -> Result<String, Fail> {
    let attempt = async {
        let source_token = Box::pin(access_token(source, env, transport)).await?;
        let mut headers: crate::http::HeaderMap = [
            ("user-agent".to_string(), AUTH_USER_AGENT.to_string()),
            ("content-type".to_string(), "application/json".to_string()),
            ("authorization".to_string(), format!("Bearer {source_token}")),
        ]
        .into();
        if let Some(quota) = source.quota() {
            headers.insert("x-goog-user-project".into(), quota);
        }
        let mut body = Map::new();
        body.insert("delegates".into(), delegates.clone());
        body.insert("scope".into(), json!([SCOPE]));
        body.insert("lifetime".into(), json!("3600s"));
        let request = HttpRequest {
            method: "POST".into(),
            url: format!(
                "https://iamcredentials.{universe}/v1/projects/-/serviceAccounts/{target}:generateAccessToken"
            ),
            headers,
            body: Value::Object(body).to_string(),
        };
        let response = gaxios(transport, request, &AUTH_RETRY, ResponseType::Unknown).await?;
        Ok(token_field(&response.data, "accessToken"))
    };
    attempt.await.map_err(|mut fail: Fail| {
        let error = fail.data.as_ref().and_then(|d| d.get("error"));
        let status = error.and_then(|e| e.get("status")).filter(|s| js_truthy(Some(s)));
        let message = error.and_then(|e| e.get("message")).filter(|m| js_truthy(Some(m)));
        fail.message = match (status, message) {
            (Some(status), Some(message)) => format!(
                "{}: unable to impersonate: {}",
                js_string(Some(status)),
                js_string(Some(message))
            ),
            _ => format!("unable to impersonate: {}: {}", fail.name, fail.message),
        };
        fail
    })
}

async fn access_token(
    credential: &Credential,
    env: Option<&ProviderEnv>,
    transport: &dyn Transport,
) -> Result<String, Fail> {
    match credential {
        Credential::User {
            client_id,
            client_secret,
            refresh_token,
            ..
        } => refresh_user(transport, client_id, client_secret, refresh_token).await,
        Credential::ServiceAccount { email, key, .. } => service_account_token(transport, email, key).await,
        Credential::Compute { .. } => compute_token(env, transport).await,
        Credential::Impersonated {
            source,
            target,
            delegates,
            universe,
            ..
        } => impersonated_token(env, transport, source, target, delegates, universe).await,
        Credential::External(client) => client.access_token(env, transport).await,
        Credential::ExternalUser(user) => user.access_token(transport).await,
        Credential::KeyFile { path, scoped } => Err(key_file_jwt_error(path, *scoped)),
    }
}

/// The key-file `JWT` fallback never yields a token: without scopes it self-signs with no key loaded; with scopes,
/// `gtoken` reads the key by extension and finds no usable key.
fn key_file_jwt_error(path: &Path, scoped: bool) -> Fail {
    if !scoped {
        return Fail::typed("TypeError", KEY_TYPE_ERROR);
    }
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    Fail::new(match extension {
        "json" => "private_key and client_email are required.",
        "der" | "crt" | "pem" => BAD_KEY,
        "p12" | "pfx" => {
            "*.p12 certificates are not supported after v6.1.2. Consider utilizing *.json format or converting *.p12 to *.pem using the OpenSSL CLI."
        }
        _ => {
            "Unknown certificate type. Type is determined based on file extension. Current supported extensions are *.json, and *.pem."
        }
    })
}

/// sha256 hex, for tests comparing a signature without storing it.
pub fn sha256_hex(text: &str) -> String {
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
