//! The HTTP exchange under the Hindsight client: one request, its status and (for a 2xx answer) its body read up to
//! a size limit, cancelled by a signal or a deadline. [`ReqwestTransport`] is the real one.

use kernel::Signal;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// One request as the client builds it. `headers` are final (names lower-case, values validated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// A status and, for a 2xx status only, the whole body (empty when there is none). A non-2xx body is never read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    /// The deadline passed before the body was read.
    Timeout,
    /// The signal was aborted before the body was read.
    Aborted,
    /// A 2xx answer declared, or streamed, more than the limit.
    TooLarge,
    /// No answer: connection, protocol or redirect (redirects are refused, not followed).
    Failed,
}

/// Sends one request. A 2xx answer whose `content-length` (read as a JavaScript `Number`) exceeds `max_bytes`, or
/// whose body grows past it, is [`TransportError::TooLarge`]; the deadline (`timeout` from the call) and the signal
/// cover the whole exchange, body included.
pub trait Transport: Send + Sync {
    fn send(
        &self,
        request: HttpRequest,
        max_bytes: usize,
        timeout: Duration,
        signal: &Signal,
    ) -> Result<HttpResponse, TransportError>;
}

/// reqwest with rustls on a private one-thread runtime, so blocking callers (host request threads) never run inside
/// another runtime. No proxy (as Node's fetch), no redirects, no decompression.
pub struct ReqwestTransport {
    client: reqwest::Client,
    runtime: Option<tokio::runtime::Runtime>,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("hindsight-http")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            runtime: Some(runtime),
        })
    }
}

impl Drop for ReqwestTransport {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// `Number(length) > max` for a `content-length` value; the HTTP stack has already rejected malformed ones.
fn declared_over(length: &str, max: usize) -> bool {
    length.trim().parse::<f64>().is_ok_and(|n| n > max as f64)
}

async fn exchange(
    client: reqwest::Client,
    request: HttpRequest,
    max_bytes: usize,
) -> Result<HttpResponse, TransportError> {
    let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| TransportError::Failed)?;
    let mut builder = client.request(method, &request.url);
    for (name, value) in &request.headers {
        // Values are Latin-1 (validated by the client): sent as their byte values, as fetch's ByteString does.
        let bytes: Vec<u8> = value.chars().map(|c| c as u32 as u8).collect();
        let value = reqwest::header::HeaderValue::from_bytes(&bytes).map_err(|_| TransportError::Failed)?;
        builder = builder.header(name.as_str(), value);
    }
    if let Some(body) = request.body {
        builder = builder.body(body);
    }
    let mut response = builder.send().await.map_err(|_| TransportError::Failed)?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Ok(HttpResponse {
            status,
            body: Vec::new(),
        });
    }
    if let Some(length) = response.headers().get(reqwest::header::CONTENT_LENGTH)
        && declared_over(&String::from_utf8_lossy(length.as_bytes()), max_bytes)
    {
        return Err(TransportError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| TransportError::Failed)? {
        if body.len() + chunk.len() > max_bytes {
            return Err(TransportError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(HttpResponse { status, body })
}

/// How often a waiting caller looks at its signal.
const POLL: Duration = Duration::from_millis(5);

impl Transport for ReqwestTransport {
    fn send(
        &self,
        request: HttpRequest,
        max_bytes: usize,
        timeout: Duration,
        signal: &Signal,
    ) -> Result<HttpResponse, TransportError> {
        let Some(runtime) = &self.runtime else {
            return Err(TransportError::Failed);
        };
        let deadline = Instant::now() + timeout;
        let (done, outcome) = mpsc::channel();
        let task = runtime.spawn({
            let client = self.client.clone();
            async move {
                let _ = done.send(exchange(client, request, max_bytes).await);
            }
        });
        let result = loop {
            if signal.is_aborted() {
                break Err(TransportError::Aborted);
            }
            let now = Instant::now();
            if now >= deadline {
                break Err(TransportError::Timeout);
            }
            match outcome.recv_timeout(POLL.min(deadline - now)) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break Err(TransportError::Failed),
            }
        };
        // Dropping the exchange closes its connection.
        task.abort();
        result
    }
}
