//! `ultron update --models` (`refreshModelCatalogs` of package-manager-cli.ts): every provider that refreshes its
//! catalog and has a credential refreshes it from the network, as `ModelRuntime.refresh({ allowNetwork: true,
//! force: true })` does: built-in providers revalidate their pi.dev catalog, Radius providers (the built-in one and
//! models.json `"oauth": "radius"` ones) load `<gateway>/v1/config` with their key (an expired OAuth token is
//! refreshed first). Results land in models-store.json, which every later start restores.
//!
//! Evidence: `crates/ai/fixtures/generate-model-refresh.mjs`; `tests/model_refresh.rs` replays it.

use crate::RunEnv as Env;
use crate::{ModelFlags, lane};
use ai::model_refresh::{load_radius_gateway_config, radius_models_from_config, refresh_remote_catalog};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// The whole refresh's time budget (`refreshModelCatalogs`' 15 s abort).
const REFRESH_BUDGET: Duration = Duration::from_secs(15);

/// [`refresh_catalogs`]' error when its whole budget ran out (`result.aborted` after the timer in TS).
pub const REFRESH_TIMED_OUT: &str = "Model catalog refresh timed out.";

/// Refresh every provider's stored catalog. `Err` is the message `ultron update --models` prints after `Error: `.
/// `user_agent` is `getPiUserAgent(VERSION)` ([`ai::model_refresh::management_user_agent`]).
pub fn refresh_model_catalogs(env: &Env, catalog_base_url: &str, user_agent: &str) -> Result<(), String> {
    let errors = refresh_catalogs(env, catalog_base_url, user_agent, true)?;
    if errors.is_empty() {
        return Ok(());
    }
    let details: Vec<String> = errors.iter().map(|(p, e)| format!("{p}: {e}")).collect();
    Err(format!("Could not refresh model catalogs: {}", details.join("; ")))
}

/// [`refresh_model_catalogs`]' per-provider failures, in provider order; `Err` when the whole refresh timed out.
/// Without `force`, a pi.dev catalog checked within the last four hours is not asked again (a login's refresh).
pub fn refresh_catalogs(
    env: &Env,
    catalog_base_url: &str,
    user_agent: &str,
    force: bool,
) -> Result<Vec<(String, String)>, String> {
    let registry = lane::registry(env);
    let store_path = config::paths::AgentPaths::new(env.agent_dir())
        .models()
        .with_file_name("models-store.json");
    let store = config::models_store::ModelsStore::new(store_path);
    let transport: Arc<dyn ai::Transport> = Arc::new(ai::ReqwestTransport::for_env(&*env.var, None));
    let builtin: Vec<&str> = ai::models::builtin_providers().collect();
    let providers: Vec<String> = registry
        .provider_ids()
        .into_iter()
        .filter(|p| builtin.contains(&p.as_str()) || registry.radius_gateway(p).is_some())
        // `claude-code` has no catalog to refresh (its models are the CLI's), though its requests always resolve.
        .filter(|p| p != ai::claude_code::PROVIDER)
        .collect();
    let now = || lane::now_ms();
    let work = async {
        let mut errors: Vec<(String, String)> = Vec::new();
        for provider in &providers {
            // `resolveRefreshCredential`: no credential, no network refresh.
            let key = match lane::request_key(&ModelFlags::default(), env, provider) {
                Ok(Some(key)) => key,
                Ok(None) => continue,
                Err(error) => {
                    errors.push((provider.clone(), error));
                    continue;
                }
            };
            if let Some((_, gateway)) = registry.radius_gateway(provider) {
                match load_radius_gateway_config(&gateway, Some(&key), &*transport).await {
                    Ok(config) => {
                        let models = radius_models_from_config(provider, &config).unwrap_or_default();
                        if let Err(error) = store.write(provider, json!({"models": models, "checkedAt": now()})) {
                            errors.push((provider.clone(), error));
                        }
                    }
                    Err(error) => errors.push((provider.clone(), error)),
                }
                continue;
            }
            let stored = store.read(provider).ok().flatten();
            let outcome = refresh_remote_catalog(
                provider,
                stored.as_ref(),
                catalog_base_url,
                force,
                user_agent,
                &now,
                &*transport,
            )
            .await;
            if let Some(entry) = outcome.persist
                && let Err(error) = store.write(provider, entry)
            {
                errors.push((provider.clone(), error));
            }
            if let Some(error) = outcome.error {
                errors.push((provider.clone(), error));
            }
        }
        errors
    };
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())?;
                runtime
                    .block_on(async { tokio::time::timeout(REFRESH_BUDGET, work).await })
                    .map_err(|_| REFRESH_TIMED_OUT.to_string())
            })
            .join()
            .unwrap_or_else(|_| Err("Model catalog refresh failed".into()))
    })
}
