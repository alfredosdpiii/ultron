//! The key each model request carries, resolved when the request is sent (`ModelRuntime.getAuth` per request, so a
//! `/login`, a changed auth.json or a refreshed OAuth token applies to the next request), and the request stack the
//! session's side lanes (frames, in-memory subagents, the predict adapter) send through.
//!
//! - [`Keys`]: `--api-key` is a runtime key of the provider it was given for (session-worker.ts
//!   `setRuntimeApiKey(resolved.model.provider, apiKey)`); every other provider resolves its own credentials.
//!   Ambient AWS or Google credentials (`<authenticated>`) authenticate a request without a key, so none is sent.
//! - [`Keyed`]: a stream whose requests carry the key of their model's provider unless the caller set one.
//! - [`Retrying`]: the harness retry policy (`settings.retry`, read per request) over a stream, as a TypeScript
//!   harness lane retries a transient provider error.

use agent::StreamFn;
use ai::http::BoxFuture;
use ai::{AssistantMessage, EventSink, Model, SimpleStreamOptions, TranscriptContext};
use harness::chat::RetryPolicy;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;

/// The key `env_api_key_in` reports for configured ambient AWS or Google credentials: the provider is configured,
/// but its requests carry no API key (TypeScript `getAuth` resolves with no `apiKey`).
pub const AMBIENT_CREDENTIALS: &str = "<authenticated>";

/// Resolves a provider's own credentials (auth.json, models.json, the environment) now.
pub type ProviderCredentials = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The keys of a session's requests.
#[derive(Clone)]
pub struct Keys {
    /// `--api-key` and the provider it was given for.
    runtime: Option<(String, String)>,
    resolve: ProviderCredentials,
}

impl Keys {
    /// `runtime`: the `--api-key` of `(provider, key)`; every provider else resolves through `resolve`.
    pub fn new(runtime: Option<(String, String)>, resolve: ProviderCredentials) -> Keys {
        Keys { runtime, resolve }
    }

    /// The session's keys: `api_key` (`--api-key`) for `provider` (the starting model's), the profile's credentials
    /// of `env` for every provider.
    pub fn session(api_key: Option<String>, provider: &str, env: &crate::RunEnv) -> Keys {
        let env = env.clone();
        Keys::new(
            api_key.map(|key| (provider.to_string(), key)),
            Arc::new(move |provider: &str| crate::lane::api_key(&crate::ModelFlags::default(), &env, provider)),
        )
    }

    /// Whether `provider` has a key (or ambient credentials): what a prompt's preflight checks.
    pub fn key(&self, provider: &str) -> Option<String> {
        match &self.runtime {
            Some((runtime, key)) if runtime == provider => Some(key.clone()),
            _ => (self.resolve)(provider),
        }
    }

    /// The API key a request of `provider` sends: [`Keys::key`] without the ambient-credentials marker.
    pub fn request_key(&self, provider: &str) -> Option<String> {
        self.key(provider).filter(|key| key != AMBIENT_CREDENTIALS)
    }

    /// [`Keys::request_key`] as a provider-key function.
    pub fn request_keys(&self) -> ProviderCredentials {
        let keys = self.clone();
        Arc::new(move |provider: &str| keys.request_key(provider))
    }
}

/// A stream whose requests carry the key of their model's provider, resolved per request, unless the caller set one.
pub struct Keyed<S> {
    pub inner: S,
    pub keys: Keys,
}

impl<S: StreamFn> StreamFn for Keyed<S> {
    fn stream<'a>(
        &'a self,
        model: &'a Model,
        context: &'a TranscriptContext,
        options: &'a SimpleStreamOptions,
        sink: &'a mut EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage> {
        Box::pin(async move {
            if options.base.api_key.is_some() {
                return self.inner.stream(model, context, options, sink).await;
            }
            let mut keyed = options.clone();
            keyed.base.api_key = self.keys.request_key(&model.provider);
            self.inner.stream(model, context, &keyed, sink).await
        })
    }
}

/// Reads the retry policy as the settings name it now.
pub type RetrySource = Arc<dyn Fn() -> RetryPolicy + Send + Sync>;

/// `settings.retry` of a settings manager (`getRetrySettings`): what the durable runtime retries a lane's
/// generation with.
pub fn retry_policy(settings: &config::settings::SettingsManager) -> RetryPolicy {
    let retry = settings
        .settings()
        .get("retry")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let policy = crate::compaction_op::SummaryRetry::from_settings(&retry, true);
    RetryPolicy {
        enabled: policy.max_attempts > 1,
        max_retries: policy.max_attempts - 1,
        base_delay_ms: policy.base_delay_ms,
        max_delay_ms: policy.max_delay_ms,
    }
}

/// A stream that asks again after a transient provider error (`isRetryableAssistantError`), up to the policy's
/// retries with its backoff; an aborted request is never retried and the wait ends with the request's signal.
pub struct Retrying<S> {
    pub inner: S,
    pub policy: RetrySource,
}

impl<S: StreamFn> StreamFn for Retrying<S> {
    fn stream<'a>(
        &'a self,
        model: &'a Model,
        context: &'a TranscriptContext,
        options: &'a SimpleStreamOptions,
        sink: &'a mut EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage> {
        Box::pin(async move {
            let policy = (self.policy)();
            let mut attempt = 0;
            loop {
                let message = self.inner.stream(model, context, options, sink).await;
                let aborted = options.base.signal.as_ref().is_some_and(ai::AbortSignal::aborted);
                if aborted
                    || !policy.enabled
                    || attempt >= policy.max_retries
                    || !harness::chat::models::is_retryable(&message)
                {
                    return message;
                }
                attempt += 1;
                let delay = std::time::Duration::from_millis(policy.delay_ms(attempt));
                if !wait(delay, options.base.signal.as_ref()).await {
                    return message;
                }
            }
        })
    }
}

/// Waits `delay`; false when `signal` aborts first.
async fn wait(delay: std::time::Duration, signal: Option<&ai::AbortSignal>) -> bool {
    let Some(signal) = signal else {
        tokio::time::sleep(delay).await;
        return true;
    };
    let mut sleep = pin!(tokio::time::sleep(delay));
    let mut cancelled = pin!(signal.cancelled());
    std::future::poll_fn(|cx| {
        if cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(false);
        }
        match sleep.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(true),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// The stream a side lane of the session requests through as a TypeScript harness lane does: the live request
/// settings and idle timeout (`streamOptions`, `httpIdleTimeoutMs`), the key of each request's provider resolved per
/// request, and the harness retry policy.
pub fn lane_stream(settings: crate::lane::SettingsReader, keys: Keys) -> Arc<dyn StreamFn> {
    let reader = settings.clone();
    Arc::new(Retrying {
        inner: Keyed {
            inner: crate::lane::Provider::new().with_live_settings(settings, true),
            keys,
        },
        policy: Arc::new(move || retry_policy(&reader())),
    })
}
