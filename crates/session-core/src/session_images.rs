//! `rlm.images` for a session ([`kernel_apis::images`]): the `images.model` and `images.baseUrl` settings, the
//! session's output directory, provider keys as chat requests resolve them (auth.json, models.json, the environment)
//! and HTTP through the session environment's proxies.

use crate::compose::Config;
use kernel_apis::{ImagesConfig, ImagesModule};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// The images module of session `session_id`: images go to `<agent dir>/rlm-output/<session id>/images`.
pub fn module(config: &Config, session_id: &str) -> ImagesModule {
    let setting = |key: &str| {
        config
            .settings
            .pointer(&format!("/images/{key}"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let env = crate::RunEnv {
        cwd: config.cwd.clone(),
        home: config.home.clone(),
        var: config.var.clone(),
        docs: PathBuf::new(),
    };
    ImagesModule::new(ImagesConfig {
        default_model: setting("model"),
        base_url: setting("baseUrl"),
        out_dir: config.agent_dir.join("rlm-output").join(session_id).join("images"),
        cwd: config.cwd.clone(),
        api_key: Arc::new(move |provider: &str| {
            crate::lane::request_key(&crate::ModelFlags::default(), &env, provider)
        }),
        transport: Arc::new(ai::ReqwestTransport::for_env(&*config.var, None)),
    })
}
