//! Replays `fixtures/auth.json` (TypeScript `resolveProviderAuth` over the built-in providers, recorded by
//! `fixtures/generate-auth.mjs`) and ports the credential-store and OAuth-refresh locking tests of
//! `models-runtime.test.ts` and `oauth-auth.test.ts`.

use ai::auth::{
    AuthContext, AuthError, AuthInteraction, AuthOverrides, AuthType, Credential, CredentialEnv, CredentialInfo,
    CredentialStore, InMemoryCredentialStore, ModelAuth, ModelsError, ModelsErrorCode, ModifyFn, OAuthAuth,
    ProviderAuth, StoreError, builtin_provider_auth_with_clock, copilot_filter_models, resolve_provider_auth,
};
use ai::http::{AbortSignal, Body, BoxFuture, HttpRequest, HttpResponse, Transport};
use ai::oauth::OAuthCredential;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const NOW: f64 = 1_767_225_600_000.0;

struct OnceBody(Option<Vec<u8>>);

impl Body for OnceBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

#[derive(Default)]
struct Mock {
    queue: Mutex<Vec<(u16, String)>>,
    requests: Mutex<Vec<Value>>,
}

impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(
            json!({"url": request.url, "method": request.method, "headers": request.headers, "body": request.body}),
        );
        let next = {
            let mut queue = self.queue.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        Box::pin(async move {
            let (status, body) = next.ok_or("unexpected request")?;
            Ok(HttpResponse {
                status,
                headers: Default::default(),
                body: Box::new(OnceBody(Some(body.into_bytes()))),
            })
        })
    }
}

struct FixedContext {
    env: HashMap<String, String>,
    files: Vec<String>,
}

impl AuthContext for FixedContext {
    fn env(&self, name: &str) -> Option<String> {
        self.env.get(name).filter(|v| !v.trim().is_empty()).cloned()
    }

    fn file_exists(&self, path: &str) -> bool {
        self.files.iter().any(|f| f == path)
    }

    fn now_ms(&self) -> f64 {
        NOW
    }
}

fn string_map(value: &Value) -> CredentialEnv {
    value
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn comparable(message: &str) -> String {
    message.split("; stack=").next().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn ai_auth_resolve_provider_auth_matches_typescript() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("../fixtures/auth.json")).unwrap();
    assert!(cases.len() >= 40);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let provider = case["provider"].as_str().unwrap();
        let mock = Arc::new(Mock::default());
        *mock.queue.lock().unwrap() = case["responses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r[0].as_u64().unwrap() as u16, r[1].as_str().unwrap().to_owned()))
            .collect();
        let auth = builtin_provider_auth_with_clock(provider, mock.clone(), || NOW as i64).unwrap();
        let store = InMemoryCredentialStore::new();
        if !case["stored"].is_null() {
            let stored = Credential::from_json(&case["stored"]).unwrap();
            store
                .modify(
                    provider,
                    Box::new(move |_| Box::pin(async move { Ok(Some(stored)) })),
                    None,
                )
                .await
                .unwrap();
        }
        let ctx = FixedContext {
            env: string_map(&case["env"]).into_iter().collect(),
            files: case["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f.as_str().unwrap().to_owned())
                .collect(),
        };
        let o = &case["overrides"];
        let overrides = AuthOverrides {
            api_key: o["apiKey"].as_str().map(str::to_owned),
            env: o.get("env").map(string_map),
            min_oauth_validity_ms: o["minOAuthValidityMs"].as_f64(),
            signal: None,
        };
        let (result, error) = match resolve_provider_auth(provider, &auth, &store, &ctx, &overrides).await {
            Ok(result) => (result.map_or(Value::Null, |r| r.to_json()), Value::Null),
            Err(AuthError::Models(ModelsError { code, message })) => (
                Value::Null,
                json!({"name": "ModelsError", "code": code.as_str(), "message": comparable(&message)}),
            ),
            Err(AuthError::Aborted) => (Value::Null, json!("aborted")),
        };
        let mut expected_error = case["error"].clone();
        if let Some(message) = expected_error.get("message").and_then(Value::as_str) {
            expected_error["message"] = json!(comparable(message));
        }
        assert_eq!(error, expected_error, "{name}: error");
        assert_eq!(result, case["result"], "{name}: result");
        let after = store.read(provider).await.unwrap().map_or(Value::Null, |c| c.to_json());
        assert_eq!(after, case["storedAfter"], "{name}: stored after");
        assert_eq!(
            Value::Array(mock.requests.lock().unwrap().clone()),
            case["requests"],
            "{name}: requests"
        );
    }
}

/// `testOAuth` of models-runtime.test.ts with a counting, optionally failing or slow refresh.
struct TestOAuth {
    refreshes: AtomicUsize,
    fail: Option<String>,
    delay_ms: u64,
    seen_signal: Mutex<Option<AbortSignal>>,
    started: tokio::sync::Notify,
    block: bool,
}

impl TestOAuth {
    fn new() -> Self {
        TestOAuth {
            refreshes: AtomicUsize::new(0),
            fail: None,
            delay_ms: 0,
            seen_signal: Mutex::new(None),
            started: tokio::sync::Notify::new(),
            block: false,
        }
    }
}

impl OAuthAuth for TestOAuth {
    fn name(&self) -> &str {
        "Test OAuth"
    }

    fn login<'a>(&'a self, _: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async { Err("no login".into()) })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
            *self.seen_signal.lock().unwrap() = signal;
            self.started.notify_one();
            if self.block {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            if let Some(message) = &self.fail {
                return Err(message.clone());
            }
            Ok(OAuthCredential {
                access: format!("new-{n}"),
                expires: now() + 60.0 * 60_000.0,
                ..credential.clone()
            })
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String> {
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            ..Default::default()
        })
    }
}

fn now() -> f64 {
    ai::auth::DefaultAuthContext.now_ms()
}

fn oauth_credential(access: &str, expires: f64) -> Credential {
    Credential::OAuth(OAuthCredential {
        refresh: "r".into(),
        access: access.into(),
        expires,
        extra: Default::default(),
    })
}

fn set<'a>(credential: Credential) -> ModifyFn<'a> {
    Box::new(move |_| Box::pin(async move { Ok(Some(credential)) }))
}

fn oauth_only(oauth: Arc<TestOAuth>) -> ProviderAuth {
    ProviderAuth {
        api_key: None,
        oauth: Some(oauth),
    }
}

async fn access_of(store: &dyn CredentialStore) -> String {
    match store.read("p1").await.unwrap() {
        Some(Credential::OAuth(c)) => c.access,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn ai_auth_concurrent_refreshes_run_once() {
    let store = InMemoryCredentialStore::new();
    store
        .modify("p1", set(oauth_credential("old", 0.0)), None)
        .await
        .unwrap();
    let oauth = Arc::new(TestOAuth {
        delay_ms: 10,
        ..TestOAuth::new()
    });
    let auth = oauth_only(oauth.clone());
    let ctx = ai::auth::DefaultAuthContext;
    let overrides = AuthOverrides::default();
    let (a, b) = tokio::join!(
        resolve_provider_auth("p1", &auth, &store, &ctx, &overrides),
        resolve_provider_auth("p1", &auth, &store, &ctx, &overrides)
    );
    assert_eq!(oauth.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(a.unwrap().unwrap().auth.api_key.as_deref(), Some("new-1"));
    assert_eq!(b.unwrap().unwrap().auth.api_key.as_deref(), Some("new-1"));
    assert_eq!(access_of(&store).await, "new-1");
}

#[tokio::test]
async fn ai_auth_refresh_failure_keeps_the_credential_and_the_reason() {
    let store = InMemoryCredentialStore::new();
    store
        .modify("p1", set(oauth_credential("old", 0.0)), None)
        .await
        .unwrap();
    let auth = oauth_only(Arc::new(TestOAuth {
        fail: Some("token refresh failed (400): invalid_grant".into()),
        ..TestOAuth::new()
    }));
    let error = resolve_provider_auth(
        "p1",
        &auth,
        &store,
        &ai::auth::DefaultAuthContext,
        &AuthOverrides::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        AuthError::Models(ModelsError {
            code: ModelsErrorCode::OAuth,
            message: "OAuth refresh failed for p1: token refresh failed (400): invalid_grant".into()
        })
    );
    assert_eq!(access_of(&store).await, "old");
}

#[tokio::test]
async fn ai_auth_valid_tokens_and_minimum_validity() {
    let store = InMemoryCredentialStore::new();
    let oauth = Arc::new(TestOAuth::new());
    let auth = oauth_only(oauth.clone());
    let ctx = ai::auth::DefaultAuthContext;
    // Ten minutes left: valid under the default five-minute window.
    store
        .modify("p1", set(oauth_credential("valid", now() + 10.0 * 60_000.0)), None)
        .await
        .unwrap();
    let result = resolve_provider_auth("p1", &auth, &store, &ctx, &AuthOverrides::default()).await;
    assert_eq!(result.unwrap().unwrap().auth.api_key.as_deref(), Some("valid"));
    assert_eq!(oauth.refreshes.load(Ordering::SeqCst), 0);
    // A caller asking for thirty minutes forces a refresh.
    let overrides = AuthOverrides {
        min_oauth_validity_ms: Some(30.0 * 60_000.0),
        ..Default::default()
    };
    let result = resolve_provider_auth("p1", &auth, &store, &ctx, &overrides).await;
    assert_eq!(result.unwrap().unwrap().auth.api_key.as_deref(), Some("new-1"));
    // One minute left: inside the window, refreshed.
    store
        .modify("p1", set(oauth_credential("old", now() + 60_000.0)), None)
        .await
        .unwrap();
    let result = resolve_provider_auth("p1", &auth, &store, &ctx, &AuthOverrides::default()).await;
    assert_eq!(result.unwrap().unwrap().auth.api_key.as_deref(), Some("new-2"));
}

#[tokio::test]
async fn ai_auth_cancelling_a_refresh_keeps_the_previous_credential() {
    let store = Arc::new(InMemoryCredentialStore::new());
    store
        .modify("p1", set(oauth_credential("old", 0.0)), None)
        .await
        .unwrap();
    let oauth = Arc::new(TestOAuth {
        block: true,
        ..TestOAuth::new()
    });
    let auth = oauth_only(oauth.clone());
    let signal = AbortSignal::new();
    let overrides = AuthOverrides {
        signal: Some(signal.clone()),
        ..Default::default()
    };
    let started = oauth.started.notified();
    let resolving = resolve_provider_auth("p1", &auth, store.as_ref(), &ai::auth::DefaultAuthContext, &overrides);
    let (result, ()) = tokio::join!(resolving, async {
        started.await;
        signal.abort();
    });
    assert_eq!(result.unwrap_err(), AuthError::Aborted);
    assert!(oauth.seen_signal.lock().unwrap().as_ref().unwrap().aborted());
    assert_eq!(access_of(store.as_ref()).await, "old");
}

#[tokio::test]
async fn ai_auth_cancelled_queued_mutations_never_run() {
    let store = Arc::new(InMemoryCredentialStore::new());
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let first = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .modify(
                    "p1",
                    Box::new(move |_| {
                        Box::pin(async move {
                            released.await.unwrap();
                            Ok(Some(
                                Credential::from_json(&json!({"type": "api_key", "key": "first"})).unwrap(),
                            ))
                        })
                    }),
                    None,
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    let ran = Arc::new(AtomicUsize::new(0));
    let signal = AbortSignal::new();
    let second = {
        let ran = Arc::clone(&ran);
        store.modify(
            "p1",
            Box::new(move |_| {
                Box::pin(async move {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
            }),
            Some(&signal),
        )
    };
    let (second, ()) = tokio::join!(second, async { signal.abort() });
    assert_eq!(second, Err(StoreError::Aborted));
    release.send(()).unwrap();
    first.await.unwrap().unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    assert_eq!(
        store.read("p1").await.unwrap().unwrap().to_json(),
        json!({"type": "api_key", "key": "first"})
    );
    assert_eq!(
        store.list().await.unwrap(),
        vec![CredentialInfo {
            provider_id: "p1".into(),
            kind: AuthType::ApiKey
        }]
    );
    store.delete("p1").await.unwrap();
    assert_eq!(store.read("p1").await.unwrap(), None);
}

/// A store whose read or modify fails, counting modifies.
struct FailingStore {
    read_fails: bool,
    modifies: AtomicUsize,
}

impl CredentialStore for FailingStore {
    fn read<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<Option<Credential>, String>> {
        let read_fails = self.read_fails;
        Box::pin(async move {
            if read_fails {
                Err("disk on fire".into())
            } else {
                Ok(Some(oauth_credential("old", 0.0)))
            }
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn modify<'a>(
        &'a self,
        _: &'a str,
        _: ModifyFn<'a>,
        _: Option<&'a AbortSignal>,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        self.modifies.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(StoreError::Store("disk on fire".into())) })
    }

    fn delete<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn ai_auth_store_failures_are_auth_errors() {
    let auth = oauth_only(Arc::new(TestOAuth::new()));
    let ctx = ai::auth::DefaultAuthContext;
    let read_failing = FailingStore {
        read_fails: true,
        modifies: AtomicUsize::new(0),
    };
    assert_eq!(
        resolve_provider_auth("p1", &auth, &read_failing, &ctx, &AuthOverrides::default()).await,
        Err(AuthError::Models(ModelsError {
            code: ModelsErrorCode::Auth,
            message: "Credential store read failed for p1: disk on fire".into()
        }))
    );
    let modify_failing = FailingStore {
        read_fails: false,
        modifies: AtomicUsize::new(0),
    };
    assert_eq!(
        resolve_provider_auth("p1", &auth, &modify_failing, &ctx, &AuthOverrides::default()).await,
        Err(AuthError::Models(ModelsError {
            code: ModelsErrorCode::Auth,
            message: "Credential store modify failed for p1: disk on fire".into()
        }))
    );
    assert_eq!(modify_failing.modifies.load(Ordering::SeqCst), 1);
}

#[test]
fn ai_auth_credentials_round_trip_auth_json_entries() {
    for entry in [
        json!({"type": "api_key", "key": "k", "env": {"A": "1"}}),
        json!({"type": "api_key"}),
        json!({"type": "oauth", "refresh": "r", "access": "a", "expires": 5, "accountId": "x"}),
        json!({"type": "oauth", "refresh": "r", "access": "a", "expires": 1.5, "availableModelIds": ["m"], "enterpriseUrl": "e"}),
    ] {
        assert_eq!(Credential::from_json(&entry).unwrap().to_json(), entry);
    }
    assert_eq!(Credential::from_json(&json!({"type": "other"})), None);
    assert_eq!(Credential::from_json(&json!({"type": "oauth", "access": "a"})), None);
}

#[test]
fn ai_auth_copilot_filters_models_by_available_ids() {
    let models = ["a", "b", "c"];
    let with = |extra: Value| {
        let mut entry = json!({"type": "oauth", "refresh": "r", "access": "a", "expires": 0});
        if let Value::Object(extra) = extra {
            entry.as_object_mut().unwrap().extend(extra);
        }
        Credential::from_json(&entry).unwrap()
    };
    let ids = |credential: Option<&Credential>| -> Vec<&str> {
        copilot_filter_models(&models, |m| m, credential)
            .into_iter()
            .copied()
            .collect()
    };
    assert_eq!(
        ids(Some(&with(json!({"availableModelIds": ["c", "a", "z"]})))),
        ["a", "c"]
    );
    assert_eq!(
        ids(Some(&with(json!({"availableModelIds": ["a", 1]})))),
        ["a", "b", "c"]
    );
    assert_eq!(ids(Some(&with(json!({})))), ["a", "b", "c"]);
    assert_eq!(ids(None), ["a", "b", "c"]);
    let api_key = Credential::from_json(&json!({"type": "api_key", "key": "k"})).unwrap();
    assert_eq!(ids(Some(&api_key)), ["a", "b", "c"]);
}

#[tokio::test]
async fn ai_auth_images_resolve_through_the_credential_store() {
    use ai::images::{ImagesAuth, ImagesAuthOverrides};
    let store = Arc::new(InMemoryCredentialStore::new());
    let resolver = ai::auth::StoredProviderAuth {
        auth: builtin_provider_auth_with_clock("openrouter", Arc::new(Mock::default()), || NOW as i64).unwrap(),
        credentials: store.clone(),
        ctx: Arc::new(FixedContext {
            env: HashMap::from([("OPENROUTER_API_KEY".to_owned(), "env-key".to_owned())]),
            files: Vec::new(),
        }),
    };
    let resolve = |overrides: ImagesAuthOverrides| {
        let resolver = &resolver;
        async move { resolver.resolve("openrouter", &overrides).await.unwrap().unwrap() }
    };
    let ambient = resolve(ImagesAuthOverrides::default()).await;
    assert_eq!(
        (ambient.api_key.as_deref(), ambient.source.as_deref()),
        (Some("env-key"), Some("OPENROUTER_API_KEY"))
    );
    store
        .modify(
            "openrouter",
            set(Credential::from_json(&json!({"type": "api_key", "key": "stored"})).unwrap()),
            None,
        )
        .await
        .unwrap();
    let stored = resolve(ImagesAuthOverrides::default()).await;
    assert_eq!(
        (stored.api_key.as_deref(), stored.source.as_deref()),
        (Some("stored"), Some("stored credential"))
    );
    let explicit = resolve(ImagesAuthOverrides {
        api_key: Some("explicit".into()),
        env: None,
    })
    .await;
    assert_eq!(explicit.api_key.as_deref(), Some("explicit"));
}
