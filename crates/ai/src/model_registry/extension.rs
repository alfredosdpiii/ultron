//! Providers an extension registers (`pi.registerProvider(name, config)`, `ModelRuntime.registerProvider`): the
//! third layer of `provider-composer.ts` over the built-in catalog and models.json. `applyExtension` runs after
//! `applyModelsJson`: a `models` list replaces the provider's models (each takes `api` and `baseUrl` from the
//! definition, the registration, else the existing model `findModelDefaults` picks), a `baseUrl` alone moves every
//! model; models.json `modelOverrides` apply last. The registration's `apiKey` wins over models.json's, its `headers`
//! go over models.json's, its `authHeader` over models.json's, and its model `headers` over the model's own.
//!
//! Only the declarative members cross the extension process boundary (`baseUrl`, `apiKey`, `api`, `headers`,
//! `authHeader`, `models`, `name`); code members (`streamSimple`, OAuth, `refreshModels`) cannot.
//!
//! Evidence: `fixtures/generate-extension-providers.mjs` runs the TypeScript model runtime's `registerProvider`;
//! `tests/extension_providers.rs` replays it.

use super::{ModelRegistry, RequestConfig, SharedLookup, apply_model_override, apply_models_json, find_defaults, get};
use crate::types::Model;
use serde_json::{Map, Value, json};

impl ModelRegistry {
    /// `registerProvider(name, config)`: validated on its own over the built-in and models.json layers (a
    /// registration that does not compose is refused with its error and changes nothing), then merged over the
    /// provider's previous registration (each member given replaces the previous one's).
    pub fn register_extension_provider(&mut self, name: &str, config: &Map<String, Value>) -> Result<(), String> {
        self.layers_below(name)
            .and_then(|models| apply_extension(name, models, config))?;
        let entry = self
            .extensions
            .entry(name.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(previous) = entry.as_object_mut() {
            for (key, value) in config {
                previous.insert(key.clone(), value.clone());
            }
        }
        Ok(())
    }

    /// The providers extensions registered, in registration order.
    pub fn extension_provider_ids(&self) -> Vec<&str> {
        self.extensions.keys().map(String::as_str).collect()
    }

    pub(super) fn extension(&self, provider: &str) -> Option<&Map<String, Value>> {
        self.extensions.get(provider).and_then(Value::as_object)
    }

    /// The provider's models under the extension layer: built-in, then models.json.
    fn layers_below(&self, provider: &str) -> Result<Vec<Value>, String> {
        let base: Vec<Value> = self
            .base_models(provider)
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect();
        match self.provider_config(provider) {
            Some(config) => apply_models_json(provider, base, config),
            None => Ok(base),
        }
    }

    /// `composeModelProvider(...).getModels()` for a provider an extension registered.
    pub(super) fn compose_extension(
        &self,
        provider: &str,
        extension: &Map<String, Value>,
    ) -> Result<Vec<Model>, String> {
        let models = apply_extension(provider, self.layers_below(provider)?, extension)?;
        let overrides = self
            .provider_config(provider)
            .and_then(|c| c.get("modelOverrides"))
            .and_then(Value::as_object);
        models
            .into_iter()
            .map(|model| {
                let id = model["id"].as_str().unwrap_or_default().to_string();
                let model = match overrides.and_then(|o| o.get(&id)).and_then(Value::as_object) {
                    Some(ov) => apply_model_override(model, ov),
                    None => model,
                };
                serde_json::from_value(with_defaults(model))
                    .map_err(|e| format!("Provider {provider}, model {id}: {e}"))
            })
            .collect()
    }

    /// The configured key: the registration's, else models.json's.
    pub(super) fn extension_raw_key(&self, provider: &str) -> Option<&str> {
        let extension = self.extension(provider)?;
        self.compose_extension(provider, extension).ok()?;
        get(extension, "apiKey")
            .or_else(|| self.provider_config(provider).and_then(|c| get(c, "apiKey")))
            .and_then(Value::as_str)
    }

    /// `configuredHeaders`, `rawModelHeaders` and `authHeader` with the registration over models.json.
    pub(super) fn extension_request_config(
        &self,
        model: &Model,
        lookup: Option<SharedLookup>,
    ) -> Option<RequestConfig> {
        let extension = self.extension(&model.provider)?;
        let config = self.provider_config(&model.provider);
        let strings = |value: Option<&Value>| -> Vec<(String, String)> {
            value
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default()
        };
        // Object spread: a later layer's key replaces an earlier one's value in its place.
        let spread = |layers: Vec<Vec<(String, String)>>| {
            let mut out: Vec<(String, String)> = Vec::new();
            for (key, value) in layers.into_iter().flatten() {
                match out.iter_mut().find(|(k, _)| *k == key) {
                    Some(entry) => entry.1 = value,
                    None => out.push((key, value)),
                }
            }
            out
        };
        let definition = |layer: Option<&Map<String, Value>>| {
            layer
                .and_then(|l| l.get("models"))
                .and_then(Value::as_array)
                .and_then(|models| models.iter().find(|m| m["id"].as_str() == Some(model.id.as_str())))
                .and_then(|d| d.get("headers"))
                .cloned()
        };
        let model_headers = spread(vec![
            strings(
                config
                    .and_then(|c| c.get("modelOverrides"))
                    .and_then(|o| o.get(&model.id))
                    .and_then(|o| o.get("headers")),
            ),
            strings(definition(config).as_ref()),
            strings(definition(Some(extension)).as_ref()),
        ]);
        let provider_headers = spread(vec![
            strings(config.and_then(|c| c.get("headers"))),
            strings(extension.get("headers")),
        ]);
        let auth_header = get(extension, "authHeader")
            .or_else(|| config.and_then(|c| get(c, "authHeader")))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Some(RequestConfig {
            provider: model.provider.clone(),
            model: model.id.clone(),
            provider_headers,
            model_headers,
            auth_header,
            lookup,
            stored_auth: None,
        })
    }
}

/// `applyExtension`.
fn apply_extension(provider: &str, models: Vec<Value>, config: &Map<String, Value>) -> Result<Vec<Value>, String> {
    let Some(definitions) = get(config, "models").and_then(Value::as_array) else {
        let Some(base_url) = get(config, "baseUrl") else {
            return Ok(models);
        };
        return Ok(models
            .into_iter()
            .map(|mut model| {
                if let Some(map) = model.as_object_mut() {
                    map.insert("baseUrl".into(), base_url.clone());
                }
                model
            })
            .collect());
    };
    definitions
        .iter()
        .map(|definition| {
            let definition = definition.as_object().cloned().unwrap_or_default();
            let id = definition.get("id").and_then(Value::as_str).unwrap_or_default();
            let wanted = get(&definition, "api")
                .or_else(|| get(config, "api"))
                .and_then(Value::as_str);
            let defaults = find_defaults(&models, id, wanted);
            let pick = |key: &str| {
                get(&definition, key)
                    .or_else(|| get(config, key))
                    .or_else(|| defaults.and_then(|d| get(d, key)))
                    .cloned()
            };
            let api = pick("api").ok_or_else(|| {
                format!("Provider {provider}, model {id}: no \"api\" specified. Set at provider or model level.")
            })?;
            let base_url = pick("baseUrl")
                .ok_or_else(|| format!("Provider {provider}: \"baseUrl\" is required when defining custom models."))?;
            let mut model = definition.clone();
            model.insert("api".into(), api);
            model.insert("provider".into(), json!(provider));
            model.insert("baseUrl".into(), base_url);
            model.remove("headers");
            Ok(Value::Object(model))
        })
        .collect()
}

/// The members `ProviderModelConfig` requires, as `modelFromJson` defaults them, for a definition that left them
/// out (TypeScript carries them as `undefined`; a Rust model needs a value).
fn with_defaults(model: Value) -> Value {
    let Value::Object(mut model) = model else {
        return model;
    };
    let id = model.get("id").cloned().unwrap_or(json!(""));
    for (key, value) in [
        ("name", id),
        ("reasoning", json!(false)),
        ("input", json!(["text"])),
        (
            "cost",
            json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}),
        ),
        ("contextWindow", json!(128000)),
        ("maxTokens", json!(16384)),
    ] {
        if get(&model, key).is_none() {
            model.insert(key.into(), value);
        }
    }
    Value::Object(model)
}
