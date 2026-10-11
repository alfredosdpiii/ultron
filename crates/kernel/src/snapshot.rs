//! Host-side snapshot authentication (port of `snapshot-auth.ts`) and the kernel's snapshot/restore reports
//! (`KernelSnapshotReport` / `KernelRestoreReport` of kernel.ts).
//!
//! The key lives only in the host: the Python kernel runs model-written code, so any key it held could be read by
//! that code. The host signs a snapshot after the kernel writes it and verifies the signature before the kernel may
//! read it. Signed file: one signature line, then the kernel's content (sha256 header line + JSON body):
//! `{"format":"ultron-rlm-snapshot-signature","version":1,"alg":"HMAC-SHA256","hmac":"<hex>"}\n<content>`; the HMAC
//! covers the content bytes exactly.

use crate::config::is_js_space;
use crate::{CellError, ExecutionResult, Status};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::OnceLock;

pub const SNAPSHOT_KEY_FILE: &str = "rlm-snapshot.key";
const SIGNATURE_FORMAT: &str = "ultron-rlm-snapshot-signature";
const KEY_BYTES: usize = 32;
/// The error name kernel.ts gives refused snapshots (`SnapshotAuthenticationError.name`).
pub const SNAPSHOT_ERROR_NAME: &str = "SnapshotIntegrityError";

/// A refused snapshot: the message the kernel result reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotAuthError(pub String);

impl std::fmt::Display for SnapshotAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SnapshotAuthError {}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Lowercase hex of exactly 64 digits, as `/^[0-9a-f]{64}$/` accepts.
fn hex64(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    (0..64)
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

pub fn sha256_hex(content: &[u8]) -> String {
    hex(&Sha256::digest(content))
}

fn hmac_sha256(key: &[u8], content: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|value| value ^ byte);
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(content).finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// The signed file: a signature line over `content`, then `content`.
pub fn sign_snapshot(key: &[u8], content: &[u8]) -> Vec<u8> {
    let mut line = Map::new();
    line.insert("format".into(), SIGNATURE_FORMAT.into());
    line.insert("version".into(), 1.into());
    line.insert("alg".into(), "HMAC-SHA256".into());
    line.insert("hmac".into(), hex(&hmac_sha256(key, content)).into());
    let mut signed = Value::Object(line).to_string().into_bytes();
    signed.push(b'\n');
    signed.extend_from_slice(content);
    signed
}

/// Verifies a signed snapshot and returns the sha256 of its content, which the kernel re-checks on read.
pub fn verify_snapshot(key: &[u8], raw: &[u8]) -> Result<String, SnapshotAuthError> {
    let newline = raw.iter().position(|&byte| byte == b'\n');
    let signature = newline
        .and_then(|newline| serde_json::from_str::<Value>(&String::from_utf8_lossy(&raw[..newline])).ok())
        .filter(Value::is_object);
    let Some(record) = signature.filter(|record| record["format"] == SIGNATURE_FORMAT) else {
        return Err(SnapshotAuthError(
            "snapshot integrity check failed: missing host signature (unsigned snapshots, including ones written before snapshots were signed, are refused)"
                .into(),
        ));
    };
    let Some(signature) = record["hmac"]
        .as_str()
        .filter(|_| record["version"].as_f64() == Some(1.0) && record["alg"] == "HMAC-SHA256")
    else {
        return Err(SnapshotAuthError(
            "snapshot integrity check failed: unsupported host signature".into(),
        ));
    };
    let content = &raw[newline.map_or(raw.len(), |newline| newline + 1)..];
    let expected = hmac_sha256(key, content);
    let actual = hex64(signature).unwrap_or_default();
    // Constant time over the digest length.
    let differs = actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected.iter())
            .fold(0u8, |acc, (left, right)| acc | (left ^ right))
            != 0;
    if differs {
        return Err(SnapshotAuthError(
            "snapshot integrity check failed: HMAC signature mismatch (the snapshot was modified or signed with another profile's key)"
                .into(),
        ));
    }
    Ok(sha256_hex(content))
}

fn parse_key(text: &str, path: &str) -> Result<Vec<u8>, String> {
    hex64(text.trim_matches(is_js_space)).ok_or_else(|| {
        format!(
            "RLM snapshot key {path} is malformed; delete it to create a new key (existing snapshots become unrestorable)"
        )
    })
}

/// Reads the per-profile snapshot key `<agent_dir>/rlm-snapshot.key`, creating it once (0600, random) when absent.
pub fn load_or_create_snapshot_key(agent_dir: &Path) -> Result<Vec<u8>, String> {
    let path = agent_dir.join(SNAPSHOT_KEY_FILE);
    let shown = path.display().to_string();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            let mut key = [0u8; KEY_BYTES];
            getrandom::fill(&mut key).map_err(|error| error.to_string())?;
            file.write_all(format!("{}\n", hex(&key)).as_bytes())
                .map_err(|error| error.to_string())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.to_string()),
    }
    // Keep the key private even if something loosened its mode.
    let mode = std::fs::metadata(&path)
        .map_err(|error| error.to_string())?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|error| error.to_string())?;
    }
    parse_key(
        &std::fs::read_to_string(&path).map_err(|error| error.to_string())?,
        &shown,
    )
}

/// Fallback for kernels built without a profile key: snapshots are then restorable only within this process.
pub fn process_snapshot_key() -> Vec<u8> {
    static KEY: OnceLock<[u8; KEY_BYTES]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = [0u8; KEY_BYTES];
        // Without randomness a process key would be guessable; an all-zero key would still sign, so fail loudly.
        getrandom::fill(&mut key).expect("random snapshot key");
        key
    })
    .to_vec()
}

/// Names are saved only as constrained data; every other user name is listed with a reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotReport {
    pub saved: Vec<String>,
    pub skipped: Vec<String>,
    /// Name to reason, in the kernel's order.
    pub reasons: Map<String, Value>,
    pub sha256: String,
    /// sha256 of the unsigned file content the host signed.
    #[serde(rename = "contentSha256", skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
}

/// `skipped` repeats the names the snapshot could not restore; they are absent, not stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub missing: bool,
    pub skipped: Vec<String>,
    pub reasons: Map<String, Value>,
}

impl SnapshotReport {
    pub fn reason(&self, name: &str) -> Option<&str> {
        self.reasons.get(name).and_then(Value::as_str)
    }
}

impl RestoreReport {
    pub fn reason(&self, name: &str) -> Option<&str> {
        self.reasons.get(name).and_then(Value::as_str)
    }
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn string_record(value: &Value) -> Map<String, Value> {
    value
        .as_object()
        .map(|record| {
            record
                .iter()
                .filter(|(_, item)| item.is_string())
                .map(|(key, item)| (key.clone(), item.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// The done frame's `snapshot` field, as kernel.ts `snapshotReport` reads it.
pub(crate) fn snapshot_report(value: Option<&Value>) -> Option<SnapshotReport> {
    let report = value.filter(|value| value.is_object())?;
    Some(SnapshotReport {
        saved: string_list(&report["saved"]),
        skipped: string_list(&report["skipped"]),
        reasons: string_record(&report["reasons"]),
        sha256: report["sha256"].as_str().unwrap_or_default().to_string(),
        content_sha256: report["content_sha256"].as_str().map(String::from),
    })
}

/// The done frame's `restore` field, as kernel.ts `restoreReport` reads it.
pub(crate) fn restore_report(value: Option<&Value>) -> Option<RestoreReport> {
    let report = value.filter(|value| value.is_object())?;
    Some(RestoreReport {
        restored: string_list(&report["restored"]),
        missing: report["missing"] == true,
        skipped: string_list(&report["skipped"]),
        reasons: string_record(&report["reasons"]),
    })
}

/// kernel.ts `snapshotError`: a refused or failed snapshot operation as a result.
pub(crate) fn snapshot_error(message: impl Into<String>) -> ExecutionResult {
    ExecutionResult {
        status: Status::Error,
        stdout: String::new(),
        stderr: String::new(),
        result: None,
        error: Some(CellError {
            ename: SNAPSHOT_ERROR_NAME.into(),
            evalue: message.into(),
            traceback: Vec::new(),
        }),
    }
}
