//! `CertificateSubjectTokenSupplier` of google-auth-library 10.6.2: an identity-pool `credential_source.certificate`
//! (X.509 workload identity). The certificate configuration file (the configured location, else
//! `GOOGLE_API_CERTIFICATE_CONFIG`, else gcloud's `certificate_config.json`) names the workload's certificate and key;
//! the subject token is the JSON array of the base64 DER certificates (the leaf, then the trust chain), and the STS
//! exchange runs on a client presenting that certificate and key (mTLS).
//!
//! Messages are upstream's. Node checks the files with OpenSSL (`new X509Certificate`, `createPrivateKey`); here a
//! certificate is a PEM `CERTIFICATE` block (or a DER buffer) holding an X.509 `Certificate` structure, and a key a PEM
//! private key block, and their failures carry OpenSSL's message for the common case.

use super::{Fail, io_message};
use crate::env::{ProviderEnv, provider_env_value};
use crate::jsutil::{atob, btoa, json_parse_error};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(super) const CERTIFICATE_CONFIGURATION_ENV_VARIABLE: &str = "GOOGLE_API_CERTIFICATE_CONFIG";
const UNAVAILABLE: &str = "CertificateSourceUnavailableError";
const INVALID: &str = "InvalidConfigurationError";

/// The constructor options (`use_default_certificate_config`, `certificate_config_location`, `trust_chain_path`).
#[derive(Clone, Debug)]
pub(super) struct CertificateSource {
    location: String,
    trust_chain: Option<String>,
}

/// What `getSubjectToken` produced: the token, and the certificate and key files the mTLS client presents.
pub(super) struct CertificateToken {
    pub token: String,
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
}

fn truthy(value: Option<&Value>) -> bool {
    super::js_truthy(value)
}

impl CertificateSource {
    /// The constructor's checks.
    pub(super) fn from_json(certificate: &Value) -> Result<CertificateSource, Fail> {
        let default = truthy(certificate.get("use_default_certificate_config"));
        let location = certificate.get("certificate_config_location");
        if !default && !truthy(location) {
            return Err(Fail::typed(
                INVALID,
                "Either `useDefaultCertificateConfig` must be true or a `certificateConfigLocation` must be provided.",
            ));
        }
        if default && truthy(location) {
            return Err(Fail::typed(
                INVALID,
                "Both `useDefaultCertificateConfig` and `certificateConfigLocation` cannot be provided.",
            ));
        }
        // `certificateConfigLocation ?? ''`: an empty location searches the defaults.
        let location = location.filter(|v| !v.is_null()).map(|v| super::js_string(Some(v)));
        Ok(CertificateSource {
            location: location.unwrap_or_default(),
            trust_chain: certificate
                .get("trust_chain_path")
                .filter(|v| truthy(Some(v)))
                .map(|v| super::js_string(Some(v))),
        })
    }

    /// `getSubjectToken`.
    pub(super) fn subject_token(&self, env: Option<&ProviderEnv>) -> Result<CertificateToken, Fail> {
        let config_path = self.config_path(env)?;
        let (cert_path, key_path) = cert_and_key_paths(&config_path)?;
        let cert = std::fs::read(&cert_path).map_err(|e| {
            Fail::typed(
                UNAVAILABLE,
                format!(
                    "Failed to read certificate file at {cert_path}: {}",
                    io_message(&e, "open", Path::new(&cert_path))
                ),
            )
        })?;
        let leaf = parse_certificate(&cert).map_err(|message| {
            Fail::typed(
                UNAVAILABLE,
                format!("Failed to read certificate file at {cert_path}: {message}"),
            )
        })?;
        let key = std::fs::read(&key_path).map_err(|e| {
            Fail::typed(
                UNAVAILABLE,
                format!(
                    "Failed to read private key file at {key_path}: {}",
                    io_message(&e, "open", Path::new(&key_path))
                ),
            )
        })?;
        check_private_key(&key).map_err(|message| {
            Fail::typed(
                UNAVAILABLE,
                format!("Failed to read private key file at {key_path}: {message}"),
            )
        })?;
        let token = self.chain(leaf)?;
        Ok(CertificateToken { token, cert, key })
    }

    /// `#resolveCertificateConfigFilePath`.
    fn config_path(&self, env: Option<&ProviderEnv>) -> Result<String, Fail> {
        if !self.location.is_empty() {
            if is_valid_file(&self.location) {
                return Ok(self.location.clone());
            }
            return Err(Fail::typed(
                UNAVAILABLE,
                format!("Provided certificate config path is invalid: {}", self.location),
            ));
        }
        if let Some(path) = provider_env_value(CERTIFICATE_CONFIGURATION_ENV_VARIABLE, env).filter(|p| !p.is_empty()) {
            if is_valid_file(&path) {
                return Ok(path);
            }
            return Err(Fail::typed(
                UNAVAILABLE,
                format!(
                    "Path from environment variable \"{CERTIFICATE_CONFIGURATION_ENV_VARIABLE}\" is invalid: {path}"
                ),
            ));
        }
        let well_known = well_known_location(env);
        if is_valid_file(&well_known) {
            return Ok(well_known);
        }
        Err(Fail::typed(
            UNAVAILABLE,
            format!(
                "Could not find certificate configuration file. Searched override path, the \"{CERTIFICATE_CONFIGURATION_ENV_VARIABLE}\" env var, and the gcloud path ({well_known})."
            ),
        ))
    }

    /// `#processChainFromPaths`.
    fn chain(&self, leaf: Vec<u8>) -> Result<String, Fail> {
        let token = |certs: &[Vec<u8>]| {
            serde_json::to_string(&certs.iter().map(|c| btoa(c)).collect::<Vec<_>>()).unwrap_or_default()
        };
        let Some(path) = &self.trust_chain else {
            return Ok(token(&[leaf]));
        };
        let text = std::fs::read(path).map_err(|e| {
            Fail::typed(
                UNAVAILABLE,
                format!(
                    "Failed to process certificate chain from {path}: {}",
                    io_message(&e, "open", Path::new(path))
                ),
            )
        })?;
        let text = String::from_utf8_lossy(&text);
        let mut chain = Vec::new();
        for (index, block) in chain_blocks(&text).into_iter().enumerate() {
            let der = parse_certificate(block.as_bytes()).map_err(|message| {
                Fail::typed(
                    INVALID,
                    format!("Failed to parse certificate at index {index} in trust chain file {path}: {message}"),
                )
            })?;
            chain.push(der);
        }
        match chain.iter().position(|c| *c == leaf) {
            None => {
                chain.insert(0, leaf);
                Ok(token(&chain))
            }
            Some(0) => Ok(token(&chain)),
            Some(index) => Err(Fail::typed(
                INVALID,
                format!(
                    "Leaf certificate exists in the trust chain but is not the first entry (found at index {index})."
                ),
            )),
        }
    }
}

/// `isValidFile`: `lstat` says a regular file (a symbolic link is not one).
fn is_valid_file(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// `getWellKnownCertificateConfigFileLocation`: `$CLOUDSDK_CONFIG`, else `$HOME/.config/gcloud`, then
/// `certificate_config.json` (`path.join`).
fn well_known_location(env: Option<&ProviderEnv>) -> String {
    let dir = provider_env_value("CLOUDSDK_CONFIG", env)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(provider_env_value("HOME", env).unwrap_or_default())
                .join(".config")
                .join("gcloud")
        });
    js_join(&dir.join("certificate_config.json"))
}

/// `path.join`'s normalization: no `.` segments, `..` folded, no repeated or trailing separators.
fn js_join(path: &Path) -> String {
    let text = path.to_string_lossy();
    let absolute = text.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            _ => parts.push(part),
        }
    }
    let joined = parts.join("/");
    match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

/// `#getCertAndKeyPaths`.
fn cert_and_key_paths(config_path: &str) -> Result<(String, String), Fail> {
    let text = std::fs::read(config_path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|_| {
            Fail::typed(
                UNAVAILABLE,
                format!("Failed to read certificate config file at: {config_path}"),
            )
        })?;
    if let Some(message) = json_parse_error(&text) {
        return Err(Fail::typed(
            INVALID,
            format!("Failed to parse certificate config from {config_path}: {message}"),
        ));
    }
    let config: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let workload = &config["cert_configs"]["workload"];
    let (cert, key) = (workload.get("cert_path"), workload.get("key_path"));
    if !truthy(cert) || !truthy(key) {
        return Err(Fail::typed(
            INVALID,
            format!(
                "Certificate config file ({config_path}) is missing required \"cert_path\" or \"key_path\" in the workload config."
            ),
        ));
    }
    Ok((super::js_string(cert), super::js_string(key)))
}

/// The PEM blocks of `text` labelled one of `labels`, decoded.
fn pem_blocks(text: &str, labels: &[&str]) -> Vec<Option<Vec<u8>>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("-----BEGIN ") {
        let after = &rest[at + "-----BEGIN ".len()..];
        let Some(label_end) = after.find("-----") else { break };
        let label = &after[..label_end];
        let body_start = &after[label_end + 5..];
        let end_marker = format!("-----END {label}-----");
        let Some(body_end) = body_start.find(&end_marker) else {
            break;
        };
        if labels.contains(&label) {
            out.push(atob(&body_start[..body_end]));
        }
        rest = &body_start[body_end + end_marker.len()..];
    }
    out
}

/// One DER element: its tag, its contents and what follows it.
fn der_element(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = data.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let length = rest[..count].iter().fold(0usize, |n, b| (n << 8) | usize::from(*b));
        (length, &rest[count..])
    };
    (rest.len() >= length).then(|| (tag, &rest[..length], &rest[length..]))
}

/// An X.509 `Certificate`: a SEQUENCE of the to-be-signed SEQUENCE, the algorithm SEQUENCE and the signature BIT
/// STRING, and nothing after it.
fn is_certificate_der(der: &[u8]) -> bool {
    let Some((0x30, body, [])) = der_element(der) else {
        return false;
    };
    let Some((0x30, _, rest)) = der_element(body) else {
        return false;
    };
    let Some((0x30, _, rest)) = der_element(rest) else {
        return false;
    };
    matches!(der_element(rest), Some((0x03, _, [])))
}

/// `new X509Certificate(buffer)`: the DER of the buffer's first PEM certificate, else of the buffer as DER.
fn parse_certificate(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(bytes);
    let blocks = pem_blocks(&text, &["CERTIFICATE", "X509 CERTIFICATE", "TRUSTED CERTIFICATE"]);
    match blocks.into_iter().next() {
        Some(Some(der)) if is_certificate_der(&der) => Ok(der),
        Some(_) => Err("error:068000A8:asn1 encoding routines::wrong tag".into()),
        None if is_certificate_der(bytes) => Ok(bytes.to_vec()),
        None => Err("error:0480006C:PEM routines::no start line".into()),
    }
}

/// `createPrivateKey(buffer)`: a PEM private key (PKCS#8, PKCS#1 or SEC1).
fn check_private_key(bytes: &[u8]) -> Result<(), String> {
    let text = String::from_utf8_lossy(bytes);
    let blocks = pem_blocks(&text, &["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY"]);
    match blocks.into_iter().next() {
        Some(Some(der)) if matches!(der_element(&der), Some((0x30, _, []))) => Ok(()),
        _ => Err("error:1E08010C:DECODER routines::unsupported".into()),
    }
}

/// `chainPems.match(/-----BEGIN CERTIFICATE-----[^-]+-----END CERTIFICATE-----/g)`.
fn chain_blocks(text: &str) -> Vec<&str> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(BEGIN).map(|i| from + i) {
        let body = &text[at + BEGIN.len()..];
        let body_len = body.find('-').unwrap_or(body.len());
        if body_len > 0 && body[body_len..].starts_with(END) {
            let end = at + BEGIN.len() + body_len + END.len();
            out.push(&text[at..end]);
            from = end;
        } else {
            from = at + 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_google_auth_trust_chain_blocks_follow_the_upstream_regex() {
        let a = "-----BEGIN CERTIFICATE-----\nAA\n-----END CERTIFICATE-----";
        let b = "-----BEGIN CERTIFICATE-----\nBB\n-----END CERTIFICATE-----";
        let text = format!("junk {a}\n{b}\n-----BEGIN CERTIFICATE-----\nC-C\n-----END CERTIFICATE-----");
        assert_eq!(chain_blocks(&text), vec![a, b]);
        assert_eq!(
            chain_blocks("-----BEGIN CERTIFICATE----------END CERTIFICATE-----"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn ai_google_auth_well_known_path_joins_like_node() {
        assert_eq!(js_join(Path::new("/a/./b//../c/x.json")), "/a/c/x.json");
        assert_eq!(js_join(Path::new(".config/gcloud/x.json")), ".config/gcloud/x.json");
    }
}
