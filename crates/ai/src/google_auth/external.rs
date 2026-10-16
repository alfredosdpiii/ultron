//! Workload identity federation as google-auth-library 10.6.2 runs it: `external_account` credentials
//! (`BaseExternalAccountClient` with an identity-pool file, URL or certificate source, an AWS source, or a pluggable
//! executable)
//! exchange a subject token at the STS endpoint, optionally impersonating a service account afterwards;
//! `external_account_authorized_user` credentials redeem a refresh token at STS. A service account outside the default
//! universe signs its own JWT (`JWTAccess`), which lives here too.
//!
//! Constructor checks and messages are upstream's, in upstream's order; `fixtures/generate-vertex-adc.mjs` records them.
//! Environment lookups go through the provider env first, like the rest of `google_auth.rs`.

use super::{
    AUTH_RETRY, AUTH_USER_AGENT, DEFAULT_UNIVERSE, Fail, NO_RETRY, ResponseType, SCOPE, absolute, gaxios, io_message,
    js_string, js_truthy, realpath, sign_rs256,
};
use crate::env::{ProviderEnv, provider_env_value};
use crate::http::{HeaderMap, HttpRequest, Transport};
use crate::jsutil::{btoa, json_parse_error};
use crate::oauth_flow::{base64url, form_urlencode};
use serde_json::{Map, Value, json};
use std::time::Duration;

const STS_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const STS_REQUEST_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const DEFAULT_TOKEN_LIFESPAN: &str = "3600";
const WORKFORCE_AUDIENCE_PREFIX: &str = "//iam.googleapis.com/locations/";
const SAML_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:saml2";
const OIDC_TOKEN_TYPE_ID: &str = "urn:ietf:params:oauth:token-type:id_token";
const OIDC_TOKEN_TYPE_JWT: &str = "urn:ietf:params:oauth:token-type:jwt";
const MIN_EXECUTABLE_TIMEOUT_MS: f64 = 5_000.0;
const MAX_EXECUTABLE_TIMEOUT_MS: f64 = 120_000.0;
const DEFAULT_EXECUTABLE_TIMEOUT_MS: f64 = 30_000.0;
const NO_SOURCE: &str =
    "No valid Identity Pool \"credential_source\" provided, must be either file, url, or certificate.";
const NO_EXECUTABLE_SOURCE: &str = "No valid Pluggable Auth \"credential_source\" provided.";

/// `originalOrCamelOptions(obj).get(key)`: the snake-case key, else its camel-case spelling, skipping null.
pub(super) fn camel_option<'a>(json: &'a Value, key: &str) -> Option<&'a Value> {
    let non_null = |v: &&Value| !v.is_null();
    json.get(key).filter(non_null).or_else(|| {
        let mut camel = String::new();
        let mut upper = false;
        for c in key.chars() {
            if c == '_' && !upper {
                upper = true;
            } else if upper {
                camel.extend(c.to_uppercase());
                upper = false;
            } else {
                camel.push(c);
            }
        }
        if upper {
            camel.push('_');
        }
        json.get(&camel).filter(non_null)
    })
}

fn truthy_string(json: &Value, key: &str) -> Option<String> {
    camel_option(json, key)
        .filter(|v| js_truthy(Some(v)))
        .map(|v| js_string(Some(v)))
}

/// JavaScript's `Number(value)` for the JSON values a config holds.
fn js_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse().unwrap_or(f64::NAN)
            }
        }
        _ => f64::NAN,
    }
}

/// `encodeURIComponent`.
fn encode_uri_component(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------- external_account

#[derive(Clone, Debug)]
enum Format {
    Text,
    Json(String),
}

#[derive(Clone, Debug)]
enum Source {
    File {
        path: String,
        format: Format,
    },
    Url {
        url: String,
        headers: HeaderMap,
        format: Format,
    },
    /// X.509 workload identity: the chain is the subject token, and STS is reached over mTLS.
    Certificate(super::certificate::CertificateSource),
    Aws {
        region_url: Option<String>,
        url: Option<String>,
        imdsv2_url: Option<String>,
        verification_url: String,
    },
    Executable {
        command: Vec<String>,
        timeout_ms: f64,
        output_file: Option<String>,
    },
}

/// An `external_account` client (`BaseExternalAccountClient` and its three subclasses).
#[derive(Clone, Debug)]
pub(crate) struct External {
    pub(super) universe: String,
    pub(super) quota: Option<String>,
    token_url: String,
    /// `clientAuth`: Basic authentication at STS.
    client: Option<(String, String)>,
    audience: String,
    subject_token_type: Option<String>,
    workforce_pool_user_project: Option<String>,
    impersonation_url: Option<String>,
    lifetime: String,
    source: Source,
}

impl External {
    /// `ExternalAccountClient.fromJSON`: the constructor checks of the base client, then of the chosen subclass.
    pub(super) fn from_json(json: &Value) -> Result<External, Fail> {
        let universe =
            camel_option(json, "universe_domain").map_or_else(|| DEFAULT_UNIVERSE.into(), |v| js_string(Some(v)));
        let token_url = camel_option(json, "token_url")
            .map_or_else(|| format!("https://sts.{universe}/v1/token"), |v| js_string(Some(v)));
        let client = truthy_string(json, "client_id").map(|id| {
            let secret = truthy_string(json, "client_secret").unwrap_or_default();
            (id, secret)
        });
        let audience = camel_option(json, "audience");
        let workforce_pool_user_project = truthy_string(json, "workforce_pool_user_project");
        let read_audience = || match audience {
            Some(Value::String(s)) => Ok(s.clone()),
            None => Err(Fail::typed(
                "TypeError",
                "Cannot read properties of undefined (reading 'match')",
            )),
            Some(_) => Err(Fail::typed("TypeError", "this.audience.match is not a function")),
        };
        if workforce_pool_user_project.is_some() {
            let audience = read_audience()?;
            if !is_workforce_audience(&audience) {
                return Err(Fail::new(
                    "workforcePoolUserProject should not be set for non-workforce pool credentials.",
                ));
            }
        }
        let audience = read_audience()?;
        let lifetime = camel_option(json, "service_account_impersonation")
            .and_then(|v| camel_option(v, "token_lifetime_seconds"))
            .filter(|v| js_truthy(Some(v)))
            .map_or_else(|| DEFAULT_TOKEN_LIFESPAN.to_string(), |v| js_string(Some(v)));
        let source = credential_source(json)?;
        Ok(External {
            universe,
            quota: truthy_string(json, "quota_project_id"),
            token_url,
            client,
            audience,
            subject_token_type: camel_option(json, "subject_token_type").map(|v| js_string(Some(v))),
            workforce_pool_user_project,
            impersonation_url: truthy_string(json, "service_account_impersonation_url"),
            lifetime,
            source,
        })
    }

    /// `#internalRefreshAccessTokenAsync`: the subject token, the STS exchange, then the optional impersonation.
    pub(super) async fn access_token(
        &self,
        env: Option<&ProviderEnv>,
        transport: &dyn Transport,
    ) -> Result<String, Fail> {
        // `retrieveSubjectToken`: a certificate source's exchange and impersonation go through a client presenting the
        // workload's certificate and key (`createMtlsHttpsAgent`).
        let (subject_token, mtls) = match &self.source {
            Source::Certificate(source) => {
                let token = source.subject_token(env)?;
                let mtls = transport
                    .with_client_identity(&token.cert, &token.key)
                    .map_err(Fail::new)?;
                (token.token, Some(mtls))
            }
            _ => (self.subject_token(env, transport).await?, None),
        };
        let transport: &dyn Transport = mtls.as_deref().unwrap_or(transport);
        let scope = SCOPE.to_string();
        let mut fields: Vec<(&str, &str)> = vec![
            ("grant_type", STS_GRANT_TYPE),
            ("audience", &self.audience),
            ("scope", &scope),
            ("requested_token_type", STS_REQUEST_TOKEN_TYPE),
            ("subject_token", &subject_token),
        ];
        if let Some(kind) = &self.subject_token_type {
            fields.push(("subject_token_type", kind));
        }
        let options = match (&self.client, &self.workforce_pool_user_project) {
            (None, Some(project)) => Some(json!({ "userProject": project }).to_string()),
            _ => None,
        };
        if let Some(options) = &options {
            fields.push(("options", options));
        }
        // StsCredentials runs on its own Gaxios: no auth-library user agent.
        let mut headers: HeaderMap = [
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded;charset=UTF-8".to_string(),
            ),
            ("accept".to_string(), "application/json".to_string()),
        ]
        .into();
        if let Some((id, secret)) = &self.client {
            headers.insert("authorization".into(), basic(id, secret));
        }
        let request = HttpRequest {
            method: "POST".into(),
            url: self.token_url.clone(),
            headers,
            body: form_urlencode(&fields),
        };
        let token = oauth_exchange(transport, request).await?;
        let Some(url) = &self.impersonation_url else {
            return Ok(token);
        };
        // `getImpersonatedAccessToken`. After a certificate source the client's transporter is a new `Gaxios` with
        // the mTLS agent, without the auth library's user-agent interceptor.
        let mut headers: HeaderMap = [
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "application/json".to_string()),
            ("authorization".to_string(), format!("Bearer {token}")),
        ]
        .into();
        if mtls.is_none() {
            headers.insert("user-agent".into(), AUTH_USER_AGENT.into());
        }
        let request = HttpRequest {
            method: "POST".into(),
            url: url.clone(),
            headers,
            body: json!({ "scope": [SCOPE], "lifetime": format!("{}s", self.lifetime) }).to_string(),
        };
        let response = gaxios(transport, request, &AUTH_RETRY, ResponseType::Json).await?;
        Ok(js_string(response.data.get("accessToken")))
    }

    async fn subject_token(&self, env: Option<&ProviderEnv>, transport: &dyn Transport) -> Result<String, Fail> {
        match &self.source {
            Source::File { path, format } => file_subject_token(path, format),
            Source::Url { url, headers, format } => url_subject_token(transport, url, headers, format).await,
            Source::Certificate(source) => source.subject_token(env).map(|t| t.token),
            Source::Aws {
                region_url,
                url,
                imdsv2_url,
                verification_url,
            } => {
                let aws = AwsSource {
                    region_url: region_url.as_deref(),
                    url: url.as_deref(),
                    imdsv2_url: imdsv2_url.as_deref(),
                };
                let region = aws.region(env, transport).await?;
                let credentials = aws.credentials(env, transport).await?;
                let url = verification_url.replacen("{region}", &region, 1);
                aws_subject_token(&url, &region, &credentials, &self.audience, crate::options::now_ms())
                    .map_err(|message| Fail::typed("TypeError", message))
            }
            Source::Executable {
                command,
                timeout_ms,
                output_file,
            } => {
                self.executable_token(env, command, *timeout_ms, output_file.as_deref())
                    .await
            }
        }
    }
}

/// `//iam.googleapis.com/locations/[^/]+/workforcePools/[^/]+/providers/.+` anywhere in the audience.
fn is_workforce_audience(audience: &str) -> bool {
    audience.match_indices(WORKFORCE_AUDIENCE_PREFIX).any(|(at, _)| {
        let rest = &audience[at + WORKFORCE_AUDIENCE_PREFIX.len()..];
        let Some((location, rest)) = rest.split_once('/') else {
            return false;
        };
        let Some(rest) = rest.strip_prefix("workforcePools/") else {
            return false;
        };
        let Some((pool, rest)) = rest.split_once('/') else {
            return false;
        };
        let provider = rest.strip_prefix("providers/").unwrap_or_default();
        !location.is_empty() && !pool.is_empty() && !provider.is_empty()
    })
}

fn basic(id: &str, secret: &str) -> String {
    format!("Basic {}", btoa(format!("{id}:{secret}").as_bytes()))
}

/// The subclass `ExternalAccountClient.fromJSON` picks, with its constructor checks.
fn credential_source(json: &Value) -> Result<Source, Fail> {
    let source = json.get("credential_source").filter(|v| !v.is_null());
    if source.is_some_and(|s| js_truthy(s.get("environment_id"))) {
        return aws_source(source.unwrap_or(&Value::Null));
    }
    if source.is_some_and(|s| js_truthy(s.get("executable"))) {
        return executable_source(source.unwrap_or(&Value::Null));
    }
    // `IdentityPoolClient`.
    let Some(source) = camel_option(json, "credential_source").filter(|v| js_truthy(Some(v))) else {
        return Err(Fail::new(
            "A credential source or subject token supplier must be specified.",
        ));
    };
    let format_opts = camel_option(source, "format").cloned().unwrap_or(Value::Null);
    let format_type = truthy_string(&format_opts, "type").unwrap_or_else(|| "text".into());
    if format_type != "json" && format_type != "text" {
        return Err(Fail::new(format!("Invalid credential_source format \"{format_type}\"")));
    }
    let field = truthy_string(&format_opts, "subject_token_field_name");
    let format = match (format_type.as_str(), field) {
        ("json", Some(field)) => Format::Json(field),
        ("json", None) => {
            return Err(Fail::new(
                "Missing subject_token_field_name for JSON credential_source format",
            ));
        }
        _ => Format::Text,
    };
    let file = truthy_string(source, "file");
    let url = truthy_string(source, "url");
    let certificate = camel_option(source, "certificate").filter(|v| js_truthy(Some(v)));
    let set = [file.is_some(), url.is_some(), certificate.is_some()];
    if set.iter().filter(|s| **s).count() > 1 {
        return Err(Fail::new(NO_SOURCE));
    }
    if let Some(path) = file {
        return Ok(Source::File { path, format });
    }
    if let Some(url) = url {
        let headers = match camel_option(source, "headers") {
            Some(Value::Object(map)) => map
                .iter()
                .map(|(k, v)| (k.to_lowercase(), js_string(Some(v))))
                .collect(),
            _ => HeaderMap::new(),
        };
        return Ok(Source::Url { url, headers, format });
    }
    if let Some(certificate) = certificate {
        // `CertificateSubjectTokenSupplier`'s constructor.
        return super::certificate::CertificateSource::from_json(certificate).map(Source::Certificate);
    }
    Err(Fail::new(NO_SOURCE))
}

// ------------------------------------------------------------------------------------------ identity pool sources

/// `FileSubjectTokenSupplier.getSubjectToken`.
fn file_subject_token(path: &str, format: &Format) -> Result<String, Fail> {
    let not_a_file = |shown: &str, inner: &str| {
        Fail::new(format!(
            "The file at {shown} does not exist, or it is not a file. {inner}"
        ))
    };
    let real = realpath(&absolute(path)).map_err(|e| not_a_file(path, &e.to_string()))?;
    let shown = real.display().to_string();
    match std::fs::symlink_metadata(&real) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(not_a_file(&shown, "")),
        Err(e) => return Err(not_a_file(&shown, &io_message(&e, "lstat", &real))),
    }
    let bytes = std::fs::read(&real).map_err(|e| Fail::new(io_message(&e, "open", &real)))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let token = match format {
        Format::Text => Value::String(text),
        Format::Json(field) => {
            if let Some(message) = json_parse_error(&text) {
                return Err(Fail::typed("SyntaxError", message));
            }
            let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            json.get(field).cloned().unwrap_or(Value::Null)
        }
    };
    if !js_truthy(Some(&token)) {
        return Err(Fail::new(
            "Unable to parse the subject_token from the credential_source file",
        ));
    }
    Ok(js_string(Some(&token)))
}

/// `UrlSubjectTokenSupplier.getSubjectToken`.
async fn url_subject_token(
    transport: &dyn Transport,
    url: &str,
    headers: &HeaderMap,
    format: &Format,
) -> Result<String, Fail> {
    let mut headers = headers.clone();
    headers.insert("user-agent".into(), AUTH_USER_AGENT.into());
    let kind = match format {
        Format::Text => ResponseType::Text,
        Format::Json(_) => {
            headers.insert("accept".into(), "application/json".into());
            ResponseType::Json
        }
    };
    let request = HttpRequest {
        method: "GET".into(),
        url: url.into(),
        headers,
        body: String::new(),
    };
    let data = gaxios(transport, request, &AUTH_RETRY, kind).await?.data;
    let token = match format {
        Format::Text => data,
        Format::Json(field) => data.get(field).cloned().unwrap_or(Value::Null),
    };
    if !js_truthy(Some(&token)) {
        return Err(Fail::new(
            "Unable to parse the subject_token from the credential_source URL",
        ));
    }
    Ok(js_string(Some(&token)))
}

// -------------------------------------------------------------------------------------------------------- AWS

fn aws_source(source: &Value) -> Result<Source, Fail> {
    let environment_id = truthy_string(source, "environment_id").unwrap_or_default();
    let verification_url = truthy_string(source, "regional_cred_verification_url");
    // `/^(aws)(\d+)$/`
    let version = environment_id
        .strip_prefix("aws")
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()));
    let (Some(version), Some(verification_url)) = (version, verification_url) else {
        return Err(Fail::new("No valid AWS \"credential_source\" provided"));
    };
    if version.trim_start_matches('0') != "1" {
        return Err(Fail::new(format!(
            "aws version \"{version}\" is not supported in the current build."
        )));
    }
    Ok(Source::Aws {
        region_url: truthy_string(source, "region_url"),
        url: truthy_string(source, "url"),
        imdsv2_url: truthy_string(source, "imdsv2_session_token_url"),
        verification_url,
    })
}

/// AWS security credentials (`AwsSecurityCredentials`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub token: Option<String>,
}

/// `DefaultAwsSecurityCredentialsSupplier`: the environment, else the EC2 metadata endpoints the config names.
struct AwsSource<'a> {
    region_url: Option<&'a str>,
    url: Option<&'a str>,
    imdsv2_url: Option<&'a str>,
}

impl AwsSource<'_> {
    async fn metadata_text(
        transport: &dyn Transport,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        kind: ResponseType,
    ) -> Result<Value, Fail> {
        let mut map: HeaderMap = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        map.insert("user-agent".into(), AUTH_USER_AGENT.into());
        if kind == ResponseType::Json {
            map.insert("accept".into(), "application/json".into());
        }
        let request = HttpRequest {
            method: method.into(),
            url: url.into(),
            headers: map,
            body: String::new(),
        };
        Ok(gaxios(transport, request, &NO_RETRY, kind).await?.data)
    }

    async fn session_token(&self, transport: &dyn Transport) -> Result<Option<String>, Fail> {
        let Some(url) = self.imdsv2_url else {
            return Ok(None);
        };
        let data = Self::metadata_text(
            transport,
            "PUT",
            url,
            &[("x-aws-ec2-metadata-token-ttl-seconds", "300")],
            ResponseType::Text,
        )
        .await?;
        Ok(Some(js_string(Some(&data))))
    }

    async fn region(&self, env: Option<&ProviderEnv>, transport: &dyn Transport) -> Result<String, Fail> {
        if let Some(region) =
            provider_env_value("AWS_REGION", env).or_else(|| provider_env_value("AWS_DEFAULT_REGION", env))
        {
            return Ok(region);
        }
        let session = self.session_token(transport).await?;
        let Some(url) = self.region_url else {
            return Err(Fail::typed(
                "RangeError",
                "Unable to determine AWS region due to missing \"options.credential_source.region_url\"",
            ));
        };
        let headers: Vec<(&str, &str)> = session
            .as_deref()
            .map(|t| ("x-aws-ec2-metadata-token", t))
            .into_iter()
            .collect();
        let zone = js_string(Some(
            &Self::metadata_text(transport, "GET", url, &headers, ResponseType::Text).await?,
        ));
        // `substr(0, length - 1)`: the availability zone without its letter.
        let mut chars: Vec<char> = zone.chars().collect();
        chars.pop();
        Ok(chars.into_iter().collect())
    }

    async fn credentials(&self, env: Option<&ProviderEnv>, transport: &dyn Transport) -> Result<AwsCredentials, Fail> {
        if let (Some(access_key_id), Some(secret_access_key)) = (
            provider_env_value("AWS_ACCESS_KEY_ID", env),
            provider_env_value("AWS_SECRET_ACCESS_KEY", env),
        ) {
            return Ok(AwsCredentials {
                access_key_id,
                secret_access_key,
                token: provider_env_value("AWS_SESSION_TOKEN", env),
            });
        }
        let session = self.session_token(transport).await?;
        let headers: Vec<(&str, &str)> = session
            .as_deref()
            .map(|t| ("x-aws-ec2-metadata-token", t))
            .into_iter()
            .collect();
        let Some(url) = self.url else {
            return Err(Fail::new(
                "Unable to determine AWS role name due to missing \"options.credential_source.url\"",
            ));
        };
        let role = js_string(Some(
            &Self::metadata_text(transport, "GET", url, &headers, ResponseType::Text).await?,
        ));
        let data =
            Self::metadata_text(transport, "GET", &format!("{url}/{role}"), &headers, ResponseType::Json).await?;
        Ok(AwsCredentials {
            access_key_id: js_string(data.get("AccessKeyId")),
            secret_access_key: js_string(data.get("SecretAccessKey")),
            token: data
                .get("Token")
                .filter(|v| js_truthy(Some(v)))
                .map(|v| js_string(Some(v))),
        })
    }
}

/// `AwsClient.retrieveSubjectToken` from the region and credentials: the GetCallerIdentity request signed with
/// SigV4 (`AwsRequestSigner`) at `now_ms`, as URL-encoded JSON with the `x-goog-cloud-target-resource` header added.
pub fn aws_subject_token(
    url: &str,
    region: &str,
    credentials: &AwsCredentials,
    audience: &str,
    now_ms: i64,
) -> Result<String, String> {
    use crate::aws::{amz_date, hmac_sha256, sha256_hex};
    let parsed = reqwest::Url::parse(url).map_err(|_| "Invalid URL".to_string())?;
    let host = match (parsed.host_str(), parsed.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => String::new(),
    };
    let path = parsed.path().to_string();
    let query = parsed.query().unwrap_or_default().to_string();
    let service = host.split('.').next().unwrap_or_default().to_string();
    let date = amz_date(now_ms);
    let day = &date[..8];
    // Sorted, as `Headers` iterates.
    let mut signed: Vec<(&str, String)> = vec![("host", host.clone()), ("x-amz-date", date.clone())];
    if let Some(token) = &credentials.token {
        signed.push(("x-amz-security-token", token.clone()));
    }
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = signed.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(";");
    let canonical_request = format!(
        "POST\n{path}\n{query}\n{canonical_headers}\n{signed_headers}\n{}",
        sha256_hex(b"")
    );
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let mut key = hmac_sha256(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        day.as_bytes(),
    );
    for part in [region, service.as_str(), "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let signature: String = hmac_sha256(&key, string_to_sign.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );
    let mut headers = vec![("authorization", authorization), ("host", host), ("x-amz-date", date)];
    if let Some(token) = &credentials.token {
        headers.push(("x-amz-security-token", token.clone()));
    }
    headers.push(("x-goog-cloud-target-resource", audience.to_string()));
    let headers: Vec<Value> = headers
        .into_iter()
        .map(|(key, value)| json!({ "key": key, "value": value }))
        .collect();
    let mut request = Map::new();
    request.insert("url".into(), json!(url));
    request.insert("method".into(), json!("POST"));
    request.insert("headers".into(), Value::Array(headers));
    Ok(encode_uri_component(&Value::Object(request).to_string()))
}

// -------------------------------------------------------------------------------------------- pluggable executable

fn executable_source(source: &Value) -> Result<Source, Fail> {
    let executable = source.get("executable").cloned().unwrap_or(Value::Null);
    let Some(command) = executable.get("command").filter(|v| js_truthy(Some(v))) else {
        return Err(Fail::new(NO_EXECUTABLE_SOURCE));
    };
    let command = js_string(Some(command));
    let timeout_ms = match executable.get("timeout_millis") {
        None => DEFAULT_EXECUTABLE_TIMEOUT_MS,
        Some(value) => {
            let ms = js_number(value);
            if ms < MIN_EXECUTABLE_TIMEOUT_MS || ms > MAX_EXECUTABLE_TIMEOUT_MS {
                return Err(Fail::new("Timeout must be between 5000 and 120000 milliseconds."));
            }
            ms
        }
    };
    let output_file = executable
        .get("output_file")
        .filter(|v| !v.is_null())
        .map(|v| js_string(Some(v)));
    Ok(Source::Executable {
        command: parse_command(&command)?,
        timeout_ms,
        output_file,
    })
}

/// `PluggableAuthHandler.parseCommand`: runs of `[^\s"]+` and `"[^"]*"` glued together, each fully quoted one
/// unquoted.
fn parse_command(command: &str) -> Result<Vec<String>, Fail> {
    let chars: Vec<char> = command.chars().collect();
    let mut components = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let start = i;
        loop {
            if i < chars.len() && !chars[i].is_whitespace() && chars[i] != '"' {
                i += 1;
            } else if i < chars.len()
                && chars[i] == '"'
                && let Some(close) = chars[i + 1..].iter().position(|c| *c == '"')
            {
                i += close + 2;
            } else {
                break;
            }
        }
        if i > start {
            let text: String = chars[start..i].iter().collect();
            let unquoted = if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
                text[1..text.len() - 1].to_string()
            } else {
                text
            };
            components.push(unquoted);
        } else {
            i += 1;
        }
    }
    if components.is_empty() {
        return Err(Fail::new(format!(
            "Provided command: \"{command}\" could not be parsed."
        )));
    }
    Ok(components)
}

/// `ExecutableResponse`.
struct ExecutableResponse {
    version: f64,
    success: bool,
    expiration: Option<Value>,
    token: String,
    code: String,
    message: String,
}

impl ExecutableResponse {
    /// `Err(Some(message))` for an `ExecutableResponseError`; `Err(None)` for anything else thrown.
    fn parse(json: &Value) -> Result<ExecutableResponse, Option<String>> {
        if json.is_null() {
            return Err(None);
        }
        if !js_truthy(json.get("version")) {
            return Err(Some("Executable response must contain a 'version' field.".into()));
        }
        let Some(success) = json.get("success") else {
            return Err(Some("Executable response must contain a 'success' field.".into()));
        };
        let mut response = ExecutableResponse {
            version: js_number(&json["version"]),
            success: js_truthy(Some(success)),
            expiration: None,
            token: String::new(),
            code: String::new(),
            message: String::new(),
        };
        if response.success {
            response.expiration = json.get("expiration_time").cloned();
            let token_type = json.get("token_type").and_then(Value::as_str).unwrap_or_default();
            if ![SAML_TOKEN_TYPE, OIDC_TOKEN_TYPE_ID, OIDC_TOKEN_TYPE_JWT].contains(&token_type) {
                return Err(Some(format!(
                    "Executable response must contain a 'token_type' field when successful and it must be one of {OIDC_TOKEN_TYPE_ID}, {OIDC_TOKEN_TYPE_JWT}, or {SAML_TOKEN_TYPE}."
                )));
            }
            let (field, message) = if token_type == SAML_TOKEN_TYPE {
                (
                    "saml_response",
                    format!(
                        "Executable response must contain a 'saml_response' field when token_type={SAML_TOKEN_TYPE}."
                    ),
                )
            } else {
                (
                    "id_token",
                    format!(
                        "Executable response must contain a 'id_token' field when token_type={OIDC_TOKEN_TYPE_ID} or {OIDC_TOKEN_TYPE_JWT}."
                    ),
                )
            };
            if !js_truthy(json.get(field)) {
                return Err(Some(message));
            }
            response.token = js_string(json.get(field));
        } else {
            if !js_truthy(json.get("code")) {
                return Err(Some(
                    "Executable response must contain a 'code' field when unsuccessful.".into(),
                ));
            }
            if !js_truthy(json.get("message")) {
                return Err(Some(
                    "Executable response must contain a 'message' field when unsuccessful.".into(),
                ));
            }
            response.code = js_string(json.get("code"));
            response.message = js_string(json.get("message"));
        }
        Ok(response)
    }

    fn is_expired(&self) -> bool {
        self.expiration
            .as_ref()
            .is_some_and(|e| js_number(e) < (crate::options::now_ms() as f64 / 1000.0).round())
    }
}

fn executable_error(message: &str, code: &str) -> Fail {
    Fail::new(format!(
        "The executable failed with exit code: {code} and error message: {message}."
    ))
}

impl External {
    /// `PluggableAuthClient.retrieveSubjectToken`.
    async fn executable_token(
        &self,
        env: Option<&ProviderEnv>,
        command: &[String],
        timeout_ms: f64,
        output_file: Option<&str>,
    ) -> Result<String, Fail> {
        if provider_env_value("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES", env).as_deref() != Some("1") {
            return Err(Fail::new(
                "Pluggable Auth executables need to be explicitly allowed to run by setting the GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES environment Variable to 1.",
            ));
        }
        let cached = match output_file {
            Some(path) => cached_response(path)?,
            None => None,
        };
        let response = match cached {
            Some(response) => response,
            None => {
                // Node leaves an undefined value out of the child's environment.
                let mut vars = vec![("GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE".to_string(), self.audience.clone())];
                if let Some(kind) = &self.subject_token_type {
                    vars.push(("GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE".into(), kind.clone()));
                }
                vars.push(("GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE".into(), "0".into()));
                if let Some(path) = output_file {
                    vars.push(("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE".into(), path.into()));
                }
                if let Some(email) = self.impersonated_email()? {
                    vars.push(("GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL".into(), email));
                }
                run_executable(command, timeout_ms, env, &vars).await?
            }
        };
        if response.version > 1.0 {
            return Err(Fail::new(
                "Version of executable is not currently supported, maximum supported version is 1.",
            ));
        }
        if !response.success {
            return Err(executable_error(&response.message, &response.code));
        }
        if output_file.is_some() && !js_truthy(response.expiration.as_ref()) {
            return Err(Fail::new(
                "The executable response must contain the `expiration_time` field for successful responses when an output_file has been specified in the configuration.",
            ));
        }
        if response.is_expired() {
            return Err(Fail::new("Executable response is expired."));
        }
        Ok(response.token)
    }

    /// `getServiceAccountEmail`.
    fn impersonated_email(&self) -> Result<Option<String>, Fail> {
        let Some(url) = &self.impersonation_url else {
            return Ok(None);
        };
        if url.encode_utf16().count() > 256 {
            return Err(Fail::typed("RangeError", format!("URL is too long: {url}")));
        }
        // `/serviceAccounts\/(?<email>[^:]+):generateAccessToken$/`
        let Some(head) = url.strip_suffix(":generateAccessToken") else {
            return Ok(None);
        };
        Ok(head
            .rfind("serviceAccounts/")
            .map(|at| &head[at + "serviceAccounts/".len()..])
            .filter(|email| !email.is_empty() && !email.contains(':'))
            .map(str::to_string))
    }
}

/// `PluggableAuthHandler.retrieveCachedResponse`.
fn cached_response(path: &str) -> Result<Option<ExecutableResponse>, Fail> {
    if path.is_empty() {
        return Ok(None);
    }
    let Ok(real) = realpath(&absolute(path)) else {
        return Ok(None);
    };
    if !std::fs::symlink_metadata(&real).is_ok_and(|m| m.is_file()) {
        return Ok(None);
    }
    let text = std::fs::read(&real).map_err(|e| Fail::new(io_message(&e, "open", &real)))?;
    let text = String::from_utf8_lossy(&text);
    if text.is_empty() {
        return Ok(None);
    }
    let invalid = || Fail::new(format!("The output file contained an invalid response: {text}"));
    if json_parse_error(&text).is_some() {
        return Err(invalid());
    }
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match ExecutableResponse::parse(&json) {
        Ok(response) => Ok((!response.is_expired() && response.success).then_some(response)),
        Err(Some(message)) => Err(Fail::new(message)),
        Err(None) => Err(invalid()),
    }
}

/// `PluggableAuthHandler.retrieveResponseFromExecutable`: stdout and stderr share one pipe, so the output keeps the
/// order the executable wrote it in; past the timeout the child is killed.
async fn run_executable(
    command: &[String],
    timeout_ms: f64,
    env: Option<&ProviderEnv>,
    vars: &[(String, String)],
) -> Result<ExecutableResponse, Fail> {
    use tokio::io::AsyncReadExt;
    let spawn_error = |e: std::io::Error| executable_error("", &(-e.raw_os_error().unwrap_or(1)).to_string());
    let (reader, writer) = std::io::pipe().map_err(spawn_error)?;
    let mut cmd = tokio::process::Command::new(&command[0]);
    cmd.args(&command[1..])
        .stdin(std::process::Stdio::null())
        .stdout(writer.try_clone().map_err(spawn_error)?)
        .stderr(writer)
        .kill_on_drop(true);
    if let Some(env) = env {
        cmd.envs(env.iter().filter(|(_, v)| !v.is_empty()));
    }
    cmd.envs(vars.iter().map(|(k, v)| (k, v)));
    let mut child = cmd.spawn().map_err(spawn_error)?;
    // The command holds the pipe's write ends until it is dropped; the read side ends when the child's copies close.
    drop(cmd);
    let mut receiver =
        tokio::net::unix::pipe::Receiver::from_owned_fd(reader.into()).map_err(|e| Fail::new(e.to_string()))?;
    let run = async {
        let mut bytes = Vec::new();
        let _ = receiver.read_to_end(&mut bytes).await;
        let status = child.wait().await;
        (String::from_utf8_lossy(&bytes).into_owned(), status)
    };
    let limit = Duration::from_millis(timeout_ms.max(1.0) as u64);
    let outcome = tokio::time::timeout(limit, run).await;
    let (output, status) = match outcome {
        Ok(done) => done,
        Err(_) => {
            let _ = child.kill().await;
            return Err(Fail::new(
                "The executable failed to finish within the timeout specified.",
            ));
        }
    };
    let code = status
        .ok()
        .and_then(|s| s.code())
        .map_or_else(|| "null".to_string(), |c| c.to_string());
    if code != "0" {
        return Err(executable_error(&output, &code));
    }
    let invalid = || Fail::new(format!("The executable returned an invalid response: {output}"));
    if json_parse_error(&output).is_some() {
        return Err(invalid());
    }
    let json: Value = serde_json::from_str(&output).unwrap_or(Value::Null);
    ExecutableResponse::parse(&json).map_err(|message| message.map_or_else(invalid, Fail::new))
}

// ------------------------------------------------------------------------------------ the shared STS error shape

/// A token request whose error response is reworded by `getErrorFromOAuthErrorResponse`.
async fn oauth_exchange(transport: &dyn Transport, request: HttpRequest) -> Result<String, Fail> {
    match gaxios(transport, request, &AUTH_RETRY, ResponseType::Json).await {
        Ok(response) => Ok(js_string(response.data.get("access_token"))),
        Err(mut fail) => {
            if let Some(data) = fail.data.as_deref() {
                let field = |key: &str| data.get(key).map(|v| js_string(Some(v)));
                let mut message = format!("Error code {}", field("error").unwrap_or_else(|| "undefined".into()));
                if let Some(description) = field("error_description") {
                    message.push_str(&format!(": {description}"));
                }
                if let Some(uri) = field("error_uri") {
                    message.push_str(&format!(" - {uri}"));
                }
                fail.message = message;
            }
            Err(fail)
        }
    }
}

// ------------------------------------------------------------------------------ external_account_authorized_user

/// An `external_account_authorized_user` client.
#[derive(Clone, Debug)]
pub(crate) struct ExternalUser {
    pub(super) universe: String,
    pub(super) quota: Option<String>,
    token_url: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

impl ExternalUser {
    pub(super) fn from_json(json: &Value) -> ExternalUser {
        let universe =
            camel_option(json, "universe_domain").map_or_else(|| DEFAULT_UNIVERSE.into(), |v| js_string(Some(v)));
        let plain = |key: &str| js_string(json.get(key).filter(|v| !v.is_null()));
        ExternalUser {
            token_url: json.get("token_url").filter(|v| !v.is_null()).map_or_else(
                || format!("https://sts.{universe}/v1/oauthtoken"),
                |v| js_string(Some(v)),
            ),
            universe,
            quota: json
                .get("quota_project_id")
                .filter(|v| js_truthy(Some(v)))
                .map(|v| js_string(Some(v))),
            client_id: plain("client_id"),
            client_secret: json
                .get("client_secret")
                .filter(|v| js_truthy(Some(v)))
                .map(|v| js_string(Some(v)))
                .unwrap_or_default(),
            refresh_token: plain("refresh_token"),
        }
    }

    /// `ExternalAccountAuthorizedUserHandler.refreshToken`.
    pub(super) async fn access_token(&self, transport: &dyn Transport) -> Result<String, Fail> {
        let request = HttpRequest {
            method: "POST".into(),
            url: self.token_url.clone(),
            headers: [
                ("user-agent".to_string(), AUTH_USER_AGENT.to_string()),
                (
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded;charset=UTF-8".to_string(),
                ),
                ("accept".to_string(), "application/json".to_string()),
                ("authorization".to_string(), basic(&self.client_id, &self.client_secret)),
            ]
            .into(),
            body: form_urlencode(&[("grant_type", "refresh_token"), ("refresh_token", &self.refresh_token)]),
        };
        oauth_exchange(transport, request).await
    }
}

// ---------------------------------------------------------------------------------------------- self-signed JWT

/// `JWTAccess.getRequestHeaders` with scopes: the bearer token a service account outside the default universe signs
/// for itself (`{alg, typ, kid?}` header, `{iss, sub, scope, exp, iat}` payload, one hour).
pub fn self_signed_jwt(email: &str, pem: &str, key_id: Option<&str>, iat_seconds: i64) -> Result<String, String> {
    let mut header = Map::new();
    header.insert("alg".into(), json!("RS256"));
    header.insert("typ".into(), json!("JWT"));
    if let Some(kid) = key_id {
        header.insert("kid".into(), json!(kid));
    }
    let mut payload = Map::new();
    payload.insert("iss".into(), json!(email));
    payload.insert("sub".into(), json!(email));
    payload.insert("scope".into(), json!(SCOPE));
    payload.insert("exp".into(), json!(iat_seconds + 3600));
    payload.insert("iat".into(), json!(iat_seconds));
    let input = format!(
        "{}.{}",
        base64url(Value::Object(header).to_string().as_bytes()),
        base64url(Value::Object(payload).to_string().as_bytes())
    );
    let signature = sign_rs256(pem, input.as_bytes())?;
    Ok(format!("{input}.{}", base64url(&signature)))
}
