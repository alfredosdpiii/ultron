//! The AWS SDK v3 default credential chain the Bedrock provider relies on (`@aws-sdk/credential-provider-node`'s
//! `defaultProvider` and its sub-providers), without the SDK: environment keys, the shared `config`/`credentials`
//! files as `@smithy/core/config` parses them, profile static keys, assume role (source profile or credential
//! source), web identity (env and profile), `credential_process`, SSO (sso-session with token refresh, and legacy),
//! container credentials (`fromHttp`, `fromContainerMetadata`) and EC2 instance metadata (IMDSv2 with the v1
//! fallback). Each link fails with the SDK's message; a failure that does not allow the next link ends the chain, as
//! upstream.
//!
//! Settings the SDK reads from `process.env` are read through `provider_env_value` (the provider env, then the
//! process env), so a stored credential's env or a test can scope them. Every request goes through the provider's
//! `Transport`.

use crate::aws::{AwsCredentials, SignableRequest, escape_uri, sign_request};
use crate::env::{ProviderEnv, provider_env_value};
use crate::http::{HeaderMap, HttpRequest, Transport};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A failed link: `name` and `message` as upstream's error, and whether the chain moves on (`tryNextLink`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialError {
    pub name: String,
    pub message: String,
    pub try_next: bool,
    /// `$metadata.httpStatusCode` of a service error (STS).
    pub status: Option<u16>,
}

impl CredentialError {
    /// A `CredentialsProviderError` that lets the chain continue (upstream's default).
    fn next(message: impl Into<String>) -> Self {
        Self::provider(message, true)
    }

    /// A `CredentialsProviderError` that ends the chain.
    fn stop(message: impl Into<String>) -> Self {
        Self::provider(message, false)
    }

    fn provider(message: impl Into<String>, try_next: bool) -> Self {
        CredentialError {
            name: "CredentialsProviderError".into(),
            message: message.into(),
            try_next,
            status: None,
        }
    }

    /// A plain `Error` (or a `TypeError`): not a provider error, so it ends the chain.
    fn plain(name: &str, message: impl Into<String>) -> Self {
        CredentialError {
            name: name.into(),
            message: message.into(),
            try_next: false,
            status: None,
        }
    }
}

type Result<T> = std::result::Result<T, CredentialError>;

/// What the chain needs from the calling client.
pub struct ChainInput<'a> {
    pub env: Option<&'a ProviderEnv>,
    pub transport: &'a dyn Transport,
    /// The client's `profile` (`init.profile`); `None` falls back to `AWS_PROFILE`, then `default`.
    pub profile: Option<&'a str>,
    /// The calling client's region: STS uses it unless the profile names its own.
    pub region: &'a str,
    pub now_ms: i64,
}

impl ChainInput<'_> {
    fn var(&self, name: &str) -> Option<String> {
        provider_env_value(name, self.env)
    }

    /// `getProfileName`.
    fn profile_name(&self) -> String {
        self.profile
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .or_else(|| self.var("AWS_PROFILE"))
            .unwrap_or_else(|| "default".into())
    }
}

/// `defaultProvider(init)()`.
pub async fn default_credentials(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    let links: [Link; 5] = [Link::Env, Link::Ini, Link::Process, Link::TokenFile, Link::Remote];
    for link in links {
        let attempt = match link {
            Link::Env => from_env_link(input),
            Link::Ini => from_ini(input, &input.profile_name()).await,
            Link::Process => from_process(input, &input.profile_name()).await,
            Link::TokenFile => from_token_file(input, None).await,
            Link::Remote => remote_provider(input).await,
        };
        match attempt {
            Ok(credentials) => return Ok(credentials),
            Err(error) if error.try_next => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CredentialError::stop("Could not load credentials from any providers"))
}

#[derive(Clone, Copy)]
enum Link {
    Env,
    Ini,
    Process,
    TokenFile,
    Remote,
}

fn from_env_link(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    if input.profile.is_some_and(|p| !p.is_empty()) || input.var("AWS_PROFILE").is_some() {
        return Err(CredentialError::next("AWS_PROFILE is set, skipping fromEnv provider."));
    }
    from_env(input)
}

/// `fromEnv`.
fn from_env(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    match (input.var("AWS_ACCESS_KEY_ID"), input.var("AWS_SECRET_ACCESS_KEY")) {
        (Some(access_key_id), Some(secret_access_key)) => Ok(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: input.var("AWS_SESSION_TOKEN"),
        }),
        _ => Err(CredentialError::next(
            "Unable to find environment variable credentials.",
        )),
    }
}

// ------------------------------------------------------------------------------------------- shared config files

/// A profile's settings in file order.
pub type Section = IndexMap<String, String>;
pub type Profiles = BTreeMap<String, Section>;

const SECTION_TYPES: [&str; 3] = ["profile", "sso-session", "services"];

/// `[\w-@+.%:/]`: the name characters of a prefixed section header.
fn is_section_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '@' | '+' | '.' | '%' | ':' | '/')
}

/// `prefixKeyRegex`: `prefix name`, the name optionally quoted.
fn prefixed_section(header: &str) -> Option<(&str, &str)> {
    let (prefix, rest) = header.split_once(char::is_whitespace)?;
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let name = match rest.chars().next() {
        Some(q @ ('"' | '\'')) if rest.len() >= 2 && rest.ends_with(q) => &rest[1..rest.len() - 1],
        _ => rest,
    };
    (!name.is_empty() && name.chars().all(is_section_name_char)).then_some((prefix, name))
}

/// The part of a line before a comment: `split(/(^|\s)[;#]/)[0]`.
fn strip_comment(line: &str) -> &str {
    let mut previous_space = true;
    for (index, c) in line.char_indices() {
        if (c == ';' || c == '#') && previous_space {
            return if index == 0 {
                ""
            } else {
                &line[..index - line[..index].chars().last().map_or(0, char::len_utf8)]
            };
        }
        previous_space = c.is_whitespace();
    }
    line
}

/// `parseIni`: sections (`prefix.name` for the known prefixes), keys, and indented sub-sections as `sub.key`. A
/// `__proto__` profile makes the whole file unreadable, as upstream's thrown error is swallowed by its caller.
pub fn parse_ini(text: &str) -> Profiles {
    let mut map = Profiles::new();
    let mut section: Option<String> = None;
    let mut sub_section: Option<String> = None;
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let line = strip_comment(raw).trim();
        if line.len() >= 2 && line.starts_with('[') && line.ends_with(']') {
            section = None;
            sub_section = None;
            let name = &line[1..line.len() - 1];
            match prefixed_section(name) {
                Some((prefix, rest)) => {
                    if SECTION_TYPES.contains(&prefix) {
                        section = Some(format!("{prefix}.{rest}"));
                    }
                }
                None => section = Some(name.to_string()),
            }
            if name == "__proto__" || name == "profile __proto__" {
                return Profiles::new();
            }
            continue;
        }
        let Some(current) = &section else { continue };
        let Some(equals) = line.find('=').filter(|&i| i > 0) else {
            continue;
        };
        let key = line[..equals].trim();
        let value = line[equals + 1..].trim();
        if value.is_empty() {
            sub_section = Some(key.to_string());
            continue;
        }
        if sub_section.is_some() && raw.trim_start() == raw {
            sub_section = None;
        }
        let key = match &sub_section {
            Some(sub) => format!("{sub}.{key}"),
            None => key.to_string(),
        };
        map.entry(current.clone()).or_default().insert(key, value.to_string());
    }
    map
}

/// `getConfigData`: the config file's `[default]`, `[profile name]` as `name`, and other prefixed sections as is.
fn config_data(data: Profiles) -> Profiles {
    let mut out = Profiles::new();
    if let Some(default) = data.get("default") {
        out.insert("default".into(), default.clone());
    }
    for (key, value) in data {
        let Some((prefix, rest)) = key.split_once('.') else {
            continue;
        };
        if !SECTION_TYPES.contains(&prefix) {
            continue;
        }
        let name = if prefix == "profile" {
            rest.to_string()
        } else {
            key.clone()
        };
        out.insert(name, value);
    }
    out
}

/// `getHomeDir`.
pub fn home_dir(env: Option<&ProviderEnv>) -> PathBuf {
    provider_env_value("HOME", env)
        .or_else(|| provider_env_value("USERPROFILE", env))
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .unwrap_or_default()
}

fn config_path(env: Option<&ProviderEnv>) -> String {
    provider_env_value("AWS_CONFIG_FILE", env)
        .unwrap_or_else(|| home_dir(env).join(".aws/config").to_string_lossy().into_owned())
}

fn credentials_path(env: Option<&ProviderEnv>) -> String {
    provider_env_value("AWS_SHARED_CREDENTIALS_FILE", env)
        .unwrap_or_else(|| home_dir(env).join(".aws/credentials").to_string_lossy().into_owned())
}

fn expand_home(path: &str, env: Option<&ProviderEnv>) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home_dir(env).join(rest),
        None => PathBuf::from(path),
    }
}

fn read_ini(path: &Path) -> Profiles {
    std::fs::read_to_string(path).map(|t| parse_ini(&t)).unwrap_or_default()
}

/// `loadSharedConfigFiles`: the config file (through `getConfigData`) and the credentials file.
pub fn load_shared_files(env: Option<&ProviderEnv>) -> (Profiles, Profiles) {
    let config = config_data(read_ini(&expand_home(&config_path(env), env)));
    let credentials = read_ini(&expand_home(&credentials_path(env), env));
    (config, credentials)
}

/// `parseKnownFiles`: config then credentials, a credentials key replacing the config's.
pub fn parse_known_files(env: Option<&ProviderEnv>) -> Profiles {
    let (mut merged, credentials) = load_shared_files(env);
    for (name, section) in credentials {
        let entry = merged.entry(name).or_default();
        for (key, value) in section {
            entry.insert(key, value);
        }
    }
    merged
}

/// `loadSsoSessionData`: `[sso-session name]` sections of the config file (its path is not `~`-expanded).
fn load_sso_sessions(env: Option<&ProviderEnv>) -> Profiles {
    read_ini(Path::new(&config_path(env)))
        .into_iter()
        .filter_map(|(key, value)| key.strip_prefix("sso-session.").map(|name| (name.to_string(), value)))
        .collect()
}

/// A config-file-preferred setting of `profile` (`loadConfig` with `preferredFile: "config"`).
pub fn profile_config_value(profile: &str, key: &str, env: Option<&ProviderEnv>) -> Option<String> {
    let (config, credentials) = load_shared_files(env);
    config
        .get(profile)
        .and_then(|s| s.get(key))
        .or_else(|| credentials.get(profile).and_then(|s| s.get(key)))
        .cloned()
}

// ------------------------------------------------------------------------------------------------------ fromIni

fn has(section: Option<&Section>, key: &str) -> bool {
    section.is_some_and(|s| s.contains_key(key))
}

fn is_static_profile(section: Option<&Section>) -> bool {
    has(section, "aws_access_key_id") && has(section, "aws_secret_access_key")
}

fn is_assume_role_profile(section: Option<&Section>) -> bool {
    has(section, "role_arn")
        && ((has(section, "source_profile") && !has(section, "credential_source"))
            || (has(section, "credential_source") && !has(section, "source_profile")))
}

fn is_credential_source_without_role_arn(section: Option<&Section>) -> bool {
    !has(section, "role_arn")
        && section
            .and_then(|s| s.get("credential_source"))
            .is_some_and(|v| !v.is_empty())
}

fn is_sso_profile(section: Option<&Section>) -> bool {
    [
        "sso_start_url",
        "sso_account_id",
        "sso_session",
        "sso_region",
        "sso_role_name",
    ]
    .iter()
    .any(|key| has(section, key))
}

fn static_credentials(section: &Section) -> AwsCredentials {
    AwsCredentials {
        access_key_id: section["aws_access_key_id"].clone(),
        secret_access_key: section["aws_secret_access_key"].clone(),
        session_token: section.get("aws_session_token").cloned(),
    }
}

/// `fromIni`.
async fn from_ini(input: &ChainInput<'_>, profile: &str) -> Result<AwsCredentials> {
    let profiles = parse_known_files(input.env);
    resolve_profile_data(input, profile, &profiles, &mut Vec::new(), false).await
}

/// `resolveProfileData`.
fn resolve_profile_data<'a>(
    input: &'a ChainInput<'a>,
    name: &'a str,
    profiles: &'a Profiles,
    visited: &'a mut Vec<String>,
    assume_role_recursive: bool,
) -> crate::http::BoxFuture<'a, Result<AwsCredentials>> {
    Box::pin(async move {
        let data = profiles.get(name);
        if !visited.is_empty() && is_static_profile(data) {
            return Ok(static_credentials(data.unwrap_or(&Section::new())));
        }
        if assume_role_recursive || is_assume_role_profile(data) {
            return resolve_assume_role(input, name, profiles, visited).await;
        }
        if let Some(section) = data.filter(|s| is_static_profile(Some(s))) {
            return Ok(static_credentials(section));
        }
        if let Some(section) = data.filter(|s| s.contains_key("web_identity_token_file") && s.contains_key("role_arn"))
        {
            let config = WebIdentity {
                token_file: section.get("web_identity_token_file").cloned(),
                role_arn: section.get("role_arn").cloned(),
                session_name: section.get("role_session_name").cloned(),
            };
            return from_token_file(input, Some(config)).await;
        }
        if has(data, "credential_process") {
            return from_process(input, name).await;
        }
        if is_sso_profile(data) {
            return from_sso(input, name).await;
        }
        if has(data, "login_session") {
            return Err(CredentialError::stop(format!(
                "Profile {name} uses login_session, which the Rust port does not support; run aws configure export-credentials or use another credential source."
            )));
        }
        Err(CredentialError::next(format!(
            "Could not resolve credentials using profile: [{name}] in configuration/credentials file(s)."
        )))
    })
}

/// `resolveAssumeRoleCredentials`.
async fn resolve_assume_role(
    input: &ChainInput<'_>,
    name: &str,
    profiles: &Profiles,
    visited: &mut Vec<String>,
) -> Result<AwsCredentials> {
    let empty = Section::new();
    let data = profiles.get(name).unwrap_or(&empty);
    let source_profile = data.get("source_profile");
    if let Some(source) = source_profile
        && visited.contains(source)
    {
        return Err(CredentialError::next(format!(
            "Detected a cycle attempting to resolve credentials for profile {}. Profiles visited: {}",
            input.profile_name(),
            visited.join(", ")
        )));
    }
    let without_role = is_credential_source_without_role_arn(Some(data));
    if !without_role && let Some(serial) = data.get("mfa_serial").filter(|s| !s.is_empty()) {
        let _ = serial;
        return Err(CredentialError::stop(format!(
            "Profile {name} requires multi-factor authentication, but no MFA code callback was provided."
        )));
    }
    let source = match source_profile {
        Some(source) => {
            visited.push(source.clone());
            let recursive = is_credential_source_without_role_arn(Some(profiles.get(source).unwrap_or(&empty)));
            resolve_profile_data(input, source, profiles, visited, recursive).await?
        }
        None => {
            let credential_source = data.get("credential_source").cloned().unwrap_or_default();
            credential_source_credentials(input, &credential_source, name).await?
        }
    };
    if without_role {
        return Ok(source);
    }
    let region = data.get("region").cloned().unwrap_or_else(|| input.region.to_string());
    let mut params = vec![
        ("RoleArn", data.get("role_arn").cloned().unwrap_or_default()),
        (
            "RoleSessionName",
            data.get("role_session_name")
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("aws-sdk-js-{}", input.now_ms)),
        ),
        (
            "DurationSeconds",
            parse_int_prefix(data.get("duration_seconds").map_or("3600", String::as_str))
                .map_or_else(|| "NaN".into(), |n| n.to_string()),
        ),
    ];
    if let Some(external) = data.get("external_id") {
        params.push(("ExternalId", external.clone()));
    }
    let role_arn = params[0].1.clone();
    sts_call(
        input,
        &region,
        "AssumeRole",
        &params,
        Some(&source),
        &role_arn,
        "assumeRole",
    )
    .await
}

/// `parseInt(text, 10)`.
fn parse_int_prefix(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse::<i64>().ok().map(|n| sign * n)
}

/// `resolveCredentialSource`.
async fn credential_source_credentials(input: &ChainInput<'_>, source: &str, profile: &str) -> Result<AwsCredentials> {
    match source {
        "EcsContainer" => container_credentials(input).await,
        "Ec2InstanceMetadata" => from_instance_metadata(input).await,
        "Environment" => from_env(input),
        other => Err(CredentialError::next(format!(
            "Unsupported credential source in profile {profile}. Got {other}, expected EcsContainer or Ec2InstanceMetadata or Environment."
        ))),
    }
}

// ------------------------------------------------------------------------------------------------------ HTTP

struct Reply {
    status: u16,
    headers: HeaderMap,
    body: String,
}

async fn request(
    input: &ChainInput<'_>,
    method: &str,
    url: String,
    headers: HeaderMap,
    body: String,
    timeout: Option<Duration>,
) -> std::result::Result<Reply, String> {
    let exchange = async {
        let mut response = input
            .transport
            .send(HttpRequest {
                method: method.into(),
                url,
                headers,
                body,
            })
            .await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.body.next_chunk().await? {
            bytes.extend_from_slice(&chunk);
        }
        Ok(Reply {
            status: response.status,
            headers: response.headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        })
    };
    match timeout {
        Some(limit) => tokio::time::timeout(limit, exchange)
            .await
            .unwrap_or_else(|_| Err("TimeoutError".into())),
        None => exchange.await,
    }
}

/// `AWS_ENDPOINT_URL_<SERVICE>`, then `AWS_ENDPOINT_URL`.
fn configured_endpoint(input: &ChainInput<'_>, service: &str) -> Option<String> {
    input
        .var(&format!("AWS_ENDPOINT_URL_{service}"))
        .or_else(|| input.var("AWS_ENDPOINT_URL"))
        .map(|url| url.trim_end_matches('/').to_string())
}

fn regional_endpoint(prefix: &str, region: &str) -> String {
    let suffix = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    format!("https://{prefix}.{region}.{suffix}")
}

fn json_body(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

// ------------------------------------------------------------------------------------------------------ STS

/// The STS modeled errors (`awsQueryError` codes to exception names).
fn sts_error_name(code: &str) -> String {
    match code {
        "IDPCommunicationError" => "IDPCommunicationErrorException".into(),
        "IDPRejectedClaim" => "IDPRejectedClaimException".into(),
        "InvalidIdentityToken" => "InvalidIdentityTokenException".into(),
        "MalformedPolicyDocument" => "MalformedPolicyDocumentException".into(),
        "PackedPolicyTooLarge" => "PackedPolicyTooLargeException".into(),
        other => other.into(),
    }
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The text of the first `<tag>` element.
fn xml_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&format!("</{tag}>"))? + start;
    Some(xml_unescape(&text[start..end]))
}

/// One STS Query call returning `Credentials`; unsigned without `sign`.
async fn sts_call(
    input: &ChainInput<'_>,
    region: &str,
    action: &str,
    params: &[(&str, String)],
    sign: Option<&AwsCredentials>,
    role_arn: &str,
    label: &str,
) -> Result<AwsCredentials> {
    let endpoint = configured_endpoint(input, "STS").unwrap_or_else(|| regional_endpoint("sts", region));
    let mut body = format!("Action={action}&Version=2011-06-15");
    for (key, value) in params {
        body.push_str(&format!("&{key}={}", escape_uri(value)));
    }
    let url = format!("{endpoint}/");
    let mut headers = HeaderMap::new();
    headers.insert("content-type".into(), "application/x-www-form-urlencoded".into());
    if let Some(credentials) = sign {
        let parsed = reqwest::Url::parse(&url).map_err(|e| CredentialError::plain("TypeError", e.to_string()))?;
        let host = match (parsed.host_str(), parsed.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => String::new(),
        };
        let mut signable = SignableRequest {
            method: "POST".into(),
            path: parsed.path().to_string(),
            query: Vec::new(),
            headers: vec![
                ("content-type".into(), "application/x-www-form-urlencoded".into()),
                ("host".into(), host),
            ],
            body: body.as_bytes().to_vec(),
        };
        sign_request(&mut signable, credentials, region, "sts", input.now_ms);
        headers = signable
            .headers
            .into_iter()
            .filter(|(name, _)| name != "host")
            .collect();
    }
    let reply = request(input, "POST", url, headers, body, None)
        .await
        .map_err(|e| CredentialError::plain("Error", e))?;
    if !(200..300).contains(&reply.status) {
        let code = xml_tag(&reply.body, "Code").unwrap_or_else(|| "UnknownError".into());
        return Err(CredentialError {
            name: sts_error_name(&code),
            message: xml_tag(&reply.body, "Message").unwrap_or_else(|| "UnknownError".into()),
            try_next: false,
            status: Some(reply.status),
        });
    }
    let credentials = xml_tag(&reply.body, "Credentials").unwrap_or_default();
    match (
        xml_tag(&credentials, "AccessKeyId").filter(|s| !s.is_empty()),
        xml_tag(&credentials, "SecretAccessKey").filter(|s| !s.is_empty()),
    ) {
        (Some(access_key_id), Some(secret_access_key)) => Ok(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: xml_tag(&credentials, "SessionToken"),
        }),
        _ => Err(CredentialError::plain(
            "Error",
            format!("Invalid response from STS.{label} call with role {role_arn}"),
        )),
    }
}

// ------------------------------------------------------------------------------------------------ web identity

struct WebIdentity {
    token_file: Option<String>,
    role_arn: Option<String>,
    session_name: Option<String>,
}

/// Node's `fs` error message for a failed open.
fn fs_error(error: &std::io::Error, path: &str) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => format!("ENOENT: no such file or directory, open '{path}'"),
        std::io::ErrorKind::PermissionDenied => format!("EACCES: permission denied, open '{path}'"),
        std::io::ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".into(),
        _ => format!("{error}, open '{path}'"),
    }
}

/// `fromTokenFile`: the profile's settings, else `AWS_WEB_IDENTITY_TOKEN_FILE`/`AWS_ROLE_ARN`/`AWS_ROLE_SESSION_NAME`.
async fn from_token_file(input: &ChainInput<'_>, config: Option<WebIdentity>) -> Result<AwsCredentials> {
    let config = config.unwrap_or(WebIdentity {
        token_file: None,
        role_arn: None,
        session_name: None,
    });
    let token_file = config.token_file.or_else(|| input.var("AWS_WEB_IDENTITY_TOKEN_FILE"));
    let role_arn = config.role_arn.or_else(|| input.var("AWS_ROLE_ARN"));
    let session_name = config.session_name.or_else(|| input.var("AWS_ROLE_SESSION_NAME"));
    let (Some(token_file), Some(role_arn)) = (token_file.filter(|f| !f.is_empty()), role_arn.filter(|r| !r.is_empty()))
    else {
        return Err(CredentialError::next("Web identity configuration not specified"));
    };
    let token = std::fs::read(&token_file)
        .map_err(|e| CredentialError::plain("Error", fs_error(&e, &token_file)))?
        .into_iter()
        .map(|b| (b & 0x7f) as char)
        .collect::<String>();
    let params = [
        ("RoleArn", role_arn.clone()),
        (
            "RoleSessionName",
            session_name.unwrap_or_else(|| format!("aws-sdk-js-session-{}", input.now_ms)),
        ),
        ("WebIdentityToken", token),
    ];
    sts_call(
        input,
        input.region,
        "AssumeRoleWithWebIdentity",
        &params,
        None,
        &role_arn,
        "assumeRoleWithWebIdentity",
    )
    .await
}

// ------------------------------------------------------------------------------------------- credential_process

/// `fromProcess`: the profile's `credential_process` run by the shell, its JSON validated.
async fn from_process(input: &ChainInput<'_>, profile: &str) -> Result<AwsCredentials> {
    let profiles = parse_known_files(input.env);
    let Some(section) = profiles.get(profile) else {
        return Err(CredentialError::next(format!(
            "Profile {profile} could not be found in shared credentials file."
        )));
    };
    let Some(command) = section.get("credential_process").cloned() else {
        return Err(CredentialError::next(format!(
            "Profile {profile} did not contain credential_process."
        )));
    };
    let overlay: Vec<(String, String)> = input
        .env
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let shell = command.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&shell)
            .envs(overlay)
            .stdin(std::process::Stdio::null())
            .output()
    })
    .await
    .map_err(|e| CredentialError::next(e.to_string()))?
    .map_err(|e| CredentialError::next(e.to_string()))?;
    if !output.status.success() {
        return Err(CredentialError::next(format!(
            "Command failed: {command}\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let data: Value = serde_json::from_str(stdout.trim())
        .map_err(|_| CredentialError::next(format!("Profile {profile} credential_process returned invalid JSON.")))?;
    if data.get("Version").and_then(Value::as_f64) != Some(1.0) {
        return Err(CredentialError::next(format!(
            "Profile {profile} credential_process did not return Version 1."
        )));
    }
    let text = |key: &str| match data.get(key) {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    };
    let (Some(access_key_id), Some(secret_access_key)) = (text("AccessKeyId"), text("SecretAccessKey")) else {
        return Err(CredentialError::next(format!(
            "Profile {profile} credential_process returned invalid credentials."
        )));
    };
    if let Some(expiration) = data.get("Expiration").and_then(Value::as_str).filter(|e| !e.is_empty())
        && crate::model_refresh::parse_iso_ms(expiration).is_some_and(|ms| ms < input.now_ms)
    {
        return Err(CredentialError::next(format!(
            "Profile {profile} credential_process returned expired credentials."
        )));
    }
    Ok(AwsCredentials {
        access_key_id,
        secret_access_key,
        session_token: data
            .get("SessionToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

// ------------------------------------------------------------------------------------------------------ SSO

const SSO_REFRESH: &str = "To refresh this SSO session run aws sso login with the corresponding profile.";
const TOKEN_REFRESH: &str = "To refresh this SSO session run 'aws sso login' with the corresponding profile.";
const EXPIRE_WINDOW_MS: i64 = 5 * 60 * 1000;

/// `getSSOTokenFilepath`.
pub fn sso_token_path(id: &str, env: Option<&ProviderEnv>) -> PathBuf {
    home_dir(env)
        .join(".aws/sso/cache")
        .join(format!("{}.json", sha1_hex(id.as_bytes())))
}

fn read_sso_token(id: &str, env: Option<&ProviderEnv>) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(sso_token_path(id, env)).ok()?;
    match serde_json::from_str(&text).ok()? {
        Value::Object(map) => Some(map),
        _ => Some(Map::new()),
    }
}

/// `lastRefreshAttemptTimes`: one refresh attempt per sso-session every 30 seconds, process-wide.
fn last_refresh_attempts() -> &'static Mutex<HashMap<String, i64>> {
    static ATTEMPTS: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    ATTEMPTS.get_or_init(Default::default)
}

/// `fromSSO` for a profile.
async fn from_sso(input: &ChainInput<'_>, name: &str) -> Result<AwsCredentials> {
    let profiles = parse_known_files(input.env);
    let Some(section) = profiles.get(name) else {
        return Err(CredentialError::next(format!("Profile {name} was not found.")));
    };
    if !is_sso_profile(Some(section)) {
        return Err(CredentialError::next(format!(
            "Profile {name} is not configured with SSO credentials."
        )));
    }
    let mut profile: IndexMap<String, Option<String>> =
        section.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
    if let Some(session_name) = section.get("sso_session") {
        let sessions = load_sso_sessions(input.env);
        let Some(session) = sessions.get(session_name) else {
            return Err(CredentialError::plain(
                "TypeError",
                "Cannot read properties of undefined (reading 'sso_region')",
            ));
        };
        profile.insert("sso_region".into(), session.get("sso_region").cloned());
        profile.insert("sso_start_url".into(), session.get("sso_start_url").cloned());
    }
    let field = |key: &str| profile.get(key).cloned().flatten().filter(|v| !v.is_empty());
    let (Some(start_url), Some(account_id), Some(region), Some(role_name)) = (
        field("sso_start_url"),
        field("sso_account_id"),
        field("sso_region"),
        field("sso_role_name"),
    ) else {
        return Err(CredentialError::stop(format!(
            "Profile is configured with invalid SSO credentials. Required parameters \"sso_account_id\", \"sso_region\", \"sso_role_name\", \"sso_start_url\". Got {}\nReference: https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sso.html",
            profile.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    };
    let access_token = match field("sso_session") {
        Some(_) => sso_session_token(input, name).await.map_err(CredentialError::stop)?,
        None => {
            let token = read_sso_token(&start_url, input.env).ok_or_else(|| {
                CredentialError::stop(format!(
                    "The SSO session associated with this profile is invalid. {SSO_REFRESH}"
                ))
            })?;
            let expires = token
                .get("expiresAt")
                .and_then(Value::as_str)
                .and_then(crate::model_refresh::parse_iso_ms);
            if expires.is_some_and(|ms| ms - input.now_ms <= 0) {
                return Err(CredentialError::stop(format!(
                    "The SSO session associated with this profile has expired. {SSO_REFRESH}"
                )));
            }
            token
                .get("accessToken")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        }
    };
    let endpoint = configured_endpoint(input, "SSO").unwrap_or_else(|| regional_endpoint("portal.sso", &region));
    let url = format!(
        "{endpoint}/federation/credentials?account_id={}&role_name={}",
        escape_uri(&account_id),
        escape_uri(&role_name)
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-amz-sso_bearer_token".into(), access_token);
    let reply = request(input, "GET", url, headers, String::new(), None)
        .await
        .map_err(|e| CredentialError::stop(format!("Error: {e}")))?;
    let body = json_body(&reply.body);
    if !(200..300).contains(&reply.status) {
        let code = reply
            .headers
            .get("x-amzn-errortype")
            .map(|t| t.split(':').next().unwrap_or_default().to_string())
            .or_else(|| str_field(&body, "__type").map(|t| t.rsplit('#').next().unwrap_or_default().to_string()))
            .or_else(|| str_field(&body, "code"))
            .unwrap_or_else(|| "UnknownError".into());
        let message = str_field(&body, "message")
            .or_else(|| str_field(&body, "Message"))
            .unwrap_or_else(|| "UnknownError".into());
        return Err(CredentialError::stop(format!("{code}: {message}")));
    }
    let role = body.get("roleCredentials").cloned().unwrap_or(Value::Null);
    let present = |key: &str| role.get(key).filter(|v| !v.is_null() && *v != "" && *v != 0).cloned();
    match (
        present("accessKeyId"),
        present("secretAccessKey"),
        present("sessionToken"),
        present("expiration"),
    ) {
        (
            Some(Value::String(access_key_id)),
            Some(Value::String(secret_access_key)),
            Some(Value::String(token)),
            Some(_),
        ) => Ok(AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token: Some(token),
        }),
        _ => Err(CredentialError::stop("SSO returns an invalid temporary credential.")),
    }
}

/// `fromSso` (token-providers): the cached sso-session token, refreshed through SSO OIDC when it expires within five
/// minutes; the refreshed token is written back to the cache.
async fn sso_session_token(input: &ChainInput<'_>, profile_name: &str) -> std::result::Result<String, String> {
    let profiles = parse_known_files(input.env);
    let Some(profile) = profiles.get(profile_name) else {
        return Err(format!(
            "Profile '{profile_name}' could not be found in shared credentials file."
        ));
    };
    let Some(session_name) = profile.get("sso_session") else {
        return Err(format!(
            "Profile '{profile_name}' is missing required property 'sso_session'."
        ));
    };
    let sessions = load_sso_sessions(input.env);
    let Some(session) = sessions.get(session_name) else {
        return Err(format!(
            "Sso session '{session_name}' could not be found in shared credentials file."
        ));
    };
    for key in ["sso_start_url", "sso_region"] {
        if session.get(key).is_none_or(String::is_empty) {
            return Err(format!(
                "Sso session '{session_name}' is missing required property '{key}'."
            ));
        }
    }
    let region = session["sso_region"].clone();
    let Some(token) = read_sso_token(session_name, input.env) else {
        return Err(format!(
            "The SSO session token associated with profile={profile_name} was not found or is invalid. {TOKEN_REFRESH}"
        ));
    };
    let value_missing = |key: &str, refresh: bool| {
        token.get(key).is_none().then(|| {
            format!(
                "Value not present for '{key}' in SSO Token{}. {TOKEN_REFRESH}",
                if refresh { ". Cannot refresh" } else { "" }
            )
        })
    };
    for key in ["accessToken", "expiresAt"] {
        if let Some(message) = value_missing(key, false) {
            return Err(message);
        }
    }
    let access_token = js_string(&token["accessToken"]);
    let expiration = parse_js_date(&token["expiresAt"]);
    if expiration.is_some_and(|ms| ms - input.now_ms > EXPIRE_WINDOW_MS) {
        return Ok(access_token);
    }
    let expired = || Err(format!("Token is expired. {TOKEN_REFRESH}"));
    let validate_existing = || match expiration {
        Some(ms) if ms < input.now_ms => expired(),
        _ => Ok(access_token.clone()),
    };
    let last = last_refresh_attempts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_name)
        .copied()
        .unwrap_or(0);
    if input.now_ms - last < 30_000 {
        return validate_existing();
    }
    for key in ["clientId", "clientSecret", "refreshToken"] {
        if let Some(message) = value_missing(key, true) {
            return Err(message);
        }
    }
    last_refresh_attempts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_name.clone(), input.now_ms);
    match refresh_sso_token(input, &region, &token).await {
        Some((new_token, expires_in, refresh_token)) => {
            let expires_at = crate::claude_code_cli::iso_string((input.now_ms + expires_in * 1000) as f64);
            let mut updated = token.clone();
            updated.insert("accessToken".into(), Value::String(new_token.clone()));
            updated.insert("expiresAt".into(), Value::String(expires_at));
            match refresh_token {
                Some(refresh) => {
                    updated.insert("refreshToken".into(), refresh);
                }
                None => {
                    updated.shift_remove("refreshToken");
                }
            }
            if let Ok(text) = serde_json::to_string_pretty(&Value::Object(updated)) {
                let _ = std::fs::write(sso_token_path(session_name, input.env), text);
            }
            Ok(new_token)
        }
        None => validate_existing(),
    }
}

/// SSO OIDC `CreateToken` with the refresh token: the new access token, its lifetime, and the new refresh token.
async fn refresh_sso_token(
    input: &ChainInput<'_>,
    region: &str,
    token: &Map<String, Value>,
) -> Option<(String, i64, Option<Value>)> {
    let endpoint = configured_endpoint(input, "SSO_OIDC").unwrap_or_else(|| regional_endpoint("oidc", region));
    let mut body = Map::new();
    body.insert("clientId".into(), token["clientId"].clone());
    body.insert("clientSecret".into(), token["clientSecret"].clone());
    body.insert("grantType".into(), Value::String("refresh_token".into()));
    body.insert("refreshToken".into(), token["refreshToken"].clone());
    let mut headers = HeaderMap::new();
    headers.insert("content-type".into(), "application/json".into());
    let reply = request(
        input,
        "POST",
        format!("{endpoint}/token"),
        headers,
        Value::Object(body).to_string(),
        None,
    )
    .await
    .ok()?;
    if !(200..300).contains(&reply.status) {
        return None;
    }
    let response = json_body(&reply.body);
    let access_token = response.get("accessToken").filter(|v| !v.is_null())?;
    let expires_in = response.get("expiresIn").and_then(Value::as_f64)?;
    let refresh = response.get("refreshToken").filter(|v| !v.is_null()).cloned();
    Some((js_string(access_token), expires_in as i64, refresh))
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_js_date(value: &Value) -> Option<i64> {
    match value {
        Value::String(s) => crate::model_refresh::parse_iso_ms(s),
        Value::Number(n) => n.as_f64().map(|f| f as i64),
        _ => None,
    }
}

// ----------------------------------------------------------------------------------------- container and IMDS

const METADATA_TIMEOUT: Duration = Duration::from_millis(1000);

fn is_imds_credentials(value: &Value) -> Option<AwsCredentials> {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    text("Expiration")?;
    Some(AwsCredentials {
        access_key_id: text("AccessKeyId")?,
        secret_access_key: text("SecretAccessKey")?,
        session_token: Some(text("Token")?),
    })
}

/// `remoteProvider`: container credentials when a container URI is set, else instance metadata unless disabled.
async fn remote_provider(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    if input.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
        || input.var("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
    {
        return container_credentials(input).await;
    }
    if input.var("AWS_EC2_METADATA_DISABLED").is_some_and(|v| v != "false") {
        return Err(CredentialError::next("EC2 Instance Metadata Service access disabled"));
    }
    from_instance_metadata(input).await
}

/// `checkUrl` of `fromHttp`.
fn http_url_accepted(url: &reqwest::Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    let host = url.host_str().unwrap_or_default();
    if [
        "169.254.170.2",
        "169.254.170.23",
        "[fd00:ec2::23]",
        "[::1]",
        "localhost",
    ]
    .contains(&host)
    {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4 && parts[0] == "127" && parts[1..].iter().all(|p| p.parse::<u8>().is_ok())
}

/// The setup of `fromHttp`: the container endpoint URL, checked. Upstream builds it before the provider chain runs,
/// so its failure is the whole remote link's.
fn http_target(input: &ChainInput<'_>) -> Result<reqwest::Url> {
    let host = match (
        input.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"),
        input.var("AWS_CONTAINER_CREDENTIALS_FULL_URI"),
    ) {
        (Some(relative), _) => format!("http://169.254.170.2{relative}"),
        (None, Some(full)) => full,
        (None, None) => {
            return Err(CredentialError::next(
                "No HTTP credential provider host provided.\nSet AWS_CONTAINER_CREDENTIALS_FULL_URI or AWS_CONTAINER_CREDENTIALS_RELATIVE_URI.",
            ));
        }
    };
    let url = reqwest::Url::parse(&host).map_err(|_| CredentialError::plain("TypeError", "Invalid URL"))?;
    if !http_url_accepted(&url) {
        return Err(CredentialError::next(
            "URL not accepted. It must either be HTTPS or match one of the following:\n  - loopback CIDR 127.0.0.0/8 or [::1/128]\n  - ECS container host 169.254.170.2\n  - EKS container host 169.254.170.23 or [fd00:ec2::23]",
        ));
    }
    Ok(url)
}

/// `chain(fromHttp, fromContainerMetadata)`.
async fn container_credentials(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    let url = http_target(input)?;
    match from_http(input, &url).await {
        Ok(credentials) => Ok(credentials),
        Err(error) if error.try_next => from_container_metadata(input).await,
        Err(error) => Err(error),
    }
}

/// `fromHttp`: the container credentials endpoint, retried three times a second apart.
async fn from_http(input: &ChainInput<'_>, url: &reqwest::Url) -> Result<AwsCredentials> {
    let token_file = input.var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE");
    let token = input.var("AWS_CONTAINER_AUTHORIZATION_TOKEN");
    let mut last = None;
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(METADATA_TIMEOUT).await;
        }
        let mut headers = HeaderMap::new();
        let authorization = match &token_file {
            Some(path) => {
                Some(std::fs::read_to_string(path).map_err(|e| CredentialError::plain("Error", fs_error(&e, path)))?)
            }
            None => token.clone(),
        };
        if let Some(authorization) = authorization {
            if authorization.contains("\r\n") {
                last = Some(CredentialError::next(
                    "Authorization token contains invalid \\r\\n sequence.",
                ));
                continue;
            }
            headers.insert("authorization".into(), authorization);
        }
        match request(
            input,
            "GET",
            url.to_string(),
            headers,
            String::new(),
            Some(METADATA_TIMEOUT),
        )
        .await
        {
            Ok(reply) if reply.status == 200 => {
                return is_imds_credentials(&json_body(&reply.body)).ok_or_else(|| {
                    CredentialError::next(
                        "HTTP credential provider response not of the required format, an object matching: { AccessKeyId: string, SecretAccessKey: string, Token: string, Expiration: string(rfc3339) }",
                    )
                });
            }
            Ok(reply) => {
                last = Some(CredentialError::next(format!(
                    "CredentialsProviderError: Server responded with status: {}",
                    reply.status
                )));
            }
            Err(error) => last = Some(CredentialError::next(format!("Error: {error}"))),
        }
    }
    Err(last.unwrap_or_else(|| CredentialError::next("Could not load credentials from any providers")))
}

/// `fromContainerMetadata`.
async fn from_container_metadata(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    let url = if let Some(relative) = input.var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        format!("http://169.254.170.2{relative}")
    } else if let Some(full) = input.var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
        let parsed = reqwest::Url::parse(&full)
            .map_err(|_| CredentialError::stop(format!("{full} is not a valid container metadata service URL")))?;
        let host = parsed.host_str().unwrap_or_default();
        if host != "localhost" && host != "127.0.0.1" {
            return Err(CredentialError::stop(format!(
                "{host} is not a valid container metadata service hostname"
            )));
        }
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(CredentialError::stop(format!(
                "{}: is not a valid container metadata service protocol",
                parsed.scheme()
            )));
        }
        parsed.to_string()
    } else {
        return Err(CredentialError::stop(
            "The container metadata credential provider cannot be used unless the AWS_CONTAINER_CREDENTIALS_RELATIVE_URI or AWS_CONTAINER_CREDENTIALS_FULL_URI environment variable is set",
        ));
    };
    let mut headers = HeaderMap::new();
    if let Some(token) = input.var("AWS_CONTAINER_AUTHORIZATION_TOKEN") {
        headers.insert("authorization".into(), token);
    }
    let body = metadata_get(input, "GET", url, headers).await?;
    is_imds_credentials(&json_body(&body))
        .ok_or_else(|| CredentialError::next("Invalid response received from instance metadata service."))
}

/// `httpRequest` of the metadata providers: a 1 s timeout, and non-2xx as a provider error.
async fn metadata_get(input: &ChainInput<'_>, method: &str, url: String, headers: HeaderMap) -> Result<String> {
    match request(input, method, url, headers, String::new(), Some(METADATA_TIMEOUT)).await {
        Ok(reply) if (200..300).contains(&reply.status) => Ok(reply.body),
        Ok(reply) => Err(CredentialError {
            name: "ProviderError".into(),
            message: "Error response received from instance metadata service".into(),
            try_next: true,
            status: Some(reply.status),
        }),
        Err(error) if error == "TimeoutError" => Err(CredentialError {
            name: "ProviderError".into(),
            message: "TimeoutError from instance metadata service".into(),
            try_next: true,
            status: None,
        }),
        Err(_) => Err(CredentialError {
            name: "ProviderError".into(),
            message: "Unable to connect to instance metadata service".into(),
            try_next: true,
            status: None,
        }),
    }
}

const IMDS_PATH: &str = "/latest/meta-data/iam/security-credentials/";

/// `fromInstanceMetadata`: IMDSv2 (a session token first), falling back to v1 unless that is disabled.
async fn from_instance_metadata(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    let profile = input.profile_name();
    let endpoint = match input
        .var("AWS_EC2_METADATA_SERVICE_ENDPOINT")
        .or_else(|| profile_config_value(&profile, "ec2_metadata_service_endpoint", input.env))
    {
        Some(endpoint) => endpoint,
        None => {
            let mode = input
                .var("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE")
                .or_else(|| profile_config_value(&profile, "ec2_metadata_service_endpoint_mode", input.env))
                .unwrap_or_else(|| "IPv4".into());
            match mode.as_str() {
                "IPv4" => "http://169.254.169.254".into(),
                "IPv6" => "http://[fd00:ec2::254]".into(),
                other => {
                    return Err(CredentialError::plain(
                        "Error",
                        format!("Unsupported endpoint mode: {other}. Select from IPv4,IPv6"),
                    ));
                }
            }
        }
    };
    let base = reqwest::Url::parse(&endpoint)
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|_| endpoint.trim_end_matches('/').to_string());
    let mut token_headers = HeaderMap::new();
    token_headers.insert("x-aws-ec2-metadata-token-ttl-seconds".into(), "21600".into());
    let token = match metadata_get(input, "PUT", format!("{base}/latest/api/token"), token_headers).await {
        Ok(token) => Some(token),
        Err(error) if error.status == Some(400) => {
            return Err(CredentialError {
                message: "EC2 Metadata token request returned error".into(),
                ..error
            });
        }
        Err(_) => None,
    };
    let mut headers = HeaderMap::new();
    match token {
        Some(token) => {
            headers.insert("x-aws-ec2-metadata-token".into(), token);
        }
        None => {
            let env_value = input.var("AWS_EC2_METADATA_V1_DISABLED");
            let (blocked, cause) = match env_value {
                Some(value) => (
                    value != "false",
                    "process environment variable (AWS_EC2_METADATA_V1_DISABLED)",
                ),
                None => (
                    profile_config_value(&profile, "ec2_metadata_v1_disabled", input.env)
                        .is_some_and(|v| !v.is_empty() && v != "false"),
                    "config file profile (ec2_metadata_v1_disabled)",
                ),
            };
            if blocked {
                return Err(CredentialError {
                    name: "InstanceMetadataV1FallbackError".into(),
                    message: format!(
                        "AWS EC2 Metadata v1 fallback has been blocked by AWS SDK configuration in the following: [{cause}]."
                    ),
                    try_next: true,
                    status: None,
                });
            }
        }
    }
    let role = metadata_get(input, "GET", format!("{base}{IMDS_PATH}"), headers.clone()).await?;
    let body = metadata_get(input, "GET", format!("{base}{IMDS_PATH}{}", role.trim()), headers).await?;
    is_imds_credentials(&json_body(&body))
        .ok_or_else(|| CredentialError::next("Invalid response received from instance metadata service."))
}

// ------------------------------------------------------------------------------------------------------ SHA-1

/// SHA-1 hex digest (the SSO cache file name).
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}
