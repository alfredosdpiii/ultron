//! Replays `fixtures/images.json` (recorded from the TypeScript images API by `fixtures/generate-images.mjs`) and
//! ports the offline tests of `images-models.test.ts`, `openrouter-images.test.ts` and the catalog checks.

use ai::http::{AbortSignal, Body, BoxFuture, HeaderMap, HttpRequest, HttpResponse, Transport};
use ai::images::{
    AssistantImages, EnvLookup, ImagesApi, ImagesAuth, ImagesAuthOverrides, ImagesAuthResult, ImagesContent,
    ImagesContext, ImagesModel, ImagesModels, ImagesOptions, ImagesProvider, ImagesStopReason, builtin_images_models,
    generate_images, image_model, image_models, image_providers, images_api_provider, register_images_api_provider,
};
use ai::json::normalize_numbers;
use ai::types::{ImageContent, ModelCost, TextContent};
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const NOW: i64 = 1_767_225_600_000;

struct OnceBody(Option<Vec<u8>>);

impl Body for OnceBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

#[derive(Default)]
struct Mock {
    queue: Mutex<Vec<(u16, HeaderMap, String)>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let next = {
            let mut queue = self.queue.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        Box::pin(async move {
            let (status, headers, body) = next.ok_or("unexpected request")?;
            Ok(HttpResponse {
                status,
                headers,
                body: Box::new(OnceBody(Some(body.into_bytes()))),
            })
        })
    }
}

fn options_from(case: &Value) -> ImagesOptions {
    let o = &case["options"];
    let signal = AbortSignal::new();
    if case["abort"] == true {
        signal.abort();
    }
    let patch = case["payloadPatch"].as_object().cloned();
    ImagesOptions {
        api_key: o["apiKey"].as_str().map(str::to_owned),
        headers: o["headers"].as_object().map(|h| {
            h.iter()
                .map(|(k, v)| (k.clone(), v.as_str().map(str::to_owned)))
                .collect()
        }),
        max_retries: o["maxRetries"].as_u64().map(|n| n as u32),
        signal: Some(signal),
        on_payload: patch.map(|patch| {
            Arc::new(move |params: &Value, _: &ImagesModel| {
                let mut next = params.as_object().cloned().unwrap_or_default();
                next.extend(patch.clone());
                Some(Value::Object(next))
            }) as ai::images::ImagesPayloadHook
        }),
        now_ms: Some(NOW),
        ..Default::default()
    }
}

fn request_json(request: &HttpRequest) -> Value {
    json!({
        "url": request.url,
        "method": request.method,
        "headers": request.headers,
        "body": serde_json::from_str::<Value>(&request.body).unwrap(),
    })
}

#[tokio::test]
async fn ai_images_generate_matches_typescript() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("../fixtures/images.json")).unwrap();
    assert_eq!(cases.len(), 20);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let model: ImagesModel = serde_json::from_value(case["model"].clone()).unwrap();
        let context: ImagesContext = serde_json::from_value(case["context"].clone()).unwrap();
        let mock = Mock::default();
        *mock.queue.lock().unwrap() = case["responses"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let body = match &r["body"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let mut headers: HeaderMap = r["headers"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect();
                headers.insert("content-type".into(), "application/json".into());
                (r["status"].as_u64().unwrap_or(200) as u16, headers, body)
            })
            .collect();
        let options = options_from(&case);
        let (result, thrown) = if case["via"] == "collection" {
            let env: HashMap<String, String> = serde_json::from_value(case["env"].clone()).unwrap();
            let lookup: EnvLookup = Arc::new(move |name| env.get(name).cloned());
            let models = builtin_images_models(lookup);
            (
                Some(models.generate_images(&model, &context, &options, &mock).await),
                None,
            )
        } else {
            match generate_images(&model, &context, &options, &mock).await {
                Ok(result) => (Some(result), None),
                Err(e) => (None, Some(e)),
            }
        };
        let result = result.map_or(Value::Null, |r| normalize_numbers(serde_json::to_value(r).unwrap()));
        assert_eq!(result, case["result"], "{name}: result");
        assert_eq!(
            thrown.map_or(Value::Null, Value::from),
            case["thrown"],
            "{name}: thrown"
        );
        let requests: Vec<Value> = mock.requests.lock().unwrap().iter().map(request_json).collect();
        assert_eq!(Value::Array(requests), case["requests"], "{name}: requests");
    }
}

// ---------------------------------------------------------------------------------------------- catalog

#[test]
fn ai_images_builtin_catalog_lists_openrouter_image_models() {
    assert_eq!(image_providers().collect::<Vec<_>>(), ["openrouter"]);
    let models: Vec<_> = image_models("openrouter").collect();
    assert_eq!(models.len(), 55);
    assert!(
        models
            .iter()
            .all(|m| m.api == "openrouter-images" && m.provider == "openrouter")
    );
    assert!(models.iter().all(|m| m.output.iter().any(|o| o == "image")));
    let flux = image_model("openrouter", "black-forest-labs/flux.2-pro").unwrap();
    assert_eq!(flux.name, "Black Forest Labs: FLUX.2 Pro");
    assert_eq!(flux.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(flux.input, ["text", "image"]);
    assert!(image_model("openrouter", "missing").is_none());
    assert_eq!(image_models("nope").count(), 0);
}

#[test]
fn ai_images_catalog_round_trips_every_model() {
    let raw: Value = serde_json::from_str(include_str!("../data/image-models.json")).unwrap();
    for (provider, models) in raw.as_object().unwrap() {
        for (id, model) in models.as_object().unwrap() {
            let parsed = image_model(provider, id).unwrap();
            assert_eq!(&normalize_numbers(serde_json::to_value(parsed).unwrap()), model, "{id}");
        }
    }
}

// ---------------------------------------------------------------------------------------------- registry

fn test_model(provider: &str, id: &str) -> ImagesModel {
    ImagesModel {
        id: id.into(),
        name: id.into(),
        api: "test-images".into(),
        provider: provider.into(),
        base_url: "https://example.test/v1".into(),
        input: vec!["text".into()],
        output: vec!["image".into()],
        cost: ModelCost::default(),
        headers: None,
        extra: Default::default(),
    }
}

fn ok_result(model: &ImagesModel) -> AssistantImages {
    AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: vec![ImagesContent::Image(ImageContent {
            data: "aGk=".into(),
            mime_type: "image/png".into(),
        })],
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: NOW,
    }
}

type Calls = Arc<Mutex<Vec<(ImagesModel, ImagesOptions)>>>;

struct RecordingApi(Calls);

impl ImagesApi for RecordingApi {
    fn generate_images<'a>(
        &'a self,
        model: &'a ImagesModel,
        _context: &'a ImagesContext,
        options: &'a ImagesOptions,
        _transport: &'a dyn Transport,
    ) -> BoxFuture<'a, AssistantImages> {
        self.0.lock().unwrap().push((model.clone(), options.clone()));
        Box::pin(async move { ok_result(model) })
    }
}

fn context() -> ImagesContext {
    ImagesContext {
        input: vec![ImagesContent::Text(TextContent {
            text: "a red circle".into(),
            text_signature: None,
        })],
    }
}

#[tokio::test]
async fn ai_images_registry_dispatches_by_api_and_rejects_mismatches() {
    let calls = Calls::default();
    register_images_api_provider("test-images", Arc::new(RecordingApi(calls.clone())));
    let model = test_model("p1", "m");
    let result = generate_images(&model, &context(), &ImagesOptions::default(), &Mock::default())
        .await
        .unwrap();
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    assert_eq!(calls.lock().unwrap().len(), 1);

    let other = ImagesModel {
        api: "openrouter-images".into(),
        ..model
    };
    let registered = images_api_provider("test-images").unwrap();
    let error = registered
        .generate_images(&other, &context(), &ImagesOptions::default(), &Mock::default())
        .await
        .unwrap_err();
    assert_eq!(error, "Mismatched api: openrouter-images expected test-images");
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert!(images_api_provider("never-registered").is_none());
}

// ---------------------------------------------------------------------------------------------- ImagesModels

/// The test provider's auth: `{auth: {}}` without an env var, else stored key or env var (`images-models.test.ts`).
struct TestAuth {
    env_var: Option<&'static str>,
    env: HashMap<String, String>,
}

impl ImagesAuth for TestAuth {
    fn resolve<'a>(
        &'a self,
        _provider_id: &'a str,
        overrides: &'a ImagesAuthOverrides,
    ) -> BoxFuture<'a, Result<Option<ImagesAuthResult>, String>> {
        Box::pin(async move {
            let Some(var) = self.env_var else {
                return Ok(Some(ImagesAuthResult::default()));
            };
            let key = overrides.api_key.clone().or_else(|| self.env.get(var).cloned());
            Ok(key.map(|key| ImagesAuthResult {
                api_key: Some(key),
                ..Default::default()
            }))
        })
    }
}

fn test_provider(
    id: &str,
    models: Vec<ImagesModel>,
    env_var: Option<&'static str>,
    env: &[(&str, &str)],
    calls: &Calls,
) -> ImagesProvider {
    ImagesProvider::new(
        id,
        None,
        Arc::new(TestAuth {
            env_var,
            env: env.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(),
        }),
        models,
        None,
        Arc::new(RecordingApi(calls.clone())),
    )
}

#[test]
fn ai_images_models_register_providers_and_read_models() {
    let calls = Calls::default();
    let mut models = ImagesModels::new();
    models.set_provider(test_provider(
        "p1",
        vec![test_model("p1", "m1"), test_model("p1", "m2")],
        None,
        &[],
        &calls,
    ));
    models.set_provider(test_provider("p2", vec![test_model("p2", "m3")], None, &[], &calls));
    let ids = |list: Vec<ImagesModel>| list.into_iter().map(|m| m.id).collect::<Vec<_>>();
    assert_eq!(
        models.providers().iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
        ["p1", "p2"]
    );
    assert_eq!(models.provider("p1").unwrap().name, "p1");
    assert_eq!(ids(models.models(None)), ["m1", "m2", "m3"]);
    assert_eq!(ids(models.models(Some("p1"))), ["m1", "m2"]);
    assert_eq!(models.model("p2", "m3").unwrap().id, "m3");
    assert!(models.model("p2", "missing").is_none());
    assert!(models.models(Some("nope")).is_empty());
    models.delete_provider("p1");
    assert!(models.provider("p1").is_none());
    models.clear_providers();
    assert!(models.providers().is_empty());
}

#[tokio::test]
async fn ai_images_models_merge_auth_and_explicit_options_win() {
    let calls = Calls::default();
    let mut models = ImagesModels::new();
    models.set_provider(test_provider(
        "p1",
        vec![test_model("p1", "model-a")],
        Some("TEST_KEY"),
        &[("TEST_KEY", "env-key")],
        &calls,
    ));
    let model = models.model("p1", "model-a").unwrap();
    let none = ImagesAuthOverrides::default();
    assert_eq!(
        models.auth("p1", &none).await.unwrap().unwrap().api_key.as_deref(),
        Some("env-key")
    );
    let explicit = ImagesAuthOverrides {
        api_key: Some("explicit-key".into()),
        env: None,
    };
    assert_eq!(
        models.auth("p1", &explicit).await.unwrap().unwrap().api_key.as_deref(),
        Some("explicit-key")
    );
    assert_eq!(models.auth("ghost", &none).await.unwrap(), None);

    let mock = Mock::default();
    let result = models
        .generate_images(&model, &context(), &ImagesOptions::default(), &mock)
        .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Stop);
    let options = ImagesOptions {
        api_key: Some("explicit".into()),
        ..Default::default()
    };
    models.generate_images(&model, &context(), &options, &mock).await;
    let calls = calls.lock().unwrap();
    assert_eq!(calls[0].1.api_key.as_deref(), Some("env-key"));
    assert_eq!(calls[1].1.api_key.as_deref(), Some("explicit"));
}

struct FixedAuth(ImagesAuthResult);

impl ImagesAuth for FixedAuth {
    fn resolve<'a>(
        &'a self,
        _provider_id: &'a str,
        _overrides: &'a ImagesAuthOverrides,
    ) -> BoxFuture<'a, Result<Option<ImagesAuthResult>, String>> {
        Box::pin(async move { Ok(Some(self.0.clone())) })
    }
}

#[tokio::test]
async fn ai_images_models_merge_provider_env_headers_and_base_url() {
    let calls = Calls::default();
    let mut models = ImagesModels::new();
    let env = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect::<HashMap<String, String>>()
    };
    let headers = |pairs: &[(&str, Option<&str>)]| {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), v.map(str::to_owned)))
            .collect::<IndexMap<String, Option<String>>>()
    };
    models.set_provider(ImagesProvider::new(
        "p1",
        None,
        Arc::new(FixedAuth(ImagesAuthResult {
            api_key: Some("provider-key".into()),
            env: Some(env(&[("PROVIDER_ONLY", "provider"), ("SHARED", "provider")])),
            headers: Some(headers(&[("x-auth", Some("a")), ("x-shared", Some("auth"))])),
            base_url: Some("https://proxy.test/v1".into()),
            source: None,
        })),
        vec![test_model("p1", "model-a")],
        None,
        Arc::new(RecordingApi(calls.clone())),
    ));
    let model = models.model("p1", "model-a").unwrap();
    let options = ImagesOptions {
        api_key: Some("request-key".into()),
        env: Some(env(&[("REQUEST_ONLY", "request"), ("SHARED", "request")])),
        headers: Some(headers(&[("x-shared", Some("request")), ("x-drop", None)])),
        ..Default::default()
    };
    models
        .generate_images(&model, &context(), &options, &Mock::default())
        .await;
    let calls = calls.lock().unwrap();
    let (sent_model, sent) = &calls[0];
    assert_eq!(sent.api_key.as_deref(), Some("request-key"));
    assert_eq!(
        sent.env,
        Some(env(&[
            ("PROVIDER_ONLY", "provider"),
            ("REQUEST_ONLY", "request"),
            ("SHARED", "request")
        ]))
    );
    assert_eq!(
        sent.headers,
        Some(headers(&[
            ("x-auth", Some("a")),
            ("x-shared", Some("request")),
            ("x-drop", None)
        ]))
    );
    assert_eq!(sent_model.base_url, "https://proxy.test/v1");
    assert_eq!(model.base_url, "https://example.test/v1");
}

#[tokio::test]
async fn ai_images_models_unknown_provider_errors_and_unconfigured_still_dispatches() {
    let calls = Calls::default();
    let mut models = ImagesModels::new();
    let ghost = models
        .generate_images(
            &test_model("ghost", "m"),
            &context(),
            &ImagesOptions::default(),
            &Mock::default(),
        )
        .await;
    assert_eq!(ghost.stop_reason, ImagesStopReason::Error);
    assert_eq!(ghost.error_message.as_deref(), Some("Unknown provider: ghost"));

    models.set_provider(test_provider(
        "p1",
        vec![test_model("p1", "model-a")],
        Some("MISSING"),
        &[],
        &calls,
    ));
    let model = models.model("p1", "model-a").unwrap();
    assert_eq!(models.auth("p1", &ImagesAuthOverrides::default()).await.unwrap(), None);
    models
        .generate_images(&model, &context(), &ImagesOptions::default(), &Mock::default())
        .await;
    assert_eq!(calls.lock().unwrap()[0].1.api_key, None);
}

struct FailingAuth;

impl ImagesAuth for FailingAuth {
    fn resolve<'a>(
        &'a self,
        _provider_id: &'a str,
        _overrides: &'a ImagesAuthOverrides,
    ) -> BoxFuture<'a, Result<Option<ImagesAuthResult>, String>> {
        Box::pin(async { Err("OAuth refresh failed for p1: invalid_grant".to_string()) })
    }
}

#[tokio::test]
async fn ai_images_models_auth_failure_is_an_error_result() {
    let calls = Calls::default();
    let mut models = ImagesModels::new();
    models.set_provider(ImagesProvider::new(
        "p1",
        None,
        Arc::new(FailingAuth),
        vec![test_model("p1", "m")],
        None,
        Arc::new(RecordingApi(calls.clone())),
    ));
    let options = ImagesOptions {
        now_ms: Some(NOW),
        ..Default::default()
    };
    let result = models
        .generate_images(&test_model("p1", "m"), &context(), &options, &Mock::default())
        .await;
    assert_eq!(result.stop_reason, ImagesStopReason::Error);
    assert_eq!(
        result.error_message.as_deref(),
        Some("OAuth refresh failed for p1: invalid_grant")
    );
    assert_eq!(result.timestamp, NOW);
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ai_images_models_refresh_dedupes_in_flight_and_wraps_failures() {
    let fetches = Arc::new(AtomicUsize::new(0));
    let counter = fetches.clone();
    let dynamic = ImagesProvider::new(
        "dyn",
        None,
        Arc::new(FixedAuth(ImagesAuthResult::default())),
        Vec::new(),
        Some(Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                Ok(vec![test_model("dyn", "listed")])
            })
        })),
        Arc::new(RecordingApi(Calls::default())),
    );
    let mut models = ImagesModels::new();
    models.set_provider(dynamic);
    assert!(models.models(Some("dyn")).is_empty());
    let (a, b) = tokio::join!(models.refresh(Some("dyn")), models.refresh(Some("dyn")));
    assert_eq!((a, b), (Ok(()), Ok(())));
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    assert!(models.model("dyn", "listed").is_some());

    models.set_provider(ImagesProvider::new(
        "flaky",
        None,
        Arc::new(FixedAuth(ImagesAuthResult::default())),
        vec![test_model("flaky", "kept")],
        Some(Arc::new(|| Box::pin(async { Err("fetch failed".to_string()) }))),
        Arc::new(RecordingApi(Calls::default())),
    ));
    let error = models.refresh(Some("flaky")).await.unwrap_err();
    assert_eq!(error.code, "model_source");
    assert_eq!(error.message, "Model refresh failed for flaky: fetch failed");
    assert!(models.model("flaky", "kept").is_some());
    assert_eq!(models.refresh(None).await, Ok(()));
    assert_eq!(models.refresh(Some("unknown")).await, Ok(()));
}

#[tokio::test]
async fn ai_images_builtin_models_register_openrouter_with_env_auth() {
    let lookup: EnvLookup = Arc::new(|name| (name == "OPENROUTER_API_KEY").then(|| "or-key".to_string()));
    let models = builtin_images_models(lookup);
    assert_eq!(
        models.providers().iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
        ["openrouter"]
    );
    let list = models.models(Some("openrouter"));
    assert!(!list.is_empty());
    assert!(list.iter().all(|m| m.api == "openrouter-images"));
    let auth = models
        .auth(&list[0].provider, &ImagesAuthOverrides::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(auth.api_key.as_deref(), Some("or-key"));
    assert_eq!(auth.source.as_deref(), Some("OPENROUTER_API_KEY"));

    // A request env value wins over the ambient lookup; an empty explicit key falls through to the env.
    let overrides = ImagesAuthOverrides {
        api_key: Some(String::new()),
        env: Some(HashMap::from([(
            "OPENROUTER_API_KEY".to_string(),
            "req-key".to_string(),
        )])),
    };
    let auth = models.auth("openrouter", &overrides).await.unwrap().unwrap();
    assert_eq!(auth.api_key.as_deref(), Some("req-key"));
}
