//! Review frame lanes wrap the session transport in `RetryingTransport`; it must pass the WebSocket capability and
//! the socket request through, or Codex frames always fall back to SSE.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Inner {
    available: bool,
    opened: AtomicUsize,
}

impl ai::Transport for Inner {
    fn send(
        &self,
        _request: ai::HttpRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ai::HttpResponse, String>> + Send + '_>> {
        Box::pin(async { Err("no http here".to_string()) })
    }

    fn websocket_available(&self) -> bool {
        self.available
    }

    fn websocket(
        &self,
        url: String,
        _headers: BTreeMap<String, String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn ai::websocket::WebSocketConnection>, String>> + Send + '_>,
    > {
        self.opened.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Err(format!("inner opened {url}")) })
    }
}

#[test]
fn review_retrying_transport_passes_the_websocket_through() {
    for available in [true, false] {
        let inner = Arc::new(Inner {
            available,
            opened: AtomicUsize::new(0),
        });
        let retrying = review::engine::RetryingTransport {
            inner: inner.clone(),
            policy: review::engine::RetryPolicy::default(),
        };
        assert_eq!(ai::Transport::websocket_available(&retrying), available);
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let opened = runtime.block_on(ai::Transport::websocket(&retrying, "wss://x/y".into(), BTreeMap::new()));
        assert_eq!(opened.err().as_deref(), Some("inner opened wss://x/y"));
        assert_eq!(inner.opened.load(Ordering::SeqCst), 1);
    }
}
