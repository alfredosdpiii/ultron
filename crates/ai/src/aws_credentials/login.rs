//! `aws login` sessions (`@aws-sdk/credential-provider-login`, reached from a profile's `login_session`): the cached
//! token `<AWS_LOGIN_CACHE_DIRECTORY or ~/.aws/login/cache>/<sha256(login_session)>.json`, its credentials while
//! they last more than five minutes, else a Signin `CreateOAuth2Token` refresh (a JSON POST to `/v1/token` carrying
//! a DPoP proof: an ES256 JWT signed with the token's P-256 `dpopKey`), written back to the cache. A failed refresh
//! keeps a token that has not expired yet; the error texts and which failures end the chain are upstream's.

use super::{CredentialError, Result, configured_endpoint, fs_error, home_dir, js_string, parse_js_date, request};
use crate::aws::AwsCredentials;
use crate::http::HeaderMap;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// `LoginCredentialsFetcher.REFRESH_THRESHOLD`.
const REFRESH_THRESHOLD_MS: i64 = 5 * 60 * 1000;
/// The SDK's standard retry strategy: three attempts for a server error or throttling.
const MAX_ATTEMPTS: usize = 3;

/// `getTokenFilePath`.
pub fn login_token_path(login_session: &str, input: &super::ChainInput<'_>) -> PathBuf {
    let directory = input
        .var("AWS_LOGIN_CACHE_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir(input.env).join(".aws").join("login").join("cache"));
    let digest = Sha256::digest(login_session.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    directory.join(format!("{hex}.json"))
}

/// The Signin endpoint of `region` (the client's endpoint rules, by partition).
pub fn signin_endpoint(region: &str) -> String {
    let suffix = [
        ("cn-", "signin.amazonaws.cn"),
        ("us-gov-", "signin.amazonaws-us-gov.com"),
        ("us-isob-", "signin.sc2shome.sgov.gov"),
        ("us-isof-", "signin.csphome.hci.ic.gov"),
        ("us-iso-", "signin.c2shome.ic.gov"),
        ("eu-isoe-", "signin.csphome.adc-e.uk"),
        ("eusc-", "signin.amazonaws-eusc.eu"),
    ]
    .iter()
    .find(|(prefix, _)| region.starts_with(prefix))
    .map_or("signin.aws.amazon.com", |(_, suffix)| suffix);
    format!("https://{region}.{suffix}")
}

/// JavaScript truthiness of a JSON field.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(_) => true,
    }
}

/// A thrown error as `String(error)` prints it.
fn thrown(name: &str, message: &str) -> String {
    format!("{name}: {message}")
}

/// `loadToken`: the cached token, validated; every failure ends the chain.
fn load_token(path: &PathBuf) -> Result<Map<String, Value>> {
    let shown = path.display().to_string();
    let fail = |inner: String| CredentialError::stop(format!("Failed to load token from {shown}: {inner}"));
    let text = std::fs::read_to_string(path).map_err(|e| fail(thrown("Error", &fs_error(&e, &shown))))?;
    let token: Value = serde_json::from_str(&text).map_err(|e| fail(thrown("SyntaxError", &e.to_string())))?;
    let token = match token {
        Value::Object(map) => map,
        Value::Null => {
            return Err(fail(thrown(
                "TypeError",
                "Cannot read properties of null (reading 'accessToken')",
            )));
        }
        _ => Map::new(),
    };
    let mut missing: Vec<&str> = ["accessToken", "clientId", "refreshToken", "dpopKey"]
        .into_iter()
        .filter(|key| !truthy(token.get(*key)))
        .collect();
    if !truthy(token.get("accessToken").and_then(|a| a.get("accountId"))) {
        missing.push("accountId");
    }
    if !missing.is_empty() {
        return Err(fail(thrown(
            "CredentialsProviderError",
            &format!("Token validation failed, missing fields: {}", missing.join(", ")),
        )));
    }
    Ok(token)
}

fn expiry(token: &Map<String, Value>) -> Option<i64> {
    token
        .get("accessToken")
        .and_then(|a| a.get("expiresAt"))
        .and_then(parse_js_date)
}

/// `toCredentials` (`expiration: new Date(token.expiresAt)`).
fn credentials(input: &super::ChainInput<'_>, token: &Map<String, Value>) -> AwsCredentials {
    input.expiration.set(expiry(token));
    let field = |key: &str| {
        token
            .get("accessToken")
            .and_then(|a| a.get(key))
            .filter(|v| !v.is_null())
            .map(js_string)
    };
    AwsCredentials {
        access_key_id: field("accessKeyId").unwrap_or_default(),
        secret_access_key: field("secretAccessKey").unwrap_or_default(),
        session_token: field("sessionToken"),
    }
}

/// `fromLoginCredentials` for a profile with `login_session`.
pub(super) async fn from_login(input: &super::ChainInput<'_>, profile: &super::Section) -> Result<AwsCredentials> {
    let session = profile.get("login_session").cloned().unwrap_or_default();
    let path = login_token_path(&session, input);
    let token = load_token(&path)?;
    // `expiryTime - now <= REFRESH_THRESHOLD` (an unparsable date is NaN: never refreshed).
    match expiry(&token) {
        Some(at) if at - input.now_ms <= REFRESH_THRESHOLD_MS => refresh(input, profile, &path, token).await,
        _ => Ok(credentials(input, &token)),
    }
}

/// `refresh`: the token on disk if another process refreshed it, else a Signin refresh.
async fn refresh(
    input: &super::ChainInput<'_>,
    profile: &super::Section,
    path: &PathBuf,
    token: Map<String, Value>,
) -> Result<AwsCredentials> {
    let now = input.now_ms;
    let disk = load_token(path).unwrap_or_else(|_| token.clone());
    let disk_expired = expiry(&disk).is_some_and(|at| at <= now);
    let token_valid = expiry(&token).is_some_and(|at| at > now);
    let fresh = if disk_expired && token_valid { token } else { disk };
    if expiry(&fresh).is_some_and(|at| at - now > REFRESH_THRESHOLD_MS) {
        return Ok(credentials(input, &fresh));
    }
    match create_token(input, profile, path, &fresh).await {
        Ok(credentials) => Ok(credentials),
        Err(Failure::AccessDenied { error, text }) => {
            let message = match error.as_str() {
                "TOKEN_EXPIRED" => "Your session has expired. Please reauthenticate.".to_string(),
                "USER_CREDENTIALS_CHANGED" => "Unable to refresh credentials because of a change in your password. Please reauthenticate with your new password.".into(),
                "INSUFFICIENT_PERMISSIONS" => "Unable to refresh credentials due to insufficient permissions. You may be missing permission for the 'CreateOAuth2Token' action.".into(),
                _ => format!("Failed to refresh token: {text}. Please re-authenticate using `aws login`"),
            };
            Err(CredentialError::stop(message))
        }
        Err(Failure::Other(text)) => {
            // The SDK logs a warning and keeps the token until it expires.
            if expiry(&fresh).is_some_and(|at| at > now) {
                return Ok(credentials(input, &fresh));
            }
            Err(CredentialError::next(format!(
                "Failed to refresh token: {text}. Please re-authenticate using aws login"
            )))
        }
    }
}

/// Why `CreateOAuth2Token` failed: an `AccessDeniedException` (its `error` code and `String(error)`), or anything
/// else as `String(error)`.
enum Failure {
    AccessDenied { error: String, text: String },
    Other(String),
}

/// `SigninClient.send(new CreateOAuth2TokenCommand(...))`, then the updated token saved.
async fn create_token(
    input: &super::ChainInput<'_>,
    profile: &super::Section,
    path: &PathBuf,
    fresh: &Map<String, Value>,
) -> std::result::Result<AwsCredentials, Failure> {
    let region = profile
        .get("region")
        .cloned()
        .unwrap_or_else(|| input.region.to_string());
    let endpoint = configured_endpoint(input, "SIGNIN").unwrap_or_else(|| signin_endpoint(&region));
    let url = format!("{endpoint}/v1/token");
    let mut body = Map::new();
    for (key, value) in [
        ("clientId", fresh.get("clientId")),
        ("grantType", Some(&json!("refresh_token"))),
        ("refreshToken", fresh.get("refreshToken")),
    ] {
        if let Some(value) = value.filter(|v| !v.is_null()) {
            body.insert(key.into(), value.clone());
        }
    }
    let body = Value::Object(body).to_string();
    let mut attempt = 0;
    let reply = loop {
        attempt += 1;
        // The DPoP proof is made for each attempt (the interceptor runs in `finalizeRequest`).
        let proof = dpop(path, "POST", &url, input.now_ms).map_err(|e| Failure::Other(thrown(&e.name, &e.message)))?;
        let mut headers = HeaderMap::new();
        headers.insert("content-type".into(), "application/json".into());
        headers.insert("dpop".into(), proof);
        let reply = request(input, "POST", url.clone(), headers, body.clone(), None)
            .await
            .map_err(|e| Failure::Other(thrown("Error", &e)))?;
        let retryable = reply.status >= 500 || reply.status == 429;
        if !retryable || attempt >= MAX_ATTEMPTS {
            break reply;
        }
    };
    let parsed: Value = serde_json::from_str(&reply.body).unwrap_or(Value::Null);
    if !(200..300).contains(&reply.status) {
        let name = reply
            .headers
            .get("x-amzn-errortype")
            .map(|t| t.split(':').next().unwrap_or_default().to_string())
            .or_else(|| {
                parsed
                    .get("__type")
                    .and_then(Value::as_str)
                    .map(|t| t.rsplit('#').next().unwrap_or_default().to_string())
            })
            .or_else(|| parsed.get("code").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "UnknownError".into());
        let message = parsed
            .get("message")
            .or_else(|| parsed.get("Message"))
            .and_then(Value::as_str)
            .unwrap_or("UnknownError")
            .to_string();
        let text = thrown(&name, &message);
        return Err(if name == "AccessDeniedException" {
            Failure::AccessDenied {
                error: parsed.get("error").map(js_string).unwrap_or_else(|| "undefined".into()),
                text,
            }
        } else {
            Failure::Other(text)
        });
    }
    let access = parsed.get("accessToken");
    let field = |key: &str| access.and_then(|a| a.get(key));
    let refresh_token = parsed.get("refreshToken");
    if !truthy(field("accessKeyId"))
        || !truthy(field("secretAccessKey"))
        || !truthy(field("sessionToken"))
        || !truthy(refresh_token)
    {
        return Err(Failure::Other(thrown(
            "CredentialsProviderError",
            "Token refresh response missing required fields",
        )));
    }
    let expires_in = parsed.get("expiresIn").and_then(Value::as_f64).unwrap_or(900.0);
    let expires_at = crate::claude_code_cli::iso_string(input.now_ms as f64 + expires_in * 1000.0);
    let mut updated = fresh.clone();
    let mut access_token = match updated.get("accessToken") {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    for key in ["accessKeyId", "secretAccessKey", "sessionToken"] {
        access_token.insert(key.into(), field(key).cloned().unwrap_or(Value::Null));
    }
    access_token.insert("expiresAt".into(), json!(expires_at));
    updated.insert("accessToken".into(), Value::Object(access_token));
    updated.insert("refreshToken".into(), refresh_token.cloned().unwrap_or(Value::Null));
    if let Some(directory) = path.parent() {
        let _ = std::fs::create_dir_all(directory);
    }
    let text = serde_json::to_string_pretty(&Value::Object(updated.clone())).unwrap_or_default();
    std::fs::write(path, text)
        .map_err(|e| Failure::Other(thrown("Error", &fs_error(&e, &path.display().to_string()))))?;
    Ok(credentials(input, &updated))
}

/// `generateDpop`: an ES256 `dpop+jwt` over the request (`htm`, `htu`, `iat`, a random `jti`), signed with the
/// cached token's `dpopKey` (SEC1 PEM), its public key as the header's JWK.
fn dpop(path: &PathBuf, method: &str, endpoint: &str, now_ms: i64) -> Result<String> {
    let token = load_token(path)?;
    let fail = |message: String| CredentialError::stop(format!("Failed to generate Dpop proof: {message}"));
    let pem = token.get("dpopKey").map(js_string).unwrap_or_default();
    let (private, public) = parse_sec1_pem(&pem).map_err(fail)?;
    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::from_private_key_and_public_key(&ECDSA_P256_SHA256_FIXED_SIGNING, &private, &public, &rng)
        .map_err(|e| fail(e.to_string()))?;
    let b64 = crate::oauth_flow::base64url;
    let header = json!({
        "alg": "ES256",
        "typ": "dpop+jwt",
        "jwk": {"kty": "EC", "crv": "P-256", "x": b64(&public[1..33]), "y": b64(&public[33..65])},
    });
    let payload = json!({"jti": uuid_v4(), "htm": method, "htu": endpoint, "iat": now_ms.div_euclid(1000)});
    let message = format!(
        "{}.{}",
        b64(header.to_string().as_bytes()),
        b64(payload.to_string().as_bytes())
    );
    let signature = key.sign(&rng, message.as_bytes()).map_err(|e| fail(e.to_string()))?;
    Ok(format!("{message}.{}", b64(signature.as_ref())))
}

/// `crypto.randomUUID()`.
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// One DER element at the start of `der`: its tag, contents and the rest.
fn der(der: &[u8]) -> std::result::Result<(u8, &[u8], &[u8]), String> {
    let invalid = || "Invalid EC private key".to_string();
    let (&tag, rest) = der.split_first().ok_or_else(invalid)?;
    let (&first, rest) = rest.split_first().ok_or_else(invalid)?;
    let (length, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 2 || rest.len() < count {
            return Err(invalid());
        }
        let length = rest[..count].iter().fold(0usize, |n, b| (n << 8) | *b as usize);
        (length, &rest[count..])
    };
    if rest.len() < length {
        return Err(invalid());
    }
    Ok((tag, &rest[..length], &rest[length..]))
}

/// The private scalar and uncompressed public point of a P-256 `EC PRIVATE KEY` (RFC 5915).
pub fn parse_sec1_pem(pem: &str) -> std::result::Result<(Vec<u8>, Vec<u8>), String> {
    let invalid = || "Invalid EC private key".to_string();
    let body: String = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect();
    if !pem.contains("-----BEGIN EC PRIVATE KEY-----") {
        return Err(invalid());
    }
    let bytes = crate::jsutil::atob(&body).ok_or_else(invalid)?;
    let (tag, mut contents, _) = der(&bytes)?;
    if tag != 0x30 {
        return Err(invalid());
    }
    let (version_tag, version, rest) = der(contents)?;
    if version_tag != 0x02 || version != [1] {
        return Err(invalid());
    }
    contents = rest;
    let (scalar_tag, scalar, rest) = der(contents)?;
    if scalar_tag != 0x04 || scalar.len() != 32 {
        return Err(invalid());
    }
    contents = rest;
    let mut public = None;
    while !contents.is_empty() {
        let (tag, inner, rest) = der(contents)?;
        contents = rest;
        match tag {
            // [0] parameters: the named curve must be prime256v1.
            0xa0 => {
                if inner != [0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07] {
                    return Err("Unsupported curve: the DPoP key must be P-256".into());
                }
            }
            // [1] publicKey: a BIT STRING with no unused bits holding the uncompressed point.
            0xa1 => {
                let (bits_tag, bits, _) = der(inner)?;
                if bits_tag != 0x03 || bits.len() != 66 || bits[0] != 0 || bits[1] != 0x04 {
                    return Err(invalid());
                }
                public = Some(bits[1..].to_vec());
            }
            _ => return Err(invalid()),
        }
    }
    let public = public.ok_or("The DPoP key has no public key")?;
    Ok((scalar.to_vec(), public))
}
