//! Replays `fixtures/models-auth.json` (recorded by `fixtures/generate-rest.mjs auth`): the same scripted providers
//! (defined here as in the generator) and the same sequence of `Models` operations must give the same results and
//! `ModelsError`s; the `auth.json` store must leave the file coding-agent's `AuthStorage` leaves.

use ai::auth::{
    ApiKeyAuth, ApiKeyCredential, AuthCheck, AuthContext, AuthError, AuthEvent, AuthInteraction, AuthOverrides,
    AuthPrompt, AuthResult, AuthTarget, AuthType, Credential, CredentialEnv, CredentialStore, EnvApiKeyAuth,
    FileCredentialStore, InMemoryCredentialStore, ModelAuth, Models, OAuthAuth, OAuthCredential, Provider,
    ProviderAuth,
};
use ai::cloudflare::{CloudflareAuth, CloudflareAuthKind};
use ai::http::{AbortSignal, BoxFuture};
use ai::json::normalize_numbers;
use ai::options::ProviderHeaders;
use ai::{Model, StreamOptions};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/models-auth.json")).expect("fixture parses"));
const NOW: i64 = 1_767_225_600_000;

#[derive(Default)]
struct ScriptContext {
    env: Mutex<IndexMap<String, String>>,
}

impl AuthContext for ScriptContext {
    fn env(&self, name: &str) -> Option<String> {
        self.env
            .lock()
            .unwrap()
            .get(name)
            .filter(|v| !v.trim().is_empty())
            .cloned()
    }

    fn file_exists(&self, _path: &str) -> bool {
        false
    }

    /// The generator's fixed `Date.now()`.
    fn now_ms(&self) -> f64 {
        NOW as f64
    }
}

/// The generator's `fakeOAuth(id, fail)`.
struct FakeOAuth {
    id: String,
    fail: bool,
    refreshes: Arc<AtomicU32>,
}

impl OAuthAuth for FakeOAuth {
    fn name(&self) -> &str {
        &self.id
    }

    fn login<'a>(&'a self, interaction: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let code = interaction
                .prompt(
                    AuthPrompt::Text {
                        message: "Code?".into(),
                        placeholder: None,
                    },
                    None,
                )
                .await?;
            interaction.notify(AuthEvent::Progress {
                message: format!("got {code}"),
            });
            Ok(OAuthCredential {
                refresh: format!("r-{code}"),
                access: format!("a-{code}"),
                expires: (NOW + 3_600_000) as f64,
                extra: Map::new(),
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        _signal: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let count = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
            // Yield so concurrent callers really queue on the store.
            tokio::task::yield_now().await;
            if self.fail {
                return Err("invalid_grant: token revoked".into());
            }
            Ok(OAuthCredential {
                refresh: format!("{}+", credential.refresh),
                access: format!("fresh-{count}"),
                expires: (NOW + 3_600_000) as f64,
                extra: Map::new(),
            })
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String> {
        let mut headers = ProviderHeaders::new();
        headers.insert("X-Auth".into(), Some("a".into()));
        headers.insert("Authorization".into(), Some(format!("Bearer {}", credential.access)));
        Ok(ModelAuth {
            api_key: Some(credential.access.clone()),
            headers: Some(headers),
            base_url: (self.id == "oauth-prov").then(|| "https://per-credential.example.com".into()),
        })
    }
}

struct CheckedAuth;

impl ApiKeyAuth for CheckedAuth {
    fn name(&self) -> &str {
        "Checked"
    }

    fn check<'a>(
        &'a self,
        _ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> Option<BoxFuture<'a, Result<Option<AuthCheck>, String>>> {
        Some(Box::pin(async move {
            Ok(Some(AuthCheck {
                source: Some(
                    if credential.is_some() {
                        "checked stored"
                    } else {
                        "checked ambient"
                    }
                    .into(),
                ),
                kind: AuthType::ApiKey,
            }))
        }))
    }

    fn resolve<'a>(
        &'a self,
        _ctx: &'a dyn AuthContext,
        _credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, String>> {
        Box::pin(async { Err("resolve must not run during checks".to_string()) })
    }
}

struct ThrowingAuth(&'static str);

impl ApiKeyAuth for ThrowingAuth {
    fn name(&self) -> &str {
        "Thrower"
    }

    fn resolve<'a>(
        &'a self,
        _ctx: &'a dyn AuthContext,
        _credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, String>> {
        let message = self.0.to_string();
        Box::pin(async move { Err(message) })
    }
}

fn model(provider: &str, id: &str, headers: Option<IndexMap<String, String>>) -> Model {
    serde_json::from_value(json!({
        "id": id, "name": "Test Model", "api": "echo", "provider": provider, "baseUrl": "https://default.example.com",
        "reasoning": true, "input": ["text", "image"], "cost": {"input": 3, "output": 15, "cacheRead": 0.3, "cacheWrite": 3.75},
        "contextWindow": 200000, "maxTokens": 32000, "headers": headers,
    }))
    .unwrap()
}

fn env_auth(name: &str, vars: &[&str]) -> Arc<dyn ApiKeyAuth> {
    Arc::new(EnvApiKeyAuth {
        name: name.into(),
        env_vars: vars.iter().map(|v| v.to_string()).collect(),
    })
}

struct Script {
    models: Models,
    store: Arc<InMemoryCredentialStore>,
    context: Arc<ScriptContext>,
    refreshes: HashMap<String, Arc<AtomicU32>>,
}

fn script() -> Script {
    let store = Arc::new(InMemoryCredentialStore::default());
    let context = Arc::new(ScriptContext::default());
    let mut models = Models::new(store.clone(), context.clone());
    let mut refreshes = HashMap::new();
    let mut oauth = |id: &str, fail: bool| -> Arc<dyn OAuthAuth> {
        let counter = Arc::new(AtomicU32::new(0));
        refreshes.insert(id.to_string(), counter.clone());
        Arc::new(FakeOAuth {
            id: id.into(),
            fail,
            refreshes: counter,
        })
    };
    let provider = |id: &str, auth: ProviderAuth, models: Vec<Model>| Provider {
        id: id.into(),
        name: id.into(),
        auth,
        models,
        filter_models: None,
    };
    let e1_headers: IndexMap<String, String> = [
        ("X-Model".to_string(), "1".to_string()),
        ("authorization".into(), "model-level".into()),
    ]
    .into();
    let list = vec![
        provider(
            "env-prov",
            ProviderAuth {
                api_key: Some(env_auth("Env key", &["ENV_KEY_A", "ENV_KEY_B"])),
                oauth: None,
            },
            vec![model("env-prov", "e1", Some(e1_headers)), model("env-prov", "e2", None)],
        ),
        provider(
            "oauth-prov",
            ProviderAuth {
                api_key: None,
                oauth: Some(oauth("oauth-prov", false)),
            },
            vec![model("oauth-prov", "o1", None)],
        ),
        provider(
            "fail-prov",
            ProviderAuth {
                api_key: None,
                oauth: Some(oauth("fail-prov", true)),
            },
            vec![model("fail-prov", "f1", None)],
        ),
        Provider {
            filter_models: Some(Arc::new(|list: &[Model], credential: Option<&Credential>| {
                list.iter()
                    .filter(|m| !matches!(credential, Some(Credential::OAuth(_))) || m.id != "b-api-only")
                    .cloned()
                    .collect()
            })),
            ..provider(
                "both-prov",
                ProviderAuth {
                    api_key: Some(env_auth("Both key", &["BOTH_KEY"])),
                    oauth: Some(oauth("both-prov", false)),
                },
                vec![model("both-prov", "b1", None)],
            )
        },
        provider(
            "check-prov",
            ProviderAuth {
                api_key: Some(Arc::new(CheckedAuth)),
                oauth: None,
            },
            vec![model("check-prov", "c1", None)],
        ),
        provider(
            "throw-prov",
            ProviderAuth {
                api_key: Some(Arc::new(ThrowingAuth("keychain locked"))),
                oauth: None,
            },
            vec![model("throw-prov", "t1", None)],
        ),
        provider(
            "dup-prov",
            ProviderAuth {
                api_key: Some(Arc::new(ThrowingAuth("provider dup-prov"))),
                oauth: None,
            },
            vec![],
        ),
        provider(
            "blank-prov",
            ProviderAuth {
                api_key: Some(Arc::new(ThrowingAuth("   "))),
                oauth: None,
            },
            vec![],
        ),
        provider(
            "cloudflare-workers-ai",
            ProviderAuth {
                api_key: Some(Arc::new(CloudflareAuth(CloudflareAuthKind::WorkersAi))),
                oauth: None,
            },
            vec![model("cloudflare-workers-ai", "cf1", None)],
        ),
        provider(
            "cloudflare-ai-gateway",
            ProviderAuth {
                api_key: Some(Arc::new(CloudflareAuth(CloudflareAuthKind::AiGateway))),
                oauth: None,
            },
            vec![model("cloudflare-ai-gateway", "gw1", None)],
        ),
    ];
    for p in list {
        models.set_provider(p);
    }
    Script {
        models,
        store,
        context,
        refreshes,
    }
}

/// Scripted answers; prompts and events are recorded.
struct Scripted {
    answers: Mutex<Vec<String>>,
    prompts: Mutex<Vec<Value>>,
    events: Mutex<Vec<Value>>,
}

impl AuthInteraction for Scripted {
    fn prompt<'a>(&'a self, prompt: AuthPrompt, _cancel: Option<AbortSignal>) -> BoxFuture<'a, Result<String, String>> {
        self.prompts
            .lock()
            .unwrap()
            .push(serde_json::to_value(&prompt).unwrap());
        let answer = {
            let mut answers = self.answers.lock().unwrap();
            if answers.is_empty() {
                None
            } else {
                Some(answers.remove(0))
            }
        };
        Box::pin(async move { answer.ok_or_else(|| "no answer".to_string()) })
    }

    fn notify(&self, event: AuthEvent) {
        self.events.lock().unwrap().push(serde_json::to_value(&event).unwrap());
    }
}

fn error(e: AuthError) -> Value {
    match e {
        AuthError::Models(e) => json!({"error": {"code": e.code, "message": e.message}}),
        AuthError::Aborted => json!({"error": "aborted"}),
    }
}

fn overrides(step: &Value) -> AuthOverrides {
    let o = &step["overrides"];
    AuthOverrides {
        api_key: o["apiKey"].as_str().map(str::to_string),
        env: o
            .get("env")
            .map(|e| serde_json::from_value::<CredentialEnv>(e.clone()).unwrap()),
        min_oauth_validity_ms: o["minOAuthValidityMs"].as_f64(),
        signal: None,
    }
}

fn opt<T: serde::Serialize>(value: Result<Option<T>, AuthError>) -> Value {
    match value {
        Ok(v) => serde_json::to_value(v).unwrap(),
        Err(e) => error(e),
    }
}

async fn run_step(s: &Script, step: &Value) -> Value {
    let provider = step["provider"].as_str().unwrap_or_default();
    match step["op"].as_str().unwrap() {
        "setEnv" => {
            *s.context.env.lock().unwrap() = serde_json::from_value(step["env"].clone()).unwrap();
            Value::Null
        }
        "store" => {
            let credential: Credential = serde_json::from_value(step["credential"].clone()).unwrap();
            s.store
                .modify(
                    provider,
                    Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })),
                    None,
                )
                .await
                .unwrap();
            Value::Null
        }
        "read" => serde_json::to_value(s.store.read(provider).await.unwrap()).unwrap(),
        "list" => serde_json::to_value(s.store.list().await.unwrap()).unwrap(),
        "refreshes" => json!(s.refreshes[provider].load(Ordering::SeqCst)),
        "getAuth" => {
            let found;
            let target = match step.get("model") {
                Some(m) => {
                    found = s
                        .models
                        .model(m["provider"].as_str().unwrap(), m["id"].as_str().unwrap())
                        .unwrap();
                    AuthTarget::Model(&found)
                }
                None => AuthTarget::Provider(provider),
            };
            opt(s.models.get_auth(target, &overrides(step)).await)
        }
        "concurrentGetAuth" => {
            let n = step["n"].as_u64().unwrap() as usize;
            let none = AuthOverrides::default();
            let calls = (0..n).map(|_| s.models.get_auth(AuthTarget::Provider(provider), &none));
            let results = futures_join(calls.collect()).await;
            Value::Array(results.into_iter().map(opt).collect())
        }
        "checkAuth" => opt(s.models.check_auth(provider, None).await),
        "getAvailable" => match s.models.get_available(step["provider"].as_str(), None).await {
            Ok(list) => json!(
                list.iter()
                    .map(|m| format!("{}/{}", m.provider, m.id))
                    .collect::<Vec<_>>()
            ),
            Err(e) => error(e),
        },
        "logout" => match s.models.logout(provider, None).await {
            Ok(()) => Value::Null,
            Err(e) => error(e),
        },
        "login" => {
            let interaction = Scripted {
                answers: Mutex::new(serde_json::from_value(step["answers"].clone()).unwrap()),
                prompts: Mutex::default(),
                events: Mutex::default(),
            };
            let kind = if step["type"] == "oauth" {
                AuthType::OAuth
            } else {
                AuthType::ApiKey
            };
            let credential = match s.models.login(provider, kind, &interaction).await {
                Ok(c) => serde_json::to_value(c).unwrap(),
                Err(e) => error(e),
            };
            json!({"credential": credential, "prompts": *interaction.prompts.lock().unwrap(), "events": *interaction.events.lock().unwrap()})
        }
        "stream" => {
            let m = &step["model"];
            let target = s
                .models
                .model(m["provider"].as_str().unwrap(), m["id"].as_str().unwrap())
                .unwrap_or_else(|| model(m["provider"].as_str().unwrap(), m["id"].as_str().unwrap(), None));
            let options: StreamOptions = serde_json::from_value(step["options"].clone()).unwrap();
            let transform = |mut headers: ProviderHeaders| {
                headers.insert("X-Transformed".into(), Some("yes".into()));
                headers
            };
            let transform: Option<&(dyn Fn(ProviderHeaders) -> ProviderHeaders + Send + Sync)> =
                if step["transform"] == true {
                    Some(&transform)
                } else {
                    None
                };
            match s.models.apply_auth(&target, &options, transform).await {
                Ok((request_model, request)) => json!({"request": {
                    "baseUrl": request_model.base_url,
                    "apiKey": request.api_key,
                    "headers": request.headers,
                    "env": request.env,
                }}),
                Err(e) => json!({"setupError": e.message()}),
            }
        }
        other => panic!("unknown op {other}"),
    }
}

/// Runs futures concurrently on the current task.
async fn futures_join<F: std::future::Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut pinned: Vec<std::pin::Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = (0..pinned.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, future) in pinned.iter_mut().enumerate() {
            if outputs[i].is_none() {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => outputs[i] = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs.into_iter().map(Option::unwrap).collect()
}

#[test]
fn ai_models_auth_script_matches_typescript() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let s = script();
    let mut failures = Vec::new();
    for (i, entry) in FIXTURE["script"].as_array().unwrap().iter().enumerate() {
        let actual = runtime.block_on(run_step(&s, &entry["step"]));
        if normalize_numbers(actual.clone()) != normalize_numbers(entry["out"].clone()) {
            failures.push(format!(
                "step {i} {}\n  expected {}\n  actual   {actual}",
                entry["step"], entry["out"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn ai_auth_json_store_writes_what_coding_agent_writes() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent").join("auth.json");
    let store = FileCredentialStore::new(&path);
    for entry in FIXTURE["fileOps"].as_array().unwrap() {
        let step = &entry["step"];
        let provider = step["provider"].as_str().unwrap_or_default();
        let out = runtime.block_on(async {
            match step["op"].as_str().unwrap() {
                "modify" => {
                    let credential: Credential = serde_json::from_value(step["credential"].clone()).unwrap();
                    store
                        .modify(
                            provider,
                            Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })),
                            None,
                        )
                        .await
                        .unwrap();
                    Value::Null
                }
                "read" => serde_json::to_value(store.read(provider).await.unwrap()).unwrap(),
                "delete" => {
                    store.delete(provider).await.unwrap();
                    Value::Null
                }
                "list" => serde_json::to_value(store.list().await.unwrap()).unwrap(),
                other => panic!("unknown op {other}"),
            }
        });
        assert_eq!(
            normalize_numbers(out),
            normalize_numbers(entry["out"].clone()),
            "{step}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        FIXTURE["fileText"].as_str().unwrap()
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}
