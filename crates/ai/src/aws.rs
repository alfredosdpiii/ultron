//! The AWS pieces the Bedrock provider needs without the AWS SDK: SigV4 request signing (`@smithy/signature-v4`), the
//! `application/vnd.amazon.eventstream` binary framing (`@smithy/core/event-streams`) and the standard retry
//! classification. Credentials and shared config files are `aws_credentials.rs`.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

// --------------------------------------------------------------------------------------------------------- SigV4

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// A request to sign: lower-case or mixed-case header names; `path` is already URI-encoded once.
#[derive(Clone, Debug, Default)]
pub struct SignableRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, Vec<String>)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

const ALWAYS_UNSIGNABLE: &[&str] = &[
    "authorization",
    "cache-control",
    "connection",
    "expect",
    "from",
    "keep-alive",
    "max-forwards",
    "pragma",
    "referer",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "x-amzn-trace-id",
];

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().to_vec()
}

/// `escapeUri`: `encodeURIComponent` plus `!'()*`.
pub fn escape_uri(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn canonical_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let normalized = format!(
        "{}{}{}",
        if path.starts_with('/') { "/" } else { "" },
        segments.join("/"),
        if !segments.is_empty() && path.ends_with('/') {
            "/"
        } else {
            ""
        }
    );
    escape_uri(&normalized).replace("%2F", "/")
}

fn canonical_query(query: &[(String, Vec<String>)]) -> String {
    let mut entries: Vec<(String, String)> = Vec::new();
    for (key, values) in query {
        if key.eq_ignore_ascii_case("x-amz-signature") {
            continue;
        }
        let key = escape_uri(key);
        let mut encoded: Vec<String> = values.iter().map(|v| format!("{key}={}", escape_uri(v))).collect();
        encoded.sort();
        entries.push((key, encoded.join("&")));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>()
        .join("&")
}

/// `YYYYMMDD'T'HHMMSS'Z'` for a Unix time in milliseconds.
pub fn amz_date(epoch_ms: i64) -> String {
    let secs = epoch_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `SignatureV4.signRequest`: adds `x-amz-date`, `x-amz-security-token`, `x-amz-content-sha256` and `authorization`
/// to `request.headers` (lower-case names).
pub fn sign_request(
    request: &mut SignableRequest,
    credentials: &AwsCredentials,
    region: &str,
    service: &str,
    epoch_ms: i64,
) {
    request.headers.retain(|(name, _)| {
        let lower = name.to_ascii_lowercase();
        lower != "authorization" && lower != "x-amz-date" && lower != "date"
    });
    let long_date = amz_date(epoch_ms);
    let short_date = &long_date[..8];
    let scope = format!("{short_date}/{region}/{service}/aws4_request");
    request.headers.push(("x-amz-date".into(), long_date.clone()));
    if let Some(token) = &credentials.session_token {
        request.headers.push(("x-amz-security-token".into(), token.clone()));
    }
    let existing_hash = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-amz-content-sha256"))
        .map(|(_, v)| v.clone());
    let payload_hash = existing_hash.clone().unwrap_or_else(|| sha256_hex(&request.body));
    if existing_hash.is_none() {
        request
            .headers
            .push(("x-amz-content-sha256".into(), payload_hash.clone()));
    }
    let mut canonical: BTreeMap<String, String> = BTreeMap::new();
    let mut names: Vec<&(String, String)> = request.headers.iter().collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in names {
        let lower = name.to_ascii_lowercase();
        if ALWAYS_UNSIGNABLE.contains(&lower.as_str()) || lower.starts_with("proxy-") || lower.starts_with("sec-") {
            continue;
        }
        let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
        canonical.insert(lower, collapsed);
    }
    let signed_headers = canonical.keys().cloned().collect::<Vec<_>>().join(";");
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n\n{}\n{}",
        request.method,
        canonical_path(&request.path),
        canonical_query(&request.query),
        canonical
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join("\n"),
        signed_headers,
        payload_hash
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{long_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let mut key = hmac_sha256(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        short_date.as_bytes(),
    );
    for part in [region, service, "aws4_request"] {
        key = hmac_sha256(&key, part.as_bytes());
    }
    let signature = hex(&hmac_sha256(&key, string_to_sign.as_bytes()));
    request.headers.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key_id
        ),
    ));
}

// ------------------------------------------------------------------------------------------- event-stream framing

/// One decoded event-stream message: string-valued headers (other header types are kept as their JSON-ish text) and
/// the payload.
#[derive(Clone, Debug, PartialEq)]
pub struct EventMessage {
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// Splits a byte stream into event-stream messages (`getChunkedStream` + `splitMessage` + header decoding).
#[derive(Debug, Default)]
pub struct EventStreamDecoder {
    buffer: Vec<u8>,
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

impl EventStreamDecoder {
    /// Complete messages in `chunk` and the bytes before it.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<EventMessage>, String> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while self.buffer.len() >= 4 {
            let total = be32(&self.buffer[..4]) as usize;
            if self.buffer.len() < total {
                break;
            }
            let message: Vec<u8> = self.buffer.drain(..total.max(4)).collect();
            out.push(decode_message(&message)?);
        }
        Ok(out)
    }

    /// The end of the stream: leftover bytes are a truncated message.
    pub fn finish(&self) -> Result<(), String> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err("Truncated event message received.".into())
        }
    }
}

fn decode_message(message: &[u8]) -> Result<EventMessage, String> {
    const PRELUDE: usize = 8;
    if message.len() < PRELUDE + 8 {
        return Err("Provided message too short to accommodate event stream message overhead".into());
    }
    if be32(&message[..4]) as usize != message.len() {
        return Err("Reported message length does not match received message length".into());
    }
    let header_length = be32(&message[4..8]) as usize;
    let expected_prelude = be32(&message[8..12]);
    let prelude_crc = crc32fast::hash(&message[..PRELUDE]);
    if expected_prelude != prelude_crc {
        return Err(format!(
            "The prelude checksum specified in the message ({expected_prelude}) does not match the calculated CRC32 checksum ({prelude_crc})"
        ));
    }
    let expected_message = be32(&message[message.len() - 4..]);
    let message_crc = crc32fast::hash(&message[..message.len() - 4]);
    if expected_message != message_crc {
        return Err(format!(
            "The message checksum ({message_crc}) did not match the expected value of {expected_message}"
        ));
    }
    let headers_start = PRELUDE + 4;
    let headers_end = (headers_start + header_length).min(message.len() - 4);
    let headers = decode_headers(&message[headers_start..headers_end])?;
    Ok(EventMessage {
        headers,
        body: message[headers_end..message.len() - 4].to_vec(),
    })
}

fn decode_headers(mut bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut headers = BTreeMap::new();
    let malformed = || "Malformed event stream header".to_string();
    while !bytes.is_empty() {
        let name_length = bytes[0] as usize;
        let name = String::from_utf8_lossy(bytes.get(1..1 + name_length).ok_or_else(malformed)?).into_owned();
        bytes = &bytes[1 + name_length..];
        let kind = *bytes.first().ok_or_else(malformed)?;
        bytes = &bytes[1..];
        let take = |bytes: &mut &[u8], n: usize| -> Result<Vec<u8>, String> {
            let value = bytes.get(..n).ok_or_else(malformed)?.to_vec();
            *bytes = &bytes[n..];
            Ok(value)
        };
        let value = match kind {
            0 => "true".to_string(),
            1 => "false".to_string(),
            2 => (take(&mut bytes, 1)?[0] as i8).to_string(),
            3 => i16::from_be_bytes(take(&mut bytes, 2)?.try_into().map_err(|_| malformed())?).to_string(),
            4 => i32::from_be_bytes(take(&mut bytes, 4)?.try_into().map_err(|_| malformed())?).to_string(),
            5 | 8 => i64::from_be_bytes(take(&mut bytes, 8)?.try_into().map_err(|_| malformed())?).to_string(),
            6 | 7 => {
                let length = u16::from_be_bytes(take(&mut bytes, 2)?.try_into().map_err(|_| malformed())?) as usize;
                String::from_utf8_lossy(&take(&mut bytes, length)?).into_owned()
            }
            9 => hex(&take(&mut bytes, 16)?),
            _ => return Err(malformed()),
        };
        headers.insert(name, value);
    }
    Ok(headers)
}

/// Encode a message with string headers (tests and fixtures).
pub fn encode_message(headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        header_bytes.push(7);
        header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value.as_bytes());
    }
    let total = 12 + header_bytes.len() + body.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32fast::hash(&out[..8]).to_be_bytes());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(body);
    out.extend_from_slice(&crc32fast::hash(&out).to_be_bytes());
    out
}

// ---------------------------------------------------------------------------------------------------------- retries

const THROTTLING_ERROR_CODES: &[&str] = &[
    "BandwidthLimitExceeded",
    "EC2ThrottledException",
    "LimitExceededException",
    "PriorRequestNotComplete",
    "ProvisionedThroughputExceededException",
    "RequestLimitExceeded",
    "RequestThrottled",
    "RequestThrottledException",
    "SlowDown",
    "ThrottledException",
    "Throttling",
    "ThrottlingException",
    "TooManyRequestsException",
    "TransactionInProgressException",
];
const TRANSIENT_ERROR_CODES: &[&str] = &["TimeoutError", "RequestTimeout", "RequestTimeoutException"];
const TRANSIENT_STATUS_CODES: &[u16] = &[500, 502, 503, 504];

/// `isThrottlingError`.
pub fn is_throttling_error(name: &str, status: Option<u16>) -> bool {
    status == Some(429) || THROTTLING_ERROR_CODES.contains(&name)
}

/// `isTransientError` for a service error (connection failures are transient too, decided by the caller).
pub fn is_transient_error(name: &str, status: Option<u16>) -> bool {
    TRANSIENT_ERROR_CODES.contains(&name) || status.is_some_and(|s| TRANSIENT_STATUS_CODES.contains(&s))
}

/// `DefaultRetryBackoffStrategy`: a uniformly random delay below `min(base * 2^retries, 20 s)`; throttling errors use
/// a 500 ms base, others 100 ms.
pub fn backoff_delay_ms(retries: u32, throttling: bool) -> u64 {
    let base: f64 = if throttling { 500.0 } else { 100.0 };
    let cap = (base * 2f64.powi(retries as i32)).min(20_000.0);
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let random = (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64;
    (random * cap).floor() as u64
}
